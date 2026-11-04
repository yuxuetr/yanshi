# RFC: Content Studio Runtime

- Status: **Decided** (2026-11-04). The three questions left open when the
  roadmap was reset — document renderer, figure/slide path, index
  persistence — were settled the same day; the decisions and the experiments
  behind them are recorded in §5 and §9.
- Parent: `RoadMap.md` § Direction; `docs/PROJECT_EVALUATION_2026-11-04.md`
  §7 (re-weighted evaluation and the per-deliverable gap table in §7.6).
- Tracking: `TODOs.md` § Y (Y0.1 is this document; Y1–Y7 implement it).
- Scope: the data contracts, storage layout, renderer strategy and API surface
  that turn the existing DAG + agent runtime into a content-production
  runtime. No change to the contract kernel, the `AsyncNode` contract, the
  workflow YAML `type:` names, or the dependency laws.

## 1. Problem

Yanshi is to be a **multimodal content-production runtime**: sources in
(an idea, a URL, uploaded files, a recording), finished deliverables out
(report, mind map, audio overview, slides, flashcards, quiz, infographic,
data table, video overview, domain engineering diagrams). The shape is
NotebookLM Studio's, with a richer and extensible catalogue.

The 2026-11-04 evaluation found the runtime underneath is sound (DAG
executor, four execution paradigms, nine LLM vendors, six modalities, MCP,
RAG, sandboxing, a gateway with cross-replica state) but that the product
layer is missing in eight specific ways (§7.2 M1–M8):

| # | Gap | Evidence |
|---|---|---|
| M1 | Media flows as base64 strings in the state pool; no node produces `FlowValue::File` | `yanshi-nodes-ai/src/nodes/tts.rs:127-134`, `text_to_image.rs:326`, `text_to_video.rs:171,224` |
| M2 | No per-call pricing for modalities; token budgets are dead (COR-1 / COR-2) | `yanshi-llm/src/providers/openai.rs:608-618`, `yanshi-agents/src/react/agent/core.rs:1240-1262` |
| M3 | Checkpoint / resume unreachable from CLI and server for DAG runs | `FlowExt::with_checkpointing` has no product caller |
| M4 | No artifact listing or download route; `ArtifactRepo` has zero callers | `yanshi-server` routes |
| M5 | No renderer tier (no ffmpeg, no document or diagram rendering) | `Cargo.lock` has only `csv` / `lopdf` / `plotters` |
| M6 | No notebook / source entity; BM25 index rebuilt per request | `yanshi-rag/src/knowledge.rs:77` |
| M7 | Video generation is Veo-only; chat image parts not parsed | `modality_dispatch` |
| M8 | No end-to-end generator exists; citations exist only inside the agent loop | `yanshi-agents/src/citation.rs` |

This RFC fixes the contracts so that Y1–Y7 can close those gaps without
re-deciding anything.

## 2. Five-stage model

Every deliverable is produced by the same pipeline. Each stage maps onto an
existing contract; the right-hand column is what is new.

| Stage | Input → output | Existing contract | New in Y |
|---|---|---|---|
| **Sources** | URL / file / text / recording → chunks with ids | `yanshi-rag` loaders + chunkers, `KnowledgeBackend`, `asr` node | `sources` + `source_chunks` tables; ingestion as a run (Y5) |
| **Planner** | request ("what do you want from this?") → chosen generators + their typed parameters | `ReActAgent` with `json_schema` output, `WorkflowTool` | planner skill in `examples/applications/content-studio/` (Y4.4) |
| **Generators** | sources + parameters → `Deliverable` JSON | config-first `FlowDefinitionV2` with typed `inputs`, `llm` / `rag` / `Map` nodes | one YAML template per deliverable kind (Y4, Y6) |
| **Renderers** | `Deliverable` → files | sandboxed `shell` tool, MCP tools, `code_exec` | `render_*` and `figure` nodes (Y3) |
| **Artifacts** | files → registered, downloadable, retained | `FlowValue::File`, `artifacts` table, run dir, cleanup | registration + routes + CLI output dir (Y1) |

Two stages are deliberately thin. The planner never authors graph structure,
and the renderers never run media code in-process. Both are decisions, not
omissions (§3, §5).

## 3. Decision: the LLM plans content, not graphs

A generator is a **hand-written DAG template** with declared `inputs`
(`FlowDefinitionV2.inputs`, `yanshi-config/src/config/v2.rs:13`). The LLM
decides *what* to make — outline, tone, length, which deliverables, which
figures — and fills those inputs. It does not decide *how* the pipeline is
wired.

Mechanics:

- Each template is registered as a tool through `WorkflowTool::with_schema`
  (`yanshi-agents/src/tools/workflow_tool.rs:41`), with the JSON schema
  derived from the template's `inputs` block. The planner therefore sees a
  closed menu of deliverables with typed parameters — the same shape as
  NotebookLM's Studio panel.
