//! Support for the [Agent Skills open standard](https://agentskills.io) (`SKILL.md` format).
//!
//! A `SKILL.md` file contains YAML frontmatter followed by Markdown instructions:
//!
//! ```markdown
//! ---
//! name: pdf-processing
//! description: Extract text and tables from PDF files.
//! allowed-tools: shell file
//! ---
//!
//! # PDF Processing
//!
//! ## When to use this skill
//! Use this skill when the user needs to work with PDF files...
//! ```
//!
//! The [`SkillMd`] type parses this format and can be converted into an
//! yanshi [`SkillManifest`] (with defaults applied for model/memory fields
//! that have no equivalent in the standard format).

use std::collections::HashMap;

use serde::Deserialize;

use crate::{
  error::SkillError,
  manifest::{
    McpServerConfig, PersonaConfig, SecurityConfig, SkillInfo, SkillManifest, ToolConfig,
  },
};

/// Filename expected for the Agent Skills standard manifest.
pub const SKILL_MD_FILE: &str = "SKILL.md";

// ── Frontmatter schema ────────────────────────────────────────────────────────

/// Raw YAML frontmatter parsed from a `SKILL.md` file.
///
/// Only fields defined in the Agent Skills specification are captured.
/// Unknown keys are silently ignored.
#[derive(Debug, Clone, Deserialize)]
struct SkillMdFrontmatter {
  /// Required: short identifier (lowercase letters, numbers, hyphens).
  pub name: String,
  /// Required: what the skill does and when to use it.
  pub description: String,
  /// Optional: license name or reference to a bundled license file.
  pub license: Option<String>,
  /// Optional: environment / compatibility notes.
  pub compatibility: Option<String>,
  /// Optional: arbitrary key-value metadata map.
  pub metadata: Option<HashMap<String, String>>,
  /// Optional Yanshi extension: MCP servers exposed to this skill.
  pub mcp_servers: Option<Vec<McpServerConfig>>,
  /// Optional Yanshi extension: security and tool governance controls.
  pub security: Option<SecurityConfig>,
  /// Optional (experimental): space-delimited list of pre-approved tools.
  /// e.g. `"shell file script"`
  #[serde(rename = "allowed-tools")]
  pub allowed_tools: Option<String>,
  /// Optional Yanshi extension: LLM model name. When set, populates
  /// `SkillManifest.model.name` instead of falling back to the
  /// `gpt-4o` default. F-AF-2: previously silently dropped because
  /// this field wasn't in the struct.
  pub model: Option<String>,
}

// ── Public type ───────────────────────────────────────────────────────────────

/// A parsed `SKILL.md` file: structured frontmatter + raw Markdown body.
///
/// Convert to [`SkillManifest`] via [`SkillMd::into_manifest`].
#[derive(Debug, Clone)]
pub struct SkillMd {
  /// Validated frontmatter fields.
  pub name: String,
  pub description: String,
  pub license: Option<String>,
  pub compatibility: Option<String>,
  pub metadata: HashMap<String, String>,
  /// Yanshi extension for declarative MCP server attachment.
  pub mcp_servers: Vec<McpServerConfig>,
  /// Yanshi extension for security and tool governance controls.
  pub security: SecurityConfig,
  /// Space-delimited tool names recognised by yanshi
  /// (`shell`, `file`, `http`, `script`, `code_exec`).
  pub allowed_tools: Vec<String>,
  /// Yanshi extension: LLM model name from frontmatter (F-AF-2).
  /// `None` when unspecified — `into_manifest` will leave
  /// `ModelConfig::name` empty so `resolved_model()` returns the
  /// `gpt-4o` default.
  pub model: Option<String>,
  /// The Markdown body that becomes the agent's persona / system prompt.
  pub body: String,
}

