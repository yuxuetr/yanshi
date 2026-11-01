//! Worker runtime for distributed Yanshi execution.
//!
//! The runtime is transport-agnostic: it drives any
//! [`WorkerProtocol`] implementation through
//! heartbeat, claim, execute, and report-result steps. The first binary uses
//! the in-memory protocol for local smoke tests; the gRPC adapter can plug in
//! behind the same API.
//!
//! ## Supported `NodeExecutionPayload` types (P2.8)
//!
//! The worker dispatches on `payload.node_type`:
//!
//! - `template` → [`yanshi_nodes::nodes::template::TemplateNode`]
//! - `file` → [`yanshi_nodes::nodes::file::FileNode`]
//! - `mock` → in-crate stub used by the scheduler smoke tests
//! - `llm` → [`yanshi_nodes_ai::nodes::llm::LlmNode`]
//! - `http` → [`yanshi_nodes::nodes::http::HttpNode`]
//! - `mcp` → [`yanshi_nodes_ai::nodes::mcp::MCPNode`]
//! - `agent` → minimal [`yanshi_agents::react::ReActAgent`] loop with an
//!   empty [`yanshi_tools::ToolRegistry`]
//!
//! Unknown node types produce a non-retryable
//! [`YanshiError::FlowDefinitionError`], so a typo in YAML cannot
//! hot-loop the pool. See `docs/DISTRIBUTED.md` for the canonical
//! contract and test references.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::time::sleep;
use yanshi_agents::react::{ReActAgent, ReActConfig};
use yanshi_core::{
  FlowValue, YanshiError,
  async_node::{AsyncNode, AsyncNodeResult},
};
use yanshi_memory::SessionMemory;
use yanshi_nodes::nodes::{file::FileNode, http::HttpNode, template::TemplateNode};
use yanshi_nodes_ai::nodes::{llm::LlmNode, mcp::MCPNode};
use yanshi_tools::ToolRegistry;
use yanshi_worker_proto::{
  ClaimHints, NodeExecutionPayload, SchedulerError, WorkerCapabilities, WorkerHeartbeat, WorkerId,
  WorkerProtocol, WorkerTask, WorkerTaskResult, WorkerTraceEvent,
};

/// Per-worker resource limits applied to every dispatched node.
///
/// **Stability:** experimental — see `docs/STABILITY.md` for the
/// distributed worker control plane row. The knobs below cover what
/// the worker can enforce in-process today; cgroup-level memory caps
/// are a documented gap on macOS (see `docs/DISTRIBUTED.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerResourceLimits {
  /// Hard wall-clock cap on a single dispatch invocation. `None` means
  /// the worker waits forever for the node to finish — only safe in
  /// tests with built-in timeouts.
  pub default_timeout: Option<Duration>,
  /// Cap on the serialized size of the success output map. When
  /// exceeded, the worker replaces the output with a small JSON marker
  /// (`{"truncated": true, "limit": N, "size": M}`) and adds a
  /// `worker.task.output_truncated` trace event.
  pub max_output_bytes: Option<usize>,
}

impl Default for WorkerResourceLimits {
  fn default() -> Self {
    Self {
      // Conservative production-leaning default. Specific deployments
      // override via `WorkerConfig::with_resource_limits`.
      default_timeout: Some(Duration::from_secs(300)),
      // 1 MiB matches the default `MAX_OUTPUT_BYTES` used by the
      // harness background-task runtime, so the two surfaces feel
      // consistent.
      max_output_bytes: Some(1024 * 1024),
    }
  }
}

impl WorkerResourceLimits {
  /// "No limits" preset — useful for the existing scheduler smokes
  /// that need to keep running 100-node fan-outs without per-task
  /// envelopes.
  pub fn unlimited() -> Self {
    Self {
      default_timeout: None,
      max_output_bytes: None,
    }
  }
}

/// Worker process configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerConfig {
  pub worker_id: WorkerId,
  pub control_plane: String,
  pub free_slots: u32,
  pub poll_interval: Duration,
  pub heartbeat_interval: Duration,
  pub resource_limits: WorkerResourceLimits,
  /// Capabilities advertised in every claim + heartbeat
  /// (P10.16.2-FU1). Empty = "any task" (pre-FU1 default).
  /// A worker that knows it only handles `template` / `file`
  /// nodes can set this to skip the server-side filter cost on
  /// unmatched tasks.
  pub capabilities: WorkerCapabilities,
  /// Q3.1.3: knobs for the `run_forever` reconnect backoff. The
  /// runtime treats `SchedulerError::Transport` as recoverable —
  /// instead of aborting the process on the first control-plane
  /// blip (which forced operators to wrap the binary in an external
  /// supervisor), it sleeps with jittered exponential backoff and
  /// retries. `max_reconnect_attempts = None` means "retry forever";
  /// any `Some(n)` caps total consecutive transport errors before
  /// the loop exits with `WorkerError::Scheduler(Transport)` —
  /// useful for fail-fast deployment smoke tests.
  pub reconnect_backoff: ReconnectBackoff,
}

/// Q3.1.3: bounded exponential backoff config for `run_forever`'s
/// recovery from `SchedulerError::Transport`. Defaults yield a curve
/// of 100ms → 200ms → 400ms → ... capped at 30s, with ±25% jitter to
/// avoid every worker in a fleet retrying in lockstep after a shared
/// control-plane restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectBackoff {
  /// Smallest sleep after the first transport error.
  pub initial: Duration,
  /// Hard cap on the per-attempt sleep.
  pub max: Duration,
  /// Multiplier applied to the previous sleep (clamped at `max`).
  /// Stored as percent (`200` = 2×) so the struct stays `Eq`.
  pub multiplier_percent: u32,
  /// `None` = retry forever; `Some(n)` = give up after `n`
  /// consecutive transport errors and let the caller decide.
  pub max_attempts: Option<u32>,
  /// Whether to randomize each sleep by ±25%. Almost always `true`
  /// in production; the default `false` is reserved for the
  /// deterministic regression tests in `tests/`.
  pub jitter: bool,
}

impl Default for ReconnectBackoff {
  fn default() -> Self {
    Self {
      initial: Duration::from_millis(100),
      max: Duration::from_secs(30),
      multiplier_percent: 200,
      max_attempts: None,
      jitter: true,
    }
  }
}

impl ReconnectBackoff {
  /// Deterministic variant for tests — disables jitter so the
  /// sequence is `initial, initial*mult, ..., max, max, ...`.
  pub fn deterministic(initial: Duration, max: Duration) -> Self {
    Self {
      initial,
      max,
      multiplier_percent: 200,
      max_attempts: None,
      jitter: false,
    }
  }

  /// Compute the next sleep given the previous one (or `None` on the
  /// first failure). Pure function — testable without an Rng.
  pub fn next_delay(&self, prev: Option<Duration>) -> Duration {
    let base_ms = match prev {
      None => self.initial.as_millis() as u64,
      Some(p) => {
        let prev_ms = p.as_millis() as u64;
        let mult = self.multiplier_percent as u64;
        prev_ms.saturating_mul(mult).saturating_div(100)
      }
    };
    let capped = base_ms.min(self.max.as_millis() as u64);
    if !self.jitter {
      return Duration::from_millis(capped);
    }
    // ±25% jitter. We use `rand::random::<u32>()` mod the window
    // rather than pulling a full Rng — backoff jitter doesn't need
    // a crypto-grade source.
    let window = capped.saturating_div(2); // half-width = 25% × 2
    let half_window = window.saturating_div(2);
    let r = rand::random::<u64>() % window.max(1);
    let jittered = capped.saturating_add(r).saturating_sub(half_window);
    Duration::from_millis(jittered)
  }
}

impl WorkerConfig {
  pub fn new(worker_id: WorkerId, control_plane: impl Into<String>) -> Self {
    Self {
      worker_id,
      control_plane: control_plane.into(),
      free_slots: 1,
      poll_interval: Duration::from_millis(250),
      heartbeat_interval: Duration::from_secs(5),
      // Existing scheduler smokes pre-date P5.6 and don't expect
      // timeouts — keep the default unlimited so they continue to
      // pass unchanged. Production callers should override via
      // `with_resource_limits` with the prod-leaning preset.
      resource_limits: WorkerResourceLimits::unlimited(),
      capabilities: WorkerCapabilities::default(),
      reconnect_backoff: ReconnectBackoff::default(),
    }
  }

  pub fn with_resource_limits(mut self, limits: WorkerResourceLimits) -> Self {
    self.resource_limits = limits;
    self
  }

  /// Override the `run_forever` reconnect backoff curve.
  pub fn with_reconnect_backoff(mut self, backoff: ReconnectBackoff) -> Self {
    self.reconnect_backoff = backoff;
    self
  }

  /// Advertise a fixed capability set on every heartbeat + claim
  /// (P10.16.2-FU1). Equivalent to setting `self.capabilities`
  /// directly.
  pub fn with_capabilities(mut self, capabilities: WorkerCapabilities) -> Self {
    self.capabilities = capabilities;
    self
  }
}

