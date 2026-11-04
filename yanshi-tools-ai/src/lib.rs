//! Capability-backed `Tool` adapters for Yanshi agent runtimes.
//!
//! # Why this crate exists
//!
//! `yanshi-nodes-ai` already gives DAG workflows vendor-agnostic access
//! to TTS/ASR/image/video generation: each node (`TTSNode`, `AsrNode`, ...)
//! calls `yanshi_llm::Yanshi::tts(model)`/`text2image_for(model)`/etc.,
//! which resolves the model's vendor from the shared registry YAML
//! (`yanshi-llm/templates/models/*.yml`) and dispatches to that
//! vendor's implementation — all per-vendor request/response reconciliation
//! (StepFun's direct-bytes TTS response vs DashScope's submit-then-fetch-URL
//! two-step protocol, etc.) lives behind that dispatch, invisible to the
//! caller.
//!
//! Agent-native loops (`ReActAgent`), dynamic workflows
//! (`DynamicWorkflowAgent`), and Harness mode don't go through DAG nodes —
//! they call `yanshi_tool::Tool` implementations registered in a
//! `ToolRegistry`. Before this crate, none of the six modalities above were
//! reachable that way at all: the only DAG→Tool bridge (`WorkflowTool` in
//! `yanshi-agents`) wraps an entire `Flow`, not a single capability.
//!
//! Each tool here is a thin wrapper calling the exact same
//! `Yanshi::*` dispatch function the matching DAG node calls — same
//! model registry, same vendor reconciliation, zero duplicated vendor
//! logic. This crate is deliberately its own adapter crate (mirroring
//! `yanshi-nodes-ai`'s role for DAG nodes) rather than adding
//! `yanshi-llm` as a dependency of the tool-tier `yanshi-tools`
//! crate — that capability dependency was already extracted out of the
//! equivalent tool-tier `yanshi-nodes` crate once before (P-A0.5), into
//! `yanshi-nodes-ai`; this crate follows the same precedent.
//!
//! # Error-handling convention
//!
//! Two distinct failure classes, surfaced two different ways (mirrors
//! `HttpTool`'s convention of only using `Err` for tool-contract-level
//! failures):
//! - **Resolution failure** (bad `model` param, vendor doesn't implement
//!   this modality, missing API key) → `Err(ToolError::InvalidParams)`.
//!   From the caller's perspective this is the same class of mistake as a
//!   malformed parameter — naming a `model` that can't do what was asked —
//!   and actionable the same way: try a different `model`.
//! - **Vendor call failure** (the model resolved fine, but the actual
//!   HTTP call to the vendor failed — network error, rate limit, invalid
//!   voice id, content policy rejection, etc.) → `Ok(ToolOutput::error(...))`.
//!   The tool call itself succeeded as a *tool call*; the underlying
//!   operation just didn't produce a usable result. An agent can inspect
//!   the message and decide whether to retry, adjust parameters, or give up.
//!
//! # Usage
//!
//! ```ignore
//! use yanshi_tool::ToolRegistry;
//! use yanshi_tools_ai::register_all;
//!
//! let mut registry = ToolRegistry::new();
//! register_all(&mut registry);
//! // `registry` now has "tts", "asr", "text_to_image", "image_to_image",
//! // "image_edit", "image_understand", and "text_to_video" tools, each
//! // driven entirely by the model config YAML — no per-vendor code here.
//! ```

mod asr;
mod common;
mod cost;
mod image_edit;
mod image_to_image;
mod image_understand;
mod text_to_image;
mod text_to_video;
mod tts;

pub use asr::AsrTool;
pub use cost::estimate_cost_usd;
pub use image_edit::ImageEditTool;
pub use image_to_image::Image2ImageTool;
pub use image_understand::ImageUnderstandTool;
pub use text_to_image::Text2ImageTool;
pub use text_to_video::Text2VideoTool;
pub use tts::TtsTool;

use std::sync::Arc;

use yanshi_tool::ToolRegistry;

/// Register all seven modality tools this crate provides into `registry`.
///
/// Convenience for the common case of wanting every modality available;
/// callers that want a narrower set (e.g. only `tts` for a voice-assistant
/// skill) should register the individual `*Tool` types directly instead.
pub fn register_all(registry: &mut ToolRegistry) {
  registry.register(Arc::new(TtsTool::new()));
  registry.register(Arc::new(AsrTool::new()));
  registry.register(Arc::new(Text2ImageTool::new()));
  registry.register(Arc::new(Image2ImageTool::new()));
  registry.register(Arc::new(ImageEditTool::new()));
  registry.register(Arc::new(ImageUnderstandTool::new()));
  registry.register(Arc::new(Text2VideoTool::new()));
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn register_all_installs_every_modality_tool_under_its_stable_name() {
    let mut registry = ToolRegistry::new();
    register_all(&mut registry);
    let tools = registry.list();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    for expected in [
      "tts",
      "asr",
      "text_to_image",
      "image_to_image",
      "image_edit",
      "image_understand",
      "text_to_video",
    ] {
      assert!(
        names.contains(&expected),
        "expected registry to contain a \"{expected}\" tool, got: {names:?}"
      );
    }
  }
}
