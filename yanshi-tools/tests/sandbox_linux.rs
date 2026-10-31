//! Linux seccomp-bpf enforcement integration tests.
//!
//! These tests assume the `python3` interpreter is present (the standard CI
//! image satisfies this). They confirm that when `ShellTool` runs under
//! `with_os_sandbox()`:
//!
//! 1. A baseline command that does no I/O outside stdout still succeeds.
//! 2. A program that creates a network socket fails with `EPERM` because
//!    `Capability::Net` is absent from the tool's required capability set.
//!
//! The seccomp filter is installed via `pre_exec`; the kernel returns
//! `EPERM` to denied syscalls. Python surfaces this as `PermissionError`,
//! which we match in stderr.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use serde_json::json;
use yanshi_tools::builtin::ShellTool;
use yanshi_tools::sandbox::{
  LinuxSeccompBackend, SandboxBackend as _, SandboxEnforcement, SandboxPolicy, SandboxScope,
};
use yanshi_tools::{Capability, Tool};

fn permissive_shell_with_sandbox() -> ShellTool {
  ShellTool::new(Arc::new(SandboxPolicy::permissive())).with_os_sandbox()
}

fn python3_available() -> bool {
  std::process::Command::new("python3")
    .arg("--version")
    .output()
    .map(|out| out.status.success())
    .unwrap_or(false)
}

/// S3.1: whether this kernel actually supports Landlock. `enforcement_level`
/// already folds this in (`Enforcing` only when Landlock is available;
/// `Permissive` when seccomp-only) — reusing it here avoids duplicating the
/// backend's own probe logic in the test suite. Not every CI / container /
/// VM kernel ships `CONFIG_SECURITY_LANDLOCK`, so the path-scoping tests
/// below skip (with a clear message) rather than fail on such hosts.
fn landlock_enforcing() -> bool {
  LinuxSeccompBackend::new().enforcement_level() == SandboxEnforcement::Enforcing
}

/// S3.2: whether this host can actually delegate a usable cgroup v2
/// subtree. Not every environment can — a minimal container init with no
/// systemd never isolates itself into a leaf scope, so the root cgroup's
/// `cgroup.procs` stays non-empty and `subtree_control` writes fail with
/// `EBUSY` (cgroup v2's "no internal processes" rule). The memory/pids
/// enforcement tests below assert real kernel-enforced behavior, so they
/// skip (with a clear message) rather than fail when delegation itself
/// isn't available here — same rationale as `landlock_enforcing()` above.
fn cgroup_delegation_available() -> bool {
  yanshi_tools::sandbox::linux::cgroup_v2_delegation_available()
}

#[tokio::test]
async fn linux_seccomp_allows_baseline_echo() {
  let tool = permissive_shell_with_sandbox();
  let result = tool
    .execute(json!({"command": "echo hello-from-seccomp"}))
    .await
    .expect("echo should succeed under seccomp filter");

  assert!(
    !result.is_error,
    "expected success, got error output: {}",
    result.content
  );
  assert!(
    result.content.contains("hello-from-seccomp"),
    "stdout did not contain expected token: {}",
    result.content
  );
}

#[tokio::test]
async fn linux_seccomp_blocks_socket_when_net_capability_absent() {
  if !python3_available() {
    eprintln!("skipping: python3 not on PATH");
    return;
  }

  let tool = permissive_shell_with_sandbox();
  // ShellTool's required capability set is `[Exec]` only; without Net, the
  // seccomp filter denies `socket(2)` with EPERM.
  let cmd = "python3 -c 'import socket; s = socket.socket(); print(\"opened\")'";
  let result = tool
    .execute(json!({"command": cmd}))
    .await
    .expect("tool call must complete");

  assert!(
    result.is_error,
    "expected seccomp to block socket(), but got success: {}",
    result.content
  );
  assert!(
    !result.content.contains("opened"),
    "socket() unexpectedly succeeded: {}",
    result.content
  );
}

