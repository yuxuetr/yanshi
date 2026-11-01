use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};
use yanshi_graph::flow::Flow;
use yanshi_graph::{FlowRunner, FlowValue, YanshiError};
use yanshi_llm::{
  LLMResponse, MultimodalMessage, ToolCallRequest, ToolSpec, Yanshi, prompt_fingerprint,
};
use yanshi_memory::{MemoryStore, Message};
use yanshi_tool::{ToolMetadata, ToolRegistry};

use crate::runtime::{
  AgentCancellationToken, AgentContext, AgentEvent, AgentMemoryHook, AgentRunResult, AgentRuntime,
  AgentRuntimeError, AgentStep, AgentStepKind, AgentStopReason, MemoryHookContext, MemoryHookKind,
};

/// Error type for Plan-and-Execute agent operations.
#[derive(Debug, thiserror::Error)]
pub enum PlanExecuteError {
  #[error("LLM error: {0}")]
  LlmError(#[from] yanshi_llm::LLMError),

  #[error("Memory error: {0}")]
  MemoryError(#[from] yanshi_memory::MemoryError),

  #[error("Plan parse error: {message}")]
  PlanParse { message: String },

  #[error("Agent run cancelled: {reason}")]
  Cancelled { reason: String },

  #[error("Agent run timed out after {timeout_ms}ms")]
  Timeout { timeout_ms: u64 },

  /// The approval layer returned `ToolError::PolicyDeniedAndStop`: the
  /// run must end, not carry on with the next plan step (W0.5 parity
  /// with ReAct; eval 2026-10-07 P0-5).
  #[error("Agent run stopped by approval denial: {message}")]
  ApprovalDenied { message: String },

  /// Compiling or executing the plan as a `Flow` failed (P-A2.2 `run_as_flow`).
  #[error("Flow execution error: {0}")]
  Flow(#[from] YanshiError),

  /// V2.1: `output_schema` was configured but the final answer still
  /// failed validation after `max_schema_correction_attempts` retries of
  /// the whole plan-and-execute cycle. A schema is a caller-declared hard
  /// contract — hard error rather than silently returning non-conformant
  /// output.
  #[error("Final answer did not match output_schema after {attempts} attempt(s): {errors:?}")]
  SchemaValidationFailed {
    errors: Vec<String>,
    attempts: usize,
  },

  /// V2.3: [`PlanExecuteAgent::resume_from_loop_checkpoint`] was handed
  /// a checkpoint/answer pair that don't agree — e.g. a checkpoint
  /// with no pending question but an answer was supplied anyway, or
  /// the reverse. Mirrors `ReActError::InvalidCheckpoint`.
  #[error("cannot resume from loop checkpoint: {message}")]
  InvalidCheckpoint { message: String },
}

/// Configuration for a [`PlanExecuteAgent`].
#[derive(Debug, Clone)]
pub struct PlanExecuteConfig {
  pub model: String,
  pub persona: Option<String>,
  pub max_steps: usize,
  /// T1.1: USD spend budget for the run's single planner call. `None`
  /// disables the guard. Overridable per-run via
  /// `AgentContext::limits.cost_limit_usd` (the context value wins when
  /// both are set, mirroring `ReActConfig::cost_limit_usd`).
  pub cost_limit_usd: Option<f64>,
  /// T1.1: pricing table used to translate the planner call's token
  /// usage into a USD cost estimate. Defaults to an empty table (every
  /// call costs $0) — reuses `yanshi-agents::eval::pricing` rather
  /// than a second pricing representation.
  pub pricing_table: crate::eval::PricingTable,

  /// V2.1: JSON Schema the final answer must validate against once parsed
  /// as JSON. `None` (the default) disables structured-output enforcement
  /// entirely — byte-identical behaviour to before this existed. See
  /// [`PlanExecuteAgent::run_with_context`]'s doc comment for how a
  /// mismatch is retried.
  pub output_schema: Option<Value>,

  /// Maximum number of times the whole plan-and-execute cycle may be
  /// retried after an `output_schema` mismatch before the run gives up
  /// with [`PlanExecuteError::SchemaValidationFailed`]. Only relevant when
  /// `output_schema` is `Some`.
  pub max_schema_correction_attempts: usize,
}

impl Default for PlanExecuteConfig {
  fn default() -> Self {
    Self {
      model: "gpt-4o".to_string(),
      persona: None,
      max_steps: 8,
      cost_limit_usd: None,
      pricing_table: crate::eval::PricingTable::default(),
      output_schema: None,
      max_schema_correction_attempts: 2,
    }
  }
}

impl PlanExecuteConfig {
  pub fn new(model: impl Into<String>) -> Self {
    Self {
      model: model.into(),
      ..Default::default()
    }
  }

  pub fn with_persona(mut self, persona: impl Into<String>) -> Self {
    self.persona = Some(persona.into());
    self
  }

  pub fn with_max_steps(mut self, max_steps: usize) -> Self {
    self.max_steps = max_steps;
    self
  }

  /// Configure the USD spend cap. See [`PlanExecuteConfig::cost_limit_usd`].
  pub fn with_cost_limit_usd(mut self, budget_usd: f64) -> Self {
    self.cost_limit_usd = Some(budget_usd);
    self
  }

  /// Configure the pricing table used to cost the planner call. See
  /// [`PlanExecuteConfig::pricing_table`].
  pub fn with_pricing_table(mut self, table: crate::eval::PricingTable) -> Self {
    self.pricing_table = table;
    self
  }

  /// Require the final answer to validate against `schema` (V2.1). See
  /// [`PlanExecuteConfig::output_schema`].
  pub fn with_output_schema(mut self, schema: Value) -> Self {
    self.output_schema = Some(schema);
    self
  }

  /// Configure the schema-correction retry budget. See
  /// [`PlanExecuteConfig::max_schema_correction_attempts`].
  pub fn with_max_schema_correction_attempts(mut self, attempts: usize) -> Self {
    self.max_schema_correction_attempts = attempts;
    self
  }
}

/// One step produced by the planner model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanExecuteStep {
  pub id: String,
  pub description: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tool: Option<String>,
  #[serde(default)]
  pub params: Value,
  /// Optional explicit dependencies (P-A2.2). When emitting a `Flow`
  /// ([`PlanExecuteAgent::compile_plan_to_flow`]), an empty `depends_on` chains
  /// the step after the previous tool step (preserving sequential semantics);
  /// a non-empty list lets the planner express a parallel DAG instead.
  #[serde(default)]
  pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PlanExecutePlan {
  #[serde(default)]
  plan: Vec<PlanExecuteStep>,
  #[serde(default)]
  final_answer: Option<String>,
}

/// A minimal Plan-and-Execute runtime.
///
/// The planner model must return JSON shaped like:
///
/// ```json
/// {
///   "plan": [
///     {"id":"1","description":"Look up data","tool":"search","params":{"q":"..."}}
///   ],
///   "final_answer": "optional answer when no tool is needed"
/// }
/// ```
pub struct PlanExecuteAgent {
  config: PlanExecuteConfig,
  memory: Box<dyn MemoryStore>,
  tools: Arc<ToolRegistry>,
  memory_hook: Option<Arc<dyn AgentMemoryHook>>,
  pub session_id: String,
  /// Token counter for `Message::*_with_counter` calls
  /// (P10.3.3-FU1). Mirrors the `ReActAgent` field — see that
  /// crate's docstring for the precision rationale.
  message_counter: Box<dyn yanshi_memory::TokenCounter>,
}

impl PlanExecuteAgent {
  pub fn new(
    config: PlanExecuteConfig,
    memory: Box<dyn MemoryStore>,
    tools: Arc<ToolRegistry>,
  ) -> Self {
    let message_counter = crate::token_counter_adapter::build_message_counter(&config.model);
    Self {
      config,
      memory,
      tools,
      memory_hook: None,
      session_id: uuid::Uuid::new_v4().to_string(),
      message_counter,
    }
  }

  pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
    self.session_id = session_id.into();
    self
  }

  pub fn with_memory_hook(mut self, hook: Arc<dyn AgentMemoryHook>) -> Self {
    self.memory_hook = Some(hook);
    self
  }

  /// Read-only view of the agent's tool registry. Mirrors
  /// `ReActAgent::tools` (V2.3: needed so `yanshi-cli`'s
  /// `harness run`/`chat` can snapshot + re-wrap the registry with the
  /// approval-gate pipeline regardless of which runtime the CLI built).
  pub fn tools(&self) -> &Arc<ToolRegistry> {
    &self.tools
  }

  /// Replace the agent's tool registry (builder-style setter). Mirrors
  /// `ReActAgent::with_tools`.
  pub fn with_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
    self.tools = tools;
    self
  }

  /// Compile a plan's tool steps into an executable [`Flow`] (P-A2.2) — the
  /// "emit a `Flow`" path that lets a Plan-and-Execute plan run on the
  /// deterministic graph engine (inheriting retry / checkpoint / timeout /
  /// tracing / replay, and parallelism where the plan allows) instead of the
  /// hand-rolled sequential loop in [`Self::run_with_context`].
  ///
  /// Reuses [`crate::dynamic::compile_plan_to_flow`]. Pure-reasoning steps
  /// (`tool = None`) carry no executable node and are dropped. Ordering is
  /// preserved by default: a step with an empty `depends_on` is chained after
  /// the previous tool step, so a plan that didn't ask for parallelism still
  /// runs in order; a step that declares `depends_on` opts into a parallel DAG.
  ///
  /// A reasoning step cannot be a dependency target (it produces no output); a
  /// dependent referencing one is rejected by the underlying compiler as a
  /// dangling dependency.
  pub fn compile_plan_to_flow(&self, steps: &[PlanExecuteStep]) -> Result<Flow, YanshiError> {
    let mut wf_steps = Vec::new();
    let mut prev_tool_id: Option<String> = None;
    for step in steps {
      let Some(tool) = &step.tool else {
        continue; // reasoning-only step: nothing to execute.
      };
      let depends_on = if !step.depends_on.is_empty() {
        step.depends_on.clone()
      } else if let Some(prev) = &prev_tool_id {
        vec![prev.clone()]
      } else {
        Vec::new()
      };
      wf_steps.push(crate::dynamic::WorkflowPlanStep {
        id: step.id.clone(),
        kind: crate::dynamic::PlanStepKind::Tool,
        tool: tool.clone(),
        params: step.params.clone(),
        depends_on,
        run_if: None,
      });
      prev_tool_id = Some(step.id.clone());
    }
    let plan = crate::dynamic::WorkflowPlan { steps: wf_steps };
    crate::dynamic::compile_plan_to_flow(&plan, Arc::clone(&self.tools))
  }

  /// Plan with the LLM, compile the plan to a [`Flow`], and execute it via the
  /// injected `runner` (P-A2.2) — the end-to-end "emit a `Flow`" path.
  ///
  /// Shares the planner / memory / limit handling with [`Self::run_with_context`]
  /// (cancellation, timeout, token + step + tool-call budgets all honoured), but
  /// runs the plan on the deterministic graph engine — inheriting retry /
  /// checkpoint / timeout / tracing / replay and the plan's parallelism — instead
  /// of the hand-rolled sequential loop. The returned [`AgentRunResult`] carries
  /// an `Observe` → `Plan` → per-node `ToolCall`/`ToolResult` → `FinalAnswer`
  /// trace built from the flow's state pool; a failed node stops with
  /// [`AgentStopReason::Error`].
  ///
  /// Surfaces inject `yanshi_core::CoreFlowRunner` (typically `concurrent(n)`).
  ///
  /// V2.1: validates the final answer against
  /// [`PlanExecuteConfig::output_schema`] when configured, retrying the
  /// whole plan-and-execute cycle on a mismatch — see
  /// [`Self::run_with_context`]'s doc comment for the rationale (shared
  /// verbatim between the two entry points).
  pub async fn run_as_flow(
    &mut self,
    context: AgentContext,
    runner: Arc<dyn FlowRunner>,
  ) -> Result<AgentRunResult, PlanExecuteError> {
    let Some(schema) = self.config.output_schema.clone() else {
      return self.run_as_flow_once(context, runner).await;
    };

    let mut attempt = 0usize;
    let mut current_input = context.input.clone();
    loop {
      let mut this_context = context.clone();
      this_context.input = current_input;
      let result = self.run_as_flow_once(this_context, runner.clone()).await?;

      if !matches!(result.stop_reason, AgentStopReason::FinalAnswer) {
        return Ok(result);
      }
      let Some(answer) = result.answer.clone() else {
        return Ok(result);
      };
      match yanshi_agent_spi::validate_json_str_against_schema(&schema, &answer) {
        Ok(()) => return Ok(result),
        Err(errors) => {
          attempt += 1;
          if attempt > self.config.max_schema_correction_attempts {
            return Err(PlanExecuteError::SchemaValidationFailed {
              errors,
              attempts: attempt,
            });
          }
          warn!(
            attempt,
            max_attempts = self.config.max_schema_correction_attempts,
            "PlanExecute (flow mode) final_answer failed output_schema validation; retrying"
          );
          current_input = format!(
            "Your previous final_answer did not match the required output schema: {}. \
             The previous answer was: {}. Correct it and provide the final answer again, \
             matching the schema exactly.",
            errors.join("; "),
            answer
          );
        }
      }
    }
  }

  async fn run_as_flow_once(
    &mut self,
    context: AgentContext,
    runner: Arc<dyn FlowRunner>,
  ) -> Result<AgentRunResult, PlanExecuteError> {
    self.apply_context(&context);
    info!(
      session = %self.session_id,
      model = %self.config.model,
      "PlanExecuteAgent (flow mode) starting"
    );

    let mut steps = vec![AgentStep::new(
      0,
      AgentStepKind::Observe {
        input: context.input.clone(),
      },
    )];
    let events = vec![AgentEvent::RunStarted {
      session_id: self.session_id.clone(),
      model: self.config.model.clone(),
      timestamp: context.started_at,
    }];
    let mut step_index = 1usize;
    let max_steps = context.limits.max_steps.unwrap_or(self.config.max_steps);
    let max_tool_calls = context.limits.max_tool_calls;
    let timeout_ms = context.limits.timeout_ms;
    let token_budget = context.limits.token_budget;
    let cancellation_token = context.cancellation_token.clone();
    let run_started_at = Instant::now();

    self
      .add_memory_message(Message::user_with_counter(
        &self.session_id,
        &context.input,
        &*self.message_counter,
      ))
      .await?;
    if is_cancelled(&cancellation_token) {
      return Ok(self.cancelled_result("cancellation token signalled", steps, events));
    }

    let history = self.read_memory_history(20).await?;
    let planner_response = match self
      .call_planner(
        &context.input,
        &history,
        run_started_at,
        timeout_ms,
        cancellation_token.clone(),
        context.trace_context.clone(),
      )
      .await
    {
      Ok(response) => response,
      Err(PlanExecuteError::Timeout { timeout_ms }) => {
        return Ok(self.timeout_result(Some(timeout_ms), steps, events));
      }
      Err(PlanExecuteError::Cancelled { reason }) => {
        return Ok(self.cancelled_result(reason, steps, events));
      }
      Err(err) => return Err(err),
    };
    let planner_text = planner_response.content.clone();
    self
      .add_memory_message(Message::assistant_with_counter(
        &self.session_id,
        &planner_text,
        &*self.message_counter,
      ))
      .await?;

    if let Some(budget) = token_budget {
      let used = self.memory.session_token_count(&self.session_id).await?;
      if used > budget {
        return Ok(self.stopped_result(
          None,
          AgentStopReason::TokenBudgetExceeded { used, budget },
          steps,
          events,
        ));
      }
    }

    // T1.1: cost-limit guard for the single planner call. PlanExecute
    // makes exactly one LLM call per run, so there is no cross-turn
    // accumulation to track — the planner call's own cost is the run's
    // total cost.
    if let Some(budget) = context.limits.cost_limit_usd.or(self.config.cost_limit_usd) {
      let used_usd = self.cost_for_response(&planner_response);
      if used_usd > budget {
        return Ok(self.stopped_result(
          None,
          AgentStopReason::CostLimitExceeded {
            used_usd,
            budget_usd: budget,
          },
          steps,
          events,
        ));
      }
    }

    let plan = if !planner_response.tool_calls.is_empty() {
      plan_from_tool_calls(&planner_response.tool_calls)
    } else {
      parse_plan(&planner_text)?
    };
    if plan.plan.len() > max_steps {
      return Ok(self.stopped_result(None, AgentStopReason::MaxSteps { max_steps }, steps, events));
    }
    let tool_count = plan.plan.iter().filter(|s| s.tool.is_some()).count();
    if let Some(max) = max_tool_calls
      && tool_count > max
    {
      return Ok(self.stopped_result(
        None,
        AgentStopReason::MaxToolCalls {
          max_tool_calls: max,
        },
        steps,
        events,
      ));
    }

    if !plan.plan.is_empty() {
      let thought = plan
        .plan
        .iter()
        .map(|step| format!("{}. {}", step.id, step.description))
        .collect::<Vec<_>>()
        .join("\n");
      steps.push(AgentStep::new(step_index, AgentStepKind::Plan { thought }));
      step_index += 1;
    }

    // Compile + execute the plan on the deterministic engine.
    let flow = self.compile_plan_to_flow(&plan.plan)?;
    let state = match timeout_ms {
      Some(ms) => {
        match tokio::time::timeout(Duration::from_millis(ms), runner.run(&flow, HashMap::new()))
          .await
        {
          Ok(result) => result?,
          Err(_elapsed) => return Ok(self.timeout_result(Some(ms), steps, events)),
        }
      }
      None => runner.run(&flow, HashMap::new()).await?,
    };

    // Translate node outputs into the step trace, in plan order.
    let mut failure: Option<String> = None;
    let mut last_result: Option<String> = None;
    for planned in &plan.plan {
      let Some(tool) = &planned.tool else {
        continue;
      };
      steps.push(AgentStep::new(
        step_index,
        AgentStepKind::ToolCall {
          tool: tool.clone(),
          params: planned.params.clone(),
        },
      ));
      step_index += 1;
      match state.get(&planned.id) {
        Some(Ok(outputs)) => {
          let content = outputs
            .get("result")
            .map(flow_value_to_string)
            .unwrap_or_default();
          last_result = Some(content.clone());
          steps.push(AgentStep::new(
            step_index,
            AgentStepKind::ToolResult {
              tool: tool.clone(),
              content,
              is_error: false,
              parts: Vec::new(),
            },
          ));
          step_index += 1;
        }
        Some(Err(err)) => {
          let message = err.to_string();
          if failure.is_none() {
            failure = Some(message.clone());
          }
          steps.push(AgentStep::new(
            step_index,
            AgentStepKind::ToolResult {
              tool: tool.clone(),
              content: message,
              is_error: true,
              parts: Vec::new(),
            },
          ));
          step_index += 1;
        }
        None => {}
      }
    }

    if let Some(message) = failure {
      return Ok(self.stopped_result(None, AgentStopReason::Error { message }, steps, events));
    }
    let answer = plan.final_answer.clone().or(last_result);
    if let Some(answer) = &answer {
      steps.push(AgentStep::new(
        step_index,
        AgentStepKind::FinalAnswer {
          answer: answer.clone(),
        },
      ));
    }
    Ok(self.stopped_result(answer, AgentStopReason::FinalAnswer, steps, events))
  }

  /// Plan with the LLM, execute the plan sequentially, and validate the
  /// final answer against [`PlanExecuteConfig::output_schema`] (V2.1) when
  /// configured — retrying the whole plan-and-execute cycle (not just the
  /// answer) up to [`PlanExecuteConfig::max_schema_correction_attempts`]
  /// times on a schema mismatch before hard-erroring with
  /// [`PlanExecuteError::SchemaValidationFailed`].
  ///
  /// Unlike `ReActAgent`'s `output_schema` support (a genuine mid-loop
  /// retry within one run — see that type's doc comment), `PlanExecuteAgent`
  /// plans in a single LLM call per attempt; there's no iterative loop to
  /// hook a retry into internally. So a schema mismatch here retries the
  /// *whole* cycle via `Self::run_with_context_once`, which is the
  /// architecturally honest equivalent for a "plan once, execute" runtime.
  /// Byte-identical to the pre-V2.1 single-call behaviour when
  /// `output_schema` is `None`.
  pub async fn run_with_context(
    &mut self,
    context: AgentContext,
  ) -> Result<AgentRunResult, PlanExecuteError> {
    let Some(schema) = self.config.output_schema.clone() else {
      return self.run_with_context_once(context).await;
    };

    let mut attempt = 0usize;
    let mut current_input = context.input.clone();
    loop {
      let mut this_context = context.clone();
      this_context.input = current_input;
      let result = self.run_with_context_once(this_context).await?;

      if !matches!(result.stop_reason, AgentStopReason::FinalAnswer) {
        return Ok(result);
      }
      let Some(answer) = result.answer.clone() else {
        return Ok(result);
      };
      match yanshi_agent_spi::validate_json_str_against_schema(&schema, &answer) {
        Ok(()) => return Ok(result),
        Err(errors) => {
          attempt += 1;
          if attempt > self.config.max_schema_correction_attempts {
            return Err(PlanExecuteError::SchemaValidationFailed {
              errors,
              attempts: attempt,
            });
          }
          warn!(
            attempt,
            max_attempts = self.config.max_schema_correction_attempts,
            "PlanExecute final_answer failed output_schema validation; retrying"
          );
          current_input = format!(
            "Your previous final_answer did not match the required output schema: {}. \
             The previous answer was: {}. Correct it and provide the final answer again, \
             matching the schema exactly.",
            errors.join("; "),
            answer
          );
        }
      }
    }
  }

  /// V2.4: thin wrapper around [`Self::run_plan_execute_loop`] that clears
  /// the loop checkpoint on genuine completion, uniformly across every
  /// exit path inside the loop (cancellation, timeout, budget limits,
  /// success) without threading a clear-call through each individual
  /// early return.
  async fn run_with_context_once(
    &mut self,
    context: AgentContext,
  ) -> Result<AgentRunResult, PlanExecuteError> {
    let checkpointer = context.loop_checkpointer.clone();
    let result = self.run_plan_execute_loop(context).await?;
    if let Some(checkpointer) = checkpointer
      && crate::checkpoint::should_clear_checkpoint(&result.stop_reason)
      && let Err(e) = checkpointer.0.clear(&self.session_id).await
    {
      warn!(session = %self.session_id, error = %e, "agent loop checkpoint clear failed");
    }
    Ok(result)
  }

  async fn run_plan_execute_loop(
    &mut self,
    context: AgentContext,
  ) -> Result<AgentRunResult, PlanExecuteError> {
    self.apply_context(&context);
    info!(
      session = %self.session_id,
      model = %self.config.model,
      "PlanExecuteAgent starting"
    );

    let mut steps = vec![AgentStep::new(
      0,
      AgentStepKind::Observe {
        input: context.input.clone(),
      },
    )];
    let mut events = vec![AgentEvent::RunStarted {
      session_id: self.session_id.clone(),
      model: self.config.model.clone(),
      timestamp: context.started_at,
    }];
    let mut step_index = 1usize;
    let max_steps = context.limits.max_steps.unwrap_or(self.config.max_steps);
    let max_tool_calls = context.limits.max_tool_calls;
    let timeout_ms = context.limits.timeout_ms;
    // Q2.9.2: respect `token_budget` like ReActAgent does. Pre-fix
    // PlanExecute read every other RuntimeLimits field but silently
    // dropped `token_budget`, so a workflow that capped the planner
    // at e.g. 4096 tokens still ran unbounded.
    let token_budget = context.limits.token_budget;
    let cancellation_token = context.cancellation_token.clone();
    let run_started_at = Instant::now();

    self
      .add_memory_message(Message::user_with_counter(
        &self.session_id,
        &context.input,
        &*self.message_counter,
      ))
      .await?;

    if is_cancelled(&cancellation_token) {
      return Ok(self.cancelled_result("cancellation token signalled", steps, events));
    }

    let history = self.read_memory_history(20).await?;
    let planner_response = self
      .call_planner(
        &context.input,
        &history,
        run_started_at,
        timeout_ms,
        cancellation_token.clone(),
        context.trace_context.clone(),
      )
      .await;
    let planner_response = match planner_response {
      Ok(response) => response,
      Err(PlanExecuteError::Timeout { timeout_ms }) => {
        return Ok(self.timeout_result(Some(timeout_ms), steps, events));
      }
      Err(PlanExecuteError::Cancelled { reason }) => {
        return Ok(self.cancelled_result(reason, steps, events));
      }
      Err(err) => return Err(err),
    };
    let planner_text = planner_response.content.clone();
    // Q5.2: planner output may contain user input / PII echoed back —
    // DEBUG emits fingerprint + length only, full text TRACE-only.
    debug!(
      response_len = planner_text.len(),
      response_sha = %prompt_fingerprint(&planner_text),
      "PlanExecute planner responded"
    );
    tracing::trace!(response = %planner_text, "PlanExecute planner response body");
    self
      .add_memory_message(Message::assistant_with_counter(
        &self.session_id,
        &planner_text,
        &*self.message_counter,
      ))
      .await?;

    // Q2.9.2: enforce `token_budget` after the planner reply lands
    // in memory. ReActAgent does the same check at the top of each
    // iteration; for PlanExecute the natural check-point is right
    // after we've ingested the planner output (it's the largest
    // single token consumer in the run).
    if let Some(budget) = token_budget {
      let used = self.memory.session_token_count(&self.session_id).await?;
      if used > budget {
        return Ok(self.stopped_result(
          None,
          AgentStopReason::TokenBudgetExceeded { used, budget },
          steps,
          events,
        ));
      }
    }

    // T1.1: cost-limit guard for the single planner call — see the
    // matching check + comment in `run_as_flow`.
    if let Some(budget) = context.limits.cost_limit_usd.or(self.config.cost_limit_usd) {
      let used_usd = self.cost_for_response(&planner_response);
      if used_usd > budget {
        return Ok(self.stopped_result(
          None,
          AgentStopReason::CostLimitExceeded {
            used_usd,
            budget_usd: budget,
          },
          steps,
          events,
        ));
      }
    }

    let plan = if !planner_response.tool_calls.is_empty() {
      // Native tool calls drive the plan directly: each call becomes one
      // sequential plan step. Falls back to JSON parsing only when the
      // model emits no tool calls (legacy prompt protocol).
      plan_from_tool_calls(&planner_response.tool_calls)
    } else {
      parse_plan(&planner_text)?
    };
    if plan.plan.len() > max_steps {
      return Ok(self.stopped_result(None, AgentStopReason::MaxSteps { max_steps }, steps, events));
    }

    if !plan.plan.is_empty() {
      let thought = plan
        .plan
        .iter()
        .map(|step| format!("{}. {}", step.id, step.description))
        .collect::<Vec<_>>()
        .join("\n");
      steps.push(AgentStep::new(step_index, AgentStepKind::Plan { thought }));
      step_index += 1;
    }

    let mut observations = Vec::new();
    let mut tool_calls = 0usize;
    // V2.4: the frozen plan (both `.plan` and `.final_answer` — the
    // latter matters when the planner answers directly without needing
    // every step executed) + the planner call's cost estimate, both
    // constant across every checkpoint saved this run (no further LLM
    // calls happen during the sequential execute loop below). Captured
    // before the loop moves `plan.plan` out.
    let plan_steps_json = serde_json::to_value(&plan).unwrap_or(Value::Null);
    let planner_cost_usd = self.cost_for_response(&planner_response);
    let system_prompt = self.system_prompt();
    for (plan_position, planned_step) in plan.plan.into_iter().enumerate() {
      if is_cancelled(&cancellation_token) {
        return Ok(self.cancelled_result("cancellation token signalled", steps, events));
      }
      if timed_out(run_started_at, timeout_ms) {
        return Ok(self.timeout_result(timeout_ms, steps, events));
      }

      let Some(tool) = planned_step.tool else {
        observations.push(planned_step.description);
        self
          .save_plan_execute_checkpoint(
            &context,
            &plan_steps_json,
            plan_position + 1,
            &steps,
            &events,
            step_index,
            tool_calls,
            &observations,
            &system_prompt,
            &context.input,
            planner_cost_usd,
            None,
          )
          .await;
        continue;
      };

      // V2.3: a reserved pseudo-tool name in the plan pauses the loop
      // to ask the user a question, rather than dispatching a real
      // tool — PlanExecuteAgent has no mid-loop LLM re-entry point to
      // intercept the way ReActAgent's `ask_user` native tool call
      // does, so the planner emits this as a plan step instead. Checked
      // before the `max_tool_calls` budget — asking a question isn't a
      // tool call and shouldn't consume it.
      if tool == crate::react::agent::ASK_USER_TOOL_NAME {
        let question = planned_step
          .params
          .get("question")
          .and_then(Value::as_str)
          .unwrap_or_default()
          .to_string();
        events.push(AgentEvent::InterruptRequested {
          session_id: self.session_id.clone(),
          step_index,
          question: question.clone(),
          timestamp: Utc::now(),
        });
        steps.push(AgentStep::new(
          step_index,
          AgentStepKind::ToolCall {
            tool: tool.clone(),
            params: planned_step.params.clone(),
          },
        ));
        step_index += 1;
        // `plan_position` (not `+1`) — this step is not yet complete;
        // resume must still "finish" it with the answer.
        self
          .save_plan_execute_checkpoint(
            &context,
            &plan_steps_json,
            plan_position,
            &steps,
            &events,
            step_index,
            tool_calls,
            &observations,
            &system_prompt,
            &context.input,
            planner_cost_usd,
            Some(question.clone()),
          )
          .await;
        return Ok(self.stopped_result(
          None,
          AgentStopReason::AwaitingInput { question },
          steps,
          events,
        ));
      }

      if let Some(max_tool_calls) = max_tool_calls
        && tool_calls >= max_tool_calls
      {
        return Ok(self.stopped_result(
          None,
          AgentStopReason::MaxToolCalls { max_tool_calls },
          steps,
          events,
        ));
      }

      let params = planned_step.params;
      let tool_step_index = step_index;
      let metadata = self.tools.tool_metadata(&tool);
      let (tool_source, tool_permissions) = tool_event_metadata(metadata.as_ref());
      if let Ok(decision) = self.tools.evaluate_policy(&tool, &params) {
        events.push(AgentEvent::ToolPolicyDecision {
          session_id: self.session_id.clone(),
          step_index: tool_step_index,
          tool: tool.clone(),
          allowed: decision.allowed,
          matched_rule: decision.matched_rule,
          deny_reason: decision.deny_reason,
          source: decision.source,
          permissions: decision.permissions,
          params_summary: decision.params_summary,
          timestamp: Utc::now(),
        });
      }
      if let Ok(effective) = self.tools.evaluate_capabilities(&tool) {
        events.push(AgentEvent::ToolCapabilityDecision {
          session_id: self.session_id.clone(),
          step_index: tool_step_index,
          tool: tool.clone(),
          allowed: effective.allowed,
          required: effective.required,
          effective: effective.effective,
          denied: effective.denied,
          deny_reason: effective.deny_reason,
          trace: effective.trace,
          sandbox: effective.sandbox,
          timestamp: Utc::now(),
        });
      }
      events.push(AgentEvent::ToolCallStarted {
        session_id: self.session_id.clone(),
        step_index: tool_step_index,
        tool: tool.clone(),
        params: params.clone(),
        source: tool_source.clone(),
        permissions: tool_permissions.clone(),
        timestamp: Utc::now(),
      });
      steps.push(AgentStep::new(
        tool_step_index,
        AgentStepKind::ToolCall {
          tool: tool.clone(),
          params: params.clone(),
        },
      ));
      step_index += 1;

      let started_at = Instant::now();
      let output = match self
        .execute_tool(
          &tool,
          params,
          run_started_at,
          timeout_ms,
          cancellation_token.clone(),
        )
        .await
      {
        Ok(output) => output,
        Err(PlanExecuteError::Cancelled { reason }) => {
          events.push(AgentEvent::ToolCallCompleted {
            session_id: self.session_id.clone(),
            step_index: tool_step_index,
            tool: tool.clone(),
            is_error: true,
            duration_ms: started_at.elapsed().as_millis() as u64,
            source: tool_source.clone(),
            permissions: tool_permissions.clone(),
            timestamp: Utc::now(),
          });
          return Ok(self.cancelled_result(reason, steps, events));
        }
        Err(PlanExecuteError::Timeout { timeout_ms }) => {
          events.push(AgentEvent::ToolCallCompleted {
            session_id: self.session_id.clone(),
            step_index: tool_step_index,
            tool: tool.clone(),
            is_error: true,
            duration_ms: started_at.elapsed().as_millis() as u64,
            source: tool_source.clone(),
            permissions: tool_permissions.clone(),
            timestamp: Utc::now(),
          });
          return Ok(self.timeout_result(Some(timeout_ms), steps, events));
        }
        Err(PlanExecuteError::ApprovalDenied { message }) => {
          events.push(AgentEvent::ToolCallCompleted {
            session_id: self.session_id.clone(),
            step_index: tool_step_index,
            tool: tool.clone(),
            is_error: true,
            duration_ms: started_at.elapsed().as_millis() as u64,
            source: tool_source.clone(),
            permissions: tool_permissions.clone(),
            timestamp: Utc::now(),
          });
          return Ok(self.stopped_result(
            None,
            AgentStopReason::ApprovalDenied { message },
            steps,
            events,
          ));
        }
        Err(err) => {
          warn!(tool = %tool, error = %err, "PlanExecute tool execution failed");
          yanshi_tool::ToolOutput::error(err.to_string())
        }
      };
      let duration_ms = started_at.elapsed().as_millis() as u64;
      events.push(AgentEvent::ToolCallCompleted {
        session_id: self.session_id.clone(),
        step_index: tool_step_index,
        tool: tool.clone(),
        is_error: output.is_error,
        duration_ms,
        source: tool_source.clone(),
        permissions: tool_permissions.clone(),
        timestamp: Utc::now(),
      });
      steps.push(
        AgentStep::new(
          step_index,
          AgentStepKind::ToolResult {
            tool: tool.clone(),
            content: output.content.clone(),
            is_error: output.is_error,
            parts: output.parts.clone(),
          },
        )
        .with_duration_ms(duration_ms),
      );
      step_index += 1;
      tool_calls += 1;

      self
        .add_memory_message(Message::tool_result_with_counter(
          &self.session_id,
          &tool,
          &output.content,
          &*self.message_counter,
        ))
        .await?;
      observations.push(output.content);
      self
        .save_plan_execute_checkpoint(
          &context,
          &plan_steps_json,
          plan_position + 1,
          &steps,
          &events,
          step_index,
          tool_calls,
          &observations,
          &system_prompt,
          &context.input,
          planner_cost_usd,
          None,
        )
        .await;
    }

    let answer = plan.final_answer.unwrap_or_else(|| {
      if observations.is_empty() {
        "Plan completed with no tool observations.".to_string()
      } else {
        observations.join("\n")
      }
    });
    self
      .add_memory_message(Message::assistant_with_counter(
        &self.session_id,
        &answer,
        &*self.message_counter,
      ))
      .await?;
    steps.push(AgentStep::new(
      step_index,
      AgentStepKind::FinalAnswer {
        answer: answer.clone(),
      },
    ));

    Ok(self.stopped_result(Some(answer), AgentStopReason::FinalAnswer, steps, events))
  }

  /// V2.4: save an [`yanshi_agent_spi::checkpoint::AgentLoopCheckpoint`]
  /// after a completed plan step (tool call or pure-reasoning), if a
  /// checkpointer is configured. A save failure is logged and swallowed —
  /// mirrors `ReActAgent`'s non-fatal checkpoint posture.
  #[allow(clippy::too_many_arguments)]
  async fn save_plan_execute_checkpoint(
    &self,
    context: &AgentContext,
    plan_steps_json: &Value,
    plan_position: usize,
    steps: &[AgentStep],
    events: &[AgentEvent],
    step_index: usize,
    tool_calls: usize,
    observations: &[String],
    system_prompt: &str,
    user_input: &str,
    cumulative_cost_usd: f64,
    pending_question: Option<String>,
  ) {
    let Some(checkpointer) = context.loop_checkpointer.as_ref() else {
      return;
    };
    let checkpoint = yanshi_agent_spi::checkpoint::AgentLoopCheckpoint {
      schema_version: yanshi_agent_spi::checkpoint::AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION,
      session_id: self.session_id.clone(),
      runtime_kind: yanshi_agent_spi::checkpoint::LoopRuntimeKind::PlanExecute,
      created_at: Utc::now(),
      steps: steps.to_vec(),
      events: events.to_vec(),
      step_index,
      iteration: 0,
      tool_calls,
      verification_attempts: 0,
      schema_correction_attempts: 0,
      last_tool_call: None,
      recent_tool_calls: std::collections::VecDeque::new(),
      cumulative_cost_usd,
      system_prompt: system_prompt.to_string(),
      user_input: user_input.to_string(),
      trace_context: context.trace_context.clone(),
      plan_steps: plan_steps_json.clone(),
      plan_position,
      observations: observations.to_vec(),
      pending_question,
    };
    if let Err(e) = checkpointer.0.save(&checkpoint).await {
      warn!(session = %self.session_id, error = %e, "agent loop checkpoint save failed");
    }
  }

  /// V2.4: resume a loop interrupted by a process restart from a saved
  /// [`yanshi_agent_spi::checkpoint::AgentLoopCheckpoint`], continuing
  /// plan execution from the checkpointed step instead of re-planning.
  /// Simpler than `ReActAgent::resume_from_loop_checkpoint`: no LLM
  /// re-call is needed at all — the plan was already frozen into the
  /// checkpoint, so this skips the planner call entirely and re-enters
  /// the execute loop at `plan_position`.
  ///
  /// Unlike `ReActAgent`'s equivalent, this never reads conversation
  /// memory back (the checkpoint itself carries every field the resumed
  /// loop needs — plan, steps, events, observations), so there is no
  /// hard durability requirement on the `MemoryStore` for *correctness*.
  /// It does still *write* new tool-result / final-answer messages as
  /// execution continues; pointing it at the same durable, session-id-
  /// keyed store the pre-interruption run used (e.g. `SqliteMemory`)
  /// keeps the overall conversation record contiguous rather than
  /// fragmenting it across two stores.
  pub async fn resume_from_loop_checkpoint(
    &mut self,
    context: AgentContext,
    checkpoint: yanshi_agent_spi::checkpoint::AgentLoopCheckpoint,
    answer: Option<String>,
  ) -> Result<AgentRunResult, PlanExecuteError> {
    if checkpoint.runtime_kind != yanshi_agent_spi::checkpoint::LoopRuntimeKind::PlanExecute {
      return Err(PlanExecuteError::PlanParse {
        message: format!(
          "expected a PlanExecute loop checkpoint, found {:?}",
          checkpoint.runtime_kind
        ),
      });
    }
    match (&checkpoint.pending_question, &answer) {
      (Some(_), None) => {
        return Err(PlanExecuteError::InvalidCheckpoint {
          message: "checkpoint is paused on a question but no answer was supplied".to_string(),
        });
      }
      (None, Some(_)) => {
        return Err(PlanExecuteError::InvalidCheckpoint {
          message: "an answer was supplied but the checkpoint has no pending question".to_string(),
        });
      }
      _ => {}
    }
    self.apply_context(&context);

    let plan: PlanExecutePlan =
      serde_json::from_value(checkpoint.plan_steps.clone()).map_err(|e| {
        PlanExecuteError::PlanParse {
          message: format!("failed to deserialize checkpointed plan: {e}"),
        }
      })?;

    let mut steps = checkpoint.steps.clone();
    let mut events = checkpoint.events.clone();
    let mut step_index = checkpoint.step_index;
    let mut tool_calls = checkpoint.tool_calls;
    let mut observations = checkpoint.observations.clone();
    let max_tool_calls = context.limits.max_tool_calls;
    let timeout_ms = context.limits.timeout_ms;
    let cancellation_token = context.cancellation_token.clone();
    let run_started_at = Instant::now();
    let plan_steps_json = checkpoint.plan_steps.clone();
    let system_prompt = checkpoint.system_prompt.clone();

    // V2.3: when an answer is supplied, it's the paused `ask_user`
    // step's synthetic tool result — exactly mirrors how a real tool's
    // output would have entered `observations`. The step is now
    // complete, so resume past it (`+ 1`); otherwise resume at the
    // checkpointed position unchanged (V2.4's plain crash-resume case).
    let resume_plan_position = if let Some(answer) = &answer {
      let question = checkpoint.pending_question.as_deref().unwrap_or("question");
      steps.push(AgentStep::new(
        step_index,
        AgentStepKind::ToolResult {
          tool: crate::react::agent::ASK_USER_TOOL_NAME.to_string(),
          content: answer.clone(),
          is_error: false,
          parts: Vec::new(),
        },
      ));
      step_index += 1;
      observations.push(format!("{question}: {answer}"));
      checkpoint.plan_position + 1
    } else {
      checkpoint.plan_position
    };

    for (plan_position, planned_step) in
      plan.plan.into_iter().enumerate().skip(resume_plan_position)
    {
      if is_cancelled(&cancellation_token) {
        return Ok(self.cancelled_result("cancellation token signalled", steps, events));
      }
      if timed_out(run_started_at, timeout_ms) {
        return Ok(self.timeout_result(timeout_ms, steps, events));
      }

      let Some(tool) = planned_step.tool else {
        observations.push(planned_step.description);
        self
          .save_plan_execute_checkpoint(
            &context,
            &plan_steps_json,
            plan_position + 1,
            &steps,
            &events,
            step_index,
            tool_calls,
            &observations,
            &system_prompt,
            &checkpoint.user_input,
            checkpoint.cumulative_cost_usd,
            None,
          )
          .await;
        continue;
      };

      // V2.3: see the identical branch in `run_plan_execute_loop` — a
      // resumed plan can contain further `ask_user` steps.
      if tool == crate::react::agent::ASK_USER_TOOL_NAME {
        let question = planned_step
          .params
          .get("question")
          .and_then(Value::as_str)
          .unwrap_or_default()
          .to_string();
        events.push(AgentEvent::InterruptRequested {
          session_id: self.session_id.clone(),
          step_index,
          question: question.clone(),
          timestamp: Utc::now(),
        });
        steps.push(AgentStep::new(
          step_index,
          AgentStepKind::ToolCall {
            tool: tool.clone(),
            params: planned_step.params.clone(),
          },
        ));
        step_index += 1;
        self
          .save_plan_execute_checkpoint(
            &context,
            &plan_steps_json,
            plan_position,
            &steps,
            &events,
            step_index,
            tool_calls,
            &observations,
            &system_prompt,
            &checkpoint.user_input,
            checkpoint.cumulative_cost_usd,
            Some(question.clone()),
          )
          .await;
        return Ok(self.stopped_result(
          None,
          AgentStopReason::AwaitingInput { question },
          steps,
          events,
        ));
      }

      if let Some(max_tool_calls) = max_tool_calls
        && tool_calls >= max_tool_calls
      {
        return Ok(self.stopped_result(
          None,
          AgentStopReason::MaxToolCalls { max_tool_calls },
          steps,
          events,
        ));
      }

      let params = planned_step.params;
      let tool_step_index = step_index;
      let metadata = self.tools.tool_metadata(&tool);
      let (tool_source, tool_permissions) = tool_event_metadata(metadata.as_ref());
      if let Ok(decision) = self.tools.evaluate_policy(&tool, &params) {
        events.push(AgentEvent::ToolPolicyDecision {
          session_id: self.session_id.clone(),
          step_index: tool_step_index,
          tool: tool.clone(),
          allowed: decision.allowed,
          matched_rule: decision.matched_rule,
          deny_reason: decision.deny_reason,
          source: decision.source,
          permissions: decision.permissions,
          params_summary: decision.params_summary,
          timestamp: Utc::now(),
        });
      }
      if let Ok(effective) = self.tools.evaluate_capabilities(&tool) {
        events.push(AgentEvent::ToolCapabilityDecision {
          session_id: self.session_id.clone(),
          step_index: tool_step_index,
          tool: tool.clone(),
          allowed: effective.allowed,
          required: effective.required,
          effective: effective.effective,
          denied: effective.denied,
          deny_reason: effective.deny_reason,
          trace: effective.trace,
          sandbox: effective.sandbox,
          timestamp: Utc::now(),
        });
      }
      events.push(AgentEvent::ToolCallStarted {
        session_id: self.session_id.clone(),
        step_index: tool_step_index,
        tool: tool.clone(),
        params: params.clone(),
        source: tool_source.clone(),
        permissions: tool_permissions.clone(),
        timestamp: Utc::now(),
      });
      steps.push(AgentStep::new(
        tool_step_index,
        AgentStepKind::ToolCall {
          tool: tool.clone(),
          params: params.clone(),
        },
      ));
      step_index += 1;

      let started_at = Instant::now();
      let output = match self
        .execute_tool(
          &tool,
          params,
          run_started_at,
          timeout_ms,
          cancellation_token.clone(),
        )
        .await
      {
        Ok(output) => output,
        Err(PlanExecuteError::Cancelled { reason }) => {
          events.push(AgentEvent::ToolCallCompleted {
            session_id: self.session_id.clone(),
            step_index: tool_step_index,
            tool: tool.clone(),
            is_error: true,
            duration_ms: started_at.elapsed().as_millis() as u64,
            source: tool_source.clone(),
            permissions: tool_permissions.clone(),
            timestamp: Utc::now(),
          });
          return Ok(self.cancelled_result(reason, steps, events));
        }
        Err(PlanExecuteError::Timeout { timeout_ms }) => {
          events.push(AgentEvent::ToolCallCompleted {
            session_id: self.session_id.clone(),
            step_index: tool_step_index,
            tool: tool.clone(),
            is_error: true,
            duration_ms: started_at.elapsed().as_millis() as u64,
            source: tool_source.clone(),
            permissions: tool_permissions.clone(),
            timestamp: Utc::now(),
          });
          return Ok(self.timeout_result(Some(timeout_ms), steps, events));
        }
        Err(PlanExecuteError::ApprovalDenied { message }) => {
          events.push(AgentEvent::ToolCallCompleted {
            session_id: self.session_id.clone(),
            step_index: tool_step_index,
            tool: tool.clone(),
            is_error: true,
            duration_ms: started_at.elapsed().as_millis() as u64,
            source: tool_source.clone(),
            permissions: tool_permissions.clone(),
            timestamp: Utc::now(),
          });
          return Ok(self.stopped_result(
            None,
            AgentStopReason::ApprovalDenied { message },
            steps,
            events,
          ));
        }
        Err(err) => {
          warn!(tool = %tool, error = %err, "PlanExecute tool execution failed");
          yanshi_tool::ToolOutput::error(err.to_string())
        }
      };
      let duration_ms = started_at.elapsed().as_millis() as u64;
      events.push(AgentEvent::ToolCallCompleted {
        session_id: self.session_id.clone(),
        step_index: tool_step_index,
        tool: tool.clone(),
        is_error: output.is_error,
        duration_ms,
        source: tool_source.clone(),
        permissions: tool_permissions.clone(),
        timestamp: Utc::now(),
      });
      steps.push(
        AgentStep::new(
          step_index,
          AgentStepKind::ToolResult {
            tool: tool.clone(),
            content: output.content.clone(),
            is_error: output.is_error,
            parts: output.parts.clone(),
          },
        )
        .with_duration_ms(duration_ms),
      );
      step_index += 1;
      tool_calls += 1;

      self
        .add_memory_message(Message::tool_result_with_counter(
          &self.session_id,
          &tool,
          &output.content,
          &*self.message_counter,
        ))
        .await?;
      observations.push(output.content);
      self
        .save_plan_execute_checkpoint(
          &context,
          &plan_steps_json,
          plan_position + 1,
          &steps,
          &events,
          step_index,
          tool_calls,
          &observations,
          &system_prompt,
          &checkpoint.user_input,
          checkpoint.cumulative_cost_usd,
          None,
        )
        .await;
    }

    let answer = plan.final_answer.unwrap_or_else(|| {
      if observations.is_empty() {
        "Plan completed with no tool observations.".to_string()
      } else {
        observations.join("\n")
      }
    });
    self
      .add_memory_message(Message::assistant_with_counter(
        &self.session_id,
        &answer,
        &*self.message_counter,
      ))
      .await?;
    steps.push(AgentStep::new(
      step_index,
      AgentStepKind::FinalAnswer {
        answer: answer.clone(),
      },
    ));

    let result = self.stopped_result(Some(answer), AgentStopReason::FinalAnswer, steps, events);
    if let Some(checkpointer) = context.loop_checkpointer.as_ref()
      && crate::checkpoint::should_clear_checkpoint(&result.stop_reason)
      && let Err(e) = checkpointer.0.clear(&self.session_id).await
    {
      warn!(session = %self.session_id, error = %e, "agent loop checkpoint clear failed");
    }
    Ok(result)
  }

  /// T1.1: estimate the USD cost of the planner call from its reported
  /// token usage and `config.pricing_table`. Returns `0.0` when the
  /// provider didn't report usage or no pricing is configured.
  fn cost_for_response(&self, response: &LLMResponse) -> f64 {
    let usage = response.usage.as_ref();
    self
      .config
      .pricing_table
      .lookup(&self.config.model)
      .cost_for_call(
        usage.and_then(|u| u.prompt_tokens),
        usage.and_then(|u| u.completion_tokens),
      )
  }

  async fn call_planner(
    &self,
    input: &str,
    history: &[Message],
    run_started_at: Instant,
    timeout_ms: Option<u64>,
    cancellation_token: Option<AgentCancellationToken>,
    trace_context: Option<yanshi_llm::LlmTraceContext>,
  ) -> Result<LLMResponse, PlanExecuteError> {
    let mut user_prompt = String::new();
    if !history.is_empty() {
      user_prompt.push_str("Conversation history:\n");
      user_prompt.push_str(
        &history
          .iter()
          .map(Message::to_prompt_line)
          .collect::<Vec<_>>()
          .join("\n"),
      );
      user_prompt.push_str("\n\nCurrent task:\n");
    }
    user_prompt.push_str(input);

    let messages = vec![
      MultimodalMessage::text("system", self.system_prompt()),
      MultimodalMessage::text("user", user_prompt),
    ];
    let tool_specs = self.collect_tool_specs();
    let mut builder = Yanshi::model(&self.config.model)
      .multimodal_messages(messages)
      .trace_context(trace_context);
    if !tool_specs.is_empty() {
      builder = builder.tools(tool_specs);
    }
    let llm_call = builder.execute_full();

    match (
      remaining_timeout(run_started_at, timeout_ms),
      cancellation_token,
    ) {
      (Some(remaining), Some(token)) => {
        tokio::select! {
          result = tokio::time::timeout(remaining, llm_call) => match result {
            Ok(result) => Ok(result?),
            Err(_) => Err(PlanExecuteError::Timeout {
              timeout_ms: timeout_ms.unwrap_or_default(),
            }),
          },
          _ = token.cancelled() => Err(PlanExecuteError::Cancelled {
            reason: "cancellation token signalled".to_string(),
          }),
        }
      }
      (Some(remaining), None) => match tokio::time::timeout(remaining, llm_call).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(PlanExecuteError::Timeout {
          timeout_ms: timeout_ms.unwrap_or_default(),
        }),
      },
      (None, Some(token)) => {
        tokio::select! {
          result = llm_call => Ok(result?),
          _ = token.cancelled() => Err(PlanExecuteError::Cancelled {
            reason: "cancellation token signalled".to_string(),
          }),
        }
      }
      (None, None) => Ok(llm_call.await?),
    }
  }

