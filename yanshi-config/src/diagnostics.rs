use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use yanshi_llm::{LLMConfig, LLMConfigSource, MODELS_CONFIG_ENV};
use yanshi_tools::sandbox::{
  SandboxBackend as _, SandboxEnforcement, code_exec_backend, default_backend,
};
use yanshi_tools::{SECURITY_PROFILE_ENV, SecurityProfile, SecurityProfileDefaults};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
  Text,
  /// Legacy bare `DoctorReport` JSON. Preserved for backward compat with
  /// existing consumers (the in-process `/v1/diagnostics` HTTP handler,
  /// CI tooling parsing the raw report). Slated to migrate to the
  /// envelope in v1.0; see `docs/CLI_JSON_OUTPUT.md`.
  Json,
  /// Canonical CLI JSON envelope (P3.3). Wraps the `DoctorReport` in
  /// `CliJsonEnvelope` so the field set is stable across commands. New
  /// JSON consumers should select this mode.
  JsonEnvelope,
}

impl OutputFormat {
  pub fn parse(value: &str) -> Result<Self> {
    match value {
      "text" => Ok(Self::Text),
      "json" => Ok(Self::Json),
      "json-envelope" => Ok(Self::JsonEnvelope),
      other => Err(anyhow::anyhow!(
        "unsupported doctor output format '{other}', expected 'text', 'json', or 'json-envelope'"
      )),
    }
  }
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
  version: &'static str,
  profile: DoctorProfile,
  features: FeatureReport,
  paths: PathReport,
  config: ConfigReport,
  security: SecurityReport,
  sandbox: SandboxReport,
  code_exec: CodeExecReport,
  /// Y2.3: the pricing table cost limits are enforced against.
  pricing: PricingReport,
  environment: EnvironmentReport,
  disk: DiskReport,
  #[serde(skip_serializing_if = "Option::is_none")]
  server: Option<ServerReport>,
  #[serde(skip_serializing_if = "Option::is_none")]
  backup_check: Option<BackupCheckReport>,
  /// Lite installation probe (P3.4): walks the local skills and plugins
  /// dirs, lists every MCP server command + plugin entrypoint, and
  /// checks whether each one resolves on PATH (or as a file). Only
  /// populated when `--check-installations` is set. Heavier transport-
  /// level reachability stays deferred until `yanshi mcp config`
  /// + plugin `dry_run` manifest entries land.
  #[serde(skip_serializing_if = "Option::is_none")]
  installations: Option<InstallationProbeReport>,
  status: DoctorStatus,
}

impl DoctorReport {
  /// Process exit code reflecting the overall status (0 = ok). The CLI command
  /// handler maps this onto `std::process::exit`.
  pub fn exit_code(&self) -> i32 {
    self.status.exit_code()
  }
}

#[derive(Debug, Serialize)]
pub struct InstallationProbeReport {
  pub skills_root: Option<PathBuf>,
  pub plugins_root: Option<PathBuf>,
  /// Where the top-level MCP config (`~/.yanshi/mcp.toml`) was
  /// loaded from. `None` when no such file exists / env-override
  /// resolved nothing. Populated by the P3.4-PR.3 wiring.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub mcp_config_source: Option<String>,
  pub mcp_servers: Vec<McpServerProbe>,
  pub plugins: Vec<PluginInstallProbe>,
}

#[derive(Debug, Serialize)]
pub struct McpServerProbe {
  /// Skill that declared the server. Absent (serialised as missing
  /// field) when the entry comes from the top-level `mcp.toml`
  /// registry rather than a skill manifest. Existing consumers
  /// who keyed on `.skill` for skill-declared servers see the same
  /// shape; consumers reading the new top-level entries learn the
  /// source via this field's presence + `InstallationProbeReport.
  /// mcp_config_source`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub skill: Option<String>,
  /// Server name as declared in the manifest.
  pub server: String,
  /// First command segment — what we attempt to resolve on PATH.
  pub command: String,
  /// `true` if the binary resolves on PATH or is a reachable absolute
  /// path. `false` means the operator will see a startup failure when
  /// the skill / config entry is invoked.
  pub reachable: bool,
}

#[derive(Debug, Serialize)]
pub struct PluginInstallProbe {
  pub name: String,
  pub version: String,
  pub entrypoint: PathBuf,
  /// `true` if the entrypoint exists at the resolved path. Surfaces
  /// stale installs whose binary was deleted. The dry-run smoke
  /// below validates the binary actually starts.
  pub entrypoint_exists: bool,
  /// Smoke outcome from running the manifest's `[plugin.dry_run]`
  /// invocation. `None` when the manifest didn't declare a
  /// `dry_run` block (operator opted out) — doctor reports the
  /// presence of the entrypoint without trying to spawn anything.
  /// `Some(report)` when the smoke ran; check `report.outcome` for
  /// pass / fail variants and `report.duration_ms` for the
  /// wall-clock cost.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub dry_run: Option<DryRunReport>,
}

/// Smoke-run result for a single plugin (P3.4-PR.3).
///
/// Mirrors `yanshi_core::plugin::DryRunOutcome` for the
/// wire-readable surface. `outcome` carries the discriminator the
/// status calculation keys on; `duration_ms` lets operators
/// distinguish "fast pass" from "slow pass" without re-running.
#[derive(Debug, Serialize)]
pub struct DryRunReport {
  pub duration_ms: u64,
  pub outcome: DryRunOutcomeReport,
}

/// JSON-shaped projection of `yanshi_core::plugin::DryRunOutcome`
/// (feature-gated behind `plugin`).
///
/// Discriminator: `"status"` — `"passed"` for success, `"failed"`
/// for any negative outcome (with `kind` distinguishing the failure
/// mode). Operators consuming the doctor JSON can branch on
/// `status` alone for the binary pass/fail check.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DryRunOutcomeReport {
  Passed {
    exit_code: i32,
  },
  Failed {
    #[serde(flatten)]
    kind: DryRunFailureKind,
  },
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DryRunFailureKind {
  WrongExitCode {
    expected: i32,
    actual: i32,
  },
  KilledBySignal {
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
  },
  Timeout {
    timeout_ms: u32,
  },
  SpawnFailed {
    reason: String,
  },
}

