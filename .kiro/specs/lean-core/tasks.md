# NonoClaw 精干化与体验提升 Implementation Plan

> 硬约束：当前已有功能全部保留。任何删除都只能发生在调用者已迁移后的重复实现、死代码、失效配置或错误历史注释上。不得以减少功能数量换取代码量下降。

- [x] 1. 建立 Feature Preservation Matrix 和工程基线
  - 枚举并记录全部 CLI flags、运行模式、内建工具、MCP 能力、HTTP routes、ClientMsg/ServerMsg/EngineEvent、配置字段、扩展路径和前端功能。
  - 为每项标记当前入口、实现文件、依赖、已知缺陷和重构后的权威所有者。
  - 记录 workspace 测试、Clippy、前端 build、关键文件复杂度、重复代码、release 体积、首屏和动画性能基线。
  - 对无法确认用途的代码先标记，不删除。
  - Requirements: 1.1-1.8, 11.8-11.9

- [x] 2. 为关键现有行为建立 characterization tests
  - 覆盖 headless、Web、remote、MCP server 和 session resume 的最小成功路径。
  - 覆盖当前全部工具注册名与 schema 快照。
  - 覆盖配置层优先级、JSONL 兼容、WebSocket 协议和扩展发现顺序。
  - 使用 Provider fixture，不请求真实外部 API。
  - 将测试结果关联到 Feature Preservation Matrix。
  - Requirements: 1.2-1.8, 11.7, 11.9

- [-] 3. 引入统一 `ResolvedConfig` 和配置来源诊断
  - 保留用户、项目、local、显式 settings、环境变量和 MCP 配置来源。
  - 把合并过程改为纯函数，并为每个最终字段记录来源。
  - 明确定义 scalar、array、permissions、models、hooks 和 MCP 的合并规则。
  - 对未知字段、错误引用和冲突给出文件/字段级诊断。
  - 迁移 CLI、Web、remote、子 Agent、compact 和 doc model 使用同一配置结果。
  - 所有调用者迁移完成后删除重复解析 helper。
  - Requirements: 2.1-2.2, 6.1-6.7

- [~] 4. 建立统一 `ClientFactory`
  - 根据 ModelProfile 和用途构造 conversation、compact、document、subagent Client。
  - 删除 Web 模型切换中的进程级 `set_var`，避免并发 session 相互污染。
  - 支持安全 Client 缓存，并确保密钥不进入 trace/WS。
  - 迁移 main、serve_http、compact 和 attachment 处理的独立 Client 构造逻辑。
  - 保持现有模型 Profile、切换模型和多模型批跑行为。
  - Requirements: 2.1-2.2, 5.7, 6.3-6.6

- [~] 5. 完善 Anthropic/OpenAI Provider adapter
  - 把请求序列化、流解析、usage、stop reason、capability 和错误归一化限制在 `nonoclaw-api`。
  - 为 OpenAI 实现真实 streaming 文本和工具参数增量。
  - 保持 Anthropic SSE、thinking、cache usage 和 prompt caching 行为。
  - 加入带 jitter 和总上限的流前重试；流中断保留已接收内容。
  - 用 fixtures 覆盖两种格式、图片、工具、错误、取消和不支持能力。
  - Requirements: 5.1-5.7

- [~] 6. 建立 `RunContext`、`RunController` 和统一终态
  - 为每次运行生成 run ID、session ID、token tree 和 event sequence。
  - 统一 Web、headless、remote 和子 Agent 的启动与取消流程。
  - 把顶层 task、事件 consumer、子 Agent 和后台工具纳入同一取消树。
  - 确保 Done/Cancelled/Error 只提交一次，并包含明确终止原因。
  - 保持现有最大轮数、预算、自动 compact 和多 Agent 能力。
  - Requirements: 2.2, 3.1, 3.4-3.8

- [~] 7. 提取并统一 `ToolExecutor`
  - 从 QueryEngine 提取 lookup、validate、permission、Hook、call、normalize 和 trace pipeline。
  - 使用 Semaphore 或 `buffer_unordered(cap)` 让 `NONOCLAW_MAX_TOOL_CONCURRENCY` 真实生效。
  - 保持 concurrency-safe 批次并行、非安全工具 barrier 和结果原序关联。
  - 实现统一的超大结果摘要/本地引用策略。
  - 统一路径、命令、网络、写入覆盖和破坏性元数据处理。
  - 保留所有现有工具名称、aliases、schema 和行为。
  - Requirements: 3.2-3.3, 4.1-4.7

