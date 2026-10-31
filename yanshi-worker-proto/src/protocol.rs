//! Worker control-plane contract types + the in-memory protocol.
//!
//! Moved from `yanshi-server::scheduler` in P-A2.3. The server keeps the
//! control plane (`WorkerControlPlane`) and the gRPC server; this is the
//! transport-neutral contract the worker depends on.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;
use yanshi_value::FlowValue;

// ── Node execution payload (the worker's node dispatch input) ───────────
/// Portable node execution payload consumed by `yanshi-worker`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeExecutionPayload {
  pub node_id: String,
  pub node_type: String,
  #[serde(default)]
  pub parameters: HashMap<String, serde_json::Value>,
  #[serde(default)]
  pub inputs: HashMap<String, FlowValue>,
}

impl NodeExecutionPayload {
  pub fn new(
    node_id: impl Into<String>,
    node_type: impl Into<String>,
    parameters: HashMap<String, serde_json::Value>,
    inputs: HashMap<String, FlowValue>,
  ) -> Self {
    Self {
      node_id: node_id.into(),
      node_type: node_type.into(),
      parameters,
      inputs,
    }
  }
}

// ── Worker protocol contract ────────────────────────────────────────────
/// Transport selected for the v1.0-rc distributed control plane.
pub const SELECTED_TRANSPORT: WorkerTransport = WorkerTransport::Grpc;

/// Distributed worker transport options considered by the design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTransport {
  /// Primary v1.0-rc path: HTTP/2 streaming RPC via tonic.
  Grpc,
  /// Reserved for high-throughput event-bus deployments.
  Nats,
  /// Reserved for teams that already run Redis as infrastructure.
  RedisStreams,
}

impl WorkerTransport {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Grpc => "grpc",
      Self::Nats => "nats",
      Self::RedisStreams => "redis_streams",
    }
  }
}

/// Stable worker identity supplied by the worker process at startup.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkerId(pub String);

impl WorkerId {
  pub fn new(value: impl Into<String>) -> Result<Self, SchedulerError> {
    let value = value.into();
    if value.trim().is_empty() {
      return Err(SchedulerError::InvalidWorkerId);
    }
    Ok(Self(value))
  }
}

/// One schedulable unit of distributed work.
///
/// `node_type` (added in P10.16.2) is the optional capability label
/// used for worker-side filtering. When set, a worker only claims the
/// task if its [`WorkerCapabilities::node_types`] contains the label
/// (or its capability set is empty, meaning "anything"). `None`
/// preserves the pre-P10.16.2 behavior of "any worker can claim me."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTask {
  pub task_id: Uuid,
  pub run_id: Uuid,
  pub node_id: String,
  pub attempt: u32,
  pub payload: serde_json::Value,
  /// Optional capability label for capability-aware dispatch.
  /// Workers compare this against
  /// [`WorkerCapabilities::node_types`]. `None` → bypass the filter.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub node_type: Option<String>,
}

impl WorkerTask {
  pub fn new(run_id: Uuid, node_id: impl Into<String>, payload: serde_json::Value) -> Self {
    Self {
      task_id: Uuid::new_v4(),
      run_id,
      node_id: node_id.into(),
      attempt: 0,
      payload,
      node_type: None,
    }
  }

  pub fn with_attempt(
    run_id: Uuid,
    node_id: impl Into<String>,
    attempt: u32,
    payload: serde_json::Value,
  ) -> Self {
    Self {
      task_id: Uuid::new_v4(),
      run_id,
      node_id: node_id.into(),
      attempt,
      payload,
      node_type: None,
    }
  }

  /// Tag this task with a capability label for worker-side filtering.
  /// Builder style so existing call sites don't need to enumerate the
  /// optional field. Returns `self` for chaining.
  pub fn with_node_type(mut self, node_type: impl Into<String>) -> Self {
    self.node_type = Some(node_type.into());
    self
  }
}

/// Worker capability descriptor (P10.16.2).
///
/// Workers advertise which task labels they can execute. The
/// in-memory protocol uses this to skip tasks the worker can't
/// handle when scanning the queue; the gRPC adapter forwards the
/// set on `claim_task` calls (wire-extension follow-up).
///
/// An empty `node_types` vector means "this worker accepts any
/// task" — the pre-P10.16.2 default. A non-empty vector restricts
/// the worker to tasks whose `node_type` is in the set OR untagged
/// (the latter keeps the upgrade additive: legacy untagged tasks
/// continue to schedule onto capability-restricted workers).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkerCapabilities {
  /// Capability labels this worker accepts. Empty = unrestricted.
  /// Match is case-sensitive against [`WorkerTask::node_type`].
  pub node_types: Vec<String>,
}

