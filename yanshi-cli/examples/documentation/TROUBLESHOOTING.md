# Troubleshooting Guide

Common issues and solutions for Yanshi CLI usage.

## 🚨 Installation Issues

### "command not found: yanshi"
**Problem**: Yanshi CLI binary not found in PATH

**Solutions**:
1. **Reinstall the CLI**:
   ```bash
   cargo install --path yanshi-cli --force
   ```

2. **Check PATH configuration**:
   ```bash
   echo $PATH | grep -q "$HOME/.cargo/bin" || echo "Add ~/.cargo/bin to PATH"
   ```

3. **Add to shell profile**:
   ```bash
   echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc  # or ~/.zshrc
   source ~/.bashrc  # or ~/.zshrc
   ```

### "failed to compile" errors during installation
**Problem**: Rust compilation issues

**Solutions**:
1. **Update Rust**:
   ```bash
   rustup update
   ```

2. **Clear cargo cache**:
   ```bash
   cargo clean
   rm -rf ~/.cargo/registry/cache
   ```

3. **Install with verbose output**:
   ```bash
   cargo install --path yanshi-cli --verbose
   ```

## 🔑 API Key Issues

### "API key missing for provider 'step'"
**Problem**: Old error from previous CLI version

**Solutions**:
1. **Reinstall latest version**:
   ```bash
   cargo install --path yanshi-cli --force
   ```

2. **If error persists, check configuration**:
   ```bash
   yanshi config show
   ```

### "Incorrect API key provided" (401 Error)
**Problem**: Invalid or expired StepFun API key

**Solutions**:
1. **Verify API key format**:
   ```bash
   echo $STEP_API_KEY | wc -c  # Should be reasonable length
   ```

2. **Check for extra characters**:
   ```bash
   # Remove quotes and whitespace
   export STEP_API_KEY=$(echo "$STEP_API_KEY" | tr -d '"' | tr -d ' ')
   ```

3. **Regenerate API key**:
   - Log into StepFun dashboard
   - Generate new API key
   - Update environment variable

4. **Test with curl**:
   ```bash
   curl -H "Authorization: Bearer $STEP_API_KEY" \
        -H "Content-Type: application/json" \
        https://api.stepfun.com/v1/models
   ```

### "API key not found" (No environment variable)
**Problem**: STEP_API_KEY environment variable not set

**Solutions**:
1. **Set for current session**:
   ```bash
   export STEP_API_KEY="your-stepfun-api-key-here"
   ```

2. **Make persistent**:
   ```bash
   echo 'export STEP_API_KEY="your-key"' >> ~/.bashrc
   source ~/.bashrc
   ```

3. **Use .env file**:
   ```bash
   echo "STEP_API_KEY=your-key" > .env
   ```

## 🌐 Network and API Issues

### "Connection timeout" or "Network unreachable"
**Problem**: Network connectivity issues

**Solutions**:
1. **Check internet connection**:
   ```bash
   ping -c 3 api.stepfun.com
   ```

2. **Test with curl**:
   ```bash
   curl -I https://api.stepfun.com/v1/models
   ```

3. **Check proxy settings**:
   ```bash
   echo $HTTP_PROXY $HTTPS_PROXY
   ```

4. **Temporarily disable VPN/proxy** and retry

### "Rate limit exceeded" (429 Error)
**Problem**: Too many API requests in short time

**Solutions**:
1. **Wait and retry**:
   ```bash
   sleep 60  # Wait 1 minute
   yanshi [your-command]
   ```

2. **Reduce concurrent requests** in batch processing

3. **Check API quota** in StepFun dashboard

### "Service temporarily unavailable" (503 Error)
**Problem**: StepFun API service issues

**Solutions**:
1. **Check StepFun status** (official channels)
2. **Retry with exponential backoff**:
   ```bash
   sleep 5 && yanshi [command] || \
   sleep 15 && yanshi [command] || \
   sleep 45 && yanshi [command]
   ```

## 📁 File and Permission Issues

### "Permission denied" when creating output files
**Problem**: Insufficient permissions in output directory

**Solutions**:
1. **Check directory permissions**:
   ```bash
   ls -la $(dirname "your-output-file")
   ```

2. **Create directory with proper permissions**:
   ```bash
   mkdir -p output_directory
   chmod 755 output_directory
   ```

