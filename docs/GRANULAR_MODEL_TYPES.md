# Granular Model Type System

Yanshi now supports a granular model type classification system that provides specific input/output type requirements for different AI model capabilities. This system enables automatic validation, better error handling, and clearer model selection.

## Overview

The granular type system replaces broad categories like "multimodal" with specific classifications like "imageunderstand" and "text2image", making it clear what each model expects as input and produces as output.

## Model Type Classifications

### Text-Based Models

#### `text`
- **Description**: Text-based language model
- **Input**: Text
- **Output**: Text  
- **Use Cases**: Conversation, Q&A, text generation, summarization
- **Supports Streaming**: ✅
- **Supports Tools**: ✅
- **Example**: `step-1-8k`, `gpt-4o`, `claude-sonnet-5`

#### `codegen`
- **Description**: Code generation and completion  
- **Input**: Text (code prompts)
- **Output**: Text (code)
- **Use Cases**: Code completion, bug fixing, code explanation
- **Supports Streaming**: ✅
- **Supports Tools**: ✅
- **Example**: `step-code-1-8k` (hypothetical)

#### `functioncalling`
- **Description**: Function calling and tool usage
- **Input**: Text + function schemas
- **Output**: Function calls
- **Use Cases**: API integration, tool usage, workflow automation
- **Supports Streaming**: ✅
- **Supports Tools**: ✅ (primary purpose)
- **Example**: Models configured specifically for function calling

### Vision/Image Models

#### `imageunderstand`
- **Description**: Image understanding and analysis
- **Input**: Text + Image (base64 or URL)
- **Output**: Text
- **Use Cases**: Image description, visual Q&A, object detection, scene analysis
- **Supports Streaming**: ✅
- **Supports Tools**: ✅
- **Example**: `step-1o-turbo-vision`, `gpt-4o-vision`

#### `text2image`
- **Description**: Text-to-image generation
- **Input**: Text
- **Output**: Image (base64)
- **Use Cases**: Art generation, concept visualization, design mockups
- **Supports Streaming**: ❌ (generates complete images)
- **Supports Tools**: ❌
- **Example**: `dall-e-3`, `stable-diffusion` (hypothetical)

#### `image2image`
- **Description**: Image-to-image transformation
- **Input**: Image (base64)
- **Output**: Image (base64)
- **Use Cases**: Style transfer, image enhancement, format conversion
- **Supports Streaming**: ❌
- **Supports Tools**: ❌
- **Example**: Specialized image transformation models

#### `imageedit`
- **Description**: Image editing with text instructions
- **Input**: Image (base64) + Text
- **Output**: Image (base64)
- **Use Cases**: Photo editing, object removal, style modification
- **Supports Streaming**: ❌
- **Supports Tools**: ❌
- **Example**: Image editing models with natural language instructions

### Audio Models

#### `tts`
- **Description**: Text-to-speech synthesis
- **Input**: Text
- **Output**: Audio (wav/mp3/opus/flac)
- **Use Cases**: Voice assistants, audio books, accessibility
- **Supports Streaming**: ✅ (can stream audio)
- **Supports Tools**: ❌
- **Example**: `step-tts-mini`, `step-tts-vivid`

#### `asr`
- **Description**: Automatic speech recognition
- **Input**: Audio (flac/mp3/wav/m4a/ogg/webm/aac/opus)
- **Output**: Text
- **Use Cases**: Transcription, voice commands, meeting notes
- **Supports Streaming**: ❌ (processes complete audio files)
- **Supports Tools**: ❌
- **Example**: `step-asr`, `whisper-1`

### Video Models

#### `videounderstand`
- **Description**: Video understanding and analysis
- **Input**: Text + Video
- **Output**: Text
- **Use Cases**: Video analysis, content moderation, scene detection
- **Supports Streaming**: ✅
- **Supports Tools**: ✅
- **Example**: Future video analysis models

#### `text2video`
- **Description**: Text-to-video generation
- **Input**: Text
- **Output**: Video
- **Use Cases**: Animation, video content creation, demonstrations
- **Supports Streaming**: ❌
- **Supports Tools**: ❌
- **Example**: Future text-to-video models

### Document Models

#### `docunderstand`
- **Description**: Document understanding and analysis
- **Input**: Text + Document (PDF, etc.)
- **Output**: Text
- **Use Cases**: Document analysis, information extraction, summarization
- **Supports Streaming**: ✅
- **Supports Tools**: ✅
- **Example**: Document analysis models

### Specialized Models

#### `embedding`
- **Description**: Text embedding generation
- **Input**: Text
- **Output**: Vector (numerical embeddings)
- **Use Cases**: Semantic search, similarity matching, classification
- **Supports Streaming**: ❌ (returns single vector)
- **Supports Tools**: ❌
- **Example**: `text-embedding-3-large`, embedding models

## Configuration Examples

### Basic Configuration
```yaml
models:
  # Text model
  step-1-8k:
    vendor: step
    type: text
    supports_streaming: true
    supports_tools: true
    
  # Image understanding model
  step-1o-turbo-vision:
    vendor: step
    type: imageunderstand
    supports_streaming: true
    supports_tools: true
    supports_multimodal: true
    
  # Text-to-speech model
  step-tts-mini:
    vendor: step
    type: tts
    supports_streaming: false
    supports_tools: false
```

