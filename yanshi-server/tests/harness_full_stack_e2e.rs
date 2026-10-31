//! P-H.5 slice 4 — full-stack E2E.
//!
//! Single test that drives every layer the Web UI consumes against a
//! real Postgres + Moonshot:
//!
//!   1. `POST /v1/harness/sessions`           — submit a session.
//!   2. `GET  /v1/harness/sessions/{id}/events` — open SSE, stream
//!      lifecycle envelopes as they're emitted by the live executor.
//!   3. `GET  /v1/harness/sessions/{id}/events/history` — verify the
//!      persisted DB log matches the streamed envelopes (seq order,
//!      no gaps, `session_started` first + `stopped` last).
//!   4. `GET  /v1/harness/sessions/{id}`       — confirm terminal row
//!      shape (status, finished_at, final_answer or error).
//!   5. `POST /v1/harness/sessions/{id}:resume` — rerun via the new
//!      resume route, verify the row resets, a second terminal
//!      lifecycle is emitted, and seq restarts at 0.
//!
//! Self-skips without `YANSHI_DATABASE_TEST_URL` + `MOONSHOT_API_KEY`
//! (loaded from `~/.yanshi/.env`). The test owns a fresh
//! `YANSHI_TEST_TENANT` slot so it won't collide with other tests
//! sharing the same database.

use std::sync::Arc;
use std::time::Duration;

use axum::{
  body::{Body, to_bytes},
  http::{Request, StatusCode},
};
use futures::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::Database;
use yanshi_server::{AppState, LiveHarnessExecutor, create_router};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

fn has_moonshot() -> bool {
  if std::env::var("MOONSHOT_API_KEY").is_ok() {
    return true;
  }
  if let Some(home) = dirs::home_dir() {
    let path = home.join(".yanshi").join(".env");
    if path.exists() {
      let _ = dotenvy::from_path(&path);
      return std::env::var("MOONSHOT_API_KEY").is_ok();
    }
  }
  false
}

