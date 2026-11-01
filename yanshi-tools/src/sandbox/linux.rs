//! Linux sandbox backend backed by seccomp-bpf, layered with Landlock (S3.1)
//! for path-scoped filesystem access.
//!
//! The seccomp filter is compiled once on the host, then re-installed in
//! each child through `pre_exec` (between `fork(2)` and `execve(2)`). The
//! default action is `Allow`; specific syscalls are denied with
//! `Errno(EPERM)` when the corresponding [`Capability`] is missing from the
//! effective set.
//!
//! ## Landlock path scoping (S3.1)
//!
//! seccomp is syscall-scoped, not path-scoped (see "Limits" below) — before
//! this, a child with `FsRead` could read anything the host user could read
//! (`~/.ssh`, `~/.aws`, ...), because path restriction only happened
//! in-process ([`crate::sandbox::SandboxPolicy::is_path_allowed`]), which a
//! subprocess's own code never goes through. [`LinuxSeccompBackend`] now
//! additionally builds a [`landlock::Ruleset`] from the [`SandboxScope`]
//! passed to [`SandboxBackend::wrap_command`] — read-only access to
//! `scope.read_paths` (+ `scope.working_directory`), read-write access to
//! `scope.write_paths` when [`Capability::FsWrite`] is granted (read-only
//! otherwise) — and applies it via `restrict_self()` in the same `pre_exec`
//! closure as the seccomp filter, so both layers are active before the
//! child's `execve` returns.
//!
//! Landlock availability is probed once at construction (kernel >= 5.13,
//! LSM enabled at boot) via a side-effect-free syscall
//! (`probe_landlock_abi`). When unavailable, the seccomp filter still
//! applies (unchanged syscall-level containment) but
//! [`SandboxBackend::enforcement_level`] reports
//! [`SandboxEnforcement::Permissive`] instead of `Enforcing`, since the
//! path-scoping guarantee this module's docs describe no longer holds —
//! the existing three-state pattern already used for "unsupported arch"
//! and the macOS `sandbox-exec`-missing case.
//!
//! ## What this enforces
//!
//! * **No `Net`** → outbound socket creation, `connect`, `bind`, `listen`,
//!   `accept`, `sendto`, `recvfrom` and friends are blocked. A child that
//!   tries `curl` will fail with a network error from libc rather than the
//!   request reaching the network stack.
//! * **No `FsWrite`** → mutating filesystem syscalls (`unlink`, `rmdir`,
//!   `rename`, `mkdir`, `chmod`, `chown`, `truncate`, `link`, `symlink`,
//!   `mknod`) are blocked. `write(2)` itself is **not** blocked because the
//!   child legitimately writes to stdout/stderr. New-file creation through
//!   `openat`/`open`/`creat`/`openat2` is also blocked when the call carries
//!   any of `O_WRONLY` / `O_RDWR` / `O_CREAT` / `O_TRUNC` (per-flag rules,
//!   Q1.1.2). `openat2` cannot be argument-filtered (the flags live in a
//!   `struct open_how` accessed by pointer), so it is denied unconditionally
//!   under `!FsWrite`.
//! * **No `Exec`** → process-creation syscalls (`clone`, `clone3`, `fork`,
//!   `vfork`, `execve`, `execveat`) are denied. In practice the in-process
//!   capability layer already rejects any tool that doesn't grant `Exec`
//!   before we reach the seccomp filter, so these rules are defense in
//!   depth (Q1.1.4): they make the filter's contract match its name even
//!   if a future caller bypasses the registry merge. A child running under
//!   a no-`Exec` filter cannot start at all (the initial `execve` from the
//!   parent fails with `EPERM`) — that is the intended failure mode.
//! * **`io_uring` always denied** → `io_uring_setup`/`io_uring_enter`/
//!   `io_uring_register` are denied unconditionally, regardless of granted
//!   capabilities. Once a ring is armed, the actual read/write/connect/
//!   openat-equivalent work happens inside the kernel off a shared ring
//!   buffer with no corresponding per-operation syscall — a child could
//!   otherwise re-implement network or filesystem-write operations via
//!   `io_uring` SQEs and bypass the per-syscall `Net`/`FsWrite` denials
//!   above entirely. No built-in tool needs `io_uring`, so it is treated
//!   as always out of scope rather than gated behind a capability.
//!
//! ## Limits
//!
//! seccomp filters are syscall-scoped, not path-scoped — Landlock (above)
//! is what adds path-scoped `FsRead`/`FsWrite` containment. Landlock itself
//! has kernel-documented limits (e.g. arbitrary mounts are always denied,
//! and rules only cover the six `AccessFs` rights defined as of ABI V1,
//! which this module targets — see `build_landlock_ruleset`). Path-prefix
//! gating is still enforced in-process too, independent of either kernel
//! layer, by [`crate::sandbox::SandboxPolicy::is_path_allowed`]. Beyond
//! `io_uring` (denied outright above), the deny-list model is inherently
//! incomplete against novel syscall-level bypasses: any future kernel
//! interface that performs privileged work off a non-syscall control path
//! (a new io_uring-like ring, a `bpf(2)` program that reaches network/FS
//! primitives, etc.) is unblocked until this module is updated to name it.
//! A default-allow blocklist can only ever cover known bypass surfaces.
//!
//! ## Architecture support
//!
//! Compiled for `x86_64` and `aarch64`. On other Linux architectures the
//! backend reports itself as non-enforcing rather than installing a filter
//! for the wrong audit arch (which would be a security footgun).
//!
//! ## Resource limits (S3.2)
//!
//! [`SandboxScope::max_memory_bytes`] / `max_pids` are enforced via a
//! cgroup v2 hierarchy (`memory.max` hard-caps resident memory with an
//! OOM-kill on breach; `pids.max` hard-caps the whole descendant process
//! tree, the primary defense against a fork bomb when [`Capability::Exec`]
//! is granted) — see `resolve_cgroup_root` for how the writable cgroup
//! directory is found (root vs. non-root) and `setup_cgroup_for_spawn`
//! for how a per-spawn leaf cgroup is built and the child migrated into it.
//! `max_cpu_secs` does **not** route through cgroups (`cpu.max` is a
//! *rate* cap — a fraction of CPU per period — not the cumulative "total
//! CPU-seconds, then kill" semantics the field name implies); instead it
//! uses `RLIMIT_CPU` via `setrlimit` in the same `pre_exec` closure,
//! exactly like the macOS backend (`crate::sandbox::macos`) does, since
//! POSIX gives `RLIMIT_CPU` exactly the right semantics on both platforms.
//!
//! When no writable cgroup delegation is available (see
//! `resolve_cgroup_root`'s doc comment for the non-root case), a call
//! that requested `max_memory_bytes` / `max_pids` degrades to spawning
//! without them — logged via `tracing::warn!`, matching the same
//! best-effort philosophy as a Landlock ruleset build failure above. This
//! does not change [`SandboxBackend::enforcement_level`] (which reflects
//! whether *anything* spawned through this backend is contained at all,
//! not whether every optional resource limit a specific call asked for
//! could be applied) — a caller that never sets `max_memory_bytes` /
//! `max_pids` is unaffected either way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use landlock::{
  ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetError,
  path_beneath_rules,
};
use seccompiler::{
  BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
  SeccompRule, TargetArch,
};
use tokio::process::Command;

