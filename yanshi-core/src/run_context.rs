//! Per-run execution context visible to nodes (Y1.1).
//!
//! `AsyncNode::execute` takes only its inputs; nothing told a node which
//! run it belongs to, so a modality node had nowhere to put bytes except
//! the state pool (as base64). The executor now scopes every node
//! execution in a task-local carrying the run's **artifact root** — the
//! run directory it already creates for per-step outputs — and a node
//! reads it with [`current_artifact_root`] to write files under
//! `<root>/artifacts/<node_id>/`.
//!
//! Why a task-local rather than a new `execute` parameter: every
//! `AsyncNode` implementation in and outside this workspace keeps
//! compiling, and nodes that never produce files never learn the concept
//! exists. The value is scoped per node future, so it is correct under
//! the concurrent scheduler's `FuturesUnordered` as well as the serial
//! loop. It does not cross `tokio::spawn`; the Map executor re-enters the
//! scope inside each spawned sub-flow task explicitly.
//!
//! Nesting rule: a Map / While sub-flow runs its own executor with its own
//! run directory, but its nodes should file artifacts under the *parent*
//! run, so [`with_artifact_root`] keeps an already-set root instead of
//! replacing it.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::async_node::AsyncNodeResult;
use crate::value::FlowValue;

tokio::task_local! {
  static ARTIFACT_ROOT: PathBuf;
}

/// The artifact root of the run this node is executing in, if the
/// executor set one (it always does inside `Flow::run`; direct
/// `node.execute()` calls outside a flow see `None`).
pub fn current_artifact_root() -> Option<PathBuf> {
  ARTIFACT_ROOT.try_with(|root| root.clone()).ok()
}

/// Run `fut` with `root` as the artifact root, unless one is already set
/// (a sub-flow inherits its parent's root — see the module docs).
pub async fn with_artifact_root<F: Future>(root: PathBuf, fut: F) -> F::Output {
  if current_artifact_root().is_some() {
    fut.await
  } else {
    ARTIFACT_ROOT.scope(root, fut).await
  }
}

/// Re-enter a captured root inside a spawned task (task-locals do not
/// cross `tokio::spawn`). `None` runs `fut` unchanged.
pub async fn with_inherited_artifact_root<F: Future>(root: Option<PathBuf>, fut: F) -> F::Output {
  match root {
    Some(root) => ARTIFACT_ROOT.scope(root, fut).await,
    None => fut.await,
  }
}

/// One `FlowValue::File` a finished run left in its state pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutput {
  pub node_id: String,
  pub output_key: String,
  pub path: PathBuf,
  pub mime_type: Option<String>,
}

/// The run's deliverables: every top-level `File` value in `state` whose
/// path sits under `base_dir` (the run base dir), ordered by (node id,
/// output key). The gateway registers these (Y1.3) and the CLI copies
/// them out (Y1.4), so both apply the same rule.
///
/// A `File` can also name a sandbox-allowed path outside the run (`file`
/// node `read_bytes`); handing that out as an artifact would turn a
/// download route into a read primitive over whatever the workflow's
/// policy allowed, so it is logged and skipped. Map items' outputs are
/// nested JSON inside the parent's `results` and are not walked.
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
          "File output outside the run base dir is not treated as an artifact"
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

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn root_is_visible_inside_scope_and_absent_outside() {
    assert_eq!(current_artifact_root(), None);
    let seen = with_artifact_root(PathBuf::from("/run/a"), async { current_artifact_root() }).await;
    assert_eq!(seen, Some(PathBuf::from("/run/a")));
    assert_eq!(current_artifact_root(), None);
  }

  #[tokio::test]
  async fn nested_scope_keeps_the_outer_root() {
    let seen = with_artifact_root(PathBuf::from("/run/parent"), async {
      with_artifact_root(PathBuf::from("/run/child"), async {
        current_artifact_root()
      })
      .await
    })
    .await;
    assert_eq!(seen, Some(PathBuf::from("/run/parent")));
  }

  #[tokio::test]
  async fn spawned_task_needs_explicit_reentry() {
    let (direct, reentered) = with_artifact_root(PathBuf::from("/run/p"), async {
      let captured = current_artifact_root();
      let direct = tokio::spawn(async { current_artifact_root() })
        .await
        .expect("task joins");
      let reentered = tokio::spawn(with_inherited_artifact_root(captured, async {
        current_artifact_root()
      }))
      .await
      .expect("task joins");
      (direct, reentered)
    })
    .await;
    assert_eq!(direct, None);
    assert_eq!(reentered, Some(PathBuf::from("/run/p")));
  }

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
        (
          "note".to_string(),
          FlowValue::Json(serde_json::json!("text")),
        ),
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
    state.insert("broken".into(), Err(crate::error::YanshiError::NodeSkipped));

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
}
