//! The generator output contract (`docs/RFC_CONTENT_STUDIO.md` §4.3).
//!
//! Every generator template ends in one `Deliverable`: structured content
//! of a known `kind`, the quotes it rests on, and the files rendered from
//! it. CLI and server parse a run's final JSON into this type so a
//! template that drifts from the contract fails loudly instead of handing
//! a client something it cannot read.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a generator produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deliverable {
  pub kind: DeliverableKind,
  pub title: String,
  /// Kind-specific structured content (sections for a report, a tree for
  /// a mind map, a speaker-tagged script for an audio overview, ...).
  pub content: Value,
  #[serde(default)]
  pub citations: Vec<Citation>,
  #[serde(default)]
  pub files: Vec<DeliverableFile>,
}

/// The deliverable catalogue (RFC §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverableKind {
  Report,
  Mindmap,
  AudioOverview,
  Slides,
  Flashcards,
  Quiz,
  Infographic,
  DataTable,
  VideoOverview,
  Figure,
}

/// One quote a deliverable relies on. `source_id` / `chunk_id` refer to
/// notebook sources and chunks once those exist (Y5); until then a
/// generator fed a URL or pasted text uses the URL (or `"text"`) and an
/// empty `chunk_id`. `quote` keeps the deliverable verifiable without
/// the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Citation {
  pub source_id: String,
  #[serde(default)]
  pub chunk_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub span: Option<Span>,
  pub quote: String,
}

/// Character range of a quote inside its chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
  pub start: usize,
  pub end: usize,
}

/// An artifact rendered for the deliverable, by the node that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliverableFile {
  pub node_id: String,
  pub name: String,
  pub mime_type: String,
  pub role: FileRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRole {
  Primary,
  Preview,
  Asset,
}

impl Deliverable {
  /// Parse and check a template's `deliverable` output: the shape (via
  /// serde, unknown fields rejected), a non-empty title, non-empty
  /// quotes, and object content.
  pub fn from_value(value: Value) -> Result<Self, String> {
    let deliverable: Self =
      serde_json::from_value(value).map_err(|e| format!("not a Deliverable: {e}"))?;
    if deliverable.title.trim().is_empty() {
      return Err("deliverable title is empty".to_string());
    }
    if !deliverable.content.is_object() {
      return Err("deliverable content must be a JSON object".to_string());
    }
    if let Some(empty) = deliverable
      .citations
      .iter()
      .position(|c| c.quote.trim().is_empty())
    {
      return Err(format!("citation {empty} has an empty quote"));
    }
    Ok(deliverable)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  fn sample() -> Value {
    json!({
      "kind": "report",
      "title": "Tides",
      "content": {"sections": []},
      "citations": [{"source_id": "text", "chunk_id": "", "quote": "Tides follow the moon."}],
      "files": [{"node_id": "report_md", "name": "1.md", "mime_type": "text/markdown", "role": "primary"}]
    })
  }

  #[test]
  fn a_report_deliverable_parses() {
    let d = Deliverable::from_value(sample()).expect("valid");
    assert_eq!(d.kind, DeliverableKind::Report);
    assert_eq!(d.files[0].role, FileRole::Primary);
  }

  #[test]
  fn drift_from_the_contract_is_rejected() {
    let mut unknown_kind = sample();
    unknown_kind["kind"] = json!("essay");
    assert!(Deliverable::from_value(unknown_kind).is_err());
    let mut extra_field = sample();
    extra_field["extra"] = json!(1);
    assert!(Deliverable::from_value(extra_field).is_err());
    let mut empty_quote = sample();
    empty_quote["citations"][0]["quote"] = json!(" ");
    assert!(Deliverable::from_value(empty_quote).is_err());
    let mut text_content = sample();
    text_content["content"] = json!("just text");
    assert!(Deliverable::from_value(text_content).is_err());
  }
}
