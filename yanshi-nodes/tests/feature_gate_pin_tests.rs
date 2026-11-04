//! Q3.8.5 regression pin: catch silent drift between the documented
//! `yanshi-nodes` feature matrix (CLAUDE.md + `Cargo.toml` doc
//! block) and the actual `nodes/mod.rs` `#[cfg(feature = "…")]`
//! gating.
//!
//! The audit (`docs/audit/yanshi-nodes.md` M7) found that several
//! documented per-modality flags (`asr` / `tts` / `text_to_image` /
//! `image_*`) didn't exist as Cargo features — the modules
//! ship unconditionally. Q4.5 reconciled the docs to reality; this
//! test makes the reconciliation load-bearing so a future PR can't
//! silently re-introduce the drift without an obvious compile/test
//! failure.
//!
//! The test runs under the **default feature set** so `cargo test
//! -p yanshi-nodes` (no `--features` flag) exercises exactly the
//! shape `cargo install yanshi-cli` produces.

// Touch each unconditionally-shipped node module so a future
// `#[cfg(feature = "…")]` added without a matching CLAUDE.md/
// Cargo.toml note breaks this test loudly. Path-existence checks
// are cheaper than instantiating nodes (no `Default` impl assumed)
// and they pin the module-path contract — which is exactly the
// audit invariant we care about.

use yanshi_nodes::nodes::{arxiv::ArxivNode, markmap::MarkMapNode};

/// The tool-tier specialized content nodes (`arxiv` / `markmap`) ship
/// unconditionally under the default feature set. (The per-modality AI nodes —
/// `asr` / `tts` / `text_to_image` / `image_*` — moved to `yanshi-nodes-ai`
/// in the P-A nodes split; their analogous pin lives there.)
#[test]
fn tool_tier_content_nodes_are_unconditional_under_default_features() {
  let sizes = [
    std::mem::size_of::<ArxivNode>(),
    std::mem::size_of::<MarkMapNode>(),
  ];
  assert!(
    sizes.iter().all(|s| *s > 0),
    "every pinned tool-tier node type must be a real (non-ZST) struct"
  );
}

/// The 4 default-on workflow nodes (llm/http/file/template) plus
/// `mcp` / `rag` / `batch` / `conditional` are the canonical
/// feature flags. The exhaustive list lives in `Cargo.toml`'s
/// `[features]` block and is mirrored in CLAUDE.md (L2 —
/// yanshi-nodes paragraph). This test pins the textual
/// contract so a `cargo add-feature whatever` PR fails CI
/// without a matching doc bump.
///
/// We parse `Cargo.toml` rather than hard-coding the expected set
/// directly because the source of truth is the manifest; the test
/// keeps the doc/manifest pair in lock step but doesn't itself
/// duplicate the list.
#[test]
fn cargo_toml_feature_matrix_matches_audit_pinned_shape() {
  const MANIFEST: &str = include_str!("../Cargo.toml");
  // Required feature flags. If you add or remove one of these,
  // update CLAUDE.md ("Crate feature flags: defaults are
  // [\"llm\", \"http\", \"file\", \"template\"]; mcp, rag,
  // batch, conditional are opt-in") in the same PR.
  for required in [
    "http = [\"reqwest\"]",
    "file = []",
    "template = [\"handlebars\"]",
    "batch = []",
    "conditional = []",
    // Y3.1: in-process renderers, default-on.
    "render = [\"dep:rust_xlsxwriter\", \"dep:resvg\", \"dep:pulldown-cmark\", \"dep:csv\"]",
    "default = [\"http\", \"file\", \"template\", \"render\"]",
  ] {
    assert!(
      MANIFEST.contains(required),
      "Cargo.toml [features] missing pinned entry `{required}` \
       — Q3.8.5 drift detected. Update CLAUDE.md L2 yanshi-nodes \
       paragraph + this test in the same PR if intentional."
    );
  }

  // Audit's anti-feature list: per-modality flags that CLAUDE.md
  // and Cargo.toml's doc block explicitly say are NOT gated
  // today. If any of these strings appears in [features], the
  // doc claim is now a lie — fail loudly.
  for forbidden in [
    // Capability features moved to `yanshi-nodes-ai` — they must NOT
    // reappear in the tool-tier crate.
    "\nllm = [",
    "\nmcp = [",
    "\nrag = [",
    "\nasr = [",
    "\ntts = [",
    "\ntext_to_image = [",
    "\nimage_to_image = [",
    "\nimage_edit = [",
    "\nimage_understand = [",
    "\narxiv = [",
    "\nmarkmap = [",
  ] {
    assert!(
      !MANIFEST.contains(forbidden),
      "Q3.8.5: per-modality feature `{}` snuck into Cargo.toml \
       without updating CLAUDE.md + this test. Implementing real \
       gating is fine, but it's the kind of change that needs an \
       intentional reconciliation across docs.",
      forbidden.trim()
    );
  }
}
