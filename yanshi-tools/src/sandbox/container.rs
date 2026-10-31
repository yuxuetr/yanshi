//! Strongly-isolated `ContainerBackend` for the `code_exec` tool (S4.2,
//! `docs/RFC_LLM_CODE_EXECUTION.md`).
//!
//! The existing OS sandbox backends ([`crate::sandbox::macos`],
//! [`crate::sandbox::linux`]) are syscall/path-scoped containment on top of
//! the *shared host kernel* — the right tier for author-signed content
//! (`ScriptTool`), but explicitly called out by the RFC as insufficient once
//! the content being run is adversarial by construction on every single
//! invocation (llm-generated code, never vetted by anyone). `ContainerBackend`
//! shells out to a real container engine so each invocation gets its own
//! kernel boundary — a full container (rootless Podman) or, when available, a
//! genuine per-invocation microVM (Apple's `container` CLI, backed by
//! Virtualization.framework — confirmed in the S3 track to provision a real
//! Linux 6.12 kernel, not a shared-kernel namespace).
//!
//! ## Engine detection
//!
//! Detected once at construction by asking the engine's service whether it
//! is up (`container system status` / `podman info`) — an installed CLI
//! with a stopped service is reported as absent — preferring Apple's
//! `container` CLI (stronger isolation) over rootless Podman when both are
//! usable. `None` if neither is.
//!
//! ## No partial-enforcement tier
//!
//! Unlike the seccomp/Landlock pair (where seccomp alone is still real,
//! partial containment when Landlock is unavailable) there is nothing
//! between "a real container engine wraps this process" and "nothing does."
//! [`SandboxBackend::wrap_command`] therefore **hard-refuses**
//! (`Err(SandboxError::Unsupported)`) when no engine is available, rather
//! than the macOS/Linux backends' pattern of always succeeding at a weaker
//! enforcement level — there is no safe degraded mode for content that was
//! never vetted. [`SandboxBackend::enforcement_level`] uses the trait's
//! default derivation (`Enforcing` / `Disabled`, never `Permissive`) for the
//! same reason: unlike a missing `sandbox-exec` binary (a misconfiguration
//! on a platform that otherwise supports enforcement), a missing container
//! engine leaves genuinely nothing to fall back to.
//!
//! ## Command translation
//!
//! `wrap_command` rewrites the `Command` wholesale (same pattern as
//! [`crate::sandbox::macos::MacosSandboxExecBackend`]): the caller's
//! program/args become the command run *inside* the container image, not on
//! the host. `scope.working_directory` is bind-mounted into the container at
//! a fixed path and becomes the container's own working directory —
//! `scope.read_paths`/`write_paths` beyond the working directory are not
//! separately mounted in this v1 (`code_exec`, the only caller today, only
//! ever needs its own ephemeral workdir; see the RFC's "ephemeral working
//! directory" constraint).
//!
//! ## Read-only root filesystem (T2.3)
//!
//! Both engines additionally get `--read-only` (the same docker-compatible
//! flag name on Apple's `container` CLI and Podman), so `/workspace` — the
//! bind mount above — is the *only* writable path inside the container;
//! everything else in the image's rootfs is read-only. Verified empirically
//! this session: a plain `python:alpine` run needs no changes under this
//! (stdlib bytecode ships precompiled in the image), and a write to `/etc`
//! fails with `OSError` while a write to `/workspace` still succeeds.
//!
//! ## Resource limits (S3.2 fields, reused)
//!
//! `max_memory_bytes` → `-m`/`--memory` (MiB-rounded). `max_cpu_secs` →
//! `--ulimit cpu=<secs>:<secs>` on both engines — this is the same exact
//! "cumulative CPU-seconds, then kill" semantics S3.2 chose `RLIMIT_CPU`
//! for on the OS-sandbox backends (a container engine's `--cpus` flag is a
//! *rate* cap, like cgroup's `cpu.max` — wrong semantics for this field,
//! same distinction S3.2's module docs already draw). `max_pids` → Podman's
//! dedicated `--pids-limit` (cgroups-backed, scoped exactly to this
//! container) when using Podman, or `--ulimit nproc=<n>:<n>` for Apple's
//! `container` CLI, which does not expose a dedicated pids-limit flag as of
//! CLI v1.0.0 — see "Non-root execution" below for why this only works
//! combined with running as a non-root uid.
//!
//! ## Network (mandatory hard-off in v1)
//!
//! `code_exec`'s effective capability set never includes [`Capability::Net`]
//! — `wrap_command` defensively refuses (`Err(SandboxError::Prepare)`) if a
//! caller ever passes it, since no egress allowlist proxy exists yet (RFC
//! §"default no network": network access for `code_exec` "should not ship"
//! until that proxy lands). Both engines get an explicit `--network none`.
//! For Apple's `container` CLI this value is **undocumented** — its `run
//! --help` only lists named networks (`container network ls`) — but was
//! empirically verified (this session, real container runs) to fully block
//! DNS resolution, HTTP, and raw outbound sockets; omitting `--network`
//! entirely was also tested and, contrary to the `--help` text's silence on
//! a default, attaches the `default` NAT'd network and grants full internet
//! access — so the flag is mandatory, never optional, for this engine.
//!
//! ## Non-root execution (mandatory)
//!
//! The container always runs as a fixed unprivileged uid
//! ([`CONTAINER_UID`]), for two reasons verified empirically this session:
//! defense in depth for adversarial content, and because Linux's kernel
//! exempts root (uid 0) from `RLIMIT_NPROC` entirely — the `max_pids`
//! `--ulimit nproc=` limit below is silently unenforced against a root
//! process (confirmed: 50 `fork()`s succeeded under `nproc=8:8` running as
//! root) but correctly caps `fork()` at the limit once the same command runs
//! as a non-root uid. The bind-mounted workdir is still writable by the
//! non-root uid on this host (verified) since the mount doesn't enforce
//! strict uid ownership checks.

