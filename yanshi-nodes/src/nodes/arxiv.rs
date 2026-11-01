use crate::common::utils::flow_value_to_string;
use async_trait::async_trait;
use flate2::read::GzDecoder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::sync::LazyLock;
use tar::Archive;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};

// Compile-time-known regex patterns. Built lazily on first use, panic on
// pattern bug at init (not per-call). Q5.1 moved these out of per-call
// `Regex::new(...).unwrap()` so the clippy `unwrap_used` deny applies
// cleanly to the rest of the file without `#[allow]` clutter.
//
// The `Result::expect` inside each `LazyLock::new` initializer fires only
// on `regex::Error::Syntax` for a literal pattern that is exhaustively
// covered by the unit tests below (see `nodes::arxiv` tests). A failure
// here would be a build-time / test-time bug, not a runtime error.
#[allow(
  clippy::expect_used,
  reason = "compile-time regex literals; covered by unit tests in module"
)]
static ARXIV_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"arxiv\.org/(?:abs|pdf)/(\d{4}\.\d{4,5})(?:v(\d+))?")
    .expect("ARXIV_URL_RE pattern is malformed — this is a bug in yanshi-nodes")
});
#[allow(
  clippy::expect_used,
  reason = "compile-time regex literals; covered by unit tests in module"
)]
static ARXIV_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"<id>https?://arxiv\.org/abs/(\d{4}\.\d{4,5})(?:v(\d+))?</id>")
    .expect("ARXIV_ID_RE pattern is malformed — this is a bug in yanshi-nodes")
});
#[allow(
  clippy::expect_used,
  reason = "compile-time regex literals; covered by unit tests in module"
)]
static LATEX_COMMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"(?m)%.*$").expect("LATEX_COMMENT_RE is malformed — bug in yanshi-nodes")
});
#[allow(
  clippy::expect_used,
  reason = "compile-time regex literals; covered by unit tests in module"
)]
static LATEX_BEGIN_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"\\begin\{.*?\}").expect("LATEX_BEGIN_RE is malformed — bug in yanshi-nodes")
});
#[allow(
  clippy::expect_used,
  reason = "compile-time regex literals; covered by unit tests in module"
)]
static LATEX_END_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"\\end\{.*?\}").expect("LATEX_END_RE is malformed — bug in yanshi-nodes")
});
#[allow(
  clippy::expect_used,
  reason = "compile-time regex literals; covered by unit tests in module"
)]
static LATEX_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"\\[a-zA-Z@]+\s*(?:\\\[.*?\])?\s*(?:\{.*?\})?")
    .expect("LATEX_TAG_RE is malformed — bug in yanshi-nodes")
});

/// Largest e-print archive the node downloads.
const MAX_ARCHIVE_BYTES: usize = 50 * 1024 * 1024;
/// Largest decompressed source it unpacks (a gzip bomb stops here).
const MAX_EXPANDED_BYTES: usize = 200 * 1024 * 1024;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn async_error(message: impl std::fmt::Display) -> YanshiError {
  YanshiError::AsyncExecutionError {
    message: message.to_string(),
  }
}

fn http_client() -> Result<reqwest::Client, YanshiError> {
  reqwest::Client::builder()
    .timeout(REQUEST_TIMEOUT)
    .https_only(true)
    .build()
    .map_err(async_error)
}

/// Read a response body, failing once it exceeds `limit` bytes.
async fn read_body_bounded(
  mut response: reqwest::Response,
  limit: usize,
) -> Result<Vec<u8>, YanshiError> {
  if response
    .content_length()
    .is_some_and(|len| len > limit as u64)
  {
    return Err(async_error(format!("arXiv archive exceeds {limit} bytes")));
  }
  let mut body = Vec::new();
  while let Some(chunk) = response.chunk().await.map_err(async_error)? {
    if body.len() + chunk.len() > limit {
      return Err(async_error(format!("arXiv archive exceeds {limit} bytes")));
    }
    body.extend_from_slice(&chunk);
  }
  Ok(body)
}

