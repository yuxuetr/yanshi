//! Workflow node for `type: multi_agent`.
//!
//! Dispatches to one of three multi-agent supervisors — `HandoffSupervisor`,
//! `BlackboardSupervisor`, or `DebateSupervisor` (in `yanshi-agents`) —
//! based on the YAML `mode` field. Each participant references a skill
//! directory the same way `skill_agent` nodes do.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use yanshi_agents::supervisor::{
  Blackboard, BlackboardReadTool, BlackboardSchedule, BlackboardStop, BlackboardSupervisorBuilder,
  BlackboardWriteTool, DebateSupervisorBuilder, HandoffSignal, HandoffSupervisorBuilder,
  HandoffTool,
};
use yanshi_agents::{AgentContext, AgentRunResult, AgentRuntime};
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};
use yanshi_llm::Yanshi;
use yanshi_skills::{SkillBuilder, SkillLoader, SkillManifest};

// ── YAML config ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MultiAgentSpec {
  /// Logical name used for handoff targets / blackboard write attribution /
  /// debate proposal labels. Must be unique within the node.
  name: String,
  /// Filesystem path to the skill directory whose manifest builds this agent.
  skill: String,
}

#[derive(Debug, Deserialize)]
struct BlackboardScheduleYaml {
  #[serde(default)]
  mode: Option<String>, // "sequential" | "parallel"
  #[serde(default)]
  agents: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct BlackboardStopYaml {
  #[serde(rename = "type")]
  kind: String, // "all_completed" | "key_set"
  #[serde(default)]
  key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum MultiAgentConfig {
  Handoff {
    agents: Vec<MultiAgentSpec>,
    #[serde(default)]
    initial_agent: Option<String>,
    #[serde(default = "default_max_handoffs")]
    max_handoffs: usize,
  },
  Blackboard {
    agents: Vec<MultiAgentSpec>,
    #[serde(default)]
    schedule: Option<BlackboardScheduleYaml>,
    #[serde(default)]
    stop_when: Option<BlackboardStopYaml>,
    #[serde(default)]
    answer_from: Option<String>,
  },
  Debate {
    participants: Vec<MultiAgentSpec>,
    judge: MultiAgentSpec,
    #[serde(default = "default_rounds")]
    rounds: usize,
    #[serde(default)]
    judge_prompt: Option<String>,
  },
}

fn default_max_handoffs() -> usize {
  5
}
fn default_rounds() -> usize {
  1
}

impl MultiAgentConfig {
  /// Build a `MultiAgentConfig` from a node's flat YAML parameter map.
  pub fn from_params(params: &HashMap<String, serde_yaml::Value>) -> anyhow::Result<Self> {
    let mut mapping = serde_yaml::Mapping::new();
    for (k, v) in params {
      mapping.insert(serde_yaml::Value::String(k.clone()), v.clone());
    }
    let value = serde_yaml::Value::Mapping(mapping);
    serde_yaml::from_value(value)
      .map_err(|e| anyhow::anyhow!("multi_agent config could not be parsed: {e}"))
  }
}

// ── Node ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct MultiAgentNode {
  name: String,
  config: MultiAgentConfig,
}

impl MultiAgentNode {
  pub fn from_params(
    name: impl Into<String>,
    params: &HashMap<String, serde_yaml::Value>,
  ) -> anyhow::Result<Self> {
    Ok(Self {
      name: name.into(),
      config: MultiAgentConfig::from_params(params)?,
    })
  }

  fn input_error(&self, message: impl Into<String>) -> YanshiError {
    YanshiError::NodeInputError {
      message: format!("multi_agent '{}': {}", self.name, message.into()),
    }
  }

  fn execution_error(&self, message: impl Into<String>) -> YanshiError {
    YanshiError::NodeExecutionFailed {
      message: format!("multi_agent '{}': {}", self.name, message.into()),
    }
  }

  async fn load_manifest(
    &self,
    skill_path: &str,
  ) -> Result<(std::path::PathBuf, SkillManifest), YanshiError> {
    let dir = std::path::PathBuf::from(skill_path);
    let manifest = SkillLoader::load(&dir)
      .map_err(|err| self.input_error(format!("failed to load skill '{}': {}", skill_path, err)))?;
    SkillLoader::validate(&manifest, &dir).map_err(|err| {
      self.input_error(format!(
        "skill validation failed for '{}': {}",
        skill_path, err
      ))
    })?;
    Ok((dir, manifest))
  }
}

#[async_trait]
impl AsyncNode for MultiAgentNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let message = get_required_string(inputs, "message")
      .map_err(|m| self.input_error(m))?
      .to_string();
    let model_override = get_optional_string(inputs, "model").map_err(|m| self.input_error(m))?;

