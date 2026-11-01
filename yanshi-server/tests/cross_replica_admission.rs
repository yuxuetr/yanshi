//! W4.2f — cross-replica run admission via a shared Postgres transaction.
//!
//! Before this, `RunAdmissionRegistry` was a purely in-process semaphore
//! (concurrency) + fixed-window counter (rate) checked before
//! `POST /v1/runs` created a `runs` row. With N gateway replicas, each
//! replica enforced the *same configured limit* independently — the
//! effective cluster-wide limit was silently multiplied by N. This test
//! builds two independent `AppState`s against the same Postgres database
//! (same fixture shape as the other `cross_replica_*.rs` files), submits
//! concurrent bursts split across both replicas — each burst small enough
//! that either replica's own *local* semaphore would admit all of its
//! share alone — and asserts the combined admitted count across both
//! replicas never exceeds the configured cluster-wide limit. Under the
//! old purely-local check this would have let up to 2x the limit through.
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`. Without
//! it the tests self-skip, matching every other server e2e test file.

use axum::{
  body::Body,
  http::{Request, StatusCode, header::CONTENT_TYPE},
};
use futures::future::join_all;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
use yanshi_db::Database;
use yanshi_server::AppState;
use yanshi_server::runs::{RunContext, RunExecutor};

const FIXED_DAG_WORKFLOW: &str = r#"
name: Cross-Replica Admission Test DAG
nodes:
  - id: render
    type: template
    parameters:
      template: "hello cross-replica"
"#;

/// Leaves every run `queued`, so it holds its admission slot.
struct NeverFinishes;

#[async_trait::async_trait]
impl RunExecutor for NeverFinishes {
  async fn execute(&self, _ctx: RunContext) {}
}

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn two_replica_states(
  defaults: yanshi_tools::SecurityProfileDefaults,
) -> Option<(AppState, AppState)> {
  let url = live_url()?;
  let db_a = Database::connect_and_migrate(&url, 8).await.ok()?;
  let db_b = Database::connect(&url, 8).await.ok()?;

  // Admitted runs must keep their slot for the whole burst. With the real
  // executor a template run finishes in milliseconds, frees its slot, and
  // a later submission in the same burst is legitimately admitted — the
  // test then fails on fast machines (3 or 4 of 4 admitted) and passes on
  // slow ones.
  let state_a = AppState::new(db_a)
    .with_security_defaults(defaults.clone())
    .with_executor(Arc::new(NeverFinishes));
  let state_b = AppState::new(db_b)
    .with_security_defaults(defaults)
    .with_executor(Arc::new(NeverFinishes));

  Some((state_a, state_b))
}

async fn submit(state: AppState, tenant: String) -> StatusCode {
  let app = yanshi_server::create_router(state);
  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/v1/runs")
        .header(CONTENT_TYPE, "application/json")
        .header("X-Yanshi-Tenant", &tenant)
        .body(Body::from(
          serde_json::json!({"workflow": FIXED_DAG_WORKFLOW}).to_string(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  response.status()
}

#[tokio::test]
async fn concurrent_admission_across_two_replicas_never_exceeds_the_shared_limit() {
  let mut defaults = yanshi_tools::SecurityProfile::Local.defaults();
  // Cluster-wide limit of 2. Each replica's own local pre-check budget
  // is also 2 (the registry is built from the same defaults), so a
  // 2-per-replica burst would sail through the OLD purely-local check
  // on both replicas at once (up to 4 admitted) — only the shared DB
  // transaction can correctly cap the combined total at 2.
  defaults.run_admission.max_concurrent_runs_per_tenant = 2;
  defaults
    .run_admission
    .max_run_submissions_per_minute_per_tenant = 1000;
  let Some((state_a, state_b)) = two_replica_states(defaults).await else {
    eprintln!("skipping concurrent_admission_across_two_replicas_never_exceeds_the_shared_limit");
    return;
  };

  let tenant = format!("tenant-cross-admission-{}", Uuid::new_v4());

  // 2 concurrent submissions to replica A, 2 to replica B, all in flight
  // at once for the same tenant.
  let futures: Vec<_> = [
    (state_a.clone(), tenant.clone()),
    (state_a, tenant.clone()),
    (state_b.clone(), tenant.clone()),
    (state_b, tenant.clone()),
  ]
  .into_iter()
  .map(|(state, tenant)| submit(state, tenant))
  .collect();
  let statuses = join_all(futures).await;

  let admitted = statuses.iter().filter(|s| **s == StatusCode::OK).count();
  let rejected = statuses
    .iter()
    .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
    .count();
  assert_eq!(
    admitted, 2,
    "combined admission across both replicas must be capped at the shared limit, got statuses: {statuses:?}"
  );
  assert_eq!(rejected, 2, "the other two submissions must be rejected");
}

#[tokio::test]
async fn concurrent_submission_rate_across_two_replicas_never_exceeds_the_shared_window() {
  let mut defaults = yanshi_tools::SecurityProfile::Local.defaults();
  // Rate limit of 2 per window; concurrency limit high so only the rate
  // window is under test.
  defaults.run_admission.max_concurrent_runs_per_tenant = 100;
  defaults
    .run_admission
    .max_run_submissions_per_minute_per_tenant = 2;
  let Some((state_a, state_b)) = two_replica_states(defaults).await else {
    eprintln!(
      "skipping concurrent_submission_rate_across_two_replicas_never_exceeds_the_shared_window"
    );
    return;
  };

  let tenant = format!("tenant-cross-admission-rate-{}", Uuid::new_v4());

  let futures: Vec<_> = [
    (state_a.clone(), tenant.clone()),
    (state_a, tenant.clone()),
    (state_b.clone(), tenant.clone()),
    (state_b, tenant.clone()),
  ]
  .into_iter()
  .map(|(state, tenant)| submit(state, tenant))
  .collect();
  let statuses = join_all(futures).await;

  let admitted = statuses.iter().filter(|s| **s == StatusCode::OK).count();
  assert_eq!(
    admitted, 2,
    "combined submission rate across both replicas must be capped at the shared window limit, got statuses: {statuses:?}"
  );
}
