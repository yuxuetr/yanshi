//! Y2.4 acceptance (gateway): `POST /v1/runs/{id}:resume` continues a
//! failed run from its checkpoint under the same run id.
//!
//! `speak` (billed `tts`, `vendor: mock`) succeeds and `note` reads a file
//! that does not exist yet, so the run fails. After the file is created
//! the run is resumed: it succeeds, `speak` started exactly once across
//! both attempts, its artifact dir holds only `1.wav`, and the event
//! `seq` continues instead of restarting. Another tenant gets 404, and a
//! succeeded run cannot be resumed.
//!
//! Skipped without `YANSHI_DATABASE_TEST_URL`.

use std::path::PathBuf;
use std::time::Duration;

use axum::{
  body::Body,
  http::{Request, StatusCode, header::CONTENT_TYPE},
};
use serde_json::{Value, json};
use tokio::time::sleep;
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::{Database, EventRepo, RunRepo};
use yanshi_server::{
  AppState, create_router, gateway_policy::GatewayPolicy, tenant::TENANT_HEADER,
};

const MOCK_REGISTRY: &str = r#"
models:
  mock-tts:
    vendor: mock
    type: tts
    model_id: mock-tts
providers:
  mock:
    api_key_env: MOCK_API_KEY
"#;

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

/// Process-wide mock registry and run base dir (the gateway resolves
/// `YANSHI_RUN_DIR` at submit time; the `tts` node reads
/// `YANSHI_MODELS_CONFIG`).
async fn install_env() -> PathBuf {
  static SETUP: tokio::sync::OnceCell<PathBuf> = tokio::sync::OnceCell::const_new();
  SETUP
    .get_or_init(|| async {
      let dir = std::env::temp_dir().join(format!("yanshi-resume-e2e-{}", std::process::id()));
      std::fs::create_dir_all(dir.join("runs")).expect("run dir");
      let models = dir.join("models.yml");
      std::fs::write(&models, MOCK_REGISTRY).expect("registry");
      // SAFETY: set once under the OnceCell lock before any reader.
      unsafe {
        std::env::set_var("YANSHI_MODELS_CONFIG", &models);
        std::env::set_var("YANSHI_RUN_DIR", dir.join("runs"));
      }
      yanshi_llm::Yanshi::init_with_config(models.to_str().expect("utf-8"))
        .await
        .expect("mock registry loads");
      dir
    })
    .await
    .clone()
}

async fn fresh_state(root: &std::path::Path) -> Option<AppState> {
  let db = Database::connect_and_migrate(&live_url()?, 4).await.ok()?;
  Some(AppState::new(db).with_gateway_policy(GatewayPolicy {
    host_node_types: ["file".to_string()].into(),
    path_roots: vec![root.to_path_buf()],
    ..Default::default()
  }))
}

async fn post(state: &AppState, uri: &str, tenant: &str, body: Value) -> (StatusCode, Value) {
  let response = create_router(state.clone())
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, "application/json")
        .header(TENANT_HEADER, tenant)
        .body(Body::from(body.to_string()))
        .expect("request"),
    )
    .await
    .expect("response");
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .expect("body");
  (
    status,
    serde_json::from_slice(&bytes).unwrap_or(Value::Null),
  )
}

async fn wait_terminal(state: &AppState, run_id: Uuid) -> String {
  for _ in 0..200 {
    sleep(Duration::from_millis(25)).await;
    let row = state
      .repos
      .runs
      .get(run_id)
      .await
      .expect("get")
      .expect("row");
    if matches!(row.status.as_str(), "succeeded" | "failed" | "cancelled") {
      return row.status;
    }
  }
  panic!("run {run_id} did not finish");
}

