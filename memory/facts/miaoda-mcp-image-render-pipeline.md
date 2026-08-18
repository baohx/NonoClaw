---
name: miaoda-mcp-image-render-pipeline
title: 妙搭 MCP 生成图片并在 NonoClaw WebUI 直接渲染的完整流程
type: convention
importance: high
confidence: high
tags: [miaoda, mcp, image-generation, webui, markdown, freeimage]
---

# 妙搭 MCP → 公共图床 → NonoClaw WebUI 图片渲染流程（2026-08-15 验证通过）

## 完整工作流程

1. **确认应用**：`mcp__miaoda__spark_apps` 列出可用应用（用「鲍鸿鑫的 OpenClaw」app_4jrru2wv6ptbb）
2. **发送生成请求**：`mcp__miaoda__spark_chat`，消息中明确要求"生成后给出图片的完整可访问 URL（https 开头的绝对路径）"
   - ⚠️ 图片生成耗时较长，MCP 调用可能超时（timeout 报错）。**超时后重发一次**，并在消息中说明"如果之前的请求已在处理，请直接返回上一张生成的图片结果"——妙搭会话有跨轮记忆，云端已完成生成，重试即拿到 URL
3. **验证 URL**：`curl -sI <url>` 确认 200 + image/jpeg + 无需登录（freeimage.host 返回 access-control-allow-origin: *，缓存至 2037 年）
4. **渲染**：在回复中使用标准 markdown 语法 `![描述](https://...)` —— NonoClaw WebUI 完整渲染外部图片，大小随组件自动缩放

## 关键结论

- **NonoClaw WebUI 支持标准 markdown 图片语法渲染外部 HTTPS URL**（与妙搭对话界面不同，后者会转义 HTML 且不渲染外部 URL）
- **公共图床选 freeimage.host**：无需登录、永久链接、CORS 全开
- 妙搭内部存储相对路径（/spark/app/...）无法外部访问（302 到登录页）；之前用 lark-cli file-upload + file-sign 得到的签名 CDN URL 也是可行方案，但走妙搭 MCP + 公共图床更简单

## 反面教训（历史尝试中失败的）

- `<img src="...">` HTML 标签在妙搭对话界面被转义为文本
- `![](https://...)` 在妙搭对话界面只显示占位符
- 妙搭 storage 的相对/绝对路径均需认证
