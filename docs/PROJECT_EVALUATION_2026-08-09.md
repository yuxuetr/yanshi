# Yanshi 项目完善度综合评估报告

**评估日期**：2026-08-09
**评估对象**：Yanshi workspace（24 个 Rust crate + 1 个 React SPA，`src/` 合计约 172,600 行，含测试/示例/基准约 224,100 行）
**评估方法**：六路并行源码级审计（DAG 引擎层 / Agent 运行时层 / Harness 治理与 Loop Engineering / 能力基座 / 平台运维层 / 工程质量横切），全部结论以 `文件:行号` 为证据，不采信 README / CLAUDE.md / CHANGELOG 的自述；关键高影响结论由独立抽查二次核验。本报告与 `docs/PROJECT_EVALUATION_2026-08-05.md` 构成延续关系，但证据独立采集。

---

## 1. 执行摘要

### 1.1 一句话结论

> **Yanshi 是一个"契约与机制达到 A 级、接线与产品化停留在 B/C 级"的智能体框架**：四执行范式（静态 DAG / Agent 原生循环 / 动态工作流 / Harness 治理）在类型契约与核心机制层面全部真实落地且经过对抗性测试，工程纪律（生产代码零 `unwrap`、零 `todo!`、17-job CI 门禁、审计闭环留痕）显著高于同类开源项目；但存在一组系统性的"**已建成未接线**"缺口——服务端 Harness 空工具注册表、分布式调度器无 HTTP 入口、动态工作流 replan 无用户入口、约 2,340 行"生产原语"零调用者、`observability` feature 全 workspace 未开启——使得框架当前是 **single-node / 库级可用，cluster / 产品级未达**。

### 1.2 综合评级

| 维度 | 评级 | 简评 |
|---|:---:|---|
| **架构设计**（契约内核 + 分层） | **A−** | 五层 + 窄腰契约内核真实落地，IR/executor 分离、runtime 互不依赖均经源码验证；但 8 条依赖律仅 3 条被 `xtask check-arch` 强制执行 |
| **DAG 工作流引擎** | **B** | 调度器（确定性拓扑、依赖就绪派发、checkpoint 三态保真）货真价实；Map/While 子流程隔离过度、retry 包装破坏部分恢复契约、resume 退化串行 |
| **Agent 原生运行时** | **B+** | ReAct 循环是全仓工程质量最高的部分（五类限额、7 处取消点、并行批量派发、HITL、loop checkpoint 全真实）；策略扩展点偏骨架、supervisor 层落后 |
| **动态工作流** | **B** | `compile_plan_to_flow` + 失败驱动 replan（含已完成节点复用）真实实现且有测试；但表达力限于无环无条件 DAG，replan 无 CLI 入口 |
| **Harness 治理** | **B** | 契约（H0）A 级、机制（H1–H4）B+；生产入口空工具注册表 + `DenyAndStop` 不真停 + 审批缓存粒度过粗，缺一次带真实工具的端到端 dogfooding |
| **Loop Engineering** | **B+** | 双层循环防御（steering nudge + 滑窗签名 `LoopDetected`）、上下文预算 tokenizer 精算 + truncate-not-drop、TaskSummary/项目记忆均真实；DAG 侧 While 与 Agent 侧循环工程割裂 |
| **能力基座**（LLM/Tool/MCP/Memory/RAG） | **B+ ~ A−** | 沙箱层（seccomp+Landlock+cgroup / ContainerBackend）是全仓工程顶点；LLM 层差一个已自我诊断的 SSE bug 就是 A；MCP 停在 2024-11-05 协议版本是明确短板 |
| **平台与运维层** | **B** | 鉴权/准入/错误信封/CI 达中上水平；四个"最后一公里"未接线（工具分发契约、分布式入口、跨副本状态、OTLP 出口） |
| **工程流程**（CI/测试/文档） | **A−** | 3,075 个测试、17-job release gate、自定义治理 lint、审计闭环可验证；无覆盖率门禁、依赖版本漂移、根目录文档失控 |
| **生产就绪度** | **C+** | 单机 CLI-first 路径可用；服务端多副本、分布式、marketplace 服务化均未达产品级 |
| **综合** | **B+** | — |

### 1.3 关键数字

- 24 个 Rust crate，`src/` 172,595 行；`#[test]`/`#[tokio::test]` 合计 **3,075** 个；`docs/` 约 80 份活跃文档 + audit/archive 子目录。
- 生产库代码 `unwrap()/expect()` 实测**接近零**（CI `clippy --lib -D clippy::unwrap_used -D clippy::expect_used` 强制，`quality.yml:133`）；全 workspace `todo!()`/`unimplemented!()` 为 **0**。
- LLM：6 个专用 provider + 4 个 OpenAI 兼容复用，注册表 198 个模型；原生 `tool_calls`/`tool_choice` 四种线格式全覆盖。
- 已识别高严重度缺陷 **9 项**、中严重度约 **20 项**、文档-代码漂移 **15+ 处**（见 §5、§6）。

---

## 2. 架构评估

### 2.1 五层架构与窄腰契约内核：真实落地，非纸面架构

项目宣称的五层模型（L0 契约内核 → L1 执行核 → L2 能力适配 → L3 Agent 编排 → L4 平台运维）经源码验证**基本属实**：

- **IR 与执行器分离是真实的**：`yanshi-graph` 定义 `Flow`/`GraphNode`/`NodeType`/`AsyncNode`（`flow.rs:21-62`），`Flow` 私有字段 + 只读访问器，`yanshi-core` 通过 `FlowExt` trait（`flow.rs:1701-1825`）作为唯一执行面。跨 crate 只能读 IR，不能改——这不是文档口号。
- **Runtime 互不依赖、只依赖契约**：`yanshi-harness` 依赖 `yanshi-agent-spi` 与 `yanshi-graph`（IR），`yanshi-core`（executor）仅为 dev-dependency，执行器经注入的 `FlowRunner` 到达——依赖倒置正确。
- **`yanshi-tool` / `yanshi-store-spi` / `yanshi-agent-spi` 是真正的零依赖（或近零依赖）L0 crate**，且 `yanshi-tool` 的 `Cargo.toml` 对"为何不 dev-depend `yanshi-tools`"（循环 dev-dep 双重编译）有正确处理与说明。

**架构瑕疵（按严重度）**：

1. **8 条依赖律仅 3 条激活**。`xtask/src/main.rs:4093` 明确注释 "Three laws are active today"：`runtime-isolation`、`surface-isolation`、`kernel-isolation`。其余 5 条以 `ARCH_LATENT_EDGES` 形式记录了 7+ 条已知目标态违规边（`agents→llm`、`skills→agents`、`memory→rag`、`mcp→tracing` 等）。门禁机制本身设计优秀（allowlist 只能收缩 + staleness 检查 + latent misfiled 检测），但 CLAUDE.md 表述的"eight dependency laws, enforced by `cargo xtask check-arch`"实际强制覆盖率为 **37.5%**。
2. **L0 纯度瑕疵**：`yanshi-graph`（宣称 L0 内核）依赖 `dirs` 与 `tokio`，仅因 `CheckpointConfig::default()` 读 home 目录与一个 `From<tokio::time::error::Elapsed>` 转换。
3. **能力层横向耦合**：`yanshi-memory → yanshi-rag`（为 `EmbeddingProvider`）违反自定的"capability crates stay off each other"原则（`rewrite` 模块文档自述该原则）。
4. **死代码入公共 API**：`yanshi-graph/src/node.rs` 的同步 `Node` trait（108 行，零实现者）仍被 `yanshi-core/src/lib.rs:71` re-export；`yanshi-agents/src/common/`（431 行，为已移出 workspace 的 `agents/paper_*` 子 crate 服务）仍 `pub use` 到公共 API。

