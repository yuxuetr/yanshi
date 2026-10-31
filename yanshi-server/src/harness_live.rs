//! Real LLM-backed [`HarnessSessionExecutor`] (P-H.5 slice 2).
//!
//! Replaces the [`StubHarnessExecutor`] for deployments that have an LLM
//! provider configured. Wires `yanshi-harness::HarnessRuntime` around
//! a `ReActAgent`, hooks tool execution through `wrap_registry` so the
//! shared `ServerApprovalProvider` can park decisions, and routes the
//! resulting `HarnessEvent` stream into the server's
//! [`HarnessEventBroker`] + Postgres event log.
//!
//! [`StubHarnessExecutor`]: crate::harness::StubHarnessExecutor

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::OnceCell;
use tracing::{error, info, warn};

use yanshi_agent_spi::checkpoint::{AgentLoopCheckpoint, AgentLoopCheckpointer, LoopRuntimeKind};
use yanshi_agents::plan_execute::{PlanExecuteAgent, PlanExecuteConfig};
use yanshi_agents::react::{ReActAgent, ReActConfig};
use yanshi_agents::runtime::{
  AgentCancellationToken, AgentRuntime, AgentStopReason, RuntimeLimits,
};
use yanshi_harness::{
  ApprovalProvider, HarnessEvent, HarnessEventBody, HarnessEventSink, HarnessProfile,
  HarnessRunOptions, HarnessRuntime, HarnessRuntimeKind, HookConfig, SeqAllocator, SinkChain,
  StopReason, StoppedPayload, default_providers, wrap_registry,
};
use yanshi_llm::Yanshi;
use yanshi_memory::{MemoryStore, SessionMemory, SqliteMemory};
use yanshi_skills::{SkillBuilder, SkillLoader};
use yanshi_tools::ToolRegistry;

use yanshi_db::{
  DbLoopCheckpointer, HarnessEventRepo, HarnessSessionRepo, HarnessSessionStatus,
  NewHarnessSessionEvent, Repositories,
};

use crate::events_stream::broker_finalize_grace;
use crate::harness::{
  HarnessEventBroker, HarnessSessionContext, HarnessSessionExecutor, StreamedHarnessEvent,
};
use crate::harness_approval::{PendingApprovalRegistry, ServerApprovalProvider};

/// HarnessEventSink that fans every envelope out to:
///
/// - the `harness_session_events` Postgres table (durable history,
///   serves SSE backfill and JSON history);
/// - the process-local [`HarnessEventBroker`] (live SSE push).
///
/// Failures persist as `tracing::warn!` and are otherwise non-fatal:
/// the agent run continues even if the event log is briefly unavailable,
/// since dropping a synthetic event is safer than aborting a real
/// session. Subscribers can reconnect with `?after_seq=` to refill from
/// the DB once writes recover.
pub struct ServerHarnessEventSink {
  repos: Repositories,
  broker: HarnessEventBroker,
}

impl ServerHarnessEventSink {
  pub fn new(repos: Repositories, broker: HarnessEventBroker) -> Self {
    Self { repos, broker }
  }
}

#[async_trait]
impl HarnessEventSink for ServerHarnessEventSink {
  fn name(&self) -> &str {
    "server"
  }

  async fn write(&self, event: &HarnessEvent) -> Result<(), yanshi_harness::HarnessError> {
    let Ok(session_uuid) = uuid::Uuid::parse_str(&event.session_id) else {
      // Non-UUID session id arrived (test runtime or external caller).
      // Drop with a warning; the contract guarantees server-managed
      // sessions always pass through `Uuid::new_v4()`.
      warn!(
        session_id = %event.session_id,
        seq = event.seq,
        "harness event sink: session id is not a UUID, skipping persistence"
      );
      return Ok(());
    };
    let kind = harness_event_kind(&event.body);
    let payload = serde_json::to_value(&event.body).unwrap_or(serde_json::Value::Null);

    let new_event = NewHarnessSessionEvent {
      session_id: session_uuid,
      seq: event.seq as i64,
      kind: kind.to_string(),
      payload,
    };
    match self.repos.harness_events.append(new_event).await {
      Ok(stored) => {
        // W4.2c: best-effort cross-replica wake-up, same contract as
        // `harness::publish_through` — never fail this write over a
        // NOTIFY hiccup, local delivery (below) already succeeded.
        let notify_payload = serde_json::json!({
          "session_id": stored.session_id,
          "seq": stored.seq,
        });
        if let Ok(payload) = serde_json::to_string(&notify_payload)
          && let Err(err) = yanshi_db::notify(
            &self.repos.harness_events.pool,
            crate::harness::HARNESS_EVENTS_NOTIFY_CHANNEL,
            &payload,
          )
          .await
        {
          warn!(
            session_id = %stored.session_id,
            error = %err,
            "harness event sink: cross-replica NOTIFY failed (local delivery already succeeded)"
          );
        }
        self.broker.publish(StreamedHarnessEvent::from(stored));
        Ok(())
      }
      Err(err) => {
        warn!(
          session_id = %event.session_id,
          seq = event.seq,
          error = %err,
          "harness event sink: persist failed"
        );
        // Surface to the runtime as Ok so the agent keeps running. The
        // event is lost from the live stream; subscribers can pull
        // history once persistence recovers.
        Ok(())
      }
    }
  }
}

