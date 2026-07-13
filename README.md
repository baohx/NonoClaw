# NonoClaw [English/中文]

A **Rust rewrite** of [Claude Code](https://claude.ai/code) (Anthropic's agent CLI). Full agentic loop, tool dispatch, permission system, session persistence, MCP client/server, a **Web UI** with PWA, and mobile-to-desktop session sync. Actively developed with an enhanced system prompt, surgical-editing rules, and anti-overengineering patterns.

> **Version**: v0.2.0 | **Goal**: a native CLI coding agent with a complete tool ecosystem, multi-model switching, remote access via Cloudflare Tunnel, and a bioluminescent web interface.

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
| **Agent Loop** | Streaming SSE, auto-retry, multi-turn tool-use/tool-result pairing, **batched parallel tool execution** (concurrency cap=10) |
| **System Prompt** | Enhanced with surgical editing rules, 6 named failure modes (Kitchen Sink, Runaway Refactor, etc.), Karpathy's anti-overengineering patterns, ToolSearch guidance for deferred tools |
| **17 Built-in Tools** | Read, Write, Edit, Bash, Grep, Glob, TodoWrite, WebFetch, WebSearch, Agent, AskUserQuestion, Coordinator, **ToolSearch**, **TaskCreate/Get/List/Update** |
| **Bash Background** | `run_in_background: true` spawns detached process with disk-persisted output, `<task_notification>` injection on completion |
| **MCP** | Client (`--mcp-config`) + Server (`--mcp-serve`), **MCP prompts → skill bridge** (`prompts/list` + `prompts/get`) |
| **Multi-Model** | Pre-define model profiles in `settings.json` → switch at runtime via UI dropdown or `/multi` slash command |
| **Permissions** | 5 modes: Default / AcceptEdits / Auto / BypassPermissions / Plan — switchable via UI dropdown |
| **Sessions** | JSONL persistence per-cwd, `--resume` / `--continue` / `--list-sessions`, **session naming** (custom/ai/auto titles), **progressive metadata** (title, tag, mode) |
| **Context** | Auto-compaction `compactThreshold` tokens, configurable `contextWindow`, **Prompt Caching** (ephemeral cache breakpoints on system prompt + tools) |
| **Skills** | `/skill-name` injection, **12 bundled built-in skills**, **dynamic activation** via `paths` globs / `triggers` regex / file-path discovery, **argument substitution** (`$1`, `$ARGUMENTS`, `${NONOCLAW_SKILL_DIR}`), **fork context** (`context: fork` → sub-agent), **usage tracking** (7-day half-life decay), **hot reload** (notify file watcher) |
| **Plugins** | `--plugin-add`, hooks via `.nonoclaw/hooks.json` (**shell + prompt + HTTP** hook kinds, 12 event types) |
| **Task System** | File-persisted task store (`~/.nonoclaw/tasks/`), dependency graph (blocks/blockedBy), owner assignment, status lifecycle (pending→in_progress→completed) |
| **Web UI** | Bioluminescent dark theme, breathing aurora, file tree, Git pane, Insight accordion, Markdown+KaTeX rendering |
| **PWA** | Add to Home Screen, offline SW cache, installable on Android/iOS |
| **Mobile Sync** | QR code → shared session → real-time MessagesLoaded broadcast between desktop ↔ phone |
| **Tunnel** | `--tunnel` auto-spawns Cloudflare Tunnel for public HTTPS access with terminal ASCII QR code |
| **Export** | Markdown copy + `.md` file download from assistant responses |

---

## Multi-Model & Multi-Provider

Define provider profiles in `settings.json` — each with its own `baseUrl`, `apiKey`, and a `label` for the UI dropdown:

```json
{
  "models": [
    {
      "name": "deepseek-v4-pro",
      "label": "DeepSeek V4",
      "baseUrl": "https://api.deepseek.com/anthropic",
      "apiKey": "sk-xxxx",
      "default": true
    },
    {
      "name": "glm-5.2",
      "label": "GLM 5.2",
      "baseUrl": "https://open.bigmodel.cn/api/anthropic",
      "apiKey": "sk-yyyy"
    },
    {
      "name": "claude-sonnet-4-5",
      "label": "Claude Sonnet",
      "baseUrl": "https://api.anthropic.com",
      "apiKey": "sk-ant-zzzz"
    }
  ]
}
```

**Runtime switching**: The status bar model name becomes a dropdown (when 2+ models configured). Switching rebuilds the API `Client` per-run with the matching endpoint and key — no restart required.

**`/multi` slash command**: Compare answers from multiple models in one turn:
```
/multi deepseek-v4-pro,glm-5.2 compare Rust and Go error handling
```
Sends the prompt to both models sequentially, labels each response with the model name.

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
| `contextWindow` | Model's total context size in tokens (auto-calculates compact threshold) |
| `maxTokens` | Max output tokens per turn |
| `env` | Environment vars injected at startup (legacy single-model mode) |
| `models[]` | Multi-model profiles: `name`, `label`, `baseUrl`, `apiKey`, `default` |
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

### v0.2.0 新增亮点
- **分批工具并发**：按 CC 标准 `partitionToolCalls` 实现，连续并发安全工具自动分组并行执行（上限 10，可通过 `NONOCLAW_MAX_TOOL_CONCURRENCY` 配置）
- **Bash 后台任务**：`run_in_background: true` 将长时间命令放入后台，输出持久化到磁盘，完成后自动注入 `<task_notification>`
- **Task 工具集**：TaskCreate/Get/List/Update 四个工具，文件持久化到 `~/.nonoclaw/tasks/`，支持依赖图（blocks/blockedBy）和 owner 分配
- **ToolSearch 按需加载**：WebSearch/WebFetch/Coordinator 等低频工具不在 system prompt 中出现，模型通过 ToolSearch 工具按关键词查找
- **Skills 动态激活**：12 个内置技能 + 条件激活（paths glob 匹配）+ 正则触发器（triggers）+ 文件路径向上发现 + fork 上下文子代理 + 参数替换 + 使用跟踪 + 热重载
- **Hooks 扩展**：支持 3 种执行方式（Shell 命令 / LLM Prompt 评估 / HTTP POST），12 种事件类型
- **Session 丰富**：支持自定义标题、AI 生成标题、自动首句提取、标签、模式元数据
- **MCP 技能桥**：`prompts/list` → 自动注册为 `/mcp__server__prompt` 可调用技能

### 核心特色
- **多模型切换**：在 `settings.json` 的 `models[]` 数组中预配不同供应商（DeepSeek、GLM、Claude）的 endpoint 和 key，通过 UI 下拉框随时切换；`/multi` 斜杠命令支持一轮对话中用多个模型回答并对比
- **Web UI**：三栏布局（文件树+Git面板 / 对话 / Insight 手风琴），生物发光暗色主题，呼吸式 aurora 背景（随 token 输出节奏脉动），支持 Markdown + KaTeX 数学公式渲染
- **Cloudflare Tunnel**：`--tunnel` 自动启动 cloudflared 隧道，终端打印 ASCII 二维码，手机在任何网络扫码即可远程访问并共享同一 session
- **权限模式**：UI 下拉框随时切换 `default` / `acceptEdits` / `auto` / `bypassPermissions` / `plan`
- **增强 System Prompt**：包含手术级改动规则、6 种命名失败模式（厨房水槽、失控重构等）、Karpathy 反过度工程规则、ToolSearch 使用指南
- **Skills 机制**：12 个内置技能 + 15 种 frontmatter 字段 + 5 种动态激活方式 + 参数替换 + 使用排名 + 热重载
- **配置灵活**：`settings.json` 集中管理模型、权限、上下文窗口、MCP server、Brave 搜索 key 等

### 安装运行
```bash
cd rust && bash install.sh
nonoclaw --serve-http 127.0.0.1:8765 --tunnel --model deepseek-v4-pro
```

---

## License

MIT