### 2.2 四执行范式：模型与代码的符合度

README/ARCHITECTURE 宣称的"单一规划/绑定时机谱系上的四个点"在代码中的真实覆盖：

| 范式 | 代码载体 | 真实度 | 关键差距 |
|---|---|---|---|
| 静态 DAG | `yanshi-core::Flow` + `FlowExt` | ✅ 完整 | Map/While 子流程丢失监听/取消/目录（§3.2） |
| Agent 原生循环 | `yanshi-agents::ReActAgent` / `PlanExecuteAgent` | ✅ 完整且最强 | 策略扩展点仅骨架（§3.4） |
| 动态工作流 | `yanshi_agents::dynamic::{compile_plan_to_flow, DynamicWorkflowAgent}` | ✅ 真实 | 无条件/循环节点；replan 无入口（§4.3） |
| Harness 治理 | `yanshi-harness::HarnessRuntime` + `wrap_registry` | ⚠️ 机制真实、生产接线空心 | 服务端/CLI 无 skill 路径注册表为空（§4.4） |

递归组合的两个关键件——`AgentNode`（agent 进 DAG，837 行，含四态 `AgentNodeResumeContract` 部分恢复契约）与 `WorkflowTool`（DAG 作为 agent 工具，经注入 `FlowRunner` 执行）——**均为真实实现并有独立集成测试**（`tests/agent_node_resume_contract.rs` 265 行）。这是该架构最有辨识度的资产：组合方向是双向的且都能落 checkpoint。

---

## 3. 逐层模块评估

### 3.1 L0 契约内核（value / graph / async-util / tool / store-spi / agent-spi）

| Crate | 行数(src) | 评级 | 要点 |
|---|---:|:---:|---|
| `yanshi-value` | 570 | **A** | 手写 tagged 序列化 + legacy 兼容 + proptest round-trip，全仓最扎实的一层 |
| `yanshi-graph` | 2,709 | **B** | IR 完备、错误模型 30 变体；但 1,051 行 `expr` 仅 7 个测试且 eval 递归无界（左深链可栈溢出，`MAX_EXPR_DEPTH=64` 只挡解析递归） |
| `yanshi-async-util` | 1,049 | **B−** | `retry`/`race_with_limits` 真实被消费；`TimeoutConfig` 整套（~150 行）**零消费者** |
| `yanshi-tool` | 3,025 | **A** | `Tool`/`ToolRegistry`/`Capability` 三层交集合并带审计轨迹/`SecurityProfile` 三档/`ToolIdempotency` 按参数动态判定；取消语义文档教科书级 |
| `yanshi-store-spi` | 887 | **A−** | 5 个 trait 被 6+ 实现消费；`EntityFactStore`/`SemanticMemoryStore` 未收敛进来（契约拆分不彻底） |
| `yanshi-agent-spi` | 4,159 | **A−** | `AgentRuntime`/Step/Event/StopReason 模型完整、封闭枚举策略经过思考、取消语义有正反双向 pin 测试；但 `max_steps` rustdoc 与两个 runtime 的实现三种语义互相矛盾（G1） |

### 3.2 L1 DAG 执行引擎（yanshi-core，11,378 行，评级 B）

**真实且高质量的部分**：
- Kahn 拓扑排序刻意确定化（`BTreeMap` + 双重 sort，服务 trace replay 可复现，`flow.rs:1556-1624`，有专项测试）。
- Concurrent 调度：`FuturesUnordered` 依赖就绪派发 + `max_concurrency` 在途上限 + 死锁硬报护栏（`flow.rs:803-1036`）。注意是单 task 协作式并发而非 `tokio::spawn` 跨线程并行；就绪扫描 O(N²)。
- Checkpoint 三态保真（`Ok` / `NodeSkipped` 哨兵 / `NodePartialExecutionFailed` 带 `partial_outputs`）+ 原子写（tmp→rename）+ 路径分量注入防护，`tests/checkpoint_recovery_test.rs` 794 行覆盖。
- `resume.rs` 的 `ResumeIdempotency × ResumeDecision` 决策矩阵设计完整。

**高严重度缺陷**：

1. **`observability` feature 全 workspace 无人开启**（`yanshi-core/Cargo.toml:22` 定义，13 个依赖方 Cargo.toml 均未启用；`yanshi-llm` 甚至把它改成了空别名）。后果：checkpoint 保存失败告警、corrupt-value 降级告警、while 循环诊断、retry 日志在**所有实际构建中被编译掉**。CLAUDE.md N8 声称的 "tagged-but-corrupt payloads warn loudly" 实际是静默降级。
2. **Map/While 子流程隔离过度**（`flow.rs:1212, 1288, 1344`）：子 Flow 用裸 `Flow::new` + 默认 config 执行，丢失 event_listener（子节点对 trace/UI/SSE 不可见）、cancellation token（不可取消）、run_base_dir（每个 map 项在 `~/.yanshi/runs/` 新建 UUID 目录）、并强制串行。
3. **retry 包装破坏错误分类**（`retry_executor.rs:63-78`）：不可重试错误也被包成 `RetryExhausted`，导致配了 `max_retries` 的 `agent`/`skill_agent` 节点的 `NodePartialExecutionFailed.partial_outputs` 无法进入 checkpoint——**部分恢复契约静默失效**。
4. **输入优先级反直觉**（`flow.rs:415-420`）：工作流级 `inputs:` 覆盖全图所有节点的同名参数，YAML `parameters` 又静默覆盖 `input_mapping` 的动态值。
5. **run_if 求值失败整体中止运行**（`flow.rs:385, 869`）：不发 `WorkflowFailed`、不写终态 checkpoint（状态卡 `Running`）、并发路径丢弃在途 futures；且 Serial（默认模式）下 run_if 跳过**不发 `NodeSkipped` 事件**，与 Concurrent 路径行为不一致。
6. **约 2,340 行"生产原语"零接线**：`resource_manager`(478) / `resource_limits`(411) / `concurrency`(610) / `state_monitor`(772) / `health`(79) 在全 workspace 无任何生产调用者（仅 `lib.rs` re-export）。CLAUDE.md "✅ 资源管理、K8s 健康检查（Phase 1.5 生产就绪）"属文档超前于代码。
7. Resume 永远退化为 Serial（`flow.rs:301-304`，被测试固化为特性）；checkpoint `latest` 用 `fs::copy` 非原子；模块文档声称 "file locking" 但无锁；`compression`/`auto_cleanup` 配置字段从未被读取。

