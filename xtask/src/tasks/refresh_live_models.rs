//! `refresh-live-models` (P10.3.4) — for each provider wired into the
//! `llm-live` nightly workflow, ping the provider's `/models`
//! endpoint and verify the hard-coded text-model default still
//! exists. Reports per-provider status + suggests replacements
//! when the default 404s.
//!
//! Run `cargo xtask refresh-live-models` to validate the hard-coded text-model
//! defaults in `yanshi-llm/tests/provider_consistency_live.rs` against each
//! provider's live `/models` endpoint. Loads API keys from `~/.yanshi/.env`
//! when present, falling back to the ambient environment (which is what the
//! `llm-live.yml` workflow uses on CI). Providers whose key is missing
//! self-skip with a clear message rather than fail the run.
//!
//! Output (one row per provider): `status default-id (n alternatives shown)`.
//! Status is one of:
//!   - `ok`       — the hard-coded default appears in the provider's model list
//!   - `missing`  — the default is NOT in the list; treat as a vendor-side
//!     deprecation, copy a suggested replacement into the test
//!     source or override via `YANSHI_LIVE_<P>_TEXT_MODEL` in
//!     `.github/workflows/llm-live.yml::env`
//!   - `skipped`  — no API key found for this provider
//!   - `error`    — HTTP request failed (network / auth / parse). Exit non-zero
//!     unless --keep-going.
//!
//! Flags:
//!   --keep-going        don't exit non-zero on per-provider HTTP errors
//!   --include <name>    repeatable; restrict to a subset of providers
//!
//! The output is the source of truth — the operator copy-pastes the suggested
//! replacement into `provider_consistency_live.rs::run_text_path` (the only
//! place these defaults live). No auto-edit: source-code mutation from a CLI
//! helper is too easy to get wrong, and the test file's defaults carry
//! explanatory comments that an automated rewrite would clobber.

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const REFRESH_LIVE_MODELS_PROBES: &[LiveModelProbe] = &[
  LiveModelProbe {
    name: "openai",
    key_envs: &["OPENAI_API_KEY"],
    default_text_model: "gpt-4o-mini",
    endpoint: LiveModelsEndpoint::OpenAICompat("https://api.openai.com/v1/models"),
  },
  LiveModelProbe {
    name: "anthropic",
    key_envs: &["ANTHROPIC_API_KEY"],
    default_text_model: "claude-haiku-4-5",
    endpoint: LiveModelsEndpoint::Anthropic("https://api.anthropic.com/v1/models"),
  },
  LiveModelProbe {
    name: "google",
    key_envs: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    // Listed is not callable: `gemini-2.5-flash` stays in `/models` but
    // 404s for new keys, which this listing check cannot see.
    default_text_model: "gemini-3.5-flash",
    endpoint: LiveModelsEndpoint::Google("https://generativelanguage.googleapis.com/v1beta/models"),
  },
  LiveModelProbe {
    name: "moonshot",
    key_envs: &["MOONSHOT_API_KEY"],
    // The moonshot-v1-* lineage was retired (404 since 2026-08-31).
    default_text_model: "kimi-k2.6",
    endpoint: LiveModelsEndpoint::OpenAICompat("https://api.moonshot.cn/v1/models"),
  },
  LiveModelProbe {
    name: "stepfun",
    key_envs: &["STEPFUN_API_KEY", "STEP_API_KEY"],
    // The whole step-1-*/step-2-* lineage was retired; step-3.5-flash is
    // the current text-reasoning default. Mirror the live test
    // source-of-truth in `yanshi-llm/tests/provider_consistency_live.rs`.
    default_text_model: "step-3.5-flash",
    endpoint: LiveModelsEndpoint::OpenAICompat("https://api.stepfun.com/v1/models"),
  },
  LiveModelProbe {
    name: "glm",
    key_envs: &["GLM_API_KEY", "BIGMODEL_API_KEY", "ZHIPU_API_KEY"],
    // `glm-4.5-flash` was de-listed; `glm-5.1` is the current Zhipu
    // BigModel default. Mirror the live test source-of-truth in
    // `yanshi-llm/tests/provider_consistency_live.rs`.
    default_text_model: "glm-5.1",
    endpoint: LiveModelsEndpoint::OpenAICompat("https://open.bigmodel.cn/api/paas/v4/models"),
  },
  LiveModelProbe {
    name: "dashscope",
    key_envs: &["DASHSCOPE_API_KEY"],
    default_text_model: "qwen-plus",
    endpoint: LiveModelsEndpoint::OpenAICompat(
      "https://dashscope.aliyuncs.com/compatible-mode/v1/models",
    ),
  },
  LiveModelProbe {
    name: "deepseek",
    key_envs: &["DEEPSEEK_API_KEY"],
    default_text_model: "deepseek-v4-flash",
    endpoint: LiveModelsEndpoint::OpenAICompat("https://api.deepseek.com/v1/models"),
  },
  LiveModelProbe {
    name: "minimax",
    key_envs: &["MINIMAX_API_KEY"],
    default_text_model: "MiniMax-M2",
    endpoint: LiveModelsEndpoint::OpenAICompat("https://api.minimaxi.com/v1/models"),
  },
];

