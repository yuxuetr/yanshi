//! Harness Mode HTTP surface (P-H.5).
//!
//! Exposes agent-native sessions (`yanshi-harness`) over the same
//! gateway Axum shell that hosts the workflow `/v1/runs` routes. The
//! initial slice mirrors the workflow runs surface:
//!
//! - `POST /v1/harness/sessions` — create a session, return id.
//! - `GET  /v1/harness/sessions` — list sessions for a tenant.
//! - `GET  /v1/harness/sessions/{id}` — read current state.
//! - `POST /v1/harness/sessions/{id}:cancel` — request cancellation.
//! - `GET  /v1/harness/sessions/{id}/events` — SSE with backfill.
//! - `GET  /v1/harness/sessions/{id}/events/history` — JSON history.
//!
//! Approval routes (`/approvals`, `/approvals/{id}`) and the
//! LLM-backed executor are deferred to follow-up P-H.5 slices so this
//! initial commit can land the schema + route plumbing without
//! standing up the full agent stack on the server.
//!
//! The route layer stays narrow: it owns the session row + event log
//! transitions, dispatches a [`HarnessSessionExecutor`] in the
//! background, and surfaces persisted events via a process-local
//! [`HarnessEventBroker`]. Real execution lives behind the executor
//! trait so we can swap [`StubHarnessExecutor`] for a real
//! `HarnessRuntime` later without touching the route layer.

use async_trait::async_trait;
use axum::{
  Extension, Json,
  extract::{Path, Query, State},
  response::sse::{Event, KeepAlive, Sse},
};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{error, info, warn};
use uuid::Uuid;
use yanshi_agents::AgentCancellationToken;

use yanshi_db::{
  DbError, HarnessEventRepo, HarnessSession, HarnessSessionRepo, HarnessSessionStatus,
  NewHarnessSession, NewHarnessSessionEvent, Notice, NotifyListener, Repositories,
};

use crate::AppState;
use crate::error::{ApiError, JsonReq};
use crate::events_stream::broker_finalize_grace;
use crate::tenant::TenantId;

/// Channel capacity per session. Slow subscribers drop oldest events when
/// they fall this far behind; the SSE handler logs a warning and lets the
/// client reconnect with `?after_seq=` to refill from the DB.
const SESSION_CHANNEL_CAPACITY: usize = 256;

/// Postgres NOTIFY channel carrying cross-replica harness-event
/// wake-ups (W4.2c). Mirrors `events_stream::RUN_EVENTS_NOTIFY_CHANNEL`.
pub const HARNESS_EVENTS_NOTIFY_CHANNEL: &str = "yanshi_harness_events";

/// Default profile assigned when the request body omits one. Matches the
/// CLI default so the surface is consistent across entry points.
const DEFAULT_PROFILE: &str = "local";

/// Default runtime kind when the body omits one.
const DEFAULT_RUNTIME_KIND: &str = "react";

/// Default model handle the stub executor records on the session row.
/// Real executor will sniff this from the request body or skill config.
const DEFAULT_MODEL: &str = "stub";

/// Wire shape published over SSE. Mirrors `yanshi_db::HarnessSessionEvent`
/// but stays minimal so we don't tie SSE consumers to DB columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamedHarnessEvent {
  pub session_id: Uuid,
  pub seq: i64,
  pub kind: String,
  pub payload: serde_json::Value,
  pub ts: chrono::DateTime<chrono::Utc>,
}

impl From<yanshi_db::HarnessSessionEvent> for StreamedHarnessEvent {
  fn from(e: yanshi_db::HarnessSessionEvent) -> Self {
    Self {
      session_id: e.session_id,
      seq: e.seq,
      kind: e.kind,
      payload: e.payload,
      ts: e.ts,
    }
  }
}

/// One session's local broadcast channel plus the highest `seq` this
/// replica has already forwarded into it. Mirrors
/// `events_stream::RunChannel` — see that type's doc comment for why.
struct SessionChannel {
  sender: broadcast::Sender<StreamedHarnessEvent>,
  last_forwarded_seq: i64,
}

impl SessionChannel {
  fn new() -> Self {
    Self {
      sender: broadcast::channel(SESSION_CHANNEL_CAPACITY).0,
      last_forwarded_seq: -1,
    }
  }
}

/// Broker over a sharded broadcast channel keyed by `session_id`. Same
/// pattern as the workflow `EventBroker` (including W4.2c's
/// NOTIFY-driven cross-replica catch-up); the two are intentionally
/// distinct types so a slow workflow subscriber can't lag a harness
/// session and vice versa.
#[derive(Clone, Default)]
pub struct HarnessEventBroker {
  inner: Arc<Mutex<HashMap<Uuid, SessionChannel>>>,
}

impl std::fmt::Debug for HarnessEventBroker {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let len = self.inner.lock().map(|g| g.len()).unwrap_or(0);
    f.debug_struct("HarnessEventBroker")
      .field("active_sessions", &len)
      .finish()
  }
}

impl HarnessEventBroker {
  pub fn new() -> Self {
    Self::default()
  }

