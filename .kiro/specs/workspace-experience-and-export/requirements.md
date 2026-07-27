# Requirements Document

## Introduction

本功能改进 NonoClaw 的工作区体验、运行透明度、远程文件获取、历史会话一致性、工具链配置和高保真导出。当前 Web UI 已具备 Technical Trace 摘要与时间线、文件树、实时 Tool Card、带修订号的会话快照、状态栏模型与执行模式选择、Markdown/KaTeX/Mermaid/SVG/ECharts 渲染，以及最后一条助手消息的 Markdown 导出；代码中还存在 Word-compatible HTML 与浏览器打印辅助逻辑，但尚未生成兼容 DOCX 或直接 PDF。当前文件树仅支持打开文件，历史 Tool Use/Tool Result 尚未统一成实时卡片，ECharts 初始化仅在组件挂载和窗口 resize 时触发，`settings.json` 也尚无显式工具链可执行文件配置。

本需求在保留本地与 `--tunnel` 部署、鉴权、会话历史、可访问性和现有协议兼容性的前提下，定义用户可见行为。此阶段只描述产品需求；技术方案和可执行任务将在后续阶段提交用户反馈。

## Glossary

- **NonoClaw**: 提供 CLI、HTTP/WebSocket 服务、会话持久化、工具执行和 Web UI 的完整产品。
- **Web_UI**: 浏览器中的 NonoClaw 用户界面。
- **Insight_Panel**: Web UI 右侧展示运行事实、工具、配置、系统和项目信息的面板。
- **Run**: 从一条用户请求开始，到成功、失败或取消终态结束的一次代理执行。
- **Run_Insight**: Insight Panel 顶部展示所选 Run 的摘要、时间线、筛选器和安全技术事实的区域。
- **Technical_Fact**: 从运行事件允许列表生成、可向用户展示且不包含隐藏推理的结构化事实。
- **Technical_Fact_Render_Limit**: 单次可渲染 Technical Fact 数量的可配置性能边界；Web UI 显示当前生效值，不在本需求中规定任意固定值。
- **Run_State**: Run 的可见状态；取值为 `idle`、`active`、`waiting`、`success`、`warning`、`failure`、`cancel` 或 `info`。
- **SYSTEM_Section**: Insight Panel 中展示本机语言运行时及关联工具可用性的区域。
- **PROJECT_Section**: Insight Panel 中展示当前工作区、上下文、会话和 Git 摘要的区域。
- **Runtime_Probe**: 对已解析可执行文件执行一次有界版本查询并生成可用性结果的操作。
- **Runtime_Probe_Timeout**: 单个 Runtime Probe 的可配置超时，默认值为 5 秒。
- **Runtime_Probe_Output_Limit**: 单个 Runtime Probe 可采集标准输出与标准错误的可配置字节上限；产品配置定义默认值并在 SYSTEM Section 显示生效值。
- **Toolchain**: Rust、Node.js、Python 运行时及关联包管理、编译和环境工具集合。
- **Executable_Entry**: `settings.json` 中一个可执行文件的 `path`、期望 `version` 和解析来源数据。
- **Executable_Settings**: `settings.json` 中由 Rust、Node.js 和 Python Executable Entry 组成的配置。
- **Settings_Layer**: 用户设置、项目设置、项目本地设置或显式 `--settings` 文件中的一个配置来源。
- **Resolution_Source**: 可执行文件最终字段取值的来源，包括 Settings Layer、等价 CLI 覆盖、运行时目录推导或 `PATH` 发现。
- **Resolved_Configuration_Fingerprint**: 根据字段级优先级合并后的全部 Executable Settings、等价 CLI 覆盖和影响探测结果的边界配置生成的稳定缓存键；任一组成字段变化都会生成不同缓存键。
- **Authoritative_Model_Catalog**: 由 Provider 与产品模型配置共同提供、用于确定可选主对话模型的权威目录；Slash Command Metadata 不属于该目录。
- **File_Tree**: Web UI 左侧展示 Workspace Root 内文件与目录的树形控件。
- **Workspace_Root**: NonoClaw 启动时选定并限制文件操作范围的规范工作目录。
- **Canonical_Path**: 按目标操作系统语义解析分隔符、`.`、`..` 和符号链接后得到的绝对文件系统路径。
- **Configured_Path**: OS 绝对路径，或以 `${HOME}`、`${WORKSPACE}` 开头且展开后为 OS 绝对路径的配置值。
- **Download_Service**: 验证请求、读取 Workspace Root 内文件并向浏览器返回下载响应的服务。
- **Download_Max_Bytes**: 单个文件允许下载的可配置安全上限，默认值为 2 GiB。
- **Download_Stream_Buffer_Limit**: 下载流单次内存缓冲区的可配置性能上限，默认值不超过 1 MiB。
- **Access_Token**: NonoClaw 为 Web 访问生成并在服务端进程内存中验证的认证值；浏览器运行时仅在内存中持有 Access Token。
- **Authenticated_Tunnel**: 通过 `--tunnel` 或公开地址提供、要求 Access Token 的 Web 连接。
- **Local_Deployment**: 仅监听 loopback 且不经过代理转发的 NonoClaw Web 部署。
- **Tool_Use**: Run 中包含工具名称、稳定调用标识和结构化输入的一次工具调用请求。
- **Tool_Result**: 与一个 Tool Use 稳定调用标识对应的成功或失败输出。
- **Stable_Call_ID**: 由运行协议提供并持久化、用于按完全相等规则关联 Tool Use 与 Tool Result 的调用标识。
- **Stable_Content_Block_ID**: 由运行协议提供并持久化、在 Session Snapshot 重放与 Rehydration 期间保持不变的内容块标识。
- **Content_Block_Order**: Session History 中用户文本、助手文本、Tool Use、Tool Result 和渲染内容块的持久化顺序；相同位置以 Stable Content Block ID 升序排列。
- **Execution_Card**: 在聊天时间线中共同展示一个 Tool Use 的命令摘要、输入、状态和 Tool Result 的单一卡片。
- **Session_History**: 持久化并可通过会话恢复加载的用户消息、助手消息、Tool Use 和 Tool Result。
- **Session_Snapshot**: 服务端向 Web UI 发送的带会话标识与修订号的完整历史快照。
- **Sensitive_Value**: API 密钥、Access Token、Authorization/Cookie 值、密码、私钥、凭据或服务端明确标记为秘密的值。
- **Restricted_Trace_Content**: 原始提示词、完整工具输入、完整工具结果、附件正文、Provider 请求/响应正文和隐藏推理；该内容可能并非凭据，但不属于 Technical Fact。
- **Observability_Surface**: Run Insight、Technical Fact、诊断、Project Info、结构化错误、日志和 trace 导出等用于解释系统行为的表面。
- **Conversation_Surface**: 用户消息、助手消息、Execution Card 和用户主动打开的源文件等承载主要内容的表面。
- **Server_Serialization_Boundary**: 服务端把 Observability Surface 数据编码为协议消息、日志、诊断、错误或导出数据之前的边界。
- **REDACTED_Marker**: 取代被安全隐藏值的分类占位符，格式为 `[REDACTED: category]`。
- **Truncation_Marker**: 表示长度裁剪而非安全隐藏的占位符，格式为 `[TRUNCATED: visible/total]`，其中 `visible` 和 `total` 均为字节数或字符数且使用同一单位。
- **Composer**: 聊天区底部包含输入框、附件、语音、运行选项和发送按钮的控件。
- **Model_Selector**: 选择下一次 Run 使用的主对话模型的控件。
- **Execution_Mode_Selector**: 选择 `default`、`acceptEdits`、`auto`、`bypassPermissions` 或 `plan` 权限执行模式的控件。
- **Slash_Command_Metadata**: 由命令权威实现提供的名称、说明、参数提示、风险和执行方式数据，仅用于斜杠命令。
- **Slash_Command_Selector**: 从 Slash Command Metadata 中选择非 Skill 斜杠命令的控件。
- **Skill_Command**: 由内置、用户、项目或插件 Skill 动态贡献的 `/skill-name` 命令。
- **Run_Last_Turn**: 一个 Run 中最后一个用户请求及对应的助手内容、Execution Card 和终态信息组成的可导出单元。
- **Export_Service**: 将 Run Last Turn 的可见渲染结果生成 DOCX 或 PDF 文件的组件。
- **Export_Artifact_Timeout**: 等待 Rendered Artifact 布局、字体和稳定快照完成的可配置时限，默认值为 10 秒。
- **Rendered_Artifact**: Markdown 中已渲染的 Mermaid、LaTeX、表格、Mojo Unicode 符号、ECharts 或 SVG 内容。
- **Chart_Source**: ECharts 代码块中符合 JSON 语法、仅包含 JSON 数据类型且不执行代码的图表 option 数据。
- **ECharts_Block**: 由 Chart Source 渲染的可交互图表区域。
- **Rehydration**: Session Snapshot 加载、重连、会话切换或组件重新挂载后，从持久化源数据恢复可见 UI 的操作。

