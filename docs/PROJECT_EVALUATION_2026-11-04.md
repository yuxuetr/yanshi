# Yanshi 项目综合评估报告

**评估日期**：2026-11-04
**基线**：HEAD `98fc37f`（2026-11-01）；对照 `docs/PROJECT_EVALUATION_2026-10-07.md`（基线 `c9249db`），期间 79 个提交
**方法**：
- 客观门禁亲自跑过（macOS, rustc 1.99.1）：`check-arch`、`clippy --all-targets -D warnings`、`cargo test --workspace`、`RUSTDOCFLAGS=-D warnings cargo doc`、全部示例工作流 `validate`、`yanshi doctor`。
- 源码审计分五路并行：安全与治理边界 / DAG 引擎与动态工作流 / Agent 运行时与 Harness / 能力层（LLM、MCP、RAG、Tracing）/ 平台 HA 与工程流程。每路先逐条复核 10-07 报告的缺陷，再找新问题。
- 结论只认当前代码里的 `文件:行号`，不采信提交信息、CHANGELOG 或 CLAUDE.md 的自述。
- 标 ✔ 的条目由主评估者在代码里二次核验过。

---

## 1. 执行摘要

### 1.1 一句话结论

> **10-07 报告的 66 条缺陷里，36 条已修复、13 条部分修复、17 条未动；其中最要紧的"租户 → 网关主机执行代码 → 外泄凭据"攻击链在 Helm 默认配置下已经关闭。** 库级质量和单机 CLI 路径达到 A−，所有客观门禁全绿（3437 个测试零失败、clippy/rustdoc 零警告、11 个示例全部 validate 通过）。
>
> 但这轮审计发现了三类新问题，决定了下一步该做什么：
> 1. **网关的防线是"按节点类型枚举"而不是"按能力约束"**：`template` 能读进程环境变量（Tera 内置 `get_env`）、`markmap` 能向任意路径写文件、`rag.qdrant_url` 能打内网——三者都不在 host-reaching 名单里，租户无需任何 opt-in 就能用。
> 2. **预算控制有两处是死的**：OpenAI 兼容的 7 家厂商流式永远拿不到 `usage`，而 ReAct 无条件走流式，所以 `cost_limit_usd` 永远累加 0；`token_budget` 读的是会被 8000 token 窗口自动裁剪的 `SessionMemory` 计数，高于窗口的预算永远不会触发。
> 3. **平台层有两个设计级容量问题**：主库连接池硬编码 8 条，而启动时 5 个 NOTIFY 监听器各长期占一条；SSE 回填超过 1000 条事件后静默截断，且不发任何提示。
>
> **Yanshi 现在适合当库、单机 CLI、以及单一可信租户的网关使用；多副本已可用但有已知缺口；互不信任的多租户仍不建议开放；分布式 worker 仍是实验级。**

### 1.2 综合评级（与 10-07 对比）

| 维度 | 10-07 | 本次 | 变化原因 |
|---|:---:|:---:|---|
| 架构设计（契约内核 + 分层） | A− | **A−** | 26 成员 / 90 内部边 / 3 条律在执行 / 10 条潜在边，与上次持平；`yanshi-graph` 已甩掉 `tokio`/`dirs`；`memory → rag` 仍在 |
| DAG 引擎（调度 / 控制流） | B | **B** | D1/D2/D3/D5/D6 都修了且有回归测试；但取消只在节点边界生效、持久化失败直接 `?` 中止、While 出口合并顺序不确定 |
| checkpoint / resume | C | **C** | ✔ 仍然没有任何产品入口开启 `with_checkpointing`；resume 仍丢 inputs 和执行配置、强制串行 |
| config / YAML 装配 | C+ | **B** | ✔ 11 个示例在 `--features mcp` 下全部通过且进了 CI；但默认 feature 不含 `mcp`，默认构建下 4 个自带示例仍失败；`validate` 没有环检测 |
| 动态工作流 | C+ | **B** | run_if 跳过改为可选输入、agent 步骤能看到依赖输出，都有测试 |
| ReAct 运行时 | A− | **B+** | 终止守卫本身扎实；但 ✔ `token_budget` 在默认内存下不可触发、✔ 流式下 7 家厂商成本累加为 0、跨进程 resume 丢对话 |
| PlanExecute | B− | **B+** | DenyAndStop 真停机且有行为测试；服务端真的跑 PlanExecute 了 |
| 多智能体 supervisor | C+ | **B−** | stop_reason 透传了、子任务随父 drop 中止；但"共享预算"只共享墙钟和工具调用数，steps/tokens/cost 仍 ×N；abort 路径无测试 |
| Harness 契约 | A− | **B+** | 运行时自身出 Err 仍不发 `stopped`（服务端补了，CLI 没补）；CLI 接受 `handoff/blackboard/debate` 三种 runtime_kind 但实际跑 ReAct |
| Harness 治理（hooks / 审批） | C+ | **B−** | S3/S4 修得干净；但 ✔ `http`/MCP/模态工具的审批摘要为 `None`，一次 Session 放行覆盖所有后续调用；✔ `AgentTool` 嵌套子 agent 完全绕过 hooks/限额/取消 |
| 后台任务 | D+ | **D+** | 未动：自建 emit 锁、不接父级取消、表只增不删、无并发上限、零调用方 |
| LLM 层 | B+ | **B** | 流式 SSE/UTF-8/Retry-After 都修了且有 fixture；但 ✔ 流式 `usage` 结构性缺失、TPM 限流仍未实现、重试和模态路径绕过限流桶 |
| 沙箱 / 工具层 | B+ | **A−** | ✔ shell/script/MCP 子进程 `env_clear` + 白名单；x86_64 旧式 syscall 已拦；`probe` 改查服务状态。残余：plugin 子进程仍继承宿主 env |
| MCP | B | **B+** | `tools/call` 不再重放、通知队列不再堵死；但两者都没有回归测试；Modern 服务端仍无 Origin/鉴权（未暴露） |
| RAG / Memory | B− / C+ | **B / B** | CJK 二元分词、N+1、hybrid alpha 都修了且有测试；MMR 仍是 Jaccard；embedding 批次不校验对齐 |
| Server API | B | **B** | 终态守卫、SSE 终止、取消令牌都接上了；✔ 回填 1000 条截断、seq 分配仍走只读副本 |
| 鉴权 / 多租户 | C | **B−** | 旧攻击链关闭；但 ✔ 三条无需 opt-in 的越界（env 读 / 任意路径写 / SSRF）；legacy 单 token + `X-Yanshi-Tenant` 自报租户在 production 下仍放行 |
| 跨副本 HA | C | **B−** | NOTIFY 断线补偿、intent 表真读、run lease 都有真实 PG 测试；但 harness 会话没有 lease、seq 分配有竞态、只读副本回填有空窗 |
| 分布式 worker | C− | **C−** | 控制面仍在进程内存、无整体期限、非幂等重派；新发现：worker 在首次心跳前死亡则任务永久挂起 |
| 工程流程（CI / 测试） | A− | **A−** | 测试矩阵补齐、DB 必连、rustdoc 门禁都进了 CI；仍无 MSRV / cargo-deny / 覆盖率；46 个 crate 多版本共存 |
| 文档 | B− | **B** | 根目录从 17 个 md 收到 7 个；CHANGELOG 有改名 BREAKING 表；但 `CURRENT_STATUS.md` 停在 08-09 且仍说 `yanshi-tools` 是 Tool 契约；12 篇 docs 未进 SUMMARY |
| **生产就绪度** | 单机 B− / 多租户 ✗ / 多副本 C / 分布式 D+ | **单机可信 B+ / 单可信租户网关 B / 多租户 仍不建议 / 多副本 B− / 分布式 D+** | |
| **综合** | B+（库级 A−，平台级 C） | **A−（库级 A−，平台级 B−）** | |

