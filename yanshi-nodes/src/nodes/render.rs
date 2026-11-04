//! In-process renderers (Y3.1, `docs/RFC_CONTENT_STUDIO.md` §5): turn the
//! structured content an LLM or upstream node produced into a
//! deliverable file, with no external binary.
//!
//! - `render_table`: rows (JSON array of objects or of arrays) → CSV or
//!   XLSX.
//! - `render_svg`: SVG text → PNG (resvg; system fonts for `<text>`).
//! - `render_markdown`: Markdown → one self-contained HTML page.
//!
//! Each reads its inputs at execute time (YAML `parameters` arrive as
//! inputs, `input_mapping` adds upstream outputs), accepts the content
//! as a JSON value, a JSON-encoded string, or a `FlowValue::File` from an
//! upstream node, and returns `FlowValue::File` under
//! `<id>_output` via the shared artifact writer.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  value::FlowValue,
};

use crate::common::artifacts::write_artifact;

const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
/// Upper bound on a rendered PNG's pixel count (8192 × 8192): an SVG
/// declaring an enormous canvas must not exhaust memory.
const MAX_PNG_PIXELS: u64 = 8192 * 8192;

fn input_error(node: &str, message: impl std::fmt::Display) -> YanshiError {
  YanshiError::NodeInputError {
    message: format!("{node}: {message}"),
  }
}

/// The text of input `key`: a JSON string, any other JSON value as JSON
/// text, or the contents of a `File` an upstream node produced.
async fn text_input(
  node: &str,
  inputs: &AsyncNodeInputs,
  key: &str,
) -> Result<String, YanshiError> {
  match inputs.get(key) {
    Some(FlowValue::Json(Value::String(text))) => Ok(text.clone()),
    Some(FlowValue::Json(other)) => Ok(other.to_string()),
    Some(FlowValue::File { path, .. }) => tokio::fs::read_to_string(path).await.map_err(|e| {
      input_error(
        node,
        format!("cannot read '{key}' from {}: {e}", path.display()),
      )
    }),
    Some(FlowValue::Url { url, .. }) => Err(input_error(
      node,
      format!("'{key}' is a URL ({url}); fetch it with an http node first"),
    )),
    None => Err(input_error(node, format!("missing input '{key}'"))),
  }
}

fn optional_str<'a>(inputs: &'a AsyncNodeInputs, key: &str) -> Option<&'a str> {
  match inputs.get(key) {
    Some(FlowValue::Json(Value::String(s))) if !s.trim().is_empty() => Some(s.trim()),
    _ => None,
  }
}

fn output(node: &str, value: FlowValue) -> HashMap<String, FlowValue> {
  HashMap::from([(format!("{node}_output"), value)])
}

// ── render_table ────────────────────────────────────────────────────────

/// `render_table`: inputs `rows` (required), `format` (`csv` default or
/// `xlsx`), `columns` (optional header order for object rows).
pub struct RenderTableNode {
  id: String,
}

impl RenderTableNode {
  pub fn new(id: impl Into<String>) -> Self {
    Self { id: id.into() }
  }
}

/// A table: a header (empty for array rows without `columns`) and cells.
struct Table {
  header: Vec<String>,
  rows: Vec<Vec<Value>>,
}

fn table_from(rows: Value, columns: Option<Vec<String>>) -> Result<Table, String> {
  let rows = match rows {
    Value::String(text) => {
      serde_json::from_str(&text).map_err(|e| format!("rows is not JSON: {e}"))?
    }
    other => other,
  };
  let Value::Array(rows) = rows else {
    return Err("rows must be a JSON array".to_string());
  };
  if rows.iter().all(Value::is_object) {
    let header = match columns {
      Some(columns) => columns,
      None => {
        let mut header: Vec<String> = Vec::new();
        for row in &rows {
          if let Value::Object(map) = row {
            for key in map.keys() {
              if !header.contains(key) {
                header.push(key.clone());
              }
            }
          }
        }
        header
      }
    };
    let cells = rows
      .iter()
      .map(|row| {
        header
          .iter()
          .map(|key| row.get(key).cloned().unwrap_or(Value::Null))
          .collect()
      })
      .collect();
    Ok(Table {
      header,
      rows: cells,
    })
  } else if rows.iter().all(Value::is_array) {
    let cells = rows
      .into_iter()
      .map(|row| match row {
        Value::Array(cells) => cells,
        _ => unreachable!("every row was checked to be an array above"),
      })
      .collect();
    Ok(Table {
      header: columns.unwrap_or_default(),
      rows: cells,
    })
  } else {
    Err("rows must be all objects or all arrays".to_string())
  }
}