/// Gunzip `compressed`, failing once the output exceeds `limit` bytes.
fn gunzip_bounded(compressed: &[u8], limit: usize) -> Result<Vec<u8>, YanshiError> {
  let mut expanded = Vec::new();
  GzDecoder::new(compressed)
    .take(limit as u64 + 1)
    .read_to_end(&mut expanded)
    .map_err(async_error)?;
  if expanded.len() > limit {
    return Err(async_error(format!(
      "arXiv source expands beyond {limit} bytes"
    )));
  }
  Ok(expanded)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArxivNode {
  pub name: String,
  pub url: String,
  pub fetch_source: Option<bool>,
  pub simplify_latex: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ArxivPaper {
  pub paper_id: String,
  pub version: Option<u32>,
}

#[derive(Debug)]
pub struct LatexSource {
  pub main_content: String,
  pub expanded_content: Option<String>,
}

#[async_trait]
impl AsyncNode for ArxivNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let resolved_url = self.resolve_arxiv_url(inputs)?;
    let paper_info = self.fetch_arxiv_paper(&resolved_url).await?;

    let mut outputs = HashMap::new();
    let source_url = format!("https://arxiv.org/abs/{}", paper_info.paper_id);
    let version = paper_info.version.unwrap_or(1);

    outputs.insert(
      "paper_id".to_string(),
      FlowValue::Json(Value::String(paper_info.paper_id.clone())),
    );
    if let Some(version) = paper_info.version {
      outputs.insert(
        "version".to_string(),
        FlowValue::Json(Value::String(version.to_string())),
      );
    }
    outputs.insert(
      "source_url".to_string(),
      FlowValue::Json(Value::String(source_url)),
    );
    outputs.insert(
      "original_url".to_string(),
      FlowValue::Json(Value::String(resolved_url.clone())),
    );

    if self.fetch_source.unwrap_or(false) {
      match self
        .download_and_extract_latex(&paper_info.paper_id, version)
        .await
      {
        Ok(latex_info) => {
          if let Some(expanded_content) = latex_info.expanded_content {
            outputs.insert(
              "expanded_content".to_string(),
              FlowValue::Json(Value::String(expanded_content)),
            );
          }

          if self.simplify_latex.unwrap_or(false) {
            let simple_latex_content = self.simplify_latex_content(&latex_info.main_content);
            outputs.insert(
              "simple_latex_content".to_string(),
              FlowValue::Json(Value::String(simple_latex_content)),
            );
          }

          // Insert main_content last after it has been borrowed
          outputs.insert(
            "main_content".to_string(),
            FlowValue::Json(Value::String(latex_info.main_content)),
          );
        }
        Err(e) => {
          // Paper doesn't have LaTeX source available, insert empty strings
          tracing::warn!(
            paper_id = %paper_info.paper_id,
            error = %e,
            "could not fetch LaTeX source for paper"
          );
          outputs.insert(
            "simple_latex_content".to_string(),
            FlowValue::Json(Value::String(String::new())),
          );
          outputs.insert(
            "main_content".to_string(),
            FlowValue::Json(Value::String(String::new())),
          );
        }
      }
    }

    Ok(outputs)
  }
}

impl ArxivNode {
  fn resolve_arxiv_url(&self, inputs: &AsyncNodeInputs) -> Result<String, YanshiError> {
    let mut url = self.url.clone();
    for (key, value) in inputs {
      // Support both {{ key }} and {{key}}
      let placeholder_with_spaces = format!("{{{{ {} }}}}", key);
      let placeholder_without_spaces = format!("{{{{{}}}}}", key);
      let value_str = flow_value_to_string(value);

      if url.contains(&placeholder_with_spaces) {
        url = url.replace(&placeholder_with_spaces, &value_str);
      }
      if url.contains(&placeholder_without_spaces) {
        url = url.replace(&placeholder_without_spaces, &value_str);
      }
    }
    Ok(url)
  }

  async fn fetch_arxiv_paper(&self, url: &str) -> Result<ArxivPaper, YanshiError> {
    // Check if it's a valid arXiv URL
    if let Some(caps) = ARXIV_URL_RE.captures(url) {
      let paper_id =
        caps
          .get(1)
          .map(|m| m.as_str().to_string())
          .ok_or_else(|| YanshiError::NodeInputError {
            message: "Could not parse paper ID from arXiv URL".to_string(),
          })?;
      let version = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
      return Ok(ArxivPaper { paper_id, version });
    }

    // If not a URL, treat as search query
    self.search_arxiv(url).await
  }

