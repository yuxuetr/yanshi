# 循环节点功能实现总结

> Historical reference：本文档记录的是 Map/While 循环节点最初落地时的
> 实现总结（早期版本快照）。Map/While 早已是 `yanshi-core` 的稳定
> 生产能力，细节以当前源码与 `CLAUDE.md` 为准；当前项目状态见
> `docs/CURRENT_STATUS.md`。**文档内全部行号/文件路径引用均已失效**
> （`factory` 早已迁移到 `yanshi-config::executor::factory`，
> `flow.rs` 已膨胀到 5000+ 行，`execute_while_node` 等函数的实际行号
> 与本文档所写完全对不上）——把本文档当叙事/设计动机参考，不要依赖任何
> 具体行号或路径去定位代码。

## 概述

Yanshi 现已完全支持两种循环节点类型：**Map 节点**（批量处理）和 **While 节点**（条件循环）。

## 已完成的工作

### 1. 核心功能实现 ✅

#### Map 节点 (yanshi-core/src/flow.rs:19, 99-223)
- **顺序处理**: `execute_map_node_sequential` - 逐个处理列表元素
- **并行处理**: `execute_map_node_parallel` - 使用 tokio::spawn 并发处理
- 支持嵌套子工作流
- 自动传递 `{{ item }}` 变量给子工作流

#### While 节点 (yanshi-core/src/flow.rs:20-24, 116-160)
- 条件评估和循环控制
- 最大迭代次数保护（防止无限循环）
- 状态传递和更新机制
- 子工作流输出自动合并回循环状态

### 2. YAML 配置解析 ✅

**文件**: `yanshi-cli/src/executor/factory.rs`

#### Map 节点解析 (第 95-101 行)
```yaml
- id: "process_list"
  type: "map"
  parameters:
    input_list: [1, 2, 3]
    parallel: false  # 可选，默认 false
    template:
      - id: "process_item"
        type: "template"
        parameters:
          template: "Item: {{ item }}"
```

#### While 节点解析 (第 87-94 行)
```yaml
- id: "loop"
  type: "while"
  parameters:
    condition: "{{ should_continue }}"
    max_iterations: 10
    counter: 0
    should_continue: true
    do:
      - id: "step"
        type: "template"
        parameters:
          template: "Count: {{ counter }}"
```

### 3. 模板变量修复 ✅

**问题**: 模板占位符 `{{ variable }}` （有空格）没有被正确替换

**解决方案**: 修改 `yanshi-nodes/src/nodes/template.rs` (第 55-72 行)
- 现在同时支持 `{{ variable }}` 和 `{{variable}}` 两种格式
- 先替换预定义变量，再替换输入变量（允许覆盖）

### 4. 示例工作流 ✅

创建了 4 个示例文件在 `yanshi-cli/templates/`:

1. **map-example.yml** - Map 节点顺序处理示例
2. **map-parallel-example.yml** - Map 节点并行处理示例
3. **while-example.yml** - While 循环基础示例
4. **while-advanced-example.yml** - While 循环 + LLM 集成示例

### 5. 测试覆盖 ✅

#### 单元测试 (yanshi-core/src/flow.rs:345-573)
- `test_map_node_sequential_execution` - Map 顺序处理
- `test_map_node_parallel_execution` - Map 并行处理
- `test_while_node_basic_loop` - While 基础循环
- `test_while_node_condition_check` - While 条件检查

#### 集成测试 (yanshi-cli/tests/workflow_tests.rs)
- `test_parallel_map_workflow` - 并行 LLM 调用（第 144-198 行）
- `test_stateful_while_loop_workflow` - 有状态 While 循环（第 201-261 行）

**测试结果**: 所有测试通过 ✅
```
running 4 tests
test flow::tests::test_while_node_condition_check ... ok
test flow::tests::test_map_node_sequential_execution ... ok
test flow::tests::test_while_node_basic_loop ... ok
test flow::tests::test_map_node_parallel_execution ... ok
```

### 6. 文档 ✅

