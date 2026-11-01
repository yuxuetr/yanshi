use serde::{Deserialize, Serialize};

use yanshi_tools::ToolPermission;

/// Top-level structure of a `skill.toml` file.
///
/// # Example
/// ```toml
/// [skill]
/// name    = "rust_expert"
/// version = "1.0.0"
/// description = "Rust code expert focused on safety and performance"
///
/// [persona]
/// role = "You are a senior Rust engineer..."
///
/// [model]
/// name           = "gpt-4o"
/// max_iterations = 15
/// budget_tokens  = 30000
///
/// [[tools]]
/// name             = "shell"
/// allowed_commands = ["cargo", "clippy", "rustfmt"]
///
/// [[tools]]
/// name          = "file"
/// allowed_paths = ["/tmp", "./src"]
///
/// [[knowledge]]
/// path        = "./knowledge/rust-guidelines.md"
/// description = "Internal Rust coding standards"
///
/// [memory]
/// type         = "sqlite"
/// window_tokens = 8000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
  pub skill: SkillInfo,
  pub persona: PersonaConfig,
  #[serde(default)]
  pub model: ModelConfig,
  #[serde(default)]
  pub security: SecurityConfig,
  #[serde(default)]
  pub tools: Vec<ToolConfig>,
  #[serde(default)]
  pub mcp_servers: Vec<McpServerConfig>,
  #[serde(default)]
  pub knowledge: Vec<KnowledgeConfig>,
  pub memory: Option<MemoryConfig>,
  /// Optional `[validation]` section backing the agent-eval harness's
  /// `final_answer_matches_skill` assertion. See
  /// `docs/SKILL_VALIDATOR_PROTOCOL.md` for the contract. Default is
  /// [`ValidationConfig::None`].
  #[serde(default)]
  pub validation: ValidationConfig,
  /// `[[scripts]]` integrity manifest for the `script` tool (S1.1):
  /// author-signed filename + sha256, fixed at install time. Checked
  /// against `scripts/` at load time (`SkillLoader::validate`) and, when
  /// non-empty, enforced again at execution time by `ScriptTool` itself
  /// (S1.2) — see `docs/RFC_CODE_EXECUTION_TRUST.md`. Empty (the default,
  /// and every manifest predating this field) means the skill has not
  /// adopted script integrity; `SkillLoader::validate` gates that on the
  /// active `SecurityProfile` (S1.3).
  #[serde(default)]
  pub scripts: Vec<ScriptIntegrityEntry>,
  /// `[dependencies]` (S2.1): per-skill Python dependency declaration,
  /// installed into an isolated `.venv/` at load time (S2.2) instead of
  /// running scripts against whatever's globally installed on the host.
  /// Default (`python: None`) means the skill declares no dependencies —
  /// `script` tool `.py` files run against the global `python3`, exactly
  /// as before this field existed.
  #[serde(default)]
  pub dependencies: DependenciesConfig,
}

/// `[dependencies]` section.
///
/// ```toml
/// [dependencies]
/// python = "requirements.txt"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DependenciesConfig {
  /// Path, relative to the skill directory, to a pip-style requirements
  /// file. Every entry must be exactly version-pinned (`==`) and carry a
  /// `--hash=sha256:...` (pip's own native hash-checking mode) —
  /// `SkillLoader::validate` rejects any entry missing either. Installed
  /// fully offline: `pip install --no-index --find-links vendor/
  /// --require-hashes -r <this file>` against the skill's own `vendor/`
  /// directory of pre-fetched wheels/sdists — Yanshi itself never
  /// touches the network to build this environment. See
  /// `docs/RFC_CODE_EXECUTION_TRUST.md`.
  #[serde(default)]
  pub python: Option<String>,
}

/// One `[[scripts]]` entry: a plain filename inside `scripts/` and the
/// lowercase hex-encoded SHA-256 of its exact bytes at install time.
///
/// ```toml
/// [[scripts]]
/// name   = "check_syntax.py"
/// sha256 = "8f0e2a1c...(64 hex chars)"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ScriptIntegrityEntry {
  pub name: String,
  pub sha256: String,
}

