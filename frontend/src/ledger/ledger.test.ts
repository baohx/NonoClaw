/**
 * Trajectory ledger invariants — ported contracts from DSH
 * `virtual-rows.client.spec.ts` + NonoClaw data-shape integration tests.
 */

import { buildLedgerLayout } from "./layout.ts";
import { deriveTrajectoryTimeline, timelineSelectionForRange, IDLE_COMPRESS_SECONDS } from "./timeline.ts";
import { TrajectorySearchIndex } from "./search.ts";
import { projectLedgerRows, visibleLedgerRows } from "./virtual-rows.ts";
import { formatDurationMillis, formatElapsedSeconds, formatTokenCount, ledgerRecordId } from "./types.ts";
import type { ChatMessage, SubagentRun } from "../types.ts";
import type { TraceEntry } from "../trace.ts";

function check(condition: boolean, message: string): void {
  if (!condition) throw new Error(`ledger invariant failed: ${message}`);
}

// ── formatting contracts (DSH trajectory-record.ts) ───────────────────────
check(formatDurationMillis(null) === "—", "null duration formats as em dash");
check(formatDurationMillis(1234) === "1,234 ms", "duration gains thousands separators");
check(formatDurationMillis(89) === "89 ms", "short durations stay integer ms");
check(formatElapsedSeconds(2) === "2,000 ms", "seconds convert to ms label");
check(formatTokenCount(950) === "950", "sub-1k tokens stay raw");
check(formatTokenCount(1500) === "1.5k", "1.5k formatting");
check(formatTokenCount(24000) === "24k", "24k formatting");
check(formatTokenCount(undefined) === "—", "missing tokens format as em dash");

// ── record identity (DSH trajectoryRecordId precedence) ───────────────────
check(
  ledgerRecordId({ index: 1, kind: "user", text: "x", timeSeconds: null, recordId: "r1" }) === "r1",
  "explicit recordId wins",
);
check(
  ledgerRecordId({ index: 2, kind: "tool", text: "x", timeSeconds: null, callId: "call-9" }).startsWith("tool\u0000call\u0000call-9"),
  "callId is second precedence",
);
check(
  ledgerRecordId({ index: 3, kind: "assistant", text: "x", timeSeconds: null, sourceSeq: 41 }).includes("41"),
  "sourceSeq is third precedence",
);

// ── layout: turns open at user prompts ────────────────────────────────────
const now = 1_700_000_000_000;
const baseMessages: ChatMessage[] = [
  { id: "u1", role: "user", content: "fix the failing test", timestamp: now },
  { id: "a1", role: "assistant", content: "I'll run the tests first.", timestamp: now + 1_000, streaming: false },
  { id: "tool-t1", role: "tool", content: "test result: ok", toolName: "Bash", toolInput: { command: "cargo test" }, toolOk: true, timestamp: now + 2_000 },
  { id: "a2", role: "assistant", content: "Tests pass. Done.", timestamp: now + 3_000, streaming: false },
  { id: "u2", role: "user", content: "now run the linter", timestamp: now + 10_000 },
  { id: "a3", role: "assistant", content: "Lint clean.", timestamp: now + 11_000, streaming: false },
];
const baseEntries: TraceEntry[] = [
  { id: "e1", runId: "r", sessionId: "s", sequence: 1, timestampMs: now + 500, kind: "model_request_started", summary: "req", details: { turn: 1, provider: "anthropic" }, category: "model", status: "active" },
  { id: "e2", runId: "r", sessionId: "s", sequence: 2, timestampMs: now + 900, kind: "stream_state_changed", summary: "stream", details: { state: "streaming", turn: 1 }, category: "model", status: "active" },
  { id: "e3", runId: "r", sessionId: "s", sequence: 3, timestampMs: now + 1_900, kind: "tool_execution_started", summary: "s", details: { tool_use_id: "t1", tool_name: "Bash" }, category: "tool", status: "active" },
  { id: "e4", runId: "r", sessionId: "s", sequence: 4, timestampMs: now + 2_950, kind: "tool_execution_finished", summary: "f", details: { tool_use_id: "t1", tool_name: "Bash", status: "succeeded", elapsed_ms: 1_050 }, category: "tool", status: "success" },
  { id: "e5", runId: "r", sessionId: "s", sequence: 5, timestampMs: now + 3_400, kind: "usage_updated", summary: "u", details: { turn: 1, turn_usage: { input_tokens: 100, output_tokens: 40, cache_read_input_tokens: 12_000, cache_creation_input_tokens: 300 } as unknown as import("../trace.ts").TraceDetail }, category: "usage", status: "info" },
];
const layout = buildLedgerLayout({ messages: baseMessages, traceEntries: baseEntries, subagentRunsById: {} });
check(layout.turns.length === 2, "two user prompts produce two turns");
check(layout.turns[0].cells[0].kind === "user" && layout.turns[0].cells[0].opensTurn === true, "first cell of a turn is the opening user record");
check(layout.turns[0].cells.some((c) => c.kind === "tool" && c.callId === "t1" && c.timeSeconds === 1.05), "tool duration derives from execution pair elapsed_ms");
check(layout.turns[0].usage !== null && layout.turns[0].usage!.cacheRead === 12_000, "turn usage attaches from usage_updated");
check(layout.turns[0].request?.provider === "anthropic", "request provenance attaches provider");
const assistantCell = layout.turns[0].cells.find((c) => c.kind === "assistant");
check(assistantCell !== undefined && assistantCell.assistantMetrics !== undefined, "assistant metrics recorded when timing events exist");
check(assistantCell!.assistantMetrics!.firstTokenTime === now + 900, "TTFT anchor uses first streaming event");
check(assistantCell!.timeSeconds !== null, "assistant duration measured from step start to completion");

