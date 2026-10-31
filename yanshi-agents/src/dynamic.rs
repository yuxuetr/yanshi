//! Dynamic workflow — compile a declarative plan into an executable `Flow`.
//!
//! This is the productized form of `examples/dynamic_workflow_spike.rs` (P-A4.4).
//! Instead of an agent executing a plan step-by-step in its loop, it emits a
//! [`WorkflowPlan`] — the kind of JSON an LLM produces — which
//! [`compile_plan_to_flow`] turns into an `yanshi-graph` [`Flow`] of tool
//! calls. The core executor then runs it deterministically, inheriting
//! retry / checkpoint / timeout / tracing / replay, and — under
//! `FlowExecutionMode::Concurrent` — the parallelism the plan's `depends_on`
//! edges express.
//!
//! This is the "collapse many runtime LLM decisions into one up-front,
//! deterministically-executed artifact" pattern from the four-paradigm model
//! (`docs/ARCHITECTURE.md`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value};
use yanshi_graph::async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult};
use yanshi_graph::flow::{Flow, GraphNode, NodeType};
use yanshi_graph::{FlowRunner, FlowValue, YanshiError};
use yanshi_llm::{MultimodalMessage, Yanshi};
use yanshi_memory::SessionMemory;
use yanshi_tool::ToolRegistry;

use crate::nodes::AgentNode;
use crate::react::{ReActAgent, ReActConfig};
use crate::runtime::{AgentCancellationToken, AgentEventSink, RuntimeLimits};

/// One step of a declarative workflow plan — the shape an LLM emits as JSON:
///
/// ```json
/// { "id": "summarize", "tool": "llm", "params": {"prompt": "..."},
///   "depends_on": ["fetch"] }
/// ```
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepKind {
  /// Invoke a registered tool (the default). Uses `tool` + `params`; emits its
  /// output as `result`.
  #[default]
  Tool,
  /// Run an embedded ReAct agent step (P-A2.2). Uses `params.model` (required),
  /// `params.persona` (optional), and `params.prompt` (the agent's `message`,
  /// required); emits the agent's answer as `response`. The agent shares the
  /// plan's tool registry, so it inherits the same sandbox / approval governance.
  Agent,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowPlanStep {
  /// Unique step id (also the graph node id).
  pub id: String,
  /// What this step is: a tool call (default) or an embedded agent.
  #[serde(default)]
  pub kind: PlanStepKind,
  /// The registered tool to invoke (required for `kind = "tool"`; ignored for
  /// `kind = "agent"`).
  #[serde(default)]
  pub tool: String,
  /// Static parameters. For a tool step, merged with dependency outputs (each
  /// keyed by the dependency's step id) before the call. For an agent step,
  /// carries `model` / `persona` / `prompt`.
  #[serde(default)]
  pub params: Value,
  /// Ids of steps this one depends on; their output feeds this step (keyed by
  /// the dependency's step id), and the edges drive ordering + parallelism.
  #[serde(default)]
  pub depends_on: Vec<String>,
  /// Optional conditional-execution expression (W2.2), in the same `expr`
  /// syntax as config-YAML `run_if` (e.g. `"nodes.check.outputs.result ==
  /// 'ok'"`). Compiled and validated by [`compile_plan_to_flow_with_precomputed`]
  /// at compile time — an invalid expression is rejected before the plan
  /// ever runs, same as a dangling `depends_on` reference. When the
  /// expression evaluates falsy at run time, the executor skips the step
  /// (`YanshiError::NodeSkipped`) rather than treating it as a failure.
  /// Map/While loop steps remain out of scope for the dynamic-plan schema
  /// (kept deliberately narrow to bound LLM-authored plan complexity).
  #[serde(default)]
  pub run_if: Option<String>,
}

impl WorkflowPlanStep {
  /// The state-pool key this step emits its primary output under: tool steps
  /// emit `result`, agent steps emit `response`. Used to wire a dependent
  /// step's `input_mapping` to the correct key.
  fn output_key(&self) -> &'static str {
    match self.kind {
      PlanStepKind::Tool => "result",
      PlanStepKind::Agent => "response",
    }
  }
}

/// A declarative workflow plan: a DAG of tool calls. Steps with no dependency
/// path between them run concurrently under `FlowExecutionMode::Concurrent`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowPlan {
  /// The steps, in any order (dependencies are resolved by id, not position).
  pub steps: Vec<WorkflowPlanStep>,
}

/// A node that invokes a registered tool. Its static `params` are merged with
/// the outputs of its dependencies (each keyed by the dependency's step id)
/// before the call; the tool's textual `content` is emitted as `result`.
struct ToolCallNode {
  /// Step id, for diagnostics.
  id: String,
  registry: Arc<ToolRegistry>,
  tool: String,
  params: Value,
}

/// Convert a [`FlowValue`] to the raw JSON a tool expects. `FlowValue`'s own
/// `Serialize` is *tagged* (`{"type":"json",...}`), which is not what a tool
/// call wants, so unwrap to the inner value explicitly.
fn flow_value_to_raw_json(value: &FlowValue) -> Value {
  match value {
    FlowValue::Json(json) => json.clone(),
    FlowValue::File { path, .. } => Value::String(path.display().to_string()),
    FlowValue::Url { url, .. } => Value::String(url.clone()),
  }
}

#[async_trait::async_trait]
impl AsyncNode for ToolCallNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    // Start from the step's static params (must be a JSON object, or empty).
    let mut merged: Map<String, Value> = match &self.params {
      Value::Object(map) => map.clone(),
      Value::Null => Map::new(),
      other => {
        let mut map = Map::new();
        map.insert("params".to_string(), other.clone());
        map
      }
    };
    // Layer in dependency outputs, keyed by dependency step id.
    for (key, flow_value) in inputs {
      merged.insert(key.clone(), flow_value_to_raw_json(flow_value));
    }

    let output = self
      .registry
      .execute(&self.tool, Value::Object(merged))
      .await
      .map_err(|err| YanshiError::NodeExecutionFailed {
        message: format!("step '{}' tool '{}' failed: {err}", self.id, self.tool),
      })?;

    if output.is_error {
      return Err(YanshiError::NodeExecutionFailed {
        message: format!(
          "step '{}' tool '{}' reported an error: {}",
          self.id, self.tool, output.content
        ),
      });
    }

    let mut result = HashMap::new();
    result.insert(
      "result".to_string(),
      FlowValue::Json(Value::String(output.content)),
    );
    Ok(result)
  }
}

