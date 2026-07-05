use crate::actions::{process_transcription_output, DiarizedTurn};
use crate::managers::{
    diarization::DiarizationManager,
    history::{HistoryManager, PaginatedHistory},
    transcription::TranscriptionManager,
};
use std::sync::Arc;
use tauri::{AppHandle, State, Manager};

#[tauri::command]
#[specta::specta]
pub async fn process_local_file(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    path: String,
    action: String, // "transcribe" or "meeting"
) -> Result<i64, String> {
    let source_path = std::path::Path::new(&path);
    if !source_path.exists() {
        return Err(format!("File does not exist: {}", path));
    }

    let file_name = format!("thegai-{}.wav", chrono::Utc::now().timestamp());
    let dest_path = history_manager.recordings_dir().join(&file_name);

    // For now, we only support WAV or we attempt to read samples and save as WAV.
    // Use read_wav_samples_with_rate to get both samples and the actual sample rate.
    // In the future, this should decode mp3/flac using rodio.
    let (samples, source_sample_rate) = crate::audio_toolkit::read_wav_samples_with_rate(&source_path).map_err(|e| {
        format!(
            "Failed to read audio file (only WAV is supported currently): {}",
            e
        )
    })?;

    if samples.is_empty() {
        return Err("Audio file contains no samples".to_string());
    }

    // Save as WAV into our recordings folder
    crate::audio_toolkit::save_wav_file(&dest_path, &samples)
        .map_err(|e| format!("Failed to save audio to recordings: {}", e))?;

    let is_meeting = action == "meeting";

    // Create the history entry initially with empty text
    history_manager
        .save_entry_with_diarization(
            file_name.clone(),
            String::new(),
            is_meeting,
            None,
            if is_meeting {
                Some("default_meeting_summary".to_string())
            } else {
                None
            },
            None,
        )
        .map_err(|e| format!("Failed to create history entry: {}", e))?;

    let (transcription, diarization_json) = if is_meeting {
        let dm = app.state::<Arc<DiarizationManager>>();
        dm.init().await.map_err(|e| format!("Failed to init diarization: {}", e))?;
        let diarization_res = dm.diarize(&samples, source_sample_rate).await.map_err(|e| format!("Diarization failed: {}", e))?;
        let merged_turns = crate::actions::merge_short_speaker_turns(&diarization_res.segments);

        let mut diarized_turns = Vec::new();
        if !merged_turns.is_empty() {
            for (spk_id, start, end) in merged_turns {
                let start_sample = (start * source_sample_rate as f64) as usize;
                let end_sample = (end * source_sample_rate as f64) as usize;
                let start_sample = start_sample.min(samples.len());
                let end_sample = end_sample.min(samples.len());
                if start_sample >= end_sample {
                    continue;
                }
                let slice = samples[start_sample..end_sample].to_vec();
                if slice.is_empty() {
                    continue;
                }

                let tm = Arc::clone(&transcription_manager);
                let transcribe_res = tauri::async_runtime::spawn_blocking(move || {
                    tm.transcribe(slice)
                })
                .await;

                match transcribe_res {
                    Ok(Ok(res)) => {
                        let text = res.text.trim().to_string();
                        if !text.is_empty() {
                            diarized_turns.push(DiarizedTurn {
                                speaker_id: spk_id,
                                start,
                                end,
                                text,
                            });
                        }
                    }
                    Ok(Err(e)) => {
                        log::warn!("Transcription error for speaker {} segment [{}, {}]: {}", spk_id, start, end, e);
                    }
                    Err(e) => {
                        log::warn!("Async task error for speaker {} segment [{}, {}]: {}", spk_id, start, end, e);
                    }
                }
            }
        }

        // Fallback to transcribing whole audio if no turns text found
        if diarized_turns.is_empty() {
            let duration = samples.len() as f64 / source_sample_rate as f64;
            let tm = Arc::clone(&transcription_manager);
            let samples_clone = samples.clone();
            let transcribe_res = tauri::async_runtime::spawn_blocking(move || {
                tm.transcribe(samples_clone)
            })
            .await;

            match transcribe_res {
                Ok(Ok(res)) => {
                    let text = res.text.trim().to_string();
                    if !text.is_empty() {
                        diarized_turns.push(DiarizedTurn {
                            speaker_id: 0,
                            start: 0.0,
                            end: duration,
                            text,
                        });
                    }
                }
                Ok(Err(e)) => {
                    log::error!("Transcription error in fallback path: {}", e);
                }
                Err(e) => {
                    log::error!("Async task error in fallback path: {}", e);
                }
            }
        }

        // If still no transcript was produced, return an error instead of empty string
        if diarized_turns.is_empty() {
            return Err("Failed to transcribe meeting audio: no text segments produced".to_string());
        }
        // Construct final transcript
        let mut final_transcript = String::new();
        for turn in &diarized_turns {
            final_transcript.push_str(&format!(
                "Speaker {}: {}\n\n",
                turn.speaker_id + 1,
                turn.text
            ));
        }
        let final_transcript = final_transcript.trim().to_string();
        let diarization_json = serde_json::to_string(&diarized_turns).ok();
        (final_transcript, diarization_json)
    } else {
        // Transcribe whole audio
        let tm = Arc::clone(&transcription_manager);
        let transcription_res = tauri::async_runtime::spawn_blocking(move || tm.transcribe(samples))
            .await
            .map_err(|e| format!("Transcription task panicked: {}", e))?
            .map(|r| r.text)
            .map_err(|e| e.to_string())?;
        (transcription_res, None)
    };

    let (post_processed_text, post_process_prompt) = if is_meeting {
        // For meetings, we want to force post-processing with the summary prompt.
        let settings = crate::settings::get_settings(&app);
        let prompt_id = if settings.google_oauth_token.is_some() {
            "default_meeting_notes_with_actions"
        } else {
            "default_meeting_summary"
        };
        let summary_opt =
            crate::actions::run_specific_llm_prompt(&settings, prompt_id, &transcription).await;
        (summary_opt, Some(prompt_id.to_string()))
    } else {
        let processed = process_transcription_output(&app, &transcription, false).await;
        (processed.post_processed_text, processed.post_process_prompt)
    };

    // Update the entry in the DB. Since we don't have the ID easily, we can find it by file_name.
    // We query the latest entries to find the one we just created.
    if let Ok(paginated) = history_manager.get_history_entries(None, Some(20)).await {
        if let Some(entry) = paginated
            .entries
            .into_iter()
            .find(|e| e.file_name == file_name)
        {
            history_manager
                .update_transcription(
                    entry.id,
                    transcription,
                    post_processed_text,
                    post_process_prompt,
                    diarization_json,
                )
                .map_err(|e| e.to_string())?;
            return Ok(entry.id);
        }
    }

    Ok(-1)
}

