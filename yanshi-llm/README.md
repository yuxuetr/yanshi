# Yanshi LLM Integration

A comprehensive LLM integration crate for Yanshi that provides a unified interface for multiple LLM providers with streaming support, configuration management, and built-in observability.

## Features

### 🚀 **Core Capabilities**
- **Multi-Provider Support**: OpenAI, Anthropic (Claude), Google (Gemini), and Moonshot
- **Streaming & Non-Streaming**: Separate methods for different execution modes
- **Smart Configuration**: Auto-discovery with project/user/built-in config hierarchy
- **Zero-Config Start**: Works immediately with built-in defaults
- **Fluent API**: Easy-to-use builder pattern for model invocation
- **Async-First**: Built on tokio with full async/await support

### 🔍 **Observability & Debugging**
- **Comprehensive Logging**: Request/response metadata, timing, and debugging
- **Configurable Log Levels**: DEBUG, INFO, WARN, ERROR with RUST_LOG control
- **Performance Metrics**: Request timing and token usage tracking
- **Error Context**: Detailed failure information with stack traces
- **API Key Security**: Automatic masking of sensitive data in logs

### 📊 **Structured Output**
- **JSON Mode**: Enforced valid JSON responses from models
- **JSON Schema Validation**: Structured data with schema compliance
- **Response Format Control**: Text, JSON Object, or JSON Schema modes
- **Automatic Validation**: Built-in parsing and format verification

### ⚙️ **Configuration Management**
- **Multi-Tier Config**: Project, user, and built-in configuration hierarchy
- **Environment Management**: Smart .env file handling with security
- **Auto-Generation**: One-command setup for new projects
- **Security-First**: Automatic .gitignore protection for API keys

## Quick Start

### Option A: Zero Configuration (Immediate Use)

```rust
use yanshi_llm::Yanshi;

#[tokio::main]
async fn main() -> Result<(), yanshi_llm::LLMError> {
  // Works immediately with built-in defaults
  Yanshi::init().await?;
  
  // Set your API key via environment variable
  std::env::set_var("OPENAI_API_KEY", "your-api-key-here");
  
  let response = Yanshi::model("gpt-4o")
    .prompt("Hello, world!")
    .execute().await?;
    
  println!("Response: {}", response);
  Ok(())
}
```

### Option B: Custom Configuration

#### Step 1: Complete Setup (Recommended)

```bash
# Generate both config and environment files
cargo run --example config_management setup
```

This creates:
- `models.yml` (model configurations)
- `.env` (API key templates)
- `.gitignore` (security protection)

#### Step 2: Add Your API Keys

Edit the generated `.env` file:

```env
# Add your real API keys
OPENAI_API_KEY=sk-your-real-openai-key-here
ANTHROPIC_API_KEY=sk-ant-your-real-anthropic-key
GOOGLE_API_KEY=your-real-google-api-key
```

**Alternative**: Set environment variables directly:

```bash
export OPENAI_API_KEY="your-openai-key"
export ANTHROPIC_API_KEY="your-anthropic-key"
export GOOGLE_API_KEY="your-google-key"
```

#### Step 3: Use in Your Code

**Basic Usage:**

```rust
use yanshi_llm::Yanshi;

#[tokio::main]
async fn main() -> Result<(), yanshi_llm::LLMError> {
  // Initialize logging and configuration
  Yanshi::init_logging()?;
  Yanshi::init_with_env().await?;
  
  // Basic text response
  let response = Yanshi::model("claude-sonnet-5")
    .prompt("Explain quantum computing")
    .temperature(0.7)
    .max_tokens(500)
    .execute().await?;
    
  println!("{}", response);
  Ok(())
}
```

**JSON Mode for Structured Output:**

```rust
use yanshi_llm::{Yanshi, ResponseFormat};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), yanshi_llm::LLMError> {
  Yanshi::init_logging()?;
  Yanshi::init_with_env().await?;
  
  // Force JSON response
  let json_response = Yanshi::model("gpt-4o")
    .prompt("Return user data as JSON with name, age, email fields")
    .json_mode()
    .enable_logging(true)
    .execute().await?;
    
  // Parse the guaranteed JSON
  let parsed: serde_json::Value = serde_json::from_str(&json_response)?;
  println!("Structured data: {}", serde_json::to_string_pretty(&parsed)?);
  
  Ok(())
}
```

