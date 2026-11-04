use std::time::Instant;

use async_trait::async_trait;
use yanshi_agent_spi::{LoopSession, TurnDrivenRuntime, TurnProgress};
use yanshi_llm::LLMResponse;
use yanshi_memory::MemoryStore;

use crate::runtime::{
  AgentCancellationToken, AgentContext, AgentEvent, AgentRunResult, AgentRuntime,
  AgentRuntimeError, AgentStep,
};

use super::config::{LoopDetectionConfig, ReActConfig, ReActError};
use super::core::ReActAgent;

/// Outcome of one turn's LLM call (RFC_HARNESS_LOOP_OWNERSHIP §6, series
/// step 2). `Proceed` carries the response for the parse + dispatch
/// phase; `Stop` carries a terminal result (cancel / timeout).
pub(crate) enum LlmTurnOutcome {
  Proceed {
    llm_response: LLMResponse,
    raw_response: String,
  },
  Stop(AgentRunResult),
}

/// Outcome of a single-tool execution under timeout/cancellation limits
/// (RFC_HARNESS_LOOP_OWNERSHIP §6, series step 3b). `Output` carries the
/// tool result for the rest of the `Action` arm; `Stop` carries a
/// terminal result (timeout / cancellation).
pub(crate) enum ToolExecOutcome {
  Output(yanshi_tool::ToolOutput),
  Stop(AgentRunResult),
}

/// Outcome of processing one turn (RFC_HARNESS_LOOP_OWNERSHIP §6).
/// `Continue` means advance to the next turn; `Stop` carries the terminal
/// result. This is the shape [`ReActAgent::run_one_turn`] returns.
pub(crate) enum TurnStep {
  Continue,
  Stop(AgentRunResult),
}

pub(crate) struct LoopState {
  pub(crate) steps: Vec<AgentStep>,
  pub(crate) events: Vec<AgentEvent>,
  pub(crate) step_index: usize,
  pub(crate) iteration: usize,
  pub(crate) tool_calls: usize,
  pub(crate) verification_attempts: usize,
  /// V2.1: independent budget from `verification_attempts` — see
  /// `ReActConfig::max_schema_correction_attempts`.
  pub(crate) schema_correction_attempts: usize,
  pub(crate) last_tool_call: Option<(String, serde_json::Value)>,
  /// L1.2: the last `loop_detection.window` (tool, params) signatures
  /// dispatched, oldest first — fed from both the single-call and batch
  /// dispatch paths, checked at the top of the next turn.
  pub(crate) recent_tool_calls: std::collections::VecDeque<(String, serde_json::Value)>,
  pub(crate) loop_detection: Option<LoopDetectionConfig>,
  pub(crate) max_iterations: usize,
  pub(crate) max_tool_calls: Option<usize>,
  pub(crate) timeout_ms: Option<u64>,
  pub(crate) budget_tokens: Option<u32>,
  /// T1.1: USD spend cap for this run. `None` disables the guard.
  pub(crate) cost_limit_usd: Option<f64>,
  /// T1.1: running total of `pricing_table`-estimated cost across every
  /// LLM call made so far this run.
  pub(crate) cumulative_cost_usd: f64,
  /// Y2.2: running total of provider-reported tokens across every LLM
  /// call this run — what `budget_tokens` is checked against.
  pub(crate) cumulative_tokens: u64,
  pub(crate) cancellation_token: Option<AgentCancellationToken>,
  pub(crate) run_started_at: Instant,
  pub(crate) system_prompt: String,
  pub(crate) trace_context: Option<yanshi_llm::LlmTraceContext>,
  pub(crate) between_turn_hook: Option<crate::runtime::BetweenTurnHookHandle>,
  /// The original user input that started this run, kept around so
  /// verification strategies can judge a candidate answer against the
  /// request it's meant to satisfy.
  pub(crate) user_input: String,
}

