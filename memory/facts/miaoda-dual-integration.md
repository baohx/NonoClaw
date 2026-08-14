---
name: miaoda-dual-integration
title: 妙搭双集成方案 — 伪模型(方案B) + MCP工具(方案A)
type: architecture
importance: high
confidence: high
tags: [feishu, miaoda, aily, proxy, mcp, tool-calling, architecture]
supersedes: []
---

# 妙搭(Miaoda/Spark) 双集成方案

妙搭 Spark API 原生不支持 function calling（`/chat` 只接受纯文本 message，轮询
`latest_turn.status` 取回复）。因此有两种集成路径（2026-08-14 均已实现）：

## 方案 A：妙搭当「工具」（推荐，用于其它主模型）
- 文件：`~/.nonoclaw/mcp-proxies/miaoda-mcp.py`（stdio MCP server，复用 lark-cli）
- 已注册进 `~/.nonoclaw/settings.json` 的 `mcpServers.miaoda`：
  `{"command":"python3","args":[".../miaoda-mcp.py"]}`
- 暴露 7 个工具：`spark_apps`（列应用）、`spark_chat`（对话，按 app_id 复用
  session 有跨轮记忆）、`spark_sessions`、`minutes_search`、`minutes_detail`、
  `docs_fetch`、`raw_api`（通用飞书 OpenAPI 代理）。
- 主模型（deepseek-v4-pro / glm-5.2 等原生 function-calling）可调用这些工具，
  让妙搭的飞书侧能力成为 agent 工具箱的一员，与 Read/Bash 平级。
- **注意**：MCP 在 nonoclaw 启动时注册（cli/main.rs:296），改 settings.json 后
  必须重启 nonoclaw 服务才能加载；`config.reload()`（project_service.rs:86）
  不会重建 MCP 客户端。

## 方案 B：妙搭当「伪模型」（proxy 模拟 function calling）
- 文件：`~/.nonoclaw/lark-proxy/lark-ai-proxy.py`
- 核心机制：请求带 `tools` 数组时启用「协议模式」——
  1. `render_tool_catalog(tools)` 把 OpenAI tools 渲染成文本工具清单注入 system
     （≤40 个工具，desc ≤160 字符）；
  2. `render_script(messages, system)` 只注入工具轨迹（assistant tool_calls +
     tool 结果，各截断 4000 字符）+ 最后一条 user 消息（普通历史靠妙搭 session
     记忆，不重复发送）；
  3. 妙搭如需调工具，回复中输出
     `[[tool_call]]{"name":"X","arguments":{...}}[[/tool_call]]`；
  4. `parse_tool_calls(reply)` 解析协议块 → `make_openai_response(..., tool_calls=)`
     finish_reason="tool_calls"；
  5. `_send_sse_openai(..., tool_calls=)` 按 OpenAI 格式流化 delta（id+name 一帧、
     arguments 一帧、finish_reason="tool_calls" 终结），NonoClaw 引擎照常执行本地
     工具，下一轮把 tool 结果带回，proxy 转成 [工具结果] 回填，维持剧本连续。
- 无 tools 的请求保持纯文本行为（兼容旧用法）。Anthropic 兼容路径不传 tools。
- 已知局限：自由文本 JSON 有解析失败率（容错：忽略非法块继续纯文本）；妙搭可能
  不遵守协议直接回答（无害，等同普通对话）。

## 关联 bug
`model_info` 回传 model 名截断问题（lark-aily vs lark-aily:app_xxx）已修复：
proxy 保留 `response_model` 完整名；前端 websocket.ts 仅当 model 名存在于
availableModels 才 setModel。见 fact `aily-proxy-model-name-truncation-bug`。
