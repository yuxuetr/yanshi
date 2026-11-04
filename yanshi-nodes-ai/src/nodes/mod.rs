//! Capability-backed `AsyncNode` adapters (LLM / audio / image / RAG / MCP).
//!
//! Split out of `yanshi-nodes` in P-A (RFC_NODES_DECOMPOSITION) so the
//! tool-tier `yanshi-nodes` crate carries no capability dependencies and
//! `yanshi-worker` can compile the tool nodes without dragging in
//! `yanshi-llm` / `yanshi-rag` / `yanshi-mcp`.

pub mod figure;
pub mod llm;

pub mod image_edit;
pub mod image_to_image;
pub mod image_understand;
pub mod text_to_image;
pub mod text_to_video;

pub mod asr;
pub mod tts;

#[cfg(feature = "mcp")]
pub mod mcp;

#[cfg(feature = "rag")]
pub mod rag;
