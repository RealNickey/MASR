//! Lifecycle management for the optional local WeSpeaker embedding model.
//!
//! The diarization model is deliberately separate from the transcription-model
//! catalogue: it must never become an accidental ASR selection or affect the
//! first-run model download. The final file is published only after its pinned
//! SHA-256 digest has been verified.

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specta::Type;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

pub const WESPEAKER_MODEL_ID: &str = "wespeaker-voxceleb-resnet34";
const WESPEAKER_MODEL_FILENAME: &str = "wespeaker-voxceleb-resnet34.onnx";
const WESPEAKER_MODEL_URL: &str =
    "https://huggingface.co/Wespeaker/wespeaker-voxceleb-resnet34/resolve/main/voxceleb_resnet34.onnx?download=true";
const WESPEAKER_MODEL_SHA256: &str =
    "9fea6516d7ad6bf0a76c7689f5a49b65d330fad6dde96c91bb4435ffbfe056a1";

#[derive(Clone, Debug, Default)]
struct DownloadState {
    is_downloading: bool,
    downloaded: u64,
    total: u64,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct IntegrityCache {
    mtime_ns: i64,
    size: u64,
    is_valid: bool,
}

/// A frontend-safe snapshot. It intentionally exposes no local absolute path.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct DiarizationModelStatus {
    pub model_id: String,
    pub is_downloaded: bool,
    pub is_downloading: bool,
    pub downloaded: u64,
    pub total: u64,
    pub error: Option<String>,
}

/// Download and verification manager for the opt-in speaker embedding model.
pub struct DiarizationModelManager {
    app_handle: AppHandle,
    models_dir: PathBuf,
    state: Arc<Mutex<DownloadState>>,
    cancelled: Arc<AtomicBool>,
    integrity_cache: Arc<Mutex<Option<IntegrityCache>>>,
}

struct DownloadCleanup {
    state: Arc<Mutex<DownloadState>>,
    completed: bool,
}

enum DigestValidation {
    Valid,
    Mismatch { actual: String },
}

impl Drop for DownloadCleanup {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.state.lock().unwrap().is_downloading = false;
    }
}