/// Backup-readiness report populated only when `--backup-check` is supplied.
/// Walks every workspace state directory an operator would need to back up
/// or restore and probes that each is present and writable. Extends the
/// existing `DiskReport` (which only covers run/trace/marketplace) with the
/// skills and plugins install dirs. See `docs/SERVER_BACKUP_RESTORE.md`
/// for the rationale behind the dir set.
#[derive(Debug, Serialize)]
pub struct BackupCheckReport {
  run_dir: DirCheck,
  trace_dir: DirCheck,
  marketplace_cache: DirCheck,
  skills_dir: DirCheck,
  plugins_dir: DirCheck,
}

#[derive(Debug, Serialize)]
pub struct FeatureReport {
  rag: bool,
  plugin: bool,
  mcp_workflow_nodes: bool,
}

#[derive(Debug, Serialize)]
pub struct PathReport {
  home: Option<PathBuf>,
  config_dir: Option<PathBuf>,
  models_config: Option<PathBuf>,
  legacy_models_config: Option<PathBuf>,
  env_file: Option<PathBuf>,
  skills_dir: Option<PathBuf>,
  plugins_dir: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct ConfigReport {
  /// Stable machine-readable kind of the resolved source (snake_case
  /// via [`LLMConfigSourceKind`]'s serde rename). Replaces the older
  /// Rust-debug-formatted `"UserModelsYml"` string for programmatic
  /// consumers.
  models_config_source_kind: yanshi_llm::LLMConfigSourceKind,
  /// Human-readable description: `"~/.yanshi/models.yml (overrides built-in)"`
  /// when a user file shadows the bundled defaults, `"built-in default_models.yml"`
  /// when nothing on disk is in effect. Designed for text output / `doctor`
  /// readers who shouldn't have to translate the enum themselves.
  ///
  /// F-A7-4: A7's dogfooding caught the silent override only by grep;
  /// this label makes the shadowing visible without leaving the
  /// doctor output.
  models_config_source_label: String,
  /// Legacy debug-formatted enum name (e.g. `"UserModelsYml"`). Kept
  /// for back-compat with anything that pinned to the prior wire
  /// shape — new consumers should prefer `models_config_source_kind`.
  models_config_source: String,
  models_config_path: String,
  models_config_exists: bool,
  models_config_loadable: bool,
  models: usize,
  providers: usize,
  missing_env_vars: Vec<String>,
  warnings: Vec<String>,
  error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SecurityReport {
  env_var: &'static str,
  profile: SecurityProfile,
  defaults: SecurityProfileDefaults,
  warning: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SandboxReport {
  backend: &'static str,
  /// Tri-state enforcement (`enforcing` / `permissive` / `disabled`). Operators
  /// reading the JSON output can distinguish "no platform backend on this OS"
  /// (`disabled`) from "backend exists but cannot enforce right now"
  /// (`permissive`, e.g. missing `sandbox-exec`, unsupported arch).
  enforcement: SandboxEnforcement,
  /// Kept for backwards-compatible JSON consumers. Equivalent to
  /// `enforcement == "enforcing"`.
  enforcing: bool,
  capabilities: Vec<&'static str>,
  warnings: Vec<String>,
}

/// S4.2: the `ContainerBackend` `code_exec` uses — a separate, stronger
/// isolation tier from [`SandboxReport`]'s OS-sandbox backend above, so
/// it's reported independently rather than folded in. Purely informational
/// (does not affect [`DoctorStatus`]): `code_exec` is opt-in per skill
/// manifest, so a non-enforcing container engine here only matters to a
/// skill that actually declares the tool, which `doctor`'s host-wide scope
/// has no visibility into.
#[derive(Debug, Serialize)]
pub struct CodeExecReport {
  backend: &'static str,
  enforcement: SandboxEnforcement,
  enforcing: bool,
}

#[derive(Debug, Serialize)]
pub struct EnvironmentReport {
  yanshi_run_dir: Option<String>,
  yanshi_trace_dir: Option<String>,
  yanshi_api_token_set: bool,
  yanshi_skills_index: Option<String>,
}

/// Filesystem reachability report for the workspace state dirs that
/// Yanshi writes during execution. The byte-cost check is
/// deliberately coarse: we look up "is the directory present, and is
/// it writable" rather than running platform-specific `statvfs`. The
/// 80 % case for operators is "did I forget to mount the run-dir
/// volume" — that case is fully covered without a new dependency.
#[derive(Debug, Serialize)]
pub struct DiskReport {
  run_dir: DirCheck,
  trace_dir: DirCheck,
  marketplace_cache: DirCheck,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirCheck {
  /// Resolved path (override → env → default).
  path: String,
  /// Stable identifier of the source (`override`, `env`, `default`).
  source: &'static str,
  /// `true` when the directory exists and is a directory.
  exists: bool,
  /// `true` when a probe-file create + remove succeeded under the dir.
  writable: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<String>,
}

/// Server reachability report populated only when `--server <url>` is
/// supplied. Issues a `GET <url>/health` with a 3 s timeout and
/// records the HTTP status code.
#[derive(Debug, Serialize)]
pub struct ServerReport {
  url: String,
  reachable: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  status_code: Option<u16>,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<String>,
}

/// Tri-state doctor verdict. `yanshi doctor` exits with the
/// corresponding `Self::exit_code` so CI gates can branch on it
/// (`P3.4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
  Ok,
  Warning,
  Fail,
}

impl DoctorStatus {
  pub fn exit_code(self) -> i32 {
    match self {
      Self::Ok => 0,
      Self::Warning => 1,
      Self::Fail => 2,
    }
  }

  fn promote(&mut self, other: Self) {
    if other.rank() > self.rank() {
      *self = other;
    }
  }

  fn rank(self) -> u8 {
    match self {
      Self::Ok => 0,
      Self::Warning => 1,
      Self::Fail => 2,
    }
  }
}

/// Pass/fail threshold profile chosen via `--profile`. Sticks to
/// `local` by default so the legacy behaviour (warn but exit 0… now
/// warn = exit 1) stays close to what existing users expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorProfile {
  /// Most lenient. Missing models config and missing API keys stay
  /// at `Warning` and never escalate to `Fail`.
  Dev,
  /// Default. Matches the security model `local` profile.
  Local,
  /// Strictest. Missing API keys, missing API token, missing
  /// marketplace cache, and missing run/trace dirs escalate to
  /// `Fail`.
  Production,
}

impl DoctorProfile {
  pub fn parse(value: &str) -> Result<Self> {
    match value {
      "dev" => Ok(Self::Dev),
      "local" => Ok(Self::Local),
      "production" => Ok(Self::Production),
      other => Err(anyhow::anyhow!(
        "unsupported --profile '{other}', expected dev | local | production"
      )),
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      Self::Dev => "dev",
      Self::Local => "local",
      Self::Production => "production",
    }
  }
}

/// Build the structured doctor report. The caller selects the `profile`, an
/// optional `--server` URL to probe, and whether to run the backup /
/// installation checks; `top_level_mcp` is the CLI-injected `mcp.toml` probe
/// result (empty for the server).
pub async fn build_report(
  profile: DoctorProfile,
  server: Option<&str>,
  backup_check: bool,
  check_installations: bool,
  // The top-level MCP registry probe `(source, servers)`. Injected by the
  // caller because the probe reads the CLI's `mcp.toml` config format, which
  // lives in `yanshi-cli`; the server passes `(None, vec![])`.
  top_level_mcp: (Option<String>, Vec<McpServerProbe>),
) -> DoctorReport {
  let home = dirs::home_dir();
  let config_dir = home.as_ref().map(|p| p.join(".yanshi"));
  let resolved_source = LLMConfig::resolve_default_source().ok();
  let models_config = resolved_source
    .as_ref()
    .and_then(|source| source.path.clone());
  let legacy_models_config = config_dir.as_ref().map(|p| p.join("models.yaml"));
  let env_file = config_dir.as_ref().map(|p| p.join(".env"));
  let skills_dir = home.as_ref().map(|p| p.join(".yanshi").join("skills"));
  let plugins_dir = home.as_ref().map(|p| p.join(".yanshi").join("plugins"));

  let config = match resolved_source.as_ref() {
    Some(source) => inspect_config(source, env_file.as_deref()).await,
    None => ConfigReport {
      models_config_source_kind: yanshi_llm::LLMConfigSourceKind::BuiltInDefault,
      models_config_source_label: "unknown (no home directory)".to_string(),
      models_config_source: "unknown".to_string(),
      models_config_path: "unknown".to_string(),
      models_config_exists: false,
      models_config_loadable: false,
      models: 0,
      providers: 0,
      missing_env_vars: Vec::new(),
      warnings: Vec::new(),
      error: Some("could not determine home directory".to_string()),
    },
  };

  let sandbox_backend = default_backend();
  let enforcement = sandbox_backend.enforcement_level();
  let sandbox = SandboxReport {
    backend: sandbox_backend.name(),
    enforcement,
    enforcing: enforcement.is_enforcing(),
    capabilities: sandbox_capabilities(enforcement.is_enforcing()),
    warnings: sandbox_warnings(sandbox_backend.name(), enforcement),
  };

  // S4.2: probing this spawns a short `<engine> --version` check (unlike
  // `default_backend()` above, which is hermetic) — acceptable for a
  // once-per-`doctor`-run diagnostic.
  let code_exec_backend = code_exec_backend();
  let code_exec_enforcement = code_exec_backend.enforcement_level();
  let code_exec = CodeExecReport {
    backend: code_exec_backend.name(),
    enforcement: code_exec_enforcement,
    enforcing: code_exec_enforcement.is_enforcing(),
  };

  let security = security_report();
  let disk = disk_report(home.as_deref());
  let server_report = match server {
    Some(url) => Some(probe_server(url).await),
    None => None,
  };

  let pricing = pricing_report();

  let mut status = DoctorStatus::Ok;

  // Y2.3: a malformed pricing file fails every billed node.
  if pricing.error.is_some() {
    status.promote(DoctorStatus::Warning);
  }

  // Config / API keys.
  if config.error.is_some() {
    status.promote(DoctorStatus::Warning);
  }
  if !config.missing_env_vars.is_empty() {
    status.promote(match profile {
      DoctorProfile::Production => DoctorStatus::Fail,
      _ => DoctorStatus::Warning,
    });
  }

  // Sandbox.
  if !sandbox.enforcing {
    let level = if matches!(profile, DoctorProfile::Production) {
      DoctorStatus::Fail
    } else {
      DoctorStatus::Warning
    };
    status.promote(level);
  }

  // Security warnings.
  if security.warning.is_some() {
    status.promote(DoctorStatus::Warning);
  }

  // Disk reachability.
  for check in [&disk.run_dir, &disk.trace_dir, &disk.marketplace_cache] {
    if !check.exists {
      status.promote(DoctorStatus::Warning);
    } else if !check.writable {
      status.promote(match profile {
        DoctorProfile::Production => DoctorStatus::Fail,
        _ => DoctorStatus::Warning,
      });
    }
  }

  // Server reachability (only when explicitly probed).
  if let Some(report) = server_report.as_ref()
    && !report.reachable
  {
    status.promote(DoctorStatus::Fail);
  }

  // Backup-readiness section (only populated when --backup-check is set).
  let backup_check_report = if backup_check {
    let skills = resolve_dir(home.as_deref(), "YANSHI_SKILLS_DIR", &["skills"]);
    let plugins = resolve_dir(home.as_deref(), "YANSHI_PLUGINS_DIR", &["plugins"]);
    let report = BackupCheckReport {
      run_dir: disk.run_dir.clone(),
      trace_dir: disk.trace_dir.clone(),
      marketplace_cache: disk.marketplace_cache.clone(),
      skills_dir: skills,
      plugins_dir: plugins,
    };
    for check in [
      &report.run_dir,
      &report.trace_dir,
      &report.marketplace_cache,
      &report.skills_dir,
      &report.plugins_dir,
    ] {
      if !check.exists {
        status.promote(match profile {
          DoctorProfile::Production => DoctorStatus::Fail,
          _ => DoctorStatus::Warning,
        });
      } else if !check.writable {
        status.promote(DoctorStatus::Fail);
      }
    }
    Some(report)
  } else {
    None
  };

  // P3.4 lite installation probe — opt-in via --check-installations.
  // Inventories installed skills/plugins and surfaces unreachable
  // command binaries / missing entrypoints. Heavier transport-level
  // checks stay deferred until the prereqs land (see doctor docs).
  let installations_report = if check_installations {
    Some(probe_installations(home.as_deref(), top_level_mcp).await)
  } else {
    None
  };
  if let Some(probe) = installations_report.as_ref() {
    for server_probe in &probe.mcp_servers {
      if !server_probe.reachable {
        status.promote(match profile {
          DoctorProfile::Production => DoctorStatus::Fail,
          _ => DoctorStatus::Warning,
        });
      }
    }
    for plugin_probe in &probe.plugins {
      if !plugin_probe.entrypoint_exists {
        status.promote(match profile {
          DoctorProfile::Production => DoctorStatus::Fail,
          _ => DoctorStatus::Warning,
        });
        continue;
      }
      // P3.4-PR.3: a dry-run smoke that exists but failed (timed
      // out / wrong exit / killed / spawn error) promotes the
      // overall status, same as a missing entrypoint. A plugin
      // without a configured dry_run leaves status untouched —
      // operators opted out of the smoke.
      if let Some(report) = &plugin_probe.dry_run
        && matches!(report.outcome, DryRunOutcomeReport::Failed { .. })
      {
        status.promote(match profile {
          DoctorProfile::Production => DoctorStatus::Fail,
          _ => DoctorStatus::Warning,
        });
      }
    }
  }

  DoctorReport {
    version: env!("CARGO_PKG_VERSION"),
    profile,
    features: FeatureReport {
      rag: cfg!(feature = "rag"),
      plugin: cfg!(feature = "plugin"),
      mcp_workflow_nodes: cfg!(feature = "mcp"),
    },
    paths: PathReport {
      home,
      config_dir,
      models_config,
      legacy_models_config,
      env_file,
      skills_dir,
      plugins_dir,
    },
    config,
    security,
    sandbox,
    code_exec,
    pricing,
    environment: EnvironmentReport {
      yanshi_run_dir: std::env::var("YANSHI_RUN_DIR").ok(),
      yanshi_trace_dir: std::env::var("YANSHI_TRACE_DIR").ok(),
      yanshi_api_token_set: std::env::var("YANSHI_API_TOKEN").is_ok(),
      yanshi_skills_index: std::env::var("YANSHI_SKILLS_INDEX").ok(),
    },
    disk,
    server: server_report,
    backup_check: backup_check_report,
    installations: installations_report,
    status,
  }
}

/// Y2.3: where the pricing table comes from and whether it loads.
#[derive(Debug, Serialize)]
pub struct PricingReport {
  /// `YANSHI_PRICING_TABLE=<path>`, the `~/.yanshi/pricing.yml` path, or
  /// `none`.
  source: String,
  /// Models with an explicit price row.
  models: usize,
  /// Whether a fallback `default:` row exists.
  has_default: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<String>,
}

fn pricing_report() -> PricingReport {
  let source = match std::env::var("YANSHI_PRICING_TABLE") {
    Ok(path) => format!("YANSHI_PRICING_TABLE={path}"),
    Err(_) => match dirs::home_dir().map(|home| home.join(".yanshi").join("pricing.yml")) {
      Some(path) if path.exists() => path.display().to_string(),
      _ => "none".to_string(),
    },
  };
  match yanshi_llm::pricing::PricingTable::load_default() {
    Ok(table) => PricingReport {
      source,
      models: table.models.len(),
      has_default: table.default.is_some(),
      error: None,
    },
    Err(error) => PricingReport {
      source,
      models: 0,
      has_default: false,
      error: Some(error.to_string()),
    },
  }
}

/// Walk `~/.yanshi/skills/*/` and `~/.yanshi/plugins/*/` (or the
/// env-overridden roots) and inventory their declared MCP servers +
/// plugin entrypoints. Returns the structured report the doctor JSON
/// surfaces under `installations`.
async fn probe_installations(
  home: Option<&Path>,
  top_level_mcp: (Option<String>, Vec<McpServerProbe>),
) -> InstallationProbeReport {
  let skills_root = resolve_install_root(home, "YANSHI_SKILLS_DIR", "skills");
  let plugins_root = resolve_install_root(home, "YANSHI_PLUGINS_DIR", "plugins");

  let mut mcp_servers = match skills_root.as_ref() {
    Some(root) => probe_mcp_servers(root),
    None => Vec::new(),
  };

  // P3.4-PR.3: also probe servers configured in the top-level
  // ~/.yanshi/mcp.toml (or YANSHI_MCP_CONFIG override). These
  // appear in the same `mcp_servers` list with `skill: None` so
  // existing consumers see one unified collection; the new
  // `mcp_config_source` field at the report level documents where
  // the top-level entries came from.
  let (mcp_config_source, top_level_probes) = top_level_mcp;
  mcp_servers.extend(top_level_probes);

  let plugins = match plugins_root.as_ref() {
    Some(root) => probe_plugin_installs(root).await,
    None => Vec::new(),
  };

  InstallationProbeReport {
    skills_root,
    plugins_root,
    mcp_config_source,
    mcp_servers,
    plugins,
  }
}

fn resolve_install_root(home: Option<&Path>, env_var: &str, default_tail: &str) -> Option<PathBuf> {
  if let Ok(value) = std::env::var(env_var) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
      return Some(PathBuf::from(trimmed));
    }
  }
  home.map(|h| h.join(".yanshi").join(default_tail))
}

