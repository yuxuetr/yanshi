//! End-to-end SSE check: submit a run, attach to `/v1/runs/{id}/events`,
//! observe the Flow executor's workflow / node events.
//!
//! Gated by `YANSHI_DATABASE_TEST_URL` like the rest of the
//! yanshi-server e2e tests.

use axum::{
  body::Body,
  http::{Request, StatusCode, header::CONTENT_TYPE},
};
use futures::StreamExt;
use serde_json::json;
use tokio::time::{Duration, timeout};
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::Database;
use yanshi_server::{AppState, create_router};

const FIXED_DAG_WORKFLOW: &str = r#"
name: Server SSE Demo
nodes:
  - id: render
    type: template
    parameters:
      template: "hello sse"
"#;

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn fresh_state() -> Option<AppState> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  // No TRUNCATE: tests run in parallel and each uses fresh ids, so a
  // global reset only races sibling tests.
  Some(AppState::new(db))
}

#[tokio::test]
async fn sse_stream_yields_run_started_and_completed_events() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping sse_stream_yields_run_started_and_completed_events");
    return;
  };
  let app = create_router(state.clone());

  // Submit the run.
  let response = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/runs")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
          json!({"workflow": FIXED_DAG_WORKFLOW}).to_string(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let bytes = axum::body::to_bytes(response.into_body(), 4096)
    .await
    .unwrap();
  let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
  let run_id: Uuid = body["run_id"].as_str().unwrap().parse().unwrap();

  // Open the SSE stream and read until the workflow lifecycle arrives.
  let response = app
    .oneshot(
      Request::builder()
        .uri(format!("/v1/runs/{}/events", run_id))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // axum's Sse keeps the body open; read chunks with a deadline. The
  // Flow executor writes named events plus periodic keep-alive comments.
  let mut body = response.into_body().into_data_stream();
  let mut buf = String::new();
  let deadline = Duration::from_secs(2);
  let read = timeout(deadline, async {
    while let Some(Ok(chunk)) = body.next().await {
      buf.push_str(&String::from_utf8_lossy(&chunk));
      if buf.contains("event: workflow.started")
        && buf.contains("event: node.started")
        && buf.contains("event: node.completed")
        && buf.contains("event: workflow.completed")
      {
        return Ok::<(), &'static str>(());
      }
    }
    Err("stream closed before workflow lifecycle arrived")
  })
  .await;

  assert!(
    matches!(read, Ok(Ok(()))),
    "did not receive workflow lifecycle events. captured:\n{buf}"
  );
}

#[tokio::test]
async fn event_history_returns_persisted_stream() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping event_history_returns_persisted_stream");
    return;
  };
  let app = create_router(state);

  let response = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/runs")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
          json!({"workflow": FIXED_DAG_WORKFLOW}).to_string(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let bytes = axum::body::to_bytes(response.into_body(), 4096)
    .await
    .unwrap();
  let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
  let run_id: Uuid = body["run_id"].as_str().unwrap().parse().unwrap();

  tokio::time::sleep(Duration::from_millis(100)).await;

  let response = app
    .oneshot(
      Request::builder()
        .uri(format!("/v1/runs/{}/events/history", run_id))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let bytes = axum::body::to_bytes(response.into_body(), 8192)
    .await
    .unwrap();
  let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
  let events = body.as_array().unwrap();
  assert!(
    events
      .iter()
      .any(|event| event["kind"] == "workflow.started")
  );
  assert!(events.iter().any(|event| event["kind"] == "node.completed"));
}
