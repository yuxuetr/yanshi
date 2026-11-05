use anyhow::{Context, Result};
use std::path::Path;

use super::error_context::mcp_context;
use super::runtime_options::{apply_memory_override, memory_label};
use crate::redaction::{redact_cli_text, to_redacted_json_value};
use yanshi_llm::Yanshi;
use yanshi_skills::{SkillBuilder, SkillLoader};

/// Resolved value of `--output` (F-A2-6). Text mode preserves the
/// pre-existing emoji-prefixed banner + `🤖 Agent:` line; json mode
/// emits a single JSON object to stdout suitable for piping into other
/// tooling. All warnings and progress messages go to stderr in json
/// mode so stdout stays pure JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
  Text,
  Json,
}

impl OutputFormat {
  fn parse(value: &str) -> Result<Self> {
    match value {
      "text" => Ok(Self::Text),
      "json" => Ok(Self::Json),
      other => anyhow::bail!("unsupported --output '{other}', expected text | json"),
    }
  }
}

/// `yanshi skill run` arguments (local mode).
pub struct RunArgs {
  pub skill_dir: String,
  pub message: Option<String>,
  /// Y4.4: `KEY=VALUE` facts appended to the message.
  pub inputs: Vec<String>,
  /// Y4.4: where to copy the `[[workflows]]` tools' files.
  pub output_dir: Option<String>,
  pub run_dir: Option<String>,
  pub model_override: Option<String>,
  pub memory_override: Option<String>,
  pub session_id: Option<String>,
  pub trace: bool,
  pub output: String,
}

/// The request text: `--message`, then `--input` facts as a list.
fn compose_message(message: Option<String>, inputs: &[String]) -> Result<String> {
  let mut facts = Vec::with_capacity(inputs.len());
  for input in inputs {
    let (key, value) = input
      .split_once('=')
      .filter(|(key, _)| !key.trim().is_empty())
      .with_context(|| format!("--input '{input}' must be KEY=VALUE"))?;
    facts.push(format!("- {}: {}", key.trim(), value.trim()));
  }
  match (message, facts.is_empty()) {
    (Some(message), true) => Ok(message),
    (Some(message), false) => Ok(format!("{message}\n\nInputs:\n{}", facts.join("\n"))),
    (None, false) => Ok(format!(
      "Produce what these inputs ask for.\n\nInputs:\n{}",
      facts.join("\n")
    )),
    (None, true) => anyhow::bail!("give --message, or at least one --input KEY=VALUE"),
  }
}