// ── multi-engine-turn ledger turn: assistant→tool→assistant in ONE turn ──
// Regression: pairing by ledger-turn index mapped every assistant message to
// trace turn n+1, so the second step of a turn got the first step's timing
// and cross-turn usage bled. Windows must pair by timestamp instead.
const multiMessages: ChatMessage[] = [
  { id: "mu1", role: "user", content: "one prompt, two engine turns", timestamp: now },
  { id: "ma1", role: "assistant", content: "Running the tool.", timestamp: now + 1_000, streaming: false },
  { id: "tool-mt1", role: "tool", content: "ok", toolName: "Bash", toolInput: { command: "ls" }, toolOk: true, timestamp: now + 2_500 },
  { id: "ma2", role: "assistant", content: "Tool finished, done.", timestamp: now + 4_000, streaming: false },
];
const multiEntries: TraceEntry[] = [
  { id: "m1", runId: "r", sessionId: "s", sequence: 1, timestampMs: now + 500, kind: "model_request_started", summary: "req", details: { turn: 1, provider: "anthropic" }, category: "model", status: "active" },
  { id: "m2", runId: "r", sessionId: "s", sequence: 2, timestampMs: now + 800, kind: "stream_state_changed", summary: "stream", details: { state: "streaming", turn: 1 }, category: "model", status: "active" },
  { id: "m3", runId: "r", sessionId: "s", sequence: 3, timestampMs: now + 2_400, kind: "tool_use_start", summary: "s", details: { id: "mt1" }, category: "tool", status: "active" },
  { id: "m4", runId: "r", sessionId: "s", sequence: 4, timestampMs: now + 3_000, kind: "model_request_started", summary: "req", details: { turn: 2, provider: "anthropic" }, category: "model", status: "active" },
  { id: "m5", runId: "r", sessionId: "s", sequence: 5, timestampMs: now + 3_800, kind: "stream_state_changed", summary: "stream", details: { state: "streaming", turn: 2 }, category: "model", status: "active" },
  { id: "m6", runId: "r", sessionId: "s", sequence: 6, timestampMs: now + 4_900, kind: "usage_updated", summary: "u", details: { turn: 2, turn_input: 200, turn_output: 30, turn_cache_read: 5_000, turn_cache_write: 0 }, category: "usage", status: "info" },
];
const multiLayout = buildLedgerLayout({ messages: multiMessages, traceEntries: multiEntries, subagentRunsById: {} });
check(multiLayout.turns.length === 1, "single user prompt stays one ledger turn");
const multiAssistants = multiLayout.turns[0].cells.filter((c) => c.kind === "assistant");
check(multiAssistants.length === 2, "both assistant steps render as records");
check(multiAssistants[0].assistantMetrics?.firstTokenTime === now + 800, "first assistant pairs with first step window");
check(multiAssistants[1].assistantMetrics?.firstTokenTime === now + 3_800, "second assistant pairs with SECOND step window (timestamp pairing)");
check(multiAssistants[0].timeSeconds !== null && multiAssistants[0].timeSeconds! > 0 && multiAssistants[0].timeSeconds! < 2, "first assistant duration ends at its tool batch start, not the later step");
check(multiLayout.turns[0].usage !== null && multiLayout.turns[0].usage!.input === 200, "turn usage sums only windows inside the turn");

