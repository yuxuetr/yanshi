# Yanshi TODOs

Last updated: 2026-08-05

## 维护约定

- 旧执行计划按时间分批归档到 `docs/archive/`：
  - `TODOs-archive-2026-05-09-n1-n10.md` — N1–N10 路线图段（已闭环）。
  - `TODOs-archive-2026-05-10-p0-p4.md` — 早期 P-段执行计划（已闭环）。
  - `TODOs-archive-2026-05-19-recently-closed.md` — 5/19 从 Recently Closed
    扫出去的中段历史。
  - `TODOs-archive-2026-05-20-closed-segments.md` — 12 个全 closed 的 P-段
    （P0/P1/P2/P3/P4/P5/P6/P7/P-H/P9/P-LLM/M）整体外迁。
  - `TODOs-archive-2026-05-24-p10-optimization-backlog.md` — P10 优化 backlog
    （v1.0.0-rc.1 ops + 19 个 crate-level 子段），全部 DONE 项 + 少量未拾起的
    polish。
  - `TODOs-archive-2026-06-20-q1-q5-audit-remediation.md` — Q1–Q5 五段
    （2026-05-24 深度审计修复波次）全部闭环（108 DONE / 0 TODO）整体外迁，含
    Audit Assessment Summary。源审计仍在 `docs/audit/`。
  - `TODOs-archive-2026-07-28-pre-audit-remediation-snapshot.md` — 7/28
    全量快照：H / P-A / S / L 四段全部 DONE/DEFERRED 收口后、启动 R（工程化
    审计修复）段之前的完整存档（含全部历史明细）。
  - `TODOs-archive-2026-07-29-post-r-pre-t-snapshot.md` — 7/29 全量快照：
    R 段（2026-07-28 工程化审计修复）收口后、启动 T（2026-07-29 架构评估
    修复）段之前的完整存档。
  - `TODOs-archive-2026-07-30-post-t-pre-u-snapshot.md` — 7/30 快照：T 段
    （2026-07-29 架构评估修复，T0–T4 十五项，14 DONE / 1 DEFERRED）全部决策
    完毕收口后、启动 U（2026-07-30 复核评估修复）段之前的完整存档（含全部
    历史明细）。
  - `TODOs-archive-2026-08-05-pre-production-readiness-snapshot.md` —
    **本次 8/5 快照**：U 段（2026-07-30 复核评估修复，17 项，16 DONE /
    1 DEFERRED）全部收口后、启动新 **V（2026-08-05 生产就绪修复）**
    段之前的完整存档（含全部历史明细）。本文件即从这份快照重建。
- 本文件是短期执行队列。H / P-A / S / L / R / T / U 七段已全闭环并整体
  存档；当前仅保留 **V（2026-08-05 生产就绪修复）** 一个 backlog。
- 本次 V 段来源：`docs/PROJECT_EVALUATION_2026-08-05.md`——6 路并行子代理
  逐层通读源码 + 编排者本地实跑 `cargo test/clippy/fmt/tree` 的全量重新
  评审（非对上一轮修复效果的复核，而是从零开始的独立评估）。综合评级
  **B+ / A-（库质量），生产就绪度 C+**。核心结论：测试文化、CI 门禁、
  错误处理卫生（生产库代码 unwrap/expect 全干净）、沙箱/脱敏设计属 Rust
  开源生态第一梯队；但**执行引擎侧的安全强制未落地**（工作流/工具在
  server/worker 侧无沙箱强制执行）、**DAG 执行引擎的终态语义与 resume
  不健全**、**部分可靠性栈（retry/timeout/health）是未接入执行器的门面
  模块**。V 段按 evaluation 报告"指导原则：核心优先，服务次之"排序：
  核心正确性/稳定性 > 核心能力补齐 > 服务能力"够用即止"加固 > 文档卫生；
  但 file 节点任意读写、SSRF 两项因直接影响个人使用（LLM 驱动工具调用）
  从"服务加固"上提到与核心 bug 同批（V0）处理。
- `docs/CURRENT_STATUS.md` 记录当前已实现状态。
- `RoadMap.md` 保留中长期路线。
- `docs/PROJECT_EVALUATION_2026-08-05.md` 是本轮 V 段的评估依据，每个
  V-item 下方引用其对应章节（§五 代码成熟度 / §六 安全性 / §七 文档 /
  §八 优先修复建议）。
- 任务状态只使用：
  - `TODO`：未开始或正在执行。
  - `DONE`：已完成、已测试、已提交。
  - `DEFERRED`：显式推迟到 RoadMap Later Tracks 或 Non-Goals。

## Active Queue Overview

Current focus: **H / P-A / S / L / R / T / U 七段已全闭环并整体存档**
（见上）；**V（2026-08-05 生产就绪修复）V0–V4 全部 26 项已收口，V 段
整体闭环**（V0 段 5 项 + V1.1–V1.7 / V2.1–V2.4 / V3.1–V3.6 / V4.1–V4.4，
按 `docs/PROJECT_EVALUATION_2026-08-05.md` §八的优先级排序，**分段严格
串行**——V0 → V1 → V2 → V3 → V4，每段收口后跑全量门禁再进下一段；V2 段
内 V2.3/V2.4 顺序已按 2026-08-06 用户确认重排为 V2.4 → V2.3；V4.4 的
Semantic 分块器子项经调查后判定为独立架构工作、深挖后确认不在这一段
范围内，已 DEFERRED 且详细记录在 V4.4 条目里，不影响 V4 段整体收
口——V 段状态见下方 Segment 表）：
**V0（阻断性正确性/安全 bug）5 项**——已逐行确认、成本低影响高的
panic / 默认不安全配置 / 终态语义错误 / 路径穿越 / 鉴权缺口；
**V1（核心执行/Agent 正确性与健壮性）7 项**——DAG 终态与 resume 语义、
取消竞态、可靠性栈接线、LLM 重试、记忆运行时集成、日志规范化；
**V2（核心能力补齐）4 项**——结构化输出、token 级流式、通用 HITL
中断/恢复、agent 循环级 checkpoint，对标主流框架的地基能力；
**V3（服务能力"够用即止"加固）6 项**——默认档 fail-closed、执行侧
沙箱强制、SSRF 加固、限流配额、供应链默认收紧、expr 递归深度上限；
**V4（文档与工程卫生）4 项**——重生成 `AGENTS.md`、`main.rs` 拆分、
供应链卫生（`cargo-audit` 入 CI）、完整度收尾（Semantic 分块 factory
暴露 / `input_mapping` 空格漂移 / Gemini URL 修正 / run 目录保留策略）。

| Segment | Theme | Status |
| --- | --- | --- |
| N1 → N10 / P0 → P9 / P-H / P-LLM / M / P10 | 历史段，全部 closed 或外迁 | ARCHIVED |
| Q1 → Q5 | 2026-05-24 深度审计修复波次，108 DONE | ARCHIVED |
| H | Harness Mode follow-ups（loop-ownership + `harness chat` 收尾） | **DONE — archived（7/28）** |
| P-A | 契约内核 + 架构演进（`docs/RFC_CRATE_ARCHITECTURE.md`） | **DONE — archived（7/28）** |
| S | 沙箱与代码执行安全演进（`code_exec` / OS sandbox 强化） | **DONE — archived（7/28）** |
| L | 长程任务与检索增强（replan / 项目记忆 / RAG 补强 / 委托契约） | **DONE — archived（7/28）** |
| R | 2026-07-28 工程化审计修复（CI 覆盖率 / 架构守卫盲区 / 文档陈旧 / 仓库卫生） | **DONE — archived（7/29）** |
| T | 2026-07-29 架构评估修复（五维度独立审计：架构 / 完整度 / Agent 生态 / 服务层 / 安全） | **DONE — archived（7/30），15 项（14 DONE / 1 DEFERRED）** |
| U | 2026-07-30 复核评估修复（对 T 段修复效果的独立复核 + 新发现） | **DONE — archived（8/1），17 项（16 DONE / 1 DEFERRED）** |
| V | 2026-08-05 生产就绪修复（6 路并行全量重新评审 + 编排者实证核验） | **DONE — 26/26 全部收口 + V4.4-FU1 follow-up（Semantic 分块器子项，原 DEFERRED，2026-08-09 补做，见该条目）** |
| Deferred | Channel adapters / OS control / SaaS | non-goal |

## V — 2026-08-05 生产就绪修复（production readiness remediation）

> 来源：`docs/PROJECT_EVALUATION_2026-08-05.md`（6 路并行子代理独立通读
> L0 契约 / L1 执行核心 / L2 能力适配 / L3 Agent 编排 / L4 平台四层源码，
> 互不共享结论；编排者额外本地实跑 `cargo test/clippy/fmt/tree` 交叉验证，
> 并对 5 项关键发现逐行复核确认为真实缺陷）。**核心结论**：`cargo fmt
> --check`、全量 `clippy -D warnings`、lib 层 `unwrap/expect` DENY 均干净，
> `cargo test --workspace` 除 1 例环境限制（`code_exec` 依赖的容器运行时在
> 本机 XPC 连接失败，非代码缺陷）外全绿——**但测试全绿不等于正确**：多个
> P0/P1 缺陷位于未被现有测试覆盖的路径（DAG 终态语义、resume、UTF-8 边界、
> file 节点默认策略等），是"重新画一遍蓝图"式的独立评审挖出来的，而非对
> 已知问题的复核。定位：个人/单租户可信场景为主、服务能力有限，因此排序
> 原则是**核心优先，服务次之**——V0/V1 是不分场景都要修的正确性/稳定性
> 问题；V2 是核心能力地基；V3 是服务面"消除默认危险"而非做全套多租户
> 平台的够用型加固；V4 是文档与工程卫生。每项修复需配 regression test
> （复现缺陷的用例）；涉及默认值变更的需同步更新对应文档。每阶段结束跑
> `cargo fmt --check` + `cargo clippy --workspace --all-targets -D
> warnings` + lib 层 `unwrap/expect` DENY + `cargo test --workspace`，
> 保持既有 CI 门禁全绿。

### V0 — 阻断性正确性/安全 bug（P0，最先做，成本低影响高）

- DONE V0.1 ReAct 热路径 UTF-8 字节切片 panic（evaluation §六 P1-5）：
  `yanshi-agents/src/react/agent.rs:1843` 与 `:3066` 的
  `&observation[..observation.len().min(200)]`、`:3695` 的
  `content.truncate(160)` 均按**字节**切片，非字符边界时直接 panic。
  **Fix**：新增私有 helper
  `truncate_str_at_char_boundary(s: &str, max_bytes: usize) -> &str`
  （从 `max_bytes` 起向左找最近的合法字符边界，纯 safe Rust，
  `is_char_boundary` 逐字节 O(1) 检查，无第三方/nightly 依赖），替换
  三处调用点；第三处（`content.truncate(160)`，操作的是 owned
  `String`）改为先用 helper 求出边界安全的截断长度再 `truncate`。
  **测试**（5 个新增，均通过，见
  `cargo test -p yanshi-agents --lib react::agent::tests`）：
  `truncate_str_at_char_boundary_never_splits_a_multibyte_codepoint`
  （纯函数级，验证 `"测试".repeat(150)` 在 200 字节预算下返回的切片
  仍是合法边界、且与直接字符串切片结果一致；同时验证短字符串/空
  字符串原样返回）；
  `compact_memory_summary_truncates_multibyte_utf8_without_panicking`
  （覆盖第三处 `content.truncate` 调用点）；
  `run_with_context_does_not_panic_on_multibyte_utf8_observation`
  端到端复现原始崩溃场景——mock LLM 先触发一次
  `echo` 工具调用，参数 `text` 为 `"测试".repeat(150)`（900 字节，
  UTF-8 三字节字符，本地验证过字节偏移 200 确实落在非法字符边界上，
  即 pre-fix 代码在此输入下必然 panic），再返回最终答案，断言整个
  ReAct 回合正常完成、不 panic。`cargo fmt --check -p yanshi-agents`、
  `cargo clippy -p yanshi-agents --all-targets --no-deps -- -D
  warnings`、`cargo clippy -p yanshi-agents --lib --no-deps -- -D
  clippy::unwrap_used -D clippy::expect_used -A warnings` 均通过；
  `cargo test -p yanshi-agents --lib`：248 passed（新增 3 个，此前
  245 个全部保持通过，无回归）。
- DONE V0.2 `file` 节点默认 `SandboxPolicy::permissive()` → 任意绝对路径
  读写（evaluation §六 P1-3，同时是"服务次之"里少数因直接影响个人使用
  被上提到 V0 的一项）：`yanshi-nodes/src/nodes/file.rs:40` 的
  `Default` 实现 + `yanshi-config/src/executor/factory.rs:83` 组合下，
  绝对路径只要不含 `..` 就绕过唯一守卫；若 `path` 由 `input_mapping`
  数据驱动（例如 LLM 输出），等价于任意文件读写。**Fix**：
  1) `yanshi-nodes/src/nodes/file.rs`：`FileNode::default()` 从
  `SandboxPolicy::permissive()` 改为 `SandboxPolicy::default()`
  （空 `allowed_paths` = 拒绝一切路径，与 `yanshi-tools` 里
  `allowed_commands`/`allowed_paths` "空列表=全部拒绝"的既有约定一致，
  Q1.2.1 早就把这个约定定成了 `SandboxPolicy::default()` 的语义，
  `FileNode` 自己的 `Default` 之前一直在覆盖它）；更新了受影响的 4 个
  单测（改用显式 `allowed_paths` 策略）并新增
  `default_policy_denies_all_paths` 回归测试；`yanshi-nodes/benches/
  node_latency.rs` 的 `bench_file_read_write` 同步改用显式策略。
  2) `yanshi-config/src/config/schema.rs`：`"file"` 节点 `ParamSpec`
  新增 `required("allowed_paths", Sequence)`（镜像 `"shell"` 节点
  `allowed_commands` 的既有先例）。
  3) `yanshi-config/src/executor/factory.rs`：`"file"` 分支从直接
  `FileNode::default()` 改为解析 `node_def.parameters["allowed_paths"]`
  构造 `SandboxPolicy`，缺失/空值在**构建期**（而非每次调用时）返回
  `anyhow!` 错误——`yanshi workflow validate` 现在会在提交前拒绝没有
  `allowed_paths` 的 `file` 节点。新增 3 个单测
  （`file_node_without_allowed_paths_fails_at_build_time` /
  `file_node_with_empty_allowed_paths_fails_at_build_time` /
  `file_node_with_allowed_paths_builds_an_enforcing_node`，后者端到端
  验证声明的 allow-list 真的被 `FileNode::execute` 强制）。
  4) `yanshi-worker/src/lib.rs::execute_file_payload`：分布式 worker
  之前完全忽略 payload 参数、直接 `FileNode::default().execute(...)`；
  改为像 `yanshi-config` factory 一样从 `payload.parameters
  ["allowed_paths"]`（本地 scheduler 的 `dispatch_node` 已经把节点
  `parameters` 原样转发进这个字段）构造策略，缺失时退化为空
  `allowed_paths`（安全的拒绝一切，而非回退到旧的 permissive 行为）。
  新增回归测试
  `run_once_denies_distributed_file_payload_without_allowed_paths`；
  更新既有的 `run_once_executes_distributed_file_payload`（此前完全没有
  `allowed_paths` 却能写入任意路径，是本条要修的问题本身在测试里的
  真实体现）补上 `allowed_paths` 使其继续覆盖"分布式 file 写入成功"
  路径。
  5) 順带修复三个受影响的既有回归点：
  `yanshi-cli/tests/workflow_tests.rs` 的
  `write_mixed_permission_workflow` fixture 补 `allowed_paths`（否则
  `--explain-permissions` 系列 3 个测试会因 schema 拒绝而失败），
  新增 `cli_workflow_validate_rejects_file_without_allowed_paths`
  （镜像 `..._rejects_shell_without_allowed_commands`）；
  `yanshi-config/src/config/schema.rs` 的
  `validates_representative_config_first_node_schemas` fixture 补
  `allowed_paths`；`yanshi-cli/src/commands/workflow/validate.rs`
  `classify_node` 的 "file" 分支注释同步说明 "permissive: no
  allowed_paths constraint" 提示现在只是 defense-in-depth（正常校验
  路径已不可达），并把三个手写的 doc-translator 示例工作流
  （`examples/applications/doc-translator/workflow-iter{2,3,4}.yml`）
  和 `docs/CHECKPOINT_RECOVERY.md` 的示例都补上了 `allowed_paths`。
  **测试**：`yanshi-nodes --lib nodes::file`（5）、
  `yanshi-config --lib`（30，含新增 3 个 factory 测试）、
  `yanshi-cli --test workflow_tests`（45，含新增 1 个 + 更新 3 个）、
  `yanshi-worker --lib`（17，含新增 1 个 + 更新 1 个）均通过；
  `cargo fmt --check`、`cargo clippy --workspace --all-targets -D
  warnings`（含 `yanshi-nodes` 的 `conditional` feature-gated bench）、
  `cargo clippy --workspace --lib -D clippy::unwrap_used -D
  clippy::expect_used` 全部干净；`cargo test --workspace` 全绿，唯一
  失败项与 V0.1 之前一致（`code_exec` 本机 macOS XPC 容器连接失败，
  环境限制非回归）。
- DONE V0.3 DAG 良性跳过被计为整体失败（evaluation §六 P1-6）：
  `yanshi-core/src/flow.rs` 里 `run_if=false` 的跳过节点经
  `Err(YanshiError::NodeSkipped)` 承载，会被 `any(Result::is_err)` 的
  失败判定误判为 workflow 失败；serial（原 467/492 行）与 concurrent
  （原 830/848 行）两条路径各自独立计算这个判定，都踩了同一个坑。
  **Fix（比原计划的 `SkipMarker` 更小、风险更低的方案 —— 决策记录见下）**：
  没有引入新的"不复用 `Err` 通道"的类型，而是新增两个模块级纯函数：
  1) `is_genuine_failure(result: &AsyncNodeResult) -> bool`——
  `!matches!(result, Ok(_) | Err(YanshiError::NodeSkipped))`，
  语义对齐既有的两处约定（`execute_concurrently` 的依赖就绪判断已经把
  `Some(Ok(_)) | Some(Err(NodeSkipped))` 视为"和完成一样"；
  `expr::evaluate` 把跳过节点的输出解析成 `Null` 而不是报错）；
  2) `representative_failure_message(&state_pool)`——挑选
  `WorkflowFailed` 事件的 `error` 字段时先过滤掉 `NodeSkipped`，修复了一个
  连带发现的次生 bug：原来的 `find_map` 不分青红皂白地拿"第一个 Err"，
  如果同一次 run 里真实失败和无关的跳过同时存在，`HashMap` 迭代顺序
  没有保证，可能把 `NodeSkipped` 那句不知所云的"Node was skipped due to
  a condition."当成失败原因汇报出去。
  serial 路径：467 行 `workflow_failed` 改用 `is_genuine_failure`；492 行
  checkpoint 状态判断原先是重新算一遍 `all(|r| r.is_ok())`，改为直接复用
  上面已经算好的 `workflow_failed`（两处判断从"各自独立计算、逻辑上互为
  否命题"变成"同一个值"，顺便消除了它们本可以出现分歧的隐患）。
  concurrent 路径：848 行 `workflow_failed` 同样改用
  `is_genuine_failure`（`|| fail_fast_triggered` 保留不变）；另外顺手修了
  830 行一处相关但独立的一致性问题——"`running` 空但 `pending` 非空"的
  死锁探测原先也用 `any(Result::is_err)` 来判断"是否已有错误能解释卡住的
  原因"，改成 `is_genuine_failure` 后，如果状态池里只有跳过、没有真实
  错误却出现这种卡死，会如实抛出 `FlowExecutionFailed`（"调度器真的有
  bug"）而不是被一个无关的跳过掩盖过去——不过其实跳过从不阻塞下游调度
  （依赖就绪判断已经把跳过视为就绪），所以这个分支目前不会被触发，纯粹是
  为了让整个文件里"跳过 ≠ 失败"这条规则处处一致。
  **决策记录（为什么没做原计划的 `SkipMarker` 重构）**：动手前发现
  `Err(YanshiError::NodeSkipped)` 这个表示方式已经深度嵌入
  `gather_inputs`（`Some(Err(NodeSkipped))` 分支据此判断"必需依赖被跳过"
  从而返回 `DependencyNotMet`）、并发路径的依赖就绪匹配、`expr.rs` 的
  条件求值、以及大量既有测试的断言写法。引入一个不复用 `Err` 通道的新
  `SkipMarker` 类型需要改变 `AsyncNodeResult`/状态池的类型形状，波及
  面会远超 `yanshi-core` 一个 crate（`resume.rs`、`checkpoint.rs`、
  下游消费 `AsyncNodeResult` 的多个 crate），且不是这条 bug 本身要求的
  ——问题根子是"终态判定的分类函数把跳过和失败混为一谈"，不是"跳过的
  底层表示方式选错了"。用两个纯函数收敛判定逻辑，在不改变线上类型/
  兼容性的前提下就修正了行为，风险和改动面都小得多；未来如果真的需要
  把跳过从 `Result` 通道里独立出来（比如要支持更丰富的"部分完成"状态
  机），可以在那个时机单独立项评估。
  刻意不动的相邻问题（属于其他条目）：serial 路径里 `gather_inputs` 的
  错误仍经 `?` 直接 abort 整个函数、不发 `WorkflowFailed`/不落最终
  checkpoint（V1.1 的范围）；Map 节点内部子流程的 `err_indexes` 汇总
  （`execute_map_node_sequential`/`_parallel`）仍用未过滤的
  `values().any(|r| r.is_err())`——子流程内部若只有良性跳过会被计入
  `results_summary` 的错误索引，同一类问题但发生在 Map 节点自己的输出
  里而不是顶层 workflow 终态，evaluation 原文只点名了顶层 467/848 两处，
  暂不在本条修复范围内，留给后续评估判断是否要开新条目。
  **测试**（7 个新增，均通过，见
  `cargo test -p yanshi-core --lib flow::tests`）：
  `is_genuine_failure_treats_only_skip_as_benign` /
  `representative_failure_message_ignores_benign_skips`（纯函数级）；
  `serial_skip_only_workflow_completes_not_failed` /
  `concurrent_skip_only_workflow_completes_not_failed`（同一个"guard
  （Ok）→ skipped_branch（run_if 引用 guard 输出、无下游依赖它的
  输出）"DAG 分别跑 serial 默认配置和 `FlowExecutionConfig::concurrent(2)`，
  断言 `workflow.completed` 事件出现且 `workflow.failed` 不出现）；
  `skip_only_workflow_checkpoints_as_completed_in_both_modes`（同一 DAG
  开启 checkpoint 后两种模式都跑一遍，断言落盘的 `checkpoint.status`
  是 `Completed`）。既有的
  `concurrent_skip_records_event_and_continues_independent_work`
  （断言必需依赖指向被跳过节点时应该触发 `DependencyNotMet` 这一真实
  失败、且 `workflow.failed` 确实要出现）无需改动即继续通过，确认了
  "跳过导致的级联真实失败"和"纯跳过"两种场景在修复后被正确区分。
  `cargo fmt --check`、`cargo clippy --workspace --all-targets -D
  warnings`、`cargo clippy --workspace --lib -D clippy::unwrap_used -D
  clippy::expect_used` 全部干净；`yanshi-core --lib` 104 项全过
  （含新增 5 项，原有 99 项无回归）；`cargo test --workspace` 全绿，
  唯一失败项与 V0.1/V0.2 之前一致（`code_exec` 环境限制，非回归）。
