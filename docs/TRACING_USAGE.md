# Yanshi 工作流追踪系统使用指南

**版本**: v1.0
**日期**: 2025-11-23
**状态**: ✅ Production Ready

---

## 🎯 概述

Yanshi 追踪系统提供详细的工作流执行追踪，包括：
- ✅ 每个节点的输入/输出
- ✅ LLM 提示词和响应
- ✅ Token 使用和成本
- ✅ 执行时间和性能指标
- ✅ 完整的调试日志

### 核心特性

1. **不污染核心** - 通过 EventListener 集成，零侵入
2. **零开销** - 不启用时无性能影响
3. **灵活存储** - 文件存储（开发）、数据库（生产）
4. **异步处理** - 不阻塞工作流执行
5. **可查询** - 支持过滤、分页、时间范围

### 关联上下文

持久化 trace 在每个层级都包含显式 `context` 字段，用来把一次
mixed run 串成同一棵树:

- workflow: `context.run_id` / `context.trace_id` 等于 workflow run id，`span_id = "workflow"`
- node: `span_id = "node:<node_id>"`，父 span 是 `workflow`
- agent: `span_id = "agent:<session_id>"`，父 span 是所在 workflow node
- tool/MCP call: `span_id = "tool:<index>:<tool>"`，父 span 是 agent

这些字段会直接写入 JSON trace，并与 OpenTelemetry exporter 的 span
层级保持一致。

Tool call trace 同时记录 `source`、`permissions`、`duration_ms` 和
`is_error`。这些字段来自 `ToolRegistry` 中注册工具的 metadata，以及
runtime 对实际调用结果的计时和错误状态。

---

## 📦 快速开始

### 1. 添加依赖

```toml
[dependencies]
yanshi-core = "0.3.0"
yanshi-tracing = "0.1.0"
```

### 2. 基本使用

```rust
use yanshi_core::events::{EventListener, WorkflowEvent};
use yanshi_tracing::{
    TraceCollector,
    TraceConfig,
    storage::file::FileTraceStorage,
    TraceStorage,
    format_trace_human_readable,
};
use std::sync::Arc;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 创建存储
    let storage = Arc::new(FileTraceStorage::new(
        PathBuf::from("./traces")
    )?);

    // 2. 创建追踪收集器
    let collector = Arc::new(TraceCollector::new(
        storage.clone(),
        TraceConfig::development()  // 或 TraceConfig::production()
    ));

    // 3. 使用收集器（实现 EventListener trait）
    // collector.on_event(&event);

    // 4. 查询追踪
    let trace = storage.get_trace("workflow-id").await?;
    if let Some(t) = trace {
        println!("{}", format_trace_human_readable(&t));
    }

    Ok(())
}
```

---

## ⚙️ 配置选项

### TraceConfig

```rust
use yanshi_tracing::{TraceConfig, StorageErrorPolicy};

// 开发环境配置（完整追踪）
let dev_config = TraceConfig::development();

// 生产环境配置（脱敏）
let prod_config = TraceConfig::production();

// 自定义配置
let custom_config = TraceConfig {
    capture_io: true,              // 是否捕获输入/输出
    capture_prompts: true,          // 是否捕获 LLM 提示词
    max_io_size_bytes: 1024 * 1024, // 最大数据大小（1MB）
    async_storage: true,            // 异步存储（推荐）
    on_storage_error: StorageErrorPolicy::LogError,
};
```

### 配置对比

| 配置项 | 开发环境 | 生产环境 | 说明 |
|--------|----------|----------|------|
| `capture_io` | ✅ true | ❌ false | 生产环境可能包含敏感数据 |
| `capture_prompts` | ✅ true | ❌ false | LLM 提示词可能包含敏感信息 |
| `max_io_size_bytes` | 10MB | 0 | 限制日志大小 |
| `async_storage` | ✅ true | ✅ true | 异步存储不阻塞工作流 |

---

## 💾 存储后端

### 1. 文件存储（开发环境）

```rust
use yanshi_tracing::storage::file::FileTraceStorage;
use std::path::PathBuf;

let storage = FileTraceStorage::new(
    PathBuf::from("./traces")
)?;

// 每个工作流一个 JSON 文件
// ./traces/workflow-001.json
// ./traces/workflow-002.json
```

