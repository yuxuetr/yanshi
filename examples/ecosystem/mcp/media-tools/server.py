#!/usr/bin/env python3
"""media-tools: a stdio MCP server that wraps ffmpeg (Y3.2).

Five tools for the audio / video finishing steps a content run needs:
audio_concat, loudness_normalize, mux_video_audio, burn_subtitles and
images_to_slideshow. Every path a tool reads or writes must resolve
(symlinks included) inside the --root directory, normally the run's
directory, and ffmpeg is invoked with an argument vector, never a shell.

Speaks MCP 2024-11-05 over newline-delimited JSON-RPC on stdin/stdout
(initialize, tools/list, tools/call). Standard library only.

  python3 server.py --root /path/to/run_dir [--ffmpeg ffmpeg] [--timeout 600]
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from typing import Any, Callable

PROTOCOL_VERSION = "2024-11-05"
SERVER_INFO = {"name": "media-tools", "version": "0.1.0"}


class ToolError(Exception):
  """A failure reported to the caller as an isError tool result."""


class MediaTools:
  def __init__(self, root: str, ffmpeg: str, timeout: int) -> None:
    self.root = os.path.realpath(root)
    self.ffmpeg = ffmpeg
    self.timeout = timeout

  # ── path confinement ──────────────────────────────────────────────

  def _inside_root(self, path: str) -> bool:
    return path == self.root or path.startswith(self.root + os.sep)

  def source(self, path: Any) -> str:
    if not isinstance(path, str) or not path:
      raise ToolError("expected a file path string")
    resolved = os.path.realpath(os.path.join(self.root, path))
    if not self._inside_root(resolved):
      raise ToolError(f"{path!r} is outside the allowed root")
    if not os.path.isfile(resolved):
      raise ToolError(f"{path!r} does not exist")
    return resolved

  def destination(self, path: Any) -> str:
    if not isinstance(path, str) or not path:
      raise ToolError("expected an output path string")
    candidate = os.path.join(self.root, path)
    parent = os.path.realpath(os.path.dirname(candidate) or self.root)
    if not self._inside_root(parent):
      raise ToolError(f"{path!r} is outside the allowed root")
    target = os.path.join(parent, os.path.basename(candidate))
    if os.path.islink(target):
      raise ToolError(f"{path!r} is a symlink; refusing to overwrite through it")
    os.makedirs(parent, exist_ok=True)
    return target

  # ── ffmpeg ────────────────────────────────────────────────────────

  def run(self, args: list[str]) -> str:
    command = [self.ffmpeg, "-hide_banner", "-nostdin", "-y", *args]
    try:
      done = subprocess.run(
        command, capture_output=True, text=True, timeout=self.timeout, check=False
      )
    except FileNotFoundError as e:
      raise ToolError(f"ffmpeg not found ({self.ffmpeg}): {e}") from e
    except subprocess.TimeoutExpired as e:
      raise ToolError(f"ffmpeg timed out after {self.timeout}s") from e
    if done.returncode != 0:
      tail = "\n".join(done.stderr.strip().splitlines()[-12:])
      raise ToolError(f"ffmpeg exited with {done.returncode}:\n{tail}")
    return done.stderr

  def written(self, output: str, **extra: Any) -> dict[str, Any]:
    return {"output": output, "bytes": os.path.getsize(output), **extra}

  # ── tools ─────────────────────────────────────────────────────────

  def audio_concat(self, args: dict[str, Any]) -> dict[str, Any]:
    inputs = args.get("inputs")
    if not isinstance(inputs, list) or len(inputs) < 2:
      raise ToolError("inputs must list at least two audio files")
    sources = [self.source(p) for p in inputs]
    output = self.destination(args.get("output"))
    command: list[str] = []
    for src in sources:
      command += ["-i", src]
    streams = "".join(f"[{i}:a]" for i in range(len(sources)))
    command += [
      "-filter_complex",
      f"{streams}concat=n={len(sources)}:v=0:a=1[a]",
      "-map",
      "[a]",
      output,
    ]
    self.run(command)
    return self.written(output, segments=len(sources))

  def loudness_normalize(self, args: dict[str, Any]) -> dict[str, Any]:
    src = self.source(args.get("input"))
    output = self.destination(args.get("output"))
    target = float(args.get("target_lufs", -16.0))
    true_peak = float(args.get("true_peak", -1.5))
    lra = float(args.get("lra", 11.0))
    base = f"loudnorm=I={target}:TP={true_peak}:LRA={lra}"
    measured = loudnorm_stats(self.run(["-i", src, "-af", f"{base}:print_format=json", "-f", "null", "-"]))
    second = (
      f"{base}:measured_I={measured['input_i']}:measured_TP={measured['input_tp']}"
      f":measured_LRA={measured['input_lra']}:measured_thresh={measured['input_thresh']}"
      f":offset={measured['target_offset']}:linear=true:print_format=json"
    )
    final = loudnorm_stats(self.run(["-i", src, "-af", second, output]))
    return self.written(
      output,
      input_lufs=float(measured["input_i"]),
      output_lufs=float(final["output_i"]),
      target_lufs=target,
    )

  def mux_video_audio(self, args: dict[str, Any]) -> dict[str, Any]:
    video = self.source(args.get("video"))
    audio = self.source(args.get("audio"))
    output = self.destination(args.get("output"))
    self.run([
      "-i", video, "-i", audio,
      "-map", "0:v:0", "-map", "1:a:0",
      "-c:v", "copy", "-c:a", "aac", "-shortest",
      output,
    ])
    return self.written(output)

  def burn_subtitles(self, args: dict[str, Any]) -> dict[str, Any]:
    video = self.source(args.get("video"))
    subtitles = self.source(args.get("subtitles"))
    output = self.destination(args.get("output"))
    self.run(["-i", video, "-vf", f"subtitles=filename={filter_quote(subtitles)}", "-c:a", "copy", output])
    return self.written(output)

  def images_to_slideshow(self, args: dict[str, Any]) -> dict[str, Any]:
    images = args.get("images")
    if not isinstance(images, list) or not images:
      raise ToolError("images must list at least one image")
    sources = [self.source(p) for p in images]
    output = self.destination(args.get("output"))
    seconds = float(args.get("seconds_per_image", 3.0))
    width = int(args.get("width", 1280))
    height = int(args.get("height", 720))
    if seconds <= 0 or width <= 0 or height <= 0 or width % 2 or height % 2:
      raise ToolError("seconds_per_image must be > 0; width and height must be positive and even")
    command: list[str] = []
    for src in sources:
      command += ["-loop", "1", "-t", str(seconds), "-i", src]
    fit = (
      f"scale={width}:{height}:force_original_aspect_ratio=decrease,"
      f"pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30,format=yuv420p"
    )
    chains = ";".join(f"[{i}:v]{fit}[v{i}]" for i in range(len(sources)))
    joined = "".join(f"[v{i}]" for i in range(len(sources)))
    graph = f"{chains};{joined}concat=n={len(sources)}:v=1:a=0[v]"
    command += ["-filter_complex", graph, "-map", "[v]"]
    audio = args.get("audio")
    if audio:
      command += ["-i", self.source(audio), "-map", f"{len(sources)}:a:0", "-c:a", "aac", "-shortest"]
    command += ["-c:v", "libx264", "-pix_fmt", "yuv420p", output]
    self.run(command)
    return self.written(output, duration_seconds=seconds * len(sources))


def loudnorm_stats(stderr: str) -> dict[str, str]:
  """The JSON block ffmpeg's loudnorm prints at the end of stderr."""
  match = re.search(r"\{[^{}]*\"input_i\"[^{}]*\}", stderr, re.S)
  if not match:
    raise ToolError("ffmpeg did not report loudness statistics")
  return json.loads(match.group(0))


