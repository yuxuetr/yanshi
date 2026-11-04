use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::info;
use yanshi_agents::react::{ReActAgent, ReActConfig};
use yanshi_memory::{
  MemoryStore, PreferenceScope, PreferenceStore, ProjectMemoryStore, RememberPreferenceTool,
  SemanticMemory, SessionMemory, SqliteMemory, SqlitePreferenceStore, SqliteProjectMemoryStore,
  project_key_for_path,
};
use yanshi_rag::embeddings::OpenAIEmbedding;
use yanshi_rag::{Bm25KnowledgeBackend, RagSearchTool};
use yanshi_tools::builtin::{CodeExecTool, FileTool, HttpTool, ScriptTool, ShellTool};
use yanshi_tools::{Capability, SandboxPolicy, Tool, ToolPolicy, ToolRegistry};
use yanshi_tools_ai::{
  AsrTool, Image2ImageTool, ImageEditTool, ImageUnderstandTool, Text2ImageTool, Text2VideoTool,
  TtsTool,
};

use crate::{
  error::SkillError,
  loader::resolve_knowledge_path,
  manifest::{
    DependenciesConfig, KnowledgeBackendKind, KnowledgeConfig, McpServerConfig, MemoryConfig,
    PreferenceMemoryConfig, ProjectMemoryConfig, ScriptIntegrityEntry, SecurityConfig,
    SkillManifest, ToolConfig,
  },
  mcp_tools::{McpClientPool, McpToolAdapter},
};

/// U2.2 deliberately does not introduce a multi-user identity concept
/// (no existing hook point for one anywhere in `yanshi-skills`/
/// `yanshi-agents`/`yanshi-harness`) — every preference write and
/// read uses this fixed local scope, matching the "single-tenant
/// local-dev" default used elsewhere in the workspace (e.g.
/// `TenantId::default_for_local()`).
fn default_preference_scope() -> PreferenceScope {
  PreferenceScope::local("default")
}

/// Assembles a [`ReActAgent`] from a loaded [`SkillManifest`].
///
/// `skill_dir` is the loaded skill directory; it is used as the base for
/// resolving relative knowledge file paths and the default SQLite db path.
pub struct SkillBuilder;

impl SkillBuilder {
  /// Build a [`ReActAgent`] ready to run.
  pub async fn build(manifest: &SkillManifest, skill_dir: &Path) -> Result<ReActAgent, SkillError> {
    Self::build_with_extra_tools(manifest, skill_dir, Vec::new()).await
  }

  /// Build a [`ReActAgent`] and inject additional tools into its registry
  /// before construction.
  ///
  /// Used by multi-agent supervisors (e.g. `HandoffSupervisor`,
  /// `BlackboardSupervisor`) to give every participant access to a shared
  /// coordination tool — the supervisor builds those tools once and passes
  /// them in here.
  pub async fn build_with_extra_tools(
    manifest: &SkillManifest,
    skill_dir: &Path,
    extra_tools: Vec<Arc<dyn Tool>>,
  ) -> Result<ReActAgent, SkillError> {
    Self::build_core(manifest, skill_dir, extra_tools, None, None).await
  }

  /// Same as [`Self::build`], but also wires up `[memory.project]` (L3.1)
  /// against `project_root` when both are present — durable, per-project
  /// facts observed from `shell`/`script`/`code_exec` tool calls, injected
  /// into the persona on every subsequent turn (fully automatic; no tool
  /// call involved, unlike the preference layer).
  ///
  /// `project_root` is `None` for every `SkillBuilder` call site except
  /// `yanshi harness run`/`chat` (which share one `build_agent` helper):
  /// those are the only callers with a real "what directory is this agent
  /// working in" concept today. Multi-agent participants, the eval harness,
  /// and DAG `agent` nodes have no such concept, so `[memory.project]` is a
  /// no-op for them even if a skill declares it — same graceful-no-op
  /// pattern as `[memory.preference]` being absent.
  pub async fn build_with_project_root(
    manifest: &SkillManifest,
    skill_dir: &Path,
    project_root: Option<&Path>,
  ) -> Result<ReActAgent, SkillError> {
    Self::build_core(manifest, skill_dir, Vec::new(), project_root, None).await
  }

  /// Shared assembly path for every `build*` entry point. `admit`, when
  /// present, is applied to every tool (including the auto-registered
  /// `remember_preference` tool) before it's added to the registry —
  /// `None` means "admit everything," matching every caller except
  /// [`Self::build_with_admission`]. Threading admission through here
  /// (W5.4) replaces what used to be a second, hand-duplicated copy of
  /// this whole function in `build_with_admission` that had drifted out
  /// of sync with this one (it never wired up `[memory.project]`).
  async fn build_core(
    manifest: &SkillManifest,
    skill_dir: &Path,
    extra_tools: Vec<Arc<dyn Tool>>,
    project_root: Option<&Path>,
    admit: Option<&(dyn Fn(&str) -> bool + Send + Sync)>,
  ) -> Result<ReActAgent, SkillError> {
    info!(
        skill = %manifest.skill.name,
        version = %manifest.skill.version,
        extra_tool_count = extra_tools.len(),
        admission_filtered = admit.is_some(),
        "Building agent from skill manifest"
    );

    // 1. Build persona string (role + optional knowledge context)
    let persona = build_persona(manifest, skill_dir)?;

    // 2. Assemble ReActConfig
    let config = ReActConfig::new(manifest.model.resolved_model())
      .with_persona(persona)
      .with_max_iterations(manifest.model.resolved_max_iterations())
      .with_budget_tokens(manifest.model.resolved_budget_tokens());

    // 3. Build ToolRegistry, then inject extras (last write wins on name
    // collision so callers can override defaults if desired), then apply
    // the admission filter if one was supplied.
    let mut registry = Self::build_registry(manifest, skill_dir).await?;
    for tool in extra_tools {
      registry.register(tool);
    }
    if let Some(admit) = admit {
      let mut filtered = ToolRegistry::new();
      for tool in registry.list() {
        if admit(tool.name()) {
          filtered.register(tool);
        }
      }
      registry = filtered;
    }

    // 4. Build MemoryStore
    let memory = build_memory(manifest.memory.as_ref(), &manifest.skill.name).await?;

    // 5. U2.2: preference layer (optional). Registers `remember_preference`
    // into the registry (the write path, subject to the same admission
    // filter as every other tool) and attaches the same store to the
    // agent for prompt injection (the read path) — see
    // `default_preference_scope`'s doc comment for why there's no
    // per-user scoping yet.
    let preference_config = manifest.memory.as_ref().and_then(|m| m.preference.as_ref());
    let preference_store = build_preference_store(preference_config, &manifest.skill.name).await?;
    if let Some(store) = &preference_store
      && admit.is_none_or(|admit| admit(RememberPreferenceTool::TOOL_NAME))
    {
      registry.register(Arc::new(RememberPreferenceTool::new(
        store.clone(),
        default_preference_scope(),
      )));
    }

    // 6. L3.1/V1.6: project memory layer (optional, requires project_root).
    // Fully automatic on both ends — no tool to register, unlike
    // preference above.
    let project_config = manifest.memory.as_ref().and_then(|m| m.project.as_ref());
    let project_memory =
      build_project_memory_store(project_config, &manifest.skill.name, project_root).await?;

    let agent = ReActAgent::new(config, memory, Arc::new(registry));
    let agent = match preference_store {
      Some(store) => agent.with_preference_store(store, default_preference_scope()),
      None => agent,
    };
    let agent = match project_memory {
      Some((store, project_key)) => agent.with_project_memory(store, project_key),
      None => agent,
    };
    Ok(agent)
  }

  /// Build the tool registry for a skill without constructing an agent.
  ///
  /// This is useful for validation, CLI tool listing, and integration tests that
  /// need to exercise built-in and MCP tools directly.
  pub async fn build_registry(
    manifest: &SkillManifest,
    skill_dir: &Path,
  ) -> Result<ToolRegistry, SkillError> {
    let mut registry = build_tool_registry(
      &manifest.tools,
      &manifest.security,
      &manifest.scripts,
      &manifest.dependencies,
      skill_dir,
    )?;
    register_mcp_tools(&mut registry, manifest, skill_dir).await?;
    register_knowledge_backends(&mut registry, manifest, skill_dir).await?;
    Ok(registry)
  }

  /// Same as [`Self::build`], but each tool produced by the skill manifest
  /// passes through `admit` before being registered with the agent. Returning
  /// `false` keeps the tool out of the agent's registry entirely — used by
  /// the eval harness (and any future operator-driven override) to enforce
  /// per-case `tools_allowed` / `tools_denied` admission.
  ///
  /// Naming the tool by its `Tool::name()` matches the wire-level name the
  /// agent uses when emitting `AgentStepKind::ToolCall { tool, .. }`, so the
  /// closure semantics line up with what `tool_called` / `tool_not_called`
  /// assertions check.
  pub async fn build_with_admission<F>(
    manifest: &SkillManifest,
    skill_dir: &Path,
    admit: F,
  ) -> Result<ReActAgent, SkillError>
  where
    F: Fn(&str) -> bool + Send + Sync,
  {
    Self::build_core(manifest, skill_dir, Vec::new(), None, Some(&admit)).await
  }
}

// ── Persona builder ──────────────────────────────────────────────────────────

pub(crate) fn build_persona(
  manifest: &SkillManifest,
  skill_dir: &Path,
) -> Result<String, SkillError> {
  let mut parts: Vec<String> = Vec::new();

  // Base role
  parts.push(manifest.persona.role.clone());

  // Language hint
  if let Some(lang) = &manifest.persona.language {
    parts.push(format!("\nPlease respond in: {}", lang));
  }

  // Knowledge files injected into context (`skill.toml` `[[knowledge]]` entries).
  // Only the `files` tier is inlined here; `rag`-tier entries are indexed and
  // exposed as a `rag_search` tool by `register_knowledge_backends` instead, so
  // their (potentially large) content never lands in the system prompt.
  let files_entries: Vec<&KnowledgeConfig> = manifest
    .knowledge
    .iter()
    .filter(|kc| kc.backend == KnowledgeBackendKind::Files)
    .collect();
  if !files_entries.is_empty() {
    parts.push("\n\n## Knowledge Context".to_string());
    for kc in files_entries {
      let paths = resolve_knowledge_path(&kc.path, skill_dir);
      for path in &paths {
        let label = kc.description.clone().unwrap_or_else(|| {
          path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| kc.path.clone())
        });
        let content = std::fs::read_to_string(path).map_err(|e| {
          SkillError::IoError(format!(
            "Cannot read knowledge file {}: {}",
            path.display(),
            e
          ))
        })?;
        parts.push(format!("\n### {}\n\n{}", label, content.trim()));
      }
    }
  }

  // references/ directory: Agent Skills standard — load all .md / .txt files.
  let references_dir = skill_dir.join("references");
  if references_dir.is_dir() {
    let mut ref_files: Vec<PathBuf> = std::fs::read_dir(&references_dir)
      .map(|rd| {
        rd.filter_map(|e| e.ok())
          .map(|e| e.path())
          .filter(|p| {
            p.is_file()
              && p
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| matches!(x, "md" | "txt"))
                .unwrap_or(false)
          })
          .collect()
      })
      .unwrap_or_default();
    ref_files.sort(); // deterministic ordering
    if !ref_files.is_empty() {
      parts.push("\n\n## Reference Documents".to_string());
      for path in &ref_files {
        let label = path
          .file_name()
          .map(|n| n.to_string_lossy().into_owned())
          .unwrap_or_else(|| path.display().to_string());
        let content = std::fs::read_to_string(path).map_err(|e| {
          SkillError::IoError(format!(
            "Cannot read reference file {}: {}",
            path.display(),
            e
          ))
        })?;
        parts.push(format!("\n### {}\n\n{}", label, content.trim()));
      }
    }
  }

  Ok(parts.join("\n"))
}

