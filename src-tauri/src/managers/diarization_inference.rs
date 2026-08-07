//! Local speech activity and WeSpeaker embedding inference for meeting tracks.
//!
//! This module is intentionally separate from [`super::diarization`]. The
//! latter owns deterministic attribution policy; this module owns the
//! fallible, model-dependent conversion from a 16 kHz mono ASR track into
//! timestamped speech turns and anonymous system-audio speaker embeddings.

use std::f32::consts::PI;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavReader, WavSpec};
use ndarray::{Array2, Array3};
use rustfft::{num_complex::Complex, Fft, FftPlanner};

use super::diarization::{CaptureSource, SpeakerEmbeddingWindow, SpeechTurn};
use super::diarization_model::DiarizationModelManager;

const SAMPLE_RATE_HZ: u32 = 16_000;
const FBANK_BINS: usize = 80;
const FFT_SIZE: usize = 512;
const FBANK_FRAME_SAMPLES: usize = 400;
const FBANK_HOP_SAMPLES: usize = 160;
const PREEMPHASIS: f32 = 0.97;
const LOG_ENERGY_FLOOR: f32 = 1.0e-10;

/// Energy-VAD controls used for both microphone and system ASR tracks.
///
/// The detector intentionally favours stable, mergeable speech spans over
/// frame-perfect boundaries. The attribution core applies its own duration
/// validation and interval merging after these turns are combined with the
/// capture manifest.
#[derive(Debug, Clone)]
pub struct SpeechActivityConfig {
    /// Analysis frame duration. The default matches the bundled Silero VAD's
    /// 30 ms cadence while keeping this optional feature model-independent.
    pub frame_ms: u32,
    /// A turn shorter than this duration is discarded as transient noise.
    pub minimum_speech_turn_ms: i64,
    /// Silence shorter than this is treated as a single turn.
    pub merge_gap_ms: i64,
    /// Keep an active turn alive for this amount of sub-threshold audio.
    pub hangover_ms: i64,
    /// Required dBFS margin above the estimated noise floor to start speech.
    pub activation_db_above_noise: f32,
    /// Lower clamp for the activation threshold in dBFS.
    pub minimum_activation_dbfs: f32,
    /// Upper clamp for the activation threshold in dBFS. This preserves recall
    /// when a meeting has little or no explicit silence to estimate from.
    pub maximum_activation_dbfs: f32,
    /// dBFS hysteresis applied after speech has started.
    pub release_hysteresis_db: f32,
}

impl Default for SpeechActivityConfig {
    fn default() -> Self {
        Self {
            frame_ms: 30,
            minimum_speech_turn_ms: 180,
            merge_gap_ms: 240,
            hangover_ms: 180,
            activation_db_above_noise: 8.0,
            minimum_activation_dbfs: -52.0,
            maximum_activation_dbfs: -38.0,
            release_hysteresis_db: 3.0,
        }
    }
}

/// Runtime limits and windowing controls for optional speaker embedding work.
#[derive(Debug, Clone)]
pub struct DiarizationInferenceConfig {
    /// Shared VAD configuration for microphone and system tracks.
    pub speech_activity: SpeechActivityConfig,
    /// Duration of each WeSpeaker input window.
    pub embedding_window_ms: i64,
    /// Candidate spacing before VAD coverage filtering.
    pub embedding_hop_ms: i64,
    /// Required fraction of each embedding window covered by detected speech.
    pub minimum_embedding_speech_coverage: f32,
    /// Hard upper bound on model invocations for a meeting.
    pub maximum_embedding_windows: usize,
    /// Safety bound for a single source file. Audio is streamed for VAD, but
    /// this prevents a malformed WAV from consuming unbounded CPU time.
    pub maximum_input_duration_ms: i64,
}

impl Default for DiarizationInferenceConfig {
    fn default() -> Self {
        Self {
            speech_activity: SpeechActivityConfig::default(),
            embedding_window_ms: 1_500,
            embedding_hop_ms: 750,
            minimum_embedding_speech_coverage: 0.60,
            maximum_embedding_windows: 10_000,
            maximum_input_duration_ms: 8 * 60 * 60 * 1_000,
        }
    }
}

/// Timestamped local-inference results for the retained system ASR track.
#[derive(Debug, Clone)]
pub struct SystemDiarizationInference {
    /// System-audio VAD turns on the shared meeting clock.
    pub speech_turns: Vec<SpeechTurn>,
    /// Unit-normalized, timestamped WeSpeaker embeddings for system speech.
    pub system_embeddings: Vec<SpeakerEmbeddingWindow>,
}

