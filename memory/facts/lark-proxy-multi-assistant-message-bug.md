---
name: lark-proxy-multi-assistant-message-bug
title: lark-ai-proxy 只回传第一条 assistant 消息 → 飞书多句回复只显示第一句
type: bug
importance: high
confidence: high
tags: [feishu, lark-proxy, spark, miaoda, sse, streaming]
supersedes: []
---

# Bug：lark-ai-proxy 只取第一条 assistant 消息，多句回复丢失

## 现象
用户用飞书模型（lark-aily:app_xxx）聊天，对方回复多句时 WebUI 只显示第一句就结束。proxy.log 特征：`finish=stop content_len=26/35` 但耗时 129~142 秒（模型实际生成了完整回复）。

## 根因
`~/.nonoclaw/lark-proxy/lark-ai-proxy.py` `spark_chat()` 取 reply_message 时循环找到**第一条**非空 assistant 消息就 return。而妙搭(Spark)一个 turn 的 reply_message 会返回**多条** assistant 消息：多句叙述 + 工具调用间的占位（len=0 被跳过）+ 最终总结。实测 turn 7673898734316784568 有 30 条消息：[0] 叙述(37字符) … [28] 最终总结(8407字符)，proxy 只转发了 [0]。

## 修复（2026-08-15）
spark_chat() 改为按顺序拼接全部非空 assistant 消息（`"\n\n".join(parts)`）。

## 关键点
- 修复后 turn 末尾的"工具调用次数超过最大次数限制，请回复'继续任务'"提示（role=assistant）也能送达，多轮工具任务不再静默卡死。
- **重启 proxy 陷阱**：`start.sh` 以 /health 存活即跳过，改完代码必须 `kill $(cat proxy.pid)` 再跑 start.sh，否则跑的还是旧代码。
- 验证方法：curl POST /v1/chat/completions stream=true，让模型"分三句话介绍自己"，拼接 SSE content 长度应为数百字符而非单句。

## 相关
- [[aily-proxy-model-name-truncation-bug]]（同文件的另一个已修 bug）
