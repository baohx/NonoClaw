# NonoClaw 秘密手册 · 操作大全

> 黑客手册 / 大师秘密文档。覆盖非常规功能、机制类自动功能、连接集成、Insight 详解。
> 每个功能四段式：**原理 → 操作 → 示例 → 验证**。
> 版本：v0.19.0（2026-08-18）

---

## 目录

**第一部分：黑科技功能**
1. Agent 图（Graph）
2. 子代理体系（Agent / Coordinator）
3. Agent Profile 完全手册
4. 跨会话记忆（Mneme 三层 + 会话向量索引）
5. AutoDream —— 机器自己"做梦"
6. LLM Wiki 知识编译器
7. 技能系统（Skills）
8. Hooks 插件
9. 后台 Bash + 通知注入
10. 本体域（Ontology-MCP）

**第二部分：机制类自动功能**
11. Prompt 缓存体系（滚动断点）
12. 上下文压缩分级（microCompact → autoCompact）
13. 精确 BPE 分词 + 记忆预算分区
14. MCP 会话级工具选择
15. Landlock 沙箱

**第三部分：连接与集成**
16. REST API
17. ACP 协议（Zed 直连）
18. MCP Server 模式
19. 手机 / 隧道
20. 远程 JSON-lines 模式

**第四部分：Insight 面板详解**
21. Insight 完全解读（含缓存命中率 + Technical Trace + 呼吸状态机）

**附录**
22. 环境变量速查
23. 文件系统地图

---

# 第一部分：黑科技功能

---

## 1. Agent 图（Graph）—— 声明式 DAG 管线

### 原理
把"多个 agent 怎么协作"写成一个 Markdown 文件（YAML frontmatter 定义节点和边），引擎用 Kahn 拓扑排序执行：能并行的节点（fan-out）同时跑，结果汇聚（fan-in）后再走下一步。节点三种：
- **agent 节点**：起一个子代理干活
- **router 节点**：LLM 看输入动态选一个分支（单选）
- **gate 节点**：暂停等人审批（headless 模式默认放行）

执行到一半断了？**checkpoint** 机制把每个节点完成状态存进 `.checkpoints/<name>.json`，重跑时跳过已完成的节点。

### 操作
文件放 `.nonoclaw/graphs/<name>.md`，然后两种触发方式：
- Web UI 输入框敲 `/graph <name>`（可带参数）
- 直接对 agent 说"用 Graph 工具跑 xxx 图"

### 示例
`.nonoclaw/graphs/code-review.md`——三路并行审查 + 汇总：

```markdown
---
name: code-review
description: 三视角并行代码审查
nodes:
  - id: security
    type: agent
    prompt: "从安全角度审查当前工作区 diff：注入、越权、密钥泄露"
  - id: perf
    type: agent
    prompt: "从性能角度审查当前工作区 diff：算法复杂度、无谓分配、IO 放大"
  - id: style
    type: agent
    prompt: "从可维护性角度审查当前工作区 diff：命名、重复、过度设计"
  - id: merge
    type: agent
    prompt: "汇总三份审查报告，按严重度排序输出 TOP 5 问题"
    next: []
  - id: dispatch
    type: router
    prompt: "判断改动规模：小改动只走 style，大改动全走"
    branches:
      - small: ["style", "merge"]
      - full: ["security", "perf", "style", "merge"]
next:
  dispatch: full
  security: merge
  perf: merge
  style: merge
---
```

### 验证
- 跑 `/graph code-review`，观察 Technical Trace 出现 subagent 事件（并行多个）
- 中途 Ctrl+C 杀掉，看 `.checkpoints/code-review.json` 出现；再跑 `/graph code-review`，日志显示跳过已完成节点
- 覆盖 Anthropic 全部 5 种工作流模式（prompt chaining / routing / parallelization / orchestrator-workers / evaluator-optimizer）都可以用图表达

---

## 2. 子代理体系（Agent / Coordinator）

### 原理
- **Agent 工具**：spawn 一个完整子代理，自主多轮干活（默认 24 轮上限），非交互（权限问题自动按策略处理），继承父级的 CancellationToken（你点停止，全家停止）和 hooks
- **Coordinator**：把一批独立任务并行扇出，各自一个子代理
- **防递归**：子代理的工具表里没有 Agent/Coordinator/Graph—— depth=1 硬限制，子代理不能再生孙子
- 子代理有自己的系统提示词（精简版），上下文独立——干完活只把最终结果交回父级，中间过程不占父级上下文