**优点**:
- 简单易用
- 无需数据库
- 适合本地开发

**缺点**:
- 查询性能差
- 不适合大规模生产

### 2. PostgreSQL / SQLite 存储（尚未实现）

W4.4：`storage::schema` 模块导出了两套 DDL 常量
（`POSTGRES_TRACE_SCHEMA`/`SQLITE_TRACE_SCHEMA`，5 表规范化 schema），但
**没有任何代码执行这些 DDL 或实现 `TraceStorage` trait**——`yanshi-tracing`
crate 里唯一的具体存储后端是上面的 `FileTraceStorage`。此前存在的
`postgres` Cargo feature 只挂了一个零引用的 `sqlx` 可选依赖（一个死
flag），已在 W4.4 移除；`cargo check --features postgres` 编译通过从来
不能证明有真实的 Postgres 后端。

如果未来需要 SQL 后端，需要把 `ExecutionTrace` 拆分写入
`trace_runs`/`trace_steps`/`trace_events`/`trace_tool_calls`/
`trace_mcp_calls` 五张表（不是简单塞进一个 JSONB 列），这是一项独立的、
需要单独 RFC 的工作量，不是"填一个存根"级别的任务。

---

## 📡 OTLP 导出（W4.4）

`otlp-http` feature（默认不启用）提供开箱即用的 OTLP/HTTP+JSON span
导出器 `OtlpHttpSpanSink`，基于官方 `opentelemetry-otlp` crate 构建，
wire 编码由该 crate 保证符合规范，不需要手写 OTLP 的 JSON schema。

```toml
[dependencies]
yanshi-tracing = { version = "0.1", features = ["otlp-http"] }
```

```rust
use std::sync::Arc;
use yanshi_tracing::{
    OtelExporterConfig, OtelTraceExporter, TraceCollector, TraceConfig,
};
use yanshi_tracing::otlp::{OtlpHttpConfig, OtlpHttpSpanSink};
use yanshi_tracing::storage::file::FileTraceStorage;
use std::path::PathBuf;

let storage = Arc::new(FileTraceStorage::new(PathBuf::from("./traces"))?);

// OTEL_EXPORTER_OTLP_ENDPOINT / OTEL_EXPORTER_OTLP_HEADERS 环境变量
// （标准 OTel SDK 约定），或手动构造：
let otlp_config = OtlpHttpConfig::from_env()
    .unwrap_or_else(|| OtlpHttpConfig::new("http://localhost:4318"));
let sink = OtlpHttpSpanSink::new(otlp_config)?;
let exporter = OtelTraceExporter::new(
    OtelExporterConfig::new("my-service"),
    sink,
);

let collector = TraceCollector::new(storage, TraceConfig::production())
    .with_exporter(Arc::new(exporter));
```

只支持 HTTP 传输（"HTTP 先行"，见 TODOs.md W4.4）；gRPC 传输仍未实现，
需要的话仍需自行实现 `OtelSpanSink`。批处理粒度是"每个完成的
`ExecutionTrace` 一次导出"（跟随 `TraceCollector` 自身的批处理边界），
不经过 OTel SDK 自己的 batch processor / sampler / 全局 tracer 注册。

---

## 🔍 查询追踪

### 获取单个追踪

```rust
use yanshi_tracing::TraceStorage;

// 通过 workflow_id 查询
let trace = storage.get_trace("workflow-001").await?;

if let Some(trace) = trace {
    println!("Status: {:?}", trace.status);
    println!("Duration: {}ms", trace.duration_ms().unwrap_or(0));
    println!("Nodes: {}", trace.nodes.len());
}
```

### 查询多个追踪

```rust
use yanshi_tracing::{TraceQuery, TraceStatus};
use chrono::{Utc, Duration};

// 构建查询
let query = TraceQuery {
    status: Some(TraceStatus::Completed),
    user_id: Some("user-123".to_string()),
    time_range: Some(TimeRange {
        start: Utc::now() - Duration::days(7),
        end: Utc::now(),
    }),
    limit: Some(50),
    offset: Some(0),
    ..Default::default()
};

// 执行查询
let traces = storage.query_traces(query).await?;

for trace in traces {
    println!("Workflow: {}", trace.workflow_id);
}
```

