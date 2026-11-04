//! RAG (Retrieval-Augmented Generation) integration node
//!
//! This node enables Yanshi workflows to perform RAG operations including:
//! - Semantic search across indexed documents
//! - Hybrid search (semantic + keyword)
//! - Document indexing and management
//!
//! # Example Usage
//!
//! ## Search Operation
//! ```yaml
//! nodes:
//!   - id: search_docs
//!     type: rag
//!     parameters:
//!       operation: search
//!       qdrant_url: "http://localhost:6334"
//!       collection: "my_docs"
//!       query: "{{user_question}}"
//!       top_k: 5
//!       search_type: semantic  # or "hybrid", "keyword"
//! ```
//!
//! ## Index Operation
//! ```yaml
//! nodes:
//!   - id: index_docs
//!     type: rag
//!     parameters:
//!       operation: index
//!       qdrant_url: "http://localhost:6334"
//!       collection: "my_docs"
//!       documents:
//!         - content: "First document"
//!           metadata:
//!             source: "user_input"
//!         - content: "Second document"
//! ```

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};

#[cfg(feature = "rag")]
use yanshi_rag::{
  embeddings::OpenAIEmbedding,
  retrieval::{bm25::BM25Retriever, hybrid::HybridRetriever},
  types::{CollectionConfig, DistanceMetric, Document},
  vectorstore::{QdrantStore, VectorStore},
};

/// RAG Node for retrieval-augmented generation operations
#[derive(Debug, Clone)]
pub struct RAGNode {
  /// Operation type (search, index, delete, etc.)
  pub operation: String,

  /// Qdrant server URL
  pub qdrant_url: String,

  /// Collection name
  pub collection: String,

  /// OpenAI embedding model (default: text-embedding-3-small)
  pub embedding_model: String,
}

impl Default for RAGNode {
  fn default() -> Self {
    Self {
      operation: "search".to_string(),
      qdrant_url: "http://localhost:6334".to_string(),
      collection: String::new(),
      embedding_model: "text-embedding-3-small".to_string(),
    }
  }
}

impl RAGNode {
  /// Create a new RAG node with specified parameters
  pub fn new(operation: impl Into<String>, collection: impl Into<String>) -> Self {
    Self {
      operation: operation.into(),
      collection: collection.into(),
      ..Default::default()
    }
  }

  /// Set Qdrant URL
  pub fn with_qdrant_url(mut self, url: impl Into<String>) -> Self {
    self.qdrant_url = url.into();
    self
  }

  /// Set embedding model
  pub fn with_embedding_model(mut self, model: impl Into<String>) -> Self {
    self.embedding_model = model.into();
    self
  }
}

#[async_trait]
impl AsyncNode for RAGNode {
  #[cfg(not(feature = "rag"))]
  async fn execute(&self, _inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    Err(YanshiError::ConfigurationError {
      message: "RAG feature not enabled. Enable with --features rag".to_string(),
    })
  }

  #[cfg(feature = "rag")]
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    // Extract parameters
    let operation = get_optional_string_input(inputs, "operation")?.unwrap_or(&self.operation);

    let qdrant_url = get_optional_string_input(inputs, "qdrant_url")?.unwrap_or(&self.qdrant_url);

    let collection = get_optional_string_input(inputs, "collection")?.unwrap_or(&self.collection);

    if collection.is_empty() {
      return Err(YanshiError::NodeInputError {
        message: "Collection name is required".to_string(),
      });
    }

    tracing::debug!(operation = %operation, collection = %collection, "RAG operation");

    match operation {
      "search" => self.execute_search(inputs, qdrant_url, collection).await,
      "index" => self.execute_index(inputs, qdrant_url, collection).await,
      "create_collection" => {
        self
          .execute_create_collection(inputs, qdrant_url, collection)
          .await
      }
      "delete_collection" => {
        self
          .execute_delete_collection(inputs, qdrant_url, collection)
          .await
      }
      "stats" => self.execute_stats(inputs, qdrant_url, collection).await,
      _ => Err(YanshiError::NodeInputError {
        message: format!("Unknown RAG operation: {}", operation),
      }),
    }
  }
}