    Yanshi::ensure_init()
      .await
      .map_err(|err| YanshiError::ConfigurationError {
        message: format!(
          "multi_agent '{}': failed to initialize LLM: {}",
          self.name, err
        ),
      })?;

    let result = match &self.config {
      MultiAgentConfig::Handoff {
        agents,
        initial_agent,
        max_handoffs,
      } => {
        self
          .run_handoff(
            agents,
            initial_agent.as_deref(),
            *max_handoffs,
            &message,
            model_override,
          )
          .await
      }
      MultiAgentConfig::Blackboard {
        agents,
        schedule,
        stop_when,
        answer_from,
      } => {
        self
          .run_blackboard(
            agents,
            schedule.as_ref(),
            stop_when.as_ref(),
            answer_from.as_deref(),
            &message,
            model_override,
          )
          .await
      }
      MultiAgentConfig::Debate {
        participants,
        judge,
        rounds,
        judge_prompt,
      } => {
        self
          .run_debate(
            participants,
            judge,
            *rounds,
            judge_prompt.as_deref(),
            &message,
            model_override,
          )
          .await
      }
    }?;

    if !result.stop_reason.is_success() {
      let partial_outputs = build_outputs(&self.name, &result)?;
      return Err(YanshiError::NodePartialExecutionFailed {
        message: format!(
          "multi_agent '{}': supervisor stopped before final answer: {:?}",
          self.name, result.stop_reason
        ),
        partial_outputs,
      });
    }
    build_outputs(&self.name, &result)
  }
}

impl MultiAgentNode {
  async fn run_handoff(
    &self,
    specs: &[MultiAgentSpec],
    initial: Option<&str>,
    max_handoffs: usize,
    message: &str,
    model_override: Option<&str>,
  ) -> Result<AgentRunResult, YanshiError> {
    let target_names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
    let signal = HandoffSignal::new();
    let handoff_tool: Arc<dyn yanshi_tools::Tool> =
      Arc::new(HandoffTool::new(target_names, signal.clone()));

    let mut builder = HandoffSupervisorBuilder::new().use_signal(signal);
    if let Some(name) = initial {
      builder = builder.initial_agent(name);
    }
    builder = builder.max_handoffs(max_handoffs);

    for spec in specs {
      let (dir, mut manifest) = self.load_manifest(&spec.skill).await?;
      if let Some(model) = model_override {
        manifest.model.name = Some(model.to_string());
      }
      let extra_tool = handoff_tool.clone();
      let agent = SkillBuilder::build_with_extra_tools(&manifest, &dir, vec![extra_tool])
        .await
        .map_err(|err| {
          self.execution_error(format!(
            "failed to build agent for '{}': {}",
            spec.name, err
          ))
        })?;
      let name = spec.name.clone();
      let description = manifest.skill.description.clone();
      builder = builder.add_agent(name, description, move |_handoff| agent);
    }

    let mut supervisor = builder
      .build()
      .map_err(|err| self.input_error(format!("handoff: {err}")))?;
    let context = AgentContext::new(supervisor.session_id().to_string(), message, "");
    AgentRuntime::run(&mut supervisor, context)
      .await
      .map_err(|err| self.execution_error(err.to_string()))
  }

