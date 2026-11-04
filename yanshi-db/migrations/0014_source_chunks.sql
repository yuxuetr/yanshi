-- Yanshi Gateway: chunks of ingested sources (Y5.2, RFC_CONTENT_STUDIO §6).
--
-- The chunks, not an index, are what persists: citations resolve a
-- `chunk_id` back to its text, and the per-notebook BM25 index is
-- rebuilt from these rows on first query after a restart (cached in
-- process, dropped when a source changes). No index serialisation
-- format, no vector store required.
CREATE TABLE IF NOT EXISTS source_chunks (
  chunk_id     TEXT PRIMARY KEY,
  source_id    UUID NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
  notebook_id  UUID NOT NULL REFERENCES notebooks(id) ON DELETE CASCADE,
  tenant_id    TEXT NOT NULL,
  ordinal      INT  NOT NULL,
  content      TEXT NOT NULL,
  metadata     JSONB NOT NULL DEFAULT '{}'::jsonb,
  UNIQUE (source_id, ordinal)
);
CREATE INDEX IF NOT EXISTS source_chunks_notebook_idx ON source_chunks (notebook_id);
