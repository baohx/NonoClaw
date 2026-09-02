import type { StateCreator } from "zustand";
import type { BreathPhase } from "../breath";
import type {
  ChatMessage,
  ClientMsg,
  FileEntry,
  ImageRef,
  ModelHealthEntry,
  ModelInfo,
  PermissionMode,
  PermissionRequired,
  ProjectInfo,
  QuestionRequired,
  ScopedSubagentEvent,
  SessionInfoWire,
  SessionRunPrompt,
  SubagentRun,
  TaskChange,
  TraceBatchWire,
} from "../types";
import { sanitizeBrowserText, sanitizeBrowserValue, sanitizeMediaAttachment, sanitizeProjectInfo } from "../security";
import {
  appendTraceEntry,
  appendTrajectoryTraceEntry,
  traceEntryFromEvent,
  trajectoryTraceEntries as selectTrajectoryTraceEntries,
  trimTraceEntries,
  type TraceEntry,
} from "../trace";
import {
  acceptLegacySnapshotTransition,
  acceptRunTransition,
  acceptSnapshotTransition,
  acknowledgeClientMessage,
  applySubagentEventTransition,
  accumulateUsage,
  addToolCardTransition,
  appendStreamingTransition,
  appendThinkingTransition,
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
  /** Sanitized wire messages accumulated across restored history pages. They
   * are remapped as one transcript so tool_use/tool_result pairs may cross a
   * page boundary without producing duplicate or orphan tool cards. */
  historyRawMessages: unknown[];
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
  appendThinking: (text: string) => void;
  finishStreaming: () => void;
  setInfo: (model: string, sessionId: string, hasMobileAccessToken?: boolean, availableModels?: ModelInfo[]) => void;
  setModel: (model: string) => void;
  setSessions: (sessions: SessionInfoWire[]) => void;
  acceptSnapshot: (sessionId: string, revision: number) => boolean;
  acceptLegacySnapshot: () => boolean;
  prepareSessionSwitch: (sessionId?: string) => void;
  loadMessages: (messages: unknown[], total?: number) => void;
  /** Replace traceEntries from persisted per-run batches (session replay). */
  loadPersistedTraces: (batches: TraceBatchWire[] | undefined) => void;
  /** Prepend one `history_page` payload; returns the new remaining count. */
  prependHistory: (messages: unknown[], remaining: number) => void;
  /** Older-history window state for the active session. */
  historyOlderRemaining: number;
  historyTotal: number;
  historyLoading: boolean;
  requestOlderHistory: (send: (msg: ClientMsg) => void, limit?: number) => void;
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
  cancelling: boolean;
  taskChanges: TaskChange[];
  /** Bounded redacted facts for the Technical Trace rail. */
  traceEntries: TraceEntry[];
  /** Complete low-frequency timing facts for Trajectory/Governance/Ledger. */
  trajectoryTraceEntries: TraceEntry[];
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
  setCancelling: (cancelling: boolean) => void;
  completeRun: () => void;
  startMultiRun: (models: string[], prompt: string) => void;
  consumeMultiModel: (model: string) => void;
  cancelMultiRun: () => void;
  addTaskChange: (change: TaskChange) => void;
  addTraceEntry: (entry: TraceEntry) => void;
  clearTrace: () => void;
  addUsage: (run: { input: number; output: number; cacheRead: number; cacheWrite: number }) => void;
  /** Overwrite all four token totals (engine sends cumulative `total` in
   * UsageUpdated every turn — additive would double-count). */
  setUsageTotal: (t: { input: number; output: number; cacheRead: number; cacheWrite: number }) => void;
}