**节点层**（`yanshi-nodes` B− / `yanshi-nodes-ai` C+）：Template/File/Http 三个默认节点经 `yanshi-tools` 继承真实沙箱；但 `HttpNode` 伪造 200/500 状态码、`ArxivNode` 绕过沙箱且无解压上限（gzip 炸弹面）+ 明文 HTTP、`MarkMapNode` 开箱不可用、`conditional`/`batch` 未注册进 YAML factory；`LlmNode` 是零字段薄壳（不支持 tool-calling/流式/多模态，每次 execute 重跑 `Yanshi::init()`）；`MCPNode` 每次执行起停一个 MCP 子进程。工作流侧并存三套模板语义（Tera / `expr` / 朴素 `{{key}}` 替换）。

### 3.3 L2 能力基座

#### yanshi-llm（15,207 行，A−）
- 流式为真 SSE 逐行解析 + token 级 `ToolCallDelta` 分片拼接；Anthropic 结构化输出用合成工具调用模拟（成熟的降级设计）；`tests/provider_matrix_doc.rs` 用穷尽解构做编译期文档漂移检测。
- **P0 缺陷：SSE buffer 残留丢弃 bug 只修了 1/5 provider**——StepFun 的修复注释（`stepfun.rs:525-533`）逐字描述了 OpenAI/Anthropic/Google/Moonshot 四家仍存在的同一 bug：单次网络读同时携带末条内容与 `[DONE]` 时，末尾内容被静默丢弃且 `is_final` 不置位。
- Google/Moonshot/StepFun 流式路径 `tool_call_deltas` 硬编码为空——流式+工具调用组合静默丢工具调用。
- **`RateLimitConfig`（RPM/TPM）纯装饰**：9 个厂商配好数值，全代码库无任何令牌桶消费点；重试无 jitter、不读 `Retry-After`。
- 模型注册表 198 个模型但严重倾斜（dashscope 46%、OpenAI 仅 6 个），缺 2026 主力模型。

#### yanshi-mcp（7,094 行，B）
- stdio transport 是**生产级**（有界读取 10 MiB、oneshot 多路复用、注册先于写入防竞态、环境变量白名单沙箱、stderr 排空、kill_on_drop、有界通知队列）。
- **协议版本停在 `2024-11-05`（首个公开版本，落后官方 spec 约两年）**；无 Streamable HTTP；`TransportType::Http/HttpWithSSE` 是枚举变体而非实现（`http` feature 为空）。
- Client 覆盖 10 方法（含真实的 resources/prompts）；但**无能力协商门控**（`supports_resources()` 等零调用点）、**无协议版本校验**、**无重连**（grep 零命中）。
- Server 脚手架仅 4 方法（initialize/initialized/tools/list/tools/call），示例 handler 是自认的 `// Placeholder implementation`；`call_tool` 是同步 `fn`。

#### yanshi-tool + yanshi-tools（8,361 行，A / 沙箱部分 A+）
- **Linux 沙箱是真实内核级过滤**：seccomp-bpf 在 `pre_exec` 中 `execve` 前生效，含参数级 `openat` flag 过滤、`clone3` 防绕过、`openat2` 整体拒绝防解引用绕过；Landlock ABI 运行时探测；cgroup v2 memory/pids 上限含 systemd 用户会话委派处理；`pre_exec` 的 async-signal-safety 分析注释教科书级。**残留缺口：blocklist 模型下 `io_uring` 未拦截**（经典逃逸路径，未在模块文档披露）。
- macOS `sandbox-exec` 用 `(deny default)` 默认拒绝模型，比 Linux 侧更强。
- **`ContainerBackend`（code_exec）是全仓最诚实的安全模块**：无降级层（无引擎硬失败而非降级）、`--network none` + `--read-only` + 非 root uid + 资源上限逐条实测留痕（含"root 被内核豁免 `RLIMIT_NPROC`"的实测记录）。
- `HttpTool` SSRF 防护链完整（DNS 解析后逐 IP 分类 + `resolve_to_addrs` 钉死防 DNS 重绑定 + 逐跳重定向重校验 + IPv4-mapped IPv6 规范化）；残留：响应体全量下载后才截断、按字节切 UTF-8 多字节边界会 panic（`http.rs:326`）。

#### yanshi-memory（4,136 行，B+）
- 四层模型（Session/Semantic/Preference/EntityFacts）trait 分离得当；age X25519 真实加密装饰器；`RememberPreferenceTool` 闭环。
- `SemanticMemory::search` 全量向量加载 + 暴力余弦 + N+1 回捞，无 ANN——长会话不可扩展；默认 `RetentionPolicy` 无任何自动遗忘；L3 项目记忆是可用库能力但 skill manifest / CLI 端到端未打通（`project.rs:14-21` 已诚实标注）。

#### yanshi-rag（14,076 行，B+）
- 6 种分块真实 + `SemanticChunker` 算法真实（但工厂函数错误文案 "not yet implemented" 误导）；ONNX 本地嵌入**真实推理**（非 mock）；Qdrant 集成完整；BM25 教科书实现（BM25+ 变体）。
- **Eval harness 是本 crate 最强部分**：Recall@K/MRR/nDCG + 配对符号检验的**统计正确**双门槛 A/B 门禁（增益阈值 ∧ p-value 阈值），可直接支撑配置变更回归。
- 缺陷：`VectorStore::hybrid_search` 默认实现静默忽略 `alpha` 退化为纯语义（QdrantStore 不 override）；`MMRReRanking` 用 Jaccard 词集相似度冒充嵌入余弦且无文档标注、`query` 参数被丢弃；BM25 无 CJK 分词/停用词/词干——**对中文语料检索质量会显著劣化**；14k 行 src 仅 455 行集成测试。

### 3.4 L3 Agent 编排层

#### yanshi-agents（23,066 行，B+）——全仓工程质量最高的运行时
- **ReAct 循环**：`LoopState` 抽象干净（turn-driven 改造的关键）；五类 `RuntimeLimits`（max_steps/max_tool_calls/timeout/token_budget/cost_limit_usd）全部真实执行；取消在 7 个 await 点全覆盖并有 4 个"飞行中取消"专项测试。
- **并行批量工具派发（H3）**：严格 6 阶段——`max_tool_calls` 原子前置检查 → 按 LLM 返回序预发全部 trace 事件（与完成顺序解耦，保 replay 确定性）→ `Idempotent` 并发 / 非幂等串行 → 按 LLM 序回填结果；`outputs[i]` 缺失降级不 panic。
- **V2 三件套均为真实实现**：结构化输出（`output_schema` → 合成 `final_answer` 工具走 provider 原生 tool calling + prompt 回退双通道 + 校验失败软恢复预算）；HITL `ask_user`（拦截 → `InterruptRequested` → 即时 checkpoint → `AwaitingInput` 停机 → `resume_from_loop_checkpoint` 配平 ToolCall/ToolResult，且有"与对照运行比对同一答案"的强断言测试）；agent-loop checkpoint（schema_version + runtime_kind 双防误用、每回合原子保存、`should_clear_checkpoint` 对 11 个 StopReason 穷举）。
- **弱点**：`react/agent.rs` 单文件 8,060 行远超可维护阈值；三个策略扩展点（`ReflectionStrategy` 全是模板串 / `VerificationStrategy` 唯一内置实现是 no-op 的 `AlwaysApprove` / `MemorySummaryBackend` 无 LLM 版）是"骨架+示例"而非能力，CLAUDE.md 将其当特性宣称；live sink 覆盖不全（`ToolPolicyDecision`/`LlmCallCompleted` 等未走实时发射，Harness 实时流缺策略决策事件）。
- **Supervisor 层（Handoff/Blackboard/Debate）明显落后于单 agent 层**：三者构造子上下文时丢弃 `event_sink`/`between_turn_hook`/`loop_checkpointer`/`trace_context`（Harness 看不到子 agent 实时事件、OTel trace 在多智能体边界断链）；只接受具体类型 `ReActAgent` 无法嵌套；`DelegationSpec`（L5 委派契约，本身实现完整含 schema 校验 + 冲突仲裁聚合）未接入任何 supervisor（模块注释自承）。