### 操作
直接在对话里用自然语言指挥即可：
- "用一个 agent 去……"（agent 会自己调 Agent 工具）
- "并行做这三件事：……"

### 示例
```
并行调查：① rust/crates/api 的重试逻辑 ② engine 的 compaction 流程 ③ tools 的权限门
每个用独立子代理，汇总成对比表
```

### 验证
- UI 里出现嵌套的子代理区块，Technical Trace 有 SubagentStart/SubagentStop 事件
- 环境变量 `NONOCLAW_SUBAGENT_MAX_TURNS` 可调轮次（默认 24，硬上限 200）
- 并发上限 `MAX_SUBAGENT_CONCURRENCY=64`（默认实际并发 4）

---

## 3. Agent Profile 完全手册

### 原理
Profile = 给 agent 换人格的声明式文件，放 `.nonoclaw/agents/<name>.md`（YAML frontmatter + 正文）。核心机制：
- **`systemPromptAppend`**：追加到默认子代理提示词后面（微调）
- **`systemPromptOverride`**：**完全替换**提示词（彻底改造，如 verifier）
- **`toolsAllow` / `toolsDeny`**：工具白/黑名单。**关键规则：只能收紧不能放大**——deny 一定生效，allow 不会绕过父级限制（防止 profile 提权）
- **`permissionMode`**：只能降到和父级一样严格或更严格（严格度排序 plan < default < acceptEdits < auto < bypass，见 agents.rs `permission_strictness`）
- 绑定方式：`models[]` 里加 `"profile": "xxx"` 字段——**按模型绑人设**（换模型 = 换人格）

另有一个容易混淆的同名概念：**`settings.json` 的 `promptProfile`**（full/minimal/ultra）——那是**系统提示词瘦身档位**，控制 identity/safety/task_completion 之外的内容量，与 agent profile 无关。

### 操作
1. 建文件 `.nonoclaw/agents/<name>.md`
2. 触发：对话里让 agent 用 `profile: <name>` 起子代理，或绑到某个模型上

### 示例
只读审计员 `.nonoclaw/agents/auditor.md`：

```markdown
---
name: auditor
description: 只读安全审计员，输出结构化报告
system_prompt_override: |
  你是安全审计员。只读代码，禁止修改。输出格式：
  ## 风险清单
  每条：等级(高/中/低) | 位置 | 描述 | 建议修复
  没有风险就明说"未发现"，不许为了凑数硬编。
tools_deny:
  - Edit
  - Write
  - MultiEdit
  - Bash
---
```

### 验证
- 起子代理时 Technical Trace 显示 profile 名
- 让它改文件 → 应被 toolsDeny 拒绝（工具不在它的表里）
- 内置 **verifier**（`.nonoclaw/agents/verifier.md`，v0.19）：对抗性验证 agent，任务是"搞崩实现"——所有结论必须有命令输出、攻击向量清单（边界/并发/幂等/孤儿）、VERDICT 三态（PASS/FAIL/INCONCLUSIVE）。注意它**不设 permissionMode**：plan 模式会拒 Bash，而 verifier 必须能执行命令

---

## 4. 跨会话记忆（Mneme 三层 + 会话向量索引）

### 原理
三层记忆，全是 Git 友好的 Markdown/JSONL：
- **Facts**（`memory/facts/*.md`）：不可变知识。写错了不删——写新 fact 用 frontmatter `supersedes` 指旧的名，链式取代
- **Beads**（`memory/beads/*.md`）：任务连续性。一个 bead 一个任务，状态 todo/in_progress/blocked/done
- **Transcript**（每会话 JSONL）：原始记录

检索是**混合搜索**：BM25 关键词 + 本地向量（字符三元组特征哈希 → 256 维符号向量 → 余弦相似度，无需嵌入 API）。**Layer 3（v0.19）**：全会话记录也向量化了（i8 量化 base64，按文件指纹增量更新），所以 agent 能"回忆"起**上一个会话**里干过的事。

会话启动时系统提示词 Block 2 自动注入：活跃 beads + 高重要性 facts + wiki index。每个分区（beads/facts/wiki/index）有独立 token 预算，互不挤占。

### 操作
全部走 `Memory` 工具：
- `session_search "上周的缓存断点工作"` —— 跨会话回忆
- `search_facts "权限"` —— 事实检索（向量×2 + BM25 + importance 加权）
- `wiki_search` / `wiki_ingest` / `wiki_lint` —— Wiki 操作
- `goal_create` / `goal_update` / `goal_list` —— 目标管理

