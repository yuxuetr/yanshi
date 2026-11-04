//! Harness Mode event envelope.
//!
//! Every Harness session emits a stream of [`HarnessEvent`]s. CLI mode
//! (`yanshi harness run --output stream-json`) renders them as
//! line-delimited JSON; the local server forwards them over SSE; the
//! Web UI subscribes to the same stream. Trace replay decodes the same
//! envelope.
//!
//! The envelope shape is:
//!
//! ```json
//! {
//!   "seq": 0,
//!   "session_id": "abc",
//!   "ts": "2026-05-14T12:34:56Z",
//!   "kind": "session_started",
//!   "payload": { ... }
//! }
//! ```
//!
//! Serialization uses `#[serde(tag = "kind", content = "payload",
//! rename_all = "snake_case")]` so the inner enum's discriminant maps
//! onto `kind` and the variant body sits under `payload`. The envelope
//! is intentionally **closed**: new variants are additive Yanshi
//! releases. Trace replay tooling depends on the closed surface.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ApprovalDecision, ApprovalRequest};
use crate::{HarnessProfile, HarnessRuntimeKind};

/// Top-level wire envelope. The runtime emits one of these per
/// observable event.
///
/// `seq` is monotonically increasing per session, starts at `0`, and
/// must never gap. Consumers reconnect with `after_seq=N` to resume
/// streaming.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessEvent {
  /// Monotonic event sequence inside the session.
  pub seq: u64,
  /// Owning session id.
  pub session_id: String,
  /// Wall-clock timestamp at emission.
  pub ts: DateTime<Utc>,
  /// The kind + payload pair.
  #[serde(flatten)]
  pub body: HarnessEventBody,
}

/// Discriminated payload for [`HarnessEvent`].
///
/// The set of variants is frozen as part of Phase H0
/// (`P-H.0` in `TODOs.md`):
///
/// - `session_started`
/// - `step_started`
/// - `tool_call_requested`
/// - `approval_requested`
/// - `approval_decided`
/// - `tool_call_completed`
/// - `background_task_updated`
/// - `memory_summary_added`
/// - `stopped`
///
/// `token_delta` was added post-H0 (`P-V2.2`) as an additive,
/// live-bridge-only kind: it is only ever emitted while translating a
/// live [`crate::AgentEvent::TokenDelta`] stream, never reconstructed by
/// the post-hoc `translate_inner_events` fallback. Per `docs/STABILITY.md`
/// this keeps the envelope on wire version `harness/1` (additive kinds do
/// not bump the wire version).
///
/// `interrupt_requested` / `interrupt_answered` were added post-H0
/// (`P-V2.3`), also additive, mirroring the shape of
/// `approval_requested`/`approval_decided`: a general "the agent asked
/// the user a question" pair, distinct from the tool-call approval gate.
/// `interrupt_requested` is bridged live from
/// [`crate::AgentEvent::InterruptRequested`]; `interrupt_answered` has no
/// agent-loop-level event counterpart (the answer arrives from outside
/// the loop entirely) and is stamped directly by the resume-with-answer
/// path, the same split `approval_requested`/`approval_decided` already
/// use for the same reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum HarnessEventBody {
  /// Session bootstrap completed; context providers have been resolved
  /// and the underlying agent loop is about to start.
  SessionStarted(SessionStartedPayload),
  /// A new agent step has begun.
  StepStarted(StepStartedPayload),
  /// The agent requested a tool call. The runtime has not yet decided
  /// whether the call will proceed.
  ToolCallRequested(ToolCallRequestedPayload),
  /// A policy or [`crate::PreToolHook`] gated the call; an approval
  /// round-trip is in flight.
  ApprovalRequested(ApprovalRequestedPayload),
  /// The approval round-trip finished.
  ApprovalDecided(ApprovalDecidedPayload),
  /// The agent asked the user a question and the loop paused. V2.3,
  /// additive — see the enum-level doc comment.
  InterruptRequested(InterruptRequestedPayload),
  /// The user's answer arrived and the loop is resuming. V2.3, additive.
  InterruptAnswered(InterruptAnsweredPayload),
  /// The tool call returned (success or failure).
  ToolCallCompleted(ToolCallCompletedPayload),
  /// A raw token-level delta from the underlying LLM stream. Live-bridge
  /// only; see the enum-level doc comment.
  TokenDelta(TokenDeltaPayload),
  /// A managed background task changed state.
  BackgroundTaskUpdated(BackgroundTaskUpdatedPayload),
  /// The memory subsystem appended a summary entry.
  MemorySummaryAdded(MemorySummaryAddedPayload),
  /// The session is terminating (any reason).
  Stopped(StoppedPayload),
}