fn cell_text(value: &Value) -> String {
  match value {
    Value::Null => String::new(),
    Value::String(s) => s.clone(),
    other => other.to_string(),
  }
}

fn table_csv(table: &Table) -> Result<Vec<u8>, String> {
  let mut writer = csv::Writer::from_writer(Vec::new());
  if !table.header.is_empty() {
    writer
      .write_record(&table.header)
      .map_err(|e| e.to_string())?;
  }
  for row in &table.rows {
    writer
      .write_record(row.iter().map(cell_text))
      .map_err(|e| e.to_string())?;
  }
  writer.into_inner().map_err(|e| e.to_string())
}

fn table_xlsx(table: &Table) -> Result<Vec<u8>, String> {
  use rust_xlsxwriter::{Format, Workbook};
  let mut workbook = Workbook::new();
  let sheet = workbook.add_worksheet();
  let bold = Format::new().set_bold();
  let mut first_row: u32 = 0;
  if !table.header.is_empty() {
    for (col, name) in table.header.iter().enumerate() {
      sheet
        .write_string_with_format(0, col_index(col)?, name, &bold)
        .map_err(|e| e.to_string())?;
    }
    first_row = 1;
  }
  for (r, row) in table.rows.iter().enumerate() {
    let r = first_row + u32::try_from(r).map_err(|_| "too many rows for xlsx".to_string())?;
    for (c, value) in row.iter().enumerate() {
      let c = col_index(c)?;
      let written = match value {
        Value::Null => continue,
        Value::Number(n) => match n.as_f64() {
          Some(f) => sheet.write_number(r, c, f).map(|_| ()),
          None => sheet.write_string(r, c, n.to_string()).map(|_| ()),
        },
        Value::Bool(b) => sheet.write_boolean(r, c, *b).map(|_| ()),
        other => sheet.write_string(r, c, cell_text(other)).map(|_| ()),
      };
      written.map_err(|e| e.to_string())?;
    }
  }
  workbook.save_to_buffer().map_err(|e| e.to_string())
}

fn col_index(col: usize) -> Result<u16, String> {
  u16::try_from(col).map_err(|_| "too many columns for xlsx".to_string())
}

#[async_trait]
impl AsyncNode for RenderTableNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let rows = match inputs.get("rows") {
      Some(FlowValue::Json(value)) => value.clone(),
      Some(_) => Value::String(text_input(&self.id, inputs, "rows").await?),
      None => return Err(input_error(&self.id, "missing input 'rows'")),
    };
    let columns = match inputs.get("columns") {
      Some(FlowValue::Json(Value::Array(items))) => Some(items.iter().map(cell_text).collect()),
      _ => None,
    };
    let table = table_from(rows, columns).map_err(|e| input_error(&self.id, e))?;
    let (bytes, mime) = match optional_str(inputs, "format").unwrap_or("csv") {
      "csv" => (table_csv(&table), "text/csv"),
      "xlsx" => (table_xlsx(&table), XLSX_MIME),
      other => {
        return Err(input_error(
          &self.id,
          format!("unknown format '{other}': expected 'csv' or 'xlsx'"),
        ));
      }
    };
    let bytes = bytes.map_err(|e| YanshiError::NodeExecutionFailed {
      message: format!("{}: rendering the table failed: {e}", self.id),
    })?;
    Ok(output(
      &self.id,
      write_artifact(&self.id, &bytes, mime).await?,
    ))
  }
}

// ── render_svg ──────────────────────────────────────────────────────────