// ── ToolRegistry builder ────────────────────────────────────────────────────────

fn build_tool_registry(
  tool_configs: &[ToolConfig],
  security: &SecurityConfig,
  script_manifest: &[ScriptIntegrityEntry],
  dependencies: &DependenciesConfig,
  skill_dir: &Path,
) -> Result<ToolRegistry, SkillError> {
  // SkillSecurity is the first layer of the three-way capability merge.
  // We populate both the legacy permission-based ToolPolicy (for
  // backward-compatible audit decisions) and the new capability layer that
  // ToolRegistry::execute also enforces. Downstream policy / CLI layers
  // remain unset and stay permissive by default.
  let mut registry = ToolRegistry::new();
  if !security.tool_permission_allowlist.is_empty() {
    registry = registry
      .with_policy(ToolPolicy::allow_permissions(
        security.tool_permission_allowlist.clone(),
      ))
      .with_skill_capabilities(Capability::from_permissions(
        &security.tool_permission_allowlist,
      ));
  }

  if tool_configs.is_empty() {
    // No tools declared — return an empty registry.
    return Ok(registry);
  }

  // Merge all per-tool constraints into a single SandboxPolicy.
  // Each built-in tool only checks its relevant policy field, so merging is
  // safe *as long as no tool's own execution-boundary defaults populate a
  // field another tool also reads*. `script` is the exception: its default
  // interpreters/`scripts/` path are layered on separately below
  // (`finalize_script_policy`), never into this shared object — see
  // docs/RFC_CODE_EXECUTION_TRUST.md (S0.2).
  let policy = Arc::new(build_sandbox_policy(tool_configs, skill_dir));

  // P10.4.1: per-tool override of the manifest-level `os_sandbox`.
  // `tool_cfg.os_sandbox = Some(true|false)` wins; `None` falls back
  // to `security.os_sandbox`. Resolved per-iteration so a mixed-policy
  // skill (sandbox shell but not script, or vice versa) gets exactly
  // what the manifest declares.
  let manifest_os_sandbox = security.os_sandbox;
  for tool_cfg in tool_configs {
    let effective_os_sandbox = tool_cfg.os_sandbox.unwrap_or(manifest_os_sandbox);
    match tool_cfg.name.to_lowercase().as_str() {
      "shell" => {
        let mut tool = ShellTool::new(policy.clone());
        if effective_os_sandbox {
          tool = tool.with_os_sandbox();
        }
        registry.register(Arc::new(tool));
      }
      "file" => {
        // S0.2: `scripts/` is `script`'s execution boundary — `file` must
        // never be able to write (or read) there, even if it leaked in via
        // explicit config, since an LLM writing content that `script` will
        // later execute defeats the author-signed-only invariant regardless
        // of how the allow-list got populated.
        let file_policy = Arc::new(exclude_scripts_dir(&policy, skill_dir));
        registry.register(Arc::new(FileTool::new(file_policy)));
      }
      "http" => {
        let http_tool = HttpTool::new(policy.clone())
          .map_err(|err| SkillError::ToolBuildError(err.to_string()))?;
        registry.register(Arc::new(http_tool));
      }
      "script" => {
        let scripts_dir = skill_dir.join("scripts");
        // S0.2: layer script's own default interpreters + `scripts/` path on
        // top of the shared policy in a private copy — never write them back
        // into `policy`, which `shell`/`file` also read.
        let script_policy = Arc::new(finalize_script_policy(&policy, skill_dir));
        let mut tool = ScriptTool::new(scripts_dir, script_policy);
        if let Some(schema) = &tool_cfg.parameters {
          tool = tool.with_parameters_schema(schema.clone());
        }
        // S1.3: once the skill declares a `[[scripts]]` integrity manifest,
        // enforce it unconditionally at execution time (S1.2) — no profile
        // check needed here, since `SkillLoader::validate` already gated
        // whether an *undeclared* skill was allowed to load at all (S1.1).
        // An empty manifest leaves `ScriptTool` permissive (`None`), matching
        // historical behaviour for skills that haven't adopted S1 yet.
        if !script_manifest.is_empty() {
          let hashes: HashMap<String, String> = script_manifest
            .iter()
            .map(|entry| (entry.name.clone(), entry.sha256.to_lowercase()))
            .collect();
          tool = tool.with_script_hashes(hashes);
        }
        // S2.2/S2.3: a declared `[dependencies].python` gets its own
        // isolated venv, built (or reused, if already up to date)
        // offline from the skill's `vendor/` directory. This must
        // succeed — a skill that declared dependencies and can't get
        // them installed correctly must not silently fall back to
        // running against the global interpreter in any profile.
        if let Some(requirements_rel_path) = &dependencies.python {
          let python_bin = crate::python_env::ensure_python_venv(skill_dir, requirements_rel_path)?;
          tool = tool.with_python_interpreter(python_bin);
        }
        if effective_os_sandbox {
          tool = tool.with_os_sandbox();
        }
        registry.register(Arc::new(tool));
      }
      // S4.2: code_exec deliberately does not consult `policy`/
      // `effective_os_sandbox` — none of its constraints are
      // author-configurable in v1 (RFC_LLM_CODE_EXECUTION.md's "no sharing
      // at all" design: unlike `script`'s opt-in OS sandbox, code_exec's
      // strong isolation is mandatory and its own `ContainerBackend` is
      // never the shared `SandboxPolicy`-driven `default_backend()`).
      "code_exec" => {
        registry.register(Arc::new(CodeExecTool::new()));
      }
      // Modality Tool adapters (`yanshi-tools-ai`): stateless thin
      // wrappers over `yanshi_llm::Yanshi::*` dispatch functions —
      // no `SandboxPolicy`/OS-sandbox knobs apply to them, unlike the
      // local-resource tools above; a skill's model access is already
      // scoped by which models the model registry YAML makes available.
      "tts" => registry.register(Arc::new(TtsTool::new())),
      "asr" => registry.register(Arc::new(AsrTool::new())),
      "text_to_image" => registry.register(Arc::new(Text2ImageTool::new())),
      "image_to_image" => registry.register(Arc::new(Image2ImageTool::new())),
      "image_edit" => registry.register(Arc::new(ImageEditTool::new())),
      "image_understand" => registry.register(Arc::new(ImageUnderstandTool::new())),
      "text_to_video" => registry.register(Arc::new(Text2VideoTool::new())),
      other => {
        // Already validated by SkillLoader; log and skip unknown tools.
        tracing::warn!(tool = other, "Skipping unknown tool during registry build");
      }
    }
  }

  Ok(registry)
}

async fn register_mcp_tools(
  registry: &mut ToolRegistry,
  manifest: &SkillManifest,
  skill_dir: &Path,
) -> Result<(), SkillError> {
  validate_mcp_governance(manifest)?;

  let mut active_pools: Vec<Arc<McpClientPool>> = Vec::new();
  for mcp in &manifest.mcp_servers {
    if mcp.name.trim().is_empty() {
      return Err(SkillError::ValidationError {
        message: "MCP server name must not be empty".to_string(),
      });
    }
    if mcp.command.trim().is_empty() {
      return Err(SkillError::ValidationError {
        message: format!("MCP server '{}' command must not be empty", mcp.name),
      });
    }

    let resolved_mcp = resolve_mcp_server_config(mcp, skill_dir, &manifest.security);
    audit_mcp_server_config(&resolved_mcp);
    let pool = Arc::new(McpClientPool::new(resolved_mcp));
    let tools = match pool.list_tools().await {
      Ok(tools) => tools,
      Err(err) => {
        for active_pool in &active_pools {
          let _ = active_pool.disconnect().await;
        }
        return Err(err);
      }
    };
    for tool in tools {
      let adapter = McpToolAdapter::new(pool.clone(), tool);
      let tool_name = yanshi_tools::Tool::name(&adapter).to_string();
      if registry.get(&tool_name).is_some() {
        let _ = pool.disconnect().await;
        for active_pool in &active_pools {
          let _ = active_pool.disconnect().await;
        }
        return Err(SkillError::ValidationError {
          message: format!(
            "Duplicate tool name '{}' while registering MCP server '{}'",
            tool_name, mcp.name
          ),
        });
      }
      tracing::info!(
          server = %mcp.name,
          tool = %tool_name,
          "Registering MCP tool"
      );
      registry.register(Arc::new(adapter));
    }
    active_pools.push(pool);
  }

  Ok(())
}

fn validate_mcp_governance(manifest: &SkillManifest) -> Result<(), SkillError> {
  let security = &manifest.security;
  let max_servers = security.resolved_mcp_max_servers();
  if manifest.mcp_servers.len() > max_servers {
    return Err(SkillError::ValidationError {
      message: format!(
        "Skill declares {} MCP servers, exceeding security.mcp_max_servers={}",
        manifest.mcp_servers.len(),
        max_servers
      ),
    });
  }

  for config in &manifest.mcp_servers {
    if !security.mcp_server_allowlist.is_empty()
      && !security
        .mcp_server_allowlist
        .iter()
        .any(|name| name == &config.name)
    {
      return Err(SkillError::ValidationError {
        message: format!(
          "MCP server '{}' is not listed in security.mcp_server_allowlist",
          config.name
        ),
      });
    }

    let executable = executable_name(&config.command);
    if !security
      .resolved_mcp_command_allowlist()
      .iter()
      .any(|allowed| allowed == &executable)
    {
      return Err(SkillError::ValidationError {
        message: format!(
          "MCP server '{}' command '{}' is not listed in security.mcp_command_allowlist",
          config.name, executable
        ),
      });
    }

    if !security.mcp_env_allowlist.is_empty() {
      for key in config.env.keys() {
        if !security
          .mcp_env_allowlist
          .iter()
          .any(|allowed| allowed == key)
        {
          return Err(SkillError::ValidationError {
            message: format!(
              "MCP server '{}' env '{}' is not listed in security.mcp_env_allowlist",
              config.name, key
            ),
          });
        }
      }
    } else if !config.env.is_empty() {
      return Err(SkillError::ValidationError {
        message: format!(
          "MCP server '{}' declares env values but security.mcp_env_allowlist is empty",
          config.name
        ),
      });
    }
  }

  Ok(())
}