/// Payload for [`HarnessEventBody::SessionStarted`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStartedPayload {
  /// Filesystem root resolved for the session.
  pub workspace_root: String,
  /// Underlying agent runtime kind.
  pub runtime: HarnessRuntimeKind,
  /// Active security profile.
  pub profile: HarnessProfile,
  /// Resolved model identifier.
  pub model: String,
  /// Skills that were loaded for the session (stable names, no
  /// secrets).
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub skills: Vec<String>,
  /// Number of context items the providers contributed.
  pub context_item_count: usize,
  /// Approximate token cost of the context bundle.
  pub context_token_estimate: usize,
}

/// Payload for [`HarnessEventBody::StepStarted`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepStartedPayload {
  /// Zero-based step index.
  pub step_index: usize,
  /// Stable step kind (`plan`, `observe`, `tool_call`, `reflect`, ...).
  pub step_type: String,
}

/// Payload for [`HarnessEventBody::ToolCallRequested`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequestedPayload {
  /// Step that requested the call.
  pub step_index: usize,
  /// Tool name as registered with `ToolRegistry`.
  pub tool: String,
  /// Tool source classification (`builtin`, `mcp`, ...). Optional so
  /// custom runtimes that do not preserve source still produce a valid
  /// payload.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub source: Option<String>,
  /// Permission category identifiers (`filesystem_read`, ...). Strings
  /// instead of the typed enum to keep replay tolerant of additive
  /// permissions in newer Yanshi versions.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub permissions: Vec<String>,
  /// Idempotency classification.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub idempotency: Option<String>,
  /// Redacted/truncated parameter summary.
  pub params_summary: serde_json::Value,
}

/// Payload for [`HarnessEventBody::ApprovalRequested`]; mirrors the
/// request envelope verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequestedPayload {
  /// The request the runtime would like resolved.
  pub request: ApprovalRequest,
}

/// Payload for [`HarnessEventBody::ApprovalDecided`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDecidedPayload {
  /// The decision returned by the [`crate::ApprovalProvider`].
  pub decision: ApprovalDecision,
}

/// Payload for [`HarnessEventBody::InterruptRequested`] (V2.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptRequestedPayload {
  /// Step that asked the question.
  pub step_index: usize,
  /// The question text, verbatim from the agent.
  pub question: String,
}

/// Payload for [`HarnessEventBody::InterruptAnswered`] (V2.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptAnsweredPayload {
  /// Step the answer resumes (matches the triggering
  /// [`InterruptRequestedPayload::step_index`]).
  pub step_index: usize,
  /// The user's answer, verbatim.
  pub answer: String,
}

/// Payload for [`HarnessEventBody::ToolCallCompleted`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallCompletedPayload {
  /// Step that produced the call.
  pub step_index: usize,
  /// Tool name.
  pub tool: String,
  /// Whether the tool reported failure.
  pub is_error: bool,
  /// Total tool latency in milliseconds.
  pub duration_ms: u64,
  /// Optional source classification.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub source: Option<String>,
  /// Optional structured output summary.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub output_summary: Option<serde_json::Value>,
}

/// Payload for [`HarnessEventBody::TokenDelta`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenDeltaPayload {
  /// Step the delta belongs to.
  pub step_index: usize,
  /// Raw text fragment as emitted by the provider stream, forwarded
  /// verbatim without reconstruction or normalization.
  pub delta: String,
}

/// Lifecycle state of a managed background task. Mirrors the eventual
/// task-runtime states (`P-H.4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
  /// Spawned but not yet started.
  Pending,
  /// Currently executing.
  Running,
  /// Finished successfully.
  Completed,
  /// Finished with an error.
  Failed,
  /// Cancelled by parent or user.
  Cancelled,
}

/// Payload for [`HarnessEventBody::BackgroundTaskUpdated`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackgroundTaskUpdatedPayload {
  /// Stable task id (UUID recommended).
  pub task_id: String,
  /// Current lifecycle status.
  pub status: BackgroundTaskStatus,
  /// Optional short summary the agent sees through `task_get`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub summary: Option<String>,
  /// Optional error string when `status == Failed`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
}

