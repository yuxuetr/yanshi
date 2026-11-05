# Changelog

All notable changes to Yanshi are documented in this file. Format
loosely follows [Keep a Changelog](https://keepachangelog.com/), and
the project tracks [Semantic Versioning](https://semver.org/) at the
workspace level (most crates pin to 0.2.x or 0.3.0-alpha as of this
file's first entry).

## [Unreleased]

_New entries go here. Will roll into the next tag (likely
`v1.0.0-rc.2` or `v1.0.0`)._

### BREAKING

- **Project renamed AgentFlow → Yanshi (`5f9065d`).** There is no
  compatibility alias: nothing reads the old env vars or directories.
  Migrate before upgrading:

  | Old | New | Notes |
  |---|---|---|
  | `AGENTFLOW_<NAME>` env vars | `YANSHI_<NAME>` | Same suffix for every variable, e.g. `AGENTFLOW_API_TOKEN` → `YANSHI_API_TOKEN`, `AGENTFLOW_SECURITY_PROFILE`, `AGENTFLOW_RUN_DIR`, `AGENTFLOW_TRACE_DIR`, `AGENTFLOW_MODELS_CONFIG`, `AGENTFLOW_WORKER_*`. An unset new name silently falls back to its default. |
  | `~/.agentflow/` | `~/.yanshi/` | `.env`, `models.yml`, `skills/`, `plugins/`, `traces/`, `runs/`, `mcp.toml`; `mv ~/.agentflow ~/.yanshi` |
  | project-local `.agentflow/` | `.yanshi/` | same layout |
  | binary `agentflow` | `yanshi` | |
  | crates `agentflow-*` / `agentflow_*` | `yanshi-*` / `yanshi_*` | `AgentFlowError` → `YanshiError` |
  | Helm chart `charts/agentflow` | `charts/yanshi` | Resource names come from `yanshi.fullname`, so a `helm upgrade` creates new objects; re-create Secrets under the new names |
  | image `ghcr.io/<owner>/agentflow-server` | `ghcr.io/<owner>/yanshi-server` | |
  | gRPC package `agentflow.scheduler.v1` | `yanshi.scheduler.v1` | Old workers cannot talk to a new server; upgrade both together |
  | Postgres NOTIFY channels `agentflow_*` | `yanshi_*` | Old and new replicas don't see each other's cancel/approval/event signals; no mixed-version rollout across the rename |
  | Prometheus metrics `agentflow_*` | `yanshi_*` | Update dashboards and alerts |

  The database schema itself is unchanged (the SQL migrations were left
  as-is so sqlx checksums still match), so an existing database carries
  over.
- **Gateway is deny-by-default for host-reaching nodes (eval 2026-10-07
  S1/S2/S7, `7d357ac`).** `POST /v1/runs` rejects `shell` / `file` /
  `mcp` / `plugin` / `agent` / `skill_agent` / `multi_agent` nodes, and
  harness sessions and resume plans reject every host path, until the
  operator sets `YANSHI_GATEWAY_HOST_NODE_TYPES`,
  `YANSHI_GATEWAY_ALLOWED_COMMANDS` and `YANSHI_GATEWAY_PATH_ROOTS`
  (see `docs/DEPLOYMENT.md` § Gateway policy). The Helm chart now
  defaults to `securityProfile: production`.
- **Production profile requires approval for every tool not declared
  `Idempotent` (`b829300`).** Previously only `NonIdempotent` escalated,
  so `Unknown` tools (most MCP and script tools) ran unapproved. `http`
  `PUT` / `DELETE` are now `NonIdempotent`; only `GET` / `HEAD` stay
  `Idempotent`.
- **`http` workflow node fails on a non-2xx response.** It used to
  return `Ok` with a made-up `status` (`200` for any success, `500` for
  any failure), so downstream nodes consumed error pages as data. Now
  `status` is the real 2xx code, and any other status fails the node
  with `HTTP <code>: <body excerpt>`; 5xx and 429 stay retryable, other
  statuses don't. Workflows that branched on `status == 500` should use
  the node's failure instead. `HttpTool::send` (new) returns the real
  status and full body.
- **`VectorStore::hybrid_search` removed.** Its default ignored `alpha`
  and ran a plain similarity search; no store overrode it and nothing
  called it. Hybrid retrieval is `retrieval::hybrid::HybridRetriever`,
  which the `rag` node and `yanshi rag ops search` already use.
- **`yanshi_harness::wrap_registry` returns `Arc<ToolRegistry>`.** A
  tool registered into the returned registry skipped the hook and
  approval pipeline. Every caller already put the result straight into
  an `Arc`, so no bypass shipped; the type now rules it out. Register
  every tool before wrapping, and drop the caller's `Arc::new(...)`.
- **`yanshi-graph` drops its `tokio` and `dirs` dependencies.** The only
  runtime use of `tokio` was `From<tokio::time::error::Elapsed> for
  YanshiError`, which nothing called and which reported
  `duration_ms: 0`; it is removed. The default checkpoint dir uses
  `std::env::home_dir`.
- **`yanshi_agents::{common, traits}` removed.** The PDF parser, batch
  processor, file/output helpers and the `AgentApplication` /
  `FileAgent` / `BatchAgent` / `AgentConfig` traits served only the
  paper_* apps deleted earlier; nothing else used them, yet `pub use
  common::*` put them at the crate root. `yanshi-agents` no longer
  depends on `reqwest`.
- **`yanshi_tracing::StorageErrorPolicy::FailWorkflow` removed
  (`2125b36`).** It never failed a workflow: its `panic!` ran in the
  trace drain task after the workflow had finished, and the drain's
  `catch_unwind` caught it. Use `LogError`.

### Security

- **Skill workflow tools take only declared, plain-JSON inputs (Y8.5).**
  An agent could pass inputs a workflow never declared, which override
  that workflow's node parameters (e.g. a `file` node's `path`), or a
  value shaped like a serialized `File` that hands any host file to a
  node. Both are refused, on the CLI and the gateway.
- **`arxiv` node: HTTPS only, bounded download and unpack.** Search used
  plain `http://export.arxiv.org`; requests had no timeout; the e-print
  archive was read and gunzipped without a size cap (a gzip bomb could
  exhaust memory). Requests now refuse non-HTTPS URLs and time out at
  60 s; the archive is capped at 50 MiB and its decompressed source at
  200 MiB.

- `shell` / `script` children no longer inherit the host environment;
  only `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`, `TMPDIR`,
  `TERM` plus explicitly set vars pass through (`7e84236`).
- Approval-cache keys hash the full call (SHA-256) instead of a 64-char
  prefix, and `script` keys include `args` (`77c09bb`).
- Linux seccomp `!FsWrite` now also blocks x86_64 legacy write syscalls
  (`unlink`, `rename`, `mkdir`, `chmod`, …), `fchmod`/`fchown`/
  `fchmodat2` and the xattr family (`c510494`).

### Removed

- **StepFun image generation, image-to-image and image editing.** StepFun
  shut these endpoints down on 2026-10-10; the client code, dispatch
  entries and the hidden `yanshi audio clone` stub are gone, along with
  the stale StepFun CLI examples and docs.

### Fixed

- **Anthropic structured output on current Claude models.** A `JsonSchema`
  response format was emulated with a forced tool call, which Opus 5.5,
  Sonnet 5.5 and Fable 5.1 reject with a 400. Claude 4.5 and later now get
  `output_config.format` (also when streaming or with tools); older models
  keep the emulation. A forced `tool_choice` on those four models is now
  refused before sending.
- **Keg-only Homebrew renderers start in the sandbox.** A binary under
  `<root>/Cellar/` (e.g. `ffmpeg-full`, the Homebrew ffmpeg with libass)
  could not load libraries from `<root>/opt/` and aborted before `main`;
  the Homebrew root is now readable, as it already was for regular
  formulae.
- **`input_mapping` wins over a same-named workflow input (Y8.1).** Flow
  inputs used to override a node's mapped value. Order is now
  `input_mapping` > workflow inputs > static parameters
  (`docs/CONFIGURATION.md`).
- **A Map item skipped by `run_if` is not an error.** It was counted in
  the Map's failed indexes and logged as a warning.
- **`text_to_video` in workflow YAML.** The node had no schema entry, so
  no config-first workflow could use it.
- **The mock video vendor returns a playable MP4** (1 s, 64×36 H.264),
  so ffmpeg-based tests can use it.
- **Billed media nodes retry only what a retry can fix (Y2.5).** The
  modality nodes flattened every vendor error into an
  `AsyncExecutionError` string, which node retry (`max_retries`,
  `node_retry_policy`) classifies by words in the message: a 5xx was
  never retried, and a 4xx content rejection whose text mentioned
  "connection" or "timeout" was retried and billed again. Vendor errors
  now map by status: 5xx and network errors become `NetworkError`, 429
  becomes a rate-limit error and timeouts `TimeoutExceeded` (all
  retryable), while other 4xx, auth and parse errors become
  `NodeExecutionFailed`, which the default policy does not retry.
  `text_to_image` / `text_to_video` no longer re-wrap that error.
- **YAML `prompt` reaches text-to-image and text-to-video vendors.** The
  factory dropped it (and `negative_prompt` / `size`, `aspect_ratio` /
  `resolution` / `duration_seconds`), so YAML calls sent an empty
  prompt.
- **Streamed LLM calls now report token usage (Y2.1, COR-1).** Every
  ReAct turn streams, and the OpenAI-compatible stream carried no usage:
  the request never set `stream_options.include_usage`, the parser
  dropped the usage-only frame (`choices: []`), and the reader stopped
  at `finish_reason` before that frame arrived. So `LlmCallCompleted`
  had `total_tokens: None` and token / cost budgets never accrued. Now
  OpenAI, DeepSeek and DashScope requests ask for usage. GLM and
  MiniMax are skipped by model name: MiniMax was reported to reject the
  field, and GLM documents none. The OpenAI, Moonshot and StepFun
  readers continue to `[DONE]`, and Moonshot's choice-level usage is
  read. Regression fixtures are in `yanshi-llm/tests/sse_buffer_regression.rs`.
- **`POST /v1/runs/{id}:resume` (Y2.4).** Gateway runs now checkpoint
  after every node to `<run_dir>/checkpoint`; a `failed` / `cancelled`
  run resumes under the same id, re-running only unfinished nodes with
  the original inputs and cost limit, continuing the event `seq`.
  `RunRepo::reopen_for_resume` is the one atomic exit from a terminal
  status (and clears stale cancellation intents). `GET
  /v1/runs/{id}/resume-plan` defaults to the run's checkpoint dir.
- **`yanshi workflow run --checkpoint` and `yanshi workflow resume`
  (Y2.4).** `--checkpoint` checkpoints after every node to
  `<run_dir>/<run_id>/checkpoint/` and prints the resume command;
  `workflow resume <run_id> <file>` continues the run with its recorded
  inputs and settings, then prints the state and artifact table like
  `run` (and takes `--output` / `--output-dir`). `--checkpoint` is
  rejected with `--server`.
- **Resume replays the original run (Y2.4, D4).** `Flow::resume` used
  to run with empty inputs, the default (serial) config and no memory of
  spend. Checkpoints now carry the run's inputs, scheduling mode and
  concurrency, cost limit, run base dir and spend so far
  (`Checkpoint::metadata`, `checkpoint::run_metadata`), and the new
  `FlowExt::resume_with_config` resumes with them, under the concurrent
  scheduler when the run was concurrent. The resumed run's cost meter
  starts from the recorded spend, so a re-run node is billed once and
  the limit counts both attempts.
- **`token_budget` limits spend, not context size (Y2.2, COR-2).** ReAct
  compared the budget with the memory store's token count, which a
  summarising or trimming store keeps below its window. A 20 000-token
  budget over an 8 000-token window therefore never tripped. ReAct now
  adds each call's provider-reported usage to a running total and stops
  when that total passes the budget. The memory count is kept as a
  floor for providers that report no usage. `AgentLoopCheckpoint`
  gains `cumulative_tokens` (`#[serde(default)]`, so older checkpoints
  load with 0), and a resumed run keeps its spend.
- **`/metrics` recorder init is serialized.** A caller that lost the
  install race could scrape before the handle was stored and get an
  empty body, which made the `metrics_endpoint` tests flaky.

- **Helm chart no longer refuses `replicaCount > 1`.** The W4.2a guard
  (fail unless `allowMultiReplica=true`) and its "process-local, not yet
  fixed" warning outlived W4.2, which moved SSE fan-out, run admission
  and approval/cancellation onto Postgres. Both are removed and
  `allowMultiReplica` is ignored. Verified with two replicas on k3s:
  live SSE, cancel and the per-tenant concurrency cap all work across
  pods. `docs/DEPLOYMENT.md` now lists what stays per pod (the run
  artifact dir, the opt-in SQLite harness memory).

- **Cross-replica SSE: no duplicate events, and the stream ends.** A
  `GET /v1/runs/{id}/events` subscriber on a replica that was not
  executing the run received every event that predated its
  subscription twice (DB backfill, then again from the NOTIFY
  catch-up, which re-paged from seq 0 for the freshly created
  channel), and its stream never closed after the terminal event,
  because the live tail only stopped when a *further* event arrived
  and nobody closes that replica's channel. The live tail now drops
  events with `seq` at or below the last one sent and ends right
  after the terminal event. Found on a 2-replica k3s install.

- **The `Dockerfile` builds again.** The builder stage never installed
  `protoc`, which `yanshi-worker-proto`'s `build.rs` needs, so
  `docker build .` failed and the release `build-docker` job (which only
  runs on a tag) would have failed too. The builder now installs
  `protobuf-compiler`. Found by installing `charts/yanshi` on a real
  k3s cluster.

- **Retention sweep covers harness sessions and approval intents.** The
  cleanup sweep only reaped runs, events and artifacts, so
  `harness_sessions`, `harness_session_events`,
  `harness_loop_checkpoints` and `approval_decision_intents` grew
  forever. Terminal harness sessions finished more than
  `runs_retention_days` ago are now deleted (their events and
  checkpoints cascade), as are approval intents decided before that
  window. `CleanupReport` gains `harness_sessions_deleted` and
  `approval_intents_deleted`.

- **BM25 and MMR handle Chinese/Japanese text.** Terms were whitespace
  splits, so an unspaced sentence was one token and a Chinese query
  matched nothing short of the whole sentence; MMR's Jaccard similarity
  had the same blind spot. Runs of Han/kana now index as overlapping
  character bigrams (other words are unchanged), shared by BM25 —
  including skill `rag` knowledge — and MMR.

- Harness `:cancel` stops the running agent; a terminal run/session
  status can no longer be overwritten by a late executor write
  (`bf834e4`).
- `PlanExecuteAgent` stops on `DenyAndStop`; Handoff / Debate /
  Blackboard supervisors propagate a child's stop reason (`428aee6`).
- Gemini streaming requests `alt=sse`; streaming decoders for all
  providers buffer bytes per line, so multi-byte UTF-8 split across
  chunks no longer garbles (`ab18a49`).
- Dynamic workflows: a `run_if`-skipped step no longer fails its
  dependents or triggers a replan, and agent steps see their
  dependencies' outputs (`8dd342b`).
- Map / While sub-flows no longer emit workflow-level lifecycle events
  to the parent listener (`998e997`).
- `ContainerBackend` reports a stopped `container` / Podman service as
  unavailable instead of `Enforcing` (`e0917bf`).
- Cross-replica signals survive a dropped LISTEN connection: every
  NOTIFY listener resyncs from the DB after reconnecting (cancellation
  and approval intent tables, run/harness event streams), and approval
  NOTIFYs carry only ids, so a decision over ~8 KB still lands
  (`8b50883`, `bf71019`).
- Runs carry a renewed lease; runs of a dead replica are failed instead
  of holding the tenant's concurrency slot forever (migration
  `0011_run_leases`), and get a `workflow.failed` event.
- `GET /v1/runs/{id}/events` ends after the run's terminal event, and
  serves only the backfill for a run that already finished. Before, a
  stream served by a replica other than the run's, or opened after the
  run finished, never closed.
- Multi-agent supervisors share the parent's `timeout_ms` and
  `max_tool_calls` across their children instead of giving each child a
  full copy, so N children no longer spend N times the budget. Children
  running in parallel split the remaining tool calls; once either budget
  is spent the supervisor stops with `Timeout` / `MaxToolCalls`.
  `max_steps` and `token_budget` stay per child; `cost_limit_usd` too,
  since child results carry no cost.
- Debate participants and parallel blackboard children stop when their
  supervisor does. They ran as detached tasks, so they kept calling
  tools after an outer timeout or cancelled caller dropped the
  supervisor, or after a sibling's error ended it early.
- `yanshi harness run-flow --profile production` refuses workflows with
  tool-running nodes (`shell`, `file`, `http`, `mcp`, `plugin`, the agent
  types, including inside `map`/`while`). Their tools bypass harness
  approval, so the profile governed none of them while appearing to.
- The `text_to_video` tool rejects `poll_interval_secs` outside 1-60 and
  `max_wait_secs` outside 1-1800. Both are LLM-chosen and enforced only
  locally: `0` busy-polled the vendor and a huge wait parked the call.
- The CLI approval prompt no longer loses the answer typed after a
  timed-out prompt: the expired prompt's stdin read stayed pending and
  consumed it. The next prompt takes that read over, a late answer to
  the expired prompt is discarded, and concurrent prompts take turns.
- `yanshi-server` loads `.env` and applies its config's env overrides
  before starting the tokio runtime; it mutated the environment from
  inside `#[tokio::main]`, with worker threads already running.
  `serve::run` no longer sets env itself; call the new
  `apply_env_overrides` first.
- LLM retries wait at most 60s. A provider `Retry-After` beyond that
  now fails the call at once instead of parking it (an hour for
  `Retry-After: 3600`), and the exponential backoff is capped too.
- Harness sessions created with `runtime_kind: "plan_execute"` run a
  PlanExecute agent; they ran ReAct under the plan_execute label (only
  `:resume` honoured the kind). `POST /v1/harness/sessions` rejects other
  kinds, and `plan_execute` with `skill_name`, with 400.
- A node that reads a sibling through `input_mapping` or `run_if`
  without listing it in `dependencies` now runs after that sibling. In
  concurrent mode it could start first, silently dropping the input or
  evaluating the condition against no output. The reference stays
  optional: a skipped or output-less source is still just omitted.
- Parallel `map` items stop when their run does. They ran as detached
  tasks, so they kept executing after a timeout or cancelled caller
  dropped the run, and a retried node ran them a second time.
- A run's events are all persisted before it is marked finished. The
  executors slept 50ms and hoped the event writer had caught up; under
  load, clients that saw `succeeded` could read an incomplete event
  history.
- The MCP client sends `tools/call` at most once. A timeout or transport
  error can arrive after the server ran the tool, and the retry ran it
  again. `max_retries` still applies to the handshake and read-only
  requests.
- MCP transports drop server notifications once their queue is full
  instead of blocking. The same reader delivers responses and nothing
  drained the queue, so after 1024 notifications every request timed
  out.
- Harness `:cancel` stops a session running on another replica, not
  just its row.
- Distributed runs release their control-plane state when they end:
  every task, result and trace fragment used to stay in gateway memory
  for good, and a cancelled run's queued tasks stayed claimable.
- `yanshi workflow logs --follow` exits when the run is over. It
  recognised none of the terminal kinds the server emits
  (`workflow.completed` etc.) and reconnected forever.
- Harness sessions no longer leak shared DB pool connections from their
  inner runtime. On a busy pool that left sessions stuck at `running`
  (`cabb272`).
- All example workflows pass `yanshi workflow validate`; CI now
  validates them, tests `yanshi-tool` / `yanshi-tools-ai` / `xtask`, and
  fails DB legs when the test database is missing (`df8ca7f`,
  `bfb5607`).

### Added

- **Streamed reasoning text.** `StreamChunk` gains a `thinking` delta,
  filled from Anthropic `thinking_delta`, the OpenAI-compatible
  `reasoning_content` / `reasoning` deltas (DeepSeek, Qwen, GLM, …),
  Moonshot and StepFun; `collect_streaming_response` joins it into
  `LLMResponse.thinking`, which streaming previously always left empty.

- `figure`'s `domain: block` takes `language: d2` as well as `tikz`: the
  prompt carries D2 conventions and the source is checked against the
  spec's blocks and links (shapes `id: label`, connections `->` / `<-` /
  `<->`) before it renders. The other domains stay TikZ-only.
- **Skill `[[workflows]]` on the gateway (Y8.5).**
  `/v1/skills/{name}:run` hands the skill's workflows to the agent as
  tools, as `yanshi skill run` does (shared `yanshi_config::skill_workflows`).
  Sub-runs use the skill run's base dir, cancellation and concurrency;
  their files are registered as the run's artifacts, also when the agent
  then fails. Every template passes `GatewayPolicy::check_workflow` at
  submit (400 otherwise). Under the production profile each workflow
  call needs approval.
- **`render_video` can burn subtitles in (Y8.6).** `burn_subtitles: true`
  draws the captions into the picture with ffmpeg's `subtitles` filter
  (libass) instead of a toggleable track; the `.srt` is still written.
  Without libass the node fails and says so rather than falling back.
  `video_overview` gains the same input.
- **Chemistry and block-diagram figure domains (Y8.4).** `domain:
  chemistry` (chemfig; spec: molecules with formulas, checked for every
  named molecule and its heteroatom counts) and `domain: block` (TikZ;
  spec: blocks and directed links, every one checked). A figure that
  compiles but leaves something out goes back to the model.
- **Generated video shots in `video_overview` (Y8.3).** Off by default;
  with `video_model` set, the script may mark scenes as moving and those
  get a `text_to_video` clip instead of a still. `text_to_video` takes
  `model` / `aspect_ratio` / `duration_seconds` from its inputs as well.
- **Images in streamed Gemini replies (Y8.2).** `StreamChunk.media`
  carries `inlineData` parts and `collect_streaming_response` keeps them.
