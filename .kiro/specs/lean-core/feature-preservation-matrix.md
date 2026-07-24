# Feature Preservation Matrix 与工程基线

> Task 1 产物。适用 Requirements 1.1–1.8、11.8–11.9。此文档是后续重构的功能保留契约；状态为“待确认”的代码在证明无调用者和兼容职责前不得删除。

## 1. 基线范围与规则

- 基线提交：`ace0002ea99a9fd261695da9f95e48ff933b9103`。
- 工具链：`rustc 1.97.0`、`cargo 1.97.0`、Node `v26.5.0`、npm `12.0.1`。
- Spec 目录没有 `.config`；根据 `requirements.md`、`design.md` 和任务内容按 feature spec 执行，不按 bugfix exploration 处理。
- 当前外部入口、名称、字段和行为均须保留；内部实现可迁移到“重构后权威所有者”。
- `当前缺陷` 不是删除理由；先迁移调用者、建立 characterization test，再处理。
- 表中“无”表示没有发现专用 alias，不表示可以改名；`ToolRegistry` 仍提供大小写不敏感 fallback。

## 2. CLI 参数与运行模式

### 2.1 CLI 参数

当前入口和解析权威位置均为 `rust/crates/cli/src/main.rs::Cli`，依赖 `clap`。目标权威所有者为 CLI `Bootstrap` + `ResolvedConfig`；运行生命周期交给 `RunController`。

| 参数/入口 | 当前行为与依赖 | 当前缺陷 | 重构后权威所有者 |
|---|---|---|---|
| positional `prompt...` | 拼接为 prompt；为空时尝试 stdin | Web/remote/headless 对输入归一化各自实现 | Bootstrap input adapter |
| `-p`, `--print` | 声明强制 headless | 当前没有 TUI，代码不读取 `cli.print`，属于兼容入口，禁止删除 | CLI headless adapter |
| `--model ID` | 覆盖主模型；remote 透传 | 模型解析在 main/Web 分叉 | `ResolvedConfig` + `ClientFactory` |
| `--permission-mode MODE` | `default/acceptEdits/auto/bypassPermissions/plan` | CLI 与 WS 字符串解析重复 | `ResolvedConfig`/Permission model |
| `--allowed-tools LIST` | 逗号分隔 allowlist | 与 profile/settings 合并规则分散 | `ResolvedConfig` |
| `--disallowed-tools LIST` | 逗号分隔 denylist | 同上 | `ResolvedConfig` |
| `--max-turns N` | 覆盖最大轮数 | remote/Web/default 构造重复 | `ResolvedConfig` RunLimits |
| `--max-tokens N` | 覆盖单轮输出 token | profile/global 解析分散 | `ResolvedConfig` RunLimits |
| `--append-system-prompt TXT` | 追加 system prompt | skill/profile 追加链缺统一来源诊断 | PromptBuilder from `ResolvedConfig` |
| `--add-dir PATH` | 可重复，增加 `NONOCLAW.md` 发现目录 | 路径来源未进入 Insight | Extension/Context discovery |
| `--dangerously-skip-permissions` | 强制 bypassPermissions | 与 mode 冲突时仅隐式胜出 | `ResolvedConfig` |
| `--output-format text|json` | headless 文本或 JSONL 事件/result | JSON 事件字段由 `handle_event` 手写 | CLI adapter over `RunEvent` |
| `--mcp-config PATH` | 加载额外 MCP JSON 并连接 | 注释仍写“Phase 0 parsed only”，与实现矛盾 | ExtensionRuntime MCP source |
| `--resume ID` | 加载指定 JSONL session | main 与 Web 各有 session helper | `SessionService` |
| `--continue` | 恢复 cwd 最新 session | 同上 | `SessionService` |
| `--list-sessions` | 打印 session 列表并退出 | 直接 `process::exit`，不可组合 | Session CLI adapter |
| `--no-session` | 禁用本次持久化 | 仅 headless 生效 | `ResolvedConfig`/SessionService |
| `--no-auto-compact` | 禁用自动压缩 | 仅 headless flag，Web 无等价消息 | `ResolvedConfig` |
| `--compact-threshold N` | 显式 token 阈值 | 默认/模型派生在多处 | `ResolvedConfig` RunLimits |
| `--context-window N` | 用于派生 compact 阈值 | Web/profile/global 计算重复 | `ResolvedConfig` ModelProfile |
| `--settings PATH` | 最高优先级 settings 文件 | 无字段来源记录；解析失败静默 | `ResolvedConfig` |
| `--serve ADDR` | TCP JSON-lines remote server | 独立 bootstrap，未复用 engine 构造 | RunController + remote adapter |
| `--serve-http ADDR` | HTTP + WS + SPA | `serve_http.rs` 超大职责 | Web service modules |
| `--public-url URL` | QR/移动端公共地址 | 自身不强制鉴权 | Web auth/static service |
| `--tunnel` | 启动 `cloudflared`，覆盖 public URL | 外部进程生命周期/失败诊断弱 | Web public access service |
| `--remote ADDR` | TCP client 发起 prompt | 仅透传 prompt/model/max_turns | RunController remote adapter |
| `--mcp-serve` | stdio JSON-RPC MCP server | 与普通注册表能力不完全一致 | MCP server adapter |
| `--plugin-add SOURCE` | copy 本地目录或 `git clone` 到用户 plugins | git URL 判定/冲突/来源诊断简单 | ExtensionRuntime Plugin installer |
| `--verbose` | 设置 tracing filter | 与 `RUST_LOG` 组合规则仅在 main | Bootstrap logging |

### 2.2 运行模式

| 模式 | 当前入口/实现 | 关键依赖 | 当前缺陷 | 重构后权威所有者 |
|---|---|---|---|---|
| Headless（参数/stdin/`-p`） | `main.rs` → `QueryEngine::run` | API client、registry、session | `-p` 实际未参与分支；构造逻辑独立 | Bootstrap + RunController + CLI adapter |
| HTTP/Web UI | `--serve-http` → `serve_http::serve` | axum、WS、frontend/dist | 单文件约 2.2k 行；多套状态/锁/构造 | Web service decomposition |
| Remote server | `--serve` → `remote::serve` | Tokio TCP/JSONL | 与本地运行 bootstrap 分离 | RunController + remote server adapter |
| Remote client | `--remote` → `remote::connect` | Tokio TCP/JSONL | 参数能力子集 | remote client adapter |
| MCP server | `--mcp-serve` → `mcp_server::serve_stdin` | stdio JSON-RPC | 排除 Agent；没有 ToolSearch；BypassPermissions | MCP server adapter + ToolExecutor |
| Plugin install | `--plugin-add` → `add_plugin` | filesystem/git | 安装与发现分属不同模块 | ExtensionRuntime Plugin |
| Public tunnel | `--tunnel` → `spawn_tunnel` | `cloudflared` | 公网 token policy 不严格 | Web auth/public service |
| Session resume/list/continue | `resolve_session`/`list_and_exit` | engine session JSONL | 与 Web session 管理重复 | SessionService |
| Multi-model batch | 前端 `/multi` | `window.__nonoclaw_pending_multi`、WS Run | 全局 window hack、串行链无 run identity | RunController + frontend run slice |

## 3. 工具与 MCP 能力

### 3.1 内建工具

正常 CLI/Web 注册路径为 `register_all()` 的 18 项，加上 MCP 注册后由 `main.rs` 动态加入 `ToolSearch`，合计 19 项。全部工具当前 alias 均为空；schema 的字段名必须保持。