fn probe_mcp_servers(skills_root: &Path) -> Vec<McpServerProbe> {
  use yanshi_skills::SkillLoader;
  let mut out = Vec::new();
  let Ok(entries) = std::fs::read_dir(skills_root) else {
    return out;
  };
  for entry in entries.flatten() {
    let dir = entry.path();
    if !dir.is_dir() {
      continue;
    }
    let Ok(manifest) = SkillLoader::load(&dir) else {
      continue;
    };
    for server in &manifest.mcp_servers {
      let cmd = server.command.trim();
      // The configured command might already be an absolute path, or
      // a bare name to resolve on PATH. `which` handles both.
      let reachable = if cmd.is_empty() {
        false
      } else {
        which::which(cmd).is_ok() || std::path::Path::new(cmd).is_file()
      };
      out.push(McpServerProbe {
        skill: Some(manifest.skill.name.clone()),
        server: server.name.clone(),
        command: cmd.to_string(),
        reachable,
      });
    }
  }
  out
}

#[cfg(feature = "plugin")]
async fn probe_plugin_installs(plugins_root: &Path) -> Vec<PluginInstallProbe> {
  use yanshi_core::plugin::{
    DryRunFailure as CoreDryRunFailure, DryRunOutcome as CoreDryRunOutcome, PluginManifest,
    run_dry_run,
  };
  let mut out = Vec::new();
  let Ok(entries) = std::fs::read_dir(plugins_root) else {
    return out;
  };
  for entry in entries.flatten() {
    let dir = entry.path();
    if !dir.is_dir() {
      continue;
    }
    let manifest_path = dir.join("plugin.toml");
    if !manifest_path.is_file() {
      continue;
    }
    let Ok((manifest, _)) = PluginManifest::load_from_path(&manifest_path) else {
      continue;
    };
    let resolved = manifest.resolve_entrypoint(&dir);
    let entrypoint_exists = resolved.exists();

    // P3.4-PR.3: run the manifest's `[plugin.dry_run]` smoke when
    // configured. Only run when the entrypoint actually exists —
    // SpawnFailed would otherwise dominate the report for missing
    // binaries, and the entrypoint_exists field already covers that
    // case explicitly.
    let dry_run = if entrypoint_exists && manifest.plugin.dry_run.is_some() {
      let started_at = std::time::Instant::now();
      let outcome = run_dry_run(&manifest, &dir).await;
      let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
      let outcome_report = match outcome {
        CoreDryRunOutcome::Skipped => {
          // Shouldn't hit this branch since we gated on
          // `dry_run.is_some()` above, but treat it defensively.
          None
        }
        CoreDryRunOutcome::Passed { exit_code } => Some(DryRunOutcomeReport::Passed { exit_code }),
        CoreDryRunOutcome::Failed(failure) => Some(DryRunOutcomeReport::Failed {
          kind: match failure {
            CoreDryRunFailure::WrongExitCode { expected, actual } => {
              DryRunFailureKind::WrongExitCode { expected, actual }
            }
            CoreDryRunFailure::KilledBySignal { signal } => {
              DryRunFailureKind::KilledBySignal { signal }
            }
            CoreDryRunFailure::Timeout { timeout_ms } => DryRunFailureKind::Timeout { timeout_ms },
            CoreDryRunFailure::SpawnFailed { reason } => DryRunFailureKind::SpawnFailed { reason },
          },
        }),
      };
      outcome_report.map(|outcome| DryRunReport {
        duration_ms,
        outcome,
      })
    } else {
      None
    };

    out.push(PluginInstallProbe {
      name: manifest.plugin.name.clone(),
      version: manifest.plugin.version.clone(),
      entrypoint: resolved.clone(),
      entrypoint_exists,
      dry_run,
    });
  }
  out
}

