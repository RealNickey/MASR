#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::apple_intelligence;
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::{is_microphone_access_denied, is_no_input_device_error};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::diarization::{
    diarize, CaptureSource, DiarizationConfig, DiarizationInput, DiarizationStatus,
    MeetingCaptureManifest, SpeakerLabel, TranscriptWord,
};
use crate::managers::diarization_inference::{
    detect_speech_turns_from_wav, DiarizationInference, DiarizationInferenceConfig,
};
use crate::managers::diarization_model::DiarizationModelManager;
use crate::managers::history::{
    HistoryEntry, HistoryManager, MeetingSession, SpeakerSegment, TranscriptSegment,
};
use crate::managers::meeting_capture::MeetingCaptureArtifacts;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{
    get_settings, AppSettings, OutputLanguage, PostProcessProvider, APPLE_INTELLIGENCE_PROVIDER_ID,
    DEFAULT_MEETING_SUMMARY_PROMPT,
};
use crate::shortcut;
use crate::tray::{change_tray_icon, TrayIconState};
use crate::utils::{
    self, show_processing_overlay, show_recording_overlay, show_transcribing_overlay,
};
use crate::TranscriptionCoordinator;
use anyhow::Context;
use ferrous_opencc::{config::BuiltinConfig, OpenCC};
use hound::{SampleFormat, WavReader};
use log::{debug, error, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tauri::Manager;
use tauri::{AppHandle, Emitter};

#[derive(Clone, serde::Serialize)]
struct RecordingErrorEvent {
    error_type: String,
    detail: Option<String>,
}

/// Drop guard that notifies the [`TranscriptionCoordinator`] when the
/// transcription pipeline finishes — whether it completes normally or panics.
struct FinishGuard(AppHandle);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        let _ = self.0.emit(
            "recording-state-changed",
            RecordingStatePayload {
                mode: "idle".to_string(),
            },
        );
        if let Some(c) = self.0.try_state::<TranscriptionCoordinator>() {
            c.notify_processing_finished();
        }
    }
}

// Shortcut Action Trait
pub trait ShortcutAction: Send + Sync {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
}

// Transcribe Action
struct TranscribeAction;

/// Field name for structured output JSON schema
const TRANSCRIPTION_FIELD: &str = "transcription";

/// Strip invisible Unicode characters that some LLMs may insert
fn strip_invisible_chars(s: &str) -> String {
    s.replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'], "")
}

#[derive(Clone, serde::Serialize)]
struct MeetingSummaryPayload {
    summary: String,
    transcript: String,
}

#[derive(Clone, serde::Serialize)]
struct RecordingStatePayload {
    mode: String, // "transcribe" | "meeting" | "idle"
}

/// Build a system prompt from the user's prompt template.
/// Removes `${output}` placeholder since the transcription is sent as the user message.
fn build_system_prompt(prompt_template: &str) -> String {
    prompt_template.replace("${output}", "").trim().to_string()
}