- [~] 8. 完善后台任务和子 Agent 生命周期
  - 将后台 Bash child、状态、通知、停止和进程回收集中到 `BackgroundTaskManager`。
  - 将 Agent/Coordinator 的递归深度、工具过滤、并发上限和取消集中管理。
  - 保证 session 结束、取消或进程退出时不遗留后台进程。
  - 为多个子 Agent 并行、部分失败和父任务取消添加测试。
  - 保持 Agent、Coordinator、后台 Bash 和任务通知功能。
  - Requirements: 1.4, 3.4, 3.8, 4.4

- [~] 9. 统一 TodoWrite 与 Task 系列底层状态
  - 分析 TodoWrite 和 TaskCreate/Get/List/Update 的语义差异，定义共享 TaskStore 与状态转换。
  - 保留所有工具名和输入/输出契约，仅合并重复存储、ID、状态和序列化逻辑。
  - 防止子 Agent 覆盖父 Agent 的 Todo，同时允许 Coordinator 管理任务图。
  - 将任务变化作为结构化 RunEvent 提供给 CLI 和 Web。
  - Requirements: 1.4, 2.3, 2.5

- [~] 10. 补齐 Hook action 和生命周期
  - 将 Command、Prompt、HTTP 实现为统一 HookAction；不支持时在加载阶段明确报错。
  - 对齐声明的 HookType 与真实调用点，补齐 PostToolUseFailure、Stop、SubagentStart/Stop、Pre/PostCompact 等行为。
  - 实现统一 HookDecision：allow、deny、ask、updated input。
  - 增加 timeout、取消、脱敏日志和失败策略。
  - 保持用户级/项目级配置、matcher 和覆盖行为兼容。
  - Requirements: 7.1-7.5, 9.1, 9.5

- [~] 11. 统一 Extensions 发现、来源和冲突诊断
  - 为 Skills、Profiles、Plugins 和 MCP 建立共享 ExtensionDescriptor。
  - 保留现有项目/用户/plugin/bundled 路径、条件激活、trigger、slash、fork、热重载和动态发现。
  - 显式定义同名覆盖优先级并在 Insight 中显示冲突。
  - MCP/Plugin/Skill 局部失败不影响其他扩展和核心 Agent。
  - 为 Skill 激活发出 reason/source/version trace event。
  - Requirements: 7.1-7.2, 7.6-7.8

- [~] 12. 将会话读写集中到 `SessionService`
  - 为每个 session 使用单 writer actor/channel 维护内存 revision 与 JSONL 顺序。
  - 保持旧 SessionEntry、resume/continue/list/title/tag/mode/summary 和 repair 兼容。
  - 让 compact replace、Clear 和 append 通过同一命令通道原子提交。
  - 旧会话坏行可跳过并产生 SessionRepair 事件。
  - 所有调用者迁移后删除 handler 中直接文件写入和重复 session helper。
  - Requirements: 2.1, 3.5, 3.7, 8.1, 8.5

- [~] 13. 为 WebSocket 增加 revision、sequence 和协议一致性
  - 为事件添加 protocol version、run ID、session revision、sequence 和 timestamp。
  - 保留所有现有 ClientMsg、ServerMsg 和 EngineEvent 能力，并提供兼容解析。
  - 使用 revision/sequence 替代前端全局 `skipOneLoad` 和 `ignoreUntilLoad`。
  - 修复 Clear/Cancel/reconnect/peer sync 的事件竞态和幽灵工具卡。
  - 通过 schema 生成或 checked fixtures 保证 Rust/TypeScript 类型一致。
  - Requirements: 2.5, 8.2-8.7

- [~] 14. 按职责拆分 `serve_http`，保持全部路由
  - 拆分 protocol、connection、run_handler、session_hub、project_service、upload_service、speech_service、static_service。
  - 保留 Web UI、PWA、QR/tunnel、多端同步、附件、文档图片、STT、文件树、Git、ProjectInfo 和模型切换。
  - 确保异步锁不跨 WebSocket send、网络请求、Git 或磁盘等待。
  - 对 ProjectInfo 按 git/config/skills version 做显式缓存或去重。
  - 统一 HTTP/WS 错误映射和公网 token policy。
  - Requirements: 1.3, 1.6, 2.6, 8.3, 8.7-8.8, 11.2-11.4