/// Q1.1.4 deny-flow regression: a child process whose seccomp filter is
/// built without `Capability::Exec` must not be able to start at all. The
/// kernel applies the BPF filter via `pre_exec` after the fork but before
/// the parent's `execve(target_binary)`. Without `execve` in the allow set
/// that initial transition fails with `EPERM`, surfacing as an `io::Error`
/// from `Command::output()`.
///
/// This guarantees that even if a future change skipped the in-process
/// capability check, the kernel filter would still gate process creation.
#[tokio::test]
async fn linux_seccomp_no_exec_filter_blocks_child_spawn() {
  let backend = LinuxSeccompBackend::new();
  if !backend.is_enforcing() {
    eprintln!("skipping: seccomp backend is not enforcing on this arch");
    return;
  }

  let mut cmd = tokio::process::Command::new("/bin/true");
  backend
    .wrap_command(&mut cmd, &[], &SandboxScope::new())
    .expect("wrap_command compiles a no-Exec filter");

  // Spawning must fail (the std lib reports the EPERM from execve back to
  // the parent through the post-fork status pipe). We don't pin a specific
  // ErrorKind because Rust's Command can report this as PermissionDenied
  // or Other depending on the libc layer.
  let outcome = cmd.output().await;
  assert!(
    outcome.is_err(),
    "spawn under no-Exec seccomp filter must fail; instead got: {:?}",
    outcome
      .as_ref()
      .ok()
      .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
  );
}

/// Q1.1.2 deny-flow regression: without `FsWrite` the seccomp filter must
/// block `openat(AT_FDCWD, "/tmp/...", O_WRONLY|O_CREAT, ...)` even though
/// the legacy filter only denied path-mutation syscalls (`unlinkat`,
/// `linkat`, etc.). Pre-fix this test would have observed the file being
/// created successfully.
#[tokio::test]
async fn linux_seccomp_blocks_openat_with_o_creat_when_fs_write_absent() {
  if !python3_available() {
    eprintln!("skipping: python3 not on PATH");
    return;
  }

  let pid = std::process::id();
  let path = format!("/tmp/yanshi_sandbox_openat_creat_blocked_{pid}.txt");
  let _ = std::fs::remove_file(&path);

  let tool = permissive_shell_with_sandbox();
  // The single quotes pin the python source so the argv parser keeps the
  // metacharacters literal. The python program uses os.open() (which goes
  // directly through openat(2)) with O_WRONLY|O_CREAT — the seccomp filter
  // must convert this into EPERM.
  let cmd = format!(
    "python3 -c 'import os; \
       fd = os.open(\"{path}\", os.O_WRONLY | os.O_CREAT, 0o600); \
       os.write(fd, b\"breach\"); \
       os.close(fd); \
       print(\"created\")'"
  );
  let result = tool
    .execute(json!({"command": cmd}))
    .await
    .expect("tool call must complete");

  assert!(
    result.is_error,
    "expected seccomp to block openat(O_CREAT), but got success: {}",
    result.content
  );
  assert!(
    !result.content.contains("created"),
    "openat(O_CREAT) unexpectedly succeeded: {}",
    result.content
  );
  assert!(
    !std::path::Path::new(&path).exists(),
    "seccomp failed: file '{path}' was created despite missing FsWrite capability"
  );
}

/// Eval 2026-10-07 S6: x86_64 glibc issues the legacy `mkdir`/`chmod`/...
/// syscalls, not the `*at` forms the filter used to deny, and Landlock
/// never covers chmod/chown/xattr. Both targets sit where Landlock alone
/// would allow the change (chmod is uncovered; cwd is in the write
/// scope), so only the seccomp deny set can stop them.
#[tokio::test]
async fn linux_seccomp_blocks_mkdir_and_chmod_when_fs_write_absent() {
  use std::os::unix::fs::PermissionsExt;
  if !python3_available() {
    eprintln!("skipping: python3 not on PATH");
    return;
  }

  let pid = std::process::id();
  let file = format!("/tmp/yanshi_sandbox_chmod_blocked_{pid}.txt");
  std::fs::write(&file, b"x").unwrap();
  std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
  let dir = std::env::current_dir()
    .unwrap()
    .join(format!("yanshi_sandbox_mkdir_blocked_{pid}"));
  let _ = std::fs::remove_dir(&dir);

  let tool = permissive_shell_with_sandbox();
  let chmod = tool
    .execute(json!({"command": format!("python3 -c 'import os; os.chmod(\"{file}\", 0o777)'")}))
    .await
    .expect("tool call must complete");
  let mkdir = tool
    .execute(json!({"command": format!("python3 -c 'import os; os.mkdir(\"{}\")'", dir.display())}))
    .await
    .expect("tool call must complete");

  let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
  let dir_created = dir.exists();
  let _ = std::fs::remove_file(&file);
  let _ = std::fs::remove_dir(&dir);
  assert_eq!(mode, 0o600, "chmod escaped the sandbox: {}", chmod.content);
  assert!(chmod.is_error, "{}", chmod.content);
  assert!(!dir_created, "mkdir escaped the sandbox: {}", mkdir.content);
  assert!(mkdir.is_error, "{}", mkdir.content);
}