async fn post_process_transcription(
    app: &AppHandle,
    settings: &AppSettings,
    transcription: &str,
) -> Option<String> {
    let provider = match settings.active_post_process_provider().cloned() {
        Some(provider) => provider,
        None => {
            debug!("Post-processing enabled but no provider is selected");
            return None;
        }
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    if model.trim().is_empty() {
        debug!(
            "Post-processing skipped because provider '{}' has no model configured",
            provider.id
        );
        return None;
    }

    let selected_prompt_id = match &settings.post_process_selected_prompt_id {
        Some(id) => id.clone(),
        None => {
            debug!("Post-processing skipped because no prompt is selected");
            return None;
        }
    };

    let prompt = match settings
        .post_process_prompts
        .iter()
        .find(|prompt| prompt.id == selected_prompt_id)
    {
        Some(prompt) => prompt.prompt.clone(),
        None => {
            debug!(
                "Post-processing skipped because prompt '{}' was not found",
                selected_prompt_id
            );
            return None;
        }
    };

    if prompt.trim().is_empty() {
        debug!("Post-processing skipped because the selected prompt is empty");
        return None;
    }

    debug!(
        "Starting LLM post-processing with provider '{}' (model: {})",
        provider.id, model
    );

    let api_key = crate::settings::resolved_post_process_api_key(settings, &provider.id);
    let mut quota_consumed = false;

    // Disable reasoning for providers where post-processing rarely benefits from it.
    // - custom: top-level reasoning_effort (works for local OpenAI-compat servers)
    // - openrouter: nested reasoning object; exclude:true also keeps reasoning text
    //   out of the response so it can't pollute structured-output JSON parsing
    let (reasoning_effort, reasoning) = match provider.id.as_str() {
        "custom" | "google" | "ollama" => (Some("none".to_string()), None),
        "openrouter" => (
            None,
            Some(crate::llm_client::ReasoningConfig {
                effort: Some("none".to_string()),
                exclude: Some(true),
            }),
        ),
        _ => (None, None),
    };

    if provider.supports_structured_output {
        debug!("Using structured outputs for provider '{}'", provider.id);

        let system_prompt = build_system_prompt(&prompt);
        let user_content = transcription.to_string();

        // Handle Apple Intelligence separately since it uses native Swift APIs
        if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                if !apple_intelligence::check_apple_intelligence_availability() {
                    debug!(
                        "Apple Intelligence selected but not currently available on this device"
                    );
                    return None;
                }

                let token_limit = model.trim().parse::<i32>().unwrap_or(0);
                return match apple_intelligence::process_text_with_system_prompt(
                    &system_prompt,
                    &user_content,
                    token_limit,
                ) {
                    Ok(result) => {
                        if result.trim().is_empty() {
                            debug!("Apple Intelligence returned an empty response");
                            None
                        } else {
                            let result = strip_invisible_chars(&result);
                            debug!(
                                "Apple Intelligence post-processing succeeded. Output length: {} chars",
                                result.len()
                            );
                            Some(result)
                        }
                    }
                    Err(err) => {
                        error!("Apple Intelligence post-processing failed: {}", err);
                        None
                    }
                };
            }

            #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
            {
                debug!("Apple Intelligence provider selected on unsupported platform");
                return None;
            }
        }

        // Define JSON schema for transcription output
        let description = if selected_prompt_id == "default_meeting_summary"
            || selected_prompt_id == "default_meeting_notes_with_actions"
        {
            "The meeting summary and action items in English"
        } else if selected_prompt_id == "default_translate_to_english" {
            "The translated text in English"
        } else if selected_prompt_id == "default_manglish_transliteration" {
            "The transliterated text in Manglish"
        } else {
            "The cleaned and processed transcription text"
        };

        let json_schema = serde_json::json!({
            "type": "object",
            "properties": {
                (TRANSCRIPTION_FIELD): {
                    "type": "string",
                    "description": description
                }
            },
            "required": [TRANSCRIPTION_FIELD],
            "additionalProperties": false
        });

        if let Err(error) =
            consume_quota_if_needed(app, settings, &provider.id, &api_key, &mut quota_consumed)
        {
            error!("LLM quota check failed: {}", error);
            return None;
        }
        match crate::llm_client::send_chat_completion_with_schema(
            app,
            &provider,
            api_key.clone(),
            &model,
            user_content,
            Some(system_prompt),
            Some(json_schema),
            reasoning_effort.clone(),
            reasoning.clone(),
        )
        .await
        {
            Ok(Some(content)) => {
                // Parse the JSON response to extract the transcription field
                match serde_json::from_str::<serde_json::Value>(&content) {
                    Ok(json) => {
                        if let Some(transcription_value) =
                            json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str())
                        {
                            let result = strip_invisible_chars(transcription_value);
                            debug!(
                                "Structured output post-processing succeeded for provider '{}'. Output length: {} chars",
                                provider.id,
                                result.len()
                            );
                            return Some(result);
                        } else {
                            error!("Structured output response missing 'transcription' field");
                            return Some(strip_invisible_chars(&content));
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to parse structured output JSON: {}. Returning raw content.",
                            e
                        );
                        return Some(strip_invisible_chars(&content));
                    }
                }
            }
            Ok(None) => {
                error!("LLM API response has no content");
                return None;
            }
            Err(e) => {
                warn!(
                    "Structured output failed for provider '{}': {}. Falling back to legacy mode.",
                    provider.id, e
                );
                // Fall through to legacy mode below
            }
        }
    }

    // Legacy mode: Replace ${output} variable in the prompt with the actual text
    let processed_prompt = prompt.replace("${output}", transcription);
    debug!("Processed prompt length: {} chars", processed_prompt.len());

    if let Err(error) =
        consume_quota_if_needed(app, settings, &provider.id, &api_key, &mut quota_consumed)
    {
        error!("LLM quota check failed: {}", error);
        return None;
    }
    match crate::llm_client::send_chat_completion(
        app,
        &provider,
        api_key,
        &model,
        processed_prompt,
        reasoning_effort,
        reasoning,
    )
    .await
    {
        Ok(Some(content)) => {
            let content = strip_invisible_chars(&content);
            debug!(
                "LLM post-processing succeeded for provider '{}'. Output length: {} chars",
                provider.id,
                content.len()
            );
            Some(content)
        }
        Ok(None) => {
            error!("LLM API response has no content");
            None
        }
        Err(e) => {
            error!(
                "LLM post-processing failed for provider '{}': {}. Falling back to original transcription.",
                provider.id,
                e
            );
            None
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/obfuscated_keys.rs"));

fn resolve_google_api_key(
    obfuscated: Option<String>,
    env_google_api: Option<String>,
    env_google_api_key: Option<String>,
) -> String {
    obfuscated.unwrap_or_else(|| env_google_api.or(env_google_api_key).unwrap_or_default())
}

static GOOGLE_API_KEY: Lazy<String> =
    Lazy::new(|| OBFUSCATED_GOOGLE_API_KEY.clone().unwrap_or_default());
static GROQ_API_KEY: Lazy<String> =
    Lazy::new(|| OBFUSCATED_GROQ_API_KEY.clone().unwrap_or_default());
static OPENROUTER_API_KEY: Lazy<String> =
    Lazy::new(|| OBFUSCATED_OPENROUTER_API_KEY.clone().unwrap_or_default());
static GEMINI_API_KEY_1: Lazy<String> =
    Lazy::new(|| OBFUSCATED_GEMINI_API_KEY_1.clone().unwrap_or_default());
static GEMINI_API_KEY_2: Lazy<String> =
    Lazy::new(|| OBFUSCATED_GEMINI_API_KEY_2.clone().unwrap_or_default());

#[derive(Clone, serde::Serialize)]
struct FallbackEventPayload {
    failed_model: String,
    failed_provider: String,
    error: String,
    next_model: Option<String>,
    next_provider: Option<String>,
}

struct FallbackModel {
    provider_id: &'static str,
    model_name: &'static str,
}

const FALLBACK_CHAIN: &[FallbackModel] = &[
    // Gemini/Google
    FallbackModel {
        provider_id: "google",
        model_name: "gemini-3.5-flash",
    },
    FallbackModel {
        provider_id: "google",
        model_name: "gemini-3.1-flash-lite",
    },
    FallbackModel {
        provider_id: "google",
        model_name: "gemini-2.5-flash",
    },
    FallbackModel {
        provider_id: "google",
        model_name: "gemini-2.5-flash-lite",
    },
    FallbackModel {
        provider_id: "google",
        model_name: "gemma-4-31b-it",
    },
    FallbackModel {
        provider_id: "google",
        model_name: "gemma-4-26b-a4b-it",
    },
    // OpenRouter
    FallbackModel {
        provider_id: "openrouter",
        model_name: "nvidia/nemotron-3-ultra-550b-a55b:free",
    },
    FallbackModel {
        provider_id: "openrouter",
        model_name: "google/gemma-4-31b-it:free",
    },
    FallbackModel {
        provider_id: "openrouter",
        model_name: "google/gemma-4-26b-a4b-it:free",
    },
    // Groq
    FallbackModel {
        provider_id: "groq",
        model_name: "llama-3.3-70b-versatile",
    },
    FallbackModel {
        provider_id: "groq",
        model_name: "llama-3.1-8b-instant",
    },
];

fn sanitize_error_msg(mut err: String, custom_keys: &[&str]) -> String {
    let keys = [
        GOOGLE_API_KEY.as_str(),
        GROQ_API_KEY.as_str(),
        OPENROUTER_API_KEY.as_str(),
        GEMINI_API_KEY_1.as_str(),
        GEMINI_API_KEY_2.as_str(),
    ];
    for key in &keys {
        if !key.is_empty() {
            err = err.replace(*key, "[REDACTED]");
        }
    }
    for provider_id in ["google", "openrouter", "groq"] {
        for key in crate::settings::default_api_keys_for_provider(provider_id) {
            if !key.is_empty() {
                err = err.replace(&key, "[REDACTED]");
            }
        }
    }
    for key in custom_keys {
        if !key.is_empty() {
            err = err.replace(*key, "[REDACTED]");
        }
    }
    err
}

fn get_candidate_keys(settings: &AppSettings, provider_id: &str) -> Vec<String> {
    let mut keys = Vec::new();

    if let Some(key) = crate::credentials::get(provider_id) {
        keys.push(key);
    } else if let Some(key) = settings.post_process_api_keys.get(provider_id) {
        let key_trimmed = key.trim().to_string();
        if !key_trimmed.is_empty() {
            keys.push(key_trimmed);
        }
    }

    for key in crate::settings::default_api_keys_for_provider(provider_id) {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }

    for legacy_key in legacy_app_keys_for_provider(provider_id) {
        if !keys.contains(&legacy_key) {
            keys.push(legacy_key);
        }
    }

    keys
}

fn legacy_app_keys_for_provider(provider_id: &str) -> Vec<String> {
    let keys: &[&str] = match provider_id {
        "google" => &[
            GOOGLE_API_KEY.as_str(),
            GEMINI_API_KEY_1.as_str(),
            GEMINI_API_KEY_2.as_str(),
        ],
        "openrouter" => &[OPENROUTER_API_KEY.as_str()],
        "groq" => &[GROQ_API_KEY.as_str()],
        _ => &[],
    };
    keys.iter()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .collect()
}

fn consume_quota_if_needed(
    app: &AppHandle,
    settings: &AppSettings,
    provider_id: &str,
    api_key: &str,
    quota_consumed: &mut bool,
) -> Result<(), String> {
    let app_provided = crate::settings::is_app_provided_api_key(settings, provider_id, api_key)
        || legacy_app_keys_for_provider(provider_id)
            .iter()
            .any(|key| key == api_key.trim());
    if app_provided && !*quota_consumed {
        crate::settings::consume_llm_request_quota(app)?;
        *quota_consumed = true;
    }
    Ok(())
}

fn get_fallback_provider(settings: &AppSettings, provider_id: &str) -> PostProcessProvider {
    if let Some(provider) = settings.post_process_provider(provider_id) {
        provider.clone()
    } else {
        match provider_id {
            "google" => PostProcessProvider {
                id: "google".to_string(),
                label: "Google (Gemini)".to_string(),
                base_url: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
                allow_base_url_edit: false,
                models_endpoint: Some("/models".to_string()),
                supports_structured_output: true,
            },
            "openrouter" => PostProcessProvider {
                id: "openrouter".to_string(),
                label: "OpenRouter".to_string(),
                base_url: "https://openrouter.ai/api/v1".to_string(),
                allow_base_url_edit: false,
                models_endpoint: Some("/models".to_string()),
                supports_structured_output: true,
            },
            "groq" => PostProcessProvider {
                id: "groq".to_string(),
                label: "Groq".to_string(),
                base_url: "https://api.groq.com/openai/v1".to_string(),
                allow_base_url_edit: false,
                models_endpoint: Some("/models".to_string()),
                supports_structured_output: false,
            },
            _ => panic!("Unknown provider"),
        }
    }
}

async fn attempt_chat_completion(
    app: &AppHandle,
    settings: &AppSettings,
    provider: &PostProcessProvider,
    api_key: &str,
    model: &str,
    prompt_id: &str,
    prompt: &str,
    text: &str,
    quota_consumed: &mut bool,
) -> Result<String, String> {
    consume_quota_if_needed(app, settings, &provider.id, api_key, quota_consumed)?;
    let (reasoning_effort, reasoning) = match provider.id.as_str() {
        "custom" | "google" | "ollama" => (Some("none".to_string()), None),
        "openrouter" => (
            None,
            Some(crate::llm_client::ReasoningConfig {
                effort: Some("none".to_string()),
                exclude: Some(true),
            }),
        ),
        _ => (None, None),
    };

    if provider.supports_structured_output {
        let system_prompt = build_system_prompt(prompt);
        let user_content = text.to_string();

        let description = if prompt_id == "default_meeting_summary"
            || prompt_id == "default_meeting_notes_with_actions"
        {
            "The meeting summary and action items in English"
        } else if prompt_id == "default_translate_to_english" {
            "The translated text in English"
        } else if prompt_id == "default_manglish_transliteration" {
            "The transliterated text in Manglish"
        } else {
            "The cleaned and processed transcription text"
        };

        let json_schema = serde_json::json!({
            "type": "object",
            "properties": {
                (TRANSCRIPTION_FIELD): {
                    "type": "string",
                    "description": description
                }
            },
            "required": [TRANSCRIPTION_FIELD],
            "additionalProperties": false
        });

        match crate::llm_client::send_chat_completion_with_schema(
            app,
            provider,
            api_key.to_string(),
            model,
            user_content,
            Some(system_prompt),
            Some(json_schema),
            reasoning_effort.clone(),
            reasoning.clone(),
        )
        .await
        {
            Ok(Some(content)) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(json) => {
                    if let Some(transcription_value) =
                        json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str())
                    {
                        return Ok(strip_invisible_chars(transcription_value));
                    } else {
                        return Ok(strip_invisible_chars(&content));
                    }
                }
                Err(_) => {
                    return Ok(strip_invisible_chars(&content));
                }
            },
            Ok(None) => return Err("LLM API response has no content".to_string()),
            Err(e) => return Err(e),
        }
    }

    let processed_prompt = prompt.replace("${output}", text);
    match crate::llm_client::send_chat_completion(
        app,
        provider,
        api_key.to_string(),
        model,
        processed_prompt,
        reasoning_effort,
        reasoning,
    )
    .await
    {
        Ok(Some(content)) => Ok(strip_invisible_chars(&content)),
        Ok(None) => Err("LLM API response has no content".to_string()),
        Err(e) => Err(e),
    }
}

pub async fn run_specific_llm_prompt(
    app: &AppHandle,
    settings: &AppSettings,
    prompt_id: &str,
    text: &str,
) -> Option<String> {
    let mut quota_consumed = false;
    run_specific_llm_prompt_with_quota(app, settings, prompt_id, text, &mut quota_consumed).await
}

