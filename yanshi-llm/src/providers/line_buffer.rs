//! Byte buffer for newline-delimited streaming bodies (SSE / NDJSON).

/// Decodes only complete lines. A network chunk can end in the middle of a
/// multi-byte UTF-8 character; decoding each chunk on its own turned such
/// characters (common in CJK output) into U+FFFD (eval 2026-10-07 L2).
/// `\n` never occurs inside a multi-byte sequence, so splitting on it
/// before decoding is exact.
#[derive(Debug, Default)]
pub(crate) struct LineBuffer {
  bytes: Vec<u8>,
}

impl LineBuffer {
  pub(crate) fn push(&mut self, chunk: &[u8]) {
    self.bytes.extend_from_slice(chunk);
  }

  /// The next complete line, trimmed; `None` until a `\n` has arrived.
  pub(crate) fn next_line(&mut self) -> Option<String> {
    let end = self.bytes.iter().position(|byte| *byte == b'\n')?;
    let line: Vec<u8> = self.bytes.drain(..=end).collect();
    Some(String::from_utf8_lossy(&line).trim().to_string())
  }
}

/// True for the SSE `data: [DONE]` sentinel that ends an OpenAI-shaped
/// stream. Readers stop here, not at the first `finish_reason`: with
/// `stream_options.include_usage` the usage arrives in a separate frame
/// *after* the finish chunk (Y2.1 / COR-1).
pub(crate) fn is_sse_done(line: &str) -> bool {
  line
    .strip_prefix("data: ")
    .is_some_and(|data| data.trim() == "[DONE]")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn multibyte_character_split_across_chunks_survives() {
    let line = "data: 你好\n".as_bytes();
    // Split inside the 3-byte encoding of '你'.
    let (first, second) = line.split_at(7);
    let mut buffer = LineBuffer::default();
    buffer.push(first);
    assert_eq!(buffer.next_line(), None);
    buffer.push(second);
    assert_eq!(buffer.next_line().as_deref(), Some("data: 你好"));
    assert_eq!(buffer.next_line(), None);
  }

  #[test]
  fn yields_every_buffered_line_in_order() {
    let mut buffer = LineBuffer::default();
    buffer.push(b"a\r\nb\n\npartial");
    assert_eq!(buffer.next_line().as_deref(), Some("a"));
    assert_eq!(buffer.next_line().as_deref(), Some("b"));
    assert_eq!(buffer.next_line().as_deref(), Some(""));
    assert_eq!(buffer.next_line(), None);
  }
}