impl WorkerCapabilities {
  /// Convenience constructor for "any task" capability.
  pub fn any() -> Self {
    Self::default()
  }

  /// Convenience constructor for a worker that handles exactly the
  /// given task labels.
  pub fn for_node_types<I, S>(node_types: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    Self {
      node_types: node_types.into_iter().map(Into::into).collect(),
    }
  }

  /// Return `true` when the worker accepts `node_type`. `None`
  /// (untagged task) is always accepted to preserve backwards
  /// compat. Empty capability set ("any task") also accepts
  /// everything.
  pub fn accepts(&self, node_type: Option<&str>) -> bool {
    if self.node_types.is_empty() {
      return true;
    }
    let Some(nt) = node_type else {
      return true;
    };
    self.node_types.iter().any(|allowed| allowed == nt)
  }
}

/// Optional hints a worker can attach to a claim call (P10.16.2).
///
/// All fields default to "no preference" so existing call sites
/// keep working unchanged. The [`WorkerProtocol`] trait default
/// implementation of [`WorkerProtocol::claim_task_with_hints`]
/// ignores hints and falls back to [`WorkerProtocol::claim_task`],
/// so protocols that don't care about hints don't need to
/// implement anything.
#[derive(Debug, Clone, Default)]
pub struct ClaimHints {
  /// What task labels this worker accepts. See
  /// [`WorkerCapabilities::accepts`].
  pub capabilities: WorkerCapabilities,
  /// Optional locality hint — a `run_id` whose tasks this worker
  /// has recently handled. The in-memory protocol uses it to
  /// prefer warm-cache tasks (same run = warm filesystem, warm
  /// model context) over cold tasks when multiple match the
  /// capability filter.
  pub locality_run_id: Option<Uuid>,
}

impl ClaimHints {
  /// Convenience: "no hints," equivalent to the pre-P10.16.2
  /// behavior.
  pub fn none() -> Self {
    Self::default()
  }

  pub fn with_capabilities(mut self, capabilities: WorkerCapabilities) -> Self {
    self.capabilities = capabilities;
    self
  }

  pub fn with_locality(mut self, run_id: Uuid) -> Self {
    self.locality_run_id = Some(run_id);
    self
  }
}

/// Task execution result sent from a worker back to the control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkerTaskResult {
  Succeeded {
    output: serde_json::Value,
    events: Vec<WorkerTraceEvent>,
  },
  Failed {
    error: String,
    retryable: bool,
    events: Vec<WorkerTraceEvent>,
  },
}

impl WorkerTaskResult {
  pub fn events(&self) -> &[WorkerTraceEvent] {
    match self {
      Self::Succeeded { events, .. } | Self::Failed { events, .. } => events,
    }
  }
}

/// Trace fragment emitted by a worker. The control plane persists these with
/// the run and later maps them into OTel spans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTraceEvent {
  pub seq: i64,
  pub kind: String,
  pub payload: serde_json::Value,
}

/// Worker trace event after the control plane assigns global ordering and
/// execution ownership metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StitchedWorkerTraceEvent {
  pub global_seq: i64,
  pub task_id: Uuid,
  pub worker_id: WorkerId,
  pub run_id: Uuid,
  pub node_id: String,
  pub attempt: u32,
  pub local_seq: i64,
  pub kind: String,
  pub payload: serde_json::Value,
  pub ts: DateTime<Utc>,
}

/// Worker heartbeat payload. `active_task` is `None` when the worker is idle.
///
/// `capabilities` (added in P10.16.2) is the per-heartbeat
/// advertisement of which task labels this worker accepts.
/// Defaults to "any task" so heartbeats from pre-P10.16.2 workers
/// keep their existing behavior. The control plane snapshots the
/// latest capabilities per worker; capability-aware dispatch reads
/// from the snapshot during `claim_task_with_hints`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHeartbeat {
  pub worker_id: WorkerId,
  pub active_task: Option<Uuid>,
  pub free_slots: u32,
  pub ts: DateTime<Utc>,
  #[serde(default, skip_serializing_if = "is_default_capabilities")]
  pub capabilities: WorkerCapabilities,
}

fn is_default_capabilities(value: &WorkerCapabilities) -> bool {
  value.node_types.is_empty()
}

impl WorkerHeartbeat {
  pub fn now(worker_id: WorkerId, active_task: Option<Uuid>, free_slots: u32) -> Self {
    Self {
      worker_id,
      active_task,
      free_slots,
      ts: Utc::now(),
      capabilities: WorkerCapabilities::default(),
    }
  }

  /// Builder-style capability advertisement. Lets workers attach
  /// their accepted node_types to the heartbeat without enumerating
  /// the (now wider) struct literal.
  pub fn with_capabilities(mut self, capabilities: WorkerCapabilities) -> Self {
    self.capabilities = capabilities;
    self
  }
}

