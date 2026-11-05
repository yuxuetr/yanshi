//! `figure` (Y3.4, `docs/RFC_CONTENT_STUDIO.md` §5): an LLM writes the
//! source of a figure in a figure language, a real renderer compiles it,
//! and a compile error is fed back for another attempt.
//!
//! Rendering *is* the validation: a source that does not compile is a
//! failed attempt whose error text becomes part of the next prompt, up to
//! `max_attempts` (default 3). Each retry is reported on the run's event
//! stream as `node.progress` with `kind: "figure_retry"`.
//!
//! Backends (`language`):
//! - `svg`: the source is the SVG; checked in-process with resvg.
//! - `tikz`: a standalone LaTeX document compiled with `lualatex` (no
//!   shell escape), then `pdftocairo -svg`. circuitikz / chemfig /
//!   pgfplots / tikz-cd are whatever the TeX Live install provides. The
//!   font cache (`TEXMFVAR`) lives in the run directory.
//! - `typst`: `typst compile` straight to SVG; packages must already be
//!   in typst's cache (the sandbox has no network).
//! - `python` (Y3.5): a script that saves `figure.svg`, run in the
//!   `code_exec` container tier (no network, read-only rootfs, only the
//!   work directory writable) on the image built from
//!   `scripts/figure_python/Containerfile`: matplotlib, RDKit,
//!   dna_features_viewer, biopython, networkx. `YANSHI_FIGURE_PYTHON_IMAGE`
//!   overrides the tag.
//!
//! Domains (`domain`, all tikz only) add conventions to the prompt and,
//! with a `spec`, check the source against it before every render, so a
//! figure that compiles but leaves something out is sent back too:
//! `circuit` (Y6.5, circuitikz, a parts list), `chemistry` (Y8.4,
//! chemfig, molecules with formulas) and `block` (Y8.4, TikZ, blocks and
//! links). See the module of each.
//!
//! The external renderers run under the OS sandbox
//! (`yanshi_nodes::common::sandboxed`), writing only inside a per-call
//! work directory. Outputs: `<id>_svg` (File), `<id>_png` (File, when
//! `png: true`), `<id>_source` (the source that rendered) and
//! `<id>_attempts`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use yanshi_core::{
  async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
  error::YanshiError,
  run_context,
  value::FlowValue,
};
use yanshi_llm::Yanshi;
use yanshi_nodes::common::{artifacts, sandboxed};

mod block;
mod chemistry;
mod circuit;

const DEFAULT_MAX_ATTEMPTS: u64 = 3;
/// Image for `language: python`; see `scripts/figure_python/Containerfile`.
const DEFAULT_PYTHON_IMAGE: &str = "yanshi-figure-python:latest";
const PYTHON_TIMEOUT: Duration = Duration::from_secs(180);
const RENDER_TIMEOUT: Duration = Duration::from_secs(120);

/// The figure languages this node renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
  Svg,
  Tikz,
  Typst,
  Python,
}

impl Language {
  fn parse(raw: &str) -> Option<Self> {
    match raw {
      "svg" => Some(Self::Svg),
      "tikz" => Some(Self::Tikz),
      "typst" => Some(Self::Typst),
      "python" => Some(Self::Python),
      _ => None,
    }
  }

  fn as_str(self) -> &'static str {
    match self {
      Self::Svg => "svg",
      Self::Tikz => "tikz",
      Self::Typst => "typst",
      Self::Python => "python",
    }
  }

  /// What the model is told about the language it must write.
  fn instructions(self) -> &'static str {
    match self {
      Self::Svg => {
        "Write one complete standalone SVG document: an <svg> root with \
         xmlns=\"http://www.w3.org/2000/svg\" and explicit width and height. \
         No scripts and no external references."
      }
      Self::Tikz => {
        "Write one complete LaTeX document using \\documentclass[border=4pt]{standalone}. \
         It is compiled with lualatex without shell escape; tikz, circuitikz, chemfig, \
         pgfplots and tikz-cd are available."
      }
      Self::Typst => {
        "Write one complete Typst document that starts with \
         #set page(width: auto, height: auto, margin: 4pt) and produces exactly one page. \
         There is no network: only packages already cached (such as @preview/cetz) can be imported."
      }
      Self::Python => {
        "Write one complete Python 3 script that draws the figure and saves it as SVG to \
         ./figure.svg. matplotlib (Agg backend: plt.savefig(\"figure.svg\")), rdkit (use \
         rdkit.Chem.Draw.rdMolDraw2D.MolDraw2DSVG and write GetDrawingText()), \
         dna_features_viewer, biopython (Bio.Phylo trees drawn with matplotlib) and networkx \
         are installed; nothing else is, and there is no network."
      }
    }
  }
}

