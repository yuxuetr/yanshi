# Yanshi CLI Commands Reference

Complete reference for all Yanshi CLI commands, parameters, and usage patterns.

## 🎨 Image Commands

### `yanshi image generate` (alias: `gen`)

Generate images from text descriptions. The default model is OpenAI's `gpt-image-2` (needs `OPENAI_API_KEY`); any registry model of type `text_to_image` works — list them with `yanshi llm models --type text_to_image`.

#### Syntax
```bash
yanshi image generate <PROMPT> --output <OUTPUT_FILE> [OPTIONS]
```

#### Required Parameters
- `<PROMPT>`: Text description of the image to generate
- `--output`, `-o`: Output file path (e.g., `image.png`)

#### Optional Parameters
| Parameter | Default | Description |
|-----------|---------|-------------|
| `--model`, `-m` | `gpt-image-2` | Model name (e.g. `gpt-image-2`, `imagen-4.0-generate-preview-06-06`, `wan2.5-t2i-preview`) |
| `--size`, `-s` | `1024x1024` | Image dimensions (supported sizes depend on the model) |
| `--format`, `-f` | `b64_json` | Response format (`b64_json`, `url`) |
| `--steps` | `20` | Inference steps (only honored by diffusion-style models) |
| `--cfg-scale` | `7.5` | CFG scale (only honored by diffusion-style models) |
| `--seed` | Random | Seed for reproducible generation, where the model supports it |

#### Examples
```bash
# Basic image generation
yanshi image generate "A sunset over mountains" --output sunset.png

# Explicit model and size
yanshi image generate "Cyberpunk cityscape with neon lights" \
  --model gpt-image-2 \
  --size 1536x1024 \
  --output cyberpunk.png

# Using alias
yanshi image gen "Abstract art" --output abstract.png
```

### `yanshi image understand` (alias: `analyze`)

Analyze and describe images using a vision-capable chat model.

#### Syntax
```bash
yanshi image understand <IMAGE_PATH> <PROMPT> [OPTIONS]
```

#### Required Parameters
- `<IMAGE_PATH>`: Path to image file (jpg, png, gif, webp, bmp)
- `<PROMPT>`: Question or instruction for analysis

#### Optional Parameters
| Parameter | Default | Description |
|-----------|---------|-------------|
| `--model`, `-m` | — | Vision model name (e.g. `step-1o-turbo-vision`, `step-3.7-flash`) |
| `--temperature`, `-t` | `0.7` | Response creativity (0.0-1.0) |
| `--max-tokens` | `800` | Maximum response length |
| `--output`, `-o` | Console | Save analysis to file |

#### Examples
```bash
# Basic image understanding
yanshi image understand photo.jpg "What do you see in this image?"

# Detailed analysis with output file
yanshi image understand artwork.png \
  "Analyze the artistic style, composition, and color palette" \
  --model step-1o-turbo-vision \
  --temperature 0.8 \
  --max-tokens 1000 \
  --output detailed_analysis.md

# Using alias
yanshi image analyze document.png "Extract the text from this document"
```

## 🎧 Audio Commands

### `yanshi audio tts` (alias: `audio text-to-speech`)

Convert text to speech using StepFun's TTS models.

#### Syntax
```bash
yanshi audio tts <TEXT> --output <OUTPUT_FILE> [OPTIONS]
```

#### Required Parameters
- `<TEXT>`: Text to convert to speech
- `--output`, `-o`: Output audio file path

#### Optional Parameters
| Parameter | Default | Description |
|-----------|---------|-------------|
| `--model`, `-m` | `step-tts-mini` | TTS model name |
| `--voice`, `-v` | `default` | Voice name (see Voice Options below) |
| `--format`, `-f` | `mp3` | Audio format (`mp3`, `wav`, `flac`) |
| `--speed` | `1.0` | Speech speed (0.5-2.0) |
| `--emotion` | None | Voice emotion/style |

#### Voice Options
- `cixingnansheng`: Male voice (tested)
- `default`: Default voice (may need adjustment)
- Other voices: Check StepFun documentation