### 示例
```
新会话第一句：
"回忆一下之前 ontology-mcp 项目的进展，从记忆里找，不要猜"
→ agent 会 session_search + 读 bead，接着上次干
```

### 验证
- 让 agent 写一个 fact（frontmatter：name/title/type/importance/confidence），下个新会话问相关问题，看它能否引用
- `search_facts` 结果带 importance 排序；`VECTOR_NOISE_FLOOR=0.1` 以下视为噪声

---

## 5. AutoDream —— 机器自己"做梦"

### 原理
灵感来自 Claude Code 泄露源码的 AutoDream/Dream Memory。你离开电脑后，服务器自己起一个 headless run 整理记忆。**触发条件（每分钟检查，全满足才做梦）**：
1. 闲置 ≥ `dreamIdleMinutes`（默认 10 分钟）——任何 WS 消息/REST 请求都会刷新活动时间
2. 无 pending permissions/questions、无运行中后台任务
3. 会话目录有新素材（文件数+最新 mtime 指纹变化，幂等防热循环）
4. `dreamEnabled`（默认 true）

**梦的四阶段（REM）**：碎片收集（session_search 捞近期会话）→ 关联分析（找重复错误模式、因果链）→ 知识萃取（提炼可复用事实）→ 写入 `memory/facts/`。纪律：先 Grep 查重、单次最多 3 条、宁缺毋滥。做完刷新会话向量索引 + 写 `.nonoclaw/last_dream.json` 标记。dream 自己也会重置闲置计时——不会连环做梦。

### 操作
全自动。想手动快速验证：`~/.nonoclaw/settings.json` 加 `"dreamIdleMinutes": 1`，重启服务器，然后离开 1 分钟。

### 示例
观察日志（用 `--log-raw-api` 启动时更全）：
```
dream scheduler watching for idle        ← 监视开始
dream run started                        ← 开始做梦
dream run finished                       ← 做完
```

### 验证
- `cat .nonoclaw/last_dream.json` 看 dream 完成时间戳
- `ls -t memory/facts/` 看有没有新萃取的 fact（没有也正常——纪律是不值得就不写）
- 不想要了：settings `"dreamEnabled": false`

---

## 6. LLM Wiki 知识编译器

### 原理
Karpathy 式知识编译：`.nonoclaw/wiki/` 目录存结构化互链页面（concepts/entities/comparisons/decisions/sources 五类），`raw/` 放不可变原始文档。LLM 当编译器：喂源文档 → 产出/更新 wiki 页 → 更新 index.md → 记 log.md。每页 YAML frontmatter 带 confidence（high/medium/low）——低置信知识明确标出，不和铁事实混在一起。

### 操作
1. 把文档丢进 `raw/`（PDF 转出的 txt、网页存档、论文……）
2. 对 agent 说："wiki_ingest raw/xxx.txt"
3. 之后任何会话问相关知识 → `wiki_search` 检索

### 示例
```
把 docs/legacy-design.txt 放进 raw/ 后：
"ingest 这份设计文档进 wiki，重点提炼缓存策略的决策原因"
→ 产出 decisions/cache-strategy.md（含 sources 指回原文）+ 更新 index
```

### 验证
- `wiki_lint` 检查：未标 tag 的页、无来源的断言、低置信条目
- 会话启动自动注入 `wiki/index.md`（预算内）——新会话直接"知道"wiki 里有什么

---

## 7. 技能系统（Skills）

### 原理
技能 = 可复用的提示词模块（`.nonoclaw/skills/<name>/SKILL.md`，全局放 `~/.nonoclaw/skills/`）。**渐进式披露**是省 token 的关键：系统提示词（缓存前缀）里只放技能名+一句话描述的**索引**，完整正文需要时才经 `Skill` 工具加载——所以装 100 个技能也不撑爆缓存。激活方式三种：`/name` 显式注入、触发词命中自动激活、文件发现（工作区出现特定文件时）。

### 操作
- 用：输入框敲 `/skill-name`（可带参数 `$1 $2`）
- 写：建 `.nonoclaw/skills/my-skill/SKILL.md`，frontmatter 写 name/description/triggers
- 热重载：改完即生效，无需重启（skill_watcher 监视）

