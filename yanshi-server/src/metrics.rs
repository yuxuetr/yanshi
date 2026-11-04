//! Prometheus `/metrics` endpoint emission (P10.14.2-FU1).
//!
//! The Grafana dashboard checked in under `dashboards/grafana/`
//! (P10.14.2) pins the metric-name contract this module satisfies.
//! Production code paths fire `metrics::counter!()` /
//! `metrics::histogram!()` / `metrics::gauge!()` macros; this
//! module installs the recorder that aggregates them into the
//! Prometheus text format scraped by `GET /metrics`.
//!
//! Wiring status (per-slice closure of `P10.14.2-FU*`):
//!
//! - **FU1** — `yanshi_workflow_completed_total{status}`,
//!   `yanshi_workflow_duration_seconds`,
//!   `yanshi_nodes_failed_total{node_type}` wired via
//!   `WorkflowEventListener`.
//! - **FU2** — `yanshi_cleanup_*_deleted_total` wired via
//!   `cleanup::cleanup_expired`.
//! - **FU3** — `yanshi_workers_admitted` /
//!   `yanshi_worker_tasks_inflight` wired via
//!   `AuthenticatedControlPlane`.
//! - **FU4** — `yanshi_harness_sessions_active{status}` /
//!   `yanshi_harness_approvals_pending` wired via
//!   scrape-time `refresh_scrape_time_gauges`.
//! - **FU5** — `yanshi_health_status{component}` /
//!   `yanshi_memory_usage_bytes` /
//!   `yanshi_workflow_runs_active{tenant}` wired via
//!   the same scrape-time helper.
//! - **Deferred to `P10.14.2-FU6`:**
//!   `yanshi_state_size_bytes{run_id}` — the gauge requires
//!   architectural access to live `Flow` state-pool contents
//!   that the server doesn't currently expose; a proxy
//!   (e.g. `events` payload-bytes per run) would mislead
//!   operators.
//!
//! The current state of each series is mirrored in
//! `dashboards/README.md` under "Current emission status."

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Snapshot handle installed by [`init_recorder`]. Held in a
/// `OnceLock` so the recorder install only happens once per
/// process — installing twice panics, so multi-`run()` callers
/// (tests that boot a server multiple times in one process) need
/// the idempotency.
static RECORDER: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the Prometheus recorder so subsequent
/// `metrics::counter!()` / `metrics::histogram!()` /
/// `metrics::gauge!()` calls anywhere in the workspace are
/// aggregated. Idempotent — subsequent calls are no-ops.
///
/// Returns `Ok(())` on first call, `Ok(())` on subsequent calls
/// (the existing handle stays in place), or `Err(String)` if the
/// recorder install fails for a reason other than "already
/// installed" (typically an internal builder error).
pub fn init_recorder() -> Result<(), String> {
  // Serialize installs: without this, a second caller could lose the
  // `set_global_recorder` race and return `Ok` before the winner has
  // stored its handle in `RECORDER`, so its first scrape rendered an
  // empty body (seen as a flaky `/metrics` test).
  static INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
  let _init = INIT
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  if RECORDER.get().is_some() {
    return Ok(());
  }
  let recorder = PrometheusBuilder::new()
    .set_buckets_for_metric(
      metrics_exporter_prometheus::Matcher::Suffix("_seconds".into()),
      // Histogram buckets tuned for workflow durations: 100ms
      // up to 10 minutes, covering the realistic span from a
      // fast template-only run to a long multi-LLM agent loop.
      // The `Inf` bucket catches the long tail.
      &[
        0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
      ],
    )
    .map_err(|e| format!("set bucket schedule: {e}"))?
    .build_recorder();
  let handle = recorder.handle();
  // `set_global_recorder` is one-shot per process. If a prior
  // install already happened (e.g. another test in the same
  // binary booted a server), we silently accept the existing
  // recorder rather than panic — `RECORDER.get()` already gated
  // the common case; this branch is the rare race.
  if metrics::set_global_recorder(recorder).is_err() {
    tracing::debug!("metrics recorder already installed; reusing existing");
    return Ok(());
  }
  RECORDER.set(handle).ok();
  Ok(())
}