### 1.3 客观门禁实测（2026-11-04，macOS）

| 门禁 | 结果 |
|---|---|
| `cargo xtask check-arch` | ✅ 26 成员、90 条内部边、3 条律在执行、0 新增违规、10 条已登记潜在边（与 10-07 完全一致） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ 零警告（`proc-macro-error2 v2.0.1` 的 future-incompat 提示仍在） |
| `cargo test --workspace --no-fail-fast` | ✅ **3437 通过、0 失败、35 忽略**（10-07：3354 通过、12 失败）。本机 `container` 服务在运行，`code_exec` 的 12 个测试这次真跑且通过。**43 个 DB 门禁测试文件本次没跑**：本机 Postgres 需要密码，没有配置 `YANSHI_DATABASE_TEST_URL`，这些测试静默通过（CI 里已有 `psql SELECT 1` 必连断言，本地没有） |
| `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps` | ✅ 零警告 |
| `workflow validate` 全部示例 | ✔ 默认构建：**11 个中 4 个失败**（`mcp_code_analyzer` / `mcp_filesystem_example` / `mcp_simple` / `examples/ecosystem/.../hybrid_offline_demo`，都是 "`mcp` 不在 CLI 工厂里"）；`--features mcp` 构建：**11 个全部通过**。CI 用的是后者。根因：`yanshi-cli` 的 `default = ["plugin", "rag"]` 不含 `mcp`，而自带示例依赖它 |
| `yanshi doctor --format json` | ✅ sandbox `sandbox-exec` enforcing；code_exec `container` enforcing（10-07 时 `probe` 报假 enforcing 的问题已修） |

---

## 2. 10-07 缺陷闭环情况

统计：**66 条 → 36 修复 / 13 部分 / 17 未动**。下面按层列出，只写状态和证据；"修复"均在当前代码里核对过。

### 2.1 安全与治理（§3）

