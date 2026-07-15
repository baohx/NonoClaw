import { useRef, useEffect, useCallback, useState } from "react";
import { useStore } from "./store";
import { useWebSocket } from "./websocket";
import BreathField from "./components/BreathField";
import ChatView from "./components/ChatView";
import CommitDialog from "./components/CommitDialog";
import FileTree from "./components/FileTree";
import GitPane from "./components/GitPane";
import InsightRail from "./components/InsightRail";
import InputBox from "./components/InputBox";
import PermissionDialog from "./components/PermissionDialog";
import QrDialog from "./components/QrDialog";
import QuestionDialog from "./components/QuestionDialog";
import SessionPicker from "./components/SessionPicker";
import StatusBar from "./components/StatusBar";

const WS_PROTO = window.location.protocol === "https:" ? "wss" : "ws";
const WS_URL = `${WS_PROTO}://${window.location.host}/ws`;

import type { AttachmentRef } from "./types";

export default function App() {
  const { send, forceReconnect } = useWebSocket(WS_URL);
  const connectionStatus = useStore((s) => s.connectionStatus);
  const model = useStore((s) => s.model);
  const sessionId = useStore((s) => s.sessionId);
  const sessions = useStore((s) => s.sessions);
  const showSessionPicker = useStore((s) => s.showSessionPicker);
  const setShowSessionPicker = useStore((s) => s.setShowSessionPicker);
  const compacting = useStore((s) => s.compacting);
  const pendingPermission = useStore((s) => s.pendingPermission);
  const pendingQuestion = useStore((s) => s.pendingQuestion);
  const pendingCommit = useStore((s) => s.pendingCommit);
  const setPendingCommit = useStore((s) => s.setPendingCommit);
  const clearMessages = useStore((s) => s.clearMessages);
  const messages = useStore((s) => s.messages);
  const streamingIdx = useStore((s) => s.streamingIdx);
  const fileTree = useStore((s) => s.fileTree);
  const fileTreeRoot = useStore((s) => s.fileTreeRoot);
  const projectInfo = useStore((s) => s.projectInfo);
  const leftRailCollapsed = useStore((s) => s.leftRailCollapsed);
  const insightCollapsed = useStore((s) => s.insightCollapsed);
  const toggleLeftRail = useStore((s) => s.toggleLeftRail);
  const toggleInsight = useStore((s) => s.toggleInsight);
  const theme = useStore((s) => s.theme);
  const [showQr, setShowQr] = useState(false);
  const [everConnected, setEverConnected] = useState(false);
  const [showSurfacing, setShowSurfacing] = useState(false);

  useEffect(() => {
    if (connectionStatus === "connected") setEverConnected(true);
  }, [connectionStatus]);

  // Surfacing overlay: ONLY on the very first connect (initial page load).
  // After that, all reconnects are silent — the UI stays usable and
  // send/refresh recover the connection lazily with no overlay.
  useEffect(() => {
    if (everConnected) {
      setShowSurfacing(false);
    } else if (connectionStatus !== "connected") {
      setShowSurfacing(true);
    } else {
      setShowSurfacing(false);
    }
  }, [connectionStatus, everConnected]);

  // Apply CSS theme to <html> so [data-theme] variable overrides take effect.
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
  }, [theme]);

  const chatRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (connectionStatus === "connected") send({ type: "file_tree" });
  }, [connectionStatus, send]);

  const userScrolledUp = useRef(false);
  useEffect(() => {
    if (!chatRef.current || userScrolledUp.current) return;
    chatRef.current.scrollTop = chatRef.current.scrollHeight;
  }, [messages]);

  const handleScroll = useCallback(() => {
    const el = chatRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
    userScrolledUp.current = !atBottom;
  }, []);

  const addMessage = useStore((s) => s.addMessage);

  const setAgentRunning = useStore((s) => s.setAgentRunning);
  const agentRunning = useStore((s) => s.agentRunning);

  const handleSubmit = useCallback(
    (prompt: string, attachments: AttachmentRef[]) => {
      const cmd = prompt.trim();
      if (cmd === "/clear") {
        // Server handles both cancel + clear atomically.
        useStore.getState().setAgentRunning(false);
        clearMessages();
        send({ type: "clear" });
        return;
      }
      if (cmd === "/compact") {
        send({ type: "compact" });
        return;
      }
      // /multi model1,model2 <prompt> — run the same prompt against multiple
      // models sequentially and display results labeled by model name.
      const multiMatch = cmd.match(/^\/multi\s+([\w\-.]+(?:,\s*[\w\-.]+)*)\s+(.+)/s);
      if (multiMatch) {
        const models = multiMatch[1].split(",").map((s) => s.trim()).filter(Boolean);
        const realPrompt = multiMatch[2].trim();
        if (models.length < 2) return;
        const label = (m: string) => {
          const info = useStore.getState().availableModels.find((x) => x.name === m);
          return info?.label || m;
        };
        addMessage({
          id: `user-${Date.now()}`,
          role: "user",
          content: `[compare: ${models.map(label).join(", ")}]\n${realPrompt}`,
        });
        userScrolledUp.current = false;
        // Label the first model before sending it (subsequent models are
        // labelled in the websocket Done handler).
        addMessage({
          id: `sys-${Date.now()}`,
          role: "system",
          content: `🟢 running ${label(models[0])}…`,
        });
        // Send the first model now; the websocket Done handler will chain the
        // rest via window.__nonoclaw_pending_multi (hacky but zero-new-infra).
        (window as any).__nonoclaw_pending_multi = {
          models: models.slice(1),
          prompt: realPrompt,
          send,
          addMessage,
          label,
        };
        const activeModel = useStore.getState().availableModels.length > 0
          ? useStore.getState().model : undefined;
        send({ type: "run", prompt: realPrompt, model: models[0] });
        return;
      }
      // /skill-name — inject the skill body into the system prompt.
      let append: string | undefined;
      const slashMatch = cmd.match(/^\/(\S+)/);
      if (slashMatch) {
        const skillName = slashMatch[1];
        const skills = useStore.getState().projectInfo?.skills ?? [];
        const found = skillName === "compact" || skillName === "clear"
          ? undefined // those are built-in commands, not skills
          : skills.find((s) => s.name === skillName);
        if (found) {
          append = found.body || `# Skill: ${found.name}\n${found.description}`;
          addMessage({
            id: `sys-${Date.now()}`,
            role: "system",
            content: `⚡ activated skill: /${found.name} — ${found.description}`,
          });
        }
      }
      addMessage({ id: `user-${Date.now()}`, role: "user", content: prompt });
      userScrolledUp.current = false;
      useStore.getState().setAgentRunning(true);
      const activeModel = useStore.getState().availableModels.length > 0
        ? useStore.getState().model : undefined;
      send({ type: "run", prompt, model: activeModel, append_system_prompt: append, attachments: attachments.length ? attachments : undefined });
    },
    [send, clearMessages, addMessage]
  );

  const setPendingPermission = useStore((s) => s.setPendingPermission);
  const setPendingQuestion = useStore((s) => s.setPendingQuestion);

  const handlePermission = useCallback(
    (decision: "allow" | "deny") => {
      if (!pendingPermission) return;
      send({
        type: "permission_decision",
        request_id: pendingPermission.request_id,
        decision,
      });
      setPendingPermission(null);
    },
    [send, pendingPermission, setPendingPermission]
  );

  const handleQuestion = useCallback(
    (answer: string | null) => {
      if (!pendingQuestion) return;
      send({
        type: "question_answer",
        request_id: pendingQuestion.request_id,
        answer,
      });
      setPendingQuestion(null);
    },
    [send, pendingQuestion, setPendingQuestion]
  );

  const handleResume = useCallback(
    (id: string) => {
      send({ type: "resume_session", id });
      setShowSessionPicker(false);
    },
    [send, setShowSessionPicker]
  );

  const handleNew = useCallback(() => {
    send({ type: "new_session" });
    setShowSessionPicker(false);
  }, [send, setShowSessionPicker]);

  const handleOpenFile = useCallback(
    (path: string, forceCode: boolean) => {
      send({ type: "open_file", path, force_code: forceCode });
    },
    [send]
  );

  const bodyClass = [
    "app-body",
    leftRailCollapsed ? "rail-collapsed" : "",
    insightCollapsed ? "insight-collapsed" : "",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <>
      <BreathField />
      <div className="aurora-grain" />
      <div className="aurora-noise" />

      <div className="app-root">
        <StatusBar
          model={model}
          sessionId={sessionId}
          connectionStatus={connectionStatus}
          onOpenSessions={() => setShowSessionPicker(true)}
          compacting={compacting}
          leftRailCollapsed={leftRailCollapsed}
          insightCollapsed={insightCollapsed}
          onToggleLeftRail={toggleLeftRail}
          onToggleInsight={toggleInsight}
          onShowQr={() => setShowQr(true)}
          onSetPermissionMode={(mode) => {
            useStore.getState().setPermissionMode(mode);
            send({ type: "set_permission_mode", mode });
          }}
          onSetModel={(name) => {
            send({ type: "set_model", name });
            const models = useStore.getState().availableModels;
            useStore.getState().setInfo(name, useStore.getState().sessionId, useStore.getState().authToken, models);
          }}
        />
        <div className={bodyClass}>
          <aside className="rail">
            <div className="rail__files">
              <FileTree
                root={fileTreeRoot}
                entries={fileTree}
                onOpen={handleOpenFile}
                onRefresh={() => send({ type: "file_tree" })}
              />
            </div>
            <div className="rail__git">
              <GitPane
                git={projectInfo?.git ?? null}
                onRefresh={() => send({ type: "project_info_refresh" })}
                onShow={(sha) => send({ type: "git_show", sha })}
              />
            </div>
          </aside>
          <main className="stage">
            <button
              className="chat-refresh"
              onClick={forceReconnect}
              title="Reconnect & sync conversation"
              aria-label="Reconnect and sync"
            >
              ↻
            </button>
            <div ref={chatRef} className="chat-scroll" onScroll={handleScroll}>
              <ChatView messages={messages} streamingIdx={streamingIdx} />
            </div>
            <InputBox onSubmit={handleSubmit} disabled={compacting || agentRunning} />
          </main>
          <aside className="insight">
            <InsightRail
              info={projectInfo}
              onOpen={handleOpenFile}
              onRefresh={() => send({ type: "project_info_refresh" })}
            />
          </aside>
        </div>
      </div>

      {showSurfacing && <ConnectingOverlay />}
      {showSessionPicker && (
        <SessionPicker
          sessions={sessions}
          currentId={sessionId}
          onNew={handleNew}
          onResume={handleResume}
          onClose={() => setShowSessionPicker(false)}
        />
      )}
      {pendingPermission && (
        <PermissionDialog
          toolName={pendingPermission.tool_name}
          message={pendingPermission.message}
          input={pendingPermission.input}
          onAllow={() => handlePermission("allow")}
          onDeny={() => handlePermission("deny")}
        />
      )}
      {pendingQuestion && (
        <QuestionDialog
          prompt={pendingQuestion.prompt}
          options={pendingQuestion.options}
          onAnswer={handleQuestion}
        />
      )}
      {pendingCommit && (
        <CommitDialog
          sha={pendingCommit.sha}
          output={pendingCommit.output}
          onClose={() => setPendingCommit(null)}
        />
      )}
      {showQr && <QrDialog onClose={() => setShowQr(false)} />}
    </>
  );
}

/** Shown briefly while the WebSocket handshake completes. */
function ConnectingOverlay() {
  return (
    <div className="connect-overlay">
      <div className="connect-orb" />
      <div className="connect-label">surfacing…</div>
    </div>
  );
}
