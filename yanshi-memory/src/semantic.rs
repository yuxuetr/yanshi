//! Semantic (vector-augmented) memory backend for Yanshi agents.
//!
//! Stores conversation history in SQLite (same schema as [`crate::SqliteMemory`]) and
//! additionally indexes `User` / `Assistant` messages as embedding vectors so
//! that [`crate::MemoryStore::search`] performs cosine-similarity retrieval
//! instead of plain keyword matching.
//!
//! # Degradation
//! If the embedding model is unavailable (API error, missing key, …) the
//! backend degrades silently:
//! * `add_message` — message is stored without an embedding; a `WARN` trace is
//!   emitted.
//! * `search` — falls back to `LIKE`-based keyword search when no embeddings
//!   are stored for the session.
//!
//! This ensures the Agent continues to run even when the embedding service is
//! temporarily unreachable.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};
use yanshi_rag::embeddings::EmbeddingProvider;

use crate::sqlite_pool;
use crate::{MemoryError, MemoryStore, Message, Role};

// ── Public struct ─────────────────────────────────────────────────────────────

/// Token-windowed, embedding-augmented SQLite memory store.
///
/// Each `User` and `Assistant` message is embedded on write.
/// On [`MemoryStore::search`] the query is embedded and compared against stored
/// vectors via cosine similarity; if no embeddings are available it falls back
/// to `LIKE`-based keyword matching.
pub struct SemanticMemory {
  pool: SqlitePool,
  embedder: Arc<dyn EmbeddingProvider>,
  window_tokens: u32,
}

// ── Constructors ─────────────────────────────────────────────────────────────

impl SemanticMemory {
  /// Open (or create) a persistent SQLite database at `path`.
  ///
  /// Q2.1.1/Q2.1.2: pool via `sqlite_pool::build_pool`.
  pub async fn open<P: AsRef<Path>>(
    path: P,
    embedder: Arc<dyn EmbeddingProvider>,
    window_tokens: u32,
  ) -> Result<Self, MemoryError> {
    let pool = sqlite_pool::build_pool(path).await?;
    let store = Self {
      pool,
      embedder,
      window_tokens,
    };
    store.init_schema().await?;
    Ok(store)
  }

  /// In-memory SQLite database — suitable for tests or ephemeral sessions.
  pub async fn in_memory(
    embedder: Arc<dyn EmbeddingProvider>,
    window_tokens: u32,
  ) -> Result<Self, MemoryError> {
    let pool = sqlite_pool::build_in_memory_pool().await?;
    let store = Self {
      pool,
      embedder,
      window_tokens,
    };
    store.init_schema().await?;
    Ok(store)
  }
}

// ── Private helpers ───────────────────────────────────────────────────────────

impl SemanticMemory {
  // ── Schema ────────────────────────────────────────────────────────────────