async fn run_specific_llm_prompt_with_quota(
    app: &AppHandle,
    settings: &AppSettings,
    prompt_id: &str,
    text: &str,
    quota_consumed: &mut bool,
) -> Option<String> {
    let is_meeting_summary =
        prompt_id == "default_meeting_summary" || prompt_id == "default_meeting_notes_with_actions";

    let prompt = match settings
        .post_process_prompts
        .iter()
        .find(|prompt| prompt.id == prompt_id)
    {
        Some(prompt) => prompt.prompt.clone(),
        None => {
            if prompt_id == "default_meeting_notes_with_actions"
                || prompt_id == "default_meeting_summary"
            {
                DEFAULT_MEETING_SUMMARY_PROMPT.to_string()
            } else {
                debug!(
                    "run_specific_llm_prompt: prompt '{}' was not found",
                    prompt_id
                );
                return None;
            }
        }
    };

    if prompt.trim().is_empty() {
        debug!("run_specific_llm_prompt: the prompt is empty");
        return None;
    }

    let mut result: Option<String> = None;

    if is_meeting_summary {
        // 1. Try configured provider/model
        let primary_provider = settings.active_post_process_provider().cloned();
        let primary_model = settings
            .post_process_models
            .get(
                &primary_provider
                    .as_ref()
                    .map(|p| p.id.clone())
                    .unwrap_or_default(),
            )
            .cloned()
            .unwrap_or_default();

        if let Some(ref provider) = primary_provider {
            if !primary_model.trim().is_empty() {
                let api_key =
                    crate::settings::resolved_post_process_api_key(settings, &provider.id);

                // Try up to 2 times (initial + 1 retry)
                for attempt in 1..=2 {
                    debug!(
                        "Attempt {} for primary model {} (provider: {})",
                        attempt, primary_model, provider.id
                    );
                    match attempt_chat_completion(
                        app,
                        settings,
                        provider,
                        &api_key,
                        &primary_model,
                        prompt_id,
                        &prompt,
                        text,
                        quota_consumed,
                    )
                    .await
                    {
                        Ok(res) => {
                            result = Some(res);
                            break;
                        }
                        Err(e) => {
                            let sanitized_err = sanitize_error_msg(e, &[&api_key]);
                            warn!(
                                "Primary model call failed (attempt {}): {}",
                                attempt, sanitized_err
                            );
                            // Emit fallback event
                            let next_fallback = FALLBACK_CHAIN.first();
                            let _ = app.emit(
                                "meeting-summary-fallback",
                                FallbackEventPayload {
                                    failed_model: primary_model.clone(),
                                    failed_provider: provider.id.clone(),
                                    error: sanitized_err,
                                    next_model: next_fallback.map(|f| f.model_name.to_string()),
                                    next_provider: next_fallback.map(|f| f.provider_id.to_string()),
                                },
                            );
                        }
                    }
                }
            }
        }

        if result.is_none() {
            warn!("Primary model failed or was not configured. Starting fallback chain.");

            // 2. Iterate through fallback chain
            for (idx, fallback) in FALLBACK_CHAIN.iter().enumerate() {
                let provider = get_fallback_provider(settings, fallback.provider_id);
                let candidate_keys = get_candidate_keys(settings, fallback.provider_id);

                if candidate_keys.is_empty() {
                    debug!(
                        "Skipping fallback model {} because no API key is available for provider {}",
                        fallback.model_name,
                        fallback.provider_id
                    );
                    continue;
                }

                let mut model_success = false;
                // Try each candidate key
                for key in &candidate_keys {
                    // Try up to 2 times for each key
                    for attempt in 1..=2 {
                        debug!(
                            "Fallback Attempt {} for model {} using provider {} (key length: {})",
                            attempt,
                            fallback.model_name,
                            fallback.provider_id,
                            key.len()
                        );
                        match attempt_chat_completion(
                            app,
                            settings,
                            &provider,
                            key,
                            fallback.model_name,
                            prompt_id,
                            &prompt,
                            text,
                            quota_consumed,
                        )
                        .await
                        {
                            Ok(res) => {
                                result = Some(res);
                                model_success = true;
                                break;
                            }
                            Err(e) => {
                                let sanitized_err = sanitize_error_msg(e, &[key]);
                                warn!(
                                    "Fallback model {} failed (attempt {}): {}",
                                    fallback.model_name, attempt, sanitized_err
                                );

                                // Determine next model in the chain for the event payload
                                let next_fallback = FALLBACK_CHAIN.get(idx + 1);
                                let _ = app.emit(
                                    "meeting-summary-fallback",
                                    FallbackEventPayload {
                                        failed_model: fallback.model_name.to_string(),
                                        failed_provider: fallback.provider_id.to_string(),
                                        error: sanitized_err,
                                        next_model: next_fallback.map(|f| f.model_name.to_string()),
                                        next_provider: next_fallback
                                            .map(|f| f.provider_id.to_string()),
                                    },
                                );
                            }
                        }
                    }
                    if model_success {
                        break;
                    }
                }

                if model_success {
                    break;
                }
            }
        }
    } else {
        // Fallback-free path for non-meeting-summary prompts
        let provider = match settings.active_post_process_provider().cloned() {
            Some(provider) => provider,
            None => {
                debug!("run_specific_llm_prompt: no provider is selected");
                return None;
            }
        };

        let model = settings
            .post_process_models
            .get(&provider.id)
            .cloned()
            .unwrap_or_default();

        if model.trim().is_empty() {
            debug!(
                "run_specific_llm_prompt: provider '{}' has no model configured",
                provider.id
            );
            return None;
        }

        let api_key = crate::settings::resolved_post_process_api_key(settings, &provider.id);

        match attempt_chat_completion(
            app,
            settings,
            &provider,
            &api_key,
            &model,
            prompt_id,
            &prompt,
            text,
            quota_consumed,
        )
        .await
        {
            Ok(res) => result = Some(res),
            Err(_) => {}
        }
    }

    result
}

async fn maybe_convert_chinese_variant(
    settings: &AppSettings,
    transcription: &str,
) -> Option<String> {
    // Check if language is set to Simplified or Traditional Chinese
    let is_simplified = settings.selected_language == "zh-Hans";
    let is_traditional = settings.selected_language == "zh-Hant";

    if !is_simplified && !is_traditional {
        debug!("selected_language is not Simplified or Traditional Chinese; skipping translation");
        return None;
    }

    debug!(
        "Starting Chinese translation using OpenCC for language: {}",
        settings.selected_language
    );

    // Use OpenCC to convert based on selected language
    let config = if is_simplified {
        // Convert Traditional Chinese to Simplified Chinese
        BuiltinConfig::Tw2sp
    } else {
        // Convert Simplified Chinese to Traditional Chinese
        BuiltinConfig::S2tw
    };

    match OpenCC::from_config(config) {
        Ok(converter) => {
            let converted = converter.convert(transcription);
            debug!(
                "OpenCC translation completed. Input length: {}, Output length: {}",
                transcription.len(),
                converted.len()
            );
            Some(converted)
        }
        Err(e) => {
            error!("Failed to initialize OpenCC converter: {}. Falling back to original transcription.", e);
            None
        }
    }
}

pub(crate) struct ProcessedTranscription {
    pub final_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
}

pub(crate) async fn process_transcription_output(
    app: &AppHandle,
    transcription: &str,
    post_process: bool,
) -> ProcessedTranscription {
    let settings = get_settings(app);
    let mut final_text = transcription.to_string();
    let mut post_processed_text: Option<String> = None;
    let mut post_process_prompt: Option<String> = None;

    if let Some(converted_text) = maybe_convert_chinese_variant(&settings, transcription).await {
        final_text = converted_text;
    }

    if post_process {
        if let Some(processed_text) = post_process_transcription(app, &settings, &final_text).await
        {
            post_processed_text = Some(processed_text.clone());
            final_text = processed_text;

            if let Some(prompt_id) = &settings.post_process_selected_prompt_id {
                if let Some(prompt) = settings
                    .post_process_prompts
                    .iter()
                    .find(|prompt| &prompt.id == prompt_id)
                {
                    post_process_prompt = Some(prompt.prompt.clone());
                }
            }
        }
    } else if final_text != transcription {
        post_processed_text = Some(final_text.clone());
    }

    match settings.output_language {
        OutputLanguage::Malayalam => {}
        OutputLanguage::Manglish => {
            if let Some(transliterated) =
                run_manglish_transliteration(app, &settings, &final_text).await
            {
                post_processed_text = Some(transliterated.clone());
                final_text = transliterated;
                if post_process_prompt.is_none() {
                    post_process_prompt = settings
                        .post_process_prompts
                        .iter()
                        .find(|p| p.id == "default_manglish_transliteration")
                        .map(|p| p.prompt.clone());
                }
            }
        }
        OutputLanguage::English => {
            if let Some(translated) = run_english_translation(app, &settings, &final_text).await {
                post_processed_text = Some(translated.clone());
                final_text = translated;
                if post_process_prompt.is_none() {
                    post_process_prompt = settings
                        .post_process_prompts
                        .iter()
                        .find(|p| p.id == "default_translate_to_english")
                        .map(|p| p.prompt.clone());
                }
            }
        }
    }

    ProcessedTranscription {
        final_text,
        post_processed_text,
        post_process_prompt,
    }
}

