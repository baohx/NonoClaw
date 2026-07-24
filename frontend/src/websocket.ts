import { useCallback, useEffect, useRef } from "react";
import { breathController } from "./breath";
import { clearMobileAccessToken, getBrowserAccessToken, sanitizeBrowserText, setMobileAccessToken } from "./security";
import { useStore } from "./store";
import { traceEntryFromEvent, traceTerminalEntry } from "./trace";
import type { ClientMsg, ServerMsg } from "./types";

const RECONNECT_DELAY_MS = 500;
const MAX_RECONNECT_DELAY_MS = 10_000;
const STALE_AFTER_MS = 12_000;
const FOREGROUND_STALE_AFTER_MS = 8_000;
const SUPPORTED_PROTOCOL_VERSION = 1;

interface SocketRuntime {
  socket: WebSocket | null;
  reconnectTimer: ReturnType<typeof setTimeout> | null;
  reconnectDelay: number;
  mounted: boolean;
  lastMessageAt: number;
  firstConnect: boolean;
}

function websocketUrl(url: string): string {
  const params = new URLSearchParams(window.location.search);
  const token = getBrowserAccessToken(window.location.search);
  const session = params.get("session");
  if (!token && !session) return url;
  const query = new URLSearchParams();
  if (token) query.set("token", token);
  if (session) query.set("session", session);
  return `${url}?${query.toString()}`;
}

