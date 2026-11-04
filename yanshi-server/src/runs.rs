//! Run submission, status, and the executor abstraction.
//!
//! `POST /v1/runs` and `GET /v1/runs/{id}` live here. Actual workflow
//! execution is delegated to a [`RunExecutor`] trait so the route layer
//! stays oblivious to whether runs are dispatched in-process via
//! `yanshi-core::Flow`, sent to a worker pool, or stubbed out for tests.
//!
//! Production state uses [`FlowRunExecutor`] to run config-first workflows
//! in-process. Tests can still inject [`StubExecutor`] when they only need
//! route / persistence plumbing.

use async_trait::async_trait;
use axum::{
  Extension, Json,
  extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};
use uuid::Uuid;
use yanshi_core::FlowExt;

use yanshi_core::{
  FlowCancellationToken, FlowExecutionConfig, MultiListener, ResumePlan, ResumePlanOptions,
  async_node::AsyncNodeResult,
  build_resume_plan,
  checkpoint::{CheckpointConfig, CheckpointManager},
  events::EventListener,
};
use yanshi_tracing::{
  ExecutionTrace, OtelExporterConfig, OtelSpanSink, OtelTraceExporter, TraceCollector, TraceConfig,
  TraceQuery, TraceStorage, storage::file::FileTraceStorage,
};

use yanshi_agents::runtime::{AgentContext, AgentRuntime, AgentStopReason};
use yanshi_harness::{
  ApprovalDecision, ApprovalProvider, ApprovalScope, HarnessEvent, HarnessEventSink,
  HarnessProfile, HookConfig, SinkChain, wrap_registry,
};
use yanshi_llm::Yanshi;
use yanshi_skills::{SkillBuilder, SkillLoader};
use yanshi_tools::ToolRegistry;

use crate::events_stream::broker_finalize_grace;
use yanshi_db::{
  EventRepo, NewEvent, NewRun, Notice, NotifyListener, Repositories, Run, RunRepo, RunStatus,
};

use crate::AppState;
use crate::distributed_run::DistributedFlowRunExecutor;
use crate::error::{ApiError, JsonReq};
use crate::events_stream::{EventBroker, WorkflowEventListener, publish_through};
use crate::harness_approval::{
  ApprovalDecisionRequest, ApprovalResolveError, PendingApprovalRegistry, PendingApprovalsResponse,
  ServerApprovalProvider, spawn_record_decision_intent_and_notify,
};
use crate::tenant::TenantId;

/// JSON body for `POST /v1/runs`.
///
/// Either `workflow` (inline YAML / JSON workflow definition as a string) or
/// `workflow_id` (reference to a stored workflow) must be supplied. The
/// gateway treats the body as opaque text and hands it to the configured
/// `RunExecutor`; parsing happens at execution time.
#[derive(Debug, Deserialize)]
pub struct CreateRunRequest {
  /// Inline workflow as a YAML or JSON string.
  pub workflow: Option<String>,
  /// Reference to a workflow stored elsewhere (future use).
  pub workflow_id: Option<String>,
  /// Optional tenant echo. Q1.4.3: this is no longer authoritative —
  /// the auth-middleware-bound `X-Yanshi-Tenant` header is the
  /// only source of truth. When the body still carries `tenant_id`
  /// it must match the header, otherwise the request is rejected
  /// with 403. Leaving this field in the body shape preserves the
  /// wire compatibility for existing clients during the transition.
  #[serde(default)]
  pub tenant_id: Option<String>,
  /// Per-run retention overrides (P10.14.1). Either field can pin
  /// the corresponding resource (events / artifacts) for at least
  /// the specified number of days, regardless of the tenant +
  /// profile default. Pinning is *additive*: the cleanup sweep
  /// uses `max(global, override)` so an override can only ever
  /// extend retention, never shorten it.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub retention_overrides: Option<RetentionOverrides>,
  /// W4.3b: opt into distributed execution — dispatched to a
  /// `DistributedFlowRunExecutor` against worker processes over the
  /// gRPC control plane instead of running in-process. `None` (or any
  /// value other than `"distributed"`) keeps today's default in-process
  /// behavior; unrecognized values are a 400, not a silent fallback.
  /// Requires `AppState::worker_control_plane` to be configured and the
  /// workflow to pass `validate_distributed_flow` — both checked before
  /// the run row is created. "Experimental" per `docs/STABILITY.md`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub execution_mode: Option<String>,
}

/// Body shape for `retention_overrides:` on `POST /v1/runs`
/// (P10.14.1). Both fields are optional; absent fields fall back
/// entirely to the tenant default.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetentionOverrides {
  /// Keep `events` rows for this run for at least N days. Must be
  /// `>= 0`. `0` is accepted as a no-op (equivalent to absent) for
  /// caller convenience.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub events_days: Option<i32>,
  /// Keep `artifacts` rows for this run for at least N days. Same
  /// semantics as `events_days`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub artifacts_days: Option<i32>,
}

impl RetentionOverrides {
  /// Validate that no override is negative. The cleanup-sweep SQL
  /// uses `GREATEST(global, COALESCE(override, 0))`, so a negative
  /// override would otherwise silently degrade to the global
  /// default — better to surface the obvious request error at the
  /// API layer.
  pub fn validate(&self) -> Result<(), &'static str> {
    if matches!(self.events_days, Some(n) if n < 0) {
      return Err("retention_overrides.events_days must be >= 0");
    }
    if matches!(self.artifacts_days, Some(n) if n < 0) {
      return Err("retention_overrides.artifacts_days must be >= 0");
    }
    Ok(())
  }

  /// Treat `Some(0)` the same as absent (caller convenience). The
  /// SQL `GREATEST(global, 0)` is already a no-op vs `GREATEST(global)`,
  /// but normalizing here keeps the DB row tidy and the audit story
  /// honest (only meaningful overrides appear in the column).
  fn normalize_nonzero(value: Option<i32>) -> Option<i32> {
    value.filter(|n| *n > 0)
  }

  pub fn into_pair(self) -> (Option<i32>, Option<i32>) {
    (
      Self::normalize_nonzero(self.events_days),
      Self::normalize_nonzero(self.artifacts_days),
    )
  }
}

#[derive(Debug, Serialize)]
pub struct CreateRunResponse {
  pub run_id: Uuid,
  pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CancelRunResponse {
  #[serde(flatten)]
  pub run: Run,
  pub cancelled: bool,
}

/// Minimal run-execution contract. Implementations are responsible for
/// every state transition after the route layer creates the row, including
/// terminal status updates and event emission.
#[async_trait]
pub trait RunExecutor: Send + Sync {
  async fn execute(&self, ctx: RunContext);
}

/// Everything an executor needs to do its job. Owns its own copies of the
/// repositories and broker so the route handler can return immediately.
pub struct RunContext {
  pub run_id: Uuid,
  pub workflow: String,
  pub repos: Repositories,
  pub run_base_dir: Option<PathBuf>,
  pub cancellation_token: FlowCancellationToken,
  /// Forwards events to live SSE subscribers. Persisting to the DB still
  /// has to happen — use [`publish_through`] for the standard path.
  pub broker: EventBroker,
  /// Tenant the run was created under. Mirrors `runs.tenant_id` so
  /// every event the executor emits gets stamped with the correct
  /// scope without re-querying the run row.
  pub tenant_id: String,
  /// Process-local registry the executor writes live state-pool sizes
  /// into (P10.14.2-FU6). `None` skips the gauge wiring — the `StubExecutor`
  /// path and tests that bypass `AppState::new` use this. Real submissions
  /// always carry the `AppState`'s shared registry so the `/metrics`
  /// scrape can read what's running.
  pub live_state_registry: Option<crate::live_state_registry::LiveStateRegistry>,
  /// W0.2: when set, this run is a skill invocation (`workflow` is the
  /// `@skill:<name>` marker `crate::skills::run_skill` builds, optionally
  /// followed by `\n---\n<user input>`) and this is the resolved skill
  /// manifest's directory, already looked up from the catalog at submit
  /// time. `FlowRunExecutor` branches on this instead of trying to parse
  /// `workflow` as a Flow YAML/JSON definition. `None` for every
  /// `POST /v1/runs` submission — only `POST /v1/skills/{name}:run` sets it.
  pub skill_dir: Option<PathBuf>,
  /// W4.1b: shared with `AppState::approval_registry` so a skill run's
  /// tool calls can be gated through the same approval pipeline every
  /// other tool-execution surface uses. Unused by `flow_execute` (only
  /// `skill_execute` consults it), but every `RunContext` carries it —
  /// mirrors how `skill_dir` is `None` on the DAG path.
  pub approval_registry: PendingApprovalRegistry,
  /// W4.1b: deadline `ServerApprovalProvider` waits for an operator
  /// decision on a skill run's pending approval before timing out.
  pub approval_timeout: Duration,
  /// W4.3a: max concurrently-running nodes for this run, sourced from
  /// `AppState::run_max_concurrency`. Unused by `skill_execute` (a skill
  /// invocation is a single agent loop, not a DAG) — only `flow_execute`
  /// consults it.
  pub run_max_concurrency: usize,
}

/// Postgres NOTIFY channel carrying cross-replica cancellation intents
/// (W4.2d). Payload is just the run_id — `RunCancellationRegistry::cancel`
/// needs nothing else, unlike the events broker's catch-up path.
pub const RUN_CANCELLATION_NOTIFY_CHANNEL: &str = "yanshi_cancellations";

#[derive(Clone, Default)]
pub struct RunCancellationRegistry {
  inner: Arc<Mutex<HashMap<Uuid, RunCancellationEntry>>>,
}

impl std::fmt::Debug for RunCancellationRegistry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let len = self.inner.lock().map(|entries| entries.len()).unwrap_or(0);
    f.debug_struct("RunCancellationRegistry")
      .field("active_runs", &len)
      .finish()
  }
}

