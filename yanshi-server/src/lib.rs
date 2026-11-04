//! Yanshi Gateway server crate.
//!
//! v0.3.0 N8 milestone: a minimum viable control plane built on Axum that
//! lets clients submit, query, and stream workflow runs over HTTP. This
//! crate stays narrow on purpose — it owns routing, AuthN, error envelope,
//! and SSE plumbing; persistence lives in `yanshi-db` and execution in
//! `yanshi-core` / `yanshi-agents`.

use axum::{
  Json, Router,
  extract::DefaultBodyLimit,
  http::{HeaderValue, Method, header},
  middleware,
  routing::{get, post},
};
use serde::Serialize;
use std::sync::Arc;
use tower_http::{
  cors::{AllowOrigin, Any, CorsLayer},
  trace::TraceLayer,
};

use yanshi_core::{HealthChecker, HealthStatus, YanshiError};
use yanshi_db::{Database, Repositories};
use yanshi_tools::{CorsMode, SecurityProfile, SecurityProfileDefaults};

pub mod artifacts;
pub mod auth;
pub mod cleanup;
pub mod diagnostics;
pub mod distributed_run;
pub mod error;
pub mod events_filter;
pub mod events_stream;
pub mod gateway_policy;
pub mod harness;
pub mod harness_approval;
pub mod harness_live;
pub mod live_state_registry;
pub mod metrics;
pub mod notebooks;
pub mod preferences;
pub mod runs;
pub mod scheduler;
pub mod serve;
pub mod skills;
pub mod tenant;
pub mod ui;
pub mod worker_grpc;

pub use auth::{
  AuthConfig, AuthConfigError, AuthenticatedTenant, TenantToken, require_bearer_token,
  resolve_auth_config, resolve_auth_config_from_env,
};
pub use cleanup::{
  CleanupConfig, CleanupError, CleanupReport, DEFAULT_CLEANUP_INTERVAL, cleanup_expired,
  cleanup_expired_local,
};
pub use distributed_run::{DistributedFlowRunExecutor, validate_distributed_flow};
pub use error::{ApiError, JsonReq};
pub use events_stream::{
  EventBroker, EventFlush, EventSink, PersistingEventSink, RUN_EVENTS_NOTIFY_CHANNEL,
  StreamedEvent, WorkflowEventListener, list_events, publish_through, spawn_run_events_listener,
  stream_events,
};
pub use harness::{
  AnswerInterruptRequest, AnswerInterruptResponse, CancelHarnessSessionResponse,
  CreateHarnessSessionRequest, CreateHarnessSessionResponse, HARNESS_CANCELLATION_NOTIFY_CHANNEL,
  HARNESS_EVENTS_NOTIFY_CHANNEL, HarnessEventBroker, HarnessEventsQuery, HarnessSessionContext,
  HarnessSessionExecutor, HarnessSessionResponse, ListHarnessSessionsQuery,
  ListHarnessSessionsResponse, PendingInterrupt, PendingInterruptResponse,
  ResumeHarnessSessionRequest, ResumeHarnessSessionResponse, StreamedHarnessEvent,
  StubHarnessExecutor, answer_interrupt, cancel_harness_session, default_harness_executor,
  get_harness_session, get_pending_interrupt, list_harness_events, list_harness_sessions,
  post_harness_session_action, resume_harness_session, spawn_harness_cancellation_listener,
  spawn_harness_events_listener, stream_harness_events, submit_harness_session,
};
pub use harness_approval::{
  APPROVAL_DECISIONS_NOTIFY_CHANNEL, ApprovalDecisionRequest, ApprovalDecisionResponse,
  ApprovalResolveError, PendingApprovalRegistry, PendingApprovalsResponse, ServerApprovalProvider,
  decide_approval, list_pending_approvals, spawn_approval_decision_listener,
};
pub use harness_live::{LiveHarnessExecutor, ServerHarnessEventSink};
pub use live_state_registry::LiveStateRegistry;
pub use runs::{
  CancelRunResponse, CreateRunRequest, CreateRunResponse, FlowRunExecutor, ListRunsQuery,
  ListRunsResponse, RUN_CANCELLATION_NOTIFY_CHANNEL, ResumePlanQuery, RetentionOverrides,
  RunAdmissionError, RunAdmissionGuard, RunAdmissionRegistry, RunApprovalDecisionResponse,
  RunCancellationRegistry, RunContext, RunExecutor, RunResponse, StubExecutor, cancel_run,
  decide_run_approval, default_executor, get_run, get_run_resume_plan, list_run_approvals,
  list_runs, post_run_action, spawn_run_cancellation_listener, spawn_run_lease_loop, submit_run,
};
pub use scheduler::{
  AdmissionConfigError, AdmissionError, AuthenticatedControlPlane, AuthenticatedGrpcWorkerService,
  ClaimHints, ControlError, DistributedDagRunResult, DistributedDagScheduler,
  DistributedNodeStatus, GrpcWorkerProtocol, GrpcWorkerService, InMemoryWorkerProtocol,
  NodeExecutionPayload, RunControlSnapshot, RunControlStatus, SELECTED_TRANSPORT, SchedulerError,
  StitchedWorkerTraceEvent, WorkerAdmissionPolicy, WorkerAssignment, WorkerCapabilities,
  WorkerControlPlane, WorkerControlServer, WorkerCredential, WorkerHeartbeat, WorkerId,
  WorkerProtocol, WorkerTask, WorkerTaskResult, WorkerTraceEvent, WorkerTransport,
  stitched_trace_to_otel_spans,
};
pub use serve::{
  DEFAULT_SERVE_BIND, ServeConfig, ServeError, ServeReadiness, StartupReport,
  YANSHI_SERVE_BIND_ENV, apply_env_overrides, build_startup_report, run, run_check,
};
pub use skills::{
  ListSkillsResponse, RunSkillRequest, SkillCatalog, SkillEntry, list_skills, run_skill,
};
pub use ui::{asset_response, index_html, ui_router};

