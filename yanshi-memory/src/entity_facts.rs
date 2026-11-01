//! SQLite-backed implementation of the [`EntityFactStore`] trait.
//!
//! Schema (single table):
//!
//! ```sql
//! CREATE TABLE entity_facts (
//!   entity_id            TEXT NOT NULL,
//!   fact_id              TEXT NOT NULL,
//!   attribute            TEXT NOT NULL,
//!   value                TEXT NOT NULL,           -- JSON
//!   source_message_id    TEXT,
//!   confidence           REAL NOT NULL,
//!   extracted_at         TEXT NOT NULL,           -- RFC 3339 UTC
//!   invalidated_at       TEXT,                    -- RFC 3339 UTC, nullable
//!   invalidation_reason  TEXT,
//!   PRIMARY KEY (entity_id, fact_id)
//! );
//! CREATE INDEX idx_entity_facts_entity ON entity_facts (entity_id);
//! ```
//!
//! Two facts about the same `(entity_id, attribute)` are intentionally kept
//! as separate rows (distinct `fact_id`). The agent runtime renders them
//! with per-row citations rather than merging them — see the design doc
//! `docs/MEMORY_LAYERING.md` §4 for the rationale.

use std::path::Path;
// Q2.1: pool construction (PRAGMA + safe path handling) is shared
// via `crate::sqlite_pool`.
use crate::sqlite_pool;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};

use crate::MemoryError;
use crate::layer::{EntityFact, EntityFactStore};

/// Persistent SQLite-backed [`EntityFactStore`].
pub struct SqliteEntityFactStore {
  pool: SqlitePool,
}

impl SqliteEntityFactStore {
  /// Open (or create) an entity-facts database at `path`.
  ///
  /// Q2.1.1/Q2.1.2: pool is built via the shared
  /// `sqlite_pool::build_pool` helper (WAL + busy_timeout + FK).
  pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self, MemoryError> {
    let pool = sqlite_pool::build_pool(path).await?;
    let store = Self { pool };
    store.init_schema().await?;
    Ok(store)
  }

  /// In-memory database for tests.
  pub async fn in_memory() -> Result<Self, MemoryError> {
    let pool = sqlite_pool::build_in_memory_pool().await?;
    let store = Self { pool };
    store.init_schema().await?;
    Ok(store)
  }

  async fn init_schema(&self) -> Result<(), MemoryError> {
    sqlx::query(
      "CREATE TABLE IF NOT EXISTS entity_facts (
        entity_id            TEXT NOT NULL,
        fact_id              TEXT NOT NULL,
        attribute            TEXT NOT NULL,
        value                TEXT NOT NULL,
        source_message_id    TEXT,
        confidence           REAL NOT NULL,
        extracted_at         TEXT NOT NULL,
        invalidated_at       TEXT,
        invalidation_reason  TEXT,
        PRIMARY KEY (entity_id, fact_id)
      );",
    )
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    sqlx::query(
      "CREATE INDEX IF NOT EXISTS idx_entity_facts_entity
       ON entity_facts (entity_id);",
    )
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    Ok(())
  }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
  DateTime::parse_from_rfc3339(s)
    .map(|dt| dt.with_timezone(&Utc))
    .unwrap_or_else(|_| Utc::now())
}

fn row_to_fact(row: &SqliteRow) -> Result<EntityFact, MemoryError> {
  let entity_id: String = row
    .try_get("entity_id")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let fact_id: String = row
    .try_get("fact_id")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let attribute: String = row
    .try_get("attribute")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let value_str: String = row
    .try_get("value")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let value: Value = serde_json::from_str(&value_str)
    .map_err(|e| MemoryError::StorageError(format!("invalid stored fact value: {e}")))?;
  let source_message_id: Option<String> = row
    .try_get("source_message_id")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let confidence_f: f64 = row
    .try_get("confidence")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let extracted_at: String = row
    .try_get("extracted_at")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let invalidated_at: Option<String> = row
    .try_get("invalidated_at")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
  let invalidation_reason: Option<String> = row
    .try_get("invalidation_reason")
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

  Ok(EntityFact {
    entity_id,
    fact_id,
    attribute,
    value,
    source_message_id,
    confidence: confidence_f as f32,
    extracted_at: parse_ts(&extracted_at),
    invalidated_at: invalidated_at.as_deref().map(parse_ts),
    invalidation_reason,
  })
}

