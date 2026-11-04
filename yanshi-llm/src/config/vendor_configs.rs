//! Vendor-specific configuration management
//!
//! This module handles loading and managing configurations split by vendor
//! for better organization and performance.

use crate::{
  LLMError, Result,
  config::{LLMConfig, ModelConfig},
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info, warn};

/// Manager for vendor-specific configuration files
pub struct VendorConfigManager {
  config_dir: PathBuf,
}

impl VendorConfigManager {
  /// Create a new VendorConfigManager with a configuration directory
  pub fn new<P: AsRef<Path>>(config_dir: P) -> Self {
    Self {
      config_dir: config_dir.as_ref().to_path_buf(),
    }
  }

  /// Load configuration from vendor-specific files
  ///
  /// This method loads configurations in the following order:
  /// 1. Main config file (defaults and providers)
  /// 2. Vendor-specific model files (models/vendor_name.yml)
  /// 3. Merges all configurations into a single LLMConfig
  pub async fn load_config(&self) -> Result<LLMConfig> {
    let start_time = std::time::Instant::now();

    // Load main configuration (providers and defaults)
    let main_config_path = self.config_dir.join("config.yml");
    let mut config = if main_config_path.exists() {
      debug!("Loading main config from: {:?}", main_config_path);
      LLMConfig::from_file(&main_config_path).await?
    } else {
      debug!("No main config found, using defaults");
      LLMConfig::default()
    };

    // Load vendor-specific model configurations
    let models_dir = self.config_dir.join("models");
    if models_dir.exists() {
      let vendor_models = self.load_vendor_models(&models_dir).await?;
      config.models.extend(vendor_models);
      info!("Loaded {} models from vendor configs", config.models.len());
    } else {
      warn!("Models directory not found: {:?}", models_dir);
    }

    let load_time = start_time.elapsed();
    debug!("Configuration loaded in {:?}", load_time);

    Ok(config)
  }

  /// Split a monolithic configuration into vendor-specific files
  pub async fn split_config(&self, source_config: &LLMConfig) -> Result<SplitResult> {
    let start_time = std::time::Instant::now();

    // Create directory structure
    fs::create_dir_all(&self.config_dir)
      .await
      .map_err(|e| LLMError::ConfigurationError {
        message: format!("Failed to create config directory: {}", e),
      })?;

    let models_dir = self.config_dir.join("models");
    fs::create_dir_all(&models_dir)
      .await
      .map_err(|e| LLMError::ConfigurationError {
        message: format!("Failed to create models directory: {}", e),
      })?;

    // Group models by vendor
    let mut vendor_models: HashMap<String, HashMap<String, ModelConfig>> = HashMap::new();
    for (model_name, model_config) in &source_config.models {
      vendor_models
        .entry(model_config.vendor.clone())
        .or_default()
        .insert(model_name.clone(), model_config.clone());
    }

    // Write vendor-specific model files
    let mut result = SplitResult::new();
    for (vendor, models) in &vendor_models {
      let vendor_file = models_dir.join(format!("{}.yml", vendor));
      let vendor_config = VendorModelConfig {
        models: models.clone(),
      };

      let yaml_content =
        serde_yaml::to_string(&vendor_config).map_err(|e| LLMError::ConfigurationError {
          message: format!("Failed to serialize {} models: {}", vendor, e),
        })?;

      // Add header comment
      let full_content = format!(
        "# {} Models Configuration\n# Auto-generated from monolithic config\n# Last updated: {}\n\n{}",
        vendor.to_uppercase(),
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        yaml_content
      );

      fs::write(&vendor_file, full_content)
        .await
        .map_err(|e| LLMError::ConfigurationError {
          message: format!("Failed to write {}: {}", vendor_file.display(), e),
        })?;

      info!(
        "Created {} with {} models",
        vendor_file.display(),
        models.len()
      );
      result.vendor_files.push(vendor_file);
      result
        .models_per_vendor
        .insert(vendor.clone(), models.len());
    }

    // Write main configuration (providers and defaults only)
    let main_config = LLMConfig {
      models: HashMap::new(), // Models are now in vendor files
      providers: source_config.providers.clone(),
      defaults: source_config.defaults.clone(),
    };

    let main_config_path = self.config_dir.join("config.yml");
    let yaml_content =
      serde_yaml::to_string(&main_config).map_err(|e| LLMError::ConfigurationError {
        message: format!("Failed to serialize main config: {}", e),
      })?;

    let full_content = format!(
      "# Yanshi LLM Main Configuration\n# Contains providers and defaults only\n# Models are loaded from models/*.yml files\n# Last updated: {}\n\n{}",
      chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
      yaml_content
    );

    fs::write(&main_config_path, full_content)
      .await
      .map_err(|e| LLMError::ConfigurationError {
        message: format!("Failed to write main config: {}", e),
      })?;

    result.main_config_file = main_config_path;
    result.total_models = source_config.models.len();
    result.split_time = start_time.elapsed();

    Ok(result)
  }