| 工具 | 当前入口/实现；schema 摘要 | 依赖 | 当前缺陷 | 重构后权威所有者 |
|---|---|---|---|---|
| `Read` | `builtin/read.rs`; `file_path, offset?, limit?` | Tokio fs | prompt 声称图片/notebook 可视读取，实际二进制含 NUL 即跳过 | ToolRegistry + ToolExecutor；实现留 tools |
| `Write` | `builtin/write.rs`; `file_path, content` | Tokio fs | prompt 的“必须先 Read”未在代码强制；覆盖安全分散 | ToolExecutor security + WriteTool |
| `Edit` | `builtin/edit.rs`; `file_path, old_string?, new_string, hash_line?, replace_all?` | fs、hash-line | “必须先 Read”未强制 | ToolExecutor security + EditTool |
| `Bash` | `builtin/bash.rs`; `command, timeout_ms?, run_in_background?` | shell、BackgroundTaskRegistry | prompt/模块注释称后台不支持，但实现支持；返回提示要求不存在的 `TaskOutput` 工具；非持久 shell | ToolExecutor + BackgroundTaskManager |
| `Grep` | `builtin/grep.rs`; `pattern, path?, glob?, type?, output_mode?, -i?, -n?, multiline?` | 外部 `rg` | 调用参数未使用 cancellation token | ToolExecutor + GrepTool |
| `Glob` | `builtin/glob.rs`; `pattern, path?` | `glob`, sync metadata | async call 内同步遍历 metadata | ToolExecutor + GlobTool |
| `TodoWrite` | `builtin/todo.rs`; `todos[{content,status,activeForm?}]` | 进程内 Mutex store | 与 Task* 是第二套任务真相 | shared TaskStore adapter |
| `WebFetch` | `builtin/webfetch.rs`; `url, prompt?` | reqwest | 无统一网络边界/SSRF policy；HTML 转换简化 | ToolExecutor network gate + WebFetchTool |
| `WebSearch` | `builtin/websearch.rs`; `query` | Serper/Brave env key、reqwest | 无 key 时返回说明；provider 诊断不结构化 | ToolExecutor + WebSearch backend |
| `Memory` | `builtin/memory.rs`; `action` + action-specific fields | `tools/memory.rs`, `.nonoclaw` files | schema 描述/字段未完整覆盖 goal/wiki/limit 等实现参数；读写均标 concurrency-safe | ToolExecutor + Memory service |
| `LSP` | `builtin/lsp.rs`; `operation,filePath,line,character,query?` | `rg`, sync file read | 名称是 LSP 但实现为 regex；workspaceSymbol 仍被 schema 要求 position；hover 能力需验证 | ToolExecutor + code intelligence adapter |
| `Agent` | `builtin/agent.rs` + `engine::EngineSubagent` | recursive QueryEngine | 子 Agent 构造与父运行生命周期分离 | RunController/Agent runtime |
| `AskUserQuestion` | `builtin/ask.rs` | QuestionResolver/WS oneshot | headless 只能降级为提示 | ToolExecutor interaction adapter |
| `Coordinator` | `builtin/coordinator.rs` | `run_subagents`, `join_all` | 取消/部分失败只做文本聚合 | RunController/Coordinator runtime |
| `TaskCreate` | `builtin/task_tools.rs`; subject/description/... | `~/.nonoclaw/tasks/*.json` | 与 TodoWrite 分离；全局目录无 session/agent namespace | shared TaskStore adapter |
| `TaskGet` | 同上；`taskId` | TaskStore | 同上 | shared TaskStore adapter |
| `TaskList` | 同上；空对象 | TaskStore | 同上 | shared TaskStore adapter |
| `TaskUpdate` | 同上；id/status/fields/dependencies | TaskStore | 同上；存在未使用 helper，先隔离 | shared TaskStore adapter |
| `ToolSearch` | `builtin/tool_search.rs`; `query` | registry snapshot | 不在 `register_all`；只在普通 main 的 MCP 注册后加入，MCP serve 不暴露 | ToolRegistry metadata |

共同当前入口为 `rust/crates/tools/src/builtin/mod.rs::register_all` 与 `rust/crates/tools/src/registry.rs`；共同 pipeline 目前仍在 `engine/src/loop_.rs`。结果上限默认 30k chars，但 `Read` 无上限、`Agent` 60k，engine preview 又允许 500k，策略不统一。

### 3.2 MCP client/server

| 能力 | 当前入口/实现 | 依赖 | 当前缺陷 | 重构后权威所有者 |
|---|---|---|---|---|
| stdio server spawn/config | `tools/src/mcp.rs::McpClient::spawn`; `type,command,args,env` | child stdio | 仅 stdio；server notification 被忽略 | ExtensionRuntime MCP transport |
| `initialize` + `notifications/initialized` | MCP client/server | JSON-RPC 2.0, protocol `2024-11-05` | capability/version 固定 | MCP protocol adapter |
| `tools/list` | client 发现并注册 `mcp__<server>__<tool>`；server 暴露 builtins | ToolRegistry | 动态工具全部保守标记非只读/非并发 | MCP adapter + Tool metadata |
| `tools/call` | client 60s timeout；server BypassPermissions | ToolExecutor/child | server 直接 `Tool::call`，绕过目标统一 pipeline | ToolExecutor |
| `prompts/list` | `McpClient::list_prompts` | remote MCP | 有实现但无生产调用/展示，标记待确认，禁止删除 | ExtensionRuntime MCP prompts |
| `prompts/get` | `McpClient::get_prompt` | remote MCP | 同上 | ExtensionRuntime MCP prompts |
| 故障隔离 | `register` 逐 server warn + skip | tracing | 无重连状态，pending 在断连时仅 clear | ExtensionRuntime diagnostics |
| MCP server tool set | `mcp_server.rs`, `register_all().filtered(["Agent"])` | stdio | ToolSearch 未注册；Agent 明确排除；权限委托 client | MCP server adapter |

## 4. HTTP 与 WebSocket 协议

### 4.1 HTTP routes

当前入口均在 `rust/crates/cli/src/serve_http.rs`。

| Method/path | 行为/依赖 | 当前缺陷 | 重构后权威所有者 |
|---|---|---|---|
| `GET /ws?token=&session=` | WS upgrade、移动鉴权、session 绑定 | 只有“提供 token 时”才校验，无 token 可放行公网 | `protocol` + `connection` + auth policy |
| `POST /api/upload` | multipart 上传、文本/OCR/图片提取 | handler 与模型 client/磁盘耦合；错误结构不统一 | `upload_service` |
| `POST /api/stt` | ElevenLabs speech-to-text | provider/错误映射写在 handler | `speech_service` |
| `GET /manifest.json` | 内联 PWA manifest/data icon | 静态内容写在 Rust | `static_service` |
| `GET /sw.js` | 内联 service worker，assets cache `nc-v3` | 静态内容写在 Rust；cache 策略无测试 | `static_service` |
| `GET /` | 读取 `frontend/dist/index.html` | 每请求磁盘读；前端目录探测耦合 cwd | `static_service` |
| `GET /assets/*` | `ServeDir(frontend/dist/assets)` | 与 server 启动路径耦合 | `static_service` |

### 4.2 ClientMsg（Browser → Server）

