//! Y8.5: a skill's `[[workflows]]` under `POST /v1/skills/{name}:run`.
//!
//! - The agent calls the workflow tool; the call is approved through the
//!   run's approvals endpoint (production profile: a workflow tool is not
//!   declared idempotent); the file the workflow renders is registered as
//!   the skill run's artifact.
//! - A skill whose workflow uses a host-reaching node the gateway policy
//!   does not allow is refused at submit time.
//!
//! Requires Postgres at `YANSHI_DATABASE_TEST_URL`; self-skips without it.
//!
//! SAFETY: the first test mutates the process-wide env vars
//! `yanshi_llm::Yanshi::init()` reads once per process; the second never
//! calls a model.

use axum::{
  body::{Body, to_bytes},
  http::{Request, StatusCode, header::CONTENT_TYPE},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use yanshi_db::Database;
use yanshi_server::{AppState, SkillCatalog, create_router};

async fn fresh_state() -> Option<AppState> {
  let url = std::env::var("YANSHI_DATABASE_TEST_URL").ok()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  Some(AppState::new(db))
}

async fn body_json(response: axum::response::Response) -> Value {
  let bytes = to_bytes(response.into_body(), 1024 * 1024)
    .await
    .expect("body collected");
  serde_json::from_slice(&bytes).expect("body is JSON")
}

/// A one-skill catalog in `root`: `skill.toml` names `workflow.yml`.
fn catalog(root: &std::path::Path, name: &str, workflow: &str) -> SkillCatalog {
  let skill_dir = root.join(name);
  std::fs::create_dir_all(&skill_dir).unwrap();
  std::fs::write(
    skill_dir.join("skill.toml"),
    format!(
      r#"
[skill]
name = "{name}"
version = "0.1.0"
description = "Y8.5 workflow-tool skill"

[persona]
role = "Follow instructions exactly."

[model]
name = "mock-chat"

[[workflows]]
name = "table"
path = "workflow.yml"
description = "A table about a topic, as CSV"
"#
    ),
  )
  .unwrap();
  std::fs::write(skill_dir.join("workflow.yml"), workflow).unwrap();
  let index_path = root.join("skills.index.toml");
  std::fs::write(
    &index_path,
    format!(
      "schema_version = 1\nname = \"test-registry\"\n\n[[skills]]\nname = \"{name}\"\nversion = \"0.1.0\"\npath = \"{name}\"\n"
    ),
  )
  .unwrap();
  let index = yanshi_skills::SkillRegistryIndex::load(&index_path).unwrap();
  SkillCatalog::from_index(index, index_path)
}

const TABLE: &str = r#"
name: table
inputs:
  topic:
    description: "What the table is about"
nodes:
  - id: rows
    type: template
    parameters:
      output_format: json
      template: '{"rows": [{"topic": {{ topic | json_encode() }}, "facts": 2}]}'
  - id: table
    type: render_table
    dependencies: ["rows"]
    input_mapping:
      rows: "{{ nodes.rows.outputs.rows }}"
"#;

async fn get(app: &axum::Router, uri: String) -> Value {
  let response = app
    .clone()
    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  body_json(response).await
}

#[tokio::test]
async fn a_workflow_tool_runs_after_approval_and_its_file_is_a_run_artifact() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping a_workflow_tool_runs_after_approval_and_its_file_is_a_run_artifact");
    return;
  };
  let tmp = tempfile::tempdir().unwrap();
  let catalog = catalog(tmp.path(), "tabler", TABLE);
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
    std::env::set_var("YANSHI_RUN_DIR", tmp.path().join("runs"));
    std::env::set_var(
      "YANSHI_MOCK_TOOL_CALLS",
      json!([
        [{ "id": "call_1", "name": "table", "arguments": { "topic": "tides" } }],
        [],
      ])
      .to_string(),
    );
    std::env::set_var(
      "YANSHI_MOCK_RESPONSES",
      json!([
        "(unused — native tool call)",
        r#"{"thought":"done","answer":"made the table"}"#,
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
        .uri("/v1/skills/tabler:run")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
          json!({"input": "a table about tides"}).to_string(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let run_id = body_json(response).await["run_id"]
    .as_str()
    .expect("run_id")
    .to_string();

  let mut request_id = None;
  for _ in 0..100 {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let payload = get(&app, format!("/v1/runs/{run_id}/approvals")).await;
    if let Some(first) = payload["approvals"].as_array().and_then(|a| a.first()) {
      request_id = first["id"].as_str().map(str::to_string);
      break;
    }
  }
  let request_id = request_id.expect("the workflow tool call waits for approval");
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

  let mut terminal = None;
  for _ in 0..100 {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let payload = get(&app, format!("/v1/runs/{run_id}")).await;
    if !matches!(payload["status"].as_str(), Some("running" | "queued")) {
      terminal = Some(payload);
      break;
    }
  }
  // SAFETY: see the file-level doc comment.
  unsafe {
    for key in [
      "YANSHI_MODELS_CONFIG",
      "MOCK_API_KEY",
      "YANSHI_RUN_DIR",
      "YANSHI_MOCK_TOOL_CALLS",
      "YANSHI_MOCK_RESPONSES",
    ] {
      std::env::remove_var(key);
    }
  }
  let final_row = terminal.expect("run finished");
  assert_eq!(final_row["status"], "succeeded", "{final_row}");

  let listed = get(&app, format!("/v1/runs/{run_id}/artifacts")).await;
  let artifacts = listed["artifacts"].as_array().expect("artifacts");
  assert_eq!(artifacts.len(), 1, "{listed}");
  assert_eq!(artifacts[0]["node_id"], "table");
  assert_eq!(artifacts[0]["mime_type"], "text/csv");
  let artifact_id = artifacts[0]["id"].as_str().expect("id");
  let download = app
    .clone()
    .oneshot(
      Request::builder()
        .uri(format!("/v1/runs/{run_id}/artifacts/{artifact_id}"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(download.status(), StatusCode::OK);
  let csv = to_bytes(download.into_body(), 1024 * 1024).await.unwrap();
  assert!(
    String::from_utf8_lossy(&csv).contains("tides"),
    "the agent's input reached the workflow"
  );
}

#[tokio::test]
async fn a_skill_whose_workflow_reaches_the_host_is_refused() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping a_skill_whose_workflow_reaches_the_host_is_refused");
    return;
  };
  let tmp = tempfile::tempdir().unwrap();
  let shell = r#"
name: table
inputs:
  topic:
    default: "x"
nodes:
  - id: run
    type: shell
    parameters:
      command: "ls"
"#;
  let app = create_router(state.with_skills(catalog(tmp.path(), "sheller", shell)));
  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/skills/sheller:run")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"input": "go"}).to_string()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  let body = body_json(response).await.to_string();
  assert!(body.contains("workflow tool 'table'"), "{body}");
  assert!(body.contains("shell"), "{body}");
}
