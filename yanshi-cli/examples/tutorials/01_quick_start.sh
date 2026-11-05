#!/bin/bash
# Yanshi CLI Quick Start Tutorial
# Learn the basics with hands-on examples

set -e

# Color output for better learning experience
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Configuration
TUTORIAL_DIR="./tutorial_01_outputs"
PAUSE_BETWEEN_STEPS=3

echo -e "${BOLD}${BLUE}🚀 Yanshi CLI Quick Start Tutorial${NC}"
echo "======================================"
echo ""
echo -e "${CYAN}This tutorial will teach you the basics of Yanshi CLI through${NC}"
echo -e "${CYAN}hands-on examples. Each step builds on the previous one.${NC}"
echo ""

# Create output directory
mkdir -p "$TUTORIAL_DIR"
echo -e "${YELLOW}📁 Tutorial outputs will be saved to: $TUTORIAL_DIR${NC}"
echo ""

# Helper functions
tutorial_step() {
    local step_num="$1"
    local title="$2"
    echo ""
    echo -e "${BOLD}${BLUE}Step $step_num: $title${NC}"
    echo "$(printf '=%.0s' {1..50})"
}

run_command() {
    local description="$1"
    local command="$2"
    local expected_output="$3"
    
    echo -e "${CYAN}💡 $description${NC}"
    echo -e "${YELLOW}Command:${NC} $command"
    echo ""
    
    # Show the command being executed
    echo -e "${GREEN}Executing...${NC}"
    if eval "$command"; then
        if [[ -n "$expected_output" && -f "$expected_output" ]]; then
            local size=$(stat -f%z "$expected_output" 2>/dev/null || stat -c%s "$expected_output" 2>/dev/null || echo "unknown")
            echo -e "${GREEN}✅ Success! Output saved to: $expected_output (${size} bytes)${NC}"
        else
            echo -e "${GREEN}✅ Success!${NC}"
        fi
    else
        echo -e "${RED}❌ Command failed. Check your setup and try again.${NC}"
        exit 1
    fi
    
    echo ""
    sleep $PAUSE_BETWEEN_STEPS
}

explain() {
    echo -e "${CYAN}📝 $1${NC}"
    echo ""
}

wait_for_user() {
    echo -e "${YELLOW}Press Enter to continue to the next step...${NC}"
    read -r
}

# Pre-flight check
echo "🔍 Pre-flight Check"
echo "==================="

if [[ -z "$STEP_API_KEY" ]]; then
    echo -e "${RED}❌ STEP_API_KEY not set${NC}"
    echo ""
    echo "Please set your StepFun API key first:"
    echo -e "${YELLOW}export STEP_API_KEY=\"your-stepfun-api-key-here\"${NC}"
    echo ""
    echo "Get your API key at: https://www.stepfun.com/"
    exit 1
fi

if [[ -z "$OPENAI_API_KEY" ]]; then
    echo -e "${RED}❌ OPENAI_API_KEY not set${NC}"
    echo ""
    echo "Image generation defaults to OpenAI's gpt-image-2:"
    echo -e "${YELLOW}export OPENAI_API_KEY=\"your-openai-api-key-here\"${NC}"
    exit 1
fi

if ! command -v yanshi &> /dev/null; then
    echo -e "${RED}❌ Yanshi CLI not found${NC}"
    echo ""
    echo "Please install Yanshi CLI first:"
    echo -e "${YELLOW}cargo install --path yanshi-cli${NC}"
    exit 1
fi

echo -e "${GREEN}✅ Setup verified! Let's begin learning.${NC}"

# Tutorial starts here
tutorial_step "1" "Discover Available Commands"

explain "Yanshi CLI organizes functionality into logical groups. Let's explore what's available:"

run_command \
    "Show all available command categories" \
    "yanshi --help | head -20"

explain "Notice the main categories: image, audio, llm, and config. Each has its own subcommands."

run_command \
    "Explore image commands in detail" \
    "yanshi image --help"

run_command \
    "Explore audio commands in detail" \
    "yanshi audio --help"

wait_for_user

tutorial_step "2" "Generate Your First Image"

explain "Let's create an image from text using AI. This demonstrates the 'text-to-image' capability."

run_command \
    "Generate a simple test image" \
    "yanshi image generate 'A cheerful robot waving hello, cartoon style' --output '$TUTORIAL_DIR/hello_robot.png'" \
    "$TUTORIAL_DIR/hello_robot.png"

