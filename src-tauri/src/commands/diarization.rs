use crate::managers::diarization_model::{DiarizationModelManager, DiarizationModelStatus};
use std::sync::Arc;
use tauri::Manager;

#[tauri::command]
#[specta::specta]
pub async fn get_diarization_model_status(
    app: tauri::AppHandle,
) -> Result<DiarizationModelStatus, String> {
    let manager = app
        .try_state::<Arc<DiarizationModelManager>>()
        .ok_or_else(|| "Diarization model manager is unavailable".to_string())?;
    let manager = Arc::clone(manager.inner());
    tokio::task::spawn_blocking(move || manager.status())
        .await
        .map_err(|error| format!("Diarization status worker panicked: {error}"))
}

#[tauri::command]
#[specta::specta]
pub async fn download_diarization_model(app: tauri::AppHandle) -> Result<(), String> {
    let manager = app
        .try_state::<Arc<DiarizationModelManager>>()
        .ok_or_else(|| "Diarization model manager is unavailable".to_string())?;
    manager.download().await.map_err(|error| error.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn cancel_diarization_model_download(app: tauri::AppHandle) {
    if let Some(manager) = app.try_state::<Arc<DiarizationModelManager>>() {
        manager.cancel_download();
    }
}
