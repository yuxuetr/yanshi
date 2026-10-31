//! Shared helpers for the multi-agent supervisors (`handoff` / `blackboard`
//! / `debate`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;

use crate::delegation::{DelegationSpec, SchemaValidation, validate_output};
use crate::runtime::{
  AgentContext, AgentRunResult, AgentRuntime, AgentStepKind, AgentStopReason, RuntimeLimits,
};

/// A supervisor participant, shared across concurrently-dispatched tasks
/// (e.g. `BlackboardSchedule::Parallel`, debate rounds) and type-erased
/// (W3.2) so any [`AgentRuntime`] — not just `ReActAgent` — can be a
/// `HandoffSupervisor`/`BlackboardSupervisor`/`DebateSupervisor`
/// participant or judge.
pub(crate) type SharedAgentRuntime = Arc<AsyncMutex<Box<dyn AgentRuntime>>>;

/// Child stop reasons that end the whole supervisor run whatever role the
/// child played: the operator chose deny-and-stop, the caller cancelled,
/// or the child is waiting on a human. Supervisors used to report all of
/// these as `FinalAnswer` (eval 2026-10-07 A1). Other failures (MaxSteps,
/// Timeout, Error, ...) stay the supervisor's to weigh — a debate, for
/// one, carries on without a failed participant.
pub(crate) fn halts_supervisor(reason: &AgentStopReason) -> bool {
  matches!(
    reason,
    AgentStopReason::ApprovalDenied { .. }
      | AgentStopReason::Cancelled { .. }
      | AgentStopReason::AwaitingInput { .. }
  )
}

/// Build a child agent's `AgentContext` for one delegation step, forwarding
/// everything from `parent` a sub-agent needs to stay governed and
/// observable the same way the top-level run is (W3.1):
///
/// - `limits` / `cancellation_token` were already forwarded pre-W3.1.
/// - `event_sink` / `between_turn_hook` / `loop_checkpointer` /
///   `trace_context` were dropped — a supervisor delegating to a child
///   silently orphaned the parent's live event stream (Harness couldn't see
///   sub-agent steps), loop checkpointing (no multi-agent checkpoint
///   coverage), and OTel trace propagation (the trace broke at the
///   delegation boundary) even though every one of these fields is a cheap
///   `Option<Arc<dyn Trait>>`-shaped handle to clone.
///
/// All three supervisors (`HandoffSupervisor`, `BlackboardSupervisor`,
/// `DebateSupervisor`) call this instead of duplicating the same
/// hand-rolled construction, so a future new field on `AgentContext` only
/// needs to be forwarded here once.
///
/// `limits` come from [`ChildBudget::limits_for`], not `parent.limits`:
/// the parent's run-wide budgets are shared by its children.
pub(crate) fn build_child_context(
  parent: &AgentContext,
  agent_name: &str,
  input: &str,
  limits: RuntimeLimits,
) -> AgentContext {
  let session = format!("{}::{}", parent.session_id, agent_name);
  let mut ctx = AgentContext::new(session, input, parent.model.clone()).with_limits(limits);
  if let Some(token) = parent.cancellation_token.clone() {
    ctx = ctx.with_cancellation_token(token);
  }
  ctx.metadata = parent.metadata.clone();
  ctx.event_sink = parent.event_sink.clone();
  ctx.between_turn_hook = parent.between_turn_hook.clone();
  ctx.loop_checkpointer = parent.loop_checkpointer.clone();
  ctx.trace_context = parent.trace_context.clone();
  ctx
}

/// The parent's run-wide limits, drawn down by its children together
/// (eval 2026-10-07 A2). Each child used to get a full copy, so a
/// supervisor of N children could spend N times the wall clock and tool
/// calls it was given. `timeout_ms` becomes one deadline for the whole
/// supervisor run and `max_tool_calls` a shared pool charged with each
/// child's `ToolCall` steps. `max_steps` and `token_budget` bound one
/// agent loop and one conversation, so every child keeps its own.
/// `cost_limit_usd` stays per child: results carry no cost to charge.
pub(crate) struct ChildBudget {
  deadline: Option<Instant>,
  tool_calls_left: Option<usize>,
}