/// A figure domain with its parsed `spec`.
enum Domain {
  Circuit(Vec<circuit::Component>),
  Chemistry(chemistry::Scheme),
  Block(block::Diagram),
}

impl Domain {
  fn parse(name: &str, language: Language, spec: Option<&Value>) -> Result<Self, String> {
    let domain = match name {
      "circuit" => Self::Circuit(circuit::parse_spec(spec)?),
      "chemistry" => Self::Chemistry(chemistry::parse_spec(spec)?),
      "block" => Self::Block(block::parse_spec(spec)?),
      other => {
        return Err(format!(
          "unknown domain '{other}': expected circuit, chemistry or block"
        ));
      }
    };
    if language != Language::Tikz {
      return Err(format!("domain {name} needs language tikz"));
    }
    Ok(domain)
  }

  fn instructions(&self) -> String {
    match self {
      Self::Circuit(parts) => circuit::instructions(parts),
      Self::Chemistry(scheme) => chemistry::instructions(scheme),
      Self::Block(diagram) => block::instructions(diagram),
    }
  }

  fn check(&self, source: &str) -> Result<(), String> {
    match self {
      Self::Circuit(parts) => circuit::check(source, parts),
      Self::Chemistry(scheme) => chemistry::check(source, scheme),
      Self::Block(diagram) => block::check(source, diagram),
    }
  }
}

pub struct FigureNode {
  id: String,
}

impl FigureNode {
  pub fn new(id: impl Into<String>) -> Self {
    Self { id: id.into() }
  }

  fn input_error(&self, message: impl std::fmt::Display) -> YanshiError {
    YanshiError::NodeInputError {
      message: format!("{}: {message}", self.id),
    }
  }
}

fn string_input<'a>(inputs: &'a AsyncNodeInputs, key: &str) -> Option<&'a str> {
  match inputs.get(key) {
    Some(FlowValue::Json(Value::String(s))) if !s.trim().is_empty() => Some(s.as_str()),
    _ => None,
  }
}

/// The source inside the reply's first fenced code block, or the whole
/// reply when there is none.
fn extract_source(reply: &str) -> String {
  let mut parts = reply.split("```");
  match (parts.next(), parts.next()) {
    (Some(_), Some(block)) => match block.split_once('\n') {
      // Drop an info string like ```latex or ```svg.
      Some((info, body)) if !info.contains('<') && !info.contains('\\') => body.trim().to_string(),
      _ => block.trim().to_string(),
    },
    _ => reply.trim().to_string(),
  }
}

fn prompt_for(language: Language, description: &str, failed: Option<(&str, &str)>) -> String {
  let mut prompt = format!("Draw this figure:\n\n{description}\n");
  if let Some((source, error)) = failed {
    prompt.push_str(&format!(
      "\nYour previous source did not render.\n\nSource:\n```\n{source}\n```\n\n\
       Renderer error:\n```\n{error}\n```\n\nReturn the corrected, complete {} source.\n",
      language.as_str()
    ));
  }
  prompt
}

/// The `!`-lines of a LaTeX log and the two lines after each: the part
/// of a failed lualatex run a model can act on.
fn latex_errors(log: &str) -> Option<String> {
  let lines: Vec<&str> = log.lines().collect();
  let mut picked = Vec::new();
  for (i, line) in lines.iter().enumerate() {
    if line.starts_with('!') {
      picked.extend(lines[i..lines.len().min(i + 3)].iter().copied());
    }
  }
  (!picked.is_empty()).then(|| picked.join("\n"))
}

/// typst's package cache and data directories (read-only in the sandbox).
fn typst_package_dirs() -> Vec<PathBuf> {
  let mut dirs = Vec::new();
  if let Some(cache) = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
    dirs.push(cache.join("typst"));
  }
  if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
    dirs.extend([
      home.join("Library/Caches/typst"),
      home.join("Library/Application Support/typst"),
      home.join(".cache/typst"),
      home.join(".local/share/typst"),
    ]);
  }
  dirs
}

/// Outcome of one render: the SVG, or an error message for the model.
type Rendered = Result<String, String>;