use crate::Capability;

use super::{SandboxBackend, SandboxEnforcement, SandboxError, SandboxScope};

/// Linux seccomp + Landlock backend. Cheap to clone: holds an `Arc` of the
/// compiled BPF; the Landlock ruleset is rebuilt per [`Self::wrap_command`]
/// call since it's scoped to that call's [`SandboxScope`] and effective
/// capabilities (unlike the seccomp filter, whose only per-call inputs are
/// the effective capabilities).
pub struct LinuxSeccompBackend {
  /// `None` means the host arch is unsupported; the backend falls back to
  /// reporting `is_enforcing = false`.
  arch: Option<TargetArch>,
  /// S3.1: highest Landlock ABI version this kernel supports (probed once,
  /// side-effect-free — see [`probe_landlock_abi`]). `None` means Landlock
  /// is unavailable (pre-5.13 kernel, or disabled at boot via `lsm=`);
  /// [`Self::wrap_command`] still applies the seccomp filter but skips
  /// Landlock, and [`Self::enforcement_level`] reports `Permissive`.
  landlock_abi: Option<i32>,
  /// T2.3: leaf cgroup directories created by [`Self::setup_cgroup_for_spawn`]
  /// that haven't yet been confirmed removable. `wrap_command` has no
  /// post-exit hook to run cleanup from (it only runs pre-spawn — see
  /// [`Self::setup_cgroup_for_spawn`]'s doc comment), so instead every call
  /// that creates a *new* leaf first sweeps this list and `rmdir`s whichever
  /// previously-tracked leaves have since become empty (their child exited
  /// and cgroup v2 auto-vacated `cgroup.procs`). This bounds the directory
  /// count to roughly "leaves from calls since the last exited child" rather
  /// than "every spawn this process has ever made".
  tracked_leaf_cgroups: Mutex<Vec<PathBuf>>,
}

impl LinuxSeccompBackend {
  pub fn new() -> Self {
    Self {
      arch: detect_target_arch(),
      landlock_abi: probe_landlock_abi(),
      tracked_leaf_cgroups: Mutex::new(Vec::new()),
    }
  }

  /// Build a per-spawn leaf cgroup (see the free function
  /// [`setup_cgroup_for_spawn`]) and track it for best-effort cleanup on a
  /// future call — see [`Self::tracked_leaf_cgroups`]'s doc comment.
  fn setup_cgroup_for_spawn(&self, scope: &SandboxScope) -> std::io::Result<std::ffi::CString> {
    let (leaf, procs_cstring) = setup_cgroup_for_spawn(scope)?;
    if let Ok(mut tracked) = self.tracked_leaf_cgroups.lock() {
      sweep_removable_leaf_cgroups(&mut tracked);
      tracked.push(leaf);
    }
    Ok(procs_cstring)
  }
}

/// Best-effort cleanup pass over previously-tracked leaf cgroup
/// directories: attempts `rmdir` on each and evicts whichever succeed.
/// `rmdir` on a cgroup v2 directory only succeeds once it has no member
/// processes — a still-populated leaf simply survives to the next sweep,
/// no different from any other best-effort cleanup in this module. Pure
/// filesystem operation, so unit tests exercise the retain/eviction
/// bookkeeping directly against plain directories standing in for leaf
/// cgroups; real per-cgroup kernel enforcement is covered separately by
/// the hardware-gated integration tests in `tests/sandbox_linux.rs`.
fn sweep_removable_leaf_cgroups(tracked: &mut Vec<PathBuf>) {
  tracked.retain(|leaf| std::fs::remove_dir(leaf).is_err());
}

impl Default for LinuxSeccompBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl SandboxBackend for LinuxSeccompBackend {
  fn name(&self) -> &'static str {
    "seccomp"
  }

  fn is_enforcing(&self) -> bool {
    // Landlock availability does not gate this: seccomp's syscall-level
    // containment is fully active either way. `enforcement_level` is where
    // the Landlock nuance (path-scoping is or isn't guaranteed) surfaces.
    self.arch.is_some()
  }

  fn enforcement_level(&self) -> SandboxEnforcement {
    if self.arch.is_none() {
      // We are on Linux but the architecture is not in the supported
      // (`x86_64`, `aarch64`) set. Report Permissive so operators can see
      // that an enforcing backend exists on this platform but isn't active.
      return SandboxEnforcement::Permissive;
    }
    if self.landlock_abi.is_some() {
      SandboxEnforcement::Enforcing
    } else {
      // S3.1: seccomp is still applied, but without Landlock the
      // path-scoped FsRead/FsWrite containment this module's docs promise
      // isn't guaranteed (pre-5.13 kernel, or LSM disabled at boot).
      SandboxEnforcement::Permissive
    }
  }

  fn wrap_command(
    &self,
    command: &mut Command,
    effective_capabilities: &[Capability],
    scope: &SandboxScope,
  ) -> Result<(), SandboxError> {
    let arch = self.arch.ok_or_else(|| SandboxError::Unsupported {
      platform: "linux",
      message: format!(
        "seccomp backend supports x86_64 and aarch64 only; current arch is '{}'",
        std::env::consts::ARCH
      ),
    })?;

    let bpf =
      compile_filter(effective_capabilities, arch).map_err(|err| SandboxError::Prepare {
        message: format!("failed to compile seccomp filter: {err}"),
      })?;

    // S3.1: build the Landlock ruleset here, in the parent, not inside
    // `pre_exec` — `PathFd::new` (via `path_beneath_rules`) opens each
    // path with `open(2)`, which is not guaranteed async-signal-safe, so
    // it must happen before the fork. Only `restrict_self()` (a single
    // `landlock_restrict_self(2)` syscall) runs inside `pre_exec`, mirroring
    // how `compile_filter` happens here while only `apply_filter` (a single
    // `seccomp(2)` syscall) runs in the child.
    let landlock_ruleset = if self.landlock_abi.is_some() {
      match build_landlock_ruleset(effective_capabilities, scope) {
        Ok(ruleset) => Some(ruleset),
        Err(err) => {
          // Best-effort: the seccomp filter below still applies. A failure
          // here (e.g. a scope path that doesn't exist) degrades this
          // spawn to seccomp-only rather than aborting it — matching the
          // "unavailable -> Permissive, not a hard error" contract this
          // module documents for the Landlock layer as a whole.
          tracing::warn!(
            error = %err,
            "failed to build landlock ruleset; child will run with seccomp-only enforcement"
          );
          None
        }
      }
    } else {
      None
    };

    // S3.2: build the per-spawn leaf cgroup here, in the parent — same
    // reasoning as the Landlock ruleset above: `mkdir`/`write` (via
    // `setup_cgroup_for_spawn`) are not async-signal-safe, so all of that
    // happens before the fork. Only the final pid migration (a raw,
    // allocation-free `open`/`write`/`close` — see
    // `migrate_self_into_cgroup`) runs inside `pre_exec`.
    let cgroup_procs_path = if scope.max_memory_bytes.is_some() || scope.max_pids.is_some() {
      match self.setup_cgroup_for_spawn(scope) {
        Ok(path) => Some(path),
        Err(err) => {
          // Best-effort, same "unavailable -> degrade, don't abort"
          // contract as Landlock above: a non-root process with no
          // systemd delegation (see `resolve_cgroup_root`) is the
          // expected common case this hits, not a bug.
          tracing::warn!(
            error = %err,
            "failed to set up cgroup resource limits; spawning without memory/pids limits"
          );
          None
        }
      }
    } else {
      None
    };
    let max_cpu_secs = scope.max_cpu_secs;

    // Share one Arc across all closure invocations (one per spawn).
    let bpf = Arc::new(bpf);
    let mut landlock_ruleset = landlock_ruleset;
    // SAFETY: the closure runs in the forked child between fork and execve.
    // `apply_filter` only calls `prctl` + `seccomp(2)`;
    // `RulesetCreated::restrict_self` only calls `landlock_restrict_self(2)`;
    // `migrate_self_into_cgroup` only calls raw `open`/`write`/`close`
    // (no `std::fs`, no allocation — see its doc comment); `setrlimit` is a
    // single syscall over a stack-local struct. All async-signal-safe. No
    // path is opened for the first time here — `cgroup_procs_path` and the
    // Landlock rule paths were all resolved in the parent, above.
    unsafe {
      command.pre_exec(move || {
        seccompiler::apply_filter(&bpf).map_err(|err| std::io::Error::other(err.to_string()))?;
        if let Some(procs_path) = &cgroup_procs_path {
          // Best-effort: a migration failure shouldn't abort a spawn that
          // seccomp (and possibly Landlock) already contain.
          let _ = migrate_self_into_cgroup(procs_path);
        }
        if let Some(secs) = max_cpu_secs {
          let _ = apply_cpu_rlimit(secs);
        }
        if let Some(ruleset) = landlock_ruleset.take() {
          // Best-effort, same rationale as the build failure above: don't
          // fail the spawn over a Landlock enforcement error once seccomp
          // is already active.
          let _ = ruleset.restrict_self();
        }
        Ok(())
      });
    }
    Ok(())
  }
}

