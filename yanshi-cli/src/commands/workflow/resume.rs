//! `yanshi workflow resume <run_id> <workflow_file>` (Y2.4): continue a
//! run started with `workflow run --checkpoint`.
//!
//! The checkpoint carries the run's inputs, scheduling, cost limit and
//! spend so far, so only the nodes that did not finish run again, with
//! the same inputs, and nothing finished is paid for twice. The workflow
//! file is read again because node implementations are not serialised;
//! it must be the same workflow (same node ids) the run started with.

use anyhow::{Context, Result, bail};
use std::fs;
use yanshi_core::checkpoint::{CheckpointConfig, CheckpointManager};
use yanshi_core::{FlowCancellationToken, FlowExt, ResumePlanOptions};

use super::run::{
  checkpoint_dir, first_genuine_state_error, parse_execution_config, report_outcome,
};
use crate::config::v2::FlowDefinitionV2;
use crate::executor::build_flow_from_definition;

pub async fn execute(
  run_id: String,
  workflow_file: String,
  run_dir: Option<String>,
  force_replay: bool,
  output: Option<String>,
  output_dir: Option<String>,
) -> Result<()> {
  let yaml = fs::read_to_string(&workflow_file)
    .with_context(|| format!("Failed to read workflow file: {workflow_file}"))?;
  let flow_def: FlowDefinitionV2 =
    serde_yaml::from_str(&yaml).context("Failed to parse V2 workflow YAML.")?;

  // The run base dir must be the one the run used: `--run-dir`,
  // `YANSHI_RUN_DIR` or the default, resolved exactly as `run` does.
  let base = parse_execution_config("serial", 1, run_dir)?
    .resolved_run_base_dir()
    .context("Failed to resolve the run base directory")?;
  let dir = checkpoint_dir(&base, &run_id);
  let config = CheckpointConfig::default().with_checkpoint_dir(&dir);
  let has_checkpoint = CheckpointManager::new(config.clone())
    .context("Failed to open the checkpoint directory")?
    .load_latest_checkpoint(&run_id)
    .await
    .context("Failed to read the checkpoint")?
    .is_some();
  if !has_checkpoint {
    bail!(
      "no checkpoint for run '{run_id}' under {} (was it started with --checkpoint, \
       and with the same --run-dir?)",
      dir.display()
    );
  }

  let flow = build_flow_from_definition(&flow_def, None)?
    .with_checkpointing(config)
    .context("Failed to enable checkpointing")?;
  println!("🔁 Resuming run {run_id} from {}", dir.display());

  let cancel = FlowCancellationToken::new();
  let state = flow
    .resume_with_config(&run_id, &ResumePlanOptions { force_replay }, |config| {
      config.with_cancellation_token(cancel)
    })
    .await
    .with_context(|| format!("Failed to resume run '{run_id}'"))?;
  if let Some(error) = first_genuine_state_error(&state) {
    bail!("resumed run '{run_id}' failed: {error}");
  }
  println!("\n✅ Run {run_id} resumed to completion.");

  report_outcome(&state, &run_id, &base, output.as_deref(), output_dir)
}
