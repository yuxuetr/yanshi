//! `code_exec` real-isolation integration tests (S4.2).
//!
//! These drive the actual `CodeExecTool` end to end against a real
//! container engine — proving genuine isolation (network blocked, host
//! filesystem unreachable, resource limits enforced by the kernel), not
//! just that the tool compiles and returns success. Skips gracefully (with
//! a clear message) on a host with no container engine on PATH, mirroring
//! the `landlock_enforcing()` / `cgroup_delegation_available()` skip-guard
//! pattern already established for S3.1/S3.2's real-enforcement tests.

#![cfg(target_os = "macos")]

use serde_json::json;
use yanshi_tools::builtin::CodeExecTool;
use yanshi_tools::{Tool, ToolError};

fn container_engine_available() -> bool {
  CodeExecTool::new()
    .sandbox_status()
    .is_some_and(|status| status.enforcement == yanshi_tools::SandboxEnforcement::Enforcing)
}

/// `code_exec_tears_down_its_ephemeral_workdir` below scans the shared
/// system temp directory for entries matching `code_exec`'s workdir
/// prefix — a real flake was observed (this session) when Rust's default
/// parallel test runner ran that scan concurrently with a sibling test's
/// own `CodeExecTool` call: the sibling's still-in-flight workdir showed up
/// as a false "leak". Every test in this file takes this lock for its
/// duration so the scan never overlaps another call creating one of these
/// directories.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Regression test for a resource-exhaustion gap found via adversarial
/// code review this session: `Child::wait_with_output()` accumulates
/// stdout/stderr into unbounded host-side `Vec`s, so a payload that just
/// writes continuously (never sleeping, so it's not caught by
/// `code_exec_orphaned_container_is_stopped_on_timeout`'s scenario, and
/// not necessarily CPU-bound enough to trip the ulimit quickly either)
/// could grow the *host* yanshi process's own memory well past
/// whatever the container's own memory cap bounds inside the guest.
#[tokio::test]
async fn code_exec_output_is_capped_independent_of_container_memory_limit() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  // Writes ~1 MiB of stdout — comfortably more than the 64 KiB cap, but
  // small enough to finish well within the CPU/wall-clock limits so this
  // test exercises the *capture* bound specifically, not the timeout path.
  let code = "print('x' * 1024 * 1024)";
  let result = tool
    .execute(json!({"code": code}))
    .await
    .expect("tool call must complete");
  assert!(
    !result.is_error,
    "expected success, got: {}",
    result.content
  );
  assert!(
    result.content.len() <= 64 * 1024,
    "expected captured output to be capped at 64 KiB, got {} bytes",
    result.content.len()
  );
}

#[tokio::test]
async fn code_exec_runs_real_computation() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  let result = tool
    .execute(json!({"code": "print(sum(range(1000)))"}))
    .await
    .expect("tool call must complete");
  assert!(
    !result.is_error,
    "expected success, got: {}",
    result.content
  );
  assert_eq!(result.content, "499500");
}

/// The single most important test: proves the code actually runs inside a
/// separate kernel, not just as a subprocess of the test runner. A bug that
/// silently downgraded `ContainerBackend::wrap_command` into a no-op would
/// still pass `code_exec_runs_real_computation` above (the host almost
/// certainly has *some* `python3`) — only an assertion about which OS
/// actually executed the code catches that.
#[tokio::test]
async fn code_exec_runs_inside_a_separate_linux_kernel_not_the_host() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  let result = tool
    .execute(json!({"code": "import platform; print(platform.system())"}))
    .await
    .expect("tool call must complete");
  assert!(
    !result.is_error,
    "expected success, got: {}",
    result.content
  );
  assert_eq!(
    result.content, "Linux",
    "code_exec must run inside a Linux container/VM even on a macOS host — \
     if this reports anything else, isolation is not actually active"
  );
}

#[tokio::test]
async fn code_exec_blocks_all_network_access() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  let code = r#"
import socket
import urllib.request

results = []
try:
    socket.setdefaulttimeout(3)
    socket.gethostbyname("example.com")
    results.append("DNS_RESOLVED")
except Exception:
    results.append("DNS_BLOCKED")
try:
    urllib.request.urlopen("http://example.com", timeout=3)
    results.append("HTTP_REACHABLE")
except Exception:
    results.append("HTTP_BLOCKED")
try:
    s = socket.create_connection(("8.8.8.8", 53), timeout=3)
    s.close()
    results.append("SOCKET_REACHABLE")
except Exception:
    results.append("SOCKET_BLOCKED")
print(",".join(results))
"#;
  let result = tool
    .execute(json!({"code": code}))
    .await
    .expect("tool call must complete");
  assert!(
    !result.is_error,
    "expected success, got: {}",
    result.content
  );
  assert_eq!(
    result.content, "DNS_BLOCKED,HTTP_BLOCKED,SOCKET_BLOCKED",
    "code_exec must have zero network access by default"
  );
}

