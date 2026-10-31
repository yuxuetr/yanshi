# Yanshi V1 to V2 Migration Guide

> Historical reference: this guide covers the long-completed V1→V2
> migration (a single-crate prototype to the early layered workspace).
> There is no V1 in the current codebase to migrate from. For the
> current architecture, see `docs/ARCHITECTURE.md` and `CLAUDE.md`; for
> current status, see `docs/CURRENT_STATUS.md`.

This guide provides instructions for migrating your workflows and custom nodes from Yanshi V1 to the new V2 architecture.

## Key Architectural Changes

The V2 architecture introduces several major improvements:

1.  **Explicit I/O & Stateless Nodes**: Nodes no longer rely on a single, mutable `SharedState`. Instead, they receive inputs explicitly and produce explicit outputs. This makes data flow clear and robust.
2.  **DAG-based Execution**: Workflows are now defined as a Directed Acyclic Graph (DAG), with dependencies explicitly declared, rather than a linked-list style of execution.
3.  **Structured Control Flow**: Conditional execution (`run_if`) and loops (`map`) are now first-class citizens in the workflow definition.
4.  **Unified Data Type (`FlowValue`)**: A new enum, `FlowValue`, is used to handle all data, including multi-modal content like files and URLs.

## 1. Migrating V1 YAML Workflows to V2

Your V1 YAML file needs to be restructured to fit the new DAG format.

### V1 YAML Example:

```yaml
# V1 format (simplified)
name: old_workflow
workflow:
  type: sequential
  nodes:
    - name: get_topic
      type: template
      parameters:
        prompt: "Give me a topic"
    - name: generate_post
      type: llm
      parameters:
        model: "gpt-4"
        prompt: "Write a blog post about {{ get_topic_output }}"
```

### V2 YAML Example:

In V2, you must define dependencies and map inputs explicitly.

```yaml
# V2 format
name: new_workflow
nodes:
  - id: get_topic
    type: template # Assuming a template node exists
    parameters:
      prompt: "Give me a topic"

  - id: generate_post
    type: llm
    dependencies: ["get_topic"]
    input_mapping:
      prompt: "{{ nodes.get_topic.outputs.text }}" # Maps this node's 'prompt' input to the 'text' output of 'get_topic'
    parameters:
      model: "gpt-4"
```

**Migration Steps:**

1.  **Replace `name` with `id`**: Each node must have a unique `id`.
2.  **Remove `workflow.type`**: The graph structure is now implicit from the dependencies.
3.  **Add `dependencies`**: For each node, list the `id`s of the nodes it depends on.
4.  **Add `input_mapping`**: This is the most critical change. You must explicitly map the inputs of a node to the outputs of its dependencies. The format is `"{{ nodes.<dependency_id>.outputs.<output_name> }}"`.
5.  **Add `run_if` for Conditionals**: If you had conditional logic, replace it with the `run_if` field on the node that should be conditionally executed.

## 2. Migrating Custom Nodes

If you have created custom nodes by implementing the `AsyncNode` or `Node` trait, you need to refactor them.

### V1 Node Trait (Simplified):

```rust
// V1
#[async_trait]
pub trait AsyncNode: Send + Sync {
  async fn prep_async(&self, shared: &SharedState) -> Result<Value>;
  async fn exec_async(&self, prep_result: Value) -> Result<Value>;
  async fn post_async(&self, shared: &SharedState, prep_result: Value, exec_result: Value) -> Result<Option<String>>;
}
```

### V2 Node Trait:

The V1 lifecycle (`prep`, `exec`, `post`) is gone. There is now a single `execute` method.

```rust
// V2
use yanshi_core::{
    async_node::{AsyncNode, AsyncNodeInputs, AsyncNodeResult},
    value::FlowValue,
};

#[async_trait]
pub trait AsyncNode: Send + Sync {
    async fn execute(&self, inputs: &AsyncNodeInputs) -> AsyncNodeResult;
}
```

**Migration Steps:**

1.  **Remove `prep_async` and `post_async`**: All logic should be contained within the `execute` method.
2.  **Implement `execute`**: 
    - Your node is now **stateless**. All configuration and data must be read from the `inputs: &AsyncNodeInputs` map.
    - Do not interact with a shared state object.
    - Return all your node's results as a `HashMap<String, FlowValue>`.
3.  **Use `FlowValue`**: Wrap your outputs in the `FlowValue` enum (e.g., `FlowValue::Json(serde_json::Value::String(my_string))`).

By following these steps, your custom nodes and workflows will be compatible with the more robust and powerful Yanshi V2 architecture.