impl SkillMd {
  /// Parse the contents of a `SKILL.md` file.
  ///
  /// Returns [`SkillError::ParseError`] if the frontmatter is missing or
  /// cannot be deserialised.
  pub fn parse(content: &str) -> Result<Self, SkillError> {
    // ── Split frontmatter from body ───────────────────────────────────────
    let (frontmatter_str, body) =
      split_frontmatter(content).ok_or_else(|| SkillError::ParseError {
        message: "SKILL.md must begin with YAML frontmatter delimited by '---'".to_string(),
      })?;

    // ── Deserialise YAML ─────────────────────────────────────────────────
    let fm: SkillMdFrontmatter =
      serde_yaml::from_str(frontmatter_str).map_err(|e| SkillError::ParseError {
        message: format!("SKILL.md frontmatter YAML error: {}", e),
      })?;

    // ── Validate required fields ─────────────────────────────────────────
    validate_name(&fm.name)?;
    if fm.description.trim().is_empty() {
      return Err(SkillError::ParseError {
        message: "SKILL.md 'description' must not be empty".to_string(),
      });
    }

    // ── Parse allowed-tools ──────────────────────────────────────────────
    let allowed_tools: Vec<String> = fm
      .allowed_tools
      .as_deref()
      .unwrap_or("")
      .split_whitespace()
      .map(|t| t.to_lowercase())
      .filter(|t| !t.is_empty())
      .collect();

    Ok(Self {
      name: fm.name,
      description: fm.description,
      license: fm.license,
      compatibility: fm.compatibility,
      metadata: fm.metadata.unwrap_or_default(),
      mcp_servers: fm.mcp_servers.unwrap_or_default(),
      security: fm.security.unwrap_or_default(),
      allowed_tools,
      model: fm
        .model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()),
      body: body.trim().to_string(),
    })
  }

  /// Convert into a [`SkillManifest`], applying yanshi-specific defaults
  /// for `model` and `memory` (which have no equivalent in the standard).
  pub fn into_manifest(self) -> SkillManifest {
    // Build tool configs from allowed-tools list (no per-tool constraints).
    let tools: Vec<ToolConfig> = self
      .allowed_tools
      .iter()
      .map(|name| ToolConfig {
        name: name.clone(),
        ..ToolConfig::default()
      })
      .collect();

    // Prefer structured SKILL.md frontmatter. Keep metadata.mcp_servers as a
    // compatibility fallback for older skills that stored JSON in metadata.
    let mut mcp_servers = self.mcp_servers;
    if mcp_servers.is_empty()
      && let Some(mcp_str) = self.metadata.get("mcp_servers")
    {
      if let Ok(parsed_servers) =
        serde_json::from_str::<Vec<crate::manifest::McpServerConfig>>(mcp_str)
      {
        mcp_servers = parsed_servers;
      } else {
        tracing::warn!("Failed to parse mcp_servers from SKILL.md metadata");
      }
    }

    SkillManifest {
      skill: SkillInfo {
        name: self.name,
        // SKILL.md has no version field; use a sensible default.
        version: self
          .metadata
          .get("version")
          .cloned()
          .unwrap_or_else(|| "1.0.0".to_string()),
        description: self.description,
      },
      persona: PersonaConfig {
        role: self.body,
        language: self.metadata.get("language").cloned(),
      },
      // F-AF-2: honour the frontmatter `model:` field. Previously
      // dropped on the floor because `SkillMdFrontmatter` didn't
      // declare the field; users would set `model: kimi-k2.6` and
      // wonder why the agent kept calling gpt-4o.
      model: crate::manifest::ModelConfig {
        name: self.model,
        ..Default::default()
      },
      security: self.security,
      tools,
      mcp_servers,
      knowledge: vec![],
      memory: None,
      validation: Default::default(),
      // SKILL.md has no `[[scripts]]`-equivalent frontmatter syntax yet; a
      // `script` tool declared this way falls into the "no integrity
      // manifest" back-compat path in `SkillLoader::validate` (S1.1).
      scripts: vec![],
      // No `[dependencies]`-equivalent frontmatter syntax either (S2.1).
      dependencies: Default::default(),
      workflows: Vec::new(),
    }
  }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Split a SKILL.md string into `(frontmatter_str, body_str)`.
