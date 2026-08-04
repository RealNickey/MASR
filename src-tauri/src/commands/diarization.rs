use crate::managers::diarization_model::{DiarizationModelManager, DiarizationModelStatus};
use std::sync::Arc;
use tauri::State;

#[tauri::command]
#[specta::specta]
pub fn get_diarization_model_status(
    manager: State<'_, Arc<DiarizationModelManager>>,
) -> DiarizationModelStatus {
    manager.status()
}

#[tauri::command]
#[specta::specta]
pub async fn download_diarization_model(
    manager: State<'_, Arc<DiarizationModelManager>>,
) -> Result<(), String> {
    manager.download().await.map_err(|error| error.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn cancel_diarization_model_download(manager: State<'_, Arc<DiarizationModelManager>>) {
    manager.cancel_download();
}