  /// Lock the per-session channel map, recovering on poison so a panicked
  /// publisher can't deadlock every subsequent subscriber. The map is
  /// mutated one channel-entry at a time, so the inner state is sound to
  /// keep using after a poisoning panic. Mirrors `EventBroker::lock_inner`
  /// in `events_stream.rs`. (Q5.1)
  fn lock_inner(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, SessionChannel>> {
    match self.inner.lock() {
      Ok(g) => g,
      Err(poisoned) => poisoned.into_inner(),
    }
  }

  /// Subscribe to live events for `session_id`. Creates the channel if no
  /// subscriber has registered for this session yet so publishers don't
  /// need to coordinate with subscribers.
  pub fn subscribe(&self, session_id: Uuid) -> broadcast::Receiver<StreamedHarnessEvent> {
    let mut map = self.lock_inner();
    map
      .entry(session_id)
      .or_insert_with(SessionChannel::new)
      .sender
      .subscribe()
  }

  /// Publish without persisting. Use [`publish_through`] (free function
  /// below) when you also want a DB row.
  pub fn publish(&self, event: StreamedHarnessEvent) {
    let mut map = self.lock_inner();
    let channel = map
      .entry(event.session_id)
      .or_insert_with(SessionChannel::new);
    channel.last_forwarded_seq = channel.last_forwarded_seq.max(event.seq);
    let _ = channel.sender.send(event);
  }

  /// Drop the channel for a finished session so it doesn't leak. Safe to
  /// call multiple times.
  pub fn finalise(&self, session_id: Uuid) {
    let mut map = self.lock_inner();
    map.remove(&session_id);
  }

  /// Like [`Self::finalise`] but defers the actual channel removal by
  /// `grace`. Mirrors the workflow broker's grace-window pattern so
  /// in-flight SSE subscribers can drain the terminal event from the
  /// broadcast buffer before the sender is dropped.
  pub fn finalise_with_grace(&self, session_id: Uuid, grace: Duration) {
    let broker = self.clone();
    tokio::spawn(async move {
      if !grace.is_zero() {
        tokio::time::sleep(grace).await;
      }
      broker.finalise(session_id);
    });
  }

  /// Snapshot of the active per-session channel count. Cheap to call;
  /// used by tests and lightweight diagnostics.
  pub fn active_sessions(&self) -> usize {
    self.inner.lock().map(|m| m.len()).unwrap_or(0)
  }

  /// Returns the number of receivers currently subscribed to
  /// `session_id`. `0` when the channel is missing entirely.
  pub fn receiver_count(&self, session_id: Uuid) -> usize {
    let map = self.lock_inner();
    map
      .get(&session_id)
      .map(|channel| channel.sender.receiver_count())
      .unwrap_or(0)
  }

  /// W4.2c: mirrors `EventBroker::catchup_baseline` — see that method's
  /// doc comment.
  pub fn catchup_baseline(&self, session_id: Uuid) -> Option<i64> {
    let map = self.lock_inner();
    let channel = map.get(&session_id)?;
    if channel.sender.receiver_count() == 0 {
      return None;
    }
    Some(channel.last_forwarded_seq)
  }

  /// Mirrors `EventBroker::catchup_targets`.
  pub fn catchup_targets(&self) -> Vec<(Uuid, i64)> {
    self
      .lock_inner()
      .iter()
      .filter(|(_, channel)| channel.sender.receiver_count() > 0)
      .map(|(session_id, channel)| (*session_id, channel.last_forwarded_seq))
      .collect()
  }
}

/// Persist + publish a harness event in one shot. Mirrors
/// `events_stream::publish_through` — see that function's doc comment
/// for the NOTIFY/best-effort contract.
pub async fn publish_through(
  repos: &Repositories,
  broker: &HarnessEventBroker,
  event: NewHarnessSessionEvent,
) -> Result<(), DbError> {
  let stored = repos.harness_events.append(event).await?;
  let notify_payload = HarnessEventNotifyPayload {
    session_id: stored.session_id,
    seq: stored.seq,
  };
  broker.publish(StreamedHarnessEvent::from(stored));
  if let Ok(payload) = serde_json::to_string(&notify_payload)
    && let Err(err) = yanshi_db::notify(
      &repos.harness_events.pool,
      HARNESS_EVENTS_NOTIFY_CHANNEL,
      &payload,
    )
    .await
  {
    warn!(
      session_id = %notify_payload.session_id,
      error = %err,
      "harness publish_through: cross-replica NOTIFY failed (local delivery already succeeded)"
    );
  }
  Ok(())
}

/// Payload carried on [`HARNESS_EVENTS_NOTIFY_CHANNEL`]. Mirrors
/// `events_stream::RunEventNotifyPayload`, minus `tenant_id` —
/// `harness_session_events` rows don't carry one directly (unlike
/// `events`), so the listener below looks it up via
/// `HarnessSessionRepo::get` before paging, same as
/// `HarnessEventRepo::list_after`'s own callers already do.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HarnessEventNotifyPayload {
  session_id: Uuid,
  seq: i64,
}

/// W4.2c: cross-replica catch-up loop for [`HarnessEventBroker`]. Mirrors
/// `events_stream::spawn_run_events_listener` — see that function's doc
/// comment for the full mechanism. The one structural difference:
/// `HarnessEventRepo::list_after` requires a `tenant_id`, which isn't in
/// the NOTIFY payload (kept small on purpose) — this listener looks the
/// session's tenant up once via `HarnessSessionRepo::get` before paging,
/// tolerating a session that's since been deleted by skipping silently.
pub fn spawn_harness_events_listener(
  pool: sqlx::PgPool,
  repos: Repositories,
  broker: HarnessEventBroker,
) {
  tokio::spawn(async move {
    loop {
      let mut listener =
        match NotifyListener::connect(&pool, &[HARNESS_EVENTS_NOTIFY_CHANNEL]).await {
          Ok(listener) => listener,
          Err(err) => {
            warn!(error = %err, "harness events listener: connect failed, retrying in 5s");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
          }
        };
      // See `events_stream::spawn_run_events_listener`: resync after
      // every (re)connect because NOTIFY is at-most-once.
      for (session_id, baseline) in broker.catchup_targets() {
        if let Ok(Some(session)) = repos.harness_sessions.get(session_id).await
          && let Err(err) =
            catch_up_harness_events(&repos, &broker, &session.tenant_id, session_id, baseline).await
        {
          warn!(%session_id, error = %err, "harness events listener: resync fetch failed");
        }
      }
      loop {
        let payload = match listener.recv().await {
          Ok(Notice::Message { payload, .. }) => payload,
          Ok(Notice::Gap) => {
            warn!("harness events listener: connection dropped, reconnecting and resyncing");
            break;
          }
          Err(err) => {
            warn!(error = %err, "harness events listener: recv failed, reconnecting");
            break;
          }
        };
        let notify_payload: HarnessEventNotifyPayload = match serde_json::from_str(&payload) {
          Ok(parsed) => parsed,
          Err(err) => {
            warn!(error = %err, "harness events listener: malformed notify payload, skipping");
            continue;
          }
        };
        let Some(baseline) = broker.catchup_baseline(notify_payload.session_id) else {
          continue;
        };
        if notify_payload.seq <= baseline {
          continue;
        }
        let tenant_id = match repos.harness_sessions.get(notify_payload.session_id).await {
          Ok(Some(session)) => session.tenant_id,
          Ok(None) => continue,
          Err(err) => {
            warn!(
              session_id = %notify_payload.session_id,
              error = %err,
              "harness events listener: session lookup failed"
            );
            continue;
          }
        };
        if let Err(err) = catch_up_harness_events(
          &repos,
          &broker,
          &tenant_id,
          notify_payload.session_id,
          baseline,
        )
        .await
        {
          warn!(
            session_id = %notify_payload.session_id,
            error = %err,
            "harness events listener: catch-up fetch failed"
          );
        }
      }
    }
  });
}

/// Pages `HarnessEventRepo::list_after` starting from `after_seq` and
/// republishes every returned row into `broker`'s local channel for
/// `session_id`.
async fn catch_up_harness_events(
  repos: &Repositories,
  broker: &HarnessEventBroker,
  tenant_id: &str,
  session_id: Uuid,
  mut after_seq: i64,
) -> Result<(), DbError> {
  loop {
    let page = repos
      .harness_events
      .list_after(tenant_id, session_id, after_seq, 200)
      .await?;
    if page.is_empty() {
      return Ok(());
    }
    after_seq = page.last().map(|e| e.seq).unwrap_or(after_seq);
    for event in page {
      broker.publish(StreamedHarnessEvent::from(event));
    }
  }
}

