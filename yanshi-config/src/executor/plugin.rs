//! `PluginWorkflowNode` — adapter that exposes a subprocess plugin to a YAML
//! workflow as `type: plugin`.
//!
//! The wrapper holds the manifest path + plugin-declared node type. On first
//! `execute` it lazily spawns the plugin via [`PluginHost::load`] and caches
//! the resulting [`Arc<PluginHost>`] in a process-wide table keyed by the
//! canonicalized manifest path. Subsequent workflow nodes pointing at the
//! same manifest reuse the same subprocess, which keeps initialize cost paid
//! once per `yanshi workflow run` invocation.
//!
//! When the `YANSHI_PLUGIN_SANDBOX=1` environment variable is set, the
//! cached host is constructed via [`PluginHost::builder`] with a
//! [`OsSandboxPluginPreparer`]. The preparer translates the plugin's
//! `[plugin.capabilities]` block into the existing
//! `yanshi_tools::sandbox` primitives (capability set + scope) and lets
//! the platform backend wrap the spawn command (macOS `sandbox-exec`,
//! Linux seccomp). This is the bridge that fulfils the `[plugin.capabilities]
//! → SandboxPolicy` wiring noted in `docs/PLUGIN_DESIGN.md` §6.5.
//!
//! See `docs/PLUGIN_DESIGN.md` §6 for the wire protocol and §6.4 for the
//! workflow integration covered here.

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::process::Command;
use tokio::sync::Mutex;
use yanshi_core::plugin::{
  Capabilities, CommandPreparer, FsAccess, PluginError, PluginHost, PluginManifest,
};
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
};
use yanshi_tools::sandbox::{SandboxBackend, SandboxScope, default_backend};
use yanshi_tools::{Capability, PluginPolicy, SecurityProfile};

/// Process-wide cache of loaded plugin hosts, keyed by canonicalized manifest
/// path. Multiple workflow nodes pointing at the same `plugin.toml` share a
/// single subprocess so we pay the spawn + handshake cost exactly once.
fn host_cache() -> &'static Mutex<HashMap<PathBuf, Arc<PluginHost>>> {
  static CELL: OnceLock<Mutex<HashMap<PathBuf, Arc<PluginHost>>>> = OnceLock::new();
  CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Legacy force-on opt-in: when set to a non-empty / non-`0` value, the
/// plugin host wraps spawns with the OS sandbox preparer regardless of
/// the active [`SecurityProfile`] default. Most useful for `dev` where
/// the profile default is "no sandbox" — operators can flip this to
/// stress-test the sandbox bridge without changing the global profile.
///
/// In `local` and `production` the policy already requires sandbox, so
/// the env var is informational (it can't loosen the requirement).
const PLUGIN_SANDBOX_ENV: &str = "YANSHI_PLUGIN_SANDBOX";

/// Operator opt-out: when set to a non-empty / non-`0` value, the host
/// will skip sandbox wrapping if (and only if) the active
/// [`SecurityProfile`] allows it. Mirrors the
/// `--allow-unsandboxed-plugin` CLI flag at install time. The
/// `production` profile rejects this opt-in and errors at spawn time.
const PLUGIN_ALLOW_UNSANDBOXED_ENV: &str = "YANSHI_ALLOW_UNSANDBOXED_PLUGIN";

#[derive(Debug, Clone)]
pub struct PluginWorkflowNode {
  pub workflow_node_id: String,
  pub manifest_path: PathBuf,
  pub plugin_node_type: String,
}

impl PluginWorkflowNode {
  pub fn new(
    workflow_node_id: impl Into<String>,
    manifest_path: PathBuf,
    plugin_node_type: impl Into<String>,
  ) -> Self {
    Self {
      workflow_node_id: workflow_node_id.into(),
      manifest_path,
      plugin_node_type: plugin_node_type.into(),
    }
  }

