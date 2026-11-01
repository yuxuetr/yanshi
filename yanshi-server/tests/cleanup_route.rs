//! Integration tests for `cleanup_expired`. Requires a live Postgres
//! pointed to by `YANSHI_DATABASE_TEST_URL`; without it the tests
//! exit early so workspace `cargo test` stays hermetic.
//!
//! `cleanup_expired` mutates global table state (deletes every
//! terminal+expired row regardless of tenant), so the tests in this
//! file must run serially relative to each other — otherwise a
//! `delete` test's destroy phase races a `dry_run` test's read phase.
//! Other test binaries in this crate are UUID-isolated (see their
//! `fresh_state` comments) and don't touch `cleanup_expired`, so they
//! can keep running in parallel with this binary; the [`CLEANUP_MUTEX`]
//! below only serialises the cleanup tests against each other.

use std::fs;

use chrono::{Duration, Utc};
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;
use yanshi_db::{Database, NewRun, Repositories, RunRepo, RunStatus};
use yanshi_server::{CleanupConfig, cleanup_expired};
use yanshi_tools::SecurityProfile;

/// Module-local mutex held for the lifetime of each cleanup test. The
/// guard crosses `.await` points (the test does seeding + cleanup +
/// re-reads all under the lock), so this uses `tokio::sync::Mutex`
/// rather than `std::sync::Mutex` to keep the futures `Send` under
/// the multi-thread tokio test runtime.
static CLEANUP_MUTEX: Mutex<()> = Mutex::const_new(());

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

fn repos(db: &Database) -> Repositories {
  Repositories::from_pool(db.pool.clone())
}

async fn fresh_db() -> Option<Database> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4).await.ok()?;
  // Intentionally no TRUNCATE: integration tests run in parallel
  // across cargo test binaries, and a global TRUNCATE wipes another
  // test's seeded rows mid-run. The cleanup tests below only assert
  // on the specific run ids they seed, so they tolerate co-resident
  // rows from other tests. Mirrors the pattern documented in
  // `yanshi-db/tests/repositories.rs::fresh_db`.
  Some(db)
}