def filter_quote(path: str) -> str:
  """Quote a path for use as an ffmpeg filter option value."""
  escaped = path.replace("\\", "\\\\").replace("'", "\\'").replace(":", "\\:")
  return f"'{escaped}'"


def schema(properties: dict[str, Any], required: list[str]) -> dict[str, Any]:
  return {"type": "object", "properties": properties, "required": required}


PATH = {"type": "string", "description": "Path relative to (or inside) the server's root"}
PATHS = {"type": "array", "items": PATH}

TOOLS: list[dict[str, Any]] = [
  {
    "name": "audio_concat",
    "description": "Join two or more audio files end to end into one file (format from the output extension).",
    "inputSchema": schema({"inputs": PATHS, "output": PATH}, ["inputs", "output"]),
  },
  {
    "name": "loudness_normalize",
    "description": "Two-pass EBU R128 loudness normalisation; reports measured input and output LUFS.",
    "inputSchema": schema(
      {
        "input": PATH,
        "output": PATH,
        "target_lufs": {"type": "number", "default": -16},
        "true_peak": {"type": "number", "default": -1.5},
        "lra": {"type": "number", "default": 11},
      },
      ["input", "output"],
    ),
  },
  {
    "name": "mux_video_audio",
    "description": "Put an audio track on a video (video stream copied, audio as AAC, cut to the shorter).",
    "inputSchema": schema({"video": PATH, "audio": PATH, "output": PATH}, ["video", "audio", "output"]),
  },
  {
    "name": "burn_subtitles",
    "description": "Render an .srt / .ass subtitle file into the video picture.",
    "inputSchema": schema({"video": PATH, "subtitles": PATH, "output": PATH}, ["video", "subtitles", "output"]),
  },
  {
    "name": "images_to_slideshow",
    "description": "Turn images into an H.264 video, each shown for seconds_per_image, letterboxed to width x height, optionally with an audio track.",
    "inputSchema": schema(
      {
        "images": PATHS,
        "output": PATH,
        "seconds_per_image": {"type": "number", "default": 3},
        "width": {"type": "integer", "default": 1280},
        "height": {"type": "integer", "default": 720},
        "audio": PATH,
      },
      ["images", "output"],
    ),
  },
]


