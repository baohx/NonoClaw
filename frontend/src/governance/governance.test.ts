import { analyzeTrajectory } from "./index.ts";
import { detectGoalDrift, detectInvalidRetry, detectLoopDeadlock } from "./anomalies.ts";
import { argumentsSimilarity, embed, textSimilarity, toolCallSimilarity, vectorCosine } from "./similarity.ts";
import { buildTrajectoryTree } from "./tree.ts";
import { DEFAULT_CONFIG, type GovernanceConfig, type ToolCallRecord, type TurnRecord } from "./types.ts";
import type { ChatMessage } from "../types.ts";

function check(condition: boolean, message: string): void {
  if (!condition) throw new Error(`governance invariant failed: ${message}`);
}

function tool(
  nodeId: string,
  name: string,
  input: unknown,
  ok: boolean,
  result: string,
  timestamp = 0,
): ToolCallRecord {
  return { nodeId, name, input, ok, result, timestamp };
}

// ── similarity primitives ──────────────────────────────────────────────────
check(textSimilarity("hello world", "hello world") === 1, "identical text must be similarity 1");
check(textSimilarity("hello", "completely unrelated phrase here") < 0.4, "unrelated text must be low similarity");
check(argumentsSimilarity({ a: 1, b: "x" }, { b: "x", a: 1 }) === 1, "argument similarity must be key-order-insensitive");
check(toolCallSimilarity({ name: "Read", input: { p: 1 } }, { name: "Read", input: { p: 1 } }) === 1, "identical tool calls must be similarity 1");
check(toolCallSimilarity({ name: "Read", input: {} }, { name: "Write", input: {} }) < 0.6, "different tool names must lower similarity");
check(Math.abs(vectorCosine(embed("hello"), embed("hello")) - 1) < 1e-6, "self embedding cosine must be 1");

// ── loop deadlock: same call + unchanged result = deadlock ────────────────
const deadlockCalls: ToolCallRecord[] = [
  tool("t1", "Read", { file_path: "a.txt" }, true, "value=1"),
  tool("t2", "Read", { file_path: "a.txt" }, true, "value=1"),
  tool("t3", "Read", { file_path: "a.txt" }, true, "value=1"),
  tool("t4", "Read", { file_path: "a.txt" }, true, "value=1"),
  tool("t5", "Read", { file_path: "a.txt" }, true, "value=1"),
];
const loops = detectLoopDeadlock(deadlockCalls, DEFAULT_CONFIG);
check(loops.length === 1, "5 identical unchanged calls must produce one loop-deadlock anomaly");
check(loops[0].nodeIds.length >= 5, "loop anomaly must span the full window");

// ── iterative refinement (result changes) must NOT be flagged ─────────────
const progressiveCalls: ToolCallRecord[] = [
  tool("t1", "Read", { file_path: "a.txt" }, true, "value=1"),
  tool("t2", "Read", { file_path: "a.txt" }, true, "value=2"),
  tool("t3", "Read", { file_path: "a.txt" }, true, "value=3"),
  tool("t4", "Read", { file_path: "a.txt" }, true, "value=4"),
  tool("t5", "Read", { file_path: "a.txt" }, true, "value=5"),
];
check(detectLoopDeadlock(progressiveCalls, DEFAULT_CONFIG).length === 0, "progressive results must not be flagged as deadlock");

// ── invalid retry: repeated identical failure ─────────────────────────────
const retryCalls: ToolCallRecord[] = [
  tool("t1", "Bash", { command: "make build" }, false, "error: missing dependency"),
  tool("t2", "Bash", { command: "make build" }, false, "error: missing dependency"),
  tool("t3", "Bash", { command: "make build" }, false, "error: missing dependency"),
  tool("t4", "Bash", { command: "make build" }, false, "error: missing dependency"),
];
check(detectInvalidRetry(retryCalls, DEFAULT_CONFIG).length === 1, "4 identical failures must produce one invalid-retry anomaly");

// ── goal drift: sampled intents far from the baseline ─────────────────────
const baseline = "Build a REST API for managing todo items with CRUD endpoints";
const driftedTurns: TurnRecord[] = [
  { turn: 1, text: "I will design the REST API endpoints and routes for todos", inputTokens: 0, outputTokens: 0, timestamp: 1 },
  { turn: 6, text: "Let me now discuss the history of the Byzantine Empire", inputTokens: 0, outputTokens: 0, timestamp: 2 },
  { turn: 11, text: "The fall of Constantinople happened in 1453", inputTokens: 0, outputTokens: 0, timestamp: 3 },
];
const drifts = detectGoalDrift(driftedTurns, baseline, DEFAULT_CONFIG);
check(drifts.length >= 1, "off-task assistant intents must produce a goal-drift anomaly");

const onTaskTurns: TurnRecord[] = [
  { turn: 1, text: "Designing the REST API endpoints for todos", inputTokens: 0, outputTokens: 0, timestamp: 1 },
  { turn: 6, text: "Adding CRUD routes and request validation for todos", inputTokens: 0, outputTokens: 0, timestamp: 2 },
  { turn: 11, text: "Testing the todo API endpoints end to end", inputTokens: 0, outputTokens: 0, timestamp: 3 },
];
check(detectGoalDrift(onTaskTurns, baseline, DEFAULT_CONFIG).length === 0, "on-task turns must not be flagged as drift");