/// Set both the soft and hard limit of `RLIMIT_CPU` to `max_cpu_secs`.
/// Mirrors `crate::sandbox::macos::apply_rlimit` — see [`SandboxScope::
/// max_cpu_secs`]'s doc comment for why this field routes through
/// `RLIMIT_CPU` on both platforms rather than through cgroups here.
fn apply_cpu_rlimit(max_cpu_secs: u64) -> std::io::Result<()> {
  let limit = libc::rlimit {
    rlim_cur: max_cpu_secs as libc::rlim_t,
    rlim_max: max_cpu_secs as libc::rlim_t,
  };
  // SAFETY: `limit` is a fully-initialised, stack-local value; `setrlimit`
  // does not retain the pointer past the call.
  let ret = unsafe { libc::setrlimit(libc::RLIMIT_CPU, &limit) };
  if ret == 0 {
    Ok(())
  } else {
    Err(std::io::Error::last_os_error())
  }
}

/// Determine a writable cgroup v2 directory this process can place child
/// processes into, or `None` if no usable delegation exists for the
/// current uid. Idempotent — safe to call on every spawn.
///
/// - **Root** (uid 0): `/sys/fs/cgroup/yanshi`.
/// - **Non-root**: the systemd user-session delegated slice conventionally
///   owned by the user's own uid (`man systemd.resource-control`'s
///   "Delegation" section — the default on modern systemd distros via
///   `user@.service`'s `Delegate=yes`):
///   `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/yanshi`.
///
/// Returns `None` when the expected parent directory doesn't exist (no
/// systemd user manager, an older distro without delegation, or — the
/// environment this module's own tests were verified against — no
/// systemd at all) rather than erroring; per the TODO's own "非 root 场景
/// 走 systemd delegation 或降级 Permissive" allowance, callers degrade
/// silently for that spawn rather than treat this as fatal.
/// Probe, without side effects beyond `resolve_cgroup_root`'s own
/// idempotent `subtree_control` writes, whether this process can actually
/// delegate a usable cgroup v2 subtree right now. Exposed `pub` (the
/// `linux` module itself is `pub`) so the integration test suite
/// (`tests/sandbox_linux.rs`, a separate crate) can skip the
/// enforcement-behavior tests with a clear message on hosts where
/// delegation isn't available (e.g. a minimal container init with no
/// systemd, where the root cgroup's `cgroup.procs` is non-empty) — mirrors
/// how those tests use the public `enforcement_level()` API to skip the
/// Landlock tests rather than reaching into a private probe.
///
/// R4.7 (2026-07-28 audit): `resolve_cgroup_root().is_some()` alone checks
/// that the delegation *directory* exists with `memory`/`pids` enabled —
/// necessary but not sufficient. cgroup v2 additionally requires write
/// access to the *nearest common ancestor* of the caller's current cgroup
/// and the destination cgroup in order to migrate a task
/// (`cgroup.procs`, per `cgroups(7)`), not just the destination itself.
/// Found on GitHub Actions' `ubuntu-24.04` runner: the job's own process
/// lives under `/sys/fs/cgroup/system.slice/hosted-compute-agent.service`,
/// not under the `user.slice/user-<uid>.slice/user@<uid>.service` tree
/// `resolve_cgroup_root()` targets — so the common ancestor is the cgroup
/// *root* itself, owned by root and unwritable by the runner's unprivileged
/// uid. `resolve_cgroup_root()` still returns `Some` there (the directory
/// genuinely exists and accepts `memory.max`/`pids.max` writes — only the
/// task-migration write fails), so `linux_cgroup_enforces_max_memory_bytes`
/// / `linux_cgroup_enforces_max_pids` used to run and assert real
/// enforcement that can never happen on that class of host. This does
/// **not** affect `wrap_command`'s runtime behavior — `migrate_self_into_
/// cgroup`'s failure there is already best-effort (swallowed, spawn
/// proceeds without the limit) exactly for hosts like this one; only the
/// test-skip decision needed to get more precise. The check below actually
/// attempts the migration — in a throwaway forked child that does nothing
/// but try it and report its own exit code, never touching the calling
/// process — since matching the kernel's own permission logic in userspace
/// (walking cgroup mounts to find the true common ancestor) would be far
/// more fragile than just asking the kernel directly.
pub fn cgroup_v2_delegation_available() -> bool {
  let Some(root) = resolve_cgroup_root() else {
    return false;
  };
  // Must probe a *leaf* under `root`, not `root` itself: `root` ("yanshi")
  // already had `memory`/`pids` delegated to its own `subtree_control` (so
  // real spawns' per-call leaf dirs can use them), and cgroup v2's "no
  // internal processes" rule forbids a cgroup from holding both
  // controller-enabled children *and* member processes at once — writing a
  // pid straight into `root/cgroup.procs` would be rejected on that
  // structural ground alone, independent of the permission question this
  // probe actually exists to answer. Mirrors the leaf-per-spawn shape
  // `setup_cgroup_for_spawn` already uses at runtime.
  let leaf = root.join("delegation-probe");
  if std::fs::create_dir_all(&leaf).is_err() {
    return false;
  }
  let probe_procs = leaf.join("cgroup.procs");
  let Some(probe_procs_str) = probe_procs.to_str() else {
    return false;
  };
  let Ok(probe_procs_cstr) = std::ffi::CString::new(probe_procs_str) else {
    return false;
  };
  can_migrate_a_disposable_child_into(&probe_procs_cstr)
}

