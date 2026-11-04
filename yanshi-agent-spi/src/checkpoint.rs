//! Agent-loop-level persistent checkpoint contract (V2.4).
//!
//! Distinct from the DAG-level `Flow` checkpoint in `yanshi-core`, which
//! only ever treats an embedded agent run as one opaque, all-or-nothing
//! unit. This contract captures a *turn-level* (ReAct) / *plan-step-level*
//! (PlanExecute) snapshot of an in-flight loop, so a run interrupted by a
//! process restart can resume from the last completed step instead of
//! restarting from scratch.
//!
//! The contract lives here (the kernel crate) so both `ReActAgent` and
//! `PlanExecuteAgent` in `yanshi-agents` can consume the same
//! abstraction; the concrete file-based implementation lives in
//! `yanshi-agents` (a kernel crate must not own file I/O or a retention
//! policy). This mirrors the existing `EventSinkHandle` /
//! `BetweenTurnHookHandle` split: contract in `agent-spi`, wiring in the
//! runtimes that consume it.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::runtime::{AgentEvent, AgentStep};

/// Current `AgentLoopCheckpoint` schema version. `FileLoopCheckpointer::load`
/// (yanshi-agents) rejects a checkpoint whose `schema_version` is newer
/// than this, rather than risk silently misinterpreting an unknown shape.
pub const AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Which runtime produced an [`AgentLoopCheckpoint`]. Guards
/// `resume_from_loop_checkpoint` against handing a `PlanExecute` checkpoint
/// to `ReActAgent` (or vice versa), where the runtime-specific fields
/// (`iteration`/`schema_correction_attempts` vs `plan_steps`/`plan_position`)
/// would be silently misinterpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopRuntimeKind {
  React,
  PlanExecute,
}

/// A durable snapshot of an in-flight agent loop, taken after a completed
/// turn (ReAct) or plan step (PlanExecute).
///
/// Fields fall into three groups:
///
/// - **Copied verbatim from the runtime's private loop state** (ReAct's
///   `LoopState`, PlanExecute's local accumulators): `steps`, `events`,
///   `step_index`, `iteration`, `tool_calls`, `verification_attempts`,
///   `schema_correction_attempts`, `last_tool_call`, `recent_tool_calls`,
///   `cumulative_cost_usd`, `system_prompt`, `user_input`, `trace_context`,
///   `plan_steps`, `plan_position`, `observations`.
/// - **New bookkeeping** not present on either runtime's loop state today:
///   `schema_version`, `session_id`, `runtime_kind`, `created_at`.
/// - **Deliberately excluded** (reconstructed fresh from the *resuming*
///   call's `AgentContext` instead of being carried in the checkpoint):
///   the cancellation token, the turn-start `Instant` (meaningless across a
///   process boundary), the between-turn hook, and every run-*configuration*
///   field (`max_iterations`/`max_tool_calls`/`timeout_ms`/`budget_tokens`/
///   `cost_limit_usd`/`loop_detection`) — a resume re-derives these from the
///   fresh context/config passed to the resume call, so a resumed run can
///   legitimately be handed different limits than the original (e.g. a
///   higher step budget to let it finish).
///
/// `verification_attempts` and `schema_correction_attempts` are carried as
/// separate typed fields rather than being reconstructed by replaying
/// `steps` — both are recorded via the same `AgentStepKind::Verify`
/// shape with independent counters, disambiguated only by a free-text
/// `feedback` prefix, not a typed discriminant. Direct field serialization
/// sidesteps that fragility entirely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentLoopCheckpoint {
  pub schema_version: u32,
  pub session_id: String,
  pub runtime_kind: LoopRuntimeKind,
  pub created_at: DateTime<Utc>,

  pub steps: Vec<AgentStep>,
  pub events: Vec<AgentEvent>,
  pub step_index: usize,
  /// ReAct only; `0` for PlanExecute checkpoints.
  pub iteration: usize,
  pub tool_calls: usize,
  /// ReAct only; `0` for PlanExecute checkpoints.
  pub verification_attempts: usize,
  /// ReAct only; `0` for PlanExecute checkpoints.
  pub schema_correction_attempts: usize,
  pub last_tool_call: Option<(String, serde_json::Value)>,
  pub recent_tool_calls: VecDeque<(String, serde_json::Value)>,
  pub cumulative_cost_usd: f64,
  /// Y2.2 (COR-2): provider-reported tokens spent by this run so far, the
  /// sum of every LLM call's usage. ReAct checks `token_budget` against
  /// it. Defaulted so checkpoints written before Y2.2 still load.
  #[serde(default)]
  pub cumulative_tokens: u64,
  pub system_prompt: String,
  pub user_input: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub trace_context: Option<yanshi_value::LlmTraceContext>,

  /// PlanExecute only: the original generated plan — both the step list
  /// *and* the planner's `final_answer` (set when the planner answers
  /// directly without needing every step executed) — frozen at planning
  /// time (`serde_json::to_value(&plan)`, the whole `PlanExecutePlan`,
  /// despite the field's name). `agent-spi` cannot import
  /// `yanshi_agents::PlanExecutePlan` (runtimes never depend on each
  /// other), so this stays generic; `PlanExecuteAgent` owns the
  /// serialize/deserialize round-trip at both boundary points. Empty
  /// (`Value::Null`) for ReAct checkpoints.
  #[serde(default)]
  pub plan_steps: serde_json::Value,
  /// PlanExecute only: index of the next not-yet-executed plan step (the
  /// loop-driving cursor — distinct from `step_index`, which numbers
  /// `AgentStep`s and does not advance for pure-reasoning plan steps).
  /// `0` for ReAct checkpoints.
  #[serde(default)]
  pub plan_position: usize,
  /// PlanExecute only: the local `observations` accumulator threaded
  /// through the execute loop. Empty for ReAct checkpoints.
  #[serde(default)]
  pub observations: Vec<String>,

  /// V2.3: set when this checkpoint was saved because the loop is
  /// paused waiting for a user's answer
  /// ([`crate::runtime::AgentStopReason::AwaitingInput`]), so a
  /// checkpoint alone is self-describing without inspecting `steps`
  /// history for the question. `None` for every other checkpoint,
  /// including the ordinary per-turn/per-step saves from V2.4's
  /// crash-resume story.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub pending_question: Option<String>,
}

