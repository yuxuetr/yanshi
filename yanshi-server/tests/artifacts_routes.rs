//! Y1.3 acceptance: a gateway run that produces media leaves registered,
//! downloadable artifacts, tenant-scoped.
//!
//! Submits a `tts` workflow under `vendor: mock` (no keys), waits for
//! `succeeded`, lists the run's artifacts, downloads the one WAV and
//! checks its SHA-256 against the listing, then confirms another tenant
//! gets 404 on both routes.
//!
//! Skipped without `YANSHI_DATABASE_TEST_URL`.

use std::time::Duration;

use axum::{
  body::Body,
  http::{Request, StatusCode, header::CONTENT_TYPE},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::sleep;
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::{Database, RunRepo};
use yanshi_server::{AppState, create_router, tenant::TENANT_HEADER};

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

const TTS_WORKFLOW: &str = r#"
name: artifacts e2e
nodes:
  - id: speak
    type: tts
    parameters:
      model: mock-tts
      voice: narrator
      input_template: "twelve words of narration for the artifact route test, nothing more"
"#;

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

/// Mock registry + a run base dir of our own, both process-wide: the
/// `tts` node calls `Yanshi::init()` (reads `YANSHI_MODELS_CONFIG`) and
/// the gateway resolves `YANSHI_RUN_DIR` at submit time.
async fn install_env() -> std::path::PathBuf {
  static SETUP: tokio::sync::OnceCell<std::path::PathBuf> = tokio::sync::OnceCell::const_new();
  SETUP
    .get_or_init(|| async {
      let dir = std::env::temp_dir().join(format!("yanshi-artifacts-e2e-{}", std::process::id()));
      let runs = dir.join("runs");
      std::fs::create_dir_all(&runs).expect("run dir");
      let models = dir.join("models.yml");
      std::fs::write(&models, MOCK_REGISTRY).expect("registry");
      // SAFETY: set once under the OnceCell lock before any reader.
      unsafe {
        std::env::set_var("YANSHI_MODELS_CONFIG", &models);
        std::env::set_var("YANSHI_RUN_DIR", &runs);
      }
      yanshi_llm::Yanshi::init_with_config(models.to_str().expect("utf-8"))
        .await
        .expect("mock registry loads");
      runs
    })
    .await
    .clone()
}

async fn fresh_state() -> Option<AppState> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  Some(AppState::new(db))
}

async fn get(
  state: &AppState,
  uri: &str,
  tenant: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
  let response = create_router(state.clone())
    .oneshot(
      Request::builder()
        .uri(uri)
        .header(TENANT_HEADER, tenant)
        .body(Body::empty())
        .expect("request"),
    )
    .await
    .expect("response");
  let status = response.status();
  let headers = response.headers().clone();
  let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
    .await
    .expect("body");
  (status, headers, bytes.to_vec())
}

#[tokio::test]
async fn tts_run_registers_a_downloadable_artifact_scoped_to_its_tenant() {
  let Some(state) = fresh_state().await else {
    eprintln!("skipping tts_run_registers_a_downloadable_artifact — set YANSHI_DATABASE_TEST_URL");
    return;
  };
  let runs_root = install_env().await;
  let tenant = format!("tenant-artifacts-{}", Uuid::new_v4());

  let response = create_router(state.clone())
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/runs")
        .header(CONTENT_TYPE, "application/json")
        .header(TENANT_HEADER, tenant.as_str())
        .body(Body::from(json!({ "workflow": TTS_WORKFLOW }).to_string()))
        .expect("request"),
    )
    .await
    .expect("response");
  assert_eq!(response.status(), StatusCode::OK);
  let body: Value = serde_json::from_slice(
    &axum::body::to_bytes(response.into_body(), 4096)
      .await
      .expect("body"),
  )
  .expect("json");
  let run_id: Uuid = body["run_id"]
    .as_str()
    .expect("run_id")
    .parse()
    .expect("uuid");

  let mut status = String::new();
  for _ in 0..120 {
    sleep(Duration::from_millis(25)).await;
    let row = state
      .repos
      .runs
      .get(run_id)
      .await
      .expect("get")
      .expect("row");
    status = row.status.clone();
    if status == "succeeded" || status == "failed" {
      break;
    }
  }
  assert_eq!(
    status, "succeeded",
    "tts run must succeed under vendor: mock"
  );

  // List.
  let (code, _, bytes) = get(&state, &format!("/v1/runs/{run_id}/artifacts"), &tenant).await;
  assert_eq!(code, StatusCode::OK);
  let listing: Value = serde_json::from_slice(&bytes).expect("json");
  let artifacts = listing["artifacts"].as_array().expect("artifacts array");
  assert_eq!(artifacts.len(), 1, "one WAV expected: {listing}");
  let artifact = &artifacts[0];
  assert_eq!(artifact["node_id"], "speak");
  assert_eq!(artifact["name"], "1.wav");
  assert_eq!(artifact["mime_type"], "audio/wav");
  let size = artifact["size_bytes"].as_i64().expect("size");
  assert!(size > 44, "WAV must have a header plus samples, got {size}");
  let sha = artifact["sha256"].as_str().expect("sha256").to_string();
  assert_eq!(sha.len(), 64);
  let artifact_id = artifact["id"].as_str().expect("id").to_string();

  // Download: bytes match the listing's digest and the file on disk.
  let (code, headers, bytes) = get(
    &state,
    &format!("/v1/runs/{run_id}/artifacts/{artifact_id}"),
    &tenant,
  )
  .await;
  assert_eq!(code, StatusCode::OK);
  assert_eq!(
    headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()),
    Some("audio/wav")
  );
  assert_eq!(
    headers
      .get(axum::http::header::CONTENT_LENGTH)
      .and_then(|v| v.to_str().ok())
      .and_then(|v| v.parse::<i64>().ok()),
    Some(size)
  );
  assert!(
    headers
      .get(axum::http::header::CONTENT_DISPOSITION)
      .and_then(|v| v.to_str().ok())
      .is_some_and(|v| v.contains("1.wav"))
  );
  assert_eq!(&bytes[0..4], b"RIFF");
  assert_eq!(format!("{:x}", Sha256::digest(&bytes)), sha);
  let on_disk = std::fs::read(
    runs_root
      .join(run_id.to_string())
      .join("artifacts")
      .join("speak")
      .join("1.wav"),
  )
  .expect("artifact file under the run dir");
  assert_eq!(on_disk, bytes);

  // Another tenant sees neither the listing nor the file.
  let other = format!("tenant-other-{}", Uuid::new_v4());
  let (code, _, _) = get(&state, &format!("/v1/runs/{run_id}/artifacts"), &other).await;
  assert_eq!(code, StatusCode::NOT_FOUND);
  let (code, _, _) = get(
    &state,
    &format!("/v1/runs/{run_id}/artifacts/{artifact_id}"),
    &other,
  )
  .await;
  assert_eq!(code, StatusCode::NOT_FOUND);

  // An unknown artifact id under the right run and tenant is also 404.
  let (code, _, _) = get(
    &state,
    &format!("/v1/runs/{run_id}/artifacts/{}", Uuid::new_v4()),
    &tenant,
  )
  .await;
  assert_eq!(code, StatusCode::NOT_FOUND);
}
