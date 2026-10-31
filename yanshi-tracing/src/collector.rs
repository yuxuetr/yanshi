//! Trace collector - implements EventListener to collect workflow traces

use crate::TraceExporter;
use crate::redaction::{RedactionConfig, redact_trace, redact_value};
use crate::storage::TraceStorage;
use crate::types::*;
use futures::FutureExt;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;
use yanshi_core::events::{EventListener, WorkflowEvent};

/// Trace collector configuration
#[derive(Debug, Clone)]
pub struct TraceConfig {
  /// Whether to capture input/output data (may contain sensitive information)
  pub capture_io: bool,

  /// Whether to capture LLM prompts
  pub capture_prompts: bool,

  /// Maximum size for input/output data in bytes (prevents huge logs)
  pub max_io_size_bytes: usize,

  /// Whether to use async storage (recommended)
  pub async_storage: bool,

  /// Behavior when storage fails
  pub on_storage_error: StorageErrorPolicy,

  /// Redaction policy applied before trace persistence and export.
  pub redaction: RedactionConfig,

  /// Q2.3.1: bounded capacity of the in-process event channel. When the
  /// drain task can't keep up (slow storage / slow exporter), `on_event`
  /// drops the newest event and bumps `events_dropped` instead of
  /// growing the queue without bound. Default 8192 events.
  pub event_channel_capacity: usize,

  /// Q2.3.2: per-exporter timeout for `TraceExporter::export_trace`.
  /// A stuck OTLP sink no longer blocks the drain task for every other
  /// workflow's events. Default 10 seconds.
  pub exporter_timeout: std::time::Duration,
}

impl Default for TraceConfig {
  fn default() -> Self {
    Self {
      capture_io: true,
      capture_prompts: true,
      max_io_size_bytes: 1024 * 1024, // 1MB
      async_storage: true,
      on_storage_error: StorageErrorPolicy::LogError,
      redaction: RedactionConfig::default(),
      event_channel_capacity: 8192,
      exporter_timeout: std::time::Duration::from_secs(10),
    }
  }
}

impl TraceConfig {
  /// Production configuration (more restrictive)
  pub fn production() -> Self {
    Self {
      capture_io: false, // Don't capture sensitive data
      capture_prompts: false,
      max_io_size_bytes: 0,
      async_storage: true,
      on_storage_error: StorageErrorPolicy::LogError,
      redaction: RedactionConfig::default(),
      event_channel_capacity: 8192,
      exporter_timeout: std::time::Duration::from_secs(10),
    }
  }

  /// Development configuration (full tracing)
  pub fn development() -> Self {
    Self {
      capture_io: true,
      capture_prompts: true,
      max_io_size_bytes: 10 * 1024 * 1024, // 10MB
      async_storage: true,
      on_storage_error: StorageErrorPolicy::Ignore,
      redaction: RedactionConfig::default().with_max_value_bytes(10 * 1024 * 1024),
      event_channel_capacity: 8192,
      exporter_timeout: std::time::Duration::from_secs(10),
    }
  }
}

/// Storage error handling policy.
///
/// There is deliberately no "fail the workflow" variant: traces are
/// persisted by a background drain task after the workflow has already
/// produced its result, and `EventListener` has no channel back into
/// `Flow`. The former `FailWorkflow` variant only panicked inside that
/// task, where `catch_unwind` swallowed it (eval 2026-10-07 L7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageErrorPolicy {
  /// Ignore storage errors silently
  Ignore,

  /// Log error but continue execution
  LogError,
}

/// Trace collector - collects execution traces by listening to workflow events
pub struct TraceCollector {
  /// Storage backend
  storage: Arc<dyn TraceStorage>,

  /// Configuration
  config: TraceConfig,

  /// Currently running traces (in-memory)
  current_traces: Arc<RwLock<HashMap<String, ExecutionTrace>>>,

  /// Pending LLM prompts (workflow_id, node_id) -> LLMTrace
  pending_llm: Arc<RwLock<HashMap<(String, String), LLMTrace>>>,

  /// Exporters invoked once a workflow trace reaches a terminal state.
  exporters: Vec<Arc<dyn TraceExporter>>,

  /// Lazily-initialised drain channel. Sender side is populated on the
  /// first event (so we don't require a tokio runtime at construction
  /// time); a single dedicated task drains the receiver in arrival
  /// order. Without this, every `on_event` call previously spun its own
  /// `tokio::spawn`, and the resulting tasks raced for `current_traces`
  /// — `WorkflowCompleted` would sometimes save the trace before an
  /// earlier `NodeCompleted` finished updating the node row, yielding
  /// `status=running` on the final node in TUI/replay output.
  /// Q2.2.3: each entry is `(captured_traceparent, event)`. The
  /// traceparent is read SYNCHRONOUSLY from the task-local in
  /// `on_event`, so we capture the producer's W3C context before the
  /// event hops to the drain task (which runs outside the scope).
  ///
  /// Q2.3.1: bounded channel sized at `config.event_channel_capacity`.
  /// `on_event` uses `try_send`; on a full queue the newest event is
  /// dropped and `events_dropped` is bumped.
  drain_tx: std::sync::OnceLock<tokio::sync::mpsc::Sender<(Option<String>, WorkflowEvent)>>,

  /// Q2.3.1: monotonic count of events dropped because the bounded
  /// channel was full. Producers (workflows) keep running but tracing
  /// is best-effort under pressure; the counter is the observability
  /// signal that ingest is falling behind.
  events_dropped: Arc<std::sync::atomic::AtomicU64>,

  /// Q2.2.1: flipped to `true` if the drain task observes too many
  /// consecutive panics or fatal errors. `on_event` consults this flag
  /// and drops further sends so the unbounded channel doesn't keep
  /// growing into a dead sink. Producers see no error (event tracing is
  /// best-effort), but the in-process logs surface the panic loudly via
  /// `tracing::error!` so the failure isn't silent.
  drain_poisoned: Arc<AtomicBool>,

  /// Q3.1.2: total events successfully placed into the drain channel
  /// (i.e. `try_send` returned Ok). Paired with `processed_count` to
  /// power [`Self::flush`]: callers (notably the CLI Ctrl-C path) can
  /// wait for the drain task to catch up before the process exits.
  submitted_count: Arc<std::sync::atomic::AtomicU64>,

  /// Q3.1.2: total events fully processed by the drain task. Incremented
  /// after each `process_event` returns, regardless of whether the
  /// underlying storage write succeeded or panicked (the catch_unwind
  /// branch counts too — the event left the queue).
  processed_count: Arc<std::sync::atomic::AtomicU64>,
}