/// Closed `kind` discriminator backing the v1 skill validator
/// protocol. New variants land under a `schema_version` bump in the
/// manifest; the loader rejects unknown kinds at v1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValidationConfig {
  /// Skill explicitly declares no validator (equivalent to omitting
  /// the `[validation]` table). `final_answer_matches_skill` reports
  /// "skill declares no validator" for runs against this skill.
  #[default]
  None,
  /// Regex match against the final answer. Pattern is compiled at
  /// `SkillLoader::validate` time so malformed regexes never reach the
  /// eval runner.
  Regex {
    pattern: String,
    /// Set the regex `m` flag. Default `false`.
    #[serde(default)]
    multiline: bool,
    /// Set the regex `s` flag (dot matches newline). Default `false`.
    #[serde(default)]
    dotall: bool,
  },
  /// Shell command that receives the final answer on stdin and returns
  /// its verdict via exit code (0 = pass, non-zero = fail, 125 reserved
  /// for "unrunnable" per the `git bisect run` convention).
  Command {
    /// `argv` with the executable at `[0]`. Resolved through `PATH` if
    /// no slash is present; absolute / relative paths are honored
    /// verbatim.
    command: Vec<String>,
    /// Wall-clock timeout in seconds (clamped to [1, 120]). Default
    /// 30.
    #[serde(default = "default_validator_timeout_secs")]
    timeout_secs: u64,
    /// Working directory relative to the skill directory. Default `.`.
    #[serde(default = "default_validator_working_dir")]
    working_dir: String,
    /// Environment variable allowlist. Default `["PATH", "LANG"]` —
    /// any variable not in this list is stripped from the child's
    /// environment before exec.
    #[serde(default = "default_validator_env_allowlist")]
    env_allowlist: Vec<String>,
  },
}

fn default_validator_timeout_secs() -> u64 {
  30
}

fn default_validator_working_dir() -> String {
  ".".to_string()
}

fn default_validator_env_allowlist() -> Vec<String> {
  vec!["PATH".to_string(), "LANG".to_string()]
}

/// Hard upper bound on validator timeouts; matches the doc.
pub const VALIDATOR_TIMEOUT_SECS_MAX: u64 = 120;
/// Hard lower bound on validator timeouts (0 would be a no-op race).
pub const VALIDATOR_TIMEOUT_SECS_MIN: u64 = 1;

/// Basic identity metadata for the skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInfo {
  pub name: String,
  pub version: String,
  pub description: String,
}

/// Defines the LLM persona injected into the system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaConfig {
  /// The role / instruction text for the LLM (becomes the system prompt base).
  pub role: String,
  /// Optional language hint appended to the system prompt.
  pub language: Option<String>,
}

/// Model and runtime constraints for the agent.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
  /// LLM model identifier. Defaults to `"gpt-4o"`.
  pub name: Option<String>,
  /// Maximum ReAct iterations. Defaults to 15.
  pub max_iterations: Option<usize>,
  /// Token budget before halting. Defaults to 50 000.
  pub budget_tokens: Option<u32>,
}

impl ModelConfig {
  pub fn resolved_model(&self) -> &str {
    self.name.as_deref().unwrap_or("gpt-4o")
  }
  pub fn resolved_max_iterations(&self) -> usize {
    self.max_iterations.unwrap_or(15)
  }
  pub fn resolved_budget_tokens(&self) -> u32 {
    self.budget_tokens.unwrap_or(50_000)
  }
}

/// Declares an MCP server the skill connects to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerConfig {
  pub name: String,
  pub command: String,
  #[serde(default)]
  pub args: Vec<String>,
  #[serde(default)]
  pub env: std::collections::HashMap<String, String>,
  /// Timeout for MCP connect, discovery, and tool calls in seconds.
  /// Defaults to [`SecurityConfig::mcp_default_timeout_secs`].
  pub timeout_secs: Option<u64>,
  /// Maximum concurrent calls admitted for this server.
  /// Defaults to [`SecurityConfig::mcp_max_concurrent_calls`].
  pub max_concurrent_calls: Option<usize>,
}

impl McpServerConfig {
  pub fn resolved_timeout(&self) -> std::time::Duration {
    std::time::Duration::from_secs(self.resolved_timeout_secs())
  }