- **`figure` renders D2 (`language: d2`).** `d2 fig.d2 fig.svg` under the
  OS sandbox lays boxes and arrows out automatically, so the model gives
  no coordinates. d2's `fig.d2:<line>:<col>: ...` errors go back to the
  model with the work directory stripped. `yanshi doctor` lists `d2`;
  `slides` mentions it in `figure_languages`; CI's media-tools job
  installs d2 v0.9.0 and runs `figure_d2`. Mermaid is not offered:
  `mmdc`'s headless Chromium cannot start inside the OS sandbox
  (`docs/RFC_CONTENT_STUDIO.md` §5).
- **Modality nodes emit `FlowValue::File` artifacts (Y1.1,
  `docs/RFC_CONTENT_STUDIO.md` §4.1).** `tts` / `text_to_image` /
  `image_to_image` / `image_edit` / `text_to_video` write their bytes to
  `<run_dir>/artifacts/<node_id>/<n>.<ext>` and put only the path in the
  state pool; the executor installs the run directory as a task-local
  artifact root (`yanshi_core::run_context`) that Map items inherit from
  the parent run. `text_to_video` fetches the vendor's URL through a new
  `Text2VideoProvider::fetch_video_bytes` (Veo attaches its API key) with
  a 512 MiB cap. YAML `output_format: data_uri` keeps the old base64
  string. `vendor: mock` now serves every modality
  (`yanshi_llm::providers::MockMediaProvider`: valid WAV, 1×1 PNG, stub
  video), so modality success paths run in tests and CI without keys.
- **Consumers accept `FlowValue::File` (Y1.2).** The `file` node gains
  `copy` / `move` (source = the `content` input as a `File` value or
  path, destination = `path`, both under the sandbox policy) and
  `read_bytes` (hand an existing file downstream as a `File` value);
  `write` with a `File` as `content` copies the bytes. `copy` / `move` /
  `read_bytes` output `FlowValue::File` with a guessed MIME. Tera gains
  `{{ x | mime }}` and `{{ x | file_size }}` (a `File` still renders as
  its path). `image_understand` already read `File` inputs; it is now
  covered end to end: `text_to_image → image_understand` and
  `tts → file(copy)` run as config-first workflows under `vendor: mock`
  (`yanshi-config/tests/media_flow.rs`).
- **Run artifacts are registered and downloadable (Y1.3).** When a
  gateway run finishes, every `FlowValue::File` under the run base dir
  is recorded in `artifacts` with `size_bytes` and `sha256` (migration
  `0012`, streamed hashing). `GET /v1/runs/{id}/artifacts` lists them;
  `GET /v1/runs/{id}/artifacts/{artifact_id}` streams the file with
  `Content-Type` / `Content-Length` / `Content-Disposition`. Both are
  tenant-scoped (404 across tenants). Files outside the run base dir
  (a `file` node's `read_bytes` of a sandbox-allowed path) are logged and
  not registered, so the route never becomes a read primitive beyond the
  run's own output. Artifact files already live in the run dir the
  retention sweep removes; the row sweep is unchanged.
- **`slides` generator (Y6.1).** `examples/generators/slides.yml`:
  source (url / text / notebook chunks) → a deck plan (`llm` with
  `json_schema`: title, bullets, speaker notes and an optional figure
  per slide) → every figure rendered by the `figure` node in a Map →
  Markdown slides → a `.pptx` through pandoc, figures embedded as SVG
  so they stay vector, with speaker notes. `render_document` can now
  read the run's own artifacts (to embed figures) and takes a `theme`:
  a bare name resolved to a reference document in `YANSHI_THEMES_DIR`.
  Slides is available to `POST /v1/notebooks/{id}/generate` and to
  content-studio. Beamer / typst slide PDFs are not done yet.
- **Generate deliverables from a notebook (Y5.4).**
  `POST /v1/notebooks/{id}/generate {kind, params, model}` runs the
  report, mindmap or audio_overview generator (compiled into the
  gateway from `examples/generators/`) on the notebook's ready chunks.
  It goes through the normal run path, with admission, cost limit and
  artifact registration, and links the run's files to the notebook.
  `GET /v1/notebooks/{id}/artifacts` lists them, labelled by kind. The
  model comes from the request or `YANSHI_GENERATE_MODEL`. `POST
  /v1/runs` also gains `inputs` (values for declared inputs; undeclared
  names are a 400) and `model`, which applies inside map and while
  bodies too.
- **Citations point at notebook chunks (Y5.3).** The three generators
  take an optional `chunks` input
  (`[{chunk_id, source_id, content}]`, what a notebook provides).
  Passages are labelled `[chunk_id]` in the prompt and the models return
  that id:
  - report quotes become `{chunk_id, quote}`, and footnotes name the
    passage;
  - mind-map branches carry `chunk_id`;
  - audio lines carry `chunk_id`.
  The Deliverable's citations then hold the real `chunk_id` and its
  `source_id` (mind map and audio quote the passage itself). The `rag`
  node gains a `citations` output in the same
  `{source_id, chunk_id, span?, quote}` shape. Breaking: a report
  section's `quotes` are now objects, not strings.
- **Notebook sources: ingestion and search (Y5.2).** New routes:
  `POST /v1/notebooks`, `GET /v1/notebooks`, `GET /v1/notebooks/{id}`
  (with its sources), `POST /v1/notebooks/{id}/sources`, and
  `GET /v1/notebooks/{id}/search?q=`. A source is a `url` (fetched
  through the http tool's SSRF filter, since the URL is
  tenant-supplied), pasted `text`, or a text-like `file`
  (`.txt` / `.md` / `.html`, base64, up to 10 MiB). Ingestion runs in
  the background: extract text, chunk by paragraph, store in the new
  `source_chunks` table (migration `0014`), then mark the source ready
  or failed with the reason. Each notebook's BM25 index is built from
  those rows on first search, cached in process, and dropped when a
  source changes, so a restart costs one rebuild. `recording` sources
  are refused until transcription is wired in.
- **Notebooks and sources in the gateway database (Y5.1).** Migration
  `0013_notebooks.sql` adds `notebooks` (tenant, title), `sources`
  (notebook, kind `url` / `file` / `text` / `recording`, ingestion
  status, sha256) and `notebook_artifacts` (links to registered
  artifacts, with the deliverable kind). The new `NotebookRepo` and
  `SourceRepo` take the caller's tenant on every read, so another
  tenant's ids behave as missing. Sources and links can only attach to
  the tenant's own notebook and artifacts, and deleting a notebook
  cascades.
- **Content Studio: a planner skill over the generators (Y4.4).**
  `examples/applications/content-studio` is a ReAct skill whose tools
  are the three generator templates, declared in a new `[[workflows]]`
  manifest section. Each becomes a `WorkflowTool` with a JSON Schema
  derived from the template's `inputs` (`inputs_json_schema`). It
  returns only the generator's `deliverable`
  (`WorkflowTool::with_result_output`), not the whole state. `yanshi
  skill run` gains `--input KEY=VALUE` (facts appended to the request;
  `--message` becomes optional), `--output-dir` (each tool's files go to
  `<dir>/<tool>/<node_id>/<file>` with a manifest) and `--run-dir`.
  These flags are local-only, and `[[workflows]]` is not built
  server-side yet.
- **`audio_overview` generator and `render_audio` (Y4.3).**
  `examples/generators/audio_overview.yml` turns a source into a
  host/guest script (`llm` with `json_schema`, one turn per line). Each
  line is voiced in parallel by a Map of `tts` with the speaker's voice,
  then joined and loudness-normalised into one mp3. The new
  `render_audio` node does the join and two-pass EBU R128 normalisation
  through the shared sandbox, to `target_lufs` (default -16), as
  mp3 / wav / m4a. Every clip must resolve inside the run's artifact
  root, so a tenant-written workflow cannot read host files. Silence is
  left unnormalised. `tts` now takes `model`, `voice` and
  `input_template` as inputs, so one node can voice a whole script
  inside a Map. The ffmpeg-backed tests run in CI's `media-tools` job.
- **`mindmap` generator and `render_mindmap` (Y4.2).**
  `examples/generators/mindmap.yml` maps a source (`url` or `text`)
  into a labelled tree (`llm` with a recursive `json_schema`), then
  renders an SVG, a PNG (`render_svg`), an interactive markmap page, and
  a `kind: mindmap` Deliverable. The new in-process `render_mindmap`
  node lays a tree out left to right: parents centred on their
  children, columns sized to their widest label, one colour-blind-safe
  colour per top-level branch, CJK-aware label widths, and size / depth
  limits. It can emit `svg`, `html` (markmap via its CDN autoloader)
  or `md`. The existing `markmap` node needs a third-party API, so it
  is not used here.
- **`report` generator template (Y4.1).** `examples/generators/report.yml`
  is the first catalogue entry (RFC §3). The source is a `url`, fetched
  through the http node's SSRF filter, or pasted `text`. It is then
  outlined (`llm` with `json_schema`), sections are written in parallel
  (Map, each returning its supporting quotes), and the result is
  assembled into Markdown with footnoted quotes. Outputs are `.md`
  (`render_markdown format: md`), `.html`, and `.pdf` unless `pdf=false`.
  The run ends in a `deliverable` that parses as the new
  `yanshi_config::content::Deliverable` contract (kind, title,
  structured content, citations, files). Supporting changes:
  - The `llm` node accepts `json_schema` and returns the parsed object.
  - A workflow-level input now satisfies a node's required input in
    validation.
  - Generator outputs are checked against the contract type.
- **Fixes found while building it.** `--model` now reaches `llm` nodes
  inside map and while bodies. Model-backed nodes load the registry once
  per process (`Yanshi::ensure_init`) instead of on every call, which
  had reset rate-limit buckets. Template rendering errors now include
  Tera's actual cause.
- **`figure` language `python`: scientific drawing in a container
  (Y3.5).** The script must save `figure.svg`. It runs in the
  `code_exec` container tier: no network, read-only rootfs, only the
  work dir writable, 1 GiB / 120 CPU-s / 64 pids, and a 180 s timeout
  that kills the container. The image is built from
  `scripts/figure_python/Containerfile` (matplotlib, RDKit,
  dna_features_viewer, biopython for `Bio.Phylo` trees, networkx); the
  build fails if a drawing module cannot be imported.
  `YANSHI_FIGURE_PYTHON_IMAGE` overrides the tag. With no container
  engine the node refuses to run. A Python traceback is fed back to the
  model like any other render error. `docs/CONFIGURATION.md` gains a
  "Rendering Deliverables" section covering every render node.
- **`figure`: LLM-written diagrams, rendered and repaired (Y3.4).** A
  model writes figure source and a real renderer compiles it. A compile
  error goes back to the model in the next prompt, up to `max_attempts`
  (default 3). Each retry emits `node.progress` with
  `kind: "figure_retry"`, the new `WorkflowEvent::NodeProgress` that
  any node can now emit through `run_context::report_progress`.
  Languages:
  - `svg`: checked in-process with resvg.
  - `tikz`: compiled by `lualatex` with no shell escape, then
    `pdftocairo -svg`, so circuitikz, chemfig, pgfplots and tikz-cd
    come from TeX Live. Its font cache lives in the run directory.
  - `typst`: cached packages only, since there is no network.
  Outputs are `<id>_svg`, an optional `<id>_png`, `<id>_source` and
  `<id>_attempts`. The external renderers share
  `yanshi_nodes::common::sandboxed` with `render_document`, which
  provides read-only install prefixes and fonts, writes confined to a
  work dir, an argv vector and a timeout. `yanshi doctor` lists
  `lualatex` and `pdftocairo`, and also looks in `/Library/TeX/texbin`.
  Mermaid and d2 are not implemented yet.
- **`render_document`: Markdown → DOCX / PPTX / HTML / EPUB / PDF (Y3.3).**
  The new node calls pandoc; PDF goes through `--pdf-engine=typst`, a
  single binary with no TeX, and typst's font fallback renders CJK.
  pandoc runs under the platform OS sandbox. Its own and typst's install
  prefixes and the font directories are readable; only the node's
  artifact directory and the temp dir are writable. It is invoked with
  an argv vector, a scrubbed environment and a 300 s timeout. Raw HTML
  in the source is read as text. `yanshi doctor` lists `pandoc` and
  `typst` separately, since without typst only PDF is unavailable.
- **`media-tools` MCP server: ffmpeg as agent / workflow tools (Y3.2).**
  `examples/ecosystem/mcp/media-tools/server.py` is a standard-library
  Python stdio MCP server with `audio_concat`, `loudness_normalize`
  (two-pass EBU R128, reports measured input / output LUFS),
  `mux_video_audio`, `burn_subtitles` and `images_to_slideshow`. Every
  path must resolve inside `--root`, and ffmpeg runs from an argument
  vector with a timeout. `yanshi doctor` gains an **External tools**
  section (`ffmpeg`, `ffprobe`). The `#[ignore]`d end-to-end test
  `yanshi-mcp/tests/media_tools_e2e.rs` runs in the new `media-tools`
  CI job, which is part of the release gate. `podcast-mastering` now
  uses this server instead of the out-of-tree `phonon-mcp` binary.
- **In-process render nodes (Y3.1).** Three new workflow node types,
  each writing a `FlowValue::File` artifact:
  - `render_table` turns `rows` (JSON array of objects or arrays, or a
    JSON string) into CSV or XLSX (`format`, optional `columns`).
  - `render_svg` turns `svg` into a PNG with resvg and system fonts
    (`width` scales it; the canvas is capped at 8192² pixels).
  - `render_markdown` turns `markdown` into one self-contained HTML page
    with an inline stylesheet. Raw HTML in the source is escaped, since
    the page can be served from the gateway origin.
  Each also accepts its content as a `File` from an upstream node. The
  artifact writer moved to `yanshi_nodes::common::artifacts`, which
  `yanshi-nodes-ai` re-exports. The renderers sit behind a default-on
  `render` feature of `yanshi-nodes`.
- **Cost limits for workflows with billed media nodes (Y2.3).** `tts`,
  `text_to_image`, `image_to_image`, `image_edit` and `text_to_video`
  price each call against the pricing table: per image, per second of
  generated audio / video, per 1k TTS input characters. The table is the
  one the eval harness already used (`YANSHI_PRICING_TABLE` or
  `~/.yanshi/pricing.yml`), moved to `yanshi_llm::pricing`; the old
  `yanshi_agents::eval::pricing` path re-exports it. Cost is recorded on
  a per-run meter in `yanshi_core::run_context`, and Map / While items
  count toward their node. A workflow's top-level `cost_limit_usd`, or
  the same field on `POST /v1/runs`, refuses every node that has not
  started once spend has passed the limit (`YanshiError::CostLimitExceeded`).
  New events: `WorkflowEvent::NodeCostRecorded` (`node.cost`) and
  `CostLimitExceeded` (`cost.limit_exceeded`), both on SSE. Trace nodes
  carry `cost_usd`. `yanshi doctor` reports the pricing source. The
  modality *tools* (`yanshi-tools-ai`) do not record cost yet.
- **`yanshi workflow run` reports and exports artifacts (Y1.4).** A local
  run ends with an artifact table (node, output, MIME, size, path).
  `--output-dir <dir>` copies each artifact to `<dir>/<node_id>/<file>`
  and writes `<dir>/artifacts.json` (`run_id` plus, per file, `node_id`,
  `output_key`, `mime_type`, `size_bytes`, `sha256`, relative `path` and
  `source`). The CLI and the gateway pick artifacts by the same rule,
  now `yanshi_core::run_context::collect_file_outputs`. `--output-dir`
  is rejected with `--server`; use the Y1.3 routes there. The `--output`
  JSON is unchanged (still the state pool keyed by node id); the
  artifact list lives in the manifest rather than beside node ids it
  could collide with.

- **`cargo xtask refresh-live-models` now calls each default model.**
  A model listed in `/models` can still be retired: Google lists
  `gemini-2.5-flash` but answers 404 "no longer available to new
  users". After the listing check the probe sends one 16-token message
  and reports a 404 as `uncallable`; other statuses are printed only.
  The Google live-test default moved to `gemini-3.5-flash`.

- **GLM Text2Image (CogView) + MiniMax TTS (T2A) implementations, closes
  P-LLM2.3 (Batch 3, Moonshot skipped).** New `GlmImageProvider`
  (`yanshi-llm/src/providers/glm_images.rs`) implements
  `Text2ImageProvider` via `POST {base_url}/images/generations` — a
  clean synchronous request/response on the same host GLM's
  already-registered chat provider uses
  (`https://open.bigmodel.cn/api/paas/v4`), unlike Google/DashScope's
  separate media hosts. New `MiniMaxTtsProvider`
  (`yanshi-llm/src/providers/minimax_tts.rs`) implements `TtsProvider`
  via `POST /v1/t2a_v2`; MiniMax returns audio as a **hex-encoded**
  string (not base64/URL like every prior batch), decoded with a small
  hand-written hex parser rather than adding a `hex` crate dependency
  for one call site (confirmed via `Cargo.toml` grep that none exists
  in the workspace). Live-researched this session (Zhipu/BigModel docs,
  platform.minimax.io docs, Moonshot/Kimi docs) to scope Batch 3 per the
  TODO's own "决定基于各家实际开放的 API, 不为凑数实现" guidance:
  **Moonshot is skipped entirely** — no dedicated image/TTS/ASR REST
  endpoint exists on the Kimi API platform (Kimi-Audio is an
  open-source HuggingFace model, not a hosted endpoint); **GLM CogVideo
  (T2V) is deferred**, not skipped — it's real and async-task-shaped
  (a natural second `Text2VideoProvider` alongside Veo), but its exact
  poll-endpoint shape isn't yet confirmed to the precision every
  implemented batch this session required, so it's left as its own
  follow-up rather than guessed at. MiniMax's regional host ambiguity
  (`api.minimax.io` international vs. `api.minimaxi.com`/`.chat` China)
  is documented in a code comment — defaults to `api.minimaxi.com` for
  consistency with this crate's already-registered MiniMax chat
  provider, overridable per-model if wrong for a given deployment.
  New registry entries `glm-image` (`type: text_to_image`) and
  `speech-2.8-hd` (`type: tts`); wired into `modality_dispatch.rs`'s
  constructor tables + 2 new opt-in `llm-live.yml`-gated dispatcher
  tests. This closes out P-LLM2.3's full 3-batch structure (Batch 1:
  OpenAI; Batch 2: Google + DashScope; Batch 3: GLM + MiniMax).