#### yanshi-skills（9,282 行，B+）
- 安全深度超出同类：脚本 SHA256 完整性（篡改全 profile 拒绝）、Python requirements 强制 pinned+hashed、knowledge 路径穿越拒绝、MCP 命令+env 双 allowlist、per-tool `os_sandbox` 覆盖、六层工具准入优先级显式建模（`skill inspect --explain-permissions` 可观测）。
- 知识分层（P-A4.2）真实：`files` 层内联 persona、`rag` 层索引进 `Bm25KnowledgeBackend` + 共享 `rag_search` 工具按需检索。
- MCP 适配完整（`McpClientPool` 懒连接 + 限流 + 幂等性推断 + 参数预校验 + 多模态内容转换），有真实 Python mock server 集成测试。
- 弱点：`SkillCapability::lower()` 产物过粗（全部工具 + 单个 Critical persona 片段）；`builder.rs` 2,600 行单文件三条装配路径有重复；仅 1 个 `.rs` example。

#### yanshi-harness（7,934 行，B）——详见 §4.4

#### yanshi-config（4,837 行，B−）与 yanshi-cli（22,441 行，A−）
- Schema 校验（ParamSpec 矩阵 + `expr` 静态编译 + plugin manifest 交叉校验）与 doctor 诊断（1,402 行、11 个 section、真实探测含容器引擎 spawn 检查）扎实；安全默认到位（`file`/`shell` 强制 allowlist）。
- factory 21 个 type 分支仅 `file` 一个被测；`max_retries → max_attempts` 语义错位（三处计数口径不一致）。
- CLI 21 个顶层命令 / 547 个测试（全仓最多）；`CliJsonEnvelope` 是 Stable 契约但迁移未完成（legacy bare body 与 envelope 并存）；`workflow dynamic` 是全仓设计最谨慎的命令（受限 sandbox + `--dry-run` + `--approve` 共享同一被治理 registry 防绕过）。

### 3.5 L4 平台运维层

| Crate | 评级 | 关键事实 |
|---|:---:|---|
| `yanshi-server` (14,689 行) | **B+** | 鉴权 fail-closed 完备（非 loopback 无 token 拒绝启动、常数时间比较、租户越权修复闭环）；**但 `POST /v1/skills/{name}:run` 是必失败的 stub**（写 `@skill:<name>` 后交给 YAML 解析器，源码自认 follow-up）、Harness 会话空工具注册表、网关执行硬编码 `serial()`、SSE broker / 准入 / 审批 / 取消四类状态全部进程内（多副本即失效） |
| `yanshi-db` (1,844 行) | **B−** | 10 张表（CLAUDE.md 写"九表"已过时）、索引合理、双池只读副本；16 个集成测试无 PG 时**静默通过**而非 `#[ignore]`；`events` 表无分区/归档 |
| `yanshi-worker`(+proto) | **B / C+** | gRPC 协议 + mTLS 双向证书 e2e + JWT 准入 + traceparent 四入口缝合均真实；**`agent` payload 跑在空 `ToolRegistry` 上**（工具分发契约未定，与服务端 Harness 同一缺口）；worker-proto 自身 0 测试 |
| `yanshi-tracing` (5,806 行) | **C+** | redaction 模块质量极高；**但存储后端只有 `FileTraceStorage` 一个实现**——SQLite/Postgres 仅 DDL 字符串常量，`postgres` feature 是死 flag，与 CLAUDE.md "JSONL or SQLite/Postgres (feature-gated)" 直接不符；无 OTLP 传输（span 模型/缝合都实现了，只差最后一公里） |
| `yanshi-ui` (5,549 行) | **C** | 真实可用的运维控制台（8 条路由、SSE、审批卡片、run 对比、zod 校验）而非空壳；但**单测在 CI 中从不执行**（`npm test` = `tsc --noEmit`，无 test runner）、e2e 非门禁仅 6 用例、手写字符串路由 |
| `xtask` (4,848 行) | **B+** | 10 个真实门禁任务、check-arch 机制 A 级但仅执行 3/8 律、`check-changelog`/`test-gate` 未接 CI |

**CI（A− 级流程）**：17-job release gate（fmt/clippy 全量 `-D warnings`/lib 级 unwrap deny/cargo-audit/redaction-lint/println-lint/check-arch/24-crate 测试矩阵含 PG service/24 组 feature 组合/examples 编译+运行/RAG 与 agent eval 冒烟）；CI 注释诚实记录 R0.1 事故复盘。缺口：无覆盖率、无 MSRV/cargo-deny、`release.yml` 不依赖 release-gate（tag 可绕过质量门发布）、无 `[workspace.dependencies]`（tokio 两种版本约束 + 12 种 feature 组合、sqlx 双 TLS runtime 并存）。

---

## 4. 智能体框架生态完成度专章（核心）

本章围绕用户最关心的问题展开：**Yanshi 作为"上层原生智能体框架"，在 DAG、动态工作流、Harness、Loop Engineering 四大范式上的切合度与完成度**。

### 4.1 静态 DAG：完成度约 85%，是最成熟的范式

**已达成**：确定性拓扑（trace replay 可复现）、依赖就绪并发调度、`run_if` 条件、`input_mapping` 显式数据流、Map（顺序/并行 + Semaphore 限流）、While（表达式条件 + max_iterations 硬保护）、三态 checkpoint 保真恢复、YAML config-first 与代码 code-first 双入口、`yanshi workflow debug`（validate/visualize/analyze/dry-run）。

**与原生智能体框架的差距**：
- **子图不是一等公民**：Map/While 子流程以裸 `Flow::new` 执行，事件、取消、run 目录、并发模式全部丢失——对比 LangGraph 的 subgraph 会继承 checkpointer/stream/interrupt，这是 Yanshi 静态 DAG 与"原生框架"最大的语义差距。
- **无动态边/条件路由原语**：`run_if` 只能跳过节点，不能选择分支目标；没有 conditional edge / router 概念，复杂分支要靠多节点 `run_if` 组合表达。
- **While 循环的可观测性为零**：迭代耗尽静默退出（与条件正常转 false 不可区分）、子流程失败被吞、无 `iterations_used` 输出、无循环进度事件——DAG 侧循环与 Agent 侧循环工程成熟度断层明显。
- 事件时间戳用 `Instant`（不可序列化）、`emit_event` 同步内联调用 listener（慢 listener 阻塞调度）。