### 示例
```markdown
---
name: commit
description: 按项目规范生成提交
triggers:
  - 提交代码
  - commit
---
检查 git diff 与 git log 最近 5 条，遵循现有 message 风格生成提交。
禁止 push。
```

### 验证
- **陷阱**：triggers 必须是**纯字符串数组**——写成 `{pattern: xxx}` 映射会被 YAML 扁平化弄坏（engine 实测踩过）
- Insight → Skills 面板看已加载列表和激活状态；正文加载走 Skill 工具（Technical Trace 可见）

---

## 8. Hooks 插件

### 原理
`.nonoclaw/hooks.json` 定义钩子：**12 种事件**（PreToolUse / PostToolUse / SessionStart / SessionEnd / RunStart / RunEnd / SubagentStart / SubagentStop / PreCompact / PostCompact 等）× **三种执行体**：
- `shell`：跑命令（stdin 收 JSON 上下文）
- `prompt`：注入提示词内容
- `http`：POST 到 URL（URL/headers 支持 `$ENV` 插值）

### 操作
写 `.nonoclaw/hooks.json`，保存即用。

### 示例
每次 Edit 后自动 format：
```json
{
  "hooks": [
    {
      "event": "PostToolUse",
      "match": "Edit|Write",
      "type": "shell",
      "command": "cargo fmt"
    }
  ]
}
```

### 验证
- Technical Trace 有 hooks 事件行
- shell 钩子非零退出码会反馈给 agent（可作为硬约束：格式检查失败 → agent 收到失败信号）

---

## 9. 后台 Bash + 通知注入

### 原理
工具调用带 `run_in_background: true` → 命令脱离执行，输出落盘，agent 立即继续干别的。任务完成时引擎注入 `<task_notification>` 到对话——agent"睡醒"发现结果到了。适合长测试/大构建。

### 操作
对 agent 说："后台跑 cargo test，同时我们继续看下一个问题"

### 示例
```
> 后台跑全量测试，先继续重构
（agent 发起 background bash，转去改代码）
…几分钟后…
<task_notification>task-7 exited 0</task_notification>
（agent 拿到通知汇报结果）
```

### 验证
- Technical Trace 显示后台任务 spawn/complete
- `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` 可禁用
- 环境变量 `NONOCLAW_MAX_TOOL_CONCURRENCY` 控制前台工具并发（默认 10）

---

## 10. 本体域（Ontology-MCP）

### 原理
自建项目（v0.19 期间落地）：把一个业务域建模成**声明式 YAML**，配一个 MCP 服务器做**运行时硬约束**。分工哲学：
- **Skill（`~/.nonoclaw/skills/ontology/`）负责建模**：教 LLM 怎么写领域 YAML、怎么验证
- **MCP 服务器（`~/.nonoclaw/ontology-mcp/server.py`，FastMCP stdio）负责确定性执行**：状态、规则、审计、审批——LLM 说了不算的地方

四个 MCP 工具：
| 工具 | 作用 |
|---|---|
| `query_ontology` | 查本体定义（对象/动作/规则） |
| `query_state` | 查实例状态（SQLite） |
| `execute_action` | 执行动作——经过规则门 |
| `approve_action` | 人审批后放行 |

核心机制：
- **领域 YAML**：objects（含 `key` 主键声明 / attributes / relations / states）、actions、rules、events、scenarios
- **主键回退链**：显式 `key` → 第一个 required 属性 → `id`/`<obj>_id` 约定 → 都没有就报错（`_key_of` 全声明式）
- **规则求值**：受限 AST 表达式（非图灵完备）——确定性规则跑在服务器，判断类的留给 LLM，边界清晰
- **聚合/增量/计算字段**：`aggregate` / `increments` / `computed` 声明式支持
- **审批门**：动作触发审批规则 → 实例置 `AWAITING_APPROVAL` 挂起 → LLM 用 AskUserQuestion 问人 → 人答了 → LLM 调 `approve_action` 放行。审批状态在引擎（MCP），同意行为在会话（AskUserQuestion）——两层解耦
- **幻觉防护**：LLM 想执行 YAML 里不存在的动作/对象 → 直接 `NOT_IN_ONTOLOGY` 拒绝
- **事件溯源**：所有变更记 JSONL（带 version tags），可审计可回放