pub const CORS_ALLOWED_ORIGINS_ENV: &str = "YANSHI_CORS_ALLOWED_ORIGINS";
pub const MAX_REQUEST_BODY_BYTES_ENV: &str = "YANSHI_MAX_REQUEST_BODY_BYTES";
pub const MAX_WORKFLOW_SUBMIT_BYTES_ENV: &str = "YANSHI_MAX_WORKFLOW_SUBMIT_BYTES";
pub const MAX_SKILL_RUN_BYTES_ENV: &str = "YANSHI_MAX_SKILL_RUN_BYTES";
pub const MAX_CONCURRENT_RUNS_PER_TENANT_ENV: &str = "YANSHI_MAX_CONCURRENT_RUNS_PER_TENANT";
pub const RUN_SUBMIT_RATE_LIMIT_PER_MINUTE_ENV: &str = "YANSHI_RUN_SUBMIT_RATE_LIMIT_PER_MINUTE";
/// W4.3a: max concurrently-running nodes within a single `/v1/runs`
/// workflow submission (`FlowExecutionMode::Concurrent` dispatches
/// DAG-independent nodes via `FuturesUnordered` up to this bound, same
/// engine mode `yanshi workflow run --execution-mode concurrent
/// --max-concurrency` already uses). Unset preserves today's exact
/// behavior — see [`DEFAULT_RUN_MAX_CONCURRENCY`].
pub const RUN_MAX_CONCURRENCY_ENV: &str = "YANSHI_RUN_MAX_CONCURRENCY";
/// Default is `1`: functionally equivalent to the pre-W4.3a hardcoded
/// `FlowExecutionConfig::serial()` (a single in-flight node at a time),
/// so upgrading the gateway does not silently reorder any existing
/// deployment's node execution. Operators opt into real parallelism via
/// [`RUN_MAX_CONCURRENCY_ENV`].
pub const DEFAULT_RUN_MAX_CONCURRENCY: usize = 1;

/// Server-wide state injected into every handler.
#[derive(Clone)]
pub struct AppState {
  pub db: Database,
  pub repos: Repositories,
  /// Auth configuration. `None` means auth is disabled — used in tests and
  /// local dev. Production deployments should always populate this.
  pub auth: Option<AuthConfig>,
  /// Catalog of installed skills exposed via `/v1/skills` and resolved by
  /// `/v1/skills/{name}:run`. Empty when no `YANSHI_SKILLS_INDEX` is
  /// configured — the routes still work, they just return 404 on resolve.
  pub skills: SkillCatalog,
  /// Background executor for submitted runs. Defaults to [`FlowRunExecutor`];
  /// tests can swap in [`StubExecutor`] when they only need route plumbing.
  pub executor: Arc<dyn RunExecutor>,
  /// Process-local broker that fans persisted run events out to SSE
  /// subscribers. Cloning is cheap (Arc-backed).
  pub event_broker: EventBroker,
  /// Process-local cancellation registry for queued/running background runs.
  pub cancellation_registry: RunCancellationRegistry,
  /// Background executor for submitted harness sessions (P-H.5). Defaults
  /// to [`StubHarnessExecutor`] until the LLM-backed runtime lands.
  pub harness_executor: Arc<dyn harness::HarnessSessionExecutor>,
  /// Process-local broker that fans persisted harness session events out
  /// to SSE subscribers. Parallel to [`AppState::event_broker`] so a slow
  /// workflow subscriber can't lag a harness session and vice versa.
  pub harness_broker: HarnessEventBroker,
  /// Process-local pending-approval registry (P-H.5 slice 2). The
  /// `ServerApprovalProvider` parks each pending request here; the
  /// `POST /v1/harness/sessions/{id}/approvals/{request_id}` route
  /// resolves the oneshot from the HTTP side.
  pub approval_registry: PendingApprovalRegistry,
  /// Process-local snapshot of live `Flow::state_pool` sizes per active
  /// run (P10.14.2-FU6). Written by the DAG executor through the
  /// `StateSizeObserver` interface; read at scrape time by the
  /// `/metrics` handler to emit `yanshi_state_size_bytes{run_id}`.
  pub live_state_registry: live_state_registry::LiveStateRegistry,
  /// Active security profile and documented defaults. Enforcement is rolled
  /// out by the follow-up P1 tasks without changing local behavior here.
  pub security: SecurityProfileDefaults,
  /// Process-local per-tenant admission control for `POST /v1/runs`
  /// (V3.4). Kept in sync with `security.run_admission` — rebuilt
  /// whenever `with_security_profile`/`with_security_defaults` sets a
  /// new profile.
  pub run_admission_registry: RunAdmissionRegistry,
  /// W4.3a: max concurrently-running nodes for a `/v1/runs` submission.
  /// See [`RUN_MAX_CONCURRENCY_ENV`] / [`DEFAULT_RUN_MAX_CONCURRENCY`].
  pub run_max_concurrency: usize,
  /// W4.3b: handle to the worker gRPC control plane, when one is
  /// configured (`ServeConfig.worker_grpc` / `serve::run`). `None` in
  /// every deployment that hasn't opted into distributed execution —
  /// `submit_run` rejects `execution_mode: "distributed"` with a clear
  /// 400 rather than silently falling back to in-process when this is
  /// unset. Cheap to clone (`Arc`-backed, same instance
  /// `worker_grpc::serve_worker_grpc`'s listener task shares).
  pub worker_control_plane:
    Option<Arc<scheduler::AuthenticatedControlPlane<scheduler::InMemoryWorkerProtocol>>>,
  /// Eval 2026-10-07 S1/S2/S7: operator ceiling on host-reaching node
  /// types, shell commands and host paths a tenant may name. Defaults to
  /// deny-all; `serve::run` loads it from the environment.
  pub gateway_policy: gateway_policy::GatewayPolicy,
  /// Process-local harness session cancellation tokens (eval 2026-10-07 P1).
  pub harness_cancellations: harness::HarnessCancellationRegistry,
  /// Y5.2: per-notebook BM25 indexes, rebuilt from `source_chunks`.
  pub notebook_indexes: notebooks::NotebookIndexCache,
}