#[derive(Debug, Clone, Copy)]
struct LiveModelProbe {
  name: &'static str,
  key_envs: &'static [&'static str],
  default_text_model: &'static str,
  endpoint: LiveModelsEndpoint,
}

#[derive(Debug, Clone, Copy)]
enum LiveModelsEndpoint {
  /// `Authorization: Bearer <key>` + `GET <url>`. Response shape:
  /// `{"data": [{"id": "model-id"}, ...]}`. Used by OpenAI itself and
  /// every OpenAI-compat vendor (Moonshot / StepFun / GLM / DashScope /
  /// DeepSeek / MiniMax).
  OpenAICompat(&'static str),
  /// `x-api-key: <key>` + `anthropic-version: 2023-06-01` + `GET <url>`.
  /// Response shape: `{"data": [{"id": "..."}, ...]}` (same outer shape
  /// as OpenAI, but the auth headers differ).
  Anthropic(&'static str),
  /// `GET <url>?key=<key>`. Response shape:
  /// `{"models": [{"name": "models/gemini-..."}, ...]}` (note the leading
  /// `models/` prefix on each name).
  Google(&'static str),
}

#[derive(Debug, Clone)]
struct LiveProbeOutcome {
  provider: &'static str,
  /// `Some(id)` when a key was found and the request succeeded;
  /// `None` when the provider was skipped.
  default_text_model: String,
  status: ProbeStatus,
}

#[derive(Debug, Clone)]
enum ProbeStatus {
  Ok,
  Missing { suggestions: Vec<String> },
  Skipped { reason: String },
  Error { reason: String },
}

pub fn refresh_live_models_from_args(
  workspace_root: &Path,
  args: Vec<String>,
  out: &mut impl Write,
  err: &mut impl Write,
) -> Result<()> {
  let mut keep_going = false;
  let mut include: Vec<String> = Vec::new();
  let mut iter = args.into_iter();
  while let Some(arg) = iter.next() {
    match arg.as_str() {
      "--keep-going" => keep_going = true,
      "--include" => {
        let value = iter
          .next()
          .ok_or_else(|| anyhow::anyhow!("--include requires a provider name"))?;
        include.push(value);
      }
      other => bail!("unknown refresh-live-models flag '{other}'"),
    }
  }

  // Load ~/.yanshi/.env into the process environment, mirroring the
  // existing `Yanshi::init()` convention. Silent no-op when the file
  // is missing — that's the expected case on CI, where keys come from
  // the workflow's `env:` block.
  let env_path = std::env::var_os("HOME")
    .map(PathBuf::from)
    .map(|home| home.join(".yanshi").join(".env"));
  if let Some(path) = env_path.as_ref()
    && path.exists()
  {
    let loaded = load_dotenv_file(path)?;
    let _ = writeln!(
      out,
      "[refresh] loaded {} key(s) from {}",
      loaded,
      path.display()
    );
  }

  let probes: Vec<LiveModelProbe> = if include.is_empty() {
    REFRESH_LIVE_MODELS_PROBES.to_vec()
  } else {
    let include_set: BTreeSet<&str> = include.iter().map(String::as_str).collect();
    REFRESH_LIVE_MODELS_PROBES
      .iter()
      .copied()
      .filter(|p| include_set.contains(p.name))
      .collect()
  };
  if probes.is_empty() {
    bail!(
      "no providers selected (got --include filter with no matching probe). \
       Known providers: {}",
      REFRESH_LIVE_MODELS_PROBES
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
    );
  }

  // Empty `_ = workspace_root`: kept on the signature to mirror the other
  // xtask subcommands (`bench_gate_from_args`, `test_gate_from_args`).
  // Future extensions may want to read the workspace root to locate the
  // canonical defaults via grep, etc.
  let _ = workspace_root;

  let mut outcomes: Vec<LiveProbeOutcome> = Vec::with_capacity(probes.len());
  for probe in &probes {
    let outcome = probe_provider(probe);
    print_probe_line(out, &outcome);
    outcomes.push(outcome);
  }

  // Summary + exit code.
  let mut ok_count = 0usize;
  let mut missing_count = 0usize;
  let mut skipped_count = 0usize;
  let mut error_count = 0usize;
  for o in &outcomes {
    match o.status {
      ProbeStatus::Ok => ok_count += 1,
      ProbeStatus::Missing { .. } => missing_count += 1,
      ProbeStatus::Skipped { .. } => skipped_count += 1,
      ProbeStatus::Error { .. } => error_count += 1,
    }
  }
  let _ = writeln!(
    out,
    "\n[refresh] summary: {ok_count} ok, {missing_count} missing, {skipped_count} skipped, {error_count} error"
  );

  if missing_count > 0 {
    // Missing models are a soft failure — surface them but don't make
    // operators pass --keep-going. The whole point of the tool is to
    // tell you what to fix; that's not a `cargo xtask` exit-nonzero
    // condition.
    let _ = writeln!(
      out,
      "[refresh] {missing_count} provider(s) need attention; copy the suggested replacement into yanshi-llm/tests/provider_consistency_live.rs::run_text_path"
    );
  }
  if error_count > 0 && !keep_going {
    let _ = writeln!(
      err,
      "[refresh] {error_count} provider(s) failed; rerun with --keep-going to ignore HTTP errors"
    );
    bail!("refresh-live-models: {error_count} probe error(s)");
  }
  Ok(())
}

fn probe_provider(probe: &LiveModelProbe) -> LiveProbeOutcome {
  let api_key = match resolve_api_key(probe.key_envs) {
    Some(k) => k,
    None => {
      return LiveProbeOutcome {
        provider: probe.name,
        default_text_model: probe.default_text_model.to_string(),
        status: ProbeStatus::Skipped {
          reason: format!(
            "no key in {} (or HOME/.yanshi/.env)",
            probe.key_envs.join(" / ")
          ),
        },
      };
    }
  };

  let model_ids = match fetch_provider_models(probe, &api_key) {
    Ok(ids) => ids,
    Err(reason) => {
      return LiveProbeOutcome {
        provider: probe.name,
        default_text_model: probe.default_text_model.to_string(),
        status: ProbeStatus::Error {
          reason: reason.to_string(),
        },
      };
    }
  };

  if model_ids.iter().any(|id| id == probe.default_text_model) {
    return LiveProbeOutcome {
      provider: probe.name,
      default_text_model: probe.default_text_model.to_string(),
      status: ProbeStatus::Ok,
    };
  }
  // Rolling-alias-with-dated-revisions fallback. Anthropic's
  // `/v1/models` lists dated revisions like `claude-haiku-4-5-20251001`
  // but not the rolling-alias-style `claude-haiku-4-5`. The alias still
  // resolves to the latest dated revision in real API calls, so a
  // strict-equality check produces a false positive. Mitigate: if any
  // returned id starts with `<default>-` followed by a digit (the
  // start of a YYYY[MM[DD]] date), treat the alias as still valid.
  // The "followed by a digit" guard prevents matching genuinely
  // different families that happen to share the alias as a prefix
  // (e.g. an unrelated `gpt-4o-mini-realtime` shouldn't satisfy
  // `gpt-4o-mini`'s check).
  if let Some(dated) = find_dated_revision(probe.default_text_model, &model_ids) {
    return LiveProbeOutcome {
      provider: probe.name,
      default_text_model: format!("{} (via dated revision {dated})", probe.default_text_model),
      status: ProbeStatus::Ok,
    };
  }
  // Missing: pick up to 3 plausible alternatives. The heuristic is
  // "ids that share the most prefix characters with the current
  // default" — for cases like `claude-3-5-haiku-20241022 →
  // claude-haiku-4-5` that's noisy, but the operator still gets a
  // useful starting set. The ranking is deterministic so two runs
  // produce the same suggestions.
  let mut scored: Vec<(usize, String)> = model_ids
    .into_iter()
    .map(|id| (shared_prefix_len(&id, probe.default_text_model), id))
    .collect();
  scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
  let suggestions: Vec<String> = scored.into_iter().take(3).map(|(_, id)| id).collect();
  LiveProbeOutcome {
    provider: probe.name,
    default_text_model: probe.default_text_model.to_string(),
    status: ProbeStatus::Missing { suggestions },
  }
}

fn print_probe_line(out: &mut impl Write, outcome: &LiveProbeOutcome) {
  match &outcome.status {
    ProbeStatus::Ok => {
      let _ = writeln!(
        out,
        "  ✓ {:<10} ok       default={}",
        outcome.provider, outcome.default_text_model
      );
    }
    ProbeStatus::Missing { suggestions } => {
      let _ = writeln!(
        out,
        "  ✗ {:<10} missing  default={} not in /models",
        outcome.provider, outcome.default_text_model
      );
      if suggestions.is_empty() {
        let _ = writeln!(
          out,
          "      (no alternatives surfaced; manual review needed)"
        );
      } else {
        let _ = writeln!(
          out,
          "      suggested replacements: {}",
          suggestions.join(", ")
        );
      }
    }
    ProbeStatus::Skipped { reason } => {
      let _ = writeln!(out, "  · {:<10} skipped  ({reason})", outcome.provider);
    }
    ProbeStatus::Error { reason } => {
      let _ = writeln!(out, "  ! {:<10} error    {reason}", outcome.provider);
    }
  }
}

fn resolve_api_key(key_envs: &[&str]) -> Option<String> {
  for env_var in key_envs {
    if let Ok(value) = std::env::var(env_var) {
      let trimmed = value.trim();
      if !trimmed.is_empty() {
        return Some(trimmed.to_string());
      }
    }
  }
  None
}

/// Hit the provider's models endpoint via `curl` and return a sorted
/// list of model ids. Uses curl rather than reqwest to keep xtask's
/// dep graph small — this is a one-shot operator tool, not a hot path.
fn fetch_provider_models(probe: &LiveModelProbe, api_key: &str) -> Result<Vec<String>> {
  let mut cmd = Command::new("curl");
  cmd.arg("--silent").arg("--show-error").arg("--fail");
  let url = match probe.endpoint {
    LiveModelsEndpoint::OpenAICompat(url) => {
      cmd
        .arg("-H")
        .arg(format!("Authorization: Bearer {api_key}"));
      url.to_string()
    }
    LiveModelsEndpoint::Anthropic(url) => {
      cmd.arg("-H").arg(format!("x-api-key: {api_key}"));
      cmd.arg("-H").arg("anthropic-version: 2023-06-01");
      url.to_string()
    }
    LiveModelsEndpoint::Google(url) => format!("{url}?key={api_key}"),
  };
  cmd.arg(&url);

  let output = cmd
    .output()
    .with_context(|| format!("spawning curl for {}", probe.name))?;
  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("curl exited {} (stderr: {})", output.status, stderr.trim());
  }
  parse_models_response(probe, &output.stdout)
}

/// Pure parser: turn a provider's `/models` JSON response into the
/// model-id list. Exposed for unit testing.
fn parse_models_response(probe: &LiveModelProbe, body: &[u8]) -> Result<Vec<String>> {
  let value: serde_json::Value = serde_json::from_slice(body).with_context(|| {
    format!(
      "parsing {} /models response (body starts with: {:?})",
      probe.name,
      std::str::from_utf8(&body[..body.len().min(120)]).unwrap_or("<non-utf8>")
    )
  })?;
  let raw_ids: Vec<String> = match probe.endpoint {
    LiveModelsEndpoint::OpenAICompat(_) | LiveModelsEndpoint::Anthropic(_) => value
      .get("data")
      .and_then(|d| d.as_array())
      .ok_or_else(|| anyhow::anyhow!("{}: missing `data` array", probe.name))?
      .iter()
      .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
      .collect(),
    LiveModelsEndpoint::Google(_) => value
      .get("models")
      .and_then(|d| d.as_array())
      .ok_or_else(|| anyhow::anyhow!("{}: missing `models` array", probe.name))?
      .iter()
      .filter_map(|m| m.get("name").and_then(|i| i.as_str()).map(String::from))
      .map(|name| {
        // Google's response prefixes with `models/`; strip it so the
        // ids align with the test file's hard-coded `gemini-2.5-flash`
        // style.
        name
          .strip_prefix("models/")
          .map(String::from)
          .unwrap_or(name)
      })
      .collect(),
  };
  let mut ids = raw_ids;
  ids.sort();
  ids.dedup();
  Ok(ids)
}

/// Return the first model id from `list` that looks like a dated
/// revision of `alias` — i.e. matches `<alias>-<digit>…`. Used to
/// suppress the rolling-alias false positive when a provider's
/// `/v1/models` only enumerates dated revisions (Anthropic does
/// this for some model families).
///
/// The "followed by a digit" guard prevents same-prefix unrelated
/// families from satisfying the check: `gpt-4o-mini-realtime`
/// shouldn't match `gpt-4o-mini`, but `gpt-4o-mini-2024-07-18`
/// should.
fn find_dated_revision<'a>(alias: &str, list: &'a [String]) -> Option<&'a str> {
  let prefix = format!("{alias}-");
  list.iter().find_map(|id| {
    id.strip_prefix(&prefix)
      .filter(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()))
      .map(|_| id.as_str())
  })
}

