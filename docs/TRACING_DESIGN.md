# Yanshi 工作流追踪系统设计

**日期**: 2025-11-23
**版本**: v1.0
**状态**: 设计中

---

## 🎯 设计目标

### 核心需求

1. **详细的执行追踪**
   - 记录每个节点的输入/输出
   - 记录 LLM 提示词（system_prompt, user_prompt）
   - 记录使用的模型信息
   - 记录执行时间、状态、错误信息

2. **用户友好的日志**
   - 为 Web 服务用户提供排错能力
   - 可查询、可过滤的结构化日志
   - 支持实时追踪和历史回溯

3. **架构原则**
   - ❌ 不污染 yanshi-core
   - ✅ 通过事件系统集成
   - ✅ 独立的 crate（yanshi-tracing）
   - ✅ 可选使用，零开销（如果不启用）

---

## 📐 架构设计

### 整体架构

```
┌─────────────────────────────────────────────────────────────┐
│                    Application Layer                        │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ yanshi-cli│  │yanshi-web │  │  Custom App  │      │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘      │
└─────────┼──────────────────┼──────────────────┼─────────────┘
          │                  │                  │
          │                  │                  │
┌─────────▼──────────────────▼──────────────────▼─────────────┐
│              yanshi-tracing (NEW)                         │
│  ┌─────────────────────────────────────────────────────┐    │
│  │         TraceCollector (EventListener)              │    │
│  │  - Structured trace collection                      │    │
│  │  - Multiple storage backends                        │    │
│  │  - Query interface                                  │    │
│  └─────────────────────────────────────────────────────┘    │
│                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ FileStorage  │  │ PostgreSQL   │  │  MongoDB     │      │
│  │  (JSON/JSONL)│  │  (Relational)│  │  (Document)  │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
└──────────────────────────┬───────────────────────────────────┘
                           │
                           │ EventListener trait
                           │
┌──────────────────────────▼───────────────────────────────────┐
│                 yanshi-core                               │
│  ┌─────────────────────────────────────────────────────┐    │
│  │              events.rs (已存在)                      │    │
│  │  - WorkflowEvent 定义                                │    │
│  │  - EventListener trait                               │    │
│  │  - 零依赖的事件系统                                  │    │
│  └─────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────┘
```

### 三层设计

#### 1. Core 层 (yanshi-core)
**职责**: 定义事件，触发事件
**不包含**: 任何日志/追踪实现

```rust
// yanshi-core/src/events.rs (已存在)
pub enum WorkflowEvent {
    NodeStarted {
        workflow_id: String,
        node_id: String,
        node_type: String,        // 新增：节点类型
        timestamp: Instant,
    },
    NodeCompleted {
        workflow_id: String,
        node_id: String,
        duration: Duration,
        input: Option<Value>,     // 新增：节点输入
        output: Option<Value>,    // 新增：节点输出
        timestamp: Instant,
    },
    LLMPromptSent {              // 新增：LLM 专用事件
        workflow_id: String,
        node_id: String,
        model: String,
        system_prompt: Option<String>,
        user_prompt: String,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
        timestamp: Instant,
    },
    LLMResponseReceived {        // 新增：LLM 响应事件
        workflow_id: String,
        node_id: String,
        model: String,
        response: String,
        usage: Option<TokenUsage>,
        duration: Duration,
        timestamp: Instant,
    },
    // ... 其他事件
}

#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}
```

#### 2. Tracing 层 (yanshi-tracing - 新建)
**职责**: 实现 EventListener，收集和存储追踪数据

```rust
// yanshi-tracing/src/lib.rs
pub mod collector;    // TraceCollector - 核心收集器
pub mod storage;      // Storage trait 和实现
pub mod query;        // 查询接口
pub mod format;       // 格式化输出（JSON, 人类可读）
pub mod exporters;    // 导出器（OpenTelemetry, Jaeger, etc.）
```

#### 3. Application 层
**职责**: 配置和使用追踪系统

---

## 🔧 核心组件设计

### 1. TraceCollector (yanshi-tracing)

