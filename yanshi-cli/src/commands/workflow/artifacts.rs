//! What a local `workflow run` produced (Y1.4): a table on stdout and,
//! with `--output-dir`, a copy of every artifact plus an
//! `artifacts.json` manifest.
//!
//! Artifacts are the run's `FlowValue::File` outputs under the run base
//! dir — the same rule the gateway registers by
//! ([`yanshi_core::run_context::collect_file_outputs`]).

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use yanshi_core::run_context::FileOutput;

/// File name of the manifest written next to the copied artifacts.
pub const MANIFEST_FILE: &str = "artifacts.json";

/// One manifest row. `path` is relative to the output dir.
#[derive(Debug, Serialize)]
pub struct CopiedArtifact {
  pub node_id: String,
  pub output_key: String,
  pub mime_type: Option<String>,
  pub size_bytes: u64,
  pub sha256: String,
  pub path: String,
  pub source: PathBuf,
}

#[derive(Debug, Serialize)]
struct Manifest<'a> {
  run_id: &'a str,
  artifacts: &'a [CopiedArtifact],
}

/// Print one line per artifact: node, output key, MIME, size, path.
pub fn print_table(artifacts: &[FileOutput]) {
  if artifacts.is_empty() {
    return;
  }
  println!("\n📦 Artifacts ({}):", artifacts.len());
  let node_width = artifacts
    .iter()
    .map(|a| a.node_id.len())
    .max()
    .unwrap_or(0)
    .max("NODE".len());
  let key_width = artifacts
    .iter()
    .map(|a| a.output_key.len())
    .max()
    .unwrap_or(0)
    .max("OUTPUT".len());
  println!(
    "  {:<node_width$}  {:<key_width$}  {:<16}  {:>10}  PATH",
    "NODE", "OUTPUT", "MIME", "SIZE"
  );
  for artifact in artifacts {
    let size = fs::metadata(&artifact.path)
      .map(|m| m.len().to_string())
      .unwrap_or_else(|_| "missing".to_string());
    println!(
      "  {:<node_width$}  {:<key_width$}  {:<16}  {:>10}  {}",
      artifact.node_id,
      artifact.output_key,
      artifact.mime_type.as_deref().unwrap_or("-"),
      size,
      artifact.path.display()
    );
  }
}

/// Copy every artifact to `<output_dir>/<node_id>/<file name>` and write
/// the manifest. Node ids come from the workflow file, so one that is not
/// a single plain path component is refused rather than allowed to steer
/// the copy outside `output_dir`; two artifacts landing on the same name
/// are refused rather than silently overwritten.
pub fn copy_to_output_dir(
  artifacts: &[FileOutput],
  output_dir: &Path,
  run_id: &str,
) -> Result<Vec<CopiedArtifact>> {
  fs::create_dir_all(output_dir)
    .with_context(|| format!("Failed to create output dir {}", output_dir.display()))?;
  let mut taken = HashSet::new();
  let mut copied = Vec::with_capacity(artifacts.len());
  for artifact in artifacts {
    if !is_single_component(&artifact.node_id) {
      bail!(
        "node id '{}' cannot be used as a directory name under --output-dir",
        artifact.node_id
      );
    }
    let file_name = artifact
      .path
      .file_name()
      .with_context(|| format!("artifact {} has no file name", artifact.path.display()))?;
    let relative = Path::new(&artifact.node_id).join(file_name);
    if !taken.insert(relative.clone()) {
      bail!(
        "two artifacts would both be copied to {}",
        relative.display()
      );
    }
    let dest = output_dir.join(&relative);
    fs::create_dir_all(output_dir.join(&artifact.node_id))
      .with_context(|| format!("Failed to create {}", output_dir.display()))?;
    fs::copy(&artifact.path, &dest).with_context(|| {
      format!(
        "Failed to copy {} to {}",
        artifact.path.display(),
        dest.display()
      )
    })?;
    let (size_bytes, sha256) =
      size_and_sha256(&dest).with_context(|| format!("Failed to hash {}", dest.display()))?;
    copied.push(CopiedArtifact {
      node_id: artifact.node_id.clone(),
      output_key: artifact.output_key.clone(),
      mime_type: artifact.mime_type.clone(),
      size_bytes,
      sha256,
      path: relative.to_string_lossy().replace('\\', "/"),
      source: artifact.path.clone(),
    });
  }
  let manifest = serde_json::to_string_pretty(&Manifest {
    run_id,
    artifacts: &copied,
  })
  .context("Failed to serialize the artifact manifest")?;
  let manifest_path = output_dir.join(MANIFEST_FILE);
  fs::write(&manifest_path, manifest)
    .with_context(|| format!("Failed to write {}", manifest_path.display()))?;
  Ok(copied)
}

fn is_single_component(name: &str) -> bool {
  let mut components = Path::new(name).components();
  matches!(
    (components.next(), components.next()),
    (Some(Component::Normal(_)), None)
  )
}

fn size_and_sha256(path: &Path) -> io::Result<(u64, String)> {
  let mut file = fs::File::open(path)?;
  let mut hasher = Sha256::new();
  let size = io::copy(&mut file, &mut hasher)?;
  Ok((size, format!("{:x}", hasher.finalize())))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn output(dir: &Path, node: &str, name: &str, body: &[u8]) -> FileOutput {
    let path = dir.join(node).join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, body).expect("write");
    FileOutput {
      node_id: node.to_string(),
      output_key: format!("{node}_output"),
      path,
      mime_type: Some("audio/wav".to_string()),
    }
  }

  #[test]
  fn copies_by_node_and_writes_a_manifest_with_digests() {
    let run = tempfile::TempDir::new().expect("tempdir");
    let out = tempfile::TempDir::new().expect("tempdir");
    let artifacts = vec![
      output(run.path(), "a", "1.wav", b"abc"),
      output(run.path(), "b", "1.wav", b"xyz"),
    ];
    let copied = copy_to_output_dir(&artifacts, out.path(), "run-1").expect("copy");
    assert_eq!(fs::read(out.path().join("a/1.wav")).expect("a"), b"abc");
    assert_eq!(fs::read(out.path().join("b/1.wav")).expect("b"), b"xyz");
    assert_eq!(copied[0].path, "a/1.wav");
    assert_eq!(
      copied[0].sha256,
      "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let manifest: serde_json::Value =
      serde_json::from_slice(&fs::read(out.path().join(MANIFEST_FILE)).expect("manifest"))
        .expect("json");
    assert_eq!(manifest["run_id"], "run-1");
    assert_eq!(manifest["artifacts"][1]["size_bytes"], 3);
  }

  #[test]
  fn refuses_node_ids_that_are_not_one_path_component() {
    let run = tempfile::TempDir::new().expect("tempdir");
    let out = tempfile::TempDir::new().expect("tempdir");
    for bad in ["..", "a/b", "/abs", ""] {
      let mut artifact = output(run.path(), "ok", "1.wav", b"x");
      artifact.node_id = bad.to_string();
      assert!(
        copy_to_output_dir(&[artifact], out.path(), "r").is_err(),
        "node id {bad:?} must be refused"
      );
    }
  }

  #[test]
  fn refuses_two_artifacts_with_the_same_destination() {
    let run = tempfile::TempDir::new().expect("tempdir");
    let out = tempfile::TempDir::new().expect("tempdir");
    let first = output(run.path(), "a", "1.wav", b"x");
    let mut second = output(run.path(), "c", "1.wav", b"y");
    second.node_id = "a".to_string();
    assert!(copy_to_output_dir(&[first, second], out.path(), "r").is_err());
  }
}