| 消息 | 字段/当前行为 | 当前缺陷 | 重构后权威所有者 |
|---|---|---|---|
| `run` | `prompt,model?,max_turns?,append_system_prompt?,arguments?,attachments?` | TS `RunRequest` 缺 `arguments`；无 run id/revision | shared protocol + run_handler |
| `cancel` | cancel token + abort run/consumer | 多个松散 handle；终态可能竞态 | RunController |
| `clear` | cancel 后清内存和 JSONL并广播 | 50ms sleep + 全局前端 ignore flag | SessionService + RunController |
| `new_session` | 新建 connection session | session registry/handle 分散 | SessionService/session_hub |
| `resume_session` | `id`，载入 JSONL | 直接文件读取 | SessionService |
| `compact` | 手动压缩 | 手写 `compacting` JSON，不属于 Rust EngineEvent | RunEvent + SessionService |
| `permission_decision` | `request_id,decision` | decision 是自由字符串 | protocol PermissionDecision |
| `question_answer` | `request_id,answer?` | 无 timeout/sequence | interaction service |
| `file_tree` | 请求 cwd tree | 每次同步遍历 | project_service |
| `open_file` | `path,force_code?` | OS side effect/error未结构化 | project_service security |
| `project_info_refresh` | 重算 tools/MCP/skills/git | 与握手/运行后重复 gather | ProjectService |
| `git_show` | `sha` | shell git/error未结构化 | project_service Git |
| `set_permission_mode` | `mode` | Rust 接受 `bypass`，TS 类型只含 `bypassPermissions` | shared protocol + ResolvedRun |
| `set_model` | `name` | 修改进程环境变量，影响并发 session | ClientFactory |

### 4.3 ServerMsg（Server → Browser）

| 消息 | 字段/当前行为 | 当前缺陷 | 重构后权威所有者 |
|---|---|---|---|
| `event` | `{event: EngineEvent-like JSON}` | 部分事件手写且不在 EngineEvent enum | EventEnvelope<RunEvent> |
| `permission_required` | request/tool/message/input | 无 run/session sequence | EventEnvelope |
| `question_required` | request/prompt/options | 同上 | EventEnvelope |
| `done` | text/usage/turns/stop_reason | Cancel 也伪装 Done；终态无唯一提交 | RunController terminal event |
| `error` | message | 无 code/retryable/operation | AppError wire mapping |
| `info` | model/session/auth token/models | auth token进入浏览器；消息混合多职责 | connection/session envelopes |
| `session_list` | sessions | 无 revision | SessionService snapshot |
| `messages_loaded` | 全量 transcript | 重连依赖 skip flag；多 peer 全量广播 | versioned Session snapshot |
| `file_tree` | root/entries | 无版本/错误响应 | ProjectService message |
| `project_info` | tools/MCP/skills/plugins/hooks/docs/settings/git | 重复 gather；可能暴露路径 | ProjectService/redaction |
| `git_show` | sha/output | 大输出/错误无结构 | ProjectService message |

### 4.4 EngineEvent

当前权威声明为 `rust/crates/engine/src/loop_.rs::EngineEvent`，CLI、remote、WS 各自序列化。

| variant | 当前消费者 | 当前缺陷 | 重构后权威所有者 |
|---|---|---|---|
| `TextDelta{text}` | CLI/WS/frontend | 无 sequence/run id | `nonoclaw-core::RunEvent` |
| `ToolUseStart{id,name,input}` | CLI/WS/tool cards | 缺 queued/validation/permission/hook 状态 | RunEvent |
| `ToolResult{id,ok,preview}` | CLI/WS/tool cards | preview 上限策略不一 | RunEvent |
| `AssistantDone{text}` | CLI/WS/breath settle | 与最终 Done 双终态语义 | RunEvent |
| `Compacted{removed,kept,tokens_before,tokens_after}` | CLI/WS | manual compact 填 0 metrics | RunEvent |
| `ModelInfo{model}` | JSON/WS/status bar | 仅实际模型，无 requested/provider/turn | RunEvent |

前端 `EngineEvent.kind` 额外声明 `compacting`，而 Rust enum 没有；这是现有兼容行为，迁移前不得删除。

## 5. 配置字段与来源

当前实现为 `rust/crates/engine/src/settings.rs`。来源优先级（低→高）：用户 `~/.nonoclaw/settings.json`/`$NONOCLAW_HOME/settings.json` → 项目 `.nonoclaw/settings.json` → 项目 `.nonoclaw/settings.local.json` → `--settings` → 项目 `.nonoclaw/mcp.json` 的 MCP per-key overlay；CLI flags 在调用侧再覆盖。目标统一所有者：`ResolvedConfig`，每字段保留来源。

| 字段 | 类型/语义/依赖 | 当前缺陷 | 目标所有者 |
|---|---|---|---|
| `model` | string，主模型 | 与 default profile 决策分散 | ResolvedConfig |
| `maxTurns` | u32 | CLI/Web 默认重复 | ResolvedConfig RunLimits |
| `maxTokens` | u32 | profile/global precedence 分散 | ResolvedConfig |
| `autoCompact` | bool | Web/CLI 应用不一致 | ResolvedConfig |
| `compactThreshold` | usize | 多处派生 | ResolvedConfig |
| `contextWindow` | usize | 多处派生 | ResolvedConfig |
| `thinking` | arbitrary JSON → ThinkingConfig | 无字段级诊断 | ResolvedConfig |
| `permissions.allow` | string[]，concat+dedup | CLI/profile merge 分散 | ResolvedConfig |
| `permissions.deny` | string[]，concat+dedup | 同上 | ResolvedConfig |
| `permissions.defaultMode` | string | 未知值静默忽略 | ResolvedConfig |
| `hooks` | arbitrary JSON deep merge | 与独立 hooks.json loader 并存，settings hooks 是否生效待确认 | ExtensionRuntime Hooks |
| `env` | map，process env 已有值优先 | 产生进程级副作用/潜在密钥泄漏 | ResolvedConfig input only |
| `mcpServers` | map name→`type,command,args,env` | 来源标签不完整；只 stdio | ExtensionRuntime MCP |
| `models[]` | 后层整数组替换 | 无冲突/引用诊断 | ResolvedConfig ModelProfile |
| `models[].name` | string | 重名无诊断 | ModelProfile registry |
| `label` | string? | UI label | ModelProfile registry |
| `baseUrl` | string | Web set_var | ClientFactory |
| `apiKey` | string/$ENV | 解析值可能进入进程 env | ClientFactory secret |
| `default` | bool | 多 default 无诊断 | ModelProfile registry |
| `role` | string/string[] (`main/doc/compact`) | 未知 role 无诊断 | ModelProfile registry |
| `contextWindow` | usize? | 运行时重复解析 | ModelProfile registry |
| `maxTokens` | u32? | 同上 | ModelProfile registry |
| `charsPerToken` | usize? | 0/无效值无校验 | ModelProfile registry |
| `profile` | agent profile name | 错误引用静默缺失 | ExtensionRuntime Profiles |
| `apiFormat` | `anthropic/openai` | 未知值回退 anthropic | Provider/ModelProfile |
| `compactModel` | model name | 未找到只 warn 且仍保留字符串 | ClientFactory |
| `elevenlabsApiKey` | secret string | 应禁止进入 ProjectInfo/WS | MediaConfig secret |
| `charsPerToken` | usize，默认 4 | 默认值导致 overlay 中无法显式表达 4 的来源 | ResolvedConfig |
| `docModel` | model name或 inline object | 未找到会禁用；来源丢失 | ClientFactory document purpose |
| `docModel.provider` | mistral_ocr/gemini/generic_vision/none | capability 靠字符串 | MediaConfig |
| `docModel.model` | model id | 无统一引用诊断 | ClientFactory |
| `docModel.baseUrl` | URL | 无验证 | ClientFactory |
| `docModel.apiKey` | secret/$ENV | 应脱敏 | ClientFactory secret |
| unknown fields | flatten 到 `extra` | 被保留但完全不诊断/不应用 | ResolvedConfig diagnostics |

## 6. 扩展路径与优先级