---

## 📊 格式化输出

### 人类可读格式

```rust
use yanshi_tracing::format_trace_human_readable;

let trace = storage.get_trace("workflow-001").await?.unwrap();
let output = format_trace_human_readable(&trace);
println!("{}", output);
```

输出示例：
```
═══════════════════════════════════════════════════════════
Workflow: AI Research Assistant
ID: workflow-001
═══════════════════════════════════════════════════════════

Status: Completed
Started: 2025-11-23 10:30:00 UTC
Completed: 2025-11-23 10:30:45 UTC
Duration: 45000ms
Environment: production
User: user-123
Tags: research, arxiv

───────────────────────────────────────────────────────────
Nodes Executed: 3
───────────────────────────────────────────────────────────

[1] search_arxiv (HttpNode)
    Status: Completed
    Duration: 5000ms
    Input: {"query": "large language models"}
    Output: {"papers": [...]}

[2] summarize (LLMNode)
    Status: Completed
    Duration: 30000ms
    Model: gpt-4o (openai)
    System Prompt: You are a research assistant...
    User Prompt: Summarize the following papers...
    Response: Here is a summary of the papers...
    Tokens: 1500 (prompt) + 500 (completion) = 2000
    Cost: $0.06
    Latency: 30000ms

[3] generate_report (TemplateNode)
    Status: Completed
    Duration: 100ms

═══════════════════════════════════════════════════════════
```

### JSON 格式

```rust
use yanshi_tracing::export_trace_json;

let json = export_trace_json(&trace)?;
println!("{}", json);

// 或保存到文件
std::fs::write("trace-001.json", json)?;
```

### 简洁摘要

```rust
use yanshi_tracing::format_trace_summary;

let summary = format_trace_summary(&trace);
println!("{}", summary);
// 输出: ✅ workflow-001 | completed | 3 nodes | 45000ms
```

### 终端调试视图

`yanshi trace tui` 提供最小的静态 TUI timeline，用于在不重新执行 workflow、tool、MCP server 或 LLM 的情况下聚焦查看已持久化 trace。

```bash
yanshi trace tui workflow-001 --dir ./traces
yanshi trace tui workflow-001 --dir ./traces --filter mcp --details
yanshi trace tui workflow-001 --dir ./traces --filter agent --max-field-chars 240
```

If `--dir` is omitted, trace commands read `YANSHI_TRACE_DIR` first and then
fall back to `~/.yanshi/traces`:

```bash
YANSHI_TRACE_DIR=./traces yanshi trace replay workflow-001
YANSHI_TRACE_DIR=./traces yanshi trace tui workflow-001 --filter agent
```

可用 filter:

- `all`: workflow、node、agent、tool/MCP 调用。
- `workflow`: workflow 和 node 层级概览。
- `agent`: agent session、step 数量、tool 数量。
- `tool`: 所有 tool 调用。
- `mcp`: 只显示 MCP tool 调用。

---

## 📝 事件类型

### 工作流事件

```rust
use yanshi_core::events::WorkflowEvent;

// 工作流开始
WorkflowEvent::WorkflowStarted {
    workflow_id: String,
    timestamp: Instant,
}

// 工作流完成
WorkflowEvent::WorkflowCompleted {
    workflow_id: String,
    duration: Duration,
    timestamp: Instant,
}

// 工作流失败
WorkflowEvent::WorkflowFailed {
    workflow_id: String,
    error: String,
    duration: Duration,
    timestamp: Instant,
}
```

### 节点事件

```rust
// 节点开始
WorkflowEvent::NodeStarted {
    workflow_id: String,
    node_id: String,
    timestamp: Instant,
}

// 节点完成
WorkflowEvent::NodeCompleted {
    workflow_id: String,
    node_id: String,
    duration: Duration,
    timestamp: Instant,
}

// 节点失败
WorkflowEvent::NodeFailed {
    workflow_id: String,
    node_id: String,
    error: String,
    duration: Duration,
    timestamp: Instant,
}
```

### LLM 专用事件

