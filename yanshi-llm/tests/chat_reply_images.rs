//! Y6.6 (evaluation M7): an image in a chat reply (Gemini image-output
//! models return `inlineData` parts next to text) reaches the caller as
//! image content instead of being dropped.

use std::collections::HashMap;

use serde_json::json;
use yanshi_llm::providers::{
  ContentBlock, ContentType, GoogleProvider, LLMProvider, ProviderRequest,
};

const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAwS2OUAAAAABJRU5ErkJggg==";

#[tokio::test]
async fn gemini_reply_images_become_content_blocks() {
  let mut server = mockito::Server::new_async().await;
  let reply = json!({
    "candidates": [{
      "content": {"role": "model", "parts": [
        {"text": "Here is the diagram: "},
        {"inlineData": {"mimeType": "image/png", "data": PNG_1X1}},
        {"text": "done."}
      ]},
      "finishReason": "STOP", "index": 0
    }],
    "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 5, "totalTokenCount": 8}
  });
  server
    .mock(
      "POST",
      mockito::Matcher::Regex(":generateContent".to_string()),
    )
    .with_body(reply.to_string())
    .create_async()
    .await;
  let client = reqwest::Client::builder()
    .no_proxy()
    .build()
    .expect("client");
  let provider =
    GoogleProvider::with_client(client, "g-key", Some(server.url())).expect("provider");
  let response = provider
    .execute(&ProviderRequest {
      model: "gemini-2.5-flash-image".to_string(),
      messages: vec![json!({"role": "user", "content": "draw a box"})],
      stream: false,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    })
    .await
    .expect("reply");
  assert!(matches!(response.content, ContentType::Mixed(_)));
  let (text, media) = response.content.split_text_and_media();
  assert_eq!(text, "Here is the diagram: done.");
  assert!(
    matches!(&media[..], [ContentBlock::Image { data, media_type }]
    if media_type == "image/png" && data.starts_with(b"\x89PNG"))
  );
  let metadata = response.metadata.expect("metadata").to_string();
  assert!(
    !metadata.contains(PNG_1X1),
    "the image is not duplicated into metadata"
  );
}

/// Y8.2: a streamed reply keeps its image too. Gemini sends the image as
/// one complete `inlineData` part inside an SSE event.
#[tokio::test]
async fn gemini_streamed_reply_images_reach_the_collected_response() {
  let mut server = mockito::Server::new_async().await;
  let event = |parts: serde_json::Value, finish: Option<&str>| {
    let mut candidate = json!({"content": {"role": "model", "parts": parts}, "index": 0});
    if let Some(reason) = finish {
      candidate["finishReason"] = json!(reason);
    }
    format!("data: {}\r\n\r\n", json!({"candidates": [candidate]}))
  };
  let body = [
    event(json!([{"text": "Here is the diagram: "}]), None),
    event(
      json!([{"inlineData": {"mimeType": "image/png", "data": PNG_1X1}}]),
      None,
    ),
    event(json!([{"text": "done."}]), Some("STOP")),
  ]
  .concat();
  server
    .mock(
      "POST",
      mockito::Matcher::Regex(":streamGenerateContent".to_string()),
    )
    .with_header("content-type", "text/event-stream")
    .with_body(body)
    .create_async()
    .await;
  let client = reqwest::Client::builder()
    .no_proxy()
    .build()
    .expect("client");
  let provider =
    GoogleProvider::with_client(client, "g-key", Some(server.url())).expect("provider");
  let stream = provider
    .execute_streaming(&ProviderRequest {
      model: "gemini-2.5-flash-image".to_string(),
      messages: vec![json!({"role": "user", "content": "draw a box"})],
      stream: true,
      parameters: HashMap::new(),
      tools: None,
      tool_choice: None,
      thinking: None,
      response_format: None,
    })
    .await
    .expect("stream");
  let mut chunk_media = 0;
  let response = yanshi_llm::client::streaming::collect_streaming_response(stream, |chunk| {
    chunk_media += chunk.media.len();
    async {}
  })
  .await
  .expect("collected");
  assert_eq!(chunk_media, 1, "the image arrives in exactly one chunk");
  assert_eq!(response.content, "Here is the diagram: done.");
  assert!(
    matches!(&response.media[..], [ContentBlock::Image { data, media_type }]
    if media_type == "image/png" && data.starts_with(b"\x89PNG"))
  );
}
