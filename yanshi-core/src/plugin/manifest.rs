//! `plugin.toml` manifest schema and loader.
//!
//! The manifest is a TOML file that lives next to (or refers to) the plugin
//! executable. It declares the plugin's name, version, the node types the
//! plugin contributes, and the OS capabilities it requires. See
//! `docs/PLUGIN_DESIGN.md` §6.2 for the full schema.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Wire protocol version this host implements. Plugins must declare the same
/// value (or an empty string, in which case it is filled in for them) in
/// their `plugin.toml`.
pub const SUPPORTED_PROTOCOL_VERSION: &str = "yanshi.plugin/1";

/// The runtime mechanism used to load and execute the plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginRuntime {
  /// Spawn the plugin as a child process and talk JSON-RPC over stdio.
  /// This is the only runtime implemented in v0.3.0.
  #[default]
  Subprocess,
  /// Reserved for v1.1+: in-process WebAssembly runtime.
  Wasm,
}

/// Top-level manifest envelope. The TOML file looks like `[plugin]\n...`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginManifest {
  pub plugin: PluginSection,
}

/// The `[plugin]` section of `plugin.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginSection {
  pub name: String,
  pub version: String,
  #[serde(default)]
  pub runtime: PluginRuntime,
  /// Path to the plugin executable. Relative paths are resolved against the
  /// directory containing the manifest.
  pub entrypoint: PathBuf,
  /// Wire protocol version the plugin claims to speak. Empty string is
  /// treated as "matches host".
  #[serde(default)]
  pub protocol: String,
  #[serde(default)]
  pub nodes: Vec<NodeSpec>,
  #[serde(default)]
  pub capabilities: Capabilities,
  /// Optional `[plugin.dry_run]` sub-table. When set, the host can ask
  /// `yanshi doctor` (or any other operator-facing diagnostic) to
  /// spawn the entrypoint with the configured args and verify the
  /// binary at least starts cleanly — without depending on the full
  /// JSON-RPC handshake. Plugins are expected to honor a fast,
  /// side-effect-free invocation (e.g. `--smoke` / `--version`) that
  /// exits 0 well within the timeout.
  ///
  /// Absent / `None` ⇒ doctor skips the smoke for this plugin.
  /// Present ⇒ doctor runs the smoke and surfaces pass / fail.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub dry_run: Option<DryRunSpec>,
  /// Optional `[plugin.signature]` sub-table (V3.5). When present,
  /// `yanshi plugin install` verifies an Ed25519 detached signature
  /// over the resolved entrypoint file's bytes before letting the
  /// install proceed — see `yanshi-cli/src/commands/plugin/install.rs`.
  /// Absent ⇒ the plugin is treated as unsigned; whether that's
  /// acceptable is a `PluginPolicy::require_signature` (profile-driven)
  /// decision, not a manifest-schema one.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub signature: Option<PluginSignature>,
}

/// `[plugin.signature]` — same shape as `yanshi-skills`'s
/// `MarketplaceSignature`, since both describe a detached signature
/// over some artifact bytes verified against a named public key.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginSignature {
  /// Signature scheme. Only `"ed25519"` is currently supported.
  pub algorithm: String,
  /// Identifies which public key file (`<key_id>.pub`) in the
  /// configured keys directory to verify against.
  pub key_id: String,
  /// Base64-encoded detached signature over the resolved entrypoint
  /// file's raw bytes.
  pub value: String,
}