use std::ffi::OsString;
use std::process::Stdio as StdStdio;

use tokio::process::Command;

use crate::Capability;

use super::{SandboxBackend, SandboxError, SandboxScope};

/// Fixed unprivileged uid every container runs as. See "Non-root execution"
/// in the module docs — arbitrary but must be non-zero and stable across
/// invocations; nothing about `code_exec` requires it to correspond to a
/// specific named user, since each invocation's container is disposable.
const CONTAINER_UID: u32 = 1000;

/// Apple's `container` CLI refuses to start a VM below this memory amount
/// (empirically verified this session:
/// `"minimum memory amount allowed is 200 MiB"` from a `-m 32m` attempt) —
/// each container is a full microVM, not a lightweight cgroup, so there's a
/// floor. `max_memory_bytes` requests below this are rounded up rather than
/// passed through and rejected at spawn time.
const MIN_CONTAINER_MEMORY_MIB: u64 = 200;

/// Which container engine CLI this backend shells out to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerEngine {
  /// Apple's `container` CLI — per-invocation Linux microVM via
  /// Virtualization.framework, macOS only.
  AppleContainer,
  /// Rootless Podman — Linux.
  Podman,
}

impl ContainerEngine {
  fn binary(self) -> &'static str {
    match self {
      Self::AppleContainer => "container",
      Self::Podman => "podman",
    }
  }

  /// Arguments for a probe that only succeeds when the engine can actually
  /// start a container, not merely when its CLI is installed: `container
  /// --version` exits 0 with the apiserver stopped (eval 2026-10-07), while
  /// `container system status` exits 1 ("apiserver is not running"). The
  /// same holds for `podman info` vs `podman --version` when the Podman
  /// machine / storage is unusable.
  fn probe_args(self) -> &'static [&'static str] {
    match self {
      Self::AppleContainer => &["system", "status"],
      Self::Podman => &["info"],
    }
  }

  /// Cheap, synchronous, run once at construction.
  fn probe(self) -> bool {
    engine_responds(self.binary(), self.probe_args())
  }

  /// Prefer Apple's `container` CLI (stronger, genuine per-invocation VM
  /// isolation) when both happen to be present.
  fn detect() -> Option<Self> {
    [Self::AppleContainer, Self::Podman]
      .into_iter()
      .find(|candidate| candidate.probe())
  }
}

