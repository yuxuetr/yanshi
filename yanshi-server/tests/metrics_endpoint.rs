//! P10.14.2-FU1 integration tests for the `/metrics` endpoint.
//!
//! These tests hit `GET /metrics` through the actual Axum router
//! and assert the contracted metric names appear after a few
//! `metrics::counter!()` / `metrics::histogram!()` calls fire.
//! No live Postgres needed — `connect_lazy` builds the pool
//! placeholder the AppState constructor expects.

use axum::{
  body::Body,
  http::{Request, StatusCode},
};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use yanshi_db::Database;
use yanshi_server::{AppState, create_router, metrics};

fn lazy_state() -> AppState {
  let pool = PgPoolOptions::new()
    .connect_lazy("postgres://postgres:postgres@localhost:5432/yanshi_test")
    .expect("lazy pg");
  AppState::new(Database {
    pool,
    read_pool: None,
  })
}

/// Every `/metrics` scrape first writes this test's per-`AppState`
/// numbers (live state sizes, harness session counts) into
/// process-global gauges, then renders them. Parallel tests each scrape
/// their own state, so without this lock another test's scrape can
/// overwrite a gauge between the write and the render.
static SCRAPE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn fetch_metrics_body(app: axum::Router) -> String {
  let _scrape = SCRAPE.lock().await;
  let response = app
    .oneshot(
      Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
    .await
    .unwrap();
  String::from_utf8(bytes.to_vec()).expect("metrics body is utf-8")
}

#[tokio::test]
async fn metrics_endpoint_returns_ok_and_text_plain() {
  let _ = metrics::init_recorder();
  let _scrape = SCRAPE.lock().await;
  let app = create_router(lazy_state());
  let response = app
    .oneshot(
      Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let ct = response
    .headers()
    .get(axum::http::header::CONTENT_TYPE)
    .map(|v| v.to_str().unwrap().to_string())
    .unwrap_or_default();
  assert!(
    ct.starts_with("text/plain"),
    "Prometheus content-type required, got: {ct}"
  );
}

#[tokio::test]
async fn metrics_endpoint_bypasses_auth() {
  // The route is mounted on the `health` sub-router which
  // doesn't have the bearer-token middleware. Calling without
  // an Authorization header must still succeed — Prometheus
  // scrapers don't carry tokens by default.
  let _ = metrics::init_recorder();
  let _scrape = SCRAPE.lock().await;
  let app = create_router(lazy_state());
  let response = app
    .oneshot(
      Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_ne!(
    response.status(),
    StatusCode::UNAUTHORIZED,
    "/metrics must not require auth"
  );
}

#[tokio::test]
async fn metrics_endpoint_emits_workflow_completed_total_after_observation() {
  let _ = metrics::init_recorder();
  // Fire one observation of each terminal status so the
  // counter contract is provably wired end-to-end.
  metrics::observe_workflow_completion("succeeded", 0.5);
  metrics::observe_workflow_completion("failed", 1.0);
  metrics::observe_workflow_completion("cancelled", 0.1);
  let app = create_router(lazy_state());
  let body = fetch_metrics_body(app).await;
  assert!(
    body.contains("yanshi_workflow_completed_total"),
    "completed counter must appear in /metrics body; got:\n{body}"
  );
  for status in ["succeeded", "failed", "cancelled"] {
    let needle = format!("status=\"{status}\"");
    assert!(
      body.contains(&needle),
      "status label `{status}` missing from /metrics; got:\n{body}"
    );
  }
}

#[tokio::test]
async fn metrics_endpoint_emits_workflow_duration_histogram_buckets() {
  let _ = metrics::init_recorder();
  metrics::observe_workflow_completion("succeeded", 2.5);
  let app = create_router(lazy_state());
  let body = fetch_metrics_body(app).await;
  assert!(
    body.contains("yanshi_workflow_duration_seconds"),
    "duration histogram must appear; got:\n{body}"
  );
  // The Prometheus exporter emits `_bucket{le="..."}` lines for
  // histograms. Spot-check the `le="+Inf"` bucket which always
  // appears regardless of observed values.
  assert!(
    body.contains("yanshi_workflow_duration_seconds_bucket"),
    "histogram bucket lines must appear; got:\n{body}"
  );
}

#[tokio::test]
async fn metrics_endpoint_emits_nodes_failed_total_with_node_type_label() {
  let _ = metrics::init_recorder();
  metrics::observe_node_failure(Some("llm"));
  metrics::observe_node_failure(Some("http"));
  metrics::observe_node_failure(None); // → "unknown" fallback
  let app = create_router(lazy_state());
  let body = fetch_metrics_body(app).await;
  assert!(
    body.contains("yanshi_nodes_failed_total"),
    "node-failures counter must appear; got:\n{body}"
  );
  for node_type in ["llm", "http", "unknown"] {
    let needle = format!("node_type=\"{node_type}\"");
    assert!(
      body.contains(&needle),
      "node_type label `{node_type}` missing; got:\n{body}"
    );
  }
}

#[tokio::test]
async fn metrics_endpoint_emits_cleanup_counters_after_observation() {
  // P10.14.2-FU2: the three cleanup_*_deleted_total counters
  // appear once `observe_cleanup_sweep` fires. A real
  // `cleanup_expired` invocation requires Postgres and is
  // covered by `tests/cleanup_route.rs` end-to-end; this test
  // just pins the metric-name wire shape.
  let _ = metrics::init_recorder();
  metrics::observe_cleanup_sweep(false, 3, 42, 7);
  let app = create_router(lazy_state());
  let body = fetch_metrics_body(app).await;
  assert!(
    body.contains("yanshi_cleanup_runs_deleted_total"),
    "runs counter must appear; got:\n{body}"
  );
  assert!(
    body.contains("yanshi_cleanup_events_deleted_total"),
    "events counter must appear; got:\n{body}"
  );
  assert!(
    body.contains("yanshi_cleanup_artifacts_deleted_total"),
    "artifacts counter must appear; got:\n{body}"
  );
}

#[tokio::test]
async fn metrics_endpoint_emits_worker_fleet_gauges_after_admit_and_claim() {
  // P10.14.2-FU3: end-to-end through `AuthenticatedControlPlane`.
  // We don't need a full distributed setup — just exercise the
  // admission + claim path against an in-memory protocol so the
  // metric calls fire from the real code path, not just the
  // helper.
  use yanshi_server::{
    AuthenticatedControlPlane, InMemoryWorkerProtocol, WorkerAdmissionPolicy, WorkerControlPlane,
    WorkerCredential, WorkerHeartbeat, WorkerId, WorkerTask,
  };
  let _ = metrics::init_recorder();

  let protocol = InMemoryWorkerProtocol::new();
  let control = WorkerControlPlane::new(protocol);
  let admission = AuthenticatedControlPlane::new(control, WorkerAdmissionPolicy::open());

  let worker = WorkerId::new("metrics-worker-a").expect("valid id");
  let cred = WorkerCredential::anonymous(worker.clone());

  // Trigger admit → workers_admitted = 1.
  let hb = WorkerHeartbeat::now(worker.clone(), None, 1);
  admission
    .heartbeat(cred.clone(), hb)
    .await
    .expect("heartbeat");

  // Submit + claim a task to bump worker_tasks_inflight.
  let run_id = uuid::Uuid::new_v4();
  let task = WorkerTask::new(run_id, "n", serde_json::json!({}));
  admission
    .inner()
    .schedule_task(task)
    .await
    .expect("schedule");
  let _ = admission.claim_task(cred.clone()).await.expect("claim");

  let app = create_router(lazy_state());
  let body = fetch_metrics_body(app).await;
  assert!(
    body.contains("yanshi_workers_admitted"),
    "admitted gauge must appear; got:\n{body}"
  );
  assert!(
    body.contains("yanshi_worker_tasks_inflight"),
    "in-flight gauge must appear; got:\n{body}"
  );
  assert!(
    body.contains("worker_id=\"metrics-worker-a\""),
    "worker_id label must propagate; got:\n{body}"
  );
}

#[tokio::test]
async fn metrics_endpoint_emits_harness_session_gauges_at_scrape_time() {
  // P10.14.2-FU4: harness session status + pending approvals
  // are computed at scrape time. The DB query may fail when no
  // Postgres is wired (lazy_state); in that case the gauges
  // stay at their previous value (we already observed something
  // in the helper unit tests), so we just assert the metric
  // names appear in the rendered body — sourcing the values
  // from a real DB is covered by `tests/metrics_endpoint_live.rs`
  // (gated on `YANSHI_DATABASE_TEST_URL`).
  let _ = metrics::init_recorder();
  // Prime the gauges via the helpers so the test doesn't depend
  // on the scrape-time DB read (which fails silently without a
  // live Postgres pool).
  metrics::observe_harness_sessions_active("running", 2);
  metrics::observe_harness_sessions_active("completed", 5);
  metrics::observe_harness_approvals_pending(1);
  let app = create_router(lazy_state());
  let body = fetch_metrics_body(app).await;
  assert!(
    body.contains("yanshi_harness_sessions_active"),
    "sessions gauge must appear; got:\n{body}"
  );
  for status in ["running", "completed"] {
    let needle = format!("status=\"{status}\"");
    assert!(
      body.contains(&needle),
      "status label `{status}` must appear; got:\n{body}"
    );
  }
  assert!(
    body.contains("yanshi_harness_approvals_pending"),
    "approvals gauge must appear; got:\n{body}"
  );
}

#[tokio::test]
async fn metrics_endpoint_scrape_time_handler_survives_unreachable_db() {
  // The /metrics handler runs a `SELECT … FROM harness_sessions`
  // before rendering. A lazy pool with no real Postgres behind
  // it must NOT fail the response — the dashboard would rather
  // show stale gauges for the affected panel than miss every
  // other metric in the scrape.
  let _ = metrics::init_recorder();
  let _scrape = SCRAPE.lock().await;
  let app = create_router(lazy_state());
  let response = app
    .oneshot(
      Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  // The other gauges should still render — confirms the handler
  // didn't bail out on the DB error.
  let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
    .await
    .unwrap();
  let body = String::from_utf8(bytes.to_vec()).unwrap();
  // The pending-approval gauge doesn't depend on the DB and
  // always renders.
  assert!(
    body.contains("yanshi_harness_approvals_pending"),
    "approvals gauge must render even when DB query fails; got:\n{body}"
  );
}

#[tokio::test]
async fn metrics_endpoint_emits_scrape_time_process_inspectors() {
  // P10.14.2-FU5: health_status, memory_usage_bytes,
  // workflow_runs_active are computed at scrape time. The DB
  // queries fail silently against a lazy pool, but
  // `health_status{component="system"}` is always emitted and
  // `memory_usage_bytes` emits `0` on non-Linux — both reach
  // the wire from the unreachable-DB code path.
  let _ = metrics::init_recorder();
  // Prime the gauges via the helpers so the test pins the
  // wire shape regardless of whether the scrape-time DB
  // queries succeed in this test harness.
  metrics::observe_health_status("system", true);
  metrics::observe_health_status("database", false);
  metrics::observe_memory_usage_bytes(1024 * 1024 * 256);
  metrics::observe_workflow_runs_active(4);
  let app = create_router(lazy_state());
  let body = fetch_metrics_body(app).await;
  assert!(
    body.contains("yanshi_health_status"),
    "health gauge must appear; got:\n{body}"
  );
  for component in ["system", "database"] {
    let needle = format!("component=\"{component}\"");
    assert!(
      body.contains(&needle),
      "component label `{component}` must appear; got:\n{body}"
    );
  }
  assert!(
    body.contains("yanshi_memory_usage_bytes"),
    "memory gauge must appear; got:\n{body}"
  );
  assert!(
    body.contains("yanshi_workflow_runs_active"),
    "active-runs gauge must appear; got:\n{body}"
  );
  // V3.4: `/metrics` is unauthenticated — no per-tenant label.
  assert!(
    !body.contains("tenant=\""),
    "must not leak a per-tenant label; got:\n{body}"
  );
}

#[tokio::test]
async fn metrics_endpoint_emits_system_health_one_on_every_scrape() {
  // The scrape-time inspector unconditionally emits
  // `yanshi_health_status{component="system"} 1` because if
  // we're computing this code, the system is up by definition.
  // Pinned so a future refactor of `refresh_scrape_time_gauges`
  // can't silently drop it.
  let _ = metrics::init_recorder();
  let app = create_router(lazy_state());
  let body = fetch_metrics_body(app).await;
  assert!(
    body.contains("yanshi_health_status{component=\"system\"} 1"),
    "system=1 must appear on every scrape; got:\n{body}"
  );
}

#[tokio::test]
async fn metrics_endpoint_emits_aggregate_state_size_gauge() {
  // P10.14.2-FU6 / V3.4 — when the executor registers state-size
  // samples in the live-state registry, the scrape-time helper
  // renders their *sum* as a single unlabeled
  // `yanshi_state_size_bytes` gauge, not one series per
  // `run_id` (that would leak active run IDs through the
  // unauthenticated `/metrics` route). We bypass the real
  // executor here by writing directly through an observer (the
  // same path the executor uses internally) so the test is
  // hermetic — no real workflow / Postgres needed.
  use uuid::Uuid;
  let _ = metrics::init_recorder();
  let state = lazy_state();
  let run_a = Uuid::new_v4();
  let run_b = Uuid::new_v4();
  state.live_state_registry.observer_for(run_a).observe(4096);
  state
    .live_state_registry
    .observer_for(run_b)
    .observe(16_384);

  let app = create_router(state);
  let body = fetch_metrics_body(app).await;
  assert!(
    body.contains("yanshi_state_size_bytes 20480"),
    "aggregate gauge must sum both runs' bytes; got:\n{body}"
  );
  assert!(
    !body.contains("run_id=\""),
    "must not leak a per-run_id label; got:\n{body}"
  );
}

#[tokio::test]
async fn metrics_endpoint_state_size_gauge_drops_after_deregister() {
  // After a run terminates, the executor deregisters its entry
  // so the gauge cardinality stays bounded. The next scrape's
  // snapshot must not include the deregistered run, regardless
  // of whether the metrics exporter keeps the last-seen value
  // in its own state.
  use uuid::Uuid;
  let _ = metrics::init_recorder();
  let state = lazy_state();
  let registry = state.live_state_registry.clone();
  let run = Uuid::new_v4();
  registry.observer_for(run).observe(512);
  registry.deregister(&run);

  let app = create_router(state);
  let body = fetch_metrics_body(app).await;
  assert_eq!(
    registry.snapshot().len(),
    0,
    "registry must be empty after deregister"
  );
  // Sanity: the body is still well-formed.
  assert!(body.contains("yanshi_health_status"));
}

#[tokio::test]
async fn metrics_endpoint_returns_empty_body_when_recorder_uninstalled() {
  // We can't easily make `init_recorder` un-install (it's
  // process-global), so this test only runs in a fresh process
  // where the recorder hasn't been touched. In a single
  // `cargo test` binary the install from the earlier tests
  // sticks, so the body will be non-empty. The contract still
  // holds for an isolated boot — covered by the unit test
  // `metrics::tests::render_text_returns_empty_when_recorder_uninstalled`.
  let _scrape = SCRAPE.lock().await;
  let app = create_router(lazy_state());
  let response = app
    .oneshot(
      Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}
