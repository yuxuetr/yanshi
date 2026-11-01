# Yanshi 项目综合评估报告

**评估日期**：2026-10-07
**基线**：HEAD `c9249db`；对照 `docs/PROJECT_EVALUATION_2026-08-09.md`，期间共 87 个提交（W0–W5、P-LLM2、AgentFlow → Yanshi 改名）
**方法**：
- 客观门禁亲自跑过：`check-arch`、`clippy -D warnings`、`cargo test --workspace`、全部示例工作流的 `validate`。
- 源码审计分五路并行：DAG 引擎层、Agent/Harness 层、能力层、平台运维层、工程质量横切。
- 结论只认当前代码里的 `文件:行号`，不采信提交信息或 CLAUDE.md 的自述。
- 标 ✔ 的条目由主评估者在代码里二次核验过。

---

## 1. 执行摘要

### 1.1 一句话结论

> **08-09 报告列出的 5 个 P0 已修复 4 个，剩下 1 个修了一部分。库级和单机 CLI 路径已经接近 A−。** 但这次审计更深入，暴露出一个更根本的问题：**项目把自己定位为多租户网关，实际的信任模型却是"工作流作者 = 运维"。** 具体有四点：
> - 租户可以自己决定 shell 命令白名单、文件路径和 `workspace_root`；
> - Production profile 把 PUT/DELETE 和 Unknown 都算作不需要审批；
> - 审批缓存可以被前缀碰撞绕过；
> - 子进程继承宿主机的全部环境变量。
>
> 这四点叠加，等于在网关主机上执行代码、外泄凭据。再加上多副本的取消、审批、事件通知属于"至多投递一次"且没有补偿，**Yanshi 现在适合当库和单机的可信环境使用，不能部署给互不信任的多个租户。**

### 1.2 综合评级（与 08-09 对比）

| 维度 | 08-09 | 本次 | 变化原因 |
|---|:---:|:---:|---|
| 架构设计（契约内核 + 分层） | A− | **A−** | 违规边从 7 条以上降到 10 条已登记的潜在边，但 8 条依赖律仍只有 3 条在强制执行 |
| DAG 引擎（调度 / 控制流） | B | **B** | run_if、While 可观测性、子流程上下文已修；子流程事件会漏到父 listener，隐式依赖有竞态 |
| checkpoint / resume | — | **C** | 格式保真做得扎实，但**所有产品入口都没开启 checkpointing**，resume 不可达 |
| config / YAML 装配 | — | **C+** | ✔ 10 个示例工作流里 4 个 `validate` 不通过，其中包括旗舰示例 |
| 动态工作流 | B | **C+** | 接入了 `--replan`；但 run_if 跳过会连带下游失败，agent 步骤看不到依赖的输出，CLI 没有启用治理继承 |
| ReAct 运行时 | B+ | **A−** | DenyAndStop、限额、取消、批量调度的确定性都扎实 |
| PlanExecute | — | **B−** | 没修 DenyAndStop |
| 多智能体 supervisor | 落后 | **C+** | 吞掉子 agent 的 stop_reason；子 agent 各拿一份完整预算，总预算放大 N 倍 |
| Harness 契约 | A | **A−** | 出 Err 时不发 `stopped`；`runtime_kind` 和实际行为不符 |
| Harness 治理（hooks / 审批） | B | **C+** | Production 下 Unknown/PUT/DELETE 可直接放行；审批缓存前缀碰撞；包装后再注册的工具绕过治理 |
| 后台任务 | — | **D+** | 无 GC、无并发上限、无生产入口 |
| LLM 层 | B+ | **B+** | SSE 残留、流式工具调用、限流都修了；✔ Google 流式疑似整体失效 |
| 沙箱 / 工具层 | A− | **B+** | ✔ 环境变量继承导致泄密；x86_64 的旧式写 syscall 绕过 seccomp |
| MCP | 短板 | **B** | 双时代协议落地；`tools/call` 重试不幂等，Modern 服务端没有 Origin 校验和鉴权 |
| RAG / Memory | B+ | **B− / C+** | 08-09 的检索质量问题基本没动，中文 BM25 不可用 |
| Server API | B | **B** | 请求体上限、错误信封、测试都扎实；✔ harness cancel 是空操作 |
| 鉴权 / 多租户 | 中上 | **C** | ✔ 租户可以拿到网关主机的代码执行权限（§3 S1） |
| 跨副本 HA | 未做 | **C** | 机制有了，但投递至多一次、✔ intent 表只写不读、孤儿 run 没有回收 |
| 分布式 worker | 无入口 | **C−** | 有 HTTP 入口；控制面在进程内存里，没有期限，内存无界增长 |
| 工程流程（CI / 测试） | A− | **A−** | 19 个 job 的 release gate，release 不能再绕过门禁；有 3 个 crate 不在测试矩阵里 |
| 文档 | — | **B−** | 根目录 17 个 md 失控；CLAUDE.md 仍有 5 处以上与事实不符 |
| **生产就绪度** | C+ | **单机可信 B− / 多租户 不可上线 / 多副本 C / 分布式 D+** | |
| **综合** | B+ | **B+**（库级 A−，平台级 C） | |

