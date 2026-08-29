import { appendStreamingTransition, appendThinkingTransition, ensureStreamingTransition, finishStreamingTransition, type ChatStreamState } from "./transitions.ts";

function assert(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(`thinking invariant failed: ${message}`);
}

const base: ChatStreamState = {
  messages: [],
  streamingIdx: null,
  nextMessageId: 1,
  model: "m",
  sessionId: "s",
  sessionRevision: 0,
  snapshotRevision: 0,
  awaitingSnapshot: false,
  sessions: [],
  hasMobileAccessToken: false,
  availableModels: [],
  taskChanges: [],
  traceEntries: [],
  inputTokens: 0,
  outputTokens: 0,
  cacheReadTokens: 0,
  cacheWriteTokens: 0,
  toolCards: {},
  subagentRunsById: {},
  childIdsByParentToolId: {},
  pendingPermission: null,
  pendingQuestions: [],
  pendingCommit: null,
  resolvedPermissionIds: [],
  resolvedQuestionIds: [],
  attachments: [],
  recording: false,
  outboundQueue: [],
  activeRunId: null,
  agentRunning: false,
  cancelling: false,
  compacting: false,
  multiRun: null,
} as unknown as ChatStreamState;

// Wire order: thinking_delta arrives BEFORE any text_delta (reasoning first).
let s = ensureStreamingTransition(base);
s = appendThinkingTransition(s, "Let me think ");
s = appendThinkingTransition(s, "about this.");
s = appendStreamingTransition(s, "The answer is 2.");
s = finishStreamingTransition(s);
const msg = s.messages[0];
assert(msg.role === "assistant", "streaming message is assistant");
assert(msg.thinking === "Let me think about this.", `thinking accumulated: ${msg.thinking}`);
assert(msg.content === "The answer is 2.", "content streams independently");
assert(msg.streaming !== true, "finish closes streaming");

// Session restore: thinking block maps onto the owning assistant message.
import { engineMessagesToChat } from "./slices.ts";
const restored = engineMessagesToChat([
  { role: "user", content: "q" },
  {
    role: "assistant",
    content: [
      { type: "thinking", thinking: "inner reasoning text" },
      { type: "text", text: "visible answer" },
    ],
  },
]);
const thinking = restored.find((m) => m.role === "assistant" && m.thinking);
assert(thinking !== undefined, "restore keeps thinking block");
assert(thinking!.thinking === "inner reasoning text", `restored thinking text: ${thinking!.thinking}`);
assert(thinking!.content === "visible answer", "restore keeps visible text");
assert(restored.filter((m) => m.role === "assistant").length === 1, "thinking merges into the visible text bubble, not a second bubble");

// tool-only turn: thinking attaches to its own assistant entry before tools
const toolTurn = engineMessagesToChat([
  { role: "user", content: "q2" },
  {
    role: "assistant",
    content: [
      { type: "thinking", thinking: "plan the grep" },
      { type: "tool_use", id: "t9", name: "Grep", input: { pattern: "x" } },
    ],
  },
  { role: "user", content: [{ type: "tool_result", tool_use_id: "t9", content: "ok" }] },
]);
const toolTurnThink = toolTurn.find((m) => m.role === "assistant" && m.thinking);
assert(toolTurnThink !== undefined, "tool-only turn keeps its thinking");
assert(toolTurnThink!.thinking === "plan the grep", `tool turn thinking: ${toolTurnThink?.thinking}`);

console.log("thinking flow invariants: all passed");
