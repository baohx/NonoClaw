import type { StateCreator } from "zustand";
import type { BreathPhase } from "../breath";
import type {
  ChatMessage,
  ClientMsg,
  FileEntry,
  ImageRef,
  ModelInfo,
  PermissionMode,
  PermissionRequired,
  ProjectInfo,
  QuestionRequired,
  ScopedSubagentEvent,
  SessionInfoWire,
  SubagentRun,
  TaskChange,
} from "../types";
import { sanitizeBrowserText, sanitizeBrowserValue, sanitizeMediaAttachment, sanitizeProjectInfo } from "../security";
import { appendTraceEntry, type TraceEntry } from "../trace";
import {
  acceptLegacySnapshotTransition,
  acceptRunTransition,
  acceptSnapshotTransition,
  acknowledgeClientMessage,
  applySubagentEventTransition,
  accumulateUsage,
  addToolCardTransition,
  appendStreamingTransition,
  enqueueClientMessage,
  ensureStreamingTransition,
  finishStreamingTransition,
  prepareSessionBoundary,
  resolvePromptTransition,
  transitionConnection,
  updateToolResultTransition,
  type ConnectionState,
  type QueuedClientMessage,
  type SessionOrderingState,
} from "./transitions";

export type ConnectionStatus = "connecting" | "connected" | "disconnected";
export type Theme = "biolume" | "amber" | "frost"
  | "meadow" | "crimson" | "teal" | "indigo"
  | "burgundy" | "gold" | "aqua" | "scarlet"
  | "espresso" | "ember" | "navy";

/** Hex color for each theme displayed as the status-bar dot. */
export const THEME_COLORS: Record<Theme, string> = {
  biolume: "#0071e3",
  amber:   "#ff9f0a",
  frost:   "#64d2ff",
  meadow:  "#34c759",
  crimson: "#ff3b30",
  teal:    "#5ac8fa",
  indigo:  "#5856d6",
  burgundy:"#af52de",
  gold:    "#ffd60a",
  aqua:    "#00CED9",
  scarlet: "#ff2d55",
  espresso:"#a2845e",
  ember:   "#ff6b35",
  navy:    "#0a84ff",
};

/** Whether each theme uses a dark background (affects canvas blending, etc). */
export const THEME_IS_DARK: Record<Theme, boolean> = {
  biolume: false,
  amber:   false,
  frost:   true,
  meadow:  false,
  crimson: false,
  teal:    false,
  indigo:  true,
  burgundy: true,
  gold:    false,
  aqua:    false,
  scarlet: false,
  espresso: true,
  ember:   false,
  navy:    true,
};
export type BreathState = BreathPhase;

export interface ConnectionSlice extends ConnectionState {
  setConnectionStatus: (status: ConnectionStatus) => void;
  beginConnection: () => number;
  markConnected: (generation: number) => boolean;
  markDisconnected: (generation: number) => boolean;
  enqueueOutbound: (message: ClientMsg) => QueuedClientMessage;
  acknowledgeOutbound: (id: number) => void;
  cleanupConnection: () => void;
}

export interface SessionSlice {
  messages: ChatMessage[];
  streamingIdx: number | null;
  nextMessageId: number;
  model: string;
  sessionId: string;
  sessionRevision: number;
  snapshotRevision: number;
  awaitingSnapshot: boolean;
  sessions: SessionInfoWire[];
  hasMobileAccessToken: boolean;
  availableModels: ModelInfo[];
  addMessage: (message: ChatMessage) => void;
  ensureStreaming: () => void;
  appendStreaming: (text: string) => void;
  finishStreaming: () => void;
  setInfo: (model: string, sessionId: string, hasMobileAccessToken?: boolean, availableModels?: ModelInfo[]) => void;
  setModel: (model: string) => void;
  setSessions: (sessions: SessionInfoWire[]) => void;
  acceptSnapshot: (sessionId: string, revision: number) => boolean;
  acceptLegacySnapshot: () => boolean;
  prepareSessionSwitch: (sessionId?: string) => void;
  loadMessages: (messages: unknown[]) => void;
  clearMessages: () => void;
}

export interface MultiRunState {
  remaining: string[];
  prompt: string;
  nextModel?: string;
}

