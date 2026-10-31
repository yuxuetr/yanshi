//! End-to-end tests for `GET /v1/runs/{id}/resume-plan`.
//!
//! Requires a live Postgres pointed to by `YANSHI_DATABASE_TEST_URL`.
//! Without it the tests exit early so workspace `cargo test` stays
//! hermetic.

use axum::{
  body::Body,
  http::{Request, StatusCode},
};
use serde_json::json;
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::{Database, NewRun, RunRepo, RunStatus};
use yanshi_server::{AppState, create_router};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn fresh_state() -> Option<AppState> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  sqlx::query("TRUNCATE runs RESTART IDENTITY CASCADE")
    .execute(&db.pool)
    .await
    .ok()?;
  // The checkpoint dirs are tempdirs; the gateway only accepts
  // checkpoint_dir under an operator root (eval 2026-10-07 S7).
  Some(
    AppState::new(db).with_gateway_policy(yanshi_server::gateway_policy::GatewayPolicy {
      path_roots: vec!["/tmp".into(), std::env::temp_dir()],
      ..Default::default()
    }),
  )
}

fn write_checkpoint(dir: &std::path::Path, run_id: Uuid, tool_records: Vec<serde_json::Value>) {
  let workflow_dir = dir.join(run_id.to_string());
  fs::create_dir_all(&workflow_dir).unwrap();
  let path = workflow_dir.join("checkpoint_latest.json");
  let body = json!({
    "workflow_id": run_id.to_string(),
    "last_completed_node": "agent_node",
    "state": {
      "agent_node": {
        "agent_resume": {
          "tool_calls": tool_records
        }
      }
    },
    "created_at": "2026-05-14T00:00:00Z",
    "status": "Running",
    "metadata": {}
  });
  fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
}

#[tokio::test]
async fn resume_plan_route_returns_plan_for_existing_checkpoint() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping resume_plan_route_returns_plan_for_existing_checkpoint");
    return;
  };
  let run_id = Uuid::new_v4();
  state
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      tenant_id: "default".into(),
      workflow: "name: stub".into(),
      status: RunStatus::Running,
      run_dir: None,
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .unwrap();
  let tmp = TempDir::new().unwrap();
  write_checkpoint(
    tmp.path(),
    run_id,
    vec![
      json!({
        "call_id": "call-1",
        "tool": "http",
        "step_index": 2,
        "side_effect_class": "idempotent",
        "replay_policy": "replay_allowed"
      }),
      json!({
        "call_id": "call-2",
        "tool": "send_email",
        "step_index": 3,
        "side_effect_class": "mutating",
        "replay_policy": "manual_required"
      }),
    ],
  );

  let app = create_router(state.clone());
  let uri = format!(
    "/v1/runs/{run_id}/resume-plan?checkpoint_dir={}",
    urlencoding::encode(tmp.path().to_str().unwrap())
  );
  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024)
    .await
    .unwrap();
  let plan: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
  assert_eq!(plan["workflow_id"], run_id.to_string());
  assert_eq!(plan["schema_version"], 1);
  let tool_calls = plan["tool_calls"].as_array().unwrap();
  assert_eq!(tool_calls.len(), 2);
  let decisions: Vec<&str> = tool_calls
    .iter()
    .map(|t| t["decision"].as_str().unwrap())
    .collect();
  assert!(decisions.contains(&"replay"));
  assert!(decisions.contains(&"requires_manual"));
  assert_eq!(plan["summary"]["requires_manual"], 1);
  assert_eq!(plan["summary"]["to_replay"], 1);
  assert_eq!(plan["force_replay"], false);
}

#[tokio::test]
async fn resume_plan_route_honours_force_replay_query_param() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping resume_plan_route_honours_force_replay_query_param");
    return;
  };
  let run_id = Uuid::new_v4();
  state
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      tenant_id: "default".into(),
      workflow: "name: stub".into(),
      status: RunStatus::Running,
      run_dir: None,
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .unwrap();
  let tmp = TempDir::new().unwrap();
  write_checkpoint(
    tmp.path(),
    run_id,
    vec![json!({
      "call_id": "call-1",
      "tool": "mystery",
      "step_index": 2,
      "side_effect_class": "unknown",
      "replay_policy": "manual_required"
    })],
  );

  let app = create_router(state.clone());
  let uri = format!(
    "/v1/runs/{run_id}/resume-plan?checkpoint_dir={}&force_replay=true",
    urlencoding::encode(tmp.path().to_str().unwrap())
  );
  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024)
    .await
    .unwrap();
  let plan: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
  assert_eq!(plan["force_replay"], true);
  assert_eq!(plan["tool_calls"][0]["decision"], "replay");
}

#[tokio::test]
async fn resume_plan_route_returns_404_when_run_missing() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping resume_plan_route_returns_404_when_run_missing");
    return;
  };
  let app = create_router(state);
  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/v1/runs/{}/resume-plan", Uuid::new_v4()))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resume_plan_route_returns_404_when_checkpoint_missing() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping resume_plan_route_returns_404_when_checkpoint_missing");
    return;
  };
  let run_id = Uuid::new_v4();
  state
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      tenant_id: "default".into(),
      workflow: "name: stub".into(),
      status: RunStatus::Running,
      run_dir: None,
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .unwrap();
  let tmp = TempDir::new().unwrap();
  let app = create_router(state.clone());
  let uri = format!(
    "/v1/runs/{run_id}/resume-plan?checkpoint_dir={}",
    urlencoding::encode(tmp.path().to_str().unwrap())
  );
  let response = app
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
