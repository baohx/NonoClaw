# NonoClaw

**NonoClaw** 是 [Claude Code](https://claude.ai/code)(Anthropic 的智能体命令行工具)的 Rust 重写版本。它以 `src/` 中的 TypeScript 源码为严格参考实现,完整复刻了智能体循环、工具派发、权限系统、会话持久化、MCP 客户端/服务端等全部核心能力,并增加了 TUI 交互界面。

> 目标:生成一个可用于编码辅助的本机 CLI 智能体,具备完整的工具生态、网络能力与多种执行模式。

---

## 功能总览

### 核心引擎
- **流式智能体循环** — 与 Anthropic Messages API 通信,支持 `tool_use`/`tool_result` 配对、**streaming SSE 解析**、自动重试
- **工具系统** — `Tool` trait,9 个内置工具 + MCP 动态加载 + 自定义 skill
- **权限门禁** — 4 级模式:默认 / AcceptEdits / Auto / Bypass;辅助头和交互两种决议路径
- **子智能体递归** — Agent 工具派生子任务;Coordinator 工具**并行派发**多个子智能体
- **自动上下文压缩** — 会话过长时自动摘要旧消息,保留近期的完整上下文
- **工具并发** — `is_concurrency_safe` 的工具在同一回合内**并行执行**

### 用户界面
- **交互式 TUI**(ratatui + crossterm):REPL 消息流、流式文本渲染、状态栏、权限弹窗、**多选问题弹窗**(AskUserQuestion)、行编辑器(光标/词移/历史/多行)
- **无头模式**(`--print`):适合脚本/SDK 调用;支持 text / JSON 双输出格式
- **远程会话**(Phase 6):`--serve` TCP 服务器 + `--remote` 客户端 + `--bridge`(本地 TUI ↔ 远程引擎)
- **MCP 服务端模式**(`--mcp-serve`):以 JSON-RPC over stdio 暴露工具,可被 Claude Desktop 等 MCP 客户端驱动

### 会话与数据
- **会话持久化** — JSONL 存储在 `~/.nonoclaw/projects/<cwd>/sessions/<id>.jsonl`
- **`--resume <id>`** / **`--continue`** / **`--list-sessions`** — 随时恢复历史会话
- **skills 加载** — `.claude/skills/*/SKILL.md` + `~/.claude/skills/` + 插件贡献的 skill
- **插件安装**(`--plugin-add`) — 本地目录复制 / git clone 到 `~/.claude/plugins/`
- **插件 hooks** — `.claude/hooks.json` 定义 PreToolUse / PostToolUse

### MCP 双向闭环
- **MCP 客户端**(`--mcp-config`) — 连接外部 MCP server(stdio JSON-RPC),动态适配其工具
- **MCP 服务端**(`--mcp-serve`) — 以 MCP server 身份运行,对外暴露内置工具

---

## 快速开始

### 环境要求
- **Rust 1.82+** (实际工具链 1.93)
- **Anthropic API Key**(或其他兼容端点,如 GLM)
- `ripgrep`(可选,Grep 工具需要;未安装时 Grep 报错但不影响其他工具)

### 构建

```bash
cd rust
cargo build --release
```

Release binary 约 5–6 MB。可用 `./target/release/nonoclaw --version` 验证。

### 配置 API 端点

```bash
# 必选:Anthropic API key
export ANTHROPIC_API_KEY=sk-ant-...

# 可选:自定义 base URL(如使用 GLM 等兼容端点)
export ANTHROPIC_BASE_URL=https://open.bigmodel.cn/api/anthropic

# 可选:HTTP 代理
export HTTP_PROXY=http://proxy:port
```

### 首次运行

```bash
# 无头模式(单次问答)
nonoclaw -p "用一句话介绍 Rust 的 ownership 特性"

# 交互式 TUI(终端内启动)
nonoclaw
```

---

## 执行模式

### 1. 交互式 TUI(默认)

在真实终端中运行 `nonoclaw`(无 `-p` 参数)自动进入 TUI:

```
nonoclaw
```

**快捷键**(TUI 内):
| 键 | 动作 |
|---|---|
| 字符/Backspace/Delete | 编辑输入 |
| ← → / Ctrl-← → | 字符/词级移动 |
| Ctrl-W / Ctrl-U | 删词 / 删至行首 |
| Home(Ctrl-A) / End(Ctrl-E) | 行首/行尾 |
| Alt-Enter / Ctrl-J | 插入换行 |
| ↑ ↓ | 历史 |
| Enter | 发送消息 |
| `/clear`, `/compact`, `/cost`, `/tools`, `/help`, `/quit` | 斜杠命令 |
| `/<skill>` | 注入 skill 指令 |
| `?` | 键位帮助弹窗 |
| PageUp/Down | 滚动消息流 |
| Ctrl+C / Esc | 退出 |

权限弹窗出现时按 `y` 允许 / `n` 拒绝。

### 2. 无头模式(`-p`)

适合脚本和 CI:

```bash
nonoclaw -p --max-turns 5 "读取 rust/Cargo.toml 并总结"
echo "你的问题" | nonoclaw -p --output-format json
```

### 3. 远程会话

**服务器端**(需要 API key):

```bash
nonoclaw --serve 127.0.0.1:8765
```

**客户端**:

```bash
nonoclaw --remote 127.0.0.1:8765 "你的问题"
```

**Bridge(本地 TUI + 远程引擎)**:

```bash
nonoclaw --bridge 127.0.0.1:8765
```

### 4. MCP 模式

**作为 MCP client**(连接外部 server):

```bash
nonoclaw --mcp-config mcp.json "使用外部工具完成任务"
```

`mcp.json` 格式:

```json
{
  "mcpServers": {
    "echo": {
      "command": "python3",
      "args": ["/path/to/mcp_server.py"]
    }
  }
}
```

MCP server 的工具会以 `mcp__<server>__<tool>` 命名注册,权限默认 ask(无头模式下需 bypass 或 allow 名单)。

**作为 MCP server**(被外部 client 驱动):

```bash
nonoclaw --mcp-serve
```

服务端在 stdio 上监听 JSON-RPC 请求(`initialize`, `tools/list`, `tools/call`),暴露内置工具(除 Agent,因 serve 模式无引擎)。

### 5. 所有 CLI 标志

| 标志 | 说明 |
|---|---|
| `-p, --print` | 强制无头模式 |
| `--model <ID>` | 覆盖模型(默认 `claude-sonnet-4-5-20250929`) |
| `--permission-mode <MODE>` | `default` / `acceptEdits` / `auto` / `bypassPermissions` / `plan` |
| `--allowed-tools <LIST>` | 逗号分隔工具白名单 |
| `--disallowed-tools <LIST>` | 逗号分隔工具黑名单 |
| `--dangerously-skip-permissions` | 跳过全部权限(=`bypass`) |
| `--max-turns <N>` | 最大回合数(默认 10) |
| `--max-tokens <N>` | 每回合最大输出 token(默认 8192) |
| `--append-system-prompt <TXT>` | 追加到 system prompt 末尾 |
| `--add-dir <PATH>` | 额外 CLAUDE.md 搜索目录(可重复) |
| `--output-format text\|json` | 无头输出格式 |
| `--mcp-config <PATH>` | MCP server 配置 |
| `--resume <ID>` | 从指定会话继续 |
| `--continue` | 从最近会话继续 |
| `--list-sessions` | 列出会话并退出 |
| `--no-session` | 禁用会话持久化 |
| `--compact-threshold <N>` | 压缩阈值(默认 150k tokens) |
| `--no-auto-compact` | 禁用自动压缩 |
| `--serve <ADDR>` | 远程服务器模式 |
| `--remote <ADDR>` | 远程客户端模式 |
| `--bridge <ADDR>` | TUI 本地 + 远程引擎 |
| `--mcp-serve` | MCP 服务端模式 |
| `--plugin-add <SOURCE>` | 安装插件 |
| `--verbose` | 详细日志 |

---

## 会话管理

每次运行自动生成 UUID 并持久化到 `~/.nonoclaw/`:

```
~/.nonoclaw/
└── projects/
    └── <sanitized-cwd>/
        └── sessions/
            └── <uuid>.jsonl
```

JSONL 文件每行一条 `SessionEntry`(header / message / summary 三种 `kind`)。

```bash
# 列出当前目录的会话
nonoclaw --list-sessions

# 恢复指定会话
nonoclaw --resume <session-id> "继续工作"

# 恢复最近会话
nonoclaw --continue "继续工作"
```

未指定 `--no-session` 时,所有运行均自动持久化。

---

## 权限系统

| 模式 | 行为 |
|---|---|
| `default` | 只读工具自动允许;写入/破坏性工具弹窗询问 |
| `acceptEdits` | 编辑类自动允许,其余弹窗 |
| `auto` | 工具自报允许的自动放行 |
| `bypassPermissions` | 跳过所有检查(=`--dangerously-skip-permissions`) |
| `plan` | 只允许只读操作 |

无头模式下未解决的 `Ask` 被自动拒绝。可通过 `--dangerously-skip-permissions` 或 `--allowed-tools Read,Bash,...` 放行。

---

## Skills 与 Plugins

### Skills

Skill 是一个包含 `SKILL.md` 的目录:

```
.claude/skills/
└── my-skill/
    └── SKILL.md
```

`SKILL.md` 格式:

```markdown
---
name: my-skill
description: 描述该 skill 的用途
---
这里是 skill 的指令文本。当你在 TUI 中输入 /my-skill 时,这段文本会被当作 prompt 发送给模型。
```

Skill 搜索路径:
1. `<cwd>/.claude/skills/*/SKILL.md`
2. `~/.claude/skills/*/SKILL.md`
3. `<cwd>/.claude/plugins/*/skills/*/SKILL.md`

### Plugins

**安装插件**:

```bash
# 从本地目录
nonoclaw --plugin-add /path/to/my-plugin

# 从 Git URL
nonoclaw --plugin-add https://github.com/user/nonoclaw-plugin.git
```

插件安装在 `~/.claude/plugins/` 下。其 `skills/` 子目录中的 skill 自动被加载。

**插件 Hooks** — 在项目根创建 `.claude/hooks.json`:

```json
{
  "hooks": {
    "pre_tool_use": [
      {
        "matcher": "Bash*",
        "command": "echo",
        "args": ["Bash tool called"]
      }
    ],
    "post_tool_use": [
      {
        "matcher": "*",
        "command": "notify-send",
        "args": ["Tool execution finished"]
      }
    ]
  }
}
```

- `PreToolUse` hook 返回非零退出码会**拒绝**工具调用
- `PostToolUse` hook 以 fire-and-forget 方式运行
- 环境变量 `NONOCLAW_TOOL_NAME` / `NONOCLAW_TOOL_INPUT` 在 hook 进程中可用

---

## 内置工具

| 工具 | 类型 | 说明 |
|---|---|---|
| **Read** | 只读 | 读文件,cat -n 格式,支持二进制检测 |
| **Write** | 破坏性 | 写文件(会创建父目录) |
| **Edit** | 破坏性 | 精确字符串替换;不支持 `replace_all` 时要求唯一匹配 |
| **Bash** | 破坏性 | 执行 shell 命令,带超时/输出截断 |
| **Grep** | 只读 | ripgrep 搜索(需要 `rg` 二进制) |
| **Glob** | 只读 | 文件 glob 匹配,按 mtime 排序 |
| **TodoWrite** | 有状态 | 维护任务列表(写内存) |
| **WebFetch** | 只读 | HTTP GET URL,HTML→文本转换,截断 |
| **WebSearch** | 只读 | Web 搜索(需 `SERPER_API_KEY` 或 `BRAVE_API_KEY`) |
| **Agent** | 子智能体 | 派生子查询(独占消息历史,限制工具集) |
| **Coordinator** | 子智能体 | 并行派发多个子智能体并聚合结果 |
| **AskUserQuestion** | 交互 | 在 TUI 中弹出多选问题 |
| **MCP 工具** | 动态 | 通过 `--mcp-config` 从外部 MCP server 加载 |

---

## 上下文压缩

长对话可能超出 token 限制。自动压缩会定期将旧消息摘要为一条小结消息:

```bash
# 自定义阈值(估计 token 数,默认 150k)
nonoclaw --compact-threshold 50000

# 禁用
nonoclaw --no-auto-compact
```

触发时机:每回合开始前,若估计 token 超过阈值,则累计足够 2 个用户 prompt 后压缩。压缩**仅在内存中生效**(session 文件保留完整历史,resume 时全量恢复)。

---

## CLI 完整用法示例

```bash
# 基本无头
nonoclaw -p "在 Cargo.toml 中查找 serde 的版本号"

# 交互式(终端)
nonoclaw

# 远程
nonoclaw --serve 127.0.0.1:9000 &
nonoclaw --remote 127.0.0.1:9000 "总结 README"

# Bridge
nonoclaw --bridge 127.0.0.1:9000

# MCP
nonoclaw --mcp-config servers.json "call the weather tool"
# servers.json: {"mcpServers":{"w":{"command":"weather-server"}}}

# 会话
nonoclaw --continue "继续刚才的工作"
nonoclaw --list-sessions

# 工具白名单(无头必备,否则非只读工具会被拒)
nonoclaw -p --allowed-tools Read,Bash,Edit "更新 version 字段到 0.2.0"

# 跳过所有权限
nonoclaw -p --dangerously-skip-permissions "rm -rf target/ 然后 cargo build"
```

---

## 环境变量参考

| 变量 | 说明 |
|---|---|
| `ANTHROPIC_API_KEY` | API Key(**必选**) |
| `ANTHROPIC_BASE_URL` | 自定义 API 基地址(默认 `https://api.anthropic.com`) |
| `ANTHROPIC_AUTH_TOKEN` | Bearer token 鉴权(与 api key 二选一) |
| `HTTP_PROXY` / `ALL_PROXY` | HTTP 代理 |
| `NONOCLAW_HOME` | 覆盖会话/插件存储根(默认 `~/.nonoclaw`) |
| `SERPER_API_KEY` | WebSearch 的 Serper 后端 |
| `BRAVE_API_KEY` | WebSearch 的 Brave 搜索后端 |
| `RUST_LOG` | tracing 日志级别(推荐 `warn` 或 `debug`) |

---

## 架构

```
rust/
├── Cargo.toml          workspace
└── crates/
    ├── core/           nonoclaw-core   — 消息/内容块/用量/权限/错误类型
    ├── api/            nonoclaw-api    — Anthropic 流式客户端(SSE)+ 重试
    ├── tools/          nonoclaw-tools  — Tool trait + 注册表 + 权限引擎 + 内置工具 + MCP 客户端/服务端
    ├── engine/         nonoclaw-engine — 查询循环 + 系统提示 + 上下文 + 压缩 + 会话
    └── cli/            nonoclaw (bin)  — CLI + TUI + 远程 + 命令 + skills
```

---

## 许可证

MIT