pub(crate) fn harness_event_kind(body: &HarnessEventBody) -> &'static str {
  match body {
    HarnessEventBody::SessionStarted(_) => "session_started",
    HarnessEventBody::StepStarted(_) => "step_started",
    HarnessEventBody::ToolCallRequested(_) => "tool_call_requested",
    HarnessEventBody::ApprovalRequested(_) => "approval_requested",
    HarnessEventBody::ApprovalDecided(_) => "approval_decided",
    HarnessEventBody::ToolCallCompleted(_) => "tool_call_completed",
    HarnessEventBody::TokenDelta(_) => "token_delta",
    HarnessEventBody::InterruptRequested(_) => "interrupt_requested",
    HarnessEventBody::InterruptAnswered(_) => "interrupt_answered",
    HarnessEventBody::BackgroundTaskUpdated(_) => "background_task_updated",
    HarnessEventBody::MemorySummaryAdded(_) => "memory_summary_added",
    HarnessEventBody::Stopped(_) => "stopped",
  }
}

/// LLM-backed harness executor.
///
/// Each `execute` call assembles a fresh `ReActAgent` + `HarnessRuntime`
/// around the session's context (workspace_root, profile, runtime kind,
/// model). The executor calls [`Yanshi::init`] lazily on first use so
/// the test suite doesn't pay for provider config when running the stub
/// path.
///
/// W0.1: every session gets a real, governed tool registry — read-only
/// file access scoped to `workspace_root` plus outbound HTTP (see
/// `build_default_tool_registry`) — wrapped through `wrap_registry` with
/// `ServerApprovalProvider` so the approval pipeline has something to
/// actually govern. W4.1c: when the session names a resolvable skill
/// (`HarnessSessionContext::skill_dir`), it gets that skill's full agent
/// (persona/model/memory/knowledge, its tools rewrapped through the same
/// approval pipeline) instead of the hardcoded default — see
/// `build_session_react_agent`. MCP/plugin capability beyond a skill's own
/// declared MCP servers is still a follow-up.
#[derive(Clone)]
pub struct LiveHarnessExecutor {
  approval_registry: PendingApprovalRegistry,
  approval_timeout: Duration,
  /// Q3.4.3: bounds concurrent harness sessions. Each session spawns
  /// a dedicated OS thread via `spawn_blocking`; uncapped that's a
  /// DoS vector (`/v1/harness/sessions` has no rate limit). The
  /// semaphore caps in-flight executions; callers wait for a permit
  /// before starting their session's blocking runtime. Default cap
  /// is set by `default_max_concurrent_sessions()` (32) and is
  /// overridable via `with_max_concurrent_sessions()`.
  ///
  /// W4.2g: deliberately **not** externalized to Postgres the way
  /// `yanshi_server::runs::RunAdmissionRegistry` was in W4.2f, and
  /// this is a permanent design choice, not a gap awaiting a follow-up.
  /// The two guard different things: `RunAdmissionRegistry` enforces a
  /// per-tenant business SLA ("this tenant gets N concurrent runs")
  /// that must hold *across the whole cluster* — a purely local check
  /// there silently multiplies the effective limit by the replica
  /// count, a real correctness bug. This semaphore instead protects
  /// *this one process's* OS-thread budget (global, not per-tenant, by
  /// design — the DoS vector above is unbounded thread creation on a
  /// single host, not unbounded work for one tenant); each replica
  /// independently bounding its own thread count is the *correct*
  /// behavior for a per-process resource guard, not an instance of the
  /// same cross-replica multiplication bug. Making it cluster-wide
  /// would be actively wrong: a busy 3-replica cluster would then only
  /// ever run `default_max_concurrent_sessions()` (32) harness sessions
  /// *total*, not 32 per host.
  concurrency_limit: Arc<tokio::sync::Semaphore>,
}

/// Q3.4.3: production-safe default cap on concurrent live harness
/// sessions. Each session burns an OS thread for the duration of its
/// run, so the cap is the upper bound on extra OS threads the live
/// executor will materialize. 32 is a balance between local-dev
/// ergonomics (rarely hit) and shared-infra survival.
pub fn default_max_concurrent_sessions() -> usize {
  32
}

impl LiveHarnessExecutor {
  pub fn new(approval_registry: PendingApprovalRegistry, approval_timeout: Duration) -> Self {
    Self {
      approval_registry,
      approval_timeout,
      concurrency_limit: Arc::new(tokio::sync::Semaphore::new(
        default_max_concurrent_sessions(),
      )),
    }
  }

  /// Q3.4.3: override the default concurrency cap. `0` is treated as
  /// "1" so the executor always permits forward progress.
  pub fn with_max_concurrent_sessions(mut self, max: usize) -> Self {
    let max = max.max(1);
    self.concurrency_limit = Arc::new(tokio::sync::Semaphore::new(max));
    self
  }
}

impl std::fmt::Debug for LiveHarnessExecutor {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("LiveHarnessExecutor")
      .field("approval_timeout", &self.approval_timeout)
      .finish()
  }
}

#[async_trait]
impl HarnessSessionExecutor for LiveHarnessExecutor {
  async fn execute(&self, ctx: HarnessSessionContext) {
    let Some(_permit_guard) = self.acquire_permit(&ctx).await else {
      return;
    };
    if let Err(err) = live_execute(self, &ctx).await {
      handle_live_failure(&ctx, &err).await;
    }
  }

  async fn resume_interrupt(&self, ctx: HarnessSessionContext, answer: String) {
    let Some(_permit_guard) = self.acquire_permit(&ctx).await else {
      return;
    };
    if let Err(err) = live_resume_interrupt(self, &ctx, answer).await {
      handle_live_failure(&ctx, &err).await;
    }
  }
}