### 操作
1. 已注册：`~/.nonoclaw/settings.json` → `mcpServers.ontology`（**改过配置必须重启 nonoclaw 服务器才生效**）
2. 测试域：`~/.nonoclaw/ontology-mcp/domains/order-fulfillment.yaml`（订单履约：4 对象/6 动作/4 规则/6 事件/2 场景）
3. 接真实域：仿照测试域写 YAML → 放 `domains/` → 对 agent 说用 ontology 工具操作

### 示例
```
"用 ontology 的 query_state 查所有状态为 待支付 的订单"
"执行 ship_order 动作，order_no=SO-001"   → 若库存不足，规则门拦截并返回原因
"给这个动作提权审批"                        → AWAITING_APPROVAL → AskUserQuestion → approve_action
```

### 验证
- `cd ~/.nonoclaw/ontology-mcp && python3 test_e2e.py`（14/14）+ `test_hardening.py`（13/13）
- 故意让 agent 执行不存在的动作 → 应看到 `NOT_IN_ONTOLOGY`
- 半途杀掉再重来 → 事件 JSONL 完整可回放

---

# 第二部分：机制类自动功能

---

## 11. Prompt 缓存体系（滚动断点）

### 原理
两级设计：
- **Block 1（缓存稳定前缀）**：身份+安全+工具列表——整个 run 字节级不变，provider 直接命中缓存。这就是为什么不能往 Block 1 塞动态内容（日期、git 状态都在 Block 2）
- **滚动 cache_control 断点（v0.19）**：每轮请求只给**最后一条消息**打 `cache_control` 标记（直接标在 Text/ToolUse/ToolResult 块上）。这样前缀部分能继续命中上一轮的缓存，只重写新增尾巴。Anthropic 限制最多 4 个断点——`enforce_cache_breakpoint_cap` 按 tools→system→messages 顺序去重合并，工具列表的标记最先被子sume

对 OpenAI 兼容供应商：Anthropic 风格标记会被剥掉（OpenAI 自动缓存无需标记）；`extra_body` 字段可注入供应商私有缓存提示（仅 OpenAI payload）。

### 操作
无需配置，自动生效（Anthropic 兼容端点：Anthropic/DeepSeek anthropic 路径/GLM anthropic 路径）。

### 示例
缓存友好的会话行为：
- 长会话里工具列表固定（autoSelectMcp 会话级固定就是为此）
- 技能正文按需加载，不塞前缀

### 验证
- Insight → Cache 面板（见第 21 章）：`cacheRead` 占比高 = 断点生效
- `--log-raw-api` 启动后翻 raw API 日志看 `cache_read_input_tokens` 逐轮增长

---

## 12. 上下文压缩分级（microCompact → autoCompact）

### 原理
四档递进，越早越便宜：
1. **microCompact（v0.19）**：每 turn 静默执行。旧工具结果 >2K 字符就裁成 头512+尾256，**最近 8 条消息字节级保护**（绝不裁正在干的活）。幂等（带 `[micro-compact]` 标记不重裁），与 8K pruner 互不重切（`[middle pruned]` 标记跳过）
2. **prune_tool_results**：压缩阈值触发后先试这个——>8K 的工具结果裁成 头4K+尾1K。**裁完若回到阈值以下，跳过 LLM 摘要**（省一次模型调用）
3. **80% 预压缩（pre-fire）**：后台异步起摘要（用 `compactModel`，独立便宜模型），不阻塞当前 turn
4. **100% 同步压缩**：阻塞式，摘要替换旧对话，**保留最近 3 轮原文**（KEEP_RECENT_TURNS=3）

摘要输出是结构化 XML：`<decisions>` `<files_modified>` `<commands_run>` `<current_state>` `<open_questions>`——压缩后 agent 不忘关键上下文。**只改内存投影，磁盘 transcript 永不动**。

### 操作
自动。可调：`compactThreshold`（触发阈值，默认 150K）、`compactModel`（摘要模型）、`compactMaxTokens`（摘要输出预算，默认 8192）、`autoCompact`（总开关）。

### 示例
UI 里辨认：技术追踪出现 `CompactionStarted`（automatic: true）；microCompact 触发时 Compacted 事件 `removed=0, pruned_results=N`。

### 验证
- 长工具会话（大量 cargo build 输出）��比：升级前后 autoCompact 触发时机明显推迟
- micro 后缓存仍部分命中（比全量摘要的全量重读便宜）——Cache 面板验证

---

## 13. 精确 BPE 分词 + 记忆预算分区

