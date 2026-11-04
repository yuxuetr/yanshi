# Yanshi Docs

Last updated: 2026-05-09

This directory contains current user and maintainer documentation. Historical
phase reports, old TODO trackers, implementation summaries, and stale backups
have been removed from the active docs tree so this directory stays focused on
working references.

The curated manual built from this directory is published at
<https://yuxuetr.github.io/yanshi/> (table of contents:
[SUMMARY.md](SUMMARY.md); build locally with `mdbook build site`).

## Start Here

- [CURRENT_STATUS.md](CURRENT_STATUS.md): current authoritative project status,
  implemented surfaces, stability links, and active work.
- [RFC_CONTENT_STUDIO.md](RFC_CONTENT_STUDIO.md): the product direction —
  a multimodal content-production runtime — and the contracts the Y track
  implements.
- [TRIAL_GUIDE.md](TRIAL_GUIDE.md): 试用手册 — which CLI paths are green today.
- [ARCHITECTURE.md](ARCHITECTURE.md): current workspace layout and runtime model.
- [CONFIGURATION.md](CONFIGURATION.md): CLI config, secrets, workflow YAML, and run directories.
- [WORKFLOW_SCHEMA.md](WORKFLOW_SCHEMA.md): implemented config-first workflow validation contract.
- [AGENT_RUNTIME.md](AGENT_RUNTIME.md): agent runtime behavior and event model.
- [SKILLS.md](SKILLS.md): Skill authoring, tools, MCP integration, memory, and CLI commands.
- [MCP_SKILLS.md](MCP_SKILLS.md): MCP tool usage from Skills.
- [TRACING_USAGE.md](TRACING_USAGE.md): trace capture, replay, and inspection.
- [CHECKPOINT_RECOVERY.md](CHECKPOINT_RECOVERY.md): checkpoint and resume behavior.
- [PLUGIN_DESIGN.md](PLUGIN_DESIGN.md): subprocess plugin runtime, manifest,
  workflow node integration, sandbox bridge, and CLI.
- [MARKETPLACE.md](MARKETPLACE.md): remote Skill/Plugin catalog schema,
  artifact cache, signature verification, and marketplace CLI.
- [WEB_UI.md](WEB_UI.md): embedded server UI for run lists, graph status,
  event history, and SSE-backed debugging.

## Topic Guides

- Workflow execution: [WORKFLOW_DEBUGGING.md](WORKFLOW_DEBUGGING.md), [HYBRID_WORKFLOW.md](HYBRID_WORKFLOW.md), [TIMEOUT_CONTROL.md](TIMEOUT_CONTROL.md), [RETRY_MECHANISM.md](RETRY_MECHANISM.md) (resource management doc archived, W5.3 — see [archive/RESOURCE_MANAGEMENT_2026-08-12-pre-w5.3.md](archive/RESOURCE_MANAGEMENT_2026-08-12-pre-w5.3.md))
- Production operations: [DEPLOYMENT.md](DEPLOYMENT.md), [DISTRIBUTED.md](DISTRIBUTED.md), [WEB_UI.md](WEB_UI.md), [KUBERNETES_DEPLOYMENT.md](KUBERNETES_DEPLOYMENT.md), [HEALTH_CHECKS.md](HEALTH_CHECKS.md), [SECRET_MANAGEMENT.md](SECRET_MANAGEMENT.md), [TOOL_PERMISSIONS.md](TOOL_PERMISSIONS.md), [SKILL_PERMISSIONS.md](SKILL_PERMISSIONS.md)
- Skills and ecosystem: [SKILL_FORMAT.md](SKILL_FORMAT.md), [SKILL_REGISTRY.md](SKILL_REGISTRY.md), [MARKETPLACE.md](MARKETPLACE.md), [PLUGIN_DESIGN.md](PLUGIN_DESIGN.md), [MCP_SKILLS_INTEGRATION.md](MCP_SKILLS_INTEGRATION.md), [MCP_PRODUCTION_DESIGN.md](MCP_PRODUCTION_DESIGN.md)
- LLM and multimodal: [LLM_PROVIDERS_MATRIX.md](LLM_PROVIDERS_MATRIX.md), [GRANULAR_MODEL_TYPES.md](GRANULAR_MODEL_TYPES.md), [MULTIMODAL_GUIDE.md](MULTIMODAL_GUIDE.md)
- Tracing internals: [TRACING_DESIGN.md](TRACING_DESIGN.md), [TRACE_PERSISTENCE_SCHEMA.md](TRACE_PERSISTENCE_SCHEMA.md)
- Examples: [examples/README.md](examples/README.md)

