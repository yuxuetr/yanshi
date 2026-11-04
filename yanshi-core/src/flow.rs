use crate::{
  async_node::{AsyncNodeInputs, AsyncNodeResult},
  checkpoint::{
    Checkpoint, CheckpointConfig, CheckpointManager, WorkflowStatus, is_safe_path_component,
  },
  error::YanshiError,
  events::{EventListener, WorkflowEvent},
  expr,
  resource_limits::ResourceLimits,
  resume::{ResumePlan, ResumePlanOptions, build_resume_plan},
  retry_executor::execute_with_retry_and_hook,
  scheduler::{FlowExecutionConfig, FlowExecutionMode},
  state_size::estimated_state_pool_bytes,
  value::FlowValue,
};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

// The IR types (`Flow` / `GraphNode` / `NodeType`) moved to `yanshi-graph`
// (P-A1.3 step 2d-iv). The executor for them lives here: the engine is
// `FlowExecutor` (a borrow of the IR) and the public `flow.run()` API is the
// `FlowExt` trait at the bottom of this module.
pub use yanshi_graph::flow::{Flow, GraphNode, NodeType};

/// V1.2: reserved sibling output keys that tag a checkpoint entry's
/// outcome. `NodeSkipped` / `NodePartialExecutionFailed` never had a
/// checkpoint representation before V1.2 (dropped entirely, or — worse,
/// for the partial-failure case — silently written with the exact same
/// bare shape as a full `Ok`, so a resumed run treated a partial failure
/// as a complete success).
///
/// These are injected as ordinary extra entries *inside* the node's
/// existing flat outputs object rather than wrapping the whole value in
/// an envelope — `yanshi-agents`' partial-resume contract, CLI
/// tooling, and this crate's own tests all read a node's checkpointed
/// fields directly as `checkpoint.state[node_id][output_key]` (see
/// `yanshi-core/tests/checkpoint_recovery_test.rs`), so nesting
/// outputs one level deeper under e.g. `{"outcome": ..., "outputs":
/// {...}}` would silently break that established, load-bearing
/// contract. A sibling key survives the existing flat-object decode path
/// (`decode_outputs_object`) unmodified; `checkpoint_state_to_state_pool`
/// only needs to look the marker up and strip it back out of the
/// decoded map afterward.
const CHECKPOINT_OUTCOME_KEY: &str = "__yanshi_outcome__";
const CHECKPOINT_OUTCOME_SKIPPED: &str = "skipped";
const CHECKPOINT_OUTCOME_PARTIAL_FAILURE: &str = "partial_failure";
const CHECKPOINT_PARTIAL_FAILURE_MESSAGE_KEY: &str = "__yanshi_partial_failure_message__";

/// Executor over a borrowed [`Flow`]. Holds all run/resume/scheduling logic as
/// inherent methods so it can read the graph IR through the IR's accessors
/// across the crate boundary, while the orphan rule keeps inherent methods off
/// the foreign `Flow` type.
struct FlowExecutor<'a> {
  flow: &'a Flow,
}

impl<'f> FlowExecutor<'f> {
  fn new(flow: &'f Flow) -> Self {
    Self { flow }
  }

  /// Build a fresh checkpoint manager from the flow's config, if any.
  ///
  /// `CheckpointManager` is stateless (it just wraps a `CheckpointConfig`), so
  /// constructing one per use is free and lets the `Flow` struct carry only IR
  /// data. Returns `None` when checkpointing is not configured or the manager
  /// cannot be built (already validated eagerly in `with_checkpointing`).
  fn checkpoint_manager(&self) -> Option<CheckpointManager> {
    self
      .flow
      .checkpoint_config()
      .and_then(|config| CheckpointManager::new(config.clone()).ok())
  }

  /// Return the workflow execution order after dependency validation.
  ///
  /// This is useful for dry-run planning, debugging, and scheduler benchmarks.
  /// It does not execute nodes or create run artifacts.
  fn execution_order(&self) -> Result<Vec<String>, YanshiError> {
    self.topological_sort()
  }

  /// W5.3: extends the pre-existing passive `StateSizeObserver` telemetry
  /// with an advisory `WorkflowEvent::ResourceWarning` when the caller
  /// opted into `FlowExecutionConfig::resource_limits`. Purely additive —
  /// the byte count was already computed for the observer; this just adds
  /// a comparison and an event emission on top, no new state, no eviction.
  fn notify_state_size(
    &self,
    workflow_id: &str,
    state_pool: &HashMap<String, AsyncNodeResult>,
    resource_limits: Option<&ResourceLimits>,
  ) {
    let bytes = estimated_state_pool_bytes(state_pool);
    if let Some(observer) = self.flow.state_size_observer() {
      observer.observe(bytes);
    }
    if let Some(limits) = resource_limits
      && limits.exceeds_state_limit(bytes as usize)
    {
      self.emit_event(WorkflowEvent::ResourceWarning {
        workflow_id: workflow_id.to_string(),
        resource_type: "state_pool_bytes".to_string(),
        usage: bytes as f64 / limits.max_state_size as f64,
        limit: limits.max_state_size as f64,
        timestamp: Instant::now(),
      });
    }
  }

  pub async fn run(&self) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    self.execute_from_inputs(HashMap::new()).await
  }

  /// Resume workflow from the latest checkpoint for a given workflow ID.
  ///
  /// Uses [`ResumePlanOptions::default()`] — call [`Flow::resume_with_options`]
  /// to opt into `--force-replay`-style behavior.
  pub async fn resume(
    &self,
    workflow_id: &str,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    self
      .resume_with_options(workflow_id, &ResumePlanOptions::default())
      .await
  }

  /// Resume workflow from the latest checkpoint, threading
  /// [`ResumePlanOptions`] (e.g. `force_replay`) into the per-tool-call
  /// resume audit log.
  ///
  /// Before execution this method emits one
  /// [`WorkflowEvent::ResumeDecisionRecorded`] per unresolved tool call
  /// found in the checkpoint. If any call's decision is
  /// [`crate::ResumeDecision::RequiresManual`], resume aborts with a
  /// configuration error so the operator can resolve the call manually
  /// (or re-run with `force_replay`).
  pub async fn resume_with_options(
    &self,
    workflow_id: &str,
    options: &ResumePlanOptions,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    if !self.flow.is_checkpoint_enabled() {
      return Err(YanshiError::ConfigurationError {
        message: "Checkpointing is not enabled. Call with_checkpointing() first.".to_string(),
      });
    }

    let manager = self
      .checkpoint_manager()
      .ok_or_else(|| YanshiError::ConfigurationError {
        message: "Checkpoint manager not initialized despite checkpointing being enabled"
          .to_string(),
      })?;
    let checkpoint = manager
      .load_latest_checkpoint(workflow_id)
      .await?
      .ok_or_else(|| YanshiError::ConfigurationError {
        message: format!("No checkpoint found for workflow '{}'", workflow_id),
      })?;

    let plan = build_resume_plan(&checkpoint, options)?;
    self.emit_resume_decisions(&plan);
    if !plan.summary.can_auto_resume() {
      return Err(YanshiError::ConfigurationError {
        message: format!(
          "resume blocked: {} tool call(s) require manual recovery. Use \
           `yanshi workflow resume-plan {workflow_id}` to inspect them.",
          plan.summary.requires_manual
        ),
      });
    }

    tracing::info!(
      "Resuming workflow '{}' from checkpoint at node '{}'",
      workflow_id,
      checkpoint.last_completed_node
    );
    self.execute_from_checkpoint(workflow_id, checkpoint).await
  }

  /// Load the [`ResumePlan`] for a checkpointed workflow without
  /// executing anything. Used by the CLI (`yanshi workflow
  /// resume-plan`) and the server (`GET /v1/runs/{id}/resume-plan`).
  pub async fn load_resume_plan(
    &self,
    workflow_id: &str,
    options: &ResumePlanOptions,
  ) -> Result<ResumePlan, YanshiError> {
    let manager = self
      .checkpoint_manager()
      .ok_or_else(|| YanshiError::ConfigurationError {
        message: "Checkpointing is not enabled on this Flow.".to_string(),
      })?;
    let checkpoint = manager
      .load_latest_checkpoint(workflow_id)
      .await?
      .ok_or_else(|| YanshiError::ConfigurationError {
        message: format!("No checkpoint found for workflow '{}'", workflow_id),
      })?;
    build_resume_plan(&checkpoint, options)
  }

  fn emit_resume_decisions(&self, plan: &ResumePlan) {
    for entry in &plan.tool_calls {
      self.emit_event(WorkflowEvent::ResumeDecisionRecorded {
        workflow_id: plan.workflow_id.clone(),
        node_id: entry.node_id.clone(),
        tool_call_id: entry.tool_call_id.clone(),
        tool: entry.tool.clone(),
        step_index: entry.step_index,
        idempotency: entry.idempotency.as_str().to_string(),
        decision: entry.decision.as_str().to_string(),
        reason: entry.reason.clone(),
        force_replay: plan.force_replay,
        timestamp: Instant::now(),
      });
    }
  }

  pub async fn execute_from_inputs(
    &self,
    initial_inputs: AsyncNodeInputs,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    self
      .execute_with_workflow_id(None, initial_inputs, None, None, None)
      .await
  }

  pub async fn execute_from_inputs_with_config(
    &self,
    initial_inputs: AsyncNodeInputs,
    config: FlowExecutionConfig,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    self
      .execute_with_workflow_id(None, initial_inputs, None, None, Some(config))
      .await
  }

  /// Execute workflow with an explicit workflow/run id and execution config.
  ///
  /// Server and platform integrations use this to keep emitted
  /// [`WorkflowEvent`] ids aligned with their persisted run id.
  pub async fn execute_from_inputs_with_id_and_config(
    &self,
    workflow_id: String,
    initial_inputs: AsyncNodeInputs,
    config: FlowExecutionConfig,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    self
      .execute_with_workflow_id(Some(workflow_id), initial_inputs, None, None, Some(config))
      .await
  }

  /// Execute workflow with optional checkpoint recovery, under the run's
  /// cost meter (Y2.3). A sub-flow started by a node (Map / While items)
  /// meters as a child of that node, so its spend counts toward it.
  async fn execute_with_workflow_id(
    &self,
    workflow_id: Option<String>,
    initial_inputs: AsyncNodeInputs,
    restored_state_pool: Option<HashMap<String, AsyncNodeResult>>,
    restored_last_completed_node: Option<String>,
    execution_config: Option<FlowExecutionConfig>,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    let run_meter = match crate::run_context::current_cost_meter() {
      Some(parent) => crate::run_context::CostMeter::child(parent),
      None => crate::run_context::CostMeter::root(),
    };
    Box::pin(crate::run_context::with_cost_meter(
      Some(run_meter),
      self.execute_with_workflow_id_metered(
        workflow_id,
        initial_inputs,
        restored_state_pool,
        restored_last_completed_node,
        execution_config,
      ),
    ))
    .await
  }

  async fn execute_with_workflow_id_metered(
    &self,
    workflow_id: Option<String>,
    initial_inputs: AsyncNodeInputs,
    restored_state_pool: Option<HashMap<String, AsyncNodeResult>>,
    restored_last_completed_node: Option<String>,
    execution_config: Option<FlowExecutionConfig>,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    let run_id = workflow_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    // V0.4: `run_id` may be a caller-supplied workflow id (not one this
    // crate generated, e.g. a server-side run id), and it becomes a path
    // component below (`run_dir`, and transitively `checkpoint_dir` via
    // `CheckpointManager`). Reject anything that could escape the
    // intended directory before doing any filesystem work.
    if !is_safe_path_component(&run_id) {
      return Err(YanshiError::ConfigurationError {
        message: format!(
          "invalid workflow id '{}': must be a single path segment (no '/', '\\', '.', or '..')",
          run_id
        ),
      });
    }
    let workflow_started_at = Instant::now();
    self.emit_event(WorkflowEvent::WorkflowStarted {
      workflow_id: run_id.clone(),
      timestamp: workflow_started_at,
    });
    let execution_config = execution_config.unwrap_or_default();
    if execution_config
      .cancellation_token
      .as_ref()
      .is_some_and(|token| token.is_cancelled())
    {
      self.emit_event(WorkflowEvent::WorkflowCancelled {
        workflow_id: run_id.clone(),
        reason: "cancellation token signalled".to_string(),
        duration: workflow_started_at.elapsed(),
        timestamp: Instant::now(),
      });
      return Err(YanshiError::TaskCancelled);
    }
    let base_dir = execution_config.resolved_run_base_dir()?;
    let run_dir = base_dir.join(&run_id);
    fs::create_dir_all(&run_dir).map_err(|e| YanshiError::PersistenceError {
      message: e.to_string(),
    })?;

    if execution_config.mode == FlowExecutionMode::Concurrent
      && restored_state_pool.is_none()
      && restored_last_completed_node.is_none()
    {
      return self
        .execute_concurrently(
          run_id,
          workflow_started_at,
          run_dir,
          initial_inputs,
          execution_config,
        )
        .await;
    }

    let sorted_nodes = self.topological_sort()?;
    let mut state_pool: HashMap<String, AsyncNodeResult> = restored_state_pool.unwrap_or_default();
    let mut last_completed_node = restored_last_completed_node.or_else(|| {
      state_pool
        .iter()
        .filter_map(|(node_id, result)| result.as_ref().ok().map(|_| node_id.as_str()))
        .filter_map(|node_id| {
          sorted_nodes
            .iter()
            .position(|sorted_node| sorted_node == node_id)
            .map(|idx| (idx, node_id.to_string()))
        })
        .max_by_key(|(idx, _)| *idx)
        .map(|(_, node_id)| node_id)
    });

    for node_id in &sorted_nodes {
      if execution_config
        .cancellation_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
      {
        self.emit_event(WorkflowEvent::WorkflowCancelled {
          workflow_id: run_id.clone(),
          reason: "cancellation token signalled".to_string(),
          duration: workflow_started_at.elapsed(),
          timestamp: Instant::now(),
        });
        return Err(YanshiError::TaskCancelled);
      }

      // V1.2: a node is genuinely done (from a restored checkpoint, or
      // from an earlier iteration of this very loop) iff `state_pool`
      // holds an `Ok` or a benign `NodeSkipped` for it — reuse
      // `is_genuine_failure`'s vocabulary (V0.3) rather than inventing a
      // second notion of "resolved". This replaces a topological-index
      // cutoff derived from a single `last_completed_node`, which is
      // unsound once the *original* run could complete nodes out of
      // topological order (concurrent execution): a node topologically
      // *before* `last_completed_node` may never have actually run,
      // while one topologically *after* it may already have. Presence
      // (of the right kind) in `state_pool` is authoritative regardless
      // of position. A `NodePartialExecutionFailed` entry deliberately
      // does NOT count as done here — it must be retried (see the
      // partial-outputs re-injection below) — matching the pre-existing
      // agent-node partial-resume contract, which relies on the node
      // being re-executed rather than skipped.
      if state_pool
        .get(node_id)
        .is_some_and(|result| !is_genuine_failure(result))
      {
        tracing::info!(
          "Skipping node '{}' (already resolved in checkpoint)",
          node_id
        );
        continue;
      }

      let graph_node =
        self
          .flow
          .nodes()
          .get(node_id)
          .ok_or_else(|| YanshiError::FlowDefinitionError {
            message: format!("Node '{}' not found in flow definition", node_id),
          })?;

      // W1.2: a `run_if` evaluation failure (malformed expression, e.g.)
      // used to propagate via `?` and abort this *entire function*
      // immediately — no `WorkflowFailed` event, no final checkpoint
      // status write. Downgraded to `Ok`/`Err` here so it flows through
      // the same unified tail as a skip or a real execution result,
      // exactly like the `gather_inputs` failure a few lines down
      // already does (V1.1). Unifying the skip branch into this same
      // tail also fixes a Serial-vs-Concurrent asymmetry: pre-fix,
      // Serial skips never called `record_node_result_events`, so no
      // `NodeSkipped` event ever reached the listener here (the
      // concurrent path already emitted one).
      let condition_result = match &graph_node.run_if {
        Some(condition) => self.evaluate_condition(condition, &state_pool),
        None => Ok(true),
      };

      let (result, node_started_at) = match condition_result {
        Err(error) => (Err(error), Instant::now()),
        Ok(false) => {
          tracing::info!("Skipping node '{}' due to condition.", node_id);
          (Err(YanshiError::NodeSkipped), Instant::now())
        }
        Ok(true) => {
          // V1.1: a `gather_inputs` failure (e.g. a required dependency was
          // skipped, or an upstream node genuinely failed) used to
          // propagate via `?` and abort this *entire function* — no
          // `WorkflowFailed` event, no final checkpoint status write, and
          // the caller got a hard `Err` instead of the per-node result the
          // concurrent path already produces for the identical case (see
          // `execute_concurrently`'s matching `gather_inputs` error
          // branch, which this mirrors: a fresh `Instant::now()` stands in
          // for `node_started_at` so a gather-input failure reports ~0
          // duration, since no node code ever ran).
          let inputs_result = match &graph_node.input_mapping {
            Some(mapping) => self.gather_inputs(node_id, mapping, &state_pool, &initial_inputs),
            None => Ok(HashMap::new()),
          };

          match inputs_result {
            Ok(mut inputs) => {
              inputs.extend(graph_node.initial_inputs.clone());

              // Inject initial inputs from execute_from_inputs (for while loops and map nodes)
              // These provide loop variables and context that should be available to all nodes
              inputs.extend(initial_inputs.clone());

              // V1.2: a node being (re-)executed here was never skipped
              // above, so any `state_pool` entry it already has can only be
              // a genuine failure (see the skip check above) — in
              // particular a `NodePartialExecutionFailed` restored from a
              // checkpoint. Feed its `partial_outputs` back in as
              // additional inputs so the node can inspect its own prior
              // partial progress (e.g. an agent's `agent_result` /
              // `agent_resume`) and decide what to replay vs. skip
              // internally, exactly like the pre-V1.2 behavior did — except
              // that behavior relied on the checkpoint round-trip bug this
              // fix closes (a restored `NodePartialExecutionFailed` used to
              // silently decode as `Ok`, which happened to feed the outputs
              // back in via a bare `Ok` match; now it's explicit).
              if let Some(Err(YanshiError::NodePartialExecutionFailed {
                partial_outputs, ..
              })) = state_pool.get(node_id)
              {
                inputs.extend(partial_outputs.clone());
              }

              tracing::info!("Executing node '{}'", node_id);
              let node_started_at = Instant::now();
              self.emit_event(WorkflowEvent::NodeStarted {
                workflow_id: run_id.clone(),
                node_id: node_id.clone(),
                timestamp: node_started_at,
              });
              let result = self
                .execute_node_with_reliability(
                  &run_id,
                  &run_dir,
                  node_id,
                  &graph_node.node_type,
                  &inputs,
                  &execution_config,
                )
                .await;
              (result, node_started_at)
            }
            Err(error) => (Err(error), Instant::now()),
          }
        }
      };

      self.persist_step_result(&run_dir, node_id, &result)?;

      if result.is_ok() {
        last_completed_node = Some(node_id.clone());
      }
      self.record_node_result_events(&run_id, node_id, node_started_at, &result);
      let is_failure = is_genuine_failure(&result);
      let is_skip = matches!(result, Err(YanshiError::NodeSkipped));

      state_pool.insert(node_id.to_string(), result);
      self.notify_state_size(
        &run_id,
        &state_pool,
        execution_config.resource_limits.as_ref(),
      );

      // Save checkpoint if enabled
      if self.flow.is_checkpoint_enabled()
        && state_pool
          .get(node_id)
          .map(|result| result.is_ok())
          .unwrap_or(false)
        && let Some(ref manager) = self.checkpoint_manager()
      {
        let checkpoint_state = self.state_pool_to_checkpoint_state(&state_pool);
        if let Err(_e) = manager
          .save_checkpoint(&run_id, node_id, &checkpoint_state)
          .await
        {
          tracing::warn!("Failed to save checkpoint after node '{}': {}", node_id, _e);
        } else {
          tracing::info!("Checkpoint saved after node '{}'", node_id);
        }
      }

      // V1.1: align with `execute_concurrently`'s fail_fast /
      // continue_on_skip semantics instead of unconditionally running
      // every remaining node regardless of configuration. Remaining
      // nodes simply never get a state_pool entry, exactly like the
      // concurrent path leaves unscheduled `pending` nodes absent.
      if is_failure && execution_config.fail_fast {
        break;
      }
      if is_skip && !execution_config.continue_on_skip {
        break;
      }
    }

    let workflow_failed = state_pool.values().any(is_genuine_failure);
    if workflow_failed {
      let error = representative_failure_message(&state_pool);
      self.emit_event(WorkflowEvent::WorkflowFailed {
        workflow_id: run_id.clone(),
        error,
        duration: workflow_started_at.elapsed(),
        timestamp: Instant::now(),
      });
    } else {
      self.emit_event(WorkflowEvent::WorkflowCompleted {
        workflow_id: run_id.clone(),
        duration: workflow_started_at.elapsed(),
        timestamp: Instant::now(),
      });
    }

    // Mark workflow as completed or failed
    if self.flow.is_checkpoint_enabled()
      && let Some(ref manager) = self.checkpoint_manager()
    {
      let checkpoint_state = self.state_pool_to_checkpoint_state(&state_pool);
      let status = if workflow_failed {
        WorkflowStatus::Failed
      } else {
        WorkflowStatus::Completed
      };
      let final_checkpoint_node = last_completed_node.as_deref().unwrap_or("");

      if let Err(_e) = manager
        .save_checkpoint_with_status(&run_id, final_checkpoint_node, &checkpoint_state, status)
        .await
      {
        tracing::warn!("Failed to save final checkpoint: {}", _e);
      }
    }

    Ok(state_pool)
  }

  /// Execute workflow from a checkpoint
  async fn execute_from_checkpoint(
    &self,
    workflow_id: &str,
    checkpoint: Checkpoint,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    // Restore state pool from checkpoint. V1.2: this now faithfully
    // round-trips `Ok` / `NodeSkipped` / `NodePartialExecutionFailed`
    // (see `checkpoint_state_to_state_pool`), so "does this node hold an
    // `Ok`/`NodeSkipped` in the restored pool" is a sound proxy for "was
    // this node actually observed to resolve" — unlike the
    // topological-index cutoff this replaces (see
    // `execute_with_workflow_id`'s per-node skip check for the full
    // rationale). A restored `NodePartialExecutionFailed` does NOT count
    // as resolved — it must be retried, matching the pre-existing
    // agent-node partial-resume contract.
    let state_pool = Self::checkpoint_state_to_state_pool(&checkpoint.state);
    let sorted_nodes = self.topological_sort()?;

    if sorted_nodes.iter().all(|node_id| {
      state_pool
        .get(node_id)
        .is_some_and(|result| !is_genuine_failure(result))
    }) {
      tracing::info!("Workflow '{}' was already completed", workflow_id);
      return Ok(state_pool);
    }

    self
      .execute_with_workflow_id(
        Some(workflow_id.to_string()),
        HashMap::new(),
        Some(state_pool),
        Some(checkpoint.last_completed_node),
        None,
      )
      .await
  }

  /// Convert state pool to checkpoint-compatible format.
  ///
  /// V1.2: a genuine failure (anything but `Ok` / `NodeSkipped` /
  /// `NodePartialExecutionFailed`) is still deliberately excluded —
  /// resume always retries those, which is the existing and desired
  /// behavior. `Ok` keeps its pre-V1.2 bare shape (`{key: tagged
  /// FlowValue, ...}`) unchanged for on-disk backward compatibility;
  /// `NodeSkipped` / `NodePartialExecutionFailed` are new additions (they
  /// were silently dropped entirely before V1.2) tagged with a reserved
  /// marker key so `checkpoint_state_to_state_pool` can round-trip the
  /// original outcome instead of losing the skip / upgrading the partial
  /// failure to a full success on resume.
  fn state_pool_to_checkpoint_state(
    &self,
    state_pool: &HashMap<String, AsyncNodeResult>,
  ) -> HashMap<String, serde_json::Value> {
    state_pool
      .iter()
      .filter_map(|(node_id, result)| {
        Self::checkpointable_value(result).map(|value| (node_id.clone(), value))
      })
      .collect()
  }

  fn checkpointable_value(result: &AsyncNodeResult) -> Option<serde_json::Value> {
    match result {
      Ok(outputs) => Some(Self::outputs_to_json(outputs)),
      Err(YanshiError::NodeSkipped) => {
        let mut tagged = HashMap::new();
        tagged.insert(
          CHECKPOINT_OUTCOME_KEY.to_string(),
          FlowValue::Json(serde_json::json!(CHECKPOINT_OUTCOME_SKIPPED)),
        );
        Some(Self::outputs_to_json(&tagged))
      }
      Err(YanshiError::NodePartialExecutionFailed {
        message,
        partial_outputs,
      }) => {
        let mut tagged = partial_outputs.clone();
        tagged.insert(
          CHECKPOINT_OUTCOME_KEY.to_string(),
          FlowValue::Json(serde_json::json!(CHECKPOINT_OUTCOME_PARTIAL_FAILURE)),
        );
        tagged.insert(
          CHECKPOINT_PARTIAL_FAILURE_MESSAGE_KEY.to_string(),
          FlowValue::Json(serde_json::json!(message)),
        );
        Some(Self::outputs_to_json(&tagged))
      }
      Err(_) => None,
    }
  }

  fn outputs_to_json(outputs: &HashMap<String, FlowValue>) -> serde_json::Value {
    let json_outputs: HashMap<String, serde_json::Value> = outputs
      .iter()
      .map(|(key, value)| {
        let checkpoint_value = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        (key.clone(), checkpoint_value)
      })
      .collect();
    serde_json::to_value(json_outputs).unwrap_or_else(|_| serde_json::json!({}))
  }

  fn emit_event(&self, event: WorkflowEvent) {
    if let Some(listener) = self.flow.event_listener() {
      listener.on_event(&event);
    }
  }

  /// V1.2: recognizes the `CHECKPOINT_OUTCOME_KEY` sibling entry
  /// `state_pool_to_checkpoint_state` / `checkpointable_value` inject
  /// for `NodeSkipped` / `NodePartialExecutionFailed` and decodes the
  /// value back to the original `Err` variant instead of the pre-V1.2
  /// behavior (silently dropped entirely for skips; silently decoded as
  /// a full `Ok` for partial failures — both were forms of "resume
  /// can't tell this apart from a real success"). A value with no such
  /// entry takes the unchanged legacy/`Ok` path, so on-disk checkpoints
  /// written before V1.2 (or any `Ok` entry, which never carries this
  /// key) decode exactly as they always did.
  fn checkpoint_state_to_state_pool(
    checkpoint_state: &HashMap<String, serde_json::Value>,
  ) -> HashMap<String, AsyncNodeResult> {
    checkpoint_state
      .iter()
      .map(|(node_id, value)| {
        let mut outputs = Self::decode_outputs_object(node_id, value);
        let outcome = match outputs.remove(CHECKPOINT_OUTCOME_KEY) {
          Some(FlowValue::Json(serde_json::Value::String(tag))) => Some(tag),
          _ => None,
        };
        let result = match outcome.as_deref() {
          Some(CHECKPOINT_OUTCOME_SKIPPED) => Err(YanshiError::NodeSkipped),
          Some(CHECKPOINT_OUTCOME_PARTIAL_FAILURE) => {
            let message = match outputs.remove(CHECKPOINT_PARTIAL_FAILURE_MESSAGE_KEY) {
              Some(FlowValue::Json(serde_json::Value::String(message))) => message,
              _ => "partial execution failure".to_string(),
            };
            Err(YanshiError::NodePartialExecutionFailed {
              message,
              partial_outputs: outputs,
            })
          }
          // Unrecognized tag or no tag at all (legacy pre-V1.2 shape,
          // or any `Ok` entry — those never carry this key): treat the
          // whole decoded map as a successful node's outputs.
          _ => Ok(outputs),
        };
        (node_id.clone(), result)
      })
      .collect()
  }

  fn decode_outputs_object(node_id: &str, value: &serde_json::Value) -> HashMap<String, FlowValue> {
    match value {
      serde_json::Value::Object(map) => map
        .iter()
        .map(|(key, value)| {
          let flow_value = decode_checkpoint_flow_value(node_id, key, value);
          (key.clone(), flow_value)
        })
        .collect(),
      other => vec![("result".to_string(), FlowValue::Json(other.clone()))]
        .into_iter()
        .collect(),
    }
  }
}

