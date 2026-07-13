# NonoClaw

**English** · [中文](#nonoclaw-zh)

A **Rust rewrite** of [Claude Code](https://claude.ai/code) (Anthropic's agent CLI). Full agentic loop, tool dispatch, permission system, session persistence, MCP client/server, TUI, and a **Web UI** with mobile PWA support. Strictly references the TypeScript source in `src/`.

> **Goal**: a native CLI coding agent with a complete tool ecosystem, remote access, and a beautiful web interface.

---

## Features

### Core Engine
- **Streaming agent loop** — Anthropic Messages API with SSE parsing, auto-retry, multi-turn execution
- **Tool system** — `Tool` trait, 12 built-in tools + MCP dynamic loading + custom skills
- **Permission gate** — 5 modes: Default / AcceptEdits / Auto / BypassPermissions / Plan
- **Subagent recursion** — Agent tool for subtasks; Coordinator tool for **parallel** subagent dispatch
- **Auto-compaction** — Smart summarisation of old conversations at plain-user-message boundaries
- **Tool concurrency** — `is_concurrency_safe` tools execute in **parallel** within the same turn
- **Multi-model support** — Pre-define profiles in `settings.json` (base_url + api_key per model); switch at runtime via UI dropdown
- **Skills** — `/skill-name` injects skill instructions (with reference file loading) into system prompt

### User Interfaces
- **Web UI** (`--serve-http`) — Bio-luminescence dark theme with breathing aurora background, three-column layout, file tree, Git pane, Insight accordion, real-time chat
- **PWA mobile** — Add to Home Screen, QR code auth/session-sync for remote access, mobile-responsive
- **Cloudflare Tunnel** (`--tunnel`) — Auto-spawn cloudflared for public internet access (zero-config HTTPS)
- **Interactive TUI** — ratatui + crossterm: REPL message flow, streaming text, status bar, permission popups
- **Headless** (`-p` / `--print`) — Single-shot mode for scripts/SDK; text / JSON output

### Sessions & Data
- **Session persistence** — JSONL stored at `~/.nonoclaw/projects/<cwd>/sessions/<id>.jsonl`
- **Resume** — `--resume <id>` / `--continue` / `--list-sessions`
- **Skills loading** — `.nonoclaw/skills/*/SKILL.md` + `~/.nonoclaw/skills/` + plugin-contributed
- **Plugin hooks** — `.nonoclaw/hooks.json` with PreToolUse / PostToolUse / session lifecycle hooks

### MCP (Model Context Protocol)
- **MCP client** (`--mcp-config`) — Connect to external MCP servers (stdio JSON-RPC)
- **MCP server** (`--mcp-serve`) — Expose built-in tools as an MCP server over stdio

---

## Quick Start

### Requirements
- **Rust 1.82+**
- **Anthropic API Key** (or compatible endpoint like DeepSeek / GLM)
- `ripgrep` (optional, for the Grep tool)
- `cloudflared` (optional, for `--tunnel`)

### Build & Install

```bash
# One-step install (Linux/macOS):
cd rust && bash install.sh

# Windows:
# powershell -ExecutionPolicy Bypass -File install.ps1

# Or just build:
cd rust && cargo build --release
# Binary: rust/target/release/nonoclaw (~5-6 MB)
```

### Configure

```bash
export ANTHROPIC_API_KEY=sk-ant-...
export ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic  # optional
```

Or use `~/.nonoclaw/settings.json`:
```json
{
  "model": "deepseek-v4-pro",
  "contextWindow": 1048576,
  "maxTokens": 8192,
  "models": [
    { "name": "deepseek-v4-pro", "label": "DeepSeek V4",
      "baseUrl": "https://api.deepseek.com/anthropic",
      "apiKey": "sk-xxx", "default": true },
    { "name": "glm-5.2", "label": "GLM 5.2",
      "baseUrl": "https://open.bigmodel.cn/api/anthropic",
      "apiKey": "sk-yyy" }
  ],
  "env": {
    "ANTHROPIC_API_KEY": "sk-xxx",
    "BRAVE_API_KEY": "your-brave-key"
  },
  "mcpServers": {
    "one_search": { "command": "python3.7", "args": ["/path/to/server.py"] }
  }
}
```

### First Run

```bash
# Web UI (default):
nonoclaw --serve-http 127.0.0.1:8765
# Open http://127.0.0.1:8765

# With Cloudflare Tunnel (public internet access):
nonoclaw --serve-http 127.0.0.1:8765 --tunnel

# Headless:
nonoclaw -p "What is Rust ownership?"

# Interactive TUI:
nonoclaw
```

---

## Execution Modes

### 1. Web UI (`--serve-http`)
Start the HTTP + WebSocket server and open the browser.

### 2. Headless (`-p`)
Single-shot mode for scripting. `--output-format text|json`.

### 3. Remote Sessions
```bash
nonoclaw --serve 127.0.0.1:8765              # server (TCP + JSON-lines)
nonoclaw --remote 127.0.0.1:8765 "prompt"     # client
```

### 4. MCP Mode
```bash
nonoclaw --mcp-config mcp.json "use external tools"  # MCP client
nonoclaw --mcp-serve                                    # MCP server
```

### 5. All CLI Flags

| Flag | Description |
|---|---|
| `-p, --print` | Force headless mode |
| `--model <ID>` | Override model |
| `--permission-mode <MODE>` | `default` / `acceptEdits` / `auto` / `bypassPermissions` / `plan` |
| `--allowed-tools <LIST>` | Comma-separated tool allowlist |
| `--disallowed-tools <LIST>` | Comma-separated tool denylist |
| `--dangerously-skip-permissions` | Skip all checks (= `bypass`) |
| `--max-turns <N>` | Max turns (default 200) |
| `--max-tokens <N>` | Max output tokens/turn (default 8192) |
| `--append-system-prompt <TXT>` | Append to system prompt |
| `--add-dir <PATH>` | Extra CLAUDE.md search dir |
| `--output-format text\|json` | Headless output format |
| `--mcp-config <PATH>` | MCP server config JSON |
| `--resume <ID>` | Resume session by id |
| `--continue` | Resume most recent session |
| `--list-sessions` | List sessions and exit |
| `--no-session` | Disable session persistence |
| `--compact-threshold <N>` | Auto-compact threshold (tokens) |
| `--no-auto-compact` | Disable auto-compaction |
| `--settings <PATH>` | Explicit settings file |
| `--serve-http <ADDR>` | Start web UI server |
| `--tunnel` | Auto-spawn cloudflared tunnel |
| `--public-url <URL>` | QR code public URL |
| `--context-window <N>` | Model context window (tokens) |
| `--plugin-add <SOURCE>` | Install plugin |
| `--verbose` | Verbose logging |

---

## Architecture

```
NonoClaw/
├── src/               TypeScript reference implementation (Claude Code source)
├── rust/              Rust rewrite
│   ├── crates/
│   │   ├── core/      nonoclaw-core     — messages, usage, permissions, errors
│   │   ├── api/       nonoclaw-api      — Anthropic streaming client (SSE)
│   │   ├── tools/     nonoclaw-tools    — Tool trait + registry + builtins + MCP
│   │   ├── engine/    nonoclaw-engine   — query loop + system prompt + compact + session
│   │   └── cli/       nonoclaw (bin)    — CLI + TUI + Web UI + remote + skills
│   ├── Cargo.toml     workspace
│   ├── install.sh     Linux/macOS installer
│   └── install.ps1    Windows installer
├── frontend/          React + Vite web UI (PWA)
│   ├── src/           TypeScript + TSX components
│   ├── index.html     design tokens + CSS
│   └── package.json
├── .gitignore
└── README.md
```

## Environment Variables

| Variable | Description |
|---|---|
| `ANTHROPIC_API_KEY` | API Key (**required**) |
| `ANTHROPIC_BASE_URL` | Custom base URL (default `api.anthropic.com`) |
| `ANTHROPIC_AUTH_TOKEN` | Bearer auth (alternative to api key) |
| `HTTP_PROXY` / `ALL_PROXY` | HTTP proxy |
| `NONOCLAW_HOME` | Override session/plugin storage root (default `~/.nonoclaw`) |
| `SERPER_API_KEY` | Serper backend for WebSearch |
| `BRAVE_API_KEY` | Brave Search backend for WebSearch |
| `RUST_LOG` | tracing log level (`warn`, `debug`, etc.) |

---

## License

MIT

---

## NonoClaw 中文

**NonoClaw** 是 [Claude Code](https://claude.ai/code)（Anthropic 智能体 CLI）的 **Rust 重写版本**。以 `src/` 中的 TypeScript 源码为参考实现，完整复刻智能体循环、工具派发、权限系统、会话持久化、MCP、TUI 及 Web 界面，并支持移动端 PWA。

### 快速开始

```bash
cd rust && bash install.sh                    # 一键安装到 ~/.local/bin
nonoclaw --serve-http 127.0.0.1:8765           # Web UI
nonoclaw --serve-http 127.0.0.1:8765 --tunnel   # 带隧道（外网访问）
nonoclaw -p "总结一下 README"                   # 无头模式
nonoclaw                                         # TUI 交互
```

### 执行模式
- **Web UI** (`--serve-http`) — 三栏布局 + 呼吸背景 + 文件树 + Git + Insight
- **PWA 移动端** — 扫码连接 + 添加到主屏幕 + session 同步
- **无头模式** (`-p`) — 适合脚本/SDK
- **远程会话** (`--serve` / `--remote`)
- **MCP 模式** (`--mcp-config` / `--mcp-serve`)

### 配置

`~/.nonoclaw/settings.json`:
```json
{
  "model": "deepseek-v4-pro",
  "contextWindow": 1048576,
  "models": [
    { "name": "deepseek-v4-pro", "label": "DeepSeek V4",
      "baseUrl": "https://api.deepseek.com/anthropic",
      "apiKey": "sk-xxx", "default": true },
    { "name": "glm-5.2", "label": "GLM 5.2",
      "baseUrl": "https://open.bigmodel.cn/api/anthropic",
      "apiKey": "sk-yyy" }
  ]
}
```

### 内置工具
Read · Write · Edit · Bash · Grep · Glob · TodoWrite · WebFetch · WebSearch · Agent · AskUserQuestion · Coordinator + MCP 动态加载

### 权限模式
`default` · `acceptEdits` · `auto` · `bypassPermissions` · `plan`

### 特性
- 会话持久化 (JSONL) + resume / continue
- 自动上下文压缩
- Skills (`/skill-name` 注入)
- 插件 hooks
- 多模型切换（UI 下拉框）
- Cloudflare Tunnel 内网穿透
- 终端 ASCII 二维码
- PWA 离线安装
