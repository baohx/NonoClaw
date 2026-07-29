import type {
  ChatMessage,
  ClientMsg,
  EngineEvent,
  ScopedSubagentEvent,
  SubagentRun,
  SubagentRunStatus,
  SubagentTool,
} from "../types";
import { sanitizeBrowserText } from "../security";

/** Parent-envelope checks for scoped child events. This intentionally does not
 * advance root ordering: child_sequence owns child idempotency, while these
 * guards prevent late frames from crossing session/snapshot/terminal bounds. */
export interface ScopedEnvelopeGateState {
  sessionId: string;
  sessionRevision: number;
  snapshotRevision: number;
  awaitingSnapshot: boolean;
  terminalRuns: Record<string, true>;
}

export interface ScopedEnvelopeMeta {
  runId?: string;
  sessionId?: string;
  sessionRevision?: number;
  sequence?: number;
}

export function acceptScopedEnvelope(
  state: ScopedEnvelopeGateState,
  meta: ScopedEnvelopeMeta,
): boolean {
  if (!meta.runId || !meta.sessionId
    || !Number.isSafeInteger(meta.sessionRevision)
    || !Number.isSafeInteger(meta.sequence)) return false;
  if (state.awaitingSnapshot || meta.sessionId !== state.sessionId) return false;
  if ((meta.sessionRevision as number) < Math.max(state.snapshotRevision, state.sessionRevision)) return false;
  if ((meta.sequence as number) < 0 || state.terminalRuns[meta.runId]) return false;
  return true;
}

export const MAX_OUTBOUND_QUEUE = 64;
export const MAX_TRACKED_RUNS = 128;
export const MAX_RESOLVED_PROMPTS = 128;

export interface QueuedClientMessage {
  id: number;
  key: string;
  message: ClientMsg;
}

export interface ConnectionState {
  connectionStatus: "connecting" | "connected" | "disconnected";
  connectionGeneration: number;
  outboundQueue: QueuedClientMessage[];
  nextOutboundId: number;
}

export type ConnectionTransition =
  | { type: "begin" }
  | { type: "connected"; generation: number }
  | { type: "disconnected"; generation: number }
  | { type: "cleanup" };

export function transitionConnection(
  state: ConnectionState,
  transition: ConnectionTransition,
): ConnectionState {
  switch (transition.type) {
    case "begin":
      return {
        ...state,
        connectionStatus: "connecting",
        connectionGeneration: state.connectionGeneration + 1,
      };
    case "connected":
      return transition.generation === state.connectionGeneration
        ? { ...state, connectionStatus: "connected" }
        : state;
    case "disconnected":
      return transition.generation === state.connectionGeneration
        ? { ...state, connectionStatus: "disconnected" }
        : state;
    case "cleanup":
      return {
        ...state,
        connectionStatus: "disconnected",
        connectionGeneration: state.connectionGeneration + 1,
        outboundQueue: [],
      };
  }
}

function outboundKey(message: ClientMsg): string {
  switch (message.type) {
    case "file_tree":
    case "project_info_refresh":
    case "cancel":
    case "clear":
    case "compact":
    case "new_session":
      return message.type;
    case "permission_decision":
    case "question_answer":
      return `${message.type}:${message.request_id}`;
    case "resume_session":
      return `${message.type}:${message.id}`;
    case "open_file":
      return `${message.type}:${message.path}:${message.force_code === true}`;
    case "git_show":
      return `${message.type}:${message.sha}`;
    case "set_permission_mode":
      return `${message.type}:${message.mode}`;
    case "set_model":
      return `${message.type}:${message.name}`;
    case "run":
      return `${message.type}:${JSON.stringify(message)}`;
  }
}

export function enqueueClientMessage(
  state: ConnectionState,
  message: ClientMsg,
  limit = MAX_OUTBOUND_QUEUE,
): { state: ConnectionState; entry: QueuedClientMessage; added: boolean } {
  const key = outboundKey(message);
  const duplicate = state.outboundQueue.find((entry) => entry.key === key);
  if (duplicate) return { state, entry: duplicate, added: false };

  const entry = { id: state.nextOutboundId, key, message };
  const boundedLimit = Math.max(1, limit);
  return {
    state: {
      ...state,
      nextOutboundId: state.nextOutboundId + 1,
      outboundQueue: [...state.outboundQueue, entry].slice(-boundedLimit),
    },
    entry,
    added: true,
  };
}

