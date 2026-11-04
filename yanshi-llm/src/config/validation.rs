use crate::{LLMError, Result, config::LLMConfig};
use std::collections::HashSet;
use std::env;

/// Comprehensive validation utility for LLM configuration
pub struct ConfigValidator {
  config: LLMConfig,
}

impl ConfigValidator {
  pub fn new(config: LLMConfig) -> Self {
    Self { config }
  }

  /// Validate the entire configuration
  pub fn validate_all(&self) -> Result<ValidationReport> {
    let mut report = ValidationReport::new();

    // Validate models
    for (model_name, model_config) in &self.config.models {
      if let Err(e) = self.validate_model(model_name, model_config) {
        report.add_error(&format!("Model '{}': {}", model_name, e));
      } else {
        report.add_success(&format!("Model '{}' is valid", model_name));
      }
    }

    // Validate providers
    for (provider_name, provider_config) in &self.config.providers {
      if let Err(e) = self.validate_provider(provider_name, provider_config) {
        report.add_error(&format!("Provider '{}': {}", provider_name, e));
      } else {
        report.add_success(&format!("Provider '{}' is valid", provider_name));
      }
    }

    // Validate environment variables
    if let Err(e) = self.validate_environment() {
      report.add_error(&format!("Environment validation: {}", e));
    } else {
      report.add_success("All required environment variables are present");
    }

    // Check for orphaned models (models with providers not defined)
    self.validate_model_provider_consistency(&mut report);

    if report.has_errors() {
      Err(LLMError::ConfigurationError {
        message: format!("Configuration validation failed:\n{}", report.summary()),
      })
    } else {
      Ok(report)
    }
  }

  fn validate_model(
    &self,
    _model_name: &str,
    model_config: &crate::config::ModelConfig,
  ) -> Result<()> {
    // Check vendor is supported
    let supported_vendors = [
      "openai",
      "anthropic",
      "google",
      "gemini",
      "moonshot",
      "dashscope",
      "step",
      "stepfun",
      "glm",
      "bigmodel",
      "zhipu",
      "deepseek",
      "minimax",
      "kling",
    ];
    if !supported_vendors.contains(&model_config.vendor.as_str()) {
      return Err(LLMError::UnsupportedProvider {
        provider: model_config.vendor.clone(),
      });
    }

    // Validate temperature range
    if let Some(temp) = model_config.temperature
      && !(0.0..=2.0).contains(&temp)
    {
      return Err(LLMError::InvalidModelConfig {
        message: "Temperature must be between 0.0 and 2.0".to_string(),
      });
    }

    // Validate top_p range
    if let Some(top_p) = model_config.top_p
      && !(0.0..=1.0).contains(&top_p)
    {
      return Err(LLMError::InvalidModelConfig {
        message: "top_p must be between 0.0 and 1.0".to_string(),
      });
    }

    // Validate max_tokens
    if let Some(max_tokens) = model_config.max_tokens {
      if max_tokens == 0 {
        return Err(LLMError::InvalidModelConfig {
          message: "max_tokens must be greater than 0".to_string(),
        });
      }

      // Provider-specific limits
      match model_config.vendor.as_str() {
        "openai" if max_tokens > 128000 => {
          return Err(LLMError::InvalidModelConfig {
            message: "OpenAI models typically support max 128k tokens".to_string(),
          });
        }
        "anthropic" if max_tokens > 200000 => {
          return Err(LLMError::InvalidModelConfig {
            message: "Anthropic models typically support max 200k tokens".to_string(),
          });
        }
        _ => {}
      }
    }

    // Validate base_url format
    if let Some(base_url) = &model_config.base_url
      && !base_url.starts_with("http://")
      && !base_url.starts_with("https://")
    {
      return Err(LLMError::InvalidModelConfig {
        message: "base_url must start with http:// or https://".to_string(),
      });
    }

    Ok(())
  }

  fn validate_provider(
    &self,
    provider_name: &str,
    provider_config: &crate::config::ProviderConfig,
  ) -> Result<()> {
    // Check if API key environment variable exists
    if env::var(&provider_config.api_key_env).is_err() {
      return Err(LLMError::MissingApiKey {
        provider: provider_name.to_string(),
      });
    }

    // Validate timeout
    if let Some(timeout) = provider_config.timeout_seconds
      && (timeout == 0 || timeout > 300)
    {
      return Err(LLMError::InvalidModelConfig {
        message: "Timeout must be between 1 and 300 seconds".to_string(),
      });
    }

    // Validate rate limits
    if let Some(rate_limit) = &provider_config.rate_limit
      && rate_limit.requests_per_minute == 0
    {
      return Err(LLMError::InvalidModelConfig {
        message: "requests_per_minute must be greater than 0".to_string(),
      });
    }

    Ok(())
  }