/// `render_svg`: inputs `svg` (required) and `width` (optional output
/// width in pixels; height follows the aspect ratio). Produces a PNG.
pub struct RenderSvgNode {
  id: String,
}

impl RenderSvgNode {
  pub fn new(id: impl Into<String>) -> Self {
    Self { id: id.into() }
  }
}

/// Parse `svg` the way [`svg_to_png`] would, without rasterising: the
/// `figure` node's check that LLM-written SVG is renderable.
pub fn check_svg(svg: &str) -> Result<(), String> {
  let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default())
    .map_err(|e| format!("invalid SVG: {e}"))?;
  let size = tree.size();
  if size.width() <= 0.0 || size.height() <= 0.0 {
    return Err("SVG has an empty canvas; set width and height".to_string());
  }
  Ok(())
}

/// SVG text → PNG bytes (also used by `figure`'s optional PNG copy).
pub fn svg_to_png(svg: &str, width: Option<u32>) -> Result<Vec<u8>, String> {
  use resvg::{tiny_skia, usvg};
  // System fonts, loaded once per process: `<text>` renders nothing
  // without them, and scanning the font directories is slow.
  static FONTS: std::sync::OnceLock<std::sync::Arc<usvg::fontdb::Database>> =
    std::sync::OnceLock::new();
  let fonts = FONTS.get_or_init(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    std::sync::Arc::new(db)
  });
  let options = usvg::Options {
    fontdb: fonts.clone(),
    ..usvg::Options::default()
  };
  let tree = usvg::Tree::from_str(svg, &options).map_err(|e| format!("invalid SVG: {e}"))?;
  let size = tree.size();
  let scale = match width {
    Some(w) if w > 0 => w as f32 / size.width(),
    _ => 1.0,
  };
  let out_w = (size.width() * scale).ceil() as u32;
  let out_h = (size.height() * scale).ceil() as u32;
  if out_w == 0 || out_h == 0 || u64::from(out_w) * u64::from(out_h) > MAX_PNG_PIXELS {
    return Err(format!(
      "output size {out_w}x{out_h} is outside 1..={MAX_PNG_PIXELS} pixels"
    ));
  }
  let mut pixmap =
    tiny_skia::Pixmap::new(out_w, out_h).ok_or_else(|| "cannot allocate the canvas".to_string())?;
  resvg::render(
    &tree,
    tiny_skia::Transform::from_scale(scale, scale),
    &mut pixmap.as_mut(),
  );
  pixmap
    .encode_png()
    .map_err(|e| format!("PNG encoding failed: {e}"))
}

#[async_trait]
impl AsyncNode for RenderSvgNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let svg = text_input(&self.id, inputs, "svg").await?;
    let width = match inputs.get("width") {
      Some(FlowValue::Json(Value::Number(n))) => n.as_u64().and_then(|w| u32::try_from(w).ok()),
      _ => None,
    };
    let png = tokio::task::spawn_blocking(move || svg_to_png(&svg, width))
      .await
      .map_err(|e| YanshiError::NodeExecutionFailed {
        message: format!("{}: SVG render task failed: {e}", self.id),
      })?
      .map_err(|e| input_error(&self.id, e))?;
    Ok(output(
      &self.id,
      write_artifact(&self.id, &png, "image/png").await?,
    ))
  }
}

// ── render_markdown ─────────────────────────────────────────────────────

/// `render_markdown`: inputs `markdown` (required) and `title`
/// (optional). Produces one HTML page with its stylesheet inline.
pub struct RenderMarkdownNode {
  id: String,
}

impl RenderMarkdownNode {
  pub fn new(id: impl Into<String>) -> Self {
    Self { id: id.into() }
  }
}

const PAGE_STYLE: &str = "body{max-width:46rem;margin:2rem auto;padding:0 1rem;\
font:16px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif;color:#1f2328}\
h1,h2,h3{line-height:1.25}code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}\
pre{background:#f6f8fa;padding:1rem;overflow:auto;border-radius:6px}\
table{border-collapse:collapse}th,td{border:1px solid #d0d7de;padding:.4rem .8rem}\
blockquote{margin:0;padding:0 1rem;color:#59636e;border-left:.25rem solid #d0d7de}\
img{max-width:100%}";

