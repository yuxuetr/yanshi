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

use std::future::Future;
use std::path::PathBuf;

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
}