- **DashScope Text2Image (Wan) + TTS (Qwen-TTS) implementations, ASR
  skipped (P-LLM2.3 Batch 2b, closes P-LLM2.3 Batch 2).** New
  `DashScopeMediaProvider` (`yanshi-llm/src/providers/dashscope_media.rs`)
  implements `Text2ImageProvider` and `TtsProvider`. Unlike every prior
  batch, DashScope's media APIs are natively async-task-shaped (`POST
  .../generation` with `X-DashScope-Async: enable` → `{task_id}`, then
  poll `GET /api/v1/tasks/{task_id}`) — `Text2ImageProvider::generate`
  wraps submit+poll-until-done internally (bounded at 60s, 2s interval)
  since the trait itself has no submit/poll surface (unlike
  `Text2VideoProvider`, deliberately built with one for Veo's
  minutes-long jobs — DashScope image gen is seconds-to-tens-of-seconds,
  so blocking internally is reasonable here). Two scope decisions made
  after live-researching Alibaba Cloud Model Studio's current docs this
  session: **CosyVoice (the TODO's originally-named TTS target) is
  WebSocket-only** with no REST variant documented, and this crate has
  no WebSocket infrastructure anywhere — implemented **Qwen-TTS**
  instead (`qwen-tts-2025-05-22`, already registered), a different but
  functionally-equivalent DashScope TTS model with a plain synchronous
  REST endpoint. Its response carries a presigned URL rather than inline
  bytes, so `synthesize()` issues a follow-up GET to satisfy
  `TtsResponse.audio: Vec<u8>`. **DashScope ASR (Paraformer/Fun-ASR) is
  not implemented** — a hard API constraint: its docs state raw
  bytes/binary streams aren't supported, only audio already at a public
  URL, and this codebase has no object-storage integration to produce
  one from `AsrRequest`'s raw `audio_data: Vec<u8>`. New registry entry
  `wan2.5-t2i-preview` (`type: text_to_image`); wired into
  `modality_dispatch.rs`'s constructor tables + 2 new opt-in
  `llm-live.yml`-gated dispatcher tests (the image-gen one uses a 90s
  bound, reflecting the real poll loop). This closes out P-LLM2.3 Batch
  2 (Google done in 2a, DashScope done in 2b); Batch 3 (GLM/MiniMax/
  Moonshot) remains open.
- **Google Text2Image + TTS implementations via `generateContent`
  (P-LLM2.3 Batch 2a).** New `GoogleMediaProvider`
  (`yanshi-llm/src/providers/google_media.rs`) implements both
  `Text2ImageProvider` and `TtsProvider` on one struct, sharing a
  `generate_content_media` core that POSTs
  `{base_url}/v1beta/models/{model}:generateContent` with
  `generationConfig.responseModalities: ["IMAGE"]` or `["AUDIO"]` and
  collects every `inlineData` part from the response — the same
  endpoint family `GoogleProvider` (chat) already speaks, just with
  different `generationConfig`. Two scope decisions made after live-
  researching Google's current docs this session: **Imagen (the
  `:predict` endpoint) is deprecated and shuts down 2026-08-17** (4 days
  out) with Google's own migration guidance pointing at `generateContent`
  instead, so Imagen is not implemented at all — Text2Image targets the
  already-registered `gemini-3.1-flash-image`, not the `imagen-*`
  entries. Also skips the beta `/v1beta/interactions` endpoint (a
  different request/response shape some newer doc pages default to)
  since Google's own TTS docs say to use `generateContent` for "stable
  production deployments." **No Google `AsrProvider`** in this batch —
  no dedicated transcription REST endpoint exists; audio input only goes
  through `generateContent` as multimodal chat content. Promotes
  `base64 = "0.21"` from `yanshi-llm`'s `[dev-dependencies]` to
  `[dependencies]` (same version — first production use, decoding the
  TTS response's base64 PCM audio). Wired into `modality_dispatch.rs`'s
  constructor tables + 2 new opt-in `llm-live.yml`-gated dispatcher
  tests. DashScope (Batch 2b) and Batch 3 (GLM/MiniMax/Moonshot) remain
  open.
- **OpenAI Text2Image + ImageEdit + TTS implementations (P-LLM2.3 Batch
  1).** OpenAI now implements 4 of the 6 modality traits (ASR was
  already covered by Whisper, P-LLM.5): new `OpenAIImageProvider`
  (`yanshi-llm/src/providers/openai_images.rs`) implements both
  `Text2ImageProvider` (`POST /images/generations`) and
  `ImageEditProvider` (`POST /images/edits`, multipart) on one struct —
  they share auth/base_url and return the same `{data: [...], created}`
  shape, mirroring how `StepFunSpecializedClient` implements multiple
  modality traits on one struct. New `OpenAITtsProvider`
  (`yanshi-llm/src/providers/openai_tts.rs`) implements `TtsProvider`
  (`POST /audio/speech`, raw-bytes response, defaults to MP3 unlike
  StepFun's WAV default). Live-verified against OpenAI's current API
  docs this session: `gpt-image-2` (added P-LLM2.0) generates but
  currently **rejects** the edit endpoint — only `gpt-image-1`/
  `-1.5`/`-1-mini`/`chatgpt-image-latest`/`dall-e-2` support edit — so a
  new `gpt-image-1` registry entry (`type: image_edit`) was added
  alongside the existing `gpt-image-2` (`type: text_to_image`,
  unchanged). Wired into `modality_dispatch.rs`'s constructor tables
  (P-LLM2.1's table-driven dispatch) and 2 new opt-in
  `llm-live.yml`-gated real-API tests (`openai_image_generation_via_modality_dispatcher`,
  `openai_tts_via_modality_dispatcher`) mirroring the existing Whisper
  dispatcher-test pattern. Batches 2 (Google/DashScope) and 3 (GLM/
  MiniMax/Moonshot) of P-LLM2.3 remain open.
- **`Text2VideoProvider` trait + Google Veo implementation (P-LLM2.2).**
  Adds the sixth modality trait, `Text2VideoProvider`
  (`yanshi-llm/src/providers/modality/text_to_video.rs`) — unlike the
  other five, an explicit async job API (`submit`/`poll`, plus a default
  `generate_and_wait` convenience method) since video generation takes
  minutes, not seconds. First implementation is `GoogleVeoClient`
  (`yanshi-llm/src/providers/google_veo.rs`), talking to Veo 3.1 via
  the plain Gemini API (not Vertex AI) — same `x-goog-api-key` auth this
  crate's existing `GoogleProvider` already uses, verified live against
  `ai.google.dev/gemini-api/docs/veo` this session (submit via
  `predictLongRunning`, poll via the returned operation name). DashScope
  was considered and passed over: its text-to-video equivalent needs a
  different auth scheme nothing in this codebase has wired yet, and
  TODOs.md's own P-LLM2.3 batch plan only lists DashScope for
  text-to-*image*, not video. Wired end-to-end: registry entry
  (`veo-3.1-generate-preview`) plus a fix for 3 **pre-existing** Veo
  registry entries (`veo-2.0-generate-001`, `veo-3.0-generate-preview`,
  `veo-3.0-fast-generate-preview`) that were mis-typed `type: chat` —
  latent since Text2Video dispatch didn't exist until now, so nothing
  exercised them — corrected to `type: text_to_video` +
  `accepts: [text]` + `supports_streaming: false`; dispatcher entry
  (`Yanshi::text2video_for`); new `TextToVideoNode`
  (`yanshi-nodes-ai`) mirroring `TextToImageNode`'s no-mock-fallback
  precedent; `type: text_to_video` YAML node registration in
  `yanshi-config`'s factory. Tests are pure-unit (request/response
  JSON shape, status-mapping logic) rather than HTTP-mocked, matching
  this crate's existing preference (confirmed via code search: `mockito`
  is a declared-but-unused dev-dependency, and the one full-HTTP test in
  this crate deliberately avoids it via a hand-rolled TCP listener) — an
  opt-in `llm-live.yml` real-call test is deferred to P-LLM2.3 per
  TODOs.md's own wording, and Veo is a paid-preview API with no test
  credential available regardless.
- **`yanshi-llm` modality dispatcher: table-driven construction +
  diagnosable unsupported-vendor errors (P-LLM2.1).**
  `modality_dispatch.rs` had 5 near-identical `match vendor.as_str() {
  "stepfun" | "step" => ..., _ => unsupported_vendor(...) }` blocks, one
  per modality (ASR/TTS/Text2Image/Image2Image/ImageEdit). Replaced with
  a `const` constructor table per modality (`ASR_PROVIDERS` etc., each
  `&[(&str, fn(&str, Option<String>) -> Result<Box<dyn Trait>>)]`) and a
  single generic `dispatch()` helper all 5 public entry points call.
  `UnsupportedProvider` errors are now more diagnosable: they name which
  modalities the vendor *does* already implement (derived from the same
  constructor tables, so it can't drift from what's actually wired up),
  e.g. `"openai (no TTS implementation yet — implements: ASR)"` instead
  of the old bare `"openai (no TTS implementation yet)"`.
- **LLM model registry split into per-vendor files + `accepts:` backfill +
  CLI `--type`/`--accepts` filters (P-LLM2.0).**
  `yanshi-llm/templates/default_models.yml` (3,165 lines, 198 models,
  skewed 46% DashScope vs. 3% OpenAI) is now nine
  `templates/models/<vendor>.yml` files plus a slimmed
  `default_models.yml` carrying only `providers:`/`defaults:`. Revives
  `VendorConfigManager`'s previously-dead directory-loading code: its
  per-vendor-file parser is now shared by a new compile-time
  9-`include_str!` bundler (`builtin_default_config`, replacing the old
  single-`include_str!` embed at all three call sites) and, via a new
  `LLMConfigSourceKind::UserModelsDir` variant, a real runtime path
  (`YANSHI_MODELS_CONFIG` or `~/.yanshi/` pointed at a
  `<dir>/models/*.yml` layout). Backfilled `accepts:` on the 133/198
  entries that were missing it, mechanically derived from each entry's
  pre-existing `supports_multimodal` flag (`true` → `[text, image]`,
  `false` → left unset, matching the existing type-default fallback) —
  verified no entry was mis-classified in either direction. Added
  current-generation OpenAI/Anthropic/Google model entries (verified
  live against each vendor's own docs this session): 4 Anthropic
  (`claude-fable-5`/`claude-opus-5`/`claude-sonnet-5`/
  `claude-haiku-4-5-20251001`), 7 OpenAI (3 chat, `gpt-image-2`,
  `gpt-4o-mini-tts`, 2 embedding), 5 Google (3 chat, 1 embedding, 1
  text-to-image, 1 TTS) — 214 models total, no existing entries lost.
  `yanshi llm models` gains `--type <ModelType>` (exact match) and
  `--accepts <InputType>` (contains-match) filters alongside the existing
  `--provider` substring filter; `PrintableModel`/`ModelInfo` both now
  carry `type`/`accepts` so there's something to filter on and to expose
  in `--format json-envelope` output.
- **`yanshi-mcp` client: protocol version verification + capability
  gating (W5.6, contained scope).** The client parsed the server's
  `initialize` response's `protocolVersion` but never compared it against
  what it sent — any value was silently accepted. It now rejects a
  mismatch before storing any session state
  (`MCPClient::connect`/`initialize`). Separately, `ServerCapabilities`
  is now stored typed (was round-tripped through `serde_json::Value` for
  no reason) and consulted before every `list_resources`/`read_resource`/
  `subscribe_resource`/`unsubscribe_resource`/`list_prompts`/`get_prompt`/
  `list_tools`/`call_tool` call — a server that never advertised e.g.
  `resources` during `initialize` now gets a clear client-side error
  instead of the request going out and (typically) getting a JSON-RPC
  "method not found" back. Fixes a related pre-existing bug uncovered
  while adding the version check: `connect()` left `is_connected()`
  reporting `true` after a failed `initialize()` instead of rolling back
  to disconnected. `server::STABLE_PROTOCOL_VERSION` and
  `protocol::types::MCP_PROTOCOL_VERSION` (previously two independently
  hand-typed copies of the same literal) are now one constant.
  Streamable HTTP transport and connection-level reconnect logic — the
  other two items under the former W5.6 TODO — are re-filed separately;
  this crate's own audit doc's "no JSON-RPC response-id correlation"
  finding was checked during this work and found to be already fixed
  (stale audit doc, not a live bug).
- **`GET /health/ready` now actually checks database connectivity (W5.3).**
  Previously all three health routes (`/health`, `/health/live`,
  `/health/ready`) unconditionally returned `{"status":"ok"}` regardless of
  whether the gateway could reach Postgres. `/health/ready` now runs a real
  `SELECT 1` against the primary pool via `yanshi-core`'s `HealthChecker`
  and returns `503` with per-check JSON when it fails; `/health`/
  `/health/live` stay unconditional `200`s per the standard Kubernetes
  liveness convention (must never depend on external dependencies).
- **`FlowExecutionConfig::resource_limits` — advisory state-pool size
  warnings (W5.3).** Opt-in `Option<ResourceLimits>` on `Flow`'s execution
  config; when set, `Flow` emits `WorkflowEvent::ResourceWarning` once the
  estimated state-pool size crosses the configured `max_state_size`. Purely
  advisory — no eviction, no rejection. Replaces an unwired
  `resource_manager`/`concurrency`/`state_monitor` stack that had zero real
  callers; the removed `state_monitor`'s LRU eviction model would have been
  unsafe for `Flow`'s state pool (a node's output can be a real dependency
  for any later node regardless of recency).
- **`cargo xtask check-changelog` wired into CI (W5.5).** The gate existed
  since P10.18.2 but was deliberately left unwired pending local-usage
  confidence; it now runs in `quality.yml` and gates `release-gate`.
- **`release.yml` now actually depends on `quality.yml`'s `release-gate`
  (W5.5).** Previously the two workflows both triggered directly on the
  same `v*` tag push and raced each other with zero coordination — a
  release could ship even with fmt/clippy/tests/audit all red, as long as
  the tag was pushed. GitHub Actions has no native cross-workflow
  `needs:`, so `release.yml` now triggers on `workflow_run` for
  `quality.yml` ("Quality")'s completion instead of directly on the tag
  push; a new leading `resolve-release-context` job is the single source
  of truth for whether to proceed, checking the upstream run's conclusion,
  trigger event, and ref, so a red `release-gate` can no longer produce a
  published release. Manual `workflow_dispatch` (with its `dry_run` flag)
  is unchanged as the human override path. **Verification caveat**: this
  cannot be exercised end-to-end without a real tag push, which was not
  done as part of this change — review the diff carefully before the next
  real tag cut; `workflow_dispatch`'s dry-run path validates the build
  matrix but does not exercise the new `workflow_run` gating branch.
- **`[workspace.dependencies]` for tokio/sqlx/uuid/reqwest, TLS backend
  unified on rustls (W5.5).** `tokio` had drifted to 2 version strings
  (`"1.0"` vs `"1.35"`) across 12 feature-set combinations; `uuid` similarly
  split `"1.0"`/`"1.6"`. `sqlx` (cli/db/server) declared a bare
  `runtime-tokio` with no explicit TLS backend, and 7 of 8 `reqwest`
  consumers used `default-tls` (native-tls) while `yanshi-memory`
  (sqlx) and `yanshi-skills` (reqwest) already forced rustls — the
  resolved dependency graph compiled **both** native-tls and rustls
  simultaneously. All 25 workspace members now reference these four
  dependencies via `{ workspace = true, features = [...] }`, and both
  sqlx and reqwest are unified on `runtime-tokio-rustls`/`rustls-tls`.
  `tokio-native-tls` is now fully absent from `Cargo.lock`; the one
  remaining `native-tls` entry is `ort`'s (ONNX Runtime, via `ureq`) model
  downloader, unrelated to this change and out of scope. No feature
  regressions — every crate's existing extra features (`multipart`,
  `stream`, `serde`, `sqlite`, etc.) are preserved on top of the new
  shared base.
- **`yanshi-ui`'s 6 hand-written unit-test files now actually run in CI
  (W5.5).** They previously only self-described as "run via `npx tsx
  <path>`" with nothing invoking them — `npm test` only ran `tsc --noEmit`.
  Added `vitest`; each file's manual assert/throw logic is unchanged, just
  wrapped in `describe`/`it` blocks so vitest can discover and run it
  (69 tests total). `npm test` now runs the real suite; the former
  typecheck-only script moved to `npm run typecheck`. A new `ui-tests` job
  in `quality.yml` runs both and gates `release-gate`.

- **`yanshi harness chat --approve cli` now works interactively (H.2.1).**
  Previously rejected at startup because the blocking `CliApprovalProvider` reads
  `std::io::stdin` while the REPL owns the async tokio stdin reader (the two
  race). A new channel-based `ChatApprovalProvider` forwards each approval
  request to the REPL loop, which prompts (`[y]es / [s]ession / [r]un / [n]o /
  [q]uit`) and reads the decision from the **same** shared stdin between
  servicing the turn — so interactive per-tool-call approval works in chat. The
  agent turn is driven in a `select!` that services approval requests until the
  turn finishes; a provider/channel error is a fail-closed deny.

- **`yanshi harness chat` gains a `/clear` command (H.5.1).** Clears the
  current session's conversation memory **in place** — keeping the session id —
  and rebuilds the runtime so the next turn starts fresh (vs. `/new`, which
  rotates to a new id). It clears the `--model` path's persistent
  `SqliteMemory` for the session; in `--skill` mode it prints a note that a
  skill configuring its own persistent memory keeps it separately (use `/new`
  for a guaranteed reset). A not-yet-created store is treated as already empty.

- **`PlanExecuteAgent::run_as_flow` — plan + run on the graph engine end-to-end
  (P-A2.2 follow-up).** Building on `compile_plan_to_flow`, the new
  `run_as_flow(context, runner)` plans with the LLM, compiles the plan to a
  `Flow`, and executes it on the deterministic graph engine via an injected
  `FlowRunner` — inheriting retry / checkpoint / timeout / tracing / replay and
  the plan's parallelism — instead of the hand-rolled sequential loop. It returns
  an `AgentRunResult` with an `Observe → Plan → per-node ToolCall/ToolResult →
  FinalAnswer` trace built from the flow's state pool (a failed node stops with
  `AgentStopReason::Error`), and honours the same cancellation / timeout /
  token + step + tool-call budgets as the sequential path. `PlanExecuteError`
  gains a `Flow(#[from] YanshiError)` variant. The legacy sequential
  `run_with_context` path is unchanged.

- **Dynamic-workflow plans can include agent steps (P-A2.2 follow-up).**
  `WorkflowPlanStep` gains a `kind` field (`tool` default | `agent`).
  `compile_plan_to_flow` compiles a `kind = "agent"` step to an `AgentNode`
  wrapping a `ReActAgent` built from the step's `params` (`model` required,
  `persona` optional, `prompt` → the agent's `message`); the agent shares the
  plan's tool registry, so it inherits the same sandbox / approval governance.
  Dependency wiring now uses each step's real output key — `result` for a tool
  step, `response` for an agent step — so a step depending on an agent receives
  its answer. The `DynamicWorkflowAgent` planner prompt teaches the LLM the agent
  step shape, so `yanshi workflow dynamic` can author them. Validation
  rejects an agent step missing `model`/`prompt` and a tool step with no `tool`.

- **`yanshi harness run-flow <workflow.yaml>` CLI (P-A2.2 follow-up).** Runs a
  config workflow (DAG) under harness governance: builds the `Flow` from YAML,
  runs it via `HarnessRuntime::run_flow` + `CoreFlowRunner`, and persists the
  Harness envelope (`session_started` runtime=`flow` → per-node `step_started` →
  `stopped`) as JSONL like an agent session — so a deterministic workflow gets
  the same observable / replayable governance stream. Flags: `--input key=value`
  (repeatable), `--model`, `--profile`, `--output text|json|stream-json|json-envelope`,
  `--workspace`, `--run-dir`, `--timeout-ms`, `--session`, `--max-concurrency`.
  A non-completed run exits non-zero. `--runtime flow` now parses for
  `HarnessRuntimeKind::Flow`. (Config-built nodes embed their own tools, so the
  CLI delivers the envelope + node events; tool-call approval governance is the
  programmatic `run_flow` + wrapped-registry path. A server route is a follow-up.)

- **Node-level `step_started` events in a governed Flow run (P-A2.2 follow-up).**
  `HarnessRuntime::run_flow` now instruments the flow with an
  `yanshi-graph::EventListener` that forwards each node's start (by id) onto a
  channel, drained concurrently with the run (biased `select!`) so a
  `step_started` event (`step_type = "node:<id>"`) interleaves in real time with
  the tool-call / approval events between `session_started` and `stopped`. The
  Flow's execution is observed through the existing `Flow::with_event_listener`
  seam — no executor coupling.

- **Harness governs a `Flow` run — orthogonal governance MVP (P-A2.2).** The
  harness governance shell now wraps a deterministic `yanshi-graph::Flow`
  run, not just an agent loop. New `HarnessRuntime::for_flow()` +
  `HarnessRuntime::run_flow(flow, runner, inputs, options)` brackets a
  `FlowRunner`-driven execution with the Harness envelope (`session_started`
  with the new `HarnessRuntimeKind::Flow` … `stopped`, classifying
  completed / failed / timed-out). Governance reaches the Flow's tool calls at
  the registry seam: when the Flow's node registry is wrapped via
  `wrap_registry` with a `HookConfig` sharing the runtime's seq counter + sinks,
  every `tool_call_requested` / `approval_requested` / `tool_call_completed`
  interleaves on the same monotonic event stream — an `AutoDeny` provider blocks
  a mutating tool inside a node and fails the run. `yanshi-harness` gains a
  downward dep on `yanshi-graph` (runtime→contract; the executor stays out
  via the injected `FlowRunner`). MVP scope: node-level step events, a CLI/server
  surface, and a `HarnessRuntimeKind::Flow` parser are follow-ups.

### Changed

- **CI now fails on rustdoc warnings.** A new `rustdoc` job in
  `quality.yml` runs `cargo doc --workspace --no-deps --all-features`
  with `RUSTDOCFLAGS=-D warnings` and is part of `release-gate`. Its
  first run caught intra-doc links in `yanshi-tools`' sandbox modules
  that only resolve on macOS; they are now plain code spans.

- **Three giant files split into modules, no behavior change (W5.4).**
  `yanshi-agents/src/react/agent.rs` (8,254 lines, one inherent
  `impl ReActAgent` block of 62 methods) split into
  `react/agent/{config,core,tool_dispatch,batch,memory,verification,
  checkpoint,prompt,turn_driven,support}.rs` by concern, plus a relocated
  `tests.rs`; `xtask/src/main.rs` (4,848 lines, 10 subcommands) split into
  `xtask/src/tasks/<task>.rs`, one file per subcommand;
  `yanshi-skills::SkillBuilder::build_with_admission` deduplicated
  against `build_core` (was a ~50-line copy that had drifted out of sync,
  silently missing `[memory.project]` support). All three are pure code
  movement — public API surfaces (`react::{ReActAgent, ReActConfig, ...}`,
  `SkillBuilder`'s 5 public methods, `cargo xtask <subcommand>`) unchanged.
- **`rag search` / `index` / `collections` CLI demoted under an `ops` group
  (P-A4.1b; RFC §9).** RAG's agent-facing retrieval path is now the `rag_search`
  tool a Skill exposes (P-A4.1/P-A4.2), so the direct vector-store commands move
  to `yanshi rag ops {search,index,collections}` — operator affordances for
  inspecting/populating the store out-of-band. `yanshi rag eval` stays
  top-level (it is the retriever quality gate). **Breaking** for scripts calling
  the old `yanshi rag search|index|collections` paths; flags are unchanged.

### Added

- **`Capability` contract + Skill lowering (P-A4.3; RFC §2).** The second
  load-bearing kernel trait lands: a *capability* is a packaged ability (persona
  + tools + knowledge + config) that `lower()`s to *tools + context* at the
  runtime boundary.
  - **`yanshi-agent-spi`** gains the `Capability` trait + `Lowered { tools,
    context }` + `CapabilityError` (`#[non_exhaustive]`). `Lowered.context`
    reuses the existing `ContextItem` (priority + token estimate) so a lowered
    capability slots straight into the harness/runtime prompt-budgeting
    machinery; `Lowered::merge` is how capabilities compose by flatten. Distinct
    from the OS-sandbox `yanshi_tools::Capability` enum (a process permission)
    — the two never share a position.
  - **`yanshi-skills`** implements it: `SkillCapability` bundles a manifest +
    skill dir and `lower()`s to the Skill's tools (built-in + MCP + the P-A4.2
    `rag_search` tool, via `build_registry`) plus its persona as one `Critical`
    context fragment. New direct L0 dep `yanshi-skills → yanshi-agent-spi`
    (capability → contract); `check-arch` stays OK.

  A surface can now hold a heterogeneous `Vec<Box<dyn Capability>>`, lower each,
  and merge the results into one registry + context bundle before handing a
  runtime its inputs — the runtime never knows a Skill was involved. Full
  surface adoption (replacing the direct `SkillBuilder::build` path) is a future
  step; the contract + impl + composition primitive land here.

- **Tiered Skill knowledge — `[[knowledge]]` gains a `backend` field (P-A4.2;
  RFC §9).** A skill's knowledge entries now choose how their content reaches
  the agent:
  - `backend = "files"` (default) — inline the file into the system prompt, as
    before. Omitting `backend` preserves the pre-P-A4.2 behaviour, so existing
    skills are unaffected.
  - `backend = "rag"` — index the file into an in-memory BM25 index and expose a
    single `rag_search` tool over it (built on the P-A4.1 `KnowledgeBackend` /
    `RagSearchTool`); the agent retrieves relevant passages on demand instead of
    carrying the whole corpus in its prompt. No vector DB / network — the skill's
    bundled files are indexed locally.

  `SkillBuilder` routes each entry independently: `build_persona` inlines only
  the files-tier entries, while a new `register_knowledge_backends` step indexes
  the rag-tier entries and registers the shared `rag_search` tool. `references/`
  documents remain files-tier. Documented in `docs/SKILLS.md`.

- **RAG repositioned onto the capability axis — `KnowledgeBackend` SPI +
  `rag_search` tool (P-A4.1; RFC §9).** RAG is no longer a top-level mode; it is
  a knowledge-retrieval *capability* behind a Skill's `knowledge:` declaration.
  - **`yanshi-store-spi`** gains the `KnowledgeBackend` trait + `KnowledgeChunk`
    / `KnowledgeError` (`#[non_exhaustive]`) — the kernel contract for "given a
    query, return relevant passages", living alongside `MemoryStore` so
    `yanshi-skills` and `yanshi-rag` agree on the shape without `skills`
    depending on the `rag` implementation crate.
  - **`yanshi-rag`** implements the SPI two ways: `Bm25KnowledgeBackend` (an
    in-memory BM25 keyword index — no network/embeddings, for the bundled-files
    tier) and `VectorStoreKnowledgeBackend` (semantic retrieval over any
    `VectorStore` via a `RetrievalStrategy`, for large/dynamic corpora). It also
    exposes `RagSearchTool` — a registry-installable `rag_search` `Tool`
    (idempotent, read-only) wrapping any `Arc<dyn KnowledgeBackend>`, so an agent
    retrieves on demand instead of having the whole corpus inlined into its
    prompt. `yanshi-rag` now depends downward on `yanshi-store-spi` +
    `yanshi-tools` (both L0); `check-arch` stays OK.

  Deferred to a P-A4.1b follow-up: demoting the user-facing `rag search` /
  `rag index` CLI to ops subcommands (pure UX regrouping, separable from the
  architectural repositioning). Skill `knowledge:` `backend:` wiring is P-A4.2,
  which sits on this contract.

- **`yanshi-nodes` split into a tool tier + `yanshi-nodes-ai` capability
  tier (P-A0.5 / P-A4 nodes decomposition; `docs/RFC_NODES_DECOMPOSITION.md`).**
  The monolithic node library is divided so the tool-tier crate carries zero
  capability dependencies:
  - **`yanshi-nodes`** now ships only the tool-tier `AsyncNode`s
    (`template`, `file`, `http`, `batch`, `conditional`, `arxiv`, `markmap`)
    and depends on just the IR + `yanshi-tools`. Its `yanshi-llm` /
    `yanshi-mcp` / `yanshi-rag` deps (and the `llm` / `mcp` / `rag`
    features) are gone; defaults stay `["http", "file", "template"]`.
  - **`yanshi-nodes-ai`** (new) carries the capability-backed adapters
    (`llm`, `asr`, `tts`, `text_to_image`, `image_to_image`, `image_understand`,
    `image_edit`, plus `mcp` / `rag` behind matching features) and depends on
    `yanshi-nodes` for the shared `common` / `error` modules.

  The workflow YAML `type:` strings are unchanged: the dispatch table in
  `yanshi-config::executor::factory` now imports tool nodes from
  `yanshi-nodes` and capability nodes from `yanshi-nodes-ai`. Consumers
  updated accordingly — `yanshi-cli` forwards `mcp` / `rag` to
  `yanshi-config`, and `yanshi-worker` keeps the tool tier while pulling
  `yanshi-nodes-ai` (with `mcp`) only for the `llm` / `mcp` payloads it
  dispatches. With the capability deps removed from the tool tier, the
  `nodes → llm`, `nodes → mcp`, and `nodes → rag` latent edges in
  `cargo xtask check-arch` are resolved and pruned from `ARCH_LATENT_EDGES`.

- **`agents → core` edge burned — the P-A contract-kernel allowlist is now EMPTY
  (0 tracked architectural violations).** The last tracked runtime-isolation
  edge is gone: `yanshi-agents` no longer depends on the `yanshi-core`
  executor. A new `FlowRunner` trait in `yanshi-graph` abstracts "execute a
  `Flow`, return the state pool"; `yanshi-core::CoreFlowRunner` is the
  executor-backed implementation. The agents that run embedded flows
  (`WorkflowTool`, `DynamicWorkflowAgent`) now take an injected
  `Arc<dyn FlowRunner>` and depend on the graph IR + the runner contract +
  `yanshi-async-util` instead of the core executor; the surface (CLI /
  examples) injects a `CoreFlowRunner`. `yanshi-core` stays an `agents`
  dev-dependency for the in-crate execution tests. With this, every
  runtime/surface-isolation edge the P-A track set out to burn —
  `agents → core`, `harness → agents` (P-A2.1), `server → cli` (P-A2.4),
  `worker → server` (P-A2.3) — is paid down, and `cargo xtask check-arch`
  reports **0 tracked** with an empty `ARCH_ALLOWLIST`.

- **P-A3 type-hardening (partial: 3.5–3.7).** Three of the five P-A3 items
  delivered:
  - **P-A3.6** — `yanshi harness chat`'s `/model` and `/skill` switch now
    commit on success: a failed switch (bad model id, unloadable skill) keeps
    the prior model + skill instead of leaving a dirty half-switched state.
  - **P-A3.5** — the duplicated char-bounded `truncate` in the trace `tui` /
    `replay` views is consolidated into one audited `format::truncate_chars`
    (the single UTF-8-safe truncation path; counts/slices by `char`, never
    panics on a multi-byte boundary).
  - **P-A3.7** — the contract error enums (`YanshiError`,
    `AgentRuntimeError`, `HarnessError`, `SchedulerError`) are now
    `#[non_exhaustive]` so error variants can be added without a breaking change
    for external consumers (one `_` arm added). Deliberately *not* applied to
    the event / stop-reason enums (would force ~600 in-workspace matches to add
    `_`, trading away internal exhaustiveness checking). P-A3.4 (seq write-order)
    and P-A3.3 (session type-state) are deferred — the seq value is already
    atomic/correct, and the `LoopSession` trait object can't carry compile-time
    state.

- **`yanshi-worker-proto` — shared worker control-plane contract; `worker →
  server` edge burned** (P-A2.3). The worker protocol surface moved out of
  `yanshi-server` into a new `yanshi-worker-proto` crate: the
  `WorkerProtocol` trait + wire types (`WorkerTask` / `WorkerTaskResult` /
  `WorkerHeartbeat` / `WorkerId` / `WorkerCapabilities` / `ClaimHints` /
  `WorkerTraceEvent` / `WorkerTransport`), `SchedulerError`,
  `InMemoryWorkerProtocol`, `NodeExecutionPayload`, the gRPC client
  (`GrpcWorkerProtocol`) + the proto↔domain conversions + traceparent helpers,
  and the `worker.proto` codegen (`build.rs` + `tonic-build`, generating the
  `pb` messages). `yanshi-worker` now depends on this shared contract instead
  of the gateway crate; `yanshi-server` keeps the control plane
  (`WorkerControlPlane`, the distributed scheduler) and the gRPC *server*
  (`WorkerControlServer` + the `WorkerControl` trait), importing the contract +
  `pb` + conversions from `yanshi-worker-proto` and re-exporting them under
  the original `scheduler::*` paths (so the control plane, the server-side
  service, and all the gRPC round-trip tests are unchanged). `yanshi-server`
  stays a worker dev-dependency for the integration tests. With the last
  surface-isolation edge gone, `cargo xtask check-arch` now reports **1 tracked
  edge** — only `agents → core` (runtime-isolation, the final P-A repoint)
  remains. Also fixes a stale `build_report` doc comment flagged by
  `clippy::empty_line_after_doc_comments`.

- **`server → cli` dependency edge burned** (P-A2.4, step 2 — complete). The
  doctor diagnostics report builder (`build_report`, `DoctorProfile`,
  `DoctorReport` + the report model, `print_text_report`) moved from
  `yanshi-cli` into `yanshi_config::diagnostics`, so `yanshi-server`'s
  `/v1/diagnostics` route depends on the shared crate. The doctor command's
  `execute` (which couples to the CLI's JSON envelope) and the top-level
  `mcp.toml` probe (which reads the CLI's `McpConfigFile`) stay in the CLI;
  `build_report` now *receives* the top-level MCP probe result as a parameter
  (the server passes an empty one). With the last usage repointed,
  **`yanshi-server` no longer depends on `yanshi-cli` at all** — the
  `yanshi xtask check-arch` allowlist drops the `server → cli` entry (now 2
  tracked edges: `agents → core`, `worker → server`). The CLI re-exports the
  diagnostics surface under the original `doctor::*` paths, so the `doctor`
  command is unchanged.

- **`yanshi-config` — shared workflow-assembly crate** (P-A2.4, step 1). The
  YAML workflow config schema (`config::v2::{FlowDefinitionV2, NodeDefinitionV2}`,
  `config::schema`) and the executor that compiles it into an `yanshi-core`
  `Flow` (`executor::build_flow_from_yaml` + the node factories) moved out of
  `yanshi-cli` into a new shared crate, so `yanshi-server` assembles and
  schedules workflows by depending on the shared crate instead of the CLI binary
  crate. `yanshi-cli` re-exports both modules under their original
  `yanshi_cli::{config, executor}` paths (consumers unchanged); the server's
  `runs.rs` (`build_flow_from_yaml`) and `scheduler::distributed` (the V2 schema
  types) now import from `yanshi-config`. The crate mirrors the CLI's
  `plugin` / `rag` / `mcp` feature flags. This is the first of two steps toward
  burning the `server → cli` dependency edge; the remaining usage is the doctor
  diagnostics `build_report`, which moves in a follow-up (it needs a careful
  split because the doctor command's `execute` couples to the CLI's JSON
  envelope).

- **`race_with_limits` consolidation completed for the ReAct hot path**
  (P-A3.2 follow-up). The batch-dispatch racing sites — the concurrent
  `join_all` group and the serial per-call loop — now also delegate to
  `race_with_limits`, so the `ReActAgent` no longer hand-writes any
  timeout/cancellation `select!` matrix (all four sites: LLM call, single tool
  call, concurrent batch, serial batch). Four batch racing characterization
  tests (concurrent/serial × timeout/cancel) were added first and pass both
  before and after the repoint, proving it behaviour-preserving; the test
  `SleepingTool` gained an idempotency switch to drive both batch paths.

- **`async-util::race_with_limits` — shared timeout/cancellation racing
  combinator** (P-A3.2). A single `race_with_limits(fut, remaining, cancel) ->
  RaceOutcome::{Completed, TimedOut, Cancelled}` replaces the hand-written
  four-arm `(Option<Duration>, Option<CancelSignal>)` match + nested
  `tokio::select!` blocks that every call site copied. The `ReActAgent`'s
  LLM-call and tool-call sites now delegate to it, so the timeout- and
  cancel-handling branches (which the old matrix duplicated across the
  both/timeout-only/cancel-only arms) are written once per call site. Behaviour
  is unchanged — proven by the P-A3.1 racing characterization tests, which pass
  against the refactored code — plus six combinator unit tests. Re-exported as
  `yanshi_core::race_with_limits` / `RaceOutcome`. Scope note: the two
  single-call sites are repointed here; the batch-dispatch sites and the
  `yanshi-core` shutdown paths still carry their own `select!` blocks and are
  a follow-up (they need their own racing-path coverage first).

- **ReAct loop timeout/cancellation racing-path characterization tests**
  (P-A3.1). Pins the runtime behaviour when a wall-clock deadline or a
  cancellation token wins a race against an *in-flight* call — the four
  `select!` arms in `run_turn_llm_call` / `run_turn_tool_call` that P-A3.2 will
  consolidate into `async-util::race_with_limits`. Four deterministic tests
  (LLM-call timeout, LLM-call cancellation, tool-call timeout, tool-call
  cancellation) using a never-completing slow operation so outcomes don't depend
  on scheduler timing. Supporting change: the Mock LLM provider now honours an
  `YANSHI_MOCK_DELAY_MS` env var, so env-driven tests (which build the mock
  through the model registry rather than the `with_delay` builder) can simulate
  a slow round-trip.
- **Harness governance contracts moved into the kernel** (P-A1.1 step 2/2). The
  Harness wire/protocol surface — `HarnessEvent` (+ all payload types),
  `ApprovalRequest` / `ApprovalDecision` / `ApprovalProvider` (+ risk / scope /
  outcome enums), `PreToolHook` / `PostToolHook`, the `HarnessEventSink` trait,
  `ContextProvider` (+ `HarnessContext` / `HarnessProfile` / `HarnessRuntimeKind`
  / `ContextItem` / `ContextPriority`), and the shared `HarnessError` — moved out
  of `yanshi-harness` into a new `yanshi-agent-spi::harness` module, so the
  operations crates can depend on the contract in the kernel (L0) rather than the
  `yanshi-harness` runtime (L3). Faithful strangler-fig: the harness `error` /
  `approval` / `context` / `hooks` / `event` modules became `pub use` re-export
  shims and `persistence` keeps the concrete sinks (`JsonlEventSink` /
  `StdoutEventSink` / `InMemoryEventSink` / `SinkChain`) while re-exporting the
  trait — so every consumer (server / cli) compiles unchanged. `yanshi-agent-spi`
  gained **no new dependencies**. Redaction (`params_summary` →
  `yanshi-tracing`) intentionally stays in `yanshi-harness` (the contract
  types hold already-redacted strings), so this step does not yet burn the
  `harness → tracing` edge. The `Capability` / `Lowered` traits (RFC §2) are split
  out to land with their consumer in P-A4.3.

- **Contract-kernel architecture track (`P-A`) — guardrails landed.** The
  contract-kernel design (`docs/RFC_CRATE_ARCHITECTURE.md`) is validated by a
  dependency-graph-grounded, per-edge architecture-lens evaluation
  (`docs/ARCHITECTURE_EVALUATION_2026-06-20.md`; verdict: direction confirmed,
  six refinements R1–R6). `cargo xtask check-arch` now runs in the Quality CI
  workflow as a required `release-gate` job (P-A0.3), and additionally reports
  the full **latent target-state edge map** — the 16-edge repoint checklist for
  the kernel migration, code-tracked with a self-maintaining staleness guard so
  it cannot rot (P-A0.4). No shipped-crate behavior change; this is internal
  architecture tooling + docs.

- **`yanshi-value` contract leaf crate** (P-A1.5, first kernel extraction).
  `FlowValue` + its serde conversions moved out of `yanshi-core` into a new
  zero-internal-dependency `yanshi-value` crate. `yanshi-core` re-exports
  it as `yanshi_core::value` / `yanshi_core::FlowValue`, so every existing
  consumer compiles unchanged. This is the universal kernel leaf and a
  prerequisite of the upcoming `yanshi-graph` split (evaluation R1).
- **`yanshi-graph` IR crate — leaf extraction** (P-A1.3 step 1/2). The pure
  IR leaf modules — `error` (`YanshiError`), `async_node` (`AsyncNode`),
  `node`, `expr` — moved out of `yanshi-core` into the new `yanshi-graph`
  crate (depends only on `yanshi-value`), beginning the IR ≠ executor split
  (RFC §5). `yanshi-core` re-exports them under their original paths; all
  consumers compile unchanged.
- **`Flow` IR moved to `yanshi-graph`; executor is now `FlowExt`** (P-A1.3
  step 2/2, completes the IR ≠ executor split). `Flow` / `GraphNode` / `NodeType`
  (+ pure builders and accessors) now live in `yanshi-graph`; the run / resume
  / scheduling logic stays in `yanshi-core` behind a new **`FlowExt`** trait
  (`use yanshi_core::FlowExt;` to run a flow) backed by an internal
  `FlowExecutor`. The `Flow` struct holds checkpoint *config* (data) rather than
  a live manager. `yanshi_core::{Flow, GraphNode, NodeType, FlowExt}` are
  re-exported, so the only caller-visible change is needing `FlowExt` in scope to
  call `flow.run()` / `with_checkpointing()` / etc. A runtime can now construct a
  `Flow` by depending on `yanshi-graph` alone — the dynamic-workflow
  prerequisite.

- **`yanshi-store-spi` storage-contract crate** (P-A1.2). The storage
  *contracts* — `MemoryStore`, `Message` / `Role` / `TokenCounter`, and the shared
  `MemoryError` — moved out of `yanshi-memory` into a new `yanshi-store-spi`
  crate. The concrete stores (`SessionMemory`, `SqliteMemory`, `SemanticMemory`, …)
  stay in `yanshi-memory`, which re-exports everything under its original
  `yanshi_memory::*` paths — no consumer changes. This gives `Message` a
  contract home so the upcoming `yanshi-agent-spi` can depend on it without
  depending on the `memory` implementation crate. (The `EmbeddingProvider`
  contract — evaluation R6 — is a follow-up pending error-surface unification.)

- **`yanshi-agent-spi` agent-runtime contract crate** (P-A1.1, runtime
  contracts). The `AgentRuntime` trait + the structured `AgentEvent` / `AgentStep`
  / `AgentContext` / `RuntimeLimits` / cancellation + event/memory hook contracts
  moved out of `yanshi-agents` into a new `yanshi-agent-spi` crate (depends
  on `store-spi` for `Message` + `tools`). The concrete runtimes (`ReActAgent`,
  `PlanExecuteAgent`, supervisors) stay in `yanshi-agents`, which re-exports
  everything under its original `yanshi_agents::runtime` path — no consumer
  changes. This lets `yanshi-harness` later govern a runtime via the contract
  instead of the `agents` impl crate (the P-A2.1 target). (The harness-side event
  / approval / hook contracts and the `Capability` / `Lowered` traits are a
  follow-up sub-step; `agent-spi → llm` for `LlmTraceContext` is transitional,
  pending a trace-context contract per evaluation R6.)

- **`yanshi-async-util` reliability crate** (P-A1.4, completes the kernel
  crates). The retry (`RetryPolicy`/`RetryContext`/`RetryStrategy`) and timeout
  combinators moved out of `yanshi-core` into a new `yanshi-async-util`
  crate, so the executor and the agent loop can share one implementation instead
  of duplicating it (the de-dup against `yanshi-agents` is P-A3.2).
  `yanshi-core` re-exports both under their original `yanshi_core::{retry,
  timeout}` paths — no consumer changes; `retry_executor` stays in core. With
  this, all five new kernel crates exist (`value`, `graph`, `store-spi`,
  `agent-spi`, `async-util`).

- **Dynamic-workflow vertical-slice spike** (P-A1.6) — the payoff of the
  contract-kernel migration. New `yanshi-agents/examples/dynamic_workflow_spike.rs`
  demonstrates a runtime *generating* a `Flow` from the `yanshi-graph` IR
  (shape decided at runtime, not compile time) and `yanshi-core` executing it
  via `FlowExt` — the two meeting only through the `graph` contract. Wired into
  the `examples-smoke` CI gate so it stays green. This proves the kernel can
  carry dynamic workflows; P-A4 productionizes it (`PlanExecuteAgent` emits a
  real `Flow`).

- **`harness` no longer depends on the `agents` impl crate** (P-A2.1, first
  architecture violation burned down). The turn-driven contracts (`TurnDrivenRuntime`
  / `LoopSession` / `TurnProgress`) moved from `yanshi-agents` into
  `yanshi-agent-spi`; `yanshi-harness` now depends on the `agent-spi`
  contract and the concrete runtime (`ReActAgent`) is injected (the smoke test
  keeps `agents` as a dev-dependency). `check-arch`'s tracked-violation allowlist
  shrank 4 -> 3.

- **Dynamic workflow productized — declarative plan -> `Flow` compiler** (P-A4.4).
  New `yanshi_agents::dynamic::compile_plan_to_flow` turns a `WorkflowPlan`
  (the JSON an LLM emits: steps of `{id, tool, params, depends_on}`) into an
  executable `Flow` of real tool calls — `depends_on` becomes graph dependencies,
  so independent steps run concurrently and dependents receive their inputs. This
  is the productized form of the P-A1.6 spike: the agent collapses its intent into
  one deterministic, replayable artifact instead of looping step-by-step. New
  `dynamic_workflow_plan` example (JSON plan -> parallel execution), wired into the
  examples-smoke CI gate; unit tests cover the diamond DAG + validation.

- **`DynamicWorkflowAgent` — LLM plans, the engine executes** (P-A4.4). An agent
  that makes one up-front LLM call to generate a `WorkflowPlan` (given a goal +
  the available tools), then compiles it to a `Flow` and runs it concurrently —
  closing the dynamic-workflow loop end to end. `plan()` returns the LLM-produced
  plan; `run()` plans + compiles + executes. Tested against a mock model that
  emits a parallel plan.

- **`yanshi workflow dynamic` — governed dynamic-workflow CLI surface**
  (P-A4.5). Exposes the dynamic-workflow paradigm on the CLI: an LLM authors a
  plan for `--goal`, which compiles to a `Flow` and executes concurrently. Because
  the plan is LLM-authored then executed, tool access is governed — the built-in
  tools (`FileTool` + `HttpTool`; shell is never registered) carry a restrictive
  `SandboxPolicy`, so file paths and HTTP domains must be granted explicitly via
  `--allow-path` / `--allow-domain`; `--dry-run` prints the plan without running
  any tool; and `--approve cli|auto-allow|auto-deny` routes every call through the
  Harness `wrap_registry` approval pipeline (the planner and compiler share the
  same wrapped `Arc<ToolRegistry>`, so governance is not bypassed). Unit tests
  cover the policy/approval/render helpers; integration tests assert dry-run does
  not execute, an ungranted path is sandbox-denied (non-zero exit), and a granted
  path writes successfully.

### Changed

- `RoadMap.md` reframed around the four execution paradigms (static DAG / native
  loop / harness / dynamic workflow) converging on the contract kernel, replacing
  the pre-kernel "two first-class paths" framing.

### Fixed

- **ReAct hot path no longer panics on multi-byte UTF-8 observations.**
  `yanshi-agents`'s ReAct loop truncated tool observations and
  memory-summary content by byte offset (`&observation[..observation.len().min(200)]`,
  `content.truncate(160)`), which panics whenever the byte offset lands
  mid-codepoint — trivially triggered by any non-ASCII tool output (e.g.
  CJK text) long enough to cross the 200-byte mark. A new
  `truncate_str_at_char_boundary` helper walks back to the nearest valid
  `char` boundary before truncating; all three call sites in
  `yanshi-agents/src/react/agent.rs` now go through it.

- **Benign `run_if`-skipped nodes no longer fail the whole workflow.**
  Both the serial and concurrent `Flow` execution paths determined
  overall workflow success with `any(Result::is_err)`, which counts a
  skipped node (`Err(YanshiError::NodeSkipped)`) as a genuine
  failure — a workflow where a conditional branch legitimately didn't
  run would be reported and checkpointed as `Failed` instead of
  `Completed`. New `is_genuine_failure()` / `representative_failure_message()`
  helpers now treat `NodeSkipped` as benign in both scheduler paths.

### Security

- **`file` node no longer defaults to unrestricted filesystem access.**
  `FileNode::default()` used `SandboxPolicy::permissive()`, so any
  absolute path without `..` bypassed the only real guard — if `path`
  came from `input_mapping` (e.g. an LLM's own output), that was
  equivalent to arbitrary file read/write. `FileNode::default()` now
  uses `SandboxPolicy::default()` (empty `allowed_paths` denies
  everything), and the `file` node's YAML schema now requires
  `allowed_paths` — `yanshi workflow validate` rejects a `file` node
  missing it at build time instead of failing (or silently succeeding)
  at run time. The distributed worker path, which previously ignored
  the `allowed_paths` parameter and always ran with the permissive
  default, now builds the same enforced policy from the dispatched
  node payload.

- **Checkpoint and run-directory paths are now sanitized against
  traversal.** `yanshi-core`'s checkpoint path construction joined
  an untrusted `workflow_id` (which can originate from a
  caller-supplied run id in server scenarios) and `node_id` (from a
  workflow author's YAML) directly into filesystem paths. Because
  `..` is resolved at the OS level, a `workflow_id` of `".."` made
  `delete_all_checkpoints` recursively delete the checkpoint
  directory's *parent*. A new `is_safe_path_component` check (rejecting
  empty strings, `.`, `..`, and any path separator) now gates every
  checkpoint read/write/delete and `Flow`'s `run_dir` / per-node output
  persistence before any filesystem call is made.

- **Worker gRPC's `submit_task` RPC now enforces admission control.**
  `AuthenticatedGrpcWorkerService::submit_task` accepted any caller
  unconditionally, on the assumption that it sat behind the same HTTP
  bearer-auth middleware as the rest of the API — but
  `serve_worker_grpc` binds its own independent socket with no such
  middleware, so anyone who could reach the port could submit arbitrary
  `WorkerTask`s. A new `WorkerAdmissionPolicy::check_any` /
  `AuthenticatedControlPlane::admit_any` now gates the call: unconfigured
  deployments keep today's open dev/local default, while any configured
  PSK or JWT allowlist makes the RPC fail-closed.

### Fixed

- **Serial and concurrent `Flow` execution now agree on terminal
  status.** In the serial path, a `gather_inputs` failure previously
  propagated through `?` and aborted `execute_from_inputs` outright —
  no `WorkflowFailed` event, no final checkpoint status (it stayed
  `Running` forever), and a hard `Err` instead of the per-node result
  shape the concurrent path already produced. Serial execution is now
  restructured into the same two-stage shape as concurrent, and it now
  honors `fail_fast` / `continue_on_skip`, which it previously ignored
  entirely. Two related bugs were fixed alongside it: `yanshi-server`'s
  run-status determination counted benign skips as run failures (same
  root cause as the DAG skip-vs-failure fix above), and
  `yanshi workflow run`'s CLI retry loop never inspected the
  returned state pool for embedded per-node failures, so it could
  print "Workflow completed" and exit 0 even when a node had genuinely
  failed.

- **DAG checkpoint resume is now sound for concurrent runs.**
  `execute_from_checkpoint` decided what to skip by locating
  `last_completed_node`'s position in topological order and treating
  everything before it as done — an assumption that only holds for
  serial runs. Under concurrent execution a checkpoint only records
  "the most recently completed node," not "everything earlier in
  topological order finished"; in the worst case, resume could
  misjudge an entire workflow as complete and permanently drop
  unexecuted nodes. Resume now looks up each node's actual recorded
  state in the restored state pool instead of relying on topological
  position. This required checkpoints to round-trip `NodeSkipped` and
  `NodePartialExecutionFailed` faithfully — the latter previously used
  the same on-disk shape as a plain `Ok`, meaning a partial failure was
  silently upgraded to success on resume.

- **Fixed a missed-wakeup race in `AgentCancellationToken`.**
  `cancelled()` checked `is_cancelled()` before awaiting
  `Notify::notified()`; a `cancel()` landing in the gap between the two
  could be missed entirely, leaving `cancelled()` able to hang forever
  even though the token had already been cancelled. The check now
  follows `tokio::sync::Notify`'s documented subscribe-then-check
  pattern: `notified()` is constructed before `is_cancelled()` is
  checked, closing the gap.

- **`Flow`'s timeout and retry now reach the execution engine itself.**
  `yanshi-core`'s `timeout` / `retry_executor` modules previously had
  no caller inside `Flow`'s own execution loop, so any retries a
  workflow performed were invisible to `WorkflowEvent` / trace replay,
  and the long-defined `WorkflowEvent::RetryAttempt` event was never
  actually constructed anywhere. `FlowExecutionConfig` gains optional
  `node_timeout_ms` / `node_retry_policy` fields (both `None` by
  default — zero behavior change for existing workflows); when set,
  both scheduler paths now wrap node execution with a timeout and a
  retry loop that emits a real `WorkflowEvent::RetryAttempt` before
  each retry.

- **LLM calls now retry transient 429/5xx errors with backoff.**
  `yanshi-llm` defined `max_retries` / `retry_delay_ms` configuration
  but never consumed it — any rate limit or transient server error was
  thrown straight to the caller. A new `LLMError::is_retryable()`
  classifies rate-limit / service-unavailable / timeout / 429/5xx
  errors as retryable (4xx parameter/auth errors are not); all three
  execution paths now retry through exponential backoff driven by the
  model registry's resolved defaults. Only failures before a request
  completes (or before a streaming connection opens) are retried —
  mid-stream failures after chunks have already reached the caller are
  out of scope.

- **Project-level agent memory is now wired end to end, and a
  `SemanticMemory` ID-corruption bug is fixed.**
  `yanshi-memory::ProjectMemoryStore` had a complete storage layer
  but zero runtime consumers. `yanshi harness run` / `harness chat`
  now open a per-skill `SqliteProjectMemoryStore` when a skill declares
  `[memory.project]`, so facts the agent extracts from
  `shell`/`script`/`code_exec` tool calls persist across sessions for
  that project. Separately, `SemanticMemory::row_to_message` silently
  generated a fresh random ID when it encountered a corrupt stored
  UUID instead of erroring; it now returns `MemoryError::StorageError`
  like its `sqlite.rs` counterpart.

- **Production logs no longer go through raw `println!`/`eprintln!`,
  closing a minor information-leak surface.** Over 90 call sites across
  `yanshi-core`, `yanshi-nodes`, and `yanshi-nodes-ai` printed
  directly to stdout/stderr as ad hoc logging — polluting CLI/JSON
  output and, in a few cases, leaking sensitive content (a full prompt,
  full tool-call parameters, rendered template content). All of these
  now go through `tracing`. A new `cargo xtask println-lint` CI check
  statically scans for stray `println!`/`eprintln!` (with an explicit
  `// allow-println-lint: <reason>` escape hatch) and is wired into
  the release gate.

### Added

- **Structured output can now be enforced on agent final answers.**
  `ReActAgent` and `PlanExecuteAgent` both gain an `output_schema`
  option: a candidate final answer is validated against the JSON
  schema, and a failed validation triggers a bounded correction retry
  (default 2 attempts) that feeds the validation error back to the
  model. Exhausting the retry budget is a hard error on both agents.
  Along the way, a pre-existing `yanshi-llm` bug was fixed:
  `response_format` was routed through the generic `parameters` map and
  silently dropped by Anthropic's and Google's parameter allowlists; it
  is now a first-class `ProviderRequest` field across all six
  providers.

- **Token-level streaming events now flow from the LLM through to the
  CLI, server, and Web UI.** `ReActAgent`'s LLM calls now stream by
  default, emitting a new `AgentEvent::TokenDelta` per chunk
  (deliberately excluded from the persisted trace/checkpoint — it's a
  live-only signal). `harness chat` and `harness run --output text`
  render them as an in-place typewriter effect; the Web UI's session
  view accumulates them into a live-growing line above the event
  timeline. As a side effect, this also fixed a latent gap where a
  model config with `requires_streaming: true` couldn't be used by
  `ReActAgent` at all.

- **Agent loops can now pause to ask the user a question mid-run, and
  resume with the answer.** A new `ask_user` tool is available to both
  `ReActAgent` and `PlanExecuteAgent`; invoking it stops the loop with
  a new `AgentStopReason::AwaitingInput{question}` and persists a
  checkpoint carrying the pending question — distinct from the existing
  tool-call approval gate, which gates a specific invocation rather
  than pausing for an open-ended question. Wired through the full
  stack: new server routes (`GET/POST .../interrupt[/answer]`), CLI
  support in `harness run`/`chat`/`resume-loop`, and a Web UI
  `InterruptCard`.

- **Long-running agent loops can now checkpoint and resume mid-loop,
  not just replay a pending tool call.** A new `AgentLoopCheckpoint`
  type captures a run's step index, iteration count, tool-call
  history, and cumulative cost at each step boundary. `yanshi
  harness run` now checkpoints every turn by default, and a new
  `yanshi harness resume-loop <session_id>` command resumes
  execution from the last saved step instead of restarting the whole
  run.

### Security

- **`yanshi serve` now refuses to start unauthenticated on a
  non-loopback bind.** A `SecurityProfile` field governing exactly this
  existed but was never actually read anywhere — so a real deployment
  binding to `0.0.0.0` (e.g. because a PaaS sets `PORT`) with no auth
  token configured would start up fine under the default `Local`
  profile, logging only a warning. The server now refuses to start
  (both the real startup path and `yanshi serve --check`) unless the
  bind is loopback or a token is configured. The Docker Compose file
  and Helm chart, both of which shipped exactly this unsafe combination
  by default, now wire an `YANSHI_API_TOKEN`.

- **DAG `shell` nodes are now wrapped in the OS-level sandbox under the
  Production security profile.** A `require_os_sandbox` field had been
  defined for Production but never consumed by the one call site that
  needed it — a whitelisted shell command got no OS-level syscall/
  filesystem/network confinement beyond "is the first token on the
  allow-list." Under Production, the node is now wrapped in an
  enforcing sandbox backend, and construction is refused outright if
  the resolved backend can't actually enforce.

- **Fixed two SSRF bypasses in `HttpTool`'s network-address checks.**
  IPv4-mapped IPv6 addresses (e.g. `::ffff:169.254.169.254`, the cloud
  metadata address in its IPv6 form) sailed through classification
  unnormalized; addresses are now normalized via `to_ipv4_mapped()`
  first. Separately, `validate_url_allowed` resolved and validated a
  hostname's IPs, then let `reqwest` resolve the same hostname *again*
  independently when it actually connected — a DNS-rebinding TOCTOU.
  Each request now builds a client pinned to exactly the validated
  address set, so `reqwest` can never connect anywhere that wasn't
  already checked.

- **Added per-tenant run admission control, and stopped `/metrics`
  from leaking tenant/run IDs.** `POST /v1/runs` previously spawned
  every submitted run unconditionally, with no per-tenant or global
  concurrency/rate limit. A new per-tenant semaphore plus sliding-window
  submission counter now returns HTTP 429 once a tenant exceeds its
  configured limits (tightest in Production). Separately, the
  intentionally-unauthenticated `/metrics` endpoint previously emitted
  per-tenant and per-run-ID labels, letting any unauthenticated scraper
  enumerate active tenants and run IDs; both gauges are now aggregate,
  unlabeled values.

- **Tightened three supply-chain defaults: marketplace signature
  verification, plugin signing, and MCP stdio input bounds.** The
  library-level default for remote marketplace/plugin caching now
  requires real Ed25519 signature verification (was checksum-only) and
  rejects plain `http://` registry URLs except on loopback. Plugin
  manifests gain a real signature section — `plugin install --signed`
  previously took the operator's word for it with zero independent
  verification; a present signature is now Ed25519-verified against the
  resolved entrypoint's bytes. `yanshi-mcp`'s stdio transport reader
  now bounds every line read instead of an unbounded `read_line`, and
  its notification channel is now bounded and backpressured instead of
  unbounded.

- **Capped expression-parser recursion depth to prevent stack-overflow
  crashes from deeply nested `run_if`/`while` expressions.**
  `yanshi-graph`'s recursive-descent expression parser had no depth
  limit across its self-recursive paths (unary prefix chains,
  parenthesized grouping, function-call arguments) — a workflow YAML
  with a sufficiently deeply nested condition could crash the process
  with a stack overflow before evaluation ever ran. The parser now
  rejects expressions past a depth of 64 with a clear error instead of
  overflowing the stack.

- **`cargo-audit` wired into CI; 11 of 13 pre-existing RUSTSEC
  advisories resolved.** No CI job had ever run `cargo-audit` against
  `Cargo.lock`, so known CVEs in dependencies went unverified
  indefinitely. A new `cargo-audit` job (gated into the release gate)
  now fails on any newly-introduced advisory not explicitly
  allow-listed in `.cargo/audit.toml`. Of the 13 advisories present at
  the time this was wired in, 11 were fixed via individually-verified
  targeted `cargo update -p <crate>` bumps (`bytes`, `crossbeam-epoch`,
  `tar`, `tracing-subscriber`, `rustls-webpki`, `quinn-proto`) — a
  blanket workspace-wide `cargo update` was tried first and discarded
  because it silently rebalanced the `rustls` crypto-provider feature
  unification and broke `yanshi-worker`'s real mTLS gRPC test
  suite. The remaining 2 (`lopdf`, needing a breaking bump incompatible
  with `yanshi-rag`'s `pdf-extract` dependency; `rsa`, via an
  unreachable `sqlx-mysql` edge, with no upstream fix available at all)
  are documented allow-list entries, not silently ignored. Separately,
  `yanshi-store-spi` (an L0 contract crate) no longer depends on
  `sqlx` at all — its only use was a single-line `From<sqlx::Error>`
  error-string conversion, and nothing in the workspace relied on the
  implicit `?`-based conversion it enabled.

### Fixed

- **`input_mapping` template values with non-canonical whitespace no
  longer silently resolve to nothing (or, in one case, a corrupted
  field name).** The executor factory's runtime parser
  (`yanshi-config::executor::factory`) matched the literal 3-character
  sequences `"{{ "` / `" }}"` when extracting a `{{ nodes.x.outputs.y }}`
  mapping, while schema validation already tolerated arbitrary
  whitespace — so a value like `"{{nodes.x.outputs.y}}"` (no spaces) or
  `"{{ nodes.x.outputs.y}}"` (asymmetric) passed validation but resolved
  to no input at all at graph-build time, or in the asymmetric case
  produced a field name with a stray `"}}"` still attached. The runtime
  parser now uses the same tolerant trim as validation.

- **Fixed a malformed request URL for the Gemini provider.** The
  shipped `base_url` for Google (in both `yanshi-llm/config/config.yml`
  and the embedded `templates/default_models.yml`) was Google's
  OpenAI-compatibility endpoint prefix (`.../v1beta/openai`), but the
  `"google"`/`"gemini"` provider dispatch routes to the native
  `GoogleProvider`, which appends its own `/v1beta/models/{model}:{method}`
  path — producing a URL with a duplicated `/v1beta/` segment and a
  stray `/openai` component on every real request. No existing test
  had ever constructed a `GoogleProvider` from the actual shipped
  config value, so this was invisible until now. Fixed the config
  value to match `GoogleProvider`'s own correct hardcoded default.

### Added

- **`yanshi cleanup` now works without a database, for local-only
  CLI usage — and defaults to the CLI's own run/trace directories.**
  The retention sweep for `~/.yanshi/runs` / `~/.yanshi/traces`
  previously hard-required `DATABASE_URL` before touching the
  filesystem at all, so an operator who only ever ran
  `yanshi workflow run` locally (never `yanshi serve`, no
  Postgres anywhere) had no way to reap either directory short of
  standing up a database just to run a cleanup sweep. A new
  filesystem-only sweep path (mtime-based, mirroring the existing
  DB-free trace-dir sweep) now runs whenever `DATABASE_URL` is unset,
  and `yanshi cleanup` with no flags now defaults `run_dir`/
  `trace_dir` to the same `~/.yanshi/{runs,traces}` paths
  `yanshi workflow run` / `yanshi trace *` already write to,
  instead of silently sweeping nothing.

- **`chunk_strategy = "semantic"` is now available in a skill's
  `[[knowledge]]` manifest entries.** `yanshi-rag` has long had a
  fully-implemented embedding-based `SemanticChunker` (splits text at
  topic boundaries via cosine similarity between sentence embeddings),
  but it was unreachable from `skill.toml` config — the config-first
  dispatch path was entirely synchronous, while semantic chunking's
  real work requires an async call to an embedding provider. A new
  async-only entry point drives `SemanticChunker` directly (bypassing
  the shared synchronous `ChunkingStrategy` trait rather than
  converting it — that would have touched dozens of unrelated call
  sites, like the RAG evaluation harness, for no benefit to them); a
  `[[knowledge]]` entry can now set `chunk_strategy = "semantic"` plus
  `embedding_model` / `chunk_similarity_threshold` /
  `chunk_min_segment_size` / `chunk_buffer_percentile` (all optional,
  mirroring the config pattern skills already use for semantic memory).
  Requires `OPENAI_API_KEY`; every other `chunk_strategy` option
  continues to work fully offline. Not extended to the RAG evaluation
  CLI's own `--chunk-strategy` benchmarking flag, and `SKILL.md`
  frontmatter still has no `[[knowledge]]` support at all — both
  pre-existing, unrelated gaps.

- **`AGENTS.md` regenerated from current sources.** The agent-facing
  workspace-instructions file (read by `yanshi-harness`'s
  `AgentsMdProvider` as high-priority context for every harness-run
  agent) had drifted badly since its last update: it described 14
  crates against an actual 25-member workspace, called
  `yanshi-server`/`yanshi-db` empty scaffolds against a
  19.5K-LOC gateway and a 9-table/9-repo persistence layer, and never
  mentioned `yanshi-harness`/`yanshi-config`/`yanshi-value`/
  `yanshi-graph`/`yanshi-store-spi`/`yanshi-agent-spi`/
  `yanshi-worker`/`yanshi-ui` at all. Regenerated from `CLAUDE.md`
  and `docs/CURRENT_STATUS.md`; five superseded root design docs
  (`ARCHITECTURE.md`, `TERA_INTEGRATION_ANALYSIS.md`,
  `TERA_INTEGRATION_COMPLETE.md`, `LOOP_NODES_IMPLEMENTATION.md`,
  `MIGRATION_V2.md`) gained "historical reference" banners pointing at
  their current replacements.

### Changed

- **`yanshi-cli`'s 2,700-line `main.rs` split into per-domain
  `commands/*/cli.rs` modules.** All clap argument-parsing structs and
  dispatch logic for roughly twenty unrelated command domains had
  accumulated in a single file. Each domain's `#[derive(Args)]` /
  `#[derive(Subcommand)]` types and its dispatch match arm now live
  alongside that domain's existing business logic (e.g.
  `commands/workflow/cli.rs`), leaving `main.rs` at ~130 lines: just
  the top-level `Cli`/`Commands` enum and a flat dispatch match. Pure
  code motion — verified via byte-identical `--help` output across
  every subcommand (both default and `--no-default-features` builds)
  before and after.

