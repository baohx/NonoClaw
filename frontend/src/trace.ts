import type { EngineEvent, EventMsg, RunWireMeta } from "./types";

export const MAX_TRACE_EVENTS = 600;
export const MAX_RENDERED_TRACE_EVENTS = 140;
const MAX_DETAIL_CHARS = 320;

export type TraceCategory =
  | "lifecycle" | "context" | "model" | "provider" | "tool"
  | "permission" | "hook" | "subagent" | "background" | "session"
  | "extension" | "config" | "usage" | "error";
export type TraceStatus = "active" | "success" | "failure" | "cancel" | "waiting" | "warning" | "info";
export type TraceDetail = string | number | boolean | null;

export interface TraceEntry {
  id: string;
  runId: string;
  parentRunId?: string;
  sessionId: string;
  sequence: number;
  timestampMs: number;
  category: TraceCategory;
  status: TraceStatus;
  kind: EngineEvent["kind"] | "done" | "wire_error";
  summary: string;
  details: Record<string, TraceDetail>;
}

export interface TraceRun {
  runId: string;
  parentRunId?: string;
  sessionId: string;
  entries: TraceEntry[];
  startedAt: number;
  updatedAt: number;
  status: TraceStatus;
}

const OMITTED_KINDS = new Set<EngineEvent["kind"]>(["text_delta", "thinking_delta", "assistant_done"]);
const SENSITIVE_KEY = /(authorization|credential|secret|token$|api[_-]?key|password|prompt|input|preview|attachment|content|text)/i;

function text(value: unknown, fallback = ""): string {
  if (typeof value !== "string") return fallback;
  const oneLine = value.replace(/[\r\n\t]+/g, " ").trim();
  return oneLine.length > MAX_DETAIL_CHARS ? `${oneLine.slice(0, MAX_DETAIL_CHARS)}…` : oneLine;
}
function number(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}
function bool(value: unknown): boolean { return value === true; }
function status(value: unknown, fallback: TraceStatus = "info"): TraceStatus {
  switch (value) {
    case "pending": case "running": return "active";
    case "waiting": return "waiting";
    case "allowed": case "succeeded": case "completed": return "success";
    case "denied": case "failed": case "interrupted": return "failure";
    case "cancelled": return "cancel";
    case "truncated": case "repaired": return "warning";
    default: return fallback;
  }
}
function detail(values: Record<string, unknown>): Record<string, TraceDetail> {
  const safe: Record<string, TraceDetail> = {};
  for (const [key, value] of Object.entries(values)) {
    if (value === undefined || SENSITIVE_KEY.test(key)) continue;
    if (typeof value === "string") safe[key] = text(value);
    else if (typeof value === "number" && Number.isFinite(value)) safe[key] = value;
    else if (typeof value === "boolean") safe[key] = value;
    else if (value === null) safe[key] = null;
  }
  return safe;
}
function usageDetails(value: unknown, prefix: string): Record<string, TraceDetail> {
  if (!value || typeof value !== "object") return {};
  const raw = value as Record<string, unknown>;
  return detail({
    [`${prefix}_in`]: raw.input_tokens,
    [`${prefix}_out`]: raw.output_tokens,
    [`${prefix}_cache_read`]: raw.cache_read_input_tokens,
    [`${prefix}_cache_write`]: raw.cache_creation_input_tokens,
  });
}
function compactNumber(value: unknown): string { return number(value).toLocaleString(); }
function budgetTop(value: unknown, limit = 5): string {
  if (!Array.isArray(value)) return "";
  return value
    .filter((item): item is { name: string; chars: number } => Boolean(item)
      && typeof item === "object"
      && typeof (item as Record<string, unknown>).name === "string"
      && typeof (item as Record<string, unknown>).chars === "number")
    .sort((a, b) => b.chars - a.chars || a.name.localeCompare(b.name))
    .slice(0, limit)
    .map((item) => `${item.name}:${item.chars}`)
    .join(" · ");
}

