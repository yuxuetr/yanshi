use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use yanshi_tools::SecurityProfile;

use crate::{error::SkillError, manifest::SkillManifest, skill_md::SkillMd};

const MANIFEST_FILE: &str = "skill.toml";
const SKILL_MD_FILE: &str = "SKILL.md";
const KNOWN_TOOLS: &[&str] = &[
  "shell",
  "file",
  "http",
  "script",
  "code_exec",
  // Modality Tool adapters from `yanshi-tools-ai` — thin wrappers over
  // `yanshi_llm::Yanshi::tts`/`text2image_for`/etc., driven entirely
  // by the model registry YAML (`vendor:`/`accepts:`). See that crate's
  // module docs for the full rationale.
  "tts",
  "asr",
  "text_to_image",
  "image_to_image",
  "image_edit",
  "image_understand",
  "text_to_video",
];
// U2.2: "semantic" was already handled by `builder.rs::build_memory` but
// missing here, so a valid `type = "semantic"` manifest failed validation
// before ever reaching the builder — a real bug the U2.4 evaluation
// finding described as a docs-only discrepancy.
const KNOWN_MEMORY_TYPES: &[&str] = &["session", "sqlite", "semantic", "none"];

/// Loads and validates a skill manifest from a skill directory.
///
/// Supported manifest formats:
/// - `SKILL.md` is the recommended human-facing skill format.
/// - `skill.toml` is retained for compatibility and structured runtime config.
///
/// If both files exist in the same directory, `skill.toml` is loaded. This
/// preserves existing Yanshi behavior and lets a structured manifest override
/// the portable `SKILL.md` entrypoint when needed.
pub struct SkillLoader;

impl SkillLoader {
  /// Load a [`SkillManifest`] from `skill_dir`.
  ///
  /// Loads `skill.toml` first when present; falls back to `SKILL.md`.
  pub fn load(skill_dir: &Path) -> Result<SkillManifest, SkillError> {
    let toml_path = skill_dir.join(MANIFEST_FILE);
    if toml_path.exists() {
      let content = std::fs::read_to_string(&toml_path)?;
      let manifest: SkillManifest = toml::from_str(&content)?;
      return Ok(manifest);
    }

    let md_path = skill_dir.join(SKILL_MD_FILE);
    if md_path.exists() {
      let content = std::fs::read_to_string(&md_path)?;
      let skill_md = SkillMd::parse(&content)?;
      return Ok(skill_md.into_manifest());
    }

    Err(SkillError::ManifestNotFound {
      path: format!("{} (tried skill.toml and SKILL.md)", skill_dir.display()),
    })
  }

  /// Validate a loaded manifest and return a list of warnings.
  /// Returns `Err` for hard failures, `Ok(warnings)` for soft issues.
  ///
  /// Uses the ambient [`SecurityProfile`] (`YANSHI_SECURITY_PROFILE`,
  /// defaulting to [`SecurityProfile::Local`] when unset or unparsable) to
  /// gate the S1.1 script-integrity back-compat check. Callers that already
  /// have a resolved profile (server/CLI entry points) should prefer
  /// [`Self::validate_with_profile`] instead of relying on the environment
  /// read happening here.
  pub fn validate(manifest: &SkillManifest, skill_dir: &Path) -> Result<Vec<String>, SkillError> {
    Self::validate_with_profile(
      manifest,
      skill_dir,
      SecurityProfile::from_env().unwrap_or_default(),
    )
  }