#[tokio::test]
async fn a_failed_run_resumes_without_rerunning_finished_nodes() {
  if live_url().is_none() {
    eprintln!("skipping run resume e2e — set YANSHI_DATABASE_TEST_URL");
    return;
  }
  let root = install_env().await;
  let Some(state) = fresh_state(&root).await else {
    eprintln!("skipping run resume e2e — database unreachable");
    return;
  };
  let tenant = format!("tenant-resume-{}", Uuid::new_v4());
  let note = root.join(format!("note-{}.txt", Uuid::new_v4()));
  let workflow = format!(
    r#"
name: resumable
nodes:
  - id: speak
    type: tts
    parameters:
      model: mock-tts
      voice: narrator
      input_template: "a short narration"
  - id: note
    type: file
    dependencies: ["speak"]
    parameters:
      operation: read
      path: "{}"
      allowed_paths: ["{}"]
"#,
    note.display(),
    root.display()
  );

  let (code, body) = post(&state, "/v1/runs", &tenant, json!({ "workflow": workflow })).await;
  assert_eq!(code, StatusCode::OK, "{body}");
  let run_id: Uuid = body["run_id"]
    .as_str()
    .expect("run_id")
    .parse()
    .expect("uuid");
  assert_eq!(wait_terminal(&state, run_id).await, "failed");
  let first_max = state
    .repos
    .events
    .max_seq(&tenant, run_id)
    .await
    .expect("max seq")
    .expect("events written");

  // Not this tenant's run: 404, and the run stays failed.
  let (code, _) = post(
    &state,
    &format!("/v1/runs/{run_id}:resume"),
    &format!("tenant-other-{}", Uuid::new_v4()),
    json!({}),
  )
  .await;
  assert_eq!(code, StatusCode::NOT_FOUND);

  std::fs::write(&note, "now it exists").expect("note");
  let (code, body) = post(
    &state,
    &format!("/v1/runs/{run_id}:resume"),
    &tenant,
    json!({}),
  )
  .await;
  assert_eq!(code, StatusCode::OK, "{body}");
  assert_eq!(wait_terminal(&state, run_id).await, "succeeded");

  let events = state
    .repos
    .events
    .list_after(&tenant, run_id, -1, 10_000)
    .await
    .expect("events");
  let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
  let mut unique = seqs.clone();
  unique.dedup();
  assert_eq!(seqs, unique, "seq must not restart on resume");
  assert!(
    seqs.iter().any(|seq| *seq > first_max),
    "resume appended events"
  );
  let speak_starts = events
    .iter()
    .filter(|e| e.kind == "node.started" && e.payload["node_id"] == "speak")
    .count();
  assert_eq!(speak_starts, 1, "speak must not run again on resume");

  let speak_dir = root
    .join("runs")
    .join(run_id.to_string())
    .join("artifacts")
    .join("speak");
  let produced: Vec<String> = std::fs::read_dir(&speak_dir)
    .expect("speak dir")
    .flatten()
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
  assert_eq!(produced, vec!["1.wav".to_string()]);

  // A succeeded run is not resumable.
  let (code, _) = post(
    &state,
    &format!("/v1/runs/{run_id}:resume"),
    &tenant,
    json!({}),
  )
  .await;
  assert_eq!(code, StatusCode::BAD_REQUEST);
}

/// Route parsing needs no database: a malformed id on `:resume` is a
/// 400, and an unknown suffix still reaches `:cancel`'s own 400.
#[tokio::test]
async fn resume_route_rejects_malformed_ids_before_touching_the_database() {
  let pool = sqlx::postgres::PgPoolOptions::new()
    .connect_lazy("postgres://postgres:postgres@localhost:5432/yanshi_test")
    .expect("lazy pg");
  let state = AppState::new(Database {
    pool,
    read_pool: None,
  });
  let (code, body) = post(&state, "/v1/runs/not-a-uuid:resume", "t", json!({})).await;
  assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
  assert!(body.to_string().contains("invalid run id"), "{body}");

  let (code, body) = post(
    &state,
    &format!("/v1/runs/{}:pause", Uuid::new_v4()),
    "t",
    json!({}),
  )
  .await;
  assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
  assert!(body.to_string().contains(":cancel"), "{body}");
}