  async fn execute_tool(
    &self,
    tool: &str,
    params: Value,
    run_started_at: Instant,
    timeout_ms: Option<u64>,
    cancellation_token: Option<AgentCancellationToken>,
  ) -> Result<yanshi_tool::ToolOutput, PlanExecuteError> {
    let tool_call = self.tools.execute(tool, params);
    match (
      remaining_timeout(run_started_at, timeout_ms),
      cancellation_token,
    ) {
      (Some(remaining), Some(token)) => {
        tokio::select! {
          result = tokio::time::timeout(remaining, tool_call) => match result {
            Ok(result) => tool_call_outcome(result),
            Err(_) => Err(PlanExecuteError::Timeout {
              timeout_ms: timeout_ms.unwrap_or_default(),
            }),
          },
          _ = token.cancelled() => Err(PlanExecuteError::Cancelled {
            reason: "cancellation token signalled".to_string(),
          }),
        }
      }
      (Some(remaining), None) => match tokio::time::timeout(remaining, tool_call).await {
        Ok(result) => tool_call_outcome(result),
        Err(_) => Err(PlanExecuteError::Timeout {
          timeout_ms: timeout_ms.unwrap_or_default(),
        }),
      },
      (None, Some(token)) => {
        tokio::select! {
          result = tool_call => tool_call_outcome(result),
          _ = token.cancelled() => Err(PlanExecuteError::Cancelled {
            reason: "cancellation token signalled".to_string(),
          }),
        }
      }
      (None, None) => tool_call_outcome(tool_call.await),
    }
  }

