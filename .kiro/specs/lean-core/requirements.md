# NonoClaw 精干化与体验提升 Requirements

## 产品目标

NonoClaw 要做的不是功能裁剪，而是把已经存在的能力做完整、做一致、做顺滑：

- **功能完整**：当前已有功能、入口和扩展能力全部保留。
- **实现精干**：一个概念只有一个权威实现，一个状态只有一个事实来源。
- **技术透明**：用户看得见模型、上下文、工具、权限、Hook、重试、压缩和消耗发生了什么。
- **呼吸体验**：界面有生命感、反馈及时、过渡自然，但不喧宾夺主。
- **用着爽**：低延迟、不中断、不丢状态、错误可恢复、操作结果可信。

“小而美”在本项目中的含义是：**功能可以丰富，但内核必须清楚；界面可以有个性，但交互必须克制。**

## 非目标

- 不通过删除已有用户功能、CLI 入口、工具或扩展机制来减少代码。
- 不为追求更少文件而合并职责清晰的 crate。
- 不进行无收益的全量重写或技术栈迁移。
- 不向用户展示模型隐藏思维链；技术透明只展示可验证的运行事实和系统决策。
- 不在本次重构中继续扩张新的产品功能面。

## Requirement 1: 建立功能保留契约

**User Story:** 作为现有用户，我希望重构后原有工作方式仍然可用，以便在获得更好体验的同时不承担功能回退。

### Acceptance Criteria

1. BEFORE 重构开始 THE SYSTEM SHALL 建立现有 CLI 参数、运行模式、内建工具、HTTP 路由、WebSocket 消息、设置字段和前端交互的功能清单。
2. WHEN 重构任一模块 THE SYSTEM SHALL 保持该模块已有的外部行为，或提供向后兼容的别名和迁移路径。
3. THE SYSTEM SHALL 保留 headless、HTTP/Web UI、远程 server/client、MCP server、插件安装、公共 tunnel 和会话恢复等现有运行能力。
4. THE SYSTEM SHALL 保留当前全部内建工具、MCP 工具、Agent/Coordinator、多任务、后台任务、权限和交互问答能力。
5. THE SYSTEM SHALL 保留 Hooks、Skills、Agent Profiles、Plugins、MCP、自动压缩、多模型和会话持久化能力。
6. THE SYSTEM SHALL 保留文件树、Insight、Git、Commit、QR/移动端、PWA、附件、文档图片、语音、模型切换、多模型批跑和多端会话同步体验。
7. WHEN 两项功能存在实现重叠 THE SYSTEM SHALL 合并其内部实现，同时保持原有用户入口和语义。
8. WHEN 无法证明一段代码无调用者且不承载兼容行为 THE SYSTEM SHALL NOT 删除该代码。

## Requirement 2: 消除重复实现与重复状态

**User Story:** 作为维护者，我希望每项核心职责只有一个权威实现，以便修改不会产生行为漂移。

### Acceptance Criteria

1. THE SYSTEM SHALL 为配置解析、模型 Client 构造、工具执行、权限判断、会话读写、运行取消和事件序列化分别指定唯一所有者模块。
2. WHEN Web、headless、remote 或子 Agent 启动一次运行 THE SYSTEM SHALL 复用同一套配置解析、Client Factory 和 Engine 构造流程。
3. WHEN TodoWrite 与 Task 系列工具操作任务数据 THE SYSTEM SHALL 复用统一的底层任务存储和状态转换，同时保留各工具现有 API。
4. THE SYSTEM SHALL 以服务端会话为持久化事实源；浏览器缓存只能用于断线期间的临时 UI 恢复，不得形成第二份长期会话真相。
5. THE SYSTEM SHALL 使用同一事件类型驱动 CLI 输出、WebSocket、运行追踪和前端状态，不得维护多套手写字段映射。
6. WHEN 相同的 ProjectInfo、模型 Profile 或 session 数据被多个分支需要 THE SYSTEM SHALL 通过共享服务计算一次或显式缓存，而不是复制业务代码。
7. THE SYSTEM SHALL 清除确认无调用入口的死代码、失效配置、重复 helper 和与当前实现矛盾的历史注释。

## Requirement 3: 提升 Agent 主循环的正确性与可预测性

**User Story:** 作为 Agent 用户，我希望长任务、并行工具、子 Agent 和取消操作都表现稳定，以便放心让系统自主工作。

