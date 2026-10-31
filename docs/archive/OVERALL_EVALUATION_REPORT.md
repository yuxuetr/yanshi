# Yanshi 当前项目整体评估报告

> Historical reference: this report captured the project state on
> 2026-04-28 and informed the early N6/N7 productisation tracks. For the
> maintained current status, see `docs/CURRENT_STATUS.md`; subsequent
> evaluations live in `docs/archive/PROJECT_EVALUATION_2026-05-01.md` and
> `docs/archive/PROJECT_EVALUATION_2026-05-14.md`.

评估日期：2026-04-28  
评估范围：workspace 全部 crate、核心文档、CLI 执行路径、DAG 与 agent-native 能力边界。  
验证结果：`cargo check --workspace --all-targets` 已通过；本次未运行完整 `cargo test`。

> 更新说明: 本报告保留 2026-04-28 的评估上下文。报告发布后，`TODOs.md` 中的 N6/N7 产品化闭环已完成；涉及 `workflow run` flags、`agent`/`skill_agent` YAML 节点、旧 CLI runner、trace/recovery 示例的“不足”和 P0/P1 建议不再代表当前状态。

## 1. 总体结论

Yanshi V2 已经从单纯 DAG 工作流引擎演进为“DAG 工作流 + agent-native runtime + 工具/MCP/Skills/Memory/RAG/Tracing 支撑层”的模块化 Rust 框架。当前代码可以编译，模块边界清晰，核心抽象已经成型。

对两种目标模式的满足度如下：

| 能力方向 | 当前满足度 | 结论 |
| --- | --- | --- |
| DAG 工作流开发 | 较高 | `yanshi-core::Flow` 已支持节点依赖、拓扑排序、显式输入映射、条件、map、while、checkpoint、事件监听；适合作为生产自动化和确定性流程的核心。 |
| agent-native 智能体开发 | 中等偏高 | `yanshi-agents` 已有 ReAct、Plan-Execute、Runtime trace、工具调用、记忆、反思、AgentNode、WorkflowTool；适合 SDK-first 构建智能体。 |
| Config-first DAG | 中等偏高 | CLI V2 能解析 YAML 并构建 Flow；后续已补齐 `workflow run` input/output/dry-run/timeout/retry，`--watch` 会显式报错。 |
| Config-first agent-native | 中等 | Skills CLI 与 SkillBuilder 已存在；后续已在 workflow YAML factory 暴露 `agent` / `skill_agent` 节点。 |
| DAG + Agent 混合模式 | 中等偏高 | SDK 层已有 `AgentNode` 和 `WorkflowTool`；后续已补齐 CLI/YAML 层的 skill-agent hybrid 示例和 smoke gate。 |

一句话判断：项目已经具备两种模式的核心骨架和可编译实现；DAG 模式更成熟，agent-native 模式功能面较完整但产品化入口、恢复语义、配置化编排仍需补齐。

## 2. 项目架构

当前 workspace 包含 14 个主要 crate：

| 模块 | 定位 |
| --- | --- |
| `yanshi-core` | DAG 执行内核、节点抽象、FlowValue、checkpoint、retry、timeout、资源限制、事件。 |
| `yanshi-nodes` | 内置节点库：LLM、HTTP、File、Template、多模态、MCP、RAG、map/while 相关节点。 |
| `yanshi-llm` | 多模型/多供应商 LLM 调用层，支持文本、多模态、流式、StepFun 专用 API。 |
| `yanshi-cli` | 命令行入口，覆盖 workflow/config/llm/image/audio/mcp/skill/trace/rag。 |
| `yanshi-agents` | agent-native 层：ReAct、Plan-Execute、AgentRuntime、AgentNode、WorkflowTool、Supervisor。 |
| `yanshi-tools` | 统一工具抽象、ToolRegistry、内置 shell/file/http 工具和 sandbox policy。 |
| `yanshi-mcp` | MCP client/server/protocol/transport 集成。 |
| `yanshi-skills` | Skill manifest、SKILL.md、Marketplace、MCP tool adapter、SkillBuilder。 |
| `yanshi-memory` | Session/SQLite/Semantic memory。 |
| `yanshi-rag` | 文档切分、embedding、Qdrant、检索、rerank、数据源。 |
| `yanshi-tracing` | workflow trace、存储 schema、redaction、replay、TUI、OTel 转换。 |
| `yanshi-db` | Gateway PostgreSQL 连接层。 |
| `yanshi-server` | Axum gateway，目前主要是 health/readiness/liveness。 |

推荐理解为四层：

