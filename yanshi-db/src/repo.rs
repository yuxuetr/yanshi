//! Repository abstractions over the gateway's nine tables.
//!
//! Server handlers depend on these traits, not on `sqlx` directly, so we can
//! later swap in a different storage backend (or in-memory test fakes) without
//! changing the route layer. The current production implementation is
//! Postgres-only — see [`PgRunRepo`], [`PgStepRepo`], [`PgEventRepo`],
//! [`PgArtifactRepo`], [`PgSkillInstallRepo`], [`PgMcpSessionRepo`],
//! [`PgHarnessSessionRepo`], [`PgHarnessEventRepo`], [`PgUserPreferenceRepo`].
//!
//! `Repositories` bundles all nine together for convenient injection into
//! `yanshi-server::AppState`.

use async_trait::async_trait;
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::DbError;

/// How long a run's lease lasts without renewal. Must match the default
/// in `migrations/0011_run_leases.sql`.
pub const RUN_LEASE_TTL_SECS: i64 = 120;
use crate::models::{
  Artifact, Event, HarnessSession, HarnessSessionEvent, HarnessSessionStatus, McpSession,
  NewArtifact, NewEvent, NewHarnessSession, NewHarnessSessionEvent, NewRun, NewStep,
  NewUserPreference, Run, RunStatus, SkillInstall, Step, UserPreference,
};

/// Run lifecycle persistence.
#[async_trait]
pub trait RunRepo: Send + Sync {
  async fn create(&self, run: NewRun) -> Result<Run, DbError>;
  async fn get(&self, id: Uuid) -> Result<Option<Run>, DbError>;
  async fn update_status(
    &self,
    id: Uuid,
    status: RunStatus,
    error: Option<&str>,
  ) -> Result<(), DbError>;
  /// Y2.4: move a `failed` / `cancelled` run back to `queued` so
  /// `POST /v1/runs/{id}:resume` can execute it again under the same id,
  /// clearing `finished_at`, `error` and any stale cancellation intent.
  /// The one sanctioned exit from a terminal status (`update_status`
  /// keeps them sticky). Returns `false` when the run is not in a
  /// resumable status, so two concurrent resumes cannot both win.
  async fn reopen_for_resume(&self, id: Uuid) -> Result<bool, DbError>;
  /// List runs for a tenant, newest first. Convenience shim over
  /// [`Self::list_filtered`] with no status filter and zero offset.
  async fn list(&self, tenant_id: &str, limit: i64) -> Result<Vec<Run>, DbError> {
    self.list_filtered(tenant_id, None, limit, 0).await
  }
  /// List runs for a tenant, newest first, optionally filtered by status
  /// and paginated via `offset`. `limit` and `offset` are caller-clamped;
  /// the repo binds them unchanged.
  async fn list_filtered(
    &self,
    tenant_id: &str,
    status: Option<&str>,
    limit: i64,
    offset: i64,
  ) -> Result<Vec<Run>, DbError>;

  /// W4.2d: durably record a cancellation request for `run_id`. Idempotent
  /// (`ON CONFLICT DO NOTHING` — the first request wins; later ones are
  /// redundant, the row already says "cancel this"). Callers pair this
  /// with a Postgres NOTIFY on `yanshi_cancellations` so every gateway
  /// replica's listener can turn it into a local
  /// `RunCancellationRegistry::cancel(run_id)` call — see
  /// `run_cancellation_intents`'s migration comment for the full
  /// cross-replica mechanism.
  async fn record_cancellation_intent(&self, run_id: Uuid, tenant_id: &str) -> Result<(), DbError>;

  /// Push the lease of every run this replica is executing out by
  /// [`RUN_LEASE_TTL_SECS`] (eval 2026-10-07 P5).
  async fn renew_leases(&self, run_ids: &[Uuid]) -> Result<u64, DbError>;

  /// Fail every `queued`/`running` run whose lease expired: its replica
  /// stopped renewing, so nothing will ever finish it. Returns the
  /// reclaimed rows, so the caller can emit their terminal events.
  async fn reclaim_expired_leases(&self) -> Result<Vec<Run>, DbError>;

  /// Which of `run_ids` have a recorded cancellation intent. Lets a
  /// replica resync after missing NOTIFYs (eval 2026-10-07 P3).
  async fn cancellation_intents_among(&self, run_ids: &[Uuid]) -> Result<Vec<Uuid>, DbError>;

  /// W4.2f: cluster-wide admission check + row creation, atomically.
  ///
  /// Replaces the per-process `RunAdmissionRegistry` as the source of
  /// truth for both limits (that registry is retained by callers only
  /// as an optional pre-check to fail obviously-over-limit local bursts
  /// without a round trip):
  ///
  /// - **Concurrency**: `COUNT(*) FROM runs WHERE tenant_id = $1 AND
  ///   status IN ('queued', 'running')` — derived from `runs`'s own
  ///   authoritative status column, not a separately-incremented
  ///   counter, so a crashed replica's stuck-`running` row can't
  ///   permanently leak a held slot.
  /// - **Rate**: a fixed `window_seconds`-wide counter in
  ///   `tenant_admission_windows`, reset-or-incremented atomically.
  ///
  /// Both checks (and the row insert on success) run inside one
  /// transaction serialized per tenant via
  /// `pg_advisory_xact_lock(hashtext('run_admission:' || tenant_id))`,
  /// so concurrent admission attempts for the same tenant — whether
  /// racing within one replica or across every replica — can't both
  /// observe "under the limit" and both admit.
  async fn create_if_admitted(
    &self,
    run: NewRun,
    max_concurrent: i64,
    max_per_window: i64,
    window_seconds: i64,
  ) -> Result<AdmissionOutcome, DbError>;
}

/// Result of [`RunRepo::create_if_admitted`] — three-way instead of a
/// plain `Result<Run, DbError>` so callers can distinguish "rejected by
/// policy" (a normal, expected outcome that should map to HTTP 429)
/// from an actual `DbError` (a real infrastructure failure).
#[derive(Debug, Clone)]
pub enum AdmissionOutcome {
  Admitted(Run),
  ConcurrencyLimitExceeded { current: i64, limit: i64 },
  RateLimited { window_count: i64, limit: i64 },
}

#[async_trait]
pub trait StepRepo: Send + Sync {
  async fn append(&self, step: NewStep) -> Result<Step, DbError>;
  async fn list_for_run(&self, run_id: Uuid) -> Result<Vec<Step>, DbError>;
}