// flattened usage keys (usageDetails() output) resolve too
const flatEntries: TraceEntry[] = [...baseEntries,
  { id: "e10", runId: "r", sessionId: "s", sequence: 10, timestampMs: now + 10_200, kind: "model_request_started", summary: "req", details: { turn: 2, provider: "anthropic" }, category: "model", status: "active" },
  { id: "e6", runId: "r", sessionId: "s", sequence: 6, timestampMs: now + 11_500, kind: "usage_updated", summary: "u", details: { turn: 2, turn_input: 500, turn_output: 60, turn_cache_read: 20_000, turn_cache_write: 0 }, category: "usage", status: "info" },
];
const layoutFlat = buildLedgerLayout({ messages: baseMessages, traceEntries: flatEntries, subagentRunsById: {} });
check(layoutFlat.turns[1].usage !== null && layoutFlat.turns[1].usage!.input === 500 && layoutFlat.turns[1].usage!.cacheRead === 20_000, "flattened turn_* detail keys attach turn usage");

// ── subagent branch records ride under parent turn ────────────────────────
const subagentRun: SubagentRun = {
  id: "sa1",
  parentToolUseId: "t1",
  description: "scan files",
  profile: "explorer",
  index: 0,
  childSequence: 1,
  status: "succeeded",
  output: "done",
  outputTruncated: false,
  segmentCount: 1,
  toolsById: {
    st1: { id: "st1", name: "Grep", input: { pattern: "x" }, result: "2 matches", ok: true, status: "succeeded", truncated: false },
  },
  toolOrder: ["st1"],
};
const layoutSub = buildLedgerLayout({ messages: baseMessages, traceEntries: baseEntries, subagentRunsById: { sa1: subagentRun } });
const subCell = layoutSub.turns[0].cells.find((c) => c.subagentRunId === "sa1");
check(subCell !== undefined && subCell.callId === "st1", "subagent tool records attach to the parent's turn");
check(layoutSub.turns.length === 2, "subagent records do not open new turns");

// ── compaction pseudo-turn (Between turns) ────────────────────────────────
const compactEntries: TraceEntry[] = [...baseEntries,
  { id: "e9", runId: "r", sessionId: "s", sequence: 9, timestampMs: now + 20_000, kind: "compaction_started", summary: "c", details: { automatic: true, tokens_before: 150_000 }, category: "context", status: "active" },
];
const layoutCompact = buildLedgerLayout({ messages: baseMessages, traceEntries: compactEntries, subagentRunsById: {} });
check(layoutCompact.turns.some((t) => t.n === -1), "standalone compaction lands in the Between turns pseudo-turn");
check(layoutCompact.turns.some((t) => t.cells.some((c) => c.kind === "compacted" && c.text.includes("150,000"))), "compaction text carries tokens_before");

// ── running (in-flight) tool call ─────────────────────────────────────────
const runningEntries: TraceEntry[] = [...baseEntries,
  { id: "e8", runId: "r", sessionId: "s", sequence: 8, timestampMs: now + 30_000, kind: "tool_execution_started", summary: "s", details: { tool_use_id: "live1", tool_name: "Bash" }, category: "tool", status: "active" },
];
const layoutLive = buildLedgerLayout({ messages: baseMessages, traceEntries: runningEntries, subagentRunsById: {} });
check(layoutLive.turns.some((t) => t.cells.some((c) => c.kind === "tool" && c.text.includes("(running)"))), "in-flight tool without a tool message emits a running record");
check(layoutLive.turns.every((t) => !t.cells.some((c) => c.requestOnly === true && c.kind === "system")), "turns with visible records emit no request-only separator");

