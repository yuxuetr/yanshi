# Yanshi Project - Current Implementation Status

> Archived historical reference (snapshot dated 2025-10-25). For the
> maintained current status, see `docs/CURRENT_STATUS.md`, which is the
> authoritative source. The workspace has since grown from the 6 crates
> described below to 15 Rust crates + 1 Web UI crate; subsequent
> evaluations live in `OVERALL_EVALUATION_REPORT.md` (2026-04-28),
> `docs/archive/PROJECT_EVALUATION_2026-05-01.md`, and
> `docs/archive/PROJECT_EVALUATION_2026-05-14.md`. The content below is preserved as
> a snapshot, not as a description of current state.

## Executive Summary

Yanshi is a Rust-based workflow orchestration platform with comprehensive LLM integration support. The project is structured as a workspace with 6 core crates, providing both SDK (code-first) and CLI (configuration-first) interfaces. The implementation is mature in core areas (workflow execution, LLM providers, built-in nodes) while MCP/RAG features documented in CLAUDE.md remain in early planning stages.

## Workspace Structure

The project is organized as a Cargo workspace with the following members:

```
yanshi/
├── yanshi-core/      - Core workflow execution engine
├── yanshi-nodes/     - Built-in node implementations
├── yanshi-llm/       - Unified LLM provider interface
├── yanshi-cli/       - Command-line interface and runner
├── yanshi-mcp/       - Model Context Protocol integration (early stage)
└── yanshi-agents/    - Reusable AI agent applications
```

## Crate Implementation Status

### 1. yanshi-core (Core Engine)

**Status: IMPLEMENTED & MATURE**

**Responsibility**: Workflow execution engine, async node framework, state management

**Key Files**:
- `src/flow.rs` (28KB) - Flow execution engine with topological sorting, conditional execution, and control flow
- `src/async_node.rs` (3.6KB) - AsyncNode trait definition with typed I/O
- `src/value.rs` (3.1KB) - FlowValue enum for multimodal data passing
- `src/error.rs` (3.4KB) - Custom error types and result handling
- `src/observability.rs` (13KB) - Event tracking and metrics collection
- `src/robustness.rs` (31KB) - Circuit breakers, rate limiting, timeout management

**Implemented Features**:
- ✅ Topological sorting for DAG execution
- ✅ Async/await based execution with Tokio
- ✅ Map nodes (sequential and parallel iteration)
- ✅ While nodes (conditional loops with max iteration protection)
- ✅ Conditional execution (run_if expressions)
- ✅ Input mapping between nodes using template syntax
- ✅ State persistence to ~/.yanshi/runs/<run_id>/
- ✅ FlowValue abstraction (JSON, File, URL types)
- ✅ Observability with tracing support

**Node Types Defined**:
```rust
pub enum NodeType {
    Standard(Arc<dyn AsyncNode>),
    Map { template: Vec<GraphNode>, parallel: bool },
    While {
        condition: String,
        max_iterations: u32,
        template: Vec<GraphNode>
    },
}
```

**Testing**: Integration tests in `tests/workflow_integration_tests.rs`

---

### 2. yanshi-nodes (Built-in Node Implementations)

**Status: IMPLEMENTED & FUNCTIONAL**

**Responsibility**: Pre-built node implementations for common workflow tasks

**Node Types Implemented** (2,636 lines total):

#### Text/LLM Nodes
- **LlmNode** (114 lines) - Execute LLM prompts, supports model, system, temperature, max_tokens
- **TemplateNode** (314 lines) - Tera template rendering with custom filters and functions
  - Supports loop variables, input variables, Tera expressions
  - Custom filters: `json`, `to_json`, `safe_string`

#### Image Processing Nodes (text-to-image APIs)
- **TextToImageNode** (447 lines) - Stable Diffusion, DALL-E, and other image generation APIs
- **ImageToImageNode** (151 lines) - Image transformation with strength/steps controls
- **ImageEditNode** (147 lines) - Inpainting with masking support
- **ImageUnderstandNode** (128 lines) - Vision models (Claude, GPT-4o) for image analysis

#### Audio Nodes
- **TtsNode** (158 lines) - Text-to-speech with voice selection (OpenAI, StepFun)
- **AsrNode** (140 lines) - Automatic speech recognition (Whisper API, StepFun)