pub async fn execute(args: RunArgs) -> Result<()> {
  let RunArgs {
    skill_dir,
    message,
    inputs,
    output_dir,
    run_dir,
    model_override,
    memory_override,
    session_id,
    trace,
    output,
  } = args;
  let message = compose_message(message, &inputs)?;
  let output = OutputFormat::parse(&output)?;
  let dir = Path::new(&skill_dir);

  // Load + validate manifest
  let mut manifest =
    SkillLoader::load(dir).with_context(|| format!("Failed to load skill from '{}'", skill_dir))?;

  if let Some(model) = model_override {
    manifest.model.name = Some(model);
  }
  apply_memory_override(&mut manifest, memory_override.as_deref());

  let warnings =
    SkillLoader::validate(&manifest, dir).with_context(|| "Skill validation failed")?;
  // Warnings always go to stderr; that holds for both output modes so
  // operators don't lose them when piping json into jq.
  for w in &warnings {
    eprintln!("⚠  {}", w);
  }

  if matches!(output, OutputFormat::Text) {
    println!(
      "🚀 Running skill '{}' v{}",
      manifest.skill.name, manifest.skill.version
    );
    println!("🤖 Model: {}", manifest.model.resolved_model());
    println!("🧠 Memory: {}", memory_label(&manifest));
  }

  // Initialise Yanshi (loads LLM provider config)
  Yanshi::init()
    .await
    .context("Failed to initialise Yanshi — is your API key configured?")?;

  // Y4.4: the skill's `[[workflows]]` become tools the agent can call.
  let run_base_dir = match run_dir.or_else(|| std::env::var("YANSHI_RUN_DIR").ok()) {
    Some(dir) => std::path::PathBuf::from(dir),
    None => yanshi_core::FlowExecutionConfig::serial()
      .resolved_run_base_dir()
      .context("Failed to resolve the run base directory")?,
  };
  let resolved_model = manifest.model.resolved_model();
  let super::workflow_tools::WorkflowTools {
    tools: workflow_tools,
    runs: workflow_runs,
  } = super::workflow_tools::build(
    &manifest,
    dir,
    Some(resolved_model),
    yanshi_core::FlowExecutionConfig::serial().with_run_base_dir(&run_base_dir),
  )?;
  if !workflow_tools.is_empty() && matches!(output, OutputFormat::Text) {
    println!(
      "🧰 Workflow tools: {}",
      manifest
        .workflows
        .iter()
        .map(|w| w.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
    );
  }

  // Build the agent from the skill manifest
  let mut agent = SkillBuilder::build_with_extra_tools(&manifest, dir, workflow_tools)
    .await
    .with_context(|| mcp_context("Failed to build agent from skill manifest", &manifest))?;

  // Optionally reuse an existing session
  if let Some(sid) = session_id {
    agent = agent.with_session_id(sid);
  }

  if matches!(output, OutputFormat::Text) {
    println!("📝 Session: {}", agent.session_id);
    println!("💬 User: {}\n", redact_cli_text(&message));
  }

  let start = std::time::Instant::now();
  let result = agent
    .run_with_trace(&message)
    .await
    .context("Agent run failed")?;
  let elapsed = start.elapsed();
  if !result.stop_reason.is_success() {
    anyhow::bail!(
      "Agent stopped before final answer: {:?}",
      result.stop_reason
    );
  }
  let answer = result.answer.clone().unwrap_or_default();

  // Y4.4: the workflow tools' files.
  let runs = workflow_runs.take();
  if let Some(out) = output_dir {
    let out = std::path::PathBuf::from(out);
    let copied = super::workflow_tools::export(runs, &run_base_dir, &out)?;
    if matches!(output, OutputFormat::Text) {
      println!("📤 Copied {copied} file(s) to {}", out.display());
    }
  }

  match output {
    OutputFormat::Text => {
      println!("🤖 Agent: {}", redact_cli_text(&answer));
      if trace {
        println!("\n📋 Runtime Trace:");
        println!(
          "{}",
          serde_json::to_string_pretty(&to_redacted_json_value(&result)?)?
        );
      }
      println!("\n⏱  Completed in {:.2?}", elapsed);
    }
    OutputFormat::Json => {
      // One JSON object to stdout. Stable keys; `trace` only present
      // when --trace was passed so callers can opt-in to the larger
      // payload. Answer is redacted just like text mode.
      let mut payload = serde_json::json!({
        "skill": manifest.skill.name,
        "skill_version": manifest.skill.version,
        "model": manifest.model.resolved_model(),
        "memory": memory_label(&manifest),
        "session_id": result.session_id,
        "message": redact_cli_text(&message),
        "answer": redact_cli_text(&answer),
        "stop_reason": result.stop_reason,
        "elapsed_ms": elapsed.as_millis(),
      });
      if trace {
        payload["trace"] = to_redacted_json_value(&result)?;
      }
      println!("{}", serde_json::to_string_pretty(&payload)?);
    }
  }

  Ok(())
}

#[cfg(test)]
mod compose_tests {
  use super::compose_message;

  #[test]
  fn inputs_become_a_list_after_the_message() {
    assert_eq!(
      compose_message(
        Some("Go".into()),
        &["url=https://x".into(), "want=report".into()]
      )
      .unwrap(),
      "Go\n\nInputs:\n- url: https://x\n- want: report"
    );
    assert!(
      compose_message(None, &["want=report".into()])
        .unwrap()
        .contains("- want: report")
    );
    assert!(compose_message(None, &[]).is_err());
    assert!(compose_message(None, &["no-equals".into()]).is_err());
  }
}