impl TraceCollector {
  /// Create a new trace collector
  pub fn new(storage: Arc<dyn TraceStorage>, config: TraceConfig) -> Self {
    Self {
      storage,
      config,
      current_traces: Arc::new(RwLock::new(HashMap::new())),
      pending_llm: Arc::new(RwLock::new(HashMap::new())),
      exporters: Vec::new(),
      drain_tx: std::sync::OnceLock::new(),
      drain_poisoned: Arc::new(AtomicBool::new(false)),
      events_dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
      submitted_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
      processed_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    }
  }

  /// Q2.3.1: monotonic count of events dropped because the bounded
  /// trace-event channel was full. Visible for tests, dashboards, and
  /// `yanshi trace doctor` integrations.
  pub fn events_dropped(&self) -> u64 {
    self.events_dropped.load(Ordering::SeqCst)
  }

  /// Q2.2.1: visible for tests + diagnostics. Returns `true` once the
  /// drain task has terminated (typically because too many panics in a
  /// row); further `on_event` calls become no-ops.
  pub fn is_drain_poisoned(&self) -> bool {
    self.drain_poisoned.load(Ordering::SeqCst)
  }

  /// Q3.1.2: wait for every event that was successfully submitted into
  /// the drain channel to be processed, up to `timeout`. Returns `true`
  /// if the drain caught up, `false` if the timeout expired with events
  /// still in flight.
  ///
  /// **Why:** the trace collector is async — `on_event` returns as soon
  /// as the event is queued, but the storage write happens on a
  /// separate drain task. When the CLI receives Ctrl-C and is about to
  /// exit, "the JSONL file is complete" requires the drain task to have
  /// caught up. Without this, `yanshi trace tui <id>` could read a
  /// half-written file missing the final `WorkflowCancelled` event.
  ///
  /// **How to apply:** call this immediately before `std::process::exit`
  /// or before dropping the last `Arc<TraceCollector>` clone. Pass a
  /// generous timeout (a few seconds) — the drain task is normally only
  /// blocked when storage is unusually slow.
  pub async fn flush(&self, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
      let submitted = self.submitted_count.load(Ordering::SeqCst);
      let processed = self.processed_count.load(Ordering::SeqCst);
      if processed >= submitted {
        return true;
      }
      if self.drain_poisoned.load(Ordering::SeqCst) {
        // Drain task is dead; processed will never catch up. Surface
        // that as a failed drain rather than spinning until timeout.
        return false;
      }
      if std::time::Instant::now() >= deadline {
        return false;
      }
      tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
  }

  /// Q3.1.2: count of events successfully placed into the drain
  /// channel. Visible for diagnostics + the `flush` regression test.
  pub fn submitted_count(&self) -> u64 {
    self.submitted_count.load(Ordering::SeqCst)
  }

  /// Q3.1.2: count of events the drain task has fully processed.
  /// Equals [`Self::submitted_count`] once the drain has caught up.
  pub fn processed_count(&self) -> u64 {
    self.processed_count.load(Ordering::SeqCst)
  }

  /// Attach an exporter for completed or failed traces.
  pub fn with_exporter(mut self, exporter: Arc<dyn TraceExporter>) -> Self {
    self.exporters.push(exporter);
    self
  }

  /// Get a trace by workflow ID
  pub async fn get_trace(
    &self,
    workflow_id: &str,
  ) -> Result<Option<ExecutionTrace>, anyhow::Error> {
    // Check in-memory first (running workflows)
    {
      let traces = self.current_traces.read().await;
      if let Some(trace) = traces.get(workflow_id) {
        return Ok(Some(trace.clone()));
      }
    }

    // Check storage (completed workflows)
    self.storage.get_trace(workflow_id).await
  }

  /// Query traces with filters
  pub async fn query_traces(
    &self,
    query: crate::storage::TraceQuery,
  ) -> Result<Vec<ExecutionTrace>, anyhow::Error> {
    self.storage.query_traces(query).await
  }

  /// List all currently running workflows
  pub async fn list_running(&self) -> Vec<ExecutionTrace> {
    let traces = self.current_traces.read().await;
    traces.values().cloned().collect()
  }