/// Run Manglish transliteration using the Google/Gemini provider with gemma-4-26b-a4b-it.
/// Falls back to the active post-processing provider if Google API key is not set.
async fn run_manglish_transliteration(
    app: &AppHandle,
    settings: &AppSettings,
    text: &str,
) -> Option<String> {
    let google_provider = settings.post_process_provider("google").cloned();
    let google_key = crate::settings::resolved_post_process_api_key(settings, "google");
    let mut quota_consumed = false;

    if let Some(provider) = google_provider {
        if !google_key.trim().is_empty() {
            let prompt_text = settings
                .post_process_prompts
                .iter()
                .find(|p| p.id == "default_manglish_transliteration")
                .map(|p| p.prompt.clone())
                .unwrap_or_else(|| {
                    "Transliterate the following Malayalam text into Manglish:\n\n${output}"
                        .to_string()
                });

            let processed_prompt = prompt_text.replace("${output}", text);
            debug!("Running Manglish transliteration with Google/gemma-4-26b-a4b-it");
            if let Err(error) = consume_quota_if_needed(
                app,
                settings,
                &provider.id,
                &google_key,
                &mut quota_consumed,
            ) {
                debug!("Manglish quota check failed: {}", error);
                return None;
            }
            match crate::llm_client::send_chat_completion(
                app,
                &provider,
                google_key,
                "gemma-4-26b-a4b-it",
                processed_prompt,
                Some("none".to_string()),
                None,
            )
            .await
            {
                Ok(Some(result)) => return Some(strip_invisible_chars(&result)),
                Ok(None) => debug!("Manglish: Google returned empty response"),
                Err(e) => debug!(
                    "Manglish: Google failed: {}; falling back to active provider",
                    e
                ),
            }
        }
    }
    // Fallback: use active post-process provider
    run_specific_llm_prompt_with_quota(
        app,
        settings,
        "default_manglish_transliteration",
        text,
        &mut quota_consumed,
    )
    .await
}

/// Run English translation using the Google/Gemini provider with gemma-4-26b-a4b-it.
/// Falls back to the active post-processing provider if Google API key is not set.
async fn run_english_translation(
    app: &AppHandle,
    settings: &AppSettings,
    text: &str,
) -> Option<String> {
    let google_provider = settings.post_process_provider("google").cloned();
    let google_key = crate::settings::resolved_post_process_api_key(settings, "google");
    let mut quota_consumed = false;

    if let Some(provider) = google_provider {
        if !google_key.trim().is_empty() {
            let prompt_text = settings
                .post_process_prompts
                .iter()
                .find(|p| p.id == "default_translate_to_english")
                .map(|p| p.prompt.clone())
                .unwrap_or_else(|| {
                    "Translate the following Malayalam text into English:\n\n${output}".to_string()
                });

            let processed_prompt = prompt_text.replace("${output}", text);
            debug!("Running English translation with Google/gemma-4-26b-a4b-it");
            if let Err(error) = consume_quota_if_needed(
                app,
                settings,
                &provider.id,
                &google_key,
                &mut quota_consumed,
            ) {
                debug!("English translation quota check failed: {}", error);
                return None;
            }
            match crate::llm_client::send_chat_completion(
                app,
                &provider,
                google_key,
                "gemma-4-26b-a4b-it",
                processed_prompt,
                Some("none".to_string()),
                None,
            )
            .await
            {
                Ok(Some(result)) => return Some(strip_invisible_chars(&result)),
                Ok(None) => debug!("English: Google returned empty response"),
                Err(e) => debug!(
                    "English: Google failed: {}; falling back to active provider",
                    e
                ),
            }
        }
    }
    // Fallback: use active post-process provider
    run_specific_llm_prompt_with_quota(
        app,
        settings,
        "default_translate_to_english",
        text,
        &mut quota_consumed,
    )
    .await
}

impl ShortcutAction for TranscribeAction {
    fn start(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        let start_time = Instant::now();
        debug!("TranscribeAction::start called for binding: {}", binding_id);

        // Load model in the background
        let tm = app.state::<Arc<TranscriptionManager>>();
        let rm = app.state::<Arc<AudioRecordingManager>>();

        // Load ASR model and VAD model in parallel
        tm.initiate_model_load();
        let rm_clone = Arc::clone(&rm);
        std::thread::spawn(move || {
            if let Err(e) = rm_clone.preload_vad() {
                debug!("VAD pre-load failed: {}", e);
            }
        });

        let binding_id = binding_id.to_string();
        change_tray_icon(app, TrayIconState::Recording);
        show_recording_overlay(app);

        // Get the microphone mode to determine audio feedback timing
        let settings = get_settings(app);
        let is_always_on = settings.always_on_microphone;
        debug!("Microphone mode - always_on: {}", is_always_on);

        // Emit recording state
        let _ = app.emit(
            "recording-state-changed",
            RecordingStatePayload {
                mode: "transcribe".to_string(),
            },
        );

        let mut recording_error: Option<String> = None;
        if is_always_on {
            // Always-on mode: Play audio feedback immediately, then apply mute after sound finishes
            debug!("Always-on mode: Playing audio feedback immediately");
            let rm_clone = Arc::clone(&rm);
            let app_clone = app.clone();
            // The blocking helper exits immediately if audio feedback is disabled,
            // so we can always reuse this thread to ensure mute happens right after playback.
            std::thread::spawn(move || {
                play_feedback_sound_blocking(&app_clone, SoundType::Start);
                rm_clone.apply_mute();
            });

            if let Err(e) = rm.try_start_recording(&binding_id) {
                debug!("Recording failed: {}", e);
                recording_error = Some(e);
            }
        } else {
            // On-demand mode: Start recording first, then play audio feedback, then apply mute
            // This allows the microphone to be activated before playing the sound
            debug!("On-demand mode: Starting recording first, then audio feedback");
            let recording_start_time = Instant::now();
            match rm.try_start_recording(&binding_id) {
                Ok(()) => {
                    debug!("Recording started in {:?}", recording_start_time.elapsed());
                    // Small delay to ensure microphone stream is active
                    let app_clone = app.clone();
                    let rm_clone = Arc::clone(&rm);
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        debug!("Handling delayed audio feedback/mute sequence");
                        // Helper handles disabled audio feedback by returning early, so we reuse it
                        // to keep mute sequencing consistent in every mode.
                        play_feedback_sound_blocking(&app_clone, SoundType::Start);
                        rm_clone.apply_mute();
                    });
                }
                Err(e) => {
                    debug!("Failed to start recording: {}", e);
                    recording_error = Some(e);
                }
            }
        }

        if recording_error.is_none() {
            // Dynamically register the cancel shortcut in a separate task to avoid deadlock
            shortcut::register_cancel_shortcut(app);
        } else {
            // Starting failed (for example due to blocked microphone permissions).
            // Revert UI state so we don't stay stuck in the recording overlay.
            let _ = app.emit(
                "recording-state-changed",
                RecordingStatePayload {
                    mode: "idle".to_string(),
                },
            );
            utils::hide_recording_overlay(app);
            change_tray_icon(app, TrayIconState::Idle);
            if let Some(err) = recording_error {
                let error_type = if is_microphone_access_denied(&err) {
                    "microphone_permission_denied"
                } else if is_no_input_device_error(&err) {
                    "no_input_device"
                } else {
                    "unknown"
                };
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: error_type.to_string(),
                        detail: Some(err),
                    },
                );
            }
        }

        debug!(
            "TranscribeAction::start completed in {:?}",
            start_time.elapsed()
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        // Unregister the cancel shortcut when transcription stops
        shortcut::unregister_cancel_shortcut(app);

        let stop_time = Instant::now();
        debug!("TranscribeAction::stop called for binding: {}", binding_id);

        let ah = app.clone();
        let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
        let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
        let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());

        change_tray_icon(app, TrayIconState::Transcribing);
        show_transcribing_overlay(app);

        // Unmute before playing audio feedback so the stop sound is audible
        rm.remove_mute();

        // Play audio feedback for recording stop
        play_feedback_sound(app, SoundType::Stop);

        let settings = get_settings(app);
        let post_process = binding_id == "transcribe_with_post_process";
        let has_llm_post_process = post_process
            || settings.output_language == OutputLanguage::Manglish
            || settings.output_language == OutputLanguage::English;

        let binding_id = binding_id.to_string(); // Clone binding_id for the async task
        tauri::async_runtime::spawn(async move {
            let _guard = FinishGuard(ah.clone());
            debug!(
                "Starting async transcription task for binding: {}",
                binding_id
            );

            let stop_recording_time = Instant::now();
            if let Some(samples) = rm.stop_recording(&binding_id) {
                debug!(
                    "Recording stopped and samples retrieved in {:?}, sample count: {}",
                    stop_recording_time.elapsed(),
                    samples.len()
                );

                if samples.is_empty() {
                    debug!("Recording produced no audio samples; skipping persistence");
                    utils::hide_recording_overlay(&ah);
                    change_tray_icon(&ah, TrayIconState::Idle);
                } else {
                    // Save WAV concurrently with transcription
                    let sample_count = samples.len();
                    let file_name = format!("thegai-{}.wav", chrono::Utc::now().timestamp());
                    let wav_path = hm.recordings_dir().join(&file_name);
                    let wav_path_for_verify = wav_path.clone();
                    let samples_for_wav = samples.clone();
                    let wav_handle = tauri::async_runtime::spawn_blocking(move || {
                        crate::audio_toolkit::save_wav_file(&wav_path, &samples_for_wav)
                    });

                    // Transcribe concurrently with WAV save
                    let transcription_time = Instant::now();
                    let tm_clone = tm.clone();
                    let transcription_result =
                        tauri::async_runtime::spawn_blocking(move || tm_clone.transcribe(samples))
                            .await
                            .unwrap_or_else(|e| {
                                Err(anyhow::anyhow!("Transcription task panicked: {}", e))
                            });
                    // Await WAV save and verify
                    let wav_saved = match wav_handle.await {
                        Ok(Ok(())) => {
                            match crate::audio_toolkit::verify_wav_file(
                                &wav_path_for_verify,
                                sample_count,
                            ) {
                                Ok(()) => true,
                                Err(e) => {
                                    error!("WAV verification failed: {}", e);
                                    false
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            error!("Failed to save WAV file: {}", e);
                            false
                        }
                        Err(e) => {
                            error!("WAV save task panicked: {}", e);
                            false
                        }
                    };

                    match transcription_result {
                        Ok(result) => {
                            let transcription = result.text;
                            debug!(
                                "Transcription completed in {:?}: '{}'",
                                transcription_time.elapsed(),
                                transcription
                            );

                            if has_llm_post_process {
                                show_processing_overlay(&ah);
                            }
                            let processed =
                                process_transcription_output(&ah, &transcription, post_process)
                                    .await;

                            // Save to history if WAV was saved
                            if wav_saved {
                                if let Err(err) = hm.save_entry(
                                    file_name,
                                    transcription,
                                    has_llm_post_process,
                                    processed.post_processed_text.clone(),
                                    processed.post_process_prompt.clone(),
                                ) {
                                    error!("Failed to save history entry: {}", err);
                                }
                            }

                            if processed.final_text.is_empty() {
                                utils::hide_recording_overlay(&ah);
                                change_tray_icon(&ah, TrayIconState::Idle);
                            } else {
                                let ah_clone = ah.clone();
                                let paste_time = Instant::now();
                                let final_text = processed.final_text;
                                ah.run_on_main_thread(move || {
                                    match utils::paste(final_text, ah_clone.clone()) {
                                        Ok(()) => debug!(
                                            "Text pasted successfully in {:?}",
                                            paste_time.elapsed()
                                        ),
                                        Err(e) => {
                                            error!("Failed to paste transcription: {}", e);
                                            let _ = ah_clone.emit("paste-error", ());
                                        }
                                    }
                                    utils::hide_recording_overlay(&ah_clone);
                                    change_tray_icon(&ah_clone, TrayIconState::Idle);
                                })
                                .unwrap_or_else(|e| {
                                    error!("Failed to run paste on main thread: {:?}", e);
                                    utils::hide_recording_overlay(&ah);
                                    change_tray_icon(&ah, TrayIconState::Idle);
                                });
                            }
                        }
                        Err(err) => {
                            debug!("Global Shortcut Transcription error: {}", err);
                            // Save entry with empty text so user can retry
                            if wav_saved {
                                if let Err(save_err) = hm.save_entry(
                                    file_name,
                                    String::new(),
                                    post_process,
                                    None,
                                    None,
                                ) {
                                    error!("Failed to save failed history entry: {}", save_err);
                                }
                            }
                            utils::hide_recording_overlay(&ah);
                            change_tray_icon(&ah, TrayIconState::Idle);
                        }
                    }
                }
            } else {
                debug!("No samples retrieved from recording stop");
                utils::hide_recording_overlay(&ah);
                change_tray_icon(&ah, TrayIconState::Idle);
            }
        });

        debug!(
            "TranscribeAction::stop completed in {:?}",
            stop_time.elapsed()
        );
    }
}