  async fn ensure_loaded(&self) -> Result<Arc<PluginHost>, YanshiError> {
    let canonical =
      self
        .manifest_path
        .canonicalize()
        .map_err(|err| YanshiError::NodeInputError {
          message: format!(
            "plugin '{}': manifest path '{}' not accessible: {}",
            self.workflow_node_id,
            self.manifest_path.display(),
            err
          ),
        })?;
    let mut cache = host_cache().lock().await;
    if let Some(existing) = cache.get(&canonical) {
      return Ok(existing.clone());
    }
    let preparer = preparer_from_env().map_err(|err| YanshiError::AsyncExecutionError {
      message: format!(
        "plugin '{}': sandbox policy rejected spawn: {err}",
        self.workflow_node_id
      ),
    })?;
    let host = PluginHost::builder()
      .with_command_preparer(preparer)
      .load(&canonical)
      .await
      .map_err(|err| YanshiError::AsyncExecutionError {
        message: format!(
          "plugin '{}': failed to load manifest '{}': {}",
          self.workflow_node_id,
          canonical.display(),
          err
        ),
      })?;
    let arc = Arc::new(host);
    cache.insert(canonical, arc.clone());
    Ok(arc)
  }
}

#[async_trait]
impl AsyncNode for PluginWorkflowNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let host = self.ensure_loaded().await?;
    let result = host
      .execute_node(&self.plugin_node_type, inputs.clone())
      .await
      .map_err(YanshiError::from)?;
    Ok(result.outputs)
  }
}

/// Pick a [`CommandPreparer`] based on the active [`SecurityProfile`]
/// (resolved via `YANSHI_SECURITY_PROFILE`) plus two env-var opt-ins:
/// `YANSHI_PLUGIN_SANDBOX` (force-on) and
/// `YANSHI_ALLOW_UNSANDBOXED_PLUGIN` (opt-out, honored only when
/// the profile allows).
///
/// Per-profile defaults (mirrors `PluginPolicy::for_profile`):
/// - `dev`: no sandbox. `YANSHI_PLUGIN_SANDBOX=1` opts in.
/// - `local`: sandbox required. `YANSHI_ALLOW_UNSANDBOXED_PLUGIN=1`
///   opts out.
/// - `production`: sandbox required, opt-out rejected at spawn time.
///
/// Returns `Err` only under `production` + opt-out (the spawn must
/// fail before the child starts).
fn preparer_from_env() -> Result<Arc<dyn CommandPreparer>, PreparerSelectionError> {
  let profile = SecurityProfile::from_env().unwrap_or_default();
  let force_sandbox = env_truthy(PLUGIN_SANDBOX_ENV);
  let allow_unsandboxed = env_truthy(PLUGIN_ALLOW_UNSANDBOXED_ENV);
  select_preparer(profile, force_sandbox, allow_unsandboxed)
}

/// Pure, unit-testable variant of [`preparer_from_env`].
///
/// Lives behind a separate function so the integration test suite can
/// exhaust the policy matrix without poking process-wide env vars.
fn select_preparer(
  profile: SecurityProfile,
  force_sandbox: bool,
  allow_unsandboxed: bool,
) -> Result<Arc<dyn CommandPreparer>, PreparerSelectionError> {
  let policy = PluginPolicy::for_profile(profile);

  // Production: opt-out is rejected regardless of other flags.
  if allow_unsandboxed && !policy.allow_sandbox_disabled_opt_in {
    return Err(PreparerSelectionError::OptOutRejected { profile });
  }

  // Caller forced sandbox on (legacy flag): always wrap.
  if force_sandbox {
    return Ok(Arc::new(OsSandboxPluginPreparer::new(default_backend())));
  }

  // Operator-blessed opt-out (only honored when profile allows it).
  if allow_unsandboxed && policy.allow_sandbox_disabled_opt_in {
    return Ok(Arc::new(NoopWithTraceparent));
  }

  // Default by profile.
  if policy.require_sandbox {
    Ok(Arc::new(OsSandboxPluginPreparer::new(default_backend())))
  } else {
    Ok(Arc::new(NoopWithTraceparent))
  }
}

/// Read an env var as a truthy bool: non-empty and not `"0"`.
fn env_truthy(name: &str) -> bool {
  matches!(std::env::var(name), Ok(v) if !matches!(v.as_str(), "" | "0"))
}