### Acceptance Criteria

1. THE SYSTEM SHALL 将单次运行封装为有唯一 run ID、session ID 和 cancellation token 的运行上下文。
2. WHEN 模型返回多个工具调用 THE SYSTEM SHALL 仅并行执行明确声明 concurrency-safe 的工具，并保持结果和事件按原始调用顺序关联。
3. WHEN `NONOCLAW_MAX_TOOL_CONCURRENCY` 被配置 THE SYSTEM SHALL 使用真实的并发限制器约束在途工具数量。
4. WHEN 用户取消运行 THE SYSTEM SHALL 取消模型流、待执行工具、后台事件消费者和子 Agent，并只发出一次终态事件。
5. WHEN 模型返回孤立或不完整的 tool use/result THE SYSTEM SHALL 修复可恢复数据，并用结构化警告记录修复行为。
6. WHEN 达到最大轮数、预算或上下文边界 THE SYSTEM SHALL 返回明确的终止原因和建议动作。
7. WHEN 自动压缩在后台完成 THE SYSTEM SHALL 只在基于当前 transcript 仍然有效时原子替换消息，并记录压缩前后指标。
8. THE SYSTEM SHALL 防止子 Agent 无限递归，并保留现有 Agent、Coordinator 和多 Agent 并行能力。

## Requirement 4: 统一并完善工具运行时

**User Story:** 作为用户，我希望所有工具遵守一致的校验、权限、执行和结果规则，以便操作安全且容易理解。

### Acceptance Criteria

1. THE SYSTEM SHALL 通过统一 `ToolExecutor` 执行 find、validate、permission、pre-hook、call、post-hook、result-normalize 和 trace 流程。
2. WHEN 工具声明只读、并发安全或破坏性 THE SYSTEM SHALL 在权限、调度和 UI 中一致使用这些元数据。
3. WHEN 工具返回超大结果 THE SYSTEM SHALL 按统一策略保存完整结果并向模型/UI 提供摘要和本地引用。
4. WHEN Bash 进入后台运行 THE SYSTEM SHALL 提供可查询、可停止、可回收且不会泄漏进程的任务生命周期。
5. WHEN MCP 工具失败或 server 断开 THE SYSTEM SHALL 隔离失败、保留核心工具并提供重连状态。
6. THE SYSTEM SHALL 保留全部现有工具名称和输入 schema；内部重构不得导致 Prompt 工具契约漂移。
7. THE SYSTEM SHALL 对路径越界、命令风险、写入覆盖和网络访问使用统一的安全检查与错误格式。

## Requirement 5: 完善 Provider 兼容层

**User Story:** 作为多模型用户，我希望 Anthropic 与 OpenAI 兼容端点在流式输出、工具调用和用量统计上行为一致。

### Acceptance Criteria

1. THE SYSTEM SHALL 将 Provider 差异完全限制在 `nonoclaw-api` 内。
2. THE SYSTEM SHALL 为 Anthropic 和 OpenAI 格式提供统一的文本增量、工具参数增量、thinking 状态、终止原因和 usage 输出。
3. WHEN OpenAI 端点支持 streaming THE SYSTEM SHALL 使用真实流式响应，不得以一次性响应伪装成流。
4. WHEN Provider 不支持 thinking、cache usage、图片或工具调用 THE SYSTEM SHALL 返回明确 capability 状态，不得静默丢弃。
5. WHEN 网络请求在流开始前遇到可重试错误 THE SYSTEM SHALL 按有上限的指数退避重试，并把重试次数呈现给 trace。
6. WHEN 流中途失败 THE SYSTEM SHALL 保留已收到内容、返回结构化错误并允许用户重试。
7. THE SYSTEM SHALL 通过统一 Client Factory 创建默认模型、切换模型、compact 模型、文档模型和子 Agent 模型 Client。

## Requirement 6: 统一配置并避免全局副作用

**User Story:** 作为用户和维护者，我希望配置优先级明确、模型切换安全，且各运行模式结果一致。

### Acceptance Criteria

