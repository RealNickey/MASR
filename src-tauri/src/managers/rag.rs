use crate::credentials;
use crate::managers::history::{HistoryEntry, HistoryManager};
use crate::settings::{get_settings, AppSettings};
use anyhow::{anyhow, Result};
use base64::Engine;
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use rusqlite::{params, Connection};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

const EMBEDDING_MODEL: &str = "gemini-embedding-2";
const EMBEDDING_DIMENSIONS: usize = 768;
const CHUNK_SIZE: usize = 1200;
const CHUNK_OVERLAP: usize = 200;
const MAX_QUERY_RESULTS: usize = 10;
const MAX_QUERY_CHARS: usize = 4096;
const INDEX_PAGE_SIZE: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum RagStatus {
    Disabled,
    NeedsByokKey,
    Ready,
    Indexing,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct RagStatusSnapshot {
    pub status: RagStatus,
    pub indexed_chunks: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, specta::Type)]
pub struct RagSearchHit {
    pub entry_id: i64,
    pub title: String,
    pub timestamp: i64,
    pub source: String,
    pub excerpt: String,
    pub score: f32,
}

#[derive(Clone)]
pub struct RagManager {
    app_handle: tauri::AppHandle,
    history_manager: Arc<HistoryManager>,
    index_lock: Arc<Mutex<()>>,
    status: Arc<RwLock<RagStatusSnapshot>>,
}

#[derive(Debug, Serialize)]
struct EmbedContent<'a> {
    parts: Vec<EmbedPart<'a>>,
}

#[derive(Debug, Serialize)]
struct EmbedPart<'a> {
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    model: &'static str,
    content: EmbedContent<'a>,
    output_dimensionality: usize,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    embedding: EmbedValues,
}

#[derive(Debug, Deserialize)]
struct EmbedValues {
    values: Vec<f32>,
}

#[derive(Debug, Clone)]
struct ChunkInput {
    index: usize,
    content: String,
    hash: String,
}

impl RagManager {
    pub fn new(app_handle: &tauri::AppHandle, history_manager: Arc<HistoryManager>) -> Arc<Self> {
        Arc::new(Self {
            app_handle: app_handle.clone(),
            history_manager,
            index_lock: Arc::new(Mutex::new(())),
            status: Arc::new(RwLock::new(RagStatusSnapshot {
                status: RagStatus::Disabled,
                indexed_chunks: 0,
                error: None,
            })),
        })
    }

    pub async fn status(&self) -> RagStatusSnapshot {
        let settings = get_settings(&self.app_handle);
        let mut snapshot = self.status.read().await.clone();
        if !settings.rag_enabled {
            snapshot.status = RagStatus::Disabled;
            snapshot.error = None;
            return snapshot;
        }
        if user_google_api_key(&settings).is_none() {
            snapshot.status = RagStatus::NeedsByokKey;
            snapshot.error = None;
        } else if matches!(&snapshot.status, RagStatus::NeedsByokKey) {
            snapshot.status = RagStatus::Ready;
            snapshot.error = None;
        }
        snapshot
    }