export function useWebSocket(url: string) {
  const runtimeRef = useRef<SocketRuntime>({
    socket: null,
    reconnectTimer: null,
    reconnectDelay: RECONNECT_DELAY_MS,
    mounted: false,
    lastMessageAt: 0,
    firstConnect: false,
  });

  const connect = useCallback(() => {
    const runtime = runtimeRef.current;
    if (!runtime.mounted) return;
    if (runtime.socket?.readyState === WebSocket.OPEN
      || runtime.socket?.readyState === WebSocket.CONNECTING) return;
    if (runtime.reconnectTimer) {
      clearTimeout(runtime.reconnectTimer);
      runtime.reconnectTimer = null;
    }

    const generation = useStore.getState().beginConnection();
    breathController.consumeConnection("connecting");
    const socket = new WebSocket(websocketUrl(url));
    runtime.socket = socket;
    runtime.lastMessageAt = Date.now();

    socket.onopen = () => {
      const current = runtimeRef.current;
      if (!current.mounted || current.socket !== socket
        || !useStore.getState().markConnected(generation)) {
        try { socket.close(); } catch {}
        return;
      }
      current.firstConnect = true;
      current.lastMessageAt = Date.now();
      current.reconnectDelay = RECONNECT_DELAY_MS;
      breathController.consumeConnection("connected");

      // Remove a queued command only after this generation successfully sends
      // it. A failed send remains queued for the next generation.
      for (const entry of [...useStore.getState().outboundQueue]) {
        try {
          socket.send(JSON.stringify(entry.message));
          useStore.getState().acknowledgeOutbound(entry.id);
        } catch {
          try { socket.close(); } catch {}
          break;
        }
      }
    };

    socket.onmessage = (event) => {
      const current = runtimeRef.current;
      if (!current.mounted || current.socket !== socket
        || generation !== useStore.getState().connectionGeneration) return;
      current.lastMessageAt = Date.now();
      try {
        dispatchServerMessage(JSON.parse(event.data as string) as ServerMsg);
      } catch {
        // Never echo malformed frames: they may contain prompts, credentials,
        // attachment data, or unsafe upstream errors.
        console.error("[ws] message rejected");
      }
    };

    socket.onclose = () => {
      const current = runtimeRef.current;
      if (!current.mounted || current.socket !== socket
        || !useStore.getState().markDisconnected(generation)) return;
      current.socket = null;
      breathController.consumeConnection("disconnected");
      if (current.reconnectTimer) return;
      const delay = current.reconnectDelay;
      current.reconnectTimer = setTimeout(() => {
        const latest = runtimeRef.current;
        latest.reconnectTimer = null;
        latest.reconnectDelay = Math.min(latest.reconnectDelay * 1.5, MAX_RECONNECT_DELAY_MS);
        connect();
      }, delay);
    };

    socket.onerror = () => {
      if (runtimeRef.current.socket !== socket) return;
      try { socket.close(); } catch {}
    };
  }, [url]);

  /** Restart at most once: a generation already CONNECTING owns recovery. */
  const forceReconnect = useCallback(() => {
    const runtime = runtimeRef.current;
    runtime.reconnectDelay = RECONNECT_DELAY_MS;
    if (runtime.reconnectTimer) {
      clearTimeout(runtime.reconnectTimer);
      runtime.reconnectTimer = null;
    }
    if (runtime.socket?.readyState === WebSocket.CONNECTING) return;
    const previous = runtime.socket;
    runtime.socket = null;
    try { previous?.close(); } catch {}
    connect();
  }, [connect]);

  const send = useCallback((message: ClientMsg) => {
    const runtime = runtimeRef.current;
    const socket = runtime.socket;
    const healthy = !!socket
      && socket.readyState === WebSocket.OPEN
      && Date.now() - runtime.lastMessageAt < STALE_AFTER_MS;
    if (healthy) {
      try {
        socket.send(JSON.stringify(message));
        return;
      } catch {}
    }
    useStore.getState().enqueueOutbound(message);
    forceReconnect();
  }, [forceReconnect]);

  useEffect(() => {
    const runtime = runtimeRef.current;
    runtime.mounted = true;
    connect();

    const onVisibility = () => {
      const current = runtimeRef.current;
      if (document.visibilityState !== "visible" || !current.mounted || !current.firstConnect) return;
      const stale = !current.socket
        || current.socket.readyState !== WebSocket.OPEN
        || Date.now() - current.lastMessageAt > FOREGROUND_STALE_AFTER_MS;
      if (stale) forceReconnect();
    };
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      const current = runtimeRef.current;
      current.mounted = false;
      document.removeEventListener("visibilitychange", onVisibility);
      if (current.reconnectTimer) clearTimeout(current.reconnectTimer);
      current.reconnectTimer = null;
      const socket = current.socket;
      current.socket = null;
      try { socket?.close(); } catch {}
      const state = useStore.getState();
      state.cleanupConnection();
      state.cancelMultiRun();
      state.setPendingPermission(null);
      state.setPendingQuestion(null);
      clearMobileAccessToken();
      breathController.consumeConnection("closed");
    };
  }, [connect, forceReconnect]);

  return { send, forceReconnect };
}

export function supportsProtocol(version: number | undefined): boolean {
  return version === undefined || version <= SUPPORTED_PROTOCOL_VERSION;
}

function acceptRunMessage(
  message: {
    protocol_version?: number;
    run_id?: string;
    session_id?: string;
    session_revision?: number;
    sequence?: number;
  },
  terminal: boolean,
): boolean {
  if (!supportsProtocol(message.protocol_version)) return false;
  if (message.run_id === undefined
    || message.session_id === undefined
    || message.session_revision === undefined
    || message.sequence === undefined) return true;
  return useStore.getState().acceptRunMessage({
    runId: message.run_id,
    sessionId: message.session_id,
    sessionRevision: message.session_revision,
    sequence: message.sequence,
  }, terminal);
}