### Changed

- Kling text-to-video now defaults to the mainland host
  (`api-beijing.klingai.com`), like MiniMax; international accounts set
  the model's `base_url` to `https://api-singapore.klingai.com`. The live
  test reads `KLING_VIDEO_BASE_URL`, and its MiniMax case uses
  `MiniMax-Hailuo-2.3`.
- Kling text-to-video supports `kling-4.0` and `kling-4.0-flash` through
  Kling's current API (`POST /text-to-video/{model}`, `GET /tasks`); the
  `kling-v*` models keep the legacy `/v1/videos/text2video` API. Resource
  packs for 4.0 are only drawn on by the current API. The Kling live test
  now generates 3 s of `kling-4.0-flash` at 720p.
- Claude thinking follows each model's API. Opus 4.6+, Sonnet 4.6+,
  Haiku 5.5 and Fable get `thinking: { type: "adaptive", display:
  "summarized" }` with the depth in `output_config.effort` (`Auto` keeps
  the model default; `Budget(n)` is bucketed); Haiku 4.5 keeps
  `budget_tokens`, which is a 400 on 4.7 and later. `Disabled` sends
  `between_tools` on Sonnet 5.5 and is an error on Opus 5.5 and Fable,
  which always think. The registry now lists the current lineup (Fable
  5.1 / 5, Opus 5.5 / 5 / 4.8 / 4.7 / 4.6, Sonnet 5.5 / 5 / 4.6, Haiku 5.5
  / 4.5), marks them all as supporting thinking (before, `.thinking()` on
  any Claude model was refused), and leaves `temperature` null on models
  that reject non-default sampling parameters.
