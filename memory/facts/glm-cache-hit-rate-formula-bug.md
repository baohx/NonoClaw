---
name: glm-cache-hit-rate-formula-bug
title: GLM「恒 100% 缓存命中率」根因 — Anthropic input_tokens 不含缓存，除法基数错误
type: bug
importance: 0.9
confidence: 0.95
tags: [cache, glm, anthropic, cache_read_input_tokens, hit-rate, usage, frontend]
supersedes: null
---

2026-08-16 用 `--log-raw-api` 抓到 GLM-5.3 真实流量后定位「GLM 恒 100% 命中率」的最终根因并修复。

## 真实流量证据（~/my-wiki/.nonoclaw/logs/api/*.resp.sse）
- GLM `/api/anthropic` 用**标准 Anthropic 字段名**：`message_delta.usage = {input_tokens: 116380, output_tokens: 196, cache_read_input_tokens: 9216}`。首轮冷缓存，命中率 7.9%（9216/(116380+9216)），数据本身完全正常。
- 历史 session（`~/.nonoclaw/projects/home-baohx-my-wiki/sessions/8af55705*.jsonl`）的 `cumulative_usage`：`cache_read=15,190,720` vs `input=2,855,385` → 比值 **532%**，物理不可能。

## 根因：公式语义错误（不是字段名，也不是重复累加）
- **Anthropic 语义**：`input_tokens` **不含** `cache_read_input_tokens` / `cache_creation_input_tokens`（三者是兄弟字段，计费上 cache_read 部分走折扣价）。真实基数 = `input + cache_read + cache_write`。
- 旧公式 `cache_read / input`：命中率越高 → 未缓存部分越小 → 比值越���（87%→532%）。前端 `Math.min(cacheRead, input)` 把 >100% 钳到 100% → 「GLM 恒 100%」假象。
- **OpenAI 语义**：`prompt_tokens` **已包含** cached tokens（hit+miss==prompt_tokens），基数就是 `prompt_tokens` 本身，再求和会重复计数。
- 排除项：累计是线性累加（今天增量 +9216 与原始日志一致），不是指数/双重累加；GLM 字段名标准，不需要别名。

## 修复（2026-08-16，v0.18.x 工作树）
1. `api/client.rs`：`usage_json_with_base()` 引入显式基数。Anthropic 摘要传 `input+cache_read+cache_write`，OpenAI 摘要传 `input`。回归测试 `cache_hit_rate_uses_billable_base_per_provider_semantics` 用真实 GLM 数字锁定。
2. `frontend/TechnicalTrace.tsx`：`cacheHitRateOf()` 改为 sum 基数，删除 `Math.min` 钳制。
3. 注意：**不要**把 `input_tokens` 改成含缓存的总量 —— billing/预算用 input_tokens 走标准计费表（cache_read 单独折扣价），只有**命中率展示**需要 sum 基数。

## 遗留观察（2026-08-17 已实现滚动断点修复）
- GLM 首轮命中率仅 7.9% 的原因已定位并修复：消息历史完全无 cache_control 断点。两层根因：
  1. `apply_cache_breakpoints` 只给 **Text 尾块**打标记，而 agent 对话 91% 的消息以 tool_result/tool_use 结尾 → 标记从未落地（302 条消息 0 断点）。
  2. 旧策略用 50%/75% 两个断点，但 Anthropic 上限 4 个/请求，system(2-3) + tools(1) 已占满。
- 修复（2026-08-17）：(a) `ContentBlock::ToolUse/ToolResult` 新增 `cache_control` 字段（serde default + skip，wire 兼容）；(b) engine 改为**滚动断点**——只标记最后一条消息的最后一个块（Anthropic 官方推荐模式，本轮发送的全部前缀成为下一轮的缓存命中）；(c) `serialize_body_anthropic` 增加 `enforce_cache_breakpoint_cap`：按 tools→system→messages 前缀序从最旧开始丢断点（tools 断点被任何 system 断点完全覆盖，最先丢），硬上限 4。
- OpenAI 兼容接口评估结论：**无需修改**。DeepSeek/GPT/vLLM 走自动前缀缓存（无请求侧断点），序列化器已忽略 cache_control（新增守卫测试 `openai_serializer_strips_anthropic_cache_markers_from_tool_blocks`），`extra_body` 的 `prompt_cache` 提示机制已存在。
- 关键认知：**cache_control 标记不参与前缀哈希**——每轮移动断点位置不会破坏前缀匹配，这是官方多轮对话示例的标准做法。空文本合成块方案被否决（会污染前缀）。
- DeepSeek OpenAI 格式的 `prompt_cache_hit_tokens` 修复见 [[deepseek-cache-field-position-and-raw-api-log]]。