**Streaming with Real-time Processing:**

```rust
#[tokio::main]
async fn main() -> Result<(), yanshi_llm::LLMError> {
  Yanshi::init_with_env().await?;
  
  let mut stream = Yanshi::model("claude-sonnet-5")
    .prompt("Write a story about AI")
    .temperature(0.8)
    .execute_streaming().await?;
    
  while let Some(chunk) = stream.next_chunk().await? {
    print!("{}", chunk.content);
    if chunk.is_final {
      println!("\n[Stream completed]");
      break;
    }
  }
  
  Ok(())
}
```

## Configuration Management

### Configuration Priority

Yanshi uses smart hierarchies for both config and environment:

#### YAML Configuration:
1. **Project Config**: `./models.yml` (highest priority)
2. **User Config**: `~/.yanshi/models.yml` (medium priority)
3. **Built-in Defaults**: Bundled in crate (lowest priority)

#### Environment Variables:
1. **System Environment**: `export OPENAI_API_KEY=...` (highest priority)
2. **Project .env**: `./.env` (medium priority)
3. **User .env**: `~/.yanshi/.env` (lowest priority)

### Configuration Strategies

| Strategy | Use Case | Setup |
|----------|----------|---------|
| **Built-in Defaults** | Quick prototyping, testing | Just call `Yanshi::init()` |
| **Complete Setup** | New projects | `cargo run --example config_management setup` |
| **Project-Specific** | Custom models per project | Generate `models.yml` + `.env` |
| **User Defaults** | Global settings | Generate `~/.yanshi/` configs |

### Installation & Security

#### Installation Behavior
- **Installing the crate**: No files generated automatically
- **First `Yanshi::init()`**: Uses built-in defaults (works immediately)
- **Generate files**: Use provided helper functions or CLI examples

#### Security Features
- **Auto .gitignore**: Prevents accidentally committing API keys
- **Template .env**: Clear placeholders, not real keys
- **Environment priority**: System vars override file-based keys
- **No hardcoded keys**: Built-in defaults require user-provided keys

#### Environment Management Commands
```bash
# Complete project setup
cargo run --example config_management setup

# Generate only .env file
cargo run --example config_management generate-env

# Generate user-specific .env
cargo run --example config_management generate-env-user

# Initialize with .env auto-loading
cargo run --example config_management init-env
```

## Advanced Features

### JSON Schema Validation

Enforce specific data structures in model responses:

```rust
use serde_json::json;

let user_schema = json!({
  "type": "object",
  "properties": {
    "name": {"type": "string"},
    "age": {"type": "number", "minimum": 0},
    "skills": {
      "type": "array",
      "items": {"type": "string"}
    }
  },
  "required": ["name", "age"]
});

let response = Yanshi::model("gpt-4o")
  .prompt("Generate a software developer profile")
  .json_schema("developer_profile", user_schema)
  .execute().await?;

// Response is guaranteed to match the schema
let developer: Developer = serde_json::from_str(&response)?;
```

### Comprehensive Logging

Control observability at multiple levels:

```bash
# Environment variable control
RUST_LOG=debug cargo run     # Full request/response content
RUST_LOG=info cargo run      # Request summaries and timing
RUST_LOG=warn cargo run      # Warnings and validation issues
RUST_LOG=error cargo run     # Only critical failures
```

```rust
// Per-request logging control
let response = Yanshi::model("gpt-4o")
  .prompt("Analyze this data")
  .enable_logging(true)  // Override global settings
  .json_mode()
  .execute().await?;
```

### Tools and Function Calling

Ready for MCP (Model Context Protocol) integration:

```rust
// Future MCP integration
let tools = get_mcp_tools().await?;  // From yanshi-mcp crate

let response = Yanshi::model("gpt-4o")
  .prompt("Search for weather in Tokyo")
  .tools(tools)  // Vec<Value> from MCP
  .execute().await?;
```

## Installation

### 1. Add Dependency

```toml
[dependencies]
# For development
yanshi-llm = { path = "../yanshi-llm", features = ["logging"] }

# For production
# yanshi-llm = { version = "0.1.0", features = ["logging"] }
tokio = { version = "1.0", features = ["full"] }
```