- DONE V0.4 checkpoint/run-dir 路径穿越 + 破坏性删除（evaluation §六
  P1-7）：`yanshi-core/src/checkpoint.rs` 的 `get_workflow_dir`
  （原 403 行）及 `flow.rs` 里 `run_dir`/`persist_step_result` 的路径拼接
  对 `workflow_id`/`node_id` 未做净化——前者可能来自调用方传入的 run id
  （例如服务端场景），后者来自工作流作者的 YAML `id:` 字段，都不可信；
  `PathBuf::join` 对绝对路径会整体替换掉 base，`..` 则要等到真正的文件
  系统调用才会被解析成上级目录，`delete_all_checkpoints("..")` 等价于对
  `checkpoint_dir` 的**上级目录**做 `remove_dir_all`。**Fix**：
  1) `checkpoint.rs` 新增 `pub(crate) fn is_safe_path_component(id:
  &str) -> bool`（拒绝空串、`.`、`..`、含 `/` 或 `\` 的字符串），
  `get_workflow_dir` 签名从 `-> PathBuf` 改为 `-> Result<PathBuf>`，
  校验不通过时返回 `ConfigurationError` 而不是拼出一个危险路径；
  `save_checkpoint_struct`/`load_latest_checkpoint`/
  `load_all_checkpoints`/`delete_checkpoint`（3 处）/
  `delete_all_checkpoints` 共 7 个调用点补 `?` 传播（`get_workflow_dir`
  本身是私有方法，仅这 6 个公开方法受影响，签名未变，下游 crate 无需
  改动）。2) `flow.rs` 从 `checkpoint` 模块导入同一个
  `is_safe_path_component`：`execute_with_workflow_id` 在算出 `run_id`
  后、构造 `run_dir`/做任何文件系统操作前立即校验（`workflow_id`
  为空时走 `Uuid::new_v4()`，恒安全，不影响默认路径）；
  `persist_step_result` 在拼 `{node_id}_outputs.json` 之前校验
  `node_id`——serial/concurrent 两条路径的跳过分支与正常执行分支都会
  经过这一个函数，天然覆盖了工作流里的每一个节点，不需要单独加一遍
  校验。两处校验不通过都走已有的 `?` fatal-error 惯例（等同于"节点在
  flow 定义里找不到"这类既有的配置错误），不是把它们当成 per-node 的
  普通运行时失败塞进 state_pool。**测试**（7 个新增，均通过）：
  `yanshi-core --lib checkpoint::tests`——
  `is_safe_path_component_rejects_traversal_and_separators`（纯函数级）、
  `delete_all_checkpoints_rejects_parent_directory_traversal`（复现原始
  灾难场景：真实临时目录 + 其父目录里放一个 sentinel 文件，对
  `workflow_id=".."` 调用 `delete_all_checkpoints`，断言拒绝且 sentinel
  和 checkpoint 目录本身都完好无损）、
  `checkpoint_operations_reject_unsafe_workflow_ids`（`save_checkpoint`/
  `load_latest_checkpoint`/`load_all_checkpoints`/`delete_checkpoint`/
  `delete_all_checkpoints` 五个公开方法用同一个恶意 `workflow_id` 各跑
  一遍，全部返回 `Err`）；`yanshi-core --lib flow::tests`——
  `execute_with_unsafe_workflow_id_is_rejected_before_touching_disk` /
  `execute_with_unsafe_node_id_is_rejected_before_touching_disk`
  （分别用恶意 `workflow_id` 和恶意 `GraphNode.id` 跑一次完整
  execute，断言报错且 `run_base_dir` 之外没有出现任何逃逸出来的目录/
  文件）。`cargo fmt --check`、`cargo clippy --workspace --all-targets
  -D warnings`、`cargo clippy --workspace --lib -D
  clippy::unwrap_used -D clippy::expect_used` 全部干净；
  `yanshi-core --lib` 全过（含新增 7 项，原有全部无回归）；
  `cargo test --workspace` 全绿，唯一失败项与前几项一致（`code_exec`
  环境限制，非回归）。
- DONE V0.5 worker-gRPC `submit_task` 无鉴权（evaluation §六 P1-2）：
  `yanshi-server/src/scheduler/grpc.rs` 的
  `AuthenticatedGrpcWorkerService::submit_task`（原 82-104 行）无条件
  接受任何调用，理由写在原注释里——"假设自己在 HTTP bearer 中间件之后"；
  但 `worker_grpc.rs::serve_worker_grpc` 绑定的是独立 socket、根本不经过
  任何 HTTP 中间件，这个假设不成立。任何能到达该端口的调用方都能投递
  任意 `WorkerTask`，与 `docs/DISTRIBUTED.md` §Worker Admission 里
  "every RPC ... verified against the active WorkerAdmissionPolicy"的
  文档承诺矛盾（只有这一个 RPC 没做到）。**Fix**：`submit_task` 的
  wire 消息（`SubmitTaskRequest`）不带 `worker_id`——它是 gateway →
  server 而非 worker → server，天然没法复用其余三个方法"按具体 worker
  身份校验"的 `admit()`。新增
  `WorkerAdmissionPolicy::check_any(token: Option<&str>)`（不绑定到
  某个具体 `WorkerId`：未配置任何凭据机制时保持"任何调用方都放行"的
  既有 dev/local 默认行为不变；配置了任一机制后，要求 token 匹配任意一个
  worker 的 PSK、或对任意一个 `jwt_workers` 成员验签通过）+
  `AuthenticatedControlPlane::admit_any()`（薄包装，不做 worker 计数/
  in-flight 记账，因为这里没有"这一个 worker"的概念）；`submit_task`
  改为先 `extract_admission_token` 再 `admit_any`，拒绝时映射成
  `Status::permission_denied`（复用既有的 `extract_admission_token`
  helper，其余三个方法早就在用）。由于生产 profile 下
  `WorkerAdmissionPolicy::for_profile`（T0.2）已经强制要求配置某种凭据
  机制，`check_any` 在生产环境下必然要求 token 匹配，因此
  `submit_task` 在生产 profile 下自动变成 fail-closed，不需要额外的
  profile 判断代码。**测试**（9 个新增，均通过）：
  `yanshi-server --lib scheduler::admission::tests`——
  `check_any_admits_anyone_under_the_open_policy` /
  `check_any_requires_a_matching_psk_once_any_worker_has_one` /
  `check_any_matches_a_psk_belonging_to_any_configured_worker` /
  `check_any_fails_closed_when_only_an_allowlist_is_configured`（纯策略
  单测，覆盖"未配置=放行"、"配置了但 token 缺失/错误=拒绝"、"token 匹配
  任一 worker 即可"、"只配 allowlist 没配 PSK/JWT 时仍然 fail-closed"
  四种场景）；`yanshi-server --lib worker_grpc::tests`——
  `submit_task_over_grpc_without_credentials_is_rejected_when_admission_is_configured`
  （真实 tonic client/server 端到端复现原始漏洞：PSK 已配置、client 不带
  token 调用 `submit_task`，断言返回 `PermissionDenied` 且任务从未进入
  队列——用 `run_snapshot` 确认）+
  `submit_task_over_grpc_with_valid_credentials_succeeds`（同样的端到端
  设置换成带正确 token 的 client，确认合法路径不受影响、任务确实进了
  队列）。`cargo fmt --check`、`cargo clippy --workspace --all-targets
  -D warnings`、`cargo clippy --workspace --lib -D
  clippy::unwrap_used -D clippy::expect_used` 全部干净；
  `yanshi-server --lib scheduler::admission`（23 项，含新增 4 项）+
  `worker_grpc::tests`（8 项，含新增 2 项）全过，原有全部无回归；
  `cargo test --workspace` 全绿，唯一失败项与前几项一致（`code_exec`
  环境限制，非回归）。

### V1 — 核心执行/Agent 正确性与健壮性（P1 核心）

- DONE V1.1 统一 DAG 终态语义（evaluation §六，depends on V0.3/V0.4）：
  serial 路径（`yanshi-core/src/flow.rs`）的 `gather_inputs` 错误
  之前经 `?` 中途 abort 整个 `execute_from_inputs` 函数——不发
  `WorkflowFailed` 事件、不落最终 checkpoint 状态（永远停在
  `Running`）、调用方拿到的是硬 `Err` 而不是 concurrent 路径早就在产的
  "per-node result"。**Fix**：把 serial 循环重构成与 concurrent 一致的
  两段式——先 `gather_inputs`（不再用 `?`），失败时构造一个
  `(Err(error), Instant::now())` 的 `(result, node_started_at)` 元组，
  和正常执行路径共用后续的 `persist_step_result` /
  `record_node_result_events`（复用 concurrent 早就抽出来的这个共享
  helper，serial 之前是内联重复一份 match）/ `state_pool.insert`
  /（可选）checkpoint 落盘逻辑；再让 serial 显式读取
  `execution_config.fail_fast`/`continue_on_skip`（之前 serial 完全不看
  这两个字段，无条件跑完所有节点）——真实失败时 `fail_fast` 决定是否
  `break` 掉 `for` 循环（未被调度的节点从 state_pool 里整体缺席，对齐
  concurrent 把未调度的 `pending` 节点晾在一边的效果）；良性跳过时
  `continue_on_skip` 同理。
  **顺带修的连带发现**（同一个"终态判定"bug 族，动手过程中发现的，
  一并收尾而非另开条目，因为改动都很小且直接相关）：
  1) Map 节点的 `execute_map_node_sequential` 子流程内部同样调用
  `execute_from_inputs`，之前一个子流程节点的 `gather_inputs` 失败会经
  由serial 的硬 abort 冒泡成整个 Map 节点自己的 `Err`——而 parallel 变体
  （`execute_map_node_parallel`）设计上从一开始就假设子流程内部的
  per-node 失败会被"埋"进 `results[i]` 并汇总进 `results_summary`
  （F-A6-3）。serial 修复后两个变体行为终于一致，更新了
  `test_input_mapping_item_missing_path_errors`（此前的注释明确写着
  "gather-inputs failures bubble through the map's `?` and end up on
  the map node itself"——把 bug 当特性钉进了测试）。
  2) `yanshi-server/src/runs.rs::first_state_error` 和
  `yanshi-core`'s `is_genuine_failure`（V0.3）是同一个 bug：把良性
  `NodeSkipped` 也算作 run 失败，导致纯跳过的 run 被误标
  `RunStatus::Failed`。改成和 `is_genuine_failure` 一致的判定。
  3) `yanshi-cli/src/commands/workflow/run.rs::run_with_retries`
  从来没检查过 `execute_from_inputs_with_id_and_config` 返回的
  `state_pool` 里是否嵌了 per-node 失败——`Ok(state)` 就直接
  `return Ok(state)`，打印"✅ Workflow completed"、退出码 0，哪怕某个
  节点真的失败了（concurrent 模式从一开始就是这样；serial 模式此前
  "碰巧"因为 gather_inputs 硬 abort 而在这一类失败上表现正确，本次修复
  等于把这个意外的"正确"也拿掉了）。新增
  `first_genuine_state_error()`（镜像 `is_genuine_failure`），
  `Ok(Ok(state))` 分支里先检查一遍，查到真实失败就并入既有的
  `last_error`/重试/最终 `Err` 路径——顺带让 `--max-retries` 对
  "gather_inputs 失败以外的任何节点失败"以及"concurrent 模式"都能生效，
  而不只是之前意外覆盖的那一种情况。改这个的过程中还揪出一个真实存在
  的测试 bug：`cli_workflow_run_uses_env_run_artifacts_directory` 用的
  模板 `"Hello {{ topic }}"` 从来没传 `--input topic`，Tera 渲染其实
  一直在失败，只是 CLI 之前不检查嵌入式失败所以测试"碰巧"通过——补上
  `--input topic Yanshi`（跟其余同模板的姊妹测试一致）。
  **测试**（yanshi-core 新增 4 个 + 更新 1 个既有测试；
  yanshi-server 新增 3 个；yanshi-cli 新增 2 个 + 修正 1 个既有
  fixture，均通过）：
  `yanshi-core --lib flow::tests`——
  `serial_gather_inputs_failure_does_not_abort_the_whole_run`（默认
  serial 配置下 `gather_inputs` 失败不再硬 abort，`fail_fast` 默认
  `true` 正确阻止了拓扑序里更后面的无关节点被调度，`workflow.failed`
  出现且 `workflow.completed` 不出现）、
  `serial_and_concurrent_agree_on_terminal_status_for_a_genuine_failure`
  （同一个 DAG 分别跑 serial/concurrent，断言两种模式都产生
  `workflow.failed`）、
  `serial_gather_inputs_failure_checkpoints_as_failed_not_stuck_running`
  （开启 checkpoint 后确认最终状态是 `Failed` 而不是卡在 `Running`）、
  `serial_fail_fast_false_continues_past_a_genuine_failure`
  （显式 `fail_fast: false` 时仍然继续跑完剩余节点，保留 pre-fix
  "无条件跑完"行为、只是现在是可配置的而非唯一行为）；
  `test_input_mapping_item_missing_path_errors` 改为断言 Map 节点整体
  `Ok` + `results[0].sink` 里嵌着 `NodeInputError` +
  `results_summary.err_indexes == [0]`。
  `yanshi-server --lib runs::first_state_error_tests`——
  `ignores_a_benign_skip` / `reports_a_genuine_failure_alongside_a_skip`
  / `none_when_every_node_succeeded`。
  `yanshi-cli --test workflow_tests`——
  `cli_workflow_run_fails_when_a_node_genuinely_fails`（serial 模式下
  模板渲染真实失败时 `yanshi workflow run` 现在正确以非零退出码
  失败）/
  `cli_workflow_run_fails_when_a_node_genuinely_fails_under_concurrent_mode`
  （同一断言换成 `--execution-mode concurrent`）。`cargo fmt --check`、
  `cargo clippy --workspace --all-targets -D warnings`、`cargo clippy
  --workspace --lib -D clippy::unwrap_used -D clippy::expect_used`
  全部干净；`yanshi-core --lib`（113 项）/ `yanshi-server --lib
  runs::`（10 项）/ `yanshi-cli --test workflow_tests`（47 项）全过，
  均无回归；`cargo test --workspace` 全绿，唯一失败项与前几项一致
  （`code_exec` 环境限制，非回归）。
- DONE V1.2 resume 对 DAG 健全化（evaluation §六 P1-8，depends on
  V0.3/V0.4 的 skip 标记与路径净化）：`execute_from_checkpoint` 原先靠
  在拓扑序里找 `last_completed_node` 的下标、把它之前的节点全部当成
  "已完成"来决定跳过谁。这在原始 run 是**并发**执行时不成立——checkpoint
  只记录"最近一次完成的那一个节点"，不代表拓扑序在它之前的节点都跑完了：
  下标更低的节点可能压根没跑到，下标更高的节点反而可能已经跑完了。
  最坏情形——`last_completed_node` 在拓扑序里没有后继——会导致整个
  workflow 被误判为"已经完成"，把还没跑的节点永久性地漏掉。`skip` 标记
  在 checkpoint 往返中被直接丢弃（和"这个节点从没跑过"无法区分）；
  `NodePartialExecutionFailed` 的部分输出之前用和 `Ok` 完全相同的裸
  shape 写入，等于**部分失败被静默升级为成功**。**Fix**：
  1) 把"跳过判定"从拓扑下标 cutoff 改成对**恢复后的 state_pool** 逐节点
  查——只有 `Ok` 或良性 `NodeSkipped` 才算"已确定完成"（直接复用 V0.3 的
  `is_genuine_failure` 语义：`!is_genuine_failure(result)`），不管它在
  拓扑序里的位置；不存在或是真实失败（含 `NodePartialExecutionFailed`）
  的节点一律（重新）执行。这一步能生效的前提是 checkpoint 本身对"跳过"
  和"部分失败"如实记录——于是：
  2) `state_pool_to_checkpoint_state`/`checkpointable_value` 新增对
  `NodeSkipped`（之前完全没有 checkpoint 条目）和
  `NodePartialExecutionFailed`（之前的裸 shape 和 `Ok` 无法区分）的
  编码：**没有**把整个值包一层 envelope（`{"outcome":...,
  "outputs":{...}}`）——那样会破坏 `yanshi-agents` 的 partial-resume
  契约、CLI 工具以及本 crate 自己的既有测试（`checkpoint_recovery_test.rs`）
  已经在依赖的"`checkpoint.state[node_id][output_key]` 直接可读"这个
  裸 shape 约定；而是在节点自己那个扁平的 outputs 对象里插入
  `__yanshi_outcome__`（值为 `"skipped"`/`"partial_failure"`）和
  `__yanshi_partial_failure_message__` 这两个保留 sibling key，跟
  其他输出字段用一样的 tagged-FlowValue 编码方式，天然复用现有的
  `decode_outputs_object`/`decode_checkpoint_flow_value` 解码链路，
  `Ok` 的裸 shape 完全不变（磁盘向后兼容零风险）。
  `checkpoint_state_to_state_pool` 解码时读到这两个保留 key 就转回原始
  `Err` 变体、并把 key 从解码出的 outputs 里摘掉（不泄漏进真实输出）；
  没有这两个 key（旧 checkpoint、或任何 `Ok` 条目）走原来的裸 shape
  legacy 路径，行为完全不变。3) `NodePartialExecutionFailed`
  **不算**"已确定完成"，必须重跑——这是动手过程中通过两个既有集成测试
  失败发现的真实约束（`test_resume_reexecutes_failed_agent_node_
  with_checkpointed_partial_trace` / `test_repeated_partial_resume_
  keeps_checkpoint_at_last_completed_node`）：`yanshi-agents` 的
  agent 节点靠"重新执行、同时把自己之前的 `partial_outputs` 当作
  额外输入注入"来实现内部的工具调用重放决策（`inputs.get("agent_result")`
  读取上一次的部分进度）——这个机制此前"能工作"完全是**依赖着**
  checkpoint 往返把 `PartialExecutionFailed` 静默解码成 `Ok` 这个 bug
  本身！于是重新加回（并推广）了 partial-outputs 重新注入的逻辑：
  节点重跑前，若 `state_pool` 里有它自己的
  `Err(NodePartialExecutionFailed{partial_outputs,..})`，把
  `partial_outputs` extend 进新一轮的 `inputs`，效果和 pre-fix 相同，
  但不再依赖那个 round-trip bug。4) 顺带删除了一段已经不可达的死代码
  （旧版"若 `state_pool.get(node_id)` 是 `Ok`，把它的输出也 extend 进
  inputs 再重新执行一遍"的逻辑——新的跳过判定下，能走到这里的节点的
  `state_pool` 条目只可能是"不存在"或"真实失败"，永远不会是 `Ok`）；
  `execute_with_workflow_id` 的 `skip_until: Option<String>` 参数
  连带其构造/传参代码整体移除（4 个调用点同步更新），`execute_from_checkpoint`
  不再需要计算"下一个要跑的节点下标"这一步。**测试**（yanshi-core
  新增 3 个单测，均通过；另外两个既有集成测试在实现过程中充当了关键的
  "设计假设校验器"，逼出了上面第 3 点的修正）：
  `checkpoint_state_round_trip_preserves_node_skipped` /
  `checkpoint_state_round_trip_preserves_partial_failure_not_upgraded_to_ok`
  （纯函数级往返测试，后者额外断言裸 shape 约定——
  `checkpoint_state["agent"]["agent_result"]["value"]...`——原样可读，
  且保留 key 不会泄漏进解码出的 `partial_outputs`）；
  `resume_executes_a_topologically_earlier_node_that_never_ran_under_concurrent_execution`
  （手工构造一个"拓扑序更靠后的节点先完成、拓扑序更靠前的节点从未跑过"
  的 checkpoint——不依赖真实并发时序竞争，直接摆出崩溃后可能落盘的
  确切状态——断言 resume 后那个"从未跑过"的节点确实被执行了；旧逻辑
  下这个场景会因为 `last_completed_node` 在拓扑序里没有后继而把整个
  workflow 误判为"已完成"，永久漏掉该节点）。`yanshi-core --test
  checkpoint_recovery_test`（10 项，含前述两个一度 FAILED、现已修复的
  既有测试）/ `--test flow_value_checkpoint_compat`（4 项，验证裸
  shape 磁盘协议未受影响）/ 全部其余集成测试文件均无回归。
  `cargo fmt --check`、`cargo clippy --workspace --all-targets -D
  warnings`、`cargo clippy --workspace --lib -D clippy::unwrap_used
  -D clippy::expect_used` 全部干净；`yanshi-core --lib`
  （116 项，含新增 3 个）全过；`cargo test --workspace` 全绿，唯一
  失败项与前几项一致（`code_exec` 环境限制，非回归）。
- DONE V1.3 `AgentCancellationToken` 漏唤醒竞态（evaluation §六 P1-9）：
  `yanshi-agent-spi/src/runtime.rs` 的 `cancelled()`（原 335 行）
  先查 `is_cancelled()` 再 `await` `self.notify.notified()`——两步之间
  到达的 `cancel()`（`AtomicBool` store + `notify_waiters()` 两步）会被
  漏过：`notify_waiters()` 只唤醒**已经构造出来**的 `Notified` future，
  这时一个都还没构造，于是下一次 `notified().await` 会永远阻塞，即使
  token 其实已经被取消。**Fix**：改成 tokio 官方文档给出的"先订阅再
  检查"模式——先调用 `self.notify.notified()`构造（但不 await）
  future，再查 `is_cancelled()`，未取消才 `.await` 已经构造好的 future。
  `Notify::notified()` 在**构造时**（不是首次 poll 时）就捕获了通知
  代际快照，所以不管 `cancel()` 落在检查之前还是之后，只要发生在
  构造之后，这个 gap 就已经被堵上了。
  **测试的诚实边界**（值得记录，因为过程中反复验证过）：这个 race
  要求 `cancel()` 的两步完整发生在"检查"和"构造 `notified()`"之间的
  几条 CPU 指令窗口内，而这个窗口在单次 poll 内部是一段没有任何 yield
  点的连续同步代码——真实复现它需要两个 OS 线程在这几条指令的粒度上
  发生硬件级别的交错，`tokio::spawn` 的调度顺序控制不了这个精度，手动
  单步 poll（`tokio_test`、手写 waker）也做不到，因为单线程单步 poll
  没有办法在一次 poll 内部"打断"它。实测过：数千次
  spawn-然后立刻-cancel 的迭代（含 64 路并发 waiter）跑在 pre-fix 代码
  上一次都没有触发挂起。确定性复现这类精确交错需要 `loom` 风格的穷举
  调度探索，而这不在本仓库现有的测试基础设施范围内。所以新增的测试
  **不是** race 已消失的证明——那个结论建立在"`cancelled()` 严格匹配
  `tokio::sync::Notify` 自己文档承诺的 race-free 模式"这一点上（代码
  注释里写清楚了）；测试只验证了一个更朴素但仍然有意义的性质：
  spawn-然后-cancel 在数百次迭代、多 worker 线程下总是能及时 resolve、
  不会退化成某种更容易触发的挂起。**测试**：
  `yanshi-agent-spi --lib`（该 crate 自身 `Cargo.toml` 只声明了
  tokio 的 `sync` feature，独立 `-p` 测试因缺 `macros`/`rt` 而编译
  失败——这是先于本次改动就存在的、与 V1.3 无关的 manifest 问题，通过
  `-p yanshi-agent-spi -p yanshi-agents` 一起测即可借到后者的
  `tokio = {features=["full"]}` 完成 feature 统一）新增
  `cancelled_resolves_promptly_across_many_spawn_then_cancel_iterations`
  （500 次迭代 × 每次 spawn 一个 waiter 立即 cancel，每次都有超时保护）；
  既有的 `cancellation_token_notifies_waiters` 保持通过。
  `cargo fmt --check`、`cargo clippy --workspace --all-targets -D
  warnings`、`cargo clippy --workspace --lib -D clippy::unwrap_used
  -D clippy::expect_used` 全部干净；`yanshi-agent-spi --lib`
  （58 项，含新增 1 项）+ `yanshi-agents --lib`（248 项）全过；
  `cargo test --workspace` 全绿，唯一失败项与前几项一致（`code_exec`
  环境限制，非回归）。
- DONE V1.4 可靠性栈接入 `Flow` 执行器（evaluation §五"可靠性栈是引擎级
  门面"）：`yanshi-core` 的 `timeout`/`retry_executor` 模块之前没有
  `Flow` 自己执行循环里的调用者——唯一的生产调用方是
  `yanshi-config::TimeoutRetryNode`（T3.1），它在节点被 Flow 拿到
  **之前**、以"装饰 `AsyncNode`"的方式套一层，天然看不见
  `WorkflowEvent`，所以它做的重试对 trace replay / 事件流完全不可见；
  `WorkflowEvent::RetryAttempt` 这个事件变体存在，但整个仓库里从来没有
  代码构造过它。**Scope 说明**：eval 原文问题描述里同时点了
  `ResourceManager`/`HealthChecker`，但 Fix/测试 两条只具体要求了
  timeout+retry+RetryAttempt 事件——`ResourceManager`/`HealthChecker`
  接入 DAG 执行路径涉及"每个节点该套用什么资源上限"这类更大的独立设计
  问题，本条不覆盖，留给以后单开条目判断。**Fix**：
  1) `FlowExecutionConfig` 新增两个可选字段
  `node_timeout_ms: Option<u64>` / `node_retry_policy:
  Option<RetryPolicy>`，默认都是 `None`（现有 workflow 零行为变化），
  配套 `with_node_timeout_ms`/`with_node_retry_policy` builder 方法。
  2) 新增 `FlowExecutor::execute_node_with_reliability`，serial 和
  concurrent 两条调度路径**统一**改用它包一层原来直接的
  `execute_node_type` 调用——配了 `node_timeout_ms` 就用
  `tokio::time::timeout` 包一层；配了 `node_retry_policy` 就用新增的
  `execute_with_retry_and_hook`（`execute_with_retry` 的变体，在每次
  重试前调一个 hook 回调）驱动重试，hook 里真正调用
  `self.emit_event(WorkflowEvent::RetryAttempt {...})`——因为这层包装
  就活在执行器内部，`self.emit_event` 就在手边，这是和 T3.1 的
  `TimeoutRetryNode`（活在另一个 crate、根本拿不到事件系统）的本质区别。
  这是 Flow 级、和 T3.1 的按 YAML 节点配置**互补**（不是替代）的机制——
  面向直接用 Rust API 构造 `GraphNode` 的 Core SDK 使用者，也面向想要
  "重试这件事本身可被观测"的任何调用方。
  3) `RetryPolicy`/`RetryStrategy`/`ErrorPattern`（`yanshi-async-util`）
  补上 `PartialEq`派生，好让 `FlowExecutionConfig` 能装下
  `Option<RetryPolicy>` 还继续派生 `PartialEq`；`FlowExecutionConfig`
  相应地把 `Eq` 从派生列表里去掉（只保留 `PartialEq`）——
  `RetryStrategy::ExponentialBackoff` 内嵌的 `f64` 倍率天生做不到
  `Eq`，核对过仓库里没有任何地方真的依赖 `FlowExecutionConfig: Eq`
  （没有拿它当 `HashSet`/`HashMap` key 的用法）。
  4) `yanshi-core/benches/scheduler.rs` 里两处手写的
  `FlowExecutionConfig { mode: Concurrent, max_concurrency: 8, ... }`
  结构体字面量因为新增必填字段而编译失败，顺手简化成
  `FlowExecutionConfig::concurrent(8)`（本来就该这么写，这两处应该是在
  `::concurrent()` 构造函数存在之前就有的）。
  **测试**（`yanshi-core --lib` 新增 5 个：`flow::tests` 3 个 +
  `retry_executor::tests` 2 个，均通过）：
  `serial_node_timeout_ms_times_out_a_slow_node`（配置 50ms Flow 级
  超时，节点睡 1 小时，断言最终状态是 `TimeoutExceeded{duration_ms:
  50}`）；`serial_node_retry_policy_retries_and_emits_retry_attempt_events`
  /`concurrent_node_retry_policy_retries_and_emits_retry_attempt_events`
  （一个节点前两次调用失败、第三次成功，serial 和 concurrent 两种模式
  各跑一遍，断言最终 `Ok`、调用次数恰好 3、且 `RecordingListener` 真的
  收到恰好 2 个 `retry.attempt` 事件——这是这条修复的真正验收点，不只是
  测试重试本身能工作，而是测试**事件真的被发出来了**）；
  `execute_with_retry_and_hook_invokes_hook_once_per_retried_attempt`
  （纯函数级，断言 hook 收到的 attempt 序号是 `[1, 2]`，1-based）/
  `execute_with_retry_and_hook_does_not_fire_for_non_retryable_errors`
  （用 `RetryPolicy::default()`而非 `builder()...build()`——后者的
  `retryable_errors` 默认是空列表、被 `is_retryable` 解释成"什么都重
  试"，跟这个测试想验证的"非可重试错误不重试"正好相反——断言
  `ValidationError` 一次都不触发 hook）。`cargo fmt --check`、`cargo
  clippy --workspace --all-targets -D warnings`（含 benches）、`cargo
  clippy --workspace --lib -D clippy::unwrap_used -D
  clippy::expect_used`、`cargo xtask check-arch` 全部干净；
  `yanshi-core --lib`（121 项，含新增 5 项）/
  `yanshi-async-util --lib`（21 项）/ `yanshi-config --lib`
  （30 项，含 T3.1 既有的 `TimeoutRetryNode` 测试，确认改动无回归）
  全过；`cargo test --workspace` 全绿，唯一失败项与前几项一致
  （`code_exec` 环境限制，非回归）。
- DONE V1.5 LLM 级 429/5xx 重试退避（evaluation §六"无 LLM 级重试"）：
  `yanshi-llm` 此前不消费已存在的 `defaults.max_retries`/
  `retry_delay_ms` 配置，遇到瞬时错误直接向上抛错，无退避重试。
  **Fix**：新增 `LLMError::is_retryable()`，归类
  `RateLimitExceeded`/`ServiceUnavailable`/`TimeoutError` 以及
  `HttpError{status_code}` 为 429 或 5xx 的情形——之所以要单独检查
  `HttpError`，是因为各 provider 自身的状态码分支只产出通用
  `HttpError`，`RateLimitExceeded`/`ServiceUnavailable` 只在
  `From<reqwest::Error>` 这条传输层映射里才会出现，实测五个真实
  provider 均未走到那条路径；非瞬时错误（4xx 参数错误、鉴权失败、
  不支持的特性等）恒不重试。在 `LLMClient::execute`/`execute_full`/
  `execute_streaming` 三个调用点接入新的 `retry_transient()` helper，
  由新增的 `ModelRegistry::get_defaults()`（暴露已解析配置的
  `GlobalDefaults`，未加载配置时回退 `default()`——重试是可选增强而
  非调用前置条件）驱动：`max_retries` 每次重试前按
  `retry_delay_ms * 2^attempt`（`saturating_mul` 防溢出）退避，
  `None`/`0` 保持修复前行为（首次失败即刻上抛，不重试）；每次重试
  都会（在启用 `logging` feature 时）打一条 `warn!` 日志。刻意不引入
  `yanshi-async-util::RetryPolicy`（`yanshi-mcp` 也未这样
  做）——其 `ErrorPattern` 是硬编码匹配 `YanshiError` 的，套到
  `LLMError` 上要么要做一次错误类型转换桥，要么等价于重新实现分类
  逻辑，收益低于自成一体的实现。
  **测试**：`error.rs` 新增 3 项 `is_retryable()` 分类测试
  （预分类瞬时变体 / `HttpError` 429\|5xx / 非瞬时错误恒 false）；
  `llm_client.rs` 新增 `retry_transient_tests` 模块 4 项测试，直接
  驱动 helper（不经注册表）：瞬时错误重试后成功、耗尽 `max_retries`
  后原样抛出、非瞬时错误首次即抛（不重试）、`max_retries: None`
  保持修复前行为；`MockProvider` 扩展 FIFO 错误队列
  （`with_queued_errors` builder + `YANSHI_MOCK_ERROR_STATUS_CODES`
  环境变量，仿照既有 `YANSHI_MOCK_RESPONSES` 模式，因为注册表构造
  的 provider 实例只经过构造函数、看不到 builder 方法），
  `mock.rs` 新增 1 项单测锁定 FIFO 消费顺序 + 落回正常响应；新增
  `yanshi-llm/tests/llm_retry.rs` 集成测试，经
  `ModelRegistry::global()` + `Yanshi::model(...).execute()` 全链路
  验证「先 429 再成功」最终成功、「400 参数错误」不会被重试吞掉。
  该集成测试文件内只有一个测试函数、且两个场景在同一 async 函数内
  顺序执行（先 429 场景、后 400 场景）——因为 cargo 把 `tests/` 下每个
  文件编译成独立进程，与其它测试文件天然隔离，但同一文件内的多个
  测试函数默认并行跑，若拆成两个都要 mutate
  `ModelRegistry::global()` 单例的测试会互相竞态，顺序执行同一函数
  内两阶段可完全规避。已回退到修复前的 `llm_client.rs`
  （`git stash` 仅暂存该文件）验证集成测试确实会失败
  （`Err(HttpError { status_code: 429, .. })`，而非最终 `Ok`），
  证实测试的确捕获了这个缺陷，而非空转通过；随后 `git stash pop`
  恢复修复。**范围**：只处理非流式/流式请求发起前的失败（流式连接
  打开前的 `execute_streaming` 初始调用），不处理流式响应开始产出
  chunk 之后的中途失败——那种情况下透明重试会向调用方重复吐出局部
  内容，超出本项范围。`cargo fmt --check`、两条 clippy 门禁、
  `cargo test --workspace` 全过；`cargo test --workspace` 全绿，唯一
  失败项与前几项一致（`code_exec` 环境限制，非回归）。
- DONE V1.6 记忆运行时集成 + `SemanticMemory` ID 回归修复（evaluation
  §六"记忆分层"与"不稳定 ID 回归"）：动手前重新核实发现该条目描述已
  过时——Preference 层的运行时接入（`skill.toml` `[memory.preference]`
  解析、`SkillBuilder` 注册 `remember_preference` 工具、`ReActAgent`
  读取注入 persona）早在 U2.2（commit `f6f6375`/`4c78ef5`）就已完成，
  `docs/MEMORY_LAYERING.md` 也已确认"Preference is wired end to end"。
  真正符合"存储层完整但运行时零消费"描述的只剩 `ProjectMemoryStore`
  （L3.1 时只接入了 `ReActAgent::with_project_memory`，`yanshi-skills`
  /`yanshi-cli`/`yanshi-harness` 从未调用过，`skill.toml` 也没有
  `[memory.project]` 解析入口——`yanshi-memory/src/project.rs`
  模块注释里已经写明这是刻意搁置的后续工作）；以及
  `yanshi-memory/src/semantic.rs:547` 的
  `SemanticMemory::row_to_message` 在行数据损坏时会生成新的随机 ID 而非
  报错（`Q2.10.1` 当时只修了 `sqlite.rs` 那一份拷贝，`semantic.rs` 这份
  未同步）。**Fix（SemanticMemory）**：套用 `sqlite.rs::row_to_message`
  已有的错误上抛写法——UUID/时间戳解析失败时返回
  `MemoryError::StorageError`（角色解析的静默兜底保持不变，与
  `Q2.10.1` 的范围一致，避免范围蔓延）。**Fix（Project memory）**：
  `yanshi-skills/src/manifest.rs` 新增 `ProjectMemoryConfig`
  （镜像 `PreferenceMemoryConfig`：`enabled` 默认 `true`、可选
  `db_path`），`MemoryConfig` 新增 `project` 字段；`loader.rs` 补对应
  校验告警。`SkillBuilder::build_with_extra_tools`/`build` 的内部逻辑
  抽成 `build_core(..., project_root: Option<&Path>)`，两者签名不变
  （`project_root` 恒 `None`，不影响现有 8 处调用点中的其余 7 处）；
  新增不破坏签名的 `SkillBuilder::build_with_project_root(manifest,
  skill_dir, project_root)`，`project_root` 为 `Some` 且
  `[memory.project]` 启用时才打开 `SqliteProjectMemoryStore`
  （默认路径 `~/.yanshi/memory/<skill>.project.db`，与主存储/偏好
  存储的默认文件名互不冲突）并调用
  `agent.with_project_memory(store, project_key_for_path(project_root))`
  ——与偏好层不同，项目记忆两端都是全自动的（`ReActAgent` 在每轮结束
  后从 `shell`/`script`/`code_exec` 工具调用里自动抽取事实写入，读取
  也是自动注入 persona，没有 LLM 可调用的写入工具）。**范围收窄决策**
  （已与用户确认）：只给 `yanshi harness run`/`chat`（两者共享同一个
  `build_agent` helper，是唯一真正解析出"当前工作目录"概念的调用点，
  即 `--workspace`，缺省回退到 `std::env::current_dir()`）接入
  `build_with_project_root`；`yanshi-cli/src/commands/harness/run.rs`
  的 `build_agent` 新增 `workspace: &Path` 参数并透传，
  `harness/chat.rs` 的 `ChatConfig` 新增 `workspace: PathBuf` 字段随
  `build_chat_runtime` 一并传入。其余 7 处调用点（`skill run`/`chat`、
  `eval` 的 admission 分支、multi-agent 的三个分支、DAG `agent` 节点
  factory）保持调用原有的 `build`/`build_with_extra_tools`/
  `build_with_admission`，未接入项目记忆——它们本来就没有"项目目录"
  这个概念，`yanshi-memory/src/project.rs` 模块注释已明确写了这一
  点；一个声明了 `[memory.project]` 的 skill 在这些调用点上该字段是
  优雅空操作而非报错，与 `[memory.preference]` 缺失时的行为一致。
  **测试**：`semantic.rs` 新增 `row_to_message_returns_err_on_corrupt_uuid`
  回归测试（镜像 `sqlite.rs` 同名测试，插入畸形 UUID 行断言
  `get_all` 返回 `Err`）；`yanshi-skills/src/builder.rs` 新增 3 项
  测试——`project_root` 为 `None` 时不创建 db 文件（优雅空操作）、
  同一 `project_root` 下一个 agent 写入的事实被另一个新建 agent 读到
  （直接调用 `ProjectMemoryStore::record_project_fact` 模拟
  `ReActAgent::record_project_facts` 的效果，再用
  `preview_llm_messages()` 断言 persona 里出现该事实——因为项目记忆
  没有写入工具可以像偏好层测试那样直接 `tool.execute(...)`）、不同
  `project_root` 之间事实相互隔离；`yanshi-cli/src/commands/harness
  /run.rs` 新增 1 项 CLI 层测试端到端验证 `build_agent` 的 `--skill`
  分支把 `workspace` 正确透传给 `SkillBuilder::build_with_project_root`
  （构造真实 `skill.toml` + 预先写入项目事实 + 断言首轮 prompt 包含
  该事实）。`docs/MEMORY_LAYERING.md` 补充 Project memory 作为"第五个
  附加层"的 Wiring status 说明（镜像 Preference 那条），并更新
  `[memory.project]` 的 skill.toml 示例。`cargo fmt`/两条 clippy 门禁
  （`yanshi-memory`/`yanshi-skills`/`yanshi-cli`，`-D
  warnings`）、`cargo test -p yanshi-memory -p yanshi-skills -p
  yanshi-cli --lib` 全过，唯一失败项与既往一致
  （`build_registers_code_exec_tool`，沙箱环境缺容器引擎，非回归）。
- DONE V1.7 生产日志规范化（evaluation §五）：`yanshi-nodes`/
  `yanshi-nodes-ai`/`yanshi-core` 里大量 `println!`/`eprintln!`
  （含 emoji、部分打印 prompt/params/loop state）当日志用，污染
  CLI/JSON 输出并有轻度信息泄露——`text_to_image.rs` 打印完整 prompt、
  `mcp.rs` 打印完整 tool-call params、`template.rs` 打印渲染后的模板
  内容，都是原文明确点名的信息泄露面。**Fix**：三个 crate 共 90+ 处
  生产代码里的 `println!`/`eprintln!` 全部改走 `tracing`（测试模块内的
  跳过提示/调试输出、文档注释里的示例保持不动，不在本次范围内）。
  `yanshi-core` 沿用其既有的 `observability` cargo feature（`tracing`
  是可选依赖，`retry_executor.rs` 已有先例）——新加的调用点统一套用
  `#[cfg(feature = "observability")] tracing::{info,warn,debug}!(...)`
  写法，未开启该 feature 时行为等价于「什么都不打印」；`flow.rs` 的
  节点生命周期消息（执行/跳过节点、checkpoint 已保存、workflow 已完成/
  恢复）定为 `info!`，可恢复失败（checkpoint 保存失败、checkpoint 值
  损坏、锁中毒）定为 `warn!`，`while` 循环每轮状态 dump 与表达式求值定
  为 `debug!`。`yanshi-nodes`/`yanshi-nodes-ai` 此前完全没有
  `tracing` 依赖，也没有 `yanshi-core` 那种可选 feature 的先例——直接
  加为普通（非 optional）依赖：`tracing` 的宏在没有注册 subscriber 时是
  免费的空操作，引入新 feature flag 矩阵收益低于复杂度，与
  `yanshi-skills`/`yanshi-tools` 等 crate 的既有用法一致；节点
  生命周期/请求细节统一定为 `debug!`，真正的失败（MCP 断开失败、
  arxiv LaTeX 拉取失败、Tera 互斥锁中毒、JSON 解析失败）定为
  `warn!`。**特意保留不动的两处**（写清楚了原因，均已有 in-code 注释
  说明）：`shutdown.rs` 的 ctrl_c/SIGTERM 安装失败 `eprintln!`——这段代码
  必须在未开启 `observability` feature 的消费者里也能工作，本来就是刻意
  不走 `tracing` 的设计决策；独立的 `bin/echo_plugin.rs` 参考插件二进制
  ——它是插件作者的模板、运行在自己的进程里，不是污染宿主应用输出的
  库代码。**测试 + CI 门禁**：新增 `cargo xtask println-lint`（模仿既有
  `redaction_lint_at` 的写法）静态扫描三个 crate 的 `src/` 树，跳过
  `#[cfg(test)]` 模块（到文件结尾）、文档注释、以及一份显式的两文件
  白名单，支持 `// allow-println-lint: <reason>` 单行豁免；6 项单测覆盖
  跳过测试模块/文档注释/豁免注释/白名单文件/仅扫描指定三个 crate 等
  场景；已接入 `.github/workflows/quality.yml` 作为独立 job 并纳入
  `release-gate` 的 `needs` 列表；`cargo xtask println-lint` 对真实仓库
  跑通过（确认手工改写没有遗漏）。`cargo fmt --check`（workspace 级）、
  三个受影响 crate 的 clippy `-D warnings`（`yanshi-core` 额外过了
  `plugin`/`observability`/`plugin+observability` 三种 feature 组合）、
  `cargo test`（含 `--all-features`/默认 features 两种组合）、
  `cargo build --workspace`、`cargo xtask check-arch` 全部通过。

