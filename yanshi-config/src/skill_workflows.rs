//! A skill's `[[workflows]]` as agent tools (Y4.4), shared by the CLI
//! (`yanshi skill run`) and the gateway (`/v1/skills/{name}:run`, Y8.5).
//!
//! Each entry becomes a `WorkflowTool` (RFC §3: the planner sees a closed
//! menu of deliverables with typed parameters): the schema comes from the
//! template's declared `inputs`, the runner fills their defaults and runs
//! under the caller's execution config, and when the template ends in a
//! `deliverable` node only that value goes back to the agent, not the
//! whole state. Every run is recorded so the caller can collect its
//! artifacts afterwards.
//!
//! The agent may pass only the template's declared inputs, as plain JSON.
//! Workflow inputs reach every node and override its parameters, so an
//! undeclared key (`path`, say) would redirect a node, and a value shaped
//! like a serialised `File` would hand a node any host file.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use yanshi_agents::tools::WorkflowTool;
use yanshi_core::async_node::{AsyncNodeInputs, AsyncNodeResult};
use yanshi_core::error::YanshiError;
use yanshi_core::value::FlowValue;
use yanshi_core::{Flow, FlowExecutionConfig, FlowExt, FlowRunner};
use yanshi_skills::manifest::SkillManifest;
use yanshi_tools::Tool;

use crate::config::v2::FlowDefinitionV2;
use crate::executor::{
  apply_declared_inputs, build_flow_from_definition, inputs_json_schema, parse_workflow_definition,
};

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
  config: FlowExecutionConfig,
  runs: Arc<RecordedRuns>,
}

impl TemplateRunner {
  fn input_error(&self, message: impl std::fmt::Display) -> YanshiError {
    YanshiError::NodeInputError {
      message: format!("workflow tool '{}': {message}", self.tool),
    }
  }
}

#[async_trait]
impl FlowRunner for TemplateRunner {
  async fn run(
    &self,
    flow: &Flow,
    mut inputs: AsyncNodeInputs,
  ) -> Result<HashMap<String, AsyncNodeResult>, YanshiError> {
    let mut keys: Vec<&String> = inputs.keys().collect();
    keys.sort();
    for key in keys {
      if !self.definition.inputs.contains_key(key) {
        let mut declared: Vec<&str> = self.definition.inputs.keys().map(String::as_str).collect();
        declared.sort_unstable();
        return Err(self.input_error(format!(
          "'{key}' is not one of its inputs ({})",
          declared.join(", ")
        )));
      }
      if !matches!(inputs[key], FlowValue::Json(_)) {
        return Err(self.input_error(format!("input '{key}' must be a plain JSON value")));
      }
    }
    apply_declared_inputs(&self.definition, &mut inputs)
      .map_err(|e| self.input_error(format!("{e:#}")))?;
    let mut config = self.config.clone();
    if let Some(limit) = self
      .definition
      .effective_cost_limit(config.cost_limit_usd)
      .map_err(|e| self.input_error(e))?
    {
      config = config.with_cost_limit_usd(limit);
    }
    let run_id = uuid::Uuid::new_v4().to_string();
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

/// Each `[[workflows]]` entry's name and parsed template, so a caller can
/// vet the templates (the gateway checks them against its policy) before
/// building anything.
pub fn load_definitions(
  manifest: &SkillManifest,
  skill_dir: &Path,
) -> Result<Vec<(String, FlowDefinitionV2)>> {
  manifest
    .workflows
    .iter()
    .map(|entry| {
      let path = skill_dir.join(&entry.path);
      let yaml = std::fs::read_to_string(&path)
        .with_context(|| format!("workflow tool '{}': reading {}", entry.name, path.display()))?;
      let definition = parse_workflow_definition(&yaml)
        .with_context(|| format!("workflow tool '{}': {}", entry.name, path.display()))?;
      Ok((entry.name.clone(), definition))
    })
    .collect()
}

/// Build the skill's workflow tools. `model` (the skill's resolved model
/// or `--model`) is applied to the templates' llm nodes; every run uses
/// `config` (run base dir, scheduling, cancellation, a cost cap the
/// template's own limit is combined with).
pub fn build(
  manifest: &SkillManifest,
  skill_dir: &Path,
  model: Option<&str>,
  config: FlowExecutionConfig,
) -> Result<WorkflowTools> {
  let runs = Arc::new(RecordedRuns::default());
  let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
  let definitions = load_definitions(manifest, skill_dir)?;
  for (entry, (name, definition)) in manifest.workflows.iter().zip(definitions) {
    let flow = build_flow_from_definition(&definition, model)
      .with_context(|| format!("workflow tool '{name}': building {}", entry.path))?;
    let has_deliverable = definition.nodes.iter().any(|node| node.id == "deliverable");
    let description = entry
      .description
      .clone()
      .unwrap_or_else(|| format!("Run the '{}' workflow", definition.name));
    let schema = inputs_json_schema(&definition);
    let runner = Arc::new(TemplateRunner {
      tool: name.clone(),
      definition,
      config: config.clone(),
      runs: runs.clone(),
    });
    let mut tool = WorkflowTool::with_schema(name, description, schema, flow, runner);
    if has_deliverable {
      tool = tool.with_result_output("deliverable", "deliverable");
    }
    tools.push(Arc::new(tool));
  }
  Ok(WorkflowTools { tools, runs })
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  const ECHO: &str = r#"
name: echo
inputs:
  topic:
    default: "tides"
nodes:
  - id: deliverable
    type: template
    parameters:
      template: "about {{ topic }}"
"#;

  fn echo_tool(dir: &Path) -> WorkflowTools {
    std::fs::write(dir.join("echo.yml"), ECHO).expect("template");
    let manifest: SkillManifest = serde_json::from_value(json!({
      "skill": {"name": "s", "version": "0.1.0", "description": "test"},
      "persona": {"role": "test"},
      "workflows": [{"name": "echo", "path": "echo.yml"}]
    }))
    .expect("manifest");
    build(
      &manifest,
      dir,
      None,
      FlowExecutionConfig::serial().with_run_base_dir(dir.join("runs")),
    )
    .expect("tools")
  }

  #[tokio::test]
  async fn declared_inputs_run_and_are_recorded() {
    let dir = tempfile::tempdir().expect("dir");
    let tools = echo_tool(dir.path());
    tools.tools[0]
      .execute(json!({"topic": "moons"}))
      .await
      .expect("runs");
    let runs = tools.runs.take();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].tool, "echo");
  }

  #[tokio::test]
  async fn undeclared_and_file_shaped_inputs_are_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let tools = echo_tool(dir.path());
    let error = tools.tools[0]
      .execute(json!({"topic": "moons", "path": "/etc/passwd"}))
      .await
      .expect_err("undeclared input");
    assert!(
      error
        .to_string()
        .contains("'path' is not one of its inputs (topic)"),
      "{error}"
    );
    let error = tools.tools[0]
      .execute(json!({"topic": {"type": "file", "path": "/etc/passwd"}}))
      .await
      .expect_err("file-shaped input");
    assert!(error.to_string().contains("plain JSON value"), "{error}");
    assert!(tools.runs.take().is_empty(), "nothing ran");
  }
}
