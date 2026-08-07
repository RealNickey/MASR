//! Track-aware, deterministic speaker attribution for captured meetings.
//!
//! This module deliberately does not depend on an ONNX model or a particular
//! capture backend. Capture adapters convert device timestamps into the shared
//! meeting clock, run VAD/embedding inference, and pass those timestamped
//! observations to [`diarize`]. Keeping that boundary explicit makes the
//! attribution policy deterministic, testable, and safe to ship separately
//! from model delivery.

use serde::{Deserialize, Serialize};
use specta::Type;
use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

/// The current on-disk capture manifest format understood by this module.
pub const CAPTURE_MANIFEST_VERSION: u32 = 1;

const MIN_VECTOR_NORM: f32 = 1.0e-12;

/// A source in a multi-track meeting capture.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    Microphone,
    System,
    /// A convenience rendering derived from the source tracks. It is never
    /// accepted as a primary diarization input.
    Mix,
}

/// The runtime state reported by a platform capture backend.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum CaptureBackendStatus {
    Ready,
    Capturing,
    Stopped,
    Unavailable,
    Failed,
}

/// Whether a recorded track can be trusted as a complete source.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum CaptureTrackStatus {
    Complete,
    Partial,
    Unavailable,
    Failed,
}

/// Provenance for one retained recording track.
///
/// `source_path` is the high-quality archival recording. `asr_path` is the
/// derived, speech-recognition-friendly representation (normally 16 kHz mono)
/// and must not replace the archival source. All clock values are expressed in
/// milliseconds relative to the meeting session unless documented otherwise.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CaptureTrackManifest {
    pub source: CaptureSource,
    pub source_path: Option<String>,
    pub asr_path: Option<String>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub asr_sample_rate_hz: Option<u32>,
    pub asr_channels: Option<u16>,
    pub device_name: Option<String>,
    pub backend_name: String,
    pub backend_status: CaptureBackendStatus,
    /// Unix time in milliseconds when this device's capture stream started.
    pub started_at_unix_ms: i64,
    /// Signed offset from the device stream clock to the shared meeting clock.
    pub clock_offset_ms: i64,
    pub dropped_frames: u64,
    pub status: CaptureTrackStatus,
}

/// Durable metadata for the source tracks belonging to one meeting.
///
/// The mix is optional because it is derived convenience output; microphone
/// and system tracks are the sources of truth for attribution.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct MeetingCaptureManifest {
    pub version: u32,
    pub session_started_at_unix_ms: i64,
    pub microphone: CaptureTrackManifest,
    pub system: CaptureTrackManifest,
    pub mix: Option<CaptureTrackManifest>,
}

/// A VAD-confirmed speech interval on the shared meeting clock.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SpeechTurn {
    pub source: CaptureSource,
    pub start_ms: i64,
    pub end_ms: i64,
    pub confidence: Option<f32>,
}

/// A system-audio embedding window produced by a local speaker-embedding model.
///
/// System audio is intentionally the only embedding source: microphone speech
/// is attributable to the local user without trying to infer their identity.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct SpeakerEmbeddingWindow {
    pub start_ms: i64,
    pub end_ms: i64,
    pub embedding: Vec<f32>,
    pub confidence: Option<f32>,
}

/// One timestamped token from a track-aware transcription pass.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct TranscriptWord {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub confidence: Option<f32>,
}

/// The input boundary between capture/model adapters and deterministic policy.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DiarizationInput {
    pub manifest: MeetingCaptureManifest,
    pub speech_turns: Vec<SpeechTurn>,
    pub system_embeddings: Vec<SpeakerEmbeddingWindow>,
    pub transcript_words: Vec<TranscriptWord>,
}

/// Tunable policy for local diarization.
///
/// `enabled` is intentionally explicit so callers can keep the feature behind
/// a user setting without loading a diarization model when it is off.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DiarizationConfig {
    pub enabled: bool,
    pub minimum_speech_turn_ms: i64,
    pub speech_turn_merge_gap_ms: i64,
    pub remote_cluster_max_cosine_distance: f32,
    pub minimum_word_label_coverage: f32,
    pub multiple_overlap_coverage: f32,
    pub word_segment_merge_gap_ms: i64,
    pub maximum_embedding_windows: usize,
}

impl Default for DiarizationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            minimum_speech_turn_ms: 120,
            speech_turn_merge_gap_ms: 150,
            remote_cluster_max_cosine_distance: 0.22,
            minimum_word_label_coverage: 0.55,
            multiple_overlap_coverage: 0.20,
            word_segment_merge_gap_ms: 750,
            maximum_embedding_windows: 10_000,
        }
    }
}

/// Attribution availability for one diarization attempt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum DiarizationStatus {
    Disabled,
    Unavailable,
    SourceAttributionOnly,
    Complete,
    Failed,
}

