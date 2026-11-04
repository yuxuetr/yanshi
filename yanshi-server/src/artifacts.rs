//! Run artifacts: registration at run end and the two download routes
//! (Y1.3, `docs/RFC_CONTENT_STUDIO.md` §4.2).
//!
//! Nodes write media under `<run_dir>/artifacts/<node_id>/` and put a
//! `FlowValue::File` in the state pool (Y1.1). When `flow_execute`
//! finishes, [`register_run_artifacts`] walks the final state pool and
//! records every such file in the `artifacts` table with its size and
//! SHA-256, so a client can list what a run produced and fetch it:
//!
//! ```text
//! GET /v1/runs/{id}/artifacts                → { run_id, artifacts: [ArtifactSummary] }
//! GET /v1/runs/{id}/artifacts/{artifact_id}  → the bytes, Content-Type from the stored MIME
//! ```
//!
//! Only files under the run base directory are registered. A `File`
//! value can also point at a sandbox-allowed path outside it (`file`
//! node `read_bytes`); serving those would turn the artifact route into
//! a read primitive over whatever the workflow's policy allowed, so they
//! are logged and skipped. Map items' outputs are nested JSON inside
//! their parent node's `results` and are not walked here — a later node
//! that surfaces such a file as its own `File` output gets registered.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::{
  Extension, Json,
  body::Body,
  extract::{Path as RoutePath, State},
  http::{HeaderValue, header},
  response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt as _;
use uuid::Uuid;
use yanshi_core::{async_node::AsyncNodeResult, value::FlowValue};
use yanshi_db::{ArtifactRepo, NewArtifact, Repositories, RunRepo};

use crate::{AppState, error::ApiError, tenant::TenantId};

/// One `FlowValue::File` found in a finished run's state pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutput {
  pub node_id: String,
  pub output_key: String,
  pub path: PathBuf,
  pub mime_type: Option<String>,
}

/// Every `File` value in `state` whose path sits under `base_dir`, in a
/// deterministic (node id, output key) order. Pure; the DB write is
/// [`register_run_artifacts`].
pub fn collect_file_outputs(
  state: &HashMap<String, AsyncNodeResult>,
  base_dir: &Path,
) -> Vec<FileOutput> {
  let base = base_dir
    .canonicalize()
    .unwrap_or_else(|_| base_dir.to_path_buf());
  let mut node_ids: Vec<&String> = state.keys().collect();
  node_ids.sort();
  let mut found = Vec::new();
  for node_id in node_ids {
    let Ok(outputs) = &state[node_id] else {
      continue;
    };
    let mut keys: Vec<&String> = outputs.keys().collect();
    keys.sort();
    for key in keys {
      let FlowValue::File { path, mime_type } = &outputs[key] else {
        continue;
      };
      let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
      if !resolved.starts_with(&base) {
        tracing::warn!(
          node_id = %node_id,
          output = %key,
          path = %path.display(),
          "File output outside the run base dir is not registered as an artifact"
        );
        continue;
      }
      found.push(FileOutput {
        node_id: node_id.clone(),
        output_key: key.clone(),
        path: path.clone(),
        mime_type: mime_type.clone(),
      });
    }
  }
  found
}

/// Size and hex SHA-256 of a file, streamed in 64 KiB reads so a large
/// video is never fully buffered.
async fn size_and_sha256(path: &Path) -> std::io::Result<(u64, String)> {
  let mut file = tokio::fs::File::open(path).await?;
  let mut hasher = Sha256::new();
  let mut buf = vec![0u8; 64 * 1024];
  let mut total: u64 = 0;
  loop {
    let n = file.read(&mut buf).await?;
    if n == 0 {
      break;
    }
    hasher.update(&buf[..n]);
    total += n as u64;
  }
  Ok((total, format!("{:x}", hasher.finalize())))
}

/// Register the run's `File` outputs in the `artifacts` table. A file
/// that cannot be read (deleted between the node and this pass) is
/// logged and skipped; a database failure is returned so the run is
/// marked failed rather than silently missing its deliverables.
pub async fn register_run_artifacts(
  repos: &Repositories,
  run_id: Uuid,
  tenant_id: &str,
  state: &HashMap<String, AsyncNodeResult>,
  base_dir: &Path,
) -> Result<usize, yanshi_db::DbError> {
  let mut registered = 0;
  for output in collect_file_outputs(state, base_dir) {
    let (size_bytes, sha256) = match size_and_sha256(&output.path).await {
      Ok(v) => v,
      Err(err) => {
        tracing::warn!(
          run_id = %run_id,
          node_id = %output.node_id,
          path = %output.path.display(),
          error = %err,
          "artifact file unreadable at run end; not registered"
        );
        continue;
      }
    };
    let name = output
      .path
      .file_name()
      .map(|n| n.to_string_lossy().to_string())
      .unwrap_or_else(|| output.output_key.clone());
    repos
      .artifacts
      .create(NewArtifact {
        id: Uuid::new_v4(),
        run_id,
        node_id: output.node_id,
        name,
        path_or_url: output.path.to_string_lossy().to_string(),
        mime_type: output.mime_type,
        tenant_id: Some(tenant_id.to_string()),
        size_bytes: Some(size_bytes as i64),
        sha256: Some(sha256),
      })
      .await?;
    registered += 1;
  }
  if registered > 0 {
    tracing::info!(run_id = %run_id, artifacts = registered, "run artifacts registered");
  }
  Ok(registered)
}

// ── Routes ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ArtifactSummary {
  pub id: Uuid,
  pub node_id: String,
  pub name: String,
  pub mime_type: Option<String>,
  pub size_bytes: Option<i64>,
  pub sha256: Option<String>,
  pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ListArtifactsResponse {
  pub run_id: Uuid,
  pub artifacts: Vec<ArtifactSummary>,
}