  pub fn resolved_timeout_secs(&self) -> u64 {
    self
      .timeout_secs
      .unwrap_or(DEFAULT_MCP_TIMEOUT_SECS)
      .clamp(1, MAX_MCP_TIMEOUT_SECS)
  }

  pub fn resolved_max_concurrent_calls(&self) -> usize {
    self
      .max_concurrent_calls
      .unwrap_or(DEFAULT_MCP_MAX_CONCURRENT_CALLS)
      .clamp(1, MAX_MCP_MAX_CONCURRENT_CALLS)
  }
}

pub const DEFAULT_MCP_TIMEOUT_SECS: u64 = 30;
pub const MAX_MCP_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_MCP_MAX_CONCURRENT_CALLS: usize = 4;
pub const MAX_MCP_MAX_CONCURRENT_CALLS: usize = 32;
pub const DEFAULT_MCP_MAX_SERVERS: usize = 4;
pub const MAX_MCP_MAX_SERVERS: usize = 32;

/// Skill-level governance controls for tool and MCP execution.
///
/// Empty allowlists mean "use Yanshi's default policy"; they do not mean
/// unrestricted execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
  /// Allowed MCP server names. Empty = allow all declared server names.
  #[serde(default)]
  pub mcp_server_allowlist: Vec<String>,
  /// Allowed executable names for MCP stdio servers.
  #[serde(default = "default_mcp_command_allowlist")]
  pub mcp_command_allowlist: Vec<String>,
  /// Allowed environment variable names forwarded to MCP servers.
  #[serde(default)]
  pub mcp_env_allowlist: Vec<String>,
  /// Default MCP connect/discovery/call timeout in seconds.
  #[serde(default = "default_mcp_timeout_secs")]
  pub mcp_default_timeout_secs: u64,
  /// Default max concurrent tool calls admitted per MCP server.
  #[serde(default = "default_mcp_max_concurrent_calls")]
  pub mcp_max_concurrent_calls: usize,
  /// Maximum MCP server declarations in one skill.
  #[serde(default = "default_mcp_max_servers")]
  pub mcp_max_servers: usize,
  /// Allowed tool permission categories. Empty = allow all registered tools.
  #[serde(default)]
  pub tool_permission_allowlist: Vec<ToolPermission>,
  /// Wrap `shell` and `script` invocations in the platform OS-level sandbox
  /// (macOS `sandbox-exec` profile, Linux seccomp BPF + Landlock + cgroup
  /// v2 resource limits). Defaults to `true` (S3.4) now that the Linux
  /// backend has path-scoped containment (S3.1) and resource limits (S3.2)
  /// on par with macOS (S3.3); a skill can still opt out per-manifest or
  /// per-tool (`ToolConfig::os_sandbox`, P10.4.1) if it genuinely needs
  /// unsandboxed shell/script access. Skills authored before this flag
  /// existed and that never set it pick up enforcement automatically —
  /// see `docs/HARNESS_MODE.md` / skill authoring docs for the opt-out.
  #[serde(default = "default_os_sandbox")]
  pub os_sandbox: bool,
}

impl Default for SecurityConfig {
  fn default() -> Self {
    Self {
      mcp_server_allowlist: Vec::new(),
      mcp_command_allowlist: default_mcp_command_allowlist(),
      mcp_env_allowlist: Vec::new(),
      mcp_default_timeout_secs: DEFAULT_MCP_TIMEOUT_SECS,
      mcp_max_concurrent_calls: DEFAULT_MCP_MAX_CONCURRENT_CALLS,
      mcp_max_servers: DEFAULT_MCP_MAX_SERVERS,
      tool_permission_allowlist: Vec::new(),
      os_sandbox: default_os_sandbox(),
    }
  }
}

fn default_os_sandbox() -> bool {
  true
}

impl SecurityConfig {
  pub fn resolved_mcp_command_allowlist(&self) -> Vec<String> {
    if self.mcp_command_allowlist.is_empty() {
      default_mcp_command_allowlist()
    } else {
      self.mcp_command_allowlist.clone()
    }
  }

