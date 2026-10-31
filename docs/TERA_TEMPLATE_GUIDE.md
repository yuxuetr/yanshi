# Yanshi Tera 模板使用指南

## 概述

Yanshi 现已集成 Tera 模板引擎，提供强大的模板渲染功能。Tera 是一个受 Jinja2 启发的模板引擎，支持条件、循环、过滤器等高级特性。

## 快速开始

### 基本变量替换

```yaml
- id: "simple"
  type: "template"
  parameters:
    template: "Hello {{ name }}!"
    name: "World"
```

**输出**: `Hello World!`

### 支持空格

Tera 支持变量名前后带空格：

```yaml
template: "{{ name }}"  # 推荐
template: "{{name}}"    # 也支持
```

## 核心功能

### 1. 条件语句

#### 基本 if/else

```yaml
template: |
  {% if user_type == "admin" %}
    Welcome, Administrator!
  {% else %}
    Welcome, User!
  {% endif %}
```

#### if/elif/else

```yaml
template: |
  {% if score >= 90 %}
    Grade: A
  {% elif score >= 80 %}
    Grade: B
  {% elif score >= 70 %}
    Grade: C
  {% else %}
    Grade: F
  {% endif %}
```

#### 条件检查

```yaml
# 检查是否存在
{% if variable %}...{% endif %}

# 检查是否定义
{% if variable is defined %}...{% endif %}

# 检查是否为空
{% if list is empty %}...{% endif %}

# 比较运算
{% if age > 18 %}...{% endif %}
{% if status == "active" %}...{% endif %}
{% if count != 0 %}...{% endif %}
```

### 2. 循环

#### 基本循环

```yaml
template: |
  {% for item in items %}
    - {{ item }}
  {% endfor %}
```

#### 循环变量

Tera 提供了特殊的 `loop` 变量：

```yaml
template: |
  {% for item in items %}
    {{ loop.index }}. {{ item }}        # 1-based index
    Index0: {{ loop.index0 }}           # 0-based index
    First: {{ loop.first }}             # true if first
    Last: {{ loop.last }}               # true if last
  {% endfor %}
```

#### 循环对象

```yaml
template: |
  {% for key, value in object %}
    {{ key }}: {{ value }}
  {% endfor %}
```

#### 带条件的循环

```yaml
template: |
  {% for task in tasks %}
    {% if task.completed %}
      ✓ {{ task.title }}
    {% endif %}
  {% endfor %}
```

### 3. 过滤器

过滤器用于转换变量值。

#### 字符串过滤器

```yaml
{{ name | upper }}              # 大写: ALICE
{{ name | lower }}              # 小写: alice
{{ name | title }}              # 标题: Alice
{{ name | capitalize }}         # 首字母大写: Alice
{{ text | truncate(length=50) }}# 截断
{{ text | wordcount }}          # 词数
{{ text | reverse }}            # 反转
{{ text | trim }}               # 去除空白
```

#### 数字过滤器

```yaml
{{ price | round }}             # 四舍五入
{{ price | round(precision=2) }}# 保留2位小数
{{ number | abs }}              # 绝对值
```

#### 数组过滤器

```yaml
{{ items | length }}            # 长度
{{ items | first }}             # 第一个元素
{{ items | last }}              # 最后一个元素
{{ items | join(sep=", ") }}    # 连接
{{ items | reverse }}           # 反转
{{ items | slice(start=0, end=5) }} # 切片
{{ items | sort }}              # 排序
{{ items | unique }}            # 去重
```

#### 条件过滤器

```yaml
{{ name | default(value="Unknown") }}  # 默认值
{{ items | filter(attribute="status", value="active") }}  # 过滤
```

#### 日期过滤器

```yaml
{{ timestamp | date(format="%Y-%m-%d") }}
{{ timestamp | date(format="%H:%M:%S") }}
```

#### 自定义过滤器

Yanshi 提供了额外的自定义过滤器：

```yaml
{{ data | json_pretty }}        # 美化JSON
{{ data | to_json }}            # 转为JSON字符串
{{ path | flow_path }}          # 处理路径
```

### 4. 数学运算

