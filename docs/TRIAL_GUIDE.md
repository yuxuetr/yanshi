# Yanshi 试用手册（Trial Guide）

**版本基线**：2026-08-09（V 段收口后 / W 段启动前）
**适用读者**：想在真实工作中试用 Yanshi 的使用者（含项目作者本人 dogfooding）
**配套文档**：成熟度依据见 `docs/PROJECT_EVALUATION_2026-08-09.md`；已知问题与修复队列见 `TODOs.md`（W 段 / P-LLM2 段）
**手册内所有命令与参数均已对照 `yanshi-cli` 源码核验**（2026-08-09）

---

## 0. 先读：当前哪些路径能用、哪些先别碰

Yanshi 的 CLI 是当前最成熟的入口，但各路径成熟度差异大。试用请沿"绿灯"路径走：

| 状态 | 路径 | 说明 |
|:---:|---|---|
| 🟢 | `workflow run / validate / debug`（本地 DAG） | 测试最厚的一条链，checkpoint / trace 齐备 |
| 🟢 | `harness run --skill <目录> --approve cli` | **唯一带真实工具的 harness 路径**，审批/日志真实生效 |
| 🟢 | `harness chat --skill <目录>` | 多轮 REPL，跨轮记忆 |
| 🟢 | `skill init / validate / test / run / chat` | 技能开发闭环 |
| 🟢 | `workflow dynamic --dry-run` → `--approve cli` | LLM 生成计划→编译执行，治理设计谨慎 |
| 🟢 | `doctor` / `trace replay` / `trace tui` | 诊断与复盘 |
| 🟡 | `harness run-flow`（DAG 套 harness 信封） | 只有信封+节点事件，**无工具审批治理**（代码自述） |
| 🟡 | `serve` + Web UI | 单机看 run 列表/事件流可以；别依赖服务端执行 skill |
| 🔴 | `harness run --model`（不带 `--skill`） | 空工具注册表，agent 无手无脚，没有意义 |
| 🔴 | `harness run --runtime handoff/blackboard/debate` | 静默回落成 ReAct，事件还谎报类型（评估 D7） |
| 🔴 | 服务端 `POST /v1/skills/{name}:run` | 必失败 stub（评估 P0-2） |
| 🔴 | 给 `agent`/`skill_agent` 节点配 `max_retries` | 会吞掉部分恢复输出（评估 P0-4，W0.4 修复前避免） |

**两个横切注意事项**（修复前全程有效）：

1. **流式末尾丢字**（评估 P0-3 / W0.3）：OpenAI / Anthropic / Google / Moonshot 四家的流式路径在特定网络时序下会静默丢失回复末尾内容。agent 循环内部一律走流式，所以**任何 provider 的 agent 回答都可能受影响**。试用期若发现回答"话没说完"，先怀疑此 bug 而非模型。StepFun 已修复，可作对照。
2. **大列表 Map 慎用**：并行 Map 的每个子项会在 `~/.yanshi/runs/` 下新建一个 UUID 目录，且子流程不可取消（评估 P1-3）。试用期 Map 输入控制在个位数。

---

## 1. 环境准备（10 分钟）

### 1.1 构建与安装

```bash
cd <repo-root>
cargo install --path yanshi-cli
yanshi --help
```

### 1.2 初始化配置与 API key

```bash
yanshi config init
# 生成:
#   ~/.yanshi/models.yml   模型配置
#   ~/.yanshi/.env         API key 模板
```

编辑 `~/.yanshi/.env`，至少填一个厂商的 key。各厂商环境变量名（与
`yanshi-llm/config/config.yml` 一致）：

```
OPENAI_API_KEY / ANTHROPIC_API_KEY / GEMINI_API_KEY / MOONSHOT_API_KEY /
DASHSCOPE_API_KEY / STEP_API_KEY / GLM_API_KEY / DEEPSEEK_API_KEY / MINIMAX_API_KEY
```

验证：

```bash
yanshi config validate
yanshi llm models          # 列出注册表中可用模型
```

### 1.3 体检

```bash
yanshi doctor --profile local
```

