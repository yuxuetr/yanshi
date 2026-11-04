//! W4.2e — cross-replica approval decisions for the run-scoped mirror
//! routes (`GET|POST /v1/runs/{id}/approvals...`, W4.1b).
//!
//! Same bug, same fix, same fixture shape as
//! `cross_replica_approvals.rs` (which covers the harness-session
//! routes) — kept as a separate file because the run-scoped routes are
//! keyed by `runs.id` / the `events` table rather than
//! `harness_sessions.id` / `harness_session_events`, so the DB seeding
//! differs even though `decide_run_approval` and `list_run_approvals`
//! share the exact same `PendingApprovalRegistry` and
//! `record_decision_intent_and_notify` helper as their harness-session
//! counterparts.
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`. Without
//! it the tests self-skip, matching every other server e2e test file.

use axum::{
  body::{Body, to_bytes},
  http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::{Database, EventRepo, NewEvent, NewRun, RunRepo, RunStatus};
use yanshi_harness::{
  ApprovalOutcome, ApprovalProvider, ApprovalRequest, ApprovalRequestedPayload, ApprovalRisk,
  ApprovalScope, HarnessEventBody,
};
use yanshi_server::{AppState, create_router, spawn_approval_decision_listener};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn two_replica_states() -> Option<(AppState, AppState)> {
  let url = live_url()?;
  let db_a = Database::connect_and_migrate(&url, 4).await.ok()?;
  // No TRUNCATE: tests run in parallel and each uses fresh ids, so a
  // global reset only races sibling tests.
  let db_b = Database::connect(&url, 4).await.ok()?;

  let state_a = AppState::new(db_a.clone());
  let state_b = AppState::new(db_b.clone());

  spawn_approval_decision_listener(db_a.pool.clone(), state_a.approval_registry.clone());
  spawn_approval_decision_listener(db_b.pool.clone(), state_b.approval_registry.clone());

  Some((state_a, state_b))
}

async fn body_json(response: axum::response::Response) -> Value {
  let bytes = to_bytes(response.into_body(), 1024 * 1024)
    .await
    .expect("body collected");
  serde_json::from_slice(&bytes).expect("body is JSON")
}

async fn insert_running_run(state: &AppState, tenant: &str) -> Uuid {
  let id = Uuid::new_v4();
  state
    .repos
    .runs
    .create(NewRun {
      id,
      workflow: "@skill:test".to_string(),
      status: RunStatus::Running,
      run_dir: None,
      tenant_id: tenant.to_string(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .expect("run row created");
  id
}

fn sample_request(run_id: Uuid, request_id: &str) -> ApprovalRequest {
  ApprovalRequest {
    id: request_id.to_string(),
    session_id: run_id.to_string(),
    step_index: 1,
    tool: "shell".into(),
    source: None,
    permissions: Vec::new(),
    idempotency: Default::default(),
    effect: Default::default(),
    params_summary: json!({"cmd": "ls"}),
    risk: ApprovalRisk::Medium,
    reason: "cross-replica run test".into(),
    requested_at: chrono::Utc::now(),
    expires_at: None,
  }
}

/// Persist the `approval_requested` event exactly as `RunHarnessEventSink`
/// (`yanshi-server/src/runs.rs`) would, ahead of parking the request
/// on the provider.
async fn persist_approval_requested(state: &AppState, tenant: &str, request: &ApprovalRequest) {
  let run_id = Uuid::parse_str(&request.session_id).expect("valid run id");
  let payload = serde_json::to_value(HarnessEventBody::ApprovalRequested(
    ApprovalRequestedPayload {
      request: request.clone(),
    },
  ))
  .expect("serializable");
  state
    .repos
    .events
    .append(NewEvent {
      run_id,
      seq: 1,
      kind: "approval_requested".to_string(),
      payload,
      tenant_id: Some(tenant.to_string()),
    })
    .await
    .expect("approval_requested event persisted");
}

async fn park_on_replica(
  state: &AppState,
  request: ApprovalRequest,
) -> tokio::task::JoinHandle<Result<yanshi_harness::ApprovalDecision, yanshi_harness::HarnessError>>
{
  let provider = Arc::new(yanshi_server::ServerApprovalProvider::new(
    state.approval_registry.clone(),
    Duration::from_secs(60),
  ));
  let handle = tokio::spawn(async move { provider.request(request).await });
  while state.approval_registry.pending_count() == 0 {
    tokio::task::yield_now().await;
  }
  handle
}

#[tokio::test]
async fn decide_run_approval_via_replica_b_resolves_the_oneshot_parked_on_replica_a() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!(
      "skipping decide_run_approval_via_replica_b_resolves_the_oneshot_parked_on_replica_a"
    );
    return;
  };

  let run_id = insert_running_run(&state_a, "default").await;
  let request = sample_request(run_id, "req-run-cross-decide");
  persist_approval_requested(&state_a, "default", &request).await;
  let handle = park_on_replica(&state_a, request).await;

  let app_b = create_router(state_b.clone());
  let response = app_b
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/runs/{run_id}/approvals/req-run-cross-decide"))
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({"decision": "allow", "scope": "once"})).unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  assert_eq!(body["resolved"], true);

  let decided = tokio::time::timeout(Duration::from_secs(5), handle)
    .await
    .expect("replica A's parked oneshot must resolve within 5s")
    .expect("join succeeds")
    .expect("provider returns a decision");
  assert!(matches!(decided.decision, ApprovalOutcome::Allow));
}

#[tokio::test]
async fn list_run_approvals_via_replica_b_surfaces_a_request_parked_only_on_replica_a() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!(
      "skipping list_run_approvals_via_replica_b_surfaces_a_request_parked_only_on_replica_a"
    );
    return;
  };

  let run_id = insert_running_run(&state_a, "default").await;
  let request = sample_request(run_id, "req-run-cross-list");
  persist_approval_requested(&state_a, "default", &request).await;
  let handle = park_on_replica(&state_a, request).await;

  assert_eq!(state_b.approval_registry.pending_count(), 0);

  let app_b = create_router(state_b.clone());
  let response = app_b
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/runs/{run_id}/approvals"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  let pending = body["approvals"].as_array().expect("approvals is array");
  assert_eq!(pending.len(), 1);
  assert_eq!(pending[0]["id"], "req-run-cross-list");

  state_a
    .approval_registry
    .decide(
      &run_id.to_string(),
      "req-run-cross-list",
      yanshi_harness::ApprovalDecision {
        request_id: "req-run-cross-list".to_string(),
        decision: ApprovalOutcome::Allow,
        scope: ApprovalScope::Once,
        decided_by: "test-cleanup".to_string(),
        decided_at: chrono::Utc::now(),
        reason: None,
      },
    )
    .expect("still parked on replica A");
  let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}