/// Markdown → a standalone page. Raw HTML in the source is rendered as
/// text, not markup: the page may be served from the gateway's own
/// origin (`GET /v1/runs/{id}/artifacts/{artifact_id}`), and LLM-written
/// Markdown must not be able to inject script there.
fn markdown_page(markdown: &str, title: Option<&str>) -> String {
  use pulldown_cmark::{Event, Options, Parser, html};
  let options = Options::ENABLE_TABLES
    | Options::ENABLE_FOOTNOTES
    | Options::ENABLE_STRIKETHROUGH
    | Options::ENABLE_TASKLISTS;
  let parser = Parser::new_ext(markdown, options).map(|event| match event {
    Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
    other => other,
  });
  let mut body = String::new();
  html::push_html(&mut body, parser);
  let title = title
    .map(str::to_string)
    .or_else(|| {
      markdown
        .lines()
        .find_map(|line| line.strip_prefix("# ").map(|t| t.trim().to_string()))
    })
    .unwrap_or_else(|| "Document".to_string());
  format!(
    "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{}</title><style>{PAGE_STYLE}</style></head>\n<body>\n{body}</body></html>\n",
    escape_html(&title)
  )
}

fn escape_html(text: &str) -> String {
  text
    .replace('&', "&amp;")
    .replace('<', "&lt;")
    .replace('>', "&gt;")
    .replace('"', "&quot;")
}

#[async_trait]
impl AsyncNode for RenderMarkdownNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let markdown = text_input(&self.id, inputs, "markdown").await?;
    let page = markdown_page(&markdown, optional_str(inputs, "title"));
    Ok(output(
      &self.id,
      write_artifact(&self.id, page.as_bytes(), "text/html").await?,
    ))
  }
}

// ── render_document (external: pandoc, typst for PDF) ─────────────────

/// `render_document` (Y3.3): Markdown → `docx` / `pptx` / `html` /
/// `epub` / `pdf` through pandoc; PDF uses `--pdf-engine=typst` (one
/// binary, no TeX). Inputs: `markdown` (required), `format` (required),
/// `title`, `toc` (bool).
///
/// pandoc (and typst under it) runs under the platform OS sandbox
/// backend, writing only to this node's artifact directory and the temp
/// dir; see [`run_pandoc`]. Raw HTML in the source is read as
/// text (`-f markdown-raw_html`), for the same reason as
/// `render_markdown`.
pub struct RenderDocumentNode {
  id: String,
}

impl RenderDocumentNode {
  pub fn new(id: impl Into<String>) -> Self {
    Self { id: id.into() }
  }
}