| 扩展/路径 | 当前入口与依赖 | 当前优先级/缺陷 | 重构后权威所有者 |
|---|---|---|---|
| User settings | `$NONOCLAW_HOME/settings.json` 或 `~/.nonoclaw/settings.json` | 最低层 | ResolvedConfig |
| Project settings | `<cwd>/.nonoclaw/settings.json` | 覆盖 user | ResolvedConfig |
| Local settings | `<cwd>/.nonoclaw/settings.local.json` | 覆盖 project | ResolvedConfig |
| Explicit settings | `--settings PATH` | 覆盖 local；失败静默 | ResolvedConfig |
| Standalone MCP | `<cwd>/.nonoclaw/mcp.json` | 最后 per-key 覆盖 settings | ExtensionRuntime MCP |
| User hooks | `~/.nonoclaw/hooks.json` | 项目同 type+matcher 覆盖 | ExtensionRuntime Hooks |
| Project hooks | `<cwd>/.nonoclaw/hooks.json` | parser schema仅装载 9/12 enum 类型 | ExtensionRuntime Hooks |
| Project skills | `<cwd>/.nonoclaw/skills/<name>/SKILL.md` | discover 先加入 | ExtensionRuntime Skills |
| User skills | `~/.nonoclaw/skills/<name>/SKILL.md` | 代码顺序在 project 后 | ExtensionRuntime Skills |
| Project plugin skills | `<cwd>/.nonoclaw/plugins/*/skills/*/SKILL.md` | discover 后加入；用户 plugin skills 未在 engine discover 中扫描 | ExtensionRuntime Plugins/Skills |
| User plugins | `~/.nonoclaw/plugins/*` | ProjectInfo 能列出；安装目标在此；engine skill discover 是否消费待确认 | ExtensionRuntime Plugins |
| Bundled skills | `engine/src/skills/bundled/*.md` | 注释称 disk 覆盖 bundled，但 `new()` 在 discover 后 append bundled，实际去重路径需 characterization test | ExtensionRuntime Skills |
| Dynamic nested skills | 从操作文件向上发现 `.nonoclaw/skills`，止于 cwd | 运行中加入 dynamic，优先于 static | ExtensionRuntime Skills |
| Skill usage | `~/.nonoclaw/skill-usage.json` | 7-day half-life ranking | ExtensionRuntime Skills |
| Agent profiles | `<cwd>/.nonoclaw/agents/<name>.md` | 仅 project；model profile 引用 | ExtensionRuntime Profiles |
| Project plugins | `<cwd>/.nonoclaw/plugins/*` | ProjectInfo 与 skills 扫描 | ExtensionRuntime Plugins |
| Context docs | project `.nonoclaw/NONOCLAW.md`, `.local.md`, `rules/*.md`, `memory/MEMORY.md`; user `NONOCLAW.md`, `rules/*.md`; `--add-dir` | 根目录 `NONOCLAW.md` 仅 Insight 展示、不自动载入 | ContextProvider |
| Session/uploads/tasks/memory | `~/.nonoclaw/...` 或 `$NONOCLAW_HOME` | 多个模块自行拼路径 | SessionService/TaskStore/Memory service |

Hook enum 声明 12 种：`PreToolUse, PostToolUse, PostToolUseFailure, Notification, UserPromptSubmit, SessionStart, SessionEnd, Stop, SubagentStart, SubagentStop, PreCompact, PostCompact`。磁盘 parser 只装载 `PreToolUse, PostToolUse, UserPromptSubmit, SessionStart, SessionEnd, Stop, SubagentStop, PreCompact, PostCompact`；主循环实际明确调用 SessionStart、UserPromptSubmit、PreCompact、PostCompact、SessionEnd，工具/子 agent 的其他调用点分散。`HookDef.prompt/http` 已声明但 runner 只执行 `command`。以上均标记待补齐，不删除。

## 7. 前端功能矩阵

| 功能 | 当前入口/实现与依赖 | 当前缺陷 | 重构后权威所有者 |
|---|---|---|---|
| Chat/streaming Markdown | `ChatView`, `Markdown`, Zustand messages | react-markdown/GFM/math/highlight | 逐 token 复制 messages 数组并 React set | run/tool slices |
| 输入与取消 | `InputBox`, App submit, WS cancel | attachments/voice | disabled 状态与终态竞态 | runSlice |
| `/clear`, `/compact` | `App.handleSubmit` | WS messages | 依赖模块级 `ignoreUntilLoad` | sessionSlice + revisions |
| `/skill-name` | App 从 ProjectInfo 注入 body | Skills | 仅前端注入，activation reason 无事件 | Extension/RunEvent |
| `/multi` | App + `window.__nonoclaw_pending_multi` | model dropdown | 全局 hack、无取消/隔离 | runSlice multi-run controller |
| Tool cards | ChatView/store | EngineEvent | tool id 状态混在 messages | toolSlice |
| Permission dialog | `PermissionDialog` | WS oneshot | 无等待时长/sequence | dialogSlice + RunEvent |
| Question dialog | `QuestionDialog` | AskUserQuestion | 同上 | dialogSlice |
| Session picker/new/resume | `SessionPicker` | server JSONL | 本地 messages 形成第二真相；全量 replay | sessionSlice |
| Reconnect/offline queue | `websocket.ts` | generation/backoff | 模块级 pending/skipOneLoad/ignoreUntilLoad 跨连接共享 | connectionSlice |
| Multi-peer sync | `messages_loaded` broadcasts | session registry | 全量快照、无 revision | sessionSlice/session_hub |
| File tree/open/code | `FileTree` | ClientMsg FileTree/OpenFile | 无版本和结构化错误 | projectSlice |
| Insight | `InsightRail` | ProjectInfo | 静态/运行技术信息混合，gather 重复 | projectSlice + traceSlice |
| Git/commit patch | `GitPane`, `CommitDialog` | git shell | 无结构化错误/缓存 | projectSlice |
| QR/mobile | `QrDialog`, `QrCode`, Info token/public_url | qrcode | 服务端允许无 token；token在 browser store | dialog/security service |
| PWA | manifest/service worker routes | browser cache | 内联 Rust、无离线契约测试 | static service |
| Attachments | InputBox upload/drop/paste → `/api/upload` | PDF/DOCX/image/doc model | media 状态混在组件 | mediaSlice/upload service |
| 文档图片/多模态 | AttachmentRef.images → ContentBlock::Image | base64 | 大 payload 直接 WS | media service |
| 语音/STT | InputBox → `/api/stt` | MediaRecorder/ElevenLabs | provider error string | mediaSlice/speech service |
| 模型切换 | StatusBar → `set_model` | model profiles | server `set_var` 污染并发 | runSlice + ClientFactory |
| 权限模式切换 | StatusBar → `set_permission_mode` | PermissionMode | client/server string 有差异 | runSlice/protocol |
| Usage/model status | StatusBar/store | Done/ModelInfo | conversation 累积不与 session revision 绑定 | runSlice/trace |
| 三主题/rails | store/CSS (`biolume`,`amber`,`frost`) | localStorage | UI preference 可保留 | UI preference slice |
| BreathField | canvas RAF + `breathMeter` | 6 orbs/gradients/frame | websocket 直接 pulse/flare/settle；无 reduced-motion；隐藏页不暂停 | BreathController |
| 首次连接 overlay/刷新 | App/ConnectingOverlay | WS | 首连 overlay state为组件局部；重连语义分散 | connectionSlice |

## 8. 工程与性能基线

### 8.1 验证命令