// ── tree building: linear chain + subagent attachment ─────────────────────
function msg(id: string, role: ChatMessage["role"], content: string, extra: Partial<ChatMessage> = {}): ChatMessage {
  return { id, role, content, ...extra };
}

const treeMessages: ChatMessage[] = [
  msg("user-1", "user", baseline),
  msg("assistant-1", "assistant", "I will build the API", { timestamp: 10 }),
  msg("tool-1", "tool", "listed files", { toolName: "Bash", toolInput: { command: "ls" }, toolOk: true }),
  msg("tool-2", "tool", "delegated to subagent", { toolName: "Agent", toolInput: { prompt: "write tests" }, toolOk: true }),
];

const subagentRuns = {
  "child-1": {
    id: "child-1",
    parentToolUseId: "2",
    description: "Write tests",
    profile: "coder",
    index: 0,
    childSequence: 0,
    status: "succeeded" as const,
    output: "done",
    outputTruncated: false,
    segmentCount: 1,
    toolsById: {
      "st1": { id: "st1", name: "Read", input: { file_path: "x" }, result: "ok", ok: true, status: "succeeded", truncated: false },
    },
    toolOrder: ["st1"],
  },
};
const childIds = { "2": ["child-1"] };

const tree = buildTrajectoryTree({ messages: treeMessages, subagentRunsById: subagentRuns, childIdsByParentToolId: childIds });
check(tree.root.children.length === 1, "tree must have one top-level turn branch");
check(tree.linearToolCalls.length === 3, "tree must flatten main-chain (2) + subagent (1) tool calls");
const agentNode = tree.root.children[0].children.find((n) => n.toolName === "Agent");
check(agentNode !== undefined, "main-chain tool call must be a tree node");
check(agentNode!.children.length === 1 && agentNode!.children[0].kind === "subagent", "subagent must attach to the parent Agent tool call");
check(tree.userBaseline === baseline, "first user message must be the drift baseline");

// ── integration: analyzeTrajectory end-to-end ─────────────────────────────
const report = analyzeTrajectory({
  messages: treeMessages,
  subagentRunsById: subagentRuns,
  childIdsByParentToolId: childIds,
});
check(report.totals.toolCalls === 3, "report must count flattened tool calls");
check(report.totals.subagents === 1, "report must count subagents");
check(report.tree.turns.length === 1, "report must count assistant turns");

// Custom low thresholds: prove the config actually gates detection.
const strictConfig: GovernanceConfig = {
  ...DEFAULT_CONFIG,
  loop: { ...DEFAULT_CONFIG.loop, callSimMin: 1.01, resultSimMin: 1.01 },
};
check(detectLoopDeadlock(deadlockCalls, strictConfig).length === 0, "impossible thresholds must suppress loop detection");

// ── restored-session path: engineMessagesToChat → governance ──────────────
// A session restored from JSONL (websocket session_loaded) produces Chat
// messages through engineMessagesToChat — governance must work on that shape.
const { engineMessagesToChat } = await import("../store/slices.ts");
const restored = engineMessagesToChat([
  { role: "user", content: "Fix the failing build in rust/crates/api" },
  { role: "assistant", content: [
    { type: "text", text: "Investigating the build failure now" },
    { type: "tool_use", id: "tu-1", name: "Bash", input: { command: "cargo build" } },
  ] },
  { role: "user", content: [
    { type: "tool_result", tool_use_id: "tu-1", content: "error: unresolved import", is_error: true },
  ] },
  { role: "assistant", content: [
    { type: "text", text: "Retrying the build" },
    { type: "tool_use", id: "tu-2", name: "Bash", input: { command: "cargo build" } },
  ] },
  { role: "user", content: [
    { type: "tool_result", tool_use_id: "tu-2", content: "error: unresolved import", is_error: true },
  ] },
  { role: "assistant", content: [
    { type: "text", text: "Trying again" },
    { type: "tool_use", id: "tu-3", name: "Bash", input: { command: "cargo build" } },
  ] },
  { role: "user", content: [
    { type: "tool_result", tool_use_id: "tu-3", content: "error: unresolved import", is_error: true },
  ] },
  { role: "assistant", content: [
    { type: "text", text: "One more time" },
    { type: "tool_use", id: "tu-4", name: "Bash", input: { command: "cargo build" } },
  ] },
  { role: "user", content: [
    { type: "tool_result", tool_use_id: "tu-4", content: "error: unresolved import", is_error: true },
  ] },
  { role: "assistant", content: [{ type: "text", text: "Still investigating the build" }] },
]);
check(restored.filter((m) => m.role === "tool").length === 4, "restored path must produce 4 tool messages");
const restoredReport = analyzeTrajectory({ messages: restored, subagentRunsById: {}, childIdsByParentToolId: {} });
check(restoredReport.totals.toolCalls === 4, "restored tool calls must reach governance");
const retry = restoredReport.anomalies.find((a) => a.type === "invalid_retry");
check(retry !== undefined, "restored identical failures must be detected as invalid_retry");
check(retry!.nodeIds.length === 4, "retry anomaly must cover exactly the failed calls");

console.log("governance invariants: all passed");
