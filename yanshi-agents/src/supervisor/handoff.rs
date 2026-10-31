//! Handoff multi-agent collaboration: each participant decides when to
//! transfer control to another agent via a shared [`HandoffTool`].
//!
//! # Lifecycle
//!
//! 1. The supervisor invokes the *active* agent with the user's input.
//! 2. The agent's LLM may call `handoff(to=X, message=...)` from its tool
//!    registry. The tool registers the request in a shared [`HandoffSignal`].
//! 3. After the agent finishes its loop, the supervisor inspects the signal:
//!    - request present → switch active to `X`, use `message` as the next
//!      input, continue.
//!    - request absent → terminate with the agent's final answer.
//! 4. `max_handoffs` caps the number of transitions so a buggy LLM cannot
//!    bounce indefinitely.
//!
//! # Why a signal instead of inspecting tool calls?
//!
//! Inspecting `AgentRunResult.steps` for a `ToolCall { tool: "handoff" }` is
//! brittle: the agent might call the tool more than once, or call other tools
//! after it. A shared signal is single-source-of-truth: the *most recent*
//! handoff request "wins", and a missing signal means the agent did not hand
//! off, regardless of what other tool calls happened.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use yanshi_tool::{Tool, ToolError, ToolMetadata, ToolOutput};

use crate::runtime::{
  AgentContext, AgentEvent, AgentRunResult, AgentRuntime, AgentRuntimeError, AgentStep,
  AgentStepKind, AgentStopReason,
};

// ── Handoff signal + tool ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct HandoffRequest {
  to: String,
  message: String,
}

/// Shared state between [`HandoffTool`] instances and a [`HandoffSupervisor`].
///
/// The tool writes a request into the signal; the supervisor takes it after
/// the active agent's run completes. Cloning is cheap (an `Arc`).
#[derive(Debug, Clone, Default)]
pub struct HandoffSignal {
  pending: Arc<Mutex<Option<HandoffRequest>>>,
}

impl HandoffSignal {
  pub fn new() -> Self {
    Self::default()
  }

  fn take(&self) -> Option<HandoffRequest> {
    self.pending.lock().ok().and_then(|mut guard| guard.take())
  }

  fn set(&self, request: HandoffRequest) {
    if let Ok(mut guard) = self.pending.lock() {
      *guard = Some(request);
    }
  }
}

/// A tool registered into each participant's tool registry so the LLM can
/// transfer control by calling `handoff(to=..., message=...)`.
///
/// All instances within a single [`HandoffSupervisor`] share the same
/// [`HandoffSignal`].
pub struct HandoffTool {
  valid_targets: Vec<String>,
  signal: HandoffSignal,
}

impl HandoffTool {
  pub fn new(valid_targets: Vec<String>, signal: HandoffSignal) -> Self {
    Self {
      valid_targets,
      signal,
    }
  }

  pub fn valid_targets(&self) -> &[String] {
    &self.valid_targets
  }
}

#[async_trait]
impl Tool for HandoffTool {
  fn name(&self) -> &str {
    "handoff"
  }

  fn description(&self) -> &str {
    "Transfer control of the conversation to another specialist agent. \
     After calling this tool produce a brief closing remark; the next agent's \
     reply will become the user-visible response."
  }

  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": {
        "to": {
          "type": "string",
          "description": "Name of the next agent to take over.",
          "enum": self.valid_targets,
        },
        "message": {
          "type": "string",
          "description": "Context to pass to the next agent."
        }
      },
      "required": ["to", "message"],
    })
  }

  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::builtin_named("handoff")
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    let to = params
      .get("to")
      .and_then(Value::as_str)
      .ok_or_else(|| ToolError::InvalidParams {
        message: "handoff: 'to' must be a string".into(),
      })?;
    let message = params
      .get("message")
      .and_then(Value::as_str)
      .ok_or_else(|| ToolError::InvalidParams {
        message: "handoff: 'message' must be a string".into(),
      })?;

    if !self.valid_targets.iter().any(|t| t == to) {
      return Err(ToolError::InvalidParams {
        message: format!(
          "handoff: unknown target '{}'. Valid targets: {:?}",
          to, self.valid_targets
        ),
      });
    }

    self.signal.set(HandoffRequest {
      to: to.to_string(),
      message: message.to_string(),
    });

    Ok(ToolOutput::success(format!(
      "Handoff to '{}' has been registered. Please provide a brief closing \
       remark; the actual response will come from '{}'.",
      to, to
    )))
  }
}

// ── Supervisor ────────────────────────────────────────────────────────────────