- [~] 15. 建立结构化技术透明事件流
  - 在 core 定义 RunEvent 和 EventEnvelope，覆盖模型、上下文、工具、权限、Hook、重试、压缩、子 Agent、usage、恢复和终态。
  - 让 CLI、WebSocket、TraceCollector 和 BreathController 消费同一事件源。
  - 显示请求模型与实际模型、等待对象/耗时、重试、结果截断和自动修复。
  - 实现字段级脱敏和单次运行 trace JSON 导出。
  - 明确不展示隐藏思维链，只展示可验证技术事实。
  - Requirements: 2.5, 9.1-9.8

- [~] 16. 将 InsightRail 演进为 Technical Trace
  - 保留现有 Project、MCP、Skills、Git 等 Insight 信息。
  - 增加按时间排序的运行 timeline、上下文占用、turn、token/cache、工具/Hook/子 Agent 状态。
  - 使用默认摘要、展开详情和开发者诊断三级信息密度。
  - 将重试、降级、自动修复和冲突诊断放入技术面板，不污染聊天正文。
  - 提供脱敏 trace 导出入口。
  - Requirements: 1.6, 9.1-9.8

- [~] 17. 重构前端状态与重连逻辑
  - 将 Zustand 拆为 connection、session、run、tool、project、dialog、media 和 breath slices。
  - 服务端 session 作为持久事实源；localStorage 只保留 UI 偏好和草稿。
  - 使用 connection generation、session revision 和 event sequence 处理重连和 peer 同步。
  - 保留 optimistic send，但以 server ack/revision 进行确认或回滚。
  - 保留全部现有 UI 功能并减少重复消息、状态迟滞和重复点击。
  - Requirements: 1.6, 2.4, 8.2-8.4, 10.8-10.9

- [~] 18. 建立统一 BreathController 和呼吸状态机
  - 将散落的 pulse/flare/settle 调用迁移到订阅 RunEvent 的 BreathController。
  - 实现 idle、connecting、thinking、streaming、tool、waiting、compacting、subagent、success、error、reconnecting 状态。
  - 使用连续插值和节流 token-energy，避免逐 token React 重渲染。
  - 页面隐藏时暂停/降频，支持 `prefers-reduced-motion` 和文本状态。
  - 校准工具、权限、错误和成功反馈，使其明显但克制。
  - 增加确定时钟状态测试和长流性能测试。
  - Requirements: 10.1-10.9

- [~] 19. 加固安全、日志和错误模型
  - 默认关闭完整 Prompt dump；显式开启时显示警告并脱敏。
  - 公网/tunnel 访问强制 token，localhost 保持低摩擦。
  - 统一 AppError code、retryable、operation、trace ID 和 safe details。
  - 加固 canonical path、上传限制、Hook/Plugin/MCP 来源展示和敏感字段过滤。
  - 验证密钥不会进入 ProjectInfo、trace、WebSocket 或浏览器 store。
  - Requirements: 8.8, 9.8, 11.1-11.3

- [~] 20. 清理已迁移的重复实现和过时说明
  - 仅删除已确认所有调用者迁移后的旧 helper、重复状态、死代码和失效配置。
  - 对每次删除提供调用搜索、测试和 Feature Preservation Matrix 证据。
  - 清理 Phase 0、旧 TUI、错误工具数量、旧路径和与当前行为矛盾的注释。
  - 从注册表/共享元数据生成工具、CLI 和配置参考，减少文档漂移。
  - 更新根 README 与 rust/README，说明技术透明和呼吸体验。
  - Requirements: 2.7, 11.5, 12.1-12.5

- [~] 21. 完成功能保留、质量和体验验收
  - 逐项执行 Feature Preservation Matrix，确认当前已有功能 100% 保留或等价改善。
  - 运行 `cargo fmt --check`、`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`。
  - 运行前端 production build 和协议一致性检查。
  - 压测多工具并发、子 Agent、后台任务、Clear/Cancel/reconnect、多 peer 和长会话 compact。
  - 验证 Anthropic/OpenAI 流式契约、三类 Hook action 和扩展故障隔离。
  - 对比重复代码、复杂度、首屏、流式渲染、内存和 BreathField 帧率基线，确保无性能回退。
  - Requirements: 1.1-1.8, 11.6-11.9