#[derive(Clone)]
struct RunCancellationEntry {
  token: FlowCancellationToken,
  abort_handle: tokio::task::AbortHandle,
}

impl RunCancellationRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  /// Lock the entry map, recovering on poison so a panicked caller can't
  /// strand every subsequent cancellation request. Same poison-recovery
  /// pattern as [`crate::events_stream::EventBroker::lock_inner`]. (Q5.1)
  fn lock_inner(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, RunCancellationEntry>> {
    match self.inner.lock() {
      Ok(g) => g,
      Err(poisoned) => poisoned.into_inner(),
    }
  }

  pub fn register(
    &self,
    run_id: Uuid,
    token: FlowCancellationToken,
    abort_handle: tokio::task::AbortHandle,
  ) {
    let mut entries = self.lock_inner();
    entries.insert(
      run_id,
      RunCancellationEntry {
        token,
        abort_handle,
      },
    );
  }

  pub fn cancel(&self, run_id: Uuid) -> bool {
    let Some(entry) = self.lock_inner().get(&run_id).cloned() else {
      return false;
    };

    entry.token.cancel();
    entry.abort_handle.abort();
    true
  }

  /// Runs whose executor task lives on this replica.
  pub fn active_run_ids(&self) -> Vec<Uuid> {
    self.lock_inner().keys().copied().collect()
  }

  pub fn complete(&self, run_id: Uuid) {
    let mut entries = self.lock_inner();
    entries.remove(&run_id);
  }
}

/// W4.2d: cross-replica cancellation listener. Spawn one per gateway
/// replica at boot (mirrors `serve::spawn_cleanup_loop`'s "log and keep
/// going, never crash the gateway" shape, and
/// `events_stream::spawn_run_events_listener`'s reconnect posture).
/// The NOTIFY payload (a bare run_id) is all `RunCancellationRegistry::cancel`
/// needs, and it's a safe no-op on every replica that doesn't hold the
/// entry. NOTIFY is at-most-once, so after every (re)connect the listener
/// also reads `run_cancellation_intents` for the runs it holds (eval
/// 2026-10-07 P3).
pub fn spawn_run_cancellation_listener(
  pool: sqlx::PgPool,
  cancellation_registry: RunCancellationRegistry,
) {
  let runs = Repositories::from_pool(pool.clone()).runs;
  tokio::spawn(async move {
    loop {
      let mut listener =
        match NotifyListener::connect(&pool, &[RUN_CANCELLATION_NOTIFY_CHANNEL]).await {
          Ok(listener) => listener,
          Err(err) => {
            warn!(error = %err, "run cancellation listener: connect failed, retrying in 5s");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
          }
        };
      resync_cancellations(&runs, &cancellation_registry).await;
      loop {
        let payload = match listener.recv().await {
          Ok(Notice::Message { payload, .. }) => payload,
          Ok(Notice::Gap) => {
            warn!("run cancellation listener: connection dropped, reconnecting and resyncing");
            break;
          }
          Err(err) => {
            warn!(error = %err, "run cancellation listener: recv failed, reconnecting");
            break;
          }
        };
        match Uuid::parse_str(&payload) {
          Ok(run_id) => {
            cancellation_registry.cancel(run_id);
          }
          Err(err) => {
            warn!(error = %err, payload = %payload, "run cancellation listener: malformed payload, skipping");
          }
        }
      }
    }
  });
}

/// How often this replica renews its runs' leases. A quarter of
/// `yanshi_db::RUN_LEASE_TTL_SECS`, so three missed renewals are tolerated.
pub const RUN_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);

/// Eval 2026-10-07 P5: keep the leases of the runs this replica executes
/// alive, and fail runs whose lease expired: their replica is gone, so
/// nothing will finish them, and they hold the tenant's concurrency slot.
/// Renewal comes first in each tick so a replica never reclaims its own
/// live runs. Each reclaimed run gets a `workflow.failed` event, since
/// the replica that would have written its terminal event is gone and
/// SSE subscribers wait for one.
pub fn spawn_run_lease_loop(
  repos: Repositories,
  broker: EventBroker,
  registry: RunCancellationRegistry,
) {
  tokio::spawn(async move {
    let mut tick = tokio::time::interval(RUN_LEASE_RENEW_INTERVAL);
    loop {
      tick.tick().await;
      if let Err(err) = repos.runs.renew_leases(&registry.active_run_ids()).await {
        warn!(error = %err, "run lease loop: renewal failed");
      }
      let reclaimed = match repos.runs.reclaim_expired_leases().await {
        Ok(reclaimed) => reclaimed,
        Err(err) => {
          warn!(error = %err, "run lease loop: reclaim failed");
          continue;
        }
      };
      for run in reclaimed {
        warn!(run_id = %run.id, "run lease loop: failed a run whose replica stopped");
        if let Err(err) = publish_reclaim_event(&repos, &broker, &run).await {
          warn!(run_id = %run.id, error = %err, "run lease loop: terminal event failed");
        }
      }
    }
  });
}

async fn publish_reclaim_event(
  repos: &Repositories,
  broker: &EventBroker,
  run: &Run,
) -> Result<(), yanshi_db::DbError> {
  let max_seq = repos.events.max_seq(&run.tenant_id, run.id).await?;
  publish_through(
    repos,
    broker,
    NewEvent {
      run_id: run.id,
      seq: max_seq.map_or(0, |seq| seq + 1),
      kind: "workflow.failed".to_string(),
      payload: serde_json::json!({
        "workflow_id": run.id.to_string(),
        "error": run.error,
      }),
      tenant_id: Some(run.tenant_id.clone()),
    },
  )
  .await
}

/// Cancel every local run that has a cancellation intent the listener may
/// have missed.
async fn resync_cancellations(runs: &impl RunRepo, registry: &RunCancellationRegistry) {
  match runs
    .cancellation_intents_among(&registry.active_run_ids())
    .await
  {
    Ok(ids) => {
      for run_id in ids {
        registry.cancel(run_id);
      }
    }
    Err(err) => warn!(error = %err, "run cancellation listener: intent resync failed"),
  }
}

/// V3.4: per-tenant admission control for `POST /v1/runs`. Bounds how
/// many in-process executor tasks a tenant can have running at once
/// (a non-blocking `try_acquire_owned` — rejects rather than queues,
/// unlike the harness's `.await`-based semaphore in
/// `LiveHarnessExecutor::acquire_permit`) and how fast a tenant can
/// submit new runs (a fixed-window counter). Both limits are
/// per-tenant, not global — one noisy tenant can't starve another.
#[derive(Clone)]
pub struct RunAdmissionRegistry {
  inner: Arc<Mutex<HashMap<String, Arc<TenantAdmissionState>>>>,
  max_concurrent_per_tenant: u32,
  max_submissions_per_minute: u32,
  window: Duration,
}

struct TenantAdmissionState {
  semaphore: Arc<tokio::sync::Semaphore>,
  window: Mutex<RateWindow>,
}

struct RateWindow {
  started_at: std::time::Instant,
  count: u32,
}

/// Held for the lifetime of an admitted run's background task. Dropping
/// it releases the tenant's concurrency slot — no manual `.complete()`
/// call needed, unlike [`RunCancellationRegistry`].
#[derive(Debug)]
pub struct RunAdmissionGuard {
  _permit: tokio::sync::OwnedSemaphorePermit,
}

/// Distinct from [`crate::scheduler::AdmissionError`] (the distributed
/// worker gRPC control plane's admission policy) — this is the
/// separate, unrelated `POST /v1/runs` in-process admission path.
#[derive(Debug, Clone)]
pub enum RunAdmissionError {
  ConcurrencyLimitExceeded {
    tenant: String,
    limit: u32,
  },
  RateLimited {
    tenant: String,
    limit_per_minute: u32,
  },
}

impl std::fmt::Display for RunAdmissionError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      RunAdmissionError::ConcurrencyLimitExceeded { tenant, limit } => write!(
        f,
        "tenant '{tenant}' has reached the concurrent-run limit ({limit})"
      ),
      RunAdmissionError::RateLimited {
        tenant,
        limit_per_minute,
      } => write!(
        f,
        "tenant '{tenant}' has exceeded the run submission rate limit ({limit_per_minute}/min)"
      ),
    }
  }
}

impl RunAdmissionRegistry {
  pub fn new(max_concurrent_per_tenant: u32, max_submissions_per_minute: u32) -> Self {
    Self {
      inner: Arc::new(Mutex::new(HashMap::new())),
      max_concurrent_per_tenant,
      max_submissions_per_minute,
      window: Duration::from_secs(60),
    }
  }

  /// Test-only knob: a real 60s window makes rate-limit tests either
  /// slow or flaky. Production always uses the default 60s window.
  pub fn with_window(mut self, window: Duration) -> Self {
    self.window = window;
    self
  }

