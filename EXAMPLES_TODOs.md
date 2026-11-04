# Yanshi Applications TODOs

Last updated: 2026-11-04（A8 content-studio 入队；A6 状态按已落地的四个 workflow 迭代更新）

## 维护约定

- 这里追踪 **dogfooding 用的端到端 application 示例**（`examples/applications/<name>/`），
  区别于：
  - `examples/README.md` — **SDK feature 矩阵**（每个能力一个最小 demo，maintainer-facing）。
  - `examples/ecosystem/` — **生态形态样本**（skills / plugins / marketplace
    标准结构示范，contributor-facing）。
- 这里的 application 必须是「**真实业务场景**」，跨越多个 yanshi 子系统，
  目的是验证「yanshi 真能搭出可用产品」并发现实际使用中的缺口。
- 每个 application 一个目录 `examples/applications/<name>/`，必含：
  - `README.md` — 业务描述 + 架构 + 外部依赖 + 所需 API key
  - 至少一个 `workflow.yml` 或 `skill.toml`（如果用 skill 形态）
  - 自定义 Rust 节点（如需）放 `src/`
  - smoke 测试（若需要 live API 必须 self-skip）
- Dogfooding 过程中发现 yanshi 缺陷，写到本文件对应 application 的 `Findings`
  段；积够再回填 `TODOs.md` 的下一批 segment（例如「P8 Dogfooding-Driven
  Refinements」）。

## 状态约定

- `TODO`: 未开始或正在做
- `WIP`: 进行中（区别于 TODO，已有 commit）
- `DONE`: 已实现 + 跑通 + 文档完整
- `DEFERRED`: 显式推迟（说明原因）

## Active Queue

| # | Application | Status | 验证 yanshi 哪些面 | 外部依赖 |
| --- | --- | --- | --- | --- |
| A1 | [blog-to-podcast](examples/applications/blog-to-podcast/) | WIP — live smoke ✅ (1st run 2026-05-18) | custom Rust node, LLM, HTTP, file, trace, skill | phonon-podcast (path dep), Moonshot LLM + MiniMax TTS (default) / Edge TTS (free) |
| A1.5 | [podcast-mastering](examples/applications/podcast-mastering/) | WIP — live smoke ✅ (1st run 2026-05-18) | **L3 validation**: skill + `[[mcp_servers]]` + ReAct agent + native tool calling driving phonon-mcp subprocess | phonon-mcp binary (`cargo build --release -p phonon-mcp`), Moonshot LLM |
| A2 | [code-reviewer](examples/applications/code-reviewer/) | WIP — live ✅ as L3 skill (2026-05-18) | ReAct agent + shell tool with git/gh allowed_commands; multi-call decision-making | gh CLI + git (system installs), kimi-k2.6 |
| A3 | [research-assistant](examples/applications/research-assistant/) | WIP — live ✅ iter 1 (2026-05-18) | arxiv search API, SqliteEntityFactStore dedup, one-shot LLM briefing | Moonshot LLM；arxiv API 免费；RAG + scheduled run 留 iter 2 |
| A4 | [meeting-transcriber](examples/applications/meeting-transcriber/) | TODO | ASR node, LLM summarize, file output | Whisper (local 或 API) |
| A5 | [weekly-digest](examples/applications/weekly-digest/) | TODO | RAG, LLM, HTTP (SMTP/SendGrid), scheduled | SendGrid/Mailgun/SMTP |
| A6 | [doc-translator](examples/applications/doc-translator/) | WIP — workflow.yml + iter2/3/4 已落地（见 A6 段） | template, batch / map (parallel), LLM, file, shell | LLM API |
| A7 | [changelog-writer](examples/applications/changelog-writer/) | WIP — live ✅ as L1 binary; L3 skill form rejected (2026-05-18) | custom AsyncNode + std::process git + single LlmInit::prompt call; LLM provider registry | 无（git + LLM key） |
| A8 | content-studio（目录待建，`TODOs.md` Y4.4） | TODO | 规划 agent + `WorkflowTool` 三个生成器模板（report / mindmap / audio_overview）+ `FlowValue::File` 产物 + `--output-dir`；是 `docs/RFC_CONTENT_STUDIO.md` 的首个端到端验证 | LLM + TTS key；pandoc + typst；ffmpeg（经 media-tools MCP） |

---

## A1 — blog-to-podcast

**业务**: 输入一篇 blog（URL 或本地 markdown），输出双人对话播客 `.wav` + `.srt`
字幕；可选 BGM / intro / outro / chapter 切分。

**为什么是第一个**: phonon-podcast 已经把 script_gen + TTS + 拼接 + BGM + 字幕
都做完了，yanshi 主要工作是把它包成可观测的 DAG，验证「yanshi + 外部
Rust 库」的集成路径是否顺滑。同时这是个 phonon 作者 + yanshi 作者都会自己
用的真实工具。

**验证 yanshi 哪些面**:
- 自定义 Rust 节点（`PodcastNode` 包 `phonon_podcast::PodcastPipeline`）
- LLM 节点做 blog → outline 提炼
- HTTP 节点拉 blog 原文
- File 节点写产物
- Trace 看每段 TTS 耗时 + LLM token
- 可选包成 skill（`podcast-producer`）让 `yanshi skill run` 触发

**外部依赖**:
- `phonon-podcast` 0.7（path dep 到 `/Users/hal/rustspace/phonon/phonon-podcast`）
- **Default 组合（零 OpenAI 依赖）**:
  - LLM: Moonshot via OpenAI-compat base URL (`MOONSHOT_API_KEY`)
  - TTS: MiniMax T2A v2 via phonon-ai 的 `MiniMaxTts`
    (`MINIMAX_API_KEY`，2026-05-18 在 phonon 70daa58 commit 落地)
- **Free-tier 备选**: Edge TTS（phonon-ai 已有，无 key）替代 MiniMax
- **Premium 备选**: ElevenLabs（高端英文）/ OpenAI TTS

**架构（薄壳方案 A）**:
```
HTTP fetch → LLM outline → PodcastNode (内部走完 phonon 全流程) → File write
```

**架构（拆开方案 B，dogfooding 后期）**:
```
HTTP fetch → LLM outline → PodcastScriptNode (phonon::OpenAiScriptGenerator)
  → PodcastTtsNode (并发 phonon TTS) → PodcastAssembleNode
  → SubtitleNode → File write
```

**TODO 子项**:
- [ ] 听 `/tmp/episode-test.wav` 主观评估：voice 区分度 / 对话自然度 /
      停顿合理性 / 是否需要 BGM 隔开
- [ ] 决定是否升级到 plan B（拆 DAG：fetch → outline → script_gen →
      tts → assemble → subtitle）。触发条件：dogfooding 中遇到
      「想中间编辑脚本再继续」「单段 TTS 失败想 retry」之类。
- [ ] 决定是否包成 skill（`SKILL.md` + persona + tool admission），
      让 `yanshi skill run podcast-producer` 直接触发
- [ ] 加 medium / long 两个 blog fixture（多场景测试）
- [ ] phonon-podcast 上游 PR：`OpenAiScriptGenerator::generate` 的
      `#[instrument]` 在 trace fields 里 dump 全文 `topic`，长 blog
      会把单行 trace 撑到几 KB。建议截断到 80-120 字符。

**DONE 子项**:
- [x] 写 `README.md`（架构图 + 跑法 + 所需 key）— commit 2f4d4b0
- [x] **Prereq: phonon-ai 加 `MiniMaxTts` provider** — phonon repo
  commit 70daa58（2026-05-18）。`MINIMAX_API_KEY` 走 phonon-ai 的
  TtsProvider trait，phonon-podcast `PodcastPipeline::new(MiniMaxTts::new()?)`
  即可用。16 个单测，含 hex 解码、business-error mapping、language_boost
  映射、9 种 emotion 白名单。Streaming SSE 留 follow-up。