#[tauri::command]
#[specta::specta]
pub async fn get_history_entries(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    cursor: Option<i64>,
    limit: Option<usize>,
) -> Result<PaginatedHistory, String> {
    history_manager
        .get_history_entries(cursor, limit)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn toggle_history_entry_saved(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager
        .toggle_saved_status(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_audio_file_path(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    file_name: String,
) -> Result<String, String> {
    let path = history_manager.get_audio_file_path(&file_name);
    path.to_str()
        .ok_or_else(|| "Invalid file path".to_string())
        .map(|s| s.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_history_entry(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager
        .delete_entry(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn retry_history_entry_transcription(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    id: i64,
) -> Result<(), String> {
    let entry = history_manager
        .get_entry_by_id(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("History entry {} not found", id))?;

    let audio_path = history_manager.get_audio_file_path(&entry.file_name);
    let samples = crate::audio_toolkit::read_wav_samples(&audio_path)
        .map_err(|e| format!("Failed to load audio: {}", e))?;

    if samples.is_empty() {
        return Err("Recording has no audio samples".to_string());
    }

    transcription_manager.initiate_model_load();

    let tm = Arc::clone(&transcription_manager);
    let transcription = tauri::async_runtime::spawn_blocking(move || tm.transcribe(samples))
        .await
        .map_err(|e| format!("Transcription task panicked: {}", e))?
        .map(|r| r.text)
        .map_err(|e| e.to_string())?;

    if transcription.is_empty() {
        return Err("Recording contains no speech".to_string());
    }

    let processed =
        process_transcription_output(&app, &transcription, entry.post_process_requested).await;
    history_manager
        .update_transcription(
            id,
            transcription,
            processed.post_processed_text,
            processed.post_process_prompt,
            None,
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn update_history_limit(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    limit: usize,
) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.history_limit = limit;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn update_recording_retention_period(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    period: String,
) -> Result<(), String> {
    use crate::settings::RecordingRetentionPeriod;

    let retention_period = match period.as_str() {
        "never" => RecordingRetentionPeriod::Never,
        "preserve_limit" => RecordingRetentionPeriod::PreserveLimit,
        "days3" => RecordingRetentionPeriod::Days3,
        "weeks2" => RecordingRetentionPeriod::Weeks2,
        "months3" => RecordingRetentionPeriod::Months3,
        _ => return Err(format!("Invalid retention period: {}", period)),
    };

    let mut settings = crate::settings::get_settings(&app);
    settings.recording_retention_period = retention_period;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn ask_meeting_question(
    app: AppHandle,
    transcript: String,
    question: String,
) -> Result<String, String> {
    let settings = crate::settings::get_settings(&app);
    let provider = match settings.active_post_process_provider().cloned() {
        Some(provider) => provider,
        None => return Err("No LLM provider selected".to_string()),
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    if model.trim().is_empty() {
        return Err(format!(
            "No model configured for provider '{}'",
            provider.id
        ));
    }

    let api_key = settings
        .post_process_api_keys
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    let system_prompt = format!(
        "You are a helpful meeting assistant. Use the following meeting transcript as context to answer the user's question accurately. If the information is not in the transcript, say so.\n\nTRANSCRIPT:\n{}",
        transcript
    );

    // Reuse reasoning config logic from actions.rs
    let (reasoning_effort, reasoning) = match provider.id.as_str() {
        "custom" | "google" => (Some("none".to_string()), None),
        "openrouter" => (
            None,
            Some(crate::llm_client::ReasoningConfig {
                effort: Some("none".to_string()),
                exclude: Some(true),
            }),
        ),
        _ => (None, None),
    };

    match crate::llm_client::send_chat_completion_with_schema(
        &provider,
        api_key,
        &model,
        question,
        Some(system_prompt),
        None, // No schema, we want natural language response
        reasoning_effort,
        reasoning,
    )
    .await
    {
        Ok(Some(content)) => Ok(content),
        Ok(None) => Err("LLM returned an empty response".to_string()),
        Err(e) => Err(e),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn regenerate_meeting_summary(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
    labeled_transcript: String,
) -> Result<(), String> {
    let settings = crate::settings::get_settings(&app);
    let prompt_id = if settings.google_oauth_token.is_some() {
        "default_meeting_notes_with_actions"
    } else {
        "default_meeting_summary"
    };

    let summary_opt =
        crate::actions::run_specific_llm_prompt(&settings, prompt_id, &labeled_transcript).await;

    // Only update if we successfully generated a new summary
    if summary_opt.is_none() {
        return Err("Failed to generate meeting summary: LLM returned no content".to_string());
    }

    // Save the new summary in history, preserving the existing diarization_json.
    history_manager
        .update_transcription_preserve_diarization(
            id,
            labeled_transcript,
            summary_opt,
            Some(prompt_id.to_string()),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}
