//! Paragraph-based chunking strategy (L4.1).
//!
//! Never splits a paragraph across two chunks — paragraphs (runs of
//! non-blank lines separated by blank lines) are the atomic unit. Small
//! consecutive paragraphs are merged up to `chunk_size`; a single paragraph
//! larger than `chunk_size` becomes its own (oversized) chunk rather than
//! being cut mid-thought. Every chunk carries `start_line` / `end_line`
//! (1-indexed, inclusive) metadata computed directly during the paragraph
//! scan, so it stays correct regardless of the char/byte offset conventions
//! used by the other chunkers.

use crate::{
  chunking::ChunkingStrategy,
  error::{RAGError, Result},
  types::{MetadataValue, TextChunk},
};
use std::collections::HashMap;

struct Paragraph {
  content: String,
  start_line: usize,
  end_line: usize,
}

fn split_paragraphs(text: &str) -> Vec<Paragraph> {
  let mut paragraphs = Vec::new();
  let mut current_lines: Vec<&str> = Vec::new();
  let mut current_start: usize = 0;

  for (idx, line) in text.lines().enumerate() {
    let line_no = idx + 1;
    if line.trim().is_empty() {
      if !current_lines.is_empty() {
        paragraphs.push(Paragraph {
          content: current_lines.join("\n"),
          start_line: current_start,
          end_line: current_start + current_lines.len() - 1,
        });
        current_lines.clear();
      }
      continue;
    }
    if current_lines.is_empty() {
      current_start = line_no;
    }
    current_lines.push(line);
  }
  if !current_lines.is_empty() {
    paragraphs.push(Paragraph {
      content: current_lines.join("\n"),
      start_line: current_start,
      end_line: current_start + current_lines.len() - 1,
    });
  }
  paragraphs
}

fn push_group(paragraphs: &[Paragraph], group: &[usize], chunks: &mut Vec<TextChunk>) {
  let content = group
    .iter()
    .map(|&i| paragraphs[i].content.as_str())
    .collect::<Vec<_>>()
    .join("\n\n");
  let start_line = paragraphs[group[0]].start_line;
  // Both call sites gate on `!group.is_empty()` before calling `push_group`
  // (the `group[0]` index above already relies on the same invariant
  // unconditionally), so `group.last()` is always `Some` here.
  #[allow(
    clippy::expect_used,
    reason = "callers only invoke push_group with a non-empty group; group[0] above already assumes it"
  )]
  let end_line = paragraphs[*group.last().expect("group is non-empty")].end_line;

  let mut metadata = HashMap::new();
  metadata.insert(
    "start_line".to_string(),
    MetadataValue::Integer(start_line as i64),
  );
  metadata.insert(
    "end_line".to_string(),
    MetadataValue::Integer(end_line as i64),
  );

  chunks.push(TextChunk {
    content,
    start_idx: start_line,
    end_idx: end_line,
    metadata,
    chunk_index: chunks.len(),
    total_chunks: 0,
  });
}

/// Seed the next group with trailing paragraphs from `group` whose combined
/// length fits within `overlap`, so consecutive chunks share context.
///
/// A trailing paragraph longer than `overlap` contributes nothing: it used
/// to be carried over whole, duplicating it into the next chunk and
/// pushing that chunk past `chunk_size`.
fn overlap_seed(paragraphs: &[Paragraph], group: &[usize], overlap: usize) -> Vec<usize> {
  if overlap == 0 {
    return Vec::new();
  }
  let mut seed = Vec::new();
  let mut len = 0usize;
  for &idx in group.iter().rev() {
    let addition = paragraphs[idx].content.len() + if seed.is_empty() { 0 } else { 2 };
    if len + addition > overlap {
      break;
    }
    seed.push(idx);
    len += addition;
  }
  seed.reverse();
  seed
}

pub struct ParagraphChunker {
  chunk_size: usize,
  overlap: usize,
}

impl ParagraphChunker {
  /// Infallible constructor; clamps like [`crate::chunking::FixedSizeChunker::new`].
  pub fn new(chunk_size: usize, overlap: usize) -> Self {
    let chunk_size = chunk_size.max(1);
    let overlap = overlap.min(chunk_size.saturating_sub(1));
    Self {
      chunk_size,
      overlap,
    }
  }

  /// Fallible constructor for config-driven call sites.
  pub fn try_new(chunk_size: usize, overlap: usize) -> Result<Self> {
    if chunk_size == 0 {
      return Err(RAGError::chunking(
        "ParagraphChunker requires chunk_size > 0",
      ));
    }
    if overlap >= chunk_size {
      return Err(RAGError::chunking(format!(
        "ParagraphChunker requires overlap < chunk_size; got overlap={overlap} chunk_size={chunk_size}"
      )));
    }
    Ok(Self {
      chunk_size,
      overlap,
    })
  }
}