impl std::fmt::Debug for AppState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AppState")
      .field("db", &self.db)
      .field("repos", &self.repos)
      .field("auth", &self.auth)
      .field("skills", &self.skills)
      .field("executor", &"<dyn RunExecutor>")
      .field("cancellation_registry", &self.cancellation_registry)
      .field("run_admission_registry", &"<RunAdmissionRegistry>")
      .field("harness_executor", &"<dyn HarnessSessionExecutor>")
      .field("harness_broker", &self.harness_broker)
      .field("security", &self.security)
      .field("worker_control_plane", &self.worker_control_plane.is_some())
      .finish()
  }
}

impl AppState {
  pub fn new(db: Database) -> Self {
    // P10.15.2: prefer the replica-aware constructor so reads
    // route to `db.read_pool()` when configured. Falls through
    // to the primary when no replica is set.
    let repos = Repositories::from_database(&db);
    Self {
      db,
      repos,
      auth: None,
      skills: SkillCatalog::empty(),
      executor: default_executor(),
      event_broker: EventBroker::new(),
      cancellation_registry: RunCancellationRegistry::new(),
      harness_executor: default_harness_executor(),
      harness_broker: HarnessEventBroker::new(),
      approval_registry: PendingApprovalRegistry::new(),
      live_state_registry: live_state_registry::LiveStateRegistry::new(),
      security: SecurityProfile::default().defaults(),
      run_admission_registry: run_admission_registry_for(&SecurityProfile::default().defaults()),
      run_max_concurrency: DEFAULT_RUN_MAX_CONCURRENCY,
      worker_control_plane: None,
      gateway_policy: gateway_policy::GatewayPolicy::default(),
      harness_cancellations: harness::HarnessCancellationRegistry::default(),
      notebook_indexes: notebooks::NotebookIndexCache::default(),
    }
  }

  /// Attach a custom harness session executor (e.g. wired to a real
  /// `yanshi-harness::HarnessRuntime`). Tests use this to keep the
  /// route layer + DB plumbing decoupled from the agent stack.
  pub fn with_harness_executor(
    mut self,
    executor: Arc<dyn harness::HarnessSessionExecutor>,
  ) -> Self {
    self.harness_executor = executor;
    self
  }

  /// Attach an auth configuration. `None` keeps auth disabled.
  pub fn with_auth(mut self, auth: Option<AuthConfig>) -> Self {
    self.auth = auth;
    self
  }

  /// Attach a custom run executor (e.g. wired to `yanshi-core::Flow`).
  pub fn with_executor(mut self, executor: Arc<dyn RunExecutor>) -> Self {
    self.executor = executor;
    self
  }

  /// Attach the operator's gateway ceiling. Defaults to deny-all.
  pub fn with_gateway_policy(mut self, policy: gateway_policy::GatewayPolicy) -> Self {
    self.gateway_policy = policy;
    self
  }

  /// Attach a populated skill catalog. Defaults to empty.
  pub fn with_skills(mut self, skills: SkillCatalog) -> Self {
    self.skills = skills;
    self
  }