/// Render the current snapshot as Prometheus text format.
/// Returns the empty string when the recorder hasn't been
/// installed yet — that's the operator-visible signal that
/// `init_recorder()` was never called (e.g. a test built the
/// router without going through `run()`).
pub fn render_text() -> String {
  RECORDER.get().map(|h| h.render()).unwrap_or_default()
}

/// Metric-name constants — pinned exactly to the contract
/// documented in `dashboards/README.md` and
/// `docs/KUBERNETES_DEPLOYMENT.md`. Centralising them here lets
/// the wire-shape compat tests assert the names without
/// stringly-typing each call site.
pub mod names {
  /// Counter, label `status` (`succeeded` / `failed` / `cancelled`).
  pub const WORKFLOW_COMPLETED_TOTAL: &str = "yanshi_workflow_completed_total";
  /// Histogram of full-run wall clock in seconds.
  pub const WORKFLOW_DURATION_SECONDS: &str = "yanshi_workflow_duration_seconds";
  /// Counter, label `node_type`.
  pub const NODES_FAILED_TOTAL: &str = "yanshi_nodes_failed_total";
  /// Counter — rows reaped from `runs` per sweep (P10.14.2-FU2).
  pub const CLEANUP_RUNS_DELETED_TOTAL: &str = "yanshi_cleanup_runs_deleted_total";
  /// Counter — rows reaped from `events` per sweep.
  pub const CLEANUP_EVENTS_DELETED_TOTAL: &str = "yanshi_cleanup_events_deleted_total";
  /// Counter — rows reaped from `artifacts` per sweep.
  pub const CLEANUP_ARTIFACTS_DELETED_TOTAL: &str = "yanshi_cleanup_artifacts_deleted_total";
  /// Gauge — currently admitted worker count (P10.14.2-FU3).
  pub const WORKERS_ADMITTED: &str = "yanshi_workers_admitted";
  /// Gauge, label `worker_id` — in-flight tasks per worker.
  pub const WORKER_TASKS_INFLIGHT: &str = "yanshi_worker_tasks_inflight";
  /// Gauge, label `status` — active Harness Mode sessions per
  /// status (P10.14.2-FU4).
  pub const HARNESS_SESSIONS_ACTIVE: &str = "yanshi_harness_sessions_active";
  /// Gauge — pending approval requests parked in the
  /// `PendingApprovalRegistry`.
  pub const HARNESS_APPROVALS_PENDING: &str = "yanshi_harness_approvals_pending";
  /// Gauge, label `component` — health probe per subsystem
  /// (P10.14.2-FU5). `1` = up, `0` = down.
  pub const HEALTH_STATUS: &str = "yanshi_health_status";
  /// Gauge — server process resident memory in bytes.
  pub const MEMORY_USAGE_BYTES: &str = "yanshi_memory_usage_bytes";
  /// Gauge, label `tenant` — queued + running workflow runs
  /// per tenant.
  pub const WORKFLOW_RUNS_ACTIVE: &str = "yanshi_workflow_runs_active";
  /// Gauge, label `run_id` — estimated bytes held in the
  /// live `Flow::state_pool` for an in-flight workflow run
  /// (P10.14.2-FU6). Re-emitted from a process-local snapshot
  /// at scrape time; absent for runs that have already
  /// terminated (the executor deregisters the entry on
  /// completion so the gauge cardinality stays bounded).
  pub const STATE_SIZE_BYTES: &str = "yanshi_state_size_bytes";
}

/// Record the terminal status of a workflow run. Fires the
/// `yanshi_workflow_completed_total{status}` counter and the
/// `yanshi_workflow_duration_seconds` histogram together —
/// every terminal transition is observed exactly once, so the
/// rate-of-completions panel and the p50/p95/p99 panel both
/// stay correct.
pub fn observe_workflow_completion(status: &'static str, duration_seconds: f64) {
  metrics::counter!(names::WORKFLOW_COMPLETED_TOTAL, "status" => status).increment(1);
  metrics::histogram!(names::WORKFLOW_DURATION_SECONDS).record(duration_seconds);
}