```rust
// yanshi-tracing/src/collector.rs
use yanshi_core::events::{WorkflowEvent, EventListener};
use serde::{Serialize, Deserialize};
use std::sync::Arc;

/// 执行追踪记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTrace {
    pub workflow_id: String,
    pub workflow_name: Option<String>,
    pub started_at: SystemTime,
    pub completed_at: Option<SystemTime>,
    pub status: TraceStatus,
    pub nodes: Vec<NodeTrace>,
    pub metadata: TraceMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceStatus {
    Running,
    Completed,
    Failed { error: String },
}

/// 节点执行追踪
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTrace {
    pub node_id: String,
    pub node_type: String,
    pub started_at: SystemTime,
    pub completed_at: Option<SystemTime>,
    pub duration_ms: Option<u64>,
    pub status: NodeStatus,

    // 输入输出
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,

    // LLM 专用字段（如果是 LLM 节点）
    pub llm_details: Option<LLMTrace>,

    // 错误信息
    pub error: Option<String>,
}

/// LLM 执行详情
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMTrace {
    pub model: String,
    pub system_prompt: Option<String>,
    pub user_prompt: String,
    pub response: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub usage: Option<TokenUsage>,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub estimated_cost_usd: Option<f64>,  // 可选：估算成本
}

/// 追踪元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceMetadata {
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub tags: Vec<String>,
    pub environment: String,  // "production", "development", etc.
}

/// 追踪收集器 - 实现 EventListener
pub struct TraceCollector {
    storage: Arc<dyn TraceStorage>,
    config: TraceConfig,
    current_traces: Arc<RwLock<HashMap<String, ExecutionTrace>>>,
}

impl TraceCollector {
    pub fn new(storage: Arc<dyn TraceStorage>, config: TraceConfig) -> Self {
        Self {
            storage,
            config,
            current_traces: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 获取工作流的完整追踪
    pub async fn get_trace(&self, workflow_id: &str) -> Result<Option<ExecutionTrace>> {
        // 先查内存（正在运行的）
        if let Some(trace) = self.current_traces.read().await.get(workflow_id) {
            return Ok(Some(trace.clone()));
        }

        // 再查存储（已完成的）
        self.storage.get_trace(workflow_id).await
    }

    /// 查询追踪（支持过滤）
    pub async fn query_traces(&self, query: TraceQuery) -> Result<Vec<ExecutionTrace>> {
        self.storage.query_traces(query).await
    }
}

impl EventListener for TraceCollector {
    fn on_event(&self, event: &WorkflowEvent) {
        // 异步处理事件，不阻塞工作流执行
        let storage = self.storage.clone();
        let traces = self.current_traces.clone();
        let event = event.clone();

        tokio::spawn(async move {
            if let Err(e) = Self::process_event(storage, traces, event).await {
                eprintln!("Failed to process trace event: {}", e);
            }
        });
    }
}

impl TraceCollector {
    async fn process_event(
        storage: Arc<dyn TraceStorage>,
        traces: Arc<RwLock<HashMap<String, ExecutionTrace>>>,
        event: WorkflowEvent,
    ) -> Result<()> {
        match event {
            WorkflowEvent::WorkflowStarted { workflow_id, timestamp } => {
                let trace = ExecutionTrace {
                    workflow_id: workflow_id.clone(),
                    workflow_name: None,
                    started_at: SystemTime::now(),
                    completed_at: None,
                    status: TraceStatus::Running,
                    nodes: Vec::new(),
                    metadata: TraceMetadata::default(),
                };
                traces.write().await.insert(workflow_id, trace);
            }

            WorkflowEvent::NodeStarted { workflow_id, node_id, node_type, .. } => {
                if let Some(trace) = traces.write().await.get_mut(&workflow_id) {
                    trace.nodes.push(NodeTrace {
                        node_id,
                        node_type,
                        started_at: SystemTime::now(),
                        completed_at: None,
                        duration_ms: None,
                        status: NodeStatus::Running,
                        input: None,
                        output: None,
                        llm_details: None,
                        error: None,
                    });
                }
            }

            WorkflowEvent::NodeCompleted { workflow_id, node_id, duration, input, output, .. } => {
                if let Some(trace) = traces.write().await.get_mut(&workflow_id) {
                    if let Some(node) = trace.nodes.iter_mut().find(|n| n.node_id == node_id) {
                        node.completed_at = Some(SystemTime::now());
                        node.duration_ms = Some(duration.as_millis() as u64);
                        node.status = NodeStatus::Completed;
                        node.input = input;
                        node.output = output;
                    }
                }
            }

            WorkflowEvent::LLMPromptSent {
                workflow_id, node_id, model, system_prompt, user_prompt,
                temperature, max_tokens, ..
            } => {
                if let Some(trace) = traces.write().await.get_mut(&workflow_id) {
                    if let Some(node) = trace.nodes.iter_mut().find(|n| n.node_id == node_id) {
                        node.llm_details = Some(LLMTrace {
                            model,
                            system_prompt,
                            user_prompt,
                            response: String::new(),  // 稍后填充
                            temperature,
                            max_tokens,
                            usage: None,
                            latency_ms: 0,
                        });
                    }
                }
            }

            WorkflowEvent::LLMResponseReceived {
                workflow_id, node_id, response, usage, duration, ..
            } => {
                if let Some(trace) = traces.write().await.get_mut(&workflow_id) {
                    if let Some(node) = trace.nodes.iter_mut().find(|n| n.node_id == node_id) {
                        if let Some(ref mut llm) = node.llm_details {
                            llm.response = response;
                            llm.usage = usage;
                            llm.latency_ms = duration.as_millis() as u64;
                        }
                    }
                }
            }

            WorkflowEvent::WorkflowCompleted { workflow_id, .. } => {
                if let Some(mut trace) = traces.write().await.remove(&workflow_id) {
                    trace.completed_at = Some(SystemTime::now());
                    trace.status = TraceStatus::Completed;

                    // 持久化到存储
                    storage.save_trace(&trace).await?;
                }
            }

            WorkflowEvent::WorkflowFailed { workflow_id, error, .. } => {
                if let Some(mut trace) = traces.write().await.remove(&workflow_id) {
                    trace.completed_at = Some(SystemTime::now());
                    trace.status = TraceStatus::Failed { error };

                    // 持久化到存储
                    storage.save_trace(&trace).await?;
                }
            }

            _ => {}
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeStatus {
    Running,
    Completed,
    Failed,
    Skipped,
}

/// 追踪配置
#[derive(Debug, Clone)]
pub struct TraceConfig {
    /// 是否记录输入/输出（可能包含敏感数据）
    pub capture_io: bool,

    /// 是否记录 LLM 提示词
    pub capture_prompts: bool,

    /// 输入/输出最大长度（防止日志过大）
    pub max_io_size_bytes: usize,

    /// 是否异步存储（推荐）
    pub async_storage: bool,

    /// 存储失败时的行为
    pub on_storage_error: StorageErrorPolicy,
}

#[derive(Debug, Clone)]
pub enum StorageErrorPolicy {
    Ignore,           // 忽略错误，不影响工作流
    LogError,         // 记录错误但继续
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            capture_io: true,
            capture_prompts: true,
            max_io_size_bytes: 1024 * 1024, // 1MB
            async_storage: true,
            on_storage_error: StorageErrorPolicy::LogError,
        }
    }
}
```