/// A human-readable source and optional remote-speaker cluster identity.
///
/// A two-track recorder can reliably identify microphone versus system audio,
/// but cannot assign participant names. `RemoteSpeaker` is an anonymous,
/// deterministic cluster number ordered by first observed speech.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Type)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerLabel {
    LocalUser,
    RemoteUnattributed,
    RemoteSpeaker { index: u32 },
    Multiple,
    Unknown,
}

/// A contiguous labelled interval of the meeting clock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Type)]
pub struct DiarizationTurn {
    pub start_ms: i64,
    pub end_ms: i64,
    pub label: SpeakerLabel,
    pub sources: Vec<CaptureSource>,
}

/// A readable transcript segment produced by intersecting words with turns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Type)]
pub struct SpeakerSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub label: SpeakerLabel,
    /// Fraction of the segment's word duration covered by its assigned label.
    pub label_coverage: f32,
    pub text: String,
}

/// Non-sensitive counters and availability context for UI and history records.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Type)]
pub struct DiarizationDiagnostics {
    pub microphone_speech_turns: usize,
    pub system_speech_turns: usize,
    pub embedding_windows: usize,
    pub remote_speaker_count: usize,
    pub microphone_dropped_frames: u64,
    pub system_dropped_frames: u64,
    pub reason: Option<String>,
}

/// The deterministic output, separate from model-loading or inference status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Type)]
pub struct DiarizationOutcome {
    pub status: DiarizationStatus,
    pub turns: Vec<DiarizationTurn>,
    pub segments: Vec<SpeakerSegment>,
    pub diagnostics: DiarizationDiagnostics,
}

/// A stable machine-readable failure category for caller-visible diagnostics.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum DiarizationErrorCode {
    InvalidConfiguration,
    InvalidManifest,
    InvalidSpeechTurn,
    InvalidEmbedding,
    InvalidTranscriptWord,
}

/// An input validation failure. Inference adapters should preserve this type
/// instead of converting malformed local data into an unhelpful panic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Type)]
pub struct DiarizationError {
    pub code: DiarizationErrorCode,
    pub message: String,
}

impl DiarizationError {
    fn new(code: DiarizationErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for DiarizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl Error for DiarizationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimeSpan {
    start_ms: i64,
    end_ms: i64,
}

impl TimeSpan {
    fn duration_ms(self) -> i64 {
        self.end_ms - self.start_ms
    }

    fn overlaps(self, other: Self) -> bool {
        self.start_ms < other.end_ms && other.start_ms < self.end_ms
    }

    fn overlap_ms(self, other: Self) -> i64 {
        (self.end_ms.min(other.end_ms) - self.start_ms.max(other.start_ms)).max(0)
    }
}

#[derive(Debug, Clone)]
struct NormalizedEmbedding {
    span: TimeSpan,
    values: Vec<f32>,
    original_index: usize,
}

#[derive(Debug, Clone)]
struct RemoteCluster {
    centroid: Vec<f32>,
    observations: usize,
    earliest_start_ms: i64,
    first_original_index: usize,
}

#[derive(Debug, Clone, Copy)]
struct LabelledEmbedding {
    span: TimeSpan,
    label: SpeakerLabel,
}

/// Runs deterministic, track-aware speaker attribution.
///
/// The capture and transcription adapters must first express every timestamp
/// on the shared meeting clock. This function then gives microphone speech the
/// stable `LocalUser` label, clusters only system-audio embeddings into
/// anonymous remote speakers, represents simultaneous microphone/system speech
/// as `Multiple`, and intersects that timeline with timestamped words.
///
/// # Errors
///
/// Returns [`DiarizationError`] when the manifest, timestamps, vectors, or
/// policy contain invalid data. A missing or incomplete source track is an
/// expected runtime condition and returns [`DiarizationStatus::Unavailable`]
/// with unlabelled transcript segments instead.
pub fn diarize(
    input: &DiarizationInput,
    config: &DiarizationConfig,
) -> Result<DiarizationOutcome, DiarizationError> {
    validate_config(config)?;

    if !config.enabled {
        return Ok(DiarizationOutcome {
            status: DiarizationStatus::Disabled,
            turns: Vec::new(),
            segments: Vec::new(),
            diagnostics: base_diagnostics(&input.manifest, None),
        });
    }

    validate_manifest(&input.manifest)?;
    let words = normalize_words(&input.transcript_words)?;
    validate_speech_turns(&input.speech_turns)?;
    let embeddings = normalize_embeddings(&input.system_embeddings, config)?;

    if !has_complete_source_tracks(&input.manifest) {
        return Ok(DiarizationOutcome {
            status: DiarizationStatus::Unavailable,
            turns: Vec::new(),
            segments: speaker_segments_from_words(&words, &[], config),
            diagnostics: base_diagnostics(
                &input.manifest,
                Some(
                    "Speaker attribution requires complete microphone and system source tracks."
                        .to_string(),
                ),
            ),
        });
    }

    let microphone_turns = normalized_speech_spans(
        &input.speech_turns,
        CaptureSource::Microphone,
        config.minimum_speech_turn_ms,
        config.speech_turn_merge_gap_ms,
    );
    let system_turns = normalized_speech_spans(
        &input.speech_turns,
        CaptureSource::System,
        config.minimum_speech_turn_ms,
        config.speech_turn_merge_gap_ms,
    );

    let (labelled_embeddings, remote_speaker_count) =
        cluster_system_embeddings(&embeddings, &system_turns, config);
    let turns = build_timeline(&microphone_turns, &system_turns, &labelled_embeddings);
    let segments = speaker_segments_from_words(&words, &turns, config);
    let status = if labelled_embeddings.is_empty() {
        DiarizationStatus::SourceAttributionOnly
    } else {
        DiarizationStatus::Complete
    };

    Ok(DiarizationOutcome {
        status,
        turns,
        segments,
        diagnostics: DiarizationDiagnostics {
            microphone_speech_turns: microphone_turns.len(),
            system_speech_turns: system_turns.len(),
            embedding_windows: embeddings.len(),
            remote_speaker_count,
            ..base_diagnostics(&input.manifest, None)
        },
    })
}

fn validate_config(config: &DiarizationConfig) -> Result<(), DiarizationError> {
    if config.minimum_speech_turn_ms < 0
        || config.speech_turn_merge_gap_ms < 0
        || config.word_segment_merge_gap_ms < 0
        || config.maximum_embedding_windows == 0
    {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidConfiguration,
            "Diarization duration limits must be non-negative and maximum_embedding_windows must be positive.",
        ));
    }