/// JSON body for `POST /v1/harness/sessions`.
#[derive(Debug, Deserialize)]
pub struct CreateHarnessSessionRequest {
  /// Operator-facing prompt or instruction handed to the agent.
  pub user_input: String,
  /// Workspace root the agent may access. Stored verbatim; sandbox
  /// enforcement is the executor's responsibility, not the route layer.
  pub workspace_root: String,
  /// Tenant scope. Defaults to `"default"` so single-tenant deployments
  /// don't need to spell it out.
  #[serde(default)]
  pub tenant_id: Option<String>,
  /// Security profile (`dev` / `local` / `production`). Defaults to
  /// `local` to match the CLI default and keep the surface conservative.
  #[serde(default)]
  pub profile: Option<String>,
  /// Runtime kind (`react`, `plan_execute`, ...). Defaults to `react`.
  #[serde(default)]
  pub runtime_kind: Option<String>,
  /// Optional model handle. The stub executor stores it verbatim; the
  /// real executor will pass it through `Yanshi::model(...)`.
  #[serde(default)]
  pub model: Option<String>,
  /// Optional named skill to load.
  #[serde(default)]
  pub skill_name: Option<String>,
  /// Maximum estimated USD cost for the session (U1.3 — CLI/API entry
  /// point for the `RuntimeLimits::cost_limit_usd` enforcement T1.1
  /// wired into `ReActAgent`/`PlanExecuteAgent`). Unset means no
  /// cost-based stop condition.
  #[serde(default)]
  pub cost_limit_usd: Option<f64>,
  /// W0.1: maximum agent-loop steps (`RuntimeLimits::max_steps`). Unset
  /// falls back to a conservative server default; either way the server
  /// still clamps to its own ceiling — see `harness_live::resolve_max_steps`.
  #[serde(default)]
  pub max_steps: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct CreateHarnessSessionResponse {
  pub session_id: Uuid,
  pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct HarnessSessionResponse {
  #[serde(flatten)]
  pub session: HarnessSession,
}

#[derive(Debug, Serialize)]
pub struct ListHarnessSessionsResponse {
  pub sessions: Vec<HarnessSession>,
}

#[derive(Debug, Deserialize)]
pub struct ListHarnessSessionsQuery {
  /// Max rows to return, clamped to 1..=100.
  #[serde(default)]
  pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CancelHarnessSessionResponse {
  #[serde(flatten)]
  pub session: HarnessSession,
  pub cancelled: bool,
}

#[derive(Debug, Deserialize)]
pub struct HarnessEventsQuery {
  /// Resume after this `seq`. SSE clients reconnecting after a network
  /// blip pass the last seq they saw to avoid duplicates and gaps.
  #[serde(default)]
  pub after_seq: Option<i64>,
}

/// Background execution contract for harness sessions.
///
/// Implementations own every state transition after the route layer
/// inserts the session row, including terminal status updates and event
/// emission. Today the only impl is [`StubHarnessExecutor`]; wiring
/// `yanshi-harness::HarnessRuntime` end-to-end through this trait is
/// tracked as a P-H.5 follow-up so we can land the schema + routes
/// without standing up an LLM-backed agent on the server.
#[async_trait]
pub trait HarnessSessionExecutor: Send + Sync {
  async fn execute(&self, ctx: HarnessSessionContext);

  /// V2.3: resume a session paused on `AwaitingInput` with the user's
  /// `answer`. Default fails the session outright — an executor that
  /// never produces an `awaiting_input` session (like
  /// [`StubHarnessExecutor`], which never runs a real agent loop) never
  /// has a checkpoint to resume from, so this path should be
  /// unreachable for it; the default exists purely so the trait stays
  /// object-safe for both impls without forcing a no-op override.
  async fn resume_interrupt(&self, ctx: HarnessSessionContext, answer: String) {
    let _ = answer;
    error!(session_id = %ctx.session_id, "resume_interrupt called on an executor that does not support it");
    let _ = ctx
      .repos
      .harness_sessions
      .update_status(
        ctx.session_id,
        HarnessSessionStatus::Failed,
        None,
        Some("this executor does not support interrupt resume"),
      )
      .await;
    ctx
      .broker
      .finalise_with_grace(ctx.session_id, broker_finalize_grace());
  }
}

/// Everything an executor needs to do its job. Owns its own copies of the
/// repositories and broker so the route handler can return immediately.
pub struct HarnessSessionContext {
  pub session_id: Uuid,
  pub user_input: String,
  pub workspace_root: String,
  pub profile: String,
  pub runtime_kind: String,
  pub model: String,
  pub skill_name: Option<String>,
  /// W4.1c: the resolved skill's directory, when `skill_name` names an
  /// installed skill — `state.skills.resolve(name)` looked up fresh at
  /// `HarnessSessionContext`-construction time (not persisted; a skill's
  /// on-disk path can move between server restarts, and the catalog is
  /// the source of truth `/v1/skills` already uses). `None` when
  /// `skill_name` is `None` or the name doesn't resolve — the live
  /// executor falls back to the hardcoded default tool registry either
  /// way, matching pre-W4.1c behavior.
  pub skill_dir: Option<PathBuf>,
  /// U1.3: see `CreateHarnessSessionRequest::cost_limit_usd`.
  pub cost_limit_usd: Option<f64>,
  /// W0.1: see `CreateHarnessSessionRequest::max_steps`.
  pub max_steps: Option<usize>,
  pub repos: Repositories,
  /// Forwards events to live SSE subscribers. Persisting to the DB still
  /// has to happen — use [`publish_through`] for the standard path.
  pub broker: HarnessEventBroker,
  /// Seq number the executor must use for the first event it emits.
  /// Fresh submissions and rerun resumes use `0`; append-mode resumes
  /// pass `MAX(existing seq) + 1` so new events extend the persisted
  /// log without colliding with prior `(session_id, seq)` rows.
  pub initial_seq: u64,
  /// Signalled by `POST /v1/harness/sessions/{id}:cancel` (eval
  /// 2026-10-07 P1); the live executor hands it to the agent loop.
  pub cancellation_token: AgentCancellationToken,
}

/// Process-local session → cancellation token map, so `:cancel` can stop
/// the agent loop on this replica instead of only flipping the row.
#[derive(Clone, Default)]
pub struct HarnessCancellationRegistry {
  inner: Arc<Mutex<HashMap<Uuid, (u64, AgentCancellationToken)>>>,
  next_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl HarnessCancellationRegistry {
  fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, (u64, AgentCancellationToken)>> {
    self
      .inner
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }

  /// Register a fresh token for `id`; the entry lives until the returned
  /// guard drops (i.e. until the executor task finishes).
  pub fn register(&self, id: Uuid) -> HarnessCancellationGuard {
    let generation = self
      .next_generation
      .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let token = AgentCancellationToken::new();
    self.lock().insert(id, (generation, token.clone()));
    HarnessCancellationGuard {
      registry: self.clone(),
      id,
      generation,
      token,
    }
  }

  /// Sessions currently executing on this replica.
  pub fn active_ids(&self) -> Vec<Uuid> {
    self.lock().keys().copied().collect()
  }

  /// Returns whether a running session on this replica was signalled.
  pub fn cancel(&self, id: Uuid) -> bool {
    match self.lock().get(&id) {
      Some((_, token)) => {
        token.cancel();
        true
      }
      None => false,
    }
  }
}

/// Postgres NOTIFY channel a cancel request uses to reach a session
/// executing on another replica. Payload: the bare session id.
pub const HARNESS_CANCELLATION_NOTIFY_CHANNEL: &str = "yanshi_harness_cancellations";

/// Eval 2026-10-07 P1: stop sessions cancelled through another replica.
/// The `cancelled` row is the durable intent; the NOTIFY only says when
/// to look. A notified id is acted on only if this replica runs that
/// session and its row is still `cancelled`, so a late notification
/// can't stop a session that was resumed in between. NOTIFY is
/// at-most-once, so after every (re)connect every local session is
/// checked against its row.
pub fn spawn_harness_cancellation_listener(
  repos: Repositories,
  registry: HarnessCancellationRegistry,
) {
  let pool = repos.harness_sessions.pool.clone();
  tokio::spawn(async move {
    loop {
      let mut listener =
        match NotifyListener::connect(&pool, &[HARNESS_CANCELLATION_NOTIFY_CHANNEL]).await {
          Ok(listener) => listener,
          Err(err) => {
            warn!(error = %err, "harness cancellation listener: connect failed, retrying in 5s");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
          }
        };
      cancel_marked_sessions(&repos, &registry, &registry.active_ids()).await;
      loop {
        let payload = match listener.recv().await {
          Ok(Notice::Message { payload, .. }) => payload,
          Ok(Notice::Gap) => {
            warn!("harness cancellation listener: connection dropped, reconnecting and resyncing");
            break;
          }
          Err(err) => {
            warn!(error = %err, "harness cancellation listener: recv failed, reconnecting");
            break;
          }
        };
        match Uuid::parse_str(&payload) {
          Ok(id) if registry.active_ids().contains(&id) => {
            cancel_marked_sessions(&repos, &registry, &[id]).await;
          }
          Ok(_) => {}
          Err(err) => {
            warn!(error = %err, payload = %payload, "harness cancellation listener: malformed payload");
          }
        }
      }
    }
  });
}

async fn cancel_marked_sessions(
  repos: &Repositories,
  registry: &HarnessCancellationRegistry,
  ids: &[Uuid],
) {
  match repos.harness_sessions.cancelled_among(ids).await {
    Ok(cancelled) => {
      for id in cancelled {
        info!(session_id = %id, "harness session cancelled through another replica");
        registry.cancel(id);
      }
    }
    Err(err) => warn!(error = %err, "harness cancellation listener: status check failed"),
  }
}

/// Drops the registry entry when the executor task ends. Generation-checked
/// so an older run finishing late can't evict a resume's newer token.
pub struct HarnessCancellationGuard {
  registry: HarnessCancellationRegistry,
  id: Uuid,
  generation: u64,
  token: AgentCancellationToken,
}

impl HarnessCancellationGuard {
  pub fn token(&self) -> AgentCancellationToken {
    self.token.clone()
  }
}

impl Drop for HarnessCancellationGuard {
  fn drop(&mut self) {
    let mut entries = self.registry.lock();
    if entries
      .get(&self.id)
      .is_some_and(|(generation, _)| *generation == self.generation)
    {
      entries.remove(&self.id);
    }
  }
}

/// Default executor used until the real LLM-backed runner lands.
///
/// Emits `session_started` + `stopped` events, marks the session as
/// `failed` with a clear error message, and finalises the broker. The
/// goal is end-to-end plumbing verification: routes accept submissions,
/// the row + events round-trip through Postgres, and SSE subscribers see
/// the lifecycle. Real execution wiring is tracked under the P-H.5
/// follow-up slice.
#[derive(Clone, Debug, Default)]
pub struct StubHarnessExecutor;

#[async_trait]
impl HarnessSessionExecutor for StubHarnessExecutor {
  async fn execute(&self, ctx: HarnessSessionContext) {
    if let Err(err) = stub_execute(&ctx).await {
      error!(session_id = %ctx.session_id, error = %err, "stub harness executor failed");
      // Best-effort: persist a terminal failure on the row even if the
      // event writes themselves failed.
      let _ = ctx
        .repos
        .harness_sessions
        .update_status(
          ctx.session_id,
          HarnessSessionStatus::Failed,
          None,
          Some(&err.to_string()),
        )
        .await;
      ctx
        .broker
        .finalise_with_grace(ctx.session_id, broker_finalize_grace());
    }
  }
}

async fn stub_execute(ctx: &HarnessSessionContext) -> Result<(), DbError> {
  let started_seq = ctx.initial_seq as i64;
  let stopped_seq = started_seq + 1;
  publish_through(
    &ctx.repos,
    &ctx.broker,
    NewHarnessSessionEvent {
      session_id: ctx.session_id,
      seq: started_seq,
      kind: "session_started".into(),
      payload: serde_json::json!({
        "executor": "stub",
        "profile": ctx.profile,
        "runtime_kind": ctx.runtime_kind,
        "model": ctx.model,
        "skill_name": ctx.skill_name,
        "workspace_root": ctx.workspace_root,
      }),
    },
  )
  .await?;

  // Brief delay so SSE subscribers have time to attach before the
  // session completes. Same trick as `StubExecutor::stub_execute` in
  // `yanshi-server::runs`.
  tokio::time::sleep(Duration::from_millis(50)).await;

  let stop_reason = "executor_not_yet_wired";
  publish_through(
    &ctx.repos,
    &ctx.broker,
    NewHarnessSessionEvent {
      session_id: ctx.session_id,
      seq: stopped_seq,
      kind: "stopped".into(),
      payload: serde_json::json!({
        "executor": "stub",
        "reason": stop_reason,
        "message": "LLM-backed harness executor not yet wired into the server; \
                    follow-up P-H.5 slice will replace this stub.",
      }),
    },
  )
  .await?;

  ctx
    .repos
    .harness_sessions
    .update_status(
      ctx.session_id,
      HarnessSessionStatus::Failed,
      None,
      Some(stop_reason),
    )
    .await?;

  ctx
    .broker
    .finalise_with_grace(ctx.session_id, broker_finalize_grace());
  info!(session_id = %ctx.session_id, "stub harness executor finished");
  Ok(())
}

/// Default executor used by [`AppState::new`]. Exposed so callers can wrap
/// or replace it (tests use this to verify the route layer in isolation
/// from the real LLM-backed runtime).
pub fn default_harness_executor() -> Arc<dyn HarnessSessionExecutor> {
  Arc::new(StubHarnessExecutor)
}

/// `POST /v1/harness/sessions` — accept a session submission, persist a
/// `running` session row, dispatch the executor in the background, return
/// the new id immediately.
///
/// Q1.4.3: tenant comes from the auth-bound `Extension<TenantId>`; if
/// the body still carries a `tenant_id` it must match, otherwise the
/// request is rejected with `tenant_mismatch` (HTTP 403).
pub async fn submit_harness_session(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  JsonReq(req): JsonReq<CreateHarnessSessionRequest>,
) -> Result<Json<CreateHarnessSessionResponse>, ApiError> {
  let user_input = req.user_input.trim();
  if user_input.is_empty() {
    return Err(ApiError::BadRequest(
      "`user_input` must be a non-empty prompt".into(),
    ));
  }
  let workspace_root = req.workspace_root.trim();
  if workspace_root.is_empty() {
    return Err(ApiError::BadRequest(
      "`workspace_root` must be a non-empty path".into(),
    ));
  }
  // Eval 2026-10-07 S2: the session's file tool is scoped to this root,
  // so `/` would expose the whole host (incl. /proc/self/environ).
  state
    .gateway_policy
    .check_path("workspace_root", workspace_root)
    .map_err(ApiError::BadRequest)?;

  let session_id = Uuid::new_v4();
  let tenant_id = tenant.as_str().to_string();
  if let Some(body_tenant) = &req.tenant_id
    && body_tenant != &tenant_id
  {
    return Err(ApiError::TenantMismatch(format!(
      "request body tenant_id '{body_tenant}' does not match authenticated tenant '{tenant_id}'"
    )));
  }
  let profile = req.profile.unwrap_or_else(|| DEFAULT_PROFILE.into());
  let runtime_kind = req
    .runtime_kind
    .unwrap_or_else(|| DEFAULT_RUNTIME_KIND.into());
  // Eval 2026-10-07 A4: an unknown kind used to run ReAct under its label.
  if !matches!(runtime_kind.as_str(), "react" | "plan_execute") {
    return Err(ApiError::BadRequest(format!(
      "runtime_kind '{runtime_kind}' is not supported (expected 'react' or 'plan_execute')"
    )));
  }
  if runtime_kind == "plan_execute" && req.skill_name.is_some() {
    return Err(ApiError::BadRequest(
      "skill-backed sessions run ReAct; 'plan_execute' cannot be combined with skill_name".into(),
    ));
  }
  let model = req.model.unwrap_or_else(|| DEFAULT_MODEL.into());

  let session = state
    .repos
    .harness_sessions
    .create(NewHarnessSession {
      id: session_id,
      tenant_id,
      user_input: user_input.to_string(),
      workspace_root: workspace_root.to_string(),
      profile: profile.clone(),
      runtime_kind: runtime_kind.clone(),
      model: model.clone(),
      skill_name: req.skill_name.clone(),
    })
    .await?;

  // Dispatch in the background so the HTTP request returns immediately.
  // The executor owns the entire session lifecycle from this point.
  let executor = state.harness_executor.clone();
  let repos = state.repos.clone();
  let broker = state.harness_broker.clone();
  let workspace_root_owned = workspace_root.to_string();
  let user_input_owned = user_input.to_string();
  // W4.1c: resolve the skill directory once here, alongside the name, so
  // the executor can build a skill-backed tool registry instead of the
  // hardcoded default.
  let skill_dir = req
    .skill_name
    .as_deref()
    .and_then(|name| state.skills.resolve(name))
    .map(|entry| entry.path);
  let cancel_guard = state.harness_cancellations.register(session_id);
  tokio::spawn(async move {
    let cancellation_token = cancel_guard.token();
    executor
      .execute(HarnessSessionContext {
        session_id,
        user_input: user_input_owned,
        workspace_root: workspace_root_owned,
        profile,
        runtime_kind,
        model,
        skill_name: req.skill_name,
        skill_dir,
        cost_limit_usd: req.cost_limit_usd,
        max_steps: req.max_steps,
        repos,
        broker,
        initial_seq: 0,
        cancellation_token,
      })
      .await;
  });

  Ok(Json(CreateHarnessSessionResponse {
    session_id: session.id,
    status: "running",
  }))
}

/// `GET /v1/harness/sessions` — list recent sessions for a tenant, newest
/// first. Q1.4.2: the tenant comes exclusively from the auth-middleware
/// `X-Yanshi-Tenant` extension. The previous code ignored that
/// extension entirely and exposed every tenant's sessions to any
/// authenticated client through the `?tenant_id=` query parameter.
pub async fn list_harness_sessions(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Query(params): Query<ListHarnessSessionsQuery>,
) -> Result<Json<ListHarnessSessionsResponse>, ApiError> {
  let tenant_id = tenant.as_str();
  let limit = params.limit.unwrap_or(25).clamp(1, 100);
  let sessions = state.repos.harness_sessions.list(tenant_id, limit).await?;
  Ok(Json(ListHarnessSessionsResponse { sessions }))
}

/// `GET /v1/harness/sessions/{id}` — return the current session state.
pub async fn get_harness_session(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(id): Path<Uuid>,
) -> Result<Json<HarnessSessionResponse>, ApiError> {
  let session = state
    .repos
    .harness_sessions
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", id)))?;
  // P2.6 tenant boundary: 404 (not 403) cross-tenant access so the
  // caller can't infer existence by status code.
  if session.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!(
      "harness session {} not found",
      id
    )));
  }
  Ok(Json(HarnessSessionResponse { session }))
}

/// Dispatcher for the two POST actions on `/v1/harness/sessions/{id}`.
///
/// Axum can't bind two POST handlers to the same path pattern, so the
/// router routes both `:cancel` and `:resume` here and the handler
/// dispatches on the suffix. The raw `:id` capture includes the
/// suffix verbatim (e.g. `<uuid>:cancel`); the matching handler strips
/// it before parsing the UUID.
pub async fn post_harness_session_action(
  state: State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(id_action): Path<String>,
  body: Option<Json<ResumeHarnessSessionRequest>>,
) -> Result<axum::response::Response, ApiError> {
  use axum::response::IntoResponse;
  if id_action.ends_with(":cancel") {
    return cancel_harness_session(state, tenant, Path(id_action))
      .await
      .map(IntoResponse::into_response);
  }
  if id_action.ends_with(":resume") {
    let body = body.map(|Json(value)| value).unwrap_or_default();
    return resume_harness_session(state, tenant, Path(id_action), Json(body))
      .await
      .map(IntoResponse::into_response);
  }
  Err(ApiError::BadRequest(
    "harness session action route must end with :cancel or :resume".into(),
  ))
}

/// `POST /v1/harness/sessions/{id}:cancel` — idempotently cancel a running
/// session. Terminal sessions return the current row with
/// `cancelled: false`.
pub async fn cancel_harness_session(
  State(state): State<AppState>,
  tenant: TenantId,
  Path(id_cancel): Path<String>,
) -> Result<Json<CancelHarnessSessionResponse>, ApiError> {
  let id_raw = id_cancel.strip_suffix(":cancel").ok_or_else(|| {
    ApiError::BadRequest("harness session cancel route must end with :cancel".into())
  })?;
  let id = Uuid::parse_str(id_raw)
    .map_err(|_| ApiError::BadRequest(format!("invalid harness session id '{}'", id_raw)))?;

  let session = state
    .repos
    .harness_sessions
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", id)))?;
  // P2.6 tenant boundary: 404 cross-tenant cancel so a probe can't
  // even toggle another tenant's session state.
  if session.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!(
      "harness session {} not found",
      id
    )));
  }

  if HarnessSessionStatus::parse(&session.status)
    .map(HarnessSessionStatus::is_terminal)
    .unwrap_or(false)
  {
    return Ok(Json(CancelHarnessSessionResponse {
      session,
      cancelled: false,
    }));
  }

  // Signal the agent loop if it runs on this replica, then mark the row.
  // The row write wins over the executor's own late terminal write
  // because `update_status` never leaves a terminal state. A session on
  // another replica is stopped by its replica's
  // `spawn_harness_cancellation_listener`, woken by the NOTIFY below.
  let signalled = state.harness_cancellations.cancel(id);
  info!(session_id = %id, signalled, "harness session cancel requested");
  state
    .repos
    .harness_sessions
    .update_status(
      id,
      HarnessSessionStatus::Cancelled,
      None,
      Some("cancel requested"),
    )
    .await?;
  if !signalled
    && let Err(err) = yanshi_db::notify(
      &state.repos.harness_sessions.pool,
      HARNESS_CANCELLATION_NOTIFY_CHANNEL,
      &id.to_string(),
    )
    .await
  {
    warn!(session_id = %id, error = %err, "harness cancel: cross-replica NOTIFY failed");
  }
  publish_cancel_event(&state.repos, &state.harness_broker, tenant.as_str(), id).await?;
  state
    .harness_broker
    .finalise_with_grace(id, broker_finalize_grace());

  let session = state
    .repos
    .harness_sessions
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", id)))?;
  Ok(Json(CancelHarnessSessionResponse {
    session,
    cancelled: true,
  }))
}

