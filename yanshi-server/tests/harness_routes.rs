//! P-H.5 Harness Mode HTTP integration tests.
//!
//! Exercises the gateway's `/v1/harness/sessions*` surface end-to-end:
//! submit a session, read it back, list sessions for a tenant, cancel
//! one, and replay events via the JSON-history + SSE endpoints. The
//! stub executor finishes synchronously (~50 ms) so SSE subscribers
//! always receive the full lifecycle from DB backfill — no live broker
//! attach needed.
//!
//! Tests require Postgres pointed to by `YANSHI_DATABASE_TEST_URL`.
//! Without it they self-skip so workspace `cargo test` stays hermetic.

use axum::{
  body::{Body, to_bytes},
  http::{Request, StatusCode},
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::time::{Duration, timeout};
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::{Database, HarnessSessionStatus};
use yanshi_server::{AppState, create_router};

const TENANT_HEADER: &str = "x-yanshi-tenant";

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn fresh_state() -> Option<AppState> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  // Intentionally no TRUNCATE — every test below either operates on a
  // freshly-uuid'd session it just submitted, or scopes its listing to
  // a unique `format!("...-{}", Uuid::new_v4())` tenant (see
  // `list_sessions_returns_newest_first`). A global TRUNCATE here used
  // to wipe other concurrent test binaries' seeded rows mid-run and
  // caused flakes in PG-parallel CI. Matches the `yanshi-db/tests/
  // repositories.rs::fresh_db` rationale.
  Some(
    AppState::new(db).with_gateway_policy(yanshi_server::gateway_policy::GatewayPolicy {
      path_roots: vec!["/tmp".into(), std::env::temp_dir()],
      ..Default::default()
    }),
  )
}

async fn body_json(response: axum::response::Response) -> Value {
  let bytes = to_bytes(response.into_body(), 1024 * 1024)
    .await
    .expect("body collected");
  serde_json::from_slice(&bytes).expect("body is JSON")
}

async fn submit_basic_session(app: axum::Router, prompt: &str) -> Uuid {
  submit_for_tenant(app, prompt, "default").await
}

