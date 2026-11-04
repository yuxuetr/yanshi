//! Approval protocol: [`ApprovalRequest`], [`ApprovalDecision`], and
//! the [`ApprovalProvider`] trait.
//!
//! The runtime emits an [`ApprovalRequest`] whenever a tool is gated by
//! policy or by a [`crate::PreToolHook`]. The active
//! [`ApprovalProvider`] (CLI prompt, server-backed UI, auto-allow for
//! tests) returns an [`ApprovalDecision`]. Both halves of the protocol
//! are serializable so CLI / server / Web UI all consume the same
//! envelope (HARNESS_MODE_EVOLUTION Risk 5).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use yanshi_tool::{ToolEffect, ToolIdempotency, ToolPermission, ToolSource};

use crate::HarnessError;

/// Coarse risk classification attached to an approval request. The
/// runtime derives this from a combination of tool source, declared
/// permissions, and idempotency. UI clients map it to colour / urgency
/// hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRisk {
  /// Read-only / sandboxed / idempotent action.
  Low,
  /// Mutates user-scoped state in a recoverable way.
  Medium,
  /// External side effects, network writes, or non-idempotent
  /// mutation.
  High,
  /// Action could be destructive, exfiltrate credentials, or otherwise
  /// require explicit operator acknowledgement.
  Critical,
}

impl ApprovalRisk {
  pub fn as_str(&self) -> &'static str {
    match self {
      Self::Low => "low",
      Self::Medium => "medium",
      Self::High => "high",
      Self::Critical => "critical",
    }
  }
}

/// Scope of an approval decision. A decision is always recorded against
/// the originating request; the scope tells the runtime how many future
/// requests for the same tool should reuse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
  /// Apply to the originating request only.
  Once,
  /// Apply to every subsequent request inside the current session.
  Session,
  /// Apply for the duration of a single agent run (a session may host
  /// multiple runs via resume).
  Run,
}

/// Terminal outcome of an approval flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
  /// Proceed with the tool call.
  Allow,
  /// Skip the tool call. The agent receives a structured denial.
  Deny,
  /// Skip the tool call *and* stop the agent loop (used for production
  /// fail-closed flows).
  DenyAndStop,
}

impl ApprovalOutcome {
  pub fn as_str(&self) -> &'static str {
    match self {
      Self::Allow => "allow",
      Self::Deny => "deny",
      Self::DenyAndStop => "deny_and_stop",
    }
  }

  /// True for outcomes that allow the runtime to proceed with the tool
  /// call.
  pub fn is_allow(&self) -> bool {
    matches!(self, Self::Allow)
  }
}

/// Wire envelope for a pending approval.
///
/// `id` is unique inside a session and is the join key between
/// [`ApprovalRequest`] and the corresponding [`ApprovalDecision`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
  /// Unique request id within the session (UUID recommended).
  pub id: String,
  /// Owning session.
  pub session_id: String,
  /// Agent step index that triggered the request.
  pub step_index: usize,
  /// Tool name as registered with `ToolRegistry`.
  pub tool: String,
  /// Optional tool source classification.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub source: Option<ToolSource>,
  /// Permission categories the tool requires.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub permissions: Vec<ToolPermission>,
  /// Idempotency classification of the call.
  #[serde(default)]
  pub idempotency: ToolIdempotency,
  /// Y7.2: `publish` when the call would put content in front of an
  /// outside audience; omitted (local) otherwise, so a consumer can filter
  /// the event stream for publish approvals.
  #[serde(default, skip_serializing_if = "ToolEffect::is_local")]
  pub effect: ToolEffect,
  /// Redacted/truncated summary of the tool parameters. Implementations
  /// MUST avoid embedding secrets or raw file contents here.
  pub params_summary: serde_json::Value,
  /// Coarse risk classification.
  pub risk: ApprovalRisk,
  /// Human-readable reason the request was raised (e.g. "shell tool in
  /// production profile", "first run of unverified plugin").
  pub reason: String,
  /// Timestamp the request was raised.
  pub requested_at: DateTime<Utc>,
  /// Optional deadline; once elapsed the runtime will treat the
  /// request as [`HarnessError::ApprovalTimeout`].
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub expires_at: Option<DateTime<Utc>>,
}

