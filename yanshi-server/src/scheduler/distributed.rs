//! Distributed DAG scheduler built on [`WorkerControlPlane`].
//!
//! This module is intentionally narrow: it turns a fixed DAG made of portable
//! node payloads into worker tasks, maintains the ready set, and folds worker
//! results back into a state pool. It does not replace `/v1/runs` yet.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use yanshi_config::config::v2::{FlowDefinitionV2, NodeDefinitionV2};
use yanshi_core::{FlowValue, YanshiError};

// `NodeExecutionPayload` moved to `yanshi-worker-proto` (P-A2.3); it reaches
// this module via the `scheduler` re-export.
use super::{NodeExecutionPayload, SchedulerError, WorkerControlPlane, WorkerProtocol, WorkerTask};

/// Terminal status for one distributed DAG node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributedNodeStatus {
  Pending,
  Running,
  Succeeded,
  Failed,
}

#[derive(Debug, Clone)]
struct NodeRuntimeState {
  status: DistributedNodeStatus,
  attempt: u32,
  task_id: Option<Uuid>,
  retryable: bool,
  last_error: Option<String>,
  outputs: Option<HashMap<String, FlowValue>>,
  updated_at: DateTime<Utc>,
}

impl NodeRuntimeState {
  fn pending() -> Self {
    Self {
      status: DistributedNodeStatus::Pending,
      attempt: 0,
      task_id: None,
      retryable: false,
      last_error: None,
      outputs: None,
      updated_at: Utc::now(),
    }
  }
}

/// Result returned after a distributed DAG run reaches a terminal state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DistributedDagRunResult {
  pub run_id: Uuid,
  pub succeeded: bool,
  pub state_pool: HashMap<String, HashMap<String, FlowValue>>,
  pub failed_nodes: HashMap<String, String>,
}

/// Ready-set scheduler for config-first DAGs.
#[derive(Debug)]
pub struct DistributedDagScheduler<P> {
  control: WorkerControlPlane<P>,
  run_id: Uuid,
  nodes: HashMap<String, NodeDefinitionV2>,
  order: Vec<String>,
  state: HashMap<String, NodeRuntimeState>,
  max_attempts: u32,
  heartbeat_timeout: Duration,
}