// Cancel Action
struct CancelAction;

impl ShortcutAction for CancelAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        utils::cancel_current_operation(app);
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // Nothing to do on stop for cancel
    }
}

// Meeting Action
struct MeetingAction;

/// A word remains associated with its source while the transcript is merged
/// on the shared meeting clock. The public diarization core intentionally sees
/// only timestamped words; source attribution comes from its speech timeline.
#[derive(Clone, Debug)]
struct MeetingTranscriptWord {
    source: CaptureSource,
    text: String,
    start_ms: u64,
    end_ms: u64,
}

#[derive(Clone, Debug, Default)]
struct MeetingTranscript {
    text: String,
    words: Vec<MeetingTranscriptWord>,
}

impl MeetingTranscript {
    fn diarization_words(&self) -> Vec<TranscriptWord> {
        self.words
            .iter()
            .map(|word| TranscriptWord {
                text: word.text.clone(),
                start_ms: i64::try_from(word.start_ms).unwrap_or(i64::MAX),
                end_ms: i64::try_from(word.end_ms).unwrap_or(i64::MAX),
                confidence: None,
            })
            .collect()
    }

    /// Preserve source attribution and meeting-clock timings even when the
    /// optional speaker diarization experiment is disabled or unavailable.
    fn history_segments(&self) -> Vec<TranscriptSegment> {
        self.words
            .iter()
            .filter_map(|word| {
                let text = word.text.trim();
                (!text.is_empty()).then(|| TranscriptSegment {
                    start_ms: word.start_ms,
                    end_ms: word.end_ms.max(word.start_ms.saturating_add(1)),
                    source: capture_source_name(word.source).to_string(),
                    text: text.to_string(),
                    confidence: None,
                })
            })
            .collect()
    }
}

/// Coalesce adjacent word-level ASR rows into readable source spans. The
/// grouping mirrors the frontend transcript timeline so citation IDs resolve
/// to the same Malayalam excerpts in both places.
fn coalesce_meeting_transcript_segments(segments: &[TranscriptSegment]) -> Vec<TranscriptSegment> {
    let mut merged: Vec<TranscriptSegment> = Vec::new();
    let mut ordered: Vec<&TranscriptSegment> = segments.iter().collect();
    ordered.sort_by_key(|segment| segment.start_ms);

    for segment in ordered {
        let text = segment.text.trim();
        if text.is_empty() {
            continue;
        }

        if let Some(previous) = merged.last_mut() {
            if previous.source == segment.source
                && segment.start_ms.saturating_sub(previous.end_ms) <= 750
            {
                previous.end_ms = previous.end_ms.max(segment.end_ms);
                if !previous.text.is_empty() {
                    previous.text.push(' ');
                }
                previous.text.push_str(text);
                continue;
            }
        }

        merged.push(TranscriptSegment {
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            source: segment.source.clone(),
            text: text.to_string(),
            confidence: segment.confidence,
        });
    }

    merged
}