/** Calculate context usage whenever a window is known, independently of UI visibility. */
export function contextUsagePercent(estimated: unknown, window: unknown): number | null {
  if (typeof window !== "number" || !Number.isFinite(window) || window < 0) return null;
  if (window === 0) return 0;
  const used = typeof estimated === "number" && Number.isFinite(estimated) ? Math.max(0, estimated) : 0;
  return Math.round(Math.min(100, used / window * 100) * 10) / 10;
}

interface Fact {
  category: TraceCategory;
  status: TraceStatus;
  summary: string;
  details?: Record<string, TraceDetail>;
}

/** Convert a RunEvent into a strictly whitelisted fact. Raw payloads are never retained. */
export function eventToSafeFact(event: EngineEvent): Fact | null {
  if (OMITTED_KINDS.has(event.kind)) return null;
  switch (event.kind) {
    case "run_started": return { category: "lifecycle", status: "active", summary: `Run started · ${text(event.requested_model, "model pending")}`, details: detail({ requested_model: event.requested_model, max_turns: event.max_turns, max_budget_usd: event.max_budget_usd }) };
    case "context_prepared": return { category: "context", status: "success", summary: `Context ${compactNumber(event.estimated_tokens)} / ${event.context_window ? compactNumber(event.context_window) : "?"} tokens`, details: detail({ estimated_tokens: event.estimated_tokens, context_window: event.context_window, tool_count: event.tool_count, skill_count: event.skill_count }) };
    case "token_budget_breakdown": return {
      category: "context",
      status: "info",
      summary: `Payload · system ${compactNumber(event.system_chars)} · tools ${compactNumber(event.tools_chars)} · messages ${compactNumber(event.messages_chars)} chars`,
      details: detail({
        estimated_tokens: event.estimated_tokens,
        chars_per_token: event.chars_per_token,
        system_chars: event.system_chars,
        tools_chars: event.tools_chars,
        messages_chars: event.messages_chars,
        system_top: budgetTop(event.system),
        tools_top: budgetTop(event.tools),
        messages_top: budgetTop(event.messages),
      }),
    };
    case "model_request_started": return { category: "model", status: "active", summary: `Turn ${number(event.turn)} · requesting ${text(event.requested_model, "model")}`, details: detail({ requested_model: event.requested_model, provider: event.provider, turn: event.turn }) };
    case "model_resolved": return { category: "model", status: "success", summary: `${text(event.requested_model, "requested")} → ${text(event.actual_model, "actual")}`, details: detail({ requested_model: event.requested_model, actual_model: event.actual_model, provider: event.provider, turn: event.turn }) };
    case "model_info": return { category: "model", status: "success", summary: `Actual model · ${text(event.model, "unknown")}`, details: detail({ model: event.model }) };
    case "provider_diagnostic": return { category: "provider", status: status(event.status), summary: `${text(event.provider, "provider")} · ${text(event.category, "diagnostic")}`, details: detail({ provider: event.provider, category: event.category, status: event.status, detail: event.detail }) };
    case "stream_state_changed": return { category: "model", status: status(event.state, event.state === "thinking" || event.state === "streaming" ? "active" : "info"), summary: `Stream ${text(event.state, "changed")} · turn ${number(event.turn)}`, details: detail({ state: event.state, turn: event.turn }) };
    case "thinking_state": {
      // The engine emits active:true before every ThinkingDelta. The delta is
      // intentionally omitted from trace, so retaining each matching state
      // pulse can evict the model_request_started anchor during a long
      // reasoning stream. The close transition is the timing fact Ledger
      // needs; live UI/breath handling still consumes every raw event.
      if (bool(event.active)) return null;
      return { category: "model", status: "success", summary: `Thinking complete · turn ${number(event.turn)}`, details: detail({ active: false, turn: event.turn }) };
    }
    case "retry_scheduled": return { category: "provider", status: "waiting", summary: `Retry ${number(event.attempt)} in ${number(event.delay_ms)} ms`, details: detail({ attempt: event.attempt, delay_ms: event.delay_ms, category: event.category, operation: event.operation }) };
    case "tool_use_start": return { category: "tool", status: "active", summary: `${text(event.name, "Tool")} started`, details: detail({ tool_use_id: event.id, tool_name: event.name }) };
    case "tool_result": return { category: "tool", status: bool(event.ok) ? "success" : "failure", summary: `${bool(event.ok) ? "Tool succeeded" : "Tool failed"} · ${text(event.id, "unknown")}`, details: detail({ tool_use_id: event.id, ok: event.ok }) };
    case "tool_queued": return { category: "tool", status: "active", summary: `${text(event.tool_name, "Tool")} queued`, details: detail({ tool_use_id: event.tool_use_id, tool_name: event.tool_name, index: event.index }) };
    case "tool_validation": return { category: "tool", status: bool(event.ok) ? "success" : "failure", summary: `${text(event.tool_name, "Tool")} validation ${bool(event.ok) ? "passed" : "failed"}`, details: detail({ tool_use_id: event.tool_use_id, tool_name: event.tool_name, ok: event.ok, detail: event.detail }) };
    case "tool_execution_started": return { category: "tool", status: "active", summary: `${text(event.tool_name, "Tool")} running`, details: detail({ tool_use_id: event.tool_use_id, tool_name: event.tool_name, read_only: event.read_only, destructive: event.destructive }) };
    case "tool_execution_finished": return { category: "tool", status: status(event.status), summary: `${text(event.tool_name, "Tool")} · ${text(event.status, "finished")}`, details: detail({ tool_use_id: event.tool_use_id, tool_name: event.tool_name, status: event.status, elapsed_ms: event.elapsed_ms }) };
    case "tool_result_normalized": return { category: "tool", status: bool(event.truncated) ? "warning" : "success", summary: bool(event.truncated) ? `Tool result truncated · ${compactNumber(event.visible_chars)} visible chars` : `Tool result normalized · ${compactNumber(event.visible_chars)} chars`, details: detail({ tool_use_id: event.tool_use_id, original_chars: event.original_chars, visible_chars: event.visible_chars, truncated: event.truncated }) };
    case "permission_requested": return { category: "permission", status: "waiting", summary: `Waiting for ${text(event.waiting_on, "permission")} · ${text(event.tool_name, "tool")}`, details: detail({ tool_use_id: event.tool_use_id, tool_name: event.tool_name, waiting_on: event.waiting_on }) };
    case "permission_resolved": return { category: "permission", status: status(event.decision), summary: `Permission ${text(event.decision, "resolved")} · ${text(event.tool_name, "tool")}`, details: detail({ tool_use_id: event.tool_use_id, tool_name: event.tool_name, decision: event.decision, elapsed_ms: event.elapsed_ms }) };
    case "hook_started": return { category: "hook", status: "active", summary: `${text(event.hook_type, "Hook")} · ${text(event.action, "action")}`, details: detail({ hook_type: event.hook_type, action: event.action, matcher: event.matcher }) };
    case "hook_finished": return { category: "hook", status: status(event.status), summary: `${text(event.hook_type, "Hook")} · ${text(event.status, "finished")}`, details: detail({ hook_type: event.hook_type, action: event.action, matcher: event.matcher, status: event.status, elapsed_ms: event.elapsed_ms }) };
    case "subagent_started": return { category: "subagent", status: "active", summary: `Subagent started · ${text(event.description, "delegated task")}`, details: detail({ description: event.description }) };
    case "subagent_finished": return { category: "subagent", status: status(event.status), summary: `Subagent ${text(event.status, "finished")} · ${text(event.description, "delegated task")}`, details: detail({ description: event.description, status: event.status, elapsed_ms: event.elapsed_ms }) };
    case "background_task_changed": return { category: "background", status: status(event.status), summary: `Background ${text(event.task_id, "task")} · ${text(event.status, "changed")}`, details: detail({ task_id: event.task_id, status: event.status, exit_code: event.exit_code }) };
    case "compacting": return { category: "context", status: "active", summary: "Context compaction started" };
    case "compaction_started": return { category: "context", status: "active", summary: `${bool(event.automatic) ? "Automatic" : "Manual"} compaction · ${compactNumber(event.tokens_before)} tokens`, details: detail({ automatic: event.automatic, tokens_before: event.tokens_before, messages_before: event.messages_before }) };
    case "compacted": return (event.removed ?? 0) > 0 ? { category: "context", status: "success", summary: `Compacted ${compactNumber(event.tokens_before)} → ${compactNumber(event.tokens_after)} tokens`, details: detail({ removed: event.removed, kept: event.kept, tokens_before: event.tokens_before, tokens_after: event.tokens_after }) } : null;
    case "recovery_applied": return { category: "session", status: "warning", summary: `Recovery applied · ${text(event.category, "session")}`, details: detail({ category: event.category, detail: event.detail, items_affected: event.items_affected }) };
    case "session_repair": {
      const repair = event.repair && typeof event.repair === "object" ? event.repair as Record<string, unknown> : {};
      return { category: "session", status: "warning", summary: `Session repaired · ${text(repair.kind, "data")}`, details: detail({ kind: repair.kind, line: repair.line, detail: repair.detail }) };
    }
    case "skill_activated": return { category: "extension", status: "success", summary: `Skill /${text(event.name, "unknown")} activated`, details: detail({ name: event.name, reason: event.reason, source: event.source, version: event.version }) };
    case "extension_diagnostic": {
      const diagnostic = event.diagnostic && typeof event.diagnostic === "object" ? event.diagnostic as Record<string, unknown> : {};
      return { category: "extension", status: diagnostic.severity === "error" ? "failure" : "warning", summary: `${text(diagnostic.kind, "Extension")} · ${text(diagnostic.name, text(diagnostic.code, "diagnostic"))}`, details: detail({ severity: diagnostic.severity, code: diagnostic.code, kind: diagnostic.kind, name: diagnostic.name, source: diagnostic.source, message: diagnostic.message, suggestion: diagnostic.suggestion }) };
    }
    case "mcp_diagnostic": return { category: "extension", status: status(event.status), summary: `MCP ${text(event.server, "server")} · ${text(event.status, "diagnostic")}`, details: detail({ server: event.server, status: event.status, source: event.source, detail: event.detail }) };
    case "config_diagnostic": return { category: "config", status: event.severity === "error" ? "failure" : "warning", summary: `Config ${text(event.code, "diagnostic")} · ${text(event.field, "general")}`, details: detail({ severity: event.severity, code: event.code, field: event.field, source: event.source, message: event.message, suggestion: event.suggestion }) };
    case "usage_updated": return { category: "usage", status: "info", summary: `Usage updated · turn ${number(event.turn)}`, details: { ...usageDetails(event.turn_usage, "turn"), ...usageDetails(event.total, "total"), ...detail({ turn: event.turn, max_budget_usd: event.max_budget_usd }) } };
    case "task_changed": return { category: "lifecycle", status: "info", summary: "Task state changed", details: detail({ scope: event.change?.scope, source: event.change?.source, change: event.change?.change, task_count: event.change?.tasks?.length }) };
    case "cancellation_requested": return { category: "lifecycle", status: "cancel", summary: `Cancellation requested · ${text(event.reason, "user request")}`, details: detail({ reason: event.reason }) };
    case "run_error": return { category: "error", status: "failure", summary: `${text(event.operation, "Run")} failed · ${text(event.message, event.code ? String(event.code) : "error")}`, details: detail({ code: event.code, operation: event.operation, retryable: event.retryable, message: event.message }) };
    case "run_finished": return { category: "lifecycle", status: status(event.status), summary: `Run ${text(event.status, "finished")} · ${text(event.reason, "complete")}`, details: { ...usageDetails(event.usage, "total"), ...detail({ status: event.status, reason: event.reason, duration_ms: event.duration_ms, turns: event.turns }) } };
    default: return null;
  }
}