fn resolve_mcp_server_config(
  config: &McpServerConfig,
  skill_dir: &Path,
  security: &SecurityConfig,
) -> McpServerConfig {
  let mut resolved = config.clone();
  resolved.command = resolve_skill_relative_command_part(&resolved.command, skill_dir);
  resolved.args = resolved
    .args
    .iter()
    .map(|arg| resolve_skill_relative_command_part(arg, skill_dir))
    .collect();
  if resolved.timeout_secs.is_none() {
    resolved.timeout_secs = Some(security.resolved_mcp_default_timeout_secs());
  } else {
    resolved.timeout_secs = Some(resolved.resolved_timeout_secs());
  }
  if resolved.max_concurrent_calls.is_none() {
    resolved.max_concurrent_calls = Some(security.resolved_mcp_max_concurrent_calls());
  } else {
    resolved.max_concurrent_calls = Some(resolved.resolved_max_concurrent_calls());
  }
  resolved
}

fn audit_mcp_server_config(config: &McpServerConfig) {
  let mut env_keys: Vec<_> = config.env.keys().cloned().collect();
  env_keys.sort();
  tracing::info!(
    event = "mcp_server_config_audit",
    server = %config.name,
    command = %executable_name(&config.command),
    args_count = config.args.len(),
    env_keys = ?env_keys,
    timeout_secs = config.resolved_timeout_secs(),
    max_concurrent_calls = config.resolved_max_concurrent_calls(),
    "Audited MCP server config"
  );
}

fn executable_name(command: &str) -> String {
  Path::new(command)
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or(command)
    .to_string()
}

fn resolve_skill_relative_command_part(value: &str, skill_dir: &Path) -> String {
  if value.starts_with("./") || value.starts_with("../") {
    skill_dir.join(value).to_string_lossy().into_owned()
  } else {
    value.to_string()
  }
}

/// Index the `backend = "rag"` knowledge entries into an in-memory BM25 backend
/// and register a single `rag_search` tool over them (P-A4.2). The `files`-tier
/// entries are left for [`build_persona`] to inline; this is a no-op (and
/// registers no tool) when the skill declares no rag-tier knowledge.
///
/// All rag-tier files share one backend / one `rag_search` tool so the LLM sees
/// a single "search the knowledge base" affordance regardless of how many files
/// back it. By default each file is still indexed as one whole-file document
/// (id = path relative to the skill dir) — unchanged from pre-L4.1 behaviour.
/// A knowledge entry that sets `chunk_strategy` (L4.1) is instead split via
/// [`yanshi_rag::chunking::chunk_document_for_knowledge_backend`] into
/// smaller, independently-citable chunks (each carrying `start_line`/
/// `end_line` metadata), so a whole-file and a chunked entry can coexist in
/// the same skill without the LLM ever seeing the difference.
///
/// A rag-tier entry that sets `query_rewrite` (L4.3) additionally wraps the
/// backend in an `yanshi_rag::rewrite::MultiQueryKnowledgeBackend` — see
/// [`KnowledgeConfig::query_rewrite`] for the "first entry wins" semantics
/// when a skill has more than one rag-tier entry.
async fn register_knowledge_backends(
  registry: &mut ToolRegistry,
  manifest: &SkillManifest,
  skill_dir: &Path,
) -> Result<(), SkillError> {
  let mut documents: Vec<(
    String,
    String,
    HashMap<String, yanshi_rag::types::MetadataValue>,
  )> = Vec::new();
  let mut query_rewrite: Option<&str> = None;
  for kc in &manifest.knowledge {
    if kc.backend != KnowledgeBackendKind::Rag {
      continue;
    }
    if query_rewrite.is_none() {
      query_rewrite = kc.query_rewrite.as_deref();
    }
    for path in resolve_knowledge_path(&kc.path, skill_dir) {
      let content = std::fs::read_to_string(&path).map_err(|e| {
        SkillError::IoError(format!(
          "Cannot read knowledge file {}: {}",
          path.display(),
          e
        ))
      })?;
      let id = path
        .strip_prefix(skill_dir)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();

      match &kc.chunk_strategy {
        Some(strategy_name) => {
          let strategy = parse_chunk_strategy(strategy_name)?;
          let chunk_size = kc.chunk_size.unwrap_or(1000);
          let overlap = kc.chunk_overlap.unwrap_or(100);
          let chunked = if strategy == yanshi_rag::types::ChunkingStrategy::Semantic {
            let model = kc.resolved_embedding_model().to_string();
            let embedder = OpenAIEmbedding::builder(&model).build().map_err(|e| {
              SkillError::ValidationError {
                message: format!(
                  "Cannot initialise semantic chunking for knowledge file {} (model '{}'): {}. \
                   Make sure OPENAI_API_KEY is set.",
                  path.display(),
                  model,
                  e
                ),
              }
            })?;
            yanshi_rag::chunking::chunk_document_for_knowledge_backend_semantic(
              Arc::new(embedder),
              chunk_size,
              overlap,
              kc.resolved_chunk_similarity_threshold(),
              kc.resolved_chunk_min_segment_size(),
              kc.resolved_chunk_buffer_percentile(),
              &id,
              &content,
            )
            .await
            .map_err(|e| SkillError::ValidationError {
              message: format!("Failed to chunk knowledge file {}: {e}", path.display()),
            })?
          } else {
            yanshi_rag::chunking::chunk_document_for_knowledge_backend(
              strategy, chunk_size, overlap, &id, &content,
            )
            .map_err(|e| SkillError::ValidationError {
              message: format!("Failed to chunk knowledge file {}: {e}", path.display()),
            })?
          };
          documents.extend(chunked);
        }
        None => documents.push((id, content, HashMap::new())),
      }
    }
  }

  if documents.is_empty() {
    return Ok(());
  }

  info!(
    rag_document_count = documents.len(),
    "Indexing rag-tier knowledge into a rag_search tool"
  );
  let backend: Arc<dyn yanshi_rag::KnowledgeBackend> =
    Arc::new(Bm25KnowledgeBackend::from_chunked_documents(documents));
  let backend = match query_rewrite {
    Some(strategy_name) => {
      let rewriter = parse_query_rewriter(strategy_name)?;
      Arc::new(yanshi_rag::rewrite::MultiQueryKnowledgeBackend::new(
        backend, rewriter,
      )) as Arc<dyn yanshi_rag::KnowledgeBackend>
    }
    None => backend,
  };
  registry.register(Arc::new(RagSearchTool::new(backend)));
  Ok(())
}

/// Parse a `[[knowledge]].query_rewrite` string into a
/// `yanshi_rag::rewrite::QueryRewriter` (L4.3).
fn parse_query_rewriter(
  name: &str,
) -> Result<Arc<dyn yanshi_rag::rewrite::QueryRewriter>, SkillError> {
  match name {
    "split" => Ok(Arc::new(yanshi_rag::rewrite::SplitQueryRewriter)),
    other => Err(SkillError::ValidationError {
      message: format!("Unknown knowledge query_rewrite '{other}'; expected 'split'"),
    }),
  }
}

/// Parse a `[[knowledge]].chunk_strategy` string into the `yanshi-rag`
/// enum (L4.1). Kept as a small string→enum mapping here (rather than making
/// `KnowledgeConfig.chunk_strategy` itself an `yanshi_rag::ChunkingStrategy`)
/// so `yanshi-skills`' manifest types don't need to special-case a
/// `code-chunking`-feature-gated variant at the serde layer.
fn parse_chunk_strategy(name: &str) -> Result<yanshi_rag::types::ChunkingStrategy, SkillError> {
  use yanshi_rag::types::ChunkingStrategy;
  match name {
    "fixed_size" => Ok(ChunkingStrategy::FixedSize),
    "sentence" => Ok(ChunkingStrategy::Sentence),
    "recursive" => Ok(ChunkingStrategy::Recursive),
    "paragraph" => Ok(ChunkingStrategy::Paragraph),
    "heading" => Ok(ChunkingStrategy::Heading),
    "code_ast" => Ok(ChunkingStrategy::CodeAst),
    "semantic" => Ok(ChunkingStrategy::Semantic),
    other => Err(SkillError::ValidationError {
      message: format!(
        "Unknown knowledge chunk_strategy '{other}'; expected one of fixed_size, sentence, recursive, paragraph, heading, code_ast, semantic"
      ),
    }),
  }
}

/// Merge all tool constraints into a unified `SandboxPolicy`.
fn build_sandbox_policy(tool_configs: &[ToolConfig], skill_dir: &Path) -> SandboxPolicy {
  let mut allowed_commands: Vec<String> = Vec::new();
  let mut allowed_paths: Vec<PathBuf> = Vec::new();
  let mut allowed_domains: Vec<String> = Vec::new();
  let mut max_exec_time_secs: u64 = 30;

  for tc in tool_configs {
    // Shell commands
    if !tc.allowed_commands.is_empty() {
      allowed_commands.extend(tc.allowed_commands.iter().cloned());
    }
    // File paths
    for p in &tc.allowed_paths {
      let expanded = expand_tilde(p);
      let path = PathBuf::from(expanded);
      if path.is_relative() {
        allowed_paths.push(skill_dir.join(path));
      } else {
        allowed_paths.push(path);
      }
    }
    // HTTP domains
    allowed_domains.extend(tc.allowed_domains.iter().cloned());
    // Exec time (take the maximum across tools)
    if let Some(t) = tc.max_exec_time_secs {
      max_exec_time_secs = max_exec_time_secs.max(t);
    }
  }

  // Deduplicate
  allowed_commands.sort();
  allowed_commands.dedup();
  allowed_paths.sort();
  allowed_paths.dedup();
  allowed_domains.sort();
  allowed_domains.dedup();

  // If the skill declares a shell tool but leaves allowed_commands empty,
  // use the built-in safe default list rather than allowing everything.
  // NOTE: `script`'s own defaults (interpreters, `scripts/`) are deliberately
  // NOT injected here — this policy is shared with `file`/`shell`/`http`,
  // and populating it with script's execution-boundary defaults would leak
  // them into tools that consume LLM-generated content. See
  // `finalize_script_policy` + docs/RFC_CODE_EXECUTION_TRUST.md (S0.2).
  let has_shell = tool_configs
    .iter()
    .any(|t| t.name.to_lowercase() == "shell");
  if has_shell && allowed_commands.is_empty() {
    // Default safe command list from SandboxPolicy::default()
    allowed_commands = SandboxPolicy::default().allowed_commands;
  }

  SandboxPolicy {
    allowed_commands,
    allowed_paths,
    allowed_domains,
    max_exec_time_secs,
    ..SandboxPolicy::default()
  }
}

/// Layer `script`'s own defaults (interpreters + `scripts/` path) on top of
/// the shared base policy, in a private copy scoped to `ScriptTool` alone.
/// Never write these back into `base` — `file`/`shell` read that same
/// object and must not inherit script's execution-boundary privileges
/// (docs/RFC_CODE_EXECUTION_TRUST.md, S0.2).
fn finalize_script_policy(base: &SandboxPolicy, skill_dir: &Path) -> SandboxPolicy {
  let mut allowed_commands = base.allowed_commands.clone();
  if allowed_commands.is_empty() {
    allowed_commands = default_script_interpreters();
  } else {
    for interpreter in default_script_interpreters() {
      if !allowed_commands.contains(&interpreter) {
        allowed_commands.push(interpreter);
      }
    }
    allowed_commands.sort();
    allowed_commands.dedup();
  }

  let mut allowed_paths = base.allowed_paths.clone();
  if allowed_paths.is_empty() {
    allowed_paths.push(skill_dir.join("scripts"));
  }

  SandboxPolicy {
    allowed_commands,
    allowed_paths,
    ..base.clone()
  }
}