    let valid_ratio = |value: f32| value.is_finite() && (0.0..=1.0).contains(&value);
    if !config.remote_cluster_max_cosine_distance.is_finite()
        || !(0.0..=2.0).contains(&config.remote_cluster_max_cosine_distance)
        || !valid_ratio(config.minimum_word_label_coverage)
        || !valid_ratio(config.multiple_overlap_coverage)
    {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidConfiguration,
            "Diarization similarity and coverage thresholds are outside their valid ranges.",
        ));
    }

    Ok(())
}

fn validate_manifest(manifest: &MeetingCaptureManifest) -> Result<(), DiarizationError> {
    if manifest.version != CAPTURE_MANIFEST_VERSION {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidManifest,
            format!(
                "Unsupported capture manifest version {}; expected {}.",
                manifest.version, CAPTURE_MANIFEST_VERSION
            ),
        ));
    }

    if manifest.session_started_at_unix_ms <= 0 {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidManifest,
            "Capture manifest session_started_at_unix_ms must be positive.",
        ));
    }

    validate_track_manifest(&manifest.microphone, CaptureSource::Microphone)?;
    validate_track_manifest(&manifest.system, CaptureSource::System)?;

    if let Some(mix) = &manifest.mix {
        validate_track_manifest(mix, CaptureSource::Mix)?;
    }

    Ok(())
}

fn validate_track_manifest(
    track: &CaptureTrackManifest,
    expected_source: CaptureSource,
) -> Result<(), DiarizationError> {
    if track.source != expected_source {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidManifest,
            format!(
                "Capture manifest expected a {:?} track but received {:?}.",
                expected_source, track.source
            ),
        ));
    }

    let has_native_format = track.sample_rate_hz > 0 && track.channels > 0;
    let has_no_native_format = track.sample_rate_hz == 0 && track.channels == 0;
    if !has_native_format && !has_no_native_format {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidManifest,
            format!("{:?} track has an invalid source format.", expected_source),
        ));
    }

    // A backend can be unavailable before it provides a single native frame.
    // In that case an empty derived ASR WAV may still exist for a stable file
    // layout, but its 16 kHz format must not be mistaken for the source
    // device's unknown format. A track declared complete, however, must have
    // a real source format.
    if !has_native_format && track.status == CaptureTrackStatus::Complete {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidManifest,
            format!(
                "{:?} track is complete but does not declare a native source format.",
                expected_source
            ),
        ));
    }

    if track.asr_sample_rate_hz == Some(0) || track.asr_channels == Some(0) {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidManifest,
            format!("{:?} track has an invalid ASR format.", expected_source),
        ));
    }

    if track.backend_name.trim().is_empty() {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidManifest,
            format!(
                "{:?} track is missing its capture backend name.",
                expected_source
            ),
        ));
    }

    if track.started_at_unix_ms <= 0 {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidManifest,
            format!(
                "{:?} track has an invalid start timestamp.",
                expected_source
            ),
        ));
    }

    Ok(())
}