- The `llm-live.yml` manual run has an `image` checkbox that sets
  `YANSHI_LIVE_IMAGE_TESTS`, so the billed image jobs can run on demand
  like the video ones; nightly runs still skip them.
- OpenAI image generation and edit send `response_format` to DALL-E
  only; `gpt-image-*` rejected it with a 400 and always returns
  `b64_json`.
- Provider requests carry a `yanshi-llm/<version>` User-Agent; Wikimedia
  refused source-image downloads without one.
- StepFun follows its current model docs. `step-5-preview` is in the
  registry (text, image and video in; 1M context, 64K output), and
  `step-3.7-flash` now accepts video. `.thinking()` on `step-5-preview`,
  `step-3.7-flash` and `step-3.5-flash-2603` sends `reasoning_effort`
  (`low` / `medium` / `high`; `medium` rounds up to `high` on 2603, which
  has only two levels); before, the provider ignored it. `Disabled` is
  an error, since StepFun has no way to turn reasoning off. Non-streaming
  responses carry the reasoning text from `reasoning` /
  `reasoning_content`.

### Removed

- StepFun image generation: StepFun retired `step-2x-large` and
  `step-image-edit-2` and stopped its image APIs on 2026-10-10. Both
  registry entries and the StepFun image live test are gone, and
  `yanshi image generate` now defaults to `gpt-image-2` instead of the
  long-gone `step-1x-medium`.