## Requirements

### Requirement 1: 可解释且字段完整的 Run Insight

**User Story:** 作为 NonoClaw 用户，我希望理解 Insight 顶部区域的用途、字段、控件和状态，以便判断一次运行正在做什么、使用了什么模型以及为何等待或结束。

#### Acceptance Criteria

1. THE Run_Insight SHALL 显示“仅展示可验证运行事实，不展示隐藏推理、原始提示词或原始私密内容”的用途说明。
2. WHEN Run_Insight 显示 `run` 字段, THE Run_Insight SHALL 同时显示所选 Run 的短标识和 Run State。
3. WHEN Run_Insight 显示 `model` 字段, THE Run_Insight SHALL 显示实际模型。
4. WHEN 实际模型与请求模型不同, THE Run_Insight SHALL 以“请求模型 → 实际模型”显示模型解析结果。
5. WHEN Run_Insight 显示 `turn` 字段, THE Run_Insight SHALL 显示从 1 开始的当前轮次或最后完成轮次。
6. WHEN 轮次上限可用, THE Run_Insight SHALL 以“当前轮次/轮次上限”显示 `turn` 字段。
7. WHEN Run_Insight 显示 `context` 字段, THE Run_Insight SHALL 显示估算上下文 token 数和上下文窗口上限。
8. WHEN 上下文窗口上限可用, THE Run_Insight SHALL 计算上下文使用百分比，并在 `context` 字段可见时对大于 0 的上限显示四舍五入到一位小数的结果、对等于 0 的上限显示 `0.0%`。
9. WHEN Run_Insight 显示 `tokens` 字段, THE Run_Insight SHALL 分别显示累计输入、输出、缓存读取和缓存写入 token 数。
10. IF 任一 Run Insight 字段没有可用数据, THEN THE Run_Insight SHALL 为对应字段显示 `—` 和“尚无数据”说明。
11. THE Run_Insight SHALL 为所选 Run、保留 Run 范围、Technical Fact 类别、Run State、文本搜索、诊断详情、复制、导出和清除控件提供可见说明。
12. THE Run_Insight SHALL 解释 `idle`、`active`、`waiting`、`success`、`warning`、`failure`、`cancel` 和 `info` 的含义。
13. WHEN Run_Insight 显示 Run 分组标题, THE Run_Insight SHALL 显示短 Run 标识、Run State 和可见 Technical Fact 数量。
14. WHEN Run_Insight 显示 Technical Fact 行, THE Run_Insight SHALL 显示时间戳、状态标记、摘要、类别和序号。
15. WHERE 诊断详情已启用, THE Run_Insight SHALL 显示安全详情键值、事件类型、事件标识、会话标识和可用的父 Run 标识。
16. WHEN Run_Insight 排序 Technical Fact, THE Run_Insight SHALL 按事件序号升序排列，并对缺少或相同事件序号的事实依次按时间戳和事件标识升序排列。
17. WHEN 用户输入 Technical Fact 搜索文本, THE Run_Insight SHALL 使用 Unicode 大小写折叠后的字面子串匹配摘要、类别、事件类型、事件标识和安全详情值。
18. IF 筛选结果为空, THEN THE Run_Insight SHALL 显示“无匹配技术事实”和 Restricted Trace Content 不被收集的说明。
19. WHEN 可见匹配事实超过 Technical Fact Render Limit, THE Run_Insight SHALL 显示最新的上限数量、匹配总数、当前生效上限和“仅显示最新事实”的说明。
20. WHEN 用户选择不同的保留 Run, THE Run_Insight SHALL 使用所选 Run 更新全部摘要、token、状态和 Technical Fact 字段。