fn has_complete_source_tracks(manifest: &MeetingCaptureManifest) -> bool {
    [&manifest.microphone, &manifest.system]
        .iter()
        .all(|track| {
            track.status == CaptureTrackStatus::Complete
                && !matches!(
                    track.backend_status,
                    CaptureBackendStatus::Unavailable | CaptureBackendStatus::Failed
                )
        })
}

fn validate_speech_turns(turns: &[SpeechTurn]) -> Result<(), DiarizationError> {
    for (index, turn) in turns.iter().enumerate() {
        if !matches!(
            turn.source,
            CaptureSource::Microphone | CaptureSource::System
        ) {
            return Err(DiarizationError::new(
                DiarizationErrorCode::InvalidSpeechTurn,
                format!("Speech turn {index} cannot use the derived mix track."),
            ));
        }

        validate_span(
            TimeSpan {
                start_ms: turn.start_ms,
                end_ms: turn.end_ms,
            },
            DiarizationErrorCode::InvalidSpeechTurn,
            &format!("Speech turn {index}"),
        )?;

        if let Some(confidence) = turn.confidence {
            validate_confidence(
                confidence,
                DiarizationErrorCode::InvalidSpeechTurn,
                &format!("Speech turn {index}"),
            )?;
        }
    }

    Ok(())
}

fn normalize_embeddings(
    windows: &[SpeakerEmbeddingWindow],
    config: &DiarizationConfig,
) -> Result<Vec<NormalizedEmbedding>, DiarizationError> {
    if windows.len() > config.maximum_embedding_windows {
        return Err(DiarizationError::new(
            DiarizationErrorCode::InvalidEmbedding,
            format!(
                "Received {} embedding windows, which exceeds the configured limit of {}.",
                windows.len(),
                config.maximum_embedding_windows
            ),
        ));
    }

    let mut dimension = None;
    let mut normalized = Vec::with_capacity(windows.len());
    for (index, window) in windows.iter().enumerate() {
        validate_span(
            TimeSpan {
                start_ms: window.start_ms,
                end_ms: window.end_ms,
            },
            DiarizationErrorCode::InvalidEmbedding,
            &format!("Embedding window {index}"),
        )?;

        if window.embedding.is_empty() || !window.embedding.iter().all(|value| value.is_finite()) {
            return Err(DiarizationError::new(
                DiarizationErrorCode::InvalidEmbedding,
                format!("Embedding window {index} does not contain a finite vector."),
            ));
        }

        if let Some(expected_dimension) = dimension {
            if window.embedding.len() != expected_dimension {
                return Err(DiarizationError::new(
                    DiarizationErrorCode::InvalidEmbedding,
                    format!(
                        "Embedding window {index} has dimension {}; expected {expected_dimension}.",
                        window.embedding.len()
                    ),
                ));
            }
        } else {
            dimension = Some(window.embedding.len());
        }

        if let Some(confidence) = window.confidence {
            validate_confidence(
                confidence,
                DiarizationErrorCode::InvalidEmbedding,
                &format!("Embedding window {index}"),
            )?;
        }

        let Some(values) = normalized_vector(&window.embedding) else {
            return Err(DiarizationError::new(
                DiarizationErrorCode::InvalidEmbedding,
                format!("Embedding window {index} has a zero vector."),
            ));
        };

        normalized.push(NormalizedEmbedding {
            span: TimeSpan {
                start_ms: window.start_ms,
                end_ms: window.end_ms,
            },
            values,
            original_index: index,
        });
    }

    normalized.sort_by(|left, right| {
        left.span
            .start_ms
            .cmp(&right.span.start_ms)
            .then_with(|| left.span.end_ms.cmp(&right.span.end_ms))
            .then_with(|| left.original_index.cmp(&right.original_index))
    });
    Ok(normalized)
}

fn normalize_words(words: &[TranscriptWord]) -> Result<Vec<TranscriptWord>, DiarizationError> {
    let mut normalized = Vec::with_capacity(words.len());
    for (index, word) in words.iter().enumerate() {
        validate_span(
            TimeSpan {
                start_ms: word.start_ms,
                end_ms: word.end_ms,
            },
            DiarizationErrorCode::InvalidTranscriptWord,
            &format!("Transcript word {index}"),
        )?;

        if let Some(confidence) = word.confidence {
            validate_confidence(
                confidence,
                DiarizationErrorCode::InvalidTranscriptWord,
                &format!("Transcript word {index}"),
            )?;
        }

        if !word.text.trim().is_empty() {
            normalized.push(word.clone());
        }
    }

    normalized.sort_by(|left, right| {
        left.start_ms
            .cmp(&right.start_ms)
            .then_with(|| left.end_ms.cmp(&right.end_ms))
            .then_with(|| left.text.cmp(&right.text))
    });
    Ok(normalized)
}