export function acknowledgeClientMessage(
  state: ConnectionState,
  id: number,
): ConnectionState {
  const outboundQueue = state.outboundQueue.filter((entry) => entry.id !== id);
  return outboundQueue.length === state.outboundQueue.length
    ? state
    : { ...state, outboundQueue };
}

export interface SessionOrderingState {
  sessionId: string;
  sessionRevision: number;
  snapshotRevision: number;
  awaitingSnapshot: boolean;
  runSequences: Record<string, number>;
  terminalRuns: Record<string, true>;
  runOrder: string[];
}

export interface RunEnvelopeMeta {
  runId: string;
  sessionId: string;
  sessionRevision: number;
  sequence: number;
}

export function prepareSessionBoundary(
  state: SessionOrderingState,
  sessionId = state.sessionId,
): SessionOrderingState {
  const switched = sessionId !== state.sessionId;
  return {
    sessionId,
    sessionRevision: switched ? -1 : state.sessionRevision,
    snapshotRevision: switched ? -1 : state.snapshotRevision,
    awaitingSnapshot: true,
    runSequences: switched ? {} : state.runSequences,
    terminalRuns: switched ? {} : state.terminalRuns,
    runOrder: switched ? [] : state.runOrder,
  };
}

export function acceptSnapshotTransition(
  state: SessionOrderingState,
  sessionId: string,
  revision: number,
): { accepted: boolean; switched: boolean; state: SessionOrderingState } {
  const switched = sessionId !== state.sessionId;
  if (!switched) {
    if (revision < state.sessionRevision || revision <= state.snapshotRevision) {
      return { accepted: false, switched: false, state };
    }
  }
  return {
    accepted: true,
    switched,
    state: {
      sessionId,
      sessionRevision: revision,
      snapshotRevision: revision,
      awaitingSnapshot: false,
      runSequences: switched ? {} : state.runSequences,
      terminalRuns: switched ? {} : state.terminalRuns,
      runOrder: switched ? [] : state.runOrder,
    },
  };
}

export function acceptLegacySnapshotTransition(
  state: SessionOrderingState,
  hasOptimisticRun: boolean,
): { accepted: boolean; state: SessionOrderingState } {
  if (hasOptimisticRun && !state.awaitingSnapshot) return { accepted: false, state };
  return { accepted: true, state: { ...state, awaitingSnapshot: false } };
}

export function acceptRunTransition(
  state: SessionOrderingState,
  meta: RunEnvelopeMeta,
  terminal: boolean,
  limit = MAX_TRACKED_RUNS,
): { accepted: boolean; state: SessionOrderingState } {
  if ((state.sessionId && meta.sessionId !== state.sessionId)
    || state.awaitingSnapshot
    || meta.sessionRevision < state.sessionRevision
    || state.terminalRuns[meta.runId]
    || meta.sequence <= (state.runSequences[meta.runId] ?? -1)) {
    return { accepted: false, state };
  }

  const isNewRun = state.runSequences[meta.runId] === undefined;
  let runOrder = isNewRun ? [...state.runOrder, meta.runId] : state.runOrder;
  const runSequences = { ...state.runSequences, [meta.runId]: meta.sequence };
  const terminalRuns = terminal
    ? { ...state.terminalRuns, [meta.runId]: true as const }
    : { ...state.terminalRuns };
  const boundedLimit = Math.max(1, limit);
  while (runOrder.length > boundedLimit) {
    const removed = runOrder[0];
    runOrder = runOrder.slice(1);
    delete runSequences[removed];
    delete terminalRuns[removed];
  }

  return {
    accepted: true,
    state: {
      ...state,
      sessionRevision: Math.max(state.sessionRevision, meta.sessionRevision),
      runSequences,
      terminalRuns,
      runOrder,
    },
  };
}

export interface ChatStreamState {
  messages: ChatMessage[];
  streamingIdx: number | null;
  nextMessageId: number;
}

export function ensureStreamingTransition(state: ChatStreamState): ChatStreamState {
  if (state.streamingIdx !== null) return state;
  const message: ChatMessage = {
    id: `msg-${state.nextMessageId}`,
    role: "assistant",
    content: "",
    streaming: true,
    timestamp: Date.now(),
  };
  return {
    messages: [...state.messages, message],
    streamingIdx: state.messages.length,
    nextMessageId: state.nextMessageId + 1,
  };
}