### Requirement 2: PROJECT 之前的 SYSTEM 工具链概览

**User Story:** 作为开发者，我希望在 PROJECT 之前看到 Rust、Node.js 和 Python 的安装与可用性，以便在代理调用工具前发现环境问题。

#### Acceptance Criteria

1. THE Insight_Panel SHALL 在 PROJECT Section 正上方显示 SYSTEM Section。
2. THE SYSTEM_Section SHALL 为 `rustc`、`cargo` 和 `rustup` 分别显示状态、解析路径、版本和 Resolution Source。
3. THE SYSTEM_Section SHALL 为 `node`、`npm`、`npx` 和 `corepack` 分别显示状态、解析路径、版本和 Resolution Source。
4. THE SYSTEM_Section SHALL 为 `python` 和 `pip` 分别显示状态、解析路径、版本和 Resolution Source。
5. THE SYSTEM_Section SHALL 显示 Python 虚拟环境能力状态和用于探测该能力的 Python 路径。
6. WHEN Runtime Probe 找到特定可执行文件且版本满足期望版本, THE SYSTEM_Section SHALL 仅为该可执行文件显示 `available` 状态。
7. WHEN Runtime Probe 找不到特定 Executable Entry 的候选可执行文件, THE SYSTEM_Section SHALL 仅为该 Executable Entry 显示 `missing` 状态。
8. IF Runtime Probe 已找到的特定路径不是可执行普通文件, THEN THE SYSTEM_Section SHALL 仅为该路径对应的 Executable Entry 显示 `invalid` 状态和失败的 Resolution Source。
9. IF Runtime Probe 得到的特定 Executable Entry 版本与 Executable Settings 中的期望版本不同, THEN THE SYSTEM_Section SHALL 仅为该 Executable Entry 显示 `version mismatch` 状态、期望版本和实际版本。
10. IF 特定 Executable Entry 的 Runtime Probe 超时、输出超过 Runtime Probe Output Limit 或无法解析版本, THEN THE SYSTEM_Section SHALL 仅为该 Executable Entry 显示 `invalid` 状态和与失败类别对应的确定性修复建议。
11. WHEN SYSTEM Section 首次请求一个Resolved Configuration Fingerprint, THE NonoClaw SHALL 对每个 Executable Entry 执行一次 Runtime Probe 并缓存完成结果。
12. WHEN 多个请求并发探测同一Resolved Configuration Fingerprint, THE NonoClaw SHALL 合并并发请求为同一次 Runtime Probe。
13. WHEN 用户刷新 SYSTEM Section, THE SYSTEM_Section SHALL 使当前探测缓存失效、执行一次新 Runtime Probe 并显示完成时间。
14. WHILE 特定 Executable Entry 的 Runtime Probe 正在运行, THE SYSTEM_Section SHALL 仅为该 Executable Entry 显示 `checking` 状态并保留该 Executable Entry 的上一次完成结果。
15. IF Runtime Probe 在 Runtime Probe Timeout 内未完成, THEN THE NonoClaw SHALL 终止探测进程并关闭探测进程资源。
16. WHEN SYSTEM Section 显示探测边界, THE SYSTEM_Section SHALL 显示 Runtime Probe Timeout 和 Runtime Probe Output Limit 的当前生效值。
17. WHEN SYSTEM Section 中任一工具显示 `invalid` 状态, THE SYSTEM_Section SHALL 为该工具显示与失败类别对应的确定性修复建议。

### Requirement 3: 本地与 tunnel 均安全的文件树下载

**User Story:** 作为本地或远程用户，我希望从左侧文件树右键下载文件，以便安全获取工作区产物而不绕过鉴权或工作区边界。

#### Acceptance Criteria

