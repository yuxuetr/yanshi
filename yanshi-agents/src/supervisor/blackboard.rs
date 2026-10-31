//! Blackboard multi-agent collaboration: agents share a key/value store and
//! cooperate by reading and writing entries while a supervisor schedules them
//! sequentially or in parallel.
//!
//! # Mental model
//!
//! Think of the [`Blackboard`] as a shared whiteboard a research team gathers
//! around: each member adds findings, builds on what others wrote, and stops
//! when a target conclusion appears.
//!
//! # Lifecycle
//!
//! 1. The supervisor records the user's request as an `Observe` step.
//! 2. According to its [`BlackboardSchedule`] it dispatches agents:
//!    - **Sequential** — one at a time, so later agents see prior writes.
//!    - **Parallel** — all in one round; writes from concurrent agents are
//!      not visible until the round ends.
//! 3. After each round (or after each agent in sequential mode), the
//!    supervisor checks the [`BlackboardStop`] condition. The first match
//!    ends the loop.
//! 4. The supervisor's final answer is the most recent value written under a
//!    configurable "answer key", or — when absent — the last agent's reply.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use yanshi_tool::{Tool, ToolError, ToolMetadata, ToolOutput};

use crate::runtime::{
  AgentContext, AgentEvent, AgentRunResult, AgentRuntime, AgentRuntimeError, AgentStep,
  AgentStepKind, AgentStopReason, BlackboardOpKind,
};

// ── Blackboard data store ─────────────────────────────────────────────────────

/// One entry in the [`Blackboard`].
#[derive(Debug, Clone)]
pub struct BlackboardEntry {
  pub value: Value,
  pub written_by: String,
  pub version: u64,
  pub written_at: DateTime<Utc>,
}

/// Shared key/value store backing a [`BlackboardSupervisor`].
///
/// Cloning is cheap: only the inner `Arc`s are copied. All instances handed
/// to tools share state with the supervisor.
#[derive(Debug, Clone, Default)]
pub struct Blackboard {
  entries: Arc<RwLock<HashMap<String, BlackboardEntry>>>,
  // ops are appended in insertion order so the supervisor can drain them
  // after each round and emit BlackboardOp steps + BlackboardWritten events.
  ops: Arc<Mutex<Vec<BlackboardOpRecord>>>,
  next_version: Arc<Mutex<u64>>,
}

#[derive(Debug, Clone)]
struct BlackboardOpRecord {
  op: BlackboardOpKind,
  key: String,
  agent: String,
  value: Option<Value>,
}

impl Blackboard {
  pub fn new() -> Self {
    Self::default()
  }

  /// Snapshot the current key/value pairs.
  pub fn snapshot(&self) -> HashMap<String, BlackboardEntry> {
    self
      .entries
      .read()
      .map(|guard| guard.clone())
      .unwrap_or_default()
  }

  /// Read a single key.
  pub fn get(&self, key: &str) -> Option<BlackboardEntry> {
    self.entries.read().ok().and_then(|g| g.get(key).cloned())
  }

  /// True when the key has been written at least once.
  pub fn has(&self, key: &str) -> bool {
    self.get(key).is_some()
  }

  fn write_internal(&self, key: &str, value: Value, agent: &str) {
    // Q3.12.2: poison-tolerant lock acquisition matching the
    // `if let Ok(...)` convention used by the entries / ops sites
    // below. The version counter is incrementing-monotonic — even
    // after a prior thread panicked while holding the lock, the
    // payload is just a `u64` with no torn-write risk, so we
    // recover via `into_inner()` rather than propagating the panic
    // out of `Blackboard::write` (which would in turn poison the
    // supervisor's outer Tokio task).
    let version = {
      let mut counter = match self.next_version.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
      };
      *counter += 1;
      *counter
    };
    let entry = BlackboardEntry {
      value: value.clone(),
      written_by: agent.to_string(),
      version,
      written_at: Utc::now(),
    };
    if let Ok(mut entries) = self.entries.write() {
      entries.insert(key.to_string(), entry);
    }
    if let Ok(mut ops) = self.ops.lock() {
      ops.push(BlackboardOpRecord {
        op: BlackboardOpKind::Write,
        key: key.to_string(),
        agent: agent.to_string(),
        value: Some(value),
      });
    }
  }

  fn record_read(&self, key: &str, agent: &str) {
    if let Ok(mut ops) = self.ops.lock() {
      ops.push(BlackboardOpRecord {
        op: BlackboardOpKind::Read,
        key: key.to_string(),
        agent: agent.to_string(),
        value: None,
      });
    }
  }

  fn drain_ops(&self) -> Vec<BlackboardOpRecord> {
    self
      .ops
      .lock()
      .map(|mut g| std::mem::take(&mut *g))
      .unwrap_or_default()
  }
}

// ── Per-agent read/write tools ────────────────────────────────────────────────