#[tokio::test]
async fn code_exec_host_filesystem_is_unreachable() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  // `/Users` only exists on the macOS host, never inside a Linux container
  // image — its absence proves the host root filesystem isn't bind-mounted
  // anywhere reachable, not just that this particular file wasn't granted.
  let code = r#"
import os
print("HOST_ROOT_VISIBLE" if os.path.exists("/Users") else "HOST_ROOT_HIDDEN")
"#;
  let result = tool
    .execute(json!({"code": code}))
    .await
    .expect("tool call must complete");
  assert!(
    !result.is_error,
    "expected success, got: {}",
    result.content
  );
  assert_eq!(result.content, "HOST_ROOT_HIDDEN");
}

/// T2.3 regression: the container's root filesystem must be read-only, so
/// only the `/workspace` bind mount (the ephemeral workdir) is writable —
/// closing the gap where adversarial llm-generated code could otherwise
/// write anywhere inside the disposable container's rootfs.
#[tokio::test]
async fn code_exec_root_filesystem_is_read_only_outside_workspace() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  let code = r#"
try:
    with open("/etc/code_exec_write_probe", "w") as f:
        f.write("x")
    print("ROOTFS_WRITABLE")
except OSError:
    print("ROOTFS_READ_ONLY")
with open("/workspace/ok.txt", "w") as f:
    f.write("x")
print("WORKSPACE_WRITE_OK")
"#;
  let result = tool
    .execute(json!({"code": code}))
    .await
    .expect("tool call must complete");
  assert!(
    !result.is_error,
    "expected success, got: {}",
    result.content
  );
  assert_eq!(
    result.content.trim(),
    "ROOTFS_READ_ONLY\nWORKSPACE_WRITE_OK",
    "expected the rootfs write to fail while the /workspace write still succeeds"
  );
}

#[tokio::test]
async fn code_exec_enforces_memory_limit() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  // Comfortably above the 256 MiB default cap; must be denied rather than
  // silently allowed to consume unbounded host memory.
  let code = "data = bytearray(512 * 1024 * 1024)\nprint('should not get here')";
  let started = std::time::Instant::now();
  let result = tool
    .execute(json!({"code": code}))
    .await
    .expect("tool call must complete");
  let elapsed = started.elapsed();
  assert!(
    result.is_error,
    "expected the 256 MiB cap to reject a 512 MiB allocation, got success: {}",
    result.content
  );
  // Not just any error: an unrelated spawn/flag failure also sets
  // is_error, and a prior version of this assertion accepted that as
  // false-positive "proof" the cap fired — the same leniency already fixed
  // once for the pids test after it masked a real Podman `--uid` bug.
  // The cap shows up one of two ways: Python's own `MemoryError` (Podman's
  // cgroup limit, and older `container` releases), or — `container` 1.6.1,
  // whose `-m` sizes the microVM itself — the guest OOM killer's SIGKILL,
  // exit 137, before Python can raise. Verified against the CLI directly:
  // the same 512 MiB allocation exits 137 within ~1s under `-m 256m` and
  // succeeds under `-m 1024m`. The only other SIGKILL source here is the
  // 30 CPU-second ulimit, which cannot fire within 10s.
  let python_oom = result.content.contains("MemoryError");
  let guest_oom_kill =
    result.content.contains("code 137") && elapsed < std::time::Duration::from_secs(10);
  assert!(
    python_oom || guest_oom_kill,
    "expected a MemoryError or a prompt exit 137 proving the 256 MiB cap fired, got an \
     unrelated failure after {elapsed:?} instead: {}",
    result.content
  );

  // Control: an allocation well under the cap succeeds, so the failure
  // above is the cap and not a sandbox that fails every allocation.
  let result = tool
    .execute(json!({"code": "data = bytearray(64 * 1024 * 1024)\nprint('allocated')"}))
    .await
    .expect("tool call must complete");
  assert!(
    !result.is_error && result.content.contains("allocated"),
    "expected a 64 MiB allocation to fit under the 256 MiB cap, got: {}",
    result.content
  );
}

#[tokio::test]
async fn code_exec_enforces_cpu_limit() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  // A busy loop with no I/O — must be killed by the CPU-seconds ulimit
  // well before the 30s spawn timeout would otherwise fire.
  let code = "\
x = 0
while True:
    x += 1
";
  let started = std::time::Instant::now();
  let result = tool
    .execute(json!({"code": code}))
    .await
    .expect("tool call must complete");
  let elapsed = started.elapsed();
  assert!(
    result.is_error,
    "expected the CPU-seconds ulimit to kill the busy loop, got success: {}",
    result.content
  );
  // Not just any error — see the identical rationale on
  // `code_exec_enforces_memory_limit`. A `--ulimit cpu=` kill reports as
  // exit code 137 (128 + SIGKILL) on Apple's `container` CLI (confirmed
  // this session), with no Python-level traceback (the process is killed
  // outright, never gets to print one) — distinguishable from any other
  // failure this call could hit.
  assert!(
    result.content.contains("code 137"),
    "expected exit code 137 (SIGKILL via the CPU ulimit) proving the cap fired, got an \
     unrelated failure instead: {}",
    result.content
  );
  assert!(
    elapsed < std::time::Duration::from_secs(40),
    "expected the ulimit (30s CPU budget) to fire noticeably before the 45s wall-clock \
     spawn timeout backstop, took {elapsed:?}"
  );
}