  /// Attach the active security profile defaults.
  pub fn with_security_profile(mut self, profile: SecurityProfile) -> Self {
    self.security = profile.defaults();
    self.run_admission_registry = run_admission_registry_for(&self.security);
    self
  }

  /// Attach fully-resolved security defaults, including server-side env
  /// overrides for CORS origins and request body limits.
  pub fn with_security_defaults(mut self, defaults: SecurityProfileDefaults) -> Self {
    self.security = defaults;
    self.run_admission_registry = run_admission_registry_for(&self.security);
    self
  }

  /// W4.3a: override the max concurrently-running nodes for a `/v1/runs`
  /// submission. Clamped to `>= 1` (mirrors
  /// `FlowExecutionConfig::concurrent`'s own clamp).
  pub fn with_run_max_concurrency(mut self, max_concurrency: usize) -> Self {
    self.run_max_concurrency = max_concurrency.max(1);
    self
  }

  /// W4.3b: attach the worker gRPC control plane so `submit_run` can
  /// accept `execution_mode: "distributed"`. `None` keeps distributed
  /// submission disabled, matching [`Self::with_auth`]'s
  /// `Option`-accepting shape. `serve::run` passes the same `Arc` it
  /// hands to the spawned `serve_worker_grpc` listener task — the HTTP
  /// route layer and the gRPC listener share one control plane instance,
  /// not two independent ones.
  pub fn with_worker_control_plane(
    mut self,
    control_plane: Option<
      Arc<scheduler::AuthenticatedControlPlane<scheduler::InMemoryWorkerProtocol>>,
    >,
  ) -> Self {
    self.worker_control_plane = control_plane;
    self
  }
}

/// Build a fresh [`RunAdmissionRegistry`] from a [`SecurityProfileDefaults`]'s
/// `run_admission` sub-struct (V3.4). Kept as a free function so both
/// `AppState::new` and the `with_security_*` builders stay in sync
/// without duplicating the limit lookup.
fn run_admission_registry_for(defaults: &SecurityProfileDefaults) -> RunAdmissionRegistry {
  RunAdmissionRegistry::new(
    defaults.run_admission.max_concurrent_runs_per_tenant,
    defaults
      .run_admission
      .max_run_submissions_per_minute_per_tenant,
  )
}

pub fn server_security_defaults_from_env(
  profile: SecurityProfile,
) -> Result<SecurityProfileDefaults, ServerHttpConfigError> {
  let mut defaults = profile.defaults();

  if let Some(origins) = comma_separated_env(CORS_ALLOWED_ORIGINS_ENV) {
    validate_origins(&origins)?;
    defaults.cors.allowed_origins = origins;
  }
  if let Some(limit) = u64_env(MAX_REQUEST_BODY_BYTES_ENV)? {
    defaults.request_limits.max_request_body_bytes = limit;
  }
  if let Some(limit) = u64_env(MAX_WORKFLOW_SUBMIT_BYTES_ENV)? {
    defaults.request_limits.max_workflow_submit_bytes = limit;
  }
  if let Some(limit) = u64_env(MAX_SKILL_RUN_BYTES_ENV)? {
    defaults.request_limits.max_skill_run_bytes = limit;
  }
  if let Some(limit) = u32_env(MAX_CONCURRENT_RUNS_PER_TENANT_ENV)? {
    defaults.run_admission.max_concurrent_runs_per_tenant = limit;
  }
  if let Some(limit) = u32_env(RUN_SUBMIT_RATE_LIMIT_PER_MINUTE_ENV)? {
    defaults
      .run_admission
      .max_run_submissions_per_minute_per_tenant = limit;
  }

  Ok(defaults)
}

/// W4.3a: resolve [`RUN_MAX_CONCURRENCY_ENV`], falling back to
/// [`DEFAULT_RUN_MAX_CONCURRENCY`] when unset. Kept as its own function
/// (not folded into `server_security_defaults_from_env`) since this
/// isn't a security-profile setting — it governs DAG scheduling, not
/// CORS/body-limits/admission.
pub fn run_max_concurrency_from_env() -> Result<usize, ServerHttpConfigError> {
  Ok(
    u32_env(RUN_MAX_CONCURRENCY_ENV)?
      .map(|value| value as usize)
      .unwrap_or(DEFAULT_RUN_MAX_CONCURRENCY),
  )
}

