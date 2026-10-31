//! A harness session runs on its own current-thread runtime (the agent
//! future is not `Send`). sqlx pools must not cross runtimes: connections
//! the inner runtime used never made it back to the shared pool, so the
//! server's pool drained one session at a time until a status write hit
//! `PoolTimedOut` and the session stayed `running` forever.
//!
//! Runs several mock-LLM sessions back to back against a deliberately
//! tiny shared pool; every one must reach a terminal status.
//!
//! Requires Postgres via `YANSHI_DATABASE_TEST_URL` (self-skips without).
//! SAFETY: mutates process-wide env vars read once by `Yanshi::init()`;
//! this is the only test in this binary.

use std::sync::Arc;
use std::time::Duration;

use axum::{
  body::{Body, to_bytes},
  http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::Database;
use yanshi_server::{AppState, LiveHarnessExecutor, create_router};

async fn body_json(response: axum::response::Response) -> Value {
  let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
  serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_do_not_drain_the_shared_pool() {
  let Some(url) = std::env::var("YANSHI_DATABASE_TEST_URL").ok() else {
    eprintln!("skipping sessions_do_not_drain_the_shared_pool: no DB url");
    return;
  };
  Database::connect_and_migrate(&url, 2).await.unwrap();
  let pool = PgPoolOptions::new()
    .max_connections(2)
    .acquire_timeout(Duration::from_secs(3))
    .connect(&url)
    .await
    .unwrap();
  let db = Database {
    pool,
    read_pool: None,
  };

  let tmp = tempfile::tempdir().unwrap();
  let models_config = tmp.path().join("models.yml");
  std::fs::write(
    &models_config,
    "models:\n  mock-chat: { vendor: mock, type: text, model_id: mock-chat }\n\
     providers:\n  mock: { api_key_env: MOCK_API_KEY }\n",
  )
  .unwrap();
  // SAFETY: see the file-level doc comment.
  unsafe {
    std::env::set_var("YANSHI_MODELS_CONFIG", &models_config);
    std::env::set_var("MOCK_API_KEY", "x");
    std::env::set_var(
      "YANSHI_MOCK_RESPONSE",
      r#"{"thought":"done","answer":"hello"}"#,
    );
  }

  let state = AppState::new(db).with_gateway_policy(yanshi_server::gateway_policy::GatewayPolicy {
    path_roots: vec!["/tmp".into(), std::env::temp_dir()],
    ..Default::default()
  });
  let live = LiveHarnessExecutor::new(state.approval_registry.clone(), Duration::from_secs(60));
  let app = create_router(state.with_harness_executor(Arc::new(live)));
  let workspace = tempfile::tempdir().unwrap();
  let tenant = format!("pool-{}", Uuid::new_v4());

  for round in 0..4 {
    let response = app
      .clone()
      .oneshot(
        Request::builder()
          .method("POST")
          .uri("/v1/harness/sessions")
          .header("content-type", "application/json")
          .header("X-Yanshi-Tenant", &tenant)
          .body(Body::from(
            json!({
              "user_input": "say hello",
              "workspace_root": workspace.path().display().to_string(),
              "profile": "local",
              "runtime_kind": "react",
              "model": "mock-chat",
            })
            .to_string(),
          ))
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "round {round}");
    let session_id = body_json(response).await["session_id"]
      .as_str()
      .unwrap()
      .to_string();

    let mut status = String::new();
    for _ in 0..150 {
      tokio::time::sleep(Duration::from_millis(100)).await;
      let get = app
        .clone()
        .oneshot(
          Request::builder()
            .uri(format!("/v1/harness/sessions/{session_id}"))
            .header("X-Yanshi-Tenant", &tenant)
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
      if get.status() != StatusCode::OK {
        continue;
      }
      status = body_json(get).await["status"]
        .as_str()
        .unwrap_or_default()
        .to_string();
      if status != "running" && status != "queued" {
        break;
      }
    }
    assert_eq!(status, "completed", "round {round}: session {session_id}");
  }
}
