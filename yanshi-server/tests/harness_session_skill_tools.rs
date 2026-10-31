//! W4.1c — a harness session backed by `skill_name` gets that skill's
//! own tools, not just the hardcoded default (read-only file + http)
//! registry.
//!
//! Before this, `harness_live.rs` only ever carried `skill_name` for
//! observability (its own doc comment said so explicitly) — every
//! session, skill-named or not, got the same hardcoded default registry.
//! This test declares a skill with a `shell` tool (never part of the
//! default registry) and drives a real `POST /v1/harness/sessions`
//! submission end to end against `LiveHarnessExecutor` + a mock LLM,
//! proving the shell call reaches the hook layer (evidenced by a pending
//! approval under `--profile production`, decidable through the existing
//! `/v1/harness/sessions/{id}/approvals*` routes) and the session reaches
//! `completed`.
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`. Without
//! it the test self-skips, matching every other server e2e test file.
//!
//! SAFETY: mutates process-wide env vars (`YANSHI_MODELS_CONFIG`,
//! `MOCK_API_KEY`, `YANSHI_MOCK_RESPONSES`, `YANSHI_MOCK_TOOL_CALLS`)
//! that `yanshi_llm::Yanshi::init()` reads exactly once per process.
//! This is the only test in this binary that touches them (see
//! `yanshi-server/tests/skill_run_approval_shell_gate.rs`'s doc comment
//! for why that matters).

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
use yanshi_server::{AppState, LiveHarnessExecutor, SkillCatalog, create_router};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn body_json(response: axum::response::Response) -> Value {
  let bytes = to_bytes(response.into_body(), 1024 * 1024)
    .await
    .expect("body collected");
  serde_json::from_slice(&bytes).expect("body is JSON")
}

#[tokio::test]
async fn skill_backed_session_gets_the_skills_shell_tool_and_gates_it() {
  let Some(url) = live_url() else {
    eprintln!("skipping skill_backed_session_gets_the_skills_shell_tool_and_gates_it: no DB url");
    return;
  };
  let db = Database::connect_and_migrate(&url, 4).await.unwrap();

  let tmp = tempfile::tempdir().unwrap();
  let skill_dir = tmp.path().join("shell-harness-skill");
  std::fs::create_dir_all(&skill_dir).unwrap();
  std::fs::write(
    skill_dir.join("skill.toml"),
    r#"
[skill]
name = "shell-harness-skill"
version = "0.1.0"
description = "W4.1c harness-session skill-tool regression skill"

[persona]
role = "Follow instructions exactly."

[model]
name = "mock-chat"

[[tools]]
name = "shell"
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
name = "shell-harness-skill"
version = "0.1.0"
path = "shell-harness-skill"
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
        [{ "id": "call_1", "name": "shell", "arguments": { "command": "echo hi" } }],
        [],
      ])
      .to_string(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      json!([
        "(unused — native tool call)",
        r#"{"thought":"done","answer":"ran the command"}"#,
      ])
      .to_string(),
    );
  }

  let state = AppState::new(db).with_gateway_policy(yanshi_server::gateway_policy::GatewayPolicy {
    path_roots: vec!["/tmp".into(), std::env::temp_dir()],
    ..Default::default()
  });
  let live = LiveHarnessExecutor::new(state.approval_registry.clone(), Duration::from_secs(60));
  let state = state
    .with_harness_executor(Arc::new(live))
    .with_skills(catalog);
  let app = create_router(state.clone());

  let workspace = tempfile::tempdir().expect("workspace tempdir");
  let tenant = format!("skill-harness-{}", Uuid::new_v4());

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
            "user_input": "run echo hi",
            "workspace_root": workspace.path().display().to_string(),
            "profile": "production",
            "runtime_kind": "react",
            "model": "mock-chat",
            "skill_name": "shell-harness-skill",
            "tenant_id": tenant,
          })
          .to_string(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  let session_id = body["session_id"].as_str().expect("session_id").to_string();

  // Poll GET /v1/harness/sessions/{id}/approvals until the shell call's
  // approval request is parked — proves the skill's shell tool actually
  // reached the hook layer (the hardcoded default registry has no shell
  // tool at all, so this could never fire without W4.1c's wiring).
  let mut request_id: Option<String> = None;
  for _ in 0..100 {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let get = app
      .clone()
      .oneshot(
        Request::builder()
          .uri(format!("/v1/harness/sessions/{session_id}/approvals"))
          .header("X-Yanshi-Tenant", &tenant)
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    let payload = body_json(get).await;
    let approvals = payload["approvals"].as_array().expect("approvals array");
    if let Some(first) = approvals.first() {
      request_id = first["id"].as_str().map(str::to_string);
      break;
    }
  }
  let request_id = request_id.expect("a pending approval appeared for the skill's shell tool call");

  let decide = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!(
          "/v1/harness/sessions/{session_id}/approvals/{request_id}"
        ))
        .header("content-type", "application/json")
        .header("X-Yanshi-Tenant", &tenant)
        .body(Body::from(
          json!({"decision": "allow", "scope": "once"}).to_string(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(decide.status(), StatusCode::OK);

  let mut terminal: Option<Value> = None;
  for _ in 0..100 {
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
    let payload = body_json(get).await;
    if payload["status"].as_str() != Some("running") {
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
  let final_row = terminal.expect("session reached a terminal status within timeout");
  assert_eq!(
    final_row["status"].as_str(),
    Some("completed"),
    "unexpected terminal row: {final_row}"
  );
}
