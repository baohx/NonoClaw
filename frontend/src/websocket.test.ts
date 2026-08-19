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
