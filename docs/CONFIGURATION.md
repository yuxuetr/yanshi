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
