# Yanshi CLI: New Image and Audio Commands

This document describes the newly implemented image and audio commands that provide a unified, discoverable CLI interface for image and audio models.

## 🎯 Overview

Previously, image generation, image understanding, and audio processing required using separate Rust binaries or shell scripts. Now everything is integrated into the main `yanshi` CLI with consistent patterns and comprehensive help.

## 🚀 Quick Start

### Prerequisites
```bash
# StepFun key: audio (step-tts-mini / step-asr) and StepFun vision models
export STEP_API_KEY="your-stepfun-api-key"
# OpenAI key: image generation defaults to gpt-image-2
export OPENAI_API_KEY="your-openai-api-key"

# Install Yanshi CLI (if not already installed)
cargo install --path yanshi-cli
```

### Discovery
```bash
# See all available commands
yanshi --help

# Explore image commands
yanshi image --help

# Explore audio commands  
yanshi audio --help
```

## 🖼️ Image Commands

### Generate Images
```bash
# Basic image generation
yanshi image generate "A serene mountain landscape at sunset" --output landscape.png

# Advanced generation with parameters
yanshi image generate "A cyberpunk cityscape with neon lights" \
  --model gpt-image-2 \
  --size 1024x1024 \
  --output cyberpunk.png

# Using alias
yanshi image gen "Abstract art" --output abstract.png
```

### Understand Images
```bash
# Basic image analysis
yanshi image understand photo.jpg "Describe this image in detail"

# Advanced analysis with output
yanshi image understand artwork.png "Analyze the artistic style and composition" \
  --model step-1o-turbo-vision \
  --temperature 0.7 \
  --max-tokens 500 \
  --output analysis.md

# Using alias
yanshi image analyze landscape.jpg "What time of day is this?"
```

## 🎧 Audio Commands

### Text-to-Speech (TTS)
```bash
# Basic text-to-speech
yanshi audio tts "Hello, welcome to Yanshi!" --output welcome.mp3

# Advanced TTS with parameters
yanshi audio tts "This is a professional announcement." \
  --model step-tts-mini \
  --voice professional_male \
  --format wav \
  --speed 0.9 \
  --emotion "formal" \
  --output announcement.wav

# Using alias
yanshi audio tts "Quick test" --output test.mp3
```

### Speech Recognition (ASR)
```bash
# Basic transcription
yanshi audio asr recording.wav --output transcript.txt

# Advanced transcription with JSON output
yanshi audio asr interview.mp3 \
  --model step-asr \
  --format json \
  --language zh \
  --output interview_transcript.json

# Generate subtitles
yanshi audio asr lecture.wav --format srt --output subtitles.srt

# Using alias
yanshi audio asr meeting.wav --output meeting_notes.txt
```

## 📊 Command Comparison

| Feature | Before | After |
|---------|--------|-------|
| **Image Understanding** | `yanshi llm prompt "text" --file image.jpg` | `yanshi image understand image.jpg "question"` |
| **Text-to-Speech** | `./stepfun_tts_cli.sh` (limited) | `yanshi audio tts "text" --output audio.mp3` |
| **Speech Recognition** | `./stepfun_asr_cli.sh` (limited) | `yanshi audio asr audio.wav --output transcript.txt` |

## 🎛️ Available Parameters

### Image Generate
- `--model`: Model name (default: gpt-image-2)
- `--size`: Image dimensions (default: 1024x1024)
- `--steps` / `--cfg-scale`: Only honored by diffusion-style models
- `--seed`: Random seed for reproducibility
- `--format`: Output format (b64_json, url)

### Image Understand
- `--model`: Vision model name (e.g. step-1o-turbo-vision, step-3.7-flash)
- `--temperature`: Creativity (0.0-1.0)
- `--max-tokens`: Response length
- `--output`: Save analysis to file

### Audio TTS
- `--model`: Model name (default: step-tts-mini)
- `--voice`: Voice name (default: default)
- `--format`: Audio format (mp3, wav, flac)
- `--speed`: Speech rate (0.5-2.0)
- `--emotion`: Voice emotion/style

### Audio ASR
- `--model`: Model name (default: step-asr)
- `--format`: Output format (text, json, srt, vtt)
- `--language`: Language code (auto-detect if not specified)

## 🧪 Testing

### Run Tests
```bash
# Test CLI structure (no API calls)
./test_cli_structure.sh

# Full test with API calls (requires valid API key)
./test_new_commands.sh

# Quick API validation
./quick_api_test.sh
```

### View Comparisons
```bash
# See before/after comparison
./comparison_demo.sh
```

## 🔧 Troubleshooting

### API Key Issues
```bash
# Error: "STEPFUN_API_KEY or STEP_API_KEY environment variable must be set"
export STEP_API_KEY="your-actual-api-key"
```

### Command Not Found
```bash
# Install or reinstall Yanshi CLI
cargo install --path yanshi-cli --force
```

## 📝 Command Aliases

All commands support shorter aliases for convenience:

| Full Command | Alias |
|-------------|-------|
| `image generate` | `image gen` |
| `image understand` | `image analyze` |
| `audio text-to-speech` | `audio tts` |
| `audio speech-to-text` | `audio asr` |

## 🎉 Benefits

✅ **Unified Discovery**: All commands discoverable through `yanshi --help`  
✅ **Consistent Interface**: Same parameter patterns across all commands  
✅ **No Compilation**: No need for separate Rust binaries  
✅ **Rich Help**: Comprehensive help for every command and parameter  
✅ **Better Error Messages**: Clear, actionable error information  
✅ **File I/O**: Consistent input/output file handling  
✅ **Aliases**: Short forms for faster usage  
✅ **Modern UX**: Professional CLI experience matching industry standards

## 🚀 Next Steps

1. **Set your API keys**: Export `STEP_API_KEY` (audio, StepFun models) and `OPENAI_API_KEY` (image generation)
2. **Try the commands**: Start with simple examples
3. **Read the help**: Use `--help` on any command for details
4. **Integrate workflows**: Use these commands in your automation scripts
5. **Provide feedback**: Report issues or suggestions

---

*This implementation transforms Yanshi from a collection of separate tools into a unified, professional CLI experience that users can easily discover and use.*