  /// W4.2f: the configured limits, for callers driving the
  /// cluster-wide-authoritative `RunRepo::create_if_admitted` DB path.
  pub fn max_concurrent_per_tenant(&self) -> u32 {
    self.max_concurrent_per_tenant
  }

  /// W4.2f: see [`Self::max_concurrent_per_tenant`].
  pub fn max_submissions_per_minute(&self) -> u32 {
    self.max_submissions_per_minute
  }

  /// W4.2f: see [`Self::max_concurrent_per_tenant`].
  pub fn window(&self) -> Duration {
    self.window
  }

  /// Same poison-recovery pattern as [`RunCancellationRegistry::lock_inner`].
  fn lock_inner(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<TenantAdmissionState>>> {
    match self.inner.lock() {
      Ok(g) => g,
      Err(poisoned) => poisoned.into_inner(),
    }
  }

  fn tenant_state(&self, tenant: &str) -> Arc<TenantAdmissionState> {
    let mut entries = self.lock_inner();
    entries
      .entry(tenant.to_string())
      .or_insert_with(|| {
        Arc::new(TenantAdmissionState {
          semaphore: Arc::new(tokio::sync::Semaphore::new(
            self.max_concurrent_per_tenant as usize,
          )),
          window: Mutex::new(RateWindow {
            started_at: std::time::Instant::now(),
            count: 0,
          }),
        })
      })
      .clone()
  }

  pub fn try_admit(&self, tenant: &str) -> Result<RunAdmissionGuard, RunAdmissionError> {
    let state = self.tenant_state(tenant);

    let permit = state.semaphore.clone().try_acquire_owned().map_err(|_| {
      RunAdmissionError::ConcurrencyLimitExceeded {
        tenant: tenant.to_string(),
        limit: self.max_concurrent_per_tenant,
      }
    })?;

    let mut window = match state.window.lock() {
      Ok(g) => g,
      Err(poisoned) => poisoned.into_inner(),
    };
    let now = std::time::Instant::now();
    if now.duration_since(window.started_at) >= self.window {
      window.started_at = now;
      window.count = 0;
    }
    if window.count >= self.max_submissions_per_minute {
      // Give the concurrency slot back — this admission attempt failed
      // on the rate-limit check, not the concurrency check.
      drop(permit);
      return Err(RunAdmissionError::RateLimited {
        tenant: tenant.to_string(),
        limit_per_minute: self.max_submissions_per_minute,
      });
    }
    window.count += 1;
    drop(window);

    Ok(RunAdmissionGuard { _permit: permit })
  }
}

/// Default no-op executor used until the real Flow runner lands. Marks runs
/// as `running` then `succeeded` and writes two synthetic events so SSE
/// subscribers see something flow through. Tests use this to verify the
/// route layer + DB plumbing without depending on `yanshi-core`.
#[derive(Clone, Debug, Default)]
pub struct StubExecutor;

#[async_trait]
impl RunExecutor for StubExecutor {
  async fn execute(&self, ctx: RunContext) {
    if let Err(e) = stub_execute(&ctx).await {
      error!(run_id = %ctx.run_id, error = %e, "stub executor failed");
      let _ = ctx
        .repos
        .runs
        .update_status(ctx.run_id, RunStatus::Failed, Some(&e.to_string()))
        .await;
    }
  }
}

/// In-process executor for config-first DAG workflows (and, since W0.2,
/// skill invocations submitted via `POST /v1/skills/{name}:run` —
/// `ctx.skill_dir` distinguishes the two rather than sniffing the
/// `workflow` string, since `@skill:<name>` is not valid Flow YAML/JSON).
#[derive(Clone, Debug, Default)]
pub struct FlowRunExecutor;

#[async_trait]
impl RunExecutor for FlowRunExecutor {
  async fn execute(&self, ctx: RunContext) {
    if ctx.skill_dir.is_some() {
      if let Err(e) = skill_execute(&ctx).await {
        error!(run_id = %ctx.run_id, error = %e, "skill executor failed");
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
      return;
    }
    if let Err(e) = flow_execute(&ctx).await {
      error!(run_id = %ctx.run_id, error = %e, "flow executor failed");
      let status = if e.is_cancelled() {
        RunStatus::Cancelled
      } else {
        RunStatus::Failed
      };
      let _ = ctx
        .repos
        .runs
        .update_status(ctx.run_id, status, Some(&e.to_string()))
        .await;
      // P10.14.2-FU6: drop the live-state gauge entry even on failure
      // so the cardinality stays bounded. (The happy path in
      // `flow_execute` deregisters after the success status update;
      // this branch covers cancellation, panic-via-Err, build_flow
      // failure, etc.)
      if let Some(registry) = &ctx.live_state_registry {
        registry.deregister(&ctx.run_id);
      }
      ctx
        .broker
        .finalise_with_grace(ctx.run_id, broker_finalize_grace());
    }
  }
}

async fn flow_execute(ctx: &RunContext) -> Result<(), anyhow_like::FlowRunError> {
  ctx
    .repos
    .runs
    .update_status(ctx.run_id, RunStatus::Running, None)
    .await?;

  let run_id = ctx.run_id.to_string();
  let flow_def = yanshi_config::executor::parse_workflow_definition(&ctx.workflow)?;
  let mut flow = yanshi_config::executor::build_flow_from_definition(&flow_def, None)?;
  // T3.2: the gateway doesn't accept ad-hoc per-run inputs yet (`--input`
  // isn't wired to `POST /v1/runs`), so `default`-filling is the only way
  // a server-submitted run can ever populate a declared input; a
  // `required` input with no `default` always fails the run here rather
  // than an in-flight node's `input_mapping` resolution failing later
  // with a far less direct error.
  let mut initial_inputs = HashMap::new();
  yanshi_config::executor::apply_declared_inputs(&flow_def, &mut initial_inputs)?;
  // The gateway always streams workflow events into Postgres + the SSE
  // broker. `YANSHI_TRACE_DIR` opts in to *additionally* writing a
  // file-backed `ExecutionTrace` JSON so operators can run `yanshi
  // trace tui <run_id>` against the same run. Kept opt-in because the
  // gateway is long-running and unmanaged trace files would accumulate
  // — the existing run/event retention sweep does not cover this dir.
  let event_listener = WorkflowEventListener::from_state(
    ctx.run_id,
    ctx.tenant_id.clone(),
    ctx.repos.clone(),
    ctx.broker.clone(),
    0,
  );
  let events_flushed = event_listener.flush_handle();
  let mut listeners: Vec<Box<dyn EventListener>> = vec![Box::new(event_listener)];
  match build_run_trace_collector(resolve_server_trace_dir().as_deref(), otlp_span_sink()) {
    Some(Ok(collector)) => listeners.push(Box::new(collector)),
    Some(Err(err)) => {
      // Trace IO is best-effort; degrade to DB-only rather than fail
      // the workflow because the operator's disk is unhappy.
      error!(
        run_id = %ctx.run_id,
        error = %err,
        "tracing: trace collector unavailable; continuing without it",
      );
    }
    None => {}
  }
  flow = flow.with_event_listener(Arc::new(MultiListener::new(listeners)));

  // P10.14.2-FU6: attach a state-size observer when one is wired in.
  // The observer keeps the live `yanshi_state_size_bytes{run_id}`
  // gauge fresh; on terminal transitions below we explicitly
  // deregister so the gauge stops emitting for this run.
  if let Some(registry) = &ctx.live_state_registry {
    flow = flow.with_state_size_observer(registry.observer_for(ctx.run_id));
  }

  let execution_config = server_execution_config(
    ctx.run_base_dir.clone(),
    ctx.cancellation_token.clone(),
    ctx.run_max_concurrency,
  );
  let state = flow
    .execute_from_inputs_with_id_and_config(run_id, initial_inputs, execution_config)
    .await?;

  // The listener bridges sync Flow events to async DB/SSE writes; persist
  // them all before the run is marked finished and its channel closed.
  events_flushed.flushed().await;

  // Y1.3: register every `FlowValue::File` the run left under its base
  // dir before the status flips, so a client that observes `succeeded`
  // can list and download immediately.
  let artifact_base = ctx
    .run_base_dir
    .clone()
    .unwrap_or_else(default_run_base_dir);
  crate::artifacts::register_run_artifacts(
    &ctx.repos,
    ctx.run_id,
    &ctx.tenant_id,
    &state,
    &artifact_base,
  )
  .await?;

  if let Some(error) = first_state_error(&state) {
    ctx
      .repos
      .runs
      .update_status(ctx.run_id, RunStatus::Failed, Some(&error))
      .await?;
  } else {
    ctx
      .repos
      .runs
      .update_status(ctx.run_id, RunStatus::Succeeded, None)
      .await?;
  }

  if let Some(registry) = &ctx.live_state_registry {
    registry.deregister(&ctx.run_id);
  }

  ctx
    .broker
    .finalise_with_grace(ctx.run_id, broker_finalize_grace());
  info!(run_id = %ctx.run_id, "flow executor finished");
  Ok(())
}

/// W0.2: lazy `Yanshi::init()` guard, mirroring
/// `harness_live::ensure_llm_initialized` — a skill run needs the LLM
/// provider registry loaded exactly like a harness session does, but
/// `LiveExecutorError` is private to that module so this is a small,
/// independent copy rather than a cross-module dependency.
async fn ensure_llm_initialized() -> anyhow::Result<()> {
  static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
  INIT
    .get_or_try_init(|| async { Yanshi::init().await })
    .await?;
  Ok(())
}

/// Split the `@skill:<name>` marker `crate::skills::run_skill` builds
/// back into the skill name and the optional user input that followed
/// `\n---\n`. Panics on a malformed marker — `skill_execute` only ever
/// receives what `run_skill` constructed, so a mismatch is a programming
/// error in this crate, not a value to degrade gracefully around.
fn parse_skill_marker(workflow: &str) -> (&str, &str) {
  let rest = match workflow.strip_prefix("@skill:") {
    Some(rest) => rest,
    None => unreachable!("skill_execute called with a non-@skill: workflow marker"),
  };
  match rest.split_once("\n---\n") {
    Some((name, input)) => (name, input),
    None => (rest, ""),
  }
}

/// W0.2: run a skill as a `POST /v1/skills/{name}:run` submission —
/// build a `ReActAgent` from the resolved manifest via `SkillBuilder`
/// and drive one turn, instead of `flow_execute`'s DAG path (a skill
/// invocation is an agent loop, not a `Flow`).
async fn skill_execute(ctx: &RunContext) -> Result<(), anyhow_like::FlowRunError> {
  ctx
    .repos
    .runs
    .update_status(ctx.run_id, RunStatus::Running, None)
    .await?;

  let skill_dir = match ctx.skill_dir.as_deref() {
    Some(dir) => dir,
    None => unreachable!("skill_execute requires RunContext::skill_dir"),
  };
  let (skill_name, user_input) = parse_skill_marker(&ctx.workflow);

  let (result, next_seq) = run_skill_agent(ctx, skill_dir, user_input).await?;

  let (status, error_message) = match &result.stop_reason {
    AgentStopReason::FinalAnswer => (RunStatus::Succeeded, None),
    other => (
      RunStatus::Failed,
      Some(format!(
        "skill run did not finish with a final answer: {other:?}"
      )),
    ),
  };

  publish_through(
    &ctx.repos,
    &ctx.broker,
    NewEvent {
      run_id: ctx.run_id,
      seq: next_seq,
      kind: "skill_run_completed".into(),
      payload: serde_json::json!({
        "skill": skill_name,
        "answer": result.answer,
        "stop_reason": format!("{:?}", result.stop_reason),
      }),
      tenant_id: Some(ctx.tenant_id.clone()),
    },
  )
  .await?;

  ctx
    .repos
    .runs
    .update_status(ctx.run_id, status, error_message.as_deref())
    .await?;

  if let Some(registry) = &ctx.live_state_registry {
    registry.deregister(&ctx.run_id);
  }
  ctx
    .broker
    .finalise_with_grace(ctx.run_id, broker_finalize_grace());
  info!(run_id = %ctx.run_id, skill = skill_name, "skill executor finished");
  Ok(())
}

/// Load the manifest, build the agent, and drive one turn. Split out of
/// `skill_execute` so the DB/event/status bookkeeping above stays
/// readable and this half can convert every failure mode uniformly via
/// `anyhow`.
///
/// W4.1b: the built agent's tool registry is wrapped through
/// `wrap_registry`/`HookConfig` exactly like the CLI's `harness run` and
/// the server's own harness-session path already do (`ReActAgent::tools()`
/// / `with_tools()` is the reusable registry-swap hook both of those use;
/// `yanshi-cli/src/commands/harness/run.rs:194-199` is the pattern
/// mirrored here) — before this, a skill's `shell`/`script`/`code_exec`
/// tools ran with zero approval gating under `/v1/skills/{name}:run`,
/// unlike every other tool-execution surface. `HarnessProfile::Production`
/// auto-escalates every `NonIdempotent` tool call to require approval,
/// matching the "safe by default" posture this API has no per-request
/// profile knob to opt out of. Returns the next free `events`-table `seq`
/// alongside the agent result so the caller's own `skill_run_completed`
/// event doesn't collide with whatever `RunHarnessEventSink` already wrote
/// (both write into the same `(run_id, seq)`-keyed table).
async fn run_skill_agent(
  ctx: &RunContext,
  skill_dir: &FsPath,
  user_input: &str,
) -> anyhow::Result<(yanshi_agent_spi::runtime::AgentRunResult, i64)> {
  ensure_llm_initialized().await?;

  let manifest = SkillLoader::load(skill_dir).map_err(|e| {
    anyhow::anyhow!(
      "failed to load skill manifest at {}: {e}",
      skill_dir.display()
    )
  })?;
  let _warnings = SkillLoader::validate(&manifest, skill_dir)
    .map_err(|e| anyhow::anyhow!("skill validation failed: {e}"))?;
  let model = manifest.model.resolved_model().to_owned();
  let mut agent = SkillBuilder::build(&manifest, skill_dir)
    .await
    .map_err(|e| anyhow::anyhow!("failed to build agent from skill manifest: {e}"))?;

  let session_id = ctx.run_id.to_string();
  let seq_counter = Arc::new(std::sync::atomic::AtomicI64::new(0));
  let run_sink: Arc<dyn HarnessEventSink> = Arc::new(RunHarnessEventSink::new(
    ctx.run_id,
    ctx.tenant_id.clone(),
    ctx.repos.clone(),
    ctx.broker.clone(),
    seq_counter.clone(),
  ));
  let approval_provider: Arc<dyn ApprovalProvider> = Arc::new(ServerApprovalProvider::new(
    ctx.approval_registry.clone(),
    ctx.approval_timeout,
  ));
  let hook_config = HookConfig::new(
    session_id.clone(),
    approval_provider,
    SinkChain::new().push(run_sink),
  )
  .with_profile(HarnessProfile::Production)
  .with_approval_timeout(ctx.approval_timeout);

  let mut snapshot = ToolRegistry::new();
  for tool in agent.tools().list() {
    snapshot.register(tool);
  }
  let wrapped = wrap_registry(snapshot, hook_config);
  agent = agent.with_tools(wrapped);

  let context = AgentContext::new(&session_id, user_input, &model);
  // `ReActAgent` has an inherent `run(&mut self, &str) -> Result<String, _>`
  // that shadows the `AgentRuntime` trait method of the same name for
  // direct calls — UFCS picks the trait method we actually want.
  let result = AgentRuntime::run(&mut agent, context)
    .await
    .map_err(|e| anyhow::anyhow!("skill agent run failed: {e}"))?;

  let next_seq = seq_counter.load(std::sync::atomic::Ordering::SeqCst);
  Ok((result, next_seq))
}

/// Fans every `HarnessEvent` a wrapped skill-run tool call emits
/// (`tool_call_requested`/`approval_requested`/`approval_decided`/
/// `tool_call_completed`) into the run's own `events` table, via the same
/// `publish_through` helper `skill_execute`'s completion event uses — so
/// these land on the SSE stream `/v1/runs/{id}/events` already serves
/// instead of an `harness_session_events` row keyed by a session id no
/// caller is watching (W4.1b). Mirrors `harness_live::ServerHarnessEventSink`,
/// with two differences: destination table, and `seq` sourced from a
/// counter private to this one skill run rather than a `SeqAllocator`
/// shared with a `HarnessRuntime` (a skill run has no such runtime).
struct RunHarnessEventSink {
  run_id: Uuid,
  tenant_id: String,
  repos: Repositories,
  broker: EventBroker,
  seq: Arc<std::sync::atomic::AtomicI64>,
}

impl RunHarnessEventSink {
  fn new(
    run_id: Uuid,
    tenant_id: String,
    repos: Repositories,
    broker: EventBroker,
    seq: Arc<std::sync::atomic::AtomicI64>,
  ) -> Self {
    Self {
      run_id,
      tenant_id,
      repos,
      broker,
      seq,
    }
  }
}

#[async_trait]
impl HarnessEventSink for RunHarnessEventSink {
  fn name(&self) -> &str {
    "run"
  }

