//! Broker that forwards persisted run events to SSE subscribers.
//!
//! `GET /v1/runs/{id}/events` first replays everything already in the
//! `events` table for that run, then keeps the connection open and pushes
//! anything the broker publishes for that `run_id`. New events come from
//! the executor: it writes through the broker, which both persists to the
//! DB and forwards to live subscribers.
//!
//! W4.2c: the broker's live fan-out is still process-local (a
//! `tokio::sync::broadcast` channel per run_id), but writes now also fire
//! a Postgres `NOTIFY` (`yanshi_db::notify`) so every other gateway
//! replica's [`spawn_run_events_listener`] task can catch its own local
//! subscribers up — see that function's doc comment for the mechanism.
//! DB is still the sole source of truth; both the local broadcast and the
//! cross-replica NOTIFY are best-effort delivery paths on top of it, and
//! `?after_seq=` reconnect (already the client-facing contract) is what
//! makes that safe.

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
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::warn;
use uuid::Uuid;

use yanshi_db::{DbError, EventRepo, NewEvent, Notice, NotifyListener, Repositories, RunRepo};

use crate::AppState;
use crate::error::ApiError;
use crate::tenant::TenantId;

/// Postgres NOTIFY channel carrying cross-replica run-event wake-ups.
pub const RUN_EVENTS_NOTIFY_CHANNEL: &str = "yanshi_run_events";

/// Channel capacity per run. Slow subscribers drop oldest events when
/// they fall this far behind; the SSE handler logs a warning and lets the
/// client reconnect with `?after_seq=` to fill the gap from the DB.
const RUN_CHANNEL_CAPACITY: usize = 256;

/// Wire shape published over SSE. Mirrors `yanshi_db::Event` but stays
/// minimal so we don't tie SSE consumers to internal DB columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamedEvent {
  pub run_id: Uuid,
  pub seq: i64,
  pub kind: String,
  pub payload: serde_json::Value,
  pub ts: chrono::DateTime<chrono::Utc>,
}

impl From<yanshi_db::Event> for StreamedEvent {
  fn from(e: yanshi_db::Event) -> Self {
    Self {
      run_id: e.run_id,
      seq: e.seq,
      kind: e.kind,
      payload: e.payload,
      ts: e.ts,
    }
  }
}

/// One run's local broadcast channel plus the highest `seq` this
/// replica has already forwarded into it — from either a direct local
/// `publish()` or a cross-replica NOTIFY catch-up fetch (`W4.2c`). Lets
/// [`EventBroker::catchup_baseline`] tell a NOTIFY listener exactly
/// where to resume from, and lets it skip the DB round-trip entirely
/// when the notified seq is already covered.
struct RunChannel {
  sender: broadcast::Sender<StreamedEvent>,
  last_forwarded_seq: i64,
}

impl RunChannel {
  fn new() -> Self {
    Self {
      sender: broadcast::channel(RUN_CHANNEL_CAPACITY).0,
      // Mirrors `EventsQuery::after_seq`'s own default — "nothing seen
      // yet" for a brand-new channel.
      last_forwarded_seq: -1,
    }
  }
}

/// Broker over a sharded broadcast channel keyed by `run_id`. The live
/// fan-out itself is process-local; W4.2c's NOTIFY-driven catch-up (see
/// the module doc comment) is what makes a subscriber on one replica
/// see events published on another.
///
/// Cloning is cheap — `Arc<Mutex<...>>` inside.
#[derive(Clone, Default)]
pub struct EventBroker {
  inner: Arc<Mutex<HashMap<Uuid, RunChannel>>>,
}

impl std::fmt::Debug for EventBroker {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let len = self.inner.lock().map(|g| g.len()).unwrap_or(0);
    f.debug_struct("EventBroker")
      .field("active_runs", &len)
      .finish()
  }
}

impl EventBroker {
  pub fn new() -> Self {
    Self::default()
  }

