//! Media artifacts on disk (Y1.1, `docs/RFC_CONTENT_STUDIO.md` §4.1).
//!
//! Every modality node used to hand its bytes back as a `data:` URI inside
//! `FlowValue::Json`, so a 1 MiB clip became a 1.4 MiB string in the state
//! pool, in every checkpoint, in every SSE payload. Nodes now write bytes
//! to `<artifact root>/artifacts/<node_id>/<n>.<ext>` and return
//! `FlowValue::File { path, mime_type }`; the pool holds a path.
//!
//! The artifact root is the run directory the executor installs for the
//! node (`yanshi_core::run_context`). A node executed outside any flow
//! (library callers, unit tests) has no run, so it falls back to a
//! per-process directory under the system temp dir and says so once.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use yanshi_core::{error::YanshiError, run_context, value::FlowValue};

/// How a modality node hands its bytes downstream. YAML: `output_format`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOutputFormat {
  /// Write the bytes under the run's artifact root and return
  /// `FlowValue::File` (the default since Y1.1).
  #[default]
  File,
  /// Pre-Y1.1 behaviour: base64 `data:` URI in a JSON string. Kept as an
  /// explicit opt-in for consumers that have not moved to `File` yet.
  DataUri,
}

impl ArtifactOutputFormat {
  /// Parse the YAML spelling; `None` (absent) is the default.
  pub fn parse(raw: Option<&str>) -> Result<Self, YanshiError> {
    match raw.map(str::trim) {
      None | Some("") | Some("file") => Ok(Self::File),
      Some("data_uri") => Ok(Self::DataUri),
      Some(other) => Err(YanshiError::ConfigurationError {
        message: format!("unknown output_format '{other}': expected 'file' or 'data_uri'"),
      }),
    }
  }
}

/// File extension for the MIME types the modality providers hand back.
pub fn extension_for_mime(mime: &str) -> &'static str {
  match mime.split(';').next().unwrap_or("").trim() {
    "audio/mpeg" | "audio/mp3" => "mp3",
    "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
    "audio/flac" | "audio/x-flac" => "flac",
    "audio/ogg" | "audio/opus" => "opus",
    "audio/aac" => "aac",
    "image/png" => "png",
    "image/jpeg" => "jpg",
    "image/webp" => "webp",
    "image/gif" => "gif",
    "image/svg+xml" => "svg",
    "video/mp4" => "mp4",
    "video/webm" => "webm",
    "video/quicktime" => "mov",
    "application/pdf" => "pdf",
    _ => "bin",
  }
}

/// Node ids come from workflow YAML; keep them to one safe path segment.
fn safe_segment(node_id: &str) -> String {
  let cleaned: String = node_id
    .chars()
    .map(|c| {
      if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
        c
      } else {
        '_'
      }
    })
    .collect();
  let trimmed = cleaned.trim_matches('.');
  if trimmed.is_empty() {
    "node".to_string()
  } else {
    trimmed.to_string()
  }
}

fn fallback_root() -> PathBuf {
  static WARNED: std::sync::Once = std::sync::Once::new();
  let root = std::env::temp_dir()
    .join("yanshi-artifacts")
    .join(std::process::id().to_string());
  WARNED.call_once(|| {
    tracing::warn!(
      root = %root.display(),
      "no run context: modality node is executing outside a Flow, writing artifacts to a temp dir"
    );
  });
  root
}

/// Directory this node's artifacts go in: `<root>/artifacts/<node_id>/`.
pub fn artifact_dir(node_id: &str) -> PathBuf {
  run_context::current_artifact_root()
    .unwrap_or_else(fallback_root)
    .join("artifacts")
    .join(safe_segment(node_id))
}

/// Write `bytes` as the next `<n>.<ext>` under this node's artifact
/// directory and return the `FlowValue::File` pointing at it. Slot
/// selection uses `create_new`, so concurrent Map items sharing a
/// `node_id` never clobber each other.
pub async fn write_artifact(
  node_id: &str,
  bytes: &[u8],
  mime: &str,
) -> Result<FlowValue, YanshiError> {
  let dir = artifact_dir(node_id);
  tokio::fs::create_dir_all(&dir)
    .await
    .map_err(|e| persistence(&dir, e))?;
  let ext = extension_for_mime(mime);
  let mut n: u32 = 1;
  loop {
    let path = dir.join(format!("{n}.{ext}"));
    match tokio::fs::OpenOptions::new()
      .write(true)
      .create_new(true)
      .open(&path)
      .await
    {
      Ok(mut file) => {
        use tokio::io::AsyncWriteExt as _;
        file
          .write_all(bytes)
          .await
          .map_err(|e| persistence(&path, e))?;
        file.flush().await.map_err(|e| persistence(&path, e))?;
        tracing::debug!(path = %path.display(), bytes = bytes.len(), mime, "artifact written");
        return Ok(FlowValue::File {
          path,
          mime_type: Some(mime.to_string()),
        });
      }
      Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
        n += 1;
      }
      Err(e) => return Err(persistence(&path, e)),
    }
  }
}