/// A node whose output is already known (L1.1). Used to represent a plan step
/// that already succeeded in an earlier `run_with_replan` round: it never
/// touches the tool registry or re-executes anything, it just replays the
/// previously-recorded result, so a revised plan reusing this id is a no-op
/// re-run rather than a real one.
struct PrecomputedResultNode {
  value: HashMap<String, FlowValue>,
}

#[async_trait::async_trait]
impl AsyncNode for PrecomputedResultNode {
  async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    Ok(self.value.clone())
  }
}

/// Suffix of the no-op node inserted after every `run_if` step that
/// something depends on. See [`compile_plan_to_flow_with_precomputed_and_governance`].
const SETTLED_SUFFIX: &str = "::settled";

/// An `agent` step: `AgentNode` reads only its `message` input, so the
/// prompt alone would leave the sub-agent blind to the outputs it was
/// ordered after (eval 2026-10-07 D2). Append whichever dependency
/// outputs arrived (a `run_if`-skipped dependency contributes nothing).
struct AgentStepNode {
  inner: AgentNode,
  prompt: String,
  depends_on: Vec<String>,
}

#[async_trait::async_trait]
impl AsyncNode for AgentStepNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let message = compose_agent_message(&self.prompt, &self.depends_on, inputs);
    let mut inputs = inputs.clone();
    inputs.insert(
      "message".to_string(),
      FlowValue::Json(Value::String(message)),
    );
    self.inner.execute(&inputs).await
  }
}

/// The state pool restricted to the plan's own steps, dropping the
/// internal `::settled` barrier nodes.
pub fn plan_step_results(
  plan: &WorkflowPlan,
  mut state: HashMap<String, AsyncNodeResult>,
) -> HashMap<String, AsyncNodeResult> {
  let ids: HashSet<&str> = plan.steps.iter().map(|step| step.id.as_str()).collect();
  state.retain(|id, _| ids.contains(id.as_str()));
  state
}

fn compose_agent_message(prompt: &str, depends_on: &[String], inputs: &AsyncNodeInputs) -> String {
  let context: Vec<String> = depends_on
    .iter()
    .filter_map(|dependency| {
      let rendered = match inputs.get(dependency)? {
        FlowValue::Json(Value::String(text)) => text.clone(),
        FlowValue::Json(value) => value.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
      };
      Some(format!("[{dependency}]\n{rendered}"))
    })
    .collect();
  if context.is_empty() {
    return prompt.to_string();
  }
  format!(
    "{prompt}\n\nOutputs of the steps this one depends on:\n\n{}",
    context.join("\n\n")
  )
}

/// Parent-flow governance forwarded into every `agent`-kind plan step
/// (W2.3), so a dynamic-plan sub-agent is cancellable, budget-constrained,
/// and Harness-visible instead of running in an isolated bubble with only
/// its own `ReActConfig`-derived defaults. `tool`-kind steps are
/// unaffected — they're already governed at the shared `ToolRegistry`
/// (sandbox policy, and Harness `wrap_registry` approval when configured).
///
/// `None`/default fields mean "no parent constraint of that kind"; a set
/// `limits` field narrows (never widens) each `agent` step's own default
/// limits field-by-field — see [`crate::nodes::AgentNode::with_governance`].
#[derive(Clone, Default)]
pub struct AgentStepGovernance {
  pub cancellation_token: Option<AgentCancellationToken>,
  pub event_sink: Option<Arc<dyn AgentEventSink>>,
  pub limits: Option<RuntimeLimits>,
}

/// Compile a [`WorkflowPlan`] into an executable [`Flow`].
///
/// Each step becomes a graph node that invokes its tool; `depends_on` becomes
/// graph dependencies (so independent steps run concurrently and dependents wait
/// for their inputs). Each dependency's `result` output is wired into the
/// dependent node's input pool keyed by the dependency's step id.
///
/// Run the result via `yanshi_core::FlowExt` — typically configured with
/// `FlowExecutionMode::Concurrent` to realize the plan's parallelism.
///
/// Validation is up front: duplicate step ids and dangling dependency
/// references are rejected as `FlowDefinitionError` (cycles are caught by the
/// executor's topological sort at run time).
pub fn compile_plan_to_flow(
  plan: &WorkflowPlan,
  registry: Arc<ToolRegistry>,
) -> Result<Flow, YanshiError> {
  compile_plan_to_flow_with_precomputed(plan, registry, &HashMap::new())
}

/// Same as [`compile_plan_to_flow`], but any step whose id is a key in
/// `precomputed` compiles to a [`PrecomputedResultNode`] instead of a real
/// tool/agent node — used by [`DynamicWorkflowAgent::run_with_replan`] (L1.1)
/// so a revised plan can carry forward already-succeeded steps without
/// re-running them. No parent [`AgentStepGovernance`] — equivalent to
/// [`compile_plan_to_flow_with_precomputed_and_governance`] with the
/// default (all-`None`) governance.
pub fn compile_plan_to_flow_with_precomputed(
  plan: &WorkflowPlan,
  registry: Arc<ToolRegistry>,
  precomputed: &HashMap<String, HashMap<String, FlowValue>>,
) -> Result<Flow, YanshiError> {
  compile_plan_to_flow_with_precomputed_and_governance(
    plan,
    registry,
    precomputed,
    &AgentStepGovernance::default(),
  )
}