### 2. Storage 抽象 (yanshi-tracing)

```rust
// yanshi-tracing/src/storage.rs
use async_trait::async_trait;

/// 追踪存储 trait
#[async_trait]
pub trait TraceStorage: Send + Sync {
    /// 保存追踪
    async fn save_trace(&self, trace: &ExecutionTrace) -> Result<()>;

    /// 获取追踪
    async fn get_trace(&self, workflow_id: &str) -> Result<Option<ExecutionTrace>>;

    /// 查询追踪
    async fn query_traces(&self, query: TraceQuery) -> Result<Vec<ExecutionTrace>>;

    /// 删除旧追踪（清理）
    async fn delete_old_traces(&self, older_than: SystemTime) -> Result<usize>;
}

/// 查询条件
#[derive(Debug, Clone, Default)]
pub struct TraceQuery {
    pub workflow_ids: Option<Vec<String>>,
    pub status: Option<TraceStatus>,
    pub user_id: Option<String>,
    pub tags: Option<Vec<String>>,
    pub time_range: Option<TimeRange>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct TimeRange {
    pub start: SystemTime,
    pub end: SystemTime,
}

// ============ 实现 1: 文件存储 (开发环境) ============

pub struct FileTraceStorage {
    base_path: PathBuf,
}

impl FileTraceStorage {
    pub fn new(base_path: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&base_path)?;
        Ok(Self { base_path })
    }

    fn trace_path(&self, workflow_id: &str) -> PathBuf {
        self.base_path.join(format!("{}.json", workflow_id))
    }
}

#[async_trait]
impl TraceStorage for FileTraceStorage {
    async fn save_trace(&self, trace: &ExecutionTrace) -> Result<()> {
        let path = self.trace_path(&trace.workflow_id);
        let json = serde_json::to_string_pretty(trace)?;
        tokio::fs::write(path, json).await?;
        Ok(())
    }

    async fn get_trace(&self, workflow_id: &str) -> Result<Option<ExecutionTrace>> {
        let path = self.trace_path(workflow_id);
        if !path.exists() {
            return Ok(None);
        }
        let json = tokio::fs::read_to_string(path).await?;
        let trace = serde_json::from_str(&json)?;
        Ok(Some(trace))
    }

    async fn query_traces(&self, query: TraceQuery) -> Result<Vec<ExecutionTrace>> {
        // 简单实现：读取所有文件并过滤
        let mut traces = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.base_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
                let json = tokio::fs::read_to_string(entry.path()).await?;
                if let Ok(trace) = serde_json::from_str::<ExecutionTrace>(&json) {
                    if Self::matches_query(&trace, &query) {
                        traces.push(trace);
                    }
                }
            }
        }

        // 应用 limit/offset
        if let Some(offset) = query.offset {
            traces = traces.into_iter().skip(offset).collect();
        }
        if let Some(limit) = query.limit {
            traces.truncate(limit);
        }

        Ok(traces)
    }

    async fn delete_old_traces(&self, older_than: SystemTime) -> Result<usize> {
        let mut count = 0;
        let mut entries = tokio::fs::read_dir(&self.base_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
            if let Ok(modified) = metadata.modified() {
                if modified < older_than {
                    tokio::fs::remove_file(entry.path()).await?;
                    count += 1;
                }
            }
        }

        Ok(count)
    }
}

impl FileTraceStorage {
    fn matches_query(trace: &ExecutionTrace, query: &TraceQuery) -> bool {
        if let Some(ref ids) = query.workflow_ids {
            if !ids.contains(&trace.workflow_id) {
                return false;
            }
        }

        if let Some(ref status) = query.status {
            if !std::mem::discriminant(&trace.status) == std::mem::discriminant(status) {
                return false;
            }
        }

        if let Some(ref user_id) = query.user_id {
            if trace.metadata.user_id.as_ref() != Some(user_id) {
                return false;
            }
        }

        // ... 其他过滤条件

        true
    }
}

// ============ 实现 2: PostgreSQL (生产环境) — 历史设计草稿，未落地 ============
//
// W4.4 核实：以下代码从未实现，且与实际的 DDL schema 不一致——
// `yanshi-tracing::storage::schema::POSTGRES_TRACE_SCHEMA` 是
// `trace_runs`/`trace_steps`/`trace_events`/`trace_tool_calls`/
// `trace_mcp_calls` 五表规范化设计，不是下面这种单表 `execution_traces`
// + JSONB 列的方案。当年支撑这个草稿的 `postgres` Cargo feature（挂了
// 一个零引用的 `sqlx` 依赖）已在 W4.4 移除。如果未来真的要实现 SQL
// 存储后端，应该以 `storage/schema.rs` 里的真实 DDL 为准重新设计，而
// 不是照抄下面这段代码——保留仅作历史参考。

pub struct PostgresTraceStorage {
    pool: sqlx::PgPool,
}

impl PostgresTraceStorage {
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;

        // 创建表
        Self::create_tables(&pool).await?;

        Ok(Self { pool })
    }

    async fn create_tables(pool: &sqlx::PgPool) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS execution_traces (
                workflow_id TEXT PRIMARY KEY,
                workflow_name TEXT,
                started_at TIMESTAMPTZ NOT NULL,
                completed_at TIMESTAMPTZ,
                status TEXT NOT NULL,
                user_id TEXT,
                session_id TEXT,
                tags TEXT[],
                environment TEXT,
                trace_data JSONB NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );

            CREATE INDEX IF NOT EXISTS idx_traces_user_id ON execution_traces(user_id);
            CREATE INDEX IF NOT EXISTS idx_traces_started_at ON execution_traces(started_at);
            CREATE INDEX IF NOT EXISTS idx_traces_status ON execution_traces(status);
            CREATE INDEX IF NOT EXISTS idx_traces_tags ON execution_traces USING GIN(tags);
            "#
        )
        .execute(pool)
        .await?;

        Ok(())
    }
}

#[async_trait]
impl TraceStorage for PostgresTraceStorage {
    async fn save_trace(&self, trace: &ExecutionTrace) -> Result<()> {
        let trace_json = serde_json::to_value(trace)?;
        let status_str = match &trace.status {
            TraceStatus::Running => "running",
            TraceStatus::Completed => "completed",
            TraceStatus::Failed { .. } => "failed",
        };

        sqlx::query(
            r#"
            INSERT INTO execution_traces
                (workflow_id, workflow_name, started_at, completed_at, status,
                 user_id, session_id, tags, environment, trace_data)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT (workflow_id)
            DO UPDATE SET
                completed_at = EXCLUDED.completed_at,
                status = EXCLUDED.status,
                trace_data = EXCLUDED.trace_data
            "#
        )
        .bind(&trace.workflow_id)
        .bind(&trace.workflow_name)
        .bind(trace.started_at)
        .bind(trace.completed_at)
        .bind(status_str)
        .bind(&trace.metadata.user_id)
        .bind(&trace.metadata.session_id)
        .bind(&trace.metadata.tags)
        .bind(&trace.metadata.environment)
        .bind(trace_json)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn get_trace(&self, workflow_id: &str) -> Result<Option<ExecutionTrace>> {
        let row = sqlx::query!(
            r#"SELECT trace_data FROM execution_traces WHERE workflow_id = $1"#,
            workflow_id
        )
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => {
                let trace = serde_json::from_value(row.trace_data)?;
                Ok(Some(trace))
            }
            None => Ok(None),
        }
    }

    async fn query_traces(&self, query: TraceQuery) -> Result<Vec<ExecutionTrace>> {
        // 构建动态查询（简化版本）
        let mut sql = String::from("SELECT trace_data FROM execution_traces WHERE 1=1");

        if let Some(ref status) = query.status {
            let status_str = match status {
                TraceStatus::Running => "running",
                TraceStatus::Completed => "completed",
                TraceStatus::Failed { .. } => "failed",
            };
            sql.push_str(&format!(" AND status = '{}'", status_str));
        }

        if let Some(ref user_id) = query.user_id {
            sql.push_str(&format!(" AND user_id = '{}'", user_id));
        }

        sql.push_str(" ORDER BY started_at DESC");

        if let Some(limit) = query.limit {
            sql.push_str(&format!(" LIMIT {}", limit));
        }

        if let Some(offset) = query.offset {
            sql.push_str(&format!(" OFFSET {}", offset));
        }

        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;

        let mut traces = Vec::new();
        for row in rows {
            let trace_data: serde_json::Value = row.try_get("trace_data")?;
            let trace = serde_json::from_value(trace_data)?;
            traces.push(trace);
        }

        Ok(traces)
    }

    async fn delete_old_traces(&self, older_than: SystemTime) -> Result<usize> {
        let result = sqlx::query!(
            r#"DELETE FROM execution_traces WHERE started_at < $1"#,
            older_than
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() as usize)
    }
}

// ============ 实现 3: MongoDB (可选) ============
// 类似实现...
```