/// V0.3: a benign `run_if`-skip is represented as
/// `Err(YanshiError::NodeSkipped)` in the state pool — dependency-
/// readiness checks already treat it as "as good as completed" (see the
/// `Some(Ok(_)) | Some(Err(NodeSkipped))` match in `execute_concurrently`),
/// and `expr::evaluate` resolves a skipped node's outputs to `Null` rather
/// than propagating an error. Terminal-status / checkpoint-status
/// judgments must apply the same rule: a workflow where every node either
/// completed or was skipped is `Completed`, not `Failed`. Plain
/// `Result::is_err()` conflated the two, so a workflow that only ever hit
/// benign skips was reported (and checkpointed) as failed.
fn is_genuine_failure(result: &AsyncNodeResult) -> bool {
  !matches!(result, Ok(_) | Err(YanshiError::NodeSkipped))
}

/// Pick a representative error message for `WorkflowEvent::WorkflowFailed`.
/// Must skip over benign `NodeSkipped` entries — otherwise, when a genuine
/// failure and an unrelated skip coexist in the same state pool,
/// `HashMap` iteration order could nondeterministically surface "Node was
/// skipped due to a condition." as the reported failure reason instead of
/// the actual error.
fn representative_failure_message(state_pool: &HashMap<String, AsyncNodeResult>) -> String {
  state_pool
    .values()
    .filter(|result| is_genuine_failure(result))
    .find_map(|result| result.as_ref().err().map(ToString::to_string))
    .unwrap_or_else(|| "workflow failed".to_string())
}

/// Decode one checkpoint output value back into a [`FlowValue`].
///
/// Three cases:
///
/// 1. **Tagged value** (object with a recognized `type: "json" | "file"
///    | "url"`): decode via `FlowValue`'s deserializer. If decode fails
///    the value was tagged but corrupt — log a warning so operators
///    can see the partial loss instead of silently downgrading to
///    `FlowValue::Json`. The function still returns a fallback so
///    resume / replay can proceed.
/// 2. **Untagged value** (no `type` field, or a `type` field that
///    doesn't match a known tag): treat as a legacy raw-JSON
///    checkpoint and wrap as `FlowValue::Json` without warning.
///    Pre-0.2 checkpoints relied on this implicit encoding (see
///    `tests/flow_value_checkpoint_compat.rs::legacy_raw_json_checkpoint_values_read_as_json_flow_values`).
/// 3. **Non-object value**: wrap as `FlowValue::Json` — primitives,
///    arrays, and `null` never used the tagged form.
fn decode_checkpoint_flow_value(node_id: &str, key: &str, value: &serde_json::Value) -> FlowValue {
  let tag = value
    .as_object()
    .and_then(|map| map.get("type"))
    .and_then(serde_json::Value::as_str);

  match tag {
    Some("json") | Some("file") | Some("url") => {
      // Tagged value — caller expects a specific variant. Only fall
      // back to `Json` if decoding genuinely fails, and warn loudly
      // so the regression is debuggable.
      serde_json::from_value::<FlowValue>(value.clone()).unwrap_or_else(|err| {
        tracing::warn!(
          "checkpoint for node '{}' field '{}' is tagged \
           `type: \"{}\"` but failed to deserialize as FlowValue: {}. \
           Falling back to FlowValue::Json — downstream consumers that \
           pattern-match on File/Url will not see this output.",
          node_id,
          key,
          tag.unwrap_or("unknown"),
          err
        );
        FlowValue::Json(value.clone())
      })
    }
    _ => FlowValue::Json(value.clone()),
  }
}

impl<'f> FlowExecutor<'f> {
  async fn execute_concurrently(
    &self,
    run_id: String,
    workflow_started_at: Instant,
    run_dir: PathBuf,
    initial_inputs: AsyncNodeInputs,
    config: FlowExecutionConfig,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    let sorted_nodes = self.topological_sort()?;
    let mut pending: HashSet<String> = sorted_nodes.iter().cloned().collect();
    let mut state_pool: HashMap<String, AsyncNodeResult> = HashMap::new();
    let mut running: FuturesUnordered<BoxFuture<'_, (String, Instant, AsyncNodeResult)>> =
      FuturesUnordered::new();
    let mut last_completed_node = None;
    let mut fail_fast_triggered = false;

    while !pending.is_empty() || !running.is_empty() {
      if config
        .cancellation_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
      {
        self.emit_event(WorkflowEvent::WorkflowCancelled {
          workflow_id: run_id.clone(),
          reason: "cancellation token signalled".to_string(),
          duration: workflow_started_at.elapsed(),
          timestamp: Instant::now(),
        });
        return Err(YanshiError::TaskCancelled);
      }

      while !fail_fast_triggered && running.len() < config.max_concurrency {
        let Some(node_id) = sorted_nodes
          .iter()
          .find(|node_id| {
            pending.contains(*node_id)
              && self
                .flow
                .nodes()
                .get(*node_id)
                .map(|node| {
                  node.dependencies.iter().all(|dep| {
                    matches!(
                      state_pool.get(dep),
                      Some(Ok(_)) | Some(Err(YanshiError::NodeSkipped))
                    )
                  }) && self
                    .undeclared_references(node_id, node)
                    .iter()
                    .all(|source| state_pool.contains_key(source))
                })
                .unwrap_or(false)
          })
          .cloned()
        else {
          break;
        };

        pending.remove(&node_id);
        let graph_node = self
          .flow
          .nodes()
          .get(&node_id)
          .ok_or_else(|| YanshiError::FlowDefinitionError {
            message: format!("Node '{}' not found in flow definition", node_id),
          })?
          .clone();

        // W1.2: an `evaluate_condition` failure (malformed `run_if`
        // expression, e.g.) used to propagate via `?` and abort this
        // *entire function* immediately — no `WorkflowFailed` event, no
        // final checkpoint status write, and any already-dispatched
        // in-flight sibling futures silently dropped instead of being
        // drained. Downgrade to a per-node error and let it flow
        // through the exact same fail_fast/continue_on_skip machinery
        // as the `gather_inputs` failure case just below, which already
        // gets this right.
        let condition_result = match &graph_node.run_if {
          Some(condition) => self.evaluate_condition(condition, &state_pool),
          None => Ok(true),
        };

        let should_run = match condition_result {
          Ok(should_run) => should_run,
          Err(error) => {
            let result = Err(error);
            self.persist_step_result(&run_dir, &node_id, &result)?;
            self.record_node_result_events(&run_id, &node_id, Instant::now(), &result);
            state_pool.insert(node_id, result);
            self.notify_state_size(&run_id, &state_pool, config.resource_limits.as_ref());
            if config.fail_fast {
              fail_fast_triggered = true;
              break;
            }
            continue;
          }
        };

        if !should_run {
          tracing::info!("Skipping node '{}' due to condition.", node_id);
          let result = Err(YanshiError::NodeSkipped);
          self.persist_step_result(&run_dir, &node_id, &result)?;
          self.emit_event(WorkflowEvent::NodeSkipped {
            workflow_id: run_id.clone(),
            node_id: node_id.clone(),
            reason: "run_if evaluated to false".to_string(),
            timestamp: Instant::now(),
          });
          state_pool.insert(node_id, result);
          self.notify_state_size(&run_id, &state_pool, config.resource_limits.as_ref());
          if !config.continue_on_skip {
            fail_fast_triggered = true;
            break;
          }
          continue;
        }

        let mut inputs = match &graph_node.input_mapping {
          Some(mapping) => {
            match self.gather_inputs(&node_id, mapping, &state_pool, &initial_inputs) {
              Ok(inputs) => inputs,
              Err(error) => {
                let result = Err(error);
                self.persist_step_result(&run_dir, &node_id, &result)?;
                self.record_node_result_events(&run_id, &node_id, Instant::now(), &result);
                state_pool.insert(node_id, result);
                self.notify_state_size(&run_id, &state_pool, config.resource_limits.as_ref());
                if config.fail_fast {
                  fail_fast_triggered = true;
                  break;
                }
                continue;
              }
            }
          }
          None => HashMap::new(),
        };
        inputs.extend(graph_node.initial_inputs.clone());
        inputs.extend(initial_inputs.clone());

        tracing::info!("Executing node '{}'", node_id);
        let node_started_at = Instant::now();
        self.emit_event(WorkflowEvent::NodeStarted {
          workflow_id: run_id.clone(),
          node_id: node_id.clone(),
          timestamp: node_started_at,
        });

        let run_id_for_task = run_id.clone();
        let run_dir_for_task = run_dir.clone();
        let config_for_task = config.clone();
        running.push(
          async move {
            let result = self
              .execute_node_with_reliability(
                &run_id_for_task,
                &run_dir_for_task,
                &node_id,
                &graph_node.node_type,
                &inputs,
                &config_for_task,
              )
              .await;
            (node_id, node_started_at, result)
          }
          .boxed(),
        );
      }

      if fail_fast_triggered && running.is_empty() {
        break;
      }

      if running.is_empty() {
        if pending.is_empty() {
          break;
        }
        // V0.3: a benign skip never blocks its dependents (see the
        // `Some(Ok(_)) | Some(Err(NodeSkipped))` readiness match above),
        // so pending nodes stalling with only skips (no genuine failure)
        // in the state pool would be a real scheduler bug, not something
        // a skip explains — fall through to the hard error below instead
        // of silently swallowing it.
        if state_pool.values().any(is_genuine_failure) {
          break;
        }
        return Err(YanshiError::FlowExecutionFailed {
          message: "No schedulable workflow nodes remain; dependency state is incomplete"
            .to_string(),
        });
      }

      if let Some((node_id, node_started_at, result)) = running.next().await {
        self.persist_step_result(&run_dir, &node_id, &result)?;
        self.record_node_result_events(&run_id, &node_id, node_started_at, &result);

        if result.is_ok() {
          last_completed_node = Some(node_id.clone());
        } else if config.fail_fast {
          fail_fast_triggered = true;
        }

        state_pool.insert(node_id.clone(), result);
        self.notify_state_size(&run_id, &state_pool, config.resource_limits.as_ref());

        if self.flow.is_checkpoint_enabled()
          && state_pool
            .get(&node_id)
            .map(|result| result.is_ok())
            .unwrap_or(false)
          && let Some(ref manager) = self.checkpoint_manager()
        {
          let checkpoint_state = self.state_pool_to_checkpoint_state(&state_pool);
          if let Err(_e) = manager
            .save_checkpoint(&run_id, &node_id, &checkpoint_state)
            .await
          {
            tracing::warn!("Failed to save checkpoint after node '{}': {}", node_id, _e);
          }
        }
      }
    }

    let workflow_failed = state_pool.values().any(is_genuine_failure) || fail_fast_triggered;
    if workflow_failed {
      let error = representative_failure_message(&state_pool);
      self.emit_event(WorkflowEvent::WorkflowFailed {
        workflow_id: run_id.clone(),
        error,
        duration: workflow_started_at.elapsed(),
        timestamp: Instant::now(),
      });
    } else {
      self.emit_event(WorkflowEvent::WorkflowCompleted {
        workflow_id: run_id.clone(),
        duration: workflow_started_at.elapsed(),
        timestamp: Instant::now(),
      });
    }

    if self.flow.is_checkpoint_enabled()
      && let Some(ref manager) = self.checkpoint_manager()
    {
      let checkpoint_state = self.state_pool_to_checkpoint_state(&state_pool);
      let status = if !workflow_failed {
        WorkflowStatus::Completed
      } else {
        WorkflowStatus::Failed
      };
      let final_checkpoint_node = last_completed_node.as_deref().unwrap_or("");
      if let Err(_e) = manager
        .save_checkpoint_with_status(&run_id, final_checkpoint_node, &checkpoint_state, status)
        .await
      {
        tracing::warn!("Failed to save final checkpoint: {}", _e);
      }
    }

    Ok(state_pool)
  }

  async fn execute_node_type(
    &self,
    node_type: &NodeType,
    inputs: &AsyncNodeInputs,
    execution_config: &FlowExecutionConfig,
  ) -> AsyncNodeResult {
    match node_type {
      NodeType::Standard(node) => node.execute(inputs).await,
      NodeType::Map {
        template,
        parallel,
        max_concurrent,
      } => {
        if *parallel {
          self
            .execute_map_node_parallel(inputs, template, *max_concurrent, execution_config)
            .await
        } else {
          self
            .execute_map_node_sequential(inputs, template, execution_config)
            .await
        }
      }
      NodeType::While {
        condition,
        max_iterations,
        template,
        continue_on_error,
        fail_on_exhausted,
      } => {
        self
          .execute_while_node(
            inputs,
            condition,
            *max_iterations,
            template,
            *continue_on_error,
            *fail_on_exhausted,
            execution_config,
          )
          .await
      }
    }
  }

  /// V1.4: wraps [`Self::execute_node_type`] with the Flow-wide,
  /// opt-in reliability primitives this crate ships (`tokio::time::timeout`,
  /// `retry_executor::execute_with_retry_and_hook`) but never applied
  /// inside its own DAG executor before this fix — `yanshi-core`'s
  /// `timeout`/`retry_executor` modules had no production caller inside
  /// `Flow` itself, and `WorkflowEvent::RetryAttempt` was never emitted
  /// anywhere in the workspace. Both `node_timeout_ms` and
  /// `node_retry_policy` default to `None` on [`FlowExecutionConfig`],
  /// so a workflow that doesn't opt in sees zero behavior change: this
  /// reduces to a bare `execute_node_type` call, used identically by
  /// both the serial and concurrent execution paths.
  ///
  /// This is a Flow-wide complement to (not a replacement for)
  /// `yanshi-config`'s per-node YAML `timeout_ms`/`max_retries`
  /// (T3.1's `TimeoutRetryNode`, which decorates an `AsyncNode` *before*
  /// it ever reaches this executor and so has no way to emit a
  /// `WorkflowEvent` — its retries are invisible to trace replay / the
  /// event stream). Every attempt made here runs inside the executor
  /// itself, where `self.emit_event` is available, so every retry is
  /// observable.
  ///
  /// Y1.1: `run_dir` is also installed as the node's artifact root
  /// (`crate::run_context`) for the duration of the call, so file-producing
  /// nodes can write under `<run_dir>/artifacts/<node_id>/`.
  async fn execute_node_with_reliability(
    &self,
    run_id: &str,
    run_dir: &Path,
    node_id: &str,
    node_type: &NodeType,
    inputs: &AsyncNodeInputs,
    config: &FlowExecutionConfig,
  ) -> AsyncNodeResult {
    // Y2.3: refuse to start once the run has overspent, and give the
    // node its own meter (a child of the run's) so its cost is known.
    let run_meter = crate::run_context::current_cost_meter();
    if let (Some(limit_usd), Some(meter)) = (config.cost_limit_usd, run_meter.as_ref()) {
      let spent_usd = meter.total();
      if spent_usd > limit_usd {
        if meter.claim_limit_report() {
          self.emit_event(WorkflowEvent::CostLimitExceeded {
            workflow_id: run_id.to_string(),
            node_id: node_id.to_string(),
            spent_usd,
            limit_usd,
            timestamp: Instant::now(),
          });
        }
        return Err(YanshiError::CostLimitExceeded {
          spent_usd,
          limit_usd,
        });
      }
    }
    let node_meter = run_meter.clone().map(crate::run_context::CostMeter::child);
    let result = self
      .execute_node_attempts(
        run_id,
        run_dir,
        node_id,
        node_type,
        inputs,
        config,
        &node_meter,
      )
      .await;
    if let (Some(node_meter), Some(run_meter)) = (node_meter, run_meter) {
      let cost_usd = node_meter.total();
      if cost_usd > 0.0 {
        self.emit_event(WorkflowEvent::NodeCostRecorded {
          workflow_id: run_id.to_string(),
          node_id: node_id.to_string(),
          cost_usd,
          total_cost_usd: run_meter.total(),
          timestamp: Instant::now(),
        });
      }
    }
    result
  }