/// Reusable CPU session for the optional local WeSpeaker model.
///
/// Construct one instance per background diarization operation and reuse it
/// across the selected embedding windows. The type is deliberately mutable:
/// ONNX Runtime sessions execute inference through `&mut self`.
pub struct DiarizationInference {
    session: ort::session::Session,
    input_name: String,
    output_name: String,
    config: DiarizationInferenceConfig,
    fbank: FbankExtractor,
}

impl DiarizationInference {
    /// Loads the verified optional model managed by [`DiarizationModelManager`].
    ///
    /// # Errors
    ///
    /// Returns an error when the user has not downloaded the opt-in model, the
    /// model cannot be loaded by ONNX Runtime, or the supplied configuration is
    /// invalid.
    pub fn new(
        manager: &DiarizationModelManager,
        config: DiarizationInferenceConfig,
    ) -> Result<Self> {
        let model_path = manager
            .model_path()
            .context("resolve verified optional speaker diarization model")?;
        Self::from_model_path(&model_path, config)
    }

    /// Loads a WeSpeaker session from a concrete path.
    ///
    /// This is public for deterministic integration tests and offline tooling;
    /// product code should use [`Self::new`] so it shares the model manager's
    /// download and checksum lifecycle.
    pub fn from_model_path(
        model_path: impl AsRef<Path>,
        config: DiarizationInferenceConfig,
    ) -> Result<Self> {
        validate_inference_config(&config)?;

        let model_path = model_path.as_ref();
        if !model_path.is_file() {
            bail!("speaker diarization model does not exist at {model_path:?}");
        }

        let builder = ort::session::Session::builder()
            .map_err(|error| anyhow::anyhow!("create WeSpeaker ONNX session builder: {error}"))?;
        let mut builder = builder.with_intra_threads(1).map_err(|error| {
            anyhow::anyhow!("configure WeSpeaker ONNX session threads: {error}")
        })?;
        let session = builder.commit_from_file(model_path).map_err(|error| {
            anyhow::anyhow!("load WeSpeaker ONNX model {model_path:?}: {error}")
        })?;

        let input_name = session
            .inputs()
            .first()
            .ok_or_else(|| anyhow::anyhow!("WeSpeaker model has no input tensor"))?
            .name()
            .to_owned();
        let output_name = session
            .outputs()
            .first()
            .ok_or_else(|| anyhow::anyhow!("WeSpeaker model has no output tensor"))?
            .name()
            .to_owned();

        if input_name != "feats" || output_name != "embs" {
            bail!(
                "unexpected WeSpeaker model interface: expected feats -> embs, found {} -> {}",
                input_name,
                output_name
            );
        }

        Ok(Self {
            session,
            input_name,
            output_name,
            config,
            fbank: FbankExtractor::new(),
        })
    }

    /// Produces system-track VAD turns and WeSpeaker embeddings.
    ///
    /// `track_offset_ms` is added to every result after sample-clock analysis,
    /// so callers can pass the device-to-session offset from the capture
    /// manifest without approximating alignment from buffer length.
    ///
    /// # Errors
    ///
    /// Returns an error for an unreadable/non-16-kHz-mono WAV or an ONNX model
    /// failure. An empty result is valid when the track contains no speech.
    pub fn infer_system_wav(
        &mut self,
        wav_path: impl AsRef<Path>,
        track_offset_ms: i64,
    ) -> Result<SystemDiarizationInference> {
        let wav_path = wav_path.as_ref();
        let (metadata, energy_frames) = analyze_wav(wav_path, &self.config)?;
        let relative_turns = speech_turns_from_energy(
            &energy_frames,
            CaptureSource::System,
            &self.config.speech_activity,
        );
        let speech_turns = apply_track_offset(&relative_turns, track_offset_ms)?;

        let candidates = embedding_candidates(metadata.sample_count, &relative_turns, &self.config);
        if candidates.is_empty() {
            return Ok(SystemDiarizationInference {
                speech_turns,
                system_embeddings: Vec::new(),
            });
        }

        let mut wav_reader = WavWindowReader::open(wav_path, metadata)?;
        let mut system_embeddings = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let samples = wav_reader.read_window(candidate.start_sample, candidate.sample_count)?;
            let features = self.fbank.features(&samples)?;
            let embedding = self.run_embedding(features)?;
            system_embeddings.push(SpeakerEmbeddingWindow {
                start_ms: add_offset(samples_to_ms(candidate.start_sample), track_offset_ms)?,
                end_ms: add_offset(samples_to_ms(candidate.end_sample()), track_offset_ms)?,
                embedding,
                confidence: Some(candidate.speech_coverage),
            });
        }

