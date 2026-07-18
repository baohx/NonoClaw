# NonoClaw [English/中文]

A **Rust rewrite** of [Claude Code](https://claude.ai/code) (Anthropic's agent CLI). Full agentic loop, tool dispatch, permission system, session persistence, MCP client/server, a **Web UI** with PWA, and mobile-to-desktop session sync. Actively developed with an enhanced system prompt, surgical-editing rules, and anti-overengineering patterns.

> **Version**: v0.4.0 | **Goal**: a native CLI coding agent with cross-session memory, file-attachment OCR, multimodal document understanding, and a bioluminescent web interface.

---

## Table of Contents
- [Quick Start](#quick-start)
- [Features](#features)
- [Multi-Model & Multi-Provider](#multi-model--multi-provider)
- [Cross-Session Memory (Mneme)](#cross-session-memory-mneme)
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
| **Cross-Session Memory (Mneme)** | Three-layer: **Facts** (immutable knowledge in `memory/facts/*.md`), **Beads** (task continuity in `memory/beads/*.md`), **Transcript** (per-session JSONL). BM25 search with importance ranking. `Memory` tool (18th built-in). Auto-injected into SystemBlock #2 each session. Git-friendly markdown files. Inspired by agentmemory. |
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

## Cross-Session Memory (Mneme)

NonoClaw features a three-layer memory system inspired by [agentmemory](https://github.com/rohitg00/agentmemory). Every session starts with the previous session's knowledge and task state injected into context — no more "starting fresh" and re-explaining everything.

### Architecture

```
┌──────────────────────────────────────────────────────────┐
│ Layer 3: TRANSCRIPT — per-session JSONL (automatic)      │
│   ~/.nonoclaw/projects/<cwd>/sessions/<uuid>.jsonl      │
├──────────────────────────────────────────────────────────┤
│ Layer 2: BEADS — task continuity (survives sessions)     │
│   <cwd>/.nonoclaw/memory/beads/*.md                      │
│   Active tasks, blocked items, progress trackers.        │
├──────────────────────────────────────────────────────────┤
│ Layer 1: FACTS — immutable knowledge (permanent)         │
│   <cwd>/.nonoclaw/memory/facts/*.md                      │
│   Conventions, preferences, decisions, bug patterns.     │
└──────────────────────────────────────────────────────────┘
```

### Facts (`memory/facts/*.md`)

One markdown file per immutable fact with YAML frontmatter. Types: `preference`, `convention`, `decision`, `architecture`, `bug`. Facts are **never deleted** — wrong ones get `superseded_by` pointing to the replacement.

```markdown
---
name: pip-use-tsinghua-mirror
title: Use Tsinghua mirror for pip installs
type: preference
importance: 0.9
confidence: 0.95
tags: [python, pip, china]
---

Always use pip install -i https://mirrors.tuna.tsinghua.edu.cn/pypi/web/simple
when installing Python packages.
```

### Beads (`memory/beads/*.md`)

Each bead tracks one active task across sessions. Status: `todo` → `in_progress` → `blocked` → `done`.

```markdown
---
id: bead-abc123
title: Fix login timeout in production
status: in_progress
priority: 8
session: abc123
---

## Progress
- [x] Reproduced in staging
- [ ] Root cause: connection pool exhaustion
- [ ] Implement circuit breaker

## Blockers
None.
```

### Memory Tool (18th built-in)

| Action | Description |
|--------|-------------|
| `Memory search <query>` | BM25 search over all facts, ranked by relevance × importance |
| `Memory save` | Create or update a fact (name, title, type, importance, tags) |
| `Memory forget <name>` | Mark a fact as superseded |
| `Memory beads` | List all active (non-done) beads, sorted by priority |
| `Memory bead_save` | Create or update a task bead |
| `Memory bead_done <id>` | Mark a bead as completed |

The model can also use standard `Read`/`Write`/`Edit` tools directly on `memory/` files.

### Context Injection

At session start, `SystemBlock #2` (uncached) automatically includes:

```
## Active Tasks (beads)
◌ Fix login timeout [priority 8]
  Investigating connection pool issue...

## Key Facts
- **pip-use-tsinghua-mirror** (preference): Use Tsinghua mirror for pip
- **rust-edition-2024** (convention): New projects use Rust 2024
```

Active beads (max 5) + top important facts (max 10). Capped at 50KB total.

### Example Session Flow

```
SESSION 1 — Discovery
  You:   "pip install 太慢了"
  Nono:  "网络问题。记住用清华源可以吗？"
  You:   "好"
  Nono:  → Memory save: pip-use-tsinghua-mirror.md
         → Memory bead_save: 优化 pip 安装速度

SESSION 2 — Next Day (automatic resume)
  [System prompt already contains:]
    ◌ 优化 pip 安装速度 [done, session 1]
    - pip-use-tsinghua-mirror (preference)

  You:   "装一个 requests 库"
  Nono:  "pip install -i https://mirrors.tuna.tsinghua.edu.cn/pypi/web/simple requests"
         ↑ 自动用了清华源，不需要你再提醒

SESSION 3 — One Week Later
  You:   "这个项目之前遇到过什么网络问题？"
  Nono:  → Memory search: "network pip mirror"
         返回 pip-use-tsinghua-mirror 事实，告诉你历史上下文
```

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

# 中文版（完整翻译）

NonoClaw 是 [Claude Code](https://claude.ai/code)（Anthropic 的智能体 CLI）的 **Rust 重写版本**。完整的智能体循环、工具调度、权限系统、会话持久化、MCP 客户端/服务端、带 PWA 的 **Web 界面**以及手机与桌面端会话同步。配备增强型系统提示词、手术级编辑规则和反过度工程模式。

> **版本**: v0.4.0 | **目标**: 一个原生 CLI 编程智能体，具备跨会话记忆、文件附件 OCR、多模态文档理解和生物发光 Web 界面。

---

## 目录
- [快速开始](#quick-start)
- [功能特性](#features)
- [多模型与多供应商](#multi-model--multi-provider)
- [跨会话记忆 (Mneme)](#cross-session-memory-mneme)
- [权限模式](#permission-modes)
- [Web 界面](#web-ui)
- [移动端与远程访问](#mobile--remote-access)
- [技能与插件](#skills--plugins)
- [配置 (settings.json)](#configuration-settingsjson)
- [CLI 参考](#cli-reference)
- [架构](#architecture)

---

## 快速开始

### 环境要求
- Rust 1.82+
- Anthropic API Key（或兼容端点：DeepSeek、GLM 等）
- `ripgrep`（可选，用于 Grep 工具）
- `cloudflared`（可选，用于 `--tunnel` 远程访问 — [安装指南](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/)）
- Node.js & npm（用于构建前端，通过 `install.sh` 安装）

### 安装

```bash
# Linux / macOS — 一行命令:
cd rust && bash install.sh
# → 安装到 ~/.local/bin/nonoclaw，配置前端

# Windows:
powershell -ExecutionPolicy Bypass -File install.ps1
```

### 运行

```bash
# Web 界面（支持多模型切换）:
nonoclaw --serve-http 127.0.0.1:8765 --model deepseek-v4-pro

# 使用 Cloudflare Tunnel（手机扫码即可从任何网络访问）:
nonoclaw --serve-http 127.0.0.1:8765 --tunnel

# 命令行模式:
nonoclaw -p "解释 Rust 所有权机制"
```

---

## 功能特性

| 类别 | 详情 |
|---|---|
| **智能体循环** | 流式 SSE、自动重试、多轮 tool-use/tool-result 配对、**孤立修复**（自动修复断裂的 tool_use/tool_result 对）、**thinking 块过滤**（Bedrock 代理兼容）、**分批并行工具执行**（并发上限=10） |
| **跨会话记忆 (Mneme)** | 三层架构：**Facts**（`memory/facts/*.md` 中的不可变知识）、**Beads**（`memory/beads/*.md` 中的任务连续性）、**Transcript**（每次会话的 JSONL 记录）。BM25 搜索 + 重要性排序。`Memory` 工具（第 18 个内置工具）。每次会话自动注入 SystemBlock #2。Git 友好的 Markdown 文件。灵感来源于 agentmemory。 |
| **系统提示词** | 增强型：手术级编辑规则、6 种命名失败模式、反过度工程规则、ToolSearch 使用指南、**Git 上下文在非缓存块中**（缓存跨轮次保持有效）、**记忆回写指令** |
| **18 个内置工具** | Read、Write、Edit、Bash、Grep、Glob、TodoWrite、WebFetch、WebSearch、Agent、AskUserQuestion、Coordinator、ToolSearch、TaskCreate/Get/List/Update、**Memory** |
| **文件附件** | 通过纸夹按钮、拖拽或粘贴上传 PDF/DOCX/DOC/TXT/MD/PNG/JPG；通过可配置的 Mistral/DeepSeek 文档模型**自动 OCR**；**直接文本提取**（pdftotext + ZIP XML）尽可能跳过 OCR；**嵌入图片提取**（pdfimages + word/media）并为每张图片生成 OCR 描述；多模态模型的 **ContentBlock::Image 注入** |
| **Bash 后台任务** | `run_in_background: true` 启动分离进程，输出持久化到磁盘，完成时注入 `<task_notification>` |
| **MCP** | 客户端（`--mcp-config`）+ 服务端（`--mcp-serve`），**MCP prompts → skill 桥接** |
| **统一模型配置** | 所有模型集中在单一 `models[]` 数组，通过 `role` 标签（`main`/`doc`/`compact`）区分；`docModel` 和 `compactModel` 通过名称引用；**每模型专属 contextWindow / maxTokens / charsPerToken**；**compactModel** 独立的摘要压缩模型 |
| **多模型切换** | 通过 UI 下拉框或 `/multi` 斜杠命令切换模型；`/multi` 语法错误时显示帮助提示 |
| **权限** | 5 种模式：Default / AcceptEdits / Auto / BypassPermissions / Plan——通过 UI 下拉框切换 |
| **会话** | 按工作目录的 JSONL 持久化，`--resume` / `--continue` / `--list-sessions`，**会话命名**，渐进式元数据 |
| **上下文** | 自动压缩 `compactThreshold` tokens，可配置 `contextWindow`，**Prompt Cache**（ephemeral，Git 排除在缓存外），**每模型 token 估算**（charsPerToken） |
| **技能** | `/skill-name` 注入，**12 个内置技能**，通过路径/触发器/文件发现**动态激活**，参数替换，fork 上下文，使用追踪，热重载 |
| **插件** | `--plugin-add`，通过 `.nonoclaw/hooks.json` 配置钩子（**shell + prompt + HTTP**，12 种事件类型） |
| **任务系统** | 文件持久化任务存储，依赖图，owner 分配，状态生命周期 |
| **Web 界面** | 生物发光暗色主题，呼吸式 aurora 背景，文件树，Git 面板，Insight 手风琴，Markdown+KaTeX，**工具卡片自动折叠 + 命令预览**，**附件 chips 上传状态**，**"Nono" 助手标签** |
| **PWA** | 添加到主屏幕，离线 SW 缓存，可在 Android/iOS 上安装 |
| **手机同步** | 二维码 → 共享 session → 实时 MessagesLoaded 广播；**skipOneLoad** 确保可靠的端到端同步；**sync_session_to_peers** 在 Run/Clear/运行后广播；**markClearing** 防止工具卡片残留 |
| **隧道** | `--tunnel` 自动启动 Cloudflare Tunnel，终端打印 ASCII 二维码，实现公网 HTTPS 访问 |
| **导出** | Markdown 复制 + `.md` 文件下载助手回复 |

---

## 多模型与多供应商

所有模型集中在单一 `models[]` 数组中，通过 `role` 标签区分——对话模型、文档处理模型和压缩模型：

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

**模型角色**:
| 角色 | 用途 | UI 下拉框 |
|------|------|:--------:|
| `main`（或空缺） | 对话模型 | 是 |
| `doc` | 文档处理（OCR / 视觉） | 否 |
| `compact` | 摘要 / 压缩 | 否 |

一个模型可以有多个角色——例如 `["main", "compact"]` 表示同一个模型同时用于对话和摘要压缩。

**每模型字段**: `contextWindow`（总 tokens）、`maxTokens`（输出上限）、`charsPerToken`（分词器估算）——覆盖全局默认值。

**运行时切换**: 状态栏的模型名称变为下拉框（需配置 2+ 个 `main` 模型）。切换会为每次运行重建 API `Client`——无需重启。

**`/multi` 斜杠命令**: 用多个模型对比回答：
```
/multi deepseek-v4-pro,glm-5.2 比较 Rust 和 Go 的错误处理机制
```
将提示词依次发送给两个模型，每个回复都标注模型名称。输入错误时显示语法帮助。

### 文档处理（文件附件）

点击纸夹按钮 (📎)、拖拽文件或粘贴上传。支持格式：PDF、DOCX、DOC、TXT、MD、PNG、JPG。

**处理管道**:
```
上传
├─ TXT/MD → 直接读取
├─ PDF    → pdftotext（文字）+ pdfimages（嵌入图片）→ 扫描件则 OCR
├─ DOCX   → ZIP XML <w:t>（文字）+ word/media/（嵌入图片）→ 文字稀少则 OCR
└─ 图片   → DeepSeek OCR 2（切片式）或 Mistral OCR
```

**文档模型提供商**（`provider` 从模型名称自动推断）:
| 提供商 | 模型名称特征 | API 格式 |
|--------|------------|----------|
| `mistral_ocr` | 包含 `mistral` | `POST /v1/ocr` |
| `deepseek_ocr` | 包含 `deepseek`+`ocr` | `POST /v1/chat/completions`（切片式） |
| `generic_vision` | 其他 | `POST /v1/chat/completions` |

嵌入图片会单独 OCR，因此纯文本模型（DeepSeek V4）也能通过内联描述"看到"图片内容。多模态模型（Sonnet）会同时收到 `ContentBlock::Image` 块和 OCR 文本。

---

## 跨会话记忆 (Mneme)

NonoClaw 采用受 [agentmemory](https://github.com/rohitg00/agentmemory) 启发的三层记忆系统。每个会话启动时，前一个会话的知识和任务状态会自动注入到上下文中——不再"从头开始"和重复解释。

### 架构

```
┌──────────────────────────────────────────────────────────┐
│ 第三层：TRANSCRIPT — 每次会话的 JSONL（自动）              │
│   ~/.nonoclaw/projects/<cwd>/sessions/<uuid>.jsonl      │
├──────────────────────────────────────────────────────────┤
│ 第二层：BEADS — 任务连续性（跨会话保持）                   │
│   <cwd>/.nonoclaw/memory/beads/*.md                      │
│   活跃任务、阻塞项、进度跟踪。                              │
├──────────────────────────────────────────────────────────┤
│ 第一层：FACTS — 不可变知识（永久保存）                     │
│   <cwd>/.nonoclaw/memory/facts/*.md                      │
│   约定、偏好、决策、Bug 模式。                             │
└──────────────────────────────────────────────────────────┘
```

### Facts (`memory/facts/*.md`)

每个不可变事实一个 Markdown 文件，带 YAML frontmatter。类型：`preference`、`convention`、`decision`、`architecture`、`bug`。事实**永不删除**——错误的通过 `superseded_by` 标记指向替代者。

```markdown
---
name: pip-use-tsinghua-mirror
title: pip 安装使用清华镜像源
type: preference
importance: 0.9
confidence: 0.95
tags: [python, pip, china]
---

安装 Python 包时始终使用
pip install -i https://mirrors.tuna.tsinghua.edu.cn/pypi/web/simple
```

### Beads (`memory/beads/*.md`)

每个 Bead 追踪一个跨会话的活跃任务。状态：`todo` → `in_progress` → `blocked` → `done`。

```markdown
---
id: bead-abc123
title: 修复生产环境登录超时
status: in_progress
priority: 8
session: abc123
---

## 进展
- [x] 在预发环境复现
- [ ] 根因：连接池耗尽
- [ ] 实现熔断器

## 阻塞项
无。
```

### Memory 工具（第 18 个内置工具）

| 操作 | 描述 |
|------|------|
| `Memory search <query>` | 对所有事实进行 BM25 搜索，按相关性 × 重要性排序 |
| `Memory save` | 创建或更新事实（name、title、type、importance、tags） |
| `Memory forget <name>` | 将事实标记为已取代 |
| `Memory beads` | 列出所有活跃（未完成）的 beads，按优先级排序 |
| `Memory bead_save` | 创建或更新任务 bead |
| `Memory bead_done <id>` | 将 bead 标记为完成 |

模型也可以使用标准的 `Read`/`Write`/`Edit` 工具直接操作 `memory/` 文件。

### 上下文注入

会话启动时，`SystemBlock #2`（非缓存）自动包含：

```
## 活跃任务 (beads)
◌ 修复登录超时 [优先级 8]
  正在调查连接池问题...

## 关键事实
- **pip-use-tsinghua-mirror** (偏好): pip 使用清华源
- **rust-edition-2024** (约定): 新项目使用 Rust 2024
```

活跃 beads（最多 5 个）+ 重要事实（最多 10 个）。总量上限 50KB。

### 示例会话流程

```
会话 1 — 发现
  You:   "pip install 太慢了"
  Nono:  "网络问题。记住用清华源可以吗？"
  You:   "好"
  Nono:  → Memory save: pip-use-tsinghua-mirror.md
         → Memory bead_save: 优化 pip 安装速度

会话 2 — 第二天（自动恢复）
  [系统提示词已包含:]
    ◌ 优化 pip 安装速度 [已完成, 会话 1]
    - pip-use-tsinghua-mirror (偏好)

  You:   "装一个 requests 库"
  Nono:  "pip install -i https://mirrors.tuna.tsinghua.edu.cn/pypi/web/simple requests"
         ↑ 自动用了清华源，不需要你再提醒

会话 3 — 一周后
  You:   "这个项目之前遇到过什么网络问题？"
  Nono:  → Memory search: "network pip mirror"
         返回 pip-use-tsinghua-mirror 事实，告诉你历史上下文
```

---

## 权限模式

所有模式可在运行时通过 UI 下拉框切换（状态栏，模型下拉框旁边）：

| 模式 | 行为 | 颜色 |
|---|---|---|
| `default` | 只读工具自动允许；写入操作弹出对话框 | 薄荷绿 |
| `acceptEdits` | 自动允许 Read + Write + Edit；Bash 仍弹出提示 | 紫罗兰 |
| `auto` | 自动允许**所有操作**——无任何提示 | 薄荷绿 |
| `bypassPermissions` | 跳过**所有**检查（= `--dangerously-skip-permissions`） | 红色 |
| `plan` | 只读模式：写入操作被**硬拒绝** | 天蓝 |

也可通过 `settings.json` 配置：
```json
{ "permissions": { "defaultMode": "auto" } }
```

---

## Web 界面

使用 `--serve-http 127.0.0.1:8765` 启动并打开浏览器。

### 布局（三栏）
```
┌─ 状态栏 ───────────────────────────────────────────────────┐
│ «NonoClaw»  [模型▾] [模式▾]  tokens · session  ◰ 主题 ●   │
├──────┬──────────────────────────────────┬───────────────────┤
│ 文件 │  聊天区 (Markdown + KaTeX)       │ INSIGHT 手风琴
│ 树   │  ─────────────────────           │  ▸ 工具 (18)
│      │  消息气泡                        │  ▸ MCP 服务器
│──────│  用户/助手/工具卡片              │  ▸ 模型
│ GIT  │                                  │  ▸ 技能
│ 面板 │  ┌─ 输入框 ─── [发送↗] ───┐    │  ▸ 钩子
│      │  └─────────────────────────┘    │  ▸ 斜杠命令
│      │                                  │  ▸ 文档与配置
│      │                                  │  ▸ CLI 参考
│      │                                  │  ▸ 项目信息
└──────┴──────────────────────────────────┴───────────────────┘
```

### 主要 UI 功能
- **呼吸式背景** — aurora 光球随 token 输出速度脉动
- **三种主题** — Biolume（青/薄荷）· Amber Forge（金色）· Glacial Frost（冰蓝）— 通过状态栏圆点切换
- **文件树** — 点击文件 → 用系统默认编辑器打开；Shift+点击 → VS Code
- **Git 面板** — 分支、领先/落后、暂存/修改/未追踪计数、最近提交（点击 → `git show` 弹窗）、按作者/主题过滤
- **Insight 手风琴** — 工具（点击 → 展开输入 schema + 提示词预览）、MCP 服务器、模型、技能、钩子、斜杠命令、文档与配置（可点击编辑）、CLI 参考、项目信息
- **Markdown 渲染** — GFM 表格、KaTeX 数学公式（行内 `$...$` 和块 `$$...$$`）、语法高亮
- **复制与导出** — 复制助手回复为 Markdown；下载为 `.md` 文件

### 斜杠命令（在输入框中输入）
| 命令 | 描述 |
|---|---|
| `/clear` | 重置对话（内存 + 磁盘） |
| `/compact` | 压缩长上下文 |
| `/skill-name` | 将技能指令注入系统提示词（可带参数：`/deploy prod main`） |
| `/multi model1,model2 <prompt>` | 用多个模型对比回答 |
| `/rename <title>` | 设置自定义会话标题 |

---

## 移动端与远程访问

### 二维码 + 会话同步

1. 桌面端：`nonoclaw --serve-http 127.0.0.1:8765` → 点击状态栏 ◰ → 二维码出现
2. 手机扫描二维码（同一局域网或隧道）→ 浏览器打开 `?token=...&session=...`
3. 手机加入桌面端的**同一会话**——共享 `SessionHandle`，实时 `MessagesLoaded` 广播
4. "添加到主屏幕" → 独立 PWA 应用

### Cloudflare Tunnel (`--tunnel`)

```bash
# 一次性：安装 cloudflared
curl -L https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 -o ~/bin/cloudflared
chmod +x ~/bin/cloudflared

# 启动隧道：
nonoclaw --serve-http 127.0.0.1:8765 --tunnel
```

流程：
1. NonoClaw 自动启动 `cloudflared tunnel --url http://127.0.0.1:8765`
2. 捕获 cloudflared 输出中的 `*.trycloudflare.com` URL
3. 在终端打印 **ASCII 二维码**（可立即扫码）
4. 自动设置 `public_url`——Web 界面的 QR 按钮使用隧道 URL

手机可以从任何网络访问 NonoClaw（4G/5G、不同 WiFi、海外）——无需端口转发，无需公网 IP。

### 会话同步逻辑

```
桌面端连接 → shared_sid = 最近会话 → registry["abc123"]
手机扫码    → shared_sid = URL 中的 "abc123" → registry["abc123"].txs += phone

桌面端发送 Run → 事件仅流式传输到桌面端
               → 完成后：MessagesLoaded 广播到手机
               → 手机 UI 自动更新
```

---

## 技能与插件

### 技能 (`/skill-name`)

创建 `.nonoclaw/skills/<name>/SKILL.md`，带 YAML frontmatter：

```markdown
---
name: deploy
description: 部署项目到生产环境
argument-hint: "<env> <branch>"
arguments: [env, branch]
paths: [deploy/**]
triggers: ["deploy|ship|release"]
when_to_use: 当用户要求部署或发布代码时
allowed-tools: [Bash, Read, Write]
context: fork
---
# 部署
运行 `./deploy.sh --env=$1 --branch=$2`
```

#### 支持的 Frontmatter 字段

| 字段 | 描述 |
|---|---|
| `name` | 技能名称（作为 `/name` 使用） |
| `description` | 一行用途描述 |
| `paths` | Glob 模式——匹配文件被读/写/编辑时自动激活技能 |
| `triggers` | 正则模式——用户输入匹配时自动激活技能 |
| `when_to_use` | 注入系统提示词的自然语言使用指南 |
| `allowed-tools` | 限制技能可使用的工具 |
| `argument-hint` | CLI 使用提示（显示在自动补全中） |
| `arguments` | `$1`、`$2` 替换所用的位置参数名称 |
| `version` | 技能版本字符串 |
| `model` | 技能激活时覆盖模型 |
| `disable-model-invocation` | 若为 true，模型不能自动调用——仅限斜杠命令 |
| `user-invocable` | 是否可通过 `/name` 调用（默认：true） |
| `context` | `"fork"` 生成隔离子代理；否则内联 |
| `agent` | `context` 为 `"fork"` 时的代理类型 |
| `effort` | 思考深度（`low`/`medium`/`high`） |
| `shell` | Shell 覆盖（`bash`/`powershell`） |

#### 动态激活（CC 兼容）

技能不仅是静态的 `/name` 命令——它们可以动态激活：

| 机制 | 工作方式 |
|---|---|
| **`paths`** | Read/Write/Edit 操作触及匹配文件后，技能自动激活（gitignore 风格 glob 匹配） |
| **`triggers`** | 用户输入正则匹配 → 在第一轮对话前自动加载技能 |
| **文件发现** | 从操作文件路径向上遍历，在会话中途发现嵌套的 `.nonoclaw/skills/` 目录 |
| **条件技能** | 带有 `paths` 的技能延迟加载，直到匹配文件被触及（减少系统提示词臃肿） |

#### 参数替换
技能正文支持 CC 兼容的变量展开：
- `$1`、`$2` — 从 `/name arg1 arg2` 获取的位置参数
- `$ARGUMENTS` — 原始参数字符串
- `$ARGUMENTS[0]`、`$ARGUMENTS[1]` — 索引访问
- `${NONOCLAW_SKILL_DIR}` — 技能自身目录路径
- `${NONOCLAW_SESSION_ID}` — 当前会话 UUID

#### 内置技能（12 个）
无需磁盘扫描始终可用：`verify`、`simplify`、`debug`、`remember`、`loop`、`update-config`、`keybindings-help`、`claude-api`、`code-review`、`init`、`review`、`security-review`

#### 使用追踪
技能调用记录持久化到 `~/.nonoclaw/skill-usage.json`，7 天半衰期衰减——常用技能在列表中排名更高。

#### 热重载
编辑磁盘上的 `SKILL.md` → 通过 `notify` 文件监视器在 500ms 内反映更改（无需重启）。

### 插件

```bash
nonoclaw --plugin-add /path/to/plugin      # 本地目录
nonoclaw --plugin-add https://github.com/... # Git URL
```
安装到 `~/.nonoclaw/plugins/`。插件贡献的技能自动发现。

### 钩子（`.nonoclaw/hooks.json`）

支持三种钩子类型——**Shell 命令**、**LLM Prompt 评估**和 **HTTP POST**：

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

| 钩子类型 | 行为 |
|---|---|
| **Shell**（`command` + `args`） | 执行子进程；`PreToolUse` 非零退出 → 阻止工具调用 |
| **Prompt**（`prompt`） | 用小模型（如 Haiku）结合钩子上下文进行评估，强制 JSON Schema `{ ok, reason? }` |
| **HTTP**（`http`） | 向 URL POST JSON 负载，支持 URL/headers 中的环境变量插值 |

**12 种事件类型**: `PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`Notification`、`UserPromptSubmit`、`SessionStart`、`SessionEnd`、`Stop`、`SubagentStart`、`SubagentStop`、`PreCompact`、`PostCompact`

---

## 配置 (settings.json)

完整示例（`~/.nonoclaw/settings.json`）:

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

### 顶层字段

| 字段 | 描述 |
|---|---|
| `model` | 默认模型（当 `models[]` 不存在时使用） |
| `contextWindow` | 全局上下文窗口（被每模型的 `contextWindow` 覆盖） |
| `maxTokens` | 全局每轮最大输出 tokens（被每模型的 `maxTokens` 覆盖） |
| `charsPerToken` | 全局 chars-per-token 估算器（默认 4；可被每模型覆盖） |
| `env` | 启动时注入的环境变量 |
| `models[]` | 所有模型配置：`name`、`label`、`baseUrl`、`apiKey`、`role[]`、`default`、`contextWindow`、`maxTokens`、`charsPerToken` |
| `docModel` | 文档处理（OCR）的模型名称引用 |
| `compactModel` | 自动压缩摘要的模型名称引用 |
| `mcpServers` | MCP 服务器配置：`command`、`args`、`env` |
| `permissions.defaultMode` | `default` / `acceptEdits` / `auto` / `bypassPermissions` / `plan` |
| `permissions.allow` | 始终允许的工具模式 |
| `permissions.deny` | 始终拒绝的工具模式 |
| `compactThreshold` | 自动压缩触发阈值（估算 tokens） |
| `autoCompact` | 启用/禁用自动压缩 |

---

## CLI 参考

```bash
# Web 界面
nonoclaw --serve-http 127.0.0.1:8765 --tunnel

# 命令行模式
nonoclaw -p "总结 README"
echo "修复这个 bug" | nonoclaw -p --allowed-tools Read,Edit,Bash

# 会话
nonoclaw --continue "继续"
nonoclaw --list-sessions
nonoclaw --resume abc123 "恢复特定会话"

# MCP
nonoclaw --mcp-config servers.json "调用天气工具"
nonoclaw --mcp-serve  # 作为 MCP 服务器暴露

# 插件
nonoclaw --plugin-add ~/my-hooks
```

### 关键参数

| 参数 | 默认值 | 描述 |
|---|---|---|
| `--model` | `claude-sonnet-4-5` | 覆盖模型 |
| `--max-turns` | 200 | 最大智能体循环轮次 |
| `--max-tokens` | 8192 | 每轮最大输出 |
| `--permission-mode` | `default` | 权限模式 |
| `--context-window` | — | 模型上下文大小（自动推导压缩阈值） |
| `--compact-threshold` | 80000 | 估算 token 的自动压缩触发值 |
| `--no-auto-compact` | false | 禁用自动压缩 |
| `--allowed-tools` | — | 逗号分隔的工具白名单 |
| `--disallowed-tools` | — | 逗号分隔的工具黑名单 |
| `--dangerously-skip-permissions` | — | 绕过所有权限检查 |
| `--append-system-prompt` | — | 附加系统提示词文本 |
| `--name` | — | 启动时设置自定义会话标题 |
| `--tunnel` | false | 自动启动 cloudflared |
| `--public-url` | — | 覆盖二维码 URL |
| `--settings` | — | 显式指定设置文件路径 |

---

## 架构

```
NonoClaw/
├── src/               TypeScript 参考实现（只读，不在 git/build 中）
├── rust/              Rust 重写版本（活跃开发）
│   ├── crates/
│   │   ├── core/      nonoclaw-core     — 消息、用量、权限
│   │   ├── api/       nonoclaw-api      — Anthropic 流式客户端
│   │   ├── tools/     nonoclaw-tools    — 工具 trait + 注册表 + 18 个内置工具 + MCP + 后台任务
│   │   ├── engine/    nonoclaw-engine   — 查询循环 + 提示词 + 压缩 + 会话 + 技能 + 钩子
│   │   └── cli/       nonoclaw（二进制）— CLI + Web 界面 + 远程 + 技能监视器 + 项目信息
│   ├── install.sh / install.ps1
│   └── Cargo.toml
├── frontend/          React + Vite (TypeScript)
│   ├── src/           组件、状态管理、WebSocket 客户端
│   ├── index.html     CSS 设计 tokens
│   └── package.json
├── .gitignore
└── README.md
```

---

## 环境变量

| 变量 | 描述 |
|---|---|
| `ANTHROPIC_API_KEY` | API 密钥 |
| `ANTHROPIC_BASE_URL` | 自定义 API 端点 |
| `ANTHROPIC_AUTH_TOKEN` | Bearer 认证（替代方式） |
| `NONOCLAW_HOME` | 覆盖数据根目录（`~/.nonoclaw`） |
| `SERPER_API_KEY` / `BRAVE_API_KEY` | WebSearch 后端 |
| `NONOCLAW_MAX_TOOL_CONCURRENCY` | 最大并行工具执行数（默认：10） |
| `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS` | 禁用 `run_in_background`（默认：启用） |
| `RUST_LOG` | 日志级别（`debug`、`info`、`warn`） |

## License

MIT