### 4.2 Agent 原生循环：完成度约 80%，机制深度对标业界第一梯队

以 2026 年主流 agent 框架的循环能力清单对照：

| 能力 | 业界参照 | Yanshi 现状 | 判定 |
|---|---|---|---|
| 原生 tool_calls 循环 | 所有框架 | ✅ 4 种 provider 线格式 + 文本协议回退 | 齐平 |
| 并行工具调用 | Claude Code / OpenAI | ✅ 幂等并发 + 非幂等串行 + trace 序确定 | **领先**（幂等性感知的分区派发少见） |
| 结构化输出 | OpenAI structured outputs | ✅ 合成工具 + prompt 双通道 + 软恢复预算 | 齐平 |
| HITL 中断/恢复 | LangGraph interrupt | ✅ `ask_user` → checkpoint → resume 配平 | 齐平 |
| 循环 checkpoint | LangGraph checkpointer | ✅ 每回合原子保存 + 穷举清理策略 | 齐平 |
| 成本/token 预算 | 少数框架 | ✅ 五类限额全真实执行 + PricingTable 累计成本 | **领先** |
| 循环检测 | 少见 | ✅ 双层（steering nudge + 滑窗签名） | **领先** |
| 反思/自我验证 | Reflexion 类 | ⚠️ trait 齐备但内置实现全是 no-op/模板串 | **落后**（骨架无实质） |
| 多智能体 | AutoGen / Swarm | ⚠️ 三种 supervisor 真实但丢子上下文、不能嵌套、未接委派契约 | 半成 |
| 流式 token 输出 | 所有框架 | ✅ `TokenDelta` live-only 事件（V2.2） | 齐平（但受 §3.3 SSE bug 影响 4/5 provider 可靠性） |

**结论**：单 agent 循环的"工程含金量"（限额、取消、trace 确定性、恢复语义）达到甚至超过主流框架；差距集中在**认知策略层**（反思/验证只有插槽没有内容物）与**多 agent 编排层**（上下文传播断链）。

### 4.3 动态工作流：完成度约 60%，方向正确、表达力与入口受限

动态工作流（agent 生成计划 → 编译为 `Flow` → 确定性执行）是 Yanshi 最具差异化的设计——"一次规划决策 + 可回放并行 DAG"确实同时拿到了 agent 的灵活性与 workflow 的可靠性。源码核验：

**真实实现**：
- `WorkflowPlan`（steps: tool|agent + depends_on）→ `compile_plan_to_flow`：依赖→`GraphNode.dependencies` + `input_mapping`，并行由 `FlowExecutionMode::Concurrent` 兑现；plan 可含 **AgentNode 步骤**（嵌套 agent 共享同一 registry）。
- **失败驱动 replan（L1.1）是真的**：`run_with_replan` 把已成功步转为 `PrecomputedResultNode` 直接复用、失败与未执行步分类喂给 replan prompt、预算耗尽返回部分状态——有三个专项测试覆盖（`dynamic.rs:998/1055/966`）。
- CLI `workflow dynamic` 治理三重（受限 SandboxPolicy + `--dry-run` + `--approve` 走 Harness 审批且 planner/compiler 共享同一被治理 registry），并把 LLM 生成的计划显式当作 "adversarial by construction" 对待。

**关键缺口**：
1. **replan 无用户入口**：CLI 只调 `compile_plan_to_flow`（`dynamic.rs:144`），`run_with_replan` 仅库内可用——动态工作流最有价值的长时程生存能力用户不可达。
2. **表达力限于"无环、无条件、无循环"**：`run_if` 硬编码 `None`、不支持 Map/While——动态计划不能表达分支与迭代，而这恰是"动态"最需要的。
3. **计划内嵌 agent 不继承治理**：`kind:"agent"` 步骤新建的 `ReActAgent` 不继承父 `RuntimeLimits`/cancellation token/event sink（`dynamic.rs:309-317`）——子 agent 不可取消、不受父预算约束、对 Harness 不可见。
4. 计划解析只做 `extract_json` + serde，无 jsonschema 校验；环在运行时才被拓扑排序发现。

### 4.4 Harness 治理：契约 A 级、机制 B+、生产接线 C——"缺一次真实 dogfooding"

**H0–H4 逐相核验结果**（对照 `HARNESS_MODE_EVOLUTION.md`）：

| Phase | 判定 | 证据 |
|---|---|---|
| H0 契约冻结 | ✅ 真实关闭 | 信封 12 kinds（CLAUDE.md 写 9 个已过时）+ 冻结 fixture + `harness/1` schema 版本 + 前向兼容测试 |
| H1 Runtime MVP | ✅ 真实关闭且超出承诺 | 三态内核（Opaque/TurnDriven/None）、live bridge 与事后翻译 split-brain 已解、上下文预算从"估算"升级为 tokenizer 精算 + truncate-not-drop |
| H2 Hooks+审批 | ⚠️ 实质关闭但有语义缺口 | fail-closed pre-hook（5s 超时即拒）、production profile 对 NonIdempotent 自动升级审批均真实；但见下列缺陷 |
| H3 并行工具 | ✅ 真实关闭 | 见 §4.2 |
| H4 后台任务 | ✅ 真实关闭 | 嵌套 spawn `task_local!` 拒绝、64 KiB 输出定界、生命周期事件；缺 GC 与并发上限 |
| H5 Server+UI | ⚠️ 路由层关闭、执行层空心 | 路由/SSE/审批 UI/resume(rerun|append) 齐备；执行器空工具注册表 |
| H6 高级兼容 | ✅ 有意 deferred | `H6_PROMOTION_CRITERIA.md` 逐项给出提升信号，决策留痕良好 |

**四个决定性缺陷（修复成本低但影响端到端可信度）**：

1. **D5 生产入口空心（最严重）**：`yanshi-server/src/harness_live.rs:471,575` 均为 `wrap_registry(ToolRegistry::new(), ...)`——服务端 Harness 会话**没有任何工具可治理**，审批 UI 永远无卡片可批，`ReActConfig` 硬编码 4 轮；CLI `--model`（无 `--skill`）路径同样空注册表。整套审批/hook/审计管线被完整接好，但生产中无对象可管。
2. **D2 `DenyAndStop` 不真停**：`ToolError::PolicyDenied` 回到 ReAct 后只是一次 error observation，agent 继续循环到 `LimitReached`；`StopReason::ApprovalDenied` 是生产路径死枚举值——契约文档 "skip the tool call *and* stop the agent loop" 未兑现。
3. **D3 审批缓存粒度过粗（安全）**：缓存键仅 `(tool_name, scope)`，一次 session 放行 `shell` 即等于放行本会话**任意 shell 命令**——对比 Claude Code 的 per-command-prefix 授权明显过宽。
4. **D1 CLI seq 未共享**：`cli/run.rs:179` 给 hook 层新建独立计数器且从未对 runtime 调 `with_seq_allocator`——`harness run --approve` 的 JSONL 会出现重复 seq，正是 Q1.7.1 声称已修复的 bug 在 CLI 路径的漏网（`chat.rs` 与 server 路径均正确）。