impl ChunkingStrategy for ParagraphChunker {
  fn chunk(&self, text: &str) -> Result<Vec<TextChunk>> {
    let paragraphs = split_paragraphs(text);
    if paragraphs.is_empty() {
      return Ok(Vec::new());
    }

    let mut chunks: Vec<TextChunk> = Vec::new();
    let mut group: Vec<usize> = Vec::new();
    let mut group_len = 0usize;

    for idx in 0..paragraphs.len() {
      let addition = paragraphs[idx].content.len() + if group.is_empty() { 0 } else { 2 };
      if !group.is_empty() && group_len + addition > self.chunk_size {
        push_group(&paragraphs, &group, &mut chunks);
        group = overlap_seed(&paragraphs, &group, self.overlap);
        group_len = group
          .iter()
          .map(|&i| paragraphs[i].content.len())
          .sum::<usize>()
          + group.len().saturating_sub(1) * 2;
      }
      let addition = paragraphs[idx].content.len() + if group.is_empty() { 0 } else { 2 };
      group.push(idx);
      group_len += addition;
    }
    if !group.is_empty() {
      push_group(&paragraphs, &group, &mut chunks);
    }

    let total = chunks.len();
    for c in &mut chunks {
      c.total_chunks = total;
    }
    Ok(chunks)
  }

  fn chunk_size(&self) -> usize {
    self.chunk_size
  }

  fn overlap(&self) -> usize {
    self.overlap
  }

  fn strategy_name(&self) -> &str {
    "paragraph"
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_paragraph_longer_than_the_overlap_is_not_carried_into_the_next_chunk() {
    let text = format!("{}\n\n{}", "a".repeat(800), "b".repeat(800));
    let chunks = ParagraphChunker::new(1200, 150)
      .chunk(&text)
      .expect("chunks");
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].content, "a".repeat(800));
    assert_eq!(
      chunks[1].content,
      "b".repeat(800),
      "no 800-char duplicate as 'overlap'"
    );
    assert!(chunks.iter().all(|c| c.content.len() <= 1200));
  }

  #[test]
  fn short_trailing_paragraphs_still_overlap() {
    let text = format!("{}\n\nshort tail\n\n{}", "a".repeat(700), "b".repeat(700));
    let chunks = ParagraphChunker::new(1000, 50)
      .chunk(&text)
      .expect("chunks");
    assert!(
      chunks[1].content.starts_with("short tail"),
      "{:?}",
      chunks[1].content
    );
  }

  #[test]
  fn never_splits_a_paragraph_across_two_chunks() {
    let chunker = ParagraphChunker::new(20, 0);
    let text = "Short one.\n\nThis second paragraph is deliberately longer than the chunk size limit.\n\nShort two.";
    let chunks = chunker.chunk(text).expect("chunk ok");
    for chunk in &chunks {
      // Every chunk's content must appear verbatim as a whole paragraph
      // (or concatenation of whole paragraphs) in the source text.
      assert!(text.contains(chunk.content.trim()) || chunk.content.contains("second paragraph"));
    }
    // The long paragraph should show up as its own oversized chunk, never
    // truncated mid-sentence.
    assert!(chunks.iter().any(
      |c| c.content.contains("deliberately longer") && c.content.ends_with("chunk size limit.")
    ));
  }

  #[test]
  fn merges_small_consecutive_paragraphs() {
    let chunker = ParagraphChunker::new(200, 0);
    let text = "Para one.\n\nPara two.\n\nPara three.";
    let chunks = chunker.chunk(text).expect("chunk ok");
    assert_eq!(
      chunks.len(),
      1,
      "small paragraphs should merge into one chunk"
    );
    assert!(chunks[0].content.contains("Para one."));
    assert!(chunks[0].content.contains("Para three."));
  }

  #[test]
  fn records_start_and_end_line_metadata() {
    let chunker = ParagraphChunker::new(5, 0);
    let text = "line1\nline2\n\nline4\nline5";
    let chunks = chunker.chunk(text).expect("chunk ok");
    assert_eq!(chunks.len(), 2);
    assert_eq!(
      chunks[0].metadata.get("start_line"),
      Some(&MetadataValue::Integer(1))
    );
    assert_eq!(
      chunks[0].metadata.get("end_line"),
      Some(&MetadataValue::Integer(2))
    );
    assert_eq!(
      chunks[1].metadata.get("start_line"),
      Some(&MetadataValue::Integer(4))
    );
    assert_eq!(
      chunks[1].metadata.get("end_line"),
      Some(&MetadataValue::Integer(5))
    );
  }

  #[test]
  fn empty_text_yields_no_chunks() {
    let chunker = ParagraphChunker::new(100, 0);
    assert!(chunker.chunk("").expect("chunk ok").is_empty());
    assert!(chunker.chunk("\n\n\n").expect("chunk ok").is_empty());
  }

  #[test]
  fn try_new_rejects_bad_inputs() {
    assert!(ParagraphChunker::try_new(0, 0).is_err());
    assert!(ParagraphChunker::try_new(100, 100).is_err());
    assert!(ParagraphChunker::try_new(100, 20).is_ok());
  }
}