3. **Use absolute paths**:
   ```bash
   yanshi image generate "test" --output "/full/path/to/output.png"
   ```

### "No such file or directory" for input files
**Problem**: Input file path incorrect

**Solutions**:
1. **Verify file exists**:
   ```bash
   ls -la "your-input-file"
   ```

2. **Use absolute paths**:
   ```bash
   realpath "your-file.jpg"  # Get absolute path
   ```

3. **Check for spaces in paths**:
   ```bash
   yanshi image understand "path with spaces/image.jpg" "prompt"
   ```

### "File too large" errors
**Problem**: Input file exceeds size limits

**Solutions**:
1. **Check file size**:
   ```bash
   ls -lh your-file.jpg
   ```

2. **Compress images** before processing:
   ```bash
   # Using ImageMagick (if available)
   convert input.jpg -quality 85 -resize 2048x2048\> output.jpg
   ```

3. **Check API documentation** for file size limits

## 🎨 Image Generation Issues

`yanshi image generate` defaults to OpenAI's `gpt-image-2`, so it needs
`OPENAI_API_KEY` (StepFun retired its image-generation APIs on 2026-10-10).
List the other registered image models with
`yanshi llm models --type text_to_image`.

### "Image generation failed" with no specific error
**Problem**: Generic generation failure

**Solutions**:
1. **Check the key for the model's provider**:
   ```bash
   echo ${OPENAI_API_KEY:+set}  # gpt-image-2
   ```

2. **Simplify the prompt**:
   ```bash
   yanshi image generate "simple red circle" --output test.png
   ```

3. **Check prompt content** for terms the provider's safety filter may reject

### Generated images are low quality
**Problem**: Poor image quality results

**Solutions**:
1. **Use a larger size**:
   ```bash
   yanshi image generate "prompt" --size 1536x1024 --output large.png
   ```

2. **Improve prompt specificity**:
   ```bash
   yanshi image generate "detailed, high quality, professional photo of [subject]" --output detailed.png
   ```

3. **Try another model**: `--steps` / `--cfg-scale` only affect diffusion-style
   models; for `gpt-image-2` the prompt and size are the main levers.

### "Invalid size" errors
**Problem**: Unsupported image dimensions

**Solutions**:
1. **Use sizes the model supports** — for `gpt-image-2`:
   `1024x1024`, `1536x1024`, `1024x1536`

2. **Check the command's options**:
   ```bash
   yanshi image generate --help
   ```

## 🎵 Audio Processing Issues

### "Voice not supported" errors
**Problem**: Specified voice not available

**Solutions**:
1. **Use tested voice**:
   ```bash
   yanshi audio tts "text" --voice cixingnansheng --output audio.mp3
   ```

2. **Check available voices**:
   ```bash
   yanshi audio tts --help | grep -A 10 "voice"
   ```

3. **Try default voice**:
   ```bash
   yanshi audio tts "text" --output audio.mp3  # No --voice parameter
   ```

### Audio files not playing correctly
**Problem**: Format or encoding issues

**Solutions**:
1. **Try different format**:
   ```bash
   yanshi audio tts "text" --format wav --output audio.wav
   ```

2. **Check file integrity**:
   ```bash
   file audio.mp3  # Should show audio format
   ```

3. **Test with different player**:
   ```bash
   # Try different audio players if available
   mpg123 audio.mp3    # Linux
   afplay audio.mp3    # macOS
   ```

### ASR transcription is inaccurate
**Problem**: Poor speech recognition accuracy

**Solutions**:
1. **Use high-quality audio**:
   - Clear speech, minimal background noise
   - Use WAV format when possible

2. **Specify language explicitly**:
   ```bash
   yanshi audio asr recording.wav --language en --output transcript.txt
   ```

3. **Try different formats**:
   ```bash
   yanshi audio asr audio.wav --format json --output detailed.json
   ```

## 🧪 Testing and Validation Issues

### Tests failing during validation
**Problem**: Test scripts report failures

**Solutions**:
1. **Run individual tests**:
   ```bash
   ./tests/test_cli_structure.sh      # Test CLI structure only
   ./tests/quick_validation.sh        # Test basic functionality
   ```

2. **Check specific error messages** in test output

3. **Verify API key and connectivity** before running tests

4. **Run with verbose output**:
   ```bash
   VERBOSE=1 ./tests/test_all_commands.sh
   ```

