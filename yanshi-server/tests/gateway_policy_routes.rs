//! Eval 2026-10-07 S1/S2/S7: a tenant must not be able to widen the
//! gateway's sandbox through the request body. Every check fires before
//! the first DB access, so these run against a lazy (never-connected)
//! pool and need no `YANSHI_DATABASE_TEST_URL`.

use axum::{
  body::Body,
  http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use yanshi_db::Database;
use yanshi_server::{AppState, create_router, gateway_policy::GatewayPolicy};

fn lazy_state() -> AppState {
  let pool = PgPoolOptions::new()
    .connect_lazy("postgres://postgres:postgres@localhost:5432/yanshi_test")
    .unwrap();
  AppState::new(Database {
    pool,
    read_pool: None,
  })
}

async fn post(state: AppState, uri: &str, body: Value) -> (StatusCode, String) {
  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-yanshi-tenant", "t1")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
    .await
    .unwrap();
  (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn default_gateway_rejects_tenant_supplied_shell_and_file() {
  for workflow in [
    "name: w\nnodes:\n  - id: n\n    type: shell\n    parameters:\n      command: env\n      allowed_commands: [env]\n      allowed_paths: [/]\n",
    "name: w\nnodes:\n  - id: n\n    type: file\n    parameters:\n      operation: read\n      path: /proc/self/environ\n      allowed_paths: [/]\n",
  ] {
    let (status, body) = post(lazy_state(), "/v1/runs", json!({"workflow": workflow})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("disabled on this gateway"), "{body}");
  }
}

#[tokio::test]
async fn operator_ceiling_caps_shell_commands_and_paths() {
  let state = lazy_state().with_gateway_policy(GatewayPolicy {
    host_node_types: ["shell".to_string()].into(),
    allowed_commands: ["git".to_string()].into(),
    path_roots: vec!["/srv/work".into()],
    ..Default::default()
  });
  let workflow = "name: w\nnodes:\n  - id: n\n    type: shell\n    parameters:\n      command: git status\n      allowed_commands: [git]\n      allowed_paths: [/proc/self]\n";
  let (status, body) = post(state, "/v1/runs", json!({"workflow": workflow})).await;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
  assert!(
    body.contains("outside the gateway's allowed roots"),
    "{body}"
  );
}

#[tokio::test]
async fn harness_workspace_root_must_sit_under_a_gateway_root() {
  let (status, body) = post(
    lazy_state(),
    "/v1/harness/sessions",
    json!({"user_input": "read /proc/self/environ", "workspace_root": "/"}),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
  assert!(body.contains("workspace_root"), "{body}");
}
