use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use tokio::fs;
use yanshi_llm::{Yanshi, providers::modality::Text2ImageRequest as ModalityText2ImageRequest};

#[allow(clippy::too_many_arguments)]
pub async fn execute(
  prompt: String,
  model: Option<String>,
  size: String,
  output: String,
  format: String,
  steps: u32,
  cfg_scale: f32,
  seed: Option<u64>,
  strength: Option<f32>,
  input_image: Option<String>,
) -> Result<()> {
  let model = model.unwrap_or_else(|| "gpt-image-2".to_string());

  // Initialize Yanshi so the registry knows about all configured
  // models. The dispatcher resolves vendor + API key by model name.
  Yanshi::init().await?;

  println!("🎨 Yanshi Image Generation");
  println!("Model: {}", model);
  println!("Prompt: {}", prompt);
  println!("Size: {}", size);
  println!("Format: {}", format);
  println!("Output: {}", output);
  if let Some(seed) = seed {
    println!("Seed: {}", seed);
  }
  if let Some(input) = &input_image {
    println!("Input image: {}", input);
  }
  println!();

  if strength.is_some() || input_image.is_some() {
    // Image-to-image takes a different dispatcher path
    // (`Yanshi::image2image`). The text-to-image CLI command stays
    // text-only for now — wire `yanshi image generate-i2i` for the
    // i2i case rather than overloading the same code path.
    println!(
      "⚠️  Warning: image-to-image (strength / input_image) is not handled by this command; \
       use the dedicated image-to-image path. Text-to-image generation continues."
    );
  }

  let provider = Yanshi::text2image_for(&model).await?;
  println!(
    "🖼️  Generating via provider '{}' (model '{}')...",
    provider.name(),
    model
  );

  let request = ModalityText2ImageRequest {
    model: model.clone(),
    prompt,
    size: Some(size.clone()),
    n: Some(1),
    response_format: Some(format.clone()),
    seed: seed.map(|s| s as i32),
    steps: Some(steps),
    cfg_scale: Some(cfg_scale),
  };

  let start_time = std::time::Instant::now();
  let response = match tokio::time::timeout(
    std::time::Duration::from_secs(120),
    provider.generate(request),
  )
  .await
  {
    Ok(result) => result.map_err(|e| {
      eprintln!("Error: Internal LLM error: {}", e);
      anyhow::anyhow!("Image generation failed: {}", e)
    })?,
    Err(_) => {
      return Err(anyhow::anyhow!(
        "Image generation timed out after 2 minutes. This may be due to:\n  - Network connectivity issues\n  - API server overload\n  - Complex prompt requiring more processing time\n\nTry again with a simpler prompt or check your internet connection."
      ));
    }
  };

  let duration = start_time.elapsed();
  println!("✅ Image generated in {:?}", duration);
  println!();

  println!("💾 Saving image to: {}", output);

  let first_image = response
    .images
    .first()
    .ok_or_else(|| anyhow::anyhow!("No image data received in response"))?;

  match format.as_str() {
    "b64_json" => {
      let b64_data = first_image
        .b64_json
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No base64 image data received"))?;
      let image_bytes = general_purpose::STANDARD.decode(b64_data)?;
      fs::write(&output, &image_bytes).await?;
      println!(
        "✅ Image saved as base64 data ({} bytes)",
        image_bytes.len()
      );
    }
    "url" => {
      let url = first_image
        .url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No image URL received"))?;
      let url_output = format!("{}.url", output);
      fs::write(&url_output, url).await?;
      println!("✅ Image URL saved to: {}", url_output);
      println!("🔗 Image URL: {}", url);
    }
    other => {
      return Err(anyhow::anyhow!("Unsupported format: {}", other));
    }
  }

  if let Some(seed) = first_image.seed {
    println!("🎯 Generation seed: {}", seed);
  }

  println!("🎉 Image generation completed successfully!");
  Ok(())
}
