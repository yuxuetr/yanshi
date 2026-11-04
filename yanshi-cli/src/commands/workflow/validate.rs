use crate::config::{
  schema::{
    UnknownParameterMode, WorkflowValidationOptions, validate_flow_definition_with_options,
  },
  v2::{FlowDefinitionV2, NodeDefinitionV2},
};
use anyhow::{Context, Result, bail};
use serde_yaml::Value as YamlValue;
use std::collections::BTreeMap;
use std::fs;

pub async fn execute(
  workflow_file: String,
  format: String,
  strict: bool,
  explain_permissions: bool,
) -> Result<()> {
  let yaml_content = fs::read_to_string(&workflow_file)
    .with_context(|| format!("Failed to read workflow file: {}", workflow_file))?;
  let flow_def: FlowDefinitionV2 =
    serde_yaml::from_str(&yaml_content).with_context(|| "Failed to parse workflow YAML")?;

  let report = validate_flow_definition_with_options(
    &flow_def,
    WorkflowValidationOptions {
      unknown_parameters: if strict {
        UnknownParameterMode::Error
      } else {
        UnknownParameterMode::Warning
      },
    },
  );

  let permissions = explain_permissions.then(|| build_permission_report(&flow_def));

  // Shared payload — produced once and reused by both JSON paths so
  // the envelope's `result` field is byte-identical to the legacy
  // `--format json` body (P0.3 additive-field contract).
  let mut payload = serde_json::json!({
    "workflow": &flow_def.name,
    "valid": report.is_valid(),
    "issues": &report.issues,
    "warnings": &report.warnings,
  });
  if let Some(perm) = &permissions {
    payload["permissions"] = serde_json::to_value(perm)?;
  }

  match format.as_str() {
    "json" => {
      println!("{}", serde_json::to_string_pretty(&payload)?);
    }
    "json-envelope" => {
      // P3.3 envelope migration: wrap the same JSON body in the
      // canonical `CliJsonEnvelope` (`yanshi.cli/1`) so downstream
      // tooling can parse `version` + `command` + `errors` uniformly
      // across every command. The legacy `--format json` stays for
      // back-compat with consumers pinned to the bare body.
      let mut errors: Vec<String> = Vec::new();
      if !report.is_valid() {
        errors.push(format!(
          "workflow '{}' failed schema validation with {} issue(s)",
          flow_def.name,
          report.issues.len()
        ));
      }
      let envelope =
        crate::json_envelope::CliJsonEnvelope::with_errors("workflow validate", &payload, errors);
      println!("{}", serde_json::to_string_pretty(&envelope)?);
    }
    _ => {
      print_schema_report(&flow_def.name, &report);
      if let Some(perm) = &permissions {
        print_permission_report(perm);
      }
    }
  }

  if !report.is_valid() {
    bail!(
      "workflow '{}' failed schema validation with {} issue(s)",
      flow_def.name,
      report.issues.len()
    );
  }

  Ok(())
}

pub fn print_schema_report(
  workflow_name: &str,
  report: &crate::config::schema::WorkflowValidationReport,
) {
  println!("Workflow: {}", workflow_name);
  if report.issues.is_empty() && report.warnings.is_empty() {
    println!("✅ Schema validation passed");
    return;
  }

  if !report.issues.is_empty() {
    println!("❌ Schema issues: {}", report.issues.len());
    for (idx, issue) in report.issues.iter().enumerate() {
      println!("  {}. {}", idx + 1, issue);
    }
  }
  if !report.warnings.is_empty() {
    println!("⚠️  Schema warnings: {}", report.warnings.len());
    for (idx, warning) in report.warnings.iter().enumerate() {
      println!("  {}. {}", idx + 1, warning);
    }
  }
}

#[derive(Debug, serde::Serialize)]
pub struct PermissionReport {
  pub nodes: Vec<NodePermission>,
  pub aggregate: AggregatePermissions,
}

#[derive(Debug, serde::Serialize)]
pub struct NodePermission {
  pub id: String,
  pub node_type: String,
  pub category: PermissionCategory,
  pub required_capabilities: Vec<String>,
  /// Constraints declared in the node's parameters (e.g. allowed_commands,
  /// allowed_paths, allowed_domains, server_command, plugin_id). Keys are the
  /// parameter name; values are the human-readable summary of the value.
  pub constraints: BTreeMap<String, String>,
  /// Notes attached for operator review (e.g. "permissive: no allowlist").
  pub notes: Vec<String>,
}