  /// Same as [`Self::validate`], but with an explicit [`SecurityProfile`]
  /// instead of reading `YANSHI_SECURITY_PROFILE` from the environment.
  pub fn validate_with_profile(
    manifest: &SkillManifest,
    skill_dir: &Path,
    profile: SecurityProfile,
  ) -> Result<Vec<String>, SkillError> {
    let mut warnings: Vec<String> = Vec::new();

    // ── skill section ───────────────────────────────────────────────────
    if manifest.skill.name.trim().is_empty() {
      return Err(SkillError::ValidationError {
        message: "[skill].name must not be empty".to_string(),
      });
    }
    if manifest.skill.version.trim().is_empty() {
      warnings.push("[skill].version is empty".to_string());
    }
    if manifest.skill.description.trim().is_empty() {
      warnings.push("[skill].description is empty".to_string());
    }

    // ── persona section ─────────────────────────────────────────────────
    if manifest.persona.role.trim().is_empty() {
      return Err(SkillError::ValidationError {
        message: "[persona].role must not be empty".to_string(),
      });
    }

    // ── workflows (Y4.4) ────────────────────────────────────────────────
    let mut workflow_names = std::collections::HashSet::new();
    for workflow in &manifest.workflows {
      let name = workflow.name.trim();
      if name.is_empty()
        || !name
          .chars()
          .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
      {
        return Err(SkillError::ValidationError {
          message: format!(
            "[[workflows]].name '{}' must be non-empty and use only letters, digits, '_' or '-'",
            workflow.name
          ),
        });
      }
      if !workflow_names.insert(name) {
        return Err(SkillError::ValidationError {
          message: format!("[[workflows]].name '{name}' is declared twice"),
        });
      }
      if !skill_dir.join(&workflow.path).is_file() {
        return Err(SkillError::ValidationError {
          message: format!(
            "[[workflows]] '{name}': {} does not exist (paths are relative to the skill directory)",
            workflow.path
          ),
        });
      }
    }

    // ── tools ───────────────────────────────────────────────────────────
    for tool in &manifest.tools {
      let name_lc = tool.name.to_lowercase();
      if !KNOWN_TOOLS.contains(&name_lc.as_str()) {
        return Err(SkillError::UnknownTool {
          name: tool.name.clone(),
        });
      }
      // "script" tool requires a scripts/ directory to exist.
      if name_lc == "script" {
        let scripts_dir = skill_dir.join("scripts");
        if !scripts_dir.is_dir() {
          return Err(SkillError::ValidationError {
            message: format!(
              "Tool 'script' declared but scripts/ directory not found at {}",
              scripts_dir.display()
            ),
          });
        }
        validate_script_integrity(manifest, &scripts_dir, profile, &mut warnings)?;
      }
    }

    // ── dependencies (S2.1) ─────────────────────────────────────────────
    if let Some(requirements_rel_path) = &manifest.dependencies.python {
      validate_python_requirements(requirements_rel_path, skill_dir)?;
    }

    // ── MCP servers ─────────────────────────────────────────────────────
    let max_servers = manifest.security.resolved_mcp_max_servers();
    if manifest.mcp_servers.len() > max_servers {
      return Err(SkillError::ValidationError {
        message: format!(
          "Skill declares {} MCP servers, exceeding security.mcp_max_servers={}",
          manifest.mcp_servers.len(),
          max_servers
        ),
      });
    }

    for server in &manifest.mcp_servers {
      if server.name.trim().is_empty() {
        return Err(SkillError::ValidationError {
          message: "[[mcp_servers]].name must not be empty".to_string(),
        });
      }
      if server.command.trim().is_empty() {
        return Err(SkillError::ValidationError {
          message: format!(
            "[[mcp_servers]] '{}' command must not be empty",
            server.name
          ),
        });
      }
      if !manifest.security.mcp_server_allowlist.is_empty()
        && !manifest
          .security
          .mcp_server_allowlist
          .iter()
          .any(|name| name == &server.name)
      {
        return Err(SkillError::ValidationError {
          message: format!(
            "[[mcp_servers]] '{}' is not listed in security.mcp_server_allowlist",
            server.name
          ),
        });
      }

      let executable = executable_name(&server.command);
      if !manifest
        .security
        .resolved_mcp_command_allowlist()
        .iter()
        .any(|allowed| allowed == &executable)
      {
        return Err(SkillError::ValidationError {
          message: format!(
            "[[mcp_servers]] '{}' command '{}' is not listed in security.mcp_command_allowlist",
            server.name, executable
          ),
        });
      }

      if manifest.security.mcp_env_allowlist.is_empty() && !server.env.is_empty() {
        return Err(SkillError::ValidationError {
          message: format!(
            "[[mcp_servers]] '{}' declares env values but security.mcp_env_allowlist is empty",
            server.name
          ),
        });
      }
      for key in server.env.keys() {
        if !manifest
          .security
          .mcp_env_allowlist
          .iter()
          .any(|allowed| allowed == key)
        {
          return Err(SkillError::ValidationError {
            message: format!(
              "[[mcp_servers]] '{}' env '{}' is not listed in security.mcp_env_allowlist",
              server.name, key
            ),
          });
        }
      }

      if server.timeout_secs.is_some_and(|timeout| timeout == 0) {
        warnings.push(format!(
          "[[mcp_servers]] '{}' timeout_secs=0 will be clamped to 1",
          server.name
        ));
      }
      if server.max_concurrent_calls.is_some_and(|limit| limit == 0) {
        warnings.push(format!(
          "[[mcp_servers]] '{}' max_concurrent_calls=0 will be clamped to 1",
          server.name
        ));
      }
    }

    // ── knowledge ───────────────────────────────────────────────────────
    for kc in &manifest.knowledge {
      let resolved = resolve_knowledge_path(&kc.path, skill_dir);
      if resolved.is_empty() {
        return Err(SkillError::KnowledgeFileNotFound {
          path: format!("{} (in {})", kc.path, skill_dir.display()),
        });
      }
    }

    // ── memory ──────────────────────────────────────────────────────────
    if let Some(mem) = &manifest.memory {
      let t = mem.memory_type.as_str();
      if !KNOWN_MEMORY_TYPES.contains(&t) {
        return Err(SkillError::ValidationError {
          message: format!(
            "[memory].type '{}' is unknown. Expected one of: {}",
            t,
            KNOWN_MEMORY_TYPES.join(", ")
          ),
        });
      }
      if t == "sqlite" && manifest.skill.name.trim().is_empty() {
        warnings.push(
          "[memory] type is sqlite but skill.name is empty; db path may be invalid".to_string(),
        );
      }
      if let Some(pref) = &mem.preference
        && pref.enabled
        && manifest.skill.name.trim().is_empty()
      {
        warnings.push(
          "[memory.preference] is enabled but skill.name is empty; db path may be invalid"
            .to_string(),
        );
      }
      if let Some(proj) = &mem.project
        && proj.enabled
        && manifest.skill.name.trim().is_empty()
      {
        warnings.push(
          "[memory.project] is enabled but skill.name is empty; db path may be invalid".to_string(),
        );
      }
    }

    // ── validator (P4.4 follow-up step 3) ──────────────────────────────
    // Pre-compile the validator so a bad regex or empty command vector
    // surfaces as a manifest error at validate-time, never at eval-run
    // time. The constructed validator is dropped here — eval / CLI
    // callers rebuild it via `build_validator` when they need to run
    // it.
    let _ = crate::validator::build_validator(manifest, skill_dir)?;

    Ok(warnings)
  }
}

