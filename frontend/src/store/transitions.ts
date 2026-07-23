import type { ChatMessage, ClientMsg } from "../types";

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
  return {
    ...state,
    messages: [...state.messages, {
      id,
      role: "tool",
      content: `Running ${name}…`,
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
  const content = !preview && ok ? "[ok — no output]" : preview;
  let changed = false;
  const messages = state.messages.map((message) => {
    if (message.id !== toolId) return message;
    if (message.content === content && message.toolOk === ok && message.streaming === false) return message;
    changed = true;
    return { ...message, content, toolOk: ok, streaming: false };
  });
  return changed ? { ...state, messages } : state;
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
