-- Yanshi Gateway: run leases (eval 2026-10-07 P5).
--
-- `create_if_admitted` counts a tenant's `queued`/`running` rows across
-- every replica. A run whose replica died never left `running`, so it
-- held the tenant's concurrency slot forever, and a blanket reclaim at
-- startup would also kill runs other replicas are still executing.
--
-- The replica executing a run renews its lease (`RunRepo::renew_leases`,
-- every `RUN_LEASE_RENEW_INTERVAL`); any replica's sweep fails rows whose
-- lease expired (`RunRepo::reclaim_expired_leases`). The default covers
-- the gap between the INSERT and the first renewal. 120 s must match
-- `yanshi_db::RUN_LEASE_TTL_SECS`.
ALTER TABLE runs
  ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ
  DEFAULT NOW() + INTERVAL '120 seconds';

-- Rows in flight at upgrade time get one TTL of grace. A replica still on
-- the previous version never renews, so its in-flight runs are reclaimed
-- after that.
UPDATE runs
  SET lease_expires_at = NOW() + INTERVAL '120 seconds'
  WHERE status IN ('queued', 'running') AND lease_expires_at IS NULL;

CREATE INDEX IF NOT EXISTS runs_live_lease_idx
  ON runs (lease_expires_at)
  WHERE status IN ('queued', 'running');