/// Exclude the skill's `scripts/` directory (and anything under it) from a
/// policy, unconditionally — regardless of whether it got there via
/// `script`'s implicit default or explicit tool config. `file` must never
/// read or write there: that directory is `script`'s execution boundary,
/// and an LLM-writable execution boundary defeats the author-signed-only
/// invariant no matter how the allow-list was populated
/// (docs/RFC_CODE_EXECUTION_TRUST.md, S0.2).
fn exclude_scripts_dir(base: &SandboxPolicy, skill_dir: &Path) -> SandboxPolicy {
  let scripts_dir = skill_dir.join("scripts");
  SandboxPolicy {
    allowed_paths: base
      .allowed_paths
      .iter()
      .filter(|p| **p != scripts_dir && !p.starts_with(&scripts_dir))
      .cloned()
      .collect(),
    ..base.clone()
  }
}

fn default_script_interpreters() -> Vec<String> {
  ["python3", "bash", "node"]
    .into_iter()
    .map(ToString::to_string)
    .collect()
}

// ── MemoryStore builder ──────────────────────────────────────────────────────

async fn build_memory(
  config: Option<&MemoryConfig>,
  skill_name: &str,
) -> Result<Box<dyn MemoryStore>, SkillError> {
  match config {
    None => Ok(Box::new(SessionMemory::default_window())),
    Some(mem) if mem.memory_type == "none" => {
      // Explicitly disabled — use in-memory session store.
      Ok(Box::new(SessionMemory::default_window()))
    }
    Some(mem) if mem.memory_type == "sqlite" => {
      let db_path = resolve_db_path(mem.db_path.as_deref(), skill_name);
      // Ensure parent directory exists.
      if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
          SkillError::IoError(format!(
            "Cannot create memory directory {}: {}",
            parent.display(),
            e
          ))
        })?;
      }
      let store = SqliteMemory::open(&db_path).await?;
      Ok(Box::new(store))
    }
    Some(mem) if mem.memory_type == "semantic" => {
      // Build the embedding provider from environment / manifest config
      let model = mem.resolved_embedding_model().to_string();
      let embedder =
        OpenAIEmbedding::builder(&model)
          .build()
          .map_err(|e| SkillError::ValidationError {
            message: format!(
              "Cannot initialise semantic memory (model '{}'): {}. \
                         Make sure OPENAI_API_KEY is set.",
              model, e
            ),
          })?;
      let db_path = resolve_db_path(mem.db_path.as_deref(), skill_name);
      if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
          SkillError::IoError(format!(
            "Cannot create memory directory {}: {}",
            parent.display(),
            e
          ))
        })?;
      }
      let window = mem.resolved_window_tokens();
      let store = SemanticMemory::open(&db_path, Arc::new(embedder), window).await?;
      Ok(Box::new(store))
    }
    _ => {
      // "session" or anything unrecognised — use in-memory.
      let window = config.map(|m| m.resolved_window_tokens()).unwrap_or(8_000);
      Ok(Box::new(SessionMemory::new(window)))
    }
  }
}

// ── PreferenceStore builder (U2.2) ───────────────────────────────────────────

/// `None` when `[memory.preference]` is absent or explicitly disabled —
/// callers treat that as "feature off," not an error.
async fn build_preference_store(
  config: Option<&PreferenceMemoryConfig>,
  skill_name: &str,
) -> Result<Option<Arc<dyn PreferenceStore>>, SkillError> {
  let Some(cfg) = config else {
    return Ok(None);
  };
  if !cfg.enabled {
    return Ok(None);
  }
  let db_path = resolve_preference_db_path(cfg.db_path.as_deref(), skill_name);
  if let Some(parent) = db_path.parent() {
    std::fs::create_dir_all(parent).map_err(|e| {
      SkillError::IoError(format!(
        "Cannot create preference memory directory {}: {}",
        parent.display(),
        e
      ))
    })?;
  }
  let store = SqlitePreferenceStore::open(&db_path)
    .await
    .map_err(|e| SkillError::IoError(format!("Cannot open preference store: {e}")))?;
  Ok(Some(Arc::new(store)))
}

// ── ProjectMemoryStore builder (L3.1/V1.6) ───────────────────────────────────

/// `None` when `[memory.project]` is absent, explicitly disabled, or
/// `project_root` wasn't supplied — the last case is the common one: most
/// `SkillBuilder` call sites have no project-root concept at all, so a
/// skill declaring `[memory.project]` is a no-op there rather than an
/// error (see `SkillBuilder::build_with_project_root`'s doc comment).
/// Returns the opened store paired with its derived `project_key` so the
/// caller can attach both to the agent via `ReActAgent::with_project_memory`.
async fn build_project_memory_store(
  config: Option<&ProjectMemoryConfig>,
  skill_name: &str,
  project_root: Option<&Path>,
) -> Result<Option<(Arc<dyn ProjectMemoryStore>, String)>, SkillError> {
  let Some(cfg) = config else {
    return Ok(None);
  };
  if !cfg.enabled {
    return Ok(None);
  }
  let Some(project_root) = project_root else {
    return Ok(None);
  };
  let db_path = resolve_project_db_path(cfg.db_path.as_deref(), skill_name);
  if let Some(parent) = db_path.parent() {
    std::fs::create_dir_all(parent).map_err(|e| {
      SkillError::IoError(format!(
        "Cannot create project memory directory {}: {}",
        parent.display(),
        e
      ))
    })?;
  }
  let store = SqliteProjectMemoryStore::open(&db_path)
    .await
    .map_err(|e| SkillError::IoError(format!("Cannot open project memory store: {e}")))?;
  let project_key = project_key_for_path(project_root);
  Ok(Some((Arc::new(store), project_key)))
}

/// Resolve
/// Resolve the SQLite db path, expanding `~` and supplying a default.
fn resolve_db_path(db_path: Option<&str>, skill_name: &str) -> PathBuf {
  match db_path {
    Some(p) => PathBuf::from(expand_tilde(p)),
    None => {
      let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
      home
        .join(".yanshi")
        .join("memory")
        .join(format!("{}.db", skill_name))
    }
  }
}

/// Same shape as [`resolve_db_path`], but defaults to a distinct
/// filename (`<skill>.preference.db`) so the preference store never
/// collides with the primary `MemoryConfig` store when both resolve to
/// the default directory.
fn resolve_preference_db_path(db_path: Option<&str>, skill_name: &str) -> PathBuf {
  match db_path {
    Some(p) => PathBuf::from(expand_tilde(p)),
    None => {
      let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
      home
        .join(".yanshi")
        .join("memory")
        .join(format!("{}.preference.db", skill_name))
    }
  }
}

