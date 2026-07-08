# CLAUDE.md

本文件为 Claude Code (claude.ai/code) 在本仓库中工作时提供指引。

> 这是英文 `CLAUDE.md` 的中文译本。默认被加载的是同目录下的英文版;若希望以本文件为主,可将其覆盖为 `CLAUDE.md`。代码、文件路径、API 名称、标志位等保持英文原样,正文译为中文。

## 本仓库是什么

本目录下存放的是 **Claude Code(智能体 CLI)的 TypeScript 源码提取**,位于 `src/`。`src/entrypoints/cli.tsx` 中的 `--version` 分支会打印 `` `${MACRO.VERSION} (Claude Code)` ``。仓库名 "NonoClaw" 是对上游产品的混淆写法。请把整棵目录树当作上游源码用于研读与修改。

## 关键提醒:本仓库不含构建工具链

本目录树**只包含 `src/`**——没有 `package.json`、`tsconfig.json`、lockfile、`node_modules`、README,也没有任何构建/测试/lint 配置,git 也还没有提交。常规命令(`npm test`、`bun run build`、lint、跑单个测试)在这里**都无法执行**,因为工具链和依赖都缺失。

源码是为 **Bun 的打包器(bundler)** 编写的,**无法直接编译/运行**,因为它依赖一个外部构建流水线来解析以下构建期设施:

- `import { feature } from 'bun:bundle'` —— 用于在内部("Ant")构建与外部构建之间做**死代码消除(DCE)**的特征开关。任何 `feature('FLAG')` / `feature(...)` 守卫的条件 `require()` 块,都会在打包时被整体保留或整体剔除。不要假设某个被开关的分支一定会执行;很多开关是仅限内部的(如 `KAIROS`、`VOICE_MODE`、`TEAMMEM`、`COORDINATOR_MODE`、`BRIDGE_MODE`、`DUMP_SYSTEM_PROMPT`)。
- `MACRO.VERSION`(以及其他 `MACRO.*`)—— 由打包器在构建期内联的宏。

修改时请原样保留这些构造——不要把 `feature(...)` 替换成布尔常量,也不要把被开关的分支直接内联展开。

## 运行时技术栈

TypeScript + TSX(React),终端 UI 基于 **Ink**(自定义 Root 在 `src/ink.ts`),CLI 解析用 `@commander-js/extra-typings`,Schema 用 **Zod v4**(`zod/v4`,仍保留部分旧的 `zod`),Anthropic SDK(`@anthropic-ai/sdk`),MCP SDK(`@modelcontextprotocol/sdk`)。运行时为 Bun/Node ESM——**所有 import 都带显式 `.js` 后缀**(包括仅类型的 import),并使用 `src/` 路径别名(如 `from 'src/services/api/claude.js'`)。

## 入口与启动流程

- `src/entrypoints/cli.tsx` —— 引导入口。对 `--version` 做零 import 快速路径,之后动态导入完整 CLI,以保持冷启动路径足够廉价。它刻意在模块顶层读取 `feature()` 开关与 `process.env`(其中一些会在 import 时被捕获进模块级常量,参见 `ABLATION_BASELINE` 的注释)。
- `src/main.tsx` —— 完整 CLI:Commander 的命令/选项树、OAuth/配置初始化,随后调用 `launchRepl()`。文件顶部会在**其余 import 之前**执行性能敏感的副作用(启动 profiler、MDM 原始读取、钥匙串预取)——顺序很关键。
- `src/replLauncher.tsx` → `src/interactiveHelpers.tsx` —— 启动 Ink TUI(`src/components/App.tsx`)。
- `src/entrypoints/init.ts` —— 全局初始化 + 信任后的遥测。
- `src/entrypoints/mcp.ts` —— 把 Claude Code 自身作为 MCP server 运行。
- `src/entrypoints/sdk/` —— Agent SDK 的请求/响应/控制 schema(`coreSchemas.ts`、`controlSchemas.ts`)。

## 智能体循环(产品的核心)

- `src/QueryEngine.ts` —— 编排完整的一个 agent 回合:组装 system prompt + 消息历史、管理 tool_use/tool_result 配对、调用 `query()`、把每个 `tool_use` 派发给对应 `Tool`、流式输出 SDK 消息、处理压缩与重试。它是单次 API 调用之上的那一层。
- `src/query.ts` —— 针对一次 Anthropic Messages API 的流式回合(`services/api/claude.ts` → `client.ts`):token/压缩追踪、错误归类、reactive-compact 与 context-collapse(二者都受 `feature()` 开关控制)。
- `src/services/api/claude.ts` —— 真正的 `messages.create` 流式调用,以及用量/成本累计(`src/cost-tracker.ts`)。

数据结构经由 `src/types/message.ts` 中集中定义的消息类型流转(`UserMessage`、`AssistantMessage`、`ToolUseSummaryMessage`、`AttachmentMessage`、`SystemMessage` 等)。

## 工具 —— `Tool` 接口与每个工具的目录布局

`Tool` 抽象(`src/Tool.ts`)是核心扩展点。每个工具都是 `src/tools/<Name>Tool/` 下的一个**目录**,包含:

- `<Name>Tool.ts(x)` —— 实现 `Tool` 接口(逻辑)。
- `prompt.ts` —— **面向模型**的描述,注入进 system prompt。
- `UI.tsx` —— **面向终端**的 Ink 渲染(工具的进度/结果展示)。
- 辅助文件(例如 BashTool 的 `bashSecurity.ts`、`bashPermissions.ts`、`readOnlyValidation.ts`)。

`Tool<Input, Output, Progress>` 接口(`src/Tool.ts:362`)关键成员:
- `name`、`aliases?`、`inputSchema`(Zod → 转成 JSON Schema 供 API 使用)、`inputJSONSchema?`(MCP 工具)、`outputSchema?`
- `call(args, context, canUseTool, parentMessage, onProgress)` —— 执行
- `prompt({ getToolPermissionContext, tools, agents })` —— 面向模型的文本
- `description(input, opts)` —— 面向人类的摘要
- 权限生命周期:`validateInput()` → `checkPermissions()` → `useCanUseTool` hook → 审批 → `call()`。工具特有逻辑放在 `checkPermissions`;通用逻辑放在 `src/utils/permissions/`(`bashClassifier`、`filesystem`、`dangerousPatterns`、`classifierDecision`、`getNextPermissionMode`……)。
- 只读/并发/破坏性提示:`isReadOnly`、`isConcurrencySafe`、`isDestructive`、`interruptBehavior`、`isSearchOrReadCommand`
- 延迟加载:`shouldDefer`(按需经 `ToolSearchTool` 加载)与 `alwaysLoad` 相对
- `maxResultSizeChars` —— 超过阈值的结果会落盘,模型拿到预览 + 路径。设为 `Infinity` 可豁免(如 Read)。

工具在 `src/tools.ts` 中注册。仅限内部的工具通过 `feature()` 守卫的条件 `require()` 引入(在外部构建中被 DCE 剔除)。`TestingPermissionTool`、`SyntheticOutputTool`、`TungstenTool` 属特殊/内部工具。

## 斜杠命令

`src/commands/` —— 每个命令要么是一个目录(含 `index.ts`),要么是一个单独的 `.ts`。全部在 `src/commands.ts` 中 import 并注册。示例:`compact`、`clear`、`commit`、`diff`、`doctor`、`mcp`、`config`、`memory`、`resume`。部分命令有非交互变体(如 `context/index.ts` 同时导出 `context` 与 `contextNonInteractive`)。

## 服务层(`src/services/`)

横切子系统,各自独立成目录:`api/`(Anthropic client、OAuth、bootstrap、files)、`mcp/`(MCP server 连接 + 审批)、`analytics/`(事件 + GrowthBook 开关)、`compact/`(auto/reactive/micro 压缩)、`oauth/`、`plugins/`、`lsp/`、`policyLimits/`、`remoteManagedSettings/`、`settingsSync/`、`extractMemories/`、`SessionMemory/`、`tokenEstimation.ts`。

## TUI 与状态

- `src/components/` —— Ink/React 组件(`App.tsx`、`design-system/`、diff 查看器、对话框、进度行)。
- `src/hooks/` —— 驱动 TUI 的 React hook(`useCanUseTool`、`useCommandQueue`、`useCancelRequest`、`useApiKeyVerification`……)。
- `src/state/` —— `AppState.tsx`(全局应用状态)、`AppStateStore.ts`、`store.ts`、`selectors.ts`。状态变更经 `onChangeAppState.ts` 流转。
- `src/screens/`、`src/keybindings/`、`src/vim/` —— 模态屏、按键绑定、vim 模式。

## 其他顶层区域

`memdir/`(memory/MEMORY.md 系统)、`skills/`、`plugins/`、`migrations/`(配置/schema 迁移)、`native-ts/`、`coordinator/` + `bridge/`(多智能体/远程协调,受开关控制)、`remote/`(远程会话管理 + WebSocket)、`upstreamproxy/`、`voice/`、`buddy/`、`assistant/`。

## 需要遵守的非显见约定

- **不要调整 import 顺序。** 文件开头有 `// biome-ignore-all assist/source/organizeImports: ANT-ONLY import markers must not be reordered`。import 顺序对打包器的 DCE 和副作用顺序是承载性的。在这些文件上禁用任何"自动整理 import"操作。
- **顶层副作用是有意为之。** 很多被标注了 `eslint-disable custom-rules/no-top-level-side-effects` / `no-process-env-top-level`,因为它们必须在 import 时执行(性能、被捕获进常量的 ablation 开关)。未经核查调用方前,不要把它们重构成惰性初始化。
- **集中类型定义以打破 import 循环。** 共享类型(权限、工具进度、消息)放在 `src/types/`,即使存在更近的来源也**从那里重新 import**——参见反复出现的 "Import … from centralized location to break import cycles" 注释。新增跨工具/引擎使用的类型时,放进 `src/types/`。
- **所有 import 都带 `.js` 后缀**,包括仅类型的 import 和 `src/` 别名。
- **`feature()` 开关与 `MACRO.*` 是构建期设施**,不是运行期——切勿替换为字面量。