#[derive(Debug, thiserror::Error)]
pub enum ServerHttpConfigError {
  #[error("{name} contains invalid HTTP origin '{value}': {source}")]
  InvalidOrigin {
    name: &'static str,
    value: String,
    source: axum::http::header::InvalidHeaderValue,
  },
  #[error("{name} must be a positive integer byte count, got '{value}'")]
  InvalidByteLimit { name: &'static str, value: String },
  #[error("{name} must be a positive integer, got '{value}'")]
  InvalidCount { name: &'static str, value: String },
}

fn comma_separated_env(name: &'static str) -> Option<Vec<String>> {
  let value = std::env::var(name).ok()?;
  let values = value
    .split(',')
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(ToOwned::to_owned)
    .collect::<Vec<_>>();
  Some(values)
}

fn u64_env(name: &'static str) -> Result<Option<u64>, ServerHttpConfigError> {
  let Some(value) = std::env::var(name).ok() else {
    return Ok(None);
  };
  let limit = value
    .trim()
    .parse()
    .map_err(|_| ServerHttpConfigError::InvalidByteLimit {
      name,
      value: value.clone(),
    })?;
  if limit == 0 {
    return Err(ServerHttpConfigError::InvalidByteLimit { name, value });
  }
  Ok(Some(limit))
}

fn u32_env(name: &'static str) -> Result<Option<u32>, ServerHttpConfigError> {
  let Some(value) = std::env::var(name).ok() else {
    return Ok(None);
  };
  let limit: u32 = value
    .trim()
    .parse()
    .map_err(|_| ServerHttpConfigError::InvalidCount {
      name,
      value: value.clone(),
    })?;
  if limit == 0 {
    return Err(ServerHttpConfigError::InvalidCount { name, value });
  }
  Ok(Some(limit))
}

fn validate_origins(origins: &[String]) -> Result<(), ServerHttpConfigError> {
  for origin in origins {
    HeaderValue::from_str(origin).map_err(|source| ServerHttpConfigError::InvalidOrigin {
      name: CORS_ALLOWED_ORIGINS_ENV,
      value: origin.clone(),
      source,
    })?;
  }
  Ok(())
}

/// Build the gateway router with the standard health / authenticated split.
///
/// Health probes (`/health`, `/health/live`, `/health/ready`) are
/// intentionally unauthenticated so kubelet / load-balancer probes don't
/// need to know the API token. `/v1/*` routes inherit the bearer-token
/// middleware when [`AppState::auth`] is `Some`; otherwise they pass
/// through (only safe in tests / local dev).
/// Axum handler for the `GET /metrics` endpoint (P10.14.2-FU1).
/// Returns the current Prometheus snapshot as text. Bypasses auth
/// so Prometheus scrapers can poll without a bearer token — the
/// same convention as `/health`.
///
/// P10.14.2-FU4: gauges that aren't kept current by per-event
/// observers are refreshed at scrape time. The Harness session
/// status buckets and the pending-approval count fall into this
/// bucket — they're cheap to compute (one indexed `SELECT` +
/// one mutex read) and there's no benefit to maintaining them
/// out-of-band.
async fn prometheus_metrics(
  axum::extract::State(state): axum::extract::State<AppState>,
) -> impl axum::response::IntoResponse {
  // Refresh the scrape-time gauges before rendering. Failures
  // here are logged + swallowed — `/metrics` must never 500
  // because a single panel can't compute, otherwise the whole
  // scrape stops.
  refresh_scrape_time_gauges(&state).await;

  let body = metrics::render_text();
  (
    [(
      axum::http::header::CONTENT_TYPE,
      "text/plain; version=0.0.4; charset=utf-8",
    )],
    body,
  )
}

/// Run the scrape-time inspectors that compute gauges from
/// "live" sources (DB row counts, in-memory registries) rather
/// than per-event observation.
///
/// Adding a new scrape-time gauge: emit it from a helper in
/// `crate::metrics::observe_*`, then call the helper here. The
/// per-gauge cost is one query / mutex read; the total stays
/// well under typical scrape budgets (15-30s).
async fn refresh_scrape_time_gauges(state: &AppState) {
  // P10.14.2-FU4 — Harness session status buckets.
  match sqlx::query_as::<_, (String, i64)>(
    "SELECT status, COUNT(*)::BIGINT FROM harness_sessions GROUP BY status",
  )
  .fetch_all(state.db.read_pool())
  .await
  {
    Ok(rows) => {
      // Always emit all four known statuses so a status that
      // drops to zero renders as zero, not as a stale value.
      // Unknown statuses (DB / app drift) are ignored — we
      // don't invent a label the dashboard doesn't expect.
      let mut totals = std::collections::HashMap::<&'static str, u64>::from([
        ("running", 0),
        ("completed", 0),
        ("failed", 0),
        ("cancelled", 0),
      ]);
      for (status, count) in rows {
        if let Some(slot) = totals.get_mut(status.as_str()) {
          *slot = count.max(0) as u64;
        }
      }
      for (status, count) in totals {
        metrics::observe_harness_sessions_active(status, count);
      }
    }
    Err(err) => {
      // Common case in tests where the DB pool is lazy and the
      // server isn't actually connected to Postgres. Log at
      // debug so production scrape errors still surface but
      // tests stay quiet.
      tracing::debug!(
        error = %err,
        "refresh_scrape_time_gauges: harness session count query failed"
      );
    }
  }

  // P10.14.2-FU4 — pending approval count from the in-process
  // registry. Always succeeds.
  metrics::observe_harness_approvals_pending(state.approval_registry.pending_count());

  // P10.14.2-FU5 — health status per component.
  //
  // `component="system"` is always 1 if we're computing it,
  // because the only way to reach this code is via a successful
  // `/metrics` poll — the rest of the stat panel is hyperbole.
  // `component="database"` is a `SELECT 1` probe against the
  // read pool so it picks up replica unavailability too. Both
  // failures map to gauge 0 (Stat panel red); never block the
  // scrape.
  metrics::observe_health_status("system", true);
  let db_up = sqlx::query("SELECT 1")
    .execute(state.db.read_pool())
    .await
    .is_ok();
  metrics::observe_health_status("database", db_up);