explain "The image generation process typically takes 15-30 seconds. The AI model interprets your text and creates a corresponding image."

run_command \
    "Generate a more complex image with specific parameters" \
    "yanshi image generate 'A serene Japanese garden with cherry blossoms, soft lighting, peaceful atmosphere' --model gpt-image-2 --size 1536x1024 --output '$TUTORIAL_DIR/zen_garden.png'" \
    "$TUTORIAL_DIR/zen_garden.png"

explain "Notice the additional parameters:"
echo "  • --model: Specifies which AI model to use (default: gpt-image-2)"
echo "  • --size: Output dimensions supported by the model (1536x1024 = landscape)"

wait_for_user

tutorial_step "3" "Understand Images with AI"

explain "Now let's analyze the images we just created using AI vision capabilities."

run_command \
    "Analyze the robot image" \
    "yanshi image understand '$TUTORIAL_DIR/hello_robot.png' 'What do you see in this image? Describe the style and mood.' --output '$TUTORIAL_DIR/robot_analysis.txt'" \
    "$TUTORIAL_DIR/robot_analysis.txt"

explain "Let's read what the AI saw in our image:"
if [[ -f "$TUTORIAL_DIR/robot_analysis.txt" ]]; then
    echo -e "${CYAN}AI Analysis:${NC}"
    echo "-------------------"
    cat "$TUTORIAL_DIR/robot_analysis.txt" | head -10
    echo ""
fi

run_command \
    "Perform detailed artistic analysis of the garden image" \
    "yanshi image understand '$TUTORIAL_DIR/zen_garden.png' 'Analyze this image focusing on: composition, color palette, artistic style, mood, and symbolic elements. What emotions does it evoke?' --temperature 0.8 --max-tokens 500 --output '$TUTORIAL_DIR/garden_analysis.txt'" \
    "$TUTORIAL_DIR/garden_analysis.txt"

wait_for_user

tutorial_step "4" "Text-to-Speech Conversion"

explain "Convert text into natural-sounding speech using AI voice synthesis."

run_command \
    "Create a welcome message" \
    "yanshi audio tts 'Welcome to Yanshi CLI! This tutorial will teach you how to use AI for creative projects.' --voice cixingnansheng --output '$TUTORIAL_DIR/welcome.mp3'" \
    "$TUTORIAL_DIR/welcome.mp3"

run_command \
    "Create speech with different parameters" \
    "yanshi audio tts 'The zen garden represents tranquility and mindfulness in Japanese culture.' --voice cixingnansheng --speed 0.8 --format mp3 --output '$TUTORIAL_DIR/zen_explanation.mp3'" \
    "$TUTORIAL_DIR/zen_explanation.mp3"

explain "The generated audio files can be played with any standard audio player."

wait_for_user

tutorial_step "5" "Speech-to-Text Recognition"

explain "Convert the audio we just generated back to text, demonstrating the full audio processing cycle."

run_command \
    "Transcribe the welcome message" \
    "yanshi audio asr '$TUTORIAL_DIR/welcome.mp3' --output '$TUTORIAL_DIR/welcome_transcript.txt'" \
    "$TUTORIAL_DIR/welcome_transcript.txt"

if [[ -f "$TUTORIAL_DIR/welcome_transcript.txt" ]]; then
    echo -e "${CYAN}Transcription Result:${NC}"
    echo "----------------------"
    cat "$TUTORIAL_DIR/welcome_transcript.txt"
    echo ""
fi

run_command \
    "Generate structured JSON transcript" \
    "yanshi audio asr '$TUTORIAL_DIR/zen_explanation.mp3' --format json --output '$TUTORIAL_DIR/zen_transcript.json'" \
    "$TUTORIAL_DIR/zen_transcript.json"

explain "JSON format provides additional metadata about the transcription process."

wait_for_user

tutorial_step "6" "Command Aliases and Shortcuts"

explain "Yanshi provides convenient aliases for common commands to speed up your workflow."

run_command \
    "Use 'gen' alias for image generation" \
    "yanshi image gen 'A laptop with code on screen, programmer workspace' --size 512x512 --output '$TUTORIAL_DIR/coding_laptop.png'" \
    "$TUTORIAL_DIR/coding_laptop.png"