Tera 支持基本的数学运算：

```yaml
{{ count + 1 }}                 # 加法
{{ price - discount }}          # 减法
{{ quantity * price }}          # 乘法
{{ total / count }}             # 除法
{{ number % 2 }}                # 取模
```

复杂表达式：

```yaml
{{ (price * quantity) * 1.1 }}  # 含税价格
{{ total - (total * discount / 100) }}  # 折扣后价格
```

### 5. 对象和数组访问

#### 对象属性

```yaml
{{ user.name }}                 # 点号访问
{{ user["email"] }}             # 括号访问
{{ user.profile.address }}      # 嵌套访问
```

#### 数组索引

```yaml
{{ items.0 }}                   # 第一个元素
{{ items.1 }}                   # 第二个元素
{{ items[index] }}              # 动态索引
```

### 6. 变量赋值

使用 `set` 语句创建新变量：

```yaml
template: |
  {% set total = items | length %}
  {% set completed = items | filter(attribute="done", value=true) | length %}

  Total: {{ total }}
  Completed: {{ completed }} ({{ completed * 100 / total }}%)
```

### 7. 内置函数

Yanshi 提供了两个自定义函数：

```yaml
{{ now() }}                     # 当前UTC时间戳
{{ uuid() }}                    # 生成UUID
```

## 实用示例

### 示例 1: 用户欢迎消息

```yaml
- id: "welcome"
  type: "template"
  parameters:
    template: |
      {% if user_type == "admin" %}
      🔑 Welcome, Administrator {{ name | title }}!
      {% elif user_type == "member" %}
      👤 Hello, {{ name | title }}!
      {% else %}
      👋 Welcome, Guest!
      {% endif %}
    user_type: "member"
    name: "alice"
```

### 示例 2: 任务列表

```yaml
- id: "task_list"
  type: "template"
  parameters:
    template: |
      # Tasks ({{ tasks | length }})

      {% for task in tasks %}
      {{ loop.index }}. [{% if task.done %}✓{% else %} {% endif %}] {{ task.title }}
         Priority: {{ task.priority | upper }}
      {% endfor %}
    tasks:
      - title: "Complete project"
        done: true
        priority: "high"
      - title: "Write docs"
        done: false
        priority: "medium"
```

### 示例 3: 数据报告

```yaml
- id: "report"
  type: "template"
  parameters:
    template: |
      # {{ project_name }} Report

      {% set total = tasks | length %}
      {% set completed = tasks | filter(attribute="status", value="done") | length %}

      ## Summary
      - Total Tasks: {{ total }}
      - Completed: {{ completed }} ({{ completed * 100 / total | round }}%)
      - Pending: {{ total - completed }}

      ## Details
      {% for task in tasks %}
      - {{ task.name }}: {{ task.status | upper }}
      {% endfor %}

      ---
      Generated: {{ now() }}
    project_name: "Yanshi"
    tasks:
      - name: "Task 1"
        status: "done"
      - name: "Task 2"
        status: "pending"
```

### 示例 4: 条件格式化

```yaml
- id: "format_price"
  type: "template"
  parameters:
    template: |
      Price: ${{ price | round(precision=2) }}
      {% if discount > 0 %}
      Discount: {{ discount }}%
      Final Price: ${{ (price * (100 - discount) / 100) | round(precision=2) }}
      You save: ${{ (price * discount / 100) | round(precision=2) }}!
      {% endif %}
    price: 99.99
    discount: 20
```

## 与循环节点配合

Tera 模板可以与 Map/While 节点无缝配合：

### 在 Map 节点中使用

```yaml
- id: "process_items"
  type: "map"
  parameters:
    input_list: [1, 2, 3, 4, 5]
    template:
      - id: "format"
        type: "template"
        parameters:
          template: |
            Item {{ item }}:
            - Squared: {{ item * item }}
            - Doubled: {{ item * 2 }}
            {% if item % 2 == 0 %}
            - Type: Even
            {% else %}
            - Type: Odd
            {% endif %}
```

### 在 While 节点中使用

