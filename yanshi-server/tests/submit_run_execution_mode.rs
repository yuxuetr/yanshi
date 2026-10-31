//! W4.3b-c — `POST /v1/runs`'s `execution_mode` opt-in wiring.
//!
//! Route-level coverage for the decision points `submit_run` makes
//! before a run row is ever created: `"distributed"` with no worker
//! control plane configured 400s cleanly; a workflow shape
//! `validate_distributed_flow` rejects 400s with the specific reason; an
//! unrecognized `execution_mode` value 400s rather than silently falling
//! back; and omitting the field entirely keeps today's in-process
//! behavior unchanged (regression guard against this whole feature).
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`. Without
//! it the tests self-skip, matching every other server e2e test file.

use axum::{
  body::{Body, to_bytes},
  http::{Request, StatusCode, header::CONTENT_TYPE},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use yanshi_db::Database;
use yanshi_server::scheduler::{
  AuthenticatedControlPlane, InMemoryWorkerProtocol, WorkerAdmissionPolicy, WorkerControlPlane,
};
use yanshi_server::{AppState, create_router};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn fresh_state() -> Option<AppState> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  Some(AppState::new(db))
}

fn with_control_plane(state: AppState) -> AppState {
  let plane = Arc::new(AuthenticatedControlPlane::new(
    WorkerControlPlane::new(InMemoryWorkerProtocol::new()),
    WorkerAdmissionPolicy::default(),
  ));
  state.with_worker_control_plane(Some(plane))
}

async fn submit(app_state: AppState, body: Value) -> (StatusCode, Value) {
  let response = create_router(app_state)
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/runs")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap(),
    )
    .await
    .unwrap();
  let status = response.status();
  let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
  let parsed: Value = serde_json::from_slice(&bytes).unwrap();
  (status, parsed)
}

const CLEAN_TEMPLATE_WORKFLOW: &str = r#"
name: distributed submit test
nodes:
  - id: n1
    type: template
    parameters:
      template: "hello"
"#;

#[tokio::test]
async fn distributed_without_a_control_plane_returns_400() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping distributed_without_a_control_plane_returns_400");
    return;
  };
  // No `.with_worker_control_plane(...)` — this deployment has none.
  let (status, body) = submit(
    state,
    json!({"workflow": CLEAN_TEMPLATE_WORKFLOW, "execution_mode": "distributed"}),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert!(
    body["error"]["message"]
      .as_str()
      .unwrap_or("")
      .contains("control plane"),
    "expected a control-plane-specific 400 message, got: {body}"
  );
}

#[tokio::test]
async fn distributed_with_a_declared_inputs_block_returns_400() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping distributed_with_a_declared_inputs_block_returns_400");
    return;
  };
  let state = with_control_plane(state);
  let workflow = "name: t\ninputs:\n  topic:\n    default: x\nnodes:\n  - id: n1\n    type: template\n    parameters:\n      template: \"{{ inputs.topic }}\"\n";
  let (status, body) = submit(
    state,
    json!({"workflow": workflow, "execution_mode": "distributed"}),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert!(
    body["error"]["message"]
      .as_str()
      .unwrap_or("")
      .contains("inputs:"),
    "expected the inputs:-block-specific validation reason, got: {body}"
  );
}

#[tokio::test]
async fn distributed_with_a_run_if_node_returns_400() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping distributed_with_a_run_if_node_returns_400");
    return;
  };
  let state = with_control_plane(state);
  let workflow = "name: t\nnodes:\n  - id: n1\n    type: template\n    run_if: \"true\"\n    parameters:\n      template: \"x\"\n";
  let (status, body) = submit(
    state,
    json!({"workflow": workflow, "execution_mode": "distributed"}),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert!(
    body["error"]["message"]
      .as_str()
      .unwrap_or("")
      .contains("run_if"),
    "expected the run_if-specific validation reason, got: {body}"
  );
}

#[tokio::test]
async fn distributed_with_an_unsupported_node_type_returns_400() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping distributed_with_an_unsupported_node_type_returns_400");
    return;
  };
  // Enable `shell` in the gateway policy so the request gets past it and
  // reaches the distributed validation this test is about.
  let state =
    with_control_plane(state).with_gateway_policy(yanshi_server::gateway_policy::GatewayPolicy {
      host_node_types: ["shell".to_string()].into(),
      allowed_commands: ["echo".to_string()].into(),
      path_roots: vec!["/tmp".into()],
    });
  // `shell` is a real, schema-known node type (so it passes
  // `parse_workflow_definition`'s validation and reaches
  // `validate_distributed_flow`) but isn't in the worker-supported
  // distributed allowlist.
  let workflow = "name: t\nnodes:\n  - id: n1\n    type: shell\n    parameters:\n      command: \"echo hi\"\n      allowed_commands: [\"echo\"]\n      allowed_paths: [\"/tmp\"]\n";
  let (status, body) = submit(
    state,
    json!({"workflow": workflow, "execution_mode": "distributed"}),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert!(
    body["error"]["message"]
      .as_str()
      .unwrap_or("")
      .contains("'shell'"),
    "expected the unsupported-node-type-specific validation reason, got: {body}"
  );
}

#[tokio::test]
async fn unrecognized_execution_mode_returns_400() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping unrecognized_execution_mode_returns_400");
    return;
  };
  let (status, body) = submit(
    state,
    json!({"workflow": CLEAN_TEMPLATE_WORKFLOW, "execution_mode": "quantum"}),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert!(
    body["error"]["message"]
      .as_str()
      .unwrap_or("")
      .contains("quantum"),
    "expected the unrecognized-execution_mode-specific message, got: {body}"
  );
}

#[tokio::test]
async fn omitting_execution_mode_keeps_the_existing_in_process_behavior() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping omitting_execution_mode_keeps_the_existing_in_process_behavior");
    return;
  };
  let (status, body) = submit(state, json!({"workflow": CLEAN_TEMPLATE_WORKFLOW})).await;
  assert_eq!(status, StatusCode::OK, "got: {body}");
  assert!(body["run_id"].is_string());
  assert_eq!(body["status"], "queued");
}

#[tokio::test]
async fn explicit_in_process_execution_mode_behaves_the_same_as_omitting_it() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping explicit_in_process_execution_mode_behaves_the_same_as_omitting_it");
    return;
  };
  let (status, body) = submit(
    state,
    json!({"workflow": CLEAN_TEMPLATE_WORKFLOW, "execution_mode": "in_process"}),
  )
  .await;
  assert_eq!(status, StatusCode::OK, "got: {body}");
  assert_eq!(body["status"], "queued");
}