  async fn init_schema(&self) -> Result<(), MemoryError> {
    // Conversation messages (identical to SqliteMemory)
    sqlx::query(
      "CREATE TABLE IF NOT EXISTS messages (
                id          TEXT NOT NULL PRIMARY KEY,
                session_id  TEXT NOT NULL,
                role        TEXT NOT NULL,
                content     TEXT NOT NULL,
                timestamp   TEXT NOT NULL,
                tool_name   TEXT,
                token_count INTEGER NOT NULL DEFAULT 1
            );",
    )
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    sqlx::query(
      "CREATE INDEX IF NOT EXISTS idx_sm_messages_session
                 ON messages (session_id, timestamp);",
    )
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    // Embedding vectors stored as little-endian f32 BLOBs.
    // Sparse: only User / Assistant messages are embedded.
    sqlx::query(
      "CREATE TABLE IF NOT EXISTS embeddings (
                message_id  TEXT NOT NULL PRIMARY KEY,
                session_id  TEXT NOT NULL,
                vector      BLOB NOT NULL
            );",
    )
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    sqlx::query(
      "CREATE INDEX IF NOT EXISTS idx_sm_embeddings_session
                 ON embeddings (session_id);",
    )
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    Ok(())
  }

  // ── Vector math ───────────────────────────────────────────────────────────

  /// Encode a `f32` slice as little-endian bytes for BLOB storage.
  fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
  }

  /// Decode a BLOB back into a `Vec<f32>`.
  fn blob_to_vec(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
  }

  /// Cosine similarity in \[−1, 1\].  Returns `0.0` on zero-magnitude or
  /// differently-dimensioned vectors.
  pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
      return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
      0.0
    } else {
      dot / (mag_a * mag_b)
    }
  }

  // ── Database helpers ──────────────────────────────────────────────────────

  /// Insert a message row; does not embed.
  async fn insert_message_row(&self, message: &Message) -> Result<(), MemoryError> {
    sqlx::query(
      "INSERT OR REPLACE INTO messages
                 (id, session_id, role, content, timestamp, tool_name, token_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(message.id.to_string())
    .bind(&message.session_id)
    .bind(message.role.as_str())
    .bind(&message.content)
    .bind(message.timestamp.to_rfc3339())
    .bind(&message.tool_name)
    .bind(message.token_count as i64)
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    Ok(())
  }

  /// Store an embedding vector for a message.
  async fn insert_embedding(
    &self,
    message_id: &str,
    session_id: &str,
    vector: &[f32],
  ) -> Result<(), MemoryError> {
    let blob = Self::vec_to_blob(vector);
    sqlx::query(
      "INSERT OR REPLACE INTO embeddings (message_id, session_id, vector)
             VALUES (?1, ?2, ?3)",
    )
    .bind(message_id)
    .bind(session_id)
    .bind(blob)
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    Ok(())
  }

  /// The `k` messages of `session_id` whose embeddings are most similar
  /// to `query_vec`, best first, with their cosine scores. `None` when
  /// the session has no embeddings (callers fall back to keyword search).
  /// The matched messages load in one query rather than one per hit.
  async fn ranked_messages(
    &self,
    session_id: &str,
    query_vec: &[f32],
    k: usize,
  ) -> Result<Option<Vec<(Message, f32)>>, MemoryError> {
    let rows = sqlx::query("SELECT message_id, vector FROM embeddings WHERE session_id = ?1")
      .bind(session_id)
      .fetch_all(&self.pool)
      .await
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    if rows.is_empty() {
      return Ok(None);
    }

    let mut scored: Vec<(String, f32)> = rows
      .iter()
      .filter_map(|row| {
        let msg_id: String = row.try_get("message_id").ok()?;
        let bytes: Vec<u8> = row.try_get("vector").ok()?;
        let score = Self::cosine_similarity(query_vec, &Self::blob_to_vec(&bytes));
        Some((msg_id, score))
      })
      .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    if scored.is_empty() {
      return Ok(Some(Vec::new()));
    }

    let placeholders = vec!["?"; scored.len()].join(", ");
    let sql = format!(
      "SELECT id, session_id, role, content, timestamp, tool_name, token_count
       FROM messages WHERE id IN ({placeholders})"
    );
    let mut fetch = sqlx::query(&sql);
    for (msg_id, _) in &scored {
      fetch = fetch.bind(msg_id);
    }
    let mut by_id: HashMap<String, Message> = fetch
      .fetch_all(&self.pool)
      .await
      .map_err(|e| MemoryError::StorageError(e.to_string()))?
      .iter()
      .filter_map(|row| {
        let id: String = row.try_get("id").ok()?;
        Some((id, row_to_message(row).ok()?))
      })
      .collect();
    Ok(Some(
      scored
        .into_iter()
        .filter_map(|(msg_id, score)| by_id.remove(&msg_id).map(|message| (message, score)))
        .collect(),
    ))
  }

  /// Keyword (LIKE) fallback used when embeddings are unavailable.
  async fn keyword_search(
    &self,
    session_id: &str,
    query: &str,
    limit: usize,
  ) -> Result<Vec<Message>, MemoryError> {
    let limit = limit.min(i64::MAX as usize) as i64;
    let like = format!("%{}%", query);
    let rows = sqlx::query(
      "SELECT id, session_id, role, content, timestamp, tool_name, token_count
             FROM messages
             WHERE session_id = ?1 AND content LIKE ?2
             ORDER BY timestamp DESC
             LIMIT ?3",
    )
    .bind(session_id)
    .bind(&like)
    .bind(limit)
    .fetch_all(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    let mut msgs: Vec<Message> = rows.iter().map(row_to_message).collect::<Result<_, _>>()?;
    msgs.reverse();
    Ok(msgs)
  }

  /// Evict oldest non-system messages until within the token budget.
  async fn prune(&self, session_id: &str) -> Result<(), MemoryError> {
    loop {
      let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(token_count), 0) FROM messages WHERE session_id = ?1",
      )
      .bind(session_id)
      .fetch_one(&self.pool)
      .await
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;

      if total as u32 <= self.window_tokens {
        break;
      }

      let oldest = sqlx::query(
        "SELECT id FROM messages
                 WHERE session_id = ?1 AND role != 'system'
                 ORDER BY timestamp ASC
                 LIMIT 1",
      )
      .bind(session_id)
      .fetch_optional(&self.pool)
      .await
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;

      match oldest {
        Some(row) => {
          let id: String = row
            .try_get("id")
            .map_err(|e| MemoryError::StorageError(e.to_string()))?;
          // Remove embedding before the message (foreign-key order)
          sqlx::query("DELETE FROM embeddings WHERE message_id = ?1")
            .bind(&id)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::StorageError(e.to_string()))?;
          sqlx::query("DELETE FROM messages WHERE id = ?1")
            .bind(&id)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::StorageError(e.to_string()))?;
        }
        None => break, // only system messages remain
      }
    }
    Ok(())
  }
}