  fn validate_environment(&self) -> Result<()> {
    let mut missing_keys = Vec::new();

    // Collect all required environment variables
    let mut required_env_vars = HashSet::new();

    for provider_config in self.config.providers.values() {
      required_env_vars.insert(provider_config.api_key_env.clone());
    }

    // Also check for models without explicit provider config
    for model_config in self.config.models.values() {
      if !self.config.providers.contains_key(&model_config.vendor) {
        // Use default environment variable patterns
        let default_env_var = format!("{}_API_KEY", model_config.vendor.to_uppercase());
        required_env_vars.insert(default_env_var);
      }
    }

    for env_var in &required_env_vars {
      if env::var(env_var).is_err() {
        missing_keys.push(env_var.clone());
      }
    }

    if !missing_keys.is_empty() {
      return Err(LLMError::ConfigurationError {
        message: format!("Missing environment variables: {}", missing_keys.join(", ")),
      });
    }

    Ok(())
  }

  fn validate_model_provider_consistency(&self, report: &mut ValidationReport) {
    let provider_names: HashSet<_> = self.config.providers.keys().collect();

    for (model_name, model_config) in &self.config.models {
      if !provider_names.contains(&model_config.vendor) {
        // This is not necessarily an error if we have default behavior
        report.add_warning(&format!(
          "Model '{}' uses provider '{}' which is not explicitly configured (using defaults)",
          model_name, model_config.vendor
        ));
      }
    }
  }
}

/// Report generated by configuration validation
#[derive(Debug)]
pub struct ValidationReport {
  pub errors: Vec<String>,
  pub warnings: Vec<String>,
  pub successes: Vec<String>,
}

impl ValidationReport {
  pub fn new() -> Self {
    Self {
      errors: Vec::new(),
      warnings: Vec::new(),
      successes: Vec::new(),
    }
  }

  pub fn add_error(&mut self, message: &str) {
    self.errors.push(message.to_string());
  }

  pub fn add_warning(&mut self, message: &str) {
    self.warnings.push(message.to_string());
  }

  pub fn add_success(&mut self, message: &str) {
    self.successes.push(message.to_string());
  }

  pub fn has_errors(&self) -> bool {
    !self.errors.is_empty()
  }

  pub fn summary(&self) -> String {
    let mut summary = String::new();

    if !self.errors.is_empty() {
      summary.push_str("ERRORS:\n");
      for error in &self.errors {
        summary.push_str(&format!("  ❌ {}\n", error));
      }
      summary.push('\n');
    }

    if !self.warnings.is_empty() {
      summary.push_str("WARNINGS:\n");
      for warning in &self.warnings {
        summary.push_str(&format!("  ⚠️  {}\n", warning));
      }
      summary.push('\n');
    }

    if !self.successes.is_empty() {
      summary.push_str("SUCCESS:\n");
      for success in &self.successes {
        summary.push_str(&format!("  ✅ {}\n", success));
      }
    }

    summary
  }
}

impl Default for ValidationReport {
  fn default() -> Self {
    Self::new()
  }
}

/// Convenience function to validate a configuration file
pub async fn validate_config(config_path: &str) -> Result<ValidationReport> {
  let config = LLMConfig::from_file(config_path).await?;
  let validator = ConfigValidator::new(config);
  validator.validate_all()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::env;

  #[test]
  fn test_config_validation_success() {
    // SAFETY: this unit test mutates a dedicated test env var before reading it.
    unsafe {
      env::set_var("TEST_OPENAI_API_KEY", "test-key");
    }

    let yaml = r#"
models:
  gpt-4o:
    vendor: openai
    temperature: 0.7
    max_tokens: 4096

providers:
  openai:
    api_key_env: "TEST_OPENAI_API_KEY"
    timeout_seconds: 30
"#;

    let config = LLMConfig::from_yaml(yaml).unwrap();
    let validator = ConfigValidator::new(config);
    let report = validator.validate_all().unwrap();

    assert!(!report.has_errors());
    assert!(!report.successes.is_empty());

    // SAFETY: cleanup of the dedicated test env var after the test read.
    unsafe {
      env::remove_var("TEST_OPENAI_API_KEY");
    }
  }

  #[test]
  fn test_config_validation_errors() {
    let yaml = r#"
models:
  invalid-model:
    vendor: unsupported_provider
    temperature: 3.0
    max_tokens: 0
"#;

    let config = LLMConfig::from_yaml(yaml).unwrap();
    let validator = ConfigValidator::new(config);
    let result = validator.validate_all();

    assert!(result.is_err());
  }
}