```rust
// LLM 提示词发送
WorkflowEvent::LLMPromptSent {
    workflow_id: String,
    node_id: String,
    model: String,              // "gpt-4o"
    provider: String,            // "openai"
    system_prompt: Option<String>,
    user_prompt: String,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    timestamp: Instant,
}

// LLM 响应接收
WorkflowEvent::LLMResponseReceived {
    workflow_id: String,
    node_id: String,
    model: String,
    response: String,
    usage: Option<TokenUsage>,  // Token 统计
    duration: Duration,
    timestamp: Instant,
}
```

---

## 🔧 高级用法

### 自定义 EventListener

```rust
use yanshi_core::events::{EventListener, WorkflowEvent};

struct MyCustomListener {
    // 你的字段
}

impl EventListener for MyCustomListener {
    fn on_event(&self, event: &WorkflowEvent) {
        match event {
            WorkflowEvent::LLMPromptSent { model, user_prompt, .. } => {
                // 发送到 Sentry
                // sentry::capture_message(&format!("LLM call to {}", model));
            }
            WorkflowEvent::WorkflowFailed { error, .. } => {
                // 发送告警
                // alert::send_notification(error);
            }
            _ => {}
        }
    }
}
```

### 组合多个监听器

```rust
use yanshi_core::events::MultiListener;

let multi_listener = MultiListener::new(vec![
    Box::new(TraceCollector::new(storage, config)),
    Box::new(MyMetricsListener),
    Box::new(MyAlertsListener),
]);

// 所有监听器都会收到事件
```

---

## 📈 性能考虑

### 1. 异步存储

```rust
let config = TraceConfig {
    async_storage: true,  // 推荐！
    ..Default::default()
};
```

**影响**:
- ✅ 不阻塞工作流执行
- ✅ 存储失败不影响工作流
- ⚠️ 需要等待异步任务完成才能查询

### 2. 数据大小限制

```rust
let config = TraceConfig {
    max_io_size_bytes: 1024 * 1024,  // 1MB
    ..Default::default()
};
```

**防止**:
- 日志文件过大
- 内存占用过高
- 查询性能下降

### 3. 数据脱敏（生产环境）

```rust
let config = TraceConfig::production(); // 自动脱敏

// 或手动配置
let config = TraceConfig {
    capture_io: false,       // 不捕获输入/输出
    capture_prompts: false,  // 不捕获提示词
    ..Default::default()
};
```

---

## 🐛 调试和故障排查

### 检查追踪是否保存

```rust
// 等待异步存储完成
tokio::time::sleep(Duration::from_millis(200)).await;

// 检查追踪
let trace = storage.get_trace("workflow-id").await?;
if trace.is_none() {
    println!("❌ Trace not found! Check storage configuration.");
}
```

### 查看错误策略

```rust
let config = TraceConfig {
    on_storage_error: StorageErrorPolicy::LogError,  // 记录错误
    // or
    on_storage_error: StorageErrorPolicy::Ignore,    // 忽略错误
    ..Default::default()
};
```

没有"存储失败就让工作流失败"的选项：trace 由后台 drain 任务在工作流产出结果之后才落盘，
此时工作流已经结束。

### 查看正在运行的工作流

```rust
let running = collector.list_running().await;
println!("Running workflows: {}", running.len());

for trace in running {
    println!("  - {}: {} nodes", trace.workflow_id, trace.nodes.len());
}
```

---

## 📚 示例

### 完整示例

参见 `yanshi-tracing/examples/simple_tracing.rs`

运行：
```bash
cargo run --example simple_tracing -p yanshi-tracing
```

### 输出示例

```
╔══════════════════════════════════════════════════════╗
║   Yanshi Tracing System - Simple Example         ║
╚══════════════════════════════════════════════════════╝

🚀 Simulating workflow execution: demo-workflow-001

   ✓ Workflow started
   ✓ Node 'fetch_papers' started
   ✓ Node 'fetch_papers' completed (200ms)
   ✓ Node 'summarize' started
   ✓ LLM prompt sent to gpt-4o
   ✓ LLM response received (300ms, 2000 tokens)
   ✓ Node 'summarize' completed (400ms)
   ✓ Workflow completed (750ms total)

📊 Retrieving execution trace...

[完整追踪输出...]

📈 Statistics:
   - Total nodes executed: 2
   - Duration: 813ms
   - Total tokens used: 2000
   - LLM latency: 300ms

✅ Example completed successfully!
```

