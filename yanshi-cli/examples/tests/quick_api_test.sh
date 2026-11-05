#!/bin/bash

# Quick API Test - Tests Yanshi CLI with better error handling
# Focuses on minimal viable functionality

set -e

echo "🧪 Yanshi CLI Quick API Test"
echo "==============================="
echo

# Check prerequisites
if [[ -z "$STEP_API_KEY" ]]; then
    echo "❌ STEP_API_KEY not set"
    echo "   Please set your API key: export STEP_API_KEY='your-key'"
    exit 1
fi

if [[ -z "$OPENAI_API_KEY" ]]; then
    echo "❌ OPENAI_API_KEY not set (image generation defaults to gpt-image-2)"
    echo "   Please set your API key: export OPENAI_API_KEY='your-key'"
    exit 1
fi

if ! command -v yanshi &> /dev/null; then
    echo "❌ yanshi command not found"
    echo "   Please install: cargo install --path yanshi-cli"
    exit 1
fi

echo "🔑 API Key configured: ${STEP_API_KEY:0:8}...${STEP_API_KEY: -4}"
echo

# Create test directory
mkdir -p api_test_output
cd api_test_output

echo "📁 Working in: $(pwd)"
echo

# Test 1: Image Generation with robust error handling
echo "🎨 Test 1: Image Generation"
echo "----------------------------"
echo "Command: yanshi image generate 'A serene mountain landscape at sunset' --output mountain.png"

# Add timeout and better error handling
if timeout 120s yanshi image generate "A simple red circle on white background" \
    --model gpt-image-2 \
    --size 1024x1024 \
    --output circle.png 2>&1 | tee generation.log; then
    
    if [[ -f "circle.png" && -s "circle.png" ]]; then
        file_size=$(stat -f%z "circle.png" 2>/dev/null || stat -c%s "circle.png" 2>/dev/null)
        echo "✅ SUCCESS: Image generated (${file_size} bytes)"
    else
        echo "❌ Image generation failed - no output file created"
        echo "Error log:"
        cat generation.log
        exit 1
    fi
else
    echo "❌ Image generation failed or timed out"
    echo "Error details:"
    cat generation.log 2>/dev/null || echo "No error log available"
    
    # Check for specific error patterns
    if grep -q "401" generation.log 2>/dev/null; then
        echo "💡 Suggestion: API key may be invalid - gpt-image-2 needs a valid OPENAI_API_KEY"
    elif grep -q "timeout\|connection" generation.log 2>/dev/null; then
        echo "💡 Suggestion: Network issue - check internet connection"
    elif grep -q "rate.limit\|quota" generation.log 2>/dev/null; then
        echo "💡 Suggestion: API quota exceeded - wait or check billing"
    fi
    exit 1
fi
echo

# Test 2: Image Understanding (if we have an image from test 1)
if [[ -f "circle.png" && -s "circle.png" ]]; then
    echo "👁️  Test 2: Image Understanding" 
    echo "-------------------------------"
    echo "Command: yanshi image understand circle.png 'What do you see?'"
    echo
    
    if timeout 60s yanshi image understand "circle.png" \
        "What do you see in this image? Describe it briefly in one sentence." \
        --model step-1o-turbo-vision \
        --max-tokens 100 \
        --temperature 0.7 \
        --output image_analysis.txt 2>&1 | tee understanding.log; then
        
        if [[ -f "image_analysis.txt" && -s "image_analysis.txt" ]]; then
            echo "✅ SUCCESS: Image understanding works"
            echo "   Analysis: $(cat image_analysis.txt | head -n 2)"
        else
            echo "❌ Image understanding failed - no analysis generated"
        fi
    else
        echo "❌ Image understanding failed or timed out"
        echo "Error details:"
        cat understanding.log 2>/dev/null || echo "No error log available"
    fi
    echo
else
    echo "👁️  Test 2: Image Understanding"
    echo "-------------------------------"
    echo "ℹ️  Skipping - no image from previous test"
    echo "   Using sample image from assets directory instead..."
    
    # Try to use a sample image from the assets directory
    SAMPLE_IMAGE="../assets/sample_images/nature_landscape.jpg"
    if [[ -f "$SAMPLE_IMAGE" ]]; then
        echo "   Found sample: $SAMPLE_IMAGE"
        if timeout 60s yanshi image understand "$SAMPLE_IMAGE" \
            "What do you see in this landscape image?" \
            --output sample_analysis.txt 2>&1 | tee understanding.log; then
            
            if [[ -f "sample_analysis.txt" && -s "sample_analysis.txt" ]]; then
                echo "✅ SUCCESS: Image understanding works with sample image"
                echo "   Analysis: $(cat sample_analysis.txt | head -n 2)"
            fi
        else
            echo "❌ Image understanding failed with sample image"
        fi
    else
        echo "ℹ️  No sample images available - skipping image understanding test"
        echo "   Place test images in ../assets/sample_images/ for testing"
    fi
    echo
fi

# Test 3: Audio TTS (quick test)
echo "🎵 Test 3: Text-to-Speech"
echo "-------------------------"
echo "Command: yanshi audio tts 'Hello world' --output hello.mp3"
echo

if timeout 60s yanshi audio tts "Hello from Yanshi test!" \
    --voice cixingnansheng \
    --format mp3 \
    --output hello.mp3 2>&1 | tee tts.log; then
    
    if [[ -f "hello.mp3" && -s "hello.mp3" ]]; then
        file_size=$(stat -f%z "hello.mp3" 2>/dev/null || stat -c%s "hello.mp3" 2>/dev/null)
        echo "✅ SUCCESS: TTS generated audio (${file_size} bytes)"
        
        # Test 4: ASR (if we have audio)
        echo ""
        echo "🎤 Test 4: Speech Recognition"
        echo "-----------------------------" 
        echo "Command: yanshi audio asr hello.mp3 --output transcript.txt"
        echo
        
        if timeout 30s yanshi audio asr "hello.mp3" \
            --format text \
            --output transcript.txt 2>&1 | tee asr.log; then
            
            if [[ -f "transcript.txt" && -s "transcript.txt" ]]; then
                echo "✅ SUCCESS: ASR transcribed audio"
                echo "   Transcript: \"$(cat transcript.txt)\""
            else
                echo "❌ ASR failed - no transcript generated"
            fi
        else
            echo "❌ ASR failed or timed out"
        fi
    else
        echo "❌ TTS failed - no audio file created"
        cat tts.log 2>/dev/null || echo "No TTS error log"
    fi
else
    echo "❌ TTS failed or timed out"
    cat tts.log 2>/dev/null || echo "No TTS error log"
fi

# Summary
echo "📊 Test Summary"
echo "==============="
echo "Generated files:"
ls -la *.png *.txt *.mp3 *.json 2>/dev/null || echo "No files generated"
echo
echo "🔍 Next Steps:"
if [ "$STEP_API_KEY" = "your-actual-stepfun-api-key" ]; then
    echo "   1. ⚠️  Set a real API key: export STEP_API_KEY='sk-xxx'"
    echo "   2. 🔄 Re-run this test script"
    echo "   3. ✅ Try the full test: ./test_new_commands.sh"
else
    echo "   1. ✅ API key is configured"
    echo "   2. 🧪 Try more commands manually"
    echo "   3. 📚 Check examples in other scripts"
fi

cd ..