# API Troubleshooting Guide

## Common API Errors and Solutions

### 1. Insufficient API Credits

**Error Message:**
```
Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.
```

**Solution:**
- Check your API provider's billing dashboard
- Add credits to your account
- Use alternative models from providers where you have active credits

### 2. Model Not Found (404 Error)

**Error Message:**
```
HTTP request failed: 404 - {"type":"error","error":{"type":"not_found_error","message":"Not Found"}}
```

**Common Causes:**
- Incorrect model name
- Model deprecated or not available in your region
- API endpoint changed

**Solution:**
- Verify the correct model name from the provider's documentation
- Use the model discovery feature to list available models
- Check the provider's API status page

### 3. Alternative Model Providers

When one provider has issues, you can easily switch to another:

| Provider | Model Examples | Environment Variable |
|----------|---------------|---------------------|
| OpenAI | `gpt-4o-mini`, `gpt-3.5-turbo` | `OPENAI_API_KEY` |
| DeepSeek | `deepseek-chat`, `deepseek-coder` | `DEEPSEEK_API_KEY` |
| Moonshot | `kimi-k2.6` | `MOONSHOT_API_KEY` |
| Qwen | `qwen-plus`, `qwen-turbo` | `DASHSCOPE_API_KEY` |
| StepFun | `step-2-mini`, `step-1-mini` | `STEP_API_KEY` |
| Zhipu | `glm-4`, `glm-3-turbo` | `ZHIPU_API_KEY` |

### 4. Fallback Strategy

The yanshi-nodes automatically falls back to mock responses when API calls fail. This ensures your workflow continues during development.

To explicitly use mock mode:
```rust
let node = LlmNode::new("test", "any-model")
    .with_mock_mode();  // Force mock mode
```

### 5. Checking Available Models

Run the model discovery example to see all configured models:
```bash
cargo run -p yanshi-llm --example model_discovery
```

### 6. Testing API Connectivity

Test a specific provider's API:
```bash
# Test OpenAI
curl https://api.openai.com/v1/models \
  -H "Authorization: Bearer $OPENAI_API_KEY" | jq '.data[0].id'

# Test DeepSeek
curl https://api.deepseek.com/v1/models \
  -H "Authorization: Bearer $DEEPSEEK_API_KEY" | jq '.data[0].id'

# Test Anthropic
curl https://api.anthropic.com/v1/messages \
  -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"claude-haiku-4-5","max_tokens":10,"messages":[{"role":"user","content":"Hi"}]}'
```

### 7. Environment Variable Setup

Ensure your API keys are properly set:
```bash
# In your .env file or shell profile
export OPENAI_API_KEY="sk-..."
export ANTHROPIC_API_KEY="sk-ant-..."
export DEEPSEEK_API_KEY="sk-..."
# ... other keys
```

### 8. Rate Limiting

If you encounter rate limiting errors:
- Implement exponential backoff
- Use the retry configuration in nodes:
```rust
use yanshi_nodes::nodes::llm::RetryConfig;

let node = LlmNode::new("test", "model")
    .with_retry_config(RetryConfig {
        max_attempts: 3,
        initial_delay_ms: 1000,
        backoff_multiplier: 2.0,
    });
```

### 9. Debugging API Calls

Enable detailed logging to debug API issues:
```bash
RUST_LOG=debug cargo run --example your_example
```

### 10. Cost Management Tips

- Use smaller models for development (`mini`, `turbo` variants)
- Set appropriate `max_tokens` limits
- Cache responses when possible
- Use mock mode during initial development
- Monitor usage through provider dashboards

## Example: Switching Providers

When you encounter API issues, you can easily switch providers:

```rust
// Original (Claude with no credits)
let node = LlmNode::new("summarizer", "claude-haiku-4-5");

// Alternative 1: OpenAI
let node = LlmNode::new("summarizer", "gpt-3.5-turbo");

// Alternative 2: DeepSeek
let node = LlmNode::new("summarizer", "deepseek-chat");

// Alternative 3: Local/Mock for testing
let node = LlmNode::new("summarizer", "any-model").with_mock_mode();
```

The node interface remains the same regardless of the provider, making it easy to switch based on availability and cost considerations.