export function appendStreamingTransition(
  state: ChatStreamState,
  text: string,
): ChatStreamState {
  if (state.streamingIdx === null || !state.messages[state.streamingIdx]) return state;
  const messages = [...state.messages];
  const current = messages[state.streamingIdx];
  messages[state.streamingIdx] = { ...current, content: current.content + text };
  return { ...state, messages };
}

export function finishStreamingTransition(state: ChatStreamState): ChatStreamState {
  if (state.streamingIdx === null || !state.messages[state.streamingIdx]) return state;
  const messages = [...state.messages];
  messages[state.streamingIdx] = { ...messages[state.streamingIdx], streaming: false };
  return { ...state, messages, streamingIdx: null };
}

export function addToolCardTransition(
  state: ChatStreamState,
  toolId: string,
  name: string,
  input: unknown,
): ChatStreamState {
  const id = `tool-${toolId}`;
  if (state.messages.some((message) => message.id === id)) return state;
  const settled = finishStreamingTransition(state);
  return {
    ...settled,
    messages: [...settled.messages, {
      id,
      role: "tool",
      content: "Result unavailable",
      toolName: name,
      toolInput: input,
      streaming: true,
    }],
  };
}

export function updateToolResultTransition(
  state: ChatStreamState,
  toolId: string,
  ok: boolean,
  preview: string,
): ChatStreamState {
  const id = `tool-${toolId}`;
  const content = !preview ? (ok ? "[ok — no output]" : "Tool execution failed") : preview;

  let changed = false;
  const messages = state.messages.map((message) => {
    if (message.id !== id) return message;
    if (message.content === content && message.toolOk === ok && message.streaming === false) return message;
    changed = true;
    return { ...message, content, toolOk: ok, streaming: false };
  });
  return changed ? { ...state, messages } : state;
}

export const MAX_SUBAGENT_RUNS = 64;
export const MAX_SUBAGENT_TOOLS = 64;
export const MAX_SUBAGENT_OUTPUT_CHARS = 100_000;
const MAX_SUBAGENT_RESULT_CHARS = 24_000;
const MAX_SUBAGENT_EVENT_TEXT_CHARS = 4_000;
const MAX_SUBAGENT_ID_CHARS = 160;

export interface SubagentState {
  subagentRunsById: Record<string, SubagentRun>;
  childIdsByParentToolId: Record<string, string[]>;
}

export function emptySubagentState(): SubagentState {
  return { subagentRunsById: {}, childIdsByParentToolId: {} };
}

function safeChildString(value: unknown, max = MAX_SUBAGENT_EVENT_TEXT_CHARS): string {
  if (typeof value !== "string") return "";
  return sanitizeBrowserText(value).replace(/\0/g, "").slice(0, max);
}

function safeChildId(value: unknown): string {
  const safe = safeChildString(value, MAX_SUBAGENT_ID_CHARS).trim();
  return safe && !/[\r\n]/.test(safe) ? safe : "";
}

/** A deliberately shallow/bounded input copy. Child wire data must never turn
 * the browser store into an unbounded payload cache. */
const SENSITIVE_CHILD_KEY = /(^|[_-])(api[_-]?key|authorization|credential|password|secret|token|prompt|attachment[_-]?data|extracted[_-]?text|images?|content|body)($|[_-])/i;

function safeChildValue(value: unknown, depth = 0, key = ""): unknown {
  if (SENSITIVE_CHILD_KEY.test(key)) return undefined;
  if (depth > 4) return "[truncated]";
  if (typeof value === "string") return safeChildString(value);
  if (typeof value === "number") return Number.isFinite(value) ? value : null;
  if (typeof value === "boolean" || value === null) return value;
  if (Array.isArray(value)) return value.slice(0, 40)
    .map((item) => safeChildValue(item, depth + 1))
    .filter((item) => item !== undefined);
  if (!value || typeof value !== "object") return undefined;
  const safe: Record<string, unknown> = {};
  let count = 0;
  for (const rawKey in value as Record<string, unknown>) {
    if (!Object.prototype.hasOwnProperty.call(value, rawKey) || count >= 40) break;
    const safeKey = safeChildString(rawKey, 120);
    const child = safeChildValue((value as Record<string, unknown>)[rawKey], depth + 1, rawKey);
    if (safeKey && child !== undefined) safe[safeKey] = child;
    count += 1;
  }
  return safe;
}