/// Wire envelope for an approval decision. Always carries the join key
/// (`request_id`) so consumers can correlate it to the originating
/// [`ApprovalRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDecision {
  /// Id of the originating request.
  pub request_id: String,
  /// Final outcome.
  pub decision: ApprovalOutcome,
  /// Scope the decision applies to.
  pub scope: ApprovalScope,
  /// Stable identifier of the decider: `user`, `policy:<name>`,
  /// `auto`, `timeout`, etc. Implementations should prefer fixed
  /// vocabulary so audit logs are easy to filter.
  pub decided_by: String,
  /// Timestamp the decision was finalized.
  pub decided_at: DateTime<Utc>,
  /// Optional operator-readable reason.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reason: Option<String>,
}

/// Async trait every approval provider implements.
///
/// Implementations are pluggable so the same runtime works in:
///
/// - CLI mode (blocking stdin prompt),
/// - non-interactive auto-allow (CI / tests with safe tools),
/// - non-interactive fail-closed (production),
/// - server-backed UI flow (decision arrives via HTTP).
#[async_trait]
pub trait ApprovalProvider: Send + Sync {
  /// Stable identifier (`cli`, `auto_allow`, `auto_deny`, `server`).
  fn name(&self) -> &str;

  /// Block until a decision is available or the request times out.
  ///
  /// Implementations MUST honor [`ApprovalRequest::expires_at`]; the
  /// runtime treats a missed deadline as an error rather than an
  /// implicit allow.
  async fn request(&self, request: ApprovalRequest) -> Result<ApprovalDecision, HarnessError>;
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::TimeZone;

  fn sample_request() -> ApprovalRequest {
    ApprovalRequest {
      id: "req-1".into(),
      session_id: "sess-1".into(),
      step_index: 3,
      tool: "shell".into(),
      source: Some(ToolSource::Builtin),
      permissions: vec![ToolPermission::ProcessExec],
      idempotency: ToolIdempotency::NonIdempotent,
      effect: Default::default(),
      params_summary: serde_json::json!({"cmd": "rm -rf /tmp/x"}),
      risk: ApprovalRisk::High,
      reason: "shell tool requires explicit approval in local profile".into(),
      requested_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
      expires_at: None,
    }
  }

  #[test]
  fn approval_request_round_trips() {
    let original = sample_request();
    let json = serde_json::to_string(&original).unwrap();
    let parsed: ApprovalRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, original);
  }

  #[test]
  fn approval_decision_round_trips() {
    let decision = ApprovalDecision {
      request_id: "req-1".into(),
      decision: ApprovalOutcome::Allow,
      scope: ApprovalScope::Session,
      decided_by: "user".into(),
      decided_at: Utc.timestamp_opt(1_700_000_010, 0).unwrap(),
      reason: Some("trusted command".into()),
    };
    let json = serde_json::to_string(&decision).unwrap();
    let parsed: ApprovalDecision = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, decision);
  }

  #[test]
  fn approval_outcome_is_allow_only_for_allow() {
    assert!(ApprovalOutcome::Allow.is_allow());
    assert!(!ApprovalOutcome::Deny.is_allow());
    assert!(!ApprovalOutcome::DenyAndStop.is_allow());
  }

  #[test]
  fn optional_fields_skip_serialization_when_absent() {
    let mut req = sample_request();
    req.source = None;
    req.permissions.clear();
    req.expires_at = None;
    let json = serde_json::to_value(&req).unwrap();
    assert!(json.get("source").is_none());
    assert!(json.get("permissions").is_none());
    assert!(json.get("expires_at").is_none());
  }
}