1. 执行内核层：`core`
2. 能力适配层：`nodes`、`llm`、`tools`、`mcp`、`rag`、`memory`
3. 智能体/编排层：`agents`、`skills`、`cli`
4. 运维与产品化层：`tracing`、`viz`、`db`、`server`、charts/docker/docs

## 3. 模块详细评估

### 3.1 yanshi-core

优势：

- `Flow` 使用 `GraphNode`、`NodeType`、`AsyncNode`、`FlowValue` 形成清晰的 DAG 执行模型。
- 支持标准节点、`Map`、`While` 三类节点形态。
- 支持拓扑排序、依赖校验、条件执行、输入映射、checkpoint resume、事件监听。
- retry、timeout、resource limits、health、state monitor 等生产化基础设施已经拆成独立模块。

不足：

- `run_if` 和 while condition 目前是较轻量的字符串/路径判断，不是完整表达式引擎。
- Flow 执行当前主要按拓扑顺序串行推进，DAG 层没有基于依赖就绪的通用并发调度。
- 持久化默认写入 `~/.yanshi/runs`，对于嵌入式或服务端场景需要更显式的运行目录配置。
- checkpoint 对非 JSON 的 `FlowValue::File/Url` 支持存在信息损失风险，部分转换只保存 JSON 或 `null`。

结论：DAG 内核可用且方向正确，是当前最成熟的模块之一；下一阶段应重点提升表达式、并发调度和 checkpoint 序列化一致性。

### 3.2 yanshi-nodes

优势：

- 节点覆盖面广：LLM、HTTP、File、Template、ASR、TTS、图像生成/理解/编辑、Arxiv、MarkMap、MCP、RAG。
- 通过 feature gate 控制 MCP/RAG 等可选能力。
- CLI factory 已支持大部分节点类型映射到 `GraphNode`。

不足：

- 节点参数约定仍偏散，缺少统一 schema/validation 输出。
- 一些节点强依赖外部服务或本地环境，测试隔离和 mock 层需要更系统。
- 后续已在 CLI factory 中暴露 `agent` / `skill_agent` 节点；剩余重点转为统一 schema、错误规范和节点参数校验。

结论：节点生态已经有雏形，适合 DAG 应用；要支撑低门槛生产使用，需要统一 schema、错误规范和配置化 agent 节点。

### 3.3 yanshi-llm

优势：

- 提供 `Yanshi::model(...).prompt(...).execute()` 形式的 fluent API。
- 支持多 provider、多模态、流式、模型注册、配置发现。
- 对 StepFun 专用 API 支持较丰富。

不足：

- 工具调用注释中仍有 “future MCP integration” 痕迹，LLM 原生 function calling 与 `yanshi-tools` 的整合还不是最终形态。
- 配置加载依赖用户目录，服务端多租户场景需要更显式的配置注入。

结论：LLM 抽象可支撑当前节点和 ReAct agent；后续重点是统一 tool calling、模型能力选择和服务端配置隔离。

### 3.4 yanshi-agents

优势：

- 已定义 agent-native 关键结构：`AgentContext`、`RuntimeLimits`、`AgentStep`、`AgentEvent`、`AgentRunResult`、`AgentStopReason`。
- ReActAgent 支持 observe/plan/tool/result/reflect/final answer 的结构化步骤。
- 支持 memory hook、memory summary、reflection strategy、cancellation token。
- `AgentNode` 可把 ReActAgent 嵌入 DAG，`WorkflowTool` 可把 DAG 暴露给 agent 调用。
- 有 golden trace 测试和多 agent/plan-execute 示例。

不足：

- `AgentRuntime` trait 存在，但 ReActAgent 的公共主路径仍以自身 API 为主，runtime trait 生态还可进一步统一。
- partial resume 已有契约，但仍明确限制 unresolved tool call，真正强恢复还没完全完成。
- 缺少可配置的 agent YAML DSL，与 CLI workflow 的集成不足。

结论：agent-native SDK 能力已经比较完整，满足工程开发原型和部分生产集成；若目标是“框架级 agent-native 应用平台”，还需补 config-first agent、强恢复和统一 runtime 插件机制。

### 3.5 yanshi-tools

优势：

- `ToolRegistry` 抽象简单清晰，支持工具注册、查找、权限过滤、OpenAI tools array、prompt 描述、执行。
- Tool metadata/permission/source 已经为 MCP、workflow、builtin 做了统一铺垫。
- 内置 shell/file/http 工具和 sandbox policy，方向符合 agent-native 安全需求。

不足：

- 权限模型更多是声明与过滤，强 enforcement 仍取决于具体工具实现。
- 缺少统一审计事件与 tracing 强绑定。

