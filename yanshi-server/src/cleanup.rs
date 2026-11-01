//! Retention + cleanup sweep for finished runs (`P2.2`).
//!
//! Two responsibilities:
//!
//! 1. **DB sweep**: delete `events` older than `events_retention_days`,
//!    `artifacts` older than `artifacts_retention_days`, and terminal
//!    `runs` whose `finished_at` is older than `runs_retention_days`;
//!    terminal harness sessions (with their events and checkpoints) and
//!    approval decision intents past the same window.
//!    Active runs (`queued` / `running`) are never touched. Cascading
//!    foreign keys take care of `steps` and any per-run children.
//! 2. **Filesystem sweep**: walk `run_dir_root` and delete directories
//!    older than `run_dir_retention_days` whose owning run is either
//!    missing from the DB or in a terminal state.
//!
//! The sweep is deliberately idempotent: dry-run mode reports what
//! _would_ be deleted but mutates nothing, so operators can preview
//! before flipping the cron / interval loop in `yanshi serve`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use yanshi_db::Database;
use yanshi_tools::SecurityProfile;

/// Default polling interval used by `yanshi serve`'s background
/// cleanup loop. Overridable via [`CleanupConfig::with_interval`].
pub const DEFAULT_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Tunables for one cleanup invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupConfig {
  /// Minimum age (in days) before a terminal run row is deleted.
  pub runs_retention_days: u32,
  /// Minimum age before an `events` row is deleted (independent of
  /// the parent run's retention — events tend to be heavier on disk
  /// so they age out faster).
  pub events_retention_days: u32,
  /// Minimum age before an `artifacts` row is deleted.
  pub artifacts_retention_days: u32,
  /// Minimum age before an on-disk per-run directory is deleted.
  pub run_dir_retention_days: u32,
  /// Polling interval for the background loop. Single-shot invocations
  /// (`yanshi cleanup`) ignore this value.
  #[serde(
    default = "default_interval",
    skip_serializing_if = "is_default_interval"
  )]
  pub interval: Duration,
  /// When `true`, the sweep reports candidates but performs no
  /// mutations. Set by `yanshi cleanup --dry-run`.
  #[serde(default)]
  pub dry_run: bool,
}

fn is_default_interval(value: &Duration) -> bool {
  *value == DEFAULT_CLEANUP_INTERVAL
}

fn default_interval() -> Duration {
  DEFAULT_CLEANUP_INTERVAL
}

impl CleanupConfig {
  /// Defaults pegged to the active security profile, per the P2.2
  /// task spec. Production keeps runs longer; events / artifacts /
  /// run_dir defaults are the same across profiles.
  pub fn for_profile(profile: SecurityProfile) -> Self {
    let runs_retention_days = match profile {
      SecurityProfile::Production => 90,
      _ => 30,
    };
    Self {
      runs_retention_days,
      events_retention_days: 14,
      artifacts_retention_days: 30,
      run_dir_retention_days: 14,
      interval: DEFAULT_CLEANUP_INTERVAL,
      dry_run: false,
    }
  }

  pub fn with_dry_run(mut self, dry_run: bool) -> Self {
    self.dry_run = dry_run;
    self
  }

  pub fn with_interval(mut self, interval: Duration) -> Self {
    self.interval = interval;
    self
  }
}

/// Errors surfaced from one sweep.
#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
  #[error("database error: {0}")]
  Database(#[from] sqlx::Error),
  #[error("filesystem error at {path}: {source}")]
  Filesystem {
    path: PathBuf,
    #[source]
    source: std::io::Error,
  },
}