  /// Load models from all vendor-specific files
  async fn load_vendor_models(&self, models_dir: &Path) -> Result<HashMap<String, ModelConfig>> {
    let mut all_models = HashMap::new();

    let mut entries = fs::read_dir(models_dir)
      .await
      .map_err(|e| LLMError::ConfigurationError {
        message: format!("Failed to read models directory: {}", e),
      })?;

    while let Some(entry) =
      entries
        .next_entry()
        .await
        .map_err(|e| LLMError::ConfigurationError {
          message: format!("Failed to read directory entry: {}", e),
        })?
    {
      let path = entry.path();

      // Only process .yml files
      if path.extension().and_then(|s| s.to_str()) == Some("yml") {
        debug!("Loading vendor models from: {:?}", path);

        match self.load_vendor_file(&path).await {
          Ok(vendor_models) => {
            let vendor_name = path
              .file_stem()
              .and_then(|s| s.to_str())
              .unwrap_or("unknown");

            info!("Loaded {} models from {}", vendor_models.len(), vendor_name);
            all_models.extend(vendor_models);
          }
          Err(e) => {
            warn!("Failed to load vendor file {:?}: {}", path, e);
            // Continue loading other files
          }
        }
      }
    }

    Ok(all_models)
  }

  /// Load models from a specific vendor file
  async fn load_vendor_file(&self, file_path: &Path) -> Result<HashMap<String, ModelConfig>> {
    let content =
      fs::read_to_string(file_path)
        .await
        .map_err(|e| LLMError::ConfigurationError {
          message: format!("Failed to read vendor file: {}", e),
        })?;

    parse_vendor_models_yaml(&content)
  }

  /// Update a specific vendor's models
  pub async fn update_vendor_models(
    &self,
    vendor: &str,
    models: HashMap<String, ModelConfig>,
  ) -> Result<()> {
    let models_dir = self.config_dir.join("models");
    fs::create_dir_all(&models_dir)
      .await
      .map_err(|e| LLMError::ConfigurationError {
        message: format!("Failed to create models directory: {}", e),
      })?;

    let vendor_file = models_dir.join(format!("{}.yml", vendor));
    let vendor_config = VendorModelConfig { models };

    let yaml_content =
      serde_yaml::to_string(&vendor_config).map_err(|e| LLMError::ConfigurationError {
        message: format!("Failed to serialize {} models: {}", vendor, e),
      })?;

    let full_content = format!(
      "# {} Models Configuration\n# Auto-updated by model discovery\n# Last updated: {}\n\n{}",
      vendor.to_uppercase(),
      chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
      yaml_content
    );

    fs::write(&vendor_file, full_content)
      .await
      .map_err(|e| LLMError::ConfigurationError {
        message: format!("Failed to write {}: {}", vendor_file.display(), e),
      })?;

    info!(
      "Updated {} with {} models",
      vendor_file.display(),
      vendor_config.models.len()
    );
    Ok(())
  }

  /// Get configuration directory path
  pub fn config_dir(&self) -> &Path {
    &self.config_dir
  }
}

/// Structure for vendor-specific model configuration files
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct VendorModelConfig {
  models: HashMap<String, ModelConfig>,
}