export interface RunSlice {
  runSequences: Record<string, number>;
  terminalRuns: Record<string, true>;
  runOrder: string[];
  activeRunId: string | null;
  compacting: boolean;
  agentRunning: boolean;
  taskChanges: TaskChange[];
  traceEntries: TraceEntry[];
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  multiRun: MultiRunState | null;
  acceptRunMessage: (meta: {
    runId: string;
    sessionId: string;
    sessionRevision: number;
    sequence: number;
  }, terminal: boolean) => boolean;
  setCompacting: (compacting: boolean) => void;
  setAgentRunning: (agentRunning: boolean) => void;
  completeRun: () => void;
  startMultiRun: (models: string[], prompt: string) => void;
  consumeMultiModel: (model: string) => void;
  cancelMultiRun: () => void;
  addTaskChange: (change: TaskChange) => void;
  addTraceEntry: (entry: TraceEntry) => void;
  clearTrace: () => void;
  addUsage: (run: { input: number; output: number; cacheRead: number; cacheWrite: number }) => void;
}

export interface ToolSlice {
  toolCards: Record<string, true>;
  addToolCard: (id: string, name: string, input: unknown) => string;
  updateToolResult: (toolId: string, ok: boolean, preview: string) => void;
}

export interface SubagentSlice {
  subagentRunsById: Record<string, SubagentRun>;
  childIdsByParentToolId: Record<string, string[]>;
  applySubagentEvent: (event: ScopedSubagentEvent) => boolean;
  clearSubagentRuns: () => void;
}

export interface ProjectSlice {
  fileTreeRoot: string;
  fileTree: FileEntry[];
  projectInfo: ProjectInfo | null;
  setFileTree: (root: string, entries: FileEntry[]) => void;
  setProjectInfo: (info: ProjectInfo) => void;
}

export interface DialogSlice {
  pendingPermission: PermissionRequired | null;
  pendingQuestion: QuestionRequired | null;
  pendingCommit: { sha: string; output: string } | null;
  showSessionPicker: boolean;
  resolvedPermissionIds: string[];
  resolvedQuestionIds: string[];
  setPendingPermission: (permission: PermissionRequired | null) => void;
  setPendingQuestion: (question: QuestionRequired | null) => void;
  resolvePermission: (requestId: string) => void;
  resolveQuestion: (requestId: string) => void;
  setPendingCommit: (commit: { sha: string; output: string } | null) => void;
  setShowSessionPicker: (show: boolean) => void;
}

export interface MediaAttachment {
  id: string;
  filename: string;
  extracted_text: string;
  image_count: number;
  images?: ImageRef[];
  uploading: boolean;
  error?: string;
}

export interface MediaSlice {
  draft: string;
  attachments: MediaAttachment[];
  recording: boolean;
  setDraft: (draft: string) => void;
  addAttachment: (attachment: MediaAttachment) => void;
  updateAttachment: (id: string, update: Partial<MediaAttachment>) => void;
  removeAttachment: (id: string) => void;
  clearAttachments: () => void;
  setRecording: (recording: boolean) => void;
}

export interface BreathSlice {
  breathState: BreathState;
  breathLabel: string;
  setBreathState: (state: BreathState, label?: string) => void;
}

export interface UiSlice {
  leftRailCollapsed: boolean;
  insightCollapsed: boolean;
  theme: Theme;
  permissionMode: PermissionMode;
  setLeftRailCollapsed: (collapsed: boolean) => void;
  setInsightCollapsed: (collapsed: boolean) => void;
  toggleLeftRail: () => void;
  toggleInsight: () => void;
  setTheme: (theme: Theme) => void;
  setPermissionMode: (mode: PermissionMode) => void;
}

export type AppState = ConnectionSlice & SessionSlice & RunSlice & ToolSlice & SubagentSlice & ProjectSlice & DialogSlice & MediaSlice & BreathSlice & UiSlice;
type Slice<T> = StateCreator<AppState, [], [], T>;

function connectionState(state: AppState): ConnectionState {
  return {
    connectionStatus: state.connectionStatus,
    connectionGeneration: state.connectionGeneration,
    outboundQueue: state.outboundQueue,
    nextOutboundId: state.nextOutboundId,
  };
}

function orderingState(state: AppState): SessionOrderingState {
  return {
    sessionId: state.sessionId,
    sessionRevision: state.sessionRevision,
    snapshotRevision: state.snapshotRevision,
    awaitingSnapshot: state.awaitingSnapshot,
    runSequences: state.runSequences,
    terminalRuns: state.terminalRuns,
    runOrder: state.runOrder,
  };
}