#### Utility Nodes
- **HttpNode** (129 lines) - HTTP requests with GET/POST/PUT/DELETE, JSON payloads
- **FileNode** (93 lines) - Read/write files, directory listing
- **BatchNode** (168 lines) - Parallel processing of arrays with configurable concurrency
- **ConditionalNode** (223 lines) - Conditional logic (Exists, Equals, GreaterThan, LessThan, Contains)

#### Specialized Nodes
- **MarkMapNode** (198 lines) - Convert markdown to interactive mind maps (HTML)
- **ArxivNode** (192 lines) - Fetch research papers from arXiv, extract plain text, simplify LaTeX

**Features**:
- ✅ Tera template engine integration with custom functions
- ✅ Streaming support for HTTP and some LLM operations
- ✅ Error handling with custom NodeError type
- ✅ Feature-gated compilation (llm, http, file, template, batch, conditional)
- ✅ Factory pattern support for configuration-first workflows

**File Structure**:
```
yanshi-nodes/src/
├── lib.rs
├── error.rs
├── factory_traits.rs
├── factories/
│   └── mod.rs (factory implementations)
├── nodes/
│   ├── llm.rs
│   ├── template.rs
│   ├── http.rs
│   ├── file.rs
│   ├── batch.rs
│   ├── conditional.rs
│   ├── text_to_image.rs
│   ├── image_to_image.rs
│   ├── image_edit.rs
│   ├── image_understand.rs
│   ├── tts.rs
│   ├── asr.rs
│   ├── markmap.rs
│   ├── arxiv.rs
│   └── while.rs (empty)
└── common/
    ├── utils.rs
    └── tera_helpers.rs
```

---

### 3. yanshi-llm (LLM Integration)

**Status: IMPLEMENTED & COMPREHENSIVE**

**Responsibility**: Unified interface for multiple LLM providers with multimodal support

**Supported LLM Providers** (3,089 lines total):

#### Provider Implementations
1. **OpenAI** (410 lines) - GPT-4o, GPT-4o-mini, GPT-4-turbo
2. **Anthropic** (439 lines) - Claude-3.5-Sonnet, Claude-3-Haiku
3. **Google** (453 lines) - Gemini-1.5-Pro, Gemini-1.5-Flash
4. **Moonshot** (398 lines) - Chinese-focused LLM provider
5. **StepFun** (1,204 lines) - Most comprehensive, includes vision, audio, and text models

#### Core Components
- **LLMClient** (`client/llm_client.rs`) - Request/response handling
- **Streaming** (`client/streaming.rs`) - Token streaming support
- **ModelRegistry** (`registry/model_registry.rs`) - Model discovery and validation
- **ModelFetcher** (`discovery/model_fetcher.rs`) - Fetch model lists from providers
- **ModelValidator** (`discovery/model_validator.rs`) - Validate model capabilities
- **ConfigUpdater** (`discovery/config_updater.rs`) - Update local model configurations
- **VendorConfigManager** (`config/vendor_configs.rs`) - Handle provider-specific configs
- **Multimodal** (`multimodal.rs`, 9.7KB) - Text+image message building
- **ModelTypes** (`model_types.rs`, 15.7KB) - Model capabilities and metadata

**Configuration System**:
- Built-in defaults in `yanshi-llm/templates/default_models.yml`
  (authoritative; embedded via `include_str!`)
- User config: `~/.yanshi/models.yml` (takes precedence over
  built-in when present; auto-generated on first use)

**Features**:
- ✅ Unified LLM interface (Yanshi fluent API)
- ✅ Streaming response handling
- ✅ Multimodal support (text + images)
- ✅ Model registry with auto-discovery
- ✅ Temperature, max_tokens, top_p, top_k parameters
- ✅ System message support
- ✅ API key management (env vars + config files)
- ✅ Error handling with custom LLMError type

**Example Usage**:
```rust
Yanshi::init().await?;
let response = Yanshi::model("gpt-4o")
    .prompt("Hello, world!")
    .temperature(0.7)
    .max_tokens(1000)
    .execute().await?;
```

---

### 4. yanshi-cli (Command-Line Interface)

**Status: IMPLEMENTED & FUNCTIONAL**

**Responsibility**: User-facing CLI for workflow execution and LLM interactions