#[cfg(feature = "rag")]
impl RAGNode {
  /// Execute search operation
  async fn execute_search(
    &self,
    inputs: &AsyncNodeInputs,
    qdrant_url: &str,
    collection: &str,
  ) -> AsyncNodeResult {
    use std::sync::Arc;

    // Get query
    let query = get_string_input(inputs, "query")?;

    // Get search parameters
    let top_k = get_optional_usize_input(inputs, "top_k")?.unwrap_or(5);
    let search_type = get_optional_string_input(inputs, "search_type")?.unwrap_or("semantic");

    // Create embedding provider
    let embedding_model =
      get_optional_string_input(inputs, "embedding_model")?.unwrap_or(&self.embedding_model);

    let embedder = Arc::new(OpenAIEmbedding::new(embedding_model).map_err(|e| {
      YanshiError::ConfigurationError {
        message: format!("Failed to create embedding provider: {}", e),
      }
    })?);

    // Connect to Qdrant
    let store = QdrantStore::with_embedding_provider(qdrant_url, embedder)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to connect to Qdrant: {}", e),
      })?;

    tracing::debug!(search_type = %search_type, top_k, "RAG search");

    // Perform search based on type
    let results = match search_type {
      "semantic" => store
        .similarity_search(collection, query, top_k, None)
        .await
        .map_err(|e| YanshiError::AsyncExecutionError {
          message: format!("Semantic search failed: {}", e),
        })?,
      "hybrid" => {
        // For hybrid search, we need both semantic and keyword results
        let semantic_results = store
          .similarity_search(collection, query, top_k * 2, None)
          .await
          .map_err(|e| YanshiError::AsyncExecutionError {
            message: format!("Semantic search failed: {}", e),
          })?;

        // Create hybrid retriever and add documents from collection
        // Note: In production, BM25 index should be pre-built
        let mut hybrid = HybridRetriever::new();
        for result in &semantic_results {
          hybrid.add_document(&result.id, &result.content);
        }

        let alpha = get_optional_f64_input(inputs, "alpha")?.unwrap_or(0.5);
        hybrid.search(semantic_results, query, top_k, alpha as f32)
      }
      "keyword" => {
        // Pure keyword search using BM25
        // Note: This requires pre-indexed documents
        let mut bm25 = BM25Retriever::new();

        // For demo, fetch some documents first
        let docs = store
          .similarity_search(collection, query, top_k * 3, None)
          .await
          .map_err(|e| YanshiError::AsyncExecutionError {
            message: format!("Document fetch failed: {}", e),
          })?;

        for doc in &docs {
          bm25.add_document(&doc.id, &doc.content);
        }

        bm25.search(query, top_k)
      }
      _ => {
        return Err(YanshiError::NodeInputError {
          message: format!("Unknown search type: {}", search_type),
        });
      }
    };

    tracing::debug!(result_count = results.len(), "RAG search complete");

    // Convert results to JSON
    let results_json =
      serde_json::to_value(&results).map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to serialize results: {}", e),
      })?;

    let mut outputs = HashMap::new();
    outputs.insert("results".to_string(), FlowValue::Json(results_json.clone()));
    outputs.insert("count".to_string(), FlowValue::Json(json!(results.len())));
    // Y5.3: the same hits in the unified citation shape
    // (`yanshi_config::content::Citation`), so a generator can cite them.
    outputs.insert(
      "citations".to_string(),
      FlowValue::Json(Value::Array(results.iter().map(citation_of).collect())),
    );

    // Also provide first result content as a convenience
    if !results.is_empty() {
      outputs.insert(
        "first_result".to_string(),
        FlowValue::Json(json!(results[0].content)),
      );
    }

    Ok(outputs)
  }

  /// Execute index operation
  async fn execute_index(
    &self,
    inputs: &AsyncNodeInputs,
    qdrant_url: &str,
    collection: &str,
  ) -> AsyncNodeResult {
    use std::sync::Arc;

    // Get documents to index
    let documents_json = get_json_input(inputs, "documents")?;

    let documents: Vec<Document> = if documents_json.is_array() {
      serde_json::from_value(documents_json.clone()).map_err(|e| YanshiError::NodeInputError {
        message: format!("Invalid documents format: {}", e),
      })?
    } else if let Some(s) = documents_json.as_str() {
      // Single string document — match-via-as_str avoids the
      // `is_string() + as_str().unwrap()` redundancy (Q5.1).
      vec![Document::new(s)]
    } else {
      return Err(YanshiError::NodeInputError {
        message: "documents must be an array or string".to_string(),
      });
    };

    tracing::debug!(document_count = documents.len(), "indexing documents");

    // Create embedding provider
    let embedding_model =
      get_optional_string_input(inputs, "embedding_model")?.unwrap_or(&self.embedding_model);

    let embedder = Arc::new(OpenAIEmbedding::new(embedding_model).map_err(|e| {
      YanshiError::ConfigurationError {
        message: format!("Failed to create embedding provider: {}", e),
      }
    })?);

    // Connect to Qdrant
    let store = QdrantStore::with_embedding_provider(qdrant_url, embedder)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to connect to Qdrant: {}", e),
      })?;

    // Add documents
    let ids = store
      .add_documents(collection, documents)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to index documents: {}", e),
      })?;

    tracing::debug!(indexed_count = ids.len(), "documents indexed");

    let mut outputs = HashMap::new();
    outputs.insert("ids".to_string(), FlowValue::Json(json!(ids)));
    outputs.insert("count".to_string(), FlowValue::Json(json!(ids.len())));

    Ok(outputs)
  }

  /// Execute create_collection operation
  async fn execute_create_collection(
    &self,
    inputs: &AsyncNodeInputs,
    qdrant_url: &str,
    collection: &str,
  ) -> AsyncNodeResult {
    use std::sync::Arc;

    tracing::debug!(collection = %collection, "creating collection");

    // Get collection parameters
    let dimension = get_optional_usize_input(inputs, "dimension")?.unwrap_or(1536); // OpenAI default

    let distance = get_optional_string_input(inputs, "distance")?
      .unwrap_or("cosine")
      .to_lowercase();

    let distance_metric = match distance.as_str() {
      "cosine" => DistanceMetric::Cosine,
      "euclidean" => DistanceMetric::Euclidean,
      "dot" => DistanceMetric::Dot,
      _ => {
        return Err(YanshiError::NodeInputError {
          message: format!("Unknown distance metric: {}", distance),
        });
      }
    };

    // Create embedding provider (needed for QdrantStore)
    let embedding_model =
      get_optional_string_input(inputs, "embedding_model")?.unwrap_or(&self.embedding_model);

    let embedder = Arc::new(OpenAIEmbedding::new(embedding_model).map_err(|e| {
      YanshiError::ConfigurationError {
        message: format!("Failed to create embedding provider: {}", e),
      }
    })?);

    let store = QdrantStore::with_embedding_provider(qdrant_url, embedder)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to connect to Qdrant: {}", e),
      })?;

    let config = CollectionConfig {
      dimension,
      distance: distance_metric,
      index_config: None,
    };

    store
      .create_collection(collection, config)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to create collection: {}", e),
      })?;

    tracing::debug!(collection = %collection, "collection created");

    let mut outputs = HashMap::new();
    outputs.insert("success".to_string(), FlowValue::Json(json!(true)));
    outputs.insert("collection".to_string(), FlowValue::Json(json!(collection)));

    Ok(outputs)
  }

  /// Execute delete_collection operation
  async fn execute_delete_collection(
    &self,
    _inputs: &AsyncNodeInputs,
    qdrant_url: &str,
    collection: &str,
  ) -> AsyncNodeResult {
    use std::sync::Arc;

    tracing::debug!(collection = %collection, "deleting collection");

    // Create a minimal embedder just for connection
    let embedder = Arc::new(OpenAIEmbedding::new(&self.embedding_model).map_err(|e| {
      YanshiError::ConfigurationError {
        message: format!("Failed to create embedding provider: {}", e),
      }
    })?);

    let store = QdrantStore::with_embedding_provider(qdrant_url, embedder)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to connect to Qdrant: {}", e),
      })?;

    store
      .delete_collection(collection)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to delete collection: {}", e),
      })?;

    tracing::debug!(collection = %collection, "collection deleted");

    let mut outputs = HashMap::new();
    outputs.insert("success".to_string(), FlowValue::Json(json!(true)));

    Ok(outputs)
  }

  /// Execute stats operation
  async fn execute_stats(
    &self,
    _inputs: &AsyncNodeInputs,
    qdrant_url: &str,
    collection: &str,
  ) -> AsyncNodeResult {
    use std::sync::Arc;

    tracing::debug!(collection = %collection, "getting collection stats");

    let embedder = Arc::new(OpenAIEmbedding::new(&self.embedding_model).map_err(|e| {
      YanshiError::ConfigurationError {
        message: format!("Failed to create embedding provider: {}", e),
      }
    })?);

    let store = QdrantStore::with_embedding_provider(qdrant_url, embedder)
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to connect to Qdrant: {}", e),
      })?;

    let stats = store.get_collection_stats(collection).await.map_err(|e| {
      YanshiError::AsyncExecutionError {
        message: format!("Failed to get stats: {}", e),
      }
    })?;

    tracing::debug!(
      document_count = stats.document_count,
      dimension = stats.dimension,
      "collection stats"
    );

    let stats_json = json!({
      "name": stats.name,
      "document_count": stats.document_count,
      "dimension": stats.dimension,
      "index_size_bytes": stats.index_size_bytes,
    });

    let mut outputs = HashMap::new();
    outputs.insert("stats".to_string(), FlowValue::Json(stats_json));
    outputs.insert(
      "document_count".to_string(),
      FlowValue::Json(json!(stats.document_count)),
    );

    Ok(outputs)
  }
}