/// Tool that lets an agent read a value from the shared [`Blackboard`].
pub struct BlackboardReadTool {
  blackboard: Blackboard,
  agent_name: String,
}

impl BlackboardReadTool {
  pub fn new(blackboard: Blackboard, agent_name: impl Into<String>) -> Self {
    Self {
      blackboard,
      agent_name: agent_name.into(),
    }
  }
}

#[async_trait]
impl Tool for BlackboardReadTool {
  fn name(&self) -> &str {
    "bb_read"
  }
  fn description(&self) -> &str {
    "Read a value previously written to the shared blackboard. Returns the JSON \
     value, or `null` if the key is unset."
  }
  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": { "key": { "type": "string" } },
      "required": ["key"],
    })
  }
  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::builtin_named("bb_read")
  }
  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    let key =
      params
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidParams {
          message: "bb_read: 'key' must be a string".into(),
        })?;
    self.blackboard.record_read(key, &self.agent_name);
    let payload = match self.blackboard.get(key) {
      Some(entry) => serde_json::to_string(&entry.value).unwrap_or_else(|_| "null".into()),
      None => "null".into(),
    };
    Ok(ToolOutput::success(payload))
  }
}

/// Tool that lets an agent write a value to the shared [`Blackboard`].
pub struct BlackboardWriteTool {
  blackboard: Blackboard,
  agent_name: String,
}

impl BlackboardWriteTool {
  pub fn new(blackboard: Blackboard, agent_name: impl Into<String>) -> Self {
    Self {
      blackboard,
      agent_name: agent_name.into(),
    }
  }
}

#[async_trait]
impl Tool for BlackboardWriteTool {
  fn name(&self) -> &str {
    "bb_write"
  }
  fn description(&self) -> &str {
    "Write a value to the shared blackboard so other agents can read it. The \
     value is any JSON-serialisable structure."
  }
  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": {
        "key":   { "type": "string", "description": "Identifier for the value." },
        "value": { "description": "JSON value to store." }
      },
      "required": ["key", "value"],
    })
  }
  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::builtin_named("bb_write")
  }
  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    let key =
      params
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidParams {
          message: "bb_write: 'key' must be a string".into(),
        })?;
    if key.trim().is_empty() {
      return Err(ToolError::InvalidParams {
        message: "bb_write: 'key' must be non-empty".into(),
      });
    }
    let value = params
      .get("value")
      .cloned()
      .ok_or_else(|| ToolError::InvalidParams {
        message: "bb_write: 'value' is required".into(),
      })?;
    self.blackboard.write_internal(key, value, &self.agent_name);
    Ok(ToolOutput::success(format!(
      "wrote key '{}' to blackboard",
      key
    )))
  }
}

// ── Schedule + stop condition ─────────────────────────────────────────────────

/// How [`BlackboardSupervisor`] dispatches its agents.
#[derive(Debug, Clone)]
pub enum BlackboardSchedule {
  /// Run the named agents one after another in this exact order. Each agent
  /// sees writes from prior agents.
  Sequential(Vec<String>),
  /// Run the named agents concurrently in a single round. Writes are visible
  /// only after the round completes.
  Parallel(Vec<String>),
}

impl BlackboardSchedule {
  fn agents(&self) -> &[String] {
    match self {
      Self::Sequential(v) | Self::Parallel(v) => v,
    }
  }
}

/// When [`BlackboardSupervisor`] should stop dispatching agents.
#[derive(Debug, Clone)]
pub enum BlackboardStop {
  /// Stop only after every agent in the schedule has run once.
  AllAgentsCompleted,
  /// Stop as soon as the named key has been written at least once.
  KeySet(String),
}

// ── Supervisor ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum BlackboardSupervisorError {
  #[error("BlackboardSupervisor needs at least one agent")]
  NoAgents,
  #[error("Schedule references unknown agent '{0}'")]
  UnknownScheduledAgent(String),
  #[error("Duplicate agent name '{0}'")]
  DuplicateAgent(String),
  #[error("delegation spec registered for unknown agent '{0}'")]
  UnknownDelegationSpecAgent(String),
}

/// A multi-agent runtime where agents share state via a [`Blackboard`].
///
/// Implements [`AgentRuntime`] so it can plug into [`AgentNode`] like any
/// other agent runtime.
///
/// [`AgentNode`]: crate::nodes::AgentNode
pub struct BlackboardSupervisor {
  agents: HashMap<String, super::common::SharedAgentRuntime>,
  agent_descriptions: Vec<(String, String)>,
  schedule: BlackboardSchedule,
  stop: BlackboardStop,
  blackboard: Blackboard,
  /// Optional key the supervisor reads as the final answer at run end.
  answer_key: Option<String>,
  session_id: String,
  /// W3.3: optional per-agent `DelegationSpec`, consulted after each turn
  /// to validate that agent's answer — see
  /// [`Self::record_delegation_validation`].
  delegation_specs: HashMap<String, crate::delegation::DelegationSpec>,
  /// `Mutex`-wrapped (not a plain field) because `run_schedule`'s parallel
  /// path records validations through a shared `&BlackboardSupervisor`
  /// (agents run concurrently under `tokio::spawn`, so no single call site
  /// holds `&mut self`) — the same interior-mutability shape [`Blackboard`]
  /// itself already uses for `entries`/`ops`.
  last_schema_validations: std::sync::Mutex<HashMap<String, crate::delegation::SchemaValidation>>,
}