1. WHEN 用户右键 File Tree 中的文件, THE File_Tree SHALL 显示包含 `Download` 的上下文菜单。
2. WHEN 用户选择 `Download`, THE Download_Service SHALL 下载所选文件并使用经过清理的原始文件名。
3. WHEN Download Service 返回文件, THE Download_Service SHALL 保持源文件与下载文件的字节序列完全一致。
4. IF 用户右键 File Tree 中的目录, THEN THE File_Tree SHALL 将 `Download` 显示为不可用并解释目录下载不受支持。
5. WHEN 浏览器请求文件下载, THE Download_Service SHALL 要求 Local Deployment 和 Authenticated Tunnel 均提供服务端进程内存中存在且有效的 Access Token。
6. IF 下载请求缺少 Access Token 或 Access Token 无效, THEN THE Download_Service SHALL 在查询目标文件元数据、规范化目标文件路径或读取目标文件内容前返回结构化 `401 authentication` 响应。
7. WHERE Local Deployment 已启用, THE Web_UI SHALL 通过当前已认证连接在浏览器运行时内存中提供 Access Token，无需用户手动输入凭据。
8. WHEN Access Token 验证成功, THE Download_Service SHALL 将请求路径规范化为 Canonical Path 并确认 Canonical Path 位于 Workspace Root 内。
9. IF 请求路径包含父目录穿越、绝对路径替换或符号链接逃逸, THEN THE Download_Service SHALL 返回结构化 `403 path_denied` 响应。
10. IF 请求目标不是可读取普通文件, THEN THE Download_Service SHALL 返回结构化 `404 not_found` 或 `400 invalid_request` 响应。
11. WHEN Download Service 返回成功响应, THE Download_Service SHALL 设置安全的 `Content-Disposition`、`Content-Type`、`Content-Length`、`Cache-Control: no-store` 和 `X-Content-Type-Options: nosniff` 响应头。
12. WHEN Download Service 生成下载文件名, THE Download_Service SHALL 仅使用目标文件 basename 并移除路径分隔符、控制字符、回车、换行、NUL 和响应头分隔字符。
13. IF 清理后的下载文件名为空、为 `.` 或为 `..`, THEN THE Download_Service SHALL 使用 `download` 作为 ASCII 回退名。
14. WHEN 下载文件名包含非 ASCII 字符, THE Download_Service SHALL 同时提供安全 ASCII 回退名和 RFC 5987 `filename*` 值。
15. WHEN 目标文件大小为 0 字节, THE Download_Service SHALL 返回成功响应和 `Content-Length: 0`。
16. IF 目标文件大小超过 Download Max Bytes, THEN THE Download_Service SHALL 在读取文件内容前返回结构化 `413 payload_too_large` 响应并显示实际大小与当前生效上限。
17. WHILE 文件内容正在传输, THE Download_Service SHALL 使用不超过 Download Stream Buffer Limit 的单次内存缓冲区流式传输文件。
18. WHILE 文件内容正在传输, THE Download_Service SHALL 排除把完整文件载入服务端或浏览器内存的行为。
19. WHILE 文件内容正在传输, THE Download_Service SHALL 在收到浏览器取消通知后停止传输并最终释放打开的文件句柄。
20. IF 文件在响应期间无法继续读取, THEN THE Download_Service SHALL 终止传输并释放打开的文件句柄。
21. WHEN 文件读取失败需要记录, THE Download_Service SHALL 记录预先构造且不含请求 URL、查询参数、Access Token、Canonical Path 或文件内容的结构化读取错误。
22. THE Download_Service SHALL 从 URL、重定向地址、应用日志、Technical Fact、错误响应和下载文件名中排除 Access Token。
23. THE Download_Service SHALL 从应用日志、Technical Fact 和错误响应中排除 URL 查询参数和文件内容。

### Requirement 4: 实时与历史统一的 Execution Card

**User Story:** 作为恢复历史会话的用户，我希望每次命令及结果显示在同一张卡片中，以便历史记录与实时执行具有相同结构。

#### Acceptance Criteria

1. WHEN Web UI 收到实时 Tool Use, THE Web_UI SHALL 创建一张以 Stable Call ID 为稳定键的 Execution Card。
2. WHEN Web UI 收到具有完全相等 Stable Call ID 的 Tool Result, THE Web_UI SHALL 在对应 Execution Card 中更新结果和完成状态。
3. WHEN Tool Result 表示失败, THE Execution_Card SHALL 显示失败状态和安全错误摘要。
4. WHEN Web UI 加载 Session Snapshot, THE Web_UI SHALL 按 Stable Call ID 完全相等规则将每个 Tool Use 与对应 Tool Result 合并为一张 Execution Card。
5. WHEN 一个助手消息包含多个 Tool Use, THE Web_UI SHALL 按 Content Block Order 显示多张 Execution Card。
6. WHEN Tool Use 前后存在助手文本, THE Web_UI SHALL 按 Content Block Order 保持助手文本与 Execution Card 的原始相对顺序。
7. IF Session History 包含无对应 Tool Result 的 Tool Use, THEN THE Web_UI SHALL 在单张 Execution Card 中显示 `result unavailable` 状态。
8. IF Session History 包含无对应 Tool Use 的 Tool Result, THEN THE Web_UI SHALL 在 Tool Result 的 Content Block Order 位置显示 `command unavailable` Execution Card、Stable Call ID 和 Tool Result 指示的成功或失败状态。
9. IF Session History 包含重复 Stable Call ID 的 Tool Use, THEN THE Web_UI SHALL 保留 Content Block Order 中首个 Tool Use 并显示重复调用诊断。
10. IF Session History 包含重复 Stable Call ID 的 Tool Result, THEN THE Web_UI SHALL 保留 Content Block Order 中最后一个 Tool Result 并显示重复结果诊断。
11. THE Execution_Card SHALL 在折叠标题中显示工具名、命令摘要和运行状态。
12. WHERE Execution Card 已展开, THE Execution_Card SHALL 分别标注 `Command` 与 `Result`。
13. WHEN 相同或较旧修订号的 Session Snapshot 再次到达, THE Web_UI SHALL 保持每个 Stable Call ID 只有一张 Execution Card 且不追加重复内容。
14. WHEN 较新修订号的 Session Snapshot 到达, THE Web_UI SHALL 按完整快照重建 Execution Card 并保持 Stable Call ID 与 Content Block Order 的确定性结果。
15. THE Web_UI SHALL 对实时和历史 Execution Card 使用相同的折叠、复制、换行恢复、成功、失败和等待呈现规则。

### Requirement 5: REDACTED 的安全含义、范围与可观测性

**User Story:** 作为用户，我希望知道 `REDACTED` 是否与安全有关、哪些数据会被隐藏以及隐藏发生在哪里，以便区分安全遮罩、内容省略、裁剪和执行失败。

#### Acceptance Criteria