/// (pandoc extension, MIME type) for each supported `format`.
fn document_format(format: &str) -> Option<(&'static str, &'static str)> {
  Some(match format {
    "docx" => (
      "docx",
      "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    "pptx" => (
      "pptx",
      "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    "html" => ("html", "text/html"),
    "epub" => ("epub", "application/epub+zip"),
    "pdf" => ("pdf", "application/pdf"),
    _ => return None,
  })
}

/// Run pandoc (and, for PDF, typst under it) through
/// [`crate::common::sandboxed`]: fonts and typst's install tree readable,
/// writes confined to `workdir` and the temp dir.
async fn run_pandoc(
  node: &str,
  workdir: &std::path::Path,
  args: &[std::ffi::OsString],
  needs_typst: bool,
) -> Result<(), YanshiError> {
  use crate::common::sandboxed;
  let not_installed = |tool: &str| YanshiError::NodeExecutionFailed {
    message: format!("{node}: {tool} is not on PATH (`yanshi doctor` lists renderer binaries)"),
  };
  let pandoc = sandboxed::find_tool("pandoc").ok_or_else(|| not_installed("pandoc"))?;
  let mut read_paths = sandboxed::font_dirs();
  if needs_typst {
    let typst =
      sandboxed::find_tool("typst").ok_or_else(|| not_installed("typst (the PDF engine)"))?;
    read_paths.extend(sandboxed::install_prefixes(&typst));
  }
  let output = sandboxed::run(sandboxed::SandboxedRun {
    node,
    program: pandoc,
    args: args.to_vec(),
    workdir,
    read_paths,
    write_paths: Vec::new(),
    env: Vec::new(),
    timeout: std::time::Duration::from_secs(300),
  })
  .await?;
  if !output.status.success() {
    return Err(YanshiError::NodeExecutionFailed {
      message: format!(
        "{node}: pandoc exited with {}: {}",
        output.status,
        sandboxed::output_tail(&output, 12)
      ),
    });
  }
  Ok(())
}

#[async_trait]
impl AsyncNode for RenderDocumentNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let markdown = text_input(&self.id, inputs, "markdown").await?;
    let format = optional_str(inputs, "format")
      .ok_or_else(|| input_error(&self.id, "missing input 'format'"))?;
    let (extension, mime) = document_format(format).ok_or_else(|| {
      input_error(
        &self.id,
        format!("unknown format '{format}': expected docx, pptx, html, epub or pdf"),
      )
    })?;

    let workdir = crate::common::artifacts::artifact_dir(&self.id);
    tokio::fs::create_dir_all(&workdir).await.map_err(|e| {
      input_error(
        &self.id,
        format!("cannot create {}: {e}", workdir.display()),
      )
    })?;
    let stem = format!(".render-{}", uuid::Uuid::new_v4());
    let source = workdir.join(format!("{stem}.md"));
    let target = workdir.join(format!("{stem}.{extension}"));
    let metadata = workdir.join(format!("{stem}.json"));
    let cleanup = [source.clone(), target.clone(), metadata.clone()];

    let rendered = async {
      tokio::fs::write(&source, markdown.as_bytes())
        .await
        .map_err(|e| input_error(&self.id, format!("cannot stage the source: {e}")))?;
      let mut args: Vec<std::ffi::OsString> = vec![
        source.clone().into(),
        "-f".into(),
        "markdown-raw_html".into(),
        "--standalone".into(),
        "-o".into(),
        target.clone().into(),
        "--resource-path".into(),
        workdir.clone().into(),
      ];
      if let Some(title) = optional_str(inputs, "title") {
        tokio::fs::write(&metadata, serde_json::json!({ "title": title }).to_string())
          .await
          .map_err(|e| input_error(&self.id, format!("cannot stage metadata: {e}")))?;
        args.push("--metadata-file".into());
        args.push(metadata.clone().into());
      }
      if matches!(inputs.get("toc"), Some(FlowValue::Json(Value::Bool(true)))) {
        args.push("--toc".into());
      }
      match extension {
        "html" => args.push("--embed-resources".into()),
        "pdf" => args.push("--pdf-engine=typst".into()),
        _ => {}
      }
      run_pandoc(&self.id, &workdir, &args, extension == "pdf").await?;
      tokio::fs::read(&target)
        .await
        .map_err(|e| YanshiError::NodeExecutionFailed {
          message: format!("{}: pandoc wrote no output: {e}", self.id),
        })
    }
    .await;
    for path in cleanup {
      let _ = tokio::fs::remove_file(path).await;
    }
    let bytes = rendered?;
    Ok(output(
      &self.id,
      write_artifact(&self.id, &bytes, mime).await?,
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use yanshi_core::run_context;

  fn inputs(pairs: &[(&str, Value)]) -> AsyncNodeInputs {
    pairs
      .iter()
      .map(|(k, v)| (k.to_string(), FlowValue::Json(v.clone())))
      .collect()
  }

  async fn run(node: &dyn AsyncNode, inputs: AsyncNodeInputs) -> (std::path::PathBuf, String) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = tmp.keep();
    let outputs = run_context::with_artifact_root(root, node.execute(&inputs))
      .await
      .expect("renders");
    let (_, value) = outputs.into_iter().next().expect("one output");
    let FlowValue::File { path, mime_type } = value else {
      panic!("expected a File");
    };
    (path, mime_type.unwrap_or_default())
  }

  #[tokio::test]
  async fn table_csv_round_trips_object_rows_in_first_seen_column_order() {
    let rows = json!([{"name": "Ada", "year": 1843}, {"name": "Grace", "note": "COBOL"}]);
    let (path, mime) = run(&RenderTableNode::new("t"), inputs(&[("rows", rows)])).await;
    assert_eq!(mime, "text/csv");
    let mut reader = csv::Reader::from_path(&path).expect("csv");
    let header: Vec<String> = reader
      .headers()
      .expect("header")
      .iter()
      .map(String::from)
      .collect();
    assert_eq!(header.len(), 3);
    let records: Vec<Vec<String>> = reader
      .records()
      .map(|r| r.expect("record").iter().map(String::from).collect())
      .collect();
    let col = |name: &str| header.iter().position(|h| h == name).expect("column");
    assert_eq!(records[0][col("name")], "Ada");
    assert_eq!(records[0][col("year")], "1843");
    assert_eq!(records[1][col("note")], "COBOL");
    assert_eq!(records[1][col("year")], "", "missing cells are empty");
  }

  #[tokio::test]
  async fn table_accepts_a_json_string_and_explicit_columns() {
    let rows = Value::String(r#"[["a", 1], ["b", 2]]"#.to_string());
    let (path, _) = run(
      &RenderTableNode::new("t"),
      inputs(&[("rows", rows), ("columns", json!(["letter", "n"]))]),
    )
    .await;
    let text = std::fs::read_to_string(path).expect("csv");
    assert_eq!(text, "letter,n\na,1\nb,2\n");
  }

  #[tokio::test]
  async fn table_xlsx_is_a_zip_workbook_with_the_cells() {
    let rows = json!([{"city": "Lisbon", "pop": 545000}]);
    let (path, mime) = run(
      &RenderTableNode::new("t"),
      inputs(&[("rows", rows), ("format", json!("xlsx"))]),
    )
    .await;
    assert_eq!(mime, XLSX_MIME);
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("xlsx"));
    let mut archive =
      zip::ZipArchive::new(std::fs::File::open(&path).expect("xlsx")).expect("a zip container");
    let mut read = |name: &str| {
      let mut text = String::new();
      std::io::Read::read_to_string(&mut archive.by_name(name).expect(name), &mut text)
        .expect("utf-8 xml");
      text
    };
    let sheet = read("xl/worksheets/sheet1.xml");
    let strings = read("xl/sharedStrings.xml");
    assert!(
      strings.contains("city") && strings.contains("Lisbon"),
      "{strings}"
    );
    assert!(
      sheet.contains("<v>545000</v>"),
      "the number is stored as a number cell: {sheet}"
    );
  }

  #[tokio::test]
  async fn table_rejects_mixed_rows() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let err = run_context::with_artifact_root(
      tmp.path().to_path_buf(),
      RenderTableNode::new("t").execute(&inputs(&[("rows", json!([{"a": 1}, [1]]))])),
    )
    .await
    .expect_err("mixed rows");
    assert!(
      err.to_string().contains("all objects or all arrays"),
      "{err}"
    );
  }

  const SQUARE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
<rect width="20" height="20" fill="#ff0000"/><rect x="20" width="20" height="20" fill="#0000ff"/></svg>"##;

  #[tokio::test]
  async fn svg_renders_to_a_png_of_the_declared_size() {
    let (path, mime) = run(&RenderSvgNode::new("s"), inputs(&[("svg", json!(SQUARE))])).await;
    assert_eq!(mime, "image/png");
    let png = resvg::tiny_skia::Pixmap::load_png(&path).expect("decodes as PNG");
    assert_eq!((png.width(), png.height()), (40, 20));
    let left = png.pixel(5, 10).expect("pixel");
    let right = png.pixel(35, 10).expect("pixel");
    assert_eq!((left.red(), left.blue()), (255, 0), "left half is red");
    assert_eq!((right.red(), right.blue()), (0, 255), "right half is blue");
  }

  #[tokio::test]
  async fn svg_width_scales_the_output() {
    let (path, _) = run(
      &RenderSvgNode::new("s"),
      inputs(&[("svg", json!(SQUARE)), ("width", json!(400))]),
    )
    .await;
    let png = resvg::tiny_skia::Pixmap::load_png(&path).expect("png");
    assert_eq!((png.width(), png.height()), (400, 200));
  }

  #[tokio::test]
  async fn svg_rejects_a_huge_canvas() {
    let huge = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="100000"/>"#;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let err = run_context::with_artifact_root(
      tmp.path().to_path_buf(),
      RenderSvgNode::new("s").execute(&inputs(&[("svg", json!(huge))])),
    )
    .await
    .expect_err("too big");
    assert!(err.to_string().contains("pixels"), "{err}");
  }

  #[tokio::test]
  async fn markdown_page_is_standalone_and_escapes_raw_html() {
    let markdown = "# Tides\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n<script>alert(1)</script>\n";
    let (path, mime) = run(
      &RenderMarkdownNode::new("m"),
      inputs(&[("markdown", json!(markdown))]),
    )
    .await;
    assert_eq!(mime, "text/html");
    let html = std::fs::read_to_string(path).expect("html");
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<title>Tides</title>"));
    assert!(html.contains("<h1>Tides</h1>"));
    assert!(html.contains("<table>") && html.contains("<td>1</td>"));
    assert!(html.contains("<style>"), "stylesheet is inline");
    assert!(!html.contains("<script>"), "raw HTML must not pass through");
    assert!(html.contains("&lt;script&gt;"));
  }

  /// Y3.3 acceptance. Needs `pandoc` and `typst` on PATH:
  /// `cargo test -p yanshi-nodes --lib render_document -- --ignored`.
  #[tokio::test]
  #[ignore = "needs pandoc and typst"]
  async fn render_document_pdf_has_pages_and_docx_reads_back() {
    let markdown =
      "# 潮汐\n\nTides follow **the moon**. 潮汐由月球引力引起。\n\n<script>x</script>\n";
    let (pdf, mime) = run(
      &RenderDocumentNode::new("doc"),
      inputs(&[
        ("markdown", json!(markdown)),
        ("format", json!("pdf")),
        ("title", json!("It's a \"report\"")),
      ]),
    )
    .await;
    assert_eq!(mime, "application/pdf");
    let bytes = std::fs::read(&pdf).expect("pdf");
    assert!(bytes.starts_with(b"%PDF"));
    let text = String::from_utf8_lossy(&bytes);
    let pages: u32 = text
      .split("/Count ")
      .skip(1)
      .filter_map(|rest| {
        rest
          .split(|c: char| !c.is_ascii_digit())
          .next()?
          .parse()
          .ok()
      })
      .max()
      .unwrap_or(0);
    assert!(pages > 0, "PDF page count must be > 0");

    let (docx, mime) = run(
      &RenderDocumentNode::new("doc"),
      inputs(&[("markdown", json!(markdown)), ("format", json!("docx"))]),
    )
    .await;
    assert!(mime.ends_with("wordprocessingml.document"));
    let back = std::process::Command::new("pandoc")
      .arg(&docx)
      .args(["-t", "plain"])
      .output()
      .expect("pandoc reads the docx back");
    let body = String::from_utf8_lossy(&back.stdout);
    assert!(body.contains("潮汐由月球引力引起"), "{body}");
    assert!(
      body.contains("<script>x</script>"),
      "raw HTML stays literal text: {body}"
    );
    let leftovers: Vec<_> = std::fs::read_dir(docx.parent().expect("dir"))
      .expect("dir")
      .flatten()
      .filter(|e| e.file_name().to_string_lossy().starts_with(".render-"))
      .collect();
    assert!(leftovers.is_empty(), "staging files are removed");
  }

  #[tokio::test]
  async fn render_document_rejects_an_unknown_format_before_running_pandoc() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let err = run_context::with_artifact_root(
      tmp.path().to_path_buf(),
      RenderDocumentNode::new("d").execute(&inputs(&[
        ("markdown", json!("# x")),
        ("format", json!("odt")),
      ])),
    )
    .await
    .expect_err("unknown format");
    assert!(err.to_string().contains("unknown format 'odt'"), "{err}");
  }
}