impl ChildBudget {
  pub(crate) fn start(limits: &RuntimeLimits) -> Self {
    Self {
      deadline: limits
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms)),
      tool_calls_left: limits.max_tool_calls,
    }
  }

  /// Limits for each of `concurrent` children about to run side by side
  /// (1 for a sequential dispatch), or why the supervisor must stop
  /// because the budget is spent. Concurrent children share the deadline
  /// and split the remaining tool calls, so together they stay within it.
  pub(crate) fn limits_for(
    &self,
    parent: &RuntimeLimits,
    concurrent: usize,
  ) -> Result<RuntimeLimits, AgentStopReason> {
    let mut limits = parent.clone();
    if let (Some(deadline), Some(timeout_ms)) = (self.deadline, parent.timeout_ms) {
      let left = deadline.saturating_duration_since(Instant::now());
      if left.is_zero() {
        return Err(AgentStopReason::Timeout { timeout_ms });
      }
      limits.timeout_ms = Some(u64::try_from(left.as_millis()).unwrap_or(u64::MAX).max(1));
    }
    if let (Some(left), Some(max_tool_calls)) = (self.tool_calls_left, parent.max_tool_calls) {
      if left == 0 {
        return Err(AgentStopReason::MaxToolCalls { max_tool_calls });
      }
      limits.max_tool_calls = Some(left / concurrent.max(1));
    }
    Ok(limits)
  }

  /// Charge a finished child's tool calls against the shared pool.
  pub(crate) fn charge(&mut self, result: &AgentRunResult) {
    if let Some(left) = self.tool_calls_left.as_mut() {
      let used = result
        .steps
        .iter()
        .filter(|step| matches!(step.kind, AgentStepKind::ToolCall { .. }))
        .count();
      *left = left.saturating_sub(used);
    }
  }
}

