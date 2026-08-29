/**
 * Trajectory ledger layout — ported from DSH `ui-trajectory` `layout.ts`.
 * Projects NonoClaw frontend state (messages + traceEntries + subagent runs)
 * into turn-grouped ledger records with usage, own-duration times and
 * in-flight running calls.
 */

import type { ChatMessage, SubagentRun } from "../types";
import type { TraceEntry } from "../trace";
import type { LedgerCell, LedgerSourceKind } from "./types";
import type { AssistantMetricDetail } from "./types";

export type { LedgerSourceKind };

/** One ledger turn = one user prompt → assistant completion cycle. */
export interface LedgerTurnModel {
  /** 0-based turn number. */
  n: number;
  /** Records in order; the first user record opens the turn. */
  cells: LedgerCell[];
  /** Usage for the assistant step(s) of this turn, when reported. */
  usage: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
  } | null;
  /** Request provenance (provider / model / retry), when reported. */
  request: {
    provider?: string;
    model?: string;
    turn?: number;
    retryAttempt?: number;
  } | null;
  /** Earliest start / latest end across cells (epoch ms), when measurable. */
  startAt: number | null;
  endAt: number | null;
}

export interface LedgerLayoutInput {
  messages: ChatMessage[];
  traceEntries: TraceEntry[];
  subagentRunsById: Record<string, SubagentRun>;
}

export interface LedgerLayoutResult {
  turns: LedgerTurnModel[];
  total: number;
}

interface UsageRaw {
  input_tokens?: number;
  output_tokens?: number;
  cache_read_input_tokens?: number;
  cache_creation_input_tokens?: number;
}

/** One engine step window: model_request_started(turn) until the next
 * model_request_started. Pairing assistant messages to trace turns by
 * ledger-turn index is WRONG (one ledger turn spans many engine turns),
 * so windows are paired by message timestamps instead. */
export interface StepWindow {
  turn: number;
  start: number;
  firstToken: number | null;
  completed: number | null;
  /** Epoch ms when the extended-thinking block closed (thinking_state
   * active:false). Distinct from `completed` (whole step), so thinking can be
   * timed without the visible text that follows it. */
  thinkingEnd: number | null;
  usage: UsageRaw | null;
  provider?: string;
  model?: string;
  retryAttempt?: number;
}

function finiteMs(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function collectTurnUsageRaw(ev: Record<string, unknown> | undefined): UsageRaw | null {
  const nested = ev?.turn_usage;
  if (nested !== null && typeof nested === "object") return nested as UsageRaw;
  const flat: UsageRaw = {};
  const inTok = finiteMs(ev?.turn_input) ?? finiteMs(ev?.turn_in);
  const outTok = finiteMs(ev?.turn_output) ?? finiteMs(ev?.turn_out);
  const cacheRead = finiteMs(ev?.turn_cache_read);
  const cacheWrite = finiteMs(ev?.turn_cache_write);
  if (inTok !== null) flat.input_tokens = inTok;
  if (outTok !== null) flat.output_tokens = outTok;
  if (cacheRead !== null) flat.cache_read_input_tokens = cacheRead;
  if (cacheWrite !== null) flat.cache_creation_input_tokens = cacheWrite;
  return Object.keys(flat).length > 0 ? flat : null;
}

/** Build engine step windows from trace entries, in chronological order. */
function buildStepWindows(entries: TraceEntry[]): StepWindow[] {
  const windows: StepWindow[] = [];
  let current: StepWindow | null = null;
  for (const entry of entries) {
    const ev = entry.details as Record<string, unknown> | undefined;
    if (entry.kind === "model_request_started") {
      current = {
        turn: finiteMs(ev?.turn) ?? (windows.length + 1),
        start: entry.timestampMs,
        firstToken: null,
        completed: null,
        thinkingEnd: null,
        usage: null,
        provider: typeof ev?.provider === "string" ? ev.provider : undefined,
      };
      windows.push(current);
      continue;
    }
    if (current === null) continue;
    if (entry.kind === "model_resolved") {
      if (typeof ev?.model === "string" && current.model === undefined) current.model = ev.model;
    } else if (entry.kind === "retry_scheduled") {
      const attempt = finiteMs(ev?.attempt);
      if (attempt !== null && (current.retryAttempt === undefined || attempt > current.retryAttempt)) current.retryAttempt = attempt;
    } else if (entry.kind === "stream_state_changed" && ev?.state === "streaming") {
      if (current.firstToken === null) current.firstToken = entry.timestampMs;
    } else if (entry.kind === "thinking_state" && ev?.active === false) {
      // Precise end of the reasoning block. The first (earliest) active:false
      // wins — providers may emit both a block-level close and a MessageStop
      // fallback; the block close is the accurate one.
      if (current.thinkingEnd === null) current.thinkingEnd = entry.timestampMs;
    } else if (entry.kind === "usage_updated") {
      if (current.completed === null) current.completed = entry.timestampMs;
      const raw = collectTurnUsageRaw(ev);
      if (raw !== null) current.usage = raw;
    } else if (entry.kind === "tool_use_start" || entry.kind === "run_finished") {
      // The assistant step ends when its tool batch starts or the run ends.
      if (current.completed === null) current.completed = entry.timestampMs;
    }
  }
  return windows;
}

function preview(text: string, limit = 400): string {
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > limit ? `${flat.slice(0, limit)}…` : flat;
}

function safeStringify(value: unknown): string | undefined {
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2) ?? undefined;
  } catch {
    return undefined;
  }
}

