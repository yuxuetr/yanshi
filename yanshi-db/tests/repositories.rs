//! Integration tests for the Postgres repository implementations.
//!
//! Gated by `YANSHI_DATABASE_TEST_URL` for the same reason as the
//! migrations test — keeps `cargo test --workspace` hermetic. To run:
//!
//! ```bash
//! YANSHI_DATABASE_TEST_URL=postgres://postgres:postgres@localhost:5432/yanshi_test \
//!   cargo test -p yanshi-db --test repositories
//! ```

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;
use yanshi_db::{
  AdmissionOutcome, ApprovalIntentRepo, Artifact, ArtifactRepo, Database, EventRepo,
  HarnessEventRepo, HarnessSessionRepo, HarnessSessionStatus, McpSession, McpSessionRepo,
  NewArtifact, NewEvent, NewHarnessSession, NewHarnessSessionEvent, NewRun, NewStep, Repositories,
  RunRepo, RunStatus, SkillInstall, SkillInstallRepo, StepRepo,
};

fn live_url() -> Option<String> {
  std::env::var("YANSHI_DATABASE_TEST_URL").ok()
}

async fn fresh_db() -> Option<Database> {
  let url = live_url()?;
  let db = Database::connect_and_migrate(&url, 4)
    .await
    .expect("connect + migrate");
  // Note: we intentionally do NOT TRUNCATE here. Integration tests run
  // in parallel and a global TRUNCATE wipes other tests' seeded rows
  // mid-test, causing flaky failures. Every test instead uses a unique
  // (tenant_id, run_id, session_id, skill name, etc.) so rows can't
  // collide across tests. Cleanup is a follow-up; for now CI runs with
  // a fresh DB per workflow run, so test artifacts don't accumulate.
  Some(db)
}

