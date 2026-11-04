# podcast-mastering

**Tracking entry**: [`EXAMPLES_TODOs.md` § A1.5](../../../EXAMPLES_TODOs.md#a15--podcast-mastering)
**Sibling**: post-processes the output of
[blog-to-podcast](../blog-to-podcast/).

## Business

Given a finished podcast recording, make it ready for upload:
loudness-normalise to a target (-16 / -14 LUFS), optionally join an
intro / outro, optionally produce a video version from a cover image.

## How

A ReAct skill drives the in-repo
[media-tools MCP server](../../ecosystem/mcp/media-tools/), which wraps
ffmpeg. The skill spawns it over stdio, and its tools surface as
`mcp_media_*`:

```
yanshi skill run → ReAct loop → mcp_media_loudness_normalize, … → media-tools (python3) → ffmpeg
```

Until Y3.2 (2026-11-05) this example drove `phonon-mcp`, an out-of-tree
Rust binary that had to be built separately. It now needs only
`python3` and `ffmpeg`.

## Run

From the repository root, with the audio under `./out/` (the server's
root; every path a tool touches must stay inside it):

```bash
mkdir -p out && cp /path/to/episode.wav out/
yanshi doctor | grep -A2 "External tools"     # ffmpeg found?
yanshi skill validate examples/applications/podcast-mastering
yanshi skill run examples/applications/podcast-mastering \
  --message "master episode.wav to mastered.wav at -16 LUFS"
```

The default model is `kimi-k2.6` (`MOONSHOT_API_KEY`); change `[model]`
in `skill.toml` for another vendor.

## What it exercises

- `[[mcp_servers]]` spawning a script-interpreter MCP server that is on
  the default `mcp_command_allowlist` (`python3`).
- `McpToolAdapter` exposing a server's tools as `mcp_<server>_<tool>`.
- An agent reporting *measured* loudness (`output_lufs`) rather than
  the requested target.

The tools themselves are tested without an LLM by
`cargo test -p yanshi-mcp --test media_tools_e2e -- --ignored`.