结论：这是 agent-native 的关键支撑模块，设计方向正确；生产可控性需要继续加强权限执行、审计和策略继承。

### 3.6 yanshi-mcp

优势：

- 包含 protocol、client、transport、server、tools 适配。
- ClientBuilder、stdio transport、list/call tools、resources、prompts、retry 等能力比较完整。
- 有状态机、timeout、integration、latency 相关测试。
- P10.5.1 已清理 `client_old` + 旧 `transport` 历史包袱；`transport_new`
  重命名为 `transport`（保留 `#[deprecated]` 别名以兼容老代码）。

不足：

- server 标注 experimental，生产服务端能力需谨慎评估。
- MCP 与 Skills/Tools/Nodes 的集成已存在，但调用链上的权限、trace、错误上下文还应统一。

结论：MCP client 已具备实用基础；server 和跨模块治理还需要收敛。

### 3.7 yanshi-skills

优势：

- 支持 `SKILL.md` 与 `skill.toml`，覆盖 persona、model、tools、knowledge、memory、security、MCP server。
- 有 SkillBuilder、SkillLoader、Marketplace、index、MCP tool adapter。
- 与 agents/tools/memory/rag/mcp 的依赖关系符合 agent-native 组合式设计。

不足：

- Skill 与 workflow DAG 的双向组合还不够自然：Skill 可以构建 agent，但 workflow YAML 不能直接声明一个 skill-agent 节点。
- 安全策略需要和 `yanshi-tools`、MCP server、CLI 权限统一。

结论：Skills 是项目走向 agent-native 应用封装的核心；建议作为未来 config-first agent 的主入口继续深化。

### 3.8 yanshi-memory

优势：

- 定义 `MemoryStore`，提供 session、sqlite、semantic memory。
- ReActAgent 已接入 memory 和 memory summary。

不足：

- 长期记忆的 schema、检索质量评估、隐私/清理策略还比较基础。
- semantic memory 依赖 RAG，但端到端 agent 记忆测试覆盖还可加强。

结论：满足基础会话记忆与持久记忆需求；高级 agent 记忆仍需质量评估和治理能力。

### 3.9 yanshi-rag

优势：

- 模块完整：chunking、embedding、indexing、retrieval、reranking、sources、vectorstore。
- 支持 Qdrant、OpenAI embedding、本地 embedding、PDF/HTML/CSV/text 数据源。
- CLI 和 nodes 有可选 RAG 集成。

不足：

- 版本为 `0.3.0-alpha`，说明稳定性定位仍偏实验。
- 需要更多真实语料端到端评测、召回/精排指标和配置模板。

结论：RAG 能力面较宽，适合支撑智能体知识检索；生产成熟度仍需评测体系补强。

### 3.10 yanshi-cli

优势：

- CLI 命令覆盖广，包含 workflow、config、llm、image、audio、mcp、skill、trace、rag。
- 当前 workflow run 使用 V2 `FlowDefinitionV2` + factory + `yanshi-core::Flow`，方向正确。
- trace/skill/mcp 等命令说明项目已在产品入口层做整合。

不足：

- `workflow run` 的 `input/output/dry_run/timeout/max_retries` 已在后续闭环中实现，`watch` 已改为显式未实现错误。
- workflow 输出契约已收敛为默认人类可读、`--output` 保存 JSON、`--output -` 输出机器可读 JSON。
- 旧 CLI runner 已删除或隔离，当前执行路径为 `FlowDefinitionV2 -> GraphNode -> yanshi_core::Flow`。
- Config-first agent-native 入口已补齐 `agent` / `skill_agent` 节点，后续重点是 schema 和错误体验。

结论：CLI 是当前最大成熟度短板之一。它已经能跑 V2 DAG，但距离稳定用户界面还有明显差距。

### 3.11 yanshi-tracing

优势：

- 基于 core `EventListener` 非侵入接入。
- 包含 file storage、schema、redaction、format、replay、TUI、OTel 转换。
- 对 workflow 调试、审计、回放有良好方向。

不足：

- agent/tool/MCP 跨边界 trace 的完整关联需要继续验证。
- PostgreSQL storage 是 feature，可用性和迁移流程仍需更多端到端测试。

结论：Tracing 设计完整，是生产化重要基础；下一阶段应加强跨 DAG/agent/tool 的统一 trace id 传播。

### 3.12 yanshi-viz（P10.13.1 已删除）