  async fn run_blackboard(
    &self,
    specs: &[MultiAgentSpec],
    schedule_yaml: Option<&BlackboardScheduleYaml>,
    stop_yaml: Option<&BlackboardStopYaml>,
    answer_from: Option<&str>,
    message: &str,
    model_override: Option<&str>,
  ) -> Result<AgentRunResult, YanshiError> {
    let blackboard = Blackboard::new();
    let mut builder = BlackboardSupervisorBuilder::new();

    for spec in specs {
      let (dir, mut manifest) = self.load_manifest(&spec.skill).await?;
      if let Some(model) = model_override {
        manifest.model.name = Some(model.to_string());
      }
      let agent_name = spec.name.clone();
      let extras: Vec<Arc<dyn yanshi_tools::Tool>> = vec![
        Arc::new(BlackboardReadTool::new(blackboard.clone(), &agent_name)),
        Arc::new(BlackboardWriteTool::new(blackboard.clone(), &agent_name)),
      ];
      let agent = SkillBuilder::build_with_extra_tools(&manifest, &dir, extras)
        .await
        .map_err(|err| {
          self.execution_error(format!(
            "failed to build agent for '{}': {}",
            spec.name, err
          ))
        })?;
      let description = manifest.skill.description.clone();
      builder = builder.add_agent(agent_name, description, move |_bb| agent);
    }

    if let Some(schedule_yaml) = schedule_yaml {
      let mode = schedule_yaml.mode.as_deref().unwrap_or("sequential");
      let agents = schedule_yaml
        .agents
        .clone()
        .unwrap_or_else(|| specs.iter().map(|s| s.name.clone()).collect());
      let schedule = match mode {
        "sequential" => BlackboardSchedule::Sequential(agents),
        "parallel" => BlackboardSchedule::Parallel(agents),
        other => {
          return Err(self.input_error(format!(
            "unknown blackboard schedule '{}': expected sequential|parallel",
            other
          )));
        }
      };
      builder = builder.schedule(schedule);
    }

    if let Some(stop) = stop_yaml {
      let stop = match stop.kind.as_str() {
        "all_completed" => BlackboardStop::AllAgentsCompleted,
        "key_set" => {
          let key = stop
            .key
            .as_ref()
            .ok_or_else(|| self.input_error("blackboard stop_when type=key_set requires 'key'"))?;
          BlackboardStop::KeySet(key.clone())
        }
        other => {
          return Err(self.input_error(format!(
            "unknown blackboard stop type '{}': expected all_completed|key_set",
            other
          )));
        }
      };
      builder = builder.stop_when(stop);
    }

    if let Some(key) = answer_from {
      builder = builder.answer_from(key);
    }

    let mut supervisor = builder
      .build()
      .map_err(|err| self.input_error(format!("blackboard: {err}")))?;
    let context = AgentContext::new(supervisor.session_id().to_string(), message, "");
    AgentRuntime::run(&mut supervisor, context)
      .await
      .map_err(|err| self.execution_error(err.to_string()))
  }

  async fn run_debate(
    &self,
    participants: &[MultiAgentSpec],
    judge: &MultiAgentSpec,
    rounds: usize,
    judge_prompt: Option<&str>,
    message: &str,
    model_override: Option<&str>,
  ) -> Result<AgentRunResult, YanshiError> {
    let mut builder = DebateSupervisorBuilder::new().rounds(rounds);
    if let Some(prompt) = judge_prompt {
      builder = builder.judge_prompt(prompt);
    }

    for spec in participants {
      let agent = self.build_skill_agent(spec, model_override).await?;
      builder = builder.add_participant(spec.name.clone(), agent);
    }
    let judge_agent = self.build_skill_agent(judge, model_override).await?;
    builder = builder.judge(judge_agent);

    let mut supervisor = builder
      .build()
      .map_err(|err| self.input_error(format!("debate: {err}")))?;
    let context = AgentContext::new(supervisor.session_id().to_string(), message, "");
    AgentRuntime::run(&mut supervisor, context)
      .await
      .map_err(|err| self.execution_error(err.to_string()))
  }

  async fn build_skill_agent(
    &self,
    spec: &MultiAgentSpec,
    model_override: Option<&str>,
  ) -> Result<yanshi_agents::react::ReActAgent, YanshiError> {
    let (dir, mut manifest) = self.load_manifest(&spec.skill).await?;
    if let Some(model) = model_override {
      manifest.model.name = Some(model.to_string());
    }
    SkillBuilder::build(&manifest, &dir).await.map_err(|err| {
      self.execution_error(format!(
        "failed to build agent for '{}': {}",
        spec.name, err
      ))
    })
  }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn get_required_string<'a>(inputs: &'a AsyncNodeInputs, key: &str) -> Result<&'a str, String> {
  get_optional_string(inputs, key)?.ok_or_else(|| format!("required input '{}' is missing", key))
}

fn get_optional_string<'a>(
  inputs: &'a AsyncNodeInputs,
  key: &str,
) -> Result<Option<&'a str>, String> {
  match inputs.get(key) {
    None => Ok(None),
    Some(FlowValue::Json(Value::String(value))) => Ok(Some(value.as_str())),
    Some(_) => Err(format!("input '{}' must be a string", key)),
  }
}

