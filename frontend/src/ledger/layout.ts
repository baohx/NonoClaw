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
  runId: string;
  /** Sequence of the model_request_started event within this run. */
  requestSequence: number;
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

/** Build engine step windows from trace entries, in chronological order.
 *
 * `sequence` is a PER-RUN counter (each engine RunContext numbers its events
 * from 1), so entries of different runs in one session are not comparable by
 * sequence — a prior run's straggler event can sort after the next run's
 * `model_request_started` and land in its window, producing window stamps
 * before the window even opened (negative durations). Build windows per run
 * (in-run sequence is causal) and merge the groups by window start. */
function buildStepWindows(entries: TraceEntry[]): StepWindow[] {
  const byRun = new Map<string, TraceEntry[]>();
  for (const entry of entries) {
    const list = byRun.get(entry.runId) ?? [];
    list.push(entry);
    byRun.set(entry.runId, list);
  }
  const windows: StepWindow[] = [];
  const completionStrength = new Map<StepWindow, number>();
  const setCompletion = (window: StepWindow, timestampMs: number, strength: number): void => {
    const previousStrength = completionStrength.get(window) ?? -1;
    if (strength > previousStrength
      || (strength === previousStrength && timestampMs > (window.completed ?? Number.NEGATIVE_INFINITY))) {
      window.completed = timestampMs;
      completionStrength.set(window, strength);
    }
  };
  for (const [runId, groupedEntries] of byRun) {
    // appendTraceEntry retains global arrival order because sequence counters
    // reset per run. Within one run, sequence is the causal clock guaranteed
    // by the wire protocol; wall-clock timestamps can move backward and must
    // never attach a close event to the preceding model window.
    const runEntries = [...groupedEntries].sort((a, b) => (
      a.sequence - b.sequence || a.timestampMs - b.timestampMs || a.id.localeCompare(b.id)
    ));
    let current: StepWindow | null = null;
    for (const entry of runEntries) {
      const ev = entry.details as Record<string, unknown> | undefined;
      if (entry.kind === "model_request_started") {
        current = {
          runId,
          requestSequence: entry.sequence,
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
      } else if (entry.kind === "stream_state_changed") {
        if (ev?.state === "streaming") {
          if (current.firstToken === null) current.firstToken = entry.timestampMs;
        } else if (ev?.state === "completed" || ev?.state === "interrupted") {
          // Authoritative stream terminal. MessageStart emits an early
          // usage_updated before any thinking deltas; that event remains a
          // compatibility fallback, but must not truncate a long reasoning
          // block when MessageStop later supplies the real completion time.
          setCompletion(current, entry.timestampMs, 3);
        }
      } else if (entry.kind === "thinking_state" && ev?.active === false) {
        // Precise end of the reasoning block. The first (earliest) active:false
        // wins — providers may emit both a block-level close and a MessageStop
        // fallback; the block close is the accurate one.
        if (current.thinkingEnd === null) current.thinkingEnd = entry.timestampMs;
      } else if (entry.kind === "usage_updated") {
        // Usage can arrive at MessageStart and again near MessageStop. It is a
        // provisional fallback only; later stream/tool/run boundaries carry
        // stronger lifecycle semantics and must replace it.
        setCompletion(current, entry.timestampMs, 0);
        const raw = collectTurnUsageRaw(ev);
        if (raw !== null) current.usage = raw;
      } else if (entry.kind === "tool_use_start") {
        // Tool dispatch is a stronger model-step boundary than provisional
        // usage, but remains below an explicit stream terminal.
        setCompletion(current, entry.timestampMs, 2);
      } else if (entry.kind === "run_finished") {
        setCompletion(current, entry.timestampMs, 1);
      }
    }
  }
  windows.sort((a, b) => a.start - b.start);
  // Thinking is a sequential half of the step, so its close can never extend
  // past the step end. Providers may flush `thinking_state active:false`
  // AFTER the step's completion event (usage_updated / tool_use_start /
  // run_finished), which would otherwise anchor the assistant window at a
  // thinkingEnd > completed → negative duration. Clamp to completed.
  // Symmetrically, a stamp BEFORE the window opened belongs to another run's
  // straggler → drop it rather than render a negative duration.
  for (const w of windows) {
    if (w.completed !== null && w.completed < w.start) w.completed = null;
    if (w.firstToken !== null && w.completed !== null && w.firstToken > w.completed) {
      if ((completionStrength.get(w) ?? 0) === 0) {
        // An early MessageStart usage cannot precede observed stream activity;
        // promote the provisional boundary to the known activity timestamp.
        w.completed = w.firstToken;
      } else {
        // A first-token stamp after a stronger terminal is inconsistent and
        // cannot safely define the assistant split.
        w.firstToken = null;
      }
    }
    if (w.thinkingEnd !== null && w.completed !== null && w.thinkingEnd > w.completed) {
      w.thinkingEnd = w.completed;
    }
    if (w.thinkingEnd !== null && w.thinkingEnd < w.start) {
      w.thinkingEnd = null;
    }
    if (w.firstToken !== null && w.firstToken < w.start) {
      w.firstToken = null;
    }
  }
  return windows;
}

/** Match a chat assistant to a model step without letting a later, unfinished
 * concurrent run steal a completed message. Wall-clock timestamps choose
 * between runs; within one run, request sequence remains authoritative when
 * the system clock moves backward. */
function selectStepWindow(windows: StepWindow[], message: ChatMessage): StepWindow | undefined {
  if (windows.length === 0) return undefined;
  const laterStarted = (best: StepWindow, candidate: StepWindow): StepWindow => {
    if (candidate.runId === best.runId && candidate.requestSequence !== best.requestSequence) {
      return candidate.requestSequence > best.requestSequence ? candidate : best;
    }
    return candidate.start > best.start ? candidate : best;
  };
  const msgTs = message.timestamp;
  if (msgTs === undefined || !Number.isFinite(msgTs)) return windows.reduce(laterStarted);
  const started = windows.filter((window) => window.start <= msgTs);
  if (started.length === 0) return undefined;
  const latestStarted = started.reduce(laterStarted);
  if (message.streaming === true) return latestStarted;

  const containing = started.filter((window) => window.completed !== null && window.completed >= msgTs);
  if (containing.length > 0) return containing.reduce(laterStarted);

  const completedBefore = started.filter((window) => window.completed !== null && window.completed <= msgTs);
  if (completedBefore.length > 0) {
    const completed = completedBefore.reduce((best, candidate) => {
      if (candidate.runId === best.runId && candidate.requestSequence !== best.requestSequence) {
        return candidate.requestSequence > best.requestSequence ? candidate : best;
      }
      return (candidate.completed as number) > (best.completed as number) ? candidate : best;
    });
    // A later causal request in the same run cannot be ignored merely because
    // its terminal timestamp rolled behind its own start and was discarded.
    // Prefer that successor over reusing the preceding completed window.
    const causalSuccessors = started.filter((window) => (
      window.runId === completed.runId
      && window.requestSequence > completed.requestSequence
    ));
    if (causalSuccessors.length > 0) return causalSuccessors.reduce(laterStarted);
    return completed;
  }
  return latestStarted;
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

  const turns: LedgerTurnModel[] = [];
  let index = 0;
  let turnNo = 0;
  let current: LedgerTurnModel | null = null;

  const pushCell = (turn: LedgerTurnModel, cell: Omit<LedgerCell, "index">): LedgerCell => {
    const placed = { ...cell, index: ++index } as LedgerCell;
    turn.cells.push(placed);
    if (cell.startedAt != null && (turn.startAt === null || cell.startedAt < turn.startAt)) turn.startAt = cell.startedAt;
    const endGuess = cell.endedAt
      ?? (cell.startedAt != null && cell.timeSeconds != null ? cell.startedAt + cell.timeSeconds * 1000 : null);
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
        endedAt: message.timestamp ?? null,
        ...(message.attachments && message.attachments.length > 0
          ? { preview: `${message.attachments.length} attachment(s)` }
          : {}),
      });
    } else if (message.role === "assistant") {
      const turnModel = current ?? (turns.length > 0 ? turns[turns.length - 1] : null);
      if (turnModel === null) continue;
      // Completed messages prefer a completed window associated with their
      // timestamp. This prevents a later concurrent run that is still open
      // from stealing the assistant record merely because it started later.
      const window = selectStepWindow(stepWindows, message);
      const timing = window;
      const usage = window?.usage ?? null;
      if (window !== undefined && usage !== null && turnModel.usage === null) {
        // First assistant message of this ledger turn: sum usage of every
        // step window that starts within the turn (user prompt → next prompt).
        const turnStart = turnModel.startAt ?? window.start;
        const nextTurn = turns.find((t) => t.n > turnModel.n && t.n !== -1);
        const turnEnd = nextTurn?.startAt ?? Number.POSITIVE_INFINITY;
        const sum = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
        for (const w of stepWindows) {
          if (w.runId !== window.runId || w.usage === null) continue;
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
      const stepStart = timing?.start ?? null;
      // Persisted assistant message timestamps are commit/end stamps. Live
      // trace windows provide their own completion event and take precedence.
      const completedAt = timing?.completed
        ?? (timing === undefined ? message.timestamp ?? null : null);
      // The thinking block and the visible output are sequential halves of one
      // step. Both rows previously anchored at step start, so they rendered
      // identical "Started" stamps and overlapping timeline spans. Anchor the
      // assistant row where thinking closed (visible output begins there).
      const thinkingEnd = timing?.thinkingEnd ?? timing?.firstToken ?? null;
      const assistantStart = timing === undefined ? null : thinkingEnd ?? stepStart;
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
        pushCell(turnModel, {
          kind: "thinking",
          text: preview(message.thinking, 200) || "(empty thinking)",
          thinkingDetail: message.thinking,
          recordId: `thinking\u0000${message.id}`,
          timeSeconds: stepStart !== null && thinkingEnd !== null && thinkingEnd >= stepStart
            ? (thinkingEnd - stepStart) / 1000
            : null,
          startedAt: stepStart,
          endedAt: thinkingEnd,
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
        timeSeconds: assistantStart !== null && completedAt !== null && completedAt >= assistantStart
          ? (completedAt - assistantStart) / 1000
          : null,
        startedAt: assistantStart,
        endedAt: completedAt,
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
        endedAt: message.timestamp != null && message.durationMs != null && message.durationMs >= 0
          ? message.timestamp + message.durationMs
          : null,
        timeSeconds: message.durationMs != null && message.durationMs >= 0
          ? message.durationMs / 1000
          : null,
      });
    }
  }

  // Pair execution endpoints in two passes. Trace sequence numbers are only
  // comparable inside a run, and tool ids can be reused by later runs, so a
  // bare tool_use_id map can join unrelated executions or miss a finish that
  // arrived in the input array before its start.
  type ToolTraceTiming = {
    key: string;
    runId: string;
    callId: string;
    toolName?: string;
    start: number | null;
    end: number | null;
    elapsed: number | null;
  };
  type ToolTraceGroup = {
    runId: string;
    callId: string;
    toolName?: string;
    starts: TraceEntry[];
    finishes: TraceEntry[];
  };
  const toolTraceGroups = new Map<string, ToolTraceGroup>();
  for (const entry of traceEntries) {
    if (entry.kind !== "tool_execution_started" && entry.kind !== "tool_execution_finished") continue;
    const ev = entry.details as Record<string, unknown> | undefined;
    const callId = typeof ev?.tool_use_id === "string" ? ev.tool_use_id : null;
    if (callId === null) continue;
    const key = `${entry.runId}\u0000${callId}`;
    let group = toolTraceGroups.get(key);
    if (group === undefined) {
      group = { runId: entry.runId, callId, starts: [], finishes: [] };
      toolTraceGroups.set(key, group);
    }
    if (typeof ev?.tool_name === "string") group.toolName = ev.tool_name;
    if (entry.kind === "tool_execution_started") group.starts.push(entry);
    else group.finishes.push(entry);
  }

  const traceOrder = (a: TraceEntry, b: TraceEntry): number => (
    a.sequence - b.sequence || a.timestampMs - b.timestampMs || a.id.localeCompare(b.id)
  );
  const toolTimings: ToolTraceTiming[] = [];
  for (const [key, group] of toolTraceGroups) {
    const startEntry = [...group.starts].sort(traceOrder)[0];
    const finishes = [...group.finishes].sort(traceOrder);
    // Input arrival order is irrelevant. Within one run, the matching finish
    // is the first causal finish at/after the start sequence.
    const finishEntry = startEntry === undefined
      ? finishes[0]
      : finishes.find((entry) => entry.sequence >= startEntry.sequence);
    const rawElapsed = finishEntry === undefined
      ? null
      : finiteMs((finishEntry.details as Record<string, unknown> | undefined)?.elapsed_ms);
    const elapsed = rawElapsed !== null && rawElapsed >= 0 ? rawElapsed : null;
    let start = startEntry?.timestampMs ?? null;
    let end = finishEntry?.timestampMs ?? null;
    if (start === null && end !== null && elapsed !== null) start = end - elapsed;
    // Keep real event endpoints when sane. If provider clocks put the finish
    // before the start, elapsed_ms is the only safe way to reconstruct an end.
    if (start !== null && end !== null && end < start) {
      end = elapsed === null ? null : start + elapsed;
    }
    toolTimings.push({
      key,
      runId: group.runId,
      callId: group.callId,
      toolName: group.toolName,
      start,
      end,
      elapsed: elapsed ?? (start !== null && end !== null ? end - start : null),
    });
  }

  // A ledger turn has no run id on persisted messages. Bind scoped tool traces
  // through the turn's wall-clock window; when timestamps are absent, only a
  // globally unique candidate is safe. This prevents a reused id in run B from
  // rewriting the cell (or running barrier) belonging to run A.
  type TurnBounds = { start: number | null; end: number | null };
  const realTurns = turns.filter((turn) => turn.n !== -1);
  const turnBounds = new Map<LedgerTurnModel, TurnBounds>();
  for (let i = 0; i < realTurns.length; i += 1) {
    const turn = realTurns[i];
    const userStart = turn.cells.find((cell) => cell.kind === "user")?.startedAt ?? turn.startAt;
    const next = realTurns[i + 1];
    const nextStart = next?.cells.find((cell) => cell.kind === "user")?.startedAt ?? next?.startAt ?? null;
    turnBounds.set(turn, { start: userStart ?? null, end: nextStart });
  }
  const traceBelongsToTurn = (timing: ToolTraceTiming, bounds: TurnBounds | undefined): boolean => {
    if (bounds === undefined) return false;
    const anchor = timing.start ?? timing.end;
    if (anchor === null) return false;
    if (bounds.start !== null && anchor < bounds.start) return false;
    if (bounds.end !== null && anchor >= bounds.end) return false;
    return true;
  };
  const timingsByCallId = new Map<string, ToolTraceTiming[]>();
  for (const timing of toolTimings) {
    timingsByCallId.set(timing.callId, [...(timingsByCallId.get(timing.callId) ?? []), timing]);
  }
  const usedToolTimingKeys = new Set<string>();
  const matchedToolTimingByCell = new Map<LedgerCell, ToolTraceTiming>();

  for (const turn of realTurns) {
    const bounds = turnBounds.get(turn);
    for (const cell of turn.cells) {
      if (cell.kind !== "tool" || cell.callId === undefined) continue;
      const available = (timingsByCallId.get(cell.callId) ?? [])
        .filter((timing) => !usedToolTimingKeys.has(timing.key));
      let candidates = available.filter((timing) => traceBelongsToTurn(timing, bounds));
      const hasTurnBoundary = bounds !== undefined && (bounds.start !== null || bounds.end !== null);
      if (candidates.length === 0 && available.length === 1 && !hasTurnBoundary) {
        candidates = available;
      }
      if (candidates.length === 0) continue;
      const cellAnchor = cell.startedAt ?? cell.endedAt ?? null;
      candidates.sort((a, b) => {
        const aAnchor = a.start ?? a.end;
        const bAnchor = b.start ?? b.end;
        const aDistance = cellAnchor === null || aAnchor === null ? 0 : Math.abs(aAnchor - cellAnchor);
        const bDistance = cellAnchor === null || bAnchor === null ? 0 : Math.abs(bAnchor - cellAnchor);
        return aDistance - bDistance
          || (aAnchor ?? Number.POSITIVE_INFINITY) - (bAnchor ?? Number.POSITIVE_INFINITY)
          || a.runId.localeCompare(b.runId);
      });
      const timing = candidates[0];
      usedToolTimingKeys.add(timing.key);
      matchedToolTimingByCell.set(cell, timing);

      if (timing.start !== null) cell.startedAt = timing.start;
      if (timing.end !== null) {
        cell.endedAt = timing.end;
      } else if (timing.start !== null && cell.timeSeconds !== null && cell.timeSeconds >= 0) {
        // The message result supplies a known duration even if its trace finish
        // was evicted; anchor that duration at the exact trace start.
        cell.endedAt = timing.start + cell.timeSeconds * 1000;
      }
      if (timing.elapsed !== null) {
        cell.timeSeconds = timing.elapsed / 1000;
      } else if (cell.startedAt != null && cell.endedAt != null && cell.endedAt >= cell.startedAt) {
        cell.timeSeconds = (cell.endedAt - cell.startedAt) / 1000;
      }
      if (cell.startedAt != null && (turn.startAt === null || cell.startedAt < turn.startAt)) {
        turn.startAt = cell.startedAt;
      }
      if (cell.endedAt != null && cell.endedAt > (turn.endAt ?? 0)) turn.endAt = cell.endedAt;
    }

    // Replayed sibling tools can inherit one identical synthetic interval.
    // Keep those raw endpoints untouched for Actual; Time gets a projection-
    // only partition so every sibling remains visible instead of overpainting.
    const syntheticGroups = new Map<string, LedgerCell[]>();
    for (const cell of turn.cells) {
      if (cell.kind !== "tool" || matchedToolTimingByCell.has(cell) || cell.startedAt == null) continue;
      const rawEnd = cell.endedAt
        ?? (cell.timeSeconds !== null && cell.timeSeconds >= 0
          ? cell.startedAt + cell.timeSeconds * 1000
          : null);
      if (rawEnd === null) continue;
      const key = `${cell.startedAt}\u0000${rawEnd}`;
      syntheticGroups.set(key, [...(syntheticGroups.get(key) ?? []), cell]);
    }
    for (const siblings of syntheticGroups.values()) {
      if (siblings.length < 2) continue;
      siblings.sort((a, b) => a.index - b.index);
      const start = siblings[0].startedAt as number;
      const end = siblings[0].endedAt
        ?? start + (siblings[0].timeSeconds ?? 0) * 1000;
      const span = end - start;
      siblings.forEach((cell, siblingIndex) => {
        if (span > 0) {
          cell.timelineStartedAt = start + span * siblingIndex / siblings.length;
          cell.timelineEndedAt = start + span * (siblingIndex + 1) / siblings.length;
        } else {
          // Distinct one-millisecond point anchors remain minimum-width markers
          // and avoid collapsing zero-duration siblings onto one pixel.
          cell.timelineStartedAt = start + siblingIndex;
          cell.timelineEndedAt = start + siblingIndex;
        }
      });
    }
  }

  // Merge exact trace-only tools into message order. Appending these records
  // afterward would break causal Sequence order for the following assistant.
  for (const timing of toolTimings) {
    const anchor = timing.start ?? timing.end;
    if (anchor === null) continue;
    const owner = realTurns.find((turn) => traceBelongsToTurn(timing, turnBounds.get(turn)))
      ?? findTurnContainingAt(realTurns, anchor);
    if (owner === null) continue;
    const alreadyProjected = owner.cells.some((cell) => {
      const matched = matchedToolTimingByCell.get(cell);
      return matched?.key === timing.key
        || (matched === undefined && cell.callId === timing.callId);
    });
    if (alreadyProjected) continue;

    const insertAt = owner.cells.findIndex((cell) => {
      const cellAnchor = cell.startedAt ?? cell.endedAt;
      return cellAnchor != null && cellAnchor > anchor;
    });
    const position = insertAt < 0 ? owner.cells.length : insertAt;
    const placed = pushCell(owner, {
      kind: "tool",
      text: `${timing.toolName ?? "Tool"}${timing.end === null ? " (running)" : ""}`,
      callId: timing.callId,
      recordId: `trace-tool\u0000${timing.runId}\u0000${timing.callId}`,
      timeSeconds: timing.elapsed === null ? null : timing.elapsed / 1000,
      startedAt: timing.start,
      endedAt: timing.end,
    });
    owner.cells.pop();
    owner.cells.splice(position, 0, placed);
    usedToolTimingKeys.add(timing.key);
    matchedToolTimingByCell.set(placed, timing);
  }

  // Persisted assistant timestamps are commit/end stamps only. Without a
  // matching live trace window, keep thinking/model starts and durations
  // unknown instead of converting message gaps or user idle into latency.

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
      endedAt: entry.timestampMs,
    };
    // Compaction is a session-level event, not part of any single turn — it
    // always lands in the "Between turns" pseudo-turn.
    const target = ensureBetweenTurns(turns);
    target.cells.push({ ...cell, index: ++index });
    if (target.startAt === null || entry.timestampMs < target.startAt) target.startAt = entry.timestampMs;
    if (entry.timestampMs > (target.endAt ?? 0)) target.endAt = entry.timestampMs;
  }

  // Trace-only insertion can place a newly allocated cell before an existing
  // one. Re-number once after all auxiliary records are attached so row order,
  // Sequence order, and displayed #N remain identical.
  let finalIndex = 0;
  for (const turn of turns) {
    for (const cell of turn.cells) cell.index = ++finalIndex;
  }

  return { turns, total: finalIndex };
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