```yaml
- id: "countdown"
  type: "while"
  parameters:
    condition: "{{ count > 0 }}"
    max_iterations: 10
    count: 5
    do:
      - id: "update"
        type: "template"
        parameters:
          template: |
            {
              "count": {{ count - 1 }},
              "message": "{% if count == 1 %}Done!{% else %}{{ count - 1 }} remaining{% endif %}"
            }
        output_format: "json"
```

## 输出格式

### 文本输出（默认）

```yaml
- id: "text"
  type: "template"
  parameters:
    template: "Hello {{ name }}"
    # output_format 默认为 "text"
```

### JSON 输出

```yaml
- id: "json"
  type: "template"
  parameters:
    template: |
      {
        "name": "{{ name }}",
        "age": {{ age }},
        "active": {% if active %}true{% else %}false{% endif %}
      }
    output_format: "json"
    name: "Alice"
    age: 30
    active: true
```

## 调试技巧

### 1. 打印变量

```yaml
template: |
  Debug: {{ variable }}
  Type: {{ variable | type_of }}
```

### 2. 检查数组长度

```yaml
template: |
  {% if items | length > 0 %}
    Items exist: {{ items | length }}
  {% else %}
    No items
  {% endif %}
```

### 3. 美化JSON

```yaml
template: |
  {{ complex_data | json_pretty }}
```

## 最佳实践

### 1. 使用有意义的变量名

```yaml
# 好
user_name: "Alice"
is_admin: true

# 不好
n: "Alice"
f: true
```

### 2. 保持模板简洁

```yaml
# 好 - 简单清晰
template: |
  {% if active %}Active{% else %}Inactive{% endif %}

# 不好 - 过度复杂
template: |
  {% if active == true and status != "disabled" and not is_suspended %}
    Active
  {% else %}
    Inactive
  {% endif %}
```

### 3. 使用 set 简化复杂计算

```yaml
# 好
template: |
  {% set completion_rate = completed * 100 / total %}
  Progress: {{ completion_rate | round }}%

# 不好
template: |
  Progress: {{ completed * 100 / total | round }}%
```

### 4. 适当使用注释

```yaml
template: |
  {# This generates a user summary #}
  Name: {{ name }}
  {# Calculate completion percentage #}
  {% set progress = tasks_done * 100 / total_tasks %}
  Progress: {{ progress | round }}%
```

## 错误处理

### 使用 default 过滤器

```yaml
{{ optional_field | default(value="N/A") }}
{{ user.name | default(value="Anonymous") }}
```

### 检查变量是否定义

```yaml
{% if variable is defined %}
  {{ variable }}
{% else %}
  Variable not set
{% endif %}
```

## 性能提示

1. **模板缓存**: Tera 模板会被自动编译和缓存
2. **避免深层嵌套**: 过深的循环和条件会影响性能
3. **合理使用过滤器**: 过滤器链不宜过长

## 与旧版本的兼容性

✅ **完全向后兼容**

旧的简单模板仍然有效：

```yaml
# 这些都能正常工作
template: "Hello {{ name }}"
template: "Count: {{ count }}"
template: "{{ greeting }} {{ name }}"
```

## 示例文件

查看更多示例：

- `yanshi-cli/templates/tera-conditional-example.yml`
- `yanshi-cli/templates/tera-loop-example.yml`
- `yanshi-cli/templates/tera-filters-example.yml`
- `yanshi-cli/templates/tera-complex-report-example.yml`

## 运行示例

```bash
# 条件示例
cargo run -- workflow run yanshi-cli/templates/tera-conditional-example.yml

# 循环示例
cargo run -- workflow run yanshi-cli/templates/tera-loop-example.yml

# 过滤器示例
cargo run -- workflow run yanshi-cli/templates/tera-filters-example.yml

# 复杂报告示例
cargo run -- workflow run yanshi-cli/templates/tera-complex-report-example.yml
```

## 更多资源

- [Tera 官方文档](https://keats.github.io/tera/docs/)
- [Tera 过滤器列表](https://keats.github.io/tera/docs/#built-in-filters)
- [Tera 测试列表](https://keats.github.io/tera/docs/#built-in-testers)

---

**最后更新**: 2025-10-06
**Yanshi 版本**: 0.1.0