impl std::fmt::Debug for BlackboardSupervisor {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("BlackboardSupervisor")
      .field("session_id", &self.session_id)
      .field("schedule", &self.schedule)
      .field("stop", &self.stop)
      .field("agents", &self.agent_descriptions)
      .field("answer_key", &self.answer_key)
      .finish()
  }
}

impl BlackboardSupervisor {
  pub fn session_id(&self) -> &str {
    &self.session_id
  }
  pub fn agent_descriptions(&self) -> &[(String, String)] {
    &self.agent_descriptions
  }
  pub fn blackboard(&self) -> &Blackboard {
    &self.blackboard
  }

  /// Schema-validation outcomes (W3.3) from the most recent run, keyed by
  /// agent name — only populated for agents with a
  /// [`DelegationSpec`](crate::delegation::DelegationSpec) registered via
  /// [`BlackboardSupervisorBuilder::with_delegation_spec`].
  pub fn last_schema_validations(&self) -> HashMap<String, crate::delegation::SchemaValidation> {
    self
      .last_schema_validations
      .lock()
      .map(|guard| guard.clone())
      .unwrap_or_default()
  }

  fn record_delegation_validation(&self, agent_name: &str, answer: Option<&str>) {
    if let Ok(mut validations) = self.last_schema_validations.lock() {
      super::common::record_delegation_validation(
        &self.delegation_specs,
        &mut validations,
        agent_name,
        answer,
      );
    }
  }

  /// Convenience: run a one-shot task and return the final answer string.
  pub async fn run(&mut self, task: &str) -> Result<String, AgentRuntimeError> {
    let context = AgentContext::new(self.session_id.clone(), task, "");
    let result = AgentRuntime::run(self, context).await?;
    result
      .answer
      .ok_or_else(|| AgentRuntimeError::ExecutionFailed {
        message: format!(
          "BlackboardSupervisor stopped without a final answer: {:?}",
          result.stop_reason
        ),
      })
  }

  fn agent_handle(
    &self,
    name: &str,
  ) -> Result<super::common::SharedAgentRuntime, AgentRuntimeError> {
    self
      .agents
      .get(name)
      .cloned()
      .ok_or_else(|| AgentRuntimeError::ExecutionFailed {
        message: format!("BlackboardSupervisor: unknown agent '{}'", name),
      })
  }

  fn build_child_context(
    &self,
    parent: &AgentContext,
    agent_name: &str,
    input: &str,
  ) -> AgentContext {
    super::common::build_child_context(parent, agent_name, input)
  }

  fn drain_ops_into_supervisor_trace(
    &self,
    next_index: &mut usize,
    steps: &mut Vec<AgentStep>,
    events: &mut Vec<AgentEvent>,
    session_id: &str,
  ) {
    let drained = self.blackboard.drain_ops();
    for record in drained {
      let idx = *next_index;
      steps.push(AgentStep::new(
        idx,
        AgentStepKind::BlackboardOp {
          op: record.op,
          key: record.key.clone(),
          agent: record.agent.clone(),
          value: record.value.clone(),
        },
      ));
      events.push(AgentEvent::BlackboardWritten {
        session_id: session_id.to_string(),
        step_index: idx,
        op: record.op,
        agent: record.agent,
        key: record.key,
        timestamp: Utc::now(),
      });
      *next_index += 1;
    }
  }

  fn check_stop_condition(&self) -> Option<AgentStopReason> {
    match &self.stop {
      BlackboardStop::AllAgentsCompleted => None, // checked by the loop
      BlackboardStop::KeySet(key) => {
        if self.blackboard.has(key) {
          Some(AgentStopReason::StopCondition {
            condition: format!("blackboard key '{}' was set", key),
          })
        } else {
          None
        }
      }
    }
  }
}

