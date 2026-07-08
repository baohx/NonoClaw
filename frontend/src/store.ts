import { create } from "zustand";
import type {
  ChatMessage,
  FileEntry,
  PermissionRequired,
  ProjectInfo,
  QuestionRequired,
  SessionInfoWire,
} from "./types";

const STORAGE_KEY = "nonoclaw:messages";

function loadPersistedMessages(): { messages: ChatMessage[]; nextId: number } {
  if (typeof localStorage === "undefined") return { messages: [], nextId: 1 };
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { messages: [], nextId: 1 };
    const messages = JSON.parse(raw) as ChatMessage[];
    // Reset streaming flags (a refresh mid-stream leaves stale cursors).
    for (const m of messages) m.streaming = false;
    let maxN = 0;
    for (const m of messages) {
      const match = String(m.id).match(/^msg-(\d+)$/);
      if (match) maxN = Math.max(maxN, parseInt(match[1], 10));
    }
    return { messages, nextId: maxN + 1 };
  } catch {
    return { messages: [], nextId: 1 };
  }
}

interface AppState {
  // ── Messages ──
  messages: ChatMessage[];
  /** Index of the currently-streaming assistant message, if any. */
  streamingIdx: number | null;

  // ── Connection ──
  connectionStatus: "connecting" | "connected" | "disconnected";
  model: string;
  sessionId: string;
  /** Resumable sessions for the current cwd (most-recent first). */
  sessions: SessionInfoWire[];

  // ── Modals ──
  pendingPermission: PermissionRequired | null;
  pendingQuestion: QuestionRequired | null;
  /** Commit patch viewer (Git pane click → git show). */
  pendingCommit: { sha: string; output: string } | null;
  /** True while a manual /compact summarization round-trip is in flight. */
  compacting: boolean;
  /** Session-picker visibility. */
  showSessionPicker: boolean;

  // ── File tree ──
  fileTreeRoot: string;
  fileTree: FileEntry[];

  // ── Project context (Insight rail + Git pane) ──
  projectInfo: ProjectInfo | null;
  leftRailCollapsed: boolean;
  insightCollapsed: boolean;

  // ── Usage ── (accumulated across the conversation)
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;

  // ── Actions ──
  addMessage: (msg: ChatMessage) => void;
  ensureStreaming: () => void;
  appendStreaming: (text: string) => void;
  finishStreaming: () => void;
  addToolCard: (id: string, name: string, input: unknown) => string;
  updateToolResult: (toolId: string, ok: boolean, preview: string) => void;
  setConnectionStatus: (s: "connecting" | "connected" | "disconnected") => void;
  setInfo: (model: string, sessionId: string) => void;
  /** Update just the displayed model (from the API's real model_info event). */
  setModel: (model: string) => void;
  setSessions: (s: SessionInfoWire[]) => void;
  setShowSessionPicker: (v: boolean) => void;
  setFileTree: (root: string, entries: FileEntry[]) => void;
  setProjectInfo: (info: ProjectInfo) => void;
  setLeftRailCollapsed: (v: boolean) => void;
  setInsightCollapsed: (v: boolean) => void;
  toggleLeftRail: () => void;
  toggleInsight: () => void;
  setCompacting: (v: boolean) => void;
  setPendingPermission: (p: PermissionRequired | null) => void;
  setPendingQuestion: (q: QuestionRequired | null) => void;
  setPendingCommit: (c: { sha: string; output: string } | null) => void;
  /** Replace the chat from a resumed/fresh session's server replay. */
  loadMessages: (msgs: unknown[]) => void;
  /** Accumulate usage from one completed run; returns the delta for display. */
  addUsage: (run: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
  }) => void;
  clearMessages: () => void;
}

let nextId = 1;

const persisted = loadPersistedMessages();
nextId = persisted.nextId;

