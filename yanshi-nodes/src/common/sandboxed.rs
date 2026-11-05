//! Run an external renderer binary under the platform OS sandbox
//! (Y3.3 / Y3.4): pandoc, typst, lualatex, pdftocairo.
//!
//! `ShellTool` grants every allowed path both read and write and parses a
//! command line; renderers need read-only access to their own install
//! trees and fonts, write access only to one work directory, and an argv
//! vector. So this calls the `SandboxBackend` directly with a scope built
//! for exactly that, a scrubbed environment and a timeout.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use yanshi_core::error::YanshiError;
use yanshi_tools::{
  Capability,
  sandbox::{SandboxScope, default_backend},
};

/// Directories searched after PATH: TeX Live's macOS link farm and the
/// usual package-manager prefixes, so a renderer installed outside the
/// service's PATH is still found.
const FALLBACK_DIRS: &[&str] = &["/Library/TeX/texbin", "/opt/homebrew/bin", "/usr/local/bin"];

/// Environment variables passed through to the renderer.
const KEEP_ENV: &[&str] = &[
  "PATH", "HOME", "USER", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
];

/// First executable named `name` on PATH, then in `FALLBACK_DIRS`.
pub fn find_tool(name: &str) -> Option<PathBuf> {
  let path_dirs = std::env::var_os("PATH")
    .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
    .unwrap_or_default();
  path_dirs
    .into_iter()
    .chain(FALLBACK_DIRS.iter().map(PathBuf::from))
    .map(|dir| dir.join(name))
    .find(|candidate| candidate.is_file())
}

/// The trees a binary loads its libraries and data from, for read-only
/// access: the install prefix of the path as found
/// (`/opt/homebrew/bin/pandoc` → `/opt/homebrew`, where Homebrew's
/// `opt/` links live) and of its canonical target
/// (`/Library/TeX/texbin/lualatex` → `/usr/local/texlive/2026`), plus
/// the Homebrew root for a binary inside `Cellar/`.
pub fn install_prefixes(binary: &Path) -> Vec<PathBuf> {
  let prefix = |path: &Path| -> Option<PathBuf> {
    let mut dir = path.parent()?;
    loop {
      if dir.file_name().is_some_and(|name| name == "bin") {
        return dir.parent().map(Path::to_path_buf);
      }
      match dir.parent() {
        Some(parent) if parent != Path::new("/") => dir = parent,
        _ => return path.parent().map(Path::to_path_buf),
      }
    }
  };
  let mut prefixes: Vec<PathBuf> = prefix(binary).into_iter().collect();
  if let Ok(canonical) = binary.canonicalize()
    && let Some(p) = prefix(&canonical)
    && !prefixes.contains(&p)
  {
    prefixes.push(p);
  }
  // A keg-only Homebrew formula lives in `<root>/Cellar/<name>/<ver>` and
  // links against other formulae through `<root>/opt/`; a regular one
  // already gets `<root>` from its `<root>/bin` link.
  let roots: Vec<PathBuf> = prefixes
    .iter()
    .filter_map(|p| {
      p.ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "Cellar"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
    })
    .collect();
  for root in roots {
    if !prefixes.contains(&root) {
      prefixes.push(root);
    }
  }
  prefixes
}

/// Font directories renderers search; read-only.
pub fn font_dirs() -> Vec<PathBuf> {
  let mut dirs: Vec<PathBuf> = [
    "/Library/Fonts",
    "/usr/share/fonts",
    "/usr/local/share/fonts",
    "/etc/fonts",
  ]
  .iter()
  .map(PathBuf::from)
  .collect();
  if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
    dirs.extend([
      home.join("Library/Fonts"),
      home.join(".local/share/fonts"),
      home.join(".fonts"),
    ]);
  }
  dirs
}

/// One sandboxed invocation.
pub struct SandboxedRun<'a> {
  /// Node id, for error messages.
  pub node: &'a str,
  pub program: PathBuf,
  pub args: Vec<OsString>,
  /// Working directory; always writable.
  pub workdir: &'a Path,
  /// Extra read-only paths (install prefixes, fonts, caches).
  pub read_paths: Vec<PathBuf>,
  /// Extra writable paths (e.g. a run-level TeX cache).
  pub write_paths: Vec<PathBuf>,
  /// Extra environment variables on top of `KEEP_ENV`.
  pub env: Vec<(&'static str, OsString)>,
  pub timeout: Duration,
}

