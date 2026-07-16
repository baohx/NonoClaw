# NonoClaw [English/中文]

A **Rust rewrite** of [Claude Code](https://claude.ai/code) (Anthropic's agent CLI). Full agentic loop, tool dispatch, permission system, session persistence, MCP client/server, a **Web UI** with PWA, and mobile-to-desktop session sync. Actively developed with an enhanced system prompt, surgical-editing rules, and anti-overengineering patterns.

> **Version**: v0.3.0 | **Goal**: a native CLI coding agent with file-attachment OCR, multimodal document understanding, unified model profiles, and a bioluminescent web interface.

---

## Table of Contents
- [Quick Start](#quick-start)
- [Features](#features)
- [Multi-Model & Multi-Provider](#multi-model--multi-provider)
- [Permission Modes](#permission-modes)
- [Web UI](#web-ui)
- [Mobile & Remote Access](#mobile--remote-access)
- [Skills & Plugins](#skills--plugins)
- [Configuration (settings.json)](#configuration-settingsjson)
- [CLI Reference](#cli-reference)
- [Architecture](#architecture)
- [中文摘要](#中文摘要)

---

## Quick Start

### Requirements
- Rust 1.82+
- Anthropic API Key (or compatible endpoint: DeepSeek, GLM, etc.)
- `ripgrep` (optional, for Grep tool)
- `cloudflared` (optional, for `--tunnel` remote access — [install guide](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/))
- Node.js & npm (for building the frontend — install via `install.sh`)

### Install

```bash
# Linux / macOS — one command:
cd rust && bash install.sh
# → installs to ~/.local/bin/nonoclaw, sets up frontend

# Windows:
powershell -ExecutionPolicy Bypass -File install.ps1
```

### Run

```bash
# Web UI (with multi-model switching):
nonoclaw --serve-http 127.0.0.1:8765 --model deepseek-v4-pro

# With Cloudflare Tunnel (phone scans QR from anywhere):
nonoclaw --serve-http 127.0.0.1:8765 --tunnel

# Headless:
nonoclaw -p "explain Rust ownership"
```

---

## Features

| Category | Details |
|---|---|
| **Agent Loop** | Streaming SSE, auto-retry, multi-turn tool-use/tool-result pairing, **orphan repair** (auto-fix broken tool_use/tool_result pairs), **thinking-block strip** (Bedrock proxy compat), **batched parallel tool execution** (concurrency cap=10) |
| **Memory System** | Multi-layer: MEMORY.md index + individual fact files with YAML frontmatter, model can write/update/delete memories, `[[link]]` references, **auto-loaded each run** |
| **System Prompt** | Enhanced with surgical editing rules, 6 named failure modes, anti-overengineering patterns, ToolSearch guidance, **git context in uncached block** (cache survives per-turn), **memory write-back instructions** |
| **17 Built-in Tools** | Read, Write, Edit, Bash, Grep, Glob, TodoWrite, WebFetch, WebSearch, Agent, AskUserQuestion, Coordinator, **ToolSearch**, **TaskCreate/Get/List/Update** |
| **File Attachments** | Upload PDF/DOCX/DOC/TXT/MD/PNG/JPG via paperclip, drag-drop, or paste; **auto-OCR** via Mistral/DeepSeek configurable doc models; **direct text extraction** (pdftotext + ZIP XML) skips OCR when possible; **embedded image extraction** (pdfimages + word/media) with per-image OCR descriptions; **ContentBlock::Image injection** for multimodal models |
| **Bash Background** | `run_in_background: true` spawns detached process with disk-persisted output, `<task_notification>` injection on completion |
| **MCP** | Client (`--mcp-config`) + Server (`--mcp-serve`), **MCP prompts → skill bridge** |
| **Unified Model Profiles** | All models in single `models[]` array with `role` tags (`main`/`doc`/`compact`); `docModel` and `compactModel` reference by name; **per-model contextWindow / maxTokens / charsPerToken**; **compactModel** independent summarization model |
| **Multi-Model** | Model switching via UI dropdown or `/multi` slash command; `/multi` now shows syntax help on error |
| **Permissions** | 5 modes: Default / AcceptEdits / Auto / BypassPermissions / Plan — switchable via UI dropdown |
| **Sessions** | JSONL persistence per-cwd, `--resume` / `--continue` / `--list-sessions`, **session naming**, progressive metadata |
| **Context** | Auto-compaction `compactThreshold` tokens, configurable `contextWindow`, **Prompt Caching** (ephemeral, git excluded from cache), **per-model token estimation** (charsPerToken) |
| **Skills** | `/skill-name` injection, **12 bundled built-in skills**, **dynamic activation** via paths/triggers/file discovery, argument substitution, fork context, usage tracking, hot reload |
| **Plugins** | `--plugin-add`, hooks via `.nonoclaw/hooks.json` (**shell + prompt + HTTP**, 12 event types) |
| **Task System** | File-persisted task store, dependency graph, owner assignment, status lifecycle |
| **Web UI** | Bioluminescent dark theme, breathing aurora, file tree, Git pane, Insight accordion, Markdown+KaTeX, **tool card auto-collapse + command preview**, **attachment chips with upload state**, **"Nono" assistant label** |
| **PWA** | Add to Home Screen, offline SW cache, installable on Android/iOS |
| **Mobile Sync** | QR code → shared session → real-time MessagesLoaded broadcast; **skipOneLoad** for reliable peer sync; **sync_session_to_peers** on Run/Clear/post-run; **markClearing** prevents tool-card residue |
| **Tunnel** | `--tunnel` auto-spawns Cloudflare Tunnel for public HTTPS access with terminal ASCII QR code |
| **Export** | Markdown copy + `.md` file download from assistant responses |

---

## Multi-Model & Multi-Provider

All models live in a single `models[]` array with `role` tags — main conversation models, document-processing models, and compaction models:

```json
{
  "models": [
    {
      "name": "deepseek-v4-pro",
      "label": "DeepSeek V4",
      "baseUrl": "https://api.deepseek.com/anthropic",
      "apiKey": "sk-xxxx",
      "role": ["main"],
      "default": true,
      "contextWindow": 1048576,
      "maxTokens": 8192,
      "charsPerToken": 3
    },
    {
      "name": "claude-sonnet-4-5-20250929",
      "label": "Claude Sonnet 4.5",
      "baseUrl": "https://api.anthropic.com",
      "apiKey": "sk-ant-zzzz",
      "role": ["main", "compact"],
      "contextWindow": 200000,
      "maxTokens": 8192,
      "charsPerToken": 4
    },
    {
      "name": "mistral-ocr-latest",
      "label": "Mistral OCR",
      "baseUrl": "https://api.mistral.ai",
      "apiKey": "sk-mistral-xxxx",
      "role": ["doc"]
    }
  ],
  "docModel": "mistral-ocr-latest",
  "compactModel": "claude-sonnet-4-5-20250929"
}
```

**Model roles**:
| Role | Purpose | UI Dropdown |
|------|---------|:-----------:|
| `main` (or absent) | Conversation model | ✅ Yes |
| `doc` | Document-processing (OCR / vision) | ❌ No |
| `compact` | Summarization / compaction | ❌ No |

A model can have multiple roles — e.g. `["main", "compact"]` for a model that serves both conversation and summarization.

**Per-model fields**: `contextWindow` (total tokens), `maxTokens` (output limit), `charsPerToken` (tokenizer estimate) — override global defaults per model.

**Runtime switching**: The status bar model name becomes a dropdown (when 2+ `main` models configured). Switching rebuilds the API `Client` per-run — no restart.

**`/multi` slash command**: Compare answers from multiple models:
```
/multi deepseek-v4-pro,glm-5.2 compare Rust and Go error handling
```
Sends the prompt to both models sequentially, labels each response with the model name. Shows syntax help on malformed input.

### Document Processing (File Attachments)

Click the paperclip (📎), drag files, or paste to upload. Supported: PDF, DOCX, DOC, TXT, MD, PNG, JPG.

**Processing pipeline**:
```
Upload
├─ TXT/MD → direct read
├─ PDF    → pdftotext (text) + pdfimages (embedded) → OCR if scanned
├─ DOCX   → ZIP XML <w:t> (text) + word/media/ (embedded) → OCR if sparse
└─ Images → DeepSeek OCR 2 (tiled) or Mistral OCR
```

**Doc model providers** (`provider` auto-inferred from model name):
| Provider | Model Name Pattern | API Format |
|----------|-------------------|------------|
| `mistral_ocr` | contains `mistral` | `POST /v1/ocr` |
| `deepseek_ocr` | contains `deepseek`+`ocr` | `POST /v1/chat/completions` (tiled) |
| `generic_vision` | anything else | `POST /v1/chat/completions` |

Embedded images are OCR'd individually so text-only models (DeepSeek V4) can "see" them as inline descriptions. Multimodal models (Sonnet) receive both `ContentBlock::Image` blocks and OCR text.

---

## Permission Modes

All modes switchable at runtime via UI dropdown (status bar, next to the model dropdown):

| Mode | Behavior | Color |
|---|---|---|
| `default` | Read-only tools auto-allowed; writes prompt a dialog | Mint |
| `acceptEdits` | Auto-allow Read + Write + Edit; Bash still prompts | Violet |
| `auto` | Auto-allow **everything** — no prompts at all | Mint |
| `bypassPermissions` | Skip ALL checks (= `--dangerously-skip-permissions`) | Red |
| `plan` | Read-only: writes are **hard-denied** | Sky Blue |

Also configurable via `settings.json`:
```json
{ "permissions": { "defaultMode": "auto" } }
```

---

## Web UI

Start with `--serve-http 127.0.0.1:8765` and open the browser.

### Layout (three-column)
```
┌─ StatusBar ─────────────────────────────────────────────────┐
│ «NonoClaw»  [model▾] [mode▾]  tokens · session  ◰ theme ● │
├──────┬──────────────────────────────────┬───────────────────┤
│ FILE │  chat (Markdown + KaTeX)         │ INSIGHT accordion
│ TREE │  ─────────────────────           │  ▸ Tools (12)
│      │  message bubbles                │  ▸ MCP servers
│──────│  user/assistant/tool cards      │  ▸ Models
│ GIT  │                                  │  ▸ Skills
│pane  │  ┌─ composer ─── [send↗] ───┐  │  ▸ Hooks
│      │  └───────────────────────────┘  │  ▸ Slash cmds
│      │                                  │  ▸ Docs & config
│      │                                  │  ▸ CLI reference
│      │                                  │  ▸ Project
└──────┴──────────────────────────────────┴───────────────────┘
```

### Key UI Features
- **Breathing background** — aurora orbs pulse in rhythm with token-stream velocity
- **Three themes** — Biolume (cyan/mint) · Amber Forge (gold) · Glacial Frost (ice-blue) — cycled via status bar dot
- **File tree** — click file → open in OS default editor; Shift+click → VS Code
- **Git pane** — branch, ahead/behind, staged/modified/untracked counts, recent commits (click → `git show` modal), filter by author/subject
- **Insight accordion** — Tools (click → expand input schema + prompt preview), MCP servers, Models, Skills, Hooks, Slash commands, Docs & config (clickable to edit), CLI reference, Project info
- **Markdown rendering** — GFM tables, KaTeX math (inline `$...$` and block `$$...$$`), syntax highlighting
- **Copy & Export** — copy assistant response as Markdown; download as `.md` file

### /slash commands (type in composer)
| Command | Description |
|---|---|
| `/clear` | Reset conversation (memory + disk) |
| `/compact` | Summarise long context |
| `/skill-name` | Inject a skill's instructions into system prompt (with args: `/deploy prod main`) |
| `/multi model1,model2 <prompt>` | Compare answers from multiple models |
| `/rename <title>` | Set a custom session title |

---

## Mobile & Remote Access

### QR Code + Session Sync

1. Desktop: `nonoclaw --serve-http 127.0.0.1:8765` → click ◰ in status bar → QR code appears
2. Phone scans QR (same LAN or tunnel) → browser opens with `?token=...&session=...`
3. Phone joins the **same session** as desktop — shared `SessionHandle`, real-time `MessagesLoaded` broadcast
4. "Add to Home Screen" → standalone PWA app

### Cloudflare Tunnel (`--tunnel`)

```bash
# One-time: install cloudflared
curl -L https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 -o ~/bin/cloudflared
chmod +x ~/bin/cloudflared

# Start with tunnel:
nonoclaw --serve-http 127.0.0.1:8765 --tunnel
```

What happens:
1. NonoClaw auto-spawns `cloudflared tunnel --url http://127.0.0.1:8765`
2. Captures the `*.trycloudflare.com` URL from cloudflared's output
3. Prints an **ASCII QR code** to the terminal (scannable immediately)
4. Sets `public_url` auto-matically — the web UI QR button uses the tunnel URL

Phone can access NonoClaw from any network (4G/5G, different WiFi, abroad) — no port forwarding, no public IP needed.

### Session Sync Logic

```
Desktop connects → shared_sid = most-recent-session → registry["abc123"]
Mobile scans QR    → shared_sid = "abc123" from URL → registry["abc123"].txs += phone

Desktop sends Run → events stream to desktop only
                 → after Done: MessagesLoaded broadcast to phone
                 → phone UI updates automatically
```

---

## Skills & Plugins

### Skills (`/skill-name`)

Create `.nonoclaw/skills/<name>/SKILL.md` with YAML frontmatter:

```markdown
---
name: deploy
description: Deploy the project to production
argument-hint: "<env> <branch>"
arguments: [env, branch]
paths: [deploy/**]
triggers: ["deploy|ship|release"]
when_to_use: when the user asks to deploy or ship code
allowed-tools: [Bash, Read, Write]
context: fork
---
# Deploy
Run `./deploy.sh --env=$1 --branch=$2`
```

#### Supported Frontmatter Fields (v0.2.0)

| Field | Description |
|---|---|
| `name` | Skill name (used as `/name`) |
| `description` | One-line purpose |
| `paths` | Glob patterns — skill auto-activates when matching files are read/written/edited |
| `triggers` | Regex patterns — skill auto-activates when user input matches |
| `when_to_use` | NL guidance injected into system prompt |
| `allowed-tools` | Restrict which tools the skill can use |
| `argument-hint` | CLI usage hint shown in autocomplete |
| `arguments` | Positional argument names for `$1`, `$2` substitution |
| `version` | Skill version string |
| `model` | Override model when skill is active |
| `disable-model-invocation` | If true, model cannot auto-invoke — slash-command only |
| `user-invocable` | Whether `/name` is available (default: true) |
| `context` | `"fork"` spawns isolated sub-agent; otherwise inline |
| `agent` | Agent type when context is `"fork"` |
| `effort` | Thinking effort level (`low`/`medium`/`high`) |
| `shell` | Shell override (`bash`/`powershell`) |

#### Dynamic Activation (CC-compatible)

Skills aren't just static `/name` commands — they activate dynamically:

| Mechanism | How it works |
|---|---|
| **`paths`** | After Read/Write/Edit touches a matching file, the skill auto-activates (gitignore-style glob matching) |
| **`triggers`** | User input regex match → skill auto-loads before the first turn |
| **File discovery** | Walking up from operation file paths discovers nested `.nonoclaw/skills/` directories mid-session |
| **Conditional** | Skills with `paths` are deferred until matching files are touched (reduces system prompt bloat) |

#### Argument Substitution
Skill bodies support CC-compatible variable expansion:
- `$1`, `$2` — positional arguments from `/name arg1 arg2`
- `$ARGUMENTS` — raw argument string
- `$ARGUMENTS[0]`, `$ARGUMENTS[1]` — indexed access
- `${NONOCLAW_SKILL_DIR}` — skill's own directory path
- `${NONOCLAW_SESSION_ID}` — current session UUID

#### Bundled Skills (12 built-in)
Always available without disk scanning: `verify`, `simplify`, `debug`, `remember`, `loop`, `update-config`, `keybindings-help`, `claude-api`, `code-review`, `init`, `review`, `security-review`

#### Usage Tracking
Skill invocations are persisted to `~/.nonoclaw/skill-usage.json` with 7-day half-life decay — frequently used skills rank higher in listings.

#### Hot Reload
Edit `SKILL.md` on disk → changes reflected within 500ms via `notify` file watcher (no restart needed).

### Plugins

```bash
nonoclaw --plugin-add /path/to/plugin      # local dir
nonoclaw --plugin-add https://github.com/... # git URL
```
Installed to `~/.nonoclaw/plugins/`. Skills contributed by plugins are auto-discovered.

### Hooks (`.nonoclaw/hooks.json`)

Three hook kinds supported — **shell command**, **LLM prompt evaluation**, and **HTTP POST**:

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash*", "command": "scripts/guard.sh" }
    ],
    "PostToolUse": [
      { "matcher": "*", "command": "notify-send", "args": ["done"] },
      { "matcher": "Write", "prompt": { "model": "claude-haiku-4-5", "timeout_secs": 30 } },
      { "matcher": "*", "http": { "url": "https://hooks.example.com/cc", "headers": { "X-Token": "${HOOK_TOKEN}" } } }
    ]
  }
}
```

| Hook Type | Behavior |
|---|---|
| **Shell** (`command` + `args`) | Executes a subprocess; `PreToolUse` non-zero exit → blocks the tool call |
| **Prompt** (`prompt`) | Calls a small model (Haiku) with the hook context, enforces JSON schema `{ ok, reason? }` |
| **HTTP** (`http`) | POSTs JSON payload to URL, supports env-var interpolation in URL/headers |

**12 event types**: `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Notification`, `UserPromptSubmit`, `SessionStart`, `SessionEnd`, `Stop`, `SubagentStart`, `SubagentStop`, `PreCompact`, `PostCompact`

---

## Configuration (settings.json)

Full example at `~/.nonoclaw/settings.json`:

```json
{
  "model": "deepseek-v4-pro",
  "contextWindow": 1048576,
  "maxTokens": 8192,
  "env": {
    "ANTHROPIC_API_KEY": "sk-xxxx",
    "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
    "BRAVE_API_KEY": "your-brave-key"
  },
  "models": [
    {
      "name": "deepseek-v4-pro", "label": "DeepSeek V4",
      "baseUrl": "https://api.deepseek.com/anthropic",
      "apiKey": "sk-xxxx", "default": true
    },
    {
      "name": "glm-5.2", "label": "GLM 5.2",
      "baseUrl": "https://open.bigmodel.cn/api/anthropic",
      "apiKey": "sk-yyyy"
    }
  ],
  "mcpServers": {
    "my-server": { "command": "npx", "args": ["-y", "@scope/mcp-server"] }
  },
  "permissions": {
    "defaultMode": "auto",
    "allow": ["Bash(cargo build:*)"],
    "deny": ["Bash(sudo:*)"]
  },
  "compactThreshold": 80000,
  "autoCompact": true
}
```

### Top-level fields

| Field | Description |
|---|---|
| `model` | Default model (used when `models[]` is absent) |
| `contextWindow` | Global context window (overridden by per-model `contextWindow`) |
| `maxTokens` | Global max output per turn (overridden by per-model `maxTokens`) |
| `charsPerToken` | Global chars-per-token estimator (default 4; overridden per-model) |
| `env` | Environment vars injected at startup |
| `models[]` | All model profiles: `name`, `label`, `baseUrl`, `apiKey`, `role[]`, `default`, `contextWindow`, `maxTokens`, `charsPerToken` |
| `docModel` | Model name reference for document processing (OCR) |
| `compactModel` | Model name reference for auto-compaction summarization |
| `mcpServers` | MCP server configs: `command`, `args`, `env` |
| `permissions.defaultMode` | `default` / `acceptEdits` / `auto` / `bypassPermissions` / `plan` |
| `permissions.allow` | Tool patterns to always allow |
| `permissions.deny` | Tool patterns to always deny |
| `compactThreshold` | Auto-compact trigger (estimated tokens) |
| `autoCompact` | Enable/disable auto-compaction |

---

## CLI Reference

```bash
# Web UI
nonoclaw --serve-http 127.0.0.1:8765 --tunnel

# Headless
nonoclaw -p "summarize README"
echo "fix the bug" | nonoclaw -p --allowed-tools Read,Edit,Bash

# Sessions
nonoclaw --continue "keep going"
nonoclaw --list-sessions
nonoclaw --resume abc123 "resume specific session"

# MCP
nonoclaw --mcp-config servers.json "call the weather tool"
nonoclaw --mcp-serve  # expose as MCP server

# Plugins
nonoclaw --plugin-add ~/my-hooks
```

### Key Flags

| Flag | Default | Description |
|---|---|---|
| `--model` | `claude-sonnet-4-5` | Override model |
| `--max-turns` | 200 | Max agentic loop turns |
| `--max-tokens` | 8192 | Max output per turn |
| `--permission-mode` | `default` | Permission posture |
| `--context-window` | — | Model context size (auto-derives compact threshold) |
| `--compact-threshold` | 80000 | Estimated-token auto-compact trigger |
| `--no-auto-compact` | false | Disable auto-compaction |
| `--allowed-tools` | — | Comma-separated tool allowlist |
| `--disallowed-tools` | — | Comma-separated tool denylist |
| `--dangerously-skip-permissions` | — | Bypass all permission checks |
| `--append-system-prompt` | — | Extra system prompt text |
| `--name` | — | Set custom session title at startup |
| `--tunnel` | false | Auto-spawn cloudflared |
| `--public-url` | — | Override QR code URL |
| `--settings` | — | Explicit settings file path |

---

## Architecture

```
NonoClaw/
├── src/               TypeScript reference (read-only, not in git/build)
├── rust/              Rust rewrite (active)
│   ├── crates/
│   │   ├── core/      nonoclaw-core     — messages, usage, permissions
│   │   ├── api/       nonoclaw-api      — Anthropic streaming client
│   │   ├── tools/     nonoclaw-tools    — Tool trait + registry + 17 builtins + MCP + background tasks
│   │   ├── engine/    nonoclaw-engine   — query loop + prompt + compact + session + skills + hooks
│   │   └── cli/       nonoclaw (bin)    — CLI + Web UI + remote + skill watcher + project info
│   ├── install.sh / install.ps1
│   └── Cargo.toml
├── frontend/          React + Vite (TypeScript)
│   ├── src/           components, store, WebSocket client
│   ├── index.html     CSS design tokens
│   └── package.json
├── .gitignore
└── README.md
```

---

## Environment Variables

| Variable | Description |
|---|---|
| `ANTHROPIC_API_KEY` | API key |
| `ANTHROPIC_BASE_URL` | Custom API endpoint |
| `ANTHROPIC_AUTH_TOKEN` | Bearer auth (alternative) |
| `NONOCLAW_HOME` | Override data root (`~/.nonoclaw`) |
| `SERPER_API_KEY` / `BRAVE_API_KEY` | WebSearch backends |
| `NONOCLAW_MAX_TOOL_CONCURRENCY` | Max parallel tool executions (default: 10) |
| `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS` | Disable `run_in_background` (default: enabled) |
| `RUST_LOG` | Log level (`debug`, `info`, `warn`) |

---

## 中文摘要

NonoClaw 是 Claude Code（Anthropic 智能体 CLI 命令行工具）的 **Rust 重写版本**。完整实现智能体循环、17 个内置工具、分批并发工具执行、后台 Bash 任务、5 级权限门禁、会话持久化（JSONL，支持命名）、MCP 双向（含 prompts→skill 桥接）、Web UI（含 PWA 移动端支持）。

### v0.3.0 新增亮点
- **文件附件上传**：支持 PDF/DOCX/DOC/TXT/MD/PNG/JPG，纸夹按钮 + 拖拽 + 粘贴三种上传方式。PDF/DOCX 优先直接提取文字（pdftotext / ZIP XML），扫描件自动降级 OCR。嵌入图片（公章、签名、图表）自动提取并 OCR 生成文字描述。
- **统一模型配置**：所有模型集中在 `models[]` 数组，通过 `role` 标签区分用途（`main` 对话 / `doc` 文档处理 / `compact` 摘要压缩）。`docModel` 和 `compactModel` 以名称字符串引用。
- **多模态文档理解**：支持 Mistral OCR（原生 PDF）和 DeepSeek OCR 2（切片式）两种文档处理后端。嵌入图片以 `ContentBlock::Image` 注入多模态模型（Sonnet），同时生成 OCR 文字描述供纯文本模型（DeepSeek V4）使用。
- **记忆系统**：模型可通过 Write 工具创建/更新 `.nonoclaw/memory/*.md` 事实文件，带 YAML frontmatter 和 `[[link]]` 引用。MEMORY.md 索引 + 独立 fact 文件双层结构。
- **Per-Model 参数**：每个模型可配专属 `contextWindow`、`maxTokens`、`charsPerToken`，自动压缩阈值和 token 估算更精确。
- **同步机制重构**：`skipOneLoad` 替代时间窗口，`sync_session_to_peers` 统一广播，Run 到达时立即同步，`markClearing` 防止 /clear 残留。
- **Prompt Cache 优化**：Git 上下文从 cached block 移至 uncached block，缓存不会每次工具执行后失效。Thinking 块自动过滤（Bedrock 代理兼容）。
- **工具卡片增强**：自动折叠 + 命令预览（Bash/WebFetch/WebSearch/Grep 等显示关键参数）。`/multi` 语法错误时显示帮助提示。

### 核心特色
- **多模型切换**：统一 `models[]` 数组，`role` 标签区分用途，UI 下拉框切换对话模型，`/multi` 多模型对比
- **文档处理**：上传即 OCR，Mistral OCR ($4/千页) 或 DeepSeek OCR 2 ($0.03/M tokens)，文字型文档直读零成本
- **Web UI**：三栏布局，生物发光暗色主题，呼吸式 aurora 背景，Markdown + KaTeX 渲染，附件 chips 状态指示
- **记忆持久化**：模型可读写 `.nonoclaw/memory/` 事实文件，跨 session 持久化用户偏好和项目上下文
- **Cloudflare Tunnel**：`--tunnel` 自动隧道 + ASCII 二维码，手机扫码远程访问共享 session
- **权限模式**：UI 下拉框切换 5 种模式
- **增强 System Prompt**：手术级改动规则、6 种命名失败模式、反过度工程规则、记忆写入指令
- **配置灵活**：每模型专属窗口/令牌/估值，集中管理

### 安装运行
```bash
cd rust && bash install.sh
nonoclaw --serve-http 127.0.0.1:8765 --tunnel --model deepseek-v4-pro
```

---

## License

MIT
