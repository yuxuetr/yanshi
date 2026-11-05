# Yanshi Configuration

Last updated: 2026-05-09

This document covers the configuration that is currently implemented by the CLI:
model/provider configuration, secrets, workflow YAML, run directories, and the
main validation commands.

## Model Configuration

Yanshi resolves model configuration with this priority:

1. `YANSHI_MODELS_CONFIG`
2. `~/.yanshi/models.yml`
3. `~/.yanshi/models.yaml`
4. bundled `default_models.yml` when no user config exists

`models.yml` is the canonical filename. `models.yaml` is supported as a
legacy fallback. If both files exist, Yanshi uses `models.yml` and prints a
warning.

Initialize local configuration with:

```bash
yanshi config init
```

This creates:

```text
~/.yanshi/models.yml
~/.yanshi/.env
```

Inspect and validate the active configuration with:

```bash
yanshi config show
yanshi config show models
yanshi config show providers
yanshi config validate
yanshi doctor
yanshi doctor --format json
yanshi llm models
yanshi llm models --provider openai --detailed
```

`config show`, `config validate`, `doctor`, and `llm models` all report or use
the same resolved model configuration source.

`yanshi llm` is limited to model discovery and diagnostics. Interactive model
use should go through `yanshi skill run`, `yanshi skill chat`, or
`yanshi workflow run`.

## Secrets

Do not store raw API keys in workflow YAML or `models.yml`. Store secrets in the
shell environment or in `~/.yanshi/.env`, and let model/provider entries refer
to the environment variable name.

Common variables:

```bash
OPENAI_API_KEY=...
ANTHROPIC_API_KEY=...
GEMINI_API_KEY=...
MOONSHOT_API_KEY=...
DASHSCOPE_API_KEY=...
STEPFUN_API_KEY=...
```

Recommended local permissions:

```bash
chmod 700 ~/.yanshi
chmod 600 ~/.yanshi/.env ~/.yanshi/models.yml
```

See [SECRET_MANAGEMENT.md](SECRET_MANAGEMENT.md) for the broader policy.

## Runtime Model Selection

Model selection precedence is:

1. CLI `--model` for the current command.
2. Node-level `parameters.model` in workflow YAML or `[model].name` in a Skill.
3. Built-in runtime default.

Supported overrides include:

```bash
yanshi workflow run flow.yml --model gpt-4o-mini
yanshi skill run ./skills/code-reviewer --message "review this" --model gpt-4o-mini
yanshi skill chat ./skills/code-reviewer --model gpt-4o-mini
```

## Workflow YAML

The current config-first workflow format is `FlowDefinitionV2`:

```yaml
name: "Example Workflow"
inputs:
  topic:
    description: "Topic to pass into the workflow"
    required: false
    default: "Yanshi"
nodes:
  - id: render_prompt
    type: template
    parameters:
      template: "Write a short summary about {{topic}}."

  - id: summarize
    type: llm
    dependencies: ["render_prompt"]
    input_mapping:
      prompt: "{{ nodes.render_prompt.outputs.output }}"
    parameters:
      model: gpt-4o-mini
      temperature: 0.2
      max_tokens: 256
```

Top-level fields:

| Field | Required | Description |
| --- | --- | --- |
| `name` | Yes | Workflow name used in CLI output and validation reports. |
| `inputs` | No | Named workflow inputs with `description`, `required` (defaults to `false` if omitted), and `default`. Enforced (T3.2) before a run starts: a missing `required` input with no `default` fails the run with a clear error naming it; a missing input with a `default` gets that value filled into the initial input pool. A value supplied via `--input` always wins over a declared `default`. `description` is documentation only. |
| `nodes` | Yes | Ordered list of workflow node definitions. |

Node fields:

