//! Integration tests for `DbLoopCheckpointer` against a real Postgres.
//!
//! Gated by `YANSHI_DATABASE_TEST_URL` for the same reason as
//! `tests/repositories.rs` — keeps `cargo test --workspace` hermetic. To run:
//!
//! ```bash
//! YANSHI_DATABASE_TEST_URL=postgres://postgres:postgres@localhost:5432/yanshi_test \
//!   cargo test -p yanshi-db --test loop_checkpoint
//! ```

use std::collections::VecDeque;

use chrono::Utc;
use uuid::Uuid;
use yanshi_agent_spi::checkpoint::{
  AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION, AgentLoopCheckpoint, AgentLoopCheckpointer, LoopRuntimeKind,
};
use yanshi_db::{
  Database, DbLoopCheckpointer, HarnessSessionRepo, NewHarnessSession, Repositories,
};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn fresh_db() -> Option<Database> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4)
    .await
    .expect("connect + migrate");
  Some(db)
}

fn sample_checkpoint(session_id: Uuid, pending_question: Option<String>) -> AgentLoopCheckpoint {
  let mut recent = VecDeque::new();
  recent.push_back(("search".to_string(), serde_json::json!({"q": "rust"})));
  AgentLoopCheckpoint {
    schema_version: AGENT_LOOP_CHECKPOINT_SCHEMA_VERSION,
    session_id: session_id.to_string(),
    runtime_kind: LoopRuntimeKind::React,
    created_at: Utc::now(),
    steps: vec![],
    events: vec![],
    step_index: 3,
    iteration: 2,
    tool_calls: 1,
    verification_attempts: 0,
    schema_correction_attempts: 0,
    last_tool_call: Some(("search".to_string(), serde_json::json!({"q": "rust"}))),
    recent_tool_calls: recent,
    cumulative_cost_usd: 0.0123,
    cumulative_tokens: 0,
    system_prompt: "be helpful".into(),
    user_input: "find the bug".into(),
    trace_context: None,
    plan_steps: serde_json::Value::Null,
    plan_position: 0,
    observations: vec![],
    pending_question,
  }
}

#[tokio::test]
async fn save_load_clear_round_trips_including_pending_question() {
  let Some(db) = fresh_db().await else {
    eprintln!(
      "skipping save_load_clear_round_trips_including_pending_question — set YANSHI_DATABASE_TEST_URL"
    );
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let session_id = Uuid::new_v4();
  repos
    .harness_sessions
    .create(NewHarnessSession {
      id: session_id,
      tenant_id: format!("tenant-loop-checkpoint-{}", Uuid::new_v4()),
      user_input: "find the bug".into(),
      workspace_root: "/tmp/ws".into(),
      profile: "local".into(),
      runtime_kind: "react".into(),
      model: "mock".into(),
      skill_name: None,
    })
    .await
    .expect("create session");

  let checkpoint = sample_checkpoint(session_id, Some("which file?".into()));

  let checkpointer = DbLoopCheckpointer::new(db.pool.clone());
  checkpointer.save(&checkpoint).await.expect("save");

  // Simulate a server restart: a fresh checkpointer instance against the
  // same pool, not the one that just saved.
  let restarted = DbLoopCheckpointer::new(db.pool.clone());
  let loaded = restarted
    .load(&session_id.to_string())
    .await
    .expect("load")
    .expect("present");
  assert_eq!(loaded, checkpoint);
  assert_eq!(loaded.pending_question.as_deref(), Some("which file?"));

  // A second save overwrites in place (upsert), not append.
  let updated = sample_checkpoint(session_id, None);
  restarted.save(&updated).await.expect("save again");
  let loaded_again = restarted
    .load(&session_id.to_string())
    .await
    .expect("load")
    .expect("present");
  assert_eq!(loaded_again.pending_question, None);

  restarted
    .clear(&session_id.to_string())
    .await
    .expect("clear");
  let after_clear = restarted.load(&session_id.to_string()).await.expect("load");
  assert!(after_clear.is_none());
}

#[tokio::test]
async fn load_returns_none_for_a_session_with_no_checkpoint() {
  let Some(db) = fresh_db().await else {
    eprintln!(
      "skipping load_returns_none_for_a_session_with_no_checkpoint — set YANSHI_DATABASE_TEST_URL"
    );
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let session_id = Uuid::new_v4();
  repos
    .harness_sessions
    .create(NewHarnessSession {
      id: session_id,
      tenant_id: format!("tenant-loop-checkpoint-empty-{}", Uuid::new_v4()),
      user_input: "say hi".into(),
      workspace_root: "/tmp/ws".into(),
      profile: "local".into(),
      runtime_kind: "react".into(),
      model: "mock".into(),
      skill_name: None,
    })
    .await
    .expect("create session");

  let checkpointer = DbLoopCheckpointer::new(db.pool.clone());
  let loaded = checkpointer
    .load(&session_id.to_string())
    .await
    .expect("load");
  assert!(loaded.is_none());
}
