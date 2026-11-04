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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use yanshi_core::{error::YanshiError, value::FlowValue};

/// The writer is shared with the tool-tier render nodes (Y3.1), so it
/// lives in `yanshi-nodes`; re-exported here so existing paths keep working.
pub use yanshi_nodes::common::artifacts::{artifact_dir, extension_for_mime, write_artifact};

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

#[cfg(test)]
mod tests {
  use super::*;

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