fn engine_responds(binary: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> bool {
  std::process::Command::new(binary)
    .args(args)
    .stdout(StdStdio::null())
    .stderr(StdStdio::null())
    .status()
    .map(|status| status.success())
    .unwrap_or(false)
}

pub struct ContainerBackend {
  engine: Option<ContainerEngine>,
  /// Image to run the caller's program inside. `code_exec` (the only caller
  /// in v1) hardcodes `"python:alpine"` — see `code_exec_backend()`.
  image: String,
}

impl ContainerBackend {
  pub fn new(image: impl Into<String>) -> Self {
    Self {
      engine: ContainerEngine::detect(),
      image: image.into(),
    }
  }
}

/// The `ContainerBackend` instance `code_exec` uses: Python-only image,
/// engine auto-detected. Analogous to [`super::default_backend`] but for the
/// strong-isolation tier rather than the general OS-sandbox tier — kept
/// separate because they answer different questions (`default_backend` picks
/// *the* backend for `shell`/`script`'s opt-in `os_sandbox`; this picks the
/// mandatory backend `code_exec` refuses to run without).
pub fn code_exec_backend() -> ContainerBackend {
  ContainerBackend::new("python:alpine")
}

impl SandboxBackend for ContainerBackend {
  fn name(&self) -> &'static str {
    "container"
  }

  fn is_enforcing(&self) -> bool {
    self.engine.is_some()
  }

  fn wrap_command(
    &self,
    command: &mut Command,
    effective_capabilities: &[Capability],
    scope: &SandboxScope,
  ) -> Result<(), SandboxError> {
    let engine = self.engine.ok_or_else(|| SandboxError::Unsupported {
      platform: "container",
      message: "no container engine ('container' or 'podman') found on PATH — code_exec \
                 requires real container isolation and refuses to run unsandboxed"
        .to_string(),
    })?;

    // Defensive invariant, not just documentation: this backend must never
    // silently grant network access — there is no egress allowlist proxy
    // yet (RFC_LLM_CODE_EXECUTION.md), so a caller requesting Net here is a
    // bug, not a config choice to honour.
    if effective_capabilities.contains(&Capability::Net) {
      return Err(SandboxError::Prepare {
        message: "ContainerBackend refuses Capability::Net — network access requires the \
                   (not yet built) egress allowlist proxy per RFC_LLM_CODE_EXECUTION.md"
          .to_string(),
      });
    }

    let workdir = scope
      .working_directory
      .as_ref()
      .ok_or_else(|| SandboxError::Prepare {
        message: "ContainerBackend requires SandboxScope::working_directory (the ephemeral \
                   workdir to mount into the container)"
          .to_string(),
      })?;
    let canonical_workdir = workdir
      .canonicalize()
      .map_err(|err| SandboxError::Prepare {
        message: format!(
          "failed to canonicalize working directory '{}': {err}",
          workdir.display()
        ),
      })?;

    let std_cmd = command.as_std();
    let original_program = std_cmd.get_program().to_os_string();
    let original_args: Vec<OsString> = std_cmd.get_args().map(|s| s.to_os_string()).collect();

    let mut new_cmd = Command::new(engine.binary());
    new_cmd.arg("run").arg("--rm");
    if let Some(name) = &scope.container_name {
      new_cmd.arg("--name").arg(name);
    }
    new_cmd
      .arg("-v")
      .arg(format!("{}:/workspace", canonical_workdir.display()));
    new_cmd.arg("-w").arg("/workspace");
    // T2.3: `--read-only` mounts the container's root filesystem read-only,
    // leaving only the `/workspace` bind mount above writable. Confirmed
    // empirically (this session, both engines accept the identical
    // docker-compatible flag name) that a plain `python:alpine` run still
    // works unmodified under it — stdlib bytecode ships precompiled in the
    // image, so nothing needs to write outside `/workspace` at runtime —
    // and that a write to `/etc` fails with `OSError` while a write to
    // `/workspace` still succeeds. Without this, code_exec's adversarial
    // payload could write anywhere in the container's rootfs; `--rm` +
    // zero network + no other bind mounts already bounded the practical
    // impact, but this closes the gap outright rather than relying on
    // those other layers alone.
    new_cmd.arg("--read-only");
    // Verified empirically (this session, both engines): `--network none` is
    // a real, effective value on both CLIs even though Apple's isn't
    // documented in `run --help` — DNS/HTTP/raw sockets all confirmed
    // blocked. Omitting the flag entirely was also tested and attaches a
    // NAT'd default network with full internet access, so this is mandatory,
    // never conditional on engine.
    new_cmd.arg("--network").arg("none");
    // Verified empirically: Linux's kernel exempts root (uid 0) from
    // RLIMIT_NPROC entirely, so `--ulimit nproc=` below is silently
    // unenforced unless the process also runs as a non-root uid — confirmed
    // by running the identical fork-bomb fixture as root (limit ignored,
    // 50/50 forks succeeded) vs. as this uid (limit enforced, fork() failed
    // past it). The bind-mounted workdir remains writable by this uid on
    // this host (verified). The flag name differs per engine: Apple's
    // `container` CLI uses `--uid <uid>`; Podman follows the
    // docker-compatible `-u, --user <user>` (`name|uid[:gid]`) convention —
    // passing `--uid` to Podman fails outright with "unknown flag", caught
    // by a real end-to-end test run against Podman this session.
    match engine {
      ContainerEngine::AppleContainer => {
        new_cmd.arg("--uid").arg(CONTAINER_UID.to_string());
      }
      ContainerEngine::Podman => {
        new_cmd.arg("-u").arg(CONTAINER_UID.to_string());
      }
    }

    if let Some(bytes) = scope.max_memory_bytes {
      let mib = bytes.div_ceil(1024 * 1024).max(MIN_CONTAINER_MEMORY_MIB);
      new_cmd.arg("-m").arg(format!("{mib}m"));
    }
    if let Some(secs) = scope.max_cpu_secs {
      new_cmd.arg("--ulimit").arg(format!("cpu={secs}:{secs}"));
    }
    if let Some(pids) = scope.max_pids {
      match engine {
        ContainerEngine::Podman => {
          new_cmd.arg("--pids-limit").arg(pids.to_string());
        }
        ContainerEngine::AppleContainer => {
          new_cmd.arg("--ulimit").arg(format!("nproc={pids}:{pids}"));
        }
      }
    }

    new_cmd.arg(&self.image);
    new_cmd.arg(original_program);
    for arg in original_args {
      new_cmd.arg(arg);
    }

    *command = new_cmd;
    Ok(())
  }

  /// Forcefully stop the container `wrap_command` named via
  /// `scope.container_name`. Required override, not optional polish: the
  /// `Command`'s own `Child` here is the `container`/`podman` **client**
  /// process, and killing that client does not stop the container it
  /// launched — confirmed empirically (S4.2 follow-up, both engines) by
  /// `SIGKILL`-ing a running client process and observing the container
  /// stayed `running`/`Up` indefinitely afterward. No-op if
  /// `scope.container_name` is unset (nothing to address) or no engine
  /// was detected (nothing to run this against).
  ///
  /// `-t 2`: both engines default `stop` to a SIGTERM-then-wait-then-SIGKILL
  /// sequence (5s / 10s default grace period respectively) — appropriate
  /// for a cooperative container, not for content that reached this path
  /// specifically because it exceeded a call it never legitimately needed
  /// (a stalled, sleeping, or deliberately unresponsive payload should not
  /// get an *extra* 5-10s grace period on top of already having timed
  /// out). 2 seconds is enough for a well-behaved process to flush and
  /// exit; anything still running past that gets SIGKILL.
  fn terminate(&self, scope: &SandboxScope) {
    let Some(engine) = self.engine else { return };
    let Some(name) = &scope.container_name else {
      return;
    };
    let _ = std::process::Command::new(engine.binary())
      .arg("stop")
      .arg("-t")
      .arg("2")
      .arg(name)
      .stdout(StdStdio::null())
      .stderr(StdStdio::null())
      .status();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A CLI that is installed but whose service is down: `--version` works,
  /// anything that talks to the service fails. The probe must say "absent".
  #[cfg(unix)]
  #[test]
  fn probe_reports_installed_but_stopped_engine_as_absent() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("container");
    std::fs::write(
      &fake,
      "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\necho 'apiserver is not running' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    for engine in [ContainerEngine::AppleContainer, ContainerEngine::Podman] {
      assert!(
        !engine_responds(&fake, engine.probe_args()),
        "{engine:?} probe must not pass on a stopped service"
      );
    }
    assert!(engine_responds(&fake, &["--version"]), "fixture sanity");
  }

  #[test]
  fn wrap_command_refuses_net_capability() {
    let backend = ContainerBackend::new("python:alpine");
    if !backend.is_enforcing() {
      eprintln!("skipping: no container engine on this host");
      return;
    }
    let mut cmd = Command::new("python3");
    let scope = SandboxScope::new().with_working_directory(std::env::temp_dir());
    let err = backend
      .wrap_command(&mut cmd, &[Capability::Net], &scope)
      .expect_err("Capability::Net must be refused");
    assert!(matches!(err, SandboxError::Prepare { .. }));
  }

  #[test]
  fn wrap_command_requires_working_directory() {
    let backend = ContainerBackend::new("python:alpine");
    if !backend.is_enforcing() {
      eprintln!("skipping: no container engine on this host");
      return;
    }
    let mut cmd = Command::new("python3");
    let err = backend
      .wrap_command(&mut cmd, &[Capability::Exec], &SandboxScope::new())
      .expect_err("missing working_directory must be refused");
    assert!(matches!(err, SandboxError::Prepare { .. }));
  }

  #[test]
  fn engine_absent_reports_disabled_and_refuses() {
    let backend = ContainerBackend {
      engine: None,
      image: "python:alpine".to_string(),
    };
    assert!(!backend.is_enforcing());
    assert_eq!(
      backend.enforcement_level(),
      crate::sandbox::SandboxEnforcement::Disabled
    );
    let mut cmd = Command::new("python3");
    let scope = SandboxScope::new().with_working_directory(std::env::temp_dir());
    let err = backend
      .wrap_command(&mut cmd, &[Capability::Exec], &scope)
      .expect_err("no engine available must be refused, never silently unwrapped");
    assert!(matches!(err, SandboxError::Unsupported { .. }));
  }
}
