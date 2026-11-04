# Yanshi 运维手册

> 面向日常使用者的排查 + 优化速查手册。目标：出问题时知道先跑哪个命令、看哪个字段；想优化时知道有哪些旋钮可以调。
>
> 本手册只讲"怎么用现有工具排查/优化"，不重复架构设计——概念性内容见 [`AGENT_RUNTIME.md`](./AGENT_RUNTIME.md) / [`AGENT_SDK.md`](./AGENT_SDK.md) / [`HARNESS_MODE.md`](./HARNESS_MODE.md) / [`STABILITY.md`](./STABILITY.md)。
>
> 阅读顺序建议：遇到问题 → 先跑 [§1 快速自检](#1-快速自检-每次遇到问题前先跑) → 按症状去 [§2 故障排查](#2-故障排查任务清单) 对应小节 → 解决后如果想调优参考 [§3 性能与成本优化](#3-性能与成本优化任务清单)。[§4](#4-命令速查表)/[§5](#5-关键参考表) 是随查随用的速查表。

## 1. 快速自检（每次遇到问题前先跑）

```bash
yanshi doctor --format json --profile local
```

看 `status` 字段：`ok` / `warning` / `fail`（对应 exit code 0/1/2，可以直接用于脚本判断）。重点看这几个子字段：

- [ ] `config.missing_env_vars` — 非空说明有 provider 的 API key 没配，对应模型会直接失败
- [ ] `sandbox.enforcing` — 生产环境应为 `true`；`permissive`/`disabled` 在 `--profile production` 下会直接判 `fail`
- [ ] `environment.yanshi_run_dir` / `yanshi_trace_dir` — 确认 run/trace 目录是你预期的路径，不是意外落到 `~/.yanshi/{runs,traces}`
- [ ] `disk.*` 三项（`run_dir`/`trace_dir`/`marketplace_cache`）的 `exists`/`writable` — 目录存在但不可写是最容易被忽略的一类

如果怀疑是 server/DB 问题，加 `--server <url>`（探测 `/health`，3s 超时）；如果怀疑是磁盘/权限问题，加 `--backup-check`；如果怀疑是 Skill/Plugin/MCP 声明的可执行文件缺失，加 `--check-installations`。

## 2. 故障排查任务清单

### 2.1 Workflow（DAG）跑挂 / 失败 / 结果不对

1. **先验证定义本身没问题**，不要直接假设是运行时 bug：
   ```bash
   yanshi workflow validate <file> --format json --strict --explain-permissions
   ```
2. **已经跑过一次** → 看 trace，而不是猜：
   ```bash
   yanshi trace tui <run_id> --filter workflow --details
   # 或非交互：
   yanshi trace replay <run_id> --json
   ```
   trace 目录默认是 `YANSHI_TRACE_DIR` 或 `~/.yanshi/traces`，跟 doctor 里 `environment.yanshi_trace_dir` 对应。
3. **`--max-retries` 不是你以为的"节点级重试"** —— 它是整个 `Flow` 重跑 `max_retries + 1` 次，每次都受 `--timeout` 独立限制（`yanshi-cli/src/commands/workflow/run.rs`）。真正的节点级 `timeout_ms`/`max_retries` 目前**只有 `mcp` 节点类型**支持；其他节点想要"失败后重试当前步骤"要用 `while` 节点包一层循环（`condition` + `max_iterations` + `do:`）。排查思路：先分清是"整个流程要重跑"还是"某一个节点要重试"，用错级别会白调半天参数。
4. **中断后想续跑**，先看计划再动手：
   ```bash
   yanshi workflow resume-plan <run_id> --format json
   ```
   重点看每个 `tool_calls[]` 项的 `idempotency`（`idempotent`/`non_idempotent`/`unknown`）和 `decision`（`replay`/`skip`/`requires_manual`）——`requires_manual` 说明这一步不能自动续跑，需要人工确认后加 `--force-replay`。
5. **`--execution-mode concurrent` 下"卡住"但其实是假象**：`--max-concurrency` 设太大可能触发下游限流/资源竞争看起来像挂起；设太小则并发节点排队看起来像变慢。先切回 `--execution-mode serial` 复现一遍，能排除是不是并发调度的问题。
6. **`workflow dynamic` 在 CI/非交互环境里"卡住不动"**（T1.3 起）：`--profile`
   默认值是 `dev`（保留旧的无监督默认，不受影响）；一旦显式传
   `--profile local`/`production` 又不显式传 `--approve`，默认值变成 `cli`
   （交互式审批），会在等 stdin 输入——这是刻意的安全默认（LLM 生成的计划
   天然对抗性），不是 bug。CI/非交互场景要么显式传
   `--approve auto-allow`/`auto-deny`，要么继续用默认的 `--profile dev`。

### 2.2 Agent（ReAct）循环异常：不收敛 / 提前退出 / 结果被拒

先看 `AgentRunResult.stop_reason` 是哪个，对照 [§5.1 AgentStopReason 表](#51-agentstopreason-对照表) 找到该查什么。几个高频场景：

- **一直循环到 `MaxSteps`**：多半是 agent 没找到"可以给出最终答案"的路径——查 trace 里最后几步的 `Plan`/`ToolResult`，看是不是工具返回的内容本身有歧义或不完整；确认后再考虑调大 `max_iterations`/`--max-steps`，不要一上来就调大掩盖问题。
- **`TokenBudgetExceeded`**：说明 `RuntimeLimits::token_budget`（ReAct 默认 50 000）被打满。先看是不是 `MemorySummaryStrategy::Disabled`（默认值！）在无脑塞全量历史——开 `RecentOnly` 或 `Compact` 通常比单纯调大 budget 更治本，见 [§3.1](#31-tokencost-优化)。
- **`Verify` 步骤反复出现 `approved=false`，最后被强制放行**：说明挂了 `VerificationStrategy` 但候选答案一直不达标，`max_verification_attempts`（默认 2）耗尽后**会强制接受而不是报错**——这是设计上的优雅降级，但意味着"最终答案"不代表"verifier 认可"。排查时读 `Verify` 步骤里的 `feedback` 字段：如果 feedback 每次都合理但 agent 没改进，问题在 agent 侧（没把 feedback 当回事）；如果 feedback 本身就在无理由拒绝，问题在 verifier 策略太严。
- **`Cancelled` 但资源没释放干净**：`AgentCancellationToken` 是协作式取消，只中断了正在 `.await` 的 in-process Tokio future；已经 `tokio::spawn`/`spawn_blocking`/FFI 出去的工作**不会**被真的打断（见 `yanshi-agent-spi/src/runtime.rs`）。如果取消后还看到副作用继续发生，先确认是不是工具内部自己 spawn 了detached task。
- **`CostLimitExceeded`**（T1.1 起 ReAct/PlanExecute 生产运行时都会真正执行）：`RuntimeLimits::cost_limit_usd`（或 `ReActConfig`/`PlanExecuteConfig::cost_limit_usd`）设置后，运行时用 `pricing_table`（`yanshi-agents::eval::pricing::PricingTable`，同一套定价表，不是另一套）估算每次 LLM 调用的花费并累加；**默认 `pricing_table` 是全零价格表**，不配置真实单价，`cost_limit_usd` 设了也不会触发——先确认价格表配的对不对。ReAct 在下一轮 turn 顶部检查（和 `TokenBudgetExceeded` 一样有一轮延迟：真正超预算的是上一次调用，停止发生在下一次调用之前）；PlanExecute 只有一次 planner 调用，检查在该调用之后、执行计划之前。`yanshi eval run` 的 `dataset.toml::cost_limit_usd` 是独立的事后核算层（`aggregate_cost` 用自己的 `--pricing` 表重新计算并在报告里改判 `Failed`），两者可以同时生效，互不依赖。**U1.3**：`yanshi harness run`/`chat` 的 `--cost-limit-usd <f64>` flag 以及 `POST
  /v1/harness/sessions` 请求体的可选 `cost_limit_usd` 字段是这个运行时限制的 CLI/API 入口（此前只能用 Rust API `.with_cost_limit_usd(...)`）；两者都直接透传进 `RuntimeLimits`，语义和上面完全一致（同样受空定价表影响、同样有一轮检查延迟）。

### 2.3 Harness Session 排查（`yanshi harness run/chat`）

```bash
yanshi harness list --run-dir <dir>
yanshi harness inspect <session_id> --run-dir <dir>
yanshi harness replay <session_id> --filter-kind approval_requested --filter-kind approval_decided
```

run-dir 解析优先级（弄错目录是最常见的"查不到 session"原因）：`--run-dir` 显式指定 → `YANSHI_RUN_DIR` → `YANSHI_TRACE_DIR` → `~/.yanshi/runs`。session 文件实际落在 `<root>/harness/sessions/<session_id>.jsonl`。

- **卡在 `approval_requested` 没有对应 `approval_decided`**：先看 `--approve` 模式——`cli` 模式需要交互终端输入，非交互环境（CI、后台任务）用它会看起来"永远卡住"；`auto-deny` 模式下第一次 deny 会 `DenyAndStop` 直接终止后续所有工具调用，如果发现大量工具都没跑，先查是不是被第一个 deny 连坐了。
- **`memory_summary_added` 频繁出现**：说明 `--context-budget`/`--token-budget` 被打满、正在持续压缩上下文——如果压缩后信息丢失导致 agent "失忆"，考虑调大 budget 或用 `--no-default-context` 减少默认注入的 AGENTS.md/TODOs.md 等 provider 内容。
- **`background_task_updated` 一直是 `pending`/`running` 不 `completed`**：用 `task_get`/`task_list`/`task_output` 工具直接问那个后台任务，而不是干等 harness session 结束。

### 2.4 LLM Provider 问题

- `yanshi llm models --format json`：确认模型确实注册了、`vendor` 对不对。
- `yanshi doctor` 的 `config.missing_env_vars`：最常见的"provider 报错"根因就是这个字段非空。
- **`.yanshi/.env` 加载顺序**：进程环境变量优先于文件内容（dotenvy 默认行为）——如果你本地导出了一个旧的/错的 key，`.env` 文件里配对的新 key 不会生效，容易误以为改配置没生效。
- **Mock provider 排查陷阱**：`YANSHI_MOCK_RESPONSES`/`YANSHI_MOCK_TOOL_CALLS` 是进程级环境变量，测试/调试脚本忘记清理会导致"明明改了代码，行为却没变"——这类诡异现象先 `env | grep YANSHI_MOCK` 排除干扰。

### 2.5 MCP 工具问题

先脱离 agent 单独测通 MCP server，不要一上来就在完整 agent 循环里排查：

```bash
yanshi mcp list-tools <server_command...> --format json
yanshi mcp call-tool <server_command...> -t <tool> -p '<json_params>'
yanshi mcp config list --format json   # 确认 mcp.toml 来源解析对不对
```

如果是通过 Skill 声明的 MCP server，检查 `skill.toml [security]` 里的 `mcp_server_allowlist`/`mcp_command_allowlist`/`mcp_env_allowlist`（后两者默认白名单很窄：命令默认只允许 `python`/`python3`/`node`/`npx`/`uvx`，env 默认**一个都不转发**）——很多"MCP server 起不来"其实是被这层默认拒绝挡住了。

### 2.6 沙箱 / 权限被拒绝

- 看具体是哪个工具调用被拒：`AgentEvent::ToolCapabilityDecision` 里的 `denied[]`/`deny_reason`。
- **踩坑高发点**：`SandboxPolicy::allowed_paths`/`allowed_commands` 为空列表时语义是"全部拒绝"，不是"不限制"（这是有意为之的安全默认值，早期版本反过来过）；要放开必须显式配置列表或设 `allow_all_paths`/`allow_all_commands`。`allowed_domains` 则相反——空列表默认允许所有域名，是非对称设计，配置时容易搞混方向。
- 对照 [§5.3 SecurityProfile 差异表](#53-securityprofile-差异表) 确认当前 profile（`dev`/`local`/`production`）下的默认权限集是不是符合预期，很多"本地能跑、线上跑不了"就是 profile 差异导致的权限收紧。
- `yanshi doctor` 的 `sandbox.backend`/`sandbox.enforcement`（`enforcing`/`permissive`/`disabled`）——`permissive` 通常代表这台机器上沙箱二进制缺失或平台不支持，是配置问题而非代码问题。

### 2.7 Checkpoint / Resume 问题

同 [§2.1 第 4 点](#21-workflowdag-跑挂--失败--结果不对)：先 `resume-plan` 看计划,不要直接 rerun。`--checkpoint-dir` 默认 `~/.yanshi/checkpoints`。

### 2.8 Server / DB 问题（用了 `yanshi serve`）

```bash
yanshi doctor --server <url> --format json   # 探测 /health
yanshi serve --check                          # 就地读性检查，不绑定端口
```

清理/备份相关：`yanshi cleanup --dry-run` 先看会删什么再真删；`yanshi backup -o <dir> --dry-run` 同理。两者都支持 `--database-url`/`YANSHI_RUN_DIR`/`YANSHI_TRACE_DIR` 覆盖。

### 2.9 RAG 检索质量问题

先脱离 agent 单独测检索本身，不要怀疑到 agent 循环上：

```bash
yanshi rag ops search --qdrant-url <url> -c <collection> -q "<query>" --rerank
yanshi rag eval -d <dataset_dir> -r hybrid --compare-baseline <path>
```

`eval --compare-baseline` 会做配对符号检验（paired sign test），比单看 Recall/nDCG 数字更能判断"这次调整是不是真的有效还是噪声"。

## 3. 性能与成本优化任务清单

### 3.1 Token/成本 优化

- [ ] **默认是 `MemorySummaryStrategy::Disabled`**（全量历史塞进每次 prompt）——多轮对话/长任务优先切到 `RecentOnly`（简单、可预测）或 `Compact`（token 占用最省，但依赖摘要质量）。
- [ ] 分清两层 budget：`RuntimeLimits::token_budget`（硬限制，超了直接 `TokenBudgetExceeded` 停止）vs `ReActConfig::memory_prompt_token_budget`（软限制，配合 summary 策略压缩而不是停止）——只调硬限制只会让 agent 更早报错,不会让它更省。
- [ ] 配 `pricing.yml`（`YANSHI_PRICING_TABLE` 或 `~/.yanshi/pricing.yml`）跑 `yanshi eval run`，用真实 `input_per_1k`/`output_per_1k` 而不是拍脑袋估算模型选型的成本差异。
- [ ] 长期看用 `AgentEvent::LlmCallCompleted` 里的 `prompt_tokens`/`completion_tokens`/`duration_ms` 搭自己的仪表盘——注意这俩字段是 `Option`,`None` 代表"未知"不是 0,聚合时别当零处理。

### 3.2 并发 / 吞吐 优化

- [ ] DAG 层：`--execution-mode concurrent --max-concurrency N`，从小值开始逐步加，观察是否有下游限流；`fail_fast`/`continue_on_skip` 默认都是符合直觉的值，一般不用碰。
- [ ] Agent 层：≥2 个原生 tool_calls 会自动走批量分发——`Idempotent` 的并发跑,`NonIdempotent`/`Unknown` 的串行跑（H3）。想更快，优先把工具的 `idempotency()` 声明准确,而不是手动改并发参数。
- [ ] 长任务丢进 Harness 后台任务（`task_create` 等 5 个内置工具，H4），别让主循环阻塞等结果——`max_output_bytes` 默认 64 KiB,输出巨大的任务要自己分页取。

### 3.3 循环收敛质量优化

- [ ] `ReflectionStrategy` 和 `VerificationStrategy` 选型：只想要"事后记录一句话方便复盘"用 Reflection；想要"答案不够好就重来一轮"用 Verification——两者可以同时挂,Reflection 先记录、Verification 再决定是否真的停。
- [ ] `max_verification_attempts` 是"收敛性"和"死循环风险"的平衡点：默认 2 次,如果 verifier 判断稳定合理可以适当调高;如果调高后经常打满,大概率是 agent 侧没吸收 feedback,该改 prompt/persona 而不是继续加次数。
- [ ] `stop_conditions`（字符串子串匹配）便宜但容易误触发——设计时避免用太短/太常见的词,否则可能在中途正常文本里意外命中提前终止。

### 3.4 沙箱安全开销

- [ ] `os_sandbox` 打开会有实际性能代价（进程级隔离开销）,但生产 profile 下 `require_os_sandbox` 是 `true`——不要为了性能在生产环境关掉,应该反过来看是不是被沙箱的 `max_exec_time_secs`（默认 30s）/`max_file_read_bytes`（默认 10MB）限制卡住,按需调大这两个参数而不是关沙箱。
- [ ] `SandboxPolicy::permissive()` 只应该在明确知道自己在干什么的场景显式调用（比如本地一次性调试）,不要把它当默认配置抄进生产 skill。

### 3.5 Trace / 可观测性开销

- [ ] `YANSHI_TRACE_DIR` 会持续增长,定期 `yanshi cleanup`（可先 `--dry-run`）或 `yanshi backup` 后清理,别等磁盘写满了才发现。
- [ ] 排查性能问题时 trace 本身是免费的观测手段（不用额外插桩）,优先用 `yanshi trace tui --filter tool` 定位慢的具体是哪个工具调用,而不是猜。

## 4. 命令速查表

| 场景 | 命令 |
| --- | --- |
| 每日健康检查 | `yanshi doctor --format json --profile local` |
| 生产环境健康检查 | `yanshi doctor --format json --profile production --server <url> --backup-check --check-installations` |
| 验证 workflow 定义 | `yanshi workflow validate <file> --format json --strict --explain-permissions` |
| 干跑不执行 | `yanshi workflow run <file> --dry-run` |
| 并发执行 | `yanshi workflow run <file> --execution-mode concurrent --max-concurrency <N>` |
| 看某次运行的 trace | `yanshi trace tui <run_id> --filter workflow --details` |
| 断点续跑评估 | `yanshi workflow resume-plan <run_id> --format json` |
| LLM 生成计划再执行（沙箱工具） | `yanshi workflow dynamic --goal "<goal>" -m <model> --allow-path <p> --allow-domain <d> --dry-run` |
| 起一个 Harness 会话 | `yanshi harness run "<input>" --skill <path> --approve cli --output stream-json` |
| 列出 Harness 会话 | `yanshi harness list --run-dir <dir>` |
| 查看 Harness 会话事件 | `yanshi harness replay <session_id> --filter-kind approval_requested` |
| 恢复 Harness 会话 | `yanshi harness resume <session_id>` |
| 测试一个 MCP server | `yanshi mcp list-tools <cmd...> --format json` |
| 检查已注册模型 | `yanshi llm models --format json` |
| 校验 Skill | `yanshi skill validate <dir>` / `yanshi skill inspect <dir> --explain-permissions` |
| RAG 检索质量对比 | `yanshi rag eval -d <dataset> -r hybrid --compare-baseline <path>` |
| Trace/Run 目录清理 | `yanshi cleanup --dry-run` → 确认后去掉 `--dry-run` |
| 全量备份 | `yanshi backup -o <dir> --dry-run` → 确认后去掉 `--dry-run` |

## 5. 关键参考表

### 5.1 `AgentStopReason` 对照表

| 变体 | 含义 | 该查什么 |
| --- | --- | --- |
| `FinalAnswer` | 正常给出最终答案 | 无需处理（`is_success() == true`） |
| `StopCondition { condition }` | 命中了 `stop_conditions` 里的字符串 | 也算成功；确认命中的不是误触发 |
| `MaxSteps { max_steps }` | 达到步数上限（ReAct 默认 15） | 先看是不是没收敛，再考虑调大 |
| `MaxToolCalls { max_tool_calls }` | 达到工具调用次数上限（默认不限） | 检查是否有工具调用重试死循环 |
| `Timeout { timeout_ms }` | 达到墙钟超时 | 检查是不是某个工具/LLM 调用异常慢 |
| `Cancelled { message }` | 收到取消信号 | 确认是谁触发的；注意 detached 任务不会被真正打断 |
| `TokenBudgetExceeded { used, budget }` | 会话记忆 token 估算超预算（默认 50 000） | 开 `RecentOnly`/`Compact` 摘要策略，或调大 budget |
| `CostLimitExceeded { used_usd, budget_usd }` | 累计成本超限（T1.1 起 ReAct/PlanExecute 生产 runtime 都会真正熔断，默认 `pricing_table` 全零价格不生效） | 检查 `RuntimeLimits::cost_limit_usd` + 是否配了非零 `pricing_table`；eval 场景另看 `dataset.toml` 的 `cost_limit_usd` |
| `Error { message }` | 未分类的运行时错误 | 直接读 `message` |

### 5.2 `HarnessEvent` kind 对照表

| kind | 含义 | 排查用途 |
| --- | --- | --- |
| `session_started` | 会话启动，context provider 已解析 | 看 `context_token_estimate` 判断上下文是否过大 |
| `step_started` | 新的 agent 步骤开始 | 定位卡顿从哪一步开始 |
| `tool_call_requested` | 请求调用某工具（尚未决定是否放行） | 看 `idempotency`/`permissions` |
| `approval_requested` | 需要人工/策略审批 | `--approve cli` 下确认没有卡在等待交互输入 |
| `approval_decided` | 审批结果已产生 | 配合上一项确认审批闭环 |
| `tool_call_completed` | 工具调用结束 | 看 `is_error`/`duration_ms` 定位慢/失败的工具 |
| `background_task_updated` | 后台任务状态变化 | 跟踪长任务的 pending/running/completed/failed |
| `memory_summary_added` | 上下文被压缩 | 出现频繁说明 budget 经常打满 |
| `stopped` | 会话终止 | 看 harness 层的 5 变体 `StopReason`（区别于上面 9 变体的 `AgentStopReason`，不要混淆） |

### 5.3 `SecurityProfile` 差异表（`dev` / `local` / `production`）

| 字段 | dev | local（默认） | production |
| --- | --- | --- | --- |
| 要求 API token | 否 | 否 | **是** |
| 允许无认证的 loopback 访问 | 是 | 是 | **否** |
| 默认工具权限集 | 全部 6 项 | 全部 6 项 | 仅 `FilesystemRead` + `Workflow` |
| 要求 OS 沙箱 enforcing | 否 | 否 | **是** |
| 允许 noop 沙箱后端 | 是 | 是 | **否** |
| 允许 subprocess 插件 | 是 | 是 | **否** |
| 市场包要求签名验证 | 否 | **是** | 是 |

### 5.4 常用环境变量速查

| 变量 | 作用 |
| --- | --- |
| `YANSHI_RUN_DIR` | Workflow/Harness 运行产物根目录 |
| `YANSHI_TRACE_DIR` | Trace 目录；也是 Harness run-dir 解析的第 3 优先级兜底 |
| `YANSHI_API_TOKEN` | Server 认证 Bearer token |
| `YANSHI_SECURITY_PROFILE` | `dev`/`local`/`production` |
| `YANSHI_MODELS_CONFIG` | 覆盖 `models.yml` 路径 |
| `YANSHI_MCP_CONFIG` | 覆盖 `mcp.toml` 路径 |
| `YANSHI_PRICING_TABLE` | 成本表路径（eval、Agent 预算、DAG `cost_limit_usd` 共用） |
| `YANSHI_MOCK_RESPONSES` / `YANSHI_MOCK_TOOL_CALLS` | Mock provider 排队响应（**调试/CI 环境记得清理，否则会污染后续运行**） |

## 6. 相关文档索引

- [`AGENT_RUNTIME.md`](./AGENT_RUNTIME.md) — Agent 运行时边界、ReAct 循环、与 DAG 的混合组合
- [`AGENT_SDK.md`](./AGENT_SDK.md) — 扩展点契约（`AgentRuntime`/`ReflectionStrategy`/`VerificationStrategy`/`MemorySummaryBackend`/`Tool`/`MemoryStore`）
- [`HARNESS_MODE.md`](./HARNESS_MODE.md) — Harness Mode 完整实现规格
- [`STABILITY.md`](./STABILITY.md) — 各表面的稳定性等级与 wire-shape 承诺
- [`ARCHITECTURE.md`](./ARCHITECTURE.md) — 四种执行范式与三轴心智模型
- [`ARCHITECTURE_DIAGRAM.md`](./ARCHITECTURE_DIAGRAM.md) — 分层架构图与模块职责（中文）
- [`TOOL_PERMISSIONS.md`](./TOOL_PERMISSIONS.md) — 工具/Skill/CLI 三方权限合并规则
- [`TRACING_DESIGN.md`](./TRACING_DESIGN.md) — `AgentEvent` 持久化与 OTel span 生成