  #[allow(clippy::too_many_arguments)]
  async fn execute_node_attempts(
    &self,
    run_id: &str,
    run_dir: &Path,
    node_id: &str,
    node_type: &NodeType,
    inputs: &AsyncNodeInputs,
    config: &FlowExecutionConfig,
    node_meter: &Option<std::sync::Arc<crate::run_context::CostMeter>>,
  ) -> AsyncNodeResult {
    let run_once = || async {
      let execute = crate::run_context::with_cost_meter(
        node_meter.clone(),
        crate::run_context::with_artifact_root(
          run_dir.to_path_buf(),
          self.execute_node_type(node_type, inputs, config),
        ),
      );
      match config.node_timeout_ms {
        Some(timeout_ms) => {
          match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), execute).await {
            Ok(result) => result,
            Err(_elapsed) => Err(YanshiError::TimeoutExceeded {
              duration_ms: timeout_ms,
            }),
          }
        }
        None => execute.await,
      }
    };

    match &config.node_retry_policy {
      Some(policy) => {
        let max_attempts = policy.max_attempts;
        execute_with_retry_and_hook(
          policy,
          node_id,
          |attempt, _error, _delay| {
            self.emit_event(WorkflowEvent::RetryAttempt {
              workflow_id: run_id.to_string(),
              node_id: node_id.to_string(),
              attempt,
              max_attempts,
              timestamp: Instant::now(),
            });
          },
          run_once,
        )
        .await
      }
      None => run_once().await,
    }
  }

  fn record_node_result_events(
    &self,
    run_id: &str,
    node_id: &str,
    node_started_at: Instant,
    result: &AsyncNodeResult,
  ) {
    match result {
      Ok(outputs) => {
        self.emit_event(WorkflowEvent::NodeOutputCaptured {
          workflow_id: run_id.to_string(),
          node_id: node_id.to_string(),
          output: Self::outputs_to_json(outputs),
          timestamp: Instant::now(),
        });
        self.emit_event(WorkflowEvent::NodeCompleted {
          workflow_id: run_id.to_string(),
          node_id: node_id.to_string(),
          duration: node_started_at.elapsed(),
          timestamp: Instant::now(),
        });
      }
      Err(YanshiError::NodeSkipped) => {
        self.emit_event(WorkflowEvent::NodeSkipped {
          workflow_id: run_id.to_string(),
          node_id: node_id.to_string(),
          reason: "run_if evaluated to false".to_string(),
          timestamp: Instant::now(),
        });
      }
      Err(err) => {
        self.emit_event(WorkflowEvent::NodeFailed {
          workflow_id: run_id.to_string(),
          node_id: node_id.to_string(),
          error: err.to_string(),
          duration: node_started_at.elapsed(),
          timestamp: Instant::now(),
        });
      }
    }
  }

  /// W1.1: build a Map/While sub-flow that inherits the parent's
  /// observability/cancellation/run-dir context instead of starting
  /// from a bare `Flow::new` + default `FlowExecutionConfig`. Pre-fix,
  /// sub-flow nodes were invisible to trace/UI/SSE (no
  /// `event_listener`), uncancellable (no `cancellation_token`), and
  /// always wrote under the hardcoded `~/.yanshi/runs/<uuid>`
  /// default regardless of what `run_base_dir` the parent was actually
  /// configured with (each sub-flow still gets its own uuid-named
  /// subdirectory via `execute_from_inputs_with_config`'s fresh
  /// `run_id` — only the *base* directory is inherited here, so
  /// concurrent Map items still can't collide with each other).
  /// Deliberately does NOT inherit `mode`/`node_timeout_ms`/
  /// `node_retry_policy` — sub-flows stay serial and policy-free unless
  /// a future change explicitly opts them in.
  fn sub_flow_execution_context(
    &self,
    template: &[GraphNode],
    execution_config: &FlowExecutionConfig,
  ) -> (Flow, FlowExecutionConfig) {
    let mut sub_flow = Flow::new(template.to_vec());
    if let Some(listener) = self.flow.event_listener() {
      sub_flow = sub_flow.with_event_listener(Arc::new(SubFlowListener(listener.clone())));
    }
    if let Some(observer) = self.flow.state_size_observer() {
      sub_flow = sub_flow.with_state_size_observer(observer.clone());
    }
    let sub_config = FlowExecutionConfig {
      run_base_dir: execution_config.run_base_dir.clone(),
      cancellation_token: execution_config.cancellation_token.clone(),
      ..FlowExecutionConfig::serial()
    };
    (sub_flow, sub_config)
  }

  #[allow(clippy::too_many_arguments)]
  fn execute_while_node<'a>(
    &'a self,
    inputs: &'a AsyncNodeInputs,
    condition_template: &'a str,
    max_iterations: u32,
    template: &'a [GraphNode],
    continue_on_error: bool,
    fail_on_exhausted: bool,
    execution_config: &'a FlowExecutionConfig,
  ) -> Pin<Box<dyn Future<Output = AsyncNodeResult> + Send + 'a>> {
    Box::pin(async move {
      let mut loop_inputs = inputs.clone();
      let mut iteration_count = 0u32;
      let empty_state_pool = HashMap::new();

      while iteration_count < max_iterations {
        let condition_value =
          expr::evaluate_bool(condition_template, &empty_state_pool, &loop_inputs).map_err(
            |err| YanshiError::FlowDefinitionError {
              message: format!("Invalid while.condition '{}': {}", condition_template, err),
            },
          )?;

        if !condition_value {
          // D11 (W2.4): a normal condition-false exit is distinguishable
          // from exhaustion via `exhausted: false` on the outputs.
          loop_inputs.insert(
            "iterations_used".to_string(),
            FlowValue::Json(Value::from(iteration_count)),
          );
          loop_inputs.insert("exhausted".to_string(), FlowValue::Json(Value::Bool(false)));
          return Ok(loop_inputs);
        }

        tracing::info!(
          event = "while_loop_iteration_started",
          iteration = iteration_count + 1,
          max_iterations,
          "While loop starting iteration"
        );

        let (sub_flow, sub_config) = self.sub_flow_execution_context(template, execution_config);
        let sub_flow_state_pool = FlowExecutor::new(&sub_flow)
          .execute_from_inputs_with_config(loop_inputs.clone(), sub_config)
          .await?;

        let exit_nodes = FlowExecutor::new(&sub_flow).find_exit_nodes();
        let mut next_loop_inputs = AsyncNodeInputs::new();
        for node_id in &exit_nodes {
          match sub_flow_state_pool.get(node_id) {
            Some(Ok(outputs)) => {
              next_loop_inputs.extend(outputs.clone());
            }
            Some(Err(err)) => {
              // D11 (W2.4): pre-fix this was swallowed at debug level and
              // the loop just kept going with whatever partial outputs it
              // had. Default now surfaces it as a While-node-level error;
              // `continue_on_error` opts back into the old behavior.
              if !continue_on_error {
                return Err(YanshiError::NodeExecutionFailed {
                  message: format!(
                    "While loop sub-flow node '{}' failed on iteration {}: {}",
                    node_id,
                    iteration_count + 1,
                    err
                  ),
                });
              }
              tracing::warn!(
                event = "while_loop_iteration_node_failed",
                iteration = iteration_count + 1,
                node_id = %node_id,
                error = %err,
                "While loop sub-flow node failed; continuing (continue_on_error=true)"
              );
            }
            None => {}
          }
        }
        loop_inputs.extend(next_loop_inputs);

        iteration_count += 1;
      }

      // D11 (W2.4): reached max_iterations without the condition ever
      // evaluating false — exhaustion, not a normal exit.
      if fail_on_exhausted {
        return Err(YanshiError::NodeExecutionFailed {
          message: format!(
            "While loop exhausted max_iterations ({}) without its condition evaluating false",
            max_iterations
          ),
        });
      }
      tracing::warn!(
        event = "while_loop_exhausted",
        max_iterations,
        "While loop exhausted max_iterations without its condition evaluating false"
      );
      loop_inputs.insert(
        "iterations_used".to_string(),
        FlowValue::Json(Value::from(iteration_count)),
      );
      loop_inputs.insert("exhausted".to_string(), FlowValue::Json(Value::Bool(true)));
      Ok(loop_inputs)
    })
  }

  fn execute_map_node_sequential<'a>(
    &'a self,
    inputs: &'a AsyncNodeInputs,
    template: &'a [GraphNode],
    execution_config: &'a FlowExecutionConfig,
  ) -> Pin<Box<dyn Future<Output = AsyncNodeResult> + Send + 'a>> {
    Box::pin(async move {
      let input_list = match inputs.get("input_list") {
        Some(FlowValue::Json(Value::Array(arr))) => arr,
        _ => {
          return Err(YanshiError::NodeInputError {
            message: "Input 'input_list' must be a JSON array for a Map node".to_string(),
          });
        }
      };

      let mut all_results = Vec::new();
      let mut err_indexes: Vec<usize> = Vec::new();
      for (idx, item) in input_list.iter().enumerate() {
        let (sub_flow, sub_config) = self.sub_flow_execution_context(template, execution_config);
        let mut initial_inputs = HashMap::new();
        initial_inputs.insert("item".to_string(), FlowValue::Json(item.clone()));

        let sub_flow_result = FlowExecutor::new(&sub_flow)
          .execute_from_inputs_with_config(initial_inputs, sub_config)
          .await?;
        // F-A6-3: track per-sub-flow node-level failures (see the
        // parallel branch for the design rationale).
        if sub_flow_result.values().any(|r| r.is_err()) {
          err_indexes.push(idx);
        }
        let json_state = serde_json::to_value(sub_flow_result)?;
        all_results.push(json_state);
      }

      Ok(map_outputs_with_summary(all_results, err_indexes))
    })
  }

  fn execute_map_node_parallel<'a>(
    &'a self,
    inputs: &'a AsyncNodeInputs,
    template: &'a [GraphNode],
    max_concurrent: Option<usize>,
    execution_config: &'a FlowExecutionConfig,
  ) -> Pin<Box<dyn Future<Output = AsyncNodeResult> + Send + 'a>> {
    Box::pin(async move {
      let input_list = match inputs.get("input_list") {
        Some(FlowValue::Json(Value::Array(arr))) => arr.clone(),
        _ => {
          return Err(YanshiError::NodeInputError {
            message: "Input 'input_list' must be a JSON array for a Map node".to_string(),
          });
        }
      };

      // F-A6-1: when `max_concurrent` is set, every spawned sub-flow
      // must acquire a permit from a shared `Semaphore` before
      // starting work. Unbounded mode (`None`) is preserved for
      // back-compat — existing callers that didn't pass the field
      // get the legacy "spawn everything" behaviour, which is fine
      // for small N but pre-existing callers that ran into provider
      // rate limits should switch to a bounded form. `0` is treated
      // as a configuration error (would deadlock).
      let semaphore = match max_concurrent {
        Some(0) => {
          return Err(YanshiError::NodeInputError {
            message: "Map node 'max_concurrent' must be >= 1 (got 0)".to_string(),
          });
        }
        Some(n) => Some(Arc::new(tokio::sync::Semaphore::new(n))),
        None => None,
      };

      let mut handles = Vec::new();
      // Aborts every item still running if this future is dropped (an
      // outer timeout, a cancelled caller, or a retry about to re-run the
      // node); a no-op for tasks that already finished.
      let mut abort_on_drop = AbortOnDrop(Vec::new());
      for item in input_list {
        let (sub_flow, sub_config) = self.sub_flow_execution_context(template, execution_config);
        let mut initial_inputs = HashMap::new();
        initial_inputs.insert("item".to_string(), FlowValue::Json(item.clone()));

        let permit_holder = semaphore.clone();
        // Y1.1: task-locals don't cross `tokio::spawn`; carry the parent
        // run's artifact root into the sub-flow task so Map items file
        // their artifacts under the parent run, not their own uuid dir.
        let artifact_root = crate::run_context::current_artifact_root();
        // Y2.3: same for the cost meter, so item spend counts toward the
        // Map node.
        let cost_meter = crate::run_context::current_cost_meter();
        let handle = tokio::spawn(crate::run_context::with_cost_meter(
          cost_meter,
          crate::run_context::with_inherited_artifact_root(artifact_root, async move {
            // Hold the permit for the entire sub-flow execution so the
            // concurrent count is a tight upper bound, not just a
            // start-rate cap. Acquired permits drop on task end (any
            // exit path) so cancellation / errors release them.
            let _permit = match permit_holder.as_ref() {
              Some(sem) => match sem.clone().acquire_owned().await {
                Ok(p) => Some(p),
                Err(_) => {
                  return Err(YanshiError::FlowExecutionFailed {
                    message: "Map semaphore closed before sub-flow could acquire permit"
                      .to_string(),
                  });
                }
              },
              None => None,
            };
            FlowExecutor::new(&sub_flow)
              .execute_from_inputs_with_config(initial_inputs, sub_config)
              .await
          }),
        ));
        abort_on_drop.0.push(handle.abort_handle());
        handles.push(handle);
      }

      let results = futures::future::join_all(handles).await;

      let mut all_results = Vec::new();
      let mut err_indexes: Vec<usize> = Vec::new();
      for (idx, result) in results.into_iter().enumerate() {
        match result {
          Ok(Ok(sub_flow_result)) => {
            // F-A6-3: per-sub-flow Err states (a node inside the
            // sub-flow returned Err) are otherwise buried inside
            // `results[i]` as nested `Err` JSON. Walk the state pool
            // here so we can emit a top-level `results_summary` that
            // downstream nodes / operators can route on without
            // re-parsing the nested JSON.
            let had_err = sub_flow_result.values().any(|r| r.is_err());
            if had_err {
              err_indexes.push(idx);
            }
            let json_state = serde_json::to_value(sub_flow_result)?;
            all_results.push(json_state);
          }
          Ok(Err(e)) => return Err(e),
          Err(e) => {
            return Err(YanshiError::FlowExecutionFailed {
              message: e.to_string(),
            });
          }
        }
      }

      Ok(map_outputs_with_summary(all_results, err_indexes))
    })
  }

  fn persist_step_result(
    &self,
    run_dir: &Path,
    node_id: &str,
    result: &AsyncNodeResult,
  ) -> Result<(), YanshiError> {
    // V0.4: `node_id` comes from a workflow author's YAML `id:` field,
    // not a value this crate controls — reject anything that could
    // escape `run_dir` (e.g. an id containing `..` or `/`) before
    // writing to the computed path.
    if !is_safe_path_component(node_id) {
      return Err(YanshiError::ConfigurationError {
        message: format!(
          "invalid node id '{}': must be a single path segment (no '/', '\\', '.', or '..')",
          node_id
        ),
      });
    }
    let file_path = run_dir.join(format!("{}_outputs.json", node_id));
    let content = serde_json::to_string_pretty(result)?;
    fs::write(&file_path, content).map_err(|e| YanshiError::PersistenceError {
      message: e.to_string(),
    })?;
    Ok(())
  }

  fn gather_inputs(
    &self,
    node_id: &str,
    input_mapping: &HashMap<String, (String, String)>,
    state_pool: &HashMap<String, AsyncNodeResult>,
    flow_initial_inputs: &AsyncNodeInputs,
  ) -> Result<AsyncNodeInputs, YanshiError> {
    let mut inputs = AsyncNodeInputs::new();
    for (input_name, (source_node_id, source_output_name)) in input_mapping {
      // F-A6-5: `{{ item.* }}` lookup. The factory encodes these
      // with the sentinel source-node id "!item". Resolve against
      // the flow-level initial inputs (where map seeds `item`)
      // rather than the state pool.
      if source_node_id == "!item" {
        let item_value = flow_initial_inputs.get("item").ok_or_else(|| {
          YanshiError::NodeInputError {
            message: format!(
              "input_mapping for node '{node_id}' input '{input_name}' references `item.{source_output_name}` but no `item` is in scope (only valid inside a map sub-flow)"
            ),
          }
        })?;
        let resolved =
          resolve_item_path(item_value, source_output_name).ok_or_else(|| {
            YanshiError::NodeInputError {
              message: format!(
                "input_mapping for node '{node_id}' input '{input_name}': path `item.{source_output_name}` did not resolve in the iteration item"
              ),
            }
          })?;
        inputs.insert(input_name.clone(), resolved);
        continue;
      }

      // Check if source node is in dependencies (required) or not (optional)
      let graph_node =
        self
          .flow
          .nodes()
          .get(node_id)
          .ok_or_else(|| YanshiError::FlowExecutionFailed {
            message: format!("Node '{}' not found in graph", node_id),
          })?;
      let is_required_dependency = graph_node.dependencies.contains(source_node_id);

      match state_pool.get(source_node_id) {
        Some(Ok(source_outputs)) => {
          match source_outputs.get(source_output_name) {
            Some(input_value) => {
              inputs.insert(input_name.clone(), input_value.clone());
            }
            None if !is_required_dependency => {
              // Optional input, source node exists but output key not found - skip it
              continue;
            }
            None => {
              return Err(YanshiError::NodeInputError {
                message: format!(
                  "Output '{}' not found in source node '{}'",
                  source_output_name, source_node_id
                ),
              });
            }
          }
        }
        Some(Err(YanshiError::NodeSkipped)) if !is_required_dependency => {
          // Optional dependency was skipped - skip this input
          continue;
        }
        Some(Err(YanshiError::NodeSkipped)) => {
          // Required dependency was skipped - error
          return Err(YanshiError::DependencyNotMet {
            node_id: node_id.to_string(),
            dependency_id: source_node_id.clone(),
          });
        }
        Some(Err(e)) => return Err(e.clone()),
        None if !is_required_dependency => {
          // Optional dependency not executed - skip this input
          continue;
        }
        None => {
          return Err(YanshiError::FlowExecutionFailed {
            message: format!(
              "Dependency node '{}' has not been executed.",
              source_node_id
            ),
          });
        }
      }
    }
    Ok(inputs)
  }

  fn evaluate_condition(
    &self,
    condition: &str,
    state_pool: &HashMap<String, AsyncNodeResult>,
  ) -> Result<bool, YanshiError> {
    let normalized = expr::normalize_expression(condition);
    tracing::debug!("Evaluating condition: '{}'", normalized);
    expr::evaluate_bool(condition, state_pool, &HashMap::new()).map_err(|err| {
      YanshiError::FlowDefinitionError {
        message: format!("Invalid run_if '{}': {}", condition, err),
      }
    })
  }

  fn find_exit_nodes(&self) -> Vec<String> {
    let mut all_deps = HashSet::new();
    for node in self.flow.nodes().values() {
      for dep in &node.dependencies {
        all_deps.insert(dep.as_str());
      }
    }
    self
      .flow
      .nodes()
      .keys()
      .filter(|id| !all_deps.contains(id.as_str()))
      .cloned()
      .collect()
  }

  /// Siblings `node` reads through `input_mapping` or `run_if` without
  /// declaring them in `dependencies`. Scheduling orders `node` after them
  /// so the read sees their result; input resolution still treats them as
  /// optional (eval 2026-10-07 D5 — concurrent mode used to start `node`
  /// first and silently drop the input).
  fn undeclared_references(&self, node_id: &str, node: &GraphNode) -> Vec<String> {
    let nodes = self.flow.nodes();
    let mut references: Vec<String> = Vec::new();
    let mut add = |source: &str| {
      if source != node_id
        && nodes.contains_key(source)
        && !node.dependencies.iter().any(|dep| dep == source)
        && !references.iter().any(|seen| seen == source)
      {
        references.push(source.to_string());
      }
    };
    if let Some(mapping) = &node.input_mapping {
      for (source, _) in mapping.values() {
        add(source);
      }
    }
    if let Some(condition) = &node.run_if {
      let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
      for (start, _) in condition.match_indices("nodes.") {
        if condition[..start].chars().next_back().is_some_and(is_ident) {
          continue;
        }
        let rest = &condition[start + "nodes.".len()..];
        let end = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
        add(&rest[..end]);
      }
    }
    references
  }

  fn topological_sort(&self) -> Result<Vec<String>, YanshiError> {
    // Q2.4.1: use BTreeMap so iteration order is deterministic by
    // node id. Pre-fix the in_degree and adjacency maps were
    // HashMaps, so the order in which roots (in_degree == 0) entered
    // the queue depended on HashMap's hashing/resizing state. That
    // made trace replay non-reproducible across runs even when the
    // workflow graph was identical.
    let mut in_degree: std::collections::BTreeMap<String, usize> = self
      .flow
      .nodes()
      .keys()
      .cloned()
      .map(|id| (id, 0))
      .collect();
    let mut adj: std::collections::BTreeMap<String, Vec<String>> = self
      .flow
      .nodes()
      .keys()
      .cloned()
      .map(|id| (id, vec![]))
      .collect();

    // Iterate the nodes deterministically too — sorted by node id —
    // so the adjacency-list construction is reproducible.
    let mut node_ids: Vec<&String> = self.flow.nodes().keys().collect();
    node_ids.sort();
    for id in node_ids {
      let node = &self.flow.nodes()[id];
      for dep_id in &node.dependencies {
        if !self.flow.nodes().contains_key(dep_id) {
          return Err(YanshiError::FlowDefinitionError {
            message: format!("Node '{}' has an invalid dependency: '{}'", id, dep_id),
          });
        }
        in_degree.entry(id.clone()).and_modify(|d| *d += 1);
        adj.entry(dep_id.clone()).or_default().push(id.clone());
      }
      for source in self.undeclared_references(id, node) {
        in_degree.entry(id.clone()).and_modify(|d| *d += 1);
        adj.entry(source).or_default().push(id.clone());
      }
    }

    // BTreeMap iteration is already sorted, so the initial queue
    // contents are deterministic.
    let mut queue: VecDeque<String> = in_degree
      .iter()
      .filter(|&(_, &d)| d == 0)
      .map(|(id, _)| id.clone())
      .collect();

    let mut sorted_order = Vec::new();
    while let Some(u) = queue.pop_front() {
      sorted_order.push(u.clone());
      if let Some(neighbors) = adj.get(&u) {
        // Sort neighbors to keep enqueue order deterministic.
        let mut sorted_neighbors: Vec<&String> = neighbors.iter().collect();
        sorted_neighbors.sort();
        for v in sorted_neighbors {
          in_degree.entry(v.clone()).and_modify(|d| *d -= 1);
          if *in_degree.get(v).unwrap_or(&1) == 0 {
            queue.push_back(v.clone());
          }
        }
      }
    }

    if sorted_order.len() != self.flow.nodes().len() {
      Err(YanshiError::CircularFlow)
    } else {
      Ok(sorted_order)
    }
  }
}

