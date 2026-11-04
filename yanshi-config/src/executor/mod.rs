pub mod factory;
pub mod multi_agent;
pub mod shell;
mod timeout_retry;

#[cfg(feature = "plugin")]
pub mod plugin;

use crate::config::{
  schema::{WorkflowValidationReport, validate_flow_definition},
  v2::{FlowDefinitionV2, NodeDefinitionV2},
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use yanshi_core::{
  async_node::AsyncNodeInputs,
  flow::{Flow, GraphNode},
  value::FlowValue,
};

/// Parse and validate config-first workflow YAML.
pub fn parse_workflow_definition(yaml_content: &str) -> Result<FlowDefinitionV2> {
  let flow_def: FlowDefinitionV2 =
    serde_yaml::from_str(yaml_content).context("Failed to parse V2 workflow YAML.")?;
  let report = validate_flow_definition(&flow_def);
  if !report.is_valid() {
    bail!(format_validation_error(&flow_def, &report));
  }
  Ok(flow_def)
}

/// Build a runnable `Flow` from config-first workflow YAML.
pub fn build_flow_from_yaml(yaml_content: &str, model_override: Option<&str>) -> Result<Flow> {
  let flow_def = parse_workflow_definition(yaml_content)?;
  build_flow_from_definition(&flow_def, model_override)
}

/// Build a runnable `Flow` from a parsed config-first definition.
pub fn build_flow_from_definition(
  flow_def: &FlowDefinitionV2,
  model_override: Option<&str>,
) -> Result<Flow> {
  let mut flow = Flow::default();
  for node_def in &flow_def.nodes {
    let mut graph_node = factory::create_graph_node(node_def)
      .with_context(|| format!("Failed to create graph node for id: {}", node_def.id))?;
    apply_model_override(node_def, &mut graph_node, model_override);
    flow.add_node(graph_node);
  }
  Ok(flow)
}

/// T3.2: enact `flow_def.inputs`'s declarations against the caller's actual
/// initial inputs, in place.
///
/// For each declared input not already present in `external_inputs`:
/// - if it has a `default`, that value is filled in;
/// - else if `required`, the run fails with a clear error naming the
///   missing input, rather than silently proceeding and letting a
///   downstream node's `input_mapping` resolution fail with a much less
///   direct error later;
/// - else (optional, no default) it's simply left absent — nothing to do.
///
/// A caller-supplied value always wins over a `default`; this never
/// overwrites an input the caller already provided.
pub fn apply_declared_inputs(
  flow_def: &FlowDefinitionV2,
  external_inputs: &mut AsyncNodeInputs,
) -> Result<()> {
  for (name, def) in &flow_def.inputs {
    if external_inputs.contains_key(name) {
      continue;
    }
    if let Some(default) = &def.default {
      let json_value: Value = serde_yaml::from_value(default.clone()).with_context(|| {
        format!(
          "workflow '{}': invalid default for input '{name}'",
          flow_def.name
        )
      })?;
      external_inputs.insert(name.clone(), FlowValue::Json(json_value));
    } else if def.required {
      bail!(
        "workflow '{}' requires input '{name}', which was not supplied and has no default",
        flow_def.name
      );
    }
  }
  Ok(())
}

/// Y4.4: a JSON Schema for a workflow's declared `inputs`, so an agent
/// that calls the workflow as a tool sees typed, described parameters
/// (RFC §3: the planner picks from a closed menu). Types come from each
/// input's `default`; inputs without one are strings. Required inputs
/// with no default are `required`.
pub fn inputs_json_schema(flow_def: &FlowDefinitionV2) -> Value {
  let mut properties = serde_json::Map::new();
  let mut required = Vec::new();
  let mut names: Vec<&String> = flow_def.inputs.keys().collect();
  names.sort();
  for name in names {
    let def = &flow_def.inputs[name];
    let default: Option<Value> = def
      .default
      .as_ref()
      .and_then(|value| serde_yaml::from_value(value.clone()).ok());
    let kind = match &default {
      Some(Value::Bool(_)) => "boolean",
      Some(Value::Number(n)) if n.is_i64() || n.is_u64() => "integer",
      Some(Value::Number(_)) => "number",
      Some(Value::Array(_)) => "array",
      Some(Value::Object(_)) => "object",
      _ => "string",
    };
    let mut property = serde_json::json!({ "type": kind });
    if let Some(description) = &def.description {
      property["description"] = Value::String(description.clone());
    }
    match default {
      Some(default) => property["default"] = default,
      None if def.required => required.push(Value::String(name.clone())),
      None => {}
    }
    properties.insert(name.clone(), property);
  }
  serde_json::json!({
    "type": "object",
    "properties": properties,
    "required": required,
    "additionalProperties": false,
  })
}

/// Apply the CLI/server model override to node kinds that invoke agents or
/// LLMs, including those inside `map` / `while` bodies (Y4.1: an `llm` in
/// a Map template used to keep its own, possibly empty, model).
pub fn apply_model_override(
  node_def: &NodeDefinitionV2,
  graph_node: &mut GraphNode,
  model_override: Option<&str>,
) {
  let Some(model) = model_override else {
    return;
  };
  if matches!(
    node_def.node_type.as_str(),
    "llm" | "skill_agent" | "agent" | "multi_agent" | "figure"
  ) {
    graph_node.initial_inputs.insert(
      "model".to_string(),
      FlowValue::Json(Value::String(model.to_string())),
    );
  }
  let body_key = match node_def.node_type.as_str() {
    "map" => "template",
    "while" => "do",
    _ => return,
  };
  let (yanshi_core::NodeType::Map { template, .. } | yanshi_core::NodeType::While { template, .. }) =
    &mut graph_node.node_type
  else {
    return;
  };
  // The factory built `template` from these definitions, in this order.
  let Some(body) = node_def
    .parameters
    .get(body_key)
    .and_then(|raw| serde_yaml::from_value::<Vec<NodeDefinitionV2>>(raw.clone()).ok())
  else {
    return;
  };
  for (nested_def, nested_node) in body.iter().zip(template.iter_mut()) {
    apply_model_override(nested_def, nested_node, Some(model));
  }
}

fn format_validation_error(
  flow_def: &FlowDefinitionV2,
  report: &WorkflowValidationReport,
) -> String {
  let mut message = format!(
    "workflow '{}' failed schema validation with {} issue(s)",
    flow_def.name,
    report.issues.len()
  );
  for issue in &report.issues {
    message.push_str("\n- ");
    message.push_str(issue);
  }
  message
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(yaml: &str) -> FlowDefinitionV2 {
    serde_yaml::from_str(yaml).expect("test fixture must parse")
  }

  #[test]
  fn missing_input_with_default_gets_filled_in() {
    let flow_def = parse(
      r#"
name: Test
inputs:
  topic:
    required: false
    default: "Yanshi"
nodes:
  - id: a
    type: template
    parameters:
      template: "hi"
"#,
    );
    let mut inputs = AsyncNodeInputs::new();
    apply_declared_inputs(&flow_def, &mut inputs).expect("default fill must succeed");
    assert_eq!(
      inputs.get("topic"),
      Some(&FlowValue::Json(Value::String("Yanshi".to_string())))
    );
  }

  #[test]
  fn caller_supplied_value_is_never_overwritten_by_default() {
    let flow_def = parse(
      r#"
name: Test
inputs:
  topic:
    required: false
    default: "Yanshi"
nodes:
  - id: a
    type: template
    parameters:
      template: "hi"
"#,
    );
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert(
      "topic".to_string(),
      FlowValue::Json(Value::String("caller-value".to_string())),
    );
    apply_declared_inputs(&flow_def, &mut inputs).expect("must succeed");
    assert_eq!(
      inputs.get("topic"),
      Some(&FlowValue::Json(Value::String("caller-value".to_string())))
    );
  }

  #[test]
  fn missing_required_input_with_no_default_fails_clearly() {
    let flow_def = parse(
      r#"
name: Test
inputs:
  topic:
    required: true
nodes:
  - id: a
    type: template
    parameters:
      template: "hi"
"#,
    );
    let mut inputs = AsyncNodeInputs::new();
    let err =
      apply_declared_inputs(&flow_def, &mut inputs).expect_err("missing required input must fail");
    assert!(
      err.to_string().contains("requires input 'topic'"),
      "unexpected message: {err}"
    );
  }

  #[test]
  fn missing_optional_input_with_no_default_is_left_absent() {
    let flow_def = parse(
      r#"
name: Test
inputs:
  topic:
    required: false
nodes:
  - id: a
    type: template
    parameters:
      template: "hi"
"#,
    );
    let mut inputs = AsyncNodeInputs::new();
    apply_declared_inputs(&flow_def, &mut inputs).expect("optional-absent must not fail");
    assert!(!inputs.contains_key("topic"));
  }

  #[test]
  fn required_defaults_to_false_when_omitted() {
    let flow_def = parse(
      r#"
name: Test
inputs:
  topic:
    description: "just documentation"
nodes:
  - id: a
    type: template
    parameters:
      template: "hi"
"#,
    );
    let mut inputs = AsyncNodeInputs::new();
    apply_declared_inputs(&flow_def, &mut inputs)
      .expect("required must default to false, so this must not fail");
  }

  #[test]
  fn model_override_reaches_llm_nodes_inside_map_bodies() {
    let flow_def = parse(
      r#"
name: nested
nodes:
  - id: outer
    type: llm
    parameters: { prompt: "hi" }
  - id: each
    type: map
    parameters:
      template:
        - id: inner
          type: llm
          parameters: { prompt: "{{ item }}" }
"#,
    );
    let flow = build_flow_from_definition(&flow_def, Some("mock-x")).expect("builds");
    let model_of = |node: &GraphNode| node.initial_inputs.get("model").cloned();
    let expected = Some(FlowValue::Json(Value::String("mock-x".to_string())));
    assert_eq!(model_of(&flow.nodes()["outer"]), expected);
    let yanshi_core::NodeType::Map { template, .. } = &flow.nodes()["each"].node_type else {
      panic!("each is a map");
    };
    assert_eq!(
      model_of(&template[0]),
      expected,
      "nested llm gets the override"
    );
  }

  #[test]
  fn inputs_schema_types_inputs_from_their_defaults() {
    let flow_def = parse(
      r#"
name: gen
inputs:
  url: { description: "Page", default: "" }
  sections: { default: 4 }
  pdf: { default: true }
  topic: { required: true }
nodes:
  - id: t
    type: template
    parameters: { template: "x" }
"#,
    );
    let schema = inputs_json_schema(&flow_def);
    assert_eq!(schema["properties"]["url"]["type"], "string");
    assert_eq!(schema["properties"]["url"]["description"], "Page");
    assert_eq!(schema["properties"]["sections"]["type"], "integer");
    assert_eq!(schema["properties"]["pdf"]["type"], "boolean");
    assert_eq!(schema["required"], serde_json::json!(["topic"]));
    assert_eq!(schema["additionalProperties"], false);
  }
}