/// Errors that prevent a [`HandoffSupervisor`] from being constructed.
#[derive(Debug, thiserror::Error)]
pub enum HandoffSupervisorError {
  #[error("HandoffSupervisor needs at least one agent")]
  NoAgents,
  #[error("Initial agent '{0}' is not registered")]
  UnknownInitialAgent(String),
  #[error("Duplicate agent name '{0}'")]
  DuplicateAgent(String),
  #[error("delegation spec registered for unknown agent '{0}'")]
  UnknownDelegationSpecAgent(String),
}

/// A multi-agent runtime where each agent decides when to transfer control to
/// another participant.
///
/// Implements [`AgentRuntime`], so it can be embedded in [`AgentNode`] just
/// like a [`ReActAgent`] or [`PlanExecuteAgent`].
///
/// [`AgentNode`]: crate::nodes::AgentNode
/// [`PlanExecuteAgent`]: crate::PlanExecuteAgent
pub struct HandoffSupervisor {
  agents: HashMap<String, super::common::SharedAgentRuntime>,
  /// Insertion-ordered list of (name, description) for trace observability.
  agent_descriptions: Vec<(String, String)>,
  initial_agent: String,
  max_handoffs: usize,
  signal: HandoffSignal,
  session_id: String,
  /// W3.3: optional per-agent `DelegationSpec`, consulted after each turn
  /// to validate that agent's answer — see
  /// [`super::common::record_delegation_validation`].
  delegation_specs: HashMap<String, crate::delegation::DelegationSpec>,
  last_schema_validations: HashMap<String, crate::delegation::SchemaValidation>,
}

impl std::fmt::Debug for HandoffSupervisor {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("HandoffSupervisor")
      .field("session_id", &self.session_id)
      .field("initial_agent", &self.initial_agent)
      .field("max_handoffs", &self.max_handoffs)
      .field("agents", &self.agent_descriptions)
      .finish()
  }
}

impl HandoffSupervisor {
  /// Unique id for this supervisor instance, used as the root of all session
  /// ids in nested agent traces.
  pub fn session_id(&self) -> &str {
    &self.session_id
  }

  /// Names + descriptions of all registered agents, in registration order.
  pub fn agent_descriptions(&self) -> &[(String, String)] {
    &self.agent_descriptions
  }

  /// Schema-validation outcomes (W3.3) from the most recent run, keyed by
  /// agent name — only populated for agents with a
  /// [`DelegationSpec`](crate::delegation::DelegationSpec) registered via
  /// [`HandoffSupervisorBuilder::with_delegation_spec`].
  pub fn last_schema_validations(&self) -> &HashMap<String, crate::delegation::SchemaValidation> {
    &self.last_schema_validations
  }

  /// Convenience: run a one-shot task and return the final answer string.
  ///
  /// Equivalent to constructing an [`AgentContext`] with a fresh session id
  /// and calling [`AgentRuntime::run`].
  pub async fn run(&mut self, task: &str) -> Result<String, AgentRuntimeError> {
    let context = AgentContext::new(self.session_id.clone(), task, "");
    let result = AgentRuntime::run(self, context).await?;
    result
      .answer
      .ok_or_else(|| AgentRuntimeError::ExecutionFailed {
        message: format!(
          "HandoffSupervisor stopped without a final answer: {:?}",
          result.stop_reason
        ),
      })
  }
}