/** Deterministic protocol dispatcher; ordering decisions are delegated to pure store transitions. */
export function dispatchServerMessage(message: ServerMsg): void {
  const state = useStore.getState();
  switch (message.type) {
    case "info":
      state.setInfo(
        message.model,
        message.session_id,
        setMobileAccessToken(message.auth_token),
        message.available_models,
      );
      break;
    case "session_list":
      state.setSessions(message.sessions);
      break;
    case "messages_loaded": {
      if (!supportsProtocol(message.protocol_version)) break;
      const accepted = message.session_id !== undefined && message.revision !== undefined
        ? state.acceptSnapshot(message.session_id, message.revision)
        : state.acceptLegacySnapshot();
      if (accepted) state.loadMessages(message.messages);
      break;
    }
    case "file_tree":
      state.setFileTree(message.root, message.entries);
      break;
    case "project_info":
      state.setProjectInfo(message.info);
      break;
    case "git_show":
      state.setPendingCommit({ sha: message.sha, output: message.output });
      break;
    case "event": {
      if (!acceptRunMessage(message, false)) break;
      const event = message.event;
      const traceEntry = traceEntryFromEvent(message);
      if (traceEntry) state.addTraceEntry(traceEntry);
      breathController.consume(event);
      switch (event.kind) {
        case "text_delta":
          state.ensureStreaming();
          state.appendStreaming(event.text || "");
          break;
        case "tool_use_start":
          state.addToolCard(event.id || "", event.name || "unknown", event.input);
          break;
        case "tool_result":
          state.updateToolResult(`tool-${event.id}`, event.ok ?? false, event.preview || "");
          break;
        case "assistant_done":
          state.finishStreaming();
          break;
        case "model_info":
          if (event.model) state.setModel(event.model);
          break;
        case "task_changed":
          if (event.change) state.addTaskChange(event.change);
          break;
        case "compacting":
        case "compaction_started":
          state.setCompacting(true);
          break;
        case "compacted":
          state.setCompacting(false);
          state.addMessage({
            id: message.event_id || `compacted-${message.timestamp_ms ?? Date.now()}`,
            role: "system",
            content: `compacted: removed ${event.removed ?? 0}, kept ${event.kept ?? 0} messages`,
          });
          break;
      }
      break;
    }
    case "permission_required":
      state.setPendingPermission(message);
      breathController.consumePrompt("permission", true);
      break;
    case "question_required":
      state.setPendingQuestion(message);
      breathController.consumePrompt("question", true);
      break;
    case "done": {
      if (!acceptRunMessage(message, true)) break;
      state.finishStreaming();
      breathController.consumeTerminal("success", message.stop_reason ?? undefined);
      state.addTraceEntry(traceTerminalEntry(message, "done", {
        usage: message.usage,
        turns: message.turns,
        stop_reason: message.stop_reason,
      }));
      state.completeRun();
      const input = message.usage.input_tokens ?? 0;
      const output = message.usage.output_tokens ?? 0;
      const cacheRead = message.usage.cache_read_input_tokens ?? 0;
      const cacheWrite = message.usage.cache_creation_input_tokens ?? 0;
      state.addUsage({ input, output, cacheRead, cacheWrite });
      const parts = [`in ${input.toLocaleString()}`, `out ${output.toLocaleString()}`];
      if (cacheRead) parts.push(`cache read ${cacheRead.toLocaleString()}`);
      if (cacheWrite) parts.push(`cache write ${cacheWrite.toLocaleString()}`);
      state.addMessage({
        id: `usage-${message.run_id ?? message.timestamp_ms ?? Date.now()}`,
        role: "system",
        content: `${message.turns ?? 1} turn${(message.turns ?? 1) > 1 ? "s" : ""} · ${parts.join(", ")}`,
      });
      break;
    }
    case "error": {
      const safeMessage = sanitizeBrowserText(message.message || "operation failed");
      if (!acceptRunMessage(message, message.run_id !== undefined)) break;
      if (message.run_id !== undefined) {
        state.addTraceEntry(traceTerminalEntry(message, "wire_error", { message: safeMessage }));
        breathController.consumeTerminal("error", safeMessage);
      } else {
        breathController.signalError(safeMessage);
      }
      state.finishStreaming();
      state.setAgentRunning(false);
      state.cancelMultiRun();
      state.addMessage({
        id: `err-${message.run_id ?? message.timestamp_ms ?? Date.now()}`,
        role: "system",
        content: `Error: ${safeMessage}`,
      });
      break;
    }
  }
}