/// Regression test for a critical bug found via adversarial code review
/// this session: killing the `container`/`podman` CLI **client** process
/// on timeout does not stop the container it launched (confirmed
/// empirically outside this test suite — `SIGKILL`-ing a running
/// `container run` client left its container `running` indefinitely).
/// A payload that sleeps rather than burns CPU/memory never trips the
/// CPU-seconds ulimit or the memory cap — only the wall-clock timeout
/// backstop can end it, and that backstop must actually stop the
/// container, not just abandon it running on the host.
#[tokio::test]
async fn code_exec_orphaned_container_is_stopped_on_timeout() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  let code = "import time\ntime.sleep(120)\n";
  let started = std::time::Instant::now();
  let result = tool.execute(json!({"code": code})).await;
  let elapsed = started.elapsed();
  assert!(
    matches!(result, Err(ToolError::ExecutionFailed { .. })),
    "expected a timeout error, got {result:?}"
  );
  assert!(
    elapsed < std::time::Duration::from_secs(60),
    "expected the 45s wall-clock timeout to fire, took {elapsed:?}"
  );

  // Give `terminate()`'s `stop` a brief moment to take effect, then
  // confirm no container from this exact test process is still running.
  // Matched by pid prefix (this test process's own pid, embedded in the
  // container name code_exec.rs generates) so this is immune to other
  // concurrent containers on the host.
  tokio::time::sleep(std::time::Duration::from_secs(2)).await;
  let prefix = format!("yanshi-code-exec-{}-", std::process::id());
  let listing = std::process::Command::new("container")
    .arg("list")
    .output()
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    .unwrap_or_default();
  assert!(
    !listing.contains(&prefix),
    "expected the timed-out container to be stopped, but it's still listed as running:\n{listing}"
  );
}

#[tokio::test]
async fn code_exec_enforces_pids_limit() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  let tool = CodeExecTool::new();
  let code = r#"
import os
ok = 0
for i in range(60):
    pid = os.fork()
    if pid == 0:
        os._exit(0)
    ok += 1
print(ok)
for _ in range(ok):
    try:
        os.wait()
    except ChildProcessError:
        pass
"#;
  let result = tool
    .execute(json!({"code": code}))
    .await
    .expect("tool call must complete");
  // Either the fixture's own `os.fork()` starts failing once the pids limit
  // is hit (a `BlockingIOError`/`OSError` traceback, `is_error`) or it
  // successfully counts how many succeeded before running out — either way
  // the count must be well below the 60 attempted, proving the cap actually
  // bit. Any *other* error (e.g. an unrelated engine flag failure — this
  // exact "either/or" shape previously masked a real `--uid` flag bug on
  // Podman by treating any `is_error` as proof of enforcement) must fail
  // loudly instead of being silently accepted as the expected outcome.
  if result.is_error {
    assert!(
      result.content.contains("BlockingIOError") || result.content.contains("OSError"),
      "expected a fork-related error proving the pids limit fired, got an unrelated \
       failure instead: {}",
      result.content
    );
  } else {
    let forked: i32 = result
      .content
      .trim()
      .parse()
      .unwrap_or_else(|_| panic!("expected a fork count, got: {}", result.content));
    assert!(
      forked < 60,
      "expected the pids limit to cap fork() well below 60, got {forked}"
    );
  }
}

#[tokio::test]
async fn code_exec_tears_down_its_ephemeral_workdir() {
  let _guard = TEST_LOCK.lock().await;
  if !container_engine_available() {
    eprintln!("skipping: no container engine ('container' or 'podman') on PATH");
    return;
  }
  // Filter by code_exec's distinguishable workdir prefix rather than
  // diffing the whole system temp dir — the latter is racy against
  // unrelated churn from other processes (e.g. the container engine's own
  // transient bookkeeping files), which caused a real flake here.
  fn code_exec_workdirs() -> std::collections::HashSet<std::path::PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
      .expect("read temp dir")
      .filter_map(|e| e.ok().map(|e| e.path()))
      .filter(|p| {
        p.file_name()
          .and_then(|n| n.to_str())
          .is_some_and(|n| n.starts_with("yanshi-code-exec-"))
      })
      .collect()
  }

  let before = code_exec_workdirs();

  let tool = CodeExecTool::new();
  let result = tool
    .execute(json!({"code": "print('done')"}))
    .await
    .expect("tool call must complete");
  assert!(
    !result.is_error,
    "expected success, got: {}",
    result.content
  );

  let after = code_exec_workdirs();
  let leaked: Vec<_> = after.difference(&before).collect();
  assert!(
    leaked.is_empty(),
    "code_exec's ephemeral workdir must be torn down after execute() returns, leaked: {leaked:?}"
  );
}