### 2. Set Up Configuration

Create a `models.yml` file (see `examples/models.yml` for a complete example):

```yaml
models:
  gpt-4o:
    vendor: openai
    temperature: 0.7
    max_tokens: 4096
    supports_streaming: true

  claude-sonnet-5:
    vendor: anthropic
    model_id: "claude-sonnet-5"
    temperature: 0.5
    max_tokens: 4096

  kimi-k2.6:
    vendor: moonshot
    temperature: 1.0
    max_tokens: 8192
    supports_streaming: true

providers:
  openai:
    api_key_env: "OPENAI_API_KEY"
  anthropic:
    api_key_env: "ANTHROPIC_API_KEY"
  moonshot:
    api_key_env: "MOONSHOT_API_KEY"
```

### 3. Set Environment Variables

Create a `.env` file:

```bash
OPENAI_API_KEY=sk-your-openai-key-here
ANTHROPIC_API_KEY=sk-ant-your-anthropic-key-here
GOOGLE_API_KEY=your-google-api-key-here
MOONSHOT_API_KEY=your-moonshot-api-key-here
```

### 4. Basic Usage

```rust
use yanshi_llm::{Yanshi, LLMError};

#[tokio::main]
async fn main() -> Result<(), LLMError> {
    // Initialize the system
    Yanshi::init_with_config("models.yml").await?;

    // Basic usage
    let response = Yanshi::model("gpt-4o")
        .prompt("What is the capital of France?")
        .temperature(0.7)
        .max_tokens(100)
        .execute()
        .await?;

    println!("Response: {}", response);

    // Streaming usage
    let mut stream = Yanshi::model("claude-sonnet-5")
        .prompt("Tell me a short story.")
        .streaming(true)
        .execute_streaming()
        .await?;

    while let Some(chunk) = stream.next_chunk().await? {
        print!("{}", chunk.content);
        if chunk.is_final {
            break;
        }
    }

    Ok(())
}
```

## Advanced Usage

### With Observability

```rust
use yanshi_core::observability::MetricsCollector;
use std::sync::Arc;

let metrics = Arc::new(MetricsCollector::new());

let response = Yanshi::model("gpt-4o")
    .prompt("Hello, world!")
    .with_metrics(metrics.clone())
    .execute()
    .await?;

// Access metrics
let request_count = metrics.get_metric("llm.gpt-4o.requests");
let success_count = metrics.get_metric("llm.gpt-4o.success");
```

### Configuration Validation

```rust
use yanshi_llm::config::validate_config;

let report = validate_config("models.yml").await?;
println!("{}", report.summary());
```

### Multiple Models

```rust
let models = ["gpt-4o", "claude-sonnet-5", "gemini-1.5-pro"];

for model in models {
    let response = Yanshi::model(model)
        .prompt("Say hello")
        .execute()
        .await?;
    
    println!("{}: {}", model, response);
}
```

## Supported Providers

### OpenAI
- Models: gpt-4o, gpt-4o-mini, gpt-4-turbo, gpt-4, gpt-3.5-turbo
- Features: Streaming, function calling, image inputs
- Environment: `OPENAI_API_KEY`

### Anthropic (Claude)
- Models: claude-opus-5, claude-sonnet-5, claude-fable-5, claude-haiku-4-5
- Features: Streaming, long context, system prompts
- Environment: `ANTHROPIC_API_KEY` or `CLAUDE_API_KEY`

### Google (Gemini)
- Models: gemini-1.5-pro, gemini-1.5-flash, gemini-1.0-pro
- Features: Streaming, multimodal inputs, function calling
- Environment: `GOOGLE_API_KEY` or `GEMINI_API_KEY`

### Moonshot
- Models: kimi-k2-0711-preview, kimi-k2.5, kimi-k2.6 (the `moonshot-v1-*`
  family was retired upstream on 2026-08-31 and removed from the registry)
