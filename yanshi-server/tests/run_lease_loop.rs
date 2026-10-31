//! Eval 2026-10-07 P5: the lease loop renews runs this replica holds and
//! fails runs whose lease expired (their replica is gone).
//!
//! Requires Postgres via `YANSHI_DATABASE_TEST_URL` (self-skips without).

use tokio::time::Duration;
use uuid::Uuid;
use yanshi_core::FlowCancellationToken;
use yanshi_db::{Database, EventRepo, NewEvent, NewRun, RunRepo, RunStatus};
use yanshi_server::{AppState, spawn_run_lease_loop};

fn new_run(tenant: &str) -> NewRun {
  NewRun {
    id: Uuid::new_v4(),
    workflow: "@test".to_string(),
    status: RunStatus::Running,
    run_dir: None,
    tenant_id: tenant.to_string(),
    events_retention_days: None,
    artifacts_retention_days: None,
  }
}

#[tokio::test]
async fn loop_keeps_local_runs_and_reclaims_orphans() {
  let Some(url) = std::env::var("YANSHI_DATABASE_TEST_URL").ok() else {
    eprintln!("skipping loop_keeps_local_runs_and_reclaims_orphans: no DB url");
    return;
  };
  let db = Database::connect_and_migrate(&url, 4).await.unwrap();
  let state = AppState::new(db.clone());
  let tenant = format!("lease-loop-{}", Uuid::new_v4());
  let local = state.repos.runs.create(new_run(&tenant)).await.unwrap();
  let orphan = state.repos.runs.create(new_run(&tenant)).await.unwrap();
  let task = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(30)).await });
  state
    .cancellation_registry
    .register(local.id, FlowCancellationToken::new(), task.abort_handle());
  // Both leases look expired; only `local` is still owned by a replica.
  sqlx::query("UPDATE runs SET lease_expires_at = NOW() - INTERVAL '1 second' WHERE id = ANY($1)")
    .bind(vec![local.id, orphan.id])
    .execute(&db.pool)
    .await
    .unwrap();

  state
    .repos
    .events
    .append(NewEvent {
      run_id: orphan.id,
      seq: 0,
      kind: "workflow.started".to_string(),
      payload: serde_json::json!({}),
      tenant_id: Some(tenant.clone()),
    })
    .await
    .unwrap();

  spawn_run_lease_loop(
    state.repos.clone(),
    state.event_broker.clone(),
    state.cancellation_registry.clone(),
  );

  let reclaimed = tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      let row = state.repos.runs.get(orphan.id).await.unwrap().unwrap();
      if row.status == RunStatus::Failed.as_str() {
        return;
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  })
  .await;
  assert!(reclaimed.is_ok(), "the orphaned run must be reclaimed");
  // Its replica is gone, so the reclaim has to write the terminal event
  // SSE subscribers are waiting for.
  let terminal = tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      let events = state
        .repos
        .events
        .list_after(&tenant, orphan.id, -1, 10)
        .await
        .unwrap();
      if let Some(event) = events.iter().find(|e| e.kind == "workflow.failed") {
        return event.clone();
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  })
  .await
  .expect("a reclaimed run must get a terminal event");
  assert_eq!(terminal.seq, 1, "continues the run's seq series");
  assert!(
    terminal.payload["error"]
      .as_str()
      .is_some_and(|error| error.contains("lease expired")),
    "{:?}",
    terminal.payload
  );
  let local_row = state.repos.runs.get(local.id).await.unwrap().unwrap();
  assert_eq!(
    local_row.status,
    RunStatus::Running.as_str(),
    "a held run must be renewed"
  );
}