#[async_trait]
impl AgentRuntime for HandoffSupervisor {
  async fn run(&mut self, context: AgentContext) -> Result<AgentRunResult, AgentRuntimeError> {
    let session_id = context.session_id.clone();
    let mut steps: Vec<AgentStep> = Vec::new();
    let mut events: Vec<AgentEvent> = Vec::new();
    let mut step_index = 0usize;

    events.push(AgentEvent::RunStarted {
      session_id: session_id.clone(),
      model: format!("multi_agent:handoff(initial={})", self.initial_agent),
      timestamp: context.started_at,
    });
    steps.push(AgentStep::new(
      step_index,
      AgentStepKind::Observe {
        input: context.input.clone(),
      },
    ));
    step_index += 1;

    let mut active = self.initial_agent.clone();
    let mut current_input = context.input.clone();
    let mut handoffs_remaining = self.max_handoffs;
    let cancellation = context.cancellation_token.clone();

    // Drain any stale signal from a prior run on the same supervisor instance.
    let _ = self.signal.take();
    let mut budget = super::common::ChildBudget::start(&context.limits);

    loop {
      if cancellation.as_ref().is_some_and(|t| t.is_cancelled()) {
        return Ok(stopped(
          session_id,
          None,
          AgentStopReason::Cancelled {
            message: "cancellation token signalled".into(),
          },
          steps,
          events,
        ));
      }

      let agent_handle =
        self
          .agents
          .get(&active)
          .ok_or_else(|| AgentRuntimeError::ExecutionFailed {
            message: format!(
              "HandoffSupervisor: active agent '{}' is not registered",
              active
            ),
          })?;

      let limits = match budget.limits_for(&context.limits, 1) {
        Ok(limits) => limits,
        Err(spent) => return Ok(stopped(session_id, None, spent, steps, events)),
      };
      let child_ctx = super::common::build_child_context(&context, &active, &current_input, limits);

      let child_result = {
        let mut agent = agent_handle.lock().await;
        agent.run(child_ctx).await?
      };
      budget.charge(&child_result);

      super::common::record_delegation_validation(
        &self.delegation_specs,
        &mut self.last_schema_validations,
        &active,
        child_result.answer.as_deref(),
      );

      step_index = merge_child_into(&mut steps, &mut events, step_index, child_result.clone());

      if super::common::halts_supervisor(&child_result.stop_reason) {
        let _ = self.signal.take();
        return Ok(stopped(
          session_id,
          child_result.answer,
          child_result.stop_reason,
          steps,
          events,
        ));
      }

      if let Some(req) = self.signal.take() {
        if handoffs_remaining == 0 {
          let stop_reason = AgentStopReason::StopCondition {
            condition: format!(
              "max_handoffs={} reached; '{}' tried to hand off to '{}' but was refused",
              self.max_handoffs, active, req.to
            ),
          };
          return Ok(stopped(
            session_id,
            child_result.answer,
            stop_reason,
            steps,
            events,
          ));
        }

        let handoff_index = step_index;
        steps.push(AgentStep::new(
          handoff_index,
          AgentStepKind::Handoff {
            from: active.clone(),
            to: req.to.clone(),
            message: req.message.clone(),
          },
        ));
        events.push(AgentEvent::HandoffOccurred {
          session_id: session_id.clone(),
          step_index: handoff_index,
          from: active.clone(),
          to: req.to.clone(),
          timestamp: Utc::now(),
        });
        step_index += 1;
        handoffs_remaining -= 1;
        active = req.to;
        current_input = req.message;
        continue;
      }

      // No handoff: this agent's result is the supervisor's result,
      // including a MaxSteps/Timeout/Error stop — not a FinalAnswer.
      return Ok(stopped(
        session_id,
        child_result.answer,
        child_result.stop_reason,
        steps,
        events,
      ));
    }
  }

  fn runtime_name(&self) -> &'static str {
    "handoff"
  }
}

/// Append `child` into the supervisor's running step/event stream and renumber
/// the child's step indices so they are contiguous in the merged trace.
fn merge_child_into(
  steps: &mut Vec<AgentStep>,
  events: &mut Vec<AgentEvent>,
  mut next_index: usize,
  child: AgentRunResult,
) -> usize {
  // Remap child step indices: each child step gets a fresh index in the
  // supervisor's stream, and we capture the mapping so events can be
  // updated to match.
  let mut index_map: HashMap<usize, usize> = HashMap::new();
  for mut step in child.steps {
    let original = step.index;
    step.index = next_index;
    index_map.insert(original, next_index);
    steps.push(step);
    next_index += 1;
  }
  for mut event in child.events {
    rewrite_event_step_index(&mut event, &index_map);
    events.push(event);
  }
  next_index
}

fn rewrite_event_step_index(event: &mut AgentEvent, map: &HashMap<usize, usize>) {
  match event {
    AgentEvent::StepStarted { step_index, .. }
    | AgentEvent::ToolCallStarted { step_index, .. }
    | AgentEvent::ToolPolicyDecision { step_index, .. }
    | AgentEvent::ToolCapabilityDecision { step_index, .. }
    | AgentEvent::ToolCallCompleted { step_index, .. }
    | AgentEvent::LlmCallCompleted { step_index, .. }
    | AgentEvent::ReflectionAdded { step_index, .. }
    | AgentEvent::HandoffOccurred { step_index, .. }
    | AgentEvent::BlackboardWritten { step_index, .. }
    | AgentEvent::DebateVerdictRendered { step_index, .. } => {
      if let Some(remapped) = map.get(step_index) {
        *step_index = *remapped;
      }
    }
    AgentEvent::StepCompleted { step, .. } => {
      if let Some(remapped) = map.get(&step.index) {
        step.index = *remapped;
      }
    }
    // Events without a `step_index` need no remap — and neither does any
    // future variant, since `AgentEvent` is `#[non_exhaustive]`.
    _ => {}
  }
}