#[tokio::test]
async fn run_repo_create_get_list_update() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping run_repo_create_get_list_update — set YANSHI_DATABASE_TEST_URL");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let id = Uuid::new_v4();
  // Unique tenant per test invocation — see fresh_db() comment.
  let tenant = format!("tenant-run-crud-{}", Uuid::new_v4());
  let new_run = NewRun {
    id,
    workflow: "demo".into(),
    status: RunStatus::Queued,
    run_dir: Some("/tmp/x".into()),
    tenant_id: tenant.clone(),
    events_retention_days: None,
    artifacts_retention_days: None,
  };
  let created = repos.runs.create(new_run).await.expect("create run");
  assert_eq!(created.id, id);
  assert_eq!(created.status, "queued");

  let fetched = repos.runs.get(id).await.expect("get run").expect("present");
  assert_eq!(fetched.workflow, "demo");

  repos
    .runs
    .update_status(id, RunStatus::Failed, Some("oops"))
    .await
    .expect("update");

  let after = repos.runs.get(id).await.expect("get run").expect("present");
  assert_eq!(after.status, "failed");
  assert_eq!(after.error.as_deref(), Some("oops"));
  assert!(after.finished_at.is_some());

  let listed = repos.runs.list(&tenant, 10).await.expect("list");
  assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn step_and_event_repos_round_trip() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping step_and_event_repos_round_trip — set YANSHI_DATABASE_TEST_URL");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let run_id = Uuid::new_v4();
  repos
    .runs
    .create(NewRun {
      id: run_id,
      workflow: "demo".into(),
      status: RunStatus::Running,
      run_dir: None,
      tenant_id: "tenant-step-event-rt".into(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .expect("create run");

  let step = repos
    .steps
    .append(NewStep {
      run_id,
      seq: 0,
      node_id: "n0".into(),
      kind: "node".into(),
      status: "started".into(),
      duration_ms: None,
      payload: Some(json!({"hello": "world"})),
    })
    .await
    .expect("append step");
  assert_eq!(step.seq, 0);

  for seq in 0..3 {
    repos
      .events
      .append(NewEvent {
        run_id,
        seq,
        kind: "node_started".into(),
        payload: json!({"seq": seq}),
        tenant_id: None,
      })
      .await
      .expect("append event");
  }

  let events_after_zero = repos
    .events
    .list_after("default", run_id, 0, 100)
    .await
    .expect("list events");
  // seq > 0 means we get seq 1 and 2 only.
  assert_eq!(events_after_zero.len(), 2);
  assert_eq!(events_after_zero[0].seq, 1);
}

// ── M.3 expanded coverage ─────────────────────────────────────────────────
//
// The two tests above cover the Run/Step/Event happy paths. The block
// below adds per-repo CRUD coverage for every remaining table:
// Artifact, SkillInstall, McpSession, HarnessSession, HarnessEvent. Each
// test exercises the closed set of methods the repo trait declares and
// asserts at least one tenant-isolation or uniqueness invariant the
// repo guarantees, so a schema or query rewrite that breaks the
// downstream server/UI contract fails here.

async fn seed_run(repos: &Repositories, tenant: &str) -> Uuid {
  let id = Uuid::new_v4();
  repos
    .runs
    .create(NewRun {
      id,
      workflow: format!("seed-for-{tenant}"),
      status: RunStatus::Running,
      run_dir: None,
      tenant_id: tenant.into(),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .expect("seed run");
  id
}

#[tokio::test]
async fn run_repo_list_isolates_tenants() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping run_repo_list_isolates_tenants — set YANSHI_DATABASE_TEST_URL");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  // Suffix tenants with a per-invocation UUID so re-runs against the
  // same database don't see accumulated rows from previous runs (we
  // intentionally stopped TRUNCATEing in fresh_db to prevent parallel
  // tests from wiping each other's data).
  let suffix = Uuid::new_v4();
  let tenant_a = format!("tenant-a-{suffix}");
  let tenant_b = format!("tenant-b-{suffix}");
  let tenant_c = format!("tenant-c-{suffix}");
  let _a = seed_run(&repos, &tenant_a).await;
  let _b = seed_run(&repos, &tenant_b).await;

  let a_runs = repos.runs.list(&tenant_a, 10).await.expect("list a");
  let b_runs = repos.runs.list(&tenant_b, 10).await.expect("list b");
  assert_eq!(a_runs.len(), 1);
  assert_eq!(b_runs.len(), 1);
  assert_eq!(a_runs[0].tenant_id, tenant_a);
  assert_eq!(b_runs[0].tenant_id, tenant_b);
  // Cross-tenant read returns no rows.
  let ghost = repos.runs.list(&tenant_c, 10).await.expect("list ghost");
  assert!(ghost.is_empty());
}

#[tokio::test]
async fn run_repo_update_status_errors_when_missing() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping run_repo_update_status_errors_when_missing");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  let bogus = Uuid::new_v4();
  let err = repos
    .runs
    .update_status(bogus, RunStatus::Failed, Some("nope"))
    .await
    .expect_err("must error on missing run");
  let message = err.to_string();
  assert!(
    message.contains(&bogus.to_string()),
    "error must echo the missing id, got: {message}"
  );
}

/// W4.2d: `record_cancellation_intent` must be idempotent (`ON CONFLICT
/// DO NOTHING`) — the first cancel request for a run wins, redundant
/// later ones (e.g. a retried client request, or the same request
/// landing on two replicas' NOTIFY fan-out) must not error.
#[tokio::test]
async fn run_repo_record_cancellation_intent_is_idempotent() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping run_repo_record_cancellation_intent_is_idempotent");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  let tenant = format!("tenant-cancel-intent-{}", Uuid::new_v4());
  let run_id = seed_run(&repos, &tenant).await;

  repos
    .runs
    .record_cancellation_intent(run_id, &tenant)
    .await
    .expect("first record succeeds");
  repos
    .runs
    .record_cancellation_intent(run_id, &tenant)
    .await
    .expect("second record is a no-op, not an error");

  let count: i64 =
    sqlx::query_scalar("SELECT count(*) FROM run_cancellation_intents WHERE run_id = $1")
      .bind(run_id)
      .fetch_one(&db.pool)
      .await
      .expect("count query");
  assert_eq!(
    count, 1,
    "duplicate cancellation intents must not create a second row"
  );
}

/// W4.2e: `ApprovalIntentRepo::record_decision_intent` must be
/// idempotent (`ON CONFLICT (session_id, request_id) DO NOTHING`) — the
/// first decision for a given request wins, matching
/// `PendingApprovalRegistry::decide`'s own single-resolution oneshot
/// semantics. Mirrors `run_repo_record_cancellation_intent_is_idempotent`.
#[tokio::test]
async fn approval_intent_repo_record_decision_intent_is_idempotent() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping approval_intent_repo_record_decision_intent_is_idempotent");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  let session_id = Uuid::new_v4().to_string();
  let tenant = format!("tenant-approval-intent-{}", Uuid::new_v4());
  let first_decision = json!({"decision": "allow", "decided_by": "operator-1"});
  let second_decision = json!({"decision": "deny_and_stop", "decided_by": "operator-2"});

  repos
    .approval_intents
    .record_decision_intent(&session_id, "req-1", &tenant, &first_decision)
    .await
    .expect("first record succeeds");
  repos
    .approval_intents
    .record_decision_intent(&session_id, "req-1", &tenant, &second_decision)
    .await
    .expect("second record is a no-op, not an error");

  let (count, stored): (i64, serde_json::Value) = sqlx::query_as(
    "SELECT count(*) OVER (), (array_agg(decision))[1] \
     FROM approval_decision_intents WHERE session_id = $1 AND request_id = $2",
  )
  .bind(&session_id)
  .bind("req-1")
  .fetch_one(&db.pool)
  .await
  .expect("count+fetch query");
  assert_eq!(
    count, 1,
    "duplicate decision intents must not create a second row"
  );
  assert_eq!(
    stored["decided_by"], "operator-1",
    "the first decision must win, not the second"
  );
}

