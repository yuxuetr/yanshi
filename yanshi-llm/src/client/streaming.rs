use crate::Result;
use async_trait::async_trait;
use futures::{Future, Stream};
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// Represents a chunk of data from a streaming LLM response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
  /// The content/delta for this chunk (usually text, but could be multimodal in future)
  pub content: String,
  /// Whether this is the final chunk
  pub is_final: bool,
  /// Optional metadata associated with this chunk
  pub metadata: Option<serde_json::Value>,
  /// Token usage information (if available)
  pub usage: Option<TokenUsage>,
  /// Content type hint for this chunk (e.g., "text", "image", "audio")
  pub content_type: Option<String>,
  /// Q2.5.2: incremental tool_call updates within this chunk, when the
  /// provider streams tool_calls. Empty by default. Consumers merge by
  /// `index`, appending `arguments_delta` and overwriting `id`/`name`
  /// when present (those fields are sent once at the start of a block).
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tool_call_deltas: Vec<ToolCallDelta>,
  /// Y8.2: non-text parts that arrived complete in this chunk (Gemini
  /// streams an image as one whole `inlineData` part, never split).
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub media: Vec<crate::providers::ContentBlock>,
  /// Y10.3: reasoning text that arrived in this chunk (Anthropic
  /// `thinking_delta`, the OpenAI-compatible `reasoning_content` /
  /// `reasoning` deltas). Kept apart from `content` so the answer text
  /// never carries the model's reasoning.
  #[serde(default, skip_serializing_if = "String::is_empty")]
  pub thinking: String,
}

/// Incremental tool_call payload streamed by a provider. Multiple deltas
/// with the same `index` belong to the same tool_call; consumers
/// concatenate `arguments_delta` to reconstruct the JSON-serialized
/// arguments. `id` and `name` are typically set on the first delta for
/// a given `index` and absent afterward.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ToolCallDelta {
  /// Zero-based position of this tool_call in the assistant message.
  pub index: u32,
  /// Tool-call id from the provider (e.g., `call_abc123`, `toolu_xyz`).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub id: Option<String>,
  /// Function name (typically only set on the first delta).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
  /// Partial JSON fragment of the arguments. Concatenating every
  /// delta's `arguments_delta` for a given `index` yields the final
  /// JSON string the provider would have returned non-streamed.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub arguments_delta: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
  pub prompt_tokens: Option<u32>,
  pub completion_tokens: Option<u32>,
  pub total_tokens: Option<u32>,
}

/// Trait for streaming LLM responses.
///
/// Q2.5.4: previously required `Send + Sync`. The `Sync` bound forced
/// every provider to ship an `unsafe impl Sync` on its
/// `*StreamingResponse` because the inner `Pin<Box<dyn Stream + Send>>`
/// isn't `Sync` in general. Streams are inherently sequential —
/// consumers borrow `&mut self` to call `next_chunk`, so `Sync`
/// (multi-threaded `&Self`) is never useful. Dropping the bound
/// removes 5 `unsafe impl`s with no observable behavior change.
#[async_trait]
pub trait StreamingResponse: Send {
  /// Get the next chunk from the stream
  async fn next_chunk(&mut self) -> Result<Option<StreamChunk>>;

  /// Collect all chunks into a single response
  async fn collect_all(mut self) -> Result<String>
  where
    Self: Sized,
  {
    let mut content = String::new();
    while let Some(chunk) = self.next_chunk().await? {
      content.push_str(&chunk.content);
    }
    Ok(content)
  }

  /// Convert to a Stream for use with async stream combinators
  fn into_stream(self) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>
  where
    Self: Sized + Unpin + 'static,
  {
    Box::pin(StreamingResponseStream::new(self))
  }
}