fn validate_span(
    span: TimeSpan,
    code: DiarizationErrorCode,
    name: &str,
) -> Result<(), DiarizationError> {
    if span.start_ms < 0 || span.end_ms <= span.start_ms {
        return Err(DiarizationError::new(
            code,
            format!("{name} has an invalid meeting-clock range."),
        ));
    }
    Ok(())
}

fn validate_confidence(
    confidence: f32,
    code: DiarizationErrorCode,
    name: &str,
) -> Result<(), DiarizationError> {
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(DiarizationError::new(
            code,
            format!("{name} has an invalid confidence value."),
        ));
    }
    Ok(())
}

fn normalized_speech_spans(
    turns: &[SpeechTurn],
    source: CaptureSource,
    minimum_turn_ms: i64,
    merge_gap_ms: i64,
) -> Vec<TimeSpan> {
    let mut spans = turns
        .iter()
        .filter(|turn| turn.source == source)
        .map(|turn| TimeSpan {
            start_ms: turn.start_ms,
            end_ms: turn.end_ms,
        })
        .filter(|span| span.duration_ms() >= minimum_turn_ms)
        .collect::<Vec<_>>();
    spans.sort_by_key(|span| (span.start_ms, span.end_ms));

    let mut merged = Vec::with_capacity(spans.len());
    for span in spans {
        let Some(current) = merged.last_mut() else {
            merged.push(span);
            continue;
        };

        if span.start_ms <= current.end_ms.saturating_add(merge_gap_ms) {
            current.end_ms = current.end_ms.max(span.end_ms);
        } else {
            merged.push(span);
        }
    }
    merged
}

fn cluster_system_embeddings(
    embeddings: &[NormalizedEmbedding],
    system_turns: &[TimeSpan],
    config: &DiarizationConfig,
) -> (Vec<LabelledEmbedding>, usize) {
    let eligible = embeddings
        .iter()
        .filter(|embedding| {
            system_turns
                .iter()
                .any(|turn| turn.overlaps(embedding.span))
        })
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        return (Vec::new(), 0);
    }

    let mut clusters = Vec::<RemoteCluster>::new();
    let mut assignments = Vec::with_capacity(eligible.len());
    for &embedding in &eligible {
        let nearest_cluster = clusters
            .iter()
            .enumerate()
            .map(|(index, cluster)| (index, cosine_distance(&cluster.centroid, &embedding.values)))
            .min_by(
                |(left_index, left_distance), (right_index, right_distance)| {
                    left_distance
                        .partial_cmp(right_distance)
                        .unwrap_or(Ordering::Equal)
                        .then_with(|| left_index.cmp(right_index))
                },
            );

        let cluster_index = match nearest_cluster {
            Some((index, distance)) if distance <= config.remote_cluster_max_cosine_distance => {
                let cluster = &mut clusters[index];
                cluster.centroid =
                    updated_centroid(&cluster.centroid, cluster.observations, &embedding.values);
                cluster.observations += 1;
                index
            }
            _ => {
                let index = clusters.len();
                clusters.push(RemoteCluster {
                    centroid: embedding.values.clone(),
                    observations: 1,
                    earliest_start_ms: embedding.span.start_ms,
                    first_original_index: embedding.original_index,
                });
                index
            }
        };
        assignments.push((embedding, cluster_index));
    }

    let mut cluster_order = (0..clusters.len()).collect::<Vec<_>>();
    cluster_order.sort_by(|left, right| {
        clusters[*left]
            .earliest_start_ms
            .cmp(&clusters[*right].earliest_start_ms)
            .then_with(|| {
                clusters[*left]
                    .first_original_index
                    .cmp(&clusters[*right].first_original_index)
            })
            .then_with(|| left.cmp(right))
    });
    let mut speaker_indices = vec![0_u32; clusters.len()];
    for (position, cluster_index) in cluster_order.into_iter().enumerate() {
        speaker_indices[cluster_index] = (position + 1) as u32;
    }

    let labelled = assignments
        .into_iter()
        .map(|(embedding, cluster_index)| LabelledEmbedding {
            span: embedding.span,
            label: SpeakerLabel::RemoteSpeaker {
                index: speaker_indices[cluster_index],
            },
        })
        .collect();
    (labelled, clusters.len())
}

fn normalized_vector(values: &[f32]) -> Option<Vec<f32>> {
    let squared_norm = values.iter().map(|value| value * value).sum::<f32>();
    if !squared_norm.is_finite() || squared_norm <= MIN_VECTOR_NORM {
        return None;
    }

    let norm = squared_norm.sqrt();
    Some(values.iter().map(|value| value / norm).collect())
}