/// Fetch `url` (bounded by `max_bytes`) and store it as this node's next
/// artifact. The MIME type comes from the response when present, else
/// `mime_hint`.
pub async fn download_artifact(
  node_id: &str,
  url: &str,
  mime_hint: &str,
  max_bytes: u64,
) -> Result<FlowValue, YanshiError> {
  let (bytes, mime) = yanshi_llm::providers::modality::download_with_cap_default(url, max_bytes)
    .await
    .map_err(|e| YanshiError::AsyncExecutionError {
      message: format!("failed to download generated media from {url}: {e}"),
    })?;
  let mime = mime.unwrap_or_else(|| mime_hint.to_string());
  write_artifact(node_id, &bytes, &mime).await
}

/// Upper bound on a generated image fetched from a vendor URL.
pub const MAX_IMAGE_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bound on a generated video fetched from a vendor URL.
pub const MAX_VIDEO_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Turn one vendor image (`b64_json` or `url`) into the node's output
/// under `format`. Shared by `text_to_image` / `image_to_image` /
/// `image_edit`, whose vendors answer with the same shape.
pub async fn image_output(
  node_id: &str,
  image: &yanshi_llm::providers::modality::GeneratedImage,
  format: ArtifactOutputFormat,
) -> Result<FlowValue, YanshiError> {
  use base64::{Engine as _, engine::general_purpose::STANDARD};
  match (format, &image.b64_json, &image.url) {
    (ArtifactOutputFormat::File, Some(b64), _) => {
      let bytes = STANDARD
        .decode(b64)
        .map_err(|e| YanshiError::AsyncExecutionError {
          message: format!("provider returned invalid base64 image data: {e}"),
        })?;
      write_artifact(node_id, &bytes, "image/png").await
    }
    (ArtifactOutputFormat::File, None, Some(url)) => {
      download_artifact(node_id, url, "image/png", MAX_IMAGE_DOWNLOAD_BYTES).await
    }
    (ArtifactOutputFormat::DataUri, Some(b64), _) => Ok(FlowValue::Json(Value::String(format!(
      "data:image/png;base64,{b64}"
    )))),
    (ArtifactOutputFormat::DataUri, None, Some(url)) => {
      Ok(FlowValue::Json(Value::String(url.clone())))
    }
    (_, None, None) => Err(YanshiError::AsyncExecutionError {
      message: "No image data in response".to_string(),
    }),
  }
}

/// Pre-Y1.1 representation, kept behind `output_format: data_uri`.
pub fn data_uri_value(bytes: &[u8], mime: &str) -> FlowValue {
  use base64::{Engine as _, engine::general_purpose::STANDARD};
  FlowValue::Json(Value::String(format!(
    "data:{mime};base64,{}",
    STANDARD.encode(bytes)
  )))
}

fn persistence(path: &Path, e: std::io::Error) -> YanshiError {
  YanshiError::PersistenceError {
    message: format!("artifact io at {}: {e}", path.display()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn writes_sequential_slots_under_the_run_root() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let (first, second) = run_context::with_artifact_root(root.clone(), async {
      let a = write_artifact("speak", b"abc", "audio/mpeg")
        .await
        .expect("write");
      let b = write_artifact("speak", b"defg", "audio/mpeg")
        .await
        .expect("write");
      (a, b)
    })
    .await;
    let FlowValue::File { path, mime_type } = &first else {
      panic!("expected File, got {first:?}");
    };
    assert_eq!(path, &root.join("artifacts").join("speak").join("1.mp3"));
    assert_eq!(mime_type.as_deref(), Some("audio/mpeg"));
    assert_eq!(std::fs::read(path).expect("read back"), b"abc");
    let FlowValue::File { path, .. } = &second else {
      panic!("expected File, got {second:?}");
    };
    assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("2.mp3"));
  }

  #[tokio::test]
  async fn concurrent_writers_never_share_a_slot() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let paths = run_context::with_artifact_root(tmp.path().to_path_buf(), async {
      let writes = (0..8).map(|_| write_artifact("img", b"x", "image/png"));
      futures::future::join_all(writes).await
    })
    .await;
    let mut names: Vec<String> = paths
      .into_iter()
      .map(|r| match r.expect("write") {
        FlowValue::File { path, .. } => path.file_name().unwrap().to_string_lossy().to_string(),
        other => panic!("expected File, got {other:?}"),
      })
      .collect();
    names.sort();
    names.dedup();
    assert_eq!(
      names.len(),
      8,
      "eight writers must land in eight distinct slots"
    );
  }

  #[test]
  fn node_ids_are_confined_to_one_path_segment() {
    assert_eq!(safe_segment("../../etc"), "_.._etc");
    assert_eq!(safe_segment("a/b"), "a_b");
    assert_eq!(safe_segment(""), "node");
    assert_eq!(safe_segment("speak-1"), "speak-1");
  }

  #[test]
  fn output_format_parses_yaml_spellings() {
    assert_eq!(
      ArtifactOutputFormat::parse(None).unwrap(),
      ArtifactOutputFormat::File
    );
    assert_eq!(
      ArtifactOutputFormat::parse(Some("file")).unwrap(),
      ArtifactOutputFormat::File
    );
    assert_eq!(
      ArtifactOutputFormat::parse(Some("data_uri")).unwrap(),
      ArtifactOutputFormat::DataUri
    );
    assert!(ArtifactOutputFormat::parse(Some("base64")).is_err());
  }
}