## Project Status And Roadmap

Live at the repository root:

- [`RoadMap.md`](../RoadMap.md): the Content Studio direction (track Y) and
  the deferred tracks with their re-evaluate conditions.
- `TODOs.md` (maintainer-local, gitignored): active task queue derived from the evaluation and roadmap.

Current evaluations in `docs/`:

- [`PROJECT_EVALUATION_2026-11-04.md`](PROJECT_EVALUATION_2026-11-04.md):
  **most recent evaluation** — 10-07 closure accounting, new findings,
  re-weighted for the content-production goal; drove track Y.
- [`PROJECT_EVALUATION_2026-10-07.md`](PROJECT_EVALUATION_2026-10-07.md): drove segment X.
- [`PROJECT_EVALUATION_2026-08-09.md`](PROJECT_EVALUATION_2026-08-09.md): drove segment W.
- [`PROJECT_EVALUATION_2026-08-05.md`](PROJECT_EVALUATION_2026-08-05.md): drove segment V.
- [`ARCHITECTURE_EVALUATION_2026-06-20.md`](ARCHITECTURE_EVALUATION_2026-06-20.md):
  dependency-graph validation of the crate-architecture RFC.

Archived under `docs/archive/`:

- [`PROJECT_EVALUATION_2026-05-19.md`](archive/PROJECT_EVALUATION_2026-05-19.md):
  evaluation (HEAD `daaa912`, A overall) — v1.0.0-rc.1 candidate;
  all N1–N10 segments closed.
- [`PROJECT_EVALUATION_2026-05-14.md`](archive/PROJECT_EVALUATION_2026-05-14.md):
  follow-up evaluation that drove the P6/P7/P-H/M segment additions.
- [`PROJECT_EVALUATION_2026-05-01.md`](archive/PROJECT_EVALUATION_2026-05-01.md):
  historical module-by-module evaluation (HEAD `41ed3f8`, B+ overall).
- [`TODOs-archive-2026-05-20-closed-segments.md`](archive/TODOs-archive-2026-05-20-closed-segments.md):
  **most recent** archive — 12 fully-closed P-segments (P0–P9 + P-H +
  P-LLM + M) moved out of the active file on 2026-05-20 once the
  2026-05-19 evaluation confirmed they were all DONE.
- [`TODOs-archive-2026-05-19-recently-closed.md`](archive/TODOs-archive-2026-05-19-recently-closed.md):
  Recently-Closed entries swept out of the active `TODOs.md` on 2026-05-19.
- [`TODOs-archive-2026-05-09-n1-n10.md`](archive/TODOs-archive-2026-05-09-n1-n10.md)
  and [`TODOs-archive-2026-05-10-p0-p4.md`](archive/TODOs-archive-2026-05-10-p0-p4.md):
  N-series + early P-series execution-plan history.

## Historical Release References

These remain as release/migration references, not as current design documents:

- [RELEASE_NOTES_v0.2.0.md](RELEASE_NOTES_v0.2.0.md)
- [MIGRATION_GUIDE_v0.2.0.md](MIGRATION_GUIDE_v0.2.0.md)

## Maintenance Rule

Keep active docs tied to implemented behavior. Put time-boxed plans, phase
completion notes, audit reports, and TODO trackers outside `docs/` or delete them
after their decisions have been folded into stable guides.