fn executable_name(command: &str) -> String {
  Path::new(command)
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or(command)
    .to_string()
}

/// S2.1: a `[dependencies].python` requirements file must be fully pinned
/// and hash-locked — every entry needs an exact `==` version and a
/// `--hash=sha256:...` (pip's own native hash-checking mode, honored by
/// `pip install --require-hashes` in S2.2). This is "declared but wrong"
/// territory (docs/RFC_CODE_EXECUTION_TRUST.md): an unpinned or unhashed
/// dependency defeats the whole point of an isolated, reproducible
/// environment, so it is a hard error in every `SecurityProfile`, not
/// gated like S1.1's "not declared at all" case.
fn validate_python_requirements(
  requirements_rel_path: &str,
  skill_dir: &Path,
) -> Result<(), SkillError> {
  if requirements_rel_path.contains("..") {
    return Err(SkillError::ValidationError {
      message: format!(
        "[dependencies].python '{}' must not contain '..'",
        requirements_rel_path
      ),
    });
  }
  let path = skill_dir.join(requirements_rel_path);
  let content = std::fs::read_to_string(&path).map_err(|e| SkillError::ValidationError {
    message: format!(
      "[dependencies].python references '{}' but it could not be read: {}",
      path.display(),
      e
    ),
  })?;

  // Join pip-style backslash line continuations into logical lines before
  // filtering comments/blanks, so a hash split across lines (common pip
  // requirements style) is validated as one requirement.
  let mut logical_lines: Vec<String> = Vec::new();
  let mut pending = String::new();
  for raw_line in content.lines() {
    let line = raw_line.trim_end();
    pending.push_str(line.trim_end_matches('\\').trim_end());
    if line.trim_end().ends_with('\\') {
      pending.push(' ');
      continue;
    }
    logical_lines.push(std::mem::take(&mut pending));
  }
  if !pending.is_empty() {
    logical_lines.push(pending);
  }

  for line in &logical_lines {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
      continue;
    }
    if !trimmed.contains("==") || !trimmed.contains("--hash=sha256:") {
      return Err(SkillError::ValidationError {
        message: format!(
          "[dependencies].python entry '{}' in {} must be exactly pinned (==) \
           and carry a --hash=sha256:... entry",
          trimmed,
          path.display()
        ),
      });
    }
  }

  Ok(())
}