---

## 🌐 W3C Trace Context 端到端连续

> 自 v0.3.0 起，Yanshi 在每个 LLM HTTP 出站调用上注入 W3C
> [`traceparent`](https://www.w3.org/TR/trace-context/) header，让 OTel-aware
> 服务器（Jaeger / Tempo / Honeycomb / 自建网关）能把 LLM 一跳作为
> Yanshi 的子 span 接进同一棵 trace，避免在 LLM 边界断开。

### 工作原理

1. `yanshi_llm::LlmTraceContext { trace_id, span_id, flags }` 是 W3C
   wire 格式的 typed 包装。`trace_id` 32 hex / `span_id` 16 hex。
2. 模块用 [tokio task-local](https://docs.rs/tokio/latest/tokio/macro.task_local.html)
   保存当前 context；用 `yanshi_llm::trace_context::scope(ctx, fut).await`
   设置一次，包裹的 future 内所有 LLM 调用都会自动看到。
3. 每个 provider（OpenAI / Anthropic / Google / Moonshot / StepFun）的
   `build_headers()` 末尾调用 `inject_into_headers(&mut headers)`，读取
   task-local 并写出 `traceparent: 00-{trace_id}-{span_id}-{flags}`。

### 自动传播

`AgentContext::trace_context: Option<LlmTraceContext>` 默认为 None；当
非空时，`ReActAgent` / `PlanExecuteAgent` 在每次 `LLMClient` 调用前
通过 `.trace_context(ctx.trace_context.clone())` builder 透传。这样
agent → LLM HTTP 这一跳的 trace 永远连贯。

### 显式调用（不走 agent runtime）

```rust
use yanshi_llm::{Yanshi, LlmTraceContext};

let ctx = LlmTraceContext::random();
let answer = Yanshi::model("gpt-4o-mini")
  .prompt("hello")
  .trace_context(ctx)        // ← 出站 HTTP 现在带 traceparent
  .execute()
  .await?;
```

### 与 OTel 后端对接

把 `LlmTraceContext::trace_id` / `span_id` 对齐到现有 OTel SDK：

- 已经在用 `opentelemetry-rust` ：从 active span 取 `SpanContext`，
  `LlmTraceContext::new(span_ctx.trace_id().to_string(), span_ctx.span_id().to_string())`。
- 只用 `yanshi-tracing` 内置 exporter：`yanshi-tracing/src/otel.rs`
  里的 `trace_id`/`span_id` 生成 W3C 规范要求的随机 16/8 字节 ID（Q2.2.2
  修正——此前是从 `workflow_id` 派生的确定性 ID，违反 W3C "MUST be
  random" 要求，已废弃）。跨进程一致性通过 `ExecutionTrace.metadata.
  external_trace_id`/`external_parent_span_id` 承接入站 `traceparent`
  实现（`trace_to_spans` 优先用这两个字段，没有才回退到随机生成），而不
  是靠确定性派生。W4.4：`yanshi-tracing::otlp`（`otlp-http` feature）
  现在提供开箱即用的 OTLP/HTTP 导出器，不必再自己实现 `OtelSpanSink`。

### 何时不会注入

- `AgentContext::trace_context = None`（默认）：不注入。
- 直接调用 provider 而未走 `LLMClient::with_trace_context`、也未在
  task-local 中安装 context：不注入。

不注入 = 行为与 v0.2.0 完全一致，向后兼容。

---

## 🚀 下一步

1. **查看示例**: `cargo run --example simple_tracing`
2. **阅读设计文档**: `docs/TRACING_DESIGN.md`
3. **集成到你的工作流**: 添加 TraceCollector as EventListener
4. **配置存储**: 选择合适的存储后端

---

## 📖 相关文档

- [追踪系统设计](./TRACING_DESIGN.md) - 详细架构设计
- [当前架构](./ARCHITECTURE.md) - workspace 和 runtime 边界
- [API 文档](../yanshi-tracing/src/lib.rs) - 完整 API 参考

---

**维护者**: Yanshi Team
**问题反馈**: GitHub Issues
**最后更新**: 2025-11-23