- The planner is a `ReActAgent` (or a `SkillBuilder`-assembled skill) whose
  final answer is `json_schema`-constrained to a plan: a list of
  `{ kind, params }`. Each entry becomes one `WorkflowTool` call.
- Content-level planning *inside* a generator (outline → sections, script →
  shots) is an ordinary `llm` node with `json_schema`, feeding a `Map` node.

Why not `DynamicWorkflowAgent` as the default: cost is unpredictable (the LLM
decides how many paid calls happen), quality is not regression-testable (no
two plans are the same graph), and a failure cannot be attributed to a
template. The dynamic path stays available for the long tail and is never the
path a catalogue entry takes.

## 4. Decision: artifacts and the deliverable contract

### 4.1 Media is a first-class value

Modality nodes (`tts`, `text_to_image`, `image_to_image`, `image_edit`,
`text_to_video`) and renderers write bytes to

```
<run_dir>/artifacts/<node_id>/<n>.<ext>
```

and emit `FlowValue::File { path, mime_type }` (`yanshi-value/src/lib.rs:32`).
The state pool holds the path; `state_size` becomes constant in the media
size. `text_to_video` downloads the vendor's temporary URL to disk with a size
cap instead of forwarding the URL. `output_format: data_uri` remains as a
compatibility switch (default `file`) so existing examples keep running.

Consumers accept `File` natively: the `file` node (copy / move / read), the
`FileTool`, the Tera helpers (`path`, `mime`, `size`), and the image inputs of
`llm` / `image_understand`. Base64 is produced only at the vendor boundary,
never stored.

### 4.2 Registration, download, retention

When a run finishes, every `File` value in the final state pool is registered
in the existing `artifacts` table (`yanshi-db/migrations/0001_initial_schema.sql:56`:
`id`, `run_id`, `node_id`, `name`, `path_or_url`, `mime_type`). Migration
`0012` adds `size_bytes BIGINT` and `sha256 TEXT` so listings and integrity
checks do not touch the file. Routes:

```
GET /v1/runs/{id}/artifacts                 → [{ id, node_id, name, mime_type, size_bytes, sha256 }]
GET /v1/runs/{id}/artifacts/{artifact_id}   → streamed body, Content-Type from mime_type, tenant-scoped (404 across tenants)
```

CLI: `yanshi workflow run` prints the artifact table and `--output-dir <dir>`
copies files as `<node_id>/<file>`; `--output json` carries an `artifacts`
array. The existing cleanup / retention sweep covers the artifact directory;
the Helm `/data` volume is the artifact root.

### 4.3 `Deliverable` and `Citation`

A generator's final output is one `FlowValue::Json` conforming to:

```json
{
  "kind": "report | mindmap | audio_overview | slides | flashcards | quiz | infographic | data_table | video_overview | figure",
  "title": "string",
  "content": { "...kind-specific structured content..." },
  "citations": [
    { "source_id": "uuid", "chunk_id": "string", "span": { "start": 0, "end": 0 }, "quote": "string" }
  ],
  "files": [
    { "node_id": "string", "name": "string", "mime_type": "string", "role": "primary | preview | asset" }
  ]
}
```

- `content` is kind-specific but always structured (sections for a report,
  a tree for a mind map, a speaker-tagged script for an audio overview,
  slides with optional figure source for a deck). Rendering reads
  `content`; nothing reads the rendered file back.
- `citations` is the **unified citation structure**. `source_id` and
  `chunk_id` refer to `sources` / `source_chunks` rows (§6); `span` is the
  character range inside the chunk when known; `quote` is the cited text so a
  deliverable stays verifiable after the index is gone.
- `files` lists the artifacts produced for this deliverable, by `node_id` +
  `name`, resolved against the `artifacts` table after registration.

The Rust type lives in `yanshi-config` (`content::Deliverable`,
`content::Citation`) so both the CLI and the server validate against the same
struct; templates produce it via a final `template` node. It is a JSON
contract, not a kernel type — the kernel's `FlowValue` is unchanged.

Relationship to `yanshi_agents::citation::Citation { marker, source, content }`
(`yanshi-agents/src/citation.rs:38`): that type is the agent loop's *check*
over `[n]` markers in tool results. It stays as is. The two existing
checkers (`KeywordOverlapCitationChecker`, `LlmCitationChecker`) can verify a
`Deliverable` by resolving `chunk_id` → chunk content; Y5.3 wires that into
the report template's test.

## 5. Decision: renderers

### 5.1 Embedded versus external