  fn apply_context(&mut self, context: &AgentContext) {
    self.session_id = context.session_id.clone();
    if !context.model.trim().is_empty() {
      self.config.model = context.model.clone();
      // P10.3.3-FU1: rebuild the per-message counter to match
      // the run's actual model. The budget enforcement uses the
      // resulting `token_count` directly.
      self.message_counter = crate::token_counter_adapter::build_message_counter(&context.model);
    }
    if let Some(persona) = &context.persona {
      self.config.persona = Some(persona.clone());
    }
  }

  fn system_prompt(&self) -> String {
    let mut prompt = String::from(
      "You are a Plan-and-Execute agent. Return only JSON with keys `plan` and optional `final_answer`. Each plan item must include `id`, `description`, optional `tool`, and optional `params`. Use only available tools.\n\nAvailable tools:\n",
    );
    prompt.push_str(&self.tools.prompt_tools_description());
    if let Some(persona) = &self.config.persona {
      prompt.push_str("\n\nPersona:\n");
      prompt.push_str(persona);
    }
    prompt
  }

  /// Build a `Vec<ToolSpec>` from the registered tools so it can be passed
  /// to the planner LLM as a native `tools` field. Returns an empty vector
  /// when no tools are registered, leaving the LLM call unchanged.
  fn collect_tool_specs(&self) -> Vec<ToolSpec> {
    self
      .tools
      .list()
      .into_iter()
      .map(|tool| ToolSpec::new(tool.name(), tool.description(), tool.parameters_schema()))
      .collect()
  }

