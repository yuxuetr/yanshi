//! P-H.5 slice 2 — live (LLM-backed) executor E2E.
//!
//! Drives a complete `POST /v1/harness/sessions` → real `HarnessRuntime`
//! → `ReActAgent` → Moonshot Kimi → JSON history round-trip against a
//! local Postgres instance and the user's Moonshot API key.
//!
//! Skips automatically when either of the two prerequisites is
//! missing:
//!   - `YANSHI_DATABASE_TEST_URL` (a Postgres URL the test owns).
//!   - `MOONSHOT_API_KEY` (loaded by `Yanshi::init` from
//!     `~/.yanshi/.env`; the test forwards it via env).
//!
//! Without both vars present the suite stays hermetic so workspace
//! `cargo test` runs cleanly on machines without LLM credentials.

use std::sync::Arc;
use std::time::Duration;

use axum::{
  body::{Body, to_bytes},
  http::{Request, StatusCode},
};
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
  // Fall back to ~/.yanshi/.env which Yanshi::init also reads.
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
async fn live_executor_runs_moonshot_session_end_to_end() {
  let Some(url) = live_url() else {
    eprintln!("skipping live_executor_runs_moonshot_session_end_to_end: no DB url");
    return;
  };
  if !has_moonshot() {
    eprintln!("skipping live_executor_runs_moonshot_session_end_to_end: no Moonshot key");
    return;
  }

  let db = Database::connect_and_migrate(&url, 4).await.unwrap();
  // No TRUNCATE: this live-executor test scopes to a unique tenant
  // (set below) and only reads back its own session id. Wiping the
  // table here used to race other parallel test binaries.

  let state = AppState::new(db).with_gateway_policy(yanshi_server::gateway_policy::GatewayPolicy {
    path_roots: vec!["/tmp".into(), std::env::temp_dir()],
    ..Default::default()
  });
  let live = LiveHarnessExecutor::new(state.approval_registry.clone(), Duration::from_secs(60));
  let state = state.with_harness_executor(Arc::new(live));
  let app = create_router(state.clone());

  let workspace = tempfile::tempdir().expect("workspace tempdir");

  // Submit the session against the live executor.
  let tenant = format!("live-{}", Uuid::new_v4());
  let response = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/harness/sessions")
        .header("X-Yanshi-Tenant", &tenant)
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({
            "user_input": "回复一个非常简短的中文问候。",
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
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  let session_id = Uuid::parse_str(body["session_id"].as_str().expect("session_id"))
    .expect("session_id is a uuid");

  // Poll the GET endpoint until the row is terminal (~30s ceiling so
  // tests don't hang forever on slow networks). The real executor
  // hits Moonshot synchronously and finishes well inside this window.
  let mut terminal: Option<Value> = None;
  for _ in 0..60 {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let get = app
      .clone()
      .oneshot(
        Request::builder()
          .method("GET")
          .uri(format!("/v1/harness/sessions/{}", session_id))
          .header("X-Yanshi-Tenant", &tenant)
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    let payload = body_json(get).await;
    let status = payload["status"].as_str().unwrap_or("").to_string();
    if status != "running" {
      terminal = Some(payload);
      break;
    }
  }
  let final_row = terminal.expect("session reached a terminal status within timeout");
  let status = final_row["status"].as_str().expect("status string");
  assert!(
    matches!(status, "completed" | "failed" | "cancelled"),
    "unexpected terminal status: {status} (row: {final_row})"
  );
  // We don't assert success outright — the Moonshot API could
  // legitimately return a rate-limit / quota error. The contract under
  // test is the round-trip: row created → executor ran → row reached
  // a terminal status with a populated `finished_at`.
  assert!(
    final_row["finished_at"].is_string(),
    "finished_at populated on terminal state"
  );

  // Fetch the persisted event history. We expect at minimum a
  // `session_started` + `stopped` envelope; intermediate step events
  // are bonus depending on what the LLM chose to do.
  let history = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!(
          "/v1/harness/sessions/{}/events/history",
          session_id
        ))
        .header("X-Yanshi-Tenant", &tenant)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(history.status(), StatusCode::OK);
  let events = body_json(history).await;
  let events = events.as_array().expect("history is array");
  assert!(
    events.len() >= 2,
    "expected ≥2 envelope rows, got {}: {events:?}",
    events.len()
  );
  let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
  assert!(
    kinds.contains(&"session_started"),
    "missing session_started in {kinds:?}"
  );
  assert!(kinds.contains(&"stopped"), "missing stopped in {kinds:?}");
}