impl LiveHarnessExecutor {
  /// Q3.4.3: acquire the per-process concurrency permit before spawning
  /// the OS thread that runs a session. Without this gate a flood of
  /// `POST /v1/harness/sessions` (or `.../interrupt/answer`) would spawn
  /// one OS thread per request — `spawn_blocking` doesn't bound thread
  /// count by itself. `None` means the semaphore is permanently closed
  /// (collector shutdown); the caller should bail out without running.
  async fn acquire_permit(
    &self,
    ctx: &HarnessSessionContext,
  ) -> Option<tokio::sync::OwnedSemaphorePermit> {
    match self.concurrency_limit.clone().acquire_owned().await {
      Ok(permit) => Some(permit),
      Err(_closed) => {
        warn!(session_id = %ctx.session_id, "harness concurrency semaphore closed; rejecting session");
        None
      }
    }
  }
}

/// Shared failure path for [`LiveHarnessExecutor::execute`] and
/// [`LiveHarnessExecutor::resume_interrupt`]: mark the session `Failed`
/// and emit a terminal `stopped` event so SSE subscribers and event-log
/// consumers see the H0 contract's required close signal. Two failure
/// shapes need this:
///   1. the live call errored before `HarnessRuntime` could start (e.g.
///      LLM init / checkpoint load failed), so nothing but
///      `session_started` (or nothing at all) was ever written.
///   2. `HarnessRuntime` errored mid-way (inner agent failed) and does
///      not itself emit `stopped` on its error path.
///
/// Both leave the broker open and the event history missing a terminal
/// kind, which the closed kind set documented in `docs/HARNESS_MODE.md`
/// promises is always present.
async fn handle_live_failure(ctx: &HarnessSessionContext, err: &LiveExecutorError) {
  let err_msg = err.to_string();
  error!(session_id = %ctx.session_id, error = %err_msg, "live harness executor failed");
  let _ = ctx
    .repos
    .harness_sessions
    .update_status(
      ctx.session_id,
      HarnessSessionStatus::Failed,
      None,
      Some(&err_msg),
    )
    .await;
  emit_failure_stopped_event(ctx, &err_msg).await;
  ctx
    .broker
    .finalise_with_grace(ctx.session_id, broker_finalize_grace());
}

/// Persist + publish a synthetic `stopped` event with
/// `StopReason::Failed` for a session whose execution failed before the
/// runtime could emit its own terminal event. seq is computed from the
/// current `MAX(seq)` in the event log so the synthetic event always
/// lands after whatever the runtime did manage to write (typically a
/// solitary `session_started`).
async fn emit_failure_stopped_event(ctx: &HarnessSessionContext, err_msg: &str) {
  let next_seq = match ctx.repos.harness_events.max_seq(ctx.session_id).await {
    Ok(Some(max)) => (max as u64).saturating_add(1),
    Ok(None) => 0,
    Err(err) => {
      warn!(
        session_id = %ctx.session_id,
        error = %err,
        "harness failure-stopped emit: max_seq lookup failed, skipping",
      );
      return;
    }
  };
  let event = HarnessEvent {
    seq: next_seq,
    session_id: ctx.session_id.to_string(),
    ts: chrono::Utc::now(),
    body: HarnessEventBody::Stopped(StoppedPayload {
      reason: StopReason::Failed,
      final_answer: None,
      error: Some(err_msg.to_string()),
    }),
  };
  let sink = ServerHarnessEventSink::new(ctx.repos.clone(), ctx.broker.clone());
  if let Err(err) = sink.write(&event).await {
    warn!(
      session_id = %ctx.session_id,
      error = %err,
      "harness failure-stopped emit: sink write failed",
    );
  }
}

/// Lazy Yanshi init guard so the LLM registry is loaded at most once
/// per process. Subsequent calls are no-ops and return immediately.
async fn ensure_llm_initialized() -> Result<(), LiveExecutorError> {
  static INIT: OnceCell<()> = OnceCell::const_new();
  INIT
    .get_or_try_init(|| async { Yanshi::init().await.map_err(LiveExecutorError::from) })
    .await
    .map(|_| ())
}

/// Snapshot of the inputs the inner harness session needs. We move a
/// fresh owned copy onto the blocking thread so the spawned task is
/// `'static` and doesn't carry a borrow of [`HarnessSessionContext`].
#[derive(Clone)]
struct RunInputs {
  session_id: uuid::Uuid,
  user_input: String,
  workspace_root: String,
  profile: String,
  runtime_kind: String,
  model: String,
  skill_name: Option<String>,
  /// W4.1c: see `HarnessSessionContext::skill_dir`.
  skill_dir: Option<PathBuf>,
  /// U1.3: see `HarnessSessionContext::cost_limit_usd`.
  cost_limit_usd: Option<f64>,
  /// W0.1: see `HarnessSessionContext::max_steps`.
  max_steps: Option<usize>,
  repos: Repositories,
  broker: HarnessEventBroker,
  initial_seq: u64,
  cancellation_token: AgentCancellationToken,
}