/// Forks a trivial child whose only job is to attempt
/// [`migrate_self_into_cgroup`] on itself and exit with a status
/// reflecting whether that write succeeded, then waits for it and reports
/// the result. Never migrates (or risks migrating) the calling process.
fn can_migrate_a_disposable_child_into(procs_path: &std::ffi::CStr) -> bool {
  // SAFETY: single-threaded-safe fork/waitpid usage — the child only calls
  // the already-async-signal-safe `migrate_self_into_cgroup` (raw
  // open/write/close, no allocation) before `_exit`; the parent only waits
  // on the exact pid `fork()` returned.
  unsafe {
    let pid = libc::fork();
    if pid < 0 {
      // Fork itself failed (resource limits, etc.) — conservatively
      // report "can't", matching this function's fail-closed contract.
      return false;
    }
    if pid == 0 {
      let ok = migrate_self_into_cgroup(procs_path).is_ok();
      libc::_exit(if ok { 0 } else { 1 });
    }
    let mut status: libc::c_int = 0;
    if libc::waitpid(pid, &mut status, 0) < 0 {
      return false;
    }
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
  }
}

fn resolve_cgroup_root() -> Option<PathBuf> {
  // SAFETY: `getuid()` takes no arguments and cannot fail.
  let uid = unsafe { libc::getuid() };
  let (parent, subdir) = if uid == 0 {
    let base = PathBuf::from("/sys/fs/cgroup");
    (base.clone(), base.join("yanshi"))
  } else {
    let base = PathBuf::from(format!(
      "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));
    let subdir = base.join("yanshi");
    (base, subdir)
  };
  if !parent.exists() {
    return None;
  }
  // Both levels must actually succeed: cgroup v2's "no internal processes"
  // rule means enabling a controller on `parent`'s `subtree_control` fails
  // with EBUSY if `parent` itself has member processes attached directly
  // (`cgroup.procs` non-empty) — true, for instance, of a minimal
  // container init that never bothered to move itself into a leaf scope
  // the way a full systemd boot sequence does. Rather than optimistically
  // proceed and let the *later* `memory.max` / `pids.max` write in
  // `setup_cgroup_for_spawn` fail instead (same end result, but after
  // wastefully creating a leaf cgroup that can never be usable), check
  // each `enable_controllers` call's own success and bail out immediately.
  if !enable_controllers(&parent, &["memory", "pids"]) {
    return None;
  }
  std::fs::create_dir_all(&subdir).ok()?;
  if !enable_controllers(&subdir, &["memory", "pids"]) {
    return None;
  }
  Some(subdir)
}

/// Enable `controllers` in `cgroup_dir`'s `cgroup.subtree_control`, so
/// cgroups created *under* `cgroup_dir` can actually use them — cgroup v2
/// requires each ancestor to opt a controller in for its descendants.
/// Returns whether every controller that `cgroup_dir` actually lists as
/// available (per its own `cgroup.controllers`) was successfully enabled;
/// a controller neither this kernel nor this cgroup's tier offers at all
/// is not requested in the first place, so its absence doesn't count as a
/// failure — only a `write` that was attempted and rejected does.
fn enable_controllers(cgroup_dir: &Path, controllers: &[&str]) -> bool {
  let available =
    std::fs::read_to_string(cgroup_dir.join("cgroup.controllers")).unwrap_or_default();
  let request: String = controllers
    .iter()
    .filter(|c| available.split_whitespace().any(|a| a == **c))
    .map(|c| format!("+{c}"))
    .collect::<Vec<_>>()
    .join(" ");
  if request.is_empty() {
    return true;
  }
  std::fs::write(cgroup_dir.join("cgroup.subtree_control"), request).is_ok()
}

/// Build a per-spawn leaf cgroup under [`resolve_cgroup_root`]'s directory
/// and apply `scope`'s `max_memory_bytes` / `max_pids` to it (whichever is
/// set — `memory.max` / `pids.max`). Returns the leaf cgroup's own path
/// (T2.3: so [`LinuxSeccompBackend::setup_cgroup_for_spawn`] can track it
/// for later best-effort `rmdir` — see that method's doc comment) alongside
/// its `cgroup.procs` path pre-converted to a `CString`, ready for the
/// allocation-free write [`migrate_self_into_cgroup`] does from inside
/// `pre_exec`.
fn setup_cgroup_for_spawn(scope: &SandboxScope) -> std::io::Result<(PathBuf, std::ffi::CString)> {
  let root = resolve_cgroup_root().ok_or_else(|| {
    std::io::Error::other("no writable cgroup v2 delegation available for this uid")
  })?;

  let unique = format!(
    "spawn-{}-{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map(|d| d.as_nanos())
      .unwrap_or_default()
  );
  let leaf = root.join(unique);
  std::fs::create_dir_all(&leaf)?;
  if let Some(bytes) = scope.max_memory_bytes {
    std::fs::write(leaf.join("memory.max"), bytes.to_string())?;
  }
  if let Some(pids) = scope.max_pids {
    std::fs::write(leaf.join("pids.max"), pids.to_string())?;
  }

  let procs_path = leaf.join("cgroup.procs");
  let procs_path_str = procs_path
    .to_str()
    .ok_or_else(|| std::io::Error::other("cgroup path is not valid UTF-8"))?;
  let procs_cstring = std::ffi::CString::new(procs_path_str)
    .map_err(|_| std::io::Error::other("cgroup path contains a NUL byte"))?;
  Ok((leaf, procs_cstring))
}

/// Write this process's own pid into `cgroup_procs_path` (a leaf cgroup's
/// `cgroup.procs` file), moving it into that cgroup.
///
/// Uses raw `open`/`write`/`close` rather than `std::fs` because this
/// runs inside `pre_exec` — after `fork()`, before `execve()` — where
/// only async-signal-safe operations are safe to call. `std::fs` types
/// may allocate (e.g. `File`'s internal buffering, `String`/`Vec` path
/// handling), which can deadlock if another thread of the *parent* held
/// the allocator's lock at the moment of `fork()`; a multithreaded tokio
/// process is exactly the case where that's not a theoretical concern.
/// Formats the pid into a fixed-size stack buffer for the same reason —
/// no `format!`, no heap.
fn migrate_self_into_cgroup(cgroup_procs_path: &std::ffi::CStr) -> std::io::Result<()> {
  let mut buf = [0u8; 20]; // enough ASCII digits for any pid_t, no NUL needed
  let len = format_pid_into(unsafe { libc::getpid() }, &mut buf);
  // SAFETY: `cgroup_procs_path` is a valid, NUL-terminated C string owned
  // by the caller for the duration of this call; `buf[..len]` is a valid,
  // fully-initialised slice. `open`/`write`/`close` are POSIX
  // async-signal-safe.
  unsafe {
    let fd = libc::open(cgroup_procs_path.as_ptr(), libc::O_WRONLY);
    if fd < 0 {
      return Err(std::io::Error::last_os_error());
    }
    let ret = libc::write(fd, buf.as_ptr().cast(), len);
    libc::close(fd);
    if ret < 0 {
      return Err(std::io::Error::last_os_error());
    }
  }
  Ok(())
}

/// Format `pid` (always non-negative in practice) as ASCII decimal digits
/// into `buf`, returning the number of bytes written. No allocation —
/// safe to call from `pre_exec`. `pid == 0` is handled explicitly since
/// the digit-extraction loop below would otherwise write nothing for it.
fn format_pid_into(pid: libc::pid_t, buf: &mut [u8; 20]) -> usize {
  if pid <= 0 {
    buf[0] = b'0';
    return 1;
  }
  let mut n = pid as u32;
  let mut tmp = [0u8; 20];
  let mut i = 0;
  while n > 0 {
    tmp[i] = b'0' + (n % 10) as u8;
    n /= 10;
    i += 1;
  }
  for j in 0..i {
    buf[j] = tmp[i - 1 - j];
  }
  i
}

/// Build a Landlock ruleset from `scope`: read-only access to
/// `scope.read_paths` and `scope.working_directory`, read-write access to
/// `scope.write_paths` when [`Capability::FsWrite`] is in
/// `effective_capabilities` (read-only otherwise — a write-scoped path a
/// tool isn't allowed to write to should still be readable, not vanish
/// from its view). Targets [`ABI::V1`] (Linux 5.13, the TODO's own stated
/// minimum) rather than probing for a newer ABI to request more access
/// rights: the `landlock` crate's own docs discourage picking `ABI` based
/// on a runtime probe ("can lead to unreliable sandboxing"); requesting a
/// fixed, tested version and letting the crate's best-effort compatibility
/// layer downgrade further is the documented pattern.
fn build_landlock_ruleset(
  effective_capabilities: &[Capability],
  scope: &SandboxScope,
) -> Result<RulesetCreated, RulesetError> {
  let abi = ABI::V1;
  let allow_fs_write = effective_capabilities.contains(&Capability::FsWrite);
  let access_all = AccessFs::from_all(abi);
  let access_read = AccessFs::from_read(abi);
  let write_access = if allow_fs_write {
    access_all
  } else {
    access_read
  };

  let mut created = Ruleset::default().handle_access(access_all)?.create()?;
  // R (2026-07-28 audit): `access_read` bundles `Execute` (landlock crate's
  // `AccessFs::from_read` = `Execute | ReadFile | ReadDir`), and Landlock
  // denies by default once a right is handled — any path *not* covered by a
  // rule below loses Execute too. Before this baseline existed, `wrap_command`
  // only ever granted `scope.read_paths` (`/tmp` + cwd, or an explicit
  // allowlist) and never anything under `/usr/bin`, `/lib`, etc., so once a
  // kernel actually enforced Landlock (ABI V1, Linux 5.13+) no dynamically
  // linked binary could exec at all — not even `/bin/echo` under a fully
  // permissive `SandboxPolicy`. S3's own local verification never caught
  // this because the Linux VM used for it (`docs/RFC_LLM_CODE_EXECUTION.md`'s
  // container-CLI setup) didn't have Landlock support, so every spawn there
  // ran in `SandboxEnforcement::Permissive` (seccomp-only) and this rule set
  // never actually got loaded into the kernel. Mirrors
  // `crate::sandbox::macos::build_profile`'s "bare minimum the child needs
  // to start and link dyld" baseline — same problem, the Linux equivalent of
  // dyld is the ELF interpreter (`ld-linux*.so`) plus libc, and the target
  // binary itself has to be `Execute`-reachable in the first place. Every
  // path below is filtered through `path_beneath_rules`'s existing
  // silently-skip-if-missing behavior (tested by
  // `build_landlock_ruleset_ignores_nonexistent_paths_rather_than_erroring`),
  // so distros missing any one of these (e.g. no separate `/lib64`) just
  // skip that entry rather than failing ruleset construction.
  const BASELINE_EXEC_PATHS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/usr/lib",
    "/usr/lib64",
    "/lib",
    "/lib64",
    "/usr/libexec",
    "/usr/share",
  ];
  created = created.add_rules(path_beneath_rules(BASELINE_EXEC_PATHS, access_read))?;
  // The dynamic linker's cache is a single file directly under `/etc`, not
  // under any of the subpaths above. Grant only this literal file — not all
  // of `/etc` — so `/etc/passwd`, `/etc/shadow`, `/etc/ssh/*`, etc. stay out
  // of view, matching `macos.rs`'s same narrow-literal-grant precedent for
  // `/private/etc/localtime` there (Q1.1.3's rationale applies equally here).
  created = created.add_rules(path_beneath_rules(["/etc/ld.so.cache"], access_read))?;
  created = created.add_rules(path_beneath_rules(&scope.read_paths, access_read))?;
  created = created.add_rules(path_beneath_rules(&scope.write_paths, write_access))?;
  if let Some(dir) = &scope.working_directory {
    created = created.add_rules(path_beneath_rules(std::iter::once(dir), access_read))?;
  }
  Ok(created)
}

/// Highest Landlock ABI version the running kernel supports (1-8+), or
/// `None` when Landlock is unavailable (pre-5.13 kernel, or disabled at
/// boot via the `lsm=` cmdline).
///
/// Uses the raw `landlock_create_ruleset(NULL, 0,
/// LANDLOCK_CREATE_RULESET_VERSION)` syscall directly rather than the
/// `landlock` crate: this specific invocation is documented as a pure
/// version query (no ruleset fd is created, nothing is restricted),
/// whereas every other path through the crate's API — including any
/// attempt to use it for ABI *detection* — is explicitly discouraged by
/// the crate's own docs ([`landlock::ABI`]'s doc comment: constructing an
/// `ABI` from a runtime probe "can lead to unreliable sandboxing where
/// rules might differ between executions"). This probe exists solely to
/// answer `enforcement_level()` *before* any ruleset is built; the actual
/// ruleset applied to a child (`build_landlock_ruleset`) always requests
/// the fixed `ABI::V1` and lets the crate's own best-effort compatibility
/// layer handle any further downgrade.
pub(crate) fn probe_landlock_abi() -> Option<i32> {
  // Stable UAPI constant from `linux/landlock.h` — `1U << 0`. Landlock's
  // ABI is a frozen userspace contract (see the kernel's Landlock
  // documentation on backward compatibility), so hard-coding this bit is
  // as stable as depending on the kernel header itself.
  const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
  // SAFETY: `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`
  // is documented as a pure version query with no side effects — it
  // creates no ruleset, opens no fd, and restricts nothing. `attr = NULL`
  // and `size = 0` are only valid together with this flag.
  let ret = unsafe {
    libc::syscall(
      libc::SYS_landlock_create_ruleset,
      std::ptr::null::<libc::c_void>(),
      0usize,
      LANDLOCK_CREATE_RULESET_VERSION,
    )
  };
  if ret > 0 { Some(ret as i32) } else { None }
}

fn detect_target_arch() -> Option<TargetArch> {
  match std::env::consts::ARCH {
    "x86_64" => Some(TargetArch::x86_64),
    "aarch64" => Some(TargetArch::aarch64),
    _ => None,
  }
}

/// Build a default-allow filter that denies a curated syscall set per
/// missing capability.
fn compile_filter(caps: &[Capability], arch: TargetArch) -> Result<BpfProgram, seccompiler::Error> {
  let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
  let allow_net = caps.contains(&Capability::Net);
  let allow_fs_write = caps.contains(&Capability::FsWrite);
  let allow_exec = caps.contains(&Capability::Exec);

  // The `*_syscall_numbers` helpers return `&'static [i64]`, so the
  // for-loop binding is `&i64`. The seccomp rules map keys on owned
  // `i64`, hence the deref on `nr`.
  if !allow_net {
    for nr in net_syscall_numbers() {
      rules.insert(*nr, vec![]);
    }
  }
  if !allow_fs_write {
    for nr in fs_write_syscall_numbers() {
      rules.insert(*nr, vec![]);
    }
    #[cfg(target_arch = "x86_64")]
    for nr in legacy_fs_write_syscall_numbers() {
      rules.insert(*nr, vec![]);
    }
    install_write_open_rules(&mut rules)?;
  }
  if !allow_exec {
    for nr in process_creation_syscall_numbers() {
      rules.insert(*nr, vec![]);
    }
    // `SYS_fork` / `SYS_vfork` only exist on x86_64; aarch64 routes both
    // through `clone(SIGCHLD, ...)` which is already in the deny list.
    #[cfg(target_arch = "x86_64")]
    {
      rules.insert(libc::SYS_fork, vec![]);
      rules.insert(libc::SYS_vfork, vec![]);
    }
  }

  // Always denied, independent of capabilities — see the module doc's
  // "io_uring always denied" bullet for why a per-syscall net/fs-write
  // deny list can't cover work submitted through io_uring's ring buffer.
  for nr in io_uring_syscall_numbers() {
    rules.insert(*nr, vec![]);
  }

  let filter = SeccompFilter::new(
    rules,
    SeccompAction::Allow,
    SeccompAction::Errno(libc::EPERM as u32),
    arch,
  )?;
  // `TryFrom<SeccompFilter> for BpfProgram` returns `BackendError`;
  // seccompiler 0.5 ships a `From<BackendError> for Error` impl so
  // we can keep this function's error type unified at the boundary.
  TryInto::<BpfProgram>::try_into(filter).map_err(seccompiler::Error::from)
}

/// Syscalls that create or use network sockets. Conservative: covers IPv4,
/// IPv6, and Unix-domain sockets. We deliberately *do not* block `read` /
/// `write` against established fds because we can't generally distinguish
/// socket fds from stdout fds without argument-level filters.
fn net_syscall_numbers() -> &'static [i64] {
  &[
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_sendto,
    libc::SYS_sendmsg,
    libc::SYS_recvfrom,
    libc::SYS_recvmsg,
    libc::SYS_setsockopt,
    libc::SYS_getsockopt,
    libc::SYS_getsockname,
    libc::SYS_getpeername,
    libc::SYS_shutdown,
  ]
}

