//! Distributed scheduler protocol boundaries.
//!
//! The first distributed milestone keeps transport concerns out of the
//! control plane. [`WorkerProtocol`] defines the semantics the server needs:
//! enqueue work, let workers claim work, accept results, and track heartbeats.
//! A later adapter can expose the same contract over gRPC, NATS, or Redis
//! Streams without changing run routes.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use yanshi_tracing::{OtelAttribute, OtelSpan, OtelSpanEvent, OtelSpanKind, OtelStatus};

pub mod admission;
pub mod distributed;
pub mod grpc;
pub mod jwt;

// The worker protocol contract + wire types + gRPC client moved to
// `yanshi-worker-proto` in P-A2.3 (burning the `worker -> server` edge);
// re-exported under their original `scheduler::*` paths so the control plane,
// the gRPC server side, and the existing tests are unchanged.
pub use admission::{
  AdmissionConfigError, AdmissionError, AuthenticatedControlPlane, ControlError,
  WorkerAdmissionPolicy, WorkerCredential,
};
pub use distributed::{DistributedDagRunResult, DistributedDagScheduler, DistributedNodeStatus};
pub use grpc::{AuthenticatedGrpcWorkerService, GrpcWorkerService, WorkerControlServer};
pub use jwt::{JwtPolicy, JwtVerificationKey, JwtVerifyError, WorkerJwtClaims, verify_worker_jwt};
pub use yanshi_worker_proto::{
  Certificate, ClaimHints, ClientTlsConfig, GrpcWorkerProtocol, Identity, InMemoryWorkerProtocol,
  NodeExecutionPayload, SELECTED_TRANSPORT, SchedulerError, StitchedWorkerTraceEvent,
  WorkerCapabilities, WorkerHeartbeat, WorkerId, WorkerProtocol, WorkerTask, WorkerTaskResult,
  WorkerTraceEvent, WorkerTransport, extract_traceparent_from_grpc_request,
  inject_traceparent_into_grpc_request, run_in_traceparent_scope,
};

/// Lightweight control-plane façade over a [`WorkerProtocol`].
///
/// This is the first server-side scheduling layer: it dispatches tasks into
/// the protocol, records which worker claimed each task, aggregates terminal
/// results per run, and keeps worker trace fragments available for later DB /
/// OTel persistence wiring.
#[derive(Debug, Clone)]
pub struct WorkerControlPlane<P> {
  protocol: P,
  state: Arc<Mutex<ControlPlaneState>>,
}

#[derive(Debug, Default)]
struct ControlPlaneState {
  runs: HashMap<Uuid, RunControlSnapshot>,
  assignments: HashMap<Uuid, WorkerAssignment>,
  heartbeats: HashMap<WorkerId, WorkerHeartbeat>,
}

