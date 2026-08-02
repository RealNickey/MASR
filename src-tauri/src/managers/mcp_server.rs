use crate::credentials;
use crate::managers::history::{HistoryEntry, HistoryManager};
use crate::managers::model::ModelManager;
use crate::managers::rag::{RagManager, RagSearchHit};
use anyhow::{anyhow, Result};
use axum::{
    extract::State,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use rand::{distributions::Alphanumeric, Rng};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo, ToolAnnotations},
    schemars::JsonSchema,
    tool, tool_handler, tool_router, Json, ServerHandler,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

const DEFAULT_PORT: u16 = 8787;
const MAX_LIST_LIMIT: usize = 50;
const MAX_AUDIO_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListModelsRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListModelsOutput {
    pub models: Vec<DownloadedModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DownloadedModel {
    pub id: String,
    pub name: String,
    pub engine_type: String,
    pub supported_languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListRecordingsRequest {
    pub cursor: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordingSummary {
    pub id: i64,
    pub title: String,
    pub timestamp: i64,
    pub kind: String,
    pub has_transcript: bool,
    pub has_summary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListRecordingsOutput {
    pub recordings: Vec<RecordingSummary>,
    pub next_cursor: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetRecordingRequest {
    pub recording_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordingOutput {
    pub id: i64,
    pub title: String,
    pub timestamp: i64,
    pub kind: String,
    pub transcript: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchMemoryRequest {
    pub query: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchMemoryOutput {
    pub results: Vec<RagSearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StartTranscriptionRequest {
    pub source_path: String,
    pub model_id: String,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StartTranscriptionOutput {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetJobRequest {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobOutput {
    pub job_id: String,
    pub status: String,
    pub recording_id: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordingIdRequest {
    pub recording_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionOutput {
    pub success: bool,
}

/// Internal job bookkeeping; never serialized (JobOutput is the wire type).
#[derive(Debug, Clone)]
struct JobState {
    status: String,
    recording_id: Option<i64>,
    error: Option<String>,
    created_at: std::time::Instant,
}

#[derive(Clone)]
struct JobManager {
    active: Arc<AtomicBool>,
    jobs: Arc<Mutex<HashMap<String, JobState>>>,
}

impl JobManager {
    fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn get(&self, job_id: &str) -> Option<JobOutput> {
        self.jobs.lock().ok()?.get(job_id).map(|state| JobOutput {
            job_id: job_id.to_string(),
            status: state.status.clone(),
            recording_id: state.recording_id,
            error: state.error.clone(),
        })
    }

    fn set(&self, job_id: &str, state: JobState) {
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.insert(job_id.to_string(), state);
            if jobs.len() > 32 {
                // Evict the oldest job that is not currently running so an
                // in-flight job can never be dropped from the map mid-run.
                if let Some(oldest) = jobs
                    .iter()
                    .filter(|(_, state)| state.status != "running")
                    .min_by_key(|(_, state)| state.created_at)
                    .map(|(id, _)| id.clone())
                {
                    jobs.remove(&oldest);
                }
            }
        }
    }

    fn start(
        &self,
        app: AppHandle,
        history: Arc<HistoryManager>,
        model_manager: Arc<ModelManager>,
        source_path: PathBuf,
        model_id: String,
        kind: String,
    ) -> Result<String> {
        if self.active.swap(true, Ordering::AcqRel) {
            return Err(anyhow!("Another MCP transcription job is already running"));
        }
        let job_id = random_id();
        let now = std::time::Instant::now();
        self.set(
            &job_id,
            JobState {
                status: "queued".to_string(),
                recording_id: None,
                error: None,
                created_at: now,
            },
        );
        let jobs = self.clone();
        let job_id_for_task = job_id.clone();
        tauri::async_runtime::spawn(async move {
            jobs.set(
                &job_id_for_task,
                JobState {
                    status: "running".to_string(),
                    recording_id: None,
                    error: None,
                    created_at: now,
                },
            );
            let result = run_transcription_job(
                &app,
                &history,
                model_manager.clone(),
                &source_path,
                &model_id,
                &kind,
            )
            .await;
            match result {
                Ok(recording_id) => jobs.set(
                    &job_id_for_task,
                    JobState {
                        status: "completed".to_string(),
                        recording_id: Some(recording_id),
                        error: None,
                        created_at: now,
                    },
                ),
                Err(error) => jobs.set(
                    &job_id_for_task,
                    JobState {
                        status: "failed".to_string(),
                        recording_id: None,
                        error: Some(error.to_string()),
                        created_at: now,
                    },
                ),
            }
            jobs.active.store(false, Ordering::Release);
        });
        Ok(job_id)
    }
}

#[derive(Clone)]
struct McpContext {
    app: AppHandle,
    history: Arc<HistoryManager>,
    model_manager: Arc<ModelManager>,
    rag: Arc<RagManager>,
    jobs: JobManager,
}

#[derive(Clone)]
struct McpToolServer {
    context: McpContext,
    tool_router: ToolRouter<Self>,
}

impl McpToolServer {
    fn new(context: McpContext) -> Self {
        let mut tool_router = Self::tool_router();
        for (name, route) in &mut tool_router.map {
            let read_only = matches!(
                name.as_ref(),
                "masr_list_downloaded_models"
                    | "masr_list_recordings"
                    | "masr_get_recording"
                    | "masr_search_memory"
                    | "masr_get_transcription_job"
            );
            route.attr.annotations = Some(
                ToolAnnotations::new()
                    .read_only(read_only)
                    .destructive(!read_only && name.as_ref() != "masr_start_transcription")
                    .idempotent(matches!(
                        name.as_ref(),
                        "masr_clear_summary" | "masr_delete_recording"
                    ))
                    .open_world(false),
            );
        }
        Self {
            context,
            tool_router,
        }
    }
}

#[tool_router]
impl McpToolServer {
    #[tool(
        name = "masr_list_downloaded_models",
        description = "List installed MASR transcription models that an agent may select."
    )]
    async fn list_downloaded_models(
        &self,
        Parameters(_request): Parameters<ListModelsRequest>,
    ) -> Result<Json<ListModelsOutput>, String> {
        let models = self
            .context
            .model_manager
            .get_available_models()
            .into_iter()
            .filter(|model| model.is_downloaded)
            .map(|model| DownloadedModel {
                id: model.id,
                name: model.name,
                engine_type: format!("{:?}", model.engine_type),
                supported_languages: model.supported_languages,
            })
            .collect();
        Ok(Json(ListModelsOutput { models }))
    }

    #[tool(
        name = "masr_list_recordings",
        description = "List transcript and summary records with cursor pagination."
    )]
    async fn list_recordings(
        &self,
        Parameters(request): Parameters<ListRecordingsRequest>,
    ) -> Result<Json<ListRecordingsOutput>, String> {
        let limit = request.limit.unwrap_or(20).clamp(1, MAX_LIST_LIMIT);
        let page = self
            .context
            .history
            .get_history_entries(request.cursor, Some(limit + 1))
            .await
            .map_err(|error| error.to_string())?;
        let has_more = page.entries.len() > limit;
        let mut entries = page.entries;
        if has_more {
            entries.truncate(limit);
        }
        let next_cursor = has_more
            .then(|| entries.last().map(|entry| entry.id))
            .flatten();
        Ok(Json(ListRecordingsOutput {
            recordings: entries.iter().map(recording_summary).collect(),
            next_cursor,
        }))
    }

    #[tool(
        name = "masr_get_recording",
        description = "Read the full transcript and saved summary for one MASR record."
    )]
    async fn get_recording(
        &self,
        Parameters(request): Parameters<GetRecordingRequest>,
    ) -> Result<Json<RecordingOutput>, String> {
        let entry = self
            .context
            .history
            .get_entry_by_id(request.recording_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("Recording {} not found", request.recording_id))?;
        Ok(Json(recording_output(&entry)))
    }

    #[tool(
        name = "masr_search_memory",
        description = "Search local Gemini-vector meeting memory and return cited excerpts."
    )]
    async fn search_memory(
        &self,
        Parameters(request): Parameters<SearchMemoryRequest>,
    ) -> Result<Json<SearchMemoryOutput>, String> {
        let results = self
            .context
            .rag
            .search(&request.query, request.limit.unwrap_or(5), None)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Json(SearchMemoryOutput { results }))
    }

    #[tool(
        name = "masr_start_transcription",
        description = "Transcribe a local audio file with any downloaded MASR model without changing app settings."
    )]
    async fn start_transcription(
        &self,
        Parameters(request): Parameters<StartTranscriptionRequest>,
    ) -> Result<Json<StartTranscriptionOutput>, String> {
        let kind = request.kind.unwrap_or_else(|| "transcription".to_string());
        if kind != "transcription" && kind != "meeting" {
            return Err("kind must be 'transcription' or 'meeting'".to_string());
        }
        let model = self
            .context
            .model_manager
            .get_model_info(&request.model_id)
            .ok_or_else(|| format!("Model '{}' not found", request.model_id))?;
        if !model.is_downloaded {
            return Err(format!("Model '{}' is not downloaded", request.model_id));
        }
        let source_path = validate_audio_path(&request.source_path)?;
        let job_id = self
            .context
            .jobs
            .start(
                self.context.app.clone(),
                self.context.history.clone(),
                self.context.model_manager.clone(),
                source_path,
                request.model_id,
                kind,
            )
            .map_err(|error| error.to_string())?;
        Ok(Json(StartTranscriptionOutput { job_id }))
    }

    #[tool(
        name = "masr_get_transcription_job",
        description = "Read the status and resulting recording ID for an MCP transcription job."
    )]
    async fn get_transcription_job(
        &self,
        Parameters(request): Parameters<GetJobRequest>,
    ) -> Result<Json<JobOutput>, String> {
        self.context
            .jobs
            .get(&request.job_id)
            .map(Json)
            .ok_or_else(|| format!("Transcription job '{}' not found", request.job_id))
    }

    #[tool(
        name = "masr_clear_summary",
        description = "Delete only the saved summary for one recording."
    )]
    async fn clear_summary(
        &self,
        Parameters(request): Parameters<RecordingIdRequest>,
    ) -> Result<Json<ActionOutput>, String> {
        // Remove the vector chunks first so a RAG failure cannot leave the DB
        // summary cleared while stale summary chunks remain searchable.
        self.context
            .rag
            .remove_source(request.recording_id, "summary")
            .await
            .map_err(|error| error.to_string())?;
        self.context
            .history
            .clear_summary(request.recording_id)
            .map_err(|error| error.to_string())?;
        Ok(Json(ActionOutput { success: true }))
    }

    #[tool(
        name = "masr_delete_recording",
        description = "Delete a recording, its transcript, summary, retained audio, and vectors."
    )]
    async fn delete_recording(
        &self,
        Parameters(request): Parameters<RecordingIdRequest>,
    ) -> Result<Json<ActionOutput>, String> {
        self.context
            .history
            .delete_entry(request.recording_id)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Json(ActionOutput { success: true }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpToolServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "MASR local transcript, summary, model, transcription, and meeting-memory tools",
        )
    }
}