  /// Process an event asynchronously
  async fn process_event(
    storage: Arc<dyn TraceStorage>,
    traces: Arc<RwLock<HashMap<String, ExecutionTrace>>>,
    pending_llm: Arc<RwLock<HashMap<(String, String), LLMTrace>>>,
    config: TraceConfig,
    exporters: Vec<Arc<dyn TraceExporter>>,
    event: WorkflowEvent,
  ) -> Result<(), anyhow::Error> {
    match event {
      WorkflowEvent::WorkflowStarted {
        workflow_id,
        timestamp: _,
      } => {
        let mut trace = ExecutionTrace::new(workflow_id.clone());
        trace.metadata.environment =
          std::env::var("YANSHI_ENV").unwrap_or_else(|_| "development".to_string());

        // Q2.2.3: honor inbound W3C `traceparent`. When the workflow runs
        // inside `crate::context::scope(...)` (typically because the
        // server gateway installed a parent context off the incoming
        // HTTP header), record the upstream trace_id + span_id so the
        // OTel exporter stitches our spans into that trace instead of
        // generating a fresh, orphaned trace. Without this, CLAUDE.md's
        // "W3C traceparent propagation" was outbound-only.
        if let Some(tp) = crate::context::current_traceparent()
          && let Some((external_trace_id, external_parent_span_id)) = parse_traceparent(&tp)
        {
          trace.metadata.external_trace_id = Some(external_trace_id);
          trace.metadata.external_parent_span_id = Some(external_parent_span_id);
        }

        traces.write().await.insert(workflow_id, trace);
      }

      WorkflowEvent::NodeStarted {
        workflow_id,
        node_id,
        timestamp: _,
      } => {
        let mut traces_guard = traces.write().await;
        if let Some(trace) = traces_guard.get_mut(&workflow_id) {
          // Q2.3.7: if a prior attempt of this node id is still "Running"
          // (because its terminal NodeCompleted / NodeFailed was lost,
          // or this is a retry without an interleaved completion event),
          // close it out as Failed so the persisted trace doesn't carry
          // a phantom never-finishing row. Retry / loop / Map sub-node
          // scenarios are the typical sources.
          for prior in trace
            .nodes
            .iter_mut()
            .filter(|n| n.node_id == node_id && n.status == NodeStatus::Running)
          {
            prior.fail("superseded by new attempt".to_string());
          }

          // Extract node_type from node_id (format: "type:id" or just "id")
          let node_type = node_id.split(':').next().unwrap_or("Unknown").to_string();

          let mut node_trace = NodeTrace::new(node_id, node_type);
          node_trace.context =
            crate::TraceContext::child(&trace.context, format!("node:{}", node_trace.node_id));
          trace.nodes.push(node_trace);
        }
      }

      WorkflowEvent::NodeCompleted {
        workflow_id,
        node_id,
        duration,
        timestamp: _,
      } => {
        let mut traces_guard = traces.write().await;
        if let Some(trace) = traces_guard.get_mut(&workflow_id)
          && let Some(node) = trace
            .nodes
            .iter_mut()
            .rev()
            .find(|n| n.node_id == node_id && n.status == NodeStatus::Running)
        {
          // Q2.3.7: only match the still-open row; if all rows for this
          // node id are already terminal, a stale event arrives and we
          // skip it instead of silently overwriting a closed attempt.
          node.complete();
          node.duration_ms = Some(duration.as_millis() as u64);
        }
      }

      WorkflowEvent::NodeOutputCaptured {
        workflow_id,
        node_id,
        mut output,
        timestamp: _,
      } => {
        if config.capture_io
          && let Err(e) = Self::limit_value_size(&mut output, config.max_io_size_bytes)
        {
          Self::handle_storage_error(&config, e);
        }

        let mut agent_details = output
          .get("agent_result")
          .and_then(AgentTrace::from_agent_result);

        let mut traces_guard = traces.write().await;
        if let Some(trace) = traces_guard.get_mut(&workflow_id)
          && let Some(node) = trace
            .nodes
            .iter_mut()
            .rev()
            .find(|n| n.node_id == node_id && n.status == NodeStatus::Running)
        {
          // Q2.3.7: attach output to the open attempt only.
          if config.capture_io {
            node.output = Some(output);
          }
          if let Some(agent) = &mut agent_details {
            agent.attach_context(&node.context);
          }
          node.agent_details = agent_details;
        }
      }

      WorkflowEvent::NodeFailed {
        workflow_id,
        node_id,
        error,
        duration,
        timestamp: _,
      } => {
        let mut traces_guard = traces.write().await;
        if let Some(trace) = traces_guard.get_mut(&workflow_id)
          && let Some(node) = trace
            .nodes
            .iter_mut()
            .rev()
            .find(|n| n.node_id == node_id && n.status == NodeStatus::Running)
        {
          // Q2.3.7: only fail the still-open attempt.
          node.fail(error);
          node.duration_ms = Some(duration.as_millis() as u64);
        }
      }

      WorkflowEvent::NodeSkipped {
        workflow_id,
        node_id,
        reason,
        timestamp: _,
      } => {
        let mut traces_guard = traces.write().await;
        if let Some(trace) = traces_guard.get_mut(&workflow_id)
          && let Some(node) = trace
            .nodes
            .iter_mut()
            .rev()
            .find(|n| n.node_id == node_id && n.status == NodeStatus::Running)
        {
          // Q2.3.7: only mark the open attempt skipped.
          node.status = NodeStatus::Skipped;
          node.error = Some(format!("Skipped: {}", reason));
        }
      }

      WorkflowEvent::LLMPromptSent {
        workflow_id,
        node_id,
        model,
        provider,
        system_prompt,
        user_prompt,
        temperature,
        max_tokens,
        timestamp: _,
      } => {
        // Create LLM trace (will be filled in when response received)
        let llm_trace = LLMTrace {
          model,
          provider,
          system_prompt: if config.capture_prompts {
            system_prompt
          } else {
            None
          },
          user_prompt: if config.capture_prompts {
            user_prompt
          } else {
            "[REDACTED]".to_string()
          },
          response: String::new(), // Will be filled later
          temperature,
          max_tokens,
          usage: None,   // Will be filled later
          latency_ms: 0, // Will be calculated later
        };

        // Store in pending LLM map
        pending_llm
          .write()
          .await
          .insert((workflow_id, node_id), llm_trace);
      }

      WorkflowEvent::LLMResponseReceived {
        workflow_id,
        node_id,
        model: _,
        response,
        usage,
        duration,
        timestamp: _,
      } => {
        // Get and remove pending LLM trace
        let llm_trace_opt = pending_llm
          .write()
          .await
          .remove(&(workflow_id.clone(), node_id.clone()));

        if let Some(mut llm_trace) = llm_trace_opt {
          // Fill in response details
          llm_trace.response = response;
          llm_trace.latency_ms = duration.as_millis() as u64;
          llm_trace.usage = usage.map(|u| TokenUsage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
            estimated_cost_usd: None, // Could calculate based on model pricing
          });

          // Attach to node trace
          let mut traces_guard = traces.write().await;
          if let Some(trace) = traces_guard.get_mut(&workflow_id)
            && let Some(node) = trace
              .nodes
              .iter_mut()
              .rev()
              .find(|n| n.node_id == node_id && n.status == NodeStatus::Running)
          {
            // Q2.3.7: LLM response belongs to the still-open attempt.
            node.llm_details = Some(llm_trace);
          }
        }
      }

      WorkflowEvent::WorkflowCompleted {
        workflow_id,
        duration: _,
        timestamp: _,
      } => {
        let mut traces_guard = traces.write().await;
        if let Some(mut trace) = traces_guard.remove(&workflow_id) {
          trace.completed_at = Some(chrono::Utc::now());
          trace.status = TraceStatus::Completed;

          let trace = Self::prepare_terminal_trace(trace, &config);

          // Save to storage
          if let Err(e) = storage.save_trace(&trace).await {
            Self::handle_storage_error(&config, e);
          }
          Self::export_trace_to_sinks(&config, &exporters, &trace).await;
        }
      }

      WorkflowEvent::WorkflowFailed {
        workflow_id,
        error,
        duration: _,
        timestamp: _,
      } => {
        let mut traces_guard = traces.write().await;
        if let Some(mut trace) = traces_guard.remove(&workflow_id) {
          trace.completed_at = Some(chrono::Utc::now());
          trace.status = TraceStatus::Failed { error };

          let trace = Self::prepare_terminal_trace(trace, &config);

          // Save to storage
          if let Err(e) = storage.save_trace(&trace).await {
            Self::handle_storage_error(&config, e);
          }
          Self::export_trace_to_sinks(&config, &exporters, &trace).await;
        }
      }

      // Q3.1.2: persist the cancellation as a terminal status so the
      // JSONL trace file reflects the run that the CLI Ctrl-C handler
      // just aborted. Before this branch the event was silently
      // dropped and `yanshi trace tui` would show the workflow as
      // perpetually "Running".
      WorkflowEvent::WorkflowCancelled {
        workflow_id,
        reason,
        duration: _,
        timestamp: _,
      } => {
        let mut traces_guard = traces.write().await;
        if let Some(mut trace) = traces_guard.remove(&workflow_id) {
          trace.completed_at = Some(chrono::Utc::now());
          trace.status = TraceStatus::Cancelled {
            reason: reason.clone(),
          };

          let trace = Self::prepare_terminal_trace(trace, &config);

          if let Err(e) = storage.save_trace(&trace).await {
            Self::handle_storage_error(&config, e);
          }
          Self::export_trace_to_sinks(&config, &exporters, &trace).await;
        }
      }

      // Other events can be ignored for now
      _ => {}
    }

