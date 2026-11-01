//! SQLite-backed implementation of the [`PreferenceStore`] trait.
//!
//! Schema (single table):
//!
//! ```sql
//! CREATE TABLE preferences (
//!   tenant_id  TEXT NOT NULL,
//!   user_id    TEXT NOT NULL,
//!   key        TEXT NOT NULL,
//!   value      TEXT NOT NULL,           -- JSON-encoded `serde_json::Value`
//!   updated_at TEXT NOT NULL,           -- RFC 3339 UTC timestamp
//!   version    INTEGER NOT NULL DEFAULT 1,
//!   PRIMARY KEY (tenant_id, user_id, key)
//! );
//! ```
//!
//! `put_preference` performs an UPSERT and increments `version` on
//! collision. `version` starts at `1` for a brand-new row and grows
//! monotonically per `(tenant_id, user_id, key)` triple.
//!
//! Encryption at rest is intentionally deferred. The trait shape allows
//! a future `EncryptedPreferenceStore` to slot in without breaking
//! existing callers — the local profile ships plaintext until P5 ships
//! the key-management plumbing.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqlitePool;

use crate::MemoryError;
use crate::layer::{PreferenceScope, PreferenceStore, PreferenceValue};
use crate::sqlite_pool;

/// Persistent SQLite-backed [`PreferenceStore`].
pub struct SqlitePreferenceStore {
  pool: SqlitePool,
}

impl SqlitePreferenceStore {
  /// Open (or create) a preference database at `path`.
  ///
  /// Q2.1.1/Q2.1.2: pool via `sqlite_pool::build_pool`.
  pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self, MemoryError> {
    let pool = sqlite_pool::build_pool(path).await?;
    let store = Self { pool };
    store.init_schema().await?;
    Ok(store)
  }

  /// In-memory database for tests and ephemeral sessions.
  pub async fn in_memory() -> Result<Self, MemoryError> {
    let pool = sqlite_pool::build_in_memory_pool().await?;
    let store = Self { pool };
    store.init_schema().await?;
    Ok(store)
  }

  async fn init_schema(&self) -> Result<(), MemoryError> {
    sqlx::query(
      "CREATE TABLE IF NOT EXISTS preferences (
        tenant_id  TEXT NOT NULL,
        user_id    TEXT NOT NULL,
        key        TEXT NOT NULL,
        value      TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        version    INTEGER NOT NULL DEFAULT 1,
        PRIMARY KEY (tenant_id, user_id, key)
      );",
    )
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    sqlx::query(
      "CREATE INDEX IF NOT EXISTS idx_preferences_scope
       ON preferences (tenant_id, user_id);",
    )
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    Ok(())
  }
}

fn parse_value(s: &str) -> Result<Value, MemoryError> {
  serde_json::from_str(s)
    .map_err(|e| MemoryError::StorageError(format!("invalid stored JSON: {e}")))
}

fn parse_ts(s: &str) -> DateTime<Utc> {
  DateTime::parse_from_rfc3339(s)
    .map(|dt| dt.with_timezone(&Utc))
    .unwrap_or_else(|_| Utc::now())
}