impl LoopState {
  /// V2.4: snapshot this loop state's plain-data fields into a durable
  /// [`AgentLoopCheckpoint`]. Excludes `cancellation_token`/
  /// `run_started_at`/`between_turn_hook` (process-local handles,
  /// reconstructed fresh on resume from the resuming call's
  /// `AgentContext`) and the run-configuration fields (`max_iterations`/
  /// `max_tool_calls`/`timeout_ms`/`budget_tokens`/`cost_limit_usd`/
  /// `loop_detection` — a resume re-derives these from the fresh context
  /// rather than replaying stale limits).
  pub(crate) fn to_checkpoint(
    &self,
    session_id: &str,
    pending_question: Option<String>,
  ) -> yanshi_agent_spi::checkpoint::AgentLoopCheckpoint {
    yanshi_agent_spi::checkpoint::AgentLoopCheckpoint {
      schema_version: yanshi_agent_spi::checkpoint::AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION,
      session_id: session_id.to_string(),
      runtime_kind: yanshi_agent_spi::checkpoint::LoopRuntimeKind::React,
      created_at: chrono::Utc::now(),
      steps: self.steps.clone(),
      events: self.events.clone(),
      step_index: self.step_index,
      iteration: self.iteration,
      tool_calls: self.tool_calls,
      verification_attempts: self.verification_attempts,
      schema_correction_attempts: self.schema_correction_attempts,
      last_tool_call: self.last_tool_call.clone(),
      recent_tool_calls: self.recent_tool_calls.clone(),
      cumulative_cost_usd: self.cumulative_cost_usd,
      cumulative_tokens: self.cumulative_tokens,
      system_prompt: self.system_prompt.clone(),
      user_input: self.user_input.clone(),
      trace_context: self.trace_context.clone(),
      plan_steps: serde_json::Value::Null,
      plan_position: 0,
      observations: Vec::new(),
      pending_question,
    }
  }

  /// V2.4: reconstruct a `LoopState` from a checkpoint, restoring loop
  /// progress while sourcing every process-local/config field fresh from
  /// `context` — the inverse of [`Self::to_checkpoint`]'s exclusions.
  /// Caller (`resume_from_loop_checkpoint`) has already validated
  /// `checkpoint.runtime_kind == LoopRuntimeKind::React`.
  pub(crate) fn from_checkpoint(
    context: &AgentContext,
    config: &ReActConfig,
    checkpoint: &yanshi_agent_spi::checkpoint::AgentLoopCheckpoint,
  ) -> Self {
    Self {
      steps: checkpoint.steps.clone(),
      events: checkpoint.events.clone(),
      step_index: checkpoint.step_index,
      iteration: checkpoint.iteration,
      tool_calls: checkpoint.tool_calls,
      verification_attempts: checkpoint.verification_attempts,
      schema_correction_attempts: checkpoint.schema_correction_attempts,
      last_tool_call: checkpoint.last_tool_call.clone(),
      recent_tool_calls: checkpoint.recent_tool_calls.clone(),
      loop_detection: config.loop_detection,
      max_iterations: context.limits.max_steps.unwrap_or(config.max_iterations),
      max_tool_calls: context.limits.max_tool_calls,
      timeout_ms: context.limits.timeout_ms,
      budget_tokens: context.limits.token_budget.or(config.budget_tokens),
      cost_limit_usd: context.limits.cost_limit_usd.or(config.cost_limit_usd),
      cumulative_cost_usd: checkpoint.cumulative_cost_usd,
      cumulative_tokens: checkpoint.cumulative_tokens,
      cancellation_token: context.cancellation_token.clone(),
      run_started_at: Instant::now(),
      system_prompt: checkpoint.system_prompt.clone(),
      trace_context: checkpoint.trace_context.clone(),
      between_turn_hook: context.between_turn_hook.clone(),
      user_input: checkpoint.user_input.clone(),
    }
  }
}

