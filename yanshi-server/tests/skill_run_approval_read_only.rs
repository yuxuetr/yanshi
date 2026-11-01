//! W4.1b regression: a skill that only ever calls a read-only tool must
//! still run to completion via `POST /v1/skills/{name}:run` without ever
//! creating a pending approval — no behavior change for the common (no
//! mutating tools) case now that `run_skill_agent` gates tool calls
//! through the approval pipeline (see `skill_run_approval_shell_gate.rs`).
//!
//! `FileTool::idempotency` classifies a `read`/`list` operation
//! `Idempotent`, so `HarnessProfile::Production`'s auto-escalation (which
//! only fires for `NonIdempotent` calls) never triggers.
//!
//! Kept in its own file (its own test binary/process) rather than
//! alongside the shell-gate test — see that file's doc comment for why
//! two mock-LLM-driving tests can't share one process.
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`. Without
//! it the test self-skips, matching every other server e2e test file.
//!
//! SAFETY: mutates process-wide env vars (`YANSHI_MODELS_CONFIG`,
//! `MOCK_API_KEY`, `YANSHI_MOCK_RESPONSES`, `YANSHI_MOCK_TOOL_CALLS`)
//! that `yanshi_llm::Yanshi::init()` reads exactly once per process.
//! This is the only test in this binary that touches them.

use axum::{
  body::{Body, to_bytes},
  http::{Request, StatusCode, header::CONTENT_TYPE},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use yanshi_db::Database;
use yanshi_server::{AppState, SkillCatalog, create_router};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn fresh_state() -> Option<AppState> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  // No TRUNCATE: tests run in parallel and each uses fresh ids, so a
  // global reset only races sibling tests.
  Some(AppState::new(db))
}

async fn body_json(response: axum::response::Response) -> Value {
  let bytes = to_bytes(response.into_body(), 1024 * 1024)
    .await
    .expect("body collected");
  serde_json::from_slice(&bytes).expect("body is JSON")
}

#[tokio::test]
async fn read_only_file_tool_call_never_creates_a_pending_approval() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping read_only_file_tool_call_never_creates_a_pending_approval");
    return;
  };

  let tmp = tempfile::tempdir().unwrap();
  let skill_dir = tmp.path().join("file-reader");
  std::fs::create_dir_all(&skill_dir).unwrap();
  let skill_toml_path = skill_dir.join("skill.toml");
  std::fs::write(
    &skill_toml_path,
    r#"
[skill]
name = "file-reader"
version = "0.1.0"
description = "W4.1b approval-gating regression skill"

[persona]
role = "Follow instructions exactly."

[model]
name = "mock-chat"

[[tools]]
name = "file"
"#,
  )
  .unwrap();

  let index_path = tmp.path().join("skills.index.toml");
  std::fs::write(
    &index_path,
    r#"
schema_version = 1
name = "test-registry"

[[skills]]
name = "file-reader"
version = "0.1.0"
path = "file-reader"
"#,
  )
  .unwrap();
  let index = yanshi_skills::SkillRegistryIndex::load(&index_path).unwrap();
  let catalog = SkillCatalog::from_index(index, index_path);

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
      "YANSHI_MOCK_TOOL_CALLS",
      json!([
        [{ "id": "call_1", "name": "file", "arguments": { "operation": "read", "path": skill_toml_path.to_string_lossy() } }],
        [],
      ])
      .to_string(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      json!([
        "(unused — native tool call)",
        r#"{"thought":"done","answer":"read the file"}"#,
      ])
      .to_string(),
    );
  }

  let approval_registry = state.approval_registry.clone();
  let app = create_router(state.with_skills(catalog));
  let response = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/skills/file-reader:run")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
          json!({"input": "read the manifest"}).to_string(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  let run_id = body["run_id"].as_str().expect("run_id").to_string();

  let mut terminal: Option<Value> = None;
  for _ in 0..100 {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let get = app
      .clone()
      .oneshot(
        Request::builder()
          .uri(format!("/v1/runs/{run_id}"))
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    let payload = body_json(get).await;
    if payload["status"].as_str() != Some("running") && payload["status"].as_str() != Some("queued")
    {
      terminal = Some(payload);
      break;
    }
  }
  // SAFETY: see the file-level doc comment; cleanup regardless of outcome.
  unsafe {
    std::env::remove_var("YANSHI_MODELS_CONFIG");
    std::env::remove_var("MOCK_API_KEY");
    std::env::remove_var("YANSHI_MOCK_TOOL_CALLS");
    std::env::remove_var("YANSHI_MOCK_RESPONSES");
  }
  let final_row = terminal.expect("run reached a terminal status within timeout");
  assert_eq!(
    final_row["status"].as_str(),
    Some("succeeded"),
    "unexpected terminal row: {final_row}"
  );

  assert_eq!(
    approval_registry.pending_count(),
    0,
    "a read-only tool call must never create a pending approval"
  );

  let get_approvals = app
    .oneshot(
      Request::builder()
        .uri(format!("/v1/runs/{run_id}/approvals"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(get_approvals.status(), StatusCode::OK);
  let payload = body_json(get_approvals).await;
  assert_eq!(
    payload["approvals"].as_array().map(Vec::len),
    Some(0),
    "expected no pending approvals for the read-only run"
  );
}
