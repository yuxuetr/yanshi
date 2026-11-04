//! Y2.4 acceptance (CLI): `workflow run --checkpoint` then
//! `workflow resume <run_id> <file>` re-runs only what did not finish.
//!
//! `speak` (a billed `tts` node under `vendor: mock`) succeeds, `note`
//! reads a file that does not exist yet and fails. After the file is
//! created, resume finishes the run; `speak` is not run again (its
//! artifact dir still holds only `1.wav` — a re-run would add `2.wav`).

use assert_cmd::Command;
use predicates::prelude::*;
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
      path: "{note}"
      allowed_paths: ["{root}"]
"#,
      note = Self::note_path(work.path()).display(),
      root = work.path().display(),
    );
    fs::write(work.path().join("flow.yml"), workflow).expect("workflow");
    Self { home, work }
  }

  fn note_path(work: &Path) -> PathBuf {
    work.join("note.txt")
  }

  fn runs(&self) -> PathBuf {
    self.work.path().join("runs")
  }

  fn yanshi(&self) -> Command {
    let mut cmd = Command::cargo_bin("yanshi").expect("binary");
    cmd
      .env("HOME", self.home.path())
      .env(
        "YANSHI_MODELS_CONFIG",
        self.home.path().join(".yanshi").join("models.yml"),
      )
      .env("YANSHI_TRACE_DIR", self.work.path().join("traces"))
      .env_remove("YANSHI_SERVER_URL")
      .env_remove("YANSHI_RUN_DIR");
    cmd
  }
}

fn run_id_from(stdout: &str) -> String {
  stdout
    .lines()
    .find_map(|line| line.split("yanshi workflow resume ").nth(1))
    .and_then(|rest| rest.split_whitespace().next())
    .expect("run prints the resume command")
    .to_string()
}

#[test]
fn resume_finishes_a_failed_run_without_rerunning_finished_nodes() {
  let fixture = Fixture::new();
  let flow = fixture.work.path().join("flow.yml");

  let first = fixture
    .yanshi()
    .args(["workflow", "run"])
    .arg(&flow)
    .arg("--run-dir")
    .arg(fixture.runs())
    .arg("--checkpoint")
    .assert()
    .failure();
  let stdout = String::from_utf8_lossy(&first.get_output().stdout).to_string();
  let run_id = run_id_from(&stdout);
  let speak_dir = fixture
    .runs()
    .join(&run_id)
    .join("artifacts")
    .join("speak");
  assert!(speak_dir.join("1.wav").exists(), "speak ran before the failure");

  fs::write(Fixture::note_path(fixture.work.path()), "now it exists").expect("note");

  fixture
    .yanshi()
    .args(["workflow", "resume", &run_id])
    .arg(&flow)
    .arg("--run-dir")
    .arg(fixture.runs())
    .assert()
    .success()
    .stdout(predicate::str::contains("resumed to completion"))
    .stdout(predicate::str::contains("now it exists"))
    .stdout(predicate::str::contains("📦 Artifacts (1):"));

  let mut produced: Vec<String> = fs::read_dir(&speak_dir)
    .expect("speak dir")
    .flatten()
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .collect();
  produced.sort();
  assert_eq!(produced, vec!["1.wav"], "speak must not run again");
}

#[test]
fn resume_without_a_checkpoint_says_so() {
  let fixture = Fixture::new();
  fixture
    .yanshi()
    .args(["workflow", "resume", "no-such-run"])
    .arg(fixture.work.path().join("flow.yml"))
    .arg("--run-dir")
    .arg(fixture.runs())
    .assert()
    .failure()
    .stderr(predicate::str::contains("no checkpoint for run 'no-such-run'"));
}

#[test]
fn checkpoint_is_rejected_in_server_mode() {
  let fixture = Fixture::new();
  fixture
    .yanshi()
    .args(["workflow", "run"])
    .arg(fixture.work.path().join("flow.yml"))
    .args(["--server", "http://127.0.0.1:9", "--checkpoint"])
    .assert()
    .failure()
    .stderr(predicate::str::contains("--checkpoint is local-only"));
}
