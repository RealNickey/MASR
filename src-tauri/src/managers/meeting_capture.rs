//! Crash-resilient, timestamped storage for an active meeting capture.
//!
//! Capture adapters own platform APIs (WASAPI, ScreenCaptureKit, PipeWire,
//! etc.) and send their native-rate frames here. This module deliberately does
//! not attempt to align sources by buffer length: microphone and system audio
//! have independent clocks, so every frame carries its own monotonic timestamp.
//!
//! Source WAVs are retained as 32-bit PCM/native-rate chunks. On a clean
//! finalize (or a checkpointed crash recovery), the session derives 16 kHz mono
//! microphone, system, and convenience mix WAVs for the existing ASR/history
//! contract. `mix.wav` is never the source of truth.

use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use log::warn;
use rubato::{FftFixedIn, Resampler};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::diarization::{
    CaptureBackendStatus, CaptureSource, CaptureTrackManifest, CaptureTrackStatus,
    MeetingCaptureManifest, CAPTURE_MANIFEST_VERSION,
};
use super::history::AudioTracks;

/// The ASR-compatible sample rate used by the existing transcription pipeline.
pub const ASR_SAMPLE_RATE: u32 = 16_000;
/// Source WAVs are capped at five minutes, so a crash cannot invalidate an
/// entire meeting recording.
pub const SOURCE_CHUNK_DURATION_SECONDS: u64 = 5 * 60;

const SOURCE_CHUNK_DURATION_NS: i64 = SOURCE_CHUNK_DURATION_SECONDS as i64 * 1_000_000_000;
const CHECKPOINT_INTERVAL_NS: i64 = 10 * 1_000_000_000;
const TIMESTAMP_JITTER_TOLERANCE_NS: i64 = 2_000_000;
const RESAMPLER_CHUNK_SIZE: usize = 1024;
const CAPTURE_SESSION_MANIFEST_VERSION: u32 = 1;
const CHECKPOINT_A: &str = ".capture-checkpoint-a.json";
const CHECKPOINT_B: &str = ".capture-checkpoint-b.json";
const MANIFEST_FILE: &str = "manifest.json";
const SOURCE_DIR: &str = "source";
const TIMELINE_DIR: &str = "timeline";

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

type WavFileWriter = WavWriter<BufWriter<File>>;

fn source_name(source: CaptureSource) -> &'static str {
    match source {
        CaptureSource::Microphone => "microphone",
        CaptureSource::System => "system",
        CaptureSource::Mix => "mix",
    }
}

/// Static device/backend information supplied by a platform capture adapter.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureSourceMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

/// Optional metadata passed when a capture session is created.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingCaptureMetadata {
    #[serde(default)]
    pub microphone: CaptureSourceMetadata,
    #[serde(default)]
    pub system: CaptureSourceMetadata,
}

/// One frame from a native capture adapter.
///
/// `timestamp_ns` must use the session-wide monotonic origin, not wall clock
/// time. `samples` are interleaved f32 samples at `sample_rate`/`channels`;
/// keeping this native representation is what preserves archival quality.
#[derive(Clone, Debug)]
pub struct TimestampedAudioFrame {
    pub source: CaptureSource,
    pub timestamp_ns: i64,
    pub sequence: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
    /// Frames known to have been dropped by the adapter since its preceding
    /// callback. The next timestamp still determines the actual timeline gap.
    pub dropped_frames: u64,
}

impl TimestampedAudioFrame {
    pub fn frame_count(&self) -> Result<u64> {
        let channels = usize::from(self.channels);
        if channels == 0 || self.samples.len() % channels != 0 {
            bail!(
                "{} frame has {} samples that are not divisible by {} channels",
                source_name(self.source),
                self.samples.len(),
                self.channels
            );
        }
        Ok((self.samples.len() / channels) as u64)
    }
}

/// Native format of a retained source chunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    /// Capture adapters normalize their native PCM samples to f32 in callback
    /// memory; chunks store a 32-bit PCM WAV representation for broad archival
    /// and BWF-tool compatibility without reducing practical device fidelity.
    pub sample_format: String,
}

impl SourceAudioFormat {
    fn from_frame(frame: &TimestampedAudioFrame) -> Self {
        Self {
            sample_rate: frame.sample_rate,
            channels: frame.channels,
            sample_format: "pcm_s32le".to_owned(),
        }
    }
}

/// A finalized or checkpointed native source WAV chunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceChunkManifest {
    pub path: String,
    pub index: u64,
    pub format: SourceAudioFormat,
    pub started_at_timestamp_ns: i64,
    pub ended_at_timestamp_ns: i64,
    pub sample_frames: u64,
    /// False means the chunk was active when the most recent checkpoint was
    /// written. Its WAV header is flushed before that checkpoint, so recovery
    /// can safely use the recorded portion.
    pub complete: bool,
}

/// A contiguous time span inside a source chunk. These spans, rather than WAV
/// length, are used to build the ASR timeline after independently clocked
/// microphone/system sources are captured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSegmentManifest {
    pub chunk_path: String,
    pub source_frame_offset: u64,
    pub source_frame_count: u64,
    pub timeline_start_timestamp_ns: i64,
    pub timeline_end_timestamp_ns: i64,
    pub sequence_start: u64,
    pub sequence_end: u64,
}

/// Aggregate timing information. Detailed per-callback timestamps live in the
/// referenced JSONL index so routine checkpoints do not rewrite a large
/// manifest during a long meeting.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackTimingManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_timestamp_ns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_timestamp_ns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_offset_ns: Option<i64>,
    /// Optional adapter calibration from a device clock to the shared meeting
    /// clock. The exact nanosecond value complements the millisecond field in
    /// the frozen diarization manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibrated_clock_offset_ns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_clock_drift_ppm: Option<f64>,
    pub input_frames: u64,
    pub stored_frames: u64,
    /// Sum of the native-duration frames received from this device clock. This
    /// is retained separately from the meeting timeline so drift can be
    /// estimated when no explicit gap or overlap was observed.
    pub source_audio_duration_ns: i64,
    pub dropped_frames: u64,
    pub gap_count: u64,
    pub gap_duration_ns: i64,
    pub overlap_count: u64,
    pub overlap_duration_ns: i64,
    pub out_of_order_sequence_count: u64,
    pub non_finite_sample_count: u64,
}

/// Information about one derived ASR-friendly track.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedAudioTrackManifest {
    pub path: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_frames: u64,
    pub complete: bool,
    pub resampler: String,
}

/// Checkpoint-only detail for one source. The stable public provenance contract
/// remains [`diarization::CaptureTrackManifest`]; this adds chunk and frame
/// timing needed to reconstruct a clock-aware ASR timeline after a crash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureTrackDetailsManifest {
    pub diarization_track: CaptureTrackManifest,
    pub complete: bool,
    /// Path to JSON Lines callback records containing the exact timestamp,
    /// sequence, source-format, offset, and dropped-frame information.
    pub frame_index_path: String,
    pub source_formats: Vec<SourceAudioFormat>,
    pub chunks: Vec<SourceChunkManifest>,
    pub segments: Vec<SourceSegmentManifest>,
    pub timing: TrackTimingManifest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_asr: Option<DerivedAudioTrackManifest>,
}

/// Description of the convenience mix. It is intentionally marked as derived
/// rather than archival source material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixManifest {
    pub path: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_frames: u64,
    pub complete: bool,
    pub method: String,
}

/// Session-level durable metadata. All paths are relative to the session root.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureSessionManifest {
    pub version: u32,
    /// The exact manifest contract consumed by diarization and history code.
    pub diarization_manifest: MeetingCaptureManifest,
    pub session_id: String,
    pub final_directory: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalized_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    pub checkpoint_generation: u64,
    pub source_chunk_duration_seconds: u64,
    pub complete: bool,
    pub recovered_from_crash: bool,
    pub microphone: CaptureTrackDetailsManifest,
    pub system: CaptureTrackDetailsManifest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mix: Option<MixManifest>,
}

/// Rich finalize output for new call sites. `finalize()` itself remains
/// backward-compatible and returns only `AudioTracks`.
#[derive(Clone, Debug)]
pub struct MeetingCaptureArtifacts {
    pub audio_tracks: AudioTracks,
    /// The shared manifest type defined by the diarization subsystem.
    pub manifest: MeetingCaptureManifest,
    /// Chunk/timestamp/checkpoint detail retained alongside `manifest.json`.
    pub capture_details: CaptureSessionManifest,
    pub manifest_path: String,
    pub session_root: String,
}