  pub fn resolved_mcp_default_timeout_secs(&self) -> u64 {
    self.mcp_default_timeout_secs.clamp(1, MAX_MCP_TIMEOUT_SECS)
  }

  pub fn resolved_mcp_max_concurrent_calls(&self) -> usize {
    self
      .mcp_max_concurrent_calls
      .clamp(1, MAX_MCP_MAX_CONCURRENT_CALLS)
  }

  pub fn resolved_mcp_max_servers(&self) -> usize {
    self.mcp_max_servers.clamp(1, MAX_MCP_MAX_SERVERS)
  }
}

fn default_mcp_timeout_secs() -> u64 {
  DEFAULT_MCP_TIMEOUT_SECS
}

fn default_mcp_max_concurrent_calls() -> usize {
  DEFAULT_MCP_MAX_CONCURRENT_CALLS
}

fn default_mcp_max_servers() -> usize {
  DEFAULT_MCP_MAX_SERVERS
}

pub fn default_mcp_command_allowlist() -> Vec<String> {
  ["python", "python3", "node", "npx", "uvx"]
    .into_iter()
    .map(ToString::to_string)
    .collect()
}

/// Declares a tool the skill is authorised to use, with optional constraints.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolConfig {
  /// Tool name — one of `"shell"`, `"file"`, `"http"`, `"script"`,
  /// `"code_exec"`. The constraint fields below (`allowed_commands`,
  /// `allowed_paths`, `allowed_domains`, `os_sandbox`) are ignored for
  /// `code_exec` — none of its isolation is author-configurable in v1
  /// (S4.2).
  pub name: String,

  // ── shell constraints ────────────────────────────────────────────────────
  /// Allowed shell commands (first token). Empty = use the default safe list.
  #[serde(default)]
  pub allowed_commands: Vec<String>,

  // ── file constraints ─────────────────────────────────────────────────────
  /// Allowed filesystem path prefixes. Empty = all paths allowed.
  #[serde(default)]
  pub allowed_paths: Vec<String>,

  // ── http constraints ─────────────────────────────────────────────────────
  /// Allowed host suffixes for HTTP requests. Empty = all domains allowed.
  #[serde(default)]
  pub allowed_domains: Vec<String>,

  /// JSON schema for validating input parameters to the tool
  #[serde(default)]
  pub parameters: Option<serde_json::Value>,

  /// Override the sandbox exec-time limit (seconds).
  pub max_exec_time_secs: Option<u64>,

  /// Per-tool override of the manifest-level `[security] os_sandbox`
  /// flag (P10.4.1). `None` (the default) means inherit from
  /// `SecurityConfig::os_sandbox`. `Some(true)` opts this tool in
  /// even when the manifest-level default is off; `Some(false)`
  /// opts it out even when the manifest-level default is on.
  /// Only honored for tools that actually spawn subprocesses
  /// (`shell`, `script`) — non-process tools (`file`, `http`)
  /// ignore the field.
  pub os_sandbox: Option<bool>,
}

/// How a knowledge entry's content reaches the agent (P-A4.2 — tiered
/// knowledge resolution, RFC §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeBackendKind {
  /// Inline the file content directly into the agent's system prompt (the
  /// persona's "Knowledge Context"). Best for small, always-relevant material.
  /// This is the default — it preserves the pre-P-A4.2 behaviour, so existing
  /// `[[knowledge]]` entries keep working unchanged.
  #[default]
  Files,
  /// Index the file content into an in-memory keyword retrieval backend and
  /// expose a `rag_search` tool over it; the agent retrieves only the relevant
  /// passages on demand instead of carrying the whole corpus in its prompt.
  /// Best for large reference material that would otherwise blow the token
  /// budget. Backed by [`yanshi_rag::Bm25KnowledgeBackend`] — no vector DB
  /// or network required, the skill's bundled files are indexed locally.
  Rag,
}

