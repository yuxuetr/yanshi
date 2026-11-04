# Yanshi Application Examples

This directory holds **dogfooding-driven, real-business application
examples** built on top of Yanshi. Each subdirectory is a complete,
runnable application — not a feature demo. The tracking + status
overview lives at the repo root in [`EXAMPLES_TODOs.md`](../../EXAMPLES_TODOs.md).

## How this relates to the rest of `examples/`

| Tree | Purpose | Audience |
| --- | --- | --- |
| [`examples/README.md`](../README.md) | SDK feature matrix — every public capability has a minimal demo | SDK learners, maintainers |
| [`examples/ecosystem/`](../ecosystem/) | Generic `SKILL.md` / `plugin.toml` / marketplace shape samples | Skill / plugin authors |
| **`examples/applications/`** *(this dir)* | **End-to-end product-shaped applications** | Dogfooding, prospective users evaluating "can Yanshi build my thing?" |

The three trees do not overlap. An application here exercises multiple
SDK capabilities through a real business workflow; an SDK demo proves
a single capability works.

## Current applications

See [`EXAMPLES_TODOs.md`](../../EXAMPLES_TODOs.md) for the full
status table.

| # | Application | Status | Subdirectory |
| --- | --- | --- | --- |
| A1 | blog → two-speaker podcast | WIP — live smoke ✅ (2026-05-18) | [`blog-to-podcast/`](blog-to-podcast/) |
| A1.5 | podcast mastering via a skill driving an MCP subprocess | WIP — live smoke ✅ (2026-05-18) | [`podcast-mastering/`](podcast-mastering/) |
| A2 | GitHub PR code reviewer | WIP — live ✅ as a skill (2026-05-18); [`code-reviewer-write/`](code-reviewer-write/) is the reference binary | [`code-reviewer/`](code-reviewer/) |
| A3 | Arxiv research assistant | WIP — live ✅ iter 1 (2026-05-18) | [`research-assistant/`](research-assistant/) |
| A4 | Meeting recording → transcript + action items | TODO | [`meeting-transcriber/`](meeting-transcriber/) |
| A5 | Scheduled weekly digest email | TODO | [`weekly-digest/`](weekly-digest/) |
| A6 | Markdown folder multi-language translator | WIP — four workflow iterations shipped | [`doc-translator/`](doc-translator/) |
| A7 | Git log → conventional CHANGELOG (yanshi eats its own dogfood) | WIP — live ✅ as a binary; skill form rejected (2026-05-18) | [`changelog-writer/`](changelog-writer/) |
| A8 | **Content Studio** — one URL or idea → report + mind map + audio overview via a planner agent (track Y, `TODOs.md` Y4.4) | TODO | `content-studio/` (not created yet) |

## Conventions for new applications

When adding a new application:

1. **Pick a real problem you (or a target user) actually have.** This
   tree is not for synthetic demos — those go to `examples/ecosystem/`.
2. **One subdirectory per application** with at minimum a `README.md`
   covering business description, architecture sketch, external
   dependencies, required API keys.
3. **At least one of `workflow.yml`, `skill.toml`, or `src/main.rs`**
   shipped alongside the README so the application is actually runnable.
4. **Smoke test that self-skips without live credentials** so CI can
   compile-check without failing on missing keys.
5. **`Findings` section in the application's `EXAMPLES_TODOs.md` entry**
   for anything that surprised you during dogfooding — too-clunky API,
   missing node type, confusing error message. These get harvested
   periodically into the main `TODOs.md` queue.

## Status flags

- `TODO` — directory + README exist; implementation not started
- `WIP`  — implementation in progress; at least one commit
- `DONE` — runnable end-to-end + smoke test exists + README complete
- `DEFERRED` — explicitly paused (always with a reason)

Promote an application to `DONE` only after you have actually used it
yourself for its intended business purpose, not just after the smoke
test passes.