#[cfg(not(feature = "plugin"))]
async fn probe_plugin_installs(_plugins_root: &Path) -> Vec<PluginInstallProbe> {
  // Without the `plugin` feature the binary doesn't know how to parse
  // a `plugin.toml`. The doctor still reports the configured plugins
  // dir under `installations.plugins_root`, just with an empty list.
  Vec::new()
}

fn disk_report(home: Option<&Path>) -> DiskReport {
  let run_dir = resolve_dir(home, "YANSHI_RUN_DIR", &["runs"]);
  let trace_dir = resolve_dir(home, "YANSHI_TRACE_DIR", &["traces"]);
  let marketplace_cache = resolve_dir(home, "YANSHI_MARKETPLACE_CACHE", &["marketplace", "cache"]);
  DiskReport {
    run_dir,
    trace_dir,
    marketplace_cache,
  }
}

fn resolve_dir(home: Option<&Path>, env_var: &str, default_tail: &[&str]) -> DirCheck {
  let (path, source) =
    if let Some(value) = std::env::var(env_var).ok().filter(|v| !v.trim().is_empty()) {
      (PathBuf::from(value), "env")
    } else if let Some(home) = home {
      let mut p = home.join(".yanshi");
      for segment in default_tail {
        p.push(segment);
      }
      (p, "default")
    } else {
      (PathBuf::from("<unknown>"), "default")
    };

  let exists = path.is_dir();
  let writable = if exists { probe_writable(&path) } else { false };
  let error = if !exists {
    Some("directory does not exist; will be created on first write".to_string())
  } else if !writable {
    Some("directory exists but write probe failed".to_string())
  } else {
    None
  };
  DirCheck {
    path: path.display().to_string(),
    source,
    exists,
    writable,
    error,
  }
}