impl<P> WorkerControlPlane<P>
where
  P: WorkerProtocol,
{
  pub fn new(protocol: P) -> Self {
    Self {
      protocol,
      state: Arc::new(Mutex::new(ControlPlaneState::default())),
    }
  }

  /// Queue a task and update the run-level control-plane counters.
  pub async fn schedule_task(&self, task: WorkerTask) -> Result<(), SchedulerError> {
    self.protocol.submit_task(task.clone()).await?;
    let mut state = self.state.lock().await;
    let run = state
      .runs
      .entry(task.run_id)
      .or_insert_with(|| RunControlSnapshot::new(task.run_id));
    run.queued_tasks += 1;
    run.status = RunControlStatus::Queued;
    Ok(())
  }

  /// Claim a task for a worker and mark it running in the control plane.
  pub async fn claim_task(
    &self,
    worker_id: WorkerId,
  ) -> Result<Option<WorkerTask>, SchedulerError> {
    self
      .claim_task_with_hints(worker_id, &ClaimHints::none())
      .await
  }

  /// Capability + locality-aware claim (P10.16.2). Forwards to the
  /// protocol's `claim_task_with_hints` so capability-aware
  /// implementations (in-memory, future capability-aware gRPC
  /// adapter) can filter and re-rank the queue.
  pub async fn claim_task_with_hints(
    &self,
    worker_id: WorkerId,
    hints: &ClaimHints,
  ) -> Result<Option<WorkerTask>, SchedulerError> {
    let Some(task) = self
      .protocol
      .claim_task_with_hints(worker_id.clone(), hints)
      .await?
    else {
      return Ok(None);
    };
    let mut state = self.state.lock().await;
    state.assignments.insert(
      task.task_id,
      WorkerAssignment {
        task_id: task.task_id,
        worker_id,
        run_id: task.run_id,
        node_id: task.node_id.clone(),
        attempt: task.attempt,
      },
    );
    let run = state
      .runs
      .entry(task.run_id)
      .or_insert_with(|| RunControlSnapshot::new(task.run_id));
    run.queued_tasks = run.queued_tasks.saturating_sub(1);
    run.running_tasks += 1;
    run.status = RunControlStatus::Running;
    Ok(Some(task))
  }

  /// Submit a worker result, aggregate run counters, and append worker trace
  /// fragments to the run snapshot.
  pub async fn report_result(
    &self,
    worker_id: WorkerId,
    task_id: Uuid,
    result: WorkerTaskResult,
  ) -> Result<(), SchedulerError> {
    let assignment = {
      let state = self.state.lock().await;
      state
        .assignments
        .get(&task_id)
        .cloned()
        .ok_or(SchedulerError::TaskNotClaimed { task_id })?
    };

    self
      .protocol
      .report_result(worker_id, task_id, result.clone())
      .await?;

    let mut state = self.state.lock().await;
    state.assignments.remove(&task_id);
    let run = state
      .runs
      .entry(assignment.run_id)
      .or_insert_with(|| RunControlSnapshot::new(assignment.run_id));
    run.running_tasks = run.running_tasks.saturating_sub(1);
    let next_global_seq = run.stitched_trace_events.len() as i64;
    let stitched_at = Utc::now();
    run
      .stitched_trace_events
      .extend(
        result
          .events()
          .iter()
          .enumerate()
          .map(|(idx, event)| StitchedWorkerTraceEvent {
            global_seq: next_global_seq + idx as i64,
            task_id,
            worker_id: assignment.worker_id.clone(),
            run_id: assignment.run_id,
            node_id: assignment.node_id.clone(),
            attempt: assignment.attempt,
            local_seq: event.seq,
            kind: event.kind.clone(),
            payload: event.payload.clone(),
            ts: stitched_at,
          }),
      );
    run.trace_events.extend(result.events().iter().cloned());
    match result {
      WorkerTaskResult::Succeeded { output, .. } => {
        run.succeeded_tasks += 1;
        run.outputs.insert(assignment.node_id, output);
      }
      WorkerTaskResult::Failed {
        error, retryable, ..
      } => {
        let node_id = assignment.node_id;
        run.failed_tasks += 1;
        run.last_error = Some(error.clone());
        run.retryable_failures += usize::from(retryable);
        run.failures.insert(
          node_id.clone(),
          WorkerTaskFailure {
            task_id,
            worker_id: assignment.worker_id,
            run_id: assignment.run_id,
            node_id,
            attempt: assignment.attempt,
            error,
            retryable,
          },
        );
      }
    }
    run.status = run.derive_status();
    Ok(())
  }

  /// Record a worker heartbeat in both the protocol and control-plane state.
  pub async fn heartbeat(&self, heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError> {
    self.protocol.heartbeat(heartbeat.clone()).await?;
    self
      .state
      .lock()
      .await
      .heartbeats
      .insert(heartbeat.worker_id.clone(), heartbeat);
    Ok(())
  }

  pub async fn run_snapshot(&self, run_id: Uuid) -> Option<RunControlSnapshot> {
    self.state.lock().await.runs.get(&run_id).cloned()
  }

  /// Return the stitched cross-worker trace for one run.
  pub async fn stitched_trace(&self, run_id: Uuid) -> Vec<StitchedWorkerTraceEvent> {
    self
      .state
      .lock()
      .await
      .runs
      .get(&run_id)
      .map(|run| run.stitched_trace_events.clone())
      .unwrap_or_default()
  }

  /// Return OpenTelemetry boundary spans derived from the stitched worker
  /// trace for one run.
  pub async fn stitched_otel_spans(&self, run_id: Uuid) -> Vec<OtelSpan> {
    stitched_trace_to_otel_spans(run_id, &self.stitched_trace(run_id).await)
  }

  pub async fn worker_heartbeat(&self, worker_id: &WorkerId) -> Option<WorkerHeartbeat> {
    self.state.lock().await.heartbeats.get(worker_id).cloned()
  }

  pub async fn assignments_for_run(&self, run_id: Uuid) -> Vec<WorkerAssignment> {
    self
      .state
      .lock()
      .await
      .assignments
      .values()
      .filter(|assignment| assignment.run_id == run_id)
      .cloned()
      .collect()
  }

  /// Drop the run's snapshot (outputs, trace fragments) and assignments
  /// once the run is over; they were kept for the life of the process.
  pub async fn forget_run(&self, run_id: Uuid) {
    let mut state = self.state.lock().await;
    state.runs.remove(&run_id);
    state
      .assignments
      .retain(|_, assignment| assignment.run_id != run_id);
  }

  /// The protocol this control plane dispatches through.
  pub fn protocol(&self) -> &P {
    &self.protocol
  }

  pub async fn forget_assignment(&self, task_id: Uuid) -> Option<WorkerAssignment> {
    let mut state = self.state.lock().await;
    let assignment = state.assignments.remove(&task_id)?;
    if let Some(run) = state.runs.get_mut(&assignment.run_id) {
      run.running_tasks = run.running_tasks.saturating_sub(1);
    }
    Some(assignment)
  }
}

#[async_trait]
impl<P> WorkerProtocol for WorkerControlPlane<P>
where
  P: WorkerProtocol + Clone,
{
  async fn submit_task(&self, task: WorkerTask) -> Result<(), SchedulerError> {
    WorkerControlPlane::schedule_task(self, task).await
  }

  async fn claim_task(&self, worker_id: WorkerId) -> Result<Option<WorkerTask>, SchedulerError> {
    WorkerControlPlane::claim_task(self, worker_id).await
  }

  async fn report_result(
    &self,
    worker_id: WorkerId,
    task_id: Uuid,
    result: WorkerTaskResult,
  ) -> Result<(), SchedulerError> {
    WorkerControlPlane::report_result(self, worker_id, task_id, result).await
  }

  async fn heartbeat(&self, heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError> {
    WorkerControlPlane::heartbeat(self, heartbeat).await
  }
}

/// Worker assignment tracked by the control plane after a claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAssignment {
  pub task_id: Uuid,
  pub worker_id: WorkerId,
  pub run_id: Uuid,
  pub node_id: String,
  pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTaskFailure {
  pub task_id: Uuid,
  pub worker_id: WorkerId,
  pub run_id: Uuid,
  pub node_id: String,
  pub attempt: u32,
  pub error: String,
  pub retryable: bool,
}

/// Run status derived from distributed task counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunControlStatus {
  Queued,
  Running,
  Succeeded,
  Failed,
}

/// In-memory snapshot of one distributed run from the control-plane view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunControlSnapshot {
  pub run_id: Uuid,
  pub status: RunControlStatus,
  pub queued_tasks: usize,
  pub running_tasks: usize,
  pub succeeded_tasks: usize,
  pub failed_tasks: usize,
  pub retryable_failures: usize,
  pub last_error: Option<String>,
  pub outputs: HashMap<String, serde_json::Value>,
  pub failures: HashMap<String, WorkerTaskFailure>,
  pub trace_events: Vec<WorkerTraceEvent>,
  pub stitched_trace_events: Vec<StitchedWorkerTraceEvent>,
}