/// A **live** turn-driven ReAct session (RFC_HARNESS_LOOP_OWNERSHIP §6 step 6).
///
/// Obtain one from [`ReActAgent::begin_turn_driven`], then drive it a turn at a
/// time with [`Self::next_turn`] until it returns [`ReActTurn::Finished`].
/// Between turns the caller (typically the Harness) owns the loop: it can
/// inspect or rewrite the conversation through [`Self::memory`] to compact /
/// refresh context under its own policy. [`ReActAgent::run_with_context`] is the
/// equivalent batteries-included driver that pumps every turn itself.
///
/// ## Compile-time "no use after finish" (P-A3.3)
///
/// This is a **typestate**: the value only ever represents a *live* session.
/// [`next_turn`](Self::next_turn) consumes `self`, returning either a fresh live
/// session ([`ReActTurn::Continued`]) or the run result with no session
/// ([`ReActTurn::Finished`]). There is therefore no way to call `next_turn` on a
/// finished session — that is a compile error (the value was moved), not a
/// runtime `SessionFinished` error. The object-safe [`LoopSession`] trait (which
/// the Harness drives through `Box<dyn LoopSession>`) cannot express this — a
/// `&mut self` method cannot consume the typestate — so that path keeps an
/// internal runtime guard in `ReActTurnDriver`.
pub struct ReActLoopSession<'a> {
  pub(crate) agent: &'a mut ReActAgent,
  pub(crate) state: LoopState,
}

/// Outcome of advancing a [`ReActLoopSession`] by one turn.
///
/// Consuming `next_turn` makes "no use after finish" a compile-time guarantee
/// (P-A3.3): a `Continued` turn hands back a live session to drive again, while
/// `Finished` hands back the result (and the agent borrow, so the caller can
/// still inspect memory) and **no session** — nothing is left to drive.
pub enum ReActTurn<'a> {
  /// The agent advanced; drive `session` again. Boxed because `LoopState`
  /// (embedded in `ReActLoopSession`) is much larger than the `Finished`
  /// variant, and clippy's `large_enum_variant` flags the unboxed gap.
  Continued(Box<ReActLoopSession<'a>>),
  /// The agent reached a terminal state.
  Finished {
    /// The completed run.
    result: AgentRunResult,
    /// The agent borrow the live session held, returned so the caller can keep
    /// reading conversation memory after the run.
    agent: &'a mut ReActAgent,
  },
}

impl<'a> ReActLoopSession<'a> {
  /// Advance exactly one turn (one observe → plan → act cycle), consuming the
  /// session. Returns [`ReActTurn::Continued`] with a fresh live session to
  /// drive, or [`ReActTurn::Finished`] once the agent reaches a terminal state.
  pub async fn next_turn(mut self) -> Result<ReActTurn<'a>, ReActError> {
    match self.agent.run_one_turn(&mut self.state).await {
      Ok(TurnStep::Continue) => {
        // V2.4: same per-turn checkpoint hook as `run_with_context` — a
        // caller driving the loop under `--context-refresh` must not get
        // silently worse checkpoint coverage than the batteries-included
        // path.
        self.agent.save_loop_checkpoint(&self.state).await;
        Ok(ReActTurn::Continued(Box::new(self)))
      }
      Ok(TurnStep::Stop(result)) => {
        self
          .agent
          .clear_loop_checkpoint_if_terminal(&result.stop_reason)
          .await;
        Ok(ReActTurn::Finished {
          result,
          agent: self.agent,
        })
      }
      Err(e) => Err(e),
    }
  }

  /// The run's conversation memory — read or rewrite it between turns to
  /// perform caller-owned context engineering.
  pub fn memory(&self) -> &dyn MemoryStore {
    self.agent.memory_ref()
  }

  /// 0-based index of the turn `next_turn` will run next.
  pub fn turn_index(&self) -> usize {
    self.state.iteration
  }
}

#[async_trait]
impl TurnDrivenRuntime for ReActAgent {
  async fn begin(
    &mut self,
    context: AgentContext,
  ) -> Result<Box<dyn LoopSession + Send + '_>, AgentRuntimeError> {
    let session =
      self
        .begin_turn_driven(context)
        .await
        .map_err(|e| AgentRuntimeError::ExecutionFailed {
          message: e.to_string(),
        })?;
    Ok(Box::new(ReActTurnDriver::new(session)))
  }

  fn runtime_name(&self) -> &'static str {
    "react"
  }
}