#[async_trait]
impl EntityFactStore for SqliteEntityFactStore {
  async fn record_fact(&mut self, fact: EntityFact) -> Result<(), MemoryError> {
    let value_str = serde_json::to_string(&fact.value)?;
    sqlx::query(
      "INSERT INTO entity_facts (
         entity_id, fact_id, attribute, value, source_message_id,
         confidence, extracted_at, invalidated_at, invalidation_reason
       ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
       ON CONFLICT (entity_id, fact_id) DO UPDATE SET
         attribute            = excluded.attribute,
         value                = excluded.value,
         source_message_id    = excluded.source_message_id,
         confidence           = excluded.confidence,
         extracted_at         = excluded.extracted_at,
         invalidated_at       = excluded.invalidated_at,
         invalidation_reason  = excluded.invalidation_reason",
    )
    .bind(&fact.entity_id)
    .bind(&fact.fact_id)
    .bind(&fact.attribute)
    .bind(value_str)
    .bind(&fact.source_message_id)
    .bind(fact.confidence as f64)
    .bind(fact.extracted_at.to_rfc3339())
    .bind(fact.invalidated_at.map(|t| t.to_rfc3339()))
    .bind(&fact.invalidation_reason)
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    Ok(())
  }

  async fn get_facts(
    &self,
    entity_id: &str,
    include_invalidated: bool,
  ) -> Result<Vec<EntityFact>, MemoryError> {
    // Ordering by extracted_at (newest first) is what the agent runtime
    // wants when surfacing facts as citations.
    let sql = if include_invalidated {
      "SELECT entity_id, fact_id, attribute, value, source_message_id,
              confidence, extracted_at, invalidated_at, invalidation_reason
       FROM entity_facts
       WHERE entity_id = ?1
       ORDER BY extracted_at DESC"
    } else {
      "SELECT entity_id, fact_id, attribute, value, source_message_id,
              confidence, extracted_at, invalidated_at, invalidation_reason
       FROM entity_facts
       WHERE entity_id = ?1 AND invalidated_at IS NULL
       ORDER BY extracted_at DESC"
    };
    let rows = sqlx::query(sql)
      .bind(entity_id)
      .fetch_all(&self.pool)
      .await
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    rows.iter().map(row_to_fact).collect()
  }

  async fn invalidate_fact(
    &mut self,
    entity_id: &str,
    fact_id: &str,
    reason: &str,
  ) -> Result<(), MemoryError> {
    let now = Utc::now().to_rfc3339();
    let result = sqlx::query(
      "UPDATE entity_facts
       SET invalidated_at = ?1, invalidation_reason = ?2
       WHERE entity_id = ?3 AND fact_id = ?4 AND invalidated_at IS NULL",
    )
    .bind(now)
    .bind(reason)
    .bind(entity_id)
    .bind(fact_id)
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    if result.rows_affected() == 0 {
      // Surface as a NotFound-style error so callers don't silently
      // assume a successful invalidate when the fact doesn't exist or
      // was already invalidated.
      return Err(MemoryError::StorageError(format!(
        "no active fact `{fact_id}` under entity `{entity_id}` to invalidate"
      )));
    }
    Ok(())
  }

