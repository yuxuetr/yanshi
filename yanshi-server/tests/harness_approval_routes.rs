//! P-H.5 slice 2 — approval route integration tests.
//!
//! Exercises `GET /v1/harness/sessions/{id}/approvals` and
//! `POST /v1/harness/sessions/{id}/approvals/{request_id}` end-to-end
//! against a real Postgres instance. The tests park a synthetic
//! approval request via [`PendingApprovalRegistry`] directly, then
//! drive the lifecycle through the HTTP surface — keeping the cases
//! hermetic from any LLM provider.
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`.
//! Without it the tests self-skip.

use axum::{
  body::{Body, to_bytes},
  http::{Request, StatusCode},
};
use chrono::Utc;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::{Database, HarnessSessionRepo, NewHarnessSession};
use yanshi_harness::{
  ApprovalOutcome, ApprovalProvider, ApprovalRequest, ApprovalRisk, ApprovalScope,
};
use yanshi_server::{AppState, ServerApprovalProvider, create_router};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn fresh_state() -> Option<AppState> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  // Intentionally no TRUNCATE — tests below seed sessions with fresh
  // uuids per case. See `yanshi-db/tests/repositories.rs::fresh_db`
  // for the parallel-cargo-test rationale.
  Some(AppState::new(db))
}

async fn body_json(response: axum::response::Response) -> Value {
  let bytes = to_bytes(response.into_body(), 1024 * 1024)
    .await
    .expect("body collected");
  serde_json::from_slice(&bytes).expect("body is JSON")
}

async fn insert_running_session(state: &AppState, tenant: &str) -> Uuid {
  let id = Uuid::new_v4();
  state
    .repos
    .harness_sessions
    .create(NewHarnessSession {
      id,
      tenant_id: tenant.to_string(),
      user_input: "approval test".into(),
      workspace_root: "/tmp".into(),
      profile: "local".into(),
      runtime_kind: "react".into(),
      model: "stub".into(),
      skill_name: None,
    })
    .await
    .expect("session inserted");
  id
}

fn sample_request(session_id: Uuid, request_id: &str) -> ApprovalRequest {
  ApprovalRequest {
    id: request_id.to_string(),
    session_id: session_id.to_string(),
    step_index: 1,
    tool: "shell".into(),
    source: None,
    permissions: Vec::new(),
    idempotency: Default::default(),
    effect: Default::default(),
    params_summary: json!({"cmd": "ls"}),
    risk: ApprovalRisk::Medium,
    reason: "integration test".into(),
    requested_at: Utc::now(),
    expires_at: None,
  }
}

#[tokio::test]
async fn list_pending_returns_404_for_unknown_session() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping list_pending_returns_404_for_unknown_session");
    return;
  };
  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{}/approvals", Uuid::new_v4()))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_pending_returns_parked_request() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping list_pending_returns_parked_request");
    return;
  };
  let session_id = insert_running_session(&state, "default").await;

  // Park a request synthetically via the provider so we can drive the
  // route without standing up the LLM-backed executor. The HTTP layer
  // is the surface under test.
  let provider = Arc::new(ServerApprovalProvider::new(
    state.approval_registry.clone(),
    Duration::from_secs(60),
  ));
  let provider_clone = provider.clone();
  let request = sample_request(session_id, "req-list");
  let handle = tokio::spawn(async move { provider_clone.request(request).await });

  // Wait until the request is parked before issuing the GET. Without
  // this the route can race the provider's `park()` call.
  while state.approval_registry.pending_count() == 0 {
    tokio::task::yield_now().await;
  }

  let response = create_router(state.clone())
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{}/approvals", session_id))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  let pending = body["approvals"].as_array().expect("approvals is array");
  assert_eq!(pending.len(), 1);
  assert_eq!(pending[0]["id"], "req-list");
  assert_eq!(pending[0]["tool"], "shell");

  // Decide it from the route side so the spawned provider future
  // resolves and the test doesn't leak the join handle.
  let decide_response = create_router(state.clone())
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!(
          "/v1/harness/sessions/{}/approvals/{}",
          session_id, "req-list"
        ))
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({"decision": "allow", "scope": "once"})).unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(decide_response.status(), StatusCode::OK);
  let decided = handle.await.unwrap().expect("provider returns decision");
  assert!(matches!(decided.decision, ApprovalOutcome::Allow));
  assert!(matches!(decided.scope, ApprovalScope::Once));
}

#[tokio::test]
async fn decide_unknown_request_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping decide_unknown_request_returns_404");
    return;
  };
  let session_id = insert_running_session(&state, "default").await;
  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!(
          "/v1/harness/sessions/{}/approvals/missing",
          session_id
        ))
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({"decision": "allow"})).unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deny_decision_resolves_provider_with_denied_outcome() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping deny_decision_resolves_provider_with_denied_outcome");
    return;
  };
  let session_id = insert_running_session(&state, "default").await;

  let provider = Arc::new(ServerApprovalProvider::new(
    state.approval_registry.clone(),
    Duration::from_secs(60),
  ));
  let provider_clone = provider.clone();
  let request = sample_request(session_id, "req-deny");
  let handle = tokio::spawn(async move { provider_clone.request(request).await });

  while state.approval_registry.pending_count() == 0 {
    tokio::task::yield_now().await;
  }

  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!(
          "/v1/harness/sessions/{}/approvals/{}",
          session_id, "req-deny"
        ))
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({
            "decision": "deny_and_stop",
            "scope": "session",
            "decided_by": "operator-x",
            "reason": "policy violation"
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  assert_eq!(body["resolved"], true);

  let decided = handle.await.unwrap().expect("provider resolves");
  assert!(matches!(decided.decision, ApprovalOutcome::DenyAndStop));
  assert!(matches!(decided.scope, ApprovalScope::Session));
  assert_eq!(decided.decided_by, "operator-x");
  assert_eq!(decided.reason.as_deref(), Some("policy violation"));
}

// ── P2.6 tenant boundary regression suite ────────────────────────────────
//
// ddc497c added the tenant check to both `list_pending_approvals` and
// `decide_approval`. Pin the contract so a regression flips these
// from 404 back to 200 / 422.

const TENANT_HEADER: &str = "x-yanshi-tenant";

async fn park_synthetic_approval(state: &AppState, session_id: Uuid, request_id: &str) {
  // Mirrors the pattern used by `list_pending_returns_parked_request`:
  // park via the provider's `request()` so we don't have to widen the
  // visibility of the private `park()` helper just for tests.
  let provider = Arc::new(ServerApprovalProvider::new(
    state.approval_registry.clone(),
    Duration::from_secs(60),
  ));
  let request = sample_request(session_id, request_id);
  tokio::spawn(async move {
    let _ = provider.request(request).await;
  });
  while state.approval_registry.pending_count() == 0 {
    tokio::task::yield_now().await;
  }
}

#[tokio::test]
async fn cross_tenant_list_pending_approvals_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping cross_tenant_list_pending_approvals_returns_404");
    return;
  };
  let owner = format!("tenant-owner-{}", Uuid::new_v4());
  let session_id = insert_running_session(&state, &owner).await;
  park_synthetic_approval(&state, session_id, "p-list").await;

  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{session_id}/approvals"))
        .header(
          TENANT_HEADER,
          format!("tenant-intruder-{}", Uuid::new_v4()).as_str(),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
  let body = body_json(response).await;
  assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn cross_tenant_decide_approval_returns_404_and_keeps_request_parked() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping cross_tenant_decide_approval_returns_404_and_keeps_request_parked");
    return;
  };
  let owner = format!("tenant-owner-{}", Uuid::new_v4());
  let session_id = insert_running_session(&state, &owner).await;
  let request_id = "p-decide";
  park_synthetic_approval(&state, session_id, request_id).await;
  let registry = state.approval_registry.clone();

  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!(
          "/v1/harness/sessions/{session_id}/approvals/{request_id}"
        ))
        .header(
          TENANT_HEADER,
          format!("tenant-intruder-{}", Uuid::new_v4()).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({ "decision": "allow", "scope": "once" })).unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
  // The parked request stays parked: a cross-tenant decide attempt
  // must not unblock the owner's pending approval. List under the
  // owner tenant still surfaces the one request.
  assert_eq!(registry.list(&session_id.to_string()).len(), 1);
}