        Ok(SystemDiarizationInference {
            speech_turns,
            system_embeddings,
        })
    }

    fn run_embedding(&mut self, features: Array2<f32>) -> Result<Vec<f32>> {
        let frame_count = features.nrows();
        if frame_count == 0 {
            bail!("cannot run WeSpeaker on an empty fbank matrix");
        }

        let input = Array3::from_shape_vec(
            (1, frame_count, FBANK_BINS),
            features.iter().copied().collect(),
        )
        .context("shape WeSpeaker fbank tensor")?;
        let input =
            ort::value::Value::from_array(input).context("create WeSpeaker ONNX input tensor")?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => input])
            .context("run WeSpeaker embedding inference")?;
        let output = outputs
            .get(self.output_name.as_str())
            .ok_or_else(|| anyhow::anyhow!("WeSpeaker inference did not return embs"))?;
        let output = output
            .try_extract_array::<f32>()
            .context("read WeSpeaker embedding output")?;

        if output.shape().first().copied() != Some(1) {
            bail!(
                "unexpected WeSpeaker embedding batch shape {:?}",
                output.shape()
            );
        }

        let mut embedding: Vec<f32> = output.iter().copied().collect();
        if embedding.is_empty() || embedding.iter().any(|value| !value.is_finite()) {
            bail!("WeSpeaker returned an empty or non-finite embedding");
        }

        let norm = embedding
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        if !norm.is_finite() || norm <= f32::EPSILON {
            bail!("WeSpeaker returned a zero-norm embedding");
        }
        for value in &mut embedding {
            *value /= norm;
        }
        Ok(embedding)
    }
}

/// Detects VAD turns from either retained 16 kHz mono ASR track.
///
/// `source` must be [`CaptureSource::Microphone`] or
/// [`CaptureSource::System`]. The mix is derived convenience output and is not
/// a primary attribution source. `track_offset_ms` places the returned turns on
/// the shared meeting clock rather than assuming the track began at sample zero.
///
/// # Errors
///
/// Returns an error for an invalid configuration, incompatible WAV, or an I/O
/// failure. A successful empty vector means the detector found no speech.
pub fn detect_speech_turns_from_wav(
    wav_path: impl AsRef<Path>,
    source: CaptureSource,
    track_offset_ms: i64,
    config: &DiarizationInferenceConfig,
) -> Result<Vec<SpeechTurn>> {
    if source == CaptureSource::Mix {
        bail!("the derived mix track is not a valid primary VAD source");
    }
    validate_inference_config(config)?;
    let (_, energy_frames) = analyze_wav(wav_path.as_ref(), config)?;
    let turns = speech_turns_from_energy(&energy_frames, source, &config.speech_activity);
    apply_track_offset(&turns, track_offset_ms)
}

#[derive(Debug, Clone, Copy)]
struct WavMetadata {
    sample_count: u64,
    sample_format: SampleFormat,
    bits_per_sample: u16,
}

#[derive(Debug, Clone, Copy)]
struct EnergyFrame {
    start_sample: u64,
    end_sample: u64,
    dbfs: f32,
}

#[derive(Debug, Clone, Copy)]
struct EmbeddingCandidate {
    start_sample: u64,
    sample_count: usize,
    speech_coverage: f32,
}

impl EmbeddingCandidate {
    fn end_sample(self) -> u64 {
        self.start_sample + self.sample_count as u64
    }
}

struct WavWindowReader {
    reader: WavReader<BufReader<File>>,
    metadata: WavMetadata,
}

impl WavWindowReader {
    fn open(path: &Path, metadata: WavMetadata) -> Result<Self> {
        let reader = WavReader::open(path).with_context(|| format!("open ASR WAV {path:?}"))?;
        validate_wav_spec(reader.spec())?;
        Ok(Self { reader, metadata })
    }