fn clone_run_inputs(ctx: &HarnessSessionContext) -> RunInputs {
  RunInputs {
    session_id: ctx.session_id,
    user_input: ctx.user_input.clone(),
    workspace_root: ctx.workspace_root.clone(),
    profile: ctx.profile.clone(),
    runtime_kind: ctx.runtime_kind.clone(),
    model: ctx.model.clone(),
    skill_name: ctx.skill_name.clone(),
    skill_dir: ctx.skill_dir.clone(),
    cost_limit_usd: ctx.cost_limit_usd,
    max_steps: ctx.max_steps,
    repos: ctx.repos.clone(),
    broker: ctx.broker.clone(),
    cancellation_token: ctx.cancellation_token.clone(),
    initial_seq: ctx.initial_seq,
  }
}

/// W0.1: default step cap when the request doesn't specify one — matches
/// `RuntimeLimits::react_defaults()`.
const DEFAULT_MAX_STEPS: usize = 15;

/// W0.1: hard server-side ceiling regardless of what the request asks
/// for, so a careless or malicious caller can't run an unbounded loop.
const MAX_STEPS_CEILING: usize = 50;

fn resolve_max_steps(requested: Option<usize>) -> usize {
  requested
    .unwrap_or(DEFAULT_MAX_STEPS)
    .min(MAX_STEPS_CEILING)
}

/// W0.1: build the tool registry a harness session governs when it has no
/// skill to load from. Real tools — not an always-empty
/// `ToolRegistry::new()` — are what makes the hook/approval pipeline
/// meaningful: without them `wrap_registry` has nothing to wrap and the
/// session can never produce an `approval_requested` event, no matter how
/// the profile is configured. Read-only file access scoped to the
/// workspace root, plus outbound HTTP.
fn build_default_tool_registry(workspace_root: &str) -> Result<ToolRegistry, LiveExecutorError> {
  yanshi_tools::default_governed_registry(std::path::Path::new(workspace_root)).map_err(|err| {
    LiveExecutorError::Harness(yanshi_harness::HarnessError::Other(format!(
      "failed to build default tool registry: {err}"
    )))
  })
}

/// W4.1c: build the session's inner `ReActAgent`, skill-backed when
/// `inputs.skill_dir` resolves, the harness default otherwise.
///
/// Mirrors the CLI's own `--skill` branch
/// (`yanshi-cli/src/commands/harness/run.rs::build_agent`) exactly:
/// `SkillBuilder::build_with_project_root` builds a *full* agent — its own
/// persona, model, memory, and knowledge from the manifest, not just its
/// tools — since a skill-backed harness session should behave like running
/// that skill under harness governance, not like the harness's own default
/// persona with borrowed tools. `inputs.model`/`build_harness_memory` are
/// only consulted in the no-skill branch; the skill manifest wins when one
/// is present, same as the CLI. The resulting registry is snapshotted and
/// rewrapped through `hook_config` — `ReActAgent::tools()`/`with_tools()`
/// is the same post-construction registry-swap hook the CLI (and W4.1b's
/// `run_skill_agent`) already use, so a skill's tools stay governed by the
/// harness approval pipeline exactly like the default registry's tools do.
async fn build_session_react_agent(
  inputs: &RunInputs,
  hook_config: HookConfig,
) -> Result<ReActAgent, LiveExecutorError> {
  let Some(skill_dir) = inputs.skill_dir.as_deref() else {
    let tools = build_default_tool_registry(&inputs.workspace_root)?;
    let registry = wrap_registry(tools, hook_config);
    let react_config = ReActConfig::new(&inputs.model);
    let memory = build_harness_memory().await?;
    return Ok(ReActAgent::new(react_config, memory, Arc::new(registry)));
  };

  let manifest = SkillLoader::load(skill_dir).map_err(|e| {
    LiveExecutorError::Harness(yanshi_harness::HarnessError::Other(format!(
      "failed to load skill manifest at {}: {e}",
      skill_dir.display()
    )))
  })?;
  let _warnings = SkillLoader::validate(&manifest, skill_dir).map_err(|e| {
    LiveExecutorError::Harness(yanshi_harness::HarnessError::Other(format!(
      "skill validation failed: {e}"
    )))
  })?;
  let mut agent = SkillBuilder::build_with_project_root(
    &manifest,
    skill_dir,
    Some(std::path::Path::new(&inputs.workspace_root)),
  )
  .await
  .map_err(|e| {
    LiveExecutorError::Harness(yanshi_harness::HarnessError::Other(format!(
      "failed to build agent from skill manifest: {e}"
    )))
  })?;

  let mut snapshot = ToolRegistry::new();
  for tool in agent.tools().list() {
    snapshot.register(tool);
  }
  let wrapped = wrap_registry(snapshot, hook_config);
  agent = agent.with_tools(Arc::new(wrapped));
  Ok(agent)
}

/// Runs `HarnessRuntime::run` on a dedicated current-thread Tokio
/// runtime hosted inside `tokio::task::spawn_blocking`.
///
/// **Why:** `HarnessRuntime::run` holds `&self` across `.await` points
/// (it calls `self.collect_context(...).await` and friends). For its
/// future to be `Send`, `HarnessRuntime: Sync` would have to hold — but
/// the inner `Box<dyn AgentRuntime>` is `Send`-only because
/// `AgentRuntime: Send`. The smoke test in `yanshi-harness` works
/// around this by being a `current_thread` tokio test (no `Send`
/// requirement on the test future). Server-side we want the same
/// relaxed-Send execution environment without forcing the rest of the
/// server onto a current-thread runtime, so we offload onto
/// `spawn_blocking` and start an isolated current-thread runtime
/// there. The cost is one OS thread per concurrent harness session,
/// which is acceptable for now and is removed once `HarnessRuntime` is
/// updated to thread `&mut self` (or `Sync` is added to
/// `AgentRuntime`).
/// Build the harness agent's conversation memory.
///
/// When `YANSHI_HARNESS_MEMORY_DB` is set to a non-empty path, use a
/// persistent SQLite store (keyed by session_id, WAL + busy_timeout via
/// the shared sqlite pool) so a `:resume` reads the prior turns back
/// across process restarts — long-lived server sessions. Otherwise the
/// in-process default (unchanged behaviour). Opt-in because a shared
/// SQLite file is a single-node assumption; multi-node deployments should
/// front it with their own backend.
async fn build_harness_memory() -> Result<Box<dyn MemoryStore>, LiveExecutorError> {
  match std::env::var("YANSHI_HARNESS_MEMORY_DB")
    .ok()
    .filter(|p| !p.trim().is_empty())
  {
    Some(path) => open_persistent_harness_memory(path.trim()).await,
    None => Ok(Box::new(SessionMemory::default_window()) as Box<dyn MemoryStore>),
  }
}