/// Resume flavour selected by the request body.
///
/// `Rerun` (default) clears the prior event log and restarts the seq
/// series at `0` — the original v1 semantic. `Append` preserves the
/// prior events and continues the seq series at `MAX(seq) + 1` so SSE
/// consumers see the new run as an extension of the old one rather
/// than a fresh shell. The two modes are mutually exclusive on a
/// single resume call: pick the rerun flavour for "retry from
/// scratch" debugging, the append flavour for "extend the
/// conversation" workflows.
#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeMode {
  #[default]
  Rerun,
  Append,
}

/// Body for `POST /v1/harness/sessions/{id}:resume`.
///
/// `user_input` is optional: omitting it replays the original prompt.
/// Pass a new prompt to extend the session with a follow-up
/// instruction. Other lifecycle fields (workspace, profile, runtime,
/// model, skill) are taken from the existing row — operators rerun
/// against the same shape, not a new shape.
///
/// `mode` defaults to [`ResumeMode::Rerun`] for full backwards
/// compatibility with pre-append callers.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ResumeHarnessSessionRequest {
  #[serde(default)]
  pub user_input: Option<String>,
  #[serde(default)]
  pub mode: ResumeMode,
}

#[derive(Debug, Serialize)]
pub struct ResumeHarnessSessionResponse {
  #[serde(flatten)]
  pub session: HarnessSession,
  pub resumed: bool,
  /// Echoes the resume flavour the server actually applied, so a
  /// caller that omits the field can confirm the default.
  pub mode: ResumeMode,
}