  // P10.14.2-FU5 — process resident memory. Linux reads from
  // `/proc/self/statm`; non-Linux falls back to 0 with a debug
  // log. Production deployments are Linux 99% of the time.
  if let Some(bytes) = process_memory_bytes() {
    metrics::observe_memory_usage_bytes(bytes);
  } else {
    // Emit 0 so the panel renders cleanly on dev macOS / Windows
    // hosts and operators can still see the scrape worked.
    metrics::observe_memory_usage_bytes(0);
  }

  // P10.14.2-FU5 — active runs across all tenants. `active` =
  // queued + running, never terminal. One indexed query against
  // the read pool. Same fail-soft pattern as the harness session
  // refresh — a query error skips this gauge but doesn't block
  // the rest of the scrape. V3.4: summed into a single unlabeled
  // gauge instead of one series per `tenant` — `/metrics` is
  // unauthenticated, so a per-tenant label would leak tenant IDs
  // to any scraper.
  match sqlx::query_as::<_, (String, i64)>(
    "SELECT tenant_id, COUNT(*)::BIGINT FROM runs \
       WHERE status IN ('queued', 'running') GROUP BY tenant_id",
  )
  .fetch_all(state.db.read_pool())
  .await
  {
    Ok(rows) => {
      let total: i64 = rows.iter().map(|(_, count)| (*count).max(0)).sum();
      metrics::observe_workflow_runs_active(total as u64);
    }
    Err(err) => {
      tracing::debug!(
        error = %err,
        "refresh_scrape_time_gauges: workflow_runs_active query failed"
      );
    }
  }

  // P10.14.2-FU6 — total live state-pool size across active runs.
  // Pure in-process registry read: no DB, no syscall. The DAG
  // executor is responsible for deregistering on terminal
  // transitions, so the snapshot here only contains currently-
  // running runs. V3.4: summed into a single unlabeled gauge
  // instead of one series per `run_id`, for the same
  // unauthenticated-route reason as workflow_runs_active above.
  let total_state_bytes: u64 = state
    .live_state_registry
    .snapshot()
    .into_iter()
    .map(|(_, bytes)| bytes)
    .sum();
  metrics::observe_state_size_bytes(total_state_bytes);
}

/// Best-effort process resident-memory probe (P10.14.2-FU5).
/// Reads `/proc/self/statm` on Linux (the second whitespace-
/// separated field is resident pages; multiplying by 4096
/// gives bytes — Linux page size is 4096 on every architecture
/// the gateway targets). Returns `None` on non-Linux + on any
/// I/O / parse failure so the caller can degrade to a `0`
/// gauge rather than fail the scrape.
fn process_memory_bytes() -> Option<u64> {
  #[cfg(target_os = "linux")]
  {
    let s = std::fs::read_to_string("/proc/self/statm").ok()?;
    let field = s.split_whitespace().nth(1)?;
    let pages: u64 = field.parse().ok()?;
    Some(pages.saturating_mul(4096))
  }
  #[cfg(not(target_os = "linux"))]
  {
    None
  }
}