- [x] **Plan A 薄壳实现** — standalone Cargo project at
  `examples/applications/blog-to-podcast/`：
  - `Cargo.toml`：empty `[workspace]` 跳出 yanshi workspace；path
    dep 到 yanshi-core + phonon-ai + phonon-podcast + phonon-io +
    phonon-core
  - `src/podcast_node.rs`：`PodcastNode` impl `AsyncNode`，内部串
    `OpenAiScriptGenerator`（Moonshot）+ `PodcastPipeline` +
    `phonon_io::write_audio` + `subtitle::write_srt_file`。
    新增 `estimate_subtitle_timing` helper（按字符数比例分配时长
    把 script segments 转成带时间的 SRT entries）。`TtsBackend` enum
    支持 MiniMax / Edge / OpenAi 切换。
  - `src/main.rs`：`tokio::main` + `tracing_subscriber` + 手写的
    `--blog --output --segments --tts` CLI 解析；`Flow::new` 构造
    2 节点 DAG（`read_blog → produce_podcast`），跑完打印 summary +
    输出路径
  - `fixtures/short_blog.md`：~500 字 zh-CN 关于 Rust 所有权和并发
  - `tests/smoke.rs`：3 个 hermetic CLI 测试（`--help` / 缺 `--blog` /
    未知 flag）+ 1 个 `#[ignore]` live end-to-end，gated 在
    `MOONSHOT_API_KEY` + (`MINIMAX_API_KEY` 或 `EDGE_TTS_OK=1`)
  - 8 unit tests in `podcast_node` 模块 + 3 hermetic integration
    tests + 1 ignored live test。`cargo check` / `cargo clippy
    --all-targets -- -D warnings` / `cargo fmt --check` 全绿。
  - 集成路径已验证：yanshi `Flow` orchestrate + 自定义 `AsyncNode`
    包外部 Rust 库（phonon-podcast）的 path-dep 跨 workspace 集成
    端到端编译 + 测试都通。

**Findings**:

- **2026-05-18 — 默认 LLM model 名称是猜的，不存在**。我默认填了
  `kimi-k2-0905-preview` 想用最新 preview model，第一次 live run 时
  Moonshot API 返回 `404 Not Found the model kimi-k2-0905-preview or
  Permission denied`。`curl https://api.moonshot.cn/v1/models` 显示
  我账号实际可用：`moonshot-v1-{8k,32k,128k}`, `moonshot-v1-auto`,
  `kimi-k2.5`, `kimi-k2.6`, 三个 vision-preview。改默认为
  `moonshot-v1-128k`（长上下文、命名稳定），加 `--model` CLI flag 让
  operator 自己挑。Lesson：**默认模型名要选 "永远不会下架" 的稳定
  名字，preview / dated 名字必须 operator 显式 opt-in**。
- **2026-05-18 — `target_segments` 是 hint 不是硬 cap**。我传
  `--segments 4`，Moonshot 自然展开成 12 段（按 blog 5 个章节自然
  分段）。这是 phonon `OpenAiScriptGenerator` 的 system prompt
  里写的 "Generate approximately N dialogue segments"。对长 blog 想
  限段数得 prompt 更严或加 post-process trim。**不是 bug**，但
  EXAMPLES_TODOs.md / README 应该改 `--segments` 描述为
  "approximate, not strict"。
- **2026-05-18 — `.env` 自动加载是 dogfooding 必需**。第一版只读
  process env vars，每次 `cargo run` 都得 `source ~/.yanshi/.env`，
  太烦。加了 `dotenvy::from_path("~/.yanshi/.env")` 在 main 开头
  silently no-op-when-missing。**这条经验值得提到所有 application
  examples 的 convention**：默认从 `~/.yanshi/.env` 加载，但允许
  process env vars 覆盖（dotenvy 默认行为）。
- **2026-05-18 — phonon trace 太冗长**。每次 `OpenAiScriptGenerator
  ::generate` / `MiniMaxTts::synthesize` 的 `#[instrument(fields(topic
  = %...))]` 都把整个 `topic`（即整篇 blog）dump 进 trace 一行。
  Terminal 输出基本不可读，需要 grep 才能看 trace 流程。phonon 侧的
  PR：截断长 instrument field（>= 80 字符就 `... (N chars)`）。
- **2026-05-18 — yanshi `ConsoleListener` events 干净好用**。
  `[workflow.started] / [node.started] / [node.output.captured] /
  [node.completed] / [workflow.completed]` 自动打，per-node 耗时
  立即可见（read_blog: 189µs vs produce_podcast: 18.78s）。这是
  yanshi 的明显 win。
- **2026-05-18 — `FlowValue::File { mime_type, .. }` 字段叫
  `mime_type` 不是 `media_type`**。第一版我猜错，编译报错才发现。
  Lesson: yanshi public types 的 field naming 可以更早在 SDK 文档
  里固化（已有 `docs/AGENT_SDK.md`，可加一节 "FlowValue field
  reference"）。
- **2026-05-18 — `ConsoleListener` 是 unit struct 没 `default()`**。
  小坑；trivial fix（直接 `ConsoleListener` 实例化）。不算缺陷。
- **2026-05-18 — 性能数据**：12 段 zh-CN 对话（2.5 分钟音频）端到端
  ~19s wall clock（read_blog 0.2ms + Moonshot script gen 几秒 +
  12 次 MiniMax T2A 并发 + 拼接 + 写 WAV），13MB WAV 文件。
  MiniMax 每段 ~150-300ms 响应（含 hex 解码 + Symphonia decode）。
  成本：估 MiniMax ~5000 字符 × 高清档定价 + Moonshot ~2000 token，
  整条 ~¥1 / $0.15。
- **2026-05-18 — guest voice 选错（HK 口音）**。我默认配的
  `Chinese (Mandarin)_HK_Flight_Attendant` 名字里 "HK" 是港式国语
  口音，听上去跟纯普通话 host 不协调（用户反馈"一个普通话一个方言"
  的真因）。这不是 phonon 或 yanshi 的问题 —— 是 default config
  的 voice_id 挑错。1-line fix：换成 MiniMax 另一个纯 Mandarin
  `Chinese (Mandarin)_*` voice。Lesson：**default voice 选择前
  应该试听 MiniMax 提供的 sample audio**（MiniMax console 里有
  voice library + 试听）。
- **2026-05-18 — `segments_to_srt_file` API 实际签名不带 duration**。
  phonon 的 helper 期望 STT TranscriptSegment（带时间），不是 script
  Segment（只有 speaker + text）。自写了 `estimate_subtitle_timing`
  按字符数比例分配时长。**真改进方向**：phonon `PodcastPipeline` 应
  在 render 过程中记录每段实际时长，输出 `Vec<(Segment, f64 duration)>`
  让 SRT 用真实时长。已在 TODOs.md 末尾标了 phonon-podcast 上游 PR
  候选。

---

## A1.5 — podcast-mastering

**业务**: 输入一个录好的播客 `.wav`（典型场景是 A1 的输出），输出
mastered 版本：LUFS 归一化到目标响度、淡入淡出、上传平台可用。

**为什么是 A1.5 而非 A2**: 它是 A1 的 sibling，验证 **L3 集成路径**
（phonon-mcp via stdio JSON-RPC），跟 A1 用同一个 `/tmp/episode-test.wav`
源音频但跨完全不同的代码路径。两者放一起对比能直接看出 L1 vs L3
的工程取舍。

**为什么有价值**: 这是 3-tier 架构里 L3 的最小可验证用例，证明 agent
可以靠 ReAct + native tool calling 串起 6 个独立 MCP 工具完成端到端
工作流，零项目特定 Rust 代码。

**验证 yanshi 哪些面**:
- `[[mcp_servers]]` 启动 native binary 子进程
- `security.mcp_command_allowlist` 安全门把关
- `McpClientPool` + `McpToolAdapter` 自动暴露 14 个 `mcp_phonon_*` tool
- ReAct + Moonshot native tool calling 跨 6 步 linear workflow
- UUID handle 在多次 tool call 之间正确传递（AssetRegistry pattern）

**外部依赖**:
- `phonon-mcp` binary（path: `/Users/hal/.target/release/phonon-mcp`）—
  `cd /Users/hal/rustspace/phonon && cargo build --release -p phonon-mcp`
- Moonshot key（已有，via `~/.yanshi/.env`）
- yanshi CLI release build

**TODO 子项**:
- [ ] 听 `/tmp/episode-mastered.wav` 主观评估 master 质量
- [ ] 验证 `mcp_phonon_audio_loudness` 在 master 后输出的 LUFS
      跟 agent 报的 -16 对得上（agent 没有 re-measure，只是 trust
      tool 的 target_lufs 参数）
- [ ] 考虑加 trim silence 步骤（很多播客头尾有静音）
- [ ] 考虑包成更通用的 audio-mastering skill（接 mp3 / flac）
- [ ] 拼装一个 A1 + A1.5 复合 skill / workflow：blog → podcast →
      mastered，端到端一条命令

**DONE 子项**:
- [x] phonon-mcp binary build OK（`cargo build --release -p phonon-mcp`）
- [x] skill validate 通过，discovers 14 MCP tools
- [x] live end-to-end skill run 通过：7 步 ReAct loop（6 tool calls
      + final answer）；agent 严格按 persona 顺序调工具；handle 链
      正确传递；输出 `/tmp/episode-mastered.wav` 13MB 147.99s
      LUFS-normalized + faded