fn probe_writable(dir: &Path) -> bool {
  let probe = dir.join(format!(".yanshi-doctor-probe-{}", std::process::id()));
  match std::fs::write(&probe, b"probe") {
    Ok(()) => {
      let _ = std::fs::remove_file(&probe);
      true
    }
    Err(_) => false,
  }
}

async fn probe_server(url: &str) -> ServerReport {
  let trimmed = url.trim_end_matches('/');
  let health = format!("{trimmed}/health");
  let client = match reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(3))
    .build()
  {
    Ok(client) => client,
    Err(err) => {
      return ServerReport {
        url: url.to_string(),
        reachable: false,
        status_code: None,
        error: Some(format!("http client init failed: {err}")),
      };
    }
  };
  match client.get(&health).send().await {
    Ok(response) => {
      let code = response.status().as_u16();
      let ok = response.status().is_success();
      ServerReport {
        url: url.to_string(),
        reachable: ok,
        status_code: Some(code),
        error: if ok {
          None
        } else {
          Some(format!("non-success HTTP status {code}"))
        },
      }
    }
    Err(err) => ServerReport {
      url: url.to_string(),
      reachable: false,
      status_code: None,
      error: Some(err.to_string()),
    },
  }
}

fn security_report() -> SecurityReport {
  match SecurityProfile::from_env() {
    Ok(profile) => SecurityReport {
      env_var: SECURITY_PROFILE_ENV,
      profile,
      defaults: profile.defaults(),
      warning: None,
    },
    Err(err) => {
      let profile = SecurityProfile::default();
      SecurityReport {
        env_var: SECURITY_PROFILE_ENV,
        profile,
        defaults: profile.defaults(),
        warning: Some(format!(
          "{SECURITY_PROFILE_ENV} is invalid ({err}); falling back to '{profile}' for diagnostics"
        )),
      }
    }
  }
}