| 检查 | 结果 | 基线证据/阻塞 |
|---|---|---|
| `cargo test --workspace` | **FAIL（编译阶段）** | `crates/cli/examples/two_agent_math.rs:99` 初始化 `EngineOptions` 缺 `chars_per_token`, `compact_model`, `compact_model_creds` 及另 1 字段；测试体未开始执行 |
| `cargo clippy --workspace --all-targets -- -D warnings` | **BLOCKED/未得到独立完整结果** | 首轮与其他 Cargo 命令争用 package/build lock；串行链因 workspace test 失败未执行到 Clippy。测试编译已报告约 20 个 unused/dead-code warnings，`-D warnings` 基线预计不能通过，须以后续独立运行确认 |
| `npm run build` | **PASS** | TypeScript + Vite production build 成功，587 modules，11.35s；有 >500 kB chunk warning |
| `cargo build --release` | **BLOCKED（串行链未到达）** | workspace test 先失败；现有 release artifact 可记录但不是本次成功重建 |

### 8.2 体积与首屏资产

- 现有 `rust/target/release/nonoclaw`：`10,755,072` bytes（约 10.26 MiB）。由于 release build 被上述编译错误阻塞，此数值标记为“现有 artifact”，后续修复后需重建刷新。
- `frontend/dist/index.html`：57.71 kB（gzip 9.92 kB）。
- 主 CSS `index-Cp579UoJ.css`：29.29 kB（gzip 8.05 kB）。
- 主 JS `index-CCPOGDQ1.js`：843.77 kB（gzip 260.37 kB），Vite 已提示大于 500 kB；当前无 code splitting。
- KaTeX 字体为独立 assets，最大单文件约 63.63 kB。
- 首屏真实 FCP/LCP：本仓库没有 Lighthouse/Playwright 脚本，也没有可复现测试页面数据集；本任务未伪造数值。以上 HTML/CSS/JS gzip 是当前可复现的首屏传输代理基线。后续性能验收必须在固定浏览器/viewport/冷缓存下补充 FCP/LCP。

### 8.3 关键文件复杂度代理

当前没有已配置的圈复杂度工具；使用“最小观察行数、职责/分支集中度、可识别函数数”作为不伪造的静态代理。后续应固定同一 analyzer 再量化 McCabe complexity。

| 文件 | 静态基线 | 复杂度/职责热点 |
|---|---|---|
| `cli/src/serve_http.rs` | 约 2,220+ 行；至少 20 个顶层函数/handler | protocol、auth、session、run、peer sync、file tree、upload、STT、tunnel、static、Git/ProjectInfo 全在一文件；`handle_ws` 是主要巨型分支 |
| `engine/src/loop_.rs` | 至少 1,200 行 | EngineOptions/Event/QueryEngine、provider stream、tool pipeline、compact、session persistence、subagent、repair 共置 |
| `engine/src/skills.rs` | 至少 1,000 行 | usage、discovery、activation、parsing、glob/regex、dynamic state 共置 |
| `cli/src/main.rs` | 约 680 行，12 个可识别函数/enum/struct | CLI、配置、client、MCP、session、plugin、mode dispatch 共置 |
| `frontend/src/store.ts` | 407 行 | messages/connection/session/dialog/project/media/theme/usage 单一 Zustand store |
| `frontend/src/App.tsx` | 约 385 行 | commands、多模型链、dialogs、layout、project actions集中 |
| `frontend/src/websocket.ts` | 约 341 行 | connection、queue、reconnect、protocol reducer、multi-run、breath直接耦合 |
| `frontend/src/components/BreathField.tsx` | 约 222 行 | 单 RAF 每帧 6 个 radial gradients；主题也每帧读取 DOM |

### 8.4 重复实现基线

自动 6-line clone 统计脚本本次因命令脚本语法错误未产出可信数字，因此不记录虚假百分比。已确认的语义重复如下，后续每项迁移后再以 clone analyzer 比较：

1. Client 构造：`main.rs`、Web 模型切换、engine compact、attachments/doc model。
2. 配置/模型解析：main headless、`serve_http::build_options`、per-model Web 分支。
3. Session 解析/读写：`main::resolve_session`、`serve_http::{create_new_session,resume_session,Clear}`、engine persist。
4. 事件序列化：CLI `handle_event`、`serve_http::serialize_event`、TS `EngineEvent`/dispatcher。
5. ProjectInfo gather：WS handshake、refresh、模型切换、run 完成后重复。
6. 任务状态：内存 TodoStore 与文件 TaskStore。
7. 工具 pipeline：lookup/permission/hooks/result handling 内嵌 QueryEngine，MCP server 又直接 call。
8. 模型/compact 阈值：global settings、profile、main、Web 各自计算。
9. Extension 路径：Skills 与 ProjectInfo 对 user/project/plugin 的扫描并不一致。
10. 会话前端状态：server JSONL 与 localStorage `nonoclaw:messages`。

### 8.5 Breath/动画性能基线

- 架构：一个持续 `requestAnimationFrame`；每帧绘制背景、6 个 radial-gradient orb，活动时再加 1 个 bloom；DPR 上限 1.5。
- token 事件调用 `breathMeter.pulse(length)`，工具开始/结束调用 `flare`，assistant_done 调用 `settle`；不经过 React state，这一点应保留。
- 当前缺口：无 FPS/long-task 自动基准；无 `prefers-reduced-motion`；页面隐藏时 RAF 不暂停/降频；theme 属性每帧读取；长流能量稳定性无测试。
- 可复现验收协议（后续 Task 18/21 使用）：固定 Chrome、1440×900、DPR 1、60s idle + 60s 20 deltas/s + 30 次 tool flare，记录 FPS p50/p95、long frames、CPU、heap delta；hidden 60s 与 reduced-motion 单独记录。当前数值标记 `UNMEASURED`，不能被当作“0 回退”。

## 9. 已知缺陷与“先标记、不删除”清单

以下项用途不完整、实现与注释冲突或编译器报告未使用；在调用搜索、characterization tests 和迁移证据齐全前一律保留：

| 标记 | 位置 | 原因/待确认事项 | 预期处理任务 |
|---|---|---|---|
| Q-01 | `main.rs::Cli.print` | 兼容 flag 当前未读取 | Task 2/20 |
| Q-02 | `McpClient::{list_prompts,get_prompt}` | 已实现但无生产消费者 | Task 2/11 |
| Q-03 | Hook `PromptHookConfig`, `HttpHookConfig` | schema 声明但 runner 只运行 command | Task 10 |
| Q-04 | HookType `PostToolUseFailure,Notification,SubagentStart` | enum 有，loader/call site 不完整 | Task 10 |
| Q-05 | `loop_.rs::concurrency_cap` | 读取 `NONOCLAW_MAX_TOOL_CONCURRENCY` 但变量未用于 limiter | Task 7 |
| Q-06 | `loop_.rs::KEEP_RECENT_MESSAGES` | dead code warning，可能是历史 compact compatibility | Task 2/20 |
| Q-07 | `task_tools.rs::nonoclaw_data_dir` | dead helper；另有实际路径 helper | Task 9/20 |
| Q-08 | `attachments.rs::OCR_PROMPT`, `resize_for_ocr` | dead warning，可能为 provider fallback | Task 2/14/20 |
| Q-09 | `serve_http::AppState.model`、局部 `DEFAULT_COMPACT_THRESHOLD` | dead warning；可能兼容旧分支 | Task 14/20 |
| Q-10 | `api/client.rs::preview` local | unused warning，可能调试残留 | Task 5/20 |
| Q-11 | background timestamp locals/imports | unused warnings，可能输出格式未完成 | Task 8/20 |
| Q-12 | `settings.hooks` | 合并但 hooks runtime 从独立 hooks.json 加载，真实用途待测 | Task 2/3/10 |
| Q-13 | user plugin-contributed skills | ProjectInfo 会列 user plugins，但 engine `discover` 只扫描 project plugin skills | Task 2/11 |
| Q-14 | bundled-vs-disk skill priority | 注释与构造顺序疑似矛盾 | Task 2/11 |
| Q-15 | Bash background `TaskOutput` 提示 | 工具不存在，但后台 registry/通知能力存在 | Task 2/8 |
| Q-16 | TS `compacting` EngineEvent | Rust enum 无对应 variant，server 手写发送 | Task 2/13/15 |
| Q-17 | TS/Rust `Run.arguments` | Rust 有字段，TS 类型缺字段 | Task 2/13 |
| Q-18 | localStorage message persistence | 当前断线恢复兼容行为，但违反目标单一事实源；不能直接删除 | Task 2/17 |
| Q-19 | `skipOneLoad`/`ignoreUntilLoad` | 当前竞态规避兼容行为；revision 上线前不能删 | Task 13/17 |
| Q-20 | 现有 release binary | 不是本次可重建产物，仅作体积参考 | Task 21 |