/// V2.2: drain `response` into the same aggregate [`crate::tool_calling::LLMResponse`]
/// shape [`crate::client::llm_client::LLMClient::execute_full`] returns —
/// concatenated `content`, native `tool_calls` reconstructed from each
/// chunk's `tool_call_deltas` (grouped by `index`, `id`/`name` from
/// whichever delta sets them, `arguments_delta` fragments concatenated in
/// arrival order), and `usage` from whichever chunk carries it (normally
/// only the final one) — while invoking `on_chunk` for every [`StreamChunk`]
/// as it arrives, so callers can forward live per-chunk updates (e.g. a
/// `ReActAgent` turn emitting `AgentEvent::TokenDelta`) without losing the
/// aggregate contract every existing `LLMResponse` consumer expects.
/// `on_chunk` is async and awaited before draining the next chunk (not
/// fired-and-forgotten via e.g. `tokio::spawn`) so a caller that forwards
/// chunks to an ordering-sensitive sink — a live event bridge with a
/// monotonic `seq` counter, say — sees them arrive in the same order they
/// were produced; spawning concurrent tasks per chunk would not give that
/// guarantee.
///
/// `stop_reason` is a best-effort reconstruction (`ToolCalls` when any tool
/// call was produced, else `Stop`) rather than a per-provider-normalized
/// value: unlike [`crate::providers::ProviderResponse::stop_reason`],
/// `StreamChunk` carries no typed stop reason (providers only ever surface
/// a raw `finish_reason` inside the opaque `metadata` field on the
/// streaming path), and no consumer in the `yanshi` workspace reads
/// `LLMResponse::stop_reason` today — so plumbing a fully-typed streaming
/// stop reason through all six providers is out of scope here.
/// `thinking` is always `None` (no provider streams a typed thinking
/// delta today) and `raw_metadata` is whichever chunk's `metadata` arrived
/// last, both best-effort for the same reason.
pub async fn collect_streaming_response<F, Fut>(
  mut response: Box<dyn StreamingResponse>,
  mut on_chunk: F,
) -> Result<crate::tool_calling::LLMResponse>
where
  F: FnMut(&StreamChunk) -> Fut,
  Fut: std::future::Future<Output = ()>,
{
  use crate::tool_calling::{LLMResponse, StopReason, ToolCallRequest};
  use std::collections::HashMap;

  let mut content = String::new();
  let mut usage: Option<TokenUsage> = None;
  let mut raw_metadata: Option<serde_json::Value> = None;
  let mut tool_call_order: Vec<u32> = Vec::new();
  let mut tool_call_acc: HashMap<u32, (Option<String>, Option<String>, String)> = HashMap::new();
  let mut media = Vec::new();
  let mut thinking = String::new();

  while let Some(chunk) = response.next_chunk().await? {
    on_chunk(&chunk).await;
    content.push_str(&chunk.content);
    thinking.push_str(&chunk.thinking);
    media.extend(chunk.media.iter().cloned());
    if chunk.usage.is_some() {
      usage = chunk.usage.clone();
    }
    if chunk.metadata.is_some() {
      raw_metadata = chunk.metadata.clone();
    }
    for delta in &chunk.tool_call_deltas {
      let entry = tool_call_acc.entry(delta.index).or_insert_with(|| {
        tool_call_order.push(delta.index);
        (None, None, String::new())
      });
      if delta.id.is_some() {
        entry.0 = delta.id.clone();
      }
      if delta.name.is_some() {
        entry.1 = delta.name.clone();
      }
      if let Some(fragment) = &delta.arguments_delta {
        entry.2.push_str(fragment);
      }
    }
  }

  let tool_calls: Vec<ToolCallRequest> = tool_call_order
    .into_iter()
    .filter_map(|index| tool_call_acc.remove(&index))
    .map(|(id, name, arguments_json)| ToolCallRequest {
      id: id.unwrap_or_default(),
      name: name.unwrap_or_default(),
      arguments: serde_json::from_str(&arguments_json).unwrap_or(serde_json::Value::Null),
    })
    .collect();

  let stop_reason = Some(if tool_calls.is_empty() {
    StopReason::Stop
  } else {
    StopReason::ToolCalls
  });

  // `StreamChunk::usage` and `ProviderResponse::usage` are distinct types
  // with the same shape (one is the streaming-chunk-local type, the other
  // the provider-response type `LLMResponse` actually carries) — convert.
  let usage = usage.map(|u| crate::providers::TokenUsage {
    prompt_tokens: u.prompt_tokens,
    completion_tokens: u.completion_tokens,
    total_tokens: u.total_tokens,
  });

  Ok(LLMResponse {
    content,
    tool_calls,
    stop_reason,
    usage,
    raw_metadata,
    thinking: (!thinking.is_empty()).then_some(thinking),
    media,
  })
}