/// Run `spec` and return its output whatever the exit status (callers
/// that feed errors back to an LLM need stderr). `Err` only when the
/// process could not be sandboxed, started, or finished in time.
pub async fn run(spec: SandboxedRun<'_>) -> Result<std::process::Output, YanshiError> {
  let mut writable = vec![spec.workdir.to_path_buf(), std::env::temp_dir()];
  writable.extend(spec.write_paths);
  let mut read_paths = spec.read_paths;
  read_paths.extend(install_prefixes(&spec.program));
  read_paths.extend(writable.iter().cloned());
  let scope = SandboxScope::new()
    .with_read_paths(read_paths)
    .with_write_paths(writable)
    .with_working_directory(spec.workdir);

  let mut command = tokio::process::Command::new(&spec.program);
  command
    .args(&spec.args)
    .current_dir(spec.workdir)
    .kill_on_drop(true);
  default_backend()
    .wrap_command(
      &mut command,
      &[Capability::FsRead, Capability::FsWrite, Capability::Exec],
      &scope,
    )
    .map_err(|e| YanshiError::NodeExecutionFailed {
      message: format!("{}: preparing the sandbox failed: {e}", spec.node),
    })?;
  // After `wrap_command`: the macOS backend rebuilds the command.
  command.env_clear();
  for key in KEEP_ENV {
    if let Some(value) = std::env::var_os(key) {
      command.env(key, value);
    }
  }
  for (key, value) in spec.env {
    command.env(key, value);
  }
  tokio::time::timeout(spec.timeout, command.output())
    .await
    .map_err(|_| YanshiError::TimeoutExceeded {
      duration_ms: spec.timeout.as_millis() as u64,
    })?
    .map_err(|e| YanshiError::NodeExecutionFailed {
      message: format!(
        "{}: starting {} failed: {e}",
        spec.node,
        spec.program.display()
      ),
    })
}

/// The last `lines` lines of a process's stderr (or stdout when stderr
/// is empty), for error messages.
pub fn output_tail(output: &std::process::Output, lines: usize) -> String {
  let stderr = String::from_utf8_lossy(&output.stderr);
  let text = if stderr.trim().is_empty() {
    String::from_utf8_lossy(&output.stdout).into_owned()
  } else {
    stderr.into_owned()
  };
  let tail: Vec<&str> = text.trim().lines().rev().take(lines).collect();
  tail.into_iter().rev().collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A keg-only formula (`ffmpeg-full`) resolves into
  /// `Cellar/<name>/<version>`, but links against libraries under the
  /// Homebrew root's `opt/`; without the root the dynamic loader is
  /// blocked before `main`.
  #[test]
  fn install_prefixes_of_a_keg_only_binary_include_the_homebrew_root() {
    let prefixes = install_prefixes(Path::new(
      "/nonexistent/homebrew/Cellar/ffmpeg-full/9.0.2_1/bin/ffmpeg",
    ));
    assert!(
      prefixes.contains(&PathBuf::from("/nonexistent/homebrew")),
      "{prefixes:?}"
    );
  }

  #[test]
  fn install_prefix_is_the_parent_of_the_nearest_bin_dir() {
    let prefixes = install_prefixes(Path::new("/nonexistent/opt/homebrew/bin/pandoc"));
    assert_eq!(prefixes, vec![PathBuf::from("/nonexistent/opt/homebrew")]);
    let prefixes = install_prefixes(Path::new(
      "/nonexistent/texlive/2026/bin/universal-darwin/luahbtex",
    ));
    assert_eq!(prefixes, vec![PathBuf::from("/nonexistent/texlive/2026")]);
    let prefixes = install_prefixes(Path::new("/nonexistent/TeX/texbin/lualatex"));
    assert_eq!(prefixes, vec![PathBuf::from("/nonexistent/TeX/texbin")]);
  }
}
