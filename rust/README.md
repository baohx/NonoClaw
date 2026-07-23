# NonoClaw Rust Workspace

本文件是 Rust workspace 的贡献者入口；产品功能、安装、完整配置示例和用户指南以仓库根目录 [`README.md`](../README.md) 为权威说明。本 workspace 不包含历史 TUI/bridge 模式；`-p` / `--print` 仍作为兼容的 headless 入口保留。

## 构建与运行

要求 Rust 1.82+。前端资源由仓库根目录的 `frontend/` 构建。

```bash
cargo build --workspace
cargo test --workspace
cargo run -p nonoclaw -- -p "explain this workspace"
cargo run -p nonoclaw -- --serve-http 127.0.0.1:8765
```

运行 `cargo run -p nonoclaw -- --help` 查看由 Clap 定义生成的完整 CLI 参考。Web Insight 中的 CLI 表使用同一份定义，不维护第二套 flags/defaults。

保留的运行模式包括：

- headless（参数或 stdin，text/JSON 输出）；
- HTTP/Web UI + WebSocket；
- TCP JSON-lines remote server/client（`--serve` / `--remote`）；
- MCP client/server（`--mcp-config` / `--mcp-serve`）；
- plugin install、public tunnel、session resume/continue/list。

## 唯一所有权边界

| 职责 | 权威所有者 |
|---|---|
| 消息、权限、usage、扩展描述、`RunEvent`/envelope | `nonoclaw-core` |
| Anthropic/OpenAI adapter、stream/retry/capability、Client 缓存 | `nonoclaw-api` / `ClientFactory` |
| 工具注册与 pipeline、权限、任务、后台进程、MCP | `nonoclaw-tools` / `ToolExecutor` / shared `TaskStore` |
| Agent loop、取消、会话、配置、Hooks/Skills/Profiles、trace | `nonoclaw-engine` / `RunController` / `SessionService` / `ResolvedConfig` |
| CLI、headless、remote、MCP shell、HTTP/WS adapters | `nonoclaw` binary |

`crates/cli/src/serve_http/` 按 `protocol`、`connection`、`run_handler`、`session_hub`、`project_service`、`upload_service`、`speech_service`、`static_service` 拆分。handler 不再拥有独立的配置、Client、session 写入或 wire 映射实现。

## 工具与配置参考

`nonoclaw-tools::register_all()` 是核心工具注册权威；完整名称与 model-facing schema 由 `crates/tools/tests/snapshots/builtin_tool_contract.json` 锁定，MCP discovery 再动态扩展运行时注册表。MCP server 继续保留既有兼容契约（不暴露 Agent，且不额外加入 ToolSearch）。不要在文档中维护独立的“工具数量”。

配置来源按 user → project → local → explicit settings 合并，独立/显式 MCP 作为对应来源进入同一 `ResolvedConfig`。CLI/Web/remote/subagent/compact/document client 都从该快照派生；环境引用作为输入解析，不通过模型切换修改进程环境。顶层配置参考来自 `settings::CONFIG_REFERENCE`，并与 unknown-field diagnostics 共用。

数据与扩展路径统一使用 `.nonoclaw`：

- 用户配置/扩展：`$NONOCLAW_HOME` 或 `~/.nonoclaw/`；
- 项目配置：`<cwd>/.nonoclaw/settings.json`、`settings.local.json`、`mcp.json`；
- Skills：项目、用户、plugin、bundled 与运行时动态发现；
- Profiles：`.nonoclaw/agents/*.md`；
- Hooks：用户和项目 `.nonoclaw/hooks.json`；
- Plugins：用户与项目 `.nonoclaw/plugins/`。

Profile 定义 Agent 行为；Skill 提供可激活工作流；Plugin 打包扩展资产；Hook 响应生命周期；MCP 提供进程外工具。冲突按显式 precedence 确定 winner，shadowed/failed 来源与可操作诊断仍展示在 Insight，单个扩展失败不会阻止核心 Agent。

## 技术透明、呼吸与安全

统一结构化事件流记录 requested/actual model、context、stream、工具校验/权限/执行、Hook、重试、压缩、子 Agent、usage、恢复与唯一终态。CLI、WebSocket、Technical Trace、trace export 和前端 BreathController 消费同一事实源；不展示或推断隐藏思维链。

BreathController 使用确定状态机驱动 idle/connecting/thinking/streaming/tool/waiting/compacting/subagent/success/error/reconnecting，采用连续插值、节流 token energy、hidden-page pause 和 `prefers-reduced-motion`，组件不直接维护第二套运行状态。

安全约束：完整 Prompt dump 默认关闭；显式诊断只写脱敏元数据；API key、Authorization、附件正文和敏感工具输入不进入 ProjectInfo/trace/WebSocket/browser store；公网/tunnel WebSocket 与 media routes 强制 token；文件打开与上传经过 canonical path、大小和类型边界。

## 会话与兼容

`SessionService` 是 JSONL 与内存 revision 的唯一写入所有者。旧 SessionEntry、resume/continue/list/title/tag/mode/summary 和坏行跳过/repair 行为保持兼容。WebSocket 使用 protocol version、run/session identity、revision、sequence 和 timestamp 去重，同时保留原有 ClientMsg/ServerMsg tags 与前端功能。

不得删除或重命名现有 flags、routes、tool names/schemas、protocol tags、config keys、`.nonoclaw` paths、hooks、extensions、media 或 UI themes，除非先有调用搜索、characterization tests 和 Feature Preservation Matrix 授权。

## 本地验证

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cd ../frontend && npm run test:breath && npm run test:transitions
npm run test:security && npm run build
```

Provider 与协议测试只使用仓库内 fixtures/loopback transports，不请求真实外部 API。

## 许可证

MIT
