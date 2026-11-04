use crate::config::v2::NodeDefinitionV2;
use yanshi_agents::{AgentNodeResumeContract, AgentRunResult};
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  flow::{GraphNode, NodeType},
  value::FlowValue,
};
use yanshi_llm::Yanshi;
// Tool-tier nodes (no capability deps) stay in `yanshi-nodes`; the
// capability-backed nodes moved to `yanshi-nodes-ai` (P-A nodes split).
use async_trait::async_trait;
use serde_json::{Value, json};
use yanshi_nodes::nodes::{
  arxiv::ArxivNode, file::FileNode, http::HttpNode, markmap::MarkMapNode, template::TemplateNode,
};
use yanshi_nodes_ai::nodes::{
  asr::ASRNode, image_edit::ImageEditNode, image_to_image::ImageToImageNode,
  image_understand::ImageUnderstandNode, llm::LlmNode, text_to_image::TextToImageNode,
  text_to_video::TextToVideoNode, tts::TTSNode,
};
use yanshi_skills::{SkillBuilder, SkillLoader};
use yanshi_tools::{SandboxPolicy, ToolRegistry};

#[cfg(feature = "mcp")]
use yanshi_nodes_ai::nodes::mcp::MCPNode;

#[cfg(feature = "rag")]
use yanshi_nodes_ai::nodes::rag::RAGNode;

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// Helper to get a string parameter from the node definition, returning a default if not found.
/// Y1.1: `output_format: file | data_uri` on the five media-producing
/// node types (`tts` / `text_to_image` / `image_to_image` / `image_edit` /
/// `text_to_video`). Absent means `file`.
fn get_output_format(
  params: &HashMap<String, serde_yaml::Value>,
) -> Result<yanshi_nodes_ai::ArtifactOutputFormat> {
  let raw = params.get("output_format").and_then(|v| v.as_str());
  yanshi_nodes_ai::ArtifactOutputFormat::parse(raw).map_err(|e| anyhow!("{e}"))
}

fn get_string_param_optional(params: &HashMap<String, serde_yaml::Value>, key: &str) -> String {
  params
    .get(key)
    .and_then(|v| v.as_str())
    .map(|s| s.to_string())
    .unwrap_or_default()
}