**Command Structure** (`src/main.rs`, 6.7KB):

```
yanshi
├── workflow
│   └── run <file> [--watch] [--output] [--input KEY VALUE] [--dry-run] [--timeout] [--max-retries]
├── llm
│   ├── chat <model> <prompt> [--system] [--temperature] [--max-tokens]
│   └── models [--provider] [--available]
├── image
│   ├── generate <prompt> [--model] [--output]
│   └── understand <image_path> [--prompt] [--model]
├── audio
│   ├── tts <text> [--model] [--output] [--voice]
│   ├── asr <file_path> [--model] [--language] [--prompt] [--format]
│   └── clone <text> <file_id> <output> [--model] [--sample-text]
└── config
    ├── init
    ├── validate
    └── show
```

**Subcommands Directory** (`src/commands/`):
- `workflow/run.rs` - Parse and execute YAML workflows
- `workflow/validate.rs` - Validate workflow syntax
- `llm/chat.rs` - Direct LLM interaction
- `llm/models.rs` - List available models
- `image/generate.rs` - Text-to-image generation
- `image/understand.rs` - Image understanding with LLMs
- `audio/tts.rs` - Text-to-speech synthesis
- `audio/asr.rs` - Speech-to-text recognition
- `audio/clone.rs` - Voice cloning (partial implementation)
- `config/init.rs` - Initialize configuration
- `config/show.rs` - Display current config
- `config/validate.rs` - Validate config files

**Workflow Execution** (`src/executor/`):
- `runner.rs` - Main workflow execution orchestrator
- `factory.rs` - YAML to GraphNode factory (parses node types, converts to core types)

**Features**:
- ✅ YAML workflow parsing and validation
- ✅ Template variable interpolation with Tera
- ✅ Conditional workflow execution
- ✅ Loop support (map, while)
- ✅ Input mapping between nodes
- ✅ Progress reporting with indicatif
- ✅ JSON output formatting
- ✅ Dry-run mode
- ✅ Timeout and retry configuration

**Configuration** (`src/config/`):
- `mod.rs` - Configuration loading and management
- `~/.yanshi/config.yml` - User configuration file

---

### 5. yanshi-mcp (Model Context Protocol)

**Status: EARLY STAGE / PLANNING**

**Responsibility**: MCP client/server integration for external tool access

**Files Implemented** (~700 lines):
- `src/lib.rs` - Module organization
- `src/client.rs` - MCP client traits (stub)
- `src/server.rs` (80 lines) - Basic MCP server with stdio transport
- `src/tools.rs` - Tool calling infrastructure
- `src/transport.rs` - JSON-RPC transport layer
- `src/error.rs` - MCP error types

**Current Status**:
- ⚠️ Basic framework in place but NOT feature-complete
- ⚠️ No official Rust MCP SDK yet (commented in Cargo.toml)
- ⚠️ MCPToolNode referenced in yanshi-agents but uses undefined SharedState
- ⚠️ Stdio transport partially implemented
- ⚠️ HTTP transport NOT implemented

**Features Status**:
- ✅ Basic server framework
- ✅ Tool definition and calling structures
- ❌ Client implementation
- ❌ Full server request/response handling
- ❌ Resource management
- ❌ Prompt template handling
- ❌ HTTP transport

**Dependencies**: No external MCP SDK (waiting for official Rust implementation)

---

### 6. yanshi-agents (AI Agent Applications)

**Status: FRAMEWORK STAGE**

**Responsibility**: Reusable AI agent applications and specialized node implementations

**Structure** (`src/`):
- `traits/agent.rs` - Agent trait definitions
- `nodes/mcp_tool_node.rs` - Generic MCP tool execution (incomplete, references undefined SharedState)
- `common/` - Utilities for agents
  - `batch_processor.rs` - Batch processing utilities
  - `file_utils.rs` - File I/O helpers
  - `pdf_parser.rs` - PDF parsing
  - `output_formatter.rs` - Result formatting

