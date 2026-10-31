//! W4.3b — distributed run execution: wiring `DistributedDagScheduler`
//! into `POST /v1/runs` via an opt-in `execution_mode: "distributed"`.
//!
//! See `docs/DISTRIBUTED.md` § Planned Control-Plane Flow for the
//! high-level design this implements, and the approved plan this shipped
//! against for the full research/decision record.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tracing::error;

use yanshi_config::config::v2::FlowDefinitionV2;
use yanshi_core::events::{EventListener, WorkflowEvent};
use yanshi_db::{RunRepo, RunStatus};

use crate::error::ApiError;
use crate::events_stream::{WorkflowEventListener, broker_finalize_grace};
use crate::runs::{RunContext, RunExecutor};
use crate::scheduler::{
  AuthenticatedControlPlane, DistributedDagScheduler, DistributedNodeStatus, InMemoryWorkerProtocol,
};

/// Node types `yanshi-worker::execute_supported_node_payload` actually
/// executes today (`yanshi-worker/src/lib.rs`). Anything outside this
/// set — `skill_agent`/`multi_agent`, `plugin`, `shell`, media/RAG nodes,
/// `while`/`map` loop constructs — has no distributed execution path;
/// expanding worker payload coverage is separate future work, not
/// silently degraded here.
const DISTRIBUTED_SUPPORTED_NODE_TYPES: &[&str] =
  &["template", "file", "mock", "llm", "http", "mcp", "agent"];

/// Reject (rather than silently mis-execute) workflow shapes
/// `DistributedDagScheduler` can't correctly express, before any DB row
/// or worker task is created:
///
/// - a declared `inputs:` block — `DistributedDagScheduler::gather_inputs`
///   only resolves `input_mapping` values shaped `nodes.<id>.outputs.<key>`
///   plus each node's own literal `parameters`; there is no path for
///   `FlowDefinitionV2.inputs` (the `T3.2` default-filling mechanism
///   `flow_execute` uses for in-process runs) to reach a node at all.
/// - any node with `run_if` set — `DistributedDagScheduler::is_ready`
///   never reads it, so a conditional node would execute unconditionally
///   instead of being silently skipped as the in-process executor would.
/// - any node whose `type` isn't in [`DISTRIBUTED_SUPPORTED_NODE_TYPES`].
pub fn validate_distributed_flow(flow: &FlowDefinitionV2) -> Result<(), ApiError> {
  if !flow.inputs.is_empty() {
    return Err(ApiError::BadRequest(
      "distributed execution does not support workflows with a declared `inputs:` block yet \
       (DistributedDagScheduler has no path for default-filled inputs to reach a node) — \
       remove `inputs:` or submit with the default in-process execution_mode"
        .to_string(),
    ));
  }
  for node in &flow.nodes {
    if node.run_if.is_some() {
      return Err(ApiError::BadRequest(format!(
        "distributed execution does not support conditional nodes yet (`run_if` on node '{}' \
         would execute unconditionally instead of being evaluated) — remove `run_if` or submit \
         with the default in-process execution_mode",
        node.id
      )));
    }
    if !DISTRIBUTED_SUPPORTED_NODE_TYPES.contains(&node.node_type.as_str()) {
      return Err(ApiError::BadRequest(format!(
        "distributed execution does not support node type '{}' (node '{}') — the worker \
         supports {:?}; submit with the default in-process execution_mode instead",
        node.node_type, node.id, DISTRIBUTED_SUPPORTED_NODE_TYPES
      )));
    }
  }
  Ok(())
}