关注三块：LLM 配置可加载性（缺哪个环境变量会直接列出）、沙箱后端与
`enforcement_level`、run/trace 目录可写性。`--format json` 可给脚本消费；
`--profile production` 会把部分 Warning 升级为 Fail（上生产前再用）。

### 1.4 打开 trace（强烈建议试用期全程开启）

```bash
export YANSHI_TRACE_DIR=~/.yanshi/traces   # 这也是默认值，显式设置便于确认
```

`workflow run` 默认就写文件 trace；这是后面复盘环节的数据来源。

---

## 2. 场景一：第一条静态 DAG 工作流（30 分钟）

### 2.1 跑通示例

仓库自带可用示例（`yanshi-cli/examples/workflows/`）：

```bash
yanshi workflow validate yanshi-cli/examples/workflows/fixed_dag_basic.yml
yanshi workflow run yanshi-cli/examples/workflows/fixed_dag_basic.yml
```

多分支示例（体验并发调度）：

```bash
yanshi workflow run yanshi-cli/examples/workflows/fixed_dag_multibranch.yml \
  --execution-mode concurrent --max-concurrency 4
```

### 2.2 写你自己的流水线

推荐从"模板 → LLM → 模板"三节点起步：

```yaml
# my_pipeline.yml
name: "daily-brief"
inputs:
  topic:
    description: "主题"
    required: true
nodes:
  - id: build_prompt
    type: template
    parameters:
      template: "请就以下主题写一份 200 字要点简报：{{topic}}"

  - id: draft
    type: llm
    dependencies: ["build_prompt"]
    input_mapping:
      prompt: "{{ nodes.build_prompt.outputs.output }}"

  - id: format_out
    type: template
    dependencies: ["draft"]
    input_mapping:
      body: "{{ nodes.draft.outputs.output }}"
    parameters:
      template: "# 简报\n\n{{body}}"
```

```bash
yanshi workflow validate my_pipeline.yml --strict
yanshi workflow run my_pipeline.yml --input topic "Rust 异步生态" -m <你的模型名>
```

注意：`workflow run` 的 `--input` 是 **KEY VALUE 两个参数**（`--input topic
"..."`），不是 `key=value`（那是 `harness run-flow` 的写法，两者不同）。

试用期 YAML 三条军规（对应评估已知缺陷）：

- **不要**让工作流级 `inputs:` 的名字与任何节点参数同名（会静默覆盖全图同名
  参数，评估 P2 输入优先级问题）；
- 同一输入**不要**同时给 `parameters` 与 `input_mapping`（静态值会静默胜出）；
- `run_if` 只引用"确定已存在"的上游输出（引用失败节点会整体中止且不留终态，
  评估 P1-6）。

### 2.3 调试与检查点

```bash
yanshi workflow debug my_pipeline.yml --visualize --analyze   # 看 DAG 结构
yanshi workflow debug my_pipeline.yml --dry-run --verbose     # 不执行的演练
```

体验断点恢复：跑一个多节点流程，中途 Ctrl-C，然后：

```bash
yanshi workflow resume-plan <run_id>    # 查看恢复计划：哪些节点跳过/重放/需人工
```

每次 run 的产物在 `~/.yanshi/runs/<run_id>/`（每节点一个
`<node>_outputs.json`），checkpoint 在 `~/.yanshi/checkpoints/`。

---

## 3. 场景二：Skill + 受治理的 Agent 会话（核心场景，1 小时）

这是 Yanshi 差异化最强的路径：把 agent 的人格/工具/知识/权限打包成
Skill，在 Harness 治理（审批、限额、审计日志）下运行。

### 3.1 创建并验证一个 Skill

```bash
yanshi skill init my-reviewer --description "代码/文档审查助手"
```

编辑生成的 `my-reviewer/SKILL.md`，最小可用形态：

```markdown
---
name: my-reviewer
description: 审查代码与文档，给出结构化意见
allowed-tools: file_read
---

你是一名严格但建设性的审查助手。收到文件路径时先读取内容，
然后按"问题 / 风险 / 建议"三段输出意见。
```

```bash
yanshi skill validate my-reviewer                        # manifest 校验
yanshi skill list-tools my-reviewer                      # 看它实际拿到哪些工具
yanshi skill inspect my-reviewer --explain-permissions   # 逐工具权限解释（推荐看一次）
yanshi skill test my-reviewer --dry-run                  # 校验 + 工具发现
```