/// Script filename extensions [`ScriptTool`](yanshi_tools::builtin::ScriptTool)
/// knows how to execute — mirrors `interpreter_for` in
/// `yanshi-tools/src/builtin/script.rs`. Only files with one of these
/// extensions are "invocable" and therefore in scope for the S1.1
/// integrity manifest; anything else in `scripts/` (data files, READMEs,
/// helper modules never named as the top-level `script` param) is out of
/// scope.
const SCRIPT_EXTENSIONS: &[&str] = &["py", "sh", "js"];

/// S1.1: check the `[[scripts]]` integrity manifest against `scripts_dir`.
///
/// Two independent failure modes, deliberately handled differently (see
/// docs/RFC_CODE_EXECUTION_TRUST.md):
/// - **Declared but wrong** (a listed script is missing from disk, or its
///   content no longer matches the declared sha256) is evidence of
///   tampering or a stale manifest — always a hard error, in every
///   `SecurityProfile`.
/// - **Not declared at all** (the skill has no `[[scripts]]` entries, or
///   `scripts/` has invocable files the manifest doesn't list) means the
///   skill simply hasn't adopted integrity checking yet — gated by
///   `profile` (S1.3): silent in `Dev`, a warning in `Local`, a hard
///   error in `Production`.
fn validate_script_integrity(
  manifest: &SkillManifest,
  scripts_dir: &Path,
  profile: SecurityProfile,
  warnings: &mut Vec<String>,
) -> Result<(), SkillError> {
  let mut declared: HashMap<String, String> = HashMap::new();
  for entry in &manifest.scripts {
    if entry.name.is_empty()
      || entry.name.contains('/')
      || entry.name.contains('\\')
      || entry.name.contains("..")
    {
      return Err(SkillError::ValidationError {
        message: format!(
          "[[scripts]] entry name '{}' must be a plain filename inside scripts/",
          entry.name
        ),
      });
    }
    if declared
      .insert(entry.name.clone(), entry.sha256.to_lowercase())
      .is_some()
    {
      return Err(SkillError::ValidationError {
        message: format!("[[scripts]] declares '{}' more than once", entry.name),
      });
    }
  }

  // Declared but wrong: always a hard error, regardless of profile.
  for (name, expected_sha256) in &declared {
    let path = scripts_dir.join(name);
    let bytes = std::fs::read(&path).map_err(|_| SkillError::ValidationError {
      message: format!(
        "[[scripts]] declares '{}' but it is missing from {}",
        name,
        scripts_dir.display()
      ),
    })?;
    let actual_sha256 = sha256_hex(&bytes);
    if &actual_sha256 != expected_sha256 {
      return Err(SkillError::ValidationError {
        message: format!(
          "script '{}' content does not match its declared sha256 (expected {}, found {}) \
           — it may have been modified after install",
          name, expected_sha256, actual_sha256
        ),
      });
    }
  }

  // Not declared: profile-gated.
  let mut undeclared: Vec<String> = Vec::new();
  if let Ok(entries) = std::fs::read_dir(scripts_dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      if !path.is_file() {
        continue;
      }
      let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
      if !SCRIPT_EXTENSIONS.contains(&ext) {
        continue;
      }
      let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        continue;
      };
      if !declared.contains_key(&name) {
        undeclared.push(name);
      }
    }
  }
  undeclared.sort();

  if !undeclared.is_empty() {
    let message = if manifest.scripts.is_empty() {
      format!(
        "Tool 'script' declared but the skill has no [[scripts]] integrity manifest \
         ({} script file(s) will run without hash verification: {})",
        undeclared.len(),
        undeclared.join(", ")
      )
    } else {
      format!(
        "scripts/ contains file(s) not listed in [[scripts]] — they will run without \
         hash verification: {}",
        undeclared.join(", ")
      )
    };
    match profile {
      SecurityProfile::Dev => {}
      SecurityProfile::Local => warnings.push(message),
      SecurityProfile::Production => return Err(SkillError::ValidationError { message }),
    }
  }

  Ok(())
}