pub fn create_graph_node(node_def: &NodeDefinitionV2) -> Result<GraphNode> {
  let node_type = match node_def.node_type.as_str() {
    "llm" => Ok(NodeType::Standard(Arc::new(LlmNode))),
    "skill_agent" | "agent" => {
      let node = SkillAgentWorkflowNode::new(&node_def.id);
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "multi_agent" => {
      let node = crate::executor::multi_agent::MultiAgentNode::from_params(
        &node_def.id,
        &node_def.parameters,
      )
      .map_err(|err| anyhow!("multi_agent '{}': {}", node_def.id, err))?;
      Ok(NodeType::Standard(Arc::new(node)))
    }
    #[cfg(feature = "plugin")]
    "plugin" => {
      let manifest = get_string_param_optional(&node_def.parameters, "manifest");
      if manifest.is_empty() {
        return Err(anyhow!(
          "plugin node '{}' requires a 'manifest' parameter (path to plugin.toml)",
          node_def.id
        ));
      }
      let plugin_node_type = get_string_param_optional(&node_def.parameters, "node_type");
      if plugin_node_type.is_empty() {
        return Err(anyhow!(
          "plugin node '{}' requires a 'node_type' parameter (declared by the plugin)",
          node_def.id
        ));
      }
      let node = crate::executor::plugin::PluginWorkflowNode::new(
        &node_def.id,
        std::path::PathBuf::from(manifest),
        plugin_node_type,
      );
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "http" => Ok(NodeType::Standard(Arc::new(HttpNode::default()))),
    "file" => {
      // V0.2 closure: `FileNode::default()` denies every path
      // (`SandboxPolicy::default()`'s `allowed_paths` is empty), so a
      // `file` node's `allowed_paths` is mandatory here — mirroring the
      // `shell` node's mandatory `allowed_commands` below. Empty/missing
      // fails at parse time rather than at run time with an opaque
      // "sandbox policy denied" on every call.
      let allowed_paths: Vec<PathBuf> = node_def
        .parameters
        .get("allowed_paths")
        .and_then(|v| v.as_sequence())
        .ok_or_else(|| {
          anyhow!(
            "file node '{}' requires 'allowed_paths' as a YAML sequence of path prefixes \
             (e.g. ['./workspace', '/tmp/yanshi']) — empty/missing would deny every path",
            node_def.id
          )
        })?
        .iter()
        .filter_map(|v| v.as_str().map(PathBuf::from))
        .collect();

      if allowed_paths.is_empty() {
        return Err(anyhow!(
          "file node '{}': 'allowed_paths' must contain at least one path prefix",
          node_def.id
        ));
      }

      let policy = Arc::new(SandboxPolicy {
        allowed_paths,
        ..SandboxPolicy::default()
      });
      Ok(NodeType::Standard(Arc::new(FileNode::new(policy))))
    }
    "shell" => {
      // F-A7-2 closure: shell node wraps `yanshi_tools::ShellTool`
      // with a SandboxPolicy built from YAML params. `allowed_commands`
      // is mandatory (see `ShellWorkflowNode::from_params`); workflows
      // without it fail at parse time rather than running with an
      // empty allowlist that would block every command.
      let node =
        crate::executor::shell::ShellWorkflowNode::from_params(&node_def.id, &node_def.parameters)
          .map_err(|err| anyhow!("shell node '{}': {}", node_def.id, err))?;
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "template" => {
      let template_str = get_string_param_optional(&node_def.parameters, "template");
      let mut node = TemplateNode::new(&node_def.id, &template_str);

      let output_key = get_string_param_optional(&node_def.parameters, "output_key");
      if !output_key.is_empty() {
        node = node.with_output_key(&output_key);
      }

      let output_format = get_string_param_optional(&node_def.parameters, "output_format");
      if !output_format.is_empty() {
        node = node.with_format(&output_format);
      }

      Ok(NodeType::Standard(Arc::new(node)))
    }
    "arxiv" => {
      let url = get_string_param_optional(&node_def.parameters, "url");
      let fetch_source = node_def
        .parameters
        .get("fetch_source")
        .and_then(|v| v.as_bool());
      let simplify_latex = node_def
        .parameters
        .get("simplify_latex")
        .and_then(|v| v.as_bool());
      let node = ArxivNode {
        name: node_def.id.clone(),
        url,
        fetch_source,
        simplify_latex,
      };
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "asr" => {
      let model = get_string_param_optional(&node_def.parameters, "model");
      let audio_source = get_string_param_optional(&node_def.parameters, "audio_source");
      let node = ASRNode::new(&node_def.id, &model, &audio_source);
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "image_edit" => {
      let model = get_string_param_optional(&node_def.parameters, "model");
      let prompt = get_string_param_optional(&node_def.parameters, "prompt");
      let image_source = get_string_param_optional(&node_def.parameters, "image_source");
      let node = ImageEditNode::new(&node_def.id, &model, &prompt, &image_source)
        .with_output_format(get_output_format(&node_def.parameters)?);
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "image_to_image" => {
      let model = get_string_param_optional(&node_def.parameters, "model");
      let prompt = get_string_param_optional(&node_def.parameters, "prompt");
      let source_image = get_string_param_optional(&node_def.parameters, "source_image");
      let node = ImageToImageNode::new(&node_def.id, &model, &prompt, &source_image)
        .with_output_format(get_output_format(&node_def.parameters)?);
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "image_understand" => {
      let model = get_string_param_optional(&node_def.parameters, "model");
      let text_prompt = get_string_param_optional(&node_def.parameters, "text_prompt");
      let image_source = get_string_param_optional(&node_def.parameters, "image_source");
      let node = ImageUnderstandNode::new(&node_def.id, &model, &text_prompt, &image_source);
      Ok(NodeType::Standard(Arc::new(node)))
    }
    // Y3.1: in-process renderers; all inputs are read at execute time.
    "render_table" => Ok(NodeType::Standard(Arc::new(
      yanshi_nodes::nodes::render::RenderTableNode::new(&node_def.id),
    ))),
    "render_svg" => Ok(NodeType::Standard(Arc::new(
      yanshi_nodes::nodes::render::RenderSvgNode::new(&node_def.id),
    ))),
    "render_markdown" => Ok(NodeType::Standard(Arc::new(
      yanshi_nodes::nodes::render::RenderMarkdownNode::new(&node_def.id),
    ))),
    // Y3.3: external, pandoc (+ typst for PDF) under the OS sandbox.
    "render_document" => Ok(NodeType::Standard(Arc::new(
      yanshi_nodes::nodes::render::RenderDocumentNode::new(&node_def.id),
    ))),
    "markmap" => {
      // markdown content will be provided via input_mapping at runtime
      let markdown = get_string_param_optional(&node_def.parameters, "markdown");
      let mut node = MarkMapNode::new(node_def.id.clone(), markdown);

      // Check if save_to_file parameter is provided
      if let Some(save_path) = node_def
        .parameters
        .get("save_to_file")
        .and_then(|v| v.as_str())
      {
        node.save_to_file = Some(save_path.to_string());
      }

      Ok(NodeType::Standard(Arc::new(node)))
    }
    "text_to_image" => {
      // The `prompt` (a `{{input}}` template) used to be dropped here, so
      // every YAML text_to_image call sent the vendor an empty prompt.
      let params = &node_def.parameters;
      let model = get_string_param_optional(params, "model");
      let mut node = TextToImageNode::new(&node_def.id, &model)
        .with_prompt(&get_string_param_optional(params, "prompt"))
        .with_output_format(get_output_format(params)?);
      let negative = get_string_param_optional(params, "negative_prompt");
      if !negative.is_empty() {
        node = node.with_negative_prompt(&negative);
      }
      let size = get_string_param_optional(params, "size");
      if !size.is_empty() {
        node = node.with_size(&size);
      }
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "text_to_video" => {
      let params = &node_def.parameters;
      let model = get_string_param_optional(params, "model");
      let mut node = TextToVideoNode::new(&node_def.id, &model)
        .with_prompt(&get_string_param_optional(params, "prompt"))
        .with_output_format(get_output_format(params)?);
      let aspect_ratio = get_string_param_optional(params, "aspect_ratio");
      if !aspect_ratio.is_empty() {
        node = node.with_aspect_ratio(&aspect_ratio);
      }
      let resolution = get_string_param_optional(params, "resolution");
      if !resolution.is_empty() {
        node = node.with_resolution(&resolution);
      }
      if let Some(seconds) = params.get("duration_seconds").and_then(|v| v.as_u64()) {
        node = node.with_duration_seconds(u32::try_from(seconds).unwrap_or(u32::MAX));
      }
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "tts" => {
      let model = get_string_param_optional(&node_def.parameters, "model");
      let voice = get_string_param_optional(&node_def.parameters, "voice");
      let input_template = get_string_param_optional(&node_def.parameters, "input_template");
      let node = TTSNode::new(&node_def.id, &model, &voice, &input_template)
        .with_output_format(get_output_format(&node_def.parameters)?);
      Ok(NodeType::Standard(Arc::new(node)))
    }
    "while" => {
      let condition = get_string_param_optional(&node_def.parameters, "condition");
      let max_iterations = node_def
        .parameters
        .get("max_iterations")
        .and_then(|v| v.as_u64())
        .context("While node requires a 'max_iterations' parameter")?
        as u32;
      let do_nodes_yaml = node_def
        .parameters
        .get("do")
        .context("While node requires a 'do' block")?;
      let do_nodes_def: Vec<NodeDefinitionV2> = serde_yaml::from_value(do_nodes_yaml.clone())?;
      let template: Vec<GraphNode> = do_nodes_def
        .iter()
        .map(create_graph_node)
        .collect::<Result<_>>()?;
      // D11 (W2.4): both default to `false` — a sub-flow node failure
      // surfaces as a While-node-level error by default (opt into the
      // legacy swallow-and-continue behavior via `continue_on_error:
      // true`), and exhausting `max_iterations` only warns by default
      // (opt into failing via `fail_on_exhausted: true`).
      let continue_on_error = node_def
        .parameters
        .get("continue_on_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
      let fail_on_exhausted = node_def
        .parameters
        .get("fail_on_exhausted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
      Ok(NodeType::While {
        condition,
        max_iterations,
        template,
        continue_on_error,
        fail_on_exhausted,
      })
    }
    "map" => {
      let template_nodes_yaml = node_def
        .parameters
        .get("template")
        .context("Map node requires a 'template' block")?;
      let template_nodes_def: Vec<NodeDefinitionV2> =
        serde_yaml::from_value(template_nodes_yaml.clone())?;
      let parallel = node_def
        .parameters
        .get("parallel")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
      // F-A6-1: optional `max_concurrent: N` cap when `parallel:
      // true`. Defaults to None (legacy unbounded behaviour) for
      // back-compat. Ignored when `parallel: false`.
      let max_concurrent = node_def
        .parameters
        .get("max_concurrent")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
      let template: Vec<GraphNode> = template_nodes_def
        .iter()
        .map(create_graph_node)
        .collect::<Result<_>>()?;
      Ok(NodeType::Map {
        template,
        parallel,
        max_concurrent,
      })
    }
    #[cfg(feature = "mcp")]
    "mcp" => {
      // Extract server_command (required)
      let server_command = node_def
        .parameters
        .get("server_command")
        .and_then(|v| match v {
          serde_yaml::Value::Sequence(seq) => seq
            .iter()
            .map(|s| s.as_str().map(|s| s.to_string()))
            .collect(),
          _ => None,
        })
        .context("MCP node requires 'server_command' as an array of strings")?;

      // Extract tool_name (required)
      let tool_name = get_string_param_optional(&node_def.parameters, "tool_name");
      if tool_name.is_empty() {
        return Err(anyhow!("MCP node requires 'tool_name' parameter"));
      }

      // Extract tool_params (optional, default to empty object)
      let tool_params = node_def
        .parameters
        .get("tool_params")
        .map(|v| serde_yaml::from_value(v.clone()))
        .transpose()?
        .unwrap_or(serde_json::json!({}));

      // Create MCPNode
      let mut node = MCPNode::new(server_command, tool_name).with_params(tool_params);

      // Optional timeout_ms
      if let Some(timeout) = node_def
        .parameters
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
      {
        node = node.with_timeout_ms(timeout);
      }

      // Optional max_retries
      if let Some(retries) = node_def
        .parameters
        .get("max_retries")
        .and_then(|v| v.as_u64())
      {
        node = node.with_max_retries(retries as u32);
      }

      Ok(NodeType::Standard(Arc::new(node)))
    }
    #[cfg(feature = "rag")]
    "rag" => {
      // Extract operation (required)
      let operation = get_string_param_optional(&node_def.parameters, "operation");
      if operation.is_empty() {
        return Err(anyhow!(
          "RAG node requires 'operation' parameter (search, index, create_collection, delete_collection, stats)"
        ));
      }

      // Extract collection (required)
      let collection = get_string_param_optional(&node_def.parameters, "collection");
      if collection.is_empty() {
        return Err(anyhow!("RAG node requires 'collection' parameter"));
      }

      // Create RAGNode with builder pattern
      let mut node = RAGNode::new(operation, collection);

      // Optional qdrant_url
      let qdrant_url = get_string_param_optional(&node_def.parameters, "qdrant_url");
      if !qdrant_url.is_empty() {
        node = node.with_qdrant_url(qdrant_url);
      }

      // Optional embedding_model
      let embedding_model = get_string_param_optional(&node_def.parameters, "embedding_model");
      if !embedding_model.is_empty() {
        node = node.with_embedding_model(embedding_model);
      }

      Ok(NodeType::Standard(Arc::new(node)))
    }
    _ => Err(anyhow!("Unknown node type: {}", node_def.node_type)),
  }?;

  // T3.1: generic timeout/retry wrapping — see `executor::timeout_retry`.
  // Only `NodeType::Standard` is supported; `config::schema` already
  // rejects `timeout_ms`/`max_retries` on `map`/`while` at validate time,
  // this is defense in depth for a caller that builds a `Flow` without
  // going through `validate_flow_definition` first.
  let node_type = match node_type {
    NodeType::Standard(inner) => {
      NodeType::Standard(crate::executor::timeout_retry::wrap_if_configured(
        inner,
        node_def.timeout_ms,
        node_def.max_retries,
      ))
    }
    other @ (NodeType::Map { .. } | NodeType::While { .. }) => {
      if node_def.timeout_ms.is_some() || node_def.max_retries.is_some() {
        return Err(anyhow!(
          "node '{}': timeout_ms/max_retries are not supported on '{}' nodes",
          node_def.id,
          node_def.node_type
        ));
      }
      other
    }
  };

  let mut input_mapping = HashMap::new();
  for (k, v) in &node_def.input_mapping {
    // V4.4: match schema.rs's `parse_mapping_source_node` exactly — trim
    // the outer whitespace, then the literal `{{`/`}}` tokens (not the
    // 3-char `"{{ "`/`" }}"` sequences this used to strip), then the
    // interior whitespace again. The old exact-space-count matching let
    // an `input_mapping` entry pass schema validation (which already used
    // the tolerant form below) while silently resolving to no input at
    // all here — or, worse, a corrupted field name — the moment the YAML
    // author's spacing didn't match the canonical single-space style.
    let path = v
      .trim()
      .trim_start_matches("{{")
      .trim_end_matches("}}")
      .trim();
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() == 4 && parts[0] == "nodes" && parts[2] == "outputs" {
      input_mapping.insert(k.clone(), (parts[1].to_string(), parts[3].to_string()));
    } else if parts.len() >= 2 && parts[0] == "item" {
      // F-A6-5: `{{ item.field }}` / `{{ item.foo.bar }}` lookups
      // inside a map sub-flow. Encoded with the sentinel source-node
      // id "!item" (the `!` prefix can't appear in a YAML node id, so
      // it can't shadow a real node — `yanshi_core::Flow` will
      // detect the sentinel at resolve time and walk the dotted path
      // in the seeded `item` initial input).
      let item_path = parts[1..].join(".");
      input_mapping.insert(k.clone(), ("!item".to_string(), item_path));
    }
  }

  let mut initial_inputs = HashMap::new();
  for (k, v) in &node_def.parameters {
    if k == "do" || k == "template" {
      continue;
    }
    // For `type: plugin`, `manifest` and `node_type` configure the wrapper
    // itself; they must not leak into the inputs forwarded to the plugin.
    if node_def.node_type == "plugin" && (k == "manifest" || k == "node_type") {
      continue;
    }
    let json_val: serde_json::Value = serde_yaml::from_value(v.clone())?;
    let flow_value = yanshi_core::value::FlowValue::Json(json_val);
    initial_inputs.insert(k.clone(), flow_value);
  }

  Ok(GraphNode {
    id: node_def.id.clone(),
    node_type,
    dependencies: node_def.dependencies.clone(),
    input_mapping: Some(input_mapping),
    run_if: node_def.run_if.clone(),
    initial_inputs,
  })
}

#[derive(Debug, Clone)]
struct SkillAgentWorkflowNode {
  name: String,
}

impl SkillAgentWorkflowNode {
  fn new(name: &str) -> Self {
    Self {
      name: name.to_string(),
    }
  }
}

#[async_trait]
impl AsyncNode for SkillAgentWorkflowNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let skill_dir =
      get_required_string(inputs, "skill").map_err(|message| YanshiError::NodeInputError {
        message: format!("skill_agent '{}': {}", self.name, message),
      })?;
    let message =
      get_required_string(inputs, "message").map_err(|message| YanshiError::NodeInputError {
        message: format!("skill_agent '{}': {}", self.name, message),
      })?;
    let model_override =
      get_optional_string(inputs, "model").map_err(|message| YanshiError::NodeInputError {
        message: format!("skill_agent '{}': {}", self.name, message),
      })?;

    let dir = std::path::Path::new(skill_dir);
    let mut manifest = SkillLoader::load(dir).map_err(|err| YanshiError::NodeInputError {
      message: format!(
        "skill_agent '{}': failed to load skill '{}': {}",
        self.name, skill_dir, err
      ),
    })?;
    if let Some(model) = model_override {
      manifest.model.name = Some(model.to_string());
    }
    SkillLoader::validate(&manifest, dir).map_err(|err| YanshiError::NodeInputError {
      message: format!(
        "skill_agent '{}': skill validation failed for '{}': {}",
        self.name, skill_dir, err
      ),
    })?;

    Yanshi::init()
      .await
      .map_err(|err| YanshiError::ConfigurationError {
        message: format!(
          "skill_agent '{}': failed to initialize LLM: {}",
          self.name, err
        ),
      })?;

    let mut agent = SkillBuilder::build(&manifest, dir).await.map_err(|err| {
      YanshiError::NodeExecutionFailed {
        message: format!(
          "skill_agent '{}': failed to build agent from skill '{}': {}",
          self.name, skill_dir, err
        ),
      }
    })?;

    let result =
      agent
        .run_with_trace(message)
        .await
        .map_err(|err| YanshiError::NodeExecutionFailed {
          message: format!("skill_agent '{}': agent run failed: {}", self.name, err),
        })?;

    let tools = agent.tools().clone();
    if !result.stop_reason.is_success() {
      let partial_outputs = build_skill_agent_outputs(&self.name, &result, &tools)?;
      return Err(YanshiError::NodePartialExecutionFailed {
        message: format!(
          "skill_agent '{}': agent stopped before final answer: {:?}",
          self.name, result.stop_reason
        ),
        partial_outputs,
      });
    }

    build_skill_agent_outputs(&self.name, &result, &tools)
  }
}

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

fn build_skill_agent_outputs(
  node_name: &str,
  result: &AgentRunResult,
  tools: &ToolRegistry,
) -> AsyncNodeResult {
  let response = result.answer.clone().unwrap_or_default();
  let stop_reason =
    serde_json::to_value(&result.stop_reason).map_err(|err| YanshiError::NodeExecutionFailed {
      message: format!(
        "skill_agent '{}': failed to serialize stop reason: {}",
        node_name, err
      ),
    })?;
  let agent_result =
    serde_json::to_value(result).map_err(|err| YanshiError::NodeExecutionFailed {
      message: format!(
        "skill_agent '{}': failed to serialize runtime result: {}",
        node_name, err
      ),
    })?;
  let agent_resume = serde_json::to_value(AgentNodeResumeContract::from_result_with_tools(
    node_name,
    "skill_agent",
    result,
    tools,
  ))
  .map_err(|err| YanshiError::NodeExecutionFailed {
    message: format!(
      "skill_agent '{}': failed to serialize resume contract: {}",
      node_name, err
    ),
  })?;

  let mut outputs = std::collections::HashMap::new();
  outputs.insert("response".to_string(), FlowValue::Json(json!(response)));
  outputs.insert(
    "session_id".to_string(),
    FlowValue::Json(json!(result.session_id)),
  );
  outputs.insert("stop_reason".to_string(), FlowValue::Json(stop_reason));
  outputs.insert("agent_result".to_string(), FlowValue::Json(agent_result));
  outputs.insert("agent_resume".to_string(), FlowValue::Json(agent_resume));
  Ok(outputs)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn node_from_yaml(yaml: &str) -> NodeDefinitionV2 {
    serde_yaml::from_str(yaml).expect("valid node YAML")
  }

  /// V0.2 regression: the factory must refuse to build a `file` node
  /// without `allowed_paths` rather than falling back to
  /// `FileNode::default()`'s (pre-V0.2 permissive, post-V0.2 deny-all)
  /// policy silently. `GraphNode` doesn't implement `Debug`, so
  /// `unwrap_err()` isn't available — match manually instead.
  #[test]
  fn file_node_without_allowed_paths_fails_at_build_time() {
    let node = node_from_yaml(
      r#"
id: read_it
type: file
parameters:
  operation: read
  path: /tmp/secret.txt
"#,
    );

    let err = match create_graph_node(&node) {
      Err(err) => err,
      Ok(_) => panic!("expected an error"),
    };
    assert!(
      err.to_string().contains("allowed_paths"),
      "expected an allowed_paths error, got: {err}"
    );
  }

  /// V0.2 regression: an empty `allowed_paths` sequence is rejected the
  /// same way a missing one is — it would otherwise build a `FileNode`
  /// whose every call fails with an opaque "sandbox policy denied".
  #[test]
  fn file_node_with_empty_allowed_paths_fails_at_build_time() {
    let node = node_from_yaml(
      r#"
id: read_it
type: file
parameters:
  operation: read
  path: /tmp/secret.txt
  allowed_paths: []
"#,
    );

    let err = match create_graph_node(&node) {
      Err(err) => err,
      Ok(_) => panic!("expected an error"),
    };
    assert!(
      err.to_string().contains("allowed_paths"),
      "expected an allowed_paths error, got: {err}"
    );
  }

  /// V0.2: with `allowed_paths` declared, the factory builds a `FileNode`
  /// whose policy actually enforces that allow-list end to end. Neither
  /// path needs to exist on disk — the allow-list check runs (and denies
  /// the outside path) before any filesystem I/O is attempted.
  #[tokio::test]
  async fn file_node_with_allowed_paths_builds_an_enforcing_node() {
    let node = node_from_yaml(
      r#"
id: write_it
type: file
parameters:
  operation: write
  path: "/definitely/outside/the/allow-list.txt"
  content: "nope"
  allowed_paths: ["/allowed/subtree"]
"#,
    );

    let graph_node = create_graph_node(&node).unwrap();
    let NodeType::Standard(async_node) = graph_node.node_type else {
      panic!("expected a Standard node");
    };

    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("operation".to_string(), FlowValue::Json(json!("write")));
    inputs.insert(
      "path".to_string(),
      FlowValue::Json(json!("/definitely/outside/the/allow-list.txt")),
    );
    inputs.insert("content".to_string(), FlowValue::Json(json!("nope")));

    let err = async_node.execute(&inputs).await.unwrap_err();
    assert!(
      err.to_string().contains("sandbox policy denied"),
      "expected a path outside allowed_paths to be denied, got: {err}"
    );
  }

  /// V4.4 regression: `input_mapping` values with non-canonical spacing
  /// around the `{{ }}` template braces used to pass schema validation
  /// (which already tolerated any whitespace) but resolve to no mapping
  /// at all here — a silently misconfigured node with no error anywhere.
  /// Covers zero, one (canonical), and multiple spaces on both sides.
  #[test]
  fn input_mapping_tolerates_non_canonical_brace_whitespace() {
    let node = node_from_yaml(
      r#"
id: consume
type: template
input_mapping:
  zero: "{{nodes.producer.outputs.field}}"
  one: "{{ nodes.producer.outputs.field }}"
  many: "{{   nodes.producer.outputs.field   }}"
"#,
    );

    let graph_node = create_graph_node(&node).unwrap();
    let mapping = graph_node
      .input_mapping
      .expect("mapping should be populated");
    for key in ["zero", "one", "many"] {
      assert_eq!(
        mapping.get(key),
        Some(&("producer".to_string(), "field".to_string())),
        "input_mapping[\"{key}\"] did not resolve to (producer, field)"
      );
    }
  }

  /// V4.4 regression: the specific asymmetric case (space before `}}`
  /// but none after `{{`, or vice versa) used to silently corrupt the
  /// resolved field name (e.g. trailing `"}}"`  characters left attached)
  /// instead of either working or failing loudly.
  #[test]
  fn input_mapping_tolerates_asymmetric_brace_whitespace() {
    let node = node_from_yaml(
      r#"
id: consume
type: template
input_mapping:
  leading_only: "{{ nodes.producer.outputs.field}}"
  trailing_only: "{{nodes.producer.outputs.field }}"
"#,
    );

    let graph_node = create_graph_node(&node).unwrap();
    let mapping = graph_node
      .input_mapping
      .expect("mapping should be populated");
    for key in ["leading_only", "trailing_only"] {
      assert_eq!(
        mapping.get(key),
        Some(&("producer".to_string(), "field".to_string())),
        "input_mapping[\"{key}\"] did not resolve to (producer, field)"
      );
    }
  }
}