/// Open a persistent SQLite conversation store at `path`, creating the
/// parent directory if needed. Keyed by session_id, so a resumed session
/// reads the prior turns back.
async fn open_persistent_harness_memory(
  path: &str,
) -> Result<Box<dyn MemoryStore>, LiveExecutorError> {
  let path = std::path::PathBuf::from(path);
  if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
    std::fs::create_dir_all(parent).map_err(|e| {
      LiveExecutorError::Harness(yanshi_harness::HarnessError::Other(format!(
        "could not create harness memory dir {}: {e}",
        parent.display()
      )))
    })?;
  }
  let store = SqliteMemory::open(&path).await.map_err(|e| {
    LiveExecutorError::Harness(yanshi_harness::HarnessError::Other(format!(
      "failed to open harness memory db {}: {e}",
      path.display()
    )))
  })?;
  Ok(Box::new(store))
}

/// sqlx pools must not cross Tokio runtimes. A connection dropped on the
/// session's inner runtime schedules its return to the pool on *that*
/// runtime, which shuts down before the task runs. The slot then leaks
/// for good: the shared pool drained one session at a time, until
/// `handle_live_failure`'s status write hit `PoolTimedOut` and the session
/// stayed `running` forever. Give the inner runtime its own small pool
/// (same connect options) and close it before the runtime exits.
async fn with_session_local_pool(
  mut inputs: RunInputs,
) -> Result<(RunInputs, sqlx::PgPool), LiveExecutorError> {
  let options = inputs.repos.harness_sessions.pool.connect_options();
  let pool = sqlx::postgres::PgPoolOptions::new()
    .max_connections(SESSION_POOL_CONNECTIONS)
    .connect_with((*options).clone())
    .await
    .map_err(yanshi_db::DbError::from)?;
  inputs.repos = Repositories::from_pool(pool.clone());
  Ok((inputs, pool))
}

/// Event sink, checkpointer and approvals of one session write serially
/// behind the emit lock; two connections leave room for one overlap.
const SESSION_POOL_CONNECTIONS: u32 = 2;

async fn run_harness_blocking(
  executor: LiveHarnessExecutor,
  inputs: RunInputs,
) -> Result<yanshi_harness::HarnessRunResult, LiveExecutorError> {
  let join = tokio::task::spawn_blocking(move || -> Result<_, LiveExecutorError> {
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .map_err(|err| {
        LiveExecutorError::Harness(yanshi_harness::HarnessError::Other(format!(
          "failed to build inner runtime: {err}"
        )))
      })?;
    rt.block_on(async move {
      let (inputs, pool) = with_session_local_pool(inputs).await?;
      let result = run_harness_inner(executor, inputs).await;
      pool.close().await;
      result
    })
  });
  match join.await {
    Ok(result) => result,
    Err(err) => Err(LiveExecutorError::Harness(
      yanshi_harness::HarnessError::Other(format!("harness task panicked: {err}")),
    )),
  }
}

async fn run_harness_inner(
  executor: LiveHarnessExecutor,
  inputs: RunInputs,
) -> Result<yanshi_harness::HarnessRunResult, LiveExecutorError> {
  let session_id_string = inputs.session_id.to_string();
  let profile = parse_profile(&inputs.profile);
  let runtime_kind = parse_runtime_kind(&inputs.runtime_kind);

  let server_sink: Arc<dyn HarnessEventSink> = Arc::new(ServerHarnessEventSink::new(
    inputs.repos.clone(),
    inputs.broker.clone(),
  ));
  let sinks = SinkChain::new().push(server_sink.clone());

  // Q1.7.1 + P-A3.4: one shared `SeqAllocator` for both the hook layer and the
  // runtime. Pre-Q1.7.1 they each owned an independent counter and mixed events
  // would collide on the JSON-Lines sink's `(session_id, seq)` PK. P-A3.4 adds
  // the emit lock to the shared unit so the hook layer's concurrent tool /
  // approval events and the runtime's live bridge events also reach the sink in
  // seq order, not just carry monotonic seq numbers.
  let seq_allocator = SeqAllocator::with_initial(inputs.initial_seq);

  let approval_provider: Arc<dyn ApprovalProvider> = Arc::new(ServerApprovalProvider::new(
    executor.approval_registry.clone(),
    executor.approval_timeout,
  ));

  let hook_config = HookConfig::new(session_id_string.clone(), approval_provider, sinks.clone())
    .with_profile(profile)
    .with_seq_allocator(seq_allocator.clone())
    .with_approval_timeout(executor.approval_timeout);

  let agent = build_session_react_agent(&inputs, hook_config).await?;

  // V2.3: attach the Postgres-backed checkpointer unconditionally — same
  // "on by default" posture the CLI's `harness run` already established.
  // A session has to be continuously checkpointed for `resume_interrupt`
  // to ever have something to load, once the loop pauses on `ask_user`.
  let checkpointer: Arc<dyn AgentLoopCheckpointer> = Arc::new(DbLoopCheckpointer::new(
    inputs.repos.harness_sessions.pool.clone(),
  ));

  let mut runtime = HarnessRuntime::new(Box::new(agent))
    .with_event_sink(server_sink.clone())
    .with_context_providers(default_providers())
    .with_seq_allocator(seq_allocator.clone());

  let options = HarnessRunOptions::new(
    inputs.user_input,
    PathBuf::from(&inputs.workspace_root),
    inputs.model,
  )
  .with_profile(profile)
  .with_runtime_kind(runtime_kind)
  .with_session_id(session_id_string)
  .with_cancellation_token(inputs.cancellation_token.clone())
  .with_loop_checkpointer(checkpointer)
  .with_limits(RuntimeLimits {
    max_steps: Some(resolve_max_steps(inputs.max_steps)),
    cost_limit_usd: inputs.cost_limit_usd,
    ..Default::default()
  });
  let options = match inputs.skill_name.as_ref() {
    Some(name) => options.with_skill_name(name.clone()),
    None => options,
  };

  let result = runtime.run(options).await?;
  Ok(result)
}

