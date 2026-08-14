---
name: aily-proxy-model-name-truncation-bug
title: lark-ai-proxy 截断 model 名导致 NonoClaw 报 "model client configuration is invalid"
type: bug
importance: high
confidence: high
tags: [feishu, aily, proxy, model, frontend, websocket]
supersedes: []
---

# Bug：Aily proxy 截断 model 名 → 前端 model 选择器被污染 → 下次运行报配置错误

## 现象
用户用「飞书 Aily AI」（profile `lark-aily:app_4jrru2wv6ptbb`）提问后，对话框 model
下拉框自动显示成第一项 "DeepSeek V4 Pro"；不切换直接继续提问报错：
`Error: model client configuration is invalid`。

## 根因（三层联动）
1. **`~/.nonoclaw/lark-proxy/lark-ai-proxy.py`**（非仓库文件）`handle_chat_completions`
   把请求 model `lark-aily:app_xxx` 按 `:` 拆成 `lark-aily` + app_id，响应（含 SSE）
   却用**截断后的** `lark-aily` 作为 model 字段回传。
2. **NonoClaw 后端**把 API 响应的 model 字段经 `StreamEvent::MessageStart.model`
   → `RunEvent::ModelInfo` 发给前端（loop_.rs forward_stream_event）。
3. **前端** `frontend/src/websocket.ts` `case "model_info"` 无条件
   `state.setModel(event.model)`，store.model 被污染成 `lark-aily`（不在
   availableModels）。React `<select value=...>` 无匹配 option 时浏览器显示**第一项**
   （deepseek-v4-pro label "DeepSeek V4 Pro"），下次提交仍用 store.model=`lark-aily`，
   后端 `client_for()` 找不到 profile → 回退 ANTHROPIC 环境变量（未配）→ Err。

## 修复（2026-08-14，双管齐下）
- **proxy**：保留 `response_model = 原始完整 model 名`，所有 `make_openai_response`
  调用改用 `response_model`（含 app 后缀）；SSE 从 result.model 取，自动正确。
- **前端** `websocket.ts`：`model_info` 仅在
  `state.availableModels.some(m => m.name === event.model)` 时才 setModel，
  防止任何 proxy/gateway 回传别名/截断名污染选择器。

## 关键代码位置
- `~/.nonoclaw/lark-proxy/lark-ai-proxy.py:290-298`（拆分 model）、`:304-351`（响应）
- `rust/crates/cli/src/serve_http/connection.rs:1257-1276`（client_for 报错点）
- `frontend/src/websocket.ts:311-319`（model_info 防御）
- `rust/crates/engine/src/loop_.rs:2785-2796`（ModelInfo 事件来源）