    fn read_window(&mut self, start_sample: u64, sample_count: usize) -> Result<Vec<f32>> {
        let start_sample =
            u32::try_from(start_sample).context("embedding window starts beyond WAV seek range")?;
        self.reader
            .seek(start_sample)
            .context("seek ASR WAV for embedding window")?;

        let mut samples = Vec::with_capacity(sample_count);
        match self.metadata.sample_format {
            SampleFormat::Float => {
                for sample in self.reader.samples::<f32>().take(sample_count) {
                    samples.push(normalize_float_sample(sample?)?);
                }
            }
            SampleFormat::Int => {
                let scale = integer_scale(self.metadata.bits_per_sample)?;
                for sample in self.reader.samples::<i32>().take(sample_count) {
                    samples.push(normalize_int_sample(sample?, scale)?);
                }
            }
        }

        if samples.len() != sample_count {
            bail!(
                "ASR WAV ended while reading a {}-sample embedding window at sample {}",
                sample_count,
                start_sample
            );
        }
        Ok(samples)
    }
}

struct FbankExtractor {
    window: Vec<f32>,
    filters: Vec<Vec<f32>>,
    fft: Arc<dyn Fft<f32>>,
}

impl FbankExtractor {
    fn new() -> Self {
        let mut planner = FftPlanner::new();
        Self {
            // WeSpeaker's published ONNX inference helper calls Kaldi fbank
            // with `window_type='hamming'`. Matching that frontend matters:
            // a Povey window is a plausible speech frontend, but produces
            // different embeddings from the model's trained distribution.
            window: hamming_window(FBANK_FRAME_SAMPLES),
            filters: mel_filterbank(),
            fft: planner.plan_fft_forward(FFT_SIZE),
        }
    }

    fn features(&self, samples: &[f32]) -> Result<Array2<f32>> {
        if samples.len() < FBANK_FRAME_SAMPLES {
            bail!("WeSpeaker embedding window is shorter than one fbank frame");
        }

        let frame_count = 1 + (samples.len() - FBANK_FRAME_SAMPLES) / FBANK_HOP_SAMPLES;
        let mut features = Array2::<f32>::zeros((frame_count, FBANK_BINS));
        let mut spectrum = vec![Complex::new(0.0_f32, 0.0_f32); FFT_SIZE];
        let mut power = vec![0.0_f32; FFT_SIZE / 2 + 1];

        for frame_index in 0..frame_count {
            let frame_start = frame_index * FBANK_HOP_SAMPLES;
            let frame = &samples[frame_start..frame_start + FBANK_FRAME_SAMPLES];
            let mean = frame.iter().sum::<f32>() / frame.len() as f32;

            spectrum.fill(Complex::new(0.0, 0.0));
            for sample_index in 0..FBANK_FRAME_SAMPLES {
                let current = frame[sample_index] - mean;
                let previous = if sample_index == 0 {
                    current
                } else {
                    frame[sample_index - 1] - mean
                };
                spectrum[sample_index].re =
                    (current - PREEMPHASIS * previous) * self.window[sample_index];
            }
            self.fft.process(&mut spectrum);

            for (bin, value) in power.iter_mut().enumerate() {
                *value = spectrum[bin].norm_sqr();
            }
            for (mel_index, filter) in self.filters.iter().enumerate() {
                let energy = filter
                    .iter()
                    .zip(&power)
                    .map(|(weight, value)| weight * value)
                    .sum::<f32>();
                features[[frame_index, mel_index]] = energy.max(LOG_ENERGY_FLOOR).ln();
            }
        }

        // WeSpeaker's exported models use Kaldi fbank followed by mean-only
        // cepstral normalization over the current embedding window.
        for mel_index in 0..FBANK_BINS {
            let mean = (0..frame_count)
                .map(|frame_index| features[[frame_index, mel_index]])
                .sum::<f32>()
                / frame_count as f32;
            for frame_index in 0..frame_count {
                features[[frame_index, mel_index]] -= mean;
            }
        }

        Ok(features)
    }
}

fn validate_inference_config(config: &DiarizationInferenceConfig) -> Result<()> {
    let activity = &config.speech_activity;
    if activity.frame_ms == 0
        || activity.minimum_speech_turn_ms < 0
        || activity.merge_gap_ms < 0
        || activity.hangover_ms < 0
        || config.embedding_window_ms < 1_000
        || config.embedding_hop_ms <= 0
        || config.maximum_embedding_windows == 0
        || config.maximum_input_duration_ms <= 0
    {
        bail!("diarization inference duration limits must be positive");
    }
    if !(0.0..=1.0).contains(&config.minimum_embedding_speech_coverage)
        || !activity.activation_db_above_noise.is_finite()
        || !activity.minimum_activation_dbfs.is_finite()
        || !activity.maximum_activation_dbfs.is_finite()
        || !activity.release_hysteresis_db.is_finite()
        || activity.minimum_activation_dbfs > activity.maximum_activation_dbfs
        || activity.release_hysteresis_db < 0.0
    {
        bail!("diarization inference thresholds are outside their valid ranges");
    }
    Ok(())
}