/// Number of leading characters two strings share, case-insensitive.
/// Used by the "suggest a replacement" heuristic.
fn shared_prefix_len(a: &str, b: &str) -> usize {
  a.chars()
    .zip(b.chars())
    .take_while(|(ca, cb)| ca.eq_ignore_ascii_case(cb))
    .count()
}

/// Minimal `.env` loader — parses `KEY=value` lines and writes them
/// to the process environment with `set_var`. Lines starting with
/// `#` are skipped; values may be wrapped in single or double quotes,
/// which are stripped. Returns the count of keys set.
///
/// **Policy: `.env` wins over the ambient shell.** A real-world run
/// (P10.3.4) surfaced an operator-stale `OPENAI_API_KEY` in the shell
/// silently blocking a valid value in `~/.yanshi/.env`. The fix is
/// to make `.env` authoritative when it exists. CI is unaffected
/// because no `.env` file ships in the runner's HOME; secrets reach
/// the process via the workflow's `env:` block exactly as before.
///
/// (Intentionally not via the `dotenv` crate — keeps xtask's dep
/// graph minimal. The parser is a faithful subset of the standard
/// `.env` grammar.)
fn load_dotenv_file(path: &Path) -> Result<usize> {
  let text =
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
  let mut count = 0usize;
  for line in text.lines() {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
      continue;
    }
    let Some(eq) = line.find('=') else { continue };
    let key = line[..eq].trim();
    let raw_value = line[eq + 1..].trim();
    let value = strip_dotenv_quotes(raw_value);
    if key.is_empty() {
      continue;
    }
    // SAFETY: xtask is a single-threaded CLI; no concurrent env
    // access while this loop runs.
    unsafe {
      std::env::set_var(key, value);
    }
    count += 1;
  }
  Ok(count)
}