/// A crash-interrupted session discovered on startup. Call
/// [`MeetingCaptureSession::recover_and_finalize`] to publish its checkpointed
/// content as an explicitly incomplete session.
#[derive(Clone, Debug)]
pub struct RecoverableMeetingCapture {
    pub session_root: PathBuf,
    pub manifest: MeetingCaptureManifest,
    pub capture_details: CaptureSessionManifest,
}

#[derive(Serialize)]
struct FrameIndexEntry {
    timestamp_ns: i64,
    input_end_timestamp_ns: i64,
    sequence: u64,
    sample_rate: u32,
    channels: u16,
    input_frames: u64,
    stored_frames: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    stored_start_timestamp_ns: Option<i64>,
    dropped_frames: u64,
    gap_before_ns: i64,
    overlap_trimmed_frames: u64,
}

struct ActiveChunk {
    track_chunk_index: usize,
    path: PathBuf,
    relative_path: String,
    bucket: i64,
    format: SourceAudioFormat,
    writer: WavFileWriter,
}

struct SourceState {
    track: CaptureTrackDetailsManifest,
    active_chunk: Option<ActiveChunk>,
    frame_index: BufWriter<File>,
    frame_index_path: PathBuf,
    next_chunk_index: u64,
    last_timeline_end_timestamp_ns: Option<i64>,
    last_sequence: Option<u64>,
}

/// Timestamp-aware, chunked session storage. Dropping a session intentionally
/// preserves its checkpoint directory; callers must invoke `discard()` for an
/// explicit cancellation.
pub struct MeetingCaptureSession {
    recordings_dir: PathBuf,
    staging_dir: PathBuf,
    final_dir_name: String,
    session_id: String,
    created_at: String,
    session_started_at_unix_ms: i64,
    microphone: SourceState,
    system: SourceState,
    checkpoint_generation: u64,
    last_checkpoint_timestamp_ns: Option<i64>,
    legacy_timestamp_ns: i64,
    legacy_sequence: [u64; 2],
    legacy_sample_count: usize,
    published: bool,
    discarded: bool,
}

impl MeetingCaptureSession {
    /// Creates a session with unknown device metadata. Adapters should call
    /// `configure_source` before their first frame when names/backends are
    /// available.
    pub fn create(recordings_dir: &Path) -> Result<Self> {
        Self::create_with_metadata(recordings_dir, MeetingCaptureMetadata::default())
    }