/// Same shape as [`resolve_db_path`], but defaults to a distinct
/// filename (`<skill>.project.db`) so the project-memory store never
/// collides with the primary or preference `MemoryConfig` stores when all
/// three resolve to the default directory.
fn resolve_project_db_path(db_path: Option<&str>, skill_name: &str) -> PathBuf {
  match db_path {
    Some(p) => PathBuf::from(expand_tilde(p)),
    None => {
      let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
      home
        .join(".yanshi")
        .join("memory")
        .join(format!("{}.project.db", skill_name))
    }
  }
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &str) -> String {
  if (path.starts_with("~/") || path == "~")
    && let Some(home) = dirs::home_dir()
  {
    return path.replacen('~', &home.to_string_lossy(), 1);
  }
  path.to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    loader::SkillLoader,
    manifest::{ModelConfig, PersonaConfig, SecurityConfig, SkillInfo},
  };
  use std::fs;
  use std::io::Write;
  use tempfile::TempDir;

  // ── helpers ───────────────────────────────────────────────────────────────

  fn write_toml(dir: &Path, content: &str) {
    let mut f = fs::File::create(dir.join("skill.toml")).expect("create skill.toml");
    f.write_all(content.as_bytes()).expect("write");
  }

  fn write_file(path: &Path, content: &str) {
    if let Some(p) = path.parent() {
      fs::create_dir_all(p).expect("mkdir");
    }
    let mut f = fs::File::create(path).expect("create");
    f.write_all(content.as_bytes()).expect("write");
  }

  fn minimal_manifest(name: &str) -> SkillManifest {
    SkillManifest {
      skill: SkillInfo {
        name: name.to_string(),
        version: "0.1.0".to_string(),
        description: "test".to_string(),
      },
      persona: PersonaConfig {
        role: "You are a test agent.".to_string(),
        language: None,
      },
      model: ModelConfig::default(),
      security: SecurityConfig::default(),
      tools: vec![],
      mcp_servers: vec![],
      knowledge: vec![],
      memory: None,
      validation: Default::default(),
      scripts: vec![],
      dependencies: Default::default(),
      workflows: Vec::new(),
    }
  }

  // ── SkillBuilder::build() tests (no LLM call, safe to run in CI) ────────

  /// build() with no tools and session memory succeeds and returns an agent
  /// with a valid UUID session_id.
  #[tokio::test]
  async fn build_minimal_skill() {
    let dir = TempDir::new().unwrap();
    let manifest = minimal_manifest("minimal");
    let agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
    // session_id should be a non-empty UUID-like string
    assert!(!agent.session_id.is_empty());
    assert!(agent.session_id.contains('-'));
  }

  /// build() applies persona text to the agent config.
  #[tokio::test]
  async fn build_sets_persona_in_config() {
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("persona-test");
    manifest.persona.role = "You are a specialised Rust expert.".to_string();

    // Build two agents: same manifest, different session IDs expected.
    let a1 = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
    let a2 = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
    assert_ne!(
      a1.session_id, a2.session_id,
      "each build should get a fresh session"
    );
  }

  /// build() with shell + file tools registers both in the agent's registry.
  #[tokio::test]
  async fn build_registers_shell_and_file_tools() {
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("two-tools");
    manifest.tools = vec![
      ToolConfig {
        name: "shell".to_string(),
        ..ToolConfig::default()
      },
      ToolConfig {
        name: "file".to_string(),
        ..ToolConfig::default()
      },
    ];
    // Build must succeed (tools are registered, no LLM called).
    let _agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
  }

  #[tokio::test]
  async fn build_with_admission_admits_only_filter_approved_tools() {
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("admission-skill");
    manifest.tools = vec![
      ToolConfig {
        name: "shell".to_string(),
        ..ToolConfig::default()
      },
      ToolConfig {
        name: "file".to_string(),
        ..ToolConfig::default()
      },
    ];

    // Filter that keeps only `file`. The agent's registry should reflect
    // that — `shell` must be absent even though the manifest declared it.
    let agent = SkillBuilder::build_with_admission(&manifest, dir.path(), |name| name == "file")
      .await
      .unwrap();
    assert!(agent.tools().get("file").is_some());
    assert!(agent.tools().get("shell").is_none());
  }

  #[tokio::test]
  async fn build_with_admission_admits_every_tool_when_filter_always_true() {
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("admission-skill-2");
    manifest.tools = vec![
      ToolConfig {
        name: "shell".to_string(),
        ..ToolConfig::default()
      },
      ToolConfig {
        name: "file".to_string(),
        ..ToolConfig::default()
      },
    ];
    let agent = SkillBuilder::build_with_admission(&manifest, dir.path(), |_| true)
      .await
      .unwrap();
    assert!(agent.tools().get("file").is_some());
    assert!(agent.tools().get("shell").is_some());
  }

  #[tokio::test]
  async fn build_registry_enforces_security_tool_permission_allowlist() {
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("policy-skill");
    manifest.security.tool_permission_allowlist = vec![
      yanshi_tools::ToolPermission::FilesystemRead,
      yanshi_tools::ToolPermission::FilesystemWrite,
    ];
    manifest.tools = vec![
      ToolConfig {
        name: "file".to_string(),
        ..ToolConfig::default()
      },
      ToolConfig {
        name: "shell".to_string(),
        ..ToolConfig::default()
      },
    ];

    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();

    let file_decision = registry
      .evaluate_policy(
        "file",
        &serde_json::json!({"operation": "read", "path": "README.md"}),
      )
      .unwrap();
    assert!(file_decision.allowed);

    let shell = registry
      .execute("shell", serde_json::json!({"command": "echo ok"}))
      .await
      .unwrap_err();
    assert!(matches!(
      shell,
      yanshi_tools::ToolError::PolicyDenied { .. }
    ));
  }

  /// build() with script tool uses the scripts/ directory from skill_dir.
  #[tokio::test]
  async fn build_registers_script_tool() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("scripts")).unwrap();
    write_file(
      &dir.path().join("scripts").join("hello.sh"),
      "#!/bin/bash\necho hi",
    );

    let mut manifest = minimal_manifest("script-skill");
    manifest.tools = vec![ToolConfig {
      name: "script".to_string(),
      ..ToolConfig::default()
    }];
    let _agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
  }

  /// S4.2: a `[[tools]] name = "code_exec"` manifest entry registers a real,
  /// callable `CodeExecTool` — no `SandboxPolicy`/`os_sandbox` threading
  /// needed (unlike `script`), matching the builder's dedicated match arm.
  /// Skips the actual `execute()` call (with a clear message) on a host
  /// with no container engine, mirroring the skip-guard pattern already
  /// established for `yanshi-tools`' S3/S4.2 real-enforcement tests.
  #[tokio::test]
  async fn build_registers_code_exec_tool() {
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("code-exec-skill");
    manifest.tools = vec![ToolConfig {
      name: "code_exec".to_string(),
      ..ToolConfig::default()
    }];
    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    let tool = registry
      .get("code_exec")
      .expect("code_exec must be registered");
    assert_eq!(
      registry.tool_idempotency("code_exec", &serde_json::json!({})),
      Some(yanshi_tools::ToolIdempotency::NonIdempotent)
    );

    if tool
      .sandbox_status()
      .is_none_or(|status| status.enforcement != yanshi_tools::SandboxEnforcement::Enforcing)
    {
      eprintln!("skipping execute(): no container engine ('container' or 'podman') on PATH");
      return;
    }
    let result = registry
      .execute("code_exec", serde_json::json!({"code": "print(1 + 1)"}))
      .await
      .expect("tool call must complete");
    assert!(
      !result.is_error,
      "expected success, got: {}",
      result.content
    );
    assert_eq!(result.content, "2");
  }

  /// A `[[tools]]` manifest entry naming any of the seven modality Tool
  /// adapters from `yanshi-tools-ai` registers the matching stateless
  /// tool — no `SandboxPolicy`/`os_sandbox` threading applies to them
  /// (unlike the local-resource tools), matching the builder's dedicated
  /// match arms.
  #[tokio::test]
  async fn build_registers_modality_tools() {
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("modality-skill");
    manifest.tools = [
      "tts",
      "asr",
      "text_to_image",
      "image_to_image",
      "image_edit",
      "image_understand",
      "text_to_video",
    ]
    .into_iter()
    .map(|name| ToolConfig {
      name: name.to_string(),
      ..ToolConfig::default()
    })
    .collect();

    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();

    for name in [
      "tts",
      "asr",
      "text_to_image",
      "image_to_image",
      "image_edit",
      "image_understand",
      "text_to_video",
    ] {
      assert!(
        registry.get(name).is_some(),
        "expected \"{name}\" to be registered"
      );
      assert_eq!(
        registry.tool_idempotency(name, &serde_json::json!({})),
        Some(yanshi_tools::ToolIdempotency::NonIdempotent),
        "modality tools are billed, non-replayable-safe vendor calls"
      );
    }
  }

  /// build() with knowledge files injects content into the persona.
  #[tokio::test]
  async fn build_injects_knowledge_into_persona() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("knowledge").join("rules.md"), "# Rule 1");

    write_toml(
      dir.path(),
      r#"
[skill]
name = "knowledgeable"
version = "0.1"
description = "has knowledge"

[persona]
role = "You are an expert."

[[knowledge]]
path = "./knowledge/rules.md"
description = "Coding rules"
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();
    // build() should succeed; persona will contain injected knowledge content
    let _agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
  }

  /// build() with references/ directory injects ref content into persona.
  #[tokio::test]
  async fn build_injects_references_dir_into_persona() {
    let dir = TempDir::new().unwrap();
    write_file(
      &dir.path().join("references").join("api.md"),
      "# API Reference",
    );

    let manifest = minimal_manifest("with-refs");
    // build() should not fail even though knowledge list is empty;
    // references/ is picked up automatically by build_persona()
    let _agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
  }

  // ── P-A4.2: tiered knowledge resolution (files vs rag backend) ──────────────

  /// An omitted `backend` defaults to the files tier (preserves pre-P-A4.2
  /// behaviour so existing skills keep inlining their knowledge).
  #[test]
  fn knowledge_backend_defaults_to_files_when_omitted() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("k.md"), "x");
    write_toml(
      dir.path(),
      r#"
[skill]
name = "defaulting"
version = "0.1"
description = "d"

[persona]
role = "r"

[[knowledge]]
path = "./k.md"
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();
    assert_eq!(manifest.knowledge[0].backend, KnowledgeBackendKind::Files);
  }

  /// A `backend = "rag"` entry is indexed into a `rag_search` tool and is NOT
  /// inlined into the persona — the whole point of the rag tier.
  #[tokio::test]
  async fn rag_tier_knowledge_registers_tool_and_skips_persona() {
    let dir = TempDir::new().unwrap();
    write_file(
      &dir.path().join("knowledge").join("manual.md"),
      "The deployment runbook lives in the ops wiki under section seven.",
    );
    write_toml(
      dir.path(),
      r#"
[skill]
name = "rag-skill"
version = "0.1"
description = "rag-tier knowledge"

[persona]
role = "You are an ops assistant."

[[knowledge]]
path = "./knowledge/manual.md"
backend = "rag"
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();

    // Persona must NOT inline rag-tier content, and with no files-tier entries
    // there is no "Knowledge Context" header at all.
    let persona = build_persona(&manifest, dir.path()).unwrap();
    assert!(
      !persona.contains("deployment runbook"),
      "rag-tier content must not be inlined into the persona: {persona}"
    );
    assert!(
      !persona.contains("## Knowledge Context"),
      "no files-tier entries → no Knowledge Context header: {persona}"
    );

    // The registry must expose exactly one rag_search tool over the indexed file.
    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    assert!(
      registry.get("rag_search").is_some(),
      "rag-tier knowledge must register a rag_search tool"
    );
  }

  /// A files-tier entry (default) inlines into the persona and registers no
  /// `rag_search` tool.
  #[tokio::test]
  async fn files_tier_knowledge_inlines_and_registers_no_rag_tool() {
    let dir = TempDir::new().unwrap();
    write_file(
      &dir.path().join("knowledge").join("rules.md"),
      "Always write idempotent tools.",
    );
    write_toml(
      dir.path(),
      r#"
[skill]
name = "files-skill"
version = "0.1"
description = "files-tier knowledge"

[persona]
role = "You are an expert."

[[knowledge]]
path = "./knowledge/rules.md"
description = "Rules"
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();

    let persona = build_persona(&manifest, dir.path()).unwrap();
    assert!(
      persona.contains("Always write idempotent tools."),
      "files-tier knowledge must be inlined into the persona: {persona}"
    );

    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    assert!(
      registry.get("rag_search").is_none(),
      "files-tier knowledge must not register a rag_search tool"
    );
  }

  /// A skill mixing both tiers: files content inlines, rag content becomes a
  /// tool, and the two paths don't interfere.
  #[tokio::test]
  async fn mixed_tier_knowledge_routes_each_entry_independently() {
    let dir = TempDir::new().unwrap();
    write_file(
      &dir.path().join("k").join("inline.md"),
      "Inline house style guide.",
    );
    write_file(
      &dir.path().join("k").join("corpus.md"),
      "Large searchable reference corpus about widgets.",
    );
    write_toml(
      dir.path(),
      r#"
[skill]
name = "mixed-skill"
version = "0.1"
description = "both tiers"

[persona]
role = "You are a helper."

[[knowledge]]
path = "./k/inline.md"

[[knowledge]]
path = "./k/corpus.md"
backend = "rag"
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();

    let persona = build_persona(&manifest, dir.path()).unwrap();
    assert!(persona.contains("Inline house style guide."));
    assert!(
      !persona.contains("searchable reference corpus"),
      "rag-tier entry must stay out of the persona even when a files entry exists"
    );

    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    assert!(registry.get("rag_search").is_some());
  }

  /// L4.1: a rag-tier entry with `chunk_strategy` set narrows a search hit
  /// down to the matching paragraph instead of returning the whole file —
  /// the literal scenario the TODO calls out ("避免整文件索引丢失精度").
  #[tokio::test]
  async fn chunk_strategy_narrows_search_hits_to_the_matching_paragraph() {
    let dir = TempDir::new().unwrap();
    let unrelated_filler = "Filler paragraph about widgets. ".repeat(20);
    let content = format!(
      "Deployment overview.\n\n{unrelated_filler}\n\nRotate the signing key every ninety days per the security policy.\n\n{unrelated_filler}"
    );
    write_file(&dir.path().join("knowledge").join("manual.md"), &content);
    write_toml(
      dir.path(),
      r#"
[skill]
name = "chunked-rag-skill"
version = "0.1"
description = "rag-tier knowledge with paragraph chunking"

[persona]
role = "You are an ops assistant."

[[knowledge]]
path = "./knowledge/manual.md"
backend = "rag"
chunk_strategy = "paragraph"
chunk_size = 200
chunk_overlap = 0
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();
    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    let tool = registry.get("rag_search").expect("rag_search registered");

    let out = tool
      .execute(serde_json::json!({ "query": "rotate signing key ninety days" }))
      .await
      .expect("execute ok");
    assert!(!out.is_error);
    assert!(
      out.content.contains("signing key"),
      "top hit must contain the matching passage: {}",
      out.content
    );
    assert!(
      !out.content.contains("Filler paragraph"),
      "chunking must keep the unrelated filler paragraphs out of the matching chunk: {}",
      out.content
    );
    assert!(
      out.content.len() < content.len(),
      "a chunked hit must be smaller than the whole source file"
    );
  }

  /// An unrecognized `chunk_strategy` must surface a clear build error
  /// rather than silently falling back to whole-file indexing.
  #[tokio::test]
  async fn unknown_chunk_strategy_is_a_build_error() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("knowledge").join("manual.md"), "content");
    write_toml(
      dir.path(),
      r#"
[skill]
name = "bad-chunk-strategy-skill"
version = "0.1"
description = "rag-tier knowledge with a typo'd chunk strategy"

[persona]
role = "You are an ops assistant."

[[knowledge]]
path = "./knowledge/manual.md"
backend = "rag"
chunk_strategy = "fixed_sized"
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();
    match SkillBuilder::build_registry(&manifest, dir.path()).await {
      Err(SkillError::ValidationError { .. }) => {}
      Err(other) => panic!("expected ValidationError, got a different SkillError: {other}"),
      Ok(_) => panic!("unknown chunk_strategy must be rejected, not silently built"),
    }
  }

  /// V4.4 follow-up: `"semantic"` is now a recognized `chunk_strategy` name
  /// (previously fell into the "unknown chunk_strategy" branch above).
  #[test]
  fn parse_chunk_strategy_accepts_semantic() {
    assert_eq!(
      parse_chunk_strategy("semantic").unwrap(),
      yanshi_rag::types::ChunkingStrategy::Semantic
    );
  }

  /// V4.4 follow-up: `chunk_strategy = "semantic"` needs an OpenAI embedding
  /// provider, which needs `OPENAI_API_KEY`. Building without it must fail
  /// with a clear, actionable `ValidationError` rather than panicking or
  /// hanging on a network call — and this must be provable without a real
  /// key or network access, since `OpenAIEmbedding::builder(...).build()`'s
  /// key check is synchronous and happens before any HTTP call.
  ///
  /// **Test isolation**: temporarily clears `OPENAI_API_KEY`, snapshot +
  /// restore, mirroring `yanshi-cli`'s
  /// `build_dense_retriever_errors_without_openai_api_key` convention so a
  /// dev environment with the key set doesn't leak into or out of this test.
  #[tokio::test]
  async fn semantic_chunk_strategy_without_openai_api_key_is_a_validation_error() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("knowledge").join("manual.md"), "content");
    write_toml(
      dir.path(),
      r#"
[skill]
name = "semantic-chunk-skill"
version = "0.1"
description = "rag-tier knowledge with semantic chunking"

[persona]
role = "You are an ops assistant."

[[knowledge]]
path = "./knowledge/manual.md"
backend = "rag"
chunk_strategy = "semantic"
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();

    let snapshot = std::env::var("OPENAI_API_KEY").ok();
    // SAFETY: dedicated test process; restore at end.
    unsafe {
      std::env::remove_var("OPENAI_API_KEY");
    }

    let result = SkillBuilder::build_registry(&manifest, dir.path()).await;

    // SAFETY: restore before asserting so a panic doesn't pollute env.
    unsafe {
      if let Some(value) = snapshot {
        std::env::set_var("OPENAI_API_KEY", value);
      }
    }

    match result {
      Err(SkillError::ValidationError { message }) => {
        assert!(
          message.contains("OPENAI_API_KEY"),
          "error should hint at the missing key: {message}"
        );
      }
      Err(other) => panic!("expected ValidationError, got a different SkillError: {other}"),
      Ok(_) => panic!("missing OPENAI_API_KEY must be rejected, not silently built"),
    }
  }

  /// L4.3: a rag-tier entry with `query_rewrite = "split"` recovers a doc
  /// whose relevant content is split across the two halves of a compound
  /// query — the literal scenario the TODO calls out ("拆分子查询 /
  /// 多路召回合并"). Without decomposition, the raw compound query
  /// matches neither half well enough to surface both docs.
  #[tokio::test]
  async fn query_rewrite_split_recovers_docs_split_across_a_compound_query() {
    let dir = TempDir::new().unwrap();
    write_file(
      &dir.path().join("knowledge").join("ownership.md"),
      "Rust ownership rules move values on assignment.",
    );
    write_file(
      &dir.path().join("knowledge").join("borrowing.md"),
      "Rust borrowing lets you reference data without taking ownership.",
    );
    write_toml(
      dir.path(),
      r#"
[skill]
name = "query-rewrite-skill"
version = "0.1"
description = "rag-tier knowledge with query rewrite"

[persona]
role = "You are a Rust tutor."

[[knowledge]]
path = "./knowledge/ownership.md"
backend = "rag"
query_rewrite = "split"

[[knowledge]]
path = "./knowledge/borrowing.md"
backend = "rag"
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();
    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    let tool = registry.get("rag_search").expect("rag_search registered");

    let out = tool
      .execute(serde_json::json!({ "query": "ownership rules and borrowing data", "top_k": 2 }))
      .await
      .expect("execute ok");
    assert!(!out.is_error);
    assert!(
      out.content.contains("move values"),
      "expected the ownership doc in results: {}",
      out.content
    );
    assert!(
      out.content.contains("reference data"),
      "expected the borrowing doc in results (recovered via decomposition): {}",
      out.content
    );
  }

  /// An unrecognized `query_rewrite` must surface a clear build error
  /// rather than silently skipping rewriting.
  #[tokio::test]
  async fn unknown_query_rewrite_is_a_build_error() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("knowledge").join("manual.md"), "content");
    write_toml(
      dir.path(),
      r#"
[skill]
name = "bad-query-rewrite-skill"
version = "0.1"
description = "rag-tier knowledge with a typo'd query_rewrite"

[persona]
role = "You are an ops assistant."

[[knowledge]]
path = "./knowledge/manual.md"
backend = "rag"
query_rewrite = "splt"
"#,
    );
    let manifest = SkillLoader::load(dir.path()).unwrap();
    match SkillBuilder::build_registry(&manifest, dir.path()).await {
      Err(SkillError::ValidationError { .. }) => {}
      Err(other) => panic!("expected ValidationError, got a different SkillError: {other}"),
      Ok(_) => panic!("unknown query_rewrite must be rejected, not silently built"),
    }
  }

  // ── P10.4.1: per-tool os_sandbox override ──────────────────────────────────
  //
  // Three resolution cases:
  //   1. tool override None + manifest false → effective false → noop backend.
  //   2. tool override None + manifest true  → effective true  → default
  //      platform backend.
  //   3. tool override Some(true)  + manifest false → effective true (opt in).
  //   4. tool override Some(false) + manifest true  → effective false (opt out).
  //
  // We assert against `Tool::sandbox_status().backend` so the test stays
  // portable: on platforms without an enforcing backend (macOS x86_64,
  // Windows), `default_backend()` returns the noop backend by design and
  // both `with_os_sandbox()` and the default constructor produce the same
  // "noop" name. The contract we're pinning is "the override controls
  // *which* `with_os_sandbox` call fires", not "this is sandbox-exec on
  // every host."

  fn expected_backend_name_when_enforcing() -> String {
    yanshi_tools::sandbox::default_backend().name().to_string()
  }

  fn sandbox_backend_for(registry: &yanshi_tools::ToolRegistry, tool_name: &str) -> String {
    registry
      .get(tool_name)
      .expect("tool present in registry")
      .sandbox_status()
      .expect("shell/script tools always report a sandbox status")
      .backend
  }

  #[tokio::test]
  async fn per_tool_os_sandbox_override_opts_in_when_manifest_default_off() {
    // Manifest default OFF, shell tool individually OPTS IN. Resolved
    // value for shell must be ON (default_backend) regardless of the
    // manifest-level flag.
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("per-tool-opt-in");
    manifest.security.os_sandbox = false;
    manifest.tools = vec![ToolConfig {
      name: "shell".to_string(),
      os_sandbox: Some(true),
      ..ToolConfig::default()
    }];
    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    assert_eq!(
      sandbox_backend_for(&registry, "shell"),
      expected_backend_name_when_enforcing(),
      "tool-level os_sandbox=true must override manifest default off"
    );
  }

  #[tokio::test]
  async fn per_tool_os_sandbox_override_opts_out_when_manifest_default_on() {
    // Manifest default ON, script tool individually OPTS OUT. Resolved
    // value for script must be OFF (noop) — proves the override beats
    // the manifest in the other direction.
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("per-tool-opt-out");
    manifest.security.os_sandbox = true;
    manifest.tools = vec![ToolConfig {
      name: "script".to_string(),
      os_sandbox: Some(false),
      ..ToolConfig::default()
    }];
    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    assert_eq!(
      sandbox_backend_for(&registry, "script"),
      "noop",
      "tool-level os_sandbox=false must override manifest default on"
    );
  }

  #[tokio::test]
  async fn missing_per_tool_os_sandbox_falls_back_to_manifest_default_on() {
    // Manifest ON, no per-tool override → inherits manifest level.
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("manifest-default-on");
    manifest.security.os_sandbox = true;
    manifest.tools = vec![ToolConfig {
      name: "shell".to_string(),
      ..ToolConfig::default()
    }];
    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    assert_eq!(
      sandbox_backend_for(&registry, "shell"),
      expected_backend_name_when_enforcing(),
      "no tool override + manifest on must inherit manifest on"
    );
  }

  #[tokio::test]
  async fn missing_per_tool_os_sandbox_falls_back_to_manifest_default_off() {
    let dir = TempDir::new().unwrap();
    let mut manifest = minimal_manifest("manifest-default-off");
    manifest.security.os_sandbox = false;
    manifest.tools = vec![ToolConfig {
      name: "shell".to_string(),
      ..ToolConfig::default()
    }];
    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    assert_eq!(
      sandbox_backend_for(&registry, "shell"),
      "noop",
      "no tool override + manifest off must inherit manifest off"
    );
  }

  #[tokio::test]
  async fn per_tool_os_sandbox_isolates_overrides_across_tools_in_same_skill() {
    // The same manifest sandboxes shell but NOT script. Covers the
    // exact "heterogeneous enforcement" use case the TODO calls out.
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("scripts")).unwrap();
    write_file(
      &dir.path().join("scripts").join("hello.sh"),
      "#!/bin/bash\necho hi",
    );
    let mut manifest = minimal_manifest("heterogeneous");
    manifest.security.os_sandbox = false;
    manifest.tools = vec![
      ToolConfig {
        name: "shell".to_string(),
        os_sandbox: Some(true),
        ..ToolConfig::default()
      },
      ToolConfig {
        name: "script".to_string(),
        os_sandbox: Some(false),
        ..ToolConfig::default()
      },
    ];
    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();
    assert_eq!(
      sandbox_backend_for(&registry, "shell"),
      expected_backend_name_when_enforcing()
    );
    assert_eq!(sandbox_backend_for(&registry, "script"), "noop");
  }

  #[test]
  fn tool_config_serde_round_trips_os_sandbox_override_field() {
    // Schema-stability pin: the field is named exactly `os_sandbox`
    // on `[[tools]]` blocks in TOML and accepts a plain boolean.
    // Renaming or moving it is a breaking change for any operator
    // who's adopted the override.
    let toml = r#"
name = "shell"
os_sandbox = true
"#;
    let parsed: ToolConfig = toml::from_str(toml).expect("parse tool config");
    assert_eq!(parsed.name, "shell");
    assert_eq!(parsed.os_sandbox, Some(true));

    let toml_off = r#"
name = "script"
os_sandbox = false
"#;
    let parsed_off: ToolConfig = toml::from_str(toml_off).unwrap();
    assert_eq!(parsed_off.os_sandbox, Some(false));

    // Field is fully optional — pre-P10.4.1 manifests without it
    // continue to parse to `None`.
    let toml_no_override = r#"
name = "shell"
"#;
    let parsed_no_override: ToolConfig = toml::from_str(toml_no_override).unwrap();
    assert_eq!(parsed_no_override.os_sandbox, None);
  }

  /// build() with sqlite memory creates the db directory.
  #[tokio::test]
  async fn build_with_sqlite_memory() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("memory").join("test.db");

    let mut manifest = minimal_manifest("sqlite-skill");
    manifest.memory = Some(crate::manifest::MemoryConfig {
      memory_type: "sqlite".to_string(),
      db_path: Some(db_path.to_string_lossy().into_owned()),
      window_tokens: None,
      embedding_model: None,
      preference: None,
      project: None,
    });

    let _agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
    assert!(db_path.exists(), "SQLite db file should have been created");
  }

  // ── U2.2: preference layer ───────────────────────────────────────────

  fn preference_manifest(name: &str, db_path: &std::path::Path) -> SkillManifest {
    let mut manifest = minimal_manifest(name);
    manifest.memory = Some(crate::manifest::MemoryConfig {
      memory_type: "none".to_string(),
      db_path: None,
      window_tokens: None,
      embedding_model: None,
      preference: Some(PreferenceMemoryConfig {
        enabled: true,
        db_path: Some(db_path.to_string_lossy().into_owned()),
      }),
      project: None,
    });
    manifest
  }

  /// `[memory.preference]` must register `remember_preference` into the
  /// built agent's tool registry.
  #[tokio::test]
  async fn build_registers_remember_preference_tool_when_configured() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("prefs.db");
    let manifest = preference_manifest("preference-skill", &db_path);

    let agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
    assert!(
      agent
        .tools()
        .get(RememberPreferenceTool::TOOL_NAME)
        .is_some(),
      "remember_preference must be registered when [memory.preference] is enabled"
    );
  }

  /// Without `[memory.preference]`, behaviour is unchanged: no tool
  /// registered, no store opened.
  #[tokio::test]
  async fn build_does_not_register_remember_preference_tool_when_not_configured() {
    let dir = TempDir::new().unwrap();
    let manifest = minimal_manifest("no-preference-skill");
    let agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
    assert!(
      agent
        .tools()
        .get(RememberPreferenceTool::TOOL_NAME)
        .is_none()
    );
  }

  /// The regression U2.2 exists for: a preference written in one
  /// Skill-produced agent's "session" must be visible to a second,
  /// brand-new Skill-produced agent — same `skill.toml` (same db path),
  /// no session in common — without either agent needing an LLM call
  /// (the tool is invoked directly here, the same way the real agent
  /// loop would invoke it after the LLM decides to call it; the
  /// mock-LLM-driven version of this same round trip is covered at the
  /// `yanshi-agents` layer by
  /// `react::agent::tests::remember_preference_tool_write_is_visible_to_a_second_agent_instance`).
  #[tokio::test]
  async fn preference_written_in_one_session_is_read_in_the_next() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("prefs.db");
    let manifest = preference_manifest("preference-skill", &db_path);

    // "Session 1": build the agent, then call the SkillBuilder-registered
    // `remember_preference` tool exactly as the real agent loop would
    // when the LLM decides to call it.
    let first_agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
    let tool = first_agent
      .tools()
      .get(RememberPreferenceTool::TOOL_NAME)
      .expect("registered above");
    let out = tool
      .execute(serde_json::json!({"key": "tone", "value": "concise"}))
      .await
      .unwrap();
    assert!(!out.is_error);

    // "Session 2": a brand-new agent built from the same skill.toml
    // (same db_path) must surface the preference without ever calling
    // `remember_preference` itself.
    let second_agent = SkillBuilder::build(&manifest, dir.path()).await.unwrap();
    let messages = second_agent.preview_llm_messages().await.unwrap();
    let rendered = format!("{messages:?}");
    assert!(
      rendered.contains("concise"),
      "the second agent's prompt must carry the preference the first agent's tool call \
       persisted, got: {rendered}"
    );
  }

  // ── L3.1/V1.6: project memory layer ──────────────────────────────────

  fn project_manifest(name: &str, project_db_path: &std::path::Path) -> SkillManifest {
    let mut manifest = minimal_manifest(name);
    manifest.memory = Some(crate::manifest::MemoryConfig {
      memory_type: "none".to_string(),
      db_path: None,
      window_tokens: None,
      embedding_model: None,
      preference: None,
      project: Some(ProjectMemoryConfig {
        enabled: true,
        db_path: Some(project_db_path.to_string_lossy().into_owned()),
      }),
    });
    manifest
  }

  /// Without a `project_root`, `[memory.project]` is a no-op even when
  /// enabled — no db file is created. This is the common case: most
  /// `SkillBuilder` call sites have no project-root concept at all (see
  /// `build_with_project_root`'s doc comment), so a skill declaring
  /// `[memory.project]` must not error or leave stray files for them.
  #[tokio::test]
  async fn build_with_project_root_none_does_not_open_project_store() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("project.db");
    let manifest = project_manifest("project-skill", &db_path);

    let _agent = SkillBuilder::build_with_project_root(&manifest, dir.path(), None)
      .await
      .unwrap();
    assert!(
      !db_path.exists(),
      "project memory db must not be created when project_root is None"
    );
  }

  /// The regression this exists for (the follow-up scope note in
  /// `yanshi-memory/src/project.rs`'s module doc comment): a project
  /// fact recorded under one `project_root` must be visible to a second,
  /// brand-new agent built against the *same* `project_root` — without
  /// either agent needing an LLM call (the store is written directly
  /// here, standing in for `ReActAgent::record_project_facts` firing
  /// after a real turn; that end-to-end extraction path is already
  /// covered at the `yanshi-agents` layer).
  #[tokio::test]
  async fn project_fact_recorded_for_one_root_is_read_by_a_second_agent_for_the_same_root() {
    let dir = TempDir::new().unwrap();
    let project_root = dir.path().join("workspace");
    std::fs::create_dir_all(&project_root).unwrap();
    let db_path = dir.path().join("project.db");
    let manifest = project_manifest("project-skill", &db_path);

    // Resolve the same db path + project_key the builder would use, and
    // record a fact directly.
    let store = SqliteProjectMemoryStore::open(&db_path).await.unwrap();
    let project_key = project_key_for_path(&project_root);
    store
      .record_project_fact(&project_key, "shell", "cargo test --workspace")
      .await
      .unwrap();

    let agent = SkillBuilder::build_with_project_root(&manifest, dir.path(), Some(&project_root))
      .await
      .unwrap();
    let messages = agent.preview_llm_messages().await.unwrap();
    let rendered = format!("{messages:?}");
    assert!(
      rendered.contains("cargo test --workspace"),
      "the agent's prompt must carry the project fact recorded for the same \
       project_root, got: {rendered}"
    );
  }

  /// A different `project_root` (different `project_key` hash) must not
  /// leak facts into an unrelated project's agent — mirrors
  /// `project_facts_are_isolated_by_project_key` at the `yanshi-agents`
  /// layer.
  #[tokio::test]
  async fn project_fact_is_isolated_by_project_root() {
    let dir = TempDir::new().unwrap();
    let project_a = dir.path().join("a");
    let project_b = dir.path().join("b");
    std::fs::create_dir_all(&project_a).unwrap();
    std::fs::create_dir_all(&project_b).unwrap();
    let db_path = dir.path().join("project.db");
    let manifest = project_manifest("project-skill", &db_path);

    let store = SqliteProjectMemoryStore::open(&db_path).await.unwrap();
    store
      .record_project_fact(&project_key_for_path(&project_a), "shell", "make build")
      .await
      .unwrap();

    let agent_b = SkillBuilder::build_with_project_root(&manifest, dir.path(), Some(&project_b))
      .await
      .unwrap();
    let messages = agent_b.preview_llm_messages().await.unwrap();
    let rendered = format!("{messages:?}");
    assert!(
      !rendered.contains("make build"),
      "a different project_root must not see another project's facts, got: {rendered}"
    );
  }

  // ── SandboxPolicy merge tests ────────────────────────────────────────

  #[test]
  fn sandbox_policy_uses_default_commands_for_shell_with_empty_list() {
    let tool_configs = vec![ToolConfig {
      name: "shell".to_string(),
      ..ToolConfig::default()
    }];
    let dir = TempDir::new().unwrap();
    let policy = build_sandbox_policy(&tool_configs, dir.path());
    // Should have the default safe command list, not empty
    assert!(!policy.allowed_commands.is_empty());
    assert!(policy.allowed_commands.iter().any(|c| c == "echo"));
  }

  #[test]
  fn sandbox_policy_merges_commands_from_multiple_tools() {
    let tool_configs = vec![
      ToolConfig {
        name: "shell".to_string(),
        allowed_commands: vec!["cargo".to_string()],
        ..ToolConfig::default()
      },
      ToolConfig {
        name: "shell".to_string(),
        allowed_commands: vec!["rustfmt".to_string()],
        ..ToolConfig::default()
      },
    ];
    let dir = TempDir::new().unwrap();
    let policy = build_sandbox_policy(&tool_configs, dir.path());
    assert!(policy.allowed_commands.contains(&"cargo".to_string()));
    assert!(policy.allowed_commands.contains(&"rustfmt".to_string()));
  }

  #[test]
  fn sandbox_policy_deduplicates_commands() {
    let tool_configs = vec![ToolConfig {
      name: "shell".to_string(),
      allowed_commands: vec!["cargo".to_string(), "cargo".to_string()],
      ..ToolConfig::default()
    }];
    let dir = TempDir::new().unwrap();
    let policy = build_sandbox_policy(&tool_configs, dir.path());
    let cargo_count = policy
      .allowed_commands
      .iter()
      .filter(|c| *c == "cargo")
      .count();
    assert_eq!(cargo_count, 1, "duplicates should be removed");
  }

  #[test]
  fn sandbox_policy_takes_max_exec_time() {
    let tool_configs = vec![
      ToolConfig {
        name: "shell".to_string(),
        max_exec_time_secs: Some(10),
        ..ToolConfig::default()
      },
      ToolConfig {
        name: "file".to_string(),
        max_exec_time_secs: Some(60),
        ..ToolConfig::default()
      },
    ];
    let dir = TempDir::new().unwrap();
    let policy = build_sandbox_policy(&tool_configs, dir.path());
    assert_eq!(policy.max_exec_time_secs, 60);
  }

  #[test]
  fn sandbox_policy_does_not_inject_script_defaults_into_shared_policy() {
    // S0.2: the shared policy (what `file`/`shell` read) must stay free of
    // script's own execution-boundary defaults, even when `script` is
    // declared — those are layered on separately via `finalize_script_policy`.
    let dir = TempDir::new().unwrap();
    let tool_configs = vec![ToolConfig {
      name: "script".to_string(),
      ..ToolConfig::default()
    }];

    let policy = build_sandbox_policy(&tool_configs, dir.path());

    assert!(policy.allowed_paths.is_empty());
    assert!(!policy.allowed_commands.contains(&"python3".to_string()));
    assert!(!policy.allowed_commands.contains(&"bash".to_string()));
    assert!(!policy.allowed_commands.contains(&"node".to_string()));
  }

  #[test]
  fn finalize_script_policy_restricts_defaults_to_scripts_dir_and_interpreters() {
    let dir = TempDir::new().unwrap();
    let tool_configs = vec![ToolConfig {
      name: "script".to_string(),
      ..ToolConfig::default()
    }];

    let base = build_sandbox_policy(&tool_configs, dir.path());
    let policy = finalize_script_policy(&base, dir.path());

    assert_eq!(policy.allowed_paths, vec![dir.path().join("scripts")]);
    assert!(policy.allowed_commands.contains(&"python3".to_string()));
    assert!(policy.allowed_commands.contains(&"bash".to_string()));
    assert!(policy.allowed_commands.contains(&"node".to_string()));
  }

  #[test]
  fn exclude_scripts_dir_strips_scripts_path_from_file_policy() {
    let dir = TempDir::new().unwrap();
    let base = SandboxPolicy {
      allowed_paths: vec![dir.path().join("scripts"), dir.path().join("data")],
      ..SandboxPolicy::default()
    };

    let policy = exclude_scripts_dir(&base, dir.path());

    assert_eq!(policy.allowed_paths, vec![dir.path().join("data")]);
  }

  /// S0.2 regression: a skill declaring `file` + `script` with no explicit
  /// `allowed_paths` must not let the LLM write a new script into
  /// `scripts/` and then have `script` execute it.
  #[tokio::test]
  async fn file_plus_script_skill_cannot_write_then_execute_a_new_script() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("scripts")).unwrap();
    write_file(
      &dir.path().join("scripts").join("hello.sh"),
      "#!/bin/bash\necho hi",
    );

    let mut manifest = minimal_manifest("file-script-skill");
    manifest.tools = vec![
      ToolConfig {
        name: "file".to_string(),
        ..ToolConfig::default()
      },
      ToolConfig {
        name: "script".to_string(),
        ..ToolConfig::default()
      },
    ];

    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();

    let evil_path = dir
      .path()
      .join("scripts")
      .join("evil.sh")
      .to_string_lossy()
      .into_owned();
    let write_result = registry
      .execute(
        "file",
        serde_json::json!({
          "operation": "write",
          "path": evil_path,
          "content": "#!/bin/bash\necho pwned"
        }),
      )
      .await;
    assert!(
      write_result.is_err(),
      "file tool must not be able to write into scripts/, got {write_result:?}"
    );

    // The author-signed script that was already there must still run.
    let run_result = registry
      .execute("script", serde_json::json!({"script": "hello.sh"}))
      .await
      .unwrap();
    assert!(run_result.content.contains("hi"));
  }

  /// S1.3 regression: `manifest.scripts` reaches `ScriptTool` end-to-end
  /// through `SkillBuilder::build_registry` — a script tampered after the
  /// registry was built is rejected at execution time, not just when
  /// exercising `ScriptTool` directly (yanshi-tools unit tests already
  /// cover the tool in isolation; this proves the manifest → tool wiring).
  #[tokio::test]
  async fn declared_script_hash_is_enforced_through_build_registry() {
    let dir = TempDir::new().unwrap();
    let content = "#!/bin/bash\necho hi";
    write_file(&dir.path().join("scripts").join("hello.sh"), content);

    let mut manifest = minimal_manifest("integrity-skill");
    manifest.tools = vec![ToolConfig {
      name: "script".to_string(),
      ..ToolConfig::default()
    }];
    manifest.scripts = vec![crate::manifest::ScriptIntegrityEntry {
      name: "hello.sh".to_string(),
      sha256: {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
      },
    }];

    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();

    // Hash matches at first: runs fine.
    let ok_result = registry
      .execute("script", serde_json::json!({"script": "hello.sh"}))
      .await
      .unwrap();
    assert!(ok_result.content.contains("hi"));

    // Tamper after the registry (and its ScriptTool) was already built.
    write_file(
      &dir.path().join("scripts").join("hello.sh"),
      "#!/bin/bash\necho tampered",
    );
    let tampered_result = registry
      .execute("script", serde_json::json!({"script": "hello.sh"}))
      .await;
    assert!(
      matches!(
        tampered_result,
        Err(yanshi_tools::ToolError::SandboxViolation { .. })
      ),
      "expected a SandboxViolation for tampered content, got {tampered_result:?}"
    );
  }

  /// S2 regression: a skill that declares `[dependencies].python` but has
  /// no `vendor/` directory to install from offline must fail to build its
  /// registry — never silently fall back to the global interpreter.
  #[tokio::test]
  async fn declared_python_dependencies_without_vendor_dir_fails_build_registry() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("scripts").join("run.py"), "print('hi')");
    write_file(
      &dir.path().join("requirements.txt"),
      "pkg==1.0.0 --hash=sha256:deadbeef\n",
    );

    let mut manifest = minimal_manifest("deps-no-vendor");
    manifest.tools = vec![ToolConfig {
      name: "script".to_string(),
      ..ToolConfig::default()
    }];
    manifest.dependencies = crate::manifest::DependenciesConfig {
      python: Some("requirements.txt".to_string()),
    };

    let result = SkillBuilder::build_registry(&manifest, dir.path()).await;
    assert!(result.is_err(), "expected a build failure");
  }

  /// S2.2/S2.3 end-to-end: `manifest.dependencies.python` reaches
  /// `ScriptTool` through `SkillBuilder::build_registry` — a `.py` script
  /// actually runs against the skill's isolated venv, with a real
  /// installed dependency importable, not the global `python3`.
  /// `yanshi-skills/src/python_env.rs` unit-tests venv construction in
  /// isolation; this proves the manifest → builder → tool wiring.
  #[tokio::test]
  async fn declared_python_dependencies_are_installed_and_used_through_build_registry() {
    if std::process::Command::new("python3")
      .arg("--version")
      .output()
      .map(|o| !o.status.success())
      .unwrap_or(true)
    {
      eprintln!("skipping: python3 not on PATH");
      return;
    }

    let dir = TempDir::new().unwrap();
    write_file(
      &dir.path().join("scripts").join("run.py"),
      "import yanshi_test_pkg\nprint(yanshi_test_pkg.VALUE)\n",
    );

    let vendor_dir = dir.path().join("vendor");
    fs::create_dir(&vendor_dir).unwrap();
    let wheel_path = vendor_dir.join("yanshi_test_pkg-1.0.0-py3-none-any.whl");
    let build_wheel_script = dir.path().join("build_wheel.py");
    write_file(&build_wheel_script, WHEEL_BUILDER_SCRIPT);
    let status = std::process::Command::new("python3")
      .arg(&build_wheel_script)
      .arg(&wheel_path)
      .status()
      .expect("spawn wheel builder");
    assert!(status.success(), "failed to build the test fixture wheel");

    let wheel_hash = {
      use sha2::{Digest, Sha256};
      let mut hasher = Sha256::new();
      hasher.update(std::fs::read(&wheel_path).unwrap());
      format!("{:x}", hasher.finalize())
    };
    write_file(
      &dir.path().join("requirements.txt"),
      &format!("yanshi-test-pkg==1.0.0 --hash=sha256:{wheel_hash}\n"),
    );

    let mut manifest = minimal_manifest("deps-real-venv");
    manifest.tools = vec![ToolConfig {
      name: "script".to_string(),
      ..ToolConfig::default()
    }];
    manifest.dependencies = crate::manifest::DependenciesConfig {
      python: Some("requirements.txt".to_string()),
    };

    let registry = SkillBuilder::build_registry(&manifest, dir.path())
      .await
      .unwrap();

    let result = registry
      .execute("script", serde_json::json!({"script": "run.py"}))
      .await
      .unwrap();
    assert_eq!(result.content.trim(), "42");
  }

  /// Builds a minimal, valid, dependency-free wheel by hand (via stdlib
  /// `zipfile`) — mirrors `yanshi-skills/src/python_env.rs`'s test
  /// fixture builder; kept separate since builder.rs's `tests` module
  /// doesn't share private items with `python_env`'s.
  const WHEEL_BUILDER_SCRIPT: &str = r#"