/// V2.3: like [`run_harness_blocking`] but resumes a session paused on
/// [`AgentStopReason::AwaitingInput`] with the user's `answer`, instead of
/// starting a fresh run.
async fn run_harness_resume_blocking(
  executor: LiveHarnessExecutor,
  inputs: RunInputs,
  checkpoint: AgentLoopCheckpoint,
  answer: String,
) -> Result<yanshi_harness::HarnessRunResult, LiveExecutorError> {
  let join = tokio::task::spawn_blocking(move || -> Result<_, LiveExecutorError> {
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .map_err(|err| {
        LiveExecutorError::Harness(yanshi_harness::HarnessError::Other(format!(
          "failed to build inner runtime: {err}"
        )))
      })?;
    rt.block_on(async move {
      let (inputs, pool) = with_session_local_pool(inputs).await?;
      let result = run_harness_resume_inner(executor, inputs, checkpoint, answer).await;
      pool.close().await;
      result
    })
  });
  match join.await {
    Ok(result) => result,
    Err(err) => Err(LiveExecutorError::Harness(
      yanshi_harness::HarnessError::Other(format!("harness task panicked: {err}")),
    )),
  }
}

/// V2.3: rebuild the inner agent matching `checkpoint.runtime_kind` (the
/// checkpoint is self-describing — it is not derived from `inputs.runtime_kind`,
/// since a checkpoint always records the runtime that actually produced it)
/// and dispatch through [`HarnessRuntime::resume_from_interrupt`].
async fn run_harness_resume_inner(
  executor: LiveHarnessExecutor,
  inputs: RunInputs,
  checkpoint: AgentLoopCheckpoint,
  answer: String,
) -> Result<yanshi_harness::HarnessRunResult, LiveExecutorError> {
  let session_id_string = inputs.session_id.to_string();
  let profile = parse_profile(&inputs.profile);

  let server_sink: Arc<dyn HarnessEventSink> = Arc::new(ServerHarnessEventSink::new(
    inputs.repos.clone(),
    inputs.broker.clone(),
  ));
  let sinks = SinkChain::new().push(server_sink.clone());
  let seq_allocator = SeqAllocator::with_initial(inputs.initial_seq);

  let approval_provider: Arc<dyn ApprovalProvider> = Arc::new(ServerApprovalProvider::new(
    executor.approval_registry.clone(),
    executor.approval_timeout,
  ));

  let hook_config = HookConfig::new(session_id_string.clone(), approval_provider, sinks.clone())
    .with_profile(profile)
    .with_seq_allocator(seq_allocator.clone())
    .with_approval_timeout(executor.approval_timeout);

  let checkpointer: Arc<dyn AgentLoopCheckpointer> = Arc::new(DbLoopCheckpointer::new(
    inputs.repos.harness_sessions.pool.clone(),
  ));

  // W4.1c: React is the only runtime kind a skill-backed session can ever
  // have produced (`SkillBuilder` only ever constructs a `ReActAgent` —
  // skill manifests don't declare a runtime pattern, matching the CLI's
  // own `--skill` restriction), so only that arm consults
  // `inputs.skill_dir`; PlanExecute always uses the harness default
  // registry, same as before this change.
  let inner_agent: Box<dyn AgentRuntime> = match checkpoint.runtime_kind {
    LoopRuntimeKind::React => Box::new(build_session_react_agent(&inputs, hook_config).await?),
    LoopRuntimeKind::PlanExecute => {
      let tools = build_default_tool_registry(&inputs.workspace_root)?;
      let registry = Arc::new(wrap_registry(tools, hook_config));
      let memory = build_harness_memory().await?;
      let plan_config = PlanExecuteConfig::new(&inputs.model);
      Box::new(PlanExecuteAgent::new(plan_config, memory, registry))
    }
  };

  let mut runtime = HarnessRuntime::new(inner_agent)
    .with_event_sink(server_sink.clone())
    .with_seq_allocator(seq_allocator.clone());

  let options = HarnessRunOptions::new(
    inputs.user_input,
    PathBuf::from(&inputs.workspace_root),
    inputs.model,
  )
  .with_cancellation_token(inputs.cancellation_token.clone())
  .with_loop_checkpointer(checkpointer)
  .with_limits(RuntimeLimits {
    max_steps: Some(resolve_max_steps(inputs.max_steps)),
    cost_limit_usd: inputs.cost_limit_usd,
    ..Default::default()
  });

  let result = runtime
    .resume_from_interrupt(options, checkpoint, answer)
    .await?;
  Ok(result)
}

