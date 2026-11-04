# media-tools — ffmpeg as MCP tools

A dependency-free (standard-library Python) stdio MCP server that gives an
agent or a workflow the audio / video finishing steps a content run needs:

| tool | does |
| --- | --- |
| `audio_concat` | join two or more audio files end to end (`inputs`, `output`) |
| `loudness_normalize` | two-pass EBU R128 to `target_lufs` (default -16); reports measured `input_lufs` and `output_lufs` |
| `mux_video_audio` | put an audio track on a video (`video`, `audio`, `output`) |
| `burn_subtitles` | render an `.srt` / `.ass` file into the picture (needs an ffmpeg built with libass) |
| `images_to_slideshow` | images → H.264 video, `seconds_per_image` each, letterboxed to `width`×`height`, optional `audio` |

Every result is a JSON text block with the `output` path and its size, plus
tool-specific fields.

## Safety

- Every path a tool reads or writes must resolve, symlinks included,
  inside `--root`. Point it at the run directory, never at `/` or `$HOME`.
- ffmpeg runs from an argument vector, never through a shell, with a
  per-call timeout (`--timeout`, default 600 s).
- No network access is needed or requested.

## Run

```bash
python3 examples/ecosystem/mcp/media-tools/server.py --root ./out
```

`yanshi doctor` reports whether `ffmpeg` / `ffprobe` are on PATH.

In a skill manifest (tools surface as `mcp_media_<tool>`):

```toml
[[mcp_servers]]
name    = "media"
command = "python3"
args    = ["examples/ecosystem/mcp/media-tools/server.py", "--root", "./out"]
```

`python3` is on the default MCP command allowlist, so no
`mcp_command_allowlist` change is needed.
[`examples/applications/podcast-mastering`](../../../applications/podcast-mastering/)
uses it.

## Test

```bash
cargo test -p yanshi-mcp --test media_tools_e2e -- --ignored
```

The test generates its media with ffmpeg, drives every tool through
yanshi's own stdio MCP client, and checks that reads and writes outside
the root are refused. CI runs it in the `media-tools` job of
`quality.yml`.