编译 warnings 同样是盘点输入而非本任务清理范围：tools 的 unused imports/vars、engine 的 unused import/constant/concurrency cap、CLI 的 unused import/doc comments/vars/dead fields。Task 1 不修改生产代码。

## 10. 后续变更验收用法

1. 修改任一模块前，从对应表选出所有行并建立 characterization tests。
2. 新实现必须保留入口、名称、schema、wire variant 和路径；若改变，提供兼容 alias/migration。
3. 将实现迁移到表中目标所有者后，搜索全部调用者并更新该行状态。
4. 删除 Q 项前必须记录：调用搜索、测试结果、兼容判断和新权威所有者。
5. Task 21 逐行标记“保留/等价改善”，并刷新 tests、Clippy、build、复杂度、体积、首屏与动画基线；未测性能不得判定通过。

## 11. Task 2 Characterization Test 关联

> 覆盖 Requirements 1.2–1.8、11.7、11.9。所有模型运行测试仅连接进程内/loopback Provider fixture，不请求真实外部 API。测试名称中的矩阵章节注释是重构时的保留证据。

| 功能保留项 | Characterization test / snapshot | 当前锁定行为 |
|---|---|---|
| Headless 最小成功路径 | `nonoclaw_engine::loop_::tests::headless_minimal_success_path_uses_provider_fixture` | 真实 `Client` 请求本地 Anthropic SSE fixture，产生 model/text/assistant done 与最终结果 |
| Session resume | `nonoclaw_engine::loop_::tests::session_resume_minimal_success_path_preserves_history` | 旧 JSONL history 进入下一次 Provider 请求，新 user/assistant turn 追加到原文件 |
| Web + WebSocket | `serve_http::connection::characterization_tests::{websocket_protocol_and_web_success_path_are_stable,websocket_server_message_tags_are_stable}` + `serve_http::protocol::tests::{all_client_tags_are_stable,rust_and_typescript_share_checked_protocol_fixtures}` | 全部 14 个 ClientMsg fixture 由 protocol 权威模块解析；ServerMsg tag 可序列化；Rust/TypeScript event/snapshot/done/error fixture 全字段一致；Run→Event→Done 最小路径稳定 |
| Remote client | `remote::tests::remote_client_minimal_success_path` | loopback TCP JSONL 保持 RunRequest、EngineEvent、Done framing |
| MCP server | `mcp_server::tests::mcp_server_minimal_success_path` | 内存 transport 完成 initialize→tools/list→Read tools/call；server 仍排除 Agent |
| 全部普通运行工具契约 | `builtin::characterization_tests::tool_registration_names_and_schemas_match_snapshot` + `rust/crates/tools/tests/snapshots/builtin_tool_contract.json` | 当前固定 `register_all` 的 20 个 core tools 及运行时追加的 `ToolSearch`，共 21 个名称和完整 input schema；历史 18/19 数量已由 Task 20 纠正 |
| 配置优先级 | `settings::tests::config_layers_follow_documented_precedence` | user < project < local < explicit；permissions 合并去重、models 整体替换、standalone MCP per-key 最后覆盖 |
| JSONL 向后/向前兼容 | `session::tests::corrupt_legacy_lines_are_skipped_and_repairs_are_surfaced` | 坏行/未知 kind 跳过；旧 title/message 兼容读取；孤立工具对修复并产生 repair 记录 |
| 扩展发现顺序 | `skills::tests::extension_discovery_precedence_is_characterized` | project < user < project plugin 同名覆盖；当前 `get_skill` 与 `all_active` 对 disk/bundled 同名项的差异被显式记录（Q-14） |

验证记录（Task 2 执行）：`cargo fmt --all --check` 通过；CLI binary tests 5/5 通过（含 Web/WS/remote）；engine 首轮 50/51 通过，唯一失败是 Q-14 测试原先只断言 bundled 胜出，随后按实际行为改为分别断言 `get_skill` 的 disk 胜出和 `all_active` 的 bundled 胜出。tools 快照已生成，非网络 tools tests 除 MCP teardown 等待外均通过；teardown 已改为显式 abort。受本任务测试尝试上限约束，最后两处精确修正尚未再次执行，需下一验证批次确认。既有 workspace warnings 与 examples 编译基线阻塞仍见 §8.1，本任务未清理生产 warning 或修改 `tasks.md`。

## 12. Task 20 清理、参考生成与文档证据

> 覆盖 Requirements 2.7、11.5、12.1–12.5。本节只记录 Task 20 的安全删除与最终架构说明；未修改 `tasks.md` 或任务状态元数据。

### 12.1 删除/收敛证据

| 项目 | 调用搜索与兼容判断 | 删除/收敛结果 | 保留证据 |
|---|---|---|---|
| `cli/src/commands.rs` | 全仓搜索 `mod commands`、`commands::`、`help_text`、`BUILTINS` 无运行时调用；文件仅含旧 TUI `/help` 文本，CLI 中不存在 TUI/`--bridge` | 删除私有 module 与 declaration | Web `/clear`、`/compact`、`/multi` 和 Skill slash 仍由 `App.tsx`/WS 路径拥有；CLI/Web/remote/protocol tests 通过 |
| `remote::connect_inline` 生产入口 | 全仓唯一消费者是 remote characterization test；没有 `--bridge` flag，也不在功能矩阵公共入口中 | 限定为 `#[cfg(test)]` fixture，生产构建不再携带旧 bridge helper | `--serve`/`--remote` 及 `remote_client_minimal_success_path` 保留 |
| `ConfigSource::path` | 私有方法零调用且编译器报告 dead code | 删除方法；`ConfigSource::label` 和所有来源 variants 保留 | 配置 provenance/diagnostic tests 通过 |
| `memory::scan_dir` 等值 comparator | comparator 永远返回 `Equal`，没有排序语义且触发 unused vars | 删除无效排序，保持原 `read_dir` 收集顺序 | memory roundtrip/search tests 通过 |
| `attachments::resize_for_ocr` | 全仓零调用且编译器报告 dead code；当前 DeepSeek 路径使用 global resize + `tile_image` + `encode_jpeg_base64` | 删除旧 helper | upload/OCR 路由与 attachment security tests 通过 |
| DeepSeek OCR prompt | 同一字符串同时存在未使用常量与请求 literal | 收敛为 `DEEPSEEK_OCR_PROMPT` 单一常量 | 请求 body 语义未变，CLI tests/build 通过 |
| 历史数量/Phase/TUI/`.claude` 说明 | 产品文档与生产注释搜索命中旧 9/17/18/19 tools、Phase、TUI、bridge、`.claude` paths | 清理或改写为当前行为 | flags、tools、routes、protocol tags、`.nonoclaw` paths 未重命名 |