/// Same as [`compile_plan_to_flow_with_precomputed`], additionally forwarding
/// `governance` (W2.3) into every `agent`-kind step's embedded [`AgentNode`]
/// via [`AgentNode::with_governance`].
pub fn compile_plan_to_flow_with_precomputed_and_governance(
  plan: &WorkflowPlan,
  registry: Arc<ToolRegistry>,
  precomputed: &HashMap<String, HashMap<String, FlowValue>>,
  governance: &AgentStepGovernance,
) -> Result<Flow, YanshiError> {
  let ids: HashSet<&str> = plan.steps.iter().map(|step| step.id.as_str()).collect();
  if ids.len() != plan.steps.len() {
    return Err(YanshiError::FlowDefinitionError {
      message: "workflow plan has duplicate step ids".to_string(),
    });
  }
  for step in &plan.steps {
    for dependency in &step.depends_on {
      if !ids.contains(dependency.as_str()) {
        return Err(YanshiError::FlowDefinitionError {
          message: format!(
            "step '{}' depends on unknown step '{}'",
            step.id, dependency
          ),
        });
      }
    }
    // Per-kind required fields.
    match step.kind {
      PlanStepKind::Tool => {
        if step.tool.trim().is_empty() {
          return Err(YanshiError::FlowDefinitionError {
            message: format!("tool step '{}' must name a tool", step.id),
          });
        }
      }
      PlanStepKind::Agent => {
        if step.params.get("model").and_then(Value::as_str).is_none() {
          return Err(YanshiError::FlowDefinitionError {
            message: format!("agent step '{}' requires params.model (a string)", step.id),
          });
        }
        if step.params.get("prompt").and_then(Value::as_str).is_none() {
          return Err(YanshiError::FlowDefinitionError {
            message: format!("agent step '{}' requires params.prompt (a string)", step.id),
          });
        }
      }
    }
    if let Some(run_if) = &step.run_if
      && let Err(err) = yanshi_graph::expr::compile(run_if)
    {
      return Err(YanshiError::FlowDefinitionError {
        message: format!("step '{}' run_if is invalid: {}", step.id, err),
      });
    }
  }

  // Each step's primary output key, so a dependent step wires to the right one
  // (`result` for a tool step, `response` for an agent step).
  let output_keys: HashMap<&str, &'static str> = plan
    .steps
    .iter()
    .map(|step| (step.id.as_str(), step.output_key()))
    .collect();

  // A dependency that carries `run_if` may be skipped. Core `Flow` fails a
  // node whose *listed* dependency it maps from was skipped, which used to
  // fail every step downstream of a benign skip (eval 2026-10-07 D1).
  // Instead the dependent orders itself after a no-op `<dep>::settled`
  // node and maps from `<dep>` without listing it — an unlisted mapping
  // source is optional in `Flow`, and the barrier keeps the ordering.
  let skippable: HashSet<&str> = plan
    .steps
    .iter()
    .filter(|step| step.run_if.is_some() && !precomputed.contains_key(&step.id))
    .map(|step| step.id.as_str())
    .collect();
  let settled = |dependency: &str| format!("{dependency}{SETTLED_SUFFIX}");

  let mut nodes: Vec<GraphNode> = plan
    .steps
    .iter()
    .map(|step| {
      // Wire each dependency's `result` output into this node's input pool,
      // keyed by the dependency's step id.
      let input_mapping = if step.depends_on.is_empty() {
        None
      } else {
        Some(
          step
            .depends_on
            .iter()
            .map(|dependency| {
              let key = output_keys
                .get(dependency.as_str())
                .copied()
                .unwrap_or("result");
              (dependency.clone(), (dependency.clone(), key.to_string()))
            })
            .collect(),
        )
      };

      let (node_type, initial_inputs): (NodeType, HashMap<String, FlowValue>) =
        if let Some(value) = precomputed.get(&step.id) {
          (
            NodeType::Standard(Arc::new(PrecomputedResultNode {
              value: value.clone(),
            })),
            HashMap::new(),
          )
        } else {
          match step.kind {
            PlanStepKind::Tool => (
              NodeType::Standard(Arc::new(ToolCallNode {
                id: step.id.clone(),
                registry: Arc::clone(&registry),
                tool: step.tool.clone(),
                params: step.params.clone(),
              })),
              HashMap::new(),
            ),
            PlanStepKind::Agent => {
              // Validated above: model + prompt are present strings.
              let model = step
                .params
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default();
              let prompt = step
                .params
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default();
              let mut config = ReActConfig::new(model);
              if let Some(persona) = step.params.get("persona").and_then(Value::as_str) {
                config = config.with_persona(persona);
              }
              let agent = ReActAgent::new(
                config,
                Box::new(SessionMemory::default_window()),
                Arc::clone(&registry),
              );
              let node = AgentNode::from_agent(step.id.clone(), agent).with_governance(
                governance.cancellation_token.clone(),
                governance.event_sink.clone(),
                governance.limits.clone(),
              );
              let node = AgentStepNode {
                inner: node,
                prompt: prompt.to_string(),
                depends_on: step.depends_on.clone(),
              };
              (NodeType::Standard(Arc::new(node)), HashMap::new())
            }
          }
        };

      let dependencies = step
        .depends_on
        .iter()
        .map(|dependency| {
          if skippable.contains(dependency.as_str()) {
            settled(dependency)
          } else {
            dependency.clone()
          }
        })
        .collect();

      GraphNode {
        id: step.id.clone(),
        node_type,
        dependencies,
        input_mapping,
        run_if: step.run_if.clone(),
        initial_inputs,
      }
    })
    .collect();

  let depended_on: HashSet<&str> = plan
    .steps
    .iter()
    .flat_map(|step| step.depends_on.iter().map(String::as_str))
    .collect();
  for id in skippable.intersection(&depended_on) {
    nodes.push(GraphNode {
      id: settled(id),
      node_type: NodeType::Standard(Arc::new(PrecomputedResultNode {
        value: HashMap::new(),
      })),
      dependencies: vec![(*id).to_string()],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    });
  }

  Ok(Flow::new(nodes))
}

/// Errors from the LLM-driven [`DynamicWorkflowAgent`].
#[derive(Debug, thiserror::Error)]
pub enum DynamicWorkflowError {
  /// The LLM planning call failed.
  #[error("LLM planning call failed: {0}")]
  Llm(String),
  /// The LLM's reply could not be parsed as a `WorkflowPlan`.
  #[error("could not parse the LLM plan as JSON: {0}")]
  PlanParse(String),
  /// Compiling or executing the plan failed.
  #[error(transparent)]
  Flow(#[from] YanshiError),
}

/// An agent that **generates** a workflow plan with an LLM, compiles it to a
/// `Flow`, and executes it — the full dynamic-workflow paradigm (P-A4.4).
///
/// This is the binding-time inversion the four-paradigm model describes: instead
/// of an agent deciding every step in a loop, it makes *one* up-front planning
/// call, then the engine runs the resulting DAG deterministically (and in
/// parallel where the plan allows).
pub struct DynamicWorkflowAgent {
  model: String,
  tools: Arc<ToolRegistry>,
  runner: Arc<dyn FlowRunner>,
  /// W2.3: forwarded into every `agent`-kind plan step; defaults to no
  /// governance (pre-W2.3 behavior) — see [`Self::with_governance`].
  governance: AgentStepGovernance,
}

/// Pull the JSON object out of an LLM reply (it may wrap it in prose or a
/// ```json fence): take the span from the first `{` to the last `}`.
fn extract_json(text: &str) -> &str {
  match (text.find('{'), text.rfind('}')) {
    (Some(start), Some(end)) if end >= start => &text[start..=end],
    _ => text,
  }
}

impl DynamicWorkflowAgent {
  /// Build an agent that plans with `model`, executes against `tools`, and
  /// runs the compiled `Flow` via the injected `runner` (the surface passes
  /// `yanshi_core::CoreFlowRunner::concurrent(...)`).
  pub fn new(
    model: impl Into<String>,
    tools: Arc<ToolRegistry>,
    runner: Arc<dyn FlowRunner>,
  ) -> Self {
    Self {
      model: model.into(),
      tools,
      runner,
      governance: AgentStepGovernance::default(),
    }
  }