#[derive(Debug, serde::Serialize, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PermissionCategory {
  /// Pure compute / template; no host-side capabilities required.
  Pure,
  /// Reads or writes files via FileTool / file node.
  Filesystem,
  /// Issues outbound HTTP requests.
  Network,
  /// Executes shell commands or scripts.
  Exec,
  /// Calls an MCP server (which may itself fan out to fs/net/exec).
  Mcp,
  /// Executes a subprocess plugin.
  Plugin,
  /// Embeds an LLM call. No host capability beyond the network hop.
  Llm,
  /// Embeds an agent runtime — capability surface depends on its tool registry.
  Agent,
  /// Unknown / not classified.
  Unknown,
}

impl PermissionCategory {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Pure => "pure",
      Self::Filesystem => "filesystem",
      Self::Network => "network",
      Self::Exec => "exec",
      Self::Mcp => "mcp",
      Self::Plugin => "plugin",
      Self::Llm => "llm",
      Self::Agent => "agent",
      Self::Unknown => "unknown",
    }
  }
}

#[derive(Debug, serde::Serialize, Default)]
pub struct AggregatePermissions {
  pub categories: Vec<String>,
  pub total_nodes: usize,
  /// Number of nodes carrying a non-pure permission category.
  pub permission_bearing_nodes: usize,
}

pub fn build_permission_report(flow: &FlowDefinitionV2) -> PermissionReport {
  let mut nodes = Vec::with_capacity(flow.nodes.len());
  let mut seen_categories: std::collections::BTreeSet<PermissionCategory> =
    std::collections::BTreeSet::new();
  let mut permission_bearing = 0usize;

  for node in &flow.nodes {
    let np = classify_node(node);
    if np.category != PermissionCategory::Pure {
      permission_bearing += 1;
    }
    seen_categories.insert(np.category);
    nodes.push(np);
  }

  let aggregate = AggregatePermissions {
    categories: seen_categories
      .iter()
      .map(|c| c.as_str().to_string())
      .collect(),
    total_nodes: flow.nodes.len(),
    permission_bearing_nodes: permission_bearing,
  };

  PermissionReport { nodes, aggregate }
}

fn classify_node(node: &NodeDefinitionV2) -> NodePermission {
  let id = node.id.clone();
  let node_type = node.node_type.clone();
  let mut constraints: BTreeMap<String, String> = BTreeMap::new();
  let mut notes: Vec<String> = Vec::new();
  let (category, capabilities) = match node.node_type.as_str() {
    "template" | "markmap" | "batch" | "conditional" | "while" | "render_table" | "render_svg"
    | "render_mindmap" | "render_markdown" => (PermissionCategory::Pure, vec![]),
    // Y3.4: calls a model, then runs lualatex / pdftocairo / typst under
    // the OS sandbox, writing only to its artifact directory.
    "figure" => (
      PermissionCategory::Exec,
      vec![
        "net".to_string(),
        "exec".to_string(),
        "fs.write".to_string(),
      ],
    ),
    // Y4.3: runs ffmpeg under the OS sandbox over this run's artifacts.
    "render_audio" => (
      PermissionCategory::Exec,
      vec!["exec".to_string(), "fs.write".to_string()],
    ),
    // Y3.3: runs pandoc (and typst) under the OS sandbox, writing only
    // to its artifact directory.
    "render_document" => (
      PermissionCategory::Exec,
      vec!["exec".to_string(), "fs.write".to_string()],
    ),
    "llm" => {
      if let Some(model) = node.parameters.get("model").and_then(yaml_summary) {
        constraints.insert("model".to_string(), model);
      }
      (PermissionCategory::Llm, vec!["net".to_string()])
    }
    "http" => {
      summarize_param(&node.parameters, "url", &mut constraints);
      summarize_param(&node.parameters, "method", &mut constraints);
      summarize_list_param(&node.parameters, "allowed_domains", &mut constraints);
      summarize_list_param(&node.parameters, "allowed_methods", &mut constraints);
      if !node.parameters.contains_key("allowed_domains") {
        notes.push("permissive: no allowed_domains constraint".to_string());
      }
      (PermissionCategory::Network, vec!["net".to_string()])
    }
    "file" => {
      summarize_param(&node.parameters, "operation", &mut constraints);
      summarize_param(&node.parameters, "path", &mut constraints);
      summarize_list_param(&node.parameters, "allowed_paths", &mut constraints);
      // V0.2 closure: the schema now requires `allowed_paths` (rejected
      // at parse time if missing/empty, mirroring `shell`'s
      // `allowed_commands` below), so this note is no longer reachable
      // from a validated workflow — kept as defense-in-depth in case
      // someone bypasses validate via a custom config path.
      if !node.parameters.contains_key("allowed_paths") {
        notes.push("permissive: no allowed_paths constraint".to_string());
      }
      // Coarse-grained — node-side caller may only read OR write, but we don't
      // know which without going deeper into the v2 schema; surface both.
      (
        PermissionCategory::Filesystem,
        vec!["fs.read".to_string(), "fs.write".to_string()],
      )
    }
    "shell" => {
      summarize_param(&node.parameters, "command", &mut constraints);
      summarize_list_param(&node.parameters, "allowed_commands", &mut constraints);
      // F-A7-2 closed (2026-05-18): `type: shell` is now wired into
      // the YAML factory via `ShellWorkflowNode`. The schema
      // requires `allowed_commands` (rejected at parse time if
      // missing), so the prior "permissive: no allowed_commands"
      // note is no longer reachable from a validated workflow —
      // kept here as defense-in-depth in case someone bypasses
      // validate via a custom config path.
      if !node.parameters.contains_key("allowed_commands") {
        notes.push("permissive: no allowed_commands constraint".to_string());
      }
      (PermissionCategory::Exec, vec!["exec".to_string()])
    }
    "mcp" => {
      summarize_list_param(&node.parameters, "server_command", &mut constraints);
      summarize_param(&node.parameters, "tool_name", &mut constraints);
      summarize_param(&node.parameters, "server_url", &mut constraints);
      // MCP can do anything the server can — be conservative.
      (
        PermissionCategory::Mcp,
        vec!["mcp.call".to_string(), "net".to_string()],
      )
    }
    "plugin" => {
      summarize_param(&node.parameters, "plugin_id", &mut constraints);
      summarize_param(&node.parameters, "entry_point", &mut constraints);
      (PermissionCategory::Plugin, vec!["plugin.exec".to_string()])
    }
    "agent" | "skill_agent" | "multi_agent" => {
      summarize_param(&node.parameters, "skill", &mut constraints);
      summarize_param(&node.parameters, "model", &mut constraints);
      summarize_list_param(&node.parameters, "allowed_tools", &mut constraints);
      notes.push(
        "agent: effective capability surface depends on the embedded tool registry".to_string(),
      );
      (PermissionCategory::Agent, vec!["agent.runtime".to_string()])
    }
    "rag" | "arxiv" | "asr" | "tts" | "text_to_image" | "image_to_image" | "image_edit"
    | "image_understand" => (PermissionCategory::Network, vec!["net".to_string()]),
    _ => (PermissionCategory::Unknown, vec![]),
  };

  NodePermission {
    id,
    node_type,
    category,
    required_capabilities: capabilities,
    constraints,
    notes,
  }
}

