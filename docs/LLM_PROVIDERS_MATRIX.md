# LLM Providers Support Matrix

> Status: foundation shipped in v0.4.0 (P1 #11). Streaming, multimodal, and
> live-LLM nightly CI all closed (see Closed follow-ups).
> Crates: `yanshi-llm`. Test entries:
> `yanshi-llm/tests/provider_consistency.rs` (offline, mocked) and
> `yanshi-llm/tests/provider_consistency_live.rs` (opt-in, real APIs).

Yanshi's LLM abstraction targets seven providers/profiles. This document is the
authoritative reference for what works on each, what doesn't, and how the
behavior is verified.

## ProviderRequest contract

Every `LLMProvider` impl receives a `ProviderRequest` and returns a
`ProviderResponse`. The wire fields are intentionally narrow so adapter
behavior can stay consistent. **The field names below are exercised by
`yanshi-llm/tests/provider_matrix_doc.rs` — adding or renaming a
field on `ProviderRequest` fails CI until this section is updated.**

| Field | Type | Required | Description |
| --- | --- | :-: | --- |
| `model` | `String` | ✅ | Provider-resolved model identifier (e.g. `gpt-4o-mini`, `claude-sonnet-5`). Adapters translate to the wire shape each provider expects. |
| `messages` | `Vec<Value>` | ✅ | OpenAI-style message array. Multimodal content is encoded as `image_url` blocks (and, Anthropic-only, `document_url`; Google-only, `video_url`); adapters translate to provider-native shapes (Anthropic `image`/`document`, Google `inline_data`/`file_data`). |
| `stream` | `bool` | ✅ | When `true`, the provider returns a chunked / SSE response. `ModelCapabilities::requires_streaming` rejects `stream = false` for streaming-only models. |
| `parameters` | `HashMap<String, Value>` | ✅ | Free-form provider passthrough (temperature, top_p, max_tokens, custom flags). Adapters whitelist and rename as needed; unknown keys are ignored. |
| `tools` | `Option<Vec<ToolSpec>>` | – | Native tool / function-calling specification. `None` skips tool wiring entirely. |
| `tool_choice` | `Option<ToolChoice>` | – | Selection strategy used together with `tools`. See the [`ToolChoice` table](#toolchoice-modes). |
| `thinking` | `Option<ThinkingConfig>` | – | Extended-reasoning ("thinking") configuration. Travels as a typed field so Anthropic/Google whitelists don't drop it. Adapters map to native shapes: Anthropic `thinking: { type: "adaptive" }` + `output_config.effort` (Haiku 4.5 and older: `thinking: { budget_tokens }`; chosen by model name), OpenAI `reasoning_effort`, Google `generationConfig.thinkingConfig.thinkingBudget`, DeepSeek V4 (`deepseek-v4-*`, model-name-dispatched inside `OpenAIProvider`) a *nested* `thinking: { type, reasoning_effort }` block that — unlike OpenAI — defaults to enabled, so `Disabled` actively sends `type: "disabled"` rather than omitting the field. `None` disables. |
| `response_format` | `Option<ResponseFormat>` | – | Structured-output constraint (V2.1). Travels as a typed field for the same reason `thinking` does — Anthropic/Google whitelists would otherwise silently drop it. OpenAI/Moonshot/StepFun map it to the native `response_format` field unconditionally (works alongside `tools`). Anthropic has no native `response_format`; when no `tools` are requested and the call isn't streaming, `JsonSchema` is emulated by forcing a single synthetic tool call whose `input_schema` is the requested schema and unwrapping its arguments back into plain-text content. Google maps it to `generationConfig.responseMimeType` + `.responseSchema`, also only when no `tools` are requested. When real `tools` are present, Anthropic/Google both skip native wiring and fall back to prompt-only constraint (forcing a tool choice, or an unverified schema+tools combination, would break the caller's own tool-calling turn). `None` leaves output format unconstrained. |

## ToolChoice modes

The `ToolChoice` enum is serialised in snake_case so the wire shape
matches OpenAI's `tool_choice` field. Anthropic / Google adapters
translate to their respective vocabularies.

| Mode | Provider behavior |
| --- | --- |
| `auto` (default) | Model decides whether to call a tool. Adapters omit `tool_choice` from the outbound request when this is the default. |
| `none` | Model MUST NOT call a tool; reply text-only. |
| `required` | Model MUST call at least one tool. Providers that lack a literal "required" mode (some Google revisions) raise `LLMError::ProviderUnsupportedMode` at request time. |
| `tool` (with `{ name }`) | Model MUST call exactly the named tool. Useful for deterministic agent steps and benchmarks. |

## ModelCapabilities flags

`ModelCapabilities` (in `yanshi-llm::model_types`) is the per-model
description loaded from the YAML registry. The flags below drive
provider-side validation and ReAct fallback behavior:

| Flag | Type | Purpose |
| --- | --- | --- |
| `model_type` | `ModelType` | Stable model-type classification (`text`, `image_generate`, `image_understand`, `audio_tts`, `audio_asr`, `video_generate`, `video_understand`, `doc_understand`, `code_gen`, `function_calling`, `embedding`). Determines admissible inputs / outputs. |
| `supports_streaming` | `bool` | Model exposes a streaming variant. |
| `requires_streaming` | `bool` | Model has no non-streaming mode; callers MUST set `ProviderRequest::stream = true`. |
| `supports_tools` | `bool` | Tool calling is supported on any path (native or prompt-based). |
| `native_tool_calling` | `bool` | Provider-native tool calling (OpenAI `tool_calls`, Anthropic `tool_use`, Google `functionCall`). When `false`, ReAct falls back to prompt-based protocols. |
| `max_context_tokens` | `Option<u32>` | Hard ceiling on the model's input context window. Used by prompt assembly. |
| `max_output_tokens` | `Option<u32>` | Hard ceiling on a single response. |
| `supports_system_messages` | `bool` | When `false`, the adapter folds system content into the first user message. |
| `custom_capabilities` | `HashMap<String, Value>` | Provider-specific opt-ins (vision detail mode, JSON-mode strictness, etc). Stable surface but opaque to the registry. |
| `outputs` | `HashSet<OutputType>` | Output modalities this model can produce (P-LLM2.6). Populated from `ModelConfig::outputs` (explicit YAML `outputs: [...]`) when set, else seeded to `{model_type.primary_output()}`. **Metadata only** — no provider response parser constructs `ContentType::Image`/`Audio`/`Mixed` for a chat response yet, so this records that e.g. `gemini-2.0-flash-preview-image-generation` *can* return inline image parts in a chat turn without this crate being able to surface that data today. |

## Configuration source

Provider definitions and model aliases are loaded with the shared Yanshi
configuration resolver:

1. `YANSHI_MODELS_CONFIG`
2. `~/.yanshi/models.yml`
3. `~/.yanshi/models.yaml`
4. bundled `default_models.yml`

`~/.yanshi/.env` remains the default local API-key file. CLI diagnostics
show the selected config path/source and redact credential values.

## Capability matrix

| Capability | OpenAI | Anthropic | Google | Moonshot | StepFun | GLM | Mock |
| --- | :-: | :-: | :-: | :-: | :-: | :-: | :-: |
| Text completion | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Token usage in response | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | n/a |
| Streaming | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Native tool calling (`tool_calls` / `tool_use` / `functionCall`) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ (injection) |
| Multimodal text + image (URL or base64) | ✅ | ✅ | ✅ | partial | ✅ | ✅ | n/a |
| Audio TTS / ASR | – | – | – | – | ✅ | – | n/a |
| Image generation | – | – | – | – | – | – | n/a |
| W3C `traceparent` injection | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | n/a |
| `with_client(...)` for custom `reqwest::Client` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | n/a |

Key:

- ✅ — supported, verified in unit + integration tests.
- partial — works for text but not all multimodal corner cases.
- – — not implemented. Most LLM providers don't ship the modality.
- n/a — Mock provider doesn't make HTTP calls; capability is irrelevant.

### StepFun live-test status

Status vocabulary: `supported`, `live_tested`, `mock_only`, `unsupported`,
`flaky`.

| StepFun capability | Status | Verification |
| --- | --- | --- |
| Text generation | `live_tested` | `stepfun_live_text_path` |
| Streaming | `live_tested` | `stepfun_live_streaming_path` |
| Native tool calling / compatible fallback | `live_tested` | `stepfun_live_tool_calling_or_fallback_path`; adapter normalizes non-empty `tool_calls` to `StopReason::ToolCalls` even when StepFun returns `finish_reason: "stop"` |
| Vision understanding | `live_tested` | `stepfun_live_vision_path` with a tiny base64 PNG |
| Image generation | `unsupported` | StepFun retired `step-2x-large` and `step-image-edit-2` and stopped its image APIs on 2026-10-10; the registry entries and the live test are gone |
| TTS | `live_tested` | `stepfun_live_tts_path` via `/audio/speech` |
| ASR | `live_tested` | `stepfun_live_asr_path`; generates a tiny TTS fixture, then transcribes it |
| Video generation | `unsupported` | No StepFun video API is implemented in `yanshi-llm` |

StepFun live tests use deterministic model selection. Environment overrides
still win (`YANSHI_LIVE_STEPFUN_TEXT_MODEL`,
`YANSHI_LIVE_STEPFUN_TOOLS_MODEL`, `YANSHI_LIVE_STEPFUN_VISION_MODEL`,
`YANSHI_LIVE_STEPFUN_TTS_MODEL`,
`YANSHI_LIVE_STEPFUN_ASR_MODEL`); otherwise the harness picks from the
loaded Yanshi model config using a low-cost preference order before falling
back to built-in defaults.

### GLM live-test status

BigModel GLM is wired as an OpenAI-compatible profile over the official
`https://open.bigmodel.cn/api/paas/v4` endpoint documented in
[BigModel 使用概述](https://docs.bigmodel.cn/cn/api/introduction). Chat
completion uses `/chat/completions`, which BigModel documents with Bearer auth,
SSE streaming, function tools, and multimodal input in
[对话补全](https://docs.bigmodel.cn/api-reference/%E6%A8%A1%E5%9E%8B-api/%E5%AF%B9%E8%AF%9D%E8%A1%A5%E5%85%A8).

Status vocabulary: `supported`, `live_tested`, `mock_only`, `unsupported`,
`flaky`.

| GLM capability | Status | Verification |
| --- | --- | --- |
| Text generation | `live_tested` | `glm_live_text_path` via `OpenAIProvider` |
| Streaming | `live_tested` | `glm_live_streaming_path`; GLM live tests are serialized to avoid account-level 429s |
| OpenAI-compatible chat path | `live_tested` | `glm_live_openai_compatible_chat_path` |
| Native tool calling / compatible fallback | `live_tested` | `glm_live_tool_calling_or_fallback_path`; OpenAI-compatible adapter normalizes non-empty `tool_calls` to `StopReason::ToolCalls` |
| Vision understanding | `live_tested` | `glm_live_vision_path` using `glm-4.5v` and an HTTPS JPEG image URL |
| Image generation | `unsupported` | BigModel exposes `/images/generations`, but Yanshi has no GLM image-generation client/profile yet |
| ASR | `unsupported` | BigModel exposes `/audio/transcriptions`, but Yanshi has no GLM ASR client/profile yet |
| TTS | `unsupported` | BigModel exposes `/audio/speech`, but Yanshi has no GLM TTS client/profile yet |
| Video generation | `unsupported` | BigModel exposes async `/videos/generations`, but Yanshi has no GLM video client/profile yet |

Environment overrides win for model selection:
`YANSHI_LIVE_GLM_TEXT_MODEL`, `YANSHI_LIVE_GLM_TOOLS_MODEL`, and
`YANSHI_LIVE_GLM_VISION_MODEL`. Without overrides, the harness first checks
the loaded Yanshi model config and then falls back to low-cost defaults:
`glm-4.5-flash` for text/tools and `glm-4.5v` for vision.

## Model families & context windows

The numbers below are the **public** context windows documented by
each vendor; the runtime ceiling is whatever the model's YAML registry
entry sets in `max_context_tokens`. When the registry value is `None`,
prompt assembly assumes a conservative 4 K token cap so adapters
never silently truncate.

| Provider | Model family | Public context | Verification |
| --- | --- | --- | --- |
| OpenAI | `gpt-4o`, `gpt-4o-mini`, `gpt-4-turbo`, `o1` | 128K–200K | `tested` (live + offline) |
| OpenAI | `gpt-3.5-turbo` | 16K | `tested` (offline only) |
| Anthropic | `claude-haiku-4-5` | 200K | `tested` (live + offline) |
| Anthropic | `claude-fable-5-1`, `claude-fable-5`, `claude-opus-5-5`, `claude-opus-5`, `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-5-5`, `claude-sonnet-5`, `claude-sonnet-4-6`, `claude-haiku-5-5` | 1M | `best_effort` (offline only) |
| Google | `gemini-1.5-flash`, `gemini-1.5-pro` | 1M | `tested` (offline; live nightly) |
| Google | `gemini-2.0-flash`, `gemini-2.0-flash-exp` | 1M | `best_effort` (offline only) |
| Moonshot | `kimi-k2.6` (`moonshot-v1-*` retired 2026-08-31) | — | `tested` (live + offline) |
| StepFun | `step-3.5-flash`, `step-3.5-flash-2603`, `step-3.7-flash` | 256K | `tested` (live + offline) |
| StepFun | `step-5-preview` | 1M | `best_effort` (offline only) |
| GLM | `glm-4.5`, `glm-4.5-flash`, `glm-4.5v` | 128K | `tested` (live + offline, vision opt-in) |
| Mock | `mock-runtime-*`, `mock-*` | configurable per test | n/a |

Status vocabulary:

- `tested` — verified in unit + integration tests (and / or nightly live CI).
- `best_effort` — provider supports the model and adapters wire it,
  but no Yanshi test asserts the exact wire shape.
- `n/a` — Mock provider; not a real backend.

## Rate-limit handling

Two independent mechanisms, corrected here as of P-LLM2.7 (this section
previously described neither accurately — it claimed no auto-retry exists
at all, missing the V1.5 `retry_transient` mechanism entirely, and claimed
`Retry-After` was preserved when the header was never read anywhere):

1. **Proactive per-vendor rate limiting** (`ModelRegistry`, P-LLM2.7).
   Each vendor's `ProviderConfig.rate_limit.requests_per_minute` (set for
   all 9 vendors in `templates/default_models.yml`) now builds one
   `governor` token bucket, keyed by vendor name. `LLMClient::execute`/
   `execute_full`/`execute_streaming` call `.until_ready().await` on it
   immediately before dispatching, so a caller that would exceed the
   configured RPM waits rather than firing the request and eating a 429.
   `tokens_per_minute` is parsed but **not** enforced — TPM throttling
   needs a pre-call token estimate, which is only solid for the
   OpenAI-BPE-family vendors (`tiktoken-rs`, see `tokenizer.rs`); left as
   documented future work. Before P-LLM2.7 this whole field was
   decorative — parsed, validated (`requests_per_minute != 0` only), then
   discarded at `create_provider(vendor, api_key, base_url)`, which never
   even accepted a rate-limit parameter.
2. **Reactive retry on a 429/5xx that happens anyway** (`retry_transient`,
   V1.5 + P-LLM2.7 jitter/`Retry-After`). Driven by the registry's
   `defaults.max_retries` / `defaults.retry_delay_ms` (`0`/unset
   `max_retries` disables retry entirely — the original pre-V1.5
   behavior). The delay between attempts is chosen in this priority
   order:
   - a server-supplied `Retry-After` header, when the response had one
     and it parsed (`LLMError::RateLimitedWithRetryAfter`, built by
     `providers::chat_http_error` — only the chat `execute`/
     `execute_streaming` methods that route through `retry_transient`
     construct this; one-shot modality endpoints like TTS/ASR/image/video
     never retry, so they keep using plain `HttpError` and don't parse
     the header at all);
   - otherwise, `base_delay_ms * 2^attempt`, equal-jittered (half fixed,
     half randomized) so concurrent callers retrying the same transient
     failure don't all wake up and re-hit the provider at the same
     instant — pure exponential backoff with zero jitter did nothing to
     prevent that before this fix.

Only the delta-seconds form of `Retry-After` (RFC 7231 §7.1.3, e.g.
`Retry-After: 30`) is parsed; the rarely-used HTTP-date form falls back
to the computed exponential+jitter delay, same as when the header is
absent entirely.

## Error mapping (verified contract)

All concrete HTTP-based providers and OpenAI-compatible profiles map non-2xx responses to a single
`LLMError` variant — this is the consistency contract that downstream code
(retry middleware, error reporting) depends on:

```rust
match err {
  LLMError::HttpError { status_code, message } => {
    // status_code is the actual HTTP status from the provider
    // (401, 429, 500, 503, ...) — NOT remapped to a higher-level variant.
  }
  _ => unreachable!(),
}
```

The `From<reqwest::Error>` impl in `yanshi-llm::error` does map 401 / 429 /
503 to `AuthenticationError` / `RateLimitExceeded` / `ServiceUnavailable`, but
this only fires when the **transport itself** signals a status (e.g.
`response.error_for_status()?`). All providers inspect `response.status()`
manually and emit `HttpError`, which is what the consistency test asserts.

If a future provider needs richer error classification, the recommended path
is a new `LLMError::ProviderError { provider, kind: ProviderErrorKind, … }`
variant rather than reinterpreting status codes inconsistently across
providers.

## Verification

### Unit tests (per-provider, JSON fixtures)

Each provider has its own unit test suite that pins down request body
construction and response parsing against representative JSON fixtures:

```bash
cargo test -p yanshi-llm --lib providers::openai     # 10 tests
cargo test -p yanshi-llm --lib providers::anthropic  # 7 tests
cargo test -p yanshi-llm --lib providers::google     # 8 tests
cargo test -p yanshi-llm --lib providers::moonshot   # 5 tests
cargo test -p yanshi-llm --lib providers::stepfun    # 8 tests
```

These tests don't make network calls — they construct `ProviderRequest` /
`ProviderResponse` instances directly and assert wire-format invariants.

### Cross-provider integration (`provider_consistency.rs`)

The integration suite drives the concrete HTTP providers through a hand-rolled
tokio TCP listener and asserts a uniform contract:

```bash
cargo test -p yanshi-llm --test provider_consistency  # 15 tests
```

Coverage:

- **Success path** (5 tests): each provider, given a well-formed response
  in its native shape, returns `ContentType::Text`, `StopReason::Stop`,
  and populated `TokenUsage` (prompt / completion / total).
- **Tool-calling success path** (5 tests): each provider, given a response
  carrying a model-issued `get_weather(city="Tokyo")` tool call in its
  native shape (OpenAI `tool_calls`, Anthropic `tool_use` content blocks,
  Google `functionCall` parts, Moonshot/StepFun OpenAI-compatible
  passthrough), parses to a single `ToolCallRequest { name, arguments,
  id }` with non-empty id (synthesised when the provider doesn't supply
  one) and reports `StopReason::ToolCalls`. Google's adapter is
  responsible for normalising `finishReason: STOP` to `ToolCalls` when
  functionCall parts are present.
- **Error mapping** (5 tests): each provider, given a 4xx / 5xx response,
  returns `LLMError::HttpError` with the exact status code preserved.

The mock server is hand-rolled (not `mockito` / `wiremock`) for the same
reason as `trace_context_propagation.rs`: it lets us inspect raw request
bytes and stays independent of HTTP-mock crate version churn. The reqwest
client is built with `.no_proxy()` to avoid being routed through a system
HTTP proxy on dev machines (a common source of `IncompleteMessage` / hang
in localhost tests).

### Trace context propagation (`trace_context_propagation.rs`)

Verifies that an active `LlmTraceContext` injects a W3C `traceparent` header
into outbound HTTP calls, and that no header is injected when no scope is
active. See [`docs/TRACING_USAGE.md`](TRACING_USAGE.md) for usage.

## Live-API tests (opt-in)

By default, every provider test in this crate is offline / mocked. Live API
calls are deliberately not part of the default `cargo test` run — they require
real keys, cost money, and are non-deterministic (provider behavior shifts
between model versions).

Live tests are gated by:

```bash
YANSHI_LIVE_LLM_TESTS=1 \
OPENAI_API_KEY=sk-… \
ANTHROPIC_API_KEY=sk-ant-… \
GEMINI_API_KEY=… \
MOONSHOT_API_KEY=… \
STEPFUN_API_KEY=… \
GLM_API_KEY=… \
cargo test -p yanshi-llm --test provider_consistency_live
```

Additional live-test gates are reserved for modality-specific suites:

```bash
YANSHI_LIVE_MULTIMODAL_TESTS=1
YANSHI_LIVE_IMAGE_TESTS=1
YANSHI_LIVE_AUDIO_TESTS=1
YANSHI_LIVE_VIDEO_TESTS=1
```

`YANSHI_LIVE_IMAGE_TESTS` gates the billed image jobs: text-to-image
through the modality dispatcher on OpenAI, Google, DashScope and GLM,
image-to-image on OpenAI and DashScope Wan (both from Alibaba's sample
image),
and a Gemini chat reply that carries an image. The `llm-live.yml`
manual run has an `image` checkbox for them.

Text-to-video (Y6.6) has one billed job per vendor under
`YANSHI_LIVE_VIDEO_TESTS`: `dashscope_wan_text_to_video_live` (`wan2.7-t2v`,
2 s at 720P), `minimax_hailuo_text_to_video_live` (`MiniMax-Hailuo-2.3`,
6 s at 768P; set `MINIMAX_VIDEO_BASE_URL=https://api.minimax.io/v1` for an
international account) and `kling_text_to_video_live` (`kling-4.0-flash`,
3 s at 720p; `KLING_API_KEY` is an API key or `ACCESS_KEY:SECRET_KEY`; the
default host is `api-beijing.klingai.com`, set
`KLING_VIDEO_BASE_URL=https://api-singapore.klingai.com` for an
international account). Nightly CI
never sets the gate; the `llm-live.yml` manual run has a `video` checkbox.
Offline, `tests/video_providers.rs` drives each vendor's submit → poll
(→ MiniMax file retrieve) flow against a local server speaking its
documented wire shape.

| Vendor | Text-to-video | Endpoint shape |
|---|---|---|
| Google | Veo | `predictLongRunning` + operation poll |
| DashScope | Wan (`wan2.7-t2v`, `wan2.5-t2v-preview`) | `video-synthesis` with `X-DashScope-Async`, `GET /api/v1/tasks/{id}` → `output.video_url` |
| MiniMax | Hailuo (`MiniMax-Hailuo-2.3`, `-02`) | `POST /video_generation`, `GET /query/video_generation`, `GET /files/retrieve` → `file.download_url` |
| Kling | `kling-4.0`, `kling-4.0-flash` | `POST /text-to-video/{model}`, `GET /tasks?task_ids=` → `data[].outputs[type=video].url` |
| Kling (legacy) | `kling-v2-6`, `kling-v2-5-turbo`, `kling-v2-1-master` | `POST /v1/videos/text2video`, `GET …/{task_id}` → `data.task_result.videos[].url` |

Image-to-image (Y6.6) adds OpenAI (`gpt-image-1-mini` through
`/images/edits` with the source image and no mask) and DashScope
(`wan2.5-i2i-preview`, async `image2image/image-synthesis`; the source
goes as a URL or a data URI). Their live cases are
`openai_image_to_image_live` and `dashscope_wan_image_to_image_live`
under `YANSHI_LIVE_IMAGE_TESTS`; `tests/image2image_providers.rs` covers
both offline. A registry name has one `type`, which is why the OpenAI
entry is a separate model from `gpt-image-1` (`image_edit`).

Images in chat replies (Y6.6, evaluation M7): Gemini `inlineData` parts
become `ContentType::Image` / `Mixed` (in order, base64 decoded, not
copied into the response metadata). `LLMResponse` keeps `content`
text-only and adds `media` (the non-text blocks); the `llm` node writes
reply images as artifacts under an `images` output. Streaming replies
still carry text only. Offline: `tests/chat_reply_images.rs`; live:
`google_chat_reply_carries_an_image_live` (`YANSHI_LIVE_IMAGE_TESTS`).

StepFun modality smoke tests are enabled by those gates:

```bash
YANSHI_LIVE_LLM_TESTS=1 \
YANSHI_LIVE_MULTIMODAL_TESTS=1 \
YANSHI_LIVE_IMAGE_TESTS=1 \
YANSHI_LIVE_AUDIO_TESTS=1 \
cargo test -p yanshi-llm --test provider_consistency_live stepfun_live
```

GLM OpenAI-compatible smoke tests are enabled with:

```bash
YANSHI_LIVE_LLM_TESTS=1 \
cargo test -p yanshi-llm --test provider_consistency_live glm_live

YANSHI_LIVE_MULTIMODAL_TESTS=1 \
cargo test -p yanshi-llm --test provider_consistency_live glm_live_vision_path
```

**Status**: live-test harness landed 2026-05-08. The default `cargo test`
run is unaffected — without `YANSHI_LIVE_LLM_TESTS` set, every test in
`provider_consistency_live.rs` short-circuits before issuing a request and
reports `ok` in milliseconds. With the gate set but a provider's API key
env var missing, that single provider self-skips with a log line; the rest
still run.

Behavior of the harness:

1. Skips cleanly when `YANSHI_LIVE_LLM_TESTS` is unset (test passes,
   prints `[live] <provider>: skipped`).
2. Calls `Yanshi::init()` after the live gate is enabled, so
   `~/.yanshi/.env`, `YANSHI_MODELS_CONFIG`, `~/.yanshi/models.yml`,
   and legacy `~/.yanshi/models.yaml` are loaded through the same resolver
   used by CLI and server code.
3. Uses minimum-cost defaults per provider (`gpt-4o-mini`,
   `claude-haiku-4-5`, `gemini-1.5-flash`, `kimi-k2.6`,
   `step-1-8k`); each is overridable via
   `YANSHI_LIVE_<PROVIDER>_TEXT_MODEL`. The older
   `YANSHI_LIVE_<PROVIDER>_MODEL` form remains accepted for compatibility.
   Examples: `YANSHI_LIVE_STEPFUN_TEXT_MODEL`,
   `YANSHI_LIVE_GLM_TEXT_MODEL`.
4. One single-turn text request per provider with `max_tokens = 16` and
   `temperature = 0.0` for deterministic-as-possible cost.
5. Runs nightly via `.github/workflows/llm-live.yml` (cron `30 9 * * *` UTC,
   plus `workflow_dispatch` with an optional `providers` filter); not part
   of the PR-blocking `release-gate` aggregate in `quality.yml`.

The harness logs provider names, selected env-var names, selected model names,
and skip reasons, but never prints API key values. Keep live runs small:
prefer one short prompt, low `max_tokens`, and explicit provider model
overrides. Do not put live tests in ordinary PR CI; use local manual runs or
nightly CI with budget/rate-limit controls.

## Adding a new provider

1. Implement `LLMProvider` in `yanshi-llm/src/providers/<name>.rs`.
2. Provide both `new(...)` and `with_client(...)` constructors. The
   `with_client` constructor is mandatory for the consistency suite to be
   able to wire in a no-proxy test client.
3. In `build_headers` / `build_auth_headers`, call
   `crate::trace_context::inject_into_headers(&mut headers)` last so
   `traceparent` propagation works automatically.
4. Map non-success HTTP responses to `LLMError::HttpError { status_code,
   message }`. Don't promote to `AuthenticationError` /
   `RateLimitExceeded` directly — that's a downstream consumer concern.
5. Add per-provider unit tests for request body shape + response parsing.
6. Add a row to `provider_consistency.rs` covering the success path and
   one error code.
7. Update this document with the capability matrix entries.

## Follow-ups (non-blocking)

- **Quota / cost dashboards**: currently each provider exposes raw
  `TokenUsage` on responses; a higher-level cost roll-up keyed by model is
  not implemented.
## Closed follow-ups

- **Streaming consistency tests** — landed 2026-05-08. Cross-provider
  streaming wire formats covered in `provider_consistency.rs` via a
  chunked-encoding mock server.
- **Streaming tool-call deltas (P-LLM2.5)** — landed 2026-08-13. Google /
  Moonshot / StepFun's streaming parsers previously hardcoded
  `tool_call_deltas: Vec::new()` — a tool call requested mid-stream was
  silently dropped with no error, directly breaking the ReAct agent loop
  (which streams unconditionally). Moonshot/StepFun speak the identical
  OpenAI-compatible `delta.tool_calls[]` shape and now share a
  `pub(crate) openai_streaming_tool_call_deltas` helper extracted from
  `openai.rs`; both also had the same "drop the whole chunk when
  `delta.content` is absent" bug OpenAI's `parse_sse_chunk` already fixed
  (Q2.5.2) but they hadn't inherited, so a tool-call-only delta (no text)
  was dropped even before reaching the empty-deltas problem. Google's
  `functionCall` parts were structurally invisible to the parser (it only
  ever inspected `parts.first()` and only handled a `text` field) — fixed
  by iterating every part and, since Gemini never fragments a function
  call's arguments across chunks (the complete `args` object always
  arrives in one shot), emitting one `ToolCallDelta` per part with a
  stream-persistent synthesised `call_<index>` id (a per-chunk-local index
  would risk colliding two distinct calls onto the same index if Gemini
  ever split them across chunks). `provider_consistency.rs` gained
  `{google,moonshot,stepfun}_streaming_tool_call_path`, each driving a
  real `execute_streaming()` call through `collect_streaming_response`
  (the same reassembly path a real caller uses) over a mocked server and
  asserting the tool call survives intact.
- **Multimodal consistency tests** — landed 2026-05-08; corrected under
  P-LLM2.4 (2026-08-13). Each provider receives the *same* OpenAI-shaped
  `image_url` request — the one shape `MultimodalMessage::
  to_openai_format()` ever produces — and its adapter translates that into
  its own native format (OpenAI/Moonshot/StepFun: `image_url` passed
  through as-is; Anthropic: native `image` content block with a base64 or
  URL `source`; Google: rewritten into `inline_data`/`file_data`). Tests
  assert the captured request body preserves the base64 payload and that
  the response parses to the same `(text, Stop, populated usage)` contract
  as the text-only path.
  - **P-LLM2.4 finding**: pre-fix, `AnthropicProvider::build_request_body`
    forwarded the OpenAI-shaped `content` array to Anthropic verbatim —
    Anthropic's real API recognizes neither `"type":"image_url"` nor a
    `"type":"image_data"` block (the latter was `MessageContent::ImageData`'s
    own non-standard, never-OpenAI-compatible wire tag), so every real
    multimodal call built through `MultimodalMessage`/`.multimodal_prompt()`
    against a Claude model was rejected outright, despite the model
    registry declaring `accepts: [text, image]`. The integration test
    didn't catch this because it fed `AnthropicProvider::execute` an
    already-Anthropic-native payload directly, bypassing translation
    entirely. Fixed: `build_request_body` now translates OpenAI-shaped
    `text`/`image_url` blocks (both remote-URL and `data:` URI forms) into
    native Anthropic `text`/`image` blocks, a multimodal *system* message
    (an array, not a string) has its text flattened instead of silently
    dropped, `MessageContent::ImageData` now serializes as a standard
    `image_url` `data:` URI (the real OpenAI wire shape — fixing the same
    silent-drop/malformed-block bug for every OpenAI-compatible vendor and
    for Google's base64 path in the same change), and the integration test
    now drives the real translation path.
- **Anthropic document (PDF) input, Google video input** — landed
  2026-08-13 (P-LLM2.4 follow-up), based on the current official API
  reference at the time (`platform.claude.com/docs/en/build-with-claude/
  pdf-support`; `ai.google.dev/api/generate-content` for the REST `Part`
  schema; `ai.google.dev/gemini-api/docs/video-understanding` for YouTube
  `file_data` usage). `MessageContent` gained `DocumentUrl`/`DocumentData`
  (Anthropic-only) and `VideoUrl`/`VideoData` (Google-only) variants, with
  matching `MultimodalMessageBuilder` methods
  (`.add_document_url()`/`.add_document_data()`/`.add_video_url()`/
  `.add_video_url_with_media_type()`/`.add_video_data()`).
  - **Anthropic**: `document_url` blocks translate into native
    `{"type":"document","source":{...}}` blocks, same base64-vs-URL split
    as `image` blocks. All active Claude models accept PDF documents — no
    beta header required — so all 13 registry entries in `anthropic.yml`
    now declare `accepts: [text, image, document]`.
  - **Google**: `video_url` blocks translate into `inline_data` (base64) or
    `file_data` (remote reference) — YouTube links (`youtube.com/watch`,
    `youtu.be/`) omit `mime_type` per the documented shape; any other
    remote reference (e.g. a Files API URI) requires one, taken from an
    explicit `media_type` hint or defaulting to `video/mp4`. All 42 `type:
    chat` entries in `google.yml` that already declared `accepts: [text,
    image]` now additionally declare `video`.
  - `ModelCapabilities::validate_request` (and the `ModelConfig` wrapper)
    gained a `has_document` parameter — previously the function signature
    only had `has_video`, but every `LLMClient` call site hardcoded it
    `false`, so a model's `accepts: video`/`accepts: document` could never
    actually be checked. Both flags are now computed from the real
    `MultimodalMessage` content at all four call sites
    (`execute`/`execute_full`/`execute_streaming`/
    `build_multimodal_messages`), so an unsupported combination — e.g.
    video input against a non-Google model — fails at validation time with
    a clear `LLMError::InvalidModelConfig` naming the rejected modality,
    per `yanshi-llm/tests/multimodal_input_validation.rs`.
  - Video and document remain unimplemented for every other provider by
    design: OpenAI's Chat Completions API has no video or document content
    block at all (its file/PDF support lives only in the separate
    Responses API, which this crate doesn't implement), and Claude has no
    video input capability. Sending either to a model outside its one
    supporting vendor is rejected by `validate_request`, not silently
    dropped or malformed.
- **Live LLM nightly CI job** — landed 2026-05-08.
  `yanshi-llm/tests/provider_consistency_live.rs` plus
  `.github/workflows/llm-live.yml` (cron + `workflow_dispatch`). Tests
  default to a clean skip; nightly CI sets `YANSHI_LIVE_LLM_TESTS=1`
  along with per-provider API keys from secrets. Each provider asserts the
  same contract as the offline suite: non-empty text, populated
  `TokenUsage`, `StopReason::Stop`.