// ── MemoryStore implementation ────────────────────────────────────────────────

#[async_trait]
impl MemoryStore for SemanticMemory {
  async fn add_message(&self, message: Message) -> Result<(), MemoryError> {
    let session_id = message.session_id.clone();
    let message_id = message.id.to_string();
    let should_embed = matches!(message.role, Role::User | Role::Assistant);

    // Always persist the message text
    self.insert_message_row(&message).await?;

    // Optionally embed — failure is non-fatal
    if should_embed {
      match self.embedder.embed_text(&message.content).await {
        Ok(vector) => {
          if let Err(e) = self
            .insert_embedding(&message_id, &session_id, &vector)
            .await
          {
            tracing::warn!(
                message_id = %message_id,
                "Failed to store embedding: {}",
                e
            );
          }
        }
        Err(e) => {
          tracing::warn!(
              message_id = %message_id,
              "Embedding failed (degrading to keyword search): {}",
              e
          );
        }
      }
    }

    // Enforce token window
    self.prune(&session_id).await
  }

  async fn get_history(&self, session_id: &str, limit: usize) -> Result<Vec<Message>, MemoryError> {
    let limit = limit.min(i64::MAX as usize) as i64;
    let rows = sqlx::query(
      "SELECT id, session_id, role, content, timestamp, tool_name, token_count
             FROM messages
             WHERE session_id = ?1
             ORDER BY timestamp DESC
             LIMIT ?2",
    )
    .bind(session_id)
    .bind(limit)
    .fetch_all(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    let mut msgs: Vec<Message> = rows.iter().map(row_to_message).collect::<Result<_, _>>()?;
    msgs.reverse();
    Ok(msgs)
  }

  async fn get_all(&self, session_id: &str) -> Result<Vec<Message>, MemoryError> {
    self.get_history(session_id, usize::MAX).await
  }

  async fn search(
    &self,
    session_id: &str,
    query: &str,
    limit: usize,
  ) -> Result<Vec<Message>, MemoryError> {
    // Attempt semantic search; degrade to keyword on any failure
    match self.embedder.embed_text(query).await {
      Ok(query_vec) => match self.ranked_messages(session_id, &query_vec, limit).await? {
        Some(ranked) => Ok(ranked.into_iter().map(|(message, _)| message).collect()),
        None => {
          tracing::debug!("No embeddings for session {session_id} — using keyword search");
          self.keyword_search(session_id, query, limit).await
        }
      },
      Err(e) => {
        tracing::warn!("Query embedding failed, using keyword search: {}", e);
        self.keyword_search(session_id, query, limit).await
      }
    }
  }

  async fn clear_session(&self, session_id: &str) -> Result<(), MemoryError> {
    // Remove embeddings first (to avoid orphaned rows)
    sqlx::query("DELETE FROM embeddings WHERE session_id = ?1")
      .bind(session_id)
      .execute(&self.pool)
      .await
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    sqlx::query("DELETE FROM messages WHERE session_id = ?1")
      .bind(session_id)
      .execute(&self.pool)
      .await
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    Ok(())
  }

  async fn session_token_count(&self, session_id: &str) -> Result<u32, MemoryError> {
    let row = sqlx::query(
      "SELECT COALESCE(SUM(token_count), 0) as total
             FROM messages WHERE session_id = ?1",
    )
    .bind(session_id)
    .fetch_one(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    let total: i64 = row.try_get("total").unwrap_or(0);
    Ok(total as u32)
  }
}

// ── SemanticMemoryStore implementation ───────────────────────────────────────
//
// New typed search API introduced under P4.7. Returns each match alongside
// its cosine similarity score so callers can apply their own threshold
// instead of inferring one from rank. The existing `MemoryStore::search`
// path stays for one stability tier (Beta) before being deprecated.

#[async_trait]
impl crate::layer::SemanticMemoryStore for SemanticMemory {
  async fn search_semantic(
    &self,
    session_id: Option<&str>,
    query: &str,
    k: usize,
  ) -> Result<Vec<(Message, f32)>, MemoryError> {
    // Today's SemanticMemory indexes per-session; cross-session search
    // is reserved for a future backend (Qdrant collection-level search).
    let session_id = session_id.ok_or_else(|| {
      MemoryError::StorageError(
        "SemanticMemory requires a session_id for search_semantic".to_string(),
      )
    })?;

    let query_vec = match self.embedder.embed_text(query).await {
      Ok(v) => v,
      Err(e) => {
        // Mirror MemoryStore::search behaviour: degrade to keyword search,
        // but emit empty score (0.0) since cosine isn't available.
        tracing::warn!("query embedding failed; degrading to keyword: {e}");
        let fallback = self.keyword_search(session_id, query, k).await?;
        return Ok(fallback.into_iter().map(|m| (m, 0.0)).collect());
      }
    };

    match self.ranked_messages(session_id, &query_vec, k).await? {
      Some(ranked) => Ok(ranked),
      None => {
        let fallback = self.keyword_search(session_id, query, k).await?;
        Ok(fallback.into_iter().map(|m| (m, 0.0)).collect())
      }
    }
  }
}

// ── Row deserialiser (private) ────────────────────────────────────────────────

fn row_to_message(row: &SqliteRow) -> Result<Message, MemoryError> {
  let id_str: String = row
    .try_get("id")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let session_id: String = row
    .try_get("session_id")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let role_str: String = row
    .try_get("role")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let content: String = row
    .try_get("content")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let ts_str: String = row
    .try_get("timestamp")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let tool_name: Option<String> = row
    .try_get("tool_name")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let token_count: i64 = row
    .try_get("token_count")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

  // Q2.10.1: pre-fix the `unwrap_or_else` fallbacks silently minted a
  // fresh UUID and a `now()` timestamp every time a row failed to
  // parse — making the same row return different ids/timestamps on
  // each read and breaking the `AgentNodeResumeContract` key
  // invariant (the resume contract assumes message ids are stable
  // across reads). Surface the parse failure as `StorageError` so
  // the caller can decide whether to repair, skip, or abort. This
  // mirrors the fix already applied to `sqlite.rs::row_to_message`,
  // which this file's copy had drifted from (V1.6).
  let id = uuid::Uuid::parse_str(&id_str).map_err(|err| {
    MemoryError::StorageError(format!(
      "corrupt row: invalid UUID '{id_str}' in messages.id: {err}"
    ))
  })?;
  let role = Role::from(role_str.as_str());
  let timestamp = chrono::DateTime::parse_from_rfc3339(&ts_str)
    .map(|dt| dt.with_timezone(&chrono::Utc))
    .map_err(|err| {
      MemoryError::StorageError(format!(
        "corrupt row (id={id}): invalid RFC3339 timestamp '{ts_str}': {err}"
      ))
    })?;

  Ok(Message {
    id,
    session_id,
    role,
    content,
    timestamp,
    tool_name,
    token_count: token_count as u32,
  })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use yanshi_rag::{embeddings::EmbeddingProvider, error::Result as RAGResult};

  // ── FixedEmbedding test stub ──────────────────────────────────────────────
  //
  // Returns deterministic unit vectors that vary by the first byte of the
  // input text.  Never calls an external API — safe to use in all tests.

  struct FixedEmbedding {
    dim: usize,
    call_count: Arc<AtomicUsize>,
    /// When `Some`, embed_text returns this error instead of a vector.
    fail_with: Option<String>,
  }

  impl FixedEmbedding {
    fn new(dim: usize) -> Self {
      Self {
        dim,
        call_count: Arc::new(AtomicUsize::new(0)),
        fail_with: None,
      }
    }

    fn always_fail(dim: usize, msg: impl Into<String>) -> Self {
      Self {
        dim,
        call_count: Arc::new(AtomicUsize::new(0)),
        fail_with: Some(msg.into()),
      }
    }

    fn calls(&self) -> usize {
      self.call_count.load(Ordering::SeqCst)
    }
  }

  #[async_trait]
  impl EmbeddingProvider for FixedEmbedding {
    async fn embed_text(&self, text: &str) -> RAGResult<Vec<f32>> {
      self.call_count.fetch_add(1, Ordering::SeqCst);

      if let Some(msg) = &self.fail_with {
        return Err(yanshi_rag::error::RAGError::embedding(msg.clone()));
      }

      // Vary the vector by first character so different texts get different
      // vectors, making cosine ranking deterministic and testable.
      let seed = text.chars().next().map(|c| c as u32).unwrap_or(1) as f32;
      let mut v = vec![0.0f32; self.dim];
      v[0] = seed.sin();
      if self.dim > 1 {
        v[1] = seed.cos();
      }
      // Normalize to unit length
      let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
      if mag > 0.0 {
        for x in &mut v {
          *x /= mag;
        }
      }
      Ok(v)
    }

    async fn embed_batch(&self, texts: Vec<&str>) -> RAGResult<Vec<Vec<f32>>> {
      let mut out = Vec::with_capacity(texts.len());
      for t in texts {
        out.push(self.embed_text(t).await?);
      }
      Ok(out)
    }

    fn dimension(&self) -> usize {
      self.dim
    }
    fn model_name(&self) -> &str {
      "fixed-test-embedding"
    }
  }

  // ── Helpers ───────────────────────────────────────────────────────────────

  async fn in_mem(dim: usize) -> SemanticMemory {
    SemanticMemory::in_memory(Arc::new(FixedEmbedding::new(dim)), 8_000)
      .await
      .expect("in_memory should succeed")
  }

  // ── cosine_similarity unit tests ──────────────────────────────────────────

  #[test]
  fn cosine_identical_vectors_is_one() {
    let v = vec![1.0, 0.0, 0.0];
    assert!((SemanticMemory::cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
  }

  #[test]
  fn cosine_orthogonal_vectors_is_zero() {
    let a = vec![1.0, 0.0];
    let b = vec![0.0, 1.0];
    assert!((SemanticMemory::cosine_similarity(&a, &b)).abs() < 1e-6);
  }

  #[test]
  fn cosine_opposite_vectors_is_minus_one() {
    let a = vec![1.0, 0.0];
    let b = vec![-1.0, 0.0];
    assert!((SemanticMemory::cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
  }

  #[test]
  fn cosine_zero_vector_returns_zero() {
    let a = vec![0.0, 0.0];
    let b = vec![1.0, 0.0];
    assert_eq!(SemanticMemory::cosine_similarity(&a, &b), 0.0);
  }

  #[test]
  fn cosine_mismatched_dimensions_returns_zero() {
    let a = vec![1.0, 0.0, 0.0];
    let b = vec![1.0, 0.0];
    assert_eq!(SemanticMemory::cosine_similarity(&a, &b), 0.0);
  }

  // ── vec_to_blob / blob_to_vec round-trip ─────────────────────────────────

  #[test]
  fn vec_blob_roundtrip() {
    let original = vec![1.5_f32, -2.7, 0.0, f32::MAX];
    let blob = SemanticMemory::vec_to_blob(&original);
    let recovered = SemanticMemory::blob_to_vec(&blob);
    assert_eq!(original.len(), recovered.len());
    for (a, b) in original.iter().zip(recovered.iter()) {
      assert_eq!(a.to_bits(), b.to_bits(), "f32 bits must survive round-trip");
    }
  }

  // ── add_message / get_all ─────────────────────────────────────────────────

  #[tokio::test]
  async fn add_and_retrieve_messages() {
    let mem = in_mem(4).await;
    let sid = "session-1";

    mem
      .add_message(Message::user(sid, "Hello, world!"))
      .await
      .unwrap();
    mem
      .add_message(Message::assistant(sid, "Hi there!"))
      .await
      .unwrap();

    let all = mem.get_all(sid).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].content, "Hello, world!");
    assert_eq!(all[1].content, "Hi there!");
  }

  /// V1.6 regression: a row with a malformed UUID returns `StorageError`
  /// instead of silently minting a fresh `Uuid::new_v4()` — this file's
  /// copy of `row_to_message` had drifted from the fix already applied to
  /// `sqlite.rs` under Q2.10.1 (same bug class: pre-fix, the same corrupt
  /// row produced a different message id on every read, breaking
  /// `AgentNodeResumeContract`'s message-id key).
  #[tokio::test]
  async fn row_to_message_returns_err_on_corrupt_uuid() {
    let mem = in_mem(4).await;
    sqlx::query(
      "INSERT INTO messages (id, session_id, role, content, timestamp, tool_name, token_count)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind("not-a-uuid")
    .bind("sess-corrupt")
    .bind("user")
    .bind("hello")
    .bind(chrono::Utc::now().to_rfc3339())
    .bind::<Option<String>>(None)
    .bind(1_i64)
    .execute(&mem.pool)
    .await
    .unwrap();

    let err = mem
      .get_all("sess-corrupt")
      .await
      .expect_err("corrupt row must surface as error");
    let msg = err.to_string();
    assert!(
      msg.contains("invalid UUID") || msg.contains("corrupt row"),
      "expected corrupt-row error, got: {msg}"
    );
  }

  #[tokio::test]
  async fn system_messages_are_stored_but_not_embedded() {
    let embedder = Arc::new(FixedEmbedding::new(4));
    let calls_before = embedder.calls();
    let mem = SemanticMemory::in_memory(embedder.clone(), 8_000)
      .await
      .unwrap();

    mem
      .add_message(Message::system("s", "You are helpful."))
      .await
      .unwrap();

    // System messages must NOT trigger an embedding call
    assert_eq!(
      embedder.calls(),
      calls_before,
      "system message should not be embedded"
    );
    // But it should still be stored
    assert_eq!(mem.get_all("s").await.unwrap().len(), 1);
  }

  // ── semantic search ───────────────────────────────────────────────────────

  #[tokio::test]
  async fn search_returns_semantically_closest_message() {
    let mem = in_mem(4).await;
    let sid = "search-session";

    // The FixedEmbedding varies vectors by first character.
    // We add several messages and then search for one whose first char
    // should have the highest similarity to the query.
    mem.add_message(Message::user(sid, "apple")).await.unwrap();
    mem.add_message(Message::user(sid, "banana")).await.unwrap();
    mem.add_message(Message::user(sid, "cherry")).await.unwrap();

    // Query starts with 'a' — should rank "apple" highest
    let results = mem.search(sid, "avocado", 1).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "apple");
  }

  #[tokio::test]
  async fn search_semantic_keeps_score_order_across_hits() {
    use crate::layer::SemanticMemoryStore;
    let mem = in_mem(4).await;
    let sid = "order-session";
    for text in ["cherry", "apple", "banana", "date"] {
      mem.add_message(Message::user(sid, text)).await.unwrap();
    }

    let hits = mem.search_semantic(Some(sid), "avocado", 3).await.unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].0.content, "apple");
    assert!(
      hits.windows(2).all(|pair| pair[0].1 >= pair[1].1),
      "best first: {:?}",
      hits
        .iter()
        .map(|(m, s)| (&m.content, s))
        .collect::<Vec<_>>()
    );
  }

  #[tokio::test]
  async fn search_returns_up_to_limit_messages() {
    let mem = in_mem(4).await;
    let sid = "limit-session";

    for i in 0..5 {
      mem
        .add_message(Message::user(sid, format!("message {i}")))
        .await
        .unwrap();
    }

    let results = mem.search(sid, "message 0", 3).await.unwrap();
    assert!(results.len() <= 3, "must not exceed requested limit");
  }

  // ── keyword fallback ──────────────────────────────────────────────────────

  #[tokio::test]
  async fn search_falls_back_to_keyword_when_embedding_fails() {
    let failing_embedder = Arc::new(FixedEmbedding::always_fail(4, "API unavailable"));
    let mem = SemanticMemory::in_memory(failing_embedder, 8_000)
      .await
      .unwrap();
    let sid = "fallback-session";

    // Messages are stored even though embedding fails
    mem
      .add_message(Message::user(sid, "rust is fast"))
      .await
      .unwrap();
    mem
      .add_message(Message::user(sid, "python is slow"))
      .await
      .unwrap();

    // Search should fall back to LIKE matching
    let results = mem.search(sid, "rust", 10).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "rust is fast");
  }

  #[tokio::test]
  async fn search_falls_back_to_keyword_when_no_embeddings_stored() {
    // Use a failing embedder so no embeddings are written
    let failing = Arc::new(FixedEmbedding::always_fail(4, "offline"));
    let mem = SemanticMemory::in_memory(failing, 8_000).await.unwrap();
    let sid = "no-emb-session";

    mem
      .add_message(Message::user(sid, "hello keyword"))
      .await
      .unwrap();
    mem
      .add_message(Message::user(sid, "other message"))
      .await
      .unwrap();

    // Keyword fallback should still work
    let results = mem.search(sid, "keyword", 5).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("keyword"));
  }