/// Internal helper to convert StreamingResponse to Stream
struct StreamingResponseStream<T: StreamingResponse> {
  response: T,
}

impl<T: StreamingResponse> StreamingResponseStream<T> {
  fn new(response: T) -> Self {
    Self { response }
  }
}

impl<T: StreamingResponse + Unpin> Stream for StreamingResponseStream<T> {
  type Item = Result<StreamChunk>;

  fn poll_next(
    mut self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Option<Self::Item>> {
    let future = self.response.next_chunk();
    tokio::pin!(future);

    match Future::poll(future, cx) {
      std::task::Poll::Ready(Ok(Some(chunk))) => std::task::Poll::Ready(Some(Ok(chunk))),
      std::task::Poll::Ready(Ok(None)) => std::task::Poll::Ready(None),
      std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Some(Err(e))),
      std::task::Poll::Pending => std::task::Poll::Pending,
    }
  }
}

#[cfg(test)]
mod collect_streaming_response_tests {
  use super::*;
  use std::collections::VecDeque;

  /// Minimal `StreamingResponse` fed from a fixed, pre-built chunk queue —
  /// used to exercise `collect_streaming_response` without depending on any
  /// particular provider's wire format.
  struct FixedChunkStream(VecDeque<StreamChunk>);

  #[async_trait]
  impl StreamingResponse for FixedChunkStream {
    async fn next_chunk(&mut self) -> Result<Option<StreamChunk>> {
      Ok(self.0.pop_front())
    }
  }

  fn text_chunk(text: &str) -> StreamChunk {
    StreamChunk {
      thinking: String::new(),
      media: Vec::new(),
      content: text.to_string(),
      is_final: false,
      metadata: None,
      usage: None,
      content_type: Some("text".to_string()),
      tool_call_deltas: Vec::new(),
    }
  }

  /// Y10.3: reasoning deltas land in `LLMResponse.thinking`, never in the
  /// answer text.
  #[tokio::test]
  async fn reasoning_deltas_collect_into_thinking() {
    let mut first = text_chunk("");
    first.thinking = "17*23 ".to_string();
    let mut second = text_chunk("");
    second.thinking = "= 391".to_string();
    let stream: Box<dyn StreamingResponse> = Box::new(FixedChunkStream(VecDeque::from(vec![
      first,
      second,
      text_chunk("391"),
    ])));
    let response = collect_streaming_response(stream, |_| async {})
      .await
      .expect("collect");
    assert_eq!(response.content, "391");
    assert_eq!(response.thinking.as_deref(), Some("17*23 = 391"));

    let plain: Box<dyn StreamingResponse> =
      Box::new(FixedChunkStream(VecDeque::from(vec![text_chunk("hi")])));
    let response = collect_streaming_response(plain, |_| async {})
      .await
      .expect("collect");
    assert_eq!(response.thinking, None);
  }