/// Payload for [`HarnessEventBody::MemorySummaryAdded`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySummaryAddedPayload {
  /// Memory layer that produced the summary (`session`, `semantic`,
  /// `preference`, etc.). Strings to stay compatible with the future
  /// `MemoryLayer` enum (`P4.5`).
  pub layer: String,
  /// The added summary text.
  pub summary: String,
  /// Approximate token cost of the summary.
  pub token_estimate: usize,
}

/// Terminal reason for [`HarnessEventBody::Stopped`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
  /// Agent emitted a final answer.
  Completed,
  /// Cancelled by the user / parent / cancellation token.
  Cancelled,
  /// Hit a runtime limit (steps, tool calls, timeout, token budget).
  LimitReached,
  /// An approval was denied with stop semantics.
  ApprovalDenied,
  /// Unrecoverable runtime error.
  Failed,
  /// V2.3: paused waiting for the user to answer a question the agent
  /// asked. Resumable via `POST .../interrupt/answer`; the question
  /// text itself lives on the preceding `InterruptRequested` event, not
  /// duplicated here.
  AwaitingInput,
}

/// Payload for [`HarnessEventBody::Stopped`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoppedPayload {
  /// Terminal reason classification.
  pub reason: StopReason,
  /// Optional final answer text when `reason == Completed`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub final_answer: Option<String>,
  /// Optional error string when `reason` is `Failed` / `LimitReached` /
  /// `ApprovalDenied`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
}