    pub fn create_with_metadata(
        recordings_dir: &Path,
        metadata: MeetingCaptureMetadata,
    ) -> Result<Self> {
        fs::create_dir_all(recordings_dir)?;

        let (session_id, final_dir_name, staging_dir) = create_session_directory(recordings_dir)?;
        let staging_dir_for_cleanup = staging_dir.clone();
        let created_at = utc_timestamp();
        let session_started_at_unix_ms = unix_timestamp_ms();
        let create_result = (|| -> Result<Self> {
            fs::create_dir_all(
                staging_dir
                    .join(SOURCE_DIR)
                    .join(source_name(CaptureSource::Microphone)),
            )?;
            fs::create_dir_all(
                staging_dir
                    .join(SOURCE_DIR)
                    .join(source_name(CaptureSource::System)),
            )?;
            fs::create_dir_all(staging_dir.join(TIMELINE_DIR))?;

            let microphone = SourceState::new(
                &staging_dir,
                CaptureSource::Microphone,
                metadata.microphone,
                session_started_at_unix_ms,
            )?;
            let system = SourceState::new(
                &staging_dir,
                CaptureSource::System,
                metadata.system,
                session_started_at_unix_ms,
            )?;

            Ok(Self {
                recordings_dir: recordings_dir.to_path_buf(),
                staging_dir,
                final_dir_name,
                session_id,
                created_at,
                session_started_at_unix_ms,
                microphone,
                system,
                checkpoint_generation: 0,
                last_checkpoint_timestamp_ns: None,
                legacy_timestamp_ns: 0,
                legacy_sequence: [0, 0],
                legacy_sample_count: 0,
                published: false,
                discarded: false,
            })
        })();

        match create_result {
            Ok(mut session) => {
                if let Err(error) = session.checkpoint() {
                    let root = session.staging_dir.clone();
                    drop(session);
                    let _ = fs::remove_dir_all(root);
                    return Err(error);
                }
                Ok(session)
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging_dir_for_cleanup);
                Err(error)
            }
        }
    }

    /// Unix-clock anchor for adapter lifecycle records. Capture callbacks use
    /// the monotonic meeting clock for frames, while this value lets adapters
    /// persist the actual device-start instant in the public manifest without
    /// depending on wall-clock reads at callback time.
    pub(crate) fn session_started_at_unix_ms(&self) -> i64 {
        self.session_started_at_unix_ms
    }

    /// Directory containing all meeting recordings. This remains stable before
    /// and after final publication.
    pub fn recordings_root(&self) -> &Path {
        &self.recordings_dir
    }

    /// Current on-disk session root. Before finalization it is a recoverable
    /// `.capture-meeting-*` directory; after `finalize` the returned artifacts
    /// expose the published root instead.
    pub fn session_root(&self) -> &Path {
        &self.staging_dir
    }

    pub fn final_session_directory(&self) -> &str {
        &self.final_dir_name
    }

    pub fn manifest_relative_path(&self) -> String {
        format!("{}/{}", self.final_dir_name, MANIFEST_FILE)
    }

    /// Updates persisted device/backend metadata for one capture source.
    pub fn configure_source(
        &mut self,
        source: CaptureSource,
        metadata: CaptureSourceMetadata,
    ) -> Result<()> {
        let track = &mut self.source_state_mut(source).track;
        track.diarization_track.device_name = metadata.device_name;
        if let Some(backend) = metadata.backend {
            track.diarization_track.backend_name = backend;
        }
        self.checkpoint()
    }

    /// Allows a platform adapter to persist a meaningful backend outcome even
    /// if it could not submit audio frames.
    pub fn set_source_backend_status(
        &mut self,
        source: CaptureSource,
        status: CaptureBackendStatus,
    ) -> Result<()> {
        let track = &mut self.source_state_mut(source).track;
        track.diarization_track.backend_status = status;
        if matches!(
            status,
            CaptureBackendStatus::Unavailable | CaptureBackendStatus::Failed
        ) {
            track.complete = false;
            track.diarization_track.status = match status {
                CaptureBackendStatus::Unavailable => CaptureTrackStatus::Unavailable,
                CaptureBackendStatus::Failed => CaptureTrackStatus::Failed,
                _ => CaptureTrackStatus::Partial,
            };
        }
        self.checkpoint()
    }

    /// Records when a native source stream successfully began rather than
    /// treating session-directory creation as device-start time. This matters
    /// when a platform permission prompt or a delayed system-audio backend
    /// starts one source after the other.
    pub fn mark_source_started_at_unix_ms(
        &mut self,
        source: CaptureSource,
        started_at_unix_ms: i64,
    ) -> Result<()> {
        if source == CaptureSource::Mix {
            bail!("mix is derived and has no native stream start time");
        }
        if started_at_unix_ms <= 0 {
            bail!("native source start time must be a positive Unix timestamp");
        }
        self.source_state_mut(source)
            .track
            .diarization_track
            .started_at_unix_ms = started_at_unix_ms;
        self.checkpoint()
    }

    /// Persists a platform adapter's measured device-clock to meeting-clock
    /// offset without requiring timestamp consumers to abandon the shared
    /// `TimestampedAudioFrame.timestamp_ns` contract.
    pub fn set_source_clock_offset_ns(
        &mut self,
        source: CaptureSource,
        clock_offset_ns: i64,
    ) -> Result<()> {
        if source == CaptureSource::Mix {
            bail!("mix is derived and has no capture clock");
        }
        let track = &mut self.source_state_mut(source).track;
        track.timing.calibrated_clock_offset_ns = Some(clock_offset_ns);
        track.diarization_track.clock_offset_ms = clock_offset_ns / 1_000_000;
        self.checkpoint()
    }

    /// Appends one independently timestamped source frame. This is the primary
    /// API for native capture adapters.
    pub fn append_frame(&mut self, frame: TimestampedAudioFrame) -> Result<()> {
        validate_frame(&frame)?;
        let frame_count = frame.frame_count()?;
        let input_duration_ns = frames_to_ns(frame_count, frame.sample_rate)?;
        let input_end_timestamp_ns = frame
            .timestamp_ns
            .checked_add(input_duration_ns)
            .context("meeting frame timestamp overflow")?;
        let checkpoint_due = self
            .last_checkpoint_timestamp_ns
            .map(|last| frame.timestamp_ns.saturating_sub(last) >= CHECKPOINT_INTERVAL_NS)
            .unwrap_or(true);
        let staging_dir = self.staging_dir.clone();

        {
            let state = self.source_state_mut(frame.source);
            append_timestamped_frame(
                state,
                &staging_dir,
                &frame,
                frame_count,
                input_end_timestamp_ns,
            )?;
        }

        if checkpoint_due {
            self.last_checkpoint_timestamp_ns = Some(frame.timestamp_ns);
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Writes a durable checkpoint. Active WAV headers are flushed and synced
    /// before metadata is committed to one of two alternating checkpoint files.
    pub fn checkpoint(&mut self) -> Result<()> {
        sync_source_state(&mut self.microphone)?;
        sync_source_state(&mut self.system)?;

        self.checkpoint_generation = self.checkpoint_generation.saturating_add(1);
        let manifest = self.build_manifest();
        let checkpoint = if self.checkpoint_generation % 2 == 0 {
            CHECKPOINT_A
        } else {
            CHECKPOINT_B
        };
        write_capture_details_durable(&self.staging_dir.join(checkpoint), &manifest)
    }

    /// Legacy adapter retained for existing call sites while platform adapters
    /// migrate to `append_frame`. It is intentionally isolated from the native
    /// timestamped path and pads only this old compatibility call.
    #[deprecated(note = "capture adapters should submit TimestampedAudioFrame values")]
    pub fn append_aligned(&mut self, microphone: &[f32], system: &[f32]) -> Result<()> {
        let frame_len = microphone.len().max(system.len());
        if frame_len == 0 {
            return Ok(());
        }
        let mut mic = microphone.to_vec();
        mic.resize(frame_len, 0.0);
        let mut desktop = system.to_vec();
        desktop.resize(frame_len, 0.0);

        let timestamp_ns = self.legacy_timestamp_ns;
        let microphone_frame = TimestampedAudioFrame {
            source: CaptureSource::Microphone,
            timestamp_ns,
            sequence: self.legacy_sequence[0],
            sample_rate: ASR_SAMPLE_RATE,
            channels: 1,
            samples: mic,
            dropped_frames: 0,
        };
        self.legacy_sequence[0] = self.legacy_sequence[0].saturating_add(1);
        let system_frame = TimestampedAudioFrame {
            source: CaptureSource::System,
            timestamp_ns,
            sequence: self.legacy_sequence[1],
            sample_rate: ASR_SAMPLE_RATE,
            channels: 1,
            samples: desktop,
            dropped_frames: 0,
        };
        self.legacy_sequence[1] = self.legacy_sequence[1].saturating_add(1);

        self.append_frame(microphone_frame)?;
        self.append_frame(system_frame)?;
        self.legacy_timestamp_ns = self
            .legacy_timestamp_ns
            .checked_add(frames_to_ns(frame_len as u64, ASR_SAMPLE_RATE)?)
            .context("legacy meeting timestamp overflow")?;
        self.legacy_sample_count = self.legacy_sample_count.saturating_add(frame_len);
        Ok(())
    }

    /// Kept for consumers of the former aligned API. New code should use the
    /// timing fields in the manifest rather than this compatibility counter.
    pub fn sample_count(&self) -> usize {
        self.legacy_sample_count
    }

    /// Finalizes native source chunks, derives ASR WAVs, writes the canonical
    /// manifest, and atomically publishes the session directory.
    pub fn finalize(self) -> Result<AudioTracks> {
        self.finalize_with_artifacts()
            .map(|artifacts| artifacts.audio_tracks)
    }

    pub fn finalize_with_artifacts(mut self) -> Result<MeetingCaptureArtifacts> {
        finish_source_state(&mut self.microphone)?;
        finish_source_state(&mut self.system)?;
        self.checkpoint()?;

        let mut capture_details = self.build_manifest();
        capture_details.finalized_at = Some(utc_timestamp());
        capture_details.complete = true;
        derive_outputs(&self.staging_dir, &mut capture_details)?;
        capture_details.published_at = Some(utc_timestamp());
        write_capture_details_durable(&self.staging_dir.join(MANIFEST_FILE), &capture_details)?;
        sync_outputs(&self.staging_dir, &capture_details)?;

        let final_dir = self.recordings_dir.join(&self.final_dir_name);
        if final_dir.exists() {
            bail!("meeting capture output directory already exists: {final_dir:?}");
        }
        fs::rename(&self.staging_dir, &final_dir).with_context(|| {
            format!(
                "atomically publish meeting capture {:?} to {:?}",
                self.staging_dir, final_dir
            )
        })?;
        self.published = true;

        Ok(MeetingCaptureArtifacts {
            audio_tracks: audio_tracks_for_directory(&self.final_dir_name),
            manifest_path: format!("{}/{}", self.final_dir_name, MANIFEST_FILE),
            session_root: self.final_dir_name.clone(),
            manifest: capture_details.diarization_manifest.clone(),
            capture_details,
        })
    }

    /// Explicitly abandons a session and removes its source/checkpoint files.
    /// Dropping a session does *not* discard it, so crash recovery remains
    /// possible.
    pub fn discard(mut self) {
        let _ = finish_source_state(&mut self.microphone);
        let _ = finish_source_state(&mut self.system);
        let _ = fs::remove_dir_all(&self.staging_dir);
        self.discarded = true;
    }

    /// Finds crash-interrupted, checkpointed sessions. This function is
    /// intentionally non-mutating; a caller can present a recovery choice or
    /// call `recover_and_finalize` during startup maintenance.
    pub fn find_recoverable_sessions(
        recordings_dir: &Path,
    ) -> Result<Vec<RecoverableMeetingCapture>> {
        let mut sessions = Vec::new();
        if !recordings_dir.exists() {
            return Ok(sessions);
        }
        for entry in fs::read_dir(recordings_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(".capture-meeting-") {
                continue;
            }
            let capture_details = match read_latest_checkpoint(&entry.path()).with_context(|| {
                format!(
                    "read interrupted meeting capture checkpoint at {:?}",
                    entry.path()
                )
            }) {
                Ok(capture_details) => capture_details,
                Err(error) => {
                    // One malformed or half-created staging directory must
                    // never prevent recovery of other valid meetings. Leave
                    // it in place for diagnostics; a later cleanup policy can
                    // decide whether it is safe to remove.
                    warn!(
                        "Skipping unreadable interrupted meeting capture {:?}: {error}",
                        entry.path()
                    );
                    continue;
                }
            };
            // A process can die after the durable final manifest has been
            // written but before the atomic staging-directory rename. Such a
            // session has `complete: true`, yet it is still under
            // `.capture-meeting-*` and is not reachable through history. The
            // directory name is the authoritative publication boundary, so
            // recover every valid staged session regardless of that flag.
            sessions.push(RecoverableMeetingCapture {
                session_root: entry.path(),
                manifest: capture_details.diarization_manifest.clone(),
                capture_details,
            });
        }
        sessions.sort_by(|left, right| left.session_root.cmp(&right.session_root));
        Ok(sessions)
    }

    /// Derives and publishes the last durable checkpoint of a crash-interrupted
    /// session. The resulting manifest remains `complete: false`, and each
    /// source reports incomplete capture, so downstream history can surface the
    /// recovery state instead of treating partial audio as a clean meeting.
    pub fn recover_and_finalize(
        recordings_dir: &Path,
        session_root: &Path,
    ) -> Result<MeetingCaptureArtifacts> {
        let mut capture_details = read_latest_checkpoint(session_root)?;
        let final_dir = recordings_dir.join(&capture_details.final_directory);
        if final_dir.exists() {
            bail!("recovered meeting output directory already exists: {final_dir:?}");
        }

        // `complete` describes a clean capture finalization, not whether its
        // staging directory was published. A crash after final manifest write
        // but before rename is recoverable, but is conservatively marked
        // partial because the process did not complete its publication path.
        mark_recovered_track(&mut capture_details.microphone);
        mark_recovered_track(&mut capture_details.system);
        capture_details.complete = false;
        capture_details.recovered_from_crash = true;
        capture_details.finalized_at = Some(utc_timestamp());
        derive_outputs(session_root, &mut capture_details)?;
        capture_details.published_at = Some(utc_timestamp());
        write_capture_details_durable(&session_root.join(MANIFEST_FILE), &capture_details)?;
        sync_outputs(session_root, &capture_details)?;
        fs::rename(session_root, &final_dir).with_context(|| {
            format!(
                "publish recovered meeting capture {:?} to {:?}",
                session_root, final_dir
            )
        })?;

        Ok(MeetingCaptureArtifacts {
            audio_tracks: audio_tracks_for_directory(&capture_details.final_directory),
            manifest_path: format!("{}/{}", capture_details.final_directory, MANIFEST_FILE),
            session_root: capture_details.final_directory.clone(),
            manifest: capture_details.diarization_manifest.clone(),
            capture_details,
        })
    }

    fn source_state_mut(&mut self, source: CaptureSource) -> &mut SourceState {
        match source {
            CaptureSource::Microphone => &mut self.microphone,
            CaptureSource::System => &mut self.system,
            CaptureSource::Mix => panic!("mix is derived and cannot accept capture frames"),
        }
    }

    fn build_manifest(&self) -> CaptureSessionManifest {
        CaptureSessionManifest {
            version: CAPTURE_SESSION_MANIFEST_VERSION,
            diarization_manifest: MeetingCaptureManifest {
                version: CAPTURE_MANIFEST_VERSION,
                session_started_at_unix_ms: self.session_started_at_unix_ms,
                microphone: self.microphone.track.diarization_track.clone(),
                system: self.system.track.diarization_track.clone(),
                mix: None,
            },
            session_id: self.session_id.clone(),
            final_directory: self.final_dir_name.clone(),
            created_at: self.created_at.clone(),
            finalized_at: None,
            published_at: None,
            checkpoint_generation: self.checkpoint_generation,
            source_chunk_duration_seconds: SOURCE_CHUNK_DURATION_SECONDS,
            complete: false,
            recovered_from_crash: false,
            microphone: self.microphone.track.clone(),
            system: self.system.track.clone(),
            mix: None,
        }
    }
}

impl Drop for MeetingCaptureSession {
    fn drop(&mut self) {
        // A deliberate no-op: hound finalizes the currently open headers on
        // drop, while our last alternating checkpoint keeps only the portion
        // that was explicitly synced. `discard()` is the destructive path.
        let _ = (self.published, self.discarded);
    }
}

impl SourceState {
    fn new(
        session_root: &Path,
        source: CaptureSource,
        metadata: CaptureSourceMetadata,
        session_started_at_unix_ms: i64,
    ) -> Result<Self> {
        let frame_index_relative = format!("{}/{}.ndjson", TIMELINE_DIR, source_name(source));
        let frame_index_path = session_root.join(&frame_index_relative);
        let frame_index = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&frame_index_path)
                .with_context(|| format!("create meeting frame index {frame_index_path:?}"))?,
        );
        Ok(Self {
            track: CaptureTrackDetailsManifest {
                diarization_track: CaptureTrackManifest {
                    source,
                    source_path: Some(format!("{}/{}", SOURCE_DIR, source_name(source))),
                    asr_path: None,
                    sample_rate_hz: 0,
                    channels: 0,
                    asr_sample_rate_hz: None,
                    asr_channels: None,
                    device_name: metadata.device_name,
                    backend_name: metadata.backend.unwrap_or_else(|| "unknown".to_owned()),
                    backend_status: CaptureBackendStatus::Ready,
                    started_at_unix_ms: session_started_at_unix_ms,
                    clock_offset_ms: 0,
                    dropped_frames: 0,
                    status: CaptureTrackStatus::Partial,
                },
                complete: false,
                frame_index_path: frame_index_relative,
                source_formats: Vec::new(),
                chunks: Vec::new(),
                segments: Vec::new(),
                timing: TrackTimingManifest::default(),
                derived_asr: None,
            },
            active_chunk: None,
            frame_index,
            frame_index_path,
            next_chunk_index: 0,
            last_timeline_end_timestamp_ns: None,
            last_sequence: None,
        })
    }
}

