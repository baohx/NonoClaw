---
name: tokenizer-and-cache-architecture
title: v0.17.1 真实 BPE tokenizer + 记忆向量库 + OpenAI 缓存策略 + 记忆预算分区
type: architecture
importance: 0.9
confidence: 0.9
tags: [tokens, tokenizer, tiktoken, vector-store, memory, cache, budget, openai, deepseek]
supersedes: null
---

2026-08-11 实施的五个互相关联的架构变更（release v0.17.1）：

## 1. 真实 BPE tokenizer（tokens.rs）
- 引入纯 Rust `tiktoken = "3.8.3"`（workspace 依赖，内置全部 rank 表，无运行时下载）。
- `tokens.rs` 新增 `encoding_for_model()`（薄包装 tiktoken）+ `count_text_tokens()` +
  `estimate_message_tokens_for_model()` + `estimate_total_for_model()`。
- 对已知模型族（OpenAI/DeepSeek/Qwen/Kimi/GLM/Mistral/MiniMax）给出**精确** BPE 计数；
  未知模型（如 Claude，无公开 tokenizer）回退到启发式（prose 4 / code 3 chars-per-token）。
- engine 两处估算调用点（ContextPrepared 事件 + 首轮估算）已切换到
  `estimate_total_for_model(Some(&model), ...)`。provider 报告的 input_tokens 仍是权威信号。

## 2. 记忆向量库检索（tools/memory.rs）
- 零依赖本地向量库：字符 trigram 特征哈希（FNV-1a 稳定哈希）→ 256 维 ±1 符号向量 → L2 归一化。
- `VECTOR_NOISE_FLOOR = 0.1`：256 维下无关文本余弦 ~0.06，真实 trigram 重叠 ~0.46；
  noise floor 过滤纯噪声命中（实测验证于 tools/tests/vec_probe.rs）。
- 持久化索引 `.nonoclaw/memory/.vector_index.json`（含每个 fact 的 content_hash），
  `load_or_build_vector_index()` 按内容哈希失效重建。
- `search_facts()` 升级为混合排序：cosine×2 + BM25 词法，importance 打破平局；
  Memory 工具 search 动作走持久化索引路径。

## 3. 缓存命中率可视化（frontend）
- InsightRail 新增 Cache 区块：命中率百分比 + 分段条（hit/write/miss）+ 图例。
  数据来自 store 累计的 inputTokens/cacheReadTokens/cacheWriteTokens
  （`hit_rate = cache_read / input_tokens`，因 Anthropic/OpenAI 的 input 已含缓存部分）。
- TechnicalTrace 摘要新增 cache 命中率芯片。

## 4. OpenAI 兼容模型缓存策略（api/client.rs）
- `OpenAiPromptTokenDetails` 解析 DeepSeek `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`，
  `cache_read_tokens()` = max(cached_tokens, prompt_cache_hit_tokens)。
- `ApiFormat::OpenAI` 的 `cache_usage` 能力改为 **Supported**（prompt_caching 仍 Unsupported：
  Chat Completions 无 cache_control 线上字段，依赖 provider 自动前缀缓存）。
- `RequestParams.extra_body`（仅注入 OpenAI 载荷，用于 provider 缓存提示如 `prompt_cache`）。

## 5. 记忆独立预算分区（budget.rs + context.rs）
- contextBudget 新增 `memoryBeadsTokens` / `memoryFactsTokens` / `memoryWikiTokens` /
  `memoryIndexTokens` 四个独立分区，替代原先单一 memoryTokens 内的硬编码分配。
- standard 默认：beads 2500 / facts 4000 / wiki 3000 / index 3000（合计 = memoryTokens 12500）；
  ultra 各 100。
- `load_memory_prompt_with_partitions()` 按分区独立渲染（大 wiki 索引不再挤占 facts）；
  `render_memory_context` 重构为 `render_memory_partitioned(beads_max, facts_max)`。
- legacy 单限额包装按 20%/20%/10%/50% 拆分（与旧 20K/20K/5K/25K 分配一致）。

## 注意事项
- loop_.rs 的 `apply_cache_breakpoints`（Block 2a 缓存拆分）是先前会话的未提交改动，
  与本会话功能无冲突，保持原样。
- 全量测试：Rust 5 crates 全部通过（core 52 / api 26 / tools 60+3 / engine 198 / cli 60），
  frontend tsc + build + transition/websocket/breath 测试通过。