/// W4.2e: `EventRepo::list_pending_approvals` derives "pending" from the
/// append-only `events` log — an `approval_requested` row surfaces until
/// a matching `approval_decided` row (correlated by `request_id`)
/// appears, at which point it's excluded. This is the exact query the
/// cross-replica list/decide routes lean on, tested directly here since
/// `cross_replica_run_approvals.rs` only ever exercises the
/// still-pending half of the contract.
#[tokio::test]
async fn event_repo_list_pending_approvals_excludes_decided() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping event_repo_list_pending_approvals_excludes_decided");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  let tenant = format!("tenant-pending-approvals-{}", Uuid::new_v4());
  let run_id = seed_run(&repos, &tenant).await;

  repos
    .events
    .append(NewEvent {
      run_id,
      seq: 1,
      kind: "approval_requested".to_string(),
      payload: json!({"kind": "approval_requested", "payload": {"request": {"id": "req-a"}}}),
      tenant_id: Some(tenant.clone()),
    })
    .await
    .expect("append approval_requested for req-a");
  repos
    .events
    .append(NewEvent {
      run_id,
      seq: 2,
      kind: "approval_requested".to_string(),
      payload: json!({"kind": "approval_requested", "payload": {"request": {"id": "req-b"}}}),
      tenant_id: Some(tenant.clone()),
    })
    .await
    .expect("append approval_requested for req-b");

  let pending = repos
    .events
    .list_pending_approvals(&tenant, run_id)
    .await
    .expect("list before any decision");
  assert_eq!(pending.len(), 2, "both requests are pending so far");

  repos
    .events
    .append(NewEvent {
      run_id,
      seq: 3,
      kind: "approval_decided".to_string(),
      payload: json!({"kind": "approval_decided", "payload": {"decision": {"request_id": "req-a"}}}),
      tenant_id: Some(tenant.clone()),
    })
    .await
    .expect("append approval_decided for req-a");

  let pending = repos
    .events
    .list_pending_approvals(&tenant, run_id)
    .await
    .expect("list after deciding req-a");
  assert_eq!(pending.len(), 1, "req-a must drop out once decided");
  assert_eq!(pending[0]["id"], "req-b");
}

fn new_run_for(tenant: &str) -> NewRun {
  NewRun {
    id: Uuid::new_v4(),
    workflow: format!("admission-test-for-{tenant}"),
    status: RunStatus::Queued,
    run_dir: None,
    tenant_id: tenant.to_string(),
    events_retention_days: None,
    artifacts_retention_days: None,
  }
}

