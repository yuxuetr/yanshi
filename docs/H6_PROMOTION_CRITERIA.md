# Harness Phase H6 — Promotion Criteria

Status: **Decision document for P10.10.1**
Owner: Yanshi core
Last updated: 2026-05-20
Closes: P10.10.1 (Medium — v1.x)

`docs/archive/HARNESS_MODE_EVOLUTION.md` §"Phase H6: Advanced Compatibility"
deliberately leaves five items open-ended — they're things we *might*
do "if user demand appears" but explicitly should not pull en bloc
into a roadmap commit. P10.10.1 captures that posture in the active
TODO list. This document is the focused follow-up: it pins, per item,
**what concrete demand signal would tip the scale**, so when (or if)
a real user request arrives, the next person doesn't re-derive the
analysis from scratch.

The recommendation up front: **no H6 item should be promoted before
v1.0 GA.** Two of the five (TUI clone, provider subscription bridge)
are documented as *explicit non-goals* in `RoadMap.md`; promoting
those at all needs an RFC that re-opens that decision. The remaining
three are reactive — wait for the demand signal, then write the
per-item RFC.

This is the same posture as P10.19.1 (WASM plugin runtime): pin the
trigger, persist the analysis, let future-me act on demand rather
than speculation.

---

## The five H6 items

The TODO list and `docs/archive/HARNESS_MODE_EVOLUTION.md` use slightly different
names; this table reconciles them.

| Item | TODO name (P10.10.1) | Evolution-doc name (§H6) |
| --- | --- | --- |
| 1 | Slash-command ecosystem expansion | richer slash-command model |
| 2 | TUI product shell (separate from CLI run) | TUI |
| 3 | OpenHarness-style config import | OpenHarness-style config import |
| 4 | Plugin compatibility adapters | plugin compatibility adapters |
| 5 | Provider subscription bridge | provider profile migration helpers |

---

## Item 1 — Slash-command ecosystem expansion

**What it would mean.** Today `yanshi harness` runs sessions by
flag (`--user-input`, `--workspace-root`, etc.). A slash-command
ecosystem would let users type `/file path/to/x.rs` or `/recent` or
`/skill some-skill` inside a session prompt and have the runtime
expand that into a structured request (read file, list recent runs,
invoke skill).

**Why we'd want it.** Quality-of-life win for interactive
operators; reduces the number of round-trips between "type a
question" and "remember the right flag." Aligns with how
Claude Code, OpenHarness, and Cursor work.

**Why we haven't done it.** No operator has filed a request,
and the existing flag surface is enough for the dogfooding loops
that drove H1-H4. Slash-command parsing is an additive layer in
the CLI; the runtime contract doesn't need to change.

**Concrete demand signal** (any one tips the scale):

- A first-party user requests `/file` or `/skill` in the
  interactive prompt at least twice in a two-week window.
- An external contributor opens a PR adding a single `/cmd` and
  asks for the wider model.
- The `yanshi harness chat` UX gets blocked on
  "the prompt template can't reference X" enough times to merit
  a structured fix.

**Scope of the RFC.** ~1 page. Decide:

1. Tokenizer: a leading `/` prefix on a line, parsed into
   `Command + args`. Whitespace handling.
2. Registry shape: a `HashMap<&str, Box<dyn SlashHandler>>`
   in `yanshi-harness` (additive, no API break).
3. First three commands to ship (recommendation: `/file`,
   `/skill`, `/recent`).
4. Whether built-in commands can be disabled by config.
5. Plumbing through the existing `ContextProvider` trait or as
   a new pipeline stage.

**Estimated scope.** ~1-2 person-weeks for parser + 3 commands +
docs + tests.

---

## Item 2 — TUI product shell

**What it would mean.** A separate `yanshi-tui` (or
`yanshi tui`) surface — a full-screen `ratatui` app that runs
Harness sessions interactively with panes for the live event
stream, the workspace tree, the approval queue, etc.

**Status: explicit non-goal in `RoadMap.md`.** Quote:
> Clone of OpenHarness TUI or provider subscription bridges.
> UI-first product shell that freezes the protocol before
> stream-JSON envelopes stabilize.