#### Examples
```bash
# Basic text-to-speech
yanshi audio tts "Hello from Yanshi!" --output hello.mp3

# With specific voice and parameters
yanshi audio tts "Welcome to our presentation" \
  --voice cixingnansheng \
  --format wav \
  --speed 0.9 \
  --output welcome.wav

# Emotional speech (if supported by model)
yanshi audio tts "This is exciting news!" \
  --voice cixingnansheng \
  --emotion "高兴" \
  --output excited.mp3
```

### `yanshi audio asr` (alias: `audio speech-to-text`)

Convert speech to text using StepFun's ASR models.

#### Syntax
```bash
yanshi audio asr <AUDIO_FILE> [OPTIONS]
```

#### Required Parameters
- `<AUDIO_FILE>`: Path to audio file (mp3, wav, flac)

#### Optional Parameters
| Parameter | Default | Description |
|-----------|---------|-------------|
| `--model`, `-m` | `step-asr` | ASR model name |
| `--format`, `-f` | `text` | Output format (`text`, `json`, `srt`, `vtt`) |
| `--language`, `-l` | Auto-detect | Language code (`zh`, `en`, `ja`) |
| `--output`, `-o` | Console | Save transcript to file |

#### Examples
```bash
# Basic transcription
yanshi audio asr recording.wav --output transcript.txt

# JSON format with language specification
yanshi audio asr interview.mp3 \
  --format json \
  --language zh \
  --output interview_transcript.json

# Generate subtitles
yanshi audio asr lecture.wav \
  --format srt \
  --output subtitles.srt
```

## 💬 LLM Commands

### `yanshi llm prompt`

Send text prompts to language models.

#### Syntax
```bash
yanshi llm prompt <TEXT> [OPTIONS]
```

#### Optional Parameters
| Parameter | Default | Description |
|-----------|---------|-------------|
| `--model`, `-m` | Auto | Model name |
| `--temperature`, `-t` | `0.7` | Response creativity (0.0-1.0) |
| `--max-tokens` | Model default | Maximum response length |
| `--output`, `-o` | Console | Save response to file |
| `--stream` | false | Enable streaming output |
| `--system` | None | System prompt |

#### Examples
```bash
# Basic prompt
yanshi llm prompt "Explain quantum computing"

# With specific model and parameters
yanshi llm prompt "Write a Python function to sort a list" \
  --model step-3.5-flash \
  --temperature 0.5 \
  --max-tokens 500 \
  --output code_example.py
```

### Agent-first interaction

The standalone `yanshi llm chat` command has been retired. Yanshi
interactions should go through Skills, Agents, or workflows:

```bash
yanshi skill chat ./skills/code-reviewer --model step-3.5-flash
yanshi skill run ./skills/code-reviewer --message "Review this" --model step-3.5-flash
yanshi workflow run flow.yml --model step-3.5-flash
```

## ⚙️ Configuration Commands

### `yanshi config init`

Initialize Yanshi configuration files.

#### Syntax
```bash
yanshi config init [OPTIONS]
```

#### Optional Parameters
| Parameter | Description |
|-----------|-------------|
| `--force`, `-f` | Force overwrite existing configuration |

### `yanshi config show`

Display current configuration.

#### Syntax
```bash
yanshi config show [SECTION]
```

#### Optional Parameters
- `[SECTION]`: Specific configuration section to show

## 🌐 Global Options

These options work with all commands:

| Option | Description |
|--------|-------------|
| `--log-level` | Set log level (`error`, `warn`, `info`, `debug`, `trace`) |
| `--output-format` | Output format (`json`, `yaml`, `text`) |
| `--no-color` | Disable colored output |
| `--verbose`, `-v` | Verbose output |
| `--help`, `-h` | Show help |
| `--version`, `-V` | Show version |

## 🔌 MCP (Model Context Protocol) Commands

### `yanshi mcp list-tools`

List available tools from an MCP server to discover what operations are supported.