- The ten retired Claude models are gone from the bundled registry
  (`templates/models/anthropic.yml`): the Claude 3 / 3.5 / 3.7 family,
  `claude-opus-4-20250514` and `claude-sonnet-4-20250514`. Anthropic
  answers requests to them with an error. `AnthropicProvider::supported_models`,
  the `llm-chain` CLI template, the READMEs, docs and examples now name
  current models. Configs that still name a removed entry fail at lookup.

## [v1.0.0-rc.1] — 2026-05-21

The R1 → R4 reflection arc (see `docs/L1_L3_REFLECTION_R*.md`) drove
this set of changes. Most landed via a multi-session dogfooding loop:
build an application that exercises a platform surface, capture
findings as it goes, close the findings, repeat. **31 dogfooding
findings closed across 4 reflection cycles** (every
yanshi-internal item in the action queue), with no regressions
across the 200+ tests touched.

### Added

#### Security

- **`AgeEncryptedPreferenceStore` — encryption-at-rest for the
  preference memory layer** (P10.7.2). New
  `yanshi-memory::AgeEncryptedPreferenceStore<S: PreferenceStore>`
  wraps any preference store impl (defaults to `SqlitePreferenceStore`)
  and transparently age-encrypts every `value` payload on write +
  decrypts on read using a single-user X25519 identity. Keys (tenant
  / user / key string) stay plaintext on disk — only the value is
  opaque to anyone without the identity file. KMS decision: `age` only,
  no cloud KMS dependency (cloud / envelope re-keying / multi-user
  deferred to v2 per `docs/ROADMAP_v2.md` Theme B). On-disk shape is
  `"age:v1:<base64-age-ciphertext>"`; the marker prefix lets readers
  reject plaintext-bleed-through from a stale
  `SqlitePreferenceStore` writer. Identity-file helpers
  (`generate_identity_file` + `load_identity_file`) refuse to
  overwrite existing keys and chmod 0600 on Unix. 12 hermetic tests
  cover round-trips, version semantics through the wrapper,
  decrypt-with-wrong-identity rejection, plaintext-bleed-through
  rejection, and identity-file lifecycle.

- **Per-tool `os_sandbox` override on `[[tools]]` blocks** (P10.4.1).
  Skill manifests can now opt individual `shell` / `script` tools in
  or out of the OS-level sandbox independently of the manifest-level
  `[security] os_sandbox` default. The new `os_sandbox: Option<bool>`
  field on `ToolConfig` is fully optional; `None` falls back to the
  manifest-level value (so every pre-P10.4.1 manifest parses
  identically with unchanged behaviour). `Some(true)` opts the tool
  in even when the manifest default is off; `Some(false)` opts it
  out even when the manifest default is on. Only the two subprocess-
  spawning built-in tools honour the override; `file` / `http`
  ignore it. `yanshi skill inspect --explain-permissions` now
  prints a per-tool resolution table under `Sandbox profile`
  showing each sandboxable tool's resolved value + its source
  (`per-tool override` vs `inherited`). 6 new builder unit tests
  + 1 serde round-trip pin the resolution rules and schema.

#### CLI ops (cont.)

- **RAG eval `--chunk-size <N>` per-chunk latency dimension**
  (P10.6.3). `yanshi rag eval` was chunking-agnostic — corpus
  documents went straight into the retriever index. Now passing
  `--chunk-size N` re-chunks every corpus doc with a `FixedSizeChunker
  (N, overlap=0)` before indexing; retrieved chunk ids are remapped
  back to source doc ids (with dedupe within the top-K window) so
  `Recall@K`/`MRR`/`nDCG@K` stay comparable across chunked and
  un-chunked runs. The latency block reflects the chunked index, so
  capturing one baseline per chunk strategy spots chunking-side
  regressions. `EvalReport.chunk_size: Option<usize>` is additive
  (omitted from JSON when un-chunked, preserving pre-P10.6.3 baseline
  shape). When `--compare-baseline` is supplied and chunk sizes
  differ, the CLI prints a stderr warning but doesn't fail — cross-
  chunk comparison is still useful for "did the chunking change hurt
  recall?" investigations. 12 hermetic tests cover the new library
  surface (`chunk_dataset`, `evaluate_with_remapping`) plus the CLI
  flag. `docs/RAG_EVAL.md` gains a "Per-chunk-size latency profile"
  section with the three-baselines capture recipe.

- **`yanshi marketplace search --format text|json|json-envelope`**
  (P10.9.2). The `search` subcommand was text-only; now also speaks
  the canonical JSON shapes so operators can script against it. Bare
  `--format json` emits the structured payload (`registry`, `query`,
  `package_type_filter`, `manifest` block, `entries[]`,
  `matched_count`); `--format json-envelope` wraps the same body in
  the `yanshi.cli/1` envelope. Empty matches produce `entries: []`
  + `matched_count: 0` (never null) so consumers don't special-case
  the no-result path. Web UI marketplace tab from the original TODO
  is deferred (out of scope per the P10.17.1 debugger-focused UI
  commitment); a `P10.9.2-FU1` carry-forward tracks it until concrete
  demand surfaces.
- **`yanshi agent replay <current> --diff <baseline>`** (P10.8.1).
  New `agent` top-level subcommand namespace + `replay` file-to-file
  diff. Compares two ReAct `AgentEvent` JSONL streams along three
  operator-facing dimensions: tool-call order, terminal stop-reason,
  per-step LLM token usage. Step-kind / tool-name / stop-reason
  divergence fails the gate; token deltas are soft variances by
  default (`--strict-tokens` promotes them). Output formats
  `text` / `stream-json` / `json-envelope` (canonical
  `yanshi.cli/1` shape). Pure file-to-file — the user produces
  both JSONL files however they like (no `agent run` wrapper yet;
  follow-up territory once one exists). 14 pure unit tests cover
  the comparator + JSONL parser; 6 hermetic CLI tests cover the
  binary end-to-end. Separate namespace from `yanshi harness
  replay` because the wire shapes differ (`AgentEvent` is
  fine-grained per-step / per-LLM-call; `HarnessEvent` is the
  approval-gated session envelope).

#### Release engineering

