-- Yanshi Gateway: artifact size and digest (Y1.3, RFC_CONTENT_STUDIO §4.2).
--
-- Modality and renderer nodes now emit `FlowValue::File` into the run
-- directory (Y1.1), and the gateway registers every such file when the
-- run finishes. `GET /v1/runs/{id}/artifacts` lists size and digest so a
-- client can show what it will download and verify what it got without
-- the server reading the file for each listing. Both columns are
-- nullable: rows written before this migration stay valid.
ALTER TABLE artifacts
  ADD COLUMN IF NOT EXISTS size_bytes BIGINT,
  ADD COLUMN IF NOT EXISTS sha256 TEXT;

-- The download route resolves `(run_id, artifact id)` under the caller's
-- tenant; the existing `artifacts_run_idx` covers the run lookup.
