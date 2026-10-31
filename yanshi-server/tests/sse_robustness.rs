//! P2.4 SSE robustness suite. Verifies reconnect behavior across the
//! three lifecycle states the broker can be in:
//!
//! - **active run** — events still arriving, broker channel live.
//! - **recently completed** — broker channel finalised but DB has
//!   every event.
//! - **long-completed** — broker dropped its channel entry, DB still
//!   serves backfill.
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`.
//! Without it the tests self-skip so workspace `cargo test` stays
//! hermetic.

use axum::{
  body::Body,
  http::{Request, StatusCode},
};
use futures::StreamExt;
use serde_json::json;
use tokio::time::{Duration, timeout};
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::{Database, EventRepo, NewEvent, NewRun, RunRepo, RunStatus};
use yanshi_server::{AppState, create_router};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

// No TRUNCATE: tests run in parallel and every run id is fresh, so a
// truncate from one test only deleted another's rows mid-test.
async fn fresh_state() -> Option<AppState> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  Some(AppState::new(db))
}

async fn insert_terminal_run(state: &AppState, status: RunStatus) -> Uuid {
  let id = Uuid::new_v4();
  state
    .repos
    .runs
    .create(NewRun {
      id,
      tenant_id: "default".into(),
      workflow: "name: stub".into(),
      status: RunStatus::Running,
      run_dir: None,
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .unwrap();
  state
    .repos
    .runs
    .update_status(id, status, None)
    .await
    .unwrap();
  id
}

async fn insert_event(state: &AppState, run_id: Uuid, seq: i64, kind: &str) {
  state
    .repos
    .events
    .append(NewEvent {
      run_id,
      seq,
      kind: kind.to_string(),
      payload: json!({"seq": seq}),
      tenant_id: None,
    })
    .await
    .unwrap();
}

async fn read_sse_lines(
  app: axum::Router,
  run_id: Uuid,
  after_seq: i64,
  read_for: Duration,
) -> Vec<String> {
  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/runs/{run_id}/events?after_seq={after_seq}"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body().into_data_stream();
  let mut buffer = String::new();
  // Read until the channel is dropped (recently / long completed) or
  // we hit our overall window (active run).
  let _ = timeout(read_for, async {
    while let Some(chunk) = body.next().await {
      if let Ok(bytes) = chunk {
        buffer.push_str(&String::from_utf8_lossy(&bytes));
      }
    }
  })
  .await;
  buffer.lines().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn reconnect_after_seq_replays_recently_completed_run_from_db() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping reconnect_after_seq_replays_recently_completed_run_from_db");
    return;
  };
  // Build a run that already finished and has 3 events persisted.
  let run_id = insert_terminal_run(&state, RunStatus::Succeeded).await;
  for seq in 0..3 {
    insert_event(&state, run_id, seq, "node.started").await;
  }
  // Finalise the broker channel like the executor would.
  state.event_broker.finalise(run_id);

  // Reconnect with after_seq = 0; we should backfill events 1..2.
  let lines = read_sse_lines(
    create_router(state.clone()),
    run_id,
    0,
    Duration::from_millis(500),
  )
  .await;
  let ids: Vec<&str> = lines
    .iter()
    .filter_map(|line| line.strip_prefix("id: "))
    .collect();
  // SSE id corresponds to the event seq. We expect 1 and 2 (after_seq=0
  // is exclusive).
  assert!(ids.contains(&"1"), "missing seq 1 in {ids:?}");
  assert!(ids.contains(&"2"), "missing seq 2 in {ids:?}");
  assert!(
    !ids.contains(&"0"),
    "must skip events at-or-before after_seq"
  );
}

#[tokio::test]
async fn reconnect_against_long_completed_run_serves_only_db_history() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping reconnect_against_long_completed_run_serves_only_db_history");
    return;
  };
  // Build a "long completed" run: no broker subscription, no
  // subscriber waiting, only DB events.
  let run_id = insert_terminal_run(&state, RunStatus::Cancelled).await;
  for seq in 0..4 {
    insert_event(&state, run_id, seq, "node.completed").await;
  }
  // Drop any broker channel that might have been created by event
  // persistence (`publish` lazily creates one).
  state.event_broker.finalise(run_id);

  // Force a fresh SSE attach with after_seq = -1; we should get all 4.
  let lines = read_sse_lines(
    create_router(state.clone()),
    run_id,
    -1,
    Duration::from_millis(500),
  )
  .await;
  let ids: Vec<&str> = lines
    .iter()
    .filter_map(|line| line.strip_prefix("id: "))
    .collect();
  for expected in ["0", "1", "2", "3"] {
    assert!(ids.contains(&expected), "missing seq {expected} in {ids:?}");
  }
}

#[tokio::test]
async fn reconnect_with_after_seq_above_all_persisted_returns_no_history() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping reconnect_with_after_seq_above_all_persisted_returns_no_history");
    return;
  };
  let run_id = insert_terminal_run(&state, RunStatus::Succeeded).await;
  for seq in 0..2 {
    insert_event(&state, run_id, seq, "node.started").await;
  }
  state.event_broker.finalise(run_id);

  let lines = read_sse_lines(
    create_router(state.clone()),
    run_id,
    99,
    Duration::from_millis(300),
  )
  .await;
  let ids: Vec<&str> = lines
    .iter()
    .filter_map(|line| line.strip_prefix("id: "))
    .collect();
  assert!(
    ids.is_empty(),
    "after_seq above max must yield no events: {ids:?}"
  );
}

#[tokio::test]
async fn sse_against_unknown_run_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping sse_against_unknown_run_returns_404");
    return;
  };
  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/runs/{}/events", Uuid::new_v4()))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn client_disconnect_mid_stream_drops_broker_receiver_count() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping client_disconnect_mid_stream_drops_broker_receiver_count");
    return;
  };
  // Build an active run with one persisted event so the SSE handler
  // has something to backfill but stays attached for the live tail.
  let id = Uuid::new_v4();
  state
    .repos
    .runs
    .create(NewRun {
      id,
      tenant_id: "default".into(),
      workflow: "name: stub".into(),
      status: RunStatus::Running,
      run_dir: None,
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .unwrap();
  insert_event(&state, id, 0, "node.started").await;

  let app = create_router(state.clone());
  // Attach + immediately drop the response stream.
  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/runs/{id}/events"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  // Pull a single chunk so the handler had a chance to register the
  // subscriber, then drop.
  let mut body = response.into_body().into_data_stream();
  let _first = timeout(Duration::from_millis(200), body.next()).await;
  drop(body);

  // Give the runtime a moment to observe the drop.
  tokio::time::sleep(Duration::from_millis(50)).await;

  // Forcing the broker to publish triggers receiver-count
  // bookkeeping inside tokio::broadcast. We expect the disconnect to
  // have already removed our subscriber.
  state.event_broker.publish(yanshi_server::StreamedEvent {
    run_id: id,
    seq: 1,
    kind: "ping".into(),
    payload: json!({}),
    ts: chrono::Utc::now(),
  });
  assert_eq!(state.event_broker.receiver_count(id), 0);
}
