---
id: autodream-fusion
title: AutoDream 融合方案（P0+P1+P2 全部完成）
status: done
priority: normal
created: 2026-08-18
updated: 2026-08-18
---

# P2 已实施（2026-08-18）
- `.nonoclaw/agents/verifier.md`：声明式 profile，零 Rust 改动（复用 AgentProfile 机制）
- system_prompt_override 全量替换提示词：对抗性纪律（结论必须有命令输出、
  不许读码写 PASS）、攻击向量清单（边界/并发/幂等/孤儿）、VERDICT 格式
  （PASS/FAIL/INCONCLUSIVE + 每个向量的命令与输出摘录）、只暴露不修 bug
- 工具隔离：tools_deny = Edit/Write/MultiEdit/NotebookEdit/WebFetch
  （Bash/Grep/Read 保留——必须能执行命令）；permission_mode 不设（继承父级，
  plan 模式会拒 Bash，实测后移除）
- 用法：Agent 工具带 profile: verifier
- 已验证：load_profile 解析 OK（override 含关键词、5 denies、mode=None），
  agents 12 测全过，全仓 390 过

# P1 已实施（2026-08-18）
- compact.rs 新增 `micro_compact()`：MICRO_THRESHOLD=2048 / HEAD=512 / TAIL=256 /
  PROTECT_RECENT=8（最近 8 条消息永不裁剪）；跳过已带 MICRO/PRUNE 标记的（幂等）；
  不碰 persisted transcript（同 prune 语义，仅内存投影）
- loop_.rs：auto_compact 分支开头每 turn 先跑 micro_compact，裁剪>0 时清
  last_input_tokens + 发 Compacted 事件（removed=0, pruned_results=N）
- 缓存语义：只裁旧 turn，滚动末消息断点仍有效；比 80% 触发的全量摘要重读便宜
- 测试：4 个新单测（保护窗口/幂等/跳过 PRUNE 标记/短历史 noop），全仓 390 过
- 已装二进制 32556880 字节（8月18 10:48）

# P0 已实施（2026-08-18）
- `rust/crates/cli/src/serve_http/dream.rs`：DreamScheduler——每分钟检查 4 条件
  （idle ≥ dreamIdleMinutes / 无 pending permissions+questions+bg tasks /
  session fingerprint 变化 / dreamEnabled 默认 true），全满足则起 dream run
- dream run 走 run_api::run_handler_for_dream（新拆的编程入口，复用 REST 全链路），
  固定四阶段 prompt（session_search→关联→萃取→写 memory/facts，最多 3 条事实），
  max_turns=16, permission_mode=auto
- run 结束后 build_index 刷新 session 向量索引 + 写 last_dream.json 标记
- AppState 加 last_activity（WS 每条消息 + REST run touch）
- settings 新增 dreamEnabled / dreamIdleMinutes（默认 true / 10）
- 测试：383 全过 + dream 3 单测（fingerprint 幂等/非 jsonl 忽略/prompt 四阶段）
- 已安装 ~/.local/bin/nonoclaw（32548304 字节 8月18 10:21）——**需重启服务器生效**
- 验证方法：重启后闲置 10 分钟，看 log "dream run started/finished" +
  memory/facts/ 新条目

# 背景（2026-08-18 调研）
Claude Code 51 万行源码泄露（2026-03）曝光的 AutoDream/Dream Memory + KAIROS 体系，
即 "AI Engineer World's Fair Anthropic 30 分钟 dreaming AI" 演讲内容。
来源：CSDN 万字解析（已全文抓取 /tmp/csdn_d.txt，62KB）。

# AutoDream 原始设计（供实现参考）
- 触发条件（全部满足）：距上次使用 ≥ N 分钟；累计足够新会话；无任务运行；非紧急
- 四阶段 REM：碎片收集(对话/代码变更/反馈) → 关联分析 → 知识萃取(结构化知识点)
  → 记忆索引(向量库)
- KAIROS：磁盘级持久后台模式（不搬，单机场景 P0 已覆盖）

# 实施计划（逐个做，P0 → P2）

## P0 — dream 后台任务（最贴合，基建 80% 就位）
- serve_http 加 idle watcher：run 结束 + 无活跃会话 + 距上次活动 > N 分钟 → 触发
- 起一个 headless run（复用 REST API POST /api/run），固定 prompt 四阶段：
  session_search 捞近期会话碎片 → 关联分析 → 萃取 → 写 memory/facts/（走 persist）
  + 触发 session_index 增量刷新
- 效果：Mneme 三层记忆从"被动记录"升级"主动整理"（KAIROS 最小版）
- 预估 ~200 行

## P1 — microCompact 缓存感知微压缩
- compact.rs 的 80% 预触发前加轻量级清理：定向截断旧 Read/Bash/Grep 大块输出，
  保 system/messages 前缀 hash 不变（不破坏滚动 cache_control 断点收益）
- 原方案分级：applyToolResultBudget → snipCompact → microCompact → contextCollapse
  → autoCompact(剩 13k 才触发 + 连续 3 次失败断路器)
- NonoClaw 现状：两阶段(80%/100%) + KEEP_RECENT_TURNS=3，无轻量级
- 挂接点：EngineCache 失效机制

## P2 — Verification Agent（对抗性验证）
- agents.rs profile 体系加 verifier profile
- 核心 prompt（泄露源码原文精神）："你的任务不是确认实现能工作，而是想方设法搞崩它。
  并发测试、边界值、幂等性、孤儿操作。所有结论必须有实际执行的命令输出，不许读码猜结果。"
- 已知失败模式自省：逃避验证（读码+写 PASS）；被前 80% 迷惑
- 工具隔离：复用 subagent registry 剥离模式（Edit/Write/Agent 禁用，只读+执行）

# NonoClaw 已有 vs 缺口（调研结论）
| 要素 | 已有 | 缺口 |
|---|---|---|
| 碎片收集 | session JSONL + session_index(L3 向量) | 无跨会话蒸馏 |
| 关联/萃取 | 仅单会话 compact 摘要 | 无离线萃取 |
| 记忆索引 | 256维 trigram + BM25 混合 | 萃取产物未入索引 |
| 空闲触发 | 无 | idle 检测 |
| 分级压缩 | 两阶段 | microCompact |

# 验证标准
- P0: 人离机后 log 出现 dream run；memory/facts/ 出现萃取产物；session_index 增量刷新
- P1: micro 后 prefix cache 命中不降（对比 raw-api log 的 cache_read）；autoCompact 推迟
- P2: 给错误实现的任务，verifier 能产出失败证据（命令输出），而非"看起来没问题"