pub fn create_router(state: AppState) -> Router {
  let health = Router::new()
    .route("/health", get(health_check))
    .route("/health/live", get(liveness_check))
    .route("/health/ready", get(readiness_check))
    // P10.14.2-FU1: Prometheus metrics endpoint. No auth — same
    // convention as `/health` so scrapers don't need a bearer
    // token. The Grafana dashboard
    // (`dashboards/grafana/yanshi-overview.json`) consumes
    // this surface.
    .route("/metrics", get(prometheus_metrics));

  let workflow_limit = state.security.request_limits.max_workflow_submit_bytes as usize;
  let skill_limit = state.security.request_limits.max_skill_run_bytes as usize;

  let v1 = Router::new()
    .route("/v1/whoami", get(whoami))
    .route(
      "/v1/runs",
      get(list_runs)
        .post(submit_run)
        .layer(DefaultBodyLimit::max(workflow_limit)),
    )
    .route("/v1/runs/:id", get(get_run).post(post_run_action))
    .route("/v1/runs/:id/resume-plan", get(get_run_resume_plan))
    .route("/v1/runs/:id/events/history", get(list_events))
    .route("/v1/runs/:id/events", get(stream_events))
    .route("/v1/runs/:id/approvals", get(list_run_approvals))
    // Y5.2: notebooks, their sources and search.
    .route(
      "/v1/notebooks",
      get(notebooks::list_notebooks).post(notebooks::create_notebook),
    )
    .route("/v1/notebooks/:id", get(notebooks::get_notebook))
    .route(
      "/v1/notebooks/:id/sources",
      post(notebooks::add_source).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
    )
    .route("/v1/notebooks/:id/search", get(notebooks::search_notebook))
    // Y1.3: what a run produced, and the bytes themselves.
    .route("/v1/runs/:id/artifacts", get(artifacts::list_run_artifacts))
    .route(
      "/v1/runs/:id/artifacts/:artifact_id",
      get(artifacts::download_run_artifact),
    )
    .route(
      "/v1/runs/:id/approvals/:request_id",
      post(decide_run_approval),
    )
    .route("/v1/skills", get(list_skills))
    // The `:run` suffix is part of the path. Axum's pattern can't match a
    // literal segment containing `:`, so we capture the whole tail and
    // strip the suffix in the handler.
    .route(
      "/v1/skills/:name_run",
      post(run_skill).layer(DefaultBodyLimit::max(skill_limit)),
    )
    .route(
      "/v1/harness/sessions",
      get(list_harness_sessions)
        .post(submit_harness_session)
        .layer(DefaultBodyLimit::max(workflow_limit)),
    )
    // GET captures `:id` as Uuid; POST captures the raw path (including the
    // literal `:cancel` / `:resume` suffix) as String and dispatches inside
    // `post_harness_session_action`. Same single-route + dual-method trick
    // as `/v1/runs/:id`.
    .route(
      "/v1/harness/sessions/:id",
      get(get_harness_session).post(post_harness_session_action),
    )
    .route(
      "/v1/harness/sessions/:id/events/history",
      get(list_harness_events),
    )
    .route(
      "/v1/harness/sessions/:id/events",
      get(stream_harness_events),
    )
    .route(
      "/v1/harness/sessions/:id/approvals",
      get(list_pending_approvals),
    )
    .route(
      "/v1/harness/sessions/:id/approvals/:request_id",
      post(decide_approval),
    )
    .route(
      "/v1/harness/sessions/:id/interrupt",
      get(get_pending_interrupt),
    )
    .route(
      "/v1/harness/sessions/:id/interrupt/answer",
      post(answer_interrupt),
    )
    .route("/v1/diagnostics", get(diagnostics::get_diagnostics))
    .route(
      "/v1/preferences",
      get(preferences::list_preferences).put(preferences::put_preferences),
    );

  // P2.6/U1.1: resolve a TenantId extension on every /v1/* request.
  // Layering order here is load-bearing — `.layer()` calls nest
  // outside-in, so whichever is applied LAST runs FIRST on the
  // request path. `extract_tenant_id` is layered first (innermost) so
  // that when auth is configured, `require_bearer_token` (layered
  // second/outermost, below) runs *before* it and can insert an
  // `AuthenticatedTenant` extension for it to consult — without auth,
  // this layer alone reproduces the pre-U1.1 "trust the client
  // header, default to `default`" behavior.
  let v1 = v1.layer(middleware::from_fn(tenant::extract_tenant_id));
  let v1 = match state.auth.clone() {
    Some(auth) => v1.layer(middleware::from_fn_with_state(auth, require_bearer_token)),
    None => v1,
  };

  // Q3.4.1: install the global request body cap so every route gets a
  // sane upper bound, not just the three (`/v1/runs`, `/v1/skills/...`,
  // `/v1/harness/sessions`) that override with route-local limits.
  // Before this layer was added, `YANSHI_MAX_REQUEST_BODY_BYTES`
  // (and the security profile default) were parsed into
  // `request_limits.max_request_body_bytes` but never applied —
  // arbitrary POST bodies to e.g. `/v1/preferences` could exhaust
  // server memory. The route-local `DefaultBodyLimit::max(...)` calls
  // for workflow/skill/harness bodies remain in place and shadow
  // this global cap when present (per-route always wins under
  // `tower-http`'s layering semantics).
  let global_body_limit = state.security.request_limits.max_request_body_bytes as usize;

  Router::new()
    .merge(health)
    .merge(v1)
    .merge(ui_router())
    .layer(cors_layer(&state.security))
    .layer(TraceLayer::new_for_http())
    .layer(DefaultBodyLimit::max(global_body_limit))
    .with_state(state)
}

fn cors_layer(defaults: &SecurityProfileDefaults) -> CorsLayer {
  match defaults.cors.mode {
    CorsMode::Permissive => CorsLayer::permissive(),
    CorsMode::ExplicitOrigins => {
      let origins = defaults
        .cors
        .allowed_origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin).ok())
        .collect::<Vec<_>>();
      let allow_origin = if origins.is_empty() {
        AllowOrigin::list(Vec::<HeaderValue>::new())
      } else {
        AllowOrigin::list(origins)
      };
      CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .expose_headers(Any)
    }
  }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
  status: &'static str,
  service: &'static str,
}

async fn health_check() -> Json<HealthResponse> {
  Json(healthy())
}

async fn liveness_check() -> Json<HealthResponse> {
  Json(healthy())
}

fn healthy() -> HealthResponse {
  HealthResponse {
    status: "ok",
    service: "yanshi-server",
  }
}