  /// Lock the per-run channel map, recovering the inner map on poison so
  /// a panicked publisher can't deadlock every subsequent subscriber. The
  /// map mutates one channel-entry at a time, so the inner state is
  /// always sound to keep using after a poisoning panic. (Q5.1)
  fn lock_inner(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, RunChannel>> {
    match self.inner.lock() {
      Ok(g) => g,
      Err(poisoned) => poisoned.into_inner(),
    }
  }

  /// Subscribe to live events for `run_id`. Creates the channel if no
  /// subscriber has registered for this run yet — that's deliberate so
  /// publishers don't need to coordinate with subscribers.
  pub fn subscribe(&self, run_id: Uuid) -> broadcast::Receiver<StreamedEvent> {
    let mut map = self.lock_inner();
    map
      .entry(run_id)
      .or_insert_with(RunChannel::new)
      .sender
      .subscribe()
  }

  /// Publish without persisting. Use [`publish_through`] instead when
  /// you also want a DB row — keeps the persisted log and live stream in
  /// sync. Advances the channel's `last_forwarded_seq` regardless of
  /// whether this publish originated locally or came from a NOTIFY
  /// catch-up fetch — both paths funnel through here.
  pub fn publish(&self, event: StreamedEvent) {
    let mut map = self.lock_inner();
    let channel = map.entry(event.run_id).or_insert_with(RunChannel::new);
    channel.last_forwarded_seq = channel.last_forwarded_seq.max(event.seq);
    let _ = channel.sender.send(event);
  }

  /// Drop the channel for a finished run so it doesn't leak.
  ///
  /// Safe to call multiple times. If a subscriber is mid-flight when this
  /// is called, their `recv()` will return `Closed` after the existing
  /// queue drains — the SSE handler treats that as end-of-stream.
  pub fn finalise(&self, run_id: Uuid) {
    let mut map = self.lock_inner();
    map.remove(&run_id);
  }

  /// Like [`Self::finalise`] but defers the actual channel removal by
  /// `grace`. Lets in-flight SSE subscribers drain the terminal event
  /// from the broadcast buffer before the sender is dropped. The
  /// caller doesn't need to await the returned task — it lives on
  /// the tokio runtime and self-terminates.
  ///
  /// Phase `P2.4`: prevents the "publish terminal event → finalise →
  /// subscriber misses it" race that fired when the executor wrote
  /// the final SSE event and immediately tore the channel down.
  pub fn finalise_with_grace(&self, run_id: Uuid, grace: Duration) {
    let broker = self.clone();
    tokio::spawn(async move {
      if !grace.is_zero() {
        tokio::time::sleep(grace).await;
      }
      broker.finalise(run_id);
    });
  }

  /// Snapshot of the active per-run channel count. Useful for tests
  /// and lightweight broker diagnostics; cheap to call (a single
  /// `Mutex::lock` + `HashMap::len`).
  pub fn active_runs(&self) -> usize {
    self.inner.lock().map(|m| m.len()).unwrap_or(0)
  }

  /// Returns the number of receivers currently subscribed to
  /// `run_id`. `0` when the channel is missing entirely. Used by
  /// integration tests to assert drop-on-disconnect and by
  /// operational diagnostics.
  pub fn receiver_count(&self, run_id: Uuid) -> usize {
    let map = self.lock_inner();
    map
      .get(&run_id)
      .map(|channel| channel.sender.receiver_count())
      .unwrap_or(0)
  }

  /// W4.2c: the seq a cross-replica NOTIFY catch-up fetch should resume
  /// from for `run_id`, or `None` if this replica has no active local
  /// subscriber for that run right now — the caller should skip the DB
  /// round-trip entirely in that case, since there's nothing local to
  /// forward to. Checking `receiver_count` (not just "does the map entry
  /// exist") matters: a channel can outlive its last subscriber until
  /// [`Self::finalise`]/[`Self::finalise_with_grace`] runs, e.g. between
  /// an SSE client disconnecting and the run finishing.
  pub fn catchup_baseline(&self, run_id: Uuid) -> Option<i64> {
    let map = self.lock_inner();
    let channel = map.get(&run_id)?;
    if channel.sender.receiver_count() == 0 {
      return None;
    }
    Some(channel.last_forwarded_seq)
  }

  /// Every `(run_id, baseline)` with a live local subscriber — what a
  /// resync after a NOTIFY gap has to re-page.
  pub fn catchup_targets(&self) -> Vec<(Uuid, i64)> {
    self
      .lock_inner()
      .iter()
      .filter(|(_, channel)| channel.sender.receiver_count() > 0)
      .map(|(run_id, channel)| (*run_id, channel.last_forwarded_seq))
      .collect()
  }
}

/// Default grace period between publishing the terminal event and
/// dropping the broadcast channel. Pulled from
/// `YANSHI_BROKER_FINALIZE_GRACE_MS` at first use; falls back to
/// 500 ms when the env var is missing or unparseable.
pub fn broker_finalize_grace() -> Duration {
  let raw = std::env::var("YANSHI_BROKER_FINALIZE_GRACE_MS")
    .ok()
    .and_then(|value| value.parse::<u64>().ok())
    .unwrap_or(500);
  Duration::from_millis(raw)
}

/// Persist + publish an event in one shot. The DB row is the source of
/// truth; the broker (local fan-out + cross-replica NOTIFY) is
/// best-effort (slow subscribers may miss events, they reconnect with
/// `?after_seq=`).
pub async fn publish_through(
  repos: &Repositories,
  broker: &EventBroker,
  event: NewEvent,
) -> Result<(), DbError> {
  let stored = repos.events.append(event).await?;
  let notify_payload = RunEventNotifyPayload {
    tenant_id: stored.tenant_id.clone(),
    run_id: stored.run_id,
    seq: stored.seq,
  };
  broker.publish(StreamedEvent::from(stored));
  // W4.2c: best-effort — the DB row (already committed above) and the
  // local broadcast (already delivered above) are what actually matter;
  // a dropped NOTIFY just means another replica's subscriber lags until
  // the next successfully-delivered notification for this run, or falls
  // back to `?after_seq=` reconnect. Never fail the write over this.
  if let Ok(payload) = serde_json::to_string(&notify_payload)
    && let Err(err) =
      yanshi_db::notify(&repos.events.pool, RUN_EVENTS_NOTIFY_CHANNEL, &payload).await
  {
    warn!(
      run_id = %notify_payload.run_id,
      error = %err,
      "publish_through: cross-replica NOTIFY failed (local delivery already succeeded)"
    );
  }
  Ok(())
}

/// Payload carried on [`RUN_EVENTS_NOTIFY_CHANNEL`]. Deliberately just
/// identifiers — never the event body — since Postgres caps a NOTIFY
/// payload at ~8000 bytes; the receiving replica re-fetches the actual
/// row(s) via [`EventRepo::list_after`], the same call the SSE handler's
/// own backfill path already uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunEventNotifyPayload {
  tenant_id: String,
  run_id: Uuid,
  seq: i64,
}