1. THE Insight_Panel SHALL 明确说明 REDACTED Marker 表示安全或隐私边界内的主动遮罩，不表示工具失败。
2. THE Insight_Panel SHALL 明确说明用户输入中的 `REACTED` 是 `REDACTED` 的拼写误差。
3. THE Insight_Panel SHALL 列出 `credential`、`authorization`、`private-key`、`restricted-content` 和 `host-path` 五种 REDACTED Marker 类别。
4. WHEN Observability Surface 包含 Sensitive Value, THE NonoClaw SHALL 在 Server Serialization Boundary 使用对应分类的 REDACTED Marker 替换完整值。
5. IF NonoClaw 无法在 Server Serialization Boundary 遮罩已识别的 Sensitive Value, THEN THE NonoClaw SHALL 阻止对应 Observability Surface 数据的序列化并返回不含该 Sensitive Value 的结构化错误。
6. WHEN NonoClaw 从原始运行事件生成 Technical Fact, THE NonoClaw SHALL 使用 `[REDACTED: restricted-content]` 表示存在但不允许进入 Technical Fact 的 Restricted Trace Content。
7. WHEN Observability Surface 显示被遮罩字段, THE NonoClaw SHALL 保留安全的字段名称、遮罩类别、运行状态、计数、持续时间、退出码和关联标识。
8. WHEN Conversation Surface 中的普通用户消息或普通助手消息不包含服务端明确分类的 Sensitive Value, THE Web_UI SHALL 保留完整对话正文。
9. WHEN Conversation Surface 包含服务端明确分类的 Sensitive Value, THE NonoClaw SHALL 在值到达 Web UI 前使用对应 REDACTED Marker 替换具体值。
10. WHEN Execution Card 显示结构化工具输入或结果, THE Web_UI SHALL 保留服务端允许显示的非敏感字段并显示服务端提供的 REDACTED Marker。
11. WHEN Workspace Root 或用户主目录内的绝对路径需要显示, THE NonoClaw SHALL 分别使用稳定的 `<WORKSPACE>` 或 `<HOME>` 前缀。
12. WHEN 其他主机绝对路径不在允许显示范围内, THE NonoClaw SHALL 使用 `[REDACTED: host-path]`。
13. WHEN 内容仅因长度限制被裁剪, THE NonoClaw SHALL 使用 Truncation Marker 而非 REDACTED Marker。
14. WHEN NonoClaw 生成 Truncation Marker, THE NonoClaw SHALL 显示裁剪前总量、保留量和一致的计量单位。
15. IF 字段名称包含 `token`、`input`、`text` 或 `content` 但字段值符合安全允许列表, THEN THE NonoClaw SHALL 根据字段语义而非名称子串决定显示或遮罩。
16. WHERE Authenticated Tunnel 已启用, THE NonoClaw SHALL 对 REDACTED Marker 使用与 Local Deployment 相同或更严格的服务端规则。
17. WHEN 用户复制或导出 Technical Fact, THE Web_UI SHALL 仅复制或导出已经过服务端遮罩的值。
18. THE Web_UI SHALL 从可序列化状态、持久化浏览器存储、DOM 属性和下载文件中排除 REDACTED Marker 所替代的原始值。
19. WHEN 用户查看 REDACTED Marker 帮助, THE Insight_Panel SHALL 显示类别、Observability Surface、Conversation Surface、未遮罩内容和 Truncation Marker 差异矩阵。

### Requirement 6: Composer 中的模型、执行模式与命令选择

**User Story:** 作为聊天用户，我希望在输入消息的位置选择模型、执行模式和斜杠命令，以便发送前在同一上下文完成所有运行设置。

#### Acceptance Criteria

1. THE Composer SHALL 包含 Model Selector、Execution Mode Selector 和 Slash Command Selector。
2. WHEN 用户更改 Model Selector, THE Composer SHALL 将选定模型用于下一次 Run。
3. WHEN Composer 显示 Model Selector 选项, THE Composer SHALL 仅显示 Authoritative Model Catalog 中的模型标签与模型标识。
4. THE Model_Selector SHALL 从模型选项、模型搜索结果和模型计数中排除 Slash Command Metadata。
5. WHEN Authoritative Model Catalog 更新, THE Model_Selector SHALL 按目录提供的稳定顺序刷新模型选项并保留仍然有效的当前选择。
6. IF 当前选择不再存在于 Authoritative Model Catalog, THEN THE Model_Selector SHALL 显示 `unavailable` 并要求用户在发送下一次 Run 前选择有效模型。
7. WHEN 用户更改 Execution Mode Selector, THE Composer SHALL 在发送前显示模式名称和权限风险说明。
8. THE Web_UI SHALL 从顶部状态栏移除可交互的 Model Selector 与 Execution Mode Selector。
9. WHERE 顶部状态栏继续显示模型或执行模式, THE Web_UI SHALL 将状态栏内容显示为只读当前状态。
10. WHEN Slash Command Selector 打开, THE Slash_Command_Selector SHALL 显示 Slash Command Metadata 中的全部非 Skill Command。
11. THE Slash_Command_Selector SHALL 从选项、搜索结果和计数中排除全部 Skill Command。
12. WHEN 用户选择 Slash Command, THE Slash_Command_Selector SHALL 根据 Slash Command Metadata 验证命令参数要求。
13. WHEN 用户选择无需参数的斜杠命令, THE Composer SHALL 显示命令说明并提供“确认执行”与“插入输入框”选项。
14. WHEN 用户选择需要参数的斜杠命令, THE Composer SHALL 先显示命令说明，再显示参数提示并将命令写入输入框供用户完成。
15. IF 用户选择会清除或替换会话状态的命令, THEN THE Composer SHALL 在执行前显示确认步骤。
16. WHEN 用户手动输入有效斜杠命令, THE Composer SHALL 保留现有键盘提交与命令解析行为。
17. WHEN 用户手动输入 Skill Command, THE Composer SHALL 保留 Skill 激活能力且不把 Skill Command 加入 Slash Command Selector。
18. WHILE 没有 Run 正在执行, THE Composer SHALL 保持 Model Selector 和 Execution Mode Selector 可供用户交互。
19. WHILE Run 正在执行, THE Composer SHALL 禁用用户对 Model Selector 和 Execution Mode Selector 的打开、搜索、键盘选择和提交行为。
20. WHILE Run 正在执行, THE Composer SHALL 保持当前 Run 已捕获的模型与执行模式值不受选择器状态或外部目录更新影响。
21. WHILE Run 正在执行, THE Composer SHALL 说明模型和执行模式可在 Run 结束后由用户变更。
22. WHEN Run 到达终态, THE Composer SHALL 恢复 Model Selector 和 Execution Mode Selector 的用户可交互状态。