def handle(tools: MediaTools, request: dict[str, Any]) -> dict[str, Any] | None:
  method = request.get("method")
  if "id" not in request:
    return None  # notification (e.g. notifications/initialized)
  if method == "initialize":
    result: Any = {
      "protocolVersion": PROTOCOL_VERSION,
      "capabilities": {"tools": {}},
      "serverInfo": SERVER_INFO,
    }
  elif method == "tools/list":
    result = {"tools": TOOLS}
  elif method == "tools/call":
    params = request.get("params") or {}
    name = params.get("name")
    handler: Callable[[dict[str, Any]], dict[str, Any]] | None = getattr(tools, str(name), None)
    if name not in {tool["name"] for tool in TOOLS} or handler is None:
      return error(request["id"], -32602, f"unknown tool {name!r}")
    try:
      payload = handler(params.get("arguments") or {})
      result = {"content": [{"type": "text", "text": json.dumps(payload)}], "isError": False}
    except (ToolError, ValueError, KeyError) as e:
      result = {"content": [{"type": "text", "text": str(e)}], "isError": True}
  elif method == "ping":
    result = {}
  else:
    return error(request["id"], -32601, f"method not found: {method}")
  return {"jsonrpc": "2.0", "id": request["id"], "result": result}


def error(request_id: Any, code: int, message: str) -> dict[str, Any]:
  return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def main() -> int:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument("--root", required=True, help="every path must resolve inside this directory")
  parser.add_argument("--ffmpeg", default=shutil.which("ffmpeg") or "ffmpeg")
  parser.add_argument("--timeout", type=int, default=600, help="seconds per ffmpeg call")
  options = parser.parse_args()
  try:
    os.makedirs(options.root, exist_ok=True)
  except OSError as e:
    print(f"--root {options.root}: {e}", file=sys.stderr)
    return 2
  tools = MediaTools(options.root, options.ffmpeg, options.timeout)
  for line in sys.stdin:
    line = line.strip()
    if not line:
      continue
    try:
      request = json.loads(line)
    except json.JSONDecodeError as e:
      response: dict[str, Any] | None = error(None, -32700, f"parse error: {e}")
    else:
      response = handle(tools, request)
    if response is not None:
      sys.stdout.write(json.dumps(response) + "\n")
      sys.stdout.flush()
  return 0


if __name__ == "__main__":
  sys.exit(main())