#[async_trait]
impl AgentRuntime for BlackboardSupervisor {
  async fn run(&mut self, context: AgentContext) -> Result<AgentRunResult, AgentRuntimeError> {
    let session_id = context.session_id.clone();
    let mut steps: Vec<AgentStep> = Vec::new();
    let mut events: Vec<AgentEvent> = Vec::new();
    let mut step_index = 0usize;

    events.push(AgentEvent::RunStarted {
      session_id: session_id.clone(),
      model: format!(
        "multi_agent:blackboard(schedule={:?})",
        self.schedule.agents()
      ),
      timestamp: context.started_at,
    });
    steps.push(AgentStep::new(
      step_index,
      AgentStepKind::Observe {
        input: context.input.clone(),
      },
    ));
    step_index += 1;

    let cancellation = context.cancellation_token.clone();
    // Drain stale ops from any prior run.
    let _ = self.blackboard.drain_ops();

    let stopped_due_to_keyset = run_schedule(
      self,
      &context,
      &mut step_index,
      &mut steps,
      &mut events,
      &cancellation,
    )
    .await?;

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

    let stop_reason = stopped_due_to_keyset.unwrap_or_else(|| match &self.stop {
      BlackboardStop::AllAgentsCompleted => AgentStopReason::StopCondition {
        condition: "all_agents_completed".into(),
      },
      BlackboardStop::KeySet(key) => AgentStopReason::StopCondition {
        condition: format!("schedule exhausted without writing key '{}'", key),
      },
    });

    let answer = self
      .answer_key
      .as_deref()
      .and_then(|key| self.blackboard.get(key))
      .map(|entry| match &entry.value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
      });

    Ok(stopped(session_id, answer, stop_reason, steps, events))
  }

  fn runtime_name(&self) -> &'static str {
    "blackboard"
  }
}

/// Run agents according to the schedule, populating `steps`/`events` and
/// returning `Some(stop_reason)` if the stop condition was met early.
async fn run_schedule(
  supervisor: &BlackboardSupervisor,
  parent: &AgentContext,
  next_index: &mut usize,
  steps: &mut Vec<AgentStep>,
  events: &mut Vec<AgentEvent>,
  cancellation: &Option<crate::runtime::AgentCancellationToken>,
) -> Result<Option<AgentStopReason>, AgentRuntimeError> {
  match &supervisor.schedule {
    BlackboardSchedule::Sequential(names) => {
      for name in names {
        if cancellation.as_ref().is_some_and(|t| t.is_cancelled()) {
          return Ok(None);
        }
        let agent = supervisor.agent_handle(name)?;
        let child_ctx = supervisor.build_child_context(parent, name, &parent.input);
        let child_result = {
          let mut guard = agent.lock().await;
          guard.run(child_ctx).await?
        };
        supervisor.record_delegation_validation(name, child_result.answer.as_deref());
        let child_stop = child_result.stop_reason.clone();
        *next_index = merge_child_into(steps, events, *next_index, child_result);
        supervisor.drain_ops_into_supervisor_trace(next_index, steps, events, &parent.session_id);
        if super::common::halts_supervisor(&child_stop) {
          return Ok(Some(child_stop));
        }
        if let Some(reason) = supervisor.check_stop_condition() {
          return Ok(Some(reason));
        }
      }
      Ok(None)
    }
    BlackboardSchedule::Parallel(names) => {
      let mut handles: Vec<tokio::task::JoinHandle<Result<AgentRunResult, AgentRuntimeError>>> =
        Vec::with_capacity(names.len());
      for name in names {
        let agent = supervisor.agent_handle(name)?;
        let ctx = supervisor.build_child_context(parent, name, &parent.input);
        handles.push(tokio::spawn(async move {
          let mut guard = agent.lock().await;
          guard.run(ctx).await
        }));
      }
      // Order results by schedule order (zip with names) to keep traces stable.
      let mut pending = names.iter().zip(handles);
      while let Some((name, handle)) = pending.next() {
        let child_result = handle
          .await
          .map_err(|e| AgentRuntimeError::ExecutionFailed {
            message: format!("BlackboardSupervisor: parallel join failed: {e}"),
          })??;
        supervisor.record_delegation_validation(name, child_result.answer.as_deref());
        let child_stop = child_result.stop_reason.clone();
        *next_index = merge_child_into(steps, events, *next_index, child_result);
        if super::common::halts_supervisor(&child_stop) {
          for (_, rest) in pending {
            rest.abort();
          }
          supervisor.drain_ops_into_supervisor_trace(next_index, steps, events, &parent.session_id);
          return Ok(Some(child_stop));
        }
      }
      supervisor.drain_ops_into_supervisor_trace(next_index, steps, events, &parent.session_id);
      Ok(supervisor.check_stop_condition())
    }
  }
}

// ── Helpers (re-used from handoff) ────────────────────────────────────────────

