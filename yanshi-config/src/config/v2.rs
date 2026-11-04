use serde::Deserialize;
use std::collections::HashMap;

/// Defines the structure of a V2 workflow YAML file.
#[derive(Debug, Deserialize)]
pub struct FlowDefinitionV2 {
  pub name: String,
  /// T3.2: enforced by `executor::apply_declared_inputs` — a `required`
  /// input missing from the caller-supplied initial inputs (and with no
  /// `default`) fails the run before any node executes; a missing input
  /// with a `default` gets that value filled in.
  #[serde(default)]
  pub inputs: HashMap<String, InputDefinitionV2>,
  pub nodes: Vec<NodeDefinitionV2>,
  /// Y2.3: USD spend cap for a run of this workflow. Billed nodes price
  /// their calls against the operator's pricing table; once the run has
  /// spent more than this, nodes that have not started are refused.
  #[serde(default)]
  pub cost_limit_usd: Option<f64>,
}

/// Y2.3: a cost limit must be a positive, finite USD amount.
pub fn validate_cost_limit(limit_usd: f64) -> Result<f64, String> {
  if limit_usd.is_finite() && limit_usd > 0.0 {
    Ok(limit_usd)
  } else {
    Err(format!(
      "cost_limit_usd must be a positive number, got {limit_usd}"
    ))
  }
}

impl FlowDefinitionV2 {
  /// Y2.3: the run's cost cap: the tighter of this workflow's
  /// `cost_limit_usd` and `requested` (e.g. the gateway request's), each
  /// validated by [`validate_cost_limit`].
  pub fn effective_cost_limit(&self, requested: Option<f64>) -> Result<Option<f64>, String> {
    let own = self.cost_limit_usd.map(validate_cost_limit).transpose()?;
    let requested = requested.map(validate_cost_limit).transpose()?;
    Ok(match (own, requested) {
      (Some(a), Some(b)) => Some(a.min(b)),
      (a, b) => a.or(b),
    })
  }
}

/// Declares one named workflow input, consumed by
/// `executor::apply_declared_inputs` (T3.2).
#[derive(Debug, Deserialize)]
pub struct InputDefinitionV2 {
  /// Documentation only — not enforced.
  #[allow(dead_code)]
  pub description: Option<String>,
  /// Defaults to `false` when omitted, so a purely-documented optional
  /// input doesn't need to spell it out.
  #[serde(default)]
  pub required: bool,
  /// Filled into the initial input pool when the caller doesn't supply
  /// this input. `required: true` with no `default` is a legal
  /// declaration (fails the run instead of filling a gap) — see
  /// `apply_declared_inputs`.
  #[serde(default)]
  pub default: Option<serde_yaml::Value>,
}

/// Defines a single node in the V2 workflow graph.
#[derive(Debug, Deserialize)]
pub struct NodeDefinitionV2 {
  pub id: String,
  #[serde(rename = "type")]
  pub node_type: String,
  #[serde(default)]
  pub dependencies: Vec<String>,
  #[serde(default)]
  pub input_mapping: HashMap<String, String>,
  #[serde(default)]
  pub run_if: Option<String>,
  /// T3.1: generic, node-type-agnostic outer execution timeout, in
  /// milliseconds. Sibling of `run_if`, not nested inside `parameters` —
  /// applies uniformly to any node type (see
  /// `executor::timeout_retry::TimeoutRetryNode`), unlike the
  /// node-specific `mcp` node's `parameters.timeout_ms` (which governs its
  /// MCP-client connection, a different and unrelated setting).
  #[serde(default)]
  pub timeout_ms: Option<u64>,
  /// T3.1: generic, node-type-agnostic retry count on transient
  /// (network/timeout/rate-limit-class) failures. See `timeout_ms`'s doc
  /// comment for the same `parameters`-vs-top-level distinction.
  #[serde(default)]
  pub max_retries: Option<u32>,
  #[serde(default)]
  pub parameters: HashMap<String, serde_yaml::Value>,
}

/// Node types that execute host processes or read/write host paths named
/// by the workflow author.
pub const HOST_NODE_TYPES: &[&str] = &[
  "shell",
  "file",
  "mcp",
  "plugin",
  "agent",
  "skill_agent",
  "multi_agent",
];

/// Visits every node of a workflow, descending into `map` (`template`) and
/// `while` (`do`) bodies, which run nodes of their own.
pub fn walk_nodes<E>(
  nodes: &[NodeDefinitionV2],
  visit: &mut impl FnMut(&NodeDefinitionV2) -> Result<(), E>,
) -> Result<(), E>
where
  E: From<String>,
{
  for node in nodes {
    visit(node)?;
    let block = match node.node_type.as_str() {
      "map" => "template",
      "while" => "do",
      _ => continue,
    };
    if let Some(yaml) = node.parameters.get(block) {
      let children: Vec<NodeDefinitionV2> =
        serde_yaml::from_value(yaml.clone()).map_err(|err| {
          E::from(format!(
            "node '{}': invalid '{block}' block: {err}",
            node.id
          ))
        })?;
      walk_nodes(&children, visit)?;
    }
  }
  Ok(())
}
