import { useEffect, useRef, useCallback } from "react";
import { useStore } from "./store";
import { breathMeter } from "./breath";
import type { ServerMsg, ClientMsg } from "./types";

const RECONNECT_DELAY_MS = 500;
const MAX_RECONNECT_DELAY_MS = 10_000;
// A connection is treated as half-dead if no data frame has arrived in this
// window. The server heartbeat is 8s, so 12s leaves margin without false
// positives on healthy idle connections.
const STALE_AFTER_MS = 12_000;

// Messages sent while disconnected/stale are queued here and flushed on
// reconnect, so a just-typed prompt is resent after reconnect succeeds.
const pending: ClientMsg[] = [];

// Timestamp until which `messages_loaded` should be skipped. Every force-
// reconnect sets this to now+2.5s so the handshake's MessagesLoaded (which
// arrives *after* onopen has already flushed pending) doesn't wipe the
// optimistic user message. After the window, sync-from-other-peers resumes.
let skipLoadUntil = 0;

export function useWebSocket(url: string) {
  const wsRef = useRef<WebSocket | null>(null);
  const reconnectTimer = useRef<ReturnType<typeof setTimeout>>();
  const reconnectDelay = useRef(RECONNECT_DELAY_MS);
  const mounted = useRef(true);
  // Generation counter: stale handlers (from a closed socket) bail out so a
  // reconnecting connection's onclose doesn't clobber the fresh one.
  const genRef = useRef(0);
  const lastMsgAt = useRef(0);
  // Don't run the visibility reconnect handler until the first connection
  // succeeds — mobile browsers sometimes fire visibilitychange during page
  // load, which would force-reconnect and set skipLoadUntil, breaking the
  // initial MessagesLoaded.
  const firstConnect = useRef(false);

  const store = useStore;

  const connect = useCallback(() => {
    if (wsRef.current?.readyState === WebSocket.OPEN) return;
    const myGen = ++genRef.current;

    store.getState().setConnectionStatus("connecting");
    // Inject auth token + session from URL params (QR-code mobile access).
    let wsUrl = url;
    const params = new URLSearchParams(window.location.search);
    const token = params.get("token");
    const session = params.get("session");
    if (token || session) {
      wsUrl = `${url}?`;
      if (token) wsUrl += `token=${encodeURIComponent(token)}`;
      if (session) wsUrl += `${wsUrl.endsWith("?") ? "" : "&"}session=${encodeURIComponent(session)}`;
    }
    const ws = new WebSocket(wsUrl);
    wsRef.current = ws;
    lastMsgAt.current = Date.now();

    ws.onopen = () => {
      if (!mounted.current || myGen !== genRef.current) return;
      firstConnect.current = true;
      lastMsgAt.current = Date.now();
      store.getState().setConnectionStatus("connected");
      reconnectDelay.current = RECONNECT_DELAY_MS;
      // Flush anything queued while we were down (send-while-broken).
      while (pending.length) {
        const m = pending.shift()!;
        try { ws.send(JSON.stringify(m)); } catch {}
      }
    };

    ws.onmessage = (e) => {
      if (!mounted.current || myGen !== genRef.current) return;
      lastMsgAt.current = Date.now();
      try {
        const msg: ServerMsg = JSON.parse(e.data as string);
        console.debug("[ws]", msg.type, msg);
        handleServerMsg(msg);
      } catch (err) {
        console.error("[ws] parse error:", err, e.data);
      }
    };

    ws.onclose = () => {
      if (!mounted.current || myGen !== genRef.current) return;
      store.getState().setConnectionStatus("disconnected");
      wsRef.current = null;
      // Silent exponential backoff reconnect (no overlay — surfacing is gone
      // for reconnects; the UI stays usable and send/refresh recover lazily).
      reconnectTimer.current = setTimeout(() => {
        if (mounted.current) {
          reconnectDelay.current = Math.min(reconnectDelay.current * 1.5, MAX_RECONNECT_DELAY_MS);
          connect();
        }
      }, reconnectDelay.current);
    };

    ws.onerror = () => {
      if (myGen !== genRef.current) return;
      try { ws.close(); } catch {}
    };
  }, [url, store]);

  /** Force a fresh connection right now (used by the refresh button + send). */
  const forceReconnect = useCallback(() => {
    // Prevent the imminent handshake MessagesLoaded from clearing the UI:
    // onopen flushes pending *before* the server's async MessagesLoaded
    // arrives, so by then pending is already empty. This window keeps the
    // load at bay until the run we just queued gets handled.
    skipLoadUntil = Date.now() + 2500;
    reconnectDelay.current = RECONNECT_DELAY_MS;
    if (reconnectTimer.current) clearTimeout(reconnectTimer.current);
    try { wsRef.current?.close(); } catch {}
    wsRef.current = null;
    connect();
  }, [connect]);

  /**
   * Send a message. If the socket is healthy (OPEN + recently received a
   * heartbeat), send immediately. Otherwise queue + force reconnect — the
   * message flushes on open. This makes "send while broken" reconnect
   * transparently and deliver, catching the mobile half-dead-socket case
   * (readyState OPEN but data frozen) where ws.send() would silently buffer.
   */
  const send = useCallback((msg: ClientMsg) => {
    const ws = wsRef.current;
    const healthy =
      !!ws &&
      ws.readyState === WebSocket.OPEN &&
      Date.now() - lastMsgAt.current < STALE_AFTER_MS;
    if (healthy) {
      try { ws.send(JSON.stringify(msg)); return; } catch {}
    }
    // Stale or closed — queue + reconnect; flush on open.
    pending.push(msg);
    forceReconnect();
  }, [forceReconnect]);

  useEffect(() => {
    mounted.current = true;
    connect();

    // Mobile browsers freeze background tabs; on returning to the foreground,
    // silently reconnect if the socket looks stale. No overlay.
    // Guard: only run AFTER the first-ever connection succeeded, so we don't
    const onVisibility = () => {
      if (document.visibilityState !== "visible") return;
      if (!mounted.current || !firstConnect.current) return;
      const ws = wsRef.current;
      const stale = !ws || ws.readyState !== WebSocket.OPEN ||
        Date.now() - lastMsgAt.current > 8000;
      if (stale) {
        console.debug("[ws] foregrounded + stale — silent reconnect");
        forceReconnect();
      }
    };
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      mounted.current = false;
      document.removeEventListener("visibilitychange", onVisibility);
      wsRef.current?.close();
      if (reconnectTimer.current) clearTimeout(reconnectTimer.current);
    };
  }, [connect, forceReconnect]);

  return { send, forceReconnect };
}