impl RunControlSnapshot {
  fn new(run_id: Uuid) -> Self {
    Self {
      run_id,
      status: RunControlStatus::Queued,
      queued_tasks: 0,
      running_tasks: 0,
      succeeded_tasks: 0,
      failed_tasks: 0,
      retryable_failures: 0,
      last_error: None,
      outputs: HashMap::new(),
      failures: HashMap::new(),
      trace_events: Vec::new(),
      stitched_trace_events: Vec::new(),
    }
  }

  fn derive_status(&self) -> RunControlStatus {
    if self.failed_tasks > 0 {
      RunControlStatus::Failed
    } else if self.running_tasks > 0 {
      RunControlStatus::Running
    } else if self.queued_tasks > 0 {
      RunControlStatus::Queued
    } else {
      RunControlStatus::Succeeded
    }
  }
}

/// Convert stitched worker trace events into OpenTelemetry boundary spans.
///
/// The output contains one distributed-run root span plus one child span per
/// task attempt. Worker-local trace fragments are preserved as span events in
/// global order.
pub fn stitched_trace_to_otel_spans(
  run_id: Uuid,
  events: &[StitchedWorkerTraceEvent],
) -> Vec<OtelSpan> {
  let trace_id = hex_hash(&format!("distributed-run:{run_id}"), 16);
  let root_span_id = hex_hash(&format!("distributed-run:{run_id}:root"), 8);
  let start = events
    .first()
    .map(|event| unix_nanos(event.ts))
    .unwrap_or_default();
  let end = events
    .last()
    .map(|event| unix_nanos(event.ts))
    .unwrap_or(start);
  let mut spans = vec![OtelSpan {
    trace_id: trace_id.clone(),
    span_id: root_span_id.clone(),
    parent_span_id: None,
    name: format!("yanshi.distributed_run {run_id}"),
    kind: OtelSpanKind::Internal,
    start_time_unix_nano: start,
    end_time_unix_nano: end,
    attributes: vec![
      OtelAttribute::string("yanshi.run.id", run_id.to_string()),
      OtelAttribute::i64("yanshi.worker.event_count", events.len() as i64),
    ],
    events: Vec::new(),
    status: otel_status_for_events(events),
  }];

  let mut groups: Vec<TaskTraceGroup> = Vec::new();
  for event in events {
    if let Some(group) = groups
      .iter_mut()
      .find(|group| group.task_id == event.task_id && group.attempt == event.attempt)
    {
      group.events.push(event.clone());
    } else {
      groups.push(TaskTraceGroup {
        task_id: event.task_id,
        worker_id: event.worker_id.clone(),
        run_id: event.run_id,
        node_id: event.node_id.clone(),
        attempt: event.attempt,
        events: vec![event.clone()],
      });
    }
  }

  for group in groups {
    spans.push(group.into_otel_span(&trace_id, &root_span_id));
  }

  spans
}