### V2 — 核心能力补齐（对标主流框架的地基）

- DONE V2.1 结构化输出强制：`yanshi-agents` 增加 `output_schema`
  支持——最终答案按 JSON schema 校验，校验失败触发一次重试/修正回合
  （而非现状的"尽力从截断 JSON 里恢复 answer 字段"）；provider 原生
  支持 JSON/schema 模式时优先使用原生能力而非纯 prompt 约束。范围经与
  用户确认扩大为两项：(a) 顺带修复研究中新发现的 `yanshi-llm`
  预置 bug——`response_format` 走 `parameters` HashMap 会被 Anthropic/
  Google 的参数白名单静默丢弃（和 `thinking` 字段之前踩过的坑同一类，
  `thinking` 已修好但 `response_format` 没有）；(b) `output_schema`
  同时覆盖 `ReActAgent` 和 `PlanExecuteAgent`（后者完全没有
  `VerificationStrategy` 类重试机制，等于要新搭一套）。
  **Fix（yanshi-llm）**：把 `response_format` 从 `parameters`
  HashMap 提升为 `ProviderRequest` 上的强类型字段（仿照 `thinking`
  字段的先例）。OpenAI/Moonshot/StepFun 无条件透传原生
  `response_format`（这三家本来就支持和 `tools` 同时使用）；Anthropic
  没有原生 `response_format`——当本次请求没有真实 `tools` 且非流式时，
  用强制单次 tool call 变通：合成一个 `input_schema` = 目标 schema 的
  工具，`execute()` 再把这次 tool_use 的 arguments 还原成普通文本
  `content`，对调用方完全透明；Google 映射到
  `generationConfig.responseMimeType` + `.responseSchema`，同样仅在无
  `tools` 时启用（Gemini 的 function-calling 和结构化输出两种模式是否
  能可靠共存未经验证，采用和 Anthropic 一致的保守跳过策略）。**Fix
  （yanshi-agents，ReActAgent）**：`ReActConfig` 新增 `output_schema`
  + 独立的 `max_schema_correction_attempts`（默认 2，与
  `max_verification_attempts` 互不干扰）。`output_schema` 一旦设置，
  `collect_tool_specs` 额外提供一个合成的 `final_answer` 原生工具（其
  `input_schema` = 目标 schema），复用现有六个 provider 都已打通的原生
  tool-calling 机制——特意不用 `response_format`/`.json_schema()`：
  ReAct 每轮调用本来就同时带着真实 `tools`，`response_format` 在
  Anthropic/Google 上两者不能共存，即便能共存，顶层单一 schema 也表达
  不出"要么执行动作要么给出这个答案"的多态选择，原生 tool-calling 天然
  能表达。调用 `final_answer` 会在批量/单次 tool 分发前被拦截，识别为
  最终答案而非真实工具调用。候选答案解析失败或校验不过时，把校验错误
  写回 memory 并循环重试（复用 `gate_candidate_answer` 的形状，独立计
  数）；和验证策略"次数用尽后强制接受"不同，schema 预算耗尽是硬错误
  `ReActError::SchemaValidationFailed`——schema 是调用方声明的硬约束，
  不是建议性评语，耗尽后静默返回不合规答案会破坏这个约束。复用现有
  `AgentStepKind::Verify`/`AgentEvent::VerificationCompleted`
  形状记录，不新增枚举变体（`AgentStepKind` 文档注释明确要求自定义
  runtime 复用现有变体、不要分叉）。system prompt 也补了 schema 感知
  的兜底说明，覆盖没有原生 tool-calling 的路径（如 `MockProvider`）。
  **Fix（yanshi-agents，PlanExecuteAgent）**：`PlanExecuteAgent`
  没有任何逐轮重试基础设施——它每次尝试只发一次 planner 调用（计划
  JSON 解析失败就是硬错误，不像 ReAct 有软恢复）。因此 schema 不匹配时
  重试的是整个 plan-and-execute 循环，而不是单次答案，这是"一次规划、
  一次执行"架构下更诚实的等价物。`PlanExecuteConfig` 同样新增
  `output_schema` + `max_schema_correction_attempts`；`run_with_context`/
  `run_as_flow` 保持公开签名和 `output_schema` 为 `None` 时的原有单次
  调用行为不变，原函数体下沉为私有的 `*_once`，公开方法变成薄重试
  包装：`final_answer` 未通过校验时，把校验错误和被拒答案塞进下一次
  尝试的 `context.input`，靠 `*_once` 自己的开场逻辑把它记成新的一轮
  用户输入——效果上和 ReAct 靠同一 session 记忆延续把"哪里错了"带给
  模型一致，只是用独立尝试而非内部循环实现；timeout/cancelled/
  max_steps/cost_limit 等其它终止原因原样透传，只有真正的 FinalAnswer
  才会被 schema 拦截。预算耗尽同样硬错误
  `PlanExecuteError::SchemaValidationFailed`。**测试**：
  `yanshi-llm` 新增 Anthropic/Google 的 `build_request_body`
  单测（含"无 tools 时走原生""有 tools 时跳过原生回退 prompt-only"
  三态）+ `provider_consistency.rs` 新增 6 provider 的
  response_format/json_schema 请求编码断言与 Anthropic 合成 tool 完整
  往返测试；`yanshi-agent-spi` 抽出可复用的
  `validate_json_str_against_schema`/`validate_json_against_schema`
  （`delegation::validate_output` 同步改造复用，避免第三份重复的
  `jsonschema` 样板）；`yanshi-agents` 覆盖 V2.1 验收标准原文场景
  （ReActAgent 和 PlanExecuteAgent 各一份：schema 不匹配触发修正回合、
  第二次尝试成功、最终产出合规输出）+ 两个 agent 各自的"预算耗尽硬
  错误"用例；`agent_trace_compat.rs` 的 golden fixture 测试原样通过，
  确认没有引入新的 `AgentStepKind`/`AgentEvent` 线上格式变更。
  `cargo fmt`/`clippy -D warnings`（`yanshi-llm`/`yanshi-agent-spi`/
  `yanshi-agents`）、`cargo test --workspace` 相关 crate 全过（唯一
  失败项与既往一致：`code_exec` 沙箱环境缺容器引擎，非回归）、
  `cargo build --workspace --all-targets`、`cargo xtask check-arch`/
  `println-lint` 全部通过。文档：`docs/LLM_PROVIDERS_MATRIX.md` 补充
  `response_format` 字段行；`docs/AGENT_RUNTIME.md` 的 ReAct/
  Plan-and-Execute 两节各补一段结构化输出说明。