async fn live_execute(
  executor: &LiveHarnessExecutor,
  ctx: &HarnessSessionContext,
) -> Result<(), LiveExecutorError> {
  ensure_llm_initialized().await?;
  let result = run_harness_blocking(executor.clone(), clone_run_inputs(ctx)).await?;
  finish_live_result(ctx, &result).await
}

/// V2.3: resume a session paused on `awaiting_input` with the user's
/// `answer`. Loads the loop checkpoint the pause saved, rebuilds the
/// matching agent (`checkpoint.runtime_kind`), and dispatches through
/// `HarnessRuntime::resume_from_interrupt`.
async fn live_resume_interrupt(
  executor: &LiveHarnessExecutor,
  ctx: &HarnessSessionContext,
  answer: String,
) -> Result<(), LiveExecutorError> {
  ensure_llm_initialized().await?;
  let checkpointer = DbLoopCheckpointer::new(ctx.repos.harness_sessions.pool.clone());
  let checkpoint = checkpointer
    .load(&ctx.session_id.to_string())
    .await?
    .ok_or_else(|| {
      LiveExecutorError::Harness(yanshi_harness::HarnessError::Other(format!(
        "no loop checkpoint found for session {}",
        ctx.session_id
      )))
    })?;
  let result =
    run_harness_resume_blocking(executor.clone(), clone_run_inputs(ctx), checkpoint, answer)
      .await?;
  finish_live_result(ctx, &result).await
}

/// Persist the outcome of a fresh run or an interrupt resume, shared by
/// [`live_execute`] and [`live_resume_interrupt`].
///
/// `AgentStopReason::AwaitingInput` is a non-terminal pause, not a
/// terminal status transition like every other variant — it records the
/// question onto the session row's `pending_question*` columns via
/// `set_pending_question` instead of `update_status`, and returns early
/// without finalising the broker channel (the session isn't done; a
/// later `POST .../interrupt/answer` reuses the same channel).
async fn finish_live_result(
  ctx: &HarnessSessionContext,
  result: &yanshi_harness::HarnessRunResult,
) -> Result<(), LiveExecutorError> {
  if let AgentStopReason::AwaitingInput { question } = &result.stop_reason {
    let step_index = result
      .inner
      .steps
      .last()
      .map(|step| step.index as i64)
      .unwrap_or(0);
    ctx
      .repos
      .harness_sessions
      .set_pending_question(ctx.session_id, question, step_index)
      .await?;
    info!(session_id = %ctx.session_id, "live harness executor paused awaiting input");
    return Ok(());
  }

  // Map the inner agent's stop reason back to the session row's
  // terminal state. The closed `AgentStopReason` enum keeps the match
  // exhaustive at compile time, so new variants surface as errors here
  // rather than silently turning into `Failed`.
  let (status, final_answer, error) = match &result.stop_reason {
    AgentStopReason::FinalAnswer | AgentStopReason::StopCondition { .. } => {
      (HarnessSessionStatus::Completed, result.answer.clone(), None)
    }
    AgentStopReason::MaxSteps { max_steps } => (
      HarnessSessionStatus::Failed,
      result.answer.clone(),
      Some(format!("max_steps_reached:{max_steps}")),
    ),
    AgentStopReason::MaxToolCalls { max_tool_calls } => (
      HarnessSessionStatus::Failed,
      result.answer.clone(),
      Some(format!("max_tool_calls_reached:{max_tool_calls}")),
    ),
    AgentStopReason::Timeout { timeout_ms } => (
      HarnessSessionStatus::Failed,
      result.answer.clone(),
      Some(format!("timeout:{timeout_ms}ms")),
    ),
    AgentStopReason::Cancelled { message } => (
      HarnessSessionStatus::Cancelled,
      None,
      Some(format!("cancelled:{message}")),
    ),
    AgentStopReason::TokenBudgetExceeded { used, budget } => (
      HarnessSessionStatus::Failed,
      result.answer.clone(),
      Some(format!("token_budget_exceeded:{used}/{budget}")),
    ),
    AgentStopReason::CostLimitExceeded {
      used_usd,
      budget_usd,
    } => (
      HarnessSessionStatus::Failed,
      result.answer.clone(),
      Some(format!(
        "cost_limit_exceeded:${used_usd:.4}/${budget_usd:.4}"
      )),
    ),
    AgentStopReason::LoopDetected { tool, repeats } => (
      HarnessSessionStatus::Failed,
      result.answer.clone(),
      Some(format!("loop_detected:{tool}x{repeats}")),
    ),
    AgentStopReason::Error { message } => (
      HarnessSessionStatus::Failed,
      None,
      Some(format!("agent_error:{message}")),
    ),
    // Handled by the early return above.
    AgentStopReason::AwaitingInput { .. } => unreachable!("AwaitingInput handled above"),
    // W0.5: DenyAndStop's terminal state.
    AgentStopReason::ApprovalDenied { message } => (
      HarnessSessionStatus::Failed,
      None,
      Some(format!("approval_denied:{message}")),
    ),
  };
  ctx
    .repos
    .harness_sessions
    .update_status(
      ctx.session_id,
      status,
      final_answer.as_deref(),
      error.as_deref(),
    )
    .await?;

  ctx
    .broker
    .finalise_with_grace(ctx.session_id, broker_finalize_grace());
  info!(session_id = %ctx.session_id, "live harness executor finished");
  Ok(())
}

