use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use log::{debug, error, info};
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use specta::Type;
use std::fs;
use std::path::{Component, Path, PathBuf};
use tauri::AppHandle;
use tauri_specta::Event;

use super::meeting_capture::{CaptureSessionManifest, MeetingCaptureSession};

/// Database migrations for transcription history.
/// Each migration is applied in order. The library tracks which migrations
/// have been applied using SQLite's user_version pragma.
///
/// Note: For users upgrading from tauri-plugin-sql, migrate_from_tauri_plugin_sql()
/// converts the old _sqlx_migrations table tracking to the user_version pragma,
/// ensuring migrations don't re-run on existing databases.
static MIGRATIONS: &[M] = &[
    M::up(
        "CREATE TABLE IF NOT EXISTS transcription_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_name TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            saved BOOLEAN NOT NULL DEFAULT 0,
            title TEXT NOT NULL,
            transcription_text TEXT NOT NULL
        );",
    ),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_processed_text TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_prompt TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_requested BOOLEAN NOT NULL DEFAULT 0;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN audio_tracks TEXT;"),
    M::up(
        "CREATE TABLE IF NOT EXISTS rag_chunks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id INTEGER NOT NULL,
            source TEXT NOT NULL,
            chunk_index INTEGER NOT NULL,
            content TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            embedding_model TEXT NOT NULL,
            dimensions INTEGER NOT NULL,
            embedding BLOB NOT NULL,
            UNIQUE(entry_id, source, chunk_index)
        );
        CREATE INDEX IF NOT EXISTS idx_rag_chunks_entry ON rag_chunks(entry_id);
    "),
    M::up(
        "ALTER TABLE transcription_history ADD COLUMN meeting_session TEXT;
         ALTER TABLE transcription_history ADD COLUMN speaker_segments TEXT;",
    ),
    M::up("ALTER TABLE transcription_history ADD COLUMN transcript_segments TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;"),
];

const DEFAULT_MEETING_SUMMARY_PROMPT_ID: &str = "default_meeting_summary";

const HISTORY_ENTRY_COLUMNS: &str = "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, audio_tracks, meeting_session, speaker_segments, transcript_segments FROM transcription_history";

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct PaginatedHistory {
    pub entries: Vec<HistoryEntry>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "action")]
pub enum HistoryUpdatePayload {
    #[serde(rename = "added")]
    Added { entry: HistoryEntry },
    #[serde(rename = "updated")]
    Updated { entry: HistoryEntry },
    #[serde(rename = "deleted")]
    Deleted { id: i64 },
    #[serde(rename = "toggled")]
    Toggled { id: i64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct HistoryEntry {
    pub id: i64,
    pub file_name: String,
    pub timestamp: i64,
    pub saved: bool,
    pub title: String,
    pub transcription_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    pub post_process_requested: bool,
    /// Relative paths for the retained meeting sources. Normal recordings and
    /// imported files deliberately keep this as `None` for backwards compatibility.
    pub audio_tracks: Option<AudioTracks>,
    /// Durable meeting-session metadata. Both paths are relative to the recordings
    /// directory, and the manifest must remain inside the session root.
    #[serde(default)]
    pub meeting_session: Option<MeetingSession>,
    /// Timestamped, speaker-labelled transcript rows. `None` preserves the
    /// unlabelled transcript behaviour used by old and imported entries.
    #[serde(default)]
    pub speaker_segments: Option<Vec<SpeakerSegment>>,
    /// Timestamped transcript rows retained independently of optional speaker
    /// diarization. `None` preserves entries created before timestamped ASR.
    #[serde(default)]
    pub transcript_segments: Option<Vec<TranscriptSegment>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, PartialEq, Eq)]
pub struct AudioTracks {
    pub mix: String,
    pub microphone: String,
    pub system: String,
}

/// Locations for a complete meeting capture session.
///
/// The paths are relative to the recordings directory. Keeping the explicit
/// session root prevents retention cleanup from mistaking a nested derived mix
/// path for the directory that owns the complete session.
#[derive(Clone, Debug, Serialize, Deserialize, Type, PartialEq, Eq)]
pub struct MeetingSession {
    pub root: String,
    pub manifest: String,
}

/// A transcript span annotated with its meeting-local speaker and source.
#[derive(Clone, Debug, Serialize, Deserialize, Type, PartialEq)]
pub struct SpeakerSegment {
    /// Start on the meeting clock, in milliseconds.
    pub start_ms: u64,
    /// Exclusive end on the meeting clock, in milliseconds.
    pub end_ms: u64,
    /// A meeting-local label such as `You`, `Remote Speaker 1`, or `Unknown`.
    pub speaker: String,
    /// Capture source such as `microphone`, `system`, `mix`, or `unknown`.
    pub source: String,
    /// Transcript text covered by this speaker interval.
    pub text: String,
    /// Optional confidence after source/speaker attribution.
    pub confidence: Option<f32>,
}

/// A timestamped ASR span on the shared meeting timeline.
///
/// These rows are persisted as soon as track-aware ASR completes, regardless
/// of whether optional diarization is enabled or produces speaker labels.
#[derive(Clone, Debug, Serialize, Deserialize, Type, PartialEq)]
pub struct TranscriptSegment {
    /// Start on the meeting clock, in milliseconds.
    pub start_ms: u64,
    /// Exclusive end on the meeting clock, in milliseconds.
    pub end_ms: u64,
    /// Capture source such as `microphone`, `system`, `mix`, or `unknown`.
    pub source: String,
    /// Transcript text covered by this timestamp interval.
    pub text: String,
    /// Optional ASR confidence in the inclusive range `0.0..=1.0`.
    pub confidence: Option<f32>,
}

fn deserialize_optional_json<T>(json: Option<String>) -> rusqlite::Result<Option<T>>
where
    T: DeserializeOwned,
{
    json.map(|json| {
        serde_json::from_str(&json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                json.len(),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    })
    .transpose()
}

pub struct HistoryManager {
    app_handle: AppHandle,
    recordings_dir: PathBuf,
    db_path: PathBuf,
}

impl HistoryManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        // Create recordings directory in app data dir
        let app_data_dir = crate::portable::app_data_dir(app_handle)?;
        let recordings_dir = app_data_dir.join("recordings");
        let db_path = app_data_dir.join("history.db");

        // Ensure recordings directory exists
        if !recordings_dir.exists() {
            fs::create_dir_all(&recordings_dir)?;
            debug!("Created recordings directory: {:?}", recordings_dir);
        }
        let manager = Self {
            app_handle: app_handle.clone(),
            recordings_dir,
            db_path,
        };

        // Initialize database and run migrations synchronously
        manager.init_database()?;

        Ok(manager)
    }

    /// Finalize checkpointed interrupted meetings at startup instead of
    /// deleting them. The capture session has already flushed source WAV
    /// headers and an alternating checkpoint before this point, so recovery
    /// can publish the durable portion as an explicitly incomplete session.
    ///
    /// Recovery deliberately does not run ASR or summarization at startup.
    /// It creates a normal history row with blank transcript text, allowing a
    /// user to inspect or retry the retained tracks without blocking launch.
    pub fn recover_abandoned_meeting_captures(&self) {
        match MeetingCaptureSession::find_recoverable_sessions(&self.recordings_dir) {
            Ok(sessions) => {
                for session in sessions {
                    let root = session.session_root.clone();
                    match MeetingCaptureSession::recover_and_finalize(&self.recordings_dir, &root) {
                        Ok(artifacts) => {
                            info!(
                                "Recovered interrupted meeting capture into {}",
                                artifacts.session_root
                            );
                        }
                        Err(error) => error!(
                            "Failed to recover interrupted meeting capture {:?}: {}",
                            root, error
                        ),
                    }
                }
            }
            Err(error) => error!(
                "Failed to discover interrupted meeting captures in {:?}: {}",
                self.recordings_dir, error
            ),
        }

        // If the process crashed after publication but before SQLite insert,
        // this second, idempotent pass registers the finalized session on the
        // next launch instead of leaving it as an inaccessible orphan.
        self.register_untracked_finalized_meeting_sessions();
    }

