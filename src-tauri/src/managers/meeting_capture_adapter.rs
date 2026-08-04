//! Native, timestamped capture adapters for meeting recording.
//!
//! Audio callbacks only copy a native PCM frame into a bounded queue. A single
//! persistence worker owns [`MeetingCaptureSession`], keeping filesystem I/O
//! and resampling off device callback threads while retaining each source's
//! own clock and loss accounting.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Sample, SizedSample, Stream, StreamInstant, SupportedStreamConfig};
use log::{error, warn};
#[cfg(target_os = "linux")]
use std::io::{self, Read};
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;
use std::time::Instant;

use super::diarization::{CaptureBackendStatus, CaptureSource};
use super::meeting_capture::{
    CaptureSourceMetadata, MeetingCaptureArtifacts, MeetingCaptureMetadata, MeetingCaptureSession,
    TimestampedAudioFrame,
};

const FRAME_QUEUE_CAPACITY: usize = 512;

enum CaptureCommand {
    Frame(TimestampedAudioFrame),
    FrameMetadata {
        source: CaptureSource,
        metadata: CaptureSourceMetadata,
    },
    SourceStarted {
        source: CaptureSource,
        started_at_unix_ms: i64,
    },
    BackendStatus {
        source: CaptureSource,
        status: CaptureBackendStatus,
    },
    Finalize(mpsc::Sender<Result<MeetingCaptureArtifacts>>),
    Discard(mpsc::Sender<()>),
    Interrupted(mpsc::Sender<()>),
}

/// Owns active native streams and their one durable session worker.
///
/// Dropping streams first is intentional: it prevents new device callbacks
/// from racing with finalization, while all already-queued frames are flushed
/// ahead of the finalization command.
pub struct MeetingCaptureCoordinator {
    command_tx: SyncSender<CaptureCommand>,
    worker: Option<std::thread::JoinHandle<()>>,
    cpal_streams: Vec<Stream>,
    #[cfg(target_os = "linux")]
    linux_system_capture: Option<LinuxSystemCapture>,
    #[cfg(target_os = "macos")]
    screen_capture_stream: Option<screencapturekit::prelude::SCStream>,
}

impl MeetingCaptureCoordinator {
    /// Starts selected/default microphone capture and the platform's native
    /// system-audio adapter. A microphone is required; system capture failure
    /// is persisted as `Unavailable` and does not discard a useful local track.
    pub fn start(
        recordings_dir: &std::path::Path,
        microphone_device: Option<Device>,
        configured_output_device: Option<&str>,
    ) -> Result<Self> {
        let host = crate::audio_toolkit::get_cpal_host();
        let microphone_device = microphone_device
            .or_else(|| host.default_input_device())
            .context("No input device found for meeting microphone capture")?;
        let microphone_name = microphone_device.name().ok();

        let metadata = MeetingCaptureMetadata {
            microphone: CaptureSourceMetadata {
                device_name: microphone_name,
                backend: Some(microphone_backend_name().to_string()),
            },
            system: CaptureSourceMetadata {
                device_name: None,
                backend: Some(system_backend_name().to_string()),
            },
        };
        let session = MeetingCaptureSession::create_with_metadata(recordings_dir, metadata)
            .context("create durable meeting capture session")?;
        let session_started_at_unix_ms = session.session_started_at_unix_ms();
        let (command_tx, command_rx) = mpsc::sync_channel(FRAME_QUEUE_CAPACITY);
        let worker = std::thread::Builder::new()
            .name("meeting-capture-writer".to_string())
            .spawn(move || run_capture_writer(session, command_rx))
            .context("start meeting capture persistence worker")?;

        let meeting_start = Instant::now();
        // On WASAPI, input and loopback timestamps are both QPC positions.
        // Keep one origin across their independent callbacks so queue and
        // driver latency cannot become a permanent inter-track offset.
        let cpal_capture_clock = Arc::new(SharedCpalCaptureClock::default());
        let microphone_stream = match build_microphone_stream(
            microphone_device,
            command_tx.clone(),
            meeting_start,
            session_started_at_unix_ms,
            Arc::clone(&cpal_capture_clock),
        ) {
            Ok(stream) => stream,
            Err(error) => {
                stop_worker_after_start_failure(&command_tx, worker);
                return Err(error.context("start native microphone meeting capture"));
            }
        };

        if let Err(error) = command_tx.send(CaptureCommand::BackendStatus {
            source: CaptureSource::Microphone,
            status: CaptureBackendStatus::Capturing,
        }) {
            drop(microphone_stream);
            stop_worker_after_start_failure(&command_tx, worker);
            return Err(anyhow!(
                "meeting capture worker stopped unexpectedly: {error}"
            ));
        }

        let mut coordinator = Self {
            command_tx,
            worker: Some(worker),
            cpal_streams: vec![microphone_stream],
            #[cfg(target_os = "linux")]
            linux_system_capture: None,
            #[cfg(target_os = "macos")]
            screen_capture_stream: None,
        };

        if let Err(error) = coordinator.start_system_capture(
            &host,
            configured_output_device,
            meeting_start,
            session_started_at_unix_ms,
            Arc::clone(&cpal_capture_clock),
        ) {
            warn!("System audio is unavailable for this meeting capture: {error}");
            let _ = coordinator.command_tx.send(CaptureCommand::BackendStatus {
                source: CaptureSource::System,
                status: CaptureBackendStatus::Unavailable,
            });
        } else {
            #[cfg(not(target_os = "linux"))]
            let _ = coordinator.command_tx.send(CaptureCommand::BackendStatus {
                source: CaptureSource::System,
                status: CaptureBackendStatus::Capturing,
            });
        }

        Ok(coordinator)
    }

