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
//!   harness `workspace_root`, `checkpoint_dir` and any node's
//!   path-naming parameter (Y7.3) must sit under.
//! - [`URL_HOSTS_ENV`] — hosts a node's endpoint parameter (`qdrant_url`,
//!   any `*_url`) may name (Y7.3). The `http` node's `url` is not capped
//!   here: its own SSRF filter runs on every request.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use yanshi_config::config::v2::{FlowDefinitionV2, HOST_NODE_TYPES, NodeDefinitionV2, walk_nodes};

pub const HOST_NODE_TYPES_ENV: &str = "YANSHI_GATEWAY_HOST_NODE_TYPES";
pub const ALLOWED_COMMANDS_ENV: &str = "YANSHI_GATEWAY_ALLOWED_COMMANDS";
pub const PATH_ROOTS_ENV: &str = "YANSHI_GATEWAY_PATH_ROOTS";
pub const URL_HOSTS_ENV: &str = "YANSHI_GATEWAY_URL_HOSTS";

#[derive(Debug, Clone, Default)]
pub struct GatewayPolicy {
  pub host_node_types: HashSet<String>,
  pub allowed_commands: HashSet<String>,
  pub path_roots: Vec<PathBuf>,
  /// Y7.3: hosts endpoint parameters may name (exact match).
  pub url_hosts: HashSet<String>,
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
      url_hosts: list(URL_HOSTS_ENV)
        .into_iter()
        .map(|host| host.to_ascii_lowercase())
        .collect(),
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
    self.check_reach_parameters(node)?;
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

/// What a parameter reaches, judged by its name (Y7.3).
#[derive(Debug, PartialEq)]
enum Reach {
  Path,
  Url,
}

fn reach_of(node_type: &str, key: &str) -> Option<Reach> {
  match key {
    // Lists checked per node type above.
    "allowed_paths" | "allowed_commands" => None,
    // `http` filters its target at run time; `arxiv` builds arxiv.org
    // URLs from the paper id it parses out of `url`.
    "url" if matches!(node_type, "http" | "arxiv") => None,
    "url" | "endpoint" | "base_url" => Some(Reach::Url),
    "path" | "save_to_file" => Some(Reach::Path),
    _ if key.ends_with("_url") || key.ends_with("_endpoint") => Some(Reach::Url),
    _ if key.ends_with("_path") || key.ends_with("_dir") || key.ends_with("_file") => {
      Some(Reach::Path)
    }
    _ => None,
  }
}

impl GatewayPolicy {
  /// Y7.3 (SEC-2/3): every node parameter that names a host path or an
  /// endpoint is held to the same ceiling as `allowed_paths` — whatever
  /// the node type, since new nodes keep adding such parameters
  /// (`markmap.save_to_file`, `rag.qdrant_url`).
  fn check_reach_parameters(&self, node: &NodeDefinitionV2) -> Result<(), String> {
    let id = &node.id;
    for (key, value) in &node.parameters {
      let Some(reach) = reach_of(&node.node_type, key) else {
        continue;
      };
      let Some(text) = value.as_str() else {
        return Err(format!(
          "node '{id}' {key}: must be a plain string on the gateway"
        ));
      };
      match reach {
        Reach::Path => self.check_path(&format!("node '{id}' {key}"), text)?,
        Reach::Url => self.check_url(&format!("node '{id}' {key}"), text)?,
      }
    }
    Ok(())
  }

  /// `what` names the field in the error message.
  pub fn check_url(&self, what: &str, text: &str) -> Result<(), String> {
    let parsed = url::Url::parse(text).map_err(|e| format!("{what} '{text}' is not a URL: {e}"))?;
    let host = parsed
      .host_str()
      .map(str::to_ascii_lowercase)
      .ok_or_else(|| format!("{what} '{text}' names no host"))?;
    if self.url_hosts.contains(&host) {
      return Ok(());
    }
    Err(format!(
      "{what} '{text}': host '{host}' is not allowed on this gateway (operator lists endpoints in {URL_HOSTS_ENV})"
    ))
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
      url_hosts: HashSet::new(),
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

  /// Y7.3 (SEC-2/3): any node parameter that names a host path or a
  /// network endpoint is capped, not only on the host-reaching types.
  #[test]
  fn path_and_url_parameters_are_capped_on_every_node() {
    let mut policy = operator_policy();
    let markmap = |path: &str| {
      workflow(&format!(
        "name: w\nnodes:\n  - id: m\n    type: markmap\n    parameters:\n      save_to_file: {path}\n"
      ))
    };
    assert!(
      policy
        .check_workflow(&markmap("/srv/work/map.html"))
        .is_ok()
    );
    for bad in ["/etc/cron.d/x", "relative.html", "/srv/work/../../etc/x"] {
      let err = policy.check_workflow(&markmap(bad)).unwrap_err();
      assert!(err.contains("save_to_file"), "{bad}: {err}");
    }
    let rag = |url: &str| {
      workflow(&format!(
        "name: w\nnodes:\n  - id: r\n    type: rag\n    parameters:\n      operation: search\n      qdrant_url: {url}\n"
      ))
    };
    let err = policy
      .check_workflow(&rag("http://169.254.169.254:6334"))
      .unwrap_err();
    assert!(
      err.contains("qdrant_url") && err.contains(URL_HOSTS_ENV),
      "{err}"
    );
    policy.url_hosts = ["qdrant.internal".to_string()].into_iter().collect();
    assert!(
      policy
        .check_workflow(&rag("http://qdrant.internal:6334"))
        .is_ok()
    );
    assert!(
      policy
        .check_workflow(&rag("http://qdrant.internal.evil.com:6334"))
        .is_err()
    );
    assert!(policy.check_workflow(&rag("not a url")).is_err());
    // `http` applies its own SSRF filter at run time; its `url` stays free.
    let http = workflow(
      "name: w\nnodes:\n  - id: h\n    type: http\n    parameters:\n      url: https://example.org\n",
    );
    assert!(GatewayPolicy::default().check_workflow(&http).is_ok());
  }
}