1. THE SYSTEM SHALL 保留当前用户、项目、local、显式 settings 和 MCP 配置来源及其兼容行为。
2. THE SYSTEM SHALL 将所有配置层解析为经过校验的 `ResolvedConfig`，并记录每个有效字段的来源。
3. WHEN CLI/Web/remote/子 Agent 使用配置 THE SYSTEM SHALL 从同一个 `ResolvedConfig` 派生运行选项。
4. WHEN 用户切换模型 THE SYSTEM SHALL 通过 Client Factory 创建或复用 Client，不得修改进程级环境变量影响其他并发运行。
5. WHEN 配置包含未知字段、无效模型引用或冲突设置 THE SYSTEM SHALL 给出文件、字段、来源和建议修复方式。
6. THE SYSTEM SHALL 继续支持环境变量引用，但不得在日志或前端暴露解析后的密钥。
7. THE SYSTEM SHALL 使配置合并规则可测试，并避免数组、权限或模型覆盖的隐式行为。

## Requirement 7: 补齐 Hooks、Skills、Profiles、Plugins 与 MCP

**User Story:** 作为高级用户，我希望已有扩展机制各司其职且实现完整，而不是存在配置字段却没有真实行为。

### Acceptance Criteria

1. THE SYSTEM SHALL 明确定义：Profile 配置 Agent 行为，Skill 提供工作流知识，Plugin 打包扩展，Hook 响应生命周期，MCP 提供外部工具。
2. THE SYSTEM SHALL 保留现有扩展发现路径、优先级和热重载能力，并在 Insight 中显示来源与覆盖关系。
3. WHEN Hook 声明 command、prompt 或 HTTP action THE SYSTEM SHALL 执行对应实现，或在加载时明确拒绝不支持的类型。
4. THE SYSTEM SHALL 让已声明的 Hook 生命周期与真实调用点一致，包括失败、Stop、Subagent 和 Compact 事件。
5. WHEN PreToolUse Hook 返回拒绝、修改输入或询问权限 THE SYSTEM SHALL 按统一协议处理，不允许含糊降级。
6. WHEN Skill 通过路径、trigger、slash command 或动态发现激活 THE SYSTEM SHALL 记录激活原因、来源和版本。
7. WHEN Plugin 或 MCP 部分加载失败 THE SYSTEM SHALL 隔离故障并展示可操作诊断，不阻止无关扩展和核心 Agent。
8. THE SYSTEM SHALL 防止扩展命名冲突被静默覆盖，并提供确定性的优先级和冲突提示。

## Requirement 8: 可靠的会话、WebSocket 与多端同步

**User Story:** 作为跨桌面和移动端使用的用户，我希望断线、刷新、取消和同步不会丢消息或留下幽灵工具卡。

### Acceptance Criteria

1. THE SYSTEM SHALL 为每个 session 使用单一串行化写入通道，避免并发 JSONL 追加和内存状态竞态。
2. WHEN WebSocket 重连 THE SYSTEM SHALL 使用连接代次和事件序号去重，不依赖脆弱的全局 skip flag。
3. WHEN 同一 session 有多个 peer THE SYSTEM SHALL 广播有序增量或版本化快照，并清理失效连接。
4. WHEN Clear 或 Cancel 发生 THE SYSTEM SHALL 先终止事件源，再提交新的 session version，确保旧事件不会污染清空后的 UI。
5. THE SYSTEM SHALL 保持旧 JSONL 会话可读、坏行可跳过、孤立工具对可修复。
6. THE SYSTEM SHALL 为 ClientMsg、ServerMsg 和 EngineEvent 建立共享 schema 或自动一致性检查。
7. THE SYSTEM SHALL 保留当前文件树、ProjectInfo、GitShow、附件、STT、权限、问题、模型切换和 session 管理协议。
8. WHEN 上传、STT、Git 或打开文件失败 THE SYSTEM SHALL 返回结构化、可恢复且不泄露敏感路径的错误。

## Requirement 9: 强化技术透明特色

**User Story:** 作为专业开发者，我希望理解 Agent 正在使用什么技术资源和为什么停顿，以便建立信任、定位问题和控制成本。

### Acceptance Criteria

1. THE SYSTEM SHALL 为每次运行生成按时间排序的技术事件流，至少包含模型、上下文预算、流状态、工具、权限、Hook、重试、压缩、子 Agent、usage 和终止原因。
2. THE SYSTEM SHALL 在 UI 中提供简洁默认视图和可展开技术详情，不把调试噪音直接塞入聊天正文。
3. WHEN Agent 等待权限、网络、工具、子 Agent 或压缩 THE SYSTEM SHALL 明确显示等待对象和已持续时间。
4. WHEN 模型实际返回的 model 与配置别名不同 THE SYSTEM SHALL 同时显示请求模型与实际模型。
5. WHEN 发生重试、自动修复、降级或结果截断 THE SYSTEM SHALL 在技术事件流中留下可见记录。
6. THE SYSTEM SHALL 显示可验证的运行事实，不展示或推断模型隐藏思维链。
7. THE SYSTEM SHALL 支持将单次运行的脱敏 trace 导出为 JSON，便于复现问题。
8. THE SYSTEM SHALL 默认隐藏密钥、完整敏感 Prompt、附件原文和超长工具结果。