impl FigureNode {
  async fn render(&self, language: Language, source: &str) -> Result<Rendered, YanshiError> {
    if language == Language::Svg {
      return Ok(yanshi_nodes::nodes::render::check_svg(source).map(|()| source.to_string()));
    }
    let node_dir = artifacts::artifact_dir(&self.id);
    let workdir = node_dir.join(format!(".figure-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workdir)
      .await
      .map_err(|e| self.input_error(format!("cannot create {}: {e}", workdir.display())))?;
    let result = match language {
      Language::Tikz => self.render_tikz(&node_dir, &workdir, source).await,
      Language::Typst => self.render_typst(&workdir, source).await,
      Language::Python => self.render_python(&workdir, source).await,
      Language::Svg => unreachable!("handled before the work directory is created"),
    };
    let _ = tokio::fs::remove_dir_all(&workdir).await;
    result
  }

  fn tool(&self, name: &str) -> Result<PathBuf, YanshiError> {
    sandboxed::find_tool(name).ok_or_else(|| YanshiError::NodeExecutionFailed {
      message: format!(
        "{}: {name} is not installed (`yanshi doctor` lists renderer binaries)",
        self.id
      ),
    })
  }

  async fn render_tikz(
    &self,
    node_dir: &Path,
    workdir: &Path,
    source: &str,
  ) -> Result<Rendered, YanshiError> {
    let lualatex = self.tool("lualatex")?;
    let pdftocairo = self.tool("pdftocairo")?;
    tokio::fs::write(workdir.join("fig.tex"), source)
      .await
      .map_err(|e| self.input_error(format!("cannot stage the source: {e}")))?;
    // lualatex's font cache: one per run (the artifacts dir), so only the
    // first tikz figure of a run pays for building it.
    let texmf_var = node_dir.parent().unwrap_or(node_dir).join(".texmf-var");
    tokio::fs::create_dir_all(&texmf_var)
      .await
      .map_err(|e| self.input_error(format!("cannot create {}: {e}", texmf_var.display())))?;
    let latex = sandboxed::run(sandboxed::SandboxedRun {
      node: &self.id,
      program: lualatex,
      args: [
        "-interaction=nonstopmode",
        "-halt-on-error",
        "-no-shell-escape",
        "fig.tex",
      ]
      .map(Into::into)
      .to_vec(),
      workdir,
      read_paths: sandboxed::font_dirs(),
      write_paths: vec![texmf_var.clone()],
      env: vec![("TEXMFVAR", texmf_var.into_os_string())],
      timeout: RENDER_TIMEOUT,
    })
    .await?;
    if !latex.status.success() {
      let log = tokio::fs::read_to_string(workdir.join("fig.log"))
        .await
        .unwrap_or_default();
      return Ok(Err(
        latex_errors(&log).unwrap_or_else(|| sandboxed::output_tail(&latex, 20)),
      ));
    }
    let cairo = sandboxed::run(sandboxed::SandboxedRun {
      node: &self.id,
      program: pdftocairo,
      args: ["-svg", "fig.pdf", "fig.svg"].map(Into::into).to_vec(),
      workdir,
      read_paths: sandboxed::font_dirs(),
      write_paths: Vec::new(),
      env: Vec::new(),
      timeout: RENDER_TIMEOUT,
    })
    .await?;
    if !cairo.status.success() {
      return Ok(Err(format!(
        "pdftocairo failed: {}",
        sandboxed::output_tail(&cairo, 10)
      )));
    }
    self.read_svg(workdir).await
  }

  async fn render_typst(&self, workdir: &Path, source: &str) -> Result<Rendered, YanshiError> {
    let typst = self.tool("typst")?;
    tokio::fs::write(workdir.join("fig.typ"), source)
      .await
      .map_err(|e| self.input_error(format!("cannot stage the source: {e}")))?;
    let mut read_paths = sandboxed::font_dirs();
    read_paths.extend(typst_package_dirs());
    let run = sandboxed::run(sandboxed::SandboxedRun {
      node: &self.id,
      program: typst,
      args: ["compile", "fig.typ", "fig.svg"].map(Into::into).to_vec(),
      workdir,
      read_paths,
      write_paths: Vec::new(),
      env: Vec::new(),
      timeout: RENDER_TIMEOUT,
    })
    .await?;
    if !run.status.success() {
      return Ok(Err(sandboxed::output_tail(&run, 20)));
    }
    self.read_svg(workdir).await
  }

  /// Run the script in the code_exec container tier. Unlike the other
  /// renderers there is no degraded mode: without a container engine the
  /// node fails rather than running model-written code on the host.
  async fn render_python(&self, workdir: &Path, source: &str) -> Result<Rendered, YanshiError> {
    use yanshi_tools::{
      Capability,
      sandbox::{ContainerBackend, SandboxBackend, SandboxScope},
    };
    let image = std::env::var("YANSHI_FIGURE_PYTHON_IMAGE")
      .ok()
      .filter(|v| !v.trim().is_empty())
      .unwrap_or_else(|| DEFAULT_PYTHON_IMAGE.to_string());
    let backend = ContainerBackend::new(image);
    if !backend.is_enforcing() {
      return Err(YanshiError::NodeExecutionFailed {
        message: format!(
          "{}: language python needs a container engine (Apple `container` or rootless Podman); \
           `yanshi doctor` reports it",
          self.id
        ),
      });
    }
    tokio::fs::write(workdir.join("fig.py"), source)
      .await
      .map_err(|e| self.input_error(format!("cannot stage the script: {e}")))?;
    let canonical = workdir
      .canonicalize()
      .map_err(|e| self.input_error(format!("cannot resolve {}: {e}", workdir.display())))?;
    let scope = SandboxScope::new()
      .with_working_directory(&canonical)
      .with_max_memory_bytes(1024 * 1024 * 1024)
      .with_max_cpu_secs(120)
      .with_max_pids(64)
      .with_container_name(format!("yanshi-figure-{}", uuid::Uuid::new_v4()));
    let mut command = tokio::process::Command::new("python3");
    command
      .arg("fig.py")
      .current_dir(&canonical)
      .kill_on_drop(true);
    backend
      .wrap_command(
        &mut command,
        &[Capability::FsRead, Capability::FsWrite, Capability::Exec],
        &scope,
      )
      .map_err(|e| YanshiError::NodeExecutionFailed {
        message: format!("{}: preparing the container failed: {e}", self.id),
      })?;
    let output = match tokio::time::timeout(PYTHON_TIMEOUT, command.output()).await {
      Ok(output) => output.map_err(|e| YanshiError::NodeExecutionFailed {
        message: format!("{}: starting the container failed: {e}", self.id),
      })?,
      Err(_) => {
        backend.terminate(&scope);
        return Ok(Err(format!(
          "the script ran longer than {}s",
          PYTHON_TIMEOUT.as_secs()
        )));
      }
    };
    if !output.status.success() {
      return Ok(Err(sandboxed::output_tail(&output, 25)));
    }
    match tokio::fs::read_to_string(canonical.join("figure.svg")).await {
      Ok(svg) if !svg.trim().is_empty() => Ok(Ok(svg)),
      Ok(_) => Ok(Err("the script wrote an empty figure.svg".to_string())),
      Err(_) => Ok(Err(
        "the script finished but wrote no ./figure.svg".to_string(),
      )),
    }
  }

  async fn read_svg(&self, workdir: &Path) -> Result<Rendered, YanshiError> {
    match tokio::fs::read_to_string(workdir.join("fig.svg")).await {
      Ok(svg) if !svg.trim().is_empty() => Ok(Ok(svg)),
      Ok(_) => Ok(Err("the renderer produced an empty SVG".to_string())),
      Err(e) => Ok(Err(format!("the renderer produced no SVG: {e}"))),
    }
  }

  async fn generate(
    &self,
    model: &str,
    prompt: String,
    language: Language,
    domain: Option<&str>,
  ) -> Result<String, YanshiError> {
    let mut system = format!(
      "You draw figures by writing source code. {} Reply with exactly one fenced code block \
       containing the complete source and nothing else.",
      language.instructions()
    );
    if let Some(domain) = domain {
      system.push_str("\n\n");
      system.push_str(domain);
    }
    let reply = Yanshi::model(model)
      .system(&system)
      .prompt(&prompt)
      .execute()
      .await
      .map_err(|e| YanshiError::NodeExecutionFailed {
        message: format!("{}: the model call failed: {e}", self.id),
      })?;
    Ok(extract_source(&reply))
  }
}

#[async_trait]
impl AsyncNode for FigureNode {
  async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult {
    let language = string_input(inputs, "language")
      .ok_or_else(|| self.input_error("missing input 'language' (svg, tikz or typst)"))?;
    let language = Language::parse(language.trim()).ok_or_else(|| {
      self.input_error(format!(
        "unsupported language '{language}': expected svg, tikz, typst or python"
      ))
    })?;
    let description = string_input(inputs, "description");
    let model = string_input(inputs, "model");
    let max_attempts = match inputs.get("max_attempts") {
      Some(FlowValue::Json(Value::Number(n))) => n.as_u64().unwrap_or(DEFAULT_MAX_ATTEMPTS),
      _ => DEFAULT_MAX_ATTEMPTS,
    }
    .clamp(1, 10);
    // Y6.5 / Y8.4: a domain's spec, checked before every render.
    let domain = match string_input(inputs, "domain").map(str::trim) {
      None => None,
      Some(name) => {
        let spec = match inputs.get("spec") {
          Some(FlowValue::Json(Value::String(text))) if !text.trim().is_empty() => Some(
            serde_json::from_str::<Value>(text)
              .map_err(|e| self.input_error(format!("spec is not JSON: {e}")))?,
          ),
          Some(FlowValue::Json(Value::String(_))) | None => None,
          Some(FlowValue::Json(value)) => Some(value.clone()),
          Some(_) => return Err(self.input_error("spec must be JSON")),
        };
        Some(Domain::parse(name, language, spec.as_ref()).map_err(|e| self.input_error(e))?)
      }
    };
    let domain_text = domain.as_ref().map(Domain::instructions);
    let can_generate = description.is_some() && model.is_some();
    if string_input(inputs, "source").is_none() && !can_generate {
      return Err(self.input_error("give 'source', or 'description' and 'model'"));
    }

    let mut source = match string_input(inputs, "source") {
      Some(source) => source.to_string(),
      None => {
        let prompt = prompt_for(language, description.unwrap_or_default(), None);
        self
          .generate(
            model.unwrap_or_default(),
            prompt,
            language,
            domain_text.as_deref(),
          )
          .await?
      }
    };
    let mut attempt: u64 = 1;
    let svg = loop {
      let rendered = match domain.as_ref().map(|domain| domain.check(&source)) {
        Some(Err(violations)) => Err(violations),
        _ => self.render(language, &source).await?,
      };
      match rendered {
        Ok(svg) => break svg,
        Err(error) if attempt < max_attempts && can_generate => {
          run_context::report_progress(
            "figure_retry",
            json!({ "attempt": attempt, "language": language.as_str(), "error": error }),
          );
          let prompt = prompt_for(
            language,
            description.unwrap_or_default(),
            Some((&source, &error)),
          );
          source = self
            .generate(
              model.unwrap_or_default(),
              prompt,
              language,
              domain_text.as_deref(),
            )
            .await?;
          attempt += 1;
        }
        Err(error) => {
          return Err(YanshiError::NodeExecutionFailed {
            message: format!(
              "{}: the {} source did not render after {attempt} attempt(s): {error}",
              self.id,
              language.as_str()
            ),
          });
        }
      }
    };

    let mut outputs = HashMap::from([
      (
        format!("{}_svg", self.id),
        artifacts::write_artifact(&self.id, svg.as_bytes(), "image/svg+xml").await?,
      ),
      (
        format!("{}_source", self.id),
        FlowValue::Json(Value::String(source)),
      ),
      (
        format!("{}_attempts", self.id),
        FlowValue::Json(json!(attempt)),
      ),
    ]);
    if matches!(inputs.get("png"), Some(FlowValue::Json(Value::Bool(true)))) {
      let width = match inputs.get("png_width") {
        Some(FlowValue::Json(Value::Number(n))) => n.as_u64().and_then(|w| u32::try_from(w).ok()),
        _ => None,
      };
      let png =
        tokio::task::spawn_blocking(move || yanshi_nodes::nodes::render::svg_to_png(&svg, width))
          .await
          .map_err(|e| YanshiError::NodeExecutionFailed {
            message: format!("{}: PNG task failed: {e}", self.id),
          })?
          .map_err(|e| YanshiError::NodeExecutionFailed {
            message: format!("{}: PNG copy failed: {e}", self.id),
          })?;
      outputs.insert(
        format!("{}_png", self.id),
        artifacts::write_artifact(&self.id, &png, "image/png").await?,
      );
    }
    Ok(outputs)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn extracts_the_first_fenced_block_without_its_info_string() {
    assert_eq!(
      extract_source("Here:\n```svg\n<svg/>\n```\nthanks"),
      "<svg/>"
    );
    assert_eq!(
      extract_source("```\n\\documentclass{standalone}\n```"),
      "\\documentclass{standalone}"
    );
    assert_eq!(extract_source("  <svg/>  "), "<svg/>");
  }

  #[test]
  fn latex_errors_keep_the_bang_lines_and_context() {
    let log = "This is LuaHBTeX\n! Undefined control sequence.\nl.4 \\drawx\n (0,0)\nmore\n";
    assert_eq!(
      latex_errors(log).as_deref(),
      Some("! Undefined control sequence.\nl.4 \\drawx\n (0,0)")
    );
    assert_eq!(latex_errors("all good"), None);
  }

  #[test]
  fn retry_prompt_carries_the_failed_source_and_error() {
    let prompt = prompt_for(
      Language::Tikz,
      "an RC circuit",
      Some(("\\bad", "! Undefined")),
    );
    assert!(prompt.contains("an RC circuit"));
    assert!(prompt.contains("\\bad") && prompt.contains("! Undefined"));
    assert!(prompt.contains("corrected, complete tikz source"));
  }
}