/// Record a node failure with its capability label so the
/// "what broke just now" panel breaks out by `node_type`. Pass
/// `None` (the pre-P10.16.2 default) when the workflow event
/// doesn't carry a node_type — the metric still increments but
/// against an `unknown` label.
pub fn observe_node_failure(node_type: Option<&str>) {
  let node_type = node_type.unwrap_or("unknown").to_string();
  metrics::counter!(names::NODES_FAILED_TOTAL, "node_type" => node_type).increment(1);
}

/// Record one retention-sweep invocation (P10.14.2-FU2). Bumps
/// the three `yanshi_cleanup_*_deleted_total` counters by
/// the per-row deletion counts from a [`crate::cleanup::CleanupReport`].
/// Dry-run sweeps are skipped — the dashboard panel renders rate
/// of *actual* deletions, not previews.
pub fn observe_cleanup_sweep(
  dry_run: bool,
  runs_deleted: u64,
  events_deleted: u64,
  artifacts_deleted: u64,
) {
  if dry_run {
    return;
  }
  metrics::counter!(names::CLEANUP_RUNS_DELETED_TOTAL).increment(runs_deleted);
  metrics::counter!(names::CLEANUP_EVENTS_DELETED_TOTAL).increment(events_deleted);
  metrics::counter!(names::CLEANUP_ARTIFACTS_DELETED_TOTAL).increment(artifacts_deleted);
}

/// Record the currently-admitted worker count (P10.14.2-FU3).
/// Called from `AuthenticatedControlPlane::admit` after every
/// successful admission so the gauge tracks the distinct-worker
/// count exactly. `usize -> f64` is safe — admission caps are
/// always small relative to `f64`'s integer-exact range.
pub fn observe_workers_admitted(count: usize) {
  metrics::gauge!(names::WORKERS_ADMITTED).set(count as f64);
}

/// Record one worker's in-flight task count (P10.14.2-FU3).
/// Called from `AuthenticatedControlPlane::claim_task` and
/// `report_result` so the gauge stays current. Pass the
/// post-mutation value — the gauge isn't an increment helper,
/// it stores the absolute count per `worker_id` label.
pub fn observe_worker_tasks_inflight(worker_id: &str, count: u32) {
  metrics::gauge!(names::WORKER_TASKS_INFLIGHT, "worker_id" => worker_id.to_string())
    .set(count as f64);
}

/// Record one Harness Mode session-status bucket
/// (P10.14.2-FU4). Called once per known status at scrape
/// time from the `/metrics` handler — the gauge stores the
/// absolute count, so emitting all four buckets every
/// scrape is the right pattern (a status that drops to 0
/// renders as 0, not as a stale value).
pub fn observe_harness_sessions_active(status: &'static str, count: u64) {
  metrics::gauge!(names::HARNESS_SESSIONS_ACTIVE, "status" => status).set(count as f64);
}

/// Record the count of pending approval requests
/// (P10.14.2-FU4). Sourced from
/// `PendingApprovalRegistry::pending_count()` at scrape time;
/// anything > 0 means an operator action is queued.
pub fn observe_harness_approvals_pending(count: usize) {
  metrics::gauge!(names::HARNESS_APPROVALS_PENDING).set(count as f64);
}

/// Record one health probe (P10.14.2-FU5). `up = true` →
/// gauge `1`, `up = false` → gauge `0`. Stat panel on the
/// dashboard maps these values to UP / DOWN colour bands.
pub fn observe_health_status(component: &'static str, up: bool) {
  metrics::gauge!(names::HEALTH_STATUS, "component" => component).set(if up { 1.0 } else { 0.0 });
}

/// Record the server process resident memory in bytes
/// (P10.14.2-FU5). Best read from `/proc/self/statm` on
/// Linux; other platforms fall back to `0` since the scrape-
/// time inspector documents non-Linux as best-effort.
pub fn observe_memory_usage_bytes(bytes: u64) {
  metrics::gauge!(names::MEMORY_USAGE_BYTES).set(bytes as f64);
}

/// Record total active workflow runs across all tenants
/// (P10.14.2-FU5). "Active" = queued + running, NOT terminal.
/// Sourced at scrape time from a single `GROUP BY` against the
/// read pool so the gauge cost is one indexed query per
/// `/metrics` poll. V3.4: emits a single unlabeled aggregate
/// (summed across the `GROUP BY tenant_id` rows) instead of one
/// series per `tenant` label — `/metrics` is deliberately
/// unauthenticated, so a per-tenant label would let any caller
/// enumerate active tenant IDs by scraping it.
pub fn observe_workflow_runs_active(count: u64) {
  metrics::gauge!(names::WORKFLOW_RUNS_ACTIVE).set(count as f64);
}