    fn register_untracked_finalized_meeting_sessions(&self) {
        let entries = match fs::read_dir(&self.recordings_dir) {
            Ok(entries) => entries,
            Err(error) => {
                error!(
                    "Failed to inspect finalized meeting directories in {:?}: {}",
                    self.recordings_dir, error
                );
                return;
            }
        };

        // Materialize candidate directory paths before registration to avoid
        // iterator invalidation if cleanup mutates the filesystem during
        // the registration loop.
        let candidate_paths: Vec<_> = entries
            .flatten()
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                if !file_type.is_dir() {
                    return None;
                }
                let root = entry.file_name().to_str().map(str::to_owned)?;
                if !root.starts_with("meeting-") {
                    return None;
                }
                Some(entry.path())
            })
            .collect();

        for directory in candidate_paths {
            let Some(root) = directory
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let manifest_path = directory.join("manifest.json");
            let manifest = match fs::File::open(&manifest_path)
                .ok()
                .and_then(|file| serde_json::from_reader::<_, CaptureSessionManifest>(file).ok())
            {
                Some(manifest)
                    if Self::is_finalized_meeting_session(&directory, &root, &manifest) =>
                {
                    manifest
                }
                _ => continue,
            };
            match self.history_contains_meeting_session(&root) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    error!(
                        "Failed to check finalized meeting history session {}: {}",
                        root, error
                    );
                    continue;
                }
            }
            let tracks = AudioTracks {
                mix: format!("{root}/mix.wav"),
                microphone: format!("{root}/microphone.wav"),
                system: format!("{root}/system.wav"),
            };
            let session = MeetingSession {
                root: root.clone(),
                manifest: format!("{root}/manifest.json"),
            };
            if let Err(error) = self.save_entry_with_meeting_session(
                tracks.mix.clone(),
                String::new(),
                true,
                None,
                Some(DEFAULT_MEETING_SUMMARY_PROMPT_ID.to_string()),
                tracks,
                session,
            ) {
                error!(
                    "Failed to register finalized meeting capture {} in history: {}",
                    manifest.session_id, error
                );
            }
        }
    }

    fn is_finalized_meeting_session(
        directory: &Path,
        root: &str,
        manifest: &CaptureSessionManifest,
    ) -> bool {
        manifest.final_directory == root
            && manifest.finalized_at.is_some()
            && manifest.published_at.is_some()
            && ["microphone.wav", "system.wav", "mix.wav"]
                .into_iter()
                .all(|name| directory.join(name).is_file())
    }

    fn history_contains_meeting_session(&self, root: &str) -> Result<bool> {
        let conn = self.get_connection()?;
        let mut statement = conn.prepare(
            "SELECT meeting_session FROM transcription_history
             WHERE meeting_session IS NOT NULL",
        )?;
        let sessions = statement.query_map([], |row| row.get::<_, String>(0))?;
        for session in sessions {
            let session = session?;
            if serde_json::from_str::<MeetingSession>(&session)
                .ok()
                .is_some_and(|session| session.root == root)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn init_database(&self) -> Result<()> {
        info!("Initializing database at {:?}", self.db_path);

        let mut conn = Connection::open(&self.db_path)?;

        // Handle migration from tauri-plugin-sql to rusqlite_migration
        // tauri-plugin-sql used _sqlx_migrations table, rusqlite_migration uses user_version pragma
        self.migrate_from_tauri_plugin_sql(&conn)?;

        // Create migrations object and run to latest version
        let migrations = Migrations::new(MIGRATIONS.to_vec());

        // Validate migrations in debug builds
        #[cfg(debug_assertions)]
        migrations.validate().expect("Invalid migrations");

        // Get current version before migration
        let version_before: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        debug!("Database version before migration: {}", version_before);

        // Apply any pending migrations
        migrations.to_latest(&mut conn)?;

        // Get version after migration
        let version_after: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version_after > version_before {
            info!(
                "Database migrated from version {} to {}",
                version_before, version_after
            );
        } else {
            debug!("Database already at latest version {}", version_after);
        }

        Ok(())
    }

    /// Migrate from tauri-plugin-sql's migration tracking to rusqlite_migration's.
    /// tauri-plugin-sql used a _sqlx_migrations table, while rusqlite_migration uses
    /// SQLite's user_version pragma. This function checks if the old system was in use
    /// and sets the user_version accordingly so migrations don't re-run.
    fn migrate_from_tauri_plugin_sql(&self, conn: &Connection) -> Result<()> {
        // Check if the old _sqlx_migrations table exists
        let has_sqlx_migrations: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !has_sqlx_migrations {
            return Ok(());
        }

        // Check current user_version
        let current_version: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if current_version > 0 {
            // Already migrated to rusqlite_migration system
            return Ok(());
        }

        // Get the highest version from the old migrations table
        let old_version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if old_version > 0 {
            info!(
                "Migrating from tauri-plugin-sql (version {}) to rusqlite_migration",
                old_version
            );

            // Set user_version to match the old migration state
            conn.pragma_update(None, "user_version", old_version)?;

            // Optionally drop the old migrations table (keeping it doesn't hurt)
            // conn.execute("DROP TABLE IF EXISTS _sqlx_migrations", [])?;

            info!(
                "Migration tracking converted: user_version set to {}",
                old_version
            );
        }

        Ok(())
    }

    fn get_connection(&self) -> Result<Connection> {
        Ok(Connection::open(&self.db_path)?)
    }

    fn map_history_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
        let audio_tracks = deserialize_optional_json(row.get("audio_tracks")?)?;
        let meeting_session = deserialize_optional_json(row.get("meeting_session")?)?;
        let speaker_segments = deserialize_optional_json(row.get("speaker_segments")?)?;
        let transcript_segments = deserialize_optional_json(row.get("transcript_segments")?)?;
        Ok(HistoryEntry {
            id: row.get("id")?,
            file_name: row.get("file_name")?,
            timestamp: row.get("timestamp")?,
            saved: row.get("saved")?,
            title: row.get("title")?,
            transcription_text: row.get("transcription_text")?,
            post_processed_text: row.get("post_processed_text")?,
            post_process_prompt: row.get("post_process_prompt")?,
            post_process_requested: row.get("post_process_requested")?,
            audio_tracks,
            meeting_session,
            speaker_segments,
            transcript_segments,
        })
    }

    pub fn recordings_dir(&self) -> &std::path::Path {
        &self.recordings_dir
    }

    pub fn db_path(&self) -> &std::path::Path {
        &self.db_path
    }

    pub fn is_meeting_entry(entry: &HistoryEntry) -> bool {
        matches!(
            entry.post_process_prompt.as_deref(),
            Some(DEFAULT_MEETING_SUMMARY_PROMPT_ID) | Some("default_meeting_notes_with_actions")
        )
    }

    /// Save a new history entry to the database.
    /// The WAV file should already have been written to the recordings directory.
    pub fn save_entry(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
    ) -> Result<HistoryEntry> {
        self.save_entry_with_audio_tracks(
            file_name,
            transcription_text,
            post_process_requested,
            post_processed_text,
            post_process_prompt,
            None,
        )
    }

    /// Save a history entry with optional retained source tracks. `file_name`
    /// always points at the compatible mixdown used by existing playback and
    /// transcription callers.
    pub fn save_entry_with_audio_tracks(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        audio_tracks: Option<AudioTracks>,
    ) -> Result<HistoryEntry> {
        self.save_entry_with_metadata(
            file_name,
            transcription_text,
            post_process_requested,
            post_processed_text,
            post_process_prompt,
            audio_tracks,
            None,
        )
    }

    /// Save a completed meeting capture with its retained tracks and manifest.
    ///
    /// The session is kept separate from ordinary recordings so legacy callers
    /// can continue to use [`Self::save_entry`] without creating session metadata.
    pub fn save_entry_with_meeting_session(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        audio_tracks: AudioTracks,
        meeting_session: MeetingSession,
    ) -> Result<HistoryEntry> {
        self.validate_meeting_session(&file_name, &meeting_session, &audio_tracks)?;
        self.save_entry_with_metadata(
            file_name,
            transcription_text,
            post_process_requested,
            post_processed_text,
            post_process_prompt,
            Some(audio_tracks),
            Some(meeting_session),
        )
    }

    fn save_entry_with_metadata(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        audio_tracks: Option<AudioTracks>,
        meeting_session: Option<MeetingSession>,
    ) -> Result<HistoryEntry> {
        let timestamp = Utc::now().timestamp();
        let title = self.format_timestamp_title(timestamp);

        let conn = self.get_connection()?;
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                audio_tracks,
                meeting_session,
                speaker_segments,
                transcript_segments
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                &file_name,
                timestamp,
                false,
                &title,
                &transcription_text,
                &post_processed_text,
                &post_process_prompt,
                post_process_requested,
                audio_tracks
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                meeting_session
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                Option::<String>::None,
                Option::<String>::None,
            ],
        )?;

        let entry = HistoryEntry {
            id: conn.last_insert_rowid(),
            file_name,
            timestamp,
            saved: false,
            title,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            post_process_requested,
            audio_tracks,
            meeting_session,
            speaker_segments: None,
            transcript_segments: None,
        };

        debug!("Saved history entry with id {}", entry.id);

        self.cleanup_old_entries()?;

        // Emit typed event for real-time frontend updates
        if let Err(e) = (HistoryUpdatePayload::Added {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    /// Update an existing history entry with new transcription results (used by retry).
    pub fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
    ) -> Result<HistoryEntry> {
        self.update_transcription_with_segments(
            id,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            None,
        )
    }

    /// Atomically replace a meeting transcript and its speaker-labelled rows.
    ///
    /// Passing `None` clears old labels, which prevents a retry from displaying
    /// speakers that were assigned to a previous transcript. This compatibility
    /// API deliberately preserves timestamped transcript rows; callers that
    /// replace ASR timing should use [`Self::update_meeting_transcription_with_timed_segments`].
    pub fn update_meeting_transcription(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        speaker_segments: Option<Vec<SpeakerSegment>>,
    ) -> Result<HistoryEntry> {
        self.update_transcription_with_segments(
            id,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            Some(speaker_segments),
        )
    }

    /// Atomically replace a meeting transcript, its timestamped ASR rows, and
    /// optional speaker labels.
    ///
    /// `transcript_segments` is the durable source-aware ASR output and is
    /// written even when diarization is disabled. Passing `None` explicitly
    /// clears the relevant JSON field, so retries cannot retain timestamps or
    /// labels from a previous transcript.
    ///
    /// Returns the refreshed entry together with the new monotonic revision,
    /// which callers use to guard detached speaker-label writes.
    pub fn update_meeting_transcription_with_timed_segments(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        transcript_segments: Option<Vec<TranscriptSegment>>,
        speaker_segments: Option<Vec<SpeakerSegment>>,
    ) -> Result<(HistoryEntry, i64)> {
        let mut conn = self.get_connection()?;
        let (entry, revision) =
            Self::update_meeting_transcription_with_timed_segments_in_connection(
                &mut conn,
                id,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                transcript_segments,
                speaker_segments,
            )?;

        self.emit_history_updated(entry.clone());
        Ok((entry, revision))
    }

    /// Persist optional speaker labels only when they still belong to the
    /// exact timestamped transcript that produced them.
    ///
    /// Diarization runs in a detached background task, while a user can retry
    /// the same meeting immediately. Matching the monotonic revision of the
    /// row written together with the transcript gives the background result an
    /// optimistic concurrency guard without allowing it to replace a newer
    /// retry's transcript, summary, or timing metadata.
    ///
    /// Returns `Ok(true)` when labels were written and `Ok(false)` when the
    /// entry was deleted or has since received a different transcript.
    pub fn update_meeting_speaker_segments_if_current(
        &self,
        id: i64,
        expected_revision: i64,
        speaker_segments: Vec<SpeakerSegment>,
    ) -> Result<bool> {
        let mut conn = self.get_connection()?;
        let entry = Self::update_meeting_speaker_segments_if_current_in_connection(
            &mut conn,
            id,
            expected_revision,
            speaker_segments,
        )?;

        if let Some(entry) = entry {
            self.emit_history_updated(entry);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn update_meeting_speaker_segments_if_current_in_connection(
        conn: &mut Connection,
        id: i64,
        expected_revision: i64,
        speaker_segments: Vec<SpeakerSegment>,
    ) -> Result<Option<HistoryEntry>> {
        Self::validate_speaker_segments(&speaker_segments)?;
        let speaker_segments_json = serde_json::to_string(&speaker_segments)?;
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE transcription_history
             SET speaker_segments = ?1
             WHERE id = ?2
               AND revision = ?3",
            params![speaker_segments_json, id, expected_revision],
        )?;

        if updated == 0 {
            tx.commit()?;
            return Ok(None);
        }

        let entry = tx.query_row(
            &format!("{HISTORY_ENTRY_COLUMNS} WHERE id = ?1"),
            params![id],
            Self::map_history_entry,
        )?;
        tx.commit()?;
        Ok(Some(entry))
    }

    fn update_meeting_transcription_with_timed_segments_in_connection(
        conn: &mut Connection,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        transcript_segments: Option<Vec<TranscriptSegment>>,
        speaker_segments: Option<Vec<SpeakerSegment>>,
    ) -> Result<(HistoryEntry, i64)> {
        if let Some(segments) = transcript_segments.as_deref() {
            Self::validate_transcript_segments(segments)?;
        }
        if let Some(segments) = speaker_segments.as_deref() {
            Self::validate_speaker_segments(segments)?;
        }

        let transcript_segments_json = transcript_segments
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let speaker_segments_json = speaker_segments
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE transcription_history
             SET transcription_text = ?1,
                  post_processed_text = ?2,
                  post_process_prompt = ?3,
                  transcript_segments = ?4,
                  speaker_segments = ?5,
                  revision = revision + 1
             WHERE id = ?6",
            params![
                transcription_text,
                post_processed_text,
                post_process_prompt,
                transcript_segments_json,
                speaker_segments_json,
                id,
            ],
        )?;
        if updated == 0 {
            return Err(anyhow!("History entry {} not found", id));
        }

        let entry = tx.query_row(
            &format!("{HISTORY_ENTRY_COLUMNS} WHERE id = ?1"),
            params![id],
            Self::map_history_entry,
        )?;
        let revision: i64 = tx.query_row(
            "SELECT revision FROM transcription_history WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        tx.commit()?;

        Ok((entry, revision))
    }

    fn update_transcription_with_segments(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        speaker_segments: Option<Option<Vec<SpeakerSegment>>>,
    ) -> Result<HistoryEntry> {
        let mut conn = self.get_connection()?;
        if let Some(Some(segments)) = speaker_segments.as_ref() {
            Self::validate_speaker_segments(segments)?;
        }
        let tx = conn.transaction()?;
        let updated = match speaker_segments {
            Some(speaker_segments) => tx.execute(
                "UPDATE transcription_history
                 SET transcription_text = ?1,
                      post_processed_text = ?2,
                      post_process_prompt = ?3,
                      speaker_segments = ?4,
                      revision = revision + 1
                 WHERE id = ?5",
                params![
                    transcription_text,
                    post_processed_text,
                    post_process_prompt,
                    speaker_segments
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    id
                ],
            )?,
            None => tx.execute(
                "UPDATE transcription_history
                 SET transcription_text = ?1,
                      post_processed_text = ?2,
                      post_process_prompt = ?3,
                      revision = revision + 1
                 WHERE id = ?4",
                params![
                    transcription_text,
                    post_processed_text,
                    post_process_prompt,
                    id
                ],
            )?,
        };

        if updated == 0 {
            return Err(anyhow!("History entry {} not found", id));
        }

        let entry = tx.query_row(
            &format!("{HISTORY_ENTRY_COLUMNS} WHERE id = ?1"),
            params![id],
            Self::map_history_entry,
        )?;
        tx.commit()?;

        debug!("Updated transcription for history entry {}", id);

        self.emit_history_updated(entry.clone());

        Ok(entry)
    }

    fn emit_history_updated(&self, entry: HistoryEntry) {
        if let Err(error) = (HistoryUpdatePayload::Updated { entry }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {error}");
        }
    }

    fn validate_transcript_segments(segments: &[TranscriptSegment]) -> Result<()> {
        for (index, segment) in segments.iter().enumerate() {
            if segment.end_ms < segment.start_ms {
                return Err(anyhow!(
                    "Transcript segment {index} ends before it starts: {} < {}",
                    segment.end_ms,
                    segment.start_ms
                ));
            }
            if segment.source.trim().is_empty() {
                return Err(anyhow!("Transcript segment {index} has an empty source"));
            }
            if segment.text.trim().is_empty() {
                return Err(anyhow!("Transcript segment {index} has empty text"));
            }
            if let Some(confidence) = segment.confidence {
                if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
                    return Err(anyhow!(
                        "Transcript segment {index} has invalid confidence {confidence}"
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_speaker_segments(segments: &[SpeakerSegment]) -> Result<()> {
        for (index, segment) in segments.iter().enumerate() {
            if segment.end_ms < segment.start_ms {
                return Err(anyhow!(
                    "Speaker segment {index} ends before it starts: {} < {}",
                    segment.end_ms,
                    segment.start_ms
                ));
            }
            if segment.source.trim().is_empty() {
                return Err(anyhow!("Speaker segment {index} has an empty source"));
            }
            if segment.text.trim().is_empty() {
                return Err(anyhow!("Speaker segment {index} has empty text"));
            }
            if segment.speaker.trim().is_empty() {
                return Err(anyhow!("Speaker segment {index} has an empty speaker"));
            }
            if let Some(confidence) = segment.confidence {
                if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
                    return Err(anyhow!(
                        "Speaker segment {index} has invalid confidence {confidence}"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn cleanup_old_entries(&self) -> Result<()> {
        let retention_period = crate::settings::get_recording_retention_period(&self.app_handle);

        match retention_period {
            crate::settings::RecordingRetentionPeriod::Never => {
                // Don't delete anything
                return Ok(());
            }
            crate::settings::RecordingRetentionPeriod::PreserveLimit => {
                // Use the old count-based logic with history_limit
                let limit = crate::settings::get_history_limit(&self.app_handle);
                return self.cleanup_by_count(limit);
            }
            _ => {
                // Use time-based logic
                return self.cleanup_by_time(retention_period);
            }
        }
    }

    fn delete_entries_and_files(
        &self,
        entries: &[(i64, String, Option<AudioTracks>, Option<MeetingSession>)],
    ) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let conn = self.get_connection()?;
        let mut deleted_count = 0;

        for (id, file_name, audio_tracks, meeting_session) in entries {
            // Keep the local vector index in sync with retention cleanup. Older
            // test databases may not have the RAG table yet, so tolerate that
            // migration gap just as delete_entry does.
            if let Err(error) =
                conn.execute("DELETE FROM rag_chunks WHERE entry_id = ?1", params![id])
            {
                if !is_missing_rag_chunks_table(&error) {
                    error!("Failed to delete RAG chunks for entry {}: {}", id, error);
                }
            }

            // Delete database entry
            conn.execute(
                "DELETE FROM transcription_history WHERE id = ?1",
                params![id],
            )?;

            // Delete WAV file. The database is durable user-controlled data, so
            // reject absolute and parent-directory paths before touching disk.
            if let Some(file_path) = Self::recordings_path_at(&self.recordings_dir, file_name) {
                if file_path.exists() {
                    if let Err(e) = fs::remove_file(&file_path) {
                        error!("Failed to delete WAV file {}: {}", file_name, e);
                    } else {
                        debug!("Deleted old WAV file: {}", file_name);
                        deleted_count += 1;
                    }
                }
            } else {
                error!(
                    "Refusing to delete history audio outside the recordings directory: {}",
                    file_name
                );
            }

            self.delete_meeting_session(meeting_session.as_ref(), audio_tracks.as_ref());
        }

        Ok(deleted_count)
    }

    fn cleanup_by_count(&self, limit: usize) -> Result<()> {
        let conn = self.get_connection()?;

        // Get all entries that are not saved, ordered by timestamp desc
        let mut stmt = conn.prepare(
            "SELECT id, file_name, audio_tracks, meeting_session FROM transcription_history WHERE saved = 0 ORDER BY timestamp DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>("id")?,
                row.get::<_, String>("file_name")?,
                deserialize_optional_json(row.get("audio_tracks")?)?,
                deserialize_optional_json(row.get("meeting_session")?)?,
            ))
        })?;

        let mut entries: Vec<(i64, String, Option<AudioTracks>, Option<MeetingSession>)> =
            Vec::new();
        for row in rows {
            entries.push(row?);
        }

        if entries.len() > limit {
            let entries_to_delete = &entries[limit..];
            let deleted_count = self.delete_entries_and_files(entries_to_delete)?;

            if deleted_count > 0 {
                debug!("Cleaned up {} old history entries by count", deleted_count);
            }
        }

        Ok(())
    }

    fn cleanup_by_time(
        &self,
        retention_period: crate::settings::RecordingRetentionPeriod,
    ) -> Result<()> {
        let conn = self.get_connection()?;

        // Calculate cutoff timestamp (current time minus retention period)
        let now = Utc::now().timestamp();
        let cutoff_timestamp = match retention_period {
            crate::settings::RecordingRetentionPeriod::Days3 => now - (3 * 24 * 60 * 60), // 3 days in seconds
            crate::settings::RecordingRetentionPeriod::Weeks2 => now - (2 * 7 * 24 * 60 * 60), // 2 weeks in seconds
            crate::settings::RecordingRetentionPeriod::Months3 => now - (3 * 30 * 24 * 60 * 60), // 3 months in seconds (approximate)
            _ => unreachable!("Should not reach here"),
        };

        // Get all unsaved entries older than the cutoff timestamp
        let mut stmt = conn.prepare(
            "SELECT id, file_name, audio_tracks, meeting_session FROM transcription_history WHERE saved = 0 AND timestamp < ?1",
        )?;

        let rows = stmt.query_map(params![cutoff_timestamp], |row| {
            Ok((
                row.get::<_, i64>("id")?,
                row.get::<_, String>("file_name")?,
                deserialize_optional_json(row.get("audio_tracks")?)?,
                deserialize_optional_json(row.get("meeting_session")?)?,
            ))
        })?;

        let mut entries_to_delete: Vec<(i64, String, Option<AudioTracks>, Option<MeetingSession>)> =
            Vec::new();
        for row in rows {
            entries_to_delete.push(row?);
        }

        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;

        if deleted_count > 0 {
            debug!(
                "Cleaned up {} old history entries based on retention period",
                deleted_count
            );
        }

        Ok(())
    }

    pub async fn get_history_entries(
        &self,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<PaginatedHistory> {
        let conn = self.get_connection()?;
        let limit = limit.map(|l| l.min(100));

        let mut entries: Vec<HistoryEntry> = match (cursor, limit) {
            (Some(cursor_id), Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(&format!(
                        "{HISTORY_ENTRY_COLUMNS}\n                     WHERE id < ?1\n                     ORDER BY id DESC\n                     LIMIT ?2",
                    ))?;
                let result = stmt
                    .query_map(params![cursor_id, fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (None, Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(&format!(
                        "{HISTORY_ENTRY_COLUMNS}\n                     ORDER BY id DESC\n                     LIMIT ?1",
                    ))?;
                let result = stmt
                    .query_map(params![fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (_, None) => {
                let mut stmt = conn.prepare(&format!(
                    "{HISTORY_ENTRY_COLUMNS}\n                     ORDER BY id DESC"
                ))?;
                let result = stmt
                    .query_map([], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
        };

        let has_more = limit.is_some_and(|lim| entries.len() > lim);
        if has_more {
            entries.pop();
        }

        Ok(PaginatedHistory { entries, has_more })
    }

    #[cfg(test)]
    fn get_latest_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(&format!(
            "{HISTORY_ENTRY_COLUMNS}\n             ORDER BY timestamp DESC\n             LIMIT 1"
        ))?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    /// Get the latest entry with non-empty transcription text.
    pub fn get_latest_completed_entry(&self) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::get_latest_completed_entry_with_conn(&conn)
    }

    fn get_latest_completed_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            &format!(
                "{HISTORY_ENTRY_COLUMNS}\n             WHERE transcription_text != ''\n             ORDER BY timestamp DESC\n             LIMIT 1"
            ),
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    pub async fn toggle_saved_status(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get current saved status
        let current_saved: bool = conn.query_row(
            "SELECT saved FROM transcription_history WHERE id = ?1",
            params![id],
            |row| row.get("saved"),
        )?;

        let new_saved = !current_saved;

        conn.execute(
            "UPDATE transcription_history SET saved = ?1 WHERE id = ?2",
            params![new_saved, id],
        )?;

        debug!("Toggled saved status for entry {}: {}", id, new_saved);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Toggled { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    pub fn get_audio_file_path(&self, file_name: &str) -> PathBuf {
        self.recordings_dir.join(file_name)
    }

    /// Resolve the retained ASR tracks and capture manifest for a meeting entry.
    ///
    /// The returned paths are canonical, regular files below the explicitly
    /// recorded meeting-session root. This rejects legacy/ordinary entries,
    /// malformed database rows, path traversal, missing files, and symlinks
    /// that escape the session directory before a retry deserializes or opens
    /// any persisted capture data.
    pub fn resolve_meeting_track_paths(
        &self,
        entry: &HistoryEntry,
    ) -> Result<(PathBuf, PathBuf, PathBuf)> {
        let meeting_session = entry
            .meeting_session
            .as_ref()
            .ok_or_else(|| anyhow!("History entry {} has no meeting session", entry.id))?;
        let audio_tracks = entry
            .audio_tracks
            .as_ref()
            .ok_or_else(|| anyhow!("History entry {} has no retained meeting tracks", entry.id))?;
        self.validate_meeting_session(&entry.file_name, meeting_session, audio_tracks)?;

        let relative_root = Self::meeting_session_root(&meeting_session.root).ok_or_else(|| {
            anyhow!(
                "History entry {} has an invalid meeting session root: {}",
                entry.id,
                meeting_session.root
            )
        })?;
        let recordings_root = self.recordings_dir.canonicalize().map_err(|error| {
            anyhow!(
                "Resolve recordings directory {:?} for history entry {}: {error}",
                self.recordings_dir,
                entry.id
            )
        })?;
        let session_root = self.recordings_dir.join(&relative_root);
        let canonical_session_root = session_root.canonicalize().map_err(|error| {
            anyhow!(
                "Resolve meeting session directory {:?} for history entry {}: {error}",
                session_root,
                entry.id
            )
        })?;
        if !canonical_session_root.is_dir() || !canonical_session_root.starts_with(&recordings_root)
        {
            return Err(anyhow!(
                "History entry {} resolves outside the recordings directory",
                entry.id
            ));
        }

        let microphone = Self::resolve_meeting_session_asset_at(
            &self.recordings_dir,
            &relative_root,
            &canonical_session_root,
            &audio_tracks.microphone,
            "microphone track",
        )?;
        let system = Self::resolve_meeting_session_asset_at(
            &self.recordings_dir,
            &relative_root,
            &canonical_session_root,
            &audio_tracks.system,
            "system track",
        )?;
        let manifest = Self::resolve_meeting_session_asset_at(
            &self.recordings_dir,
            &relative_root,
            &canonical_session_root,
            &meeting_session.manifest,
            "capture manifest",
        )?;

        Ok((microphone, system, manifest))
    }

    pub async fn get_entry_by_id(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(&format!(
            "{HISTORY_ENTRY_COLUMNS}\n             WHERE id = ?1"
        ))?;

        let entry = stmt.query_row([id], Self::map_history_entry).optional()?;

        Ok(entry)
    }

    pub async fn delete_entry(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get the entry to find the file name
        if let Some(entry) = self.get_entry_by_id(id).await? {
            // Delete the audio file first. Never let a corrupt database path
            // escape the recordings directory.
            if let Some(file_path) =
                Self::recordings_path_at(&self.recordings_dir, &entry.file_name)
            {
                if file_path.exists() {
                    if let Err(e) = fs::remove_file(&file_path) {
                        error!("Failed to delete audio file {}: {}", entry.file_name, e);
                        // Continue with database deletion even if file deletion fails
                    }
                }
            } else {
                error!(
                    "Refusing to delete history audio outside the recordings directory: {}",
                    entry.file_name
                );
            }
            self.delete_meeting_session(
                entry.meeting_session.as_ref(),
                entry.audio_tracks.as_ref(),
            );
        }

        // Delete from database. Older test databases may not have the RAG table
        // yet, but unrelated database errors must remain visible.
        if let Err(error) = conn.execute("DELETE FROM rag_chunks WHERE entry_id = ?1", params![id])
        {
            if !is_missing_rag_chunks_table(&error) {
                error!("Failed to delete RAG chunks for entry {}: {}", id, error);
            }
        }
        conn.execute(
            "DELETE FROM transcription_history WHERE id = ?1",
            params![id],
        )?;

        debug!("Deleted history entry with id: {}", id);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Deleted { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    pub fn clear_summary(&self, id: i64) -> Result<HistoryEntry> {
        let mut conn = self.get_connection()?;
        let tx = conn.transaction()?;
        let entry = self.clear_summary_with_connection(&tx, id)?;
        tx.commit()?;
        self.emit_summary_updated(entry.clone());
        Ok(entry)
    }

    pub(crate) fn clear_summary_with_connection(
        &self,
        conn: &Connection,
        id: i64,
    ) -> Result<HistoryEntry> {
        let updated = conn.execute(
            "UPDATE transcription_history
             SET post_processed_text = NULL
             WHERE id = ?1",
            params![id],
        )?;
        if updated == 0 {
            return Err(anyhow!("History entry {} not found", id));
        }
        let entry = conn.query_row(
            &format!("{HISTORY_ENTRY_COLUMNS} WHERE id = ?1"),
            params![id],
            Self::map_history_entry,
        )?;
        Ok(entry)
    }

    pub(crate) fn emit_summary_updated(&self, entry: HistoryEntry) {
        if let Err(error) = (HistoryUpdatePayload::Updated { entry }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", error);
        }
    }

    /// Validate paths supplied by a new meeting capture before they become
    /// durable. The session root is intentionally a direct `meeting-*` child
    /// of the recordings directory; allowing a nested root would make a
    /// corrupted history row capable of deleting an unrelated directory.
    fn validate_meeting_session(
        &self,
        file_name: &str,
        session: &MeetingSession,
        tracks: &AudioTracks,
    ) -> Result<()> {
        let root = Self::meeting_session_root(&session.root).ok_or_else(|| {
            anyhow!(
                "meeting session root must be a direct non-empty meeting-* directory: {}",
                session.root
            )
        })?;

        for (kind, path) in [
            ("history audio", file_name),
            ("manifest", session.manifest.as_str()),
            ("mix track", tracks.mix.as_str()),
            ("microphone track", tracks.microphone.as_str()),
            ("system track", tracks.system.as_str()),
        ] {
            if Self::meeting_asset_path(&root, path).is_none() {
                return Err(anyhow!(
                    "{kind} must remain inside meeting session root {}: {}",
                    session.root,
                    path
                ));
            }
        }

        Ok(())
    }

    /// Return a safe, recordings-directory-relative path. This accepts normal
    /// legacy file names but rejects absolute, current-directory, and parent
    /// traversal components before filesystem operations.
    fn relative_recordings_path(path: &str) -> Option<PathBuf> {
        let path = Path::new(path);
        if path.as_os_str().is_empty() {
            return None;
        }

        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(component) => relative.push(component),
                _ => return None,
            }
        }

        (!relative.as_os_str().is_empty()).then_some(relative)
    }

    fn recordings_path_at(recordings_dir: &Path, path: &str) -> Option<PathBuf> {
        Self::relative_recordings_path(path).map(|relative| recordings_dir.join(relative))
    }

    fn meeting_session_root(root: &str) -> Option<PathBuf> {
        let root = Self::relative_recordings_path(root)?;
        let mut components = root.components();
        let Component::Normal(name) = components.next()? else {
            return None;
        };
        if components.next().is_some() {
            return None;
        }

        let name = name.to_str()?;
        if !name.starts_with("meeting-") || name == "meeting-" {
            return None;
        }

        Some(root)
    }

    /// Resolve a session asset only when it is nested below the explicit root.
    fn meeting_asset_path(root: &Path, path: &str) -> Option<PathBuf> {
        let path = Self::relative_recordings_path(path)?;
        if path == root || !path.starts_with(root) {
            return None;
        }
        Some(path)
    }

    fn resolve_meeting_session_asset_at(
        recordings_dir: &Path,
        relative_root: &Path,
        canonical_session_root: &Path,
        path: &str,
        kind: &str,
    ) -> Result<PathBuf> {
        let relative_asset = Self::meeting_asset_path(relative_root, path)
            .ok_or_else(|| anyhow!("{kind} is outside the meeting session root: {path}"))?;
        let asset = recordings_dir.join(relative_asset);
        if !asset.is_file() {
            return Err(anyhow!(
                "{kind} is missing or not a regular file: {asset:?}"
            ));
        }
        let canonical_asset = asset
            .canonicalize()
            .map_err(|error| anyhow!("Resolve {kind} {asset:?}: {error}"))?;
        if !canonical_asset.starts_with(canonical_session_root) {
            return Err(anyhow!(
                "{kind} resolves outside the meeting session root: {path}"
            ));
        }
        Ok(canonical_asset)
    }

    /// Infer the ownership directory used by v1 audio-track rows. This is only
    /// used when the explicit session metadata is absent, and requires all
    /// tracks to share one direct `meeting-*` root.
    fn legacy_session_root_from_tracks(tracks: &AudioTracks) -> Option<PathBuf> {
        let root = Self::relative_recordings_path(&tracks.mix)?;
        let root = root
            .components()
            .next()
            .and_then(|component| match component {
                Component::Normal(name) => Self::meeting_session_root(name.to_str()?),
                _ => None,
            })?;

        [
            tracks.mix.as_str(),
            tracks.microphone.as_str(),
            tracks.system.as_str(),
        ]
        .into_iter()
        .all(|path| Self::meeting_asset_path(&root, path).is_some())
        .then_some(root)
    }

    /// Remove the directory which owns all meeting tracks. New rows use their
    /// explicit session root. Legacy rows can fall back to the common top-level
    /// mix root only when session metadata is absent.
    fn delete_meeting_session(
        &self,
        session: Option<&MeetingSession>,
        tracks: Option<&AudioTracks>,
    ) {
        Self::delete_meeting_session_at(&self.recordings_dir, session, tracks);
    }

    fn delete_meeting_session_at(
        recordings_dir: &Path,
        session: Option<&MeetingSession>,
        tracks: Option<&AudioTracks>,
    ) {
        let root = match session {
            Some(session) => Self::meeting_session_root(&session.root),
            None => tracks.and_then(Self::legacy_session_root_from_tracks),
        };
        let Some(root) = root else {
            error!("Refusing to delete unsafe meeting session directory");
            return;
        };

        let directory = recordings_dir.join(root);
        if directory.exists() {
            if let Err(error) = fs::remove_dir_all(&directory) {
                error!(
                    "Failed to delete meeting session directory {:?}: {}",
                    directory, error
                );
            }
        }
    }

    fn format_timestamp_title(&self, timestamp: i64) -> String {
        if let Some(utc_datetime) = DateTime::from_timestamp(timestamp, 0) {
            // Convert UTC to local timezone
            let local_datetime = utc_datetime.with_timezone(&Local);
            local_datetime.format("%B %e, %Y - %l:%M%p").to_string()
        } else {
            format!("Recording {}", timestamp)
        }
    }
}

fn is_missing_rag_chunks_table(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(_, Some(message))
            if message.contains("no such table: rag_chunks")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};
    use tempfile::tempdir;

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE transcription_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_name TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                saved BOOLEAN NOT NULL DEFAULT 0,
                title TEXT NOT NULL,
                transcription_text TEXT NOT NULL,
                post_processed_text TEXT,
                post_process_prompt TEXT,
                post_process_requested BOOLEAN NOT NULL DEFAULT 0,
                audio_tracks TEXT,
                meeting_session TEXT,
                speaker_segments TEXT,
                transcript_segments TEXT
            );",
        )
        .expect("create transcription_history table");
        conn
    }

    fn setup_v6_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open v6 in-memory db");
        conn.execute_batch(
            "CREATE TABLE transcription_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_name TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                saved BOOLEAN NOT NULL DEFAULT 0,
                title TEXT NOT NULL,
                transcription_text TEXT NOT NULL,
                post_processed_text TEXT,
                post_process_prompt TEXT,
                post_process_requested BOOLEAN NOT NULL DEFAULT 0,
                audio_tracks TEXT
            );
            CREATE TABLE rag_chunks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id INTEGER NOT NULL,
                source TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                embedding_model TEXT NOT NULL,
                dimensions INTEGER NOT NULL,
                embedding BLOB NOT NULL,
                UNIQUE(entry_id, source, chunk_index)
            );
            CREATE INDEX idx_rag_chunks_entry ON rag_chunks(entry_id);
            PRAGMA user_version = 6;",
        )
        .expect("create v6 transcription_history schema");
        conn
    }

    fn setup_v7_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open v7 in-memory db");
        conn.execute_batch(
            "CREATE TABLE transcription_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_name TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                saved BOOLEAN NOT NULL DEFAULT 0,
                title TEXT NOT NULL,
                transcription_text TEXT NOT NULL,
                post_processed_text TEXT,
                post_process_prompt TEXT,
                post_process_requested BOOLEAN NOT NULL DEFAULT 0,
                audio_tracks TEXT,
                meeting_session TEXT,
                speaker_segments TEXT
            );
            PRAGMA user_version = 7;",
        )
        .expect("create v7 transcription_history schema");
        conn
    }

    fn insert_entry(conn: &Connection, timestamp: i64, text: &str, post_processed: Option<&str>) {
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                format!("thegai-{}.wav", timestamp),
                timestamp,
                false,
                format!("Recording {}", timestamp),
                text,
                post_processed,
                Option::<String>::None,
                false,
            ],
        )
        .expect("insert history entry");
    }

    #[test]
    fn get_latest_entry_returns_none_when_empty() {
        let conn = setup_conn();
        let entry = HistoryManager::get_latest_entry_with_conn(&conn).expect("fetch latest entry");
        assert!(entry.is_none());
    }

    #[test]
    fn get_latest_entry_returns_newest_entry() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "first", None);
        insert_entry(&conn, 200, "second", Some("processed"));

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");

        assert_eq!(entry.timestamp, 200);
        assert_eq!(entry.transcription_text, "second");
        assert_eq!(entry.post_processed_text.as_deref(), Some("processed"));
    }

    #[test]
    fn get_latest_completed_entry_skips_empty_entries() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "completed", None);
        insert_entry(&conn, 200, "", None);

        let entry = HistoryManager::get_latest_completed_entry_with_conn(&conn)
            .expect("fetch latest completed entry")
            .expect("completed entry exists");

        assert_eq!(entry.timestamp, 100);
        assert_eq!(entry.transcription_text, "completed");
    }

    #[test]
    fn migrates_v6_history_without_losing_audio_track_rows() {
        let mut conn = setup_v6_conn();
        let audio_tracks = AudioTracks {
            mix: "meeting-legacy/mix.wav".to_string(),
            microphone: "meeting-legacy/microphone.wav".to_string(),
            system: "meeting-legacy/system.wav".to_string(),
        };
        conn.execute(
            "INSERT INTO transcription_history (
                file_name, timestamp, saved, title, transcription_text,
                post_process_requested, audio_tracks
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &audio_tracks.mix,
                100,
                false,
                "Legacy meeting",
                "legacy transcript",
                false,
                serde_json::to_string(&audio_tracks).unwrap(),
            ],
        )
        .expect("insert v6 entry");

        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .expect("migrate v6 history");

        let columns = conn
            .prepare("PRAGMA table_info(transcription_history)")
            .expect("prepare table info")
            .query_map([], |row| row.get::<_, String>("name"))
            .expect("query table info")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read table info");
        assert!(columns.contains(&"meeting_session".to_string()));
        assert!(columns.contains(&"speaker_segments".to_string()));
        assert!(columns.contains(&"transcript_segments".to_string()));

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("read migrated entry")
            .expect("migrated entry exists");
        assert_eq!(entry.audio_tracks, Some(audio_tracks));
        assert!(entry.meeting_session.is_none());
        assert!(entry.speaker_segments.is_none());
        assert!(entry.transcript_segments.is_none());
    }

    #[test]
    fn migrates_v7_history_without_losing_speaker_segments() {
        let mut conn = setup_v7_conn();
        let speaker_segments = vec![SpeakerSegment {
            start_ms: 500,
            end_ms: 1_000,
            speaker: "Remote Speaker 1".to_string(),
            source: "system".to_string(),
            text: "A prior diarization result".to_string(),
            confidence: Some(0.9),
        }];
        conn.execute(
            "INSERT INTO transcription_history (
                file_name, timestamp, saved, title, transcription_text,
                post_process_requested, speaker_segments
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "meeting-legacy/mix.wav",
                100,
                false,
                "Legacy meeting",
                "A prior diarization result",
                true,
                serde_json::to_string(&speaker_segments).unwrap(),
            ],
        )
        .expect("insert v7 entry");

        Migrations::new(MIGRATIONS.to_vec())
            .to_latest(&mut conn)
            .expect("migrate v7 history");

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("read migrated entry")
            .expect("migrated entry exists");
        assert_eq!(entry.speaker_segments, Some(speaker_segments));
        assert!(entry.transcript_segments.is_none());
    }

    #[test]
    fn maps_meeting_session_timestamped_transcript_and_speaker_segments() {
        let conn = setup_conn();
        let tracks = meeting_tracks("meeting-2026");
        let session = MeetingSession {
            root: "meeting-2026".to_string(),
            manifest: "meeting-2026/manifest.json".to_string(),
        };
        let segments = vec![SpeakerSegment {
            start_ms: 125,
            end_ms: 875,
            speaker: "Remote Speaker 1".to_string(),
            source: "system".to_string(),
            text: "Can you hear me?".to_string(),
            confidence: Some(0.91),
        }];
        let transcript_segments = vec![TranscriptSegment {
            start_ms: 125,
            end_ms: 875,
            source: "system".to_string(),
            text: "Can you hear me?".to_string(),
            confidence: Some(0.83),
        }];
        conn.execute(
            "INSERT INTO transcription_history (
                file_name, timestamp, saved, title, transcription_text,
                post_process_requested, audio_tracks, meeting_session, speaker_segments,
                transcript_segments
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &tracks.mix,
                100,
                false,
                "Meeting",
                "Can you hear me?",
                false,
                serde_json::to_string(&tracks).unwrap(),
                serde_json::to_string(&session).unwrap(),
                serde_json::to_string(&segments).unwrap(),
                serde_json::to_string(&transcript_segments).unwrap(),
            ],
        )
        .expect("insert meeting entry");

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("read meeting entry")
            .expect("meeting entry exists");
        assert_eq!(entry.audio_tracks, Some(tracks));
        assert_eq!(entry.meeting_session, Some(session));
        assert_eq!(entry.speaker_segments, Some(segments));
        assert_eq!(entry.transcript_segments, Some(transcript_segments));
    }

    #[test]
    fn timed_meeting_update_replaces_transcript_speaker_and_timing_rows_together() {
        let mut conn = setup_conn();
        insert_entry(&conn, 100, "pending", None);
        let transcript_segments = vec![TranscriptSegment {
            start_ms: 100,
            end_ms: 450,
            source: "microphone".to_string(),
            text: "Good morning".to_string(),
            confidence: Some(0.92),
        }];
        let speaker_segments = vec![SpeakerSegment {
            start_ms: 100,
            end_ms: 450,
            speaker: "You".to_string(),
            source: "microphone".to_string(),
            text: "Good morning".to_string(),
            confidence: Some(1.0),
        }];

        let (entry, _) =
            HistoryManager::update_meeting_transcription_with_timed_segments_in_connection(
                &mut conn,
                1,
                "Good morning".to_string(),
                Some("Summary".to_string()),
                Some(DEFAULT_MEETING_SUMMARY_PROMPT_ID.to_string()),
                Some(transcript_segments.clone()),
                Some(speaker_segments.clone()),
            )
            .expect("update timestamped meeting transcript");

        assert_eq!(entry.transcript_segments, Some(transcript_segments));
        assert_eq!(entry.speaker_segments, Some(speaker_segments));
        assert_eq!(entry.transcription_text, "Good morning");
    }

    #[test]
    fn conditional_speaker_update_only_changes_labels_for_current_timed_transcript() {
        let mut conn = setup_conn();
        insert_entry(&conn, 100, "pending", None);
        let transcript_segments = vec![TranscriptSegment {
            start_ms: 100,
            end_ms: 450,
            source: "microphone".to_string(),
            text: "Good morning".to_string(),
            confidence: None,
        }];
        let (_, revision) =
            HistoryManager::update_meeting_transcription_with_timed_segments_in_connection(
                &mut conn,
                1,
                "Good morning".to_string(),
                Some("Original summary".to_string()),
                Some(DEFAULT_MEETING_SUMMARY_PROMPT_ID.to_string()),
                Some(transcript_segments.clone()),
                None,
            )
            .expect("write initial transcript");
        let speaker_segments = vec![SpeakerSegment {
            start_ms: 100,
            end_ms: 450,
            speaker: "You".to_string(),
            source: "microphone".to_string(),
            text: "Good morning".to_string(),
            confidence: Some(1.0),
        }];

        let entry = HistoryManager::update_meeting_speaker_segments_if_current_in_connection(
            &mut conn,
            1,
            revision,
            speaker_segments.clone(),
        )
        .expect("write current speaker labels")
        .expect("snapshot should still match");

        assert_eq!(entry.transcription_text, "Good morning");
        assert_eq!(
            entry.post_processed_text.as_deref(),
            Some("Original summary")
        );
        assert_eq!(entry.transcript_segments, Some(transcript_segments));
        assert_eq!(entry.speaker_segments, Some(speaker_segments));
    }

    #[test]
    fn conditional_speaker_update_does_not_overwrite_a_newer_retry() {
        let mut conn = setup_conn();
        insert_entry(&conn, 100, "pending", None);
        let original_segments = vec![TranscriptSegment {
            start_ms: 100,
            end_ms: 450,
            source: "microphone".to_string(),
            text: "Original words".to_string(),
            confidence: None,
        }];
        let (_, original_revision) =
            HistoryManager::update_meeting_transcription_with_timed_segments_in_connection(
                &mut conn,
                1,
                "Original words".to_string(),
                Some("Original summary".to_string()),
                Some(DEFAULT_MEETING_SUMMARY_PROMPT_ID.to_string()),
                Some(original_segments.clone()),
                None,
            )
            .expect("write original transcript");

        let retry_segments = vec![TranscriptSegment {
            start_ms: 120,
            end_ms: 480,
            source: "system".to_string(),
            text: "Retried words".to_string(),
            confidence: None,
        }];
        HistoryManager::update_meeting_transcription_with_timed_segments_in_connection(
            &mut conn,
            1,
            "Retried words".to_string(),
            Some("Retry summary".to_string()),
            Some(DEFAULT_MEETING_SUMMARY_PROMPT_ID.to_string()),
            Some(retry_segments.clone()),
            None,
        )
        .expect("write newer retry");

        let stale_labels = vec![SpeakerSegment {
            start_ms: 100,
            end_ms: 450,
            speaker: "You".to_string(),
            source: "microphone".to_string(),
            text: "Original words".to_string(),
            confidence: Some(1.0),
        }];
        let updated = HistoryManager::update_meeting_speaker_segments_if_current_in_connection(
            &mut conn,
            1,
            original_revision,
            stale_labels,
        )
        .expect("compare stale diarization snapshot");

        assert!(updated.is_none());
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("read retried transcript")
            .expect("entry exists");
        assert_eq!(entry.transcription_text, "Retried words");
        assert_eq!(entry.post_processed_text.as_deref(), Some("Retry summary"));
        assert_eq!(entry.transcript_segments, Some(retry_segments));
        assert!(entry.speaker_segments.is_none());
    }

    #[test]
    fn timed_meeting_update_rejects_invalid_timing_without_mutating_entry() {
        let mut conn = setup_conn();
        insert_entry(&conn, 100, "pending", None);
        let invalid_segment = TranscriptSegment {
            start_ms: 500,
            end_ms: 100,
            source: "system".to_string(),
            text: "Invalid timing".to_string(),
            confidence: None,
        };

        let error = HistoryManager::update_meeting_transcription_with_timed_segments_in_connection(
            &mut conn,
            1,
            "should not persist".to_string(),
            None,
            Some(DEFAULT_MEETING_SUMMARY_PROMPT_ID.to_string()),
            Some(vec![invalid_segment]),
            None,
        )
        .expect_err("invalid timestamp must fail before updating the history row");

        assert!(error.to_string().contains("ends before it starts"));
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("read unchanged entry")
            .expect("entry exists");
        assert_eq!(entry.transcription_text, "pending");
    }

    fn meeting_tracks(root: &str) -> AudioTracks {
        AudioTracks {
            mix: format!("{root}/derived/mix.wav"),
            microphone: format!("{root}/asr/microphone-16k.wav"),
            system: format!("{root}/asr/system-16k.wav"),
        }
    }

    fn assert_sentinel_survives_unsafe_session(
        session: Option<&MeetingSession>,
        tracks: Option<&AudioTracks>,
    ) {
        let temp = tempdir().unwrap();
        let recordings_dir = temp.path().join("recordings");
        fs::create_dir(&recordings_dir).unwrap();
        let sentinel = temp.path().join("sentinel.txt");
        fs::write(&sentinel, "keep").unwrap();

        HistoryManager::delete_meeting_session_at(&recordings_dir, session, tracks);

        assert!(sentinel.exists());
    }

    #[test]
    fn delete_meeting_session_rejects_absolute_root() {
        let temp = tempdir().unwrap();
        let session = MeetingSession {
            root: temp
                .path()
                .join("meeting-outside")
                .to_string_lossy()
                .into_owned(),
            manifest: "meeting-outside/manifest.json".to_string(),
        };
        assert_sentinel_survives_unsafe_session(Some(&session), None);
    }

    #[test]
    fn delete_meeting_session_rejects_parent_root() {
        let session = MeetingSession {
            root: "../meeting-outside".to_string(),
            manifest: "meeting-outside/manifest.json".to_string(),
        };
        assert_sentinel_survives_unsafe_session(Some(&session), None);
    }

    #[test]
    fn delete_meeting_session_does_not_fall_back_when_explicit_root_is_invalid() {
        let temp = tempdir().unwrap();
        let recordings_dir = temp.path().join("recordings");
        let meeting_dir = recordings_dir.join("meeting-valid");
        fs::create_dir_all(&meeting_dir).unwrap();
        fs::write(meeting_dir.join("keep.wav"), "keep").unwrap();
        let tracks = meeting_tracks("meeting-valid");
        let invalid_session = MeetingSession {
            root: "../meeting-valid".to_string(),
            manifest: "meeting-valid/manifest.json".to_string(),
        };

        HistoryManager::delete_meeting_session_at(
            &recordings_dir,
            Some(&invalid_session),
            Some(&tracks),
        );

        assert!(meeting_dir.exists());
    }

    #[test]
    fn delete_meeting_session_removes_explicit_root_with_nested_derived_tracks() {
        let temp = tempdir().unwrap();
        let recordings_dir = temp.path().join("recordings");
        let root = "meeting-2026";
        let meeting_dir = recordings_dir.join(root);
        fs::create_dir_all(meeting_dir.join("derived")).unwrap();
        fs::create_dir_all(meeting_dir.join("sources")).unwrap();
        fs::write(meeting_dir.join("derived/mix.wav"), "mix").unwrap();
        fs::write(meeting_dir.join("sources/microphone.wav"), "microphone").unwrap();
        let sentinel = recordings_dir.join("unrelated.wav");
        fs::write(&sentinel, "keep").unwrap();
        let session = MeetingSession {
            root: root.to_string(),
            manifest: format!("{root}/manifest.json"),
        };

        HistoryManager::delete_meeting_session_at(&recordings_dir, Some(&session), None);

        assert!(!meeting_dir.exists());
        assert!(sentinel.exists());
    }

    #[test]
    fn delete_meeting_session_uses_legacy_track_root_only_when_metadata_is_absent() {
        let temp = tempdir().unwrap();
        let recordings_dir = temp.path().join("recordings");
        let meeting_dir = recordings_dir.join("meeting-legacy");
        fs::create_dir_all(meeting_dir.join("derived")).unwrap();
        let tracks = meeting_tracks("meeting-legacy");

        HistoryManager::delete_meeting_session_at(&recordings_dir, None, Some(&tracks));

        assert!(!meeting_dir.exists());
    }

    #[test]
    fn meeting_asset_path_rejects_root_escape() {
        let root = Path::new("meeting-2026");
        assert!(HistoryManager::meeting_asset_path(root, "meeting-2026/manifest.json").is_some());
        assert!(HistoryManager::meeting_asset_path(root, "meeting-2026/../outside.json").is_none());
        assert!(HistoryManager::meeting_asset_path(root, "other/manifest.json").is_none());
    }

    #[test]
    fn resolve_meeting_session_asset_returns_canonical_file_inside_session() {
        let temp = tempdir().expect("create temp directory");
        let recordings_dir = temp.path().join("recordings");
        let relative_root = Path::new("meeting-2026");
        let session_dir = recordings_dir.join(relative_root);
        fs::create_dir_all(&session_dir).expect("create meeting session directory");
        let microphone = session_dir.join("microphone.wav");
        fs::write(&microphone, "audio").expect("write microphone track");
        let canonical_session = session_dir
            .canonicalize()
            .expect("canonicalize meeting session directory");

        let resolved = HistoryManager::resolve_meeting_session_asset_at(
            &recordings_dir,
            relative_root,
            &canonical_session,
            "meeting-2026/microphone.wav",
            "microphone track",
        )
        .expect("resolve retained microphone track");

        assert_eq!(
            resolved,
            microphone
                .canonicalize()
                .expect("canonicalize microphone track")
        );
    }

    #[test]
    fn resolve_meeting_session_asset_rejects_root_escape() {
        let temp = tempdir().expect("create temp directory");
        let recordings_dir = temp.path().join("recordings");
        let relative_root = Path::new("meeting-2026");
        let session_dir = recordings_dir.join(relative_root);
        fs::create_dir_all(&session_dir).expect("create meeting session directory");
        let canonical_session = session_dir
            .canonicalize()
            .expect("canonicalize meeting session directory");

        let error = HistoryManager::resolve_meeting_session_asset_at(
            &recordings_dir,
            relative_root,
            &canonical_session,
            "../outside.wav",
            "microphone track",
        )
        .expect_err("root escape must not resolve");

        assert!(error
            .to_string()
            .contains("outside the meeting session root"));
    }
}