/// F-A6-5: walk a dotted path inside the JSON value of the
/// iteration `item` to resolve `{{ item.foo.bar }}` lookups in
/// input_mapping. Returns `None` if any segment is missing or the
/// item isn't a JSON object at the right point in the path.
///
/// Strings unwrap into `FlowValue::Json(String)` (so a path like
/// `item.read_path` becomes the literal string the downstream
/// FileNode wants in its `path` input). Other JSON types pass
/// through wrapped in `FlowValue::Json` so callers can still
/// receive e.g. structured objects when that's what they intend.
fn resolve_item_path(item_value: &FlowValue, dotted_path: &str) -> Option<FlowValue> {
  let mut cursor = match item_value {
    FlowValue::Json(v) => v,
    _ => return None,
  };
  for segment in dotted_path.split('.') {
    cursor = cursor.as_object()?.get(segment)?;
  }
  Some(FlowValue::Json(cursor.clone()))
}

/// Assemble the standard map-node output map plus the F-A6-3
/// `results_summary` sibling.
///
/// `results` keeps its legacy shape (a JSON array of sub-flow state
/// pools, one per input item) so existing workflows that already
/// route on it continue to work unchanged. `results_summary`
/// surfaces the partial-failure shape: `{total, ok, err,
/// err_indexes}`, suitable for `run_if` expressions or for
/// downstream nodes that need to react to any per-sub-flow failure
/// without re-parsing the nested `results` JSON.
///
/// Emits a `tracing::warn!` when any sub-flow had a node-level failure
/// so operators see the partial failure even when nothing downstream
/// routes on the summary.
fn map_outputs_with_summary(
  all_results: Vec<Value>,
  err_indexes: Vec<usize>,
) -> HashMap<String, FlowValue> {
  let total = all_results.len();
  let err = err_indexes.len();
  let ok = total.saturating_sub(err);

  if err > 0 {
    tracing::warn!(
      "Map node: {err} of {total} sub-flows had at least one node-level error (err_indexes={err_indexes:?}). See results[i] for details, or branch on results_summary.err_indexes."
    );
  }

  let summary = serde_json::json!({
    "total": total,
    "ok": ok,
    "err": err,
    "err_indexes": err_indexes,
  });

  let mut outputs = HashMap::new();
  outputs.insert(
    "results".to_string(),
    FlowValue::Json(Value::Array(all_results)),
  );
  outputs.insert("results_summary".to_string(), FlowValue::Json(summary));
  outputs
}

/// The execution surface for a [`Flow`]: run / resume / execute, plus the
/// checkpoint builders that need the (core-side) checkpoint manager.
///
/// The `Flow` IR lives in `yanshi-graph`; this trait is the executor half of
/// the IR ≠ executor split (P-A1.3, RFC §5). Bring it into scope
/// (`use yanshi_core::FlowExt`) to run a flow. Every method delegates to the
/// internal `FlowExecutor`.
pub trait FlowExt: Sized {
  /// Enable checkpointing with a custom configuration (validated eagerly).
  fn with_checkpointing(self, config: CheckpointConfig) -> Result<Self, YanshiError>;
  /// Enable checkpointing with the default configuration.
  fn with_default_checkpointing(self) -> Result<Self, YanshiError>;
  /// The workflow execution order after dependency validation (no execution).
  fn execution_order(&self) -> Result<Vec<String>, YanshiError>;
  /// Run the workflow from empty inputs.
  fn run(
    &self,
  ) -> impl std::future::Future<Output = Result<HashMap<String, AsyncNodeResult>, YanshiError>> + Send;
  /// Resume from the latest checkpoint (default options).
  fn resume(
    &self,
    workflow_id: &str,
  ) -> impl std::future::Future<Output = Result<HashMap<String, AsyncNodeResult>, YanshiError>> + Send;
  /// Resume from the latest checkpoint with explicit options.
  fn resume_with_options(
    &self,
    workflow_id: &str,
    options: &ResumePlanOptions,
  ) -> impl std::future::Future<Output = Result<HashMap<String, AsyncNodeResult>, YanshiError>> + Send;
  /// Load the resume plan without executing.
  fn load_resume_plan(
    &self,
    workflow_id: &str,
    options: &ResumePlanOptions,
  ) -> impl std::future::Future<Output = Result<ResumePlan, YanshiError>> + Send;
  /// Execute from explicit initial inputs.
  fn execute_from_inputs(
    &self,
    initial_inputs: AsyncNodeInputs,
  ) -> impl std::future::Future<Output = Result<HashMap<String, AsyncNodeResult>, YanshiError>> + Send;
  /// Execute from initial inputs with an execution config.
  fn execute_from_inputs_with_config(
    &self,
    initial_inputs: AsyncNodeInputs,
    config: FlowExecutionConfig,
  ) -> impl std::future::Future<Output = Result<HashMap<String, AsyncNodeResult>, YanshiError>> + Send;
  /// Execute with an explicit workflow id + execution config.
  fn execute_from_inputs_with_id_and_config(
    &self,
    workflow_id: String,
    initial_inputs: AsyncNodeInputs,
    config: FlowExecutionConfig,
  ) -> impl std::future::Future<Output = Result<HashMap<String, AsyncNodeResult>, YanshiError>> + Send;
}

impl FlowExt for Flow {
  fn with_checkpointing(self, config: CheckpointConfig) -> Result<Self, YanshiError> {
    // Validate eagerly (build a manager to fail fast on a bad checkpoint dir),
    // then store the config on the IR via the graph-side setter.
    let _ = CheckpointManager::new(config.clone())?;
    Ok(self.with_checkpoint_config(config))
  }

  fn with_default_checkpointing(self) -> Result<Self, YanshiError> {
    FlowExt::with_checkpointing(self, CheckpointConfig::default())
  }

  fn execution_order(&self) -> Result<Vec<String>, YanshiError> {
    FlowExecutor::new(self).execution_order()
  }

  async fn run(&self) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    FlowExecutor::new(self).run().await
  }

  async fn resume(
    &self,
    workflow_id: &str,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    FlowExecutor::new(self).resume(workflow_id).await
  }

  async fn resume_with_options(
    &self,
    workflow_id: &str,
    options: &ResumePlanOptions,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    FlowExecutor::new(self)
      .resume_with_options(workflow_id, options)
      .await
  }

  async fn load_resume_plan(
    &self,
    workflow_id: &str,
    options: &ResumePlanOptions,
  ) -> Result<ResumePlan, YanshiError> {
    FlowExecutor::new(self)
      .load_resume_plan(workflow_id, options)
      .await
  }

  async fn execute_from_inputs(
    &self,
    initial_inputs: AsyncNodeInputs,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    FlowExecutor::new(self)
      .execute_from_inputs(initial_inputs)
      .await
  }

  async fn execute_from_inputs_with_config(
    &self,
    initial_inputs: AsyncNodeInputs,
    config: FlowExecutionConfig,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    FlowExecutor::new(self)
      .execute_from_inputs_with_config(initial_inputs, config)
      .await
  }

  async fn execute_from_inputs_with_id_and_config(
    &self,
    workflow_id: String,
    initial_inputs: AsyncNodeInputs,
    config: FlowExecutionConfig,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    FlowExecutor::new(self)
      .execute_from_inputs_with_id_and_config(workflow_id, initial_inputs, config)
      .await
  }
}

/// Forwards a Map/While sub-flow's node-level events to the parent's
/// listener but drops the sub-flow's own workflow lifecycle events: each
/// item/iteration is an internal sub-run, not a workflow. Forwarding them
/// counted every item as a completed workflow and made the trace
/// collector open an orphan trace per item (eval 2026-10-07 D3).
struct SubFlowListener(Arc<dyn EventListener>);

impl EventListener for SubFlowListener {
  fn on_event(&self, event: &WorkflowEvent) {
    if !matches!(
      event,
      WorkflowEvent::WorkflowStarted { .. }
        | WorkflowEvent::WorkflowCompleted { .. }
        | WorkflowEvent::WorkflowFailed { .. }
        | WorkflowEvent::WorkflowCancelled { .. }
    ) {
      self.0.on_event(event);
    }
  }
}