/// Record the total live state-pool size across all in-flight
/// workflows (P10.14.2-FU6). `total_bytes` is the sum of the
/// estimated serialized size of every active run's
/// `Flow::context.state_pool`, snapshotted by the executor after
/// every node completes and held in the `LiveStateRegistry` for
/// scrape-time reads. Called once per scrape from the `/metrics`
/// handler. V3.4: emits a single unlabeled aggregate instead of
/// one series per `run_id` label, for the same unauthenticated-route
/// reason as `observe_workflow_runs_active` above.
pub fn observe_state_size_bytes(total_bytes: u64) {
  metrics::gauge!(names::STATE_SIZE_BYTES).set(total_bytes as f64);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn metric_names_match_contracted_constants() {
    // Pin the exact wire strings — the Grafana dashboard JSON
    // queries against them verbatim, so any rename here is a
    // breaking change for operators.
    assert_eq!(
      names::WORKFLOW_COMPLETED_TOTAL,
      "yanshi_workflow_completed_total"
    );
    assert_eq!(
      names::WORKFLOW_DURATION_SECONDS,
      "yanshi_workflow_duration_seconds"
    );
    assert_eq!(names::NODES_FAILED_TOTAL, "yanshi_nodes_failed_total");
  }

  #[test]
  fn render_text_returns_empty_when_recorder_uninstalled() {
    // A different process (`cargo test`) than the one that
    // installed the recorder gets an empty string instead of a
    // panic. Lets unit tests run without booting the full
    // server.
    if RECORDER.get().is_none() {
      assert_eq!(render_text(), "");
    }
  }

  #[test]
  fn init_recorder_is_idempotent_within_process() {
    // Calling twice is fine — the second call is a no-op.
    let _ = init_recorder();
    let _ = init_recorder();
    // After two installs we should still have a single handle.
    // Render shouldn't panic even if the install path raced.
    let _ = render_text();
  }

  #[test]
  fn observe_workflow_completion_increments_counter_and_histogram() {
    let _ = init_recorder();
    observe_workflow_completion("succeeded", 1.23);
    observe_workflow_completion("failed", 0.05);
    let text = render_text();
    // The exporter's text format names the counter with a
    // `_total` suffix (already in the constant) and emits a
    // `# TYPE` header line we can grep for.
    assert!(
      text.contains("yanshi_workflow_completed_total"),
      "counter must be emitted; got: {text}"
    );
    assert!(
      text.contains("status=\"succeeded\""),
      "status label must be in the output; got: {text}"
    );
    assert!(
      text.contains("yanshi_workflow_duration_seconds"),
      "histogram must be emitted; got: {text}"
    );
  }

  #[test]
  fn observe_cleanup_sweep_increments_three_counters() {
    let _ = init_recorder();
    observe_cleanup_sweep(false, 5, 100, 7);
    let text = render_text();
    assert!(
      text.contains("yanshi_cleanup_runs_deleted_total"),
      "runs counter must appear; got: {text}"
    );
    assert!(
      text.contains("yanshi_cleanup_events_deleted_total"),
      "events counter must appear; got: {text}"
    );
    assert!(
      text.contains("yanshi_cleanup_artifacts_deleted_total"),
      "artifacts counter must appear; got: {text}"
    );
  }

  #[test]
  fn observe_workers_admitted_emits_gauge() {
    let _ = init_recorder();
    observe_workers_admitted(7);
    let text = render_text();
    assert!(
      text.contains("yanshi_workers_admitted"),
      "workers gauge must appear; got: {text}"
    );
  }

  #[test]
  fn observe_harness_sessions_active_emits_per_status_label() {
    let _ = init_recorder();
    observe_harness_sessions_active("running", 3);
    observe_harness_sessions_active("completed", 17);
    let text = render_text();
    assert!(
      text.contains("yanshi_harness_sessions_active"),
      "harness sessions gauge must appear; got: {text}"
    );
    assert!(
      text.contains("status=\"running\""),
      "status label `running` must appear; got: {text}"
    );
    assert!(
      text.contains("status=\"completed\""),
      "status label `completed` must appear; got: {text}"
    );
  }

  #[test]
  fn observe_health_status_maps_bool_to_zero_or_one() {
    let _ = init_recorder();
    observe_health_status("system", true);
    observe_health_status("database", false);
    let text = render_text();
    assert!(
      text.contains("yanshi_health_status"),
      "health gauge must appear; got: {text}"
    );
    assert!(
      text.contains("component=\"system\""),
      "system component label must appear; got: {text}"
    );
    assert!(
      text.contains("component=\"database\""),
      "database component label must appear; got: {text}"
    );
  }

  #[test]
  fn observe_memory_usage_bytes_emits_gauge() {
    let _ = init_recorder();
    observe_memory_usage_bytes(123_456_789);
    let text = render_text();
    assert!(
      text.contains("yanshi_memory_usage_bytes"),
      "memory gauge must appear; got: {text}"
    );
  }

  #[test]
  fn observe_workflow_runs_active_emits_an_aggregate_gauge() {
    let _ = init_recorder();
    observe_workflow_runs_active(21);
    let text = render_text();
    assert!(
      text.contains("yanshi_workflow_runs_active"),
      "active-runs gauge must appear; got: {text}"
    );
    // V3.4: `/metrics` is unauthenticated — a `tenant` label would let
    // any caller enumerate active tenant IDs by scraping it.
    assert!(
      !text.contains("tenant=\""),
      "must not leak a per-tenant label; got: {text}"
    );
  }

  #[test]
  fn observe_harness_approvals_pending_emits_gauge() {
    let _ = init_recorder();
    observe_harness_approvals_pending(2);
    let text = render_text();
    assert!(
      text.contains("yanshi_harness_approvals_pending"),
      "pending approvals gauge must appear; got: {text}"
    );
  }

  #[test]
  fn observe_worker_tasks_inflight_emits_per_worker_label() {
    let _ = init_recorder();
    observe_worker_tasks_inflight("worker-a", 3);
    observe_worker_tasks_inflight("worker-b", 0);
    let text = render_text();
    assert!(
      text.contains("yanshi_worker_tasks_inflight"),
      "inflight gauge must appear; got: {text}"
    );
    assert!(
      text.contains("worker_id=\"worker-a\""),
      "worker_id label `worker-a` must appear; got: {text}"
    );
    assert!(
      text.contains("worker_id=\"worker-b\""),
      "worker_id label `worker-b` must appear; got: {text}"
    );
  }

  // Note: a `dry_run_is_a_no_op` test was attempted but races
  // against `observe_cleanup_sweep_increments_three_counters`
  // when cargo runs the tests in parallel (the absolute counter
  // value depends on which test ran first). The dry-run no-op
  // invariant is enforced by an obvious `if dry_run { return; }`
  // branch in `observe_cleanup_sweep` and verified by code review;
  // an integration test against a real Postgres `cleanup_expired`
  // call would test the same invariant cleanly.

  #[test]
  fn observe_node_failure_labels_with_node_type_or_unknown() {
    let _ = init_recorder();
    observe_node_failure(Some("llm"));
    observe_node_failure(None);
    let text = render_text();
    assert!(
      text.contains("yanshi_nodes_failed_total"),
      "counter must be emitted; got: {text}"
    );
    assert!(
      text.contains("node_type=\"llm\""),
      "known node_type label must appear; got: {text}"
    );
    assert!(
      text.contains("node_type=\"unknown\""),
      "fallback label `unknown` must appear for untagged failures; got: {text}"
    );
  }

  #[test]
  fn observe_state_size_bytes_emits_an_aggregate_gauge() {
    let _ = init_recorder();
    observe_state_size_bytes(1024 + 8192);
    let text = render_text();
    assert!(
      text.contains("yanshi_state_size_bytes"),
      "state size gauge must appear; got: {text}"
    );
    // V3.4: `/metrics` is unauthenticated — a `run_id` label would let
    // any caller enumerate active run IDs by scraping it.
    assert!(
      !text.contains("run_id=\""),
      "must not leak a per-run_id label; got: {text}"
    );
  }
}