- DONE V2.2 token 级流式事件：`yanshi-harness` 的 `HarnessEventBody`
  增加 `TokenDelta` 事件类型，LLM 流式 token 经 SSE 透传给客户端；CLI/UI
  支持增量渲染而非等到 step 完成才显示。范围经与用户确认扩大为两项：
  (a) `ReActAgent` 的所有 LLM 调用默认全部走流式（而非仅在 Harness 调用方
  需要 token 级事件时才切换），一条调用路径可推理可测试，同时顺带堵上一
  个潜在缺口——某个只支持流式的 model config（`requires_streaming: true`）
  过去经由 `execute_full` 根本无法被 `ReActAgent` 使用；(b) 直接透传
  provider 原始输出流的文本片段，不做增量 JSON 解析提炼干净 prose（增量
  JSON 解析器超出本次范围，`docs/AGENT_RUNTIME.md` 有说明）。
  **Fix（yanshi-llm）**：`client/streaming.rs` 新增
  `collect_streaming_response(stream, on_chunk)`——drain 一个
  `StreamingResponse`，对每个 chunk **await** 调用方提供的
  `on_chunk`（`FnMut(&StreamChunk) -> impl Future<Output=()>`，特意是异步
  闭包而非同步闭包+`tokio::spawn`：后者是 fire-and-forget，无法保证并发
  任务间的到达顺序，直接违反验收标准"delta 序列被完整转发且顺序正确"），
  同时把流拼接还原成和 `execute_full` 一致的聚合 `LLMResponse`
  （`content` 拼接、`tool_calls` 按 `index` 从 `tool_call_deltas` 分组重
  建、`stop_reason`/`thinking`/`raw_metadata` 是尽力而为——经排查
  `yanshi-agents` 里没有任何代码读取这三个字段，因此这个简化是安全
  的）。`LLMClient`/`LLMClientBuilder` 新增
  `execute_streaming_collected(on_chunk)` 组合 `execute_streaming()` +
  `collect_streaming_response`。`providers::TokenUsage` 与
  `client::streaming::TokenUsage` 是两个同形状但不同的类型，需要手动逐字
  段转换（同一坑此前在别处也踩过）。**Fix（yanshi-llm，
  MockProvider）**：重写 `MockStreamingResponse`——按字符切分（UTF-8
  安全）成多个 chunk，最后追加一个携带完整 `usage` +
  `tool_call_deltas`（从 `execute()` 同款的 tool_calls 转换而来）的
  `is_final` chunk；此前 mock 的流式路径完全不产出 `tool_call_deltas`，
  是纯 no-op。**Fix（yanshi-agent-spi）**：`AgentEvent` 新增
  `TokenDelta { session_id, step_index, delta, is_final, timestamp }`
  变体——`AgentEvent` 已经是 `#[non_exhaustive]`，新增变体无需改动任何
  下游穷尽匹配（workspace 全量编译验证）。该变体故意**不**经由
  `emit_and_push!` 累积进 `AgentRunResult.events`，只直接推给
  `live_sink`——逐 token 事件如果永久嵌进每条落盘 trace/checkpoint 会造
  成膨胀，而它的全部价值只在于被实时观察。**Fix（yanshi-agents，
  ReActAgent）**：`run_turn_llm_call` 把 `builder.execute_full()` 换成
  `builder.execute_streaming_collected(...)`，闭包里把非空 chunk 实时
  转发成 `AgentEvent::TokenDelta` 发给 `self.live_sink`；换掉的这个
  future 原样交给既有的 `race_with_limits(...)`，超时/取消语义不变。
  **Fix（yanshi-harness）**：`yanshi-agent-spi` 的
  `HarnessEventBody`（这个枚举**不是** `#[non_exhaustive]`，是 P-H.0 冻结
  的封闭集合）新增 `TokenDelta(TokenDeltaPayload { step_index, delta })`
  ——加性变更，wire 版本仍是 `harness/1`；`HarnessAgentEventBridge::emit`
  新增匹配分支把 `AgentEvent::TokenDelta` 经既有的
  `SeqAllocator`/`dispatch_body` 机制转发，`is_final` 不进 wire payload
  （只是 LLM 客户端内部的 chunk 重建标记）；`translate_inner_events`
  这条事后重建路径**不**处理它——`token_delta` 本质是纯 live-only。同步
  修复了因此产生的 4 处穷尽匹配编译错误
  （`yanshi-server/src/harness_live.rs::harness_event_kind`、
  `yanshi-cli` 的 `harness inspect/replay/resume` 三个渲染器）。
  **Fix（yanshi-cli）**：`harness chat` 的 `ChatProgressSink` 和
  `harness run --output text` 新增的 `TypingProgressSink` 把同一 step 内
  连续的 `token_delta` 累积进一行，用 `\r\x1b[K` 原地重绘（"打字机"效
  果），在下一个 `step_started` 或任意一条离散 progress 行打印时清空
  累积区；两者都是 TTY-gated（走 stderr），不触碰 stdout 上权威的最终答
  案输出。**Fix（yanshi-ui）**：`HarnessSessionDetail` 新增独立的
  `liveTyping` 累积区（`token_delta` 是纯 live-only kind，从不出现在
  `/events/history`），渲染成时间线上方一条随流增长的文本行，而不是并入
  离散事件列表；在下一个 `step_started`、resume rerun 或 fallback 重新拉
  取时清空。**测试**：`yanshi-llm` 新增 4 个
  `collect_streaming_response` 单测（顺序转发+拼接、tool_call_deltas 按
  index 重组、单 chunk 场景与非流式行为一致）+ 2 个 `MockProvider` 新测
  （多 chunk 拼接、tool_calls 透出）；`yanshi-agents` 新增
  `run_with_context_emits_token_delta_events_in_order_matching_the_response`
  （验证 `TokenDelta` 顺序正确且不进入 `AgentRunResult.events`）；
  `yanshi-harness` 新增
  `bridge_translates_agent_event_token_delta_into_harness_token_delta`；
  `yanshi-cli` 新增
  `chat_progress_sink_accumulates_token_deltas_and_resets_on_step_boundary`；
  `yanshi-ui` 新增 `lib/harness.test.ts`（`extractTokenDelta` 的载荷
  窄化规则）。`cargo test --workspace` 相关 crate（255 + 80 + 208 +
  cli harness 全过，唯一既往失败项 `code_exec` 沙箱缺容器引擎非回归）、
  `cargo build --workspace --all-features`、`cargo clippy -D warnings`、
  `cargo fmt --check` 全部通过；`yanshi-ui` 的 `tsc --noEmit` +
  `vite build` + 全部既有 e2e/单测通过。文档：`docs/HARNESS_MODE.md` 的
  frozen kind 表补 `token_delta` 行 + 一段 live-only 语义说明；
  `docs/STABILITY.md` 的 `HarnessEvent` 封闭 kind 集合补充；
  `docs/AGENT_RUNTIME.md` 的 ReAct Runtime 一节补一段"Token-level
  streaming (V2.2)"说明。
