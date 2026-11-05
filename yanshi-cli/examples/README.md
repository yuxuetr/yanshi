# Yanshi CLI Examples

Welcome to the Yanshi CLI examples directory! This collection demonstrates all capabilities of the unified Yanshi CLI, including text generation, image creation, image understanding, audio processing, and more.

## 📁 Directory Structure

```
examples/
├── README.md                     # This file - main overview and quick start
├── assets/                      # Sample inputs
│   └── sample_images/          # Test images for understanding
├── documentation/              # Detailed guides and references
│   ├── GETTING_STARTED.md     # Step-by-step setup guide
│   ├── COMMANDS_REFERENCE.md  # Complete command reference
│   ├── MIGRATION_GUIDE.md     # Migrating from old examples
│   └── TROUBLESHOOTING.md     # Common issues and solutions
├── tests/                      # Test scripts and validation
│   ├── test_all_commands.sh   # Comprehensive functionality test
│   ├── test_cli_structure.sh  # CLI structure validation
│   └── quick_validation.sh    # Quick API connectivity test
└── tutorials/                  # Step-by-step learning examples
    ├── 01_quick_start.sh      # Basic usage tutorial
    └── 03_audio_workflows.sh  # Audio processing examples
```

## 🚀 Quick Start

### Prerequisites

1. **Install Yanshi CLI**:
   ```bash
   cargo install --path yanshi-cli
   ```

2. **Set your provider API keys**:
   ```bash
   # Image generation defaults to OpenAI's gpt-image-2
   export OPENAI_API_KEY="your-openai-api-key-here"
   # Audio (step-tts-mini / step-asr) and StepFun chat models
   export STEP_API_KEY="your-stepfun-api-key-here"
   ```

3. **Verify installation**:
   ```bash
   yanshi --help
   ```

### Try Your First Commands

```bash
# Test connectivity first (recommended)
./tests/test_api_connectivity.sh

# Generate an image (defaults to gpt-image-2)
yanshi image generate "A serene mountain landscape at sunset" --output mountain.png

# Understand an image
yanshi image understand mountain.png "Describe the mood and atmosphere"

# Create speech from text
yanshi audio tts "Welcome to Yanshi CLI!" --output welcome.mp3

# Transcribe audio
yanshi audio asr welcome.mp3 --output transcript.txt
```

## 🧪 Testing and Validation

### Quick Validation
```bash
# Step 1: Test CLI structure (no API calls needed)
./tests/test_cli_structure.sh

# Step 2: Test API connectivity and key validity
export STEP_API_KEY="your-key" && ./tests/test_api_connectivity.sh

# Step 3: Quick functionality test
./tests/quick_validation.sh
```

### Comprehensive Testing
```bash
# Full functionality test with real API calls
export STEP_API_KEY="your-key" && ./tests/test_all_commands.sh
```

## 📚 Learning Path

### 1. **Getting Started** → [`documentation/GETTING_STARTED.md`](documentation/GETTING_STARTED.md)
Complete setup guide from installation to first API call

### 2. **Basic Tutorial** → [`tutorials/01_quick_start.sh`](tutorials/01_quick_start.sh)
Learn core commands with hands-on examples

### 3. **Audio Processing** → [`tutorials/03_audio_workflows.sh`](tutorials/03_audio_workflows.sh)
Explore text-to-speech and speech recognition

### 4. **Commands Reference** → [`documentation/COMMANDS_REFERENCE.md`](documentation/COMMANDS_REFERENCE.md)
Complete parameter guide for all commands

## 🎯 Key Features Demonstrated

### ✅ **Unified CLI Experience**
- Text, vision, image and audio capabilities across providers in one command
- Consistent parameter patterns
- Comprehensive help system

### ✅ **Image Processing**
```bash
# Generate: Text → Image
yanshi image generate "cyberpunk city" --size 1024x1024 --output city.png

# Understand: Image → Text  
yanshi image understand photo.jpg "What's happening here?"
```

### ✅ **Audio Processing**
```bash
# Text → Speech
yanshi audio tts "Hello world" --voice cixingnansheng --output hello.mp3

# Speech → Text
yanshi audio asr recording.wav --format json --output transcript.json
```

### ✅ **Developer-Friendly**
- Rich error messages
- File I/O support  
- Command aliases (`gen`, `analyze`, `tts`, `asr`)
- Scriptable and automation-ready

## 🔧 Command Categories

| Command | Purpose | Example |
|---------|---------|---------|
| `yanshi llm` | Text generation and chat | `yanshi llm prompt "Explain AI"` |
| `yanshi image generate` | Create images from text | `yanshi image gen "sunset" -o sunset.png` |
| `yanshi image understand` | Analyze and describe images | `yanshi image analyze photo.jpg "What is this?"` |
| `yanshi audio tts` | Convert text to speech | `yanshi audio tts "Hello" -o hello.mp3` |
| `yanshi audio asr` | Convert speech to text | `yanshi audio asr audio.wav -o transcript.txt` |
| `yanshi config` | Manage configuration | `yanshi config show` |

## 🆘 Need Help?

- **Quick Help**: `yanshi --help` or `yanshi [command] --help`
- **Setup Issues**: See [`documentation/GETTING_STARTED.md`](documentation/GETTING_STARTED.md)
- **Command Issues**: See [`documentation/TROUBLESHOOTING.md`](documentation/TROUBLESHOOTING.md)
- **Migration**: See [`documentation/MIGRATION_GUIDE.md`](documentation/MIGRATION_GUIDE.md)

## 🎉 What's New

**Yanshi CLI now provides a unified interface for text, vision, image and audio models!**

❌ **Before**: Separate Rust binaries, complex shell scripts, inconsistent interfaces  
✅ **After**: One CLI, discoverable commands, professional UX

Experience the difference with:
```bash
./tests/comparison_demo.sh
```

---

**Ready to explore?** Start with `./tutorials/01_quick_start.sh` and dive in! 🚀