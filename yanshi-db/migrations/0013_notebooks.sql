-- Yanshi Gateway: notebooks and their sources (Y5.1,
-- docs/RFC_CONTENT_STUDIO.md §6).
--
-- A notebook groups sources (what the user brought: a URL, an uploaded
-- file, pasted text, a recording) and the deliverables generated from
-- them. Every row carries its tenant; repositories filter on it so a
-- notebook id from another tenant behaves as missing.

CREATE TABLE IF NOT EXISTS notebooks (
  id          UUID PRIMARY KEY,
  tenant_id   TEXT NOT NULL,
  title       TEXT NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS notebooks_tenant_idx ON notebooks (tenant_id, created_at DESC);

-- `status`: pending → ingesting → ready | failed (Y5.2 runs the
-- ingestion). `sha256` is the fetched / uploaded content's digest, set
-- when ingestion reads it.
CREATE TABLE IF NOT EXISTS sources (
  id           UUID PRIMARY KEY,
  notebook_id  UUID NOT NULL REFERENCES notebooks(id) ON DELETE CASCADE,
  tenant_id    TEXT NOT NULL,
  kind         TEXT NOT NULL CHECK (kind IN ('url', 'file', 'text', 'recording')),
  title        TEXT NOT NULL,
  uri          TEXT,
  status       TEXT NOT NULL DEFAULT 'pending'
               CHECK (status IN ('pending', 'ingesting', 'ready', 'failed')),
  error        TEXT,
  sha256       TEXT,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS sources_notebook_idx ON sources (notebook_id, created_at);

-- Deliverables generated in a notebook: links to registered artifacts
-- (Y1.3), labelled with the deliverable kind that produced them.
CREATE TABLE IF NOT EXISTS notebook_artifacts (
  notebook_id  UUID NOT NULL REFERENCES notebooks(id) ON DELETE CASCADE,
  artifact_id  UUID NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  kind         TEXT NOT NULL,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (notebook_id, artifact_id)
);