- DONE V2.3 通用 HITL 中断/恢复：在 V2.4 的逐步 checkpoint 之上新增
  "运行中向用户提问、待用户回答后带答案 resume、正确续接循环状态"的
  原语，区别于现有的工具调用审批机制（审批门控的是"某次具体工具调用
  是否放行"，这里是通用信息请求，且整个循环都会停下来）。经与用户三选
  一问询确认的三个决定全部落实：① 先做 V2.4 再做 V2.3（而非弱化成
  memory-replay 近似实现）；② 原语同时覆盖 `ReActAgent` 和
  `PlanExecuteAgent`；③ 交付范围为 CLI + server API + Web UI 全端打通。
  **核心类型（yanshi-agent-spi，L0）**：`AgentStopReason` 新增
  `AwaitingInput{question}`（第 11 个变体，依旧刻意不标
  `#[non_exhaustive]`，逼每个消费者穷尽匹配处理新变体）；
  `AgentLoopCheckpoint` 新增 `pending_question: Option<String>`
  （`#[serde(default, skip_serializing_if)]`，旧 checkpoint 反序列化
  自动补 `None`，不需要 schema version bump）；`harness/event.rs` 的
  `StopReason` 线上枚举新增 `AwaitingInput`，`HarnessEventBody` 新增
  `InterruptRequested{step_index,question}` / `InterruptAnswered{
  step_index,answer}`（照抄 `ApprovalRequested`/`ApprovalDecided` 的
  形状）；`AgentRuntime` trait 新增 `resume_from_loop_checkpoint` 方法
  （`async_trait` 允许带默认实现直接落地——默认返回
  "runtime 不支持"错误，`HandoffSupervisor` 等无 loop checkpoint 概念
  的实现者不必被迫覆写）。**穷尽匹配站点**：`AgentStopReason` 新增变体
  逼编译器暴露的 7 个站点（`yanshi-harness::stopped_payload_from`、
  `yanshi-server::live_execute`、`yanshi-agents::checkpoint::
  should_clear_checkpoint`、eval runner/CLI replay 的
  `stop_reason_label` 两处、`react::agent::answer_from_result`
  委托子调用分支、CLI `harness run::format_stop_reason`）全部补齐，另
  外在实现过程中新发现第 8 个此前研究阶段未确认到的站点
  `yanshi-harness::tracing_bridge`（`StopReason`→`TraceStatus` 映
  射），映射到 `TraceStatus::Cancelled{reason:"awaiting user input"}`
  （暂停未失败的语义，复用已有变体而非新增）。**ReActAgent**：新增
  `pub const ASK_USER_TOOL_NAME`，无条件注册进
  `collect_tool_specs`（不像 `final_answer` 需要 `output_schema` 才
  注册——`ask_user` 不需要任何配置就安全，多一条工具 spec 只多几个
  token，不改变行为）；`run_one_turn` 在 `final_answer` 扫描之前先扫
  `ask_user`，命中时显式在拦截点单独存一份带 `pending_question` 的
  checkpoint（不能依赖上一次 `TurnStep::Continue` 存的那份——那份落后
  整整一轮，不包含这次提问）；`resume_from_loop_checkpoint` 新增
  `answer: Option<String>` 参数，双向校验（有 pending_question 无
  answer / 无 pending_question 有 answer 都是硬错误），命中时把答案写
  回 memory 再进入 turn loop。**PlanExecuteAgent**（更难：循环中途没有
  LLM 重入点）：复用同一个 `ASK_USER_TOOL_NAME`，在 `run_plan_execute_
  loop` 和 `resume_from_loop_checkpoint` 两处循环体（本来就是重复代
  码）里、`max_tool_calls` 检查之前拦截，命中时在**当前**
  `plan_position`（不 `+1`，这步还没算完）存 checkpoint；resume 时把
  答案当作这一步的合成工具结果推进 `observations`，从
  `plan_position + 1` 继续。**yanshi-harness**：
  `HarnessAgentEventBridge::emit` 新增 `InterruptRequested` 分支；新增
  `HarnessRuntime::resume_from_interrupt(options,checkpoint,answer)`
  ——不复用整个 `run()`（那个会话已经有过 `session_started`，不需要重
  新装配 context providers），只重新接上事件桥（保证 SSE 不断流）、
  先落一条 `InterruptAnswered`、再转发进
  `AgentRuntime::resume_from_loop_checkpoint`；只支持
  `InnerRuntime::Opaque`，`--context-refresh` 走的
  `InnerRuntime::TurnDriven` 暂不支持（标注为已知缺口而非静默错处
  理）。`run()`/`resume_from_interrupt()` 共用尾段抽成
  `finish_inner_result` 消除重复。**yanshi-db**：新增迁移
  `0007_harness_loop_checkpoints.sql`（`harness_loop_checkpoints` 表，
  session 一行、覆盖写；`harness_sessions` 新增 `pending_question` /
  `pending_question_step_index` 两个可空列，供 `GET .../interrupt` 路
  由直接读列而不必反序列化整份 checkpoint payload）+
  `DbLoopCheckpointer`（Postgres 版 `AgentLoopCheckpointer`，upsert /
  select / delete 三件套，选它而不是复用 CLI 的
  `FileLoopCheckpointer` 是因为 server 侧完全没有本地磁盘惯例、却有贯
  彻始终的 Postgres-everything 模式）；`HarnessSessionRepo` 新增
  `set_pending_question`/`clear_pending_question_for_resume` 两个方
  法。**yanshi-server**：`LiveHarnessExecutor` 默认（无需 flag）挂
  载 `DbLoopCheckpointer`；新增 `HarnessSessionExecutor::resume_
  interrupt` trait 方法（带默认实现——对不支持中断恢复的执行器直接标
  记会话失败并给出清楚错误，`StubHarnessExecutor` 天然落在这个默认分
  支，因为它从不真正跑 agent 循环，也就永远不会产生
  `awaiting_input` 会话）；`live_execute`/新 `live_resume_interrupt`
  共用的 `finish_live_result` helper 把 `AwaitingInput` 特判成"落
  `pending_question*` 列、不落终态"而非 `update_status`；新增路由
  `GET /v1/harness/sessions/{id}/interrupt`（直接读会话行的
  `pending_question*` 列）+
  `POST /v1/harness/sessions/{id}/interrupt/answer`（400 校验会话是
  否处于 `awaiting_input`，校验通过后清空 pending 列、`tokio::spawn`
  异步派发 resume，立即返回刷新后的行——照抄 `:resume` 路由的
  fire-and-forget 模式）。`HarnessSessionStatus` 新增 `AwaitingInput`
  （刻意不进 `is_terminal()` 的匹配列表，因为整个设计意图就是它能通过
  `.../interrupt/answer` 恢复而不是要求走"先终态、再 `:resume`"那条
  路）。**yanshi-cli**：修掉一个既存 bug——`build_agent` 此前完全不
  看 `runtime_kind`，`harness run --runtime plan_execute` 会悄悄还是
  跑 ReAct；改造后 `build_agent` 返回 `BuiltHarnessAgent` 枚举
  （`React`/`PlanExecute` 两个变体各自装箱，绕开
  `clippy::large_enum_variant`），`--model` 路径按 `runtime_kind` 真
  的分派到对应类型，`--skill` 路径（`SkillBuilder` 目前只会造
  `ReActAgent`）遇到 `--runtime plan_execute` 直接报清楚的错而不是悄
  悄退化；`--context-refresh` + `--runtime plan_execute` 同理报错（
  PlanExecute 没有逐轮 LLM 重入点，做不了 `TurnDrivenRuntime`）。
  `PlanExecuteAgent` 补齐 `tools()`/`with_tools()` 访问器（照抄
  `ReActAgent` 的形状），CLI 层的审批门管道包装才能对两种 runtime 都
  生效。`harness resume-loop` 新增 `--runtime`/`--answer` 两个 flag（
  补上 V2.4 遗留的 TODO），`--answer` 省略且 checkpoint 恰好卡在提问
  上时退化成交互式 stdin 读一行。`harness run` 在 `AwaitingInput` 上
  做 TTY 门控的内联问答：真终端就打印问题、阻塞读一行、原进程内直接
  resume；非交互（`assert_cmd` 测试、管道场景）打印问题后干净退出
  （exit 0，不是失败）并指向 `resume-loop --answer` 作为后续操作。
  `harness chat` 的 REPL 此前完全没有挂载 loop checkpointer（因此
  `AwaitingInput` 根本无法 resume）——补上默认挂载的
  `FileLoopCheckpointer`，一轮停在 `ask_user` 上时打印问题、把下一行
  当作答案走 `resume_from_interrupt` 而不是开新一轮；`/` 开头的 REPL
  命令会清空待答问题状态（新建会话/切模型等都让原问题失去意义）。
  **yanshi-ui**：新增 `InterruptCard` 组件，结构照抄
  `ApprovalCard`（`{pending,busy,onAnswer}` props，textarea + 提交按
  钮），`fetchInterrupt` 用和 approvals/session 同一个 2 秒轮询节奏拉
  `GET .../interrupt`，提交后立即 `fetchSession()` 刷新而不等下一个
  轮询 tick；`schemas.ts` 新增 `PendingInterruptSchema`（`{question,
  step_index}` 或整体 `null`——每个 session 同一时刻最多一个待答问
  题，不像审批是列表，不需要包装数组）；`harnessStatusTone` 补
  `awaiting_input` 分支（当作 `running` 同一档"活跃、非终态"处理）；
  新增网格区域 `interrupt`（横跨两列，位于 summary/approvals 那一行之
  上），仅当有待答问题时渲染，为空时不占视觉空间。**测试**：
  `yanshi-agent-spi` 新增 checkpoint 序列化往返 + `StopReason`/
  `HarnessEventBody` 往返单测；`yanshi-agents` 新增 ReAct 三个集成
  测试（暂停+resume 走到与不中断对照跑一致的答案、两个方向的校验错
  误）+ PlanExecute 两个同构测试；`yanshi-harness` 新增
  `resume_from_interrupt` 转发测试；`yanshi-db` 新增
  `DbLoopCheckpointer` 两个单测（session_id 校验）+ 两个
  `YANSHI_DATABASE_TEST_URL` 门控的集成测试（save→模拟重启→
  load→校验 `pending_question` 往返、load 未知 session 返回
  `None`）；`yanshi-server` 新增 7 个 `harness_interrupt_routes`
  集成测试（GET 空/有值、POST 400×2、happy path 校验行状态+错误落
  地、跨租户 404×2）；`yanshi-cli` 新增 3 个 `build_agent` 回归单
  测（`--runtime plan_execute` 真的造出 `PlanExecuteAgent`、默认造
  `ReActAgent`、`--skill`+`plan_execute` 报清楚错误）+ 2 个跑真实二
  进制的端到端 smoke 测试（对 offline mock provider）：`harness chat`
  在 `ask_user` 上暂停、REPL 下一行当答案 resume 拿到最终答案；
  `harness run` 非交互场景打印问题干净退出，独立的 `resume-loop
  --answer` 进程从 checkpoint 续接拿到同一个最终答案。`yanshi-ui`
  的 `tsc --noEmit` + `vite build` 全过，e2e 补一条"新会话默认不渲染
  interrupt 卡片"断言。全工作区门禁：`cargo build --workspace
  --all-features --all-targets`、`cargo fmt --check --all`、
  `cargo clippy --workspace --all-features --all-targets -D
  warnings`（零警告）、`cargo xtask check-arch`、`cargo xtask
  println-lint`、`cargo test --workspace --all-features`（156 个测试
  二进制全过，唯一失败项仍是既往已知的 `code_exec` 沙箱缺容器引擎问
  题，与本次改动无关）全部通过。
- DONE V2.4 agent 循环级持久 checkpoint：为 ReAct/PlanExecute 循环增加
  每步 checkpointer（区别于 V1.1/V1.2 讨论的 DAG checkpoint），使长程
  agent 运行可以从循环中途某一步恢复，而不是像现状一样只能重放只读/
  幂等的未决工具调用（其余情况走 `PartialRunUnsupported`）。**测试**：
  长程 ReAct 循环在第 N 步后模拟进程终止，resume 后从第 N 步（而非
  从头）继续，最终结果与不中断的对照跑一致。
  **设计（yanshi-agent-spi，L0 契约）**：新增
  `checkpoint.rs`——`AgentLoopCheckpoint`（steps/events/step_index/
  iteration/tool_calls/verification_attempts/schema_correction_attempts/
  last_tool_call/recent_tool_calls/cumulative_cost_usd/system_prompt/
  user_input/trace_context 直接从运行时循环状态逐字段拷贝；
  schema_version/session_id/runtime_kind/created_at 是新增字段；
  cancellation_token/run_started_at/between_turn_hook 等进程本地句柄，
  以及 max_iterations/max_tool_calls/timeout_ms/budget_tokens/
  cost_limit_usd/loop_detection 等运行**配置**字段，刻意不放进
  checkpoint——resume 时从当次调用的全新 `AgentContext` 重新派生，因此
  resume 时可以合法地给一个比原始运行更宽松的 limits，让卡住的长程
  运行有机会跑完）+ `AgentLoopCheckpointer` trait（`save`/`load`/
  `clear`，实现须容忍 save 失败不中断当前回合，镜像 DAG checkpoint 的
  非致命姿态）+ `LoopCheckpointerHandle`（`Arc<dyn Trait>` 的
  `Debug`/`PartialEq` 包装，和已有的 `EventSinkHandle`/
  `BetweenTurnHookHandle` 同款模式）。`AgentContext` 新增
  `loop_checkpointer` 字段（`#[serde(skip)]`，和另外两个进程本地句柄
  字段一致）+ `with_loop_checkpointer(...)`。**为什么不从
  `AgentStep`/`AgentEvent` 历史重放反推计数器**：`verification_attempts`
  和 `schema_correction_attempts` 都复用同一个
  `AgentStepKind::Verify{approved,feedback,attempt}` 形状、各自独立计
  数，只能靠 `feedback` 文本前缀字符串匹配才能区分是哪一种重试——直接
  逐字段序列化彻底绕开这个脆弱点。**设计（yanshi-agents，L3
  实现）**：新增 `checkpoint.rs`——`FileLoopCheckpointer`（每 session
  一个 JSON 文件，`.tmp` 写入 + 原子 rename，`default_dir` 与
  `yanshi_harness::default_session_dir` 的 `<root>/harness/sessions`
  同级放在 `<root>/harness/loop_checkpoints`；重新实现（而非导入，因为
  `pub(crate)`）`yanshi-core::checkpoint::is_safe_path_component` 校
  验 `session_id`，因为它是外部输入且会拼进文件路径）；刻意不复用
  `yanshi-core::checkpoint::CheckpointManager`——那是按 DAG 节点粒度
  的审计式历史，和"只关心最新一个循环位置"的单文件快照是两种不同的
  形状。`should_clear_checkpoint`（对 `AgentStopReason` 十个变体逐一穷
  尽匹配——该枚举刻意不是 `#[non_exhaustive]`，就是要让每个消费者显式
  处理新变体，这里跟随同一约定而不是用通配符开一次先例——
  `FinalAnswer`/`StopCondition`/`Error` 清除，其余七种保留）两个 runtime
  共用，放在这里而非某一方的文件里。**ReActAgent**：`run_with_context`
  在每个 `TurnStep::Continue` 后保存 checkpoint、`TurnStep::Stop` 时按
  停因决定是否清除；`ReActLoopSession::next_turn`（`--context-refresh`
  走的 turn-driven 通道）拿到同款 hook，不然会悄悄比默认路径少一份
  checkpoint 覆盖。新增 `resume_from_loop_checkpoint`，**刻意不与**
  `resume_with_context` 合并——后者回答"运行带着一个未决工具调用停
  下，重放它、然后走全新 `init_run` 开始一轮全新循环"；前者回答"进程
  在没有任何干净停因的情况下死掉，跳过 `init_run`，把恢复出来的循环状
  态直接接回 turn loop"——两种前提、两种循环状态来源完全不同，两个
  清晰命名的方法比一个靠 `Option` 分支的方法更诚实。不会把用户消息重
  新写回 memory（假定 memory store 已经在中断前那次运行里写过了）——
  这意味着 resume 依赖底层 `MemoryStore` 也是持久化的、按 session_id
  索引的（如 `SqliteMemory`，而非进程内的 `SessionMemory`）。
  **PlanExecuteAgent**：`run_with_context_once` 变成薄包装，调用新的
  `run_plan_execute_loop` 拿到结果后统一按停因清除 checkpoint（不用在
  每个提前 return 分支各写一遍）；执行循环新增显式 `plan_position` 游
  标（区别于 `step_index`——纯推理型 plan step 不会推进 `step_index`），
  每完成一步（工具调用或纯推理）保存一次 checkpoint。`resume_from_
  loop_checkpoint` 比 ReAct 版本更简单：完全不需要再调用 LLM——plan 已
  经整份冻结进 checkpoint（`plan_steps` 存的是整个 `PlanExecutePlan`
  即 `.plan` + `.final_answer`，字段名有点误导但注释里写清楚了），直
  接跳过 planner 调用，从 `plan_position` 处继续执行循环。
  **yanshi-harness**：`HarnessRunOptions` 新增 `loop_checkpointer`
  字段（照抄 `between_turn_hook` 的存储方式）+
  `with_loop_checkpointer(...)`；`HarnessRuntime::run` 里紧跟
  `between_turn_hook` 转发那行加一行转发——`InnerRuntime::Opaque` 和
  `InnerRuntime::TurnDriven` 两条路径共用同一个 `agent_context`，一次
  插入两条路径都覆盖。**yanshi-cli**：`harness run` 默认（无需 flag）
  挂载 `FileLoopCheckpointer`——每回合一次小 JSON 写入很便宜，跟 DAG
  checkpoint 本身没有 interval 旋钮的先例一致，默认关闭反而是需要额外
  理由的选择。新增子命令 `yanshi harness resume-loop <session_id>`
  （特意不叫 `resume`——那个名字已经被"只读回放 JSONL"占用且有文档
  明确说明），复用 `run.rs::build_agent` 保证 tool registry/memory/
  skill 装配和原始运行一致，加载 checkpoint 后直接调用
  `resume_from_loop_checkpoint`——绕过完整的 `HarnessRuntime`/
  `HarnessRunOptions` 包装（无实时事件流、无 approval-gate 重新包装）
  是刻意的最小化范围：这一轮只需要证明循环续接是对的，完整
  `HarnessRuntime` 级别的 resume 留给后续（大概率并入 V2.3）。
  **范围裁剪（经与用户三选一问询确认，详见上面 V2.3 条目的重排说
  明）**：不碰 PlanExecute 的 DAG 编译执行路径（`compile_plan_to_flow`/
  `run_as_flow`，那条路径本来就通过 DAG checkpoint 免费获得逐步恢复）；
  不碰 server 路由或 Web UI（`HarnessRunOptions.loop_checkpointer`
  默认 `None`，server 行为不受影响）；不碰
  `AgentNodeResumeContract`/`PartialRunUnsupported`（那是另一个问题：
  DAG 里嵌入的 agent 事后分类未决工具调用是否安全重放，和"循环中途
  续接"是两回事，这次不试图统一两者）。**测试**：`yanshi-agent-spi`
  新增 4 个 `AgentLoopCheckpoint` 序列化往返单测；`yanshi-agents`
  新增 8 个 `FileLoopCheckpointer` 单测（原子写、schema 版本拒绝、
  session_id 路径安全校验等）+ ReActAgent 两个集成测试（分别走
  `run_with_context` 和 turn-driven 通道，用"2 次 checkpoint save 后
  触发已配置好的 cancellation token"而不是真的杀进程来确定性模拟中
  断，第三个全新 `ReActAgent` 实例只拿到剩余轮次的 mock 脚本 resume，
  断言最终答案与不中断对照跑一致、且 LLM 恰好只多调用了剩余轮次那么
  多次）+ PlanExecuteAgent 同构的一个集成测试（3 步计划中断在第 2 步，
  resume 后不再调用 planner、只执行剩余 1 步）；`yanshi-harness`
  新增一个转发测试（scripted 内层 runtime 模拟保存 checkpoint，确认
  `HarnessRunOptions::with_loop_checkpointer` 真的传到了内层
  `AgentContext`）。`cargo test --workspace` 相关 crate 全过、
  `cargo build --workspace --all-features --all-targets`、
  `cargo clippy --workspace --all-features --all-targets -D warnings`
  （全工作区零警告）、`cargo fmt --check --all`、`cargo xtask
  check-arch`、`cargo xtask println-lint` 全部通过。另外用真实二进制
  做了一次端到端人工验证：`harness run` 跑在真实进程里，3 秒后发送
  真实 SIGINT（而非单测里的确定性取消），确认 checkpoint 文件在第一
  轮完成后落盘；`harness resume-loop` 用剩余的 2 条 mock 脚本正确从
  `step_index: 4` 续接、得到与预期一致的最终答案，且成功完成后
  checkpoint 文件被清除。文档：`docs/AGENT_RUNTIME.md` 新增"Agent-
  loop-level persistent checkpoint (V2.4)"一节；`docs/HARNESS_MODE.md`
  CLI surface 一节补充 `resume-loop` 子命令说明；`docs/STABILITY.md`
  新增 `AgentLoopCheckpoint` 信封条目（Experimental——首次发布，尚无
  fixture 化兼容性测试）。

### V3 — 服务能力"够用即止"加固（服务次之，消除默认危险而非做全套平台）

- DONE V3.1 默认档 fail-closed：修掉一个"定义了但从没接上"的死字段——
  `yanshi-tool/src/security_profile.rs` 的 `AuthDefaults.
  allow_unauthenticated_loopback`（Dev/Local 为 `true`、Production 为
  `false`）在整个代码库里从未被任何地方读取过，尽管这个字段名本身已经
  精确编码了本条要修的语义（"无 token 只有在绑定 loopback 时才可以接
  受"）。真正的漏洞：一个真实的 `yanshi serve` 进程今天会在默认
  `Local` 档下，`bind` 绑到非 loopback 地址（`PORT` 环境变量——常见
  PaaS 约定——一旦设置就恒定绑 `0.0.0.0`）却完全没配置鉴权 token 的情
  况下，正常启动、只打一行 `warn!`。**修复**（不改全局
  `SecurityProfile::default()`，也不把 Helm 的 `securityProfile: local`
  改成 `production`——新检查本身是"无论 profile 名义是什么，非
  loopback+无 token 一律拒绝"，已经足够堵住这个口子，不需要更大范围
  改动默认值）：`yanshi-server/src/serve.rs` 新增私有 helper
  `allows_unauthenticated_bind(profile, bind)`（复用
  `profile.defaults().auth.allow_unauthenticated_loopback &&
  bind.ip().is_loopback()`），同时接入两条既有的独立鉴权判定路径——
  `run()`（真实启动路径，之前只有 `warn!` 从不拒绝）现在会直接返回新
  增的 `ServeError::UnauthenticatedPublicBind` 拒绝启动；
  `build_startup_report()`（`--check`/`run_check` 用）在既有的
  `require_token` Fail 分支和既有的通用 Warn 分支之间插入新分支，命中
  时 `errors.push` + `readiness.promote(Fail)`。刻意不碰
  `yanshi-server/src/auth.rs` 的 `resolve_auth_config`/
  `resolve_auth_config_from_env` 公开签名（会牵连 13 处既有单测调用
  点）——`serve.rs` 本来就独立拥有 `SocketAddr` 且已经各自重新推导一遍
  鉴权状态，新 helper 放在这里最小侵入。**部署配置联动**：
  `docker-compose.yml`（`PORT: "3000"` 恒定绑 `0.0.0.0`）和 Helm chart
  （pod 的 `containerPort` 同理恒定非 loopback）两者今天出厂配置都恰
  好踩中这个不安全组合——`docker-compose.yml` 补一个可见的占位符
  `YANSHI_API_TOKEN: "local-dev-change-me"`（不是真机密，只是让
  `docker compose up --build` 零配置体验继续可用，注释写清楚必须在真
  实暴露前替换）；Helm chart 新增**按需**（不像 `DATABASE_URL` 那样强
  制）的 `secretEnv.YANSHI_API_TOKEN` 接线（`values.yaml` 新字段 +
  `templates/secret.yaml` 的 `if` 判断从只看 `DATABASE_URL` 扩成
  `DATABASE_URL` 或 `YANSHI_API_TOKEN` 任一非空即建 Secret、每个
  `stringData` key 各自独立判空 + `templates/deployment.yaml` 新增一
  段仅在配置了 token 来源时才渲染的 `YANSHI_API_TOKEN` env
  block）——没有这个，`values.yaml` 原本就没有任何官方支持的方式喂
  token 进去，新检查落地后一个全新 install 会直接不可用而不只是"默认
  不安全"。用 `helm lint charts/yanshi` + `helm template` 三种场
  景（无 token / `secretEnv.YANSHI_API_TOKEN` / `existingSecret`）
  实测渲染结果符合预期。**测试**：`yanshi-server/src/serve.rs`
  `mod tests` 新增 3 个：`run_check_local_profile_no_token_non_loopback_
  bind_fails`（`bind=0.0.0.0:8080` 无 token → `Fail`，错误信息含具体
  bind 地址）、`..._loopback_bind_is_warn_only`（默认 loopback bind 无
  token 仍是 `Warn`，防回归）、
  `run_check_production_no_token_non_loopback_bind_still_uses_missing_
  token_message`（确认 Production 既有的"requires bearer auth"错误分
  支没被新分支盖掉，防回归）；第一个测试需要额外持有既有的
  `tenant_tokens_env_lock()` 守卫（否则会和其它并发测试竞争
  `YANSHI_API_TOKEN_TENANTS` 环境变量，曾在实现过程中实际触发过一
  次 flaky 失败，加锁后跑 3 轮确认稳定）。`yanshi-cli` 既有的
  `serve_cli_tests.rs` 4 个测试确认不受影响（均用 loopback bind）。
  全工作区门禁（`cargo build/fmt/clippy -D warnings/check-arch/
  println-lint/test --workspace --all-features`，156 个测试二进制全
  过，唯一失败项仍是既往已知的 `code_exec` 容器引擎缺失问题，与本次
  改动无关）全部通过。**文档**：`docs/DEPLOYMENT.md` 新增"PORT and
  PaaS-style public binding (V3.1)"一节（解释触发条件、三种满足方
  式、以及研究过程中顺带发现的一个相关但不在本条修复范围内的小坑——
  `yanshi-cli`'s `serve` 子命令把 `YANSHI_SERVE_BIND` 设到子进
  程但没清掉从父 shell 继承来的 `PORT`，而 `resolve_bind()` 里 `PORT`
  优先级更高，所以 `yanshi serve --bind 127.0.0.1:...` 在父 shell
  设了 `PORT` 时仍可能被悄悄绑到公网——只记录成已知注意事项，不在本条
  改动绑定优先级，因为本条的鉴权修复已经让这个场景从"静默暴露"变成
  "拒绝启动"，安全影响已经被消解），更新 Docker Compose / Helm 两节的
  既有文字反映新的 token 要求；`docs/SECURITY_PROFILES.md`
  的"Current Wiring"一节补一段说明 profile 表格里"Unauthenticated
  loopback allowed"那一行现在是真正被强制执行的，不再只是文档描述。
- DONE V3.2 执行侧沙箱强制（evaluation §六 P1-1，depends on V0.2）：
  两轮研究（第二轮是因为用户就范围做了一次确认）发现真正缺的东西比
  TODO 原文列出的 `http`/`file`/`shell` 三个节点类型窄得多——先跑
  Explore 研究了一遍，再用 AskUserQuestion 向用户确认"是只修真正缺
  的（推荐）还是照字面把 http/file 也强制关掉"，用户选了前者，随后
  继续深挖才发现连"http/file 需要 worker 侧补一个 parity fix"这个初
  步猜测本身都是错的（工人路径和 server 路径对 http 处理其实完全一
  致，两边都恒定用 `HttpNode::default()`，都不读 YAML/payload 参
  数——不存在"server 尊重配置、worker 不尊重"的落差）：
  - **`file`**：V0.2 已经把 `allowed_paths` 做成每个 profile 下都强
    制的 YAML 必填字段（server 工厂 + worker 两条路径），解析期直接
    拒绝空/缺失——不需要再动。
  - **`http`**：`HttpNode::default()` 用的 `SandboxPolicy::default()`
    里，loopback/link-local/private/cloud-metadata 四类危险网络地址
    默认全部拒绝——SSRF 防护本来就恒定开启。唯一"宽松"的是
    `allowed_domains`（空=允许任意外部域名），但这是
    `yanshi-tools/src/sandbox/policy.rs` 里一条写明原因的**既有
    设计决定**（穷举一个 workflow 可能要调用的每个外部域名不现
    实），不是遗漏——按字面把它在生产环境下强制关闭会破坏真实工作
    流、且没有证据支撑这样做有安全收益，所以不动。
  - **`shell`**：这是唯一真实存在的缺口。`ShellTool` 有两种解释模
    式——`Argv`（默认，第一个 token 必须在白名单里）和 `Shell`
    （`sh -c` 自由字符串）。`Shell` 模式早就有"后端不 enforcing 就拒
    绝 spawn"的检查（`ShellTool::prepare_shell`），但 DAG `shell` 节
    点（`yanshi-config/src/executor/shell.rs::ShellWorkflowNode::
    from_params`）永远用 `Argv` 模式，`backend` 停留在构造函数默认
    的 `NoopSandboxBackend`，从未调用过 `.with_os_sandbox()`——一个
    在白名单里的命令，除了"第一个 token 在不在允许列表里"之外，没
    有任何 OS 级别的系统调用/文件系统/网络限制，尽管这套沙箱后端机
    制本身早就在别处（plugin 执行、skill 脚本执行）用起来了，只是
    从没接到这一个调用点。**Fix**：新增
    `resolve_shell_backend(name, profile, backend)`（纯函数，`backend`
    作为参数传入而非内部直接调用 `default_backend()`——镜像既有的
    `executor::plugin::select_preparer`/`preparer_from_env` 拆分方式，
    这样测试能模拟"平台不支持沙箱"而不依赖跑测试的机器实际有没有
    真沙箱），复用**已经定义但从未被消费过**的
    `SecurityProfileDefaults.sandboxing.require_os_sandbox`
    （`yanshi-tool/src/security_profile.rs`，Dev/Local 为
    `false`、Production 为 `true`，跟 V3.1 挖出来的
    `allow_unauthenticated_loopback` 是同一种"字段定义了、从没接线"
    模式）作为门控——Production 下如果解析出的后端 `!is_enforcing()`
    直接在构造期报错拒绝，而不是静默不沙箱运行；Dev/Local 不请求
    包裹，跟今天的行为完全一致，零回归风险。**范围外**（记录下来，
    不是漏做）：不碰 worker 的 `shell` 支持——worker 今天压根没有
    `"shell"` 分支，直接报"不支持"拒绝执行，这已经是安全的"因为不
    支持所以不会跑"状态，给 worker 新增 shell 支持是新增能力（功能
    工作），不是本条要修的安全加固，不在这次范围内；顺带在
    `executor::plugin::select_preparer` 里发现了一个同构的既有缺口
    （`require_sandbox` 为真时永远直接包一层 `default_backend()`，
    从不检查 `is_enforcing()`，平台解析出 Noop 时会静默包一层不生
    效的包装而不是报错）——留给未来某个条目，这次不顺手改。
    **测试**：`yanshi-config/src/executor/shell.rs` 新增 4 个单
    测——`resolve_shell_backend_dev_and_local_skip_wrapping`（Dev/Local
    不请求包裹，即使传入不 enforcing 的 backend）、
    `resolve_shell_backend_production_wraps_an_enforcing_backend`
    （自定义 `FakeEnforcingBackend` 测试替身，`is_enforcing()` 返回
    `true`）、
    `resolve_shell_backend_production_rejects_a_non_enforcing_backend`
    （传入既有的 `NoopSandboxBackend` 触发拒绝，错误信息含节点名和
    "not enforcing"）、`from_params_dev_profile_does_not_require_a_
    command`（既有构造路径回归防护）。人工验证：本地跑一个真实
    `type: shell` 的 workflow YAML，`YANSHI_SECURITY_PROFILE=
    production` 下（macOS，`default_backend()` 解析出真实
    `sandbox-exec` 后端）确认白名单命令依然透明执行成功（不只是测
    拒绝路径，也确认沙箱包裹对正常场景无感）；不带 profile（默认
    Local）跑同一份 YAML 结果完全一致，证实非 Production 场景零回
    归。全工作区门禁（`cargo build/fmt/clippy -D warnings/
    check-arch/println-lint/test --workspace --all-features`，156 个
    测试二进制全过，唯一失败项仍是既往已知的 `code_exec` 容器引擎
    缺失问题，与本次改动无关）全部通过。
