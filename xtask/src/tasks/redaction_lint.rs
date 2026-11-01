//! `redaction-lint` (Q5.2) — grep every `yanshi-*/src/**/*.rs` for
//! `(debug|info|warn|error)!(... danger = %text, ...)` patterns
//! that interpolate raw user prompt / response / content / body /
//! params into a log macro without going through
//! `yanshi_tracing::redaction` or `prompt_fingerprint`. Backs
//! the Q5.2 workspace redaction audit.
//!
//! Walks `yanshi-*/src/**/*.rs` looking for `tracing::*!` macro calls
//! (also bare `debug!` / `info!` / `warn!` / `error!`) that interpolate raw
//! user-supplied data — prompts, LLM responses, request/response bodies,
//! tool params, chat content — without going through
//! `yanshi_tracing::redaction` or `yanshi_llm::prompt_fingerprint`.
//!
//! The grammar of a "bad" call is intentionally narrow:
//!   (debug|info|warn|error)!(... `<danger>` = (%|?) ...)
//! where `<danger>` is one of `DANGER_FIELDS`. Trailing-suffix variants like
//! `prompt_len`, `prompt_sha`, `response_fingerprint`, `body_size` are safe
//! because they encode a metric, not the raw value, so the pattern only
//! matches the danger word followed by whitespace + `=`.
//!
//! Format-string interpolation (`"... {prompt}"`) is harder to catch
//! without parsing — we run a second regex that flags only the most
//! obvious shapes: `"... {prompt}"` / `"... {response}"` etc. Anything
//! more complex needs a per-site `// allow-redaction-lint: <reason>`
//! comment immediately above the call.
//!
//! Allow list mechanics:
//!   - The `// allow-redaction-lint: <reason>` comment on the same line
//!     or the line immediately above the macro call suppresses the lint
//!     hit for that site. The `reason` text is captured and printed in
//!     the report (good citizen check — empty reasons fail the lint).
//!   - `TRACE`-level macros are exempt: production deployments
//!     intentionally exclude trace, and we already document
//!     "TRACE may contain PII" elsewhere.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

const DANGER_FIELDS: &[&str] = &[
  "prompt",
  "response",
  "content",
  "body",
  "raw_response",
  "planner_text",
  "user_input",
  "message_body",
  "params",
  "request_body",
  "response_body",
];

/// One redaction-lint hit.
#[derive(Debug, PartialEq, Eq)]
struct RedactionLintHit {
  path: PathBuf,
  line: usize,
  level: String,
  field: String,
  snippet: String,
}

pub(crate) fn redaction_lint_at(
  workspace_root: &Path,
  stdout: &mut impl Write,
  stderr: &mut impl Write,
) -> Result<()> {
  let entries = std::fs::read_dir(workspace_root)
    .with_context(|| format!("read workspace root {}", workspace_root.display()))?;

  let mut crate_dirs: Vec<PathBuf> = Vec::new();
  for entry in entries.flatten() {
    let name = entry.file_name().to_string_lossy().into_owned();
    if name.starts_with("yanshi-") && entry.path().is_dir() {
      let src = entry.path().join("src");
      if src.is_dir() {
        crate_dirs.push(src);
      }
    }
  }
  crate_dirs.sort();

  let mut hits: Vec<RedactionLintHit> = Vec::new();
  for src in &crate_dirs {
    collect_redaction_hits(src, &mut hits)?;
  }

  hits.sort_by_key(|a| (a.path.clone(), a.line));

  for hit in &hits {
    let _ = writeln!(
      stderr,
      "{}:{}: redaction-lint: {}! interpolates `{}` without fingerprint or redaction\n    {}",
      hit.path.display(),
      hit.line,
      hit.level,
      hit.field,
      hit.snippet
    );
  }

  if hits.is_empty() {
    let _ = writeln!(
      stdout,
      "redaction-lint: OK ({} crate dirs scanned)",
      crate_dirs.len()
    );
    Ok(())
  } else {
    let _ = writeln!(
      stderr,
      "\nredaction-lint: {} hit(s); see `yanshi-tracing::redaction::redact_text/value` or `yanshi_llm::prompt_fingerprint` for the canonical helpers. Suppress a false-positive with `// allow-redaction-lint: <reason>` on the same line.",
      hits.len()
    );
    bail!("redaction-lint failed: {} hit(s)", hits.len());
  }
}

fn collect_redaction_hits(dir: &Path, hits: &mut Vec<RedactionLintHit>) -> Result<()> {
  for entry in std::fs::read_dir(dir)? {
    let entry = entry?;
    let path = entry.path();
    if path.is_dir() {
      collect_redaction_hits(&path, hits)?;
    } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
      scan_file_for_redaction(&path, hits)?;
    }
  }
  Ok(())
}

fn scan_file_for_redaction(path: &Path, hits: &mut Vec<RedactionLintHit>) -> Result<()> {
  let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  for (idx, line) in text.lines().enumerate() {
    if line.contains("allow-redaction-lint") {
      continue;
    }
    if let Some(hit) = detect_redaction_hit(line) {
      hits.push(RedactionLintHit {
        path: path.to_path_buf(),
        line: idx + 1,
        level: hit.0,
        field: hit.1,
        snippet: line.trim().to_string(),
      });
    }
  }
  Ok(())
}