/// `fchmodat2` (Linux 6.6). Syscalls added since 5.1 share one number on
/// every architecture; libc 0.2.189 exports it for x86_64 only.
const SYS_FCHMODAT2: i64 = 452;

/// Syscalls that mutate the filesystem layout. `write` itself is allowed
/// because the child writes to stdout/stderr through it; new file creation
/// is gated through the openat / creat / mknodat / openat2 path-creation
/// surface (see [`install_write_open_rules`]).
fn fs_write_syscall_numbers() -> &'static [i64] {
  &[
    libc::SYS_unlinkat,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_mkdirat,
    libc::SYS_mknodat,
    libc::SYS_symlinkat,
    libc::SYS_linkat,
    libc::SYS_fchmodat,
    SYS_FCHMODAT2,
    libc::SYS_fchownat,
    libc::SYS_fchmod,
    libc::SYS_fchown,
    libc::SYS_truncate,
    libc::SYS_ftruncate,
    // Landlock doesn't cover metadata changes, so seccomp is the only
    // layer that can stop these (eval 2026-10-07 S6).
    libc::SYS_setxattr,
    libc::SYS_lsetxattr,
    libc::SYS_fsetxattr,
    libc::SYS_removexattr,
    libc::SYS_lremovexattr,
    libc::SYS_fremovexattr,
  ]
}