| Field | Required | Description |
| --- | --- | --- |
| `id` | Yes | Unique node id. |
| `type` | Yes | Node type supported by the CLI factory. |
| `dependencies` | No | Node ids that must complete before this node runs. |
| `input_mapping` | No | Runtime mappings from previous node outputs. |
| `run_if` | No | Conditional expression evaluated by the workflow runtime. |
| `timeout_ms` | No | Outer execution timeout in milliseconds, applied uniformly to any node type (T3.1). Not supported on `map`/`while` nodes (rejected at validation). Distinct from the `mcp` node's own `parameters.timeout_ms`, which only governs its MCP-client connection. |
| `max_retries` | No | Retry count on transient (network/timeout/rate-limit-class) failures, applied uniformly to any node type (T3.1). Non-transient failures (validation errors, 4xx-style application errors) are not retried. Not supported on `map`/`while` nodes. |
| `parameters` | No | Node-specific parameter map. |

Supported mapping expressions currently use this form:

```yaml
input_mapping:
  prompt: "{{ nodes.render_prompt.outputs.output }}"
```

A node receives three kinds of input under one name space. When names
collide, a value from `input_mapping` wins. Next come the workflow inputs
(and, inside `map` / `while`, the loop variables such as `item`), which
every node receives under their own names. The node's own `parameters`
come last. A workflow input therefore never replaces a mapped value, but it
does override a `parameters` entry of the same name.

`timeout_ms` / `max_retries` example — the two node types most prone to
transient failures in practice:

```yaml
  - id: fetch
    type: http
    timeout_ms: 5000
    max_retries: 2
    parameters:
      url: "https://api.example.com/data"

  - id: summarize
    type: llm
    timeout_ms: 10000
    max_retries: 3
    parameters:
      model: gpt-4o-mini
```

See [WORKFLOW_SCHEMA.md](WORKFLOW_SCHEMA.md) for the supported node types and
their required/optional parameters.

## Workflow Commands

Run a workflow:

```bash
yanshi workflow run flow.yml
yanshi workflow run flow.yml --dry-run
yanshi workflow run flow.yml --model gpt-4o-mini
yanshi workflow run flow.yml --execution-mode concurrent --max-concurrency 4
yanshi workflow run flow.yml --input topic Yanshi
```

Validate without execution:

```bash
yanshi workflow validate flow.yml
yanshi workflow validate flow.yml --format json
yanshi workflow validate flow.yml --strict
```

Debug workflow structure:

```bash
yanshi workflow debug flow.yml --validate
yanshi workflow debug flow.yml --visualize
yanshi workflow debug flow.yml --analyze
yanshi workflow debug flow.yml --plan
yanshi workflow debug flow.yml --dry-run --verbose
```

`workflow run` and `workflow run --dry-run` both execute schema validation before
building the graph.

## Run And Trace Directories

Workflow run artifacts default to:

```text
~/.yanshi/runs
```

Override the base directory with:

```bash
yanshi workflow run flow.yml --run-dir /var/lib/yanshi/runs
YANSHI_RUN_DIR=/tmp/yanshi-runs yanshi workflow run flow.yml
```

Media nodes (`tts`, `text_to_image`, `text_to_video`, ...) write their
output under `<run_dir>/<run_id>/artifacts/<node_id>/`, and a run ends
with a table of those files. To collect them somewhere stable:

```bash
yanshi workflow run flow.yml --output-dir ./out
# ./out/<node_id>/<file>   copies of every artifact
# ./out/artifacts.json     run_id + node_id / output_key / mime_type /
#                          size_bytes / sha256 / path per artifact
```

Only `File` outputs under the run base directory count as artifacts.

A long run with billed steps can be made resumable:

```bash
yanshi workflow run flow.yml --checkpoint --input topic=tides
# fails at node 9 of 10; prints: yanshi workflow resume <run_id> flow.yml
yanshi workflow resume <run_id> flow.yml
```

`--checkpoint` writes `<run_dir>/<run_id>/checkpoint/` after every node.
`resume` re-runs only the nodes that did not finish, with the original
`--input` values, execution mode, concurrency and `cost_limit_usd`; the
spend already recorded counts toward the limit, so nothing finished is
paid for twice. Pass the same `--run-dir` the run used. Inspect what a
resume would replay with
`yanshi workflow resume-plan <run_id> --checkpoint-dir <run_dir>/<run_id>/checkpoint`.
`--output-dir` is local-only; against `--server`, list and download
through `GET /v1/runs/{id}/artifacts`.

Trace files default to:

```text
~/.yanshi/traces
```

Inspect persisted traces with:

```bash
yanshi trace replay <run_id>
yanshi trace tui <run_id>
```