| Embed in-process when | Run as an external binary when |
|---|---|
| a pure-Rust crate exists with no native build dependency, output is deterministic, and the input is small structured data | the tool is a mature external program (ffmpeg, pandoc, TeX, typst, mermaid, d2), or the input is LLM-authored source code |
| `render_table` (`csv`, `rust_xlsxwriter`), `render_svg` (`resvg`), `render_markdown` (`pulldown-cmark`, self-contained CSS) | `render_document`, `figure`, the `media-tools` MCP server |

External binaries run through the already-built security tiers: the sandboxed
`shell` tool with a per-renderer command allowlist and paths confined to the
run dir, an MCP server (ffmpeg), or the `code_exec` container (Python). This
is where the project's sandbox investment pays off for this product: every
renderer input is LLM-authored or user-supplied, so every renderer is a
boundary.

`yanshi doctor` reports each binary separately so a missing one degrades a
single output format, never a whole generator.

### 5.2 Documents: pandoc, PDF through typst

`render_document` converts Markdown to DOCX / PPTX / HTML / EPUB with pandoc
directly and to PDF with `pandoc --pdf-engine=typst`. Theming is a pandoc
`--reference-doc`. No LibreOffice. Verified 2026-11-04 with pandoc 3.13: a
Markdown deck with an SVG figure and a two-column slide produces a PPTX whose
`ppt/media/image1.svg` is the original vector file, so figures do not lose
fidelity in the slide container. Older PowerPoint builds do not render SVG;
the node writes a PNG sibling for those.

### 5.3 Figures: fidelity comes from the figure tier, not the slide writer

Engineering and scientific drawings are produced by one `figure` node whose
`language` parameter selects a backend. Every backend emits SVG (PNG copy
optional). Validation *is* rendering: a compile error is fed back to the
LLM with the source, up to three attempts, reusing the failure-driven
re-planning from the L track.

| `language` | Toolchain | Covers | Verified 2026-11-04 |
|---|---|---|---|
| `tikz` | TeX Live: `standalone` class → `lualatex` → PDF → `pdftocairo -svg` | circuitikz (electrical), chemfig (chemistry), pgfplots (plots), tikz-cd, tikz-3dplot, and every package TeX Live ships | circuitikz RC loop → 6.9 KB SVG |
| `typst` | `typst compile` with cetz | vector drawings, faster compiles, cleaner error text | cetz canvas compiles (package download on first use) |
| `mermaid`, `d2` | `mmdc`, `d2` | software and structure diagrams | — |
| `python` (Y3.5) | `code_exec` container image with matplotlib, RDKit, dna_features_viewer, ete3, networkx | data-driven science figures: molecules, plasmid and gene maps, phylogenetic trees | — |

Domains (Y6.5) add a second check before the compile. With `domain:
circuit` (tikz only), the prompt carries circuitikz conventions and a
skeleton. A `spec` parts list (`[{id, kind, value}]`) is also checked
against the source: every part must be drawn as a bipole of its kind and
labelled with its id. A missing or wrongly drawn part is fed back like a
compile error. `examples/generators/circuit_diagram.yml` is the sample
domain template. Other domains (chemfig molecules, block diagrams) follow
the same shape when there is a generator that needs them.

Sandbox notes that follow from the experiments: TeX Live installs outside
`PATH` on macOS (`/Library/TeX/texbin`), so the allowlist names full paths
and `TEXMFVAR` is pointed inside the run dir; typst packages are fetched
from the network on first use, so a no-network sandbox needs a pre-warmed
package cache; `code_exec` has no network at all, so Python dependencies are
baked into the image.

### 5.4 Slides

`slides` is Markdown → pandoc → PPTX with a themed reference doc and figures
from §5.3 embedded as SVG. A print-quality PDF variant goes through beamer or
typst-touying with the same figures. A programmatic PPTX writer
(python-pptx / pptxgenjs) is **not** built; see §8.

### 5.5 Media

Audio and video assembly (concat, loudness normalisation, mux, subtitle
burn-in, image slideshow) is an MCP server wrapping ffmpeg
(`examples/ecosystem/mcp/media-tools/`), with every path confined to the run
dir. ffmpeg is never linked in.

## 6. Decision: notebook entity and index persistence

Migration `0012_notebooks.sql`:

| Table | Columns (beyond `id`, `tenant_id`, timestamps) |
|---|---|
| `notebooks` | `title` |
| `sources` | `notebook_id`, `kind` (`url` / `file` / `text` / `recording`), `status`, `sha256`, `origin` (URL or original filename), `error` |
| `source_chunks` | `source_id`, `notebook_id`, `chunk_id`, `ordinal`, `content`, `metadata JSONB` |
| `notebook_artifacts` | `notebook_id`, `artifact_id`, `kind`, `deliverable JSONB` |