/// Errors returned by `select_preparer`. Today there's exactly one,
/// kept as an enum so future policy gates (signature-required, network
/// admission band) can land additively without breaking callers.
#[derive(Debug)]
pub enum PreparerSelectionError {
  /// Operator passed `YANSHI_ALLOW_UNSANDBOXED_PLUGIN=1` under a
  /// profile that rejects the opt-out (today, only `production`).
  OptOutRejected { profile: SecurityProfile },
}

impl std::fmt::Display for PreparerSelectionError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::OptOutRejected { profile } => write!(
        f,
        "{profile} profile refuses YANSHI_ALLOW_UNSANDBOXED_PLUGIN",
      ),
    }
  }
}

impl std::error::Error for PreparerSelectionError {}

/// Bridge from `yanshi-core::plugin::CommandPreparer` to the platform
/// sandbox backend in `yanshi-tools`.
///
/// Translates the plugin manifest's `[plugin.capabilities]` block into:
///
/// * a capability set ([`yanshi_tools::Capability`]) — `FsRead` /
///   `FsWrite` / `Net` / `Exec` / `Env` are granted only when the
///   manifest declares the corresponding section;
/// * a [`SandboxScope`] — paths from `filesystem` entries become
///   `read_paths` / `write_paths`. The plugin's manifest directory is
///   pre-allowed for reading so the binary itself, plus any local
///   resources next to `plugin.toml`, are reachable. Relative paths in
///   the manifest are resolved against the manifest directory.
///
/// The translation is intentionally permissive in two places to match
/// `yanshi_tools::builtin::shell::build_scope_from_policy`:
///
/// * a manifest with no `filesystem` entries gets `/tmp` + the plugin's
///   manifest directory as its read scope (the binary needs to be readable
///   to be executable);
/// * `env_vars` is recorded as a granted capability (`Capability::Env`) but
///   not enforced at the OS sandbox layer — neither macOS sandbox-exec nor
///   Linux seccomp can scrub the env passed to `execve`. The CLI inherits
///   the caller's env unchanged.
pub struct OsSandboxPluginPreparer {
  backend: Arc<dyn SandboxBackend>,
}

impl std::fmt::Debug for OsSandboxPluginPreparer {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OsSandboxPluginPreparer")
      .field("backend", &self.backend.name())
      .finish()
  }
}

impl OsSandboxPluginPreparer {
  pub fn new(backend: Arc<dyn SandboxBackend>) -> Self {
    Self { backend }
  }
}

/// Inject the active `traceparent` (P3.8) into the spawn command's env
/// as `TRACEPARENT=<value>` when a context is in scope. No-op
/// otherwise — propagating an empty value would mask the
/// "no upstream context" case for OTel-aware plugins.
pub(crate) fn inject_traceparent_into_command(command: &mut Command) {
  if let Some(value) = yanshi_tracing::context::current_traceparent() {
    command.env(yanshi_tracing::context::TRACEPARENT_ENV, value);
  }
}

/// `CommandPreparer` shim that mirrors `NoopCommandPreparer` but also
/// stamps the active `traceparent` onto the spawn command's env when
/// one is in scope. Used by [`select_preparer`] in place of the bare
/// no-op so the trace tree stays connected when sandbox wrapping is
/// disabled.
#[derive(Debug, Default)]
pub(crate) struct NoopWithTraceparent;

impl CommandPreparer for NoopWithTraceparent {
  fn name(&self) -> &str {
    "noop-plugin"
  }
  fn prepare(
    &self,
    command: &mut Command,
    _manifest: &PluginManifest,
    _manifest_dir: &Path,
  ) -> Result<(), PluginError> {
    inject_traceparent_into_command(command);
    Ok(())
  }
}

impl CommandPreparer for OsSandboxPluginPreparer {
  fn name(&self) -> &str {
    self.backend.name()
  }