/// Render an `LLMConfigSource` into the human-friendly label surfaced
/// by `yanshi doctor` (F-A7-4). The goal is to make the "user file
/// is shadowing the bundled default" case immediately visible in
/// text output, since A7's dogfooding only caught the silent override
/// by grepping the codebase.
fn source_label(source: &LLMConfigSource) -> String {
  use yanshi_llm::LLMConfigSourceKind as K;
  match source.kind {
    K::BuiltInDefault => "built-in default_models.yml".to_string(),
    K::UserModelsYml => format!("{} (overrides built-in)", source.display_path()),
    K::UserModelsYaml => format!("{} (overrides built-in)", source.display_path()),
    K::EnvOverride => format!(
      "{} (via YANSHI_MODELS_CONFIG, overrides ~/.yanshi + built-in)",
      source.display_path()
    ),
    K::UserModelsDir => format!(
      "{} (directory-based config, overrides ~/.yanshi + built-in)",
      source.display_path()
    ),
  }
}

async fn inspect_config(source: &LLMConfigSource, env_path: Option<&Path>) -> ConfigReport {
  let source_kind = source.kind;
  let source_name = format!("{:?}", source.kind);
  let source_path = source.display_path();
  let source_label = source_label(source);
  let Some(path) = source.path.as_ref() else {
    return match LLMConfig::from_default_source().await {
      Ok((config, _)) => ConfigReport {
        models_config_source_kind: source_kind,
        models_config_source_label: source_label.clone(),
        models_config_source: source_name.clone(),
        models_config_path: source_path.clone(),
        models_config_exists: true,
        models_config_loadable: true,
        models: config.models.len(),
        providers: config.providers.len(),
        missing_env_vars: Vec::new(),
        warnings: source.warnings.clone(),
        error: None,
      },
      Err(e) => ConfigReport {
        models_config_source_kind: source_kind,
        models_config_source_label: source_label,
        models_config_source: source_name,
        models_config_path: source_path,
        models_config_exists: false,
        models_config_loadable: false,
        models: 0,
        providers: 0,
        missing_env_vars: Vec::new(),
        warnings: source.warnings.clone(),
        error: Some(e.to_string()),
      },
    };
  };

  if !path.exists() {
    return ConfigReport {
      models_config_source_kind: source_kind,
      models_config_source_label: source_label,
      models_config_source: source_name,
      models_config_path: source_path,
      models_config_exists: false,
      models_config_loadable: false,
      models: 0,
      providers: 0,
      missing_env_vars: Vec::new(),
      warnings: source.warnings.clone(),
      error: Some(format!(
        "{} not found; run `yanshi config init` or set {MODELS_CONFIG_ENV}",
        path.display()
      )),
    };
  }

  // `from_source` (not `from_file`) so a `UserModelsDir` source (path is
  // a directory laid out for `VendorConfigManager`, not a single file)
  // loads correctly instead of erroring on "is a directory".
  match LLMConfig::from_source(source).await {
    Ok(config) => {
      let configured_env = env_path
        .map(load_env_file_keys)
        .transpose()
        .unwrap_or_default()
        .unwrap_or_default();
      let mut missing_env_vars = Vec::new();
      for provider in config.providers.values() {
        if std::env::var(&provider.api_key_env).is_err()
          && !configured_env.contains(&provider.api_key_env)
        {
          missing_env_vars.push(provider.api_key_env.clone());
        }
      }
      missing_env_vars.sort();
      missing_env_vars.dedup();

      ConfigReport {
        models_config_source_kind: source_kind,
        models_config_source_label: source_label,
        models_config_source: source_name,
        models_config_path: source_path,
        models_config_exists: true,
        models_config_loadable: true,
        models: config.models.len(),
        providers: config.providers.len(),
        missing_env_vars,
        warnings: source.warnings.clone(),
        error: None,
      }
    }
    Err(e) => ConfigReport {
      models_config_source_kind: source_kind,
      models_config_source_label: source_label,
      models_config_source: source_name,
      models_config_path: source_path,
      models_config_exists: true,
      models_config_loadable: false,
      models: 0,
      providers: 0,
      missing_env_vars: Vec::new(),
      warnings: source.warnings.clone(),
      error: Some(e.to_string()),
    },
  }
}

fn load_env_file_keys(path: &Path) -> Result<std::collections::BTreeSet<String>> {
  if !path.exists() {
    return Ok(std::collections::BTreeSet::new());
  }
  let content = std::fs::read_to_string(path)
    .with_context(|| format!("failed to read env file '{}'", path.display()))?;
  Ok(content.lines().filter_map(parse_env_key).collect())
}