/// x86_64 keeps the pre-`*at` path syscalls, and glibc there still issues
/// them (`unlink(3)` → `SYS_unlink`, not `unlinkat`), so denying only the
/// `*at` family left `!FsWrite` bypassable (eval 2026-10-07 S6). aarch64
/// never had them.
#[cfg(target_arch = "x86_64")]
fn legacy_fs_write_syscall_numbers() -> &'static [i64] {
  &[
    libc::SYS_unlink,
    libc::SYS_rmdir,
    libc::SYS_rename,
    libc::SYS_mkdir,
    libc::SYS_mknod,
    libc::SYS_symlink,
    libc::SYS_link,
    libc::SYS_chmod,
    libc::SYS_chown,
    libc::SYS_lchown,
  ]
}

/// Syscalls that create new processes or replace the address space.
/// Denied under `!Exec` (Q1.1.4). This deliberately includes `execve` /
/// `execveat`: with these in the deny set the initial `execve` performed
/// by the parent after `fork()` fails with EPERM, which is the correct
/// containment outcome — a tool that doesn't earn the `Exec` capability
/// must not be able to spawn a subprocess at all.
fn process_creation_syscall_numbers() -> &'static [i64] {
  &[
    libc::SYS_clone,
    // `clone3` is a Linux 5.3+ superset of `clone` and would be a bypass
    // route if we only blocked `clone`. libc 0.2.174 exports it for the
    // architectures we support.
    libc::SYS_clone3,
    libc::SYS_execve,
    libc::SYS_execveat,
  ]
}