#[derive(Clone)]
struct AuthState {
    token: String,
    port: u16,
}

async fn auth_middleware(
    State(state): State<AuthState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let valid_origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(|origin| {
            origin == format!("http://127.0.0.1:{}", state.port)
                || origin == format!("http://localhost:{}", state.port)
        })
        .unwrap_or(true);
    let valid_token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value == state.token);
    if !valid_origin {
        return (StatusCode::FORBIDDEN, "Origin is not allowed").into_response();
    }
    if !valid_token {
        return (StatusCode::UNAUTHORIZED, "Bearer token required").into_response();
    }
    next.run(request).await
}

struct ServerRuntime {
    port: u16,
    cancellation: CancellationToken,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct McpServerStatus {
    pub enabled: bool,
    pub running: bool,
    pub port: u16,
    pub endpoint: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct McpConnectionInfo {
    pub endpoint: String,
    pub bearer_token: String,
}

pub struct McpServerManager {
    app: AppHandle,
    context: McpContext,
    token: Mutex<String>,
    runtime: Mutex<Option<ServerRuntime>>,
    last_error: Mutex<Option<String>>,
}

impl McpServerManager {
    pub fn new(
        app: &AppHandle,
        history: Arc<HistoryManager>,
        model_manager: Arc<ModelManager>,
        rag: Arc<RagManager>,
    ) -> Arc<Self> {
        let token = load_or_create_token(app);
        Arc::new(Self {
            app: app.clone(),
            context: McpContext {
                app: app.clone(),
                history,
                model_manager,
                rag,
                jobs: JobManager::new(),
            },
            token: Mutex::new(token),
            runtime: Mutex::new(None),
            last_error: Mutex::new(None),
        })
    }

    pub fn sync_from_settings(&self, app: &AppHandle) {
        let settings = crate::settings::get_settings(app);
        self.clear_error();
        self.stop();
        if settings.mcp_server_enabled {
            self.start(settings.mcp_server_port);
        }
    }

    fn start(&self, port: u16) {
        let token = self
            .token
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        if token.is_empty() {
            self.set_error("MCP bearer token is unavailable".to_string());
            return;
        }
        let cancellation = CancellationToken::new();
        let runtime_token = cancellation.clone();
        let context = self.context.clone();
        let endpoint = format!("http://127.0.0.1:{}/mcp", port);
        // A previous runtime may still be releasing its socket after a stop()
        // during token rotation or port changes; retry briefly instead of
        // leaving the server down with an AddrInUse error.
        let mut bind_error: Option<std::io::Error> = None;
        let mut std_listener = None;
        for attempt in 0..10 {
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => {
                    std_listener = Some(listener);
                    break;
                }
                Err(error) if attempt < 9 => {
                    std::thread::sleep(Duration::from_millis(100));
                    bind_error = Some(error);
                }
                Err(error) => bind_error = Some(error),
            }
        }
        let std_listener = match std_listener {
            Some(listener) => listener,
            None => {
                self.set_error(format!(
                    "Failed to bind MCP server at {}: {}",
                    endpoint,
                    bind_error
                        .as_ref()
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "unknown error".to_string())
                ));
                return;
            }
        };
        if let Err(error) = std_listener.set_nonblocking(true) {
            self.set_error(format!("Failed to configure MCP listener: {}", error));
            return;
        }
        let listener = match tokio::net::TcpListener::from_std(std_listener) {
            Ok(listener) => listener,
            Err(error) => {
                self.set_error(format!("Failed to initialize MCP listener: {}", error));
                return;
            }
        };
        self.clear_error();
        tauri::async_runtime::spawn(async move {
            let service = rmcp::transport::streamable_http_server::StreamableHttpService::new(
                move || Ok(McpToolServer::new(context.clone())),
                Arc::new(
                    rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
                ),
                rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
                    .with_allowed_hosts([
                        format!("127.0.0.1:{}", port),
                        format!("localhost:{}", port),
                    ])
                    .with_allowed_origins([
                        format!("http://127.0.0.1:{}", port),
                        format!("http://localhost:{}", port),
                    ])
                    .with_json_response(true)
                    .with_cancellation_token(runtime_token.clone()),
            );
            let app =
                Router::new()
                    .nest_service("/mcp", service)
                    .layer(middleware::from_fn_with_state(
                        AuthState { token, port },
                        auth_middleware,
                    ));
            log::info!("MCP server listening at {}", endpoint);
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(runtime_token.cancelled_owned())
                .await;
        });
        if let Ok(mut runtime) = self.runtime.lock() {
            *runtime = Some(ServerRuntime { port, cancellation });
        }
    }

