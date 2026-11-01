//! W4.1b — approval gating for `POST /v1/skills/{name}:run`.
//!
//! Before this, `run_skill_agent` ran a skill's tools with zero approval
//! gating, unlike every other tool-execution surface (CLI `harness run`,
//! the server's own harness-session path). This test drives the real
//! `/v1/skills/{name}:run` route end to end against a mock LLM + a real
//! `ShellTool`, proving a `shell` tool call is escalated to an approval
//! request, visible via the new `GET /v1/runs/{id}/approvals`, decidable
//! via the new `POST /v1/runs/{id}/approvals/{request_id}`, and that the
//! run reaches a terminal status with the tool call visible in
//! `/v1/runs/{id}/events`.
//!
//! The read-only-tool regression case
//! (`read_only_file_tool_call_never_creates_a_pending_approval`) lives in
//! its own `skill_run_approval_read_only.rs` file rather than here: both
//! tests drive the mock LLM provider, and `yanshi_llm::Yanshi::init()`
//! (in turn `MockProvider::new`'s `YANSHI_MOCK_TOOL_CALLS`/
//! `YANSHI_MOCK_RESPONSES` queue load) happens once per process behind
//! an internal `OnceCell` — whichever test's `ensure_llm_initialized()`
//! call wins the race captures its env vars for the rest of the process,
//! silently starving the other test's queue. Splitting into two files (two
//! test binaries, two processes) sidesteps this the same way
//! `skill_routes.rs`'s established "only one mock-LLM-driving test per
//! binary" convention does.
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
async fn shell_tool_call_requires_approval_and_appears_in_run_events() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping shell_tool_call_requires_approval_and_appears_in_run_events");
    return;
  };

  let tmp = tempfile::tempdir().unwrap();
  let skill_dir = tmp.path().join("shell-runner");
  std::fs::create_dir_all(&skill_dir).unwrap();
  std::fs::write(
    skill_dir.join("skill.toml"),
    r#"
[skill]
name = "shell-runner"
version = "0.1.0"
description = "W4.1b approval-gating regression skill"

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
name = "shell-runner"
version = "0.1.0"
path = "shell-runner"
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

  let app = create_router(state.with_skills(catalog));
  let response = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/skills/shell-runner:run")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"input": "run echo hi"}).to_string()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let body = body_json(response).await;
  let run_id = body["run_id"].as_str().expect("run_id").to_string();

  // Poll GET /v1/runs/{id}/approvals until the hook layer parks the
  // shell call's approval request (HarnessProfile::Production
  // auto-escalates every NonIdempotent tool call, and shell is always
  // NonIdempotent).
  let mut request_id: Option<String> = None;
  for _ in 0..100 {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let get = app
      .clone()
      .oneshot(
        Request::builder()
          .uri(format!("/v1/runs/{run_id}/approvals"))
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
  let request_id = request_id.expect("a pending approval appeared for the shell tool call");

  let decide = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/runs/{run_id}/approvals/{request_id}"))
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
          json!({"decision": "allow", "scope": "once"}).to_string(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(decide.status(), StatusCode::OK);
  let decide_body = body_json(decide).await;
  assert_eq!(decide_body["resolved"], true);

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

  let history = app
    .oneshot(
      Request::builder()
        .uri(format!("/v1/runs/{run_id}/events/history"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  let events = body_json(history).await;
  let events = events.as_array().expect("history is array");
  let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
  assert!(
    kinds.contains(&"approval_requested"),
    "expected approval_requested in run events; kinds: {kinds:?}"
  );
  assert!(
    kinds.contains(&"approval_decided"),
    "expected approval_decided in run events; kinds: {kinds:?}"
  );
  // No `tool_call_requested`/`tool_call_completed` here: those kinds come
  // from the `HarnessRuntime` bridge (see the CLI's
  // `harness_run_shares_one_seq_series_between_hook_and_runtime_events`
  // test), which `run_skill_agent` deliberately doesn't spin up — a skill
  // run drives a bare `ReActAgent` turn via `AgentRuntime::run`, not a
  // full Harness session. `HookedTool`'s own `approval_requested`/
  // `approval_decided` pair (asserted above) is the complete signal this
  // design emits for a gated tool call.
  assert!(
    kinds.contains(&"skill_run_completed"),
    "expected the skill's own completion event to still land; kinds: {kinds:?}"
  );

  // Every event seq is unique — proves the hook-layer sink and
  // skill_execute's own completion event share one collision-free
  // series rather than both starting at 0.
  let mut seqs: Vec<i64> = events.iter().filter_map(|e| e["seq"].as_i64()).collect();
  let original_len = seqs.len();
  seqs.sort_unstable();
  seqs.dedup();
  assert_eq!(
    seqs.len(),
    original_len,
    "expected every event seq to be unique, got duplicates: {events:?}"
  );
}
