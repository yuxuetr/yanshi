//! Server-side ceiling on what a submitted workflow or harness session may
//! touch on the gateway host (eval 2026-10-07 S1/S2/S7).
//!
//! A workflow YAML carries its own sandbox settings (`allowed_commands`,
//! `allowed_paths`), and several node types run host processes or load
//! code from a path the YAML names (`mcp`'s `server_command`, `plugin`'s
//! `manifest`, `skill_agent`/`agent`/`multi_agent`'s skill dir). On the
//! CLI the author is the operator, so that's fine; on the gateway the
//! author is any token holder, so those settings must be capped by the
//! operator. Secure by default: with nothing configured, every
//! host-reaching node type is rejected and no path is allowed.
//!
//! Operator knobs (comma-separated):
//! - [`HOST_NODE_TYPES_ENV`] — host-reaching node types tenants may use.
//! - [`ALLOWED_COMMANDS_ENV`] — the most a `shell` node may allow.
//! - [`PATH_ROOTS_ENV`] — absolute roots every `allowed_paths` entry,
//!   harness `workspace_root` and `checkpoint_dir` must sit under.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use yanshi_config::config::v2::{FlowDefinitionV2, HOST_NODE_TYPES, NodeDefinitionV2, walk_nodes};

pub const HOST_NODE_TYPES_ENV: &str = "YANSHI_GATEWAY_HOST_NODE_TYPES";
pub const ALLOWED_COMMANDS_ENV: &str = "YANSHI_GATEWAY_ALLOWED_COMMANDS";
pub const PATH_ROOTS_ENV: &str = "YANSHI_GATEWAY_PATH_ROOTS";

#[derive(Debug, Clone, Default)]
pub struct GatewayPolicy {
  pub host_node_types: HashSet<String>,
  pub allowed_commands: HashSet<String>,
  pub path_roots: Vec<PathBuf>,
}

impl GatewayPolicy {
  /// Read the operator ceiling from the environment. Fails on a relative
  /// root or an unknown node type so a typo can't silently widen or
  /// narrow the policy.
  pub fn from_env() -> Result<Self, String> {
    let list = |key: &str| -> Vec<String> {
      std::env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
    };
    let host_node_types: HashSet<String> = list(HOST_NODE_TYPES_ENV).into_iter().collect();
    if let Some(unknown) = host_node_types
      .iter()
      .find(|kind| !HOST_NODE_TYPES.contains(&kind.as_str()))
    {
      return Err(format!(
        "{HOST_NODE_TYPES_ENV}: '{unknown}' is not a host-reaching node type (expected any of {HOST_NODE_TYPES:?})"
      ));
    }
    let path_roots: Vec<PathBuf> = list(PATH_ROOTS_ENV)
      .into_iter()
      .map(PathBuf::from)
      .collect();
    if let Some(bad) = path_roots.iter().find(|root| !is_plain_absolute(root)) {
      return Err(format!(
        "{PATH_ROOTS_ENV}: '{}' must be an absolute path without '..'",
        bad.display()
      ));
    }
    Ok(Self {
      host_node_types,
      allowed_commands: list(ALLOWED_COMMANDS_ENV).into_iter().collect(),
      path_roots,
    })
  }

  /// Reject a workflow that reaches past the operator ceiling.
  pub fn check_workflow(&self, def: &FlowDefinitionV2) -> Result<(), String> {
    walk_nodes(&def.nodes, &mut |node| self.check_node(node))
  }

  /// `what` names the field in the error message (e.g. `workspace_root`).
  pub fn check_path(&self, what: &str, path: &str) -> Result<(), String> {
    let path = Path::new(path);
    if !is_plain_absolute(path) {
      return Err(format!(
        "{what} '{}' must be an absolute path without '..'",
        path.display()
      ));
    }
    if self.path_roots.iter().any(|root| path.starts_with(root)) {
      return Ok(());
    }
    if self.path_roots.is_empty() {
      return Err(format!(
        "{what} '{}' is not allowed: this gateway permits no host paths (operator sets {PATH_ROOTS_ENV})",
        path.display()
      ));
    }
    Err(format!(
      "{what} '{}' is outside the gateway's allowed roots {:?}",
      path.display(),
      self.path_roots
    ))
  }