### Advanced Configuration with Capabilities
```yaml
models:
  step-vision-pro:
    vendor: step
    type: imageunderstand
    capabilities:
      model_type: imageunderstand
      supports_streaming: true
      requires_streaming: false
      supports_tools: true
      max_context_tokens: 32768
      max_output_tokens: 16384
      supports_system_messages: true
      custom_capabilities:
        max_images_per_request: 10
        supported_image_formats: ["jpeg", "png", "webp"]
        max_image_size_mb: 20
```

## Usage Examples

### Automatic Type Detection
```rust
use yanshi_llm::{Yanshi, ModelType, MultimodalMessage};

// The system automatically validates input types against model capabilities
let response = Yanshi::model("step-1o-turbo-vision")  // imageunderstand type
    .text_and_image(
        "Describe this image",
        "https://example.com/image.jpg"
    )
    .execute().await?;
```

### Capability Inspection
```rust
use yanshi_llm::ModelRegistry;

let registry = ModelRegistry::global();
let config = registry.get_model("step-1o-turbo-vision")?;

let granular_type = config.granular_type();
println!("Model type: {:?}", granular_type);  // ModelType::ImageUnderstand
println!("Description: {}", granular_type.description());
println!("Use cases: {:?}", granular_type.use_cases());
println!("Supports streaming: {}", granular_type.supports_streaming());
println!("Is multimodal: {}", granular_type.is_multimodal());
```

### Request Validation
```rust
// This will automatically fail if you try to send images to a text-only model
let message = MultimodalMessage::text_and_image("user", "Describe", "image.jpg");

let result = Yanshi::model("step-1-8k")  // text type - doesn't support images
    .multimodal_prompt(message)
    .execute().await;

// Result will be Err with a clear message about unsupported input type
```

### Model Selection Based on Capabilities
```rust
use yanshi_llm::{ModelRegistry, InputType};

fn find_models_supporting_images() -> Vec<String> {
    let registry = ModelRegistry::global();
    let config = registry.get_config().await.unwrap();
    
    config.models.iter()
        .filter(|(_, model_config)| {
            model_config.supports_input_type(&InputType::Image)
        })
        .map(|(name, _)| name.clone())
        .collect()
}
```

## Migration from Legacy Types

The system maintains backward compatibility with legacy type names:

| Legacy Type | New Granular Type | Notes |
|------------|-------------------|-------|
| `text` | `text` | No change |
| `multimodal` | `imageunderstand` | Default mapping for multimodal |
| `image` | `text2image` | Image generation |
| `audio` | `tts` | Text-to-speech |
| `tts` | `tts` | No change |
| `asr` | `asr` | No change |

Legacy configurations will continue to work, but new configurations should use granular types for better precision.

## Benefits

### 1. Clear Input/Output Requirements
- Know exactly what each model expects and produces
- Avoid confusion between image understanding vs image generation

### 2. Automatic Validation
- Requests are validated against model capabilities before sending
- Clear error messages when using incompatible input types

### 3. Better Model Selection
- Filter models by specific capabilities
- Find the right model for your use case more easily

### 4. Future-Proof Architecture
- Easy to add new model types as AI capabilities expand
- Extensible framework for specialized models

### 5. Development Experience
- IDE autocompletion and type safety
- Clear documentation of what each model does
- Reduced trial-and-error in model selection

## Error Handling

The granular type system provides specific error messages:

```rust
// Example error messages:
"Model step-1-8k does not support image input"
"Model step-asr does not support streaming"  
"Model step-tts-mini requires streaming mode"
```

## Performance Considerations

### Request Validation
- Validation happens before API calls, saving on failed requests
- Fast local validation vs expensive API calls

### Model Selection
- Choose the most appropriate model for your specific use case
- Avoid over-powered models for simple tasks

### Resource Management
- Know which models support streaming for better resource usage
- Understand output types for proper response handling

## Future Extensions

The granular type system is designed to accommodate future AI model types:

- `text2music`: Text-to-music generation
- `code2code`: Code transformation (e.g., language translation)
- `3dgen`: 3D model generation
- `multimodalunderstand`: Understanding multiple input modalities simultaneously
- `realtime`: Real-time conversation models

## Best Practices

1. **Use Specific Types**: Prefer granular types over legacy broad categories
2. **Validate Early**: Let the system validate requests before making API calls
3. **Inspect Capabilities**: Use capability inspection to understand model limits
4. **Handle Errors**: Provide clear error handling for validation failures
5. **Document Usage**: Include model type requirements in your application documentation

## Troubleshooting

### Common Issues

**"Model does not support X input"**
- Check the model's granular type and supported inputs
- Verify you're using a model that accepts your input type

**"Model requires streaming"**
- Some models (rare) only work in streaming mode
- Use `execute_streaming()` instead of `execute()`

**"Invalid model configuration"**
- Check that your model configuration matches the expected format
- Verify required fields for the specific model type

For more examples and detailed usage, see:
- `examples/granular_types_demo.rs` - Comprehensive examples
- `yanshi-llm/templates/default_models.yml` - authoritative model
  registry with `type:` per model (text / multimodal / tts / etc.)