以下 public compatibility helpers 经调用搜索虽无当前 production caller，但属于公开迁移面，Task 20 **未删除**：`load_settings`、`load_mcp_json`、`apply_env`、`apply_settings`、`DocModelConfig::resolved_api_key`、`nonoclaw_config_dir`。`-p/--print` 同样保留并显式读取为 headless compatibility flag。

### 12.2 参考生成与唯一所有者

- 工具参考：`ProjectInfo.tools` 直接遍历运行时 `ToolRegistry`；名称/schema 快照来自同一 registry。当前普通 CLI/Web/remote 为 20 个 `register_all` core tools + `ToolSearch`，不再维护手写数量。
- CLI 参考：Web Insight 的 `cli_reference` 由 `Cli::command()`（Clap `CommandFactory`）生成；新增测试锁定 `-p/--print`、`--serve-http`、`--mcp-serve`，并证明不存在历史 `--bridge`。
- 配置参考：`settings::CONFIG_REFERENCE` 是顶层字段共享元数据；unknown-field diagnostics 与 Web `config_reference` 共用该列表，避免文档与校验漂移。
- 根 `README.md` 是权威产品说明；`rust/README.md` 缩减为 workspace 贡献者入口并链接根文档，不再复制旧 TUI、工具数量和 `.claude` 路径。

### 12.3 最终架构、透明度、安全与兼容说明

- 唯一所有权已文档化：core 管领域事件；api 管 Provider/ClientFactory；tools 管 registry/ToolExecutor/TaskStore/background/MCP；engine 管 RunController/SessionService/ResolvedConfig/extensions/trace；CLI/Web 只做 adapter；frontend 消费 revision/sequence、Technical Trace 与 BreathController。
- 技术透明文档只描述 requested/actual model、context、工具、权限、Hook、重试、压缩、子 Agent、usage、恢复和终态等可验证事实；明确不展示 hidden chain-of-thought。
- 呼吸体验文档覆盖事件驱动状态、连续插值、token energy 节流、hidden-page pause、reduced motion 和文字状态。
- 安全文档覆盖 prompt log 默认关闭/脱敏、公网或 tunnel token、canonical path、上传限制，以及 secrets/附件正文不进入 ProjectInfo、trace、WS/browser store。
- 未删除或重命名任何 public command、flag、route、alias、config key、protocol tag、session behavior、hook type/action、extension path、media feature、UI theme 或 compatibility path。

### 12.4 Task 20 验证记录

- Rust：`cargo fmt --all --check` 通过；`cargo test --workspace` 通过（188 passed，1 ignored 的真实外网 WebFetch test 未运行）；Provider、WebSocket、remote、MCP、tool schema、session/config/extension/security fixtures 均为本地文件或 loopback transport。
- Warnings：移除已证实 dead/no-op 项，并对保持 public `StreamFailure { partial }` shape 的 large-error lint做窄范围、带理由的兼容豁免；各 crate `cargo clippy --all-targets -- -D warnings` 通过。
- Frontend：`npm run test:breath`、`npm run test:transitions`、`npm run test:security` 通过；`npm run build` 通过（592 modules）。Vite 仍报告既有 >500 kB chunk 性能提示，Task 20 未进行无授权 code-splitting 重写。
- 协议：Rust/TypeScript checked fixtures、全部 ClientMsg tags 与 ServerMsg serialization tests 通过。
- 最终 whitespace 检查：见 Task 20 执行末尾 `git diff --check`；未使用真实 Provider/API 网络请求。

## 13. Task 21 最终功能、质量与体验验收

> 覆盖 Requirements 1.1–1.8、11.6–11.9。验收只使用仓库 fixture、临时文件、进程内状态与 loopback TCP/HTTP；未调用真实 Provider API、公共 tunnel 或其他外部网络服务。Task 21 未修改 `tasks.md`、`tasks.meta.json` 或任务状态。

### 13.1 Feature Preservation Matrix 逐域结论

| 矩阵域 | 最终证据 | 结论 |
|---|---|---|
| CLI 与运行模式 | Clap 生成参考测试；`headless_minimal_success_path_uses_provider_fixture`；Web/WS characterization；`remote_client_minimal_success_path`；`mcp_server_minimal_success_path` | headless、Web、remote client/server wire、MCP server 与所有 flags 的契约保留。`cloudflared` public tunnel、真实 plugin git install 未启动，因为会访问外部服务；其 CLI 入口/配置/鉴权契约通过静态与本地测试保留 |
| 内建工具与 MCP | `tool_registration_names_and_schemas_match_snapshot`；ToolExecutor pipeline、barrier、bounded concurrency、large result、MCP failure isolation 测试 | 21 个普通运行 core names/schema、MCP 动态能力、权限与结果契约保留；MCP 局部失败不影响 core registry |
| Agent、Task 与后台任务 | 共享 TaskStore 状态/作用域测试；6 个 subagent、cap=2；部分失败；4 个父取消；后台完成/停止/父取消/drop 回收 | Agent/Coordinator、Todo/Task、后台 Bash、取消树、递归限制与任务隔离通过 |
| Provider 与模型 | Anthropic/OpenAI 本地 SSE fixtures；增量文本/工具参数/usage；图片、prompt cache、capability、retry、mid-stream failure/cancel；ClientFactory cache/purpose tests | 两种 Provider 流式契约与 conversation/compact/document/subagent client 构造通过；没有真实 API 请求 |
| 配置、Hooks 与扩展 | 配置层级/provenance/pure merge/diagnostics；command/prompt/HTTP 三类 Hook 使用本地命令或 loopback；12 lifecycle loader；profile/MCP/Skill failure isolation、precedence、hot reload | 配置来源、三类 Hook action、Skills/Profiles/Plugins/MCP 的来源/冲突/局部失败行为通过 |
| Session、WS 与多 peer/reconnect | 64 个并发 append；clear/replace/append revision；legacy repair；64 个并发 event sequence；前端 generation/snapshot/run sequence/terminal/retention 确定性检查 | 单 writer、compact revision、Clear/Cancel/reconnect、旧事件拒绝、peer snapshot 所需 ordering contract 通过；没有浏览器 socket farm，见 §13.5 |
| HTTP、媒体与工程 UI | 全部 ClientMsg tags、ServerMsg serialization、Web success path；file tree/Git/ProjectInfo CLI metadata；upload/STT 本地 signature/path/size tests；production frontend build | routes/wire tags、FileTree/Insight/Git/Commit/QR/PWA/附件/文档图片/STT/模型和权限切换的编译与协议契约保留 |
| 技术透明、呼吸与安全 | RunEvent envelope/order/redaction/trace export；50,000 text deltas；500 generated breath transitions；hidden/reduced-motion；公网 token、canonical path、upload、prompt/secret/browser redaction tests | Technical Trace、BreathController 状态/连续插值/有界能量、敏感边界通过确定性验收 |

Task 21 发现 `connection.rs` 与权威 `protocol.rs` 重复实现同一 Rust/TypeScript checked-fixture test；已删除 connection 侧副本，保留 `protocol::tests::rust_and_typescript_share_checked_protocol_fixtures` 和 Web end-to-end smoke path。外部名称、tag、schema 与行为未改变。

### 13.2 必需质量门与计数