- **GitHub Release workflow + multi-arch GHCR image push** (P10.0.4).
  New `.github/workflows/release.yml` fires on any `v*.*.*` tag push
  (or manual `workflow_dispatch` with `dry_run: false`) and runs three
  jobs: a 4-target matrix building `yanshi-<target>.tar.gz` for
  linux x86_64 + linux aarch64 + macOS Intel + macOS Apple Silicon;
  a `docker buildx` multi-arch (linux/amd64 + linux/arm64) build of
  the root `Dockerfile` pushed to `ghcr.io/<owner>/yanshi-server`
  with `:<tag>` always and `:latest` only on stable tags; and a
  publish step that aggregates tarballs + a combined `SHA256SUMS.txt`
  and creates the GitHub Release with auto-generated commit-list body.
  `prerelease` flag auto-derives from a `-` in the tag (RC / alpha /
  beta tags don't promote `latest`). Companion
  `scripts/release_dry_run/run.sh` rehearses the build legs locally
  without pushing; uses `cargo metadata` to resolve the target dir
  through the Yanshi `~/.cargo/config.toml::build.target-dir`
  redirect. `docs/RELEASE_CHECKLIST.md` §10 documents the trigger,
  first-push prerequisites (GHCR public-visibility flip), and
  manual-dispatch rehearsal mode.

- **Production deployment dress-rehearsal reproducible via Apple
  container** (P10.0.1). New `scripts/production_dress_rehearsal/`
  ships a two-stage Containerfile (rust builder + both `yanshi`
  + `yanshi-server` binaries), a step-walking
  `inside_container.sh`, a host-side driver `run.sh`, and checked-in
  `last-run.{log,json}` fixtures. Walks all 6 steps of the
  `docs/RELEASE_NOTES_v1.0.0-rc.1.md::Production Deployment
  Checklist` plus the 4 acceptance gates inside a fresh
  `ubuntu:24.04` container. Canonical outcome on a clean apple-
  aarch64 box: **doctor exit code 0, status `ok`**, 5 of 6 steps
  PASS, step 6 / AG3 / AG4 SKIP (host-side, requires Docker +
  Postgres sidecar — documented host-side commands in the README).
  Discovered + filed two follow-ups: (a) `serve --check` requires a
  real Postgres connection despite a stale source comment claiming
  otherwise; (b) `yanshi serve` shells out to a separate
  `yanshi-server` binary not bundled with the doctor-smoke
  image. Paired with P10.0.5 in the pre-tag checklist.

- **Fresh-VM `yanshi doctor` smoke reproducible via Apple container**
  (P10.0.5). New `scripts/doctor_smoke/` directory ships a multi-stage
  `Containerfile` (rust:1-slim-bookworm builder → ubuntu:24.04 smoke
  image with the binary copied in), a driver `run.sh`, a checked-in
  `last-run.json` fixture, and a README. Drives Apple's `container`
  CLI by default; `DOCTOR_SMOKE_RUNTIME=docker` switches to Docker.
  Reproduces step 5 of `docs/RELEASE_NOTES_v1.0.0-rc.1.md`'s pre-cut
  checklist. The canonical fresh-VM outcome is `status: fail` /
  exit code 2 — production-profile treats missing `~/.yanshi/*`
  dirs as fail (vs. warning on dev/local), so a zero-state Ubuntu
  produces fail by design. The fixture + README document this
  expected outcome so future operators don't mis-interpret it as a
  binary crash.

#### Workspace tooling

- **`cargo xtask refresh-live-models`** (P10.3.4 / P10.18.1). New
  subcommand that pings each of the 9 live-test providers' `/models`
  endpoints (OpenAI / Anthropic / Google / Moonshot / StepFun / GLM /
  DashScope / DeepSeek / MiniMax), verifies the hard-coded text-model
  default in `yanshi-llm/tests/provider_consistency_live.rs` is
  still listed, and prints suggestions ranked by shared-prefix when a
  default goes missing. Loads `~/.yanshi/.env` locally (silently
  no-op on CI where keys come from the workflow's env block); treats
  existing-but-empty env vars as unset so an exported-but-empty key
  doesn't block a valid `.env` value. Validate-only by design — the
  operator copies suggested replacements into the test source rather
  than auto-editing (the defaults carry inline rationale comments
  that an automated rewrite would clobber). First real-world run
  surfaced two findings: `glm-4.5-flash` actually deprecated (rotate
  to `glm-4.5` / `glm-4.6`); `claude-haiku-4-5` is a rolling alias
  that doesn't appear in `/v1/models` but resolves in real API calls
  (Anthropic-specific false positive of the "is the id in the list"
  check — documented).

- **FlowValue + checkpoint hot-path benches in `yanshi-core`**
  (P10.1.1). New `yanshi-core/benches/hot_paths.rs` covers the two
  hot paths the existing scheduler bench misses: `serde_json::from_value
  ::<FlowValue>` over the P0.2 tagged-enum wire shape (5 variants ×
  sizes) and the full `CheckpointManager` save+load round-trip plus
  decode-only isolation (10 / 100 node state pools, 9 bench points
  total). Wired into the bench CI workflow + `apple-m2-max.json`
  baseline; `cargo xtask bench-gate --allow-missing` now compares 19
  rows (10 from P10.2.1 + 9 here) at the default 1.25× threshold. The
  TODO's "look for P3.3 envelope regressions" half found no target —
  P3.3 was the CLI envelope wave with zero touches to `yanshi-core::
  {value,checkpoint,flow}`. The new baseline locks in post-N8 numbers
  so any future regression in these paths trips the gate.

- **Per-node criterion benches in `yanshi-nodes`** (P10.2.1). New
  `yanshi-nodes/benches/node_latency.rs` covers the pure-compute
  built-ins — Tera template rendering (3 sizes), conditional
  dispatch (3 variants), and tokio fs read/write (2 payload sizes,
  10 bench points total). Wired into the existing `bench-gate` CI
  workflow + the `apple-m2-max.json` baseline so a template-render
  regression now trips the same gate `scheduler` / `provider_hop` /
  `retrieval` / `event_write` already feed. Nodes that depend on
  external services (LLM, HTTP, MCP, RAG, image, audio, etc.) are
  intentionally out of scope — their latency is dominated by
  round-trip, not Yanshi code.

- **`cargo xtask test-gate`** (P10.19.2). New sibling to the existing
  `bench-gate` (criterion microbench gate). Runs
  `cargo test -p <crate> --all-targets --quiet` per workspace member,
  measures per-crate wall-clock, compares against a host-specific
  baseline JSON, fails on a configurable threshold (default **1.5×**,
  looser than bench-gate's 1.25× because `cargo test` is meaningfully
  noisier than criterion). Three modes — compare (default), `--update`
  (refresh the baseline), and `--input <path>` (pure-data comparison
  for CI two-stage flows). Baselines live at
  `benches/baselines/test-timings/<host>.json` with a README pinning
  schema + capture flow. 16 hermetic unit tests cover the pure
  comparator, the test-count parser, crate selection, and the
  argument validation paths. Not wired into CI yet — landing the
  xtask first lets contributors confirm the heuristic against real
  PRs locally, same staged rollout as bench-gate.

#### Observability

- **`yanshi_state_size_bytes{run_id}` Prometheus gauge wired
  end-to-end** (P10.14.2-FU6). The last deferred series from the
  "Memory & workflow state" Grafana panel now renders live: the
  dashboard matrix is 14/14 ✅. Surface lands in two pieces:
  - `yanshi-core`: new public `state_size` module with the
    `StateSizeObserver` trait, `estimated_state_pool_bytes`
    helper, and `FlowValue::estimated_size_bytes` method. `Flow`
    gains a `with_state_size_observer(Arc<dyn StateSizeObserver>)`
    builder + a private hook that fires after every node insert
    in both serial and concurrent execution paths.
  - `yanshi-server`: new public `live_state_registry` module
    with the process-local `LiveStateRegistry`. `AppState` carries
    one (cloned cheaply into every `RunContext`); the DAG
    executor attaches a per-run observer at submit time and
    deregisters on terminal transitions so gauge cardinality is
    bounded to currently-running runs. The `/metrics`
    `refresh_scrape_time_gauges` helper iterates the snapshot
    and emits one labelled gauge sample per active run.
  - 15 hermetic tests across 3 layers (`yanshi-core` lib +
    integration, `yanshi-server` lib + integration). No
    Postgres or real workflow needed at test time — the
    integration test seeds the registry directly through the
    same observer path the executor uses.

#### CLI ops

- **`yanshi backup --output <path>`** (P10.15.1). Orchestrates
  `pg_dump --format=custom` + `tar -czf` of the five filesystem
  state surfaces (`run_dir`, `trace_dir`, marketplace cache,
  skills, plugins) into a single bundle directory with a
  versioned `manifest.json`. Closes the operator loop that
  `docs/SERVER_BACKUP_RESTORE.md` documents — that doc described
  *which* state surfaces must be backed up; this command actually
  does it in one invocation instead of leaving the operator to
  run six commands by hand and reason about the order.
  - Flags: `--output` (required), `--database-url`, `--include`
    (repeatable; aliases like `runs` → `run_dir`, `database` →
    `db` accepted), `--dry-run`, `--force`,
    `--format text|json|json-envelope` (canonical `yanshi.cli/1`).
  - Manifest schema discriminator `yanshi.backup/1` is the
    wire-shape promise a future `yanshi restore --input <path>`
    will consume. Restore itself stays out of this TODO's scope.
  - Tool requirements (`pg_dump`, `tar`) are PATH-probed up front;
    "tool not found" surfaces as a `failed` step with a
    package-manager hint, not an unhelpful panic.
  - A missing source directory is `skipped` (not `failed`) — the
    common case where the operator only opted into a subset of
    state surfaces stays out of the failure path.
  - 12 hermetic unit tests in `commands::backup::tests`:
    include-name parsing + aliases, artifact-name layout
    discipline, URL password redaction (3 variants), dir-prep
    refuse/force/create/dry-run paths, end-to-end dry-run
    behavior (all 6 includes), explicit-include subset, and
    DB-step skip-without-`DATABASE_URL`. Postgres / tar are
    never invoked in the test suite, so this runs hermetically
    in CI.

#### LLM provider layer

- **Memory layer routed through real tokenizer in agent
  production paths** (P10.3.3-FU1). New
  `yanshi_memory::TokenCounter` trait + `HeuristicCounter`
  default + five `Message::*_with_counter` constructors
  preserve every existing `Message::new` callsite as the
  heuristic path while adding a parallel precise path for
  callers that know their target model id.
  `yanshi-agents::token_counter_adapter` bridges
  `yanshi_llm::TokenCounter` (BPE from P10.3.3) to the
  memory crate's local trait without creating a new
  cross-crate dependency. `ReActAgent` and
  `PlanExecuteAgent` gained a `message_counter` field
  rebuilt from `context.model` in `apply_context`; all 15
  production `Message::user/assistant/system/tool_result`
  callsites inside the two agents now route through
  `*_with_counter(&self.session_id, content,
  &*self.message_counter)`. Direct consequence:
  `apply_memory_prompt_budget` compacts against precise BPE
  counts for OpenAI family + cl100k_base-compatible vendors,
  ending the CJK over-estimation (3-5×) and code
  under-estimation the heuristic produced. 9 new hermetic
  tests; ~50 test-site callers stay on the heuristic
  intentionally (they're testing message-handling logic, not
  tokenization accuracy).

- **Provider-specific tokenizer trait (`TokenCounter`)** (P10.3.3
  foundation slice). New `yanshi_llm::tokenizer` module ships
  `TokenCounter` trait, `TiktokenCounter` (BPE via `tiktoken-rs`
  — cl100k_base / o200k_base / p50k_base / r50k_base),
  `HeuristicCounter` (preserves the workspace's existing
  `len / 4` fallback), and `counter_for_model(model_id)` +
  `count_tokens_for_model(model_id, text)` factories. Closes the
  precision gap for OpenAI-family pre-call token budgeting: the
  heuristic over-estimates CJK text by 3-5× and code by
  4× — the BPE counter is exact for `gpt-3.5-*`, `gpt-4*`,
  `gpt-4o*`, `o1*`, `o3*`, `gpt-5*`, and within ~5-15% for the
  OpenAI-compat vendors (Moonshot, DeepSeek, GLM, DashScope
  Qwen, MiniMax, StepFun) that ship documented BPE-compatible
  tokenizers. Anthropic / Google fall back to the heuristic;
  their post-call responses still report exact counts so cost
  tracking stays accurate. 13 hermetic unit tests cover BPE
  counts against known inputs, model-name routing for every
  documented family, case-insensitivity, and the error path.
  **Follow-up `P10.3.3-FU1`** is open to wire
  `count_tokens_for_model` into `yanshi-memory::Message::new`
  (the existing heuristic site) — the foundation landed first
  so the accuracy improvement is visible without rippling
  through 50+ test sites. Adds `tiktoken-rs = "0.6"` dep to
  `yanshi-llm`.

#### Database layer

- **DB read-replica support** (P10.15.2). `Database` gains
  optional `read_pool: Option<PgPool>` + helper
  `read_pool()` that falls back to the primary when no
  replica is configured. New constructors
  `Database::connect_with_replica` and
  `Database::connect_and_migrate_with_replica` take the
  primary URL + replica URL + per-pool connection caps;
  migrations always run against the primary so DDL never
  races the replica. Every Pg*Repo carries both `pool`
  (write) and `read_pool` (read); 12 `SELECT`-shaped sites
  in the repo layer route to `read_pool`, while every
  `INSERT...RETURNING` / `UPDATE` / `DELETE` stays on
  `pool`. New `Repositories::from_pools(write, read)` +
  `Repositories::from_database(&db)` constructors;
  `from_pool(pool)` stays as a backwards-compat shim.
  `yanshi serve` gains `--database-read-url <URL>`
  (default env `YANSHI_DATABASE_READ_URL`) forwarded
  through to the server binary. Single-node deployments
  are unaffected — `read_pool: None` falls through to the
  primary and the existing test suite passes unchanged.
  Documented in `docs/DEPLOYMENT.md` "Read-replica routing
  (P10.15.2)" with the replication-lag caveat called out.
  6 hermetic unit tests (lazy pools — no live Postgres
  required).

#### Operator dashboards

- **Prometheus `/metrics` — process / state inspectors**
  (P10.14.2-FU5). Three new scrape-time gauges
  plug into the `refresh_scrape_time_gauges` helper
  established by FU4:
  `yanshi_health_status{component}` (emits `system=1`
  unconditionally and `database=1|0` from a `SELECT 1`
  probe), `yanshi_memory_usage_bytes` (reads
  `/proc/self/statm` on Linux, `0` fallback elsewhere),
  and `yanshi_workflow_runs_active{tenant}` (single
  `GROUP BY` query against the read pool). All inherit the
  fail-soft contract from FU4. 13 of 14 dashboard series
  are now live; the remaining `state_size_bytes{run_id}`
  is deferred to `P10.14.2-FU6` because it requires
  architectural access to live `Flow::context.state_pool`
  contents the server doesn't expose, and the available
  proxies (events table size, artifacts size) would
  mislead operators. 5 new tests (3 unit + 2 integration,
  including a contract-pin that
  `yanshi_health_status{component="system"} 1` renders
  on every scrape).

- **Prometheus `/metrics` — harness session gauges**
  (P10.14.2-FU4). New scrape-time pattern: the `/metrics`
  handler now runs `refresh_scrape_time_gauges(&state)`
  before rendering. Two gauges source from this path:
  `yanshi_harness_sessions_active{status}` (computed via
  `SELECT status, COUNT(*) FROM harness_sessions GROUP BY
  status` against the read pool — reuse of P10.15.2's
  optional replica) and `yanshi_harness_approvals_pending`
  (sourced from `PendingApprovalRegistry::pending_count()`,
  an in-process mutex read). All four known status buckets
  emit every scrape so a bucket dropping to 0 renders as 0
  instead of leaving a stale value. **Fail-soft contract:**
  a DB-query failure inside the refresh is logged and
  swallowed; the scrape still returns 200 and the remaining
  metrics render. Pinned by a dedicated integration test.
  4 new tests (2 unit + 2 integration including the
  unreachable-DB invariant). 10 of the 14 dashboard series
  are now live.

- **Prometheus `/metrics` — worker fleet gauges**
  (P10.14.2-FU3). `AuthenticatedControlPlane` now emits the
  two worker-fleet gauges from its three mutation sites:
  `admit()` sets `yanshi_workers_admitted` to the
  distinct-worker count after every successful admission;
  `claim_task` sets
  `yanshi_worker_tasks_inflight{worker_id}` to the
  post-increment value; `report_result` sets the same gauge
  after the decrement. Gauges are absolute (set-not-
  increment) so idempotent re-admissions don't double-count.
  New `metrics::observe_workers_admitted` /
  `metrics::observe_worker_tasks_inflight` helpers + matching
  constants. 2 new unit tests + 1 end-to-end integration test
  exercising the real admission code path against an
  in-memory protocol. 8 of the 14 dashboard series are now
  live (6 from FU1+FU2 + 2 from this slice).

- **Prometheus `/metrics` — cleanup sweep counters**
  (P10.14.2-FU2). `cleanup_expired` now fires
  `yanshi_cleanup_runs_deleted_total`,
  `yanshi_cleanup_events_deleted_total`, and
  `yanshi_cleanup_artifacts_deleted_total` at the end of
  every sweep via a new
  `metrics::observe_cleanup_sweep(dry_run, runs, events,
  artifacts)` helper. Dry-run sweeps skip the increment (the
  Grafana panel is about actual reaping, not previews). The
  six-series matrix in `dashboards/README.md` updates from 3
  ✅ to 6 ✅; the three retention bars in the Grafana
  overview now render real data. 1 new unit test + 1 new
  integration test exercising the `/metrics` route after a
  synthetic sweep observation.

- **Prometheus `/metrics` endpoint emission — slice 1**
  (P10.14.2-FU1). `yanshi-server` now exposes
  `GET /metrics` returning Prometheus text format. The
  endpoint bypasses auth (same convention as `/health`) so
  scrapers don't need a bearer token. Three workflow-event-
  derived series are live: `yanshi_workflow_completed_total
  {status}` (counter), `yanshi_workflow_duration_seconds`
  (histogram, buckets 0.1s … 10min), and
  `yanshi_nodes_failed_total{node_type}` (counter, labelled
  by `node_id` until a future event-payload extension splits
  node_type from node_id). The recorder is installed once
  during `serve::run` boot via a `OnceLock`-guarded
  `init_recorder()` so multi-`run()` callers don't panic.
  `WorkflowEventListener::on_event` fires the counter +
  histogram on each terminal event. Deps:
  `metrics = "0.23"` + `metrics-exporter-prometheus = "0.15"`
  added to `yanshi-server`. The four remaining series
  classes in the dashboard contract (retention sweep counters,
  worker fleet gauges, harness session gauges, scrape-time
  process inspectors) are deferred to follow-up TODOs
  `P10.14.2-FU2/FU3/FU4/FU5`; `dashboards/README.md`
  "Current emission status" carries the per-metric
  live/deferred matrix. 11 hermetic tests (5 unit + 6
  integration via `tests/metrics_endpoint.rs` exercising
  the actual Axum route, content-type, auth bypass, and
  every contracted metric name).

- **Checked-in Grafana dashboard template** (P10.14.2). New
  `dashboards/grafana/yanshi-overview.json` (9 panels:
  system health, active runs per tenant, workflow completions
  by status, p50/p95/p99 duration, node failures by node_type,
  worker fleet admitted + in-flight, memory + state size,
  retention sweep deletions, Harness Mode sessions +
  approvals) + `dashboards/README.md` operator playbook
  (import recipe, metric contract, conventions). The dashboard
  uses a `${DS_PROMETHEUS}` variable so it survives datasource
  renames during import, and tags with `yanshi / overview /
  operator` for discoverability. Imports cleanly into Grafana
  8+ (`schemaVersion: 38`). `docs/KUBERNETES_DEPLOYMENT.md`
  §Grafana Dashboard updated to link the JSON.
  - **Documented gap:** the in-core Prometheus metrics module
    was removed during the observability split, and
    `yanshi-server` doesn't expose `/metrics` today. The
    dashboard is forward-compatible — it will render the
    moment emission lands. Shipping the JSON now pins the
    operator-side metric-name contract so the emission code
    (tracked under follow-up `P10.14.2-FU1`) can be
    unit-tested against an external source of truth, and so
    operators have something to import on day one of
    `P10.14.2-FU1` closure.

#### Worker dispatch hints

- **gRPC wire-extension for capability + locality hints**
  (P10.16.2-FU1). Closes the follow-up opened during P10.16.2.
  `pb::WorkerTask` gained `node_type: string` (tag 6),
  `pb::ClaimTaskRequest` gained `accepted_node_types:
  repeated string` (tag 2) + `locality_run_id: string`
  (tag 3), and `pb::HeartbeatRequest` gained
  `accepted_node_types: repeated string` (tag 5). All four
  fields are wire-additive — pre-FU1 workers (which never set
  them) encode as empty values which the server decodes as "no
  hints / untagged task," preserving pre-P10.16.2 FIFO
  behavior exactly. Both `GrpcWorkerService` and
  `WorkerControlPlane`'s tonic adapter now route through
  `protocol.claim_task_with_hints`; `GrpcWorkerProtocol`
  (client side) gained an explicit `claim_task_with_hints`
  impl. `yanshi-worker::WorkerConfig` gained a
  `capabilities: WorkerCapabilities` knob +
  `with_capabilities` builder; `run_once` sends them on every
  heartbeat AND attaches them to claim hints, so distributed
  workers can declare which node types they accept and the
  queue scan skips work they can't run. 7 hermetic
  round-trip tests in `scheduler::grpc::hint_proto_tests`
  cover the wire-shape conversions, malformed locality UUID
  rejection, and the pre-FU1 backwards-compat invariants.
  `docs/DISTRIBUTED.md` "Wire-extension status" subsection
  marks FU1 closed with a wire-shape mapping table.

- **Capability + locality hints on worker claim** (P10.16.2
  foundation). `WorkerCapabilities { node_types }` advertises
  which task labels a worker accepts; `WorkerTask.node_type:
  Option<String>` tags tasks with their capability label;
  `ClaimHints { capabilities, locality_run_id }` is the optional
  per-claim payload. New trait method
  `WorkerProtocol::claim_task_with_hints(worker_id, hints)`
  with a default impl falling back to `claim_task` so the gRPC
  adapter compiles unchanged; `InMemoryWorkerProtocol` overrides
  to scan the queue in three passes (same-run + capability,
  capability anywhere, FIFO). The protocol also caches the
  most-recently-claimed `run_id` per worker so workers without
  an explicit locality hint still get warm-cache continuity.
  `WorkerHeartbeat` gains a `capabilities` field with a default
  empty value; `WorkerControlPlane::claim_task_with_hints` is
  the public entry point and still increments the run snapshot
  the same way bare `claim_task` does.
  - Wire-extension to gRPC stays as a tracked follow-up
    (`P10.16.2-FU1`); the trait surface is forward-compatible.
    Workers talking gRPC today effectively send "no hints" and
    get pre-P10.16.2 FIFO behavior — fully additive upgrade.
  - 9 hermetic unit tests cover capability default, restricted
    set with untagged-task fallback, capability filtering,
    locality preference, FIFO fallback when no locality match,
    cached-last-run bias, combined capability + locality, and
    the control-plane snapshot invariant.

#### Worker admission

- **Signed-JWT identity flavour for worker admission** (P10.16.1).
  New `yanshi-server::scheduler::jwt` module ships `JwtPolicy`
  (issuer / audience / key pool / leeway), HS256 + RS256
  `JwtVerificationKey`, and a strict claim validator
  (`iss`/`aud`/`sub`/`exp`/`nbf`). `WorkerAdmissionPolicy` gains
  `jwt: Option<JwtPolicy>` + `jwt_workers: HashSet<WorkerId>` so
  workers can opt into JWT auth alongside (or instead of) the
  existing PSK path. PSK takes precedence over JWT when a worker
  is misconfigured into both sets so there's no silent
  downgrade. The `aud` claim deserializer is tolerant of both
  the string and string-array forms per RFC §4.1.3. Key
  rotation works the same way as PSK: append a new
  `JwtVerificationKey` to the policy pool, flip the IdP, drop
  the old key. HS256 fits self-administered deployments; RS256
  fits the production path where an external IdP (Okta / Auth0
  / Vault / GCP Workload Identity) signs and the control plane
  only holds the public key.
  - **Wire-shape additive change:** `AdmissionError::InvalidCredential`
    gained a `reason: String` field so the verifier-specific
    failure mode (PSK rotation mismatch / JWT issuer mismatch /
    expired / etc.) reaches the operator-facing log line. The
    contract is `Experimental` per `docs/STABILITY.md` so this
    is in scope; no external matches exist in the workspace.
  - 14 hermetic unit tests in `scheduler::jwt::tests` cover
    happy path, every documented failure mode, leeway boundary,
    key rotation pool, and the multi-`aud` shape; 7 more tests
    in `scheduler::admission::tests::jwt_flavor` exercise the
    policy-layer routing (valid token admitted, missing
    credential, wrong subject, expired token,
    `jwt_workers`-without-`jwt`-policy as server config error,
    PSK-takes-precedence, anonymous workers still anonymous
    when JWT is configured). `now()` injection keeps the suite
    deterministic.
  - gRPC-metadata propagation of admission tokens is still
    deferred (separate TODO).
  - Adds `jsonwebtoken = "9.3"` dep to `yanshi-server`.

#### Server gateway

- **Per-run retention override on `POST /v1/runs`** (P10.14.1).
  The request body now accepts an optional `retention_overrides`
  object with `events_days` and `artifacts_days` fields. The
  cleanup sweep uses `max(global, override)` so a per-run
  override can only *extend* retention — it cannot shorten the
  tenant + profile default. Pinning a run's events or artifacts
  also pins the parent `runs` row itself: the cleanup SQL keys
  the run-row deletion on `GREATEST(global, events_override,
  artifacts_override)` so the `ON DELETE CASCADE` from `runs`
  doesn't yank the pinned children out from under the override.
  Negative overrides are rejected at the API layer with a clean
  `bad_request` error; `Some(0)` is accepted (caller convenience)
  and normalized to NULL in the DB so the audit story stays
  honest. New migration `0005_run_retention_overrides.sql` adds
  the two nullable columns to `runs`; existing rows default to
  NULL (no override) so the upgrade is a no-op for everyone
  who doesn't opt in. See `docs/DEPLOYMENT.md` "Per-run
  retention overrides" for the operator-facing snippet.
  Closes the P2.2-deferred per-run override item.

#### Workflow grammar

- **`type: shell` YAML workflow node** (F-A7-2 fully closed,
  `3c3ab02`). Wraps `yanshi_tools::ShellTool` with a
  `SandboxPolicy` built from YAML params. `allowed_commands` is a
  required schema field — workflows without an allowlist fail at
  parse time, no permissive-by-default arbitrary code execution.
  See `yanshi-cli/src/executor/shell.rs`.
- **`input_mapping` accepts `{{ item.* }}` lookups inside a map
  sub-flow** (F-A6-5, `54a2751`). Flat (`item.field`) and dotted
  (`item.foo.bar`) paths both supported. Encoded via the sentinel
  source-node id `"!item"`. Existing `{{ nodes.X.outputs.Y }}`
  lookups work unchanged.
- **Map node `max_concurrent: N` parameter** bounds simultaneously-
  running sub-flows via `tokio::sync::Semaphore` (F-A6-1,
  `a4e89e8`). Unbounded preserved as legacy default. `Some(0)` is
  rejected as a config error.
- **Map node `results_summary` sibling output** surfaces
  `{total, ok, err, err_indexes}` alongside `results` (F-A6-3,
  `fee8586`). Workflows can route on partial failure without
  walking nested JSON; `eprintln!` warning fires on any failure.
- **Template node auto-detects JSON output** when the rendered
  string starts with `[` or `{` (F-A6-7, `8b73298`). Parse failure
  falls back to String wrap (safe for prose). Explicit
  `output_format: "json"` preserved as strict mode.

#### CLI

- **Dense + hybrid RAG eval baselines checked in** (P10.6.2). The
  bundled `ci_offline` dataset now has three regression-gate
  baselines under `yanshi-rag/eval_baselines/ci_offline/`:
  `bm25.json` (offline, always gated on every PR), plus the new
  `dense.json` (`text-embedding-3-small`) and `hybrid.json` (RRF
  over BM25 + dense). CI gates against all three; the dense + hybrid
  steps self-skip via `if: ${{ secrets.OPENAI_API_KEY != '' }}` so
  forks without the secret stay green. A bug-fix in the
  `--compare-baseline` reader lets it accept BOTH the bare
  `EvalReport` shape (the bm25.json convention) AND the
  `{ dataset, baseline, candidate, ... }` envelope shape that
  `--output <path>` writes — pre-P10.6.2 operators had to
  hand-extract the `.baseline` field to round-trip their own
  `--output` files back through the regression gate.
- **Pluggable RAG eval retrievers** (P10.6.1):
  `yanshi-rag::eval::DenseEval` (in-memory cosine similarity over
  pre-embedded corpus + queries) and
  `yanshi-rag::eval::HybridEval` (Reciprocal Rank Fusion with
  configurable `k` and inner-k multiplier) join the existing
  `Bm25Eval` behind the `Retriever` trait. The CLI gains
  `--retriever {bm25,dense,hybrid}` plus `--embedding-model
  <name>` (defaults to `text-embedding-3-small`); dense and
  hybrid require `OPENAI_API_KEY` at run time and surface a
  single-line actionable error when it's missing. Eval-scale
  corpora (<100k docs) keep the full vector matrix in RAM — no
  Qdrant required for the eval harness. RRF tie-break is
  deterministic (score desc, then id asc) so paired sign-test
  comparisons across runs remain reproducible. 10 new unit
  tests in `eval::retrievers::tests` plus 1 hermetic CLI test
  (`build_dense_retriever_errors_without_openai_api_key`) cover
  the new code paths.
- **`yanshi skill inspect --explain-permissions` now runs MCP
  discovery by default** (P10.9.1). Pre-P10.9.1 it was opt-in via
  `--with-mcp-discovery` because spawning every declared MCP
  server is heavy. This release flips the default and adds a
  manifest-level JSON cache at
  `~/.yanshi/cache/skill_mcp_discovery.json` (24-hour TTL,
  keyed by a stable SHA-256 of the manifest's `mcp_servers`
  section — including `name`/`command`/`args`/`env`, excluding
  `timeout_secs`/`max_concurrent_calls` which don't affect tool
  advertisements). Cache hits return in microseconds; cache
  misses show an `indicatif` spinner while the servers are
  spawned. The summary line now identifies which path was
  taken (`cache hit` / `fresh discovery (cached for next run)` /
  `forced re-discovery` / `skipped`). New `--no-mcp-discovery`
  flag opts out entirely; `--refresh-mcp-cache` busts the cache
  on demand. The old `--with-mcp-discovery` flag is kept as a
  no-op + deprecation warning so existing scripts don't break.
  13 unit tests in `commands::skill::mcp_discovery_cache::tests`
  (hash stability across env iteration order / server ordering;
  hash distinguishes argv / command / env-value changes; hash
  ignores timeout; load/save round-trip; load returns empty on
  missing file / schema mismatch / malformed JSON; TTL fresh/
  stale/unknown) + 4 hermetic CLI tests covering the
  deprecation warning, baseline (no warning when not set),
  `--no-mcp-discovery` short-circuit (with an MCP-declaring
  skill whose server script doesn't exist, so a spurious spawn
  would fail loudly), and stray-flag-without-`--explain-permissions`
  note.
- **`docs/ROADMAP_v2.md`** consolidated post-v1.0 roadmap
  (P10.19.3). Single source of truth for "what comes after v1.0
  GA", consolidating signals previously scattered across
  `RoadMap.md` Later Tracks, `TODOs.md` v1.x entries, and
  `docs/archive/PROJECT_EVALUATION_2026-05-19.md` §7. Ten themes
  (LLM/provider expansion, memory/RAG, server platform, Web UI
  debugger-scope, distributed/worker, Harness H6, plugin
  runtime WASM, perf, ops tooling, docs/contributor experience)
  with backreferences to the canonical `TODOs.md` IDs. Explicit
  v2 non-goals carry the v1 non-goals forward + add
  operator-dashboard Web UI per P10.17.1. `RoadMap.md::Later
  Tracks` gains an inline pointer at the top of the section so
  future contributors land on the consolidated view first.
- **`yanshi mcp config list --format json-envelope`** support
  (P10.11.3). The audit of all `format: String` clap fields in
  `yanshi-cli/src/main.rs` found exactly one holdout that
  didn't accept `json-envelope`: this one. The legacy `--format
  json` bare-body shape is preserved for back-compat; the new
  envelope mode wraps the same `{source, servers}` payload in
  the canonical `yanshi.cli/1` wire schema. Hermetic CLI test
  added alongside the existing json test.
- **`cargo xtask check-changelog`** subcommand (P10.18.2). Fails
  when a non-trivial source change versus a base ref (default
  `origin/main`) didn't touch `CHANGELOG.md` AND no commit body
  in the branch range carries `chore(skip-changelog)`. Trivial
  paths (`docs/`, `*.md`, `Cargo.lock` / `package-lock.json`,
  `.github/workflows/`, `tests/`, `**/fixtures/`, `*.test.ts` /
  `*.test.rs`) are excluded — that classifier is the
  single-source-of-truth pass/fail boundary, pinned by a
  dedicated unit test. Not wired into `quality.yml` today;
  available for manual + local pre-commit use. New
  `check_changelog_tests` module covers the 4 outcomes
  (only-docs / changelog-touched / skip-marker / source-but-no-
  signal) with a git fixture per test.
- **Checkpoint schema documentation** (P10.1.2) —
  `docs/CHECKPOINT_SCHEMA.md` formally documents the
  `decode_checkpoint_flow_value` warn-vs-silent fallback
  asymmetry: tagged-but-corrupt values warn loudly so a writer
  regression is debuggable; genuinely untagged legacy values
  fall back silently because spamming the operator on every
  pre-0.2 resume would be noise. The doc names the tests that
  pin each branch + the operator-facing diagnostic surface
  (`tagged ... but failed to deserialize` substring) so the
  contract stays auditable. STABILITY.md cross-references it.
- **Playwright UI e2e wired into CI** (P10.17.4). The 6 specs in
  `yanshi-ui/e2e/` (across two files: `runs-new.spec.ts` +
  `harness-sessions.spec.ts`) now run on a new
  `.github/workflows/ui-e2e.yml` workflow — `workflow_dispatch`
  + nightly schedule at 10:30 UTC, **not** in
  `quality.yml::release-gate.needs`. The pattern mirrors
  `llm-live.yml`: manual + nightly catches regressions between
  releases without the build + browser-install + flakiness tax
  on every PR. `@playwright/test` promoted from optional dev
  dep to a real `devDependencies` entry; new `npm run e2e`
  script plus a `playwright.config.ts` (Chromium-only, JUnit
  XML + HTML report on CI, trace-on-first-retry). Full
  operator + CI runbook in `yanshi-ui/e2e/README.md`. The
  CI job spins up a Postgres 16 service container, builds the
  server (release), boots it in the background with a 30s
  readiness probe against `/ui`, runs the suite, and uploads
  the `playwright-report/` HTML report as an artifact on
  failure. `workflow_dispatch` accepts an optional
  `spec_filter` input that maps to `playwright test -g
  <pattern>`.
- **Server-side `?filter=` pre-filter for events history**
  (P10.17.3). `GET /v1/runs/{id}/events/history?filter=<expr>`
  accepts the same grammar as the client-side `eventFilter.ts`
  (kind=/kind!=/kind~ + step compares joined by AND). Long runs
  no longer need to ship every event over the wire just to be
  filtered client-side. New `yanshi-server::events_filter`
  module with 21 unit tests pinning every clause shape +
  case-insensitivity + the AND-inside-value non-split rule +
  every parse-error path; new self-skipping integration tests
  in `yanshi-server/tests/events_filter_route.rs` cover
  kind-contains + after_seq+filter compose + 400-on-bad-expr +
  empty-param no-op. UI side: run-console history fetch now
  passes the operator's saved filter expression on initial
  attach; 400 responses fall back to no-filter so a malformed
  expression still loads the timeline (the inline parse error
  from the client `compileFilter` is what the operator
  actually sees and fixes).
- **Web UI preferences sync to `/v1/preferences`** (P10.17.2).
  Selected localStorage values now round-trip through the
  server's tenant-scoped preferences API so an operator's
  settings roam across browsers. Pure-helper module
  (`yanshi-ui/src/preferences.ts`) lists the syncable keys —
  tenant ids, profile selections, harness runtime kind, per-run
  event-filter expressions — and explicitly excludes the API
  token, workflow YAML drafts, harness user_input prompts, and
  workspace_root paths (security / size / machine-specific).
  React hook (`usePreferenceSync.ts`) GETs once per
  `(apiToken, tenant)` pair, debounces PUTs at 500 ms via a
  last-write-wins queue, flushes on unmount. localStorage stays
  as a fast first-paint cache. End-to-end wired today for the
  run-console tenant; the other 6 keys are mapped in the helper
  with the same 3-line pattern available for replication —
  tracked as follow-up work inside the new
  `docs/WEB_UI.md § Durable preferences (P10.17.2)` table.
  28 PASS in `preferences.test.ts` (bun-driven; same node-tsx
  pattern as `eventFilter.test.ts`); `npx tsc --noEmit` clean.
- **`yanshi harness replay <session_id>`** subcommand
  (P10.10.2). Re-streams a persisted JSONL session log with
  time-paced output (sleeps between events based on their
  original `ts` deltas). Useful for debugging long-finished
  sessions where the *pacing* of events carries diagnostic value
  — e.g. spotting a tool call that fired right before a stall.
  `--speed 1x` (default) honours the original timing; `2x` /
  `0.5x` scale it; `inf` / `instant` skip all sleeps (== `resume`
  but routed through the per-event formatter). `--from-seq` /
  `--to-seq` clip the visible window; `--filter-kind` (repeatable)
  acts as an OR include-list over the `kind` discriminator.
  `--output {text, stream-json}` — `json` / `json-envelope` are
  rejected because replay is open-ended (mirrors the
  `workflow logs --follow` rejection). The 1-hour sleep cap
  prevents an overnight idle gap from hanging the replay (run
  with `--speed inf` if you really want the original timing).
- **`yanshi memory prune --layer {preference,entity_facts}
  --db <path> --older-than <duration>`** subcommand (P10.7.1)
  wires the existing trait surface
  (`PreferenceStore::prune_older_than`,
  `EntityFactStore::prune_invalidated`) to a CLI front-end so
  operators can drop stale memory rows from the command line.
  `--older-than` accepts `<integer><unit>` where unit ∈
  `{s, m, h, d, w, y}` — bare integers are rejected so a typo
  (`--older-than 30` instead of `--older-than 30d`) can't
  silently mean "30 seconds". `--format text` (coloured ✓ line
  with row count) or `--format json-envelope` (canonical
  `yanshi.cli/1` wrapper carrying `{layer, db, older_than,
  older_than_seconds, removed_rows}`). The `entity_facts` path
  is invalidation-bounded — active facts are never touched, even
  with `--older-than 0s`. Session + semantic layers are out of
  scope for this slice because they expose per-session clear
  rather than retention-based prune. Defaults `--db` to
  `~/.yanshi/memory.db` matching the agent-runtime
  convention. 6 unit tests + 5 hermetic integration tests cover
  the parser, layer routing, missing-db error, unsupported-layer
  rejection, and the end-to-end round-trip against a real
  on-disk SQLite file seeded via the public memory-crate API.
- **`yanshi workflow run --server` flag validation** (P10.11.4):
  closes the silent-drop class of bug for the local-only flag set
  by rejecting up front with a single-line actionable error when
  any of `--model`, `--execution-mode` (non-default),
  `--max-concurrency` (non-default), `--run-dir`, `--watch`,
  `--output`, `--input`, `--dry-run`, `--timeout` (non-default),
  or `--max-retries` (non-zero) is combined with `--server`. Two
  categories: **always-local** (filesystem + in-process flow:
  `--run-dir` / `--output` / `--watch` / `--dry-run`) each name a
  concrete server-side alternative (e.g. `--watch` points at
  `yanshi workflow logs <run_id> --follow`); **future API
  addition** (server-side execution knobs the wire format could
  accept but doesn't today: `--model` / `--execution-mode` /
  `--max-concurrency` / `--input` / `--timeout` / `--max-retries`)
  each name P10.11.4 so curious operators can find the follow-up
  work. Defaults pass through silently — the validator only fires
  when the operator explicitly overrode a flag. 13 unit tests + 11
  hermetic CLI tests cover every guard + the baseline-passes path.
- **`yanshi skill run --server <url>`** dispatches the skill
  to a remote `yanshi serve` instance via
  `POST /v1/skills/{name}:run` (P10.11.2). Mirrors the
  `workflow run --server` pattern: submits, polls
  `GET /v1/runs/{id}` until terminal status (succeeded / failed /
  cancelled), and pretty-prints the final row. The positional
  argument shifts semantics in server mode — it's the skill NAME
  resolved via the server's `YANSHI_SKILLS_INDEX` catalog, not
  a local filesystem path. New flags: `--server <url>`,
  `--auth-token <token>`, `--tenant <id>`. Local-only flags
  (`--memory`, `--model`, `--session`, `--trace`) are rejected
  with a single-line actionable error when combined with
  `--server` because the wire contract doesn't accept per-request
  overrides today. `--output` accepts `json-envelope` in server
  mode (canonical `CliJsonEnvelope`); the local-mode `json`
  value is rejected to keep the wire-schema surface narrow.
  Hermetic axum mock-server integration tests cover the
  submission round-trip, envelope wrap, local-only flag
  rejection, and 404 propagation.
- **`yanshi workflow logs <run_id>`** subcommand consumes the
  server's persisted event log (P10.11.1). Without `--follow`,
  fetches the history snapshot as a single JSON array via
  `GET /v1/runs/{id}/events/history`. With `--follow` (`-f`),
  opens an SSE stream against `GET /v1/runs/{id}/events` and
  prints each event as it arrives until the server closes the
  connection. Supports `--after-seq <n>` for resuming reconnects,
  `--format text|json|json-envelope` (envelope incompatible with
  `--follow` — rejected with a clear error since envelopes are
  bounded and follow streams are not), `--server` / `--auth-token`
  / `--tenant` matching the other server-backed `workflow`
  subcommands. Hermetic round-trip tests via a tiny axum mock
  server (no Postgres required).
- **`yanshi harness run --approve {none,cli,auto-allow,auto-deny}`**
  wires `HookedTool` into the CLI Harness path (F-A2-11, `9d386b3`).
  Combined with `--profile production`, every NonIdempotent tool
  call surfaces an interactive operator prompt. Default `none`
  preserves legacy behaviour for back-compat.
- **`yanshi skill run --output {text,json}`** emits a single
  JSON object on stdout suitable for piping into jq / downstream
  tooling (F-A2-6, `9a96058`). Banners go to stderr in JSON mode;
  `--trace` inlines the runtime trace under the `trace` key.
- **`yanshi doctor` opens its Config section with a
  human-readable source label** ("`/path/to/models.yml` (overrides
  built-in)") instead of the bare Rust-debug enum (F-A7-4,
  `bdaff36`). JSON output gains `models_config_source_kind` as a
  stable snake_case enum.
- **`yanshi llm models --refresh-from-api`** live-queries each
  OpenAI-compatible provider's `/v1/models` endpoint and prints the
  delta vs the local registry (F-A7-6, `6290ca9`). Output groups
  per-provider: `new` (provider-side additions to add to
  `models.yml`), `only_local` (deprecated / typo / private
  deployment), `shared` (count). Read-only; respects `--provider`
  filter. Currently supports openai / moonshot / stepfun /
  dashscope. 5 new unit tests on URL construction + truncation.

#### Agent runtime

- **ReAct steering note on repeat tool calls** (F-A2-13, `d7651f7`).
  When the LLM returns the exact same `(tool, params)` two
  iterations in a row, the second tool result memory message gets
  an `[yanshi steering note (F-A2-13): ...]` nudging the model
  to advance. Advisory only — tool still runs. Trace-side
  `AgentStepKind::ToolResult` stays clean.

#### Skill manifest

- **SKILL.md frontmatter `model:` field now honoured** (F-AF-2,
  `100c267`). Was silently dropped because the field wasn't on
  `SkillMdFrontmatter`. Empty/whitespace strings collapse to
  `None` so a stray `model: ""` doesn't propagate.

#### Examples

- **`examples/applications/code-reviewer-write/`** (`83a9765`)
  end-to-end Harness Mode approval gate validation binary; uses
  `wrap_registry` + `CliApprovalProvider` to exercise the
  approval flow against real shell + file:write tools. Includes
  `--auto-approve` for CI smoke and `--prefetch-diff` workaround
  for moonshot-v1-128k's loop pathology.
- **`examples/applications/research-assistant/`** (`b492f6a`)
  L1 binary fetching arxiv papers, deduping via
  `SqliteEntityFactStore`, summarising via a single LLM call.
- **`examples/applications/doc-translator/`** — full A6 spec
  shipped in 4 iterations:
  - iter 1 (`141b993`): `map parallel + LLM` primitive validator,
    4 langs hardcoded.
  - iter 2 (`4b882eb`): real file I/O, 2 files × 4 langs.
  - iter 3 (`ec2c15d`): `file_list × lang_list` cross-product via
    Tera template.
  - iter 4 (`35c1d01`): real file discovery via the new
    `type: shell` node. Adding a markdown file to `input/` is a
    zero-line workflow change.

### Changed

- **Workspace package metadata centralised via `[workspace.package]`**
  (P10.0.2-FU1). New shared metadata table in the root `Cargo.toml`
  pins `edition` / `license` / `authors` / `repository` / `homepage`
  once for every publishable member; each member's `[package]` table
  opts in with `<field>.workspace = true`. Clears the
  `manifest has no documentation, homepage or repository` warning
  from `cargo publish --dry-run` across all 15 publishable crates.
  Two pre-existing `repository = "https://github.com/yanshi/yanshi"`
  values (an org that doesn't exist — 404) replaced with the inherited
  canonical URL `https://github.com/yuxuetr/yanshi`. A future repo
  rename is now a one-line change to the workspace block instead of
  a 15-file `sed`. `xtask` stays opted out (`publish = false`).
- **Workspace-internal `[dependencies]` now carry explicit
  `version = "X.Y"`** (P10.0.2). Every path-dep on a workspace
  crate was previously bare `{ path = "../..." }`, which
  `cargo publish` rejects with `all dependencies must have a
  version requirement specified when publishing`. Eleven Cargo.toml
  files were patched (the four leaf crates `yanshi-core` /
  `yanshi-tools` / `yanshi-rag` / `yanshi-db` had no
  internal deps). Workspace builds and test suites are unaffected
  — `path =` still wins at resolution time; the version is only
  consulted when the package gets published. `[dev-dependencies]`
  path-deps are left as-is (cargo strips them at publish).
- **`Cargo.lock` refreshed off the yanked `slab v0.4.10`** (P10.0.2).
  `cargo update -p slab` bumped to `v0.4.12`, removing the
  `package 'slab v0.4.10' in Cargo.lock is yanked in registry
  'crates-io'` warning from `cargo publish --dry-run`.
- **Default template node behaviour**: auto-detects JSON when
  rendered output starts with `[`/`{`. Workflows that already set
  `output_format: "json"` are unaffected; new workflows can omit
  the hint. Parse failure falls back to legacy String wrap (safe).
- **Validator behaviour for template nodes**: arbitrary
  `parameters` keys no longer false-warn (F-A6-6, `8b73298`).
  Template's whole point is to consume arbitrary Tera context.
  Typo-detection for other node types (closed ParamSpec) is
  unaffected.
- **`LLMConfig::validate()` is lenient on missing API keys**
  (P10.3.1). Previously, `Yanshi::init()` against the bundled
  `default_models.yml` would fail-close for a fresh user with
  only `OPENAI_API_KEY` set, because the YAML references ~9
  providers and the strict validator required every key to be
  present. Now `validate()` emits an `eprintln!` warning naming
  each missing provider + the affected models, and returns
  `Ok(())` so init proceeds. The fail-fast moves to the lookup
  path: `ModelRegistry::get_provider("anthropic")` now returns
  `LLMError::MissingApiKey { provider: "anthropic" }` (actionable
  — names the env var to set) when that provider was skipped at
  init time, rather than the misleading `UnsupportedProvider`.
  **Migration**: callers that need the old fail-close semantics
  (e.g., `yanshi doctor --profile production` health checks)
  should use the new `LLMConfig::validate_strict()` method, which
  returns `Err(LLMError::MissingApiKey)` on the first missing
  configured key. Structural validation (unsupported vendor,
  out-of-range numeric fields) remains a hard error in both
  paths.

### Fixed

- **`HarnessProfile::Local` silent-allow footgun documented**
  (F-A2-12, `c552d3c`). Rustdoc on the enum variants + a footgun
  callout in `docs/HARNESS_MODE.md` make it clear that without
  `.with_profile(HarnessProfile::Production)`, the approval gate
  doesn't fire for NonIdempotent tools.
- **`yanshi workflow validate` no longer false-warns on map's
  `input_list` / `max_concurrent`** (F-A6-2, `a4e89e8`). ParamSpec
  list bumped.
- **`yanshi skill run` answer recovery from truncated JSON**
  (F-A2-1, `ec3e1a7`). Best-effort `answer` field extraction in
  `react/parser.rs` when `max_tokens` truncates the response
  mid-JSON. 6 new tests.
- **Doctor integration test patched** for the F-A7-4 output-format
  change (`8b73298`). Previously only the unit tests covered it.
- **94 text models bumped from `max_tokens: 4096` to `32768`**
  (F-A7-8, `42c3225`). 1M+ context models can sustain much
  longer outputs; the previous cap was too conservative.
- **`LLMError::MissingApiKey` renders an actionable one-liner**
  (F-AF-4, `6b89317`). Names the provider-specific env var (e.g.
  `MOONSHOT_API_KEY (or MOONSHOT_KEY)`), points at
  `~/.yanshi/.env`, suggests `yanshi config init` to
  generate the template, and references the docs. New
  `env_var_hint(provider)` helper has table coverage for 6
  providers with a generic fallback. 3 new unit tests.
- **A1.5 persona now re-measures LUFS before save** (F-EX-1,
  `b35f371`). Adds a step 6 audio_loudness call after
  normalize_lufs / fade so the final report uses the **实测**
  value rather than the target parameter (integrity issue
  caught in R1 dogfooding).

### Changed (stability)

- **`yanshi-mcp::server` promoted from Experimental to Beta**
  (P10.5.2). The closed method set is now pinned:
  `initialize` / `notifications/initialized` / `tools/list` /
  `tools/call`. New methods may be added in minor releases; the
  existing four stay wire-stable. New public surface includes
  `MCPServer::handle_request` (the single request → response
  entry point, now `pub` so non-stdio transports can drive it)
  and `STABLE_PROTOCOL_VERSION = "2024-11-05"` (bumping this is
  the explicit signal that the wire contract changed). Backed by
  6 fixture-driven compat tests + 2 invariant tests in
  `yanshi-mcp/tests/server_contracts.rs`. The fixture format
  pins required fields + exact values + error envelope shapes but
  tolerates additive fields, matching the Beta promise. See
  `docs/STABILITY.md` for the full contract and fixture-ownership
  row.

### Removed

- **`yanshi-viz` crate** (P10.13.1). Removed alongside the
  `/v1/runs/{id}/graph` REST endpoint, the `yanshi workflow
  graph` CLI subcommand, the `RunGraphResponse` shape, and the
  Mermaid `<pre>` block in the Web UI. An honest audit revealed
  the "DAG visualisation" surface was a button grid of node
  status badges plus the raw Mermaid markdown text in a code
  block — no SVG, no spatial layout, no edges. The data-plumbing
  cost (an entire workspace crate + a beta REST route + a CLI
  subcommand + the UI fetch path) was disproportionate to the
  rendering value. The UI's node-status grid is now derived
  entirely from event payloads, which was already the source of
  truth for execution state. A future RFC may revisit graphical
  DAG / agent topology rendering as an additive feature
  (e.g. mounting mermaid.js to render `yanshi workflow
  validate --output mermaid` as SVG client-side); see
  `docs/ROADMAP_v2.md` Theme D for the decision rationale.
  Stability impact: `/v1/runs/{id}/graph` was listed as Beta in
  `docs/STABILITY.md`; the row was deleted with a P10.13.1
  cross-reference so anyone hitting the old endpoint gets a
  pointer to the rationale.

- **`yanshi-mcp::client_old`** and the legacy `transport` module
  it depended on (P10.5.1). Both were `#[doc(hidden)]` and had no
  external callers in the workspace; deleting them removes ~330
  lines of dead code. The current `transport_new` module is renamed
  to `transport` so the post-cleanup name is internally consistent.
  A `#[deprecated]` `pub use transport as transport_new;` re-export
  preserves the old import path for any 3rd-party caller through
  the transition window — they get a deprecation warning instead of
  a hard break. A compat unit test pins the alias's type identity
  so the re-export can't silently degrade. yanshi-mcp is below
  the stability tier line per `docs/STABILITY.md`, so this rename
  is in scope.

- **6 dead `yanshi-llm/config/models/*.yml` files** (F-A7-3,
  `5578743`). The vendor_configs split was never read by the
  runtime registry. 4 misdirecting docs updated to point at the
  real source (`templates/default_models.yml`).

### Docs / conventions

- **LLM provider module promotion criteria 1-pager** (P10.3.2,
  `docs/LLM_PROVIDER_MODULE_PROMOTION.md`). Pins per-vendor
  promotion triggers for the four OpenAI-compat vendors
  (GLM, DashScope, DeepSeek, MiniMax) that currently share
  `OpenAIProvider`. Documents six concrete divergence signals
  (tool-call shape, multimodal shape, streaming-protocol,
  auth/endpoint topology, vendor-specific feature with no
  upstream OpenAI mapping, operator-side request) and ties
  each to an existing `cross_provider_*` consistency-test
  invariant where applicable, so "should I extract X?" is
  answerable in minutes against the empirical state of the
  nightly live suite rather than a re-derived analysis. As
  of 2026-05-20 no trigger has fired. Closes a *tracking*
  TODO; no Rust code shipped. Same posture as the WASM
  1-pager (P10.19.1) and the H6 criteria 1-pager (P10.10.1).
  `docs/ROADMAP_v2.md` Theme A updated to mark closed with
  a pointer to the criteria doc.

- **H6 promotion criteria 1-pager** (P10.10.1,
  `docs/H6_PROMOTION_CRITERIA.md`). Pins per-item promotion
  triggers for the 5 H6 (Phase Harness "advanced compatibility")
  items so the TODO list doesn't accidentally pull them en
  bloc when a single one gets demand. For each of
  slash-command expansion, TUI product shell, OpenHarness
  config import, plugin compatibility adapters, and provider
  subscription bridge: documents the demand signal that would
  tip the scale, the per-item RFC scope, the estimated effort,
  and the explicit cross-reference to the non-goal stance in
  `RoadMap.md` for the two items currently documented as
  non-goals (TUI, subscription bridge). Closes a *tracking*
  TODO — no code shipped; the value is preserving the per-item
  review discipline so a future P11.x opens a focused RFC
  instead of a five-item batch. Same posture as the WASM
  1-pager (P10.19.1). `docs/HARNESS_MODE.md` H6 row links to
  the criteria doc; `docs/ROADMAP_v2.md` Theme F flags
  non-goals explicitly.

- **WASM plugin runtime 1-pager** (P10.19.1,
  `docs/WASM_PLUGIN_EVALUATION.md`). The narrowed
  wasmtime-vs-wasmer-vs-extism comparison concludes that
  wasmtime + WIT + WASI 0.2 is the right runtime *if* we adopt
  WASM, and decides to **push the adoption itself to v2.0**.
  Subprocess JSON-RPC is mature and the 50-200 ms subprocess
  cold start is dominated by the first LLM call's TCP
  handshake in any realistic workflow — the ~6-8 person-week
  pre-GA investment doesn't fix any current friction. The
  `PluginRuntime::Wasm` enum variant stays in `manifest.rs` as
  a forward-compatible reservation; v2 wires the real host
  when at least one of the documented re-evaluation triggers
  fires (latency complaint, polyglot demand, distribution
  complaint, or peer-project precedent). Closes the
  P10.19.1 HIGH pre-GA item from the v1.0 RC backlog.

- **R1 → R4 reflection sequence** (`docs/L1_L3_REFLECTION_*.md`,
  `b3ee990` / `2d3d06d` / `edd9572`). R2 froze the L1↔L3
  selection rule + per-application matrix; R3 documented the
  R2-follow-up sweep + 6 emergent patterns; R4 captured the A6
  sweep + 6 more patterns. R4 §5 has the cumulative arc.
- **Examples conventions** (`examples/README.md`) gained two
  cross-cutting bullets: LLM-judgement output is
  non-deterministic — run multiple times, union the findings
  (F-A2-5, `0d921aa`); translation workflows must guard
  `source_lang != target_lang` before LLM dispatch (F-A6-4,
  `907b6e7`).
- **`docs/HARNESS_MODE.md`** got a footgun callout for the
  Local-vs-Production approval-gate asymmetry + an inline comment
  in the canonical snippet (F-A2-12, `c552d3c`).
- **`docs/AGENT_SDK.md` gained two reference sections**
  (`b35f371`): a `FlowValue` field reference table enumerating
  exact field names per variant (F-DOC-2; prevents `media_type`
  vs `mime_type` round-trips), and a "Loading
  `~/.yanshi/.env` from standalone binaries" canonical 6-line
  snippet (F-A7-7; standardises the pattern used by every
  standalone example binary).
- **`yanshi-llm/README.md` § Moonshot** documents the
  kimi-k2.6 `temperature: 1.0` constraint with the exact API 400
  error message (F-A7-5, `b35f371`), plus the org-level
  concurrency limit of 3 that motivates `max_concurrent: 3` in
  `map parallel` workflows.

### Known still-open

- **F-A6-8** (Tera library quirk, not an yanshi bug): Tera
  `loop.parent.*` introspection doesn't work in this Tera
  version. `set_global` accumulator is the documented workaround
  in `examples/applications/doc-translator/workflow-iter3.yml`.
- **A6 iter 5** (100+ file stress test): all platform capability
  axes are now in place; iter 5 is a quantity question rather
  than a capability one. Probably worth running once before any
  v0.3.0 cut.
- **Phonon-external items** (F-PH-1/2/3, plus F-DOC-3/4 docs that
  live phonon-side): not in the yanshi workspace; tracked
  separately.
- **All other yanshi-internal items from the R1 → R4 sweep
  are now closed** (F-DOC-2, F-A7-5, F-A7-6, F-A7-7, F-AF-4,
  F-EX-1 all landed; see Added / Fixed / Docs above).

---

## [0.2.0] and earlier

No structured changelog kept before this entry. For change history
prior to v0.3.0 prep, see `git log` (most commits follow
Conventional Commits) and `docs/archive/PROJECT_EVALUATION_2026-05-19.md`
for the most recent cumulative project evaluation (or
`docs/archive/PROJECT_EVALUATION_2026-05-14.md` for the prior one).
