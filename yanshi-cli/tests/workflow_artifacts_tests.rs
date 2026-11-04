//! Y1.4 acceptance: a local `workflow run` that produces media prints an
//! artifact table, and `--output-dir` copies each artifact to
//! `<dir>/<node_id>/<file>` with an `artifacts.json` manifest whose
//! digests match the copied bytes. Runs a `tts` node under the
//! registry's `vendor: mock`, so no keys are needed.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

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
name: cli artifacts
nodes:
  - id: speak
    type: tts
    parameters:
      model: mock-tts
      voice: narrator
      input_template: "a few words of narration for the CLI artifact test"
"#;

struct Fixture {
  home: TempDir,
  work: TempDir,
}

impl Fixture {
  fn new() -> Self {
    let home = TempDir::new().expect("home");
    let work = TempDir::new().expect("work");
    let config_dir = home.path().join(".yanshi");
    fs::create_dir_all(&config_dir).expect("config dir");
    fs::write(config_dir.join("models.yml"), MOCK_REGISTRY).expect("registry");
    fs::write(work.path().join("tts.yml"), TTS_WORKFLOW).expect("workflow");
    Self { home, work }
  }

  fn runs(&self) -> PathBuf {
    self.work.path().join("runs")
  }

  /// `workflow run <file>` with the mock registry and isolated dirs, but
  /// no `--run-dir` (server mode rejects that flag on its own).
  fn bare_command(&self) -> Command {
    let mut cmd = Command::cargo_bin("yanshi").expect("binary");
    cmd
      .env("HOME", self.home.path())
      .env(
        "YANSHI_MODELS_CONFIG",
        self.home.path().join(".yanshi").join("models.yml"),
      )
      .env("YANSHI_TRACE_DIR", self.work.path().join("traces"))
      .env_remove("YANSHI_SERVER_URL")
      .env_remove("YANSHI_RUN_DIR")
      .args(["workflow", "run"])
      .arg(self.work.path().join("tts.yml"));
    cmd
  }

  fn command(&self) -> Command {
    let mut cmd = self.bare_command();
    cmd.arg("--run-dir").arg(self.runs());
    cmd
  }
}

fn sha256_hex(path: &Path) -> String {
  format!("{:x}", Sha256::digest(fs::read(path).expect("read")))
}

#[test]
fn run_prints_the_artifact_table_and_copies_artifacts_with_a_manifest() {
  let fixture = Fixture::new();
  let out = fixture.work.path().join("out");

  fixture
    .command()
    .arg("--output-dir")
    .arg(&out)
    .assert()
    .success()
    .stdout(predicate::str::contains("📦 Artifacts (1):"))
    .stdout(predicate::str::contains("speak_output"))
    .stdout(predicate::str::contains("audio/wav"))
    .stdout(predicate::str::contains("Copied 1 artifact(s)"));

  let copied = out.join("speak").join("1.wav");
  let bytes = fs::read(&copied).expect("copied artifact");
  assert_eq!(&bytes[0..4], b"RIFF");

  let manifest: Value =
    serde_json::from_slice(&fs::read(out.join("artifacts.json")).expect("manifest"))
      .expect("manifest json");
  let artifacts = manifest["artifacts"].as_array().expect("artifacts array");
  assert_eq!(artifacts.len(), 1, "{manifest}");
  let entry = &artifacts[0];
  assert_eq!(entry["node_id"], "speak");
  assert_eq!(entry["output_key"], "speak_output");
  assert_eq!(entry["mime_type"], "audio/wav");
  assert_eq!(entry["path"], "speak/1.wav");
  assert_eq!(entry["size_bytes"], bytes.len() as u64);
  assert_eq!(entry["sha256"], sha256_hex(&copied));

  // The source is the run's own artifact, under the run dir, and the
  // manifest's run id names that run dir.
  let run_id = manifest["run_id"].as_str().expect("run id");
  let source = PathBuf::from(entry["source"].as_str().expect("source"));
  assert_eq!(
    source,
    fixture
      .runs()
      .join(run_id)
      .join("artifacts")
      .join("speak")
      .join("1.wav")
  );
  assert_eq!(fs::read(&source).expect("source"), bytes);
}

#[test]
fn run_without_output_dir_lists_artifacts_but_copies_nothing() {
  let fixture = Fixture::new();
  fixture
    .command()
    .assert()
    .success()
    .stdout(predicate::str::contains("📦 Artifacts (1):"))
    .stdout(predicate::str::contains("Copied").not());
  assert!(!fixture.work.path().join("out").exists());
}

#[test]
fn output_dir_is_rejected_in_server_mode() {
  let fixture = Fixture::new();
  fixture
    .bare_command()
    .args(["--server", "http://127.0.0.1:9", "--output-dir", "out"])
    .assert()
    .failure()
    .stderr(predicate::str::contains("--output-dir is local-only"));
}