**Agent Examples** (`agents/`):
- **paper_assistant/** - Academic paper analysis workflow
- **paper_research_analyzer/** - Research paper analysis with visual outputs

**Status**:
- ✅ Common utilities implemented
- ⚠️ Agent traits defined but limited implementations
- ⚠️ MCPToolNode has unresolved references (SharedState not in core)

---

## Testing Coverage

**Test Types**:

### Unit Tests
- **yanshi-core** (`src/*.rs`): Inline tests in modules
  - `async_node.rs` - AsyncNode trait tests
  - `flow.rs` - Map and While node tests

### Integration Tests
- **yanshi-core** (`tests/workflow_integration_tests.rs`, 100+ lines)
  - `test_simple_two_step_llm_workflow` - Sequential LLM execution
  - `test_conditional_workflow_runs` - Conditional node execution
  - Requires STEPFUN_API_KEY environment variable

- **yanshi-cli** (`tests/workflow_tests.rs`)
  - `test_parallel_map_workflow` - Parallel node execution
  - `test_stateful_while_loop_workflow` - Loop state management

**Test Results**: All tests passing (requires API keys for LLM tests)

---

## Workflow Templates & Examples

**Template Workflows** (`yanshi-cli/templates/`):

1. **simple.yml** - Basic single-node workflow
2. **llm-chain.yml** - Multi-step LLM chaining
3. **map-example.yml** - Sequential map node
4. **map-parallel-example.yml** - Parallel map node
5. **map-simple-test.yml** - Minimal map example
6. **while-example.yml** - Basic while loop
7. **while-advanced-example.yml** - While + LLM integration
8. **tera-loop-example.yml** - Template variable looping
9. **tera-conditional-example.yml** - Conditional templates
10. **tera-filters-example.yml** - Tera filter demonstrations
11. **tera-complex-report-example.yml** - Complex report generation

**Complex Example** (`yanshi-cli/examples/ai_research_assistant.yml`):
- Multi-step research workflow with:
  - While loop for arXiv paper search
  - LLM summarization
  - Language detection
  - Conditional translation
  - MarkMap visualization

---

## Feature Implementation Status

### COMPLETED & TESTED

- ✅ **Core Workflow Engine**
  - Topological DAG execution
  - Input/output mapping
  - State persistence
  - Async/concurrent execution
  
- ✅ **Control Flow**
  - Conditional execution (run_if)
  - Map nodes (sequential + parallel)
  - While loops with max iterations
  - Nested workflow templates

- ✅ **LLM Integration**
  - 5 provider implementations (OpenAI, Anthropic, Google, Moonshot, StepFun)
  - Multimodal support (text + images)
  - Streaming responses
  - Model discovery and validation
  - Configuration management

- ✅ **Built-in Nodes**
  - Text: LLM, Template
  - Image: TextToImage, ImageToImage, ImageEdit, ImageUnderstand
  - Audio: TTS, ASR
  - Utility: HTTP, File, Batch, Conditional
  - Specialized: MarkMap, Arxiv

- ✅ **CLI Interface**
  - Workflow execution and validation
  - LLM interaction commands
  - Image generation/understanding
  - Audio processing (TTS/ASR)
  - Configuration management

- ✅ **Template Engine**
  - Tera integration
  - Custom filters and functions
  - Variable interpolation
  - Conditional rendering

### IN PROGRESS

- ⚠️ **Voice Cloning** (Audio)
  - Basic structure present
  - Marked with TODO comments for file upload API

- ⚠️ **Configuration System**
  - Partial factory pattern implementation
  - Some nodes need factory improvements

### PLANNED / NOT STARTED

- ❌ **MCP (Model Context Protocol)**
  - Framework structure only
  - No working client/server implementation
  - Blocked by official Rust SDK availability
  - MCPToolNode references undefined SharedState

- ❌ **RAG (Retrieval-Augmented Generation)**
  - No implementation started
  - Vector store abstractions planned but not coded
  - Would require: yanshi-rag crate, vector DB integrations

- ❌ **MCP-First LLM Context**
  - Auto-discovery of MCP tools
  - Hybrid MCP + RAG strategies
  - Context strategy enums

- ❌ **Distributed Workflows**
  - No multi-node execution support
  - No remote node communication

- ❌ **WebAssembly (WASM) Nodes**
  - No WASM runtime integration

---

## Code Quality

### Code Organization
- Clear separation of concerns across crates
- Trait-based abstractions (AsyncNode, NodeFactory, LLMProvider)
- Feature-gated compilation for optional functionality
- Well-documented public APIs with doc comments

### Error Handling
- Custom error types per crate (YanshiError, NodeError, LLMError, MCPError)
- Using `thiserror` for error derivation
- Comprehensive error variants with contextual information

### Documentation
- ARCHITECTURE.md - V2 design principles
- LOOP_NODES_IMPLEMENTATION.md - Implementation details in Chinese
- TERA_INTEGRATION_ANALYSIS.md - Template engine integration
- Inline documentation in source files
- Example workflows and templates

### Known Issues
- MCPToolNode references undefined `SharedState` (should use `AsyncNodeInputs`)
- Voice cloning has multiple TODO comments for incomplete APIs
- Some factory implementations incomplete

---

## Performance Characteristics

### Execution Model
- **Async/Await**: Tokio runtime for non-blocking execution
- **Parallelism**: Map nodes support parallel execution via tokio::spawn
- **Concurrency Control**: Robustness module includes rate limiting and circuit breakers
- **State Management**: In-memory HashMap for flow state (not distributed)

### Resource Management
- FlowValue design passes large data by reference (File/URL types)
- State persistence only after successful node execution
- No explicit memory limits (unbounded state growth possible)

---

## Dependency Analysis

### Core Dependencies
- **tokio** - Async runtime
- **serde/serde_json/serde_yaml** - Serialization
- **reqwest** - HTTP client
- **tera** - Template engine
- **clap** - CLI argument parsing
- **uuid** - Unique identifiers

### Optional Dependencies (via features)
- **handlebars** - Alternative template engine (unused in practice)
- **tracing** - Observability (feature-gated)

### Missing/Unavailable
- **rmcp** - Official Rust MCP SDK (commented out, not yet released)

---

## Gaps Between Documentation & Implementation

### Documented in CLAUDE.md but NOT Implemented

1. **MCP Integration**
   - Documented: Full MCP client/server, auto-discovery
   - Actual: Basic framework, no working implementation

2. **RAG Integration**
   - Documented: yanshi-rag crate, vector store abstractions
   - Actual: No RAG crate, no implementations

3. **MCPNode & RAGNode**
   - Documented: Native node types with context discovery
   - Actual: No MCPNode, attempted MCPToolNode with unresolved references

4. **Hybrid Context Strategies**
   - Documented: RAGFirst, MCPFirst, Smart strategies
   - Actual: Not implemented

5. **Distributed Execution**
   - Documented: Planned feature
   - Actual: No implementation

6. **WASM Support**
   - Documented: Planned
   - Actual: No implementation

---

## Recommendations for Future Development

### Priority 1: Stabilize Existing Features
1. Resolve MCPToolNode references
2. Complete voice cloning implementation
3. Improve factory pattern implementations
4. Add more comprehensive error handling

### Priority 2: MCP Implementation
1. Wait for official Rust MCP SDK or implement from spec
2. Implement MCPClient properly
3. Add resource and prompt support
4. Integration with LLMNode for tool discovery

### Priority 3: RAG Integration
1. Create yanshi-rag crate
2. Implement vector store abstractions
3. Add popular vector DB integrations (Pinecone, Weaviate, Qdrant)
4. Create RAGNode for retrieval

### Priority 4: Advanced Features
1. Distributed workflow execution
2. WASM node support
3. Hybrid context strategies
4. Enhanced observability and monitoring

---

## Summary by Feature Completeness

| Feature | Status | Notes |
|---------|--------|-------|
| **Core Execution** | ✅ Complete | DAG, control flow, state management |
| **LLM Providers** | ✅ Complete | 5 providers, multimodal support |
| **Built-in Nodes** | ✅ Complete | 16+ node types implemented |
| **CLI Interface** | ✅ Complete | All major commands implemented |
| **YAML Workflows** | ✅ Complete | Full parsing and execution |
| **Template Engine** | ✅ Complete | Tera with custom functions |
| **Testing** | ✅ Partial | Core tests present, LLM tests need keys |
| **MCP** | ⚠️ Early | Framework only, not functional |
| **RAG** | ❌ Planned | Not started |
| **Voice Cloning** | ⚠️ WIP | Partial implementation with TODOs |
| **Distributed** | ❌ Planned | Not started |
| **WASM** | ❌ Planned | Not started |

