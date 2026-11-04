use crate::redaction::redact_cli_value;
use crate::shutdown::{DEFAULT_TRACE_FLUSH_TIMEOUT, SIGINT_EXIT_CODE, shutdown_signal};
use crate::{
  commands::workflow::validate::print_schema_report, config::schema::validate_flow_definition,
  config::v2::FlowDefinitionV2, executor::build_flow_from_definition,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use yanshi_core::FlowExt;
use yanshi_core::{
  FlowCancellationToken, FlowExecutionConfig, async_node::AsyncNodeInputs, flow::Flow,
  value::FlowValue,
};
use yanshi_tracing::{TraceCollector, TraceConfig, storage::file::FileTraceStorage};

#[allow(clippy::too_many_arguments)]
pub async fn execute(
  workflow_file: String,
  watch: bool,
  output: Option<String>,
  model: Option<String>,
  input: Vec<(String, String)>,
  dry_run: bool,
  timeout: String,
  max_retries: u32,
  execution_mode: String,
  max_concurrency: usize,
  run_dir: Option<String>,
  output_dir: Option<String>,
) -> Result<()> {
  if watch {
    bail!("--watch is not implemented yet; run without --watch or use workflow debug --dry-run");
  }

  println!(
    "🚀 Starting Yanshi V2 workflow execution: {}",
    workflow_file
  );

  // 1. Read and parse the V2 workflow file
  let yaml_content = fs::read_to_string(&workflow_file)
    .with_context(|| format!("Failed to read workflow file: {}", workflow_file))?;
  let flow_def: FlowDefinitionV2 =
    serde_yaml::from_str(&yaml_content).with_context(|| "Failed to parse V2 workflow YAML.")?;

  println!("📄 Workflow '\'{}\'\' loaded.", flow_def.name);
  let schema_report = validate_flow_definition(&flow_def);
  if !schema_report.is_valid() || !schema_report.warnings.is_empty() {
    print_schema_report(&flow_def.name, &schema_report);
  }
  if !schema_report.is_valid() {
    bail!(
      "workflow '{}' failed schema validation with {} issue(s)",
      flow_def.name,
      schema_report.issues.len()
    );
  }

  let mut flow = build_flow_from_definition(&flow_def, model.as_deref())?;
  if let Some(model) = &model {
    println!("🤖 Model override: {}", model);
  }

  // T3.2: enact `inputs:` declarations (required-check + default-fill)
  // against the CLI-supplied `--input` values before either the dry-run
  // preview or a real execution — a workflow requiring an input the
  // caller forgot to pass should fail clearly here, not with a confusing
  // downstream input_mapping error deep into a real run.
  let mut initial_inputs = parse_inputs(input)?;
  crate::executor::apply_declared_inputs(&flow_def, &mut initial_inputs)?;
  if !initial_inputs.is_empty() {
    println!("📥 Loaded {} input value(s).", initial_inputs.len());
  }

  if dry_run {
    let order = flow
      .execution_order()
      .context("Failed to build workflow execution order")?;
    println!("\n🧪 Dry run complete. No nodes were executed.");
    println!("📅 Execution order:");
    for (idx, node_id) in order.iter().enumerate() {
      println!("  {}. {}", idx + 1, node_id);
    }
    return Ok(());
  }

  // Stable workflow_id for both the flow's emitted events and the trace
  // file. Printing it up front so the operator can run `yanshi trace
  // tui <workflow_id>` against the JSON written below.
  let workflow_id = Uuid::new_v4().to_string();
  let trace_dir = resolve_trace_dir()?;
  // Q3.1.2: keep an Arc clone of the trace collector so the Ctrl-C
  // path can `flush()` the drain queue before exiting. Without this
  // the JSONL trace file the CLI just told the operator to inspect
  // could be missing the terminal `WorkflowCancelled` event.
  let mut trace_collector: Option<Arc<TraceCollector>> = None;
  if let Some(dir) = trace_dir.as_ref() {
    fs::create_dir_all(dir)
      .with_context(|| format!("Failed to create trace dir {}", dir.display()))?;
    let storage = Arc::new(
      FileTraceStorage::new(dir.clone())
        .with_context(|| format!("Failed to initialise trace storage at {}", dir.display()))?,
    );
    let collector = Arc::new(TraceCollector::new(storage, TraceConfig::development()));
    flow = flow.with_event_listener(collector.clone());
    trace_collector = Some(collector);
    println!(
      "📓 Tracing enabled — workflow_id={} dir={}",
      workflow_id,
      dir.display()
    );
    println!(
      "   View timeline: yanshi trace tui {} --dir {}",
      workflow_id,
      dir.display()
    );
  }

  let timeout_duration =
    parse_duration(&timeout).with_context(|| format!("Invalid --timeout value '{}'", timeout))?;
  let mut execution_config = parse_execution_config(&execution_mode, max_concurrency, run_dir)?;
  // Q3.1.2: install a FlowCancellationToken so the Ctrl-C handler
  // below can ask the flow to stop after the current node instead of
  // the runtime aborting the in-flight node mid-`await`.
  let cancel_token = FlowCancellationToken::new();
  execution_config = execution_config.with_cancellation_token(cancel_token.clone());
  if execution_config.mode == yanshi_core::FlowExecutionMode::Concurrent {
    println!(
      "⚙️  Execution mode: concurrent (max_concurrency={})",
      execution_config.max_concurrency
    );
  }
  if let Some(run_base_dir) = &execution_config.run_base_dir {
    println!("📁 Run artifacts directory: {}", run_base_dir.display());
  }
  let artifact_base = execution_config
    .resolved_run_base_dir()
    .context("Failed to resolve the run base directory")?;

  // 3. Execute the flow
  println!("\n▶️  Running flow...");
  let start_time = std::time::Instant::now();
  let run_future = run_with_retries(
    flow,
    workflow_id.clone(),
    initial_inputs,
    timeout_duration,
    max_retries,
    execution_config,
  );
  // Q3.1.2: race the run against SIGINT/SIGTERM. On signal we flip
  // the cancellation token (the flow then emits `WorkflowCancelled`
  // and returns `TaskCancelled` after the current node finishes),
  // wait for the trace drain to catch up, then exit 130. Without
  // this Ctrl-C silently corrupts the JSONL trace file.
  tokio::pin!(run_future);
  let final_state = tokio::select! {
    biased;
    res = &mut run_future => res?,
    _ = shutdown_signal() => {
      eprintln!("\n🛑 Cancelled (received SIGINT/SIGTERM)");
      cancel_token.cancel();
      // Give the in-flight node a bounded window to observe the
      // cancellation and let the flow emit `WorkflowCancelled`.
      // 10s is conservative — most nodes either complete or check
      // the token within their own timeout much sooner.
      const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
      let _ = tokio::time::timeout(CANCEL_DRAIN_TIMEOUT, &mut run_future).await;
      if let Some(collector) = trace_collector.as_ref() {
        let drained = collector.flush(DEFAULT_TRACE_FLUSH_TIMEOUT).await;
        if !drained {
          eprintln!(
            "⚠  trace drain timed out after {:?}; some events may be missing from the JSONL file",
            DEFAULT_TRACE_FLUSH_TIMEOUT
          );
        }
      }
      std::process::exit(SIGINT_EXIT_CODE);
    }
  };
  let duration = start_time.elapsed();
  println!("\n✅ Workflow completed in {:.2?}.", duration);

  // 4. Print or save the results
  let mut redacted_final_state =
    serde_json::to_value(&final_state).context("Failed to serialize final state for redaction.")?;
  redact_cli_value(&mut redacted_final_state);
  let final_state_json = serde_json::to_string_pretty(&redacted_final_state)
    .context("Failed to serialize redacted final state to JSON.")?;

  match output.as_deref() {
    Some("-") => {
      println!("{}", final_state_json);
    }
    Some(path) => {
      fs::write(path, &final_state_json)
        .with_context(|| format!("Failed to write workflow output to {}", path))?;
      println!("💾 Final state written to {}", path);
    }
    None => {
      println!("\n📊 Final State Pool:");
      println!("{}", final_state_json);
    }
  }

  // Y1.4: what the run produced, and optionally a copy of it.
  let produced = yanshi_core::run_context::collect_file_outputs(&final_state, &artifact_base);
  super::artifacts::print_table(&produced);
  if let Some(dir) = output_dir {
    let dir = PathBuf::from(dir);
    let copied = super::artifacts::copy_to_output_dir(&produced, &dir, &workflow_id)?;
    println!(
      "📤 Copied {} artifact(s) to {} (manifest: {})",
      copied.len(),
      dir.display(),
      super::artifacts::MANIFEST_FILE
    );
  }

  Ok(())
}

/// Resolve where to write trace JSON: explicit `YANSHI_TRACE_DIR` env
/// wins; otherwise default to `~/.yanshi/traces` so `yanshi trace
/// tui <workflow_id>` works out of the box. Returns `Ok(None)` only when
/// the home directory cannot be located AND the env var is unset (rare;
/// container runs without HOME). Tracing then degrades to a no-op.
fn resolve_trace_dir() -> Result<Option<PathBuf>> {
  if let Some(raw) = std::env::var("YANSHI_TRACE_DIR")
    .ok()
    .filter(|v| !v.is_empty())
  {
    return Ok(Some(PathBuf::from(raw)));
  }
  Ok(dirs::home_dir().map(|h| h.join(".yanshi").join("traces")))
}

fn parse_inputs(input: Vec<(String, String)>) -> Result<AsyncNodeInputs> {
  let mut inputs = AsyncNodeInputs::new();
  for (key, raw_value) in input {
    if key.trim().is_empty() {
      bail!("Input key cannot be empty");
    }
    let value = parse_input_value(&raw_value);
    inputs.insert(key, FlowValue::Json(value));
  }
  Ok(inputs)
}

fn parse_input_value(raw_value: &str) -> Value {
  serde_json::from_str(raw_value).unwrap_or_else(|_| Value::String(raw_value.to_string()))
}

fn parse_duration(raw: &str) -> Result<Duration> {
  let raw = raw.trim();
  if raw.is_empty() {
    bail!("duration cannot be empty");
  }

  let (number, multiplier) = if let Some(value) = raw.strip_suffix("ms") {
    (value, 1)
  } else if let Some(value) = raw.strip_suffix('s') {
    (value, 1_000)
  } else if let Some(value) = raw.strip_suffix('m') {
    (value, 60_000)
  } else {
    (raw, 1_000)
  };

  let amount: u64 = number
    .trim()
    .parse()
    .with_context(|| "duration must be an integer followed by ms, s, or m")?;
  Ok(Duration::from_millis(amount.saturating_mul(multiplier)))
}

fn parse_execution_config(
  execution_mode: &str,
  max_concurrency: usize,
  run_dir: Option<String>,
) -> Result<FlowExecutionConfig> {
  let mut config = match execution_mode {
    "serial" => FlowExecutionConfig::serial(),
    "concurrent" => {
      if max_concurrency == 0 {
        bail!("--max-concurrency must be greater than zero");
      }
      FlowExecutionConfig::concurrent(max_concurrency)
    }
    other => bail!("unsupported execution mode '{}'", other),
  };

  if let Some(run_dir) = run_dir.or_else(|| std::env::var("YANSHI_RUN_DIR").ok()) {
    if run_dir.trim().is_empty() {
      bail!("--run-dir cannot be empty");
    }
    config = config.with_run_base_dir(PathBuf::from(run_dir));
  }

  Ok(config)
}

async fn run_with_retries(
  flow: Flow,
  workflow_id: String,
  initial_inputs: AsyncNodeInputs,
  timeout_duration: Duration,
  max_retries: u32,
  execution_config: FlowExecutionConfig,
) -> Result<std::collections::HashMap<String, yanshi_core::async_node::AsyncNodeResult>> {
  let attempts = max_retries.saturating_add(1);
  let mut last_error = None;

  for attempt in 1..=attempts {
    if attempts > 1 {
      println!("🔁 Workflow attempt {}/{}", attempt, attempts);
    }

    let run = flow.execute_from_inputs_with_id_and_config(
      workflow_id.clone(),
      initial_inputs.clone(),
      execution_config.clone(),
    );
    match tokio::time::timeout(timeout_duration, run).await {
      // V1.1: `execute_from_inputs_with_id_and_config` returning `Ok`
      // no longer means every node succeeded -- a node can carry a
      // genuine failure in its own state_pool entry while the function
      // itself succeeds (true for the concurrent path all along, and
      // now also true for serial after yanshi-core's V0.3/V1.1
      // fixes to its terminal-status semantics). Treat that the same
      // as a hard `Err` here so `--max-retries` and the exit code this
      // command's caller derives from `Result::Err` behave uniformly
      // regardless of which layer reported the failure or which
      // execution mode ran it.
      Ok(Ok(state)) => match first_genuine_state_error(&state) {
        None => return Ok(state),
        Some(error) => {
          last_error = Some(anyhow::anyhow!(
            "workflow attempt {}/{} failed: {}",
            attempt,
            attempts,
            error
          ));
        }
      },
      Ok(Err(err)) => {
        last_error = Some(
          anyhow::Error::new(err)
            .context(format!("workflow attempt {}/{} failed", attempt, attempts)),
        );
      }
      Err(_) => {
        last_error = Some(anyhow::anyhow!(
          "workflow attempt {}/{} timed out after {:?}",
          attempt,
          attempts,
          timeout_duration
        ));
      }
    }

    if attempt < attempts {
      println!("⚠️  Attempt {} failed; retrying...", attempt);
    }
  }

  Err(last_error.unwrap_or_else(|| anyhow::anyhow!("workflow execution failed")))
}

/// V1.1: mirrors `yanshi_core::flow`'s `is_genuine_failure` -- a
/// benign `run_if` skip (`YanshiError::NodeSkipped`) is not a
/// workflow failure and must not trigger a retry or a non-zero exit.
fn first_genuine_state_error(
  state: &std::collections::HashMap<String, yanshi_core::async_node::AsyncNodeResult>,
) -> Option<String> {
  state.iter().find_map(|(node_id, result)| match result {
    Err(yanshi_core::YanshiError::NodeSkipped) => None,
    Err(err) => Some(format!("{node_id}: {err}")),
    Ok(_) => None,
  })
}