/// W4.2c: cross-replica catch-up loop for [`EventBroker`]. Spawn one of
/// these per gateway replica at boot (mirrors `spawn_cleanup_loop`'s
/// "log and keep going, never crash the gateway" shape). On each
/// notification: if this replica has no active local subscriber for the
/// notified run ([`EventBroker::catchup_baseline`] returns `None`) or is
/// already caught up (`seq <= last_forwarded_seq`), it's a cheap no-op —
/// no DB hit. Otherwise it pages through [`EventRepo::list_after`]
/// starting at the broker's own `last_forwarded_seq` and republishes
/// each row locally via [`EventBroker::publish`], which is what actually
/// reaches this replica's SSE subscribers.
///
/// Reconnects on listener error (connection drop, etc.) with a short
/// backoff rather than ending the task — a permanently-dead listener
/// would silently degrade every other replica's cross-replica delivery
/// for every run this replica happens to be serving SSE for.
pub fn spawn_run_events_listener(pool: sqlx::PgPool, repos: Repositories, broker: EventBroker) {
  tokio::spawn(async move {
    loop {
      let mut listener = match NotifyListener::connect(&pool, &[RUN_EVENTS_NOTIFY_CHANNEL]).await {
        Ok(listener) => listener,
        Err(err) => {
          warn!(error = %err, "run events listener: connect failed, retrying in 5s");
          tokio::time::sleep(Duration::from_secs(5)).await;
          continue;
        }
      };
      // NOTIFY is at-most-once: re-page every locally streamed run once
      // we're listening again, so nothing sent while we weren't is lost.
      resync_run_events(&repos, &broker).await;
      loop {
        let payload = match listener.recv().await {
          Ok(Notice::Message { payload, .. }) => payload,
          Ok(Notice::Gap) => {
            warn!("run events listener: connection dropped, reconnecting and resyncing");
            break;
          }
          Err(err) => {
            warn!(error = %err, "run events listener: recv failed, reconnecting");
            break;
          }
        };
        let notify_payload: RunEventNotifyPayload = match serde_json::from_str(&payload) {
          Ok(parsed) => parsed,
          Err(err) => {
            warn!(error = %err, "run events listener: malformed notify payload, skipping");
            continue;
          }
        };
        let Some(baseline) = broker.catchup_baseline(notify_payload.run_id) else {
          continue;
        };
        if notify_payload.seq <= baseline {
          continue;
        }
        if let Err(err) = catch_up_run_events(
          &repos,
          &broker,
          &notify_payload.tenant_id,
          notify_payload.run_id,
          baseline,
        )
        .await
        {
          warn!(
            run_id = %notify_payload.run_id,
            error = %err,
            "run events listener: catch-up fetch failed"
          );
        }
      }
    }
  });
}

/// Pages `EventRepo::list_after` starting from `after_seq` and republishes
/// every returned row into `broker`'s local channel for `run_id`.
async fn resync_run_events(repos: &Repositories, broker: &EventBroker) {
  for (run_id, baseline) in broker.catchup_targets() {
    let tenant_id = match repos.runs.get(run_id).await {
      Ok(Some(run)) => run.tenant_id,
      Ok(None) => continue,
      Err(err) => {
        warn!(%run_id, error = %err, "run events listener: resync lookup failed");
        continue;
      }
    };
    if let Err(err) = catch_up_run_events(repos, broker, &tenant_id, run_id, baseline).await {
      warn!(%run_id, error = %err, "run events listener: resync fetch failed");
    }
  }
}

async fn catch_up_run_events(
  repos: &Repositories,
  broker: &EventBroker,
  tenant_id: &str,
  run_id: Uuid,
  mut after_seq: i64,
) -> Result<(), DbError> {
  loop {
    let page = repos
      .events
      .list_after(tenant_id, run_id, after_seq, 200)
      .await?;
    if page.is_empty() {
      return Ok(());
    }
    after_seq = page.last().map(|e| e.seq).unwrap_or(after_seq);
    for event in page {
      broker.publish(StreamedEvent::from(event));
    }
  }
}

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
  /// Resume after this `seq`. SSE clients reconnecting after a network blip
  /// pass the last seq they saw to avoid duplicates and gaps.
  #[serde(default)]
  pub after_seq: Option<i64>,
  /// P10.17.3: optional event-filter expression. Grammar mirrors
  /// `yanshi-ui/src/eventFilter.ts` — `kind=…` / `kind!=…` /
  /// `kind~…` / `step<op>N` joined by case-insensitive `AND`.
  /// Empty / absent → no filter (every persisted event returned).
  /// Parse errors surface as 400; valid expressions filter rows
  /// in-memory after the page read so the page-size cap still
  /// applies *to the matching set*.
  #[serde(default)]
  pub filter: Option<String>,
}