  async fn write(&self, event: &HarnessEvent) -> Result<(), yanshi_harness::HarnessError> {
    let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let kind = crate::harness_live::harness_event_kind(&event.body);
    let payload = serde_json::to_value(&event.body).unwrap_or(serde_json::Value::Null);
    if let Err(err) = publish_through(
      &self.repos,
      &self.broker,
      NewEvent {
        run_id: self.run_id,
        seq,
        kind: kind.to_string(),
        payload,
        tenant_id: Some(self.tenant_id.clone()),
      },
    )
    .await
    {
      // Non-fatal, mirroring `ServerHarnessEventSink`: dropping a
      // synthetic event is safer than aborting a real skill run over a
      // transient DB write failure.
      error!(
        run_id = %self.run_id,
        seq,
        error = %err,
        "run harness event sink: persist failed"
      );
    }
    Ok(())
  }
}

/// `GET /v1/runs/{id}/approvals` — list pending approvals for a skill
/// run, oldest first. Mirrors `harness_approval::list_pending_approvals`
/// but keyed by `run_id` against the same shared
/// `AppState::approval_registry` (W4.1b).
pub async fn list_run_approvals(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(run_id): Path<Uuid>,
) -> Result<Json<PendingApprovalsResponse>, ApiError> {
  let run = state
    .repos
    .runs
    .get(run_id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {} not found", run_id)))?;
  // P2.6 tenant boundary, same as the harness-session route.
  if run.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!("run {} not found", run_id)));
  }