  async fn prune_invalidated(&mut self, older_than: Duration) -> Result<u64, MemoryError> {
    let chrono_dur = chrono::Duration::from_std(older_than)
      .map_err(|e| MemoryError::StorageError(format!("invalid retention duration: {e}")))?;
    let cutoff = Utc::now()
      .checked_sub_signed(chrono_dur)
      .ok_or_else(|| MemoryError::StorageError("retention cutoff underflow".to_string()))?;
    let result = sqlx::query(
      "DELETE FROM entity_facts
       WHERE invalidated_at IS NOT NULL AND invalidated_at < ?1",
    )
    .bind(cutoff.to_rfc3339())
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    Ok(result.rows_affected())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  async fn fresh_store() -> SqliteEntityFactStore {
    SqliteEntityFactStore::in_memory().await.expect("in_memory")
  }

  fn fact(entity: &str, id: &str, attribute: &str, value: Value) -> EntityFact {
    EntityFact::new(entity, id, attribute, value, 0.8)
  }

  #[tokio::test]
  async fn record_and_get_facts_roundtrip() {
    let mut store = fresh_store().await;
    let f = fact("user:alice", "fact_1", "tone", json!("formal")).with_source("msg_42");
    store.record_fact(f.clone()).await.unwrap();

    let got = store.get_facts("user:alice", false).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].fact_id, "fact_1");
    assert_eq!(got[0].value, json!("formal"));
    assert_eq!(got[0].source_message_id.as_deref(), Some("msg_42"));
    assert!((got[0].confidence - 0.8).abs() < 1e-6);
  }

  #[tokio::test]
  async fn multiple_facts_per_entity_are_not_merged() {
    let mut store = fresh_store().await;
    store
      .record_fact(fact("user:alice", "fact_1", "tone", json!("formal")))
      .await
      .unwrap();
    store
      .record_fact(fact("user:alice", "fact_2", "tone", json!("playful")))
      .await
      .unwrap();
    store
      .record_fact(fact("user:alice", "fact_3", "language", json!("zh")))
      .await
      .unwrap();

    let got = store.get_facts("user:alice", false).await.unwrap();
    assert_eq!(got.len(), 3, "all three facts must surface");
    let tone_count = got.iter().filter(|f| f.attribute == "tone").count();
    assert_eq!(
      tone_count, 2,
      "conflicting tone facts must remain separate rows"
    );
  }

  #[tokio::test]
  async fn invalidate_hides_fact_from_default_get() {
    let mut store = fresh_store().await;
    store
      .record_fact(fact("user:alice", "f1", "tone", json!("formal")))
      .await
      .unwrap();
    store
      .invalidate_fact("user:alice", "f1", "user retracted")
      .await
      .unwrap();

    // Default: invalidated rows hidden.
    let visible = store.get_facts("user:alice", false).await.unwrap();
    assert!(
      visible.is_empty(),
      "invalidated facts are hidden by default"
    );

    // include_invalidated = true surfaces the row with metadata.
    let all = store.get_facts("user:alice", true).await.unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].is_invalidated());
    assert_eq!(
      all[0].invalidation_reason.as_deref(),
      Some("user retracted")
    );
  }

  #[tokio::test]
  async fn invalidate_missing_fact_errors() {
    let mut store = fresh_store().await;
    let err = store
      .invalidate_fact("nope", "nope", "reason")
      .await
      .expect_err("invalidating non-existent fact must error");
    assert!(matches!(err, MemoryError::StorageError(_)), "got {err:?}");
  }

  #[tokio::test]
  async fn invalidate_already_invalidated_errors() {
    let mut store = fresh_store().await;
    store
      .record_fact(fact("user:alice", "f1", "tone", json!("x")))
      .await
      .unwrap();
    store
      .invalidate_fact("user:alice", "f1", "first")
      .await
      .unwrap();
    let err = store
      .invalidate_fact("user:alice", "f1", "second")
      .await
      .expect_err("re-invalidate must error");
    assert!(matches!(err, MemoryError::StorageError(_)));
  }

  #[tokio::test]
  async fn record_with_same_id_replaces_value() {
    let mut store = fresh_store().await;
    store
      .record_fact(fact("e1", "f1", "color", json!("blue")))
      .await
      .unwrap();
    store
      .record_fact(fact("e1", "f1", "color", json!("green")))
      .await
      .unwrap();
    let got = store.get_facts("e1", false).await.unwrap();
    assert_eq!(got.len(), 1, "same fact_id must overwrite, not duplicate");
    assert_eq!(got[0].value, json!("green"));
  }

  #[tokio::test]
  async fn prune_invalidated_only_drops_expired() {
    let mut store = fresh_store().await;
    // Two invalidated facts, one backdated, one fresh.
    store
      .record_fact(fact("e", "old", "x", json!(1)))
      .await
      .unwrap();
    store
      .record_fact(fact("e", "fresh", "x", json!(2)))
      .await
      .unwrap();
    store.invalidate_fact("e", "old", "ancient").await.unwrap();
    store.invalidate_fact("e", "fresh", "recent").await.unwrap();

    // Backdate `old` by 100 days.
    let backdated = (Utc::now() - chrono::Duration::days(100)).to_rfc3339();
    sqlx::query("UPDATE entity_facts SET invalidated_at = ?1 WHERE fact_id = 'old'")
      .bind(backdated)
      .execute(&store.pool)
      .await
      .unwrap();

    // 30-day prune drops `old` but keeps `fresh`.
    let dropped = store
      .prune_invalidated(Duration::from_secs(30 * 24 * 3600))
      .await
      .unwrap();
    assert_eq!(dropped, 1);

    let remaining = store.get_facts("e", true).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].fact_id, "fresh");
  }

  #[tokio::test]
  async fn prune_invalidated_skips_active_rows() {
    let mut store = fresh_store().await;
    store
      .record_fact(fact("e", "f1", "x", json!(1)))
      .await
      .unwrap();
    // Active rows must never be pruned even when the prune window is 0.
    let dropped = store
      .prune_invalidated(Duration::from_secs(0))
      .await
      .unwrap();
    assert_eq!(dropped, 0);
    assert_eq!(store.get_facts("e", true).await.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn entities_are_isolated() {
    let mut store = fresh_store().await;
    store
      .record_fact(fact("e1", "f", "color", json!("red")))
      .await
      .unwrap();
    store
      .record_fact(fact("e2", "f", "color", json!("blue")))
      .await
      .unwrap();

    let e1 = store.get_facts("e1", false).await.unwrap();
    let e2 = store.get_facts("e2", false).await.unwrap();
    assert_eq!(e1.len(), 1);
    assert_eq!(e2.len(), 1);
    assert_eq!(e1[0].value, json!("red"));
    assert_eq!(e2[0].value, json!("blue"));
  }
}