/// Protocol boundary between the server control plane and worker processes.
#[async_trait]
pub trait WorkerProtocol: Send + Sync {
  /// Submit one task to the distributed queue.
  async fn submit_task(&self, task: WorkerTask) -> Result<(), SchedulerError>;

  /// Claim the next available task for `worker_id`.
  async fn claim_task(&self, worker_id: WorkerId) -> Result<Option<WorkerTask>, SchedulerError>;

  /// Claim the next task that matches the worker's capabilities and
  /// (optionally) locality preference (P10.16.2). Defaults to
  /// [`Self::claim_task`] when the implementation doesn't care about
  /// hints — additive on the trait so the gRPC adapter, which
  /// hasn't grown wire-level capability fields yet, doesn't need
  /// to do anything.
  async fn claim_task_with_hints(
    &self,
    worker_id: WorkerId,
    _hints: &ClaimHints,
  ) -> Result<Option<WorkerTask>, SchedulerError> {
    self.claim_task(worker_id).await
  }

  /// Report a terminal result for a task previously claimed by a worker.
  async fn report_result(
    &self,
    worker_id: WorkerId,
    task_id: Uuid,
    result: WorkerTaskResult,
  ) -> Result<(), SchedulerError>;

  /// Record liveness and current capacity for a worker.
  async fn heartbeat(&self, heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError>;
}

// ── In-memory protocol (tests + single-process execution) ───────────────
/// In-memory implementation used for unit tests and local control-plane
/// prototyping. It is intentionally single-process and not durable.
#[derive(Debug, Clone, Default)]
pub struct InMemoryWorkerProtocol {
  state: Arc<Mutex<InMemoryState>>,
}

#[derive(Debug, Default)]
struct InMemoryState {
  queued: VecDeque<WorkerTask>,
  claimed: HashMap<Uuid, ClaimedTask>,
  completed: HashMap<Uuid, CompletedTask>,
  heartbeats: HashMap<WorkerId, WorkerHeartbeat>,
  /// Locality cache (P10.16.2). Tracks the most recent `run_id`
  /// each worker successfully claimed so subsequent claims can
  /// prefer same-run tasks (warm filesystem, warm context). A
  /// single Option per worker is the v1.x foundation; a future
  /// LRU set could remember the last N runs if real workloads
  /// need broader locality.
  last_claimed_run: HashMap<WorkerId, Uuid>,
}

#[derive(Debug)]
struct ClaimedTask {
  worker_id: WorkerId,
  run_id: Uuid,
}

#[derive(Debug)]
struct CompletedTask {
  worker_id: WorkerId,
  run_id: Uuid,
  result: WorkerTaskResult,
}

impl InMemoryWorkerProtocol {
  pub fn new() -> Self {
    Self::default()
  }

  /// Snapshot the last heartbeat seen for a worker.
  pub async fn last_heartbeat(&self, worker_id: &WorkerId) -> Option<WorkerHeartbeat> {
    self.state.lock().await.heartbeats.get(worker_id).cloned()
  }

  /// Drop everything held for `run_id` once the run is over: its queued
  /// tasks (a cancelled run's tasks must not be claimed any more), its
  /// claims (a late report then gets `TaskNotClaimed`) and its results.
  /// Without this every task of every run stayed in memory for good.
  pub async fn forget_run(&self, run_id: Uuid) {
    let mut state = self.state.lock().await;
    state.queued.retain(|task| task.run_id != run_id);
    state.claimed.retain(|_, claimed| claimed.run_id != run_id);
    state
      .completed
      .retain(|_, completed| completed.run_id != run_id);
    state.last_claimed_run.retain(|_, last| *last != run_id);
  }

  /// How many tasks of `run_id` are still held (queued, claimed or
  /// completed). Test/debug support.
  pub async fn retained_tasks(&self, run_id: Uuid) -> usize {
    let state = self.state.lock().await;
    state
      .queued
      .iter()
      .filter(|task| task.run_id == run_id)
      .count()
      + state
        .claimed
        .values()
        .filter(|c| c.run_id == run_id)
        .count()
      + state
        .completed
        .values()
        .filter(|c| c.run_id == run_id)
        .count()
  }

  /// Snapshot a completed task result. This is test/debug support, not part
  /// of the distributed protocol contract.
  pub async fn completed_result(&self, task_id: Uuid) -> Option<WorkerTaskResult> {
    self
      .state
      .lock()
      .await
      .completed
      .get(&task_id)
      .map(|completed| completed.result.clone())
  }