  // W4.2e: union of the local registry (same-replica, always current)
  // and the DB-derived pending set (correct regardless of which
  // replica parked the request) — mirrors
  // `harness_approval::merged_pending_approvals`, keyed by `run_id`
  // against `EventRepo::list_pending_approvals` instead of
  // `HarnessEventRepo`'s session-scoped equivalent.
  let mut by_id: std::collections::HashMap<String, yanshi_harness::ApprovalRequest> =
    std::collections::HashMap::new();
  for request in state.approval_registry.list(&run_id.to_string()) {
    by_id.insert(request.id.clone(), request);
  }
  let db_pending = state
    .repos
    .events
    .list_pending_approvals(tenant.as_str(), run_id)
    .await?;
  for value in db_pending {
    if let Ok(request) = serde_json::from_value::<yanshi_harness::ApprovalRequest>(value) {
      by_id.entry(request.id.clone()).or_insert(request);
    }
  }
  let mut approvals: Vec<yanshi_harness::ApprovalRequest> = by_id.into_values().collect();
  approvals.sort_by_key(|req| req.requested_at);
  Ok(Json(PendingApprovalsResponse { approvals }))
}

#[derive(Debug, Serialize)]
pub struct RunApprovalDecisionResponse {
  pub run_id: Uuid,
  pub request_id: String,
  pub resolved: bool,
}

/// `POST /v1/runs/{id}/approvals/{request_id}` — decide a pending
/// approval for a skill run. Mirrors `harness_approval::decide_approval`
/// but keyed by `run_id` (W4.1b).
pub async fn decide_run_approval(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path((run_id, request_id)): Path<(Uuid, String)>,
  JsonReq(body): JsonReq<ApprovalDecisionRequest>,
) -> Result<Json<RunApprovalDecisionResponse>, ApiError> {
  let run = state
    .repos
    .runs
    .get(run_id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {} not found", run_id)))?;
  if run.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!("run {} not found", run_id)));
  }

  let decision = ApprovalDecision {
    request_id: request_id.clone(),
    decision: body.decision,
    scope: body.scope.unwrap_or(ApprovalScope::Once),
    decided_by: body.decided_by.unwrap_or_else(|| "user:http".to_string()),
    decided_at: chrono::Utc::now(),
    reason: body.reason,
  };
  let run_key = run_id.to_string();

  // W4.2e: same local-first-then-DB-fallback shape as
  // `harness_approval::decide_approval`.
  match state
    .approval_registry
    .decide(&run_key, &request_id, decision.clone())
  {
    Ok(()) => {
      spawn_record_decision_intent_and_notify(
        &state,
        &run_key,
        &request_id,
        tenant.as_str(),
        &decision,
      );
      return Ok(Json(RunApprovalDecisionResponse {
        run_id,
        request_id,
        resolved: true,
      }));
    }
    Err(ApprovalResolveError::ProviderGone) => {
      return Err(ApiError::BadRequest(format!(
        "approval {} cannot be decided: provider future already dropped",
        request_id
      )));
    }
    Err(ApprovalResolveError::NotFound) => {}
  }

  let db_pending = state
    .repos
    .events
    .list_pending_approvals(tenant.as_str(), run_id)
    .await?;
  let exists = db_pending
    .iter()
    .any(|value| value.get("id").and_then(|v| v.as_str()) == Some(request_id.as_str()));
  if !exists {
    return Err(ApiError::NotFound(format!(
      "no pending approval {} for run {}",
      request_id, run_id
    )));
  }

  spawn_record_decision_intent_and_notify(
    &state,
    &run_key,
    &request_id,
    tenant.as_str(),
    &decision,
  );
  Ok(Json(RunApprovalDecisionResponse {
    run_id,
    request_id,
    resolved: true,
  }))
}

/// Resolve the gateway's opt-in file-backed trace dir. Returns `None`
/// when `YANSHI_TRACE_DIR` is unset / empty so the default deployment
/// does not silently accumulate JSON files outside the cleanup sweep.
fn resolve_server_trace_dir() -> Option<PathBuf> {
  std::env::var("YANSHI_TRACE_DIR")
    .ok()
    .filter(|v| !v.is_empty())
    .map(PathBuf::from)
}

/// Process-wide OTLP span sink (eval 2026-10-07 P8). Installed once at
/// startup by `serve` when `OTEL_EXPORTER_OTLP_ENDPOINT` is set, so a bad
/// endpoint fails the boot instead of every run, and all runs share one
/// HTTP client.
static OTLP_SPAN_SINK: std::sync::OnceLock<Arc<dyn OtelSpanSink>> = std::sync::OnceLock::new();

/// Install the sink every run exports to. Later calls are ignored.
pub fn install_otlp_span_sink(sink: Arc<dyn OtelSpanSink>) {
  let _ = OTLP_SPAN_SINK.set(sink);
}

pub(crate) fn otlp_span_sink() -> Option<Arc<dyn OtelSpanSink>> {
  OTLP_SPAN_SINK.get().cloned()
}

/// The collector a run gets: a file trace under `trace_dir`, an OTLP
/// export through `sink`, both, or (`None`) neither.
fn build_run_trace_collector(
  trace_dir: Option<&FsPath>,
  sink: Option<Arc<dyn OtelSpanSink>>,
) -> Option<Result<TraceCollector, anyhow::Error>> {
  if trace_dir.is_none() && sink.is_none() {
    return None;
  }
  let storage: Result<Arc<dyn TraceStorage>, anyhow::Error> = match trace_dir {
    Some(dir) => std::fs::create_dir_all(dir)
      .map_err(anyhow::Error::from)
      .and_then(|()| FileTraceStorage::new(dir.to_path_buf()))
      .map(|storage| Arc::new(storage) as Arc<dyn TraceStorage>),
    None => Ok(Arc::new(DiscardTraceStorage)),
  };
  Some(storage.map(|storage| {
    // Production config: skips capturing prompts / IO bodies so traces
    // don't fan out to the size of every per-node payload. The server
    // already persists the full event stream to Postgres.
    let collector = TraceCollector::new(storage, TraceConfig::production());
    match sink {
      Some(sink) => {
        collector.with_exporter(Arc::new(OtelTraceExporter::new(server_otel_config(), sink)))
      }
      None => collector,
    }
  }))
}

pub(crate) fn server_otel_config() -> OtelExporterConfig {
  let mut config = OtelExporterConfig::new("yanshi-server");
  config.service_version = Some(env!("CARGO_PKG_VERSION").to_string());
  config
}

/// `TraceCollector` needs a storage; with OTLP but no `YANSHI_TRACE_DIR`
/// the trace is only exported, never kept.
struct DiscardTraceStorage;

#[async_trait]
impl TraceStorage for DiscardTraceStorage {
  async fn save_trace(&self, _trace: &ExecutionTrace) -> Result<(), anyhow::Error> {
    Ok(())
  }
  async fn get_trace(&self, _workflow_id: &str) -> Result<Option<ExecutionTrace>, anyhow::Error> {
    Ok(None)
  }
  async fn query_traces(&self, _query: TraceQuery) -> Result<Vec<ExecutionTrace>, anyhow::Error> {
    Ok(Vec::new())
  }
  async fn delete_old_traces(
    &self,
    _older_than: chrono::DateTime<chrono::Utc>,
  ) -> Result<usize, anyhow::Error> {
    Ok(0)
  }
}

/// W4.3a: `FlowExecutionMode::Concurrent` dispatches DAG-independent
/// nodes via `FuturesUnordered` up to `max_concurrency`; explicit
/// dependency edges are unaffected either way — only nodes with no edge
/// between them (order-agnostic by DAG semantics) can now interleave.
/// `max_concurrency` defaults to `1` (see `DEFAULT_RUN_MAX_CONCURRENCY`),
/// which is functionally equivalent to the prior hardcoded `serial()`.
fn server_execution_config(
  run_base_dir: Option<PathBuf>,
  cancellation_token: FlowCancellationToken,
  max_concurrency: usize,
) -> FlowExecutionConfig {
  let base_dir = run_base_dir.unwrap_or_else(default_run_base_dir);
  FlowExecutionConfig::concurrent(max_concurrency)
    .with_run_base_dir(base_dir)
    .with_cancellation_token(cancellation_token)
}

fn default_run_base_dir() -> PathBuf {
  if let Ok(path) = std::env::var("YANSHI_RUN_DIR")
    && !path.trim().is_empty()
  {
    return PathBuf::from(path);
  }

  dirs::home_dir()
    .map(|home| home.join(".yanshi").join("runs"))
    .unwrap_or_else(|| std::env::temp_dir().join("yanshi-runs"))
}

fn run_base_dir_for_request() -> PathBuf {
  default_run_base_dir()
}

fn run_dir_for_run(base_dir: &FsPath, run_id: Uuid) -> PathBuf {
  base_dir.join(run_id.to_string())
}

/// V1.1: mirrors `yanshi_core::flow`'s `is_genuine_failure` -- a
/// benign `run_if` skip (`YanshiError::NodeSkipped`) is not a
/// workflow failure. Before this fix, a run whose only "error" was a
/// skipped node was wrongly marked `RunStatus::Failed`.
fn first_state_error(state: &HashMap<String, AsyncNodeResult>) -> Option<String> {
  state.iter().find_map(|(node_id, result)| match result {
    Err(yanshi_core::error::YanshiError::NodeSkipped) => None,
    Err(err) => Some(format!("{node_id}: {err}")),
    Ok(_) => None,
  })
}