/// Lowercase hex-encoded SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
  let mut hasher = Sha256::new();
  hasher.update(bytes);
  format!("{:x}", hasher.finalize())
}

/// Resolve a knowledge path (possibly a glob) relative to `skill_dir`.
/// Returns all matching absolute paths.
///
/// Q1.10.2: any path that escapes `skill_dir` is silently dropped from
/// the result. The previous behaviour resolved `../../etc/passwd` and
/// happily returned the matched file, letting a malicious marketplace
/// skill scrape arbitrary host files into the persona prompt context.
/// We canonicalize the skill root once and reject every match whose
/// canonical form doesn't `starts_with` it. `..` components in the
/// raw pattern are also rejected up front so glob expansion doesn't
/// get a chance to materialize symlink-followed escapes.
pub fn resolve_knowledge_path(pattern: &str, skill_dir: &Path) -> Vec<PathBuf> {
  // Hard reject `..` in the configured pattern — any legitimate
  // knowledge entry should be inside the skill directory.
  if pattern
    .split(['/', '\\'])
    .any(|component| component == "..")
  {
    return Vec::new();
  }

  let base = if Path::new(pattern).is_absolute() {
    pattern.to_string()
  } else {
    skill_dir.join(pattern).to_string_lossy().into_owned()
  };

  let skill_root = skill_dir
    .canonicalize()
    .unwrap_or_else(|_| skill_dir.to_path_buf());

  let matches: Vec<PathBuf> = match glob::glob(&base) {
    Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
    Err(_) => {
      let p = PathBuf::from(&base);
      if p.exists() { vec![p] } else { vec![] }
    }
  };

  matches
    .into_iter()
    .filter(|p| {
      let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
      canonical.starts_with(&skill_root)
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use std::io::Write;
  use tempfile::TempDir;

  // ── helpers ───────────────────────────────────────────────────────────────

  fn write_toml(dir: &Path, content: &str) {
    let mut f = fs::File::create(dir.join(MANIFEST_FILE)).expect("create skill.toml");
    f.write_all(content.as_bytes()).expect("write skill.toml");
  }

  fn write_skill_md(dir: &Path, content: &str) {
    let mut f = fs::File::create(dir.join(SKILL_MD_FILE)).expect("create SKILL.md");
    f.write_all(content.as_bytes()).expect("write SKILL.md");
  }

  fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent).expect("create dirs");
    }
    let mut f = fs::File::create(path).expect("create file");
    f.write_all(content.as_bytes()).expect("write file");
  }

  const MINIMAL_TOML: &str = r#"
[skill]
name = "test"
version = "0.1"
description = "test skill"

[persona]
role = "You are a helpful assistant."
"#;

  // ── skill.toml tests ──────────────────────────────────────────────────────

  #[test]
  fn loads_minimal_toml_manifest() {
    let dir = TempDir::new().unwrap();
    write_toml(dir.path(), MINIMAL_TOML);
    let m = SkillLoader::load(dir.path()).unwrap();
    assert_eq!(m.skill.name, "test");
    assert!(m.tools.is_empty());
    assert!(m.knowledge.is_empty());
    assert!(m.memory.is_none());
  }

  #[test]
  fn toml_preferred_over_skill_md_when_both_present() {
    let dir = TempDir::new().unwrap();
    write_toml(dir.path(), MINIMAL_TOML);
    write_skill_md(
      dir.path(),
      "---\nname: md-skill\ndescription: From SKILL.md.\n---\nBody.\n",
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    // skill.toml wins
    assert_eq!(m.skill.name, "test");
  }

  #[test]
  fn rejects_unknown_tool() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "bad"
version = "0.1"
description = "bad skill"

[persona]
role = "test"

[[tools]]
name = "laser_cannon"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::UnknownTool { .. })));
  }

  #[test]
  fn rejects_missing_persona_role() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "no-persona"
version = "0.1"
description = "test"

[persona]
role = "   "
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  #[test]
  fn warns_on_empty_description() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "sparse"
version = "0.1"
description = ""

