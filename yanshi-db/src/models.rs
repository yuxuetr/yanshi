//! Strongly-typed row models for the gateway's nine-table schema
//! (`runs`, `steps`, `events`, `artifacts`, `skill_installs`,
//! `mcp_sessions`, `harness_sessions`, `harness_session_events`,
//! `user_preferences`).
//!
//! These structs round-trip through `sqlx::FromRow` for queries and `Serialize`
//! for HTTP responses, so they double as the wire format for `yanshi-server`
//! routes. Stick to plain owned types — no borrowed slices — to keep
//! `Repository` impls and async handlers simple.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// Lifecycle states a `runs` row can hold.
///
/// Stored as TEXT so future variants are additive without a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
  Queued,
  Running,
  Succeeded,
  Failed,
  Cancelled,
}

impl RunStatus {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Queued => "queued",
      Self::Running => "running",
      Self::Succeeded => "succeeded",
      Self::Failed => "failed",
      Self::Cancelled => "cancelled",
    }
  }

  /// Parse the canonical `runs.status` column into the typed enum.
  ///
  /// Returns `None` for unknown values; callers usually want to bubble that
  /// up as a 500 since it indicates DB / app drift. Named `parse` (not
  /// `from_str`) so it doesn't shadow the standard `std::str::FromStr`
  /// signature and clippy's `should_implement_trait` lint stays quiet.
  pub fn parse(value: &str) -> Option<Self> {
    Some(match value {
      "queued" => Self::Queued,
      "running" => Self::Running,
      "succeeded" => Self::Succeeded,
      "failed" => Self::Failed,
      "cancelled" => Self::Cancelled,
      _ => return None,
    })
  }
}

/// One row in the `runs` table.
///
/// `events_retention_days` / `artifacts_retention_days` are the
/// per-run override knobs introduced by P10.14.1 (migration
/// `0005_run_retention_overrides.sql`). Either field as `Some(N)`
/// asks the cleanup sweep to keep that resource for at least
/// `max(global_default, N)` days; `None` (the common case) falls
/// back entirely to the tenant + profile default. The override
/// only ever extends retention — it cannot shorten it.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Run {
  pub id: Uuid,
  pub workflow: String,
  pub status: String,
  pub started_at: DateTime<Utc>,
  pub finished_at: Option<DateTime<Utc>>,
  pub run_dir: Option<String>,
  pub tenant_id: String,
  pub error: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub events_retention_days: Option<i32>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub artifacts_retention_days: Option<i32>,
}

/// Input for creating a new run via [`crate::repo::RunRepo::create`].
#[derive(Debug, Clone)]
pub struct NewRun {
  pub id: Uuid,
  pub workflow: String,
  pub status: RunStatus,
  pub run_dir: Option<String>,
  pub tenant_id: String,
  /// Per-run override: keep `events` rows for at least this many
  /// days, regardless of the global tenant default. `None` ⇒ no
  /// override. Negative values are rejected at the server layer.
  pub events_retention_days: Option<i32>,
  /// Per-run override: keep `artifacts` rows for at least this many
  /// days. Same semantics as `events_retention_days`.
  pub artifacts_retention_days: Option<i32>,
}

/// One row in the `steps` table. `payload` is provider-specific JSON
/// captured at the source — typically the serialised step trace.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Step {
  pub run_id: Uuid,
  pub seq: i32,
  pub node_id: String,
  pub kind: String,
  pub status: String,
  pub started_at: DateTime<Utc>,
  pub duration_ms: Option<i64>,
  pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct NewStep {
  pub run_id: Uuid,
  pub seq: i32,
  pub node_id: String,
  pub kind: String,
  pub status: String,
  pub duration_ms: Option<i64>,
  pub payload: Option<serde_json::Value>,
}

/// One row in the `events` table. `seq` is monotonic per `run_id`; SSE
/// subscribers use it as a resume cursor. `tenant_id` mirrors the
/// owning run for direct row-level filtering (P2.6).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Event {
  pub run_id: Uuid,
  pub seq: i64,
  pub kind: String,
  pub payload: serde_json::Value,
  pub ts: DateTime<Utc>,
  #[serde(default = "default_tenant_id")]
  pub tenant_id: String,
}

fn default_tenant_id() -> String {
  "default".to_string()
}

#[derive(Debug, Clone)]
pub struct NewEvent {
  pub run_id: Uuid,
  pub seq: i64,
  pub kind: String,
  pub payload: serde_json::Value,
  /// Tenant scope. Defaults to `"default"` if not set so single-tenant
  /// local-dev callers stay zero-config.
  pub tenant_id: Option<String>,
}

