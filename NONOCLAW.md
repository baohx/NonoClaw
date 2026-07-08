# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

This directory holds a TypeScript **source extraction of Claude Code** (the agent CLI) under `src/`. The `--version` path in `src/entrypoints/cli.tsx` prints `` `${MACRO.VERSION} (Claude Code)` ``. The repo name "NonoClaw" is an obfuscation of the upstream product. Treat the tree as the upstream source for study/editing.

## Critical caveat: no build tooling is present

This tree contains **only `src/`** — there is no `package.json`, `tsconfig.json`, lockfile, `node_modules`, README, or any build/test/lint config, and git has no commits. Standard commands (`npm test`, `bun run build`, lint, run a single test) **cannot be run** here because the toolchain and dependencies are absent.

The source is written for **Bun's bundler** and cannot be compiled/run as-is because it relies on build-time facilities that an external pipeline resolves:

- `import { feature } from 'bun:bundle'` — feature gates used for **dead-code elimination (DCE)** between internal ("Ant") and external builds. Any `feature('FLAG')` / `feature(...)` conditional `require()` block is stripped or kept wholesale at bundle time. Do not assume a gated branch runs; many flags are internal-only (e.g. `KAIROS`, `VOICE_MODE`, `TEAMMEM`, `COORDINATOR_MODE`, `BRIDGE_MODE`, `DUMP_SYSTEM_PROMPT`).
- `MACRO.VERSION` (and other `MACRO.*`) — build-time macros inlined by the bundler.

When editing, preserve these constructs verbatim — do not replace `feature(...)` with boolean constants or inline gated branches.

## Runtime stack

TypeScript + TSX (React), terminal UI via **Ink** (custom root in `src/ink.ts`), CLI parsing via `@commander-js/extra-typings`, schemas via **Zod v4** (`zod/v4`, with legacy `zod` still present), Anthropic SDK (`@anthropic-ai/sdk`), MCP SDK (`@modelcontextprotocol/sdk`). Runtime is Bun/Node ESM — **all imports carry explicit `.js` extensions** (including type-only imports) and use a `src/` path alias (e.g. `from 'src/services/api/claude.js'`).

## Entrypoints and startup flow

- `src/entrypoints/cli.tsx` — bootstrap. Fast-paths `--version` (zero imports), then dynamically imports the full CLI to keep cold paths cheap. Reads `feature()` gates and `process.env` at module top-level intentionally (some are captured into module-level consts at import time — see the `ABLATION_BASELINE` comment).
- `src/main.tsx` — the full CLI: Commander command/options tree, OAuth/config setup, then `launchRepl()`. Top of file runs perf-critical side-effects (startup profiler, MDM raw read, keychain prefetch) **before** other imports — order matters.
- `src/replLauncher.tsx` → `src/interactiveHelpers.tsx` — launches the Ink TUI (`src/components/App.tsx`).
- `src/entrypoints/init.ts` — global init + telemetry-after-trust.
- `src/entrypoints/mcp.ts` — runs Claude Code itself as an MCP server.
- `src/entrypoints/sdk/` — Agent SDK request/response/control schemas (`coreSchemas.ts`, `controlSchemas.ts`).

## The agentic loop (the core of the product)

- `src/QueryEngine.ts` — orchestrates a full agent turn: assembles the system prompt + message history, manages tool-use/tool-result pairing, calls `query()`, dispatches each `tool_use` to its `Tool`, streams SDK messages, handles compaction and retries. This is the layer above a single API call.
- `src/query.ts` — one streaming turn against the Anthropic Messages API (`services/api/claude.ts` → `client.ts`): token/compact tracking, error categorization, reactive-compact and context-collapse (both `feature()`-gated).
- `src/services/api/claude.ts` — the actual `messages.create` streaming call and usage/cost accumulation (`src/cost-tracker.ts`).

Data shape flows through centralized message types in `src/types/message.ts` (`UserMessage`, `AssistantMessage`, `ToolUseSummaryMessage`, `AttachmentMessage`, `SystemMessage`, etc.).

## Tools — the `Tool` interface and per-tool layout

The `Tool` abstraction (`src/Tool.ts`) is the central extension point. Each tool is a **directory** under `src/tools/<Name>Tool/` containing:

- `<Name>Tool.ts(x)` — implements the `Tool` interface (the logic).
- `prompt.ts` — the **model-facing** description injected into the system prompt.
- `UI.tsx` — the **terminal-facing** Ink rendering of the tool's progress/result.
- Supporting files (e.g. BashTool's `bashSecurity.ts`, `bashPermissions.ts`, `readOnlyValidation.ts`).

The `Tool<Input, Output, Progress>` interface (`src/Tool.ts:362`) key members:
- `name`, `aliases?`, `inputSchema` (Zod → converted to JSON Schema for the API), `inputJSONSchema?` (MCP tools), `outputSchema?`
- `call(args, context, canUseTool, parentMessage, onProgress)` — execution
- `prompt({ getToolPermissionContext, tools, agents })` — model-facing text
- `description(input, opts)` — human-facing summary
- Permission lifecycle: `validateInput()` → `checkPermissions()` → `useCanUseTool` hook → approval → `call()`. Tool-specific logic lives in `checkPermissions`; general logic lives in `src/utils/permissions/` (`bashClassifier`, `filesystem`, `dangerousPatterns`, `classifierDecision`, `getNextPermissionMode`, …).
- Read-only/concurrency/destructive hints: `isReadOnly`, `isConcurrencySafe`, `isDestructive`, `interruptBehavior`, `isSearchOrReadCommand`
- Deferred loading: `shouldDefer` (loaded via `ToolSearchTool` on demand) vs `alwaysLoad`
- `maxResultSizeChars` — large results persist to disk; the model gets a preview + path. `Infinity` opts out (e.g. Read).

Tools are registered in `src/tools.ts`. Ant-only tools are pulled in via conditional `require()` guarded by `feature()` (DCE'd from external builds). The `TestingPermissionTool`, `SyntheticOutputTool`, and `TungstenTool` are special/internal.

## Slash commands

`src/commands/` — each command is a directory (with `index.ts`) or a single `.ts`. All are imported and registered in `src/commands.ts`. Examples: `compact`, `clear`, `commit`, `diff`, `doctor`, `mcp`, `config`, `memory`, `resume`. Some have non-interactive variants (e.g. `context/index.ts` exports `context` + `contextNonInteractive`).

## Services (`src/services/`)

Cross-cutting subsystems, each its own dir: `api/` (Anthropic client, OAuth, bootstrap, files), `mcp/` (MCP server connections + approval), `analytics/` (events + GrowthBook flags), `compact/` (auto/reactive/micro compaction), `oauth/`, `plugins/`, `lsp/`, `policyLimits/`, `remoteManagedSettings/`, `settingsSync/`, `extractMemories/`, `SessionMemory/`, `tokenEstimation.ts`.

## TUI and state

- `src/components/` — Ink/React components (`App.tsx`, `design-system/`, diff viewers, dialogs, progress lines).
- `src/hooks/` — React hooks driving the TUI (`useCanUseTool`, `useCommandQueue`, `useCancelRequest`, `useApiKeyVerification`, …).
- `src/state/` — `AppState.tsx` (global app state), `AppStateStore.ts`, `store.ts`, `selectors.ts`. State changes flow through `onChangeAppState.ts`.
- `src/screens/`, `src/keybindings/`, `src/vim/` — modal screens, keybindings, vim mode.

## Other top-level areas

`memdir/` (the memory/MEMORY.md system), `skills/`, `plugins/`, `migrations/` (config/schema migrations), `native-ts/`, `coordinator/` + `bridge/` (multi-agent/remote coordination, feature-gated), `remote/` (remote session manager + WebSocket), `upstreamproxy/`, `voice/`, `buddy/`, `assistant/`.

## Non-obvious conventions to respect

- **Do not reorder imports.** Files begin with `// biome-ignore-all assist/source/organizeImports: ANT-ONLY import markers must not be reordered`. Import order is load-bearing for the bundler's DCE and side-effect ordering. Disable any auto-organize-imports action on these files.
- **Top-level side-effects are intentional.** Many are tagged with `eslint-disable custom-rules/no-top-level-side-effects` / `no-process-env-top-level` because they must run at import time (perf, ablation flags captured into consts). Don't refactor them into lazy init without checking callers.
- **Centralize types to break import cycles.** Shared types (permissions, tool progress, messages) live in `src/types/` and are *re-imported* from there even when closer sources exist — see the repeated "Import … from centralized location to break import cycles" comments. When adding a type used across tools/engine, put it in `src/types/`.
- **`.js` extensions everywhere**, including type-only imports and the `src/` alias.
- **`feature()` gates and `MACRO.*` are build-time**, not runtime — never replace with literals.