impl PluginSignature {
  /// Structural validation only — non-empty fields. Cryptographic
  /// verification needs a keys directory and the entrypoint bytes,
  /// neither of which is available at generic `PluginManifest::validate()`
  /// time (called from several contexts, e.g. `yanshi plugin list`,
  /// that don't have a keys directory configured); that happens
  /// separately in `yanshi-cli/src/commands/plugin/install.rs`.
  pub fn validate(&self) -> Result<(), ManifestError> {
    if self.algorithm.trim().is_empty() {
      return Err(ManifestError::InvalidSignature(
        "[plugin.signature].algorithm must not be empty".to_string(),
      ));
    }
    if self.key_id.trim().is_empty() {
      return Err(ManifestError::InvalidSignature(
        "[plugin.signature].key_id must not be empty".to_string(),
      ));
    }
    if self.value.trim().is_empty() {
      return Err(ManifestError::InvalidSignature(
        "[plugin.signature].value must not be empty".to_string(),
      ));
    }
    Ok(())
  }
}

/// `[plugin.dry_run]` configuration.
///
/// Describes a fast, side-effect-free invocation the host can use to
/// verify the plugin entrypoint binary works. See
/// [`PluginSection::dry_run`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DryRunSpec {
  /// Arguments to pass to the entrypoint (e.g. `["--smoke"]`,
  /// `["--version"]`). Must be non-empty — passing no args usually
  /// boots the plugin into JSON-RPC mode, which is exactly what the
  /// dry-run smoke is trying to avoid.
  pub args: Vec<String>,
  /// Wall-clock timeout in milliseconds. The smoke is "fast by
  /// design" — anything past 5s is almost certainly a hang.
  /// Defaults to 1000 (1s) which is what the original P3.4 spec asks
  /// for, but plugins with first-run JIT or signature checks can bump
  /// it as long as they document the reason.
  #[serde(default = "default_dry_run_timeout_ms")]
  pub timeout_ms: u32,
  /// Exit code that signals success. Defaults to 0. Non-zero allows
  /// plugins that conventionally exit `1` (or `64` for "usage")
  /// without polluting the standard.
  #[serde(default)]
  pub expected_exit: i32,
}

fn default_dry_run_timeout_ms() -> u32 {
  1000
}

impl DryRunSpec {
  /// Validate the spec at load time. Empty `args` is the only hard
  /// error today — every other field has a meaningful default.
  pub fn validate(&self) -> Result<(), ManifestError> {
    if self.args.is_empty() {
      return Err(ManifestError::InvalidDryRun(
        "[plugin.dry_run].args must contain at least one argument".to_string(),
      ));
    }
    if self.timeout_ms == 0 {
      return Err(ManifestError::InvalidDryRun(
        "[plugin.dry_run].timeout_ms must be > 0".to_string(),
      ));
    }
    Ok(())
  }
}

/// One declared node type. The host pre-registers a `PluginNode` for each
/// entry so workflows can reference it by `type`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeSpec {
  #[serde(rename = "type")]
  pub node_type: String,
  #[serde(default)]
  pub description: String,
}

/// OS capability declarations.
///
/// The translation rules consumed by sandbox backends live on this type
/// itself ([`Capabilities::filesystem_entries`] / `requires_*`). A higher
/// layer (the CLI plugin executor) bridges these into
/// [`crate::plugin::CommandPreparer`] implementations that wrap the
/// spawned plugin process in the platform sandbox.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Capabilities {
  /// Filesystem grants. Each entry is either a bare path (defaults to
  /// read access) or a `read:<path>` / `write:<path>` prefixed string.
  #[serde(default)]
  pub filesystem: Vec<String>,
  /// Network grants. The current backend treats any non-empty list as
  /// "allow outbound network"; future versions may parse domain rules.
  #[serde(default)]
  pub network: Vec<String>,
  /// Process / exec grants. Non-empty grants the `Exec`-equivalent cap.
  #[serde(default)]
  pub processes: Vec<String>,
  /// Allowed environment variable names. Non-empty grants the
  /// `Env`-equivalent cap (the OS-sandbox layer cannot currently
  /// scrub env vars at exec time on macOS / Linux, so this is recorded
  /// for audit only).
  #[serde(default)]
  pub env_vars: Vec<String>,
}

/// Filesystem access mode declared by a `[plugin.capabilities].filesystem`
/// entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsAccess {
  Read,
  Write,
}