### 1.3 客观门禁实测（2026-10-07，macOS）

| 门禁 | 结果 |
|---|---|
| `cargo xtask check-arch` | ✅ 26 个成员、90 条内部边；**3 条依赖律在执行**；10 条已登记的潜在违规边 |
| `cargo clippy --workspace --all-targets -D warnings` | ✅ 零警告（另有 `proc-macro-error2` 的未来不兼容提示） |
| `cargo test --workspace --no-fail-fast` | **3354 通过、12 失败、35 忽略** |
| `workflow validate` 全部示例 | ❌ **10 个中 4 个失败**：`ai_research_assistant.yml`（While 条件用了 `and`，表达式只支持 `&&`）和 3 个 `mcp_*.yml` |

12 个测试失败的根因都是本机 `container` 服务没有启动，不是代码回归。但这暴露了一个真缺陷 ✔：
- `ContainerEngine::probe()`（`yanshi-tools/src/sandbox/container.rs:146`）只运行 `container --version`，服务没起时也报告 `Enforcing`。
- 后果：`code_exec_macos.rs` 的跳过保护失效，`yanshi doctor` 和 `skill inspect --explain-permissions` 显示的状态是错的。
- 工具本身在这种情况下拒绝执行，所以不是安全问题。

---

## 2. 08-09 缺陷闭环情况

### 2.1 P0

| # | 缺陷 | 状态 | 证据 |
|---|---|---|---|
| P0-1 | 服务端 Harness / CLI 空工具注册表 | **已修复** | `harness_live.rs:426-458`、`cli/harness/run.rs:558` 改用 `default_governed_registry` |
| P0-2 | `skills/{name}:run` 是 stub | **已修复** | `runs.rs:520-535, 810-823`，有 3 个集成测试 |
| P0-3 | SSE 缓冲残留，影响 4 家 provider | **已修复** | 4 家都先 drain 缓冲区；回归测试在 `tests/sse_buffer_regression.rs`，但 Google 那条喂的是自造数据（见 S5） |
| P0-4 | retry 吞掉 `NodePartialExecutionFailed` | **部分修复** | `retry_executor.rs:76-84` 只在 `attempt>0` 时包装；"先超时、再返回 partial"的路径仍会丢掉 partial 结果 |
| P0-5 | DenyAndStop 不停机 | **部分修复** | ReAct 三条路径都已停机；**PlanExecute 没修**（`plan_execute.rs:1583-1611` 用 `unwrap_or_else` 把错误降级）；supervisor 不向上传递 |

### 2.2 P1

