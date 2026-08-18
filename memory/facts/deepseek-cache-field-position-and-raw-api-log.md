---
name: deepseek-cache-field-position-and-raw-api-log
title: DeepSeek 缓存命中率解析修复（字段层级）+ --log-raw-api 无脱敏日志开关
type: bug
importance: 0.9
confidence: 0.85
tags: [cache, deepseek, glm, prompt_cache_hit_tokens, raw-api-log, redaction, cli]
supersedes: null
---

2026-08-16 修复「DeepSeek 缓存命中率从来不对」+ 补全「查看真实（未脱敏）数据的 CLI 开关」。

## 根因 1：DeepSeek 缓存命中字段在错误的 JSON 层级（OpenAI 路径）
- `api/src/client.rs` 的 `OpenAiPromptTokenDetails` 原本把 `prompt_cache_hit_tokens` /
  `prompt_cache_miss_tokens` 放在 `prompt_tokens_details` **内层**（DeepSeek 原生 OpenAI
  接口实际上把这两个字段放在 `usage` **顶层**，是 `prompt_tokens_details` 的兄弟，且
  hit + miss == prompt_tokens）。
- 修复：把 `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens` 上移到 `OpenAiUsage` 顶层；
  `OpenAiPromptTokenDetails` 只保留 OpenAI/vLLM 的 `cached_tokens`。`cache_read_tokens()`
  改为 `max(cached_tokens, prompt_cache_hit_tokens)`。
- 这是「DeepSeek 缓存量从来不对」的代码级根因（OpenAI 格式路径）。

## 根因 2：Anthropic 路径不认 DeepSeek 原生字段名（UsagePart 别名）
- 用户 settings.json 里 deepseek/glm 都走 `/anthropic` 端点（Anthropic 格式，无 apiFormat
  显式声明），走的是 `fold_anthropic_stream` + `core/src/usage.rs::UsagePart` 反序列化。
- `UsagePart.cache_read_input_tokens` 只认 Anthropic 标准名；DeepSeek 的
  Anthropic 兼容端点若透传原生 `prompt_cache_hit_tokens`，会被静默丢弃 → cache_read 恒 0。
- 修复：给 `cache_read_input_tokens` 加 `#[serde(alias = "prompt_cache_hit_tokens")]`。
  语义等价（都是"从 provider 缓存命中的 token 数"）。`cache_creation_input_tokens` 不加别名
  （DeepSeek 的 miss 是"未命中量"，不是"新写缓存量"，语义不等价）。

## 交付：--log-raw-api（查看真实未脱敏数据的 CLI 开关）
- `cli/src/main.rs` 已有 `--log-raw-api` flag，启动时设 `NONOCLAW_RAW_API_LOG=1`。
- `api/src/client.rs` 补全 `RawApiLogger`：`build_request` 写入完整请求体
  （`.nonoclaw/logs/api/<ms>-<trace>.request.json`，含完整 prompt），流式 fold 追加原始
  SSE 帧（`.resp.sse`），每轮结束写 `.summary.json`（含 `cache_hit_rate_pct`）。
- 目录权限 0700，API key 永不落盘（只在 header）。**这是用户要的"看真实数据"渠道**：
  用它诊断 GLM 为何恒 100%（看 provider 到底报了 `cache_read_input_tokens` 还是
  `prompt_cache_hit_tokens`，数值是否 == input）。

## 附带修复：测试里的目录删除地雷
- 未提交的 `raw_api_logger_*` 测试原 `DirGuard(original_cwd)` 在 Drop 时
  `remove_dir_all(original_cwd)` —— 会把**整个 crate 目录删掉**（表现为 `cargo test`
  后 `rust/crates/api/` 神秘消失、`include_str!` 报 fixture 缺失）。
- 修复：guard 持有 `{ original, tmp }`，Drop 时恢复 cwd 到 original、只删 tmp。
- 另：`MessageContent::from_text("...".into())` 因 `.into()` 目标类型歧义需写纯 `&str`。

## 验证
- `cargo test --all`：379 passed / 1 ignored（core 21、api 27、engine 202、tools 67+3、cli 58+1）。
- `cargo check --all` 通过。api crate 目录在测试后完好。

## 待办（需原始日志确认）
- GLM「恒 100%」：GLM `/api/anthropic` 端点报告的确切 usage 字段/数值仍需
  `--log-raw-api` 抓取确认（可能是 provider 报 cache_read==input，或前端累计口径问题）。
  拿到 `.resp.sse` 后再做针对性修复，避免猜。