/// Variant that pins a specific tenant so list-ordering tests can isolate
/// themselves from parallel test inserts on the shared `default` tenant.
async fn submit_for_tenant(app: axum::Router, prompt: &str, tenant: &str) -> Uuid {
  // Q1.4.3: tenant comes from the header; body field is still allowed
  // (must match) for backwards-compat smoke.
  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/harness/sessions")
        .header("content-type", "application/json")
        .header(TENANT_HEADER, tenant)
        .body(Body::from(
          serde_json::to_vec(&json!({
            "user_input": prompt,
            "workspace_root": "/tmp",
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
  assert_eq!(body["status"], "running");
  let id_raw = body["session_id"].as_str().expect("session_id is string");
  Uuid::parse_str(id_raw).expect("session_id is a uuid")
}

#[tokio::test]
async fn submit_then_get_returns_stub_terminal_state() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping submit_then_get_returns_stub_terminal_state");
    return;
  };

  let app = create_router(state.clone());
  let id = submit_basic_session(app.clone(), "hello harness").await;

  // Give the stub executor a beat to finish (~50 ms inside the stub).
  tokio::time::sleep(Duration::from_millis(250)).await;

  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{id}"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  // Stub marks the session as `failed` with a known reason. The schema
  // is intentionally explicit so the future LLM-backed executor can be
  // dropped in without changing the route shape.
  assert_eq!(body["status"], HarnessSessionStatus::Failed.as_str());
  assert_eq!(body["workspace_root"], "/tmp");
  assert_eq!(body["profile"], "local");
  assert_eq!(body["runtime_kind"], "react");
  assert_eq!(body["error"], "executor_not_yet_wired");
}

#[tokio::test]
async fn submit_rejects_empty_user_input() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping submit_rejects_empty_user_input");
    return;
  };

  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/harness/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({
            "user_input": "  ",
            "workspace_root": "/tmp",
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// U1.3: `cost_limit_usd` is an optional field on the create-session
/// body — the CLI/API entry point for the `RuntimeLimits::cost_limit_usd`
/// runtime enforcement T1.1 wired into `ReActAgent`/`PlanExecuteAgent`.
/// The `StubHarnessExecutor` used in this test file doesn't run a real
/// agent loop, so this only proves the field parses and doesn't reject
/// session creation — the runtime enforcement itself is proven in
/// `yanshi-harness/tests/runtime_react_smoke.rs::
/// harness_runtime_stops_react_agent_when_cost_limit_usd_is_exceeded`.
#[tokio::test]
async fn submit_accepts_optional_cost_limit_usd_field() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping submit_accepts_optional_cost_limit_usd_field");
    return;
  };

  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/harness/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({
            "user_input": "budget-constrained request",
            "workspace_root": "/tmp",
            "cost_limit_usd": 0.5,
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  assert_eq!(body["status"], "running");
}

#[tokio::test]
async fn list_sessions_returns_newest_first() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping list_sessions_returns_newest_first");
    return;
  };

  // Pin both submissions to a unique tenant so parallel tests can't
  // race their own `default`-tenant inserts into our listing.
  let tenant = format!("list-ordering-{}", Uuid::new_v4());
  let app = create_router(state);
  let first = submit_for_tenant(app.clone(), "first", &tenant).await;
  // Small delay so started_at ordering is stable across the two rows.
  tokio::time::sleep(Duration::from_millis(10)).await;
  let second = submit_for_tenant(app.clone(), "second", &tenant).await;

  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri("/v1/harness/sessions")
        .header(TENANT_HEADER, tenant.as_str())
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  let sessions = body["sessions"].as_array().expect("sessions is array");
  assert_eq!(sessions.len(), 2, "tenant-scoped listing returns both rows");
  // Newest first — submitted-second should land at index 0.
  assert_eq!(sessions[0]["id"], second.to_string());
  assert_eq!(sessions[1]["id"], first.to_string());
}

#[tokio::test]
async fn cancel_running_session_transitions_to_cancelled() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping cancel_running_session_transitions_to_cancelled");
    return;
  };

  // Use a state with a no-op executor so the session stays "running"
  // long enough for the cancel request to observe the non-terminal
  // status. The default stub executor finishes in ~50 ms.
  use async_trait::async_trait;
  use yanshi_server::HarnessSessionExecutor;
  struct SleepyExecutor;
  #[async_trait]
  impl HarnessSessionExecutor for SleepyExecutor {
    async fn execute(&self, _ctx: yanshi_server::HarnessSessionContext) {
      // Hold the row in `running` indefinitely; the cancel route owns
      // the terminal status transition.
      tokio::time::sleep(Duration::from_secs(60)).await;
    }
  }

  let state = state.with_harness_executor(std::sync::Arc::new(SleepyExecutor));
  let app = create_router(state.clone());
  let id = submit_basic_session(app.clone(), "long-running").await;

  // Submit the cancel.
  let response = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{id}:cancel"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  assert_eq!(body["cancelled"], true);
  assert_eq!(body["status"], HarnessSessionStatus::Cancelled.as_str());

  // Second cancel returns `cancelled: false` since the row is terminal.
  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{id}:cancel"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  assert_eq!(body["cancelled"], false);
}

#[tokio::test]
async fn events_history_returns_full_session_log_from_db() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping events_history_returns_full_session_log_from_db");
    return;
  };

  let app = create_router(state);
  let id = submit_basic_session(app.clone(), "events history").await;
  tokio::time::sleep(Duration::from_millis(250)).await;

  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{id}/events/history"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  let events = body.as_array().expect("events history is array");
  assert_eq!(events.len(), 2, "stub executor emits exactly two events");
  assert_eq!(events[0]["kind"], "session_started");
  assert_eq!(events[1]["kind"], "stopped");
  assert_eq!(events[1]["payload"]["reason"], "executor_not_yet_wired");
}