  /// Forward `governance` (cancellation token / event sink / resource
  /// limits) into every `agent`-kind plan step this agent compiles from
  /// here on (W2.3). Defaults to no governance, matching pre-W2.3
  /// behavior, until a caller opts in.
  pub fn with_governance(mut self, governance: AgentStepGovernance) -> Self {
    self.governance = governance;
    self
  }

  /// Ask the LLM for a [`WorkflowPlan`] for `goal`, given the available tools.
  pub async fn plan(&self, goal: &str) -> Result<WorkflowPlan, DynamicWorkflowError> {
    let tools_desc: String = self
      .tools
      .list()
      .iter()
      .map(|tool| format!("- {}: {}", tool.name(), tool.description()))
      .collect::<Vec<_>>()
      .join("\n");
    let system = "You are a workflow planner. Given a goal and a list of tools, \
      respond with ONLY a JSON object of the form \
      {\"steps\":[{\"id\":\"...\",\"tool\":\"...\",\"params\":{...},\"depends_on\":[\"...\"]}]}. \
      Steps with no dependency between them run in parallel; use depends_on to \
      order dependent steps. Use only the listed tools. For a step that needs \
      open-ended reasoning rather than a single tool call, emit an agent step: \
      {\"id\":\"...\",\"kind\":\"agent\",\"params\":{\"model\":\"<model>\",\"prompt\":\"<instruction>\"},\"depends_on\":[...]} \
      (it runs a sub-agent that may call the same tools). Prefer plain tool steps; use agent steps sparingly.";
    let user = format!("Goal: {goal}\n\nAvailable tools:\n{tools_desc}");

    let response = Yanshi::model(&self.model)
      .multimodal_messages(vec![
        MultimodalMessage::text("system", system),
        MultimodalMessage::text("user", user),
      ])
      .execute_full()
      .await
      .map_err(|err| DynamicWorkflowError::Llm(err.to_string()))?;

    serde_json::from_str(extract_json(&response.content))
      .map_err(|err| DynamicWorkflowError::PlanParse(err.to_string()))
  }

  /// Plan with the LLM, compile to a `Flow`, and execute it concurrently.
  /// Returns the executed state pool (node id → result).
  pub async fn run(
    &self,
    goal: &str,
  ) -> Result<HashMap<String, AsyncNodeResult>, DynamicWorkflowError> {
    let plan = self.plan(goal).await?;
    let flow = compile_plan_to_flow_with_precomputed_and_governance(
      &plan,
      Arc::clone(&self.tools),
      &HashMap::new(),
      &self.governance,
    )?;
    let state = self.runner.run(&flow, HashMap::new()).await?;
    Ok(plan_step_results(&plan, state))
  }

  /// Like [`Self::run`], but recovers from node failures instead of
  /// stopping at the first one (L1.1).
  ///
  /// After each attempt, any plan step without a successful result —
  /// whether it failed outright or was never attempted because an
  /// upstream dependency failed — is treated as still outstanding.
  /// Already-succeeded steps are never re-run: their results are carried
  /// forward as [`PrecomputedResultNode`]s, and the planner is asked for a
  /// revised plan covering only what's still missing, given the goal, the
  /// ids (and results) already available, and why the rest failed.
  /// Repeats until everything succeeds or `max_replans` revisions have
  /// been spent, at which point the last attempt's (partial) state is
  /// returned as-is — same "no top-level `Err` for node failures"
  /// contract as [`Self::run`].
  pub async fn run_with_replan(
    &self,
    goal: &str,
    max_replans: usize,
  ) -> Result<DynamicWorkflowRunOutcome, DynamicWorkflowError> {
    let mut plan = self.plan(goal).await?;
    let mut precomputed: HashMap<String, HashMap<String, FlowValue>> = HashMap::new();
    let mut completed_steps: HashMap<String, WorkflowPlanStep> = HashMap::new();
    let mut revisions = 0usize;

    loop {
      let flow = compile_plan_to_flow_with_precomputed_and_governance(
        &plan,
        Arc::clone(&self.tools),
        &precomputed,
        &self.governance,
      )?;
      let state = self.runner.run(&flow, HashMap::new()).await?;

      let mut failures: Vec<(String, String)> = Vec::new();
      let mut outstanding = false;
      for step in &plan.steps {
        match state.get(&step.id) {
          Some(Ok(value)) => {
            precomputed.insert(step.id.clone(), value.clone());
            completed_steps.insert(step.id.clone(), step.clone());
          }
          // A `run_if` skip is a decision, not a failure: no replan. Kept
          // in the plan (not precomputed) so a revised plan may still
          // depend on it and its `run_if` is evaluated again.
          Some(Err(YanshiError::NodeSkipped)) => {
            completed_steps.insert(step.id.clone(), step.clone());
          }
          Some(Err(err)) => {
            failures.push((step.id.clone(), err.to_string()));
            outstanding = true;
          }
          None => {
            outstanding = true;
          }
        }
      }

      if !outstanding || revisions >= max_replans {
        return Ok(DynamicWorkflowRunOutcome {
          state: plan_step_results(&plan, state),
          plan,
          revisions,
        });
      }

      revisions += 1;
      tracing::info!(
        event = "dynamic_workflow_plan_revision",
        revision = revisions,
        failed_steps = ?failures.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        completed_step_count = completed_steps.len(),
        "Revising dynamic workflow plan after failures"
      );
      let completed_ids: Vec<&str> = completed_steps.keys().map(String::as_str).collect();
      let revised = self.replan(goal, &completed_ids, &failures).await?;

      // Merge: every completed step (unchanged, becomes a precomputed
      // pass-through node) + this round's replacement steps for whatever
      // is still missing.
      let mut merged_steps: Vec<WorkflowPlanStep> = completed_steps.values().cloned().collect();
      merged_steps.extend(revised.steps);
      plan = WorkflowPlan {
        steps: merged_steps,
      };
    }
  }