/// `io_uring_setup`/`io_uring_enter`/`io_uring_register`: a classic
/// bypass path for per-syscall net/fs-write denials, see the module doc's
/// "io_uring always denied" bullet. Denied unconditionally in
/// `compile_filter`, not gated behind any [`Capability`].
fn io_uring_syscall_numbers() -> &'static [i64] {
  &[
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
  ]
}

/// Open-family flags that imply the call will write to a file. We deny any
/// `open` / `openat` whose `flags` argument intersects this mask under
/// `!FsWrite`. The bits are checked individually because seccompiler's
/// `MaskedEq(mask)` matches when `arg & mask == value`; we want "any bit
/// in the mask set", which we express as one rule per bit.
fn write_open_flag_bits() -> &'static [u64] {
  // O_WRONLY (0o1), O_RDWR (0o2), O_CREAT (0o100), O_TRUNC (0o1000).
  // Hard-coded here (rather than read from `libc::O_*` at runtime) because
  // the seccomp filter is built once on the host and applied in the child;
  // libc constants are `c_int` and pulling them into a `const fn` context
  // adds no value over the well-known POSIX/Linux numeric values.
  const O_WRONLY: u64 = 0o1;
  const O_RDWR: u64 = 0o2;
  const O_CREAT: u64 = 0o100;
  const O_TRUNC: u64 = 0o1000;
  &[O_WRONLY, O_RDWR, O_CREAT, O_TRUNC]
}