### Requirement 7: settings.json 中每个可执行文件的显式路径与版本

**User Story:** 作为跨平台开发者，我希望在 settings.json 中声明 Rust、Node.js、Python 及关联工具的路径和版本，以便工具调用使用确定的可执行文件。

#### Acceptance Criteria

1. THE Executable_Settings SHALL 支持 `rust`、`node` 和 `python` 三个 Toolchain 配置组。
2. THE Executable_Settings SHALL 为 `rustc`、`cargo`、`rustup`、`node`、`npm`、`npx`、`corepack`、`python` 和 `pip` 分别支持 Executable Entry。
3. THE Executable_Entry SHALL 支持可选 `path` 和可选精确 `version`。
4. WHEN Executable Entry 同时省略 `path` 和 `version`, THE NonoClaw SHALL 生成包含字段来源和修复建议的配置诊断。
5. IF NonoClaw 无法生成必需的 Executable Settings 配置诊断, THEN THE NonoClaw SHALL 返回结构化系统错误并阻止使用对应 Executable Entry。
6. THE Executable_Settings SHALL 支持 Python 虚拟环境能力要求。
7. THE Executable_Settings SHALL 仅接受 OS 绝对路径或以 `${HOME}`、`${WORKSPACE}` 开头且展开后为 OS 绝对路径的 Configured Path。
8. IF Executable Settings 包含普通相对路径、未支持变量、空路径、未知 Toolchain 字段或未知可执行文件字段, THEN THE NonoClaw SHALL 生成包含字段、来源和修复建议的配置诊断。
9. WHEN Configured Path 使用 `${HOME}` 或 `${WORKSPACE}` 前缀, THE NonoClaw SHALL 分别以用户主目录或 Workspace Root 展开前缀并生成 OS 绝对路径。
10. THE NonoClaw SHALL 排除相对于 settings.json 文件目录解析 Configured Path 的行为。
11. WHEN 多个 Settings Layer 定义同一 Executable Entry 字段, THE NonoClaw SHALL 按用户设置、项目设置、项目本地设置、显式 `--settings` 文件的低到高优先级解析对应字段。
12. WHERE 等价 CLI 覆盖已提供, THE NonoClaw SHALL 使等价 CLI 覆盖成为对应字段的最高优先级来源。
13. WHEN 高优先级来源只覆盖 Executable Entry 的 `path` 或 `version`, THE NonoClaw SHALL 保留低优先级来源提供的未覆盖兄弟字段。
14. WHEN NonoClaw 展示解析后的 Executable Settings, THE NonoClaw SHALL 为 `path`、`version` 和 Python 虚拟环境能力要求分别显示字段级 Resolution Source。
15. WHEN settings.json 未包含 Executable Settings, THE NonoClaw SHALL 保留现有 PATH 发现行为和兼容启动流程。
16. WHEN Executable Entry 包含显式 Configured Path, THE NonoClaw SHALL 仅使用对应 Configured Path 并排除验证失败后的 PATH 回退。
17. WHEN Executable Settings 在不同操作系统加载, THE NonoClaw SHALL 使用目标操作系统的路径语义完成前缀展开和规范化。

### Requirement 8: 确定性工具链解析、验证与安全执行

**User Story:** 作为开发者，我希望显式配置经过验证并被工具调用复用，以便代理减少路径猜测、错误版本和重复重试。

#### Acceptance Criteria

1. WHEN 工具存在显式 Configured Path, THE NonoClaw SHALL 优先使用对应 Configured Path。
2. WHEN 工具没有显式 Configured Path 且同一 Toolchain 的主运行时 Configured Path 已配置, THE NonoClaw SHALL 使用主运行时目录中的文档化固定候选名称顺序解析工具。
3. WHEN Toolchain 没有任何显式 Configured Path, THE NonoClaw SHALL 按文档化固定候选名称顺序执行一次 PATH 发现。
4. IF 任一显式 Configured Path 验证失败, THEN THE NonoClaw SHALL 停止对应 Executable Entry 的自动回退并返回配置诊断。
5. WHEN Runtime Probe 验证 Configured Path, THE NonoClaw SHALL 将 Configured Path 规范化为 Canonical Path 并确认目标为可执行普通文件。
6. WHEN Runtime Probe 查询 version, THE NonoClaw SHALL 以参数数组直接启动目标可执行文件并应用 Runtime Probe Timeout 与 Runtime Probe Output Limit。
7. THE NonoClaw SHALL 将 Executable Settings path 作为单一进程路径处理，并排除 shell 插值、命令拼接和额外参数解析。
8. IF Runtime Probe 超时、退出失败、超过输出上限或返回无法识别的 version, THEN THE NonoClaw SHALL 标记 Executable Entry 为 `invalid` 并返回类型分别为 `probe_timeout`、`probe_execution`、`probe_output_limit` 或 `version_unrecognized` 的单一确定性诊断。
9. IF 实际 version 明确不匹配显式精确 version, THEN THE NonoClaw SHALL 阻止依赖对应 Executable Entry 的内部工具调用并返回 `version mismatch` 诊断。
10. IF version 比较结果不确定但可执行普通文件验证与 Runtime Probe 执行成功, THEN THE NonoClaw SHALL 允许对应内部工具调用并返回 `version comparison inconclusive` 警告。
11. WHEN 首次解析一个 Resolved Configuration Fingerprint, THE NonoClaw SHALL 对每个 Executable Entry 完成一次发现、规范化和 Runtime Probe 并缓存结果。
12. WHEN Resolved Configuration Fingerprint未变化, THE NonoClaw SHALL 在同一进程中复用缓存结果而不再次发现路径或执行 Runtime Probe。
13. WHEN Executable Settings 文件变化、等价 CLI 覆盖变化或用户刷新 SYSTEM Section, THE NonoClaw SHALL 使对应配置指纹缓存失效并执行一次新解析与 Runtime Probe。
14. WHEN 内部工具调用需要 Toolchain, THE NonoClaw SHALL 使用缓存中已验证的可执行文件而不重新猜测候选路径。
15. WHEN Run 需要 Toolchain 信息, THE NonoClaw SHALL 向代理提供已选择的可执行文件、version 和 Resolution Source 的安全 Technical Fact。
16. WHEN Execution Card 显示 Toolchain 选择, THE NonoClaw SHALL 应用 Host Path 遮罩规则。

