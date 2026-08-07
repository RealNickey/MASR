pub mod audio;
pub mod diarization;
pub mod google;
pub mod history;
pub mod mcp;
pub mod models;
pub mod transcription;

use crate::settings::{get_settings, write_settings, AppSettings, LogLevel};
use crate::utils::cancel_current_operation;
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
#[specta::specta]
pub fn cancel_operation(app: AppHandle) {
    cancel_current_operation(&app);
}

#[tauri::command]
#[specta::specta]
pub fn is_portable() -> bool {
    crate::portable::is_portable()
}

#[tauri::command]
#[specta::specta]
pub fn get_app_dir_path(app: AppHandle) -> Result<String, String> {
    let app_data_dir = crate::portable::app_data_dir(&app)
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    Ok(app_data_dir.to_string_lossy().to_string())
}

#[tauri::command]
#[specta::specta]
pub fn get_app_settings(app: AppHandle) -> Result<AppSettings, String> {
    let mut settings = get_settings(&app);
    // Keys are read by the Rust backend only. The frontend receives the map's
    // provider shape but never a retrievable secret.
    for key in settings.post_process_api_keys.values_mut() {
        key.clear();
    }
    settings.mcp_server_token = None;
    Ok(settings)
}

#[tauri::command]
#[specta::specta]
pub fn get_default_settings() -> Result<AppSettings, String> {
    Ok(crate::settings::get_default_settings())
}

#[tauri::command]
#[specta::specta]
pub fn get_log_dir_path(app: AppHandle) -> Result<String, String> {
    let log_dir = crate::portable::app_log_dir(&app)
        .map_err(|e| format!("Failed to get log directory: {}", e))?;

    Ok(log_dir.to_string_lossy().to_string())
}