**其余值得记录的机制事实**：`SeqAllocator` 用 emit lock 保证"上线序 == seq 序"并有反向延迟对抗测试（罕见的高质量并发测试）；`params_summary` 先脱敏再 4 KiB 截断且顺序论证正确；但 `PendingToolCall.params` 以未脱敏原文交给第三方 pre-hook（违背契约 MUST）；`run_flow` 超时仅 drop future 无取消传播；`wrap_registry` 无重包装守卫且**包装后再注册的工具（如运行期发现的 MCP 工具）完全绕过治理**；三条 resume 路径（`resume` 纯回放 / `resume-loop` 真恢复但绕过 Harness / server `:resume` 唯一完整）命名混淆能力不等价。

### 4.5 Loop Engineering：完成度约 75%，Agent 侧强、DAG 侧弱、两侧割裂

对"循环工程"的系统性核验（跨 harness/agents/core/memory 四个 crate）：

**Agent 侧（B+ ~ A−）**：
- **循环防御双层真实**：F-A2-13 近邻纠偏（第 2 次相同调用注入 steering note，只进 memory 不污染 observation）+ L1.2 滑窗签名检测（`window:6, threshold:3` 默认开启，能抓 A,B,A,B 交替模式，`recent_tool_calls` 随 checkpoint 持久化——跨 resume 不丢窗口）。threshold=3 刻意给 nudge 留起效空间，这类"两层防御的相互配合"是有设计深度的。已知弱点：检测滞后一个 turn（代码自陈）。
- **上下文预算工程**：真实 tokenizer 重算（覆写 provider 估算）→ 优先级稳定排序 → 装不下则**截断而非丢弃**（修复了"大高优项被丢、小低优项混入"的优先级倒置）→ 被丢项交给 `ContextSummarizer` 压实为 Critical 合成项 + 发 `MemorySummaryAdded`。压实器默认确定性（可 replay），LLM 版可插拔。
- **长时程恢复链完整**：`TaskSummary`（压缩时重生成、诚实不编造 open_questions）→ 项目记忆（从 shell/script/code_exec 调用提取事实、跨会话持久化）→ 偏好层（`remember_preference` 工具闭环）→ agent-loop checkpoint（每回合保存）。四层皆可选、None 时零开销。

**DAG 侧（C）**：While 节点条件只能引用 loop inputs（nodes 上下文传空 HashMap）、迭代耗尽静默、子流程失败被吞、无循环事件——见 §4.1。

**割裂点**：DAG While 与 Agent loop 之间没有统一的"循环预算/循环事件"抽象；`run_flow` 治理不会把 While 迭代暴露为 `step_started`；`TurnDrivenRuntime` 只有 ReAct 实现，导致 `--context-refresh`（turn 间刷新 workspace 上下文）与 interrupt-resume 互斥。**长时程能力在两条执行路径上不等价**。

### 4.6 能力基座对上层范式的支撑度

上层四范式共享的 capability substrate（"所有范式调同一个 `ToolRegistry`"）核验为真：Builtin/Script/MCP/Workflow 四种 `ToolSource` 统一进 registry，`Capability` 三层交集合并带可解释审计轨迹（`CapabilityDecisionEntry` 逐层记录 allowed/dropped），Skill 经 `SkillCapability::lower()` 汇入。这一"窄腰"是框架生态的正确地基。

但按生态成熟度排序，各能力支柱参差：

1. **工具与沙箱（A）**：三档隔离（进程内 `SandboxPolicy` → OS 沙箱 seccomp/Landlock/sandbox-exec → `ContainerBackend` microVM）+ 代码来源信任模型（author-signed / user-provided / llm-generated 分道），这是 2026 年 agent 框架里少见的完整纵深。
- 2. **Skills（B+）**：SKILL.md 生态对齐（Anthropic Skills 形制）、完整性校验、分层知识、marketplace 验签俱备；缺服务端 marketplace 路由与更丰富的示例生态。
3. **RAG（B+）**：作为 `KnowledgeBackend` capability 挂在 Skill `knowledge:` 之后（而非顶层模式）的重定位是正确的生态判断；eval 门禁是稀缺资产；中文检索（CJK 分词缺失）是硬伤。
4. **Memory（B+）**：四层模型 + 保留策略齐备；语义检索不可扩展、项目记忆未端到端打通。
5. **MCP（B−）**：作为 2026 年 agent 生态的"USB 接口"，停在 2024-11-05 协议 + 仅 stdio + 无重连，是**生态兼容面上最急迫的短板**——它直接限制了 Yanshi 接入现代 MCP server（Streamable HTTP、elicitation、structured tool output）的能力。
6. **LLM（A−）**：provider 覆盖与 tool-calling 一致性测试体系优秀；SSE bug 与流式工具调用缺口影响的恰是 agent 循环最依赖的路径。

### 4.7 与业界上层原生智能体框架的横向对标

以四个代表性参照系做定性对照（基于本仓库源码事实 + 截至评估者知识的业界公开形态）：

| 维度 | LangGraph | OpenAI Agents SDK | Claude Code（Harness 形态） | **Yanshi 现状** |
|---|---|---|---|---|
| 图/状态机表达 | 一等公民（conditional edge、subgraph、Send API） | 弱（handoff 为主） | 无显式图 | DAG 真实但无条件边、子图降级 |
| 计划→执行分离 | 需自建 | 需自建 | 内置 TODO/计划工具 | **`compile_plan_to_flow` 内建**（差异化优势），但表达力受限 |
| Checkpoint/时间旅行 | 一等公民 | 有 sessions | 会话续跑 | 双层（DAG checkpoint + agent loop checkpoint）真实，resume 语义碎片化（三条路径） |
| HITL | interrupt 原语 | 需自建 | 审批一等公民 | `ask_user` + 审批管线均有；审批粒度粗于 Claude Code |
| 治理（hooks/审批/沙箱） | 基本无内建 | Guardrails | **最强参照** | 机制齐平 Claude Code（hooks/approval/背景任务/上下文注入），接线未闭环 |
| 多智能体 | supervisor/swarm 模式库 | handoffs 一等公民 | subagent 一等公民 | 三种 supervisor 真实但不能嵌套、丢观测上下文 |
| 工具生态 | MCP adapters 活跃 | MCP + hosted tools | MCP 宿主 | 自研 MCP 停在 2024-11-05，**生态接口落后** |
| 可观测性 | LangSmith 商业闭环 | tracing 内建 | 会话日志 | OTel 模型+缝合齐备但无 OTLP 出口、无托管后端 |
| 沙箱/代码执行安全 | 无内建 | 托管沙箱 | OS 级 + 容器 | **齐平或领先**（内核级过滤 + microVM + 来源信任模型） |
| 语言/性能定位 | Python/JS | Python/JS | 产品非框架 | **Rust 原生**——类型安全 + 单二进制部署是稀缺定位 |

