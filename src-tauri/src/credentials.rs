//! Desktop credential-vault access for user-supplied LLM keys.
//!
//! Portable installs intentionally keep the legacy settings-file behavior so
//! moving the portable Data directory does not make credentials disappear.

use crate::settings::{write_settings, AppSettings};
use tauri::AppHandle;

const SERVICE: &str = "com.thegai.app.llm";

fn entry(provider_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, provider_id)
        .map_err(|error| format!("System credential vault is unavailable: {error}"))
}

pub fn get(provider_id: &str) -> Option<String> {
    if crate::portable::is_portable() {
        return None;
    }

    entry(provider_id)
        .ok()
        .and_then(|entry| entry.get_password().ok())
        .filter(|value| !value.trim().is_empty())
}

pub fn set(provider_id: &str, value: &str) -> Result<(), String> {
    entry(provider_id)?
        .set_password(value)
        .map_err(|error| format!("Could not save API key in the system credential vault: {error}"))
}

pub fn delete(provider_id: &str) -> Result<(), String> {
    let entry = entry(provider_id)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!(
            "Could not remove API key from the system credential vault: {error}"
        )),
    }
}

/// Move plaintext keys only after the vault confirms it can be read back.
/// A failure leaves the legacy value intact, preserving existing behavior.
pub fn migrate_legacy_api_keys(app: &AppHandle, settings: &mut AppSettings) {
    if crate::portable::is_portable() {
        return;
    }

    let mut changed = false;
    for (provider_id, legacy_key) in settings.post_process_api_keys.clone() {
        if legacy_key.trim().is_empty() || get(&provider_id).is_some() {
            continue;
        }
        if set(&provider_id, &legacy_key).is_ok()
            && get(&provider_id).as_deref() == Some(legacy_key.as_str())
        {
            settings
                .post_process_api_keys
                .insert(provider_id, String::new());
            changed = true;
        }
    }
    if changed {
        write_settings(app, settings.clone());
    }
}