impl HarnessEvent {
  /// Convenience constructor used in tests / fixtures.
  pub fn new(seq: u64, session_id: impl Into<String>, body: HarnessEventBody) -> Self {
    Self {
      seq,
      session_id: session_id.into(),
      ts: Utc::now(),
      body,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ApprovalOutcome, ApprovalRisk, ApprovalScope};
  use chrono::TimeZone;
  use yanshi_tool::{ToolIdempotency, ToolPermission, ToolSource};

  fn ts() -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000, 0).unwrap()
  }

  fn roundtrip<T>(value: &T) -> T
  where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
  {
    let json = serde_json::to_string(value).expect("serialize");
    serde_json::from_str(&json).expect("deserialize")
  }

  #[test]
  fn session_started_envelope_shape() {
    let event = HarnessEvent {
      seq: 0,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::SessionStarted(SessionStartedPayload {
        workspace_root: "/tmp/ws".into(),
        runtime: HarnessRuntimeKind::React,
        profile: HarnessProfile::Local,
        model: "step-1".into(),
        skills: vec!["code-review".into()],
        context_item_count: 3,
        context_token_estimate: 1024,
      }),
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["seq"], 0);
    assert_eq!(json["kind"], "session_started");
    assert_eq!(json["payload"]["workspace_root"], "/tmp/ws");
    assert_eq!(json["payload"]["runtime"], "react");
    let parsed: HarnessEvent = serde_json::from_value(json).unwrap();
    assert_eq!(parsed, event);
  }

  #[test]
  fn approval_requested_round_trips() {
    let request = ApprovalRequest {
      id: "req-1".into(),
      session_id: "sess-1".into(),
      step_index: 5,
      tool: "shell".into(),
      source: Some(ToolSource::Builtin),
      permissions: vec![ToolPermission::ProcessExec],
      idempotency: ToolIdempotency::NonIdempotent,
      effect: Default::default(),
      params_summary: serde_json::json!({"cmd": "ls"}),
      risk: ApprovalRisk::High,
      reason: "shell tool".into(),
      requested_at: ts(),
      expires_at: None,
    };
    let event = HarnessEvent {
      seq: 7,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::ApprovalRequested(ApprovalRequestedPayload { request }),
    };
    let parsed = roundtrip(&event);
    assert_eq!(parsed, event);
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["kind"], "approval_requested");
    assert_eq!(json["payload"]["request"]["risk"], "high");
  }

  #[test]
  fn approval_decided_round_trips() {
    let decision = ApprovalDecision {
      request_id: "req-1".into(),
      decision: ApprovalOutcome::Deny,
      scope: ApprovalScope::Once,
      decided_by: "user".into(),
      decided_at: ts(),
      reason: Some("destructive".into()),
    };
    let event = HarnessEvent {
      seq: 8,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::ApprovalDecided(ApprovalDecidedPayload { decision }),
    };
    let parsed = roundtrip(&event);
    assert_eq!(parsed, event);
  }

  #[test]
  fn interrupt_requested_round_trips() {
    let event = HarnessEvent {
      seq: 9,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::InterruptRequested(InterruptRequestedPayload {
        step_index: 3,
        question: "which deploy target?".into(),
      }),
    };
    let parsed = roundtrip(&event);
    assert_eq!(parsed, event);
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["kind"], "interrupt_requested");
    assert_eq!(json["payload"]["question"], "which deploy target?");
  }

  #[test]
  fn interrupt_answered_round_trips() {
    let event = HarnessEvent {
      seq: 10,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::InterruptAnswered(InterruptAnsweredPayload {
        step_index: 3,
        answer: "staging".into(),
      }),
    };
    let parsed = roundtrip(&event);
    assert_eq!(parsed, event);
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["kind"], "interrupt_answered");
  }

  #[test]
  fn stopped_payload_awaiting_input_round_trips() {
    let event = HarnessEvent {
      seq: 11,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::Stopped(StoppedPayload {
        reason: StopReason::AwaitingInput,
        final_answer: None,
        error: None,
      }),
    };
    let parsed = roundtrip(&event);
    assert_eq!(parsed, event);
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["payload"]["reason"], "awaiting_input");
  }

  #[test]
  fn stopped_round_trips_for_all_reasons() {
    for reason in [
      StopReason::Completed,
      StopReason::Cancelled,
      StopReason::LimitReached,
      StopReason::ApprovalDenied,
      StopReason::Failed,
    ] {
      let event = HarnessEvent {
        seq: 99,
        session_id: "sess-1".into(),
        ts: ts(),
        body: HarnessEventBody::Stopped(StoppedPayload {
          reason,
          final_answer: matches!(reason, StopReason::Completed).then(|| "done".into()),
          error: matches!(reason, StopReason::Failed).then(|| "boom".into()),
        }),
      };
      let parsed = roundtrip(&event);
      assert_eq!(parsed, event);
    }
  }

  #[test]
  fn background_task_payload_round_trips() {
    let event = HarnessEvent {
      seq: 21,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::BackgroundTaskUpdated(BackgroundTaskUpdatedPayload {
        task_id: "task-1".into(),
        status: BackgroundTaskStatus::Running,
        summary: Some("scanning repo".into()),
        error: None,
      }),
    };
    let parsed = roundtrip(&event);
    assert_eq!(parsed, event);
  }

  #[test]
  fn memory_summary_payload_round_trips() {
    let event = HarnessEvent {
      seq: 22,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::MemorySummaryAdded(MemorySummaryAddedPayload {
        layer: "session".into(),
        summary: "user asked about TODOs".into(),
        token_estimate: 64,
      }),
    };
    let parsed = roundtrip(&event);
    assert_eq!(parsed, event);
  }

  #[test]
  fn tool_call_payloads_round_trip() {
    let requested = HarnessEvent {
      seq: 4,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::ToolCallRequested(ToolCallRequestedPayload {
        step_index: 1,
        tool: "http".into(),
        source: Some("builtin".into()),
        permissions: vec!["network".into()],
        idempotency: Some("idempotent".into()),
        params_summary: serde_json::json!({"url": "https://example.test"}),
      }),
    };
    let parsed = roundtrip(&requested);
    assert_eq!(parsed, requested);

    let completed = HarnessEvent {
      seq: 5,
      session_id: "sess-1".into(),
      ts: ts(),
      body: HarnessEventBody::ToolCallCompleted(ToolCallCompletedPayload {
        step_index: 1,
        tool: "http".into(),
        is_error: false,
        duration_ms: 33,
        source: Some("builtin".into()),
        output_summary: Some(serde_json::json!({"status": 200})),
      }),
    };
    let parsed = roundtrip(&completed);
    assert_eq!(parsed, completed);
  }

  #[test]
  fn step_started_round_trips() {
    let event = HarnessEvent::new(
      1,
      "sess-1",
      HarnessEventBody::StepStarted(StepStartedPayload {
        step_index: 0,
        step_type: "plan".into(),
      }),
    );
    let parsed = roundtrip(&event);
    assert_eq!(parsed.body, event.body);
  }
}