  /// Ask the LLM for a revised [`WorkflowPlan`] covering only the steps
  /// still needed after a partial failure (L1.1). `completed_ids` are
  /// steps a revised plan may `depends_on` but must not redefine — their
  /// results are already fixed. `failures` are `(step_id, error message)`
  /// pairs for steps that failed outright; steps that were merely blocked
  /// by an upstream failure aren't in `failures` at all (they were simply
  /// never attempted).
  async fn replan(
    &self,
    goal: &str,
    completed_ids: &[&str],
    failures: &[(String, String)],
  ) -> Result<WorkflowPlan, DynamicWorkflowError> {
    let tools_desc: String = self
      .tools
      .list()
      .iter()
      .map(|tool| format!("- {}: {}", tool.name(), tool.description()))
      .collect::<Vec<_>>()
      .join("\n");
    let system = "You are a workflow planner revising a plan after a partial failure. \
      Some steps from the prior plan already succeeded and must NOT be redefined — you may \
      reference their ids in depends_on, their results are already available. Respond with \
      ONLY a JSON object {\"steps\":[{\"id\":\"...\",\"tool\":\"...\",\"params\":{...},\"depends_on\":[\"...\"]}]} \
      containing ONLY the new steps needed to reach the goal, given what already succeeded and \
      why the rest failed. Use only the listed tools.";
    let completed_desc = if completed_ids.is_empty() {
      "(none)".to_string()
    } else {
      completed_ids.join(", ")
    };
    let failures_desc = if failures.is_empty() {
      "(none of the outstanding steps failed outright — they were simply never attempted \
       because an earlier dependency failed)"
        .to_string()
    } else {
      failures
        .iter()
        .map(|(id, err)| format!("- step '{id}' failed: {err}"))
        .collect::<Vec<_>>()
        .join("\n")
    };
    let user = format!(
      "Goal: {goal}\n\nAlready succeeded (do not redefine, may depend_on): {completed_desc}\n\n\
       Failures to address:\n{failures_desc}\n\nAvailable tools:\n{tools_desc}"
    );

    let response = Yanshi::model(&self.model)
      .multimodal_messages(vec![
        MultimodalMessage::text("system", system),
        MultimodalMessage::text("user", user),
      ])
      .execute_full()
      .await
      .map_err(|err| DynamicWorkflowError::Llm(err.to_string()))?;

    serde_json::from_str(extract_json(&response.content))
      .map_err(|err| DynamicWorkflowError::PlanParse(err.to_string()))
  }
}

/// Outcome of [`DynamicWorkflowAgent::run_with_replan`]: the final executed
/// state pool, the plan that actually produced it (merged across every
/// revision — completed steps from earlier rounds plus each round's
/// replacement steps for whatever was still missing), and how many
/// revisions it took (`0` means the first attempt already succeeded).
#[derive(Debug)]
pub struct DynamicWorkflowRunOutcome {
  pub state: HashMap<String, AsyncNodeResult>,
  pub plan: WorkflowPlan,
  pub revisions: usize,
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use yanshi_core::CoreFlowRunner;
  use yanshi_tool::{Tool, ToolError, ToolMetadata, ToolOutput};

  /// Echoes its received params back as JSON `content` — lets a test trace
  /// exactly what reached each tool call (static params + dependency outputs).
  struct EchoTool;