  async fn add_memory_message(&mut self, message: Message) -> Result<(), PlanExecuteError> {
    let context = MemoryHookContext {
      session_id: self.session_id.clone(),
      kind: MemoryHookKind::Write,
      query: None,
      limit: None,
      messages: vec![message.clone()],
    };
    self.memory.add_message(message).await?;
    if let Some(hook) = &self.memory_hook {
      hook.on_memory_write(&context);
    }
    Ok(())
  }

  async fn read_memory_history(&self, limit: usize) -> Result<Vec<Message>, PlanExecuteError> {
    let messages = self.memory.get_history(&self.session_id, limit).await?;
    if let Some(hook) = &self.memory_hook {
      hook.on_memory_read(&MemoryHookContext {
        session_id: self.session_id.clone(),
        kind: MemoryHookKind::ReadHistory,
        query: None,
        limit: Some(limit),
        messages: messages.clone(),
      });
    }
    Ok(messages)
  }

  fn stopped_result(
    &self,
    answer: Option<String>,
    stop_reason: AgentStopReason,
    steps: Vec<AgentStep>,
    mut events: Vec<AgentEvent>,
  ) -> AgentRunResult {
    events.push(AgentEvent::RunStopped {
      session_id: self.session_id.clone(),
      reason: stop_reason.clone(),
      timestamp: Utc::now(),
    });
    AgentRunResult {
      session_id: self.session_id.clone(),
      answer,
      stop_reason,
      steps,
      events,
    }
  }