impl<P> DistributedDagScheduler<P>
where
  P: WorkerProtocol + Clone,
{
  pub fn new(
    run_id: Uuid,
    flow: FlowDefinitionV2,
    control: WorkerControlPlane<P>,
  ) -> Result<Self, YanshiError> {
    let order = topological_order(&flow.nodes)?;
    let nodes = flow
      .nodes
      .into_iter()
      .map(|node| (node.id.clone(), node))
      .collect::<HashMap<_, _>>();
    let state = nodes
      .keys()
      .map(|node_id| (node_id.clone(), NodeRuntimeState::pending()))
      .collect();
    Ok(Self {
      control,
      run_id,
      nodes,
      order,
      state,
      max_attempts: 2,
      heartbeat_timeout: Duration::from_secs(30),
    })
  }

  pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
    self.max_attempts = max_attempts.max(1);
    self
  }

  pub fn with_heartbeat_timeout(mut self, timeout: Duration) -> Self {
    self.heartbeat_timeout = timeout;
    self
  }

  pub fn control_plane(&self) -> WorkerControlPlane<P> {
    self.control.clone()
  }

  pub fn node_status(&self, node_id: &str) -> Option<DistributedNodeStatus> {
    self.state.get(node_id).map(|state| state.status)
  }

  pub fn pending_count(&self) -> usize {
    self
      .state
      .values()
      .filter(|state| state.status == DistributedNodeStatus::Pending)
      .count()
  }

  pub fn running_count(&self) -> usize {
    self
      .state
      .values()
      .filter(|state| state.status == DistributedNodeStatus::Running)
      .count()
  }

  /// Dispatch all currently-ready nodes.
  pub async fn dispatch_ready(&mut self) -> Result<usize, YanshiError> {
    let ready = self
      .order
      .iter()
      .filter(|node_id| self.is_ready(node_id))
      .cloned()
      .collect::<Vec<_>>();
    let mut dispatched = 0;
    for node_id in ready {
      self.dispatch_node(&node_id).await?;
      dispatched += 1;
    }
    Ok(dispatched)
  }

  /// Fold terminal worker results into scheduler state.
  pub async fn reconcile_results(&mut self) -> Result<usize, YanshiError> {
    let Some(snapshot) = self.control.run_snapshot(self.run_id).await else {
      return Ok(0);
    };
    let mut changed = 0;
    for (node_id, output) in snapshot.outputs {
      let Some(runtime_state) = self.state.get_mut(&node_id) else {
        continue;
      };
      if runtime_state.status == DistributedNodeStatus::Succeeded {
        continue;
      }
      let outputs = flow_outputs_from_json(output)?;
      runtime_state.status = DistributedNodeStatus::Succeeded;
      runtime_state.outputs = Some(outputs);
      runtime_state.task_id = None;
      runtime_state.updated_at = Utc::now();
      changed += 1;
    }
    if snapshot.failed_tasks > 0 {
      self.reconcile_failed_assignments().await?;
    }
    Ok(changed)
  }

  /// Requeue tasks held by workers that stopped heartbeating.
  pub async fn requeue_stale_tasks(&mut self) -> Result<usize, YanshiError> {
    let assignments = self.control.assignments_for_run(self.run_id).await;
    let mut requeued = 0;
    for assignment in assignments {
      let Some(heartbeat) = self.control.worker_heartbeat(&assignment.worker_id).await else {
        continue;
      };
      let Ok(age) = Utc::now().signed_duration_since(heartbeat.ts).to_std() else {
        continue;
      };
      if age <= self.heartbeat_timeout {
        continue;
      }
      let Some(state) = self.state.get_mut(&assignment.node_id) else {
        continue;
      };
      if state.status != DistributedNodeStatus::Running {
        continue;
      }
      let _ = self.control.forget_assignment(assignment.task_id).await;
      if state.attempt + 1 >= self.max_attempts {
        state.status = DistributedNodeStatus::Failed;
        state.retryable = false;
        state.last_error = Some(format!(
          "worker '{}' heartbeat stale while running task {}",
          assignment.worker_id.0, assignment.task_id
        ));
        state.task_id = None;
        state.updated_at = Utc::now();
        continue;
      }
      state.status = DistributedNodeStatus::Pending;
      state.attempt += 1;
      state.retryable = true;
      state.last_error = Some(format!(
        "worker '{}' heartbeat stale while running task {}",
        assignment.worker_id.0, assignment.task_id
      ));
      state.task_id = None;
      state.updated_at = Utc::now();
      requeued += 1;
    }
    Ok(requeued)
  }

  /// Drive scheduling until a terminal state. The caller is responsible for
  /// running workers concurrently.
  pub async fn drive_until_complete(
    &mut self,
    idle_sleep: Duration,
  ) -> Result<DistributedDagRunResult, YanshiError> {
    loop {
      let _ = self.requeue_stale_tasks().await?;
      let _ = self.reconcile_results().await?;
      let _ = self.dispatch_ready().await?;
      if self.is_terminal() {
        return Ok(self.run_result());
      }
      tokio::time::sleep(idle_sleep).await;
    }
  }

  pub fn run_result(&self) -> DistributedDagRunResult {
    let mut state_pool = HashMap::new();
    let mut failed_nodes = HashMap::new();
    for (node_id, state) in &self.state {
      if let Some(outputs) = &state.outputs {
        state_pool.insert(node_id.clone(), outputs.clone());
      }
      if state.status == DistributedNodeStatus::Failed {
        failed_nodes.insert(
          node_id.clone(),
          state
            .last_error
            .clone()
            .unwrap_or_else(|| "distributed node failed".to_string()),
        );
      }
    }
    DistributedDagRunResult {
      run_id: self.run_id,
      succeeded: failed_nodes.is_empty()
        && self
          .state
          .values()
          .all(|state| state.status == DistributedNodeStatus::Succeeded),
      state_pool,
      failed_nodes,
    }
  }

  pub fn is_terminal(&self) -> bool {
    self.state.values().all(|state| {
      matches!(
        state.status,
        DistributedNodeStatus::Succeeded | DistributedNodeStatus::Failed
      )
    })
  }

  fn is_ready(&self, node_id: &str) -> bool {
    let Some(node) = self.nodes.get(node_id) else {
      return false;
    };
    let Some(state) = self.state.get(node_id) else {
      return false;
    };
    state.status == DistributedNodeStatus::Pending
      && node.dependencies.iter().all(|dep| {
        self
          .state
          .get(dep)
          .map(|state| state.status == DistributedNodeStatus::Succeeded)
          .unwrap_or(false)
      })
  }

  async fn dispatch_node(&mut self, node_id: &str) -> Result<(), YanshiError> {
    let node = self
      .nodes
      .get(node_id)
      .ok_or_else(|| YanshiError::FlowDefinitionError {
        message: format!("node '{}' not found", node_id),
      })?;
    let inputs = self.gather_inputs(node)?;
    let parameters = node
      .parameters
      .iter()
      .map(|(key, value)| {
        let value = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        (key.clone(), value)
      })
      .collect::<HashMap<_, _>>();
    let payload = NodeExecutionPayload::new(&node.id, &node.node_type, parameters, inputs);
    let attempt = self
      .state
      .get(node_id)
      .map(|state| state.attempt)
      .unwrap_or_default();
    let task = WorkerTask::with_attempt(
      self.run_id,
      node.id.clone(),
      attempt,
      serde_json::to_value(payload)?,
    );
    self
      .control
      .schedule_task(task.clone())
      .await
      .map_err(agent_error_from_scheduler)?;
    if let Some(state) = self.state.get_mut(node_id) {
      state.status = DistributedNodeStatus::Running;
      state.task_id = Some(task.task_id);
      state.updated_at = Utc::now();
    }
    Ok(())
  }

  async fn reconcile_failed_assignments(&mut self) -> Result<(), YanshiError> {
    let Some(snapshot) = self.control.run_snapshot(self.run_id).await else {
      return Ok(());
    };
    if snapshot.status != super::RunControlStatus::Failed {
      return Ok(());
    }
    let failed_nodes = snapshot
      .failures
      .iter()
      .map(|(node_id, failure)| (node_id.clone(), failure.clone()))
      .collect::<Vec<_>>();
    for (node_id, failure) in failed_nodes {
      let Some(state) = self.state.get_mut(&node_id) else {
        continue;
      };
      if state.status != DistributedNodeStatus::Running {
        continue;
      }
      if failure.retryable && state.attempt + 1 < self.max_attempts {
        state.attempt = failure.attempt + 1;
        state.status = DistributedNodeStatus::Pending;
        state.retryable = true;
        state.last_error = Some(failure.error);
        state.task_id = None;
      } else {
        state.status = DistributedNodeStatus::Failed;
        state.last_error = Some(failure.error);
        state.task_id = None;
      }
      state.updated_at = Utc::now();
    }
    Ok(())
  }

  fn gather_inputs(
    &self,
    node: &NodeDefinitionV2,
  ) -> Result<HashMap<String, FlowValue>, YanshiError> {
    let mut inputs = HashMap::new();
    for (input_name, mapping) in &node.input_mapping {
      let Some((source_node, output_key)) = parse_input_mapping(mapping) else {
        return Err(YanshiError::FlowDefinitionError {
          message: format!(
            "{}.input_mapping.{} uses unsupported mapping expression '{}'",
            node.id, input_name, mapping
          ),
        });
      };
      let Some(source_state) = self.state.get(&source_node) else {
        return Err(YanshiError::DependencyNotMet {
          node_id: node.id.clone(),
          dependency_id: source_node,
        });
      };
      let Some(outputs) = source_state.outputs.as_ref() else {
        return Err(YanshiError::DependencyNotMet {
          node_id: node.id.clone(),
          dependency_id: source_node,
        });
      };
      let Some(value) = outputs.get(&output_key) else {
        return Err(YanshiError::NodeInputError {
          message: format!(
            "Output '{}' not found in source node '{}'",
            output_key, source_node
          ),
        });
      };
      inputs.insert(input_name.clone(), value.clone());
    }
    for (key, value) in &node.parameters {
      let json_value = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
      inputs.insert(key.clone(), FlowValue::Json(json_value));
    }
    Ok(inputs)
  }
}