/// A knowledge file (markdown, txt, …) made available to the agent.
///
/// `backend` chooses how the content is surfaced: [`KnowledgeBackendKind::Files`]
/// (default) inlines it into the system prompt; [`KnowledgeBackendKind::Rag`]
/// indexes it and exposes a `rag_search` tool for on-demand retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeConfig {
  /// Path to the file, relative to the skill directory. Glob patterns supported.
  pub path: String,
  /// Human-readable label shown in the system prompt header.
  pub description: Option<String>,
  /// Retrieval tier for this entry. Defaults to [`KnowledgeBackendKind::Files`].
  #[serde(default)]
  pub backend: KnowledgeBackendKind,
  /// Chunking strategy for `backend = "rag"` entries (L4.1): one of
  /// `"fixed_size"`, `"sentence"`, `"recursive"`, `"paragraph"`, `"heading"`,
  /// `"code_ast"` (Rust source only, requires `yanshi-rag`'s
  /// `code-chunking` feature), or `"semantic"` (embedding-based, requires
  /// `OPENAI_API_KEY` — see `embedding_model`/`chunk_similarity_threshold`/
  /// `chunk_min_segment_size`/`chunk_buffer_percentile` below). `None` (the
  /// default) preserves the pre-L4.1 behaviour of indexing each file as one
  /// whole-file document — set this to opt a knowledge entry into
  /// finer-grained, citable chunks.
  #[serde(default)]
  pub chunk_strategy: Option<String>,
  /// Target chunk size in characters. Only consulted when `chunk_strategy`
  /// is set. Defaults to 1000.
  #[serde(default)]
  pub chunk_size: Option<usize>,
  /// Overlap between consecutive chunks in characters. Only consulted when
  /// `chunk_strategy` is set. Defaults to 100.
  #[serde(default)]
  pub chunk_overlap: Option<usize>,
  /// OpenAI embedding model used by `chunk_strategy = "semantic"`.
  /// Supported: `"text-embedding-3-small"` (default), `"text-embedding-3-large"`,
  /// `"text-embedding-ada-002"`. Ignored for every other `chunk_strategy`.
  #[serde(default)]
  pub embedding_model: Option<String>,
  /// Similarity threshold (0.0-1.0) below which `chunk_strategy = "semantic"`
  /// starts a new chunk at a sentence boundary. Lower = more, smaller
  /// chunks; higher = fewer, larger chunks. Defaults to 0.6. Ignored for
  /// every other `chunk_strategy`.
  #[serde(default)]
  pub chunk_similarity_threshold: Option<f32>,
  /// Minimum sentence-segment size in characters for `chunk_strategy =
  /// "semantic"`'s sentence splitter. Defaults to 20. Ignored for every
  /// other `chunk_strategy`.
  #[serde(default)]
  pub chunk_min_segment_size: Option<usize>,
  /// Buffer percentile (0.0-1.0) `chunk_strategy = "semantic"` uses to
  /// dynamically derive its boundary threshold from the similarity
  /// distribution, instead of the fixed `chunk_similarity_threshold`.
  /// Defaults to 0.25. Ignored for every other `chunk_strategy`.
  #[serde(default)]
  pub chunk_buffer_percentile: Option<f32>,
  /// Query rewrite / decomposition strategy for the shared `rag_search`
  /// tool (L4.3): currently only `"split"` (deterministic decomposition on
  /// conjunctions/punctuation — see `yanshi_rag::rewrite::SplitQueryRewriter`)
  /// is supported. `None` (the default) preserves pre-L4.3 behaviour: the
  /// tool searches with the caller's query verbatim.
  ///
  /// All rag-tier `[[knowledge]]` entries share one backend / one
  /// `rag_search` tool (see `crate::builder::register_knowledge_backends`
  /// doc comment), so this is effectively a skill-wide setting: the first
  /// entry (in manifest order) that sets it wins. Skills with a single
  /// rag-tier entry — the common case — have no ambiguity here.
  #[serde(default)]
  pub query_rewrite: Option<String>,
}

impl KnowledgeConfig {
  /// Resolves the OpenAI embedding model for `chunk_strategy = "semantic"`,
  /// defaulting to `"text-embedding-3-small"` — same default and set of
  /// supported models as [`MemoryConfig::resolved_embedding_model`].
  pub fn resolved_embedding_model(&self) -> &str {
    self
      .embedding_model
      .as_deref()
      .unwrap_or("text-embedding-3-small")
  }