  #[async_trait::async_trait]
  impl Tool for EchoTool {
    fn name(&self) -> &str {
      "echo"
    }
    fn description(&self) -> &str {
      "echoes its params as JSON"
    }
    fn parameters_schema(&self) -> Value {
      json!({ "type": "object" })
    }
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata::builtin_named("echo")
    }
    async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
      Ok(ToolOutput::success(params.to_string()))
    }
  }

  fn registry_with_echo() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    Arc::new(registry)
  }

  fn result_text(state: &HashMap<String, AsyncNodeResult>, node: &str) -> String {
    match state
      .get(node)
      .and_then(|r| r.as_ref().ok())
      .and_then(|o| o.get("result"))
    {
      Some(FlowValue::Json(Value::String(s))) => s.clone(),
      other => panic!("node '{node}' produced unexpected result: {other:?}"),
    }
  }

  #[tokio::test]
  async fn diamond_plan_runs_with_dependencies_wired() {
    // a, b run independently (concurrently); c depends on both and sees their
    // outputs keyed by step id.
    let plan = WorkflowPlan {
      steps: vec![
        WorkflowPlanStep {
          id: "a".into(),
          kind: PlanStepKind::Tool,
          tool: "echo".into(),
          params: json!({"v": "A"}),
          depends_on: vec![],
          run_if: None,
        },
        WorkflowPlanStep {
          id: "b".into(),
          kind: PlanStepKind::Tool,
          tool: "echo".into(),
          params: json!({"v": "B"}),
          depends_on: vec![],
          run_if: None,
        },
        WorkflowPlanStep {
          id: "c".into(),
          kind: PlanStepKind::Tool,
          tool: "echo".into(),
          params: json!({}),
          depends_on: vec!["a".into(), "b".into()],
          run_if: None,
        },
      ],
    };
    let flow = compile_plan_to_flow(&plan, registry_with_echo()).expect("compile");

    let state = CoreFlowRunner::concurrent(8)
      .run(&flow, HashMap::new())
      .await
      .expect("run");

    // a and b echoed their own static params.
    assert!(result_text(&state, "a").contains("\"v\":\"A\""));
    assert!(result_text(&state, "b").contains("\"v\":\"B\""));
    // c received BOTH dependency results, keyed by step id.
    let c = result_text(&state, "c");
    assert!(c.contains("\"a\":"), "c should see a's output: {c}");
    assert!(c.contains("\"b\":"), "c should see b's output: {c}");
    assert!(
      c.contains('A') && c.contains('B'),
      "c should carry both payloads: {c}"
    );
  }

  // ── W2.2: run_if conditional steps ──────────────────────────────────

  #[tokio::test]
  async fn run_if_false_skips_the_step() {
    let plan = WorkflowPlan {
      steps: vec![WorkflowPlanStep {
        id: "a".into(),
        kind: PlanStepKind::Tool,
        tool: "echo".into(),
        params: json!({}),
        depends_on: vec![],
        run_if: Some("false".into()),
      }],
    };
    let flow = compile_plan_to_flow(&plan, registry_with_echo()).expect("compiles");
    let state = CoreFlowRunner::concurrent(8)
      .run(&flow, HashMap::new())
      .await
      .expect("run");
    assert!(
      matches!(state.get("a"), Some(Err(YanshiError::NodeSkipped))),
      "a run_if that evaluates false must skip the step rather than run it: {:?}",
      state.get("a")
    );
  }

  #[tokio::test]
  async fn run_if_true_runs_the_step() {
    let plan = WorkflowPlan {
      steps: vec![WorkflowPlanStep {
        id: "a".into(),
        kind: PlanStepKind::Tool,
        tool: "echo".into(),
        params: json!({}),
        depends_on: vec![],
        run_if: Some("true".into()),
      }],
    };
    let flow = compile_plan_to_flow(&plan, registry_with_echo()).expect("compiles");
    let state = CoreFlowRunner::concurrent(8)
      .run(&flow, HashMap::new())
      .await
      .expect("run");
    assert!(
      state.get("a").is_some_and(|r| r.is_ok()),
      "a run_if that evaluates true must not skip the step: {:?}",
      state.get("a")
    );
  }

  /// Eval 2026-10-07 D1: a skipped dependency used to fail every
  /// downstream step (`DependencyNotMet`) and trigger a replan.
  #[tokio::test]
  async fn skipped_dependency_leaves_downstream_steps_running() {
    let plan = WorkflowPlan {
      steps: vec![
        WorkflowPlanStep {
          id: "maybe".into(),
          kind: PlanStepKind::Tool,
          tool: "echo".into(),
          params: json!({"v": "M"}),
          depends_on: vec![],
          run_if: Some("false".into()),
        },
        WorkflowPlanStep {
          id: "always".into(),
          kind: PlanStepKind::Tool,
          tool: "echo".into(),
          params: json!({"v": "A"}),
          depends_on: vec![],
          run_if: None,
        },
        WorkflowPlanStep {
          id: "join".into(),
          kind: PlanStepKind::Tool,
          tool: "echo".into(),
          params: json!({}),
          depends_on: vec!["maybe".into(), "always".into()],
          run_if: None,
        },
      ],
    };
    let flow = compile_plan_to_flow(&plan, registry_with_echo()).expect("compile");
    for runner in [CoreFlowRunner::serial(), CoreFlowRunner::concurrent(8)] {
      let state = runner.run(&flow, HashMap::new()).await.expect("run");
      assert!(matches!(
        state.get("maybe"),
        Some(Err(YanshiError::NodeSkipped))
      ));
      let join = result_text(&state, "join");
      assert!(join.contains("\"always\":"), "{join}");
      assert!(!join.contains("\"maybe\":"), "{join}");
    }
  }

  /// Eval 2026-10-07 D2: an agent step only ever saw its static prompt.
  #[test]
  fn agent_message_carries_the_dependency_outputs_that_arrived() {
    let inputs: AsyncNodeInputs = HashMap::from([
      (
        "research".to_string(),
        FlowValue::Json(json!("three findings")),
      ),
      ("stats".to_string(), FlowValue::Json(json!({"n": 3}))),
    ]);
    let deps = vec![
      "research".to_string(),
      "stats".to_string(),
      "skipped".to_string(),
    ];
    let message = compose_agent_message("Summarise.", &deps, &inputs);
    assert!(message.starts_with("Summarise."));
    assert!(message.contains("[research]\nthree findings"), "{message}");
    assert!(message.contains("[stats]\n{\"n\":3}"), "{message}");
    assert!(!message.contains("[skipped]"), "{message}");
    assert_eq!(compose_agent_message("Solo.", &[], &inputs), "Solo.");
  }

  #[test]
  fn rejects_invalid_run_if_expression() {
    let plan = WorkflowPlan {
      steps: vec![WorkflowPlanStep {
        id: "a".into(),
        kind: PlanStepKind::Tool,
        tool: "echo".into(),
        params: json!({}),
        depends_on: vec![],
        run_if: Some("1 +".into()),
      }],
    };
    let result = compile_plan_to_flow(&plan, registry_with_echo());
    assert!(
      matches!(result, Err(YanshiError::FlowDefinitionError { .. })),
      "an invalid run_if expression must be rejected at compile time (FlowDefinitionError), \
       got is_ok={}",
      result.is_ok()
    );
  }

  #[test]
  fn rejects_dangling_dependency() {
    let plan = WorkflowPlan {
      steps: vec![WorkflowPlanStep {
        id: "x".into(),
        kind: PlanStepKind::Tool,
        tool: "echo".into(),
        params: json!({}),
        depends_on: vec!["missing".into()],
        run_if: None,
      }],
    };
    let result = compile_plan_to_flow(&plan, registry_with_echo());
    assert!(matches!(
      result,
      Err(YanshiError::FlowDefinitionError { .. })
    ));
  }

  #[test]
  fn rejects_duplicate_ids() {
    let step = WorkflowPlanStep {
      id: "dup".into(),
      kind: PlanStepKind::Tool,
      tool: "echo".into(),
      params: json!({}),
      depends_on: vec![],
      run_if: None,
    };
    let plan = WorkflowPlan {
      steps: vec![step.clone(), step],
    };
    let result = compile_plan_to_flow(&plan, registry_with_echo());
    assert!(matches!(
      result,
      Err(YanshiError::FlowDefinitionError { .. })
    ));
  }

  /// Point a mock text model at `response` and load it as the active config.
  /// Serialized by `crate::LLM_TEST_LOCK` (mutates process-wide LLM config/env).
  async fn init_mock_model(model: &str, response: &str) {
    // SAFETY: callers hold LLM_TEST_LOCK while mutating these process-wide vars.
    unsafe {
      std::env::set_var("YANSHI_MOCK_RESPONSE", response);
    }
    let config_path =
      std::env::temp_dir().join(format!("yanshi-dyn-wf-{}.yml", uuid::Uuid::new_v4()));
    std::fs::write(
      &config_path,
      format!(
        "models:\n  {model}:\n    vendor: mock\n    type: text\n    model_id: {model}\n\
         providers:\n  mock:\n    api_key_env: MOCK_API_KEY\n"
      ),
    )
    .expect("write mock config");
    yanshi_llm::Yanshi::init_with_config(config_path.to_str().expect("utf8 path"))
      .await
      .expect("init mock model");
  }

  /// Clears `YANSHI_MOCK_RESPONSES` on drop (including on panic/unwind)
  /// so a queue set up by one test can never leak into whichever test
  /// acquires `LLM_TEST_LOCK` next — `YANSHI_MOCK_RESPONSES` takes
  /// priority over `YANSHI_MOCK_RESPONSE` in the mock provider, so a
  /// stale queue silently hijacks every subsequent mock-model test in the
  /// process until the binary exits.
  struct MockResponsesGuard;
  impl Drop for MockResponsesGuard {
    fn drop(&mut self) {
      // SAFETY: guarded by LLM_TEST_LOCK for the guard's whole lifetime.
      unsafe {
        std::env::remove_var("YANSHI_MOCK_RESPONSES");
      }
    }
  }

  /// Same as [`init_mock_model`], but queues multiple responses consumed
  /// FIFO across successive LLM calls (`YANSHI_MOCK_RESPONSES`) — used
  /// by replan tests where `plan()` and `replan()` each need a distinct
  /// canned response. Returns a guard that clears the queue env var when
  /// dropped, even if the test panics.
  #[must_use]
  async fn init_mock_model_with_responses(model: &str, responses: &[&str]) -> MockResponsesGuard {
    // SAFETY: callers hold LLM_TEST_LOCK while mutating these process-wide vars.
    unsafe {
      std::env::set_var(
        "YANSHI_MOCK_RESPONSES",
        serde_json::to_string(responses).expect("serialize mock responses"),
      );
      std::env::remove_var("YANSHI_MOCK_RESPONSE");
    }
    let config_path =
      std::env::temp_dir().join(format!("yanshi-dyn-wf-{}.yml", uuid::Uuid::new_v4()));
    std::fs::write(
      &config_path,
      format!(
        "models:\n  {model}:\n    vendor: mock\n    type: text\n    model_id: {model}\n\
         providers:\n  mock:\n    api_key_env: MOCK_API_KEY\n"
      ),
    )
    .expect("write mock config");
    yanshi_llm::Yanshi::init_with_config(config_path.to_str().expect("utf8 path"))
      .await
      .expect("init mock model");
    MockResponsesGuard
  }

  #[tokio::test]
  async fn llm_plans_then_engine_executes_in_parallel() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    // The "LLM" returns a parallel plan: a and b are independent, c joins them.
    init_mock_model(
      "mock-dyn-wf",
      r#"{"steps":[
        {"id":"a","tool":"echo","params":{"v":"A"}},
        {"id":"b","tool":"echo","params":{"v":"B"}},
        {"id":"c","tool":"echo","depends_on":["a","b"]}
      ]}"#,
    )
    .await;

    let agent = DynamicWorkflowAgent::new(
      "mock-dyn-wf",
      registry_with_echo(),
      Arc::new(CoreFlowRunner::concurrent(8)),
    );
    let state = agent.run("combine A and B").await.expect("run");

    assert!(result_text(&state, "a").contains('A'));
    assert!(result_text(&state, "b").contains('B'));
    let c = result_text(&state, "c");
    assert!(
      c.contains("\"a\":") && c.contains("\"b\":"),
      "c should receive both planned dependencies: {c}"
    );
  }

  // ── P-A2.2: agent steps in a plan ─────────────────────────────────────────

  fn agent_step(id: &str, params: Value, deps: Vec<&str>) -> WorkflowPlanStep {
    WorkflowPlanStep {
      id: id.into(),
      kind: PlanStepKind::Agent,
      tool: String::new(),
      params,
      depends_on: deps.into_iter().map(String::from).collect(),
      run_if: None,
    }
  }

  #[test]
  fn agent_step_compiles_to_a_flow_node() {
    let plan = WorkflowPlan {
      steps: vec![agent_step(
        "think",
        json!({"model": "m", "prompt": "hello", "persona": "be terse"}),
        vec![],
      )],
    };
    let flow = compile_plan_to_flow(&plan, registry_with_echo()).expect("compiles");
    assert_eq!(flow.nodes().len(), 1);
  }

  /// W2.3 regression: pre-fix, `compile_plan_to_flow` always hardcoded
  /// `AgentNode::from_agent` with no governance, so an `agent`-kind plan
  /// step could not be cancelled by whatever cancelled the parent run — it
  /// built its own isolated `AgentContext` via `run_with_trace` and just
  /// kept going. A cancellation check runs before `ReActAgent` ever calls
  /// the model, so a pre-cancelled token proves propagation without
  /// needing a real/mocked LLM at all: if governance weren't forwarded,
  /// this step would instead hang waiting on (or error out reaching) a
  /// nonexistent "any-model" provider.
  #[tokio::test]
  async fn agent_step_honors_a_shared_parent_cancellation_token() {
    let token = AgentCancellationToken::new();
    token.cancel();
    let governance = AgentStepGovernance {
      cancellation_token: Some(token),
      event_sink: None,
      limits: None,
    };
    let plan = WorkflowPlan {
      steps: vec![agent_step(
        "think",
        json!({"model": "any-model", "prompt": "hello"}),
        vec![],
      )],
    };
    let flow = compile_plan_to_flow_with_precomputed_and_governance(
      &plan,
      registry_with_echo(),
      &HashMap::new(),
      &governance,
    )
    .expect("compiles");
    let state = CoreFlowRunner::concurrent(8)
      .run(&flow, HashMap::new())
      .await
      .expect("run");

    match state.get("think") {
      Some(Err(YanshiError::NodePartialExecutionFailed { message, .. })) => {
        assert!(
          message.to_lowercase().contains("cancel"),
          "expected a cancellation stop reason, got: {message}"
        );
      }
      other => panic!("expected a cancelled partial-execution failure, got {other:?}"),
    }
  }

  #[test]
  fn agent_step_requires_model_and_prompt() {
    for bad in [json!({"prompt": "hi"}), json!({"model": "m"})] {
      let plan = WorkflowPlan {
        steps: vec![agent_step("a", bad, vec![])],
      };
      assert!(
        matches!(
          compile_plan_to_flow(&plan, registry_with_echo()),
          Err(YanshiError::FlowDefinitionError { .. })
        ),
        "agent step missing model or prompt must be rejected"
      );
    }
  }

  #[test]
  fn tool_step_with_empty_tool_is_rejected() {
    let plan = WorkflowPlan {
      steps: vec![WorkflowPlanStep {
        id: "x".into(),
        kind: PlanStepKind::Tool,
        tool: String::new(),
        params: json!({}),
        depends_on: vec![],
        run_if: None,
      }],
    };
    assert!(matches!(
      compile_plan_to_flow(&plan, registry_with_echo()),
      Err(YanshiError::FlowDefinitionError { .. })
    ));
  }

  /// The key correctness check: a tool step depending on an *agent* step must
  /// wire its input_mapping to the agent's `response` output, not `result`.
  #[test]
  fn dependent_step_wires_to_the_agent_response_key() {
    let plan = WorkflowPlan {
      steps: vec![
        agent_step("plan", json!({"model": "m", "prompt": "p"}), vec![]),
        WorkflowPlanStep {
          id: "use".into(),
          kind: PlanStepKind::Tool,
          tool: "echo".into(),
          params: json!({}),
          depends_on: vec!["plan".into()],
          run_if: None,
        },
      ],
    };
    let flow = compile_plan_to_flow(&plan, registry_with_echo()).expect("compiles");
    let mapping = flow
      .nodes()
      .get("use")
      .expect("use node")
      .input_mapping
      .as_ref()
      .expect("use has an input mapping");
    assert_eq!(
      mapping.get("plan"),
      Some(&("plan".to_string(), "response".to_string())),
      "a dependent of an agent step must read its `response` output"
    );
  }

  // ── L1.1: failure-driven replan loop ────────────────────────────────────

  /// Counts invocations so tests can prove a precomputed step is never
  /// re-run across replan rounds.
  struct CountingEchoTool {
    calls: Arc<std::sync::atomic::AtomicUsize>,
  }

  #[async_trait::async_trait]
  impl Tool for CountingEchoTool {
    fn name(&self) -> &str {
      "counting_echo"
    }
    fn description(&self) -> &str {
      "echoes its params as JSON and counts how many times it ran"
    }
    fn parameters_schema(&self) -> Value {
      json!({ "type": "object" })
    }
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata::builtin_named("counting_echo")
    }
    async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
      self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
      Ok(ToolOutput::success(params.to_string()))
    }
  }

  /// Always reports a tool-level error — simulates a step that fails
  /// outright (as opposed to the tool call itself erroring at the
  /// transport level).
  struct FailTool;

  #[async_trait::async_trait]
  impl Tool for FailTool {
    fn name(&self) -> &str {
      "fail"
    }
    fn description(&self) -> &str {
      "always reports failure"
    }
    fn parameters_schema(&self) -> Value {
      json!({ "type": "object" })
    }
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata::builtin_named("fail")
    }
    async fn execute(&self, _params: Value) -> Result<ToolOutput, ToolError> {
      Ok(ToolOutput::error("boom"))
    }
  }

  /// `compile_plan_to_flow_with_precomputed` in isolation: a precomputed
  /// step's node replays the stored value and never touches the registry
  /// (a registry with no tools at all still compiles and runs fine).
  #[tokio::test]
  async fn precomputed_step_replays_stored_value_without_touching_registry() {
    let plan = WorkflowPlan {
      steps: vec![WorkflowPlanStep {
        id: "a".into(),
        kind: PlanStepKind::Tool,
        tool: "does-not-exist".into(),
        params: json!({}),
        depends_on: vec![],
        run_if: None,
      }],
    };
    let precomputed = HashMap::from([(
      "a".to_string(),
      HashMap::from([(
        "result".to_string(),
        FlowValue::Json(Value::String("cached".to_string())),
      )]),
    )]);

    let empty_registry = Arc::new(ToolRegistry::new());
    let flow = compile_plan_to_flow_with_precomputed(&plan, empty_registry, &precomputed)
      .expect("compiles even though 'does-not-exist' is never registered");
    let state = CoreFlowRunner::concurrent(1)
      .run(&flow, HashMap::new())
      .await
      .expect("run");

    assert_eq!(result_text(&state, "a"), "cached");
  }

  /// L1.1 regression: a step that fails is replanned; the step that
  /// already succeeded is carried forward and never re-executed.
  #[tokio::test]
  async fn run_with_replan_reuses_completed_steps_and_recovers_from_failure() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let _responses_guard = init_mock_model_with_responses(
      "mock-replan",
      &[
        // Round 1: 'a' will succeed, 'b' is doomed.
        r#"{"steps":[
          {"id":"a","tool":"counting_echo","params":{"v":"A"}},
          {"id":"b","tool":"fail","params":{}}
        ]}"#,
        // Round 2 (replan): only the replacement for 'b' — must NOT
        // redefine 'a'.
        r#"{"steps":[{"id":"b2","tool":"echo","params":{"v":"B-fixed"}}]}"#,
      ],
    )
    .await;

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    registry.register(Arc::new(CountingEchoTool {
      calls: calls.clone(),
    }));
    registry.register(Arc::new(FailTool));

    let agent = DynamicWorkflowAgent::new(
      "mock-replan",
      Arc::new(registry),
      Arc::new(CoreFlowRunner::concurrent(8)),
    );

    let outcome = agent
      .run_with_replan("do a then b", 3)
      .await
      .expect("run_with_replan");

    assert_eq!(outcome.revisions, 1);
    assert_eq!(
      calls.load(std::sync::atomic::Ordering::SeqCst),
      1,
      "step 'a' must run exactly once across both rounds, never re-run"
    );
    assert!(result_text(&outcome.state, "a").contains('A'));
    assert!(result_text(&outcome.state, "b2").contains("B-fixed"));
    assert!(
      !outcome.state.contains_key("b"),
      "the failed step id from round 1 must not reappear in the final state"
    );
    let final_ids: HashSet<&str> = outcome.plan.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(final_ids, HashSet::from(["a", "b2"]));
  }

  /// When every attempt keeps failing, `run_with_replan` gives up after
  /// `max_replans` revisions and returns the last (partial) state rather
  /// than looping forever or erroring out — same "no top-level Err for
  /// node failures" contract as `run`.
  /// Eval 2026-10-07 D1: a `run_if` skip used to count as a failure and
  /// trigger a pointless replan.
  #[tokio::test]
  async fn run_with_replan_does_not_replan_for_a_skipped_step() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let _responses_guard = init_mock_model_with_responses(
      "mock-replan-skip",
      &[r#"{"steps":[
          {"id":"a","tool":"echo","params":{"v":"A"},"run_if":"false"},
          {"id":"b","tool":"echo","params":{},"depends_on":["a"]}
        ]}"#],
    )
    .await;
    let agent = DynamicWorkflowAgent::new(
      "mock-replan-skip",
      registry_with_echo(),
      Arc::new(CoreFlowRunner::concurrent(8)),
    );
    let outcome = agent
      .run_with_replan("maybe a, then b", 3)
      .await
      .expect("run");
    assert_eq!(outcome.revisions, 0);
    assert!(outcome.state.get("b").is_some_and(|r| r.is_ok()));
  }

  #[tokio::test]
  async fn run_with_replan_stops_after_max_replans_and_returns_partial_state() {
    let _guard = crate::LLM_TEST_LOCK.lock().await;
    let _responses_guard = init_mock_model_with_responses(
      "mock-replan-exhausted",
      &[
        r#"{"steps":[{"id":"b","tool":"fail","params":{}}]}"#,
        r#"{"steps":[{"id":"b2","tool":"fail","params":{}}]}"#,
        // A 3rd response would only be consumed if max_replans were higher
        // than 1; its presence would not matter either way here.
        r#"{"steps":[{"id":"b3","tool":"fail","params":{}}]}"#,
      ],
    )
    .await;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FailTool));

    let agent = DynamicWorkflowAgent::new(
      "mock-replan-exhausted",
      Arc::new(registry),
      Arc::new(CoreFlowRunner::concurrent(8)),
    );

    let outcome = agent
      .run_with_replan("do something doomed", 1)
      .await
      .expect("run_with_replan must not error out on exhaustion");

    assert_eq!(outcome.revisions, 1);
    assert!(
      outcome.state.values().any(|r| r.is_err()),
      "the last attempt's failure must still be visible in the returned state"
    );
  }
}
