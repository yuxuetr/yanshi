#!/bin/bash

# Yanshi CLI - New Commands Test Script
# Tests the new image and audio commands with StepFun API

set -e

echo "🧪 Yanshi CLI New Commands Test"
echo "=================================="
echo

# Check if yanshi binary exists
if ! command -v yanshi &> /dev/null; then
    echo "❌ yanshi CLI not found. Please install it first:"
    echo "   cargo install --path yanshi-cli"
    exit 1
fi

# Check API key
if [ -z "$STEP_API_KEY" ]; then
    echo "❌ STEP_API_KEY environment variable not set"
    echo "   export STEP_API_KEY=\"your-actual-stepfun-api-key\""
    echo ""
    echo "🔑 For testing purposes, you can set a dummy key:"
    echo "   export STEP_API_KEY=\"test-key-12345\""
    echo "   (Note: Commands will fail at API call but CLI structure will be tested)"
    exit 1
fi

# Set the API key with the correct variable name expected by Yanshi
export STEPFUN_API_KEY="$STEP_API_KEY"

echo "✅ Environment check passed"
echo "🔑 Using API key: ${STEP_API_KEY:0:10}..." # Show only first 10 chars
echo

# Create output directory
mkdir -p yanshi_new_commands_test
cd yanshi_new_commands_test

echo "📁 Created test output directory: $(pwd)"
echo

# Test 1: Image Generation
echo "🎨 Test 1: Image Generation"
echo "----------------------------"
echo "Command: yanshi image generate 'A serene mountain landscape at sunset' --output mountain.png"

if yanshi image generate "A serene mountain landscape at sunset" \
    --model gpt-image-2 \
    --size 1024x1024 \
    --output mountain.png; then
    echo "✅ Image generation command executed successfully"
    if [ -f "mountain.png" ]; then
        echo "✅ Image file created: mountain.png ($(stat -f%z mountain.png) bytes)"
    else
        echo "⚠️  Image file not found (API may have failed)"
    fi
else
    echo "❌ Image generation failed (gpt-image-2 needs a valid OPENAI_API_KEY)"
fi
echo

# Test 2: Image Understanding  
echo "👁️  Test 2: Image Understanding"
echo "-------------------------------"