fn analyze_wav(
    path: &Path,
    config: &DiarizationInferenceConfig,
) -> Result<(WavMetadata, Vec<EnergyFrame>)> {
    let mut reader = WavReader::open(path).with_context(|| format!("open ASR WAV {path:?}"))?;
    let spec = reader.spec();
    validate_wav_spec(spec)?;
    let metadata = WavMetadata {
        sample_count: u64::from(reader.duration()),
        sample_format: spec.sample_format,
        bits_per_sample: spec.bits_per_sample,
    };
    let duration_ms = samples_to_ms(metadata.sample_count);
    if duration_ms > config.maximum_input_duration_ms {
        bail!(
            "ASR WAV duration ({duration_ms} ms) exceeds the configured diarization limit ({} ms)",
            config.maximum_input_duration_ms
        );
    }

    let frame_samples = ms_to_samples(i64::from(config.speech_activity.frame_ms))
        .try_into()
        .context("VAD frame size exceeds platform limits")?;
    let mut frames = Vec::new();
    match metadata.sample_format {
        SampleFormat::Float => {
            let mut samples = reader.samples::<f32>();
            collect_energy_frames(&mut samples, frame_samples, &mut frames, |sample| {
                normalize_float_sample(sample)
            })?;
        }
        SampleFormat::Int => {
            let scale = integer_scale(metadata.bits_per_sample)?;
            let mut samples = reader.samples::<i32>();
            collect_energy_frames(&mut samples, frame_samples, &mut frames, |sample| {
                normalize_int_sample(sample, scale)
            })?;
        }
    }

    Ok((metadata, frames))
}

fn collect_energy_frames<T, I, F>(
    samples: &mut I,
    frame_samples: usize,
    frames: &mut Vec<EnergyFrame>,
    normalize: F,
) -> Result<()>
where
    I: Iterator<Item = std::result::Result<T, hound::Error>>,
    F: Fn(T) -> Result<f32>,
{
    let mut start_sample = 0_u64;
    let mut frame = Vec::with_capacity(frame_samples);
    loop {
        frame.clear();
        for sample in samples.by_ref().take(frame_samples) {
            frame.push(normalize(sample?)?);
        }
        if frame.is_empty() {
            break;
        }

        let mean_square =
            frame.iter().map(|sample| sample * sample).sum::<f32>() / frame.len() as f32;
        let dbfs = 10.0 * mean_square.max(1.0e-12).log10();
        let end_sample = start_sample + frame.len() as u64;
        frames.push(EnergyFrame {
            start_sample,
            end_sample,
            dbfs,
        });
        start_sample = end_sample;
    }
    Ok(())
}

fn validate_wav_spec(spec: WavSpec) -> Result<()> {
    if spec.sample_rate != SAMPLE_RATE_HZ || spec.channels != 1 {
        bail!(
            "speaker diarization requires a 16 kHz mono ASR WAV, found {} Hz / {} channels",
            spec.sample_rate,
            spec.channels
        );
    }
    match spec.sample_format {
        SampleFormat::Float if spec.bits_per_sample == 32 => Ok(()),
        SampleFormat::Int if (1..=32).contains(&spec.bits_per_sample) => Ok(()),
        SampleFormat::Float => bail!(
            "speaker diarization supports 32-bit float WAVs, found {} bits",
            spec.bits_per_sample
        ),
        SampleFormat::Int => bail!(
            "speaker diarization supports signed PCM WAVs up to 32 bits, found {} bits",
            spec.bits_per_sample
        ),
    }
}

fn normalize_float_sample(sample: f32) -> Result<f32> {
    if !sample.is_finite() {
        bail!("ASR WAV contains a non-finite floating-point sample");
    }
    Ok(sample.clamp(-1.0, 1.0))
}

fn normalize_int_sample(sample: i32, scale: f32) -> Result<f32> {
    if !scale.is_finite() || scale <= 0.0 {
        bail!("invalid PCM normalization scale");
    }
    Ok((sample as f32 / scale).clamp(-1.0, 1.0))
}

