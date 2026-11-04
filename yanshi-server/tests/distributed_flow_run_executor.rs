//! W4.3b-b — `DistributedFlowRunExecutor` correctness.
//!
//! Drives `DistributedFlowRunExecutor::execute` directly (no HTTP, no
//! `submit_run` — that's W4.3b-c/d's job) against an `InMemoryWorkerProtocol`
//! control plane, with a hand-rolled claim/execute/report loop standing in
//! for a real `yanshi-worker` process (mirrors the pattern
//! `yanshi-worker`'s own scheduler tests use, but written independently
//! here since `yanshi-server` doesn't — and shouldn't — depend on
//! `yanshi-worker`). Proves the executor bridges scheduler state
//! transitions into the same DB `runs`/`events` shape an in-process run
//! produces.
//!
//! Requires Postgres pointed to by `YANSHI_DATABASE_TEST_URL`. Without
//! it the tests self-skip, matching every other server e2e test file.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use uuid::Uuid;
use yanshi_core::FlowCancellationToken;
use yanshi_db::{Database, EventRepo, NewRun, Repositories, RunRepo, RunStatus};
use yanshi_server::scheduler::{
  AuthenticatedControlPlane, InMemoryWorkerProtocol, WorkerAdmissionPolicy, WorkerControlPlane,
  WorkerId, WorkerTaskResult,
};
use yanshi_server::{
  DistributedFlowRunExecutor, EventBroker, LiveStateRegistry, PendingApprovalRegistry, RunContext,
  RunExecutor,
};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

// `template` (not `mock`) — `mock` is a worker-only synthetic node type
// `yanshi_config::executor::parse_workflow_definition`'s schema
// validator doesn't know about (it's never meant to reach a real HTTP
// submission), so it fails schema validation before the distributed
// scheduler ever sees it. `template` is schema-valid and requires
// nothing this test's hand-rolled fake worker can't trivially satisfy —
// what "executes" it is this file's own fake-success loop below, not a
// real template render, so the exact node type only matters for passing
// schema validation.
const TWO_NODE_TEMPLATE_WORKFLOW: &str = r#"
name: W4.3b-b distributed executor test
nodes:
  - id: n1
    type: template
    parameters:
      template: "hello from n1"
  - id: n2
    type: template
    dependencies: [n1]
    parameters:
      template: "hello from n2"
"#;

/// Claim every task the scheduler dispatches and report a trivial
/// `{"text": "ok"}` success, until `run_id` reaches a status this test's
/// caller no longer needs to feed — bounded by `max_iterations` so a
/// broken scheduler can't hang the test forever.
async fn run_fake_worker_until_drained(
  plane: WorkerControlPlane<InMemoryWorkerProtocol>,
  worker_id: WorkerId,
  max_iterations: usize,
) {
  for _ in 0..max_iterations {
    match plane.claim_task(worker_id.clone()).await {
      Ok(Some(task)) => {
        let _ = plane
          .report_result(
            worker_id.clone(),
            task.task_id,
            WorkerTaskResult::Succeeded {
              output: json!({"text": "ok"}),
              events: Vec::new(),
            },
          )
          .await;
      }
      Ok(None) => {
        tokio::time::sleep(Duration::from_millis(20)).await;
      }
      Err(_) => break,
    }
  }
}

