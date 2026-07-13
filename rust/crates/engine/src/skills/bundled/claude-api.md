---
name: claude-api
description: Reference for the Claude API / Anthropic SDK — model ids, pricing, params, streaming, tool use, MCP, agents, caching, token counting, model migration.
when_to_use: when the user asks about Claude API, Anthropic SDK, model pricing, token limits, or API parameters
paths:
  - "**/anthropic*"
  - "**/claude*"
---

# Claude API Reference

Reference for the Claude API and Anthropic SDK.

## Model IDs
- `claude-sonnet-4-5-20250929` — Latest Sonnet (fast, capable)
- `claude-opus-4-8` — Latest Opus (most capable)
- `claude-haiku-4-5-20251001` — Latest Haiku (fastest)

## Key parameters
- `model` — model ID string
- `max_tokens` — maximum output tokens
- `system` — system prompt (string or array of blocks)
- `messages` — conversation history (user/assistant roles)
- `tools` — tool definitions
- `thinking` — extended thinking config (`{type: "enabled", budget_tokens: N}`)
- `temperature` — sampling temperature (0-1)

## Streaming (SSE)
Events: `message_start`, `content_block_start`, `content_block_delta`, `message_delta`, `message_stop`

## Prompt Caching
Add `cache_control: {type: "ephemeral"}` to system blocks and tool definitions. Minimum 1024 tokens per cache breakpoint.

## Token counting
- Input: all messages + system + tools
- Output: generated text + tool use JSON
- Cache read: tokens served from cache (discounted)
- Cache write: tokens written to cache (higher cost)