/// `GET /v1/runs/{id}/events` — server-sent events stream.
///
/// 1. Verifies the run exists; 404s if not (clients shouldn't burn a
///    long-lived connection on a typo).
/// 2. Subscribes to the broker first so events emitted while we're still
///    setting up don't fall on the floor.
/// 3. Replays any events with `seq > after_seq` (default `-1`) from the DB.
/// 4. Forwards live broker events until a terminal event
///    ([`is_terminal_run_event_kind`]) or the channel closes. A run that
///    was already terminal when the request arrived gets the backfill
///    only.
///
/// Ending on the terminal event rather than on channel close matters
/// because only the replica executing the run closes the channel: a
/// subscriber on any other replica, or one that attaches after the run
/// finished, would otherwise hold an open stream forever.
pub async fn stream_events(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(run_id): Path<Uuid>,
  Query(params): Query<EventsQuery>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
  let run = state
    .repos
    .runs
    .get(run_id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {} not found", run_id)))?;
  // P2.6 tenant boundary: same "pretend the row doesn't exist" 404
  // shape as `cancel_run`, so a cross-tenant SSE subscriber can't probe
  // by status code or — worse — silently tail another tenant's events.
  if run.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!("run {} not found", run_id)));
  }

  let mut after_seq = params.after_seq.unwrap_or(-1);

  let receiver = state.event_broker.subscribe(run_id);

  // Backfill from DB so resuming clients can see history. Page through to
  // avoid loading huge runs in one query — 200 per page is plenty for
  // human-paced SSE streams.
  let mut backfill: Vec<StreamedEvent> = Vec::new();
  loop {
    let page = state
      .repos
      .events
      .list_after(tenant.as_str(), run_id, after_seq, 200)
      .await?;
    if page.is_empty() {
      break;
    }
    after_seq = page.last().map(|e| e.seq).unwrap_or(after_seq);
    backfill.extend(page.into_iter().map(StreamedEvent::from));
    if backfill.len() >= 1_000 {
      // Defensive cap: if we somehow have more than 1k pending events, let
      // the live stream catch the tail. The client can reconnect.
      break;
    }
  }

  let finished = is_terminal_run_status(&run.status)
    || backfill
      .iter()
      .any(|event| is_terminal_run_event_kind(&event.kind));
  let backfill_stream = futures::stream::iter(backfill).map(BrokerItem::Event);
  let live_stream = if finished {
    futures::future::Either::Left(futures::stream::empty())
  } else {
    futures::future::Either::Right(
      BroadcastStream::new(receiver)
        .map(|res| match res {
          Ok(event) => BrokerItem::Event(event),
          Err(BroadcastStreamRecvError::Lagged(_)) => BrokerItem::Lagged,
        })
        .scan(false, |ended, item| {
          if *ended {
            return futures::future::ready(None);
          }
          if let BrokerItem::Event(event) = &item {
            *ended = is_terminal_run_event_kind(&event.kind);
          }
          futures::future::ready(Some(item))
        }),
    )
  };
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

/// Event kinds after which a run emits nothing more: the Flow lifecycle
/// terminals (in-process and distributed runs), the skill-run summary,
/// the stub executor's summary, the cancel route's marker, and the
/// lease-reclaim failure. `yanshi workflow logs --follow` mirrors this
/// list; change both together.
pub fn is_terminal_run_event_kind(kind: &str) -> bool {
  matches!(
    kind,
    "workflow.completed"
      | "workflow.failed"
      | "workflow.cancelled"
      | "skill_run_completed"
      | "run_completed"
      | "run.cancelled"
  )
}

pub(crate) fn is_terminal_run_status(status: &str) -> bool {
  matches!(status, "succeeded" | "failed" | "cancelled")
}

/// Internal stream item: a real event or a "you fell behind, reconnect" hint.
enum BrokerItem {
  Event(StreamedEvent),
  Lagged,
}

fn serialise_item(item: BrokerItem) -> Event {
  match item {
    BrokerItem::Event(event) => serialise_event(&event),
    BrokerItem::Lagged => {
      // Surface as a comment so the connection survives — clients should
      // reconnect with their last seen seq to refill from the DB.
      Event::default().comment("lagged: reconnect with ?after_seq=<last_seen>")
    }
  }
}

fn serialise_event(event: &StreamedEvent) -> Event {
  let json = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
  Event::default()
    .id(event.seq.to_string())
    .event(event.kind.clone())
    .data(json)
}

/// `GET /v1/runs/{id}/events/history` — return persisted events as a
/// JSON array. Supports both `?after_seq=<n>` (for pagination /
/// reconnect catch-up) and `?filter=<expr>` (P10.17.3, pre-filter
/// before the wire so long runs don't ship every event just to be
/// filtered client-side).
///
/// Filter syntax mirrors `yanshi-ui/src/eventFilter.ts`; parse
/// failures surface as 400 with the single-line parser message.
pub async fn list_events(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  Path(run_id): Path<Uuid>,
  Query(params): Query<EventsQuery>,
) -> Result<Json<Vec<StreamedEvent>>, ApiError> {
  let run = state
    .repos
    .runs
    .get(run_id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {} not found", run_id)))?;
  // P2.6 tenant boundary: collapse cross-tenant access to 404 so the
  // history endpoint doesn't leak another tenant's event log.
  if run.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!("run {} not found", run_id)));
  }

  // P10.17.3: parse the filter up front so a malformed expression
  // returns 400 instead of silently degrading to "no filter applied".
  let filter = match params.filter.as_deref() {
    Some(raw) => crate::events_filter::parse_filter(raw)
      .map_err(|err| ApiError::BadRequest(format!("invalid filter: {err}")))?,
    None => Default::default(),
  };

  let after_seq = params.after_seq.unwrap_or(-1);
  let raw_events: Vec<StreamedEvent> = state
    .repos
    .events
    .list_after(tenant.as_str(), run_id, after_seq, 1_000)
    .await?
    .into_iter()
    .map(StreamedEvent::from)
    .collect();

  // Empty filter is a no-op fast path; the `matches` helper does
  // the same check internally but skipping the iter avoids the
  // allocation for the common case.
  let events = if filter.is_empty() {
    raw_events
  } else {
    raw_events
      .into_iter()
      .filter(|e| crate::events_filter::matches(e, &filter))
      .collect()
  };
  Ok(Json(events))
}