#[tokio::test]
async fn events_sse_replays_persisted_lifecycle() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping events_sse_replays_persisted_lifecycle");
    return;
  };

  let app = create_router(state);
  let id = submit_basic_session(app.clone(), "sse replay").await;
  // Let the stub finish so SSE backfill has the full lifecycle ready.
  tokio::time::sleep(Duration::from_millis(250)).await;

  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{id}/events"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let mut body = response.into_body().into_data_stream();
  let mut buffer = String::new();
  let _ = timeout(Duration::from_millis(500), async {
    while let Some(chunk) = body.next().await {
      if let Ok(bytes) = chunk {
        buffer.push_str(&String::from_utf8_lossy(&bytes));
      }
    }
  })
  .await;

  let ids: Vec<&str> = buffer
    .lines()
    .filter_map(|line| line.strip_prefix("id: "))
    .collect();
  assert!(ids.contains(&"0"), "missing seq 0 in {ids:?}");
  assert!(ids.contains(&"1"), "missing seq 1 in {ids:?}");
}

#[tokio::test]
async fn sse_against_unknown_session_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping sse_against_unknown_session_returns_404");
    return;
  };
  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{}/events", Uuid::new_v4()))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resume_terminal_session_clears_events_and_restarts() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping resume_terminal_session_clears_events_and_restarts");
    return;
  };

  let app = create_router(state.clone());
  let id = submit_basic_session(app.clone(), "first run").await;
  // Stub executor reaches `failed: executor_not_yet_wired` in ~50 ms.
  tokio::time::sleep(Duration::from_millis(250)).await;

  // Sanity-check the prior event log so we can assert the reset later.
  let history_before = app
    .clone()
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{id}/events/history"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(history_before.status(), StatusCode::OK);
  let events_before = body_json(history_before).await;
  let before_count = events_before
    .as_array()
    .map(|arr| arr.len())
    .unwrap_or_default();
  assert!(
    before_count >= 1,
    "stub executor should have persisted ≥1 event before resume"
  );

  let resume = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{id}:resume"))
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({"user_input": "second run with new prompt"})).unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(resume.status(), StatusCode::OK);
  let body = body_json(resume).await;
  assert_eq!(body["resumed"], true);
  // The response carries the freshly-reset row: new user_input, no
  // finished_at / final_answer / error, status flipped back to running.
  assert_eq!(body["user_input"], "second run with new prompt");
  assert!(matches!(
    body["status"].as_str(),
    Some("running") | Some("failed")
  ));
  assert!(
    body["finished_at"].is_null() || body["status"] == "failed",
    "finished_at should be null right after resume (or already terminal again if the stub finished fast)"
  );

  // Wait for the executor to finish the rerun. The stub writes two
  // events (session_started + stopped), so once the rerun is terminal
  // the event count is at most 2 — proving the prior log was wiped.
  tokio::time::sleep(Duration::from_millis(300)).await;
  let history_after = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/harness/sessions/{id}/events/history"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  let events_after = body_json(history_after).await;
  let after = events_after
    .as_array()
    .expect("events history is array")
    .clone();
  assert_eq!(
    after.len(),
    2,
    "stub rerun should produce exactly 2 events, got {after:?}"
  );
  assert_eq!(after[0]["kind"], "session_started");
  assert_eq!(after[1]["kind"], "stopped");
  // The seq counter restarted at 0 because the prior rows were
  // cleared — proves the rerun semantic.
  assert_eq!(after[0]["seq"], 0);
  assert_eq!(after[1]["seq"], 1);
}