    pub async fn enable_or_validate(&self) -> Result<()> {
        let settings = get_settings(&self.app_handle);
        if !settings.rag_enabled {
            self.set_status(RagStatus::Disabled, None).await;
            return Ok(());
        }
        if user_google_api_key(&settings).is_none() {
            self.set_status(RagStatus::NeedsByokKey, None).await;
            return Err(anyhow!(
                "Vector Memory requires a user-provided Gemini API key"
            ));
        }
        self.set_status(RagStatus::Ready, None).await;
        Ok(())
    }

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        exclude_entry_id: Option<i64>,
    ) -> Result<Vec<RagSearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(anyhow!(
                "Search query exceeds the {} character limit",
                MAX_QUERY_CHARS
            ));
        }
        self.enable_or_validate().await?;
        if !get_settings(&self.app_handle).rag_enabled {
            return Err(anyhow!("Vector Memory is disabled"));
        }
        self.ensure_indexed().await?;

        let settings = get_settings(&self.app_handle);
        let key = user_google_api_key(&settings)
            .ok_or_else(|| anyhow!("Vector Memory requires a user-provided Gemini API key"))?;
        let query_embedding = embed_text(&key, query).await?;
        let query_embedding = normalize(query_embedding)?;

        let conn = Connection::open(self.history_manager.db_path())?;
        let mut stmt = conn.prepare(
            "SELECT r.entry_id, h.title, h.timestamp, r.source, r.content, r.embedding
             FROM rag_chunks r
             JOIN transcription_history h ON h.id = r.entry_id
             WHERE h.post_process_prompt IN ('default_meeting_summary', 'default_meeting_notes_with_actions')",
        )?;
        let rows = stmt.query_map([], |row| {
            let embedding: Vec<u8> = row.get(5)?;
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                embedding,
            ))
        })?;

        let mut hits = Vec::new();
        for row in rows {
            let (entry_id, title, timestamp, source, excerpt, bytes) = row?;
            if exclude_entry_id == Some(entry_id) {
                continue;
            }
            let embedding = decode_embedding(&bytes)?;
            if embedding.len() != query_embedding.len() {
                continue;
            }
            hits.push(RagSearchHit {
                entry_id,
                title,
                timestamp,
                source,
                excerpt,
                score: dot(&query_embedding, &embedding),
            });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit.clamp(1, MAX_QUERY_RESULTS));
        Ok(hits)
    }

    pub async fn reindex(&self) -> Result<()> {
        self.enable_or_validate().await?;
        if !get_settings(&self.app_handle).rag_enabled {
            return Err(anyhow!("Vector Memory is disabled"));
        }
        let _guard = self.index_lock.lock().await;
        self.set_status(RagStatus::Indexing, None).await;
        let result = self.reindex_locked().await;
        match &result {
            Ok(()) => self.refresh_count(RagStatus::Ready, None).await,
            Err(error) => {
                self.set_status(RagStatus::Error, Some(error.to_string()))
                    .await
            }
        }
        result
    }

    pub async fn clear_index(&self) -> Result<()> {
        let conn = Connection::open(self.history_manager.db_path())?;
        conn.execute("DELETE FROM rag_chunks", [])?;
        let settings = get_settings(&self.app_handle);
        let status = if settings.rag_enabled && user_google_api_key(&settings).is_some() {
            RagStatus::Ready
        } else {
            RagStatus::Disabled
        };
        self.set_status(status, None).await;
        Ok(())
    }

    pub async fn remove_source(&self, entry_id: i64, source: &str) -> Result<()> {
        let conn = Connection::open(self.history_manager.db_path())?;
        conn.execute(
            "DELETE FROM rag_chunks WHERE entry_id = ?1 AND source = ?2",
            params![entry_id, source],
        )?;
        self.refresh_count(RagStatus::Ready, None).await;
        Ok(())
    }

    async fn ensure_indexed(&self) -> Result<()> {
        let _guard = self.index_lock.lock().await;
        let settings = get_settings(&self.app_handle);
        if !settings.rag_enabled {
            return Ok(());
        }
        self.set_status(RagStatus::Indexing, None).await;
        let result = self.ensure_indexed_locked(&settings).await;
        match &result {
            Ok(()) => self.refresh_count(RagStatus::Ready, None).await,
            Err(error) => {
                self.set_status(RagStatus::Error, Some(error.to_string()))
                    .await
            }
        }
        result
    }

    async fn reindex_locked(&self) -> Result<()> {
        let conn = Connection::open(self.history_manager.db_path())?;
        conn.execute("DELETE FROM rag_chunks", [])?;
        let settings = get_settings(&self.app_handle);
        self.ensure_indexed_locked(&settings).await
    }

    async fn ensure_indexed_locked(&self, settings: &AppSettings) -> Result<()> {
        let key = user_google_api_key(settings)
            .ok_or_else(|| anyhow!("Vector Memory requires a user-provided Gemini API key"))?;
        // Paginate the history scan so a large history is never materialized in
        // memory at once, and tolerate per-entry failures (a single transient
        // Gemini error must not abort indexing for every other meeting).
        let mut cursor: Option<i64> = None;
        let mut failures: Vec<(i64, String)> = Vec::new();
        loop {
            let page = self
                .history_manager
                .get_history_entries(cursor, Some(INDEX_PAGE_SIZE))
                .await?;
            for entry in &page.entries {
                if !HistoryManager::is_meeting_entry(entry) {
                    continue;
                }
                if let Err(error) = self
                    .index_source(&entry, "transcript", &entry.transcription_text, &key)
                    .await
                {
                    failures.push((entry.id, error.to_string()));
                }
                if let Err(error) = self
                    .index_source(
                        &entry,
                        "summary",
                        entry.post_processed_text.as_deref().unwrap_or_default(),
                        &key,
                    )
                    .await
                {
                    failures.push((entry.id, error.to_string()));
                }
            }
            if !page.has_more {
                break;
            }
            cursor = page.entries.last().map(|entry| entry.id);
        }
        for (entry_id, error) in &failures {
            log::warn!("Failed to index entry {}: {}", entry_id, error);
        }
        Ok(())
    }

    async fn index_source(
        &self,
        entry: &HistoryEntry,
        source: &str,
        content: &str,
        api_key: &str,
    ) -> Result<()> {
        let chunks = split_chunks(content);
        let conn = Connection::open(self.history_manager.db_path())?;
        if chunks.is_empty() {
            conn.execute(
                "DELETE FROM rag_chunks WHERE entry_id = ?1 AND source = ?2",
                params![entry.id, source],
            )?;
            return Ok(());
        }

        let existing: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT content_hash FROM rag_chunks
                 WHERE entry_id = ?1 AND source = ?2
                   AND embedding_model = ?3 AND dimensions = ?4
                 ORDER BY chunk_index",
            )?;
            stmt.query_map(
                params![
                    entry.id,
                    source,
                    EMBEDDING_MODEL,
                    EMBEDDING_DIMENSIONS as i64
                ],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<String>>>()?
        };
        if existing.len() == chunks.len()
            && existing
                .iter()
                .zip(&chunks)
                .all(|(stored, chunk)| stored == &chunk.hash)
        {
            return Ok(());
        }

        let mut embeddings = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            embeddings.push(normalize(embed_text(api_key, &chunk.content).await?)?);
        }

        let mut conn = Connection::open(self.history_manager.db_path())?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM rag_chunks WHERE entry_id = ?1 AND source = ?2",
            params![entry.id, source],
        )?;
        for (chunk, embedding) in chunks.iter().zip(embeddings) {
            tx.execute(
                "INSERT INTO rag_chunks
                 (entry_id, source, chunk_index, content, content_hash, embedding_model, dimensions, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    entry.id,
                    source,
                    chunk.index as i64,
                    chunk.content,
                    chunk.hash,
                    EMBEDDING_MODEL,
                    EMBEDDING_DIMENSIONS as i64,
                    encode_embedding(&embedding),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    async fn refresh_count(&self, status: RagStatus, error: Option<String>) {
        let count = Connection::open(self.history_manager.db_path())
            .and_then(|conn| {
                conn.query_row("SELECT COUNT(*) FROM rag_chunks", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .unwrap_or(0)
            .max(0) as usize;
        let mut snapshot = self.status.write().await;
        *snapshot = RagStatusSnapshot {
            status,
            indexed_chunks: count,
            error,
        };
    }

    async fn set_status(&self, status: RagStatus, error: Option<String>) {
        let count = Connection::open(self.history_manager.db_path())
            .and_then(|conn| {
                conn.query_row("SELECT COUNT(*) FROM rag_chunks", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .unwrap_or(0)
            .max(0) as usize;
        let mut snapshot = self.status.write().await;
        *snapshot = RagStatusSnapshot {
            status,
            indexed_chunks: count,
            error,
        };
    }
}

fn user_google_api_key(settings: &AppSettings) -> Option<String> {
    credentials::get("google").or_else(|| {
        settings
            .post_process_api_keys
            .get("google")
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
    })
}

async fn embed_text(api_key: &str, text: &str) -> Result<Vec<f32>> {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:embedContent",
        EMBEDDING_MODEL
    );
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(45))
        .build()?;
    let response = client
        .post(url)
        .header("x-goog-api-key", HeaderValue::from_str(api_key)?)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .json(&EmbedRequest {
            model: EMBEDDING_MODEL,
            content: EmbedContent {
                parts: vec![EmbedPart { text }],
            },
            output_dimensionality: EMBEDDING_DIMENSIONS,
        })
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Gemini embedding request failed ({}): {}",
            status,
            body
        ));
    }
    let payload: EmbedResponse = response.json().await?;
    if payload.embedding.values.len() != EMBEDDING_DIMENSIONS {
        return Err(anyhow!(
            "Gemini returned {} dimensions; expected {}",
            payload.embedding.values.len(),
            EMBEDDING_DIMENSIONS
        ));
    }
    Ok(payload.embedding.values)
}

fn normalize(mut embedding: Vec<f32>) -> Result<Vec<f32>> {
    let norm = embedding
        .iter()
        .map(|value| (*value as f64) * (*value as f64))
        .sum::<f64>()
        .sqrt() as f32;
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(anyhow!("Gemini returned an invalid zero-length embedding"));
    }
    for value in &mut embedding {
        *value /= norm;
    }
    Ok(embedding)
}

fn encode_embedding(embedding: &[f32]) -> Vec<u8> {
    embedding
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode_embedding(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.len() % std::mem::size_of::<f32>() != 0 {
        return Err(anyhow!("Stored embedding has invalid byte length"));
    }
    Ok(bytes
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("f32-sized chunk")))
        .collect())
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn split_chunks(content: &str) -> Vec<ChunkInput> {
    debug_assert!(
        CHUNK_SIZE > CHUNK_OVERLAP,
        "CHUNK_SIZE must exceed CHUNK_OVERLAP or chunking never advances"
    );
    let content = content.trim();
    if content.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = content.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while start < chars.len() {
        let end = (start + CHUNK_SIZE).min(chars.len());
        let text: String = chars[start..end].iter().collect();
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let hash = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
        chunks.push(ChunkInput {
            index,
            content: text,
            hash,
        });
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_OVERLAP);
        index += 1;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::{decode_embedding, dot, encode_embedding, normalize, split_chunks};

    #[test]
    fn chunks_have_overlap_and_stable_hashes() {
        let input = "a".repeat(2400);
        let chunks = split_chunks(&input);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0].content.chars().count(), 1200);
        assert_eq!(chunks[0].hash, split_chunks(&input)[0].hash);
    }

    #[test]
    fn normalized_embeddings_round_trip() {
        let normalized = normalize(vec![3.0, 4.0]).unwrap();
        assert!((dot(&normalized, &normalized) - 1.0).abs() < 0.0001);
        assert_eq!(
            decode_embedding(&encode_embedding(&normalized)).unwrap(),
            normalized
        );
    }
}