impl DiarizationModelManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        let models_dir = crate::portable::app_data_dir(app_handle)
            .context("resolve application data directory for diarization")?
            .join("models");
        fs::create_dir_all(&models_dir)
            .with_context(|| format!("create diarization model directory {models_dir:?}"))?;

        Ok(Self {
            app_handle: app_handle.clone(),
            models_dir,
            state: Arc::new(Mutex::new(DownloadState::default())),
            cancelled: Arc::new(AtomicBool::new(false)),
            integrity_cache: Arc::new(Mutex::new(None)),
        })
    }

    pub fn status(&self) -> DiarizationModelStatus {
        // Never advertise an arbitrary file in the models directory as ready.
        // The final file is small enough to validate here, and doing so also
        // repairs an interrupted or externally-corrupted install before the UI
        // hides its retry affordance.
        let (is_downloaded, integrity_error) = match self.has_verified_final_file() {
            Ok(is_downloaded) => (is_downloaded, None),
            Err(error) => (false, Some(error.to_string())),
        };
        let mut state = self.state.lock().unwrap();
        if let Some(integrity_error) = integrity_error {
            state.error = Some(integrity_error);
        } else if is_downloaded && !state.is_downloading {
            state.error = None;
        }
        let state = state.clone();

        DiarizationModelStatus {
            model_id: WESPEAKER_MODEL_ID.to_string(),
            is_downloaded,
            is_downloading: state.is_downloading,
            downloaded: state.downloaded,
            total: state.total,
            error: state.error,
        }
    }

    /// Returns the verified-on-download model path when it is available.
    pub fn model_path(&self) -> Result<PathBuf> {
        let path = self.model_path_unchecked();
        if self.has_verified_final_file()? {
            Ok(path)
        } else {
            Err(anyhow!(
                "The optional speaker diarization model has not been downloaded"
            ))
        }
    }

    pub fn cancel_download(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Downloads with HTTP range-resume support and publishes atomically after
    /// checksum validation. Failed or cancelled transfers retain the partial
    /// file so the next explicit enable/retry can resume safely.
    pub async fn download(&self) -> Result<()> {
        if self.model_path_unchecked().is_file() {
            return self.complete_download(self.verify_final_file());
        }

        {
            let mut state = self.state.lock().unwrap();
            if state.is_downloading {
                return Ok(());
            }
            state.is_downloading = true;
            state.error = None;
            state.downloaded = self
                .partial_path()
                .metadata()
                .map(|meta| meta.len())
                .unwrap_or(0);
            state.total = 0;
        }
        self.cancelled.store(false, Ordering::SeqCst);
        let mut cleanup = DownloadCleanup {
            state: Arc::clone(&self.state),
            completed: false,
        };

        let result = self.complete_download(self.download_inner().await);
        cleanup.completed = true;
        result
    }

    async fn download_inner(&self) -> Result<()> {
        let partial_path = self.partial_path();
        let existing_bytes = partial_path.metadata().map(|meta| meta.len()).unwrap_or(0);
        let client = reqwest::Client::new();
        let mut request = client.get(WESPEAKER_MODEL_URL);
        if existing_bytes > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={existing_bytes}-"));
        }

        let response = request
            .send()
            .await
            .context("request optional speaker diarization model")?;
        let is_resuming = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && existing_bytes > 0 {
            Self::discard_invalid_file(&partial_path, "partial speaker diarization model")?;
            return Err(anyhow!(
                "Diarization model partial download could not be resumed and was discarded; retry to download a clean copy"
            ));
        }
        if !response.status().is_success() {
            return Err(anyhow!(
                "Diarization model download failed with HTTP {}",
                response.status()
            ));
        }

        let start_at = if is_resuming { existing_bytes } else { 0 };
        let content_length = response.content_length().unwrap_or(0);
        {
            let mut state = self.state.lock().unwrap();
            state.downloaded = start_at;
            state.total = start_at.saturating_add(content_length);
        }
        self.emit_status();

        let mut file = if is_resuming {
            OpenOptions::new()
                .append(true)
                .open(&partial_path)
                .with_context(|| format!("open partial diarization model {partial_path:?}"))?
        } else {
            File::create(&partial_path)
                .with_context(|| format!("create partial diarization model {partial_path:?}"))?
        };

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(anyhow!("Diarization model download cancelled"));
            }
            let chunk = chunk.context("read diarization model download body")?;
            file.write_all(&chunk)
                .context("write diarization model download chunk")?;
            let mut state = self.state.lock().unwrap();
            state.downloaded = state.downloaded.saturating_add(chunk.len() as u64);
            drop(state);
            self.emit_status();
        }
        file.sync_all()
            .context("sync diarization model partial file")?;
        drop(file);

        Self::verify_and_discard_checksum_mismatch(
            &partial_path,
            "partial speaker diarization model",
        )?;
        fs::rename(&partial_path, self.model_path_unchecked())
            .context("atomically publish verified diarization model")?;
        Ok(())
    }

    fn verify_final_file(&self) -> Result<()> {
        Self::verify_and_discard_checksum_mismatch(
            &self.model_path_unchecked(),
            "downloaded speaker diarization model",
        )
    }

    fn has_verified_final_file(&self) -> Result<bool> {
        let path = self.model_path_unchecked();
        if !path.is_file() {
            return Ok(false);
        }

        let metadata = path.metadata().context("read diarization model metadata")?;
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let size = metadata.len();

        let mut cache = self.integrity_cache.lock().unwrap();
        if let Some(cached) = cache.as_ref() {
            if cached.mtime_ns == mtime_ns && cached.size == size {
                return Ok(cached.is_valid);
            }
        }

        let is_valid = self.verify_final_file().is_ok();
        *cache = Some(IntegrityCache {
            mtime_ns,
            size,
            is_valid,
        });
        Ok(is_valid)
    }

    fn complete_download(&self, result: Result<()>) -> Result<()> {
        let completion = result.and_then(|()| {
            self.model_path_unchecked()
                .metadata()
                .map(|metadata| metadata.len())
                .context("read verified speaker diarization model metadata")
        });

        {
            let mut state = self.state.lock().unwrap();
            state.is_downloading = false;
            match &completion {
                Ok(downloaded) => {
                    state.error = None;
                    state.downloaded = *downloaded;
                    state.total = *downloaded;
                }
                Err(error) => state.error = Some(error.to_string()),
            }
        }
        // `emit_status` calls `status`, which locks `state`, so the guard must
        // be out of scope before emitting to avoid self-deadlocking completion.
        self.emit_status();

        completion.map(|_| ())
    }

    fn verify_and_discard_checksum_mismatch(path: &Path, file_kind: &str) -> Result<()> {
        match Self::validate_digest(path)? {
            DigestValidation::Valid => Ok(()),
            DigestValidation::Mismatch { actual } => {
                Self::discard_invalid_file(path, file_kind)?;
                Err(anyhow!(
                    "Diarization model checksum mismatch: expected {WESPEAKER_MODEL_SHA256}, got {actual}. The invalid {file_kind} was discarded; retry to download a clean copy"
                ))
            }
        }
    }

    fn validate_digest(path: &Path) -> Result<DigestValidation> {
        let mut file =
            File::open(path).with_context(|| format!("open diarization model {path:?}"))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .context("read diarization model for checksum")?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != WESPEAKER_MODEL_SHA256 {
            return Ok(DigestValidation::Mismatch { actual });
        }
        Ok(DigestValidation::Valid)
    }

    fn discard_invalid_file(path: &Path, file_kind: &str) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("discard invalid {file_kind} at {path:?}"))
            }
        }
    }

    fn model_path_unchecked(&self) -> PathBuf {
        self.models_dir.join(WESPEAKER_MODEL_FILENAME)
    }

    fn partial_path(&self) -> PathBuf {
        self.models_dir
            .join(format!("{WESPEAKER_MODEL_FILENAME}.partial"))
    }

    fn emit_status(&self) {
        let _ = self
            .app_handle
            .emit("diarization-model-status", self.status());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn pinned_wespeaker_checksum_has_expected_shape() {
        assert_eq!(WESPEAKER_MODEL_SHA256.len(), 64);
        assert!(WESPEAKER_MODEL_SHA256
            .chars()
            .all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn checksum_mismatch_discards_partial_for_a_clean_retry() {
        let temp_dir = tempdir().unwrap();
        let partial_path = temp_dir.path().join("speaker-model.onnx.partial");
        fs::write(&partial_path, b"corrupt speaker model").unwrap();

        let error = DiarizationModelManager::verify_and_discard_checksum_mismatch(
            &partial_path,
            "partial speaker diarization model",
        )
        .unwrap_err();

        assert!(
            !partial_path.exists(),
            "a failed digest check must discard the unrecoverable partial: {error}"
        );
    }
}