  fn prepare(
    &self,
    command: &mut Command,
    manifest: &PluginManifest,
    manifest_dir: &Path,
  ) -> Result<(), PluginError> {
    inject_traceparent_into_command(command);
    let caps = capabilities_for_manifest(&manifest.plugin.capabilities);
    let scope = scope_for_manifest(&manifest.plugin.capabilities, manifest_dir).map_err(|err| {
      PluginError::PreparerRejected {
        plugin: manifest.plugin.name.clone(),
        reason: err,
      }
    })?;
    self
      .backend
      .wrap_command(command, &caps, &scope)
      .map_err(|err| PluginError::PreparerRejected {
        plugin: manifest.plugin.name.clone(),
        reason: format!("OS sandbox backend '{}' failed: {err}", self.backend.name()),
      })
  }
}

/// Decompose `[plugin.capabilities]` into the [`Capability`] set the
/// platform backend should enforce.
pub(crate) fn capabilities_for_manifest(caps: &Capabilities) -> Vec<Capability> {
  let mut out = Vec::new();
  if caps.requires_fs_read() {
    out.push(Capability::FsRead);
  }
  if caps.requires_fs_write() {
    out.push(Capability::FsWrite);
  }
  if caps.requires_net() {
    out.push(Capability::Net);
  }
  if caps.requires_exec() {
    out.push(Capability::Exec);
  }
  if caps.requires_env() {
    out.push(Capability::Env);
  }
  out
}