function terminalChildStatus(value: unknown): SubagentRunStatus {
  switch (value) {
    case "succeeded": case "success": case "completed": return "succeeded";
    case "cancelled": case "canceled": return "cancelled";
    case "interrupted": return "interrupted";
    default: return "failed";
  }
}

function childToolId(event: EngineEvent): string {
  return safeChildId(event.kind === "tool_use_start" || event.kind === "tool_result"
    ? event.id
    : event.tool_use_id);
}

function updateChildTool(run: SubagentRun, event: EngineEvent): SubagentRun {
  const id = childToolId(event);
  if (!id) return run;
  const current = run.toolsById[id] ?? {
    id,
    name: safeChildString(event.name ?? event.tool_name, 160) || "tool",
    result: "",
    status: "pending",
    truncated: false,
  };
  let tool: SubagentTool = { ...current };
  if (event.name || event.tool_name) tool.name = safeChildString(event.name ?? event.tool_name, 160) || tool.name;

  switch (event.kind) {
    case "tool_use_start":
      tool = { ...tool, input: safeChildValue(event.input), status: "running" };
      break;
    case "tool_result": {
      const result = safeChildString(event.preview, MAX_SUBAGENT_RESULT_CHARS + 1);
      tool = {
        ...tool,
        ok: event.ok === true,
        result: result.slice(0, MAX_SUBAGENT_RESULT_CHARS),
        truncated: result.length > MAX_SUBAGENT_RESULT_CHARS,
        status: event.ok === true ? "succeeded" : "failed",
      };
      break;
    }
    case "tool_queued": tool.status = "queued"; break;
    case "tool_validation":
      tool.status = event.ok === false ? "validation_failed" : "validated";
      break;
    case "permission_requested":
      tool.status = "waiting_permission";
      tool.permission = safeChildString(event.waiting_on, 120) || "requested";
      break;
    case "permission_resolved":
      tool.permission = safeChildString(event.decision, 120) || "resolved";
      tool.status = event.decision === "denied" ? "permission_denied" : "permission_allowed";
      break;
    case "tool_execution_started": tool.status = "running"; break;
    case "tool_execution_finished": tool.status = safeChildString(event.status, 120) || "finished"; break;
    case "tool_result_normalized":
      tool.truncated = tool.truncated || event.truncated === true;
      break;
  }

  const isNew = run.toolsById[id] === undefined;
  let toolOrder = isNew ? [...run.toolOrder, id] : run.toolOrder;
  const toolsById = { ...run.toolsById, [id]: tool };
  while (toolOrder.length > MAX_SUBAGENT_TOOLS) {
    const removed = toolOrder[0];
    toolOrder = toolOrder.slice(1);
    delete toolsById[removed];
  }
  return { ...run, toolsById, toolOrder };
}

/** Apply one scoped event without consulting or mutating parent-run ordering.
 * A child sequence is accepted only when it is newer than that child's last
 * sequence; duplicates and late/out-of-order events are ignored. */