- DONE V3.3 SSRF 加固（evaluation §六 P1-4）：`yanshi-tools/src/
  builtin/http.rs` 里两个独立的 SSRF 漏洞，逐行读代码确认（`HttpTool`
  是全工作区唯一的 SSRF 判定实现——`yanshi-nodes::HttpNode` 完全委
  托给它，没有第二份逻辑要同步改）：
  **漏洞 1（IPv4-mapped IPv6 绕过）**：`classify_network_address` 对
  `IpAddr::V4`/`IpAddr::V6` 分别匹配、没有归一化步骤——
  `http://[::ffff:169.254.169.254]/…` 解析成 `IpAddr::V6`，永远不会
  等于 `CLOUD_METADATA_IPS`（一个纯 `V4` 列表——枚举跨 `V4`/`V6`
  variant 永远不相等），也不落进 V6 分支的三个 range 检查（loopback /
  `fe80::/10` / `fc00::/7`，因为该地址第一个 segment 是 `0000`）——分
  类结果是空列表，云元数据请求直接放行。根因：全工作区 grep 确认
  `Ipv6Addr::to_ipv4_mapped()`（RFC 4291 §2.5.5.2 `::ffff:0:0/96` 的
  标准库实现，稳定 API）从未被调用过。**Fix**：在
  `classify_network_address` 最前面加一行归一化，把 IPv4-mapped IPv6
  转成对应的 `IpAddr::V4` 再往下走既有逻辑（既有的 V4 分支和
  `CLOUD_METADATA_IPS` 判断本身是对的，只是从来没等到归一化后的地
  址）。**漏洞 2（DNS-rebinding TOCTOU）**：`validate_url_allowed` 本
  来就会真的解析主机名（`resolve_host_ips` 内部调用真实的
  `tokio::net::lookup_host`，不是只对字面 IP 生效——所以漏洞不是"检查
  压根没对域名生效"这个更大的缺口，而是 TODO 原文字面描述的
  TOCTOU：校验完就把解析结果扔了，`self.client.get(url)...send()`
  接着让 `reqwest` 自己独立再解析一次域名——两次解析互不关联，攻击者
  用一个 TTL 很短的域名可以在两次解析之间返回不同答案，校验时给安全
  地址、连接时给内网/元数据地址。**Fix**：`reqwest` 的 DNS 覆盖
  （`resolve`/`resolve_to_addrs`/`dns_resolver`）只在 `ClientBuilder`
  阶段生效、无法对一个已经 `build()` 好的 `Client` 事后接线——评估过
  两个方案：(a) 每个重定向 hop 现造一个新 `Client`，用
  `resolve_to_addrs(host, &validated_addrs)` pin 到刚校验过的地址集
  合；(b) 装一个共享的自定义 `Resolve`，靠 `validate_url_allowed` 提
  前往共享表里塞地址。选了 (a)——`Resolve::resolve(name)` 拿不到任何
  per-request 上下文，(b) 面对"两个并发请求打同一个 host"这种场景没
  有天然的隔离手段，会互相踩踏；(a) 没有任何共享状态，天然并发安全，
  代价只是放弃跨调用的连接池复用——`HttpTool` 本来就不是高吞吐路径
  （一次一个工具调用、30 秒超时），这个代价可以接受。顺带把
  `resolve_host_ips`/`validate_url_allowed` 的返回类型从
  `Vec<IpAddr>`/`()` 改成 `Vec<SocketAddr>`（原来解析时算出的端口被
  `.ip()` 扔掉了，现在保留下来喂给 `resolve_to_addrs`）；pin 校验到
  的**全部**地址而不是随便挑一个（保留域名本来就解析到多个 IP 的既有
  行为，同时保证 reqwest 绝不会连到任何没被检查过的地址）。
  **`HttpTool` API 联动**：`pub fn with_client(client: Client, ...)`
  换成 `pub fn with_no_proxy(self) -> Self`——全工作区 grep 确认
  `with_client` 唯一的真实调用方就是这个文件自己的测试 helper（所有
  生产/库调用点都走 `HttpTool::new(policy)`），一旦每个真实请求都走
  现造的 pinned client，外部注入一个预先造好的 `Client` 已经没有意义
  了（新的 pinned client 根本不会用它）；改存 `user_agent`/`timeout`/
  `no_proxy` 三个原始字段而不是一个不透明的 `Client`，让"默认"和
  "pinned per-hop"两条路径共用同一份构造配方，测试 helper 原来靠
  `with_client` 注入的 `.no_proxy()` 需求（CLAUDE.md 里记录过的系统
  代理坑）也就自然延续到了每次现造的 pinned client 上。**测试**：
  `yanshi-tools/src/builtin/http.rs` 既有 7 个测试全部原样通过
  （`test_tool`/`test_tool_default_policy` helper 内部换掉构造方式，
  调用方不用改）；新增 5 个——
  `classify_network_address_normalizes_ipv4_mapped_ipv6_cloud_metadata`
  /`..._private`（漏洞 1 直接单测）、
  `classify_network_address_still_handles_native_ipv6_ranges`（防回
  归——确认归一化不会误伤真正的原生 IPv6 loopback/link-local/ULA/公网
  地址分类）、
  `pinned_client_ignores_real_dns_and_connects_to_the_pinned_address`
  （漏洞 2 的核心证明——真实起一个 loopback 测试服务器，把
  `build_pinned_client` pin 到"example.com"这个真实存在但无关的域名
  上、地址指向本地服务器，发一个请求到
  `http://example.com:<本地端口>/`，断言本地服务器真的收到了请求——
  如果 pinning 没生效，这个请求要么真的打到 example.com、要么直接
  hang，不可能命中本地监听；这是在不需要伪造/多答案 DNS resolver 基
  础设施的前提下，直接、确定性地证明"pin 之后 reqwest 绝不会再独立解
  析这个 host"这条核心性质）。全工作区门禁（`cargo build/fmt/clippy
  -D warnings/check-arch/println-lint/test --workspace
  --all-features`，156 个测试二进制全过，唯一失败项仍是既往已知的
  `code_exec` 容器引擎缺失问题，与本次改动无关）全部通过。
- DONE V3.4 限流与配额：逐行读代码确认两个独立缺口——
  **缺口 1（`/v1/runs` 无准入控制）**：`submit_run`
  （`yanshi-server/src/runs.rs`）插入 `runs` 行之后无条件
  `tokio::spawn` 执行任务，没有任何并发上限或速率限制，per-tenant 或
  全局都没有；唯一现成的信号量先例（`LiveHarnessExecutor` 的
  Q3.4.3 并发信号量）是全局的（不区分租户）、而且是"排队等待"
  （`.await` on `acquire_owned()`）而不是"拒绝"，形状不对不能直接复
  用。**缺口 2（`/metrics` 标签泄露租户/run ID）**：`/metrics` 是刻意
  不鉴权的（`metrics_endpoint_bypasses_auth` 测试明确断言这一点，理
  由是"Prometheus scrapers 默认不带 token"）,但
  `observe_workflow_runs_active(tenant, …)` /
  `observe_state_size_bytes(run_id, …)` 各自按标签发一条时间序列——
  任何未鉴权调用方靠抓取 `/metrics` 就能枚举出当前活跃的租户 ID 和
  run ID。**Fix（缺口 1）**：仿照 `RunCancellationRegistry` 的结构
  （`Arc<Mutex<HashMap<...>>>` + poison-recovery 的 `lock_inner`）新
  增 `RunAdmissionRegistry`——每个租户一个 `Semaphore`（`try_admit`
  内部先做非阻塞 `try_acquire_owned`，拿不到直接拒绝、不排队，这是跟
  Harness 那个信号量的关键区别）+ 一个固定窗口（60s）计数器（拿到
  并发槽位之后如果这一窗口内提交数已达上限，把刚拿到的
  permit `drop` 掉还回去，再拒绝——避免"并发槽位白占"）。新增
  `SecurityProfileDefaults.run_admission: RunAdmissionDefaults`
  （`max_concurrent_runs_per_tenant` / `max_run_submissions_per_
  minute_per_tenant`，三档 Dev 64/1000/min、Local 32/300/min、
  Production 10/60/min，跟 `request_limits` 的宽松度梯度一致）+
  `YANSHI_MAX_CONCURRENT_RUNS_PER_TENANT` /
  `YANSHI_RUN_SUBMIT_RATE_LIMIT_PER_MINUTE` 两个环境变量覆盖
  （复用 `server_security_defaults_from_env` 既有的
  `u64_env`-风格模式，新增 `u32_env` 辅助函数，错误信息用新的
  `InvalidCount` variant 而不是复用会误导的 `InvalidByteLimit`
  ——这不是字节数）。`AppState` 新增
  `run_admission_registry: RunAdmissionRegistry` 字段，
  `with_security_profile`/`with_security_defaults` 都会跟着重建它
  （新增 `run_admission_registry_for(&SecurityProfileDefaults)`
  helper 避免两处构造逻辑重复），这样 `serve.rs` 里既有的
  `AppState::new(db).with_security_defaults(security_defaults)`
  调用点完全不用改就能拿到正确的限额。`submit_run` 在算出
  `tenant_id` 之后、创建 `runs` 行之前调用
  `try_admit`——一个不被准入的请求不应该在 DB 里留下一行 `queued`
  记录；拿到的 `RunAdmissionGuard` 被 move 进 `tokio::spawn` 的闭包
  里，随任务结束自动 drop 释放槽位（RAII，不需要像
  `RunCancellationRegistry` 那样手动 `.complete()`）。新增
  `ApiError::TooManyRequests(String)` variant 对齐既有
  `PayloadTooLarge` 的写法（→ HTTP 429，`error_code()` 返回
  `"too_many_requests"`）。新的 `RunAdmissionError`（区别于
  `yanshi-server::scheduler::AdmissionError`——那是 worker gRPC
  控制面的准入策略，完全不相关的另一套东西，两者都在 crate root
  重导出所以刻意换了名字避免撞名）。**Fix（缺口 2）**：
  `observe_workflow_runs_active`/`observe_state_size_bytes` 签名去掉
  `tenant`/`run_id` 参数，改成发一条不带标签的聚合 gauge；
  `lib.rs::refresh_scrape_time_gauges` 里两个调用点从"按行/按 run
  循环调用"改成"先 sum 再调用一次"（`GROUP BY tenant_id` 的 SQL 本
  身不用改，只是拿到结果之后的处理方式变了）。`observe_worker_
  tasks_inflight` 的 `worker_id` 标签明确不动——TODO 原文没点这个名
  字，而且 worker ID 不是租户数据。选"去标签"而不是"加鉴权"是因为
  加鉴权会跟已经存在、且措辞明确的 `metrics_endpoint_bypasses_auth`
  测试（注释写的是"Prometheus scrapers 默认不带 token"）直接冲突。
  Grafana dashboard JSON（`dashboards/grafana/yanshi-overview.
  json`）和文档（`docs/KUBERNETES_DEPLOYMENT.md`、
  `dashboards/README.md`）同步更新到新的、去标签的 wire shape。
  **测试**：`yanshi-tool/src/security_profile.rs` 新增
  `run_admission_defaults_tighten_from_dev_to_production`（三档限额
  单调递减断言）；`yanshi-server/src/runs.rs` 新增
  `run_admission_registry_tests` 模块 3 个测试——
  `try_admit_allows_up_to_the_concurrency_limit_then_rejects`（占满
  并发上限后第三次请求被拒绝、换个租户不受影响）、
  `try_admit_releases_the_slot_when_the_guard_drops`（drop guard
  之后槽位能重新被占用）、
  `try_admit_rejects_over_the_rate_limit_within_the_window`（窗口内
  超限被拒绝，睡过窗口之后计数器重置、恢复放行）；
  `yanshi-server/tests/runs_routes.rs` 新增
  `submit_run_rejects_over_the_per_tenant_rate_limit`（真实打
  `POST /v1/runs`，注入 `max_run_submissions_per_minute_per_
  tenant: 1` 的极小配置，断言第二次提交返回 429 +
  `"code":"too_many_requests"`，换一个租户仍然放行——这个测试需要
  真实 Postgres，`YANSHI_DATABASE_TEST_URL` 未设置时按既有约定
  自跳过，本地未验证、留给 CI）；`yanshi-server/src/metrics.rs`
  两个既有单测改名并重写为聚合语义
  （`observe_workflow_runs_active_emits_an_aggregate_gauge` /
  `observe_state_size_bytes_emits_an_aggregate_gauge`，都新增"不
  包含 `tenant="`/`run_id="`"的反向断言，直接对着这次要堵的泄露点做
  回归防护）；`yanshi-server/tests/metrics_endpoint.rs` 里
  `metrics_endpoint_emits_scrape_time_process_inspectors` 的
  `tenant="default"` 断言换成"不含 `tenant="`"，
  `metrics_endpoint_emits_state_size_gauge_per_active_run_id` 重命
  名为 `metrics_endpoint_emits_aggregate_state_size_gauge` 并断言精
  确的求和结果（`20480 = 4096 + 16384`）+ 不含 `run_id="`。全工作区
  门禁（`cargo build/fmt/clippy -D warnings/check-arch/println-lint`
  + `cargo test --workspace --all-features --no-fail-fast`）全部通
  过，唯一失败项仍是既往已知的 `code_exec` 容器引擎缺失问题
  （`yanshi-skills::builder::tests::build_registers_code_exec_tool`
  + 11 个 `yanshi-tools` `code_exec_macos` 系列测试），与本次改
  动无关。
- DONE V3.5 供应链默认收紧：三个独立子项，逐行读代码确认后用一次
  AskUserQuestion 跟用户确认了范围（三项全做 vs. 拆出 plugin 签名
  单独收尾），用户选了"全做"。
  **子项 1（marketplace 默认改用 Ed25519 + 拒绝 http://）**：
  `yanshi-skills/src/remote_marketplace.rs` 的 `validate_http_url`/
  `validate_registry_url` 原来无条件接受 `http://`——加了
  `is_loopback_http_host`（用 `reqwest::Url::parse` 取 host，判断是
  否 `localhost` 或 `IpAddr::is_loopback()`），非回环的 `http://`
  一律拒绝、回环（`127.0.0.1`/`::1`/`localhost`）继续放行，本地测试
  用的 loopback HTTP mock server（`spawn_registry_server`/
  `spawn_blocking_manifest_server`，都 bind `127.0.0.1:0`）不受影响。
  `RemoteMarketplaceCache::new()`（库自己的默认构造函数）原来默认用
  `ChecksumSha256SignatureVerifier`（自算 checksum 重新哈希，不是真
  签名）——虽然 `yanshi marketplace install/verify` 这条 CLI 路径
  早就在 T0.1 里给远程注册表默认选了 Ed25519（`cache_from_dir`），但
  那是 CLI 命令自己的逻辑、完全绕过 `SecurityProfile`，库级别的默认
  构造函数才是 TODO 原文和 `docs/PROJECT_EVALUATION_2026-08-05.md`
  §7 点名"marketplace 默认 checksum-only"这句话字面指向的东西，也是
  任何不走 CLI 的调用方会默默拿到的行为。改成默认
  `Ed25519SignatureVerifier::new(default_keys_dir()).with_require_signature(false)`
  ——`with_require_signature(false)` 是刻意的，保留"没有 `[signature]`
  块 ⇒ `Unsigned`/`Ok`"这条既有行为，不然会破坏三个测试文件里
  ~30 处纯粹用 `RemoteMarketplaceCache::new()` 预填充本地磁盘缓存
  的 helper（这些 helper 用的是自签的 `checksum-sha256` 算法签名，
  不是在测签名语义本身）——发现并修复了这批测试：新增
  `checksum_cache()` helper（显式 `with_client_and_verifier(...,
  Arc::new(ChecksumSha256SignatureVerifier))`），替换掉
  `yanshi-skills/src/remote_marketplace.rs`（5 处）、
  `yanshi-skills/tests/marketplace_signed.rs`（6 处）、
  `yanshi-cli/tests/marketplace_cli_tests.rs`（17 处）、
  `yanshi-cli/tests/marketplace_unpack_hardening_tests.rs`（2 处）
  里所有依赖旧默认值的调用点；这个批量修复过程本身就是"默认值真的
  变了"的直接证据。
  **子项 2（`plugin install --signed` 改为真实验证）**：
  `plugin.toml` 原来完全没有 `[plugin.signature]` 字段（
  `docs/PLUGIN_DESIGN.md` 里"manifest 已经有这个 slot"的说法是设计
  阶段的预留描述，从未真正实现过）；`--signed` 是纯操作者自证明的
  bool，直接怼进 `PluginEvaluationInput.has_signature`，零独立校验。
  跟用户确认范围时选了"entrypoint-only 签名"（只对 resolved
  entrypoint 文件的字节签名，仿照 marketplace"对下载下来的 artifact
  字节签名"的模型，不做整个目录树的规范化哈希——后者是一个量级更大
  的独立设计问题）。**Fix**：`yanshi-core::plugin::manifest` 新增
  `PluginSignature { algorithm, key_id, value }`（跟
  `yanshi-skills::MarketplaceSignature` 同形状），挂到
  `PluginSection.signature: Option<PluginSignature>`，结构性校验
  （非空字段，不碰 IO/加密）接进 `PluginManifest::validate()`（跟
  `dry_run.validate()?` 一样的接法）。把
  `Ed25519SignatureVerifier::load_public_key` 的函数体抽成独立公开
  函数 `load_ed25519_public_key(keys_dir, key_id)`，签名验证部分抽成
  `verify_ed25519_detached(public_key, artifact, signature_b64,
  context)`，都从 `yanshi-skills::remote_marketplace` 导出——这样
  `yanshi-cli` 的 plugin 签名路径复用跟 marketplace 完全一致的
  key 文件格式/校验逻辑，不用另起一份重复代码（`yanshi-cli` 本来
  就同时依赖 `yanshi-skills` 和 `ed25519-dalek`，零新增跨 crate
  依赖）。`yanshi-cli/src/commands/plugin/install.rs::execute`：
  `has_signature: bool` 参数换成 `keys_dir: Option<String>`；manifest
  校验完之后、构建 `PluginEvaluationInput` 之前，读
  `manifest.plugin.signature`——`None` 则 `has_signature=false`
  （production 档照旧靠既有的 `PluginPolicy::require_signature` 拒绝
  未签名安装，这部分逻辑完全没动）；`Some(sig)` 则真的去读
  entrypoint 字节、加载公钥、跑 Ed25519 验证，**签名存在但验证失败
  是硬失败**（`?` 直接冒泡，绝不会静默降级成"当作未签名继续装"）。
  `main.rs` 的 `PluginCommands::Install` 把 `--signed: bool` 换成
  `--keys-dir <path>`（跟 `yanshi marketplace install/verify` 现有
  的 `--keys-dir` 命名一致，默认
  `~/.yanshi/marketplace-keys/`，两条命令共享同一个 key 目录
  约定）。**测试**：`yanshi-core::plugin::manifest` 新增
  `parses_manifest_with_signature_section`/
  `manifest_validate_rejects_empty_signature_fields`；
  `yanshi-cli/tests/plugin_cli_tests.rs` 新增
  `write_signed_source_plugin` helper（真实 Ed25519 签名 fixture，
  仿 `marketplace_cli_tests.rs` 的
  `ed25519_signed_entry_for_bytes`/`write_ed25519_pub_key` 套路）+ 3
  个新测试——`plugin_install_production_profile_accepts_a_validly_signed_plugin`、
  `plugin_install_rejects_a_tampered_signature`（篡改 signature
  value，断言安装失败且目录未落地）、
  `plugin_install_rejects_an_unknown_key_id`；改了 2 个既有测试——
  `plugin_install_production_profile_refuses_allow_unsandboxed_opt_in`
  去掉了 `--signed` 参数（读代码确认 production 档下沙箱 opt-in 的
  deny reason 是无条件 push 的，不需要签名先过关才能单独测这条）、
  `plugin_install_help_lists_p18_flags` 的断言从 `--signed` 换成
  `--keys-dir`。
  **子项 3（MCP stdio 无界读 + 无界通知 channel）**：逐行读代码确认
  `yanshi-mcp/src/transport/stdio.rs::run_reader_task` 的
  `stdout.read_line(&mut line).await` 读的时候完全没有上限——既有的
  `max_message_size` 检查（`n > max_message_size`）是读完之后才跑
  的，只能决定要不要丢弃已经读满的那一行，读的过程本身不受它约束；
  OS pipe buffer 也帮不上忙（那只限制写端在管道满的时候会不会阻塞，
  不限制读端往 `String` 里累加了多少字节）——恶意/异常 MCP server
  发一段几百 MB、不带换行的数据，`read_line` 会在那个
  `max_message_size` 检查有机会跑之前就把 `line` 的堆缓冲区撑到底。
  通知 channel 是 `mpsc::unbounded_channel()`，没有背压。**Fix**：
  `run_reader_task` 把 `read_line(&mut String)` 换成
  `AsyncReadExt::take(max_message_size)` 包一层再
  `AsyncBufReadExt::read_until(b'\n', &mut Vec<u8>)`——`Take` 保证单
  次读取物理上不可能超过配置的上限；`Take::set_limit` 官方文档写明
  "等价于重新构造一个新 Take"，每次循环开头重置一次就能安全复用同一
  个 `Take` 包装。行为上一个刻意的改动：读到 `Take` 上限还没见到
  `\n`（行太长）或者子进程在行中间退出（没有 `\n` 收尾）——这两种
  情况现在都会**直接终止连接**，而不是像原来那样"丢弃这一行继续读
  下一行"——原因是一旦 `Take` 把某一行从中间截断，这一行剩下的字节
  还原封不动躺在管道里没读，继续往下读会跟真实的消息边界错位、把后
  续字节误解析成新消息；终止连接是跟既有的 `Err(err)` 分支完全一样
  的处理（复用同一份 `terminate_reader` 清理逻辑：`connected` 置
  false、清空 writer、清空 inflight oneshot），也是大多数真实
  JSON-RPC/LSP 风格行式协议对"消息过大"的标准处理方式。同样的思路
  套到了 `yanshi-mcp/src/server.rs`（服务端自己读 stdin 的循环，
  发现原来**连读完之后的兜底检查都没有**，比客户端那边还弱一档，新
  增 `MCPServer::DEFAULT_MAX_MESSAGE_SIZE` 常量套用同一个 `Take` +
  终止逻辑）和 `spawn_stderr_drain`（stderr 排水线程，同样套
  `Take`，但这里不是 JSON-RPC 协议通道，只是给
  `tracing::warn!` 用的人类可读诊断输出，命中上限时不终止、只是把一
  个巨大的"行"拆成多条日志继续读，用 `String::from_utf8_lossy`
  代替严格 UTF-8 校验）。通知 channel 从
  `mpsc::unbounded_channel()` 换成 `mpsc::channel(capacity)`（新增
  `StdioTransport::DEFAULT_NOTIFICATION_CHANNEL_CAPACITY = 1024` +
  `with_notification_channel_capacity` builder，仿
  `with_max_message_size` 的接法），唯一的生产调用点
  `notifications_tx.send(value)` 从同步的 `let _ = ...` 变成
  `.send(value).await`——这个 `await` 本身就是背压机制：channel 满了
  之后 reader task 会阻塞在这一步，连带不再从子进程 stdout 读更多字
  节，子进程自己的 `write()` 系统调用最终也会因为管道满而阻塞，形成
  一路传导回子进程的真实背压，而不是在进程内部堆一个无限增长的队
  列。**测试**：`yanshi-mcp/src/transport/stdio.rs` 新增
  `stdout_flood_with_no_newline_terminates_the_connection`（子进程用
  `yes x | tr -d '\n' | head -c 200000` 灌一段 200KB、不带换行的
  数据，配一个 64KB 的小 `max_message_size`，断言连接会变成
  disconnected 而不是挂起或者内存无界增长）、
  `notifications_channel_applies_backpressure_without_unbounded_growth`
  （子进程一口气吐 50 条通知，channel 容量设成 4，断言即使消费者一
  开始不读，这 50 条最终一条不丢全部送达，且送达完之后 echo 请求仍
  然正常工作，证明 reader task 只是被背压卡住、没有卡死）；既有 37
  个 stdio 测试全部原样通过，证明改动不破坏正常的逐行解析路径。
  全工作区门禁（`cargo build/fmt/clippy -D warnings/check-arch/
  println-lint` + `cargo test --workspace --all-features
  --no-fail-fast`）全部通过，唯一失败项仍是既往已知的 `code_exec`
  容器引擎缺失问题（`yanshi-skills::builder::tests::
  build_registers_code_exec_tool` + 11 个 `yanshi-tools`
  `code_exec_macos` 系列测试），与本次改动无关。