#[tokio::test]
async fn resume_append_mode_preserves_events_and_continues_seq() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping resume_append_mode_preserves_events_and_continues_seq");
    return;
  };

  let app = create_router(state.clone());
  let id = submit_basic_session(app.clone(), "first run").await;
  // Stub executor finishes in ~50 ms.
  tokio::time::sleep(Duration::from_millis(250)).await;

  // Snapshot the prior log so we can assert it survives the resume.
  let history_before = body_json(
    app
      .clone()
      .oneshot(
        Request::builder()
          .method("GET")
          .uri(format!("/v1/harness/sessions/{id}/events/history"))
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap(),
  )
  .await;
  let before = history_before
    .as_array()
    .expect("events history is array")
    .clone();
  // Stub writes exactly two events on the first run (seq 0,1).
  assert_eq!(before.len(), 2);
  assert_eq!(before[0]["seq"], 0);
  assert_eq!(before[1]["seq"], 1);

  // Resume in append mode with a follow-up prompt.
  let resume = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{id}:resume"))
        .header("content-type", "application/json")
        .body(Body::from(
          serde_json::to_vec(&json!({
            "user_input": "follow-up step",
            "mode": "append",
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(resume.status(), StatusCode::OK);
  let body = body_json(resume).await;
  assert_eq!(body["resumed"], true);
  assert_eq!(body["mode"], "append");
  assert_eq!(body["user_input"], "follow-up step");

  // Wait for the rerun to finish (stub writes two more events).
  tokio::time::sleep(Duration::from_millis(300)).await;
  let history_after = body_json(
    app
      .oneshot(
        Request::builder()
          .method("GET")
          .uri(format!("/v1/harness/sessions/{id}/events/history"))
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap(),
  )
  .await;
  let after = history_after
    .as_array()
    .expect("events history is array")
    .clone();

  // Append mode preserves the prior log: 2 old + 2 new = 4 total, with
  // a strictly monotonic seq series 0,1,2,3.
  assert_eq!(
    after.len(),
    4,
    "append mode must preserve all 4 events, got {after:?}"
  );
  let seqs: Vec<i64> = after
    .iter()
    .map(|event| event["seq"].as_i64().expect("seq is i64"))
    .collect();
  assert_eq!(seqs, vec![0, 1, 2, 3], "seq series must be continuous");
  assert_eq!(after[0]["kind"], "session_started");
  assert_eq!(after[1]["kind"], "stopped");
  assert_eq!(after[2]["kind"], "session_started");
  assert_eq!(after[3]["kind"], "stopped");
}

#[tokio::test]
async fn resume_default_mode_is_rerun_when_field_omitted() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping resume_default_mode_is_rerun_when_field_omitted");
    return;
  };
  let app = create_router(state);
  let id = submit_basic_session(app.clone(), "baseline").await;
  tokio::time::sleep(Duration::from_millis(250)).await;

  // Body intentionally omits `mode` — the wire default must be `rerun`.
  let resume = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{id}:resume"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&json!({})).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(resume.status(), StatusCode::OK);
  let body = body_json(resume).await;
  assert_eq!(
    body["mode"], "rerun",
    "omitting mode must surface as rerun in the response"
  );

  tokio::time::sleep(Duration::from_millis(300)).await;
  let history = body_json(
    app
      .oneshot(
        Request::builder()
          .method("GET")
          .uri(format!("/v1/harness/sessions/{id}/events/history"))
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap(),
  )
  .await;
  let events = history.as_array().expect("history is array").clone();
  // Rerun semantic: prior log wiped, new run produces exactly 2 events
  // restarting at seq 0.
  assert_eq!(events.len(), 2);
  assert_eq!(events[0]["seq"], 0);
  assert_eq!(events[1]["seq"], 1);
}

#[tokio::test]
async fn resume_rejects_running_session_with_400() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping resume_rejects_running_session_with_400");
    return;
  };

  // Use a sleepy executor so the row stays `running` while we issue
  // the resume call.
  use async_trait::async_trait;
  use yanshi_server::HarnessSessionExecutor;
  struct SleepyExecutor;
  #[async_trait]
  impl HarnessSessionExecutor for SleepyExecutor {
    async fn execute(&self, _ctx: yanshi_server::HarnessSessionContext) {
      tokio::time::sleep(Duration::from_secs(60)).await;
    }
  }
  let state = state.with_harness_executor(std::sync::Arc::new(SleepyExecutor));
  let app = create_router(state);
  let id = submit_basic_session(app.clone(), "still running").await;

  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{id}:resume"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&json!({})).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn resume_unknown_session_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping resume_unknown_session_returns_404");
    return;
  };
  let response = create_router(state)
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{}:resume", Uuid::new_v4()))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&json!({})).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_action_unknown_suffix_returns_400() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping post_action_unknown_suffix_returns_400");
    return;
  };
  let app = create_router(state);
  // Any valid uuid-shaped suffix lands at the dispatcher.
  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{}:wat", Uuid::new_v4()))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── P2.6 tenant boundary regression suite ────────────────────────────────