### 3. 查询和导出接口

```rust
// yanshi-tracing/src/query.rs

/// 人类可读格式输出
pub fn format_trace_human_readable(trace: &ExecutionTrace) -> String {
    let mut output = String::new();

    output.push_str(&format!("Workflow: {} ({})\n",
        trace.workflow_name.as_deref().unwrap_or("Unnamed"),
        trace.workflow_id
    ));

    output.push_str(&format!("Status: {:?}\n", trace.status));
    output.push_str(&format!("Started: {:?}\n", trace.started_at));

    if let Some(completed_at) = trace.completed_at {
        let duration = completed_at.duration_since(trace.started_at).unwrap();
        output.push_str(&format!("Duration: {:?}\n", duration));
    }

    output.push_str("\nNodes:\n");
    for (i, node) in trace.nodes.iter().enumerate() {
        output.push_str(&format!("\n[{}] {} ({})\n", i + 1, node.node_id, node.node_type));
        output.push_str(&format!("  Status: {:?}\n", node.status));

        if let Some(duration_ms) = node.duration_ms {
            output.push_str(&format!("  Duration: {}ms\n", duration_ms));
        }

        if let Some(ref llm) = node.llm_details {
            output.push_str(&format!("  Model: {}\n", llm.model));
            output.push_str(&format!("  System Prompt: {}\n",
                llm.system_prompt.as_deref().unwrap_or("N/A")));
            output.push_str(&format!("  User Prompt: {}\n", llm.user_prompt));
            output.push_str(&format!("  Response: {}\n", llm.response));

            if let Some(ref usage) = llm.usage {
                output.push_str(&format!("  Tokens: {} + {} = {}\n",
                    usage.prompt_tokens, usage.completion_tokens, usage.total_tokens));
            }
        }

        if let Some(ref input) = node.input {
            output.push_str(&format!("  Input: {}\n",
                serde_json::to_string_pretty(input).unwrap_or_default()));
        }

        if let Some(ref output_val) = node.output {
            output.push_str(&format!("  Output: {}\n",
                serde_json::to_string_pretty(output_val).unwrap_or_default()));
        }
    }

    output
}

/// 导出为 JSON
pub fn export_trace_json(trace: &ExecutionTrace) -> Result<String> {
    Ok(serde_json::to_string_pretty(trace)?)
}

/// 导出为 OpenTelemetry 格式
pub fn export_trace_otel(trace: &ExecutionTrace) -> Result<Vec<u8>> {
    // 转换为 OpenTelemetry Span 格式
    // TODO: 实现
    todo!("OpenTelemetry export")
}
```