fn integer_scale(bits_per_sample: u16) -> Result<f32> {
    if !(1..=32).contains(&bits_per_sample) {
        bail!("unsupported PCM bit depth {bits_per_sample}");
    }
    Ok((1_i64 << (bits_per_sample - 1)) as f32)
}

fn speech_turns_from_energy(
    frames: &[EnergyFrame],
    source: CaptureSource,
    config: &SpeechActivityConfig,
) -> Vec<SpeechTurn> {
    if frames.is_empty() {
        return Vec::new();
    }

    let noise_floor = percentile_dbfs(frames, 0.20);
    let activation = (noise_floor + config.activation_db_above_noise).clamp(
        config.minimum_activation_dbfs,
        config.maximum_activation_dbfs,
    );
    let release = activation - config.release_hysteresis_db;
    let hangover_samples = ms_to_samples(config.hangover_ms);

    let mut turns = Vec::new();
    let mut active_start = None;
    let mut last_active_end = 0_u64;
    let mut confidence_sum = 0.0_f32;
    let mut confidence_count = 0_u64;

    for frame in frames {
        let starts_speech = frame.dbfs >= activation;
        let remains_speech = frame.dbfs >= release;
        match active_start {
            None if starts_speech => {
                active_start = Some(frame.start_sample);
                last_active_end = frame.end_sample;
                confidence_sum = frame_confidence(frame.dbfs, activation);
                confidence_count = 1;
            }
            Some(start_sample) if remains_speech => {
                last_active_end = frame.end_sample;
                confidence_sum += frame_confidence(frame.dbfs, activation);
                confidence_count += 1;
                active_start = Some(start_sample);
            }
            Some(start_sample)
                if frame.start_sample.saturating_sub(last_active_end) <= hangover_samples =>
            {
                active_start = Some(start_sample);
            }
            Some(start_sample) => {
                push_turn(
                    &mut turns,
                    source,
                    start_sample,
                    last_active_end,
                    confidence_sum,
                    confidence_count,
                    config.minimum_speech_turn_ms,
                );
                active_start = if starts_speech {
                    last_active_end = frame.end_sample;
                    confidence_sum = frame_confidence(frame.dbfs, activation);
                    confidence_count = 1;
                    Some(frame.start_sample)
                } else {
                    confidence_sum = 0.0;
                    confidence_count = 0;
                    None
                };
            }
            None => {}
        }
    }

    if let Some(start_sample) = active_start {
        push_turn(
            &mut turns,
            source,
            start_sample,
            last_active_end,
            confidence_sum,
            confidence_count,
            config.minimum_speech_turn_ms,
        );
    }

    merge_nearby_turns(&mut turns, config.merge_gap_ms);
    turns
}

fn percentile_dbfs(frames: &[EnergyFrame], percentile: f32) -> f32 {
    let mut levels: Vec<f32> = frames.iter().map(|frame| frame.dbfs).collect();
    levels.sort_unstable_by(|left, right| left.total_cmp(right));
    let index = ((levels.len() - 1) as f32 * percentile.clamp(0.0, 1.0)).round() as usize;
    levels[index]
}

fn frame_confidence(dbfs: f32, activation: f32) -> f32 {
    ((dbfs - activation) / 24.0).clamp(0.0, 1.0)
}

fn push_turn(
    turns: &mut Vec<SpeechTurn>,
    source: CaptureSource,
    start_sample: u64,
    end_sample: u64,
    confidence_sum: f32,
    confidence_count: u64,
    minimum_duration_ms: i64,
) {
    let start_ms = samples_to_ms(start_sample);
    let end_ms = samples_to_ms(end_sample);
    if end_ms - start_ms < minimum_duration_ms {
        return;
    }
    turns.push(SpeechTurn {
        source,
        start_ms,
        end_ms,
        confidence: (confidence_count > 0)
            .then(|| (confidence_sum / confidence_count as f32).clamp(0.0, 1.0)),
    });
}