#[async_trait]
impl PreferenceStore for SqlitePreferenceStore {
  async fn get_preference(
    &self,
    scope: &PreferenceScope,
    key: &str,
  ) -> Result<Option<PreferenceValue>, MemoryError> {
    let row = sqlx::query(
      "SELECT value, updated_at, version
       FROM preferences
       WHERE tenant_id = ?1 AND user_id = ?2 AND key = ?3",
    )
    .bind(&scope.tenant_id)
    .bind(&scope.user_id)
    .bind(key)
    .fetch_optional(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    let Some(row) = row else {
      return Ok(None);
    };

    let value: String = row
      .try_get("value")
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    let updated_at: String = row
      .try_get("updated_at")
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    let version: i64 = row
      .try_get("version")
      .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    Ok(Some(PreferenceValue {
      value: parse_value(&value)?,
      updated_at: parse_ts(&updated_at),
      version,
    }))
  }

  async fn put_preference(
    &self,
    scope: &PreferenceScope,
    key: &str,
    value: Value,
  ) -> Result<(), MemoryError> {
    let json = serde_json::to_string(&value)?;
    let now = Utc::now().to_rfc3339();
    // ON CONFLICT bumps version monotonically per (tenant, user, key).
    sqlx::query(
      "INSERT INTO preferences (tenant_id, user_id, key, value, updated_at, version)
       VALUES (?1, ?2, ?3, ?4, ?5, 1)
       ON CONFLICT (tenant_id, user_id, key) DO UPDATE SET
         value      = excluded.value,
         updated_at = excluded.updated_at,
         version    = preferences.version + 1",
    )
    .bind(&scope.tenant_id)
    .bind(&scope.user_id)
    .bind(key)
    .bind(json)
    .bind(now)
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    Ok(())
  }

  async fn delete_preference(&self, scope: &PreferenceScope, key: &str) -> Result<(), MemoryError> {
    sqlx::query(
      "DELETE FROM preferences
       WHERE tenant_id = ?1 AND user_id = ?2 AND key = ?3",
    )
    .bind(&scope.tenant_id)
    .bind(&scope.user_id)
    .bind(key)
    .execute(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;
    Ok(())
  }

  async fn list_preferences(
    &self,
    scope: &PreferenceScope,
  ) -> Result<Vec<(String, PreferenceValue)>, MemoryError> {
    let rows = sqlx::query(
      "SELECT key, value, updated_at, version
       FROM preferences
       WHERE tenant_id = ?1 AND user_id = ?2
       ORDER BY key",
    )
    .bind(&scope.tenant_id)
    .bind(&scope.user_id)
    .fetch_all(&self.pool)
    .await
    .map_err(|e| MemoryError::StorageError(e.to_string()))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows.iter() {
      let key: String = row
        .try_get("key")
        .map_err(|e| MemoryError::StorageError(e.to_string()))?;
      let value: String = row
        .try_get("value")
        .map_err(|e| MemoryError::StorageError(e.to_string()))?;
      let updated_at: String = row
        .try_get("updated_at")
        .map_err(|e| MemoryError::StorageError(e.to_string()))?;
      let version: i64 = row
        .try_get("version")
        .map_err(|e| MemoryError::StorageError(e.to_string()))?;
      out.push((
        key,
        PreferenceValue {
          value: parse_value(&value)?,
          updated_at: parse_ts(&updated_at),
          version,
        },
      ));
    }
    Ok(out)
  }

  async fn prune_older_than(&self, older_than: Duration) -> Result<u64, MemoryError> {
    let chrono_dur = chrono::Duration::from_std(older_than)
      .map_err(|e| MemoryError::StorageError(format!("invalid retention duration: {e}")))?;
    let cutoff = Utc::now()
      .checked_sub_signed(chrono_dur)
      .ok_or_else(|| MemoryError::StorageError("retention cutoff underflow".to_string()))?;
    let result = sqlx::query("DELETE FROM preferences WHERE updated_at < ?1")
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

  async fn fresh_store() -> SqlitePreferenceStore {
    SqlitePreferenceStore::in_memory().await.expect("in_memory")
  }

  #[tokio::test]
  async fn put_get_delete_roundtrip() {
    let store = fresh_store().await;
    let scope = PreferenceScope::local("alice");

    assert!(
      store
        .get_preference(&scope, "tone")
        .await
        .unwrap()
        .is_none()
    );

    store
      .put_preference(&scope, "tone", json!("friendly"))
      .await
      .unwrap();
    let pv = store
      .get_preference(&scope, "tone")
      .await
      .unwrap()
      .expect("present");
    assert_eq!(pv.value, json!("friendly"));
    assert_eq!(pv.version, 1);

    store.delete_preference(&scope, "tone").await.unwrap();
    assert!(
      store
        .get_preference(&scope, "tone")
        .await
        .unwrap()
        .is_none()
    );
  }

  #[tokio::test]
  async fn put_twice_increments_version() {
    let store = fresh_store().await;
    let scope = PreferenceScope::local("alice");

    store
      .put_preference(&scope, "verbosity", json!("low"))
      .await
      .unwrap();
    store
      .put_preference(&scope, "verbosity", json!("high"))
      .await
      .unwrap();

    let pv = store
      .get_preference(&scope, "verbosity")
      .await
      .unwrap()
      .expect("present");
    assert_eq!(pv.value, json!("high"));
    assert_eq!(pv.version, 2);
  }

  #[tokio::test]
  async fn delete_missing_key_is_idempotent() {
    let store = fresh_store().await;
    let scope = PreferenceScope::local("ghost");
    // Should not error even though the row doesn't exist.
    store.delete_preference(&scope, "nope").await.unwrap();
  }

  #[tokio::test]
  async fn scopes_are_isolated() {
    let store = fresh_store().await;
    let alice = PreferenceScope::local("alice");
    let bob = PreferenceScope::local("bob");
    let tenant_b_alice = PreferenceScope::new("tenant_b", "alice");

    store
      .put_preference(&alice, "tone", json!("alice"))
      .await
      .unwrap();
    store
      .put_preference(&bob, "tone", json!("bob"))
      .await
      .unwrap();
    store
      .put_preference(&tenant_b_alice, "tone", json!("alice_b"))
      .await
      .unwrap();

    assert_eq!(
      store
        .get_preference(&alice, "tone")
        .await
        .unwrap()
        .unwrap()
        .value,
      json!("alice")
    );
    assert_eq!(
      store
        .get_preference(&bob, "tone")
        .await
        .unwrap()
        .unwrap()
        .value,
      json!("bob")
    );
    assert_eq!(
      store
        .get_preference(&tenant_b_alice, "tone")
        .await
        .unwrap()
        .unwrap()
        .value,
      json!("alice_b")
    );
  }

  #[tokio::test]
  async fn list_returns_all_keys_in_scope_sorted() {
    let store = fresh_store().await;
    let scope = PreferenceScope::local("alice");

    store
      .put_preference(&scope, "zulu", json!(1))
      .await
      .unwrap();
    store
      .put_preference(&scope, "alpha", json!(2))
      .await
      .unwrap();
    store
      .put_preference(&scope, "mike", json!(3))
      .await
      .unwrap();

    let all = store.list_preferences(&scope).await.unwrap();
    let keys: Vec<_> = all.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["alpha", "mike", "zulu"]);
  }

  #[tokio::test]
  async fn prune_older_than_removes_only_old_rows() {
    let store = fresh_store().await;
    let scope = PreferenceScope::local("alice");

    // Insert one row, then manually backdate its updated_at so prune can
    // catch it without needing a real time delay.
    store
      .put_preference(&scope, "tone", json!("old"))
      .await
      .unwrap();
    let backdated = (Utc::now() - chrono::Duration::days(30)).to_rfc3339();
    sqlx::query("UPDATE preferences SET updated_at = ?1 WHERE key = 'tone'")
      .bind(backdated)
      .execute(&store.pool)
      .await
      .unwrap();

    // Insert a fresh row that should survive the prune.
    store
      .put_preference(&scope, "verbosity", json!("fresh"))
      .await
      .unwrap();

    let dropped = store
      .prune_older_than(Duration::from_secs(7 * 24 * 3600))
      .await
      .unwrap();
    assert_eq!(dropped, 1);

    assert!(
      store
        .get_preference(&scope, "tone")
        .await
        .unwrap()
        .is_none()
    );
    assert!(
      store
        .get_preference(&scope, "verbosity")
        .await
        .unwrap()
        .is_some()
    );
  }

  #[tokio::test]
  async fn value_preserves_complex_json() {
    let store = fresh_store().await;
    let scope = PreferenceScope::local("alice");
    let complex = json!({
      "nested": {"a": 1, "b": [true, false, null]},
      "string": "ok",
    });

    store
      .put_preference(&scope, "profile", complex.clone())
      .await
      .unwrap();
    let pv = store
      .get_preference(&scope, "profile")
      .await
      .unwrap()
      .unwrap();
    assert_eq!(pv.value, complex);
  }
}