// ── virtual rows (DSH virtual-rows spec contracts) ────────────────────────
const projection = projectLedgerRows(layoutSub.turns);
check(projection.rows[0].kind === "turn-header", "projection starts with a turn header");
check(projection.rows.some((r) => r.kind === "turn-divider"), "turns end with a boundary divider");
check(projection.rows.filter((r) => r.kind === "record").length === projection.cells.length, "record rows match flattened cells");
check(projection.totalHeight === projection.rows.reduce((s, r) => s + r.height, 0), "total height is the row height sum");
const win = visibleLedgerRows(projection, 0, 60);
check(win.rows.length > 0 && win.offsetY === 0, "top window starts at offset 0");
const winTail = visibleLedgerRows(projection, projection.totalHeight - 10, 60);
check(winTail.rows.length > 0, "tail window still renders rows");
const betweenProjection = projectLedgerRows(layoutCompact.turns.filter((t) => t.n === -1));
check(betweenProjection.rows[0].kind === "between-header", "between-turns group renders its own header");

// ── timeline (four modes, drag selection, idle compression) ───────────────
const seqTimeline = deriveTrajectoryTimeline(layoutSub.turns, "sequence");
check(seqTimeline !== null && seqTimeline.mode === "sequence", "sequence timeline derives");
check(seqTimeline!.spans.every((s) => s.end > s.start || s.end === s.start), "sequence spans are monotonic");
const durTimeline = deriveTrajectoryTimeline(layoutSub.turns, "duration");
check(durTimeline !== null && durTimeline.domain[0] === 0, "duration domain starts at 0");
const timeTimeline = deriveTrajectoryTimeline(layoutSub.turns, "time");
check(timeTimeline !== null && timeTimeline.isTimeDomain, "time mode uses wall-clock domain");
const actualTimeline = deriveTrajectoryTimeline(layoutSub.turns, "actual");
check(actualTimeline !== null, "actual mode derives");
const sel = timelineSelectionForRange(timeTimeline, { start: 0, end: 1 });
check(sel.size === timeTimeline!.spans.length, "full-range selection covers every span");
const selNarrow = timelineSelectionForRange(timeTimeline, { start: 0, end: 0.0001 });
check(selNarrow.size < timeTimeline!.spans.length, "narrow selection focuses fewer records");
// idle compression: append a far-future record pair and re-derive
const idleEntries: TraceEntry[] = [...baseEntries,
  { id: "e7", runId: "r", sessionId: "s", sequence: 7, timestampMs: now + 10 * IDLE_COMPRESS_SECONDS * 1000, kind: "tool_execution_started", summary: "s", details: { tool_use_id: "late", tool_name: "Read" }, category: "tool", status: "active" },
  { id: "e8", runId: "r", sessionId: "s", sequence: 8, timestampMs: now + 10 * IDLE_COMPRESS_SECONDS * 1000 + 100, kind: "tool_execution_finished", summary: "f", details: { tool_use_id: "late", tool_name: "Read", status: "succeeded", elapsed_ms: 100 }, category: "tool", status: "success" },
];
const layoutIdle = buildLedgerLayout({ messages: baseMessages, traceEntries: idleEntries, subagentRunsById: {} });
const idleTimeline = deriveTrajectoryTimeline(layoutIdle.turns, "time");
check(idleTimeline !== null && idleTimeline.idleBreaks.length > 0, "gap beyond threshold inserts an idle break mark");
// Regression: idle compression must actually shrink the rendered domain — the
// far-future record pair sits 600s after the main activity, so the compressed
// domain must be far below the raw 600s span (not just a cosmetic "⌁" mark).
{
  const rawSpan = (10 * IDLE_COMPRESS_SECONDS * 1000 + 100); // ~600.1s of wall clock
  const renderedSpan = idleTimeline!.domain[1] - idleTimeline!.domain[0];
  check(renderedSpan < rawSpan / 2, `idle compression shrinks the rendered domain (raw ${rawSpan}ms -> ${renderedSpan.toFixed(0)}ms)`);
  // Idle mark must sit inside the rendered domain (normalized 0..1), not at the
  // raw far-future coordinate that the old code left behind.
  check(idleTimeline!.idleBreaks.every((b) => b.at >= 0 && b.at <= 1), "idle marks are normalized into the compressed domain");

  // Time vs Actual must differ: Time compresses the idle gap, Actual keeps the
  // raw wall clock (honest time proportions). Regression for the port gap that
  // ran both modes through the same code path — identical charts.
  const actualTimeline = deriveTrajectoryTimeline(layoutIdle.turns, "actual");
  check(actualTimeline !== null, "actual timeline derives");
  check(actualTimeline!.idleBreaks.length === 0, "actual mode reports no idle breaks (raw wall clock)");
  const actualSpan = actualTimeline!.domain[1] - actualTimeline!.domain[0];
  check(Math.abs(actualSpan - rawSpan) < 1_000, `actual keeps the raw span (~${(rawSpan / 1000).toFixed(1)}s, got ${(actualSpan / 1000).toFixed(1)}s)`);
  check(actualSpan - renderedSpan > IDLE_COMPRESS_SECONDS * 1000, "time mode compresses while actual mode does not — the two charts differ");
  const actualMaxEnd = Math.max(...actualTimeline!.spans.map((s) => s.end));
  check(actualMaxEnd <= 1 + 1e-9, `actual spans stay inside the raw domain (max ${actualMaxEnd.toFixed(4)})`);
  const actualToolSpans = actualTimeline!.spans.filter((s) => s.kind === "tool");
  const actualLateSpan = actualToolSpans[actualToolSpans.length - 1];
  check(actualLateSpan !== undefined && actualLateSpan.start > 0.9, "actual mode keeps the late tool near the far end of the raw timeline");
}