- [x] 总耗时 ~37s（含 LLM 决策 + 6 次跨进程 MCP 调用）
- [x] 沉 Finding 到本文件

**Findings**:

- **2026-05-18 — SKILL.md 不支持 `model:` 字段**。SKILL.md 的
  frontmatter `model` 始终被 `Default::default()` 覆盖（默认 `gpt-4o`）。
  Bug or feature？SKILL.md 跨工具 portable 不带 yanshi 特定字段
  是合理 design，但 doc / error message 没说明白；用户配了 model
  但被静默忽略很迷惑。**改进方向**：要么 SKILL.md 加 model 支持，
  要么 validate 时 warn "ignoring frontmatter.model in SKILL.md;
  use skill.toml for model config"。
- **2026-05-18 — `yanshi skill validate` 错误信息不够具体**。
  报 `Error: Validation failed` 没说哪一条 validation 失败。
  实际是 `[[mcp_servers]] command '/.../phonon-mcp' executable
  name 'phonon-mcp' is not in security.mcp_command_allowlist`，
  但要 grep loader.rs 才知道。**改进方向**：validate command
  应该把 underlying `SkillError::ValidationError.message` 直接打
  出来，而不是吞掉。可能 anyhow `with_context` 链丢了底层 message。
- **2026-05-18 — Default `mcp_command_allowlist` 设计合理但要文档化**。
  默认只有 `["python", "python3", "node", "npx", "uvx"]` —— 解释器
  脚本可以跑，compiled Rust binary 默认拒绝。这是好的安全 posture
  （强迫 operator 显式审批每个 native binary），但 docs/AGENT_SDK.md
  或 SKILL_FORMAT.md 里要明写「想跑 compiled binary MCP 一定要加
  `security.mcp_command_allowlist`」。
- **2026-05-18 — `mcp_phonon_*` 命名 convention 干净**。yanshi
  自动用 `mcp_<server_name>_<tool_name>` 命名，14 个 phonon tool
  全自动暴露 `mcp_phonon_audio_load` 等，agent 看到的名字一致、
  好预测。
- **2026-05-18 — Moonshot tool calling first-shot 工作正常**。`moonshot-v1-128k`
  对 native tool calling 支持稳定，按 persona 写的步骤严格走 6 步，
  不乱用 tool、不跳步、handle 串接正确。这是 yanshi ↔ Moonshot
  集成的额外验证点。
- **2026-05-18 — phonon-mcp `AssetRegistry` 在 multi-tool-call
  pattern 下完全正常**。每个 `normalize_lufs` / `fade` 返回新 UUID
  handle，agent 自动用新 handle 喂下一步，最终 save 用最新 handle，
  没有用错老 handle 或 leak。
- **2026-05-18 — phonon-mcp 内部 sample rate resample 没说明**。
  源 wav 是 32kHz，audio_load 后 audio_info 报 44100Hz。phonon-mcp
  内部某处做了 resample（可能 audio_load 默认转 44.1k？也可能 LUFS
  pipeline 内部）。不影响功能但行为不可见。**改进方向**：phonon-mcp
  的 audio_load tool description 或 audio_info 输出加一个
  `resampled_from` 字段说清楚是不是被改采样率了。
- **2026-05-18 — agent 没有 verify mastering 后的实际 LUFS**。
  persona 让 agent 在 save 后汇报，但 agent 只 trust normalize_lufs
  的 target 参数，没真的 re-call audio_loudness 量一下结果。这是
  persona prompt 的弱点（没强制要求 post-measure 步骤）。**改进方向**：
  persona 加 "Step 5.5: re-measure with audio_loudness before save,
  report actual achieved LUFS"。
- **2026-05-18 — skill run 总耗时 37s 主要花在 LLM 思考上**。tool
  calls 本身全都 sub-second（最慢的 audio_save 也只 50ms）；agent
  在每步 tool_call 之后要等 LLM 决定下一步，单次决策 2-15s。这是
  L3 跨进程 + LLM-in-loop 架构的固有特性，不是 phonon-mcp 慢。
  对比 A1 用 L1 同进程直接调，PodcastPipeline 走完 12 段 TTS 才 19s。
- **2026-05-18 — `--trace` 输出 `RuntimeTrace` JSON 极其清晰**。
  每个 `plan` / `tool_call` / `tool_result` / `final_answer` 都
  带 timestamp + index，是 dogfooding / debug 利器。这是 yanshi
  的明显 win。
- **2026-05-18 — L1 vs L3 的真实工程取舍数据点**：A1 (L1) 19s 出
  2.5 分钟播客；A1.5 (L3) 37s 对同一文件 mastering。L3 慢的是
  「LLM 决策开销」，不是 IPC 序列化。意味着：**如果工作流形态
  固定，用 L1；如果需要 agent 在中间做决策 / 工具组合，用 L3**。

---

## A2 — code-reviewer

**业务**: 输入 PR URL（GitHub）或本地 diff 文件，输出结构化评审评论（按文件
分组、按严重度排序），可选直接推到 GitHub。

**为什么有价值**: 验证 yanshi 的 ReAct agent + MCP（GitHub server）组合是否
真能搭出可用 reviewer；skill 包装让团队能 `yanshi skill install
code-reviewer` 后立即用。

**验证 yanshi 哪些面**:
- `ReActAgent` 主循环（读 diff → 思考 → 调 tool → 回答）
- MCP 集成（GitHub MCP server 提供 `get_pr_diff` / `add_comment`）
- Skill 包装 + persona + 工具白名单（admission）
- 工具沙箱（shell node 调 git）

**外部依赖**:
- `gh` CLI 或 GitHub MCP server（`@modelcontextprotocol/server-github`）
- `GITHUB_TOKEN`
- LLM API key（建议 Anthropic / OpenAI 强模型，评审质量敏感）

**TODO 子项**:
- [ ] 写 `README.md`
- [ ] 设计 persona（评审风格、关注点）
- [ ] 选 MCP server vs shell 调 gh CLI
- [ ] 把 review 作为 GitHub PR comment 自动发回（write-side，Harness
  Mode approval gate 验证 —— A2 spec 第 3 点，故意推到下一轮）
- [ ] swap shell+gh 路径成 GitHub MCP server (`@modelcontextprotocol
  /server-github`)，验证 MCP 路径下的同样 skill
- [ ] 跑多个 PR fixture，覆盖：超大 diff (>5000 行)、跨语言 (TS+Rust)、
  纯 docs 改动 (Strengths/Verdict 应不同) 等场景
- [x] 修 F-A2-1：parser truncated-JSON 健壮性（详见 commit；下面
  Findings 段已 mark DONE）

**DONE 子项**:
- [x] persona / model / tool admission 设计：kimi-k2.6 + shell with
  `allowed_commands = ["git", "gh"]`
- [x] 写 `skill.toml`（带 persona 极度显式说明"用用户原话 reference，
  不要替换"，应用 A7 finding F-A7-1 教训）
- [x] live 跑通：`Review commit 11b3707`（A1 podcast commit 1166-line
  diff），kimi-k2.6 2 个 tool call + 高质量 markdown review
- [x] review 实际找到 5 个真实 issue 包括：phonon path dep 跨 repo
  不可移植（🔴）、`json!` hack（🟡）、3 个重复 match arm（🟡）、
  `unsafe env::set_var` UB（🟡）、speaker name 字符串脆弱匹配（🟡）
- [x] 把第一次 review 输出存为 fixture：`sample-reviews/commit-11b3707-A1-podcast.md`
- [x] 写 README（架构 + 决策日志 + 抽 answer 的 python 一行解决方案）

**Findings** (2026-05-18, A2 first dogfooding pass):

- **F-A2-1 — `yanshi skill run` 顶层 `🤖 Agent:` 行打印空字符串
  即便 answer 已 produce** —— **DONE 2026-05-18**。深入调查后发现
  根因不是 `result.answer` 没被填充（实际填了），而是 **LLM 响应被
  `max_tokens` 截断时，`serde_json::from_str` 在不完整 JSON 上失败，
  parser fallback 到 `Malformed` 把整个 raw text（含 `{"thought":..,
  "answer":..` JSON 外壳）当 answer 显示**。修复：`react/parser.rs`
  在 strict-parse 失败后做 best-effort 提取 `"answer"` 字段的字符串
  值，handle 截断在字符串内、截断在 escape 后、缺 closing `"` 等情况，
  并 emit `warn!` 让 operator 知道发生了截断+部分恢复（暗示需要调
  `max_tokens` —— F-A7-8）。6 new unit tests 覆盖：truncated mid-
  string / truncated after complete answer / truncated mid-escape /
  no answer field stays Malformed / unescape all common sequences /
  empty answer field stays empty Answer。19 parser tests + 168 agents
  lib tests 全绿。"empty agent line" 的初始观察可能是 LLM 偶发返回
  `{"answer":""}` 的非确定性（这种情况下 println 正确显示空，是
  LLM-side 问题，不是 yanshi bug）。