fn parse_input_mapping(mapping: &str) -> Option<(String, String)> {
  let path = mapping
    .trim()
    .trim_start_matches("{{")
    .trim_end_matches("}}")
    .trim();
  let parts = path.split('.').collect::<Vec<_>>();
  if parts.len() == 4 && parts[0] == "nodes" && parts[2] == "outputs" {
    Some((parts[1].to_string(), parts[3].to_string()))
  } else {
    None
  }
}

fn topological_order(nodes: &[NodeDefinitionV2]) -> Result<Vec<String>, YanshiError> {
  let ids = nodes
    .iter()
    .map(|node| node.id.clone())
    .collect::<HashSet<_>>();
  let mut in_degree = nodes
    .iter()
    .map(|node| (node.id.clone(), 0usize))
    .collect::<HashMap<_, _>>();
  let mut edges = nodes
    .iter()
    .map(|node| (node.id.clone(), Vec::<String>::new()))
    .collect::<HashMap<_, _>>();
  for node in nodes {
    for dep in &node.dependencies {
      if !ids.contains(dep) {
        return Err(YanshiError::FlowDefinitionError {
          message: format!("Node '{}' has an invalid dependency: '{}'", node.id, dep),
        });
      }
      *in_degree.entry(node.id.clone()).or_default() += 1;
      edges.entry(dep.clone()).or_default().push(node.id.clone());
    }
  }

  let mut ready = in_degree
    .iter()
    .filter(|(_, degree)| **degree == 0)
    .map(|(node_id, _)| node_id.clone())
    .collect::<Vec<_>>();
  let mut order = Vec::new();
  while let Some(node_id) = ready.pop() {
    order.push(node_id.clone());
    if let Some(children) = edges.get(&node_id) {
      for child in children {
        if let Some(degree) = in_degree.get_mut(child) {
          *degree = degree.saturating_sub(1);
          if *degree == 0 {
            ready.push(child.clone());
          }
        }
      }
    }
  }
  if order.len() != nodes.len() {
    return Err(YanshiError::CircularFlow);
  }
  Ok(order)
}

