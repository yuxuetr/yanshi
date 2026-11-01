//! W4.2c — cross-replica SSE delivery via Postgres NOTIFY.
//!
//! Builds two independent `AppState` instances against the same
//! Postgres database, each with its own `EventBroker` + its own
//! `spawn_run_events_listener` task, to stand in for two separate
//! gateway replicas. Publishing an event through replica A's
//! `publish_through` must reach a client subscribed to replica B's
//! `GET /v1/runs/{id}/events` — proving delivery goes through the
//! NOTIFY-driven catch-up path (`events_stream::spawn_run_events_listener`)
//! rather than replica B ever touching replica A's local broker object.
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`. Without
//! it the tests self-skip, matching every other server e2e test file.

use axum::{
  body::Body,
  http::{Request, StatusCode},
};
use futures::StreamExt;
use serde_json::json;
use tokio::time::{Duration, timeout};
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::{Database, NewEvent, NewRun, RunRepo, RunStatus};
use yanshi_server::{AppState, create_router, events_stream};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

/// Two `AppState`s sharing one Postgres database, each with its own
/// in-process broker + NOTIFY listener — the minimal stand-in for "two
/// gateway replicas" needed to prove cross-replica delivery without
/// actually running two processes.
async fn two_replica_states() -> Option<(AppState, AppState)> {
  let url = live_url()?;
  let db_a = Database::connect_and_migrate(&url, 4).await.ok()?;
  // No TRUNCATE: tests run in parallel and each uses fresh ids, so a
  // global reset only races sibling tests.
  let db_b = Database::connect(&url, 4).await.ok()?;

  let state_a = AppState::new(db_a.clone());
  let state_b = AppState::new(db_b.clone());

  events_stream::spawn_run_events_listener(
    db_a.pool.clone(),
    state_a.repos.clone(),
    state_a.event_broker.clone(),
  );
  events_stream::spawn_run_events_listener(
    db_b.pool.clone(),
    state_b.repos.clone(),
    state_b.event_broker.clone(),
  );

  Some((state_a, state_b))
}

#[tokio::test]
async fn event_published_via_replica_a_reaches_a_subscriber_on_replica_b() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!("skipping event_published_via_replica_a_reaches_a_subscriber_on_replica_b");
    return;
  };

  let run_id = Uuid::new_v4();
  let tenant_id = format!("tenant-cross-events-{}", Uuid::new_v4());
  state_a
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      workflow: "@test".to_string(),
      status: RunStatus::Running,
      run_dir: None,
      tenant_id: tenant_id.clone(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .expect("run row created via replica A");

  // Subscribe through replica B's own SSE route — this is the client
  // that must observe an event it never asked replica A about directly.
  let app_b = create_router(state_b.clone());
  let sse_response = app_b
    .oneshot(
      Request::builder()
        .uri(format!("/v1/runs/{run_id}/events"))
        .header("X-Yanshi-Tenant", &tenant_id)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(sse_response.status(), StatusCode::OK);
  let mut body = sse_response.into_body().into_data_stream();

  // Give replica B's `stream_events` handler time to actually call
  // `broker.subscribe(run_id)` before we publish — otherwise the
  // publish could race ahead of the subscription.
  tokio::time::sleep(Duration::from_millis(150)).await;

  events_stream::publish_through(
    &state_a.repos,
    &state_a.event_broker,
    NewEvent {
      run_id,
      seq: 0,
      kind: "cross_replica_test".to_string(),
      payload: json!({"hello": "from replica a"}),
      tenant_id: Some(tenant_id.clone()),
    },
  )
  .await
  .expect("publish via replica A");

  let mut buf = String::new();
  let read = timeout(Duration::from_secs(10), async {
    while let Some(Ok(chunk)) = body.next().await {
      buf.push_str(&String::from_utf8_lossy(&chunk));
      if buf.contains("event: cross_replica_test") {
        return Ok::<(), &'static str>(());
      }
    }
    Err("stream closed before the cross-replica event arrived")
  })
  .await;

  assert!(
    matches!(read, Ok(Ok(()))),
    "replica B's SSE stream never observed an event published via replica A. captured:\n{buf}"
  );
  assert!(buf.contains("from replica a"));

  // Replica B's own broker must have actually received it (proves the
  // NOTIFY listener path, not some other coincidence) — replica A's
  // broker was never touched by anything replica B did.
  assert!(state_b.event_broker.receiver_count(run_id) >= 1);
}

/// Regression: a replica with no local subscriber for a run must not do
/// any DB work in response to a NOTIFY for that run (the "cheap no-op"
/// path in `EventBroker::catchup_baseline`). Indirect proof: publish
/// several events via replica A while replica B has never subscribed to
/// that run_id, then confirm replica B's broker never created a channel
/// entry for it (`active_runs` / `receiver_count` stay at baseline).
#[tokio::test]
async fn replica_with_no_local_subscriber_does_not_create_a_channel_entry() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!("skipping replica_with_no_local_subscriber_does_not_create_a_channel_entry");
    return;
  };

  let run_id = Uuid::new_v4();
  let tenant_id = format!("tenant-cross-events-{}", Uuid::new_v4());
  state_a
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      workflow: "@test".to_string(),
      status: RunStatus::Running,
      run_dir: None,
      tenant_id: tenant_id.clone(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .expect("run row created via replica A");

  events_stream::publish_through(
    &state_a.repos,
    &state_a.event_broker,
    NewEvent {
      run_id,
      seq: 0,
      kind: "no_subscriber_test".to_string(),
      payload: json!({}),
      tenant_id: Some(tenant_id.clone()),
    },
  )
  .await
  .expect("publish via replica A");

  // Give replica B's listener plenty of time to (not) act on the
  // notification.
  tokio::time::sleep(Duration::from_millis(300)).await;

  assert_eq!(
    state_b.event_broker.receiver_count(run_id),
    0,
    "replica B must not create a channel entry for a run nobody local is watching"
  );
}

/// Regression (found on a 2-replica k3s install): a subscriber on the
/// replica that is *not* executing the run got every event that predated
/// its subscription twice — once from the DB backfill, then again live.
/// Replica B's channel for the run is brand new (`last_forwarded_seq =
/// -1`), so the next NOTIFY made the catch-up fetch re-page from the
/// start and republish what the backfill had already sent.
#[tokio::test]
async fn subscriber_on_replica_b_sees_backfilled_events_exactly_once() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!("skipping subscriber_on_replica_b_sees_backfilled_events_exactly_once");
    return;
  };

  let run_id = Uuid::new_v4();
  let tenant_id = format!("tenant-cross-events-{}", Uuid::new_v4());
  state_a
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      workflow: "@test".to_string(),
      status: RunStatus::Running,
      run_dir: None,
      tenant_id: tenant_id.clone(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .expect("run row created via replica A");

  let publish = |seq: i64, kind: &str| {
    let repos = state_a.repos.clone();
    let broker = state_a.event_broker.clone();
    let event = NewEvent {
      run_id,
      seq,
      kind: kind.to_string(),
      payload: json!({"seq": seq}),
      tenant_id: Some(tenant_id.clone()),
    };
    async move { events_stream::publish_through(&repos, &broker, event).await }
  };

  // Two events land before anyone on replica B is watching.
  publish(0, "workflow.started").await.expect("seq 0");
  publish(1, "node.started").await.expect("seq 1");

  let app_b = create_router(state_b.clone());
  let sse_response = app_b
    .oneshot(
      Request::builder()
        .uri(format!("/v1/runs/{run_id}/events"))
        .header("X-Yanshi-Tenant", &tenant_id)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(sse_response.status(), StatusCode::OK);
  let mut body = sse_response.into_body().into_data_stream();
  tokio::time::sleep(Duration::from_millis(150)).await;

  publish(2, "workflow.completed").await.expect("seq 2");

  let mut buf = String::new();
  let read = timeout(Duration::from_secs(10), async {
    while let Some(Ok(chunk)) = body.next().await {
      buf.push_str(&String::from_utf8_lossy(&chunk));
    }
  })
  .await;
  assert!(
    read.is_ok(),
    "stream did not end on the terminal event. captured:\n{buf}"
  );

  let ids: Vec<&str> = buf
    .lines()
    .filter_map(|line| line.strip_prefix("id:"))
    .map(str::trim)
    .collect();
  assert_eq!(
    ids,
    ["0", "1", "2"],
    "each event exactly once, in order. captured:\n{buf}"
  );
}