/// Error surface for [`AgentLoopCheckpointer`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum AgentLoopCheckpointError {
  /// The checkpointer's backing store rejected the read/write (e.g. an I/O
  /// error, a serialization failure).
  #[error("agent loop checkpoint I/O failed: {message}")]
  Io { message: String },
  /// `session_id` failed the checkpointer's path/key-safety validation.
  #[error("invalid session id for checkpoint storage: {session_id}")]
  InvalidSessionId { session_id: String },
  /// A loaded checkpoint's `schema_version` is newer than this build knows
  /// how to interpret.
  #[error("checkpoint schema version {found} is newer than supported ({supported})")]
  UnsupportedSchemaVersion { found: u32, supported: u32 },
}

/// Durable storage for [`AgentLoopCheckpoint`]s, keyed by session id.
///
/// Implementations must be cheap to call after every turn/plan-step — the
/// default posture (see `yanshi-agents::react::agent` /
/// `yanshi-agents::plan_execute`) is an unconditional save after every
/// unit of loop progress, mirroring the DAG-level `Flow` checkpoint's own
/// unconditional-after-every-node precedent. A save failure must not abort
/// the run; callers log and continue.
#[async_trait]
pub trait AgentLoopCheckpointer: Send + Sync {
  /// Persist (overwrite) the checkpoint for `checkpoint.session_id`.
  async fn save(&self, checkpoint: &AgentLoopCheckpoint) -> Result<(), AgentLoopCheckpointError>;

  /// Load the most recently saved checkpoint for `session_id`, or `Ok(None)`
  /// if none exists.
  async fn load(
    &self,
    session_id: &str,
  ) -> Result<Option<AgentLoopCheckpoint>, AgentLoopCheckpointError>;

  /// Remove any checkpoint for `session_id`. Must be idempotent — clearing
  /// an already-clear/never-written session is not an error.
  async fn clear(&self, session_id: &str) -> Result<(), AgentLoopCheckpointError>;
}

/// Cloneable handle wrapping an [`AgentLoopCheckpointer`] so
/// [`crate::runtime::AgentContext`] can keep deriving `Debug` / `PartialEq`
/// (a bare `Arc<dyn AgentLoopCheckpointer>` implements neither). Mirrors
/// `EventSinkHandle` / `BetweenTurnHookHandle`.
#[derive(Clone)]
pub struct LoopCheckpointerHandle(pub Arc<dyn AgentLoopCheckpointer>);

impl std::fmt::Debug for LoopCheckpointerHandle {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("LoopCheckpointerHandle(<dyn AgentLoopCheckpointer>)")
  }
}