/// The run must exist under the caller's tenant; anything else is a 404
/// (never a 403, so tenants cannot probe each other's run ids).
async fn run_for_tenant(state: &AppState, id: Uuid, tenant: &TenantId) -> Result<(), ApiError> {
  let run = state
    .repos
    .runs
    .get(id)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("run {id} not found")))?;
  if run.tenant_id != tenant.as_str() {
    return Err(ApiError::NotFound(format!("run {id} not found")));
  }
  Ok(())
}

pub async fn list_run_artifacts(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  RoutePath(id): RoutePath<Uuid>,
) -> Result<Json<ListArtifactsResponse>, ApiError> {
  run_for_tenant(&state, id, &tenant).await?;
  let artifacts = state
    .repos
    .artifacts
    .list_for_run(id)
    .await?
    .into_iter()
    .map(|a| ArtifactSummary {
      id: a.id,
      node_id: a.node_id,
      name: a.name,
      mime_type: a.mime_type,
      size_bytes: a.size_bytes,
      sha256: a.sha256,
      created_at: a.created_at,
    })
    .collect();
  Ok(Json(ListArtifactsResponse {
    run_id: id,
    artifacts,
  }))
}

/// Stream the artifact's bytes. `Content-Type` is the MIME recorded at
/// registration (`application/octet-stream` when unknown); the file name
/// goes out as an attachment with any quote / control characters
/// stripped so it cannot break the header.
pub async fn download_run_artifact(
  State(state): State<AppState>,
  Extension(tenant): Extension<TenantId>,
  RoutePath((id, artifact_id)): RoutePath<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
  run_for_tenant(&state, id, &tenant).await?;
  let artifact = state
    .repos
    .artifacts
    .get(artifact_id)
    .await?
    .filter(|a| a.run_id == id && a.tenant_id == tenant.as_str())
    .ok_or_else(|| ApiError::NotFound(format!("artifact {artifact_id} not found")))?;

  let path = PathBuf::from(&artifact.path_or_url);
  let file = tokio::fs::File::open(&path).await.map_err(|err| {
    tracing::warn!(artifact_id = %artifact_id, path = %path.display(), error = %err, "artifact file missing");
    ApiError::NotFound(format!(
      "artifact {artifact_id} is registered but its file is no longer on disk"
    ))
  })?;
  let len = file.metadata().await.map(|m| m.len()).ok();

  let mime = artifact
    .mime_type
    .as_deref()
    .filter(|m| !m.is_empty())
    .unwrap_or("application/octet-stream");
  let safe_name: String = artifact
    .name
    .chars()
    .filter(|c| !c.is_control() && *c != '"' && *c != '\\' && *c != '/')
    .collect();
  let disposition = format!("attachment; filename=\"{safe_name}\"");

  let mut response = Body::from_stream(tokio_util::io::ReaderStream::new(file)).into_response();
  let headers = response.headers_mut();
  headers.insert(
    header::CONTENT_TYPE,
    HeaderValue::from_str(mime)
      .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
  );
  if let Some(len) = len {
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
  }
  if let Ok(value) = HeaderValue::from_str(&disposition) {
    headers.insert(header::CONTENT_DISPOSITION, value);
  }
  Ok(response)
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn file(path: &Path, mime: &str) -> FlowValue {
    FlowValue::File {
      path: path.to_path_buf(),
      mime_type: Some(mime.to_string()),
    }
  }

  #[test]
  fn collects_files_under_the_base_dir_in_stable_order_and_skips_the_rest() {
    let base = tempfile::TempDir::new().expect("tempdir");
    let outside = tempfile::TempDir::new().expect("tempdir");
    let run = base.path().join("run-1").join("artifacts");
    std::fs::create_dir_all(run.join("speak")).expect("mkdir");
    std::fs::create_dir_all(run.join("draw")).expect("mkdir");
    let wav = run.join("speak").join("1.wav");
    let png = run.join("draw").join("1.png");
    let secret = outside.path().join("secret.txt");
    for p in [&wav, &png, &secret] {
      std::fs::write(p, b"x").expect("write");
    }

    let mut state: HashMap<String, AsyncNodeResult> = HashMap::new();
    state.insert(
      "speak".into(),
      Ok(HashMap::from([
        ("speak_output".to_string(), file(&wav, "audio/wav")),
        ("note".to_string(), FlowValue::Json(json!("text"))),
      ])),
    );
    state.insert(
      "draw".into(),
      Ok(HashMap::from([(
        "draw_image".to_string(),
        file(&png, "image/png"),
      )])),
    );
    state.insert(
      "leak".into(),
      Ok(HashMap::from([(
        "content".to_string(),
        file(&secret, "text/plain"),
      )])),
    );
    state.insert(
      "broken".into(),
      Err(yanshi_core::error::YanshiError::NodeSkipped),
    );

    let found = collect_file_outputs(&state, base.path());
    let summary: Vec<(String, String)> = found
      .iter()
      .map(|f| (f.node_id.clone(), f.output_key.clone()))
      .collect();
    assert_eq!(
      summary,
      vec![
        ("draw".to_string(), "draw_image".to_string()),
        ("speak".to_string(), "speak_output".to_string()),
      ],
      "outside-base and non-File outputs must be skipped; order is by node id"
    );
    assert_eq!(found[1].mime_type.as_deref(), Some("audio/wav"));
  }

  #[tokio::test]
  async fn size_and_digest_match_the_file() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("a.bin");
    std::fs::write(&path, b"hello artifact").expect("write");
    let (size, digest) = size_and_sha256(&path).await.expect("hash");
    assert_eq!(size, 14);
    assert_eq!(digest, format!("{:x}", Sha256::digest(b"hello artifact")),);
  }
}