import sys
import zipfile

out_path = sys.argv[1]
dist_info = "yanshi_test_pkg-1.0.0.dist-info"

with zipfile.ZipFile(out_path, "w") as zf:
    zf.writestr("yanshi_test_pkg.py", "VALUE = 42\n")
    zf.writestr(
        f"{dist_info}/METADATA",
        "Metadata-Version: 2.1\nName: yanshi-test-pkg\nVersion: 1.0.0\n",
    )
    zf.writestr(
        f"{dist_info}/WHEEL",
        "Wheel-Version: 1.0\nGenerator: yanshi-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    )
    zf.writestr(f"{dist_info}/RECORD", "")
"#;

  #[test]
  fn mcp_governance_applies_default_limits() {
    let mut manifest = minimal_manifest("mcp-defaults");
    manifest.mcp_servers = vec![McpServerConfig {
      name: "fixture".to_string(),
      command: "python3".to_string(),
      args: vec!["server.py".to_string()],
      env: Default::default(),
      timeout_secs: None,
      max_concurrent_calls: None,
    }];

    validate_mcp_governance(&manifest).unwrap();
    let resolved = resolve_mcp_server_config(
      &manifest.mcp_servers[0],
      Path::new("/tmp/skill"),
      &manifest.security,
    );

    assert_eq!(
      resolved.timeout_secs,
      Some(manifest.security.resolved_mcp_default_timeout_secs())
    );
    assert_eq!(
      resolved.max_concurrent_calls,
      Some(manifest.security.resolved_mcp_max_concurrent_calls())
    );
  }

  #[test]
  fn mcp_governance_rejects_unapproved_command() {
    let mut manifest = minimal_manifest("mcp-blocked");
    manifest.mcp_servers = vec![McpServerConfig {
      name: "fixture".to_string(),
      command: "curl".to_string(),
      args: vec![],
      env: Default::default(),
      timeout_secs: None,
      max_concurrent_calls: None,
    }];

    let result = validate_mcp_governance(&manifest);

    assert!(matches!(result, Err(SkillError::ValidationError { .. })));
  }
}