### W3C Trace Context 出站传播 (v0.3.0+)

**问题**：OTel exporter 把 Yanshi 内部 trace 转成 span 树（workflow →
node → agent → tool / LLM），但 LLM HTTP 一跳是出站调用 —— 接收方
（OpenAI / Anthropic / 自建 LLM gateway）创建的 span 与发送方的 trace 不
共享 trace_id，导致 trace 在 LLM 边界断裂。

**解法**：W3C Trace Context（`traceparent` header）是 OTel 跨服务传播
的标准。Yanshi 现在在每个 LLM HTTP 出站请求上自动注入：

```text
traceparent: 00-{trace_id}-{span_id}-{flags}
```

**Wiring**：
- `yanshi_llm::trace_context::LlmTraceContext { trace_id, span_id, flags, tracestate }` 是 W3C 包装。
- tokio `task_local!` 持有当前 context；`scope(ctx, fut).await` 安装。
- 每个 provider 的 `build_headers` 末尾调用 `inject_into_headers(&mut headers)`，读取 task-local 写出 traceparent。
- `LLMClient::with_trace_context(ctx)` 让显式 LLM 调用安装 context。
- `AgentContext::trace_context` 让 ReAct / Plan-Execute runtime 自动透传到底层 LLM 调用，无需用户手动 plumbing。