export interface ToolSlice {
  toolCards: Record<string, true>;
  addToolCard: (id: string, name: string, input: unknown, timestampMs?: number) => string;
  updateToolResult: (toolId: string, ok: boolean, preview: string, timestampMs?: number) => void;
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
  /** Insight refresh in flight — drives the spin/disabled state on the button. */
  insightRefreshing: boolean;
  /** Run-boundary prompts per session id, lazily fetched from the server. */
  sessionPrompts: Record<string, SessionRunPrompt[]>;
  /** Per-model liveness from the last "run all" probe (name → entry). */
  modelsHealth: Record<string, ModelHealthEntry>;
  /** Probe sweep in flight — drives the button spin/disabled state. */
  modelsHealthChecking: boolean;
  setFileTree: (root: string, entries: FileEntry[]) => void;
  setProjectInfo: (info: ProjectInfo) => void;
  beginInsightRefresh: () => void;
  beginModelsHealthCheck: () => void;
  setModelsHealth: (results: ModelHealthEntry[]) => void;
  setSessionPrompts: (sessionId: string, prompts: SessionRunPrompt[]) => void;
}

export interface DialogSlice {
  pendingPermission: PermissionRequired | null;
  /** FIFO queue of unanswered questions. Concurrent `question_required`
   *  frames (e.g. parallel AskUserQuestion calls) must never clobber each
   *  other — the frontend shows `pendingQuestions[0]` and advances on
   *  resolve, so every question gets a chance to be answered. */
  pendingQuestions: QuestionRequired[];
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
  /** Local object-URL for image thumbnail preview (images only). */
  previewUrl?: string;
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
  /** Chat message id to scroll into view and highlight (rail session jump). */
  locatedMessageId: string | null;
  /** F2 Context X-Ray: latest token_budget_breakdown payload verbatim. */
  xrayBudget: import("../types").EngineEvent | null;
  /** Tool cards collapsed into group placeholders (ChatView toolsHidden). */
  toolsHidden: boolean;
  /** Whether the raw API log viewer drawer is open. */
  showApiLog: boolean;
  /** Whether the trajectory governance tab is open. */
  showGovernance: boolean;
  setXrayBudget: (event: import("../types").EngineEvent | null) => void;
  setLeftRailCollapsed: (collapsed: boolean) => void;
  setInsightCollapsed: (collapsed: boolean) => void;
  toggleLeftRail: () => void;
  toggleInsight: () => void;
  setShowApiLog: (show: boolean) => void;
  setShowGovernance: (show: boolean) => void;
  setTheme: (theme: Theme) => void;
  setPermissionMode: (mode: PermissionMode) => void;
  setLocatedMessage: (id: string | null) => void;
  setToolsHidden: (hidden: boolean) => void;
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
    historyRawMessages: [],
    streamingIdx: null,
    nextMessageId: 1,
    historyOlderRemaining: 0,
    historyTotal: 0,
    historyLoading: false,
    activeRunId: null,
    agentRunning: false,
    cancelling: false,
    compacting: false,
    multiRun: null,
    taskChanges: [],
    traceEntries: [],
    trajectoryTraceEntries: [],
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    cacheWriteTokens: 0,
    toolCards: {},
    subagentRunsById: {},
    childIdsByParentToolId: {},
    pendingPermission: null,
    pendingQuestions: [],
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
  historyRawMessages: [],
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
  historyOlderRemaining: 0,
  historyTotal: 0,
  historyLoading: false,
  addMessage: (message) => set((state) => state.messages.some((item) => item.id === message.id)
    ? {}
    : { messages: [...state.messages, { timestamp: Date.now(), ...message }] }),
  ensureStreaming: () => set((state) => ensureStreamingTransition(state)),
  appendStreaming: (text) => set((state) => appendStreamingTransition(state, text)),
  appendThinking: (text) => set((state) => appendThinkingTransition(state, text)),
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
        pendingQuestions: [],
        compacting: false,
        agentRunning: false,
        cancelling: false,
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
  loadPersistedTraces: (batches) => {
    const entries: TraceEntry[] = [];
    const rootEntries: TraceEntry[] = [];
    const scopedEvents: ScopedSubagentEvent[] = [];

    for (const batch of batches ?? []) {
      for (const envelope of batch.events) {
        // Never replace an envelope's child/root identity with the batch key.
        // Legacy v1 batches may contain raw child envelopes because their
        // collector was shared; those remain visible in bounded diagnostics
        // but are excluded from the root trajectory to match the live stream.
        const envelopeRunId = envelope.run_id ?? batch.run_id;
        const entry = traceEntryFromEvent({
          type: "event",
          ...envelope,
          run_id: envelopeRunId,
        });
        if (entry) {
          entries.push(entry);
          if (envelopeRunId === batch.run_id) rootEntries.push(entry);
        }
        if (envelope.event.kind === "subagent_event") {
          scopedEvents.push(envelope.event as ScopedSubagentEvent);
        }
      }
    }

    entries.sort((a, b) => a.timestampMs - b.timestampMs
      || a.runId.localeCompare(b.runId)
      || a.sequence - b.sequence
      || a.id.localeCompare(b.id));
    rootEntries.sort((a, b) => a.timestampMs - b.timestampMs
      || a.runId.localeCompare(b.runId)
      || a.sequence - b.sequence
      || a.id.localeCompare(b.id));
    scopedEvents.sort((a, b) => a.subagent_id.localeCompare(b.subagent_id)
      || a.child_sequence - b.child_sequence
      || a.parent_tool_use_id.localeCompare(b.parent_tool_use_id));

    const replayRunLimit = Math.max(1, new Set(scopedEvents.map((event) => event.subagent_id)).size);
    let childState = {
      subagentRunsById: {} as Record<string, SubagentRun>,
      childIdsByParentToolId: {} as Record<string, string[]>,
    };
    for (const event of scopedEvents) {
      childState = applySubagentEventTransition(
        childState,
        event,
        replayRunLimit,
        Number.MAX_SAFE_INTEGER,
      ).state;
    }

    set({
      traceEntries: trimTraceEntries(entries),
      trajectoryTraceEntries: selectTrajectoryTraceEntries(rootEntries),
      ...childState,
    });
  },
  loadMessages: (messages, total) => {
    const mapped = engineMessagesToChat(messages);
    const nextMessageId = mapped.reduce((next, message) => {
      const match = String(message.id).match(/^msg-(\d+)$/);
      return match ? Math.max(next, Number.parseInt(match[1], 10) + 1) : next;
    }, 1);
    const toolCards = Object.fromEntries(mapped.filter((message) => message.role === "tool").map((message) => [message.id, true as const]));
    set({
      messages: mapped,
      historyRawMessages: [...messages],
      streamingIdx: null,
      nextMessageId,
      toolCards,
      subagentRunsById: {},
      childIdsByParentToolId: {},
      // Tail window: total persisted messages vs. what we hold now.
      historyTotal: total ?? mapped.length,
      historyOlderRemaining: Math.max(0, (total ?? mapped.length) - mapped.length),
      historyLoading: false,
    });
  },
  prependHistory: (messages, remaining) => {
    const state = get();
    if (!messages.length) {
      set({ historyLoading: false, historyOlderRemaining: 0 });
      return;
    }

    // Re-map every restored wire message together. Tool use and tool result
    // blocks can straddle a page boundary; mapping pages independently creates
    // duplicate cards and loses duration/status information. Messages created
    // live after the snapshot have no srcIndex and remain appended unchanged.
    const historyRawMessages = [...messages, ...state.historyRawMessages];
    const mapped = engineMessagesToChat(historyRawMessages);
    const liveMessages = state.messages.filter((message) => message.srcIndex === undefined);
    const merged = [...mapped, ...liveMessages];
    const nextMessageId = merged.reduce((next, message) => {
      const match = String(message.id).match(/^msg-(\d+)$/);
      return match ? Math.max(next, Number.parseInt(match[1], 10) + 1) : next;
    }, 1);
    const toolCards = Object.fromEntries(merged
      .filter((message) => message.role === "tool")
      .map((message) => [message.id, true as const]));

    set({
      messages: merged,
      historyRawMessages,
      nextMessageId,
      toolCards,
      historyOlderRemaining: remaining,
      historyLoading: false,
    });
  },
  requestOlderHistory: (send, limit = 100) => {
    const state = get();
    if (!state.sessionId || state.historyLoading || state.historyOlderRemaining <= 0) return;
    // The held tail window starts at absolute index `historyOlderRemaining`
    // (= total - held): the server returns [before-limit, before).
    const before = state.historyOlderRemaining;
    set({ historyLoading: true });
    send({ type: "load_older", session_id: state.sessionId, before, limit });
  },
  clearMessages: () => set((state) => ({
    ...prepareSessionBoundary(orderingState(state)),
    messages: [],
    historyRawMessages: [],
    streamingIdx: null,
    nextMessageId: 1,
    toolCards: {},
    subagentRunsById: {},
    childIdsByParentToolId: {},
    traceEntries: [],
    trajectoryTraceEntries: [],
    historyOlderRemaining: 0,
    historyTotal: 0,
    historyLoading: false,
    activeRunId: null,
    agentRunning: false,
    cancelling: false,
    compacting: false,
    multiRun: null,
    pendingPermission: null,
    pendingQuestions: [],
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
  cancelling: false,
  taskChanges: [],
  traceEntries: [],
  trajectoryTraceEntries: [],
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
  setCancelling: (cancelling) => set({ cancelling }),
  completeRun: () => set((state) => {
    const remaining = state.multiRun?.remaining ?? [];
    if (!state.multiRun) return { agentRunning: false, cancelling: false, activeRunId: null };
    if (state.multiRun.nextModel) return { agentRunning: false, cancelling: false, activeRunId: null };
    if (remaining.length === 0) return { agentRunning: false, cancelling: false, activeRunId: null, multiRun: null };
    return {
      agentRunning: false,
      cancelling: false,
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
  addTraceEntry: (entry) => set((state) => ({
    traceEntries: appendTraceEntry(state.traceEntries, entry),
    trajectoryTraceEntries: appendTrajectoryTraceEntry(state.trajectoryTraceEntries, entry),
  })),
  clearTrace: () => set({ traceEntries: [], trajectoryTraceEntries: [] }),
  addUsage: (run) => set((state) => accumulateUsage(state, run)),
  setUsageTotal: (t) => set({
    inputTokens: t.input,
    outputTokens: t.output,
    cacheReadTokens: t.cacheRead,
    cacheWriteTokens: t.cacheWrite,
  }),
});

export const createToolSlice: Slice<ToolSlice> = (set, get) => ({
  toolCards: {},
  addToolCard: (toolId, name, input, timestampMs) => {
    const id = `tool-${toolId}`;
    const safeInput = sanitizeBrowserValue(input);
    set((state) => {
      const next = addToolCardTransition(state, toolId, name, safeInput, timestampMs);
      return next === state ? {} : { ...next, toolCards: { ...state.toolCards, [id]: true as const } };
    });
    return id;
  },
  updateToolResult: (toolId, ok, preview, timestampMs) => set((state) => (
    updateToolResultTransition(state, toolId, ok, preview, timestampMs)
  )),
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
  insightRefreshing: false,
  sessionPrompts: {},
  modelsHealth: {},
  modelsHealthChecking: false,
  setFileTree: (fileTreeRoot, fileTree) => set({ fileTreeRoot, fileTree }),
  beginInsightRefresh: () => set({ insightRefreshing: true }),
  setProjectInfo: (projectInfo) => set({ projectInfo: sanitizeProjectInfo(projectInfo), insightRefreshing: false }),
  beginModelsHealthCheck: () => set({ modelsHealthChecking: true, modelsHealth: {} }),
  setModelsHealth: (results) => set(() => {
    const modelsHealth: Record<string, ModelHealthEntry> = {};
    for (const r of results) modelsHealth[r.name] = r;
    return { modelsHealth, modelsHealthChecking: false };
  }),
  setSessionPrompts: (sessionId, prompts) => set((state) => ({
    sessionPrompts: { ...state.sessionPrompts, [sessionId]: prompts },
  })),
});

export const createDialogSlice: Slice<DialogSlice> = (set) => ({
  pendingPermission: null,
  pendingQuestions: [],
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
  setPendingQuestion: (pendingQuestion) => set((state) => {
    // `null` clears the whole queue (connection teardown). A non-null frame
    // is appended unless already resolved or already queued — never clobbered.
    if (!pendingQuestion) return { pendingQuestions: [] };
    const { request_id } = pendingQuestion;
    if (state.resolvedQuestionIds.includes(request_id)) return {};
    if (state.pendingQuestions.some((queued) => queued.request_id === request_id)) return {};
    return { pendingQuestions: [...state.pendingQuestions, {
      ...pendingQuestion,
      prompt: sanitizeBrowserText(pendingQuestion.prompt),
      options: pendingQuestion.options.map(sanitizeBrowserText),
    }] };
  }),
  resolvePermission: (requestId) => set((state) => ({
    ...resolvePromptTransition(state, "permission", requestId),
    pendingPermission: state.pendingPermission?.request_id === requestId ? null : state.pendingPermission,
  })),
  resolveQuestion: (requestId) => set((state) => ({
    ...resolvePromptTransition(state, "question", requestId),
    // Remove the answered question; the next queued question surfaces as
    // pendingQuestions[0] so sequential confirmation of parallel questions works.
    pendingQuestions: state.pendingQuestions.filter((queued) => queued.request_id !== requestId),
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
  if (typeof localStorage === "undefined") return "frost";
  try {
    const value = localStorage.getItem("nonoclaw:theme");
    return Object.prototype.hasOwnProperty.call(THEME_COLORS, value as string)
      ? (value as Theme)
      : "frost";
  } catch { return "frost"; }
}

export const createUiSlice: Slice<UiSlice> = (set) => ({
  leftRailCollapsed: false,
  insightCollapsed: false,
  theme: initialTheme(),
  permissionMode: "auto",
  locatedMessageId: null,
  xrayBudget: null,
  toolsHidden: false,
  showApiLog: false,
  showGovernance: false,
  setXrayBudget: (event) => set({ xrayBudget: event }),
  setToolsHidden: (toolsHidden) => set({ toolsHidden }),
  setShowApiLog: (showApiLog) => set({ showApiLog }),
  setShowGovernance: (showGovernance) => set({ showGovernance }),
  setLeftRailCollapsed: (leftRailCollapsed) => set({ leftRailCollapsed }),
  setInsightCollapsed: (insightCollapsed) => set({ insightCollapsed }),
  toggleLeftRail: () => set((state) => ({ leftRailCollapsed: !state.leftRailCollapsed })),
  toggleInsight: () => set((state) => ({ insightCollapsed: !state.insightCollapsed })),
  setTheme: (theme) => {
    try { if (typeof localStorage !== "undefined") localStorage.setItem("nonoclaw:theme", theme); } catch {}
    set({ theme });
  },
  setPermissionMode: (permissionMode) => set({ permissionMode }),
  setLocatedMessage: (locatedMessageId) => set({ locatedMessageId }),
});

export function engineMessagesToChat(messages: unknown[]): ChatMessage[] {
  type Block = { type?: string; text?: string; thinking?: string; id?: string; tool_use_id?: string; name?: string; input?: unknown; content?: unknown; is_error?: boolean };
  const output: ChatMessage[] = [];
  let counter = 1;
  const nextId = () => `msg-${counter++}`;
  const firstUseById = new Map<string, Block>();
  const lastResultById = new Map<string, Block>();
  const duplicateUses = new Set<string>();
  const duplicateResults = new Set<string>();
  const resultTsById = new Map<string, number>();

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
        // Wall-clock of the result's source entry — gives replayed tool
        // records a precise use→result duration without trace entries.
        const ts = (raw as { ts?: unknown }).ts;
        if (typeof ts === "number" && Number.isFinite(ts)) resultTsById.set(block.tool_use_id, ts);
      }
    }
  }

  const emittedUses = new Set<string>();
  // Timestamp of the last source message — inherited by messages exploded
  // from the same persisted entry (text + tool calls share one source line).
  let currentTs: number | undefined;
  // Ordinal of the source JSONL message — recorded on each derived chat
  // message so the session rail can locate a run's first prompt exactly.
  let currentSrcIndex = 0;
  const appendText = (
    role: "user" | "assistant",
    text: string,
    attachments: ChatMessage["attachments"] = [],
  ) => {
    if (!text) return;
    const previous = output[output.length - 1];
    if (previous?.role === role && previous.srcIndex === currentSrcIndex
      && !previous.streaming && !previous.toolName
      && !previous.attachments?.length && !attachments.length) previous.content += text;
    else output.push({
      id: nextId(),
      role,
      content: text,
      srcIndex: currentSrcIndex,
      ...(currentTs !== undefined ? { timestamp: currentTs } : {}),
      ...(attachments.length ? { attachments } : {}),
    });
  };

  for (let srcIndex = 0; srcIndex < messages.length; srcIndex++) {
    const raw = messages[srcIndex];
    const message = raw as { role?: string; content?: unknown; attachments?: unknown; ts?: unknown; src_index?: unknown };
    currentSrcIndex = typeof message.src_index === "number" && Number.isSafeInteger(message.src_index)
      && message.src_index >= 0 ? message.src_index : srcIndex;
    currentTs = typeof message.ts === "number" && Number.isFinite(message.ts) ? message.ts : undefined;
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
    let pendingThinking: string | undefined;
    for (const block of blocks) {
      if (block.type === "thinking") {
        const text = typeof block.thinking === "string" ? block.thinking : "";
        if (text.length > 0) pendingThinking = pendingThinking ? `${pendingThinking}\n${text}` : text;
        continue;
      }
      if (block.type === "text") {
        if (message.role === "user" || message.role === "assistant") {
          appendText(message.role, block.text ?? "", pendingAttachments);
          // Extended thinking precedes its visible text: attach to the entry
          // this text just landed in (merged or fresh).
          if (pendingThinking !== undefined && message.role === "assistant") {
            const target = output[output.length - 1];
            if (target?.role === "assistant" && !target.toolName) {
              target.thinking = target.thinking
                ? `${target.thinking}\n${pendingThinking}`
                : pendingThinking;
            }
            pendingThinking = undefined;
          }
          pendingAttachments = [];
        }
        continue;
      }
      if (block.type === "tool_use") {
        // Thinking before a tool-only step belongs to that assistant turn.
        if (pendingThinking !== undefined && message.role === "assistant") {
          const target = output[output.length - 1];
          if (target?.role === "assistant" && !target.toolName) {
            target.thinking = target.thinking
              ? `${target.thinking}\n${pendingThinking}`
              : pendingThinking;
          } else {
            output.push({ id: nextId(), role: "assistant", content: "", thinking: pendingThinking, srcIndex: currentSrcIndex, ...(currentTs !== undefined ? { timestamp: currentTs } : {}) });
          }
          pendingThinking = undefined;
        }
        const callId = block.id;
        if (!callId || firstUseById.get(callId) !== block || emittedUses.has(callId)) continue;
        emittedUses.add(callId);
        const result = lastResultById.get(callId);
        const resultTs = resultTsById.get(callId);
        output.push({
          id: `tool-${callId}`,
          role: "tool",
          content: result ? (extractText(result.content) || (result.is_error ? "Tool execution failed" : "[ok — no output]")) : "Result unavailable",
          toolName: block.name,
          toolInput: sanitizeBrowserValue(block.input),
          toolOk: result ? !result.is_error : undefined,
          streaming: false,
          srcIndex: currentSrcIndex,
          ...(currentTs !== undefined ? { timestamp: currentTs } : {}),
          ...(currentTs !== undefined && resultTs !== undefined && resultTs >= currentTs ? { durationMs: resultTs - currentTs } : {}),
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
          srcIndex: currentSrcIndex,
          ...(currentTs !== undefined ? { timestamp: currentTs } : {}),
        });
      }
      if (pendingThinking !== undefined && message.role === "assistant") {
        // Thinking-only assistant step (no text, no tool use).
        output.push({ id: nextId(), role: "assistant", content: "", thinking: pendingThinking, srcIndex: currentSrcIndex, ...(currentTs !== undefined ? { timestamp: currentTs } : {}) });
        pendingThinking = undefined;
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