/// Trait used by the executor to publish events. Lets the route layer
/// inject a writer that goes through both the DB and the broker without
/// the executor knowing about either type directly.
#[async_trait]
pub trait EventSink: Send + Sync {
  async fn publish(&self, event: NewEvent) -> Result<(), DbError>;
}

/// Sink that writes through the DB and forwards to the broker. The
/// executor uses this; tests can substitute a fake.
#[derive(Clone)]
pub struct PersistingEventSink {
  pub repos: Repositories,
  pub broker: EventBroker,
}

#[async_trait]
impl EventSink for PersistingEventSink {
  async fn publish(&self, event: NewEvent) -> Result<(), DbError> {
    publish_through(&self.repos, &self.broker, event).await
  }
}

/// Bridge from `yanshi_core::events::WorkflowEvent` (synchronous, fired
/// inside Flow execution) to the gateway's persisted + broadcast event
/// pipeline (async).
///
/// `EventListener::on_event` is synchronous, but `EventSink::publish` is
/// async — we bridge the two with an unbounded mpsc channel and a single
/// drain task that keeps writes ordered. The listener buffers events
/// while writes catch up; writes that fail (DB hiccup) are logged and
/// dropped, since dropping a synthetic event is safer than blocking the
/// Flow scheduler. SSE subscribers can reconnect with `?after_seq=` to
/// refill from the DB if anything was dropped. Before an executor marks
/// a run finished it awaits [`EventFlush::flushed`], so the run's events
/// are persisted ahead of its terminal status.
pub struct WorkflowEventListener {
  run_id: Uuid,
  tenant_id: String,
  tx: tokio::sync::mpsc::UnboundedSender<DrainMessage>,
  seq: std::sync::atomic::AtomicI64,
  start_seq: i64,
}

impl WorkflowEventListener {
  /// Create a listener for `run_id` under `tenant_id`. The drain task
  /// owns `sink` for its lifetime; closing the channel (drop the
  /// listener) ends the task.
  ///
  /// `start_seq` is the first event sequence number to assign — pass the
  /// last seq already persisted so the listener picks up after any
  /// pre-existing rows (avoids duplicate-key violations on the
  /// `(run_id, seq)` PK).
  pub fn new(
    run_id: Uuid,
    tenant_id: impl Into<String>,
    sink: Arc<dyn EventSink>,
    start_seq: i64,
  ) -> Self {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DrainMessage>();
    tokio::spawn(async move {
      while let Some(message) = rx.recv().await {
        match message {
          DrainMessage::Event(event) => {
            if let Err(e) = sink.publish(event).await {
              tracing::warn!(error = %e, "WorkflowEventListener: persist failed");
            }
          }
          DrainMessage::Flush(done) => {
            let _ = done.send(());
          }
        }
      }
    });
    Self {
      run_id,
      tenant_id: tenant_id.into(),
      tx,
      seq: std::sync::atomic::AtomicI64::new(start_seq),
      start_seq,
    }
  }

  /// Construct from the standard `Repositories` + `EventBroker` pair.
  pub fn from_state(
    run_id: Uuid,
    tenant_id: impl Into<String>,
    repos: Repositories,
    broker: EventBroker,
    start_seq: i64,
  ) -> Self {
    let sink: Arc<dyn EventSink> = Arc::new(PersistingEventSink { repos, broker });
    Self::new(run_id, tenant_id, sink, start_seq)
  }

  /// First sequence number this listener will assign. Useful for tests
  /// that need to predict the persisted event seq range.
  pub fn start_seq(&self) -> i64 {
    self.start_seq
  }

  /// A handle that waits for this listener's drain; take it before the
  /// listener is handed to a `Flow`.
  pub fn flush_handle(&self) -> EventFlush {
    EventFlush(self.tx.clone())
  }
}

enum DrainMessage {
  Event(NewEvent),
  Flush(tokio::sync::oneshot::Sender<()>),
}

/// Waits for a [`WorkflowEventListener`]'s drain task to catch up.
#[derive(Clone)]
pub struct EventFlush(tokio::sync::mpsc::UnboundedSender<DrainMessage>);

impl EventFlush {
  /// Resolves once every event emitted before this call has been
  /// persisted (or its write has failed and been logged). The drain is
  /// ordered, so a marker queued behind those events is reached last.
  pub async fn flushed(&self) {
    let (done, wait) = tokio::sync::oneshot::channel();
    if self.0.send(DrainMessage::Flush(done)).is_ok() {
      let _ = wait.await;
    }
  }
}