/// Register seccomp rules that deny `open`/`openat`/`creat`/`openat2` when
/// they imply a write. Called from [`compile_filter`] under `!FsWrite`.
///
/// * `openat(dirfd, path, flags, ...)` → `flags` is argument index 2. One
///   rule per bit in [`write_open_flag_bits`] (rules are ORed within the
///   same syscall).
/// * `open(path, flags, ...)` → `flags` is argument index 1. Only emitted
///   on `x86_64`, since `aarch64` does not export `SYS_open` (it's been
///   replaced by `openat` system-wide).
/// * `creat(path, mode)` → always implies `O_WRONLY | O_CREAT | O_TRUNC`.
///   Denied unconditionally (x86_64 only — same reason as `open`).
/// * `openat2(dirfd, path, how, size)` → `how` is a pointer to
///   `struct open_how`; seccomp cannot dereference. Denied unconditionally
///   to avoid a bypass via the newer syscall.
fn install_write_open_rules(
  rules: &mut BTreeMap<i64, Vec<SeccompRule>>,
) -> Result<(), seccompiler::Error> {
  for &mask in write_open_flag_bits() {
    let cond_openat = SeccompCondition::new(
      2,
      SeccompCmpArgLen::Dword,
      SeccompCmpOp::MaskedEq(mask),
      mask,
    )?;
    rules
      .entry(libc::SYS_openat)
      .or_default()
      .push(SeccompRule::new(vec![cond_openat])?);

    #[cfg(target_arch = "x86_64")]
    {
      let cond_open = SeccompCondition::new(
        1,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::MaskedEq(mask),
        mask,
      )?;
      rules
        .entry(libc::SYS_open)
        .or_default()
        .push(SeccompRule::new(vec![cond_open])?);
    }
  }

  // `openat2` flags live in a pointed-to struct; can't filter by arg, so
  // deny the syscall outright under !FsWrite.
  rules.insert(libc::SYS_openat2, vec![]);

  // `creat` always writes; deny unconditionally on platforms that ship it.
  #[cfg(target_arch = "x86_64")]
  {
    rules.insert(libc::SYS_creat, vec![]);
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn arch_detection_supports_known_targets() {
    let detected = detect_target_arch();
    if cfg!(target_arch = "x86_64") {
      assert_eq!(detected, Some(TargetArch::x86_64));
    } else if cfg!(target_arch = "aarch64") {
      assert_eq!(detected, Some(TargetArch::aarch64));
    }
  }

  #[test]
  fn filter_compiles_for_both_capability_sets() {
    let arch = detect_target_arch().expect("running on a supported arch");
    let permissive = compile_filter(&[Capability::Net, Capability::FsWrite], arch).unwrap();
    let restrictive = compile_filter(&[], arch).unwrap();
    // The restrictive filter must be larger because it carries deny rules.
    assert!(restrictive.len() > permissive.len());
  }

  #[test]
  fn write_open_rules_emit_per_flag_under_no_fs_write() {
    // Build the rule map directly to assert openat carries one rule per
    // write-relevant flag bit, plus the syscall numbers we deny outright.
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    install_write_open_rules(&mut rules).expect("rules compile under no-fs-write");

    let openat = rules
      .get(&libc::SYS_openat)
      .expect("openat rules must be present");
    assert_eq!(openat.len(), write_open_flag_bits().len());

    assert!(
      rules.get(&libc::SYS_openat2).is_some_and(|r| r.is_empty()),
      "openat2 must be denied unconditionally (empty-vec marker)"
    );

    #[cfg(target_arch = "x86_64")]
    {
      let open = rules.get(&libc::SYS_open).expect("open rules on x86_64");
      assert_eq!(open.len(), write_open_flag_bits().len());
      assert!(
        rules.get(&libc::SYS_creat).is_some_and(|r| r.is_empty()),
        "creat must be denied unconditionally on x86_64"
      );
    }
  }

  /// Q1.1.4 regression: under `!Exec` the filter must populate the
  /// process-creation syscall deny set. We can't easily exercise the
  /// runtime deny path (a no-`Exec` sandbox would fail to spawn its own
  /// child by design, which is the whole point), so we assert rule shape
  /// via [`compile_filter`] instead.
  #[test]
  fn process_creation_syscalls_denied_when_exec_capability_absent() {
    let arch = detect_target_arch().expect("running on a supported arch");

    // With Exec, none of these rules should be present.
    let with_exec = compile_filter(&[Capability::Exec], arch).unwrap();
    let no_exec = compile_filter(&[], arch).unwrap();
    assert!(
      no_exec.len() > with_exec.len(),
      "no-Exec filter must carry more deny rules than Exec-enabled filter"
    );

    // Spot-check by rebuilding the rule map directly. Going through the
    // BpfProgram opaque slice would tie the test to seccompiler internals.
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in process_creation_syscall_numbers() {
      rules.insert(*nr, vec![]);
    }
    assert!(rules.contains_key(&libc::SYS_clone));
    assert!(rules.contains_key(&libc::SYS_clone3));
    assert!(rules.contains_key(&libc::SYS_execve));
    assert!(rules.contains_key(&libc::SYS_execveat));
  }

  /// W1.6 regression: `io_uring` is a known seccomp bypass — a child can
  /// resubmit `connect`/`openat`-equivalent work through the ring instead
  /// of the syscalls this filter denies above. Unlike `Net`/`FsWrite`/
  /// `Exec`, no capability legitimately unlocks it, so it must stay in
  /// the deny set no matter what capabilities are granted.
  #[test]
  fn io_uring_syscalls_are_always_denied_regardless_of_capabilities() {
    let arch = detect_target_arch().expect("running on a supported arch");
    // Grant every capability the filter understands — if io_uring denial
    // were mistakenly gated behind a capability check, this is exactly
    // the filter that would leak it.
    let fully_permissive = compile_filter(
      &[Capability::Net, Capability::FsWrite, Capability::Exec],
      arch,
    )
    .unwrap();
    let truly_empty_program: BpfProgram = SeccompFilter::new(
      BTreeMap::new(),
      SeccompAction::Allow,
      SeccompAction::Errno(libc::EPERM as u32),
      arch,
    )
    .unwrap()
    .try_into()
    .unwrap();
    assert!(
      fully_permissive.len() > truly_empty_program.len(),
      "a filter with every capability granted must still carry the unconditional io_uring deny rules"
    );

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in io_uring_syscall_numbers() {
      rules.insert(*nr, vec![]);
    }
    assert!(rules.contains_key(&libc::SYS_io_uring_setup));
    assert!(rules.contains_key(&libc::SYS_io_uring_enter));
    assert!(rules.contains_key(&libc::SYS_io_uring_register));
  }

  #[test]
  fn backend_is_enforcing_on_supported_arch() {
    let backend = LinuxSeccompBackend::new();
    if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
      assert!(backend.is_enforcing());
      assert_eq!(backend.name(), "seccomp");
    }
  }

  // ── S3.1: Landlock ─────────────────────────────────────────────────

  #[test]
  fn probe_landlock_abi_returns_a_positive_version_or_none() {
    // Can't assert a specific outcome — depends on the test host's
    // kernel — but the probe must never panic, and any `Some` value must
    // be a real positive ABI number (matches the syscall's documented
    // return-value contract: positive version, or -1/errno).
    if let Some(version) = probe_landlock_abi() {
      assert!(
        version > 0,
        "landlock ABI version must be positive: {version}"
      );
    }
  }

  #[test]
  fn build_landlock_ruleset_succeeds_with_empty_scope() {
    // An empty SandboxScope (no read/write paths, no working directory)
    // is a degenerate-but-legal input: the ruleset just won't grant any
    // path-scoped access. Must not error, even when Landlock itself is
    // unavailable on this kernel (best-effort mode degrades silently at
    // the `landlock` crate level; `create()` only fails on a genuine
    // misuse, not on kernel unavailability).
    let scope = SandboxScope::new();
    let result = build_landlock_ruleset(&[], &scope);
    assert!(
      result.is_ok(),
      "empty scope must build a (possibly no-op) ruleset: {result:?}"
    );
  }

  #[test]
  fn build_landlock_ruleset_succeeds_with_real_paths() {
    let temp = tempfile::TempDir::new().expect("create temp dir");
    let read_dir = temp.path().join("read");
    let write_dir = temp.path().join("write");
    std::fs::create_dir_all(&read_dir).unwrap();
    std::fs::create_dir_all(&write_dir).unwrap();

    let scope = SandboxScope::new()
      .with_read_paths([read_dir])
      .with_write_paths([write_dir.clone()])
      .with_working_directory(write_dir);
    let result = build_landlock_ruleset(&[Capability::FsWrite], &scope);
    assert!(
      result.is_ok(),
      "real, existing paths must build a ruleset: {result:?}"
    );
  }

  #[test]
  fn build_landlock_ruleset_ignores_nonexistent_paths_rather_than_erroring() {
    // `path_beneath_rules` (from the `landlock` crate) silently skips
    // paths it can't open — matches this backend's own "best-effort,
    // degrade rather than abort the spawn" philosophy for the Landlock
    // layer (see `wrap_command`'s doc comment on the build-failure path).
    let scope =
      SandboxScope::new().with_read_paths(["/yanshi-sandbox-test-path-that-does-not-exist"]);
    let result = build_landlock_ruleset(&[], &scope);
    assert!(
      result.is_ok(),
      "a missing path must not fail ruleset construction: {result:?}"
    );
  }

  // ── T2.3: leaf cgroup cleanup bookkeeping ───────────────────────────

  #[test]
  fn tracked_leaf_cgroups_do_not_accumulate_once_each_becomes_removable() {
    let temp = tempfile::TempDir::new().expect("create temp dir");
    let mut tracked: Vec<PathBuf> = Vec::new();

    // Simulate 5 spawns whose prior leaf has already emptied out by the
    // time the next spawn's sweep runs (the common case: short-lived
    // children). Each iteration's sweep must reclaim the previous leaf
    // before pushing the new one, so the tracked count never grows past 1
    // regardless of how many spawns have happened.
    for i in 0..5 {
      let leaf = temp.path().join(format!("leaf-{i}"));
      std::fs::create_dir_all(&leaf).expect("create leaf dir");
      sweep_removable_leaf_cgroups(&mut tracked);
      tracked.push(leaf);
      assert_eq!(
        tracked.len(),
        1,
        "tracked leaves must not accumulate once each becomes removable, iteration {i}: {tracked:?}"
      );
    }
  }

  #[test]
  fn tracked_leaf_cgroups_keep_a_still_populated_leaf_until_it_empties() {
    let temp = tempfile::TempDir::new().expect("create temp dir");
    let mut tracked: Vec<PathBuf> = Vec::new();

    let busy_leaf = temp.path().join("busy");
    std::fs::create_dir_all(&busy_leaf).expect("create leaf dir");
    // Stand-in for a leaf cgroup that still has a member process: any
    // directory entry makes `rmdir` fail, exactly like a cgroup v2
    // directory with a non-empty `cgroup.procs`.
    std::fs::write(busy_leaf.join("still-running-marker"), b"").expect("create marker file");
    tracked.push(busy_leaf.clone());

    sweep_removable_leaf_cgroups(&mut tracked);
    assert_eq!(
      tracked,
      vec![busy_leaf.clone()],
      "a still-populated leaf must survive a sweep, not be dropped"
    );

    // The child "exits" — the leaf empties out — and the next sweep must
    // reclaim it.
    std::fs::remove_file(busy_leaf.join("still-running-marker")).expect("remove marker file");
    sweep_removable_leaf_cgroups(&mut tracked);
    assert!(
      tracked.is_empty(),
      "an emptied leaf must be rmdir'd on the next sweep: {tracked:?}"
    );
    assert!(
      !busy_leaf.exists(),
      "the leaf directory itself must actually be removed from disk"
    );
  }
}