### Requirement 9: 每个 Run Last Turn 的高保真 DOCX/PDF 导出

**User Story:** 作为需要共享运行结果的用户，我希望导出每次运行的最后一轮为兼容 DOCX 或 PDF，以便保留文本、公式、图表和符号的可见效果。

#### Acceptance Criteria

1. WHEN Run 到达终态, THE Web_UI SHALL 在对应 Run Last Turn 上显示导出按钮。
2. WHEN 历史 Session Snapshot 完成 Rehydration, THE Web_UI SHALL 在每个可识别 Run Last Turn 上恢复导出按钮。
3. WHEN 用户打开导出选项, THE Export_Service SHALL 提供 DOCX 和 PDF 两种格式。
4. WHEN 用户选择 DOCX, THE Export_Service SHALL 生成具有 `.docx` 扩展名、DOCX MIME 类型、ZIP 文件签名以及有效 Office Open XML 内容类型、关系和文档部件的文件。
5. WHEN 用户选择 PDF, THE Export_Service SHALL 生成具有 `.pdf` 扩展名、PDF MIME 类型、有效 PDF 文件头与结束标记且可由标准 PDF 阅读器直接打开的下载文件。
6. THE Export_Service SHALL 按 Run Last Turn 的 Content Block Order 包含用户文本、助手文本、Execution Card 摘要、代码块、列表、链接和表格。
7. WHEN Run Last Turn 包含 Mermaid, THE Export_Service SHALL 将已完成布局的 Mermaid 图形导出为矢量内容或至少 144 DPI 的图像。
8. WHEN Run Last Turn 包含 LaTeX, THE Export_Service SHALL 导出排版后的公式而非原始定界符文本。
9. WHEN Run Last Turn 包含 ECharts Block, THE Export_Service SHALL 导出与屏幕当前 option、主题和尺寸一致的稳定图像。
10. WHEN Run Last Turn 包含 SVG, THE Export_Service SHALL 保留 SVG 的几何、文本、颜色和纵横比。
11. WHEN Run Last Turn 包含 Mojo Unicode 符号, THE Export_Service SHALL 使用嵌入字体或兼容字体回退保持符号可见。
12. WHEN Run Last Turn 包含宽表格或宽代码块, THE Export_Service SHALL 通过分页、缩放或换行使内容保持在页面边界内。
13. WHILE Rendered Artifact 尚未完成布局、字体加载或稳定快照, THE Export_Service SHALL 等待不超过 Export Artifact Timeout 并显示导出进度。
14. IF Rendered Artifact 在 Export Artifact Timeout 内无法完成, THEN THE Export_Service SHALL 停止等待并按 Content Block Order 与稳定内容块标识生成确定性的未完成内容集合、准备源代码回退结果，并显示“取消”与“下载已明确标注的源代码回退”操作。
15. WHILE Export Service 等待用户处理已准备的源代码回退结果, THE Export_Service SHALL 保持未完成内容集合与回退文件内容不变且不重新启动 Rendered Artifact 等待。
16. WHEN 用户选择源代码回退导出, THE Export_Service SHALL 按固定映射把 Mermaid、LaTeX、ECharts 和 SVG 的持久化源数据放入标注对应类型的代码块。
17. WHEN 用户选择源代码回退导出, THE Export_Service SHALL 对 DOCX 和 PDF 使用相同的 Content Block Order 与未完成内容集合。
18. IF DOCX 或 PDF 文件结构验证失败, THEN THE Export_Service SHALL 阻止下载并始终显示不含 Run Last Turn 正文的结构化导出错误。
19. IF 同一导出操作中的 DOCX 与 PDF 文件结构验证均失败, THEN THE Export_Service SHALL 显示一个列出两种受影响格式的合并结构错误。
20. IF 导出文件扩展名、MIME 类型或文件签名不属于同一所选格式, THEN THE Export_Service SHALL 阻止下载并显示格式不匹配错误。
21. WHEN DOCX 或 PDF 文件结构验证成功, THE Export_Service SHALL 通过一次下载响应交付完整文件。
22. THE Export_Service SHALL 使用经过清理的会话标识、Run 标识、UTC 时间和格式生成文件名。
23. WHERE Authenticated Tunnel 已启用, THE Export_Service SHALL 在已认证浏览器或已认证 NonoClaw 服务内完成导出。
24. THE Export_Service SHALL 从导出流程中排除向第三方服务发送 Run Last Turn 内容的行为。
25. WHEN 导出内容包含 REDACTED Marker, THE Export_Service SHALL 保留 marker 且排除 marker 所替代的原始值。

### Requirement 10: ECharts 历史持久化、Rehydration 与生命周期

**User Story:** 作为恢复历史结果或导出图表的用户，我希望 ECharts 在重连、切换会话、调整布局和导出后持续可见，以便历史图表不再消失。

#### Acceptance Criteria

