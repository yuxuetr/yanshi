# A8 — content-studio

One source in, the deliverables you ask for out. This is the Content
Studio shape from `docs/RFC_CONTENT_STUDIO.md` §3: a planner agent picks
from a closed menu of generators, and each generator is a hand-written
workflow template.

```
yanshi skill run → planner (ReAct) → report / mindmap / audio_overview / slides / flashcards / quiz tools
                                        │  (WorkflowTool over examples/generators/*.yml)
                                        └→ files → --output-dir/<tool>/<node>/<file>
```

The model decides **what** to make (which deliverables, focus, length).
The templates decide **how**. A failed generator is attributable to one
template and its run, never to an ad-hoc plan.

## Run

```bash
yanshi doctor                     # pandoc + typst for the PDF, ffmpeg for audio
yanshi skill run examples/applications/content-studio \
  --input url=https://example.org/article \
  --input want=report,mindmap,audio_overview \
  --output-dir out
```

`--input` facts are appended to the request. `--model` overrides the
planner model and the generators' LLM steps. The audio overview's TTS
model and voices are generator inputs (`tts_model`, `host_voice`,
`guest_voice`), and the planner can pass them.

Output:

```
out/report/report_md/1.md       out/mindmap/map_svg/1.svg      out/audio_overview/episode/1.mp3
out/report/report_html/1.html   out/mindmap/map_png/1.png
out/report/report_pdf/1.pdf     out/mindmap/map_html/1.html    out/quiz/page/1.html
```

Each tool directory also has an `artifacts.json` (size and SHA-256 per
file). The planner receives each generator's `Deliverable` (title,
content, citations, file names), not the runs' full state.

## Tests

- CI: `cargo test -p yanshi-cli --test content_studio_cli_tests` scripts
  the mock model (planner turn → report + mind map → final answer) and
  checks the exported files.
- `--ignored` adds the audio overview and the PDF (needs ffmpeg, pandoc
  and typst).
