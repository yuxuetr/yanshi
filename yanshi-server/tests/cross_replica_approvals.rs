//! W4.2e — cross-replica approval decisions via Postgres NOTIFY.
//!
//! `PendingApprovalRegistry` parks a `oneshot::Sender` in whichever
//! process's executor task is blocked awaiting a decision. Before this,
//! `POST .../approvals/{id}` and `GET .../approvals` landing on a
//! *different* gateway replica than the one holding the entry would
//! 404 / silently return an empty list even though the approval
//! genuinely existed. This test builds two independent `AppState`s
//! against the same Postgres database (same fixture shape as
//! `cross_replica_cancellation.rs`), parks a request on replica A
//! (standing in for "the process actually running this session"),
//! persists the `approval_requested` event exactly as the real
//! `HookedTool` pipeline would, and decides/lists it via replica B —
//! proving both the decide-404 bug and the list-silently-empty bug are
//! closed.
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
use yanshi_db::{Database, HarnessSessionRepo, NewHarnessSession, NewHarnessSessionEvent};
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
    params_summary: json!({"cmd": "ls"}),
    risk: ApprovalRisk::Medium,
    reason: "cross-replica test".into(),
    requested_at: chrono::Utc::now(),
    expires_at: None,
  }
}

/// Persist the `approval_requested` event exactly as `HookedTool`
/// (`yanshi-harness/src/hooks_runtime.rs`) would, ahead of parking
/// the request on the provider — this is what makes the request
/// visible to a non-owning replica's DB-derived pending set.
async fn persist_approval_requested(state: &AppState, request: &ApprovalRequest) {
  use yanshi_db::HarnessEventRepo;
  let session_id = Uuid::parse_str(&request.session_id).expect("valid session id");
  let payload = serde_json::to_value(HarnessEventBody::ApprovalRequested(
    ApprovalRequestedPayload {
      request: request.clone(),
    },
  ))
  .expect("serializable");
  state
    .repos
    .harness_events
    .append(NewHarnessSessionEvent {
      session_id,
      seq: 1,
      kind: "approval_requested".to_string(),
      payload,
    })
    .await
    .expect("approval_requested event persisted");
}

/// Park a request on `state` via the real [`ServerApprovalProvider`]
/// path (not the private `park()` helper) and return the join handle
/// awaiting the eventual decision, plus wait for it to actually be
/// registered before returning.
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
async fn decide_via_replica_b_resolves_the_oneshot_parked_on_replica_a() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!("skipping decide_via_replica_b_resolves_the_oneshot_parked_on_replica_a");
    return;
  };

  let session_id = insert_running_session(&state_a, "default").await;
  let request = sample_request(session_id, "req-cross-decide");
  persist_approval_requested(&state_a, &request).await;
  let handle = park_on_replica(&state_a, request).await;

  // Decide through replica B's own HTTP route — replica B has no local
  // registry entry for this request at all.
  let app_b = create_router(state_b.clone());
  let response = app_b
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!(
          "/v1/harness/sessions/{session_id}/approvals/req-cross-decide"
        ))
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({
            "decision": "allow",
            "scope": "once",
            "decided_by": "operator-b"
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

  // The 200 alone proves nothing about replica A's parked future (that's
  // exactly the bug this closes) — the real assertion is that it
  // actually resolves via the NOTIFY-driven listener.
  let decided = tokio::time::timeout(Duration::from_secs(5), handle)
    .await
    .expect("replica A's parked oneshot must resolve within 5s")
    .expect("join succeeds")
    .expect("provider returns a decision");
  assert!(matches!(decided.decision, ApprovalOutcome::Allow));
  assert!(matches!(decided.scope, ApprovalScope::Once));
  assert_eq!(decided.decided_by, "operator-b");
}

#[tokio::test]
async fn list_pending_via_replica_b_surfaces_a_request_parked_only_on_replica_a() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!("skipping list_pending_via_replica_b_surfaces_a_request_parked_only_on_replica_a");
    return;
  };

  let session_id = insert_running_session(&state_a, "default").await;
  let request = sample_request(session_id, "req-cross-list");
  persist_approval_requested(&state_a, &request).await;
  let handle = park_on_replica(&state_a, request).await;

  // Replica B's own in-memory registry has zero entries for this
  // session — the list must be derived from the DB-persisted event.
  assert_eq!(state_b.approval_registry.pending_count(), 0);

  let app_b = create_router(state_b.clone());
  let response = app_b
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{session_id}/approvals"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  let pending = body["approvals"].as_array().expect("approvals is array");
  assert_eq!(pending.len(), 1);
  assert_eq!(pending[0]["id"], "req-cross-list");

  // Clean up the spawned provider task via replica A directly so the
  // test doesn't leak it.
  state_a
    .approval_registry
    .decide(
      &session_id.to_string(),
      "req-cross-list",
      yanshi_harness::ApprovalDecision {
        request_id: "req-cross-list".to_string(),
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

/// Regression: deciding a request that genuinely doesn't exist anywhere
/// (no local park, no DB event) must still 404 — the DB fallback isn't
/// a blanket "always succeed" escape hatch.
#[tokio::test]
async fn decide_on_nonexistent_request_still_404s_via_either_replica() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!("skipping decide_on_nonexistent_request_still_404s_via_either_replica");
    return;
  };

  let session_id = insert_running_session(&state_a, "default").await;

  let app_b = create_router(state_b.clone());
  let response = app_b
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{session_id}/approvals/nope"))
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

/// Eval 2026-10-07 P3: a decision bigger than Postgres's ~8 KB NOTIFY cap
/// used to make `pg_notify` fail, so the parked request on the other
/// replica never resolved. NOTIFY now carries only the ids.
#[tokio::test]
async fn oversized_decision_still_reaches_the_owning_replica() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!("skipping oversized_decision_still_reaches_the_owning_replica");
    return;
  };
  let session_id = insert_running_session(&state_a, "default").await;
  let request = sample_request(session_id, "req-oversized");
  persist_approval_requested(&state_a, &request).await;
  let handle = park_on_replica(&state_a, request).await;

  let decided_by = format!("operator-{}", "b".repeat(9_000));
  let response = create_router(state_b.clone())
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!(
          "/v1/harness/sessions/{session_id}/approvals/req-oversized"
        ))
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({
            "decision": "allow",
            "scope": "once",
            "decided_by": decided_by,
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let decided = tokio::time::timeout(Duration::from_secs(5), handle)
    .await
    .expect("replica A's parked request must resolve despite the 8 KB NOTIFY cap")
    .unwrap()
    .unwrap();
  assert_eq!(decided.decided_by, decided_by);
}
