//! Node artifacts on disk (Y1.1, `docs/RFC_CONTENT_STUDIO.md` §4.1).
//!
//! A node that produces media or documents writes the bytes to
//! `<artifact root>/artifacts/<node_id>/<n>.<ext>` and returns
//! `FlowValue::File { path, mime_type }`, so the state pool holds a path
//! instead of the bytes. The artifact root is the run directory the
//! executor installs for the node (`yanshi_core::run_context`); a node
//! executed outside any flow falls back to a per-process temp directory.
//!
//! Shared by the modality nodes in `yanshi-nodes-ai` and the render nodes
//! here (Y3.1).

use std::path::{Path, PathBuf};

use yanshi_core::{error::YanshiError, run_context, value::FlowValue};

/// File extension for the MIME types nodes write.
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
    "text/csv" => "csv",
    "text/html" => "html",
    "text/markdown" => "md",
    "application/x-subrip" => "srt",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
    "application/epub+zip" => "epub",
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
      "no run context: node is executing outside a Flow, writing artifacts to a temp dir"
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
}