**Why we haven't done it.** The Web UI (`yanshi-ui`,
debugger-focused per P10.17.1) covers the visual debugger
use case; the CLI + JSONL persistence covers the headless
case; and the Harness event envelope is still Beta (per
`docs/STABILITY.md`). Building a TUI right now would freeze
the consumer side of an envelope that's still moving.

**Concrete demand signal** (must include both):

- The Harness envelope graduates to Stable (today: Beta, see
  `docs/STABILITY.md`). Without that, a TUI freezes its
  rendering against a shape we still have flexibility to
  change additively.
- Three operators independently ask for "the Web UI but
  inside SSH" — i.e. the headless-environment case
  (`yanshi harness replay` per P10.10.2 already covers
  most of this without a full TUI).

**Scope of the RFC.** ~2 pages. Decide:

1. Where it lives (`yanshi-tui` crate vs. `yanshi tui`
   subcommand vs. `yanshi harness watch` in the CLI).
2. Wire-shape contract: must consume `HarnessEvent` over SSE
   or JSONL identically to the Web UI (per the
   "UI is a client of the protocol" invariant in
   `docs/HARNESS_MODE.md` §"Architectural invariants").
3. Pane layout — pick a fixed v1 design (resist configurability
   on day one).
4. Keybindings spec.
5. **Justify** against the existing
   `yanshi harness replay --speed 2x` (P10.10.2)
   non-interactive replay, which already covers most of the
   "watch a long-running session unfold" use case.

**Estimated scope.** ~4-6 person-weeks. The non-goal status
in `RoadMap.md` means promotion requires a deliberate
re-opening of that decision in the RFC.

---

## Item 3 — OpenHarness-style config import

**What it would mean.** A tool that reads OpenHarness'
configuration files (its YAML / TOML format) and emits an
equivalent `yanshi harness` config so operators migrating
*from* OpenHarness don't rewrite by hand.

**Why we'd want it.** Lowers the bar for an OpenHarness
operator evaluating Yanshi.

**Why we haven't done it.** Zero migration requests from
OpenHarness users to date. OpenHarness' config is a moving
target (it's pre-1.0 itself), so an importer written today
would chase its schema changes. And the surface overlap
between OpenHarness config and Yanshi's Skill / Harness
contract is partial — a 1:1 import is impossible without
loss.

**Concrete demand signal** (any one):

- An OpenHarness user opens an issue asking how to bring
  their config across.
- We talk to a partner at a vendor / customer evaluating
  both projects.
- OpenHarness ships a 1.0 with a frozen config schema, making
  the import target stable.

**Scope of the RFC.** ~1 page. Decide:

1. Source format snapshot — pin a specific OpenHarness
   version as the import target.
2. Coverage: which OpenHarness concepts map to which
   Yanshi concepts; which don't map (document the gaps
   explicitly).
3. Output: emit a `skill.toml` + a `harness run` flag set, or
   a self-contained skill that wraps the imported config?
4. Tool surface: standalone binary, `yanshi harness import`
   subcommand, or one-off Python script?

**Estimated scope.** ~1-2 person-weeks once a stable
OpenHarness schema exists.

---

## Item 4 — Plugin compatibility adapters

**What it would mean.** A shim layer so OpenHarness plugins
can run inside Yanshi's subprocess JSON-RPC plugin runtime
without modification.

**Why we'd want it.** Lets us ship the existing OpenHarness
plugin ecosystem (if one materializes) as drop-in extensions.

**Why we haven't done it.** OpenHarness plugin format isn't
1.0; the Yanshi plugin runtime (subprocess JSON-RPC, stable
per `docs/PLUGIN_DESIGN.md` §5) is already polyglot — anyone
can write a plugin in any language. A compatibility adapter
solves a problem nobody has yet, and the WASM-plugin-runtime
decision in `docs/WASM_PLUGIN_EVALUATION.md` already pushed
the heavyweight plugin work to v2.

**Concrete demand signal** (any one):

- A non-trivial OpenHarness plugin (>2 of them, or one used
  by ≥ 2 organizations) gets requested to run in Yanshi.
- OpenHarness plugin format reaches 1.0 with a published
  conformance suite.
- An OpenHarness contributor opens a PR adding the adapter
  themselves.

**Scope of the RFC.** ~1-2 pages. Decide:

1. Which OpenHarness plugin contract is being adapted (config
   format, JSON-RPC method names, lifecycle semantics).