async fn insert_run(db: &Database, status: RunStatus, finished_offset_days: Option<i64>) -> Uuid {
  let id = Uuid::new_v4();
  repos(db)
    .runs
    .create(NewRun {
      id,
      tenant_id: "default".into(),
      workflow: "name: stub".into(),
      status: RunStatus::Running,
      run_dir: None,
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .unwrap();
  if !matches!(status, RunStatus::Running | RunStatus::Queued) {
    // Mark terminal and back-date finished_at.
    repos(db)
      .runs
      .update_status(id, status, None)
      .await
      .unwrap();
    if let Some(days) = finished_offset_days {
      let target = Utc::now() - Duration::days(days);
      sqlx::query("UPDATE runs SET finished_at = $1 WHERE id = $2")
        .bind(target)
        .bind(id)
        .execute(&db.pool)
        .await
        .unwrap();
    }
  }
  id
}

#[tokio::test]
async fn cleanup_dry_run_targets_old_terminal_runs_without_deleting() {
  let _guard = CLEANUP_MUTEX.lock().await;
  let Some(db) = fresh_db().await else {
    eprintln!("skipping cleanup_dry_run_targets_old_terminal_runs_without_deleting");
    return;
  };

  let old_id = insert_run(&db, RunStatus::Succeeded, Some(60)).await;
  let young_id = insert_run(&db, RunStatus::Succeeded, Some(1)).await;
  let active_id = insert_run(&db, RunStatus::Running, None).await;

  let cfg = CleanupConfig::for_profile(SecurityProfile::Local).with_dry_run(true);
  let report = cleanup_expired(&db, None, None, &cfg).await.unwrap();

  assert!(report.dry_run);
  assert!(report.run_ids_targeted.contains(&old_id));
  assert!(!report.run_ids_targeted.contains(&young_id));
  assert!(!report.run_ids_targeted.contains(&active_id));

  // No actual rows deleted — dry-run is read-only.
  let still_there = repos(&db).runs.get(old_id).await.unwrap();
  assert!(
    still_there.is_some(),
    "dry-run must not delete rows: {old_id}"
  );
}

async fn insert_harness_session(
  db: &Database,
  status: &str,
  finished_days_ago: Option<i64>,
) -> Uuid {
  let id = Uuid::new_v4();
  sqlx::query(
    "INSERT INTO harness_sessions
       (id, status, user_input, workspace_root, profile, runtime_kind, model, finished_at)
     VALUES ($1, $2, 'hi', '/tmp', 'local', 'react', 'mock', $3)",
  )
  .bind(id)
  .bind(status)
  .bind(finished_days_ago.map(|days| Utc::now() - Duration::days(days)))
  .execute(&db.pool)
  .await
  .unwrap();
  sqlx::query(
    "INSERT INTO harness_session_events (session_id, seq, kind, payload)
     VALUES ($1, 0, 'session_started', '{}')",
  )
  .bind(id)
  .execute(&db.pool)
  .await
  .unwrap();
  id
}

async fn count(db: &Database, sql: &str, id: &str) -> i64 {
  sqlx::query_scalar(sql)
    .bind(id)
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn cleanup_sweeps_expired_harness_sessions_and_approval_intents() {
  let _guard = CLEANUP_MUTEX.lock().await;
  let Some(db) = fresh_db().await else {
    eprintln!("skipping cleanup_sweeps_expired_harness_sessions_and_approval_intents");
    return;
  };

  let old = insert_harness_session(&db, "completed", Some(60)).await;
  let young = insert_harness_session(&db, "failed", Some(1)).await;
  let running = insert_harness_session(&db, "running", None).await;
  let old_intent = Uuid::new_v4().to_string();
  let young_intent = Uuid::new_v4().to_string();
  for (session, age_days) in [(&old_intent, 60), (&young_intent, 1)] {
    sqlx::query(
      "INSERT INTO approval_decision_intents (session_id, request_id, tenant_id, decision, decided_at)
       VALUES ($1, 'r', 'default', '{}', $2)",
    )
    .bind(session)
    .bind(Utc::now() - Duration::days(age_days))
    .execute(&db.pool)
    .await
    .unwrap();
  }
  let sessions = "SELECT COUNT(*) FROM harness_sessions WHERE id::TEXT = $1";
  let events = "SELECT COUNT(*) FROM harness_session_events WHERE session_id::TEXT = $1";
  let intents = "SELECT COUNT(*) FROM approval_decision_intents WHERE session_id = $1";

  let cfg = CleanupConfig::for_profile(SecurityProfile::Local).with_dry_run(true);
  let report = cleanup_expired(&db, None, None, &cfg).await.unwrap();
  assert!(report.harness_sessions_deleted >= 1 && report.approval_intents_deleted >= 1);
  assert_eq!(
    count(&db, sessions, &old.to_string()).await,
    1,
    "dry run keeps rows"
  );

  let cfg = CleanupConfig::for_profile(SecurityProfile::Local);
  let report = cleanup_expired(&db, None, None, &cfg).await.unwrap();
  assert!(report.harness_sessions_deleted >= 1 && report.approval_intents_deleted >= 1);
  assert_eq!(count(&db, sessions, &old.to_string()).await, 0);
  assert_eq!(
    count(&db, events, &old.to_string()).await,
    0,
    "events cascade"
  );
  for kept in [young, running] {
    assert_eq!(count(&db, sessions, &kept.to_string()).await, 1, "{kept}");
  }
  assert_eq!(count(&db, intents, &old_intent).await, 0);
  assert_eq!(count(&db, intents, &young_intent).await, 1);
}

#[tokio::test]
async fn cleanup_deletes_terminal_runs_past_retention() {
  let _guard = CLEANUP_MUTEX.lock().await;
  let Some(db) = fresh_db().await else {
    eprintln!("skipping cleanup_deletes_terminal_runs_past_retention");
    return;
  };

  let old_id = insert_run(&db, RunStatus::Succeeded, Some(60)).await;
  let young_id = insert_run(&db, RunStatus::Succeeded, Some(1)).await;
  let active_id = insert_run(&db, RunStatus::Running, None).await;

  let cfg = CleanupConfig::for_profile(SecurityProfile::Local);
  let report = cleanup_expired(&db, None, None, &cfg).await.unwrap();
  assert!(!report.dry_run);
  assert!(report.runs_deleted >= 1);

  assert!(repos(&db).runs.get(old_id).await.unwrap().is_none());
  assert!(repos(&db).runs.get(young_id).await.unwrap().is_some());
  assert!(repos(&db).runs.get(active_id).await.unwrap().is_some());
}

/// P10.14.1: a terminal run with `events_retention_days > global`
/// must survive the sweep until the longer window expires; the
/// cleanup SQL pins the run-row deletion on
/// `GREATEST(global, events_override, artifacts_override)` to keep
/// the cascade from yanking events out from under the override.
#[tokio::test]
async fn cleanup_skips_terminal_run_pinned_by_events_override() {
  let _guard = CLEANUP_MUTEX.lock().await;
  let Some(db) = fresh_db().await else {
    eprintln!("skipping cleanup_skips_terminal_run_pinned_by_events_override");
    return;
  };

  // A run finished 60 days ago. The Local profile defaults
  // `runs_retention_days = 30`, so without an override this would
  // be deleted. With `events_retention_days = 365` it must stay.
  let id = Uuid::new_v4();
  repos(&db)
    .runs
    .create(NewRun {
      id,
      tenant_id: "default".into(),
      workflow: "name: stub".into(),
      status: RunStatus::Running,
      run_dir: None,
      events_retention_days: Some(365),
      artifacts_retention_days: None,
    })
    .await
    .unwrap();
  repos(&db)
    .runs
    .update_status(id, RunStatus::Succeeded, None)
    .await
    .unwrap();
  let target = Utc::now() - Duration::days(60);
  sqlx::query("UPDATE runs SET finished_at = $1 WHERE id = $2")
    .bind(target)
    .bind(id)
    .execute(&db.pool)
    .await
    .unwrap();

  // Also add a baseline "no override, old" run that should still
  // get swept to prove the global path still works.
  let unpinned = insert_run(&db, RunStatus::Succeeded, Some(60)).await;

  let cfg = CleanupConfig::for_profile(SecurityProfile::Local);
  let report = cleanup_expired(&db, None, None, &cfg).await.unwrap();

  // The pinned run must NOT be in run_ids_targeted, and must
  // survive the sweep.
  assert!(
    !report.run_ids_targeted.contains(&id),
    "pinned run should be excluded from targets: {id}"
  );
  assert!(
    repos(&db).runs.get(id).await.unwrap().is_some(),
    "pinned run must survive cleanup"
  );
  // The unpinned old run is gone.
  assert!(
    repos(&db).runs.get(unpinned).await.unwrap().is_none(),
    "unpinned old run should be deleted"
  );
}

/// P10.14.1: the events-sweep also pins on the per-run override
/// independently of the run-row deletion. An ancient terminal run
/// with `events_retention_days = 365` keeps its event rows even
/// though the global default would normally cull them.
#[tokio::test]
async fn cleanup_skips_events_pinned_by_override() {
  use yanshi_db::{EventRepo, NewEvent};

  let _guard = CLEANUP_MUTEX.lock().await;
  let Some(db) = fresh_db().await else {
    eprintln!("skipping cleanup_skips_events_pinned_by_override");
    return;
  };

  // Pinned run: finished 60 days ago, override 365 days.
  let pinned = Uuid::new_v4();
  repos(&db)
    .runs
    .create(NewRun {
      id: pinned,
      tenant_id: "default".into(),
      workflow: "name: stub".into(),
      status: RunStatus::Running,
      run_dir: None,
      events_retention_days: Some(365),
      artifacts_retention_days: None,
    })
    .await
    .unwrap();
  repos(&db)
    .runs
    .update_status(pinned, RunStatus::Succeeded, None)
    .await
    .unwrap();
  sqlx::query("UPDATE runs SET finished_at = $1 WHERE id = $2")
    .bind(Utc::now() - Duration::days(60))
    .bind(pinned)
    .execute(&db.pool)
    .await
    .unwrap();
  // Back-date an event row so it would otherwise be eligible for
  // sweep (events_retention_days = 14 in Local profile).
  repos(&db)
    .events
    .append(NewEvent {
      run_id: pinned,
      seq: 0,
      kind: "node.completed".into(),
      payload: serde_json::json!({"node_id": "stub"}),
      tenant_id: Some("default".into()),
    })
    .await
    .unwrap();
  sqlx::query("UPDATE events SET ts = $1 WHERE run_id = $2 AND seq = 0")
    .bind(Utc::now() - Duration::days(60))
    .bind(pinned)
    .execute(&db.pool)
    .await
    .unwrap();

  let cfg = CleanupConfig::for_profile(SecurityProfile::Local);
  cleanup_expired(&db, None, None, &cfg).await.unwrap();

  // The event row must still be there.
  let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM events WHERE run_id = $1")
    .bind(pinned)
    .fetch_one(&db.pool)
    .await
    .unwrap();
  assert_eq!(
    remaining, 1,
    "event row should be retained under the 365-day override"
  );
}

#[tokio::test]
async fn cleanup_filesystem_sweep_removes_orphaned_dirs() {
  let _guard = CLEANUP_MUTEX.lock().await;
  let Some(db) = fresh_db().await else {
    eprintln!("skipping cleanup_filesystem_sweep_removes_orphaned_dirs");
    return;
  };

  let tmp = TempDir::new().unwrap();
  let orphan_id = Uuid::new_v4();
  let orphan_dir = tmp.path().join(orphan_id.to_string());
  fs::create_dir_all(&orphan_dir).unwrap();
  fs::write(orphan_dir.join("artifact.txt"), "hi").unwrap();
  // Back-date the directory mtime via the `touch -t` shell utility so
  // the retention age gate fires. On platforms without `touch` the
  // test self-skips; that keeps CI portable.
  let old_path = orphan_dir.display().to_string();
  let touched = std::process::Command::new("touch")
    .args(["-t", "200001010000", &old_path])
    .status();
  if !touched.map(|s| s.success()).unwrap_or(false) {
    eprintln!("skipping (couldn't backdate dir mtime via `touch`)");
    return;
  }

  let active_id = insert_run(&db, RunStatus::Running, None).await;
  let active_dir = tmp.path().join(active_id.to_string());
  fs::create_dir_all(&active_dir).unwrap();

  let cfg = CleanupConfig::for_profile(SecurityProfile::Local);
  let report = cleanup_expired(&db, Some(tmp.path()), None, &cfg)
    .await
    .unwrap();
  assert!(report.run_dirs_skipped_active >= 1);
  assert!(!orphan_dir.exists(), "orphan dir must be deleted");
  assert!(active_dir.exists(), "active run's dir must be retained");
}