## Rendering Deliverables

Render nodes turn content into files (`FlowValue::File` artifacts under
the run directory). All read their inputs at execute time, so content
can come from `parameters` or from an upstream node via `input_mapping`.

| node | input → output | needs |
| --- | --- | --- |
| `render_table` | `rows` (+ `format: csv\|xlsx`, `columns`) → CSV / XLSX | — |
| `render_svg` | `svg` (+ `width`) → PNG | — |
| `render_markdown` | `markdown` (+ `title`) → one self-contained HTML page | — |
| `render_document` | `markdown`, `format: docx\|pptx\|html\|epub\|pdf` (+ `title`, `toc`) | `pandoc`; `typst` for PDF |
| `figure` | `language`, `description` + `model` (or `source`) → SVG (+ `png: true`) | per language, below |

`figure` asks the model for source in `language`, renders it, and feeds
render errors back for up to `max_attempts` (default 3); each retry is a
`node.progress` event with `kind: figure_retry`.

| `language` | renderer |
| --- | --- |
| `svg` | in-process (resvg) |
| `tikz` | `lualatex` + `pdftocairo` (TeX Live: circuitikz, chemfig, pgfplots, tikz-cd) |
| `typst` | `typst`, cached packages only (e.g. `@preview/cetz`) |
| `python` | container engine + the image from `scripts/figure_python/Containerfile` (matplotlib, RDKit, dna_features_viewer, biopython, networkx) |

```yaml
- id: circuit
  type: figure
  parameters:
    language: tikz
    model: claude-sonnet
    description: "An RC low-pass filter with labelled R1, C1 and Vin"
    png: true
```

External renderers run under the OS sandbox: read-only access to their
install trees and fonts, writes only to the node's work directory, no
network. `python` runs in the `code_exec` container tier and refuses to
run without a container engine. `yanshi doctor` lists which renderer
binaries are installed. Build the Python image once with
`container build -t yanshi-figure-python:latest scripts/figure_python`
(or `podman build`), or point `YANSHI_FIGURE_PYTHON_IMAGE` at your own.

## Cost Limits

Billed modality nodes (`tts`, `text_to_image`, `image_to_image`,
`image_edit`, `text_to_video`) price each call against the pricing
table: `YANSHI_PRICING_TABLE`, else `~/.yanshi/pricing.yml`, else empty
(every model costs $0). No prices are bundled; wrong bundled prices
would enforce the wrong limit.

```yaml
# ~/.yanshi/pricing.yml
models:
  gpt-image-1:
    per_image: 0.04            # each generated / edited image
  step-tts-mini:
    per_1k_input_chars: 0.015  # TTS input text
    per_output_second: 0.0     # seconds of audio (derived for WAV only)
  veo-3:
    per_output_second: 0.50    # seconds of video
  gpt-4o:                      # token prices (agents / eval)
    input_per_1k: 0.005
    output_per_1k: 0.015
```

Cap a run's spend with a top-level `cost_limit_usd`:

```yaml
name: illustrated report
cost_limit_usd: 2.00
nodes: [...]
```

Once the run's recorded spend has passed the limit, nodes that have not
started are refused with `CostLimitExceeded`. The node that crossed the
limit, and any already running concurrently, finish. Events:
`node.cost` (per billed node, with the run total) and one
`cost.limit_exceeded`. `POST /v1/runs` also accepts `cost_limit_usd`;
when both are set the tighter applies. `yanshi doctor` shows which
pricing table is in effect, and warns when it does not parse.

## Skills

Skills use `SKILL.md` as the recommended entry point, with `skill.toml` still
supported for explicit structured overrides. Skill commands include:

```bash
yanshi skill init ./my-skill --description "Describe this skill"
yanshi skill validate ./my-skill
yanshi skill inspect ./my-skill
yanshi skill list-tools ./my-skill
yanshi skill run ./my-skill --message "hello"
yanshi skill chat ./my-skill
yanshi skill test ./my-skill --dry-run
```

See [SKILLS.md](SKILLS.md), [SKILL_FORMAT.md](SKILL_FORMAT.md), and
[SKILL_REGISTRY.md](SKILL_REGISTRY.md).