fn parse_env_key(line: &str) -> Option<String> {
  let trimmed = line.trim();
  if trimmed.is_empty() || trimmed.starts_with('#') {
    return None;
  }
  let (key, value) = trimmed.split_once('=')?;
  let key = key.trim();
  let value = value.trim().trim_matches('"').trim_matches('\'');
  if key.is_empty() || value.is_empty() {
    return None;
  }
  Some(key.to_string())
}

pub fn print_text_report(report: &DoctorReport) {
  println!("Yanshi doctor");
  println!("Version: {}", report.version);
  println!("Status: {}", status_label(&report.status));
  println!();

  println!("Features:");
  println!("  rag: {}", enabled_label(report.features.rag));
  println!("  plugin: {}", enabled_label(report.features.plugin));
  println!(
    "  mcp workflow nodes: {}",
    enabled_label(report.features.mcp_workflow_nodes)
  );
  println!();

  println!("Paths:");
  println!("  home: {}", optional_path(report.paths.home.as_deref()));
  println!(
    "  config: {}",
    optional_path(report.paths.models_config.as_deref())
  );
  println!(
    "  legacy config: {}",
    optional_path(report.paths.legacy_models_config.as_deref())
  );
  println!(
    "  skills: {}",
    optional_path(report.paths.skills_dir.as_deref())
  );
  println!(
    "  plugins: {}",
    optional_path(report.paths.plugins_dir.as_deref())
  );
  println!();

  println!("Config:");
  // F-A7-4: lead with the human-readable label so users can see at a
  // glance whether their `~/.yanshi/models.yml` is shadowing the
  // bundled defaults. The legacy enum-debug `source` line below is
  // preserved for diff-stability with consumers that grep for it.
  println!("  source: {}", report.config.models_config_source_label);
  println!(
    "  source (kind): {:?}",
    report.config.models_config_source_kind
  );
  println!("  path: {}", report.config.models_config_path);
  println!(
    "  models config: {}",
    if report.config.models_config_exists {
      "found"
    } else {
      "missing"
    }
  );
  println!(
    "  loadable: {}",
    enabled_label(report.config.models_config_loadable)
  );
  println!("  models: {}", report.config.models);
  println!("  providers: {}", report.config.providers);
  if report.config.missing_env_vars.is_empty() {
    println!("  missing env vars: none");
  } else {
    println!(
      "  missing env vars: {}",
      report.config.missing_env_vars.join(", ")
    );
  }
  if let Some(error) = &report.config.error {
    println!("  warning: {error}");
  }
  for warning in &report.config.warnings {
    println!("  warning: {warning}");
  }
  println!();

  println!("Security:");
  println!("  profile: {}", report.security.profile);
  println!("  env var: {}", report.security.env_var);
  println!(
    "  auth token required: {}",
    enabled_label(report.security.defaults.auth.require_api_token)
  );
  println!("  cors: {:?}", report.security.defaults.cors.mode);
  println!(
    "  max request body: {} bytes",
    report
      .security
      .defaults
      .request_limits
      .max_request_body_bytes
  );
  println!(
    "  os sandbox required: {}",
    enabled_label(report.security.defaults.sandboxing.require_os_sandbox)
  );
  println!(
    "  subprocess plugins: {}",
    enabled_label(report.security.defaults.plugins.allow_subprocess_plugins)
  );
  println!(
    "  marketplace signatures: {}",
    enabled_label(
      report
        .security
        .defaults
        .marketplace
        .require_signature_verification
    )
  );
  println!(
    "  worker admission credentials required: {}",
    enabled_label(
      report
        .security
        .defaults
        .worker_admission
        .require_credential_config
    )
  );
  if let Some(warning) = &report.security.warning {
    println!("  warning: {warning}");
  }
  println!();

  println!("Sandbox:");
  println!("  backend: {}", report.sandbox.backend);
  println!("  enforcement: {}", report.sandbox.enforcement.as_str());
  println!("  enforcing: {}", enabled_label(report.sandbox.enforcing));
  println!(
    "  capabilities: {}",
    if report.sandbox.capabilities.is_empty() {
      "none".to_string()
    } else {
      report.sandbox.capabilities.join(", ")
    }
  );
  for warning in &report.sandbox.warnings {
    println!("  warning: {warning}");
  }
  println!();

  println!("Code-exec isolation (code_exec tool, opt-in per skill):");
  println!("  backend: {}", report.code_exec.backend);
  println!("  enforcement: {}", report.code_exec.enforcement.as_str());
  println!("  enforcing: {}", enabled_label(report.code_exec.enforcing));
  println!();

  println!("Pricing (cost_limit_usd is enforced against this):");
  println!("  source: {}", report.pricing.source);
  println!("  priced models: {}", report.pricing.models);
  println!(
    "  default price: {}",
    enabled_label(report.pricing.has_default)
  );
  if let Some(error) = &report.pricing.error {
    println!("  error: {error}");
  } else if report.pricing.models == 0 && !report.pricing.has_default {
    println!("  note: every model costs $0, so cost limits never trip");
  }
  println!();

  println!("Environment:");
  println!(
    "  YANSHI_RUN_DIR: {}",
    optional_env(report.environment.yanshi_run_dir.as_deref())
  );
  println!(
    "  YANSHI_TRACE_DIR: {}",
    optional_env(report.environment.yanshi_trace_dir.as_deref())
  );
  println!(
    "  YANSHI_API_TOKEN: {}",
    if report.environment.yanshi_api_token_set {
      "set"
    } else {
      "unset"
    }
  );
  println!(
    "  YANSHI_SKILLS_INDEX: {}",
    optional_env(report.environment.yanshi_skills_index.as_deref())
  );
  println!();

  println!("Disk:");
  print_dir_check("run dir", &report.disk.run_dir);
  print_dir_check("trace dir", &report.disk.trace_dir);
  print_dir_check("marketplace cache", &report.disk.marketplace_cache);
  println!();

  if let Some(server) = &report.server {
    println!("Server:");
    println!("  url: {}", server.url);
    println!("  reachable: {}", enabled_label(server.reachable));
    if let Some(code) = server.status_code {
      println!("  status: {code}");
    }
    if let Some(err) = &server.error {
      println!("  error: {err}");
    }
    println!();
  }

  if let Some(backup) = &report.backup_check {
    println!("Backup check:");
    print_dir_check("run dir", &backup.run_dir);
    print_dir_check("trace dir", &backup.trace_dir);
    print_dir_check("marketplace cache", &backup.marketplace_cache);
    print_dir_check("skills dir", &backup.skills_dir);
    print_dir_check("plugins dir", &backup.plugins_dir);
    println!();
  }

  println!("Profile: {}", report.profile.as_str());
}

