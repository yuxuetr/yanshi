//! Tool-tier node implementations.
//!
//! Capability-backed nodes (LLM / audio / image / RAG / MCP) moved to
//! `yanshi-nodes-ai` in P-A (RFC_NODES_DECOMPOSITION); these tool-tier nodes
//! carry no capability dependencies.

#[cfg(feature = "http")]
pub mod http;

#[cfg(feature = "file")]
pub mod file;

#[cfg(feature = "template")]
pub mod template;

#[cfg(feature = "batch")]
pub mod batch;

#[cfg(feature = "conditional")]
pub mod conditional;

// Specialized content processing nodes (tool tier — no capability deps).
pub mod arxiv;

pub mod markmap;
#[cfg(feature = "render")]
pub mod render;