//
// After ddc497c plugged the cross-tenant leaks on every `:id`-bound
// harness endpoint, lock the contract down: seed a session as one
// tenant, hit the endpoint as a different tenant, expect 404. The
// shape mirrors `cross_tenant_get_run_returns_404` in `e2e_runs.rs`.
//
// `TENANT_HEADER` const is now defined at the top of this file so the
// Q1.4.x rewrites of the listing / submit tests can also use it.

#[tokio::test]
async fn cross_tenant_get_harness_session_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping cross_tenant_get_harness_session_returns_404");
    return;
  };
  let owner = format!("tenant-owner-{}", Uuid::new_v4());
  let app = create_router(state);
  let session_id = submit_for_tenant(app.clone(), "owner session", &owner).await;

  let response = app
    .oneshot(
      Request::builder()
        .uri(format!("/v1/harness/sessions/{session_id}"))
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
async fn cross_tenant_list_harness_events_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping cross_tenant_list_harness_events_returns_404");
    return;
  };
  let owner = format!("tenant-owner-{}", Uuid::new_v4());
  let app = create_router(state);
  let session_id = submit_for_tenant(app.clone(), "owner events", &owner).await;

  let response = app
    .oneshot(
      Request::builder()
        .uri(format!("/v1/harness/sessions/{session_id}/events/history"))
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
}

#[tokio::test]
async fn cross_tenant_stream_harness_events_sse_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping cross_tenant_stream_harness_events_sse_returns_404");
    return;
  };
  let owner = format!("tenant-owner-{}", Uuid::new_v4());
  let app = create_router(state);
  let session_id = submit_for_tenant(app.clone(), "owner sse", &owner).await;

  // Cross-tenant SSE subscribe must 404 before any envelope is
  // replayed — otherwise the intruder gets a live channel into
  // another tenant's session.
  let response = app
    .oneshot(
      Request::builder()
        .uri(format!("/v1/harness/sessions/{session_id}/events"))
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
}

#[tokio::test]
async fn cross_tenant_cancel_harness_session_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping cross_tenant_cancel_harness_session_returns_404");
    return;
  };
  let owner = format!("tenant-owner-{}", Uuid::new_v4());
  let app = create_router(state);
  let session_id = submit_for_tenant(app.clone(), "owner cancel", &owner).await;

  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{session_id}:cancel"))
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
}

#[tokio::test]
async fn cross_tenant_resume_harness_session_returns_404() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping cross_tenant_resume_harness_session_returns_404");
    return;
  };
  let owner = format!("tenant-owner-{}", Uuid::new_v4());
  let app = create_router(state);
  let session_id = submit_for_tenant(app.clone(), "owner resume", &owner).await;
  // Wait briefly for the stub executor to reach a terminal state so
  // the resume route's precondition check ("session must be terminal")
  // isn't what produces the error — we want the tenant 404 path.
  for _ in 0..50 {
    tokio::time::sleep(Duration::from_millis(20)).await;
    let row = body_json(
      axum::Router::clone(&app)
        .oneshot(
          Request::builder()
            .uri(format!("/v1/harness/sessions/{session_id}"))
            .header(TENANT_HEADER, owner.as_str())
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    if row["status"] != "running" {
      break;
    }
  }

  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{session_id}:resume"))
        .header(
          TENANT_HEADER,
          format!("tenant-intruder-{}", Uuid::new_v4()).as_str(),
        )
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&json!({})).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