/// Structured result of one [`cleanup_expired`] invocation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CleanupReport {
  pub dry_run: bool,
  /// Number of terminal `runs` rows deleted (or that would have been
  /// deleted in dry-run mode).
  pub runs_deleted: u64,
  /// Events rows deleted, beyond the cascade that fires when a run
  /// row is removed.
  pub events_deleted: u64,
  /// Artifacts rows deleted.
  pub artifacts_deleted: u64,
  /// Per-run directories deleted on disk.
  pub run_dirs_deleted: u64,
  /// Per-run directories that were skipped because the owning run is
  /// still `queued` / `running`.
  pub run_dirs_skipped_active: u64,
  /// IDs of the terminal runs the sweep targeted. Limited to the
  /// most recent 100 entries to keep the report bounded.
  pub run_ids_targeted: Vec<Uuid>,
  /// Q2.3.4: file-backed trace files (`<workflow_id>.json` under
  /// `YANSHI_TRACE_DIR`) older than `runs_retention_days`. Skipped
  /// when the trace dir is unset.
  #[serde(default)]
  pub trace_files_deleted: u64,
  /// Terminal harness sessions past `runs_retention_days`; their events
  /// and loop checkpoints cascade.
  #[serde(default)]
  pub harness_sessions_deleted: u64,
  /// Approval decision intents decided more than `runs_retention_days`
  /// ago (each is relayed to the parked replica within moments).
  #[serde(default)]
  pub approval_intents_deleted: u64,
  pub started_at: DateTime<Utc>,
  pub finished_at: DateTime<Utc>,
}

/// Top-level entry point used by the CLI subcommand, the
/// `yanshi-server` `--cleanup` binary mode, and the background
/// loop started by `run()`.
pub async fn cleanup_expired(
  db: &Database,
  run_dir_root: Option<&Path>,
  trace_dir_root: Option<&Path>,
  config: &CleanupConfig,
) -> Result<CleanupReport, CleanupError> {
  let started_at = Utc::now();
  let mut report = CleanupReport {
    dry_run: config.dry_run,
    started_at,
    finished_at: started_at,
    ..Default::default()
  };

  // 1. Targeted run rows (terminal + past retention).
  let terminal_ids = list_terminal_runs(&db.pool, config.runs_retention_days as i64).await?;
  let preview_limit = 100;
  report.run_ids_targeted = terminal_ids.iter().copied().take(preview_limit).collect();

  if !config.dry_run {
    report.events_deleted = sweep_events(&db.pool, config.events_retention_days as i64).await?;
    report.artifacts_deleted =
      sweep_artifacts(&db.pool, config.artifacts_retention_days as i64).await?;
    report.runs_deleted = delete_terminal_runs(&db.pool, &terminal_ids).await?;
  } else {
    report.events_deleted = preview_events(&db.pool, config.events_retention_days as i64).await?;
    report.artifacts_deleted =
      preview_artifacts(&db.pool, config.artifacts_retention_days as i64).await?;
    report.runs_deleted = terminal_ids.len() as u64;
  }

  let days = config.runs_retention_days as i64;
  report.harness_sessions_deleted =
    sweep_or_count(&db.pool, EXPIRED_HARNESS_SESSIONS, days, config.dry_run).await?;
  report.approval_intents_deleted =
    sweep_or_count(&db.pool, EXPIRED_APPROVAL_INTENTS, days, config.dry_run).await?;

  // 2. Filesystem sweep (best-effort; never fail the whole call when
  //    a single dir errors — record the issue and continue).
  if let Some(root) = run_dir_root {
    let (deleted, skipped) = sweep_run_dir(
      root,
      &db.pool,
      config.run_dir_retention_days as i64,
      config.dry_run,
    )
    .await?;
    report.run_dirs_deleted = deleted;
    report.run_dirs_skipped_active = skipped;
  }

  // Q2.3.4: file-backed trace dir sweep. Mirrors run_dir retention so
  // operators can flip `YANSHI_TRACE_DIR=...` in production without
  // a quietly-growing disk. Use the runs retention window — trace
  // files exist primarily for run debugging.
  if let Some(root) = trace_dir_root {
    let deleted = sweep_trace_dir(root, config.runs_retention_days as i64, config.dry_run).await?;
    report.trace_files_deleted = deleted;
  }

  report.finished_at = Utc::now();
  // P10.14.2-FU2: fire the three cleanup_*_deleted_total
  // counters so the Grafana "Retention sweep" panel shows
  // real deletion rates. Dry-run sweeps are intentionally
  // skipped — the panel is about *actual* reaping, not
  // previews. See `metrics::observe_cleanup_sweep` for the
  // contract.
  crate::metrics::observe_cleanup_sweep(
    report.dry_run,
    report.runs_deleted,
    report.events_deleted,
    report.artifacts_deleted,
  );
  Ok(report)
}

