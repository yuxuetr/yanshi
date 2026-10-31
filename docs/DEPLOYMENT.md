# Deployment

Yanshi currently has two runnable binaries:

- `yanshi-server`: long-running Axum gateway service. This is the primary container and Helm target.
- `yanshi`: CLI workflow, skill, MCP, trace, and configuration utility. It can be built from the same Dockerfile by changing build args, but it is not the default Kubernetes workload.

## Docker Image

Build the server image:

```bash
docker build -t yanshi:server .
```

Build the CLI image:

```bash
docker build \
  --build-arg PACKAGE=yanshi-cli \
  --build-arg BIN=yanshi \
  -t yanshi:cli .
```

The image runs as a non-root user and exposes port `3000` for `yanshi-server`.

## Docker Compose

The included Compose file starts PostgreSQL and the gateway:

```bash
docker compose up --build
curl http://localhost:3000/health
curl http://localhost:3000/health/live
curl http://localhost:3000/health/ready
```

Compose sets:

- `DATABASE_URL=postgres://yanshi:yanshi@postgres:5432/yanshi`
- `PORT=3000`
- `YANSHI_API_TOKEN=local-dev-change-me` (V3.1 — see below)
- `YANSHI_RUN_DIR=/data/runs` can be set to control workflow artifact storage.
- `RUST_LOG=info`