/// `POST /v1/harness/sessions/{id}:resume` — restart or extend a
/// terminated session.
///
/// **Rerun semantic (default, `mode = "rerun"`):** clear prior
/// persisted events, flip the row back to `running`, spawn a fresh
/// `HarnessSessionExecutor` run with the same `session_id`. The
/// original `user_input` is reused unless the request body overrides
/// it. Useful for retry-with-tweak debugging or replaying after a
/// transient LLM failure.
///
/// **Append semantic (`mode = "append"`):** keep the prior event log
/// intact, flip the row back to `running`, spawn a fresh executor run
/// that emits new events at `MAX(existing seq) + 1`. SSE consumers see
/// one continuous timeline. This is the natural shape for follow-up
/// instructions ("after that finished, also do X") and for resuming
/// from a forced cancel without losing the trace.
///
/// Returns 409 Conflict when the session is still running so two
/// reruns can't race the same row. Cancel first if needed.
pub async fn resume_harness_session(
  State(state): State<AppState>,
  tenant: TenantId,
  Path(id_resume): Path<String>,
  Json(body): Json<ResumeHarnessSessionRequest>,
) -> Result<Json<ResumeHarnessSessionResponse>, ApiError> {
  let id_raw = id_resume.strip_suffix(":resume").ok_or_else(|| {
    ApiError::BadRequest("harness session resume route must end with :resume".into())
  })?;
  let id = Uuid::parse_str(id_raw)
    .map_err(|_| ApiError::BadRequest(format!("invalid harness session id '{}'", id_raw)))?;

  let session = state
    .repos
    .harness_sessions
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", id)))?;
  // P2.6 tenant boundary: 404 cross-tenant resume so an attacker can't
  // restart another tenant's session.
  if session.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!(
      "harness session {} not found",
      id
    )));
  }

  if !HarnessSessionStatus::parse(&session.status)
    .map(HarnessSessionStatus::is_terminal)
    .unwrap_or(false)
  {
    return Err(ApiError::BadRequest(format!(
      "harness session {} is still {}; cancel before resuming",
      id, session.status
    )));
  }

  // Pick the next prompt: explicit override, else the original.
  let user_input = body
    .user_input
    .as_deref()
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(str::to_string)
    .unwrap_or_else(|| session.user_input.clone());

  // Resolve the seq offset and reset the row in one place per mode so
  // the spawned executor only sees a fully-prepared session.
  let initial_seq: u64 = match body.mode {
    ResumeMode::Rerun => {
      // Clear the prior event log — destructive by design, see the
      // function's docstring. The inner runtime's seq counter then
      // restarts cleanly at 0.
      state
        .repos
        .harness_sessions
        .reset_for_resume(id, &user_input)
        .await?;
      0
    }
    ResumeMode::Append => {
      // Keep prior events; pick `MAX(seq) + 1` as the offset for the
      // next run so seq numbers remain monotonic across the resumes.
      let max_seq = state.repos.harness_events.max_seq(id).await?;
      state
        .repos
        .harness_sessions
        .reset_for_append_resume(id, &user_input)
        .await?;
      // Map None (no prior events) → 0 so the append flavour degrades
      // gracefully into a clean first run if the operator is calling
      // it on a session that somehow has zero rows. Otherwise the
      // u64 cast is safe because seqs are stored as non-negative
      // bigints by the append path.
      max_seq.map(|m| (m as u64) + 1).unwrap_or(0)
    }
  };

  // Spawn the executor in the background. The HTTP request returns
  // immediately with the refreshed row.
  let executor = state.harness_executor.clone();
  let repos = state.repos.clone();
  let broker = state.harness_broker.clone();
  let workspace_root = session.workspace_root.clone();
  let profile = session.profile.clone();
  let runtime_kind = session.runtime_kind.clone();
  let model = session.model.clone();
  let skill_name = session.skill_name.clone();
  // W4.1c: re-resolve, same as the create path — the session row only
  // persists the name.
  let skill_dir = skill_name
    .as_deref()
    .and_then(|name| state.skills.resolve(name))
    .map(|entry| entry.path);
  let user_input_owned = user_input.clone();
  let cancel_guard = state.harness_cancellations.register(id);
  tokio::spawn(async move {
    let cancellation_token = cancel_guard.token();
    executor
      .execute(HarnessSessionContext {
        session_id: id,
        user_input: user_input_owned,
        workspace_root,
        profile,
        runtime_kind,
        model,
        skill_name,
        skill_dir,
        // U1.3: cost_limit_usd isn't a persisted `harness_sessions`
        // column (unlike profile/runtime_kind/model/skill_name), so a
        // resume can't recover the original session's limit — it
        // restarts with no cost-based stop condition. Only the create
        // path (`CreateHarnessSessionRequest::cost_limit_usd`) can set
        // it today; persisting + re-applying it across resume is a
        // follow-up if operators need it.
        cost_limit_usd: None,
        // W0.1: same "not a persisted column" caveat as cost_limit_usd
        // above — a resume falls back to the server default step cap.
        max_steps: None,
        repos,
        broker,
        initial_seq,
        cancellation_token,
      })
      .await;
  });

  // Refetch so the response reflects the freshly-reset row (status,
  // cleared finished_at / final_answer / error, optional new prompt).
  let session = state
    .repos
    .harness_sessions
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", id)))?;
  Ok(Json(ResumeHarnessSessionResponse {
    session,
    resumed: true,
    mode: body.mode,
  }))
}