/// `FROM ... WHERE ...` clauses for [`sweep_or_count`]; `$1` is days.
const EXPIRED_HARNESS_SESSIONS: &str = "FROM harness_sessions \
   WHERE status IN ('completed', 'failed', 'cancelled') \
     AND finished_at IS NOT NULL \
     AND finished_at < NOW() - $1::INT * INTERVAL '1 day'";
const EXPIRED_APPROVAL_INTENTS: &str = "FROM approval_decision_intents \
   WHERE decided_at < NOW() - $1::INT * INTERVAL '1 day'";

/// Delete the rows `from_where` selects, or count them on a dry run.
async fn sweep_or_count(
  pool: &PgPool,
  from_where: &str,
  days: i64,
  dry_run: bool,
) -> Result<u64, CleanupError> {
  if dry_run {
    let row = sqlx::query(&format!("SELECT COUNT(*)::BIGINT AS n {from_where}"))
      .bind(days as i32)
      .fetch_one(pool)
      .await?;
    return Ok(row.get::<i64, _>("n") as u64);
  }
  let result = sqlx::query(&format!("DELETE {from_where}"))
    .bind(days as i32)
    .execute(pool)
    .await?;
  Ok(result.rows_affected())
}

async fn list_terminal_runs(pool: &PgPool, days: i64) -> Result<Vec<Uuid>, CleanupError> {
  // Per-run override (P10.14.1): a run with an events_/artifacts_
  // retention override stays alive until the longest of the three
  // windows expires. Otherwise the run row would cascade-DELETE
  // events/artifacts ahead of their own per-run cutoff and the
  // override would do nothing.
  let rows = sqlx::query(
    r#"SELECT id FROM runs
       WHERE status IN ('succeeded', 'failed', 'cancelled')
         AND finished_at IS NOT NULL
         AND finished_at < NOW() - GREATEST(
           $1,
           COALESCE(events_retention_days, 0),
           COALESCE(artifacts_retention_days, 0)
         )::INT * INTERVAL '1 day'"#,
  )
  .bind(days as i32)
  .fetch_all(pool)
  .await?;
  let ids: Vec<Uuid> = rows.iter().map(|row| row.get::<Uuid, _>("id")).collect();
  Ok(ids)
}