fn strip_dotenv_quotes(value: &str) -> &str {
  if value.len() >= 2 {
    let bytes = value.as_bytes();
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
      return &value[1..value.len() - 1];
    }
  }
  value
}

#[cfg(test)]
mod refresh_live_models_tests {
  use super::*;

  fn openai_probe() -> LiveModelProbe {
    LiveModelProbe {
      name: "openai",
      key_envs: &["OPENAI_API_KEY"],
      default_text_model: "gpt-4o-mini",
      endpoint: LiveModelsEndpoint::OpenAICompat("https://api.openai.com/v1/models"),
    }
  }

  fn google_probe() -> LiveModelProbe {
    LiveModelProbe {
      name: "google",
      key_envs: &["GEMINI_API_KEY"],
      default_text_model: "gemini-2.5-flash",
      endpoint: LiveModelsEndpoint::Google(
        "https://generativelanguage.googleapis.com/v1beta/models",
      ),
    }
  }

  #[test]
  fn parse_openai_compat_extracts_id_array() {
    let body = br#"{"object":"list","data":[
      {"id":"gpt-4o-mini","object":"model"},
      {"id":"gpt-4o","object":"model"}
    ]}"#;
    let ids = parse_models_response(&openai_probe(), body).expect("parse ok");
    assert_eq!(ids, vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]);
  }

  #[test]
  fn parse_anthropic_uses_data_array_too() {
    let probe = LiveModelProbe {
      name: "anthropic",
      key_envs: &["ANTHROPIC_API_KEY"],
      default_text_model: "claude-haiku-4-5",
      endpoint: LiveModelsEndpoint::Anthropic("https://api.anthropic.com/v1/models"),
    };
    let body = br#"{"data":[
      {"id":"claude-haiku-4-5","display_name":"Haiku 4.5","type":"model"},
      {"id":"claude-sonnet-4-5","display_name":"Sonnet 4.5","type":"model"}
    ]}"#;
    let ids = parse_models_response(&probe, body).expect("parse ok");
    assert!(ids.contains(&"claude-haiku-4-5".to_string()));
  }

  #[test]
  fn parse_google_strips_models_prefix() {
    // Google's `/v1beta/models` returns names like `models/gemini-2.5-flash`;
    // strip the prefix so the ids align with the test file's `gemini-2.5-flash`
    // style. Critical: without stripping, every Google default would always
    // report missing.
    let body = br#"{"models":[
      {"name":"models/gemini-2.5-flash"},
      {"name":"models/gemini-2.5-pro"}
    ]}"#;
    let ids = parse_models_response(&google_probe(), body).expect("parse ok");
    assert_eq!(
      ids,
      vec!["gemini-2.5-flash".to_string(), "gemini-2.5-pro".to_string()]
    );
  }

  #[test]
  fn parse_handles_missing_id_field_by_skipping_entry() {
    // Defensive: vendors occasionally ship a `data: []` row with no `id`
    // (sometimes pre-release placeholders). Skip them quietly rather
    // than fail the whole parse.
    let body = br#"{"data":[
      {"id":"gpt-4o-mini"},
      {"object":"model"},
      {"id":"gpt-4o"}
    ]}"#;
    let ids = parse_models_response(&openai_probe(), body).expect("parse ok");
    assert_eq!(ids, vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]);
  }

  #[test]
  fn parse_returns_clear_error_on_missing_data_array() {
    let body = br#"{"error":{"message":"unauthorized"}}"#;
    let err = parse_models_response(&openai_probe(), body).unwrap_err();
    assert!(
      err.to_string().contains("missing `data` array"),
      "expected clear shape error; got: {err}"
    );
  }

  #[test]
  fn find_dated_revision_matches_anthropic_haiku_alias() {
    // Anthropic's `/v1/models` only lists dated revisions for some
    // model families; the alias still resolves in real API calls. The
    // tool should not flag the alias as missing when at least one
    // matching dated revision is present.
    let list = vec![
      "claude-haiku-4-5-20251001".to_string(),
      "claude-opus-4-1-20250805".to_string(),
    ];
    assert_eq!(
      find_dated_revision("claude-haiku-4-5", &list),
      Some("claude-haiku-4-5-20251001")
    );
  }

  #[test]
  fn find_dated_revision_skips_same_prefix_but_different_family() {
    // `gpt-4o-mini-realtime-preview` shouldn't satisfy `gpt-4o-mini`
    // — the suffix-must-start-with-digit guard guards this.
    let list = vec![
      "gpt-4o".to_string(),
      "gpt-4o-mini-realtime-preview".to_string(),
    ];
    assert_eq!(find_dated_revision("gpt-4o-mini", &list), None);
  }

  #[test]
  fn find_dated_revision_returns_none_when_no_match() {
    let list = vec!["unrelated".to_string(), "other".to_string()];
    assert!(find_dated_revision("claude-haiku-4-5", &list).is_none());
  }

  #[test]
  fn find_dated_revision_matches_openai_style_yyyy_mm_dd_suffix() {
    // The rule generalises beyond Anthropic — OpenAI dated revisions
    // are `<alias>-<YYYY>-<MM>-<DD>`, which also start with a digit
    // after the alias prefix. (In practice the strict-equality path
    // in `probe_provider` catches `gpt-4o-mini` before this function
    // runs, but we confirm the helper itself behaves correctly.)
    let list = vec!["gpt-4o-mini-2024-07-18".to_string()];
    assert_eq!(
      find_dated_revision("gpt-4o-mini", &list),
      Some("gpt-4o-mini-2024-07-18")
    );
  }

  #[test]
  fn shared_prefix_len_is_case_insensitive() {
    assert_eq!(shared_prefix_len("gpt-4o-mini", "gpt-4O-mini"), 11);
    assert_eq!(
      shared_prefix_len("claude-haiku-4-5", "claude-sonnet-4-5"),
      7
    );
    assert_eq!(shared_prefix_len("", "anything"), 0);
  }

  #[test]
  fn strip_dotenv_quotes_handles_both_quote_kinds() {
    assert_eq!(strip_dotenv_quotes("\"abc\""), "abc");
    assert_eq!(strip_dotenv_quotes("'abc'"), "abc");
    assert_eq!(strip_dotenv_quotes("abc"), "abc");
    // Mixed quotes don't strip (would be ambiguous).
    assert_eq!(strip_dotenv_quotes("\"abc'"), "\"abc'");
    // Too short to be a quoted pair.
    assert_eq!(strip_dotenv_quotes("\""), "\"");
    assert_eq!(strip_dotenv_quotes(""), "");
  }

  #[test]
  fn refresh_rejects_unknown_include_filter() {
    let workspace_root = crate::workspace_root();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let res = refresh_live_models_from_args(
      &workspace_root,
      vec!["--include".to_string(), "no-such-provider".to_string()],
      &mut out,
      &mut err,
    );
    assert!(res.is_err(), "unknown provider must fail fast");
    let msg = res.unwrap_err().to_string();
    assert!(msg.contains("no providers selected"), "got: {msg}");
  }

  #[test]
  fn probe_skips_providers_without_api_key() {
    // SAFETY: this test mutates the process environment. The env var
    // name is unique to this test so concurrent tests don't collide.
    const KEY: &str = "OPENAI_API_KEY_REFRESH_PROBE_TEST_SKIP";
    unsafe {
      std::env::remove_var(KEY);
    }
    let probe = LiveModelProbe {
      name: "openai-skip-test",
      key_envs: &[KEY],
      default_text_model: "gpt-4o-mini",
      endpoint: LiveModelsEndpoint::OpenAICompat("https://example.invalid/models"),
    };
    let outcome = probe_provider(&probe);
    match outcome.status {
      ProbeStatus::Skipped { reason } => {
        assert!(
          reason.contains(KEY),
          "diagnostic must name the env var: {reason}"
        );
      }
      other => panic!("expected Skipped, got {other:?}"),
    }
  }
}
