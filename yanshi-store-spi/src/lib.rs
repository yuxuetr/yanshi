//! `yanshi-store-spi` — the storage contracts of the Yanshi kernel.
//!
//! This crate holds the *interfaces and data types* for the kernel's
//! storage axes:
//!
//! - **Conversation memory** — the [`MemoryStore`] trait,
//!   the [`Message`] / [`Role`] /
//!   [`TokenCounter`] types, and the shared
//!   [`MemoryError`]. The concrete stores (`SessionMemory`,
//!   `SqliteMemory`, `SemanticMemory`, …) remain in `yanshi-memory`, which
//!   re-exports everything here under its original paths.
//! - **Knowledge retrieval** — the [`KnowledgeBackend`]
//!   trait + [`KnowledgeChunk`] /
//!   [`KnowledgeError`] (RFC §9). The concrete
//!   backends (BM25 in-memory + vector store) live in `yanshi-rag`.
//! - **Task summary checkpoint** (L2.1) — the
//!   [`TaskSummaryStore`] trait +
//!   [`TaskSummary`], a structured "task
//!   narrative" checkpoint distinct from the raw message log, generated on
//!   memory compaction and surfaced again on the next run for the same
//!   session. Concrete stores live in `yanshi-memory`.
//! - **Project memory** (L3.1, U2.5) — the
//!   [`ProjectMemoryStore`] trait +
//!   [`ProjectFact`] + [`project_key_for_path`],
//!   durable facts about a project shared across every session that runs
//!   against it. Concrete stores (`InMemoryProjectMemoryStore` /
//!   `SqliteProjectMemoryStore`) live in `yanshi-memory`.
//! - **Preference** (U2.6) — the [`PreferenceStore`]
//!   trait + [`PreferenceScope`] /
//!   [`PreferenceValue`], durable per-user
//!   key/value storage. U2.2/U2.5 originally left this off `store-spi`
//!   because its write methods took `&mut self`; U2.6 found that
//!   constraint wasn't load-bearing (see `preference` module docs) and
//!   redesigned the trait to `&self`, matching `ProjectMemoryStore` /
//!   `TaskSummaryStore`. Concrete stores (`SqlitePreferenceStore` /
//!   `AgeEncryptedPreferenceStore`) live in `yanshi-memory`.
//!   `yanshi-agents` still carries a real (non-dev) `yanshi-memory`
//!   dependency regardless of this extraction — `dynamic.rs`'s
//!   `DynamicWorkflowAgent` constructs a concrete `SessionMemory` as the
//!   default memory backend for LLM-authored `agent` plan steps, which
//!   has no store-spi contract by design (it's a concrete impl, not a
//!   contract gap).
//!
//! Memory was extracted from `yanshi-memory` in P-A1.2 so that runtime/agent
//! contracts (`yanshi-agent-spi`) can depend on `Message` without depending
//! on the `memory` implementation crate (RFC §4 store-spi); the knowledge
//! contract (P-A4.1) follows the same pattern for `yanshi-skills` ⟷
//! `yanshi-rag`. The `EmbeddingProvider` contract (evaluation R6) is a
//! follow-up: it needs the rag/memory error surfaces unified first.

pub mod error;
pub mod knowledge;
pub mod preference;
pub mod project;
pub mod store;
pub mod task_summary;
pub mod types;

pub use error::MemoryError;
pub use knowledge::{KnowledgeBackend, KnowledgeChunk, KnowledgeError};
pub use preference::{PreferenceScope, PreferenceStore, PreferenceValue};
pub use project::{ProjectFact, ProjectMemoryStore, project_key_for_path};
pub use store::MemoryStore;
pub use task_summary::{TaskSummary, TaskSummaryStore};
pub use types::{HeuristicCounter, Message, Role, TokenCounter};