  fn check_node(&self, node: &NodeDefinitionV2) -> Result<(), String> {
    let kind = node.node_type.as_str();
    let id = &node.id;
    if HOST_NODE_TYPES.contains(&kind) && !self.host_node_types.contains(kind) {
      return Err(format!(
        "node '{id}': type '{kind}' is disabled on this gateway (operator enables it via {HOST_NODE_TYPES_ENV})"
      ));
    }
    match kind {
      "shell" => {
        for command in string_list(node, "allowed_commands") {
          if !self.allowed_commands.contains(&command) {
            return Err(format!(
              "node '{id}': command '{command}' exceeds the gateway ceiling (operator sets {ALLOWED_COMMANDS_ENV})"
            ));
          }
        }
        // Without allowed_paths the shell sandbox falls back to the
        // gateway's own cwd, which is not the tenant's to choose.
        let paths = string_list(node, "allowed_paths");
        if paths.is_empty() {
          return Err(format!(
            "node '{id}': shell nodes on the gateway must declare 'allowed_paths'"
          ));
        }
        for path in paths {
          self.check_path(&format!("node '{id}' allowed_paths"), &path)?;
        }
      }
      "file" => {
        for path in string_list(node, "allowed_paths") {
          self.check_path(&format!("node '{id}' allowed_paths"), &path)?;
        }
      }
      _ => {}
    }
    Ok(())
  }
}

fn string_list(node: &NodeDefinitionV2, key: &str) -> Vec<String> {
  node
    .parameters
    .get(key)
    .and_then(|value| value.as_sequence())
    .map(|seq| {
      seq
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
    })
    .unwrap_or_default()
}

/// Containment is checked lexically, so `..` must be refused outright —
/// `/srv/work/../../etc` "starts with" `/srv/work`.
fn is_plain_absolute(path: &Path) -> bool {
  path.is_absolute() && !path.components().any(|c| c == Component::ParentDir)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn workflow(yaml: &str) -> FlowDefinitionV2 {
    serde_yaml::from_str(yaml).unwrap()
  }

  fn operator_policy() -> GatewayPolicy {
    GatewayPolicy {
      host_node_types: ["shell", "file"].iter().map(|s| s.to_string()).collect(),
      allowed_commands: ["git", "ls"].iter().map(|s| s.to_string()).collect(),
      path_roots: vec![PathBuf::from("/srv/work")],
    }
  }

  #[test]
  fn default_policy_rejects_every_host_node_type() {
    for kind in HOST_NODE_TYPES {
      let def = workflow(&format!("name: w\nnodes:\n  - id: n\n    type: {kind}\n"));
      let err = GatewayPolicy::default().check_workflow(&def).unwrap_err();
      assert!(err.contains(HOST_NODE_TYPES_ENV), "{kind}: {err}");
    }
    let llm = workflow("name: w\nnodes:\n  - id: n\n    type: llm\n");
    assert!(GatewayPolicy::default().check_workflow(&llm).is_ok());
  }

  #[test]
  fn shell_commands_and_paths_are_capped() {
    let policy = operator_policy();
    let ok = workflow(
      "name: w\nnodes:\n  - id: n\n    type: shell\n    parameters:\n      allowed_commands: [git]\n      allowed_paths: [/srv/work/repo]\n",
    );
    assert!(policy.check_workflow(&ok).is_ok());
    for bad in [
      "allowed_commands: [python3]\n      allowed_paths: [/srv/work]",
      "allowed_commands: [git]\n      allowed_paths: [/]",
      "allowed_commands: [git]\n      allowed_paths: [/srv/work/../../proc/self]",
      "allowed_commands: [git]\n      allowed_paths: [srv/work]",
      "allowed_commands: [git]",
    ] {
      let def = workflow(&format!(
        "name: w\nnodes:\n  - id: n\n    type: shell\n    parameters:\n      {bad}\n"
      ));
      assert!(policy.check_workflow(&def).is_err(), "{bad}");
    }
  }

  #[test]
  fn nested_map_and_while_nodes_are_checked() {
    let def = workflow(
      "name: w\nnodes:\n  - id: m\n    type: map\n    parameters:\n      template:\n        - id: inner\n          type: while\n          parameters:\n            do:\n              - id: leaf\n                type: mcp\n",
    );
    let err = operator_policy().check_workflow(&def).unwrap_err();
    assert!(err.contains("'leaf'"), "{err}");
  }

  #[test]
  fn path_roots_bound_workspace_root() {
    let policy = operator_policy();
    assert!(policy.check_path("workspace_root", "/srv/work/t1").is_ok());
    assert!(policy.check_path("workspace_root", "/").is_err());
    assert!(
      policy
        .check_path("workspace_root", "/srv/workshop")
        .is_err()
    );
    assert!(
      GatewayPolicy::default()
        .check_path("workspace_root", "/srv/work")
        .is_err()
    );
  }
}