| # | 缺陷 | 状态 |
|---|---|---|
| P1-1 | observability feature | 已修复（W0.7 删除该 feature） |
| P1-2 | 审批缓存只按工具名 | **部分修复，而且修法本身有缺陷**（见 S3） |
| P1-3 | Map/While 子流程上下文 | 部分修复（继承了 listener、cancel 和 run_dir；仍是串行，并引入了事件泄漏 D3） |
| P1-4 | CLI seq 计数器未共享 | 已修复 |
| P1-5 | seccomp 未拦 io_uring | 已修复（但发现新缺口 S4） |
| P1-6 | run_if 失败整体中止 | 已修复 |
| P1-7 | worker agent 空注册表 | 部分修复（有 File/Http 清单，但没有 hook/审批，sandbox 由提交方决定） |
| P1-8 | HttpTool UTF-8 截断 panic | 已修复（但 `response.text()` 仍全量下载，没有字节上限） |
| P1-9 | 多副本 | 部分修复（NOTIFY 四通道 + 真实 PG 测试；投递语义见 P3） |

### 2.3 P2 中仍未解决的项

- **RAG / Memory**：`hybrid_search` 仍忽略 `alpha`；MMR 仍用 Jaccard 计算相似度；**BM25 只按空白切分，CJK 文本召回近乎为零**（`bm25.rs:122-130`）；`SemanticMemory` 仍全量加载再逐条查询（N+1）；`memory → rag` 仍耦合。
- **DAG 层**：resume 强制串行；工作流 inputs 覆盖同名参数仍只告警不改合并顺序；While 条件看不到节点输出。
- **节点**：HttpNode 对 4xx/5xx 返回 `Ok`；ArxivNode 用明文 http，且不受沙箱和解压大小限制。
- **治理**：`wrap_registry` 之后注册的工具绕过治理。
- **平台**：events 表没有分区；harness 相关的 4 张表没有任何保留策略。
- **架构**：`yanshi-graph` 仍依赖 `dirs`/`tokio`；`yanshi-agents/src/common` 仍被 `pub use` 导出。

---

## 3. 新发现：安全与治理边界（最高优先级）

这几项单独看是中高风险，**合在一起就是一条完整的攻击链**，所以放在最前面。

| # | 缺陷 | 证据 | 场景 |
|---|---|---|---|
| **S1** ✔ | **网关信任模型错位：租户能决定沙箱边界** | `yanshi-config/src/executor/shell.rs:61`（`allowed_commands` 来自提交的 YAML）；`factory.rs:84-117`（file 节点的 `allowed_paths`）；`charts/yanshi/values.yaml:20` 默认 `securityProfile: local` | 持 token 的租户 `POST /v1/runs` 提交 `allowed_commands:[python3]`，或用 `allowed_paths:["/"]` 读 `/proc/self/environ`，就能拿到 `DATABASE_URL` 和 `YANSHI_API_TOKEN_TENANTS`，进而冒充所有租户 |
| **S2** ✔ | `workspace_root` 只检查非空 | `yanshi-server/src/harness.rs:639-644` | 提交 `workspace_root:"/"`，只读 file 工具就能读整个主机；再配合 http GET（判为幂等、不审批）外传 |
| **S3** ✔ | 审批缓存的"前缀摘要"只取前 64 个字符，**比精确匹配更宽** | `yanshi-harness/src/hooks_runtime.rs:318-334`；注释声称"只会更保守"，这是错的 | `code_exec` 的代码开头通常是相同的 `import` 样板，一次 Session 范围的放行就覆盖了之后任意同样开头的代码；`script` 的摘要不含 `args` |
| **S4** | Production 只在 `NonIdempotent` 时升级审批 | `hooks_runtime.rs:519-521`；✔ `http.rs:218` 把 `PUT`/`DELETE` 判为 Idempotent；MCP、AgentTool、WorkflowTool、tools-ai 默认都是 Unknown | Production 下 `DELETE https://api/...` 不审批，还会在批量调度里并发执行；付费的生成调用不审批 |
| **S5** ✔ | Shell/Script 子进程继承宿主机的全部环境变量 | 全仓没有 `env_clear`；`sandbox/macos.rs:309` 逐个复制父进程的 env | LLM 执行 `env` 就能拿到所有 API key，并经工具输出流向 LLM 厂商和 trace。对照：MCP stdio 已经有环境变量白名单 |
| S6 | x86_64 上 seccomp 只拦 `*at` 系列写 syscall | `sandbox/linux.rs:824-838` | x86_64 的 glibc 调 `unlink/rename/mkdir/chmod` 走的是旧式 syscall，能绕过 `!FsWrite`；Landlock 不覆盖 chmod/chown/xattr |
| S7 | `resume-plan` 允许调用方指定 `checkpoint_dir` | ✔ `yanshi-server/src/runs.rs:1587-1590` | 服务端会去读调用方指定的任意目录 |
| S8 | Modern MCP 服务端没有 Origin 校验，也没有鉴权 | `server_streamable_http.rs:126-219` | 目前没有生产调用方，属于潜伏问题；一旦接上真实工具，就违反 spec 的 MUST 条款（防 DNS rebinding） |
| S9 | `/metrics`、`/v1/diagnostics` 没有鉴权，也不按租户隔离 | `lib.rs:551` | 暴露租户名和主机环境信息 |