    fn start_system_capture(
        &mut self,
        host: &cpal::Host,
        configured_output_device: Option<&str>,
        meeting_start: Instant,
        session_started_at_unix_ms: i64,
        cpal_capture_clock: Arc<SharedCpalCaptureClock>,
    ) -> Result<()> {
        #[cfg(target_os = "windows")]
        {
            let device = selected_output_device(host, configured_output_device)
                .or_else(|| host.default_output_device())
                .context("No output device found for WASAPI loopback")?;
            let device_name = device.name().ok();
            self.command_tx
                .send(CaptureCommand::FrameMetadata {
                    source: CaptureSource::System,
                    metadata: CaptureSourceMetadata {
                        device_name,
                        backend: Some(system_backend_name().to_string()),
                    },
                })
                .map_err(|error| anyhow!("meeting capture worker stopped unexpectedly: {error}"))?;
            let config = device
                .default_output_config()
                .context("read WASAPI output mix format for loopback")?;
            let stream = build_cpal_stream(
                device,
                config,
                CaptureSource::System,
                self.command_tx.clone(),
                meeting_start,
                session_started_at_unix_ms,
                cpal_capture_clock,
            )
            .context("build WASAPI loopback stream")?;
            self.cpal_streams.push(stream);
            return Ok(());
        }

        #[cfg(target_os = "linux")]
        {
            // CPAL's Linux backend is ALSA-oriented, so PipeWire/Pulse monitor
            // sources do not reliably appear as CPAL input devices. Use their
            // native client tools instead and keep their reader off callbacks.
            let _ = (host, cpal_capture_clock);
            self.linux_system_capture = Some(start_linux_system_capture(
                self.command_tx.clone(),
                configured_output_device,
                meeting_start,
                session_started_at_unix_ms,
            )?);
            return Ok(());
        }

        #[cfg(target_os = "macos")]
        {
            let _ = cpal_capture_clock;
            let stream = macos_system_capture(
                self.command_tx.clone(),
                meeting_start,
                session_started_at_unix_ms,
            )?;
            self.screen_capture_stream = Some(stream);
            return Ok(());
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
        {
            let _ = (
                host,
                configured_output_device,
                meeting_start,
                session_started_at_unix_ms,
                cpal_capture_clock,
            );
            Err(anyhow!(
                "System audio capture is not supported on this platform"
            ))
        }
    }

    pub fn finalize(mut self) -> Result<MeetingCaptureArtifacts> {
        self.cpal_streams.clear();
        #[cfg(target_os = "linux")]
        if let Some(capture) = self.linux_system_capture.take() {
            capture.stop();
        }
        #[cfg(target_os = "macos")]
        if let Some(stream) = self.screen_capture_stream.take() {
            if let Err(error) = stream.stop_capture() {
                warn!("Failed to stop ScreenCaptureKit audio capture cleanly: {error}");
            }
        }

        let (reply_tx, reply_rx) = mpsc::channel();
        self.command_tx
            .send(CaptureCommand::Finalize(reply_tx))
            .map_err(|error| anyhow!("meeting capture worker stopped unexpectedly: {error}"))?;
        let result = reply_rx
            .recv()
            .map_err(|error| anyhow!("meeting capture finalization response failed: {error}"))?;
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow!("meeting capture worker panicked during finalization"))?;
        }
        result
    }

    pub fn discard(mut self) {
        self.cpal_streams.clear();
        #[cfg(target_os = "linux")]
        if let Some(capture) = self.linux_system_capture.take() {
            capture.stop();
        }
        #[cfg(target_os = "macos")]
        if let Some(stream) = self.screen_capture_stream.take() {
            let _ = stream.stop_capture();
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let _ = self.command_tx.send(CaptureCommand::Discard(reply_tx));
        let _ = reply_rx.recv();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for MeetingCaptureCoordinator {
    fn drop(&mut self) {
        if self.worker.is_some() {
            // A dropped coordinator is an interrupted meeting, not an explicit
            // user cancellation. Leave checkpointed source chunks recoverable.
            self.cpal_streams.clear();
            #[cfg(target_os = "linux")]
            if let Some(capture) = self.linux_system_capture.take() {
                capture.stop();
            }
            #[cfg(target_os = "macos")]
            if let Some(stream) = self.screen_capture_stream.take() {
                let _ = stream.stop_capture();
            }
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = self.command_tx.send(CaptureCommand::Interrupted(reply_tx));
            let _ = reply_rx.recv();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

fn run_capture_writer(mut session: MeetingCaptureSession, command_rx: Receiver<CaptureCommand>) {
    let mut write_error: Option<anyhow::Error> = None;
    while let Ok(command) = command_rx.recv() {
        match command {
            CaptureCommand::Frame(frame) => {
                if write_error.is_none() {
                    if let Err(error) = session.append_frame(frame) {
                        error!("Meeting capture persistence failed: {error}");
                        write_error = Some(error);
                    }
                }
            }
            CaptureCommand::FrameMetadata { source, metadata } => {
                if write_error.is_none() {
                    if let Err(error) = session.configure_source(source, metadata) {
                        error!("Meeting capture metadata checkpoint failed: {error}");
                        write_error = Some(error);
                    }
                }
            }
            CaptureCommand::SourceStarted {
                source,
                started_at_unix_ms,
            } => {
                if write_error.is_none() {
                    if let Err(error) =
                        session.mark_source_started_at_unix_ms(source, started_at_unix_ms)
                    {
                        error!("Meeting capture source-start checkpoint failed: {error}");
                        write_error = Some(error);
                    }
                }
            }
            CaptureCommand::BackendStatus { source, status } => {
                if write_error.is_none() {
                    if let Err(error) = session.set_source_backend_status(source, status) {
                        error!("Meeting capture backend status checkpoint failed: {error}");
                        write_error = Some(error);
                    }
                }
            }
            CaptureCommand::Finalize(reply_tx) => {
                let result = match write_error {
                    Some(error) => {
                        Err(error.context("meeting capture persistence failed before finalization"))
                    }
                    None => session
                        .finalize_with_artifacts()
                        .context("finalize durable meeting capture"),
                };
                let _ = reply_tx.send(result);
                return;
            }
            CaptureCommand::Discard(reply_tx) => {
                session.discard();
                let _ = reply_tx.send(());
                return;
            }
            CaptureCommand::Interrupted(reply_tx) => {
                // `Drop` preserves the latest checkpoint for recovery.
                let _ = session.checkpoint();
                let _ = reply_tx.send(());
                return;
            }
        }
    }
}

fn stop_worker_after_start_failure(
    command_tx: &SyncSender<CaptureCommand>,
    worker: std::thread::JoinHandle<()>,
) {
    let (reply_tx, reply_rx) = mpsc::channel();
    let _ = command_tx.send(CaptureCommand::Discard(reply_tx));
    let _ = reply_rx.recv();
    let _ = worker.join();
}

fn build_microphone_stream(
    device: Device,
    command_tx: SyncSender<CaptureCommand>,
    meeting_start: Instant,
    session_started_at_unix_ms: i64,
    cpal_capture_clock: Arc<SharedCpalCaptureClock>,
) -> Result<Stream> {
    let config = device
        .default_input_config()
        .context("read microphone capture format")?;
    build_cpal_stream(
        device,
        config,
        CaptureSource::Microphone,
        command_tx,
        meeting_start,
        session_started_at_unix_ms,
        cpal_capture_clock,
    )
}

fn build_cpal_stream(
    device: Device,
    config: SupportedStreamConfig,
    source: CaptureSource,
    command_tx: SyncSender<CaptureCommand>,
    meeting_start: Instant,
    session_started_at_unix_ms: i64,
    cpal_capture_clock: Arc<SharedCpalCaptureClock>,
) -> Result<Stream> {
    let sample_format = config.sample_format();
    let sample_rate = config.sample_rate().0;
    let channels = config.channels();
    let stream_config = config.config();
    let source_started_tx = command_tx.clone();

    let stream = match sample_format {
        cpal::SampleFormat::I8 => build_typed_cpal_stream::<i8>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        cpal::SampleFormat::I16 => build_typed_cpal_stream::<i16>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        cpal::SampleFormat::I32 => build_typed_cpal_stream::<i32>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        cpal::SampleFormat::I64 => build_typed_cpal_stream::<i64>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        cpal::SampleFormat::U8 => build_typed_cpal_stream::<u8>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        cpal::SampleFormat::U16 => build_typed_cpal_stream::<u16>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        cpal::SampleFormat::U32 => build_typed_cpal_stream::<u32>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        cpal::SampleFormat::U64 => build_typed_cpal_stream::<u64>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        cpal::SampleFormat::F32 => build_typed_cpal_stream::<f32>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        cpal::SampleFormat::F64 => build_typed_cpal_stream::<f64>(
            &device,
            &stream_config,
            source,
            sample_rate,
            channels,
            command_tx,
            meeting_start,
            Arc::clone(&cpal_capture_clock),
        ),
        unsupported => {
            return Err(anyhow!(
                "Unsupported native capture sample format {unsupported:?}"
            ))
        }
    }
    .context("create native capture stream")?;
    stream.play().context("start native capture stream")?;
    source_started_tx
        .send(CaptureCommand::SourceStarted {
            source,
            started_at_unix_ms: source_started_at_unix_ms(
                session_started_at_unix_ms,
                meeting_start,
            ),
        })
        .map_err(|error| anyhow!("meeting capture worker stopped unexpectedly: {error}"))?;
    Ok(stream)
}

fn build_typed_cpal_stream<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    source: CaptureSource,
    sample_rate: u32,
    channels: u16,
    command_tx: SyncSender<CaptureCommand>,
    meeting_start: Instant,
    cpal_capture_clock: Arc<SharedCpalCaptureClock>,
) -> Result<Stream, cpal::BuildStreamError>
where
    T: Sample + SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let error_tx = command_tx.clone();
    let mut sequence = 0_u64;
    let mut dropped_frames = 0_u64;
    device.build_input_stream(
        config,
        move |data: &[T], info| {
            let frames = (data.len() / usize::from(channels)) as u64;
            let timestamp_ns =
                cpal_capture_clock.map_capture_time(info.timestamp().capture, meeting_start);
            let frame = TimestampedAudioFrame {
                source,
                timestamp_ns,
                sequence,
                sample_rate,
                channels,
                samples: data
                    .iter()
                    .map(|sample| sample.to_sample::<f32>())
                    .collect(),
                dropped_frames,
            };
            sequence = sequence.saturating_add(1);
            match command_tx.try_send(CaptureCommand::Frame(frame)) {
                Ok(()) => dropped_frames = 0,
                Err(TrySendError::Full(_)) => {
                    dropped_frames = dropped_frames.saturating_add(frames);
                }
                Err(TrySendError::Disconnected(_)) => {
                    // The finalization path intentionally drops streams before
                    // the worker. Do not log-spam a high-priority callback.
                }
            }
        },
        move |stream_error| {
            warn!("{source:?} meeting capture stream error: {stream_error}");
            let _ = error_tx.try_send(CaptureCommand::BackendStatus {
                source,
                status: CaptureBackendStatus::Failed,
            });
        },
        None,
    )
}

/// A single capture-clock origin shared by all CPAL streams in one meeting.
///
/// WASAPI provides microphone and loopback capture timestamps as global QPC
/// positions. The streams can reach their callbacks with very different driver
/// buffering, so using one `SourceClock` per callback would turn that delivery
/// latency into an incorrect, permanent alignment offset. This atomic one-time
/// publication keeps callback work lock-free: after the first frame it is one
/// pointer load plus timestamp arithmetic, without a mutex or channel wait.
#[derive(Default)]
struct SharedCpalCaptureClock {
    anchor: AtomicPtr<CpalCaptureClockAnchor>,
}

#[derive(Clone, Copy)]
struct CpalCaptureClockAnchor {
    capture: StreamInstant,
    meeting_timestamp_ns: i64,
}

impl SharedCpalCaptureClock {
    fn map_capture_time(&self, capture: StreamInstant, meeting_start: Instant) -> i64 {
        self.map_capture_time_at(capture, duration_to_ns(meeting_start.elapsed()))
    }

    fn map_capture_time_at(&self, capture: StreamInstant, observed_meeting_ns: i64) -> i64 {
        let observed_meeting_ns = observed_meeting_ns.max(0);
        let anchor = self.anchor_for(capture, observed_meeting_ns);
        let timestamp_ns = capture
            .duration_since(&anchor.capture)
            .map(duration_to_ns)
            .and_then(|delta| anchor.meeting_timestamp_ns.checked_add(delta))
            .or_else(|| {
                anchor
                    .capture
                    .duration_since(&capture)
                    .map(duration_to_ns)
                    .and_then(|delta| anchor.meeting_timestamp_ns.checked_sub(delta))
            })
            .unwrap_or(observed_meeting_ns);
        timestamp_ns.max(0)
    }

    fn anchor_for(
        &self,
        capture: StreamInstant,
        observed_meeting_ns: i64,
    ) -> &CpalCaptureClockAnchor {
        let current = self.anchor.load(Ordering::Acquire);
        if !current.is_null() {
            // The anchor is immutable after publication and this object is
            // owned by every callback through an `Arc`, so it remains alive.
            return unsafe { &*current };
        }

        let candidate = Box::into_raw(Box::new(CpalCaptureClockAnchor {
            capture,
            meeting_timestamp_ns: observed_meeting_ns,
        }));
        let anchor = match self.anchor.compare_exchange(
            std::ptr::null_mut(),
            candidate,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => candidate,
            Err(existing) => {
                // Another callback won the first-frame race. Its immutable
                // anchor is the meeting-wide source-clock origin.
                unsafe { drop(Box::from_raw(candidate)) };
                existing
            }
        };
        // `compare_exchange` can only fail with a non-null current value.
        debug_assert!(!anchor.is_null());
        unsafe { &*anchor }
    }
}

impl Drop for SharedCpalCaptureClock {
    fn drop(&mut self) {
        let anchor = self.anchor.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !anchor.is_null() {
            // Safe because `Drop` requires the final callback-held Arc to be
            // gone, and the anchor is never replaced after publication.
            unsafe { drop(Box::from_raw(anchor)) };
        }
    }
}

fn duration_to_ns(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

fn source_started_at_unix_ms(session_started_at_unix_ms: i64, meeting_start: Instant) -> i64 {
    let elapsed_ms = i64::try_from(meeting_start.elapsed().as_millis()).unwrap_or(i64::MAX);
    session_started_at_unix_ms.saturating_add(elapsed_ms)
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct MacSourceClock {
    first_source_timestamp_ns: Option<i64>,
    first_meeting_timestamp_ns: Option<i64>,
}

#[cfg(target_os = "macos")]
impl MacSourceClock {
    fn map_presentation_time(&mut self, source_timestamp_ns: i64, meeting_start: Instant) -> i64 {
        let fallback = duration_to_ns(meeting_start.elapsed());
        let first_source = self
            .first_source_timestamp_ns
            .get_or_insert(source_timestamp_ns);
        let first_meeting = *self.first_meeting_timestamp_ns.get_or_insert(fallback);
        source_timestamp_ns
            .checked_sub(*first_source)
            .and_then(|delta| first_meeting.checked_add(delta))
            .unwrap_or(fallback)
    }
}

/// Start the native ScreenCaptureKit audio output on macOS 13 or later.
///
/// ScreenCaptureKit delivers an independent CoreMedia presentation clock, so
/// the handler maps that clock to the same meeting origin used by CPAL. The
/// audio callback performs only a bounded copy and a non-blocking queue send;
/// chunk writing and conversion remain on the persistence worker.
#[cfg(target_os = "macos")]
fn macos_system_capture(
    command_tx: SyncSender<CaptureCommand>,
    meeting_start: Instant,
    session_started_at_unix_ms: i64,
) -> Result<screencapturekit::prelude::SCStream> {
    use screencapturekit::prelude::{
        CMSampleBufferExt, SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration,
        SCStreamOutputType,
    };

    let content = SCShareableContent::get().context("enumerate shareable macOS displays")?;
    let displays = content.displays();
    let display = displays
        .first()
        .context("No macOS display is available for system-audio capture")?;
    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();
    let configuration = SCStreamConfiguration::new()
        .with_captures_audio(true)
        .with_sample_rate(48_000)
        .with_channel_count(2);

    command_tx
        .send(CaptureCommand::FrameMetadata {
            source: CaptureSource::System,
            metadata: CaptureSourceMetadata {
                device_name: Some("macOS system audio".to_string()),
                backend: Some(system_backend_name().to_string()),
            },
        })
        .map_err(|error| anyhow!("meeting capture worker stopped unexpectedly: {error}"))?;

    let mut stream = SCStream::new(&filter, &configuration);
    let callback_tx = command_tx.clone();
    let clock = Arc::new(std::sync::Mutex::new(MacSourceClock::default()));
    let callback_clock = Arc::clone(&clock);
    let mut sequence = 0_u64;
    let mut dropped_frames = 0_u64;
    if stream
        .add_output_handler(
            move |sample, _output_type| {
                let Some(audio_buffers) = sample.audio_buffer_list() else {
                    return;
                };
                let Some((samples, channels)) = decode_macos_float_audio(&audio_buffers) else {
                    // Unexpected CoreAudio format is a backend failure, but do not
                    // panic across Apple's callback boundary.
                    let _ = callback_tx.try_send(CaptureCommand::BackendStatus {
                        source: CaptureSource::System,
                        status: CaptureBackendStatus::Failed,
                    });
                    return;
                };
                let frame_count = (samples.len() / usize::from(channels)) as u64;
                let presentation = sample.presentation_timestamp();
                let source_timestamp_ns = if presentation.timescale > 0 {
                    let value_i128 = i128::from(presentation.value);
                    let timescale_i128 = i128::from(presentation.timescale);
                    let ns_i128 = value_i128
                        .saturating_mul(1_000_000_000)
                        .saturating_div(timescale_i128);
                    ns_i128.clamp(i64::MIN as i128, i64::MAX as i128) as i64
                } else {
                    duration_to_ns(meeting_start.elapsed())
                };
                let timestamp_ns = callback_clock
                    .lock()
                    .map(|mut clock| {
                        clock.map_presentation_time(source_timestamp_ns, meeting_start)
                    })
                    .unwrap_or_else(|_| duration_to_ns(meeting_start.elapsed()));
                let frame = TimestampedAudioFrame {
                    source: CaptureSource::System,
                    timestamp_ns,
                    sequence,
                    sample_rate: 48_000,
                    channels,
                    samples,
                    dropped_frames,
                };
                sequence = sequence.saturating_add(1);
                match callback_tx.try_send(CaptureCommand::Frame(frame)) {
                    Ok(()) => dropped_frames = 0,
                    Err(TrySendError::Full(_)) => {
                        dropped_frames = dropped_frames.saturating_add(frame_count)
                    }
                    Err(TrySendError::Disconnected(_)) => {}
                }
            },
            SCStreamOutputType::Audio,
        )
        .is_none()
    {
        return Err(anyhow!(
            "ScreenCaptureKit rejected the system-audio output handler"
        ));
    }
    stream
        .start_capture()
        .context("start ScreenCaptureKit system-audio stream")?;
    command_tx
        .send(CaptureCommand::SourceStarted {
            source: CaptureSource::System,
            started_at_unix_ms: source_started_at_unix_ms(
                session_started_at_unix_ms,
                meeting_start,
            ),
        })
        .map_err(|error| anyhow!("meeting capture worker stopped unexpectedly: {error}"))?;
    Ok(stream)
}

/// ScreenCaptureKit's requested audio output is linear PCM f32. It may be
/// delivered as one interleaved buffer or one planar buffer per channel; copy
/// either form into the common interleaved frame representation.
#[cfg(target_os = "macos")]
fn decode_macos_float_audio(
    buffers: &screencapturekit::AudioBufferList,
) -> Option<(Vec<f32>, u16)> {
    let mut decoded: Vec<(u16, Vec<f32>)> = Vec::new();
    for buffer in buffers {
        let channels = u16::try_from(buffer.number_channels).ok()?;
        if channels == 0 || buffer.data().len() % std::mem::size_of::<f32>() != 0 {
            return None;
        }
        let values = buffer
            .data()
            .chunks_exact(std::mem::size_of::<f32>())
            .map(|bytes| f32::from_ne_bytes(bytes.try_into().expect("f32 chunk size")))
            .collect::<Vec<_>>();
        decoded.push((channels, values));
    }
    match decoded.as_slice() {
        [(channels, samples)] if *channels > 0 && samples.len() % usize::from(*channels) == 0 => {
            Some((samples.clone(), *channels))
        }
        _ if decoded.len() >= 2 && decoded.iter().all(|(channels, _)| *channels == 1) => {
            let frames = decoded.first()?.1.len();
            if decoded.iter().any(|(_, samples)| samples.len() != frames) {
                return None;
            }
            let channel_count = u16::try_from(decoded.len()).ok()?;
            let mut interleaved = Vec::with_capacity(frames * decoded.len());
            for frame in 0..frames {
                for (_, channel_samples) in &decoded {
                    interleaved.push(channel_samples[frame]);
                }
            }
            Some((interleaved, channel_count))
        }
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn selected_output_device(host: &cpal::Host, configured_name: Option<&str>) -> Option<Device> {
    let configured_name = configured_name?;
    host.output_devices().ok()?.find(|device| {
        device
            .name()
            .map(|name| name == configured_name)
            .unwrap_or(false)
    })
}

// PipeWire's native graph and PulseAudio-compatible server use different
// discovery APIs. Both paths write identical 48 kHz f32 frames so the durable
// capture session remains independent of the server chosen by a distribution.
#[cfg(target_os = "linux")]
const LINUX_CAPTURE_SAMPLE_RATE: u32 = 48_000;
#[cfg(target_os = "linux")]
const LINUX_CAPTURE_CHANNELS: u16 = 2;
#[cfg(target_os = "linux")]
const LINUX_CAPTURE_FRAME_COUNT: usize = 960; // 20 ms at 48 kHz.
#[cfg(target_os = "linux")]
const LINUX_CAPTURE_FRAME_BYTES: usize =
    LINUX_CAPTURE_FRAME_COUNT * LINUX_CAPTURE_CHANNELS as usize * std::mem::size_of::<f32>();
#[cfg(target_os = "linux")]
const LINUX_CAPTURE_READ_BUFFER_BYTES: usize = LINUX_CAPTURE_FRAME_BYTES * 4;
#[cfg(target_os = "linux")]
const LINUX_CLOCK_DISCONTINUITY_THRESHOLD: Duration = Duration::from_millis(250);
#[cfg(target_os = "linux")]
const LINUX_CAPTURE_STDERR_LIMIT: usize = 16 * 1024;

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct LinuxPulseMonitor {
    sink_name: String,
    monitor_source: String,
    display_name: String,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct LinuxPipeWireTarget {
    node_name: String,
    display_name: String,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct LinuxPulseSink {
    name: String,
    description: Option<String>,
    device_description: Option<String>,
    monitor_source: Option<String>,
}

/// Owns the child process used for Linux system capture and the two threads
/// that drain its stdout/stderr. It is intentionally separate from CPAL: the
/// ALSA host does not expose PipeWire/Pulse monitor sources consistently.
#[cfg(target_os = "linux")]
struct LinuxSystemCapture {
    child: Child,
    audio_reader: Option<std::thread::JoinHandle<()>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    stop_requested: Arc<AtomicBool>,
    stopped: bool,
}

#[cfg(target_os = "linux")]
impl LinuxSystemCapture {
    fn start_parec(
        monitor: &LinuxPulseMonitor,
        command_tx: SyncSender<CaptureCommand>,
        meeting_start: Instant,
        session_started_at_unix_ms: i64,
    ) -> Result<Self> {
        let mut command = Command::new("parec");
        command
            .args([
                "--raw",
                "--format=float32le",
                "--rate=48000",
                "--channels=2",
            ])
            .arg("--device")
            .arg(&monitor.monitor_source)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().with_context(|| {
            format!(
                "launch parec for PulseAudio/PipeWire monitor {}",
                monitor.monitor_source
            )
        })?;
        Self::from_child(
            child,
            command_tx,
            meeting_start,
            session_started_at_unix_ms,
            format!("{} ({})", monitor.display_name, monitor.monitor_source),
            "pulse-monitor-parec",
        )
    }

    fn start_pw_record(
        target: &LinuxPipeWireTarget,
        command_tx: SyncSender<CaptureCommand>,
        meeting_start: Instant,
        session_started_at_unix_ms: i64,
    ) -> Result<Self> {
        let mut command = Command::new("pw-record");
        command
            .args([
                "--raw",
                "--format=f32",
                "--rate=48000",
                "--channels=2",
                "--latency=50ms",
                // This directs a capture stream at the target sink's monitor
                // ports rather than letting the session manager select a mic.
                "--properties={\"stream.capture.sink\":true}",
            ])
            .arg("--target")
            .arg(&target.node_name)
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().with_context(|| {
            format!(
                "launch pw-record for PipeWire sink monitor {}",
                target.node_name
            )
        })?;
        Self::from_child(
            child,
            command_tx,
            meeting_start,
            session_started_at_unix_ms,
            target.display_name.clone(),
            "pipewire-monitor-pw-record",
        )
    }

    fn from_child(
        mut child: Child,
        command_tx: SyncSender<CaptureCommand>,
        meeting_start: Instant,
        session_started_at_unix_ms: i64,
        device_name: String,
        backend_name: &'static str,
    ) -> Result<Self> {
        let Some(stdout) = child.stdout.take() else {
            terminate_linux_child(&mut child);
            return Err(anyhow!(
                "Linux system-capture process did not provide stdout"
            ));
        };
        let stderr = child.stderr.take();
        let stop_requested = Arc::new(AtomicBool::new(false));

        if let Err(error) = command_tx.send(CaptureCommand::FrameMetadata {
            source: CaptureSource::System,
            metadata: CaptureSourceMetadata {
                device_name: Some(device_name),
                backend: Some(backend_name.to_string()),
            },
        }) {
            terminate_linux_child(&mut child);
            return Err(anyhow!(
                "meeting capture worker stopped before Linux system capture metadata: {error}"
            ));
        }
        if let Err(error) = command_tx.send(CaptureCommand::BackendStatus {
            source: CaptureSource::System,
            status: CaptureBackendStatus::Capturing,
        }) {
            terminate_linux_child(&mut child);
            return Err(anyhow!(
                "meeting capture worker stopped before Linux system capture began: {error}"
            ));
        }

        let (reader_ready_tx, reader_ready_rx) = mpsc::sync_channel(0);
        let (reader_start_tx, reader_start_rx) = mpsc::sync_channel(0);
        let reader_stop = Arc::clone(&stop_requested);
        let reader_tx = command_tx.clone();
        let audio_reader = match std::thread::Builder::new()
            .name("linux-system-audio-reader".to_string())
            .spawn(move || {
                if reader_ready_tx.send(()).is_ok() && reader_start_rx.recv().is_ok() {
                    read_linux_system_audio(stdout, reader_tx, reader_stop, meeting_start);
                }
            }) {
            Ok(reader) => reader,
            Err(error) => {
                stop_requested.store(true, Ordering::Release);
                terminate_linux_child(&mut child);
                let _ = command_tx.send(CaptureCommand::BackendStatus {
                    source: CaptureSource::System,
                    status: CaptureBackendStatus::Failed,
                });
                return Err(error).context("start Linux system-audio reader");
            }
        };
        if let Err(error) = reader_ready_rx.recv() {
            stop_requested.store(true, Ordering::Release);
            drop(reader_start_tx);
            terminate_linux_child(&mut child);
            let _ = audio_reader.join();
            let _ = command_tx.send(CaptureCommand::BackendStatus {
                source: CaptureSource::System,
                status: CaptureBackendStatus::Failed,
            });
            return Err(anyhow!(
                "Linux system-audio reader stopped before it was ready: {error}"
            ));
        }

        let stderr_reader = stderr.map(|stderr| {
            let stderr_stop = Arc::clone(&stop_requested);
            std::thread::spawn(move || {
                drain_linux_capture_stderr(stderr, stderr_stop, backend_name)
            })
        });

        let mut capture = Self {
            child,
            audio_reader: Some(audio_reader),
            stderr_reader,
            stop_requested,
            stopped: false,
        };
        if let Err(error) = command_tx.send(CaptureCommand::SourceStarted {
            source: CaptureSource::System,
            started_at_unix_ms: source_started_at_unix_ms(
                session_started_at_unix_ms,
                meeting_start,
            ),
        }) {
            drop(reader_start_tx);
            capture.stop_inner();
            return Err(anyhow!(
                "meeting capture worker stopped before Linux system capture was ready: {error}"
            ));
        }
        if let Err(error) = reader_start_tx.send(()) {
            capture.stop_inner();
            return Err(anyhow!(
                "Linux system-audio reader stopped before capture could begin: {error}"
            ));
        }
        Ok(capture)
    }

    fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.stop_requested.store(true, Ordering::Release);
        terminate_linux_child(&mut self.child);
        if let Some(reader) = self.audio_reader.take() {
            if reader.join().is_err() {
                warn!("Linux system-audio reader thread panicked during shutdown");
            }
        }
        if let Some(reader) = self.stderr_reader.take() {
            if reader.join().is_err() {
                warn!("Linux system-audio stderr reader thread panicked during shutdown");
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxSystemCapture {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

#[cfg(target_os = "linux")]
fn start_linux_system_capture(
    command_tx: SyncSender<CaptureCommand>,
    configured_output_device: Option<&str>,
    meeting_start: Instant,
    session_started_at_unix_ms: i64,
) -> Result<LinuxSystemCapture> {
    match linux_pulse_monitor(configured_output_device) {
        Ok(monitor) => match LinuxSystemCapture::start_parec(
            &monitor,
            command_tx.clone(),
            meeting_start,
            session_started_at_unix_ms,
        ) {
            Ok(capture) => Ok(capture),
            Err(error) if linux_command_was_not_found(&error) => {
                warn!(
                    "parec is not installed; falling back to PipeWire's native pw-record monitor capture"
                );
                LinuxSystemCapture::start_pw_record(
                    &LinuxPipeWireTarget {
                        node_name: monitor.sink_name,
                        display_name: monitor.display_name,
                    },
                    command_tx,
                    meeting_start,
                    session_started_at_unix_ms,
                )
            }
            Err(error) => Err(error),
        },
        Err(pulse_error) => {
            warn!(
                "PulseAudio-compatible monitor discovery is unavailable ({pulse_error:#}); trying native PipeWire capture"
            );
            let target = linux_pipewire_target(configured_output_device).with_context(|| {
                format!(
                    "resolve native PipeWire sink after Pulse monitor discovery failed: {pulse_error:#}"
                )
            })?;
            LinuxSystemCapture::start_pw_record(
                &target,
                command_tx,
                meeting_start,
                session_started_at_unix_ms,
            )
        }
    }
}

#[cfg(target_os = "linux")]
fn terminate_linux_child(child: &mut Child) {
    if let Err(error) = child.kill() {
        if !matches!(
            error.kind(),
            io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
        ) {
            warn!("Failed to stop Linux system-audio capture process: {error}");
        }
    }
    if let Err(error) = child.wait() {
        warn!("Failed to reap Linux system-audio capture process: {error}");
    }
}

#[cfg(target_os = "linux")]
fn linux_command_was_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<io::Error>()
            .is_some_and(|source| source.kind() == io::ErrorKind::NotFound)
    })
}

#[cfg(target_os = "linux")]
fn read_linux_system_audio(
    mut stdout: std::process::ChildStdout,
    command_tx: SyncSender<CaptureCommand>,
    stop_requested: Arc<AtomicBool>,
    meeting_start: Instant,
) {
    let mut raw_buffer = Vec::with_capacity(LINUX_CAPTURE_READ_BUFFER_BYTES * 2);
    let mut read_buffer = [0_u8; LINUX_CAPTURE_READ_BUFFER_BYTES];
    let mut sequence = 0_u64;
    let mut dropped_frames = 0_u64;
    let mut clock = LinuxProcessClock::default();
    let mut failed = None;

    loop {
        match stdout.read(&mut read_buffer) {
            Ok(0) => {
                if !stop_requested.load(Ordering::Acquire) {
                    failed = Some("the capture process closed its audio stream".to_string());
                }
                break;
            }
            Ok(bytes_read) => {
                raw_buffer.extend_from_slice(&read_buffer[..bytes_read]);
                let complete_bytes =
                    raw_buffer.len() / LINUX_CAPTURE_FRAME_BYTES * LINUX_CAPTURE_FRAME_BYTES;
                for encoded_frame in
                    raw_buffer[..complete_bytes].chunks_exact(LINUX_CAPTURE_FRAME_BYTES)
                {
                    let frame_count = LINUX_CAPTURE_FRAME_COUNT as u64;
                    let timestamp_ns = clock.next_timestamp(frame_count, meeting_start);
                    let frame = TimestampedAudioFrame {
                        source: CaptureSource::System,
                        timestamp_ns,
                        sequence,
                        sample_rate: LINUX_CAPTURE_SAMPLE_RATE,
                        channels: LINUX_CAPTURE_CHANNELS,
                        samples: decode_linux_f32_frame(encoded_frame),
                        dropped_frames,
                    };
                    sequence = sequence.saturating_add(1);
                    match command_tx.try_send(CaptureCommand::Frame(frame)) {
                        Ok(()) => dropped_frames = 0,
                        Err(TrySendError::Full(_)) => {
                            dropped_frames = dropped_frames.saturating_add(frame_count);
                        }
                        Err(TrySendError::Disconnected(_)) => return,
                    }
                }
                if complete_bytes > 0 {
                    raw_buffer.drain(..complete_bytes);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if !stop_requested.load(Ordering::Acquire) {
                    failed = Some(format!("failed to read monitor PCM: {error}"));
                }
                break;
            }
        }
    }

    if let Some(reason) = failed {
        warn!("Linux system-audio capture failed: {reason}");
        let _ = command_tx.send(CaptureCommand::BackendStatus {
            source: CaptureSource::System,
            status: CaptureBackendStatus::Failed,
        });
    }
}

#[cfg(target_os = "linux")]
fn decode_linux_f32_frame(encoded_frame: &[u8]) -> Vec<f32> {
    encoded_frame
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|bytes| {
            let sample = f32::from_le_bytes(bytes.try_into().unwrap_or([0; 4]));
            if sample.is_finite() {
                sample
            } else {
                0.0
            }
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn drain_linux_capture_stderr(
    mut stderr: std::process::ChildStderr,
    stop_requested: Arc<AtomicBool>,
    backend_name: &str,
) {
    let mut bytes = Vec::with_capacity(LINUX_CAPTURE_STDERR_LIMIT);
    let mut read_buffer = [0_u8; 1024];
    loop {
        match stderr.read(&mut read_buffer) {
            Ok(0) => break,
            Ok(read) if bytes.len() < LINUX_CAPTURE_STDERR_LIMIT => {
                let remaining = LINUX_CAPTURE_STDERR_LIMIT - bytes.len();
                bytes.extend_from_slice(&read_buffer[..read.min(remaining)]);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if !stop_requested.load(Ordering::Acquire) {
                    warn!("Failed to read {backend_name} stderr: {error}");
                }
                return;
            }
        }
    }
    if !bytes.is_empty() && !stop_requested.load(Ordering::Acquire) {
        let detail = String::from_utf8_lossy(&bytes).trim().to_string();
        if !detail.is_empty() {
            warn!("{backend_name} reported: {detail}");
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct LinuxProcessClock {
    next_timestamp_ns: Option<i64>,
}

#[cfg(target_os = "linux")]
impl LinuxProcessClock {
    fn next_timestamp(&mut self, frame_count: u64, meeting_start: Instant) -> i64 {
        self.next_timestamp_at(frame_count, duration_to_ns(meeting_start.elapsed()))
    }

    fn next_timestamp_at(&mut self, frame_count: u64, observed_timestamp_ns: i64) -> i64 {
        let expected_timestamp_ns = self.next_timestamp_ns.unwrap_or(observed_timestamp_ns);
        let discontinuity_ns = duration_to_ns(LINUX_CLOCK_DISCONTINUITY_THRESHOLD);
        let timestamp_ns =
            if observed_timestamp_ns.saturating_sub(expected_timestamp_ns) > discontinuity_ns {
                observed_timestamp_ns
            } else {
                expected_timestamp_ns
            };
        self.next_timestamp_ns = Some(timestamp_ns.saturating_add(linux_frames_to_ns(frame_count)));
        timestamp_ns
    }
}

#[cfg(target_os = "linux")]
fn linux_frames_to_ns(frame_count: u64) -> i64 {
    let nanoseconds = u128::from(frame_count)
        .saturating_mul(1_000_000_000)
        .saturating_div(u128::from(LINUX_CAPTURE_SAMPLE_RATE));
    i64::try_from(nanoseconds).unwrap_or(i64::MAX)
}

#[cfg(target_os = "linux")]
fn linux_pulse_monitor(configured_output_device: Option<&str>) -> Result<LinuxPulseMonitor> {
    let default_sink = linux_command_output("pactl", &["get-default-sink"])
        .context("query the default PulseAudio/PipeWire sink")?;
    let default_sink = default_sink
        .lines()
        .next()
        .map(str::trim)
        .filter(|sink| !sink.is_empty())
        .context("PulseAudio/PipeWire did not report a default sink")?;
    let source_names = parse_pactl_short_names(
        &linux_command_output("pactl", &["list", "short", "sources"])
            .context("list PulseAudio/PipeWire sources")?,
    );
    if source_names.is_empty() {
        return Err(anyhow!(
            "PulseAudio/PipeWire did not expose any recording sources"
        ));
    }

    let sinks = linux_command_output("pactl", &["-f", "json", "list", "sinks"])
        .ok()
        .map(|raw| parse_pulse_sinks_json(&raw))
        .unwrap_or_default();
    let selected_sink = select_pulse_sink(&sinks, configured_output_device, default_sink);
    let sink_name = selected_sink
        .map(|sink| sink.name.clone())
        .unwrap_or_else(|| default_sink.to_string());
    let display_name = selected_sink
        .and_then(pulse_sink_display_name)
        .unwrap_or_else(|| sink_name.clone());

    let mut monitor_candidates = selected_sink
        .and_then(|sink| sink.monitor_source.clone())
        .into_iter()
        .collect::<Vec<_>>();
    monitor_candidates.push(format!("{sink_name}.monitor"));
    if sink_name != default_sink {
        monitor_candidates.push(format!("{default_sink}.monitor"));
    }
    let monitor_source = monitor_candidates
        .into_iter()
        .find(|candidate| source_names.iter().any(|source| source == candidate))
        .context("No PulseAudio/PipeWire monitor source is available")?;

    Ok(LinuxPulseMonitor {
        sink_name,
        monitor_source,
        display_name,
    })
}

#[cfg(target_os = "linux")]
fn linux_pipewire_target(configured_output_device: Option<&str>) -> Result<LinuxPipeWireTarget> {
    let graph = linux_command_output("pw-dump", &[]).ok();
    let sinks = graph
        .as_deref()
        .map(parse_pipewire_sinks)
        .unwrap_or_default();
    let default_sink = graph
        .as_deref()
        .and_then(parse_pipewire_default_sink)
        .or_else(|| linux_wpctl_default_sink().ok());

    let selected = configured_output_device
        .and_then(|configured| {
            sinks.iter().find(|sink| {
                sink.node_name.eq_ignore_ascii_case(configured)
                    || sink
                        .description
                        .as_deref()
                        .is_some_and(|description| description.eq_ignore_ascii_case(configured))
            })
        })
        .or_else(|| {
            default_sink.as_deref().and_then(|default_sink| {
                sinks.iter().find(|sink| {
                    sink.node_name == default_sink || sink.object_serial == default_sink
                })
            })
        })
        .or_else(|| (sinks.len() == 1).then(|| &sinks[0]));

    if let Some(sink) = selected {
        return Ok(LinuxPipeWireTarget {
            node_name: sink.node_name.clone(),
            display_name: sink
                .description
                .clone()
                .unwrap_or_else(|| sink.node_name.clone()),
        });
    }
    if let Some(node_name) = default_sink {
        return Ok(LinuxPipeWireTarget {
            display_name: node_name.clone(),
            node_name,
        });
    }
    if let Some(configured_output_device) = configured_output_device.filter(|name| !name.is_empty())
    {
        return Ok(LinuxPipeWireTarget {
            node_name: configured_output_device.to_string(),
            display_name: configured_output_device.to_string(),
        });
    }
    Err(anyhow!(
        "No PipeWire output sink could be resolved for system-audio capture"
    ))
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct LinuxPipeWireSink {
    node_name: String,
    description: Option<String>,
    object_serial: String,
}

#[cfg(target_os = "linux")]
fn linux_command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {program}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "{program} exited with {}{}",
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn parse_pactl_short_names(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(target_os = "linux")]
fn parse_pulse_sinks_json(output: &str) -> Vec<LinuxPulseSink> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sink| {
            let name = json_property(sink, "name")?.to_string();
            Some(LinuxPulseSink {
                description: json_property(sink, "description").map(str::to_string),
                device_description: json_property(sink, "device.description").map(str::to_string),
                monitor_source: json_property(sink, "monitor_source").map(str::to_string),
                name,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn json_property<'a>(value: &'a serde_json::Value, property: &str) -> Option<&'a str> {
    value
        .get(property)
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .and_then(|properties| properties.get(property))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            value
                .get("info")
                .and_then(|info| info.get("props"))
                .and_then(serde_json::Value::as_object)
                .and_then(|properties| properties.get(property))
                .and_then(serde_json::Value::as_str)
        })
}

#[cfg(target_os = "linux")]
fn select_pulse_sink<'a>(
    sinks: &'a [LinuxPulseSink],
    configured_output_device: Option<&str>,
    default_sink: &str,
) -> Option<&'a LinuxPulseSink> {
    configured_output_device
        .and_then(|configured| {
            sinks.iter().find(|sink| {
                sink.name.eq_ignore_ascii_case(configured)
                    || sink
                        .description
                        .as_deref()
                        .is_some_and(|description| description.eq_ignore_ascii_case(configured))
                    || sink
                        .device_description
                        .as_deref()
                        .is_some_and(|description| description.eq_ignore_ascii_case(configured))
            })
        })
        .or_else(|| sinks.iter().find(|sink| sink.name == default_sink))
}

#[cfg(target_os = "linux")]
fn pulse_sink_display_name(sink: &LinuxPulseSink) -> Option<String> {
    sink.description
        .clone()
        .or_else(|| sink.device_description.clone())
        .or_else(|| Some(sink.name.clone()))
}

#[cfg(target_os = "linux")]
fn parse_pipewire_sinks(output: &str) -> Vec<LinuxPipeWireSink> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|object| {
            (json_property(object, "media.class") == Some("Audio/Sink")).then_some(())?;
            let node_name = json_property(object, "node.name")?.to_string();
            let object_serial = json_property(object, "object.serial")
                .map(str::to_string)
                .or_else(|| object.get("id").map(json_value_to_string))?;
            Some(LinuxPipeWireSink {
                description: json_property(object, "node.description")
                    .or_else(|| json_property(object, "device.description"))
                    .map(str::to_string),
                node_name,
                object_serial,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn parse_pipewire_default_sink(output: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(output).ok()?;
    value.as_array()?.iter().find_map(|object| {
        (json_property(object, "metadata.name") == Some("default")).then_some(())?;
        object
            .get("metadata")?
            .as_array()?
            .iter()
            .find_map(|entry| {
                (entry.get("key")?.as_str() == "default.audio.sink").then_some(())?;
                let value = entry.get("value")?.as_str()?;
                serde_json::from_str::<serde_json::Value>(value)
                    .ok()
                    .and_then(|value| value.get("name")?.as_str().map(str::to_string))
                    .or_else(|| {
                        let value = value.trim_matches('"').trim();
                        (!value.is_empty()).then(|| value.to_string())
                    })
            })
    })
}

#[cfg(target_os = "linux")]
fn linux_wpctl_default_sink() -> Result<String> {
    let output = linux_command_output("wpctl", &["inspect", "@DEFAULT_AUDIO_SINK@"])?;
    output
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "node.name").then(|| value.trim().trim_matches('"').to_string())
        })
        .filter(|name| !name.is_empty())
        .context("wpctl did not report the default sink node name")
}

#[cfg(target_os = "linux")]
fn json_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_cpal_clock_uses_capture_time_not_callback_delivery_time() {
        let clock = SharedCpalCaptureClock::default();
        let microphone_capture = StreamInstant::new(42, 100_000_000);
        let loopback_capture = StreamInstant::new(42, 180_000_000);
        let later_microphone_capture = StreamInstant::new(42, 200_000_000);

        // The microphone callback establishes the meeting origin at 8 ms. The
        // loopback callback arrives much later, but its QPC sample timestamp
        // is only 80 ms after the microphone sample and must stay at 88 ms.
        assert_eq!(
            clock.map_capture_time_at(microphone_capture, 8_000_000),
            8_000_000
        );
        assert_eq!(
            clock.map_capture_time_at(loopback_capture, 70_000_000),
            88_000_000
        );
        assert_eq!(
            clock.map_capture_time_at(later_microphone_capture, 75_000_000),
            108_000_000
        );
    }

    #[test]
    fn shared_cpal_clock_preserves_a_capture_that_predates_the_first_callback() {
        let clock = SharedCpalCaptureClock::default();
        let first_callback_capture = StreamInstant::new(12, 800_000_000);
        let earlier_capture = StreamInstant::new(12, 760_000_000);

        assert_eq!(
            clock.map_capture_time_at(first_callback_capture, 120_000_000),
            120_000_000
        );
        // A device with a deeper callback buffer can deliver an older sample
        // after another stream has already set the shared QPC anchor.
        assert_eq!(
            clock.map_capture_time_at(earlier_capture, 125_000_000),
            80_000_000
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[test]
    fn selects_a_configured_pulse_sink_by_its_device_description() {
        let sinks = vec![
            LinuxPulseSink {
                name: "alsa_output.hdmi".to_string(),
                description: Some("HDMI".to_string()),
                device_description: Some("Display Audio".to_string()),
                monitor_source: None,
            },
            LinuxPulseSink {
                name: "alsa_output.analog".to_string(),
                description: Some("Analog Stereo".to_string()),
                device_description: Some("Laptop Speakers".to_string()),
                monitor_source: None,
            },
        ];

        let selected = select_pulse_sink(&sinks, Some("Laptop Speakers"), "alsa_output.hdmi");

        assert_eq!(
            selected.map(|sink| sink.name.as_str()),
            Some("alsa_output.analog")
        );
    }

    #[test]
    fn parses_monitor_source_from_pactl_json() {
        let sinks = parse_pulse_sinks_json(
            r#"[{"name":"alsa_output.analog","description":"Analog Stereo","monitor_source":"alsa_output.analog.monitor"}]"#,
        );

        assert_eq!(
            sinks
                .first()
                .and_then(|sink| sink.monitor_source.as_deref()),
            Some("alsa_output.analog.monitor")
        );
    }

    #[test]
    fn parses_default_pipewire_sink_from_default_metadata() {
        let default_sink = parse_pipewire_default_sink(
            r#"[{"info":{"props":{"metadata.name":"default"}},"metadata":[{"key":"default.audio.sink","value":"{\"name\":\"alsa_output.analog\"}"}]}]"#,
        );

        assert_eq!(default_sink.as_deref(), Some("alsa_output.analog"));
    }

    #[test]
    fn decodes_raw_f32_and_replaces_non_finite_samples() {
        let raw = [0.25_f32.to_le_bytes(), f32::NAN.to_le_bytes()].concat();

        assert_eq!(decode_linux_f32_frame(&raw), vec![0.25, 0.0]);
    }

    #[test]
    fn source_clock_uses_sample_time_but_records_real_discontinuities() {
        let mut clock = LinuxProcessClock::default();
        let first = clock.next_timestamp_at(960, 1_000);
        let second = clock.next_timestamp_at(960, 5_000);
        let resumed = clock.next_timestamp_at(960, 300_000_001);

        assert_eq!((first, second, resumed), (1_000, 20_001_000, 300_000_001));
    }
}

fn microphone_backend_name() -> &'static str {
    #[cfg(target_os = "windows")]
    return "wasapi-input";
    #[cfg(target_os = "macos")]
    return "coreaudio-input";
    #[cfg(target_os = "linux")]
    return "cpal-input";
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    return "cpal-input";
}

fn system_backend_name() -> &'static str {
    #[cfg(target_os = "windows")]
    return "wasapi-loopback";
    #[cfg(target_os = "macos")]
    return "screencapturekit-audio";
    #[cfg(target_os = "linux")]
    return "pipewire-pulse-monitor";
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    return "unavailable";
}