/// A pending `ask_user` question, as surfaced by
/// `GET /v1/harness/sessions/{id}/interrupt`.
#[derive(Debug, Serialize)]
pub struct PendingInterrupt {
  pub question: String,
  pub step_index: i64,
}

#[derive(Debug, Serialize)]
pub struct PendingInterruptResponse {
  /// `None` when the session has no question pending — at most one can
  /// ever be pending per session (the whole loop is stopped, not one
  /// gated call mid-batch, unlike tool-call approvals), so this is a
  /// nullable object rather than a list.
  pub pending: Option<PendingInterrupt>,
}

/// `GET /v1/harness/sessions/{id}/interrupt` — read the question the
/// session is currently paused on, if any. Reads straight off the
/// `pending_question*` columns on the session row rather than loading
/// and deserializing the full loop checkpoint.
pub async fn get_pending_interrupt(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(session_id): Path<Uuid>,
) -> Result<Json<PendingInterruptResponse>, ApiError> {
  let session = state
    .repos
    .harness_sessions
    .get(session_id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", session_id)))?;
  // P2.6 tenant boundary: 404 cross-tenant access so the pending
  // question doesn't leak to another tenant.
  if session.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!(
      "harness session {} not found",
      session_id
    )));
  }

  let pending = session.pending_question.map(|question| PendingInterrupt {
    question,
    step_index: session.pending_question_step_index.unwrap_or(0),
  });
  Ok(Json(PendingInterruptResponse { pending }))
}