---

## 4. 新发现：正确性与可靠性

### 4.1 平台 / HA

| # | 缺陷 | 证据 |
|---|---|---|
| P1 ✔ | **Harness 会话 cancel 只改了状态**：agent 继续执行，结束时还会把 Cancelled 覆盖回 Completed | `harness.rs:825-839`（注释还写着 "Once the real executor lands"）；`harness_live.rs` 里没有取消令牌 |
| P2 | `update_status` 没有终态守卫；`publish_cancellation_event` 用 `MAX(seq)+1`，会撞主键，而且走只读副本 | `repo.rs:331-336, 590`；`runs.rs:1546-1558` |
| P3 ✔ | **NOTIFY 至多投递一次，没有补偿**：断线期间的通知直接丢；`run_cancellation_intents`/`approval_decision_intents` **只写不读**；审批 payload 超过 8KB 时被丢弃 | `events_stream.rs:296-340`；`harness_approval.rs:567-578` |
| P4 | 远端副本的 SSE 会重复回放历史，且流永远不结束（`last_forwarded_seq=-1`；只在执行所在副本调用 `finalise`） | `events_stream.rs:88-93, 440-447` |
| P5 | 孤儿 `running` 行永久占用租户的并发名额，没有 lease 也没有启动时回收 | `repo.rs:428`；`cleanup.rs:212-230` |
| P6 | 分布式控制面在进程内存里，没有整体期限；心跳超时会重派非幂等节点；控制面状态内存无界增长 | `distributed_run.rs`、`distributed.rs:183-225` |
| P7 | Helm 默认 `readOnlyRootFilesystem: true`，但没有可写卷，`~/.yanshi/runs` 创建失败，run 必然失败；chart 里也没有 worker 部署 | `values.yaml`、`flow.rs:319` |
| P8 | OTLP 导出器没有接入任何 binary | grep `OtlpHttpSpanSink` 的使用方为 0 |

### 4.2 Agent / Harness