fn merge_child_into(
  steps: &mut Vec<AgentStep>,
  events: &mut Vec<AgentEvent>,
  mut next_index: usize,
  child: AgentRunResult,
) -> usize {
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

/// Builder for [`BlackboardSupervisor`].
///
/// The factory closure receives the shared [`Blackboard`] and is responsible
/// for registering [`BlackboardReadTool`] and [`BlackboardWriteTool`] (or
/// equivalents) so the agent can interact with the board.
pub struct BlackboardSupervisorBuilder {
  pending: Vec<BlackboardAgentSpec>,
  schedule: Option<BlackboardSchedule>,
  stop: BlackboardStop,
  answer_key: Option<String>,
  delegation_specs: HashMap<String, crate::delegation::DelegationSpec>,
}

struct BlackboardAgentSpec {
  name: String,
  description: String,
  factory: Box<dyn FnOnce(Blackboard) -> Box<dyn AgentRuntime> + Send>,
}

impl Default for BlackboardSupervisorBuilder {
  fn default() -> Self {
    Self {
      pending: Vec::new(),
      schedule: None,
      stop: BlackboardStop::AllAgentsCompleted,
      answer_key: None,
      delegation_specs: HashMap::new(),
    }
  }
}

impl BlackboardSupervisorBuilder {
  pub fn new() -> Self {
    Self::default()
  }

  /// Register an agent. The factory receives the shared blackboard so it can
  /// register `bb_read` / `bb_write` tools (or equivalents) on the agent's
  /// registry. Accepts any [`AgentRuntime`] (W3.2), not just `ReActAgent`.
  pub fn add_agent<F, A>(
    mut self,
    name: impl Into<String>,
    description: impl Into<String>,
    factory: F,
  ) -> Self
  where
    F: FnOnce(Blackboard) -> A + Send + 'static,
    A: AgentRuntime + 'static,
  {
    self.pending.push(BlackboardAgentSpec {
      name: name.into(),
      description: description.into(),
      factory: Box::new(move |bb| Box::new(factory(bb)) as Box<dyn AgentRuntime>),
    });
    self
  }

  pub fn schedule(mut self, schedule: BlackboardSchedule) -> Self {
    self.schedule = Some(schedule);
    self
  }

  pub fn stop_when(mut self, stop: BlackboardStop) -> Self {
    self.stop = stop;
    self
  }

  /// Optional: read this key from the blackboard at the end of the run and
  /// surface its value as the supervisor's `answer`. When unset (or the key
  /// is missing) the supervisor returns `None`.
  pub fn answer_from(mut self, key: impl Into<String>) -> Self {
    self.answer_key = Some(key.into());
    self
  }

  /// Register a [`DelegationSpec`](crate::delegation::DelegationSpec) for
  /// an already-registered agent (W3.3): after each of that agent's turns,
  /// the supervisor validates its answer against the spec's
  /// `expected_output_schema` (when set) and records the outcome, readable
  /// via [`BlackboardSupervisor::last_schema_validations`]. `build()`
  /// rejects a spec registered for a name that was never passed to
  /// [`Self::add_agent`].
  pub fn with_delegation_spec(
    mut self,
    name: impl Into<String>,
    spec: crate::delegation::DelegationSpec,
  ) -> Self {
    self.delegation_specs.insert(name.into(), spec);
    self
  }

  pub fn build(self) -> Result<BlackboardSupervisor, BlackboardSupervisorError> {
    if self.pending.is_empty() {
      return Err(BlackboardSupervisorError::NoAgents);
    }

    let mut seen = std::collections::HashSet::new();
    for spec in &self.pending {
      if !seen.insert(spec.name.clone()) {
        return Err(BlackboardSupervisorError::DuplicateAgent(spec.name.clone()));
      }
    }

    let known_names: std::collections::HashSet<String> =
      self.pending.iter().map(|a| a.name.clone()).collect();

    let schedule = match self.schedule {
      Some(s) => {
        for name in s.agents() {
          if !known_names.contains(name) {
            return Err(BlackboardSupervisorError::UnknownScheduledAgent(
              name.clone(),
            ));
          }
        }
        s
      }
      None => BlackboardSchedule::Sequential(self.pending.iter().map(|a| a.name.clone()).collect()),
    };

    for name in self.delegation_specs.keys() {
      if !known_names.contains(name) {
        return Err(BlackboardSupervisorError::UnknownDelegationSpecAgent(
          name.clone(),
        ));
      }
    }

    let blackboard = Blackboard::new();
    let mut agents: HashMap<String, super::common::SharedAgentRuntime> = HashMap::new();
    let mut agent_descriptions: Vec<(String, String)> = Vec::new();
    for spec in self.pending {
      let agent = (spec.factory)(blackboard.clone());
      agent_descriptions.push((spec.name.clone(), spec.description));
      agents.insert(spec.name, Arc::new(AsyncMutex::new(agent)));
    }

    Ok(BlackboardSupervisor {
      agents,
      agent_descriptions,
      schedule,
      stop: self.stop,
      blackboard,
      answer_key: self.answer_key,
      session_id: Uuid::new_v4().to_string(),
      delegation_specs: self.delegation_specs,
      last_schema_validations: std::sync::Mutex::new(HashMap::new()),
    })
  }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;

  use serde_json::json;
  use yanshi_llm::Yanshi;
  use yanshi_memory::SessionMemory;
  use yanshi_tool::ToolRegistry;

  use crate::react::{ReActAgent, ReActConfig};

  fn build_agent(blackboard: Blackboard, name: &str, model: &str) -> ReActAgent {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BlackboardReadTool::new(blackboard.clone(), name)));
    registry.register(Arc::new(BlackboardWriteTool::new(blackboard, name)));
    ReActAgent::new(
      ReActConfig::new(model).with_max_iterations(3),
      Box::new(SessionMemory::default_window()),
      Arc::new(registry),
    )
  }

  async fn init_mock_model(model: &str) {
    let path = std::env::temp_dir().join(format!("yanshi-bb-mock-{}.yml", uuid::Uuid::new_v4()));
    std::fs::write(
      &path,
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
    Yanshi::init_with_config(path.to_str().unwrap())
      .await
      .unwrap();
  }

  fn set_mock_responses(responses: Vec<&str>) {
    let s = serde_json::to_string(&responses).unwrap();
    // SAFETY: callers hold crate::LLM_TEST_LOCK to serialise env mutation.
    unsafe {
      std::env::set_var("YANSHI_MOCK_RESPONSES", s);
      std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    }
  }

  // ── Pure-data tests (no LLM) ──────────────────────────────────────────────

  #[test]
  fn blackboard_read_after_write_returns_the_value() {
    let bb = Blackboard::new();
    bb.write_internal("topic", json!("rust async"), "researcher");
    let entry = bb.get("topic").expect("must exist");
    assert_eq!(entry.value, json!("rust async"));
    assert_eq!(entry.written_by, "researcher");
    assert_eq!(entry.version, 1);
  }

  #[test]
  fn blackboard_versions_increment_on_each_write() {
    let bb = Blackboard::new();
    bb.write_internal("k", json!(1), "a");
    bb.write_internal("k", json!(2), "b");
    let entry = bb.get("k").unwrap();
    assert_eq!(entry.version, 2);
    assert_eq!(entry.value, json!(2));
    assert_eq!(entry.written_by, "b");
  }

  #[tokio::test]
  async fn bb_write_tool_records_op_and_persists_entry() {
    let bb = Blackboard::new();
    let tool = BlackboardWriteTool::new(bb.clone(), "writer");
    let out = tool
      .execute(json!({"key": "facts", "value": {"score": 0.9}}))
      .await
      .unwrap();
    assert!(!out.is_error);
    assert_eq!(bb.get("facts").unwrap().value, json!({"score": 0.9}));
    let ops = bb.drain_ops();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0].op, BlackboardOpKind::Write));
    assert_eq!(ops[0].key, "facts");
  }

  #[tokio::test]
  async fn bb_write_tool_rejects_empty_key() {
    let tool = BlackboardWriteTool::new(Blackboard::new(), "x");
    let err = tool
      .execute(json!({"key": "", "value": 1}))
      .await
      .unwrap_err();
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }

  #[tokio::test]
  async fn bb_read_tool_returns_null_when_key_missing() {
    let bb = Blackboard::new();
    let tool = BlackboardReadTool::new(bb.clone(), "reader");
    let out = tool.execute(json!({"key": "nope"})).await.unwrap();
    assert_eq!(out.content, "null");
    let ops = bb.drain_ops();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0].op, BlackboardOpKind::Read));
  }

  // ── Builder validation ────────────────────────────────────────────────────

  #[tokio::test]
  async fn builder_rejects_empty_agents() {
    let err = BlackboardSupervisorBuilder::new().build().unwrap_err();
    assert!(matches!(err, BlackboardSupervisorError::NoAgents));
  }

  #[tokio::test]
  async fn builder_rejects_unknown_scheduled_agent() {
    let err = BlackboardSupervisorBuilder::new()
      .add_agent("a", "first", |bb| build_agent(bb, "a", "mock"))
      .schedule(BlackboardSchedule::Sequential(vec![
        "a".into(),
        "ghost".into(),
      ]))
      .build()
      .unwrap_err();
    assert!(matches!(
      err,
      BlackboardSupervisorError::UnknownScheduledAgent(_)
    ));
  }

  #[tokio::test]
  async fn builder_defaults_schedule_to_registration_order() {
    let supervisor = BlackboardSupervisorBuilder::new()
      .add_agent("a", "first", |bb| build_agent(bb, "a", "mock"))
      .add_agent("b", "second", |bb| build_agent(bb, "b", "mock"))
      .build()
      .unwrap();
    if let BlackboardSchedule::Sequential(names) = &supervisor.schedule {
      assert_eq!(names, &vec!["a".to_string(), "b".to_string()]);
    } else {
      panic!("expected Sequential");
    }
  }

  // ── End-to-end via mock LLM ───────────────────────────────────────────────

  #[tokio::test]
  async fn sequential_schedule_lets_b_read_a_write() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-bb-seq-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![
      // Agent A iter 0: write "facts" key
      r#"{"thought":"recording finding","action":{"tool":"bb_write","params":{"key":"facts","value":"rust is fast"}}}"#,
      // Agent A iter 1: final answer
      r#"{"thought":"done","answer":"recorded"}"#,
      // Agent B iter 0: read "facts"
      r#"{"thought":"checking","action":{"tool":"bb_read","params":{"key":"facts"}}}"#,
      // Agent B iter 1: based on the read, produce answer
      r#"{"thought":"got it","answer":"rust is fast — verified"}"#,
    ]);
    init_mock_model(&model).await;

    let mut supervisor = BlackboardSupervisorBuilder::new()
      .add_agent("a", "writer", {
        let model = model.clone();
        move |bb| build_agent(bb, "a", &model)
      })
      .add_agent("b", "reader", {
        let model = model.clone();
        move |bb| build_agent(bb, "b", &model)
      })
      .build()
      .unwrap();

    let context = AgentContext::new("session-1", "do research", &model);
    let result = AgentRuntime::run(&mut supervisor, context).await.unwrap();

    assert!(matches!(
      result.stop_reason,
      AgentStopReason::StopCondition { .. }
    ));
    // The write must be visible afterwards.
    assert_eq!(
      supervisor.blackboard().get("facts").unwrap().value,
      json!("rust is fast")
    );
    // BlackboardOp steps are emitted by the supervisor in addition to the
    // raw ToolCall steps from the agents.
    let bb_op_steps: Vec<&AgentStep> = result
      .steps
      .iter()
      .filter(|s| matches!(s.kind, AgentStepKind::BlackboardOp { .. }))
      .collect();
    assert!(
      bb_op_steps.len() >= 2,
      "expected ≥2 BlackboardOp steps, got {}",
      bb_op_steps.len()
    );
    assert!(
      result
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::BlackboardWritten { .. })),
      "BlackboardWritten event must be emitted"
    );
  }

  #[tokio::test]
  async fn key_set_stop_condition_terminates_early() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-bb-keyset-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![
      // Agent A: write the target key, then final answer
      r#"{"thought":"writing","action":{"tool":"bb_write","params":{"key":"final","value":"done"}}}"#,
      r#"{"thought":"done","answer":"wrote"}"#,
      // Agent B should never run because key_set stop fires after A.
    ]);
    init_mock_model(&model).await;

    let mut supervisor = BlackboardSupervisorBuilder::new()
      .add_agent("a", "writer", {
        let model = model.clone();
        move |bb| build_agent(bb, "a", &model)
      })
      .add_agent("b", "would-be-second", {
        let model = model.clone();
        move |bb| build_agent(bb, "b", &model)
      })
      .stop_when(BlackboardStop::KeySet("final".into()))
      .answer_from("final")
      .build()
      .unwrap();

    let context = AgentContext::new("session-1", "go", &model);
    let result = AgentRuntime::run(&mut supervisor, context).await.unwrap();

    match &result.stop_reason {
      AgentStopReason::StopCondition { condition } => {
        assert!(condition.contains("'final'"));
      }
      other => panic!("expected StopCondition, got {other:?}"),
    }
    assert_eq!(result.answer.as_deref(), Some("done"));
  }

  #[tokio::test]
  async fn parallel_schedule_runs_both_agents_concurrently() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-bb-par-{}", uuid::Uuid::new_v4());
    // Both agents emit only an answer (no tool calls) so order of consumption
    // from the global FIFO queue does not matter for correctness.
    set_mock_responses(vec![
      r#"{"thought":"a done","answer":"answer a"}"#,
      r#"{"thought":"b done","answer":"answer b"}"#,
    ]);
    init_mock_model(&model).await;

    let mut supervisor = BlackboardSupervisorBuilder::new()
      .add_agent("a", "first", {
        let model = model.clone();
        move |bb| build_agent(bb, "a", &model)
      })
      .add_agent("b", "second", {
        let model = model.clone();
        move |bb| build_agent(bb, "b", &model)
      })
      .schedule(BlackboardSchedule::Parallel(vec!["a".into(), "b".into()]))
      .build()
      .unwrap();

    let context = AgentContext::new("session-1", "go", &model);
    let result = AgentRuntime::run(&mut supervisor, context).await.unwrap();

    // Both agents should have run (look for two FinalAnswer steps in merged trace).
    let final_steps = result
      .steps
      .iter()
      .filter(|s| matches!(s.kind, AgentStepKind::FinalAnswer { .. }))
      .count();
    assert_eq!(final_steps, 2, "parallel schedule must run both agents");
  }

  /// Q3.12.2 regression — a panic in a prior writer must not poison
  /// the version `Mutex` to the point that subsequent `write_*` calls
  /// also panic. Pre-fix `write_internal` used `.expect("blackboard
  /// version poisoned")`, so one bad agent killed every subsequent
  /// write. The fix recovers the lock via `into_inner()` (mirroring
  /// the `if let Ok` convention used for the entries/ops sites
  /// directly below it), so the second writer still observes the
  /// monotonic version counter and persists its entry.
  #[test]
  fn write_survives_prior_version_lock_panic() {
    let blackboard = Blackboard::new();

    // Intentionally poison the inner version mutex from a thread
    // that panics while holding the lock — this is the literal
    // failure mode `expect()` would have re-propagated.
    let counter = blackboard.next_version.clone();
    let panic_join = std::thread::spawn(move || {
      let _guard = counter.lock().expect("test pre-condition");
      panic!("intentional panic to poison the version mutex");
    })
    .join();
    assert!(
      panic_join.is_err(),
      "the worker thread must have panicked so the version mutex is poisoned"
    );
    assert!(
      blackboard.next_version.is_poisoned(),
      "Q3.12.2: pre-condition — mutex must report as poisoned"
    );

    // After the poison, a subsequent write must still land — no
    // panic, and the entry must be observable.
    blackboard.write_internal("after-poison", json!({"ok": true}), "agent-survivor");
    let entry = blackboard
      .get("after-poison")
      .expect("entry must be persisted after recovering from poison");
    assert_eq!(entry.value, json!({"ok": true}));
    assert_eq!(entry.written_by, "agent-survivor");
    assert_eq!(
      entry.version, 1,
      "version counter must still tick monotonically from 1 after lock recovery"
    );
  }

  #[tokio::test]
  async fn pre_cancelled_token_stops_blackboard_supervisor() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let model = format!("mock-bb-cancel-{}", uuid::Uuid::new_v4());
    set_mock_responses(vec![]);
    init_mock_model(&model).await;

    let mut supervisor = BlackboardSupervisorBuilder::new()
      .add_agent("a", "x", {
        let model = model.clone();
        move |bb| build_agent(bb, "a", &model)
      })
      .build()
      .unwrap();

    let token = crate::runtime::AgentCancellationToken::new();
    token.cancel();
    let context = AgentContext::new("s", "x", &model).with_cancellation_token(token);
    let result = AgentRuntime::run(&mut supervisor, context).await.unwrap();

    assert!(matches!(
      result.stop_reason,
      AgentStopReason::Cancelled { .. }
    ));
  }

  /// W3.2 regression: pre-fix, `add_agent`'s factory closure had to return
  /// a concrete `ReActAgent`. This stub needs no LLM at all, proving any
  /// `AgentRuntime` impl can now participate — it writes its answer to the
  /// blackboard directly via `BlackboardWriteTool` rather than going
  /// through an LLM tool-call loop.
  struct StubRuntime {
    write_tool: BlackboardWriteTool,
    answer: String,
  }
  #[async_trait]
  impl AgentRuntime for StubRuntime {
    async fn run(&mut self, context: AgentContext) -> Result<AgentRunResult, AgentRuntimeError> {
      self
        .write_tool
        .execute(json!({"key": "answer", "value": self.answer}))
        .await
        .map_err(|err| AgentRuntimeError::ExecutionFailed {
          message: err.to_string(),
        })?;
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
    let mut supervisor = BlackboardSupervisorBuilder::new()
      .add_agent("stub", "a non-ReActAgent participant", |bb| StubRuntime {
        write_tool: BlackboardWriteTool::new(bb, "stub"),
        answer: "stub answer".to_string(),
      })
      .answer_from("answer")
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
    let mut supervisor = BlackboardSupervisorBuilder::new()
      .add_agent("stub", "d", |bb| StubRuntime {
        write_tool: BlackboardWriteTool::new(bb, "stub"),
        answer: r#"{"ok":true}"#.to_string(),
      })
      .answer_from("answer")
      .with_delegation_spec("stub", spec)
      .build()
      .unwrap();
    supervisor.run("hello").await.unwrap();
    assert_eq!(
      supervisor.last_schema_validations().get("stub"),
      Some(&crate::delegation::SchemaValidation::Valid)
    );
  }

  #[test]
  fn build_rejects_delegation_spec_for_unknown_agent() {
    let result = BlackboardSupervisorBuilder::new()
      .add_agent("stub", "d", |bb| StubRuntime {
        write_tool: BlackboardWriteTool::new(bb, "stub"),
        answer: "x".to_string(),
      })
      .with_delegation_spec(
        "does-not-exist",
        crate::delegation::DelegationSpec::new("goal"),
      )
      .build();
    assert!(matches!(
      result,
      Err(BlackboardSupervisorError::UnknownDelegationSpecAgent(_))
    ));
  }
}