export function traceEntryFromEvent(message: EventMsg): TraceEntry | null {
  const fact = eventToSafeFact(message.event);
  if (!fact) return null;
  return {
    id: message.event_id || `${message.run_id ?? "legacy"}:${message.sequence ?? 0}:${message.event.kind}`,
    runId: message.run_id || "legacy-run",
    parentRunId: message.parent_run_id,
    sessionId: message.session_id || "legacy-session",
    sequence: message.sequence ?? 0,
    timestampMs: message.timestamp_ms ?? Date.now(),
    kind: message.event.kind,
    ...fact,
    details: fact.details ?? {},
  };
}

export function traceTerminalEntry(meta: RunWireMeta, kind: "done" | "wire_error", values: Record<string, unknown>): TraceEntry {
  const failed = kind === "wire_error";
  return {
    id: `${meta.run_id ?? "legacy"}:${meta.sequence ?? 0}:${kind}`,
    runId: meta.run_id || "legacy-run",
    sessionId: meta.session_id || "legacy-session",
    sequence: meta.sequence ?? Number.MAX_SAFE_INTEGER,
    timestampMs: meta.timestamp_ms ?? Date.now(),
    category: failed ? "error" : "lifecycle",
    status: failed ? "failure" : values.stop_reason === "cancelled" ? "cancel" : "success",
    kind,
    summary: failed ? `Run failed · ${text(values.message, "error")}` : `Run complete · ${text(values.stop_reason, "finished")}`,
    details: failed ? detail({ message: values.message }) : { ...usageDetails(values.usage, "total"), ...detail({ turns: values.turns, stop_reason: values.stop_reason }) },
  };
}