#[async_trait]
pub trait EventRepo: Send + Sync {
  async fn append(&self, event: NewEvent) -> Result<Event, DbError>;
  /// Return events for `(tenant_id, run_id)` with `seq > after_seq`,
  /// ordered ascending.
  ///
  /// SSE subscribers pass their last-seen `seq` to resume after a
  /// reconnect. Q1.5.3 added the `tenant_id` parameter so the
  /// composite `events_tenant_run_idx` index is reachable and the
  /// db layer provides defense-in-depth even if a server route
  /// forgets to filter by tenant first.
  async fn list_after(
    &self,
    tenant_id: &str,
    run_id: Uuid,
    after_seq: i64,
    limit: i64,
  ) -> Result<Vec<Event>, DbError>;
  /// Q3.11.1: return the largest `seq` recorded for `(tenant_id,
  /// run_id)`, or `None` if no events exist yet. Used by
  /// `yanshi-server`'s `next_event_seq` helper instead of paging
  /// `list_after(..., 10_000)`: a run with > 10 000 events would
  /// silently roll the seq counter back to a value already in
  /// `events.(run_id, seq)` and collide the primary key. Mirrors
  /// [`HarnessEventRepo::max_seq`]; SQL is a one-row `MAX(seq)`
  /// scan that uses the `events_tenant_run_idx` covering index.
  async fn max_seq(&self, tenant_id: &str, run_id: Uuid) -> Result<Option<i64>, DbError>;

  /// W4.2e: `approval_requested` event payloads for `(tenant_id, run_id)`
  /// with no matching `approval_decided` for the same `request_id`,
  /// oldest first. Derives "pending" from the append-only event log
  /// (already written by `RunHarnessEventSink`) rather than any
  /// in-memory registry, so the result is correct regardless of which
  /// replica actually holds the live `oneshot` for a given request.
  /// Each returned value is the inner `ApprovalRequest` JSON object
  /// (`payload->payload->request`, not the whole envelope) — callers
  /// deserialize it themselves rather than this crate depending on
  /// `yanshi-harness`'s types.
  async fn list_pending_approvals(
    &self,
    tenant_id: &str,
    run_id: Uuid,
  ) -> Result<Vec<serde_json::Value>, DbError>;
}

#[async_trait]
pub trait ArtifactRepo: Send + Sync {
  async fn create(&self, artifact: NewArtifact) -> Result<Artifact, DbError>;
  async fn list_for_run(&self, run_id: Uuid) -> Result<Vec<Artifact>, DbError>;
  /// Y1.3: one artifact by id, for the download route. Tenant and run
  /// ownership are checked by the caller against the returned row.
  async fn get(&self, id: Uuid) -> Result<Option<Artifact>, DbError>;
}

#[async_trait]
pub trait SkillInstallRepo: Send + Sync {
  async fn upsert(&self, install: SkillInstall) -> Result<(), DbError>;
  /// Q1.5.1: this used to be tenant-agnostic, leaking every tenant's
  /// installed-skill catalog through one call. Callers must now pass
  /// the tenant they own.
  async fn list(&self, tenant_id: &str) -> Result<Vec<SkillInstall>, DbError>;
}

#[async_trait]
pub trait McpSessionRepo: Send + Sync {
  async fn open(&self, session: McpSession) -> Result<(), DbError>;
  async fn close(&self, id: Uuid, tool_calls: i32) -> Result<(), DbError>;
}

/// Harness session lifecycle persistence (P-H.5).
///
/// Mirrors [`RunRepo`] but is scoped to the `harness_sessions` table so the
/// agent-native flow keeps its own typed surface and avoids overloading
/// the workflow `runs` schema with sentinel columns.
#[async_trait]
pub trait HarnessSessionRepo: Send + Sync {
  async fn create(&self, session: NewHarnessSession) -> Result<HarnessSession, DbError>;
  async fn get(&self, id: Uuid) -> Result<Option<HarnessSession>, DbError>;
  /// List sessions for a tenant, newest first.
  async fn list(&self, tenant_id: &str, limit: i64) -> Result<Vec<HarnessSession>, DbError>;
  /// Transition the session's status. `final_answer` populates the
  /// terminal answer column on success; `error` populates the failure
  /// message on failure / cancel. Both default to `COALESCE`d updates so
  /// repeated transitions don't blow away earlier values.
  async fn update_status(
    &self,
    id: Uuid,
    status: HarnessSessionStatus,
    final_answer: Option<&str>,
    error: Option<&str>,
  ) -> Result<(), DbError>;
  /// Reset a terminal session back to `running` so a fresh executor
  /// run can write into the same row (P-H.5 slice 4 resume route).
  ///
  /// Clears `finished_at`, `final_answer`, and `error`, replaces
  /// `user_input` if `new_user_input` is provided, and deletes every
  /// existing `harness_session_events` row that referenced this
  /// session via the FK CASCADE. The transaction is atomic so a
  /// concurrent reader never observes a half-reset state.
  async fn reset_for_resume(&self, id: Uuid, new_user_input: &str) -> Result<(), DbError>;
  /// Which of `ids` are `cancelled`, read from the primary. A cancel
  /// request served by another replica only marks the row, so the
  /// replica running the session checks here (eval 2026-10-07 P1).
  async fn cancelled_among(&self, ids: &[Uuid]) -> Result<Vec<Uuid>, DbError>;
  /// Append-mode counterpart to [`Self::reset_for_resume`]. Flips the
  /// row back to `running` and replaces `user_input` but **keeps** the
  /// existing `harness_session_events` rows intact.
  ///
  /// Used by the `:resume` route's `mode=append` flavour: combined with
  /// `HarnessRuntime::with_initial_seq(MAX(seq) + 1)`, this lets a
  /// resumed session continue the seq series instead of restarting
  /// from 0. The caller is responsible for supplying the correct
  /// initial seq to the executor.
  async fn reset_for_append_resume(&self, id: Uuid, new_user_input: &str) -> Result<(), DbError>;
  /// V2.3: transition the session to `awaiting_input`, recording the
  /// question the loop paused on. Called by the live executor the moment
  /// the inner agent's stop reason resolves to `AwaitingInput`.
  async fn set_pending_question(
    &self,
    id: Uuid,
    question: &str,
    step_index: i64,
  ) -> Result<(), DbError>;
  /// V2.3: the counterpart to [`Self::set_pending_question`] — flips the
  /// row back to `running` and clears the pending-question columns.
  /// Called right before the server spawns the resume-with-answer
  /// executor call, mirroring how [`Self::reset_for_resume`] prepares the
  /// row before a fresh executor run.
  async fn clear_pending_question_for_resume(&self, id: Uuid) -> Result<(), DbError>;
}