- **F-A2-2 — L3 skill 在 "agent-decides" 任务上工作得很好**。跟
  A1.5 一起验证：当 agent 真有决策（不是 pass-through）时，kimi-k2.6
  能按 persona 步骤走 2-3 个 tool call、做合理决定、输出结构化结果。
  对比 A7 changelog-writer（pass-through 失败）：这次成功侧面 confirm
  reflection rule 选 tier 的正确性。**Positive validation**。
- **F-A2-3 — kimi-k2.6 严格遵守 persona 的 "用用户原话 reference"
  指令**。给 `Review commit 11b3707`，agent 直接调
  `git show --stat 11b3707` + `git show 11b3707`，没出现 A7 那样
  把 hash 换成 `v1.0.0..HEAD` 的 hallucination。Persona 写得明确
  + reference 在用户原话里是唯一显眼字符串，两个条件共同生效。
  **Positive — A7 教训直接 apply 到 A2 起作用**。
- **F-A2-4 — Review 质量超预期**。在 1166-line diff 里找到 5 个
  真 issue（不是 nit 灌水），severity 分类合理（🔴 vs 🟡 vs 🔵），
  甚至给出可执行的修改建议（"用 `&Path` 而非 `&PathBuf`"、"引入
  `serial_test`"）。Strengths 段也客观平衡，Verdict 决断果敢
  （🟡 Approve with comments）。**Positive**。
- **F-A2-5 — 2 次 run 的 review 不一样但都对**。第一次跑（trace 早期
  在 stderr 里看到的）找到 unsafe env + `/tmp` hardcode + edition=2024
  兼容性 + return-vs-panic skip pattern + media_type 大小写。第二次
  跑（保存的 sample fixture）找到 phonon path dep 跨 repo + json!
  hack + 3 重复 match arm + unsafe env + speaker name 脆弱匹配 +
  `&Path` 惯用法 + io::Error chain 丢失。**共有 1 个（unsafe env），
  其余完全不重叠**。说明 LLM-based code review 是 non-deterministic
  但每次都覆盖一组合理的 issue 子集。dogfooding 含义：对重要 PR
  可能要 review 多次取并集。**或** 在 persona 里要求 agent 系统性
  walk 每个文件而不是挑几个深入。
  ✅ **CLOSED 2026-05-18** as docs (no code fix): added
  "Operating practice: LLM review is non-deterministic" section to
  `examples/applications/code-reviewer/README.md` with the actual
  finding-set comparison table, plus a cross-cutting bullet in
  `examples/README.md` § Conventions covering all
  LLM-judgement-output examples (not just code review). Practice
  guidance: 3-5 runs + union for human use; quorum (≥2 of N) for
  automated gates; don't try to make LLM judgement deterministic
  — use `cargo clippy`/linters for that complementary surface.
- **F-A2-6 — `--trace` 的 JSON 输出 schema 易解析但 stdout 既有
  人类格式又夹 JSON**。`yanshi skill run --trace` 把人类摘要
  + JSON trace 混在 stdout，从中抽 answer 需要小段 python 找 brace
  匹配（README 里有 snippet）。**改进**：分两路输出 —— 人类格式
  到 stderr，JSON 到 stdout，便于 pipe parsing。或加 `--output json`
  / `--output stream-json` 像 harness 那样的 mode。
- **F-A2-7 — 长 diff 推理慢**。1166-line diff → 200s wall clock
  (kimi-k2.6)。多数时间是 LLM 思考。对 daily PR review 流来说能
  接受；对几千行的大 PR 可能太慢。考虑：先 `git diff --stat` 让
  agent 决定关键文件，再分批 review？或 split 成多个 LLM 调用。
- **F-A2-8 — 顺手验证 P9.3 dotenvy auto-load 在 skill run 路径下
  也工作**。`MOONSHOT_API_KEY` 没在 shell env 里但 skill run 能拿
  到 key 跑 kimi-k2.6。**Positive，P9.3 的覆盖面 confirmed beyond
  doctor / workflow run**。
- **F-A2-9 — Harness Mode approval gate 验证 deferred**。原 A2 spec
  第 3 点是测「敏感建议先走人工审批」。当前 iteration 是 read-only
  跑通，没用 write-side tool，没经 Harness Mode。下一轮 iteration
  加 `add_review_comment` 类似的写 PR comment 工具时再走 Harness
  approval flow。**不是 finding 是 scope 显式延后**。
  - **Status (2026-05-18 iter 2)**: ✅ CLOSED via
    `examples/applications/code-reviewer-write/`. End-to-end approval
    flow validated; see F-A2-11..F-A2-13 for follow-ups surfaced
    during validation.
- **F-A2-10 — 我意识到自己花了不少 commit 没 push 也没用 GitHub PR**。
  整个 dogfooding 都在本地 commit，从没创建过真的 PR。A2 用 `gh pr`
  调远程 GitHub 不在本仓库 demo 范围内。**侧面 finding**：dogfooding
  跑了一段后开始模拟"如果有 PR 流程会怎样"才能更有意义。下次可能
  在另一个有真 PR 流的仓库做。