/// W4.2f: `create_if_admitted` admits up to `max_concurrent`
/// non-terminal runs for a tenant, then rejects — and the count is
/// derived from `runs.status`, not a separate counter, so completing
/// one of the admitted runs frees a slot for the next submission.
#[tokio::test]
async fn run_repo_create_if_admitted_enforces_concurrency_and_self_heals() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping run_repo_create_if_admitted_enforces_concurrency_and_self_heals");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  let tenant = format!("tenant-admission-concurrency-{}", Uuid::new_v4());

  let first = repos
    .runs
    .create_if_admitted(new_run_for(&tenant), 2, 100, 60)
    .await
    .expect("first admission call succeeds");
  let first_run = match first {
    AdmissionOutcome::Admitted(run) => run,
    other => panic!("expected Admitted, got {other:?}"),
  };

  let second = repos
    .runs
    .create_if_admitted(new_run_for(&tenant), 2, 100, 60)
    .await
    .expect("second admission call succeeds");
  assert!(matches!(second, AdmissionOutcome::Admitted(_)));

  let third = repos
    .runs
    .create_if_admitted(new_run_for(&tenant), 2, 100, 60)
    .await
    .expect("third admission call succeeds");
  match third {
    AdmissionOutcome::ConcurrencyLimitExceeded { current, limit } => {
      assert_eq!(current, 2);
      assert_eq!(limit, 2);
    }
    other => panic!("expected ConcurrencyLimitExceeded, got {other:?}"),
  }

  // Complete one of the two admitted runs — the count must self-heal
  // without any admission-specific cleanup call.
  repos
    .runs
    .update_status(first_run.id, RunStatus::Succeeded, None)
    .await
    .expect("mark first run terminal");

  let fourth = repos
    .runs
    .create_if_admitted(new_run_for(&tenant), 2, 100, 60)
    .await
    .expect("fourth admission call succeeds");
  assert!(
    matches!(fourth, AdmissionOutcome::Admitted(_)),
    "a terminal run must free its concurrency slot"
  );
}

/// W4.2f: `create_if_admitted` rejects once the tenant's submissions
/// within `window_seconds` exceed `max_per_window`, even though the
/// concurrency limit is nowhere near reached.
#[tokio::test]
async fn run_repo_create_if_admitted_enforces_the_rate_window() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping run_repo_create_if_admitted_enforces_the_rate_window");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  let tenant = format!("tenant-admission-rate-{}", Uuid::new_v4());

  let first = repos
    .runs
    .create_if_admitted(new_run_for(&tenant), 100, 1, 60)
    .await
    .expect("first admission call succeeds");
  assert!(matches!(first, AdmissionOutcome::Admitted(_)));

  let second = repos
    .runs
    .create_if_admitted(new_run_for(&tenant), 100, 1, 60)
    .await
    .expect("second admission call succeeds");
  match second {
    AdmissionOutcome::RateLimited {
      window_count,
      limit,
    } => {
      assert_eq!(window_count, 2);
      assert_eq!(limit, 1);
    }
    other => panic!("expected RateLimited, got {other:?}"),
  }

  // A different tenant has its own untouched window.
  let other_tenant = format!("tenant-admission-rate-other-{}", Uuid::new_v4());
  let unaffected = repos
    .runs
    .create_if_admitted(new_run_for(&other_tenant), 100, 1, 60)
    .await
    .expect("other tenant's admission call succeeds");
  assert!(matches!(unaffected, AdmissionOutcome::Admitted(_)));
}

#[tokio::test]
async fn step_repo_list_for_run_returns_in_seq_order() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping step_repo_list_for_run_returns_in_seq_order");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  let run_id = seed_run(&repos, "tenant-step-order").await;

  // Insert in shuffled order; list_for_run must still return seq-ascending.
  for seq in [2, 0, 1] {
    repos
      .steps
      .append(NewStep {
        run_id,
        seq,
        node_id: format!("n{seq}"),
        kind: "node".into(),
        status: "started".into(),
        duration_ms: None,
        payload: None,
      })
      .await
      .expect("append");
  }
  let steps = repos.steps.list_for_run(run_id).await.expect("list");
  let seqs: Vec<i32> = steps.iter().map(|s| s.seq).collect();
  assert_eq!(seqs, vec![0, 1, 2]);
}