async fn seed_queued_run(repos: &Repositories, tenant: &str, workflow: &str) -> Uuid {
  let id = Uuid::new_v4();
  repos
    .runs
    .create(NewRun {
      id,
      workflow: workflow.to_string(),
      status: RunStatus::Queued,
      run_dir: None,
      tenant_id: tenant.to_string(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .expect("run row seeded");
  id
}

#[tokio::test]
async fn distributed_run_reaches_succeeded_and_emits_ordered_events() {
  let Some(url) = live_url() else {
    eprintln!("skipping distributed_run_reaches_succeeded_and_emits_ordered_events");
    return;
  };
  let db = Database::connect_and_migrate(&url, 4)
    .await
    .expect("connect");
  let repos = Repositories::from_pool(db.pool.clone());
  let tenant = format!("tenant-distributed-executor-{}", Uuid::new_v4());
  let run_id = seed_queued_run(&repos, &tenant, TWO_NODE_TEMPLATE_WORKFLOW).await;

  let raw_plane = WorkerControlPlane::new(InMemoryWorkerProtocol::new());
  let control_plane = Arc::new(AuthenticatedControlPlane::new(
    raw_plane.clone(),
    WorkerAdmissionPolicy::default(),
  ));
  let worker_id = WorkerId::new("test-worker").expect("valid worker id");
  let worker_handle = tokio::spawn(run_fake_worker_until_drained(
    raw_plane.clone(),
    worker_id, // 2 nodes, generous headroom for claim/reconcile polling races.
    50,
  ));

  let ctx = RunContext {
    run_id,
    workflow: TWO_NODE_TEMPLATE_WORKFLOW.to_string(),
    repos: repos.clone(),
    run_base_dir: None,
    cancellation_token: FlowCancellationToken::new(),
    broker: EventBroker::new(),
    tenant_id: tenant.clone(),
    live_state_registry: Some(LiveStateRegistry::new()),
    skill_dir: None,
    approval_registry: PendingApprovalRegistry::new(),
    approval_timeout: Duration::from_secs(60),
    run_max_concurrency: 1,
    cost_limit_usd: None,
    resume: false,
  };

  let executor = DistributedFlowRunExecutor::new(control_plane);
  tokio::time::timeout(Duration::from_secs(10), executor.execute(ctx))
    .await
    .expect("executor must complete within 10s");
  let _ = worker_handle.await;

  let run = repos
    .runs
    .get(run_id)
    .await
    .expect("get run")
    .expect("run row exists");
  assert_eq!(run.status, "succeeded", "run must reach succeeded");
  // Eval 2026-10-07: a finished run's tasks and snapshot used to stay in
  // memory for the life of the process.
  assert!(raw_plane.run_snapshot(run_id).await.is_none());
  assert_eq!(raw_plane.protocol().retained_tasks(run_id).await, 0);

  let events = repos
    .events
    .list_after(&tenant, run_id, -1, 100)
    .await
    .expect("list events");
  let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
  assert_eq!(
    kinds.first(),
    Some(&"workflow.started"),
    "workflow.started must be the first event, got: {kinds:?}"
  );
  assert_eq!(
    kinds.last(),
    Some(&"workflow.completed"),
    "workflow.completed must be the last event, got: {kinds:?}"
  );
  assert!(
    kinds.iter().filter(|k| **k == "node.started").count() == 2,
    "both nodes must emit node.started, got: {kinds:?}"
  );
  assert!(
    kinds.iter().filter(|k| **k == "node.completed").count() == 2,
    "both nodes must emit node.completed, got: {kinds:?}"
  );
  // n1 must fully complete before n2 starts (n2 depends on n1) — the exact
  // sequence the executor's completed-before-started emission ordering
  // (see distributed_run.rs's drive loop) exists to guarantee.
  assert_eq!(
    kinds,
    vec![
      "workflow.started",
      "node.started",
      "node.completed",
      "node.started",
      "node.completed",
      "workflow.completed",
    ],
    "expected n1 to fully start+complete before n2 starts, got: {kinds:?}"
  );
}

#[tokio::test]
async fn distributed_run_fails_when_a_node_reports_failure() {
  let Some(url) = live_url() else {
    eprintln!("skipping distributed_run_fails_when_a_node_reports_failure");
    return;
  };
  let db = Database::connect_and_migrate(&url, 4)
    .await
    .expect("connect");
  let repos = Repositories::from_pool(db.pool.clone());
  let tenant = format!("tenant-distributed-executor-fail-{}", Uuid::new_v4());
  let workflow = "name: fail test\nnodes:\n  - id: n1\n    type: template\n    parameters:\n      template: \"x\"\n";
  let run_id = seed_queued_run(&repos, &tenant, workflow).await;

  let raw_plane = WorkerControlPlane::new(InMemoryWorkerProtocol::new());
  let control_plane = Arc::new(AuthenticatedControlPlane::new(
    raw_plane.clone(),
    WorkerAdmissionPolicy::default(),
  ));
  let worker_id = WorkerId::new("test-worker-fail").expect("valid worker id");
  let plane_for_worker = raw_plane.clone();
  let worker_handle = tokio::spawn(async move {
    for _ in 0..50 {
      match plane_for_worker.claim_task(worker_id.clone()).await {
        Ok(Some(task)) => {
          let _ = plane_for_worker
            .report_result(
              worker_id.clone(),
              task.task_id,
              WorkerTaskResult::Failed {
                error: "synthetic node failure".to_string(),
                retryable: false,
                events: Vec::new(),
              },
            )
            .await;
        }
        Ok(None) => tokio::time::sleep(Duration::from_millis(20)).await,
        Err(_) => break,
      }
    }
  });

  let ctx = RunContext {
    run_id,
    workflow: workflow.to_string(),
    repos: repos.clone(),
    run_base_dir: None,
    cancellation_token: FlowCancellationToken::new(),
    broker: EventBroker::new(),
    tenant_id: tenant.clone(),
    live_state_registry: None,
    skill_dir: None,
    approval_registry: PendingApprovalRegistry::new(),
    approval_timeout: Duration::from_secs(60),
    run_max_concurrency: 1,
    cost_limit_usd: None,
    resume: false,
  };

  let executor = DistributedFlowRunExecutor::new(control_plane);
  tokio::time::timeout(Duration::from_secs(10), executor.execute(ctx))
    .await
    .expect("executor must complete within 10s");
  let _ = worker_handle.await;

  let run = repos
    .runs
    .get(run_id)
    .await
    .expect("get run")
    .expect("run row exists");
  assert_eq!(run.status, "failed", "run must reach failed");
  assert!(
    run
      .error
      .as_deref()
      .unwrap_or("")
      .contains("synthetic node failure"),
    "error must surface the node failure reason, got: {:?}",
    run.error
  );
}

/// A cancelled run's queued tasks leave the queue, so no worker picks
/// them up after the run is over.
#[tokio::test]
async fn cancelled_distributed_run_releases_its_queued_tasks() {
  let Some(url) = live_url() else {
    eprintln!("skipping cancelled_distributed_run_releases_its_queued_tasks");
    return;
  };
  let db = Database::connect_and_migrate(&url, 4)
    .await
    .expect("connect");
  let repos = Repositories::from_pool(db.pool.clone());
  let tenant = format!("tenant-distributed-cancel-{}", Uuid::new_v4());
  let run_id = seed_queued_run(&repos, &tenant, TWO_NODE_TEMPLATE_WORKFLOW).await;

  let raw_plane = WorkerControlPlane::new(InMemoryWorkerProtocol::new());
  let control_plane = Arc::new(AuthenticatedControlPlane::new(
    raw_plane.clone(),
    WorkerAdmissionPolicy::default(),
  ));
  let token = FlowCancellationToken::new();
  let ctx = RunContext {
    run_id,
    workflow: TWO_NODE_TEMPLATE_WORKFLOW.to_string(),
    repos: repos.clone(),
    run_base_dir: None,
    cancellation_token: token.clone(),
    broker: EventBroker::new(),
    tenant_id: tenant.clone(),
    live_state_registry: None,
    skill_dir: None,
    approval_registry: PendingApprovalRegistry::new(),
    approval_timeout: Duration::from_secs(60),
    run_max_concurrency: 1,
    cost_limit_usd: None,
    resume: false,
  };
  // No worker runs: n1 sits in the queue until the cancel.
  let canceller = tokio::spawn(async move {
    tokio::time::sleep(Duration::from_millis(500)).await;
    token.cancel();
  });
  let executor = DistributedFlowRunExecutor::new(control_plane);
  tokio::time::timeout(Duration::from_secs(10), executor.execute(ctx))
    .await
    .expect("executor must stop on cancel");
  let _ = canceller.await;

  let run = repos.runs.get(run_id).await.unwrap().unwrap();
  assert_eq!(run.status, "cancelled");
  assert_eq!(raw_plane.protocol().retained_tasks(run_id).await, 0);
  let worker_id = WorkerId::new("late-worker").expect("valid worker id");
  assert!(
    raw_plane.claim_task(worker_id).await.unwrap().is_none(),
    "a cancelled run's task must not be claimable"
  );
}