    fn stop(&self) {
        if let Ok(mut runtime) = self.runtime.lock() {
            if let Some(runtime) = runtime.take() {
                runtime.cancellation.cancel();
            }
        }
    }

    fn set_error(&self, error: String) {
        if let Ok(mut last_error) = self.last_error.lock() {
            *last_error = Some(error);
        }
    }

    fn clear_error(&self) {
        if let Ok(mut last_error) = self.last_error.lock() {
            *last_error = None;
        }
    }

    pub fn status(&self) -> McpServerStatus {
        let settings = crate::settings::get_settings(&self.app);
        let runtime = self
            .runtime
            .lock()
            .ok()
            .and_then(|runtime| runtime.as_ref().map(|runtime| runtime.port));
        McpServerStatus {
            enabled: settings.mcp_server_enabled,
            running: runtime.is_some(),
            port: settings.mcp_server_port,
            endpoint: format!("http://127.0.0.1:{}/mcp", settings.mcp_server_port),
            error: self.last_error.lock().ok().and_then(|error| error.clone()),
        }
    }

    pub fn connection_info(&self) -> McpConnectionInfo {
        let settings = crate::settings::get_settings(&self.app);
        McpConnectionInfo {
            endpoint: format!("http://127.0.0.1:{}/mcp", settings.mcp_server_port),
            bearer_token: self
                .token
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default(),
        }
    }

