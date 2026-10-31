# Yanshi V2 Architecture

> Historical reference: this document captured the early V2 refactor's
> design intent (layered SDK/CLI/plugin vision) before most of the
> workspace existed. For the current five-layer contract-kernel
> architecture (L0–L4, `cargo xtask check-arch`), see `docs/ARCHITECTURE.md`
> and `CLAUDE.md`; for current status, see `docs/CURRENT_STATUS.md`.

This document outlines the core architectural principles and design of Yanshi V2. It serves as the foundational guide for the refactoring and future development of the framework.

## 1. Core Philosophy: A Layered, Extensible Framework

Yanshi is designed as a layered framework to cater to different user groups, from core framework developers to non-technical users.

- **Layer 1: Core SDK (Code-First)**: A comprehensive Rust library (`yanshi-core`, etc.) providing maximum power, performance, and type-safety for developers to create custom nodes or embed Yanshi into larger applications.
- **Layer 2: Runtime & CLI (Config-First)**: A standalone binary (`yanshi-cli`) that executes workflows defined in declarative YAML files. This layer prioritizes ease of use, dynamic execution, and accessibility for non-programmers.
- **Layer 3: Plugin Ecosystem (Hybrid)**: The synergy of the first two layers. Developers use the Core SDK to build and distribute custom node packages (plugins), which can then be used by anyone in their YAML workflows.

## 2. DataFlow and State Management

To ensure robustness and clarity, we are moving from a shared-state model to an **Explicit Input/Output (I/O)** model.

### 2.1. The `FlowValue` Enum

A unified data wrapper, `FlowValue`, will be used for all data passed between nodes. This allows for handling heterogeneous, multi-modal data in a type-safe and efficient manner.

- **Principle**: Pass large data (files, images) by **reference** (path or URL) and small data (text, numbers) by **value**.
- **Structure (Conceptual)**:
  ```rust
  pub enum FlowValue {
      Json(serde_json::Value),
      File { path: PathBuf, mime_type: Option<String> },
      Url { url: String, mime_type: Option<String> },
  }
  ```
- **Persistence**: `FlowValue` will have a defined JSON serialization format for state persistence (e.g., `FlowValue::File` becomes `{"type": "file", "path": "..."}`).

### 2.2. Namespaced State Pool

The flow engine will manage an in-memory, namespaced state pool.

- **Structure**: `HashMap<NodeId, HashMap<OutputName, FlowValue>>`
- **Access Pattern**: Nodes declare their inputs by referencing outputs from previous nodes using a template syntax (e.g., `input_a: "{{ nodes.node_1.outputs.result }}"`).

## 3. Control Flow

The engine will support complex, dynamic workflows through structured control flow constructs, not arbitrary graph cycles.

- **Conditional Execution**: Nodes will have an optional `run_if` field containing an expression. The engine will evaluate this expression to decide whether to run or skip a node.
- **Loops**:
    - **Map (`for-each`)**: A special `map` node type will iterate over a list, executing a sub-workflow template for each item.
    - **While (Conditional Loop)**: A `while` construct will execute a sub-workflow as long as a specified condition holds true.

## 4. Persistence and Resumability

The engine will persist state after each successful node execution to ensure fault tolerance.

- **Strategy**: For each workflow run, create a dedicated directory (`/runs/<run_id>`).
- **Artifacts**: After each node `N` completes, save:
    - `N_outputs.json`: The direct, granular output of the node.
    - `state_after_N.json`: A snapshot of the entire cumulative state pool.
- **Recovery**: The engine can resume a failed workflow by loading the last valid state snapshot and continuing from the next node.