/// Body for `POST /v1/harness/sessions/{id}/interrupt/answer`.
#[derive(Debug, Deserialize)]
pub struct AnswerInterruptRequest {
  pub answer: String,
}

#[derive(Debug, Serialize)]
pub struct AnswerInterruptResponse {
  #[serde(flatten)]
  pub session: HarnessSession,
  pub resumed: bool,
}

/// `POST /v1/harness/sessions/{id}/interrupt/answer` — answer a pending
/// question and resume the paused loop.
///
/// 400s when the session isn't currently `awaiting_input` (mirrors
/// `:resume`'s `is_terminal()` guard, just for the opposite state).
/// Otherwise flips the row back to `running`, clears the pending-question
/// columns, and spawns the resume through
/// [`HarnessSessionExecutor::resume_interrupt`] in the background —
/// fire-and-forget, matching `:resume`'s async-dispatch pattern. The seq
/// series continues at `MAX(existing seq) + 1` (this is a continuation of
/// the same session's event log, not a fresh run, so it always behaves
/// like the `:resume` route's append flavour).
pub async fn answer_interrupt(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(session_id): Path<Uuid>,
  JsonReq(body): JsonReq<AnswerInterruptRequest>,
) -> Result<Json<AnswerInterruptResponse>, ApiError> {
  let session = state
    .repos
    .harness_sessions
    .get(session_id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", session_id)))?;
  if session.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!(
      "harness session {} not found",
      session_id
    )));
  }

  if HarnessSessionStatus::parse(&session.status) != Some(HarnessSessionStatus::AwaitingInput) {
    return Err(ApiError::BadRequest(format!(
      "harness session {} is {}; nothing pending to answer",
      session_id, session.status
    )));
  }

  let answer = body.answer.trim();
  if answer.is_empty() {
    return Err(ApiError::BadRequest("answer must not be empty".into()));
  }

  state
    .repos
    .harness_sessions
    .clear_pending_question_for_resume(session_id)
    .await?;

  let max_seq = state.repos.harness_events.max_seq(session_id).await?;
  let initial_seq = max_seq.map(|m| (m as u64) + 1).unwrap_or(0);

  let executor = state.harness_executor.clone();
  let repos = state.repos.clone();
  let broker = state.harness_broker.clone();
  // W4.1c: re-resolve, same as the create/resume paths.
  let skill_dir = session
    .skill_name
    .as_deref()
    .and_then(|name| state.skills.resolve(name))
    .map(|entry| entry.path);
  let cancel_guard = state.harness_cancellations.register(session_id);
  let ctx = HarnessSessionContext {
    session_id,
    user_input: session.user_input.clone(),
    workspace_root: session.workspace_root.clone(),
    profile: session.profile.clone(),
    runtime_kind: session.runtime_kind.clone(),
    model: session.model.clone(),
    skill_name: session.skill_name.clone(),
    skill_dir,
    cost_limit_usd: None,
    // W0.1: not a persisted column — see the `reset_for_resume` site above.
    max_steps: None,
    repos,
    broker,
    initial_seq,
    cancellation_token: cancel_guard.token(),
  };
  let answer_owned = answer.to_string();
  tokio::spawn(async move {
    let _cancel_guard = cancel_guard;
    executor.resume_interrupt(ctx, answer_owned).await;
  });

  // Refetch so the response reflects the freshly-cleared row (status
  // `running`, `pending_question*` cleared).
  let session = state
    .repos
    .harness_sessions
    .get(session_id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", session_id)))?;
  Ok(Json(AnswerInterruptResponse {
    session,
    resumed: true,
  }))
}

async fn publish_cancel_event(
  repos: &Repositories,
  broker: &HarnessEventBroker,
  tenant_id: &str,
  session_id: Uuid,
) -> Result<(), ApiError> {
  let seq = next_event_seq(repos, tenant_id, session_id).await?;
  publish_through(
    repos,
    broker,
    NewHarnessSessionEvent {
      session_id,
      seq,
      kind: "stopped".to_string(),
      payload: serde_json::json!({
        "reason": "cancelled",
        "source": "operator",
      }),
    },
  )
  .await?;
  Ok(())
}

async fn next_event_seq(
  repos: &Repositories,
  _tenant_id: &str,
  session_id: Uuid,
) -> Result<i64, ApiError> {
  // Q3.11.1: O(1) MAX(seq) aggregate instead of paging
  // `list_after(..., 10_000)`. A session whose event count crossed
  // the cap would have silently rolled the seq counter back into a
  // colliding range, breaking the
  // `harness_session_events.(session_id, seq)` primary key on the
  // next `append`. `harness_events.max_seq` itself enforces the
  // tenant boundary via FK join to `harness_sessions`, so the
  // tenant_id parameter stays on the surface for symmetry with
  // [`runs.rs::next_event_seq`] but is not forwarded.
  let max = repos.harness_events.max_seq(session_id).await?;
  Ok(max.map(|seq| seq + 1).unwrap_or(0))
}

/// `GET /v1/harness/sessions/{id}/events` — SSE stream.
///
/// 1. Verifies the session exists; 404s if not.
/// 2. Subscribes to the broker first so events emitted while we're still
///    setting up don't fall on the floor.
/// 3. Replays any events with `seq > after_seq` (default `-1`) from the DB.
/// 4. Forwards live broker events for as long as the channel stays open.
pub async fn stream_harness_events(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(session_id): Path<Uuid>,
  Query(params): Query<HarnessEventsQuery>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
  let session = state
    .repos
    .harness_sessions
    .get(session_id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", session_id)))?;
  // P2.6 tenant boundary: 404 cross-tenant SSE subscription so a
  // caller can't tail another tenant's session.
  if session.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!(
      "harness session {} not found",
      session_id
    )));
  }

  let mut after_seq = params.after_seq.unwrap_or(-1);
  let receiver = state.harness_broker.subscribe(session_id);

  let mut backfill: Vec<StreamedHarnessEvent> = Vec::new();
  loop {
    let page = state
      .repos
      .harness_events
      .list_after(tenant.as_str(), session_id, after_seq, 200)
      .await?;
    if page.is_empty() {
      break;
    }
    after_seq = page.last().map(|e| e.seq).unwrap_or(after_seq);
    backfill.extend(page.into_iter().map(StreamedHarnessEvent::from));
    if backfill.len() >= 1_000 {
      break;
    }
  }

  let backfill_stream = futures::stream::iter(backfill).map(HarnessBrokerItem::Event);
  let live_stream = BroadcastStream::new(receiver).map(|res| match res {
    Ok(event) => HarnessBrokerItem::Event(event),
    Err(BroadcastStreamRecvError::Lagged(_)) => HarnessBrokerItem::Lagged,
  });
  let stream = backfill_stream
    .chain(live_stream)
    .map(serialise_item)
    .map(Ok::<_, Infallible>);

  Ok(
    Sse::new(stream).keep_alive(
      KeepAlive::new()
        .interval(Duration::from_secs(15))
        .text("keep-alive"),
    ),
  )
}