1. WHEN 助手消息包含有效 Chart Source, THE NonoClaw SHALL 将完整 Chart Source、稳定内容块标识和 Content Block Order 作为 Session History 的一部分持久化。
2. THE NonoClaw SHALL 把 Chart Source 作为仅含 JSON 对象、数组、字符串、数字、布尔值和 `null` 的惰性数据处理。
3. THE NonoClaw SHALL 从 Chart Source 解析、持久化、Rehydration 和导出流程中排除函数、脚本、表达式求值、事件处理代码和动态代码执行。
4. WHEN Session Snapshot 加载包含 Chart Source 的消息, THE Web_UI SHALL 为每个稳定内容块标识显示 `pending` 状态、重新解析 Chart Source 并创建一个 ECharts Block 图表实例。
5. WHEN ECharts Block 图表实例完成首次布局, THE Web_UI SHALL 把对应 ECharts Block 状态从 `pending` 更新为 `active`。
6. WHEN WebSocket 重连后重复接收相同或较旧修订号的 Session Snapshot, THE Web_UI SHALL 保持每个稳定内容块标识只有一个活动图表实例。
7. WHEN 用户切换离开再返回会话, THE Web_UI SHALL 从持久化 Chart Source 恢复全部 ECharts Block。
8. WHEN ECharts 库在消息组件之后可用, THE Web_UI SHALL 在库就绪后对尚未初始化的 ECharts Block 自动重试一次初始化。
9. IF ECharts 库重试后仍不可用, THEN THE Web_UI SHALL 显示 `chart unavailable` 和可复制 Chart Source。
10. WHEN ECharts Block 创建成功, THE Web_UI SHALL 为对应容器建立 ResizeObserver。
11. WHEN ResizeObserver 报告 ECharts Block 容器尺寸变化, THE Web_UI SHALL 在下一次动画帧按容器实际尺寸执行一次合并后的图表 resize。
12. WHEN Insight Panel、File Tree、窗口或移动布局改变聊天宽度, THE Web_UI SHALL 通过 ECharts Block 的 ResizeObserver 保持图表与可见容器宽度一致。
13. WHILE ECharts Block 所在区域不可见或宽度为零, THE Web_UI SHALL 延迟 resize 并在 ResizeObserver 报告非零可见尺寸后的下一次动画帧执行一次 resize。
14. WHEN ECharts Block 卸载, THE Web_UI SHALL 释放图表实例、ResizeObserver、动画帧请求和事件监听器。
15. WHEN Chart Source 改变, THE Web_UI SHALL 在创建新图表实例前释放旧图表实例、ResizeObserver、动画帧请求和事件监听器。
16. IF Chart Source 不是有效 JSON 或 ECharts 拒绝 option, THEN THE Web_UI SHALL 显示安全错误摘要和可复制 Chart Source 而不修改 Session History。
17. WHEN Export Service 捕获 ECharts Block, THE Web_UI SHALL 等待图表完成布局并从当前图表实例生成稳定快照。
18. WHEN 导出完成或取消, THE Web_UI SHALL 保持屏幕中的 ECharts Block 实例、尺寸和交互状态可用。
19. THE ECharts_Block SHALL 在 `pending`、`active`、`chart unavailable` 和错误状态中提供可复制的持久化 Chart Source。

### Requirement 11: 部署、会话、协议与可访问性兼容

**User Story:** 作为现有 NonoClaw 用户，我希望上述改进兼容本地、tunnel、历史会话和现有命令，以便升级不破坏已有工作流。

#### Acceptance Criteria

1. WHERE Local Deployment 已启用, THE NonoClaw SHALL 保留无需用户手动输入 Access Token 的 loopback 启动体验。
2. WHEN Local Deployment 浏览器连接成功, THE NonoClaw SHALL 在浏览器运行时内存中提供上传、下载、语音和需要服务端数据的导出所需 Access Token。
3. WHERE Authenticated Tunnel 已启用, THE NonoClaw SHALL 对 WebSocket、上传、下载、语音和需要服务端数据的导出统一验证 Access Token。
4. WHEN 客户端加载不包含新 Run 边界元数据的旧 Session Snapshot, THE Web_UI SHALL 按消息顺序、Stable Call ID 和 Content Block Order 推导 Execution Card 与可导出的最后助手轮次。
5. WHEN 客户端加载包含新 Run 边界元数据的 Session Snapshot, THE Web_UI SHALL 使用显式 Run 标识和轮次标识恢复 Run Last Turn。
6. WHEN 旧客户端连接新服务端, THE NonoClaw SHALL 保持现有 WebSocket 消息标签、必需字段和既有字段语义兼容。
7. WHEN 新客户端连接缺少可选新字段的兼容服务端, THE Web_UI SHALL 使用 `—`、`unavailable` 或旧历史推导并保留聊天内容。
8. THE NonoClaw SHALL 保留手动斜杠命令、Skill Command、主题、附件、语音、权限提示、会话切换和 Markdown 渲染行为。
9. WHEN 任一新增操作失败, THE NonoClaw SHALL 返回不含 Sensitive Value 的结构化错误、操作名称、可重试标志和关联标识。
10. WHEN 键盘焦点位于 File Tree 项目, THE Web_UI SHALL 支持 `Shift+F10` 或 Context Menu 键打开上下文菜单。
11. WHEN 键盘焦点位于 Composer 选择器, THE Web_UI SHALL 支持方向键移动、`Home`/`End` 跳转、`Enter` 或 `Space` 选择以及 `Escape` 关闭。
12. WHEN 上下文菜单或 Composer 选择器从打开状态转换为关闭状态, THE Web_UI SHALL 把键盘焦点恢复到打开对应控件的元素。
13. THE Web_UI SHALL 为上下文菜单、Composer 选择器、SYSTEM Section、Run Insight 和导出控件提供符合可见主题对比度的焦点指示器。
14. THE Web_UI SHALL 为上下文菜单、Composer 选择器、SYSTEM Section、Run Insight 和导出控件提供可访问名称、角色、状态和值。
15. WHEN Run State、Runtime Probe 状态、下载错误、导出进度或 ECharts 错误发生变化, THE Web_UI SHALL 通过不抢占键盘焦点的屏幕阅读器状态区域播报变化。
16. WHERE `prefers-reduced-motion` 已启用, THE Web_UI SHALL 取消非必要动画并以静态视觉状态显示 Run State、导出进度和 ECharts 错误。
17. WHERE `prefers-reduced-motion` 已启用, THE Web_UI SHALL 保留与默认动效模式相同的操作结果、状态文本和键盘行为。