/// Project `[plugin.capabilities].filesystem` into the read/write scope
/// the platform backend will materialise. Relative paths in the manifest
/// are resolved against the manifest directory; the manifest directory
/// itself is always readable so the plugin binary can be exec'd.
pub(crate) fn scope_for_manifest(
  caps: &Capabilities,
  manifest_dir: &Path,
) -> Result<SandboxScope, String> {
  let entries = caps
    .filesystem_entries()
    .map_err(|err| format!("invalid filesystem capability: {err}"))?;

  let mut scope = SandboxScope::new();
  // The plugin executable must be readable. Pre-allow the manifest dir.
  scope.read_paths.push(manifest_dir.to_path_buf());
  // Match shell::build_scope_from_policy's permissive default so plugins
  // that don't declare anything still have a working /tmp.
  if entries.is_empty() {
    scope.read_paths.push(PathBuf::from("/tmp"));
  }

  for entry in entries {
    let resolved = if entry.path.is_absolute() {
      entry.path.clone()
    } else {
      manifest_dir.join(&entry.path)
    };
    match entry.access {
      FsAccess::Read => scope.read_paths.push(resolved),
      FsAccess::Write => {
        // A writable path must also be readable.
        scope.read_paths.push(resolved.clone());
        scope.write_paths.push(resolved);
      }
    }
  }

  scope.working_directory = Some(manifest_dir.to_path_buf());
  Ok(scope)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn full_capabilities() -> Capabilities {
    Capabilities {
      filesystem: vec!["read:./inputs".into(), "write:/tmp/out".into()],
      network: vec!["api.example.com".into()],
      processes: vec!["sh".into()],
      env_vars: vec!["DEMO".into()],
    }
  }

  #[test]
  fn capability_set_reflects_manifest_grants() {
    let resolved = capabilities_for_manifest(&full_capabilities());
    assert!(resolved.contains(&Capability::FsRead));
    assert!(resolved.contains(&Capability::FsWrite));
    assert!(resolved.contains(&Capability::Net));
    assert!(resolved.contains(&Capability::Exec));
    assert!(resolved.contains(&Capability::Env));
  }

  #[test]
  fn empty_capabilities_yield_no_grants() {
    let caps = Capabilities::default();
    assert!(capabilities_for_manifest(&caps).is_empty());
  }

  #[test]
  fn scope_resolves_relative_paths_against_manifest_dir() {
    let caps = Capabilities {
      filesystem: vec!["read:./data".into(), "write:/tmp/out".into()],
      ..Capabilities::default()
    };
    let manifest_dir = Path::new("/var/yanshi/plugins/demo");
    let scope = scope_for_manifest(&caps, manifest_dir).unwrap();

    // Manifest dir is always read-allowed (so the binary is reachable).
    assert!(scope.read_paths.iter().any(|p| p == manifest_dir));
    // Relative read path resolves against manifest dir.
    assert!(
      scope
        .read_paths
        .iter()
        .any(|p| p == &manifest_dir.join("data"))
    );
    // Absolute write path stays absolute and is also read-allowed.
    let tmp_out = PathBuf::from("/tmp/out");
    assert!(scope.write_paths.contains(&tmp_out));
    assert!(scope.read_paths.contains(&tmp_out));
    assert_eq!(scope.working_directory.as_deref(), Some(manifest_dir));
  }

  #[test]
  fn empty_filesystem_block_falls_back_to_tmp() {
    let caps = Capabilities::default();
    let manifest_dir = Path::new("/opt/plugins/demo");
    let scope = scope_for_manifest(&caps, manifest_dir).unwrap();

    assert!(scope.read_paths.iter().any(|p| p == Path::new("/tmp")));
    assert!(scope.write_paths.is_empty());
    assert!(scope.read_paths.iter().any(|p| p == manifest_dir));
  }

  #[test]
  fn invalid_filesystem_entry_surfaces_as_string_reason() {
    let caps = Capabilities {
      filesystem: vec!["read:".into()],
      ..Capabilities::default()
    };
    let err = scope_for_manifest(&caps, Path::new("/tmp")).unwrap_err();
    assert!(err.contains("invalid filesystem capability"));
  }

  // ── select_preparer policy matrix ──────────────────────────────────────
  //
  // The pure function exhausted here is what the CLI plugin host actually
  // calls; the env-var glue (`preparer_from_env`) is a thin wrapper. The
  // five test cases mirror the install-time `PluginPolicy::evaluate`
  // matrix one-to-one so the spawn-time and install-time decisions stay
  // synchronized.

  #[test]
  fn select_preparer_dev_default_is_noop() {
    // Dev profile, no env opt-ins → no sandbox wrapping at spawn time.
    let preparer = select_preparer(SecurityProfile::Dev, false, false).unwrap();
    assert_eq!(preparer.name(), "noop-plugin");
  }

  #[test]
  fn select_preparer_dev_force_sandbox_opts_in() {
    // Dev + legacy YANSHI_PLUGIN_SANDBOX=1 wraps the spawn so authors
    // can stress-test the sandbox bridge without flipping profiles.
    let preparer = select_preparer(SecurityProfile::Dev, true, false).unwrap();
    assert_ne!(
      preparer.name(),
      "noop-plugin",
      "force-sandbox must engage the OS backend preparer"
    );
  }

  #[test]
  fn select_preparer_local_default_engages_sandbox() {
    // Local profile (the install-time default) → sandbox by default at
    // spawn time. This is the contract P5.4 closes.
    let preparer = select_preparer(SecurityProfile::Local, false, false).unwrap();
    assert_ne!(preparer.name(), "noop-plugin");
  }

  #[test]
  fn select_preparer_local_honors_opt_out() {
    // Local + YANSHI_ALLOW_UNSANDBOXED_PLUGIN=1 mirrors the install-
    // time --allow-unsandboxed-plugin flag.
    let preparer = select_preparer(SecurityProfile::Local, false, true).unwrap();
    assert_eq!(preparer.name(), "noop-plugin");
  }

  #[test]
  fn select_preparer_production_default_engages_sandbox() {
    let preparer = select_preparer(SecurityProfile::Production, false, false).unwrap();
    assert_ne!(preparer.name(), "noop-plugin");
  }

  #[test]
  fn select_preparer_production_rejects_opt_out() {
    let err = select_preparer(SecurityProfile::Production, false, true)
      .expect_err("production must refuse the opt-out");
    match err {
      PreparerSelectionError::OptOutRejected { profile } => {
        assert_eq!(profile, SecurityProfile::Production);
      }
    }
  }

  #[test]
  fn select_preparer_force_sandbox_overrides_opt_out_under_local() {
    // Both flags set under local: force_sandbox wins because the user
    // explicitly asked for the OS bridge. This stays consistent with
    // the install-time semantics where `--allow-unsandboxed-plugin` is
    // only meaningful when the policy default wants a sandbox.
    let preparer = select_preparer(SecurityProfile::Local, true, true).unwrap();
    assert_ne!(preparer.name(), "noop-plugin");
  }
}
