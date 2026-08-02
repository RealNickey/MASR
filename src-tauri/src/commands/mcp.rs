use crate::managers::mcp_server::{McpConnectionInfo, McpServerManager, McpServerStatus};
use std::sync::Arc;
use tauri::State;

#[tauri::command]
#[specta::specta]
pub fn get_mcp_server_status(server: State<'_, Arc<McpServerManager>>) -> McpServerStatus {
    server.status()
}

/// Returns the local endpoint and bearer token only when explicitly requested
/// by the settings UI. Ordinary AppSettings responses always redact it.
#[tauri::command]
#[specta::specta]
pub fn get_mcp_connection_info(server: State<'_, Arc<McpServerManager>>) -> McpConnectionInfo {
    server.connection_info()
}

#[tauri::command]
#[specta::specta]
pub fn rotate_mcp_token(server: State<'_, Arc<McpServerManager>>) -> McpConnectionInfo {
    server.rotate_token()
}