### 3.2 单发运行与多轮对话

```bash
# 单发（快速验证）
yanshi skill run my-reviewer -m "审查一下 ./src/main.rs" --trace

# 多轮（sqlite 记忆跨进程续聊）
yanshi skill chat my-reviewer --memory sqlite --session review-1
```

### 3.3 上 Harness：审批 + 限额 + 会话日志

```bash
yanshi harness run "审查 yanshi-core 的 checkpoint 模块并总结风险" \
  --skill my-reviewer \
  --approve cli \
  --profile local \
  --max-steps 12 \
  --timeout-ms 300000 \
  --cost-limit-usd 0.50
```

体验要点：

- 非幂等工具调用（shell / 文件写入 / 变更型 HTTP）会弹出 stdin 审批：
  `y`(一次) / `s`(本会话) / `r`(本 run) / `n`(拒绝) / `q`(拒绝并停)。
  **注意**：`s`/`r` 的放行目前**按工具名**缓存——对 shell 放行一次等于放行
  本会话任意命令（评估 P1-2，W1.7 修复前建议只用 `y` 逐次批）。
  **注意**：`q`（DenyAndStop）当前不会真正停下 agent 循环，只会让后续
  工具调用全部被拒、直到步数耗尽（评估 P0-5，W0.5 修复前知悉）。
- 会话事件写入 `~/.yanshi/runs/harness/sessions/<session>.jsonl`，逐行
  JSON 可直接阅读。**已知问题**：`harness run` 路径下审批事件与运行时事件
  的 seq 会重复编号（评估 P1-4，不影响内容只影响排序字段；`chat` 路径无
  此问题）。
- 长会话可加 `--token-budget 32000`（超预算自动压缩旧对话并发
  `memory_summary_added` 事件）与 `--context-budget 8000`（workspace 上下文
  按真实 tokenizer 裁剪）。
- 复跑/续聊：`--session <id>` 固定会话 id；`harness list` 列会话、
  `harness inspect <id>` 看详情、`harness replay <id>` 重放事件流。
  被 `ask_user` 中断的会话用 `harness resume-loop <id>` 携答案续跑
  （注意 `resume` 是纯回放、`resume-loop` 才是真恢复，两者不同）。

### 3.4 给 Skill 加知识与 MCP 工具（进阶）

- 知识分层：`skill.toml` 的 `[[knowledge]]` 条目 `backend = "files"`（小文档
  内联进人格）或 `backend = "rag"`（大语料建 BM25 索引 + `rag_search` 工具
  按需检索）。**中文语料注意**：BM25 目前无 CJK 分词，中文检索质量有限
  （评估 §3.3，P-LLM2 之外的已知项）。
- MCP：frontmatter 里声明 `mcp_servers` 后工具自动以 `mcp__<server>__<tool>`
  注入；先用 `yanshi mcp list-tools` / `call-tool` 单独验证 server 可用。
  示例见 `examples/skills/` 与 `yanshi-cli/examples/workflows/mcp_*.yml`。

---

## 4. 场景三：动态工作流（30 分钟）

让 LLM 针对目标自动生成计划，编译成并行 DAG 执行。**永远先 `--dry-run`**：

```bash
# 第一步：只看计划，不执行
yanshi workflow dynamic \
  --goal "读取 ./docs 下所有 md 文件的标题，汇总成一份索引" \
  -m <你的模型名> \
  --allow-path ./docs \
  --dry-run

# 第二步：确认计划合理后，带审批执行
yanshi workflow dynamic \
  --goal "..." -m <模型> \
  --allow-path ./docs \
  --approve cli --profile local
```

要点：

- 文件/网络访问必须显式授予（`--allow-path` / `--allow-domain`，可重复）；
  **shell 永不可用**——这是刻意设计（LLM 生成的计划按对抗性输入对待）。
- 默认 `--profile dev` 是**不审批**的（为本地迭代保留）；正式使用务必带
  `--profile local --approve cli`。
- 当前计划只支持"工具步 + agent 步"的无环 DAG，**不支持条件与循环**；某步
  失败即整 run 失败（失败重规划 `run_with_replan` 已实现但尚无 CLI 入口，
  见 TODOs W2.1/W2.2）。