/// One parsed filesystem grant from `[plugin.capabilities].filesystem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemEntry {
  pub access: FsAccess,
  pub path: PathBuf,
}

impl FilesystemEntry {
  /// Parse a single `filesystem = [...]` entry. Accepts:
  ///
  /// * `"./models"` — bare path, defaults to read access
  /// * `"read:./models"` / `"write:/tmp/output"` — explicit access prefix
  ///
  /// Whitespace around the prefix is tolerated. An empty path or an
  /// unknown prefix produces a [`ManifestError::InvalidCapability`].
  pub fn parse(spec: &str) -> Result<Self, ManifestError> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
      return Err(ManifestError::InvalidCapability {
        section: "filesystem",
        value: spec.to_string(),
        reason: "empty filesystem entry",
      });
    }
    let (access, path_str) = match trimmed.split_once(':') {
      Some((prefix, rest)) => match prefix.trim().to_ascii_lowercase().as_str() {
        "read" => (FsAccess::Read, rest.trim()),
        "write" => (FsAccess::Write, rest.trim()),
        // No recognised prefix → treat the whole spec as a bare path
        // (e.g. an absolute path on Windows like "C:\foo" or a path
        // containing a literal colon). This mirrors the most common
        // reading of unprefixed entries.
        _ => (FsAccess::Read, trimmed),
      },
      None => (FsAccess::Read, trimmed),
    };
    if path_str.is_empty() {
      return Err(ManifestError::InvalidCapability {
        section: "filesystem",
        value: spec.to_string(),
        reason: "missing path after access prefix",
      });
    }
    Ok(FilesystemEntry {
      access,
      path: PathBuf::from(path_str),
    })
  }
}

impl Capabilities {
  /// Parse every `filesystem` entry into structured [`FilesystemEntry`].
  /// Returns the first parse error if any entry is malformed.
  pub fn filesystem_entries(&self) -> Result<Vec<FilesystemEntry>, ManifestError> {
    self
      .filesystem
      .iter()
      .map(|spec| FilesystemEntry::parse(spec))
      .collect()
  }

  /// `true` when at least one filesystem entry requests read access.
  pub fn requires_fs_read(&self) -> bool {
    self
      .filesystem_entries()
      .map(|entries| entries.iter().any(|e| matches!(e.access, FsAccess::Read)))
      .unwrap_or(false)
  }

  /// `true` when at least one filesystem entry requests write access.
  pub fn requires_fs_write(&self) -> bool {
    self
      .filesystem_entries()
      .map(|entries| entries.iter().any(|e| matches!(e.access, FsAccess::Write)))
      .unwrap_or(false)
  }

  /// `true` when the manifest grants outbound network.
  pub fn requires_net(&self) -> bool {
    !self.network.is_empty()
  }

  /// `true` when the manifest grants child-process spawning.
  pub fn requires_exec(&self) -> bool {
    !self.processes.is_empty()
  }

  /// `true` when the manifest grants environment-variable access.
  pub fn requires_env(&self) -> bool {
    !self.env_vars.is_empty()
  }
}