| 命令 | 最终结果 |
|---|---|
| `cargo fmt --all --check` | PASS；无输出 |
| `cargo test --workspace` | PASS；187 passed、0 failed、1 ignored。ignored 项是明确需要真实外网的 `builtin::webfetch::tests::fetches_a_real_url`，本验收按禁止外网要求未运行 |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS；0 warnings/errors |
| `cargo build --release --workspace` | PASS；optimized workspace release；最终增量重建 2m52s（首次完整验收构建 6m51s） |
| `npm run test:breath` | PASS；确定时钟、50,000-delta 长流、500-event generated sequence、hidden/reduced-motion |
| `npm run test:transitions` | PASS；generation、queue dedupe/bounds、snapshot/revision/sequence、terminal、128-run retention、tool/prompt idempotence、trace ordering |
| `npm run test:security` | PASS；ProjectInfo/tool/attachment/token browser boundary |
| `npm run build` | PASS；TypeScript project build + Vite production build，592 modules，3.14s |
| Rust/TypeScript protocol consistency | PASS；canonical checked fixture 对 event/snapshot/done/error 全字段相等；14 ClientMsg tags 与 ServerMsg serialization 同轮通过 |

### 13.3 本地确定性压力/验收明细

- **多工具并发**：7 个 concurrency-safe calls 在 cap=2 下实测 peak=2，结果保持原始 0..6 顺序；unsafe barrier 位于前后 safe batch 之间；环境 cap parser 对全部输入不返回 0。
- **子 Agent/Coordinator**：6 个并行子 Agent 在 cap=2 下完成；3 个任务中单个失败不取消 siblings；4 个 30s child 在父取消后 1s 内全部返回 Cancelled；Agent/Coordinator 递归工具被过滤。
- **后台任务**：完成输出可查询且通知仅一次；`sleep 30` 可停止并回收；取消/drop 后等待 1.2s 验证 descendant marker 不产生。
- **会话/compact/Clear**：4-thread runtime 上 64 个并发 append 得到 revision 1..64 和唯一 JSONL 顺序；replace-after-compact 使用 expected revision 拒绝 stale replace；Clear 后 revision 继续单调且磁盘仅保留新 session 内容。
- **Cancel/终态**：provider loopback 阻塞流被取消；cancel/error race 只提交一次终态；父 token 传递至 children；64 个并发 event sequence 无重复。
- **Reconnect/multi-peer ordering**：前端拒绝 stale connection generation、重复/倒序 snapshot、重复 sequence、terminal 后事件和 session snapshot barrier 前事件；metadata 保持最多 128 runs，outbound queue 最多 64。
- **长流与 Provider**：Anthropic/OpenAI fixture 分别验证真实 SSE 增量、thinking/cache、tool arguments/usage、pre-stream retry、mid-stream partial 与 cancellation；BreathController 消费 50,000 text deltas，只产生一次 phase subscriber emission，energy 饱和为 1 后衰减。
- **Hooks/扩展**：command updated-input/timeout、prompt local-provider deny、HTTP loopback ask 全部通过；全部 12 HookType loader/lifecycle、profile malformed isolation、MCP failed isolation、Skill precedence/dynamic/hot reload 通过。
- **安全边界**：public/tunnel auth policy、canonical/symlink/path traversal、upload magic/type/decompressed limit、STT bounds、prompt diagnostics、trace/WS/ProjectInfo/browser secret redaction 全部通过。

### 13.4 最终产物、复杂度与重复代理

#### 产物

| 产物 | Task 1 基线 | Task 21 最终 | 说明 |
|---|---:|---:|---|
| release `nonoclaw` | 10,755,072 B（当时是未能重建的 existing artifact） | 12,288,928 B；SHA-256 `b9a83f1b30c0dd71053ab3f95337b104055da853ae6cc7e81be97ad350d197e1` | +14.26%；最终值由成功 release build 产生，基线不是成功同工具链重建，不能当作严格 apples-to-apples 性能回退 |
| `dist/index.html` | 57.71 kB / gzip 9.92 kB | 64.01 kB / gzip 11.10 kB | raw +10.92%，gzip +11.90% |
| 主 JS | 843.77 kB / gzip 260.37 kB | 883.37 kB / gzip 270.34 kB；SHA-256 `06b4632754009cb06bc0dafce907e0552f062c00fe695d8fb0d4f4b2cc01e09b` | raw +4.69%，gzip +3.83%；Vite 仍给出 >500 kB advisory |
| 主 CSS | 29.29 kB / gzip 8.05 kB | 29.29 kB / gzip 8.05 kB | 无实质变化 |

#### 静态复杂度代理

- 原 `serve_http.rs` 约 2,231 行单体已删除；当前按 10 个职责模块拆分，总计 4,044 行（包含各模块测试），最大文件 `connection.rs` 1,739 行，单文件峰值下降约 22%。协议、session、run、project、upload、speech、static 与错误映射已有独立所有者；总行数增加来自补齐协议、安全与测试，不能解释为复杂度下降本身。
- `engine/src/loop_.rs` 2,190 行、`settings.rs` 2,813 行、`skills.rs` 1,756 行；相对 Task 1 的粗略行数代理没有下降。其 ToolExecutor、RunController、SessionService、ResolvedConfig、Extensions/Trace 已拆到权威模块，但仓库没有固定 McCabe/cognitive-complexity analyzer，因此**圈复杂度数值仍为 UNMEASURED**，不伪造结论。
- 前端 `store.ts` 从 407 行单体变为 31 行 facade + `slices.ts` 596 行 + `transitions.ts` 336 行（另 150 行测试）；`App.tsx` 403、`websocket.ts` 342、`BreathField.tsx` 223。状态转换已成为纯函数并可独立压力测试，但总 LOC 不是下降 KPI。

#### 重复代码代理

Task 21 使用可复现的“生产源码去测试模块、去空白/`//` 行、规范空白后，跨文件完全相同 6-line window”代理回算基线与最终：

- 基线：75 files / 16,747 logical lines / 52 repeated windows / 112 extra instances。
- 最终：100 files / 26,966 logical lines / 86 repeated windows / 209 extra instances。
- repeated-window density：约 3.10 → 3.19 / 1k logical lines；该粗代理**未证明重复下降**。Task 21 已移除一份 checked protocol fixture 重复（全源码代理从 171 降至 144 windows），但剩余高频窗口主要是各 ToolDefinition/schema 的结构形状和 media service 边界样板。不得把这一结果写成“重复已下降”；后续应配置语法感知 clone analyzer 并继续收敛共享 metadata。

### 13.5 性能结论、advisory 与限制

- 确定性无界增长代理通过：50,000 token deltas 不逐 token 通知 React，energy 有界并衰减；trace/run/prompt/outbound metadata 有固定 retention；hidden page pause 与 reduced motion 通过。
- Vite production build 有一个非致命 advisory：主 JS chunk >500 kB；相对 Task 1 的 gzip 传输代理增加 3.83%。这不是构建失败，但首屏体积代理没有改善。
- **UNMEASURED**：真实 Chrome FCP/LCP、60s FPS p50/p95、long frames、CPU、heap delta、hidden-tab CPU、真实多浏览器 peer socket farm。仓库仍没有 Lighthouse/Playwright/browser performance harness；本任务没有虚构这些数值。
- **未执行（有意）**：真实 Anthropic/OpenAI、WebSearch/WebFetch、ElevenLabs、cloudflared tunnel、远程 MCP/provider 服务。对应行为只用 fixture/loopback/contract tests 验收。

**最终判定**：功能保留与所有必需的确定性质量门为 **PASS**（187 Rust tests + 3 frontend deterministic suites + Rust/TS protocol fixtures + release/frontend builds）。然而 Requirement 11.8 的“重复下降”和真实浏览器“性能不回退”不能宣称完全通过：精确 6-line clone proxy 与首屏 JS gzip proxy均略升，真实浏览器指标未测。因此完整 KPI 验收状态为 **PASS WITH DOCUMENTED METRIC GAPS / NOT FULLY PROVEN**，剩余限制如上，不以臆测数据掩盖。