// Helper functions for input extraction

fn get_string_input<'a>(inputs: &'a AsyncNodeInputs, key: &str) -> Result<&'a str, YanshiError> {
  inputs
    .get(key)
    .and_then(|v| match v {
      FlowValue::Json(Value::String(s)) => Some(s.as_str()),
      _ => None,
    })
    .ok_or_else(|| YanshiError::NodeInputError {
      message: format!(
        "Required string input '{}' is missing or has wrong type",
        key
      ),
    })
}

fn get_optional_string_input<'a>(
  inputs: &'a AsyncNodeInputs,
  key: &str,
) -> Result<Option<&'a str>, YanshiError> {
  match inputs.get(key) {
    None => Ok(None),
    Some(v) => match v {
      FlowValue::Json(Value::String(s)) => Ok(Some(s.as_str())),
      _ => Err(YanshiError::NodeInputError {
        message: format!("Input '{}' has wrong type, expected a string", key),
      }),
    },
  }
}

fn get_optional_usize_input(
  inputs: &AsyncNodeInputs,
  key: &str,
) -> Result<Option<usize>, YanshiError> {
  match inputs.get(key) {
    None => Ok(None),
    Some(v) => match v {
      FlowValue::Json(Value::Number(n)) => Ok(n.as_u64().map(|u| u as usize)),
      _ => Err(YanshiError::NodeInputError {
        message: format!("Input '{}' has wrong type, expected a number", key),
      }),
    },
  }
}