    Ok(())
  }

  async fn export_trace_to_sinks(
    config: &TraceConfig,
    exporters: &[Arc<dyn TraceExporter>],
    trace: &ExecutionTrace,
  ) {
    // Q2.3.2: wrap every exporter call in a timeout so one stuck OTLP
    // endpoint can't block the drain task (and every other workflow's
    // events) indefinitely. Errors (including timeout) route through
    // the configured StorageErrorPolicy.
    for exporter in exporters {
      let export_fut = exporter.export_trace(trace);
      match tokio::time::timeout(config.exporter_timeout, export_fut).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => Self::handle_storage_error(config, e),
        Err(_elapsed) => Self::handle_storage_error(
          config,
          anyhow::anyhow!(
            "trace exporter timed out after {:?}",
            config.exporter_timeout
          ),
        ),
      }
    }
  }

  fn prepare_terminal_trace(mut trace: ExecutionTrace, config: &TraceConfig) -> ExecutionTrace {
    redact_trace(&mut trace, &config.redaction);
    trace
  }

  /// Handle storage errors according to policy
  fn handle_storage_error(config: &TraceConfig, error: anyhow::Error) {
    match config.on_storage_error {
      StorageErrorPolicy::Ignore => {
        // Do nothing
      }
      StorageErrorPolicy::LogError => {
        eprintln!("Trace storage error: {}", error);
      }
    }
  }

  /// Limit captured value size before it is attached to a node.
  #[allow(dead_code)]
  fn limit_value_size(value: &mut serde_json::Value, max_size: usize) -> Result<(), anyhow::Error> {
    redact_value(value, &RedactionConfig::default());
    let json_str = serde_json::to_string(value)?;
    if json_str.len() > max_size {
      *value = serde_json::Value::String(format!("[TRUNCATED: {} bytes]", json_str.len()));
    }

    Ok(())
  }
}

/// Q2.2.3: parse a W3C `traceparent` into `(trace_id, parent_span_id)`. The
/// header has the form `00-<32-hex trace_id>-<16-hex span_id>-<2-hex flags>`.
/// Returns `None` for anything that doesn't match the version-00 shape; we
/// don't attempt to interpret newer versions (per the W3C compatibility
/// rule: "MUST be ignored if version is unknown but in a future format").
fn parse_traceparent(header: &str) -> Option<(String, String)> {
  let parts: Vec<&str> = header.split('-').collect();
  if parts.len() != 4 {
    return None;
  }
  if parts[0] != "00" {
    return None;
  }
  let trace_id = parts[1];
  let span_id = parts[2];
  if trace_id.len() != 32 || span_id.len() != 16 {
    return None;
  }
  if !trace_id.chars().all(|c| c.is_ascii_hexdigit())
    || !span_id.chars().all(|c| c.is_ascii_hexdigit())
  {
    return None;
  }
  if trace_id.chars().all(|c| c == '0') || span_id.chars().all(|c| c == '0') {
    return None;
  }
  Some((trace_id.to_string(), span_id.to_string()))
}

/// Q2.2.1: format an unwound panic payload into a printable message for the
/// drain task's error log. Mirrors `std::panic::PanicInfo` formatting.
fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
  if let Some(s) = payload.downcast_ref::<&'static str>() {
    return (*s).to_string();
  }
  if let Some(s) = payload.downcast_ref::<String>() {
    return s.clone();
  }
  "<non-string panic payload>".to_string()
}