/// If `agent_name` has a registered [`DelegationSpec`] (W3.3), validate
/// `answer` against its `expected_output_schema` and record the outcome in
/// `validations`. A no-op — nothing recorded — when no spec is registered
/// for `agent_name`, so a supervisor with no `DelegationSpec`s wired in
/// behaves exactly as it did pre-W3.3.
///
/// This wires `yanshi_agent_spi::delegation`'s L5.1 primitive (up to
/// now a standalone helper callers had to invoke by hand around
/// `run_delegated`, per `crate::delegation`'s own module doc) directly
/// into each supervisor's dispatch loop: register a spec once via
/// `with_delegation_spec`/`with_judge_delegation_spec`, and every future
/// run validates that participant's answer automatically.
pub(crate) fn record_delegation_validation(
  specs: &HashMap<String, DelegationSpec>,
  validations: &mut HashMap<String, SchemaValidation>,
  agent_name: &str,
  answer: Option<&str>,
) {
  let Some(spec) = specs.get(agent_name) else {
    return;
  };
  let validation = answer
    .map(|answer| validate_output(spec, answer))
    .unwrap_or(SchemaValidation::NotRequired);
  validations.insert(agent_name.to_string(), validation);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::runtime::{AgentEvent, AgentEventSink, BetweenTurnHook};
  use async_trait::async_trait;
  use std::sync::Arc;
  use yanshi_agent_spi::checkpoint::{
    AgentLoopCheckpoint, AgentLoopCheckpointError, AgentLoopCheckpointer,
  };
  use yanshi_memory::MemoryStore;

  struct NoopSink;
  #[async_trait]
  impl AgentEventSink for NoopSink {
    async fn emit(&self, _event: &AgentEvent) {}
  }

  struct NoopHook;
  #[async_trait]
  impl BetweenTurnHook for NoopHook {
    async fn before_turn(&self, _turn_index: usize, _session_id: &str, _memory: &dyn MemoryStore) {}
  }

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

  /// W3.1 regression: pre-fix, `build_child_context` only forwarded
  /// `limits`/`cancellation_token`/`metadata` — `event_sink`,
  /// `between_turn_hook`, `loop_checkpointer`, and `trace_context` were
  /// silently dropped, so a supervisor's sub-agent ran with none of the
  /// parent's observability/checkpointing wiring.
  #[test]
  fn forwards_event_sink_hook_checkpointer_and_trace_context() {
    let trace = yanshi_value::LlmTraceContext::new("1".repeat(32), "1".repeat(16))
      .expect("valid trace context");
    let parent = AgentContext::new("sess", "goal", "gpt-4o")
      .with_event_sink(Arc::new(NoopSink))
      .with_between_turn_hook(Arc::new(NoopHook))
      .with_loop_checkpointer(Arc::new(NoopCheckpointer))
      .with_trace_context(trace);

    let child = build_child_context(&parent, "sub_agent", "sub-input", parent.limits.clone());

    assert_eq!(
      child.event_sink, parent.event_sink,
      "event_sink must be the same shared handle as the parent's"
    );
    assert_eq!(
      child.between_turn_hook, parent.between_turn_hook,
      "between_turn_hook must be the same shared handle as the parent's"
    );
    assert_eq!(
      child.loop_checkpointer, parent.loop_checkpointer,
      "loop_checkpointer must be the same shared handle as the parent's"
    );
    assert_eq!(
      child.trace_context, parent.trace_context,
      "trace_context must propagate to the child"
    );
  }

  #[test]
  fn leaves_governance_fields_none_when_parent_has_none() {
    let parent = AgentContext::new("sess", "goal", "gpt-4o");
    let child = build_child_context(&parent, "sub_agent", "sub-input", parent.limits.clone());
    assert!(child.event_sink.is_none());
    assert!(child.between_turn_hook.is_none());
    assert!(child.loop_checkpointer.is_none());
    assert!(child.trace_context.is_none());
  }

  /// Child that stops with a fixed reason, so supervisor propagation can be
  /// tested without a mock LLM.
  struct ScriptedChild(AgentStopReason);

  #[async_trait]
  impl AgentRuntime for ScriptedChild {
    async fn run(
      &mut self,
      context: AgentContext,
    ) -> Result<crate::runtime::AgentRunResult, crate::runtime::AgentRuntimeError> {
      let mut result = crate::runtime::AgentRunResult::final_answer(context.session_id, "partial");
      result.stop_reason = self.0.clone();
      Ok(result)
    }

    fn runtime_name(&self) -> &'static str {
      "scripted"
    }
  }

  fn denied() -> AgentStopReason {
    AgentStopReason::ApprovalDenied {
      message: "deny_and_stop".into(),
    }
  }

  /// Eval 2026-10-07 A1: every supervisor used to report a child's
  /// DenyAndStop / MaxSteps as `FinalAnswer`.
  #[tokio::test]
  async fn supervisors_propagate_child_stop_reasons() {
    use crate::supervisor::{
      BlackboardSchedule, BlackboardSupervisorBuilder, DebateSupervisorBuilder,
      HandoffSupervisorBuilder,
    };
    let ctx = || AgentContext::new("s", "go", "mock");

    for reason in [denied(), AgentStopReason::MaxSteps { max_steps: 3 }] {
      let expected = reason.clone();
      let mut handoff = HandoffSupervisorBuilder::new()
        .add_agent("only", "only agent", move |_tool| ScriptedChild(reason))
        .initial_agent("only")
        .build()
        .unwrap();
      assert_eq!(
        AgentRuntime::run(&mut handoff, ctx())
          .await
          .unwrap()
          .stop_reason,
        expected
      );
    }

    let mut debate = DebateSupervisorBuilder::new()
      .add_participant("a", ScriptedChild(AgentStopReason::FinalAnswer))
      .judge(ScriptedChild(AgentStopReason::MaxSteps { max_steps: 2 }))
      .build()
      .unwrap();
    assert_eq!(
      AgentRuntime::run(&mut debate, ctx())
        .await
        .unwrap()
        .stop_reason,
      AgentStopReason::MaxSteps { max_steps: 2 }
    );

    let mut debate = DebateSupervisorBuilder::new()
      .add_participant("a", ScriptedChild(denied()))
      .judge(ScriptedChild(AgentStopReason::FinalAnswer))
      .build()
      .unwrap();
    assert_eq!(
      AgentRuntime::run(&mut debate, ctx())
        .await
        .unwrap()
        .stop_reason,
      denied()
    );

    for schedule in [
      BlackboardSchedule::Sequential(vec!["a".into(), "b".into()]),
      BlackboardSchedule::Parallel(vec!["a".into(), "b".into()]),
    ] {
      let mut blackboard = BlackboardSupervisorBuilder::new()
        .add_agent("a", "first", |_bb| ScriptedChild(denied()))
        .add_agent("b", "second", |_bb| {
          ScriptedChild(AgentStopReason::FinalAnswer)
        })
        .schedule(schedule)
        .build()
        .unwrap();
      assert_eq!(
        AgentRuntime::run(&mut blackboard, ctx())
          .await
          .unwrap()
          .stop_reason,
        denied()
      );
    }
  }

  /// Child that records the tool-call limit it was handed, sleeps, and reports
  /// `tool_calls` ToolCall steps.
  struct Spender {
    tool_calls: usize,
    sleep_ms: u64,
    seen: Arc<std::sync::Mutex<Vec<Option<usize>>>>,
  }

  #[async_trait]
  impl AgentRuntime for Spender {
    async fn run(
      &mut self,
      context: AgentContext,
    ) -> Result<crate::runtime::AgentRunResult, crate::runtime::AgentRuntimeError> {
      if let Ok(mut seen) = self.seen.lock() {
        seen.push(context.limits.max_tool_calls);
      }
      tokio::time::sleep(Duration::from_millis(self.sleep_ms)).await;
      let mut result = crate::runtime::AgentRunResult::final_answer(context.session_id, "done");
      for index in 0..self.tool_calls {
        result.steps.push(crate::runtime::AgentStep::new(
          index + 1,
          AgentStepKind::ToolCall {
            tool: "t".into(),
            params: serde_json::Value::Null,
          },
        ));
      }
      Ok(result)
    }

    fn runtime_name(&self) -> &'static str {
      "spender"
    }
  }

  fn limited(max_tool_calls: Option<usize>, timeout_ms: Option<u64>) -> AgentContext {
    AgentContext::new("s", "go", "mock").with_limits(RuntimeLimits {
      max_tool_calls,
      timeout_ms,
      ..RuntimeLimits::default()
    })
  }

  /// Eval 2026-10-07 A2: children used to get a full copy of the parent's
  /// limits each, so N children could spend N times the budget.
  #[tokio::test]
  async fn sequential_children_share_the_tool_call_pool() {
    use crate::supervisor::{BlackboardSchedule, BlackboardSupervisorBuilder};
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (a, b, c) = (seen.clone(), seen.clone(), seen.clone());
    let mut blackboard = BlackboardSupervisorBuilder::new()
      .add_agent("a", "first", move |_bb| Spender {
        tool_calls: 2,
        sleep_ms: 0,
        seen: a,
      })
      .add_agent("b", "second", move |_bb| Spender {
        tool_calls: 1,
        sleep_ms: 0,
        seen: b,
      })
      .add_agent("c", "third", move |_bb| Spender {
        tool_calls: 0,
        sleep_ms: 0,
        seen: c,
      })
      .schedule(BlackboardSchedule::Sequential(vec![
        "a".into(),
        "b".into(),
        "c".into(),
      ]))
      .build()
      .unwrap();
    let result = AgentRuntime::run(&mut blackboard, limited(Some(3), None))
      .await
      .unwrap();
    let seen: Vec<_> = seen.lock().unwrap().clone();
    assert_eq!(
      seen,
      vec![Some(3), Some(1)],
      "c must not start once the pool is empty"
    );
    assert_eq!(
      result.stop_reason,
      AgentStopReason::MaxToolCalls { max_tool_calls: 3 }
    );
  }

  #[tokio::test]
  async fn concurrent_children_split_the_pool_and_the_judge_gets_the_rest() {
    use crate::supervisor::DebateSupervisorBuilder;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let spender = |tool_calls| Spender {
      tool_calls,
      sleep_ms: 0,
      seen: seen.clone(),
    };
    let mut debate = DebateSupervisorBuilder::new()
      .add_participant("a", spender(2))
      .add_participant("b", spender(1))
      .judge(spender(0))
      .build()
      .unwrap();
    AgentRuntime::run(&mut debate, limited(Some(7), None))
      .await
      .unwrap();
    let mut seen: Vec<_> = seen.lock().unwrap().clone();
    let judge = seen.pop();
    seen.sort();
    assert_eq!(
      seen,
      vec![Some(3), Some(3)],
      "7 split across 2 participants"
    );
    assert_eq!(judge, Some(Some(4)), "the judge gets what the round left");
  }

  #[tokio::test]
  async fn children_share_one_deadline() {
    use crate::supervisor::{BlackboardSchedule, BlackboardSupervisorBuilder};
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (a, b) = (seen.clone(), seen.clone());
    let mut blackboard = BlackboardSupervisorBuilder::new()
      .add_agent("a", "slow", move |_bb| Spender {
        tool_calls: 0,
        sleep_ms: 150,
        seen: a,
      })
      .add_agent("b", "next", move |_bb| Spender {
        tool_calls: 0,
        sleep_ms: 0,
        seen: b,
      })
      .schedule(BlackboardSchedule::Sequential(vec!["a".into(), "b".into()]))
      .build()
      .unwrap();
    let result = AgentRuntime::run(&mut blackboard, limited(None, Some(100)))
      .await
      .unwrap();
    assert_eq!(
      seen.lock().unwrap().len(),
      1,
      "b must not start past the deadline"
    );
    assert_eq!(
      result.stop_reason,
      AgentStopReason::Timeout { timeout_ms: 100 }
    );
  }

  #[test]
  fn budget_without_limits_passes_the_parent_limits_through() {
    let limits = RuntimeLimits::react_defaults();
    let mut budget = ChildBudget::start(&limits);
    budget.charge(&crate::runtime::AgentRunResult::final_answer("s", "x"));
    assert_eq!(budget.limits_for(&limits, 3).unwrap(), limits);
  }
}
