//! Example: extended-reasoning ("thinking") across providers.
//!
//! Demonstrates the unified `.thinking(ThinkingConfig)` API working
//! across Anthropic claude-3.7+/4.x, OpenAI o-series, Google Gemini
//! 2.5+, and DeepSeek-R1 (output-only). Each provider's native wire
//! shape is generated transparently — caller code stays portable.
//!
//! Run (requires the relevant API key in the environment):
//! ```bash
//! ANTHROPIC_API_KEY=... cargo run --example thinking --features logging
//! ```
//!
//! The example does NOT panic on missing API keys; it skips models that
//! aren't reachable so you can run it with whichever provider(s) you
//! have credentials for.

use yanshi_llm::{LLMError, ThinkingConfig, Yanshi};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  Yanshi::init().await?;

  // The same builder chain works for every reasoning model — the LLM
  // layer maps `ThinkingConfig::Medium` to each provider's native wire
  // shape (Anthropic thinking block, OpenAI reasoning_effort, Google
  // thinkingConfig).
  let candidates = &[
    ("gemini-2.5-pro", "Google Gemini 2.5 Pro"),
    ("deepseek-reasoner", "DeepSeek-R1 (output-only)"),
  ];

  for (model, label) in candidates {
    println!("\n=== {label} ({model}) ===");
    let result = Yanshi::model(model)
      .prompt("A snail climbs a 10m wall: it climbs 3m each day, slides 2m each night. How many days to reach the top?")
      .thinking(ThinkingConfig::Medium)
      .execute_full()
      .await;

    match result {
      Ok(response) => {
        if let Some(thinking) = response.thinking.as_deref() {
          println!("--- thinking ---");
          println!("{thinking}");
        }
        println!("--- answer ---");
        println!("{}", response.content);
        if let Some(usage) = response.usage {
          println!(
            "tokens: prompt={:?} completion={:?} total={:?}",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
          );
        }
      }
      // Skip providers without credentials so the example stays useful
      // when only some keys are set. UnsupportedFeature would mean the
      // registry entry is missing a `supports_thinking: true` flag —
      // surface that loudly because it's a config bug, not a missing key.
      Err(LLMError::MissingApiKey { provider }) => {
        println!("(skipping — no API key set for provider '{provider}')");
      }
      Err(LLMError::UnsupportedFeature { model, feature }) => {
        eprintln!(
          "ERROR: model '{model}' is not configured for '{feature}'. \
           Did you forget `supports_thinking: true` in the registry?"
        );
      }
      Err(err) => {
        eprintln!("ERROR for {model}: {err}");
      }
    }
  }

  Ok(())
}