#[tokio::test]
async fn artifact_repo_create_and_list_round_trip() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping artifact_repo_create_and_list_round_trip");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  let run_id = seed_run(&repos, "tenant-artifact").await;

  for i in 0..3 {
    repos
      .artifacts
      .create(NewArtifact {
        id: Uuid::new_v4(),
        run_id,
        node_id: format!("node-{i}"),
        name: format!("artifact-{i}"),
        path_or_url: format!("/tmp/artifact-{i}"),
        mime_type: Some("text/plain".into()),
        tenant_id: None,
      })
      .await
      .expect("create artifact");
  }

  let listed = repos.artifacts.list_for_run(run_id).await.expect("list");
  assert_eq!(listed.len(), 3);
  // Foreign-key tenant isolation: artifacts for another run id should
  // not surface here.
  let other_run = seed_run(&repos, "tenant-artifact-other").await;
  let other_listed = repos
    .artifacts
    .list_for_run(other_run)
    .await
    .expect("list other");
  assert!(other_listed.is_empty());
  // Sanity-check that the round-trip preserved every column.
  let first: &Artifact = &listed[0];
  assert_eq!(first.run_id, run_id);
  assert_eq!(first.mime_type.as_deref(), Some("text/plain"));
}

#[tokio::test]
async fn skill_install_repo_upsert_replaces_on_conflict() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping skill_install_repo_upsert_replaces_on_conflict");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  // Unique skill name per test invocation so accumulated rows from
  // previous runs don't pollute the global `list()`.
  let skill_name = format!("rust-expert-{}", Uuid::new_v4());
  let install = SkillInstall {
    name: skill_name.clone(),
    version: "1.0.0".into(),
    source: "local".into(),
    installed_at: Utc::now(),
    checksum: Some("abc".into()),
    tenant_id: "default".into(),
  };
  repos.skill_installs.upsert(install.clone()).await.unwrap();

  // Second upsert with a new source/checksum overwrites the row.
  let mut updated = install.clone();
  updated.source = "marketplace".into();
  updated.checksum = Some("def".into());
  repos.skill_installs.upsert(updated).await.unwrap();

  let listed = repos.skill_installs.list("default").await.unwrap();
  let rows_for_this_skill: Vec<&SkillInstall> =
    listed.iter().filter(|s| s.name == skill_name).collect();
  assert_eq!(
    rows_for_this_skill.len(),
    1,
    "upsert must not duplicate the row"
  );
  assert_eq!(rows_for_this_skill[0].source, "marketplace");
  assert_eq!(rows_for_this_skill[0].checksum.as_deref(), Some("def"));

  // Different (name, version) is a separate row.
  repos
    .skill_installs
    .upsert(SkillInstall {
      name: skill_name.clone(),
      version: "2.0.0".into(),
      source: "local".into(),
      installed_at: Utc::now(),
      checksum: None,
      tenant_id: "default".into(),
    })
    .await
    .unwrap();
  let listed = repos.skill_installs.list("default").await.unwrap();
  let rows_for_this_skill: Vec<&SkillInstall> =
    listed.iter().filter(|s| s.name == skill_name).collect();
  assert_eq!(rows_for_this_skill.len(), 2);
}

/// Q1.5.1 regression: `list(tenant)` must only return rows for the
/// requested tenant. Pre-fix `list()` was tenant-agnostic and leaked
/// every tenant's installed skill set through one call.
#[tokio::test]
async fn skill_install_repo_list_filters_by_tenant() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping skill_install_repo_list_filters_by_tenant");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let alpha = format!("tenant-alpha-{}", Uuid::new_v4());
  let beta = format!("tenant-beta-{}", Uuid::new_v4());

  repos
    .skill_installs
    .upsert(SkillInstall {
      name: format!("skill-alpha-{}", Uuid::new_v4()),
      version: "1.0.0".into(),
      source: "local".into(),
      installed_at: Utc::now(),
      checksum: None,
      tenant_id: alpha.clone(),
    })
    .await
    .unwrap();
  repos
    .skill_installs
    .upsert(SkillInstall {
      name: format!("skill-beta-{}", Uuid::new_v4()),
      version: "1.0.0".into(),
      source: "local".into(),
      installed_at: Utc::now(),
      checksum: None,
      tenant_id: beta.clone(),
    })
    .await
    .unwrap();

  let alpha_rows = repos.skill_installs.list(&alpha).await.unwrap();
  assert!(
    alpha_rows.iter().all(|r| r.tenant_id == alpha),
    "alpha listing leaked rows from another tenant"
  );
  assert!(!alpha_rows.is_empty(), "alpha should see its own row");

  let beta_rows = repos.skill_installs.list(&beta).await.unwrap();
  assert!(
    beta_rows.iter().all(|r| r.tenant_id == beta),
    "beta listing leaked rows from another tenant"
  );

  // Cross-tenant guard: alpha must not see beta's rows even if their
  // names happen to share a prefix.
  for row in &alpha_rows {
    assert_ne!(row.tenant_id, beta);
  }
}