fn cosine_distance(left: &[f32], right: &[f32]) -> f32 {
    let similarity = left
        .iter()
        .zip(right)
        .map(|(left_value, right_value)| left_value * right_value)
        .sum::<f32>()
        .clamp(-1.0, 1.0);
    1.0 - similarity
}

fn updated_centroid(current: &[f32], observations: usize, next: &[f32]) -> Vec<f32> {
    let weight = observations as f32;
    let values = current
        .iter()
        .zip(next)
        .map(|(current_value, next_value)| current_value * weight + next_value)
        .collect::<Vec<_>>();
    normalized_vector(&values).unwrap_or_else(|| next.to_vec())
}

fn build_timeline(
    microphone_turns: &[TimeSpan],
    system_turns: &[TimeSpan],
    labelled_embeddings: &[LabelledEmbedding],
) -> Vec<DiarizationTurn> {
    let mut boundaries = microphone_turns
        .iter()
        .chain(system_turns)
        .flat_map(|span| [span.start_ms, span.end_ms])
        .collect::<Vec<_>>();
    boundaries.extend(
        labelled_embeddings
            .iter()
            .flat_map(|embedding| [embedding.span.start_ms, embedding.span.end_ms]),
    );
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut timeline = Vec::<DiarizationTurn>::new();
    for window in boundaries.windows(2) {
        let span = TimeSpan {
            start_ms: window[0],
            end_ms: window[1],
        };
        if span.duration_ms() <= 0 {
            continue;
        }

        let microphone_active = microphone_turns.iter().any(|turn| turn.overlaps(span));
        let system_active = system_turns.iter().any(|turn| turn.overlaps(span));
        let Some((label, sources)) =
            label_timeline_span(span, microphone_active, system_active, labelled_embeddings)
        else {
            continue;
        };

        if let Some(previous) = timeline.last_mut() {
            if previous.end_ms == span.start_ms
                && previous.label == label
                && previous.sources == sources
            {
                previous.end_ms = span.end_ms;
                continue;
            }
        }

        timeline.push(DiarizationTurn {
            start_ms: span.start_ms,
            end_ms: span.end_ms,
            label,
            sources,
        });
    }
    timeline
}

fn label_timeline_span(
    span: TimeSpan,
    microphone_active: bool,
    system_active: bool,
    labelled_embeddings: &[LabelledEmbedding],
) -> Option<(SpeakerLabel, Vec<CaptureSource>)> {
    match (microphone_active, system_active) {
        (false, false) => None,
        (true, false) => Some((SpeakerLabel::LocalUser, vec![CaptureSource::Microphone])),
        (false, true) => {
            let mut labels = labelled_embeddings
                .iter()
                .filter(|embedding| embedding.span.overlaps(span))
                .map(|embedding| embedding.label)
                .collect::<Vec<_>>();
            labels.sort_unstable();
            labels.dedup();
            let label = match labels.as_slice() {
                [] => SpeakerLabel::RemoteUnattributed,
                [label] => *label,
                _ => SpeakerLabel::Multiple,
            };
            Some((label, vec![CaptureSource::System]))
        }
        (true, true) => Some((
            SpeakerLabel::Multiple,
            vec![CaptureSource::Microphone, CaptureSource::System],
        )),
    }
}

fn speaker_segments_from_words(
    words: &[TranscriptWord],
    timeline: &[DiarizationTurn],
    config: &DiarizationConfig,
) -> Vec<SpeakerSegment> {
    let mut segments = Vec::<SpeakerSegment>::new();
    for word in words {
        let (label, coverage) = label_for_word(word, timeline, config);
        if let Some(previous) = segments.last_mut() {
            if previous.label == label
                && word.start_ms
                    <= previous
                        .end_ms
                        .saturating_add(config.word_segment_merge_gap_ms)
            {
                append_word(&mut previous.text, &word.text);
                previous.end_ms = previous.end_ms.max(word.end_ms);
                previous.label_coverage = previous.label_coverage.min(coverage);
                continue;
            }
        }

        segments.push(SpeakerSegment {
            start_ms: word.start_ms,
            end_ms: word.end_ms,
            label,
            label_coverage: coverage,
            text: word.text.clone(),
        });
    }
    segments
}