  async fn search_arxiv(&self, query: &str) -> Result<ArxivPaper, YanshiError> {
    // Use arXiv API to search for papers
    let encoded_query = urlencoding::encode(query);
    let api_url = format!(
      "https://export.arxiv.org/api/query?search_query=all:{}&start=0&max_results=1",
      encoded_query
    );

    let response =
      http_client()?
        .get(&api_url)
        .send()
        .await
        .map_err(|e| YanshiError::AsyncExecutionError {
          message: format!("Failed to search arXiv: {}", e),
        })?;

    let body = response
      .text()
      .await
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: format!("Failed to read arXiv search response: {}", e),
      })?;

    // Parse XML response to extract paper ID
    // Look for <id>http://arxiv.org/abs/XXXX.XXXXX</id>
    let caps = ARXIV_ID_RE
      .captures(&body)
      .ok_or_else(|| YanshiError::NodeInputError {
        message: format!("No papers found for query: {}", query),
      })?;

    let paper_id =
      caps
        .get(1)
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| YanshiError::NodeInputError {
          message: "Could not parse paper ID from search results".to_string(),
        })?;
    let version = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());

    Ok(ArxivPaper { paper_id, version })
  }

  async fn download_and_extract_latex(
    &self,
    paper_id: &str,
    version: u32,
  ) -> Result<LatexSource, YanshiError> {
    let url = format!(
      "https://arxiv.org/e-print/{v_id}v{v_num}",
      v_id = paper_id,
      v_num = version
    );
    let response = http_client()?
      .get(&url)
      .send()
      .await
      .and_then(reqwest::Response::error_for_status)
      .map_err(async_error)?;
    let compressed_bytes = read_body_bounded(response, MAX_ARCHIVE_BYTES).await?;
    let decompressed_data = gunzip_bounded(&compressed_bytes, MAX_EXPANDED_BYTES)?;
    let mut archive = Archive::new(&decompressed_data[..]);

    let mut main_content = String::new();
    let mut all_tex_files = String::new();

    for file in archive
      .entries()
      .map_err(|e| YanshiError::AsyncExecutionError {
        message: e.to_string(),
      })?
    {
      let mut entry = file.map_err(|e| YanshiError::AsyncExecutionError {
        message: e.to_string(),
      })?;
      let path = entry.path().map_err(|e| YanshiError::AsyncExecutionError {
        message: e.to_string(),
      })?;
      if let Some(ext) = path.extension()
        && ext == "tex"
      {
        let mut content = String::new();
        entry
          .read_to_string(&mut content)
          .map_err(|e| YanshiError::AsyncExecutionError {
            message: e.to_string(),
          })?;
        all_tex_files.push_str(&content);
        all_tex_files.push_str("\n\n"); // Separator

        // Q3.8.6: real arXiv .tex sources contain literal
        // `\begin{document}` (one backslash). The previous raw
        // string `r"\\begin{document}"` searches for two literal
        // backslashes and therefore never matched — `main_content`
        // silently fell back to the concatenated `all_tex_files`
        // for every paper. The audit's suggested fix
        // (`r"\\begin\{document\}"`) is still two backslashes; the
        // correct literal is `r"\begin{document}"`. Helper kept as
        // a free fn so the regression test below can exercise the
        // marker logic without spinning up a tar archive.
        if contains_document_marker(&content) {
          main_content = content;
        }
      }
    }

    if main_content.is_empty() {
      main_content = all_tex_files.clone(); // Fallback to all content
    }

    Ok(LatexSource {
      main_content,
      expanded_content: Some(all_tex_files),
    })
  }

  fn simplify_latex_content(&self, latex: &str) -> String {
    let no_comments = LATEX_COMMENT_RE.replace_all(latex, "");
    let no_begin = LATEX_BEGIN_RE.replace_all(&no_comments, "");
    let no_end = LATEX_END_RE.replace_all(&no_begin, "");
    let no_tags = LATEX_TAG_RE.replace_all(&no_end, "");
    no_tags.trim().to_string()
  }
}

/// Q3.8.6: free helper so the marker test below can run without a
/// tar archive. Returns true when `content` looks like the main
/// LaTeX source (contains the literal `\begin{document}` macro).
fn contains_document_marker(content: &str) -> bool {
  content.contains(r"\begin{document}")
}

#[cfg(test)]
mod tests {
  use super::{ARXIV_ID_RE, contains_document_marker, gunzip_bounded};

  /// Q3.8.6 regression: pre-fix the search literal was
  /// `r"\\begin{document}"` (two backslashes) and therefore never
  /// matched real LaTeX (which uses a single backslash). Confirm
  /// both a positive (real LaTeX) and negative (no marker) sample.
  #[test]
  fn gunzip_stops_at_the_expansion_limit() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&vec![0u8; 1024 * 1024]).unwrap();
    let bomb = encoder.finish().unwrap();
    assert!(bomb.len() < 4096, "1 MiB of zeros compresses to a few KiB");

    let err = gunzip_bounded(&bomb, 64 * 1024).unwrap_err();
    assert!(err.to_string().contains("expands beyond"), "{err}");
    assert_eq!(
      gunzip_bounded(&bomb, 1024 * 1024).unwrap().len(),
      1024 * 1024
    );
  }

  #[test]
  fn search_result_ids_parse_over_either_scheme() {
    for scheme in ["http", "https"] {
      let body = format!("<id>{scheme}://arxiv.org/abs/2401.01234v3</id>");
      let caps = ARXIV_ID_RE.captures(&body).unwrap();
      assert_eq!(&caps[1], "2401.01234");
      assert_eq!(&caps[2], "3");
    }
  }

  #[test]
  fn detects_real_latex_begin_document_marker() {
    let real_latex = r#"\documentclass{article}
\title{Hello}
\begin{document}
Body text here.
\end{document}
"#;
    assert!(
      contains_document_marker(real_latex),
      "real .tex must be recognised as the main file; one-backslash literal didn't match"
    );

    let with_macros = r"
\usepackage{amsmath}
\begin{document}
Even with neighbouring text \alpha + \beta, the marker still matches.
\end{document}
";
    assert!(contains_document_marker(with_macros));
  }

  #[test]
  fn ignores_supporting_tex_files_without_marker() {
    let figure_only = r#"\input{figures/main}
\caption{Some helper caption.}
"#;
    assert!(
      !contains_document_marker(figure_only),
      "supporting .tex without \\begin{{document}} must not be picked as main"
    );
  }
}