### 原理
- **Token 估算（v0.18+）**：内置 `tiktoken` 3.8.3 纯 Rust（带 rank 表，无运行时下载）——OpenAI/DeepSeek/Qwen/Kimi/GLM/Mistral/MiniMax 精确分词；Claude 启发式回退（每模型可调 `charsPerToken`）。压缩阈值判断、上下文预算都基于这个（旧版 chars/4 误差 20-30%）
- **记忆预算分区**：beads / facts / wiki / index 四个分区各有独立 token 上限（`contextBudget` 设置），互相不挤占——beads 再多也不会把 facts 挤出上下文
- 优先级：provider 上报的 `input_tokens`（最准）> BPE 估算 > 启发式

### 操作
settings.json：
```json
{
  "contextBudget": {
    "beads": 2000, "facts": 4000, "wiki": 2000, "index": 1000
  }
}
```

### 示例
GLM/Claude 类分词不准的模型给每模型 `charsPerToken` 覆盖全局。

### 验证
- Technical Trace 里 token 估算与实际 usage 对比（差距大就调 charsPerToken）
- 塞 100 个 bead 后看 facts 注入是否仍完整（分区隔离生效）

---

## 14. MCP 会话级工具选择

### 原理
MCP 服务器可能带几百个工具——全塞进 tools 数组会：① 烧 token ② **破坏缓存**（tools 数组字节变化 → Block 1 失效）。解法：
- **关键词相关性评分**：第一个用户消息 + 会话上下文打分，选 TOP K（默认 15）
- **会话固定（pinning）**：选定后整个会话不再变——tools 数组字节稳定，缓存命中保住
- 兜底策略 `mcpNoMatchPolicy`：`none`（不展示）/ `safe`（只展示 `mcpSafeTools` 白名单）/ `all`

### 操作
settings.json：`autoSelectMcp`（默认 true）、`autoSelectMcpTopK`（默认 15）、`mcpNoMatchPolicy`、`mcpSafeTools`。

### 示例
装了 ontology + 文件系统 + 数据库三个 MCP 服务器（共 80 工具）→ 聊订单业务 → 只有 ontology 的 4 个工具进上下文。

### 验证
- Insight → MCP Servers 面板看"advertised vs filtered"
- 会话中途 Cache 命中率不因 MCP 工具集变化而崩（pinning 生效）

---

## 15. Landlock 沙箱

### 原理
Linux 内核 LSM（5.13+）的强制访问控制——进程级文件系统权限收紧，用户态不可绕过。Bash 工具的兜底层：
- **workspace-write**：工作区可写，其他路径只读
- **read-only**：全只读
- 启动时探测内核支持，不支持（老内核/WSL1）优雅回退到常规权限检查

### 操作
随权限模式自动启用（read-only 相关模式），无需手动配置。可用 `--verbose` 启动看探测结果日志。

### 示例
plan 模式 + Landlock 双保险：即使提示词被诱导写文件，内核层直接 EACCES。

### 验证
`nonoclaw --verbose -p --permission-mode plan "尝试在 /tmp 写一个文件"` → 应看到拒绝；日志有 Landlock probe 结果。

---

# 第三部分：连接与集成

---

## 16. REST API

### 原理
`--serve-http` 的服务器同时暴露 REST——外部系统（CI/CD、webhook、脚本）无需 WebSocket 即可驱动 agent。权限/问题在无交互环境自动拒绝（安全兜底），要全自动用 `permissionMode: "auto"` 或 `bypassPermissions`。

端点：
| Method | Path | 用途 |
|---|---|---|
| POST | `/api/run` | 起 headless run，NDJSON 流返回事件 |
| POST | `/api/sessions/:id/cancel` | 取消运行 |
| GET/POST | `/api/sessions/:id/permissions[/request_id]` | 查看/审批待批权限 |

### 操作
```bash
curl -N http://127.0.0.1:8765/api/run \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"跑 cargo test 并总结失败","permissionMode":"auto","maxTurns":10}'
```

### 示例
CI 里挂 webhook 审批：agent 请求危险操作 → pending_permissions → 外部审批 UI 调 POST allow → agent 继续。

### 验证
返回 NDJSON 每行一个事件，最终 `done`（含 usage/turns）或 `error`（retryable 标志）。

---

## 17. ACP 协议（Zed 直连）

### 原理
Agent Client Protocol——编辑器直连 agent 的标准协议。`nonoclaw --acp` 以 stdio JSON-RPC 暴露 initialize/session/new/prompt/cancel。