[persona]
role = "test"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let warnings = SkillLoader::validate(&m, dir.path()).unwrap();
    assert!(warnings.iter().any(|w| w.contains("description")));
  }

  // ── knowledge tests ───────────────────────────────────────────────────────

  #[test]
  fn validates_existing_knowledge_file() {
    let dir = TempDir::new().unwrap();
    let kb_path = dir.path().join("knowledge").join("guide.md");
    write_file(&kb_path, "# Guide");
    write_toml(
      dir.path(),
      r#"
[skill]
name = "knows"
version = "0.1"
description = "has knowledge"

[persona]
role = "expert"

[[knowledge]]
path = "./knowledge/guide.md"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let warnings = SkillLoader::validate(&m, dir.path()).unwrap();
    assert!(warnings.is_empty());
  }

  #[test]
  fn rejects_missing_knowledge_file() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "broken"
version = "0.1"
description = "missing knowledge"

[persona]
role = "expert"

[[knowledge]]
path = "./knowledge/missing.md"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(
      result,
      Err(SkillError::KnowledgeFileNotFound { .. })
    ));
  }

  #[test]
  fn knowledge_glob_matches_multiple_files() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("knowledge").join("a.md"), "A");
    write_file(&dir.path().join("knowledge").join("b.md"), "B");
    let paths = resolve_knowledge_path("./knowledge/*.md", dir.path());
    assert_eq!(paths.len(), 2);
  }

  /// Q1.10.2 regression: a manifest that references a relative path
  /// with `..` components must not be allowed to read files outside
  /// the skill's own directory.
  #[test]
  fn knowledge_path_with_parent_dir_traversal_is_rejected() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("inside.md"), "ok");

    // Place a "victim" file as a sibling of the skill dir.
    let parent = dir.path().parent().unwrap();
    let victim = parent.join("victim.md");
    fs::write(&victim, "secret").unwrap();

    let paths = resolve_knowledge_path("../victim.md", dir.path());
    assert!(paths.is_empty(), "traversal escape resolved to {paths:?}");

    // Clean up — the temp dir's drop won't reach this sibling.
    let _ = fs::remove_file(&victim);
  }

  /// Q1.10.2 regression: an absolute path that doesn't live under the
  /// skill directory is dropped even though it exists. Pre-fix the
  /// loader would happily fold `/etc/hosts` into the agent persona.
  #[test]
  fn knowledge_absolute_path_outside_skill_dir_is_rejected() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("inside.md"), "ok");

    let outside_dir = TempDir::new().unwrap();
    let outside = outside_dir.path().join("outside.md");
    fs::write(&outside, "leak me").unwrap();

    let paths = resolve_knowledge_path(outside.to_str().unwrap(), dir.path());
    assert!(
      paths.is_empty(),
      "absolute outside-skill-dir path resolved to {paths:?}"
    );
  }

  // ── script tool tests ─────────────────────────────────────────────────────

  #[test]
  fn validates_script_tool_with_scripts_dir() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("scripts")).unwrap();
    write_file(
      &dir.path().join("scripts").join("run.sh"),
      "#!/bin/bash\necho ok",
    );
    // S1.1: declaring the script in [[scripts]] with its correct hash is
    // the fully-adopted, zero-warning path.
    write_toml(
      dir.path(),
      r#"
[skill]
name = "scripter"
version = "0.1"
description = "has scripts"

[persona]
role = "expert"

[[tools]]
name = "script"

[[scripts]]
name = "run.sh"
sha256 = "1a51f79939e75f9c3891c2000ca479781486d2c04dd3c39db2f05c4ecfe01b54"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let warnings = SkillLoader::validate(&m, dir.path()).unwrap();
    assert!(warnings.is_empty());
  }

  /// S1.1: a `script` tool with no `[[scripts]]` integrity manifest at all
  /// is the pre-S1.1 back-compat case — `Local` (the ambient default when
  /// no profile is set) warns rather than rejecting.
  #[test]
  fn script_tool_without_integrity_manifest_warns_under_local_profile() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("scripts")).unwrap();
    write_file(
      &dir.path().join("scripts").join("run.sh"),
      "#!/bin/bash\necho ok",
    );
    write_toml(
      dir.path(),
      r#"
[skill]
name = "scripter-no-manifest"
version = "0.1"
description = "has scripts, no integrity manifest"

[persona]
role = "expert"

[[tools]]
name = "script"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();

    let warnings =
      SkillLoader::validate_with_profile(&m, dir.path(), SecurityProfile::Local).unwrap();
    assert!(
      warnings.iter().any(|w| w.contains("run.sh")),
      "expected a warning naming the unverified script, got {warnings:?}"
    );

    // Dev is the fast-iteration profile: no [[scripts]] manifest, no noise.
    let dev_warnings =
      SkillLoader::validate_with_profile(&m, dir.path(), SecurityProfile::Dev).unwrap();
    assert!(dev_warnings.is_empty());

    // Production fails closed: an unverified script tool must not load.
    let result = SkillLoader::validate_with_profile(&m, dir.path(), SecurityProfile::Production);
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  /// S1.1 regression: a declared script whose on-disk content no longer
  /// matches its manifest sha256 is tampering/staleness evidence and must
  /// be rejected at load time in every profile, not just Production.
  #[test]
  fn tampered_declared_script_is_rejected_in_every_profile() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("scripts")).unwrap();
    write_file(
      &dir.path().join("scripts").join("run.sh"),
      "#!/bin/bash\necho tampered-after-install",
    );
    write_toml(
      dir.path(),
      r#"
[skill]
name = "scripter-tampered"
version = "0.1"
description = "hash no longer matches"

[persona]
role = "expert"

[[tools]]
name = "script"

[[scripts]]
name = "run.sh"
sha256 = "1a51f79939e75f9c3891c2000ca479781486d2c04dd3c39db2f05c4ecfe01b54"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();

    for profile in [
      SecurityProfile::Dev,
      SecurityProfile::Local,
      SecurityProfile::Production,
    ] {
      let result = SkillLoader::validate_with_profile(&m, dir.path(), profile);
      assert!(
        matches!(result, Err(SkillError::ValidationError { .. })),
        "profile {profile:?} must reject a tampered declared script"
      );
    }
  }

  #[test]
  fn rejects_script_tool_without_scripts_dir() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "no-scripts"