- Features: Streaming, Chinese and English, long context (up to 128k tokens)
- Environment: `MOONSHOT_API_KEY` or `MOONSHOT_KEY`
- **kimi-k2.6 quirk (F-A7-5)**: only accepts `temperature: 1.0`.
  Moonshot returns `400 invalid temperature: only 1 is allowed
  for this model` on any other value — likely a reasoning-model
  constraint. The bundled `templates/default_models.yml` sets
  this correctly with an inline comment, but users who hand-copy
  the entry into `~/.yanshi/models.yml` should preserve it.
  Other kimi-k2.x variants (`kimi-k2-0711-preview`, `kimi-k2.5`)
  accept normal temperatures.
- **Organisation concurrency limit**: Moonshot's default tier
  caps simultaneous requests at 3 per org. Workflows using
  `map parallel` should set `max_concurrent: 3` (or upgrade the
  tier) — see [F-A6-1 in the
  changelog](../CHANGELOG.md#unreleased--v030-candidate).

## Configuration Options

### Model Configuration

```yaml
model-name:
  vendor: openai|anthropic|google|moonshot
  model_id: "actual-model-id"      # Optional, defaults to model name
  base_url: "https://api.example.com"  # Optional, uses provider default
  temperature: 0.7                 # Optional
  top_p: 0.9                      # Optional
  max_tokens: 4096                # Optional
  supports_streaming: true        # Optional, defaults to true
```

### Provider Configuration

```yaml
providers:
  openai:
    api_key_env: "OPENAI_API_KEY"
    base_url: "https://api.openai.com/v1"
    timeout_seconds: 60
    rate_limit:
      requests_per_minute: 500
      tokens_per_minute: 80000
```

## Error Handling

The crate provides comprehensive error types:

```rust
use yanshi_llm::LLMError;

match result {
    Err(LLMError::ModelNotFound { model_name }) => {
        println!("Model '{}' not configured", model_name);
    }
    Err(LLMError::MissingApiKey { provider }) => {
        println!("API key missing for {}", provider);
    }
    Err(LLMError::RateLimitExceeded { provider, message }) => {
        println!("Rate limited by {}: {}", provider, message);
    }
    // ... handle other error types
    Ok(response) => println!("Success: {}", response),
}
```

## Examples

Run the examples:

```bash
# Basic usage
cargo run --example basic_usage

# With observability
cargo run --example with_observability

# Configuration validation
cargo run --example config_validation
```

## Testing

```bash
# Run all tests
cargo test

# Run with all features
cargo test --all-features

# Run specific test
cargo test test_openai_provider
```

## Integration with Yanshi

The LLM crate integrates seamlessly with Yanshi's observability system:

```rust
use yanshi_core::{AsyncFlow, MetricsCollector};
use std::sync::Arc;

let metrics = Arc::new(MetricsCollector::new());
let mut flow = AsyncFlow::new(my_node);
flow.set_metrics_collector(metrics.clone());

// Use the same metrics collector for LLM calls
let response = Yanshi::model("gpt-4o")
    .with_metrics(metrics)
    .prompt("Process this data")
    .execute()
    .await?;
```

## Quick Reference

### Execution Methods

```rust
// Non-streaming (returns complete response)
let response: String = client.execute().await?;

// Streaming (returns chunk-by-chunk)
let mut stream = client.execute_streaming().await?;
while let Some(chunk) = stream.next_chunk().await? {
  print!("{}", chunk.content);
}
```

### Response Formats

```rust
// Text response (default)
.execute().await?

// JSON object (enforced)
.json_mode().execute().await?

// JSON schema (validated)
.json_schema("name", schema).execute().await?
```

### Configuration Commands

```bash
# Complete setup
cargo run --example config_management setup

# Generate config only
cargo run --example config_management generate

# Test functionality
cargo run --example config_management demo
```

### Logging Levels

```bash
RUST_LOG=error   # Errors only
RUST_LOG=warn    # Warnings + errors
RUST_LOG=info    # Request summaries + above
RUST_LOG=debug   # Full content + above
```

## Performance

- **Minimal Overhead**: Less than 1ms overhead for model switching
- **Concurrent Requests**: Full async support with connection pooling
- **Streaming Optimized**: Low-latency streaming with backpressure handling
- **Memory Efficient**: Lazy loading of providers and configurations
- **Production Ready**: Comprehensive logging and error handling

## License

MIT License - see LICENSE file for details.

---

**Built with ❤️ for the Rust community**