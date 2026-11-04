//! Y6.6: image-to-image for OpenAI (`/images/edits` with the source
//! image, no mask) and DashScope Wan (`image2image/image-synthesis`, an
//! async task), against a local server speaking each documented shape.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use yanshi_llm::providers::{
  dashscope_media::DashScopeMediaProvider,
  modality::{Image2ImageProvider, Image2ImageRequest, source_image_bytes},
  openai_images::OpenAIImageProvider,
};

const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAwS2OUAAAAABJRU5ErkJggg==";

fn client() -> reqwest::Client {
  reqwest::Client::builder()
    .no_proxy()
    .build()
    .expect("client")
}

fn request(model: &str) -> Image2ImageRequest {
  Image2ImageRequest {
    model: model.to_string(),
    prompt: "make it a watercolour".to_string(),
    source_url: format!("data:image/png;base64,{PNG_1X1}"),
    source_weight: 0.5,
    size: Some("1024x1024".to_string()),
    n: None,
    response_format: None,
    seed: Some(3),
    steps: None,
    cfg_scale: None,
  }
}

#[tokio::test]
async fn data_uris_decode_and_bad_sources_are_refused() {
  let (bytes, mime) = source_image_bytes(&format!("data:image/png;base64,{PNG_1X1}"), 1 << 20)
    .await
    .expect("decodes");
  assert_eq!(mime, "image/png");
  assert_eq!(bytes, STANDARD.decode(PNG_1X1).expect("b64"));
  assert!(
    source_image_bytes("data:text/plain;base64,aGk=", 1 << 20)
      .await
      .is_err()
  );
  assert!(
    source_image_bytes(&format!("data:image/png;base64,{PNG_1X1}"), 10)
      .await
      .is_err(),
    "over the cap"
  );
  assert!(
    source_image_bytes("/etc/passwd", 1 << 20).await.is_err(),
    "no file paths"
  );
}

#[tokio::test]
async fn openai_sends_the_source_to_images_edits() {
  let mut server = mockito::Server::new_async().await;
  let edits = server
    .mock("POST", "/images/edits")
    .match_header("authorization", "Bearer oa-key")
    .match_body(mockito::Matcher::AllOf(vec![
      mockito::Matcher::Regex(r#"name="image"; filename="source.png""#.to_string()),
      mockito::Matcher::Regex("make it a watercolour".to_string()),
      mockito::Matcher::Regex("gpt-image-1-mini".to_string()),
    ]))
    .with_body(format!(
      r#"{{"created":1,"data":[{{"b64_json":"{PNG_1X1}"}}]}}"#
    ))
    .create_async()
    .await;
  let provider =
    OpenAIImageProvider::with_client(client(), "oa-key", Some(server.url())).expect("provider");
  let response = provider
    .transform(request("gpt-image-1-mini"))
    .await
    .expect("image");
  assert_eq!(response.images[0].b64_json.as_deref(), Some(PNG_1X1));
  edits.assert_async().await;
}

#[tokio::test]
async fn dashscope_wan_submits_the_source_and_keeps_only_successful_images() {
  let mut server = mockito::Server::new_async().await;
  let submit = server
    .mock("POST", "/api/v1/services/aigc/image2image/image-synthesis")
    .match_header("x-dashscope-async", "enable")
    .match_body(mockito::Matcher::PartialJsonString(format!(
      r#"{{"model":"wan2.5-i2i-preview","input":{{"prompt":"make it a watercolour","images":["data:image/png;base64,{PNG_1X1}"]}},"parameters":{{"n":1,"size":"1024*1024","seed":3}}}}"#
    )))
    .with_body(r#"{"output":{"task_status":"PENDING","task_id":"i-1"},"request_id":"r"}"#)
    .create_async()
    .await;
  server
    .mock("GET", "/api/v1/tasks/i-1")
    .with_body(r#"{"output":{"task_id":"i-1","task_status":"SUCCEEDED","results":[{"url":"https://oss.example/a.png"},{"code":"DataInspectionFailed","message":"blocked"}]},"usage":{"image_count":1}}"#)
    .create_async()
    .await;
  let provider =
    DashScopeMediaProvider::with_client(client(), "ds-key", Some(server.url())).expect("provider");
  let response = provider
    .transform(request("wan2.5-i2i-preview"))
    .await
    .expect("image");
  assert_eq!(response.images.len(), 1, "the failed entry is skipped");
  assert_eq!(
    response.images[0].url.as_deref(),
    Some("https://oss.example/a.png")
  );
  submit.assert_async().await;
}