function boundaryCleanup(state: AppState, sessionId: string): Partial<AppState> {
  return {
    ...prepareSessionBoundary(orderingState(state), sessionId),
    messages: [],
    streamingIdx: null,
    nextMessageId: 1,
    activeRunId: null,
    agentRunning: false,
    compacting: false,
    multiRun: null,
    taskChanges: [],
    traceEntries: [],
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    cacheWriteTokens: 0,
    toolCards: {},
    subagentRunsById: {},
    childIdsByParentToolId: {},
    pendingPermission: null,
    pendingQuestion: null,
    pendingCommit: null,
    resolvedPermissionIds: [],
    resolvedQuestionIds: [],
    attachments: [],
    recording: false,
    outboundQueue: [],
  };
}

export const createConnectionSlice: Slice<ConnectionSlice> = (set, get) => ({
  connectionStatus: "disconnected",
  connectionGeneration: 0,
  outboundQueue: [],
  nextOutboundId: 1,
  setConnectionStatus: (connectionStatus) => set({ connectionStatus }),
  beginConnection: () => {
    let generation = 0;
    set((state) => {
      const next = transitionConnection(connectionState(state), { type: "begin" });
      generation = next.connectionGeneration;
      return next;
    });
    return generation;
  },
  markConnected: (generation) => {
    let accepted = false;
    set((state) => {
      const current = connectionState(state);
      const next = transitionConnection(current, { type: "connected", generation });
      accepted = next !== current;
      return next;
    });
    return accepted;
  },
  markDisconnected: (generation) => {
    let accepted = false;
    set((state) => {
      const current = connectionState(state);
      const next = transitionConnection(current, { type: "disconnected", generation });
      accepted = next !== current;
      return next;
    });
    return accepted;
  },
  enqueueOutbound: (message) => {
    let queued!: QueuedClientMessage;
    set((state) => {
      const result = enqueueClientMessage(connectionState(state), message);
      queued = result.entry;
      return result.state;
    });
    return queued;
  },
  acknowledgeOutbound: (id) => set((state) => acknowledgeClientMessage(connectionState(state), id)),
  cleanupConnection: () => set((state) => transitionConnection(connectionState(state), { type: "cleanup" })),
});

export const createSessionSlice: Slice<SessionSlice> = (set, get) => ({
  messages: [],
  streamingIdx: null,
  nextMessageId: 1,
  model: "",
  sessionId: "",
  sessionRevision: -1,
  snapshotRevision: -1,
  awaitingSnapshot: false,
  sessions: [],
  hasMobileAccessToken: false,
  availableModels: [],
  addMessage: (message) => set((state) => state.messages.some((item) => item.id === message.id)
    ? {}
    : { messages: [...state.messages, message] }),
  ensureStreaming: () => set((state) => ensureStreamingTransition(state)),
  appendStreaming: (text) => set((state) => appendStreamingTransition(state, text)),
  finishStreaming: () => set((state) => finishStreamingTransition(state)),
  setInfo: (model, sessionId, hasMobileAccessToken = false, availableModels = []) => set((state) => ({
    ...(state.sessionId && state.sessionId !== sessionId ? boundaryCleanup(state, sessionId) : {}),
    model,
    sessionId,
    hasMobileAccessToken,
    availableModels,
  })),
  setModel: (model) => set({ model }),
  setSessions: (sessions) => set({ sessions }),
  acceptSnapshot: (sessionId, revision) => {
    let accepted = false;
    set((state) => {
      const result = acceptSnapshotTransition(orderingState(state), sessionId, revision);
      accepted = result.accepted;
      if (!accepted) return {};
      return {
        ...(result.switched ? boundaryCleanup(state, sessionId) : {}),
        ...result.state,
        streamingIdx: null,
        pendingPermission: null,
        pendingQuestion: null,
        compacting: false,
        agentRunning: false,
        activeRunId: null,
      };
    });
    return accepted;
  },
  acceptLegacySnapshot: () => {
    let accepted = false;
    set((state) => {
      const hasQueuedRun = state.outboundQueue.some((entry) => entry.message.type === "run");
      const result = acceptLegacySnapshotTransition(orderingState(state), hasQueuedRun || state.agentRunning);
      accepted = result.accepted;
      return accepted ? { ...result.state, streamingIdx: null } : {};
    });
    return accepted;
  },
  prepareSessionSwitch: (sessionId) => set((state) => boundaryCleanup(state, sessionId ?? state.sessionId)),
  loadMessages: (messages) => {
    const mapped = engineMessagesToChat(messages);
    const nextMessageId = mapped.reduce((next, message) => {
      const match = String(message.id).match(/^msg-(\d+)$/);
      return match ? Math.max(next, Number.parseInt(match[1], 10) + 1) : next;
    }, 1);
    const toolCards = Object.fromEntries(mapped.filter((message) => message.role === "tool").map((message) => [message.id, true as const]));
    set({
      messages: mapped,
      streamingIdx: null,
      nextMessageId,
      toolCards,
      subagentRunsById: {},
      childIdsByParentToolId: {},
    });
  },
  clearMessages: () => set((state) => ({
    ...prepareSessionBoundary(orderingState(state)),
    messages: [],
    streamingIdx: null,
    nextMessageId: 1,
    toolCards: {},
    subagentRunsById: {},
    childIdsByParentToolId: {},
    activeRunId: null,
    agentRunning: false,
    compacting: false,
    multiRun: null,
    pendingPermission: null,
    pendingQuestion: null,
    outboundQueue: [],
  })),
});