// ── S3.1: Landlock path-scoped filesystem containment ──────────────────

/// S3.1 literal regression scenario: a child confined to a specific
/// directory via `SandboxScope` must not be able to read a file outside
/// that scope — the gap seccomp alone left open (seccomp is syscall-
/// scoped: any `open`/`read` of an already-world-readable path succeeds
/// regardless of which directory it lives in). Skips (doesn't fail) on a
/// kernel without Landlock, since this specifically asserts what S3.1
/// *adds* on top of seccomp, not what seccomp already covered — some CI /
/// container / VM kernels don't ship `CONFIG_SECURITY_LANDLOCK`.
#[tokio::test]
async fn linux_landlock_blocks_reads_outside_the_allowed_scope() {
  if !landlock_enforcing() {
    eprintln!("skipping: this kernel does not support Landlock (CONFIG_SECURITY_LANDLOCK)");
    return;
  }
  if !python3_available() {
    eprintln!("skipping: python3 not on PATH");
    return;
  }

  let temp = tempfile::TempDir::new().expect("create temp dir");
  let policy = SandboxPolicy {
    allowed_paths: vec![temp.path().to_path_buf()],
    allow_all_commands: true,
    ..SandboxPolicy::default()
  };
  let tool = ShellTool::new(Arc::new(policy)).with_os_sandbox();

  // /etc/hostname is world-readable on any stock Linux distro and lives
  // well outside the temp dir the policy scoped this child to.
  let cmd = "python3 -c 'print(open(\"/etc/hostname\").read())'";
  let result = tool
    .execute(json!({"command": cmd}))
    .await
    .expect("tool call must complete");

  assert!(
    result.is_error,
    "expected Landlock to block reading outside the allowed scope, but got success: {}",
    result.content
  );
}

/// Sibling positive case: a read *inside* the allowed scope must still
/// succeed — S3.1 must add path scoping without turning into a blanket
/// deny of everything the policy did grant.
#[tokio::test]
async fn linux_landlock_allows_reads_inside_the_allowed_scope() {
  if !landlock_enforcing() {
    eprintln!("skipping: this kernel does not support Landlock (CONFIG_SECURITY_LANDLOCK)");
    return;
  }
  if !python3_available() {
    eprintln!("skipping: python3 not on PATH");
    return;
  }

  let temp = tempfile::TempDir::new().expect("create temp dir");
  let file_path = temp.path().join("inside.txt");
  std::fs::write(&file_path, "hello-from-inside-scope").expect("write fixture file");

  let policy = SandboxPolicy {
    allowed_paths: vec![temp.path().to_path_buf()],
    allow_all_commands: true,
    ..SandboxPolicy::default()
  };
  let tool = ShellTool::new(Arc::new(policy)).with_os_sandbox();

  let cmd = format!(
    "python3 -c 'print(open(\"{}\").read())'",
    file_path.display()
  );
  let result = tool
    .execute(json!({"command": cmd}))
    .await
    .expect("tool call must complete");

  assert!(
    !result.is_error,
    "expected read inside the allowed scope to succeed, got error: {}",
    result.content
  );
  assert!(
    result.content.contains("hello-from-inside-scope"),
    "stdout did not contain expected content: {}",
    result.content
  );
}