  /// The V2.2 test bar: every delta in the sequence is forwarded via
  /// `on_chunk`, in order, and the aggregate `content` matches exact
  /// concatenation.
  #[tokio::test]
  async fn every_delta_is_forwarded_in_order_and_content_concatenates_exactly() {
    let stream: Box<dyn StreamingResponse> = Box::new(FixedChunkStream(VecDeque::from(vec![
      text_chunk("Hel"),
      text_chunk("lo, "),
      text_chunk("wor"),
      StreamChunk {
        thinking: String::new(),
        media: Vec::new(),
        content: "ld!".to_string(),
        is_final: true,
        metadata: None,
        usage: Some(TokenUsage {
          prompt_tokens: Some(10),
          completion_tokens: Some(4),
          total_tokens: Some(14),
        }),
        content_type: Some("text".to_string()),
        tool_call_deltas: Vec::new(),
      },
    ])));

    let mut forwarded: Vec<String> = Vec::new();
    let response = collect_streaming_response(stream, |chunk| {
      forwarded.push(chunk.content.clone());
      async {}
    })
    .await
    .unwrap();

    assert_eq!(forwarded, vec!["Hel", "lo, ", "wor", "ld!"]);
    assert_eq!(response.content, "Hello, world!");
    assert_eq!(response.usage.unwrap().completion_tokens, Some(4));
    assert!(response.tool_calls.is_empty());
    assert_eq!(
      response.stop_reason,
      Some(crate::tool_calling::StopReason::Stop)
    );
  }

  /// Tool-call arguments arrive fragmented across multiple chunks at the
  /// same `index`; the collector must concatenate them in arrival order
  /// and parse the reassembled JSON.
  #[tokio::test]
  async fn tool_call_deltas_reassemble_across_chunks_by_index() {
    let stream: Box<dyn StreamingResponse> = Box::new(FixedChunkStream(VecDeque::from(vec![
      StreamChunk {
        thinking: String::new(),
        media: Vec::new(),
        content: String::new(),
        is_final: false,
        metadata: None,
        usage: None,
        content_type: Some("text".to_string()),
        tool_call_deltas: vec![ToolCallDelta {
          index: 0,
          id: Some("call_0".to_string()),
          name: Some("get_weather".to_string()),
          arguments_delta: Some("{\"city\":".to_string()),
        }],
      },
      StreamChunk {
        thinking: String::new(),
        media: Vec::new(),
        content: String::new(),
        is_final: true,
        metadata: None,
        usage: None,
        content_type: Some("text".to_string()),
        tool_call_deltas: vec![ToolCallDelta {
          index: 0,
          id: None,
          name: None,
          arguments_delta: Some("\"Tokyo\"}".to_string()),
        }],
      },
    ])));

    let response = collect_streaming_response(stream, |_| async {})
      .await
      .unwrap();

    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "call_0");
    assert_eq!(response.tool_calls[0].name, "get_weather");
    assert_eq!(
      response.tool_calls[0].arguments,
      serde_json::json!({"city": "Tokyo"})
    );
    assert_eq!(
      response.stop_reason,
      Some(crate::tool_calling::StopReason::ToolCalls)
    );
  }

  /// A stream with a single is_final chunk (the shape every real provider
  /// used before this task, and still what a short response looks like)
  /// round-trips identically to `execute_full`'s aggregate contract.
  #[tokio::test]
  async fn single_final_chunk_round_trips_like_a_non_chunked_response() {
    let stream: Box<dyn StreamingResponse> =
      Box::new(FixedChunkStream(VecDeque::from(vec![StreamChunk {
        thinking: String::new(),
        media: Vec::new(),
        content: "the whole answer at once".to_string(),
        is_final: true,
        metadata: None,
        usage: Some(TokenUsage {
          prompt_tokens: Some(5),
          completion_tokens: Some(5),
          total_tokens: Some(10),
        }),
        content_type: Some("text".to_string()),
        tool_call_deltas: Vec::new(),
      }])));

    let mut forwarded_count = 0;
    let response = collect_streaming_response(stream, |_| {
      forwarded_count += 1;
      async {}
    })
    .await
    .unwrap();

    assert_eq!(forwarded_count, 1);
    assert_eq!(response.content, "the whole answer at once");
  }
}