fn create_session_directory(recordings_dir: &Path) -> Result<(String, String, PathBuf)> {
    for _ in 0..128 {
        let nonce = format!(
            "{}-{}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id(),
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let final_dir_name = format!("meeting-{nonce}");
        let staging_dir = recordings_dir.join(format!(".capture-{final_dir_name}"));
        match fs::create_dir(&staging_dir) {
            Ok(()) => return Ok((nonce, final_dir_name, staging_dir)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create meeting capture directory {staging_dir:?}"));
            }
        }
    }
    bail!("could not allocate a unique meeting capture directory")
}

fn validate_frame(frame: &TimestampedAudioFrame) -> Result<()> {
    if frame.source == CaptureSource::Mix {
        bail!("mix is derived and cannot be submitted as a capture source");
    }
    if frame.timestamp_ns < 0 {
        bail!("meeting frame timestamp must be relative and non-negative");
    }
    if frame.sample_rate == 0 || frame.sample_rate > 384_000 {
        bail!(
            "unsupported meeting source sample rate: {}",
            frame.sample_rate
        );
    }
    if frame.channels == 0 || frame.channels > 32 {
        bail!(
            "unsupported meeting source channel count: {}",
            frame.channels
        );
    }
    let _ = frame.frame_count()?;
    Ok(())
}

fn append_timestamped_frame(
    state: &mut SourceState,
    session_root: &Path,
    frame: &TimestampedAudioFrame,
    frame_count: u64,
    input_end_timestamp_ns: i64,
) -> Result<()> {
    if let Some(last_sequence) = state.last_sequence {
        if frame.sequence <= last_sequence {
            state.track.timing.out_of_order_sequence_count = state
                .track
                .timing
                .out_of_order_sequence_count
                .saturating_add(1);
        }
    }
    state.last_sequence = Some(
        state
            .last_sequence
            .map_or(frame.sequence, |last| last.max(frame.sequence)),
    );
    state.track.timing.first_timestamp_ns = Some(
        state
            .track
            .timing
            .first_timestamp_ns
            .map_or(frame.timestamp_ns, |first| first.min(frame.timestamp_ns)),
    );
    state.track.timing.last_timestamp_ns = Some(
        state
            .track
            .timing
            .last_timestamp_ns
            .map_or(input_end_timestamp_ns, |last| {
                last.max(input_end_timestamp_ns)
            }),
    );
    if state.track.timing.initial_offset_ns.is_none() {
        state.track.timing.initial_offset_ns = Some(frame.timestamp_ns);
        // The first native callback establishes the measured device-stream
        // start relative to the shared meeting origin. A platform adapter may
        // later refine this with `set_source_clock_offset_ns`, but never leave
        // the public provenance manifest at a misleading implicit zero.
        state.track.diarization_track.clock_offset_ms = frame.timestamp_ns / 1_000_000;
    }
    state.track.timing.input_frames = state.track.timing.input_frames.saturating_add(frame_count);
    state.track.timing.source_audio_duration_ns = state
        .track
        .timing
        .source_audio_duration_ns
        .saturating_add(input_end_timestamp_ns.saturating_sub(frame.timestamp_ns));
    state.track.timing.dropped_frames = state
        .track
        .timing
        .dropped_frames
        .saturating_add(frame.dropped_frames);
    if state.track.diarization_track.backend_status == CaptureBackendStatus::Ready {
        state.track.diarization_track.backend_status = CaptureBackendStatus::Capturing;
    }
    state.track.diarization_track.dropped_frames = state
        .track
        .diarization_track
        .dropped_frames
        .saturating_add(frame.dropped_frames);
    if state.track.diarization_track.sample_rate_hz == 0 {
        state.track.diarization_track.sample_rate_hz = frame.sample_rate;
        state.track.diarization_track.channels = frame.channels;
    }

    let format = SourceAudioFormat::from_frame(frame);
    if !state.track.source_formats.contains(&format) {
        state.track.source_formats.push(format.clone());
    }

    let mut gap_before_ns = 0;
    let mut overlap_trimmed_frames = 0;
    let mut stored_start_timestamp_ns = Some(frame.timestamp_ns);
    let mut source_frame_offset = 0_u64;
    let mut stored_frame_count = frame_count;

    if let Some(previous_end) = state.last_timeline_end_timestamp_ns {
        let delta = frame.timestamp_ns.saturating_sub(previous_end);
        if delta > TIMESTAMP_JITTER_TOLERANCE_NS {
            gap_before_ns = delta;
            state.track.timing.gap_count = state.track.timing.gap_count.saturating_add(1);
            state.track.timing.gap_duration_ns =
                state.track.timing.gap_duration_ns.saturating_add(delta);
        } else if delta < -TIMESTAMP_JITTER_TOLERANCE_NS {
            let overlap_ns = previous_end.saturating_sub(frame.timestamp_ns);
            state.track.timing.overlap_count = state.track.timing.overlap_count.saturating_add(1);
            state.track.timing.overlap_duration_ns = state
                .track
                .timing
                .overlap_duration_ns
                .saturating_add(overlap_ns);
            overlap_trimmed_frames = ns_to_source_frames_ceil(overlap_ns, frame.sample_rate);
            if overlap_trimmed_frames >= frame_count {
                stored_frame_count = 0;
                stored_start_timestamp_ns = None;
            } else {
                source_frame_offset = overlap_trimmed_frames;
                stored_frame_count = frame_count - overlap_trimmed_frames;
                stored_start_timestamp_ns = Some(previous_end);
            }
        } else {
            // Callback scheduling jitter is not an audio discontinuity. Keep
            // a contiguous timeline and retain the original timestamp in JSONL.
            stored_start_timestamp_ns = Some(previous_end);
        }
    }

    if stored_frame_count > 0 {
        let start = stored_start_timestamp_ns.context("stored frame start timestamp missing")?;
        let start_sample = source_frame_offset
            .checked_mul(u64::from(frame.channels))
            .context("meeting source sample offset overflow")? as usize;
        let end_sample = start_sample
            .checked_add(
                stored_frame_count
                    .checked_mul(u64::from(frame.channels))
                    .context("meeting source sample length overflow")? as usize,
            )
            .context("meeting source sample slice overflow")?;
        write_source_frames(
            state,
            session_root,
            &frame.samples[start_sample..end_sample],
            start,
            stored_frame_count,
            &format,
            frame.sequence,
        )?;
        let stored_end = start
            .checked_add(frames_to_ns(stored_frame_count, frame.sample_rate)?)
            .context("meeting stored frame timestamp overflow")?;
        state.last_timeline_end_timestamp_ns = Some(stored_end);
        state.track.timing.stored_frames = state
            .track
            .timing
            .stored_frames
            .saturating_add(stored_frame_count);
    }

    recompute_clock_drift(&mut state.track.timing);
    let entry = FrameIndexEntry {
        timestamp_ns: frame.timestamp_ns,
        input_end_timestamp_ns,
        sequence: frame.sequence,
        sample_rate: frame.sample_rate,
        channels: frame.channels,
        input_frames: frame_count,
        stored_frames: stored_frame_count,
        stored_start_timestamp_ns,
        dropped_frames: frame.dropped_frames,
        gap_before_ns,
        overlap_trimmed_frames,
    };
    serde_json::to_writer(&mut state.frame_index, &entry)?;
    state.frame_index.write_all(b"\n")?;
    Ok(())
}

fn write_source_frames(
    state: &mut SourceState,
    session_root: &Path,
    samples: &[f32],
    start_timestamp_ns: i64,
    frame_count: u64,
    format: &SourceAudioFormat,
    sequence: u64,
) -> Result<()> {
    let mut frames_remaining = frame_count;
    let mut frame_offset = 0_u64;
    let mut timestamp = start_timestamp_ns;
    while frames_remaining > 0 {
        let bucket = timestamp.div_euclid(SOURCE_CHUNK_DURATION_NS);
        let bucket_end = (bucket + 1)
            .checked_mul(SOURCE_CHUNK_DURATION_NS)
            .context("meeting chunk timestamp overflow")?;
        let frames_to_boundary = source_frames_until(timestamp, bucket_end, format.sample_rate);
        let part_frames = frames_remaining.min(frames_to_boundary.max(1));
        ensure_active_chunk(state, session_root, bucket, timestamp, format)?;

        let sample_start = frame_offset
            .checked_mul(u64::from(format.channels))
            .context("meeting source part offset overflow")? as usize;
        let sample_end = sample_start
            .checked_add(
                part_frames
                    .checked_mul(u64::from(format.channels))
                    .context("meeting source part size overflow")? as usize,
            )
            .context("meeting source part slice overflow")?;
        let source_frame_offset;
        let chunk_path;
        let mut non_finite_sample_count = 0_u64;
        {
            let active = state
                .active_chunk
                .as_mut()
                .context("meeting source chunk was not opened")?;
            let chunk = state
                .track
                .chunks
                .get_mut(active.track_chunk_index)
                .context("meeting source chunk manifest missing")?;
            source_frame_offset = chunk.sample_frames;
            for sample in &samples[sample_start..sample_end] {
                if sample.is_finite() {
                    active.writer.write_sample(to_i32(*sample))?;
                } else {
                    active.writer.write_sample(0_i32)?;
                    non_finite_sample_count = non_finite_sample_count.saturating_add(1);
                }
            }
            chunk.sample_frames = chunk.sample_frames.saturating_add(part_frames);
            chunk.ended_at_timestamp_ns = timestamp
                .checked_add(frames_to_ns(part_frames, format.sample_rate)?)
                .context("meeting chunk end timestamp overflow")?;
            chunk_path = active.relative_path.clone();
        }
        state.track.timing.non_finite_sample_count = state
            .track
            .timing
            .non_finite_sample_count
            .saturating_add(non_finite_sample_count);

        let part_end = timestamp
            .checked_add(frames_to_ns(part_frames, format.sample_rate)?)
            .context("meeting source segment end timestamp overflow")?;
        append_segment(
            &mut state.track.segments,
            SourceSegmentManifest {
                chunk_path,
                source_frame_offset,
                source_frame_count: part_frames,
                timeline_start_timestamp_ns: timestamp,
                timeline_end_timestamp_ns: part_end,
                sequence_start: sequence,
                sequence_end: sequence,
            },
        );
        timestamp = part_end;
        frame_offset = frame_offset.saturating_add(part_frames);
        frames_remaining -= part_frames;
    }
    Ok(())
}

fn ensure_active_chunk(
    state: &mut SourceState,
    session_root: &Path,
    bucket: i64,
    timestamp_ns: i64,
    format: &SourceAudioFormat,
) -> Result<()> {
    let needs_new = state.active_chunk.as_ref().map_or(true, |active| {
        active.bucket != bucket || active.format != *format
    });
    if !needs_new {
        return Ok(());
    }
    finish_active_chunk(state)?;

    let index = state.next_chunk_index;
    state.next_chunk_index = state.next_chunk_index.saturating_add(1);
    let relative_path = format!(
        "{}/{}/chunk-{index:06}.wav",
        SOURCE_DIR,
        source_name(state.track.diarization_track.source)
    );
    let path = session_root.join(&relative_path);
    let spec = WavSpec {
        channels: format.channels,
        sample_rate: format.sample_rate,
        bits_per_sample: 32,
        sample_format: SampleFormat::Int,
    };
    let writer = WavWriter::create(&path, spec)
        .with_context(|| format!("create meeting source chunk {path:?}"))?;
    let track_chunk_index = state.track.chunks.len();
    state.track.chunks.push(SourceChunkManifest {
        path: relative_path.clone(),
        index,
        format: format.clone(),
        started_at_timestamp_ns: timestamp_ns,
        ended_at_timestamp_ns: timestamp_ns,
        sample_frames: 0,
        complete: false,
    });
    state.active_chunk = Some(ActiveChunk {
        track_chunk_index,
        path,
        relative_path,
        bucket,
        format: format.clone(),
        writer,
    });
    Ok(())
}

fn append_segment(segments: &mut Vec<SourceSegmentManifest>, next: SourceSegmentManifest) {
    if let Some(last) = segments.last_mut() {
        if last.chunk_path == next.chunk_path
            && last
                .source_frame_offset
                .saturating_add(last.source_frame_count)
                == next.source_frame_offset
            && last.timeline_end_timestamp_ns == next.timeline_start_timestamp_ns
            && last.sequence_end <= next.sequence_start
        {
            last.source_frame_count = last
                .source_frame_count
                .saturating_add(next.source_frame_count);
            last.timeline_end_timestamp_ns = next.timeline_end_timestamp_ns;
            last.sequence_end = next.sequence_end;
            return;
        }
    }
    segments.push(next);
}

fn finish_active_chunk(state: &mut SourceState) -> Result<()> {
    let Some(active) = state.active_chunk.take() else {
        return Ok(());
    };
    let chunk_index = active.track_chunk_index;
    let path = active.path.clone();
    active.writer.finalize()?;
    File::open(&path)?.sync_all()?;
    if let Some(chunk) = state.track.chunks.get_mut(chunk_index) {
        chunk.complete = true;
    }
    Ok(())
}

fn sync_source_state(state: &mut SourceState) -> Result<()> {
    if let Some(active) = state.active_chunk.as_mut() {
        active.writer.flush()?;
        File::open(&active.path)?.sync_all()?;
    }
    state.frame_index.flush()?;
    File::open(&state.frame_index_path)?.sync_all()?;
    Ok(())
}

fn finish_source_state(state: &mut SourceState) -> Result<()> {
    finish_active_chunk(state)?;
    state.frame_index.flush()?;
    File::open(&state.frame_index_path)?.sync_all()?;
    let has_source_audio = !state.track.chunks.is_empty() && state.track.timing.stored_frames > 0;
    match state.track.diarization_track.backend_status {
        CaptureBackendStatus::Ready | CaptureBackendStatus::Capturing => {
            state.track.diarization_track.backend_status = CaptureBackendStatus::Stopped;
            state.track.complete = has_source_audio;
        }
        CaptureBackendStatus::Stopped => state.track.complete = has_source_audio,
        CaptureBackendStatus::Unavailable | CaptureBackendStatus::Failed => {
            state.track.complete = false;
        }
    }
    if !has_source_audio {
        // A derived empty 16 kHz WAV is still useful as a stable ASR-path
        // placeholder, but it must not be presented as the native source
        // recording or overwrite the unknown source format in the manifest.
        state.track.diarization_track.source_path = None;
    }
    state.track.diarization_track.status = if state.track.complete {
        CaptureTrackStatus::Complete
    } else {
        match state.track.diarization_track.backend_status {
            CaptureBackendStatus::Unavailable => CaptureTrackStatus::Unavailable,
            CaptureBackendStatus::Failed => CaptureTrackStatus::Failed,
            _ => CaptureTrackStatus::Partial,
        }
    };
    Ok(())
}

fn source_frames_until(timestamp_ns: i64, boundary_ns: i64, sample_rate: u32) -> u64 {
    let available_ns = boundary_ns.saturating_sub(timestamp_ns).max(1) as i128;
    let frames = available_ns * i128::from(sample_rate) / 1_000_000_000_i128;
    frames.max(1) as u64
}

fn frames_to_ns(frames: u64, sample_rate: u32) -> Result<i64> {
    let nanoseconds = u128::from(frames)
        .checked_mul(1_000_000_000)
        .context("meeting frame duration overflow")?
        / u128::from(sample_rate);
    i64::try_from(nanoseconds).context("meeting frame duration exceeds i64")
}

fn ns_to_source_frames_ceil(duration_ns: i64, sample_rate: u32) -> u64 {
    if duration_ns <= 0 {
        return 0;
    }
    let numerator = u128::try_from(duration_ns).unwrap_or_default() * u128::from(sample_rate);
    ((numerator + 999_999_999) / 1_000_000_000) as u64
}

fn recompute_clock_drift(timing: &mut TrackTimingManifest) {
    if timing.gap_count > 0 || timing.overlap_count > 0 || timing.input_frames == 0 {
        timing.estimated_clock_drift_ppm = None;
        return;
    }
    let (Some(first), Some(last)) = (timing.first_timestamp_ns, timing.last_timestamp_ns) else {
        return;
    };
    let observed_ns = last.saturating_sub(first);
    if observed_ns <= 0 {
        return;
    }
    let nominal_ns = timing.source_audio_duration_ns;
    if nominal_ns <= 0 {
        return;
    }
    timing.estimated_clock_drift_ppm =
        Some(((observed_ns.saturating_sub(nominal_ns)) as f64 / nominal_ns as f64) * 1_000_000.0);
}

fn derive_outputs(session_root: &Path, manifest: &mut CaptureSessionManifest) -> Result<()> {
    derive_asr_track(session_root, &mut manifest.microphone, "microphone.wav")?;
    derive_asr_track(session_root, &mut manifest.system, "system.wav")?;
    // Both derived tracks were rendered into the shared timestamp origin. The
    // mix duration therefore comes from that clock, not `max(track.len())`;
    // startup offsets, callback gaps, and long-running device-clock drift are
    // already represented as silence at their measured positions.
    let mix_frames = meeting_timeline_asr_frames(manifest)?;
    let mix_frames = derive_mix(session_root, mix_frames)?;
    manifest.mix = Some(MixManifest {
        path: "mix.wav".to_owned(),
        sample_rate: ASR_SAMPLE_RATE,
        channels: 1,
        sample_frames: mix_frames,
        complete: manifest.microphone.complete && manifest.system.complete,
        method: "timestamp-aligned 50/50 average; derived convenience output".to_owned(),
    });
    manifest.diarization_manifest.microphone = manifest.microphone.diarization_track.clone();
    manifest.diarization_manifest.system = manifest.system.diarization_track.clone();
    manifest.diarization_manifest.mix = Some(CaptureTrackManifest {
        source: CaptureSource::Mix,
        source_path: None,
        asr_path: Some("mix.wav".to_owned()),
        sample_rate_hz: ASR_SAMPLE_RATE,
        channels: 1,
        asr_sample_rate_hz: Some(ASR_SAMPLE_RATE),
        asr_channels: Some(1),
        device_name: None,
        backend_name: "derived timestamp-aligned mix".to_owned(),
        backend_status: CaptureBackendStatus::Stopped,
        started_at_unix_ms: manifest.diarization_manifest.session_started_at_unix_ms,
        clock_offset_ms: 0,
        dropped_frames: manifest
            .microphone
            .diarization_track
            .dropped_frames
            .saturating_add(manifest.system.diarization_track.dropped_frames),
        status: if manifest.microphone.complete && manifest.system.complete {
            CaptureTrackStatus::Complete
        } else {
            CaptureTrackStatus::Partial
        },
    });
    Ok(())
}

fn meeting_timeline_asr_frames(manifest: &CaptureSessionManifest) -> Result<u64> {
    let latest_timestamp_ns = [
        manifest.microphone.timing.last_timestamp_ns,
        manifest.system.timing.last_timestamp_ns,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0);
    timestamp_to_asr_frame(latest_timestamp_ns)
}

fn derive_asr_track(
    session_root: &Path,
    track: &mut CaptureTrackDetailsManifest,
    output_name: &str,
) -> Result<u64> {
    let output_path = session_root.join(output_name);
    let spec = WavSpec {
        channels: 1,
        sample_rate: ASR_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let writer = WavWriter::create(&output_path, spec)
        .with_context(|| format!("create derived ASR track {output_path:?}"))?;
    let mut timeline = AsrTimelineWriter { writer, cursor: 0 };
    let mut segments = track.segments.clone();
    segments.sort_by_key(|segment| {
        (
            segment.timeline_start_timestamp_ns,
            segment.sequence_start,
            segment.chunk_path.clone(),
        )
    });

    for segment in &segments {
        let chunk = track
            .chunks
            .iter()
            .find(|chunk| chunk.path == segment.chunk_path)
            .with_context(|| {
                format!(
                    "derived track references missing chunk {}",
                    segment.chunk_path
                )
            })?;
        let raw = read_source_segment(
            &session_root.join(&chunk.path),
            segment.source_frame_offset,
            segment.source_frame_count,
            chunk.format.channels,
        )?;
        let mono = downmix_to_mono(&raw, chunk.format.channels);
        let expected_samples = scaled_frame_count(
            segment.source_frame_count,
            chunk.format.sample_rate,
            ASR_SAMPLE_RATE,
        );
        let resampled = resample_to_asr(mono, chunk.format.sample_rate, expected_samples)?;
        timeline.write_at(segment.timeline_start_timestamp_ns, &resampled)?;
    }
    let sample_frames = timeline.finalize()?;
    File::open(&output_path)?.sync_all()?;
    track.derived_asr = Some(DerivedAudioTrackManifest {
        path: output_name.to_owned(),
        sample_rate: ASR_SAMPLE_RATE,
        channels: 1,
        sample_frames,
        complete: track.complete,
        resampler: "rubato fft fixed-in; group-delay compensated".to_owned(),
    });
    track.diarization_track.asr_path = Some(output_name.to_owned());
    track.diarization_track.asr_sample_rate_hz = Some(ASR_SAMPLE_RATE);
    track.diarization_track.asr_channels = Some(1);
    track.diarization_track.status = if track.complete {
        CaptureTrackStatus::Complete
    } else {
        match track.diarization_track.backend_status {
            CaptureBackendStatus::Unavailable => CaptureTrackStatus::Unavailable,
            CaptureBackendStatus::Failed => CaptureTrackStatus::Failed,
            _ => CaptureTrackStatus::Partial,
        }
    };
    Ok(sample_frames)
}

struct AsrTimelineWriter {
    writer: WavFileWriter,
    cursor: u64,
}

impl AsrTimelineWriter {
    fn write_at(&mut self, timestamp_ns: i64, samples: &[f32]) -> Result<()> {
        let target = timestamp_to_asr_frame(timestamp_ns)?;
        if target > self.cursor {
            write_silence(&mut self.writer, target - self.cursor)?;
            self.cursor = target;
        }
        let skip = self.cursor.saturating_sub(target) as usize;
        for sample in samples.iter().skip(skip) {
            self.writer.write_sample(to_i16(*sample))?;
            self.cursor = self.cursor.saturating_add(1);
        }
        Ok(())
    }

    fn finalize(self) -> Result<u64> {
        let cursor = self.cursor;
        self.writer.finalize()?;
        Ok(cursor)
    }
}

fn timestamp_to_asr_frame(timestamp_ns: i64) -> Result<u64> {
    let timestamp_ns = u128::try_from(timestamp_ns).context("negative ASR timestamp")?;
    let frames = (timestamp_ns
        .checked_mul(u128::from(ASR_SAMPLE_RATE))
        .context("ASR timestamp frame overflow")?
        + 500_000_000)
        / 1_000_000_000;
    u64::try_from(frames).context("ASR timestamp frame exceeds u64")
}

fn write_silence(writer: &mut WavFileWriter, count: u64) -> Result<()> {
    for _ in 0..count {
        writer.write_sample(0_i16)?;
    }
    Ok(())
}

fn read_source_segment(
    path: &Path,
    source_frame_offset: u64,
    source_frame_count: u64,
    channels: u16,
) -> Result<Vec<f32>> {
    let mut reader =
        WavReader::open(path).with_context(|| format!("open source chunk {path:?}"))?;
    let spec = reader.spec();
    if spec.sample_format != SampleFormat::Int || spec.bits_per_sample != 32 {
        bail!("source chunk {path:?} is not a 32-bit PCM WAV");
    }
    if spec.channels != channels {
        bail!(
            "source chunk {path:?} has {} channels, manifest expects {channels}",
            spec.channels
        );
    }
    let sample_offset = source_frame_offset
        .checked_mul(u64::from(channels))
        .context("source segment sample offset overflow")? as usize;
    let sample_count = source_frame_count
        .checked_mul(u64::from(channels))
        .context("source segment sample length overflow")? as usize;
    let mut samples = Vec::with_capacity(sample_count);
    for sample in reader
        .samples::<i32>()
        .skip(sample_offset)
        .take(sample_count)
    {
        samples.push(sample? as f32 / i32::MAX as f32);
    }
    if samples.len() != sample_count {
        bail!(
            "source chunk {path:?} ended after {} samples; expected {sample_count}",
            samples.len()
        );
    }
    Ok(samples)
}

fn downmix_to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    let channels = usize::from(channels);
    if channels == 1 {
        return samples.to_vec();
    }
    samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().copied().sum::<f32>() / channels as f32)
        .collect()
}