    pub fn rotate_token(&self) -> McpConnectionInfo {
        let token = generate_token();
        persist_token(&self.app, &token);
        if let Ok(mut current) = self.token.lock() {
            *current = token;
        }
        self.sync_from_settings(&self.app);
        self.connection_info()
    }
}

async fn run_transcription_job(
    app: &AppHandle,
    history: &HistoryManager,
    model_manager: Arc<ModelManager>,
    source_path: &Path,
    model_id: &str,
    kind: &str,
) -> Result<i64> {
    let source_path = source_path.to_path_buf();
    let samples = tauri::async_runtime::spawn_blocking(move || {
        crate::audio_toolkit::read_any_audio_file(&source_path)
    })
    .await
    .map_err(|error| anyhow!("Audio decoding task panicked: {}", error))??;
    if samples.is_empty() {
        return Err(anyhow!("Audio file contains no samples"));
    }
    let file_name = format!("mcp-{}.wav", random_id());
    let dest_path = history.recordings_dir().join(&file_name);
    crate::audio_toolkit::save_wav_file(&dest_path, &samples)?;
    let is_meeting = kind == "meeting";
    let entry = history.save_entry(
        file_name,
        String::new(),
        is_meeting,
        None,
        is_meeting.then(|| "default_meeting_summary".to_string()),
    )?;
    let app = app.clone();
    let model_id = model_id.to_string();
    let entry_id = entry.id;
    let transcription_result = tauri::async_runtime::spawn_blocking(move || {
        crate::managers::transcription::transcribe_isolated(
            &app,
            &model_manager,
            &model_id,
            samples,
        )
    })
    .await
    .map_err(|error| anyhow!("Transcription task panicked: {}", error))?;
    let transcription = match transcription_result {
        Ok(result) => result.text,
        Err(error) => {
            // The WAV and DB entry were committed before inference; remove them
            // so failed jobs do not accumulate orphaned recordings.
            log::warn!(
                "MCP transcription failed for entry {}; cleaning up: {}",
                entry_id,
                error
            );
            let _ = history.delete_entry(entry_id).await;
            return Err(error);
        }
    };
    if let Err(error) =
        history.update_transcription(entry_id, transcription, None, entry.post_process_prompt)
    {
        log::warn!(
            "MCP transcription finished but saving failed for entry {}; cleaning up: {}",
            entry_id,
            error
        );
        let _ = history.delete_entry(entry_id).await;
        return Err(error.into());
    }
    Ok(entry_id)
}