**完整使用指南**: `yanshi-cli/templates/LOOP_NODES_GUIDE.md`
- Map 节点详细说明和示例
- While 节点详细说明和示例
- 嵌套循环示例
- 最佳实践
- 常见问题解答

## 如何使用

### 运行示例

```bash
# Map 节点示例
cargo run --release -- workflow run yanshi-cli/templates/map-example.yml

# While 节点示例
cargo run --release -- workflow run yanshi-cli/templates/while-example.yml

# 高级 While 示例（需要 STEPFUN_API_KEY）
export STEPFUN_API_KEY=your_key_here
cargo run --release -- workflow run yanshi-cli/templates/while-advanced-example.yml
```

### 运行测试

```bash
# 核心流程测试
cargo test --package yanshi-core --lib flow::tests

# 集成测试（需要 STEPFUN_API_KEY）
export STEPFUN_API_KEY=your_key_here
cargo test --package yanshi-cli workflow_tests
```

## 技术细节

### Map 节点工作原理

1. 接收 `input_list` 数组作为输入
2. 为每个元素创建一个子工作流实例
3. 将当前元素作为 `item` 变量传递给子工作流
4. 收集所有子工作流的结果
5. 返回包含所有结果的 `results` 数组

**并行 vs 顺序**:
- `parallel: false`: 使用 for 循环顺序执行
- `parallel: true`: 使用 tokio::spawn 并发执行，然后用 futures::future::join_all 等待

### While 节点工作原理

1. 使用提供的初始变量开始
2. 每次迭代：
   - 评估条件表达式（模板替换后检查真值）
   - 如果为 true，执行子工作流
   - 找到子工作流的退出节点（没有依赖的节点）
   - 将退出节点的输出合并回循环变量
3. 当条件为 false 或达到 max_iterations 时停止
4. 返回最终的循环变量作为输出

### 条件评估规则

在 `execute_while_node` (flow.rs:136) 中：
- 空字符串 → false
- "false" (字符串) → false
- "0" (字符串) → false
- 其他任何值 → true

## 已知限制

1. **错误处理**: Map/While 节点中任何子工作流失败都会导致整个节点失败
2. **变量作用域**: While 循环变量在整个循环过程中共享，可能导致命名冲突
3. **调试**: 循环内部的调试信息有限
4. **性能**: 并行 Map 节点没有并发限制（可能需要添加 semaphore）

## 未来改进方向

1. **错误恢复**: 支持部分失败继续执行
2. **并发控制**: 为并行 Map 添加最大并发数限制
3. **Break/Continue**: 支持提前退出和跳过迭代
4. **循环变量作用域**: 更好的变量隔离
5. **进度报告**: 长时间运行的循环的进度指示
6. **调试模式**: 更详细的循环执行日志

## 相关文件清单

### 核心实现
- `yanshi-core/src/flow.rs` - Flow 执行引擎和循环节点实现

### CLI 和配置
- `yanshi-cli/src/executor/factory.rs` - YAML 解析和节点创建
- `yanshi-cli/src/config/v2.rs` - 工作流配置结构定义

### 节点实现
- `yanshi-nodes/src/nodes/template.rs` - 模板节点（修复了空格支持）

### 测试
- `yanshi-core/src/flow.rs` - 单元测试（第 345-573 行）
- `yanshi-cli/tests/workflow_tests.rs` - 集成测试

### 示例和文档
- `yanshi-cli/templates/map-example.yml`
- `yanshi-cli/templates/map-parallel-example.yml`
- `yanshi-cli/templates/while-example.yml`
- `yanshi-cli/templates/while-advanced-example.yml`
- `yanshi-cli/templates/LOOP_NODES_GUIDE.md`

## 总结

循环节点功能已完全实现并经过测试。用户现在可以：

1. ✅ 在 YAML 工作流中使用 Map 和 While 节点
2. ✅ 顺序或并行处理数据列表
3. ✅ 实现条件循环和迭代式工作流
4. ✅ 嵌套循环以处理复杂场景
5. ✅ 在循环中使用所有标准节点类型（LLM、HTTP、Template 等）

所有核心功能都已实现，测试通过，文档完善。可以投入生产使用。