**定位判断**：Yanshi 的差异化组合是"**Rust 原生 + 治理纵深 + 计划编译**"——这三点同时具备的开源框架在 2026 年仍然稀少。它不是 LangGraph 的图表达力对手，也还不是 Claude Code 的产品化对手，但"把 agent 的计划编译成可治理、可回放、可恢复的 DAG"这条中间道路是自洽且有真实代码支撑的。**当前完成度支持"框架内核"叙事，尚不支持"生态平台"叙事**——生态平台需要 MCP 现代化、marketplace 服务化、多副本部署、OTLP 出口四块补齐。

---

## 5. 关键缺陷与风险清单（按优先级）

### P0（阻断端到端可信度，建议立即修复）

| # | 缺陷 | 位置 | 影响 |
|---|---|---|---|
| P0-1 | 服务端 Harness / CLI `--model` 路径空 `ToolRegistry` | `harness_live.rs:471,575`；`cli/run.rs:547,558` | 治理管线生产中无对象可管；"workspace-aware agent"名不副实 |
| P0-2 | `POST /v1/skills/{name}:run` 是必失败 stub | `server/skills.rs:197-198` + `runs.rs`（无 `@skill:` 处理） | 列表可见、执行必 Failed，用户直接受骗面 |
| P0-3 | SSE buffer 残留 bug 仅修 1/5 provider | `openai.rs:554-590` 等四处（对照 `stepfun.rs:525-533` 的修复） | 流式回复末尾内容静默丢失、`is_final` 不置位 |
| P0-4 | retry 包装吞 `NodePartialExecutionFailed` | `retry_executor.rs:63-78` | 配 `max_retries` 的 agent 节点部分恢复契约静默失效 |
| P0-5 | `DenyAndStop` 不停循环 / `ApprovalDenied` 死枚举 | `hooks_runtime.rs:496-521`；react 无 `PolicyDenied` 停机分支 | 最高级别治理动作语义未兑现 |

### P1（高，影响正确性/安全边界）

| # | 缺陷 | 位置 |
|---|---|---|
| P1-1 | `observability` feature 全 workspace 未开启——引擎级告警全静默 | `yanshi-core/Cargo.toml:22` + 13 个依赖方 |
| P1-2 | 审批缓存仅按工具名（session 放行 shell = 放行任意命令） | `hooks_runtime.rs:260-284` |
| P1-3 | Map/While 子流程丢 listener/cancel/run_dir/并发模式 | `core/flow.rs:1212,1288,1344` |
| P1-4 | CLI `harness run` seq 计数器未共享 → JSONL 重复 seq | `cli/run.rs:179` |
| P1-5 | seccomp blocklist 未拦 `io_uring`（经典逃逸面，未文档披露） | `tools/sandbox/linux.rs` |
| P1-6 | run_if 求值失败整体中止（无终态事件/checkpoint 卡 Running） | `core/flow.rs:385,869` |
| P1-7 | worker `agent` payload 空 `ToolRegistry`（工具分发契约未定） | `worker/lib.rs:890-948` |
| P1-8 | `HttpTool` 响应体按字节切 UTF-8 边界 panic | `tools/builtin/http.rs:326` |
| P1-9 | 多副本部署即失效（SSE broker/准入/审批/取消全进程内） | `server/events_stream.rs` 等 |

### P2（中，影响体验/扩展性/一致性）

工作流级 `inputs:` 覆盖全图同名参数（静默）；resume 强制串行；Google/Moonshot/StepFun 流式无 tool_call deltas；`RateLimitConfig` 纯装饰；MCP 无重连/无能力门控/协议 2024-11-05；`hybrid_search` 忽略 alpha；MMR 用 Jaccard 冒充语义多样性；BM25 无 CJK 分词；`SemanticMemory` 全量加载 + N+1；supervisor 丢子上下文且不能嵌套；dynamic agent 步不继承限额/取消；replan 无 CLI 入口；`compile_plan_to_flow` 无条件/循环；后台任务表无 GC 无并发上限；`wrap_registry` 后注册工具绕过治理；While 耗尽静默；`events` 表无分区；~2,340 行生产原语零接线；`TimeoutConfig`/`ErrorContext`/同步 `Node` trait 死代码；UI 单测不在 CI 执行；tracing 只有文件后端；网关硬编码串行执行。

---

## 6. 文档-代码漂移清单（可直接据此修文档）

| 文档声明 | 代码事实 |
|---|---|
| CLAUDE.md："Production primitives: resource_manager / resource_limits / health / state_monitor" | 全部零生产调用者 |
| CLAUDE.md N8："tagged-but-corrupt payloads **warn loudly**" | warn 在 `observability` feature 后面，实际构建全被编译掉 → 静默降级 |
| `checkpoint.rs:12`："Concurrent-safe with **file locking**" | 无任何锁 |
| CLAUDE.md：harness 信封 "closed kind set" 列 9 个 | 实际 12 个（+`interrupt_requested`/`interrupt_answered`/`token_delta`） |
| CLAUDE.md：DB "Nine-table schema" | 已 10 张表（`0007_harness_loop_checkpoints`） |
| CLAUDE.md / tracing 文档："JSONL or **SQLite/Postgres** (feature-gated)" | 仅 `FileTraceStorage` 一个实现；SQLite/PG 只有 DDL 字符串；`postgres` feature 是死 flag |
| CLAUDE.md："Harness file-backed trace integration ... **not wired today**" | `tracing_bridge.rs` 的 `ExecutionTraceSink` 已实现（同一文档他处又承认）——文档内部自相矛盾 |
| `agent-spi/runtime.rs:20`：`max_steps` "counts every emitted AgentStep" | ReAct 按回合计、PlanExecute 按计划步计——同名字段三种语义 |
| `agent-spi/runtime.rs:417`：`CostLimitExceeded` "runtimes do not enforce cost budgets yet" | 两个 runtime 均已强制执行（T1.1） |
| CLAUDE.md 把 `VerificationStrategy`/`ReflectionStrategy`/`MemorySummaryBackend` 列为交付特性 | 内置实现分别为 no-op / 模板串 / 确定性拼接——是扩展点骨架非能力 |
| `docs/HARNESS_MODE.md:638-648` 相位表 | H2/H3/H4 同时出现 "closed" 与 "gated" 两行，自相矛盾 |
| `LOOP_NODES_IMPLEMENTATION.md` 全部行号/路径 | 已失效（factory 迁移、flow.rs 膨胀至 4,388 行） |
| `ProviderResponse::thinking` 文档称支持 DeepSeek-R1 `reasoning_content` | `OpenAIProvider` 无解析分支 |
| `create_chunker(Semantic)` 报 "not yet implemented" | 已实现，走 `chunk_async` 独立入口——错误文案误导 |
| `yanshi-nodes/Cargo.toml` feature 注释 | 仍描述已移除的 `llm` feature |

---

## 7. 生产就绪度评估