/// Errors emitted by the worker runtime.
#[derive(Debug, Error)]
pub enum WorkerError {
  #[error("scheduler error: {0}")]
  Scheduler(#[from] SchedulerError),

  #[error("invalid configuration: {0}")]
  InvalidConfig(String),
}

/// Cooperative cancellation token shared between the supervising
/// runtime and the worker. The runtime checks the flag before
/// dispatching the next task and before the inner await; cancellation
/// arriving mid-dispatch lets the current task finish and is reported
/// as a non-retryable cancellation failure.
///
/// **Stability:** experimental, tracked under P5.6 (see
/// `docs/STABILITY.md`). The shape may grow to thread per-task
/// cancellation in later milestones.
#[derive(Debug, Clone, Default)]
pub struct WorkerCancellationToken {
  flag: Arc<AtomicBool>,
}

impl WorkerCancellationToken {
  pub fn new() -> Self {
    Self::default()
  }

  /// Trip the cancellation flag. Subsequent dispatches return
  /// immediately with a cancellation failure; an already-running
  /// dispatch finishes naturally (no abort).
  pub fn cancel(&self) {
    self.flag.store(true, Ordering::SeqCst);
  }

  pub fn is_cancelled(&self) -> bool {
    self.flag.load(Ordering::SeqCst)
  }
}

/// Transport-independent worker loop.
#[derive(Debug, Clone)]
pub struct WorkerRuntime<P> {
  protocol: P,
  config: WorkerConfig,
  cancellation: WorkerCancellationToken,
  /// Q3.3.2: free-slot semaphore. Initialised to `config.free_slots`
  /// permits at construction. Every concurrent dispatch in
  /// `run_forever` acquires one permit; the heartbeat reports the
  /// real-time `available_permits()` count instead of the static
  /// config value, so the scheduler's placement decisions match
  /// what the worker can actually accept. With `free_slots = 1`
  /// the behavior is identical to pre-Q3.3.2 (serial dispatch).
  dispatch_slots: Arc<tokio::sync::Semaphore>,
}

impl<P> WorkerRuntime<P>
where
  P: WorkerProtocol,
{
  pub fn new(protocol: P, config: WorkerConfig) -> Self {
    // Q3.3.2: clamp to >= 1 so a misconfigured `free_slots = 0` does
    // not deadlock the dispatcher (semaphore::acquire would block
    // forever). Surface a warn so the operator notices the override.
    let slots = config.free_slots.max(1) as usize;
    if slots != config.free_slots as usize {
      eprintln!(
        "yanshi-worker: free_slots={} clamped to 1 to avoid dispatch deadlock",
        config.free_slots
      );
    }
    Self {
      protocol,
      config,
      cancellation: WorkerCancellationToken::new(),
      dispatch_slots: Arc::new(tokio::sync::Semaphore::new(slots)),
    }
  }

  /// Replace the runtime's cancellation token. Tests and supervisors
  /// keep a clone to signal a graceful shutdown.
  pub fn with_cancellation(mut self, token: WorkerCancellationToken) -> Self {
    self.cancellation = token;
    self
  }

  pub fn cancellation_token(&self) -> WorkerCancellationToken {
    self.cancellation.clone()
  }

  /// Run one heartbeat/claim/execute/report cycle.
  ///
  /// Q3.3.2: the heartbeat now reports the dynamic
  /// `dispatch_slots.available_permits()` count rather than the
  /// static `config.free_slots`. With concurrent dispatch in
  /// `run_forever` this lets the server see "this worker is
  /// currently saturated; route elsewhere" without waiting for a
  /// stale-heartbeat timeout. Single-dispatch callers of
  /// `run_once` continue to see the configured value because no
  /// permit is held at the call site.
  pub async fn run_once(&self) -> Result<Option<WorkerTask>, WorkerError> {
    let advertised_free = self.dispatch_slots.available_permits() as u32;
    self
      .protocol
      .heartbeat(
        WorkerHeartbeat::now(self.config.worker_id.clone(), None, advertised_free)
          .with_capabilities(self.config.capabilities.clone()),
      )
      .await?;

    // P10.16.2-FU1: send the worker's capabilities (and an empty
    // locality hint — the server defaults to "most-recently-
    // claimed run" when this is absent) so the queue scan can
    // skip work the worker can't run.
    let hints = ClaimHints::default().with_capabilities(self.config.capabilities.clone());
    let Some(task) = self
      .protocol
      .claim_task_with_hints(self.config.worker_id.clone(), &hints)
      .await?
    else {
      return Ok(None);
    };
    let result = execute_stub(
      &self.config.worker_id,
      &task,
      &self.config.resource_limits,
      &self.cancellation,
    )
    .await;
    self
      .protocol
      .report_result(self.config.worker_id.clone(), task.task_id, result)
      .await?;
    Ok(Some(task))
  }

  /// Run until cancelled. Q3.1.3 + Q3.3.2: the loop dispatches up
  /// to `config.free_slots` concurrent tasks, with each in-flight
  /// claim holding one semaphore permit until the spawned execute+
  /// report future completes. With `free_slots = 1` the behavior
  /// matches the pre-Q3.3.2 serial dispatcher; with `free_slots > 1`
  /// the loop saturates concurrent capacity instead of pinning at 1
  /// task per `poll_interval`.
  ///
  /// Error handling mirrors the prior serial loop: transport errors
  /// from the claim path trigger backoff retry; other errors are
  /// fatal; SIGTERM unblocks the backoff sleep immediately.
  pub async fn run_forever(&self) -> Result<(), WorkerError>
  where
    P: Clone + Send + 'static,
  {
    let mut last_backoff: Option<Duration> = None;
    let mut transport_failures: u32 = 0;
    loop {
      if self.cancellation.is_cancelled() {
        // Wait for any in-flight dispatch to surrender its permit
        // before returning. The semaphore caps at `free_slots`
        // total permits, so once we can re-acquire all of them
        // every spawned task has finished.
        let total = self.config.free_slots.max(1);
        let _drain = self.dispatch_slots.acquire_many(total).await.ok(); // semaphore can be closed; ignore in shutdown path
        return Ok(());
      }

      // Q3.3.2: block here until a slot is free. Race against the
      // cancellation token so SIGTERM doesn't wait for in-flight
      // tasks indefinitely (the drain branch above runs next).
      let permit = {
        let acquire_fut = self.dispatch_slots.clone().acquire_owned();
        tokio::pin!(acquire_fut);
        tokio::select! {
          permit = &mut acquire_fut => match permit {
            Ok(p) => p,
            Err(_) => return Ok(()), // semaphore closed
          },
          _ = wait_for_cancel(&self.cancellation) => continue,
        }
      };

      match self.dispatch_one_with_permit(permit).await {
        Ok(()) => {
          last_backoff = None;
          transport_failures = 0;
        }
        Err(WorkerError::Scheduler(SchedulerError::Transport { message })) => {
          transport_failures = transport_failures.saturating_add(1);
          if let Some(cap) = self.config.reconnect_backoff.max_attempts
            && transport_failures > cap
          {
            eprintln!(
              "yanshi-worker: transport error past --max-reconnect-attempts ({cap}); \
               giving up. last error: {message}"
            );
            return Err(WorkerError::Scheduler(SchedulerError::Transport {
              message,
            }));
          }
          let delay = self.config.reconnect_backoff.next_delay(last_backoff);
          eprintln!(
            "yanshi-worker: transport error (attempt {transport_failures}); \
             retrying in {delay:?}. error: {message}"
          );
          last_backoff = Some(delay);
          let sleep_fut = sleep(delay);
          tokio::pin!(sleep_fut);
          tokio::select! {
            _ = &mut sleep_fut => {}
            _ = wait_for_cancel(&self.cancellation) => return Ok(()),
          }
        }
        Err(other) => return Err(other),
      }
    }
  }

  /// Q3.3.2: claim + dispatch one task with the supplied permit
  /// in hand. The permit is moved into the spawned task and
  /// released only when execute+report completes, so the
  /// semaphore acts as the real free_slots gate.
  ///
  /// Returns `Ok(())` on (a) claim returned no task — permit is
  /// released here so the next loop iteration can re-claim, OR
  /// (b) a task was successfully spawned. Returns `Err` only on
  /// heartbeat / claim transport failures (so `run_forever`'s
  /// backoff branch fires); execute+report errors live inside
  /// the spawned task and are reported back through the
  /// protocol as `WorkerTaskResult::Failed`.
  async fn dispatch_one_with_permit(
    &self,
    permit: tokio::sync::OwnedSemaphorePermit,
  ) -> Result<(), WorkerError>
  where
    P: Clone + Send + 'static,
  {
    let advertised_free = self.dispatch_slots.available_permits() as u32;
    self
      .protocol
      .heartbeat(
        WorkerHeartbeat::now(self.config.worker_id.clone(), None, advertised_free)
          .with_capabilities(self.config.capabilities.clone()),
      )
      .await?;
    let hints = ClaimHints::default().with_capabilities(self.config.capabilities.clone());
    let Some(task) = self
      .protocol
      .claim_task_with_hints(self.config.worker_id.clone(), &hints)
      .await?
    else {
      // No work — release the permit immediately so the next iter
      // can try again. `permit` drops at end of scope, but we make
      // it explicit so future readers don't think it leaks.
      drop(permit);
      // Idle pause so we don't tight-loop the control plane.
      let sleep_fut = sleep(self.config.poll_interval);
      tokio::pin!(sleep_fut);
      tokio::select! {
        _ = &mut sleep_fut => {}
        _ = wait_for_cancel(&self.cancellation) => {}
      }
      return Ok(());
    };

    // Spawn execute+report; the permit lives until the task ends.
    let protocol = self.protocol.clone();
    let worker_id = self.config.worker_id.clone();
    let limits = self.config.resource_limits.clone();
    let cancellation = self.cancellation.clone();
    tokio::spawn(async move {
      let result = execute_stub(&worker_id, &task, &limits, &cancellation).await;
      if let Err(err) = protocol
        .report_result(worker_id, task.task_id, result)
        .await
      {
        // Best-effort: log + drop. The scheduler will eventually
        // requeue the task via stale-heartbeat reaping.
        eprintln!(
          "yanshi-worker: failed to report result for task {}: {err}",
          task.task_id
        );
      }
      // permit drops here, freeing the slot.
      drop(permit);
    });
    Ok(())
  }
}

async fn execute_stub(
  worker_id: &WorkerId,
  task: &WorkerTask,
  limits: &WorkerResourceLimits,
  cancellation: &WorkerCancellationToken,
) -> WorkerTaskResult {
  // Pre-cancel check: tasks that were claimed before the runtime was
  // asked to shut down are still rejected so we don't run extra work
  // post-cancellation. The claim itself is allowed because that path
  // is owned by the supervising runtime.
  if cancellation.is_cancelled() {
    return cancelled_result(worker_id, task);
  }

  if let Ok(payload) = serde_json::from_value::<NodeExecutionPayload>(task.payload.clone()) {
    return execute_node_payload(worker_id, task, payload, limits, cancellation).await;
  }

  WorkerTaskResult::Succeeded {
    output: serde_json::json!({
      "worker_id": worker_id.0,
      "task_id": task.task_id,
      "node_id": task.node_id,
      "attempt": task.attempt,
      "payload": task.payload,
    }),
    events: vec![
      WorkerTraceEvent {
        seq: 0,
        kind: "worker.task.started".into(),
        payload: serde_json::json!({
          "worker_id": worker_id.0,
          "task_id": task.task_id,
          "node_id": task.node_id,
        }),
      },
      WorkerTraceEvent {
        seq: 1,
        kind: "worker.task.completed".into(),
        payload: serde_json::json!({
          "worker_id": worker_id.0,
          "task_id": task.task_id,
          "node_id": task.node_id,
        }),
      },
    ],
  }
}

/// Future that resolves once the cancellation flag flips. Polled
/// alongside the dispatcher so the worker reacts to cancel within one
/// poll interval.
async fn wait_for_cancel(token: &WorkerCancellationToken) {
  loop {
    if token.is_cancelled() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
  }
}

fn cancelled_during_dispatch(
  worker_id: &WorkerId,
  task: &WorkerTask,
  node_type: &str,
  started: WorkerTraceEvent,
) -> WorkerTaskResult {
  WorkerTaskResult::Failed {
    error: format!("distributed worker cancelled mid-dispatch of node '{node_type}'"),
    retryable: false,
    events: vec![
      started,
      WorkerTraceEvent {
        seq: 1,
        kind: "worker.task.cancelled".into(),
        payload: serde_json::json!({
          "worker_id": worker_id.0,
          "task_id": task.task_id,
          "node_id": task.node_id,
          "node_type": node_type,
          "attempt": task.attempt,
        }),
      },
    ],
  }
}

/// Cap the serialized success output. When `max_output_bytes` is set
/// and the output exceeds the cap, replace it with a small marker
/// envelope and emit a `worker.task.output_truncated` trace event so
/// operators can audit where the cut happened.
fn cap_success_output(
  worker_id: &WorkerId,
  task: &WorkerTask,
  outputs: std::collections::HashMap<String, FlowValue>,
  max_output_bytes: Option<usize>,
) -> (serde_json::Value, Vec<WorkerTraceEvent>) {
  let value = serde_json::to_value(&outputs).unwrap_or_else(|_| serde_json::json!({}));
  let Some(max) = max_output_bytes else {
    return (value, Vec::new());
  };
  let serialized = match serde_json::to_vec(&value) {
    Ok(bytes) => bytes,
    Err(_) => return (value, Vec::new()),
  };
  if serialized.len() <= max {
    return (value, Vec::new());
  }
  let truncated = serde_json::json!({
    "truncated": true,
    "limit_bytes": max,
    "size_bytes": serialized.len(),
  });
  let event = WorkerTraceEvent {
    // `seq` here is 1; `execute_node_payload` re-indexes the event
    // stream so this stays consistent with the `started` event.
    seq: 1,
    kind: "worker.task.output_truncated".into(),
    payload: serde_json::json!({
      "worker_id": worker_id.0,
      "task_id": task.task_id,
      "node_id": task.node_id,
      "attempt": task.attempt,
      "limit_bytes": max,
      "size_bytes": serialized.len(),
    }),
  };
  (truncated, vec![event])
}

fn cancelled_result(worker_id: &WorkerId, task: &WorkerTask) -> WorkerTaskResult {
  WorkerTaskResult::Failed {
    error: "worker cancelled before dispatching task".to_string(),
    // Cancellation is operator-initiated, never a transport hiccup.
    // The scheduler treats this as terminal so retries don't loop
    // when the worker is draining.
    retryable: false,
    events: vec![WorkerTraceEvent {
      seq: 0,
      kind: "worker.task.cancelled".into(),
      payload: serde_json::json!({
        "worker_id": worker_id.0,
        "task_id": task.task_id,
        "node_id": task.node_id,
        "attempt": task.attempt,
      }),
    }],
  }
}

async fn execute_node_payload(
  worker_id: &WorkerId,
  task: &WorkerTask,
  payload: NodeExecutionPayload,
  limits: &WorkerResourceLimits,
  cancellation: &WorkerCancellationToken,
) -> WorkerTaskResult {
  let started = WorkerTraceEvent {
    seq: 0,
    kind: "worker.task.started".into(),
    payload: serde_json::json!({
      "worker_id": worker_id.0,
      "task_id": task.task_id,
      "node_id": task.node_id,
      "node_type": payload.node_type,
      "attempt": task.attempt,
    }),
  };

  let node_type = payload.node_type.clone();
  let inner = execute_supported_node_payload(payload, task.attempt);
  let timeout = limits.default_timeout;

  let dispatch = async {
    if let Some(deadline) = timeout {
      match tokio::time::timeout(deadline, inner).await {
        Ok(result) => result,
        Err(_) => Err(YanshiError::AsyncExecutionError {
          message: format!("distributed worker timeout: node '{node_type}' exceeded {deadline:?}"),
        }),
      }
    } else {
      inner.await
    }
  };

  // Cancellation cuts the dispatch off as soon as it can yield. The
  // inner await is the only suspension point we control, so we race
  // it against a cancellation poll.
  let result = tokio::select! {
    biased;
    () = wait_for_cancel(cancellation) => {
      return cancelled_during_dispatch(worker_id, task, &node_type, started);
    }
    res = dispatch => res,
  };

  match result {
    Ok(outputs) => {
      let (output_value, mut extra_events) =
        cap_success_output(worker_id, task, outputs, limits.max_output_bytes);
      let mut events = vec![started];
      events.append(&mut extra_events);
      events.push(WorkerTraceEvent {
        seq: events.len() as i64,
        kind: "worker.task.completed".into(),
        payload: serde_json::json!({
          "worker_id": worker_id.0,
          "task_id": task.task_id,
          "node_id": task.node_id,
          "attempt": task.attempt,
        }),
      });
      WorkerTaskResult::Succeeded {
        output: output_value,
        events,
      }
    }
    Err(error) => WorkerTaskResult::Failed {
      error: error.to_string(),
      retryable: matches!(error, YanshiError::AsyncExecutionError { .. }),
      events: vec![
        started,
        WorkerTraceEvent {
          seq: 1,
          kind: "worker.task.failed".into(),
          payload: serde_json::json!({
            "worker_id": worker_id.0,
            "task_id": task.task_id,
            "node_id": task.node_id,
            "attempt": task.attempt,
            "error": error.to_string(),
          }),
        },
      ],
    },
  }
}

async fn execute_supported_node_payload(
  payload: NodeExecutionPayload,
  attempt: u32,
) -> Result<std::collections::HashMap<String, FlowValue>, YanshiError> {
  match payload.node_type.as_str() {
    "template" => execute_template_payload(&payload).await,
    "file" => execute_file_payload(&payload).await,
    "mock" => execute_mock_payload(&payload, attempt).await,
    // P2.8: distributed support for LLM / HTTP / MCP / agent payloads.
    // The local scheduler already inlines `parameters` into `inputs` (see
    // `gather_inputs` in `yanshi-server::scheduler::distributed`), so
    // each node's `execute` receives the same input map it would in-process.
    "llm" => execute_llm_payload(&payload).await,
    "http" => execute_http_payload(&payload).await,
    "mcp" => execute_mcp_payload(&payload).await,
    "agent" => execute_agent_payload(&payload).await,
    other => Err(YanshiError::FlowDefinitionError {
      message: format!("distributed worker does not support node type '{other}'"),
    }),
  }
}

async fn execute_template_payload(payload: &NodeExecutionPayload) -> AsyncNodeResult {
  let template = string_parameter(payload, "template")?;
  let mut node = TemplateNode::new(&payload.node_id, &template);
  if let Some(output_key) = optional_string_parameter(payload, "output_key") {
    node = node.with_output_key(&output_key);
  }
  if let Some(output_format) = optional_string_parameter(payload, "output_format") {
    node = node.with_format(&output_format);
  }
  node.execute(&payload.inputs).await
}

async fn execute_file_payload(payload: &NodeExecutionPayload) -> AsyncNodeResult {
  // V0.2: `FileNode::default()` denies every path (`SandboxPolicy::
  // default()`'s `allowed_paths` is empty). The local config factory
  // (`yanshi-config::executor::factory`) requires the YAML `file`
  // node to declare `allowed_paths`, and the scheduler forwards a
  // node's `parameters` verbatim into `payload.parameters` (see
  // `dispatch_node` in `yanshi-server::scheduler::distributed`), so
  // read it the same way here rather than falling back to a permissive
  // policy that would undo the factory's guarantee for distributed runs.
  let allowed_paths: Vec<std::path::PathBuf> = payload
    .parameters
    .get("allowed_paths")
    .and_then(|value| value.as_array())
    .map(|seq| {
      seq
        .iter()
        .filter_map(|v| v.as_str().map(std::path::PathBuf::from))
        .collect()
    })
    .unwrap_or_default();
  let policy = Arc::new(yanshi_tools::SandboxPolicy {
    allowed_paths,
    ..yanshi_tools::SandboxPolicy::default()
  });
  FileNode::new(policy).execute(&payload.inputs).await
}

async fn execute_mock_payload(payload: &NodeExecutionPayload, attempt: u32) -> AsyncNodeResult {
  if let Some(fail_until_attempt) = payload
    .parameters
    .get("fail_until_attempt")
    .and_then(|value| value.as_u64())
    && u64::from(attempt) < fail_until_attempt
  {
    return Err(YanshiError::AsyncExecutionError {
      message: format!("mock node requested failure until attempt {fail_until_attempt}"),
    });
  }
  if matches!(
    payload
      .parameters
      .get("fail")
      .and_then(|value| value.as_bool()),
    Some(true)
  ) {
    return Err(YanshiError::AsyncExecutionError {
      message: "mock node requested failure".to_string(),
    });
  }
  // P5.6 — synthetic runaway hook: a non-zero `sleep_ms` parameter
  // makes the mock node yield for the given wall-clock duration so
  // the timeout / cancellation paths can be exercised deterministically
  // without spinning up a real long-running node.
  if let Some(sleep_ms) = payload
    .parameters
    .get("sleep_ms")
    .and_then(|value| value.as_u64())
    && sleep_ms > 0
  {
    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
  }
  let mut outputs = std::collections::HashMap::new();
  let value = payload
    .parameters
    .get("value")
    .cloned()
    .unwrap_or_else(|| serde_json::json!(payload.node_id));
  outputs.insert("output".to_string(), FlowValue::Json(value));
  // P5.6 — synthetic large-output hook: emit an extra `payload` key
  // with `output_size_bytes` worth of 'x' characters so the
  // truncation path is testable hermetically.
  if let Some(size) = payload
    .parameters
    .get("output_size_bytes")
    .and_then(|value| value.as_u64())
    && size > 0
  {
    let big = "x".repeat(size.min(64 * 1024 * 1024) as usize);
    outputs.insert(
      "payload".to_string(),
      FlowValue::Json(serde_json::json!(big)),
    );
  }
  Ok(outputs)
}

async fn execute_llm_payload(payload: &NodeExecutionPayload) -> AsyncNodeResult {
  LlmNode.execute(&payload.inputs).await
}

async fn execute_http_payload(payload: &NodeExecutionPayload) -> AsyncNodeResult {
  HttpNode::default().execute(&payload.inputs).await
}

async fn execute_mcp_payload(payload: &NodeExecutionPayload) -> AsyncNodeResult {
  MCPNode::default().execute(&payload.inputs).await
}

/// Minimal ReAct loop dispatcher for distributed `agent` nodes.
///
/// The worker reads the canonical agent inputs (`message`, `model`, optional
/// `persona` / `max_iterations`) from the gathered input map. W4.1d: an
/// optional `payload.parameters["tools"]` field, when present, deserializes
/// as a `ToolManifest` (`yanshi-tools::manifest`, File+Http only this
/// pass) and builds a real registry via `build_registry_from_manifest` —
/// mirrors `execute_file_payload`'s existing `allowed_paths`-from-`parameters`
/// precedent, no `worker.proto` change needed. Absent the field, the agent
/// still gets an empty `ToolRegistry` (unchanged, backward-compatible
/// behavior — no DAG author is required to declare a manifest).
/// Shell/Script/`code_exec` manifest entries and a worker-side approval RPC
/// are deferred (`docs/RFC_TOOL_DISTRIBUTION.md`).
/// W4.1d: parse `payload.parameters["tools"]` as a `ToolManifest` and
/// build the registry it declares (File+Http only this pass); absent the
/// field, an empty registry — unchanged, backward-compatible behavior.
/// Factored out of `execute_agent_payload` so it's unit-testable without
/// standing up a real (or mock) LLM call.
fn build_agent_tool_registry(payload: &NodeExecutionPayload) -> Result<ToolRegistry, YanshiError> {
  match payload.parameters.get("tools") {
    Some(value) => {
      let manifest: yanshi_tools::ToolManifest =
        serde_json::from_value(value.clone()).map_err(|e| YanshiError::AsyncExecutionError {
          message: format!("invalid `tools` manifest for agent node: {e}"),
        })?;
      // No workspace concept for a distributed `agent` node — this is
      // only the fallback for an entry that omits its own `sandbox`;
      // manifest authors should set `sandbox.allowed_paths` explicitly
      // rather than rely on the worker process's CWD.
      yanshi_tools::build_registry_from_manifest(&manifest, std::path::Path::new(".")).map_err(
        |e| YanshiError::AsyncExecutionError {
          message: format!("failed to build tool registry from manifest: {e}"),
        },
      )
    }
    None => Ok(ToolRegistry::new()),
  }
}

async fn execute_agent_payload(payload: &NodeExecutionPayload) -> AsyncNodeResult {
  let message = required_string_input(payload, "message")?;
  let model = required_string_input(payload, "model")?;
  let persona =
    optional_string_input(payload, "persona").or_else(|| optional_string_input(payload, "system"));
  let max_iterations = optional_u64_input(payload, "max_iterations");

  let mut config = ReActConfig::new(model);
  if let Some(persona) = persona {
    config = config.with_persona(persona);
  }
  if let Some(max_iterations) = max_iterations {
    config = config.with_max_iterations(max_iterations.min(usize::MAX as u64) as usize);
  }

  let registry = build_agent_tool_registry(payload)?;

  let mut agent = ReActAgent::new(
    config,
    Box::new(SessionMemory::default_window()),
    Arc::new(registry),
  );

  let result =
    agent
      .run_with_trace(&message)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("distributed agent run failed: {e}"),
      })?;

  let stop_reason =
    serde_json::to_value(&result.stop_reason).map_err(|e| YanshiError::AsyncExecutionError {
      message: format!("failed to serialize agent stop reason: {e}"),
    })?;

  let mut outputs = std::collections::HashMap::new();
  outputs.insert(
    "answer".to_string(),
    FlowValue::Json(serde_json::Value::String(
      result.answer.clone().unwrap_or_default(),
    )),
  );
  outputs.insert("stop_reason".to_string(), FlowValue::Json(stop_reason));
  outputs.insert(
    "session_id".to_string(),
    FlowValue::Json(serde_json::Value::String(result.session_id.clone())),
  );
  outputs.insert(
    "step_count".to_string(),
    FlowValue::Json(serde_json::json!(result.steps.len())),
  );
  Ok(outputs)
}

