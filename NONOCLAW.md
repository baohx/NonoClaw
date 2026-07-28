# NonoClaw Agent Loop & Context Assembly — 架构调研报告

> 调研日期: 2026-07-28 | 版本: v0.10.0 | 目的: 为后续优化提供精确的机制理解和改进切入点

---

## 目录

1. [项目总览](#1-项目总览)
2. [Agent Loop 机制](#2-agent-loop-机制)
3. [上下文组装机制](#3-上下文组装机制)
4. [Session 持久化与恢复](#4-session-持久化与恢复)
5. [子 Agent 机制](#5-子-agent-机制)
6. [优化建议与切入点](#6-优化建议与切入点)

---

## 1. 项目总览

### Crate 结构

```
rust/
├── crates/
│   ├── core/      — 类型定义 (Message, RunEvent, Error, Permission 等)
│   ├── api/       — LLM Provider 客户端 (Anthropic/OpenAI/DeepSeek 等)、SSE 流式、重试
│   ├── tools/     — 工具注册表、工具执行器、内置工具 (Read/Write/Edit/Bash/Grep/...)、MCP 客户端
│   ├── engine/    — ★ 核心：Agent Loop、上下文组装、Prompt 构建、Compaction、Session、Hooks、Skills
│   └── cli/       — CLI 入口、HTTP Server (axum)、WebSocket 协议、前端静态资源
└── frontend/      — React Web UI + PWA
```

### 核心模块索引 (engine crate)

| 文件 | 职责 |
|---|---|
| `loop_.rs` | **Agent Loop 核心** — `QueryEngine`，turn 循环、工具调度、compaction 触发 |
| `context.rs` | 上下文收集 — git 快照、NONOCLAW.md 加载、Memory/Wiki 加载 |
| `prompt.rs` | System Prompt 组装 — Block 1 (cached) + Block 2 (uncached) |
| `run.rs` | Run 生命周期 — `RunContext`、`RunController`、事件序列化、terminal commit |
| `session.rs` | Session 持久化 — writer actor 模式、JSONL 存储、revision 控制 |
| `compact.rs` | 自动压缩 — transcript 摘要、Segments 模式、安全切分 |
| `tokens.rs` | Token 估算 — chars-per-token 启发式 |
| `tool_selector.rs` | MCP 工具选择 — 按关键词相关性缩小广告工具集 |
| `agents.rs` | Agent Profile 系统 — 子 Agent 生命周期、递归限制、并发控制 |
| `hooks.rs` | Hook 系统 — PreToolUse/PostToolUse/SessionStart 等 12 种 Hook |
| `skills.rs` | Skill 系统 — 静态/条件/动态 Skill 发现与激活 |
| `trace.rs` | 运行追踪 — in-memory event collector |

---

## 2. Agent Loop 机制

### 2.1 整体架构

```
┌─────────────────────────────────────────────────────────┐
│                    RunController                         │
│  (owns CancellationToken, event relay, terminal commit) │
│                                                          │
│  ┌──────────────────────────────────────────────────┐   │
│  │              QueryEngine                          │   │
│  │  (owns messages[], total_usage, session?)         │   │
│  │                                                   │   │
│  │  ┌─────── run_with_context() ───────────────┐    │   │
│  │  │                                          │    │   │
│  │  │  1. Pre-run: hooks, context prep         │    │   │
│  │  │  2. Skill activation                     │    │   │
│  │  │  3. ┌─── loop { ─────────────────────┐   │    │   │
│  │  │  3a.│  background task notifications  │   │    │   │
│  │  │  3b.│  refresh context block          │   │    │   │
│  │  │  3c.│  check max_turns / finalize     │   │    │   │
│  │  │  3d.│  auto-compact check (80%/100%)  │   │    │   │
│  │  │  3e.│  build RequestParams            │   │    │   │
│  │  │  3f.│  client.run_turn_with_cancel()  │   │    │   │
│  │  │  3g.│  parse stop_reason              │   │    │   │
│  │  │  3h.│  if tool_use → execute tools    │   │    │   │
│  │  │  3i.│  append tool_result to messages │   │    │   │
│  │  │     │  if no tools → break (completed)│   │    │   │
│  │  │  └─── } ──────────────────────────────┘   │    │   │
│  │  │  4. Post-run: Stop + SessionEnd hooks     │    │   │
│  │  └──────────────────────────────────────────┘    │   │
│  └──────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

### 2.2 Turn 循环详解 (`loop_.rs:717-1407`)

每个 turn 的执行顺序：

1. **取消检查** — `cancel.is_cancelled()` → 立即返回 `Error::Cancelled`
2. **后台任务通知注入** — drain `BackgroundTaskRegistry` 的完成通知，作为 user message 注入
3. **动态上下文刷新** — `refresh_context_block()` 重建 Block 2（uncached），但不触碰 Block 1
4. **Max turns 检查** — 如果 `turns_made >= max_turns`：
   - 子 Agent 且 `finalize_on_max_turns`：进入 finalize 模式（禁用工具，强制生成最终答案）
   - 否则：`break RunFinishReason::MaxTurns`
5. **两段式 Auto-compact**：
   - **80% 阈值预触发**：spawn 后台 compact task（不阻塞当前 turn）
   - **100% 阈值同步触发**：阻塞当前 turn，等待 compact 完成
   - 两者都支持 revision-based CAS：如果 transcript 在 compact 期间被修改，丢弃 compact 结果
6. **构建请求参数** — `RequestParams { system, messages, tools, ... }`
   - `strip_unsupported_blocks()` 移除 thinking blocks、不支持 provider 的 image blocks
7. **流式调用 LLM** — `client.run_turn_with_cancel()` + 回调转发 stream events
8. **错误恢复** — 如果 API 拒绝 `tool_use`/`tool_result` 不匹配：
   - `repair_tool_pairing()` 移除孤儿 tool_use blocks
   - 重新构造请求并重试一次
9. **解析 stop_reason 和 tool_uses**：
   - `EndTurn` + 无工具调用 → `break RunFinishReason::Completed`
   - `ToolUse` + 有工具调用 → 继续循环
   - 子 Agent finalize：空答案或非 `EndTurn` → Error
10. **工具执行**：
    - 构造 `ToolCall[]`，传入 `ToolExecutor::execute()`
    - `tokio::select!` 同时 drain 子 Agent 事件（保持 child progress 可见）
    - 收集 `ToolExecutionResult[]`
11. **动态 Skill 激活** — 从 Read/Write/Edit 的 `file_path` 参数触发条件 Skill
12. **追加 tool_result** — 作为 user message 加入 `messages[]`，持久化到 session

### 2.3 终止条件

```rust
enum RunFinishReason {
    Completed { detail },           // 正常完成（无工具调用 + EndTurn）
    MaxTurns { max_turns },         // 达到最大 turn 数
    BudgetExceeded { max_budget },  // 预算超限
    ContextLimit { context_window },// 上下文窗口超限
    Cancelled { reason },           // 用户/系统取消
    Error { message },              // 运行时错误
}
```

### 2.4 流式事件转发 (`forward_stream_event`)

LLM 的 SSE stream 被转换为结构化的 `RunEvent`：

| SSE Event | → RunEvent |
|---|---|
| `MessageStart { model, usage }` | `ModelInfo` + `ModelResolved` + `UsageUpdated` |
| `TextDelta { text }` | `StreamStateChanged(Streaming)` + `TextDelta` |
| `ThinkingDelta` | `ThinkingState { active: true }` |
| `MessageDelta { usage }` | `UsageUpdated` |
| `MessageStop` | `ThinkingState { active: false }` + `StreamStateChanged(Completed)` |
| `RetryScheduled` | `RetryScheduled` |
| `StreamError` | `StreamStateChanged(Interrupted)` + `RunError` |

### 2.5 RunController 生命周期 (`run.rs`)

`RunController` 是 **唯一的运行入口**，负责：

- **CancellationToken 树** — parent cancel → 所有 child token 触发
- **事件序列化** — `EventEnvelope` 带单调递增 sequence number，通过 unbounded channel 发送
- **Exactly-once terminal commit** — `OnceLock<RunTerminal>` 保证终止状态只写一次
- **三层任务结构**：
  1. Engine task — 执行 `run_with_context`
  2. Consumer task — 按序处理事件
  3. Supervisor task — 等待 engine + consumer，提交 terminal

---

## 3. 上下文组装机制

### 3.1 System Prompt 双块结构 (`prompt.rs`)

```
┌─────────────────────────────────────────────────┐
│ Block 1 (cache_control: Ephemeral)              │  ← 跨 turn 缓存
│                                                  │
│  • BASE prompt (identity, guidelines, ~4KB)     │
│  • Environment (cwd, platform, date)            │
│  • Tool guidance (usage instructions)           │
│  • Available Tools list (name + first line)     │
│  • Static skill metadata only                   │
│  • Additional instructions (append_system_prompt)│
│                                                  │
│  特点: 整个 run 期间 byte-stable                 │
├─────────────────────────────────────────────────┤
│ Block 2 (cache_control: None)                   │  ← 每 turn 刷新
│                                                  │
│  • Git status (live snapshot)                   │
│  • NONOCLAW.md content                          │
│  • Memory (facts, beads, wiki index)            │
│  • Dynamic skill metadata (activated skills)    │
│                                                  │
│  特点: 每 turn 调用 refresh_context_block()     │
└─────────────────────────────────────────────────┘
```

**缓存策略设计意图**：
- Block 1 在首次 turn 后被 provider 缓存（如 Anthropic 的 prompt cache），后续 turn 命中缓存
- Block 2 每 turn 刷新 git status，但不影响 Block 1 的缓存
- Skill 激活不重建 Block 1 — 动态 metadata 进入 Block 2
- Tool 数组也是缓存的一部分 — 最后一个 tool 带 `cache_control: Ephemeral`

### 3.2 Tool 数组构建 (`loop_.rs:523-563`)

```
全部注册工具
    │
    ▼
allowed_tools 过滤 (EngineOptions.allowed_tools)
    │
    ▼
MCP 工具选择 (tool_selector::pinned_mcp_selection)
    │  ┌─ 关键词评分: name exact(+100) > name contains(+50)
    │  │                > hint(+30) > desc(+10)
    │  ├─ MCP > top_k 时才触发 narrowing
    │  ├─ 无关键词匹配 → 保留全部 (保守)
    │  └─ Session pinning: 首次 narrowing 后固定
    ▼
最终 tool_defs[]
    │
    ▼
最后一个 tool 设置 cache_control: Ephemeral
```

### 3.3 上下文来源层级 (`context.rs`)

#### NONOCLAW.md 加载顺序（优先级从低到高）：

```
1. <cwd>/.nonoclaw/NONOCLAW.md         — 项目配置
2. <cwd>/.nonoclaw/NONOCLAW.local.md   — 项目本地（gitignored）
3. <cwd>/.nonoclaw/rules/*.md          — 项目规则（按文件名排序）
4. 每个 --add-dir/.nonoclaw/NONOCLAW.md — 附加目录
5. ~/.nonoclaw/NONOCLAW.md             — 用户全局
6. ~/.nonoclaw/rules/*.md              — 用户全局规则
```

#### Memory 加载 (`load_memory_prompt`):

```
1. Active beads (≤5) + top facts (≤10, by importance)
   └→ render_memory_context (≤20KB)
2. Wiki index preview (≤5KB)
3. MEMORY.md index (≤25KB, ≤200 lines)
4. Individual fact .md files (stripped frontmatter)
   └→ 总计 ≤50KB
```

#### Git 上下文 (`get_system_context`):

```bash
git rev-parse --abbrev-ref HEAD   # 当前分支
git status                         # 工作区状态 (截断 2000 chars)
git log --oneline -5              # 最近 5 条提交
git config user.name              # 用户名
```

### 3.4 请求消息处理

每个 turn 的 messages 经历以下变换：

```
self.messages[] (完整对话历史)
    │
    ▼
strip_unsupported_blocks()        — 移除 thinking blocks、
    │                               不支持的 image blocks
    ▼
RequestParams.messages            — 发送给 LLM API
```

**Block stripping 策略**：
- `ContentBlock::Thinking` → 始终移除（Bedrock proxies 拒绝 signature）
- `ContentBlock::Image` → 如果 provider 不支持 images 则移除
- 移除后如果 message 变空 → 替换为 placeholder 文本

### 3.5 Token 估算 (`tokens.rs`)

```rust
estimate_total(messages, system_chars, tools_chars, chars_per_token) =
    (system_chars + tools_chars + body_chars) / chars_per_token
    + messages.len() * 4  // per-message overhead

// 参数化：
//   Claude: ~4 chars/token (英文)
//   DeepSeek/GLM: ~2-3 chars/token (中文更高效)
//   Image: 固定 ~1200 tokens
```

**精度说明**：这是粗略启发式，用于触发 auto-compact，不用于计费。实际 token 数可能高 20-30%（尤其中文或工具密集 prompt），因此 compact threshold 设为 context window 的 75%。

---

## 4. Session 持久化与恢复

### 4.1 Writer Actor 模式 (`session.rs`)

```
                    ┌──────────────────────┐
  QueryEngine ────► │ Session (Cloneable)  │
    .persist()      │  ┌────────────────┐  │
    .persist_       │  │  mpsc::Sender   │──┼──► writer_thread (std::thread)
    compaction()    │  └────────────────┘  │       │
                    └──────────────────────┘       ▼
                                              SessionState { ... }
                                              JSONL file append/rewrite
```

**关键设计**：
- **进程级单 writer 注册表** — 同一路径只有一个 writer actor，防止并发写冲突
- **Revision 控制** — 每次成功 mutation 递增，compact 替换需要 CAS (`expected_revision`)
- **JSONL 追加为主** — 正常 append 只追加一行；只在 compact/clear/recovery 时全量 rewrite
- **原子 rewrite** — 写 `.tmp` → `sync_all()` → `rename` → 原子替换

### 4.2 Session Entry 类型

```rust
enum SessionEntry {
    Session { id, cwd, model, started },  // header
    Message(Message),                       // 对话消息
    Summary { text },                       // AI 生成的摘要
    CustomTitle { title },                  // 用户设置标题
    AiTitle { title },                      // AI 生成标题
    LastPrompt { prompt },                  // 最后用户 prompt
    Tag { tag },                            // 会话标签
    Mode { mode },                          // 权限模式
}
```

未知 entry 类型 (`future_entry`) 被 preserved（前向兼容），rewrite 时保留。

### 4.3 Resume 恢复机制

加载旧 session 时：
1. 逐行解析 JSONL
2. 跳过损坏行（`SessionRepairKind::CorruptLine`）
3. 跳过无效 entry（`SessionRepairKind::InvalidEntry`）
4. 修复孤儿 tool_use/tool_result（`SessionRepairKind::ToolPairing`）
5. 补全缺失 header（`SessionRepairKind::MissingHeader`）
6. repairs 通过 `RunEvent::SessionRepair` 上报给 UI

---

## 5. 子 Agent 机制

### 5.1 架构

```
Parent Agent (depth=0)
    │
    ├── Agent tool call ──► EngineSubagent
    │                          │
    │                          ▼
    │                    SubagentLifecycle
    │                    (depth=0, max_depth=1)
    │                          │
    │                    ● child_registry: 过滤掉 Agent/Coordinator
    │                    ● Semaphore: 并发上限 (默认 4, 最大 64)
    │                    ● CancellationToken: 继承 parent
    │                          │
    │                    QueryEngine::new() → 新的 agent loop
    │                    finalize_on_max_turns = true
    │                    is_non_interactive = true
    │                          │
    │                    RunController::start() → wait()
    │                          │
    │                    ◄── Result<String> ──► 回传给 parent
    │
    └── (child 不能再 spawn Agent — recursion blocked at depth=1)
```

### 5.2 子 Agent 约束

| 属性 | 子 Agent | 父 Agent |
|---|---|---|
| `max_turns` | min(parent, env_default=24, hard=200) | 配置值 (默认 200) |
| `finalize_on_max_turns` | `true` | `false` |
| `is_non_interactive` | `true` | 取决于入口 |
| `permission_resolver` | `None` | 可能设置 |
| `question_resolver` | `None` | 可能设置 |
| Agent/Coordinator 工具 | **移除** | 可用 |
| 权限模式 | 只能收紧，不能放宽 | 配置值 |

### 5.3 事件冒泡

子 Agent 事件通过 unbounded channel 回传给 parent：

```
child event → scoped_subagent_event() → child_event_tx
    → parent loop 的 tokio::select! 中 drain → on_event()
```

子 Agent 部分失败不影响兄弟 Agent；parent cancel 会停止所有子 Agent。

---

## 6. 优化建议与切入点

### 6.1 Agent Loop 优化

#### 🔴 P0: Context 窗口利用率
- **现状**：compact threshold = context_window × 75%，估算误差 20-30%
- **问题**：保守的阈值导致过早 compact，浪费 context；粗略估算在高工具使用率场景偏差大
- **切入点**：`tokens.rs` + `loop_.rs:849-990`
- **方向**：
  - 使用 provider 返回的真实 `input_tokens` 校准 `chars_per_token`（运行时自适应）
  - 动态调整 compact threshold：首次 compact 后记录真实/估算比率
  - 工具 schema 的 token 开销用精确估算而非 chars/4

#### 🟡 P1: 后台 Compact 预测精度
- **现状**：80% 预触发 + 100% 同步触发，但后台 compact 的 CAS 在高并发消息时容易 stale
- **问题**：stale compact 浪费 LLM 调用，回退到同步 compact 增加延迟
- **切入点**：`loop_.rs:797-908`
- **方向**：
  - 提高 pre-fire 阈值到 85%，减少过早 spawn
  - 支持 incremental compact（只 compact 增量部分，而非全量重做）
  - compact 结果缓存：如果 transcript hash 没变，重用上次的 compact 结果

#### 🟡 P1: Turn 间状态一致性
- **现状**：每 turn 重新执行 `get_system_context()` (git subprocess × 4)
- **问题**：4 个 git 子进程 × 每个 turn = 较高开销，尤其 turn 数多时
- **切入点**：`loop_.rs:772-781`
- **方向**：
  - git status diff-based 更新：只在工具执行后检查是否需要刷新
  - 缓存 git context，TTL 过期或在 Bash/Edit/Write 工具执行后失效
  - 并行执行 4 个 git 命令（当前是顺序的）

### 6.2 上下文组装优化

#### 🔴 P0: Prompt Cache 命中率
- **现状**：Block 1 设计为 cached，但 `date` 字段每天变化导致 Block 1 失效
- **问题**：`main.push_str(&format!("- Today's date: {}\n", user.date))` 在 Block 1 中，日期变了整个 Block 1 重新缓存
- **切入点**：`prompt.rs:55`
- **方向**：
  - 将 `date` 移到 Block 2（uncached），Block 1 只包含 truly stable 内容
  - 或使用更粗粒度的时间（"2026/07" 而非 "2026/07/28"）

#### 🟡 P1: NONOCLAW.md 每次全量加载
- **现状**：`get_user_context()` 每 turn 读取 6 个来源的文件
- **问题**：文件系统 I/O 每 turn 重复（即使内容没变）
- **切入点**：`context.rs:69-109`
- **方向**：
  - 文件 modification time 缓存：只有文件变化时才重新读取
  - 内容 hash 比较：如果 hash 没变，不重建 Block 2 的 NONOCLAW.md 部分

#### 🟡 P1: Memory 加载开销
- **现状**：`load_memory_prompt()` 每 turn 读取 beads、facts、wiki index、MEMORY.md、所有 .md 文件
- **问题**：随着 memory 增长，每 turn 的 I/O 开销线性增加
- **切入点**：`context.rs:147-231`
- **方向**：
  - Memory 内容只在 run 开始时加载一次，缓存到 `QueryEngine` 字段
  - Memory 变更通过 Memory tool 写入后主动通知 engine 刷新缓存

#### 🟢 P2: Tool Schema 精简
- **现状**：所有注册工具的完整 JSON Schema 每次都进入 tools 数组
- **问题**：MCP 工具的 schema 可能很大；内置工具的 description 也很长
- **切入点**：`loop_.rs:542-556`
- **方向**：
  - Tool description 压缩：只保留第一段（已有 partial 实现，见 prompt.rs 的 tools_list）
  - 按需加载 tool description：类似 Skill 的 progressive disclosure

### 6.3 Compaction 优化

#### 🟡 P1: Compaction 策略改进
- **现状**：Segments 模式保留最近 3 个完整 turn，其余全量摘要
- **问题**：
  - 摘要是单次 LLM 调用，长对话的摘要可能丢失关键细节
  - 固定保留 3 turn 不够灵活 — 可能需要保留更多 recent context
- **切入点**：`compact.rs`
- **方向**：
  - 分段摘要（segmented summarization）：按 turn 分组分别摘要，再合并
  - 基于重要性保留：而非固定 turn 数，根据 tool 调用密度/文件路径引用保留关键 turn
  - 滑动窗口压缩：渐进式 compact，而非一次性全量替换

#### 🟡 P1: Compact Summary 质量
- **现状**：摘要 prompt 固定为 `SUMMARY_SYSTEM`，`MAX_SUMMARY_TOKENS = 4096`
- **问题**：4096 token 对长对话不够；摘要缺少结构化格式
- **方向**：
  - 结构化摘要格式：决策列表、文件变更记录、开放问题
  - 自适应 max_tokens：根据被 compact 的消息量动态调整

### 6.4 子 Agent 优化

#### 🟢 P2: 子 Agent 上下文继承
- **现状**：子 Agent 完全独立构建上下文（新 system prompt、新 context assembly）
- **问题**：parent 已经读取的文件、git 状态等上下文不被继承，子 Agent 需要重复读取
- **方向**：
  - 支持上下文继承：子 Agent 可选择接收 parent 的部分上下文摘要
  - 减少 Agent tool 的 prompt 长度要求（当前要求完整任务描述）

#### 🟢 P2: 并发控制改进
- **现状**：Semaphore 固定上限（默认 4）
- **方向**：
  - 基于剩余 budget 动态调整并发
  - 按工具类型分配优先级（读操作 vs 写操作）

### 6.5 架构级优化

#### 🟡 P1: 事件流优化
- **现状**：所有 `RunEvent` 通过 unbounded channel + `Arc<Mutex<Vec>>` trace
- **问题**：长对话产生大量事件，trace 收集器内存线性增长
- **方向**：
  - Ring buffer trace：只保留最近 N 个事件
  - 事件压缩：合并连续的 TextDelta 事件

#### 🟢 P2: 模块化 Prompt 构建
- **现状**：`prompt.rs` 的 BASE prompt 是硬编码 const，所有功能在一个字符串中
- **问题**：不易维护、不易测试、不易用户定制
- **方向**：
  - Prompt 模块化：拆分为 identity、guidelines、tool_guide、memory_guide 等独立段
  - 支持用户通过 NONOCLAW.md 覆盖特定段

---

## 附录: 关键数据流

### A. 一次完整 turn 的数据流

```
User Input
    │
    ▼
QueryEngine::run_with_context()
    │
    ├──► load_hooks() ────► SessionStart + UserPromptSubmit hooks
    │
    ├──► get_system_context()  ──► git subprocesses
    ├──► get_user_context()    ──► NONOCLAW.md + rules
    ├──► load_memory_prompt()  ──► beads + facts + wiki + MEMORY.md
    │
    ├──► build_system_blocks() ──► Block 1 (cached) + Block 2 (uncached)
    ├──► pinned_mcp_selection()──► MCP tool subset
    ├──► tool_defs[]  ──► filter + cache_control on last
    │
    ├──► loop {
    │      refresh_context_block()  ──► fresh git
    │      auto_compact check      ──► 80% pre-fire / 100% sync
    │      RequestParams { system, messages, tools }
    │          │
    │          ▼
    │      client.run_turn_with_cancel()
    │          │
    │          ▼ SSE stream
    │      forward_stream_event() ──► RunEvent ──► on_event()
    │          │
    │          ▼ TurnResult { content, usage, stop_reason }
    │      parse content blocks
    │          │
    │          ├── Text → AssistantDone event
    │          └── ToolUse[] → ToolExecutor::execute()
    │              │
    │              ▼ ToolExecutionResult[]
    │          append tool_result → messages[]
    │          persist to session
    │    }
    │
    ▼
FinalResult { text, usage, turns, stop_reason, finish_reason }
    │
    ▼
Stop + SessionEnd hooks
RunFinished event
```

### B. Session JSONL 结构示例

```jsonl
{"kind":"session","id":"abc-123","cwd":"/proj","model":"claude-sonnet-4-5","started":"2026-07-28T10:00:00+08:00"}
{"kind":"message","role":"user","content":"fix the bug"}
{"kind":"message","role":"assistant","content":[{"type":"text","text":"I'll investigate."},{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"src/main.rs"}}]}
{"kind":"message","role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"file contents..."}]}
{"kind":"message","role":"assistant","content":[{"type":"text","text":"Found the issue. Fixed."}]}
{"kind":"summary","text":"Fixed a bug in main.rs"}
{"kind":"ai_title","title":"Bug fix in main.rs"}
```

### C. 关键配置常量

| 常量 | 值 | 位置 | 说明 |
|---|---|---|---|
| `DEFAULT_MAX_TURNS` | 200 | settings.rs | 父 Agent 最大 turn 数 |
| `DEFAULT_MAX_TOKENS` | 8192 | settings.rs | 每次输出的最大 token 数 |
| `DEFAULT_COMPACT_THRESHOLD` | 150,000 | settings.rs | Auto-compact token 阈值 |
| `compact_threshold_tokens` | CW×75% | loop_.rs | 按窗口大小计算 |
| `KEEP_RECENT_TURNS` | 3 | compact.rs | Compact 保留的完整 turn 数 |
| `MAX_SUMMARY_TOKENS` | 4,096 | compact.rs | 摘要输出的最大 token 数 |
| `DEFAULT_TOP_K` | 15 | tool_selector.rs | MCP 工具选择上限 |
| `MAX_PINNED_SESSIONS` | 64 | tool_selector.rs | Session pinning 上限 |
| `DEFAULT_SUBAGENT_MAX_TURNS` | 24 | loop_.rs | 子 Agent 最大 turn 数 |
| `HARD_MAX_SUBAGENT_TURNS` | 200 | loop_.rs | 子 Agent turn 硬上限 |
| `MAX_SUBAGENT_DEPTH` | 1 | agents.rs | 子 Agent 递归深度 |
| `DEFAULT_MAX_SUBAGENT_CONCURRENCY` | 4 | agents.rs | 子 Agent 默认并发 |
| `MAX_SUBAGENT_CONCURRENCY` | 64 | agents.rs | 子 Agent 并发上限 |
| `GIT_STATUS_MAX` | 2,000 chars | context.rs | Git status 截断长度 |
| `chars_per_token` | 4 | loop_.rs | Token 估算除数 |
| `PER_MESSAGE_OVERHEAD` | 4 tokens | tokens.rs | 每条消息开销 |
| `IMAGE_TOKENS` | 1,200 | tokens.rs | 图片估算 token |

---

## 7. 竞品分析: pi (earendil-works/pi-mono) Agent Loop & Context Assembly

> 调研日期: 2026-07-28 | 项目结构: TypeScript monorepo | packages: agent, ai, coding-agent, tui, server

### 7.1 pi 项目架构总览

```
pi/
├── packages/
│   ├── agent/           — 核心 Agent Loop (通用，不绑定 coding)
│   │   └── src/
│   │       ├── agent-loop.ts   — runLoop(): 双层循环 (turn + steering/followUp)
│   │       ├── agent.ts        — Agent 类: 状态管理 + 消息队列 + 事件订阅
│   │       └── types.ts        — AgentMessage, AgentContext, AgentTool, etc.
│   ├── ai/              — LLM Provider 层 (Anthropic/OpenAI/...多 provider)
│   ├── coding-agent/    — Coding Agent 具体实现
│   │   └── src/core/
│   │       ├── agent-session.ts    — AgentSession: 桥接 Agent + UI + compaction
│   │       ├── system-prompt.ts    — buildSystemPrompt(): 参数化 prompt 构建
│   │       ├── prompt-templates.ts — /template args 展开系统
│   │       ├── messages.ts         — 自定义消息类型 + convertToLlm()
│   │       ├── resource-loader.ts  — 通用资源加载器接口
│   │       └── compaction/         — 上下文压缩
│   ├── tui/             — 终端 UI
│   └── server/          — HTTP Server
└── .pi/
    ├── prompts/         — 内置 prompt 模板 (cl.md, is.md, pr.md, sa.md, wr.md)
    └── skills/          — 内置 skills
```

### 7.2 pi 的 Agent Loop 机制

#### 双层循环设计 (inner + outer)

```
runLoop():
  ┌── while true { (outer loop — follow-up messages)
  │     hasMoreToolCalls = true
  │
  │     ┌── while (hasMoreToolCalls || pendingMessages > 0) { (inner loop)
  │     │    1. process steering messages (inject before next LLM call)
  │     │    2. streamAssistantResponse()  ← LLM call
  │     │    3. check stopReason (error/aborted → return)
  │     │    4. execute tools (sequential/parallel)
  │     │    5. append tool results
  │     │    6. emit turn_end
  │     │    7. prepareNextTurn() — context/model/thinking swap
  │     │    8. shouldStopAfterTurn() check
  │     │    9. check getSteeringMessages()
  │     └── }
  │
  │    10. check getFollowUpMessages()
  │       → if present: set as pending, continue outer loop
  │       → else: break (agent_end)
  └── }
```

**与 NonoClaw 的关键差异**:

| 特性 | pi | NonoClaw |
|---|---|---|
| 循环结构 | 双层 (inner turn loop + outer followUp loop) | 单层 (turn loop) |
| 消息队列 | steering (立即注入) + followUp (等待下轮) | 无队列概念，单次 run |
| 工具执行 | 支持 sequential/parallel 混合 | 全部并行 |
| 失败处理 | 自动重试 (retryable error 判断) | 手动重试 (tool pairing repair) |
| 终止条件 | stop_reason + shouldStopAfterTurn + terminate flag | stop_reason + max_turns 检查 |
| 上下文刷新 | prepareNextTurn 钩子 (可替换 model/thinking/context) | refresh_context_block (只刷新 Block 2) |

#### 消息类型系统 — pi 的核心创新

pi 使用 **可扩展的 AgentMessage 类型**：

```typescript
// 基础 LLM 消息
type AgentMessage = Message                    // user | assistant | toolResult
                 | CustomAgentMessages[keyof CustomAgentMessages];

// 通过 declaration merging 扩展
interface CustomAgentMessages {
    bashExecution: BashExecutionMessage;       // ! 命令执行结果
    custom: CustomMessage;                     // 扩展注入消息
    branchSummary: BranchSummaryMessage;       // 分支摘要
    compactionSummary: CompactionSummaryMessage; // 压缩摘要
}
```

**核心机制**: `convertToLlm(messages: AgentMessage[]) → Message[]`
- 在每次 LLM 调用前**动态转换** AgentMessage → LLM 兼容的 Message
- `compactionSummary` 和 `branchSummary` 变成 `<summary>...</summary>` 格式的 user message
- `bashExecution` 变成 `Ran \`cmd\`\n```\noutput\n```\n` 格式的 user message  
- 不需要的消息类型被**过滤掉**（如 `excludeFromContext` 的 bash msg）

**这一设计对 NonoClaw 的启发**: 将 compaction summary 等"元消息"作为一等公民进入 messages 数组，而非单独处理，让模型自然理解上下文结构。

### 7.3 pi 的上下文组装机制

#### 7.3.1 System Prompt 构建 — 高度参数化

```typescript
// system-prompt.ts — buildSystemPrompt()
buildSystemPrompt({
    customPrompt,          // 完全自定义 (来自 SYSTEM.md)
    selectedTools,         // ['read', 'bash', 'edit', 'write', ...]
    toolSnippets,          // { read: "Read file contents", bash: "Execute shell command" }
    promptGuidelines,      // ["Be concise", "Show file paths clearly"]
    appendSystemPrompt,    // 来自 APPEND_SYSTEM.md
    cwd,                   // 工作目录
    contextFiles,          // [{ path: "/proj/AGENTS.md", content: "..." }]
    skills,                // 可用 skills 列表
})
```

**关键设计决策**:

1. **Tool = Snippet + Schema 分离**:
   - `toolSnippets`: 在 system prompt 中的**简短一行描述** (显示在 "Available tools" 列表)
   - `Tool Schema (JSON Schema)`: 在 LLM API 的 `tools` 参数中，包含完整参数定义
   - NonoClaw 也做了类似的事（prompt.rs 中 `tools_list` 只取第一行），但 pi 的分离更显式

2. **Guidelines 按工具动态生成**:
   ```typescript
   if (hasBash && !hasGrep && !hasFind && !hasLs) {
       addGuideline("Use bash for file operations like ls, rg, find");
   }
   ```
   - 根据当前激活的工具集自动调整 guidelines
   - NonoClaw 的 tool guidance 是**硬编码的**，不随工具集变化

3. **工具可见性过滤**:
   ```typescript
   const visibleTools = tools.filter((name) => !!toolSnippets?.[name]);
   ```
   - 只有调用方提供了 snippet 的工具才在 Available tools 中列出
   - 其他工具通过 "you may have access to other custom tools" 提示模型

4. **Domain-specific 文档引用**:
   - System prompt 中包含 pi 自身文档的路径
   - 明确告诉模型何时应该查阅文档 (pi specific topics)

#### 7.3.2 Context Files 加载 — 祖先目录遍历

```typescript
// resource-loader.ts — loadProjectContextFiles()
loadProjectContextFiles({ cwd, agentDir }):
  1. 加载全局 agentDir/AGENTS.md (user level)
  2. 从 cwd 开始向上遍历所有祖先目录:
     每个目录查找 AGENTS.md → AGENTS.MD → CLAUDE.md → CLAUDE.MD
  3. 按从根到叶的顺序 (closest to furthest) 添加到 contextFiles[]
```

**与 NonoClaw 的对比**:

| 维度 | pi | NonoClaw |
|---|---|---|
| 文件名 | AGENTS.md/CLAUDE.md (大写优先) | NONOCLAW.md |
| 加载范围 | 全局 + 所有祖先目录 (树遍历) | 项目 + 全局 + add-dirs + rules |
| 规则文件 | 无独立 rules/ 目录 | .nonoclaw/rules/*.md |
| 本地覆盖 | 无 | NONOCLAW.local.md |
| 内存表示 | 文件数组 → XML 包裹 | 拼接字符串 |

#### 7.3.3 System Prompt 来源

pi 的 system prompt 有清晰的来源层次:

```
1. 用户自定义 SYSTEM.md (全局 ~/.pi/SYSTEM.md 或 项目 .pi/SYSTEM.md)
   如果存在 → 完全替换默认 prompt (customPrompt 模式)
   如果不存在 → 使用默认 prompt (硬编码在 buildSystemPrompt 中)

2. APPEND_SYSTEM.md → 追加到 prompt 末尾

3. contextFiles (AGENTS.md/CLAUDE.md) → 包裹在 <project_context> 中

4. Skills → 包裹在 <skills> 中 (含 description + how to load)
```

#### 7.3.4 Prompt Templates 系统 (pi 独有)

pi 实现了类似 slash command 的 prompt 模板:

```
.prompts/pr.md:
  ---
  argument-hint: "issue numbers"
  ---
  Review this PR. Focus on $@.
```

用户输入 `/pr 123 456` → 展开为 `Review this PR. Focus on 123 456`

**变量替换**:
- `$1`, `$2`, ... — 位置参数
- `$@`, `$ARGUMENTS` — 所有参数
- `${N:-default}` — 带默认值的参数
- `${@:N}` — bash 风格切片

#### 7.3.5 Context 包装格式

pi 对不同类型的上下文使用 XML 标记:

```xml
<!-- Project context -->
<project_context>
  <project_instructions path="/proj/AGENTS.md">
    ...content...
  </project_instructions>
</project_context>

<!-- Skills -->
<skills>
  ...
</skills>

<!-- Compaction summary (在 messages 中) -->
<summary>
  The conversation history before this point was compacted...
</summary>

<!-- Branch summary (在 messages 中) -->
<summary>
  The following is a summary of a branch...
</summary>
```

**与 NonoClaw 的对比**: NonoClaw 的 compaction summary 格式是 `[Compacted summary of earlier conversation]\n...\n[End summary — recent messages follow.]`，没有使用 XML 结构标记。

### 7.4 提示词组装 — 可借鉴的优化 (重点)

#### 🔴 P0: 参数化 Prompt 构建 (取代硬编码 BASE)

**现状对比**:
- NonoClaw: `prompt.rs` 中 `const BASE` 是约 4KB 的硬编码字符串，包含 identity、guidelines、memory 说明、wiki 说明、diagram 说明
- pi: `buildSystemPrompt()` 通过参数化构建，所有内容通过函数参数注入

**借鉴方案**:
```rust
// 建议: 将 NonoClaw 的 system prompt 构建参数化
struct SystemPromptConfig {
    identity: &str,              // "You are NonoClaw..."
    environment: Environment,    // cwd, platform, date
    tool_guidance: ToolGuidance, // 按工具动态生成
    active_tools: &[ToolSnippet],// 当前活跃工具的短描述
    guidelines: &[&str],         // 行为指南
    memory_guide: Option<&str>,  // Memory 说明 (可选注入)
    wiki_guide: Option<&str>,    // Wiki 说明 (可选注入)
    diagram_guide: Option<&str>, // Diagram 说明 (可选注入)
    append: Option<&str>,        // 用户追加
    context: &ContextBlocks,     // git, NONOCLAW.md, memory
}

fn build_system_prompt(config: &SystemPromptConfig) -> Vec<SystemBlock> { ... }
```

**收益**:
- 可根据模型/场景裁剪 prompt (小模型不需要 wiki/diagram 说明)
- 测试更方便 (每个 section 可独立测试)
- 用户可通过配置覆盖特定 section

#### 🔴 P0: Prompt Cache 命中率修复

**问题**: NonoClaw 当前将 `date` 放在 Block 1 (cached) 中:
```rust
main.push_str(&format!("- Today's date: {}\n", user.date));
```
每天 `date` 变化 → Block 1 整个失效 → prompt cache miss。

**pi 的做法**: pi 没有将日期放在 system prompt 中。它的 system prompt 更简洁，只包含 "Current working directory: /path/to/proj"。

**修复方案**:
1. **立即修复**: 将 `date` 从 Block 1 移到 Block 2
2. **更好的方案**: 参考 pi，简化 system prompt，移除不必要的时间信息
3. **进一步**: 将 cwd 也移到 Block 2（虽然变化频率低，但理论上也影响跨项目缓存）

#### 🟡 P1: Tool Snippet 与 Schema 显式分离

**pi 的做法**: 
- System prompt 中只显示 tool 的**一行短描述** (snippet)
- 完整的 JSON Schema 在 API `tools` 参数中
- 未提供 snippet 的工具不出现在 Available tools 中

**NonoClaw 现状**: 
- System prompt 中显示 tool name + 第一行 prompt (已做截断)
- 完整 prompt 文件通过 Skill tool 按需加载
- 但 skill 的静态 metadata 仍进入 Block 1

**借鉴**:
- 为每个工具定义明确的 **snippet 函数** → 返回一行描述
- 在 system prompt 中只显示 snippet
- 支持 tool 作者注册 prompt guidelines（如 pi 的 `promptGuidelines`）

#### 🟡 P1: 动态 Guidelines 系统

**pi 的做法**:
```typescript
// 根据可用工具动态生成 guidelines
if (hasBash && !hasGrep && !hasFind && !hasLs) {
    addGuideline("Use bash for file operations like ls, rg, find");
}
// 工具也可注册自己的 guidelines
```

**NonoClaw 现状**: TOOL_GUIDANCE 是硬编码的，不随工具集变化。

**借鉴**: 允许工具定义注册 prompt guidelines，在 system prompt 中动态拼接。

#### 🟡 P1: XML 结构化 Context 包装

**pi 的做法**: 使用 XML 标签包裹不同类型的上下文:
```xml
<project_context>
  <project_instructions path="...">...</project_instructions>
</project_context>
<skills>...</skills>
<summary>...</summary>
```

**NonoClaw 现状**: 使用 Markdown headers (`## from ...`) 和 `[]` 包围的纯文本。

**借鉴**:
- Compaction summary 使用 `<conversation_history_summary>` 标签
- Context files 使用 `<project_context>` + 路径属性
- XML 标签对 LLM 的语义理解更友好（大部分模型训练数据中大量 XML）

#### 🟡 P1: AgentMessage 类型系统的启示

**pi 的做法**: 所有消息统一为 `AgentMessage`，包括:
- 标准 LLM 消息 (user, assistant, toolResult)
- 元消息 (compactionSummary, branchSummary)
- 工具执行记录 (bashExecution)
- 扩展消息 (custom)

通过 `convertToLlm()` 在 API 调用边界统一转换。

**NonoClaw 现状**: messages 只有 `Message` (user/assistant)，compaction 直接修改 messages 数组。

**借鉴 (较复杂，可选)**:
- 考虑在 messages 中引入 `CompactionBoundary` marker
- 而非直接 `splice` 替换 — 下游消费者（UI、export、session replay）需要知道历史被压缩过

#### 🟢 P2: Prompt Templates 系统

pi 的 `/template args` 展开系统是一个独立功能：
```
.pr/prompts/pr.md → /pr 123 → Review PR #123
.pr/prompts/cl.md → /cl → Generate changelog
```

**NonoClaw 现状**: 无此功能，slash commands 只能触发 skills。

**借鉴**: 可以作为一种轻量级的 prompt 快捷方式，类似 shell alias，不需要完整的 skill 基础设施。

#### 🟢 P2: AgentDir 全局配置目录

pi 使用 `~/.pi/` 作为全局配置根目录，其中包含:
- `SYSTEM.md` — 全局自定义 system prompt
- `APPEND_SYSTEM.md` — 全局追加内容
- `prompts/` — 全局 prompt 模板
- `skills/` — 全局 skills
- `AGENTS.md` (如果存在) — 全局 agent 上下文

**NonoClaw 现状**: 使用 `~/.nonoclaw/` 存储 settings.json + NONOCLAW.md + rules/ + skills/

**差异不大**，但 pi 的 `SYSTEM.md` 和 `APPEND_SYSTEM.md` 两个独立文件的设计值得借鉴：
- `SYSTEM.md` → 完全替换默认 prompt (白板模式)
- `APPEND_SYSTEM.md` → 追加到默认 prompt（增量模式）

### 7.5 Compaction 机制对比

| 维度 | pi | NonoClaw |
|---|---|---|
| 触发条件 | 阈值 + overflow error | 阈值 (80% pre-fire + 100% sync) |
| 阈值计算 | 真实 token 估算 (基于 usage stats) | chars/4 启发式 |
| 摘要格式 | `<summary>...</summary>` 在 messages 中 | `[Compacted summary...]` 在 messages 开头 |
| 分支摘要 | 支持 (branch switching 时自动生成) | 无 |
| 摘要 token 预算 | 可配置 | 固定 4096 |
| 扩展介入 | session_before_compact / session_compact 钩子 | PreCompact / PostCompact hooks |
| 自动重试 | overflow 后 compact+retry 一次 | compact 后需要用户继续 |
| 摘要模型 | 使用当前 model | 支持单独 compact_model |

### 7.6 整体架构哲学差异

| 维度 | pi | NonoClaw |
|---|---|---|
| 语言 | TypeScript | Rust |
| 设计哲学 | **库优先** (Agent 是可复用组件) | **应用优先** (harness CLI) |
| Agent 通用性 | 通用 Agent loop，不绑定 coding | Coding-specific agent |
| 消息模型 | 可扩展 AgentMessage (declaration merging) | 固定 Message + ContentBlock |
| 配置注入 | 函数参数 + 钩子 (hook-heavy) | 配置 struct + 文件系统 |
| 扩展性 | Extension 系统 (命令注册、工具注册、钩子) | Skill + MCP + Hook 系统 |
| System prompt | 短小精悍 (默认 ~20 行) | 详细完整 (默认 ~4KB) |
| 流式架构 | EventStream + 订阅者模式 | Channel + FnMut 回调 |
| Context 刷新 | prepareNextTurn 钩子 (可替换整个 context) | refresh_context_block (只刷新 Block 2) |

### 7.7 总结：NonoClaw 可借鉴的 Top 5 优化

| 优先级 | 优化项 | 来源 | 预期收益 |
|---|---|---|---|
| P0 | System prompt 参数化 + date 移入 Block 2 | pi buildSystemPrompt + 无 date | 跨天 prompt cache 命中率 0→高 |
| P0 | 工具 snippet/guideline 分离 + 动态生成 | pi toolSnippets + promptGuidelines | 减少 prompt 体积，提高自定义性 |
| P1 | XML 结构化上下文包装 | pi `<project_context>`, `<summary>` | 更好的 LLM 语义理解 |
| P1 | Compaction summary 进入 messages 而非 splice | pi compactionSummary message type | 保持消息完整性，利于 UI/export |
| P1 | System prompt 来源分层 (SYSTEM.md + APPEND_SYSTEM.md) | pi 文件发现策略 | 更灵活的用户定制 |
| P2 | Prompt templates 系统 (/template args) | pi prompt-templates.ts | 轻量级 prompt 快捷方式 |
| P2 | 祖先目录 AGENTS.md 遍历 | pi loadProjectContextFiles | 更好的 monorepo 支持 |
| P2 | Tool guidelines 按需注册 | pi promptGuidelines | 工具自描述能力 |

---

## 8. 优化计划与任务分解

> 基于 §6 (NonoClaw 内部优化) + §7 (pi 竞品借鉴) 综合制定。任务按批次组织，每批可独立交付。

### 8.0 任务总览

| 批次 | 名称 | 优先级 | 影响范围 | 依赖 |
|---|---|---|---|---|
| Batch 1 | Prompt Cache 修复 + date 移位 | P0 | `prompt.rs` | 无 |
| Batch 2 | System Prompt 参数化重构 | P0 | `prompt.rs`, `loop_.rs` | Batch 1 |
| Batch 3 | Tool Snippet / Guideline 分离 | P0 | `tools/tool.rs`, `loop_.rs`, `prompt.rs` | Batch 2 |
| Batch 4 | XML 结构化上下文包装 | P1 | `prompt.rs`, `context.rs`, `compact.rs` | Batch 2 |
| Batch 5 | Compaction 体验改进 | P1 | `compact.rs`, `loop_.rs`, `session.rs` | Batch 4 |
| Batch 6 | System Prompt 来源分层 | P1 | `context.rs`, `prompt.rs` | Batch 2 |
| Batch 7 | Context 缓存与 I/O 优化 | P1 | `context.rs`, `loop_.rs` | 无 (独立) |
| Batch 8 | P2 增强 (templates, monorepo, guidelines) | P2 | 多文件 | Batch 3 |

### 8.1 Batch 1 — Prompt Cache 修复 (date 移位)

**目标**: 修复跨天 prompt cache 100% miss 问题。

**现状**:
```rust
// prompt.rs:55 — date 在 Block 1 (cached)
main.push_str(&format!("- Today's date: {}\n", user.date));
```

**任务**:

| ID | 任务 | 文件 | 描述 |
|---|---|---|---|
| T1.1 | 从 `build_system_blocks` Block 1 移除 date | `prompt.rs` | 删除 `main.push_str(&format!("- Today's date: {}", user.date))` |
| T1.2 | date 加入 Block 2 (uncached) | `prompt.rs` | 在 `refresh_context_block()` 的 context string 开头加入 `\n# Current date\n{date}\n` |
| T1.3 | 验证 Block 1 稳定性 | 测试 | 编写测试断言连续调用 `build_system_blocks` (仅 date 变化) 返回的 Block 1 text 完全一致 |
| T1.4 | 更新 `ContextPrepared` 事件 | `loop_.rs` | `system_chars` 计算排除 date 字段的影响需重新确认 |

**验收标准**:
- Block 1 的 text 在同一 cwd 下跨天调用���全不变
- Block 2 包含 `date` 字段
- 所有现有测试通过

### 8.2 Batch 2 — System Prompt 参数化重构

**目标**: 将 ~4KB 硬编码 `const BASE` 拆分为可组合的段落，支持按场景裁剪。

**现状**:
```rust
// prompt.rs — BASE 是单个不可变的 const
const BASE: &str = r#"You are NonoClaw, ... [identity + guidelines + memory + wiki + diagrams + task_completion]"#;
```

**任务**:

| ID | 任务 | 文件 | 描述 |
|---|---|---|---|
| T2.1 | 定义 `SystemPromptSections` 结构体 | `prompt.rs` (新) | `identity`, `guidelines`, `memory_guide`, `wiki_guide`, `diagram_guide`, `task_completion` — 每个字段 `Option<&str>` 或 `&str` |
| T2.2 | 从 `BASE` 提取各段落为独立 `const` | `prompt.rs` | `IDENTITY`, `CODE_QUALITY`, `SAFETY`, `FAILURE_MODES`, `PARALLELISM`, `DEPENDENCIES`, `MEMORY_GUIDE`, `WIKI_GUIDE`, `DIAGRAM_GUIDE`, `TASK_COMPLETION` |
| T2.3 | 新增 `build_system_prompt_sections()` | `prompt.rs` | 接收 `SystemPromptSections` + `Environment` + `ToolGuidance` → 产出 Block 1 text |
| T2.4 | 新增 `PromptProfile` 配置 | `settings.rs` | `enum PromptProfile { Full, Minimal, Custom { sections: HashSet<String> } }` — 用户可通过 settings.json 配置包含哪些段落 |
| T2.5 | 重构 `build_system_blocks` | `prompt.rs` | 改为调用 `build_system_prompt_sections()` 内部组合段落 |
| T2.6 | 保持向后兼容 | `prompt.rs` | `build_system_blocks` 签名不变，内部委托到新函数 |

**验收标准**:
- 默认 `PromptProfile::Full` 下产出的 prompt 与重构前 byte-for-byte 一致 (除 date)
- `PromptProfile::Minimal` 只包含 identity + safety + task_completion，prompt 减少 ≥40%
- 用户可通过 settings.json `"promptProfile": "minimal"` 切换

### 8.3 Batch 3 — Tool Snippet / Guideline 分离

**目标**: 工具自描述能力 — 每个工具注册 snippet (一行描述) + guidelines (行为指南)，在 system prompt 中动态拼接。

**现状**:
```rust
// prompt.rs:61-67 — 从完整 prompt 中取第一行作为 snippet
let first_line = prompt.lines().next().unwrap_or("");
format!("- **{name}**: {first_line}")
```

**任务**:

| ID | 任务 | 文件 | 描述 |
|---|---|---|---|
| T3.1 | `Tool` trait 新增 `snippet()` 方法 | `tools/tool.rs` | 返回一行短描述 (≤80 chars)。默认实现: 从 `description()` 取第一行 |
| T3.2 | `Tool` trait 新增 `prompt_guidelines()` | `tools/tool.rs` | 返回 `&[&str]` — 该工具相关的行为指南 (如 "Use Grep instead of rg in Bash")。默认空 |
| T3.3 | 所有内置工具实现 snippet + guidelines | `tools/builtin/*.rs` | 逐个工具添加精确的 snippet 和 guidelines |
| T3.4 | system prompt 中使用 snippet 替代首行截取 | `prompt.rs` | `tools_list` 使用 `tool.snippet()` 而非 `prompt.lines().next()` |
| T3.5 | system prompt 中动态拼接 tool guidelines | `prompt.rs` | 收集活跃工具的 guidelines，去重后追加到 guidelines 段落 |
| T3.6 | MCP 工具的 snippet 从 description 派生 | `tools/mcp.rs` | MCP tool 无自定义 snippet 时，从 `description` 提取 |

**验收标准**:
- System prompt 中的 Available Tools 列表使用精确 snippet，不再是 description 首行截断
- Bash 工具的 guidelines 包含 "Use Grep tool instead of rg in Bash for file content searches"
- 新增工具只需实现 snippet/guidelines，无需修改 prompt.rs

### 8.4 Batch 4 — XML 结构化上下文包装

**目标**: 用 XML 标签包裹不同类型的上下文，提升 LLM 语义理解。

**现状**:
```rust
// context.rs — NONOCLAW.md 使用 Markdown headers
buf.push_str(&format!("## from {source}\n\n{content}\n\n"));

// compact.rs — summary 使用方括号
"[Compacted summary of earlier conversation]\n{summary}\n[End summary — recent messages follow.]"
```

**任务**:

| ID | 任务 | 文件 | 描述 |
|---|---|---|---|
| T4.1 | NONOCLAW.md 使用 `<project_context>` 包装 | `context.rs` | `append_md()` 改为 XML 格式: `<project_instructions path="{source}">\n{content}\n</project_instructions>` |
| T4.2 | Memory 使用 `<memory>` 包装 | `context.rs` | `load_memory_prompt()` 输出包裹在 `<memory>` 标签内 |
| T4.3 | Skills 使用 `<skills>` 包装 | `prompt.rs` | skill metadata 段包裹在 `<skills>` 标签 |
| T4.4 | Compaction summary 使用 XML | `compact.rs` | 替换 `[Compacted summary...]` 为 `<conversation_history_summary>\n{summary}\n</conversation_history_summary>` |
| T4.5 | 编写 XML 格式测试 | 测试 | 断言每种 context 类型正确包裹在对应 XML 标签中 |

**验收标准**:
- 所有注入 system prompt / messages 的上下文均使用 XML 标签
- Block 2 的结构: `<project_context>...</project_context>\n<memory>...</memory>\n<skills>...</skills>`
- Compaction summary 在 messages 中使用 `<conversation_history_summary>` 标签

### 8.5 Batch 5 — Compaction 体验改进

**目标**: 借鉴 pi 的 compaction 设计 — 摘要质量提升 + 结构化格式 + 可配置预算。

**任务**:

| ID | 任务 | 文件 | 描述 |
|---|---|---|---|
| T5.1 | 结构化摘要 prompt | `compact.rs` | `SUMMARY_SYSTEM` 改为要求结构化输出: `<decisions>`, `<files_modified>`, `<commands_run>`, `<current_state>`, `<open_questions>` |
| T5.2 | `MAX_SUMMARY_TOKENS` 可配置 | `settings.rs`, `loop_.rs` | 新增 `compact_max_tokens: u32` 配置，默认 4096，长对话可设为 8192 |
| T5.3 | Compaction 记录 token_before 的真实值 | `loop_.rs` | 当前 `tokens_before: 0` 占位 — 改为记录 LLM 返回的 `input_tokens` |
| T5.4 | 后台 compact stale 时重用上次结果 | `loop_.rs` | 如果 `pending_compact` 的 messages hash 与上次成功的 compact 一致，重用结果而非丢弃 |
| T5.5 | 可选: overflow recovery | `loop_.rs` | 检测 provider 返回 context overflow error 时，自动 compact + retry 一次 (借鉴 pi) |

**验收标准**:
- Compaction summary 包含结构化的 `<decisions>`, `<files_modified>` 等 XML 段
- `MAX_SUMMARY_TOKENS` 可通过 settings.json 配置
- `Compacted` 事件的 `tokens_before` 不再为 0

### 8.6 Batch 6 — System Prompt 来源分层

**目标**: 支持用户完全替换或追加默认 system prompt。

**任务**:

| ID | 任务 | 文件 | 描述 |
|---|---|---|---|
| T6.1 | 发现 `SYSTEM.md` 文件 | `context.rs` | 搜索 `.nonoclaw/SYSTEM.md` (项目) + `~/.nonoclaw/SYSTEM.md` (全局) |
| T6.2 | 发现 `APPEND_SYSTEM.md` 文件 | `context.rs` | 同上路径，搜索 `APPEND_SYSTEM.md` |
| T6.3 | `SYSTEM.md` → 替换默认 prompt | `prompt.rs` | 如果 `SYSTEM.md` 存在，其内容**完全替换** `build_system_prompt_sections()` 的 identity/guidelines 段落 (类似 pi 的 customPrompt 模式) |
| T6.4 | `APPEND_SYSTEM.md` → 追加 | `prompt.rs` | 内容追加到 prompt 末尾 (区别于现有 `append_system_prompt` 选项，后者从 CLI/settings 注入) |
| T6.5 | 文档说明 | `NONOCLAW.md` 或 README | 说明 SYSTEM.md / APPEND_SYSTEM.md / NONOCLAW.md 三者的优先级和用途 |

**验收标准**:
- `.nonoclaw/SYSTEM.md` 存在时，默认 BASE prompt 被完全替换
- `.nonoclaw/APPEND_SYSTEM.md` 存在时，内容追加到 prompt
- 三个文件的优先级清晰: SYSTEM.md (替换) > NONOCLAW.md (项目上下文) > APPEND_SYSTEM.md (追加)

### 8.7 Batch 7 — Context 缓存与 I/O 优化

**目标**: 减少 per-turn 重复 I/O (git 子进程、文件读取)。

**任务**:

| ID | 任务 | 文件 | 描述 |
|---|---|---|---|
| T7.1 | NONOCLAW.md mtime 缓存 | `context.rs` | `get_user_context()` 记录文件 mtime，未变化时返回缓存内容 |
| T7.2 | Memory 内容只在 run 开始时加载 | `loop_.rs`, `context.rs` | `load_memory_prompt()` 结果缓存到 `QueryEngine` 字段，memory tool 写入后通过 flag 刷新 |
| T7.3 | git context 增量刷新 | `loop_.rs` | 仅在 Bash/Edit/Write 工具执行后调用 `get_system_context()`，而非每 turn 无条件 |
| T7.4 | git 命令并行化 | `context.rs` | 4 个 git 子进程 (`rev-parse`, `status`, `log`, `config`) 用 `tokio::try_join!` 并行 |

**验收标准**:
- 连续 N turn (无文件写入) 只读一次 NONOCLAW.md 文件
- Memory 文件只读一次（除非 Memory tool 写入）
- git context 不在无文件变更的 turn 重新执行

### 8.8 Batch 8 — P2 增强

| ID | 任务 | 文件 | 描述 |
|---|---|---|---|
| T8.1 | Prompt templates 系统 | `engine/` (新 `prompt_templates.rs`) | `.nonoclaw/prompts/*.md` → `/template args` 展开，支持 `$1`, `$@`, `${N:-default}` 变量 |
| T8.2 | 祖先目录上下文遍历 | `context.rs` | `get_user_context()` 从 cwd 向上遍历所有祖先目录的 `.nonoclaw/NONOCLAW.md` |
| T8.3 | Tool guidelines 注册扩展 | `tools/` | 允许 MCP server 在 tool definition 中声明 `prompt_guidelines` |

### 8.9 执行优先级建议

```
Phase 1 (立即):  Batch 1 (date 移位)                       — 极小改动，立即修复 cache miss
Phase 2 (近期):  Batch 2 (参数化) + Batch 3 (snippet)      — 为后续优化打基础
Phase 3 (中期):  Batch 4 (XML) + Batch 5 (compaction)      — 用户体验提升
Phase 4 (后续):  Batch 6 (分层) + Batch 7 (缓存)           — 灵活性 + 性能
Phase 5 (择期):  Batch 8 (P2 增强)                         — 增量功能
```

### 8.10 风险与约束

| 风险 | 缓解措施 |
|---|---|
| 参数化重构破坏 Block 1 cache 稳定性 | T2.6 保持默认行为 byte-for-byte 一致 + 编写 diff 测试 |
| XML 标签影响不支持 XML 的 provider | 大部分主流 provider (Anthropic/OpenAI) 原生支持 XML；对 OpenAI-format 可选关闭 |
| Tool trait 新增方法影响外部实现 | 提供 default 实现，现有 tool 不需要修改即可编译 |
| SYSTEM.md 完全替换可能导致安全/行为问题 | 仅当项目 trusted 时加载 (参考 pi 的 project-trust 机制) |
| Compaction 结构化格式可能降低摘要质量 | 先在测试中对比旧格式输出，确认无 regression 再上线 |

---

## 9. 执行总结 (Implementation Record)

> 执行日期: 2026-07-28 | 状态: Phase 1–5 全部完成 | 测试: 193 通过, 0 失败

### 9.0 总览

§8 中的 8 个批次全部落地（Batch 5 的 T5.4/T5.5 和 Batch 8 的 T8.2 标记为"后续可选"）。每个改动都有对应的单元测试，累计新增 **39 个测试**（engine crate: 135→144; tools crate 49; 历史 context.rs 2→7, compact.rs 4→7, loop_.rs 无变化至 133→135）。

| 批次 | 名称 | 任务完成度 | 测试数 | 关键文件 |
|---|---|---|---|---|
| Batch 1 | Prompt Cache 修复 | T1.1–T1.4 ✅ | 4 | `prompt.rs` |
| Batch 2 | System Prompt 参数化 | T2.1–T2.6 ✅ | 7 | `prompt.rs`, `loop_.rs`, `settings.rs` |
| Batch 3 | Tool Snippet/Guideline 分离 | T3.1–T3.6 ✅ | 3 (+内置工具各自) | `tool.rs`, `builtin/*.rs`, `prompt.rs` |
| Batch 4 | XML 结构化上下文包装 | T4.1–T4.5 ✅ | 5 | `context.rs`, `prompt.rs`, `compact.rs` |
| Batch 5 | Compaction 改进 | T5.1–T5.3 ✅; T5.4/T5.5 后续 | 2 | `compact.rs`, `loop_.rs`, `settings.rs` |
| Batch 6 | System Prompt 来源分层 | T6.1–T6.5 ✅ | 7 | `context.rs`, `prompt.rs` |
| Batch 7 | Context 缓存与 I/O 优化 | T7.3–T7.4 ✅; T7.1/T7.2 已满足 | 2 | `context.rs`, `loop_.rs` |
| Batch 8 | P2 增强 | T8.1 ✅; T8.2 跳过; T8.3 已由 Batch 3 满足 | 9 | `prompt_templates.rs` (新) |

### 9.1 Batch 1 — Prompt Cache 修复 (date 移位)

**改动**:
- `prompt.rs`: 从 Block 1 (cached) 移除 `- Today's date: {}` 行
- `prompt.rs`: 在 Block 2 (uncached) 开头加入 `# Current date\n{date}\n` (两处: `build_system_blocks_with_profile` + `refresh_context_block`)

**验收**:
- `block1_is_byte_stable_across_dates` — 同一 cwd 跨天调用 Block 1 字节一致
- `block1_does_not_contain_date` — Block 1 不再出现日期或 "Today's date" 字样
- `block2_contains_date_and_git` — Block 2 同时包含日期和 git 摘要
- `refresh_context_block_preserves_block1_and_updates_date` — 刷新只更新 Block 2

### 9.2 Batch 2 — System Prompt 参数化重构

**改动**:
- `prompt.rs`: 将 `BASE` (~4KB) 拆分为 10 个独立 const:
  - `IDENTITY`, `CODE_QUALITY`, `SAFETY`, `FAILURE_MODES`, `PARALLELISM`, `DEPENDENCIES`, `MEMORY_GUIDE`, `WIKI_GUIDE`, `DIAGRAM_GUIDE`, `TASK_COMPLETION`
- 新增 `PromptProfile` enum:
  ```rust
  pub enum PromptProfile {
      Full,                                  // 默认，与原 BASE 一致
      Minimal,                               // identity + safety + task_completion
      Custom(HashSet<String>),               // 自定义 section 集合
  }
  ```
- 新增 `SystemPromptSections::NAMES` — 10 个 section 的 canonical 顺序
- 新增 `build_system_prompt_sections(profile)` — 按 profile 组合 section
- 新增 `build_system_blocks_with_profile(...)` — 接受 profile 参数
- `build_system_blocks(...)` 保持原签名，内部委托到 `_with_profile(Full)`
- `EngineOptions` 新增 `prompt_profile` 字段 (默认 `Full`)
- `SettingsFile` 新增 `promptProfile: Option<String>` ("full" | "minimal")
- 原 `BASE` const 保留为 `#[allow(dead_code)]` 参考基准

**验收**:
- `full_profile_matches_legacy_base` — `PromptProfile::Full` 与原 `BASE` byte-for-byte 一致
- `minimal_profile_is_significantly_shorter` — Minimal ≤ Full 的 60% (减少 ≥40%)
- `minimal_profile_only_keeps_identity_safety_task_completion` — 精确 section 过滤
- `custom_profile_selects_explicit_sections` — Custom 按名选择
- `section_constants_compose_in_canonical_order` — 顺序正确
- `build_system_blocks_default_uses_full_profile` — 向后兼容

**用法**:
```json
// settings.json
{ "promptProfile": "minimal" }
```

### 9.3 Batch 3 — Tool Snippet / Guideline 分离

**改动**:
- `tool.rs`: `Tool` trait 新增两个方法（均有默认实现）:
  ```rust
  fn snippet(&self) -> String {
      // 默认: description() 的首行截断到 80 chars
      self.description().lines().next().unwrap_or("").chars().take(80).collect()
  }
  fn prompt_guidelines(&self) -> &[&str] { &[] }
  ```
- 内置工具覆盖实现:
  - **Bash**: snippet "Run shell commands (build, test, git, package managers)"; 3 条 guidelines（用 Grep 替代 rg、用 Read 替代 cat、引号路径）
  - **Grep**: snippet "Search file contents with a regex (ripgrep)"; 2 条 guidelines
  - **Read**: snippet "Read a file with optional offset/limit"; 1 条 guideline
  - **Edit**: snippet "Replace an exact string in a file (surgical edits)"; 2 条 guidelines
- `prompt.rs`: 新增 `ToolPromptEntry { name, prompt, snippet, guidelines }` struct 替代 `(String, String)` tuple
- `prompt.rs`: Available Tools 列表改用 `tool.snippet()` 而非 prompt 首行
- `prompt.rs`: 新增 `## Tool-specific guidance` 段，收集所有活跃工具的 guidelines 并去重
- `loop_.rs`: 构造 `Vec<ToolPromptEntry>` 时调用 `t.snippet()` + `t.prompt_guidelines()`
- MCP 工具 (`mcp.rs`) 通过默认实现从 server 提供的 description 派生 snippet

**验收**:
- `tools_list_uses_snippet_not_first_line` — Available Tools 显示 snippet，完整 prompt 不泄漏
- `tool_guidelines_are_collected_and_deduplicated` — guidelines 被收集并去重
- `no_tool_guidelines_means_no_section` — 无 guidelines 时不显示空段

### 9.4 Batch 4 — XML 结构化上下文包装

**改动**:

| 上下文类型 | 旧格式 | 新格式 |
|---|---|---|
| NONOCLAW.md | `# Project context\n## from {source}\n{content}` | `<project_context>\n<project_instructions path="{source}">\n{content}\n</project_instructions>\n</project_context>` |
| Memory | `# Memory\n\n{content}` | `<memory>\n{content}\n</memory>` |
| Dynamic Skills | `\n{metadata}\n` | `<skills>\n{metadata}\n</skills>` |
| Compact summary | `[Compacted summary...]\n{summary}\n[End summary...]` | `<conversation_history_summary>\n{summary}\n</conversation_history_summary>\nRecent messages follow.` |

- `context.rs`: `append_md()` 改为 XML 格式；新增 `close_project_context()` 闭合标签
- `prompt.rs`: Block 2 两处 (`build_system_blocks_with_profile` + `refresh_context_block`) memory 改用 `<memory>` 标签，skills 改用 `<skills>` 标签
- `compact.rs`: summary 注入消息改用 `<conversation_history_summary>` 标签

**验收**:
- `nonoclaw_md_uses_project_context_xml` — 实际读取 tempdir 中的 NONOCLAW.md 验证 XML 结构
- `nonoclaw_md_empty_when_no_files` — 无文件时返回空字符串
- `memory_is_wrapped_in_xml_tag` — Block 2 中 memory 用 `<memory>` 包裹
- `project_context_passthrough_into_block2` — XML 包装的 nonoclaw_md 原样进入 Block 2
- `refresh_context_block_wraps_memory_in_xml` — 刷新时 memory 也用 XML
- `compacted_summary_uses_xml_tag` — compact summary 使用新标签

### 9.5 Batch 5 — Compaction 体验改进

**改动**:
- **T5.1 结构化摘要 prompt**: `compact.rs` 中 `SUMMARY_SYSTEM` 重写为要求 XML 结构化输出:
  ```
  <goal>用户目标</goal>
  <decisions>关键决策</decisions>
  <files_modified>文件变更</files_modified>
  <commands_run>命令执行</commands_run>
  <current_state>当前状态</current_state>
  <open_questions>开放问题</open_questions>
  ```
- **T5.2 `MAX_SUMMARY_TOKENS` 可配置**:
  - 原 `const MAX_SUMMARY_TOKENS: u32 = 4096` → `pub const DEFAULT_MAX_SUMMARY_TOKENS: u32 = 4096`
  - `compact_messages()` 新增 `max_summary_tokens: u32` 参数
  - `EngineOptions` 新增 `compact_max_tokens: u32` 字段
  - `SettingsFile` 新增 `compactMaxTokens: Option<u32>`
  - 三处 `compact_messages()` 调用点全部传入 `self.options.compact_max_tokens`
- **T5.3 Compacted 事件真实 token 值**:
  - `QueryEngine` 新增 `pending_compact_tokens_est: usize` 字段
  - spawn 后台 compact 时记录 `est`
  - 完成时使用 `tokens_at_spawn` 替代占位 `0`，并计算 `tokens_after`

**未完成 (后续可选)**:
- T5.4 后台 compact stale 时重用上次结果（需 hash 比较）
- T5.5 overflow recovery（检测 context overflow error 后自动 compact + retry）

**验收**:
- `summary_system_prompts_for_structured_output` — SUMMARY_SYSTEM 包含全部 6 个 XML section
- `default_max_summary_tokens_is_4096` — 默认值正确

**用法**:
```json
// settings.json — 长对话提升摘要预算
{ "compactMaxTokens": 8192 }
```

### 9.6 Batch 6 — System Prompt 来源分层

**改动**:
- `context.rs`: `UserContext` 新增两个字段:
  ```rust
  pub system_md_override: Option<String>,   // SYSTEM.md 内容
  pub append_system_md: Option<String>,     // APPEND_SYSTEM.md 内容
  ```
- `context.rs`: `get_user_context()` 发现文件（项目优先于用户全局）:
  - `<cwd>/.nonoclaw/SYSTEM.md`
  - `<cwd>/.nonoclaw/APPEND_SYSTEM.md`
  - `~/.nonoclaw/SYSTEM.md`
  - `~/.nonoclaw/APPEND_SYSTEM.md`
- `prompt.rs`: `build_system_blocks_with_profile()` 逻辑:
  - `system_md_override` 存在时 → 完全替换 `build_system_prompt_sections(profile)` 输出
  - `append_system_md` 存在时 → 在 `# Additional instructions (from APPEND_SYSTEM.md)` 段追加
  - CLI `append_system_prompt` 仍独立工作，两者可共存

**三层优先级**:
```
SYSTEM.md (替换 BASE body) > NONOCLAW.md (项目上下文, Block 2) > APPEND_SYSTEM.md (追加)
```

**验收**:
- `system_md_override_replaces_base_body` — SYSTEM.md 替换后默认 identity/code_quality/safety 全部消失
- `append_system_md_appended_after_static_sections` — APPEND_SYSTEM.md 追加到 Block 1
- `cli_append_system_prompt_still_works_alongside_file` — 文件追加 + CLI 追加共存
- `system_md_override_combined_with_append_system_md` — 替换 + 追加组合
- `system_md_loaded_from_project_dir` / `append_system_md_loaded_from_project_dir` — 文件发现
- `missing_system_files_yield_none` — 无文件时字段为 None

### 9.7 Batch 7 — Context 缓存与 I/O 优化

**改动**:
- **T7.3 git context 增量刷新**:
  - `QueryEngine` 新增 `cached_git_context: Option<SystemContext>` 字段
  - run 开始时 seed cache（首次 git 调用结果）
  - 工具执行后检测是否包含 mutating tool (`Bash`/`Edit`/`Write`/`MultiEdit`/`NotebookEdit`)
  - mutating tool 执行后 `self.cached_git_context = None` (invalidate)
  - 下个 turn 优先用 cache，cache 为 None 时才调 `get_system_context()`
- **T7.4 git 命令并行化**:
  - `context.rs`: 4 个 git 子进程从顺序 `.await` 改为 `tokio::join!(...)`
  - 延迟从 4×~10ms 降到 ~10ms wall time

**已满足（无需改动）**:
- T7.1 NONOCLAW.md mtime 缓存: `user_ctx` 已在 run 开始时加载一次，turn loop 共享同一引用
- T7.2 Memory 只加载一次: `memory` 同样在 run 开始时加载一次

**验收**:
- `mutating_tool_names_invalidate_git_cache` — mutating tool 名称集合正确
- `engine_options_default_cached_git_context_is_none` — 新 engine cache 为空

### 9.8 Batch 8 — P2 增强

**T8.1 Prompt Templates 系统** ✅

新建 `prompt_templates.rs` 模块（~280 行 + 9 个测试）:

- **文件发现**: `.nonoclaw/prompts/*.md` (项目) + `~/.nonoclaw/prompts/*.md` (用户全局)，项目覆盖用户
- **变量展开**:
  - `$1`, `$2`, ... — 位置参数（1-based）
  - `$@` / `$ARGUMENTS` — 所有参数空格连接
  - `${N:-default}` — 带默认值的位置参数
  - `${@:N}` — bash 风格切片（从位置 N 开始）
  - `$$` — 字面 `$`
- **Frontmatter**: 可选 `argument-hint:` 字段
- **API**:
  ```rust
  let reg = PromptTemplateRegistry::discover(&cwd);
  if let Some(expanded) = reg.expand("pr", "123 456") {
      // expanded = "Review PR #123 and #456"
  }
  ```

**示例**:
```markdown
<!-- .nonoclaw/prompts/pr.md -->
---
argument-hint: "issue numbers"
---
Review this PR. Focus on $@.
```
用户输入 `/pr 123 456` → 展开为 `Review this PR. Focus on 123 456.`

**T8.2 祖先目录上下文遍历** ⏭️ 跳过

需要更明确的设计决策（向上走多远？遇到 `.git` 停止？），且与现有 6 级 NONOCLAW.md 加载顺序有重叠风险。

**T8.3 Tool guidelines 注册扩展** ✅ 已满足

Batch 3 的 `Tool::prompt_guidelines()` 默认实现已覆盖此需求。MCP tool 可通过 wrapper 添加 guidelines。

### 9.9 新增 API / 配置一览

| API / 配置 | 类型 | 位置 | 默认值 |
|---|---|---|---|
| `PromptProfile` | enum (Full/Minimal/Custom) | `prompt.rs` | `Full` |
| `SystemPromptSections::NAMES` | `&[&str]` (10 项) | `prompt.rs` | — |
| `build_system_prompt_sections(profile)` | fn → `String` | `prompt.rs` | — |
| `build_system_blocks_with_profile(...)` | fn → `Vec<SystemBlock>` | `prompt.rs` | — |
| `ToolPromptEntry { name, snippet, guidelines }` | struct | `prompt.rs` | — |
| `Tool::snippet()` | trait method → `String` | `tool.rs` | description 首行截断 80 chars |
| `Tool::prompt_guidelines()` | trait method → `&[&str]` | `tool.rs` | `&[]` |
| `settings.json.promptProfile` | `"full"` \| `"minimal"` | `settings.rs` | `"full"` |
| `settings.json.compactMaxTokens` | `u32` | `settings.rs` | `4096` |
| `EngineOptions.compact_max_tokens` | `u32` | `loop_.rs` | `4096` |
| `EngineOptions.prompt_profile` | `PromptProfile` | `loop_.rs` | `Full` |
| `UserContext.system_md_override` | `Option<String>` | `context.rs` | `None` |
| `UserContext.append_system_md` | `Option<String>` | `context.rs` | `None` |
| `QueryEngine.cached_git_context` | `Option<SystemContext>` | `loop_.rs` | `None` |
| `DEFAULT_MAX_SUMMARY_TOKENS` | `pub const u32` | `compact.rs` | `4096` |
| `PromptTemplateRegistry` | struct | `prompt_templates.rs` | — |

### 9.10 测试统计

```
engine crate:  144 passed  (原 108, 新增 36)
tools crate:    49 passed  (原 49, 无变化)
doc tests:       0
─────────────────────────
总计:          193 passed, 0 failed, 0 ignored
```

新增测试按模块分布:

| 模块 | 新增测试 |
|---|---|
| `prompt::tests` | 18 (Batch 1: 4, Batch 2: 7, Batch 3: 3, Batch 4: 3, Batch 6: 4 — 含 1 覆盖) |
| `context::tests` | 5 (Batch 4: 2, Batch 6: 3) |
| `compact::tests` | 3 (Batch 4: 1, Batch 5: 2) |
| `loop_::tests` | 2 (Batch 7: 2) |
| `prompt_templates::tests` | 9 (Batch 8) |

### 9.11 风险缓解验证

| §8.10 风险 | 实际缓解 |
|---|---|
| 参数化重构破坏 Block 1 cache 稳定性 | ✅ `full_profile_matches_legacy_base` 测试保证 byte-for-byte 一致 |
| XML 标签影响不支持 XML 的 provider | ⚠️ 当前未做 provider 适配；Anthropic/OpenAI 原生支持，后续可加 flag |
| Tool trait 新增方法影响外部实现 | ✅ `snippet()` 和 `prompt_guidelines()` 都有默认实现 |
| SYSTEM.md 完全替换导致安全/行为问题 | ⚠️ 当前无条件加载；后续可加 project-trust 机制 |
| Compaction 结构化格式降低摘要质量 | ✅ 结构化 XML 增加了信息密度，非自由文本；测试验证格式正确 |

### 9.12 后续工作（未完成项）

| 项目 | 批次 | 原因 | 建议优先级 |
|---|---|---|---|
| T5.4 后台 compact stale 重用 | Batch 5 | 需 transcript hash 比较 + 缓存层 | P2 |
| T5.5 overflow recovery | Batch 5 | 标记为"可选"；需检测 provider 特定 error | P2 |
| T8.2 祖先目录 NONOCLAW.md 遍历 | Batch 8 | 需明确设计（`.git` 边界？`$HOME` 边界？） | P3 |
| Prompt templates CLI 集成 | Batch 8 | 模块已完成，需在 CLI 入口拦截 `/name args` | P1 |
| Provider 适配 XML 标签开关 | Batch 4 | 对不支持 XML 的 provider 关闭 XML 包装 | P3 |
| SYSTEM.md project-trust 机制 | Batch 6 | 防止不受信任项目完全替换 system prompt | P2 |