  /// Snapshot the worker that completed a task.
  pub async fn completed_by(&self, task_id: Uuid) -> Option<WorkerId> {
    self
      .state
      .lock()
      .await
      .completed
      .get(&task_id)
      .map(|completed| completed.worker_id.clone())
  }
}

#[async_trait]
impl WorkerProtocol for InMemoryWorkerProtocol {
  async fn submit_task(&self, task: WorkerTask) -> Result<(), SchedulerError> {
    self.state.lock().await.queued.push_back(task);
    Ok(())
  }

  async fn claim_task(&self, worker_id: WorkerId) -> Result<Option<WorkerTask>, SchedulerError> {
    // Preserve the pre-P10.16.2 FIFO behavior when no hints are
    // supplied. Capability-aware dispatch goes through
    // `claim_task_with_hints` (called by `WorkerControlPlane`
    // when a worker provides capabilities + locality).
    let mut state = self.state.lock().await;
    let Some(task) = state.queued.pop_front() else {
      return Ok(None);
    };
    state
      .last_claimed_run
      .insert(worker_id.clone(), task.run_id);
    state.claimed.insert(
      task.task_id,
      ClaimedTask {
        worker_id,
        run_id: task.run_id,
      },
    );
    Ok(Some(task))
  }

  async fn claim_task_with_hints(
    &self,
    worker_id: WorkerId,
    hints: &ClaimHints,
  ) -> Result<Option<WorkerTask>, SchedulerError> {
    let mut state = self.state.lock().await;
    if state.queued.is_empty() {
      return Ok(None);
    }

    // The locality hint defaults to "the run this worker last
    // claimed from" so a worker that doesn't supply
    // `locality_run_id` still gets warm-cache continuity.
    let locality_run_id = hints
      .locality_run_id
      .or_else(|| state.last_claimed_run.get(&worker_id).copied());

    // Pass 1: same run AND capability-accepting (warmest match).
    // Pass 2: capability-accepting regardless of run.
    // Pass 3: untagged tasks (no node_type) — accepts under any
    //         capability set per the
    //         `WorkerCapabilities::accepts` contract.
    //
    // Each pass scans queued in FIFO order so ties between equally-
    // preferred tasks keep the original ordering.
    let chosen_index = (|| {
      if let Some(run) = locality_run_id
        && let Some(idx) = state.queued.iter().position(|task| {
          task.run_id == run && hints.capabilities.accepts(task.node_type.as_deref())
        })
      {
        return Some(idx);
      }
      if let Some(idx) = state
        .queued
        .iter()
        .position(|task| hints.capabilities.accepts(task.node_type.as_deref()))
      {
        return Some(idx);
      }
      None
    })();

    let Some(idx) = chosen_index else {
      return Ok(None);
    };
    // Remove from the deque without disturbing the order of the
    // remaining tasks. `idx` was returned by `state.queued.iter().position(..)`
    // a few lines above (line 970-972), so it is guaranteed to be in range
    // while still holding `state` — no other task has had a chance to mutate
    // the deque between `position` and `remove`. Q5.1.
    #[allow(
      clippy::expect_used,
      reason = "idx came from .position() on the same locked deque"
    )]
    let task = state.queued.remove(idx).expect("index from position");
    state
      .last_claimed_run
      .insert(worker_id.clone(), task.run_id);
    state.claimed.insert(
      task.task_id,
      ClaimedTask {
        worker_id,
        run_id: task.run_id,
      },
    );
    Ok(Some(task))
  }

  async fn report_result(
    &self,
    worker_id: WorkerId,
    task_id: Uuid,
    result: WorkerTaskResult,
  ) -> Result<(), SchedulerError> {
    let mut state = self.state.lock().await;
    let Some(claimed) = state.claimed.remove(&task_id) else {
      return Err(SchedulerError::TaskNotClaimed { task_id });
    };
    if claimed.worker_id != worker_id {
      state.claimed.insert(task_id, claimed);
      return Err(SchedulerError::WorkerMismatch { task_id });
    }
    state.completed.insert(
      task_id,
      CompletedTask {
        worker_id,
        run_id: claimed.run_id,
        result,
      },
    );
    Ok(())
  }

  async fn heartbeat(&self, heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError> {
    self
      .state
      .lock()
      .await
      .heartbeats
      .insert(heartbeat.worker_id.clone(), heartbeat);
    Ok(())
  }
}

/// Scheduler/protocol errors. These are intentionally transport-neutral.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SchedulerError {
  #[error("worker id must not be empty")]
  InvalidWorkerId,
  #[error("task {task_id} has not been claimed")]
  TaskNotClaimed { task_id: Uuid },
  #[error("task {task_id} was claimed by a different worker")]
  WorkerMismatch { task_id: Uuid },
  #[error("transport error: {message}")]
  Transport { message: String },
}