该 crate 在 2026-05-20 的 P10.13.1 中整体下线：从 workspace
members、`/v1/runs/{id}/graph` REST 路由、`yanshi workflow
graph` CLI 子命令、以及 Web UI 的 Mermaid 文本块全部移除。
触发原因：UI 端的"DAG 可视化"实际只是事件驱动的状态按钮 +
原始 Mermaid markdown 文本，没有 SVG、没有节点布局、没有
依赖边——后端只为"提供一段 Markdown 文本字符串"维护一个
crate + 一条 REST 路由，性价比为负。

未来若要做真正的图形化 DAG / agent topology 渲染，应作为
增量 UI 特性（例如直接在 SPA 中接 mermaid.js 把
`yanshi workflow validate --output mermaid` 输出渲染为
SVG），不再需要后端常驻 crate。决策记录见
`docs/ROADMAP_v2.md` Theme D。

### 3.13 yanshi-db 与 yanshi-server

优势：

- DB 模块有 PostgreSQL pool 管理。
- Server 基于 Axum，具备 health/live/ready 基础路由。
- charts/docker-compose/Dockerfile 存在，说明部署方向已启动。

不足：

- Server 目前主要是健康检查，没有 workflow/agent/skill/run 管理 API。
- DB 没有看到完整业务 schema/migration/仓储层。
- `yanshi-db` 和 `yanshi-server` 使用 Rust 2024 edition，而其他 crate 多为 2021，workspace 风格不完全统一。

结论：Gateway 仍是骨架阶段，不能视为完整平台服务端。

## 4. DAG 模式满足度评估

已满足：

- 显式节点依赖和拓扑排序。
- 标准节点、map、while、条件执行。
- 显式输入映射和 namespaced state pool。
- checkpoint 与 resume。
- 事件监听、trace 接入、可视化基础。
- Code-first 与部分 config-first 支持。

未完全满足：

- 通用并发 DAG 调度不足。
- 表达式语言较弱。
- CLI 参数和运行控制不完整。
- YAML schema、校验、错误提示还需要强化。
- checkpoint 对复杂 FlowValue 的恢复一致性需加强。

判断：DAG 模式可以满足当前智能体应用中的确定性工作流、批处理、工具链编排、RAG pipeline 和自动化任务开发；若定位生产编排引擎，还需补并发调度、配置规范和运维控制。

## 5. Agent-native 模式满足度评估

已满足：

- 有结构化 runtime model：context、limits、step、event、result、stop reason。
- 有 ReAct agent、Plan-Execute、Supervisor 雏形。
- 有工具注册、工具权限、MCP 工具、WorkflowTool。
- 有 Session/SQLite/Semantic memory。
- 有 reflection、memory summary、runtime guard、cancellation。
- 有 AgentNode 支持 DAG 嵌入 agent。

未完全满足：

- config-first agent DSL 不完整。
- workflow YAML 不能直接声明 agent/skill-agent 节点。
- partial resume 仍有限制。
- tool calling 与 LLM provider 原生 function calling 的统一程度不足。
- 多智能体协作目前更多是模块/示例，平台级调度和观测还不完整。

判断：agent-native 模式适合 SDK-first 开发智能体应用，也能通过 Skills 封装部分应用；但如果目标是“非 Rust 用户通过配置构建 agent-native 应用”，目前还不充分。

## 6. 主要风险

1. YAML schema 和节点参数校验仍需统一，否则 config-first 错误体验会不稳定。
2. agent 工具副作用恢复仍是高风险区，特别是 unresolved tool call 的幂等边界。
3. MCP server、PostgreSQL trace storage、server gateway 等平台化能力仍需更多端到端验证。
4. 模块多、功能面宽，需要持续维护权威端到端样例作为回归基准。

## 7. 优先级建议

P0：

- 统一 YAML schema 和节点参数校验，输出机器可读错误。
- 梳理 MCP server、DB/server gateway 的生产化边界。
- 强化 agent 工具副作用恢复和幂等策略。

P1：

- 引入 DAG 并发调度器。
- 建立 RAG/Memory/Agent 的端到端评测用例。
- 扩展 server API，支持 run 管理、trace 查询、skill registry、workflow 提交。

P2：

- 引入 DAG 并发调度器。
- 建立 RAG/Memory/Agent 的端到端评测用例。
- 扩展 server API，支持 run 管理、trace 查询、skill registry、workflow 提交。

## 8. 最终评级

| 维度 | 评级 |
| --- | --- |
| 架构清晰度 | A- |
| DAG 核心成熟度 | B+ |
| agent-native SDK 成熟度 | B |
| CLI/config-first 成熟度 | C+ |
| 生产可观测性 | B |
| 服务端平台化 | C |
| 综合状态 | B：架构正确、核心可编译可扩展，但产品化闭环仍需集中补齐。 |