fn flow_outputs_from_json(
  value: serde_json::Value,
) -> Result<HashMap<String, FlowValue>, YanshiError> {
  let serde_json::Value::Object(map) = value else {
    return Err(YanshiError::FlowExecutionFailed {
      message: "worker output must be a JSON object".to_string(),
    });
  };
  map
    .into_iter()
    .map(|(key, value)| {
      serde_json::from_value::<FlowValue>(value.clone())
        .or(Ok(FlowValue::Json(value)))
        .map(|value| (key, value))
    })
    .collect()
}

fn agent_error_from_scheduler(error: SchedulerError) -> YanshiError {
  YanshiError::FlowExecutionFailed {
    message: error.to_string(),
  }
}

/// Helper used by tests and smoke scripts to make large mock DAGs.
pub fn mock_node(
  id: impl Into<String>,
  dependencies: Vec<String>,
  value: serde_json::Value,
) -> NodeDefinitionV2 {
  let id = id.into();
  NodeDefinitionV2 {
    id,
    node_type: "mock".to_string(),
    dependencies,
    input_mapping: HashMap::new(),
    run_if: None,
    timeout_ms: None,
    max_retries: None,
    parameters: HashMap::from([(
      "value".to_string(),
      serde_yaml::to_value(value).unwrap_or(serde_yaml::Value::Null),
    )]),
  }
}

pub fn mock_flow(name: impl Into<String>, nodes: Vec<NodeDefinitionV2>) -> FlowDefinitionV2 {
  FlowDefinitionV2 {
    name: name.into(),
    inputs: HashMap::new(),
    nodes,
    cost_limit_usd: None,
  }
}