2. Translation table: each OpenHarness host call ↔ each
   Yanshi `yanshi-tools` / `yanshi-mcp` call.
3. Whether the adapter is a separate binary that operators
   point Yanshi at, or an in-process layer in
   `yanshi-tools`.
4. Acceptance criteria: a named OpenHarness plugin runs
   end-to-end through `yanshi workflow run` against an
   Yanshi-managed sandbox.

**Estimated scope.** ~3-4 person-weeks if OpenHarness plugin
format is stable; open-ended otherwise.

---

## Item 5 — Provider subscription bridge

**What it would mean.** Reuse an operator's existing Anthropic /
Claude.ai / OpenAI Plus / Mistral / etc. *subscription* (web
chat) for backend API calls — same provider account, no
separate API key needed. This is the
"OpenHarness lets me sign in once" experience.

**Status: explicit non-goal in `RoadMap.md`.** Quote:
> Clone of OpenHarness TUI or **provider subscription bridges**.

**Why we haven't done it.** Every provider has a different
auth model for their subscription tier (Anthropic's Claude.ai
session cookie ≠ Anthropic's API key; same for OpenAI Plus vs.
OpenAI API). Reverse-engineering session cookies is fragile —
the provider can break it any release. The Yanshi LLM
provider abstraction (`yanshi-llm`) is API-only by design
because that's the contract providers actually maintain.

**Concrete demand signal** (must include both):

- At least one major provider ships a documented "use your
  subscription credits via API" path (Anthropic / OpenAI /
  Google / etc. — this would be a provider-side announcement,
  not something we reverse-engineer).
- ≥ 5 separate operators ask for the bridge.

**Scope of the RFC.** ~2 pages. Decide:

1. Which provider's documented bridge is the v1 target.
2. Auth flow: OAuth device-code? PKCE? Session-cookie pass-
   through? — must use the provider's documented
   subscription-API surface, not scraped cookies.
3. Where the bridge lives: a new `yanshi-llm` provider
   variant per subscription kind, or a wrapper that adapts
   subscription auth into the existing API providers.
4. Threat model: what does the bridge expose to a malicious
   workflow? (Subscription tokens are typically more
   privileged than API keys.)
5. **Justify** against the non-goal stance in `RoadMap.md`.

**Estimated scope.** ~4-6 person-weeks per provider once a
documented subscription API exists. Multiple providers compound
nearly linearly.

---

## What this document does **not** do

- It does not commit to any of the five items.
- It does not approve work on the two items currently listed as
  non-goals (#2 TUI, #5 subscription bridge); promoting those
  requires re-opening that decision.
- It does not write any of the per-item RFCs in advance.

When a demand signal in the matching item fires, open a new
`P11.x` TODO, link this document, and write the per-item RFC
under `docs/RFC_H6_<item-slug>.md`.

---

## Why a 1-pager instead of code

P10.10.1's TODO note is explicit:
> Each requires its own RFC. Don't pull en bloc.

The work P10.10.1 actually represents is *maintaining the
gate* — preserving the per-item promotion discipline so a
future operator with a real request gets a focused RFC instead
of a five-item batch that lumps the TUI in with a slash-command
parser. This document is that gate, made explicit. The TODO can
close because the gate is now documented; the TODO didn't
require shipping any of the five items, only ensuring they
won't drift into the roadmap without a per-item review.

---

## References

- `RoadMap.md` §"Later Tracks" / §"Harness Agent Mode" / non-goals.
- `docs/archive/HARNESS_MODE_EVOLUTION.md` §"Phase H6: Advanced Compatibility".
- `docs/HARNESS_MODE.md` §"Architectural invariants" — the
  "UI is a client of the protocol" invariant that any TUI
  promotion must respect.
- `docs/ROADMAP_v2.md` Theme F (Harness expansion).
- `docs/STABILITY.md` — the Beta tier for `HarnessEvent` that
  any UI/TUI consumer must respect.
- P10.10.2 (closed) — `yanshi harness replay --speed 2x`
  covers the non-interactive watch-a-session-unfold case.
- P10.17.1 (closed) — Web UI debugger-focused positioning
  decision; the TUI promotion would need to justify itself
  against this stance.
- P10.19.1 (closed) — `docs/WASM_PLUGIN_EVALUATION.md` is the
  template this document follows (decide-when-to-revisit, not
  implement-now).