`YANSHI_SECURITY_PROFILE` is left unset (defaults to `local`) — see
[§ Security profile](#security-profile-u12) below for what that means
and what to set for anything beyond local development.

**V3.1:** because `PORT` makes the container bind `0.0.0.0` (never
loopback-only), the gateway now refuses to start under *any* profile
if no bearer token is configured — see [§ PORT and PaaS-style public
binding](#port-and-paas-style-public-binding-v31). The shipped
`YANSHI_API_TOKEN` value above is a well-known placeholder, not a
secret; it exists only so `docker compose up --build` keeps working
out of the box for a local trial. Replace it with a private value
before this stack (or its published `3000:3000` port) is reachable by
anyone else.

## Helm

Install with an existing PostgreSQL connection secret:

```bash
kubectl create secret generic yanshi-db \
  --from-literal=DATABASE_URL='postgres://user:password@postgres:5432/yanshi'

helm install yanshi charts/yanshi \
  --set image.repository=yanshi \
  --set image.tag=server \
  --set existingSecret=yanshi-db
```

For local development only, Helm can create the secret from values:

```bash
helm install yanshi charts/yanshi \
  --set image.repository=yanshi \
  --set image.tag=server \
  --set secretEnv.DATABASE_URL='postgres://user:password@postgres:5432/yanshi'
```

Prefer `existingSecret` in shared environments so credentials do not live in Helm release values.

### Security profile (U1.2)

`values.yaml` ships `securityProfile: production` by default (changed
from `local` after the 2026-10-07 evaluation, S1). Set
`--set securityProfile=local` only for a single trusted operator; an
existing install that relied on the old default changes posture on
`helm upgrade` (CORS narrows to `YANSHI_CORS_ALLOWED_ORIGINS`).

**V3.1:** a missing `YANSHI_API_TOKEN` now fails startup
(`CrashLoopBackOff`) under *every* profile, not just `production` — a
pod's `containerPort` is never loopback-only, so an unauthenticated
gateway is refused regardless of `securityProfile`. Set a token before
installing, the same way `DATABASE_URL` is already wired:

```bash
kubectl create secret generic yanshi-db \
  --from-literal=DATABASE_URL='postgres://user:password@postgres:5432/yanshi' \
  --from-literal=YANSHI_API_TOKEN='replace-with-a-real-secret'

helm install yanshi charts/yanshi \
  --set image.repository=yanshi \
  --set image.tag=server \
  --set existingSecret=yanshi-db
```

Or, for local development only, let Helm create the secret from values
(same caveat as `secretEnv.DATABASE_URL` — prefer `existingSecret` in
shared environments so credentials don't live in Helm release values):

```bash
helm install yanshi charts/yanshi \
  --set image.repository=yanshi \
  --set image.tag=server \
  --set secretEnv.DATABASE_URL='postgres://user:password@postgres:5432/yanshi' \
  --set secretEnv.YANSHI_API_TOKEN='replace-with-a-real-secret'
```

A configured token satisfies the new startup check under every
profile, but any Helm install reachable by users or hosts you don't
fully trust should still set `production` explicitly for the rest of
its fail-closed posture, which the token check alone does not cover:

```bash
helm install yanshi charts/yanshi \
  --set image.repository=yanshi \
  --set image.tag=server \
  --set existingSecret=yanshi-db \
  --set securityProfile=production
```

Under `production`: CORS defaults to an explicit origin allow-list
instead of permissive (see `YANSHI_CORS_ALLOWED_ORIGINS` above);
and if you also enable the worker gRPC control plane (T1.2,
`--worker-grpc`), worker admission becomes fail-closed. See
`docs/SECURITY_PROFILES.md` for the full per-profile defaults table.

### Gateway policy (eval 2026-10-07 S1/S2/S7)

A workflow YAML carries its own sandbox settings (`allowed_commands`,
`allowed_paths`), and several node types run host processes or load code
from a path the YAML names. On the CLI the author is the operator; on
the gateway the author is any token holder. The gateway therefore caps
what a request may name, and **denies everything by default**:

| Env var (comma-separated) | Caps | Unset |
|---|---|---|
| `YANSHI_GATEWAY_HOST_NODE_TYPES` | which of `shell`, `file`, `mcp`, `plugin`, `agent`, `skill_agent`, `multi_agent` a `POST /v1/runs` workflow may use | all rejected (400) |
| `YANSHI_GATEWAY_ALLOWED_COMMANDS` | the most a `shell` node's `allowed_commands` may list | every shell command rejected |
| `YANSHI_GATEWAY_PATH_ROOTS` | absolute roots that every `allowed_paths` entry, harness `workspace_root` and resume-plan `checkpoint_dir` must sit under | every host path rejected — so harness sessions need this set |

Nested `map` / `while` nodes are checked too. `shell` nodes on the
gateway must declare `allowed_paths`. Containment is lexical (`..` is
refused), so keep symlinks out of the roots. The gateway refuses to start
on a relative root or an unknown node type. Skills run through
`/v1/skills/{name}:run` come from the operator's catalog and are not
affected.

```bash
helm install yanshi charts/yanshi \
  --set existingSecret=yanshi-db \
  --set env.YANSHI_GATEWAY_PATH_ROOTS=/srv/yanshi/work \
  --set env.YANSHI_GATEWAY_HOST_NODE_TYPES=file
```

### PORT and PaaS-style public binding (V3.1)

The server binary's `resolve_bind()` gives the `PORT` env var
unconditional priority, always binding `0.0.0.0:$PORT` when it's set —
the standard convention PaaS platforms (Heroku/Render/Railway/Cloud Run
and similar) use to tell a process which port to listen on for public
traffic. Because that bind is never loopback-only, the gateway now
refuses to start (exit code 2, or a `Fail` readiness under `yanshi
serve --check`) if no bearer token is configured, **regardless of
`YANSHI_SECURITY_PROFILE`** — `local`/`dev`'s historical
no-token-required posture only ever excused a missing token when the
socket was loopback-only (`127.0.0.1`/`::1`), and that exemption never
applied to a public bind in the first place; it just wasn't enforced
before V3.1. To satisfy the check: set `YANSHI_API_TOKEN` (or
`YANSHI_API_TOKEN_TENANTS`), bind to a loopback address instead
(unset `PORT`, set `YANSHI_SERVE_BIND=127.0.0.1:<port>`), or set
`YANSHI_SECURITY_PROFILE=production` with a token configured.

Caveat: `yanshi-cli`'s `yanshi serve --bind <addr>` spawns the
`yanshi-server` binary as a child process and sets
`YANSHI_SERVE_BIND` on it, but does not clear an `PORT` inherited
from the parent shell — since `resolve_bind()` checks `PORT` first,
an inherited `PORT` still wins over an explicit `--bind 127.0.0.1:...`
flag. If you're launching `yanshi serve` from an environment that
already has `PORT` set (common when a process manager or PaaS buildpack
exports it globally), unset it first or rely on `YANSHI_SERVE_BIND`
being what you actually want bound.

### Resource requests/limits and autoscaling (T4.3)

`values.yaml` ships default CPU/memory requests and limits on the
`yanshi-server` container (`resources.requests` = 100m CPU / 128Mi
memory, `resources.limits` = 500m CPU / 512Mi memory); override with
`--set resources.requests.cpu=...` or a values override file for
production sizing.

A `HorizontalPodAutoscaler` template is available but disabled by
default (`autoscaling.enabled: false`) to keep the prior single-replica
behavior for existing installs unchanged.

**W4.2a — running more than one `yanshi-server` replica is not yet
safe.** `yanshi-server` keeps several pieces of state process-local:
SSE event fan-out (a run's live events only reach subscribers on the
same pod that's executing it), per-tenant run-admission concurrency and
rate limits (each pod enforces its own independent counter, so N
replicas silently multiply the effective limit by N), and harness
approval/cancellation routing (a decide/cancel request landing on a
different pod than the one running the session either 404s or silently
no-ops while still reporting success). This is tracked as `TODOs.md`
W4.2 and not yet fixed. The chart therefore refuses to render
`replicaCount > 1` or `autoscaling.minReplicas > 1` unless you set
`allowMultiReplica: true` — an explicit, informed opt-out for operators
who understand and accept the gaps above (or who have their own
session-affine routing in front of the gateway that sidesteps most of
them).

Once you've read that and decided to proceed, enable HPA to scale on
CPU (and optionally memory) utilization:

```bash
helm install yanshi charts/yanshi \
  --set image.repository=yanshi \
  --set image.tag=server \
  --set existingSecret=yanshi-db \
  --set allowMultiReplica=true \
  --set autoscaling.enabled=true \
  --set autoscaling.minReplicas=2 \
  --set autoscaling.maxReplicas=5 \
  --set autoscaling.targetCPUUtilizationPercentage=80
```

When `autoscaling.enabled` is `true`, the Deployment's `spec.replicas`
field is omitted so the HPA is free to manage replica count without the
chart fighting it back to `replicaCount` on every `helm upgrade`.
`autoscaling.targetMemoryUtilizationPercentage` is unset by default; set
it (e.g. `--set autoscaling.targetMemoryUtilizationPercentage=80`) to add
a memory-based scaling metric alongside CPU.

### PodDisruptionBudget (U3.2)

A `PodDisruptionBudget` template is available but disabled by default
(`podDisruptionBudget.enabled: false`). **Only enable it for
multi-replica deployments** (`replicaCount > 1`, or `autoscaling.enabled`
with `minReplicas > 1`) — with the single-replica default, a PDB
requiring `minAvailable: 1` can never be satisfied while giving up the
only replica, so it blocks voluntary pod eviction entirely (node
drains for maintenance/upgrades stall indefinitely instead of
respecting `terminationGracePeriodSeconds`).

```bash
helm install yanshi charts/yanshi \
  --set image.repository=yanshi \
  --set image.tag=server \
  --set existingSecret=yanshi-db \
  --set autoscaling.enabled=true \
  --set autoscaling.minReplicas=2 \
  --set podDisruptionBudget.enabled=true \
  --set podDisruptionBudget.minAvailable=1
```

Set exactly one of `podDisruptionBudget.minAvailable` /
`podDisruptionBudget.maxUnavailable` (both accept an integer or a
percentage string like `"50%"`); `minAvailable` takes precedence if
both are set. `helm template`/`helm lint` validate cleanly with the
default (PDB omitted entirely) and with it explicitly enabled.

## Health Checks

`yanshi-server` exposes:

- `/health`: basic service health.
- `/health/live`: liveness probe.
- `/health/ready`: readiness probe.

The Helm chart wires liveness and readiness probes to those endpoints. The current readiness endpoint confirms the process is serving HTTP; startup still fails if `DATABASE_URL` cannot be connected.

## Volumes And Secrets

- The server requires `DATABASE_URL`.
- LLM provider keys and tool credentials should be provided through Kubernetes Secrets or external secret injection, not image layers.
- CLI containers that need `~/.yanshi` can mount it as a volume at `/home/yanshi/.yanshi`.
- Trace files should be backed by a persistent volume only when using file-backed trace storage.
- The Helm chart runs with `readOnlyRootFilesystem: true` and mounts two
  writable `emptyDir`s, `/data` (with `YANSHI_RUN_DIR=/data/runs`) and
  `/tmp`, each capped by `scratch.sizeLimit` (default `1Gi`). Before
  this, every run failed creating its run dir. Run artifacts are per-pod
  scratch and are lost on restart.

### Run leases (orphaned runs)

Each run carries a lease (`runs.lease_expires_at`, 120 s) that the replica
executing it renews every 30 s. Every replica fails `queued`/`running`
runs whose lease has expired, with error `lease expired: ...`, and
appends a `workflow.failed` event so SSE followers see the run end. A
crashed replica's runs thus stop holding the tenant's concurrency slot. On
upgrade to this version, in-flight runs get one TTL of grace. A replica
still on an older version never renews, so its runs are reclaimed after
about 2 minutes. Finish the rollout within that window, or drain first.

### OTLP trace export

Set `OTEL_EXPORTER_OTLP_ENDPOINT` (the OTLP/HTTP traces URL, e.g.
`http://otel-collector:4318/v1/traces`; optional
`OTEL_EXPORTER_OTLP_HEADERS=key=value,...`) and the gateway exports each
finished `POST /v1/runs` trace as spans with service name
`yanshi-server`. This works without `YANSHI_TRACE_DIR`. Distributed runs
(`execution_mode: "distributed"`) also export the stitched worker-task
spans. Workers report through the server and don't export themselves.
The exporter is built at startup, so a malformed endpoint fails the boot.
gRPC transport is not built in.

### File-backed trace dir is opt-in and not garbage-collected

Setting `YANSHI_TRACE_DIR=<path>` opts the gateway in to writing one
`<run_id>.json` file per `POST /v1/runs` execution (so operators can
inspect via `yanshi trace tui <run_id> --dir <path>`). The Postgres
event log remains the source of truth either way.

### Persistent harness conversation memory (resume)

By default the live harness executor runs each session on an in-process
conversation memory, so `POST /v1/harness/sessions/{id}:resume` restores the
**event log** but not the agent's prior conversation. Set
`YANSHI_HARNESS_MEMORY_DB=<path>` to back the harness agent with a
persistent SQLite store keyed by `session_id` — then a resumed session reads
its prior turns back across restarts (long-lived sessions). It is opt-in
because a shared SQLite file assumes a single gateway node; multi-node
deployments should front conversation memory with their own backend. The CLI
(`yanshi harness run --session <id>`, `--model` path) persists under the
run-dir automatically.

The cleanup sweep documented under "Per-run retention overrides" deletes
expired `runs` / `events` / `artifacts` rows but **does not touch the
trace JSON directory**. If you enable this on a long-running deployment,
rotate the directory externally (e.g. a `logrotate`-style cron or a
volume with its own retention policy) — otherwise it will grow without
bound. Default-deployment behaviour with the env var unset is unchanged:
no trace files are produced, no cleanup needed.

## v0.3.0 N8: Control-plane HTTP surface

The gateway applies its `yanshi-db` migrations on startup
(`connect_and_migrate`). Six tables back the platform: `runs`, `steps`,
`events`, `artifacts`, `skill_installs`, `mcp_sessions`. To verify the
schema is up:

```bash
docker compose up -d postgres yanshi-server
docker compose exec postgres psql -U yanshi -d yanshi \
  -c "\dt"
```

### Authentication

Every `/v1/*` route requires `Authorization: Bearer <token>` when
`YANSHI_API_TOKEN` and/or `YANSHI_API_TOKEN_TENANTS` is set. With
neither set the server runs open (useful for local dev — startup logs a
warning).

```bash
export YANSHI_API_TOKEN="dev-secret"
curl -H "Authorization: Bearer dev-secret" http://localhost:3000/v1/whoami
```

Health probes (`/health`, `/health/live`, `/health/ready`) bypass auth so
load balancers / kubelet probes work without secrets.

#### Multi-tenant deployments: bind tokens to tenants (U1.1)

`YANSHI_API_TOKEN` is a single **unbound** token: any request
authenticated with it can claim to act as *any* tenant via the
`X-Yanshi-Tenant` header, because nothing ties the token to a specific
tenant. That's fine for a single-tenant deployment, but it means a
multi-tenant deployment sharing one token has **no real tenant
isolation** — the header is a self-reported claim, not a credential.

For genuine isolation, issue one token per tenant via
`YANSHI_API_TOKEN_TENANTS` — comma-separated `token:tenant_id` pairs:

```bash
export YANSHI_API_TOKEN_TENANTS="tokA:tenant-acme,tokB:tenant-globex"
```

A request authenticated with `tokA` always acts as `tenant-acme`,
regardless of what `X-Yanshi-Tenant` it sends — a header naming a
*different* tenant is rejected (`403 tenant_mismatch`), not silently
honored. `YANSHI_API_TOKEN` can be set alongside
`YANSHI_API_TOKEN_TENANTS` (the legacy token keeps trusting the
header, for callers that don't need per-tenant isolation — e.g. an
internal ops tool) or omitted entirely (pure per-tenant-token mode). A
token cannot appear in both variables — that's a startup config error.
See `yanshi-server/src/auth.rs` module doc for the full precedence
rule.

### Read-replica routing (P10.15.2)

Read-heavy gateways can route `GET /v1/runs/{id}`,
`GET /v1/runs/{id}/events/history`, `GET /v1/harness/sessions`,
and similar `list_*` / `get_*` paths to a Postgres read replica
while writes (run submission, status updates, retention sweep
deletes) continue to hit the primary.

```bash
# Primary URL for writes + migrations:
export DATABASE_URL="postgres://gw:secret@primary.db.internal/yanshi"
# Replica URL for SELECTs:
export YANSHI_DATABASE_READ_URL="postgres://gw:secret@replica.db.internal/yanshi"

yanshi serve
# Or via the CLI flag:
yanshi serve \
  --database-url "$DATABASE_URL" \
  --database-read-url "$YANSHI_DATABASE_READ_URL"
```

When `YANSHI_DATABASE_READ_URL` is unset (the default),
reads fall back to the primary — that's the single-node
deployment behavior and is fully backwards-compatible.

**Caveats:**

- **Replication lag.** A client that writes (`POST /v1/runs`)
  and immediately reads (`GET /v1/runs/{id}`) may observe the
  prior state because the replica hasn't caught up. The
  cleanup sweep, run-row creation, and harness session
  creation all read+write through the primary in the same
  call, so this only affects HTTP clients that submit then
  re-query in the same round trip.
- **Migrations always run against the primary.** The replica
  catches up via Postgres streaming replication; we never
  apply DDL against it directly.
- **Pool budgets are independent.** The replica pool defaults
  to 2× the primary's connection cap (16 vs 8) on the
  assumption that the gateway is read-heavy. Operators with
  unusual ratios can rebuild from `Database::connect_with_replica`
  directly.

### Submit and inspect a run

`POST /v1/runs` executes config-first workflow YAML through
`yanshi-core::Flow`. The server persists the queued row immediately,
switches it to `running` in the background, stores workflow events in the
`events` table, streams them over SSE, and sets the terminal status to
`succeeded` or `failed`.

Run artifacts are written under `YANSHI_RUN_DIR/<run_id>` when
`YANSHI_RUN_DIR` is set; otherwise the default is
`~/.yanshi/runs/<run_id>` (or a temp directory if the home directory cannot
be resolved). The chosen per-run path is returned as `run_dir` from
`GET /v1/runs/{id}`.

```bash
# Submit a workflow body. Returns { "run_id": "...", "status": "queued" }.
RUN=$(curl -sX POST http://localhost:3000/v1/runs \
  -H "Authorization: Bearer dev-secret" \
  -H "Content-Type: application/json" \
  -d @examples/server/fixed_dag_run.json)
RUN_ID=$(echo "$RUN" | jq -r .run_id)

# Poll for state.
curl -s -H "Authorization: Bearer dev-secret" \
  http://localhost:3000/v1/runs/$RUN_ID | jq .

# Subscribe to live events (Server-Sent Events). Press Ctrl-C to detach.
curl -N -H "Authorization: Bearer dev-secret" \
  http://localhost:3000/v1/runs/$RUN_ID/events
```

Expected event kinds for a successful fixed DAG include:
`workflow.started`, `node.started`, `node.output.captured`, `node.completed`,
and `workflow.completed`.

#### Per-run retention overrides (P10.14.1)

The `POST /v1/runs` body accepts an optional `retention_overrides`
object that pins a run's events and/or artifacts for longer than
the tenant + profile default:

```json
{
  "workflow": "...yaml...",
  "retention_overrides": {
    "events_days": 90,
    "artifacts_days": 365
  }
}
```

Semantics: the cleanup sweep uses
`max(global_default, override)` so a per-run override can only
ever *extend* retention. Pinning a run also pins its row
itself — otherwise the `ON DELETE CASCADE` from `runs` would
yank the pinned events/artifacts out from under the override.
Negative values are rejected with `bad_request`; `Some(0)` is
accepted and normalized to "no override". See
`docs/SERVER_BACKUP_RESTORE.md` for the operator-side retention
defaults.

To inspect run status (the previously-documented
`/v1/runs/{id}/graph` endpoint was removed in P10.13.1 along with
the `yanshi-viz` crate; use the SSE event stream or the run
detail endpoint instead):

```bash
curl -s -H "Authorization: Bearer dev-secret" \
  http://localhost:3000/v1/runs/$RUN_ID | jq .
```

To cancel a queued or running run:

```bash
curl -sX POST -H "Authorization: Bearer dev-secret" \
  http://localhost:3000/v1/runs/$RUN_ID:cancel | jq .
```

Cancellation is idempotent. A queued or running run is marked `cancelled`, the
background task receives a cancellation signal and is aborted, and a
`run.cancelled` event is appended for SSE/history consumers. Cancelling an
already terminal run returns its current status without error.

To resume a stream after a network blip, pass the last seq the client saw:

```bash
curl -N -H "Authorization: Bearer dev-secret" \
  "http://localhost:3000/v1/runs/$RUN_ID/events?after_seq=12"
```

### Skills

Mount a `skills.index.toml` and point `YANSHI_SKILLS_INDEX` at it. Then:

```bash
curl -s -H "Authorization: Bearer dev-secret" http://localhost:3000/v1/skills | jq .
curl -sX POST -H "Authorization: Bearer dev-secret" \
  -H "Content-Type: application/json" \
  -d '{"input": "summarise this paragraph: ..."}' \
  http://localhost:3000/v1/skills/summariser:run | jq .
```

Skill invocation creates a `runs` row with `workflow = "@skill:<name>"` and
dispatches through the same executor used by `/v1/runs`. Direct skill run
execution remains a separate integration path; config-first workflows can run
skills today by using `skill_agent` / `agent` nodes in workflow YAML.

### Unified error envelope

Every error response is shaped `{ "error": { "code", "message", "details" } }`.
Stable codes: `unauthorized`, `forbidden`, `bad_request`, `not_found`,
`database_error`, `internal_error`, `server_misconfigured`. Branch on
`code` rather than message text — the message is informational and may
change between releases.

### Postgres test database for development

`yanshi-db` integration tests are gated by `YANSHI_DATABASE_TEST_URL`
to keep `cargo test --workspace` hermetic. To run them locally against the
docker-compose Postgres:

```bash
docker compose up -d postgres
export YANSHI_DATABASE_TEST_URL=postgres://yanshi:yanshi@localhost:5432/yanshi
cargo test -p yanshi-db -p yanshi-server
```