fn parse_profile(value: &str) -> HarnessProfile {
  match value {
    "dev" => HarnessProfile::Dev,
    "production" => HarnessProfile::Production,
    _ => HarnessProfile::Local,
  }
}

fn parse_runtime_kind(value: &str) -> HarnessRuntimeKind {
  match value {
    "plan_execute" => HarnessRuntimeKind::PlanExecute,
    _ => HarnessRuntimeKind::React,
  }
}

#[derive(Debug, thiserror::Error)]
enum LiveExecutorError {
  #[error(transparent)]
  Llm(#[from] yanshi_llm::LLMError),
  #[error(transparent)]
  Harness(#[from] yanshi_harness::HarnessError),
  #[error(transparent)]
  Db(#[from] yanshi_db::DbError),
  #[error(transparent)]
  Checkpoint(#[from] yanshi_agent_spi::checkpoint::AgentLoopCheckpointError),
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Resume contract: a persistent harness store at a path keeps a
  /// session's turns across re-opens (keyed by session_id), so a `:resume`
  /// against the same DB reads the prior conversation back. Tested via
  /// the path helper (no env) to stay race-free.
  #[tokio::test]
  async fn persistent_harness_memory_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("harness-memory.sqlite");
    let path = db.to_string_lossy().into_owned();

    let first = open_persistent_harness_memory(&path).await.unwrap();
    first
      .add_message(yanshi_memory::Message::user(
        "sess-srv",
        "remember the deploy key",
      ))
      .await
      .unwrap();
    drop(first);

    let second = open_persistent_harness_memory(&path).await.unwrap();
    let history = second.get_all("sess-srv").await.unwrap();
    assert!(
      history
        .iter()
        .any(|m| m.content.contains("remember the deploy key")),
      "resume must restore the prior conversation from the persistent store"
    );
  }

  use chrono::Utc;
  use yanshi_harness::{
    HarnessEvent, HarnessEventBody, SessionStartedPayload, StopReason, StoppedPayload,
  };

  #[test]
  fn parse_profile_falls_back_to_local() {
    assert!(matches!(parse_profile("dev"), HarnessProfile::Dev));
    assert!(matches!(
      parse_profile("production"),
      HarnessProfile::Production
    ));
    assert!(matches!(parse_profile("local"), HarnessProfile::Local));
    assert!(matches!(parse_profile(""), HarnessProfile::Local));
    assert!(matches!(parse_profile("wat"), HarnessProfile::Local));
  }

  #[test]
  fn parse_runtime_kind_defaults_to_react() {
    assert!(matches!(
      parse_runtime_kind("react"),
      HarnessRuntimeKind::React
    ));
    assert!(matches!(
      parse_runtime_kind("plan_execute"),
      HarnessRuntimeKind::PlanExecute
    ));
    assert!(matches!(
      parse_runtime_kind("unknown"),
      HarnessRuntimeKind::React
    ));
  }

  #[test]
  fn harness_event_kind_covers_every_variant() {
    // Sanity check: each variant's kind() matches the canonical wire
    // name. The closed enum guarantees this exhaustively at compile
    // time; the assertions guard against future renames.
    let started = HarnessEvent {
      seq: 0,
      session_id: "s".into(),
      ts: chrono::Utc::now(),
      body: HarnessEventBody::SessionStarted(SessionStartedPayload {
        workspace_root: "/".into(),
        runtime: HarnessRuntimeKind::React,
        profile: HarnessProfile::Local,
        model: "m".into(),
        skills: Vec::new(),
        context_item_count: 0,
        context_token_estimate: 0,
      }),
    };
    let stopped = HarnessEvent {
      seq: 1,
      session_id: "s".into(),
      ts: Utc::now(),
      body: HarnessEventBody::Stopped(StoppedPayload {
        reason: StopReason::Completed,
        final_answer: None,
        error: None,
      }),
    };
    assert_eq!(harness_event_kind(&started.body), "session_started");
    assert_eq!(harness_event_kind(&stopped.body), "stopped");
  }

  #[test]
  fn resolve_max_steps_defaults_and_clamps() {
    assert_eq!(resolve_max_steps(None), DEFAULT_MAX_STEPS);
    assert_eq!(resolve_max_steps(Some(5)), 5);
    assert_eq!(resolve_max_steps(Some(10_000)), MAX_STEPS_CEILING);
  }

  /// W0.1: the default (no-`--skill`) registry must actually contain
  /// governed tools — this is the regression test for the bug where
  /// every harness session (server or CLI) started from an always-empty
  /// `ToolRegistry::new()`, leaving the approval/hook pipeline nothing
  /// to ever govern.
  #[test]
  fn build_default_tool_registry_contains_file_and_http() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = build_default_tool_registry(&tmp.path().display().to_string()).unwrap();
    let names: Vec<String> = registry
      .list()
      .iter()
      .map(|tool| tool.name().to_string())
      .collect();
    assert!(names.contains(&"file".to_string()), "names: {names:?}");
    assert!(names.contains(&"http".to_string()), "names: {names:?}");
  }
}
