import type { ClientMsg } from "../types.ts";
import { checkTraceStateInvariants } from "../trace.test.ts";
import {
  MAX_OUTBOUND_QUEUE,
  MAX_RESOLVED_PROMPTS,
  MAX_TRACKED_RUNS,
  acceptRunTransition,
  acceptSnapshotTransition,
  accumulateUsage,
  addToolCardTransition,
  enqueueClientMessage,
  prepareSessionBoundary,
  resolvePromptTransition,
  transitionConnection,
  type ChatStreamState,
  type ConnectionState,
  type SessionOrderingState,
} from "./transitions.ts";

function assert(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(`transition invariant failed: ${message}`);
}

function connection(): ConnectionState {
  return {
    connectionStatus: "disconnected",
    connectionGeneration: 0,
    outboundQueue: [],
    nextOutboundId: 1,
  };
}

function ordering(): SessionOrderingState {
  return {
    sessionId: "session-a",
    sessionRevision: -1,
    snapshotRevision: -1,
    awaitingSnapshot: false,
    runSequences: {},
    terminalRuns: {},
    runOrder: [],
  };
}

// Validates: Requirements 8.2, 10.8-10.9
function connectionAndQueueChecks(): void {
  let state = transitionConnection(connection(), { type: "begin" });
  const firstGeneration = state.connectionGeneration;
  state = transitionConnection(state, { type: "begin" });
  assert(state.connectionGeneration === firstGeneration + 1, "generations must be monotonic");
  const stale = transitionConnection(state, { type: "connected", generation: firstGeneration });
  assert(stale === state && stale.connectionStatus === "connecting", "stale sockets cannot become active");
  state = transitionConnection(state, { type: "connected", generation: state.connectionGeneration });
  assert(state.connectionStatus === "connected", "current generation should connect");

  const duplicate: ClientMsg = { type: "run", prompt: "same prompt" };
  let result = enqueueClientMessage(state, duplicate);
  state = result.state;
  const duplicateId = result.entry.id;
  result = enqueueClientMessage(state, duplicate);
  assert(!result.added && result.entry.id === duplicateId, "queued prompts must deduplicate");

  for (let index = 0; index < MAX_OUTBOUND_QUEUE * 3; index += 1) {
    state = enqueueClientMessage(state, { type: "run", prompt: `prompt-${index}` }).state;
    assert(state.outboundQueue.length <= MAX_OUTBOUND_QUEUE, "outbound retention must stay bounded");
  }
  assert(state.outboundQueue[state.outboundQueue.length - 1].message.type === "run", "queue must preserve FIFO tail");
}

// Validates: Requirements 2.4, 8.2-8.4
function snapshotAndRunOrderingChecks(): void {
  let state = ordering();
  let snapshot = acceptSnapshotTransition(state, "session-a", 2);
  assert(snapshot.accepted, "first authoritative snapshot should be accepted");
  state = snapshot.state;
  assert(!acceptSnapshotTransition(state, "session-a", 2).accepted, "duplicate snapshots must be rejected");
  assert(!acceptSnapshotTransition(state, "session-a", 1).accepted, "older snapshots must be rejected");

  let event = acceptRunTransition(state, {
    runId: "run-a", sessionId: "session-a", sessionRevision: 3, sequence: 0,
  }, false);
  assert(event.accepted, "sequence zero must be a valid first event");
  state = event.state;
  assert(!acceptSnapshotTransition(state, "session-a", 2).accepted, "snapshot older than observed run revision must be stale");
  snapshot = acceptSnapshotTransition(state, "session-a", 3);
  assert(snapshot.accepted, "authoritative snapshot at observed revision should reconcile optimism");
  state = snapshot.state;

  for (let sequence = 1; sequence <= 50; sequence += 1) {
    event = acceptRunTransition(state, {
      runId: "run-a", sessionId: "session-a", sessionRevision: 3, sequence,
    }, sequence === 50);
    assert(event.accepted, `increasing sequence ${sequence} should be accepted`);
    state = event.state;
    const replay = acceptRunTransition(state, {
      runId: "run-a", sessionId: "session-a", sessionRevision: 3, sequence,
    }, sequence === 50);
    assert(!replay.accepted, `sequence ${sequence} replay must be rejected`);
  }
  assert(!acceptRunTransition(state, {
    runId: "run-a", sessionId: "session-a", sessionRevision: 4, sequence: 51,
  }, false).accepted, "events after a terminal must be rejected");

  state = prepareSessionBoundary(state, "session-b");
  assert(state.awaitingSnapshot && state.sessionId === "session-b", "session switch must establish a snapshot barrier");
  assert(!acceptRunTransition(state, {
    runId: "old", sessionId: "session-b", sessionRevision: 0, sequence: 1,
  }, false).accepted, "events must not cross a session snapshot barrier");

  state = acceptSnapshotTransition(state, "session-b", 0).state;
  for (let index = 0; index < MAX_TRACKED_RUNS * 2; index += 1) {
    const next = acceptRunTransition(state, {
      runId: `run-${index}`, sessionId: "session-b", sessionRevision: index, sequence: 1,
    }, true);
    assert(next.accepted, `new run ${index} should be accepted`);
    state = next.state;
    assert(state.runOrder.length <= MAX_TRACKED_RUNS, "run ordering metadata must stay bounded");
  }
}

// Validates: Requirements 1.6, 8.2-8.4
function chatPromptAndUsageChecks(): void {
  let chat: ChatStreamState = { messages: [], streamingIdx: null, nextMessageId: 1 };
  chat = addToolCardTransition(chat, "tool-a", "Read", { path: "a" });
  const once = chat.messages.length;
  chat = addToolCardTransition(chat, "tool-a", "Read", { path: "a" });
  assert(chat.messages.length === once, "tool cards must be idempotent");

  let prompts = { resolvedPermissionIds: [] as string[], resolvedQuestionIds: [] as string[] };
  prompts = resolvePromptTransition(prompts, "permission", "permission-a");
  prompts = resolvePromptTransition(prompts, "permission", "permission-a");
  assert(prompts.resolvedPermissionIds.length === 1, "resolved permission prompts must deduplicate");
  for (let index = 0; index < MAX_RESOLVED_PROMPTS * 2; index += 1) {
    prompts = resolvePromptTransition(prompts, "question", `question-${index}`);
  }
  assert(prompts.resolvedQuestionIds.length === MAX_RESOLVED_PROMPTS, "resolved prompt retention must stay bounded");

  const usage = accumulateUsage({ inputTokens: 1, outputTokens: 2, cacheReadTokens: 3, cacheWriteTokens: 4 }, {
    input: 10, output: 20, cacheRead: 30, cacheWrite: 40,
  });
  assert(usage.inputTokens === 11 && usage.outputTokens === 22
    && usage.cacheReadTokens === 33 && usage.cacheWriteTokens === 44,
  "usage accumulation must be component-wise");
}

connectionAndQueueChecks();
snapshotAndRunOrderingChecks();
chatPromptAndUsageChecks();
checkTraceStateInvariants();
console.log("frontend transition checks passed");