/** Stable per-run/turn key for high-frequency stream state transitions. */
function traceTurnKey(entry: TraceEntry): string {
  const turn = entry.details.turn;
  return `${entry.runId}\u0000${typeof turn === "number" || typeof turn === "string" ? String(turn) : "?"}`;
}

/** Remove wire-level state pulses that carry no new trace information. */
function compactTracePulses(entries: TraceEntry[]): TraceEntry[] {
  const compacted: TraceEntry[] = [];
  const latestStreamState = new Map<string, TraceDetail>();
  const thinkingClosed = new Set<string>();
  for (const entry of entries) {
    const key = traceTurnKey(entry);
    if (entry.kind === "thinking_state") {
      if (entry.details.active === true) continue;
      // ThinkingEnd and MessageStop may both emit active:false. Ledger uses the
      // first close because it is the precise block boundary.
      if (thinkingClosed.has(key)) continue;
      thinkingClosed.add(key);
    } else if (entry.kind === "stream_state_changed") {
      const state = entry.details.state;
      if (state === "streaming" && latestStreamState.get(key) === "streaming") continue;
      latestStreamState.set(key, state ?? null);
    }
    compacted.push(entry);
  }
  return compacted;
}

function isTimingBoundary(entry: TraceEntry): boolean {
  if (entry.kind === "model_request_started"
    || entry.kind === "usage_updated"
    || entry.kind === "retry_scheduled"
    || entry.kind === "tool_use_start"
    || entry.kind === "tool_execution_started"
    || entry.kind === "tool_execution_finished"
    || entry.kind === "run_finished"
    || entry.kind === "done"
    || entry.kind === "wire_error") return true;
  if (entry.kind === "thinking_state") return entry.details.active === false;
  return entry.kind === "stream_state_changed"
    && (entry.details.state === "streaming"
      || entry.details.state === "completed"
      || entry.details.state === "interrupted");
}