async fn stub_execute(ctx: &RunContext) -> Result<(), yanshi_db::DbError> {
  ctx
    .repos
    .runs
    .update_status(ctx.run_id, RunStatus::Running, None)
    .await?;
  publish_through(
    &ctx.repos,
    &ctx.broker,
    NewEvent {
      run_id: ctx.run_id,
      seq: 0,
      kind: "run_started".into(),
      payload: serde_json::json!({"executor": "stub"}),
      tenant_id: Some(ctx.tenant_id.clone()),
    },
  )
  .await?;

  // Brief delay so SSE subscribers have time to attach for tests that
  // race the spawn against the subscribe call.
  tokio::time::sleep(Duration::from_millis(50)).await;

  publish_through(
    &ctx.repos,
    &ctx.broker,
    NewEvent {
      run_id: ctx.run_id,
      seq: 1,
      kind: "run_completed".into(),
      payload: serde_json::json!({"executor": "stub"}),
      tenant_id: Some(ctx.tenant_id.clone()),
    },
  )
  .await?;
  ctx
    .repos
    .runs
    .update_status(ctx.run_id, RunStatus::Succeeded, None)
    .await?;
  // Drop the per-run broadcast channel so live subscribers see EOF after
  // any in-flight events drain.
  ctx
    .broker
    .finalise_with_grace(ctx.run_id, broker_finalize_grace());
  info!(run_id = %ctx.run_id, "stub executor finished");
  Ok(())
}

/// `POST /v1/runs` — accept a workflow body, persist a queued `runs` row,
/// dispatch the executor in the background, return the new id immediately.
pub async fn submit_run(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  JsonReq(req): JsonReq<CreateRunRequest>,
) -> Result<Json<CreateRunResponse>, ApiError> {
  // W0.2: `workflow_id` (reference-by-id) has no backing store — it used
  // to be silently accepted and persisted as an opaque `@workflow:<id>`
  // marker string that the executor could never actually run (it isn't
  // valid Flow YAML/JSON), so every such submission failed deep inside
  // `flow_execute` with a confusing parse error instead of a clear 400 at
  // the API boundary. Reject it explicitly here until a workflow store
  // exists to resolve it against.
  let workflow = match (req.workflow, req.workflow_id) {
    (Some(workflow), _) => workflow,
    (None, Some(_id)) => {
      return Err(ApiError::BadRequest(
        "`workflow_id` (reference-by-id) is not implemented yet — submit an inline \
         `workflow` string instead"
          .into(),
      ));
    }
    (None, None) => {
      return Err(ApiError::BadRequest(
        "request body must include `workflow` (string)".into(),
      ));
    }
  };

  let (events_retention_days, artifacts_retention_days) = match req.retention_overrides {
    Some(overrides) => {
      if let Err(msg) = overrides.validate() {
        return Err(ApiError::BadRequest(msg.into()));
      }
      overrides.into_pair()
    }
    None => (None, None),
  };

  // Eval 2026-10-07 S1: the submitter is a tenant, not the operator, so
  // the workflow's own sandbox settings are capped by the gateway policy.
  // Parse failures fall through to the executor's existing error path.
  if let Ok(flow_def) = yanshi_config::executor::parse_workflow_definition(&workflow) {
    state
      .gateway_policy
      .check_workflow(&flow_def)
      .map_err(ApiError::BadRequest)?;
  }

  let tenant_id = tenant.as_str().to_string();
  // Q1.4.3: refuse to accept a body tenant_id that disagrees with the
  // auth-bound tenant. We don't silently override (that masks bugs in
  // the client); instead force the client to either omit the field or
  // align it with the header.
  if let Some(body_tenant) = &req.tenant_id
    && body_tenant != &tenant_id
  {
    return Err(ApiError::TenantMismatch(format!(
      "request body tenant_id '{body_tenant}' does not match authenticated tenant '{tenant_id}'"
    )));
  }

  // W4.3b: resolve which executor this run dispatches to *before*
  // admission/DB-row creation — an inadmissible or workflow-invalid
  // distributed request shouldn't leave a `queued` row behind, matching
  // the same posture the admission check below already has.
  let executor: Arc<dyn RunExecutor> = match req.execution_mode.as_deref() {
    None => state.executor.clone(),
    Some("in_process") => state.executor.clone(),
    Some("distributed") => {
      let control_plane = state.worker_control_plane.clone().ok_or_else(|| {
        ApiError::BadRequest(
          "execution_mode: \"distributed\" requires a worker gRPC control plane to be \
           configured on this gateway (--worker-grpc-bind) — this deployment has none"
            .to_string(),
        )
      })?;
      let flow_def = yanshi_config::executor::parse_workflow_definition(&workflow)
        .map_err(|err| ApiError::BadRequest(err.to_string()))?;
      crate::distributed_run::validate_distributed_flow(&flow_def)?;
      Arc::new(DistributedFlowRunExecutor::new(control_plane))
    }
    Some(other) => {
      return Err(ApiError::BadRequest(format!(
        "unrecognized execution_mode '{other}' — expected \"in_process\" (default) or \
         \"distributed\""
      )));
    }
  };

  // V3.4 / W4.2f: admission control before the run row is even created —
  // an inadmissible request shouldn't leave a `queued` row behind.
  //
  // `RunAdmissionRegistry::try_admit` is a process-local pre-check only
  // (fails fast on an obviously-over-limit local burst without a DB
  // round trip) — it can no longer reject on its own authority, since
  // with multiple gateway replicas each replica's local semaphore only
  // sees its own share of a tenant's traffic. `RunRepo::create_if_admitted`
  // is the actual source of truth: it re-derives the concurrency count
  // from `runs.status` and the rate window from a shared Postgres table,
  // inside one transaction serialized per tenant via an advisory lock,
  // so the combined count across every replica never exceeds the
  // configured limit.
  let local_admission_guard = state
    .run_admission_registry
    .try_admit(&tenant_id)
    .map_err(|err| ApiError::TooManyRequests(err.to_string()))?;

  let run_id = Uuid::new_v4();
  let run_base_dir = run_base_dir_for_request();
  let run_dir = run_dir_for_run(&run_base_dir, run_id);

  let admission_outcome = state
    .repos
    .runs
    .create_if_admitted(
      NewRun {
        id: run_id,
        workflow: workflow.clone(),
        status: RunStatus::Queued,
        run_dir: Some(run_dir.display().to_string()),
        tenant_id: tenant_id.clone(),
        events_retention_days,
        artifacts_retention_days,
      },
      state.run_admission_registry.max_concurrent_per_tenant() as i64,
      state.run_admission_registry.max_submissions_per_minute() as i64,
      state.run_admission_registry.window().as_secs() as i64,
    )
    .await?;
  let run = match admission_outcome {
    yanshi_db::AdmissionOutcome::Admitted(run) => run,
    yanshi_db::AdmissionOutcome::ConcurrencyLimitExceeded { current, limit } => {
      return Err(ApiError::TooManyRequests(format!(
        "tenant '{tenant_id}' has reached the concurrent-run limit ({current} running, limit {limit})"
      )));
    }
    yanshi_db::AdmissionOutcome::RateLimited {
      window_count,
      limit,
    } => {
      return Err(ApiError::TooManyRequests(format!(
        "tenant '{tenant_id}' has exceeded the run submission rate limit ({window_count} submitted, limit {limit} per window)"
      )));
    }
  };

  // Dispatch in the background so the HTTP request returns immediately. The
  // executor owns the entire run lifecycle from this point.
  let repos = state.repos.clone();
  let broker = state.event_broker.clone();
  let cancellation_registry = state.cancellation_registry.clone();
  let live_state_registry = state.live_state_registry.clone();
  let approval_registry = state.approval_registry.clone();
  let run_max_concurrency = state.run_max_concurrency;
  let cancellation_token = FlowCancellationToken::new();
  let task_token = cancellation_token.clone();
  let handle = tokio::spawn(async move {
    // Held until the task completes — releases the local pre-check's
    // admission slot on drop (V3.4). Cluster-wide correctness now comes
    // from `create_if_admitted`'s DB-derived concurrency count instead
    // (W4.2f), which self-heals from `runs.status` regardless of this
    // guard's lifetime.
    let _admission_guard = local_admission_guard;
    executor
      .execute(RunContext {
        run_id,
        workflow,
        repos,
        run_base_dir: Some(run_base_dir),
        cancellation_token: task_token,
        broker,
        tenant_id,
        live_state_registry: Some(live_state_registry),
        skill_dir: None,
        approval_registry,
        approval_timeout: crate::serve::HARNESS_APPROVAL_TIMEOUT,
        run_max_concurrency,
      })
      .await;
    cancellation_registry.complete(run_id);
  });
  state
    .cancellation_registry
    .register(run_id, cancellation_token, handle.abort_handle());
  if handle.is_finished() {
    state.cancellation_registry.complete(run_id);
  }

  Ok(Json(CreateRunResponse {
    run_id: run.id,
    status: "queued",
  }))
}

#[derive(Debug, Serialize)]
pub struct RunResponse {
  #[serde(flatten)]
  pub run: Run,
}