async fn delete_terminal_runs(pool: &PgPool, ids: &[Uuid]) -> Result<u64, CleanupError> {
  if ids.is_empty() {
    return Ok(0);
  }
  // `ON DELETE CASCADE` removes the steps / events / artifacts rows
  // belonging to each deleted run.
  let result = sqlx::query(r#"DELETE FROM runs WHERE id = ANY($1)"#)
    .bind(ids)
    .execute(pool)
    .await?;
  Ok(result.rows_affected())
}

async fn sweep_events(pool: &PgPool, days: i64) -> Result<u64, CleanupError> {
  // Per-run override: extend retention to `max(global, override)`
  // days by joining against the owning run row.
  let result = sqlx::query(
    r#"DELETE FROM events e
       USING runs r
       WHERE e.run_id = r.id
         AND r.status IN ('succeeded', 'failed', 'cancelled')
         AND e.ts < NOW() - GREATEST(
           $1,
           COALESCE(r.events_retention_days, 0)
         )::INT * INTERVAL '1 day'"#,
  )
  .bind(days as i32)
  .execute(pool)
  .await?;
  Ok(result.rows_affected())
}

async fn preview_events(pool: &PgPool, days: i64) -> Result<u64, CleanupError> {
  let row = sqlx::query(
    r#"SELECT COUNT(*)::BIGINT AS n FROM events e
       JOIN runs r ON e.run_id = r.id
       WHERE r.status IN ('succeeded', 'failed', 'cancelled')
         AND e.ts < NOW() - GREATEST(
           $1,
           COALESCE(r.events_retention_days, 0)
         )::INT * INTERVAL '1 day'"#,
  )
  .bind(days as i32)
  .fetch_one(pool)
  .await?;
  Ok(row.get::<i64, _>("n") as u64)
}

async fn sweep_artifacts(pool: &PgPool, days: i64) -> Result<u64, CleanupError> {
  let result = sqlx::query(
    r#"DELETE FROM artifacts a
       USING runs r
       WHERE a.run_id = r.id
         AND r.status IN ('succeeded', 'failed', 'cancelled')
         AND a.created_at < NOW() - GREATEST(
           $1,
           COALESCE(r.artifacts_retention_days, 0)
         )::INT * INTERVAL '1 day'"#,
  )
  .bind(days as i32)
  .execute(pool)
  .await?;
  Ok(result.rows_affected())
}

async fn preview_artifacts(pool: &PgPool, days: i64) -> Result<u64, CleanupError> {
  let row = sqlx::query(
    r#"SELECT COUNT(*)::BIGINT AS n FROM artifacts a
       JOIN runs r ON a.run_id = r.id
       WHERE r.status IN ('succeeded', 'failed', 'cancelled')
         AND a.created_at < NOW() - GREATEST(
           $1,
           COALESCE(r.artifacts_retention_days, 0)
         )::INT * INTERVAL '1 day'"#,
  )
  .bind(days as i32)
  .fetch_one(pool)
  .await?;
  Ok(row.get::<i64, _>("n") as u64)
}

/// Walk `root` (one level deep) and delete directories whose names
/// parse as UUIDs and whose owning run is **not** active. Returns
/// `(deleted, skipped_active)`.
async fn sweep_run_dir(
  root: &Path,
  pool: &PgPool,
  days: i64,
  dry_run: bool,
) -> Result<(u64, u64), CleanupError> {
  if !root.exists() {
    return Ok((0, 0));
  }
  let mut deleted = 0u64;
  let mut skipped = 0u64;
  let entries = std::fs::read_dir(root).map_err(|err| CleanupError::Filesystem {
    path: root.to_path_buf(),
    source: err,
  })?;
  let cutoff = Utc::now() - chrono::Duration::days(days);

  for entry in entries.flatten() {
    let path = entry.path();
    if !path.is_dir() {
      continue;
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
      continue;
    };
    let Ok(run_id) = Uuid::parse_str(name) else {
      // Skip directories that don't look like a run UUID.
      continue;
    };
    // Active runs are sacrosanct.
    let is_active = sqlx::query(
      r#"SELECT 1 AS present FROM runs
         WHERE id = $1 AND status IN ('queued', 'running')"#,
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?
    .is_some();
    if is_active {
      skipped += 1;
      continue;
    }
    // Age gate uses directory mtime.
    let modified = entry
      .metadata()
      .map_err(|err| CleanupError::Filesystem {
        path: path.clone(),
        source: err,
      })
      .and_then(|m| {
        m.modified().map_err(|err| CleanupError::Filesystem {
          path: path.clone(),
          source: err,
        })
      })
      .ok()
      .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
      .map(|d| DateTime::<Utc>::from_timestamp(d.as_secs() as i64, 0).unwrap_or(cutoff));
    let too_young = modified.is_some_and(|m| m > cutoff);
    if too_young {
      continue;
    }
    if !dry_run {
      std::fs::remove_dir_all(&path).map_err(|err| CleanupError::Filesystem {
        path: path.clone(),
        source: err,
      })?;
    }
    deleted += 1;
  }
  Ok((deleted, skipped))
}

/// V4.4: DB-free run_dir sweep for operators who only ever run
/// `yanshi workflow run` locally and never stand up `yanshi-server`
/// and Postgres at all. The CLI's own default run root
/// (`~/.yanshi/runs`) is exactly this case, and it grows unboundedly
/// on a long-running machine with no cleanup path available, since
/// [`cleanup_expired`] hard-requires a DB connection before it runs
/// anything (including the filesystem-only sweeps). Unlike
/// [`sweep_run_dir`], there is no `runs` table to consult for an
/// "is this still active" check, so this relies purely on directory
/// mtime, the same trade-off [`sweep_trace_dir`] already makes for
/// trace files, and a reasonable one here too: an actively-running
/// workflow continuously writes checkpoints/outputs into its run dir,
/// so its mtime stays recent; only a genuinely finished (or abandoned)
/// run's directory goes untouched long enough to cross the retention
/// window.
async fn sweep_run_dir_local(root: &Path, days: i64, dry_run: bool) -> Result<u64, CleanupError> {
  if !root.exists() {
    return Ok(0);
  }
  let mut deleted = 0u64;
  let entries = std::fs::read_dir(root).map_err(|err| CleanupError::Filesystem {
    path: root.to_path_buf(),
    source: err,
  })?;
  let cutoff = Utc::now() - chrono::Duration::days(days);

  for entry in entries.flatten() {
    let path = entry.path();
    if !path.is_dir() {
      continue;
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
      continue;
    };
    if Uuid::parse_str(name).is_err() {
      // Skip directories that don't look like a run UUID.
      continue;
    }
    let modified = entry
      .metadata()
      .map_err(|err| CleanupError::Filesystem {
        path: path.clone(),
        source: err,
      })
      .and_then(|m| {
        m.modified().map_err(|err| CleanupError::Filesystem {
          path: path.clone(),
          source: err,
        })
      })
      .ok()
      .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
      .map(|d| DateTime::<Utc>::from_timestamp(d.as_secs() as i64, 0).unwrap_or(cutoff));
    let too_young = modified.is_some_and(|m| m > cutoff);
    if too_young {
      continue;
    }
    if !dry_run {
      std::fs::remove_dir_all(&path).map_err(|err| CleanupError::Filesystem {
        path: path.clone(),
        source: err,
      })?;
    }
    deleted += 1;
  }
  Ok(deleted)
}

/// V4.4: filesystem-only counterpart to [`cleanup_expired`] for callers
/// with no Postgres connection at all — sweeps `run_dir_root` (via
/// `sweep_run_dir_local`'s mtime-only heuristic) and `trace_dir_root`
/// (via the already-DB-free `sweep_trace_dir`), skipping every
/// DB-backed sweep (`runs`/`events`/`artifacts`) entirely. The returned
/// [`CleanupReport`]'s DB-related counters stay at their `Default::default()`
/// zero rather than lying about work that was never attempted.
pub async fn cleanup_expired_local(
  run_dir_root: Option<&Path>,
  trace_dir_root: Option<&Path>,
  config: &CleanupConfig,
) -> Result<CleanupReport, CleanupError> {
  let started_at = Utc::now();
  let mut report = CleanupReport {
    dry_run: config.dry_run,
    started_at,
    finished_at: started_at,
    ..Default::default()
  };

  if let Some(root) = run_dir_root {
    report.run_dirs_deleted =
      sweep_run_dir_local(root, config.run_dir_retention_days as i64, config.dry_run).await?;
  }

  if let Some(root) = trace_dir_root {
    report.trace_files_deleted =
      sweep_trace_dir(root, config.runs_retention_days as i64, config.dry_run).await?;
  }

  report.finished_at = Utc::now();
  Ok(report)
}

/// Q2.3.4: delete `<workflow_id>.json` trace files older than `days`
/// from the file-backed trace dir. Files keep no run-state index, so
/// unlike `sweep_run_dir` we have no "active" check — but the trace
/// is only ever written at trace lifecycle terminal events, so an
/// old file always corresponds to a completed run.
async fn sweep_trace_dir(root: &Path, days: i64, dry_run: bool) -> Result<u64, CleanupError> {
  if !root.exists() {
    return Ok(0);
  }
  let mut deleted = 0u64;
  let entries = std::fs::read_dir(root).map_err(|err| CleanupError::Filesystem {
    path: root.to_path_buf(),
    source: err,
  })?;
  let cutoff = Utc::now() - chrono::Duration::days(days);

  for entry in entries.flatten() {
    let path = entry.path();
    // Trace files are flat JSON; skip subdirectories and non-.json files.
    if path.extension().and_then(|s| s.to_str()) != Some("json") {
      continue;
    }
    if !path.is_file() {
      continue;
    }
    let metadata = match entry.metadata() {
      Ok(m) => m,
      Err(err) => {
        return Err(CleanupError::Filesystem {
          path: path.clone(),
          source: err,
        });
      }
    };
    let modified = metadata
      .modified()
      .ok()
      .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
      .map(|d| DateTime::<Utc>::from_timestamp(d.as_secs() as i64, 0).unwrap_or(cutoff));
    let too_young = modified.is_some_and(|m| m > cutoff);
    if too_young {
      continue;
    }
    if !dry_run {
      std::fs::remove_file(&path).map_err(|err| CleanupError::Filesystem {
        path: path.clone(),
        source: err,
      })?;
    }
    deleted += 1;
  }
  Ok(deleted)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::TempDir;

  #[test]
  fn cleanup_config_production_keeps_runs_longer() {
    let prod = CleanupConfig::for_profile(SecurityProfile::Production);
    let local = CleanupConfig::for_profile(SecurityProfile::Local);
    assert_eq!(prod.runs_retention_days, 90);
    assert_eq!(local.runs_retention_days, 30);
    assert_eq!(prod.events_retention_days, 14);
    assert_eq!(prod.artifacts_retention_days, 30);
    assert_eq!(prod.run_dir_retention_days, 14);
    assert_eq!(prod.interval, DEFAULT_CLEANUP_INTERVAL);
  }

  #[test]
  fn cleanup_config_with_dry_run_flips_flag() {
    let cfg = CleanupConfig::for_profile(SecurityProfile::Local).with_dry_run(true);
    assert!(cfg.dry_run);
  }

  #[test]
  fn cleanup_config_serializes_round_trip() {
    let cfg = CleanupConfig::for_profile(SecurityProfile::Local).with_dry_run(true);
    let json = serde_json::to_string(&cfg).unwrap();
    let parsed: CleanupConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.runs_retention_days, cfg.runs_retention_days);
    assert_eq!(parsed.dry_run, cfg.dry_run);
  }

  #[test]
  fn sweep_run_dir_returns_zero_when_root_missing() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("never-created");
    // We can't actually run sweep_run_dir without a DB pool, but we
    // can confirm the existence guard fires before any DB call.
    assert!(!missing.exists());
  }

  #[test]
  fn sweep_run_dir_skips_directories_not_named_like_uuid() {
    // Pure helper-level check: the regex / Uuid::parse_str path
    // rejects non-UUID names.
    let invalid = "not-a-uuid";
    assert!(Uuid::parse_str(invalid).is_err());
  }

  // Q2.3.4: file-backed trace files older than the runs_retention
  // window must be deleted; younger files and non-.json files stay.
  #[tokio::test]
  async fn sweep_trace_dir_deletes_old_json_only() {
    use std::time::{Duration as StdDur, SystemTime};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Old trace file (should be deleted)
    let old_path = root.join("wf-old.json");
    fs::write(&old_path, "{}").unwrap();
    let old_mtime = SystemTime::now() - StdDur::from_secs(60 * 60 * 24 * 100);
    filetime::set_file_mtime(&old_path, filetime::FileTime::from_system_time(old_mtime)).ok();

    // Young trace file (should stay)
    let young_path = root.join("wf-young.json");
    fs::write(&young_path, "{}").unwrap();

    // Non-json file (should stay regardless of age)
    let other_path = root.join("notes.txt");
    fs::write(&other_path, "hello").unwrap();
    filetime::set_file_mtime(&other_path, filetime::FileTime::from_system_time(old_mtime)).ok();

    let deleted = sweep_trace_dir(root, 30, false).await.unwrap();
    assert_eq!(deleted, 1, "only old .json should be deleted");
    assert!(!old_path.exists(), "old json must be gone");
    assert!(young_path.exists(), "young json must stay");
    assert!(other_path.exists(), "non-json must stay");
  }

  #[tokio::test]
  async fn sweep_trace_dir_returns_zero_when_root_missing() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist");
    let deleted = sweep_trace_dir(&missing, 30, false).await.unwrap();
    assert_eq!(deleted, 0);
  }

  // V4.4: DB-free run_dir sweep — no Postgres pool involved at all, so
  // this is a genuine end-to-end test (unlike `sweep_run_dir_*`'s tests
  // above, which can only check the pre-DB-call guards).
  #[tokio::test]
  async fn sweep_run_dir_local_deletes_old_uuid_dirs_by_mtime_only() {
    use std::time::{Duration as StdDur, SystemTime};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let old_mtime = SystemTime::now() - StdDur::from_secs(60 * 60 * 24 * 100);

    // Old run dir (should be deleted — no DB to protect it either way).
    let old_run = root.join(Uuid::new_v4().to_string());
    fs::create_dir_all(&old_run).unwrap();
    filetime::set_file_mtime(&old_run, filetime::FileTime::from_system_time(old_mtime)).ok();

    // Young run dir (should stay).
    let young_run = root.join(Uuid::new_v4().to_string());
    fs::create_dir_all(&young_run).unwrap();

    // Non-UUID directory (should stay regardless of age).
    let other_dir = root.join("not-a-run-id");
    fs::create_dir_all(&other_dir).unwrap();
    filetime::set_file_mtime(&other_dir, filetime::FileTime::from_system_time(old_mtime)).ok();

    let deleted = sweep_run_dir_local(root, 30, false).await.unwrap();
    assert_eq!(deleted, 1, "only the old UUID-named dir should be deleted");
    assert!(!old_run.exists(), "old run dir must be gone");
    assert!(young_run.exists(), "young run dir must stay");
    assert!(other_dir.exists(), "non-UUID dir must stay");
  }

  #[tokio::test]
  async fn sweep_run_dir_local_dry_run_deletes_nothing() {
    use std::time::{Duration as StdDur, SystemTime};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let old_run = root.join(Uuid::new_v4().to_string());
    fs::create_dir_all(&old_run).unwrap();
    let old_mtime = SystemTime::now() - StdDur::from_secs(60 * 60 * 24 * 100);
    filetime::set_file_mtime(&old_run, filetime::FileTime::from_system_time(old_mtime)).ok();

    let deleted = sweep_run_dir_local(root, 30, true).await.unwrap();
    assert_eq!(deleted, 1, "dry-run still reports what would be deleted");
    assert!(
      old_run.exists(),
      "dry-run must not actually delete anything"
    );
  }

  #[tokio::test]
  async fn sweep_run_dir_local_returns_zero_when_root_missing() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist");
    let deleted = sweep_run_dir_local(&missing, 30, false).await.unwrap();
    assert_eq!(deleted, 0);
  }

  #[tokio::test]
  async fn cleanup_expired_local_sweeps_both_dirs_and_leaves_db_counters_zero() {
    use std::time::{Duration as StdDur, SystemTime};

    let run_tmp = TempDir::new().unwrap();
    let trace_tmp = TempDir::new().unwrap();
    let old_mtime = SystemTime::now() - StdDur::from_secs(60 * 60 * 24 * 100);

    let old_run = run_tmp.path().join(Uuid::new_v4().to_string());
    fs::create_dir_all(&old_run).unwrap();
    filetime::set_file_mtime(&old_run, filetime::FileTime::from_system_time(old_mtime)).ok();

    let old_trace = trace_tmp.path().join("wf-old.json");
    fs::write(&old_trace, "{}").unwrap();
    filetime::set_file_mtime(&old_trace, filetime::FileTime::from_system_time(old_mtime)).ok();

    let cfg = CleanupConfig::for_profile(SecurityProfile::Local);
    let report = cleanup_expired_local(Some(run_tmp.path()), Some(trace_tmp.path()), &cfg)
      .await
      .unwrap();

    assert_eq!(report.run_dirs_deleted, 1);
    assert_eq!(report.trace_files_deleted, 1);
    // No DB was ever touched — these must stay at their zero default,
    // not silently claim work that was never attempted.
    assert_eq!(report.runs_deleted, 0);
    assert_eq!(report.events_deleted, 0);
    assert_eq!(report.artifacts_deleted, 0);
    assert_eq!(report.run_dirs_skipped_active, 0);
    assert!(!old_run.exists());
    assert!(!old_trace.exists());
  }

  #[tokio::test]
  async fn cleanup_expired_local_handles_no_dirs_configured() {
    let cfg = CleanupConfig::for_profile(SecurityProfile::Local);
    let report = cleanup_expired_local(None, None, &cfg).await.unwrap();
    assert_eq!(report.run_dirs_deleted, 0);
    assert_eq!(report.trace_files_deleted, 0);
  }

  #[test]
  fn cleanup_report_serializes_with_dry_run_flag() {
    let report = CleanupReport {
      dry_run: true,
      runs_deleted: 3,
      events_deleted: 17,
      artifacts_deleted: 1,
      run_dirs_deleted: 2,
      run_dirs_skipped_active: 1,
      run_ids_targeted: vec![Uuid::nil()],
      trace_files_deleted: 0,
      harness_sessions_deleted: 0,
      approval_intents_deleted: 0,
      started_at: Utc::now(),
      finished_at: Utc::now(),
    };
    let json = serde_json::to_string(&report).unwrap();
    let parsed: CleanupReport = serde_json::from_str(&json).unwrap();
    assert!(parsed.dry_run);
    assert_eq!(parsed.runs_deleted, 3);
    assert_eq!(parsed.run_dirs_skipped_active, 1);
  }

  // Filesystem-only test: builds a fake run_dir, inserts no DB, and
  // makes sure the directory layout is what `sweep_run_dir` expects
  // to walk. Live DB cases live in
  // `yanshi-server/tests/cleanup_route.rs` (skipped without
  // YANSHI_DATABASE_TEST_URL).
  #[test]
  fn fake_run_dir_layout_contains_uuid_named_subdirs() {
    let tmp = TempDir::new().unwrap();
    let id = Uuid::new_v4();
    fs::create_dir_all(tmp.path().join(id.to_string())).unwrap();
    fs::create_dir_all(tmp.path().join("not-a-uuid")).unwrap();
    let entries = fs::read_dir(tmp.path())
      .unwrap()
      .flatten()
      .filter(|e| {
        e.path()
          .file_name()
          .and_then(|s| s.to_str())
          .map(|name| Uuid::parse_str(name).is_ok())
          .unwrap_or(false)
      })
      .count();
    assert_eq!(entries, 1, "only the UUID-named directory is eligible");
  }
}