#[tokio::test]
async fn mcp_session_repo_open_and_close_lifecycle() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping mcp_session_repo_open_and_close_lifecycle");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let id = Uuid::new_v4();
  repos
    .mcp_sessions
    .open(McpSession {
      id,
      server: "stdio:python.calc".into(),
      started_at: Utc::now(),
      ended_at: None,
      tool_calls: 0,
      metadata: Some(json!({"version": "0.1"})),
      tenant_id: "default".into(),
    })
    .await
    .unwrap();

  // close() flips ended_at and bumps tool_calls; missing id errors.
  repos.mcp_sessions.close(id, 7).await.unwrap();
  let err = repos
    .mcp_sessions
    .close(Uuid::new_v4(), 0)
    .await
    .expect_err("close on missing id must error");
  let message = err.to_string();
  assert!(
    message.contains("mcp_session"),
    "missing-id error must mention the entity type, got: {message}"
  );
}

#[tokio::test]
async fn harness_session_repo_create_get_list_update() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping harness_session_repo_create_get_list_update");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let id = Uuid::new_v4();
  let tenant = format!("tenant-harness-crud-{}", Uuid::new_v4());
  let new_session = NewHarnessSession {
    id,
    tenant_id: tenant.clone(),
    user_input: "say hi".into(),
    workspace_root: "/tmp/ws".into(),
    profile: "local".into(),
    runtime_kind: "react".into(),
    model: "mock".into(),
    skill_name: Some("rust-expert".into()),
  };
  let created = repos.harness_sessions.create(new_session).await.unwrap();
  assert_eq!(created.status, "running");

  let fetched = repos
    .harness_sessions
    .get(id)
    .await
    .unwrap()
    .expect("present");
  assert_eq!(fetched.user_input, "say hi");
  assert_eq!(fetched.skill_name.as_deref(), Some("rust-expert"));

  repos
    .harness_sessions
    .update_status(id, HarnessSessionStatus::Completed, Some("done"), None)
    .await
    .unwrap();
  let after = repos
    .harness_sessions
    .get(id)
    .await
    .unwrap()
    .expect("present");
  assert_eq!(after.status, "completed");
  assert_eq!(after.final_answer.as_deref(), Some("done"));
  assert!(after.finished_at.is_some());

  let listed = repos.harness_sessions.list(&tenant, 10).await.unwrap();
  assert_eq!(listed.len(), 1);
  let other_tenant = repos
    .harness_sessions
    .list(&format!("ghost-tenant-{}", Uuid::new_v4()), 10)
    .await
    .unwrap();
  assert!(other_tenant.is_empty());
}