#[derive(Debug, Deserialize)]
pub struct ListRunsQuery {
  /// Max rows to return, clamped to 1..=100.
  #[serde(default)]
  pub limit: Option<i64>,
  /// Skip the first N rows (after the limit clamp). Lets clients
  /// paginate with `?limit=N&offset=M`. Clamped to ≥ 0.
  #[serde(default)]
  pub offset: Option<i64>,
  /// Optional run-status filter. Accepts the canonical `RunStatus`
  /// strings: `queued`, `running`, `succeeded`, `failed`, `cancelled`.
  #[serde(default)]
  pub status: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListRunsResponse {
  pub runs: Vec<Run>,
}

/// Query string for `GET /v1/runs/{id}/resume-plan`.
#[derive(Debug, Deserialize, Default)]
pub struct ResumePlanQuery {
  /// Override the checkpoint directory. Defaults to the
  /// `CheckpointConfig::default()` path
  /// (`~/.yanshi/checkpoints` for the server's user).
  pub checkpoint_dir: Option<String>,
  /// Treat `Unknown` idempotency calls as safe to replay.
  #[serde(default)]
  pub force_replay: bool,
}

/// `GET /v1/runs` — list recent runs for a tenant, newest first.
///
/// Tenant resolution (Q1.4.1): the `X-Yanshi-Tenant` header bound by
/// the auth middleware is the only source of truth. The previous
/// `?tenant_id=` query parameter is gone — it overrode the header and
/// let any authenticated client list arbitrary tenants' runs.
///
/// Query parameters:
/// - `limit` (default 25, clamped to 1..=100)
/// - `offset` (default 0, clamped to ≥ 0)
/// - `status` (one of the canonical [`RunStatus`] strings; rejects
///   anything else with a 400). Omit to list all statuses.
pub async fn list_runs(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Query(params): Query<ListRunsQuery>,
) -> Result<Json<ListRunsResponse>, ApiError> {
  let tenant_id = tenant.as_str();
  let limit = params.limit.unwrap_or(25).clamp(1, 100);
  let offset = params.offset.unwrap_or(0).max(0);
  let status = match params.status.as_deref() {
    Some(s) => Some(parse_status_filter(s)?),
    None => None,
  };
  let runs = state
    .repos
    .runs
    .list_filtered(tenant_id, status, limit, offset)
    .await?;
  Ok(Json(ListRunsResponse { runs }))
}

/// Validate the `?status=` query parameter against the closed
/// [`RunStatus`] set. Rejects unknown values with a 400 so a typo never
/// silently returns "no runs found".
fn parse_status_filter(raw: &str) -> Result<&str, ApiError> {
  match raw {
    "queued" | "running" | "succeeded" | "failed" | "cancelled" => Ok(raw),
    other => Err(ApiError::BadRequest(format!(
      "invalid status filter '{other}'; expected one of queued|running|succeeded|failed|cancelled"
    ))),
  }
}

/// `POST /v1/runs/{id}:cancel` — idempotently cancel a queued/running run.
pub async fn cancel_run(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(id_cancel): Path<String>,
) -> Result<Json<CancelRunResponse>, ApiError> {
  let id_raw = id_cancel
    .strip_suffix(":cancel")
    .ok_or_else(|| ApiError::BadRequest("run cancellation route must end with :cancel".into()))?;
  let id = Uuid::parse_str(id_raw)
    .map_err(|_| ApiError::BadRequest(format!("invalid run id '{}'", id_raw)))?;

  let run = state
    .repos
    .runs
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {} not found", id)))?;
  // P2.6 tenant boundary: pretend the row doesn't exist when the caller's
  // tenant doesn't own it. 404 (not 403) so a cross-tenant probe can't
  // infer existence by status code.
  if run.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!("run {} not found", id)));
  }

  if crate::events_stream::is_terminal_run_status(&run.status) {
    return Ok(Json(CancelRunResponse {
      run,
      cancelled: false,
    }));
  }

  // W4.2d: local fast path (works today, whether or not this replica
  // actually owns the run) plus a durable, cross-replica intent — see
  // `RUN_CANCELLATION_NOTIFY_CHANNEL`'s doc comment and the
  // `run_cancellation_intents` migration for the full mechanism. Both
  // are best-effort on top of the DB status flip below, which is what
  // actually makes the cancellation durable regardless of which (or
  // whether any) replica's live executor task observes it promptly.
  state.cancellation_registry.cancel(id);
  if let Err(err) = state
    .repos
    .runs
    .record_cancellation_intent(id, &run.tenant_id)
    .await
  {
    warn!(run_id = %id, error = %err, "cancel_run: failed to record cancellation intent");
  } else if let Err(err) = yanshi_db::notify(
    &state.repos.runs.pool,
    RUN_CANCELLATION_NOTIFY_CHANNEL,
    &id.to_string(),
  )
  .await
  {
    warn!(run_id = %id, error = %err, "cancel_run: cross-replica NOTIFY failed");
  }
  state
    .repos
    .runs
    .update_status(id, RunStatus::Cancelled, Some("cancel requested"))
    .await?;
  publish_cancellation_event(&state.repos, &state.event_broker, id, &run.tenant_id).await?;
  state
    .event_broker
    .finalise_with_grace(id, broker_finalize_grace());
  state.cancellation_registry.complete(id);

  let run = state
    .repos
    .runs
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {} not found", id)))?;
  Ok(Json(CancelRunResponse {
    run,
    cancelled: true,
  }))
}

async fn publish_cancellation_event(
  repos: &Repositories,
  broker: &EventBroker,
  run_id: Uuid,
  tenant_id: &str,
) -> Result<(), ApiError> {
  let seq = next_event_seq(repos, tenant_id, run_id).await?;
  publish_through(
    repos,
    broker,
    NewEvent {
      run_id,
      seq,
      kind: "run.cancelled".to_string(),
      payload: serde_json::json!({
        "workflow_id": run_id.to_string(),
        "reason": "cancel requested",
      }),
      tenant_id: Some(tenant_id.to_string()),
    },
  )
  .await?;
  Ok(())
}

async fn next_event_seq(
  repos: &Repositories,
  tenant_id: &str,
  run_id: Uuid,
) -> Result<i64, ApiError> {
  // Q3.11.1: O(1) `MAX(seq)` aggregate instead of paging
  // `list_after(..., 10_000)`. A run with > 10 000 events would
  // silently roll the seq counter back to a value already in
  // `events.(run_id, seq)` and collide the primary key on the next
  // `append`. Mirrors the long-standing pattern already used by
  // `harness_events.max_seq`.
  let max = repos.events.max_seq(tenant_id, run_id).await?;
  Ok(max.map(|seq| seq + 1).unwrap_or(0))
}

/// `GET /v1/runs/{id}/resume-plan` — derive a structured resume plan
/// from the persisted checkpoint for this run.
///
/// Returns the same envelope produced by `yanshi workflow
/// resume-plan` so CLI / UI / Harness approval consumers share one
/// wire shape. Loading the plan does **not** execute anything; it
/// only reads the checkpoint state.
pub async fn get_run_resume_plan(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(id): Path<Uuid>,
  Query(params): Query<ResumePlanQuery>,
) -> Result<Json<ResumePlan>, ApiError> {
  // Confirm the run exists so the route returns a meaningful 404 even
  // when no checkpoint has been written yet.
  let run = state
    .repos
    .runs
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {} not found", id)))?;
  // P2.6 tenant boundary.
  if run.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!("run {} not found", id)));
  }

  let mut config = CheckpointConfig::default();
  if let Some(dir) = params.checkpoint_dir.as_ref() {
    // Eval 2026-10-07 S7: the server reads whatever directory this names.
    state
      .gateway_policy
      .check_path("checkpoint_dir", dir)
      .map_err(ApiError::BadRequest)?;
    config = config.with_checkpoint_dir(PathBuf::from(dir));
  }
  let manager = CheckpointManager::new(config)
    .map_err(|e| ApiError::Internal(format!("checkpoint manager init failed: {e}")))?;
  let checkpoint = manager
    .load_latest_checkpoint(&id.to_string())
    .await
    .map_err(|e| ApiError::Internal(format!("failed to load checkpoint: {e}")))?
    .ok_or_else(|| ApiError::NotFound(format!("no checkpoint found for run {}", id)))?;

  let plan = build_resume_plan(
    &checkpoint,
    &ResumePlanOptions {
      force_replay: params.force_replay,
    },
  )
  .map_err(|e| ApiError::Internal(format!("failed to build resume plan: {e}")))?;

  Ok(Json(plan))
}

/// `GET /v1/runs/{id}` — return the current run state.
pub async fn get_run(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(id): Path<Uuid>,
) -> Result<Json<RunResponse>, ApiError> {
  let run = state
    .repos
    .runs
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {} not found", id)))?;
  // P2.6 tenant boundary: hide cross-tenant rows behind 404.
  if run.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!("run {} not found", id)));
  }
  Ok(Json(RunResponse { run }))
}

/// Default executor used by [`AppState::new`]. Exposed so callers can wrap
/// or replace it (tests use this to verify the route layer).
pub fn default_executor() -> Arc<dyn RunExecutor> {
  Arc::new(FlowRunExecutor)
}