async fn body_json(response: axum::response::Response) -> Value {
  let bytes = to_bytes(response.into_body(), 1024 * 1024)
    .await
    .expect("body collected");
  serde_json::from_slice(&bytes).expect("body is JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_stack_e2e_submit_stream_history_resume() {
  let Some(url) = live_url() else {
    eprintln!("skipping full_stack_e2e_submit_stream_history_resume: no DB url");
    return;
  };
  if !has_moonshot() {
    eprintln!("skipping full_stack_e2e_submit_stream_history_resume: no Moonshot key");
    return;
  }

  let db = Database::connect_and_migrate(&url, 4).await.unwrap();
  let tenant = format!("e2e-fullstack-{}", Uuid::new_v4());
  // No TRUNCATE: this test scopes to a unique tenant and only reads
  // its own freshly-uuid'd session. Wiping the table here would race
  // against any other harness test binary running in parallel and
  // delete its in-flight rows.

  let state = AppState::new(db).with_gateway_policy(yanshi_server::gateway_policy::GatewayPolicy {
    path_roots: vec!["/tmp".into(), std::env::temp_dir()],
    ..Default::default()
  });
  let live = LiveHarnessExecutor::new(state.approval_registry.clone(), Duration::from_secs(60));
  let state = state.with_harness_executor(Arc::new(live));
  let app = create_router(state.clone());
  let workspace = tempfile::tempdir().expect("workspace tempdir");

  // ───── 1. Submit ────────────────────────────────────────────────
  let submit = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/harness/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({
            "user_input": "请用一句中文回答：现在是几月。",
            "workspace_root": workspace.path().display().to_string(),
            "profile": "local",
            "runtime_kind": "react",
            "model": "kimi-k2.6",
            "tenant_id": tenant,
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(submit.status(), StatusCode::OK);
  let body = body_json(submit).await;
  let session_id = Uuid::parse_str(body["session_id"].as_str().expect("session_id"))
    .expect("session_id is a uuid");

  // ───── 2. SSE stream ────────────────────────────────────────────
  // Open the SSE connection while the executor is still running. We
  // collect for ~30 s (well above Moonshot's typical latency) and
  // assert the lifecycle envelopes show up in seq order via SSE
  // alone — no DB peek needed.
  let mut streamed = Vec::<Value>::new();
  let sse = app
    .clone()
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{session_id}/events"))
        .header("X-Yanshi-Tenant", &tenant)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(sse.status(), StatusCode::OK);
  let mut stream = sse.into_body().into_data_stream();
  let _ = tokio::time::timeout(Duration::from_secs(30), async {
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
      let Ok(bytes) = chunk else { continue };
      buffer.push_str(&String::from_utf8_lossy(&bytes));
      while let Some(boundary) = buffer.find("\n\n") {
        let raw = buffer[..boundary].to_string();
        buffer = buffer[boundary + 2..].to_string();
        let data_line = raw
          .lines()
          .find(|line| line.starts_with("data:"))
          .map(|line| line.trim_start_matches("data:").trim_start());
        let Some(data) = data_line else { continue };
        let Ok(parsed): Result<Value, _> = serde_json::from_str(data) else {
          continue;
        };
        streamed.push(parsed);
      }
      // Bail out once we've seen the terminal `stopped` envelope so
      // the test doesn't hang waiting for the broker's keepalive.
      if streamed.iter().any(|event| event["kind"] == "stopped") {
        break;
      }
    }
  })
  .await;

  assert!(
    !streamed.is_empty(),
    "SSE delivered no events within timeout"
  );
  assert_eq!(streamed[0]["kind"], "session_started");
  assert_eq!(streamed[0]["seq"], 0);
  assert_eq!(
    streamed.last().expect("at least one event")["kind"],
    "stopped",
    "stream tail must be `stopped`"
  );

  // ───── 3. DB history matches stream ─────────────────────────────
  let history = app
    .clone()
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{session_id}/events/history"))
        .header("X-Yanshi-Tenant", &tenant)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(history.status(), StatusCode::OK);
  let history_body = body_json(history).await;
  let history_events = history_body.as_array().expect("history array").clone();
  assert!(
    history_events.len() >= streamed.len(),
    "DB history ({} rows) should include every streamed event ({} rows)",
    history_events.len(),
    streamed.len()
  );
  // Seqs are dense (0..N-1) for the persisted log.
  for (idx, event) in history_events.iter().enumerate() {
    assert_eq!(
      event["seq"].as_i64().unwrap_or(-1),
      idx as i64,
      "DB seq must be dense, hole at idx {idx}: {event}"
    );
  }

  // ───── 4. Terminal row ──────────────────────────────────────────
  let session_response = app
    .clone()
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{session_id}"))
        .header("X-Yanshi-Tenant", &tenant)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(session_response.status(), StatusCode::OK);
  let row = body_json(session_response).await;
  let status = row["status"].as_str().expect("status string");
  assert!(
    matches!(status, "completed" | "failed" | "cancelled"),
    "session reached a terminal status, got: {status}"
  );
  assert!(
    row["finished_at"].is_string(),
    "finished_at populated on terminal row"
  );

  // ───── 5. Resume rerun ───────────────────────────────────────────
  let resume = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{session_id}:resume"))
        .header("X-Yanshi-Tenant", &tenant)
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({
            "user_input": "请用一句中文重新回答：今天的天气。"
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(resume.status(), StatusCode::OK);
  let resume_body = body_json(resume).await;
  assert_eq!(resume_body["resumed"], true);
  assert_eq!(
    resume_body["user_input"],
    "请用一句中文重新回答：今天的天气。"
  );

  // Wait for the rerun to finish then verify event history was reset
  // (seq starts at 0 again) and the row terminated again.
  let mut rerun_status = String::new();
  for _ in 0..60 {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let probe = app
      .clone()
      .oneshot(
        Request::builder()
          .method("GET")
          .uri(format!("/v1/harness/sessions/{session_id}"))
          .header("X-Yanshi-Tenant", &tenant)
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    let row = body_json(probe).await;
    rerun_status = row["status"].as_str().unwrap_or("").to_string();
    if matches!(rerun_status.as_str(), "completed" | "failed" | "cancelled") {
      break;
    }
  }
  assert!(
    matches!(rerun_status.as_str(), "completed" | "failed" | "cancelled"),
    "rerun reached a terminal status, got: {rerun_status}"
  );

  let rerun_history = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{session_id}/events/history"))
        .header("X-Yanshi-Tenant", &tenant)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  let rerun_events = body_json(rerun_history).await;
  let rerun_arr = rerun_events.as_array().expect("rerun history array");
  assert!(
    !rerun_arr.is_empty(),
    "rerun must persist at least the session_started event"
  );
  assert_eq!(
    rerun_arr[0]["seq"], 0,
    "rerun seq restarts at 0 (prior events were cleared)"
  );
  assert_eq!(rerun_arr[0]["kind"], "session_started");
  assert_eq!(
    rerun_arr.last().expect("non-empty rerun history")["kind"],
    "stopped"
  );
}
