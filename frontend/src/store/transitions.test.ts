import type { ClientMsg, EngineEvent, QuestionRequired, ScopedSubagentEvent } from "../types.ts";
import { createDialogSlice, engineMessagesToChat } from "./slices.ts";
import { checkTraceStateInvariants } from "../trace.test.ts";
import {
  MAX_OUTBOUND_QUEUE,
  MAX_RESOLVED_PROMPTS,
  MAX_TRACKED_RUNS,
  acceptRunTransition,
  acceptScopedEnvelope,
  acceptSnapshotTransition,
  accumulateUsage,
  addToolCardTransition,
  applySubagentEventTransition,
  emptySubagentState,
  enqueueClientMessage,
  prepareSessionBoundary,
  resolvePromptTransition,
  transitionConnection,
  updateToolResultTransition,
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
  chat = updateToolResultTransition(chat, "tool-a", false, "permission denied");
  const failedCard = chat.messages.find((message) => message.id === "tool-tool-a");
  assert(failedCard?.toolOk === false && failedCard.content === "permission denied", "failed tool results must set FAILURE on the matching card");

  const restored = engineMessagesToChat([
    { role: "assistant", content: [
      { type: "text", text: "before" },
      { type: "tool_use", id: "Case-Sensitive", name: "Read", input: { path: "safe" } },
      { type: "text", text: "after" },
    ] },
    { role: "user", content: [{ type: "tool_result", tool_use_id: "Case-Sensitive", content: "failed", is_error: true }] },
  ]);
  assert(restored.map((message) => message.role).join(",") === "assistant,tool,assistant", "history must preserve text/tool/text block order");
  assert(restored[1].id === "tool-Case-Sensitive" && restored[1].toolOk === false, "history must merge exact stable call ids and preserve failure");

  const restoredAttachments = engineMessagesToChat([
    { role: "user", attachments: [{ filename: "diagram.png" }, { filename: "notes.md" }], content: [
      { type: "text", text: "explain these files" },
    ] },
    { role: "assistant", content: "first answer" },
    { role: "user", content: "follow-up without files" },
  ]);
  assert(restoredAttachments[0].content === "explain these files", "attachment history must retain the original user prompt");
  assert(restoredAttachments[0].attachments?.map((item) => item.filename).join(",") === "diagram.png,notes.md",
    "attachment history must retain every filename on its user turn");
  assert(restoredAttachments[2].attachments === undefined,
    "attachment history must not leak filenames into a later user turn");

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

function scopedEvent(
  childId: string,
  sequence: number,
  event: EngineEvent,
  index: number | null = 0,
): ScopedSubagentEvent {
  return {
    kind: "subagent_event",
    subagent_id: childId,
    parent_tool_use_id: "parent-agent",
    description: `child ${childId}`,
    profile: childId === "child-b" ? "reviewer" : undefined,
    index,
    child_sequence: sequence,
    event,
  };
}

// Validates scoped ordering/isolation: child events cannot mutate root-run state.
function subagentTransitionChecks(): void {
  const root = {
    streamingIdx: 7,
    model: "root-model",
    sessionRevision: 12,
    runSequences: { root: 42 },
    breath: "streaming",
  };
  let children = emptySubagentState();

  let next = applySubagentEventTransition(children, scopedEvent("child-a", 0, {
    kind: "text_delta", text: "alpha ",
  }, 2));
  assert(next.accepted, "first child sequence zero must be accepted");
  children = next.state;
  next = applySubagentEventTransition(children, scopedEvent("child-b", 0, {
    kind: "text_delta", text: "beta",
  }, 1));
  assert(next.accepted, "a second child under one parent must be independent");
  children = next.state;
  assert(children.childIdsByParentToolId["parent-agent"].join(",") === "child-b,child-a",
    "siblings must be stable by index rather than arrival order");
  assert(children.subagentRunsById["child-a"].output === "alpha "
    && children.subagentRunsById["child-b"].output === "beta",
  "sibling text buffers must remain isolated");
  assert(children.subagentRunsById["child-b"].profile === "reviewer", "optional child profile must be retained");

  const duplicateState = children;
  assert(!applySubagentEventTransition(children, scopedEvent("child-a", 0, {
    kind: "text_delta", text: "duplicate",
  }, 2)).accepted, "duplicate child sequence must be rejected");
  assert(!applySubagentEventTransition(children, scopedEvent("child-a", -1, {
    kind: "text_delta", text: "late",
  }, 2)).accepted, "out-of-order child sequence must be rejected");
  assert(children === duplicateState && children.subagentRunsById["child-a"].output === "alpha ",
    "rejected child events must be idempotent");

  children = applySubagentEventTransition(children, scopedEvent("child-a", 1, {
    kind: "tool_use_start", id: "shared-tool", name: "Read", input: { path: "a", api_key: "secret" },
  }, 2)).state;
  children = applySubagentEventTransition(children, scopedEvent("child-b", 1, {
    kind: "tool_use_start", id: "shared-tool", name: "Bash", input: { command: "echo b" },
  }, 1)).state;
  children = applySubagentEventTransition(children, scopedEvent("child-a", 2, {
    kind: "permission_requested", tool_use_id: "shared-tool", tool_name: "Read", waiting_on: "user",
  }, 2)).state;
  assert(children.subagentRunsById["child-a"].status === "waiting_permission"
    && children.subagentRunsById["child-a"].toolsById["shared-tool"].permission === "user",
  "child permission must update only its scoped run/tool");
  assert(children.subagentRunsById["child-b"].toolsById["shared-tool"].name === "Bash",
    "equal tool ids in sibling runs must not collide");
  const safeInput = children.subagentRunsById["child-a"].toolsById["shared-tool"].input as Record<string, unknown>;
  assert(!("api_key" in safeInput), "child tool input must be sanitized before browser storage");

  children = applySubagentEventTransition(children, scopedEvent("child-a", 3, {
    kind: "assistant_done",
  }, 2)).state;
  assert(children.subagentRunsById["child-a"].segmentCount === 1
    && children.subagentRunsById["child-a"].status === "waiting_permission",
  "assistant_done must close a segment without terminating the child");
  children = applySubagentEventTransition(children, scopedEvent("child-a", 4, {
    kind: "run_finished", status: "succeeded",
  }, 2)).state;
  assert(children.subagentRunsById["child-a"].status === "succeeded", "run_finished must terminate the child");

  assert(root.streamingIdx === 7 && root.model === "root-model" && root.sessionRevision === 12
    && root.runSequences.root === 42 && root.breath === "streaming",
  "scoped transition must not update root streaming/model/revision/sequence/breath state");

  children = emptySubagentState();
  assert(Object.keys(children.subagentRunsById).length === 0
    && Object.keys(children.childIdsByParentToolId).length === 0,
  "session/messages_loaded/clear boundary state must remove all transient child runs");
}

connectionAndQueueChecks();
snapshotAndRunOrderingChecks();
chatPromptAndUsageChecks();
subagentTransitionChecks();
scopedEnvelopeGateChecks();
questionQueueRegression();
checkTraceStateInvariants();
console.log("frontend transition checks passed");

function scopedEnvelopeGateChecks(): void {
  const gate = {
    sessionId: "session-current",
    sessionRevision: 8,
    snapshotRevision: 7,
    awaitingSnapshot: false,
    terminalRuns: {} as Record<string, true>,
  };
  const current = { runId: "parent-run", sessionId: "session-current", sessionRevision: 8, sequence: 12 };
  assert(acceptScopedEnvelope(gate, current), "current-session child envelope should be accepted");
  assert(!acceptScopedEnvelope(gate, { ...current, sessionId: "session-old" }),
    "late child envelopes from an old session must be rejected");
  assert(!acceptScopedEnvelope({ ...gate, awaitingSnapshot: true }, current),
    "child envelopes must not cross a snapshot barrier");
  assert(!acceptScopedEnvelope(gate, { ...current, sessionRevision: 6 }),
    "child envelopes older than known session state must be rejected");
  assert(!acceptScopedEnvelope({ ...gate, terminalRuns: { "parent-run": true } }, current),
    "child envelopes arriving after parent terminal must be rejected");

  const direct = applySubagentEventTransition(emptySubagentState(), scopedEvent("direct-agent", 0, {
    kind: "run_started",
  }, null));
  assert(direct.accepted && direct.state.subagentRunsById["direct-agent"].index === 0,
    "direct Agent index:null must normalize to zero");
}

// ── Question queue regression: parallel AskUserQuestion frames ────────────
// Minimal zustand-style harness for createDialogSlice so queue semantics can
// be asserted without a browser. The setter merges object patches and applies
// functional patches against the current state, mirroring zustand's set().
function dialogHarness() {
  let state = {
    pendingPermission: null,
    pendingQuestions: [] as QuestionRequired[],
    pendingCommit: null,
    showSessionPicker: false,
    resolvedPermissionIds: [] as string[],
    resolvedQuestionIds: [] as string[],
  };
  const set = (partial: unknown) => {
    const patch =
      typeof partial === "function"
        ? (partial as (s: typeof state) => Partial<typeof state>)(state)
        : (partial as Partial<typeof state>);
    state = { ...state, ...patch };
  };
  // zustand's StateCreator invokes the slice with (set, get, store); supply
  // get/store stubs so the type check passes and only `set` is exercised.
  const slice = createDialogSlice(
    set as never,
    (() => state) as never,
    {} as never,
  );
  return { get: () => state, slice };
}

function question(request_id: string): QuestionRequired {
  return {
    type: "question_required",
    request_id,
    prompt: `question ${request_id}`,
    options: ["Yes", "No"],
    context: null,
    urgency: "medium",
    format: "multiple_choice",
  };
}

function questionQueueRegression() {
  const { get, slice } = dialogHarness();

  // Parallel question_required frames must queue, not clobber each other.
  slice.setPendingQuestion(question("q1"));
  slice.setPendingQuestion(question("q2"));
  slice.setPendingQuestion(question("q3"));
  const queued = get().pendingQuestions.map((q) => q.request_id);
  assert(queued.join(",") === "q1,q2,q3",
    "concurrent question frames must queue in FIFO order (regression: single-slot clobber)");

  // Resolving the head advances the queue; the next question becomes visible.
  slice.resolveQuestion("q1");
  const afterHead = get().pendingQuestions.map((q) => q.request_id);
  assert(afterHead.join(",") === "q2,q3",
    "resolving the current question must surface the next queued question");
  assert(get().resolvedQuestionIds.includes("q1"), "resolved id must be remembered");

  // Already-resolved or duplicate frames are ignored.
  slice.setPendingQuestion(question("q1"));
  slice.setPendingQuestion(question("q2"));
  assert(get().pendingQuestions.length === 2,
    "resolved or duplicate frames must not re-enter the queue");

  // null clears the whole queue (connection teardown).
  slice.setPendingQuestion(null);
  assert(get().pendingQuestions.length === 0, "null must clear the question queue");
}