fn merge_nearby_turns(turns: &mut Vec<SpeechTurn>, merge_gap_ms: i64) {
    let mut merged: Vec<SpeechTurn> = Vec::with_capacity(turns.len());
    for turn in turns.drain(..) {
        let can_merge = merged.last().is_some_and(|previous| {
            previous.source == turn.source && turn.start_ms - previous.end_ms <= merge_gap_ms
        });
        if !can_merge {
            merged.push(turn);
            continue;
        }

        if let Some(previous) = merged.last_mut() {
            let previous_duration = (previous.end_ms - previous.start_ms).max(1) as f32;
            let next_duration = (turn.end_ms - turn.start_ms).max(1) as f32;
            previous.end_ms = turn.end_ms;
            previous.confidence = match (previous.confidence, turn.confidence) {
                (Some(left), Some(right)) => Some(
                    (left * previous_duration + right * next_duration)
                        / (previous_duration + next_duration),
                ),
                (Some(value), None) | (None, Some(value)) => Some(value),
                (None, None) => None,
            };
        }
    }
    *turns = merged;
}

fn embedding_candidates(
    sample_count: u64,
    speech_turns: &[SpeechTurn],
    config: &DiarizationInferenceConfig,
) -> Vec<EmbeddingCandidate> {
    let window_samples = ms_to_samples(config.embedding_window_ms);
    let hop_samples = ms_to_samples(config.embedding_hop_ms);
    if sample_count < window_samples || hop_samples == 0 {
        return Vec::new();
    }

    let last_start = sample_count - window_samples;
    let mut starts = Vec::new();
    let mut start = 0_u64;
    while start <= last_start {
        starts.push(start);
        start = start.saturating_add(hop_samples);
    }
    if starts.last().copied() != Some(last_start) {
        starts.push(last_start);
    }

    let mut candidates = starts
        .into_iter()
        .filter_map(|start_sample| {
            let end_sample = start_sample + window_samples;
            let coverage = speech_coverage(start_sample, end_sample, speech_turns);
            (coverage >= config.minimum_embedding_speech_coverage).then_some(EmbeddingCandidate {
                start_sample,
                sample_count: usize::try_from(window_samples).ok()?,
                speech_coverage: coverage,
            })
        })
        .collect::<Vec<_>>();

    if candidates.len() > config.maximum_embedding_windows {
        candidates = select_evenly(candidates, config.maximum_embedding_windows);
    }
    candidates
}

fn speech_coverage(start_sample: u64, end_sample: u64, turns: &[SpeechTurn]) -> f32 {
    let covered_samples = turns
        .iter()
        .map(|turn| {
            let turn_start = ms_to_samples(turn.start_ms);
            let turn_end = ms_to_samples(turn.end_ms);
            end_sample
                .min(turn_end)
                .saturating_sub(start_sample.max(turn_start))
        })
        .sum::<u64>();
    covered_samples as f32 / (end_sample - start_sample).max(1) as f32
}

fn select_evenly(candidates: Vec<EmbeddingCandidate>, maximum: usize) -> Vec<EmbeddingCandidate> {
    if maximum == 1 {
        return candidates.into_iter().next().into_iter().collect();
    }
    let last = candidates.len() - 1;
    (0..maximum)
        .map(|index| candidates[index * last / (maximum - 1)])
        .collect()
}

fn apply_track_offset(turns: &[SpeechTurn], offset_ms: i64) -> Result<Vec<SpeechTurn>> {
    turns
        .iter()
        .map(|turn| {
            Ok(SpeechTurn {
                source: turn.source,
                start_ms: add_offset(turn.start_ms, offset_ms)?,
                end_ms: add_offset(turn.end_ms, offset_ms)?,
                confidence: turn.confidence,
            })
        })
        .collect()
}

fn add_offset(value_ms: i64, offset_ms: i64) -> Result<i64> {
    value_ms
        .checked_add(offset_ms)
        .ok_or_else(|| anyhow::anyhow!("track timestamp offset overflow"))
}

fn samples_to_ms(samples: u64) -> i64 {
    ((samples.saturating_mul(1_000)) / u64::from(SAMPLE_RATE_HZ)) as i64
}

fn ms_to_samples(milliseconds: i64) -> u64 {
    milliseconds.max(0) as u64 * u64::from(SAMPLE_RATE_HZ) / 1_000
}

fn hamming_window(sample_count: usize) -> Vec<f32> {
    (0..sample_count)
        .map(|index| 0.54 - 0.46 * (2.0 * PI * index as f32 / (sample_count - 1) as f32).cos())
        .collect()
}

