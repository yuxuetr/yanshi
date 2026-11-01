//! The `rag_search` [`Tool`] (RFC §3 — `rag` on the tools axis).
//!
//! [`RagSearchTool`] adapts any [`KnowledgeBackend`] into an atomic, registry-
//! installable tool an agent loop can call. This is the runtime-facing half of
//! the RAG repositioning: a Skill lowers its `knowledge:` backend to a
//! `rag_search` tool plus context, and the agent retrieves on demand instead of
//! having the whole corpus inlined into its prompt.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use yanshi_store_spi::{KnowledgeBackend, KnowledgeChunk};
use yanshi_tools::{Tool, ToolError, ToolIdempotency, ToolMetadata, ToolOutput, ToolOutputPart};

/// Default number of passages returned when the caller omits `top_k`.
const DEFAULT_TOP_K: usize = 5;

/// A registry-installable knowledge-search tool backed by a [`KnowledgeBackend`].
pub struct RagSearchTool {
  backend: Arc<dyn KnowledgeBackend>,
  default_top_k: usize,
}

impl RagSearchTool {
  /// Wrap a backend with the default `top_k` of `DEFAULT_TOP_K`.
  pub fn new(backend: Arc<dyn KnowledgeBackend>) -> Self {
    Self {
      backend,
      default_top_k: DEFAULT_TOP_K,
    }
  }

  /// Override the fallback `top_k` used when a call omits the parameter.
  pub fn with_default_top_k(mut self, top_k: usize) -> Self {
    self.default_top_k = top_k.max(1);
    self
  }

  /// Render retrieved chunks into the tool's textual content payload. Each
  /// passage is numbered with its source (when known) and score so the LLM can
  /// cite provenance.
  fn render(chunks: &[KnowledgeChunk]) -> String {
    if chunks.is_empty() {
      return "No relevant passages found.".to_string();
    }
    let mut out = String::new();
    for (i, c) in chunks.iter().enumerate() {
      let source = c.source.as_deref().unwrap_or(&c.id);
      out.push_str(&format!(
        "[{}] (source: {}, score: {:.3})\n{}\n\n",
        i + 1,
        source,
        c.score,
        c.content.trim()
      ));
    }
    out.truncate(out.trim_end().len());
    out
  }
}

#[async_trait]
impl Tool for RagSearchTool {
  fn name(&self) -> &str {
    "rag_search"
  }

  fn description(&self) -> &str {
    "Search the knowledge base for passages relevant to a query. Returns the \
     most relevant passages ranked best-first, each with its source and score."
  }

  fn parameters_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": {
        "query": {
          "type": "string",
          "description": "Natural-language search query."
        },
        "top_k": {
          "type": "integer",
          "minimum": 1,
          "description": "Maximum number of passages to return.",
          "default": self.default_top_k
        }
      },
      "required": ["query"]
    })
  }

  fn metadata(&self) -> ToolMetadata {
    // Knowledge search is a pure, side-effect-free read — safe to replay on
    // partial resume, so declare it idempotent.
    ToolMetadata::builtin().with_idempotency(ToolIdempotency::Idempotent)
  }

  async fn execute(&self, params: Value) -> Result<ToolOutput, ToolError> {
    let query =
      params
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidParams {
          message: "missing required string field `query`".to_string(),
        })?;

    let top_k = params
      .get("top_k")
      .and_then(Value::as_u64)
      .map(|n| (n as usize).max(1))
      .unwrap_or(self.default_top_k);

    let chunks =
      self
        .backend
        .search(query, top_k)
        .await
        .map_err(|e| ToolError::ExecutionFailed {
          message: format!("rag_search failed: {e}"),
        })?;

    let parts = chunks
      .iter()
      .map(|c| ToolOutputPart::Text {
        text: c.content.clone(),
      })
      .collect::<Vec<_>>();

    Ok(ToolOutput::success_parts(Self::render(&chunks), parts))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::knowledge::Bm25KnowledgeBackend;

  fn tool() -> RagSearchTool {
    let backend = Arc::new(Bm25KnowledgeBackend::from_documents([
      (
        "doc-rust",
        "Rust is a systems programming language focused on safety",
      ),
      ("doc-python", "Python is a high level scripting language"),
    ]));
    RagSearchTool::new(backend)
  }

  #[test]
  fn declares_idempotent_read_only_metadata() {
    assert_eq!(tool().metadata().idempotency, ToolIdempotency::Idempotent);
    assert_eq!(tool().name(), "rag_search");
  }

  #[test]
  fn schema_requires_query() {
    let schema = tool().parameters_schema();
    assert_eq!(schema["required"][0], "query");
  }

  #[tokio::test]
  async fn execute_returns_ranked_passages_with_parts() {
    let out = tool()
      .execute(json!({ "query": "rust safety" }))
      .await
      .expect("execute ok");
    assert!(!out.is_error);
    assert!(out.content.contains("Rust"), "content should cite the hit");
    assert!(
      !out.parts.is_empty(),
      "structured parts should be populated"
    );
  }

  #[tokio::test]
  async fn execute_missing_query_is_invalid_params() {
    let err = tool()
      .execute(json!({ "top_k": 3 }))
      .await
      .expect_err("missing query rejected");
    assert!(matches!(err, ToolError::InvalidParams { .. }));
  }

  #[tokio::test]
  async fn execute_no_match_reports_empty_payload() {
    let out = tool()
      .execute(json!({ "query": "zzzznonexistentterm" }))
      .await
      .expect("execute ok");
    assert!(!out.is_error);
    assert!(out.content.contains("No relevant passages"));
  }
}