/// Parse a single vendor file's YAML content (`models: {...}` top-level
/// shape) into its models map.
///
/// Shared by [`VendorConfigManager::load_vendor_file`] (disk-based, async)
/// and [`builtin_models_map`] (compile-time, sync via `include_str!`) so
/// there is exactly one parsing implementation for the vendor-file shape.
pub(crate) fn parse_vendor_models_yaml(content: &str) -> Result<HashMap<String, ModelConfig>> {
  let vendor_config: VendorModelConfig =
    serde_yaml::from_str(content).map_err(|e| LLMError::ConfigurationError {
      message: format!("Failed to parse vendor config: {}", e),
    })?;
  Ok(vendor_config.models)
}

/// The registry's built-in vendor model files, bundled into the binary at
/// compile time (T-split, see `docs/` model registry restructuring).
///
/// `include_str!` can't glob a directory, so this is ten hardcoded calls
/// — fine and idiomatic, since the vendor list is fixed at compile time.
/// Filenames use each vendor's actual name (`stepfun`, not the `vendor:
/// step` value used inside the YAML — the vendor field and the filename
/// are independent; the bundler below merges by content, not by name).
const BUILTIN_VENDOR_FILES: &[(&str, &str)] = &[
  ("openai", include_str!("../../templates/models/openai.yml")),
  (
    "anthropic",
    include_str!("../../templates/models/anthropic.yml"),
  ),
  ("google", include_str!("../../templates/models/google.yml")),
  (
    "dashscope",
    include_str!("../../templates/models/dashscope.yml"),
  ),
  (
    "moonshot",
    include_str!("../../templates/models/moonshot.yml"),
  ),
  (
    "stepfun",
    include_str!("../../templates/models/stepfun.yml"),
  ),
  ("glm", include_str!("../../templates/models/glm.yml")),
  (
    "deepseek",
    include_str!("../../templates/models/deepseek.yml"),
  ),
  (
    "minimax",
    include_str!("../../templates/models/minimax.yml"),
  ),
  ("kling", include_str!("../../templates/models/kling.yml")),
];

/// Merge every bundled vendor file into one models map. Sync/compile-time
/// — no disk I/O, unlike [`VendorConfigManager::load_config`].
pub(crate) fn builtin_models_map() -> Result<HashMap<String, ModelConfig>> {
  let mut all_models = HashMap::new();
  for (vendor, content) in BUILTIN_VENDOR_FILES {
    let models = parse_vendor_models_yaml(content).map_err(|e| LLMError::ConfigurationError {
      message: format!("Failed to parse bundled '{}' models: {}", vendor, e),
    })?;
    all_models.extend(models);
  }
  Ok(all_models)
}

/// Build the full built-in default [`LLMConfig`]: every bundled vendor
/// file's models, merged with the `providers:`/`defaults:` parsed from
/// the slimmed `templates/default_models.yml` (models split out to
/// `templates/models/*.yml`, T-split).
///
/// This is what backs `Yanshi::init()`'s built-in-default path,
/// `ModelRegistry::load_builtin_config`, and (re-serialized to a string
/// by [`builtin_default_config_yaml`]) `Yanshi::generate_config`'s
/// starter file.
pub fn builtin_default_config() -> Result<LLMConfig> {
  let models = builtin_models_map()?;
  let mut config: LLMConfig =
    serde_yaml::from_str(include_str!("../../templates/default_models.yml")).map_err(|e| {
      LLMError::ConfigurationError {
        message: format!(
          "Failed to parse bundled default_models.yml providers/defaults: {}",
          e
        ),
      }
    })?;
  config.models = models;
  Ok(config)
}

/// [`builtin_default_config`], re-serialized to a single YAML string.
///
/// `Yanshi::generate_config()` writes this out as the user's starter
/// `~/.yanshi/models.yml` — a single self-contained file is easier to
/// hand-edit than the split `templates/models/*.yml` layout the crate
/// ships internally, and matches the file shape `LLMConfig::from_file`
/// already knows how to read back.
pub fn builtin_default_config_yaml() -> Result<String> {
  let config = builtin_default_config()?;
  serde_yaml::to_string(&config).map_err(|e| LLMError::ConfigurationError {
    message: format!("Failed to serialize built-in default config: {}", e),
  })
}