run_command \
    "Use 'analyze' alias for image understanding" \
    "yanshi image analyze '$TUTORIAL_DIR/coding_laptop.png' 'What programming language is shown on the screen?'"

explain "Common aliases you can use:"
echo "  • 'gen' instead of 'generate'"
echo "  • 'analyze' instead of 'understand'"
echo "  • 'tts' instead of 'text-to-speech'"
echo "  • 'asr' instead of 'speech-to-text'"

wait_for_user

tutorial_step "7" "Error Handling and Help"

explain "Yanshi provides helpful error messages and comprehensive help system."

echo -e "${CYAN}💡 Demonstrating error handling with an invalid file path${NC}"
echo -e "${YELLOW}Command:${NC} yanshi image generate 'test' --output '/invalid/path/test.png'"
echo ""
if yanshi image generate "test" --output "/invalid/path/test.png" 2>&1 | head -5; then
    echo ""
else
    echo -e "${GREEN}✅ Error handling works correctly!${NC}"
fi

run_command \
    "Get help for any specific command" \
    "yanshi image generate --help | head -15"

explain "Every command has detailed help available with the --help flag."

wait_for_user

tutorial_step "8" "Putting It All Together - Complete Workflow"

explain "Let's create a complete creative workflow that combines multiple AI capabilities."

run_command \
    "Create a landscape image" \
    "yanshi image generate 'A majestic mountain landscape with a crystal-clear lake reflecting snow-capped peaks, golden hour lighting' --size 1024x1024 --output '$TUTORIAL_DIR/final_landscape.png'" \
    "$TUTORIAL_DIR/final_landscape.png"

run_command \
    "Analyze the landscape for storytelling elements" \
    "yanshi image understand '$TUTORIAL_DIR/final_landscape.png' 'Write a short, poetic description of this landscape that could be used as a story opening. Focus on the atmosphere and emotions it evokes.' --temperature 0.9 --output '$TUTORIAL_DIR/story_opening.txt'" \
    "$TUTORIAL_DIR/story_opening.txt"

if [[ -f "$TUTORIAL_DIR/story_opening.txt" ]]; then
    STORY_TEXT=$(cat "$TUTORIAL_DIR/story_opening.txt")
    run_command \
        "Convert the story description to speech" \
        "yanshi audio tts '$STORY_TEXT' --voice cixingnansheng --speed 0.7 --output '$TUTORIAL_DIR/story_narration.mp3'" \
        "$TUTORIAL_DIR/story_narration.mp3"
fi

explain "You've now created a complete multimedia story:"
echo "  🎨 Visual: AI-generated landscape"
echo "  📝 Text: AI-written story opening"
echo "  🎵 Audio: AI-synthesized narration"

# Tutorial completion
echo ""
echo -e "${BOLD}${GREEN}🎉 Congratulations! Tutorial Complete!${NC}"
echo "======================================="
echo ""
echo "You've successfully learned how to:"
echo -e "${GREEN}✅ Generate images from text descriptions${NC}"
echo -e "${GREEN}✅ Analyze images with AI vision${NC}"
echo -e "${GREEN}✅ Convert text to natural speech${NC}"
echo -e "${GREEN}✅ Transcribe audio to text${NC}"
echo -e "${GREEN}✅ Use command aliases for efficiency${NC}"
echo -e "${GREEN}✅ Handle errors gracefully${NC}"
echo -e "${GREEN}✅ Create complete AI-powered workflows${NC}"
echo ""
echo "📁 Your tutorial outputs are saved in: $TUTORIAL_DIR"
echo ""
echo -e "${CYAN}🚀 Next Steps:${NC}"
echo "  • Try the audio processing tutorial: ./tutorials/03_audio_workflows.sh"
echo "  • Read the commands reference: ./documentation/COMMANDS_REFERENCE.md"
echo "  • Run comprehensive tests: ./tests/test_all_commands.sh"
echo ""
echo -e "${YELLOW}💡 Pro Tips:${NC}"
echo "  • Use yanshi [command] --help for detailed parameter information"
echo "  • Experiment with different models, sizes, and parameters"
echo "  • Save your successful parameter combinations for reuse"
echo "  • Use descriptive prompts for better AI results"
echo ""
echo -e "${BOLD}Happy creating with Yanshi CLI! 🎨🤖${NC}"