fn scaled_frame_count(frames: u64, input_rate: u32, output_rate: u32) -> usize {
    let numerator = u128::from(frames) * u128::from(output_rate);
    ((numerator + u128::from(input_rate / 2)) / u128::from(input_rate)) as usize
}

fn resample_to_asr(input: Vec<f32>, input_rate: u32, expected_samples: usize) -> Result<Vec<f32>> {
    if input_rate == ASR_SAMPLE_RATE {
        let mut output = input;
        output.resize(expected_samples, 0.0);
        output.truncate(expected_samples);
        return Ok(output);
    }
    if input.is_empty() {
        return Ok(vec![0.0; expected_samples]);
    }

    let mut resampler = FftFixedIn::<f32>::new(
        input_rate as usize,
        ASR_SAMPLE_RATE as usize,
        RESAMPLER_CHUNK_SIZE,
        1,
        1,
    )
    .context("create meeting source resampler")?;
    let output_delay = resampler.output_delay();
    let tail_input_frames = (((output_delay as u128 * u128::from(input_rate))
        + u128::from(ASR_SAMPLE_RATE - 1))
        / u128::from(ASR_SAMPLE_RATE)) as usize;
    let tail_input_frames = tail_input_frames.saturating_add(RESAMPLER_CHUNK_SIZE * 2);
    let total_input_frames = input.len().saturating_add(tail_input_frames);
    let mut output = Vec::with_capacity(expected_samples.saturating_add(output_delay));
    let mut offset = 0_usize;
    while offset < total_input_frames {
        let mut block = vec![0.0_f32; RESAMPLER_CHUNK_SIZE];
        let remaining = input.len().saturating_sub(offset);
        let copied = remaining.min(RESAMPLER_CHUNK_SIZE);
        if copied > 0 {
            block[..copied].copy_from_slice(&input[offset..offset + copied]);
        }
        let processed = resampler.process(&[&block], None)?;
        output.extend_from_slice(&processed[0]);
        offset = offset.saturating_add(RESAMPLER_CHUNK_SIZE);
    }
    if output.len() > output_delay {
        output.drain(..output_delay);
    } else {
        output.clear();
    }
    output.resize(expected_samples, 0.0);
    output.truncate(expected_samples);
    Ok(output)
}