fn indexed_meeting_transcript(transcript: &MeetingTranscript) -> String {
    let segments = coalesce_meeting_transcript_segments(&transcript.history_segments());
    if segments.is_empty() {
        return transcript.text.clone();
    }

    segments
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            format!(
                "[SEG-{index:03} start_ms={} end_ms={} source={}]\n{}",
                segment.start_ms, segment.end_ms, segment.source, segment.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn capture_source_name(source: CaptureSource) -> &'static str {
    match source {
        CaptureSource::Microphone => "microphone",
        CaptureSource::System => "system",
        CaptureSource::Mix => "mix",
    }
}

fn merge_meeting_transcripts(
    microphone: crate::malayalam_asr::MalayalamTranscription,
    system: crate::malayalam_asr::MalayalamTranscription,
) -> MeetingTranscript {
    let mut words = Vec::new();
    words.extend(
        microphone
            .words
            .into_iter()
            .map(|word| MeetingTranscriptWord {
                source: CaptureSource::Microphone,
                text: word.text,
                start_ms: word.start_ms,
                end_ms: word.end_ms,
            }),
    );
    words.extend(system.words.into_iter().map(|word| MeetingTranscriptWord {
        source: CaptureSource::System,
        text: word.text,
        start_ms: word.start_ms,
        end_ms: word.end_ms,
    }));
    words.sort_by_key(|word| {
        (
            word.start_ms,
            word.end_ms,
            match word.source {
                CaptureSource::Microphone => 0_u8,
                CaptureSource::System => 1,
                CaptureSource::Mix => 2,
            },
        )
    });

    let text = if words.is_empty() {
        [microphone.text, system.text]
            .into_iter()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let mut text = String::new();
        for word in &words {
            append_meeting_word(&mut text, &word.text);
        }
        text
    };

    MeetingTranscript { text, words }
}

fn append_meeting_word(target: &mut String, word: &str) {
    let word = word.trim();
    if word.is_empty() {
        return;
    }
    let is_closing_punctuation = word.chars().next().is_some_and(|character| {
        matches!(character, '.' | ',' | '!' | '?' | ':' | ';' | ')' | ']')
    });
    if !target.is_empty() && !is_closing_punctuation {
        target.push(' ');
    }
    target.push_str(word);
}

const MEETING_ASR_SAMPLE_RATE_HZ: u32 = 16_000;
const MEETING_ASR_CORE_CHUNK_SECONDS: u64 = 30;
const MEETING_ASR_CONTEXT_SECONDS: u64 = 1;

fn track_has_audio(samples: &[f32]) -> bool {
    // A sparse peak check avoids sending a fully silent unavailable track to
    // ASR while preserving quiet speech. Derived ASR tracks are already mono.
    samples
        .iter()
        .any(|sample| sample.is_finite() && sample.abs() > 1.0e-5)
}

fn empty_malayalam_transcription() -> crate::malayalam_asr::MalayalamTranscription {
    crate::malayalam_asr::MalayalamTranscription {
        text: String::new(),
        words: Vec::new(),
    }
}

/// Transcribe one timestamp-rendered 16 kHz ASR track in bounded, overlapping
/// windows. The source WAV begins at meeting clock zero (including any
/// captured leading silence), so each chunk's sample offset is also its shared
/// meeting offset. This avoids loading several hours of two-track audio into
/// memory or presenting CTC timings on a length-aligned synthetic timeline.
fn transcribe_meeting_asr_track(
    manager: &TranscriptionManager,
    path: &Path,
    source: CaptureSource,
) -> anyhow::Result<crate::malayalam_asr::MalayalamTranscription> {
    let mut reader = WavReader::open(path)
        .map_err(|error| anyhow::anyhow!("open derived {source:?} ASR track {path:?}: {error}"))?;
    let spec = reader.spec();
    if spec.channels != 1
        || spec.sample_rate != MEETING_ASR_SAMPLE_RATE_HZ
        || spec.sample_format != SampleFormat::Int
        || spec.bits_per_sample != 16
    {
        return Err(anyhow::anyhow!(
            "derived {source:?} ASR track {path:?} must be 16 kHz mono 16-bit PCM"
        ));
    }

    let total_samples = u64::from(reader.duration());
    if total_samples == 0 {
        return Ok(empty_malayalam_transcription());
    }
    let core_samples = MEETING_ASR_CORE_CHUNK_SECONDS
        .checked_mul(u64::from(MEETING_ASR_SAMPLE_RATE_HZ))
        .context("meeting ASR core chunk duration overflow")?;
    let context_samples = MEETING_ASR_CONTEXT_SECONDS
        .checked_mul(u64::from(MEETING_ASR_SAMPLE_RATE_HZ))
        .context("meeting ASR context duration overflow")?;

    let mut words = Vec::new();
    let mut core_start_sample = 0_u64;
    while core_start_sample < total_samples {
        let core_end_sample = core_start_sample
            .saturating_add(core_samples)
            .min(total_samples);
        let window_start_sample = core_start_sample.saturating_sub(context_samples);
        let window_end_sample = core_end_sample
            .saturating_add(context_samples)
            .min(total_samples);
        let window_sample_count = window_end_sample.saturating_sub(window_start_sample);
        let window_start_u32 = u32::try_from(window_start_sample)
            .context("meeting ASR track exceeds hound's seek range")?;
        reader
            .seek(window_start_u32)
            .map_err(|error| anyhow::anyhow!("seek {source:?} ASR track: {error}"))?;
        let window_sample_count_usize = usize::try_from(window_sample_count)
            .context("meeting ASR window length exceeds addressable memory")?;
        let mut samples = Vec::with_capacity(window_sample_count_usize);
        for sample in reader.samples::<i16>().take(window_sample_count_usize) {
            samples.push(
                sample.map_err(|error| {
                    anyhow::anyhow!("read {source:?} ASR track samples: {error}")
                })? as f32
                    / i16::MAX as f32,
            );
        }

        if track_has_audio(&samples) {
            match manager.transcribe_thegav1_with_timing(samples) {
                Ok(transcription) => {
                    let window_start_ms = samples_to_ms(window_start_sample);
                    let core_start_ms = samples_to_ms(core_start_sample);
                    let core_end_ms = samples_to_ms(core_end_sample);
                    if transcription.words.is_empty() && !transcription.text.trim().is_empty() {
                        words.push(crate::malayalam_asr::TimedWord {
                            text: transcription.text,
                            start_ms: core_start_ms,
                            end_ms: core_end_ms.max(core_start_ms.saturating_add(1)),
                        });
                    } else {
                        for mut word in transcription.words {
                            word.start_ms = word.start_ms.saturating_add(window_start_ms);
                            word.end_ms = word.end_ms.saturating_add(window_start_ms);
                            let midpoint_ms = word
                                .start_ms
                                .saturating_add(word.end_ms.saturating_sub(word.start_ms) / 2);
                            // Keep exactly one copy of CTC words in overlap
                            // regions: their midpoint belongs to this core
                            // interval, except that the final interval owns
                            // its closing boundary.
                            if midpoint_ms >= core_start_ms
                                && (core_end_sample == total_samples || midpoint_ms < core_end_ms)
                            {
                                words.push(word);
                            }
                        }
                    }
                }
                Err(error) => warn!(
                    "{source:?} meeting ASR chunk {}-{} ms failed; retaining other chunks: {error}",
                    samples_to_ms(core_start_sample),
                    samples_to_ms(core_end_sample),
                ),
            }
        }

        core_start_sample = core_end_sample;
    }

    words.sort_by_key(|word| (word.start_ms, word.end_ms));
    let mut text = String::new();
    for word in &words {
        append_meeting_word(&mut text, &word.text);
    }
    Ok(crate::malayalam_asr::MalayalamTranscription { text, words })
}

fn samples_to_ms(samples: u64) -> u64 {
    samples
        .saturating_mul(1_000)
        .saturating_add(u64::from(MEETING_ASR_SAMPLE_RATE_HZ / 2))
        / u64::from(MEETING_ASR_SAMPLE_RATE_HZ)
}

fn transcribe_meeting_tracks(
    manager: &TranscriptionManager,
    microphone_path: &Path,
    system_path: &Path,
) -> anyhow::Result<MeetingTranscript> {
    // A source can legitimately be unavailable (for example, system loopback
    // denied by the platform). Keep a usable source transcript rather than
    // discarding an otherwise recoverable meeting because its peer track is
    // incomplete or cannot be decoded.
    let mut track_errors = Vec::new();
    let microphone =
        match transcribe_meeting_asr_track(manager, microphone_path, CaptureSource::Microphone) {
            Ok(transcription) => transcription,
            Err(error) => {
                warn!("Microphone meeting ASR track could not be transcribed: {error}");
                track_errors.push(format!("microphone: {error}"));
                empty_malayalam_transcription()
            }
        };
    let system = match transcribe_meeting_asr_track(manager, system_path, CaptureSource::System) {
        Ok(transcription) => transcription,
        Err(error) => {
            warn!("System meeting ASR track could not be transcribed: {error}");
            track_errors.push(format!("system: {error}"));
            empty_malayalam_transcription()
        }
    };

    let transcript = merge_meeting_transcripts(microphone, system);
    if transcript.text.trim().is_empty() {
        let mut message = "No speech was detected in the microphone or system meeting tracks".to_string();
        if !track_errors.is_empty() {
            message.push_str(&format!(". Underlying failures: {}", track_errors.join("; ")));
        }
        return Err(anyhow::anyhow!(message));
    }
    Ok(transcript)
}

fn pending_meeting_history_entry(
    history: &HistoryManager,
    artifacts: &MeetingCaptureArtifacts,
    prompt_id: &str,
) -> Option<i64> {
    match history.save_entry_with_meeting_session(
        artifacts.audio_tracks.mix.clone(),
        String::new(),
        true,
        None,
        Some(prompt_id.to_string()),
        artifacts.audio_tracks.clone(),
        MeetingSession {
            root: artifacts.session_root.clone(),
            manifest: artifacts.manifest_path.clone(),
        },
    ) {
        Ok(entry) => Some(entry.id),
        Err(error) => {
            error!("Failed to save pending meeting history entry: {error}");
            None
        }
    }
}

/// Run the intentionally opt-in diarization path without making it a
/// prerequisite for the ordinary meeting transcript or summary. The derived
/// ASR WAVs have already been rendered onto the shared meeting timeline by
/// `MeetingCaptureSession`, including leading gaps, so their sample zero is
/// meeting time zero. Passing the device offset here would apply it twice.
fn diarize_meeting_tracks(
    diarization_model_manager: &DiarizationModelManager,
    manifest: MeetingCaptureManifest,
    microphone_path: &Path,
    system_path: &Path,
    transcript: &MeetingTranscript,
) -> Option<Vec<SpeakerSegment>> {
    let inference_config = DiarizationInferenceConfig::default();
    let microphone_turns = match detect_speech_turns_from_wav(
        microphone_path,
        CaptureSource::Microphone,
        0,
        &inference_config,
    ) {
        Ok(turns) => turns,
        Err(error) => {
            warn!("Microphone meeting-track VAD failed; continuing without microphone labels: {error}");
            Vec::new()
        }
    };

    let (system_turns, system_embeddings) = match DiarizationInference::new(
        diarization_model_manager,
        inference_config.clone(),
    )
    .and_then(|mut inference| inference.infer_system_wav(system_path, 0))
    {
        Ok(result) => (result.speech_turns, result.system_embeddings),
        Err(error) => {
            // A missing, corrupted, or temporarily unavailable optional
            // embedding model must not suppress the useful two-source
            // attribution fallback.
            warn!(
                    "System speaker embedding inference unavailable; using source-only attribution: {error}"
                );
            let turns = match detect_speech_turns_from_wav(
                system_path,
                CaptureSource::System,
                0,
                &inference_config,
            ) {
                Ok(turns) => turns,
                Err(vad_error) => {
                    warn!(
                            "System meeting-track VAD failed; continuing without remote labels: {vad_error}"
                        );
                    Vec::new()
                }
            };
            (turns, Vec::new())
        }
    };

    let mut speech_turns = microphone_turns;
    speech_turns.extend(system_turns);
    let input = DiarizationInput {
        manifest,
        speech_turns,
        system_embeddings,
        transcript_words: transcript.diarization_words(),
    };
    let config = DiarizationConfig {
        enabled: true,
        maximum_embedding_windows: inference_config.maximum_embedding_windows,
        ..DiarizationConfig::default()
    };

    let outcome = match diarize(&input, &config) {
        Ok(outcome) => outcome,
        Err(error) => {
            warn!("Meeting diarization policy failed; preserving unlabelled transcript: {error}");
            return None;
        }
    };
    debug!(
        "Meeting diarization finished with {:?}: microphone_turns={}, system_turns={}, embeddings={}, remote_speakers={}",
        outcome.status,
        outcome.diagnostics.microphone_speech_turns,
        outcome.diagnostics.system_speech_turns,
        outcome.diagnostics.embedding_windows,
        outcome.diagnostics.remote_speaker_count,
    );

    match outcome.status {
        DiarizationStatus::Complete | DiarizationStatus::SourceAttributionOnly => Some(
            outcome
                .segments
                .into_iter()
                .map(history_speaker_segment)
                .collect(),
        ),
        DiarizationStatus::Disabled
        | DiarizationStatus::Unavailable
        | DiarizationStatus::Failed => {
            if let Some(reason) = outcome.diagnostics.reason {
                debug!("Meeting diarization did not produce labels: {reason}");
            }
            None
        }
    }
}

fn history_speaker_segment(
    segment: crate::managers::diarization::SpeakerSegment,
) -> SpeakerSegment {
    let (speaker, source) = match segment.label {
        SpeakerLabel::LocalUser => ("You".to_string(), "microphone".to_string()),
        SpeakerLabel::RemoteUnattributed => ("Remote".to_string(), "system".to_string()),
        SpeakerLabel::RemoteSpeaker { index } => {
            (format!("Remote Speaker {index}"), "system".to_string())
        }
        SpeakerLabel::Multiple => (
            "Multiple speakers".to_string(),
            "microphone+system".to_string(),
        ),
        SpeakerLabel::Unknown => ("Unknown".to_string(), "unknown".to_string()),
    };
    SpeakerSegment {
        start_ms: u64::try_from(segment.start_ms).unwrap_or_default(),
        end_ms: u64::try_from(segment.end_ms).unwrap_or_default(),
        speaker,
        source,
        text: segment.text,
        confidence: Some(segment.label_coverage),
    }
}

struct CompletedMeetingTranscription {
    transcript: MeetingTranscript,
    summary: Option<String>,
    display_summary: String,
}

/// Run track-aware ASR first, then the existing meeting-summary prompt. The
/// model wait is intentional: if the normal startup download is already in
/// progress, a completed meeting should wait for it rather than silently
/// falling back to the derived mix. If no download exists, the caller gets an
/// error while the durable source tracks remain retryable.
async fn transcribe_and_summarize_meeting(
    app: &AppHandle,
    transcription_manager: Arc<TranscriptionManager>,
    settings: &AppSettings,
    prompt_id: &str,
    microphone_path: PathBuf,
    system_path: PathBuf,
) -> anyhow::Result<CompletedMeetingTranscription> {
    let app_for_transcription = app.clone();
    let transcription_started = Instant::now();
    let transcript = tokio::task::spawn_blocking(move || {
        let loaded_thegav1 = transcription_manager
            .load_model_if_different_waiting_for_download("thegav1")
            .map_err(|error| anyhow::anyhow!("load ThegaV1 for meeting transcription: {error}"));

        let result = match loaded_thegav1 {
            Ok(()) => transcribe_meeting_tracks(
                &transcription_manager,
                &microphone_path,
                &system_path,
            ),
            Err(error) => Err(error),
        };

        // Restore the user's selected engine after both success and failure.
        // This prevents a retryable failed meeting from unexpectedly changing
        // future ordinary transcription behaviour.
        let selected_model = get_settings(&app_for_transcription).selected_model;
        if !selected_model.is_empty() && selected_model != "thegav1" {
            if let Err(error) = transcription_manager.load_model_if_different(&selected_model) {
                warn!(
                    "Failed to restore selected model {selected_model} after meeting transcription: {error}"
                );
            }
        } else {
            transcription_manager.maybe_unload_immediately("meeting transcription");
        }
        result
    })
    .await
    .map_err(|error| anyhow::anyhow!("Meeting transcription worker panicked: {error}"))??;

    debug!(
        "Track-aware meeting transcription completed in {:?}",
        transcription_started.elapsed()
    );
    let summary_input = indexed_meeting_transcript(&transcript);
    let summary = run_specific_llm_prompt(app, settings, prompt_id, &summary_input).await;
    let display_summary = if prompt_id == "default_meeting_notes_with_actions" {
        summary
            .as_ref()
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
            .and_then(|value| {
                value
                    .get("summary")
                    .and_then(|summary| summary.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| summary.clone().unwrap_or_else(|| transcript.text.clone()))
    } else {
        summary.clone().unwrap_or_else(|| transcript.text.clone())
    };

    Ok(CompletedMeetingTranscription {
        transcript,
        summary,
        display_summary,
    })
}

/// Schedule speaker inference after the primary transcript has been written.
/// This intentionally has no await point in the normal meeting completion or
/// retry command: diarization is experimental and must never delay, fail, or
/// replace the source-aware ASR result.
fn queue_optional_meeting_diarization(
    enabled: bool,
    history: Arc<HistoryManager>,
    entry_id: Option<i64>,
    diarization_model_manager: Option<Arc<DiarizationModelManager>>,
    manifest: MeetingCaptureManifest,
    microphone_path: PathBuf,
    system_path: PathBuf,
    transcript: MeetingTranscript,
    transcript_segments: Vec<TranscriptSegment>,
) {
    if !enabled {
        return;
    }
    let (entry_id, model_manager) = match (entry_id, diarization_model_manager) {
        (Some(entry_id), Some(model_manager)) => (entry_id, model_manager),
        (None, _) => {
            debug!("Skipping optional meeting diarization because history persistence failed");
            return;
        }
        (_, None) => {
            warn!("Meeting diarization was enabled but its model manager is unavailable");
            return;
        }
    };

    tauri::async_runtime::spawn(async move {
        let expected_transcription_text = transcript.text.clone();
        let speaker_segments = match tokio::task::spawn_blocking(move || {
            diarize_meeting_tracks(
                &model_manager,
                manifest,
                &microphone_path,
                &system_path,
                &transcript,
            )
        })
        .await
        {
            Ok(segments) => segments,
            Err(error) => {
                warn!(
                    "Meeting diarization worker panicked; transcript remains unlabelled: {error}"
                );
                None
            }
        };

        if let Some(speaker_segments) = speaker_segments {
            match history.update_meeting_speaker_segments_if_current(
                entry_id,
                &expected_transcription_text,
                &transcript_segments,
                speaker_segments,
            ) {
                Ok(true) => {}
                Ok(false) => debug!(
                    "Skipping stale optional meeting speaker labels for history entry {entry_id}"
                ),
                Err(error) => error!("Failed to persist optional meeting speaker labels: {error}"),
            }
        }
    });
}

/// Retry a native meeting from its individual, timestamp-rendered ASR tracks.
/// The legacy generic retry command uses a single mix WAV for ordinary
/// recordings; native meetings must not use that path because it erases the
/// local-vs-system source attribution retained in their capture manifest.
pub async fn retry_meeting_history_entry(
    app: &AppHandle,
    history_manager: Arc<HistoryManager>,
    transcription_manager: Arc<TranscriptionManager>,
    diarization_model_manager: Option<Arc<DiarizationModelManager>>,
    entry: HistoryEntry,
) -> Result<(), String> {
    let (microphone_path, system_path, manifest_path) = history_manager
        .resolve_meeting_track_paths(&entry)
        .map_err(|error| error.to_string())?;
    let manifest =
        tokio::task::spawn_blocking(move || -> anyhow::Result<MeetingCaptureManifest> {
            let file = std::fs::File::open(&manifest_path)
                .with_context(|| format!("open meeting capture manifest {manifest_path:?}"))?;
            let capture: crate::managers::meeting_capture::CaptureSessionManifest =
                serde_json::from_reader(file)
                    .with_context(|| format!("parse meeting capture manifest {manifest_path:?}"))?;
            Ok(capture.diarization_manifest)
        })
        .await
        .map_err(|error| format!("Meeting manifest reader panicked: {error}"))?
        .map_err(|error| error.to_string())?;

    let settings = get_settings(app);
    let prompt_id = entry.post_process_prompt.clone().unwrap_or_else(|| {
        if settings.google_oauth_token.is_some() {
            "default_meeting_notes_with_actions".to_string()
        } else {
            "default_meeting_summary".to_string()
        }
    });
    let completed = transcribe_and_summarize_meeting(
        app,
        transcription_manager,
        &settings,
        &prompt_id,
        microphone_path.clone(),
        system_path.clone(),
    )
    .await
    .map_err(|error| error.to_string())?;

    let transcript_segments = completed.transcript.history_segments();
    history_manager
        .update_meeting_transcription_with_timed_segments(
            entry.id,
            completed.transcript.text.clone(),
            completed.summary.clone(),
            Some(prompt_id.clone()),
            Some(transcript_segments.clone()),
            None,
        )
        .map_err(|error| error.to_string())?;

    if !completed.display_summary.is_empty() {
        let _ = app.emit(
            "meeting-summary",
            MeetingSummaryPayload {
                summary: completed.display_summary,
                transcript: completed.transcript.text.clone(),
            },
        );
    }

    queue_optional_meeting_diarization(
        settings.meeting_diarization_enabled,
        history_manager,
        Some(entry.id),
        diarization_model_manager,
        manifest,
        microphone_path,
        system_path,
        completed.transcript,
        transcript_segments,
    );
    Ok(())
}

impl ShortcutAction for MeetingAction {
    fn start(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        let start_time = Instant::now();
        debug!("MeetingAction::start called for binding: {binding_id}");
        let recording_start_time = Instant::now();
        let result = app
            .state::<Arc<AudioRecordingManager>>()
            .try_start_meeting_capture();

        match result {
            Ok(()) => {
                debug!(
                    "Timestamped meeting recording started in {:?}",
                    recording_start_time.elapsed()
                );
                change_tray_icon(app, TrayIconState::Recording);
                crate::overlay::show_meeting_recording_overlay(app);
                let _ = app.emit(
                    "recording-state-changed",
                    RecordingStatePayload {
                        mode: "meeting".to_string(),
                    },
                );
                shortcut::register_cancel_shortcut(app);
            }
            Err(error) => {
                debug!("Failed to start native meeting recording: {error}");
                let _ = app.emit(
                    "recording-state-changed",
                    RecordingStatePayload {
                        mode: "idle".to_string(),
                    },
                );
                crate::overlay::hide_meeting_prompt_window(app);
                change_tray_icon(app, TrayIconState::Idle);
                let error_type = if is_microphone_access_denied(&error) {
                    "microphone_permission_denied"
                } else if is_no_input_device_error(&error) {
                    "no_input_device"
                } else {
                    "unknown"
                };
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: error_type.to_string(),
                        detail: Some(error),
                    },
                );
            }
        }
        debug!(
            "MeetingAction::start completed in {:?}",
            start_time.elapsed()
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        shortcut::unregister_cancel_shortcut(app);
        let stop_time = Instant::now();
        debug!("MeetingAction::stop called for binding: {binding_id}");

        let ah = app.clone();
        let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
        let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
        let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());
        let diarization_model_manager = app
            .try_state::<Arc<DiarizationModelManager>>()
            .map(|manager| Arc::clone(manager.inner()));
        change_tray_icon(app, TrayIconState::Transcribing);

        tauri::async_runtime::spawn(async move {
            let _guard = FinishGuard(ah.clone());
            let finalize_started = Instant::now();
            let artifacts =
                match tokio::task::spawn_blocking(move || rm.stop_meeting_capture()).await {
                    Ok(Ok(Some(artifacts))) => artifacts,
                    Ok(Ok(None)) => {
                        debug!("No active native meeting capture to stop");
                        change_tray_icon(&ah, TrayIconState::Idle);
                        return;
                    }
                    Ok(Err(error)) => {
                        error!("Failed to finalize meeting capture: {error}");
                        change_tray_icon(&ah, TrayIconState::Idle);
                        return;
                    }
                    Err(error) => {
                        error!("Meeting capture finalization worker panicked: {error}");
                        change_tray_icon(&ah, TrayIconState::Idle);
                        return;
                    }
                };
            debug!(
                "Meeting source tracks finalized in {:?}: {}",
                finalize_started.elapsed(),
                artifacts.manifest_path
            );
            crate::overlay::show_meeting_stopped_overlay(&ah);

            let settings = get_settings(&ah);
            let prompt_id = if settings.google_oauth_token.is_some() {
                "default_meeting_notes_with_actions"
            } else {
                "default_meeting_summary"
            };
            let history_entry_id = pending_meeting_history_entry(&hm, &artifacts, prompt_id);

            let microphone_path = hm.recordings_dir().join(&artifacts.audio_tracks.microphone);
            let system_path = hm.recordings_dir().join(&artifacts.audio_tracks.system);
            // The ASR worker owns its path copies. Keep independent copies for
            // the optional background diarization pass so the standard
            // transcript/summary completion never waits on speaker inference.
            let microphone_diarization_path = microphone_path.clone();
            let system_diarization_path = system_path.clone();
            match transcribe_and_summarize_meeting(
                &ah,
                Arc::clone(&tm),
                &settings,
                prompt_id,
                microphone_path,
                system_path,
            )
            .await
            {
                Ok(completed) => {
                    let transcript = completed.transcript;
                    let summary_opt = completed.summary;
                    let transcript_segments = transcript.history_segments();

                    if let Some(entry_id) = history_entry_id {
                        if let Err(error) = hm.update_meeting_transcription_with_timed_segments(
                            entry_id,
                            transcript.text.clone(),
                            summary_opt.clone(),
                            Some(prompt_id.to_string()),
                            Some(transcript_segments.clone()),
                            None,
                        ) {
                            error!("Failed to update meeting history entry: {error}");
                        }
                    }
                    if !completed.display_summary.is_empty() {
                        let _ = ah.emit(
                            "meeting-summary",
                            MeetingSummaryPayload {
                                summary: completed.display_summary,
                                transcript: transcript.text.clone(),
                            },
                        );
                    }
                    queue_optional_meeting_diarization(
                        settings.meeting_diarization_enabled,
                        Arc::clone(&hm),
                        history_entry_id,
                        diarization_model_manager,
                        artifacts.manifest.clone(),
                        microphone_diarization_path,
                        system_diarization_path,
                        transcript,
                        transcript_segments,
                    );
                }
                Err(error) => {
                    error!("Track-aware meeting transcription failed; source tracks remain saved: {error}");
                }
            }
            change_tray_icon(&ah, TrayIconState::Idle);
        });

        debug!("MeetingAction::stop completed in {:?}", stop_time.elapsed());
    }
}
// Test Action
struct TestAction;

impl ShortcutAction for TestAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Started - {} (App: {})",
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Stopped - {} (App: {})",
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }
}