/// Eval 2026-10-07 P2: a cancel must survive the executor's own late
/// terminal write, for both runs and harness sessions.
#[tokio::test]
async fn update_status_never_leaves_a_terminal_state() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping update_status_never_leaves_a_terminal_state");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let run_id = Uuid::new_v4();
  repos
    .runs
    .create(NewRun {
      id: run_id,
      workflow: "demo".into(),
      status: RunStatus::Queued,
      run_dir: None,
      tenant_id: format!("tenant-sticky-{}", Uuid::new_v4()),
      events_retention_days: None,
      artifacts_retention_days: None,
    })
    .await
    .unwrap();
  repos
    .runs
    .update_status(run_id, RunStatus::Cancelled, Some("cancel requested"))
    .await
    .unwrap();
  for late in [RunStatus::Running, RunStatus::Succeeded] {
    repos.runs.update_status(run_id, late, None).await.unwrap();
  }
  let run = repos.runs.get(run_id).await.unwrap().expect("present");
  assert_eq!(run.status, "cancelled");

  let session_id = Uuid::new_v4();
  repos
    .harness_sessions
    .create(NewHarnessSession {
      id: session_id,
      tenant_id: format!("tenant-sticky-{}", Uuid::new_v4()),
      user_input: "hi".into(),
      workspace_root: "/tmp/ws".into(),
      profile: "local".into(),
      runtime_kind: "react".into(),
      model: "mock".into(),
      skill_name: None,
    })
    .await
    .unwrap();
  repos
    .harness_sessions
    .update_status(
      session_id,
      HarnessSessionStatus::Cancelled,
      None,
      Some("cancel requested"),
    )
    .await
    .unwrap();
  repos
    .harness_sessions
    .update_status(
      session_id,
      HarnessSessionStatus::Completed,
      Some("late answer"),
      None,
    )
    .await
    .unwrap();
  let session = repos
    .harness_sessions
    .get(session_id)
    .await
    .unwrap()
    .expect("present");
  assert_eq!(session.status, "cancelled");
  assert_eq!(session.final_answer, None);

  // A missing row is still an error, not a silent no-op.
  assert!(
    repos
      .harness_sessions
      .update_status(Uuid::new_v4(), HarnessSessionStatus::Completed, None, None)
      .await
      .is_err()
  );
}

#[tokio::test]
async fn harness_event_repo_append_list_max_seq() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping harness_event_repo_append_list_max_seq");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let session_id = Uuid::new_v4();
  repos
    .harness_sessions
    .create(NewHarnessSession {
      id: session_id,
      tenant_id: "tenant-harness-resume".into(),
      user_input: "go".into(),
      workspace_root: "/tmp".into(),
      profile: "local".into(),
      runtime_kind: "react".into(),
      model: "mock".into(),
      skill_name: None,
    })
    .await
    .unwrap();

  assert!(
    repos
      .harness_events
      .max_seq(session_id)
      .await
      .unwrap()
      .is_none(),
    "max_seq on an empty session must return None"
  );

  for seq in 0..3 {
    repos
      .harness_events
      .append(NewHarnessSessionEvent {
        session_id,
        seq,
        kind: "step_started".into(),
        payload: json!({"seq": seq}),
      })
      .await
      .unwrap();
  }

  assert_eq!(
    repos.harness_events.max_seq(session_id).await.unwrap(),
    Some(2)
  );
  let events_after_zero = repos
    .harness_events
    .list_after("tenant-harness-resume", session_id, 0, 100)
    .await
    .unwrap();
  assert_eq!(events_after_zero.len(), 2);
  assert_eq!(events_after_zero[0].seq, 1);
}

#[tokio::test]
async fn harness_session_reset_for_resume_wipes_events() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping harness_session_reset_for_resume_wipes_events");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let session_id = Uuid::new_v4();
  repos
    .harness_sessions
    .create(NewHarnessSession {
      id: session_id,
      tenant_id: "tenant-harness-resume".into(),
      user_input: "v1".into(),
      workspace_root: "/tmp".into(),
      profile: "local".into(),
      runtime_kind: "react".into(),
      model: "mock".into(),
      skill_name: None,
    })
    .await
    .unwrap();
  repos
    .harness_events
    .append(NewHarnessSessionEvent {
      session_id,
      seq: 0,
      kind: "step_started".into(),
      payload: json!({}),
    })
    .await
    .unwrap();
  // Move to completed so reset_for_resume needs to flip the row back.
  repos
    .harness_sessions
    .update_status(session_id, HarnessSessionStatus::Completed, Some("x"), None)
    .await
    .unwrap();

  repos
    .harness_sessions
    .reset_for_resume(session_id, "v2")
    .await
    .unwrap();

  let after = repos
    .harness_sessions
    .get(session_id)
    .await
    .unwrap()
    .expect("present");
  assert_eq!(after.status, "running");
  assert_eq!(after.user_input, "v2");
  assert!(after.final_answer.is_none());
  let events = repos
    .harness_events
    .list_after("tenant-harness-resume", session_id, -1, 100)
    .await
    .unwrap();
  assert!(
    events.is_empty(),
    "rerun resume must wipe prior events; got {} rows",
    events.len()
  );
}

