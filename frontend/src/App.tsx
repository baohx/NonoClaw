import { useRef, useEffect, useCallback } from "react";
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
import QuestionDialog from "./components/QuestionDialog";
import SessionPicker from "./components/SessionPicker";
import StatusBar from "./components/StatusBar";

const WS_URL = `ws://${window.location.host}/ws`;

export default function App() {
  const { send } = useWebSocket(WS_URL);
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

  const handleSubmit = useCallback(
    (prompt: string) => {
      const cmd = prompt.trim();
      if (cmd === "/clear") {
        clearMessages();
        send({ type: "clear" });
        return;
      }
      if (cmd === "/compact") {
        send({ type: "compact" });
        return;
      }
      addMessage({ id: `user-${Date.now()}`, role: "user", content: prompt });
      userScrolledUp.current = false;
      send({ type: "run", prompt });
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
            <div ref={chatRef} className="chat-scroll" onScroll={handleScroll}>
              <ChatView messages={messages} streamingIdx={streamingIdx} />
            </div>
            <InputBox onSubmit={handleSubmit} disabled={compacting} />
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

      {connectionStatus === "connecting" && <ConnectingOverlay />}
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