// Static Action Map
pub static ACTION_MAP: Lazy<HashMap<String, Arc<dyn ShortcutAction>>> = Lazy::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "transcribe".to_string(),
        Arc::new(TranscribeAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "transcribe_with_post_process".to_string(),
        Arc::new(TranscribeAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "cancel".to_string(),
        Arc::new(CancelAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "meeting".to_string(),
        Arc::new(MeetingAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "test".to_string(),
        Arc::new(TestAction) as Arc<dyn ShortcutAction>,
    );
    map
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deobfuscate_recovers_original_string() {
        let original = "test-api-key-12345";
        let obfuscated: Vec<u8> = original
            .as_bytes()
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ XOR_KEY[i % XOR_KEY.len()])
            .collect();

        let recovered = deobfuscate(&obfuscated);
        assert_eq!(recovered, Some(original.to_string()));
    }

    #[test]
    fn test_deobfuscate_empty_input() {
        assert_eq!(deobfuscate(&[]), None);
    }

    #[test]
    fn test_runtime_environment_fallback() {
        let fallback_result =
            resolve_google_api_key(None, Some("runtime-fallback-key-test".to_string()), None);
        assert_eq!(fallback_result, "runtime-fallback-key-test");
    }

    #[test]
    fn meeting_tracks_merge_by_timestamp_not_source_length() {
        let microphone = crate::malayalam_asr::MalayalamTranscription {
            text: "local first local later".to_string(),
            words: vec![
                crate::malayalam_asr::TimedWord {
                    text: "local".to_string(),
                    start_ms: 100,
                    end_ms: 200,
                },
                crate::malayalam_asr::TimedWord {
                    text: "later".to_string(),
                    start_ms: 900,
                    end_ms: 1_000,
                },
            ],
        };
        let system = crate::malayalam_asr::MalayalamTranscription {
            text: "remote".to_string(),
            words: vec![crate::malayalam_asr::TimedWord {
                text: "remote".to_string(),
                start_ms: 500,
                end_ms: 700,
            }],
        };

        let merged = merge_meeting_transcripts(microphone, system);
        assert_eq!(merged.text, "local remote later");
        assert_eq!(merged.words.len(), 3);
        assert_eq!(merged.words[1].source, CaptureSource::System);
        assert_eq!(merged.diarization_words()[1].start_ms, 500);
    }

    #[test]
    fn meeting_word_joining_keeps_closing_punctuation_attached() {
        let mut output = String::new();
        for word in ["hello", ",", "world", "!"] {
            append_meeting_word(&mut output, word);
        }
        assert_eq!(output, "hello, world!");
    }

    #[test]
    fn meeting_history_segments_retain_source_and_shared_clock() {
        let transcript = MeetingTranscript {
            text: "local remote".to_string(),
            words: vec![
                MeetingTranscriptWord {
                    source: CaptureSource::Microphone,
                    text: "local".to_string(),
                    start_ms: 125,
                    end_ms: 260,
                },
                MeetingTranscriptWord {
                    source: CaptureSource::System,
                    text: "remote".to_string(),
                    start_ms: 400,
                    end_ms: 540,
                },
            ],
        };

        let segments = transcript.history_segments();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].source, "microphone");
        assert_eq!(segments[0].start_ms, 125);
        assert_eq!(segments[1].source, "system");
        assert_eq!(segments[1].end_ms, 540);
    }

    #[test]
    fn samples_to_meeting_clock_rounds_half_up() {
        assert_eq!(samples_to_ms(0), 0);
        assert_eq!(samples_to_ms(8), 1);
        assert_eq!(samples_to_ms(16_000), 1_000);
    }
}