fn mel_filterbank() -> Vec<Vec<f32>> {
    let low_mel = hz_to_mel(20.0);
    let high_mel = hz_to_mel(SAMPLE_RATE_HZ as f32 / 2.0);
    let mel_points = (0..FBANK_BINS + 2)
        .map(|index| {
            let fraction = index as f32 / (FBANK_BINS + 1) as f32;
            mel_to_hz(low_mel + fraction * (high_mel - low_mel))
        })
        .collect::<Vec<_>>();

    (0..FBANK_BINS)
        .map(|mel_index| {
            let left = mel_points[mel_index];
            let center = mel_points[mel_index + 1];
            let right = mel_points[mel_index + 2];
            (0..=FFT_SIZE / 2)
                .map(|bin| {
                    let frequency = bin as f32 * SAMPLE_RATE_HZ as f32 / FFT_SIZE as f32;
                    if frequency < left || frequency > right {
                        0.0
                    } else if frequency <= center {
                        (frequency - left) / (center - left).max(f32::EPSILON)
                    } else {
                        (right - frequency) / (right - center).max(f32::EPSILON)
                    }
                })
                .collect()
        })
        .collect()
}

fn hz_to_mel(hz: f32) -> f32 {
    1_127.0 * (1.0 + hz / 700.0).ln()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (mel / 1_127.0).exp_m1()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> DiarizationInferenceConfig {
        DiarizationInferenceConfig::default()
    }

    fn energy_frame(start_ms: i64, end_ms: i64, dbfs: f32) -> EnergyFrame {
        EnergyFrame {
            start_sample: ms_to_samples(start_ms),
            end_sample: ms_to_samples(end_ms),
            dbfs,
        }
    }

    #[test]
    fn energy_vad_should_ignore_silence_and_detect_sustained_speech() {
        let frames = vec![
            energy_frame(0, 30, -96.0),
            energy_frame(30, 60, -96.0),
            energy_frame(60, 90, -28.0),
            energy_frame(90, 120, -27.0),
            energy_frame(120, 150, -27.0),
            energy_frame(150, 180, -27.0),
            energy_frame(180, 210, -27.0),
            energy_frame(210, 240, -27.0),
            energy_frame(240, 270, -27.0),
            energy_frame(270, 300, -96.0),
        ];

        let turns = speech_turns_from_energy(
            &frames,
            CaptureSource::System,
            &test_config().speech_activity,
        );

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].source, CaptureSource::System);
        assert_eq!(turns[0].start_ms, 60);
        assert_eq!(turns[0].end_ms, 270);
    }

    #[test]
    fn energy_vad_should_merge_a_short_silence_gap() {
        let frames = vec![
            energy_frame(0, 30, -30.0),
            energy_frame(30, 60, -30.0),
            energy_frame(60, 90, -30.0),
            energy_frame(90, 120, -30.0),
            energy_frame(120, 150, -30.0),
            energy_frame(150, 180, -30.0),
            energy_frame(180, 210, -96.0),
            energy_frame(210, 240, -96.0),
            energy_frame(240, 270, -30.0),
            energy_frame(270, 300, -30.0),
            energy_frame(300, 330, -30.0),
            energy_frame(330, 360, -30.0),
            energy_frame(360, 390, -30.0),
            energy_frame(390, 420, -30.0),
        ];

        let turns = speech_turns_from_energy(
            &frames,
            CaptureSource::Microphone,
            &test_config().speech_activity,
        );

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].start_ms, 0);
        assert_eq!(turns[0].end_ms, 420);
    }

    #[test]
    fn fbank_should_produce_mean_normalized_eighty_bin_features() {
        let samples = (0..24_000)
            .map(|index| (2.0 * PI * 220.0 * index as f32 / SAMPLE_RATE_HZ as f32).sin() * 0.1)
            .collect::<Vec<_>>();
        let features = FbankExtractor::new().features(&samples).unwrap();

        assert_eq!(features.ncols(), FBANK_BINS);
        assert!(features
            .columns()
            .into_iter()
            .all(|column| column.iter().sum::<f32>().abs() < 1.0e-3));
    }

    #[test]
    fn embedding_candidates_should_keep_only_speech_covered_windows() {
        let turns = vec![SpeechTurn {
            source: CaptureSource::System,
            start_ms: 500,
            end_ms: 2_000,
            confidence: Some(1.0),
        }];
        let mut config = test_config();
        config.minimum_embedding_speech_coverage = 0.75;

        let candidates = embedding_candidates(ms_to_samples(3_000), &turns, &config);

        assert_eq!(candidates.len(), 1);
        assert_eq!(samples_to_ms(candidates[0].start_sample), 750);
    }
}