fn summarize_param(
  parameters: &std::collections::HashMap<String, YamlValue>,
  key: &str,
  constraints: &mut BTreeMap<String, String>,
) {
  if let Some(v) = parameters.get(key).and_then(yaml_summary) {
    constraints.insert(key.to_string(), v);
  }
}

fn summarize_list_param(
  parameters: &std::collections::HashMap<String, YamlValue>,
  key: &str,
  constraints: &mut BTreeMap<String, String>,
) {
  if let Some(list) = parameters.get(key).and_then(yaml_list_summary) {
    constraints.insert(key.to_string(), list);
  }
}

fn yaml_summary(value: &YamlValue) -> Option<String> {
  match value {
    YamlValue::String(s) => Some(s.clone()),
    YamlValue::Number(n) => Some(n.to_string()),
    YamlValue::Bool(b) => Some(b.to_string()),
    YamlValue::Sequence(_) => yaml_list_summary(value),
    YamlValue::Null => None,
    YamlValue::Mapping(_) => Some("<object>".to_string()),
    YamlValue::Tagged(t) => yaml_summary(&t.value),
  }
}

fn yaml_list_summary(value: &YamlValue) -> Option<String> {
  let seq = value.as_sequence()?;
  let items: Vec<String> = seq
    .iter()
    .map(|v| match v {
      YamlValue::String(s) => s.clone(),
      YamlValue::Number(n) => n.to_string(),
      YamlValue::Bool(b) => b.to_string(),
      _ => "<value>".to_string(),
    })
    .collect();
  if items.is_empty() {
    return Some("[]".to_string());
  }
  Some(format!("[{}]", items.join(", ")))
}

pub fn print_permission_report(report: &PermissionReport) {
  println!("\nPermission requirements:");
  println!(
    "  {} of {} nodes carry a host-side permission category",
    report.aggregate.permission_bearing_nodes, report.aggregate.total_nodes
  );
  if !report.aggregate.categories.is_empty() {
    println!("  categories: {}", report.aggregate.categories.join(", "));
  }
  if report.nodes.is_empty() {
    println!("  (no nodes)");
    return;
  }
  for node in &report.nodes {
    println!(
      "  - {} (type={}) → {}",
      node.id,
      node.node_type,
      node.category.as_str()
    );
    if !node.required_capabilities.is_empty() {
      println!(
        "      capabilities: [{}]",
        node.required_capabilities.join(", ")
      );
    }
    for (k, v) in &node.constraints {
      println!("      {}: {}", k, v);
    }
    for note in &node.notes {
      println!("      note: {}", note);
    }
  }
}