// ── Message dispatcher ─────────────────────────────────────────────────────

function handleServerMsg(msg: ServerMsg) {
  const s = useStore.getState();

  switch (msg.type) {
    case "info": {
      s.setInfo(msg.model, msg.session_id, msg.auth_token, msg.available_models);
      break;
    }

    case "session_list": {
      s.setSessions(msg.sessions);
      break;
    }

    case "messages_loaded": {
      // Skip the wholesale clear right after a forced reconnect: the handshake
      // MessagesLoaded arrives AFTER onopen flushes pending, so by then pending
      // is empty. The skip window preserves the optimistic user message.
      if (Date.now() < skipLoadUntil || pending.length > 0) break;
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
      s.setAgentRunning(false);

      // Chain the next /multi model if there's a pending multi-run queue.
      const multi = (window as any).__nonoclaw_pending_multi;
      if (multi?.models?.length) {
        const nextModel = multi.models.shift();
        setTimeout(() => {
          multi.addMessage({
            id: `sys-${Date.now()}`,
            role: "system",
            content: `\u{1F7E2} running ${multi.label(nextModel)}…`,
          });
          multi.send({ type: "run", prompt: multi.prompt, model: nextModel });
        }, 600);
      } else {
        delete (window as any).__nonoclaw_pending_multi;
      }

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
      s.setAgentRunning(false);
      s.addMessage({
        id: `err-${Date.now()}`,
        role: "system",
        content: `Error: ${msg.message}`,
      });
      break;
    }
  }
}
