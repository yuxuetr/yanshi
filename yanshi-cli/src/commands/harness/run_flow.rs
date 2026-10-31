//! `yanshi harness run-flow` — run a config workflow under harness
//! governance (P-A2.2).
//!
//! Loads a workflow YAML, builds an `yanshi-core` `Flow`, and runs it via
//! [`yanshi_harness::HarnessRuntime::run_flow`]. The run is bracketed with
//! the Harness envelope (`session_started` runtime=`flow` → per-node
//! `step_started` → `stopped`) and persisted as JSONL like an agent session, so
//! a deterministic workflow gets the same observable / replayable governance
//! stream. Tool-call approval governance additionally applies to any node whose
//! tools are routed through a harness-wrapped registry (the programmatic
//! `run_flow` path); config-built nodes embed their own tools, so the CLI
//! surface delivers the envelope + node events — and under the production
//! profile, whose promise is approval for every non-idempotent call, it
//! refuses workflows with tool-running nodes rather than run them ungoverned.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use yanshi_core::CoreFlowRunner;
use yanshi_core::async_node::AsyncNodeInputs;
use yanshi_core::value::FlowValue;
use yanshi_harness::{
  FlowRunOutcome, HarnessEventSink, HarnessFlowRunOptions, HarnessProfile, HarnessRuntime,
  JsonlEventSink, StdoutEventSink, default_session_dir,
};

use super::{OutputFormat, parse_profile, resolve_run_dir};
use crate::executor::{build_flow_from_yaml, parse_workflow_definition};
use crate::json_envelope::CliJsonEnvelope;
use yanshi_config::config::v2::{HOST_NODE_TYPES, walk_nodes};

#[allow(clippy::too_many_arguments)]
pub async fn execute(
  workflow_file: String,
  model: Option<String>,
  input: Vec<String>,
  profile: String,
  output: String,
  workspace: Option<String>,
  run_dir_override: Option<String>,
  timeout_ms: Option<u64>,
  session: Option<String>,
  max_concurrency: usize,
) -> Result<()> {
  let profile = parse_profile(&profile)?;
  let output = OutputFormat::parse(&output)?;

  let yaml = std::fs::read_to_string(&workflow_file)
    .with_context(|| format!("could not read workflow file '{workflow_file}'"))?;
  if profile == HarnessProfile::Production {
    refuse_ungoverned_nodes(&yaml)
      .with_context(|| format!("refusing to run '{workflow_file}' under the production profile"))?;
  }
  let flow = build_flow_from_yaml(&yaml, model.as_deref())
    .with_context(|| format!("failed to build a flow from '{workflow_file}'"))?;

  let inputs = parse_inputs(input)?;

  let workspace = workspace
    .map(PathBuf::from)
    .or_else(|| std::env::current_dir().ok())
    .context("could not determine workspace root (pass --workspace or run from a real dir)")?;
  let run_root = resolve_run_dir(run_dir_override)?;
  let session_dir = default_session_dir(&run_root);

  // Persist the governed event stream as JSONL; stream-json additionally
  // fans the same envelope out to stdout live.
  let jsonl: Arc<dyn HarnessEventSink> = Arc::new(JsonlEventSink::new(session_dir.clone()));
  let mut harness = HarnessRuntime::for_flow().with_event_sink(jsonl);
  if matches!(output, OutputFormat::StreamJson) {
    harness = harness.with_event_sink(Arc::new(StdoutEventSink::new()));
  }

  let runner = Arc::new(CoreFlowRunner::concurrent(max_concurrency.max(1)));
  let options = HarnessFlowRunOptions {
    workspace_root: workspace,
    profile,
    session_id: session,
    timeout: timeout_ms.map(Duration::from_millis),
    metadata: Value::Null,
  };

  let result = harness
    .run_flow(&flow, runner, inputs, options)
    .await
    .context("harness flow run failed")?;

  let (status, error): (&str, Option<String>) = match &result.outcome {
    FlowRunOutcome::Completed(_) => ("completed", None),
    FlowRunOutcome::Failed(err) => ("failed", Some(err.clone())),
    FlowRunOutcome::TimedOut => (
      "timed_out",
      Some("flow run exceeded the timeout".to_string()),
    ),
  };
  let node_count = match &result.outcome {
    FlowRunOutcome::Completed(state) => state.len(),
    _ => 0,
  };

  match output {
    OutputFormat::Text => {
      println!("session: {}", result.session_id);
      match &result.outcome {
        FlowRunOutcome::Completed(state) => {
          println!("✅ flow completed — {} node output(s)", state.len());
        }
        FlowRunOutcome::Failed(err) => println!("❌ flow failed: {err}"),
        FlowRunOutcome::TimedOut => println!("⏱️  flow timed out"),
      }
      println!("session log: {}", session_dir.display());
      println!("final event seq: {}", result.final_event_seq);
    }
    OutputFormat::StreamJson => {
      // Events already streamed live; nothing else to print.
    }
    OutputFormat::Json | OutputFormat::JsonEnvelope => {
      let summary = json!({
        "session_id": result.session_id,
        "status": status,
        "error": error,
        "node_output_count": node_count,
        "final_event_seq": result.final_event_seq,
      });
      let rendered = if matches!(output, OutputFormat::JsonEnvelope) {
        serde_json::to_string_pretty(&CliJsonEnvelope::ok("harness run-flow", summary))?
      } else {
        serde_json::to_string_pretty(&summary)?
      };
      println!("{rendered}");
    }
  }

  // A failed/timed-out flow is a non-zero exit so scripts can gate on it.
  if !matches!(result.outcome, FlowRunOutcome::Completed(_)) {
    anyhow::bail!("flow run did not complete successfully ({status})");
  }
  Ok(())
}

/// Node types whose side effects the production profile would require
/// approval for, but which config-built flows run with their own embedded
/// tools, outside the harness hook pipeline.
fn ungoverned_tool_node(kind: &str) -> bool {
  HOST_NODE_TYPES.contains(&kind) || kind == "http"
}

fn refuse_ungoverned_nodes(yaml: &str) -> Result<()> {
  let definition = parse_workflow_definition(yaml)?;
  walk_nodes(&definition.nodes, &mut |node| {
    if ungoverned_tool_node(&node.node_type) {
      return Err(format!(
        "node '{}' (type '{}') runs its own tools, which harness approval cannot \
         govern; the production profile requires approval for them. Use \
         `--profile local` or `yanshi workflow run` for this workflow.",
        node.id, node.node_type
      ));
    }
    Ok(())
  })
  .map_err(anyhow::Error::msg)
}

/// Parse repeated `--input key=value` flags into the flow's initial inputs.
/// Values that parse as JSON are kept as JSON; anything else becomes a string.
fn parse_inputs(input: Vec<String>) -> Result<AsyncNodeInputs> {
  let mut inputs = AsyncNodeInputs::new();
  for entry in input {
    let (key, raw_value) = entry
      .split_once('=')
      .with_context(|| format!("invalid --input '{entry}', expected key=value"))?;
    if key.trim().is_empty() {
      anyhow::bail!("input key cannot be empty in '{entry}'");
    }
    let value =
      serde_json::from_str(raw_value).unwrap_or_else(|_| Value::String(raw_value.to_string()));
    inputs.insert(key.to_string(), FlowValue::Json(value));
  }
  Ok(inputs)
}
