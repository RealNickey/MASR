# Meeting Capture and Opt-in Diarization Plan

## Summary

Replace the meeting-only microphone/VAD buffer path with a continuous,
timestamped, dual-source capture coordinator. It will preserve recoverable
high-quality source chunks, derive 16 kHz ASR tracks and a compatibility mix,
and keep diarization an independently downloadable, opt-in local feature.

## Verified context

- `MeetingAction` currently calls `AudioRecordingManager::try_start_recording`
  and `stop_recording`, receives one VAD-filtered 16 kHz `Vec<f32>`, saves one
  WAV, and transcribes it. It does not construct `MeetingCaptureSession`.
- `MeetingCaptureSession` is an unused 16 kHz/16-bit writer. Its
  `append_aligned` method pads to `max(microphone.len(), system.len())`, which
  cannot represent device start offsets, discontinuities, or drift.
- `AudioTracks` holds only three paths; the history migration, retry, deletion,
  and retention paths know nothing about a session root, capture manifest, or
  speaker-labelled segments. Retention currently derives a root from
  `mix.parent()`, so a nested derived path would be unsafe/incomplete.
- ThegaV1 produces CTC logits but `MalayalamAsr` discards their timing and
  returns only text. Its `TranscriptionResult` currently has no segments.
- MASR already has Rust ONNX Runtime, Hound, Rubato, FFT support, and a Silero
  VAD resource. It does not have a native system-audio adapter or a diarization
  manager.

## System impact and source of truth

- The authoritative recording becomes a validated meeting session directory with
  `source/<source>/chunk-*.wav` native chunks, `timeline/<source>.ndjson`
  metadata, flat `microphone.wav`/`system.wav`/`mix.wav` derived outputs, and an
  atomically checkpointed `manifest.json`. The single
  `managers/meeting_capture_adapter.rs` module implements the platform adapters.
- Every incoming frame carries source, native stream format, monotonic meeting
  timestamp, sequence number, and discontinuity/drop information. The
  coordinator maps both device clocks to one meeting clock and explicitly
  inserts/measures gaps; it never aligns by vector length.
- `HistoryEntry` gains optional session/manifest and speaker-segment metadata.
  Existing recordings retain their current playback/transcript behavior.
- Diarization is isolated behind a persisted default-off setting and an
  auxiliary model asset. It cannot affect model auto-setup or become an ASR
  model selector.

## Recommended approach

1. Add a meeting-capture coordinator and platform adapter contract that owns
   continuous pre-VAD microphone and system frames. Implement native Windows
   WASAPI loopback, macOS ScreenCaptureKit/CoreAudio, and Linux
   PipeWire/PulseAudio adapters behind it, with a precise per-source status for
   unavailable hardware, permissions, errors, and incomplete tracks.
2. Rework `MeetingCaptureSession` into durable chunked persistence. Preserve
   native/high-quality PCM source chunks and derive 16 kHz mono ASR chunks.
   Checkpoint a schema-versioned manifest after each independently valid chunk;
   startup recovery retains and marks incomplete sessions instead of deleting
   evidence. Derive a gain-controlled, limited `mix.wav` only after source
   writes succeed.
3. Wire meeting start/stop/cancel through the coordinator, then finalize tracks
   before history persistence. ASR runs per source where available and merges on
   the meeting clock; the old mix-only path remains the fallback for old or
   degraded entries.
4. Extend ThegaV1 with structured CTC spans and a calibrated timing model,
   retaining text compatibility. Do not assume its output frame equals 10 ms;
   use fixture-backed stride/offset calibration and expose approximate timing
   when that calibration is unavailable.
5. Add a local diarization manager that runs source-aware VAD, assigns
   microphone speech to `You`, clusters only system-audio embeddings into
   meeting-local `Remote Speaker N`, and preserves `Unknown`/`Multiple` for
   weak/overlap cases. Join these turns to timed ASR segments. No participant
   names, cloud calls, or cross-meeting voice profiles.
6. Add migrations and update retry, cleanup, model delivery, generated
   bindings, settings UI, meeting views, copy behavior, and Playwright mocks
   together. A retry must reload retained source/ASR tracks and replace stale
   diarization metadata atomically.

## Planned files

- `src-tauri/src/managers/meeting_capture.rs` — timestamped frame mapping,
  high-quality/chunked source storage, derivation, manifest checkpoints,
  recovery, and unit coverage.
- `src-tauri/src/managers/meeting_capture_adapter.rs` (new) and
  `src-tauri/src/managers/audio.rs` — common adapter contract and native source
  capture lifecycle/status.
- `src-tauri/src/actions.rs` — route `MeetingAction` through the coordinator;
  finalize durable tracks before ASR/history and make cancellation safe.
- `src-tauri/src/managers/history.rs` — compatible session/manifest/speaker
  persistence, migration, retry, retention, and safe session-root cleanup.
- `src-tauri/src/malayalam_asr.rs` and
  `src-tauri/src/managers/transcription.rs` — structured ThegaV1 timing and
  multi-track transcript merge.
- `src-tauri/src/managers/diarization.rs` (new) — local VAD/embedding/clustering
  pipeline, diagnostics, and deterministic speaker-turn output.
- `src-tauri/src/managers/model.rs` — separately typed, optional diarization
  asset with pin/checksum/provenance; never part of first-run ASR setup.
- `src-tauri/src/settings.rs`, `src-tauri/src/lib.rs`, and generated
  `src/bindings.ts` — persisted toggle, commands, and binding regeneration.
- `src/stores/settingsStore.ts`,
  `src/components/settings/meetings/MeetingsSettings.tsx`, and
  `src/primary/MeetingsView.tsx` — opt-in status/settings and accessible
  labelled-transcript rendering with an unlabelled fallback.
- Rust/Playwright tests — timestamp drift/gap fixtures, chunks/recovery,
  migration/retry/retention, source status, CTC timing, diarization no-network
  behavior, and UI/copy compatibility.

## Verification

- Targeted Rust tests for clock offsets/drift/dropouts, manifest checkpoint and
  recovery, source/derived WAV headers, limiter behavior, nested session cleanup,
  history migration/retry, CTC timing, and clustering determinism.
- Adapter fakes exercised through `MeetingAction`, including a successful dual
  source capture and truthful one-source degradation.
- Binding-export test, frontend lint/build, and Playwright coverage for the new
  setting and meeting presentation.
- Per-platform manual validation: permission prompt, device/source identity,
  capture completeness, startup offset, long-session drift, crash recovery, and
  no network use after the optional model is installed.

## Decisions needed before shipping model/audio assets

1. The official WeSpeaker ResNet34 ONNX model has an Apache-2.0 repository card,
   while the publisher's pretrained-model guidance says VoxCeleb-derived models
   follow CC-BY-4.0. I recommend a pinned model revision with SHA-256 and the
   stricter attribution/notice set. Please approve that distribution policy, or
   name an already-approved alternative model.
2. I recommend native PCM WAV source chunks (max fidelity and recoverability),
   five-minute chunks, and applying the existing recording-retention setting to
   the complete session, including source audio. Please confirm those storage
   defaults or provide your preferred lossless format/chunk interval/retention
   policy.

## Platform release gate

All three adapters can be implemented behind the common contract, but this
Windows workspace cannot prove macOS and Linux system-audio capture on real
hardware. I will not claim cross-platform capture is production-verified until
those platform checks have run. Confirm whether the feature should wait for that
three-platform validation, or whether it may ship with platform status marked
unavailable until each native adapter is verified.
