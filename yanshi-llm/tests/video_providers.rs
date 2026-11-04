//! Y6.6: the DashScope (Wan), MiniMax (Hailuo) and Kling text-to-video
//! providers end to end against a local server speaking each vendor's
//! documented wire shape: submit → poll → (MiniMax) file retrieve → URL.

use std::time::Duration;

use yanshi_llm::providers::{
  dashscope_media::DashScopeMediaProvider,
  kling_video::KlingVideoProvider,
  minimax_video::MiniMaxVideoProvider,
  modality::{Text2VideoProvider, Text2VideoRequest},
};

/// Bypasses any system proxy (CLAUDE.md, Rust HTTP testing).
fn client() -> reqwest::Client {
  reqwest::Client::builder()
    .no_proxy()
    .build()
    .expect("client")
}

fn request(model: &str) -> Text2VideoRequest {
  Text2VideoRequest {
    model: model.to_string(),
    prompt: "a fox in the snow".to_string(),
    aspect_ratio: Some("16:9".to_string()),
    duration_seconds: Some(5),
    resolution: None,
    seed: None,
    extra: None,
  }
}

async fn wait(provider: &dyn Text2VideoProvider, model: &str) -> String {
  let done = provider
    .generate_and_wait(
      request(model),
      Duration::from_millis(10),
      Duration::from_secs(5),
    )
    .await
    .expect("video");
  done.videos[0].url.clone().expect("url")
}

#[tokio::test]
async fn dashscope_wan_submits_async_and_polls_the_task() {
  let mut server = mockito::Server::new_async().await;
  let submit = server
    .mock("POST", "/api/v1/services/aigc/video-generation/video-synthesis")
    .match_header("x-dashscope-async", "enable")
    .match_header("authorization", "Bearer ds-key")
    .match_body(mockito::Matcher::PartialJsonString(
      r#"{"model":"wan2.7-t2v","input":{"prompt":"a fox in the snow"},"parameters":{"ratio":"16:9","duration":5}}"#.to_string(),
    ))
    .with_body(r#"{"output":{"task_status":"PENDING","task_id":"t-1"},"request_id":"r"}"#)
    .create_async()
    .await;
  let poll = server
    .mock("GET", "/api/v1/tasks/t-1")
    .with_body(r#"{"output":{"task_id":"t-1","task_status":"SUCCEEDED","video_url":"https://oss.example/v.mp4"},"usage":{"output_video_duration":5}}"#)
    .create_async()
    .await;
  let provider =
    DashScopeMediaProvider::with_client(client(), "ds-key", Some(server.url())).expect("provider");
  assert_eq!(
    wait(&provider, "wan2.7-t2v").await,
    "https://oss.example/v.mp4"
  );
  submit.assert_async().await;
  poll.assert_async().await;
}

#[tokio::test]
async fn minimax_polls_then_retrieves_the_file_url() {
  let mut server = mockito::Server::new_async().await;
  let submit = server
    .mock("POST", "/video_generation")
    .match_header("authorization", "Bearer mm-key")
    .match_body(mockito::Matcher::PartialJsonString(
      r#"{"model":"MiniMax-Hailuo-2.3","prompt":"a fox in the snow","duration":5}"#.to_string(),
    ))
    .with_body(
      r#"{"task_id":"106916112212032","base_resp":{"status_code":0,"status_msg":"success"}}"#,
    )
    .create_async()
    .await;
  let query = server
    .mock("GET", "/query/video_generation")
    .match_query(mockito::Matcher::UrlEncoded("task_id".into(), "106916112212032".into()))
    .with_body(r#"{"task_id":"106916112212032","status":"Success","file_id":"205258526306433","video_width":1366,"video_height":768,"base_resp":{"status_code":0,"status_msg":"success"}}"#)
    .create_async()
    .await;
  let retrieve = server
    .mock("GET", "/files/retrieve")
    .match_query(mockito::Matcher::UrlEncoded("file_id".into(), "205258526306433".into()))
    .with_body(r#"{"file":{"file_id":205258526306433,"bytes":5896337,"created_at":1700469398,"filename":"output_aigc.mp4","purpose":"video_generation","download_url":"https://cdn.example/output_aigc.mp4"},"base_resp":{"status_code":0,"status_msg":"success"}}"#)
    .create_async()
    .await;
  let provider =
    MiniMaxVideoProvider::with_client(client(), "mm-key", Some(server.url())).expect("provider");
  assert_eq!(
    wait(&provider, "MiniMax-Hailuo-2.3").await,
    "https://cdn.example/output_aigc.mp4"
  );
  submit.assert_async().await;
  query.assert_async().await;
  retrieve.assert_async().await;
}

#[tokio::test]
async fn minimax_error_codes_fail_the_submission() {
  let mut server = mockito::Server::new_async().await;
  server
    .mock("POST", "/video_generation")
    .with_body(r#"{"task_id":"","base_resp":{"status_code":1026,"status_msg":"input sensitive"}}"#)
    .create_async()
    .await;
  let provider =
    MiniMaxVideoProvider::with_client(client(), "mm-key", Some(server.url())).expect("provider");
  let err = provider
    .submit(request("MiniMax-Hailuo-2.3"))
    .await
    .expect_err("1026");
  assert!(err.to_string().contains("1026"), "{err}");
}

#[tokio::test]
async fn kling_signs_a_jwt_for_a_key_pair_and_polls_the_task() {
  let mut server = mockito::Server::new_async().await;
  let jwt = mockito::Matcher::Regex(r"^Bearer [\w-]+\.[\w-]+\.[\w-]+$".to_string());
  let submit = server
    .mock("POST", "/v1/videos/text2video")
    .match_header("authorization", jwt.clone())
    .match_body(mockito::Matcher::PartialJsonString(
      r#"{"model_name":"kling-v2-1-master","prompt":"a fox in the snow","aspect_ratio":"16:9","duration":"5"}"#.to_string(),
    ))
    .with_body(r#"{"code":0,"message":"SUCCEED","request_id":"r","data":{"task_id":"k-1","task_status":"submitted"}}"#)
    .create_async()
    .await;
  let poll = server
    .mock("GET", "/v1/videos/text2video/k-1")
    .match_header("authorization", jwt)
    .with_body(r#"{"code":0,"message":"SUCCEED","data":{"task_id":"k-1","task_status":"succeed","task_result":{"videos":[{"id":"v","url":"https://kling.example/v.mp4","duration":"5.1"}]}}}"#)
    .create_async()
    .await;
  let provider =
    KlingVideoProvider::with_client(client(), "ak-1:sk-1", Some(server.url())).expect("provider");
  assert_eq!(
    wait(&provider, "kling-v2-1-master").await,
    "https://kling.example/v.mp4"
  );
  submit.assert_async().await;
  poll.assert_async().await;

  // A plain API key is sent as-is.
  let plain = server
    .mock("POST", "/v1/videos/text2video")
    .match_header("authorization", "Bearer kling-api-key")
    .with_body(r#"{"code":1102,"message":"balance not enough"}"#)
    .create_async()
    .await;
  let provider = KlingVideoProvider::with_client(client(), "kling-api-key", Some(server.url()))
    .expect("provider");
  let err = provider
    .submit(request("kling-v2-1-master"))
    .await
    .expect_err("1102");
  assert!(err.to_string().contains("balance not enough"), "{err}");
  plain.assert_async().await;
}