// ── search index (incremental, AND terms, case-insensitive) ───────────────
const searchIndex = new TrajectorySearchIndex();
check(searchIndex.addCells(projection.cells) === projection.cells.length, "first add indexes every cell");
check(searchIndex.addCells(projection.cells) === 0, "re-adding the same cells is a no-op");
const hitSet = searchIndex.search("linter");
check(hitSet !== null && hitSet.size === 1, "search matches the linter prompt");
const missSet = searchIndex.search("linter quantum");
check(missSet === null || missSet.size === 0, "AND terms with no co-occurrence match nothing");
const noneSet = searchIndex.search("");
check(noneSet === null, "empty query returns null (no filter)");

// ── thinking records: first-class lane in all four modes ──────────────────
// Regression: thinking lived only inside the assistant inspector; the four
// timeline modes (sequence/duration/time/actual) had no lane for it.
const thinkMessages: ChatMessage[] = [
  { id: "tu1", role: "user", content: "think hard", timestamp: now },
  { id: "ta1", role: "assistant", content: "answer", thinking: "I should inspect the code first", timestamp: now + 1_000, streaming: false },
];
const thinkEntries: TraceEntry[] = [
  { id: "te1", runId: "r", sessionId: "s", sequence: 1, timestampMs: now + 100, kind: "model_request_started", summary: "req", details: { turn: 1, provider: "anthropic" }, category: "model", status: "active" },
  { id: "te2", runId: "r", sessionId: "s", sequence: 2, timestampMs: now + 900, kind: "stream_state_changed", summary: "stream", details: { state: "streaming", turn: 1 }, category: "model", status: "active" },
];
const layoutThink = buildLedgerLayout({ messages: thinkMessages, traceEntries: thinkEntries, subagentRunsById: {} });
const thinkCell = layoutThink.turns[0].cells.find((c) => c.kind === "thinking");
check(thinkCell !== undefined, "assistant message with thinking projects a thinking record");
check(thinkCell!.thinkingDetail === "I should inspect the code first", "thinking record carries the full text");
check(layoutThink.turns[0].cells.indexOf(thinkCell!) < layoutThink.turns[0].cells.findIndex((c) => c.kind === "assistant"), "thinking record precedes its assistant record");
for (const mode of ["sequence", "duration", "time", "actual"] as const) {
  const tl = deriveTrajectoryTimeline(layoutThink.turns, mode);
  check(tl !== null, `thinking timeline derives in ${mode} mode`);
  check(tl!.spans.some((s) => s.lane === "thinking"), `thinking span appears on its own lane in ${mode} mode`);
}
const thinkSearch = new TrajectorySearchIndex();
thinkSearch.addCells(layoutThink.turns[0].cells);
check(thinkSearch.search("inspect") !== null, "thinking text is searchable");

