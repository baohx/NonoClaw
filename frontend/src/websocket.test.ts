import { useStore } from "./store.ts";
import { dispatchServerMessage } from "./websocket.ts";
import type { ServerMsg } from "./types.ts";

function assert(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(`websocket invariant failed: ${message}`);
}

function resetStore(): void {
  useStore.setState(useStore.getInitialState(), true);
}

/** info + authoritative snapshot so the ordering guard has a session to match. */
function establishSession(): void {
  dispatchServerMessage({
    type: "info",
    model: "fixture-model",
    session_id: "session-1",
    available_models: [{ name: "fixture-model", label: "Fixture" }],
  } satisfies ServerMsg);
  dispatchServerMessage({
    type: "messages_loaded",
    protocol_version: 1,
    session_id: "session-1",
    revision: 1,
    messages: [],
  } satisfies ServerMsg);
}

/** One accepted text-delta event for run r1, sequence 1. */
function beginRun(): void {
  dispatchServerMessage({
    type: "event",
    protocol_version: 1,
    run_id: "run-1",
    session_id: "session-1",
    session_revision: 1,
    sequence: 1,
    event: { kind: "text_delta", text: "hello" },
  } satisfies ServerMsg);
}

function doneFrame(runId: string, sequence: number): ServerMsg {
  return {
    type: "done",
    protocol_version: 1,
    run_id: runId,
    session_id: "session-1",
    session_revision: 1,
    sequence,
    text: "hello",
    usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 },
    turns: 1,
    stop_reason: "end_turn",
  };
}

function stuckRunFlags(): void {
  const state = useStore.getState();
  state.setAgentRunning(true);
  state.setCompacting(true);
  state.setCancelling(true);
}

function assertRunCleared(context: string): void {
  const state = useStore.getState();
  assert(!state.agentRunning, `${context}: agentRunning must clear`);
  assert(!state.compacting, `${context}: compacting must clear`);
  assert(!state.cancelling, `${context}: cancelling must clear`);
}

// A terminal "done" for a run must always release the composer: the stop
// button shows exactly while `agentRunning || compacting` is set, so a done
// that is accepted must clear compacting too (a pre-fire auto-compact can
// have set it without ever emitting the paired "compacted" event — e.g. the
// run ends before the background compact resolves).
function acceptedDoneClearsStuckFlags(): void {
  resetStore();
  establishSession();
  beginRun();
  stuckRunFlags();
  dispatchServerMessage(doneFrame("run-1", 2));
  assertRunCleared("accepted done");
}

// Even a terminal frame the ordering guard rejects (duplicate/late done) must
// release the composer. This mirrors the run-scoped error path: the guard
// exists to deduplicate ordering bookkeeping, not to keep the UI on "stop".
function rejectedDoneStillClearsStuckFlags(): void {
  resetStore();
  establishSession();
  beginRun();
  stuckRunFlags();
  dispatchServerMessage(doneFrame("run-1", 2)); // accepted — marks run-1 terminal
  assertRunCleared("first done");
  // Re-arm the stuck state (as if a stale terminal arrives while a fresh run
  // is optimistically marked running) and replay the SAME done — the guard
  // rejects the replay (`sequence <= runSequences[run-1]` + terminalRuns).
  stuckRunFlags();
  dispatchServerMessage(doneFrame("run-1", 2)); // rejected replay
  assertRunCleared("rejected duplicate done");
}

// A run-scoped error frame must also clear the compacting indicator so the
// composer never stays locked after a failed run.
function runErrorClearsCompacting(): void {
  resetStore();
  establishSession();
  stuckRunFlags();
  dispatchServerMessage({
    type: "error",
    protocol_version: 1,
    run_id: "run-2",
    session_id: "session-1",
    session_revision: 1,
    sequence: 1,
    message: "provider request failed (HTTP 500)",
  } satisfies ServerMsg);
  assertRunCleared("run-scoped error");
}

function terminalFramesReleaseTheComposer(): void {
  acceptedDoneClearsStuckFlags();
  rejectedDoneStillClearsStuckFlags();
  runErrorClearsCompacting();
  console.log("✓ terminal frames always release the composer (agentRunning / compacting / cancelling)");
}

terminalFramesReleaseTheComposer();

/** F2/F4 regression: token_budget_breakdown populates xrayBudget, and
 *  usage_updated overwrites (not accumulates) the four token totals. */