#[derive(Debug, Serialize)]
struct ReadinessCheckResult {
  name: String,
  status: &'static str,
  message: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReadinessResponse {
  status: &'static str,
  service: &'static str,
  checks: Vec<ReadinessCheckResult>,
}

/// W5.3: unlike `/health`/`/health/live` (always unconditional 200 — K8s
/// liveness must never depend on external dependencies), `/health/ready`
/// actually probes the database via the same `SELECT 1` pattern
/// `refresh_scrape_time_gauges` already runs for the `/metrics` gauge, and
/// returns 503 when it fails so a load balancer stops routing traffic to
/// an instance that can't reach Postgres.
async fn readiness_check(
  axum::extract::State(state): axum::extract::State<AppState>,
) -> impl axum::response::IntoResponse {
  let checker = HealthChecker::new();
  let pool = state.db.read_pool().clone();
  checker
    .add_check("database", move || {
      let pool = pool.clone();
      Box::pin(async move {
        sqlx::query("SELECT 1")
          .execute(&pool)
          .await
          .map(|_| HealthStatus::Healthy)
          .map_err(|error| YanshiError::NetworkError {
            message: format!("database readiness check failed: {error}"),
          })
      })
    })
    .await;

  let report = checker.check_health().await;
  let status_code = if report.is_healthy {
    axum::http::StatusCode::OK
  } else {
    axum::http::StatusCode::SERVICE_UNAVAILABLE
  };

  let checks = report
    .checks
    .into_iter()
    .map(|check| ReadinessCheckResult {
      name: check.name,
      status: match check.status {
        HealthStatus::Healthy => "healthy",
        HealthStatus::Degraded => "degraded",
        HealthStatus::Unhealthy => "unhealthy",
      },
      message: check.message,
    })
    .collect();

  (
    status_code,
    Json(ReadinessResponse {
      status: if report.is_healthy { "ok" } else { "unhealthy" },
      service: "yanshi-server",
      checks,
    }),
  )
}

#[derive(Debug, Serialize)]
struct WhoamiResponse {
  authenticated: bool,
}

/// Smoke endpoint that requires bearer auth when configured. Subsequent
/// commits replace this with real `/v1/runs` etc., but it gives the auth
/// middleware something concrete to gate during initial rollout.
async fn whoami() -> Json<WhoamiResponse> {
  Json(WhoamiResponse {
    authenticated: true,
  })
}

#[cfg(test)]
mod run_max_concurrency_tests {
  use super::*;

  /// W4.3a: single test function (not several `#[test]`s) so the
  /// `YANSHI_RUN_MAX_CONCURRENCY` mutations below can't race a
  /// parallel-running sibling test touching the same process-wide env
  /// var — nothing else in this binary reads it, but keeping every case
  /// sequential in one function removes any doubt.
  #[test]
  fn run_max_concurrency_from_env_defaults_parses_and_rejects_garbage() {
    // SAFETY: RUN_MAX_CONCURRENCY_ENV is not read by any other test in
    // this binary.
    unsafe {
      std::env::remove_var(RUN_MAX_CONCURRENCY_ENV);
    }
    assert_eq!(
      run_max_concurrency_from_env().unwrap(),
      DEFAULT_RUN_MAX_CONCURRENCY
    );

    unsafe {
      std::env::set_var(RUN_MAX_CONCURRENCY_ENV, "8");
    }
    assert_eq!(run_max_concurrency_from_env().unwrap(), 8);

    unsafe {
      std::env::set_var(RUN_MAX_CONCURRENCY_ENV, "not-a-number");
    }
    assert!(run_max_concurrency_from_env().is_err());

    unsafe {
      std::env::remove_var(RUN_MAX_CONCURRENCY_ENV);
    }
  }
}

#[cfg(test)]
mod worker_control_plane_tests {
  use super::*;
  use crate::scheduler::{AuthenticatedControlPlane, InMemoryWorkerProtocol, WorkerControlPlane};

  /// W4.3b: `AppState::new` defaults to no distributed execution support;
  /// `with_worker_control_plane` is the only way to opt in, mirroring
  /// `with_auth`'s `Option`-accepting shape. `submit_run` relies on this
  /// being `None` by default to reject `execution_mode: "distributed"`
  /// cleanly rather than silently falling back to in-process.
  #[tokio::test]
  async fn defaults_to_none_and_attaches_when_set() {
    let plane = std::sync::Arc::new(AuthenticatedControlPlane::new(
      WorkerControlPlane::new(InMemoryWorkerProtocol::new()),
      crate::scheduler::WorkerAdmissionPolicy::default(),
    ));

    // No live Postgres needed — `connect_lazy` builds the pool
    // placeholder `AppState::new` expects, matching the same pattern
    // `yanshi-server/tests/metrics_endpoint.rs` already uses.
    let pool = sqlx::postgres::PgPoolOptions::new()
      .connect_lazy("postgres://postgres:postgres@localhost:5432/yanshi_test")
      .expect("lazy pg");
    let bare = AppState::new(yanshi_db::Database {
      pool,
      read_pool: None,
    });
    assert!(bare.worker_control_plane.is_none());

    let attached = bare.with_worker_control_plane(Some(plane));
    assert!(attached.worker_control_plane.is_some());

    let detached = attached.with_worker_control_plane(None);
    assert!(detached.worker_control_plane.is_none());
  }
}