- DONE V3.6 expr 递归深度上限：逐行读
  `yanshi-graph/src/expr.rs` 确认递归-下降解析器（`Parser`
  结构体，`parse_or → parse_and → … → parse_unary → parse_primary`
  优先级链）完全没有深度/复杂度限制，三个独立的无界递归入口——
  `parse_unary` 对 `!`/`-` 前缀的自递归（`!!!!!x`）、`parse_primary`
  对括号分组的 `parse_or` 回环递归（`(((...)))`）、`parse_args` 对函
  数参数的 `parse_or` 递归（`len(len(len(...)))`）——服务端/worker
  执行客户端提交的深嵌套 `run_if`/`while` YAML 表达式可在 `parse()`
  返回之前（早于既有的 `Expr::validate`/`Expr::eval` 递归遍历）栈溢
  出崩溃进程。全工作区 grep 确认 `expr::compile`/`expr::evaluate*`
  只有两个逻辑调用方——`yanshi-config/src/config/schema.rs`
  （`run_if`/`while.condition` 的 YAML-lint 期静态校验）和
  `yanshi-core/src/flow.rs`（运行时求值，while 循环条件 + run_if
  跳过判定）——两者都经过同一个 `Parser::new(...).parse()` 入口，单
  一深度上限可以同时保护两条路径，不需要按调用方区分限额。**Fix**：
  `Parser` 新增 `depth: usize` 字段 + `MAX_EXPR_DEPTH = 64` 常量，
  `enter_depth`/`exit_depth` 一对辅助方法（进入时自增并在超限时立刻
  自减 + 返回新增的
  `ExprError::new(col, "expression is too deeply nested")`；成功返回
  路径上显式自减，失败路径不需要——一旦触发 `?` 提前返回，整个
  `parse()` 调用链连同 `Parser` 实例一起被丢弃，不存在"残留计数影响
  后续兄弟节点解析"的风险）。两个调用点接入：`parse_or`（覆盖括号分
  组和函数参数两条递归路径，因为二者最终都回环到 `parse_or`）、
  `parse_unary`（覆盖 `!`/`-` 自递归路径，这条路径不经过
  `parse_or`，需要单独计数）——共享同一个 `self.depth` 计数器，真实
  反映解析器调用栈在任意时刻的实际嵌套深度（同级、非嵌套的兄弟表达
  式如 `(1)+(2)+(3)+...` 在成功返回时正确归零，不会跨兄弟节点误
  累加）。`ExprError` 是扁平 struct 不是 enum，新增消息直接复用既有
  `ExprError::new(col, message)` 构造惯例，自动经由既有的
  `YanshiError::FlowDefinitionError` 包装路径透出，`error.rs`/
  `YanshiError` 枚举本身零改动。**测试**：`expr.rs` 新增 4 个单
  测——`rejects_deeply_nested_unary_chain`/`rejects_deeply_nested_
  parens`/`rejects_deeply_nested_function_calls`（各自构造 1 万层嵌
  套的字符串，断言 `compile()` 返回含"too deeply nested"的错误而非
  panic/挂起）、`accepts_moderately_nested_expression`（20 层
  `!`/括号嵌套仍能正常编译，防止上限设得过严破坏合法但较复杂的手写
  条件——防回归）；既有 4 个 `expr` 单测原样通过。全工作区门禁
  （`cargo build/fmt/clippy -D warnings --workspace --all-features` +
  `cargo xtask check-arch` + `cargo xtask println-lint` +
  `cargo test --workspace --all-features --no-fail-fast`）全部通过。

### V4 — 文档与工程卫生

- DONE V4.1 重生成 `AGENTS.md`（evaluation §七，单点杠杆最高的修复）：
  逐行核对确认旧 `AGENTS.md`（Last Updated 2026-05-03）严重过时——称
  "14 crates + 2 scaffold" 实际 25 个 workspace 成员、称
  `yanshi-server` 是 "130 LOC scaffold, 0 tests" 实际 19.5K LOC 完
  整网关、称 `yanshi-db` "48 LOC" 实际 2370 行/9 表/9 仓、称"479
  tests（2025-11-17 verified）"实际数千+、完全未提及
  `yanshi-harness`/`yanshi-config`/`yanshi-value`/
  `yanshi-graph`/`yanshi-store-spi`/`yanshi-agent-spi`/
  `yanshi-worker`/`yanshi-ui` 等后建 crate、仍称
  `yanshi-nodes` 含已迁到 `nodes-ai` 的节点与已删除的
  `factory_traits.rs`、把已实现的 RAG eval 说成 pending，且全文档
  末尾还留着大段早已过时的"Future Architecture Vision"代码草图（MCP/
  RAG 尚未开始时期的设计稿）与 2025 年 11 月的历史更新日志。**Fix**：
  用 `CLAUDE.md`（逐字核对与磁盘文件一致，当前唯一权威来源）+
  `docs/CURRENT_STATUS.md` 的现行内容重新生成 `AGENTS.md`——五层契
  约内核架构表 + 每层 crate 职责、"What's Implemented" 现状清单（对
  齐 `CLAUDE.md` 的 Current Implementation Status）、开发规范/
  Pre-Commit 门禁/安全注意事项、文档索引指路（`docs/CURRENT_STATUS.md`
  / `docs/ARCHITECTURE.md` / `TODOs.md` / `RoadMap.md` /
  `docs/STABILITY.md` / `docs/LLM_PROVIDERS_MATRIX.md`）；开头显式标
  注"是 `CLAUDE.md` 的工具无关对应文件，两者保持同步"，避免未来再次
  单向漂移不被发现。全文从 877 行压缩到约 200 行（`yanshi-harness
  ::providers::AgentsMdProvider` 以 `DEFAULT_DOC_CHAR_CAP = 8_000` 字
  符注入 Harness 上下文，重写时刻意把项目概览 + 五层架构表前置在前
  8000 字符内，靠后的开发规范/安全章节即使被截断也不影响最关键信
  息）。顺带发现并修复 `docs/CURRENT_STATUS.md` 自身的过时——"Active
  Work"一节仍称 R 段（2026-07-28）是当前活跃段，而 R/T/U 三段早已
  全部收口存档、真正活跃的是本次 V 段——同步更新该节到 V0-V3 全部收
  口、V4 进行中的现状，并把 `docs/PROJECT_EVALUATION_2026-08-05.md`
  补进 Historical References 列表（此前只列到 05-19）。**根目录遗留
  设计文档加"历史参考"横幅**（复用 `IMPLEMENTATION_STATUS.md`/
  `MODULE_OPTIMIZATION_REPORT.md`/`OVERALL_EVALUATION_REPORT.md` 已
  经确立的横幅格式——`>` 引用块，指向当前权威文档）：`ARCHITECTURE.md`
  （早期 V2 重构设计稿，指向 `docs/ARCHITECTURE.md`/`CLAUDE.md`）、
  `TERA_INTEGRATION_ANALYSIS.md`（集成前差距分析，指向
  `TERA_TEMPLATE_GUIDE.md`）、`TERA_INTEGRATION_COMPLETE.md`（集成完
  成快照）、`LOOP_NODES_IMPLEMENTATION.md`（Map/While 早期实现总
  结）、`MIGRATION_V2.md`（V1→V2 迁移指南，代码库里早已没有 V1 可
  迁）——五个文件均只加横幅未移动位置，保留原文供历史追溯；
  `TERA_TEMPLATE_GUIDE.md`（仍是当前可用的使用指南，非设计快照）按
  TODO 原文范围不动。**验证**：纯文档改动，无需 cargo 全量门禁；
  `grep` 确认工作区内没有任何 `.rs` 测试依赖真实 `AGENTS.md`/
  `docs/CURRENT_STATUS.md` 的具体文本内容（`AgentsMdProvider` 相关
  测试全部读写临时 fixture 文件，不触碰仓库根目录的真实文件），也
  没有 xtask lint 校验这两个文件的内容，改动零回归风险。
- DONE V4.2 `yanshi-cli/src/main.rs` 拆分：2739 行的 clap 胶水代
  码（~20 个互不相关的命令域全部塞进一个文件——每个域的
  `#[derive(Args)]`/`#[derive(Subcommand)]` 结构体/枚举定义 + `async
  fn main()` 里对应的巨型 match 分派逻辑）是全仓复杂度热点。**Fix**：
  按命令域拆分到 `commands/*` 下各自的 `cli.rs`（与该域既有的
  `execute()` 业务逻辑文件同目录）——每个 `cli.rs` 遵循统一约定：
  `pub struct <Domain>Args { #[command(subcommand)] command:
  <Domain>Commands }`（clap 包装结构体，`pub` 因为 `main.rs` 的顶层
  `Commands` 枚举要引用它）+ 私有 `enum <Domain>Commands`（该域全部
  子命令 + 原样保留的每一条 `#[arg(...)]`/`///` doc comment，doc
  comment 是 clap 的 `--help` 文本来源，一个字都不能改）+ `pub async
  fn dispatch(args: <Domain>Args) -> anyhow::Result<()>`（原
  `main()` 里对应 match arm 的分派逻辑原样搬入，包括较复杂的分支——
  例如 `workflow run` 的 server-mode/本地模式分流 +
  `reject_local_only_flags` 校验、`harness` 域的
  `#[allow(clippy::large_enum_variant)]`）。`skill` 域连带把嵌套的
  `IndexArgs`/`SkillIndexCommands`、`MarketplaceArgs`/
  `SkillMarketplaceCommands`（`yanshi skill index`/`yanshi
  skill marketplace`，注意与顶层 `yanshi marketplace` 远程市场命
  令——`RemoteMarketplaceArgs`/`RemoteMarketplaceCommands`，落在
  `commands/marketplace/cli.rs`——是两个完全不同的概念，未混淆）一并
  收进同一个 `skill/cli.rs`。`plugin`/`rag` 两个 feature-gated 域的
  `#[cfg(feature = "...")]` 门控原样保留；两者共用的禁用态占位类型
  `FeatureUnavailableArgs`（給关闭对应 feature 时的 `Plugin`/`Rag`
  变体用，带 `after_help` 提示重新编译）新增独立小文件
  `commands/feature_unavailable.rs`（`#[cfg(any(not(feature =
  "plugin"), not(feature = "rag")))]` 门控在 `commands/mod.rs` 的声
  明处，避免默认全 feature 构建下产生死代码）。`main.rs` 瘦身到
  **128 行**（2739 → 128）：只剩 `Cli`/顶层 `Commands` 枚举（每个变
  体包一层 `commands::<domain>::cli::<Domain>Args`，doc comment 和
  `#[cfg]` 门控原样保留）、`load_yanshi_dotenv()`、以及一个扁平
  的 `match cli.command { Commands::Workflow(args) =>
  commands::workflow::cli::dispatch(args).await, ... }` 分派 + 原样
  保留的尾部错误处理块（含解释 `{:#}` anyhow chain-display 选择的
  注释）。纯代码搬迁，零行为变化。分两轮完成——第一轮 agent 在处理到
  `restore` 域中途被一次瞬时 API 错误打断（已完成 agent/audio/
  backup/cleanup/config/doctor/eval/image/llm/mcp/memory/restore/
  serve/trace 共 14 个域，`ListAgents` 显示该 agent 进程已不可恢
  复），第二轮 agent 读取 `git status` 现场状态后接续完成剩余
  harness/marketplace/plugin/rag/skill/workflow 六个域 + `main.rs`
  瘦身。**验证**（编排者独立复核，非只信任子 agent 报告）：逐个
  `git diff` 确认所有 `RM`（单文件模块转目录模块，如
  `doctor.rs``doctor/mod.rs`）与 `M`（既有 `mod.rs` 加一行 `pub mod
  cli;`）的 diff 除新增声明外零内容变化；`commands/mod.rs` 的
  `feature_unavailable` 门控、`main.rs` 里禁用态 `Plugin`/`Rag` 分支
  的错误信息，逐字比对 `git show HEAD:...`（拆分前）与拆分后完全一
  致；`cargo build -p yanshi-cli --all-features` +
  `--no-default-features`、`cargo fmt --check -p yanshi-cli`、
  `cargo clippy -p yanshi-cli --all-features -D warnings` +
  `--no-default-features -D warnings`、`cargo test -p yanshi-cli
  --all-features`（33 个测试二进制、189+ 用例全过）、`cargo xtask
  check-arch` + `println-lint` 全部本地重新独立跑过一遍（不是照抄子
  agent 报告），外加手工核对 `yanshi --help`/`workflow run
  --help`/`skill marketplace --help` 等多层嵌套子命令的输出与预期一
  致。
