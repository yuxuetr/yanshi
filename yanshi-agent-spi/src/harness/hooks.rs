//! Pre/post tool hook traits and the data they observe.
//!
//! Phase H0 defines the trait boundaries only; Phase H2 (`P-H.2`) wires
//! them into the runtime. Keeping the structs serde-friendly lets the
//! same payloads be reused by trace events and server-side admission
//! logs without re-modeling.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use yanshi_tool::{ToolEffect, ToolIdempotency, ToolPermission, ToolSource};

use crate::ApprovalRisk;
use crate::HarnessError;

/// Description of a tool call as observed by a [`PreToolHook`]. Mirrors
/// the corresponding [`crate::ToolCallRequestedPayload`] but is kept
/// separate so the hook trait can be reused by SDK-only consumers
/// without depending on the event envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingToolCall {
  /// Owning Harness session.
  pub session_id: String,
  /// Agent step index that produced the call.
  pub step_index: usize,
  /// Tool name as registered with `ToolRegistry`.
  pub tool: String,
  /// Tool source classification when known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub source: Option<ToolSource>,
  /// Permission categories the tool requires.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub permissions: Vec<ToolPermission>,
  /// Replay safety classification of the call.
  #[serde(default)]
  pub idempotency: ToolIdempotency,
  /// Y7.2: what the call does beyond its result; see [`ToolEffect`].
  #[serde(default, skip_serializing_if = "ToolEffect::is_local")]
  pub effect: ToolEffect,
  /// Redacted/truncated parameters surfaced to the hook. The runtime
  /// MUST redact secrets before constructing this struct.
  pub params: serde_json::Value,
  /// Timestamp the call was queued.
  pub requested_at: DateTime<Utc>,
}

/// Description of a tool call observed by a [`PostToolHook`] after the
/// tool returned (success or failure).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedToolCall {
  /// Owning Harness session.
  pub session_id: String,
  /// Agent step index that produced the call.
  pub step_index: usize,
  /// Tool name.
  pub tool: String,
  /// Tool source classification when known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub source: Option<ToolSource>,
  /// Permission categories that were granted.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub permissions: Vec<ToolPermission>,
  /// Whether the tool reported a failure.
  pub is_error: bool,
  /// Total tool latency in milliseconds.
  pub duration_ms: u64,
  /// Optional structured summary (truncated content, exit code, etc.).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub output_summary: Option<serde_json::Value>,
  /// Timestamp the call completed.
  pub completed_at: DateTime<Utc>,
}

/// Outcome a [`PreToolHook`] can return.
///
/// `Allow` lets the runtime dispatch the call. `RequireApproval` pauses
/// the agent and raises an [`crate::ApprovalRequest`]. `Deny` short-
/// circuits the call and surfaces a failure event without invoking the
/// tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PreToolDecision {
  /// Dispatch the tool call.
  Allow,
  /// Raise an [`crate::ApprovalRequest`] with the supplied risk and
  /// reason. The runtime will populate the rest of the request.
  RequireApproval {
    /// Risk classification surfaced to the approval provider.
    risk: ApprovalRisk,
    /// Operator-readable reason ("shell tool", "first call to plugin
    /// X", etc.).
    reason: String,
  },
  /// Skip the call without raising an approval request.
  Deny {
    /// Operator-readable reason persisted in the trace event.
    reason: String,
  },
}

impl PreToolDecision {
  /// True when the runtime can proceed without an approval round trip.
  pub fn is_allow(&self) -> bool {
    matches!(self, Self::Allow)
  }
}

/// Async hook invoked before a tool call is dispatched.
///
/// Implementations should be cheap and side-effect free; emit events
/// through the runtime's trace listener rather than touching state
/// directly. The runtime composes multiple hooks; the strictest
/// returned [`PreToolDecision`] wins.
#[async_trait]
pub trait PreToolHook: Send + Sync {
  /// Stable identifier (`policy`, `audit_log`, `risk_classifier`,
  /// etc.).
  fn name(&self) -> &str;

  /// Decide whether the call should proceed, require approval, or be
  /// denied outright.
  async fn before_tool(&self, call: &PendingToolCall) -> Result<PreToolDecision, HarnessError>;
}

/// Async hook invoked after a tool call finishes (success or failure).
///
/// Post hooks are advisory: a hook failure is recorded but does not
/// roll back the tool call.
#[async_trait]
pub trait PostToolHook: Send + Sync {
  /// Stable identifier.
  fn name(&self) -> &str;

  /// Observe the completed call. Errors are reported through trace
  /// events but never undo the tool invocation.
  async fn after_tool(&self, call: &CompletedToolCall) -> Result<(), HarnessError>;
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::TimeZone;

  #[test]
  fn pending_tool_call_round_trips() {
    let original = PendingToolCall {
      session_id: "sess".into(),
      step_index: 2,
      tool: "http".into(),
      source: Some(ToolSource::Builtin),
      permissions: vec![ToolPermission::Network],
      idempotency: ToolIdempotency::Idempotent,
      effect: Default::default(),
      params: serde_json::json!({"method": "GET", "url": "https://example.test"}),
      requested_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
    };
    let json = serde_json::to_string(&original).unwrap();
    let parsed: PendingToolCall = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, original);
  }

  #[test]
  fn completed_tool_call_round_trips() {
    let original = CompletedToolCall {
      session_id: "sess".into(),
      step_index: 2,
      tool: "http".into(),
      source: Some(ToolSource::Builtin),
      permissions: vec![ToolPermission::Network],
      is_error: false,
      duration_ms: 42,
      output_summary: Some(serde_json::json!({"status": 200})),
      completed_at: Utc.timestamp_opt(1_700_000_001, 0).unwrap(),
    };
    let json = serde_json::to_string(&original).unwrap();
    let parsed: CompletedToolCall = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, original);
  }

  #[test]
  fn pre_tool_decision_serializes_with_tag_and_payload() {
    let allow = PreToolDecision::Allow;
    let json = serde_json::to_value(&allow).unwrap();
    assert_eq!(json["decision"], "allow");
    assert!(allow.is_allow());

    let require = PreToolDecision::RequireApproval {
      risk: ApprovalRisk::High,
      reason: "shell call".into(),
    };
    let json = serde_json::to_value(&require).unwrap();
    assert_eq!(json["decision"], "require_approval");
    assert_eq!(json["risk"], "high");
    assert_eq!(json["reason"], "shell call");

    let deny = PreToolDecision::Deny {
      reason: "policy".into(),
    };
    let json = serde_json::to_value(&deny).unwrap();
    assert_eq!(json["decision"], "deny");
  }
}
