//! Eval 2026-10-07 P3: NOTIFY is at-most-once. State written while a
//! replica's listener wasn't connected (no NOTIFY reaches it) must still
//! take effect, because every listener resyncs from the DB after each
//! (re)connect: cancellation intents and persisted run events.
//!
//! Requires Postgres via `YANSHI_DATABASE_TEST_URL` (self-skips without).

use tokio::time::Duration;
use uuid::Uuid;
use yanshi_core::FlowCancellationToken;
use yanshi_db::{Database, NewRun, RunRepo, RunStatus};
use yanshi_server::{AppState, spawn_run_cancellation_listener};

#[tokio::test]
async fn intent_recorded_without_notify_cancels_the_local_run() {
  let Some(url) = std::env::var("YANSHI_DATABASE_TEST_URL").ok() else {
    eprintln!("skipping intent_recorded_without_notify_cancels_the_local_run: no DB url");
    return;
  };
  let db = Database::connect_and_migrate(&url, 4).await.unwrap();
  let state = AppState::new(db.clone());

  let run_id = Uuid::new_v4();
  state
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
    .unwrap();
  let token = FlowCancellationToken::new();
  let task = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(30)).await });
  state
    .cancellation_registry
    .register(run_id, token.clone(), task.abort_handle());

  // The intent lands while nobody is listening: no NOTIFY is delivered.
  state
    .repos
    .runs
    .record_cancellation_intent(run_id, "default")
    .await
    .unwrap();

  spawn_run_cancellation_listener(db.pool.clone(), state.cancellation_registry.clone());

  let cancelled = tokio::time::timeout(Duration::from_secs(5), async {
    while !token.is_cancelled() {
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  })
  .await
  .is_ok();
  assert!(
    cancelled,
    "the listener must resync from run_cancellation_intents"
  );
}

/// Same gap, event side: an event persisted while the listener wasn't
/// connected must still reach this replica's live SSE subscriber once
/// the listener (re)connects.
#[tokio::test]
async fn event_persisted_without_notify_reaches_the_local_subscriber() {
  let Some(url) = std::env::var("YANSHI_DATABASE_TEST_URL").ok() else {
    eprintln!("skipping event_persisted_without_notify_reaches_the_local_subscriber: no DB url");
    return;
  };
  let db = Database::connect_and_migrate(&url, 4).await.unwrap();
  let state = AppState::new(db.clone());
  let run_id = Uuid::new_v4();
  state
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
    .unwrap();
  let mut subscriber = state.event_broker.subscribe(run_id);

  yanshi_db::EventRepo::append(
    &state.repos.events,
    yanshi_db::NewEvent {
      run_id,
      seq: 1,
      kind: "node_completed".to_string(),
      payload: serde_json::json!({}),
      tenant_id: Some("default".to_string()),
    },
  )
  .await
  .unwrap();

  yanshi_server::spawn_run_events_listener(
    db.pool.clone(),
    state.repos.clone(),
    state.event_broker.clone(),
  );
  let event = tokio::time::timeout(Duration::from_secs(5), subscriber.recv())
    .await
    .expect("the listener must resync persisted events after connecting")
    .unwrap();
  assert_eq!(event.seq, 1);
}