#[tokio::test]
async fn harness_session_reset_for_append_resume_keeps_events() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping harness_session_reset_for_append_resume_keeps_events");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());

  let session_id = Uuid::new_v4();
  repos
    .harness_sessions
    .create(NewHarnessSession {
      id: session_id,
      tenant_id: "tenant-harness-resume".into(),
      user_input: "v1".into(),
      workspace_root: "/tmp".into(),
      profile: "local".into(),
      runtime_kind: "react".into(),
      model: "mock".into(),
      skill_name: None,
    })
    .await
    .unwrap();
  repos
    .harness_events
    .append(NewHarnessSessionEvent {
      session_id,
      seq: 0,
      kind: "step_started".into(),
      payload: json!({}),
    })
    .await
    .unwrap();
  repos
    .harness_sessions
    .update_status(session_id, HarnessSessionStatus::Completed, Some("x"), None)
    .await
    .unwrap();

  repos
    .harness_sessions
    .reset_for_append_resume(session_id, "v2")
    .await
    .unwrap();

  let after = repos
    .harness_sessions
    .get(session_id)
    .await
    .unwrap()
    .expect("present");
  assert_eq!(after.status, "running");
  assert_eq!(after.user_input, "v2");
  // Append-resume preserves prior events for chronology.
  let events = repos
    .harness_events
    .list_after("tenant-harness-resume", session_id, -1, 100)
    .await
    .unwrap();
  assert_eq!(events.len(), 1, "append-resume must keep prior events");
}

/// Eval 2026-10-07 P5: a run whose replica died stays `running` and held
/// the tenant's concurrency slot forever. Runs now carry a lease the
/// owning replica renews; an expired lease is reclaimed as `failed`.
#[tokio::test]
async fn expired_run_lease_is_reclaimed_and_frees_the_tenant_slot() {
  let Some(db) = fresh_db().await else {
    eprintln!("skipping expired_run_lease_is_reclaimed_and_frees_the_tenant_slot");
    return;
  };
  let repos = Repositories::from_pool(db.pool.clone());
  let tenant = format!("tenant-lease-{}", Uuid::new_v4());

  let AdmissionOutcome::Admitted(orphan) = repos
    .runs
    .create_if_admitted(new_run_for(&tenant), 1, 100, 60)
    .await
    .unwrap()
  else {
    panic!("first run must be admitted");
  };
  let AdmissionOutcome::Admitted(alive) = repos
    .runs
    .create_if_admitted(new_run_for(&format!("{tenant}-other")), 1, 100, 60)
    .await
    .unwrap()
  else {
    panic!("other tenant's run must be admitted");
  };
  // The orphan's replica stopped heartbeating; the other run's didn't.
  sqlx::query("UPDATE runs SET lease_expires_at = NOW() - INTERVAL '1 second' WHERE id = ANY($1)")
    .bind(vec![orphan.id, alive.id])
    .execute(&db.pool)
    .await
    .unwrap();
  repos.runs.renew_leases(&[alive.id]).await.unwrap();

  assert!(matches!(
    repos
      .runs
      .create_if_admitted(new_run_for(&tenant), 1, 100, 60)
      .await
      .unwrap(),
    AdmissionOutcome::ConcurrencyLimitExceeded { .. }
  ));

  let reclaimed: Vec<Uuid> = repos
    .runs
    .reclaim_expired_leases()
    .await
    .unwrap()
    .into_iter()
    .map(|run| run.id)
    .collect();
  assert!(reclaimed.contains(&orphan.id), "{reclaimed:?}");
  assert!(
    !reclaimed.contains(&alive.id),
    "a renewed lease must survive"
  );
  let orphan_row = repos.runs.get(orphan.id).await.unwrap().unwrap();
  assert_eq!(orphan_row.status, RunStatus::Failed.as_str());
  assert_eq!(
    repos.runs.get(alive.id).await.unwrap().unwrap().status,
    RunStatus::Queued.as_str()
  );

  assert!(matches!(
    repos
      .runs
      .create_if_admitted(new_run_for(&tenant), 1, 100, 60)
      .await
      .unwrap(),
    AdmissionOutcome::Admitted(_)
  ));
}