/// R (2026-07-28 audit) regression: on a kernel that actually enforces
/// Landlock, exec of a dynamically-linked system binary (`python3`) under a
/// scope that only grants a scratch temp dir must still succeed, because
/// `build_landlock_ruleset` now always grants `Execute`+`ReadFile`+`ReadDir`
/// on the standard system binary/library directories regardless of what the
/// caller's `SandboxPolicy` scoped. Before that baseline existed, Landlock
/// handled the `Execute` right workspace-wide (`AccessFs::from_all`/
/// `from_read` both include it) but only ever granted it on the caller's own
/// `allowed_paths` — so `python3` itself (living in `/usr/bin`, never in the
/// scoped temp dir) had `Execute` denied, and every seccomp+Landlock spawn
/// failed regardless of policy. This is exactly the failure mode that let
/// this ship silently: the S3 track's own local verification ran on a
/// kernel without Landlock support, so `SandboxEnforcement::Permissive`
/// kept this rule set from ever actually loading there; the bug only
/// surfaced once this crate's tests ran on real CI for the first time.
#[tokio::test]
async fn linux_landlock_allows_exec_of_system_binary_outside_the_allowed_scope() {
  if !landlock_enforcing() {
    eprintln!("skipping: this kernel does not support Landlock (CONFIG_SECURITY_LANDLOCK)");
    return;
  }
  if !python3_available() {
    eprintln!("skipping: python3 not on PATH");
    return;
  }

  let temp = tempfile::TempDir::new().expect("create temp dir");
  // Mirrors the two tests above: policy only ever scopes a scratch temp
  // dir, never `/usr/bin` (where `python3` actually lives).
  let policy = SandboxPolicy {
    allowed_paths: vec![temp.path().to_path_buf()],
    allow_all_commands: true,
    ..SandboxPolicy::default()
  };
  let tool = ShellTool::new(Arc::new(policy)).with_os_sandbox();

  let result = tool
    .execute(json!({"command": "python3 -c 'print(\"exec-still-works\")'"}))
    .await
    .expect("tool call must complete");

  assert!(
    !result.is_error,
    "expected python3 to exec successfully under a Landlock-restricted scope, got error: {}",
    result.content
  );
  assert!(
    result.content.contains("exec-still-works"),
    "stdout did not contain expected content: {}",
    result.content
  );
}

// ── S3.2: resource limits via cgroup v2 / RLIMIT_CPU ────────────────────

fn cc_available() -> bool {
  std::process::Command::new("cc")
    .arg("--version")
    .output()
    .map(|out| out.status.success())
    .unwrap_or(false)
}

/// Compile a tiny, dependency-free C fixture and return its path. Used
/// throughout the S3.2 tests below to isolate each resource-limit
/// mechanism from `ShellTool`/`python3`-specific concerns (interpreter
/// availability, argv-mode quoting) that aren't what these tests are
/// about — same rationale as `tests/sandbox_macos.rs`'s
/// `compile_busy_loop_binary`.
fn compile_c_fixture(dir: &std::path::Path, name: &str, source: &str) -> std::path::PathBuf {
  let src = dir.join(format!("{name}.c"));
  std::fs::write(&src, source).expect("write fixture source");
  let bin = dir.join(name);
  let status = std::process::Command::new("cc")
    .arg("-O2")
    .arg("-o")
    .arg(&bin)
    .arg(&src)
    .status()
    .expect("invoke cc");
  assert!(status.success(), "cc failed to compile {name}");
  bin
}

/// S3.2 literal regression: `max_memory_bytes` must actually bound
/// resident memory — a process that touches (not just reserves) more
/// memory than its budget must be OOM-killed by the cgroup, not merely
/// slowed down or left to swap.
#[tokio::test]
async fn linux_cgroup_enforces_max_memory_bytes() {
  if !cgroup_delegation_available() {
    eprintln!(
      "skipping: no writable cgroup v2 delegation available on this host (no systemd / root cgroup has member processes)"
    );
    return;
  }
  if !cc_available() {
    eprintln!("skipping: no working `cc` to compile the memory-hog test fixture");
    return;
  }
  let dir = tempfile::TempDir::new().expect("create temp dir");
  // Allocate 256 MiB and actually touch every page (`memset`) — a bare
  // `malloc` without touching pages may not count against `memory.max`
  // (Linux only charges pages to the cgroup when they're first faulted
  // in), so this must write to the memory, not just reserve it.
  let bin = compile_c_fixture(
    dir.path(),
    "memory_hog",
    r#"
#include <stdlib.h>
#include <string.h>
int main(void) {
  size_t n = 256UL * 1024 * 1024;
  char *p = malloc(n);
  if (!p) { return 1; }
  memset(p, 1, n);
  return 0;
}
"#,
  );

  let backend = LinuxSeccompBackend::new();
  // 32 MiB budget — comfortably below the 256 MiB the fixture touches.
  // R4.5 (2026-07-28 audit): the compiled fixture lives in `dir`, a path
  // Landlock never grants Execute on unless the scope says so explicitly
  // (same bug class R4.2 fixed for system binaries — this is the test's
  // own scope under-granting its own tempdir, not a `wrap_command` defect).
  let scope = SandboxScope::new()
    .with_max_memory_bytes(32 * 1024 * 1024)
    .with_read_paths([dir.path()]);
  let mut cmd = tokio::process::Command::new(&bin);
  backend
    .wrap_command(&mut cmd, &[Capability::Exec], &scope)
    .expect("wrap_command must succeed");

  let status = cmd.status().await.expect("child must spawn");
  assert!(
    !status.success(),
    "expected the cgroup memory.max limit to OOM-kill the fixture, but it exited successfully"
  );
}

