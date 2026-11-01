//! `yanshi harness resume-loop` — resume a ReAct or Plan-Execute agent
//! loop from its last saved
//! [`AgentLoopCheckpoint`](yanshi_agents::checkpoint::AgentLoopCheckpoint) (V2.4),
//! continuing from the interrupted turn instead of restarting the
//! session from scratch.
//!
//! Distinct from `yanshi harness resume`, which is JSONL-replay-only
//! (it re-prints the persisted event log and never touches the agent
//! loop). This command rebuilds the agent exactly as `harness run` does
//! (same `super::run::build_agent` helper, so tool registry / memory /
//! skill wiring stay identical, and `--runtime` selects the same
//! `ReActAgent` / `PlanExecuteAgent` dispatch), loads the checkpoint
//! `harness run` wrote by default, and dispatches to the matching
//! runtime's `resume_from_loop_checkpoint` to genuinely continue
//! execution.
//!
//! V2.3: also resolves an answer to a pending
//! [`AgentStopReason::AwaitingInput`](yanshi_agents::runtime::AgentStopReason::AwaitingInput)
//! question — from `--answer`, or an interactive stdin prompt when the
//! checkpoint is paused and `--answer` is omitted.
//!
//! Minimal surface deliberately: this bypasses the full
//! `HarnessRuntime`/`HarnessRunOptions` envelope (no live event stream,
//! no approval-gate re-wrapping) — it proves loop-continuation
//! correctness. The full `HarnessRuntime`-level equivalent is
//! `yanshi-server`'s `POST .../interrupt/answer` route
//! (`HarnessRuntime::resume_from_interrupt`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use yanshi_agents::FileLoopCheckpointer;
use yanshi_agents::checkpoint::AgentLoopCheckpointer as _;
use yanshi_agents::runtime::AgentContext;
use yanshi_llm::Yanshi;

use super::run::{parse_runtime_kind, prompt_for_answer};
use super::{OutputFormat, resolve_run_dir};

