# Yanshi Roadmap

Last updated: 2026-11-04

This roadmap is forward-looking only. For implemented behavior and current
stability boundaries, see:

- `docs/CURRENT_STATUS.md`
- `docs/STABILITY.md`
- `docs/API_COMPATIBILITY.md`

The architecture direction is set by `docs/RFC_CRATE_ARCHITECTURE.md`
(contract kernel) and validated by `docs/ARCHITECTURE_EVALUATION_2026-06-20.md`.
The product direction below was reset on 2026-11-04 by
`docs/PROJECT_EVALUATION_2026-11-04.md` §7. Directional post-v1 ideas are
staged in `docs/ROADMAP_v2.md`.

## Direction (reset 2026-11-04)

Yanshi is a Rust-native runtime for **multimodal content production**: a
source (an idea, a link, a set of documents, a recording) goes in, and a set
of finished deliverables comes out — reports, mind maps, audio overviews,
video overviews, slides, flashcards, quizzes, infographics, data tables,
and domain engineering diagrams. The reference product shape is a
NotebookLM-style studio, with a richer catalogue of outputs and
domain-specific generators on top.

It is **not** a coding-agent framework. Harness Mode, the OS sandbox,
`code_exec`, and the distributed worker stay as runtime substrate, but no
further investment goes into them until a content-production application is
end-to-end DONE (`examples/applications/content-studio/`).

The runtime is organised as five stages, each mapped to contracts that
already exist:

| Stage | What it does | Contract / crate |
|---|---|---|
| **Sources** | ingest URL / file / recording, chunk, index, persist per notebook | `yanshi-rag` loaders + `KnowledgeBackend`, `asr` node |
| **Planner** | an agent decides *which* deliverables and with what outline / style — content-level planning | `ReActAgent` + `WorkflowTool` (each generator is a tool) |
| **Generators** | one hand-authored DAG template per deliverable, typed `inputs`, structured output with citations | `yanshi-config` `FlowDefinitionV2`, `json_schema`, `Map` fan-out |
| **Renderers** | structured content → file (pdf / pptx / xlsx / png / mp3 / mp4 / diagram) | new `render_*` nodes; external binaries (pandoc + typst, ffmpeg, mermaid-cli / D2) run as sandboxed shell / MCP tools |
| **Artifacts** | deliverables stored, listed, downloaded, retained per tenant / notebook | `FlowValue::File` + `run_dir` + `ArtifactRepo` + `/v1/runs/{id}/artifacts` |

Two decisions that follow from this shape:

- **The LLM plans content, not graphs.** Generators are static DAG templates
  so cost is predictable, quality is regression-testable, and failures are
  attributable. `DynamicWorkflowAgent` stays available for the long tail,
  never as the default path.
- **Media is a first-class value.** Modality nodes emit `FlowValue::File`
  written to the run directory; the state pool holds paths and hashes, never
  base64 blobs.
- **Figure fidelity comes from the figure tier, not the slide writer**
  (2026-11-04). Engineering / scientific drawings (circuits, chemical
  structures, plots, pathways, gene maps) are rendered to vector SVG by a
  `figure` node with several language backends — TikZ family via TeX Live
  (`standalone` → `lualatex` → `pdftocairo -svg`), Typst + cetz, Mermaid / D2 /
  Graphviz, and a sandboxed Python image for data-driven science figures —
  all behind one validate → render → feed-back loop. The document / slide
  container stays simple: pandoc (`--reference-doc` for theming; verified to
  embed SVG as vector in PPTX, 2026-11-04). A programmatic PPTX writer
  (python-pptx / pptxgenjs) is deferred until a generator needs a layout
  pandoc cannot express (native charts, free-positioned callouts).

The four execution paradigms (static DAG / native loop / harness / dynamic
workflow) and the contract kernel remain as described in
`docs/ARCHITECTURE.md`; nothing here changes the dependency laws.

## Target Product Shape

Three access modes, in this priority order:

1. **Local server / daemon** — notebooks, sources, `generate` requests, run
   events over SSE, artifact download. This is the surface a product UI
   talks to.
2. **CLI** — the same generators runnable from a URL or file in one command,
   with artifacts copied to an output directory; diagnostics, trace replay,
   evaluation.
3. **Rust SDK** — embedding `Flow`, `AgentRuntime`, `ToolRegistry`,
   `SkillBuilder`, RAG, memory and tracing in applications.

## Active Track — Y: Content Studio

Tracked task-by-task in `TODOs.md` § Y. Order and acceptance in one line each:

| Stage | Goal | Done when |
|---|---|---|
| Y0 design | `docs/RFC_CONTENT_STUDIO.md`: artifact model, deliverable schema with citations, notebook entity, renderer strategy | RFC merged with a "not doing / re-evaluate when" section |
| Y1 artifacts | modality nodes emit `FlowValue::File`; `file` node / `FileTool` / Tera accept it; `GET /v1/runs/{id}/artifacts[/{id}]`; CLI `--output-dir` | a `tts` → `file` workflow leaves no base64 in the state pool, and the server serves the mp3 |
| Y2 cost + resume | streaming `usage` for OpenAI-compatible vendors; real token accumulation; per-call modality pricing; run-level `cost_limit_usd`; checkpoint/resume reachable from CLI and server | a ten-step pipeline failing at step nine resumes without re-billing steps one to eight (proven by cost events) |
| Y3 renderers | embedded light renderers (`render_table` csv/xlsx, `render_svg` → png); `media-tools` MCP server over ffmpeg (concat, loudness, mux, subtitles); `render_document` over pandoc (docx / pptx / html direct, pdf via `--pdf-engine=typst`; decided 2026-11-04, no LibreOffice); `figure` node (TikZ / Typst-cetz / Mermaid / D2 first; sandboxed Python science backend second) with validate → render → feed-back loop | each renderer has a mock-free smoke test behind a feature or `#[ignore]` guard |
| Y4 three generators DONE | `report`, `mindmap`, `audio_overview` templates + a planner agent that picks and parameterises them; `examples/applications/content-studio/` | one command turns one URL into three downloadable deliverables; CI smoke with the mock provider |
| Y5 notebook entity | `notebooks` / `sources` / `source_chunks` / `notebook_artifacts`; per-notebook BM25 index rebuilt from persisted chunks and cached in-process (decided 2026-11-04; Qdrant per-notebook collection stays opt-in, re-evaluate when a rebuild measures > 500 ms or keyword recall fails the citation tests); `POST /v1/notebooks/{id}/generate`; unified citation structure in every generator output | the three generators run against a notebook's sources and every claim in the report cites a source chunk |
| Y6 second batch | `slides` (pandoc → pptx with themed reference doc, figures from the `figure` tier; beamer / typst-touying for a print-quality PDF variant), `flashcards` / `quiz`, `infographic` (SVG template → png), `video_overview` (storyboard + voice + mux), `data_table`, domain diagram templates; video / image-to-image provider breadth (DashScope Wan video, MiniMax, Kling) | each added to the studio catalogue with its own smoke test |
| Y7 governance for this product | budget-threshold approvals ("ask above X"), a `publish` action class in tool metadata, the four cheap gateway escapes from the 11-04 eval (template `get_env`, markmap path, rag URL, http approval digest) | production profile asks before spending over the threshold and before any publish |

## Deferred (re-evaluate when stated)

These are not wrong, but they solve problems this product does not have
yet. Each carries the condition that reopens it.

| Deferred | Re-evaluate when |
|---|---|
| Distributed worker control plane (in-memory, no deadline, never-heartbeated task hang) | a content-studio deployment needs more than one execution host |
| Remaining multi-replica gaps (harness session lease, seq allocation race, replica-lag backfill window) | a deployment runs `replicaCount > 1` **and** harness sessions in production |
| Harness Mode coding providers (`AgentsMd` / `TodosMd` / `RoadmapMd`), background tasks (`task_*`) | a user asks for an interactive coding session, not a generation request |
| seccomp / Landlock refinement, `code_exec` egress proxy | a renderer needs network from inside the sandbox |
| MCP modern-era follow-ups (era probing, shared client construction) | a second Modern-era MCP server is actually used by a generator |
| `VerificationStrategy` / reflection entry points | a generator's quality gate needs an LLM judge (likely at Y6 `video_overview`) |
| Dependency-law activation beyond 3/8 | latent edges drop below 3 |
| `events` table partitioning | `pg_total_relation_size('events')` > 10 GB or a cleanup DELETE > 30 s |

## Standing Constraints

- Harness Mode stays additive: it wraps `AgentRuntime` / `ToolRegistry` /
  Trace / Skill contracts rather than replacing them (`docs/HARNESS_MODE.md`,
  `docs/archive/HARNESS_MODE_EVOLUTION.md`).
- Plugins stay subprocess JSON-RPC; WASM and native `dlopen` are not planned.
- Web UI is a debugger / run console (`docs/WEB_UI.md` § positioning). The
  product UI is a client of `/v1/*`, not part of this repo.
- Live provider tests stay opt-in with cost guards;
  `cargo xtask refresh-live-models` checks each default model is callable.
- Release gates: fmt, clippy `-D warnings`, rustdoc `-D warnings`, workspace
  tests with a live Postgres, `check-arch`, example validation.

## Non-Goals

- LLM-planned graph structure as the default generation path.
- In-process media processing (ffmpeg et al. run as sandboxed tools, never
  linked in).
- Native dynamic library plugins; unbounded remote code execution from
  marketplace packages.
- Executing LLM-generated code outside the isolated `ContainerBackend`.
- Slack / Telegram / Discord / desktop channel integrations before the
  content-studio API is stable.
- A Web UI required for headless operation.