mod anyhow_like {
  #[derive(Debug, thiserror::Error)]
  pub enum FlowRunError {
    #[error(transparent)]
    Db(#[from] yanshi_db::DbError),
    #[error(transparent)]
    Build(#[from] anyhow::Error),
    #[error(transparent)]
    Flow(#[from] yanshi_core::error::YanshiError),
  }

  impl FlowRunError {
    pub fn is_cancelled(&self) -> bool {
      matches!(
        self,
        Self::Flow(yanshi_core::error::YanshiError::TaskCancelled)
      )
    }
  }
}

#[cfg(test)]
mod retention_overrides_tests {
  use super::RetentionOverrides;

  #[test]
  fn validate_rejects_negative_events_days() {
    let o = RetentionOverrides {
      events_days: Some(-1),
      artifacts_days: None,
    };
    assert!(o.validate().is_err());
  }

  #[test]
  fn validate_rejects_negative_artifacts_days() {
    let o = RetentionOverrides {
      events_days: None,
      artifacts_days: Some(-7),
    };
    assert!(o.validate().is_err());
  }

  #[test]
  fn validate_accepts_zero_and_positive() {
    let o = RetentionOverrides {
      events_days: Some(0),
      artifacts_days: Some(180),
    };
    assert!(o.validate().is_ok());
  }

  #[test]
  fn into_pair_normalizes_zero_to_none() {
    let o = RetentionOverrides {
      events_days: Some(0),
      artifacts_days: Some(180),
    };
    // The cleanup SQL treats 0 the same as absent via GREATEST(...,
    // COALESCE(override, 0)). Normalizing in `into_pair` keeps the
    // DB row honest (only meaningful overrides are persisted) and
    // makes the audit story unambiguous.
    assert_eq!(o.into_pair(), (None, Some(180)));
  }

  #[test]
  fn into_pair_passes_through_positive_values() {
    let o = RetentionOverrides {
      events_days: Some(30),
      artifacts_days: Some(60),
    };
    assert_eq!(o.into_pair(), (Some(30), Some(60)));
  }

  #[test]
  fn deserialize_accepts_partial_body() {
    let parsed: RetentionOverrides =
      serde_json::from_str(r#"{"events_days": 90}"#).expect("valid body");
    assert_eq!(parsed.events_days, Some(90));
    assert!(parsed.artifacts_days.is_none());
  }

  #[test]
  fn deserialize_accepts_empty_object() {
    let parsed: RetentionOverrides = serde_json::from_str("{}").expect("empty body ok");
    assert!(parsed.events_days.is_none());
    assert!(parsed.artifacts_days.is_none());
  }
}

#[cfg(test)]
mod run_admission_registry_tests {
  use super::{RunAdmissionError, RunAdmissionRegistry};
  use std::time::Duration;

  #[test]
  fn try_admit_allows_up_to_the_concurrency_limit_then_rejects() {
    let registry = RunAdmissionRegistry::new(2, 1000);
    let _g1 = registry.try_admit("tenant-a").expect("first admit ok");
    let _g2 = registry.try_admit("tenant-a").expect("second admit ok");
    let err = registry
      .try_admit("tenant-a")
      .expect_err("third admit over the concurrency limit must be rejected");
    assert!(matches!(
      err,
      RunAdmissionError::ConcurrencyLimitExceeded { .. }
    ));

    // A different tenant has its own independent slot.
    let _g3 = registry
      .try_admit("tenant-b")
      .expect("a different tenant is unaffected");
  }

  #[test]
  fn try_admit_releases_the_slot_when_the_guard_drops() {
    let registry = RunAdmissionRegistry::new(1, 1000);
    let guard = registry.try_admit("tenant-a").expect("first admit ok");
    assert!(registry.try_admit("tenant-a").is_err());
    drop(guard);
    assert!(
      registry.try_admit("tenant-a").is_ok(),
      "dropping the guard must release the concurrency slot"
    );
  }

  #[test]
  fn try_admit_rejects_over_the_rate_limit_within_the_window() {
    let registry = RunAdmissionRegistry::new(100, 2).with_window(Duration::from_millis(50));
    let _g1 = registry.try_admit("tenant-a").expect("first submit ok");
    let _g2 = registry.try_admit("tenant-a").expect("second submit ok");
    let err = registry
      .try_admit("tenant-a")
      .expect_err("third submit within the window must be rate-limited");
    assert!(matches!(err, RunAdmissionError::RateLimited { .. }));

    std::thread::sleep(Duration::from_millis(75));
    assert!(
      registry.try_admit("tenant-a").is_ok(),
      "a new window must reset the submission count"
    );
  }
}

#[cfg(test)]
mod first_state_error_tests {
  use super::first_state_error;
  use std::collections::HashMap;
  use yanshi_core::error::YanshiError;

  /// V1.1 regression: a state pool whose only "error" is a benign
  /// `run_if` skip must not be reported as a run failure.
  #[test]
  fn ignores_a_benign_skip() {
    let mut state = HashMap::new();
    state.insert("skipped".to_string(), Err(YanshiError::NodeSkipped));
    state.insert("ok".to_string(), Ok(HashMap::new()));
    assert!(first_state_error(&state).is_none());
  }

  /// A genuine failure alongside an unrelated skip is still reported,
  /// and the message names the failing node (not the skipped one).
  #[test]
  fn reports_a_genuine_failure_alongside_a_skip() {
    let mut state = HashMap::new();
    state.insert("skipped".to_string(), Err(YanshiError::NodeSkipped));
    state.insert(
      "failed".to_string(),
      Err(YanshiError::NodeExecutionFailed {
        message: "boom".to_string(),
      }),
    );
    let error = first_state_error(&state).expect("a genuine failure must be reported");
    assert!(error.contains("failed"));
    assert!(error.contains("boom"));
  }

  #[test]
  fn none_when_every_node_succeeded() {
    let mut state = HashMap::new();
    state.insert("a".to_string(), Ok(HashMap::new()));
    state.insert("b".to_string(), Ok(HashMap::new()));
    assert!(first_state_error(&state).is_none());
  }
}

/// W4.3a regression: `/v1/runs` submissions now go through
/// `FlowExecutionConfig::concurrent(n)` instead of the prior hardcoded
/// `serial()`, sourced from `RunContext::run_max_concurrency`.
#[cfg(test)]
mod server_execution_config_tests {
  use super::server_execution_config;
  use std::path::PathBuf;
  use yanshi_core::{FlowCancellationToken, FlowExecutionMode};

  #[test]
  fn builds_concurrent_mode_with_the_given_max_concurrency() {
    let config = server_execution_config(None, FlowCancellationToken::new(), 4);
    assert_eq!(config.mode, FlowExecutionMode::Concurrent);
    assert_eq!(config.max_concurrency, 4);
  }

  /// The pre-W4.3a default (`max_concurrency: 1`) must behave like the
  /// old `serial()` — a single in-flight node at a time — even though
  /// the engine now takes the `Concurrent` code path to get there.
  #[test]
  fn max_concurrency_one_still_clamps_to_at_least_one() {
    let config = server_execution_config(None, FlowCancellationToken::new(), 0);
    assert_eq!(
      config.max_concurrency, 1,
      "FlowExecutionConfig::concurrent must clamp 0 up to 1"
    );
  }

  #[test]
  fn falls_back_to_default_run_base_dir_when_none() {
    let config = server_execution_config(None, FlowCancellationToken::new(), 1);
    assert!(config.run_base_dir.is_some());
  }

  #[test]
  fn honors_an_explicit_run_base_dir() {
    let explicit = PathBuf::from("/tmp/yanshi-w4-3a-test");
    let config = server_execution_config(Some(explicit.clone()), FlowCancellationToken::new(), 1);
    assert_eq!(config.run_base_dir, Some(explicit));
  }
}

#[cfg(test)]
mod run_trace_collector_tests {
  use super::{OtelSpanSink, build_run_trace_collector};
  use async_trait::async_trait;
  use std::sync::{Arc, Mutex};
  use std::time::{Duration, Instant};
  use yanshi_core::events::{EventListener, WorkflowEvent};
  use yanshi_tracing::OtelSpan;

  #[derive(Default)]
  struct RecordingSink(Mutex<Vec<OtelSpan>>);

  #[async_trait]
  impl OtelSpanSink for RecordingSink {
    async fn export_spans(&self, spans: Vec<OtelSpan>) -> Result<(), anyhow::Error> {
      self.0.lock().unwrap().extend(spans);
      Ok(())
    }
  }

  #[test]
  fn no_trace_dir_and_no_otlp_means_no_collector() {
    assert!(build_run_trace_collector(None, None).is_none());
  }

  /// Eval 2026-10-07 P8: with an OTLP endpoint but no `YANSHI_TRACE_DIR`
  /// the run must still be exported (previously no collector was built).
  #[tokio::test]
  async fn otlp_without_trace_dir_exports_the_finished_run() {
    let sink = Arc::new(RecordingSink::default());
    let collector = build_run_trace_collector(None, Some(sink.clone()))
      .expect("OTLP alone must yield a collector")
      .unwrap();
    collector.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: "run-1".to_string(),
      timestamp: Instant::now(),
    });
    collector.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: "run-1".to_string(),
      duration: Duration::from_millis(5),
      timestamp: Instant::now(),
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while sink.0.lock().unwrap().is_empty() && Instant::now() < deadline {
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let spans = sink.0.lock().unwrap();
    assert!(!spans.is_empty(), "finished run was not exported");
    assert!(
      spans[0]
        .attributes
        .iter()
        .any(|attr| format!("{attr:?}").contains("yanshi-server")),
      "{:?}",
      spans[0].attributes
    );
  }
}