fn validate_audio_path(path: &str) -> Result<PathBuf> {
    let input = Path::new(path);
    if !input.is_absolute() {
        return Err(anyhow!("source_path must be an absolute local path"));
    }
    let canonical = std::fs::canonicalize(input)
        .map_err(|error| anyhow!("Cannot access source audio file: {}", error))?;
    let metadata = std::fs::metadata(&canonical)?;
    if !metadata.is_file() {
        return Err(anyhow!("source_path must point to a regular file"));
    }
    if metadata.len() > MAX_AUDIO_BYTES {
        return Err(anyhow!("source audio file exceeds the 2 GiB limit"));
    }
    Ok(canonical)
}

fn recording_summary(entry: &HistoryEntry) -> RecordingSummary {
    RecordingSummary {
        id: entry.id,
        title: entry.title.clone(),
        timestamp: entry.timestamp,
        kind: if HistoryManager::is_meeting_entry(entry) {
            "meeting".to_string()
        } else {
            "transcription".to_string()
        },
        has_transcript: !entry.transcription_text.is_empty(),
        has_summary: entry
            .post_processed_text
            .as_deref()
            .is_some_and(|summary| !summary.trim().is_empty()),
    }
}

fn recording_output(entry: &HistoryEntry) -> RecordingOutput {
    RecordingOutput {
        id: entry.id,
        title: entry.title.clone(),
        timestamp: entry.timestamp,
        kind: recording_summary(entry).kind,
        transcript: entry.transcription_text.clone(),
        summary: entry.post_processed_text.clone(),
    }
}

fn random_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect()
}

fn generate_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect()
}

fn load_or_create_token(app: &AppHandle) -> String {
    let settings = crate::settings::get_settings(app);
    if let Some(token) = credentials::get("mcp_server_token") {
        return token;
    }
    if let Some(token) = settings.mcp_server_token.filter(|token| !token.is_empty()) {
        return token;
    }
    let token = generate_token();
    persist_token(app, &token);
    token
}

fn persist_token(app: &AppHandle, token: &str) {
    if crate::portable::is_portable() {
        // credentials::set is a silent no-op on portable installs; without this
        // fallback a fresh token would be generated on every launch and MCP
        // clients would break after each restart.
        let mut settings = crate::settings::get_settings(app);
        settings.mcp_server_token = Some(token.to_string());
        crate::settings::write_settings(app, settings);
        return;
    }
    if let Err(error) = credentials::set("mcp_server_token", token) {
        log::warn!("Could not persist MCP token in credential vault: {}", error);
        let mut settings = crate::settings::get_settings(app);
        settings.mcp_server_token = Some(token.to_string());
        crate::settings::write_settings(app, settings);
    }
}

pub fn default_port() -> u16 {
    DEFAULT_PORT
}

#[cfg(test)]
mod tests {
    use super::{generate_token, validate_audio_path};

    #[test]
    fn generated_tokens_are_long_and_nonempty() {
        let token = generate_token();
        assert_eq!(token.len(), 64);
        assert!(token
            .chars()
            .all(|character| character.is_ascii_alphanumeric()));
    }

    #[test]
    fn audio_paths_must_be_absolute() {
        let error = validate_audio_path("relative.wav").expect_err("relative path should fail");
        assert!(error.to_string().contains("absolute"));
    }
}