fn stopped(
  session_id: String,
  answer: Option<String>,
  reason: AgentStopReason,
  steps: Vec<AgentStep>,
  mut events: Vec<AgentEvent>,
) -> AgentRunResult {
  events.push(AgentEvent::RunStopped {
    session_id: session_id.clone(),
    reason: reason.clone(),
    timestamp: Utc::now(),
  });
  AgentRunResult {
    session_id,
    answer,
    stop_reason: reason,
    steps,
    events,
  }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Builder for [`HandoffSupervisor`].
///
/// Each call to [`add_agent`](HandoffSupervisorBuilder::add_agent) registers a
/// participant. The factory closure receives an [`Arc<HandoffTool>`] that the
/// caller must register into the agent's [`ToolRegistry`] so the LLM can hand
/// off; the supervisor uses the same tool instance across all agents.
///
/// [`ToolRegistry`]: yanshi_tool::ToolRegistry
pub struct HandoffSupervisorBuilder {
  pending: Vec<HandoffAgentSpec>,
  initial_agent: Option<String>,
  max_handoffs: usize,
  preset_signal: Option<HandoffSignal>,
  delegation_specs: HashMap<String, crate::delegation::DelegationSpec>,
}

struct HandoffAgentSpec {
  name: String,
  description: String,
  factory: Box<dyn FnOnce(Arc<HandoffTool>) -> Box<dyn AgentRuntime> + Send>,
}

impl Default for HandoffSupervisorBuilder {
  fn default() -> Self {
    Self {
      pending: Vec::new(),
      initial_agent: None,
      max_handoffs: 5,
      preset_signal: None,
      delegation_specs: HashMap::new(),
    }
  }
}

impl HandoffSupervisorBuilder {
  pub fn new() -> Self {
    Self::default()
  }

  /// Register an agent. The factory receives the shared [`HandoffTool`] and
  /// must include it in the constructed agent's tool registry. Accepts any
  /// [`AgentRuntime`] (W3.2) — a [`ReActAgent`], a [`PlanExecuteAgent`], or
  /// even a nested [`Supervisor`](crate::Supervisor) — not just `ReActAgent`.
  ///
  /// [`PlanExecuteAgent`]: crate::PlanExecuteAgent
  pub fn add_agent<F, A>(
    mut self,
    name: impl Into<String>,
    description: impl Into<String>,
    factory: F,
  ) -> Self
  where
    F: FnOnce(Arc<HandoffTool>) -> A + Send + 'static,
    A: AgentRuntime + 'static,
  {
    self.pending.push(HandoffAgentSpec {
      name: name.into(),
      description: description.into(),
      factory: Box::new(move |tool| Box::new(factory(tool)) as Box<dyn AgentRuntime>),
    });
    self
  }

  /// Register a [`DelegationSpec`](crate::delegation::DelegationSpec) for
  /// an already-registered agent (W3.3): after each of that agent's turns,
  /// the supervisor validates its answer against the spec's
  /// `expected_output_schema` (when set) and records the outcome, readable
  /// via [`HandoffSupervisor::last_schema_validations`]. `build()` rejects
  /// a spec registered for a name that was never passed to
  /// [`Self::add_agent`].
  pub fn with_delegation_spec(
    mut self,
    name: impl Into<String>,
    spec: crate::delegation::DelegationSpec,
  ) -> Self {
    self.delegation_specs.insert(name.into(), spec);
    self
  }

  /// Choose which agent receives the user's first input. Defaults to the
  /// first registered agent.
  pub fn initial_agent(mut self, name: impl Into<String>) -> Self {
    self.initial_agent = Some(name.into());
    self
  }

  /// Maximum number of consecutive handoffs allowed. Defaults to 5.
  pub fn max_handoffs(mut self, n: usize) -> Self {
    self.max_handoffs = n;
    self
  }

  /// Use a pre-existing [`HandoffSignal`] rather than letting the builder
  /// allocate a fresh one. Required when callers pre-bake [`HandoffTool`]
  /// instances into agents (e.g. via `SkillBuilder::build_with_extra_tools`)
  /// so the agents and the supervisor share the same signal.
  pub fn use_signal(mut self, signal: HandoffSignal) -> Self {
    self.preset_signal = Some(signal);
    self
  }

  pub fn build(self) -> Result<HandoffSupervisor, HandoffSupervisorError> {
    if self.pending.is_empty() {
      return Err(HandoffSupervisorError::NoAgents);
    }

    let mut seen = std::collections::HashSet::new();
    for spec in &self.pending {
      if !seen.insert(spec.name.clone()) {
        return Err(HandoffSupervisorError::DuplicateAgent(spec.name.clone()));
      }
    }

    let target_names: Vec<String> = self.pending.iter().map(|a| a.name.clone()).collect();
    let initial = match self.initial_agent {
      Some(name) => {
        if !target_names.contains(&name) {
          return Err(HandoffSupervisorError::UnknownInitialAgent(name));
        }
        name
      }
      None => target_names[0].clone(),
    };

    for name in self.delegation_specs.keys() {
      if !target_names.contains(name) {
        return Err(HandoffSupervisorError::UnknownDelegationSpecAgent(
          name.clone(),
        ));
      }
    }

    let signal = self.preset_signal.clone().unwrap_or_default();
    let handoff_tool = Arc::new(HandoffTool::new(target_names.clone(), signal.clone()));

    let mut agents: HashMap<String, super::common::SharedAgentRuntime> = HashMap::new();
    let mut agent_descriptions: Vec<(String, String)> = Vec::new();
    for spec in self.pending {
      let agent = (spec.factory)(handoff_tool.clone());
      agent_descriptions.push((spec.name.clone(), spec.description));
      agents.insert(spec.name, Arc::new(AsyncMutex::new(agent)));
    }

    Ok(HandoffSupervisor {
      agents,
      agent_descriptions,
      initial_agent: initial,
      max_handoffs: self.max_handoffs,
      signal,
      session_id: Uuid::new_v4().to_string(),
      delegation_specs: self.delegation_specs,
      last_schema_validations: HashMap::new(),
    })
  }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  use serde_json::json;
  use yanshi_llm::Yanshi;
  use yanshi_memory::SessionMemory;
  use yanshi_tool::ToolRegistry;

  use crate::react::{ReActAgent, ReActConfig};
  use crate::runtime::AgentCancellationToken;

  fn build_agent_with_handoff(handoff: Arc<HandoffTool>, model: &str, persona: &str) -> ReActAgent {
    let mut registry = ToolRegistry::new();
    registry.register(handoff);
    ReActAgent::new(
      ReActConfig::new(model)
        .with_persona(persona)
        .with_max_iterations(4),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    )
  }

  async fn init_mock_model(model: &str) {
    let config_path =
      std::env::temp_dir().join(format!("yanshi-handoff-mock-{}.yml", uuid::Uuid::new_v4()));
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

  fn set_mock_responses(responses: Vec<&str>) {
    let serialized = serde_json::to_string(&responses).unwrap();
    // SAFETY: callers hold crate::LLM_TEST_LOCK to serialise env mutation.
    unsafe {
      std::env::set_var("YANSHI_MOCK_RESPONSES", serialized);
      std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    }
  }

  // ── Builder validation ────────────────────────────────────────────────────

  #[tokio::test]
  async fn builder_rejects_empty_agent_set() {
    let err = HandoffSupervisorBuilder::new().build().unwrap_err();
    assert!(matches!(err, HandoffSupervisorError::NoAgents));
  }

  #[tokio::test]
  async fn builder_rejects_unknown_initial_agent() {
    let err = HandoffSupervisorBuilder::new()
      .add_agent("triage", "triage queue", |handoff| {
        build_agent_with_handoff(handoff, "mock", "Triage agent.")
      })
      .initial_agent("billing")
      .build()
      .unwrap_err();
    assert!(matches!(
      err,
      HandoffSupervisorError::UnknownInitialAgent(_)
    ));
  }

  #[tokio::test]
  async fn builder_rejects_duplicate_agent_names() {
    let err = HandoffSupervisorBuilder::new()
      .add_agent("triage", "first", |handoff| {
        build_agent_with_handoff(handoff, "mock", "first")
      })
      .add_agent("triage", "second", |handoff| {
        build_agent_with_handoff(handoff, "mock", "second")
      })
      .build()
      .unwrap_err();
    assert!(matches!(err, HandoffSupervisorError::DuplicateAgent(_)));
  }

  #[tokio::test]
  async fn builder_assigns_first_agent_as_default_initial() {
    let supervisor = HandoffSupervisorBuilder::new()
      .add_agent("a", "first", |handoff| {
        build_agent_with_handoff(handoff, "mock", "")
      })
      .add_agent("b", "second", |handoff| {
        build_agent_with_handoff(handoff, "mock", "")
      })
      .build()
      .unwrap();
    assert_eq!(supervisor.initial_agent, "a");
    assert_eq!(supervisor.agent_descriptions().len(), 2);
  }

  // ── HandoffTool unit tests ────────────────────────────────────────────────

  #[tokio::test]
  async fn handoff_tool_records_request_and_returns_success() {
    let signal = HandoffSignal::new();
    let tool = HandoffTool::new(vec!["billing".into(), "tech".into()], signal.clone());
    let out = tool
      .execute(json!({"to": "billing", "message": "Refund order #42"}))
      .await
      .unwrap();
    assert!(!out.is_error);

    let req = signal.take().expect("signal must be set");
    assert_eq!(req.to, "billing");
    assert_eq!(req.message, "Refund order #42");
  }

  #[tokio::test]
  async fn handoff_tool_rejects_unknown_target() {
    let signal = HandoffSignal::new();
    let tool = HandoffTool::new(vec!["billing".into()], signal.clone());
    let err = tool
      .execute(json!({"to": "shipping", "message": "where is my package"}))
      .await
      .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParams { .. }));
    assert!(
      signal.take().is_none(),
      "signal must remain empty on rejection"
    );
  }

  #[tokio::test]
  async fn handoff_tool_rejects_missing_fields() {
    let signal = HandoffSignal::new();
    let tool = HandoffTool::new(vec!["a".into()], signal);
    let err = tool.execute(json!({"to": "a"})).await.unwrap_err();
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }

  // ── End-to-end via mock LLM ───────────────────────────────────────────────
  //
  // The mock LLM provider consumes a global FIFO queue of canned responses
  // (`YANSHI_MOCK_RESPONSES`), so tests must hold `crate::LLM_TEST_LOCK`
  // to serialise env-var mutation across the workspace test binary.

  #[tokio::test]
  async fn handoff_chain_routes_input_to_target_and_returns_its_answer() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-handoff-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![
      // Triage iter 0: hand off to billing
      r#"{"thought":"this is a refund request","action":{"tool":"handoff","params":{"to":"billing","message":"refund order #42"}}}"#,
      // Triage iter 1: brief wrap-up after handoff tool returns success
      r#"{"thought":"handed off","answer":"transferred to billing"}"#,
      // Billing iter 0: produce the actual answer
      r#"{"thought":"approve refund","answer":"refund approved"}"#,
    ]);
    init_mock_model(&model).await;

    let triage_model = model.clone();
    let billing_model = model.clone();
    let mut supervisor = HandoffSupervisorBuilder::new()
      .add_agent("triage", "front desk", move |handoff| {
        build_agent_with_handoff(handoff, &triage_model, "front desk")
      })
      .add_agent("billing", "billing specialist", move |handoff| {
        build_agent_with_handoff(handoff, &billing_model, "billing specialist")
      })
      .max_handoffs(3)
      .build()
      .unwrap();

    let result = AgentRuntime::run(
      &mut supervisor,
      AgentContext::new("test-session", "I want a refund", &model),
    )
    .await
    .unwrap();

    assert_eq!(result.answer.as_deref(), Some("refund approved"));
    assert!(matches!(result.stop_reason, AgentStopReason::FinalAnswer));

    let handoff_steps: Vec<&AgentStep> = result
      .steps
      .iter()
      .filter(|s| matches!(s.kind, AgentStepKind::Handoff { .. }))
      .collect();
    assert_eq!(handoff_steps.len(), 1, "exactly one handoff expected");
    if let AgentStepKind::Handoff { from, to, message } = &handoff_steps[0].kind {
      assert_eq!(from, "triage");
      assert_eq!(to, "billing");
      assert_eq!(message, "refund order #42");
    }

    assert!(
      result
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::HandoffOccurred { from, to, .. } if from == "triage" && to == "billing")),
      "HandoffOccurred event must be present"
    );
  }

  #[tokio::test]
  async fn handoff_supervisor_returns_initial_agent_answer_when_no_handoff_requested() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-no-handoff-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![
      // Solo agent: answer immediately, no handoff
      r#"{"thought":"easy","answer":"hello back"}"#,
    ]);
    init_mock_model(&model).await;

    let solo_model = model.clone();
    let mut supervisor = HandoffSupervisorBuilder::new()
      .add_agent("solo", "single agent", move |handoff| {
        build_agent_with_handoff(handoff, &solo_model, "be brief")
      })
      .max_handoffs(3)
      .build()
      .unwrap();

    let result = AgentRuntime::run(
      &mut supervisor,
      AgentContext::new("test-session", "hi", &model),
    )
    .await
    .unwrap();

    assert_eq!(result.answer.as_deref(), Some("hello back"));
    assert!(matches!(result.stop_reason, AgentStopReason::FinalAnswer));
    assert!(
      !result
        .steps
        .iter()
        .any(|s| matches!(s.kind, AgentStepKind::Handoff { .. })),
      "no handoff step should be recorded"
    );
  }

  #[tokio::test]
  async fn max_handoffs_zero_blocks_transitions_and_records_stop_condition() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-max-handoff-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![
      // A iter 0: hand off to B (which will be refused)
      r#"{"thought":"need help","action":{"tool":"handoff","params":{"to":"b","message":"please help"}}}"#,
      // A iter 1: forced wrap-up after handoff tool returns; this answer
      // is what the supervisor returns because the handoff is refused.
      r#"{"thought":"handed off","answer":"transferred (capped)"}"#,
    ]);
    init_mock_model(&model).await;

    let a_model = model.clone();
    let b_model = model.clone();
    let mut supervisor = HandoffSupervisorBuilder::new()
      .add_agent("a", "first", move |handoff| {
        build_agent_with_handoff(handoff, &a_model, "first")
      })
      .add_agent("b", "second", move |handoff| {
        build_agent_with_handoff(handoff, &b_model, "second")
      })
      .max_handoffs(0)
      .build()
      .unwrap();

    let result = AgentRuntime::run(
      &mut supervisor,
      AgentContext::new("test-session", "hi", &model),
    )
    .await
    .unwrap();

    match &result.stop_reason {
      AgentStopReason::StopCondition { condition } => {
        assert!(condition.contains("max_handoffs=0"));
        assert!(condition.contains("'a'"));
        assert!(condition.contains("'b'"));
      }
      other => panic!("expected StopCondition, got {other:?}"),
    }
    // No Handoff step should have been recorded since the transition was refused.
    assert!(
      !result
        .steps
        .iter()
        .any(|s| matches!(s.kind, AgentStepKind::Handoff { .. })),
      "refused handoff must not be recorded as completed"
    );
    // The capped supervisor still returns the initial agent's answer so the
    // user is not left without a response.
    assert_eq!(result.answer.as_deref(), Some("transferred (capped)"));
  }

  #[tokio::test]
  async fn pre_cancelled_token_short_circuits_supervisor() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-cancel-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![]); // never consumed
    init_mock_model(&model).await;

    let solo_model = model.clone();
    let mut supervisor = HandoffSupervisorBuilder::new()
      .add_agent("solo", "single", move |handoff| {
        build_agent_with_handoff(handoff, &solo_model, "")
      })
      .build()
      .unwrap();

    let token = AgentCancellationToken::new();
    token.cancel();
    let context = AgentContext::new("session-1", "anything", &model).with_cancellation_token(token);
    let result = AgentRuntime::run(&mut supervisor, context).await.unwrap();

    assert!(matches!(
      result.stop_reason,
      AgentStopReason::Cancelled { .. }
    ));
    assert!(result.answer.is_none());
  }

  // Direct check that handoff to a non-registered name raises a tool error
  // (the LLM would have to retry). Drives the same scenario as
  // `handoff_tool_rejects_unknown_target` but through a real ReActAgent
  // invocation, asserting the agent itself surfaces a wrap-up answer.
  #[tokio::test]
  async fn handoff_tool_invalid_target_is_surfaced_as_tool_error_not_supervisor_failure() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-bad-target-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![
      // A iter 0: try to hand off to non-existent agent
      r#"{"thought":"send to ghost","action":{"tool":"handoff","params":{"to":"ghost","message":"hi"}}}"#,
      // A iter 1: after tool error, give up and produce final answer
      r#"{"thought":"that didn't work","answer":"sorry, stuck"}"#,
    ]);
    init_mock_model(&model).await;

    let solo_model = model.clone();
    let mut supervisor = HandoffSupervisorBuilder::new()
      .add_agent("solo", "single", move |handoff| {
        build_agent_with_handoff(handoff, &solo_model, "")
      })
      .max_handoffs(3)
      .build()
      .unwrap();

    let result = AgentRuntime::run(
      &mut supervisor,
      AgentContext::new("session-1", "hello", &model),
    )
    .await
    .unwrap();

    // Supervisor itself succeeds (FinalAnswer) — the bad handoff was just a
    // tool error inside the agent's loop.
    assert!(matches!(result.stop_reason, AgentStopReason::FinalAnswer));
    assert_eq!(result.answer.as_deref(), Some("sorry, stuck"));
    // No successful handoff step recorded.
    assert!(
      !result
        .steps
        .iter()
        .any(|s| matches!(s.kind, AgentStepKind::Handoff { .. })),
      "rejected handoff must not produce a Handoff step"
    );
  }

  #[test]
  fn signal_is_drained_per_run() {
    let signal = HandoffSignal::new();
    signal.set(HandoffRequest {
      to: "x".into(),
      message: "hi".into(),
    });
    assert!(signal.take().is_some());
    assert!(signal.take().is_none(), "second take must return None");
  }

  #[test]
  fn merge_child_renumbers_step_indices() {
    let mut steps = Vec::new();
    let mut events = Vec::new();
    let child = AgentRunResult {
      session_id: "child".into(),
      answer: Some("ans".into()),
      stop_reason: AgentStopReason::FinalAnswer,
      steps: vec![
        AgentStep::new(0, AgentStepKind::Observe { input: "in".into() }),
        AgentStep::new(
          1,
          AgentStepKind::FinalAnswer {
            answer: "ans".into(),
          },
        ),
      ],
      events: vec![AgentEvent::ReflectionAdded {
        session_id: "child".into(),
        step_index: 1,
        timestamp: Utc::now(),
      }],
    };

    let next = merge_child_into(&mut steps, &mut events, 5, child);
    assert_eq!(next, 7);
    assert_eq!(steps[0].index, 5);
    assert_eq!(steps[1].index, 6);
    if let AgentEvent::ReflectionAdded { step_index, .. } = &events[0] {
      assert_eq!(*step_index, 6, "child step_index must be remapped");
    } else {
      panic!("expected ReflectionAdded");
    }
  }

  /// W3.2 regression: pre-fix, `add_agent`'s factory closure had to return
  /// a concrete `ReActAgent` — a `PlanExecuteAgent`, a nested `Supervisor`,
  /// or (as here) any other `AgentRuntime` impl could not participate. This
  /// stub needs no LLM at all, proving the fix at the type level (the
  /// closure compiles) and at runtime (the supervisor actually dispatches
  /// to it and returns its answer).
  struct StubRuntime {
    answer: String,
  }
  #[async_trait]
  impl AgentRuntime for StubRuntime {
    async fn run(&mut self, context: AgentContext) -> Result<AgentRunResult, AgentRuntimeError> {
      Ok(AgentRunResult {
        session_id: context.session_id,
        answer: Some(self.answer.clone()),
        stop_reason: AgentStopReason::FinalAnswer,
        steps: Vec::new(),
        events: Vec::new(),
      })
    }
    fn runtime_name(&self) -> &'static str {
      "stub"
    }
  }

  #[tokio::test]
  async fn add_agent_accepts_a_non_react_agent_runtime() {
    let mut supervisor = HandoffSupervisorBuilder::new()
      .add_agent("stub", "a non-ReActAgent participant", |_handoff| {
        StubRuntime {
          answer: "stub answer".to_string(),
        }
      })
      .build()
      .unwrap();
    let answer = supervisor.run("hello").await.unwrap();
    assert_eq!(answer, "stub answer");
  }

  /// W3.3 regression: a `DelegationSpec` registered via
  /// `with_delegation_spec` must actually be consulted by the run loop —
  /// pre-fix, `DelegationSpec`/`validate_output` existed only as a
  /// standalone primitive a caller had to invoke by hand.
  #[tokio::test]
  async fn with_delegation_spec_validates_the_agents_answer() {
    let spec =
      crate::delegation::DelegationSpec::new("say hi").with_expected_output_schema(json!({
        "type": "object",
        "properties": { "ok": { "type": "boolean" } },
        "required": ["ok"]
      }));
    let mut supervisor = HandoffSupervisorBuilder::new()
      .add_agent("stub", "d", |_handoff| StubRuntime {
        answer: r#"{"ok":true}"#.to_string(),
      })
      .with_delegation_spec("stub", spec)
      .build()
      .unwrap();
    supervisor.run("hello").await.unwrap();
    assert_eq!(
      supervisor.last_schema_validations().get("stub"),
      Some(&crate::delegation::SchemaValidation::Valid)
    );
  }

  #[tokio::test]
  async fn with_delegation_spec_reports_invalid_when_answer_violates_schema() {
    let spec =
      crate::delegation::DelegationSpec::new("say hi").with_expected_output_schema(json!({
        "type": "object",
        "properties": { "ok": { "type": "boolean" } },
        "required": ["ok"]
      }));
    let mut supervisor = HandoffSupervisorBuilder::new()
      .add_agent("stub", "d", |_handoff| StubRuntime {
        answer: "not json".to_string(),
      })
      .with_delegation_spec("stub", spec)
      .build()
      .unwrap();
    supervisor.run("hello").await.unwrap();
    assert!(
      !supervisor
        .last_schema_validations()
        .get("stub")
        .unwrap()
        .is_valid()
    );
  }

  #[test]
  fn build_rejects_delegation_spec_for_unknown_agent() {
    let result = HandoffSupervisorBuilder::new()
      .add_agent("stub", "d", |_handoff| StubRuntime {
        answer: "x".to_string(),
      })
      .with_delegation_spec(
        "does-not-exist",
        crate::delegation::DelegationSpec::new("goal"),
      )
      .build();
    assert!(matches!(
      result,
      Err(HandoffSupervisorError::UnknownDelegationSpecAgent(_))
    ));
  }
}
