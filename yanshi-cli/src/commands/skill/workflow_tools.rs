//! Y4.4: a skill's `[[workflows]]` as agent tools, and the files those
//! workflows produced.
//!
//! Each entry becomes a `WorkflowTool` (RFC §3: the planner sees a closed
//! menu of deliverables with typed parameters): the schema comes from
//! the template's declared `inputs`, the runner fills their defaults and
//! runs under a known run base dir, and when the template ends in a
//! `deliverable` node only that value goes back to the agent, not the
//! whole state. Every run is recorded so `--output-dir` can copy its
//! artifacts out afterwards.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use yanshi_agents::tools::WorkflowTool;
use yanshi_config::config::v2::FlowDefinitionV2;
use yanshi_config::executor::{
  apply_declared_inputs, build_flow_from_definition, inputs_json_schema, parse_workflow_definition,
};
use yanshi_core::async_node::{AsyncNodeInputs, AsyncNodeResult};
use yanshi_core::error::YanshiError;
use yanshi_core::{Flow, FlowExecutionConfig, FlowExt, FlowRunner};
use yanshi_skills::manifest::SkillManifest;
use yanshi_tools::Tool;

/// One finished workflow-tool run.
pub struct RecordedRun {
  pub tool: String,
  pub run_id: String,
  pub state: HashMap<String, AsyncNodeResult>,
}

/// Runs recorded by every workflow tool of one skill run.
#[derive(Default)]
pub struct RecordedRuns(Mutex<Vec<RecordedRun>>);

impl RecordedRuns {
  pub fn take(&self) -> Vec<RecordedRun> {
    std::mem::take(
      &mut *self
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
  }
}

struct TemplateRunner {
  tool: String,
  definition: FlowDefinitionV2,
  run_base_dir: PathBuf,
  runs: Arc<RecordedRuns>,
}

#[async_trait]
impl FlowRunner for TemplateRunner {
  async fn run(
    &self,
    flow: &Flow,
    mut inputs: AsyncNodeInputs,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    apply_declared_inputs(&self.definition, &mut inputs).map_err(|e| {
      YanshiError::NodeInputError {
        message: format!("workflow tool '{}': {e:#}", self.tool),
      }
    })?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut config = FlowExecutionConfig::serial().with_run_base_dir(&self.run_base_dir);
    if let Some(limit) = self.definition.cost_limit_usd {
      config = config.with_cost_limit_usd(limit);
    }
    let state = flow
      .execute_from_inputs_with_id_and_config(run_id.clone(), inputs, config)
      .await?;
    self
      .runs
      .0
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .push(RecordedRun {
        tool: self.tool.clone(),
        run_id,
        state: state.clone(),
      });
    Ok(state)
  }
}

/// A skill's workflow tools and the record of what they ran.
pub struct WorkflowTools {
  pub tools: Vec<Arc<dyn Tool>>,
  pub runs: Arc<RecordedRuns>,
}

/// Build the skill's workflow tools. `model` (the skill's resolved model
/// or `--model`) is applied to the templates' llm nodes.
pub fn build(
  manifest: &SkillManifest,
  skill_dir: &Path,
  model: Option<&str>,
  run_base_dir: &Path,
) -> Result<WorkflowTools> {
  let runs = Arc::new(RecordedRuns::default());
  let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
  for entry in &manifest.workflows {
    let path = skill_dir.join(&entry.path);
    let yaml = std::fs::read_to_string(&path)
      .with_context(|| format!("workflow tool '{}': reading {}", entry.name, path.display()))?;
    let definition = parse_workflow_definition(&yaml)
      .with_context(|| format!("workflow tool '{}': {}", entry.name, path.display()))?;
    let flow = build_flow_from_definition(&definition, model).with_context(|| {
      format!(
        "workflow tool '{}': building {}",
        entry.name,
        path.display()
      )
    })?;
    let has_deliverable = definition.nodes.iter().any(|node| node.id == "deliverable");
    let description = entry
      .description
      .clone()
      .unwrap_or_else(|| format!("Run the '{}' workflow", definition.name));
    let schema = inputs_json_schema(&definition);
    let runner = Arc::new(TemplateRunner {
      tool: entry.name.clone(),
      definition,
      run_base_dir: run_base_dir.to_path_buf(),
      runs: runs.clone(),
    });
    let mut tool = WorkflowTool::with_schema(entry.name.clone(), description, schema, flow, runner);
    if has_deliverable {
      tool = tool.with_result_output("deliverable", "deliverable");
    }
    tools.push(Arc::new(tool));
  }
  Ok(WorkflowTools { tools, runs })
}

/// Copy each recorded run's artifacts to `<out>/<tool>/<node_id>/<file>`
/// (`<tool>-2`, `-3`, ... when a tool ran more than once) with a
/// manifest per run. Returns the number of files copied.
pub fn export(runs: Vec<RecordedRun>, run_base_dir: &Path, out: &Path) -> Result<usize> {
  let mut seen: HashMap<String, usize> = HashMap::new();
  let mut copied = 0;
  for run in runs {
    let count = seen.entry(run.tool.clone()).or_insert(0);
    *count += 1;
    let dir = if *count == 1 {
      out.join(&run.tool)
    } else {
      out.join(format!("{}-{count}", run.tool))
    };
    let files = yanshi_core::run_context::collect_file_outputs(&run.state, run_base_dir);
    copied +=
      crate::commands::workflow::artifacts::copy_to_output_dir(&files, &dir, &run.run_id)?.len();
  }
  Ok(copied)
}