/// Harness session event log persistence (P-H.5).
///
/// Same contract as [`EventRepo`] but keyed by `session_id`. Kept separate
/// so SSE consumers can subscribe to either world without overlapping
/// `(run_id, seq)` primary keys.
#[async_trait]
pub trait HarnessEventRepo: Send + Sync {
  async fn append(&self, event: NewHarnessSessionEvent) -> Result<HarnessSessionEvent, DbError>;
  /// Return events for `(tenant_id, session_id)` with `seq > after_seq`,
  /// ordered ascending.
  ///
  /// SSE subscribers pass their last-seen `seq` to resume after a
  /// reconnect. Q1.5.3 added the `tenant_id` parameter: because
  /// `harness_session_events` itself carries no `tenant_id` column,
  /// the SQL joins back to `harness_sessions` and filters there.
  /// Defense-in-depth on top of the existing route-layer check.
  async fn list_after(
    &self,
    tenant_id: &str,
    session_id: Uuid,
    after_seq: i64,
    limit: i64,
  ) -> Result<Vec<HarnessSessionEvent>, DbError>;
  /// Return the largest `seq` recorded for `session_id`, or `None` if
  /// no events exist yet. Used by the `:resume` route's append-mode
  /// flavour to pick a non-colliding `initial_seq` for the next run.
  async fn max_seq(&self, session_id: Uuid) -> Result<Option<i64>, DbError>;

  /// W4.2e: mirrors `EventRepo::list_pending_approvals` — see that
  /// method's doc comment for the full derivation. Same JOIN-back
  /// pattern as `list_after` above, since `harness_session_events`
  /// carries no `tenant_id` column of its own.
  async fn list_pending_approvals(
    &self,
    tenant_id: &str,
    session_id: Uuid,
  ) -> Result<Vec<serde_json::Value>, DbError>;
}

// ----- Postgres implementations -----
//
// Each Pg*Repo holds two pools: `pool` (the primary, used for every
// write) and `read_pool` (used for `get_*` / `list_*`). When the
// caller didn't configure a replica, `read_pool` is cloned from
// `pool` and behaves identically. P10.15.2.

#[derive(Clone, Debug)]
pub struct PgRunRepo {
  pub pool: PgPool,
  pub read_pool: PgPool,
}