- **F-A2-11 — `yanshi harness run` CLI doesn't wrap registry with
  HookedTool / ApprovalProvider**.
  ✅ **CLOSED 2026-05-18** via the `--approve {none,cli,auto-allow,
  auto-deny}` flag on `yanshi harness run`. Default `none`
  preserves pre-existing behaviour; the other modes install a
  `HookConfig` + matching `ApprovalProvider` around the agent's
  registry. With `--approve cli --profile production` (or
  `auto-allow` for CI), every NonIdempotent tool call now flows
  through the approval gate before executing. Live-tested
  end-to-end against the existing `code-reviewer` skill — first
  shell call fires `approval_requested` with the correct params /
  risk / reason, then the run continues per the approval decision.
  New `ReActAgent::with_tools` helper enabled the CLI to swap the
  registry after `SkillBuilder::build` without duplicating
  manifest/persona/memory wiring. Trying to use the CLI for the
  write-side validation surfaced that `yanshi-cli/src/commands/
  harness/run.rs` builds a bare `ReActAgent` from the skill but never
  calls `yanshi_harness::wrap_registry(...)`. Only the server's
  `LiveHarnessExecutor` wires it. So Harness Mode's approval flow is
  only reachable today via (a) the HTTP gateway or (b) hand-rolled
  binaries (which is what `code-reviewer-write` had to do). **Action**:
  promote the registry-wrap + approval-provider-injection step into
  `yanshi harness run` (probably gated on a `--profile` flag or
  on the skill's manifest declaring write tools). Until then the CLI
  ≠ production Harness contract, and that asymmetry needs a doc fix
  at minimum.
- **F-A2-12 — `HarnessProfile::Local` (default) doesn't trigger
  approval for NonIdempotent tools without an explicit pre-hook**.
  ✅ **CLOSED 2026-05-18** via docs sweep: `HarnessProfile::{Local,
  Dev,Production}` enum docs + `HookConfig::new` + `with_profile`
  rustdoc now explicitly warn about the silent-allow default; the
  approval-gate section of `docs/HARNESS_MODE.md` got a footgun
  callout + an inline comment in the canonical snippet + a pointer
  to `code-reviewer-write` as a reference binary.
  Spent ~15min debugging "why does the approval prompt never fire?"
  before reading `yanshi-harness/src/hooks_runtime.rs::
  resolve_proceed_decision` (~line 369). The escalation rule is:
  `Production` profile auto-escalates NonIdempotent → RequireApproval,
  but `Local` profile only fires when a pre-hook explicitly returns
  `PreToolDecision::RequireApproval`. With no pre-hooks (the
  beginner setup), Local just auto-allows everything. This is the
  H2 design (opt-in by profile or hook) but the default-Local
  behaviour is **silently permissive** which makes the approval
  feature easy to miss in dogfooding. **Action**: (a) docstring for
  `HarnessProfile::default()` and `HookConfig::new` should call out
  this asymmetry; (b) consider a `HarnessProfile::Strict` variant
  that escalates everything for testing; (c) at minimum the binary
  template in `docs/HARNESS_MODE.md` should show `.with_profile(
  HarnessProfile::Production)`.
- **F-A2-13 — `moonshot-v1-128k` loops on identical tool calls and
  hallucinates commit hashes when the value lives in the user
  prompt**. Initial persona had `git show <用户给的 commit 引用>`
  and the model invented `4b4ab6cd0f` / `abc1234` on iteration 1
  despite explicit "verbatim" wording. Inlining the literal commit
  into the persona (`git show 11b3707` rendered at runtime) fixed
  the hallucination — both calls then carried the correct hash —
  but the model still re-called `git show` twice instead of
  advancing to `file:write` step 2. Persona escalation ("if the
  last observation is git-show output, you MUST call file:write
  now") didn't dislodge the loop. **Workarounds tried**:
  - (a) cap `max_tool_calls` (4) — bounds budget cost but only
    validates the shell approval path.
  - (b) **`--prefetch-diff` mode (shipped in `code-reviewer-write`)**:
    run `git show` outside the agent, inline the diff into the user
    prompt, register only `FileTool`. Reliably reaches the
    file:write approval path with 1 tool call and clean
    `FinalAnswer` stop. Recommended dogfooding path for
    moonshot-v1-128k.
  - (c) (not tried this run): upgrade to a stronger model
    (kimi-k2.6 / kimi-thinking-preview) — needs API tier access,
    both 404 on this account today.
  - (d) (not tried this run): wrap the script in `PlanExecuteAgent`
    for a hard pre-committed 2-step plan.
  **Adjacent yanshi gap**: ReAct's anti-loop heuristic could
  detect "same tool + same params, twice in a row" and synthesise
  a stronger steering message ("you already ran this; analyse the
  prior observation instead"). Today it just lets the model loop
  until budget exhausts.
  ✅ **The adjacent gap is CLOSED 2026-05-18**: `ReActAgent::
  run_with_context` now tracks the prior `(tool, params)` and
  appends an `[yanshi steering note (F-A2-13): ...]` to the
  tool-result memory message when iteration N+1 matches N. Trace
  stays clean; tool still runs (advisory, not blocking). With
  this nudge the moonshot loop pathology should self-correct
  inside the LLM's own working memory instead of burning through
  `MaxToolCalls`.

---

## A3 — research-assistant

**业务**: 配置感兴趣的 arxiv 主题（如 "LLM agents", "Rust async"）+ 关键词，
agent 周期性抓新论文、读摘要、与已索引文献做对比、产出周报 markdown。

**为什么有价值**: 验证 yanshi 的 Arxiv node + RAG + memory（"已读过哪些论文"
持久化）+ scheduled run（CronCreate / `/schedule`）的组合。

**验证 yanshi 哪些面**:
- `arxiv` node 抓论文 metadata + 摘要
- RAG ingest 持久索引 + retrieve 做新旧对比
- `MemoryStore`（`SqlitePreferenceStore` 存「关注的主题列表」+
  `SqliteEntityFactStore` 存「这篇我读过」）
- 调度（手动 cron 或 yanshi 自己的 schedule 机制）
- LLM 写周报

**外部依赖**:
- Arxiv API（免费，无 key）
- LLM provider
- 可选 Qdrant（如果用 vector RAG）；不用也能跑 BM25

**TODO 子项**（iteration 2+）:
- [ ] **跨 paper 引用**：用 `yanshi-rag` 索引 paper abstracts，让 briefing
  能 surface "this builds on paper X from last week"
- [ ] **scheduled run**：决定走 OS cron / systemd timer / yanshi 自己
  的 `/schedule`；当前一次性 binary，定时部分待定
- [ ] **per-user preference store**：用 `SqlitePreferenceStore` 持久化关注
  主题列表（多 category 一次跑完）
- [ ] **跑一周看产出质量** —— 主观评估 LLM briefing 是否值得每天扫
- [ ] 写一个 multi-day fixture（>20 papers）验证大 batch LLM 响应不被
  max_tokens 截断（F-A7-8 bump 后理论上 32k 足够，但 100+ paper 时再说）

**DONE 子项**:
- [x] iteration 1 binary：3-node Flow（FetchArxiv → DiffSeen → Briefing）
  通过 `Arc<Mutex<Option<Vec<Paper>>>>` 共享 bus 传 in-memory paper list
- [x] arxiv search Atom feed 解析（quick-xml + serde；handle 版本后缀 + 旧式
  category-prefix id 如 `math.AT/0512345`）
- [x] `SqliteEntityFactStore` 包装 (`SeenStore`)：per-category isolation +
  idempotent mark_seen + 跨 run 持久化
- [x] one-shot LLM briefing 生成（基于 A7 同款 `LlmInit::model().prompt().execute()`）
- [x] live live 验证 cs.AI / 5 papers / 20s 首跑、0.9s dedup-only 复跑
- [x] 11 hermetic unit tests（Atom parse / id extraction / store dedup / prompt build）
- [x] sample-briefings/cs.AI-first-run.md 作为 fixture 保留

**Findings** (2026-05-18, A3 iteration 1):

- **F-A3-1 — `SqliteEntityFactStore` API 自然好用**。`open(path)` →
  `record_fact` / `get_facts(entity_id, include_invalidated)` 三个
  方法就 cover 了 "track-by-id" 这种典型 dedup 场景。`entity_id +
  fact_id` 复合键正合 "per-category, paper-id" 这种命名空间需求。
  P4.7 设计经得起 dogfooding 检验。**Positive validation**。
- **F-A3-2 — `EntityFact.confidence: f32` 字段对"自己抓的"数据不太
  meaningful**。dedup 用例下我每次都填 `1.0`（我刚 fetch 的，没有
  不确定性）。`confidence` 适合 NER / extraction-from-text 场景；
  对外部数据源 ingest 是 noise。**改进**：提供
  `EntityFact::observed(...)` 构造器 default confidence=1.0 (确定性)，
  让 ingest 用例不必每次填同一个魔法数字。
- **F-A3-3 — 节点间传 Rust 结构需要自己拉 shared bus**。3-node DAG
  里我必须维护 `Arc<Mutex<Option<Vec<Paper>>>>` 在节点 struct 之间
  共享 `Vec<Paper>`。`FlowValue::Json` 路由能传值但需要 serialize +
  parse（成本 + 类型丢失）。**改进方向**：`FlowValue` 加一个
  `Rust(Arc<dyn Any + Send + Sync>)` 变体让节点直接传 Rust object
  without serialize？或者 `yanshi-core` 文档 / 例子化"shared bus
  pattern" 作为同进程 Vec/struct 传递的 idiom。Today 是隐式 idiom。
- **F-A3-4 — arxiv Atom XML 解析靠 quick-xml + serde 直接 work**。
  没遇到 schema drift / 命名空间 quirks 之类。Atom 字段稳定。dep
  size +1 个 crate (quick-xml)。Alternative: `roxmltree` 也 work。
- **F-A3-5 — `yanshi-memory` 的 in_memory() 测试 helper 干净**。
  `SqliteEntityFactStore::in_memory().await` 让 4 个 dedup tests
  hermetic 跑（共 0.01s），没有 tempdir / file cleanup boilerplate。
  P4.7 testing convenience design correct。**Positive**。
- **F-A3-6 — `quick-xml` 0.36 在 release mode 编译需要 71s**。
  Cargo cache 后 incremental fast，但 cold build 慢。Alternative
  XML libs（`roxmltree`、`xmlparser`）可能更轻。Low priority — 一次
  cold build 只要 71s，下游 dev loop 都是 incremental。
- **F-A3-7 — LLM 把 prompt 里的 `<abs_url>` placeholder 文本当成
  literal markdown 渲染**。output 出现 `[abs_url](http://...)` 而
  不是裸 URL。Prompt 角度是模型遵循"语法"而非"语义" —— `<abs_url>`
  在 prompt 里是 schema placeholder，但模型把它当成 markdown link
  text。**Iteration 1 acceptable**（链接还是 click 得到），prompt
  下次改成 "Link: \\<http://...\\>" 之类避歧义。
- **F-A3-8 — `yanshi-core` 多 node DAG 通过 dependencies 串行
  + shared bus 传 in-mem Vec 工作良好**。`fetch_arxiv` →
  `diff_seen` → `briefing` 全部从顺序运行，每个节点的 `info!` 日志
  跟 `[node.completed] ... in <duration>` 配合让性能瓶颈一眼看出
  (LLM 19s of 20s total)。**Positive**。
- **F-A3-9 — 第 2 次 run 不需要 LLM 调用，900ms 完成**。dedup 在
  empty unseen set 时短路。这是 production 部署的关键性质（cron
  跑每小时但 LLM 只在真有新东西时调）。Cost-efficient。**Positive**。
- **F-A3-10 — quick-xml 字段顺序 sensitivity**。Atom 标准说 `<author>`
  可以放在任意位置，但 quick-xml serde 默认要求 fields 跟 struct
  顺序匹配 ish。这次没坑到（arxiv 返回固定顺序），但 schema drift
  风险存在。Iteration 2 改用 `#[serde(rename = "$value")]` 或 element-
  buffered 解析更稳。

---

## A4 — meeting-transcriber

**业务**: 输入会议录音（.wav / .mp3 / .m4a），输出转录 markdown +
按 speaker 切分 + 行动项列表（"X 负责 Y，DDL Z"）+ 可选会议纪要。

**为什么有价值**: 验证 yanshi 的 ASR node（StepFun 或 OpenAI Whisper）+
LLM 后处理的组合；典型「音频 → 结构化输出」流程。

**验证 yanshi 哪些面**:
- `asr` node（现有）
- LLM 多轮做摘要 + action item 提取
- 长输入分块（如果会议超过 ASR 上下文限制）
- File 输出多文件（transcript + summary + action items）

**外部依赖**:
- ASR provider（StepFun ASR / OpenAI Whisper API / 本地 Whisper）
- LLM provider

**TODO 子项**:
- [ ] 写 `README.md`
- [ ] 决定 ASR provider（成本 / 质量 / 隐私三角）
- [ ] 准备会议录音 fixture（自己录一段或用公开样本）
- [ ] 写 `workflow.yml`

**DONE 子项**: （待填）

**Findings**: （待填）

---

## A5 — weekly-digest

**业务**: 每周固定时间，从 RAG 索引（前面 A3 / 自己博客 / 收藏夹）查最近一周
新增内容，LLM 生成 digest，通过 SMTP/SendGrid 发邮件给指定收件人。

**为什么有价值**: 验证 yanshi 的 scheduled run + RAG + LLM + HTTP（往外
发请求）完整业务回路；测「无人值守」可靠性。

**验证 yanshi 哪些面**:
- 长期 scheduled run（不是 dev 期间手动触发）
- RAG `search` query
- LLM 写长文
- HTTP node 调邮件 API
- 失败重试 / 死信处理（如果某周邮件没发出去怎么办）

**外部依赖**:
- SendGrid / Mailgun / SMTP server
- 持久 RAG 索引（A3 的产物可以复用）

**TODO 子项**:
- [ ] 写 `README.md`
- [ ] 选邮件 provider
- [ ] 设计失败兜底（重试 + 上次成功时间戳）
- [ ] 跑 4 周观察

**DONE 子项**: （待填）

**Findings**: （待填）

---

## A6 — doc-translator

**业务**: 输入一个 markdown 文件夹（如 `docs/`），目标语言列表（如 `["en",
"ja", "zh"]`），输出按语言分目录的翻译版本，保留 markdown 结构 + code fence
不翻译。

**为什么有价值**: 验证 yanshi 的 `batch` / `map`（并行）+ template +
LLM 大量调用 + file batch write；典型「输入扇出、输出扇入」场景。

**验证 yanshi 哪些面**:
- `map` 节点并行（每个文件 × 每个语言）
- Template 节点做 system prompt 渲染
- 并发上限 / rate limit 应对
- 失败单个文件不阻塞整体（partial failure tolerance）
- Checkpoint 恢复（中途挂了重启不重译）

**外部依赖**:
- LLM provider（Anthropic 中长文翻译质量较好）

**TODO 子项**:
- [x] 写 `README.md`（iter 1 scope + observations）
- [ ] 设计 prompt（保留 code fence / 链接 / 标题层级）— 还在 hello-world 阶段
- [x] 写 `workflow.yml` 用 `map` parallel（iter 1: 4 langs hardcoded, no file I/O）
- [ ] 验证 checkpoint 中途重启
- [ ] 测 100+ 文件 fanout 时的稳定性

**DONE 子项 (iteration 1, 2026-05-18)**:
- iter 1 workflow.yml ships in `examples/applications/doc-translator/`
- Validated end-to-end: `yanshi workflow run` produces 4 sub-flows
  in parallel, returns a fan-in result with 3 OK + 1 ERR translations
- N=3 baseline confirms the failure mode is provider rate-limit, not
  yanshi logic

**DONE 子项 (iteration 2, 2026-05-18)**:
- iter 2 workflow-iter2.yml ships with real file I/O end-to-end
- 2 input markdown fixtures with code fences (input/intro.md, usage.md)
- 8 sub-flows = 2 files × 4 languages with `max_concurrent: 3`;
  `results_summary: {total: 8, ok: 8, err: 0}` — full success
- Outputs persisted to output/<lang>/<file>, code fences and inline
  code preserved across all 4 target languages
- ~25s wall clock (3 batches), ledger now real on-disk markdown

**DONE 子项 (iteration 3, 2026-05-18)**:
- iter 3 workflow-iter3.yml computes work list as `file_list ×
  lang_list` cross product via Tera template → JSON; map consumes
  via `input_mapping`. Adding a language is a one-line YAML change.
- Same 8/8 success as iter 2, files identical in shape; confirms
  the new template-as-list-builder pattern is wire-compatible with
  the existing map consumer.
- 3 new findings surfaced (F-A6-6/7/8) about template node validation,
  JSON auto-detection, and Tera loop introspection workarounds.

**DONE 子项 (iteration 4, 2026-05-18)**:
- iter 4 workflow-iter4.yml uses the new YAML `type: shell` node
  (F-A7-2 fully closed) to `find input/*.md` at workflow startup;
  stdout feeds the cross-product template via input_mapping. Adding
  a markdown file to `input/` is now a zero-line workflow change.
- 8/8 translations succeed; same outputs as iter 2/3.
- Closes A6's original spec on the file-discovery axis. Only iter 5
  (100+ file stress test) remains, and that's quantity not capability.

**Findings (iteration 1)**:

- **F-A6-1 — `map parallel: true` has NO concurrency cap**.
  `yanshi-core::Flow::execute_map_node_parallel` does
  `for item in input_list { tokio::spawn(...) }` unbounded. With
  Moonshot's org concurrency limit of 3, N=4 fan-out hits 429 on
  the 4th item. Real A6 use case (100+ files × N langs = 300+
  fan-out) would shred any provider's limits. **Action**: add
  `parallel: { max_concurrent: N }` map YAML schema + plumb
  through `tokio::sync::Semaphore` in the executor. This is the
  blocker for iter 3+ (the stress-test pillar).
  ✅ **CLOSED 2026-05-18**: `NodeType::Map` gained
  `max_concurrent: Option<usize>` field; `tokio::sync::Semaphore`
  acquired per-sub-flow in `execute_map_node_parallel` when set
  (legacy unbounded behaviour preserved for `None`). YAML factory
  reads `max_concurrent: N` from the map node's parameters.
  `Some(0)` rejected as a config error rather than deadlocking.
  Two new unit tests assert (a) the cap is observed in
  practice (probe node tracks high-water mark) and (b) zero is
  rejected. Live re-run of the A6 workflow with `max_concurrent: 3`
  on N=4 produced 4/4 successes (was 3/4 before the cap).
- **F-A6-2 — `yanshi workflow validate` warns that `input_list`
  isn't in the map schema**, even though the factory accepts it
  (via the generic `initial_inputs` dump path). False-positive
  warning hurts the validate UX. **Action**: declare `input_list`
  / `parallel` / `template` as first-class fields on map nodes in
  `yanshi-cli/src/config/schema.rs`.
  ✅ **CLOSED 2026-05-18**: map ParamSpec list bumped to include
  `input_list` (optional Sequence) and `max_concurrent` (optional
  Integer). `yanshi workflow validate` now reports `✅ Schema
  validation passed` on the A6 workflow instead of 2 false
  warnings.
- **F-A6-3 — per-sub-flow Err is buried inside the results array**,
  not at the map-level. The map node returns `Ok({results: [...]})`
  with `Err` siblings nested inside results elements. A workflow
  author only checking top-level Ok misses partial failures
  silently. **Action**: emit `results_summary: { total, ok, err,
  err_indexes }` alongside `results` on map output; or at minimum
  `tracing::warn!` when any sub-flow returns an Err-containing
  state. Note that `Flow::execute_from_inputs` returning
  `Ok(state_with_errs)` instead of bubbling per-node Err to the
  Flow level is the upstream cause — possibly intentional but
  worth re-evaluating.
  ✅ **CLOSED 2026-05-18**: map node now emits `results_summary:
  {total, ok, err, err_indexes}` alongside `results` (both
  parallel and sequential paths via a shared
  `map_outputs_with_summary` helper). Workflows can route on
  `results_summary.err` via `run_if` without walking the nested
  `results` JSON. An `eprintln!` warning fires on partial failure
  matching the existing logging idiom in `flow.rs`. Back-compat:
  `results` keeps its legacy shape; new field is purely additive.
  2 new unit tests assert the summary shape on partial failure
  and clean runs. The upstream design choice (per-node Err inside
  `Ok(state)`) is intentionally left as-is — it's the right
  default for fan-out workflows where one failure shouldn't tank
  the rest, and `results_summary` is the correct way to surface
  that signal without changing semantics.
- **F-A6-6 — template node parameters trigger false validator
  warnings** when used as initial_inputs for Tera context. The
  template ParamSpec in `yanshi-cli/src/config/schema.rs` only
  declares `template`, `output_key`, `output_format`; any
  workflow-author-defined parameter (e.g. `file_list`, `lang_list`)
  validates as `... is not defined in the CLI schema for node type
  'template'` even though the factory accepts them fine (dumps to
  initial_inputs). Friction for any pattern where the template node
  is used as a list-builder or cross-product computer with literal
  YAML inputs. **Action**: either declare template's ParamSpec list
  as accepting extra keys (like F-A6-2 did for map), OR the schema
  validator should permit arbitrary extra params on template nodes
  by design (since the whole point of template is to consume
  arbitrary Tera context). Surfaced during A6 iter 3.
  ✅ **CLOSED 2026-05-18**: validator now exempts `template`
  nodes from the unknown-parameter check (the whole point of
  template is arbitrary Tera context). `yanshi workflow
  validate` on the A6 iter 3 workflow now reports
  `✅ Schema validation passed`. Existing typo-detection test
  switched to use an `llm` node (closed ParamSpec) so the
  validator contract is still covered for other node types.

- **F-A6-7 — template node requires explicit `output_format: "json"`
  even when the rendered output starts with `[` or `{`**. The
  parser at `yanshi-nodes/src/nodes/template.rs:97` branches on
  `output_format` rather than auto-detecting from the rendered
  shape. Without the explicit hint, a JSON-array-rendering template
  lands as `FlowValue::Json(String)` and downstream map nodes
  error `Input 'input_list' must be a JSON array`. Fixing A6 iter 3
  took two debug rounds because the failure message points at the
  consumer (map) not the producer (template). **Action**:
  auto-detect — if `rendered.trim_start().starts_with('[' | '{')`,
  attempt `serde_json::from_str` and fall back to String if it
  fails. Keep `output_format: "json"` as an explicit override that
  errors loudly when parse fails (current behaviour). Surfaced
  during A6 iter 3.
  ✅ **CLOSED 2026-05-18**: default branch (text / unset) now
  opportunistically attempts `serde_json::from_str` when the
  trimmed rendered output starts with `[` or `{`. Successful
  parse → `FlowValue::Json(Array|Object)`; parse failure falls
  back to String (safe for prose templates that incidentally
  start with `{`). The explicit `output_format: "json"` path is
  preserved as strict mode (warns on parse failure). A6 iter 3
  refactored to drop the explicit hint; live re-run still produces
  8/8 successes with the auto-detect log line confirming
  `(auto-detected JSON array via leading '[')`. All 25
  yanshi-nodes tests + 39 yanshi-cli tests pass.

- **F-A6-8 — Tera `loop.parent.*` introspection doesn't work in
  this Tera version**, so cross-product comma logic via
  `{% if not loop.first or not loop.parent.first %},{% endif %}`
  emits a comma right after the opening `[`, producing invalid
  JSON. Workaround: an explicit `needs_comma` flag manipulated via
  `set_global`. **Not an yanshi bug** but worth recording as a
  templating convention: prefer `set_global` accumulators over
  Tera loop introspection for any list-of-N rendering pattern.
  Documented in the A6 iter 3 workflow comments. Surfaced during
  A6 iter 3.

- **F-A6-5 — `input_mapping` can only reference upstream node
  outputs, not the map iteration `item`**. Per-iteration `item`
  fields (e.g. `item.read_path`) are visible in Tera template
  contexts (template node uses them fine) but
  `input_mapping: { path: "{{ item.read_path }}" }` doesn't work
  on non-template nodes — the factory parser only matches
  `{{ nodes.X.outputs.Y }}` literals (see
  `yanshi-cli/src/executor/factory.rs:298`). The workaround in
  A6 iter 2 is a 2-node-per-path detour (`render_read_path`
  template → file:read with input_mapping pulling from it),
  which doubles the sub-flow line count for what should be a
  one-line wire. Friction grows linearly with how many per-iteration
  values a sub-flow needs (read path, write path, source lang,
  ...). **Action**: extend the input_mapping grammar to also
  accept `{{ item.* }}` lookups inside a map sub-flow (parse
  leftmost segment as `nodes` OR `item`), OR have file/llm nodes
  Tera-expand their string parameters at execute time. The first
  has smaller blast radius. Surfaced during A6 iter 2.
  ✅ **CLOSED 2026-05-18**: factory parser now recognises
  `{{ item.field }}` / `{{ item.foo.bar }}` in YAML
  `input_mapping`, encoded as `(("!item", "path.path"))` tuples
  via a sentinel source-node id (`!` is a reserved YAML char so
  no collision with real node ids). `gather_inputs` walks the
  dotted path against the seeded `item` initial input and inserts
  the resolved value. A6 iter 2 refactored from 6-node sub-flows
  to 4-node sub-flows; 8/8 still succeed, files still land on
  disk. 2 new unit tests in `yanshi-core::flow::tests` cover
  flat + nested item lookups + missing-path error reporting.
  130 lib tests pass (was 128; +2).

- **F-A6-4 — prompt ambiguity: "translate to English" when source
  is already English produces unrelated language output**.
  Workflow-author trap: validate `source_lang != target_lang`
  before dispatching. Easy guard at the `build_prompt` template
  step (Tera `{% if %}`). **Not an yanshi bug**, but worth
  documenting in examples conventions: "translation workflows
  should always check source != target before LLM dispatch".
  ✅ **CLOSED 2026-05-18**: added a translation-specific bullet
  to `examples/README.md` § Conventions covering the trap +
  recommending either filtering `input_list` or guarding via Tera.
  The A6 workflow.yml itself drops `en` from input_list (source
  is English) so the demo doesn't hit the trap; comment in the
  workflow points readers at the conventions doc. Re-running A6
  with the new `[ja, fr, de, zh]` set produces 4 distinct-language
  translations with no en→en degeneracy.

---

## A7 — changelog-writer

**业务**: 输入 git tag 范围（如 `v1.0.0..HEAD`），shell 节点跑 `git log`
拿提交，LLM 按 conventional-commits 分类（feat/fix/docs/chore），生成
markdown changelog 段。

**为什么有价值**: **yanshi 给自己用** —— 每次 release 时跑一遍，验证基础
工具链；纯本地无外部依赖，最适合频繁 dogfood。

**验证 yanshi 哪些面**:
- `shell` node 跑 `git log --pretty=...`（验证 shell admission + sandbox）
- LLM 分类 + 改写
- File 节点写 / append CHANGELOG.md
- 测「OS sandbox 把 shell 限制在 git 命令内」是否真有效

**外部依赖**:
- 无（git 在 PATH 上即可）；LLM 用 mock provider 也能 dry-run

**TODO 子项**:
- [ ] 写 `README.md`
- [ ] 设计 prompt（conventional commits 分类规则）
- [ ] 决定要不要把它包成 `yanshi changelog` CLI 子命令（升级成 P3.x）
- [x] 把 `max_tokens` 在 templates/default_models.yml 里调高
  （F-A7-8，2026-05-18 commit；94 个 text 模型从 4096 → 32768，
  multimodal/tts 不动）
- [ ] 解决 F-A7-2 `shell` node 在 schema 但不在 factory 的不一致

**DONE 子项**:
- [x] 决定方案：L1 binary（原计划 YAML 工作流不可行，因 `type: shell`
  没注册；L3 skill 形态试了失败，详见 Findings）
- [x] 实现 `RunGitLogNode` (std::process git log) + `ClassifyAndRenderNode`
  (one-shot LlmInit::prompt) + 2-node Flow + CLI
- [x] live 跑通：`v0.2.0..HEAD` 399 commits → 11k 字符 markdown
  到 `/tmp/CHANGELOG-v0.2.0-to-HEAD.md`，~117s wall clock
- [x] 给 yanshi 自己生成 CHANGELOG（dogfood 完成）
- [x] 沉 10 个 Finding 到本文件
- [x] 顺手在 `yanshi-llm/templates/default_models.yml` 加
  `kimi-k2.5` + `kimi-k2.6` 进 registry（带 `temperature: 1.0`
  for k2.6）

**Findings** (2026-05-18, A7 first dogfooding pass):

- **F-A7-1 — L3 skill form rejected after multi-model fail**.
  Original spec called for `[[tools]] name = "shell" allowed_commands
  = ["git"]` skill driving a ReAct agent. Across `moonshot-v1-128k`
  和 `kimi-k2.6`，agent **始终把用户提供的 range 替换成 hallucinate
  的"典型例子"**（`v1.0.0..v1.1.0`、`v1.0.0..v2.0.0`、
  `v1.2.3..v1.3.0`）—— 即使 user message 给的是真实存在的 tag
  (`v0.2.0..HEAD`)。多版 persona ("永远用用户原话里的字符串") 不起
  作用。**Reflection-doc 规则现场验证**：「固定 pipeline → L1；
  agent 在中间挑分支 → L3」。Changelog 生成 zero agent decision
  → L1 binary 是对的，skill 形态在跟架构对抗。
  `skill.toml.rejected` 文件保留在 binary 旁作为文档。
- **F-A7-2 — `type: shell` 在 permission classifier 里但不在 CLI
  factory 里** —— **DONE 2026-05-18** (honesty-note 路线，不是 full
  factory add)。`classify_node` 的 shell 分支保留（permission shape
  仍然是有信息量的）但加显式 note：
  "not wired into the CLI workflow factory; use the shell tool from
  a skill / harness instead, or shell out from a custom AsyncNode
  binary"。这样 `yanshi workflow validate --explain-permissions`
  对 `type: shell` 节点诚实告知它在 YAML 不能直接跑。Full ShellNode
  factory wrap 需要设计 SandboxPolicy 注入、allowed_commands YAML
  schema、`Arc<SandboxPolicy>` 从 workflow config 到 Tool 的串接 ——
  ~200-300 LOC 的真正功能而不是 small fix。A1/A1.5/A7/A2 dogfooding
  没遇到 "shell-in-YAML 必需" 场景，可待真有需求时再做。 Schema
  validator 一直能 catch `type: shell` 为 "not supported by the
  CLI workflow factory"（unchanged），permission report 现在跟它
  对齐口径。Test
  `cli_workflow_validate_explain_permissions_shell_node_capability`
  updated to assert the new note。
  ✅ **FULLY CLOSED 2026-05-18** (real factory add this time): A6
  iter 3's reach for file discovery surfaced the need. New
  `ShellWorkflowNode` in `yanshi-cli/src/executor/shell.rs`
  wraps `yanshi_tools::ShellTool` with a `SandboxPolicy` built
  from YAML params; `allowed_commands` is a REQUIRED schema field
  so workflows fail validate at parse time if missing (no
  permissive-by-default arbitrary code execution). `command` can
  come from either parameters or `input_mapping`. Output keys:
  `stdout` (string), `is_error` (bool). Honest-note removed from
  the permission classifier. 3 new workflow tests (accepts with
  allowlist / rejects without / explain-permissions). Live-tested
  with `find examples/applications/doc-translator/input -name
  '*.md'` from a YAML workflow — discovered both fixture files,
  stdout flowed through to a downstream template node.
- **F-A7-3 — Model registry 加载：per-provider `config/models/*.yml`
  是死代码** —— **DONE 2026-05-18**。删了 6 个 dead 文件
  (`anthropic.yml`/`dashscope.yml`/`google.yml`/`moonshot.yml`/
  `openai.yml`/`step.yml`)。背景：`yanshi-llm/src/config/vendor_configs.rs`
  是一个 split 工具（把 monolithic config.yml 切成 per-vendor 文件），
  但 split 输出从来没被 runtime registry 读取，是 misleading dead
  artefact。同时 update 3 个误导性文档（`AGENTS.md` × 2 处，
  `IMPLEMENTATION_STATUS.md`，`GRANULAR_MODEL_TYPES.md`）—— 之前
  这些文档都把 `config/models/` 当成权威路径，导致添加新 model 的
  contributor（包括我）改错地方。现在统一指向真实源
  `templates/default_models.yml`。`config/config.yml` + vendor_configs
  split 工具本身保留（独立用途，跟 registry 加载是两回事）。
- **F-A7-4 — 用户级 `~/.yanshi/models.yml` 静默覆盖 built-in
  registry**。`Yanshi::init()` 优先级是 YANSHI_MODELS_CONFIG
  > `~/.yanshi/models.yml` > built-in。意味着：往
  `templates/default_models.yml` 加 model 对已有 user-level
  models.yml 的用户不起作用。lib.rs rustdoc 里写了但 `yanshi
  doctor` 不 surface 当前用的是哪个 source。应该显眼报告
  "models config source: <path>"。
- **F-A7-5 — `kimi-k2.6` 强制 `temperature: 1.0`**。Moonshot 拒绝
  其它值，HTTP 400 `invalid temperature: only 1 is allowed for
  this model`。可能是 reasoning-model 约定。已在
  `templates/default_models.yml` 修正并带注释。手动 copy kimi-k2.6
  到自己的 models.yml 但没读注释的用户会撞墙。值得在 yanshi-llm
  provider 文档里 surface。
- **F-A7-6 — `yanshi-llm` registry 滞后 Moonshot 实际 model 列表**。
  `kimi-k2.5` 和 `kimi-k2.6` 在真实 Moonshot 账号的 `/v1/models`
  里有，但 yanshi registry 直到这次 commit 才加。yanshi 没有
  auto-detect drift 的机制。任何 provider 发新 model 时模式会
  重现。可能的改进：`yanshi llm models --refresh-from-api`
  子命令，拉各 provider 的 `/v1/models` 报告 add/drop。低优先级
  但值得记。
- **F-A7-7 — yanshi-cli 有 P9.3 dotenvy auto-load；A7 binary
  又复制了一份**。binary 有自己的 `load_yanshi_dotenv()` 因为
  它是 standalone Cargo project，不通过 yanshi CLI 调用。模式
  能用但 duplication 是 smell。长期：抽 `yanshi-dotenv` helper
  crate，或在 `docs/AGENT_SDK.md` 文档化标准 snippet。低优先级。
- **F-A7-8 — moonshot-v1-128k 在 4096 max_tokens 下大输出被截断**
  —— **DONE 2026-05-18**。`templates/default_models.yml` 里把所有
  text 类 model 的 `max_tokens: 4096` 整批 bump 到 `32768`（94 个
  text 条目），multimodal（12 个）和 tts（1 个）保持 4096 不动
  （vision 输出短描述够用、tts max_tokens 语义不同）。Bump 选 32k
  是因为：(a) 现代各家 chat model 都支持 8k+ 输出，32k 是大多家
  上限（Moonshot 32k、OpenAI gpt-4o 16k、Anthropic 8k 默认 64k 可
  请求、Gemini 65k），保险范围足够；(b) 用户期望 "1M output" 实际
  指 context window；输出 cap 通常 16-64k，32k 是 90 分位 safe
  default；(c) 配 F-A2-1 truncation recovery 是双保险。Registry
  tests 全绿。
- **F-A7-9 — `yanshi-llm` 对 354k-char 输入在 moonshot-v1-128k
  花了 117s**。不是 bug —— 长 context inference 在 Moonshot 这边
  本来就慢。但 long-context dogfooding 真的烧 wall clock；workflow
  需要迭代长 context 时 batch / cache / smaller-model 策略重要。
- **F-A7-10 — One-shot LLM 输出质量超预期**。即便 399 commits 模型
  也产出干净的分类，scope 保留（`feat(cli):` 正确归到 Features），
  并且超出 prompt 加了 GitHub commit URL link。Single-shot prompt
  approach 显然对这个任务是对的；验证 L1 + one-LLM-call 模式不仅
  "能用"，而且结果真正可用。

---

## Cross-References

- `TODOs.md` — yanshi 主任务队列；从 dogfooding 涌出的缺陷回填到这里
- `examples/README.md` — SDK feature 矩阵（性质不同，互补）
- `examples/ecosystem/` — 生态形态样本（性质不同，互补）
- `docs/RELEASE_NOTES_v1.0.0-rc.1.md` — release notes draft，dogfooding 阶段
  保持 DRAFT 不动