#[specta::specta]
#[tauri::command]
pub fn set_log_level(app: AppHandle, level: LogLevel) -> Result<(), String> {
    let tauri_log_level: tauri_plugin_log::LogLevel = level.into();
    let log_level: log::Level = tauri_log_level.into();
    // Update the file log level atomic so the filter picks up the new level
    crate::FILE_LOG_LEVEL.store(
        log_level.to_level_filter() as u8,
        std::sync::atomic::Ordering::Relaxed,
    );

    let mut settings = get_settings(&app);
    settings.log_level = level;
    write_settings(&app, settings);

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn open_recordings_folder(app: AppHandle) -> Result<(), String> {
    let app_data_dir = crate::portable::app_data_dir(&app)
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    let recordings_dir = app_data_dir.join("recordings");

    let path = recordings_dir.to_string_lossy().as_ref().to_string();
    app.opener()
        .open_path(path, None::<String>)
        .map_err(|e| format!("Failed to open recordings folder: {}", e))?;

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn open_log_dir(app: AppHandle) -> Result<(), String> {
    let log_dir = crate::portable::app_log_dir(&app)
        .map_err(|e| format!("Failed to get log directory: {}", e))?;

    let path = log_dir.to_string_lossy().as_ref().to_string();
    app.opener()
        .open_path(path, None::<String>)
        .map_err(|e| format!("Failed to open log directory: {}", e))?;

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn open_app_data_dir(app: AppHandle) -> Result<(), String> {
    let app_data_dir = crate::portable::app_data_dir(&app)
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    let path = app_data_dir.to_string_lossy().as_ref().to_string();
    app.opener()
        .open_path(path, None::<String>)
        .map_err(|e| format!("Failed to open app data directory: {}", e))?;

    Ok(())
}

/// Check if Apple Intelligence is available on this device.
/// Called by the frontend when the user selects Apple Intelligence provider.
#[specta::specta]
#[tauri::command]
pub fn check_apple_intelligence_available() -> bool {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        crate::apple_intelligence::check_apple_intelligence_availability()
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        false
    }
}

/// Try to initialize Enigo (keyboard/mouse simulation).
/// On macOS, this will return an error if accessibility permissions are not granted.
#[specta::specta]
#[tauri::command]
pub fn initialize_enigo(app: AppHandle) -> Result<(), String> {
    use crate::input::EnigoState;

    // Check if already initialized
    if app.try_state::<EnigoState>().is_some() {
        log::debug!("Enigo already initialized");
        return Ok(());
    }

    // Try to initialize
    match EnigoState::new() {
        Ok(enigo_state) => {
            app.manage(enigo_state);
            log::info!("Enigo initialized successfully after permission grant");
            Ok(())
        }
        Err(e) => {
            if cfg!(target_os = "macos") {
                log::warn!(
                    "Failed to initialize Enigo: {} (accessibility permissions may not be granted)",
                    e
                );
            } else {
                log::warn!("Failed to initialize Enigo: {}", e);
            }
            Err(format!("Failed to initialize input system: {}", e))
        }
    }
}

/// Marker state to track if shortcuts have been initialized.
pub struct ShortcutsInitialized;

/// Initialize keyboard shortcuts.
/// On macOS, this should be called after accessibility permissions are granted.
/// This is idempotent - calling it multiple times is safe.
#[specta::specta]
#[tauri::command]
pub fn initialize_shortcuts(app: AppHandle) -> Result<(), String> {
    // Check if already initialized
    if app.try_state::<ShortcutsInitialized>().is_some() {
        log::debug!("Shortcuts already initialized");
        return Ok(());
    }

    // Initialize shortcuts
    crate::shortcut::init_shortcuts(&app);

    // Mark as initialized
    app.manage(ShortcutsInitialized);

    log::info!("Shortcuts initialized successfully");
    Ok(())
}

/// Test a post-processing API key by attempting to fetch models
#[specta::specta]
#[tauri::command]
pub async fn test_post_process_api_key(
    app: AppHandle,
    provider_id: String,
    api_key: String,
) -> Result<bool, String> {
    let settings = get_settings(&app);
    let provider = settings
        .post_process_providers
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| format!("Provider '{}' not found", provider_id))?;

    // Apple Intelligence is always valid (local, no API key)
    if provider_id == "apple_intelligence" {
        return Ok(true);
    }

    let effective_key = if api_key.trim().is_empty() {
        crate::settings::resolved_post_process_api_key(&settings, &provider_id)
    } else {
        api_key
    };

    match crate::llm_client::fetch_models(provider, effective_key).await {
        Ok(_) => Ok(true),
        Err(e) => Err(e),
    }
}

#[derive(serde::Serialize, specta::Type)]
pub struct OllamaStatus {
    pub connected: bool,
    pub model_count: u32,
    pub error: Option<String>,
}

/// Check the status of the local Ollama instance
#[specta::specta]
#[tauri::command]
pub async fn check_ollama_status(app: AppHandle) -> Result<OllamaStatus, String> {
    let settings = get_settings(&app);
    let provider = settings
        .post_process_providers
        .iter()
        .find(|p| p.id == "ollama")
        .ok_or_else(|| "Ollama provider settings not found".to_string())?;

    let api_key = crate::settings::resolved_post_process_api_key(&settings, "ollama");
    match crate::llm_client::fetch_models(provider, api_key).await {
        Ok(models) => Ok(OllamaStatus {
            connected: true,
            model_count: models.len() as u32,
            error: None,
        }),
        Err(e) => Ok(OllamaStatus {
            connected: false,
            model_count: 0,
            error: Some(e),
        }),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn set_rag_enabled(
    app: AppHandle,
    rag_manager: State<'_, Arc<crate::managers::rag::RagManager>>,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = get_settings(&app);
    let previous = settings.rag_enabled;
    settings.rag_enabled = enabled;
    write_settings(&app, settings);

    if let Err(error) = rag_manager.enable_or_validate().await {
        let mut rollback = get_settings(&app);
        rollback.rag_enabled = previous;
        write_settings(&app, rollback);
        return Err(error.to_string());
    }

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn get_rag_status(
    rag_manager: State<'_, Arc<crate::managers::rag::RagManager>>,
) -> Result<crate::managers::rag::RagStatusSnapshot, String> {
    Ok(rag_manager.status().await)
}

#[tauri::command]
#[specta::specta]
pub async fn reindex_rag(
    rag_manager: State<'_, Arc<crate::managers::rag::RagManager>>,
) -> Result<(), String> {
    rag_manager
        .reindex()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn clear_rag_index(
    rag_manager: State<'_, Arc<crate::managers::rag::RagManager>>,
) -> Result<(), String> {
    rag_manager
        .clear_index()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn set_mcp_server_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.mcp_server_enabled = enabled;
    write_settings(&app, settings);
    if let Some(server) = app.try_state::<Arc<crate::managers::mcp_server::McpServerManager>>() {
        server.inner().sync_from_settings(&app);
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn set_mcp_server_port(app: AppHandle, port: u16) -> Result<(), String> {
    if !(1024..=65535).contains(&port) {
        return Err("MCP server port must be between 1024 and 65535".to_string());
    }
    let mut settings = get_settings(&app);
    settings.mcp_server_port = port;
    write_settings(&app, settings);
    if let Some(server) = app.try_state::<Arc<crate::managers::mcp_server::McpServerManager>>() {
        server.inner().sync_from_settings(&app);
    }
    Ok(())
}