#[derive(Debug, thiserror::Error)]
enum DistributedRunError {
  #[error(transparent)]
  Db(#[from] yanshi_db::DbError),
  #[error(transparent)]
  Build(#[from] anyhow::Error),
  #[error(transparent)]
  Scheduler(#[from] yanshi_core::error::YanshiError),
  #[error("{0}")]
  Validation(String),
}

impl From<ApiError> for DistributedRunError {
  fn from(err: ApiError) -> Self {
    Self::Validation(err.to_string())
  }
}

/// How often the drive loop polls the scheduler between
/// dispatch/reconcile/requeue passes. Matches the general shape of
/// `DistributedDagScheduler::drive_until_complete`'s own `idle_sleep`
/// parameter (that method isn't used directly here since it doesn't
/// bridge events or check cancellation — see `drive_distributed_run`).
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// W4.3b: `RunExecutor` implementation that drives a
/// [`DistributedDagScheduler`] to completion instead of running the
/// workflow in-process. Mirrors `runs::flow_execute`'s shape (parse →
/// run → map terminal state to `RunStatus`) but the "run" step is a
/// node-by-node dispatch/reconcile loop against real (or in-process, in
/// tests) workers instead of a single `yanshi_core::Flow::execute`
/// call.
pub struct DistributedFlowRunExecutor {
  control_plane: Arc<AuthenticatedControlPlane<InMemoryWorkerProtocol>>,
}

impl DistributedFlowRunExecutor {
  pub fn new(control_plane: Arc<AuthenticatedControlPlane<InMemoryWorkerProtocol>>) -> Self {
    Self { control_plane }
  }
}

#[async_trait]
impl RunExecutor for DistributedFlowRunExecutor {
  async fn execute(&self, ctx: RunContext) {
    if let Err(e) = drive_distributed_run(&ctx, &self.control_plane).await {
      error!(run_id = %ctx.run_id, error = %e, "distributed flow executor failed");
      let _ = ctx
        .repos
        .runs
        .update_status(ctx.run_id, RunStatus::Failed, Some(&e.to_string()))
        .await;
      if let Some(registry) = &ctx.live_state_registry {
        registry.deregister(&ctx.run_id);
      }
      ctx
        .broker
        .finalise_with_grace(ctx.run_id, broker_finalize_grace());
    }
  }
}

/// Drives one distributed run to a terminal DB/event state.
///
/// Node-level `WorkflowEvent`s are synthesized from the scheduler's
/// polled state rather than pushed by an executing node (unlike
/// in-process `Flow` execution) — [`DistributedDagScheduler::run_result`]
/// is a cheap, side-effect-free snapshot of every node's current
/// `outputs`/failure the scheduler already tracks internally, so each
/// loop iteration diffs it against what this function has already
/// emitted and fires the corresponding `NodeCompleted`/`NodeFailed`
/// event for whatever changed. `NodeStarted` is emitted the moment
/// [`DistributedDagScheduler::node_status`] first reports `Running` for
/// a node. Per-node duration is measured from when this function first
/// observed the node running, not from true worker-side start/end
/// timestamps (the scheduler doesn't expose those) — an accepted
/// approximation of a polling design.
async fn drive_distributed_run(
  ctx: &RunContext,
  control_plane: &Arc<AuthenticatedControlPlane<InMemoryWorkerProtocol>>,
) -> Result<(), DistributedRunError> {
  ctx
    .repos
    .runs
    .update_status(ctx.run_id, RunStatus::Running, None)
    .await?;

  let flow_def: FlowDefinitionV2 =
    yanshi_config::executor::parse_workflow_definition(&ctx.workflow)?;
  validate_distributed_flow(&flow_def)?;
  let node_ids: Vec<String> = flow_def.nodes.iter().map(|node| node.id.clone()).collect();

  let workflow_id = ctx.run_id.to_string();
  let listener = WorkflowEventListener::from_state(
    ctx.run_id,
    ctx.tenant_id.clone(),
    ctx.repos.clone(),
    ctx.broker.clone(),
    0,
  );
  let run_started_at = Instant::now();
  listener.on_event(&WorkflowEvent::WorkflowStarted {
    workflow_id: workflow_id.clone(),
    timestamp: Instant::now(),
  });

  let mut scheduler =
    DistributedDagScheduler::new(ctx.run_id, flow_def, control_plane.inner().clone())?;

  let mut node_started_at: HashMap<String, Instant> = HashMap::new();
  let mut emitted_started: HashSet<String> = HashSet::new();
  let mut emitted_completed: HashSet<String> = HashSet::new();
  let mut emitted_failed: HashSet<String> = HashSet::new();

  loop {
    if ctx.cancellation_token.is_cancelled() {
      listener.on_event(&WorkflowEvent::WorkflowCancelled {
        workflow_id: workflow_id.clone(),
        reason: "run cancelled".to_string(),
        duration: run_started_at.elapsed(),
        timestamp: Instant::now(),
      });
      ctx
        .repos
        .runs
        .update_status(ctx.run_id, RunStatus::Cancelled, Some("cancel requested"))
        .await?;
      break;
    }

    let _ = scheduler.requeue_stale_tasks().await?;
    let _ = scheduler.reconcile_results().await?;

    // Emit completions/failures from *before* this iteration's dispatch,
    // so a node that just unblocked a dependent (both visible in this
    // same poll) always reports its own completion first — dispatching
    // the dependent below, then checking for newly-`Running` nodes
    // afterward, keeps `NodeCompleted(parent)` ordered before
    // `NodeStarted(child)` in the emitted stream even though both
    // transitions happened within one poll window.
    let snapshot = scheduler.run_result();
    for node_id in snapshot.state_pool.keys() {
      if emitted_completed.insert(node_id.clone()) {
        let duration = node_started_at
          .get(node_id)
          .map(|start| start.elapsed())
          .unwrap_or_default();
        listener.on_event(&WorkflowEvent::NodeCompleted {
          workflow_id: workflow_id.clone(),
          node_id: node_id.clone(),
          duration,
          timestamp: Instant::now(),
        });
      }
    }
    for (node_id, error) in &snapshot.failed_nodes {
      if emitted_failed.insert(node_id.clone()) {
        let duration = node_started_at
          .get(node_id)
          .map(|start| start.elapsed())
          .unwrap_or_default();
        listener.on_event(&WorkflowEvent::NodeFailed {
          workflow_id: workflow_id.clone(),
          node_id: node_id.clone(),
          error: error.clone(),
          duration,
          timestamp: Instant::now(),
        });
      }
    }

    let _ = scheduler.dispatch_ready().await?;

    for node_id in &node_ids {
      if scheduler.node_status(node_id) == Some(DistributedNodeStatus::Running)
        && emitted_started.insert(node_id.clone())
      {
        node_started_at.insert(node_id.clone(), Instant::now());
        listener.on_event(&WorkflowEvent::NodeStarted {
          workflow_id: workflow_id.clone(),
          node_id: node_id.clone(),
          timestamp: Instant::now(),
        });
      }
    }

    if scheduler.is_terminal() {
      let result = scheduler.run_result();
      if result.succeeded {
        listener.on_event(&WorkflowEvent::WorkflowCompleted {
          workflow_id: workflow_id.clone(),
          duration: run_started_at.elapsed(),
          timestamp: Instant::now(),
        });
        ctx
          .repos
          .runs
          .update_status(ctx.run_id, RunStatus::Succeeded, None)
          .await?;
      } else {
        let error_summary = result
          .failed_nodes
          .iter()
          .map(|(node_id, error)| format!("{node_id}: {error}"))
          .collect::<Vec<_>>()
          .join("; ");
        listener.on_event(&WorkflowEvent::WorkflowFailed {
          workflow_id: workflow_id.clone(),
          error: error_summary.clone(),
          duration: run_started_at.elapsed(),
          timestamp: Instant::now(),
        });
        ctx
          .repos
          .runs
          .update_status(ctx.run_id, RunStatus::Failed, Some(&error_summary))
          .await?;
      }
      break;
    }

    tokio::time::sleep(POLL_INTERVAL).await;
  }

  // Worker-side spans come back over the protocol and are stitched here,
  // so the server is the one exporter for distributed runs too.
  if let Some(sink) = crate::runs::otlp_span_sink() {
    let spans = control_plane.inner().stitched_otel_spans(ctx.run_id).await;
    if let Err(err) = sink.export_spans(spans).await {
      error!(run_id = %ctx.run_id, error = %err, "tracing: OTLP export of worker spans failed");
    }
  }

  if let Some(registry) = &ctx.live_state_registry {
    registry.deregister(&ctx.run_id);
  }
  ctx
    .broker
    .finalise_with_grace(ctx.run_id, broker_finalize_grace());
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;
  use yanshi_config::config::v2::NodeDefinitionV2;

  fn node(id: &str, node_type: &str) -> NodeDefinitionV2 {
    NodeDefinitionV2 {
      id: id.to_string(),
      node_type: node_type.to_string(),
      dependencies: Vec::new(),
      input_mapping: HashMap::new(),
      run_if: None,
      timeout_ms: None,
      max_retries: None,
      parameters: Default::default(),
    }
  }

  fn flow(nodes: Vec<NodeDefinitionV2>) -> FlowDefinitionV2 {
    FlowDefinitionV2 {
      name: "test".to_string(),
      inputs: HashMap::new(),
      nodes,
    }
  }

  #[test]
  fn accepts_a_clean_template_to_http_chain() {
    let mut render = node("render", "template");
    let mut fetch = node("fetch", "http");
    fetch.dependencies = vec!["render".to_string()];
    fetch.input_mapping.insert(
      "body".to_string(),
      "{{nodes.render.outputs.text}}".to_string(),
    );
    render.dependencies = Vec::new();
    assert!(validate_distributed_flow(&flow(vec![render, fetch])).is_ok());
  }

  #[test]
  fn rejects_a_declared_inputs_block() {
    let mut f = flow(vec![node("render", "template")]);
    f.inputs.insert(
      "topic".to_string(),
      yanshi_config::config::v2::InputDefinitionV2 {
        description: None,
        required: false,
        default: None,
      },
    );
    let err = validate_distributed_flow(&f).expect_err("must reject declared inputs");
    assert!(matches!(err, ApiError::BadRequest(msg) if msg.contains("inputs:")));
  }

  #[test]
  fn rejects_a_run_if_node() {
    let mut n = node("maybe", "template");
    n.run_if = Some("{{ nodes.render.outputs.ok }}".to_string());
    let err = validate_distributed_flow(&flow(vec![n])).expect_err("must reject run_if");
    assert!(matches!(err, ApiError::BadRequest(msg) if msg.contains("run_if")));
  }

  #[test]
  fn rejects_an_unsupported_node_type() {
    let err = validate_distributed_flow(&flow(vec![node("agent1", "skill_agent")]))
      .expect_err("must reject unsupported node type");
    assert!(matches!(err, ApiError::BadRequest(msg) if msg.contains("skill_agent")));
  }

  #[test]
  fn accepts_every_worker_supported_node_type() {
    for node_type in DISTRIBUTED_SUPPORTED_NODE_TYPES {
      let f = flow(vec![node("n", node_type)]);
      assert!(
        validate_distributed_flow(&f).is_ok(),
        "expected '{node_type}' to be accepted"
      );
    }
  }
}
