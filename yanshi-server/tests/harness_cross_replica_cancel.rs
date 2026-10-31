//! Eval 2026-10-07 P1: a harness cancel served by one replica stops the
//! session executing on another, not just its row.
//!
//! Requires Postgres via `YANSHI_DATABASE_TEST_URL` (self-skips without).

use axum::{body::Body, http::Request};
use tokio::time::{Duration, timeout};
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_agents::AgentCancellationToken;
use yanshi_db::{Database, HarnessSessionRepo, HarnessSessionStatus, NewHarnessSession};
use yanshi_server::{
  AppState, HARNESS_CANCELLATION_NOTIFY_CHANNEL, create_router, spawn_harness_cancellation_listener,
};

async fn db() -> Option<Database> {
  let url = std::env::var("YANSHI_DATABASE_TEST_URL").ok()?;
  Database::connect_and_migrate(&url, 4).await.ok()
}

async fn new_session(state: &AppState, tenant: &str) -> Uuid {
  let id = Uuid::new_v4();
  state
    .repos
    .harness_sessions
    .create(NewHarnessSession {
      id,
      tenant_id: tenant.to_string(),
      user_input: "cancel test".into(),
      workspace_root: "/tmp".into(),
      profile: "local".into(),
      runtime_kind: "react".into(),
      model: "stub".into(),
      skill_name: None,
    })
    .await
    .unwrap();
  id
}

async fn cancelled_within(token: &AgentCancellationToken, wait: Duration) -> bool {
  timeout(wait, async {
    while !token.is_cancelled() {
      tokio::time::sleep(Duration::from_millis(25)).await;
    }
  })
  .await
  .is_ok()
}

#[tokio::test]
async fn cancel_on_another_replica_stops_the_running_session() {
  let Some(db) = db().await else {
    eprintln!("skipping cancel_on_another_replica_stops_the_running_session: no DB url");
    return;
  };
  let owner = AppState::new(db.clone());
  let other = AppState::new(db.clone());
  let tenant = format!("xr-cancel-{}", Uuid::new_v4());
  let id = new_session(&owner, &tenant).await;
  let guard = owner.harness_cancellations.register(id);
  spawn_harness_cancellation_listener(owner.repos.clone(), owner.harness_cancellations.clone());
  // Let the listener LISTEN before the NOTIFY fires.
  tokio::time::sleep(Duration::from_millis(300)).await;

  let response = create_router(other.clone())
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/v1/harness/sessions/{id}:cancel"))
        .header("X-Yanshi-Tenant", &tenant)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert!(response.status().is_success(), "{}", response.status());

  assert!(
    cancelled_within(&guard.token(), Duration::from_secs(5)).await,
    "the owning replica must stop the session"
  );
}

/// A cancel marked while the owner wasn't listening is picked up when
/// its listener connects.
#[tokio::test]
async fn cancel_marked_before_the_listener_connects_is_resynced() {
  let Some(db) = db().await else {
    eprintln!("skipping cancel_marked_before_the_listener_connects_is_resynced: no DB url");
    return;
  };
  let owner = AppState::new(db.clone());
  let tenant = format!("xr-cancel-{}", Uuid::new_v4());
  let id = new_session(&owner, &tenant).await;
  let guard = owner.harness_cancellations.register(id);
  owner
    .repos
    .harness_sessions
    .update_status(
      id,
      HarnessSessionStatus::Cancelled,
      None,
      Some("cancel requested"),
    )
    .await
    .unwrap();

  spawn_harness_cancellation_listener(owner.repos.clone(), owner.harness_cancellations.clone());
  assert!(cancelled_within(&guard.token(), Duration::from_secs(5)).await);
}

/// A notification for a session whose row is no longer `cancelled`
/// (resumed in between) must not stop it.
#[tokio::test]
async fn stale_notification_does_not_stop_a_running_session() {
  let Some(db) = db().await else {
    eprintln!("skipping stale_notification_does_not_stop_a_running_session: no DB url");
    return;
  };
  let owner = AppState::new(db.clone());
  let tenant = format!("xr-cancel-{}", Uuid::new_v4());
  let id = new_session(&owner, &tenant).await;
  let guard = owner.harness_cancellations.register(id);
  spawn_harness_cancellation_listener(owner.repos.clone(), owner.harness_cancellations.clone());
  tokio::time::sleep(Duration::from_millis(300)).await;

  yanshi_db::notify(
    &db.pool,
    HARNESS_CANCELLATION_NOTIFY_CHANNEL,
    &id.to_string(),
  )
  .await
  .unwrap();
  assert!(!cancelled_within(&guard.token(), Duration::from_millis(800)).await);
}