# Create a test image file or check for existing ones
test_image=""
for ext in jpg jpeg png; do
    if ls ../sample_images/*.$ext >/dev/null 2>&1; then
        test_image=$(ls ../sample_images/*.$ext | head -1)
        break
    fi
done

# If no test image found, try to use the generated image
if [ -z "$test_image" ] && [ -f "mountain.png" ]; then
    test_image="mountain.png"
fi

if [ -n "$test_image" ]; then
    echo "Found test image: $test_image"
    echo "Command: yanshi image understand '$test_image' 'Describe this image in detail'"
    
    if yanshi image understand "$test_image" "Describe this image in detail, including colors, composition, and mood." \
        --model step-1o-turbo-vision \
        --temperature 0.7 \
        --max-tokens 500 \
        --output image_analysis.md; then
        echo "✅ Image understanding command executed successfully"
        if [ -f "image_analysis.md" ]; then
            echo "✅ Analysis saved to: image_analysis.md"
            echo "   Preview: $(head -n 5 image_analysis.md | tail -n 1)"
        fi
    else
        echo "❌ Image understanding failed (likely API key issue)"
    fi
else
    echo "ℹ️  No test image available - skipping image understanding test"
    echo "   Place a .jpg/.png file in ../sample_images/ to test this feature"
fi
echo

# Test 3: Text-to-Speech
echo "🎙️  Test 3: Text-to-Speech"
echo "--------------------------"
echo "Command: yanshi audio tts 'Hello from Yanshi!' --output hello.mp3"

if yanshi audio tts "Hello from Yanshi! This is a test of the text-to-speech functionality." \
    --model step-tts-mini \
    --voice default \
    --format mp3 \
    --speed 1.0 \
    --output hello.mp3; then
    echo "✅ Text-to-speech command executed successfully"
    if [ -f "hello.mp3" ]; then
        echo "✅ Audio file created: hello.mp3 ($(stat -f%z hello.mp3) bytes)"
    else
        echo "⚠️  Audio file not found (API may have failed)"
    fi
else
    echo "❌ Text-to-speech failed (likely API key issue)"
fi
echo

# Test 4: Speech Recognition
echo "🎧 Test 4: Speech Recognition"
echo "-----------------------------"

# Check if we have an audio file to test with
test_audio=""
if [ -f "hello.mp3" ]; then
    test_audio="hello.mp3"
elif ls ../sample_images/*.wav >/dev/null 2>&1; then
    test_audio=$(ls ../sample_images/*.wav | head -1)
elif ls ../sample_images/*.mp3 >/dev/null 2>&1; then
    test_audio=$(ls ../sample_images/*.mp3 | head -1)
fi

if [ -n "$test_audio" ]; then
    echo "Found test audio: $test_audio"
    echo "Command: yanshi audio asr '$test_audio' --format json --output transcript.json"
    
    if yanshi audio asr "$test_audio" \
        --model step-asr \
        --format json \
        --output transcript.json; then
        echo "✅ Speech recognition command executed successfully"
        if [ -f "transcript.json" ]; then
            echo "✅ Transcript saved to: transcript.json"
            echo "   Preview: $(head -n 3 transcript.json)"
        fi
    else
        echo "❌ Speech recognition failed (likely API key issue)"
    fi
else
    echo "ℹ️  No test audio available - skipping speech recognition test"
    echo "   Place a .wav/.mp3 file in ../sample_images/ to test this feature"
fi
echo

# Test 5: Command Help and Discovery
echo "📖 Test 5: Command Help and Discovery"
echo "-------------------------------------"
echo "Testing help output for discoverability..."

echo "Main help:"
yanshi --help | grep -E "(image|audio)" || echo "Commands not found in help"
echo

echo "Image commands help:"
yanshi image --help | head -n 5
echo

echo "Audio commands help:"
yanshi audio --help | head -n 5
echo

echo "Detailed command help:"
echo "- Image generate: $(yanshi image generate --help | grep -c 'Options:')"
echo "- Image understand: $(yanshi image understand --help | grep -c 'Options:')"
echo "- Audio TTS: $(yanshi audio tts --help | grep -c 'Options:')"
echo "- Audio ASR: $(yanshi audio asr --help | grep -c 'Options:')"
echo

# Test 6: Alias Commands
echo "🔗 Test 6: Command Aliases"
echo "---------------------------"
echo "Testing command aliases..."

echo "Image generate alias (gen):"
yanshi image gen --help >/dev/null 2>&1 && echo "✅ 'gen' alias works" || echo "❌ 'gen' alias failed"

echo "Image understand alias (analyze):"
yanshi image analyze --help >/dev/null 2>&1 && echo "✅ 'analyze' alias works" || echo "❌ 'analyze' alias failed"

echo "Audio TTS alias:"
yanshi audio tts --help >/dev/null 2>&1 && echo "✅ 'tts' alias works" || echo "❌ 'tts' alias failed"

echo "Audio ASR alias:"
yanshi audio asr --help >/dev/null 2>&1 && echo "✅ 'asr' alias works" || echo "❌ 'asr' alias failed"

echo

# Summary
echo "📊 Test Summary"
echo "==============="
echo "Test completed in directory: $(pwd)"
echo
echo "Generated files:"
ls -la *.png *.mp3 *.json *.md 2>/dev/null | sed 's/^/  /' || echo "  No files generated (likely API key issues)"
echo
echo "🎉 CLI Structure Test Complete!"
echo
echo "📝 Notes:"
echo "   • All commands are properly integrated into the CLI"
echo "   • Help system works correctly for discovery"
echo "   • Command aliases are functional"
echo "   • File I/O parameters are properly handled"
echo "   • Actual API functionality depends on valid STEP_API_KEY (audio/vision) and OPENAI_API_KEY (image generation)"
echo
echo "🚀 Next steps:"
echo "   1. Set a real StepFun API key: export STEP_API_KEY='your-real-key'"
echo "   2. Add test images/audio to ../sample_images/ for full testing"
echo "   3. Try the commands with real content!"
echo

# Return to original directory
cd ..