export const useStore = create<AppState>((set, get) => ({
  messages: persisted.messages,
  streamingIdx: null,
  connectionStatus: "disconnected",
  model: "",
  sessionId: "",
  sessions: [],
  pendingPermission: null,
  pendingQuestion: null,
  pendingCommit: null,
  compacting: false,
  showSessionPicker: false,
  fileTreeRoot: "",
  fileTree: [],
  projectInfo: null,
  leftRailCollapsed: false,
  insightCollapsed: false,
  inputTokens: 0,
  outputTokens: 0,
  cacheReadTokens: 0,
  cacheWriteTokens: 0,

  addMessage: (msg) =>
    set((s) => ({ messages: [...s.messages, msg] })),

  ensureStreaming: () => {
    set((s) => {
      if (s.streamingIdx !== null) return {}; // already streaming
      const id = `msg-${nextId++}`;
      const msg: ChatMessage = {
        id,
        role: "assistant",
        content: "",
        streaming: true,
      };
      return {
        messages: [...s.messages, msg],
        streamingIdx: s.messages.length,
      };
    });
  },

  appendStreaming: (text) => {
    set((s) => {
      if (s.streamingIdx === null) return {};
      const updated = [...s.messages];
      updated[s.streamingIdx] = {
        ...updated[s.streamingIdx],
        content: updated[s.streamingIdx].content + text,
      };
      return { messages: updated };
    });
  },

  finishStreaming: () => {
    set((s) => {
      if (s.streamingIdx === null) return {};
      const updated = [...s.messages];
      updated[s.streamingIdx] = { ...updated[s.streamingIdx], streaming: false };
      return { messages: updated, streamingIdx: null };
    });
  },

  addToolCard: (toolId, name, input) => {
    const msg: ChatMessage = {
      id: `tool-${toolId}`,
      role: "tool",
      content: `Running ${name}…`,
      toolName: name,
      toolInput: input,
      streaming: true,
    };
    set((s) => ({ messages: [...s.messages, msg] }));
    return msg.id;
  },

  updateToolResult: (msgId, ok, preview) => {
    set((s) => ({
      messages: s.messages.map((m) =>
        m.id === msgId
          ? { ...m, content: preview, toolOk: ok, streaming: false }
          : m
      ),
    }));
  },

  setConnectionStatus: (status) => set({ connectionStatus: status }),
  setInfo: (model, sessionId) => set({ model, sessionId }),
  setModel: (model) => set({ model }),
  setSessions: (sessions) => set({ sessions }),
  setShowSessionPicker: (showSessionPicker) => set({ showSessionPicker }),
  setFileTree: (root, entries) => set({ fileTreeRoot: root, fileTree: entries }),
  setProjectInfo: (info) => set({ projectInfo: info }),
  setLeftRailCollapsed: (leftRailCollapsed) => set({ leftRailCollapsed }),
  setInsightCollapsed: (insightCollapsed) => set({ insightCollapsed }),
  toggleLeftRail: () => set((s) => ({ leftRailCollapsed: !s.leftRailCollapsed })),
  toggleInsight: () => set((s) => ({ insightCollapsed: !s.insightCollapsed })),
  setCompacting: (compacting) => set({ compacting }),
  setPendingPermission: (p) => set({ pendingPermission: p }),
  setPendingQuestion: (q) => set({ pendingQuestion: q }),
  setPendingCommit: (pendingCommit) => set({ pendingCommit }),
  loadMessages: (msgs) => {
    nextId = 1;
    const mapped = engineMessagesToChat(msgs);
    for (const m of mapped) {
      const match = String(m.id).match(/^msg-(\d+)$/);
      if (match) nextId = Math.max(nextId, parseInt(match[1], 10) + 1);
    }
    set({ messages: mapped, streamingIdx: null });
    // Clear the stale local cache so it can't fight the server replay.
    if (typeof localStorage !== "undefined") localStorage.removeItem(STORAGE_KEY);
  },
  addUsage: (run) =>
    set((s) => ({
      inputTokens: s.inputTokens + run.input,
      outputTokens: s.outputTokens + run.output,
      cacheReadTokens: s.cacheReadTokens + run.cacheRead,
      cacheWriteTokens: s.cacheWriteTokens + run.cacheWrite,
    })),
  clearMessages: () => {
    nextId = 1;
    set({ messages: [], streamingIdx: null });
    if (typeof localStorage !== "undefined") localStorage.removeItem(STORAGE_KEY);
  },
}));

// Persist messages to localStorage on every change (debounced via microtask).
let saveScheduled = false;
useStore.subscribe(() => {
  if (saveScheduled) return;
  saveScheduled = true;
  queueMicrotask(() => {
    saveScheduled = false;
    if (typeof localStorage === "undefined") return;
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(useStore.getState().messages));
    } catch {
      // storage full or unavailable — ignore.
    }
  });
});

// ── Engine Message → ChatMessage mapping (for session replay) ──────────────

/**
 * Convert serialized engine Messages ({role, content}) back into the
 * ChatMessage[] shape the UI renders. v1: collapses each assistant turn's
 * tool_use blocks into inline plain-text annotations; correctness of the
 * on-disk transcript is what matters, rendering polish can follow.
 */
function engineMessagesToChat(msgs: unknown[]): ChatMessage[] {
  const out: ChatMessage[] = [];
  let counter = 0;
  const uid = () => `msg-${counter++}`;

  for (const raw of msgs) {
    const m = raw as { role?: string; content?: unknown };
    const role = m.role;
    const content = m.content;

    // Text-only content (string or {type:"text", text}).
    const text = extractText(content);
    const blocks = Array.isArray(content) ? content : [];

    if (role === "user") {
      // A user message may carry tool_result blocks (paired with a prior
      // assistant tool_use). Surface them as compact tool cards.
      const toolResults = blocks.filter(
        (b) => (b as { type?: string })?.type === "tool_result"
      );
      if (toolResults.length && !text) {
        for (const tr of toolResults) {
          const b = tr as {
            tool_use_id?: string;
            content?: unknown;
            is_error?: boolean;
          };
          out.push({
            id: `tool-${b.tool_use_id ?? counter++}`,
            role: "tool",
            content: extractText(b.content) || "(tool result)",
            toolOk: !b.is_error,
            streaming: false,
          });
        }
      } else if (text) {
        out.push({ id: uid(), role: "user", content: text });
      }
    } else if (role === "assistant") {
      const toolUses = blocks.filter(
        (b) => (b as { type?: string })?.type === "tool_use"
      ) as { id?: string; name?: string; input?: unknown }[];
      if (text) {
        out.push({ id: uid(), role: "assistant", content: text });
      }
      for (const tu of toolUses) {
        out.push({
          id: `tool-${tu.id ?? counter++}`,
          role: "tool",
          content: `${tu.name ?? "tool"}(${JSON.stringify(tu.input ?? {}).slice(0, 80)})`,
          toolName: tu.name,
          toolInput: tu.input,
          toolOk: true,
          streaming: false,
        });
      }
    }
    // system/other roles are skipped in replay.
  }
  return out;
}

function extractText(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((b) => (b as { type?: string; text?: string })?.type === "text" ? b.text ?? "" : "")
      .join("");
  }
  return "";
}