| # | 缺陷 | 状态 | 证据 |
|---|---|---|---|
| S1 | 租户决定沙箱边界 | **修复（核心）/ 部分（覆盖面）** | `yanshi-server/src/gateway_policy.rs:72-140`，在 `runs.rs:1360`（POST /v1/runs）、`runs.rs:1744`（resume-plan）、`harness.rs:822`（会话）处强制；`walk_nodes` 递归进 map/while body（`yanshi-config/src/config/v2.rs:67-105`）；`charts/yanshi/values.yaml:11` 默认 `production`。覆盖面问题见 §3 SEC-1/2/3 |
| S2 | `workspace_root` 只查非空 | **修复** | `harness.rs:820-824` → `check_path`；`gateway_policy.rs:155-157` 拒 `..`；测试 `:222-237` |
| S3 | 审批缓存前缀摘要 | **修复** | ✔ `yanshi-harness/src/hooks_runtime.rs:319-344` 对 `command`/`code`/`script`+NUL+`args`/`path` 做 SHA-256；碰撞测试 `:1301` |
| S4 | Production 只升级 NonIdempotent；PUT/DELETE 判幂等 | **修复** | `hooks_runtime.rs:550-551` 非 Idempotent 一律升级；`yanshi-tools/src/builtin/http.rs:338-339` 只有 GET/HEAD 幂等；两处都有测试 |
| S5 | 子进程继承宿主 env | **部分** | ShellTool `shell.rs:148`、ScriptTool `script.rs:341`、MCP stdio `transport/stdio.rs:433-446`、validator、container 都已清空+白名单；**plugin 子进程未清**（`yanshi-core/src/plugin/host.rs:168-176`、`dry_run.rs:80-86`） |
| S6 | x86_64 旧式写 syscall | **修复，无测试** | `yanshi-tools/src/sandbox/linux.rs:864-876` 拦 unlink/rmdir/rename/mkdir/chmod/chown 等；`:841-856` 拦 fchmod*/fchown*/*xattr；测试只断言 open/creat（`:1026-1031`） |
| S7 | resume-plan 任意 `checkpoint_dir` | **修复** | `runs.rs:1741-1747` → `check_path` |
| S8 | Modern MCP 服务端无 Origin/鉴权 | **未动** | `yanshi-mcp/src/server_streamable_http.rs:122-235`；仍无生产调用方 |
| S9 | `/metrics`、`/v1/diagnostics` 无鉴权 | **部分** | metrics 去掉了租户标签（`metrics.rs:240-244`），仍不鉴权（有意为之）；diagnostics 进了鉴权路由（`lib.rs:700,715`）但任何租户都能看到 `skills_root`/`plugins_root`/MCP `command`（`yanshi-config/src/diagnostics.rs:75-115`） |
| A5 | CLI 审批 read_line 不可取消 | **修复** | `approval_providers.rs:253-296` 保留 `pending_read` 复用 + `tokio::Mutex` 串行；测试 `:466` |
| A6 | `run-flow --profile production` 不治理 | **修复** | `yanshi-cli/src/commands/harness/run_flow.rs:57-60,148-170` 拒绝 HOST 类型和 `http` |
| — | `wrap_registry` 之后注册绕过治理 | **修复** | `hooks_runtime.rs:220-243` 返回 `Arc<ToolRegistry>`，`register` 需 `&mut self`，全仓无 `make_mut`/`try_unwrap` |
| — | `ContainerEngine::probe` 只跑 `--version` | **修复** | `container.rs:150-154` Apple 查 `system status`，Podman 查 `info`；✔ `doctor` 实测正确 |

### 2.2 平台 / HA（§4.1）

| # | 缺陷 | 状态 | 证据 |
|---|---|---|---|
| P1 | Harness cancel 只改状态 | **修复** | `harness.rs:547-590` `HarnessCancellationRegistry`；`:cancel` 触发令牌 `:1024`；跨副本监听 `:604-660`；令牌接入 agent `harness_live.rs:700,794`；有真实 PG 测试 |
| P2 | 无终态守卫；`MAX(seq)+1` 撞主键 | **部分** | 守卫修了（`yanshi-db/src/repo.rs:356-361, 903`）；**seq 分配仍走 `read_pool` 读 `MAX(seq)+1`**（`repo.rs:659,1092`），`append` 是裸 INSERT，与执行器的原子计数器并存 |
| P3 | NOTIFY 至多一次；intent 只写不读 | **修复** | 重连后从 DB 追赶 `events_stream.rs:316-318,366-380`；intent 真读 `runs.rs:411`、`harness_approval.rs:131-150`；审批 NOTIFY 只带 id；测试 `notify_gap_resync.rs`、`cross_replica_approvals.rs:284` |
| P4 | 远端副本 SSE 重放且不结束 | **修复** | `events_stream.rs:514-540` 丢 `seq <= last_seq`，终态 kind 结束流；测试 `sse_terminal.rs`、`cross_replica_events.rs` |
| P5 | 孤儿 running 行永久占名额 | **部分** | run 有了 lease（`0011_run_leases.sql`，120s；`repo.rs:435-463`；循环 `runs.rs:355-380`）；**harness 会话没有 lease**，`cleanup.rs:229-232` 只删终态会话 |
| P6 | 分布式控制面在内存、无期限 | **未动** | `scheduler/mod.rs:52-59` 仍是 `Arc<Mutex<HashMap>>`；`distributed_run.rs:199-330` 无整体 deadline；`requeue_stale_tasks` 不查幂等（`distributed.rs:183-226`）；heartbeats 只增不删 |
| P7 | Helm 只读根无可写卷；无 worker | **部分** | `/data` + `/tmp` emptyDir（`templates/deployment.yaml:84-94`），`YANSHI_RUN_DIR=/data/runs`；**仍无 worker Deployment**，CI 无 `helm lint` |
| P8 | OTLP 零调用方 | **部分** | `serve.rs:517-521` 装 `OtlpHttpSpanSink`，`runs.rs:691` 和 `distributed_run.rs:314` 消费；skills:run、harness、CLI 都不导出 |

### 2.3 Agent / Harness（§4.2）

| # | 缺陷 | 状态 | 证据 |
|---|---|---|---|
| P0-5 | PlanExecute 不停机 | **修复** | `plan_execute.rs:1853-1862` `PolicyDeniedAndStop → ApprovalDenied`；行为测试 `:2013` |
| A1 | supervisor 吞 stop_reason | **修复** | `supervisor/common.rs:28-35` `halts_supervisor`；handoff/debate 原样透传；Blackboard 对非 halting 的 MaxSteps/Timeout 仍折叠为 `all_agents_completed`（`blackboard.rs:513-520`，注释称有意） |
| A2 | 子 agent 预算 ×N | **部分** | `common.rs:85-128` `ChildBudget` 只共享 deadline 和工具调用池（并发时按份平分）；`max_steps`/`token_budget`/`cost_limit_usd` 仍是整份复制（`:108`，注释自己承认） |
| A3 | 游离 spawn 不随父中止 | **修复，无测试** | `debate.rs:248`、`blackboard.rs:593` 用 `AbortOnDropHandle`；没有"父超时后子被中止"的断言 |
| A4 | 服务端 plan_execute 跑 ReAct | **修复** | `harness_live.rs:452-465` 按 `LoopRuntimeKind` 分派；测试 `:1026-1050` |
| A7 | 验证/反思不受限额、fail-open、无入口 | **未动，且更糟** | `verification.rs:259-266` 出错 → `Approved`；`record_reflection` 在超时/MaxSteps 决定之后无 deadline 执行（`core.rs:1362-1385`），出错通过 `?` 变成 `Err`（`:1523-1529`），把一次干净的 `Timeout` 停机变成失败；全仓无 `with_verification`/`with_reflection` 调用方 |
| A8 | 后台任务 | **未动** | `tasks.rs:229` `from_counter` 新建 emit 锁；`:257` 新建未关联的取消令牌；`:300` 裸 spawn；`:275` 表不删；无并发上限；cli/server 零调用 |
| — | 出 Err 不发 `stopped`；runtime_kind 不符 | **部分** | `runtime.rs:560-585` 仍 `?` 返回；服务端 `harness_live.rs:297-309` 合成 `Failed`；CLI `harness/run.rs:293` 直接 `?`，JSONL 没有 `stopped`。CLI `parse_runtime_kind` 接受 `handoff|blackboard|debate`（`run.rs:634-636`）但 `build_agent` 落到 ReAct（`:573-583`） |
| — | `yanshi-agents` 孤儿 `common`/`traits` | **修复** | 两个文件不存在；`lib.rs` 不再导出 |

### 2.4 DAG / 动态工作流 / 节点（§4.3、§2.3）

| # | 缺陷 | 状态 | 证据 |
|---|---|---|---|
| D1 | run_if 跳过连带下游失败 | **修复** | `dynamic.rs:368-378,461-470,489-500` `<dep>::settled` 屏障 + 未声明源视为可选；replan 忽略 `NodeSkipped` `:663`；CLI 退出码排除跳过（`commands/workflow/dynamic.rs:230`）；测试 `:931` |
| D2 | agent 步骤看不到依赖输出 | **修复** | `dynamic.rs:197-243` `AgentStepNode` + `compose_agent_message`；单测 `:975` |
| D3 | 子流程生命周期事件泄漏 | **修复** | `flow.rs:1263` 用 `SubFlowListener`（`:1987-2004`）过滤 Workflow* 事件；测试 `:3970` |
| D4 | checkpoint 产品入口不可达 | **未动** | ✔ cli/server/config/harness 里无 `with_checkpointing` 调用；`execute_from_checkpoint` 仍传空 inputs 和 `None` 配置（`flow.rs:598-606`） |
| D5 | 隐式依赖竞态 | **修复（测试不全）** | `undeclared_references`（`flow.rs:1681-1706`）转成硬边，Serial 拓扑序 `:1744-1747` 和 Concurrent 就绪判断 `:864-867` 都用；回归测试只覆盖 Concurrent（`:2691`） |
| D6 | 并行 Map 不随 drop 中止 | **修复** | `AbortOnDrop`（`flow.rs:1476-1482, 2007-2015`）；测试 `:4221` |
| D7 | 状态池大小估算 O(N²) | **未动** | `notify_state_size` 每个节点后全量序列化（`flow.rs:100-108` + `state_size.rs:43-54`） |
| P0-4 | retry 丢 partial | **未动** | `retry_executor.rs:118-132` attempt>0 时仍包成 `RetryExhausted`，消费方只匹配裸 `NodePartialExecutionFailed`（`flow.rs:464,656`）；builder 空列表策略会重试一切含 partial（`yanshi-async-util/src/retry.rs:74-77`） |
| — | 示例 validate 失败 | **修复（改示例，不是改语言）** | `expr.rs:640,659` 仍只认 `&&`/`\|\|`；示例改写为单比较；CI `quality.yml:607-616` 用 `--features mcp` 验证两个示例目录 |
| — | HttpNode 4xx/5xx 返回 Ok | **修复** | `yanshi-nodes/src/nodes/http.rs:100-116`；5xx/429 可重试；测试 `:185,209` |
| — | ArxivNode 明文 + 无界 | **修复** | `arxiv.rs:73-75` 50 MiB / 200 MiB 上限，有界 gunzip，https |
| — | HttpTool `text()` 无字节上限 | **未动** | `yanshi-tools/src/builtin/http.rs:145` 全量下载后才按 `max_response_chars` 截断 |
| — | resume 强制串行 | **未动** | `flow.rs:323-326` |
| — | inputs 覆盖同名参数只告警 | **未动（设计如此）** | `schema.rs:142-175` |
| — | While 条件看不到节点输出 | **未动** | `flow.rs:1290-1294` 用空状态池；反向地 `run_if` 看不到工作流 inputs（`:1653`） |

### 2.5 能力层（§4.4、§2.3）

| # | 缺陷 | 状态 | 证据 |
|---|---|---|---|
| L1 | Google 流式空流 | **修复（一处残留）** | `google.rs:192` 加 `?alt=sse`；`:666` 剥 `data:`；fixture `sse_buffer_regression.rs:170` 用真实 CRLF 帧。残留见 §3 COR-4 |
| L2 | 按网络块 `from_utf8_lossy` | **修复** | 5 家都走 `providers/line_buffer.rs`，`\n` 之后才解码；单测 `line_buffer.rs:30` 覆盖多字节跨块 |
| L3 | Retry-After 无上限 | **部分** | `llm_client.rs:41` 60s 上限、`:83` 超长直接拒绝；仍无取消令牌 |
| L4 | TPM 未执行；重试绕过限流桶 | **未动** | `model_registry.rs:377` 只建 RPM 桶；`until_ready()` 在 `retry_transient` 之外（`llm_client.rs:231/239` 等）；`modality_dispatch.rs` 零限流调用 |
| L5 | MCP `tools/call` 重放；通知队列堵死 | **修复，无测试** | `session.rs:441` 强制 `max_retries=0`；`transport/mod.rs:48` `try_send` 满则丢弃告警 |
| L6 | text_to_video 无上下界 | **部分** | `text_to_video.rs:120-131` 钳制轮询参数，有测试；仍无模型白名单 |
| L7 | `FailWorkflow` 名不副实 | **修复** | 变体已删（`collector.rs:101` 只剩 `Ignore\|LogError`） |
| L8 | 运行时启动后 `set_var` | **修复** | `main.rs:45` 在 `Builder::new_multi_thread()` 之前调用 `unsafe fn apply_env_overrides` |
| — | BM25 CJK 召回为零 | **修复** | `bm25.rs:401-432` 汉字/假名连续段切重叠二元组，ASCII 不变；索引和查询同一 `tokenize`；测试 `:452,475` |
| — | `hybrid_search` 忽略 alpha | **修复** | 方法已删；`HybridRetriever::search`（`hybrid.rs:128-181`）在 RRF 上应用 alpha，测试 `:270-325` |
| — | MMR 用 Jaccard | **未动（有改善）** | `reranking/mod.rs:74-90` 仍 Jaccard，但改用二元组 token，能识别 CJK 近重复 |
| — | SemanticMemory N+1 | **修复** | `semantic.rs:218` 一次 SELECT + `:240-255` 一次 `IN` 取 top-k |
| — | `memory → rag` 耦合 | **未动** | `yanshi-memory/Cargo.toml:26` |

### 2.6 工程流程（§5）

| 项 | 状态 | 证据 |
|---|---|---|
| `yanshi-tool` / `tools-ai` / `xtask` 不在测试矩阵 | **修复** | `quality.yml:320-322` |
| 本地/CI 缺 DB 时静默通过 | **修复（CI）** | `quality.yml:343-349` `psql SELECT 1` 必连；本地仍按设计自跳过 |
| 无 `cargo doc -D warnings` | **修复** | `quality.yml:388-407` `rustdoc` job |
| 无覆盖率 / MSRV / cargo-deny | **未动** | 无 `deny.toml`、无 `rust-version`、所有 job 浮动 `stable`；只有 `cargo-audit` |
| `workspace.dependencies` 只收 4 个；多版本共存 | **未动** | 仍 4 个；`cargo metadata` 46 个 crate 多版本、62 个多余版本 |
| 版本散落 | **未动** | 18× `0.1.0`、7× `0.2.0`、1× `0.3.0-alpha`，tag 是 `v1.0.0-rc.1` |
| 改名无 BREAKING / 无 env 回退 | **部分** | `CHANGELOG.md:14-26` 有完整迁移表；不做 `AGENTFLOW_*` 回退是明确决定；39 处残留都在迁移文件注释里 |
| 垃圾文件 / 过时 md | **部分** | `_to_delete/`、`agents/paper_*` 已删；根目录 7 个 md；**`.claude/worktrees/` 下两个 6 月的目录仍带完整 `agentflow-*` 源码树** |
| CLAUDE.md 事实错误（9 条） | **基本修复** | 剩 "~190 registry models" vs 实际 212（`CLAUDE.md:297`）；L4 tracing 一节"Harness … not wired today"对 CLI 的 `JsonlEventSink` 路径（`harness/chat.rs:335`）表述偏严 |

---

## 3. 新发现

### 3.1 安全与治理边界

| # | 缺陷 | 证据 | 场景 |
|---|---|---|---|
| **SEC-1** ✔ | **`template` 节点能读进程环境变量**：`Tera::default()` 自带 `get_env` 函数，`template.rs` 没有注销；`template` 不是 host-reaching 类型，不受 `GatewayPolicy` 约束 | `yanshi-nodes/src/nodes/template.rs:20`；tera-1.20.1 `src/tera.rs:754`；`gateway_policy.rs:108` 只查 `HOST_NODE_TYPES` | 租户提交 `{{ get_env(name="DATABASE_URL") }}`，密钥进入状态池、事件流、trace、checkpoint |
| **SEC-2** ✔ | **`markmap` 节点向任意路径写文件**：`save_to_file` 直接 `tokio::fs::write`，路径来自提交的 YAML，不经 `check_path`，节点类型不在名单里 | `yanshi-nodes/src/nodes/markmap.rs:129-136`；`factory.rs:195-203` | 租户以网关 uid 覆盖任意可写文件 |
| **SEC-3** ✔ | **`rag.qdrant_url` SSRF**：租户任选 URL，没有 HttpTool 那套私网地址过滤；`rag` 不在名单里 | `factory.rs:365-369`；`yanshi-nodes-ai/src/nodes/rag.rs:64,95-96` | 打内网 Qdrant / 元数据服务 |
| **SEC-4** ✔ | **`http`/MCP/模态工具的审批缓存摘要为 `None`**：`approval_cache_digest` 只对 shell/code_exec/script/file 计算摘要，其余 `_ => None`，缓存键退化为 `(tool, None, scope)` | `yanshi-harness/src/hooks_runtime.rs:342`；查找 `:279-283,575-599` | 一次 Session 范围放行 `POST a.com`，等于放行之后所有 `DELETE b.com` |
| SEC-5 | plugin 子进程继承宿主全部 env（S5 的残余） | `yanshi-core/src/plugin/host.rs:168-176`；`dry_run.rs:80-86`；两个 preparer 都没 `env_clear` | 运维一旦开启 `plugin` 类型，env 外泄重新打开 |
| SEC-6 | production 下 legacy 单 token + `X-Yanshi-Tenant` 自报租户仍放行 | `auth.rs:94-108`；`tenant.rs:21-25,98-100`；Helm 默认只设 `YANSHI_API_TOKEN`（`values.yaml:8-11`） | 共享 token 的任何持有者可冒充任意租户 |
| **SEC-7** ✔ | **`AgentTool` 嵌套子 agent 完全绕过治理**：`context_for_input` 新建上下文，无父级取消令牌、`timeout_ms: None`、`max_tool_calls: None`、无 event sink；子 agent 的注册表不经 `wrap_registry`；子 agent 的 `PolicyDeniedAndStop` 被折成 `ToolOutput::error` | `yanshi-agents/src/tools/agent_tool.rs:121-124`；`react/agent/prompt.rs:18-31` | 外层 DenyAndStop 对子 agent 无效；子 agent 不受父级限额 |
| SEC-8 | Skill 清单自己授权自己要 spawn 的 MCP 命令，且只比 basename | `yanshi-skills/src/builder.rs:535-547, 617-629`；无运维级覆盖 | 清单写 `bash` 或自带 `./bin/python3` 即通过 |
| SEC-9 | 工具输出被提升到 `system` 角色：内存摘要（由省略的工具结果构成）、项目事实（取自 LLM 选择的 shell/script 参数）、任务摘要都以 `MultimodalMessage::system()` 注入 | `react/agent/prompt.rs:259-282`；`support.rs:280-288` | 攻击者控制的工具结果在下一轮获得 system 权威 |
| SEC-10 | 未声明 `[[scripts]]` 的 skill 脚本不做 hash 校验 | `builder.rs:388-400`；`script.rs:239-266` | 空清单 = `ScriptTool` 全 profile 宽松 |
| SEC-11 | `/v1/diagnostics` 对任意租户暴露 `skills_root`/`plugins_root`/MCP `command` | `yanshi-config/src/diagnostics.rs:75-115` | 泄露运维主机布局 |

已核对为健全：bearer 比较恒定时间（`auth.rs:247-256`）；请求体全局+按路由限额；marketplace tar 解压拒 symlink/`..`/绝对路径且限大小（`marketplace/mod.rs:448-568`）；HttpTool/HttpNode 逐跳钉 DNS、归一 v4-mapped v6、默认拒 loopback/私网/link-local/metadata；code_exec 容器 `--read-only --network none` 非 root 带资源限额；FileTraceStorage 对 run id 校验后才碰文件系统。

### 3.2 正确性与可靠性

| # | 缺陷 | 证据 |
|---|---|---|
| **COR-1** ✔ | **OpenAI 兼容流式永远没有 `usage`**：请求不带 `stream_options.include_usage`（全仓零命中），解析器要求 `choices.first()`，`finish_reason` 后即停止读取。ReAct 无条件 `execute_streaming_collected`（`core.rs:1169`），`cumulative_cost_usd` 对 OpenAI/Moonshot/StepFun/GLM/DashScope/DeepSeek/MiniMax 累加的永远是 0，`cost_limit_usd` 失效 | `openai.rs:614-615`；`moonshot.rs:335`；`core.rs:1240-1262` |
| **COR-2** ✔ | **`token_budget` 在默认内存下不可触发**：预算检查读 `memory.session_token_count`，而 `SessionMemory::add_message` 每次写入都按窗口（默认 8000）裁剪，计数永远 ≤ 8000；真实 `usage` 只写进事件，从不累加。无 `TokenBudgetExceeded` 测试 | `core.rs:1413-1414`；`yanshi-memory/src/session.rs:34-36,66-76` |
| COR-3 | 跨进程 resume 丢对话：`resume_from_loop_checkpoint` 恢复 `LoopState` 但不重建内存；服务端默认 `SessionMemory::default_window()`，`interrupt/answer` 之后 prompt 里只有答案。crate 内测试靠共享内存掩盖了这一点 | `core.rs:498-530`；`harness_live.rs:560`；`tests.rs:471-535` |
| COR-4 | Google 流式吞掉错误体：非 `GoogleResponse` 的行（如 `{"error":...}`）被 `.ok()?` 丢弃，流以 `Ok(None)` 正常结束，调用方看到"成功的空回答" | `google.rs:670,755-758` |
| COR-5 | DAG 取消只在节点完成之间被观察：令牌是裸 `AtomicBool`，无 waker，节点本身收不到令牌；长节点期间取消要等它自己返回 | `flow.rs:841-853,1026`（并发）、`:349-361`（串行）；`scheduler.rs:131-150` |
| COR-6 | `persist_step_result` 的 I/O 错误直接 `?` 中止，不发 `WorkflowFailed`、不落终态 checkpoint；并发路径静默丢弃在飞的 future | `flow.rs:494, 921, 964, 1027` |
| COR-7 | "可选"的未声明输入源在失败时并不可选：`gather_inputs` 对任何失败的源都返回 `Err`，与 `:1676-1680` 的注释矛盾 | `flow.rs:1620` vs `:1597-1631` |
| COR-8 | D5 的隐式边是文本匹配且不校验：`run_if` 扫描任何 `nodes.<id>` 子串（含字符串字面量）并转成硬边；引用传递依赖者会在运行时报 `CircularFlow`，但 `workflow validate` 完全没有环检测 | `flow.rs:1693-1702,1744-1747,1774`；`schema.rs:110-131` |
| COR-9 | While 出口节点输出合并顺序不确定：`find_exit_nodes` 遍历 `HashMap`，同名 key 后写者胜 | `flow.rs:1660-1672,1319-1347` |
| COR-10 | 子流程节点事件带新 UUID `workflow_id`（每个 Map 项/While 迭代都是 `execute_with_workflow_id(None, …)`），按 run id 过滤的 listener 看到孤儿节点事件；D3 只修了生命周期事件 | `flow.rs:1364-1366,1436-1438 → :230-236` |
| COR-11 | embedding 批次不校验对齐：`data[].index` 解析后不用，`data.len()` 不与批次长度比较；厂商少返回或乱序会让之后每个 chunk 的向量错位 | `yanshi-rag/src/embeddings/openai.rs:56,258,269` |
| COR-12 | 流式工具调用空参数变 `Null`、非流式变 `{}`；Anthropic 无 `input_json_delta` 的 `tool_use` 以 `Null` 派发 | `client/streaming.rs:169`；`anthropic.rs:629` |
| COR-13 | Anthropic 静默丢弃 `role:"tool"` 消息（`_ => {}`），目前 agent 不喂原生 tool 消息，属潜伏 | `anthropic.rs:99` |
| COR-14 | 批量派发是"断路器"而非预算：`counter + n > max_tool_calls` 时整轮以 `MaxToolCalls` 停机，即使 `counter == 0` | `batch.rs:71-96` |
| COR-15 | 审批等待在 hook 层无超时，`expires_at` 仅是建议；hook panic 没有 `catch_unwind`，会解开会话线程 | `hooks_runtime.rs:640, 614-616, 490` |
| COR-16 | 内存摘要丢工具结果：`CompactMemorySummary` 只留前 8 条被省略消息各 160 字符；`RecentOnly` 一条不留；8000 窗口下最先被挤出的是用户目标 | `support.rs:274-293`；`config.rs:104-110`；`session.rs:53-54` |

### 3.3 平台 / HA / 运维

| # | 缺陷 | 证据 |
|---|---|---|
| **PLT-1** ✔ | **连接池饥饿是设计出来的**：主库池硬编码 8（`serve.rs:469`，有副本时 8/16，不可配置），启动时 5 个 NOTIFY 监听器（run 事件、harness 事件、run 取消、harness 取消、审批决策）各通过 `PgListener::connect_with(pool)` 长期占一条；剩 3 条给所有 handler、执行器、lease 循环和清理，`acquire_timeout` 3s。lease 续约因拿不到连接失败 120s 后，同一 tick 的回收会把本副本自己的活 run 判死 | `serve.rs:462-469, 554-583`；`yanshi-db/src/notify.rs:47`；`database.rs:35`；`runs.rs:360-369` |
| **PLT-2** ✔ | **SSE 回填超过 1000 条静默截断**：循环在 1000 处 break，之后 `live_events(receiver, after_seq)` 只给 broker 上 `> after_seq` 的新事件，已持久化的第 1001..N 条永远不发，也不发 `lagged` 提示。注释写"客户端可以重连"，但重连同样从头截断 | `events_stream.rs:476-480` |
| PLT-3 | harness 会话没有 lease：副本死亡或 SIGTERM 后会话永远 `running`；`HarnessCancellationGuard::Drop` 只覆盖进程内 drop | `harness.rs:678-706`；`cleanup.rs:229-232` |
| PLT-4 | seq 分配走只读副本 `MAX(seq)+1` 裸 INSERT（P2 残余） | `runs.rs:1680,1706`；`harness.rs:1468`；`repo.rs:611,659,1092` |
| PLT-5 | 配了 `YANSHI_DATABASE_READ_URL` 时，回填从副本读而订阅在前，主库已有、副本未到、broker 已发的事件丢失 | `events_stream.rs:458 vs 469`；`repo.rs:594/692` |
| PLT-6 | 分布式：worker 在首次心跳前死亡则任务永久挂起（`requeue_stale_tasks` 跳过无心跳记录的 assignment；`claim_task_with_hints` 插 assignment 不插心跳），叠加无 run deadline | `scheduler/distributed.rs:187-189`；`mod.rs:100-135` |
| PLT-7 | 优雅停机只排空 HTTP：`with_graceful_shutdown` 之后不 join/abort 已 spawn 的执行任务 | `serve.rs:600-634` |
| PLT-8 | 构建输入浮动：Dockerfile 基础镜像无 digest；UI e2e 用 `npm install` 而非 `npm ci`；`package.json` 全 caret | `Dockerfile:3,18`；`ui-e2e.yml:111` |
| PLT-9 | 无 worker Helm 模板；CI 无 `helm lint/template` | `charts/yanshi/templates/` |
| PLT-10 | trace 脱敏跳过 `NodeTrace.error` 和 `ToolCallTrace` 结果字段，错误串里的 header/URL 原样落盘 | `yanshi-tracing/src/redaction.rs:55-77`；`types.rs:184` |
| PLT-11 | Streamable-HTTP MCP 客户端整体缓冲 SSE body 再解析，进度通知只在最终响应后到达 | `transport/streamable_http.rs:136-141,154-170` |
| PLT-12 | Qdrant `add_documents` 一次 gRPC 上传整批，不分块 | `vectorstore/qdrant.rs:294-307` |

### 3.4 工程卫生与文档

| 项 | 证据 |
|---|---|
| ✔ `yanshi-cli` 默认 feature 不含 `mcp`，`cargo install` 用户拿到的二进制跑不了 4 个自带示例；CI 用 `--features mcp` 所以绿 | `yanshi-cli/Cargo.toml:28`；`quality.yml:611` |
| ✔ `docs/CURRENT_STATUS.md` 停在 2026-08-09，仍称 `yanshi-tools` 是 Tool 契约（T3.3 之后是 `yanshi-tool`） | `CURRENT_STATUS.md:3,26` |
| ✔ 12 篇 docs 不在 `SUMMARY.md`，因此不发布：三份评估报告、`TRIAL_GUIDE.md`、`TERA_TEMPLATE_GUIDE.md`、`ARCHITECTURE_EVALUATION_2026-06-20.md`（RoadMap 和 CLAUDE.md 都指向它）等 | `docs/SUMMARY.md` |
| `docs/ARCHITECTURE.md` 2026-07-23、`docs/HARNESS_MODE.md` 2026-05-15，早于 P-A/W 段的大部分改动 | 文件头 |
| `.claude/worktrees/` 两个 6 月目录含完整 `agentflow-*` 源码树（tokei 统计时会把它们算进去） | `find . -name '*.rs' -path '*agentflow*'` |
| `yanshi-core/src/flow.rs` 5339 行（2016 行生产 + 3323 行测试），串行执行器、并发调度、Map、While、checkpoint 编解码、输入收集、拓扑排序、`FlowExt` 门面同住一文件；W5.4 对 `react/agent.rs` 做的按关注点拆分没有用到这里 | `flow.rs:2017` 唯一 `#[cfg(test)]` |
| `TODOs.md` 标题仍是 "AgentFlow TODOs"；活跃队列已清空 | `TODOs.md:1` |
| `proc-macro-error2 v2.0.1` future-incompat 提示（10-07 已有） | clippy/doc 输出 |

---

## 4. 工程质量亮点（保持）

- 生产代码零 `todo!`/`unimplemented!`；lib 级 `clippy::unwrap_used` 在 CI 强制。
- 3437 个测试零失败；10-07 的 12 个失败（容器服务未起）这次真跑通过。跨副本行为有 5 组以上真实 PG 测试。
- 每一条已修复的 10-07 缺陷几乎都附带了"修复前失败、修复后通过"的回归测试；少数例外（S6 旧式 syscall、L5 MCP 单发、A3 abort）已在上文标出。
- `check-arch` 机制本身仍是 A 级：0 新增违规、0 过期条目。
- `release-gate` 聚合 19 个 job；`quality.yml` 新增 rustdoc、DB 必连、三个 crate 的测试、示例 validate、`examples-smoke`。
- 最近 79 个提交 100% 符合 Conventional Commits，每个修复都能追溯到评估编号。
- CHANGELOG `[Unreleased]` 有完整的改名迁移表和 4 条 BREAKING 说明。

---

## 5. 修复路线（按收益排序）

### 第一批：把网关防线从"枚举节点类型"改成"约束能力"（不做就不能对多租户开放）

1. **SEC-1/2/3 一起修**：`template` 注销 `get_env`（`tera.register_function("get_env", …)` 覆盖为报错，或改用 `Tera::new` 不带默认函数）；`markmap.save_to_file` 和 `rag.qdrant_url` 走 `GatewayPolicy::check_path` / HttpTool 的地址过滤。顺手把"任何带 path 或 URL 参数的节点"写成 `GatewayPolicy` 的通用校验，而不是再加三个特例。
2. **SEC-4**：`approval_cache_digest` 对 `http` 以 `method+url` 计摘要，MCP/模态工具以全参数 JSON 计摘要；`_ => None` 改成"没摘要就不进缓存"。
3. **SEC-7**：`AgentTool` 把父级 `AgentContext`（取消令牌、剩余 deadline、工具调用池）传给子 agent；子 agent 的注册表经同一 `wrap_registry`；`PolicyDeniedAndStop` 原样上抛。
4. **SEC-5**：plugin 子进程沿用 shell/script 的 `env_clear` + 白名单。
5. **SEC-6**：production profile 拒绝 legacy 单 token；或至少在该模式下忽略 `X-Yanshi-Tenant`。

### 第二批：让预算真的生效

6. **COR-1**：OpenAI 兼容请求加 `stream_options: {include_usage: true}`，解析器接受 `choices: []` 的 usage 帧，`finish_reason` 之后继续读到流结束。补一条 fixture：最后一帧只有 usage。
7. **COR-2**：ReAct 把每次 `usage.total_tokens` 累加到 `LoopState`，预算检查用累加值而不是内存计数；补 `TokenBudgetExceeded` 测试。
8. **COR-3**：`resume_from_loop_checkpoint` 从 checkpoint 的 `steps` 重建内存（`restore_trace_memory` 已经存在，只是没在这条路径上调用）。
9. **COR-4**：Google 流式遇到不可解析行时返回 `Err`，而不是 `.ok()?`。
10. **A7**：`record_reflection` 用 `race_with_limits` 包起来，出错降级为日志；或者既然没有入口，先把它从停机路径上拿掉。

### 第三批：平台容量与正确性

11. **PLT-1**：NOTIFY 监听器用独立连接（`PgListener::connect(url)`）不占池；池大小可配置。重估条件：`yanshi_db_pool_acquire_timeout_total` 这个指标（需要先加）在压测下非零。
12. **PLT-2**：回填分页直到终态或追上 broker，去掉 1000 硬顶；或至少发一个 `lagged` 事件让客户端知道。
13. **PLT-3**：harness 会话加 lease，复用 run 的 `0011` 模式。
14. **PLT-4**：seq 分配统一走执行器的原子计数器；非执行副本的取消事件通过 NOTIFY 让执行副本写。
15. **COR-5/6**：取消令牌用 `tokio::sync::Notify` 或 `CancellationToken` 并在 `running.next()` 上 `select!`；`persist_step_result` 失败发 `WorkflowFailed` 再返回。

### 第四批：工程卫生

16. `yanshi-cli` 默认 feature 加 `mcp`（它已经是 `yanshi-config` 的 feature，示例依赖它）；或者把 mcp 示例移到单独目录并在 README 说明。
17. `validate` 加环检测（复用 `topological_sort`，把 D5 的隐式边算进去）。
18. 刷新 `CURRENT_STATUS.md`、把 `ARCHITECTURE_EVALUATION_2026-06-20.md`、`TRIAL_GUIDE.md`、三份评估加进 `SUMMARY.md`；CLAUDE.md "~190" 改 212；删 `.claude/worktrees/` 两个目录。
19. 加 `rust-version`、`deny.toml`；把 reqwest/prost/rustls 这类多版本 crate 收进 `[workspace.dependencies]`。
20. 给 S6 旧式 syscall 名单、L5 `tools/call` 单发、A3 父超时子中止各补一条测试。

### 重估条件（当前版本暂不做）

| 不做的项 | 什么时候重估 |
|---|---|
| D7 状态池 O(N²) 估算 | 一个 50 节点、单节点输出 1 MiB 的工作流里 `notify_state_size` 总耗时超过节点执行总耗时的 5%（先加一个计时再说） |
| 后台任务（A8）五项 | 有任何 CLI/server 入口要注册 `task_create` 的那一刻，整组一起做 |
| 分布式控制面持久化（P6） | 单网关 + 演示规模之外有真实需求；在那之前先修 PLT-6 的永久挂起 |
| MMR 换 embedding 余弦 | RAG eval 的 nDCG 对比显示 Jaccard-MMR 比不做 MMR 还差 |
| events 表分区 | 同 10-07：`pg_total_relation_size('events')` > 10 GB 或单次 cleanup DELETE > 30s |
| 再激活依赖律 | 10 条潜在边降到 3 条以下时逐条激活 |

---

## 6. 生产就绪结论

| 部署形态 | 结论 |
|---|---|
| 库集成 / 单机 CLI（可信操作者） | **可用（A−）**。注意：`cost_limit_usd` 和 `token_budget` 现在都不可靠（COR-1/2），不要依赖它们控费；checkpoint 仍不可达（D4） |
| 单节点网关 + 单一可信租户 | **可用（B）**。前提：`securityProfile=production`（Helm 默认）、不开 `plugin`、连接池问题（PLT-1）在低并发下不会触发、回填不超过 1000 条事件 |
| 单节点网关 + 互不信任的多租户 | **仍不建议**。旧攻击链已关，但 SEC-1/2/3 是三条无需 opt-in 的越界，SEC-6 允许冒充租户。第一批修完后可重估 |
| 多副本 | **基本可用（B−）**。取消/审批/事件补偿都有真实 PG 测试；harness 会话无 lease（PLT-3）、seq 竞态（PLT-4）、副本回填空窗（PLT-5）是已知缺口 |
| 分布式 worker | **实验级（D+）**。只适合单网关加演示规模；PLT-6 的永久挂起是最先会撞上的 |

---

## 7. 按产品定位重新加权（2026-11-04 补充）

项目目标是**面向多模态业务的智能体框架**（文章→博客、想法→剧本→视频），不是编码类智能体。按这个目标重看，§1–§6 里的权重要调整：安全沙箱、seccomp、code_exec、分布式 worker 是过去三个月的投入重心，但对这个目标它们是配角；真正决定成败的是**媒体资产怎么在节点间流转、按次计费的调用怎么控成本、十步里失败在第九步怎么不重花钱**。

### 7.1 对这个目标已经具备的

| 能力 | 现状 | 证据 |
|---|---|---|
| 模态覆盖 | TTS 5 家、文生图 5 家、ASR 2 家、图编辑 2 家、图生图 1 家（StepFun）、文生视频 1 家（Google Veo）；7 种模态都有 DAG 节点和 agent 工具 | `grep 'impl .*Provider for'`；`yanshi-nodes-ai/src/nodes/`；`yanshi-tools-ai` |
| 结构化输出 | `json_mode()` / `json_schema(name, schema)` 在 fluent builder 上，剧本/分镜这类结构化生成可直接用 | `yanshi-llm/src/client/llm_client.rs:843-875` |
| 编排原语 | Map 并行（一篇文章扇出到 N 个平台）、While、run_if、Tera 模板节点、Skill 打包 persona、RAG 作品牌知识库 | — |
| 输入模态 | 文本 / 图片（全家）/ 音频（`ContentType::Audio`）/ 视频与 PDF（Google / Anthropic，P-LLM2.4） | `providers/mod.rs:245-254`；`google.rs:303-306` |
| 面向媒体的 dogfooding 应用 | `blog-to-podcast`（Plan A 薄壳已跑通）、`podcast-mastering`（7 步 ReAct + 6 次 MCP 调用 live 跑通）、`meeting-transcriber` | `EXAMPLES_TODOs.md:98-104, 233-240` |

### 7.2 对这个目标是硬伤的

| # | 问题 | 证据 | 为什么对这个目标致命 |
|---|---|---|---|
| **M1** ✔ | **媒体以 base64 data URI 塞在 JSON 状态池里**：`tts` / `text_to_image` 输出 `FlowValue::Json(String("data:…;base64,…"))`，L0 的 `FlowValue::File` 没有任何节点构造它；`text_to_video` 只返回厂商临时 URL，不落盘；`file` 节点和 `FileTool` 不解码 data URI | `tts.rs:127-134`；`text_to_image.rs:326`；`text_to_video.rs:171,224`；`grep 'FlowValue::File {'` 在 nodes 里只有读没有写 | 每个节点完成后全量序列化状态池（D7）在 MB 级 blob 上是平方级；checkpoint、trace、事件流、DB `output`、SSE 推到 UI 的都是整段 base64；`resource_limits` 告警会频繁触发；生成一张图想存到磁盘都要自己写节点 |
| **M2** ✔ | **按次计费的调用没有成本账**：`PricingTable` 只有 token 单价，图/TTS/视频调用计 0；而 token 这一侧在流式下对 7 家厂商也是 0（COR-1）、`token_budget` 不可触发（COR-2） | `yanshi-agents/src/eval/pricing.rs` 无 image/tts/video 项；`core.rs:1252-1262` | 这类产品每一步都是付费厂商调用，成本控制是第一治理需求，现在是最弱的一环 |
| **M3** ✔ | **失败后不能从断点续跑**：checkpoint 没有任何 CLI/server 入口（D4），retry 策略会原样重调厂商、没有幂等键 | `flow.rs:598-606`；无 `with_checkpointing` 产品调用方 | 十步视频管线在第九步失败，重跑要把前八步的钱再花一遍。对编码 agent 这是"nice to have"，对这个目标是核心 |
| M4 | **没有产物检索接口**：`ArtifactRepo` 在 DB 层存在，server 没有任何 artifact 路由；生成的媒体只能从 run 的 `output` JSON 里抠 base64 | `grep ArtifactRepo yanshi-server/src` 零命中 | 产品形态是"交付一段视频/一篇带图博客"，但网关交不出文件 |
| M5 | **没有媒体处理层**：全仓零 ffmpeg / mux / 字幕 / 裁剪（唯一的 `ffmpeg` 命中在 gRPC 测试里）；只能通过外部 MCP（如 phonon） | `grep -ri ffmpeg yanshi-*/src` | "剧本→视频"要拼镜头、配音、字幕；框架不必自带，但要有明确的扩展点和一个官方示例 |
| M6 | **视频生成厂商只有 Veo**；图生图只有 StepFun；DashScope 有 Wan 文生图但没接视频；MiniMax / Kling 没有 | §7.1 表 | 国内业务场景的视频模型覆盖不够；好在 `modality_dispatch` 是表驱动，补一家是小改 |
| M7 | 聊天回复里的图片输出不解析（P-LLM2.6 只加了元数据），Gemini 图文混合回复的图片被丢 | CLAUDE.md 自述；`ContentType::Image` 无响应侧构造 | "边聊边出图"的交互做不了 |
| M8 | 应用轨停滞：`examples/applications/README.md` 7 个应用全标 TODO，`EXAMPLES_TODOs.md` 停在 2026-05-17；而同期 Harness / 分布式 / HA 走了 W0–W5 五段 | 文件头与状态表 | 没有一个 DONE 的端到端媒体应用在驱动框架补缺口；§7.2 的 M1–M4 正是"真做一个就会撞上"的那种 |

### 7.3 治理模型的错位

当前治理围绕"危险 = 碰主机"（shell / 文件 / 网络 / 代码执行）设计。对这个目标，危险动作是**花钱**（每次生成调用）、**发布**（推到博客/平台）、**覆盖资产**。现状：模态工具的幂等性是 `Unknown`，production 下每次调用都要审批——正确但粗糙；没有"本次运行预算超过 X 元时才问我"这种按预算的审批；没有发布类动作的概念。`template` 节点能读 env（SEC-1）在这里反而更要紧，因为它是这类产品的核心节点。

### 7.4 按这个目标重排的路线

第一批（做完才算"能搭媒体产品"）：
1. **媒体作为一等公民**：模态节点把字节写进 run_dir，输出 `FlowValue::File { path, mime }`（L0 类型和 checkpoint 保真都已经有了，缺的只是生产者）；`text_to_video` 下载落盘；`file` 节点 / `FileTool` 接受 `File` 值；状态池只存路径和哈希。这是 §5 里 D7 的真正解法。
2. **成本账**：`PricingTable` 加按张 / 按秒 / 按千字符的单价；修 COR-1 / COR-2；YAML 和 server 请求上加 run 级 `cost_limit`；成本进事件流和 UI。
3. **断点续跑接入 CLI 和 server**（D4）：`workflow run --checkpoint` + `resume` 不丢 inputs；付费节点成功即落 checkpoint。
4. **server 加 artifact 路由**：`GET /v1/runs/{id}/artifacts` 列表 + 下载，复用 `ArtifactRepo`。
5. **把 `blog-to-podcast` 或"文章 → 博客 + 封面图 + 配音"做到 DONE**，以它暴露的缺口为准修框架，而不是按评估表修。

第二批：视频/图生图厂商补齐（DashScope Wan 视频、MiniMax、Kling）；聊天回复图片解析（M7）；媒体处理扩展点 + 一个 ffmpeg MCP 示例；按预算的审批策略；SEC-1/2/3（便宜，且 `template` 是核心节点）。

降权（对这个目标暂不投入）：分布式 worker、多副本 HA 的剩余缺口、Harness 的编码类上下文提供者、seccomp 细化、后台任务。它们不是错的，但在有一个 DONE 的媒体应用之前，再投入是在解不存在的问题。

### 7.5 重新加权后的评级

| 维度 | 评级 | 一句话 |
|---|:---:|---|
| 模态能力广度 | B+ | 七种模态都有节点和工具，结构化输出齐全；视频只有一家 |
| 媒体资产流转 | **D** | base64 进 JSON 状态池，没有 File 生产者，没有产物接口 |
| 成本控制 | **D** | 模态调用计 0，token 流式计 0，预算不可触发 |
| 失败恢复（付费管线） | **C−** | 机制齐全但产品入口没接 |
| 编排表达力 | A− | Map / While / run_if / 模板 / Skill / RAG 够用 |
| 端到端验证 | C | 两个媒体应用跑通过，但没有一个到 DONE |
| **面向该目标的综合** | **B−** | 底座扎实，但产品最需要的三件事（媒体流转、成本、续跑）恰好是最薄的 |

### 7.6 对照 NotebookLM Studio 形态的逐项差距（2026-11-04 补充）

产品形态：来源（想法 / 链接 / 文件）→ 落地为一组固定类型的成品。按"生成结构化内容"和"把结构化内容渲染成文件"两段拆，每种成品的现状：

| 成品 | 生成（LLM 侧） | 渲染（文件侧） | 差距 |
|---|---|---|---|
| 报告 | llm + json_schema + Tera 模板 ✅ | Markdown ✅；PDF/DOCX ❌（`lopdf` 只读） | 渲染器 |
| 思维导图 | llm ✅ | `markmap` 节点出 HTML ✅；PNG/SVG ❌ | 栅格化 |
| 闪卡 / 测验 / 数据表格 | json_schema ✅ | JSON ✅；CSV（`csv` crate 在依赖里但无节点）；xlsx ❌ | 轻量渲染节点 |
| 音频概览 | 大纲 → 双人剧本（json_schema）✅ → 逐句 TTS（Map 并行）✅ | 音频拼接 / 响度 ❌（零 ffmpeg；`podcast-mastering` 示例靠外部 phonon MCP） | 媒体处理层 |
| 视频概览 | 剧本 ✅ → 分镜图（text_to_image）✅ → 配音 ✅ | 合成 / 字幕 / 转场 ❌ | 媒体处理层 |
| 演示文稿 | 结构化大纲 ✅ | pptx ❌ | 渲染器 |
| 信息图 | 结构化数据 ✅ | SVG 模板（Tera 可出 SVG 文本）→ PNG ❌（`plotters` 在依赖里但无节点） | 栅格化 |
| 领域工程图 | LLM 出 Mermaid / PlantUML / D2 文本 ✅ | 校验 + 渲染 ❌ | DSL 校验-渲染-回灌循环 |
| 来源接入 | PDF/HTML/CSV/文本 loader ✅、URL 抓取（带 SSRF 过滤）✅、音视频经 ASR ✅ | — | `Bm25KnowledgeBackend` 纯内存（`knowledge.rs:77`），每次请求重建；没有"笔记本"实体持久化来源 |
| 引用溯源 | `yanshi-agents/src/citation.rs` 只服务 agent 循环；`rag` 节点带 `source` 元数据 | — | DAG 生成器的输出没有统一的引用结构 |
| 成品交付 | — | 无 artifact 路由（M4）；媒体 base64 进状态池（M1） | 产物模型 |
| 人工确认（大纲确认后再生成） | Harness 有 `AwaitingInput` / interrupt | — | DAG 没有 HITL 节点；可用"两次 run"绕过 |

**规划层的取舍**：这种产品里生成器应当是**手写的、带类型化 inputs 的 DAG 模板**（`FlowDefinitionV2.inputs` 已支持），LLM 规划的是**内容层**（大纲、选哪些成品、风格参数），不是图结构。`WorkflowTool` 已能把每个生成器暴露成规划 agent 的工具，`DynamicWorkflowAgent` 留给长尾的"更丰富"需求。理由：成本可预估、质量可回归测试、失败可定位；图级动态规划三者都做不到。

**安全投入在这里重新有了意义**：渲染器（ffmpeg、mermaid-cli、pandoc、D2）天然是外部二进制，正好跑在已经建好的沙箱 shell / MCP 工具层里。