///
/// The frontmatter must be enclosed between `---` lines at the start of the
/// file.  Leading/trailing whitespace around the frontmatter block is trimmed.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
  let content = content.trim_start();
  // Must start with "---"
  let after_first = content.strip_prefix("---")?;
  // The first `---` may be followed by optional spaces/a newline.
  let after_first = after_first
    .strip_prefix('\n')
    .or_else(|| after_first.strip_prefix("\r\n"))
    .unwrap_or(after_first);

  // Find the closing "---"
  let close_marker = "\n---";
  let close_pos = after_first.find(close_marker)?;
  let frontmatter = &after_first[..close_pos];
  let remainder = &after_first[close_pos + close_marker.len()..];
  // Skip optional newline after closing ---
  let body = remainder
    .strip_prefix('\n')
    .or_else(|| remainder.strip_prefix("\r\n"))
    .unwrap_or(remainder);
  Some((frontmatter, body))
}

/// Validate the `name` field according to the Agent Skills specification.
fn validate_name(name: &str) -> Result<(), SkillError> {
  if name.is_empty() || name.len() > 64 {
    return Err(SkillError::ParseError {
      message: format!(
        "SKILL.md 'name' must be 1-64 characters, got {}",
        name.len()
      ),
    });
  }
  if name.starts_with('-') || name.ends_with('-') {
    return Err(SkillError::ParseError {
      message: "SKILL.md 'name' must not start or end with '-'".to_string(),
    });
  }
  if name.contains("--") {
    return Err(SkillError::ParseError {
      message: "SKILL.md 'name' must not contain consecutive hyphens '--'".to_string(),
    });
  }
  let valid = name
    .chars()
    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
  if !valid {
    return Err(SkillError::ParseError {
      message: "SKILL.md 'name' may only contain lowercase letters, digits, and '-'".to_string(),
    });
  }
  Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;

  const MINIMAL: &str = r#"---
name: pdf-processing
description: Extract text and tables from PDF files.
---

# PDF Processing
Instructions here.
"#;

  #[test]
  fn parses_minimal_skill_md() {
    let skill = SkillMd::parse(MINIMAL).unwrap();
    assert_eq!(skill.name, "pdf-processing");
    assert_eq!(skill.description, "Extract text and tables from PDF files.");
    assert!(skill.body.contains("Instructions here."));
    assert!(skill.allowed_tools.is_empty());
  }

  /// F-AF-2: `model:` in frontmatter must populate `SkillMd.model`
  /// and survive into `SkillManifest.model.name` after conversion.
  /// Previously this was silently dropped because the field wasn't
  /// in `SkillMdFrontmatter`, causing user `model: kimi-k2.6` to be
  /// ignored and the default `gpt-4o` to win.
  #[test]
  fn parses_model_field_into_manifest() {
    let content = r#"---
name: my-skill
description: A skill with a model override.
model: kimi-k2.6
---

Body.
"#;
    let skill = SkillMd::parse(content).unwrap();
    assert_eq!(skill.model.as_deref(), Some("kimi-k2.6"));

    let manifest = skill.into_manifest();
    assert_eq!(manifest.model.name.as_deref(), Some("kimi-k2.6"));
    assert_eq!(manifest.model.resolved_model(), "kimi-k2.6");
  }

  /// F-AF-2: absence of `model:` keeps the default behaviour
  /// (`SkillMd.model == None`, manifest falls through to `gpt-4o`).
  #[test]
  fn parses_no_model_field_keeps_default() {
    let skill = SkillMd::parse(MINIMAL).unwrap();
    assert!(skill.model.is_none());

    let manifest = skill.into_manifest();
    assert!(manifest.model.name.is_none());
    assert_eq!(manifest.model.resolved_model(), "gpt-4o");
  }

  /// F-AF-2: empty / whitespace-only `model:` is treated as absent.
  /// Prevents callers from accidentally setting `model: ""` (which
  /// would otherwise pass through to `resolved_model` as the empty
  /// string and break LLM provider lookups).
  #[test]
  fn parses_empty_model_field_as_none() {
    let content = r#"---
name: my-skill
description: A skill with an empty model.
model: "   "
---

Body.
"#;
    let skill = SkillMd::parse(content).unwrap();
    assert!(skill.model.is_none());
  }

  #[test]
  fn parses_allowed_tools() {
    let content = r#"---
name: my-skill
description: A skill with tools.
allowed-tools: shell file script
---

Body text.
"#;
    let skill = SkillMd::parse(content).unwrap();
    assert_eq!(skill.allowed_tools, vec!["shell", "file", "script"]);
  }

  #[test]
  fn parses_metadata() {
    let content = r#"---
name: versioned-skill
description: Has metadata.
metadata:
  author: acme-corp
  version: "2.1"
---

Body.
"#;
    let skill = SkillMd::parse(content).unwrap();
    assert_eq!(
      skill.metadata.get("author").map(String::as_str),
      Some("acme-corp")
    );
    let manifest = skill.into_manifest();
    assert_eq!(manifest.skill.version, "2.1");
  }

  #[test]
  fn parses_structured_mcp_servers() {
    let content = r#"---
name: mcp-skill
description: Has MCP servers.
mcp_servers:
  - name: local-fixture
    command: python3
    args:
      - server.py
    env:
      TEST_MODE: "1"
---

Body.
"#;
    let skill = SkillMd::parse(content).unwrap();
    assert_eq!(skill.mcp_servers.len(), 1);
    assert_eq!(skill.mcp_servers[0].name, "local-fixture");

    let manifest = skill.into_manifest();
    assert_eq!(manifest.mcp_servers.len(), 1);
    assert_eq!(manifest.mcp_servers[0].command, "python3");
    assert_eq!(manifest.mcp_servers[0].args, vec!["server.py"]);
    assert_eq!(
      manifest.mcp_servers[0]
        .env
        .get("TEST_MODE")
        .map(String::as_str),
      Some("1")
    );
  }

  #[test]
  fn structured_mcp_servers_take_precedence_over_metadata_json() {
    let content = r#"---
name: mcp-precedence
description: Uses structured MCP servers first.
metadata:
  mcp_servers: '[{"name":"metadata","command":"ignored"}]'
mcp_servers:
  - name: structured
    command: python3
---

Body.
"#;
    let manifest = SkillMd::parse(content).unwrap().into_manifest();
    assert_eq!(manifest.mcp_servers.len(), 1);
    assert_eq!(manifest.mcp_servers[0].name, "structured");
  }

  #[test]
  fn rejects_uppercase_name() {
    let content = r#"---
name: MySkill
description: Bad name.
---
Body.
"#;
    let err = SkillMd::parse(content).unwrap_err();
    assert!(matches!(err, SkillError::ParseError { .. }));
  }

  #[test]
  fn rejects_missing_frontmatter() {
    let err = SkillMd::parse("# Just a markdown file with no frontmatter").unwrap_err();
    assert!(matches!(err, SkillError::ParseError { .. }));
  }

  #[test]
  fn into_manifest_round_trip() {
    let skill = SkillMd::parse(MINIMAL).unwrap();
    let manifest = skill.into_manifest();
    assert_eq!(manifest.skill.name, "pdf-processing");
    assert_eq!(
      manifest.persona.role,
      "# PDF Processing\nInstructions here."
    );
    assert!(manifest.tools.is_empty());
    assert_eq!(manifest.skill.version, "1.0.0");
  }
}