fn required_string_input(payload: &NodeExecutionPayload, key: &str) -> Result<String, YanshiError> {
  optional_string_input(payload, key).ok_or_else(|| YanshiError::NodeInputError {
    message: format!(
      "distributed node '{}' requires string input '{}'",
      payload.node_id, key
    ),
  })
}

fn optional_string_input(payload: &NodeExecutionPayload, key: &str) -> Option<String> {
  payload.inputs.get(key).and_then(|value| match value {
    FlowValue::Json(serde_json::Value::String(s)) => Some(s.clone()),
    _ => None,
  })
}

fn optional_u64_input(payload: &NodeExecutionPayload, key: &str) -> Option<u64> {
  payload.inputs.get(key).and_then(|value| match value {
    FlowValue::Json(serde_json::Value::Number(n)) => n.as_u64(),
    _ => None,
  })
}

fn string_parameter(payload: &NodeExecutionPayload, key: &str) -> Result<String, YanshiError> {
  optional_string_parameter(payload, key).ok_or_else(|| YanshiError::NodeInputError {
    message: format!(
      "distributed node '{}' requires string parameter '{}'",
      payload.node_id, key
    ),
  })
}

fn optional_string_parameter(payload: &NodeExecutionPayload, key: &str) -> Option<String> {
  payload
    .parameters
    .get(key)
    .and_then(|value| value.as_str())
    .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::Duration as ChronoDuration;
  use std::net::SocketAddr;
  use tokio::sync::oneshot;
  use tonic::transport::Server;
  use uuid::Uuid;
  use yanshi_server::{
    GrpcWorkerProtocol, InMemoryWorkerProtocol, RunControlStatus, WorkerControlPlane,
    WorkerControlServer,
    scheduler::distributed::{mock_flow, mock_node},
  };

  #[tokio::test]
  async fn run_once_heartbeats_claims_and_reports_success() {
    let protocol = InMemoryWorkerProtocol::new();
    let run_id = Uuid::new_v4();
    let task = WorkerTask::new(run_id, "node-a", serde_json::json!({"input": 1}));
    let task_id = task.task_id;
    protocol.submit_task(task).await.unwrap();

    let worker_id = WorkerId::new("worker-a").unwrap();
    let runtime = WorkerRuntime::new(
      protocol.clone(),
      WorkerConfig::new(worker_id.clone(), "memory://local"),
    );
    let claimed = runtime.run_once().await.unwrap();

    assert_eq!(claimed.map(|task| task.task_id), Some(task_id));
    assert!(protocol.last_heartbeat(&worker_id).await.is_some());
    let result = protocol.completed_result(task_id).await.unwrap();
    let WorkerTaskResult::Succeeded { output, events } = result else {
      panic!("expected success");
    };
    assert_eq!(output["node_id"], "node-a");
    assert_eq!(events.len(), 2);
  }

  #[tokio::test]
  async fn run_once_returns_none_when_queue_is_empty() {
    let protocol = InMemoryWorkerProtocol::new();
    let worker_id = WorkerId::new("worker-a").unwrap();
    let runtime = WorkerRuntime::new(
      protocol,
      WorkerConfig::new(worker_id.clone(), "memory://local"),
    );

    assert!(runtime.run_once().await.unwrap().is_none());
  }

  #[tokio::test]
  async fn two_workers_claim_and_report_over_grpc() {
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let run_id = Uuid::new_v4();
    control
      .schedule_task(WorkerTask::new(
        run_id,
        "node-a",
        serde_json::json!({"input": "a"}),
      ))
      .await
      .unwrap();
    control
      .schedule_task(WorkerTask::new(
        run_id,
        "node-b",
        serde_json::json!({"input": "b"}),
      ))
      .await
      .unwrap();

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let addr = unused_local_addr();
    let server_control = control.clone();
    let server = tokio::spawn(async move {
      Server::builder()
        .add_service(WorkerControlServer::new(server_control))
        .serve_with_shutdown(addr, async {
          let _ = shutdown_rx.await;
        })
        .await
    });

    let endpoint = format!("http://{addr}");
    let worker_a = WorkerId::new("worker-a").unwrap();
    let worker_b = WorkerId::new("worker-b").unwrap();
    let runtime_a = WorkerRuntime::new(
      connect_with_retry(&endpoint).await,
      WorkerConfig::new(worker_a.clone(), endpoint.clone()),
    );
    let runtime_b = WorkerRuntime::new(
      connect_with_retry(&endpoint).await,
      WorkerConfig::new(worker_b.clone(), endpoint),
    );

    let task_a = runtime_a.run_once().await.unwrap().unwrap();
    let task_b = runtime_b.run_once().await.unwrap().unwrap();
    assert_ne!(task_a.task_id, task_b.task_id);

    let snapshot = control.run_snapshot(run_id).await.unwrap();
    assert_eq!(snapshot.status, RunControlStatus::Succeeded);
    assert_eq!(snapshot.succeeded_tasks, 2);
    assert_eq!(snapshot.outputs.len(), 2);
    assert_eq!(snapshot.stitched_trace_events.len(), 4);
    assert!(control.worker_heartbeat(&worker_a).await.is_some());
    assert!(control.worker_heartbeat(&worker_b).await.is_some());

    let _ = shutdown_tx.send(());
    server.await.unwrap().unwrap();
  }

  #[tokio::test]
  async fn run_once_executes_distributed_template_payload() {
    let protocol = InMemoryWorkerProtocol::new();
    let worker_id = WorkerId::new("worker-template").unwrap();
    let run_id = Uuid::new_v4();
    let payload = NodeExecutionPayload::new(
      "render",
      "template",
      std::collections::HashMap::from([(
        "template".to_string(),
        serde_json::json!("Hello {{ name }}"),
      )]),
      std::collections::HashMap::from([(
        "name".to_string(),
        FlowValue::Json(serde_json::json!("Ada")),
      )]),
    );
    let task = WorkerTask::new(run_id, "render", serde_json::to_value(payload).unwrap());
    let task_id = task.task_id;
    protocol.submit_task(task).await.unwrap();

    let runtime = WorkerRuntime::new(
      protocol.clone(),
      WorkerConfig::new(worker_id, "memory://local"),
    );
    runtime.run_once().await.unwrap();

    let WorkerTaskResult::Succeeded { output, .. } =
      protocol.completed_result(task_id).await.unwrap()
    else {
      panic!("expected template success");
    };
    assert_eq!(output["output"]["value"], "Hello Ada");
  }

  #[tokio::test]
  async fn run_once_executes_distributed_file_payload() {
    let protocol = InMemoryWorkerProtocol::new();
    let worker_id = WorkerId::new("worker-file").unwrap();
    let run_id = Uuid::new_v4();
    let path = std::env::temp_dir().join(format!("yanshi-worker-{}.txt", Uuid::new_v4()));
    let payload = NodeExecutionPayload::new(
      "write_file",
      "file",
      std::collections::HashMap::from([(
        "allowed_paths".to_string(),
        serde_json::json!([std::env::temp_dir().to_string_lossy()]),
      )]),
      std::collections::HashMap::from([
        (
          "operation".to_string(),
          FlowValue::Json(serde_json::json!("write")),
        ),
        (
          "path".to_string(),
          FlowValue::Json(serde_json::json!(path.to_string_lossy())),
        ),
        (
          "content".to_string(),
          FlowValue::Json(serde_json::json!("distributed file write")),
        ),
      ]),
    );
    protocol
      .submit_task(WorkerTask::new(
        run_id,
        "write_file",
        serde_json::to_value(payload).unwrap(),
      ))
      .await
      .unwrap();

    let runtime = WorkerRuntime::new(protocol, WorkerConfig::new(worker_id, "memory://local"));
    runtime.run_once().await.unwrap();

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "distributed file write");
    let _ = tokio::fs::remove_file(path).await;
  }

  /// V0.2 regression: a distributed `file` payload with no `allowed_paths`
  /// parameter must be denied, not silently executed against a permissive
  /// default (which is what `FileNode::default()` used to be).
  #[tokio::test]
  async fn run_once_denies_distributed_file_payload_without_allowed_paths() {
    let protocol = InMemoryWorkerProtocol::new();
    let worker_id = WorkerId::new("worker-file-denied").unwrap();
    let run_id = Uuid::new_v4();
    let path = std::env::temp_dir().join(format!("yanshi-worker-{}.txt", Uuid::new_v4()));
    let payload = NodeExecutionPayload::new(
      "write_file",
      "file",
      std::collections::HashMap::new(),
      std::collections::HashMap::from([
        (
          "operation".to_string(),
          FlowValue::Json(serde_json::json!("write")),
        ),
        (
          "path".to_string(),
          FlowValue::Json(serde_json::json!(path.to_string_lossy())),
        ),
        (
          "content".to_string(),
          FlowValue::Json(serde_json::json!("should never be written")),
        ),
      ]),
    );
    let task = WorkerTask::new(run_id, "write_file", serde_json::to_value(payload).unwrap());
    let task_id = task.task_id;
    protocol.submit_task(task).await.unwrap();

    let runtime = WorkerRuntime::new(
      protocol.clone(),
      WorkerConfig::new(worker_id, "memory://local"),
    );
    runtime.run_once().await.unwrap();

    let WorkerTaskResult::Failed { error, .. } = protocol.completed_result(task_id).await.unwrap()
    else {
      panic!("expected the sandbox-denied write to fail");
    };
    assert!(
      error.contains("sandbox policy denied"),
      "expected a sandbox denial, got: {error}"
    );
    assert!(!path.exists(), "file must not have been created");
  }

  /// W4.1d: a `parameters["tools"]` manifest builds a registry with
  /// exactly the declared File+Http tools instead of the always-empty
  /// default.
  #[test]
  fn build_agent_tool_registry_builds_declared_tools_from_manifest() {
    let payload = NodeExecutionPayload::new(
      "agent-node",
      "agent",
      std::collections::HashMap::from([(
        "tools".to_string(),
        serde_json::json!({
          "tools": [
            {
              "kind": "file",
              "definition": {
                "name": "file",
                "description": "file tool",
                "parameters": {},
              },
            },
            {
              "kind": "http",
              "definition": {
                "name": "http",
                "description": "http tool",
                "parameters": {},
              },
            },
          ]
        }),
      )]),
      std::collections::HashMap::new(),
    );

    let registry = build_agent_tool_registry(&payload).expect("registry built from manifest");
    let names: Vec<String> = registry
      .list()
      .iter()
      .map(|tool| tool.name().to_string())
      .collect();
    assert!(names.contains(&"file".to_string()));
    assert!(names.contains(&"http".to_string()));
    assert_eq!(names.len(), 2);
  }

  /// W4.1d regression: no `tools` field must still yield an empty
  /// registry — unchanged, backward-compatible behavior for DAG authors
  /// who don't declare a manifest.
  #[test]
  fn build_agent_tool_registry_with_no_tools_field_is_empty() {
    let payload = NodeExecutionPayload::new(
      "agent-node",
      "agent",
      std::collections::HashMap::new(),
      std::collections::HashMap::new(),
    );

    let registry = build_agent_tool_registry(&payload).expect("empty registry, no manifest");
    assert!(registry.list().is_empty());
  }

  /// A malformed `tools` value (fails to deserialize as `ToolManifest`)
  /// must surface as an error rather than silently falling back to an
  /// empty registry — a DAG author who typo'd the manifest should see it
  /// fail loudly, not run with unexpectedly no tools.
  #[test]
  fn build_agent_tool_registry_rejects_malformed_manifest() {
    let payload = NodeExecutionPayload::new(
      "agent-node",
      "agent",
      std::collections::HashMap::from([(
        "tools".to_string(),
        serde_json::json!({"tools": [{"kind": "not_a_real_kind"}]}),
      )]),
      std::collections::HashMap::new(),
    );

    let result = build_agent_tool_registry(&payload);
    assert!(
      result.is_err(),
      "malformed manifest must fail, not silently degrade"
    );
  }

  #[tokio::test]
  async fn distributed_scheduler_runs_100_mock_nodes_with_two_workers() {
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let run_id = Uuid::new_v4();
    let nodes = (0..100)
      .map(|idx| mock_node(format!("node-{idx}"), Vec::new(), serde_json::json!(idx)))
      .collect::<Vec<_>>();
    let flow = mock_flow("large mock", nodes);
    let mut scheduler =
      yanshi_server::DistributedDagScheduler::new(run_id, flow, control.clone()).unwrap();
    let worker_a = WorkerRuntime::new(
      control.clone(),
      WorkerConfig::new(WorkerId::new("worker-a").unwrap(), "memory://local"),
    );
    let worker_b = WorkerRuntime::new(
      control.clone(),
      WorkerConfig::new(WorkerId::new("worker-b").unwrap(), "memory://local"),
    );

    while !scheduler.is_terminal() {
      scheduler.dispatch_ready().await.unwrap();
      let claimed_a = worker_a.run_once().await.unwrap();
      let claimed_b = worker_b.run_once().await.unwrap();
      scheduler.reconcile_results().await.unwrap();
      if claimed_a.is_none() && claimed_b.is_none() && scheduler.running_count() == 0 {
        break;
      }
    }

    let result = scheduler.run_result();
    assert!(result.succeeded);
    assert_eq!(result.state_pool.len(), 100);
    let snapshot = control.run_snapshot(run_id).await.unwrap();
    assert_eq!(snapshot.succeeded_tasks, 100);
    assert_eq!(snapshot.stitched_trace_events.len(), 200);
  }

  #[tokio::test]
  async fn distributed_scheduler_retries_retryable_failure() {
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let run_id = Uuid::new_v4();
    let mut node = mock_node("retry-once", Vec::new(), serde_json::json!("ok"));
    node.parameters.insert(
      "fail_until_attempt".to_string(),
      serde_yaml::to_value(1).unwrap(),
    );
    let flow = mock_flow("retry mock", vec![node]);
    let mut scheduler = yanshi_server::DistributedDagScheduler::new(run_id, flow, control.clone())
      .unwrap()
      .with_max_attempts(2);
    let worker = WorkerRuntime::new(
      control.clone(),
      WorkerConfig::new(WorkerId::new("worker-retry").unwrap(), "memory://local"),
    );

    while !scheduler.is_terminal() {
      scheduler.dispatch_ready().await.unwrap();
      let _ = worker.run_once().await.unwrap();
      scheduler.reconcile_results().await.unwrap();
    }

    let result = scheduler.run_result();
    assert!(result.succeeded);
    let snapshot = control.run_snapshot(run_id).await.unwrap();
    assert_eq!(snapshot.failed_tasks, 1);
    assert_eq!(snapshot.succeeded_tasks, 1);
  }

  #[tokio::test]
  async fn distributed_scheduler_requeues_stale_heartbeat_task() {
    let protocol = InMemoryWorkerProtocol::new();
    let control = WorkerControlPlane::new(protocol);
    let run_id = Uuid::new_v4();
    let flow = mock_flow(
      "stale mock",
      vec![mock_node("stale-node", Vec::new(), serde_json::json!("ok"))],
    );
    let mut scheduler = yanshi_server::DistributedDagScheduler::new(run_id, flow, control.clone())
      .unwrap()
      .with_max_attempts(2)
      .with_heartbeat_timeout(Duration::from_millis(1));
    scheduler.dispatch_ready().await.unwrap();

    let worker_id = WorkerId::new("stale-worker").unwrap();
    let claimed = control
      .claim_task(worker_id.clone())
      .await
      .unwrap()
      .unwrap();
    control
      .heartbeat(WorkerHeartbeat {
        worker_id,
        active_task: Some(claimed.task_id),
        free_slots: 0,
        ts: chrono::Utc::now() - ChronoDuration::seconds(5),
        capabilities: Default::default(),
      })
      .await
      .unwrap();

    let requeued = scheduler.requeue_stale_tasks().await.unwrap();
    assert_eq!(requeued, 1);
    assert_eq!(
      scheduler.node_status("stale-node"),
      Some(yanshi_server::DistributedNodeStatus::Pending)
    );
    scheduler.dispatch_ready().await.unwrap();
    assert_eq!(scheduler.running_count(), 1);
  }

  async fn connect_with_retry(endpoint: &str) -> GrpcWorkerProtocol {
    let mut last_error = None;
    for _ in 0..20 {
      match GrpcWorkerProtocol::connect(endpoint).await {
        Ok(protocol) => return protocol,
        Err(err) => {
          last_error = Some(err);
          sleep(Duration::from_millis(25)).await;
        }
      }
    }
    panic!("failed to connect to gRPC worker control: {last_error:?}");
  }

  fn unused_local_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
  }

  // ─── Q3.1.3 backoff + cancellation regression suite ───────────────

  /// Mock protocol that returns `SchedulerError::Transport` for the
  /// first N heartbeat calls and then succeeds. Used to drive
  /// `run_forever`'s recovery path without spinning up a real gRPC
  /// server.
  struct FlakyProtocol {
    transport_failures_remaining: std::sync::Mutex<u32>,
    heartbeat_count: std::sync::Mutex<u32>,
  }

  impl FlakyProtocol {
    fn new(initial_failures: u32) -> Self {
      Self {
        transport_failures_remaining: std::sync::Mutex::new(initial_failures),
        heartbeat_count: std::sync::Mutex::new(0),
      }
    }

    fn heartbeat_count(&self) -> u32 {
      *self.heartbeat_count.lock().unwrap()
    }
  }

  #[async_trait::async_trait]
  impl WorkerProtocol for FlakyProtocol {
    async fn submit_task(&self, _task: WorkerTask) -> Result<(), SchedulerError> {
      Ok(())
    }
    async fn claim_task(&self, _worker_id: WorkerId) -> Result<Option<WorkerTask>, SchedulerError> {
      Ok(None)
    }
    async fn report_result(
      &self,
      _worker_id: WorkerId,
      _task_id: Uuid,
      _result: WorkerTaskResult,
    ) -> Result<(), SchedulerError> {
      Ok(())
    }
    async fn heartbeat(&self, _heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError> {
      *self.heartbeat_count.lock().unwrap() += 1;
      let mut remaining = self.transport_failures_remaining.lock().unwrap();
      if *remaining > 0 {
        *remaining -= 1;
        return Err(SchedulerError::Transport {
          message: "simulated control-plane blip".into(),
        });
      }
      Ok(())
    }
  }

  #[test]
  fn reconnect_backoff_doubles_until_cap() {
    let backoff =
      ReconnectBackoff::deterministic(Duration::from_millis(100), Duration::from_secs(1));
    // First failure → initial.
    assert_eq!(backoff.next_delay(None), Duration::from_millis(100));
    // Subsequent failures double until capped.
    assert_eq!(
      backoff.next_delay(Some(Duration::from_millis(100))),
      Duration::from_millis(200)
    );
    assert_eq!(
      backoff.next_delay(Some(Duration::from_millis(200))),
      Duration::from_millis(400)
    );
    assert_eq!(
      backoff.next_delay(Some(Duration::from_millis(400))),
      Duration::from_millis(800)
    );
    // Cap kicks in.
    assert_eq!(
      backoff.next_delay(Some(Duration::from_millis(800))),
      Duration::from_secs(1)
    );
    assert_eq!(
      backoff.next_delay(Some(Duration::from_secs(1))),
      Duration::from_secs(1)
    );
  }

  #[test]
  fn reconnect_backoff_jitter_stays_within_window() {
    // ±25% jitter band; with cap=200ms the value must always land in
    // [150, 250].
    let backoff = ReconnectBackoff {
      initial: Duration::from_millis(200),
      max: Duration::from_millis(200),
      jitter: true,
      ..Default::default()
    };
    for _ in 0..50 {
      let d = backoff
        .next_delay(Some(Duration::from_millis(200)))
        .as_millis() as u64;
      assert!(
        (150..=250).contains(&d),
        "jittered backoff must stay within ±25% of cap; got {d}ms"
      );
    }
  }

  #[tokio::test]
  async fn run_forever_recovers_from_transport_blip() {
    // The flaky protocol fails the first 3 heartbeats then succeeds.
    // `run_forever` must keep going instead of returning Err on the
    // very first transport failure (the pre-Q3.1.3 behaviour).
    let protocol = Arc::new(FlakyProtocol::new(3));
    let worker_id = WorkerId::new("worker-flaky").unwrap();
    let mut config = WorkerConfig::new(worker_id, "memory://local");
    config.poll_interval = Duration::from_millis(5);
    config.reconnect_backoff =
      ReconnectBackoff::deterministic(Duration::from_millis(1), Duration::from_millis(5));
    let runtime = WorkerRuntime::new(ArcProtocol(protocol.clone()), config);
    let cancel = runtime.cancellation_token();

    let handle = tokio::spawn(async move { runtime.run_forever().await });

    // Wait until the runtime has clearly recovered (at least 5
    // heartbeats — 3 failed + several succeeded), then cancel.
    for _ in 0..200 {
      if protocol.heartbeat_count() >= 5 {
        break;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
      protocol.heartbeat_count() >= 5,
      "runtime did not recover from transient transport errors; \
       heartbeat_count={}",
      protocol.heartbeat_count()
    );
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(2), handle)
      .await
      .expect("runtime must exit within 2s after cancel")
      .expect("join");
    assert!(
      result.is_ok(),
      "run_forever must return Ok after cancel; got {result:?}"
    );
  }

  #[tokio::test]
  async fn run_forever_gives_up_after_max_attempts() {
    // 100 failures, but max_attempts=3 — the loop must surface the
    // error instead of retrying forever.
    let protocol = Arc::new(FlakyProtocol::new(100));
    let worker_id = WorkerId::new("worker-cap").unwrap();
    let mut config = WorkerConfig::new(worker_id, "memory://local");
    let mut backoff =
      ReconnectBackoff::deterministic(Duration::from_millis(1), Duration::from_millis(2));
    backoff.max_attempts = Some(3);
    config.reconnect_backoff = backoff;
    let runtime = WorkerRuntime::new(ArcProtocol(protocol.clone()), config);

    let result = tokio::time::timeout(Duration::from_secs(5), runtime.run_forever()).await;
    let outcome = result.expect("must not hang past max_attempts");
    assert!(
      matches!(
        outcome,
        Err(WorkerError::Scheduler(SchedulerError::Transport { .. }))
      ),
      "must surface Transport error past max_attempts; got {outcome:?}"
    );
    assert_eq!(
      protocol.heartbeat_count(),
      4,
      "max_attempts=3 allows 3 retries past the initial failure (4 total)"
    );
  }

  #[tokio::test]
  async fn run_forever_cancellation_unblocks_backoff_sleep() {
    // A long backoff sleep must yield to cancellation immediately —
    // otherwise SIGTERM would have to wait the full backoff window
    // before the runtime sees the flag.
    let protocol = Arc::new(FlakyProtocol::new(100));
    let worker_id = WorkerId::new("worker-cancel-during-backoff").unwrap();
    let mut config = WorkerConfig::new(worker_id, "memory://local");
    config.reconnect_backoff =
      ReconnectBackoff::deterministic(Duration::from_secs(60), Duration::from_secs(60));
    let runtime = WorkerRuntime::new(ArcProtocol(protocol), config);
    let cancel = runtime.cancellation_token();

    let handle = tokio::spawn(async move { runtime.run_forever().await });

    // Give the runtime time to enter the first 60s backoff sleep.
    tokio::time::sleep(Duration::from_millis(150)).await;
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(2), handle)
      .await
      .expect("cancel must interrupt the 60s backoff in well under 2s")
      .expect("join");
    assert!(
      result.is_ok(),
      "run_forever must return Ok after cancel mid-backoff; got {result:?}"
    );
  }

  // ─── Q3.3.2 free_slots parallel dispatch suite ───────────────────

  /// Protocol that holds a configurable queue of tasks and slows
  /// `report_result` by a fixed delay so we can observe whether the
  /// runtime's spawn-per-permit machinery actually achieves wall-
  /// clock concurrency. `claims_observed` + `reports_observed` are
  /// the assertion knobs.
  struct SlowReportProtocol {
    queue: std::sync::Mutex<std::collections::VecDeque<WorkerTask>>,
    report_delay: Duration,
    /// Atomic counter of completed `report_result` calls.
    reports_observed: std::sync::atomic::AtomicU32,
    /// Tracks the high-water mark of concurrent reports in flight.
    /// `run_forever` must drive this above 1 for a parallel test.
    in_flight: std::sync::atomic::AtomicU32,
    max_concurrent: std::sync::atomic::AtomicU32,
  }

  impl SlowReportProtocol {
    fn new(tasks: Vec<WorkerTask>, report_delay: Duration) -> Self {
      Self {
        queue: std::sync::Mutex::new(tasks.into()),
        report_delay,
        reports_observed: std::sync::atomic::AtomicU32::new(0),
        in_flight: std::sync::atomic::AtomicU32::new(0),
        max_concurrent: std::sync::atomic::AtomicU32::new(0),
      }
    }
  }

  #[async_trait::async_trait]
  impl WorkerProtocol for SlowReportProtocol {
    async fn submit_task(&self, _task: WorkerTask) -> Result<(), SchedulerError> {
      Ok(())
    }
    async fn claim_task(&self, _worker_id: WorkerId) -> Result<Option<WorkerTask>, SchedulerError> {
      Ok(self.queue.lock().unwrap().pop_front())
    }
    async fn report_result(
      &self,
      _worker_id: WorkerId,
      _task_id: Uuid,
      _result: WorkerTaskResult,
    ) -> Result<(), SchedulerError> {
      use std::sync::atomic::Ordering;
      // Bump in_flight + record the high water mark atomically.
      let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
      self.max_concurrent.fetch_max(now, Ordering::SeqCst);
      tokio::time::sleep(self.report_delay).await;
      self.in_flight.fetch_sub(1, Ordering::SeqCst);
      self.reports_observed.fetch_add(1, Ordering::SeqCst);
      Ok(())
    }
    async fn heartbeat(&self, _heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError> {
      Ok(())
    }
  }

  #[derive(Clone)]
  struct ArcSlowProtocol(Arc<SlowReportProtocol>);

  #[async_trait::async_trait]
  impl WorkerProtocol for ArcSlowProtocol {
    async fn submit_task(&self, task: WorkerTask) -> Result<(), SchedulerError> {
      self.0.submit_task(task).await
    }
    async fn claim_task(&self, worker_id: WorkerId) -> Result<Option<WorkerTask>, SchedulerError> {
      self.0.claim_task(worker_id).await
    }
    async fn report_result(
      &self,
      worker_id: WorkerId,
      task_id: Uuid,
      result: WorkerTaskResult,
    ) -> Result<(), SchedulerError> {
      self.0.report_result(worker_id, task_id, result).await
    }
    async fn heartbeat(&self, heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError> {
      self.0.heartbeat(heartbeat).await
    }
  }

  /// Q3.3.2 — `free_slots = 4` must actually let 4 dispatches run
  /// in parallel. The mock `report_result` sleeps 400ms and tracks
  /// the high-water concurrency mark; with `free_slots = 4` the
  /// runtime should drive the mark to 4 (or close to it) and the
  /// 4 reports should finish in ~400ms wall clock, not 4×400ms
  /// serial.
  #[tokio::test]
  async fn free_slots_4_dispatches_concurrently() {
    use std::sync::atomic::Ordering;
    let run_id = Uuid::new_v4();
    let tasks: Vec<WorkerTask> = (0..4)
      .map(|i| WorkerTask::new(run_id, format!("node-{i}"), serde_json::json!({"n": i})))
      .collect();
    let slow = Arc::new(SlowReportProtocol::new(tasks, Duration::from_millis(400)));
    let mut config = WorkerConfig::new(WorkerId::new("worker-parallel").unwrap(), "memory://local");
    config.free_slots = 4;
    config.poll_interval = Duration::from_millis(10);
    let runtime = WorkerRuntime::new(ArcSlowProtocol(slow.clone()), config);
    let cancel = runtime.cancellation_token();

    let started = std::time::Instant::now();
    let join = tokio::spawn(async move { runtime.run_forever().await });
    // Wait until the queue is drained.
    for _ in 0..200 {
      if slow.reports_observed.load(Ordering::SeqCst) >= 4 {
        break;
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let elapsed = started.elapsed();
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), join).await;

    assert_eq!(
      slow.reports_observed.load(Ordering::SeqCst),
      4,
      "all 4 tasks must be reported"
    );
    let peak = slow.max_concurrent.load(Ordering::SeqCst);
    assert!(
      peak >= 2,
      "Q3.3.2: peak in-flight reports must exceed 1 (parallel dispatch); got {peak}"
    );
    // Serial wall clock would be ≥ 4 × 400ms = 1600ms.
    // Parallel with free_slots=4 should land near 400ms; allow
    // generous headroom for spawn + scheduler scheduling.
    assert!(
      elapsed < Duration::from_millis(1200),
      "Q3.3.2: 4 × 400ms parallel reports must finish < 1.2s; took {elapsed:?}"
    );
  }

  /// Q3.3.2 — `free_slots = 1` must preserve the pre-Q3.3.2 serial
  /// behavior: peak in-flight stays at 1.
  #[tokio::test]
  async fn free_slots_1_keeps_serial_dispatch() {
    use std::sync::atomic::Ordering;
    let run_id = Uuid::new_v4();
    let tasks: Vec<WorkerTask> = (0..3)
      .map(|i| WorkerTask::new(run_id, format!("node-{i}"), serde_json::json!({})))
      .collect();
    let slow = Arc::new(SlowReportProtocol::new(tasks, Duration::from_millis(50)));
    let mut config = WorkerConfig::new(WorkerId::new("worker-serial").unwrap(), "memory://local");
    config.free_slots = 1;
    config.poll_interval = Duration::from_millis(5);
    let runtime = WorkerRuntime::new(ArcSlowProtocol(slow.clone()), config);
    let cancel = runtime.cancellation_token();
    let join = tokio::spawn(async move { runtime.run_forever().await });
    for _ in 0..200 {
      if slow.reports_observed.load(Ordering::SeqCst) >= 3 {
        break;
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), join).await;
    let peak = slow.max_concurrent.load(Ordering::SeqCst);
    assert_eq!(
      peak, 1,
      "free_slots=1 must serialize; got peak in-flight {peak}"
    );
  }

  /// Q3.3.2 — heartbeat must report dynamic available_permits, not
  /// the static config value. We capture the heartbeat
  /// `free_slots` field by intercepting via a custom protocol
  /// that records every heartbeat call.
  #[tokio::test]
  async fn heartbeat_reports_dynamic_available_permits() {
    use std::sync::atomic::Ordering;

    struct HeartbeatRecorder {
      observed_free: std::sync::Mutex<Vec<u32>>,
      block_report: tokio::sync::Notify,
      report_seen: std::sync::atomic::AtomicU32,
    }
    #[async_trait::async_trait]
    impl WorkerProtocol for HeartbeatRecorder {
      async fn submit_task(&self, _task: WorkerTask) -> Result<(), SchedulerError> {
        Ok(())
      }
      async fn claim_task(
        &self,
        _worker_id: WorkerId,
      ) -> Result<Option<WorkerTask>, SchedulerError> {
        // Only hand out one task — we want to see the heartbeat
        // drop the advertised free count once a permit is held.
        if self.report_seen.load(Ordering::SeqCst) == 0 {
          Ok(Some(WorkerTask::new(
            Uuid::new_v4(),
            "node-a",
            serde_json::json!({}),
          )))
        } else {
          Ok(None)
        }
      }
      async fn report_result(
        &self,
        _worker_id: WorkerId,
        _task_id: Uuid,
        _result: WorkerTaskResult,
      ) -> Result<(), SchedulerError> {
        // Block until the test releases us — gives us a window where
        // the permit is held so the next heartbeat sees free=1
        // instead of 2.
        self.block_report.notified().await;
        self.report_seen.fetch_add(1, Ordering::SeqCst);
        Ok(())
      }
      async fn heartbeat(&self, heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError> {
        self
          .observed_free
          .lock()
          .unwrap()
          .push(heartbeat.free_slots);
        Ok(())
      }
    }
    #[derive(Clone)]
    struct ArcRecorder(Arc<HeartbeatRecorder>);
    #[async_trait::async_trait]
    impl WorkerProtocol for ArcRecorder {
      async fn submit_task(&self, t: WorkerTask) -> Result<(), SchedulerError> {
        self.0.submit_task(t).await
      }
      async fn claim_task(&self, w: WorkerId) -> Result<Option<WorkerTask>, SchedulerError> {
        self.0.claim_task(w).await
      }
      async fn report_result(
        &self,
        w: WorkerId,
        t: Uuid,
        r: WorkerTaskResult,
      ) -> Result<(), SchedulerError> {
        self.0.report_result(w, t, r).await
      }
      async fn heartbeat(&self, h: WorkerHeartbeat) -> Result<(), SchedulerError> {
        self.0.heartbeat(h).await
      }
    }

    let recorder = Arc::new(HeartbeatRecorder {
      observed_free: std::sync::Mutex::new(Vec::new()),
      block_report: tokio::sync::Notify::new(),
      report_seen: std::sync::atomic::AtomicU32::new(0),
    });
    let mut config = WorkerConfig::new(WorkerId::new("worker-hb").unwrap(), "memory://local");
    config.free_slots = 2;
    config.poll_interval = Duration::from_millis(5);
    let runtime = WorkerRuntime::new(ArcRecorder(recorder.clone()), config);
    let cancel = runtime.cancellation_token();
    let join = tokio::spawn(async move { runtime.run_forever().await });

    // Wait long enough for run_forever to: heartbeat (free=2), claim
    // (acquires permit, now 1 left), spawn report (which blocks),
    // loop back, acquire 2nd permit, heartbeat (free=1 now), then
    // get Ok(None) on claim and idle-sleep.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let observed = recorder.observed_free.lock().unwrap().clone();
    // Note: heartbeat happens AFTER the permit is acquired inside
    // dispatch_one_with_permit, so the highest free count we
    // observe is one less than `config.free_slots`. The key
    // assertion is that free MOVES (which proves the value is
    // dynamic, not the static config) and at least one heartbeat
    // hit the lower bound.
    assert!(
      !observed.is_empty(),
      "must observe at least one heartbeat; got {observed:?}"
    );
    assert!(
      observed.iter().any(|n| *n < 2),
      "Q3.3.2: heartbeat must observe free_slots < 2 while a permit is held; got {observed:?}"
    );
    assert!(
      observed.contains(&0),
      "Q3.3.2: once both permits are held, heartbeat must report 0; got {observed:?}"
    );

    // Release the blocked report so the runtime can drain.
    recorder.block_report.notify_waiters();
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), join).await;
  }

  /// Thin Arc wrapper so we can share a single FlakyProtocol between
  /// the runtime under test and the assertions side.
  #[derive(Clone)]
  struct ArcProtocol(Arc<FlakyProtocol>);

  #[async_trait::async_trait]
  impl WorkerProtocol for ArcProtocol {
    async fn submit_task(&self, task: WorkerTask) -> Result<(), SchedulerError> {
      self.0.submit_task(task).await
    }
    async fn claim_task(&self, worker_id: WorkerId) -> Result<Option<WorkerTask>, SchedulerError> {
      self.0.claim_task(worker_id).await
    }
    async fn report_result(
      &self,
      worker_id: WorkerId,
      task_id: Uuid,
      result: WorkerTaskResult,
    ) -> Result<(), SchedulerError> {
      self.0.report_result(worker_id, task_id, result).await
    }
    async fn heartbeat(&self, heartbeat: WorkerHeartbeat) -> Result<(), SchedulerError> {
      self.0.heartbeat(heartbeat).await
    }
  }
}