/// Result of splitting a monolithic configuration
#[derive(Debug)]
pub struct SplitResult {
  pub main_config_file: PathBuf,
  pub vendor_files: Vec<PathBuf>,
  pub models_per_vendor: HashMap<String, usize>,
  pub total_models: usize,
  pub split_time: std::time::Duration,
}

impl SplitResult {
  fn new() -> Self {
    Self {
      main_config_file: PathBuf::new(),
      vendor_files: Vec::new(),
      models_per_vendor: HashMap::new(),
      total_models: 0,
      split_time: std::time::Duration::from_secs(0),
    }
  }

  /// Create a report of the split operation
  pub fn create_report(&self) -> String {
    let mut report = String::new();

    report.push_str("Configuration Split Report\n");
    report.push_str("=========================\n\n");

    report.push_str(&format!(
      "📁 Main config: {}\n",
      self.main_config_file.display()
    ));
    report.push_str(&format!("📊 Total models: {}\n", self.total_models));
    report.push_str(&format!("🏢 Vendor files: {}\n\n", self.vendor_files.len()));

    for (vendor, count) in &self.models_per_vendor {
      report.push_str(&format!("  - {}: {} models\n", vendor, count));
    }

    report.push_str(&format!(
      "\n⏱️  Split completed in: {:?}\n",
      self.split_time
    ));

    // Calculate file size benefits
    let avg_models_per_file = self.total_models as f64 / self.vendor_files.len() as f64;
    report.push_str(&format!(
      "💡 Average models per file: {:.1}\n",
      avg_models_per_file
    ));
    report.push_str("📈 Benefits:\n");
    report.push_str("  - Faster loading (only load needed vendors)\n");
    report.push_str("  - Better organization and maintainability\n");
    report.push_str("  - Easier vendor-specific updates\n");
    report.push_str("  - Reduced memory usage for partial loads\n");

    report
  }
}

/// Performance comparison utility
pub struct PerformanceComparison;

impl PerformanceComparison {
  /// Compare loading performance between monolithic and split configurations
  pub async fn benchmark_loading(
    monolithic_path: &Path,
    split_config_dir: &Path,
  ) -> Result<LoadingBenchmark> {
    let mut benchmark = LoadingBenchmark::new();

    // Benchmark monolithic loading
    let start = std::time::Instant::now();
    let _monolithic_config = LLMConfig::from_file(monolithic_path).await?;
    benchmark.monolithic_load_time = start.elapsed();

    // Benchmark split loading
    let start = std::time::Instant::now();
    let manager = VendorConfigManager::new(split_config_dir);
    let _split_config = manager.load_config().await?;
    benchmark.split_load_time = start.elapsed();

    // Calculate file sizes
    let monolithic_metadata =
      fs::metadata(monolithic_path)
        .await
        .map_err(|e| LLMError::ConfigurationError {
          message: format!("Failed to get monolithic file metadata: {}", e),
        })?;
    benchmark.monolithic_file_size = monolithic_metadata.len();

    // Calculate total size of split files
    let mut total_split_size = 0;
    let mut entries =
      fs::read_dir(split_config_dir)
        .await
        .map_err(|e| LLMError::ConfigurationError {
          message: format!("Failed to read split config directory: {}", e),
        })?;

    while let Some(entry) =
      entries
        .next_entry()
        .await
        .map_err(|e| LLMError::ConfigurationError {
          message: format!("Failed to read directory entry: {}", e),
        })?
    {
      let metadata = entry
        .metadata()
        .await
        .map_err(|e| LLMError::ConfigurationError {
          message: format!("Failed to get file metadata: {}", e),
        })?;
      total_split_size += metadata.len();
    }

    // Check models subdirectory
    let models_dir = split_config_dir.join("models");
    if models_dir.exists() {
      let mut models_entries =
        fs::read_dir(&models_dir)
          .await
          .map_err(|e| LLMError::ConfigurationError {
            message: format!("Failed to read models directory: {}", e),
          })?;

      while let Some(entry) =
        models_entries
          .next_entry()
          .await
          .map_err(|e| LLMError::ConfigurationError {
            message: format!("Failed to read models directory entry: {}", e),
          })?
      {
        let metadata = entry
          .metadata()
          .await
          .map_err(|e| LLMError::ConfigurationError {
            message: format!("Failed to get models file metadata: {}", e),
          })?;
        total_split_size += metadata.len();
      }
    }

    benchmark.split_total_size = total_split_size;

    Ok(benchmark)
  }
}