#[allow(clippy::too_many_arguments)]
pub async fn execute(
  session_id: String,
  skill_dir: Option<String>,
  model_override: Option<String>,
  workspace: Option<String>,
  run_dir_override: Option<String>,
  runtime: String,
  answer: Option<String>,
  output: String,
) -> Result<()> {
  let output = OutputFormat::parse(&output)?;
  let runtime_kind = parse_runtime_kind(&runtime)?;

  if skill_dir.is_none() && model_override.is_none() {
    anyhow::bail!("either --skill or --model is required");
  }

  let workspace = workspace
    .map(PathBuf::from)
    .or_else(|| std::env::current_dir().ok())
    .context("could not determine workspace root (pass --workspace or run from a real dir)")?;
  let run_root = resolve_run_dir(run_dir_override)?;

  // Persistent conversation memory keyed by session_id — same path
  // `harness run` uses, so the resumed agent picks up prior turns when
  // reasoning is involved (`--model` path only, matching `harness run`).
  let memory_db = run_root.join("harness").join("memory.sqlite");
  if let Some(parent) = memory_db.parent() {
    std::fs::create_dir_all(parent)
      .with_context(|| format!("could not create harness memory dir {}", parent.display()))?;
  }

  Yanshi::init()
    .await
    .context("failed to initialise Yanshi LLM config — is your API key configured?")?;

  let (agent, model, skill_name) = super::run::build_agent(
    skill_dir.as_deref(),
    model_override.as_deref(),
    &memory_db,
    &workspace,
    runtime_kind,
  )
  .await
  .context("failed to construct the inner Harness agent")?;

  let checkpoint_dir = FileLoopCheckpointer::default_dir(&run_root);
  let checkpointer = Arc::new(FileLoopCheckpointer::new(&checkpoint_dir).with_context(|| {
    format!(
      "could not open loop checkpoint dir {}",
      checkpoint_dir.display()
    )
  })?);
  let checkpoint = checkpointer
    .load(&session_id)
    .await
    .with_context(|| format!("failed to load loop checkpoint for session '{session_id}'"))?;
  let checkpoint = checkpoint.with_context(|| {
    format!(
      "no loop checkpoint found for session '{session_id}' under {} — either the session \
       finished normally (checkpoints are cleared on completion), or --run-dir doesn't match \
       the original `harness run` invocation",
      checkpoint_dir.display()
    )
  })?;

  let context =
    AgentContext::new(session_id.clone(), "", &model).with_loop_checkpointer(checkpointer);

  let started = std::time::Instant::now();
  if matches!(output, OutputFormat::Text) {
    eprintln!("🔁 Resuming Harness loop checkpoint");
    eprintln!("   session: {session_id}");
    eprintln!("   model: {model}");
    if let Some(name) = &skill_name {
      eprintln!("   skill: {name}");
    }
    eprintln!("   from step_index: {}", checkpoint.step_index);
  }

  // V2.3: resolve the answer to a pending `ask_user` question, if any.
  // Two-directional: a paused checkpoint requires an answer (from
  // `--answer`, or an interactive stdin prompt as a fallback); an
  // unpaused checkpoint rejects one (mirrors the agent-level validation
  // in `ReActAgent`/`PlanExecuteAgent::resume_from_loop_checkpoint`, just
  // with a clearer CLI-facing message before spending an LLM call).
  let answer = match (&checkpoint.pending_question, answer) {
    (Some(_), Some(answer)) => Some(answer),
    (Some(question), None) => Some(prompt_for_answer(question).with_context(|| {
      format!(
        "session '{session_id}' is awaiting an answer to: {question:?} — pass --answer '<text>' \
         (no interactive terminal available to prompt for one)"
      )
    })?),
    (None, Some(_)) => {
      anyhow::bail!(
        "--answer was given but session '{session_id}' has no pending question to answer"
      );
    }
    (None, None) => None,
  };

  let result = agent
    .resume_from_loop_checkpoint(context, checkpoint, answer)
    .await?;
  let elapsed = started.elapsed();

  match output {
    OutputFormat::Text => {
      let answer = result.answer.as_deref().unwrap_or("(no answer)");
      println!("{answer}");
      println!();
      println!("Session: {}", result.session_id);
      println!(
        "Stop reason: {:?}, elapsed {:.2?}",
        result.stop_reason, elapsed
      );
    }
    OutputFormat::Json | OutputFormat::JsonEnvelope => {
      let payload = serde_json::json!({
        "session_id": result.session_id,
        "answer": result.answer,
        "stop_reason": result.stop_reason,
        "model": model,
        "skill": skill_name,
        "elapsed_ms": elapsed.as_millis(),
      });
      if matches!(output, OutputFormat::JsonEnvelope) {
        let errors: Vec<String> = if result.stop_reason.is_success() {
          Vec::new()
        } else {
          vec![format!(
            "Harness session stopped before final answer: {:?}",
            result.stop_reason
          )]
        };
        let envelope = crate::json_envelope::CliJsonEnvelope::with_errors(
          "harness resume-loop",
          &payload,
          errors,
        );
        println!("{}", serde_json::to_string_pretty(&envelope)?);
      } else {
        println!("{}", serde_json::to_string_pretty(&payload)?);
      }
    }
    OutputFormat::StreamJson => {
      // No live event stream on this minimal-surface resume path (see
      // module doc comment). Emit the same one-line summary shape
      // `harness run`'s stream-json mode ends with.
      let payload = serde_json::json!({
        "type": "harness_resume_loop_summary",
        "session_id": result.session_id,
        "stop_reason": result.stop_reason,
        "elapsed_ms": elapsed.as_millis(),
      });
      println!("{}", serde_json::to_string(&payload)?);
    }
  }

  if !result.stop_reason.is_success() {
    anyhow::bail!(
      "Harness session stopped before producing a final answer: {:?}",
      result.stop_reason
    );
  }

  Ok(())
}
