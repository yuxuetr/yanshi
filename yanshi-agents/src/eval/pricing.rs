//! Per-model pricing. The table moved to [`yanshi_llm::pricing`] (Y2.3)
//! so the modality nodes, which cannot depend on this crate, price their
//! calls from the same file; this path re-exports it unchanged.

pub use yanshi_llm::pricing::{ModelPricing, PricingError, PricingTable};