fn get_optional_f64_input(inputs: &AsyncNodeInputs, key: &str) -> Result<Option<f64>, YanshiError> {
  match inputs.get(key) {
    None => Ok(None),
    Some(v) => match v {
      FlowValue::Json(Value::Number(n)) => Ok(n.as_f64()),
      _ => Err(YanshiError::NodeInputError {
        message: format!("Input '{}' has wrong type, expected a number", key),
      }),
    },
  }
}

fn get_json_input<'a>(inputs: &'a AsyncNodeInputs, key: &str) -> Result<&'a Value, YanshiError> {
  inputs
    .get(key)
    .and_then(|v| match v {
      FlowValue::Json(json_val) => Some(json_val),
      _ => None,
    })
    .ok_or_else(|| YanshiError::NodeInputError {
      message: format!("Required JSON input '{}' is missing or has wrong type", key),
    })
}

/// One search hit as `{source_id, chunk_id, span?, quote}`: the hit's id
/// is the chunk id; the source and character span come from metadata
/// (`source_id` or `source`; `start` / `end` or `start_line` / `end_line`)
/// when the indexer recorded them.
fn citation_of(hit: &yanshi_rag::types::SearchResult) -> Value {
  use yanshi_rag::types::MetadataValue;
  let text = |key: &str| match hit.metadata.get(key) {
    Some(MetadataValue::String(s)) => Some(s.clone()),
    Some(MetadataValue::Integer(i)) => Some(i.to_string()),
    _ => None,
  };
  let int = |key: &str| match hit.metadata.get(key) {
    Some(MetadataValue::Integer(i)) => Some(*i),
    _ => None,
  };
  let mut citation = json!({
    "source_id": text("source_id").or_else(|| text("source")).unwrap_or_default(),
    "chunk_id": hit.id,
    "quote": hit.content,
  });
  if let (Some(start), Some(end)) = (
    int("start").or_else(|| int("start_line")),
    int("end").or_else(|| int("end_line")),
  ) {
    citation["span"] = json!({ "start": start, "end": end });
  }
  citation
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn search_hits_become_unified_citations() {
    use yanshi_rag::types::{MetadataValue, SearchResult};
    let hit = SearchResult {
      id: "src-1:3".to_string(),
      content: "Tides follow the moon.".to_string(),
      score: 0.9,
      metadata: HashMap::from([
        (
          "source".to_string(),
          MetadataValue::String("src-1".to_string()),
        ),
        ("start_line".to_string(), MetadataValue::Integer(4)),
        ("end_line".to_string(), MetadataValue::Integer(6)),
      ]),
    };
    assert_eq!(
      citation_of(&hit),
      json!({"source_id": "src-1", "chunk_id": "src-1:3", "quote": "Tides follow the moon.", "span": {"start": 4, "end": 6}})
    );
  }
  use serde_json::json;

  #[test]
  fn test_rag_node_creation() {
    let node = RAGNode::new("search", "test_collection");
    assert_eq!(node.operation, "search");
    assert_eq!(node.collection, "test_collection");
  }

  #[test]
  fn test_rag_node_builder() {
    let node = RAGNode::new("search", "test")
      .with_qdrant_url("http://localhost:6334")
      .with_embedding_model("text-embedding-3-large");

    assert_eq!(node.qdrant_url, "http://localhost:6334");
    assert_eq!(node.embedding_model, "text-embedding-3-large");
  }

  #[cfg(not(feature = "rag"))]
  #[tokio::test]
  async fn test_rag_feature_not_enabled() {
    let node = RAGNode::default();
    let inputs = AsyncNodeInputs::new();

    let result = node.execute(&inputs).await;
    assert!(result.is_err());
    assert!(
      result
        .unwrap_err()
        .to_string()
        .contains("RAG feature not enabled")
    );
  }

  #[test]
  fn test_helper_get_optional_string() {
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("test".to_string(), FlowValue::Json(json!("value")));

    let result = get_optional_string_input(&inputs, "test").unwrap();
    assert_eq!(result, Some("value"));

    let missing = get_optional_string_input(&inputs, "missing").unwrap();
    assert_eq!(missing, None);
  }

  #[test]
  fn test_helper_get_optional_usize() {
    let mut inputs = AsyncNodeInputs::new();
    inputs.insert("num".to_string(), FlowValue::Json(json!(42)));

    let result = get_optional_usize_input(&inputs, "num").unwrap();
    assert_eq!(result, Some(42));
  }
}