| # | 缺陷 | 证据 |
|---|---|---|
| A1 | 三个 supervisor 都忽略子 agent 的 `stop_reason`：DenyAndStop、MaxSteps、Timeout、AwaitingInput 都被当成 FinalAnswer | `handoff.rs:362-370`、`debate.rs:349-355`、`blackboard.rs:563-573` |
| A2 | 子 agent 各拿一份完整的父级限额，总费用和墙钟可放大 N 倍 | `supervisor/common.rs` 的 `build_child_context` |
| A3 | Debate/Blackboard 用游离的 `tokio::spawn` 跑子 agent，父级超时后子 agent 还在调工具 | `debate.rs:239`、`blackboard.rs:584` |
| A4 | server 请求 `runtime_kind=plan_execute` 时实际跑的是 ReAct | `harness_live.rs:620,633` |
| A5 | CLI 审批 provider：超时后 `read_line` 不可取消，会吞掉用户下一行输入；并发请求时答案可能对错请求 | `approval_providers.rs:221-260` |
| A6 | `harness run-flow --profile production` 实际不治理任何节点工具 | `run_flow.rs:9-12` |
| A7 | LLM 验证/反思调用不受限额约束，验证失败时 fail-open；而且没有任何配置入口能启用它们 | `react/agent/verification.rs:256-265` |
| A8 | 后台任务表只增不删，没有并发上限，不接父级取消；`TaskRuntime` 自建 emit 锁，违背"上线序 == seq 序" | `tasks.rs:192, 226, 245-305` |

### 4.3 DAG / 动态工作流

| # | 缺陷 | 证据 |
|---|---|---|
| D1 | 动态计划中，被 `run_if` 跳过的步骤会让所有下游步骤失败（依赖被生成为必需的 input_mapping），还会触发无谓的 replan 并返回非 0 退出码 | `dynamic.rs:335-350, 556-566`；`flow.rs:1607-1613` |
| D2 | 动态 agent 步骤的 `message` 是固定 prompt，看不到依赖的输出 | `dynamic.rs:378-385`；`agent_node.rs:318` |
| D3 | Map/While 子流程的 `WorkflowStarted/Completed` 漏到父 listener：完成指标被放大 N 倍，collector 为每个子流程存一份孤儿 trace | `flow.rs:288, 544-556, 1253` |
| D4 ✔ | **DAG checkpoint/resume 在所有产品入口都用不到**：除 core 和它的 example 外没有任何地方调用 `with_checkpointing`；`resume` 会丢掉 inputs 和执行配置 | `flow.rs:618-625` |
| D5 | 隐式依赖：`input_mapping`/`run_if` 引用的节点不在 `dependencies` 里时，Concurrent 模式下产生竞态、静默丢输入；示例里有 4 处这种写法 | `schema.rs:121-128`；`flow.rs:1614-1617` |
| D6 | 并行 Map 的 spawn 不随父 future 被丢弃而中止；配了重试时会重复执行 | `flow.rs:1460-1478` |
| D7 | 每个节点完成后都把整个 state pool 序列化一遍估算大小，复杂度 O(N²) | `flow.rs:104` |

### 4.4 能力层

| # | 缺陷 | 证据 |
|---|---|---|
| L1 ✔ | **Google 流式大概率产出空流**：请求没加 `?alt=sse`，解析器按单行 JSON 解析、失败的行静默丢弃；ReAct 无条件走流式。live 套件从未覆盖 Google 流式 | `google.rs:177-186, 664-670` |
| L2 | 5 家 provider 流式都按网络块各自做 `from_utf8_lossy`，中文跨块时变乱码 | `openai.rs:585` 等 |
| L3 | `Retry-After` 没有上限，也不可取消 | `llm_client.rs:72-75` |
| L4 | TPM 限流未执行；重试和 modality 路径不经过限流桶 | `model_config.rs:409` |
| L5 | MCP 客户端超时后重放 `tools/call`，非幂等工具会被重复执行；通知队列没有消费者，满 1024 条后堵死 | `session.rs:445`；`streamable_http.rs:165` |
| L6 | tools-ai 的参数没有上下界（`poll_interval_secs:0` 时忙轮询），模型没有白名单 | `text_to_video.rs:126-135` |
| L7 | `FailWorkflow` 策略的 panic 被 drain task 的 `catch_unwind` 接住，**工作流永远不会因此失败**，名不副实 | ✔ `yanshi-tracing/src/collector.rs:643` |
| L8 | `serve.rs:416` 在 tokio 多线程运行时启动后调用 `set_var`，SAFETY 注释的前提不成立 | `yanshi-server/src/serve.rs:416` |