fn derive_mix(session_root: &Path, output_frames: u64) -> Result<u64> {
    let microphone_path = session_root.join("microphone.wav");
    let system_path = session_root.join("system.wav");
    let mix_path = session_root.join("mix.wav");
    let mut microphone = WavReader::open(&microphone_path)?;
    let mut system = WavReader::open(&system_path)?;
    let spec = WavSpec {
        channels: 1,
        sample_rate: ASR_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut mix = WavWriter::create(&mix_path, spec)?;
    let mut microphone_samples = microphone.samples::<i16>();
    let mut system_samples = system.samples::<i16>();
    for _ in 0..output_frames {
        let mic = match microphone_samples.next() {
            Some(sample) => sample? as f32 / i16::MAX as f32,
            None => 0.0,
        };
        let desktop = match system_samples.next() {
            Some(sample) => sample? as f32 / i16::MAX as f32,
            None => 0.0,
        };
        mix.write_sample(to_i16((mic + desktop) * 0.5))?;
    }
    mix.finalize()?;
    File::open(&mix_path)?.sync_all()?;
    Ok(output_frames)
}

fn mark_recovered_track(track: &mut CaptureTrackDetailsManifest) {
    track.complete = false;
    if track.diarization_track.backend_status == CaptureBackendStatus::Capturing {
        track.diarization_track.backend_status = CaptureBackendStatus::Failed;
    }
    track.diarization_track.status = match track.diarization_track.backend_status {
        CaptureBackendStatus::Unavailable => CaptureTrackStatus::Unavailable,
        CaptureBackendStatus::Failed => CaptureTrackStatus::Failed,
        _ => CaptureTrackStatus::Partial,
    };
    for chunk in &mut track.chunks {
        if !chunk.complete {
            // Its latest checkpoint flushed the header and fsynced the file.
            // It is usable, but explicitly not a cleanly completed chunk.
            chunk.complete = false;
        }
    }
}

fn sync_outputs(session_root: &Path, manifest: &CaptureSessionManifest) -> Result<()> {
    for path in ["microphone.wav", "system.wav", "mix.wav", MANIFEST_FILE] {
        File::open(session_root.join(path))?.sync_all()?;
    }
    for track in [&manifest.microphone, &manifest.system] {
        for chunk in &track.chunks {
            File::open(session_root.join(&chunk.path))?.sync_all()?;
        }
    }
    Ok(())
}

fn write_capture_details_durable(path: &Path, manifest: &CaptureSessionManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    let mut file = File::create(path).with_context(|| format!("create manifest {path:?}"))?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn read_latest_checkpoint(session_root: &Path) -> Result<CaptureSessionManifest> {
    let mut newest: Option<CaptureSessionManifest> = None;
    for filename in [CHECKPOINT_A, CHECKPOINT_B, MANIFEST_FILE] {
        let path = session_root.join(filename);
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_reader::<_, CaptureSessionManifest>(file) else {
            continue;
        };
        if manifest.version != CAPTURE_SESSION_MANIFEST_VERSION {
            continue;
        }
        if newest
            .as_ref()
            .map(|current| manifest.checkpoint_generation >= current.checkpoint_generation)
            .unwrap_or(true)
        {
            newest = Some(manifest);
        }
    }
    newest.with_context(|| format!("no valid meeting capture checkpoint in {session_root:?}"))
}

fn audio_tracks_for_directory(directory: &str) -> AudioTracks {
    AudioTracks {
        mix: format!("{directory}/mix.wav"),
        microphone: format!("{directory}/microphone.wav"),
        system: format!("{directory}/system.wav"),
    }
}

fn utc_timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn unix_timestamp_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn to_i16(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    if sample <= -1.0 {
        i16::MIN
    } else if sample >= 1.0 {
        i16::MAX
    } else {
        (sample * i16::MAX as f32).round() as i16
    }
}

fn to_i32(sample: f32) -> i32 {
    if !sample.is_finite() {
        return 0;
    }
    (sample.clamp(-1.0, 1.0) * i32::MAX as f32).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn frame(
        source: CaptureSource,
        timestamp_ns: i64,
        sequence: u64,
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    ) -> TimestampedAudioFrame {
        TimestampedAudioFrame {
            source,
            timestamp_ns,
            sequence,
            sample_rate,
            channels,
            samples,
            dropped_frames: 0,
        }
    }

    #[test]
    fn timestamped_tracks_preserve_startup_offset_and_gap() {
        let temp = tempdir().unwrap();
        let mut session = MeetingCaptureSession::create_with_metadata(
            temp.path(),
            MeetingCaptureMetadata {
                microphone: CaptureSourceMetadata {
                    device_name: Some("Mic".to_owned()),
                    backend: Some("wasapi-input".to_owned()),
                    ..CaptureSourceMetadata::default()
                },
                system: CaptureSourceMetadata {
                    device_name: Some("Speakers".to_owned()),
                    backend: Some("wasapi-loopback".to_owned()),
                    ..CaptureSourceMetadata::default()
                },
            },
        )
        .unwrap();
        session
            .append_frame(frame(
                CaptureSource::Microphone,
                0,
                0,
                ASR_SAMPLE_RATE,
                1,
                vec![0.5; 160],
            ))
            .unwrap();
        session
            .append_frame(frame(
                CaptureSource::Microphone,
                20_000_000,
                1,
                ASR_SAMPLE_RATE,
                1,
                vec![0.25; 160],
            ))
            .unwrap();
        session
            .append_frame(frame(
                CaptureSource::System,
                10_000_000,
                0,
                ASR_SAMPLE_RATE,
                1,
                vec![0.75; 160],
            ))
            .unwrap();

        let artifacts = session.finalize_with_artifacts().unwrap();
        let microphone =
            WavReader::open(temp.path().join(&artifacts.audio_tracks.microphone)).unwrap();
        let system = WavReader::open(temp.path().join(&artifacts.audio_tracks.system)).unwrap();
        let mix = WavReader::open(temp.path().join(&artifacts.audio_tracks.mix)).unwrap();
        assert_eq!(microphone.len(), 480);
        assert_eq!(system.len(), 320);
        assert_eq!(mix.len(), 480);
        assert_eq!(artifacts.capture_details.microphone.timing.gap_count, 1);
        assert_eq!(
            artifacts.manifest.microphone.device_name.as_deref(),
            Some("Mic")
        );
        assert_eq!(artifacts.manifest.system.backend_name, "wasapi-loopback");
        assert!(temp.path().join(&artifacts.manifest_path).exists());
        let disk_manifest: CaptureSessionManifest = serde_json::from_reader(
            File::open(temp.path().join(&artifacts.manifest_path)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            disk_manifest
                .diarization_manifest
                .microphone
                .asr_path
                .as_deref(),
            Some("microphone.wav")
        );
        assert_eq!(
            disk_manifest
                .diarization_manifest
                .system
                .source_path
                .as_deref(),
            Some("source/system")
        );
        assert_eq!(
            disk_manifest
                .diarization_manifest
                .mix
                .as_ref()
                .map(|track| track.source),
            Some(CaptureSource::Mix)
        );
        assert!(temp
            .path()
            .join(&artifacts.session_root)
            .join("source/microphone/chunk-000000.wav")
            .exists());
    }

    #[test]
    fn derives_native_rate_stereo_source_to_16_khz() {
        let temp = tempdir().unwrap();
        let mut session = MeetingCaptureSession::create(temp.path()).unwrap();
        let mut stereo = Vec::new();
        for _ in 0..480 {
            stereo.extend([0.3, 0.1]);
        }
        session
            .append_frame(frame(CaptureSource::Microphone, 0, 0, 48_000, 2, stereo))
            .unwrap();
        let artifacts = session.finalize_with_artifacts().unwrap();
        let source = WavReader::open(
            temp.path()
                .join(&artifacts.session_root)
                .join("source/microphone/chunk-000000.wav"),
        )
        .unwrap();
        assert_eq!(source.spec().sample_rate, 48_000);
        assert_eq!(source.spec().channels, 2);
        assert_eq!(source.spec().sample_format, SampleFormat::Int);
        let microphone =
            WavReader::open(temp.path().join(&artifacts.audio_tracks.microphone)).unwrap();
        assert_eq!(microphone.spec().sample_rate, ASR_SAMPLE_RATE);
        assert_eq!(microphone.spec().channels, 1);
        assert_eq!(microphone.len(), 160);
    }

    #[test]
    fn keeps_checkpointed_session_for_recovery_after_drop() {
        let temp = tempdir().unwrap();
        let root = {
            let mut session = MeetingCaptureSession::create(temp.path()).unwrap();
            session
                .append_frame(frame(
                    CaptureSource::Microphone,
                    0,
                    0,
                    ASR_SAMPLE_RATE,
                    1,
                    vec![0.5; 160],
                ))
                .unwrap();
            session.checkpoint().unwrap();
            session.session_root().to_path_buf()
        };
        assert!(root.exists());
        let recoverable = MeetingCaptureSession::find_recoverable_sessions(temp.path()).unwrap();
        assert_eq!(recoverable.len(), 1);
        let artifacts = MeetingCaptureSession::recover_and_finalize(temp.path(), &root).unwrap();
        assert!(!artifacts.capture_details.complete);
        assert!(artifacts.capture_details.recovered_from_crash);
        assert!(temp
            .path()
            .join(&artifacts.audio_tracks.microphone)
            .exists());
    }

    #[test]
    fn recovers_a_finalized_but_unpublished_staging_session() {
        let temp = tempdir().unwrap();
        let root = {
            let mut session = MeetingCaptureSession::create(temp.path()).unwrap();
            session
                .append_frame(frame(
                    CaptureSource::Microphone,
                    0,
                    0,
                    ASR_SAMPLE_RATE,
                    1,
                    vec![0.5; 160],
                ))
                .unwrap();
            session.checkpoint().unwrap();

            // Simulate a power loss after the final manifest has been made
            // durable, but before the staging directory is atomically renamed
            // into its `meeting-*` publication path.
            let mut manifest = session.build_manifest();
            manifest.complete = true;
            manifest.finalized_at = Some("2026-08-04T00:00:00Z".to_owned());
            manifest.published_at = Some("2026-08-04T00:00:01Z".to_owned());
            write_capture_details_durable(&session.staging_dir.join(MANIFEST_FILE), &manifest)
                .unwrap();
            session.session_root().to_path_buf()
        };

        let recoverable = MeetingCaptureSession::find_recoverable_sessions(temp.path()).unwrap();
        assert_eq!(recoverable.len(), 1);

        let artifacts = MeetingCaptureSession::recover_and_finalize(temp.path(), &root).unwrap();
        assert!(!artifacts.capture_details.complete);
        assert!(artifacts.capture_details.recovered_from_crash);
        assert!(temp.path().join(&artifacts.manifest_path).exists());
        assert!(temp.path().join(&artifacts.audio_tracks.mix).exists());
    }

    #[test]
    #[allow(deprecated)]
    fn legacy_alignment_remains_compatible_for_existing_callers() {
        let temp = tempdir().unwrap();
        let mut session = MeetingCaptureSession::create(temp.path()).unwrap();
        session.append_aligned(&[1.0, 0.5], &[1.0]).unwrap();
        let tracks = session.finalize().unwrap();
        let mix = WavReader::open(temp.path().join(&tracks.mix)).unwrap();
        let microphone = WavReader::open(temp.path().join(&tracks.microphone)).unwrap();
        let system = WavReader::open(temp.path().join(&tracks.system)).unwrap();
        assert_eq!(mix.len(), 2);
        assert_eq!(microphone.len(), 2);
        assert_eq!(system.len(), 2);
        assert_eq!(mix.into_samples::<i16>().next().unwrap().unwrap(), i16::MAX);
    }
}