### 操作
Zed 配置（settings.json）：
```json
{
  "agent_servers": {
    "NonoClaw": { "command": ["nonoclaw", "--acp"] }
  }
}
```

### 示例
Zed 里打开 agent panel 选 NonoClaw，直接在编辑器里对话，享受完整工具链。

### 验证
Zed agent panel 显示 NonoClaw 可选；prompt 后流式回包。

---

## 18. MCP Server 模式

### 原理
反转角色：NonoClaw 自己当 MCP 服务器，把内建工具暴露给**其他** agent 宿主。

### 操作
```bash
nonoclaw --mcp-serve        # stdio MCP server，暴露内建工具
nonoclaw --mcp-serve-memory # 只暴露 Mneme 记忆系统（facts/beads/wiki/goals）
```

### 示例
Claude Desktop / 其他 CLI agent 挂 NonoClaw 的记忆——它们也能跨会话记忆了（`--mcp-serve-memory` 是记忆即服务）。

### 验证
宿主侧工具列表出现 NonoClaw 工具；调用返回正常结果。

---

## 19. 手机 / 隧道

### 原理
- **QR 同步**：桌面 UI 生成带 token 的二维码 → 手机扫码进同一会话。协议是 revision 校验的：每条消息带 revision，旧 revision 被拒——两端不会错乱
- **`--tunnel`**：自动 spawn cloudflared，公网 HTTPS 访问。公网模式强制 token 鉴权（loopback 保持免密）

### 操作
```bash
nonoclaw --serve-http 127.0.0.1:8765 --tunnel
# 终端打印 Tunnel ready: https://xxx.trycloudflare.com?token=xxx
# 手机扫码即连
```

### 示例
出门后手机接续家里的任务；run 结束 MessagesLoaded 广播推到手机端 UI 自动更新。

### 验证
手机断网重连 → 快照重同步不丢消息；无 token 访问公网 URL 被拒。

---

## 20. 远程 JSON-lines 模式

### 原理
最原始的双终端模式：`--serve` 起 JSON-lines TCP 服务，`--remote` 连接发 prompt 收流。

### 操作
```bash
# 终端 1
nonoclaw --serve 127.0.0.1:8766
# 终端 2
nonoclaw --remote 127.0.0.1:8766 "检查项目"
```

### 示例
适合管道/脚本集成：echo JSON | nonoclaw --remote ...。

### 验证
终端 2 流式收到事件行直到 done。


---

# 第四部分：Insight 面板详解

---

## 21. Insight 完全解读

### 原理
Insight 是右侧可折叠手风琴栏——**所有数据来自权威 owner**（工具表来自 ToolRegistry、CLI 参数来自 Clap 定义、settings 字段来自 ResolvedConfig 元数据），不是手写文档，永不失真。它和 Technical Trace 面板、Status Bar 共用同一条结构化 RunEvent 流。

### 各面板速览

**① Technical Trace（技术追踪）**——每个 run 的完整事件时间线：
| 事件 | 含义 |
|---|---|
| stream state | SSE 流状态（连接/数据/断） |
| tool validation/permission/execution | 工具参数校验→权限门→执行三段 |
| retry | API 重试（带次数与原因） |
| compaction | 各档压缩触发（见第 12 章） |
| subagent | 子代理起止 |
| recovery | 孤儿修复等自愈动作 |
| usage | 每轮 token 消耗 |
| 终态 | 唯一终止原因 |

安全边界：提示词正文、API key、附件体、超大工具输出**不出服务器**或已脱敏。右上角可导出脱敏 trace（`GET /api/sessions/:id/trace`）——可以放心贴给别人 debug。

**② Cache（缓存命中率）**（v0.18+ 面板，v0.19 修正公式）：
- 显示条件：`cacheReadTokens + cacheWriteTokens > 0`
- **正确公式**：`base = inputTokens + cacheReadTokens + cacheWriteTokens`（分母三合一，v0.18 旧版漏了 write 项，v0.19 已修）
- 分段条三色：绿 Read（命中，免费/折扣价）/ 黄 Write（首次写缓存，略贵）/ 灰 Miss（全价）
- 供应商来源：Anthropic `cache_read_input_tokens`、DeepSeek `prompt_cache_hit_tokens`、OpenAI `cached_tokens`
- **怎么判断断点生效**：长会话稳定在 50%+ 绿色 = 滚动断点工作正常；每轮全灰 = 前缀被破坏（检查是否动态改了 Block 1 / tools 数组）
- Trace 里的缓存 chip：≥50% 绿 / ≥20% 黄 / 否则灰