impl yanshi_core::events::EventListener for WorkflowEventListener {
  fn on_event(&self, event: &yanshi_core::events::WorkflowEvent) {
    let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let payload = workflow_event_payload(event);
    let kind = event.event_type().to_string();
    // P10.14.2-FU1: fire Prometheus counters / histograms for the
    // terminal-status + per-node-failure surface the Grafana
    // overview dashboard renders. Updates fall through whether
    // the recorder is installed or not — `metrics::counter!()`
    // is a no-op when no global recorder exists.
    use yanshi_core::events::WorkflowEvent as W;
    match event {
      W::WorkflowCompleted { duration, .. } => {
        crate::metrics::observe_workflow_completion("succeeded", duration.as_secs_f64());
      }
      W::WorkflowFailed { duration, .. } => {
        crate::metrics::observe_workflow_completion("failed", duration.as_secs_f64());
      }
      W::WorkflowCancelled { duration, .. } => {
        crate::metrics::observe_workflow_completion("cancelled", duration.as_secs_f64());
      }
      W::NodeFailed { node_id, .. } => {
        // The `WorkflowEvent::NodeFailed` payload doesn't carry
        // a `node_type` directly — we have `node_id` (a workflow-
        // local identifier like `"render"`). The capability label
        // P10.16.2 introduces is a worker-side concept that
        // doesn't surface through the workflow event stream, so
        // for now the failed-by-node-type panel buckets by
        // node_id. A future event-payload extension can split
        // the two.
        crate::metrics::observe_node_failure(Some(node_id));
      }
      _ => {}
    }
    if let Err(e) = self.tx.send(DrainMessage::Event(NewEvent {
      run_id: self.run_id,
      seq,
      kind,
      payload,
      tenant_id: Some(self.tenant_id.clone()),
    })) {
      // Drain task gone (listener was dropped). Synchronous on_event has
      // no good way to surface this — log and move on so the Flow
      // scheduler keeps making progress.
      tracing::debug!(error = %e, "WorkflowEventListener: drain task closed");
    }
  }
}