impl PartialEq for LoopCheckpointerHandle {
  fn eq(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.0, &other.0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample_checkpoint() -> AgentLoopCheckpoint {
    let mut recent = VecDeque::new();
    recent.push_back(("search".to_string(), serde_json::json!({"q": "rust"})));
    AgentLoopCheckpoint {
      schema_version: AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION,
      session_id: "sess-1".into(),
      runtime_kind: LoopRuntimeKind::React,
      created_at: Utc::now(),
      steps: vec![],
      events: vec![],
      step_index: 3,
      iteration: 2,
      tool_calls: 1,
      verification_attempts: 0,
      schema_correction_attempts: 0,
      last_tool_call: Some(("search".to_string(), serde_json::json!({"q": "rust"}))),
      recent_tool_calls: recent,
      cumulative_cost_usd: 0.0123,
      cumulative_tokens: 21_890,
      system_prompt: "be helpful".into(),
      user_input: "find the bug".into(),
      trace_context: None,
      plan_steps: serde_json::Value::Null,
      plan_position: 0,
      observations: vec![],
      pending_question: None,
    }
  }

  #[test]
  fn checkpoint_round_trips_through_json_with_every_field_intact() {
    let checkpoint = sample_checkpoint();
    let json = serde_json::to_string(&checkpoint).expect("serialize");
    let parsed: AgentLoopCheckpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, checkpoint);
  }

  #[test]
  fn empty_recent_tool_calls_and_none_last_tool_call_round_trip() {
    let mut checkpoint = sample_checkpoint();
    checkpoint.recent_tool_calls = VecDeque::new();
    checkpoint.last_tool_call = None;
    let json = serde_json::to_string(&checkpoint).expect("serialize");
    let parsed: AgentLoopCheckpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, checkpoint);
  }

  #[test]
  fn pending_question_round_trips_when_set() {
    let mut checkpoint = sample_checkpoint();
    checkpoint.pending_question = Some("what's the deploy target?".into());
    let json = serde_json::to_string(&checkpoint).expect("serialize");
    assert!(json.contains("pending_question"));
    let parsed: AgentLoopCheckpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, checkpoint);
  }

  /// Y2.2: a checkpoint written before `cumulative_tokens` existed loads
  /// with the total at zero rather than failing.
  #[test]
  fn checkpoint_without_cumulative_tokens_loads_as_zero() {
    let mut json = serde_json::to_value(sample_checkpoint()).expect("serialize");
    json
      .as_object_mut()
      .expect("object")
      .remove("cumulative_tokens");
    let parsed: AgentLoopCheckpoint = serde_json::from_value(json).expect("deserialize");
    assert_eq!(parsed.cumulative_tokens, 0);
  }

  /// V2.3: `pending_question` is additive (`#[serde(default)]`) — a
  /// checkpoint serialized by a pre-V2.3 build (no `pending_question`
  /// key at all) must still deserialize cleanly as `None`, not fail.
  #[test]
  fn checkpoint_json_without_pending_question_key_deserializes_as_none() {
    let checkpoint = sample_checkpoint();
    let mut json = serde_json::to_value(&checkpoint).expect("serialize");
    json.as_object_mut().unwrap().remove("pending_question");
    let parsed: AgentLoopCheckpoint = serde_json::from_value(json).expect("deserialize");
    assert_eq!(parsed.pending_question, None);
  }

  #[test]
  fn plan_execute_variant_round_trips_plan_steps_and_position() {
    let mut checkpoint = sample_checkpoint();
    checkpoint.runtime_kind = LoopRuntimeKind::PlanExecute;
    checkpoint.iteration = 0;
    checkpoint.plan_steps = serde_json::json!([{"id": "s1", "tool": "search"}]);
    checkpoint.plan_position = 1;
    checkpoint.observations = vec!["found nothing".into()];
    let json = serde_json::to_string(&checkpoint).expect("serialize");
    let parsed: AgentLoopCheckpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, checkpoint);
  }

  #[test]
  fn loop_checkpointer_handle_equality_is_pointer_identity() {
    struct NoopCheckpointer;
    #[async_trait]
    impl AgentLoopCheckpointer for NoopCheckpointer {
      async fn save(
        &self,
        _checkpoint: &AgentLoopCheckpoint,
      ) -> Result<(), AgentLoopCheckpointError> {
        Ok(())
      }
      async fn load(
        &self,
        _session_id: &str,
      ) -> Result<Option<AgentLoopCheckpoint>, AgentLoopCheckpointError> {
        Ok(None)
      }
      async fn clear(&self, _session_id: &str) -> Result<(), AgentLoopCheckpointError> {
        Ok(())
      }
    }

    let shared: Arc<dyn AgentLoopCheckpointer> = Arc::new(NoopCheckpointer);
    let a = LoopCheckpointerHandle(shared.clone());
    let b = LoopCheckpointerHandle(shared);
    let c = LoopCheckpointerHandle(Arc::new(NoopCheckpointer));
    assert_eq!(a, b);
    assert_ne!(a, c);
  }
}