  // ── clear_session ─────────────────────────────────────────────────────────

  #[tokio::test]
  async fn clear_session_removes_messages_and_embeddings() {
    let mem = in_mem(4).await;
    let sid = "clear-session";

    mem
      .add_message(Message::user(sid, "to be cleared"))
      .await
      .unwrap();
    mem.clear_session(sid).await.unwrap();

    assert_eq!(mem.get_all(sid).await.unwrap().len(), 0);

    // Embeddings table must also be empty for the session
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM embeddings WHERE session_id = ?1")
      .bind(sid)
      .fetch_one(&mem.pool)
      .await
      .unwrap();
    assert_eq!(count, 0);
  }

  // ── token-window pruning ──────────────────────────────────────────────────

  #[tokio::test]
  async fn prune_evicts_oldest_non_system_messages() {
    // Very tight window: 10 tokens
    let embedder = Arc::new(FixedEmbedding::new(4));
    let mem = SemanticMemory::in_memory(embedder, 10).await.unwrap();
    let sid = "prune-session";

    mem.add_message(Message::system(sid, "sys")).await.unwrap(); // ~1 token, preserved
    mem
      .add_message(Message::user(sid, "message one"))
      .await
      .unwrap(); // ~3 tokens
    mem
      .add_message(Message::user(sid, "message two"))
      .await
      .unwrap(); // ~3 tokens
    mem
      .add_message(Message::user(sid, "message three and more"))
      .await
      .unwrap(); // ~6 tokens

    let remaining = mem.get_all(sid).await.unwrap();
    let total_tokens: u32 = remaining.iter().map(|m| m.token_count).sum();
    assert!(total_tokens <= 10, "token count must be within window");

    // System message must never be evicted
    assert!(
      remaining.iter().any(|m| m.role == crate::Role::System),
      "system message should survive pruning"
    );
  }