fn label_for_word(
    word: &TranscriptWord,
    timeline: &[DiarizationTurn],
    config: &DiarizationConfig,
) -> (SpeakerLabel, f32) {
    let word_span = TimeSpan {
        start_ms: word.start_ms,
        end_ms: word.end_ms,
    };
    let word_duration = word_span.duration_ms() as f32;
    let mut coverage_by_label = Vec::<(SpeakerLabel, i64)>::new();
    for turn in timeline {
        let overlap = word_span.overlap_ms(TimeSpan {
            start_ms: turn.start_ms,
            end_ms: turn.end_ms,
        });
        if overlap == 0 {
            continue;
        }

        if let Some((_, covered_ms)) = coverage_by_label
            .iter_mut()
            .find(|(label, _)| *label == turn.label)
        {
            *covered_ms += overlap;
        } else {
            coverage_by_label.push((turn.label, overlap));
        }
    }

    if coverage_by_label.is_empty() {
        return (SpeakerLabel::Unknown, 0.0);
    }

    let multiple_coverage = coverage_by_label
        .iter()
        .find(|(label, _)| *label == SpeakerLabel::Multiple)
        .map_or(0, |(_, covered_ms)| *covered_ms) as f32
        / word_duration;
    if multiple_coverage > 0.0 && multiple_coverage >= config.multiple_overlap_coverage {
        return (SpeakerLabel::Multiple, multiple_coverage);
    }

    coverage_by_label.sort_by(|(left_label, left_ms), (right_label, right_ms)| {
        right_ms
            .cmp(left_ms)
            .then_with(|| left_label.cmp(right_label))
    });
    let (dominant_label, dominant_ms) = coverage_by_label[0];
    let dominant_coverage = dominant_ms as f32 / word_duration;
    if dominant_coverage >= config.minimum_word_label_coverage {
        return (dominant_label, dominant_coverage);
    }

    let total_coverage = coverage_by_label
        .iter()
        .map(|(_, covered_ms)| *covered_ms)
        .sum::<i64>() as f32
        / word_duration;
    if total_coverage >= config.minimum_word_label_coverage {
        (SpeakerLabel::Multiple, total_coverage)
    } else {
        (SpeakerLabel::Unknown, total_coverage)
    }
}

fn append_word(target: &mut String, word: &str) {
    let punctuation = word.chars().next().is_some_and(|character| {
        matches!(
            character,
            ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
        )
    });
    if !target.is_empty() && !punctuation {
        target.push(' ');
    }
    target.push_str(word);
}