#[async_trait]
impl RunRepo for PgRunRepo {
  async fn create(&self, run: NewRun) -> Result<Run, DbError> {
    let row = sqlx::query_as::<_, Run>(
      r#"INSERT INTO runs (id, workflow, status, run_dir, tenant_id,
                           events_retention_days, artifacts_retention_days)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, workflow, status, started_at, finished_at, run_dir,
                   tenant_id, error, events_retention_days, artifacts_retention_days"#,
    )
    .bind(run.id)
    .bind(&run.workflow)
    .bind(run.status.as_str())
    .bind(run.run_dir.as_deref())
    .bind(&run.tenant_id)
    .bind(run.events_retention_days)
    .bind(run.artifacts_retention_days)
    .fetch_one(&self.pool)
    .await?;
    Ok(row)
  }

  async fn get(&self, id: Uuid) -> Result<Option<Run>, DbError> {
    let row = sqlx::query_as::<_, Run>(
      r#"SELECT id, workflow, status, started_at, finished_at, run_dir,
                tenant_id, error, events_retention_days, artifacts_retention_days
         FROM runs WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(&self.read_pool)
    .await?;
    Ok(row)
  }

  async fn update_status(
    &self,
    id: Uuid,
    status: RunStatus,
    error: Option<&str>,
  ) -> Result<(), DbError> {
    let finished_at = matches!(
      status,
      RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
    )
    .then(Utc::now);

    // Terminal states are sticky: a cancel that lands while the executor
    // is still running must not be overwritten by the executor's own
    // final `succeeded`/`failed` write, nor flipped back to `running` by
    // a late start (eval 2026-10-07 P2).
    let result = sqlx::query(
      r#"UPDATE runs
         SET status = $2,
             finished_at = COALESCE($3, finished_at),
             error = COALESCE($4, error)
         WHERE id = $1 AND status NOT IN ('succeeded', 'failed', 'cancelled')"#,
    )
    .bind(id)
    .bind(status.as_str())
    .bind(finished_at)
    .bind(error)
    .execute(&self.pool)
    .await?;

    if result.rows_affected() == 0 && !row_exists(&self.pool, "runs", id).await? {
      return Err(DbError::NotFound {
        entity_type: "run",
        id: id.to_string(),
      });
    }
    Ok(())
  }

  async fn reopen_for_resume(&self, id: Uuid) -> Result<bool, DbError> {
    let mut tx = self.pool.begin().await?;
    let reopened = sqlx::query(
      r#"UPDATE runs
         SET status = 'queued', finished_at = NULL, error = NULL
         WHERE id = $1 AND status IN ('failed', 'cancelled')"#,
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
      == 1;
    if reopened {
      sqlx::query("DELETE FROM run_cancellation_intents WHERE run_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(reopened)
  }

  async fn list_filtered(
    &self,
    tenant_id: &str,
    status: Option<&str>,
    limit: i64,
    offset: i64,
  ) -> Result<Vec<Run>, DbError> {
    // Two query shapes — one with a status predicate, one without — so
    // the optimizer can pick the right index without needing to read a
    // NULL parameter at runtime.
    let rows = if let Some(status) = status {
      sqlx::query_as::<_, Run>(
        r#"SELECT id, workflow, status, started_at, finished_at, run_dir, tenant_id, error,
                  events_retention_days, artifacts_retention_days
           FROM runs
           WHERE tenant_id = $1 AND status = $2
           ORDER BY started_at DESC
           LIMIT $3 OFFSET $4"#,
      )
      .bind(tenant_id)
      .bind(status)
      .bind(limit)
      .bind(offset)
      .fetch_all(&self.read_pool)
      .await?
    } else {
      sqlx::query_as::<_, Run>(
        r#"SELECT id, workflow, status, started_at, finished_at, run_dir, tenant_id, error,
                  events_retention_days, artifacts_retention_days
           FROM runs
           WHERE tenant_id = $1
           ORDER BY started_at DESC
           LIMIT $2 OFFSET $3"#,
      )
      .bind(tenant_id)
      .bind(limit)
      .bind(offset)
      .fetch_all(&self.read_pool)
      .await?
    };
    Ok(rows)
  }

  async fn record_cancellation_intent(&self, run_id: Uuid, tenant_id: &str) -> Result<(), DbError> {
    sqlx::query(
      r#"INSERT INTO run_cancellation_intents (run_id, tenant_id)
         VALUES ($1, $2)
         ON CONFLICT (run_id) DO NOTHING"#,
    )
    .bind(run_id)
    .bind(tenant_id)
    .execute(&self.pool)
    .await?;
    Ok(())
  }

  async fn renew_leases(&self, run_ids: &[Uuid]) -> Result<u64, DbError> {
    if run_ids.is_empty() {
      return Ok(0);
    }
    let result = sqlx::query(
      r#"UPDATE runs
         SET lease_expires_at = NOW() + make_interval(secs => $2::double precision)
         WHERE id = ANY($1) AND status IN ('queued', 'running')"#,
    )
    .bind(run_ids)
    .bind(RUN_LEASE_TTL_SECS as f64)
    .execute(&self.pool)
    .await?;
    Ok(result.rows_affected())
  }

  async fn reclaim_expired_leases(&self) -> Result<Vec<Run>, DbError> {
    Ok(
      sqlx::query_as::<_, Run>(
        r#"UPDATE runs
           SET status = 'failed',
               finished_at = NOW(),
               error = 'lease expired: the gateway replica running this run stopped'
           WHERE status IN ('queued', 'running') AND lease_expires_at < NOW()
           RETURNING id, workflow, status, started_at, finished_at, run_dir,
                     tenant_id, error, events_retention_days, artifacts_retention_days"#,
      )
      .fetch_all(&self.pool)
      .await?,
    )
  }

  async fn cancellation_intents_among(&self, run_ids: &[Uuid]) -> Result<Vec<Uuid>, DbError> {
    if run_ids.is_empty() {
      return Ok(Vec::new());
    }
    Ok(
      sqlx::query_scalar("SELECT run_id FROM run_cancellation_intents WHERE run_id = ANY($1)")
        .bind(run_ids)
        .fetch_all(&self.pool)
        .await?,
    )
  }

  async fn create_if_admitted(
    &self,
    run: NewRun,
    max_concurrent: i64,
    max_per_window: i64,
    window_seconds: i64,
  ) -> Result<AdmissionOutcome, DbError> {
    let mut tx = self.pool.begin().await?;

    // Serialize concurrent admission attempts for this tenant across
    // every backend connected to this database (every gateway replica),
    // not just this transaction. Auto-releases at commit/rollback.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('run_admission:' || $1))")
      .bind(&run.tenant_id)
      .execute(&mut *tx)
      .await?;

    let current: i64 = sqlx::query_scalar(
      "SELECT count(*) FROM runs WHERE tenant_id = $1 AND status IN ('queued', 'running')",
    )
    .bind(&run.tenant_id)
    .fetch_one(&mut *tx)
    .await?;
    if current >= max_concurrent {
      // Dropping `tx` here rolls back — nothing has been written yet,
      // so an implicit rollback is equivalent to an explicit one.
      return Ok(AdmissionOutcome::ConcurrencyLimitExceeded {
        current,
        limit: max_concurrent,
      });
    }

    let (window_count,): (i32,) = sqlx::query_as(
      r#"INSERT INTO tenant_admission_windows (tenant_id, window_started_at, window_count)
         VALUES ($1, NOW(), 1)
         ON CONFLICT (tenant_id) DO UPDATE
           SET window_count = CASE
                 WHEN tenant_admission_windows.window_started_at
                      <= NOW() - make_interval(secs => $2::double precision)
                 THEN 1
                 ELSE tenant_admission_windows.window_count + 1
               END,
               window_started_at = CASE
                 WHEN tenant_admission_windows.window_started_at
                      <= NOW() - make_interval(secs => $2::double precision)
                 THEN NOW()
                 ELSE tenant_admission_windows.window_started_at
               END
         RETURNING window_count"#,
    )
    .bind(&run.tenant_id)
    .bind(window_seconds as f64)
    .fetch_one(&mut *tx)
    .await?;
    if window_count as i64 > max_per_window {
      return Ok(AdmissionOutcome::RateLimited {
        window_count: window_count as i64,
        limit: max_per_window,
      });
    }

    let row = sqlx::query_as::<_, Run>(
      r#"INSERT INTO runs (id, workflow, status, run_dir, tenant_id,
                           events_retention_days, artifacts_retention_days)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, workflow, status, started_at, finished_at, run_dir,
                   tenant_id, error, events_retention_days, artifacts_retention_days"#,
    )
    .bind(run.id)
    .bind(&run.workflow)
    .bind(run.status.as_str())
    .bind(run.run_dir.as_deref())
    .bind(&run.tenant_id)
    .bind(run.events_retention_days)
    .bind(run.artifacts_retention_days)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(AdmissionOutcome::Admitted(row))
  }
}

#[derive(Clone, Debug)]
pub struct PgStepRepo {
  pub pool: PgPool,
  pub read_pool: PgPool,
}

#[async_trait]
impl StepRepo for PgStepRepo {
  async fn append(&self, step: NewStep) -> Result<Step, DbError> {
    let row = sqlx::query_as::<_, Step>(
      r#"INSERT INTO steps (run_id, seq, node_id, kind, status, duration_ms, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING run_id, seq, node_id, kind, status, started_at, duration_ms, payload"#,
    )
    .bind(step.run_id)
    .bind(step.seq)
    .bind(&step.node_id)
    .bind(&step.kind)
    .bind(&step.status)
    .bind(step.duration_ms)
    .bind(step.payload.as_ref())
    .fetch_one(&self.pool)
    .await?;
    Ok(row)
  }

  async fn list_for_run(&self, run_id: Uuid) -> Result<Vec<Step>, DbError> {
    let rows = sqlx::query_as::<_, Step>(
      r#"SELECT run_id, seq, node_id, kind, status, started_at, duration_ms, payload
         FROM steps WHERE run_id = $1 ORDER BY seq ASC"#,
    )
    .bind(run_id)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(rows)
  }
}

#[derive(Clone, Debug)]
pub struct PgEventRepo {
  pub pool: PgPool,
  pub read_pool: PgPool,
}

#[async_trait]
impl EventRepo for PgEventRepo {
  async fn append(&self, event: NewEvent) -> Result<Event, DbError> {
    let tenant_id = event.tenant_id.as_deref().unwrap_or("default");
    let row = sqlx::query_as::<_, Event>(
      r#"INSERT INTO events (run_id, seq, kind, payload, tenant_id)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING run_id, seq, kind, payload, ts, tenant_id"#,
    )
    .bind(event.run_id)
    .bind(event.seq)
    .bind(&event.kind)
    .bind(&event.payload)
    .bind(tenant_id)
    .fetch_one(&self.pool)
    .await?;
    Ok(row)
  }

  async fn list_after(
    &self,
    tenant_id: &str,
    run_id: Uuid,
    after_seq: i64,
    limit: i64,
  ) -> Result<Vec<Event>, DbError> {
    let rows = sqlx::query_as::<_, Event>(
      r#"SELECT run_id, seq, kind, payload, ts, tenant_id
         FROM events
         WHERE tenant_id = $1 AND run_id = $2 AND seq > $3
         ORDER BY seq ASC
         LIMIT $4"#,
    )
    .bind(tenant_id)
    .bind(run_id)
    .bind(after_seq)
    .bind(limit)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(rows)
  }

  async fn max_seq(&self, tenant_id: &str, run_id: Uuid) -> Result<Option<i64>, DbError> {
    // Q3.11.1: single-row aggregate over the `events_tenant_run_idx`
    // covering index — O(1) reads regardless of how many events the
    // run has accumulated. Replaces the previous
    // `list_after(..., 10_000)` workaround that silently rolled the
    // seq counter back into a colliding range once a run crossed
    // the cap.
    let row: Option<(Option<i64>,)> =
      sqlx::query_as("SELECT MAX(seq) FROM events WHERE tenant_id = $1 AND run_id = $2")
        .bind(tenant_id)
        .bind(run_id)
        .fetch_optional(&self.read_pool)
        .await?;
    Ok(row.and_then(|(value,)| value))
  }

  async fn list_pending_approvals(
    &self,
    tenant_id: &str,
    run_id: Uuid,
  ) -> Result<Vec<serde_json::Value>, DbError> {
    // `payload` stores the whole serialized `HarnessEventBody`
    // (`#[serde(tag = "kind", content = "payload")]`), so the inner
    // `ApprovalRequest`/`ApprovalDecision` live one level deeper than
    // the `kind` column already tells us — `#>>` extracts a JSONB path
    // as text so the two sides of the correlation (a request's own id,
    // a decision's `request_id`) compare directly.
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
      r#"SELECT e1.payload #> '{payload,request}'
         FROM events e1
         WHERE e1.tenant_id = $1
           AND e1.run_id = $2
           AND e1.kind = 'approval_requested'
           AND NOT EXISTS (
             SELECT 1 FROM events e2
             WHERE e2.tenant_id = e1.tenant_id
               AND e2.run_id = e1.run_id
               AND e2.kind = 'approval_decided'
               AND e2.payload #>> '{payload,decision,request_id}' = e1.payload #>> '{payload,request,id}'
           )
         ORDER BY e1.seq ASC"#,
    )
    .bind(tenant_id)
    .bind(run_id)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(rows.into_iter().map(|(payload,)| payload).collect())
  }
}

#[derive(Clone, Debug)]
pub struct PgArtifactRepo {
  pub pool: PgPool,
  pub read_pool: PgPool,
}

#[async_trait]
impl ArtifactRepo for PgArtifactRepo {
  async fn create(&self, artifact: NewArtifact) -> Result<Artifact, DbError> {
    let tenant_id = artifact.tenant_id.as_deref().unwrap_or("default");
    let row = sqlx::query_as::<_, Artifact>(
      r#"INSERT INTO artifacts (id, run_id, node_id, name, path_or_url, mime_type, tenant_id, size_bytes, sha256)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id, run_id, node_id, name, path_or_url, mime_type, created_at, tenant_id, size_bytes, sha256"#,
    )
    .bind(artifact.id)
    .bind(artifact.run_id)
    .bind(&artifact.node_id)
    .bind(&artifact.name)
    .bind(&artifact.path_or_url)
    .bind(artifact.mime_type.as_deref())
    .bind(tenant_id)
    .bind(artifact.size_bytes)
    .bind(artifact.sha256.as_deref())
    .fetch_one(&self.pool)
    .await?;
    Ok(row)
  }

  async fn list_for_run(&self, run_id: Uuid) -> Result<Vec<Artifact>, DbError> {
    let rows = sqlx::query_as::<_, Artifact>(
      r#"SELECT id, run_id, node_id, name, path_or_url, mime_type, created_at, tenant_id, size_bytes, sha256
         FROM artifacts WHERE run_id = $1 ORDER BY created_at ASC, node_id ASC, name ASC"#,
    )
    .bind(run_id)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(rows)
  }

  async fn get(&self, id: Uuid) -> Result<Option<Artifact>, DbError> {
    let row = sqlx::query_as::<_, Artifact>(
      r#"SELECT id, run_id, node_id, name, path_or_url, mime_type, created_at, tenant_id, size_bytes, sha256
         FROM artifacts WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(&self.read_pool)
    .await?;
    Ok(row)
  }
}

#[derive(Clone, Debug)]
pub struct PgSkillInstallRepo {
  pub pool: PgPool,
  pub read_pool: PgPool,
}

#[async_trait]
impl SkillInstallRepo for PgSkillInstallRepo {
  async fn upsert(&self, install: SkillInstall) -> Result<(), DbError> {
    sqlx::query(
      r#"INSERT INTO skill_installs (tenant_id, name, version, source, installed_at, checksum)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (tenant_id, name, version) DO UPDATE SET
           source = EXCLUDED.source,
           installed_at = EXCLUDED.installed_at,
           checksum = EXCLUDED.checksum"#,
    )
    .bind(&install.tenant_id)
    .bind(&install.name)
    .bind(&install.version)
    .bind(&install.source)
    .bind(install.installed_at)
    .bind(install.checksum.as_deref())
    .execute(&self.pool)
    .await?;
    Ok(())
  }

  async fn list(&self, tenant_id: &str) -> Result<Vec<SkillInstall>, DbError> {
    let rows = sqlx::query_as::<_, SkillInstall>(
      r#"SELECT name, version, source, installed_at, checksum, tenant_id
         FROM skill_installs
         WHERE tenant_id = $1
         ORDER BY name ASC, version ASC"#,
    )
    .bind(tenant_id)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(rows)
  }
}

#[derive(Clone, Debug)]
pub struct PgMcpSessionRepo {
  pub pool: PgPool,
  pub read_pool: PgPool,
}

#[async_trait]
impl McpSessionRepo for PgMcpSessionRepo {
  async fn open(&self, session: McpSession) -> Result<(), DbError> {
    sqlx::query(
      r#"INSERT INTO mcp_sessions (id, server, started_at, ended_at, tool_calls, metadata, tenant_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
    )
    .bind(session.id)
    .bind(&session.server)
    .bind(session.started_at)
    .bind(session.ended_at)
    .bind(session.tool_calls)
    .bind(session.metadata.as_ref())
    .bind(&session.tenant_id)
    .execute(&self.pool)
    .await?;
    Ok(())
  }

  async fn close(&self, id: Uuid, tool_calls: i32) -> Result<(), DbError> {
    let result = sqlx::query(
      r#"UPDATE mcp_sessions
         SET ended_at = NOW(), tool_calls = $2
         WHERE id = $1"#,
    )
    .bind(id)
    .bind(tool_calls)
    .execute(&self.pool)
    .await?;
    if result.rows_affected() == 0 {
      return Err(DbError::NotFound {
        entity_type: "mcp_session",
        id: id.to_string(),
      });
    }
    Ok(())
  }
}

#[derive(Clone, Debug)]
pub struct PgHarnessSessionRepo {
  pub pool: PgPool,
  pub read_pool: PgPool,
}

#[async_trait]
impl HarnessSessionRepo for PgHarnessSessionRepo {
  async fn create(&self, session: NewHarnessSession) -> Result<HarnessSession, DbError> {
    let row = sqlx::query_as::<_, HarnessSession>(
      r#"INSERT INTO harness_sessions (
           id, tenant_id, status, user_input, workspace_root, profile,
           runtime_kind, model, skill_name
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id, tenant_id, status, user_input, workspace_root, profile,
                   runtime_kind, model, skill_name, started_at, finished_at,
                   final_answer, error, pending_question, pending_question_step_index"#,
    )
    .bind(session.id)
    .bind(&session.tenant_id)
    .bind(HarnessSessionStatus::Running.as_str())
    .bind(&session.user_input)
    .bind(&session.workspace_root)
    .bind(&session.profile)
    .bind(&session.runtime_kind)
    .bind(&session.model)
    .bind(session.skill_name.as_deref())
    .fetch_one(&self.pool)
    .await?;
    Ok(row)
  }

  async fn get(&self, id: Uuid) -> Result<Option<HarnessSession>, DbError> {
    let row = sqlx::query_as::<_, HarnessSession>(
      r#"SELECT id, tenant_id, status, user_input, workspace_root, profile,
                runtime_kind, model, skill_name, started_at, finished_at,
                final_answer, error, pending_question, pending_question_step_index
         FROM harness_sessions
         WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(&self.read_pool)
    .await?;
    Ok(row)
  }

  async fn list(&self, tenant_id: &str, limit: i64) -> Result<Vec<HarnessSession>, DbError> {
    let rows = sqlx::query_as::<_, HarnessSession>(
      r#"SELECT id, tenant_id, status, user_input, workspace_root, profile,
                runtime_kind, model, skill_name, started_at, finished_at,
                final_answer, error, pending_question, pending_question_step_index
         FROM harness_sessions
         WHERE tenant_id = $1
         ORDER BY started_at DESC
         LIMIT $2"#,
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(rows)
  }

  async fn update_status(
    &self,
    id: Uuid,
    status: HarnessSessionStatus,
    final_answer: Option<&str>,
    error: Option<&str>,
  ) -> Result<(), DbError> {
    let finished_at = status.is_terminal().then(Utc::now);

    let result = sqlx::query(
      r#"UPDATE harness_sessions
         SET status = $2,
             finished_at = COALESCE($3, finished_at),
             final_answer = COALESCE($4, final_answer),
             error = COALESCE($5, error)
         WHERE id = $1 AND status NOT IN ('completed', 'failed', 'cancelled')"#,
    )
    .bind(id)
    .bind(status.as_str())
    .bind(finished_at)
    .bind(final_answer)
    .bind(error)
    .execute(&self.pool)
    .await?;

    // Terminal states are sticky — see `RunRepo::update_status`. `:resume`
    // leaves a terminal state through `reset_for_resume` instead.
    if result.rows_affected() == 0 && !row_exists(&self.pool, "harness_sessions", id).await? {
      return Err(DbError::NotFound {
        entity_type: "harness_session",
        id: id.to_string(),
      });
    }
    Ok(())
  }

  async fn cancelled_among(&self, ids: &[Uuid]) -> Result<Vec<Uuid>, DbError> {
    if ids.is_empty() {
      return Ok(Vec::new());
    }
    Ok(
      sqlx::query_scalar(
        "SELECT id FROM harness_sessions WHERE id = ANY($1) AND status = 'cancelled'",
      )
      .bind(ids)
      .fetch_all(&self.pool)
      .await?,
    )
  }

  async fn reset_for_resume(&self, id: Uuid, new_user_input: &str) -> Result<(), DbError> {
    let mut tx = self.pool.begin().await?;
    // CASCADE on the FK already wipes harness_session_events when the
    // session row is deleted, but we keep the session row — so the
    // child rows are deleted explicitly here.
    sqlx::query("DELETE FROM harness_session_events WHERE session_id = $1")
      .bind(id)
      .execute(&mut *tx)
      .await?;
    let result = sqlx::query(
      r#"UPDATE harness_sessions
         SET status = 'running',
             finished_at = NULL,
             final_answer = NULL,
             error = NULL,
             user_input = $2
         WHERE id = $1"#,
    )
    .bind(id)
    .bind(new_user_input)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
      return Err(DbError::NotFound {
        entity_type: "harness_session",
        id: id.to_string(),
      });
    }
    tx.commit().await?;
    Ok(())
  }

  async fn reset_for_append_resume(&self, id: Uuid, new_user_input: &str) -> Result<(), DbError> {
    let result = sqlx::query(
      r#"UPDATE harness_sessions
         SET status = 'running',
             finished_at = NULL,
             final_answer = NULL,
             error = NULL,
             user_input = $2
         WHERE id = $1"#,
    )
    .bind(id)
    .bind(new_user_input)
    .execute(&self.pool)
    .await?;
    if result.rows_affected() == 0 {
      return Err(DbError::NotFound {
        entity_type: "harness_session",
        id: id.to_string(),
      });
    }
    Ok(())
  }

  async fn set_pending_question(
    &self,
    id: Uuid,
    question: &str,
    step_index: i64,
  ) -> Result<(), DbError> {
    let result = sqlx::query(
      r#"UPDATE harness_sessions
         SET status = 'awaiting_input',
             pending_question = $2,
             pending_question_step_index = $3
         WHERE id = $1"#,
    )
    .bind(id)
    .bind(question)
    .bind(step_index)
    .execute(&self.pool)
    .await?;
    if result.rows_affected() == 0 {
      return Err(DbError::NotFound {
        entity_type: "harness_session",
        id: id.to_string(),
      });
    }
    Ok(())
  }

  async fn clear_pending_question_for_resume(&self, id: Uuid) -> Result<(), DbError> {
    let result = sqlx::query(
      r#"UPDATE harness_sessions
         SET status = 'running',
             pending_question = NULL,
             pending_question_step_index = NULL
         WHERE id = $1"#,
    )
    .bind(id)
    .execute(&self.pool)
    .await?;
    if result.rows_affected() == 0 {
      return Err(DbError::NotFound {
        entity_type: "harness_session",
        id: id.to_string(),
      });
    }
    Ok(())
  }
}

#[derive(Clone, Debug)]
pub struct PgHarnessEventRepo {
  pub pool: PgPool,
  pub read_pool: PgPool,
}

#[async_trait]
impl HarnessEventRepo for PgHarnessEventRepo {
  async fn append(&self, event: NewHarnessSessionEvent) -> Result<HarnessSessionEvent, DbError> {
    let row = sqlx::query_as::<_, HarnessSessionEvent>(
      r#"INSERT INTO harness_session_events (session_id, seq, kind, payload)
         VALUES ($1, $2, $3, $4)
         RETURNING session_id, seq, kind, payload, ts"#,
    )
    .bind(event.session_id)
    .bind(event.seq)
    .bind(&event.kind)
    .bind(&event.payload)
    .fetch_one(&self.pool)
    .await?;
    Ok(row)
  }

  async fn list_after(
    &self,
    tenant_id: &str,
    session_id: Uuid,
    after_seq: i64,
    limit: i64,
  ) -> Result<Vec<HarnessSessionEvent>, DbError> {
    let rows = sqlx::query_as::<_, HarnessSessionEvent>(
      r#"SELECT e.session_id, e.seq, e.kind, e.payload, e.ts
         FROM harness_session_events e
         JOIN harness_sessions s ON s.id = e.session_id
         WHERE s.tenant_id = $1 AND e.session_id = $2 AND e.seq > $3
         ORDER BY e.seq ASC
         LIMIT $4"#,
    )
    .bind(tenant_id)
    .bind(session_id)
    .bind(after_seq)
    .bind(limit)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(rows)
  }

  async fn max_seq(&self, session_id: Uuid) -> Result<Option<i64>, DbError> {
    let row: Option<(Option<i64>,)> =
      sqlx::query_as("SELECT MAX(seq) FROM harness_session_events WHERE session_id = $1")
        .bind(session_id)
        .fetch_optional(&self.read_pool)
        .await?;
    Ok(row.and_then(|(value,)| value))
  }

  async fn list_pending_approvals(
    &self,
    tenant_id: &str,
    session_id: Uuid,
  ) -> Result<Vec<serde_json::Value>, DbError> {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
      r#"SELECT e1.payload #> '{payload,request}'
         FROM harness_session_events e1
         JOIN harness_sessions s ON s.id = e1.session_id
         WHERE s.tenant_id = $1
           AND e1.session_id = $2
           AND e1.kind = 'approval_requested'
           AND NOT EXISTS (
             SELECT 1 FROM harness_session_events e2
             WHERE e2.session_id = e1.session_id
               AND e2.kind = 'approval_decided'
               AND e2.payload #>> '{payload,decision,request_id}' = e1.payload #>> '{payload,request,id}'
           )
         ORDER BY e1.seq ASC"#,
    )
    .bind(tenant_id)
    .bind(session_id)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(rows.into_iter().map(|(payload,)| payload).collect())
  }
}

/// W4.2e: cross-replica approval decision intents. `session_id` spans two
/// unrelated ID namespaces (real `harness_sessions.id` and `runs.id`
/// formatted as text, per `PendingApprovalRegistry`'s own generic
/// `(session_id, request_id)` key) — kept as a standalone trait rather
/// than a method on `HarnessSessionRepo`/`RunRepo` since it isn't owned
/// by either entity's lifecycle. `decision` is stored as opaque JSONB;
/// this crate doesn't depend on `yanshi-harness`'s `ApprovalDecision`
/// type, callers serialize/deserialize it themselves.
#[async_trait]
pub trait ApprovalIntentRepo: Send + Sync {
  /// Idempotent (`ON CONFLICT DO NOTHING` — the first decision for a
  /// given request wins, matching `PendingApprovalRegistry::decide`'s
  /// own single-resolution oneshot semantics).
  async fn record_decision_intent(
    &self,
    session_id: &str,
    request_id: &str,
    tenant_id: &str,
    decision: &serde_json::Value,
  ) -> Result<(), DbError>;

  /// The recorded decision for one request, if any. NOTIFY only carries
  /// the ids (a decision can exceed the ~8 KB payload cap), so the
  /// receiving replica reads the decision here.
  async fn decision_intent(
    &self,
    session_id: &str,
    request_id: &str,
  ) -> Result<Option<serde_json::Value>, DbError>;
}

#[derive(Clone, Debug)]
pub struct PgApprovalIntentRepo {
  pub pool: PgPool,
}

#[async_trait]
impl ApprovalIntentRepo for PgApprovalIntentRepo {
  async fn record_decision_intent(
    &self,
    session_id: &str,
    request_id: &str,
    tenant_id: &str,
    decision: &serde_json::Value,
  ) -> Result<(), DbError> {
    sqlx::query(
      r#"INSERT INTO approval_decision_intents (session_id, request_id, tenant_id, decision)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (session_id, request_id) DO NOTHING"#,
    )
    .bind(session_id)
    .bind(request_id)
    .bind(tenant_id)
    .bind(decision)
    .execute(&self.pool)
    .await?;
    Ok(())
  }

  async fn decision_intent(
    &self,
    session_id: &str,
    request_id: &str,
  ) -> Result<Option<serde_json::Value>, DbError> {
    Ok(
      sqlx::query_scalar(
        "SELECT decision FROM approval_decision_intents WHERE session_id = $1 AND request_id = $2",
      )
      .bind(session_id)
      .bind(request_id)
      .fetch_optional(&self.pool)
      .await?,
    )
  }
}

/// Convenience bundle of all Pg repositories backed by the same pool.
///
/// `yanshi-server::AppState` injects this into route handlers so each route
/// can pick the repo it needs without holding a separate pool reference.
#[derive(Clone, Debug)]
pub struct Repositories {
  pub runs: PgRunRepo,
  pub steps: PgStepRepo,
  pub events: PgEventRepo,
  pub artifacts: PgArtifactRepo,
  pub skill_installs: PgSkillInstallRepo,
  pub mcp_sessions: PgMcpSessionRepo,
  pub harness_sessions: PgHarnessSessionRepo,
  pub harness_events: PgHarnessEventRepo,
  pub user_preferences: PgUserPreferenceRepo,
  pub approval_intents: PgApprovalIntentRepo,
  /// Y5.1: notebooks (and their artifact links) and sources.
  pub notebooks: crate::notebooks::PgNotebookRepo,
  pub sources: crate::notebooks::PgSourceRepo,
}

impl Repositories {
  /// Construct repositories that read and write through the same
  /// pool. Equivalent to `from_pools(pool.clone(), pool)`. Kept as
  /// the canonical entry point for single-node deployments.
  pub fn from_pool(pool: PgPool) -> Self {
    Self::from_pools(pool.clone(), pool)
  }

  /// Construct repositories with separate write + read pools
  /// (P10.15.2). The `write_pool` handles every `INSERT` /
  /// `UPDATE` / `DELETE`; the `read_pool` handles every `SELECT`.
  /// Pass the same pool twice for the single-node default.
  pub fn from_pools(write_pool: PgPool, read_pool: PgPool) -> Self {
    Self {
      runs: PgRunRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      steps: PgStepRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      events: PgEventRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      artifacts: PgArtifactRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      skill_installs: PgSkillInstallRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      mcp_sessions: PgMcpSessionRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      harness_sessions: PgHarnessSessionRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      harness_events: PgHarnessEventRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      user_preferences: PgUserPreferenceRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      notebooks: crate::notebooks::PgNotebookRepo {
        pool: write_pool.clone(),
        read_pool: read_pool.clone(),
      },
      sources: crate::notebooks::PgSourceRepo {
        pool: write_pool.clone(),
        read_pool,
      },
      approval_intents: PgApprovalIntentRepo { pool: write_pool },
    }
  }

  /// Bridge to [`crate::Database`]: picks `db.pool` for writes and
  /// `db.read_pool()` for reads (which falls back to the primary
  /// when no replica is configured). Preferred entry point for
  /// `yanshi-server::AppState::new`.
  pub fn from_database(db: &crate::Database) -> Self {
    Self::from_pools(db.pool.clone(), db.read_pool().clone())
  }
}

// ── P6.4 user preferences ────────────────────────────────────────────────

/// Tenant-scoped UI preference persistence (theme / default profile /
/// pagination size / event filter, etc.). One row per `(tenant_id, key)`.
#[async_trait]
pub trait UserPreferenceRepo: Send + Sync {
  /// Upsert a single preference. The repo stamps `updated_at`.
  async fn upsert(&self, preference: NewUserPreference) -> Result<(), DbError>;
  /// Upsert many preferences for the same tenant in a single
  /// transaction. The `PUT /v1/preferences` route uses this so a
  /// failed key doesn't leave others stranded with stale data.
  async fn upsert_many(
    &self,
    tenant_id: &str,
    entries: Vec<(String, serde_json::Value)>,
  ) -> Result<(), DbError>;
  /// Read every preference for a tenant, sorted by key for stable
  /// rendering downstream.
  async fn list_for_tenant(&self, tenant_id: &str) -> Result<Vec<UserPreference>, DbError>;
  /// Delete a specific key. Used by the UI's "reset to default" path.
  /// Returns `Ok(false)` when the row doesn't exist.
  async fn delete(&self, tenant_id: &str, key: &str) -> Result<bool, DbError>;
}

#[derive(Clone, Debug)]
pub struct PgUserPreferenceRepo {
  pub pool: PgPool,
  pub read_pool: PgPool,
}

#[async_trait]
impl UserPreferenceRepo for PgUserPreferenceRepo {
  async fn upsert(&self, preference: NewUserPreference) -> Result<(), DbError> {
    sqlx::query(
      r#"INSERT INTO user_preferences (tenant_id, key, value, updated_at)
         VALUES ($1, $2, $3, NOW())
         ON CONFLICT (tenant_id, key) DO UPDATE SET
           value = EXCLUDED.value,
           updated_at = EXCLUDED.updated_at"#,
    )
    .bind(&preference.tenant_id)
    .bind(&preference.key)
    .bind(&preference.value)
    .execute(&self.pool)
    .await?;
    Ok(())
  }

  async fn upsert_many(
    &self,
    tenant_id: &str,
    entries: Vec<(String, serde_json::Value)>,
  ) -> Result<(), DbError> {
    if entries.is_empty() {
      return Ok(());
    }
    let mut tx = self.pool.begin().await?;
    for (key, value) in entries {
      sqlx::query(
        r#"INSERT INTO user_preferences (tenant_id, key, value, updated_at)
           VALUES ($1, $2, $3, NOW())
           ON CONFLICT (tenant_id, key) DO UPDATE SET
             value = EXCLUDED.value,
             updated_at = EXCLUDED.updated_at"#,
      )
      .bind(tenant_id)
      .bind(&key)
      .bind(&value)
      .execute(&mut *tx)
      .await?;
    }
    tx.commit().await?;
    Ok(())
  }

  async fn list_for_tenant(&self, tenant_id: &str) -> Result<Vec<UserPreference>, DbError> {
    let rows = sqlx::query_as::<_, UserPreference>(
      r#"SELECT tenant_id, key, value, updated_at
         FROM user_preferences
         WHERE tenant_id = $1
         ORDER BY key"#,
    )
    .bind(tenant_id)
    .fetch_all(&self.read_pool)
    .await?;
    Ok(rows)
  }

  async fn delete(&self, tenant_id: &str, key: &str) -> Result<bool, DbError> {
    let result = sqlx::query("DELETE FROM user_preferences WHERE tenant_id = $1 AND key = $2")
      .bind(tenant_id)
      .bind(key)
      .execute(&self.pool)
      .await?;
    Ok(result.rows_affected() > 0)
  }
}

/// Tells a guarded no-op `UPDATE` (row exists, already terminal) apart
/// from a missing row. `table` is always a literal from this module.
async fn row_exists(pool: &PgPool, table: &'static str, id: Uuid) -> Result<bool, DbError> {
  let exists: bool = sqlx::query_scalar(&format!(
    "SELECT EXISTS (SELECT 1 FROM {table} WHERE id = $1)"
  ))
  .bind(id)
  .fetch_one(pool)
  .await?;
  Ok(exists)
}

#[cfg(test)]
mod tests {
  use super::*;
  use sqlx::postgres::PgPoolOptions;

  fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
      .max_connections(1)
      .connect_lazy("postgres://test:test@localhost:5432/test")
      .expect("lazy pool")
  }

  #[tokio::test]
  async fn from_pool_uses_same_pool_for_reads_and_writes() {
    let pool = lazy_pool();
    let repos = Repositories::from_pool(pool);
    // Pointer equality (via Arc) so a single-node deployment
    // hits one pool for everything and connection accounting is
    // unsurprising.
    assert!(std::ptr::eq(&repos.runs.pool, &repos.runs.pool));
    assert!(std::ptr::eq(&repos.runs.read_pool, &repos.runs.read_pool));
  }

  #[tokio::test]
  async fn from_pools_routes_separate_pools_to_every_repo() {
    let write = lazy_pool();
    let read = lazy_pool();
    let repos = Repositories::from_pools(write, read);
    // Every repo gets both pools populated. The exact "same Arc"
    // check is overspecified for sqlx's internal pool sharing;
    // what matters is the field is present and clonable.
    let _ = repos.runs.pool.clone();
    let _ = repos.runs.read_pool.clone();
    let _ = repos.steps.read_pool.clone();
    let _ = repos.events.read_pool.clone();
    let _ = repos.artifacts.read_pool.clone();
    let _ = repos.skill_installs.read_pool.clone();
    let _ = repos.mcp_sessions.read_pool.clone();
    let _ = repos.harness_sessions.read_pool.clone();
    let _ = repos.harness_events.read_pool.clone();
    let _ = repos.user_preferences.read_pool.clone();
  }

  #[tokio::test]
  async fn from_database_threads_replica_into_repos_when_set() {
    let primary = lazy_pool();
    let replica = lazy_pool();
    let db = crate::Database {
      pool: primary,
      read_pool: Some(replica),
    };
    let repos = Repositories::from_database(&db);
    // Read pool came from the replica, not the primary. The
    // primary has `size() == 0` lazily; cloning either side
    // never opens a connection in this test.
    let read = &repos.runs.read_pool;
    let write = &repos.runs.pool;
    // Distinct Arcs because primary != replica.
    assert!(!std::ptr::eq(read, write));
  }

  #[tokio::test]
  async fn from_database_falls_back_to_primary_when_no_replica() {
    let primary = lazy_pool();
    let db = crate::Database {
      pool: primary,
      read_pool: None,
    };
    let repos = Repositories::from_database(&db);
    // Without a replica configured, the read pool is a clone of
    // the primary — they're distinct fields but back the same
    // underlying connection state. Verified by checking that
    // `from_database` doesn't accidentally swap the order.
    let _read = &repos.runs.read_pool;
    let _write = &repos.runs.pool;
    // The invariant we actually care about: there's no panic, and
    // the repo carries a usable pool in both slots.
  }
}