version = "0.1"
description = "missing scripts dir"

[persona]
role = "expert"

[[tools]]
name = "script"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  #[test]
  fn validates_mcp_security_allowlists() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "mcp-secure"
version = "0.1"
description = "mcp"

[persona]
role = "expert"

[security]
mcp_server_allowlist = ["fixture"]
mcp_command_allowlist = ["python3"]
mcp_env_allowlist = ["FIXTURE_TOKEN"]

[[mcp_servers]]
name = "fixture"
command = "python3"
args = ["server.py"]
env = { FIXTURE_TOKEN = "secret" }
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let warnings = SkillLoader::validate(&m, dir.path()).unwrap();
    assert!(warnings.is_empty());
  }

  #[test]
  fn rejects_mcp_server_outside_allowlist() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "mcp-blocked"
version = "0.1"
description = "mcp"

[persona]
role = "expert"

[security]
mcp_server_allowlist = ["approved"]
mcp_command_allowlist = ["python3"]

[[mcp_servers]]
name = "blocked"
command = "python3"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  #[test]
  fn rejects_mcp_env_without_env_allowlist() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "mcp-env"
version = "0.1"
description = "mcp"

[persona]
role = "expert"

[[mcp_servers]]
name = "fixture"
command = "python3"
env = { API_KEY = "secret" }
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  // ── SKILL.md loading tests ────────────────────────────────────────────────

  #[test]
  fn loads_skill_md_when_no_toml() {
    let dir = TempDir::new().unwrap();
    write_skill_md(
      dir.path(),
      "---\nname: my-skill\ndescription: A test skill loaded from SKILL.md.\n---\n\nInstructions here.\n",
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    assert_eq!(m.skill.name, "my-skill");
    assert!(m.persona.role.contains("Instructions here."));
  }

  #[test]
  fn skill_md_with_allowed_tools_and_scripts_dir_validates() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("scripts")).unwrap();
    write_file(&dir.path().join("scripts").join("run.py"), "print('ok')");
    write_skill_md(
      dir.path(),
      "---\nname: scripted\ndescription: Has a script tool.\nallowed-tools: script\n---\n\nUse the script tool to run things.\n",
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    assert_eq!(m.tools.len(), 1);
    assert_eq!(m.tools[0].name, "script");
    // S1.1: SKILL.md has no `[[scripts]]`-equivalent frontmatter syntax, so
    // this always lands in the "no integrity manifest" back-compat path —
    // still a successful (non-fatal) load under the default Local profile,
    // but with a warning naming the unverified script.
    let warnings = SkillLoader::validate(&m, dir.path()).unwrap();
    assert!(
      warnings.iter().any(|w| w.contains("run.py")),
      "expected a warning naming the unverified script, got {warnings:?}"
    );
  }

  #[test]
  fn skill_md_with_script_tool_but_no_scripts_dir_fails_validation() {
    let dir = TempDir::new().unwrap();
    write_skill_md(
      dir.path(),
      "---\nname: broken\ndescription: Declares script tool without scripts dir.\nallowed-tools: script\n---\n\nBody.\n",
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  // ── dependencies (S2.1) tests ───────────────────────────────────────────

  #[test]
  fn pinned_and_hashed_requirements_validate_cleanly() {
    let dir = TempDir::new().unwrap();
    write_file(
      &dir.path().join("requirements.txt"),
      "requests==2.31.0 --hash=sha256:1111111111111111111111111111111111111111111111111111111111111111\n",
    );
    write_toml(
      dir.path(),
      r#"
[skill]
name = "deps-ok"
version = "0.1"
description = "has pinned deps"

[persona]
role = "expert"

[dependencies]
python = "requirements.txt"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let warnings = SkillLoader::validate(&m, dir.path()).unwrap();
    assert!(warnings.is_empty());
  }

  /// Pip's own line-continuation style (hash on its own continued line)
  /// must be accepted, not misread as two separate broken requirements.
  #[test]
  fn requirements_with_line_continuation_validate_cleanly() {
    let dir = TempDir::new().unwrap();
    write_file(
      &dir.path().join("requirements.txt"),
      "requests==2.31.0 \\\n    --hash=sha256:1111111111111111111111111111111111111111111111111111111111111111\n",
    );
    write_toml(
      dir.path(),
      r#"
[skill]
name = "deps-continuation"
version = "0.1"
description = "continuation line"

[persona]
role = "expert"

[dependencies]
python = "requirements.txt"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let warnings = SkillLoader::validate(&m, dir.path()).unwrap();
    assert!(warnings.is_empty());
  }

  #[test]
  fn unpinned_requirement_is_rejected() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("requirements.txt"), "requests\n");
    write_toml(
      dir.path(),
      r#"
[skill]
name = "deps-unpinned"
version = "0.1"
description = "no pin"

[persona]
role = "expert"

[dependencies]
python = "requirements.txt"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  #[test]
  fn pinned_but_unhashed_requirement_is_rejected() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("requirements.txt"), "requests==2.31.0\n");
    write_toml(
      dir.path(),
      r#"
[skill]
name = "deps-unhashed"
version = "0.1"
description = "no hash"

[persona]
role = "expert"

[dependencies]
python = "requirements.txt"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  #[test]
  fn missing_requirements_file_is_rejected() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "deps-missing-file"
version = "0.1"
description = "no such file"

[persona]
role = "expert"

[dependencies]
python = "requirements.txt"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  #[test]
  fn requirements_path_traversal_is_rejected() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "deps-traversal"
version = "0.1"
description = "escapes skill dir"

[persona]
role = "expert"

[dependencies]
python = "../../etc/requirements.txt"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }

  // ── fallback / not-found tests ────────────────────────────────────────────

  #[test]
  fn returns_manifest_not_found_when_neither_file_exists() {
    let dir = TempDir::new().unwrap();
    let result = SkillLoader::load(dir.path());
    assert!(matches!(result, Err(SkillError::ManifestNotFound { .. })));
  }

  // ── memory validation ─────────────────────────────────────────────────────

  #[test]
  fn rejects_unknown_memory_type() {
    let dir = TempDir::new().unwrap();
    write_toml(
      dir.path(),
      r#"
[skill]
name = "memtest"
version = "0.1"
description = "test"

[persona]
role = "agent"

[memory]
type = "redis"
"#,
    );
    let m = SkillLoader::load(dir.path()).unwrap();
    let result = SkillLoader::validate(&m, dir.path());
    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }
}
