use std::path::Path;

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};

use crate::sqlite_pool;
use crate::{MemoryError, MemoryStore, Message, Role};

/// Persistent SQLite-backed memory store for long-term conversation history.
///
/// Uses `sqlx` async I/O — no blocking thread overhead.
pub struct SqliteMemory {
  pool: SqlitePool,
}

impl SqliteMemory {
  /// Open (or create) a SQLite database file at `path`.
  ///
  /// Q2.1.1/Q2.1.2: connection goes through `sqlite_pool::build_pool`
  /// which applies WAL + `busy_timeout` + `foreign_keys` PRAGMAs and
  /// constructs the connect options from a raw path (no URL parsing,
  /// so paths with `?`/`#`/spaces no longer silently fail).
  pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self, MemoryError> {
    let pool = sqlite_pool::build_pool(path).await?;
    let store = Self { pool };
    store.init_schema().await?;
    Ok(store)
  }

  /// In-memory SQLite database — useful for tests or ephemeral sessions.
  pub async fn in_memory() -> Result<Self, MemoryError> {
    let pool = sqlite_pool::build_in_memory_pool().await?;
    let store = Self { pool };
    store.init_schema().await?;
    Ok(store)
  }

  async fn init_schema(&self) -> Result<(), MemoryError> {
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
      "CREATE INDEX IF NOT EXISTS idx_messages_session
                ON messages (session_id, timestamp);",
    )
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    Ok(())
  }
}

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
  // the caller can decide whether to repair, skip, or abort.
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

#[async_trait]
impl MemoryStore for SqliteMemory {
  async fn add_message(&self, message: Message) -> Result<(), MemoryError> {
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

    let mut messages: Vec<Message> = rows.iter().map(row_to_message).collect::<Result<_, _>>()?;

    messages.reverse(); // oldest-first
    Ok(messages)
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

    let mut messages: Vec<Message> = rows.iter().map(row_to_message).collect::<Result<_, _>>()?;

    messages.reverse();
    Ok(messages)
  }

  async fn clear_session(&self, session_id: &str) -> Result<(), MemoryError> {
    sqlx::query("DELETE FROM messages WHERE session_id = ?1")
      .bind(session_id)
      .execute(&self.pool)
      .await
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    Ok(())
  }

  async fn session_token_count(&self, session_id: &str) -> Result<u32, MemoryError> {
    let row = sqlx::query(
      "SELECT COALESCE(SUM(token_count), 0) as total FROM messages WHERE session_id = ?1",
    )
    .bind(session_id)
    .fetch_one(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    let total: i64 = row.try_get("total").unwrap_or(0);
    Ok(total as u32)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::Utc;
  use uuid::Uuid;

  /// Q2.10.1 regression: a row with a malformed UUID returns
  /// `StorageError`. Pre-fix the loader minted a fresh `Uuid::new_v4()`
  /// every time it was read, so the same row produced different
  /// message ids across reads and broke
  /// `AgentNodeResumeContract`'s message-id key.
  #[tokio::test]
  async fn row_to_message_returns_err_on_corrupt_uuid() {
    let mem = SqliteMemory::in_memory().await.unwrap();
    // Bypass the model layer to inject a corrupt id directly.
    sqlx::query(
      "INSERT INTO messages (id, session_id, role, content, timestamp, tool_name, token_count)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind("not-a-uuid")
    .bind("sess-corrupt")
    .bind("user")
    .bind("hello")
    .bind(Utc::now().to_rfc3339())
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

  /// Q2.10.2 regression: many concurrent `add_message` calls land
  /// without serializing through a `&mut self` borrow. Pre-fix the
  /// trait required `&mut self`, so the ReAct H3 parallel
  /// tool-call dispatcher was forced to acquire an outer lock and
  /// fanned out writes serially.
  #[tokio::test]
  async fn add_message_supports_concurrent_writers() {
    let mem = std::sync::Arc::new(SqliteMemory::in_memory().await.unwrap());
    let mut handles = Vec::new();
    for i in 0..32 {
      let mem = mem.clone();
      handles.push(tokio::spawn(async move {
        let msg = Message {
          id: Uuid::new_v4(),
          session_id: "sess-concurrent".into(),
          role: Role::User,
          content: format!("msg-{i}"),
          timestamp: Utc::now(),
          tool_name: None,
          token_count: 1,
        };
        mem.add_message(msg).await
      }));
    }
    for handle in handles {
      handle.await.unwrap().unwrap();
    }

    let all = mem.get_all("sess-concurrent").await.unwrap();
    assert_eq!(all.len(), 32, "every concurrent insert must land");
  }
}