## Requirement 10: 打磨呼吸感与“用着爽”的交互

**User Story:** 作为长时间使用 NonoClaw 的开发者，我希望界面反馈有生命感、流畅且克制，以便始终知道系统状态又不被打扰。

### Acceptance Criteria

1. THE SYSTEM SHALL 以确定的运行状态机驱动 BreathField，不依赖散落组件直接修改动画。
2. THE SYSTEM SHALL 区分 idle、connecting、thinking、tool-running、waiting-permission、waiting-question、compacting、success、error 和 reconnecting 状态。
3. WHEN 状态切换 THE SYSTEM SHALL 使用连续、无跳变的速度和幅度过渡。
4. WHEN 文本流持续到达 THE SYSTEM SHALL 以节流后的 token rhythm 调制呼吸，不触发逐 token React 重渲染。
5. WHEN 工具开始、成功或失败 THE SYSTEM SHALL 使用短促但克制的 flare，并与工具卡状态同步。
6. WHEN 用户启用 `prefers-reduced-motion` THE SYSTEM SHALL 降低或关闭非必要运动，同时保留文本状态反馈。
7. THE SYSTEM SHALL 在主流桌面设备保持稳定动画帧率，并避免后台标签页持续消耗资源。
8. WHEN 网络断开或操作等待超过阈值 THE SYSTEM SHALL 提供非阻塞恢复提示和明确的继续操作，不使用遮挡式全屏等待层。
9. THE SYSTEM SHALL 保持输入、取消、权限决策和会话切换的即时反馈，避免重复点击和状态迟滞。

## Requirement 11: 改善安全、性能与可维护性

**User Story:** 作为维护者，我希望项目在保留丰富功能的同时仍然容易理解、验证和演进。

### Acceptance Criteria

1. THE SYSTEM SHALL 默认关闭完整 Prompt 日志；显式开启时必须提示敏感信息风险并执行脱敏。
2. THE SYSTEM SHALL 对公网 tunnel、移动 token、文件打开、上传、Bash 和写入维持明确安全边界。
3. THE SYSTEM SHALL 避免在持有异步锁时执行网络、磁盘或 WebSocket 等待操作。
4. THE SYSTEM SHALL 将超大职责文件拆分为按协议、会话、运行、上传、语音和静态服务划分的模块，但不改变功能。
5. THE SYSTEM SHALL 为重复实现建立可检查的唯一所有者表；新增代码不得绕开所有者模块。
6. THE SYSTEM SHALL 通过 Rust fmt、workspace tests、Clippy 和前端 production build。
7. THE SYSTEM SHALL 为核心 Agent loop、工具 pipeline、Provider、扩展、session/WS 和配置建立聚焦的契约测试。
8. THE SYSTEM SHALL 以重复代码减少、圈复杂度降低、职责清晰和性能不回退衡量精干化，不以盲目减少总 LOC 为目标。
9. WHEN 重构完成 THE SYSTEM SHALL 在功能保留矩阵中证明每项现有功能仍可使用或已获得等价改善。

## Requirement 12: 统一文档与真实实现

**User Story:** 作为使用者和贡献者，我希望 README、配置示例和代码行为一致，以便不被历史说明误导。

### Acceptance Criteria

1. THE SYSTEM SHALL 以根 README 作为权威产品说明，并同步修正 `rust/README.md` 的过时内容。
2. THE SYSTEM SHALL 从实际注册表或共享元数据生成工具、CLI 和配置参考，避免手写数量漂移。
3. THE SYSTEM SHALL 清理 Phase 0、旧 TUI、错误路径和已不符合实际的注释。
4. THE SYSTEM SHALL 文档化各扩展机制的职责边界、覆盖优先级和诊断方式。
5. THE SYSTEM SHALL 提供“技术透明事件”和“呼吸状态”的简短说明，使特色可被用户理解而不只停留在视觉效果。