  // ── session_token_count ───────────────────────────────────────────────────

  #[tokio::test]
  async fn session_token_count_matches_stored_messages() {
    let mem = in_mem(4).await;
    let sid = "token-count-session";

    mem.add_message(Message::user(sid, "hello")).await.unwrap();
    mem
      .add_message(Message::assistant(sid, "world"))
      .await
      .unwrap();

    let stored = mem.get_all(sid).await.unwrap();
    let expected: u32 = stored.iter().map(|m| m.token_count).sum();
    let counted = mem.session_token_count(sid).await.unwrap();
    assert_eq!(counted, expected);
  }

  // ── cross-session isolation ───────────────────────────────────────────────

  #[tokio::test]
  async fn search_is_isolated_per_session() {
    let mem = in_mem(4).await;

    mem
      .add_message(Message::user("sess-a", "alpha message"))
      .await
      .unwrap();
    mem
      .add_message(Message::user("sess-b", "beta message"))
      .await
      .unwrap();

    // Search in sess-a must not return sess-b's messages
    let results_a = mem.search("sess-a", "message", 10).await.unwrap();
    assert!(results_a.iter().all(|m| m.session_id == "sess-a"));

    let results_b = mem.search("sess-b", "message", 10).await.unwrap();
    assert!(results_b.iter().all(|m| m.session_id == "sess-b"));
  }
}