export const createRunSlice: Slice<RunSlice> = (set, get) => ({
  runSequences: {},
  terminalRuns: {},
  runOrder: [],
  activeRunId: null,
  compacting: false,
  agentRunning: false,
  taskChanges: [],
  traceEntries: [],
  inputTokens: 0,
  outputTokens: 0,
  cacheReadTokens: 0,
  cacheWriteTokens: 0,
  multiRun: null,
  acceptRunMessage: (meta, terminal) => {
    let accepted = false;
    set((state) => {
      const result = acceptRunTransition(orderingState(state), meta, terminal);
      accepted = result.accepted;
      return accepted ? {
        ...result.state,
        activeRunId: terminal ? (state.activeRunId === meta.runId ? null : state.activeRunId) : meta.runId,
      } : {};
    });
    return accepted;
  },
  setCompacting: (compacting) => set({ compacting }),
  setAgentRunning: (agentRunning) => set({ agentRunning }),
  completeRun: () => set((state) => {
    const remaining = state.multiRun?.remaining ?? [];
    if (!state.multiRun) return { agentRunning: false, activeRunId: null };
    if (state.multiRun.nextModel) return { agentRunning: false, activeRunId: null };
    if (remaining.length === 0) return { agentRunning: false, activeRunId: null, multiRun: null };
    return {
      agentRunning: false,
      activeRunId: null,
      multiRun: { ...state.multiRun, remaining: remaining.slice(1), nextModel: remaining[0] },
    };
  }),
  startMultiRun: (models, prompt) => set({ multiRun: { remaining: [...models], prompt } }),
  consumeMultiModel: (model) => set((state) => state.multiRun?.nextModel === model
    ? { multiRun: { ...state.multiRun, nextModel: undefined }, agentRunning: true }
    : {}),
  cancelMultiRun: () => set({ multiRun: null }),
  addTaskChange: (change) => set((state) => ({ taskChanges: [...state.taskChanges, change] })),
  addTraceEntry: (entry) => set((state) => ({ traceEntries: appendTraceEntry(state.traceEntries, entry) })),
  clearTrace: () => set({ traceEntries: [] }),
  addUsage: (run) => set((state) => accumulateUsage(state, run)),
});

export const createToolSlice: Slice<ToolSlice> = (set, get) => ({
  toolCards: {},
  addToolCard: (toolId, name, input) => {
    const id = `tool-${toolId}`;
    const safeInput = sanitizeBrowserValue(input);
    set((state) => {
      const next = addToolCardTransition(state, toolId, name, safeInput);
      return next === state ? {} : { ...next, toolCards: { ...state.toolCards, [id]: true as const } };
    });
    return id;
  },
  updateToolResult: (toolId, ok, preview) => set((state) => updateToolResultTransition(state, toolId, ok, preview)),
});

export const createSubagentSlice: Slice<SubagentSlice> = (set) => ({
  subagentRunsById: {},
  childIdsByParentToolId: {},
  applySubagentEvent: (event) => {
    let accepted = false;
    set((state) => {
      const result = applySubagentEventTransition({
        subagentRunsById: state.subagentRunsById,
        childIdsByParentToolId: state.childIdsByParentToolId,
      }, event);
      accepted = result.accepted;
      return accepted ? result.state : {};
    });
    return accepted;
  },
  clearSubagentRuns: () => set({ subagentRunsById: {}, childIdsByParentToolId: {} }),
});

export const createProjectSlice: Slice<ProjectSlice> = (set) => ({
  fileTreeRoot: "",
  fileTree: [],
  projectInfo: null,
  setFileTree: (fileTreeRoot, fileTree) => set({ fileTreeRoot, fileTree }),
  setProjectInfo: (projectInfo) => set({ projectInfo: sanitizeProjectInfo(projectInfo) }),
});

