# Yanshi Server Backup & Restore

Status: stable as of `P2.7`; `yanshi restore` (T2.2) closes the
round trip described below.
Scope: covers `yanshi serve` deployments in the `local` and
`production` security profiles. Workflow-only single-binary
deployments back up the same state minus the Postgres bullet point.

This document is the operator playbook for backing up an Yanshi
deployment and recovering from a fresh host. Use `yanshi doctor
--backup-check` to validate that a candidate host can actually receive
a restore before swinging traffic at it (see
[Validation](#validation) below).

## State surfaces

Yanshi persists state across four surfaces. Each one is described
below in the order it must be backed up *and* restored — DB before
filesystem, because the DB rows reference paths under run_dir and
trace_dir.

### 1. Postgres (`yanshi-db`)

Required for the gateway, harness sessions, and the run console.
Eight authoritative tables, all owned by the schema bundled with
`yanshi-server`:

| Table | Owns |
| --- | --- |
| `runs` | DAG workflow runs (status, tenant, profile, dates) |
| `steps` | Per-node execution rows (status, retries, output ptr) |
| `events` | DAG workflow event log (`EventBroker` source of truth) |
| `artifacts` | Pointer rows for run outputs persisted under run_dir |
| `skill_installs` | Installed skill metadata + checksum |
| `mcp_sessions` | Live MCP session bookkeeping |
| `harness_sessions` | Harness Agent Mode session rows |
| `harness_session_events` | Harness event log (mirrors the SSE stream) |

Backup with `pg_dump` (or `yanshi backup` — see below). Restore
with `pg_restore`. Migrations are embedded via `sqlx::migrate!()`
and run on first server start, so a restored DB at the target
schema version Just Works; a restored DB at an *older* schema
version is upgraded by `yanshi serve` before it accepts
traffic.

Use `pg_dump` / `pg_restore` of the same major version as the
server. A dump written by a newer `pg_dump` (17+) does not restore
into an older server: `pg_restore` fails on
`unrecognized configuration parameter "transaction_timeout"`.

### 2. Run artifacts (`YANSHI_RUN_DIR`)

Default: `<home>/.yanshi/runs`. Each run produces one UUID-named
subdirectory containing the node-level state files referenced by
`artifacts` rows in Postgres. **Never restore filesystem before
Postgres** — orphaned artifact directories with no owning `runs` row
will be reaped by the next cleanup sweep (P2.2).

### 3. Trace storage (`YANSHI_TRACE_DIR`)

Default: `<home>/.yanshi/traces`. JSONL / SQLite / Postgres backend
chosen at runtime via feature flags + env. The JSONL files are simple
log lines and back up via straight `tar`. SQLite + Postgres tracing
backends back up the same way as the main Postgres above.

### 4. Marketplace cache, skills, plugins

| Path | Default | Notes |
| --- | --- | --- |
| Marketplace cache | `<home>/.yanshi/marketplace/cache` | Read-only artifact cache; safe to rebuild on demand, but restoring it avoids re-downloading and re-verifying signatures. |
| Skills install dir | `<home>/.yanshi/skills` (override: `YANSHI_SKILLS_DIR`) | Authoritative for installed skill manifests. Lose this and every skill needs to be reinstalled. |
| Plugins install dir | `<home>/.yanshi/plugins` (override: `YANSHI_PLUGINS_DIR`) | Same shape as skills, but holds subprocess plugin binaries + manifests. |

## `yanshi backup` (P10.15.1)

`yanshi backup --output <path>` orchestrates the four state
surfaces above into a single bundle directory in one command:

```bash
yanshi backup --output /var/backups/yanshi/2026-05-20 \
  --database-url "$DATABASE_URL"
```

Output layout:

```text
<output>/
  manifest.json          # schema version, timestamps, per-artifact bytes + sha256
  db.dump                # pg_dump --format=custom of $DATABASE_URL
  run_dir.tar.gz         # tar -czf of $YANSHI_RUN_DIR
  trace_dir.tar.gz       # tar -czf of $YANSHI_TRACE_DIR
  marketplace_cache.tar.gz
  skills_dir.tar.gz
  plugins_dir.tar.gz
```

Each artifact entry in `manifest.json` records a `sha256:<hex>` of the
artifact file (U0.2), computed right after `pg_dump`/`tar` writes it.
`yanshi restore` recomputes this hash from the artifact on disk and
compares it before running `pg_restore`/`tar` — see
[Integrity verification](#integrity-verification) below.

Key flags:

- `--output <PATH>` *(required)* — destination directory. The
  command creates it if missing; refuses to overwrite a
  non-empty directory unless `--force` is supplied.
- `--database-url <URL>` — Postgres connection string. Falls
  back to `$DATABASE_URL`. Only consulted when the `db` include
  is in the active set.
- `--include <NAME>` *(repeatable)* — restrict to one or more
  includes. Empty = all 6. Aliases accepted: `database` → `db`,
  `runs` → `run_dir`, `traces` → `trace_dir`, etc.
- `--dry-run` — print the plan + the `manifest.json` shape the
  command would emit, mutate nothing. Useful for production
  rehearsal before running with real credentials.
- `--force` — overwrite a non-empty `--output` directory.
- `--format text|json|json-envelope` — output format for the
  per-step report. `json-envelope` is the canonical
  `yanshi.cli/1` envelope; see `docs/CLI_JSON_OUTPUT.md`.

Failure handling: a missing source directory is `skipped` (not
a failure); a tool not on PATH (`pg_dump` / `tar`) is `failed`
and the exit code is `2`. Each step's row in the manifest /
report carries `status`, `bytes`, `duration_ms`, and a
`reason` field for skipped or failed steps. The bundle
manifest is written even on partial failure so a future
`yanshi restore` can diff "what we got" against "what we
wanted" instead of guessing.

The `manifest_version` field on the bundle (currently
`"yanshi.backup/1"`) is the wire-shape promise. Bumping it
is a breaking change for any future restore tooling that pins
to the prior shape.

`yanshi restore <bundle-dir>` consumes exactly this manifest and
reverses each artifact back into the paths `yanshi backup` read
them from — see [Restore sequencing](#restore-sequencing) below.

## `yanshi restore` (T2.2)

`yanshi restore <input>` reads a bundle directory produced by
`yanshi backup` and reverses each artifact it finds in
`manifest.json`: `pg_restore` for `db.dump`, `tar -xzf` for the
directory tarballs. It restores in a fixed order — Postgres first,
then the independent caches, then trace storage, then run artifacts
last — regardless of `--include` order or the manifest's own artifact
order, for the same reason backup/restore ordering matters below.

```bash
yanshi restore /var/backups/yanshi/2026-05-20 \
  --database-url "$DATABASE_URL"
```

Key flags (mirror `yanshi backup`):

- `<input>` *(positional, required)* — bundle directory containing
  `manifest.json` (as produced by `yanshi backup --output`).
- `--database-url <URL>` — Postgres connection string to restore
  into. Falls back to `$DATABASE_URL`. Only consulted when the `db`
  include is active and the manifest has a `db` artifact.
- `--include <NAME>` *(repeatable)* — restrict to one or more
  includes, same names/aliases as `yanshi backup`. Empty = every
  include present in the manifest.
- `--dry-run` — parse the manifest and print what would run, mutate
  nothing (no DB writes, no target directories created).
- `--force` — overwrite an existing target directory for a
  filesystem include (removes it before extracting). Without
  `--force`, a target directory that already exists fails that step
  rather than merging tar contents into stale files.
- `--skip-integrity-check` — restore an artifact even when its
  recomputed SHA-256 does not match the manifest's recorded hash
  (U0.2). Off by default. Not recommended outside of deliberate
  recovery from a bundle you already know is off-spec (e.g. testing);
  see [Integrity verification](#integrity-verification).
- `--format text|json|json-envelope` — same three report formats as
  `yanshi backup`.

Failure handling mirrors `yanshi backup`: an artifact absent from
the manifest is `skipped`, a missing bundle/manifest file or a tool
not on PATH (`pg_restore` / `tar`) is `failed`, and the process exits
`2` if any step failed. A restore target directory is created fresh
(not merged into) so the archive's original top-level path component
never leaks into the restored layout, even when the restore target
has a different name than the directory that was originally backed
up (different host, renamed env override, etc.). The safety guard
that refuses a top-level/root restore target always runs before any
destructive filesystem operation on that target (U0.1) — a
misconfigured env override (e.g. `YANSHI_RUN_DIR=/`) combined with
`--force` fails closed instead of deleting the target's parent
directory tree.

## Integrity verification

`yanshi restore` recomputes the SHA-256 of each artifact on disk
and compares it against the hash `yanshi backup` recorded in
`manifest.json`, **before** running `pg_restore --clean --if-exists`
(which deletes existing database objects unconditionally) or `tar`
(which has no archive-entry sanity checks of its own — path traversal
and symlink safety depend entirely on the host's `tar` binary). A
mismatch — a corrupted transfer, or a bundle that was tampered with —
fails that step without touching the target:

```text
$ yanshi restore /var/backups/yanshi/2026-05-20
  skills_dir         failed   /home/op/.yanshi/skills
    └─ artifact integrity check failed: manifest recorded sha256:ab12…, computed sha256:cd34… —
       the artifact may be corrupted or tampered with; pass --skip-integrity-check to restore
       anyway (not recommended)
```

`--skip-integrity-check` overrides this per invocation (applies to
every artifact restored in that run, not a per-artifact flag) and
still surfaces a visible warning in the step's `reason` field rather
than silently proceeding. A bundle produced by an `yanshi` build
that predates U0.2 has no recorded hash for its artifacts; restoring
it is unaffected — such artifacts are treated as "cannot verify" (a
warning, not a failure) rather than rejected outright.

## Restore sequencing

`yanshi restore` already applies this order internally; it's
listed here for operators restoring a surface manually (e.g. via raw
`pg_restore` / `tar -xzf`) instead of through the command.

1. **Postgres first.** `pg_restore` into a fresh database (or
   `yanshi restore --include db`), then point `yanshi serve
   --database-url` at it.
2. **Marketplace cache, skills, plugins.** These are independent of
   the DB and can restore in any order before the server starts.
3. **Trace storage.** Optional. Restore only if you need history for
   replay. Restoring trace files for runs whose rows are missing from
   `runs` is harmless but the rows will be unjoined.
4. **Run artifacts.** Restore last. Restoring run_dir before Postgres
   produces orphan trees that the next cleanup sweep deletes.
5. **Doctor smoke before traffic.** Run `yanshi doctor --profile
   production --backup-check` (and `--server <url>` once the gateway
   is bound) before swinging traffic. The check refuses to pass if any
   of the five backup-relevant dirs are non-writable, and refuses to
   pass if the server `<url>/health` probe doesn't return 2xx.

## Validation

`yanshi doctor --backup-check` is the deployment-time smoke. It
adds a `backup_check` section to the doctor report enumerating
writability probes for run_dir, trace_dir, marketplace_cache,
skills_dir, plugins_dir.

```text
$ yanshi doctor --profile production --backup-check
…
Backup check:
  run dir: ok (/var/yanshi/runs) [env]
  trace dir: ok (/var/yanshi/traces) [env]
  marketplace cache: ok (~/.yanshi/marketplace/cache) [default]
  skills dir: ok (~/.yanshi/skills) [default]
  plugins dir: ok (~/.yanshi/plugins) [default]
…
Status: ok
```

Production profile escalation rules:

| State | Production | Local / Dev |
| --- | --- | --- |
| Directory does not exist | Fail (exit 2) | Warning (exit 1) |
| Directory exists but is not writable | Fail (exit 2) | Fail (exit 2) |

Exit codes:

- `0` — every dir writable; backup readiness OK.
- `1` — one or more dirs missing (warn-level under local / dev).
- `2` — one or more dirs explicitly non-writable, or production profile
  with a missing dir.

## First-stable-release validation checklist

For the v1.0 release dress rehearsal (`P7.4`), run this checklist on
a freshly provisioned host before declaring the deployment ready:

- [ ] Postgres restored from latest backup; `yanshi serve --check`
  reports `db.reachable: true`.
- [ ] Marketplace cache restored; `yanshi doctor --backup-check`
  reports `marketplace cache: ok`.
- [ ] Skills install dir restored; `yanshi skill list` enumerates
  the expected skills.
- [ ] Plugins install dir restored (if any plugins are installed);
  `yanshi plugin list` enumerates them.
- [ ] Run artifacts restored (if required for the deployment's
  history retention policy).
- [ ] Trace storage restored (if required for replay).
- [ ] `yanshi doctor --profile production --backup-check --server
  http://127.0.0.1:8080` exits `0`.
- [ ] One representative run replays cleanly via `yanshi trace
  replay <run-id>`.

If any line fails, **do not swing traffic**. Fix the gap, re-run
`doctor --backup-check`, and only proceed once the report exits `0`.

## Related

- `docs/CHECKPOINT_RECOVERY.md` — workflow-level resume semantics.
- `docs/CURRENT_STATUS.md` — current state of the gateway and Web UI.
- `yanshi-server::cleanup` (P2.2) — the retention sweep that prunes
  orphaned run_dir subdirectories.
- `yanshi doctor --backup-check` (P2.7) — the deployment-time
  smoke this document refers to.
- `yanshi backup --output <path>` (P10.15.1) — the
  orchestrator that wraps `pg_dump` + `tar` into one command.
- `yanshi restore <input>` (T2.2) — the counterpart orchestrator
  that wraps `pg_restore` + `tar -xzf` into one command, consuming
  the manifest `yanshi backup` writes.
