---
name: zen-provider-config
title: OpenCode Zen provider configuration for NonoClaw
type: architecture
importance: medium
confidence: high
tags: [provider, zen, opencode, billing, configuration]
---

## OpenCode Zen API

- **Base URL**: `https://opencode.ai/zen/v1`
- **API format**: OpenAI-compatible (`/v1/chat/completions`)
- **Auth**: `Authorization: Bearer <key>`
- **No balance API**: `/v1/credits`, `/v1/billing`, `/v1/balance` all return 404
- **Health check**: `/v1/models` returns model list if key is valid

## NonoClaw integration (2026-08-10)

- **Model**: `mimo-v2.5-free` (MiMo V2.5 Free, OpenAI format, contextWindow 131072)
- **billingProvider**: `zen`
- **ProviderBilling entry**: balanceUrl = `/v1/models` (used as health check)
- **parse_zen()** in `billing.rs`: counts models from `/v1/models` response, displays "Key valid · N models · Free tier"
- **base_url heuristic**: `opencode.ai` → `zen` in `model_provider()`

## Zen free models available
big-pickle, deepseek-v4-flash-free, mimo-v2.5-free, ling-3.0-flash-free, ling-3.0-tiny-free, nemotron-3-ultra-free, north-mini-code-free, laguna-s-2.1-free, longcat-2.0-free

Note: MiMo-V2.5 Free upstream was temporarily unavailable at config time ("Endpoint is unavailable"). big-pickle and deepseek-v4-flash-free confirmed working.