---

## 5. 复盘：每次试用后花 5 分钟

```bash
yanshi trace replay <run_id>                      # 时间线重放（不重新执行）
yanshi trace tui <run_id> --filter tool --details # 聚焦工具调用，展开字段
yanshi trace tui <run_id> --filter agent          # 聚焦 agent 步骤
```

复盘时问自己四个问题（每个都直接映射到改进队列）：

1. **agent 有没有绕圈？**（相同工具+参数反复出现 → loop detection 是否触发；
   触发太晚/太早 → 调 `LoopDetectionConfig` 或记录到 TODOs）
2. **哪个节点/工具最慢？**（时间线上的长条 → 是否该并行、该缓存）
3. **审批打断是否合理？**（太频繁 → 幂等性标注问题；太宽松 → 缓存粒度问题）
4. **回答质量问题出在检索、prompt 还是模型？**（`--trace` 的结构化步骤 +
   `LlmPromptSent` 事件可以还原每次 LLM 实际收到什么）

---

## 6. 把使用发现变成改进（反馈闭环）

这个仓库的质量来自"评估 → 修复 → 留痕"的循环。试用期把评估来源从"审计"
换成"实际使用"，闭环方法不变：

1. **当天记录**：发现的问题直接追加到 `TODOs.md` 对应段落（多数会落在 W 段
   已有条目上——那就在条目下加一行实测佐证；新问题按 `W1.x`/`W2.x` 惯例
   新增，带复现命令）。
2. **每周分诊**：按"撞上频率 × 修复成本"重排 W 段顺序。例如：一周内被
   While 静默耗尽坑过 → W2.4 提级；审批烦不胜烦 → W1.7 提级。
3. **坏 case 固化为基线**：反复出现的质量问题写成 eval 用例——agent 侧用
   `yanshi eval`（mock provider 基线已入 CI），检索侧用
   `yanshi rag eval`（Recall@K / MRR / nDCG + 配对检验门禁）。修复必须
   让基线变绿，防止修东坏西。
4. **改 SKILL.md 优于改代码**：审查风格、输出格式类的不满优先通过技能的
   persona / 知识条目迭代解决，`skill test` 回归；只有框架行为问题才进
   TODOs。

**建议的试用节奏**：第 1 周先修 TODOs 的 W0.3 / W0.4 / W0.6 / W0.7（四个
数十行级修复，直接改善 CLI 体感）→ 第 2–3 周按本手册三个场景各建一个真实
任务每天用 → 第 4 周做第一次分诊，重排 W 段。

---

## 7. 常用命令速查

```bash
# 诊断
yanshi doctor --profile local
yanshi config validate
yanshi llm models

# 工作流
yanshi workflow validate <yml> --strict --explain-permissions
yanshi workflow run <yml> --input KEY VALUE -m <model> [--execution-mode concurrent]
yanshi workflow debug <yml> --visualize --analyze --dry-run
yanshi workflow resume-plan <run_id>
yanshi workflow dynamic --goal "..." -m <model> --allow-path <p> --dry-run

# Skill
yanshi skill init <dir> --description "..."
yanshi skill validate <dir> && yanshi skill test <dir> --dry-run
yanshi skill inspect <dir> --explain-permissions
yanshi skill run <dir> -m "<消息>" --trace
yanshi skill chat <dir> --memory sqlite --session <id>

# Harness
yanshi harness run "<输入>" --skill <dir> --approve cli --profile local \
  --max-steps 12 --cost-limit-usd 0.50
yanshi harness chat --skill <dir>
yanshi harness list / inspect <id> / replay <id> / resume-loop <id>

# 复盘
yanshi trace replay <run_id>
yanshi trace tui <run_id> --filter tool --details
```

---

## 8. 本手册的时效性

本手册中的 🔴/🟡 标记与"注意"条目均对应 `TODOs.md` W 段的具体条目；每收口
一个 W0/W1 条目，请回到本手册把对应告警移除或降级（建议在关闭 TODO 时顺手
更新，保持"文档不超前也不落后于代码"——这是 2026-08-09 评估中反复出现的
教训）。