fn print_dir_check(label: &str, check: &DirCheck) {
  let status = match (check.exists, check.writable) {
    (true, true) => "ok",
    (true, false) => "read-only",
    (false, _) => "missing",
  };
  println!("  {label}: {status} ({}) [{}]", check.path, check.source);
  if let Some(err) = &check.error {
    println!("    note: {err}");
  }
}

fn enabled_label(value: bool) -> &'static str {
  if value { "yes" } else { "no" }
}

fn status_label(status: &DoctorStatus) -> &'static str {
  match status {
    DoctorStatus::Ok => "ok",
    DoctorStatus::Warning => "warning",
    DoctorStatus::Fail => "fail",
  }
}

fn optional_path(path: Option<&Path>) -> String {
  path
    .map(|p| p.display().to_string())
    .unwrap_or_else(|| "unknown".to_string())
}

fn optional_env(value: Option<&str>) -> &str {
  value.unwrap_or("unset")
}

fn sandbox_capabilities(enforcing: bool) -> Vec<&'static str> {
  if enforcing {
    vec!["process", "filesystem", "network"]
  } else {
    Vec::new()
  }
}

fn sandbox_warnings(backend: &str, enforcement: SandboxEnforcement) -> Vec<String> {
  match enforcement {
    SandboxEnforcement::Enforcing => Vec::new(),
    SandboxEnforcement::Permissive => vec![format!(
      "sandbox backend '{backend}' is installed but not enforcing in this environment; shell, script, and plugin runs rely only on in-process policy checks"
    )],
    SandboxEnforcement::Disabled => vec![format!(
      "no enforcing sandbox backend is available (running with backend '{backend}'); shell, script, and plugin runs rely only on in-process policy checks"
    )],
  }
}

#[cfg(test)]
mod tests {
  use super::{
    LLMConfigSource, OutputFormat, SandboxEnforcement, parse_env_key, sandbox_warnings,
    source_label,
  };
  use std::path::PathBuf;

  #[test]
  fn output_format_rejects_unknown_values() {
    assert!(OutputFormat::parse("yaml").is_err());
    assert_eq!(OutputFormat::parse("text").unwrap(), OutputFormat::Text);
    assert_eq!(OutputFormat::parse("json").unwrap(), OutputFormat::Json);
  }

  #[test]
  fn parse_env_key_ignores_empty_and_comments() {
    assert_eq!(
      parse_env_key("OPENAI_API_KEY=secret").as_deref(),
      Some("OPENAI_API_KEY")
    );
    assert_eq!(parse_env_key("# OPENAI_API_KEY=secret"), None);
    assert_eq!(parse_env_key("OPENAI_API_KEY="), None);
  }

  #[test]
  fn sandbox_warnings_explain_disabled_state() {
    let warnings = sandbox_warnings("noop", SandboxEnforcement::Disabled);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("no enforcing sandbox backend"));
    assert!(warnings[0].contains("in-process policy"));
  }

  #[test]
  fn sandbox_warnings_distinguish_permissive_from_disabled() {
    // Operators need to tell "platform has no backend at all" apart from
    // "platform backend exists but cannot enforce right now". The two
    // warnings must therefore be different strings.
    let disabled = sandbox_warnings("noop", SandboxEnforcement::Disabled);
    let permissive = sandbox_warnings("sandbox-exec", SandboxEnforcement::Permissive);
    assert_ne!(disabled, permissive);
    assert!(permissive[0].contains("installed but not enforcing"));
  }

  #[test]
  fn sandbox_warnings_empty_when_enforcing() {
    let warnings = sandbox_warnings("seccomp", SandboxEnforcement::Enforcing);
    assert!(warnings.is_empty());
  }

  /// F-A7-4: built-in source renders as a static label, no path.
  #[test]
  fn source_label_built_in_default() {
    let source = LLMConfigSource {
      kind: yanshi_llm::LLMConfigSourceKind::BuiltInDefault,
      path: None,
      warnings: Vec::new(),
    };
    assert_eq!(source_label(&source), "built-in default_models.yml");
  }

  /// F-A7-4: user `~/.yanshi/models.yml` MUST flag itself as
  /// overriding the built-in, otherwise the silent-override surprise
  /// from A7 dogfooding can recur. Regression-locks the "(overrides
  /// built-in)" suffix that the doctor text output relies on.
  #[test]
  fn source_label_user_models_yml_marks_shadow() {
    let source = LLMConfigSource {
      kind: yanshi_llm::LLMConfigSourceKind::UserModelsYml,
      path: Some(PathBuf::from("/home/u/.yanshi/models.yml")),
      warnings: Vec::new(),
    };
    let label = source_label(&source);
    assert!(label.contains("/home/u/.yanshi/models.yml"), "{label}");
    assert!(label.contains("overrides built-in"), "{label}");
  }

  /// F-A7-4: `YANSHI_MODELS_CONFIG` env override should be the
  /// loudest of the three (it shadows BOTH ~/.yanshi AND the
  /// built-in defaults), so the label calls that out explicitly.
  #[test]
  fn source_label_env_override_names_env_var() {
    let source = LLMConfigSource {
      kind: yanshi_llm::LLMConfigSourceKind::EnvOverride,
      path: Some(PathBuf::from("/tmp/custom-models.yml")),
      warnings: Vec::new(),
    };
    let label = source_label(&source);
    assert!(label.contains("/tmp/custom-models.yml"), "{label}");
    assert!(label.contains("YANSHI_MODELS_CONFIG"), "{label}");
    assert!(label.contains("overrides"), "{label}");
  }
}