#[derive(Debug)]
struct TaskTraceGroup {
  task_id: Uuid,
  worker_id: WorkerId,
  run_id: Uuid,
  node_id: String,
  attempt: u32,
  events: Vec<StitchedWorkerTraceEvent>,
}

impl TaskTraceGroup {
  fn into_otel_span(self, trace_id: &str, parent_span_id: &str) -> OtelSpan {
    let start = self
      .events
      .first()
      .map(|event| unix_nanos(event.ts))
      .unwrap_or_default();
    let end = self
      .events
      .last()
      .map(|event| unix_nanos(event.ts))
      .unwrap_or(start);
    let span_events = self
      .events
      .iter()
      .map(|event| OtelSpanEvent {
        name: event.kind.clone(),
        time_unix_nano: unix_nanos(event.ts),
        attributes: vec![
          OtelAttribute::i64("yanshi.worker.global_seq", event.global_seq),
          OtelAttribute::i64("yanshi.worker.local_seq", event.local_seq),
          OtelAttribute::string("yanshi.worker.payload", event.payload.to_string()),
        ],
      })
      .collect();

    OtelSpan {
      trace_id: trace_id.to_string(),
      span_id: hex_hash(
        &format!("{}:{}:{}", self.task_id, self.worker_id.0, self.attempt),
        8,
      ),
      parent_span_id: Some(parent_span_id.to_string()),
      name: format!("yanshi.worker_task {}", self.node_id),
      kind: OtelSpanKind::Internal,
      start_time_unix_nano: start,
      end_time_unix_nano: end,
      attributes: vec![
        OtelAttribute::string("yanshi.run.id", self.run_id.to_string()),
        OtelAttribute::string("yanshi.worker.id", self.worker_id.0),
        OtelAttribute::string("yanshi.task.id", self.task_id.to_string()),
        OtelAttribute::string("yanshi.node.id", self.node_id),
        OtelAttribute::i64("yanshi.task.attempt", i64::from(self.attempt)),
      ],
      events: span_events,
      status: otel_status_for_events(&self.events),
    }
  }
}

fn otel_status_for_events(events: &[StitchedWorkerTraceEvent]) -> OtelStatus {
  if let Some(event) = events.iter().find(|event| {
    let kind = event.kind.to_ascii_lowercase();
    kind.contains("failed") || kind.contains("error")
  }) {
    OtelStatus::error(event.kind.clone())
  } else {
    OtelStatus::ok()
  }
}

fn unix_nanos(time: DateTime<Utc>) -> u64 {
  time.timestamp_nanos_opt().unwrap_or_default() as u64
}