// ── thinking/assistant split at the thinking close ─────────────────────────
// Regression: both rows anchored at step start — identical "Started" stamps
// and overlapping timeline spans. They are sequential halves of one step:
// thinking = [start, thinkEnd), assistant = [thinkEnd, stepEnd).
{
  const start = now + 100;
  const thinkEnd = now + 700;   // reasoning closes here
  const stepEnd = now + 1_500;  // visible text + usage finish the step later
  const entries: TraceEntry[] = [
    { id: "x1", runId: "r", sessionId: "s", sequence: 1, timestampMs: start, kind: "model_request_started", summary: "req", details: { turn: 1 }, category: "model", status: "active" },
    { id: "x2", runId: "r", sessionId: "s", sequence: 2, timestampMs: thinkEnd, kind: "thinking_state", summary: "thinking", details: { active: false, turn: 1 }, category: "model", status: "success" },
    { id: "x3", runId: "r", sessionId: "s", sequence: 3, timestampMs: stepEnd, kind: "usage_updated", summary: "usage", details: { turn: 1, turn_output: 10 }, category: "usage", status: "success" },
  ];
  const msgs: ChatMessage[] = [
    { id: "u1", role: "user", content: "hi", timestamp: now },
    { id: "a1", role: "assistant", content: "visible answer", thinking: "hidden reasoning", timestamp: stepEnd, streaming: false },
  ];
  const layout = buildLedgerLayout({ messages: msgs, traceEntries: entries, subagentRunsById: {} });
  const thinkingCell = layout.turns[0].cells.find((c) => c.kind === "thinking");
  const assistantCell = layout.turns[0].cells.find((c) => c.kind === "assistant");
  check(thinkingCell !== undefined && assistantCell !== undefined, "both thinking and assistant records projected");
  const thinkSec = thinkingCell!.timeSeconds;
  const assistantSec = assistantCell!.timeSeconds;
  check(thinkSec !== null && Math.abs(thinkSec - (thinkEnd - start) / 1000) < 1e-9, `thinking duration is block-close minus start (got ${thinkSec}s, want ${((thinkEnd - start) / 1000).toFixed(3)}s)`);
  check(assistantSec !== null && Math.abs(assistantSec - (stepEnd - thinkEnd) / 1000) < 1e-9, `assistant duration starts where thinking closed (got ${assistantSec}s, want ${((stepEnd - thinkEnd) / 1000).toFixed(3)}s)`);
  check(assistantCell!.startedAt === thinkEnd && thinkingCell!.startedAt === start, "assistant and thinking rows carry distinct sequential starts");
  // No thinking close on record (restored session without trace): assistant
  // falls back to anchoring at the message timestamp, not overlapping a
  // synthetic thinking end.
  const layoutNoTrace = buildLedgerLayout({
    messages: [{ id: "u2", role: "user", content: "hi", timestamp: now },
               { id: "a2", role: "assistant", content: "plain answer", thinking: "reasoning without close event", timestamp: stepEnd, streaming: false }],
    traceEntries: [],
    subagentRunsById: {},
  });
  const plainAssistant = layoutNoTrace.turns[0].cells.find((c) => c.kind === "assistant");
  check(plainAssistant !== undefined && plainAssistant.timeSeconds === null, "no trace → assistant duration stays null rather than guessing");
  // Replay fallback: the thinking row must NOT take the same gap as the
  // assistant row — adjacent thinking/assistant durations were identical
  // (double-counted). With a following stamped record the assistant gap fills,
  // thinking stays unknown.
  const replayWithNext = buildLedgerLayout({
    messages: [{ id: "u4", role: "user", content: "hi", timestamp: now },
               { id: "a3", role: "assistant", content: "answer", thinking: "reasoning", timestamp: stepEnd, streaming: false },
               { id: "t1", role: "tool", toolName: "Read", toolOk: true, content: "ok", timestamp: stepEnd + 1_500, durationMs: 900 } as unknown as ChatMessage],
    traceEntries: [],
    subagentRunsById: {},
  });
  const filledAssistant = replayWithNext.turns[0].cells.find((c) => c.kind === "assistant");
  check(filledAssistant !== undefined && filledAssistant.timeSeconds !== null && Math.abs(filledAssistant.timeSeconds! - 1.5) < 1e-9, `replay fallback still fills the assistant gap (got ${filledAssistant?.timeSeconds}s, want 1.5s)`);
  const skippedThinking = replayWithNext.turns[0].cells.find((c) => c.kind === "thinking");
  check(skippedThinking !== undefined && skippedThinking.timeSeconds === null, "replay fallback skips thinking rows — adjacent thinking/assistant durations no longer identical");
  const replayUser = replayWithNext.turns[0].cells.find((c) => c.kind === "user");
  check(replayUser !== undefined && replayUser.timeSeconds === null, "replay fallback skips user rows — the gap is model latency, not prompt runtime");

  // Parallel tool_use in one replayed step: without per-tool trace both tool
  // rows inherit the identical synthetic span and draw fully overlapping bars
  // on the tool lane. Siblings must be serialized start-after-previous-end.
  {
    const parallel = buildLedgerLayout({
      messages: [
        { id: "pu1", role: "user", content: "go", timestamp: now },
        { id: "pa1", role: "assistant", content: "", timestamp: now + 1_000, streaming: false },
        { id: "pt1", role: "tool", toolName: "Read", toolOk: true, content: "ok", timestamp: now + 3_000, durationMs: 2_000 } as unknown as ChatMessage,
        { id: "pt2", role: "tool", toolName: "Read", toolOk: true, content: "ok", timestamp: now + 3_000, durationMs: 2_000 } as unknown as ChatMessage,
      ],
      traceEntries: [],
      subagentRunsById: {},
    });
    const tools = parallel.turns[0].cells.filter((c) => c.kind === "tool");
    check(tools.length === 2, `both parallel tool records projected (got ${tools.length})`);
    const [first, second] = tools;
    const fEnd = (first.startedAt ?? 0) + (first.timeSeconds ?? 0) * 1000;
    const sStart = second.startedAt ?? 0;
    check(
      first.startedAt !== null && second.startedAt !== null && sStart >= fEnd,
      `parallel tool rows serialized, no overlap (first ends ${fEnd}, second starts ${sStart})`,
    );
    const laneEnd = (now + 3_000) + 2_000;
    check(fEnd <= laneEnd && sStart + (second.timeSeconds ?? 0) * 1000 <= laneEnd, "serialized tools stay within the shared replay window");
  }

  // Out-of-order thinking close: the provider flushes `thinking_state
  // active:false` AFTER the step's completion event (usage_updated /
  // tool_use_start / run_finished). Without a clamp the assistant window
  // starts at thinkingEnd > stepEnd → negative "time" (regression from the
  // live run where ASSISTANT showed -527ms / -399ms). The thinking block is a
  // sequential half of the step, so it can never extend past stepEnd.
  const lateThinkEnd = now + 1_600; // reasoning close arrives late (AFTER step completion)
  const earlyStepEnd = now + 1_400; // step actually completes earlier
  const outOfOrderEntries: TraceEntry[] = [
    { id: "o1", runId: "r", sessionId: "s", sequence: 1, timestampMs: now + 100, kind: "model_request_started", summary: "req", details: { turn: 1 }, category: "model", status: "active" },
    // usage_updated fires BEFORE the thinking close (provider flushes the
    // reasoning block last) — this is what pushed assistant duration negative.
    { id: "o2", runId: "r", sessionId: "s", sequence: 2, timestampMs: earlyStepEnd, kind: "usage_updated", summary: "usage", details: { turn: 1, turn_output: 10 }, category: "usage", status: "success" },
    { id: "o3", runId: "r", sessionId: "s", sequence: 3, timestampMs: lateThinkEnd, kind: "thinking_state", summary: "thinking", details: { active: false, turn: 1 }, category: "model", status: "success" },
  ];
  const oooMsgs: ChatMessage[] = [
    { id: "u1", role: "user", content: "hi", timestamp: now },
    { id: "a1", role: "assistant", content: "visible answer", thinking: "hidden reasoning", timestamp: lateThinkEnd, streaming: false },
  ];
  const layoutOoo = buildLedgerLayout({ messages: oooMsgs, traceEntries: outOfOrderEntries, subagentRunsById: {} });
  const oooThinking = layoutOoo.turns[0].cells.find((c) => c.kind === "thinking");
  const oooAssistant = layoutOoo.turns[0].cells.find((c) => c.kind === "assistant");
  check(oooThinking !== undefined && oooAssistant !== undefined, "out-of-order thinking close still projects both records");
  check(oooAssistant!.timeSeconds !== null && oooAssistant!.timeSeconds! >= 0, `out-of-order thinking close never yields a negative assistant duration (got ${oooAssistant?.timeSeconds}s)`);
  check(oooThinking!.timeSeconds !== null && oooThinking!.timeSeconds! <= (earlyStepEnd - (now + 100)) / 1000 + 1e-9, `thinking duration clamps at the step end (got ${oooThinking?.timeSeconds}s, cap ${((earlyStepEnd - (now + 100)) / 1000).toFixed(3)}s)`);
}

console.log("ledger invariants: all passed");
