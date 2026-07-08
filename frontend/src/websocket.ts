import { useEffect, useRef, useCallback } from "react";
import { useStore } from "./store";
import { breathMeter } from "./breath";
import type { ServerMsg, ClientMsg } from "./types";

const RECONNECT_DELAY_MS = 2000;
const MAX_RECONNECT_DELAY_MS = 30_000;

export function useWebSocket(url: string) {
  const wsRef = useRef<WebSocket | null>(null);
  const reconnectTimer = useRef<ReturnType<typeof setTimeout>>();
  const reconnectDelay = useRef(RECONNECT_DELAY_MS);
  const mounted = useRef(true);
  const resolveRef = useRef<((msg: ServerMsg) => void) | null>(null);

  const store = useStore;

  const connect = useCallback(() => {
    if (wsRef.current?.readyState === WebSocket.OPEN) return;

    store.getState().setConnectionStatus("connecting");
    const ws = new WebSocket(url);
    wsRef.current = ws;

    ws.onopen = () => {
      if (!mounted.current) return;
      store.getState().setConnectionStatus("connected");
      reconnectDelay.current = RECONNECT_DELAY_MS;
    };

    ws.onmessage = (e) => {
      if (!mounted.current) return;
      try {
        const msg: ServerMsg = JSON.parse(e.data as string);
        console.debug("[ws]", msg.type, msg);
        handleServerMsg(msg);
      } catch (err) {
        console.error("[ws] parse error:", err, e.data);
      }
    };

    ws.onclose = () => {
      if (!mounted.current) return;
      store.getState().setConnectionStatus("disconnected");
      wsRef.current = null;

      // Exponential backoff reconnect
      reconnectTimer.current = setTimeout(() => {
        if (mounted.current) {
          reconnectDelay.current = Math.min(
            reconnectDelay.current * 1.5,
            MAX_RECONNECT_DELAY_MS
          );
          connect();
        }
      }, reconnectDelay.current);
    };

    ws.onerror = () => {
      ws.close();
    };
  }, [url, store]);

  const send = useCallback((msg: ClientMsg) => {
    if (wsRef.current?.readyState === WebSocket.OPEN) {
      wsRef.current.send(JSON.stringify(msg));
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    connect();
    return () => {
      mounted.current = false;
      wsRef.current?.close();
      if (reconnectTimer.current) clearTimeout(reconnectTimer.current);
    };
  }, [connect]);

  return { send };
}

// ── Message dispatcher ─────────────────────────────────────────────────────

function handleServerMsg(msg: ServerMsg) {
  const s = useStore.getState();

  switch (msg.type) {
    case "info": {
      s.setInfo(msg.model, msg.session_id);
      break;
    }

    case "session_list": {
      s.setSessions(msg.sessions);
      break;
    }

    case "messages_loaded": {
      s.loadMessages(msg.messages);
      break;
    }

    case "file_tree": {
      s.setFileTree(msg.root, msg.entries);
      break;
    }

    case "project_info": {
      s.setProjectInfo(msg.info);
      break;
    }

    case "git_show": {
      s.setPendingCommit({ sha: msg.sha, output: msg.output });
      break;
    }

    case "event": {
      const ev = msg.event;
      switch (ev.kind) {
        case "text_delta": {
          s.ensureStreaming();
          s.appendStreaming(ev.text || "");
          // Drive the breathing background from the token-stream rhythm.
          breathMeter.pulse((ev.text || "").length);
          break;
        }
        case "tool_use_start": {
          const toolId = ev.id || "";
          s.addToolCard(toolId, ev.name || "unknown", ev.input);
          breathMeter.flare(0.45);
          break;
        }
        case "tool_result": {
          const id = `tool-${ev.id}`;
          s.updateToolResult(id, ev.ok ?? false, ev.preview || "");
          breathMeter.flare(0.35);
          break;
        }
        case "assistant_done": {
          s.finishStreaming();
          breathMeter.settle();
          break;
        }
        case "model_info": {
          // Show the model the API actually used (e.g. deepseek-chat) instead
          // of the configured default.
          if (ev.model) s.setModel(ev.model);
          break;
        }
        case "compacting": {
          s.setCompacting(true);
          break;
        }
        case "compacted": {
          s.setCompacting(false);
          s.addMessage({
            id: `sys-${Date.now()}`,
            role: "system",
            content: `[compacted: removed ${ev.removed}, kept ${ev.kept}, ~${ev.tokens_before}→${ev.tokens_after} tokens]`,
          });
          break;
        }
      }
      break;
    }

    case "permission_required": {
      s.setPendingPermission(msg);
      break;
    }

    case "question_required": {
      s.setPendingQuestion(msg);
      break;
    }

    case "done": {
      const { usage } = msg;
      const inTok = usage.input_tokens ?? 0;
      const outTok = usage.output_tokens ?? 0;
      const cacheRead = usage.cache_read_input_tokens ?? 0;
      const cacheWrite = usage.cache_creation_input_tokens ?? 0;
      // Accumulate into the running totals (drives the StatusBar display).
      s.addUsage({
        input: inTok,
        output: outTok,
        cacheRead,
        cacheWrite,
      });
      // Append a compact usage line under the answer.
      const parts: string[] = [
        `in ${inTok.toLocaleString()}`,
        `out ${outTok.toLocaleString()}`,
      ];
      if (cacheRead) parts.push(`cache read ${cacheRead.toLocaleString()}`);
      if (cacheWrite) parts.push(`cache write ${cacheWrite.toLocaleString()}`);
      s.addMessage({
        id: `usage-${Date.now()}`,
        role: "system",
        content: `${msg.turns ?? 1} turn${(msg.turns ?? 1) > 1 ? "s" : ""} · ${parts.join(", ")}`,
      });
      break;
    }

    case "error": {
      s.addMessage({
        id: `err-${Date.now()}`,
        role: "system",
        content: `Error: ${msg.message}`,
      });
      break;
    }
  }
}