**Index persistence: persist the chunks, not the index.** `source_chunks`
must exist anyway — a citation's `chunk_id` has to resolve to text after the
run — and a BM25 index is a pure function of the chunks. Each notebook's
`Bm25KnowledgeBackend` is rebuilt from `source_chunks` via
`from_chunked_documents` (`yanshi-rag/src/knowledge.rs:111`) on first query,
cached in-process, and invalidated when a source is added or removed. There
is no serialised index format and no Qdrant requirement.
`VectorStoreKnowledgeBackend` with one collection per notebook remains an
opt-in; its re-evaluate condition is in §8.

Ingestion is a run, not a bespoke job: `POST /v1/notebooks/{id}/sources`
submits an ingestion workflow (fetch with the gateway's SSRF filter → loader →
chunk → insert chunks; `recording` sources pass through `asr` first) so
progress and failures arrive on the same SSE stream as every other run.

Generation against a notebook: `POST /v1/notebooks/{id}/generate`
`{ kind, params }` selects the template, runs it with the notebook's
`KnowledgeBackend` injected into the `rag` node, applies Y2.3's
`cost_limit_usd`, registers artifacts (§4.2) and links them through
`notebook_artifacts`. `GET /v1/notebooks/{id}/artifacts` lists them with
their `Deliverable`.

## 7. Cost, resume and governance (summary; detail in `TODOs.md` Y2 / Y7)

- Every paid call reports `cost_usd` in its completion event: token-priced
  chat (after COR-1 / COR-2 are fixed so streaming `usage` arrives and is
  accumulated), per-image, per-second (TTS output, video) and per-thousand
  characters (TTS input) prices in `PricingTable`. A run-level
  `cost_limit_usd` (YAML top-level and request field) stops the run with
  `WorkflowEvent::CostLimitExceeded`.
- Checkpointing becomes reachable: `workflow run --checkpoint`,
  `workflow resume <run_id>`, `POST /v1/runs/{id}:resume`, keeping the run id
  and artifact directory so a failure at step nine does not re-bill steps one
  to eight.
- Governance reuses the existing tiers rather than adding a model: the
  production profile asks for approval above a spend threshold and before
  any tool declaring the new `ToolEffect::Publish`; the four cheap gateway
  escapes from the evaluation (template `get_env`, markmap path, rag URL,
  http approval digest) are closed in Y7.3.

## 8. Not doing, and when to re-evaluate

| Not doing | Re-evaluate when |
|---|---|
| LLM-planned graph structure as the default generation path | a catalogue request cannot be expressed as a template with typed inputs — three such requests logged |
| Qdrant / vector index per notebook | a notebook's BM25 rebuild measures > 500 ms, or keyword recall fails the Y5.3 citation tests |
| A serialised BM25 index format | same as above; chunks are the source of truth |
| Programmatic PPTX writer (python-pptx / pptxgenjs) | a generator needs a layout pandoc cannot express: native charts, free-positioned callouts |
| LibreOffice | a user needs DOC / XLS legacy formats pandoc does not write |
| In-process ffmpeg / image / document libraries | never for media; embedded renderers stay pure-Rust (§5.1) |
| Distributed workers for generation | a deployment needs more than one execution host |
| Streaming artifact bytes over SSE | a UI needs progressive media; today the download route plus run events suffice |
| A product UI in this repo | out of scope by standing constraint; `yanshi-ui` stays a run console |

## 9. Decision log

| Date | Question | Decision | Basis |
|---|---|---|---|
| 2026-11-04 | Document renderer | pandoc; PDF via `--pdf-engine=typst`; no LibreOffice | typst is a single binary; pandoc writes DOCX / PPTX / HTML / EPUB without it |
| 2026-11-04 | Slide path for complex figures | figure tier renders SVG; pandoc embeds it as vector | experiment: `ppt/media/image1.svg` preserved by pandoc 3.13 |
| 2026-11-04 | Figure backends | TikZ (TeX Live), typst + cetz, mermaid, d2; Python via `code_exec` second | experiments: circuitikz → SVG via `lualatex` + `pdftocairo`; cetz compiles |
| 2026-11-04 | Index persistence | persist chunks, rebuild BM25 per notebook, cache in-process | chunks are required for citations regardless; "good enough" over a second storage service |

## 10. Compatibility

- Node `type:` names, `FlowValue`, `AsyncNode`, the dependency laws and the
  `check-arch` gates are unchanged.
- Modality nodes default to `output_format: file`; `data_uri` is kept as an
  explicit opt-in so existing examples pass unchanged after setting it.
- New tables are additive (`0012`); existing `artifacts` rows remain valid
  with `size_bytes` / `sha256` null.
- New routes are additive under `/v1/runs/{id}/artifacts` and
  `/v1/notebooks`; existing routes keep their shapes.