### Tutorial scripts fail to run
**Problem**: Tutorial execution errors

**Solutions**:
1. **Check script permissions**:
   ```bash
   chmod +x tutorials/*.sh
   ```

2. **Run with bash explicitly**:
   ```bash
   bash tutorials/01_quick_start.sh
   ```

3. **Check prerequisites**:
   ```bash
   # Ensure API key is set
   echo $STEP_API_KEY
   # Ensure yanshi is installed
   which yanshi
   ```

## 🔧 Command-Specific Issues

### "Unknown command" errors
**Problem**: Command not recognized

**Solutions**:
1. **Check command syntax**:
   ```bash
   yanshi --help                 # See all commands
   yanshi image --help           # See image subcommands
   ```

2. **Use correct command structure**:
   ```bash
   yanshi [category] [action] [arguments]
   # Example: yanshi image generate "prompt" --output file.png
   ```

3. **Check for typos**:
   ```bash
   yanshi image generate  # Not: yanshi images generate
   ```

### Help system not working
**Problem**: --help flag not showing information

**Solutions**:
1. **Reinstall CLI**:
   ```bash
   cargo install --path yanshi-cli --force
   ```

2. **Try different help formats**:
   ```bash
   yanshi help
   yanshi -h
   yanshi --help
   ```

## 🧹 Cache and State Issues

### "Inconsistent state" errors
**Problem**: CLI internal state issues

**Solutions**:
1. **Clear any cache directories**:
   ```bash
   rm -rf ~/.yanshi/cache  # If exists
   ```

2. **Reinstall CLI**:
   ```bash
   cargo install --path yanshi-cli --force
   ```

3. **Reset configuration**:
   ```bash
   yanshi config init --force
   ```

## 📊 Performance Issues

### Commands running very slowly
**Problem**: Poor performance

**Solutions**:
1. **Check internet connection speed**
2. **Use smaller parameters for testing**:
   ```bash
   yanshi image generate "test" --output /tmp/test.png
   ```

3. **Monitor system resources** (CPU, memory, disk)

### Out of disk space errors
**Problem**: Insufficient storage

**Solutions**:
1. **Check available space**:
   ```bash
   df -h .
   ```

2. **Clean up old outputs**:
   ```bash
   rm -rf test_outputs/ tutorial_*_outputs/
   ```

3. **Use smaller image sizes** and shorter audio files

## 🔍 Advanced Debugging

### Enable verbose logging
```bash
# Set log level to debug
yanshi --log-level debug [your-command]

# Or export environment variable
export RUST_LOG=debug
yanshi [your-command]
```

### Capture full error output
```bash
yanshi [your-command] 2>&1 | tee error.log
```

### Test minimal functionality
```bash
# Test basic CLI structure
yanshi --version

# Test help system
yanshi --help

# Test API connectivity (minimal)
yanshi image generate "test" --output /tmp/test.png
```

## 📞 Getting Additional Help

### Documentation Resources
- **Getting Started**: [`documentation/GETTING_STARTED.md`](GETTING_STARTED.md)
- **Commands Reference**: [`documentation/COMMANDS_REFERENCE.md`](COMMANDS_REFERENCE.md)
- **Migration Guide**: [`documentation/MIGRATION_GUIDE.md`](MIGRATION_GUIDE.md)

### Validation Tools
- **Quick Check**: `./tests/quick_validation.sh`
- **Full Test Suite**: `./tests/test_all_commands.sh`
- **CLI Structure**: `./tests/test_cli_structure.sh`

### Community Resources
- Check Yanshi project documentation
- Review StepFun API documentation
- Search existing issues in project repository

---

## 🎯 Issue Resolution Checklist

When encountering any issue:

1. ✅ **Check API key** - Is `STEP_API_KEY` set correctly?
2. ✅ **Verify installation** - Does `yanshi --version` work?
3. ✅ **Test connectivity** - Can you reach `api.stepfun.com`?
4. ✅ **Check file paths** - Do input files exist and output directories have permissions?
5. ✅ **Simplify command** - Does a minimal version work?
6. ✅ **Review error message** - What specific error is reported?
7. ✅ **Check this guide** - Is there a specific solution above?
8. ✅ **Run validation** - Do the test scripts pass?

Most issues fall into one of these categories and have straightforward solutions. When in doubt, start with the validation scripts to isolate the problem area.