/// Convert a `WorkflowEvent` to a JSON payload suitable for the `events`
/// table. Drops `std::time::Instant` (not serialisable) and surfaces
/// duration as milliseconds; keeps `node_id`, `error`, `model`, etc. so
/// SSE subscribers can render meaningful UIs.
fn workflow_event_payload(event: &yanshi_core::events::WorkflowEvent) -> serde_json::Value {
  use yanshi_core::events::WorkflowEvent as W;
  match event {
    W::WorkflowStarted { workflow_id, .. } => serde_json::json!({"workflow_id": workflow_id}),
    W::WorkflowCompleted {
      workflow_id,
      duration,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "duration_ms": duration.as_millis() as u64,
    }),
    W::WorkflowFailed {
      workflow_id,
      error,
      duration,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "error": error,
      "duration_ms": duration.as_millis() as u64,
    }),
    W::WorkflowCancelled {
      workflow_id,
      reason,
      duration,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "reason": reason,
      "duration_ms": duration.as_millis() as u64,
    }),
    W::NodeStarted {
      workflow_id,
      node_id,
      ..
    } => serde_json::json!({"workflow_id": workflow_id, "node_id": node_id}),
    W::NodeCompleted {
      workflow_id,
      node_id,
      duration,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "node_id": node_id,
      "duration_ms": duration.as_millis() as u64,
    }),
    W::NodeOutputCaptured {
      workflow_id,
      node_id,
      output,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "node_id": node_id,
      "output": output,
    }),
    W::NodeFailed {
      workflow_id,
      node_id,
      error,
      duration,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "node_id": node_id,
      "error": error,
      "duration_ms": duration.as_millis() as u64,
    }),
    W::NodeSkipped {
      workflow_id,
      node_id,
      reason,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "node_id": node_id,
      "reason": reason,
    }),
    W::CheckpointSaved {
      workflow_id,
      checkpoint_id,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "checkpoint_id": checkpoint_id,
    }),
    W::CheckpointRestored {
      workflow_id,
      checkpoint_id,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "checkpoint_id": checkpoint_id,
    }),
    W::RetryAttempt {
      workflow_id,
      node_id,
      attempt,
      max_attempts,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "node_id": node_id,
      "attempt": attempt,
      "max_attempts": max_attempts,
    }),
    W::ResourceWarning {
      workflow_id,
      resource_type,
      usage,
      limit,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "resource_type": resource_type,
      "usage": usage,
      "limit": limit,
    }),
    W::LLMPromptSent {
      workflow_id,
      node_id,
      model,
      provider,
      temperature,
      max_tokens,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "node_id": node_id,
      "model": model,
      "provider": provider,
      "temperature": temperature,
      "max_tokens": max_tokens,
    }),
    W::LLMResponseReceived {
      workflow_id,
      node_id,
      model,
      duration,
      usage,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "node_id": node_id,
      "model": model,
      "duration_ms": duration.as_millis() as u64,
      "usage": usage.as_ref().map(|u| serde_json::json!({
        "prompt_tokens": u.prompt_tokens,
        "completion_tokens": u.completion_tokens,
        "total_tokens": u.total_tokens,
      })),
    }),
    W::ResumeDecisionRecorded {
      workflow_id,
      node_id,
      tool_call_id,
      tool,
      step_index,
      idempotency,
      decision,
      reason,
      force_replay,
      ..
    } => serde_json::json!({
      "workflow_id": workflow_id,
      "node_id": node_id,
      "resume": {
        "tool_call_id": tool_call_id,
        "tool": tool,
        "step_index": step_index,
        "idempotency": idempotency,
        "decision": decision,
        "reason": reason,
        "force_replay": force_replay,
      },
    }),
    // `WorkflowEvent` is `#[non_exhaustive]`: a future variant we don't yet
    // render falls back to an empty payload rather than failing to compile.
    _ => serde_json::json!({}),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::Utc;

  fn sample_event(run_id: Uuid, seq: i64) -> StreamedEvent {
    StreamedEvent {
      run_id,
      seq,
      kind: "test".into(),
      payload: serde_json::json!({"seq": seq}),
      ts: Utc::now(),
    }
  }

  #[tokio::test]
  async fn broker_subscribe_then_publish_delivers_event() {
    let broker = EventBroker::new();
    let run_id = Uuid::new_v4();
    let mut rx = broker.subscribe(run_id);
    broker.publish(sample_event(run_id, 0));
    let received = rx.recv().await.expect("event delivered");
    assert_eq!(received.seq, 0);
  }

  #[tokio::test]
  async fn broker_isolates_events_per_run_id() {
    let broker = EventBroker::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut rx_a = broker.subscribe(a);
    let _rx_b = broker.subscribe(b);
    broker.publish(sample_event(b, 0));
    // rx_a should see nothing — `try_recv` returns Empty.
    assert!(rx_a.try_recv().is_err());
  }

  #[tokio::test]
  async fn broker_finalise_closes_subscribers() {
    let broker = EventBroker::new();
    let run_id = Uuid::new_v4();
    let mut rx = broker.subscribe(run_id);
    broker.finalise(run_id);
    // After finalise the sender is dropped; recv eventually yields Closed.
    let result = rx.recv().await;
    assert!(matches!(result, Err(broadcast::error::RecvError::Closed)));
  }

  #[tokio::test]
  async fn broker_finalise_with_grace_preserves_terminal_event_for_subscriber() {
    let broker = EventBroker::new();
    let run_id = Uuid::new_v4();
    let mut rx = broker.subscribe(run_id);

    // Publish a terminal event then schedule finalise with a short grace
    // window. The subscriber should still receive the terminal event
    // before the channel goes away.
    broker.publish(sample_event(run_id, 0));
    broker.finalise_with_grace(run_id, std::time::Duration::from_millis(50));

    let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
      .await
      .expect("recv did not time out")
      .expect("terminal event delivered before channel teardown");
    assert_eq!(received.seq, 0);

    // After the grace window elapses, the channel is removed from the
    // broker.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(broker.active_runs(), 0);
  }

  #[tokio::test]
  async fn broker_finalise_with_zero_grace_still_removes_channel() {
    let broker = EventBroker::new();
    let run_id = Uuid::new_v4();
    let _rx = broker.subscribe(run_id);
    broker.finalise_with_grace(run_id, std::time::Duration::ZERO);
    // Allow the spawned task to run.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(broker.active_runs(), 0);
  }

  #[tokio::test]
  async fn broker_receiver_count_tracks_subscriber_drops() {
    let broker = EventBroker::new();
    let run_id = Uuid::new_v4();
    let rx_one = broker.subscribe(run_id);
    let rx_two = broker.subscribe(run_id);
    assert_eq!(broker.receiver_count(run_id), 2);
    drop(rx_one);
    // tokio::broadcast updates receiver_count lazily on send/recv; force
    // an interaction by publishing a no-op event.
    broker.publish(sample_event(run_id, 0));
    assert_eq!(broker.receiver_count(run_id), 1);
    drop(rx_two);
    broker.publish(sample_event(run_id, 1));
    assert_eq!(broker.receiver_count(run_id), 0);
  }

  /// W4.2c: `catchup_baseline` is the gate `spawn_run_events_listener`
  /// uses to decide whether a NOTIFY is worth a DB round-trip. No local
  /// subscriber (channel never created) → `None`. A local publish must
  /// advance the baseline so a same-origin NOTIFY resolves to a no-op
  /// catch-up (an empty `list_after` page) rather than redundant work.
  #[tokio::test]
  async fn catchup_baseline_reflects_local_subscriber_and_publish_state() {
    let broker = EventBroker::new();
    let run_id = Uuid::new_v4();
    assert_eq!(
      broker.catchup_baseline(run_id),
      None,
      "no channel at all yet — no local subscriber to catch up"
    );

    let _rx = broker.subscribe(run_id);
    assert_eq!(
      broker.catchup_baseline(run_id),
      Some(-1),
      "fresh channel with a subscriber starts at -1, matching after_seq's own default"
    );

    broker.publish(sample_event(run_id, 5));
    assert_eq!(
      broker.catchup_baseline(run_id),
      Some(5),
      "local publish must advance the baseline"
    );
  }

  /// A channel that exists (e.g. the run hasn't finished yet) but has no
  /// *active* subscriber right now must still report `None` — the
  /// listener should skip the DB round-trip even though the map entry
  /// is present.
  #[tokio::test]
  async fn catchup_baseline_is_none_after_last_subscriber_disconnects() {
    let broker = EventBroker::new();
    let run_id = Uuid::new_v4();
    let rx = broker.subscribe(run_id);
    assert!(broker.catchup_baseline(run_id).is_some());
    drop(rx);
    // `tokio::broadcast` updates receiver_count lazily on send/recv;
    // force an interaction, mirroring `broker_receiver_count_tracks_
    // subscriber_drops` above.
    broker.publish(sample_event(run_id, 0));
    assert_eq!(
      broker.catchup_baseline(run_id),
      None,
      "channel entry survives (run not finalised) but has zero active receivers"
    );
  }

  #[tokio::test]
  async fn broker_subscriber_disconnect_does_not_block_other_subscribers() {
    let broker = EventBroker::new();
    let run_id = Uuid::new_v4();
    let rx_short = broker.subscribe(run_id);
    let mut rx_long = broker.subscribe(run_id);
    // Drop the short-lived subscriber to simulate a client disconnect.
    drop(rx_short);
    // The remaining subscriber should still receive published events.
    broker.publish(sample_event(run_id, 0));
    let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx_long.recv())
      .await
      .expect("recv did not time out")
      .expect("surviving subscriber gets the event");
    assert_eq!(received.seq, 0);
  }

  #[test]
  fn broker_finalize_grace_env_transitions_are_picked_up() {
    // Single test (synchronous, no tokio scheduler racing with other
    // tokio::test cases) walks default → override → invalid →
    // default. The three states share one env var, so they must
    // run serially.
    // SAFETY: the env var is dedicated to this surface and no other
    // test reads it during this test's lifetime.
    unsafe {
      std::env::remove_var("YANSHI_BROKER_FINALIZE_GRACE_MS");
    }
    assert_eq!(
      broker_finalize_grace(),
      std::time::Duration::from_millis(500),
      "unset → 500 ms default"
    );

    unsafe {
      std::env::set_var("YANSHI_BROKER_FINALIZE_GRACE_MS", "120");
    }
    assert_eq!(
      broker_finalize_grace(),
      std::time::Duration::from_millis(120),
      "valid override honored"
    );

    unsafe {
      std::env::set_var("YANSHI_BROKER_FINALIZE_GRACE_MS", "not-a-number");
    }
    assert_eq!(
      broker_finalize_grace(),
      std::time::Duration::from_millis(500),
      "invalid value → 500 ms default"
    );

    unsafe {
      std::env::remove_var("YANSHI_BROKER_FINALIZE_GRACE_MS");
    }
  }

  /// Executors used to sleep 50ms and hope the drain had persisted the
  /// terminal events before they marked the run finished; under CI load
  /// it had not. `flushed` waits for every event emitted before it.
  #[tokio::test]
  async fn flushed_waits_for_every_prior_event_to_persist() {
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};
    use yanshi_core::events::{EventListener, WorkflowEvent};

    struct SlowSink(Arc<StdMutex<Vec<String>>>);

    #[async_trait]
    impl EventSink for SlowSink {
      async fn publish(&self, event: NewEvent) -> Result<(), DbError> {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(mut kinds) = self.0.lock() {
          kinds.push(event.kind);
        }
        Ok(())
      }
    }

    let persisted = Arc::new(StdMutex::new(Vec::new()));
    let sink: Arc<dyn EventSink> = Arc::new(SlowSink(persisted.clone()));
    let listener = WorkflowEventListener::new(Uuid::new_v4(), "default", sink, 0);
    let flush = listener.flush_handle();
    listener.on_event(&WorkflowEvent::NodeStarted {
      workflow_id: "demo".into(),
      node_id: "n1".into(),
      timestamp: Instant::now(),
    });
    listener.on_event(&WorkflowEvent::WorkflowCompleted {
      workflow_id: "demo".into(),
      duration: Duration::from_millis(1),
      timestamp: Instant::now(),
    });

    flush.flushed().await;
    assert_eq!(
      *persisted.lock().unwrap(),
      vec!["node.started".to_string(), "workflow.completed".to_string()]
    );
  }

  #[tokio::test]
  async fn workflow_event_listener_bridges_to_sink() {
    use std::time::{Duration, Instant};
    use yanshi_core::events::{EventListener, WorkflowEvent};

    /// In-memory sink that records every published event for assertions.
    struct CapturingSink {
      tx: tokio::sync::mpsc::UnboundedSender<NewEvent>,
    }

    #[async_trait]
    impl EventSink for CapturingSink {
      async fn publish(&self, event: NewEvent) -> Result<(), DbError> {
        self.tx.send(event).expect("test channel closed");
        Ok(())
      }
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<NewEvent>();
    let sink: Arc<dyn EventSink> = Arc::new(CapturingSink { tx });
    let run_id = Uuid::new_v4();
    let listener = WorkflowEventListener::new(run_id, "default", sink, 0);

    listener.on_event(&WorkflowEvent::WorkflowStarted {
      workflow_id: "demo".into(),
      timestamp: Instant::now(),
    });
    listener.on_event(&WorkflowEvent::NodeStarted {
      workflow_id: "demo".into(),
      node_id: "n1".into(),
      timestamp: Instant::now(),
    });
    listener.on_event(&WorkflowEvent::NodeCompleted {
      workflow_id: "demo".into(),
      node_id: "n1".into(),
      duration: Duration::from_millis(7),
      timestamp: Instant::now(),
    });

    // Drain task is async; give it a moment to flush all three events.
    let mut events = Vec::new();
    for _ in 0..3 {
      let event = tokio::time::timeout(Duration::from_millis(500), rx.recv())
        .await
        .expect("listener delivers events promptly")
        .expect("listener channel still open");
      events.push(event);
    }
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].seq, 0);
    assert_eq!(events[0].kind, "workflow.started");
    assert_eq!(events[1].kind, "node.started");
    assert_eq!(events[2].kind, "node.completed");
    assert_eq!(events[2].payload["duration_ms"], 7);
    assert_eq!(events[2].payload["node_id"], "n1");
  }

  #[test]
  fn workflow_event_payload_preserves_error_text() {
    use std::time::{Duration, Instant};
    use yanshi_core::events::WorkflowEvent;

    let event = WorkflowEvent::NodeFailed {
      workflow_id: "demo".into(),
      node_id: "n1".into(),
      error: "boom".into(),
      duration: Duration::from_millis(20),
      timestamp: Instant::now(),
    };
    let payload = workflow_event_payload(&event);
    assert_eq!(payload["error"], "boom");
    assert_eq!(payload["node_id"], "n1");
    assert_eq!(payload["duration_ms"], 20);
  }
}