fn base_diagnostics(
    manifest: &MeetingCaptureManifest,
    reason: Option<String>,
) -> DiarizationDiagnostics {
    DiarizationDiagnostics {
        microphone_speech_turns: 0,
        system_speech_turns: 0,
        embedding_windows: 0,
        remote_speaker_count: 0,
        microphone_dropped_frames: manifest.microphone.dropped_frames,
        system_dropped_frames: manifest.system.dropped_frames,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(source: CaptureSource) -> CaptureTrackManifest {
        CaptureTrackManifest {
            source,
            source_path: Some(format!("{source:?}.wav")),
            asr_path: Some(format!("{source:?}-asr.wav")),
            sample_rate_hz: 48_000,
            channels: 2,
            asr_sample_rate_hz: Some(16_000),
            asr_channels: Some(1),
            device_name: Some(format!("{source:?} device")),
            backend_name: "test_backend".to_string(),
            backend_status: CaptureBackendStatus::Stopped,
            started_at_unix_ms: 1_700_000_000_000,
            clock_offset_ms: 0,
            dropped_frames: 0,
            status: CaptureTrackStatus::Complete,
        }
    }

    fn manifest() -> MeetingCaptureManifest {
        MeetingCaptureManifest {
            version: CAPTURE_MANIFEST_VERSION,
            session_started_at_unix_ms: 1_700_000_000_000,
            microphone: track(CaptureSource::Microphone),
            system: track(CaptureSource::System),
            mix: Some(track(CaptureSource::Mix)),
        }
    }

    fn enabled_config() -> DiarizationConfig {
        DiarizationConfig {
            enabled: true,
            ..DiarizationConfig::default()
        }
    }

    fn speech(source: CaptureSource, start_ms: i64, end_ms: i64) -> SpeechTurn {
        SpeechTurn {
            source,
            start_ms,
            end_ms,
            confidence: Some(0.95),
        }
    }

    fn word(text: &str, start_ms: i64, end_ms: i64) -> TranscriptWord {
        TranscriptWord {
            text: text.to_string(),
            start_ms,
            end_ms,
            confidence: Some(0.95),
        }
    }

    fn embedding(start_ms: i64, end_ms: i64, values: &[f32]) -> SpeakerEmbeddingWindow {
        SpeakerEmbeddingWindow {
            start_ms,
            end_ms,
            embedding: values.to_vec(),
            confidence: Some(0.95),
        }
    }

    #[test]
    fn diarize_clusters_remote_speakers_by_first_observed_speech() {
        let input = DiarizationInput {
            manifest: manifest(),
            speech_turns: vec![speech(CaptureSource::System, 0, 4_000)],
            system_embeddings: vec![
                embedding(0, 900, &[1.0, 0.0]),
                embedding(1_000, 1_900, &[0.99, 0.01]),
                embedding(2_000, 2_900, &[0.0, 1.0]),
                embedding(3_000, 3_900, &[0.01, 0.99]),
            ],
            transcript_words: vec![],
        };

        let result = diarize(&input, &enabled_config()).expect("valid deterministic input");

        assert_eq!(result.status, DiarizationStatus::Complete);
        assert_eq!(result.diagnostics.remote_speaker_count, 2);
        assert_eq!(
            result.turns[0].label,
            SpeakerLabel::RemoteSpeaker { index: 1 }
        );
        assert!(
            result
                .turns
                .iter()
                .any(|turn| turn.label == SpeakerLabel::RemoteSpeaker { index: 2 }),
            "the later remote cluster should retain the second stable speaker index"
        );
    }

    #[test]
    fn diarize_marks_simultaneous_microphone_and_system_speech_as_multiple() {
        let input = DiarizationInput {
            manifest: manifest(),
            speech_turns: vec![
                speech(CaptureSource::Microphone, 0, 1_000),
                speech(CaptureSource::System, 500, 1_500),
            ],
            system_embeddings: vec![],
            transcript_words: vec![
                word("local", 0, 400),
                word("overlap", 600, 900),
                word("remote", 1_100, 1_300),
                word("silence", 1_600, 1_800),
            ],
        };

        let result = diarize(&input, &enabled_config()).expect("valid source attribution input");

        assert_eq!(result.status, DiarizationStatus::SourceAttributionOnly);
        assert_eq!(
            result
                .segments
                .iter()
                .find(|segment| segment.text == "overlap")
                .expect("overlap transcript segment")
                .label,
            SpeakerLabel::Multiple
        );
        assert_eq!(
            result
                .segments
                .iter()
                .find(|segment| segment.text == "silence")
                .expect("uncovered transcript segment")
                .label,
            SpeakerLabel::Unknown
        );
    }

    #[test]
    fn diarize_uses_source_attribution_when_remote_embeddings_are_unavailable() {
        let input = DiarizationInput {
            manifest: manifest(),
            speech_turns: vec![
                speech(CaptureSource::Microphone, 0, 900),
                speech(CaptureSource::System, 1_000, 1_900),
            ],
            system_embeddings: vec![],
            transcript_words: vec![word("me", 100, 600), word("them", 1_100, 1_500)],
        };

        let result = diarize(&input, &enabled_config()).expect("valid source-only input");

        assert_eq!(result.status, DiarizationStatus::SourceAttributionOnly);
        assert_eq!(result.diagnostics.remote_speaker_count, 0);
        assert_eq!(result.segments[0].label, SpeakerLabel::LocalUser);
        assert_eq!(result.segments[1].label, SpeakerLabel::RemoteUnattributed);
    }

    #[test]
    fn diarize_rejects_inconsistent_embedding_dimensions() {
        let input = DiarizationInput {
            manifest: manifest(),
            speech_turns: vec![speech(CaptureSource::System, 0, 2_000)],
            system_embeddings: vec![
                embedding(0, 900, &[1.0, 0.0]),
                embedding(1_000, 1_900, &[0.0, 1.0, 0.0]),
            ],
            transcript_words: vec![],
        };

        let error =
            diarize(&input, &enabled_config()).expect_err("mismatched dimensions must fail");

        assert_eq!(error.code, DiarizationErrorCode::InvalidEmbedding);
    }

    #[test]
    fn diarize_returns_disabled_without_processing_capture_input() {
        let mut input = DiarizationInput {
            manifest: manifest(),
            speech_turns: vec![],
            system_embeddings: vec![],
            transcript_words: vec![],
        };
        input.manifest.version = 99;

        let result = diarize(&input, &DiarizationConfig::default())
            .expect("disabled feature should not process model input");

        assert_eq!(result.status, DiarizationStatus::Disabled);
    }

    #[test]
    fn diarize_returns_unavailable_for_partial_source_capture() {
        let mut input = DiarizationInput {
            manifest: manifest(),
            speech_turns: vec![speech(CaptureSource::Microphone, 0, 1_000)],
            system_embeddings: vec![],
            transcript_words: vec![word("unlabelled", 100, 900)],
        };
        input.manifest.system.status = CaptureTrackStatus::Partial;

        let result =
            diarize(&input, &enabled_config()).expect("partial capture is an expected state");

        assert_eq!(result.status, DiarizationStatus::Unavailable);
        assert_eq!(result.segments[0].label, SpeakerLabel::Unknown);
    }
}