fn build_outputs(node_name: &str, result: &AgentRunResult) -> AsyncNodeResult {
  let response = result.answer.clone().unwrap_or_default();
  let stop_reason =
    serde_json::to_value(&result.stop_reason).map_err(|err| YanshiError::NodeExecutionFailed {
      message: format!(
        "multi_agent '{}': failed to serialize stop reason: {}",
        node_name, err
      ),
    })?;
  let agent_result =
    serde_json::to_value(result).map_err(|err| YanshiError::NodeExecutionFailed {
      message: format!(
        "multi_agent '{}': failed to serialize runtime result: {}",
        node_name, err
      ),
    })?;

  let mut outputs: HashMap<String, FlowValue> = HashMap::new();
  outputs.insert("response".to_string(), FlowValue::Json(json!(response)));
  outputs.insert(
    "session_id".to_string(),
    FlowValue::Json(json!(result.session_id)),
  );
  outputs.insert("stop_reason".to_string(), FlowValue::Json(stop_reason));
  outputs.insert("agent_result".to_string(), FlowValue::Json(agent_result));
  Ok(outputs)
}

// Avoid unused-import warning on the helper module.
#[allow(dead_code)]
fn _typecheck_path(_p: &Path) {}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;

  fn yaml_map(pairs: &[(&str, &str)]) -> HashMap<String, serde_yaml::Value> {
    let mut m = HashMap::new();
    for (k, v) in pairs {
      m.insert(k.to_string(), serde_yaml::from_str(v).unwrap());
    }
    m
  }

  #[test]
  fn parses_handoff_config_with_defaults() {
    let params = yaml_map(&[
      ("mode", "handoff"),
      ("agents", "[{name: a, skill: ./a}, {name: b, skill: ./b}]"),
    ]);
    let cfg = MultiAgentConfig::from_params(&params).unwrap();
    match cfg {
      MultiAgentConfig::Handoff {
        agents,
        initial_agent,
        max_handoffs,
      } => {
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "a");
        assert!(initial_agent.is_none());
        assert_eq!(max_handoffs, 5);
      }
      other => panic!("expected handoff, got {other:?}"),
    }
  }

  #[test]
  fn parses_blackboard_config_with_full_options() {
    let params = yaml_map(&[
      ("mode", "blackboard"),
      ("agents", "[{name: r, skill: ./r}, {name: w, skill: ./w}]"),
      ("schedule", "{mode: parallel, agents: [r, w]}"),
      ("stop_when", "{type: key_set, key: report}"),
      ("answer_from", "report"),
    ]);
    let cfg = MultiAgentConfig::from_params(&params).unwrap();
    match cfg {
      MultiAgentConfig::Blackboard {
        agents,
        schedule,
        stop_when,
        answer_from,
      } => {
        assert_eq!(agents.len(), 2);
        let schedule = schedule.expect("schedule");
        assert_eq!(schedule.mode.as_deref(), Some("parallel"));
        let stop = stop_when.expect("stop_when");
        assert_eq!(stop.kind, "key_set");
        assert_eq!(stop.key.as_deref(), Some("report"));
        assert_eq!(answer_from.as_deref(), Some("report"));
      }
      other => panic!("expected blackboard, got {other:?}"),
    }
  }

  #[test]
  fn parses_debate_config_with_judge() {
    let params = yaml_map(&[
      ("mode", "debate"),
      (
        "participants",
        "[{name: a, skill: ./a}, {name: b, skill: ./b}]",
      ),
      ("judge", "{name: judge, skill: ./judge}"),
      ("rounds", "2"),
    ]);
    let cfg = MultiAgentConfig::from_params(&params).unwrap();
    match cfg {
      MultiAgentConfig::Debate {
        participants,
        judge,
        rounds,
        judge_prompt,
      } => {
        assert_eq!(participants.len(), 2);
        assert_eq!(judge.name, "judge");
        assert_eq!(rounds, 2);
        assert!(judge_prompt.is_none());
      }
      other => panic!("expected debate, got {other:?}"),
    }
  }

  #[test]
  fn rejects_unknown_mode() {
    let params = yaml_map(&[("mode", "ghost")]);
    let err = MultiAgentConfig::from_params(&params).unwrap_err();
    let s = err.to_string();
    assert!(s.contains("multi_agent"));
  }

  #[test]
  fn rejects_missing_judge_in_debate() {
    let params = yaml_map(&[
      ("mode", "debate"),
      ("participants", "[{name: a, skill: ./a}]"),
    ]);
    let err = MultiAgentConfig::from_params(&params).unwrap_err();
    let s = err.to_string();
    assert!(
      s.contains("judge") || s.contains("missing"),
      "expected error about missing 'judge', got: {s}"
    );
  }
}