impl EventListener for TraceCollector {
  fn on_event(&self, event: &WorkflowEvent) {
    // Clone what we need for async task
    let storage = self.storage.clone();
    let traces = self.current_traces.clone();
    let pending_llm = self.pending_llm.clone();
    let config = self.config.clone();
    let exporters = self.exporters.clone();
    let event = event.clone();

    // Spawn async task to process event (non-blocking)
    if self.config.async_storage {
      // Q2.2.1: short-circuit when the drain task has poisoned itself
      // after repeated panics. Without this guard the unbounded channel
      // would grow without bound (the receiver is dead), turning a one-
      // time bug into a memory leak.
      if self.drain_poisoned.load(Ordering::SeqCst) {
        return;
      }
      // Lazily set up a single drain task on first use so events are
      // applied to `current_traces` in arrival order. The previous
      // `tokio::spawn` per event raced across tasks for the same
      // workflow's RwLock — see field-level comment on `drain_tx`.
      let tx = self.drain_tx.get_or_init(|| {
        // Q2.3.1: bounded channel — `try_send` failures drop the
        // newest event and bump `events_dropped`, so a slow consumer
        // cannot grow the queue indefinitely.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(Option<String>, WorkflowEvent)>(
          self.config.event_channel_capacity.max(1),
        );
        let storage_drain = storage.clone();
        let traces_drain = traces.clone();
        let pending_drain = pending_llm.clone();
        let config_drain = config.clone();
        let exporters_drain = exporters.clone();
        let poisoned_drain = self.drain_poisoned.clone();
        let processed_drain = self.processed_count.clone();
        tokio::spawn(async move {
          // Q2.2.1: each event handler is wrapped in `catch_unwind` so a
          // panic in `process_event` / exporter / storage no longer kills
          // the drain task and silently swallows every subsequent event.
          // Consecutive panics beyond CONSECUTIVE_PANIC_BUDGET poison the
          // drain so producers stop filling the channel.
          const CONSECUTIVE_PANIC_BUDGET: u32 = 16;
          let mut consecutive_panics: u32 = 0;
          while let Some((captured_tp, ev)) = rx.recv().await {
            let storage_for_event = storage_drain.clone();
            let traces_for_event = traces_drain.clone();
            let pending_for_event = pending_drain.clone();
            let config_for_event = config_drain.clone();
            let exporters_for_event = exporters_drain.clone();
            // Q2.2.3: re-install the producer's traceparent for the
            // duration of `process_event`. The drain task itself does
            // not inherit task-locals, so we restore the scope here so
            // `current_traceparent()` inside `process_event` sees the
            // upstream context.
            let fut = async move {
              let work = async {
                if let Err(e) = Self::process_event(
                  storage_for_event,
                  traces_for_event,
                  pending_for_event,
                  config_for_event.clone(),
                  exporters_for_event,
                  ev,
                )
                .await
                {
                  Self::handle_storage_error(&config_for_event, e);
                }
              };
              match captured_tp {
                Some(tp) => crate::context::scope(tp, work).await,
                None => work.await,
              }
            };
            let outcome = AssertUnwindSafe(fut).catch_unwind().await;
            // Q3.1.2: count this event as processed regardless of
            // whether the storage write succeeded or the handler
            // panicked — the event has left the channel either way.
            // `flush()` callers care about queue drain, not write
            // success (storage errors are surfaced through their own
            // policy path).
            processed_drain.fetch_add(1, Ordering::SeqCst);
            match outcome {
              Ok(()) => consecutive_panics = 0,
              Err(payload) => {
                consecutive_panics += 1;
                let msg = panic_payload_message(&payload);
                tracing::error!(
                  consecutive_panics,
                  panic = %msg,
                  "trace collector drain task caught panic; continuing"
                );
                if consecutive_panics >= CONSECUTIVE_PANIC_BUDGET {
                  tracing::error!(
                    panic_budget = CONSECUTIVE_PANIC_BUDGET,
                    "trace collector drain task poisoning itself after repeated panics; dropping further events"
                  );
                  poisoned_drain.store(true, Ordering::SeqCst);
                  return;
                }
              }
            }
          }
        });
        tx
      });
      // Q2.2.3: capture the producer's W3C `traceparent` *now* (we are
      // still synchronously inside the producer's task-local scope).
      let captured_tp = crate::context::current_traceparent();
      // Q2.3.1: non-blocking send — when the bounded queue is full, drop
      // the newest event and bump the dropped counter. Producers must
      // never block on tracing.
      match tx.try_send((captured_tp, event)) {
        Ok(()) => {
          // Q3.1.2: bump the submitted counter so `flush()` can wait
          // for processed >= submitted before the CLI exits.
          self.submitted_count.fetch_add(1, Ordering::SeqCst);
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
          let prev = self.events_dropped.fetch_add(1, Ordering::SeqCst);
          // Log at every power-of-two boundary so a slow consumer
          // leaves a visible breadcrumb without flooding logs.
          if prev == 0 || (prev + 1).is_power_of_two() {
            tracing::warn!(
              dropped_total = prev + 1,
              capacity = self.config.event_channel_capacity,
              "trace event channel full; dropping events (best-effort tracing under backpressure)"
            );
          }
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
          // Receiver was dropped — collector going away. Best effort.
        }
      }
    } else {
      // Blocking mode (for testing or special cases)
      let rt = tokio::runtime::Handle::current();
      rt.block_on(async {
        if let Err(e) = Self::process_event(
          storage,
          traces,
          pending_llm,
          config.clone(),
          exporters,
          event,
        )
        .await
        {
          Self::handle_storage_error(&config, e);
        }
      });
      // Q3.1.2: in blocking mode the event is processed before
      // `on_event` returns, so submitted == processed always holds.
      // Bump both counters together to keep `flush()` consistent.
      self.submitted_count.fetch_add(1, Ordering::SeqCst);
      self.processed_count.fetch_add(1, Ordering::SeqCst);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::storage::file::FileTraceStorage;
  use async_trait::async_trait;
  use std::time::Duration as StdDuration;
  use tempfile::tempdir;

  #[derive(Default)]
  struct RecordingTraceExporter {
    workflow_ids: tokio::sync::Mutex<Vec<String>>,
    traces: tokio::sync::Mutex<Vec<ExecutionTrace>>,
  }

  #[async_trait]
  impl TraceExporter for RecordingTraceExporter {
    async fn export_trace(&self, trace: &ExecutionTrace) -> Result<(), anyhow::Error> {
      self
        .workflow_ids
        .lock()
        .await
        .push(trace.workflow_id.clone());
      self.traces.lock().await.push(trace.clone());
      Ok(())
    }
  }

  #[tokio::test]
  async fn test_trace_collector_workflow_lifecycle() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let config = TraceConfig::development();
    let collector = TraceCollector::new(storage.clone(), config);

    let workflow_id = "test-wf-1".to_string();

    // Start workflow
    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: workflow_id.clone(),
      timestamp: std::time::Instant::now(),
    });

    // Give async task time to process
    tokio::time::sleep(StdDuration::from_millis(50)).await;

    // Check trace exists
    let trace = collector.get_trace(&workflow_id).await.unwrap();
    assert!(trace.is_some());
    assert!(trace.unwrap().is_running());

    // Complete workflow
    collector.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: workflow_id.clone(),
      duration: StdDuration::from_secs(5),
      timestamp: std::time::Instant::now(),
    });

    tokio::time::sleep(StdDuration::from_millis(100)).await;

    // Check trace is completed and stored
    let trace = storage.get_trace(&workflow_id).await.unwrap();
    assert!(trace.is_some());
    assert!(trace.unwrap().is_completed());
  }

  #[tokio::test]
  async fn test_trace_collector_node_tracking() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let collector = TraceCollector::new(storage, TraceConfig::development());

    let workflow_id = "test-wf-2".to_string();
    let node_id = "node1".to_string();

    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: workflow_id.clone(),
      timestamp: std::time::Instant::now(),
    });

    tokio::time::sleep(StdDuration::from_millis(50)).await;

    collector.on_event(&WorkflowEvent::NodeStarted {
      workflow_id: workflow_id.clone(),
      node_id: node_id.clone(),
      timestamp: std::time::Instant::now(),
    });

    tokio::time::sleep(StdDuration::from_millis(50)).await;

    collector.on_event(&WorkflowEvent::NodeCompleted {
      workflow_id: workflow_id.clone(),
      node_id: node_id.clone(),
      duration: StdDuration::from_millis(100),
      timestamp: std::time::Instant::now(),
    });

    tokio::time::sleep(StdDuration::from_millis(50)).await;

    let trace = collector.get_trace(&workflow_id).await.unwrap().unwrap();
    assert_eq!(trace.nodes.len(), 1);
    assert_eq!(trace.nodes[0].node_id, node_id);
    assert_eq!(trace.nodes[0].status, NodeStatus::Completed);
  }

  #[tokio::test]
  async fn test_trace_collector_exports_terminal_trace() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let exporter = Arc::new(RecordingTraceExporter::default());
    let collector =
      TraceCollector::new(storage, TraceConfig::development()).with_exporter(exporter.clone());

    let workflow_id = "test-wf-export".to_string();
    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: workflow_id.clone(),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: workflow_id.clone(),
      duration: StdDuration::from_millis(5),
      timestamp: std::time::Instant::now(),
    });

    tokio::time::sleep(StdDuration::from_millis(100)).await;

    let exported = exporter.workflow_ids.lock().await;
    assert_eq!(exported.as_slice(), &[workflow_id]);
  }

  #[tokio::test]
  async fn test_trace_collector_redacts_terminal_trace_before_storage_and_export() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let exporter = Arc::new(RecordingTraceExporter::default());
    let collector = TraceCollector::new(storage.clone(), TraceConfig::development())
      .with_exporter(exporter.clone());

    let workflow_id = "test-wf-redact".to_string();
    let node_id = "agent_node".to_string();
    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: workflow_id.clone(),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::NodeStarted {
      workflow_id: workflow_id.clone(),
      node_id: node_id.clone(),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::NodeOutputCaptured {
      workflow_id: workflow_id.clone(),
      node_id: node_id.clone(),
      output: serde_json::json!({
        "agent_result": {
          "session_id": "session-1",
          "answer": "done",
          "stop_reason": {"reason": "final_answer"},
          "steps": [
            {
              "index": 0,
              "kind": {
                "type": "tool_call",
                "tool": "http",
                "params": {
                  "url": "https://example.test",
                  "headers": {"Authorization": "Bearer secret"},
                  "api_key": "secret"
                }
              }
            }
          ],
          "events": []
        }
      }),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: workflow_id.clone(),
      duration: StdDuration::from_millis(5),
      timestamp: std::time::Instant::now(),
    });

    tokio::time::sleep(StdDuration::from_millis(100)).await;

    let stored = storage
      .get_trace(&workflow_id)
      .await
      .unwrap()
      .expect("stored trace");
    let stored_agent = stored.nodes[0].agent_details.as_ref().expect("agent trace");
    assert_eq!(
      stored_agent.steps[0]["kind"]["params"]["api_key"],
      serde_json::json!("[REDACTED]")
    );
    assert_eq!(
      stored_agent.steps[0]["kind"]["params"]["headers"]["Authorization"],
      serde_json::json!("[REDACTED]")
    );

    let exported = exporter.traces.lock().await;
    let exported_agent = exported[0].nodes[0]
      .agent_details
      .as_ref()
      .expect("exported agent trace");
    assert_eq!(
      exported_agent.steps[0]["kind"]["params"]["api_key"],
      serde_json::json!("[REDACTED]")
    );
  }

  #[tokio::test]
  async fn test_trace_collector_links_agent_tool_and_mcp_output() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let collector = TraceCollector::new(storage, TraceConfig::development());

    let workflow_id = "test-wf-agent".to_string();
    let node_id = "agent_node".to_string();

    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: workflow_id.clone(),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::NodeStarted {
      workflow_id: workflow_id.clone(),
      node_id: node_id.clone(),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::NodeOutputCaptured {
      workflow_id: workflow_id.clone(),
      node_id: node_id.clone(),
      output: serde_json::json!({
        "response": "done",
        "agent_result": {
          "session_id": "session-1",
          "answer": "done",
          "stop_reason": {"reason": "final_answer"},
          "steps": [
            {
              "index": 0,
              "kind": {
                "type": "tool_call",
                "tool": "mcp_fixture_echo",
                "params": {"message": "hello"}
              }
            }
          ],
          "events": [
            {
              "event": "tool_call_completed",
              "tool": "mcp_fixture_echo",
              "source": "mcp",
              "permissions": ["mcp", "network"],
              "is_error": false,
              "duration_ms": 12
            }
          ]
        }
      }),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::NodeCompleted {
      workflow_id: workflow_id.clone(),
      node_id: node_id.clone(),
      duration: StdDuration::from_millis(20),
      timestamp: std::time::Instant::now(),
    });

    tokio::time::sleep(StdDuration::from_millis(100)).await;

    let trace = collector.get_trace(&workflow_id).await.unwrap().unwrap();
    assert_eq!(trace.context.run_id, workflow_id);
    assert_eq!(trace.context.span_id, "workflow");
    let node = &trace.nodes[0];
    assert_eq!(node.context.parent_span_id.as_deref(), Some("workflow"));
    assert_eq!(node.context.span_id, "node:agent_node");
    let agent = node.agent_details.as_ref().expect("agent trace");
    assert_eq!(
      agent.context.parent_span_id.as_deref(),
      Some("node:agent_node")
    );
    assert_eq!(agent.context.span_id, "agent:session-1");
    assert_eq!(agent.session_id, "session-1");
    assert_eq!(agent.tool_calls.len(), 1);
    assert_eq!(agent.tool_calls[0].tool, "mcp_fixture_echo");
    assert_eq!(agent.tool_calls[0].source.as_deref(), Some("mcp"));
    assert_eq!(
      agent.tool_calls[0].permissions,
      vec!["mcp".to_string(), "network".to_string()]
    );
    assert_eq!(
      agent.tool_calls[0].context.parent_span_id.as_deref(),
      Some("agent:session-1")
    );
    assert_eq!(
      agent.tool_calls[0].context.span_id,
      "tool:0:mcp_fixture_echo"
    );
    assert!(agent.tool_calls[0].is_mcp);
    assert_eq!(agent.tool_calls[0].is_error, Some(false));
    assert_eq!(agent.tool_calls[0].duration_ms, Some(12));
    assert!(node.output.is_some());
  }

  // Q2.2.3: parse_traceparent rejects malformed inputs and accepts the
  // canonical version-00 form. Without these guards we'd happily install
  // a garbage trace_id from a typo'd header.
  #[test]
  fn parse_traceparent_accepts_canonical_v00() {
    let (trace_id, span_id) =
      parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        .expect("canonical traceparent must parse");
    assert_eq!(trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(span_id, "00f067aa0ba902b7");
  }

  #[test]
  fn parse_traceparent_rejects_unknown_version() {
    assert!(
      parse_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").is_none(),
      "version != 00 must be rejected (W3C: ignore unknown version)"
    );
  }

  #[test]
  fn parse_traceparent_rejects_all_zero_ids() {
    assert!(
      parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none(),
      "all-zero trace_id is forbidden by W3C"
    );
    assert!(
      parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01").is_none(),
      "all-zero span_id is forbidden by W3C"
    );
  }

  #[test]
  fn parse_traceparent_rejects_wrong_length_or_non_hex() {
    assert!(parse_traceparent("00-deadbeef-00f067aa0ba902b7-01").is_none());
    assert!(parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-XYZ-01").is_none());
    assert!(parse_traceparent("not a traceparent").is_none());
  }

  // Q2.2.3 integration — when WorkflowStarted runs inside
  // `crate::context::scope(traceparent)`, the resulting ExecutionTrace
  // must carry the upstream trace_id, and `trace_to_spans` must emit
  // spans under that id (stitching cross-service traces).
  #[tokio::test]
  async fn workflow_started_inherits_inbound_traceparent() {
    use crate::otel::{OtelExporterConfig, trace_to_spans};

    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let collector = Arc::new(TraceCollector::new(
      storage.clone(),
      TraceConfig::development(),
    ));

    let parent_traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string();
    let workflow_id = "wf-inbound-tp".to_string();

    {
      let collector = collector.clone();
      let workflow_id = workflow_id.clone();
      crate::context::scope(parent_traceparent.clone(), async move {
        collector.on_event(&WorkflowEvent::WorkflowStarted {
          workflow_id: workflow_id.clone(),
          timestamp: std::time::Instant::now(),
        });
        collector.on_event(&WorkflowEvent::WorkflowCompleted {
          workflow_id,
          duration: StdDuration::from_millis(1),
          timestamp: std::time::Instant::now(),
        });
      })
      .await;
    }

    tokio::time::sleep(StdDuration::from_millis(150)).await;

    let trace = storage
      .get_trace(&workflow_id)
      .await
      .unwrap()
      .expect("trace must be stored");

    assert_eq!(
      trace.metadata.external_trace_id.as_deref(),
      Some("4bf92f3577b34da6a3ce929d0e0e4736"),
      "captured trace_id must equal the upstream traceparent's trace_id"
    );
    assert_eq!(
      trace.metadata.external_parent_span_id.as_deref(),
      Some("00f067aa0ba902b7"),
      "captured parent_span_id must equal the upstream traceparent's span_id"
    );

    // Spans emitted by the OTel exporter must inherit the upstream trace_id
    // and reference the upstream span_id as the workflow's parent.
    let spans = trace_to_spans(&trace, &OtelExporterConfig::default());
    let workflow_span = spans
      .iter()
      .find(|s| s.name.starts_with("yanshi.workflow"))
      .expect("workflow span must exist");
    assert_eq!(
      workflow_span.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736",
      "OTel workflow span must carry inbound trace_id"
    );
    assert_eq!(
      workflow_span.parent_span_id.as_deref(),
      Some("00f067aa0ba902b7"),
      "OTel workflow span's parent_span_id must equal inbound caller's span_id"
    );
  }

  // Q2.3.7: when a retry / loop re-emits NodeStarted for an id that's
  // still in Running state (because its terminal event was lost or
  // never fired), the previous row must be closed out — otherwise the
  // persisted trace carries a phantom never-finishing node.
  #[tokio::test]
  async fn node_started_supersedes_stale_running_row() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let collector = TraceCollector::new(storage.clone(), TraceConfig::development());

    let wf = "wf-retry".to_string();
    let node = "retried_node".to_string();

    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: wf.clone(),
      timestamp: std::time::Instant::now(),
    });
    // First attempt starts but never completes (events lost).
    collector.on_event(&WorkflowEvent::NodeStarted {
      workflow_id: wf.clone(),
      node_id: node.clone(),
      timestamp: std::time::Instant::now(),
    });
    // Second attempt starts.
    collector.on_event(&WorkflowEvent::NodeStarted {
      workflow_id: wf.clone(),
      node_id: node.clone(),
      timestamp: std::time::Instant::now(),
    });
    // Second attempt completes.
    collector.on_event(&WorkflowEvent::NodeCompleted {
      workflow_id: wf.clone(),
      node_id: node.clone(),
      duration: StdDuration::from_millis(5),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: wf.clone(),
      duration: StdDuration::from_millis(10),
      timestamp: std::time::Instant::now(),
    });

    tokio::time::sleep(StdDuration::from_millis(150)).await;

    let trace = storage.get_trace(&wf).await.unwrap().expect("stored trace");
    assert_eq!(trace.nodes.len(), 2, "two attempts must produce two rows");

    let running_count = trace
      .nodes
      .iter()
      .filter(|n| n.status == NodeStatus::Running)
      .count();
    assert_eq!(
      running_count, 0,
      "no phantom Running row may persist — first attempt should be Failed (superseded), second should be Completed"
    );

    // First (older) attempt: superseded → Failed
    assert_eq!(trace.nodes[0].status, NodeStatus::Failed);
    assert!(
      trace.nodes[0]
        .error
        .as_ref()
        .map(|e| e.contains("superseded"))
        .unwrap_or(false)
    );
    // Second attempt: properly completed.
    assert_eq!(trace.nodes[1].status, NodeStatus::Completed);
  }

  // Q2.3.1: when the bounded event channel is saturated, `on_event`
  // must drop new events and bump the counter rather than block or
  // grow the queue without bound. We construct a config with a 1-slot
  // channel and immediately push 100 events; only the first can fit
  // (and even that may be drained quickly), so most should land in
  // the dropped counter.
  #[tokio::test]
  async fn on_event_drops_when_channel_full() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());

    // Block the drain task by pointing it at a storage that takes a
    // long time per event. We approximate with a custom slow exporter
    // that holds each event for 30ms; with a 1-slot channel, push 50
    // events quickly so the drain task can never catch up.
    struct SlowExporter;
    #[async_trait]
    impl TraceExporter for SlowExporter {
      async fn export_trace(&self, _trace: &ExecutionTrace) -> Result<(), anyhow::Error> {
        tokio::time::sleep(StdDuration::from_millis(30)).await;
        Ok(())
      }
    }

    let mut config = TraceConfig::development();
    config.event_channel_capacity = 1;
    let collector = TraceCollector::new(storage, config).with_exporter(Arc::new(SlowExporter));

    for i in 0..50 {
      let workflow_id = format!("wf-drop-{i}");
      collector.on_event(&WorkflowEvent::WorkflowStarted {
        workflow_id: workflow_id.clone(),
        timestamp: std::time::Instant::now(),
      });
      collector.on_event(&WorkflowEvent::WorkflowCompleted {
        workflow_id,
        duration: StdDuration::from_millis(1),
        timestamp: std::time::Instant::now(),
      });
    }

    // No sleep — we want to observe the dropped count *during* backpressure.
    // The first sends fill the 1-slot channel; the rest must be dropped.
    let dropped = collector.events_dropped();
    assert!(
      dropped > 0,
      "events_dropped should be > 0 under saturation, got {dropped}"
    );
  }

  // Q2.3.2: a hung exporter must not block the drain task forever.
  // We configure a 100ms timeout and an exporter that sleeps 5s.
  // The drain task should record the timeout (via the configured
  // storage error policy — Ignore in development()) and continue
  // processing subsequent workflows.
  #[tokio::test]
  async fn exporter_timeout_isolates_drain_task() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());

    struct HungExporter {
      seen: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl TraceExporter for HungExporter {
      async fn export_trace(&self, trace: &ExecutionTrace) -> Result<(), anyhow::Error> {
        self.seen.lock().await.push(trace.workflow_id.clone());
        // Far longer than the configured timeout.
        tokio::time::sleep(StdDuration::from_secs(5)).await;
        Ok(())
      }
    }

    let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let mut config = TraceConfig::development();
    config.exporter_timeout = StdDuration::from_millis(100);
    let collector = TraceCollector::new(storage.clone(), config)
      .with_exporter(Arc::new(HungExporter { seen: seen.clone() }));

    // First workflow: exporter will time out after 100ms.
    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: "wf-hung-1".to_string(),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: "wf-hung-1".to_string(),
      duration: StdDuration::from_millis(1),
      timestamp: std::time::Instant::now(),
    });

    // Second workflow: should NOT have to wait 5s for the first export
    // to finish — the timeout unsticks the drain task within ~100ms.
    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: "wf-hung-2".to_string(),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: "wf-hung-2".to_string(),
      duration: StdDuration::from_millis(1),
      timestamp: std::time::Instant::now(),
    });

    // 300ms is well over the 100ms timeout but well under the 5s sleep.
    tokio::time::sleep(StdDuration::from_millis(500)).await;

    let stored_2 = storage.get_trace("wf-hung-2").await.unwrap();
    assert!(
      stored_2.is_some(),
      "second workflow must be storage-persisted within the 500ms window — the hung exporter must NOT block the drain task"
    );
  }

  /// Q2.2.1 regression — a panicking exporter must NOT silently kill the
  /// drain task and start swallowing every subsequent event. The drain
  /// task is wrapped in `catch_unwind`; we feed one workflow that panics
  /// the exporter, then a second workflow, and assert the second one
  /// still lands in storage.
  #[tokio::test]
  async fn drain_task_survives_exporter_panic() {
    /// Exporter that panics on a single target workflow_id but exports
    /// every other trace normally. Wrapped exports go through `await`,
    /// so the panic surfaces inside the spawned drain future.
    struct PoisonExporter {
      poison_workflow_id: String,
      exported: tokio::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TraceExporter for PoisonExporter {
      async fn export_trace(&self, trace: &ExecutionTrace) -> Result<(), anyhow::Error> {
        if trace.workflow_id == self.poison_workflow_id {
          panic!("intentional drain-task panic for Q2.2.1 regression");
        }
        self.exported.lock().await.push(trace.workflow_id.clone());
        Ok(())
      }
    }

    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let exporter = Arc::new(PoisonExporter {
      poison_workflow_id: "wf-poison".to_string(),
      exported: Default::default(),
    });
    let collector = TraceCollector::new(storage.clone(), TraceConfig::development())
      .with_exporter(exporter.clone());

    // Workflow that panics the exporter on completion.
    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: "wf-poison".to_string(),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: "wf-poison".to_string(),
      duration: StdDuration::from_millis(1),
      timestamp: std::time::Instant::now(),
    });

    // Give the drain task time to process the panicking export.
    tokio::time::sleep(StdDuration::from_millis(150)).await;

    // Drain should still be alive — not poisoned by a single panic.
    assert!(
      !collector.is_drain_poisoned(),
      "single panic should not poison drain"
    );

    // A second workflow MUST still flow through.
    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: "wf-survivor".to_string(),
      timestamp: std::time::Instant::now(),
    });
    collector.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: "wf-survivor".to_string(),
      duration: StdDuration::from_millis(1),
      timestamp: std::time::Instant::now(),
    });

    tokio::time::sleep(StdDuration::from_millis(150)).await;

    let exported = exporter.exported.lock().await;
    assert!(
      exported.iter().any(|id| id == "wf-survivor"),
      "survivor workflow must reach exporter despite earlier panic; got {exported:?}"
    );

    let stored = storage.get_trace("wf-survivor").await.unwrap();
    assert!(
      stored.is_some(),
      "survivor workflow must land in storage despite earlier panic"
    );
  }

  /// Q3.1.2 regression — the trace collector must expose a `flush` API
  /// so the CLI Ctrl-C path can wait for the async drain task before
  /// the process exits. Without this, the JSONL trace file the CLI
  /// tells operators to inspect could be missing the terminal
  /// `WorkflowCancelled` event.
  #[tokio::test]
  async fn flush_waits_until_drain_catches_up() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let collector = TraceCollector::new(storage.clone(), TraceConfig::development());

    for i in 0..20u32 {
      let workflow_id = format!("wf-flush-{i}");
      collector.on_event(&WorkflowEvent::WorkflowStarted {
        workflow_id: workflow_id.clone(),
        timestamp: std::time::Instant::now(),
      });
      collector.on_event(&WorkflowEvent::WorkflowCompleted {
        workflow_id,
        duration: StdDuration::from_millis(1),
        timestamp: std::time::Instant::now(),
      });
    }

    let submitted = collector.submitted_count();
    assert!(
      submitted >= 40,
      "20 workflows × 2 events each should land in the queue (or be dropped if full); got submitted={submitted}"
    );

    // Pre-flush: processed < submitted is normal — the drain task is
    // still working through events.
    let drained = collector.flush(StdDuration::from_secs(5)).await;
    assert!(drained, "flush must catch up within 5s on a quiet system");
    assert_eq!(
      collector.processed_count(),
      collector.submitted_count(),
      "after flush, every queued event must have been processed"
    );

    // The terminal `WorkflowCompleted` events should now be observable
    // via the storage path — proving the drain has actually flushed.
    for i in 0..20u32 {
      let workflow_id = format!("wf-flush-{i}");
      let trace = storage.get_trace(&workflow_id).await.unwrap();
      assert!(
        trace.is_some(),
        "workflow {workflow_id} must be persisted after flush"
      );
    }
  }

  /// Q3.1.2 regression — flush must return `false` (not hang) when the
  /// drain task is poisoned and submitted will never equal processed.
  #[tokio::test]
  async fn flush_returns_false_when_drain_is_poisoned() {
    let dir = tempdir().unwrap();
    let storage = Arc::new(FileTraceStorage::new(dir.path().to_path_buf()).unwrap());
    let collector = TraceCollector::new(storage, TraceConfig::development());

    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: "wf-flush-poison".to_string(),
      timestamp: std::time::Instant::now(),
    });

    // Manually poison the drain to simulate the failure mode.
    collector.drain_poisoned.store(true, Ordering::SeqCst);

    // Fake an outstanding submission that will never be processed.
    collector.submitted_count.fetch_add(1, Ordering::SeqCst);

    let started = std::time::Instant::now();
    let drained = collector.flush(StdDuration::from_secs(2)).await;
    assert!(!drained, "flush must surface drain failure, not hang");
    assert!(
      started.elapsed() < StdDuration::from_millis(500),
      "flush must short-circuit on poisoned drain rather than wait the full timeout"
    );
  }
}