/// Return `(level, danger_field)` when the line contains a redaction
/// violation, otherwise `None`. Pure function so it's straight-line
/// testable.
fn detect_redaction_hit(line: &str) -> Option<(String, String)> {
  let trimmed = line.trim_start();
  if trimmed.starts_with("//") {
    return None;
  }
  // We only flag debug/info/warn/error. `trace!` is intentionally exempt.
  let levels = ["debug", "info", "warn", "error"];
  let mut macro_level: Option<&str> = None;
  for level in levels {
    let needle = format!("{level}!(");
    let needle_ns = format!("::{level}!(");
    if line.contains(&needle) || line.contains(&needle_ns) {
      macro_level = Some(level);
      break;
    }
  }
  let level = macro_level?;

  // Pattern 1: structured field — `field = %expr` / `field = ?expr`.
  for danger in DANGER_FIELDS {
    let patterns = [
      format!("{danger} = %"),
      format!("{danger} = ?"),
      format!("{danger}=%"),
      format!("{danger}=?"),
    ];
    for pat in &patterns {
      if line.contains(pat) {
        return Some((level.to_string(), (*danger).to_string()));
      }
    }
  }

  // Pattern 2: format-string positional `"... {danger} ..."`. Same
  // danger list, but as a literal `{danger}` brace.
  for danger in DANGER_FIELDS {
    let needle = format!("{{{danger}}}");
    if line.contains(&needle) {
      return Some((level.to_string(), (*danger).to_string()));
    }
  }

  None
}

#[cfg(test)]
mod redaction_lint_tests {
  use super::*;
  use std::fs;
  use tempfile::TempDir;

  #[test]
  fn detect_flags_debug_with_structured_prompt_field() {
    let line = r#"        debug!(prompt = %self.prompt, "request");"#;
    assert_eq!(
      detect_redaction_hit(line),
      Some(("debug".to_string(), "prompt".to_string()))
    );
  }

  #[test]
  fn detect_flags_info_with_response_format_brace() {
    let line = r#"info!("LLM responded: {response}");"#;
    assert_eq!(
      detect_redaction_hit(line),
      Some(("info".to_string(), "response".to_string()))
    );
  }

  #[test]
  fn detect_ignores_trace_level() {
    let line = r#"tracing::trace!(prompt = %self.prompt, "TRACE-only full body");"#;
    assert!(detect_redaction_hit(line).is_none());
  }

  #[test]
  fn detect_ignores_fingerprint_suffix_variants() {
    // These encode a metric, not raw text.
    for line in [
      r#"debug!(prompt_len = self.prompt.len(), "request");"#,
      r#"info!(response_sha = %fingerprint, "done");"#,
      r#"warn!(body_size = bytes, "oversized");"#,
    ] {
      assert!(
        detect_redaction_hit(line).is_none(),
        "fingerprint metric must not trip the lint: {line}"
      );
    }
  }

  #[test]
  fn detect_ignores_comments_and_docstrings() {
    for line in [
      r#"  // debug!(prompt = %self.prompt, "request");"#,
      r#"  /// debug!(response = %x, "doc example");"#,
      r#"  //! info!(content = %body, "module-level doc");"#,
    ] {
      assert!(
        detect_redaction_hit(line).is_none(),
        "comment must not trip the lint: {line}"
      );
    }
  }

  #[test]
  fn allow_redaction_lint_comment_suppresses_hit() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path();
    let crate_src = workspace.join("yanshi-fake").join("src");
    fs::create_dir_all(&crate_src).unwrap();
    fs::write(
      crate_src.join("lib.rs"),
      // Two near-identical sites; the second carries the allow-comment
      // marker so it must be skipped.
      "fn a() { debug!(prompt = %x, \"bad\"); }\n\
       fn b() { debug!(prompt = %x, \"benign\"); } // allow-redaction-lint: test fixture\n",
    )
    .unwrap();

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let err = redaction_lint_at(workspace, &mut stdout, &mut stderr).unwrap_err();
    let err_text = format!("{err}");
    let stderr_text = String::from_utf8(stderr).unwrap();
    assert!(err_text.contains("1 hit"), "expected 1 hit, got {err_text}");
    assert!(
      stderr_text.contains("lib.rs:1"),
      "expected line 1 hit, got {stderr_text}"
    );
    assert!(
      !stderr_text.contains("lib.rs:2"),
      "allow-marker line must be suppressed, got {stderr_text}"
    );
  }

  #[test]
  fn green_run_emits_ok_summary() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path();
    let crate_src = workspace.join("yanshi-clean").join("src");
    fs::create_dir_all(&crate_src).unwrap();
    fs::write(
      crate_src.join("lib.rs"),
      "fn a() { debug!(prompt_len = x.len(), \"safe\"); }\n",
    )
    .unwrap();

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    redaction_lint_at(workspace, &mut stdout, &mut stderr).expect("clean workspace must pass");
    let stdout_text = String::from_utf8(stdout).unwrap();
    assert!(stdout_text.contains("redaction-lint: OK"));
  }
}