function xrayAndUsageRealTime(): void {
  resetStore();
  establishSession();
  dispatchServerMessage({
    type: "event",
    protocol_version: 1,
    run_id: "run-x",
    session_id: "session-1",
    session_revision: 1,
    sequence: 1,
    event: {
      kind: "token_budget_breakdown",
      chars_per_token: 4,
      estimated_tokens: 30,
      system_chars: 80,
      tools_chars: 30,
      messages_chars: 10,
      system: [{ name: "base_prompt", chars: 80, estimated_tokens: 20 }],
      tools: [{ name: "builtin:read", chars: 30, estimated_tokens: 8 }],
      messages: [{ name: "history", chars: 10, estimated_tokens: 2 }],
    },
  } satisfies ServerMsg);
  const xray = useStore.getState().xrayBudget;
  assert(xray !== null && xray.kind === "token_budget_breakdown", "xrayBudget captures the breakdown");
  assert(Array.isArray(xray.system) && xray.system.length === 1, "xrayBudget keeps verbatim arrays");

  // usage_updated: cumulative total overwrites the counters.
  dispatchServerMessage({
    type: "event",
    protocol_version: 1,
    run_id: "run-x",
    session_id: "session-1",
    session_revision: 1,
    sequence: 2,
    event: {
      kind: "usage_updated",
      turn: 1,
      turn_usage: { input_tokens: 5, output_tokens: 5 },
      total: {
        input_tokens: 100, output_tokens: 200,
        cache_read_input_tokens: 50, cache_creation_input_tokens: 25,
      },
    },
  } satisfies ServerMsg);
  const st = useStore.getState();
  assert(st.inputTokens === 100, "usage_updated sets input total");
  assert(st.outputTokens === 200, "usage_updated sets output total");
  assert(st.cacheReadTokens === 50, "usage_updated sets cache read");
  assert(st.cacheWriteTokens === 25, "usage_updated sets cache write");

  // A second usage_updated must overwrite, not double.
  dispatchServerMessage({
    type: "event",
    protocol_version: 1,
    run_id: "run-x",
    session_id: "session-1",
    session_revision: 1,
    sequence: 3,
    event: {
      kind: "usage_updated",
      turn: 2,
      turn_usage: { input_tokens: 5, output_tokens: 5 },
      total: { input_tokens: 105, output_tokens: 205, cache_read_input_tokens: 55, cache_creation_input_tokens: 30 },
    },
  } satisfies ServerMsg);
  assert(useStore.getState().inputTokens === 105, "usage_updated overwrites (no double count)");
  console.log("✓ x-ray data + usage totals update in real time");
}

xrayAndUsageRealTime();

/** Tail-windowed restore + load_older paging invariants. */
function historyPaging(): void {
  resetStore();
  establishSession();
  // Restore a 600-message session as a 50-message tail window.
  dispatchServerMessage({
    type: "messages_loaded",
    protocol_version: 1,
    session_id: "session-1",
    revision: 2,
    messages: [{ role: "user", content: "recent" }],
    total: 600,
  } satisfies ServerMsg);
  let state = useStore.getState();
  assert(state.historyTotal === 600, "historyTotal set from snapshot total");
  assert(state.historyOlderRemaining === 599, "older remaining = total - window");
  assert(state.messages.length === 1, "tail window holds only the sent messages");

  // load_older sends before = window start (= remaining count).
  const sent: unknown[] = [];
  state.requestOlderHistory((msg) => sent.push(msg));
  assert(useStore.getState().historyLoading, "load in flight flag set");
  const req = sent[0] as { type: string; before?: number; limit?: number };
  assert(req.type === "load_older" && req.before === 599, `before = remaining (599), got ${req.before}`);
  assert(req.limit === 100, "default page size 100");

  // Page arrives: prepend + advance the boundary.
  dispatchServerMessage({
    type: "history_page",
    protocol_version: 1,
    session_id: "session-1",
    revision: 2,
    messages: [{ role: "user", content: "older" }],
    remaining: 499,
  } satisfies ServerMsg);
  state = useStore.getState();
  assert(!state.historyLoading, "loading cleared on page");
  assert(state.messages.length === 2, "page prepended");
  assert(state.messages[0].content === "older", "older message comes first");
  assert(state.historyOlderRemaining === 499, "boundary advanced to page start");

  // Cross-session pages are dropped.
  dispatchServerMessage({
    type: "history_page",
    protocol_version: 1,
    session_id: "other-session",
    revision: 9,
    messages: [{ role: "user", content: "stale" }],
    remaining: 0,
  } satisfies ServerMsg);
  assert(useStore.getState().messages.length === 2, "stale page dropped");

  // Exhausted: no request when nothing remains.
  const sent2: unknown[] = [];
  useStore.setState({ historyOlderRemaining: 0 });
  useStore.getState().requestOlderHistory((msg) => sent2.push(msg));
  assert(sent2.length === 0, "no load_older when exhausted");
  console.log("✓ tail-window restore + load_older paging invariants");
}

historyPaging();

/** Streaming deltas coalesce per frame but preserve text/thinking order. */
function deltaCoalescing(): void {
  resetStore();
  establishSession();
  const state = useStore.getState();
  state.setAgentRunning(true);
  const ev = (kind: string, text: string, seq: number): ServerMsg => ({
    type: "event",
    protocol_version: 1,
    run_id: "run-1",
    session_id: "session-1",
    session_revision: 1,
    sequence: seq,
    event: { kind, text } as never,
  });
  // Three text deltas + two thinking deltas interleaved.
  dispatchServerMessage(ev("text_delta", "a", 1));
  dispatchServerMessage(ev("text_delta", "b", 2));
  dispatchServerMessage(ev("thinking_delta", "t1", 3));
  dispatchServerMessage(ev("thinking_delta", "t2", 4));
  dispatchServerMessage(ev("text_delta", "c", 5));
  // rAF is unavailable in Node: the scheduler falls back to setTimeout(16).
  // Ordering guard: nothing flushed yet is fine; wait a tick and assert.
  setTimeout(() => {
    const st = useStore.getState();
    const streaming = st.messages[st.streamingIdx ?? -1];
    assert(!!streaming, "streaming message exists after flush");
    assert(streaming?.content === "abc", `text coalesced in order, got "${streaming?.content}"`);
    assert(streaming?.thinking === "t1t2", `thinking coalesced, got "${streaming?.thinking}"`);
    console.log("✓ streaming deltas coalesce per frame with order preserved");
  }, 40);
}

deltaCoalescing();