---

## 5. 工程质量

### 5.1 亮点（保持）

- 生产代码几乎没有 panic：`todo!`/`unimplemented!` 为 0；`unwrap/expect` 约 62 处，大部分是常量正则且带 `#[allow(reason)]`。lib 级的 `clippy::unwrap_used` 在 CI 中强制。
- `release-gate` 聚合 19 个 job。`release.yml` 通过 `workflow_run` 依赖 Quality，不能再绕过门禁。UI 的 vitest 已真跑。
- 测试 3,416 个（08-09 时 3,075 个）。30 个 `#[ignore]` 都有正当理由。有 5 个使用真实 PG 的跨副本测试。
- 最近 100 个提交 100% 符合 Conventional Commits。审计、修复、测试的闭环可以追溯。
- `check-arch` 机制本身是 A 级：清单只能缩小，有过期检查，还会检测分类记错的边。

### 5.2 问题

| 项 | 证据 |
|---|---|
| ✔ **`yanshi-tool`（52 个测试）、`yanshi-tools-ai`（19 个）、`xtask`（86 个）不在 CI 测试矩阵里** | `quality.yml:281-314` |
| 没有覆盖率门禁、没有 MSRV、没有 cargo-deny；~~没有 `cargo doc -D warnings`，而 CLAUDE.md 的提交前清单要求它~~（2026-11-01 已加 `rustdoc` CI 任务，`8f739ad`） | — |
| 本地没设 `YANSHI_DATABASE_TEST_URL` 时，37 个 DB 测试**静默通过**；CI 里也没有"必须连上 DB"的断言 | — |
| `[workspace.dependencies]` 只收了 4 个包；Cargo.lock 中 55 个 crate 存在多个版本（reqwest 0.12/0.13、prost 0.13/0.14 等） | `Cargo.toml:61-65` |
| crate 版本分散在 0.1.0、0.2.0、0.3.0-alpha，而 `v1.0.0-rc.1` tag 已经存在 | — |
| 改名：代码中残留为 0。但 `YANSHI_*` 环境变量没有回退读 `AGENTFLOW_*`，`~/.agentflow` 不会迁移，**CHANGELOG 没有 BREAKING 条目** | 5f9065d |
| 垃圾文件：`_to_delete/.agentflow-src.tgz`（36MB）；两个可 prune 的 worktree；`agents/paper_*` 孤儿 crate（4,519 行，仍被 git 跟踪） | — |
| 根目录 17 个过时 md；`RoadMap.md` 停在 2026-07-23；TODOs.md 队列已清空 | — |
| 热点文件：`yanshi-core/src/flow.rs` 5,137 行（08-09 时 4,388 行，生产代码约 1,936 行） | — |

### 5.3 CLAUDE.md 与事实不符（可直接修）

| 声明 | 事实 |
|---|---|
| "eight dependency laws, enforced by `cargo xtask check-arch`" | 只有 3 条在执行 |
| "24 Rust crates" | 25 个，外加 xtask |
| "~190 registry models" | 212 个 |
| "`yanshi-llm/config/models/`" | 实际路径是 `templates/models/` |
| L4 tracing 一节："Harness file-backed trace … not wired today" | L3 harness 一节自己就写了 `open_execution_trace_sink` 已交付，两处矛盾（08-09 就指出过，仍未修） |
| "checkpoint recovery" 列为 Production-Ready；N8 称"DAG + skill_agent 部分恢复已接线" | 没有任何产品入口开启 checkpointing（D4） |
| `ai_research_assistant.yml` 列为推荐示例 | validate 不通过 |
| "Version 0.2.0+ … before the actual v1.0.0-rc.1 tag" | 该 tag 已存在 |
| H4 后台任务、W3.4 LLM 验证/反思写成已交付能力 | 都没有生产入口 |

---

## 6. 修复路线（按收益排序，每项基本是数十行量级）