function closesModelWindow(entry: TraceEntry): boolean {
  if (entry.kind === "tool_use_start"
    || entry.kind === "run_finished"
    || entry.kind === "done"
    || entry.kind === "wire_error") return true;
  return entry.kind === "stream_state_changed"
    && (entry.details.state === "completed" || entry.details.state === "interrupted");
}

/**
 * Retain newest arrival-ordered facts while pinning the model-request start of
 * every represented per-run window. This avoids keeping orphan close/end facts
 * that buildStepWindows cannot interpret after a high-volume stream.
 */
export function trimTraceEntries(entries: TraceEntry[]): TraceEntry[] {
  if (entries.length <= MAX_TRACE_EVENTS) return entries;

  const startForEntry = new Map<string, TraceEntry>();
  const currentStartByRun = new Map<string, TraceEntry>();
  const openStartByRun = new Map<string, TraceEntry>();
  for (const entry of entries) {
    if (entry.kind === "model_request_started") {
      currentStartByRun.set(entry.runId, entry);
      openStartByRun.set(entry.runId, entry);
    }
    const start = currentStartByRun.get(entry.runId);
    if (start !== undefined) startForEntry.set(entry.id, start);
    if (closesModelWindow(entry)) openStartByRun.delete(entry.runId);
  }

  const kept = entries.slice(-MAX_TRACE_EVENTS);
  // Open windows must retain their start before any close exists to pull it
  // back into the bounded tail. Once closed, represented endpoint facts below
  // keep requiring the same start.
  const requiredStarts = new Map<string, TraceEntry>();
  for (const start of openStartByRun.values()) requiredStarts.set(start.id, start);
  for (const entry of kept) {
    const start = startForEntry.get(entry.id);
    if (start !== undefined) requiredStarts.set(start.id, start);
  }
  const keptIds = new Set(kept.map((entry) => entry.id));
  const newestEntryId = entries[entries.length - 1]?.id;
  for (const start of requiredStarts.values()) {
    if (keptIds.has(start.id)) continue;
    let evictAt = kept.findIndex((entry) => (
      entry.id !== newestEntryId
      && !requiredStarts.has(entry.id)
      && !isTimingBoundary(entry)
    ));
    // If every older candidate is itself a boundary, discard the oldest
    // non-required boundary rather than rejecting the newly arrived fact.
    // This can leave an old window start-only (honest unknown end), but never
    // starves a newer run or manufactures an orphan end without its start.
    if (evictAt < 0) evictAt = kept.findIndex((entry) => (
      entry.id !== newestEntryId && !requiredStarts.has(entry.id)
    ));
    if (evictAt < 0) break;
    keptIds.delete(kept[evictAt].id);
    kept[evictAt] = start;
    keptIds.add(start.id);
  }

  const arrivalOrder = new Map(entries.map((entry, position) => [entry.id, position]));
  kept.sort((a, b) => (arrivalOrder.get(a.id) ?? 0) - (arrivalOrder.get(b.id) ?? 0));
  return kept;
}