- DONE V4.3 供应链卫生：三个独立子项，逐项验证后确认。
  **子项 1（`cargo-audit` 接入 CI）**：本地装 `cargo-audit` 跑一遍确
  认真实缺口——13 项已知 RUSTSEC 漏洞（`bytes`/`crossbeam-epoch`/
  `lopdf`/`quinn-proto`×2/`rsa`/`rustls-webpki`×4/`tar`×2/
  `tracing-subscriber`）+ 20 条 unmaintained/unsound/yanked 警告（后
  者 `cargo audit` 默认不算失败，不需要处理）。**先尝试整体
  `cargo update`**（无 `-p`，语义化版本内的全量刷新）——11/13 漏洞
  能干净解决，但引入了一个真实回归：`rustls` 生态的 crypto-provider
  feature 联合解析被打乱，导致 `yanshi-worker` 的真实 mTLS gRPC
  测试（`grpc_tls_e2e.rs`）panic
  在`CryptoProvider::install_default()`——一个安全相关路径的真实倒
  退，不适合作为"顺手"的供应链卫生改动捎带引入，**整体回退**。改用
  **逐包定点 `cargo update -p <crate>`**，每次改动后单独针对
  `yanshi-worker`（受影响最直接的 crate）的 `grpc_tls_e2e` 重新
  验证：`bytes`（1.10.1→1.12.1）/`crossbeam-epoch`
  （0.9.18→0.9.20）/`tar`（0.4.44→0.4.46）/`tracing-subscriber`
  （0.3.19→0.3.20）批量验证通过；`rustls-webpki`
  （0.103.8→0.103.13）单独验证通过；`quinn-proto`
  （0.11.13→0.11.16，`cargo tree -i` 确认在当前 feature 组合下不被
  任何工作区 crate 实际引用，仍被 Cargo 的统一解析收进 `Cargo.lock`）
  单独验证通过——11 项漏洞全部干净解决，且全程零回归（全工作区
  `cargo build/test --all-features`，`yanshi-rag` 的
  `local-embeddings` feature 因需要联网下载 ONNX Runtime 预编译二进
  制、这次会话里两次实测在当前环境下必现 TLS 失败——`ort-sys` build
  script 报 `native-tls: record overflow`，与本次改动无关，是既有
  的已知网络脆弱点（`quality.yml` 的 `features` job 早就为同一原因
  排除了这个 feature 组合）——按 CI 实际覆盖范围排除该 feature 单独
  验证，其余全部照常跑）。剩余 2 项**确认无法安全解决**——`lopdf`
  （0.34.0，yanshi-rag 的 `pdf` feature 经 `pdf-extract` 引入，深
  层嵌套 PDF 对象栈溢出 DoS，RustSec 给出的修复版本 >=0.42.0 是一个
  breaking bump，需要先确认 `pdf-extract` 自身是否兼容新版
  `lopdf`，属于独立的兼容性核验工作，不是一次 `cargo update` 能完成
  的）与 `rsa`（0.9.9，经 `sqlx-mysql` 的鉴权握手引入——全工作区没
  有任何 crate 启用 `sqlx` 的 `mysql` feature，`sqlx-mysql` 本身对
  当前 feature 组合不可达但仍被 Cargo 统一解析收进
  `Cargo.lock`，Marvin Attack 时序旁路攻击，RustSec 原文写明"No
  fixed upgrade is available!"——没有任何版本号能解决，只能等上游）
  ——写入新增的 `.cargo/audit.toml`（`[advisories] ignore = [...]`，
  每条 RUSTSEC ID 都带内联注释说明原因，含"为什么不是整体
  `cargo update`"的完整背景，方便未来复核）。`.github/workflows/
  quality.yml` 新增 `cargo-audit` job（`cargo install cargo-audit
  --locked` + `cargo audit`，跟其余 job 一样走 plain cargo 命令、不
  引入第三方 Action），接入 `release-gate` 的 `needs` 列表 + 汇总
  echo + 断言三处。本地验证 `cargo audit` 命中 `.cargo/audit.toml`
  后 exit code 0（只剩 20 条不影响退出码的 warning）。
  **子项 2（`yanshi-store-spi` 对 `sqlx` 的硬依赖）**：逐行确认
  `sqlx` 在这个 L0 契约 crate 里唯一的用途就是
  `impl From<sqlx::Error> for MemoryError`（`error.rs`，函数体只有
  一行 `MemoryError::StorageError(e.to_string())`，不保留任何
  sqlx-specific 信息）——直接删除这个 `impl` + `Cargo.toml` 里的
  `sqlx` 依赖声明（连同那条早就自认"future cleanup"的注释）。**验
  证发现比预期更干净**：全工作区没有任何调用点靠 `?` 隐式转换依赖
  这个 `From` 实现——`cargo build -p yanshi-store-spi -p
  yanshi-memory -p yanshi-skills`（以及之后全工作区
  build/test）零报错、不需要在 `yanshi-memory` 里补
  `.map_err(...)`。`yanshi-async-util` → `yanshi-graph` 的对
  称问题（同一个 Cargo.toml 注释旁边点名的"未来清理"）见下方子项 3。
  **子项 3（`yanshi-async-util` → `yanshi-graph` 方向倒置依
  赖，**调查后判定不在本次范围内、留作独立后续**）**：确认
  `yanshi-async-util::error` 就是
  `pub use yanshi_graph::error;`——`RFC_CRATE_ARCHITECTURE.md` 的
  契约内核目标态表明确写"`yanshi-async-util` only depends on
  tokio, futures"，`cargo xtask check-arch` 的三条 law 里
  `LAW_KERNEL_ISOLATION` 只检查"kernel crate 依赖 non-kernel
  crate"，对 kernel-to-kernel 的横向边（`async-util` 和 `graph` 都
  是 L0）完全不设防，所以这条边至今没被任何 CI 门禁捕获，纯粹是设
  计文档记录、代码未落实的技术债。深挖真实修复成本发现比 sqlx 那条
  深得多——`retry.rs` 的 `ErrorPattern::matches`/`RetryPolicy::
  is_retryable` 对 ~30 个具体 `YanshiError` variant（`NodeExecutionFailed`/
  `FlowExecutionFailed`/`CircularFlow`/…）做穷举 match，真正解耦需要
  把 `RetryPolicy`/`RetryContext`/`ErrorPattern` 改成对错误类型泛
  型、新增一个 trait（如 `RetryableError`）让 `yanshi-graph`/
  `yanshi-core` 反向实现——这正是 `yanshi-async-util/
  Cargo.toml` 自己注释里写的"a future generic-error refactor
  (RFC §4)"，一次真正的架构级改动而非顺手依赖清理，牵连
  `yanshi-core` 的 retry_executor、`yanshi-agents` 的
  `RetryPolicy` 使用点、以及任何引用 `ErrorPattern` 字段的 YAML/
  serde 契约——风险和范围都远超一次"供应链卫生"改动应该承担的量，
  且代码自己的注释已经把它标记成"未来"工作，不是遗漏。**决定不在
  本次动手**，留给未来一个独立的、专门测试过的 generic-error 重构
  条目（RoadMap/TODOs 后续段落可引用本条备查）。
  **子项 4（梳理 ~30 处双版本依赖）**：`cargo tree -d --all-features`
  枚举出约 55 组唯二/多版本共存的依赖（比 TODO 原文估计的"~30"略
  多，量级一致）。逐个核对哪些是**我们自己**的 Cargo.toml 造成的
  （可低风险直接改）vs. 第三方 crate 各自锁定不同大版本、不受我们
  控制（改起来是真正的破坏性升级，不在本次范围）：TODO 原文点名的
  `hyper`/`http`/`reqwest`/`thiserror`/`uuid` 全部核实为后者——例如
  `thiserror` 全工作区 21 处引用清一色 `"1.0"`，`reqwest` 全工作区
  9 处清一色 `"0.12"`，`uuid` 虽然各 crate 写法不同（`"1.0"` vs
  `"1.6"`）但都落在同一个 semver 兼容区间、Cargo 本来就会统一解析
  成同一个版本——`cargo tree -d` 里出现的重复版本号完全是第三方依
  赖各自独立锁定的（例如 `hyper-util`/`axum`/`tonic` 链路拉的
  `base64 0.22`、其它更老的第三方库还停在 `hyper 0.14`/
  `reqwest 0.11`），本身没有一处是我们自己造成的重复，不存在"改自
  己的 Cargo.toml 就能修"的低风险修法。**唯一一处真正是我们自己造
  成、且低风险可修的**：`base64`——本仓 5 处引用里 4 处
  `"0.21"`、`yanshi-memory` 一处 `"0.22"`（用途是标准
  `base64::engine::general_purpose::STANDARD`，0.21 起就稳定支持），
  改成 `"0.21"` 消灭这一处自造的不一致（`cargo tree -d` 显示
  `base64 0.22.1` 仍然存在，但确认是 `hyper-util` 独立引入的，不是
  我们造成的，这个残留不在本次可控范围内，如实记录不夸大修复效
  果）。**验证**：`cargo build -p yanshi-memory` + `cargo test -p
  yanshi-memory`（67+1 用例全过）。
  **全工作区门禁**（按 CI 实际覆盖范围核验，`yanshi-rag` 的
  `local-embeddings` feature 因上述已知网络脆弱点单独用
  `--features pdf,html,code-chunking` 验证，其余全部
  `--all-features`）：`cargo build --workspace`、`cargo test
  --workspace --no-fail-fast`（唯一失败项仍是既往已知的 `code_exec`
  容器引擎缺失问题，`yanshi-skills::builder::tests::
  build_registers_code_exec_tool` + `yanshi-tools`
  `code_exec_macos` 系列，与本次改动无关；`yanshi-worker` 的
  `grpc_tls_e2e` 明确验证通过，证明整体 `cargo update` 那次回归确
  实已经消解）、`cargo fmt --check --all`、`cargo clippy --workspace
  --all-targets --all-features -D warnings`、`cargo xtask
  check-arch`、`cargo xtask println-lint` 全部通过。
- DONE V4.4 完整度收尾：四个独立子项，逐项调查后确认范围。
  **子项 1（`input_mapping` 空格 bug，已修）**：逐行核对确认真实存
  在——`yanshi-config/src/config/schema.rs::parse_mapping_source_node`
  （校验期）用 `.trim().trim_start_matches("{{").trim_end_matches("}}").trim()`，
  容忍任意空白；而 `yanshi-config/src/executor/factory.rs`（图构
  建期，唯一真正把 `"{{ ... }}"` 解析成 `(source_node_id,
  source_output_field)` tuple 的地方）用的是
  `v.trim_start_matches("{{ ").trim_end_matches(" }}")`——精确匹配
  "恰好一个空格"的 3 字符字面量，不是 `\s*`。手工核对发现比预期更
  糟：非对称场景（如 `"{{ nodes.x.outputs.y}}"`，前有空格后没有）
  不只是静默丢弃整条映射，还会把 `"}}"` 残留拼进字段名（解析出
  `("x", "y}}")`），比"什么都不做"更隐蔽。且确认这条解析**不经过**
  `yanshi-graph::expr::normalize_expression`（那个函数只服务
  `run_if`/`while.condition`，`input_mapping` 从没接入过 expr
  模块）——`factory.rs` 是唯一实现，独立于 schema.rs 那份宽松版本
  各写各的。**Fix**：把 `factory.rs` 的两行 trim 改成跟
  `schema.rs` 完全一致的四步 trim（`.trim()` → 去字面 `"{{"`/`"}}"`
  → `.trim()`）。**测试**：`yanshi-config/src/executor/factory.rs`
  新增 2 个——`input_mapping_tolerates_non_canonical_brace_whitespace`
  （0/1/多空格三种写法都要解析成
  `("producer","field")`）、`input_mapping_tolerates_asymmetric_brace_whitespace`
  （两种非对称场景，覆盖发现的字段名污染 bug）。
  **子项 2（Gemini `base_url` 拼接错误，已修）**：逐行核对确认
  `yanshi-llm/config/config.yml` 与
  `yanshi-llm/templates/default_models.yml`（后者是通过
  `include_str!` 内嵌进二进制的权威源）里 Google provider 的
  `base_url` 都写成
  `https://generativelanguage.googleapis.com/v1beta/openai`（Google
  的 OpenAI 兼容端点前缀）；但 `yanshi-llm/src/providers/mod.rs`
  的 provider 分派表里 `"google"`/`"gemini"` 并**没有**像
  dashscope/glm/deepseek 那样路由到 `OpenAIProvider`，而是走原生
  `GoogleProvider`，其 `get_model_endpoint` 用
  `format!("{}/v1beta/models/{}:{}", self.base_url, model, method)`
  自行拼接原生路径——两者叠加产生
  `.../v1beta/openai/v1beta/models/...:generateContent`，重复的
  `/v1beta/` 段加一截不该出现的 `/openai`。之所以全套测试从未捕获
  到：`yanshi-llm/tests/provider_consistency.rs` 的 `google_*`
  测试全部用 `spawn_mock_server` 起的本地假 URL 构造
  `GoogleProvider`（从不是配置里的真实值），`google.rs` 自己的单测
  也全部传 `base_url: None`（走硬编码的正确默认值
  `https://generativelanguage.googleapis.com`，与配置文件的值完全
  脱节）——没有任何一个测试真正拿配置文件里的字符串构造过
  provider。**Fix**：两个 YAML 文件的 `base_url` 都改成
  `https://generativelanguage.googleapis.com`（与 `GoogleProvider`
  自己的硬编码默认值完全一致，`GoogleProvider` 本身的拼接逻辑没有
  问题，问题纯粹在配置数据）。**测试**：
  `yanshi-llm/src/providers/google.rs` 新增
  `model_endpoint_from_the_real_shipped_config_is_well_formed`——用
  `crate::config::model_config::LLMConfig::from_yaml` 解析生产代码
  实际读取的同一份 `include_str!("../../templates/default_models.yml")`，
  取出 google provider 的真实 `base_url` 构造一个真实
  `GoogleProvider`，断言拼出的 URL 精确等于期望值、不含
  `/v1beta/openai`、`/v1beta/` 只出现一次——第一个真正覆盖"配置文件
  里的值"这条路径的测试，堵住这个盲区未来复发。
  **子项 3（`~/.yanshi/runs` 保留策略，已扩展）**：调查发现机制
  其实已经存在——`yanshi-server/src/cleanup.rs` 的
  `CleanupConfig.run_dir_retention_days`（默认 14 天）+
  `sweep_run_dir`（按 mtime + `runs` 表 `is_active` 双重判定）+
  `yanshi-cli`/`yanshi serve --cleanup` 的完整接线全都已经落
  地——真正的缺口是 `yanshi-server/src/main.rs::run_cleanup_once`
  在做任何清理之前**硬性要求** `DATABASE_URL`（`.ok_or("DATABASE_URL
  is required for cleanup")?`），导致一个从未跑过 `yanshi serve`
  + Postgres、只本地用 `yanshi workflow run` 的用户（`~/.yanshi/runs`
  正是这条路径的默认落地目录，TODO 原文描述的"长期运行的机器无限
  堆积"场景）**完全没有任何办法**清理这个目录，除非专门为了跑一次
  清理去搭一个数据库。且即使配了 DB，`run_cleanup_once` 也从不给
  `run_dir`/`trace_dir` 填默认值——不显式传 `--run-dir`/
  `YANSHI_RUN_DIR` 时清理对象是空的，同样清不到 CLI 自己默认写
  入的 `~/.yanshi/runs`。**Fix**：
  `yanshi-server/src/cleanup.rs` 新增
  `sweep_run_dir_local`（DB-free，仿照已有的 `sweep_trace_dir`——纯
  mtime 判定，不查 `runs` 表；这是刻意的权衡而非疏漏：本地跑的
  workflow 在执行期间会持续往 run 目录写 checkpoint/输出，mtime 会
  保持新鲜，真正"沉寂超过保留窗口"的目录基本等价于"已经结束或被遗
  弃"）+ `cleanup_expired_local`（只跑文件系统扫，`runs`/`events`/
  `artifacts` 三个 DB 相关计数器保持 `Default::default()` 的
  0，不假装做了没做的事）。`yanshi-server/src/lib.rs` 导出新增
  的 `cleanup_expired_local`。`yanshi-server/src/main.rs::
  run_cleanup_once` 改写：`run_dir`/`trace_dir` 现在会 fallback 到
  `~/.yanshi/{runs,traces}`（跟 `yanshi workflow run`/`yanshi
  trace *` 的默认值一致）；`database_url` 存在时走原有的
  `cleanup_expired`（DB 路径行为完全不变），不存在时改为调用新的
  `cleanup_expired_local` 而不是直接报错退出。**测试**：
  `yanshi-server/src/cleanup.rs` 新增 6 个——
  `sweep_run_dir_local_deletes_old_uuid_dirs_by_mtime_only`（老
  UUID 目录删、新目录留、非 UUID 目录留）、
  `sweep_run_dir_local_dry_run_deletes_nothing`、
  `sweep_run_dir_local_returns_zero_when_root_missing`、
  `cleanup_expired_local_sweeps_both_dirs_and_leaves_db_counters_zero`
  （端到端，run_dir + trace_dir 都真删，同时断言三个 DB 计数器精确
  为 0）、`cleanup_expired_local_handles_no_dirs_configured`；全部
  不需要真实 Postgres，纯文件系统测试。全工作区门禁全部通过（14 个
  cleanup 单测 + 全工作区 build/test/fmt/clippy/check-arch/
  println-lint）。
  **子项 4（Semantic 分块器暴露到 config-first，调查后判定不在本次
  范围内、留作独立后续）**：深挖发现比 TODO 原文暗示的"接一个
  factory 分支"深得多——`yanshi-rag/src/chunking/semantic.rs` 的
  `SemanticChunker` 对同步 `ChunkingStrategy` trait 的实现
  （`fn chunk(&self, _: &str)`）**硬编码返回错误**（"Semantic
  chunking requires async context. Use chunk_async() instead."）——
  真正的分块逻辑是 `async fn chunk_async`，内部调用
  `self.embedding_provider.embed_batch(...)`（真实 HTTP 调用）。而
  config-first 路径唯二的两个分发点——`yanshi-rag/src/chunking/
  mod.rs::create_chunker`/`chunk_document_for_knowledge_backend`
  和 `yanshi-skills/src/builder.rs::parse_chunk_strategy`/
  `register_knowledge_backends`——全部是**同步**函数，签名里完全没
  有 embedding provider 参数，`create_chunker`
  的 Semantic 分支本来就已经存在、只是原样透传那个硬编码错误
  （"Semantic chunking not yet implemented"），`parse_chunk_strategy`
  甚至连 `"semantic"` 这个字符串都不认识。真正接上需要：把这一串同
  步函数改造成能接收/构造 `Arc<dyn EmbeddingProvider>` 并转 async，
  再让 `Box<dyn ChunkingStrategy>` 这套同步 trait-object 抽象容纳一
  个本质异步的分块器——这是一次真正的架构改动，而不是配置接线，且
  `yanshi-rag` 自己的类型设计已经明确承认了这一点（`chunk()` 的
  错误信息直接指向 `chunk_async()`）。风险和范围都超出一次"完整度
  收尾"改动应该承担的量，参照 V4.3 处理
  `yanshi-async-util` → `yanshi-graph` 的先例——**决定不在本
  次动手**，留给未来一个独立的、专门设计过 async 分块路径的条目。
  **Update（2026-08-09，V4.4-FU1，DONE）**：按用户要求捡回并独立设计
  实现——三个并行 Explore agent 分别核对了 `SemanticChunker`/
  `ChunkingStrategy` trait 的同步/异步边界、`yanshi-skills`
  `register_knowledge_backends` 调用链是否已经全程 async（结论：是,
  纯签名改动零新增运行时基础设施）、以及 `KnowledgeConfig` 是否已有
  可抄的 embedding provider 配置先例（结论：`MemoryConfig.
  embedding_model` + `resolved_embedding_model()` 模式，`semantic`
  memory 用的同一套），一个 Plan agent 据此出具体实现方案，编排者核
  对代码后写最终 plan、经用户 `ExitPlanMode` 确认后实现。**不**改造
  共享的同步 `ChunkingStrategy` trait（原方案考虑过，评估后否决——
  机械上对每个 chunker 都简单，但会波及 RAG eval 评测 harness 等几十
  个不相关调用点），改为新增一个平行的、只服务 semantic 策略的异步
  入口 `chunk_document_for_knowledge_backend_semantic`（直接构造
  `SemanticChunker` 走 `chunk_async`，跳过对 semantic permanently-erroring
  的 `Box<dyn ChunkingStrategy>` 抽象），复用从
  `chunk_document_for_knowledge_backend` 抽出的公共打包逻辑
  `package_chunks_for_knowledge_backend`。`KnowledgeConfig` 新增
  `embedding_model`/`chunk_similarity_threshold`/
  `chunk_min_segment_size`/`chunk_buffer_percentile` 四个字段 +
  `resolved_*` accessor，镜像 `MemoryConfig` 既有模式。
  `register_knowledge_backends`/`build_registry` 改 async（纯签名改
  动，验证：workspace 里唯一调用点、且从 `SkillBuilder::build*` 往上
  全部本来就是 async），`parse_chunk_strategy` 认出 `"semantic"`。
  **测试**：`yanshi-rag` 新增 `TwoClusterEmbedder` 本地 mock
  `EmbeddingProvider`（离线、确定性，两个语义簇制造真实的余弦相似度
  断崖）+ 2 个新测试；`yanshi-skills` 新增 2 个测试（`"semantic"`
  被 `parse_chunk_strategy` 接受；缺 `OPENAI_API_KEY` 时走
  `SkillError::ValidationError` 而不是挂起或 panic，用既有的
  env-var 快照/恢复约定，纯离线）。**非目标**（明确写进最终 plan 和
  commit message）：`yanshi rag eval --chunk-strategy` 不接
  `semantic`（RAG 评测 benchmark 是独立功能，需要自己的 embedding-model
  flag）；`SKILL.md` frontmatter 仍然没有 `[[knowledge]]` 支持
  （`SkillMd::into_manifest()` 硬编码 `knowledge: vec![]`）——本次改
  动只覆盖 `skill.toml` 路径，是既有的、与本次改动无关的缺口。**验
  证**：`cargo fmt/clippy -D warnings`（两个 crate）、
  `cargo test`（chunking:: 46/46、skills builder:: 全绿含
  `build_registers_code_exec_tool`）、`cargo xtask check-arch/
  println-lint`、`cargo doc`（零本次改动引入的 warning）全部通过；
  收尾前完整跑了两遍全工作区 `cargo test --workspace --all-features
  --exclude yanshi-rag --no-fail-fast`（154/154 测试二进制零失败,
  含之前修复的 `code_exec`/`metrics_endpoint` 两个本地环境问题，见下
  方旁注）+ 远程 Ubuntu 22.04（内核 5.15，`yanshi-tools` 的
  Linux-only 沙箱测试 `sandbox_linux.rs`/`code_exec_linux.rs`，
  `#[cfg(target_os = "linux")]` 在 macOS 上完全编译不进去）用专门配
  置的非 root 用户（`loginctl enable-linger` 修出真 systemd user
  session，否则 podman 静默退化成 `cgroupfs` manager 而不是真正的
  cgroup v2 委托）全部 14/14 通过——三条此前用 root 直接测会失败的用
  例（workspace 目录 UID 不匹配一条、cgroup 内存/pids 强制两条）确认
  纯粹是"用 root 测"这个环境问题，不是代码 bug。commit `da0a2de`。
  旁注（不是这次改动的一部分，是同一轮会话里顺手修的本地环境）：本
  机 `code_exec_macos`/`build_registers_code_exec_tool` 之前失败是
  因为 Apple `container` 系统服务没启动（`container system start`
  一次性修复）；`metrics_endpoint_emits_aggregate_state_size_gauge`
  之前的间歇性超时不是并发资源争用，是 `/metrics` scrape handler
  （`refresh_scrape_time_gauges`）真的会查 Postgres、本地
  `localhost:5432` 上此前没有任何监听导致连接反复超时——起一个最小
  可连通的本地 Postgres 容器后 90s → 0.28s。

## Exit Criteria（生产就绪出场标准）

- V0 + V1 + V3.1/V3.2/V3.3/V3.4/V3.5 全部 DONE，且每项都配有 regression test。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -D
  warnings`、lib 层 `unwrap/expect` DENY、`cargo test --workspace`
  全绿（`code_exec` 相关的环境限制项在支持容器沙箱的 CI runner 上
  另行验证）。
- 新增"破坏性/fail-closed 代码路径需要独立安全审查"这一检查项，纳入
  Pre-Commit Requirements（呼应 U 段教训：把已有机制接成默认路径这件
  事进步很快，但新写的接线代码本身的安全审查容易掉队）。
- 个人/单租户可信场景下：无已知 UTF-8/边界 panic 路径、DAG 终态/resume
  语义正确、执行侧默认危险配置（file 任意路径、SSRF 绕过）已消除。

## Suggested Execution Grouping（建议的并行分工，供后续启动执行时参考）

V0–V4 之间必须严格串行（每段收口后编排者做集成 + 全量门禁验证再进
下一段）；**段内**按下列 crate 分组可并行，`yanshi-core`
（flow.rs/checkpoint.rs 集中了 V0.3/V0.4/V1.1/V1.2 多项，改动高度
耦合）必须独占单线程处理，其余分组互不重叠：

| 分组 | 负责 crate | 覆盖条目 |
| --- | --- | --- |
| core-kernel | `yanshi-core` + `yanshi-graph`(expr) | V0.3 / V0.4 / V1.1 / V1.2 / V1.4 / V3.6 / V4.4(部分) |
| agents-runtime | `yanshi-agents` + `yanshi-agent-spi` | V0.1 / V1.3 / V2.1 / V2.3 / V2.4 |
| tools-sec | `yanshi-tools` / `yanshi-nodes` / `yanshi-mcp` | V0.2 / V3.3 / V3.5(MCP 部分) |
| llm-mem | `yanshi-llm` / `yanshi-memory` | V1.5 / V1.6 / V4.4(Gemini URL) |
| service | `yanshi-server` / `yanshi-skills` / `charts` | V0.5 / V3.1 / V3.2 / V3.4 / V3.5(marketplace 部分) / V2.2 |
| docs-hygiene | `docs` / 根文档 / `main.rs` / CI | V1.7(跨 crate，需与各分组协调) / V4.1 / V4.2 / V4.3 |

依赖顺序提示：V1.2（resume）依赖 V0.3/V0.4（skip 标记 + 路径净化）
先落地；V3.2（执行沙箱强制）依赖 V0.2（file 节点默认策略）先落地。

---

## Cross-References

- `docs/PROJECT_EVALUATION_2026-08-05.md` — **本次 V 段的评估依据**：
  6 路并行子代理独立通读四层源码 + 编排者本地实证核验（build/test/
  clippy/fmt/tree + 5 项关键缺陷逐行复核）。
- `docs/archive/PROJECT_EVALUATION_2026-07-30.md` — U 段的评估依据
  （对 T 段修复效果的复核）。
- `docs/archive/TODOs-archive-2026-08-05-pre-production-readiness-snapshot.md` —
  8/5 全量快照：U 段（17 项）收口前的完整历史明细。
- `docs/archive/TODOs-archive-2026-07-30-post-t-pre-u-snapshot.md` — 7/30
  全量快照：T 段（15 项）收口前的完整历史明细。
- `docs/archive/TODOs-archive-2026-07-29-post-r-pre-t-snapshot.md` — 7/29
  全量快照：R 段（12+8 项）收口前的完整历史明细。
- `docs/archive/TODOs-archive-2026-07-28-pre-audit-remediation-snapshot.md` —
  7/28 全量快照：H/P-A/S/L 四段收口前的完整历史明细。
- `docs/CURRENT_STATUS.md` — 当前已实现状态。
- `RoadMap.md` / `docs/ROADMAP_v2.md` — 中长期方向。
- `docs/STABILITY.md` / `docs/API_COMPATIBILITY.md` — 稳定面契约。
- `docs/RFC_CRATE_ARCHITECTURE.md` / `docs/RFC_TOOL_CONTRACT_SPLIT.md` /
  `docs/ARCHITECTURE_EVALUATION_2026-06-20.md` — 依赖铁律定义 + T3.3
  拆分 RFC + 架构评估（V4.3 涉及）。
- `docs/DISTRIBUTED.md` — 分布式部署形态说明（V0.5/V3.2 涉及）。
- `docs/MEMORY_LAYERING.md` — 四层记忆设计（V1.6 涉及）。
- `docs/SERVER_BACKUP_RESTORE.md` — 备份/恢复流程（U0 遗留，V 段未涉及）。
- `docs/DEPLOYMENT.md` — 部署指南（V3.1/V3.2 涉及）。
- `HARNESS_MODE_EVOLUTION.md` — Harness Mode 设计规范（V2.2/V2.3 涉及）。
- `docs/archive/TODOs-archive-2026-06-20-q1-q5-audit-remediation.md` — 更早
  的深度审计修复波次（108 DONE）。
- `docs/archive/TODOs-archive-2026-05-24-p10-optimization-backlog.md` —
  P10 优化 backlog 全部 DONE 项 + 少量 polish 未拾起。
- `docs/archive/TODOs-archive-2026-05-20-closed-segments.md` — 12 个全 closed
  P-段（P0–P9 + P-H + P-LLM + M）。
- `docs/archive/TODOs-archive-2026-05-19-recently-closed.md` —
  5/19 扫出的中段历史。
- `docs/archive/TODOs-archive-2026-05-09-n1-n10.md` + `...05-10-p0-p4.md` —
  N 系列 + 早期 P 系列执行计划历史。