fn hex_hash(input: &str, bytes: usize) -> String {
  let mut hash = 0xcbf29ce484222325u64;
  for byte in input.as_bytes() {
    hash ^= u64::from(*byte);
    hash = hash.wrapping_mul(0x100000001b3);
  }

  let mut out = format!("{hash:016x}");
  let required_len = bytes * 2;
  while out.len() < required_len {
    hash ^= hash.rotate_left(13);
    hash = hash.wrapping_mul(0x100000001b3);
    out.push_str(&format!("{hash:016x}"));
  }
  out.truncate(required_len);
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn selected_transport_is_grpc() {
    assert_eq!(SELECTED_TRANSPORT.as_str(), "grpc");
  }

  #[test]
  fn worker_id_rejects_blank_values() {
    assert!(matches!(
      WorkerId::new("  "),
      Err(SchedulerError::InvalidWorkerId)
    ));
  }

  #[tokio::test]
  async fn in_memory_protocol_claims_tasks_fifo() {
    let protocol = InMemoryWorkerProtocol::new();
    let worker = WorkerId::new("worker-a").expect("valid worker");
    let run_id = Uuid::new_v4();
    let first = WorkerTask::new(run_id, "node_a", serde_json::json!({"n": 1}));
    let second = WorkerTask::new(run_id, "node_b", serde_json::json!({"n": 2}));

    protocol
      .submit_task(first.clone())
      .await
      .expect("submit first");
    protocol
      .submit_task(second.clone())
      .await
      .expect("submit second");

    assert_eq!(
      protocol
        .claim_task(worker.clone())
        .await
        .expect("claim first"),
      Some(first)
    );
    assert_eq!(
      protocol.claim_task(worker).await.expect("claim second"),
      Some(second)
    );
  }

  #[tokio::test]
  async fn in_memory_protocol_records_results_for_claiming_worker() {
    let protocol = InMemoryWorkerProtocol::new();
    let worker = WorkerId::new("worker-a").expect("valid worker");
    let task = WorkerTask::new(
      Uuid::new_v4(),
      "node_a",
      serde_json::json!({"input": "hello"}),
    );
    let task_id = task.task_id;
    protocol.submit_task(task).await.expect("submit");
    protocol
      .claim_task(worker.clone())
      .await
      .expect("claim")
      .expect("task");

    let result = WorkerTaskResult::Succeeded {
      output: serde_json::json!({"ok": true}),
      events: vec![WorkerTraceEvent {
        seq: 0,
        kind: "node_completed".into(),
        payload: serde_json::json!({"worker": "worker-a"}),
      }],
    };
    protocol
      .report_result(worker.clone(), task_id, result.clone())
      .await
      .expect("report");

    assert_eq!(protocol.completed_result(task_id).await, Some(result));
    assert_eq!(protocol.completed_by(task_id).await, Some(worker));
  }

  #[tokio::test]
  async fn in_memory_protocol_rejects_result_from_wrong_worker() {
    let protocol = InMemoryWorkerProtocol::new();
    let worker_a = WorkerId::new("worker-a").expect("valid worker");
    let worker_b = WorkerId::new("worker-b").expect("valid worker");
    let task = WorkerTask::new(Uuid::new_v4(), "node_a", serde_json::json!({}));
    let task_id = task.task_id;
    protocol.submit_task(task).await.expect("submit");
    protocol
      .claim_task(worker_a)
      .await
      .expect("claim")
      .expect("task");

    let err = protocol
      .report_result(
        worker_b,
        task_id,
        WorkerTaskResult::Failed {
          error: "boom".into(),
          retryable: true,
          events: Vec::new(),
        },
      )
      .await
      .expect_err("wrong worker must fail");

    assert!(matches!(err, SchedulerError::WorkerMismatch { .. }));
  }

  #[tokio::test]
  async fn in_memory_protocol_records_heartbeats() {
    let protocol = InMemoryWorkerProtocol::new();
    let worker = WorkerId::new("worker-a").expect("valid worker");
    let heartbeat = WorkerHeartbeat::now(worker.clone(), None, 4);

    protocol
      .heartbeat(heartbeat.clone())
      .await
      .expect("heartbeat");

    assert_eq!(protocol.last_heartbeat(&worker).await, Some(heartbeat));
  }

  #[tokio::test]
  async fn control_plane_schedules_claims_and_tracks_running_state() {
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let run_id = Uuid::new_v4();
    let worker = WorkerId::new("worker-a").expect("valid worker");
    let task = WorkerTask::new(run_id, "node_a", serde_json::json!({"input": 1}));

    control.schedule_task(task.clone()).await.expect("schedule");
    let queued = control.run_snapshot(run_id).await.expect("run snapshot");
    assert_eq!(queued.status, RunControlStatus::Queued);
    assert_eq!(queued.queued_tasks, 1);

    let claimed = control
      .claim_task(worker)
      .await
      .expect("claim")
      .expect("task");
    assert_eq!(claimed.task_id, task.task_id);

    let running = control.run_snapshot(run_id).await.expect("run snapshot");
    assert_eq!(running.status, RunControlStatus::Running);
    assert_eq!(running.queued_tasks, 0);
    assert_eq!(running.running_tasks, 1);
  }

  #[tokio::test]
  async fn control_plane_aggregates_success_outputs_and_trace() {
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let run_id = Uuid::new_v4();
    let worker = WorkerId::new("worker-a").expect("valid worker");
    let task = WorkerTask::new(run_id, "node_a", serde_json::json!({"input": 1}));
    let task_id = task.task_id;

    control.schedule_task(task).await.expect("schedule");
    control
      .claim_task(worker.clone())
      .await
      .expect("claim")
      .expect("task");
    control
      .report_result(
        worker,
        task_id,
        WorkerTaskResult::Succeeded {
          output: serde_json::json!({"answer": 42}),
          events: vec![WorkerTraceEvent {
            seq: 7,
            kind: "node_completed".into(),
            payload: serde_json::json!({"worker": "worker-a"}),
          }],
        },
      )
      .await
      .expect("report");

    let snapshot = control.run_snapshot(run_id).await.expect("run snapshot");
    assert_eq!(snapshot.status, RunControlStatus::Succeeded);
    assert_eq!(snapshot.running_tasks, 0);
    assert_eq!(snapshot.succeeded_tasks, 1);
    assert_eq!(
      snapshot.outputs.get("node_a"),
      Some(&serde_json::json!({"answer": 42}))
    );
    assert_eq!(snapshot.trace_events.len(), 1);
    assert_eq!(snapshot.trace_events[0].kind, "node_completed");
    assert_eq!(snapshot.stitched_trace_events.len(), 1);
    assert_eq!(snapshot.stitched_trace_events[0].global_seq, 0);
    assert_eq!(snapshot.stitched_trace_events[0].local_seq, 7);
    assert_eq!(snapshot.stitched_trace_events[0].task_id, task_id);
    assert_eq!(snapshot.stitched_trace_events[0].worker_id.0, "worker-a");
    assert_eq!(snapshot.stitched_trace_events[0].node_id, "node_a");

    let stitched = control.stitched_trace(run_id).await;
    assert_eq!(stitched, snapshot.stitched_trace_events);
    let spans = control.stitched_otel_spans(run_id).await;
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].parent_span_id, None);
    assert_eq!(
      spans[0].attributes[1],
      OtelAttribute::i64("yanshi.worker.event_count", 1)
    );
    assert_eq!(spans[1].parent_span_id, Some(spans[0].span_id.clone()));
    assert_eq!(spans[1].name, "yanshi.worker_task node_a");
    assert_eq!(spans[1].events.len(), 1);
    assert_eq!(spans[1].events[0].name, "node_completed");
  }

  #[tokio::test]
  async fn control_plane_aggregates_failure_state() {
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let run_id = Uuid::new_v4();
    let worker = WorkerId::new("worker-a").expect("valid worker");
    let task = WorkerTask::new(run_id, "node_a", serde_json::json!({}));
    let task_id = task.task_id;

    control.schedule_task(task).await.expect("schedule");
    control
      .claim_task(worker.clone())
      .await
      .expect("claim")
      .expect("task");
    control
      .report_result(
        worker,
        task_id,
        WorkerTaskResult::Failed {
          error: "node failed".into(),
          retryable: true,
          events: vec![WorkerTraceEvent {
            seq: 2,
            kind: "node_failed".into(),
            payload: serde_json::json!({"retryable": true}),
          }],
        },
      )
      .await
      .expect("report");

    let snapshot = control.run_snapshot(run_id).await.expect("run snapshot");
    assert_eq!(snapshot.status, RunControlStatus::Failed);
    assert_eq!(snapshot.failed_tasks, 1);
    assert_eq!(snapshot.retryable_failures, 1);
    assert_eq!(snapshot.last_error.as_deref(), Some("node failed"));
    assert_eq!(snapshot.trace_events[0].kind, "node_failed");
  }

  #[tokio::test]
  async fn control_plane_rejects_wrong_worker_without_mutating_state() {
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let run_id = Uuid::new_v4();
    let worker_a = WorkerId::new("worker-a").expect("valid worker");
    let worker_b = WorkerId::new("worker-b").expect("valid worker");
    let task = WorkerTask::new(run_id, "node_a", serde_json::json!({}));
    let task_id = task.task_id;

    control.schedule_task(task).await.expect("schedule");
    control
      .claim_task(worker_a)
      .await
      .expect("claim")
      .expect("task");

    let err = control
      .report_result(
        worker_b,
        task_id,
        WorkerTaskResult::Succeeded {
          output: serde_json::json!({}),
          events: Vec::new(),
        },
      )
      .await
      .expect_err("wrong worker must fail");

    assert!(matches!(err, SchedulerError::WorkerMismatch { .. }));
    let snapshot = control.run_snapshot(run_id).await.expect("run snapshot");
    assert_eq!(snapshot.status, RunControlStatus::Running);
    assert_eq!(snapshot.running_tasks, 1);
  }

  #[tokio::test]
  async fn control_plane_records_heartbeats() {
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let worker = WorkerId::new("worker-a").expect("valid worker");
    let heartbeat = WorkerHeartbeat::now(worker.clone(), None, 3);

    control
      .heartbeat(heartbeat.clone())
      .await
      .expect("heartbeat");

    assert_eq!(control.worker_heartbeat(&worker).await, Some(heartbeat));
  }

  // ----- P10.16.2: capability + locality hints -----

  #[test]
  fn worker_capabilities_default_accepts_everything() {
    let caps = WorkerCapabilities::default();
    assert!(caps.accepts(None));
    assert!(caps.accepts(Some("template")));
    assert!(caps.accepts(Some("llm")));
  }

  #[test]
  fn worker_capabilities_restricted_accepts_only_listed_types() {
    let caps = WorkerCapabilities::for_node_types(["template", "file"]);
    assert!(caps.accepts(Some("template")));
    assert!(caps.accepts(Some("file")));
    assert!(!caps.accepts(Some("llm")));
  }

  #[test]
  fn worker_capabilities_restricted_still_accepts_untagged_tasks() {
    // Backwards compat: tasks without a `node_type` label are
    // accepted by every worker regardless of capability set. That
    // makes the P10.16.2 upgrade additive — pre-P10.16.2 tasks
    // (which never set `node_type`) keep scheduling onto
    // capability-restricted workers.
    let caps = WorkerCapabilities::for_node_types(["template"]);
    assert!(caps.accepts(None));
  }

  #[tokio::test]
  async fn claim_task_with_hints_skips_unmatched_capability_tasks() {
    let protocol = InMemoryWorkerProtocol::new();
    let run = Uuid::new_v4();
    let llm_task = WorkerTask::new(run, "llm-a", json!({})).with_node_type("llm");
    let tpl_task = WorkerTask::new(run, "tpl-a", json!({})).with_node_type("template");
    protocol.submit_task(llm_task.clone()).await.unwrap();
    protocol.submit_task(tpl_task.clone()).await.unwrap();

    let worker = WorkerId::new("template-only").unwrap();
    let hints =
      ClaimHints::default().with_capabilities(WorkerCapabilities::for_node_types(["template"]));

    let claimed = protocol
      .claim_task_with_hints(worker.clone(), &hints)
      .await
      .unwrap()
      .expect("template task should be returned");
    assert_eq!(claimed.task_id, tpl_task.task_id);

    // Re-claim: no more template tasks remain, so the llm-only
    // task in front of the queue is NOT returned to this worker.
    let next = protocol
      .claim_task_with_hints(worker, &hints)
      .await
      .unwrap();
    assert!(
      next.is_none(),
      "worker without llm capability must not get llm task"
    );

    // A different worker without restrictions claims it cleanly.
    let any_worker = WorkerId::new("anything-goes").unwrap();
    let any = protocol
      .claim_task_with_hints(any_worker, &ClaimHints::none())
      .await
      .unwrap()
      .expect("anything-goes worker should claim the llm task");
    assert_eq!(any.task_id, llm_task.task_id);
  }

  #[tokio::test]
  async fn claim_task_with_hints_prefers_locality_match() {
    let protocol = InMemoryWorkerProtocol::new();
    let run_a = Uuid::new_v4();
    let run_b = Uuid::new_v4();
    // Submit in order: run_b first (would be FIFO without hints),
    // then run_a.
    let task_b = WorkerTask::new(run_b, "n", json!({}));
    let task_a = WorkerTask::new(run_a, "n", json!({}));
    protocol.submit_task(task_b.clone()).await.unwrap();
    protocol.submit_task(task_a.clone()).await.unwrap();

    let worker = WorkerId::new("local-worker").unwrap();
    let hints = ClaimHints::default().with_locality(run_a);
    let claimed = protocol
      .claim_task_with_hints(worker, &hints)
      .await
      .unwrap()
      .expect("a task should be claimed");
    assert_eq!(
      claimed.task_id, task_a.task_id,
      "locality hint should beat FIFO ordering"
    );
  }

  #[tokio::test]
  async fn claim_task_with_hints_falls_back_to_fifo_when_no_locality_match() {
    let protocol = InMemoryWorkerProtocol::new();
    let run_a = Uuid::new_v4();
    let run_b = Uuid::new_v4();
    let task_b = WorkerTask::new(run_b, "n", json!({}));
    let task_a = WorkerTask::new(run_a, "n", json!({}));
    protocol.submit_task(task_b.clone()).await.unwrap();
    protocol.submit_task(task_a.clone()).await.unwrap();

    let worker = WorkerId::new("no-locality").unwrap();
    // Locality hint points at a run that has no queued tasks; the
    // worker should fall through to FIFO order (run_b first).
    let hints = ClaimHints::default().with_locality(Uuid::new_v4());
    let claimed = protocol
      .claim_task_with_hints(worker, &hints)
      .await
      .unwrap()
      .expect("a task should be claimed");
    assert_eq!(
      claimed.task_id, task_b.task_id,
      "no matching locality should preserve FIFO"
    );
  }

  #[tokio::test]
  async fn claim_task_with_hints_remembers_last_run_as_locality() {
    let protocol = InMemoryWorkerProtocol::new();
    let run_a = Uuid::new_v4();
    let run_b = Uuid::new_v4();
    // Sequence:
    //   1. submit run_a/task_a + run_b/task_b in that order.
    //   2. claim with explicit run_a locality → returns task_a.
    //   3. submit run_a/task_a2 + run_b/task_b2.
    //   4. claim with NO explicit locality → should still return
    //      a run_a task (the cached last-claimed run) instead of
    //      the FIFO winner (task_b that was already queued, now
    //      followed by task_a2/task_b2).
    let task_a = WorkerTask::new(run_a, "n1", json!({}));
    let task_b = WorkerTask::new(run_b, "n2", json!({}));
    protocol.submit_task(task_a.clone()).await.unwrap();
    protocol.submit_task(task_b.clone()).await.unwrap();

    let worker = WorkerId::new("sticky-worker").unwrap();
    let claimed = protocol
      .claim_task_with_hints(worker.clone(), &ClaimHints::default().with_locality(run_a))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(claimed.task_id, task_a.task_id);

    let task_a2 = WorkerTask::new(run_a, "n3", json!({}));
    let task_b2 = WorkerTask::new(run_b, "n4", json!({}));
    protocol.submit_task(task_a2.clone()).await.unwrap();
    protocol.submit_task(task_b2.clone()).await.unwrap();

    let claimed = protocol
      .claim_task_with_hints(worker, &ClaimHints::none())
      .await
      .unwrap()
      .unwrap();
    assert_eq!(
      claimed.task_id, task_a2.task_id,
      "cached last-claimed run should bias the second claim"
    );
  }

  #[tokio::test]
  async fn claim_task_with_hints_combines_capability_and_locality() {
    // Queue:   run_a/llm    run_b/template    run_a/template
    // Worker:  template-only, locality = run_a
    // Expect:  run_a/template (capability OK + locality match).
    let protocol = InMemoryWorkerProtocol::new();
    let run_a = Uuid::new_v4();
    let run_b = Uuid::new_v4();
    let t1 = WorkerTask::new(run_a, "x", json!({})).with_node_type("llm");
    let t2 = WorkerTask::new(run_b, "y", json!({})).with_node_type("template");
    let t3 = WorkerTask::new(run_a, "z", json!({})).with_node_type("template");
    protocol.submit_task(t1).await.unwrap();
    protocol.submit_task(t2.clone()).await.unwrap();
    protocol.submit_task(t3.clone()).await.unwrap();

    let worker = WorkerId::new("template-and-local").unwrap();
    let hints = ClaimHints::default()
      .with_capabilities(WorkerCapabilities::for_node_types(["template"]))
      .with_locality(run_a);
    let claimed = protocol
      .claim_task_with_hints(worker, &hints)
      .await
      .unwrap()
      .unwrap();
    assert_eq!(
      claimed.task_id, t3.task_id,
      "should pick the run_a/template task over the run_b/template task"
    );
  }

  #[tokio::test]
  async fn control_plane_claim_task_with_hints_updates_run_snapshot() {
    // End-to-end: the WorkerControlPlane wrapper around the
    // capability-aware protocol still updates per-run state.
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let run = Uuid::new_v4();
    let task = WorkerTask::new(run, "n", json!({})).with_node_type("template");
    control.schedule_task(task.clone()).await.unwrap();

    let worker = WorkerId::new("w").unwrap();
    let hints =
      ClaimHints::default().with_capabilities(WorkerCapabilities::for_node_types(["template"]));
    let claimed = control
      .claim_task_with_hints(worker, &hints)
      .await
      .unwrap()
      .expect("task claimed");
    assert_eq!(claimed.task_id, task.task_id);
    let snapshot = control.run_snapshot(run).await.expect("snapshot");
    assert_eq!(snapshot.running_tasks, 1);
    assert_eq!(snapshot.status, RunControlStatus::Running);
  }
}