#[derive(Debug, Error)]
pub enum ManifestError {
  #[error("failed to read plugin manifest: {0}")]
  Io(#[from] std::io::Error),
  #[error("failed to parse plugin manifest: {0}")]
  Parse(#[from] toml::de::Error),
  #[error("plugin manifest declares protocol '{actual}' but host implements '{expected}'")]
  ProtocolMismatch {
    actual: String,
    expected: &'static str,
  },
  #[error("plugin runtime '{runtime:?}' is not supported by this host")]
  UnsupportedRuntime { runtime: PluginRuntime },
  #[error("invalid capability in [plugin.capabilities].{section}: '{value}' — {reason}")]
  InvalidCapability {
    section: &'static str,
    value: String,
    reason: &'static str,
  },
  #[error("invalid [plugin.dry_run] configuration: {0}")]
  InvalidDryRun(String),
  #[error("invalid [plugin.signature] configuration: {0}")]
  InvalidSignature(String),
}

impl PluginManifest {
  /// Load a manifest from `path` and return both the parsed value and the
  /// directory the manifest lives in (for resolving relative `entrypoint`).
  pub fn load_from_path(path: &Path) -> Result<(Self, PathBuf), ManifestError> {
    let raw = std::fs::read_to_string(path)?;
    let mut manifest: PluginManifest = toml::from_str(&raw)?;
    if manifest.plugin.protocol.is_empty() {
      manifest.plugin.protocol = SUPPORTED_PROTOCOL_VERSION.to_string();
    }
    let manifest_dir = match path.parent() {
      Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
      _ => PathBuf::from("."),
    };
    Ok((manifest, manifest_dir))
  }

  /// Reject manifests this host cannot run.
  pub fn validate(&self) -> Result<(), ManifestError> {
    if self.plugin.protocol != SUPPORTED_PROTOCOL_VERSION {
      return Err(ManifestError::ProtocolMismatch {
        actual: self.plugin.protocol.clone(),
        expected: SUPPORTED_PROTOCOL_VERSION,
      });
    }
    if !matches!(self.plugin.runtime, PluginRuntime::Subprocess) {
      return Err(ManifestError::UnsupportedRuntime {
        runtime: self.plugin.runtime.clone(),
      });
    }
    if let Some(dry_run) = &self.plugin.dry_run {
      dry_run.validate()?;
    }
    if let Some(signature) = &self.plugin.signature {
      signature.validate()?;
    }
    Ok(())
  }

  /// Resolve the entrypoint path against `manifest_dir` if it's relative.
  pub fn resolve_entrypoint(&self, manifest_dir: &Path) -> PathBuf {
    if self.plugin.entrypoint.is_absolute() {
      self.plugin.entrypoint.clone()
    } else {
      manifest_dir.join(&self.plugin.entrypoint)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_minimal_manifest() {
    let raw = r#"
[plugin]
name = "demo"
version = "0.1.0"
entrypoint = "bin/demo"

[[plugin.nodes]]
type = "demo_node"
description = "A demo node."
"#;
    let manifest: PluginManifest = toml::from_str(raw).unwrap();
    assert_eq!(manifest.plugin.name, "demo");
    assert_eq!(manifest.plugin.runtime, PluginRuntime::Subprocess);
    assert_eq!(manifest.plugin.nodes.len(), 1);
    assert_eq!(manifest.plugin.nodes[0].node_type, "demo_node");
    // Default — no `[plugin.dry_run]` section means doctor smoke
    // is opt-in, not silently enabled.
    assert!(manifest.plugin.dry_run.is_none());
    // Default — no `[plugin.signature]` section means unsigned.
    assert!(manifest.plugin.signature.is_none());
  }

  #[test]
  fn parses_manifest_with_signature_section() {
    let raw = r#"
[plugin]
name = "signed-demo"
version = "0.1.0"
entrypoint = "bin/demo"

[plugin.signature]
algorithm = "ed25519"
key_id = "publisher-a"
value = "c29tZS1zaWduYXR1cmU="
"#;
    let mut manifest: PluginManifest = toml::from_str(raw).unwrap();
    manifest.plugin.protocol = SUPPORTED_PROTOCOL_VERSION.to_string();
    let signature = manifest
      .plugin
      .signature
      .as_ref()
      .expect("signature parsed");
    assert_eq!(signature.algorithm, "ed25519");
    assert_eq!(signature.key_id, "publisher-a");
    assert_eq!(signature.value, "c29tZS1zaWduYXR1cmU=");
    assert!(signature.validate().is_ok());
    assert!(manifest.validate().is_ok());
  }

  #[test]
  fn manifest_validate_rejects_empty_signature_fields() {
    let raw = r#"
[plugin]
name = "bad-signature"
version = "0.1.0"
entrypoint = "bin/demo"

[plugin.signature]
algorithm = "ed25519"
key_id = ""
value = "c29tZS1zaWduYXR1cmU="
"#;
    let mut manifest: PluginManifest = toml::from_str(raw).unwrap();
    manifest.plugin.protocol = SUPPORTED_PROTOCOL_VERSION.to_string();
    assert!(matches!(
      manifest.validate(),
      Err(ManifestError::InvalidSignature(_))
    ));
  }

  #[test]
  fn parses_manifest_with_dry_run_section() {
    let raw = r#"
[plugin]
name = "smoke-aware"
version = "0.1.0"
entrypoint = "bin/smoke"

[plugin.dry_run]
args = ["--smoke", "--quiet"]
timeout_ms = 2000
expected_exit = 0
"#;
    let manifest: PluginManifest = toml::from_str(raw).unwrap();
    let dry_run = manifest.plugin.dry_run.expect("dry_run parsed");
    assert_eq!(dry_run.args, vec!["--smoke", "--quiet"]);
    assert_eq!(dry_run.timeout_ms, 2000);
    assert_eq!(dry_run.expected_exit, 0);
    assert!(dry_run.validate().is_ok());
  }

  #[test]
  fn parses_manifest_dry_run_applies_defaults_for_optional_fields() {
    // Only `args` is required; `timeout_ms` and `expected_exit` use
    // crate-level defaults (1000 / 0).
    let raw = r#"
[plugin]
name = "minimal-smoke"
version = "0.1.0"
entrypoint = "bin/x"

[plugin.dry_run]
args = ["--version"]
"#;
    let manifest: PluginManifest = toml::from_str(raw).unwrap();
    let dry_run = manifest.plugin.dry_run.expect("dry_run parsed");
    assert_eq!(dry_run.args, vec!["--version"]);
    assert_eq!(dry_run.timeout_ms, 1000);
    assert_eq!(dry_run.expected_exit, 0);
  }

  #[test]
  fn manifest_validate_rejects_empty_dry_run_args() {
    let manifest = PluginManifest {
      plugin: PluginSection {
        name: "bad-smoke".into(),
        version: "0.1.0".into(),
        runtime: PluginRuntime::Subprocess,
        entrypoint: PathBuf::from("bin/x"),
        protocol: SUPPORTED_PROTOCOL_VERSION.into(),
        nodes: vec![],
        capabilities: Capabilities::default(),
        dry_run: Some(DryRunSpec {
          args: vec![],
          timeout_ms: 1000,
          expected_exit: 0,
        }),
        signature: None,
      },
    };
    assert!(matches!(
      manifest.validate(),
      Err(ManifestError::InvalidDryRun(_))
    ));
  }

  #[test]
  fn rejects_unknown_protocol() {
    let mut manifest = PluginManifest {
      plugin: PluginSection {
        name: "x".into(),
        version: "0.1.0".into(),
        runtime: PluginRuntime::Subprocess,
        entrypoint: PathBuf::from("bin/x"),
        protocol: "yanshi.plugin/999".into(),
        nodes: vec![],
        capabilities: Capabilities::default(),
        dry_run: None,
        signature: None,
      },
    };
    assert!(matches!(
      manifest.validate(),
      Err(ManifestError::ProtocolMismatch { .. })
    ));
    manifest.plugin.protocol = SUPPORTED_PROTOCOL_VERSION.into();
    assert!(manifest.validate().is_ok());
  }

  #[test]
  fn rejects_wasm_runtime_in_v0_3() {
    let manifest = PluginManifest {
      plugin: PluginSection {
        name: "x".into(),
        version: "0.1.0".into(),
        runtime: PluginRuntime::Wasm,
        entrypoint: PathBuf::from("bin/x"),
        protocol: SUPPORTED_PROTOCOL_VERSION.into(),
        nodes: vec![],
        capabilities: Capabilities::default(),
        dry_run: None,
        signature: None,
      },
    };
    assert!(matches!(
      manifest.validate(),
      Err(ManifestError::UnsupportedRuntime { .. })
    ));
  }

  #[test]
  fn resolves_relative_entrypoint() {
    let manifest = PluginManifest {
      plugin: PluginSection {
        name: "x".into(),
        version: "0.1.0".into(),
        runtime: PluginRuntime::Subprocess,
        entrypoint: PathBuf::from("bin/x"),
        protocol: SUPPORTED_PROTOCOL_VERSION.into(),
        nodes: vec![],
        capabilities: Capabilities::default(),
        dry_run: None,
        signature: None,
      },
    };
    assert_eq!(
      manifest.resolve_entrypoint(Path::new("/tmp/foo")),
      PathBuf::from("/tmp/foo/bin/x")
    );
  }

  #[test]
  fn filesystem_entry_parses_bare_path_as_read() {
    let entry = FilesystemEntry::parse("./models").unwrap();
    assert_eq!(entry.access, FsAccess::Read);
    assert_eq!(entry.path, PathBuf::from("./models"));
  }

  #[test]
  fn filesystem_entry_parses_explicit_access_prefix() {
    let read = FilesystemEntry::parse("read:./inputs").unwrap();
    assert_eq!(read.access, FsAccess::Read);
    assert_eq!(read.path, PathBuf::from("./inputs"));

    let write = FilesystemEntry::parse("write:/tmp/out").unwrap();
    assert_eq!(write.access, FsAccess::Write);
    assert_eq!(write.path, PathBuf::from("/tmp/out"));
  }

  #[test]
  fn filesystem_entry_tolerates_whitespace() {
    let entry = FilesystemEntry::parse("  write : /var/log  ").unwrap();
    assert_eq!(entry.access, FsAccess::Write);
    assert_eq!(entry.path, PathBuf::from("/var/log"));
  }

  #[test]
  fn filesystem_entry_falls_back_to_bare_path_for_unknown_prefix() {
    // Windows-style absolute paths contain a literal colon; treat as bare.
    let entry = FilesystemEntry::parse("C:\\models").unwrap();
    assert_eq!(entry.access, FsAccess::Read);
    assert_eq!(entry.path, PathBuf::from("C:\\models"));
  }

  #[test]
  fn filesystem_entry_rejects_empty_or_pathless() {
    assert!(matches!(
      FilesystemEntry::parse(""),
      Err(ManifestError::InvalidCapability { .. })
    ));
    assert!(matches!(
      FilesystemEntry::parse("read:"),
      Err(ManifestError::InvalidCapability { .. })
    ));
    assert!(matches!(
      FilesystemEntry::parse("write:   "),
      Err(ManifestError::InvalidCapability { .. })
    ));
  }

  #[test]
  fn capabilities_classify_required_caps() {
    let caps = Capabilities {
      filesystem: vec!["read:./models".into(), "write:/tmp/output".into()],
      network: vec!["api.example.com".into()],
      processes: vec![],
      env_vars: vec!["TESSDATA_PREFIX".into()],
    };
    assert!(caps.requires_fs_read());
    assert!(caps.requires_fs_write());
    assert!(caps.requires_net());
    assert!(!caps.requires_exec());
    assert!(caps.requires_env());

    let entries = caps.filesystem_entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].access, FsAccess::Read);
    assert_eq!(entries[1].access, FsAccess::Write);
  }

  #[test]
  fn empty_capabilities_grant_nothing() {
    let caps = Capabilities::default();
    assert!(!caps.requires_fs_read());
    assert!(!caps.requires_fs_write());
    assert!(!caps.requires_net());
    assert!(!caps.requires_exec());
    assert!(!caps.requires_env());
    assert!(caps.filesystem_entries().unwrap().is_empty());
  }
}