/// Aborts the tasks behind its handles when dropped.
struct AbortOnDrop(Vec<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
  fn drop(&mut self) {
    for handle in &self.0 {
      handle.abort();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  // The IR types moved to yanshi-graph, so these traits are no longer
  // imported at module top (only the tests construct nodes / listeners).
  use crate::async_node::AsyncNode;
  use crate::events::EventListener;
  use async_trait::async_trait;
  use serde_json::json;
  use std::sync::Mutex;
  use tempfile::TempDir;

  struct RecordingListener {
    events: Arc<Mutex<Vec<&'static str>>>,
  }

  impl EventListener for RecordingListener {
    fn on_event(&self, event: &WorkflowEvent) {
      self.events.lock().unwrap().push(event.event_type());
    }
  }

  fn use_writable_home() {
    let home = std::env::temp_dir().join(format!("yanshi-flow-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: tests set HOME before invoking code that reads it; no other
    // thread in these tests concurrently mutates the process environment.
    unsafe {
      std::env::set_var("HOME", home);
    }
  }

  async fn load_only_latest_checkpoint(temp_dir: &TempDir) -> Checkpoint {
    let workflow_dirs = std::fs::read_dir(temp_dir.path())
      .unwrap()
      .collect::<Result<Vec<_>, _>>()
      .unwrap();
    assert_eq!(
      workflow_dirs.len(),
      1,
      "expected exactly one checkpoint workflow directory"
    );
    let workflow_id = workflow_dirs[0].file_name().to_string_lossy().into_owned();
    let manager = CheckpointManager::new(
      CheckpointConfig::default()
        .with_checkpoint_dir(temp_dir.path())
        .with_auto_cleanup(false),
    )
    .unwrap();
    manager
      .load_latest_checkpoint(&workflow_id)
      .await
      .unwrap()
      .unwrap()
  }

  /// V0.3 regression: `is_genuine_failure` must treat a benign `run_if`
  /// skip as non-failing while still treating every other `Err` variant
  /// (including a partial-execution failure) as a genuine failure.
  #[test]
  fn is_genuine_failure_treats_only_skip_as_benign() {
    assert!(!is_genuine_failure(&Ok(HashMap::new())));
    assert!(!is_genuine_failure(&Err(YanshiError::NodeSkipped)));
    assert!(is_genuine_failure(&Err(YanshiError::NodeExecutionFailed {
      message: "boom".to_string(),
    })));
    assert!(is_genuine_failure(&Err(
      YanshiError::NodePartialExecutionFailed {
        message: "partial".to_string(),
        partial_outputs: HashMap::new(),
      }
    )));
  }

  /// V0.3 regression: when a genuine failure and an unrelated benign skip
  /// coexist in the same state pool, the representative error message
  /// picked for `WorkflowEvent::WorkflowFailed` must be the genuine
  /// failure's — never the skip's "Node was skipped due to a condition."
  /// placeholder (which `HashMap` iteration order could otherwise surface
  /// nondeterministically).
  #[test]
  fn representative_failure_message_ignores_benign_skips() {
    let mut state_pool: HashMap<String, AsyncNodeResult> = HashMap::new();
    state_pool.insert("skipped".to_string(), Err(YanshiError::NodeSkipped));
    state_pool.insert(
      "failed".to_string(),
      Err(YanshiError::NodeExecutionFailed {
        message: "boom".to_string(),
      }),
    );

    let message = representative_failure_message(&state_pool);
    assert!(message.contains("boom"), "got: {message}");
    assert!(!message.contains("skipped"), "got: {message}");
  }

  #[test]
  fn checkpoint_state_roundtrips_flowvalue_variants() {
    let file_value = FlowValue::File {
      path: "/tmp/yanshi-report.txt".into(),
      mime_type: Some("text/plain".to_string()),
    };
    let url_value = FlowValue::Url {
      url: "https://example.test/data.json".to_string(),
      mime_type: Some("application/json".to_string()),
    };

    let mut outputs = HashMap::new();
    outputs.insert("json".to_string(), FlowValue::Json(json!({"ok": true})));
    outputs.insert("file".to_string(), file_value.clone());
    outputs.insert("url".to_string(), url_value.clone());

    let mut state_pool = HashMap::new();
    state_pool.insert("node".to_string(), Ok(outputs.clone()));

    let checkpoint_state =
      FlowExecutor::new(&Flow::default()).state_pool_to_checkpoint_state(&state_pool);
    let raw_node = checkpoint_state
      .get("node")
      .and_then(serde_json::Value::as_object)
      .unwrap();

    assert_eq!(raw_node["json"]["type"], json!("json"));
    assert_eq!(raw_node["json"]["value"], json!({"ok": true}));
    assert_eq!(raw_node["file"]["type"], json!("file"));
    assert_eq!(raw_node["url"]["type"], json!("url"));

    let restored = FlowExecutor::checkpoint_state_to_state_pool(&checkpoint_state);
    let restored_outputs = restored.get("node").unwrap().as_ref().unwrap();

    assert_eq!(restored_outputs.get("json"), outputs.get("json"));
    assert_eq!(restored_outputs.get("file"), Some(&file_value));
    assert_eq!(restored_outputs.get("url"), Some(&url_value));
  }

  #[test]
  fn legacy_untagged_checkpoint_values_decode_as_json() {
    // Pre-tag-schema checkpoints stored raw JSON without the
    // `{"type": "json", "value": ...}` envelope. The fallback must
    // still accept them, wrapping into `FlowValue::Json` without
    // warning.
    let mut node = serde_json::Map::new();
    node.insert("legacy_string".to_string(), json!("hello"));
    node.insert("legacy_number".to_string(), json!(42));
    node.insert("legacy_object".to_string(), json!({"answer": 42}));
    node.insert("legacy_array".to_string(), json!([1, 2, 3]));

    let mut checkpoint_state = HashMap::new();
    checkpoint_state.insert("legacy_node".to_string(), Value::Object(node));

    let restored = FlowExecutor::checkpoint_state_to_state_pool(&checkpoint_state);
    let outputs = restored.get("legacy_node").unwrap().as_ref().unwrap();

    assert_eq!(
      outputs.get("legacy_string"),
      Some(&FlowValue::Json(json!("hello")))
    );
    assert_eq!(
      outputs.get("legacy_number"),
      Some(&FlowValue::Json(json!(42)))
    );
    assert_eq!(
      outputs.get("legacy_object"),
      Some(&FlowValue::Json(json!({"answer": 42})))
    );
    assert_eq!(
      outputs.get("legacy_array"),
      Some(&FlowValue::Json(json!([1, 2, 3])))
    );
  }

  /// V1.2 regression: a benign `run_if` skip used to be dropped from the
  /// checkpoint entirely (no entry at all), indistinguishable from "this
  /// node never ran". It must now round-trip as `Err(NodeSkipped)`.
  #[test]
  fn checkpoint_state_round_trip_preserves_node_skipped() {
    let mut state_pool: HashMap<String, AsyncNodeResult> = HashMap::new();
    state_pool.insert("skipped_node".to_string(), Err(YanshiError::NodeSkipped));
    let mut ok_outputs = HashMap::new();
    ok_outputs.insert("value".to_string(), FlowValue::Json(json!("ok")));
    state_pool.insert("ok_node".to_string(), Ok(ok_outputs));

    let checkpoint_state =
      FlowExecutor::new(&Flow::default()).state_pool_to_checkpoint_state(&state_pool);
    assert!(
      checkpoint_state.contains_key("skipped_node"),
      "a skipped node must now have a checkpoint entry at all (pre-V1.2 it had none)"
    );

    let restored = FlowExecutor::checkpoint_state_to_state_pool(&checkpoint_state);
    assert!(matches!(
      restored.get("skipped_node"),
      Some(Err(YanshiError::NodeSkipped))
    ));
    assert_eq!(
      restored
        .get("ok_node")
        .unwrap()
        .as_ref()
        .unwrap()
        .get("value"),
      Some(&FlowValue::Json(json!("ok")))
    );
  }

  /// V1.2 regression: `NodePartialExecutionFailed` used to be written
  /// with the exact same bare shape as a full `Ok`, so decoding it back
  /// silently upgraded a partial failure to a complete success. It must
  /// now round-trip as `Err(NodePartialExecutionFailed { message,
  /// partial_outputs })`, and the reserved tag entries injected to make
  /// that possible must not leak into `partial_outputs`.
  #[test]
  fn checkpoint_state_round_trip_preserves_partial_failure_not_upgraded_to_ok() {
    let mut partial_outputs = HashMap::new();
    partial_outputs.insert(
      "agent_result".to_string(),
      FlowValue::Json(json!({"steps": ["observe", "tool_call"]})),
    );
    let mut state_pool: HashMap<String, AsyncNodeResult> = HashMap::new();
    state_pool.insert(
      "agent".to_string(),
      Err(YanshiError::NodePartialExecutionFailed {
        message: "interrupted mid-tool-call".to_string(),
        partial_outputs,
      }),
    );

    let checkpoint_state =
      FlowExecutor::new(&Flow::default()).state_pool_to_checkpoint_state(&state_pool);
    // The bare-shape contract other tooling depends on: the real output
    // key is still directly accessible, not nested under a wrapper.
    assert_eq!(
      checkpoint_state["agent"]["agent_result"]["value"]["steps"][1],
      json!("tool_call")
    );

    let restored = FlowExecutor::checkpoint_state_to_state_pool(&checkpoint_state);
    match restored.get("agent") {
      Some(Err(YanshiError::NodePartialExecutionFailed {
        message,
        partial_outputs,
      })) => {
        assert_eq!(message, "interrupted mid-tool-call");
        assert_eq!(partial_outputs.len(), 1, "tag entries must not leak in");
        assert!(partial_outputs.contains_key("agent_result"));
      }
      other => panic!("expected NodePartialExecutionFailed, got {other:?}"),
    }
  }

  /// V1.2 regression (evaluation §六 P1-8): a checkpoint saved mid-flight
  /// under concurrent execution only records `last_completed_node` as
  /// whichever node happened to finish most recently in wall-clock
  /// time — not "everything topologically before it is done". Here
  /// `zzz_finished_first` (topologically *last*) completed and
  /// triggered the checkpoint save while `aaa_never_ran` (topologically
  /// *earlier*, but still in flight) had not. The old topological-index
  /// cutoff resumed from `last_completed_node`'s index + 1 — past the
  /// end of the node list here — and wrongly declared the whole
  /// workflow already complete, permanently dropping `aaa_never_ran`.
  #[tokio::test]
  async fn resume_executes_a_topologically_earlier_node_that_never_ran_under_concurrent_execution()
  {
    use_writable_home();

    struct StaticNode {
      key: &'static str,
      value: serde_json::Value,
    }

    #[async_trait]
    impl AsyncNode for StaticNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert(self.key.to_string(), FlowValue::Json(self.value.clone()));
        Ok(outputs)
      }
    }

    let root = GraphNode {
      id: "root".to_string(),
      node_type: NodeType::Standard(Arc::new(StaticNode {
        key: "value",
        value: json!("root"),
      })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let aaa_never_ran = GraphNode {
      id: "aaa_never_ran".to_string(),
      node_type: NodeType::Standard(Arc::new(StaticNode {
        key: "value",
        value: json!("aaa"),
      })),
      dependencies: vec!["root".to_string()],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let zzz_finished_first = GraphNode {
      id: "zzz_finished_first".to_string(),
      node_type: NodeType::Standard(Arc::new(StaticNode {
        key: "value",
        value: json!("zzz"),
      })),
      dependencies: vec!["root".to_string()],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let temp_dir = TempDir::new().unwrap();
    let config = CheckpointConfig::default()
      .with_checkpoint_dir(temp_dir.path())
      .with_auto_cleanup(false);
    let manager = CheckpointManager::new(config.clone()).unwrap();

    // Hand-construct the exact checkpoint a crash mid-concurrent-run
    // would leave: `zzz_finished_first` recorded as `last_completed_node`
    // (it happened to finish first), `aaa_never_ran` entirely absent
    // (never got to run).
    let mut state = HashMap::new();
    state.insert(
      "root".to_string(),
      json!({"value": {"type": "json", "value": "root"}}),
    );
    state.insert(
      "zzz_finished_first".to_string(),
      json!({"value": {"type": "json", "value": "zzz"}}),
    );
    manager
      .save_checkpoint("resume-concurrent-workflow", "zzz_finished_first", &state)
      .await
      .unwrap();

    let flow = Flow::new(vec![root, aaa_never_ran, zzz_finished_first])
      .with_checkpointing(config)
      .unwrap();
    let resumed = flow.resume("resume-concurrent-workflow").await.unwrap();

    assert!(
      resumed.get("aaa_never_ran").is_some_and(|r| r.is_ok()),
      "a node that never actually ran must be executed on resume, \
       regardless of its topological position relative to last_completed_node: {resumed:?}"
    );
  }

  #[test]
  fn malformed_tagged_checkpoint_value_falls_back_to_json() {
    // A checkpoint claims `type: "file"` but is missing the required
    // `path` field. The decoder must NOT panic and must NOT pretend
    // it decoded successfully — it falls back to `FlowValue::Json`
    // (preserving the raw object) while loudly logging the
    // regression so operators can investigate. Pre-bridge code did
    // the same fallback silently, swallowing corruption.
    let malformed = json!({
      "type": "file",
      "mime_type": "text/plain"
      // `path` is missing on purpose
    });
    let restored = decode_checkpoint_flow_value("node", "file_output", &malformed);

    match restored {
      FlowValue::Json(value) => {
        assert_eq!(
          value, malformed,
          "fallback must preserve the raw object so operators can inspect it"
        );
      }
      other => panic!(
        "malformed tagged value must fall back to FlowValue::Json (so resume can still proceed), got {:?}",
        other
      ),
    }
  }

  #[tokio::test]
  async fn cancellation_token_stops_flow_before_first_node() {
    use_writable_home();
    struct NeverNode;
    #[async_trait]
    impl AsyncNode for NeverNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        panic!("cancelled flow should not execute nodes");
      }
    }

    let events = Arc::new(Mutex::new(Vec::new()));
    let token = crate::scheduler::FlowCancellationToken::new();
    token.cancel();
    let mut flow = Flow::default().with_event_listener(Arc::new(RecordingListener {
      events: events.clone(),
    }));
    flow.add_node(GraphNode {
      id: "never".to_string(),
      node_type: NodeType::Standard(Arc::new(NeverNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    });

    let result = flow
      .execute_from_inputs_with_config(
        HashMap::new(),
        FlowExecutionConfig::serial().with_cancellation_token(token),
      )
      .await;

    assert!(matches!(result, Err(YanshiError::TaskCancelled)));
    assert_eq!(
      *events.lock().unwrap(),
      vec!["workflow.started", "workflow.cancelled"]
    );
  }

  #[tokio::test]
  async fn test_map_node_sequential_execution() {
    use_writable_home();
    struct MultiplyNode;
    #[async_trait]
    impl AsyncNode for MultiplyNode {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let val = match inputs.get("item").unwrap() {
          FlowValue::Json(Value::Number(n)) => n.as_i64().unwrap(),
          _ => 0,
        };
        let mut outputs = HashMap::new();
        outputs.insert("result".to_string(), FlowValue::Json(json!(val * 2)));
        Ok(outputs)
      }
    }

    let sub_flow_node = GraphNode {
      id: "multiply".to_string(),
      node_type: NodeType::Standard(Arc::new(MultiplyNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "map_node".to_string(),
      node_type: NodeType::Map {
        template: vec![sub_flow_node],
        parallel: false,
        max_concurrent: None,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("input_list".to_string(), FlowValue::Json(json!([1, 2, 3])));
        inputs
      },
    };

    let flow = Flow::new(vec![map_node]);

    let final_state = flow.run().await.unwrap();
    let map_result = final_state.get("map_node").unwrap().as_ref().unwrap();
    let results_array = match map_result.get("results").unwrap() {
      FlowValue::Json(Value::Array(arr)) => arr,
      _ => panic!("Not an array"),
    };

    assert_eq!(results_array.len(), 3);
  }

  #[tokio::test]
  async fn test_flow_emits_workflow_node_and_output_events() {
    use_writable_home();
    struct TestNode;
    #[async_trait]
    impl AsyncNode for TestNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert("result".to_string(), FlowValue::Json(json!("ok")));
        Ok(outputs)
      }
    }

    let node = GraphNode {
      id: "node".to_string(),
      node_type: NodeType::Standard(Arc::new(TestNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });

    Flow::new(vec![node])
      .with_event_listener(listener)
      .run()
      .await
      .unwrap();

    let events = events.lock().unwrap().clone();
    assert!(events.contains(&"workflow.started"));
    assert!(events.contains(&"node.started"));
    assert!(events.contains(&"node.output.captured"));
    assert!(events.contains(&"node.completed"));
    assert!(events.contains(&"workflow.completed"));
  }

  #[tokio::test]
  async fn concurrent_execution_runs_independent_branches_together() {
    use_writable_home();

    struct SleepNode {
      millis: u64,
    }

    #[async_trait]
    impl AsyncNode for SleepNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        tokio::time::sleep(std::time::Duration::from_millis(self.millis)).await;
        let mut outputs = HashMap::new();
        outputs.insert("done".to_string(), FlowValue::Json(json!(true)));
        Ok(outputs)
      }
    }

    let left = GraphNode {
      id: "left".to_string(),
      node_type: NodeType::Standard(Arc::new(SleepNode { millis: 120 })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let right = GraphNode {
      id: "right".to_string(),
      node_type: NodeType::Standard(Arc::new(SleepNode { millis: 120 })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let serial_started = Instant::now();
    let serial_state = Flow::new(vec![left.clone(), right.clone()])
      .run()
      .await
      .unwrap();
    let serial_elapsed = serial_started.elapsed();
    assert_eq!(serial_state.len(), 2);

    let concurrent_started = Instant::now();
    let concurrent_state = Flow::new(vec![left, right])
      .execute_from_inputs_with_config(HashMap::new(), FlowExecutionConfig::concurrent(2))
      .await
      .unwrap();
    let concurrent_elapsed = concurrent_started.elapsed();

    assert_eq!(concurrent_state.len(), 2);
    assert!(
      concurrent_elapsed < serial_elapsed,
      "concurrent {:?} should be faster than serial {:?}",
      concurrent_elapsed,
      serial_elapsed
    );
  }

  #[tokio::test]
  async fn concurrent_execution_waits_for_dependencies() {
    use_writable_home();

    struct EchoInputNode;

    #[async_trait]
    impl AsyncNode for EchoInputNode {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        let value = inputs
          .get("value")
          .cloned()
          .unwrap_or_else(|| FlowValue::Json(json!("root")));
        outputs.insert("value".to_string(), value);
        Ok(outputs)
      }
    }

    let root = GraphNode {
      id: "root".to_string(),
      node_type: NodeType::Standard(Arc::new(EchoInputNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("value".to_string(), FlowValue::Json(json!("from-root")));
        inputs
      },
    };
    let child = GraphNode {
      id: "child".to_string(),
      node_type: NodeType::Standard(Arc::new(EchoInputNode)),
      dependencies: vec!["root".to_string()],
      input_mapping: Some(HashMap::from([(
        "value".to_string(),
        ("root".to_string(), "value".to_string()),
      )])),
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let state = Flow::new(vec![root, child])
      .execute_from_inputs_with_config(HashMap::new(), FlowExecutionConfig::concurrent(2))
      .await
      .unwrap();

    let child_outputs = state.get("child").unwrap().as_ref().unwrap();
    assert_eq!(
      child_outputs.get("value"),
      Some(&FlowValue::Json(json!("from-root")))
    );
  }

  #[tokio::test]
  async fn execution_config_can_override_run_base_directory() {
    use_writable_home();

    struct TestNode;

    #[async_trait]
    impl AsyncNode for TestNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert("value".to_string(), FlowValue::Json(json!("ok")));
        Ok(outputs)
      }
    }

    let node = GraphNode {
      id: "node".to_string(),
      node_type: NodeType::Standard(Arc::new(TestNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let temp_dir = TempDir::new().unwrap();

    Flow::new(vec![node])
      .execute_from_inputs_with_config(
        HashMap::new(),
        FlowExecutionConfig::serial().with_run_base_dir(temp_dir.path()),
      )
      .await
      .unwrap();

    let run_dirs = std::fs::read_dir(temp_dir.path())
      .unwrap()
      .collect::<Result<Vec<_>, _>>()
      .unwrap();
    assert_eq!(run_dirs.len(), 1);
    assert!(run_dirs[0].path().join("node_outputs.json").exists());
  }

  /// Eval 2026-10-07 D5: a node that reads a sibling through
  /// `input_mapping` or `run_if` without declaring it a dependency started
  /// before that sibling in concurrent mode, so the input was silently
  /// dropped and the condition saw no output. It now waits, and the
  /// reference stays optional (a skipped source is still just omitted).
  #[tokio::test]
  async fn concurrent_execution_waits_for_undeclared_references() {
    use_writable_home();

    struct SlowSource;
    #[async_trait]
    impl AsyncNode for SlowSource {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        Ok(HashMap::from([
          ("value".to_string(), FlowValue::Json(json!("late"))),
          ("ready".to_string(), FlowValue::Json(json!(true))),
        ]))
      }
    }
    struct Echo;
    #[async_trait]
    impl AsyncNode for Echo {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let value = inputs
          .get("value")
          .cloned()
          .unwrap_or_else(|| FlowValue::Json(json!("missing")));
        Ok(HashMap::from([("value".to_string(), value)]))
      }
    }

    let node = |id: &str, kind: Arc<dyn AsyncNode>| GraphNode {
      id: id.to_string(),
      node_type: NodeType::Standard(kind),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let source = node("source", Arc::new(SlowSource));
    let mut reader = node("reader", Arc::new(Echo));
    reader.input_mapping = Some(HashMap::from([(
      "value".to_string(),
      ("source".to_string(), "value".to_string()),
    )]));
    let mut gate = node("gate", Arc::new(Echo));
    gate.run_if = Some("nodes.source.outputs.ready == true".to_string());

    let state = Flow::new(vec![source, reader, gate])
      .execute_from_inputs_with_config(HashMap::new(), FlowExecutionConfig::concurrent(3))
      .await
      .unwrap();

    let reader = state.get("reader").unwrap().as_ref().unwrap();
    assert_eq!(reader.get("value"), Some(&FlowValue::Json(json!("late"))));
    assert!(
      matches!(state.get("gate"), Some(Ok(_))),
      "gate's run_if must see source's output: {:?}",
      state.get("gate")
    );
  }

  #[tokio::test]
  async fn concurrent_fail_fast_stops_new_work_but_records_in_flight_results() {
    use_writable_home();

    struct DelayedOkNode {
      millis: u64,
      value: &'static str,
    }

    #[async_trait]
    impl AsyncNode for DelayedOkNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        tokio::time::sleep(std::time::Duration::from_millis(self.millis)).await;
        let mut outputs = HashMap::new();
        outputs.insert("value".to_string(), FlowValue::Json(json!(self.value)));
        Ok(outputs)
      }
    }

    struct FailingNode;

    #[async_trait]
    impl AsyncNode for FailingNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        Err(YanshiError::NodeExecutionFailed {
          message: "planned failure".to_string(),
        })
      }
    }

    let root = GraphNode {
      id: "root".to_string(),
      node_type: NodeType::Standard(Arc::new(DelayedOkNode {
        millis: 20,
        value: "root",
      })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let ok_branch = GraphNode {
      id: "ok_branch".to_string(),
      node_type: NodeType::Standard(Arc::new(DelayedOkNode {
        millis: 1,
        value: "ok",
      })),
      dependencies: vec!["root".to_string()],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let fail_branch = GraphNode {
      id: "fail_branch".to_string(),
      node_type: NodeType::Standard(Arc::new(FailingNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let after_failure = GraphNode {
      id: "after_failure".to_string(),
      node_type: NodeType::Standard(Arc::new(DelayedOkNode {
        millis: 1,
        value: "after",
      })),
      dependencies: vec!["fail_branch".to_string()],
      input_mapping: Some(HashMap::from([(
        "value".to_string(),
        ("fail_branch".to_string(), "value".to_string()),
      )])),
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });

    let state = Flow::new(vec![root, ok_branch, fail_branch, after_failure])
      .with_event_listener(listener)
      .execute_from_inputs_with_config(HashMap::new(), FlowExecutionConfig::concurrent(2))
      .await
      .unwrap();

    assert!(state.get("root").unwrap().is_ok());
    assert!(matches!(
      state.get("fail_branch"),
      Some(Err(YanshiError::NodeExecutionFailed { .. }))
    ));
    assert!(
      !state.contains_key("ok_branch"),
      "fail_fast should not schedule nodes that were not ready when failure occurred"
    );
    assert!(
      !state.contains_key("after_failure"),
      "downstream nodes of failed branches must not execute"
    );

    let events = events.lock().unwrap().clone();
    assert!(events.contains(&"node.failed"));
    assert!(events.contains(&"workflow.failed"));
  }

  #[tokio::test]
  async fn concurrent_non_fail_fast_continues_independent_ready_work() {
    use_writable_home();

    struct OkNode {
      value: &'static str,
    }

    #[async_trait]
    impl AsyncNode for OkNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert("value".to_string(), FlowValue::Json(json!(self.value)));
        Ok(outputs)
      }
    }

    struct FailingNode;

    #[async_trait]
    impl AsyncNode for FailingNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        Err(YanshiError::NodeExecutionFailed {
          message: "planned failure".to_string(),
        })
      }
    }

    let root = GraphNode {
      id: "root".to_string(),
      node_type: NodeType::Standard(Arc::new(OkNode { value: "root" })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let ok_branch = GraphNode {
      id: "ok_branch".to_string(),
      node_type: NodeType::Standard(Arc::new(OkNode { value: "ok" })),
      dependencies: vec!["root".to_string()],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let fail_branch = GraphNode {
      id: "fail_branch".to_string(),
      node_type: NodeType::Standard(Arc::new(FailingNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let after_failure = GraphNode {
      id: "after_failure".to_string(),
      node_type: NodeType::Standard(Arc::new(OkNode { value: "after" })),
      dependencies: vec!["fail_branch".to_string()],
      input_mapping: Some(HashMap::from([(
        "value".to_string(),
        ("fail_branch".to_string(), "value".to_string()),
      )])),
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });
    let mut config = FlowExecutionConfig::concurrent(2);
    config.fail_fast = false;

    let state = Flow::new(vec![root, ok_branch, fail_branch, after_failure])
      .with_event_listener(listener)
      .execute_from_inputs_with_config(HashMap::new(), config)
      .await
      .unwrap();

    assert!(state.get("root").unwrap().is_ok());
    assert!(state.get("ok_branch").unwrap().is_ok());
    assert!(matches!(
      state.get("fail_branch"),
      Some(Err(YanshiError::NodeExecutionFailed { .. }))
    ));
    assert!(
      !state.contains_key("after_failure"),
      "nodes depending on failed branches should remain unscheduled"
    );

    let events = events.lock().unwrap().clone();
    assert!(events.contains(&"node.failed"));
    assert!(events.contains(&"workflow.failed"));
  }

  #[tokio::test]
  async fn concurrent_skip_records_event_and_continues_independent_work() {
    use_writable_home();

    struct StaticNode {
      key: &'static str,
      value: serde_json::Value,
    }

    #[async_trait]
    impl AsyncNode for StaticNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert(self.key.to_string(), FlowValue::Json(self.value.clone()));
        Ok(outputs)
      }
    }

    let guard = GraphNode {
      id: "guard".to_string(),
      node_type: NodeType::Standard(Arc::new(StaticNode {
        key: "enabled",
        value: json!(false),
      })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let skipped_branch = GraphNode {
      id: "skipped_branch".to_string(),
      node_type: NodeType::Standard(Arc::new(StaticNode {
        key: "value",
        value: json!("should-not-run"),
      })),
      dependencies: vec!["guard".to_string()],
      input_mapping: None,
      run_if: Some("{{ nodes.guard.outputs.enabled }}".to_string()),
      initial_inputs: HashMap::new(),
    };
    let independent_branch = GraphNode {
      id: "independent_branch".to_string(),
      node_type: NodeType::Standard(Arc::new(StaticNode {
        key: "value",
        value: json!("independent"),
      })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let requires_skipped_output = GraphNode {
      id: "requires_skipped_output".to_string(),
      node_type: NodeType::Standard(Arc::new(StaticNode {
        key: "value",
        value: json!("blocked"),
      })),
      dependencies: vec!["skipped_branch".to_string()],
      input_mapping: Some(HashMap::from([(
        "value".to_string(),
        ("skipped_branch".to_string(), "value".to_string()),
      )])),
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });

    let state = Flow::new(vec![
      guard,
      skipped_branch,
      independent_branch,
      requires_skipped_output,
    ])
    .with_event_listener(listener)
    .execute_from_inputs_with_config(HashMap::new(), FlowExecutionConfig::concurrent(2))
    .await
    .unwrap();

    assert!(matches!(
      state.get("skipped_branch"),
      Some(Err(YanshiError::NodeSkipped))
    ));
    assert!(state.get("independent_branch").unwrap().is_ok());
    assert!(matches!(
      state.get("requires_skipped_output"),
      Some(Err(YanshiError::DependencyNotMet {
        node_id,
        dependency_id
      })) if node_id == "requires_skipped_output" && dependency_id == "skipped_branch"
    ));

    let events = events.lock().unwrap().clone();
    assert!(events.contains(&"node.skipped"));
    assert!(events.contains(&"workflow.failed"));
  }

  /// Builds `guard -> skipped_branch` where `skipped_branch`'s `run_if`
  /// evaluates to `false` and nothing depends on its output — a purely
  /// benign skip with no genuine failure anywhere in the DAG.
  fn skip_only_dag() -> Vec<GraphNode> {
    struct StaticNode {
      key: &'static str,
      value: serde_json::Value,
    }

    #[async_trait]
    impl AsyncNode for StaticNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert(self.key.to_string(), FlowValue::Json(self.value.clone()));
        Ok(outputs)
      }
    }

    vec![
      GraphNode {
        id: "guard".to_string(),
        node_type: NodeType::Standard(Arc::new(StaticNode {
          key: "enabled",
          value: json!(false),
        })),
        dependencies: vec![],
        input_mapping: None,
        run_if: None,
        initial_inputs: HashMap::new(),
      },
      GraphNode {
        id: "skipped_branch".to_string(),
        node_type: NodeType::Standard(Arc::new(StaticNode {
          key: "value",
          value: json!("should-not-run"),
        })),
        dependencies: vec!["guard".to_string()],
        input_mapping: None,
        run_if: Some("{{ nodes.guard.outputs.enabled }}".to_string()),
        initial_inputs: HashMap::new(),
      },
    ]
  }

  /// V0.3 regression: a workflow where the only "error" in the state pool
  /// is a benign `run_if` skip must report `WorkflowCompleted`, not
  /// `WorkflowFailed` — under the default (serial) execution config.
  #[tokio::test]
  async fn serial_skip_only_workflow_completes_not_failed() {
    use_writable_home();

    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });

    let state = Flow::new(skip_only_dag())
      .with_event_listener(listener)
      .execute_from_inputs(HashMap::new())
      .await
      .unwrap();

    assert!(matches!(
      state.get("skipped_branch"),
      Some(Err(YanshiError::NodeSkipped))
    ));

    let events = events.lock().unwrap().clone();
    assert!(
      events.contains(&"workflow.completed"),
      "expected workflow.completed, got: {events:?}"
    );
    assert!(
      !events.contains(&"workflow.failed"),
      "a benign run_if skip must not be reported as a workflow failure, got: {events:?}"
    );
  }

  /// V0.3 regression: the concurrent execution mode must agree with serial
  /// on the same skip-only DAG — both report `WorkflowCompleted`.
  #[tokio::test]
  async fn concurrent_skip_only_workflow_completes_not_failed() {
    use_writable_home();

    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });

    let state = Flow::new(skip_only_dag())
      .with_event_listener(listener)
      .execute_from_inputs_with_config(HashMap::new(), FlowExecutionConfig::concurrent(2))
      .await
      .unwrap();

    assert!(matches!(
      state.get("skipped_branch"),
      Some(Err(YanshiError::NodeSkipped))
    ));

    let events = events.lock().unwrap().clone();
    assert!(
      events.contains(&"workflow.completed"),
      "expected workflow.completed, got: {events:?}"
    );
    assert!(
      !events.contains(&"workflow.failed"),
      "a benign run_if skip must not be reported as a workflow failure, got: {events:?}"
    );
  }

  /// V0.3 regression: a skip-only run's final checkpoint status must be
  /// `Completed`, not `Failed` — covering both execution modes since they
  /// compute the checkpoint status independently.
  #[tokio::test]
  async fn skip_only_workflow_checkpoints_as_completed_in_both_modes() {
    use_writable_home();

    for config in [
      FlowExecutionConfig::serial(),
      FlowExecutionConfig::concurrent(2),
    ] {
      let temp_dir = TempDir::new().unwrap();
      let checkpoint_config = CheckpointConfig::default()
        .with_checkpoint_dir(temp_dir.path())
        .with_auto_cleanup(false);

      Flow::new(skip_only_dag())
        .with_checkpointing(checkpoint_config)
        .unwrap()
        .execute_from_inputs_with_config(HashMap::new(), config)
        .await
        .unwrap();

      let checkpoint = load_only_latest_checkpoint(&temp_dir).await;
      assert_eq!(
        checkpoint.status,
        WorkflowStatus::Completed,
        "a benign run_if skip must not flip the checkpoint status to Failed"
      );
    }
  }

  /// V0.4 regression: a caller-supplied workflow id that could escape
  /// `run_base_dir` (e.g. `..`) must be rejected before any filesystem
  /// work happens, not silently place run artifacts outside it.
  #[tokio::test]
  async fn execute_with_unsafe_workflow_id_is_rejected_before_touching_disk() {
    use_writable_home();

    let base_dir = TempDir::new().unwrap();
    let config = FlowExecutionConfig::serial().with_run_base_dir(base_dir.path());

    let err = Flow::new(skip_only_dag())
      .execute_from_inputs_with_id_and_config("../escape".to_string(), HashMap::new(), config)
      .await
      .unwrap_err();

    assert!(
      err.to_string().contains("invalid workflow id"),
      "got: {err}"
    );
    let escaped = base_dir.path().parent().unwrap().join("escape");
    assert!(
      !escaped.exists(),
      "run dir must not have escaped run_base_dir"
    );
  }

  /// V0.4 regression: a node `id` that could escape `run_dir` (e.g. `..`,
  /// from a workflow author's YAML `id:` field) must fail the run rather
  /// than writing its per-step output outside `run_dir`.
  #[tokio::test]
  async fn execute_with_unsafe_node_id_is_rejected_before_touching_disk() {
    use_writable_home();

    struct NoopNode;

    #[async_trait]
    impl AsyncNode for NoopNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        Ok(HashMap::new())
      }
    }

    let malicious_node = GraphNode {
      id: "../escape".to_string(),
      node_type: NodeType::Standard(Arc::new(NoopNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let base_dir = TempDir::new().unwrap();
    let config = FlowExecutionConfig::serial().with_run_base_dir(base_dir.path());

    let err = Flow::new(vec![malicious_node])
      .execute_from_inputs_with_config(HashMap::new(), config)
      .await
      .unwrap_err();

    assert!(err.to_string().contains("invalid node id"), "got: {err}");
    let escaped = base_dir.path().parent().unwrap().join("escape");
    assert!(
      !escaped.exists(),
      "node output must not have escaped run_dir"
    );
  }

  /// Builds `root -> consumer -> unrelated_after`. `consumer`'s
  /// `input_mapping` requires an output key `root` never produces, so
  /// `gather_inputs` genuinely fails for it (`NodeInputError`) — not a
  /// benign skip. `unrelated_after` has a purely structural dependency
  /// on `consumer` (no `input_mapping` referencing it) so the chain's
  /// topological order is deterministic regardless of id sort order,
  /// while still being a node whose own `gather_inputs` would trivially
  /// succeed if it were ever reached.
  fn gather_inputs_failure_dag() -> Vec<GraphNode> {
    struct StaticNode {
      key: &'static str,
      value: serde_json::Value,
    }

    #[async_trait]
    impl AsyncNode for StaticNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert(self.key.to_string(), FlowValue::Json(self.value.clone()));
        Ok(outputs)
      }
    }

    vec![
      GraphNode {
        id: "root".to_string(),
        node_type: NodeType::Standard(Arc::new(StaticNode {
          key: "other_key",
          value: json!("value"),
        })),
        dependencies: vec![],
        input_mapping: None,
        run_if: None,
        initial_inputs: HashMap::new(),
      },
      GraphNode {
        id: "consumer".to_string(),
        node_type: NodeType::Standard(Arc::new(StaticNode {
          key: "unused",
          value: json!(null),
        })),
        dependencies: vec!["root".to_string()],
        input_mapping: Some(HashMap::from([(
          "expected".to_string(),
          ("root".to_string(), "expected_key".to_string()),
        )])),
        run_if: None,
        initial_inputs: HashMap::new(),
      },
      GraphNode {
        id: "unrelated_after".to_string(),
        node_type: NodeType::Standard(Arc::new(StaticNode {
          key: "value",
          value: json!("should-not-run-under-fail-fast"),
        })),
        dependencies: vec!["consumer".to_string()],
        input_mapping: None,
        run_if: None,
        initial_inputs: HashMap::new(),
      },
    ]
  }

  /// V1.1 regression: a `gather_inputs` failure used to propagate via
  /// `?` and abort `execute_from_inputs` itself — no `WorkflowFailed`
  /// event, no state_pool returned to the caller at all. It must now be
  /// a per-node result like the concurrent path already produces, and
  /// (with the default `fail_fast: true`) stop scheduling further
  /// nodes.
  #[tokio::test]
  async fn serial_gather_inputs_failure_does_not_abort_the_whole_run() {
    use_writable_home();

    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });

    let state = Flow::new(gather_inputs_failure_dag())
      .with_event_listener(listener)
      .execute_from_inputs(HashMap::new())
      .await
      .expect("a gather_inputs failure must not abort the whole function");

    assert!(state.get("root").unwrap().is_ok());
    assert!(matches!(
      state.get("consumer"),
      Some(Err(YanshiError::NodeInputError { .. }))
    ));
    assert!(
      !state.contains_key("unrelated_after"),
      "fail_fast (default) must stop scheduling further nodes after a genuine failure"
    );

    let events = events.lock().unwrap().clone();
    assert!(events.contains(&"workflow.failed"));
    assert!(!events.contains(&"workflow.completed"));
  }

  /// V1.1: serial and concurrent must agree on terminal status for the
  /// identical DAG — both `WorkflowFailed`, never one `Failed` and the
  /// other silently `Ok`/`Completed`.
  #[tokio::test]
  async fn serial_and_concurrent_agree_on_terminal_status_for_a_genuine_failure() {
    use_writable_home();

    for config in [
      FlowExecutionConfig::serial(),
      FlowExecutionConfig::concurrent(2),
    ] {
      let events = Arc::new(Mutex::new(Vec::new()));
      let listener = Arc::new(RecordingListener {
        events: events.clone(),
      });

      let state = Flow::new(gather_inputs_failure_dag())
        .with_event_listener(listener)
        .execute_from_inputs_with_config(HashMap::new(), config)
        .await
        .expect("must not hard-abort in either mode");

      assert!(matches!(
        state.get("consumer"),
        Some(Err(YanshiError::NodeInputError { .. }))
      ));
      let events = events.lock().unwrap().clone();
      assert!(
        events.contains(&"workflow.failed"),
        "expected workflow.failed in both modes, got: {events:?}"
      );
      assert!(!events.contains(&"workflow.completed"));
    }
  }

  /// V1.1 regression: before the fix, a `gather_inputs` failure aborted
  /// the function before the final checkpoint write ever ran, leaving
  /// the checkpoint stuck at `Running` forever. It must now reach
  /// `Failed`.
  #[tokio::test]
  async fn serial_gather_inputs_failure_checkpoints_as_failed_not_stuck_running() {
    use_writable_home();

    let temp_dir = TempDir::new().unwrap();
    let checkpoint_config = CheckpointConfig::default()
      .with_checkpoint_dir(temp_dir.path())
      .with_auto_cleanup(false);

    Flow::new(gather_inputs_failure_dag())
      .with_checkpointing(checkpoint_config)
      .unwrap()
      .execute_from_inputs(HashMap::new())
      .await
      .expect("must not hard-abort");

    let checkpoint = load_only_latest_checkpoint(&temp_dir).await;
    assert_eq!(checkpoint.status, WorkflowStatus::Failed);
  }

  /// V1.1: `fail_fast: false` must still let serial mode continue past
  /// a genuine failure — the pre-fix "unconditionally run every
  /// remaining node" behavior is preserved when explicitly configured,
  /// it's just no longer the *only* behavior serial mode has.
  #[tokio::test]
  async fn serial_fail_fast_false_continues_past_a_genuine_failure() {
    use_writable_home();

    let config = FlowExecutionConfig {
      fail_fast: false,
      ..FlowExecutionConfig::serial()
    };

    let state = Flow::new(gather_inputs_failure_dag())
      .execute_from_inputs_with_config(HashMap::new(), config)
      .await
      .unwrap();

    assert!(matches!(
      state.get("consumer"),
      Some(Err(YanshiError::NodeInputError { .. }))
    ));
    assert!(
      state.get("unrelated_after").unwrap().is_ok(),
      "fail_fast: false must let serial mode keep going past the failure"
    );
  }

  /// V1.4 regression: `FlowExecutionConfig::node_timeout_ms` (opt-in,
  /// `None` by default) times out a node that never returns, from
  /// *inside* the executor itself — before this fix, `yanshi-core`'s
  /// own `timeout` module had no caller inside `Flow`'s node dispatch at
  /// all.
  #[tokio::test]
  async fn serial_node_timeout_ms_times_out_a_slow_node() {
    use_writable_home();

    struct ForeverPendingNode;

    #[async_trait]
    impl AsyncNode for ForeverPendingNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        Ok(HashMap::new())
      }
    }

    let node = GraphNode {
      id: "slow".to_string(),
      node_type: NodeType::Standard(Arc::new(ForeverPendingNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let config = FlowExecutionConfig::serial().with_node_timeout_ms(50);

    let state = Flow::new(vec![node])
      .execute_from_inputs_with_config(HashMap::new(), config)
      .await
      .unwrap();

    match state.get("slow") {
      Some(Err(YanshiError::TimeoutExceeded { duration_ms })) => assert_eq!(*duration_ms, 50),
      other => panic!("expected TimeoutExceeded, got {other:?}"),
    }
  }

  /// Builds a single-node Flow around a node that fails with a
  /// retryable (network-class) error the first `fail_times` calls, then
  /// succeeds, tracking every attempt in `calls`.
  fn flaky_node_flow(fail_times: u32, calls: Arc<std::sync::atomic::AtomicU32>) -> Vec<GraphNode> {
    struct FlakyNode {
      fail_times: u32,
      calls: Arc<std::sync::atomic::AtomicU32>,
    }

    #[async_trait]
    impl AsyncNode for FlakyNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call < self.fail_times {
          return Err(YanshiError::NetworkError {
            message: "connection reset".to_string(),
          });
        }
        let mut outputs = HashMap::new();
        outputs.insert("value".to_string(), FlowValue::Json(json!("ok")));
        Ok(outputs)
      }
    }

    vec![GraphNode {
      id: "flaky".to_string(),
      node_type: NodeType::Standard(Arc::new(FlakyNode { fail_times, calls })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    }]
  }

  /// V1.4 regression: `FlowExecutionConfig::node_retry_policy` (opt-in,
  /// `None` by default) retries a node that fails with a transient
  /// error, and — the actual point of this fix, since T3.1's
  /// config-first `TimeoutRetryNode` already retried nodes without ever
  /// telling `Flow`'s event system about it — every attempt emits a
  /// real `WorkflowEvent::RetryAttempt` observable from outside the
  /// executor (trace replay, the event stream), not just an eventual
  /// `Ok`/`Err`.
  #[tokio::test]
  async fn serial_node_retry_policy_retries_and_emits_retry_attempt_events() {
    use_writable_home();

    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });

    let policy = crate::retry::RetryPolicy {
      max_attempts: 5,
      ..crate::retry::RetryPolicy::default()
    };
    let config = FlowExecutionConfig::serial().with_node_retry_policy(policy);

    let state = Flow::new(flaky_node_flow(2, calls.clone()))
      .with_event_listener(listener)
      .execute_from_inputs_with_config(HashMap::new(), config)
      .await
      .unwrap();

    assert!(state.get("flaky").unwrap().is_ok());
    assert_eq!(
      calls.load(std::sync::atomic::Ordering::SeqCst),
      3,
      "2 failed attempts + 1 successful attempt"
    );

    let events = events.lock().unwrap().clone();
    let retry_attempt_count = events.iter().filter(|e| **e == "retry.attempt").count();
    assert_eq!(
      retry_attempt_count, 2,
      "expected exactly 2 RetryAttempt events (one per failed attempt), got: {events:?}"
    );
  }

  /// V1.4: the concurrent execution path must wire the same reliability
  /// primitives — this is the same scenario as
  /// `serial_node_retry_policy_retries_and_emits_retry_attempt_events`
  /// but under `FlowExecutionConfig::concurrent`, since retry/timeout
  /// wrapping was added independently to both dispatch sites.
  #[tokio::test]
  async fn concurrent_node_retry_policy_retries_and_emits_retry_attempt_events() {
    use_writable_home();

    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });

    let policy = crate::retry::RetryPolicy {
      max_attempts: 5,
      ..crate::retry::RetryPolicy::default()
    };
    let config = FlowExecutionConfig::concurrent(2).with_node_retry_policy(policy);

    let state = Flow::new(flaky_node_flow(2, calls.clone()))
      .with_event_listener(listener)
      .execute_from_inputs_with_config(HashMap::new(), config)
      .await
      .unwrap();

    assert!(state.get("flaky").unwrap().is_ok());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);

    let events = events.lock().unwrap().clone();
    let retry_attempt_count = events.iter().filter(|e| **e == "retry.attempt").count();
    assert_eq!(
      retry_attempt_count, 2,
      "expected exactly 2 RetryAttempt events under concurrent execution too, got: {events:?}"
    );
  }

  /// W0.4 regression: a node configured with `node_retry_policy` that
  /// fails with `NodePartialExecutionFailed` on a non-retryable error
  /// (the default policy only retries Network/Timeout/RateLimit) used to
  /// have `execute_with_retry_and_hook` unconditionally coerce that into
  /// `RetryExhausted` even though zero retries ever happened — discarding
  /// `partial_outputs` before it ever reached `checkpointable_value`,
  /// which only recognizes `NodeSkipped` / `NodePartialExecutionFailed`
  /// (anything else, including `RetryExhausted`, is `Err(_) => None` and
  /// never checkpointed at all). The state pool entry must still be the
  /// original `NodePartialExecutionFailed`, and it must round-trip
  /// through the checkpoint encode/decode with `partial_outputs` intact.
  #[tokio::test]
  async fn node_retry_policy_does_not_swallow_partial_failure_into_retry_exhausted() {
    use_writable_home();

    struct PartialFailureNode;

    #[async_trait]
    impl AsyncNode for PartialFailureNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut partial_outputs = HashMap::new();
        partial_outputs.insert(
          "agent_result".to_string(),
          FlowValue::Json(json!({"steps": ["observe"]})),
        );
        Err(YanshiError::NodePartialExecutionFailed {
          message: "interrupted mid-tool-call".to_string(),
          partial_outputs,
        })
      }
    }

    let flow_nodes = vec![GraphNode {
      id: "agent".to_string(),
      node_type: NodeType::Standard(Arc::new(PartialFailureNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    }];

    // Default policy: only Network/Timeout/RateLimit errors are
    // retryable, so `NodePartialExecutionFailed` is never retried —
    // exactly the "zero retries actually happened" case this fix covers.
    let config =
      FlowExecutionConfig::serial().with_node_retry_policy(crate::retry::RetryPolicy::default());

    let state = Flow::new(flow_nodes)
      .execute_from_inputs_with_config(HashMap::new(), config)
      .await
      .unwrap();

    let partial_outputs = match state.get("agent") {
      Some(Err(YanshiError::NodePartialExecutionFailed {
        message,
        partial_outputs,
      })) => {
        assert_eq!(message, "interrupted mid-tool-call");
        partial_outputs.clone()
      }
      other => panic!(
        "expected NodePartialExecutionFailed to survive node_retry_policy wrapping, got {other:?}"
      ),
    };
    assert!(partial_outputs.contains_key("agent_result"));

    // And it must actually make it into the checkpoint, not fall into
    // `checkpointable_value`'s `Err(_) => None` catch-all.
    let checkpoint_state =
      FlowExecutor::new(&Flow::default()).state_pool_to_checkpoint_state(&state);
    assert!(
      checkpoint_state.contains_key("agent"),
      "partial failure must still be checkpointed, not dropped"
    );
    let restored = FlowExecutor::checkpoint_state_to_state_pool(&checkpoint_state);
    match restored.get("agent") {
      Some(Err(YanshiError::NodePartialExecutionFailed {
        partial_outputs, ..
      })) => {
        assert!(partial_outputs.contains_key("agent_result"));
      }
      other => {
        panic!("expected NodePartialExecutionFailed after checkpoint round-trip, got {other:?}")
      }
    }
  }

  #[tokio::test]
  async fn concurrent_checkpoint_captures_successful_branch_outputs() {
    use_writable_home();

    struct OutputNode {
      key: &'static str,
      value: &'static str,
    }

    #[async_trait]
    impl AsyncNode for OutputNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert(self.key.to_string(), FlowValue::Json(json!(self.value)));
        Ok(outputs)
      }
    }

    let left = GraphNode {
      id: "left".to_string(),
      node_type: NodeType::Standard(Arc::new(OutputNode {
        key: "value",
        value: "left",
      })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let right = GraphNode {
      id: "right".to_string(),
      node_type: NodeType::Standard(Arc::new(OutputNode {
        key: "value",
        value: "right",
      })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let temp_dir = TempDir::new().unwrap();
    let config = CheckpointConfig::default()
      .with_checkpoint_dir(temp_dir.path())
      .with_auto_cleanup(false);

    Flow::new(vec![left, right])
      .with_checkpointing(config)
      .unwrap()
      .execute_from_inputs_with_config(HashMap::new(), FlowExecutionConfig::concurrent(2))
      .await
      .unwrap();

    let checkpoint = load_only_latest_checkpoint(&temp_dir).await;
    assert_eq!(checkpoint.status, WorkflowStatus::Completed);
    assert_eq!(
      checkpoint.state["left"]["value"],
      json!({"type": "json", "value": "left"})
    );
    assert_eq!(
      checkpoint.state["right"]["value"],
      json!({"type": "json", "value": "right"})
    );
  }

  #[tokio::test]
  async fn concurrent_checkpoint_marks_failed_run_and_keeps_completed_node() {
    use_writable_home();

    struct SlowOkNode;

    #[async_trait]
    impl AsyncNode for SlowOkNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut outputs = HashMap::new();
        outputs.insert("value".to_string(), FlowValue::Json(json!("ok")));
        Ok(outputs)
      }
    }

    struct FailingNode;

    #[async_trait]
    impl AsyncNode for FailingNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        Err(YanshiError::NodeExecutionFailed {
          message: "planned failure".to_string(),
        })
      }
    }

    let ok_branch = GraphNode {
      id: "ok_branch".to_string(),
      node_type: NodeType::Standard(Arc::new(SlowOkNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let fail_branch = GraphNode {
      id: "fail_branch".to_string(),
      node_type: NodeType::Standard(Arc::new(FailingNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let temp_dir = TempDir::new().unwrap();
    let config = CheckpointConfig::default()
      .with_checkpoint_dir(temp_dir.path())
      .with_auto_cleanup(false);

    Flow::new(vec![ok_branch, fail_branch])
      .with_checkpointing(config)
      .unwrap()
      .execute_from_inputs_with_config(HashMap::new(), FlowExecutionConfig::concurrent(2))
      .await
      .unwrap();

    let checkpoint = load_only_latest_checkpoint(&temp_dir).await;
    assert_eq!(checkpoint.status, WorkflowStatus::Failed);
    assert_eq!(checkpoint.last_completed_node, "ok_branch");
    assert_eq!(
      checkpoint.state["ok_branch"]["value"],
      json!({"type": "json", "value": "ok"})
    );
    assert!(!checkpoint.state.contains_key("fail_branch"));
  }

  #[tokio::test]
  async fn checkpoint_resume_uses_serial_compatibility_path() {
    use_writable_home();

    struct OutputNode;

    #[async_trait]
    impl AsyncNode for OutputNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert("value".to_string(), FlowValue::Json(json!("root")));
        Ok(outputs)
      }
    }

    struct SleepNode;

    #[async_trait]
    impl AsyncNode for SleepNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        tokio::time::sleep(std::time::Duration::from_millis(90)).await;
        let mut outputs = HashMap::new();
        outputs.insert("done".to_string(), FlowValue::Json(json!(true)));
        Ok(outputs)
      }
    }

    let root = GraphNode {
      id: "root".to_string(),
      node_type: NodeType::Standard(Arc::new(OutputNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let left = GraphNode {
      id: "left".to_string(),
      node_type: NodeType::Standard(Arc::new(SleepNode)),
      dependencies: vec!["root".to_string()],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let right = GraphNode {
      id: "right".to_string(),
      node_type: NodeType::Standard(Arc::new(SleepNode)),
      dependencies: vec!["root".to_string()],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let temp_dir = TempDir::new().unwrap();
    let config = CheckpointConfig::default()
      .with_checkpoint_dir(temp_dir.path())
      .with_auto_cleanup(false);
    let manager = CheckpointManager::new(config.clone()).unwrap();
    manager
      .save_checkpoint(
        "resume-workflow",
        "root",
        &HashMap::from([(
          "root".to_string(),
          json!({
            "value": "root"
          }),
        )]),
      )
      .await
      .unwrap();

    let started = Instant::now();
    let state = Flow::new(vec![root, left, right])
      .with_checkpointing(config)
      .unwrap()
      .resume("resume-workflow")
      .await
      .unwrap();
    let elapsed = started.elapsed();

    assert!(state.get("left").unwrap().is_ok());
    assert!(state.get("right").unwrap().is_ok());
    assert!(
      elapsed >= std::time::Duration::from_millis(170),
      "resume should execute sibling nodes serially, elapsed: {:?}",
      elapsed
    );
  }

  #[tokio::test]
  async fn test_map_node_parallel_execution() {
    use_writable_home();
    struct MultiplyNode;
    #[async_trait]
    impl AsyncNode for MultiplyNode {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let val = match inputs.get("item").unwrap() {
          FlowValue::Json(Value::Number(n)) => n.as_i64().unwrap(),
          _ => 0,
        };
        let mut outputs = HashMap::new();
        outputs.insert("result".to_string(), FlowValue::Json(json!(val * 2)));
        Ok(outputs)
      }
    }

    let sub_flow_node = GraphNode {
      id: "multiply".to_string(),
      node_type: NodeType::Standard(Arc::new(MultiplyNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "map_node".to_string(),
      node_type: NodeType::Map {
        template: vec![sub_flow_node],
        parallel: true,
        max_concurrent: None,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert(
          "input_list".to_string(),
          FlowValue::Json(json!([1, 2, 3, 4, 5])),
        );
        inputs
      },
    };

    let flow = Flow::new(vec![map_node]);

    let final_state = flow.run().await.unwrap();
    let map_result = final_state.get("map_node").unwrap().as_ref().unwrap();
    let results_array = match map_result.get("results").unwrap() {
      FlowValue::Json(Value::Array(arr)) => arr,
      _ => panic!("Not an array"),
    };

    assert_eq!(results_array.len(), 5);
  }

  /// W1.1 regression: pre-fix, `execute_map_node_parallel` built each
  /// sub-flow via a bare `Flow::new` with no `event_listener`, so the
  /// inner template node's `node.started`/`node.completed` events were
  /// silently dropped instead of reaching the parent's listener — the
  /// operator's trace/UI/SSE saw the Map node as a single opaque step
  /// with no visibility into its 5 sub-flow items. Asserts the listener
  /// observes one `node.started`/`node.completed` pair for the outer
  /// `map_node` PLUS one pair per sub-flow item.
  #[tokio::test]
  async fn map_node_parallel_sub_flow_events_reach_parent_listener() {
    use_writable_home();
    struct MultiplyNode;
    #[async_trait]
    impl AsyncNode for MultiplyNode {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let val = match inputs.get("item").unwrap() {
          FlowValue::Json(Value::Number(n)) => n.as_i64().unwrap(),
          _ => 0,
        };
        let mut outputs = HashMap::new();
        outputs.insert("result".to_string(), FlowValue::Json(json!(val * 2)));
        Ok(outputs)
      }
    }

    let sub_flow_node = GraphNode {
      id: "multiply".to_string(),
      node_type: NodeType::Standard(Arc::new(MultiplyNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "map_node".to_string(),
      node_type: NodeType::Map {
        template: vec![sub_flow_node],
        parallel: true,
        max_concurrent: None,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert(
          "input_list".to_string(),
          FlowValue::Json(json!([1, 2, 3, 4, 5])),
        );
        inputs
      },
    };

    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });
    let flow = Flow::new(vec![map_node]).with_event_listener(listener);

    flow.run().await.unwrap();

    let events = events.lock().unwrap();
    let started = events.iter().filter(|e| **e == "node.started").count();
    let completed = events.iter().filter(|e| **e == "node.completed").count();
    assert_eq!(
      started, 6,
      "expected 1 (map_node) + 5 (one per sub-flow item), got {started}: {events:?}"
    );
    assert_eq!(
      completed, 6,
      "expected 1 (map_node) + 5 (one per sub-flow item), got {completed}: {events:?}"
    );
    // Eval 2026-10-07 D3: each item's sub-run used to emit its own
    // workflow.started/completed too, inflating completion counts 6x.
    for lifecycle in ["workflow.started", "workflow.completed"] {
      let count = events.iter().filter(|e| **e == lifecycle).count();
      assert_eq!(count, 1, "{lifecycle}: {events:?}");
    }
  }

  /// W1.1 regression: pre-fix, `execute_map_node_sequential` built each
  /// sub-flow via a bare `Flow::new` + `execute_from_inputs` (no
  /// config), so the parent's `cancellation_token` was never threaded
  /// down — a cancelled parent run would still march every Map item to
  /// completion instead of stopping. The template node cancels the
  /// shared token as a side effect of its first call; if the fix is
  /// working, the second sub-flow's own top-of-run cancellation check
  /// (the same one a top-level `Flow` run already honors) fails it
  /// before it ever reaches the node, so `calls` never exceeds 1 and
  /// the Map node's own `?` propagates `TaskCancelled` up.
  #[tokio::test]
  async fn map_node_sequential_sub_flow_respects_parent_cancellation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use_writable_home();

    struct CancelOnFirstCallNode {
      calls: Arc<AtomicUsize>,
      token: crate::scheduler::FlowCancellationToken,
    }
    #[async_trait]
    impl AsyncNode for CancelOnFirstCallNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == 1 {
          self.token.cancel();
        }
        let mut outputs = HashMap::new();
        outputs.insert("ok".to_string(), FlowValue::Json(json!(true)));
        Ok(outputs)
      }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let token = crate::scheduler::FlowCancellationToken::new();

    let sub_flow_node = GraphNode {
      id: "cancel_on_first".to_string(),
      node_type: NodeType::Standard(Arc::new(CancelOnFirstCallNode {
        calls: calls.clone(),
        token: token.clone(),
      })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "map_node".to_string(),
      node_type: NodeType::Map {
        template: vec![sub_flow_node],
        parallel: false,
        max_concurrent: None,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("input_list".to_string(), FlowValue::Json(json!([1, 2, 3])));
        inputs
      },
    };

    let final_state = Flow::new(vec![map_node])
      .execute_from_inputs_with_config(
        HashMap::new(),
        FlowExecutionConfig::serial().with_cancellation_token(token),
      )
      .await
      .unwrap();

    assert!(
      matches!(
        final_state.get("map_node"),
        Some(Err(YanshiError::TaskCancelled))
      ),
      "expected the map_node entry to be TaskCancelled once the shared \
       token was tripped, got {:?}",
      final_state.get("map_node")
    );
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "only the first sub-flow item should have run before cancellation was observed"
    );
  }

  /// F-A6-1: `max_concurrent: Some(N)` on a parallel map node MUST
  /// hold the number of simultaneously-running sub-flows at or
  /// below N. A probe sub-flow increments a shared counter on
  /// entry, sleeps 30ms to keep the bound observable, then
  /// decrements. We measure the high-water mark across the whole
  /// map and assert it never exceeded the configured cap.
  #[tokio::test]
  async fn test_map_node_parallel_respects_max_concurrent_cap() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use_writable_home();

    let concurrent = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));

    struct ProbeNode {
      concurrent: Arc<AtomicUsize>,
      max_observed: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl AsyncNode for ProbeNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_observed.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
        let mut outputs = HashMap::new();
        outputs.insert("ok".to_string(), FlowValue::Json(json!(true)));
        Ok(outputs)
      }
    }

    let sub_flow_node = GraphNode {
      id: "probe".to_string(),
      node_type: NodeType::Standard(Arc::new(ProbeNode {
        concurrent: concurrent.clone(),
        max_observed: max_observed.clone(),
      })),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "bounded_map".to_string(),
      node_type: NodeType::Map {
        template: vec![sub_flow_node],
        parallel: true,
        max_concurrent: Some(3),
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        // 8 items, cap 3 — high-water mark MUST be ≤ 3 at any point.
        inputs.insert(
          "input_list".to_string(),
          FlowValue::Json(json!([1, 2, 3, 4, 5, 6, 7, 8])),
        );
        inputs
      },
    };

    let flow = Flow::new(vec![map_node]);
    let final_state = flow.run().await.unwrap();
    let map_result = final_state.get("bounded_map").unwrap().as_ref().unwrap();
    let results_array = match map_result.get("results").unwrap() {
      FlowValue::Json(Value::Array(arr)) => arr,
      _ => panic!("Not an array"),
    };

    assert_eq!(results_array.len(), 8, "all 8 sub-flows must complete");
    let high = max_observed.load(Ordering::SeqCst);
    assert!(
      high <= 3,
      "max_concurrent=3 violated: observed {high} concurrent sub-flows"
    );
    // Sanity: parallelism should actually engage — if the cap is
    // 3 and we sleep 30ms per item, 8 items serial would take
    // 240ms. We don't time-assert that here (CI noise) but the
    // high-water mark of >= 2 confirms at least *some* parallelism.
    assert!(
      high >= 2,
      "expected at least 2 concurrent sub-flows (got {high}); cap may be too tight or executor isn't actually parallel"
    );
  }

  /// Eval 2026-10-07 D6: parallel map items ran on detached tasks, so
  /// they kept executing after the run's future was dropped (a timeout or
  /// cancelled caller) — and a retried node then ran them twice.
  #[tokio::test]
  async fn parallel_map_items_stop_when_the_run_is_dropped() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use_writable_home();

    struct SlowNode(Arc<AtomicUsize>);
    #[async_trait]
    impl AsyncNode for SlowNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(HashMap::new())
      }
    }

    let finished = Arc::new(AtomicUsize::new(0));
    let map_node = GraphNode {
      id: "map".to_string(),
      node_type: NodeType::Map {
        template: vec![GraphNode {
          id: "slow".to_string(),
          node_type: NodeType::Standard(Arc::new(SlowNode(finished.clone()))),
          dependencies: vec![],
          input_mapping: None,
          run_if: None,
          initial_inputs: HashMap::new(),
        }],
        parallel: true,
        max_concurrent: None,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::from([(
        "input_list".to_string(),
        FlowValue::Json(json!([1, 2, 3])),
      )]),
    };

    let flow = Flow::new(vec![map_node]);
    let dropped = tokio::time::timeout(std::time::Duration::from_millis(50), flow.run()).await;
    assert!(dropped.is_err(), "the run must still be in flight at 50ms");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
      finished.load(Ordering::SeqCst),
      0,
      "map items kept running after the run was dropped"
    );
  }

  /// F-A6-1: `max_concurrent: Some(0)` is rejected as a config
  /// error rather than silently deadlocking (`Semaphore::new(0)`
  /// would block every acquire forever).
  #[tokio::test]
  async fn test_map_node_parallel_rejects_zero_max_concurrent() {
    use_writable_home();

    struct NoopNode;
    #[async_trait]
    impl AsyncNode for NoopNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        Ok(HashMap::new())
      }
    }

    let sub_flow_node = GraphNode {
      id: "noop".to_string(),
      node_type: NodeType::Standard(Arc::new(NoopNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "zero_cap_map".to_string(),
      node_type: NodeType::Map {
        template: vec![sub_flow_node],
        parallel: true,
        max_concurrent: Some(0),
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("input_list".to_string(), FlowValue::Json(json!([1, 2])));
        inputs
      },
    };

    let flow = Flow::new(vec![map_node]);
    // F-A6-3: per-node errors are buried inside the Ok(state) pool
    // rather than bubbled as a top-level Flow Err. Walk the state to
    // find the node's actual error.
    let state = flow.run().await.expect("flow run itself must not Err");
    let node_result = state
      .get("zero_cap_map")
      .expect("zero_cap_map node must appear in the state pool");
    match node_result {
      Err(YanshiError::NodeInputError { message }) if message.contains("max_concurrent") => {}
      other => {
        panic!("expected NodeInputError mentioning max_concurrent on the map node, got {other:?}")
      }
    }
  }

  /// F-A6-5: `input_mapping` MUST resolve the `!item` sentinel
  /// against the map sub-flow's seeded `item`. A node downstream of
  /// nothing (no dependencies) can pull `item.foo` directly into its
  /// own inputs without an intermediate render-template node.
  ///
  /// Asserts (a) flat field access works, (b) nested dotted access
  /// works, (c) the resolved value reaches the node's `execute`.
  #[tokio::test]
  async fn test_input_mapping_resolves_item_field_lookups() {
    use_writable_home();
    use std::sync::Mutex;

    let captured = Arc::new(Mutex::new(None::<String>));

    struct PathSink {
      captured: Arc<Mutex<Option<String>>>,
    }
    #[async_trait]
    impl AsyncNode for PathSink {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        // The downstream node receives a `path` input that was
        // wired from `{{ item.read_path }}` via input_mapping.
        let path = match inputs.get("path") {
          Some(FlowValue::Json(Value::String(s))) => s.clone(),
          other => panic!("expected String input on `path`, got {other:?}"),
        };
        let nested = match inputs.get("nested_field") {
          Some(FlowValue::Json(Value::String(s))) => s.clone(),
          other => panic!("expected String input on `nested_field`, got {other:?}"),
        };
        *self.captured.lock().unwrap() = Some(format!("{path}|{nested}"));
        let mut outputs = HashMap::new();
        outputs.insert("ok".to_string(), FlowValue::Json(json!(true)));
        Ok(outputs)
      }
    }

    let sink_node = GraphNode {
      id: "sink".to_string(),
      node_type: NodeType::Standard(Arc::new(PathSink {
        captured: captured.clone(),
      })),
      dependencies: vec![],
      // F-A6-5 wire: both flat (`item.read_path`) and nested
      // (`item.meta.tag`) lookups.
      input_mapping: Some(HashMap::from([
        (
          "path".to_string(),
          ("!item".to_string(), "read_path".to_string()),
        ),
        (
          "nested_field".to_string(),
          ("!item".to_string(), "meta.tag".to_string()),
        ),
      ])),
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "item_lookup_map".to_string(),
      node_type: NodeType::Map {
        template: vec![sink_node],
        parallel: false,
        max_concurrent: None,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert(
          "input_list".to_string(),
          FlowValue::Json(json!([{
            "read_path": "input/intro.md",
            "meta": { "tag": "nested-ok" }
          }])),
        );
        inputs
      },
    };

    let state = Flow::new(vec![map_node]).run().await.unwrap();
    let map_result = state
      .get("item_lookup_map")
      .unwrap()
      .as_ref()
      .expect("map must Ok overall");
    let summary = match map_result.get("results_summary").unwrap() {
      FlowValue::Json(s) => s,
      _ => panic!(),
    };
    assert_eq!(summary["err"], 0, "sub-flow must not have errored");

    let captured = captured.lock().unwrap();
    assert_eq!(
      captured.as_deref(),
      Some("input/intro.md|nested-ok"),
      "item.read_path and item.meta.tag both resolved into the sink node's inputs"
    );
  }

  /// F-A6-5: clear error when `item.X` references a path that
  /// doesn't exist in the iteration value, instead of silently
  /// passing through nothing.
  #[tokio::test]
  async fn test_input_mapping_item_missing_path_errors() {
    use_writable_home();

    struct NoopNode;
    #[async_trait]
    impl AsyncNode for NoopNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        Ok(HashMap::new())
      }
    }

    let sink_node = GraphNode {
      id: "sink".to_string(),
      node_type: NodeType::Standard(Arc::new(NoopNode)),
      dependencies: vec![],
      input_mapping: Some(HashMap::from([(
        "missing".to_string(),
        ("!item".to_string(), "nope.not_here".to_string()),
      )])),
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "bad_lookup_map".to_string(),
      node_type: NodeType::Map {
        template: vec![sink_node],
        parallel: false,
        max_concurrent: None,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert(
          "input_list".to_string(),
          FlowValue::Json(json!([{"read_path": "x"}])),
        );
        inputs
      },
    };

    // V1.1: a missing item path fails `gather_inputs` for the `sink`
    // node inside the sub-flow. Before V1.1 that propagated via `?`
    // and aborted the whole sub-flow's `execute_from_inputs`, which
    // the map node's `.await?` then rethrew as an `Err` on the map
    // node itself. Now the sub-flow's `execute_from_inputs` returns
    // `Ok` (per node V1.1 fixed serial execution to match the
    // concurrent path), so the failure is buried inside `results[0]`
    // and tallied in `results_summary` exactly like a regular
    // per-node execution error (F-A6-3) — `gather_inputs` failures no
    // longer get special-cased.
    let state = Flow::new(vec![map_node]).run().await.unwrap();
    let map_result = state
      .get("bad_lookup_map")
      .unwrap()
      .as_ref()
      .expect("map must Ok overall (per-sub-flow Err doesn't bubble)");

    let results_array = match map_result.get("results").expect("results must exist") {
      FlowValue::Json(Value::Array(arr)) => arr,
      other => panic!("results must be a JSON array, got {other:?}"),
    };
    assert_eq!(results_array.len(), 1);
    let err_msg = results_array[0]["sink"]["Err"]["NodeInputError"]["message"]
      .as_str()
      .expect("sink's NodeInputError message must be present");
    assert!(
      err_msg.contains("item.nope.not_here"),
      "error must name the missing item path: {err_msg}"
    );

    let summary = match map_result
      .get("results_summary")
      .expect("results_summary must exist (F-A6-3)")
    {
      FlowValue::Json(s) => s,
      other => panic!("results_summary must be JSON, got {other:?}"),
    };
    assert_eq!(summary["total"], 1);
    assert_eq!(summary["ok"], 0);
    assert_eq!(summary["err"], 1);
    assert_eq!(summary["err_indexes"], json!([0]));
  }

  /// F-A6-3: when one or more sub-flows have a node-level Err in
  /// their state, the map node MUST emit a `results_summary`
  /// sibling output that surfaces `{total, ok, err, err_indexes}`
  /// so downstream consumers can route on partial failure without
  /// walking the nested `results` JSON. `results` itself stays
  /// shaped exactly as before (back-compat).
  #[tokio::test]
  async fn test_map_node_emits_results_summary_for_partial_failure() {
    use_writable_home();

    // Inner node fails iff item == 2. Three items: [1, 2, 3] →
    // one sub-flow has a failed node, two are clean.
    struct FailOnTwo;
    #[async_trait]
    impl AsyncNode for FailOnTwo {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let val = match inputs.get("item").unwrap() {
          FlowValue::Json(Value::Number(n)) => n.as_i64().unwrap(),
          _ => 0,
        };
        if val == 2 {
          Err(YanshiError::AsyncExecutionError {
            message: "synthetic failure on item=2".to_string(),
          })
        } else {
          let mut outputs = HashMap::new();
          outputs.insert("result".to_string(), FlowValue::Json(json!(val * 2)));
          Ok(outputs)
        }
      }
    }

    let sub_flow_node = GraphNode {
      id: "fail_on_two".to_string(),
      node_type: NodeType::Standard(Arc::new(FailOnTwo)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "partial_failure_map".to_string(),
      node_type: NodeType::Map {
        template: vec![sub_flow_node],
        parallel: true,
        max_concurrent: Some(2),
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("input_list".to_string(), FlowValue::Json(json!([1, 2, 3])));
        inputs
      },
    };

    let flow = Flow::new(vec![map_node]);
    let state = flow.run().await.expect("flow itself must Ok");
    let map_result = state
      .get("partial_failure_map")
      .expect("map node must appear")
      .as_ref()
      .expect("map must Ok overall (per-sub-flow Err doesn't bubble)");

    // Back-compat: `results` still present with all 3 sub-flow
    // states (one with an inner Err).
    let results_array = match map_result.get("results").expect("results must exist") {
      FlowValue::Json(Value::Array(arr)) => arr,
      other => panic!("results must be a JSON array, got {other:?}"),
    };
    assert_eq!(results_array.len(), 3, "all 3 sub-flow states present");

    // New: `results_summary` surfaces the partial failure.
    let summary = match map_result
      .get("results_summary")
      .expect("results_summary must exist (F-A6-3)")
    {
      FlowValue::Json(s) => s,
      other => panic!("results_summary must be JSON, got {other:?}"),
    };
    assert_eq!(summary["total"], 3);
    assert_eq!(summary["ok"], 2);
    assert_eq!(summary["err"], 1);
    // The order in which sub-flows finish under parallel + cap is
    // deterministic by index (we enumerate before spawning), so
    // err_indexes should be exactly [1].
    assert_eq!(summary["err_indexes"], json!([1]));
  }

  /// F-A6-3 back-compat: when every sub-flow is clean, the
  /// summary still ships but reports `err: 0` so workflows can
  /// rely on the field being present.
  #[tokio::test]
  async fn test_map_node_emits_clean_results_summary_when_all_ok() {
    use_writable_home();

    struct AlwaysOk;
    #[async_trait]
    impl AsyncNode for AlwaysOk {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let mut outputs = HashMap::new();
        outputs.insert("ok".to_string(), FlowValue::Json(json!(true)));
        Ok(outputs)
      }
    }

    let sub_flow_node = GraphNode {
      id: "ok".to_string(),
      node_type: NodeType::Standard(Arc::new(AlwaysOk)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let map_node = GraphNode {
      id: "all_ok_map".to_string(),
      node_type: NodeType::Map {
        template: vec![sub_flow_node],
        parallel: true,
        max_concurrent: Some(2),
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("input_list".to_string(), FlowValue::Json(json!([1, 2, 3])));
        inputs
      },
    };

    let state = Flow::new(vec![map_node]).run().await.unwrap();
    let map_result = state.get("all_ok_map").unwrap().as_ref().unwrap();
    let summary = match map_result.get("results_summary").unwrap() {
      FlowValue::Json(s) => s,
      _ => panic!(),
    };
    assert_eq!(summary["total"], 3);
    assert_eq!(summary["ok"], 3);
    assert_eq!(summary["err"], 0);
    assert_eq!(summary["err_indexes"], json!([]));
  }

  #[tokio::test]
  async fn test_while_node_basic_loop() {
    use_writable_home();
    struct IncrementNode;
    #[async_trait]
    impl AsyncNode for IncrementNode {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let counter = match inputs.get("counter") {
          Some(FlowValue::Json(Value::Number(n))) => n.as_i64().unwrap(),
          _ => 1,
        };
        let mut outputs = HashMap::new();
        outputs.insert("counter".to_string(), FlowValue::Json(json!(counter + 1)));
        outputs.insert(
          "continue_loop".to_string(),
          FlowValue::Json(json!(counter < 4)),
        );
        Ok(outputs)
      }
    }

    let increment_node = GraphNode {
      id: "increment".to_string(),
      node_type: NodeType::Standard(Arc::new(IncrementNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let while_node = GraphNode {
      id: "while_loop".to_string(),
      node_type: NodeType::While {
        condition: "{{continue_loop}}".to_string(),
        max_iterations: 10,
        template: vec![increment_node],
        continue_on_error: false,
        fail_on_exhausted: false,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("counter".to_string(), FlowValue::Json(json!(1)));
        inputs.insert("continue_loop".to_string(), FlowValue::Json(json!(true)));
        inputs
      },
    };

    let flow = Flow::new(vec![while_node]);
    let final_state = flow.run().await.unwrap();
    let while_result = final_state.get("while_loop").unwrap().as_ref().unwrap();

    let counter = match while_result.get("counter").unwrap() {
      FlowValue::Json(Value::Number(n)) => n.as_i64().unwrap(),
      _ => panic!("Counter should be a number"),
    };

    // Loop runs while continue_loop=true
    // Iteration 1: counter=1, sets counter=2, continue_loop=true (1 < 4 = true)
    // Iteration 2: counter=2, sets counter=3, continue_loop=true (2 < 4 = true)
    // Iteration 3: counter=3, sets counter=4, continue_loop=true (3 < 4 = true)
    // Iteration 4: counter=4, sets counter=5, continue_loop=false (4 < 4 = false)
    // Next iteration checks: continue_loop=false, loop exits
    assert_eq!(counter, 5);
  }

  /// W1.1 regression: pre-fix, `execute_while_node` built each
  /// iteration's sub-flow via a bare `Flow::new` + `execute_from_inputs`
  /// (no config), so every iteration ignored whatever `run_base_dir` the
  /// parent was configured with and instead fell back to the hardcoded
  /// `~/.yanshi/runs` default. Runs the parent under an explicit
  /// custom `run_base_dir` and asserts every sub-flow iteration's run
  /// directory lands underneath it (parent dir + one per iteration),
  /// not off in the unrelated default location.
  #[tokio::test]
  async fn while_node_sub_flow_honors_parent_run_base_dir() {
    use_writable_home();
    struct IncrementNode;
    #[async_trait]
    impl AsyncNode for IncrementNode {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let counter = match inputs.get("counter") {
          Some(FlowValue::Json(Value::Number(n))) => n.as_i64().unwrap(),
          _ => 1,
        };
        let mut outputs = HashMap::new();
        outputs.insert("counter".to_string(), FlowValue::Json(json!(counter + 1)));
        outputs.insert(
          "continue_loop".to_string(),
          FlowValue::Json(json!(counter < 4)),
        );
        Ok(outputs)
      }
    }

    let increment_node = GraphNode {
      id: "increment".to_string(),
      node_type: NodeType::Standard(Arc::new(IncrementNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let while_node = GraphNode {
      id: "while_loop".to_string(),
      node_type: NodeType::While {
        condition: "{{continue_loop}}".to_string(),
        max_iterations: 10,
        template: vec![increment_node],
        continue_on_error: false,
        fail_on_exhausted: false,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("counter".to_string(), FlowValue::Json(json!(1)));
        inputs.insert("continue_loop".to_string(), FlowValue::Json(json!(true)));
        inputs
      },
    };

    let temp_dir = TempDir::new().unwrap();
    Flow::new(vec![while_node])
      .execute_from_inputs_with_config(
        HashMap::new(),
        FlowExecutionConfig::serial().with_run_base_dir(temp_dir.path()),
      )
      .await
      .unwrap();

    // Parent run dir + 4 while-loop iterations, all nested under the
    // explicit custom base dir.
    let run_dirs = std::fs::read_dir(temp_dir.path())
      .unwrap()
      .collect::<Result<Vec<_>, _>>()
      .unwrap();
    assert_eq!(
      run_dirs.len(),
      5,
      "expected 1 parent + 4 sub-flow iteration dirs under the custom \
       run_base_dir, got {}: {:?}",
      run_dirs.len(),
      run_dirs.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
  }

  #[tokio::test]
  async fn test_while_node_condition_check() {
    use_writable_home();
    struct CheckNode;
    #[async_trait]
    impl AsyncNode for CheckNode {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let count = match inputs.get("count") {
          Some(FlowValue::Json(Value::Number(n))) => n.as_i64().unwrap(),
          _ => 0,
        };
        let mut outputs = HashMap::new();
        outputs.insert("count".to_string(), FlowValue::Json(json!(count + 1)));
        outputs.insert("continue".to_string(), FlowValue::Json(json!(count < 2)));
        Ok(outputs)
      }
    }

    let check_node = GraphNode {
      id: "check".to_string(),
      node_type: NodeType::Standard(Arc::new(CheckNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let while_node = GraphNode {
      id: "while_loop".to_string(),
      node_type: NodeType::While {
        condition: "{{continue}}".to_string(),
        max_iterations: 10,
        template: vec![check_node],
        continue_on_error: false,
        fail_on_exhausted: false,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("count".to_string(), FlowValue::Json(json!(0)));
        inputs.insert("continue".to_string(), FlowValue::Json(json!(true)));
        inputs
      },
    };

    let flow = Flow::new(vec![while_node]);
    let final_state = flow.run().await.unwrap();
    let while_result = final_state.get("while_loop").unwrap().as_ref().unwrap();

    let count = match while_result.get("count").unwrap() {
      FlowValue::Json(Value::Number(n)) => n.as_i64().unwrap(),
      _ => panic!("Count should be a number"),
    };

    // Loop runs while continue=true
    // Iteration 1: count=0, sets count=1, continue=true (0 < 2 = true)
    // Iteration 2: count=1, sets count=2, continue=true (1 < 2 = true)
    // Iteration 3: count=2, sets count=3, continue=false (2 < 2 = false)
    // Next iteration checks: continue=false, loop exits
    assert_eq!(count, 3);
  }

  #[tokio::test]
  async fn run_if_supports_compound_expression_language() {
    use_writable_home();

    struct SearchNode;
    #[async_trait]
    impl AsyncNode for SearchNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        Ok(HashMap::from([
          (
            "items".to_string(),
            FlowValue::Json(json!(["alpha", "beta"])),
          ),
          ("score".to_string(), FlowValue::Json(json!(0.8))),
        ]))
      }
    }

    struct MarkerNode;
    #[async_trait]
    impl AsyncNode for MarkerNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        Ok(HashMap::from([(
          "ran".to_string(),
          FlowValue::Json(json!(true)),
        )]))
      }
    }

    let search = GraphNode {
      id: "search".to_string(),
      node_type: NodeType::Standard(Arc::new(SearchNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let classify = GraphNode {
      id: "classify".to_string(),
      node_type: NodeType::Standard(Arc::new(MarkerNode)),
      dependencies: vec!["search".to_string()],
      input_mapping: None,
      run_if: Some(
        "len(nodes.search.outputs.items) > 0 && nodes.search.outputs.score > 0.7".to_string(),
      ),
      initial_inputs: HashMap::new(),
    };

    let state = Flow::new(vec![search, classify]).run().await.unwrap();
    assert!(state.get("classify").unwrap().is_ok());
  }

  /// W1.2 regression: a malformed `run_if` (here, referencing a node that
  /// doesn't exist) used to propagate via `?` and abort
  /// `execute_with_workflow_id` entirely — the caller got a hard `Err`
  /// instead of the per-node `Err` result every other node-level failure
  /// produces, no `WorkflowFailed` event was ever emitted, and (with
  /// checkpointing enabled) the run's status would stay stuck at
  /// `Running` forever since no terminal status write ever happened.
  /// Asserts the call still returns `Ok(state_pool)` with the node's own
  /// entry holding the evaluation error, plus a `WorkflowFailed` event.
  #[tokio::test]
  async fn serial_run_if_evaluation_error_downgrades_to_node_failure() {
    use_writable_home();
    struct NeverRunsNode;
    #[async_trait]
    impl AsyncNode for NeverRunsNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        panic!("must never execute — run_if should fail before this node runs");
      }
    }

    let node = GraphNode {
      id: "guarded".to_string(),
      node_type: NodeType::Standard(Arc::new(NeverRunsNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: Some("nodes.does_not_exist.outputs.x > 0".to_string()),
      initial_inputs: HashMap::new(),
    };

    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });
    let final_state = Flow::new(vec![node])
      .with_event_listener(listener)
      .run()
      .await
      .expect("a run_if evaluation error must not abort the whole call");

    assert!(
      matches!(
        final_state.get("guarded"),
        Some(Err(YanshiError::FlowDefinitionError { .. }))
      ),
      "expected a FlowDefinitionError entry for 'guarded', got {:?}",
      final_state.get("guarded")
    );
    let events = events.lock().unwrap();
    assert!(
      events.contains(&"workflow.failed"),
      "expected a WorkflowFailed event, got {events:?}"
    );
  }

  /// W1.2 regression: the concurrent scheduler's `evaluate_condition`
  /// call had the identical `?`-abort bug as the serial path — fixed in
  /// lockstep with the serial path above.
  #[tokio::test]
  async fn concurrent_run_if_evaluation_error_downgrades_to_node_failure() {
    use_writable_home();
    struct NeverRunsNode;
    #[async_trait]
    impl AsyncNode for NeverRunsNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        panic!("must never execute — run_if should fail before this node runs");
      }
    }

    let node = GraphNode {
      id: "guarded".to_string(),
      node_type: NodeType::Standard(Arc::new(NeverRunsNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: Some("nodes.does_not_exist.outputs.x > 0".to_string()),
      initial_inputs: HashMap::new(),
    };

    let final_state = Flow::new(vec![node])
      .execute_from_inputs_with_config(HashMap::new(), FlowExecutionConfig::concurrent(2))
      .await
      .expect("a run_if evaluation error must not abort the whole call");

    assert!(
      matches!(
        final_state.get("guarded"),
        Some(Err(YanshiError::FlowDefinitionError { .. }))
      ),
      "expected a FlowDefinitionError entry for 'guarded', got {:?}",
      final_state.get("guarded")
    );
  }

  /// W1.2 regression: pre-fix, the Serial execution path's skip branch
  /// never called `record_node_result_events`, so a skipped node
  /// produced no `NodeSkipped` event for the listener — asymmetric with
  /// the Concurrent path, which always emitted one. Both paths now
  /// route skips through the same unified tail.
  #[tokio::test]
  async fn serial_skip_emits_node_skipped_event() {
    use_writable_home();
    struct NeverRunsNode;
    #[async_trait]
    impl AsyncNode for NeverRunsNode {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        panic!("must never execute — run_if is false");
      }
    }

    let node = GraphNode {
      id: "skipped".to_string(),
      node_type: NodeType::Standard(Arc::new(NeverRunsNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: Some("false".to_string()),
      initial_inputs: HashMap::new(),
    };

    let events = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::new(RecordingListener {
      events: events.clone(),
    });
    let final_state = Flow::new(vec![node])
      .with_event_listener(listener)
      .run()
      .await
      .unwrap();

    assert!(matches!(
      final_state.get("skipped"),
      Some(Err(YanshiError::NodeSkipped))
    ));
    let events = events.lock().unwrap();
    assert!(
      events.contains(&"node.skipped"),
      "expected a node.skipped event under Serial execution too, got {events:?}"
    );
  }

  #[tokio::test]
  async fn while_condition_supports_numeric_expression() {
    use_writable_home();

    struct IncrementNode;
    #[async_trait]
    impl AsyncNode for IncrementNode {
      async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let count = match inputs.get("count") {
          Some(FlowValue::Json(Value::Number(n))) => n.as_i64().unwrap(),
          _ => 0,
        };
        Ok(HashMap::from([(
          "count".to_string(),
          FlowValue::Json(json!(count + 1)),
        )]))
      }
    }

    let increment_node = GraphNode {
      id: "increment".to_string(),
      node_type: NodeType::Standard(Arc::new(IncrementNode)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };

    let while_node = GraphNode {
      id: "while_loop".to_string(),
      node_type: NodeType::While {
        condition: "{{ count < 3 }}".to_string(),
        max_iterations: 10,
        template: vec![increment_node],
        continue_on_error: false,
        fail_on_exhausted: false,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::from([("count".to_string(), FlowValue::Json(json!(0)))]),
    };

    let final_state = Flow::new(vec![while_node]).run().await.unwrap();
    let while_result = final_state.get("while_loop").unwrap().as_ref().unwrap();
    assert_eq!(while_result.get("count"), Some(&FlowValue::Json(json!(3))));
  }

  // ── D11 (W2.4): While loop observability ──────────────────────────────

  struct IncrementUntil3;
  #[async_trait]
  impl AsyncNode for IncrementUntil3 {
    async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
      let count = match inputs.get("count") {
        Some(FlowValue::Json(Value::Number(n))) => n.as_i64().unwrap(),
        _ => 0,
      };
      Ok(HashMap::from([(
        "count".to_string(),
        FlowValue::Json(json!(count + 1)),
      )]))
    }
  }

  fn count_while_node(condition: &str, max_iterations: u32, fail_on_exhausted: bool) -> GraphNode {
    let increment_node = GraphNode {
      id: "increment".to_string(),
      node_type: NodeType::Standard(Arc::new(IncrementUntil3)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    GraphNode {
      id: "while_loop".to_string(),
      node_type: NodeType::While {
        condition: condition.to_string(),
        max_iterations,
        template: vec![increment_node],
        continue_on_error: false,
        fail_on_exhausted,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::from([("count".to_string(), FlowValue::Json(json!(0)))]),
    }
  }

  #[tokio::test]
  async fn while_node_reports_iterations_used_and_exhausted_false_on_normal_exit() {
    use_writable_home();
    // Condition turns false once count reaches 3, well before max_iterations
    // (10) — a normal exit, not exhaustion.
    let while_node = count_while_node("{{ count < 3 }}", 10, false);
    let final_state = Flow::new(vec![while_node]).run().await.unwrap();
    let while_result = final_state.get("while_loop").unwrap().as_ref().unwrap();
    assert_eq!(
      while_result.get("iterations_used"),
      Some(&FlowValue::Json(json!(3))),
      "3 iterations must actually run before the condition turns false"
    );
    assert_eq!(
      while_result.get("exhausted"),
      Some(&FlowValue::Json(json!(false))),
      "a normal condition-false exit must not report exhausted"
    );
  }

  #[tokio::test]
  async fn while_node_reports_exhausted_true_when_max_iterations_reached() {
    use_writable_home();
    // Condition is always true — the loop can only stop via exhaustion.
    let while_node = count_while_node("true", 3, false);
    let final_state = Flow::new(vec![while_node]).run().await.unwrap();
    let while_result = final_state.get("while_loop").unwrap().as_ref().unwrap();
    assert_eq!(
      while_result.get("iterations_used"),
      Some(&FlowValue::Json(json!(3)))
    );
    assert_eq!(
      while_result.get("exhausted"),
      Some(&FlowValue::Json(json!(true))),
      "reaching max_iterations without the condition ever going false must report exhausted"
    );
  }

  #[tokio::test]
  async fn while_node_fail_on_exhausted_fails_the_while_node() {
    use_writable_home();
    let while_node = count_while_node("true", 3, true);
    let final_state = Flow::new(vec![while_node]).run().await.unwrap();
    let while_result = final_state.get("while_loop").unwrap();
    assert!(
      while_result.is_err(),
      "fail_on_exhausted=true must fail the While node on exhaustion, got {:?}",
      while_result
    );
  }

  struct AlwaysFails;
  #[async_trait]
  impl AsyncNode for AlwaysFails {
    async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
      Err(YanshiError::NodeExecutionFailed {
        message: "boom".to_string(),
      })
    }
  }

  fn failing_while_node(continue_on_error: bool) -> GraphNode {
    let failing_node = GraphNode {
      id: "fail".to_string(),
      node_type: NodeType::Standard(Arc::new(AlwaysFails)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    GraphNode {
      id: "while_loop".to_string(),
      node_type: NodeType::While {
        condition: "true".to_string(),
        max_iterations: 3,
        template: vec![failing_node],
        continue_on_error,
        fail_on_exhausted: false,
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    }
  }

  #[tokio::test]
  async fn while_node_sub_flow_failure_fails_the_while_node_by_default() {
    use_writable_home();
    let final_state = Flow::new(vec![failing_while_node(false)])
      .run()
      .await
      .unwrap();
    let while_result = final_state.get("while_loop").unwrap();
    assert!(
      while_result.is_err(),
      "a failing sub-flow exit node must fail the While node by default, got {:?}",
      while_result
    );
  }

  #[tokio::test]
  async fn while_node_continue_on_error_swallows_failures_and_reaches_exhaustion() {
    use_writable_home();
    let final_state = Flow::new(vec![failing_while_node(true)])
      .run()
      .await
      .unwrap();
    let while_result =
      final_state.get("while_loop").unwrap().as_ref().expect(
        "continue_on_error=true must swallow the sub-flow failure, not fail the While node",
      );
    assert_eq!(
      while_result.get("iterations_used"),
      Some(&FlowValue::Json(json!(3))),
      "all 3 iterations must run despite every one failing"
    );
    assert_eq!(
      while_result.get("exhausted"),
      Some(&FlowValue::Json(json!(true)))
    );
  }

  /// Q2.4.1 regression: `topological_sort` returns the same node
  /// order across many invocations on the same graph. Pre-fix the
  /// HashMap-driven queue construction made the order depend on the
  /// HashMap's hashing/resizing state, breaking trace replay.
  #[test]
  fn topological_sort_is_deterministic_across_runs() {
    use crate::async_node::AsyncNodeInputs;
    use crate::async_node::AsyncNodeResult;
    use async_trait::async_trait;

    struct Stub;
    #[async_trait]
    impl AsyncNode for Stub {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        Ok(HashMap::new())
      }
    }

    fn graph_node(id: &str, deps: Vec<&str>) -> GraphNode {
      GraphNode {
        id: id.to_string(),
        node_type: NodeType::Standard(Arc::new(Stub)),
        dependencies: deps.into_iter().map(String::from).collect(),
        input_mapping: None,
        run_if: None,
        initial_inputs: HashMap::new(),
      }
    }

    // Diamond: a → b, a → c, b → d, c → d. b and c have equal
    // in-degree after a is consumed; the order they enter the queue
    // must be deterministic (alphabetical by id under our BTreeMap
    // fix).
    let make_graph = || -> Vec<GraphNode> {
      vec![
        graph_node("d", vec!["b", "c"]),
        graph_node("b", vec!["a"]),
        graph_node("c", vec!["a"]),
        graph_node("a", vec![]),
      ]
    };

    let mut seen_orders: std::collections::HashSet<Vec<String>> = std::collections::HashSet::new();
    for _ in 0..50 {
      let flow = Flow::new(make_graph());
      let order = FlowExecutor::new(&flow).topological_sort().unwrap();
      seen_orders.insert(order);
    }
    assert_eq!(
      seen_orders.len(),
      1,
      "topological_sort must be deterministic; observed {} distinct orders",
      seen_orders.len()
    );
  }
  /// Y2.3: spend recorded inside parallel Map items (spawned sub-flow
  /// tasks) counts toward the Map node and the run, and the run's limit
  /// then refuses the next node.
  #[tokio::test]
  async fn map_item_spend_rolls_up_to_the_map_node_and_trips_the_limit() {
    use_writable_home();

    struct Billed;
    #[async_trait]
    impl AsyncNode for Billed {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        crate::run_context::record_cost_usd(0.01);
        Ok(HashMap::new())
      }
    }

    #[derive(Default)]
    struct Costs(std::sync::Mutex<Vec<(String, f64, f64)>>);
    impl EventListener for Costs {
      fn on_event(&self, event: &WorkflowEvent) {
        if let WorkflowEvent::NodeCostRecorded {
          node_id,
          cost_usd,
          total_cost_usd,
          ..
        } = event
        {
          self
            .0
            .lock()
            .unwrap()
            .push((node_id.clone(), *cost_usd, *total_cost_usd));
        }
      }
    }

    let standard = |id: &str, deps: Vec<&str>| GraphNode {
      id: id.to_string(),
      node_type: NodeType::Standard(Arc::new(Billed)),
      dependencies: deps.into_iter().map(String::from).collect(),
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let map_node = GraphNode {
      id: "map_node".to_string(),
      node_type: NodeType::Map {
        template: vec![standard("item", vec![])],
        parallel: true,
        max_concurrent: Some(2),
      },
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("input_list".to_string(), FlowValue::Json(json!([1, 2, 3])));
        inputs
      },
    };

    let costs = Arc::new(Costs::default());
    let temp_dir = TempDir::new().unwrap();
    let state = Flow::new(vec![map_node, standard("after", vec!["map_node"])])
      .with_event_listener(costs.clone())
      .execute_from_inputs_with_config(
        HashMap::new(),
        FlowExecutionConfig::serial()
          .with_run_base_dir(temp_dir.path())
          .with_cost_limit_usd(0.025),
      )
      .await
      .unwrap();

    let costs = costs.0.lock().unwrap().clone();
    let map_cost = costs
      .iter()
      .find(|(id, _, _)| id == "map_node")
      .unwrap_or_else(|| panic!("no cost event for the Map node: {costs:?}"));
    assert!((map_cost.1 - 0.03).abs() < 1e-9, "{costs:?}");
    assert!((map_cost.2 - 0.03).abs() < 1e-9, "{costs:?}");
    assert!(
      matches!(
        state.get("after"),
        Some(Err(YanshiError::CostLimitExceeded { .. }))
      ),
      "the node after the Map must be refused: {:?}",
      state.get("after")
    );
  }

  /// Y1.1: every node sees its run's directory as the artifact root,
  /// and Map items (which run in spawned sub-flow tasks with their own
  /// run dirs) inherit the *parent* run's root rather than their own.
  #[tokio::test]
  async fn nodes_see_the_run_dir_as_artifact_root_and_map_items_inherit_it() {
    use_writable_home();

    struct RootReporter;
    #[async_trait]
    impl AsyncNode for RootReporter {
      async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
        let root = crate::run_context::current_artifact_root()
          .map(|p| p.to_string_lossy().to_string())
          .unwrap_or_default();
        let mut outputs = HashMap::new();
        outputs.insert("root".to_string(), FlowValue::Json(json!(root)));
        Ok(outputs)
      }
    }

    let top = GraphNode {
      id: "top".to_string(),
      node_type: NodeType::Standard(Arc::new(RootReporter)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let item = GraphNode {
      id: "item".to_string(),
      node_type: NodeType::Standard(Arc::new(RootReporter)),
      dependencies: vec![],
      input_mapping: None,
      run_if: None,
      initial_inputs: HashMap::new(),
    };
    let map_node = GraphNode {
      id: "map_node".to_string(),
      node_type: NodeType::Map {
        template: vec![item],
        parallel: true,
        max_concurrent: Some(2),
      },
      dependencies: vec!["top".to_string()],
      input_mapping: None,
      run_if: None,
      initial_inputs: {
        let mut inputs = HashMap::new();
        inputs.insert("input_list".to_string(), FlowValue::Json(json!([1, 2, 3])));
        inputs
      },
    };

    let temp_dir = TempDir::new().unwrap();
    let state = Flow::new(vec![top, map_node])
      .execute_from_inputs_with_config(
        HashMap::new(),
        FlowExecutionConfig::serial().with_run_base_dir(temp_dir.path()),
      )
      .await
      .unwrap();

    let top_root = match state.get("top").unwrap().as_ref().unwrap().get("root") {
      Some(FlowValue::Json(Value::String(s))) => s.clone(),
      other => panic!("unexpected root output: {other:?}"),
    };
    assert!(
      std::path::Path::new(&top_root).starts_with(temp_dir.path()),
      "root {top_root} must be under the run base dir"
    );
    assert!(
      std::path::Path::new(&top_root)
        .join("top_outputs.json")
        .exists(),
      "root must be the run dir itself (where per-step outputs land)"
    );

    let results = match state
      .get("map_node")
      .unwrap()
      .as_ref()
      .unwrap()
      .get("results")
    {
      Some(FlowValue::Json(Value::Array(arr))) => arr.clone(),
      other => panic!("unexpected map results: {other:?}"),
    };
    assert_eq!(results.len(), 3);
    for result in results {
      // Sub-flow state pools serialise each node's `AsyncNodeResult`
      // as `{"Ok": {...}}` and each `FlowValue` as `{"type", "value"}`.
      let item_root = result["item"]["Ok"]["root"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("unexpected item shape: {result}"))
        .to_string();
      assert_eq!(
        item_root, top_root,
        "a Map item must inherit the parent run's artifact root"
      );
    }
  }
}
