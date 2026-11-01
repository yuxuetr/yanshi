//! W4.2d — cross-replica cancellation via Postgres NOTIFY.
//!
//! Before this, `RunCancellationRegistry::cancel(run_id)` only ever did
//! anything when the HTTP request happened to land on the same replica
//! that registered the run's `FlowCancellationToken`/`AbortHandle` —
//! `cancel_run` never checked the (already-existed) `bool` return value,
//! so a cross-replica cancel silently no-op'd while still reporting
//! `cancelled: true` and flipping the DB row. This test builds two
//! independent `AppState`s against the same Postgres database (see
//! `cross_replica_events.rs` for the same fixture shape), registers a
//! cancellation entry on replica A (standing in for "the process
//! actually running this task"), and cancels it via replica B's HTTP
//! route — proving the intent reaches the owning replica's live
//! `FlowCancellationToken`/`AbortHandle`, not just its DB row.
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`. Without
//! it the tests self-skip, matching every other server e2e test file.

use axum::{
  body::Body,
  http::{Request, StatusCode},
};
use tokio::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_core::FlowCancellationToken;
use yanshi_db::{Database, NewRun, RunRepo, RunStatus};
use yanshi_server::{AppState, create_router, spawn_run_cancellation_listener};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn two_replica_states() -> Option<(AppState, AppState)> {
  let url = live_url()?;
  let db_a = Database::connect_and_migrate(&url, 4).await.ok()?;
  // No TRUNCATE: tests run in parallel and each uses fresh ids, so a
  // global reset only races sibling tests.
  let db_b = Database::connect(&url, 4).await.ok()?;

  let state_a = AppState::new(db_a.clone());
  let state_b = AppState::new(db_b.clone());

  spawn_run_cancellation_listener(db_a.pool.clone(), state_a.cancellation_registry.clone());
  spawn_run_cancellation_listener(db_b.pool.clone(), state_b.cancellation_registry.clone());

  Some((state_a, state_b))
}

#[tokio::test]
async fn cancel_via_replica_b_fires_the_token_and_abort_registered_on_replica_a() {
  let Some((state_a, state_b)) = two_replica_states().await else {
    eprintln!("skipping cancel_via_replica_b_fires_the_token_and_abort_registered_on_replica_a");
    return;
  };

  let run_id = Uuid::new_v4();
  let tenant_id = "default".to_string();
  state_a
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      workflow: "@test".to_string(),
      status: RunStatus::Running,
      run_dir: None,
      tenant_id,
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .expect("run row created via replica A");

  // Register a cancellation entry on replica A, standing in for "an
  // executor task pinned to this process" — same shape as this
  // codebase's own single-replica regression test
  // (runs_routes.rs::cancel_running_run_marks_cancelled_and_emits_event).
  let token = FlowCancellationToken::new();
  let handle = tokio::spawn(async {
    tokio::time::sleep(Duration::from_secs(30)).await;
  });
  state_a
    .cancellation_registry
    .register(run_id, token.clone(), handle.abort_handle());

  // Cancel through replica B's own HTTP route — replica B has no local
  // registry entry for this run at all.
  let app_b = create_router(state_b.clone());
  let response = app_b
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/runs/{run_id}:cancel"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // The HTTP response returning 200 proves nothing about replica A's
  // live task by itself (that's exactly the bug this closes) — the
  // real assertion is that replica A's token/abort actually fired.
  let cancelled = tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      if token.is_cancelled() {
        return true;
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  })
  .await
  .unwrap_or(false);
  assert!(
    cancelled,
    "replica A's FlowCancellationToken must fire from a cancel request submitted to replica B"
  );
  assert!(
    tokio::time::timeout(Duration::from_secs(1), handle)
      .await
      .is_ok(),
    "replica A's registered task must have been aborted, not just the token flipped"
  );
}

/// Regression: a cancel request for a run this replica DOES own must
/// keep working exactly as before — the NOTIFY path is additive, not a
/// replacement for the existing local fast path.
#[tokio::test]
async fn cancel_on_the_owning_replica_still_works_locally() {
  let Some((state_a, _state_b)) = two_replica_states().await else {
    eprintln!("skipping cancel_on_the_owning_replica_still_works_locally");
    return;
  };

  let run_id = Uuid::new_v4();
  state_a
    .repos
    .runs
    .create(NewRun {
      id: run_id,
      workflow: "@test".to_string(),
      status: RunStatus::Running,
      run_dir: None,
      tenant_id: "default".to_string(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .expect("run row created");

  let token = FlowCancellationToken::new();
  let handle = tokio::spawn(async {
    tokio::time::sleep(Duration::from_secs(30)).await;
  });
  state_a
    .cancellation_registry
    .register(run_id, token.clone(), handle.abort_handle());

  let app_a = create_router(state_a.clone());
  let response = app_a
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/runs/{run_id}:cancel"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  assert!(token.is_cancelled());
}