/// Results of loading performance benchmark
#[derive(Debug)]
pub struct LoadingBenchmark {
  pub monolithic_load_time: std::time::Duration,
  pub split_load_time: std::time::Duration,
  pub monolithic_file_size: u64,
  pub split_total_size: u64,
}

impl LoadingBenchmark {
  fn new() -> Self {
    Self {
      monolithic_load_time: std::time::Duration::from_secs(0),
      split_load_time: std::time::Duration::from_secs(0),
      monolithic_file_size: 0,
      split_total_size: 0,
    }
  }

  /// Create a performance comparison report
  pub fn create_report(&self) -> String {
    let mut report = String::new();

    report.push_str("Loading Performance Comparison\n");
    report.push_str("=============================\n\n");

    // Loading times
    report.push_str("⏱️  Loading Times:\n");
    report.push_str(&format!("  Monolithic: {:?}\n", self.monolithic_load_time));
    report.push_str(&format!("  Split:      {:?}\n", self.split_load_time));

    let time_diff = if self.split_load_time > self.monolithic_load_time {
      let diff = self.split_load_time - self.monolithic_load_time;
      format!("Split is {:?} slower", diff)
    } else {
      let diff = self.monolithic_load_time - self.split_load_time;
      format!("Split is {:?} faster", diff)
    };
    report.push_str(&format!("  Difference: {}\n\n", time_diff));

    // File sizes
    report.push_str("💾 File Sizes:\n");
    report.push_str(&format!(
      "  Monolithic: {} KB\n",
      self.monolithic_file_size / 1024
    ));
    report.push_str(&format!(
      "  Split total: {} KB\n",
      self.split_total_size / 1024
    ));

    let size_diff = if self.split_total_size > self.monolithic_file_size {
      let diff = self.split_total_size - self.monolithic_file_size;
      format!(
        "Split uses {} KB more (due to headers/structure)",
        diff / 1024
      )
    } else {
      let diff = self.monolithic_file_size - self.split_total_size;
      format!("Split uses {} KB less", diff / 1024)
    };
    report.push_str(&format!("  Difference: {}\n\n", size_diff));

    // Recommendations
    report.push_str("💡 Recommendations:\n");
    if self.split_load_time <= self.monolithic_load_time * 2 {
      report.push_str(
        "  ✅ Split configuration provides good organization with minimal performance impact\n",
      );
    } else {
      report.push_str(
        "  ⚠️  Split configuration has notable performance impact - consider lazy loading\n",
      );
    }

    if self.monolithic_file_size > 50_000 {
      // 50KB
      report.push_str("  ✅ Monolithic file is large enough to benefit from splitting\n");
    } else {
      report.push_str(
        "  ℹ️  Monolithic file is small - splitting may not provide significant benefits\n",
      );
    }

    report
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::tempdir;

  #[tokio::test]
  async fn test_vendor_config_manager() {
    let temp_dir = tempdir().unwrap();
    let manager = VendorConfigManager::new(temp_dir.path());

    // Test with empty directory
    let config = manager.load_config().await.unwrap();
    assert!(config.models.is_empty());
  }

  #[tokio::test]
  async fn test_split_result_report() {
    let mut result = SplitResult::new();
    result.total_models = 100;
    result.models_per_vendor.insert("openai".to_string(), 30);
    result.models_per_vendor.insert("anthropic".to_string(), 70);
    result.split_time = std::time::Duration::from_millis(150);

    let report = result.create_report();
    assert!(report.contains("Total models: 100"));
    assert!(report.contains("openai: 30 models"));
    assert!(report.contains("anthropic: 70 models"));
  }
}
