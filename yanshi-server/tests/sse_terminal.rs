//! A run's SSE stream ends once the run is over, whichever replica serves
//! it. Only the replica executing a run closes its broker channel, so a
//! stream that waited for channel close hung forever on every other
//! replica, and for any subscriber that attached after the run finished.
//!
//! Requires Postgres via `YANSHI_DATABASE_TEST_URL` (self-skips without).

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
use yanshi_server::{AppState, create_router, publish_through};

async fn state_with_run(status: RunStatus) -> Option<(AppState, Uuid)> {
  let url = std::env::var("YANSHI_DATABASE_TEST_URL").ok()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  let state = AppState::new(db);
  let run_id = Uuid::new_v4();
  state
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      workflow: "@test".to_string(),
      status,
      run_dir: None,
      tenant_id: "default".to_string(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .ok()?;
  Some((state, run_id))
}

async fn append(state: &AppState, run_id: Uuid, seq: i64, kind: &str) {
  publish_through(
    &state.repos,
    &state.event_broker,
    NewEvent {
      run_id,
      seq,
      kind: kind.to_string(),
      payload: json!({}),
      tenant_id: Some("default".to_string()),
    },
  )
  .await
  .unwrap();
}

/// Reads the SSE body to its end; `None` if it is still open after 3s.
async fn read_to_end(body: Body) -> Option<String> {
  let mut stream = body.into_data_stream();
  let mut buf = String::new();
  timeout(Duration::from_secs(3), async {
    while let Some(Ok(chunk)) = stream.next().await {
      buf.push_str(&String::from_utf8_lossy(&chunk));
    }
  })
  .await
  .ok()?;
  Some(buf)
}

async fn open_stream(state: &AppState, run_id: Uuid) -> Body {
  let response = create_router(state.clone())
    .oneshot(
      Request::builder()
        .uri(format!("/v1/runs/{run_id}/events"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  response.into_body()
}

#[tokio::test]
async fn stream_for_a_finished_run_ends_after_the_backfill() {
  let Some((state, run_id)) = state_with_run(RunStatus::Running).await else {
    eprintln!("skipping stream_for_a_finished_run_ends_after_the_backfill: no DB url");
    return;
  };
  append(&state, run_id, 0, "workflow.started").await;
  append(&state, run_id, 1, "workflow.completed").await;
  state
    .repos
    .runs
    .update_status(run_id, RunStatus::Succeeded, None)
    .await
    .unwrap();

  let body = read_to_end(open_stream(&state, run_id).await)
    .await
    .expect("stream for a finished run must end");
  assert!(body.contains("event: workflow.completed"), "{body}");
}

/// The cross-replica shape: the terminal event reaches this replica's
/// broker, but this replica never finalises the channel.
#[tokio::test]
async fn live_stream_ends_on_a_terminal_event_without_channel_close() {
  let Some((state, run_id)) = state_with_run(RunStatus::Running).await else {
    eprintln!("skipping live_stream_ends_on_a_terminal_event_without_channel_close: no DB url");
    return;
  };
  append(&state, run_id, 0, "workflow.started").await;
  let body = open_stream(&state, run_id).await;

  let publisher = state.clone();
  tokio::spawn(async move {
    tokio::time::sleep(Duration::from_millis(200)).await;
    append(&publisher, run_id, 1, "node.started").await;
    append(&publisher, run_id, 2, "workflow.failed").await;
    append(&publisher, run_id, 3, "after.terminal").await;
  });

  let body = read_to_end(body)
    .await
    .expect("stream must end on the terminal event");
  assert!(body.contains("event: node.started"), "{body}");
  assert!(body.contains("event: workflow.failed"), "{body}");
  assert!(!body.contains("after.terminal"), "{body}");
}