/// `GET /v1/harness/sessions/{id}/events/history` — JSON list of events,
/// optionally filtered by `?after_seq=`. Same contract as the workflow
/// `list_events` route.
pub async fn list_harness_events(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(session_id): Path<Uuid>,
  Query(params): Query<HarnessEventsQuery>,
) -> Result<Json<Vec<StreamedHarnessEvent>>, ApiError> {
  let session = state
    .repos
    .harness_sessions
    .get(session_id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("harness session {} not found", session_id)))?;
  // P2.6 tenant boundary: 404 cross-tenant history reads so the
  // endpoint doesn't leak another tenant's harness event log.
  if session.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!(
      "harness session {} not found",
      session_id
    )));
  }

  let after_seq = params.after_seq.unwrap_or(-1);
  let events = state
    .repos
    .harness_events
    .list_after(tenant.as_str(), session_id, after_seq, 1_000)
    .await?
    .into_iter()
    .map(StreamedHarnessEvent::from)
    .collect();
  Ok(Json(events))
}

enum HarnessBrokerItem {
  Event(StreamedHarnessEvent),
  Lagged,
}

fn serialise_item(item: HarnessBrokerItem) -> Event {
  match item {
    HarnessBrokerItem::Event(event) => serialise_event(&event),
    HarnessBrokerItem::Lagged => {
      Event::default().comment("lagged: reconnect with ?after_seq=<last_seen>")
    }
  }
}

fn serialise_event(event: &StreamedHarnessEvent) -> Event {
  let json = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
  Event::default()
    .id(event.seq.to_string())
    .event(event.kind.clone())
    .data(json)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cancellation_registry_signals_and_cleans_up() {
    let registry = HarnessCancellationRegistry::default();
    let id = Uuid::new_v4();
    assert!(!registry.cancel(id), "nothing registered yet");

    let guard = registry.register(id);
    let token = guard.token();
    assert!(registry.cancel(id));
    assert!(token.is_cancelled());

    drop(guard);
    assert!(
      !registry.cancel(id),
      "entry removed when the executor task ends"
    );
  }

  #[test]
  fn stale_guard_does_not_evict_a_newer_registration() {
    let registry = HarnessCancellationRegistry::default();
    let id = Uuid::new_v4();
    let old_run = registry.register(id);
    let resumed = registry.register(id);
    drop(old_run);
    assert!(
      registry.cancel(id),
      "the resume's token must still be reachable"
    );
    assert!(resumed.token().is_cancelled());
  }
  use chrono::Utc;

  fn sample_event(session_id: Uuid, seq: i64) -> StreamedHarnessEvent {
    StreamedHarnessEvent {
      session_id,
      seq,
      kind: "test".into(),
      payload: serde_json::json!({"seq": seq}),
      ts: Utc::now(),
    }
  }

  #[tokio::test]
  async fn broker_subscribe_then_publish_delivers_event() {
    let broker = HarnessEventBroker::new();
    let session_id = Uuid::new_v4();
    let mut rx = broker.subscribe(session_id);
    broker.publish(sample_event(session_id, 0));
    let received = rx.recv().await.expect("event delivered");
    assert_eq!(received.seq, 0);
  }

  #[tokio::test]
  async fn broker_isolates_events_per_session_id() {
    let broker = HarnessEventBroker::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut rx_a = broker.subscribe(a);
    let _rx_b = broker.subscribe(b);
    broker.publish(sample_event(b, 0));
    assert!(rx_a.try_recv().is_err());
  }

  #[tokio::test]
  async fn broker_finalise_closes_subscribers() {
    let broker = HarnessEventBroker::new();
    let session_id = Uuid::new_v4();
    let mut rx = broker.subscribe(session_id);
    broker.finalise(session_id);
    let result = rx.recv().await;
    assert!(matches!(result, Err(broadcast::error::RecvError::Closed)));
  }

  #[tokio::test]
  async fn broker_finalise_with_grace_preserves_terminal_event() {
    let broker = HarnessEventBroker::new();
    let session_id = Uuid::new_v4();
    let mut rx = broker.subscribe(session_id);
    broker.publish(sample_event(session_id, 0));
    broker.finalise_with_grace(session_id, Duration::from_millis(50));

    let received = tokio::time::timeout(Duration::from_millis(200), rx.recv())
      .await
      .expect("recv did not time out")
      .expect("terminal event delivered before channel teardown");
    assert_eq!(received.seq, 0);

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(broker.active_sessions(), 0);
  }

  #[tokio::test]
  async fn broker_receiver_count_tracks_subscriber_drops() {
    let broker = HarnessEventBroker::new();
    let session_id = Uuid::new_v4();
    let rx_one = broker.subscribe(session_id);
    let rx_two = broker.subscribe(session_id);
    assert_eq!(broker.receiver_count(session_id), 2);
    drop(rx_one);
    broker.publish(sample_event(session_id, 0));
    assert_eq!(broker.receiver_count(session_id), 1);
    drop(rx_two);
    broker.publish(sample_event(session_id, 1));
    assert_eq!(broker.receiver_count(session_id), 0);
  }

  /// W4.2c: `catchup_baseline` is the gate `spawn_harness_events_listener`
  /// uses to decide whether a NOTIFY is worth a DB round-trip. No local
  /// subscriber (channel never created) → `None`. A local publish must
  /// advance the baseline so a same-origin NOTIFY resolves to a no-op
  /// catch-up (an empty `list_after` page) rather than redundant work.
  #[tokio::test]
  async fn catchup_baseline_reflects_local_subscriber_and_publish_state() {
    let broker = HarnessEventBroker::new();
    let session_id = Uuid::new_v4();
    assert_eq!(
      broker.catchup_baseline(session_id),
      None,
      "no channel at all yet — no local subscriber to catch up"
    );

    let _rx = broker.subscribe(session_id);
    assert_eq!(
      broker.catchup_baseline(session_id),
      Some(-1),
      "fresh channel with a subscriber starts at -1, matching after_seq's own default"
    );

    broker.publish(sample_event(session_id, 5));
    assert_eq!(
      broker.catchup_baseline(session_id),
      Some(5),
      "local publish must advance the baseline"
    );
  }

  /// A channel that exists (e.g. the run hasn't finished yet) but has no
  /// *active* subscriber right now must still report `None` — the
  /// listener should skip the DB round-trip even though the map entry
  /// is present, matching `EventBroker::catchup_baseline`'s contract.
  #[tokio::test]
  async fn catchup_baseline_is_none_after_last_subscriber_disconnects() {
    let broker = HarnessEventBroker::new();
    let session_id = Uuid::new_v4();
    let rx = broker.subscribe(session_id);
    assert!(broker.catchup_baseline(session_id).is_some());
    drop(rx);
    // `tokio::broadcast` updates receiver_count lazily on send/recv;
    // force an interaction, mirroring `broker_receiver_count_tracks_
    // subscriber_drops` above.
    broker.publish(sample_event(session_id, 0));
    assert_eq!(
      broker.catchup_baseline(session_id),
      None,
      "channel entry survives (run not finalised) but has zero active receivers"
    );
  }
}