#### Syntax
```bash
yanshi mcp list-tools <SERVER_COMMAND> [OPTIONS]
```

#### Required Parameters
- `<SERVER_COMMAND>`: Space-separated command to execute the MCP server
  - Example: `npx -y @modelcontextprotocol/server-filesystem /tmp`

#### Optional Parameters
| Parameter | Default | Description |
|-----------|---------|-------------|
| `--timeout-ms` | `30000` | Request timeout in milliseconds |
| `--max-retries` | `3` | Maximum number of retry attempts |

#### Examples
```bash
# List filesystem server tools
yanshi mcp list-tools npx -y @modelcontextprotocol/server-filesystem /tmp

# List with custom timeout
yanshi mcp list-tools npx -y @modelcontextprotocol/server-database \
  --timeout-ms 60000
```

---

### `yanshi mcp call-tool`

Execute a specific tool on an MCP server with parameters.

#### Syntax
```bash
yanshi mcp call-tool <SERVER_COMMAND> --tool <TOOL_NAME> [OPTIONS]
```

#### Required Parameters
- `<SERVER_COMMAND>`: Command to execute the MCP server
- `--tool`, `-t`: Tool name to call

#### Optional Parameters
| Parameter | Default | Description |
|-----------|---------|-------------|
| `--params`, `-p` | `{}` | Tool parameters as JSON string |
| `--timeout-ms` | `30000` | Request timeout in milliseconds |
| `--max-retries` | `3` | Maximum number of retry attempts |
| `--output`, `-o` | - | Save result to file (JSON format) |

#### Examples
```bash
# Read a file
yanshi mcp call-tool npx -y @modelcontextprotocol/server-filesystem /tmp \
  --tool read_file \
  --params '{"path": "/tmp/config.json"}'

# List directory with output save
yanshi mcp call-tool npx -y @modelcontextprotocol/server-filesystem /tmp \
  --tool list_directory \
  --params '{"path": "/tmp"}' \
  --output directory_list.json
```

---

### `yanshi mcp list-resources`

List available resources (data sources) from an MCP server.

#### Syntax
```bash
yanshi mcp list-resources <SERVER_COMMAND> [OPTIONS]
```

#### Required Parameters
- `<SERVER_COMMAND>`: Command to execute the MCP server

#### Optional Parameters
| Parameter | Default | Description |
|-----------|---------|-------------|
| `--timeout-ms` | `30000` | Request timeout in milliseconds |
| `--max-retries` | `3` | Maximum number of retry attempts |

#### Examples
```bash
# List filesystem resources
yanshi mcp list-resources npx -y @modelcontextprotocol/server-filesystem /data
```

**See Also**: For comprehensive MCP integration guide, see [MCP_GUIDE.md](MCP_GUIDE.md)

---

## 🔍 Command Aliases

For faster usage, many commands have shorter aliases:

| Full Command | Alias |
|-------------|-------|
| `image generate` | `image gen` |
| `image understand` | `image analyze` |
| `audio text-to-speech` | `audio tts` |
| `audio speech-to-text` | `audio asr` |
| `llm prompt` | `llm p` |

## 💡 Usage Tips

### File Paths
- Always use quotes for paths with spaces: `"My Documents/image.png"`
- Relative paths work: `./output/result.png`
- Absolute paths are recommended for scripts

### Output Files
- Parent directories must exist
- File extensions determine format where applicable
- Use `-` as output to write to stdout (where supported)

### Error Codes
- `0`: Success
- `1`: General error (API error, file not found, etc.)
- Check error messages for specific details

### Performance Tips
- Use smaller image sizes for testing
- Use appropriate models for your use case

### Scripting
All commands are script-friendly:
```bash
#!/bin/bash
set -e  # Exit on error

yanshi image generate "Logo design" --output logo.png
if [ -f "logo.png" ]; then
    echo "Logo generated successfully"
    yanshi image understand logo.png "Describe this logo" --output description.txt
fi
```

---

**Need more help?** Use `yanshi [command] --help` for detailed help on any specific command!