**当前真实可用形态**：单机、CLI-first。`yanshi workflow run`（本地 DAG + 文件 trace + checkpoint 恢复）、`yanshi harness run --skill ...`（带真实工具的受治理 agent 会话）、`yanshi workflow dynamic`（受限沙箱下的计划编译执行）、`skill test/validate` + `rag eval` + agent eval（质量门禁）——这条路径经过大量测试且安全默认到位，**可以负责任地给个人/小团队使用**。

**距离产品级（服务化/集群）的四个"最后一公里"**（每项下游组件均已实现并测试，缺的是接线）：
1. **工具分发契约**（P5.5 挂起）→ 解锁服务端 Harness 与分布式 agent 的空注册表（P0-1 / P1-7 同根）。
2. **分布式调度器接入 `/v1/runs`**（`DistributedDagScheduler` 有实现无入口）+ 网关取消硬编码串行。
3. **跨副本状态外置**（SSE broker → Redis/NATS；准入 → 分布式令牌桶；审批/取消 → DB 表）——Helm chart 默认 `replicaCount: 1` 实为被这些进程内状态逼出的约束。
4. **OTLP exporter**（span 模型/traceparent 缝合/worker trace stitching 全部就绪，差传输层）。

按当前工程节奏（审计→修复→留痕的闭环速度），这是**季度级而非年度级**的距离。

---

## 8. 评级矩阵汇总

| Crate / 维度 | 评级 | | Crate / 维度 | 评级 |
|---|:---:|---|---|:---:|
| yanshi-value | A | | yanshi-agents | B+ |
| yanshi-tool | A | | yanshi-skills | B+ |
| yanshi-tools | A（沙箱 A+） | | yanshi-rag | B+ |
| yanshi-llm | A− | | yanshi-memory | B+ |
| yanshi-agent-spi | A− | | yanshi-server | B+ |
| yanshi-store-spi | A− | | yanshi-cli | A− |
| yanshi-graph | B | | yanshi-harness | B |
| yanshi-core | B | | yanshi-worker | B |
| yanshi-async-util | B− | | yanshi-config | B− |
| yanshi-nodes | B− | | yanshi-db | B− |
| yanshi-nodes-ai | C+ | | yanshi-worker-proto | C+ |
| yanshi-mcp | B | | yanshi-tracing | C+ |
| xtask | B+ | | yanshi-ui | C |
| **工程流程（CI/测试/文档）** | **A−** | | **综合** | **B+** |

与 `PROJECT_EVALUATION_2026-08-05.md`（B+/A−，生产就绪 C+）对比：V 段修复（V0–V4）真实闭环了当时的越权/终态/SSRF/供应链等发现（本次抽查 C1/C2/C3/M1 均确认修复且注释留痕），综合评级维持 **B+**；生产就绪维持 **C+**——因为 V 段修的是"正确性与安全"，而本次识别的主要残余是"接线与产品化"，属不同性质的债务。

---

## 9. 建议路线（按投入产出排序）

### 9.1 立即（1–2 周量级，多为数十行修复）

1. 修复 P0-1：给 `LiveHarnessExecutor` 与 CLI `--model` 路径接入 `SkillBuilder`/内置工具注册表（哪怕先只挂 File/Http 只读工具），让治理管线第一次"有物可治"；同步移除 `max_iterations(4)` 硬编码。
2. 修复 P0-3：把 StepFun 的 buffer-drain 修复复制到 OpenAI/Anthropic/Google/Moonshot 四个 provider（模式已有，逐字参照）。
3. 修复 P0-4：`retry_executor` 对不可重试错误直接透传原错误而非包装 `RetryExhausted`。
4. 修复 P0-5：ReAct 对 `ToolError::PolicyDenied` + stop_after_deny 增加停机分支，产出 `AgentStopReason` → `StopReason::ApprovalDenied`。
5. 修复 P1-4（一行）：CLI `run.rs` 与 runtime 共享 `SeqAllocator`。
6. 决策 `observability` feature：要么默认开启（推荐，`tracing` 本身近零成本），要么删除 cfg 门改为无条件——当前状态等于自废可观测性。
7. 对 §6 漂移清单做一次文档批改（半天量级，换取"文档可信"这一项目最重要的隐性资产）。

### 9.2 一个月量级

8. 定义**工具分发契约**（序列化工具清单 + 能力协商 + 沙箱策略下发），同时解锁服务端 Harness、worker agent payload、`/v1/skills/:run` 三个空心点——这是当前单点投入回报最大的一项。
9. 审批缓存键加入参数摘要/命令前缀维度（对齐 per-command-prefix 授权粒度）。
10. Map/While 子流程继承父 config（listener/cancel/run_dir/并发模式）；While 增加 `iterations_used` 输出与耗尽事件。
11. `workflow dynamic --replan <n>` 暴露 `run_with_replan`；`compile_plan_to_flow` 支持 `run_if`；agent 步继承父限额与取消。
12. MCP 升级：协议版本协商 + Streamable HTTP transport + 断线重连——这是生态兼容面的最短板。
13. seccomp 拒绝集补 `io_uring_setup/io_uring_enter`（或至少在模块文档披露该缺口）。

### 9.3 一个季度量级

14. 跨副本状态外置（broker/准入/审批/取消）+ 分布式调度器接线 + 网关并发执行——解锁 cluster 叙事。
15. OTLP exporter（HTTP 先行）+ tracing 存储 DDL 落地为真实现或删除死 feature。
16. Supervisor 层重构：接受 `Box<dyn AgentRuntime>`、透传 event_sink/checkpointer/trace_context、接入 `DelegationSpec`。
17. `react/agent.rs`（8,060 行）与 `builder.rs`（2,600 行）拆分；清理死代码（同步 `Node` trait、`TimeoutConfig`、`ErrorContext`、`common/`、空测试文件）。
18. 认知策略充实：提供 LLM-backed `VerificationStrategy`/`ReflectionStrategy` 参考实现（citation 模块已有 `LlmCitationChecker` 先例可循）。
19. 建立 `[workspace.dependencies]` 统一依赖版本；`release.yml` 依赖 `release-gate`；UI 引入 vitest 并入门禁。

---

## 附录：评估方法与局限

- 六路并行子代理各自负责一个架构切面，全量阅读对应 crate 源码（按 `#[cfg(test)]` 边界区分生产/测试代码统计），交叉引用顶层文档但不采信其结论；高影响结论（空注册表、seq 计数器、observability 消费者、MCP 协议版本、check-arch 激活律数、replan 入口、hybrid_search override、`@skill:` stub）由主评估者独立 grep 二次核验。
- **未执行 `cargo build/test`**（评估环境无预建 target/ 与 crates.io 缓存），因此"测试通过率"采信 CI 配置与测试代码存在性，而非本地运行结果。
- 业界对标（§4.7）中参照框架的形态基于评估者截至 2026 年初的公开知识，未联网核实其最新版本；该表用于定位差距方向，不用于精确特性比对。
- 行号引用基于 2026-08-09 的工作副本，后续提交可能漂移；`.claude/worktrees/` 下的历史副本已从所有统计中排除。