  /// Resolves `chunk_similarity_threshold`, defaulting to 0.6 — matches
  /// `SemanticChunkerBuilder`'s own default.
  pub fn resolved_chunk_similarity_threshold(&self) -> f32 {
    self.chunk_similarity_threshold.unwrap_or(0.6)
  }

  /// Resolves `chunk_min_segment_size`, defaulting to 20 — matches
  /// `SemanticChunkerBuilder`'s own default.
  pub fn resolved_chunk_min_segment_size(&self) -> usize {
    self.chunk_min_segment_size.unwrap_or(20)
  }

  /// Resolves `chunk_buffer_percentile`, defaulting to 0.25 — matches
  /// `SemanticChunkerBuilder`'s own default.
  pub fn resolved_chunk_buffer_percentile(&self) -> f32 {
    self.chunk_buffer_percentile.unwrap_or(0.25)
  }
}

/// Configures the memory backend for the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
  /// `"session"` (in-memory, lost on exit) | `"sqlite"` (persistent) |
  /// `"semantic"` (SQLite + embedding vectors) | `"none"`.
  #[serde(rename = "type")]
  pub memory_type: String,
  /// Path to the SQLite database file. Supports `~` expansion.
  /// Defaults to `~/.yanshi/memory/<skill_name>.db`.
  pub db_path: Option<String>,
  /// Maximum tokens to keep in the sliding window. Defaults to 8 000.
  pub window_tokens: Option<u32>,
  /// OpenAI embedding model used by `"semantic"` memory.
  /// Supported: `"text-embedding-3-small"` (default), `"text-embedding-3-large"`,
  /// `"text-embedding-ada-002"`.
  pub embedding_model: Option<String>,
  /// U2.2: `[memory.preference]` sub-table — durable per-user
  /// preferences, injected into the persona at prompt-assembly time and
  /// writable mid-conversation via the `remember_preference` tool.
  /// Independent of `memory_type` above; can be combined with any
  /// primary memory type (including `"none"`). See
  /// `docs/MEMORY_LAYERING.md` § Precedence at prompt-assembly time.
  #[serde(default)]
  pub preference: Option<PreferenceMemoryConfig>,
  /// L3.1/V1.6: `[memory.project]` sub-table — durable, per-project facts
  /// observed from `shell`/`script`/`code_exec` tool calls, injected into
  /// the persona at prompt-assembly time. Fully automatic on both ends (no
  /// tool the LLM calls to write it, unlike `preference` above). Only takes
  /// effect for callers that resolve a `project_root` and build via
  /// `SkillBuilder::build_with_project_root` — see that method's doc
  /// comment for why most `SkillBuilder` call sites don't have one.
  #[serde(default)]
  pub project: Option<ProjectMemoryConfig>,
}

impl MemoryConfig {
  pub fn resolved_window_tokens(&self) -> u32 {
    self.window_tokens.unwrap_or(8_000)
  }

  pub fn resolved_embedding_model(&self) -> &str {
    self
      .embedding_model
      .as_deref()
      .unwrap_or("text-embedding-3-small")
  }
}

/// `[memory.preference]` sub-table (U2.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreferenceMemoryConfig {
  /// Defaults to `true` — presence of the `[memory.preference]` table
  /// implies "on"; an override file can still disable it explicitly
  /// (`enabled = false`) without deleting the table.
  #[serde(default = "default_preference_enabled")]
  pub enabled: bool,
  /// Path to the SQLite database file. Supports `~` expansion. Defaults
  /// to `~/.yanshi/memory/<skill_name>.preference.db`.
  pub db_path: Option<String>,
}

fn default_preference_enabled() -> bool {
  true
}

/// `[memory.project]` sub-table (L3.1/V1.6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMemoryConfig {
  /// Defaults to `true` — presence of the `[memory.project]` table
  /// implies "on"; an override file can still disable it explicitly
  /// (`enabled = false`) without deleting the table.
  #[serde(default = "default_project_enabled")]
  pub enabled: bool,
  /// Path to the SQLite database file. Supports `~` expansion. Defaults
  /// to `~/.yanshi/memory/<skill_name>.project.db`.
  pub db_path: Option<String>,
}

fn default_project_enabled() -> bool {
  true
}