/// S3.2 literal regression: `max_pids` must actually bound the process
/// count — a fork-bomb-shaped fixture must hit `EAGAIN`/`ENOMEM` well
/// before it manages to spawn as many children as it tries to.
#[tokio::test]
async fn linux_cgroup_enforces_max_pids() {
  if !cgroup_delegation_available() {
    eprintln!(
      "skipping: no writable cgroup v2 delegation available on this host (no systemd / root cgroup has member processes)"
    );
    return;
  }
  if !cc_available() {
    eprintln!("skipping: no working `cc` to compile the fork-bomb test fixture");
    return;
  }
  let dir = tempfile::TempDir::new().expect("create temp dir");
  // Try to fork 200 children (each just sleeping briefly, never reaped
  // eagerly) and print how many `fork()` calls actually succeeded before
  // exiting. Without a pids limit this would trivially print 200.
  let bin = compile_c_fixture(
    dir.path(),
    "fork_bomb",
    r#"
#include <stdio.h>
#include <unistd.h>
#include <sys/wait.h>
int main(void) {
  int ok = 0;
  for (int i = 0; i < 200; i++) {
    pid_t pid = fork();
    if (pid == 0) {
      usleep(50000);
      _exit(0);
    } else if (pid > 0) {
      ok++;
    } else {
      break;
    }
  }
  printf("%d\n", ok);
  fflush(stdout);
  // Best-effort reap so we don't leave 200 zombies behind if the limit
  // didn't trip for some reason.
  for (int i = 0; i < ok; i++) { wait(NULL); }
  return 0;
}
"#,
  );

  let backend = LinuxSeccompBackend::new();
  // R4.5: same tempdir-grant reasoning as linux_cgroup_enforces_max_memory_bytes.
  let scope = SandboxScope::new()
    .with_max_pids(5)
    .with_read_paths([dir.path()]);
  let mut cmd = tokio::process::Command::new(&bin);
  backend
    .wrap_command(&mut cmd, &[Capability::Exec], &scope)
    .expect("wrap_command must succeed");

  let output = cmd.output().await.expect("child must spawn");
  let stdout = String::from_utf8_lossy(&output.stdout);
  let forked: i32 = stdout.trim().parse().unwrap_or_else(|_| {
    panic!(
      "fixture did not print a fork count; stdout={stdout:?} stderr={:?}",
      String::from_utf8_lossy(&output.stderr)
    )
  });
  assert!(
    forked < 200,
    "expected the cgroup pids.max=5 limit to cap fork() well below 200, but all 200 succeeded"
  );
}

/// S3.2: a scope with no resource limits set must not attempt any cgroup
/// setup at all — spawning must behave identically to pre-S3.2, not
/// silently degrade because `resolve_cgroup_root()` found nothing.
#[tokio::test]
async fn linux_no_resource_limits_requested_spawns_normally() {
  let backend = LinuxSeccompBackend::new();
  let scope = SandboxScope::new();
  let mut cmd = tokio::process::Command::new("/bin/true");
  backend
    .wrap_command(&mut cmd, &[Capability::Exec], &scope)
    .expect("wrap_command must succeed");
  let status = cmd.status().await.expect("child must spawn");
  assert!(
    status.success(),
    "/bin/true must still succeed with no resource limits requested"
  );
}