**③ Tools**：注册表全量工具 + snippet（一句话说明）。行为提示在 prompt_guidelines，不占面板。

**④ MCP Servers**：连接状态 + 会话固定后的 advertised 子集 vs 全量。

**⑤ Models**：全部模型 profile + 角色（main/doc/compact）+ 当前激活。

**⑥ Skills**：已发现技能列表（点跳源文件，Shift+点 VS Code 打开）；空时提示往 `.nonoclaw/skills/` 丢 SKILL.md。

**⑦ Hooks / Plugins / Slash Commands / CLI Reference / Docs & Config / System / Project**：各自运行时事实（hooks 注册、斜杠命令表、CLI 参数、配置字段、环境、git 摘要）。

### Status Bar 与呼吸状态机
BreathController 把连接/run 事件映射为动画状态：
`idle → connecting → thinking → streaming → tool → waiting → compacting → subagent → success / error → reconnecting`
连续插值 + 节流 token 能量驱动；页面隐藏时暂停；`prefers-reduced-motion` 时只留文字状态。看到 aurora 变"compacting 色"= 正在压缩，别慌。

### 操作
点右上工具箱图标切换显隐；各手风琴独立展开。

### 示例
排障三连：Cache 面板（钱花哪了）→ Technical Trace（哪轮出问题）→ 导出脱敏 trace 给别人看。

### 验证
Insight 显示的工具数 == `nonoclaw --mcp-serve` 列出的（同源）；改动 settings 后 Docs & Config 面板刷新一致。

---

# 附录

---

## 22. 环境变量速查

| 变量 | 作用 | 默认 |
|---|---|---|
| `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` | API 接入 | — |
| `NONOCLAW_HOME` | 运行时根目录 | `~/.nonoclaw` |
| `NONOCLAW_MAX_TOOL_CONCURRENCY` | 前台工具并发上限 | 10 |
| `NONOCLAW_SUBAGENT_MAX_TURNS` | 子代理轮次上限 | 24（硬上限 200） |
| `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS` | 禁后台 Bash | 启用 |
| `SERPER_API_KEY` / `BRAVE_API_KEY` | WebSearch 后端 | — |
| `RUST_LOG` | 日志级别 debug/info/warn | info |
| `VECTOR_NOISE_FLOOR` | 向量噪声地板 | 0.1 |

---

## 23. 文件系统地图

```
~/.nonoclaw/                       ← 运行时根（NONOCLAW_HOME）
├── settings.json                  全局设置（models/mcpServers/permissions/dream*）
├── skills/<name>/SKILL.md         全局技能
├── ontology-mcp/                  本体域项目（server.py + domains/*.yaml + instances.db）
├── projects/<sanitized-cwd>/      每个工作区一个目录
│   ├── sessions/*.jsonl           会话记录（Layer 3 向量索引的原料）
│   ├── uploads/                   附件
│   ├── pending_permissions.json   待审批（重启可恢复）
│   └── .vector_index.json         facts 向量索引
└── agents/                        （可选）全局 profile

<project>/.nonoclaw/               ← 项目级
├── NONOCLAW.md / .local.md / rules/*.md   项目上下文（注入提示词）
├── SYSTEM.md / APPEND_SYSTEM.md           替换/追加系统提示词
├── agents/*.md                    Agent profile（verifier.md 等）
├── graphs/<name>.md               Agent 图定义
├── skills/<name>/SKILL.md         项目技能
├── hooks.json                     钩子
├── graphs 检查点 → .checkpoints/<name>.json（项目根）
└── last_dream.json                AutoDream 上次做梦时间戳

<project>/memory/                  ← Mneme（Git 友好）
├── facts/*.md                     不可变事实（supersedes 链）
├── beads/*.md                     任务连续性
├── goals/*.md                     目标
└── .session_index.json            会话向量索引（i8 量化 base64）

<project>/.nonoclaw/wiki/          ← LLM Wiki
├── WIKI.md / index.md / log.md
├── concepts/ entities/ comparisons/ decisions/ sources/
└── ../raw/                        不可变原始文档
```

**谁在写**：agent 写 facts/beads/goals/wiki；引擎写 sessions/索引/检查点/last_dream；你写 settings/skills/profiles/graphs/hooks。