  fn cancelled_result(
    &self,
    reason: impl Into<String>,
    steps: Vec<AgentStep>,
    events: Vec<AgentEvent>,
  ) -> AgentRunResult {
    self.stopped_result(
      None,
      AgentStopReason::Cancelled {
        message: reason.into(),
      },
      steps,
      events,
    )
  }

  fn timeout_result(
    &self,
    timeout_ms: Option<u64>,
    steps: Vec<AgentStep>,
    events: Vec<AgentEvent>,
  ) -> AgentRunResult {
    self.stopped_result(
      None,
      AgentStopReason::Timeout {
        timeout_ms: timeout_ms.unwrap_or_default(),
      },
      steps,
      events,
    )
  }
}

#[async_trait]
impl AgentRuntime for PlanExecuteAgent {
  async fn run(&mut self, context: AgentContext) -> Result<AgentRunResult, AgentRuntimeError> {
    self
      .run_with_context(context)
      .await
      .map_err(|err| AgentRuntimeError::ExecutionFailed {
        message: err.to_string(),
      })
  }

  fn runtime_name(&self) -> &'static str {
    "plan_execute"
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

/// Convert a list of native tool calls into a `PlanExecutePlan`. Each call
/// becomes a sequential step with empty `description` and the tool's
/// arguments as `params`. Used when the planner emits provider-native tool
/// calls instead of a JSON plan envelope.
fn plan_from_tool_calls(calls: &[ToolCallRequest]) -> PlanExecutePlan {
  let plan = calls
    .iter()
    .enumerate()
    .map(|(idx, call)| PlanExecuteStep {
      id: format!("{}", idx + 1),
      description: String::new(),
      tool: Some(call.name.clone()),
      params: call.arguments.clone(),
      depends_on: Vec::new(),
    })
    .collect();
  PlanExecutePlan {
    plan,
    final_answer: None,
  }
}

/// Read a node output `FlowValue` as its display string. `ToolCallNode` emits
/// its result as `FlowValue::Json(Value::String(...))`; other shapes fall back
/// to their JSON / path / url text.
fn flow_value_to_string(value: &FlowValue) -> String {
  match value {
    FlowValue::Json(Value::String(s)) => s.clone(),
    FlowValue::Json(other) => other.to_string(),
    FlowValue::File { path, .. } => path.display().to_string(),
    FlowValue::Url { url, .. } => url.clone(),
  }
}

fn parse_plan(raw: &str) -> Result<PlanExecutePlan, PlanExecuteError> {
  serde_json::from_str(raw).map_err(|err| PlanExecuteError::PlanParse {
    message: err.to_string(),
  })
}

fn timed_out(started_at: Instant, timeout_ms: Option<u64>) -> bool {
  timeout_ms
    .map(Duration::from_millis)
    .is_some_and(|timeout| started_at.elapsed() >= timeout)
}

/// Fold an ordinary tool failure into an error `ToolOutput` the plan can
/// react to, but keep `PolicyDeniedAndStop` as a run-ending error.
fn tool_call_outcome(
  result: Result<yanshi_tool::ToolOutput, yanshi_tool::ToolError>,
) -> Result<yanshi_tool::ToolOutput, PlanExecuteError> {
  match result {
    Ok(output) => Ok(output),
    Err(yanshi_tool::ToolError::PolicyDeniedAndStop { message }) => {
      Err(PlanExecuteError::ApprovalDenied { message })
    }
    Err(err) => Ok(yanshi_tool::ToolOutput::error(err.to_string())),
  }
}

fn remaining_timeout(started_at: Instant, timeout_ms: Option<u64>) -> Option<Duration> {
  timeout_ms
    .map(Duration::from_millis)
    .map(|timeout| timeout.saturating_sub(started_at.elapsed()))
}

fn is_cancelled(token: &Option<AgentCancellationToken>) -> bool {
  token
    .as_ref()
    .is_some_and(AgentCancellationToken::is_cancelled)
}

fn tool_event_metadata(metadata: Option<&ToolMetadata>) -> (Option<String>, Vec<String>) {
  match metadata {
    Some(metadata) => (
      Some(metadata.source.as_str().to_string()),
      metadata
        .permissions
        .permissions
        .iter()
        .map(|permission| permission.as_str().to_string())
        .collect(),
    ),
    None => (None, Vec::new()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::runtime::RuntimeLimits;
  use serde_json::json;
  use std::sync::Mutex;
  use yanshi_agent_spi::checkpoint::AgentLoopCheckpointer as _;
  use yanshi_memory::SessionMemory;
  use yanshi_tool::{Tool, ToolError, ToolOutput};

  struct EchoTool;

  #[async_trait]
  impl Tool for EchoTool {
    fn name(&self) -> &str {
      "echo"
    }

    fn description(&self) -> &str {
      "Echo text"
    }

    fn parameters_schema(&self) -> Value {
      json!({
        "type": "object",
        "properties": {
          "text": {"type": "string"}
        },
        "required": ["text"]
      })
    }

    async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
      Ok(ToolOutput::success(format!(
        "echo: {}",
        params["text"].as_str().unwrap_or_default()
      )))
    }
  }

  #[test]
  fn parse_plan_accepts_tool_steps() {
    let plan = parse_plan(
      r#"{"plan":[{"id":"1","description":"echo it","tool":"echo","params":{"text":"hi"}}]}"#,
    )
    .unwrap();

    assert_eq!(plan.plan.len(), 1);
    assert_eq!(plan.plan[0].tool.as_deref(), Some("echo"));
  }

  #[tokio::test]
  async fn run_executes_planned_tool_and_returns_trace() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    init_mock_model(
      "mock-plan-execute-test",
      r#"{"plan":[{"id":"1","description":"echo input","tool":"echo","params":{"text":"hi"}}]}"#,
    )
    .await;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-plan-execute-test"),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    );

    let result = agent
      .run_with_context(AgentContext::new(
        "plan-execute-session",
        "say hi",
        "mock-plan-execute-test",
      ))
      .await
      .unwrap();

    assert_eq!(result.answer.as_deref(), Some("echo: hi"));
    assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
    assert!(
      result
        .steps
        .iter()
        .any(|step| matches!(step.kind, AgentStepKind::ToolCall { .. }))
    );
    assert!(
      result
        .events
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolCallCompleted { .. }))
    );
  }

  struct DenyAndStopTool {
    calls: Arc<Mutex<usize>>,
  }

  #[async_trait]
  impl Tool for DenyAndStopTool {
    fn name(&self) -> &str {
      "guarded"
    }

    fn description(&self) -> &str {
      "Always denied with DenyAndStop"
    }

    fn parameters_schema(&self) -> Value {
      json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _params: Value) -> Result<ToolOutput, ToolError> {
      *self.calls.lock().unwrap() += 1;
      Err(ToolError::PolicyDeniedAndStop {
        message: "operator chose deny_and_stop".to_string(),
      })
    }
  }

  /// Eval 2026-10-07 P0-5: PlanExecute used to fold `PolicyDeniedAndStop`
  /// into an error output and run the remaining plan steps anyway.
  #[tokio::test]
  async fn deny_and_stop_ends_the_run_instead_of_continuing_the_plan() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    init_mock_model(
      "mock-plan-execute-deny-stop",
      r#"{"plan":[{"id":"1","description":"first","tool":"guarded","params":{}},{"id":"2","description":"second","tool":"guarded","params":{}}]}"#,
    )
    .await;

    let calls = Arc::new(Mutex::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(DenyAndStopTool {
      calls: calls.clone(),
    }));
    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-plan-execute-deny-stop"),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    );

    let result = agent
      .run_with_context(AgentContext::new(
        "plan-execute-deny-stop",
        "do it",
        "mock-plan-execute-deny-stop",
      ))
      .await
      .unwrap();

    assert_eq!(
      result.stop_reason,
      AgentStopReason::ApprovalDenied {
        message: "operator chose deny_and_stop".to_string()
      }
    );
    assert_eq!(
      *calls.lock().unwrap(),
      1,
      "the second plan step must not run"
    );
  }

  // ── V2.1: output_schema ─────────────────────────────────────────────

  fn answer_schema() -> Value {
    serde_json::json!({
      "type": "object",
      "properties": {"answer": {"type": "string"}},
      "required": ["answer"]
    })
  }

  /// The V2.1 test bar applied to `PlanExecuteAgent`: a schema mismatch on
  /// the first attempt's `final_answer` retries the whole plan-and-execute
  /// cycle (not just the answer — there's no mid-loop retry point here),
  /// and the run eventually succeeds once the model self-corrects.
  #[tokio::test]
  async fn run_with_context_output_schema_mismatch_retries_whole_cycle_and_succeeds() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-pe-schema-correct-{}", uuid::Uuid::new_v4());
    // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
    unsafe {
      std::env::set_var(
        "YANSHI_MOCK_RESPONSES",
        serde_json::to_string(&vec![
          // Attempt 1: `final_answer` violates the schema (missing `answer`).
          r#"{"plan":[],"final_answer":"{\"wrong_field\":1}"}"#,
          // Attempt 2: conforms.
          r#"{"plan":[],"final_answer":"{\"answer\":\"42\"}"}"#,
        ])
        .unwrap(),
      );
    }
    init_mock_model(
      &model,
      "unused — YANSHI_MOCK_RESPONSES queue drives this test",
    )
    .await;

    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new(&model).with_output_schema(answer_schema()),
      Box::new(SessionMemory::default_window()),
      Arc::new(ToolRegistry::new()),
    );

    let result = agent
      .run_with_context(AgentContext::new(
        "pe-schema-correct-session",
        "answer please",
        &model,
      ))
      .await
      .unwrap();

    assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
    assert_eq!(result.answer.as_deref(), Some(r#"{"answer":"42"}"#));

    // SAFETY: cleanup of the dedicated mock env var after the test read.
    unsafe {
      std::env::remove_var("YANSHI_MOCK_RESPONSES");
    }
  }

  /// When the model never produces a conforming `final_answer` within the
  /// correction budget, the run hard-errors rather than returning a
  /// non-conformant answer labelled as final.
  #[tokio::test]
  async fn run_with_context_output_schema_exhausted_attempts_returns_hard_error() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-pe-schema-exhaust-{}", uuid::Uuid::new_v4());
    // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
    unsafe {
      std::env::set_var(
        "YANSHI_MOCK_RESPONSES",
        serde_json::to_string(&vec![
          r#"{"plan":[],"final_answer":"{}"}"#,
          r#"{"plan":[],"final_answer":"{}"}"#,
          r#"{"plan":[],"final_answer":"{}"}"#,
        ])
        .unwrap(),
      );
    }
    init_mock_model(
      &model,
      "unused — YANSHI_MOCK_RESPONSES queue drives this test",
    )
    .await;

    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new(&model)
        .with_output_schema(answer_schema())
        .with_max_schema_correction_attempts(2),
      Box::new(SessionMemory::default_window()),
      Arc::new(ToolRegistry::new()),
    );

    let err = agent
      .run_with_context(AgentContext::new(
        "pe-schema-exhaust-session",
        "answer please",
        &model,
      ))
      .await
      .expect_err("schema-exhausted run must hard-error");
    match err {
      PlanExecuteError::SchemaValidationFailed { attempts, .. } => assert_eq!(attempts, 3),
      other => panic!("expected SchemaValidationFailed, got {other:?}"),
    }

    // SAFETY: cleanup of the dedicated mock env var after the test read.
    unsafe {
      std::env::remove_var("YANSHI_MOCK_RESPONSES");
    }
  }

  /// Without `output_schema`, behaviour is byte-identical to before V2.1 —
  /// a single planner call, no retry machinery engaged.
  #[tokio::test]
  async fn run_with_context_without_output_schema_is_unaffected() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    init_mock_model(
      "mock-pe-no-schema",
      r#"{"plan":[],"final_answer":"whatever, not JSON-schema-checked"}"#,
    )
    .await;

    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-pe-no-schema"),
      Box::new(SessionMemory::default_window()),
      Arc::new(ToolRegistry::new()),
    );

    let result = agent
      .run_with_context(AgentContext::new(
        "pe-no-schema-session",
        "answer please",
        "mock-pe-no-schema",
      ))
      .await
      .unwrap();

    assert_eq!(
      result.answer.as_deref(),
      Some("whatever, not JSON-schema-checked")
    );
    assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  }

  // ── T1.1: production cost-limit enforcement ───────────────────────────

  /// $1.00 flat per planner call: the mock provider always reports
  /// `prompt_tokens: 50`, so pricing entirely off `input_per_1k` (with
  /// `output_per_1k: 0.0`) makes the call's cost independent of the
  /// response word count.
  fn flat_dollar_per_call_pricing() -> crate::eval::PricingTable {
    crate::eval::PricingTable::default().with_default(crate::eval::ModelPricing {
      input_per_1k: 20.0,
      output_per_1k: 0.0,
    })
  }

  #[tokio::test]
  async fn run_with_context_stops_with_cost_limit_exceeded_before_executing_plan() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    init_mock_model(
      "mock-pe-cost-limit",
      r#"{"plan":[{"id":"1","description":"echo input","tool":"echo","params":{"text":"hi"}}]}"#,
    )
    .await;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-pe-cost-limit")
        .with_pricing_table(flat_dollar_per_call_pricing()),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    );

    let limits = RuntimeLimits {
      // The single planner call costs $1.00; a $0.50 budget is blown
      // immediately (no tool runs, no plan executed).
      cost_limit_usd: Some(0.5),
      ..Default::default()
    };
    let result = agent
      .run_with_context(
        AgentContext::new("pe-cost-session", "say hi", "mock-pe-cost-limit").with_limits(limits),
      )
      .await
      .unwrap();

    match result.stop_reason {
      AgentStopReason::CostLimitExceeded {
        used_usd,
        budget_usd,
      } => {
        assert_eq!(budget_usd, 0.5);
        assert!(
          (used_usd - 1.0).abs() < 1e-9,
          "expected $1.00, got {used_usd}"
        );
      }
      other => panic!("expected CostLimitExceeded, got {other:?}"),
    }
    assert!(
      !result
        .steps
        .iter()
        .any(|step| matches!(step.kind, AgentStepKind::ToolCall { .. })),
      "plan must not execute once the planner call alone exceeds budget"
    );
  }

  #[tokio::test]
  async fn run_with_context_cost_limit_does_not_interrupt_a_run_within_budget() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    init_mock_model(
      "mock-pe-cost-ok",
      r#"{"plan":[{"id":"1","description":"echo input","tool":"echo","params":{"text":"hi"}}]}"#,
    )
    .await;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-pe-cost-ok").with_pricing_table(flat_dollar_per_call_pricing()),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    );

    let limits = RuntimeLimits {
      // A single $1.00 call comfortably fits a $100 budget.
      cost_limit_usd: Some(100.0),
      ..Default::default()
    };
    let result = agent
      .run_with_context(
        AgentContext::new("pe-cost-ok-session", "say hi", "mock-pe-cost-ok").with_limits(limits),
      )
      .await
      .unwrap();

    assert_eq!(result.answer.as_deref(), Some("echo: hi"));
    assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
  }

  #[tokio::test]
  async fn run_as_flow_plans_compiles_and_executes() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    init_mock_model(
      "mock-pe-flow",
      r#"{"plan":[{"id":"1","description":"echo input","tool":"echo","params":{"text":"hi"}}]}"#,
    )
    .await;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-pe-flow"),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    );

    let result = agent
      .run_as_flow(
        AgentContext::new("pe-flow-session", "say hi", "mock-pe-flow"),
        Arc::new(yanshi_core::CoreFlowRunner::concurrent(4)),
      )
      .await
      .unwrap();

    // The plan ran on the Flow engine and produced the tool's output as answer.
    assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
    assert_eq!(result.answer.as_deref(), Some("echo: hi"));
    // The trace carries the planned tool call + its (non-error) result + answer.
    assert!(
      result
        .steps
        .iter()
        .any(|step| matches!(step.kind, AgentStepKind::ToolCall { .. }))
    );
    assert!(result.steps.iter().any(|step| matches!(
      &step.kind,
      AgentStepKind::ToolResult {
        is_error: false,
        ..
      }
    )));
    assert!(
      result
        .steps
        .iter()
        .any(|step| matches!(step.kind, AgentStepKind::FinalAnswer { .. }))
    );
  }

  #[tokio::test]
  async fn run_consumes_native_tool_calls_when_available() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = "mock-plan-execute-native";
    // SAFETY: LLM_TEST_LOCK serializes mutation of process-wide mock env vars.
    //
    // Drives Plan-Execute through the native tool-calling path. The text
    // content is unparseable JSON, so a successful run proves the plan
    // came from `tool_calls` rather than `parse_plan`.
    unsafe {
      std::env::set_var(
        "YANSHI_MOCK_TOOL_CALLS",
        serde_json::to_string(&vec![vec![serde_json::json!({
          "id": "call_0",
          "name": "echo",
          "arguments": {"text": "hi"}
        })]])
        .unwrap(),
      );
    }
    init_mock_model(model, "(unused — native tool call)").await;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new(model),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    );

    let result = agent
      .run_with_context(AgentContext::new("plan-execute-native", "say hi", model))
      .await
      .unwrap();

    assert_eq!(result.answer.as_deref(), Some("echo: hi"));
    assert_eq!(result.stop_reason, AgentStopReason::FinalAnswer);
    assert!(
      result
        .steps
        .iter()
        .any(|step| matches!(step.kind, AgentStepKind::ToolCall { .. }))
    );

    // SAFETY: cleanup of the dedicated mock env var after the test read.
    unsafe {
      std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    }
  }

  #[test]
  fn plan_from_tool_calls_maps_each_call_to_a_step() {
    let calls = vec![
      ToolCallRequest {
        id: "call_0".into(),
        name: "echo".into(),
        arguments: serde_json::json!({"text": "a"}),
      },
      ToolCallRequest {
        id: "call_1".into(),
        name: "shell".into(),
        arguments: serde_json::json!({"command": "ls"}),
      },
    ];
    let plan = plan_from_tool_calls(&calls);
    assert_eq!(plan.plan.len(), 2);
    assert_eq!(plan.plan[0].tool.as_deref(), Some("echo"));
    assert_eq!(plan.plan[0].params["text"], "a");
    assert_eq!(plan.plan[1].tool.as_deref(), Some("shell"));
    assert!(plan.final_answer.is_none());
  }

  #[tokio::test]
  async fn run_returns_cancelled_when_token_already_signalled() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    init_mock_model(
      "mock-plan-execute-test",
      r#"{"plan":[{"id":"1","description":"echo input","tool":"echo","params":{"text":"hi"}}]}"#,
    )
    .await;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-plan-execute-test"),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    );
    let token = AgentCancellationToken::new();
    token.cancel();

    let result = agent
      .run_with_context(
        AgentContext::new("plan-cancelled", "say hi", "mock-plan-execute-test")
          .with_cancellation_token(token),
      )
      .await
      .unwrap();

    assert!(matches!(
      result.stop_reason,
      AgentStopReason::Cancelled { .. }
    ));
  }

  // ── V2.4: agent-loop checkpoint ──────────────────────────────────────

  /// Test-double `AgentLoopCheckpointer`, mirroring the one in
  /// `react::agent`'s test module (not shared across files — each is
  /// small and self-contained). `cancel_after` simulates "the process
  /// died mid-loop" by firing a cancellation token once `save` has been
  /// called that many times, which the next loop iteration's
  /// `is_cancelled` check picks up before touching the next plan step.
  #[derive(Clone)]
  struct RecordingCheckpointer {
    store: Arc<
      Mutex<std::collections::HashMap<String, yanshi_agent_spi::checkpoint::AgentLoopCheckpoint>>,
    >,
    saves: Arc<std::sync::atomic::AtomicUsize>,
    cancel_after: Option<(usize, AgentCancellationToken)>,
  }

  impl RecordingCheckpointer {
    fn new() -> Self {
      Self {
        store: Arc::new(Mutex::new(std::collections::HashMap::new())),
        saves: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        cancel_after: None,
      }
    }

    fn with_cancel_after(mut self, count: usize, token: AgentCancellationToken) -> Self {
      self.cancel_after = Some((count, token));
      self
    }
  }

  #[async_trait]
  impl yanshi_agent_spi::checkpoint::AgentLoopCheckpointer for RecordingCheckpointer {
    async fn save(
      &self,
      checkpoint: &yanshi_agent_spi::checkpoint::AgentLoopCheckpoint,
    ) -> Result<(), yanshi_agent_spi::checkpoint::AgentLoopCheckpointError> {
      self
        .store
        .lock()
        .unwrap()
        .insert(checkpoint.session_id.clone(), checkpoint.clone());
      let count = self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
      if let Some((target, token)) = &self.cancel_after
        && count >= *target
      {
        token.cancel();
      }
      Ok(())
    }

    async fn load(
      &self,
      session_id: &str,
    ) -> Result<
      Option<yanshi_agent_spi::checkpoint::AgentLoopCheckpoint>,
      yanshi_agent_spi::checkpoint::AgentLoopCheckpointError,
    > {
      Ok(self.store.lock().unwrap().get(session_id).cloned())
    }

    async fn clear(
      &self,
      session_id: &str,
    ) -> Result<(), yanshi_agent_spi::checkpoint::AgentLoopCheckpointError> {
      self.store.lock().unwrap().remove(session_id);
      Ok(())
    }
  }

  /// V2.4 acceptance scenario for `PlanExecuteAgent`: a 3-step plan is
  /// interrupted (simulated process death via deterministic cancellation
  /// timed off the 2nd checkpoint save) after 2 tool steps; resuming with
  /// a brand-new `PlanExecuteAgent` instance — no further planner call
  /// needed at all, the checkpoint carries the frozen plan — executes
  /// only the 1 remaining step and reaches the same final answer an
  /// uninterrupted control run produces.
  #[tokio::test]
  async fn resume_from_loop_checkpoint_continues_after_interrupted_plan_execution() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;

    let plan_json = r#"{"plan":[
      {"id":"1","description":"step one","tool":"echo","params":{"text":"a"}},
      {"id":"2","description":"step two","tool":"echo","params":{"text":"b"}},
      {"id":"3","description":"step three","tool":"echo","params":{"text":"c"}}
    ]}"#;

    // ── Control run: uninterrupted. ──
    init_mock_model("mock-plan-ckpt-control", plan_json).await;
    let mut control_registry = ToolRegistry::new();
    control_registry.register(Arc::new(EchoTool));
    let mut control_agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-plan-ckpt-control"),
      Box::new(SessionMemory::default_window()),
      Arc::new(control_registry),
    );
    let control_result = control_agent
      .run_with_context(AgentContext::new(
        "plan-ckpt-session",
        "do the three-step task",
        "mock-plan-ckpt-control",
      ))
      .await
      .unwrap();
    assert_eq!(
      control_result.answer.as_deref(),
      Some("echo: a\necho: b\necho: c")
    );
    assert_eq!(control_result.stop_reason, AgentStopReason::FinalAnswer);

    // ── Interrupted run: cancel right after the 2nd checkpoint save (2
    // tool steps completed, 1 remaining). ──
    init_mock_model("mock-plan-ckpt-interrupted", plan_json).await;
    let mut interrupted_registry = ToolRegistry::new();
    interrupted_registry.register(Arc::new(EchoTool));
    // A fresh, independent memory store is fine here (unlike ReActAgent's
    // resume, PlanExecuteAgent's resume never reads history back — see
    // `resume_from_loop_checkpoint`'s doc comment).
    let mut interrupted_agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-plan-ckpt-interrupted"),
      Box::new(SessionMemory::default_window()),
      Arc::new(interrupted_registry),
    );

    let cancel_token = AgentCancellationToken::new();
    let checkpointer = RecordingCheckpointer::new().with_cancel_after(2, cancel_token.clone());
    let checkpointer_handle: Arc<dyn yanshi_agent_spi::checkpoint::AgentLoopCheckpointer> =
      Arc::new(checkpointer.clone());

    let interrupted_context = AgentContext::new(
      "plan-ckpt-session",
      "do the three-step task",
      "mock-plan-ckpt-interrupted",
    )
    .with_cancellation_token(cancel_token)
    .with_loop_checkpointer(checkpointer_handle.clone());
    let interrupted_result = interrupted_agent
      .run_with_context(interrupted_context)
      .await
      .unwrap();
    assert!(
      matches!(
        interrupted_result.stop_reason,
        AgentStopReason::Cancelled { .. }
      ),
      "expected Cancelled, got {:?}",
      interrupted_result.stop_reason
    );
    assert_eq!(
      checkpointer.saves.load(std::sync::atomic::Ordering::SeqCst),
      2
    );

    let checkpoint = checkpointer
      .load("plan-ckpt-session")
      .await
      .unwrap()
      .expect("a checkpoint must have been saved before cancellation");
    assert_eq!(checkpoint.tool_calls, 2);
    assert_eq!(checkpoint.plan_position, 2);

    // ── Resume: a brand-new PlanExecuteAgent instance, no planner call
    // needed — the checkpoint carries the frozen plan. If resume
    // incorrectly re-planned or restarted from step 0, the answer would
    // include duplicated or missing observations instead of matching the
    // control run exactly. ──
    let mut resume_registry = ToolRegistry::new();
    resume_registry.register(Arc::new(EchoTool));
    let mut resumed_agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-plan-ckpt-resume-unused"),
      Box::new(SessionMemory::default_window()),
      Arc::new(resume_registry),
    );
    let resume_context = AgentContext::new("plan-ckpt-session", "", "mock-plan-ckpt-resume-unused")
      .with_loop_checkpointer(checkpointer_handle);
    let resumed_result = resumed_agent
      .resume_from_loop_checkpoint(resume_context, checkpoint.clone(), None)
      .await
      .unwrap();

    assert_eq!(resumed_result.answer, control_result.answer);
    assert_eq!(resumed_result.stop_reason, AgentStopReason::FinalAnswer);
    // Continuity: the resumed result's steps carry the checkpoint's
    // history forward rather than starting a fresh run at step 0.
    assert!(resumed_result.steps.len() > checkpoint.steps.len());
    assert_eq!(
      resumed_result.steps[..checkpoint.steps.len()],
      checkpoint.steps[..]
    );
    // Successful completion clears the checkpoint.
    assert_eq!(checkpointer.load("plan-ckpt-session").await.unwrap(), None);
  }

  // ── V2.3: ask_user / HITL interrupt-resume ───────────────────────────

  /// V2.3 acceptance scenario for `PlanExecuteAgent`: a plan containing
  /// an `ask_user` step pauses with `AwaitingInput` at the unadvanced
  /// step; resuming with a fresh instance + an answer needs no further
  /// planner call at all (the plan was already frozen into the
  /// checkpoint) and completes the remaining step.
  #[tokio::test]
  async fn resume_from_loop_checkpoint_continues_after_ask_user_step_with_answer() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;

    let plan_json = r#"{"plan":[
      {"id":"1","description":"ask","tool":"ask_user","params":{"question":"what's the deploy target?"}},
      {"id":"2","description":"step two","tool":"echo","params":{"text":"b"}}
    ]}"#;

    init_mock_model("mock-plan-ask-user-interrupted", plan_json).await;
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-plan-ask-user-interrupted"),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    );
    let checkpointer = RecordingCheckpointer::new();
    let checkpointer_handle: Arc<dyn yanshi_agent_spi::checkpoint::AgentLoopCheckpointer> =
      Arc::new(checkpointer.clone());
    let result = agent
      .run_with_context(
        AgentContext::new(
          "plan-ask-user-session",
          "deploy the app",
          "mock-plan-ask-user-interrupted",
        )
        .with_loop_checkpointer(checkpointer_handle.clone()),
      )
      .await
      .unwrap();
    assert_eq!(
      result.stop_reason,
      AgentStopReason::AwaitingInput {
        question: "what's the deploy target?".to_string()
      }
    );
    assert!(
      result
        .steps
        .iter()
        .any(|s| matches!(&s.kind, AgentStepKind::ToolCall { tool, .. } if tool == crate::react::agent::ASK_USER_TOOL_NAME))
    );

    let checkpoint = checkpointer
      .load("plan-ask-user-session")
      .await
      .unwrap()
      .expect("a checkpoint must have been saved when the loop paused");
    assert_eq!(
      checkpoint.pending_question.as_deref(),
      Some("what's the deploy target?")
    );
    // Unadvanced — the ask_user step is not yet complete.
    assert_eq!(checkpoint.plan_position, 0);

    // ── Resume: a brand-new PlanExecuteAgent instance, no planner call
    // needed — the checkpoint carries the frozen plan. ──
    let mut resume_registry = ToolRegistry::new();
    resume_registry.register(Arc::new(EchoTool));
    let mut resumed_agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-plan-ask-user-resume-unused"),
      Box::new(SessionMemory::default_window()),
      Arc::new(resume_registry),
    );
    let resume_context = AgentContext::new(
      "plan-ask-user-session",
      "",
      "mock-plan-ask-user-resume-unused",
    )
    .with_loop_checkpointer(checkpointer_handle);
    let resumed_result = resumed_agent
      .resume_from_loop_checkpoint(resume_context, checkpoint, Some("staging".to_string()))
      .await
      .unwrap();

    assert_eq!(resumed_result.stop_reason, AgentStopReason::FinalAnswer);
    let answer = resumed_result.answer.expect("must have an answer");
    assert!(answer.contains("what's the deploy target?: staging"));
    assert!(answer.contains("echo: b"));
    assert!(
      resumed_result
        .steps
        .iter()
        .any(|s| matches!(&s.kind, AgentStepKind::ToolResult { tool, content, .. } if tool == crate::react::agent::ASK_USER_TOOL_NAME && content == "staging"))
    );
    // Successful completion clears the checkpoint.
    assert_eq!(
      checkpointer.load("plan-ask-user-session").await.unwrap(),
      None
    );
  }

  #[tokio::test]
  async fn resume_from_loop_checkpoint_rejects_answer_without_pending_question() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    init_mock_model("mock-plan-ask-user-validate", "{}").await;
    let mut agent = PlanExecuteAgent::new(
      PlanExecuteConfig::new("mock-plan-ask-user-validate"),
      Box::new(SessionMemory::default_window()),
      Arc::new(ToolRegistry::new()),
    );
    let checkpoint = yanshi_agent_spi::checkpoint::AgentLoopCheckpoint {
      schema_version: yanshi_agent_spi::checkpoint::AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION,
      session_id: "s".into(),
      runtime_kind: yanshi_agent_spi::checkpoint::LoopRuntimeKind::PlanExecute,
      created_at: chrono::Utc::now(),
      steps: vec![],
      events: vec![],
      step_index: 1,
      iteration: 0,
      tool_calls: 0,
      verification_attempts: 0,
      schema_correction_attempts: 0,
      last_tool_call: None,
      recent_tool_calls: std::collections::VecDeque::new(),
      cumulative_cost_usd: 0.0,
      system_prompt: String::new(),
      user_input: "hello".into(),
      trace_context: None,
      plan_steps: serde_json::json!({"plan": [], "final_answer": null}),
      plan_position: 0,
      observations: vec![],
      pending_question: None,
    };
    let err = agent
      .resume_from_loop_checkpoint(
        AgentContext::new("s", "", "mock-plan-ask-user-validate"),
        checkpoint,
        Some("unsolicited".to_string()),
      )
      .await
      .unwrap_err();
    assert!(matches!(err, PlanExecuteError::InvalidCheckpoint { .. }));
  }

  async fn init_mock_model(model: &str, response: &str) {
    // SAFETY: tests serialize LLM config/env mutation with LLM_TEST_LOCK.
    unsafe {
      std::env::set_var("YANSHI_MOCK_RESPONSE", response);
    }

    let config_path =
      std::env::temp_dir().join(format!("yanshi-plan-execute-{}.yml", uuid::Uuid::new_v4()));
    std::fs::write(
      &config_path,
      format!(
        r#"
models:
  {model}:
    vendor: mock
    type: text
    model_id: {model}
providers:
  mock:
    api_key_env: MOCK_API_KEY
"#
      ),
    )
    .unwrap();

    Yanshi::init_with_config(config_path.to_str().unwrap())
      .await
      .unwrap();
  }

  // ── P-A2.2: emit a Flow (compile_plan_to_flow bridge) ──────────────────────

  fn agent_with_echo() -> PlanExecuteAgent {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    PlanExecuteAgent::new(
      PlanExecuteConfig::new("m"),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    )
  }

  fn tool_step(id: &str, deps: Vec<&str>) -> PlanExecuteStep {
    PlanExecuteStep {
      id: id.into(),
      description: "d".into(),
      tool: Some("echo".into()),
      params: json!({}),
      depends_on: deps.into_iter().map(String::from).collect(),
    }
  }

  #[test]
  fn compile_plan_to_flow_chains_steps_sequentially_by_default() {
    let agent = agent_with_echo();
    let steps = vec![
      tool_step("s1", vec![]),
      tool_step("s2", vec![]),
      tool_step("s3", vec![]),
    ];
    let flow = agent.compile_plan_to_flow(&steps).expect("compiles");
    assert_eq!(flow.nodes().len(), 3);
    // Empty depends_on chains each step after the previous one.
    assert!(flow.nodes().get("s1").unwrap().dependencies.is_empty());
    assert_eq!(
      flow.nodes().get("s2").unwrap().dependencies,
      vec!["s1".to_string()]
    );
    assert_eq!(
      flow.nodes().get("s3").unwrap().dependencies,
      vec!["s2".to_string()]
    );
  }

  #[test]
  fn compile_plan_to_flow_honors_explicit_depends_on() {
    let agent = agent_with_echo();
    // s3 explicitly depends on s1 (not the previous s2) — a parallel branch.
    let steps = vec![
      tool_step("s1", vec![]),
      tool_step("s2", vec![]),
      tool_step("s3", vec!["s1"]),
    ];
    let flow = agent.compile_plan_to_flow(&steps).expect("compiles");
    assert_eq!(
      flow.nodes().get("s3").unwrap().dependencies,
      vec!["s1".to_string()]
    );
  }

  #[test]
  fn compile_plan_to_flow_skips_reasoning_steps() {
    let agent = agent_with_echo();
    let steps = vec![
      tool_step("s1", vec![]),
      PlanExecuteStep {
        id: "think".into(),
        description: "reason".into(),
        tool: None,
        params: json!({}),
        depends_on: vec![],
      },
      tool_step("s2", vec![]),
    ];
    let flow = agent.compile_plan_to_flow(&steps).expect("compiles");
    // The reasoning step has no node; s2 chains after the previous *tool* step.
    assert_eq!(flow.nodes().len(), 2);
    assert!(flow.nodes().contains_key("s1"));
    assert!(flow.nodes().contains_key("s2"));
    assert_eq!(
      flow.nodes().get("s2").unwrap().dependencies,
      vec!["s1".to_string()]
    );
  }
}