export const createDialogSlice: Slice<DialogSlice> = (set) => ({
  pendingPermission: null,
  pendingQuestion: null,
  pendingCommit: null,
  showSessionPicker: false,
  resolvedPermissionIds: [],
  resolvedQuestionIds: [],
  setPendingPermission: (pendingPermission) => set((state) => pendingPermission && state.resolvedPermissionIds.includes(pendingPermission.request_id)
    ? {}
    : { pendingPermission: pendingPermission ? {
      ...pendingPermission,
      input: sanitizeBrowserValue(pendingPermission.input),
      message: sanitizeBrowserText(pendingPermission.message),
    } : null }),
  setPendingQuestion: (pendingQuestion) => set((state) => pendingQuestion && state.resolvedQuestionIds.includes(pendingQuestion.request_id)
    ? {}
    : { pendingQuestion: pendingQuestion ? {
      ...pendingQuestion,
      prompt: sanitizeBrowserText(pendingQuestion.prompt),
      options: pendingQuestion.options.map(sanitizeBrowserText),
    } : null }),
  resolvePermission: (requestId) => set((state) => ({
    ...resolvePromptTransition(state, "permission", requestId),
    pendingPermission: state.pendingPermission?.request_id === requestId ? null : state.pendingPermission,
  })),
  resolveQuestion: (requestId) => set((state) => ({
    ...resolvePromptTransition(state, "question", requestId),
    pendingQuestion: state.pendingQuestion?.request_id === requestId ? null : state.pendingQuestion,
  })),
  setPendingCommit: (pendingCommit) => set({ pendingCommit }),
  setShowSessionPicker: (showSessionPicker) => set({ showSessionPicker }),
});

const DRAFT_STORAGE_KEY = "nonoclaw:draft";
const LEGACY_MESSAGES_STORAGE_KEY = "nonoclaw:messages";
function initialDraft(): string {
  if (typeof localStorage === "undefined") return "";
  try {
    // Session JSONL is authoritative. Remove the obsolete browser transcript
    // cache once while retaining UI preferences and the unsent draft.
    localStorage.removeItem(LEGACY_MESSAGES_STORAGE_KEY);
    return localStorage.getItem(DRAFT_STORAGE_KEY) ?? "";
  } catch { return ""; }
}

export const createMediaSlice: Slice<MediaSlice> = (set) => ({
  draft: initialDraft(),
  attachments: [],
  recording: false,
  setDraft: (draft) => {
    try {
      if (typeof localStorage !== "undefined") {
        if (draft) localStorage.setItem(DRAFT_STORAGE_KEY, draft);
        else localStorage.removeItem(DRAFT_STORAGE_KEY);
      }
    } catch {}
    set({ draft });
  },
  addAttachment: (attachment) => set((state) => state.attachments.some((item) => item.id === attachment.id)
    ? {}
    : { attachments: [...state.attachments, sanitizeMediaAttachment(attachment)] }),
  updateAttachment: (id, update) => set((state) => ({
    attachments: state.attachments.map((attachment) => attachment.id === id
      ? sanitizeMediaAttachment({ ...attachment, ...update })
      : attachment),
  })),
  removeAttachment: (id) => set((state) => ({ attachments: state.attachments.filter((attachment) => attachment.id !== id) })),
  clearAttachments: () => set({ attachments: [] }),
  setRecording: (recording) => set({ recording }),
});

export const createBreathSlice: Slice<BreathSlice> = (set) => ({
  breathState: "idle",
  breathLabel: "idle",
  setBreathState: (breathState, breathLabel = breathState) => set({ breathState, breathLabel }),
});

function initialTheme(): Theme {
  if (typeof localStorage === "undefined") return "biolume";
  try {
    const value = localStorage.getItem("nonoclaw:theme");
    return Object.prototype.hasOwnProperty.call(THEME_COLORS, value as string)
      ? (value as Theme)
      : "biolume";
  } catch { return "biolume"; }
}

export const createUiSlice: Slice<UiSlice> = (set) => ({
  leftRailCollapsed: false,
  insightCollapsed: false,
  theme: initialTheme(),
  permissionMode: "default",
  setLeftRailCollapsed: (leftRailCollapsed) => set({ leftRailCollapsed }),
  setInsightCollapsed: (insightCollapsed) => set({ insightCollapsed }),
  toggleLeftRail: () => set((state) => ({ leftRailCollapsed: !state.leftRailCollapsed })),
  toggleInsight: () => set((state) => ({ insightCollapsed: !state.insightCollapsed })),
  setTheme: (theme) => {
    try { if (typeof localStorage !== "undefined") localStorage.setItem("nonoclaw:theme", theme); } catch {}
    set({ theme });
  },
  setPermissionMode: (permissionMode) => set({ permissionMode }),
});