export function applySubagentEventTransition(
  state: SubagentState,
  raw: ScopedSubagentEvent,
  runLimit = MAX_SUBAGENT_RUNS,
): { accepted: boolean; state: SubagentState } {
  const id = safeChildId(raw.subagent_id);
  const parentToolUseId = safeChildId(raw.parent_tool_use_id);
  const sequence = raw.child_sequence;
  const inner = raw.event;
  if (!id || !parentToolUseId || !Number.isSafeInteger(sequence) || sequence < 0
    || !inner || typeof inner !== "object" || inner.kind === "subagent_event") {
    return { accepted: false, state };
  }

  const previous = state.subagentRunsById[id];
  if ((previous && previous.parentToolUseId !== parentToolUseId)
    || sequence <= (previous?.childSequence ?? -1)) {
    return { accepted: false, state };
  }

  let run: SubagentRun = previous ? { ...previous, childSequence: sequence } : {
    id,
    parentToolUseId,
    description: safeChildString(raw.description, 500) || "Delegated task",
    profile: safeChildString(raw.profile, 160) || undefined,
    index: typeof raw.index === "number" && Number.isSafeInteger(raw.index)
      ? Math.max(0, Math.min(raw.index, 1_000_000)) : 0,
    childSequence: sequence,
    status: "running",
    output: "",
    outputTruncated: false,
    segmentCount: 0,
    toolsById: {},
    toolOrder: [],
  };

  // Metadata may arrive before the child's first lifecycle event; retain safe
  // updates while preserving the immutable parent scope.
  run.description = safeChildString(raw.description, 500) || run.description;
  run.profile = safeChildString(raw.profile, 160) || run.profile;

  if (inner.kind === "text_delta") {
    const delta = safeChildString(inner.text, MAX_SUBAGENT_EVENT_TEXT_CHARS);
    const room = Math.max(0, MAX_SUBAGENT_OUTPUT_CHARS - run.output.length);
    run.output = run.output + delta.slice(0, room);
    run.outputTruncated = run.outputTruncated || delta.length > room;
  } else if (inner.kind === "assistant_done") {
    run.segmentCount += 1;
  } else if (inner.kind === "run_finished") {
    run.status = terminalChildStatus(inner.status);
  } else if (inner.kind === "permission_requested") {
    run.status = "waiting_permission";
  } else if (inner.kind === "permission_resolved" && run.status === "waiting_permission") {
    run.status = "running";
  }

  if ([
    "tool_use_start", "tool_result", "tool_queued", "tool_validation",
    "permission_requested", "permission_resolved", "tool_execution_started",
    "tool_execution_finished", "tool_result_normalized",
  ].includes(inner.kind)) run = updateChildTool(run, inner);

  const subagentRunsById = { ...state.subagentRunsById, [id]: run };
  const childIdsByParentToolId = { ...state.childIdsByParentToolId };
  const parentIds = childIdsByParentToolId[parentToolUseId] ?? [];
  if (!parentIds.includes(id)) childIdsByParentToolId[parentToolUseId] = [...parentIds, id];
  childIdsByParentToolId[parentToolUseId] = [...childIdsByParentToolId[parentToolUseId]]
    .sort((left, right) => subagentRunsById[left].index - subagentRunsById[right].index || left.localeCompare(right));

  const boundedLimit = Math.max(1, runLimit);
  const allIds = Object.keys(subagentRunsById);
  if (allIds.length > boundedLimit) {
    const retained = new Set(allIds
      .sort((left, right) => subagentRunsById[right].childSequence - subagentRunsById[left].childSequence || left.localeCompare(right))
      .slice(0, boundedLimit));
    for (const childId of allIds) if (!retained.has(childId)) delete subagentRunsById[childId];
    for (const [parentId, ids] of Object.entries(childIdsByParentToolId)) {
      const kept = ids.filter((childId) => retained.has(childId));
      if (kept.length) childIdsByParentToolId[parentId] = kept;
      else delete childIdsByParentToolId[parentId];
    }
  }

  return { accepted: true, state: { subagentRunsById, childIdsByParentToolId } };
}

export interface PromptDedupState {
  resolvedPermissionIds: string[];
  resolvedQuestionIds: string[];
}

function rememberBounded(values: string[], value: string, limit: number): string[] {
  if (values.includes(value)) return values;
  return [...values, value].slice(-Math.max(1, limit));
}

export function resolvePromptTransition(
  state: PromptDedupState,
  kind: "permission" | "question",
  requestId: string,
  limit = MAX_RESOLVED_PROMPTS,
): PromptDedupState {
  return kind === "permission"
    ? { ...state, resolvedPermissionIds: rememberBounded(state.resolvedPermissionIds, requestId, limit) }
    : { ...state, resolvedQuestionIds: rememberBounded(state.resolvedQuestionIds, requestId, limit) };
}

export interface UsageState {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
}

export function accumulateUsage(
  state: UsageState,
  run: { input: number; output: number; cacheRead: number; cacheWrite: number },
): UsageState {
  return {
    inputTokens: state.inputTokens + run.input,
    outputTokens: state.outputTokens + run.output,
    cacheReadTokens: state.cacheReadTokens + run.cacheRead,
    cacheWriteTokens: state.cacheWriteTokens + run.cacheWrite,
  };
}