**不影响 v0.2.0 行为**：context 默认 None；只有显式安装时才注入。
向后兼容。

详细使用见 [`TRACING_USAGE.md`](TRACING_USAGE.md#-w3c-trace-context-端到端连续)。

---

## 🔌 集成方式

### 方式 1: CLI 使用（开发环境）

```rust
// yanshi-cli/src/main.rs
use yanshi_tracing::{TraceCollector, FileTraceStorage, TraceConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // 1. 创建存储
    let storage = Arc::new(FileTraceStorage::new(
        PathBuf::from("~/.yanshi/traces")
    )?);

    // 2. 创建追踪收集器
    let trace_config = TraceConfig::default();
    let collector = Box::new(TraceCollector::new(storage.clone(), trace_config));

    // 3. 创建工作流，注入收集器
    let flow = Flow::new()
        .with_listener(collector)  // 通过 EventListener 集成！
        .add_node(llm_node)
        .add_node(template_node);

    // 4. 执行工作流
    let workflow_id = flow.execute().await?;

    // 5. 获取追踪结果
    let trace = storage.get_trace(&workflow_id).await?;

    // 6. 输出人类可读格式
    if let Some(trace) = trace {
        println!("{}", format_trace_human_readable(&trace));
    }

    Ok(())
}
```

### 方式 2: Web 服务使用（生产环境）

```rust
// yanshi-web/src/main.rs
use yanshi_tracing::{TraceCollector, PostgresTraceStorage, TraceConfig};
use axum::{routing::get, Router, Json};

#[tokio::main]
async fn main() -> Result<()> {
    // 1. 创建 PostgreSQL 存储
    let storage = Arc::new(PostgresTraceStorage::new(
        &env::var("DATABASE_URL")?
    ).await?);

    // 2. 创建全局追踪收集器
    let collector = Arc::new(TraceCollector::new(
        storage.clone(),
        TraceConfig {
            capture_io: true,
            capture_prompts: true,
            max_io_size_bytes: 1024 * 1024,
            async_storage: true,
            on_storage_error: StorageErrorPolicy::LogError,
        }
    ));

    // 3. 创建 Web API
    let app = Router::new()
        .route("/api/workflows/:id/execute", post(execute_workflow))
        .route("/api/traces/:id", get(get_trace))
        .route("/api/traces", get(list_traces))
        .with_state(AppState { collector, storage });

    // 4. 启动服务
    axum::Server::bind(&"0.0.0.0:8080".parse()?)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

struct AppState {
    collector: Arc<TraceCollector>,
    storage: Arc<dyn TraceStorage>,
}

/// 执行工作流
async fn execute_workflow(
    State(state): State<AppState>,
    Json(req): Json<ExecuteWorkflowRequest>,
) -> Result<Json<ExecuteWorkflowResponse>> {
    // 创建工作流，注入追踪收集器
    let flow = Flow::from_yaml(&req.workflow_yaml)?
        .with_listener(Box::new(state.collector.as_ref().clone()));

    // 执行
    let workflow_id = flow.execute().await?;

    Ok(Json(ExecuteWorkflowResponse { workflow_id }))
}

/// 获取追踪详情（用户排错）
async fn get_trace(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> Result<Json<ExecutionTrace>> {
    let trace = state.storage.get_trace(&workflow_id).await?
        .ok_or_else(|| anyhow!("Trace not found"))?;

    Ok(Json(trace))
}

/// 列出追踪（查询）
async fn list_traces(
    State(state): State<AppState>,
    Query(params): Query<TraceQueryParams>,
) -> Result<Json<Vec<ExecutionTrace>>> {
    let query = TraceQuery {
        user_id: params.user_id,
        status: params.status,
        limit: Some(params.limit.unwrap_or(50)),
        offset: params.offset,
        ..Default::default()
    };

    let traces = state.storage.query_traces(query).await?;
    Ok(Json(traces))
}
```

---

## 📝 Core 中需要新增的事件

### 更新 yanshi-core/src/events.rs

```rust
// 只需要添加新的事件变体，不改变架构

pub enum WorkflowEvent {
    // ... 已有事件

    // ===== 新增：详细追踪事件 =====

    /// Node execution started with input
    NodeStarted {
        workflow_id: String,
        node_id: String,
        node_type: String,        // NEW
        input: Option<Value>,     // NEW
        timestamp: Instant,
    },

    /// Node execution completed with output
    NodeCompleted {
        workflow_id: String,
        node_id: String,
        duration: Duration,
        input: Option<Value>,     // NEW
        output: Option<Value>,    // NEW
        timestamp: Instant,
    },

    /// LLM prompt sent (详细的 LLM 追踪)
    LLMPromptSent {
        workflow_id: String,
        node_id: String,
        model: String,
        provider: String,         // "openai", "anthropic", etc.
        system_prompt: Option<String>,
        user_prompt: String,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
        timestamp: Instant,
    },

    /// LLM response received
    LLMResponseReceived {
        workflow_id: String,
        node_id: String,
        model: String,
        response: String,
        usage: Option<TokenUsage>,
        duration: Duration,
        timestamp: Instant,
    },
}

/// Token usage statistics
#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}
```

### 在 LLM Node 中触发事件

```rust
// yanshi-llm/src/client.rs (或 LLMNode 实现)

impl LLMNode {
    async fn execute(&mut self, context: &mut ExecutionContext) -> Result<Value> {
        let workflow_id = context.workflow_id();
        let node_id = &self.id;

        // 1. 发送 LLMPromptSent 事件
        context.emit_event(WorkflowEvent::LLMPromptSent {
            workflow_id: workflow_id.clone(),
            node_id: node_id.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            system_prompt: self.system_prompt.clone(),
            user_prompt: self.prompt.clone(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            timestamp: Instant::now(),
        });

        // 2. 调用 LLM
        let start = Instant::now();
        let response = self.client.complete(&self.prompt).await?;
        let duration = start.elapsed();

        // 3. 发送 LLMResponseReceived 事件
        context.emit_event(WorkflowEvent::LLMResponseReceived {
            workflow_id: workflow_id.clone(),
            node_id: node_id.clone(),
            model: self.model.clone(),
            response: response.text.clone(),
            usage: Some(TokenUsage {
                prompt_tokens: response.usage.prompt_tokens,
                completion_tokens: response.usage.completion_tokens,
                total_tokens: response.usage.total_tokens,
            }),
            duration,
            timestamp: Instant::now(),
        });

        Ok(serde_json::to_value(&response.text)?)
    }
}
```

---

## 🌐 Web API 设计

### RESTful API 端点

```
GET    /api/traces                    # 列出所有追踪
GET    /api/traces/:workflow_id       # 获取特定追踪
GET    /api/traces/:workflow_id/logs  # 获取格式化日志
DELETE /api/traces/:workflow_id       # 删除追踪
POST   /api/traces/query              # 高级查询
```

### 响应示例

```json
// GET /api/traces/wf-123456
{
  "workflow_id": "wf-123456",
  "workflow_name": "AI Research Assistant",
  "started_at": "2025-11-23T10:30:00Z",
  "completed_at": "2025-11-23T10:30:45Z",
  "status": "completed",
  "nodes": [
    {
      "node_id": "search_arxiv",
      "node_type": "HttpNode",
      "started_at": "2025-11-23T10:30:00Z",
      "completed_at": "2025-11-23T10:30:05Z",
      "duration_ms": 5000,
      "status": "completed",
      "input": {
        "query": "large language models"
      },
      "output": {
        "papers": [...]
      }
    },
    {
      "node_id": "summarize",
      "node_type": "LLMNode",
      "started_at": "2025-11-23T10:30:05Z",
      "completed_at": "2025-11-23T10:30:35Z",
      "duration_ms": 30000,
      "status": "completed",
      "llm_details": {
        "model": "gpt-4",
        "system_prompt": "You are a research assistant...",
        "user_prompt": "Summarize the following papers...",
        "response": "Here is a summary of the papers...",
        "temperature": 0.7,
        "max_tokens": 2000,
        "usage": {
          "prompt_tokens": 1500,
          "completion_tokens": 500,
          "total_tokens": 2000,
          "estimated_cost_usd": 0.06
        },
        "latency_ms": 30000
      }
    }
  ],
  "metadata": {
    "user_id": "user-789",
    "tags": ["research", "arxiv"],
    "environment": "production"
  }
}
```

---

## 📊 实现路线图

### Phase 1: 基础追踪 (3-4 天)

**Week 1**:
- [ ] 创建 `yanshi-tracing` crate
- [ ] 实现 `TraceCollector` 和 `ExecutionTrace` 结构
- [ ] 实现 `FileTraceStorage`
- [ ] 更新 `yanshi-core/events.rs` 添加新事件
- [ ] 在 `yanshi-llm` 中触发 LLM 事件

**Week 2**:
- [ ] 在 CLI 中集成追踪系统
- [ ] 实现查询和导出接口
- [ ] 编写单元测试和集成测试
- [ ] 编写使用文档

### Phase 2: 生产级存储 (4-5 天)

**Week 3**:
- [ ] 实现 `PostgresTraceStorage`
- [ ] 实现数据库迁移脚本
- [ ] 添加索引优化查询性能
- [ ] 实现批量写入优化

**Week 4**:
- [ ] 实现 Web API 端点
- [ ] 添加认证和授权
- [ ] 性能测试和优化
- [ ] 生产环境部署文档

### Phase 3: 高级特性 (可选)

- [ ] MongoDB 存储实现
- [ ] OpenTelemetry 导出
- [ ] 实时追踪流（WebSocket）
- [ ] 追踪可视化 UI
- [ ] 成本分析和预算告警

---

## ✅ 设计优势

1. **不污染 Core** ✅
   - Core 只定义事件，不包含任何追踪实现
   - 通过 `EventListener` trait 集成

2. **零开销（如果不使用）** ✅
   - 不启用追踪时，只有事件定义开销（几乎为零）
   - 使用 `NoOpListener` 时编译器可以优化掉

3. **灵活的存储** ✅
   - 开发环境：文件存储
   - 生产环境：PostgreSQL/MongoDB
   - 可扩展：自定义存储实现

4. **完整的可观测性** ✅
   - 工作流级别追踪
   - 节点级别详情
   - LLM 专用追踪（提示词、响应、成本）
   - 用户友好的查询接口

5. **生产就绪** ✅
   - 异步存储，不阻塞工作流
   - 错误容忍（存储失败不影响工作流）
   - 数据清理机制
   - 安全性考虑（敏感数据可配置）

---

## 🔐 安全考虑

### 敏感数据处理

```rust
impl TraceConfig {
    /// 生产环境配置（脱敏）
    pub fn production() -> Self {
        Self {
            capture_io: false,           // 不记录输入/输出
            capture_prompts: false,      // 不记录提示词
            max_io_size_bytes: 0,
            async_storage: true,
            on_storage_error: StorageErrorPolicy::LogError,
        }
    }

    /// 开发环境配置（完整追踪）
    pub fn development() -> Self {
        Self {
            capture_io: true,
            capture_prompts: true,
            max_io_size_bytes: 10 * 1024 * 1024,  // 10MB
            async_storage: true,
            on_storage_error: StorageErrorPolicy::Ignore,
        }
    }
}
```

### 数据脱敏

```rust
impl TraceCollector {
    /// 脱敏敏感字段
    fn sanitize_value(value: &mut Value, config: &TraceConfig) {
        // 移除 API keys, passwords, tokens 等
        if let Value::Object(map) = value {
            let sensitive_keys = ["api_key", "password", "token", "secret"];
            for key in sensitive_keys {
                if map.contains_key(key) {
                    map.insert(key.to_string(), Value::String("[REDACTED]".into()));
                }
            }
        }
    }
}
```

---

**下一步**: 开始实现 Phase 1？