/** Build the unbounded, low-frequency fact stream used only by trajectory
 * analysis. TechnicalTrace keeps its separate bounded list, so preserving all
 * model/tool boundaries here does not make the live diagnostics rail grow. */
export function trajectoryTraceEntries(entries: TraceEntry[]): TraceEntry[] {
  return compactTracePulses(entries.filter(isTimingBoundary));
}

/** Add one live fact to the trajectory stream without applying the diagnostic
 * trace cap. Event ids provide deterministic replay/live de-duplication. */
export function appendTrajectoryTraceEntry(entries: TraceEntry[], entry: TraceEntry): TraceEntry[] {
  if (!isTimingBoundary(entry) || entries.some((existing) => existing.id === entry.id)) return entries;
  return compactTracePulses([...entries, entry]);
}

/** Append with deterministic de-duplication and bounded arrival retention. */
export function appendTraceEntry(entries: TraceEntry[], entry: TraceEntry): TraceEntry[] {
  if (entries.some((existing) => existing.id === entry.id)) return entries;
  return trimTraceEntries(compactTracePulses([...entries, entry]));
}

export function groupTraceRuns(entries: TraceEntry[]): TraceRun[] {
  const groups = new Map<string, TraceEntry[]>();
  for (const entry of entries) groups.set(entry.runId, [...(groups.get(entry.runId) ?? []), entry]);
  return [...groups.entries()].map(([runId, runEntries]) => {
    runEntries.sort((a, b) => a.sequence - b.sequence || a.timestampMs - b.timestampMs);
    const last = runEntries[runEntries.length - 1];
    return {
      runId,
      parentRunId: runEntries.find((entry) => entry.parentRunId)?.parentRunId,
      sessionId: last.sessionId,
      entries: runEntries,
      startedAt: runEntries[0].timestampMs,
      updatedAt: last.timestampMs,
      status: last.status,
    };
  }).sort((a, b) => b.updatedAt - a.updatedAt || a.runId.localeCompare(b.runId));
}

export function exportTraceRun(run: TraceRun): object {
  return {
    format: "nonoclaw-redacted-trace-v1",
    run_id: run.runId,
    parent_run_id: run.parentRunId,
    session_id: run.sessionId,
    started_at_ms: run.startedAt,
    updated_at_ms: run.updatedAt,
    status: run.status,
    events: run.entries.map(({ id, runId: _runId, parentRunId: _parent, sessionId: _session, ...entry }) => ({ event_id: id, ...entry })),
  };
}
