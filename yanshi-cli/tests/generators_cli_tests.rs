//! Y4.1 acceptance: `yanshi workflow run examples/generators/report.yml`
//! under a mock model writes the report's files to `--output-dir`.
//!
//! - CI: pasted `text`, `pdf=false` → `.md` and `.html`.
//! - `#[ignore]`d: the same with the PDF (needs pandoc + typst), and a
//!   real `url` fetched through the http node's SSRF filter (needs
//!   network; loopback is blocked by design, so no local server).

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const MOCK_REGISTRY: &str = "models:\n  mock-model:\n    vendor: mock\n    type: text\n    model_id: mock-model\nproviders:\n  mock:\n    api_key_env: MOCK_API_KEY\n";

fn replies() -> String {
  json!([
    json!({"title": "Tides", "sections": [
      {"heading": "Cause", "brief": "why"},
      {"heading": "Rhythm", "brief": "how often"}
    ]})
    .to_string(),
    json!({"markdown": "The moon pulls the oceans.", "quotes": ["Tides are caused by the moon."]})
      .to_string(),
    json!({"markdown": "Two highs a day.", "quotes": ["High tide comes twice daily."]}).to_string(),
  ])
  .to_string()
}

struct Fixture {
  home: TempDir,
  work: TempDir,
}

impl Fixture {
  fn new() -> Self {
    let home = TempDir::new().expect("home");
    let work = TempDir::new().expect("work");
    fs::create_dir_all(home.path().join(".yanshi")).expect("dir");
    fs::write(home.path().join(".yanshi/models.yml"), MOCK_REGISTRY).expect("registry");
    Self { home, work }
  }

  fn out(&self) -> PathBuf {
    self.work.path().join("out")
  }

  fn report(&self) -> Command {
    let template = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/generators/report.yml");
    let mut cmd = Command::cargo_bin("yanshi").expect("binary");
    cmd
      .env("HOME", self.home.path())
      .env(
        "YANSHI_MODELS_CONFIG",
        self.home.path().join(".yanshi/models.yml"),
      )
      .env("YANSHI_TRACE_DIR", self.work.path().join("traces"))
      .env("YANSHI_MOCK_RESPONSES", replies())
      .env_remove("YANSHI_SERVER_URL")
      .args(["workflow", "run"])
      .arg(template)
      .arg("--run-dir")
      .arg(self.work.path().join("runs"))
      .args(["--model", "mock-model", "--input", "sections", "2"])
      .arg("--output-dir")
      .arg(self.out());
    cmd
  }

  fn produced(&self, node: &str) -> Vec<PathBuf> {
    fs::read_dir(self.out().join(node))
      .map(|entries| entries.flatten().map(|e| e.path()).collect())
      .unwrap_or_default()
  }
}

#[test]
fn report_from_text_writes_markdown_and_html() {
  let fixture = Fixture::new();
  fixture
    .report()
    .args([
      "--input",
      "text",
      "Tides are caused by the moon. High tide comes twice daily.",
    ])
    .args(["--input", "pdf", "false"])
    .assert()
    .success()
    .stdout(predicate::str::contains("Copied 2 artifact(s)"));
  let md = fixture.produced("report_md");
  assert_eq!(md.len(), 1);
  let markdown = fs::read_to_string(&md[0]).expect("md");
  assert!(
    markdown.starts_with("# Tides") && markdown.contains("[^2]:"),
    "{markdown}"
  );
  assert_eq!(fixture.produced("report_html").len(), 1);
  assert!(fixture.produced("report_pdf").is_empty());
}

#[test]
#[ignore = "needs pandoc and typst"]
fn report_from_text_also_writes_a_pdf() {
  let fixture = Fixture::new();
  fixture
    .report()
    .args([
      "--input",
      "text",
      "Tides are caused by the moon. High tide comes twice daily.",
    ])
    .assert()
    .success();
  let pdf = fixture.produced("report_pdf");
  assert_eq!(pdf.len(), 1);
  assert!(fs::read(&pdf[0]).expect("pdf").starts_with(b"%PDF"));
}

#[test]
#[ignore = "needs network access to example.com, plus pandoc and typst"]
fn report_from_a_url() {
  let fixture = Fixture::new();
  fixture
    .report()
    .args(["--input", "url", "https://example.com/"])
    .assert()
    .success();
  assert_eq!(fixture.produced("report_md").len(), 1);
  assert_eq!(fixture.produced("report_pdf").len(), 1);
}