/// One row in the `artifacts` table.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Artifact {
  pub id: Uuid,
  pub run_id: Uuid,
  pub node_id: String,
  pub name: String,
  pub path_or_url: String,
  pub mime_type: Option<String>,
  pub created_at: DateTime<Utc>,
  #[serde(default = "default_tenant_id")]
  pub tenant_id: String,
  /// Y1.3: byte length and hex SHA-256 recorded at registration; `None`
  /// on rows written before migration 0012.
  #[serde(default)]
  pub size_bytes: Option<i64>,
  #[serde(default)]
  pub sha256: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewArtifact {
  pub id: Uuid,
  pub run_id: Uuid,
  pub node_id: String,
  pub name: String,
  pub path_or_url: String,
  pub mime_type: Option<String>,
  pub tenant_id: Option<String>,
  pub size_bytes: Option<i64>,
  pub sha256: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SkillInstall {
  pub name: String,
  pub version: String,
  pub source: String,
  pub installed_at: DateTime<Utc>,
  pub checksum: Option<String>,
  #[serde(default = "default_tenant_id")]
  pub tenant_id: String,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct McpSession {
  pub id: Uuid,
  pub server: String,
  pub started_at: DateTime<Utc>,
  pub ended_at: Option<DateTime<Utc>>,
  pub tool_calls: i32,
  pub metadata: Option<serde_json::Value>,
  /// Tenant the MCP session was opened under. Defaults to `"default"`
  /// for backward-compat (Q1.5.2 added the column in migration 0006).
  #[serde(default = "default_tenant_id")]
  pub tenant_id: String,
}

/// Lifecycle states a `harness_sessions` row can hold.
///
/// Mirrors the closed enum of [`RunStatus`] but uses a Harness-specific
/// terminal vocabulary (`completed` / `failed` / `cancelled`). Stored as
/// TEXT so adding new variants is additive without a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessSessionStatus {
  Running,
  Completed,
  Failed,
  Cancelled,
  /// V2.3: paused waiting for the user to answer a question the agent
  /// asked (`AgentStopReason::AwaitingInput`). Deliberately NOT terminal
  /// — `is_terminal()` returns `false` for it — since the whole point is
  /// that the session resumes via `POST .../interrupt/answer` rather
  /// than being restarted via the existing `:resume` route (which
  /// requires `is_terminal() == true`).
  AwaitingInput,
}

impl HarnessSessionStatus {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Running => "running",
      Self::Completed => "completed",
      Self::Failed => "failed",
      Self::Cancelled => "cancelled",
      Self::AwaitingInput => "awaiting_input",
    }
  }

  /// Parse the canonical `harness_sessions.status` column into the typed
  /// enum. Returns `None` for unknown values; callers usually want to
  /// bubble that up as a 500 since it indicates DB / app drift. Named
  /// `parse` (not `from_str`) so it doesn't shadow `std::str::FromStr`.
  pub fn parse(value: &str) -> Option<Self> {
    Some(match value {
      "running" => Self::Running,
      "completed" => Self::Completed,
      "failed" => Self::Failed,
      "cancelled" => Self::Cancelled,
      "awaiting_input" => Self::AwaitingInput,
      _ => return None,
    })
  }

  /// Terminal statuses are not transitioned through again — used by the
  /// cancel route to short-circuit when a session has already finished.
  pub fn is_terminal(self) -> bool {
    matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
  }
}

/// One row in the `harness_sessions` table.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct HarnessSession {
  pub id: Uuid,
  pub tenant_id: String,
  pub status: String,
  pub user_input: String,
  pub workspace_root: String,
  pub profile: String,
  pub runtime_kind: String,
  pub model: String,
  pub skill_name: Option<String>,
  pub started_at: DateTime<Utc>,
  pub finished_at: Option<DateTime<Utc>>,
  pub final_answer: Option<String>,
  pub error: Option<String>,
  /// V2.3: the question the loop is paused on when `status ==
  /// "awaiting_input"`. `None` otherwise. Read directly by
  /// `GET /v1/harness/sessions/{id}/interrupt` without needing to
  /// deserialize the full loop checkpoint payload.
  pub pending_question: Option<String>,
  /// V2.3: the `AgentStep` index the paused question belongs to.
  /// Meaningless when `pending_question` is `None`.
  pub pending_question_step_index: Option<i64>,
}

/// Input for creating a new harness session via
/// [`crate::repo::HarnessSessionRepo::create`].
#[derive(Debug, Clone)]
pub struct NewHarnessSession {
  pub id: Uuid,
  pub tenant_id: String,
  pub user_input: String,
  pub workspace_root: String,
  pub profile: String,
  pub runtime_kind: String,
  pub model: String,
  pub skill_name: Option<String>,
}

/// One row in the `harness_session_events` table. `seq` is monotonic per
/// `session_id`; SSE subscribers use it as a resume cursor — same shape
/// and contract as the workflow `events` table, just scoped to a session.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct HarnessSessionEvent {
  pub session_id: Uuid,
  pub seq: i64,
  pub kind: String,
  pub payload: serde_json::Value,
  pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewHarnessSessionEvent {
  pub session_id: Uuid,
  pub seq: i64,
  pub kind: String,
  pub payload: serde_json::Value,
}

// ── P6.4 user preferences ────────────────────────────────────────────────

/// One row in the `user_preferences` table — a tenant-scoped UI / operator
/// preference (theme, default profile, event-filter expression, etc.).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct UserPreference {
  pub tenant_id: String,
  pub key: String,
  pub value: serde_json::Value,
  pub updated_at: DateTime<Utc>,
}

/// Input row for upsert. The repo stamps `updated_at` server-side.
#[derive(Debug, Clone)]
pub struct NewUserPreference {
  pub tenant_id: String,
  pub key: String,
  pub value: serde_json::Value,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn run_status_round_trips() {
    for status in [
      RunStatus::Queued,
      RunStatus::Running,
      RunStatus::Succeeded,
      RunStatus::Failed,
      RunStatus::Cancelled,
    ] {
      assert_eq!(RunStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(RunStatus::parse("unknown"), None);
  }

  #[test]
  fn harness_session_status_round_trips() {
    for status in [
      HarnessSessionStatus::Running,
      HarnessSessionStatus::Completed,
      HarnessSessionStatus::Failed,
      HarnessSessionStatus::Cancelled,
    ] {
      assert_eq!(HarnessSessionStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(HarnessSessionStatus::parse("queued"), None);
    assert!(HarnessSessionStatus::Completed.is_terminal());
    assert!(HarnessSessionStatus::Cancelled.is_terminal());
    assert!(!HarnessSessionStatus::Running.is_terminal());
  }
}
