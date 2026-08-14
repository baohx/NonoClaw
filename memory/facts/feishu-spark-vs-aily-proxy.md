---
name: feishu-spark-vs-aily-proxy
title: 飞书 AI proxy 的妙搭(Spark)与 Aily 是两套 API，app_ 前缀是妙搭应用
type: bug
importance: 0.95
confidence: 0.95
tags: [feishu, aily, spark, miaoda, lark-proxy, lark-cli, app_id, agent_id]
supersedes: null
---

2026-08-13 修复「飞书 Aily AI 模型返回空」的根因。核心结论：飞书「Aily」智能体与「妙搭(Spark/Miaoda)」应用是**两套不同产品**，API 与 ID 格式完全不同，proxy 曾混用导致返回空。

## ID 格式三套（勿混淆）
- 妙搭/Spark 应用：`app_xxx`（如 `app_4jrru2wv6ptbb`），走 `/open-apis/spark/v1/...`
- Aily 应用：`spring_xxx__c`，走 `/open-apis/aily/v1/apps/:app_id/...`
- Aily 智能体：`agent_xxx`，走 `/open-apis/aily/v1/agents/:agent_id/chats`（新版 SDK master）
- lark-cli 自身鉴权 app：`cli_xxx`（`auth status` 的 `appId`），**绝不能**传给 apps 命令

## 妙搭(Spark)对话 API（异步轮询模型，已实测可用）
1. `POST /open-apis/spark/v1/apps/{app_id}/sessions/{sid}/chat` body `{"message": "..."}` → 异步入队立即返回空
2. `GET  /open-apis/spark/v1/apps/{app_id}/sessions/{sid}` → 轮询 `latest_turn.status`（running→completed/failed），拿 `turn_id`
3. `GET  /open-apis/spark/v1/apps/{app_id}/sessions/{sid}/turns/{turn_id}/reply_message` → 回复在 `data.messages[]`（`role=="assistant"` 的 `content`）

对应 lark-cli 命令：`lark-cli apps +session-create` → `+chat` → `+session-get` → `+session-messages-list`。用途是「云端 Agent 生成/迭代应用」，但也能当 AI 助手对话（实测返回"我是妙搭，你的 OpenClaw 开发助手"）。

## 关键坑
- `POST /open-apis/spark/v1/apps/{app_id}/sessions`（session 创建）实测返回 `{"code":0,"data":{},"msg":"[ErrInvalidParam]"}`，**不返回 session_id**，也不创建可见 session。lark-cli 的 `+session-create` 同样返回空 data（把 code=0 当成功掩盖了 msg 错误）。所以 proxy 只能**复用已有 session**（GET sessions 列表取第一个 `session_id`，如 `conversation_4jrruya78qah2`）。
- Aily 旧版 session API（lark-cli 内嵌）：`POST /open-apis/aily/v1/sessions`（body 只有 `channel_context`/`metadata`，**无 app_id**，传了也被忽略→session 孤立）；`POST .../messages` 报 "field validation failed"；正确绑定 app 要靠 `POST .../runs`（body 带 `app_id: spring_xxx__c`）。这条链路对 `app_` 前缀的妙搭 app 完全不可用。
- lark-cli 无 aily/spark/agent 的 typed service（`lark-cli schema aily` → Unknown service），只能用 `lark-cli api` raw 模式。

## proxy 修复要点（/home/baohx/.nonoclaw/lark-proxy/lark-ai-proxy.py）
- 原 `aily_chat` 改用 Spark 会话流程（`spark_get_session` 复用已有 session + `spark_chat` chat→轮询→reply_message）。
- model 名仍用 `lark-aily:app_4jrru2wv6ptbb`（`:` 后是 app_id，settings.json 无需改）。
- 轮询参数：timeout 180s / poll_interval 3s（简单问题实测 5~11s 完成）。