/// Object-safe adapter that drives a [`ReActLoopSession`] through the `&mut self`
/// [`LoopSession`] trait the Harness governs via `Box<dyn LoopSession>`.
///
/// The consuming typestate cannot cross the `dyn` boundary (a `&mut self` method
/// cannot move the session out), so this adapter keeps the runtime "already
/// finished" guard the concrete [`ReActLoopSession`] no longer needs: it holds
/// the live session in an `Option` that becomes `None` once a turn finishes.
/// `memory()` / `turn_index()` keep answering after the run finishes by
/// retaining the agent borrow the finished turn handed back.
struct ReActTurnDriver<'a> {
  /// `Some` while the session is live; `None` once a turn returned `Finished`
  /// — or after a turn errored (the consuming turn took the session with it).
  session: Option<ReActLoopSession<'a>>,
  /// Retained on `Finished` so `memory()` / `turn_index()` still answer once the
  /// live session that owned the agent borrow is gone. `(agent, next_turn_index)`.
  finished: Option<(&'a mut ReActAgent, usize)>,
}

impl<'a> ReActTurnDriver<'a> {
  fn new(session: ReActLoopSession<'a>) -> Self {
    Self {
      session: Some(session),
      finished: None,
    }
  }
}

#[async_trait]
impl LoopSession for ReActTurnDriver<'_> {
  async fn next_turn(&mut self) -> Result<TurnProgress, AgentRuntimeError> {
    // `None` means a prior turn already finished (or errored). On the object-safe
    // path this is the runtime guard the concrete typestate makes a compile error.
    let Some(session) = self.session.take() else {
      return Err(AgentRuntimeError::ExecutionFailed {
        message: "turn-driven session already finished".to_string(),
      });
    };
    let next_index = session.turn_index();
    match session
      .next_turn()
      .await
      .map_err(|e| AgentRuntimeError::ExecutionFailed {
        message: e.to_string(),
      })? {
      ReActTurn::Continued(active) => {
        self.session = Some(*active);
        Ok(TurnProgress::Continued)
      }
      ReActTurn::Finished { result, agent } => {
        self.finished = Some((agent, next_index));
        Ok(TurnProgress::Finished(result))
      }
    }
  }

  fn memory(&self) -> &dyn MemoryStore {
    match (&self.session, &self.finished) {
      (Some(session), _) => session.memory(),
      (None, Some((agent, _))) => agent.memory_ref(),
      // Both `None` only after a turn *errored* (the session was consumed with
      // no agent to hand back). The Harness propagates that error and never
      // calls `memory()` afterwards, so this is unreachable in practice.
      (None, None) => unreachable!("turn-driven session errored; memory() not callable after"),
    }
  }

  fn turn_index(&self) -> usize {
    match (&self.session, &self.finished) {
      (Some(session), _) => session.turn_index(),
      (None, Some((_, index))) => *index,
      (None, None) => 0,
    }
  }
}

#[async_trait]
impl AgentRuntime for ReActAgent {
  async fn run(&mut self, context: AgentContext) -> Result<AgentRunResult, AgentRuntimeError> {
    self
      .run_with_context(context)
      .await
      .map_err(|err| AgentRuntimeError::ExecutionFailed {
        message: err.to_string(),
      })
  }

  fn runtime_name(&self) -> &'static str {
    "react"
  }

  async fn resume_from_loop_checkpoint(
    &mut self,
    context: AgentContext,
    checkpoint: yanshi_agent_spi::checkpoint::AgentLoopCheckpoint,
    answer: Option<String>,
  ) -> Result<AgentRunResult, AgentRuntimeError> {
    self
      .resume_from_loop_checkpoint(context, checkpoint, answer)
      .await
      .map_err(|err| AgentRuntimeError::ExecutionFailed {
        message: err.to_string(),
      })
  }
}