### 第一批：安全边界（不修就不能对外开放网关）

1. **网关模式下用服务端白名单约束沙箱边界**（S1、S2、S7）：
   - 节点类型、`allowed_commands`、`allowed_paths`、`workspace_root`、`checkpoint_dir` 都以服务端配置为上限，提交方只能在此范围内收窄；
   - Helm 默认改为 `production`。
2. **子进程执行前 `env_clear()`，再按白名单透传**（S5），沿用 MCP stdio 已有的做法。
3. **Production 下 Unknown 一律升级为需审批**；HTTP `PUT/DELETE` 改判 NonIdempotent（S4）。
4. **审批缓存键改为对完整字段做哈希**，`script` 的键纳入 `args`（S3）；补一条前缀碰撞的测试。
5. seccomp 补上 x86_64 旧式写 syscall 以及 `fchmod/fchown/setxattr` 一族（S6）。

### 第二批：语义正确性

6. Harness cancel 接上取消令牌；`update_status` 加终态守卫（P1、P2）。
7. supervisor 透传子 agent 的非成功 `stop_reason`；PlanExecute 补上 `PolicyDeniedAndStop` 停机分支（A1、P0-5）。
8. Google 流式加上 `alt=sse` 并补一条 live 用例；流式在字节层缓冲后再解码（L1、L2）。
9. 动态工作流：run_if 跳过时把下游依赖改为可选输入；agent 步骤的 message 注入依赖输出；CLI 启用 `with_governance`（D1、D2）。
10. 子流程的 `WorkflowStarted/Completed` 不再转发给父 listener（D3）。

### 第三批：HA 与运维

11. NOTIFY 监听重连后从 DB 追赶缺失的事件；真正读取 intent 表；给 `running` 行加 lease 回收（P3、P5）。
12. Helm 挂可写卷或设置 `YANSHI_RUN_DIR`；把 OTLP 接入 server 和 worker（P7、P8）。

### 第四批：工程卫生

13. CI 测试矩阵补上 `yanshi-tool`、`yanshi-tools-ai`、`xtask`；所有示例纳入 CI 的 `validate`，先修好那 4 个失败的示例；CI 中缺 DB URL 时直接失败。
14. CHANGELOG 补一条改名的 BREAKING 条目和环境变量迁移表；清理 `_to_delete/`、可 prune 的 worktree、`paper_*` 和根目录过时 md。
15. 修正 §5.3 中 CLAUDE.md 的错误；修好 `ContainerEngine::probe()`（改用 `container system status`）。
16. 二选一：把 `FailWorkflow` 做成真的会让工作流失败，或者改名。

### 重估条件（当前版本暂不做）

| 不做的项 | 什么时候重估 |
|---|---|
| events 表分区 | `SELECT pg_total_relation_size('events')` 超过 10GB，或 cleanup 的单次 DELETE 超过 30s |
| 再激活 5 条依赖律 | `ARCH_LATENT_EDGES` 降到 3 条以下时，逐条激活 |
| BM25 CJK 分词 | 有 skill 声明中文 `rag` 知识库的那一刻（目前中文召回近乎为零，属于"有就坏"，而不是"慢慢优化"） |

---

## 7. 生产就绪结论

| 部署形态 | 结论 |
|---|---|
| 库集成 / 单机 CLI（可信操作者） | **可用（A−）**。需要注意 Google 流式（L1）和 checkpoint 不可达（D4） |
| 单节点网关 + 单一可信租户 | **可用（B−）**。前提是 `securityProfile=production`、挂可写卷、不暴露 `/metrics` |
| 单节点网关 + 互不信任的多租户 | **不可上线**。S1、S2、S5 构成从租户到主机凭据的完整攻击链 |
| 多副本 | **未就绪（C）**。正常路径能通；断线时取消、审批、终态事件会静默丢失，孤儿 run 会占满并发名额 |
| 分布式 worker | **实验级（D+）**。只适合单网关加演示规模 |