function toolUseIdOf(message: ChatMessage): string | null {
  if (message.toolName === undefined || message.toolName.length === 0) return null;
  if (message.id.startsWith("tool-")) return message.id.slice("tool-".length);
  return message.id;
}

/** Build the trajectory ledger layout from live frontend state. */
export function buildLedgerLayout(input: LedgerLayoutInput): LedgerLayoutResult {
  const { messages, traceEntries, subagentRunsById } = input;
  const stepWindows = buildStepWindows(traceEntries);
  let windowCursor = 0;

  const turns: LedgerTurnModel[] = [];
  let index = 0;
  let turnNo = 0;
  let current: LedgerTurnModel | null = null;

  const pushCell = (turn: LedgerTurnModel, cell: Omit<LedgerCell, "index">): LedgerCell => {
    const placed = { ...cell, index: ++index } as LedgerCell;
    turn.cells.push(placed);
    if (cell.startedAt != null && (turn.startAt === null || cell.startedAt < turn.startAt)) turn.startAt = cell.startedAt;
    const endGuess = cell.startedAt != null && cell.timeSeconds != null ? cell.startedAt + cell.timeSeconds * 1000 : null;
    if (endGuess != null && endGuess > (turn.endAt ?? 0)) turn.endAt = endGuess;
    return placed;
  };

  for (const message of messages) {
    if (message.role === "user") {
      current = {
        n: turnNo++,
        cells: [],
        usage: null,
        request: null,
        startAt: message.timestamp ?? null,
        endAt: message.timestamp ?? null,
      };
      turns.push(current);
      pushCell(current, {
        kind: "user",
        opensTurn: true,
        text: preview(message.content, 160) || "(empty prompt)",
        inputDetail: message.content,
        recordId: `user\u0000${message.id}`,
        timeSeconds: null,
        startedAt: message.timestamp ?? null,
        ...(message.attachments && message.attachments.length > 0
          ? { preview: `${message.attachments.length} attachment(s)` }
          : {}),
      });
    } else if (message.role === "assistant") {
      const turnModel = current ?? (turns.length > 0 ? turns[turns.length - 1] : null);
      if (turnModel === null) continue;
      // Pair this assistant message with the step window whose start is the
      // latest one at or before the message timestamp. Streaming messages
      // carry the streaming-start timestamp; tool-only turns have no message.
      const msgTs = message.timestamp ?? Number.POSITIVE_INFINITY;
      while (
        windowCursor + 1 < stepWindows.length
        && stepWindows[windowCursor + 1].start <= msgTs
      ) windowCursor += 1;
      let window = stepWindows[windowCursor];
      if (window !== undefined && window.start > msgTs) {
        // Message predates every window (restored sessions without trace);
        // fall back to the nearest earlier window, else none.
        window = stepWindows[Math.max(0, windowCursor)] ?? undefined;
        if (window.start > msgTs) window = undefined as unknown as StepWindow;
      }
      const timing = window;
      const usage = window?.usage ?? null;
      if (usage !== null && turnModel.usage === null) {
        // First assistant message of this ledger turn: sum usage of every
        // step window that starts within the turn (user prompt → next prompt).
        const turnStart = turnModel.startAt ?? window.start;
        const nextTurn = turns.find((t) => t.n > turnModel.n && t.n !== -1);
        const turnEnd = nextTurn?.startAt ?? Number.POSITIVE_INFINITY;
        const sum = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
        for (const w of stepWindows) {
          if (w.usage === null) continue;
          if (w.start < turnStart || w.start >= turnEnd) continue;
          sum.input += w.usage.input_tokens ?? 0;
          sum.output += w.usage.output_tokens ?? 0;
          sum.cacheRead += w.usage.cache_read_input_tokens ?? 0;
          sum.cacheWrite += w.usage.cache_creation_input_tokens ?? 0;
        }
        turnModel.usage = sum;
      }
      if (turnModel.request === null && window !== undefined) {
        turnModel.request = {
          provider: window.provider,
          model: window.model,
          turn: window.turn,
          retryAttempt: window.retryAttempt,
        };
      }
      const startedAt = timing?.start ?? message.timestamp ?? null;
      const completedAt = timing?.completed ?? null;
      const metric: AssistantMetricDetail | undefined = timing === undefined
        ? undefined
        : {
          timingRecorded: true,
          stepStartTime: timing.start,
          firstTokenTime: timing.firstToken,
          completedTime: timing.completed,
          usageProvided: usage !== null,
          outputTokens: usage?.output_tokens ?? null,
        };
      if (message.thinking !== undefined && message.thinking.length > 0) {
        // Thinking precedes the visible assistant output; project it as its
        // own ledger record so all four timeline modes can render it. Its
        // duration is the thinking block alone (step start → thinking close),
        // not the whole assistant step — the visible text that follows must
        // not be charged to the thinking row.
        const thinkingEnd = timing?.thinkingEnd ?? null;
        pushCell(turnModel, {
          kind: "thinking",
          text: preview(message.thinking, 200) || "(empty thinking)",
          thinkingDetail: message.thinking,
          recordId: `thinking\u0000${message.id}`,
          timeSeconds: startedAt !== null && thinkingEnd !== null
            ? (thinkingEnd - startedAt) / 1000
            : null,
          startedAt,
        } as LedgerCell);
      }
      pushCell(turnModel, {
        kind: "assistant",
        text: preview(message.content, 200) || "(tool use turn)",
        inputDetail: message.content,
        thinkingDetail: message.thinking !== undefined && message.thinking.length > 0
          ? message.thinking
          : undefined,
        recordId: `assistant\u0000${message.id}`,
        timeSeconds: startedAt !== null && completedAt !== null ? (completedAt - startedAt) / 1000 : null,
        startedAt,
        input: usage?.input_tokens,
        cacheRead: usage?.cache_read_input_tokens,
        cacheWrite: usage?.cache_creation_input_tokens,
        output: usage?.output_tokens,
        assistantMetrics: metric,
      } as LedgerCell);
    } else if (message.role === "tool") {
      const turnModel = current ?? (turns.length > 0 ? turns[turns.length - 1] : null);
      if (turnModel === null) continue;
      const callId = toolUseIdOf(message);
      pushCell(turnModel, {
        kind: "tool",
        text: `${message.toolName ?? "Tool"}${message.toolOk === false ? " ✗" : ""}`,
        callId: callId ?? undefined,
        recordId: `tool\u0000${message.id}`,
        inputDetail: message.toolInput !== undefined ? safeStringify(message.toolInput) : undefined,
        outputDetail: message.content,
        isError: message.toolOk === false,
        startedAt: message.timestamp ?? null,
        timeSeconds: message.durationMs != null && message.durationMs > 0
          ? message.durationMs / 1000
          : null,
      });
    }
  }

  // Replay fallback: wall-clock gap to the next stamped record approximates
  // the duration of records that have no measured span (no trace entries).
  // Capped at 60s — matching the idle-compression threshold in the timeline,
  // beyond which the gap is user idle, not this record's runtime.
  {
    const REPLAY_FALLBACK_CAP_SECONDS = 60;
    const stamped = messages
      .map((m) => (typeof m.timestamp === "number" && Number.isFinite(m.timestamp) ? m.timestamp : null))
      .filter((t): t is number => t !== null);
    if (stamped.length >= 2) {
      const timelineMs = stamped.map((t, i) => ({ t, next: stamped[i + 1] as number | undefined }));
      for (const turn of turns) {
        for (const cell of turn.cells) {
          if (cell.timeSeconds != null || cell.startedAt == null) continue;
          let cursor: { t: number; next: number | undefined } | null = null;
          for (const entry of timelineMs) {
            if (entry.t <= cell.startedAt) cursor = entry;
            else break;
          }
          if (cursor === null || cursor.next === undefined) continue;
          const spanMs = cursor.next - cursor.t;
          if (spanMs > 0 && spanMs <= REPLAY_FALLBACK_CAP_SECONDS * 1000) {
            cell.timeSeconds = spanMs / 1000;
          }
        }
      }
    }
  }

  // Per-tool elapsed / start from tool_execution_started ↔ finished pairs.
  const startsById = new Map<string, number>();
  const execSpan = new Map<string, { start: number; elapsed: number }>();
  for (const entry of traceEntries) {
    const ev = entry.details as Record<string, unknown> | undefined;
    const id = typeof ev?.tool_use_id === "string" ? ev.tool_use_id : null;
    if (id === null) continue;
    if (entry.kind === "tool_execution_started") {
      if (!startsById.has(id)) startsById.set(id, entry.timestampMs);
    } else if (entry.kind === "tool_execution_finished") {
      const start = startsById.get(id);
      const elapsed = finiteMs(ev?.elapsed_ms);
      const span = elapsed !== null && elapsed >= 0
        ? { start: start ?? entry.timestampMs - elapsed, elapsed }
        : start !== undefined
          ? { start, elapsed: Math.max(0, entry.timestampMs - start) }
          : { start: entry.timestampMs, elapsed: 0 };
      execSpan.set(id, span);
    }
  }
  for (const turn of turns) {
    for (const cell of turn.cells) {
      if (cell.kind === "tool" && cell.callId !== undefined) {
        const span = execSpan.get(cell.callId);
        if (span !== undefined) {
          cell.timeSeconds = span.elapsed / 1000;
          cell.startedAt = span.start;
          if (turn.startAt === null || span.start < turn.startAt) turn.startAt = span.start;
          const end = span.start + span.elapsed;
          if (end > (turn.endAt ?? 0)) turn.endAt = end;
        }
      }
    }
  }

  // Subagent branches: child tool records ride under the parent's turn.
  for (const run of Object.values(subagentRunsById)) {
    const parentId = run.parentToolUseId;
    if (parentId === undefined || parentId.length === 0) continue;
    let targetTurn: LedgerTurnModel | null = null;
    for (let i = turns.length - 1; i >= 0; i--) {
      if (turns[i].cells.some((c) => c.callId === parentId)) { targetTurn = turns[i]; break; }
    }
    if (targetTurn === null) targetTurn = turns[turns.length - 1] ?? null;
    if (targetTurn === null) continue;
    let first: number | null = null;
    for (const toolId of run.toolOrder) {
      const tool = run.toolsById[toolId];
      if (tool === undefined) continue;
      pushCell(targetTurn, {
        kind: "tool",
        text: `${tool.name} · subagent${run.profile != null && run.profile.length > 0 ? ` (${run.profile})` : ""}${tool.ok === false ? " ✗" : ""}`,
        callId: tool.id,
        subagentRunId: run.id,
        recordId: `subagent\u0000${run.id}\u0000${tool.id}`,
        inputDetail: tool.input !== undefined ? safeStringify(tool.input) : undefined,
        outputDetail: tool.result,
        isError: tool.ok === false,
        timeSeconds: null,
        startedAt: null,
      });
      if (first === null) first = null; // timestamps not tracked per subagent tool
    }
    void first;
  }

  // Compaction records: standalone ones land in a "Between turns" pseudo-turn.
  for (const entry of traceEntries) {
    if (entry.kind !== "compaction_started" && entry.kind !== "compacted") continue;
    const ev = entry.details as Record<string, unknown> | undefined;
    const cell: LedgerCell = {
      index: 0,
      kind: "compacted",
      text: `Compacted${typeof ev?.tokens_before === "number" ? ` · ${ev.tokens_before.toLocaleString()} tokens before` : ""}`,
      sourceSeq: entry.sequence,
      recordId: `compacted\u0000${entry.id}`,
      timeSeconds: null,
      startedAt: entry.timestampMs,
    };
    // Compaction is a session-level event, not part of any single turn — it
    // always lands in the "Between turns" pseudo-turn.
    const target = ensureBetweenTurns(turns);
    target.cells.push({ ...cell, index: ++index });
    if (target.startAt === null || entry.timestampMs < target.startAt) target.startAt = entry.timestampMs;
    if (entry.timestampMs > (target.endAt ?? 0)) target.endAt = entry.timestampMs;
  }

  // In-flight tool calls with no tool message yet: emit running records.
  const seenCallIds = new Set<string>();
  for (const turn of turns) {
    for (const cell of turn.cells) if (cell.callId !== undefined) seenCallIds.add(cell.callId);
  }
  for (const entry of traceEntries) {
    if (entry.kind !== "tool_execution_started") continue;
    const ev = entry.details as Record<string, unknown> | undefined;
    const id = typeof ev?.tool_use_id === "string" ? ev.tool_use_id : null;
    if (id === null || seenCallIds.has(id)) continue;
    const owner = findTurnContainingAt(turns, entry.timestampMs) ?? turns[turns.length - 1] ?? null;
    if (owner === null) continue;
    owner.cells.push({
      index: ++index,
      kind: "tool",
      text: `${typeof ev?.tool_name === "string" ? ev.tool_name : "Tool"} (running)`,
      callId: id,
      recordId: `running\u0000${id}`,
      timeSeconds: null,
      startedAt: entry.timestampMs,
    });
    seenCallIds.add(id);
  }

  return { turns, total: index };
}

function findTurnContainingAt(turns: LedgerTurnModel[], tsMs: number): LedgerTurnModel | null {
  // Trace entries carry real timestamps but messages do not carry sequence
  // numbers, so place in-flight records by timestamp overlap. Walk newest →
  // oldest and return the first turn whose measured start does not exceed the
  // entry timestamp (the nearest turn that already began). Turns without a
  // measurable start are skipped; the caller falls back to the last turn.
  for (let i = turns.length - 1; i >= 0; i--) {
    const start = turns[i].startAt;
    if (start !== null && tsMs >= start) return turns[i];
  }
  return null;
}

/** Ensure a trailing pseudo-turn holding between-turns records exists. */
function ensureBetweenTurns(turns: LedgerTurnModel[]): LedgerTurnModel {
  const last = turns[turns.length - 1];
  if (last !== undefined && last.n === -1) return last;
  const between: LedgerTurnModel = {
    n: -1,
    cells: [],
    usage: null,
    request: null,
    startAt: null,
    endAt: null,
  };
  turns.push(between);
  return between;
}
