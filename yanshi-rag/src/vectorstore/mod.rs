//! Vector store abstractions and implementations

use crate::{
  error::Result,
  types::{CollectionConfig, Document, Filter, SearchResult},
};
use async_trait::async_trait;

#[cfg(feature = "qdrant")]
pub mod qdrant;

#[cfg(feature = "qdrant")]
pub use qdrant::{QdrantStore, QdrantStoreBuilder};

/// Vector store trait for semantic search operations
#[async_trait]
pub trait VectorStore: Send + Sync {
  /// Create a new collection with specified configuration
  async fn create_collection(&self, name: &str, config: CollectionConfig) -> Result<()>;

  /// Delete a collection
  async fn delete_collection(&self, name: &str) -> Result<()>;

  /// Check if a collection exists
  async fn collection_exists(&self, name: &str) -> Result<bool>;

  /// List all collections
  async fn list_collections(&self) -> Result<Vec<String>>;

  /// Add documents to a collection
  /// Returns the IDs of the added documents
  async fn add_documents(&self, collection: &str, docs: Vec<Document>) -> Result<Vec<String>>;

  /// Delete documents by IDs
  async fn delete_documents(&self, collection: &str, ids: Vec<String>) -> Result<()>;

  /// Perform similarity search
  async fn similarity_search(
    &self,
    collection: &str,
    query: &str,
    top_k: usize,
    filter: Option<Filter>,
  ) -> Result<Vec<SearchResult>>;

  /// Perform similarity search with pre-computed embedding
  async fn similarity_search_by_vector(
    &self,
    collection: &str,
    vector: Vec<f32>,
    top_k: usize,
    filter: Option<Filter>,
  ) -> Result<Vec<SearchResult>>;

  /// Get collection statistics
  async fn get_collection_stats(&self, collection: &str) -> Result<CollectionStats>;
}

/// Collection statistics
#[derive(Debug, Clone)]
pub struct CollectionStats {
  /// Collection name
  pub name: String,

  /// Number of documents
  pub document_count: usize,

  /// Vector dimension
  pub dimension: usize,

  /// Index size in bytes
  pub index_size_bytes: u64,
}
