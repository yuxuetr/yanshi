#!/bin/bash

# Yanshi CLI - Before vs After Comparison Demo
# Shows the improvement in user experience

echo "🔄 Yanshi CLI: Before vs After Comparison"
echo "============================================="
echo

echo "❌ BEFORE: Complex, Inconsistent Usage"
echo "--------------------------------------"
echo
echo "1. Image Understanding (inconsistent with other commands):"
echo "   yanshi llm prompt 'Describe this image' --file image.jpg --model step-1o-turbo-vision"
echo
echo "2. Text-to-Speech (shell script, limited options):"
echo "   ./stepfun_tts_cli.sh"
echo
echo "3. Speech Recognition (shell script, limited options):"
echo "   ./stepfun_asr_cli.sh"
echo

echo "✅ AFTER: Unified, Discoverable, Consistent"
echo "-------------------------------------------"
echo
echo "1. Image Understanding (dedicated, clear command):"
echo "   yanshi image understand image.jpg 'Describe this image in detail' --model step-1o-turbo-vision"
echo
echo "2. Text-to-Speech (full CLI integration):"
echo "   yanshi audio tts 'Hello world' --voice default --output hello.mp3"
echo
echo "3. Speech Recognition (comprehensive options):"
echo "   yanshi audio asr recording.wav --format json --output transcript.json"
echo
echo "4. Image Generation (defaults to OpenAI gpt-image-2):"
echo "   yanshi image generate 'A beautiful sunset' --output sunset.png"
echo

echo "🎯 Key Improvements"
echo "==================="
echo "✅ Unified Discovery: 'yanshi --help' shows everything"
echo "✅ Consistent Interface: Same patterns across all commands"
echo "✅ No Compilation: No need for separate Rust binaries"
echo "✅ Rich Help System: Detailed help for every command"
echo "✅ Command Aliases: Short forms (gen, analyze, tts, asr)"
echo "✅ Better Parameters: Comprehensive options for each command"
echo "✅ Error Handling: Clear, actionable error messages"
echo "✅ File I/O: Consistent input/output file handling"
echo

echo "🚀 Try It Yourself!"
echo "===================="
echo
echo "Set your API keys:"
echo "  export STEP_API_KEY='your-stepfun-api-key'      # audio + StepFun models"
echo "  export OPENAI_API_KEY='your-openai-api-key'     # image generation (gpt-image-2)"
echo
echo "Discover commands:"
echo "  yanshi --help"
echo "  yanshi image --help"
echo "  yanshi audio --help"
echo
echo "Generate an image:"
echo "  yanshi image generate 'A cyberpunk cityscape' --size 1024x1024 --output city.png"
echo
echo "Analyze an image:"
echo "  yanshi image understand city.png 'What architectural style is this?'"
echo
echo "Create speech:"
echo "  yanshi audio tts 'Yanshi makes AI workflows simple!' --output demo.mp3"
echo
echo "🎉 Modern CLI Experience Achieved!"