export function engineMessagesToChat(messages: unknown[]): ChatMessage[] {
  type Block = { type?: string; text?: string; id?: string; tool_use_id?: string; name?: string; input?: unknown; content?: unknown; is_error?: boolean };
  const output: ChatMessage[] = [];
  let counter = 1;
  const nextId = () => `msg-${counter++}`;
  const firstUseById = new Map<string, Block>();
  const lastResultById = new Map<string, Block>();
  const duplicateUses = new Set<string>();
  const duplicateResults = new Set<string>();

  for (const raw of messages) {
    const blocks = Array.isArray((raw as { content?: unknown })?.content)
      ? (raw as { content: Block[] }).content : [];
    for (const block of blocks) {
      if (block.type === "tool_use" && block.id) {
        if (firstUseById.has(block.id)) duplicateUses.add(block.id);
        else firstUseById.set(block.id, block);
      } else if (block.type === "tool_result" && block.tool_use_id) {
        if (lastResultById.has(block.tool_use_id)) duplicateResults.add(block.tool_use_id);
        lastResultById.set(block.tool_use_id, block);
      }
    }
  }

  const emittedUses = new Set<string>();
  const appendText = (
    role: "user" | "assistant",
    text: string,
    attachments: ChatMessage["attachments"] = [],
  ) => {
    if (!text) return;
    const previous = output[output.length - 1];
    if (previous?.role === role && !previous.streaming && !previous.toolName
      && !previous.attachments?.length && !attachments.length) previous.content += text;
    else output.push({ id: nextId(), role, content: text, ...(attachments.length ? { attachments } : {}) });
  };

  for (const raw of messages) {
    const message = raw as { role?: string; content?: unknown; attachments?: unknown };
    const attachments = Array.isArray(message.attachments)
      ? message.attachments.flatMap((value) => {
        const filename = (value as { filename?: unknown })?.filename;
        return typeof filename === "string" && filename.length > 0
          ? [{ filename: sanitizeBrowserText(filename.slice(0, 255)) }]
          : [];
      })
      : [];
    const blocks: Block[] = Array.isArray(message.content) ? message.content as Block[] : [];
    if (typeof message.content === "string") {
      if (message.role === "user" || message.role === "assistant") appendText(message.role, message.content, attachments);
      continue;
    }
    let pendingAttachments = attachments;
    for (const block of blocks) {
      if (block.type === "text") {
        if (message.role === "user" || message.role === "assistant") {
          appendText(message.role, block.text ?? "", pendingAttachments);
          pendingAttachments = [];
        }
        continue;
      }
      if (block.type === "tool_use") {
        const callId = block.id;
        if (!callId || firstUseById.get(callId) !== block || emittedUses.has(callId)) continue;
        emittedUses.add(callId);
        const result = lastResultById.get(callId);
        output.push({
          id: `tool-${callId}`,
          role: "tool",
          content: result ? (extractText(result.content) || (result.is_error ? "Tool execution failed" : "[ok — no output]")) : "Result unavailable",
          toolName: block.name,
          toolInput: sanitizeBrowserValue(block.input),
          toolOk: result ? !result.is_error : undefined,
          streaming: false,
        });
        if (duplicateUses.has(callId)) output.push({ id: nextId(), role: "system", content: `Duplicate tool call ignored: ${callId}` });
        if (duplicateResults.has(callId)) output.push({ id: nextId(), role: "system", content: `Duplicate tool results resolved to the last result: ${callId}` });
        continue;
      }
      if (block.type === "tool_result") {
        const callId = block.tool_use_id;
        if (!callId || firstUseById.has(callId) || lastResultById.get(callId) !== block) continue;
        output.push({
          id: `tool-${callId}`,
          role: "tool",
          content: extractText(block.content) || (block.is_error ? "Tool execution failed" : "[ok — no output]"),
          toolName: "Command unavailable",
          toolOk: !block.is_error,
          streaming: false,
        });
      }
    }
  }
  return output;
}

function extractText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((block) => (block as { type?: string; text?: string })?.type === "text" ? block.text ?? "" : "").join("");
}
