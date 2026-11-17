import { useEffect, useMemo, useState } from "react";
import { useStore } from "../store";
import type { ClientMsg, SessionInfoWire } from "../types";

interface Props {
  sessions: SessionInfoWire[];
  currentId: string;
  send: (msg: ClientMsg) => void;
  collapsed?: boolean;
  onToggleCollapsed?: () => void;
}

function formatStarted(started: string | null): string {
  if (!started) return "";
  const d = new Date(started);
  if (isNaN(d.getTime())) return "";
  const yyyy = d.getFullYear();
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  const hh = String(d.getHours()).padStart(2, "0");
  const mi = String(d.getMinutes()).padStart(2, "0");
  return `${yyyy}-${mm}-${dd} ${hh}:${mi}`;
}

function formatSessionDate(started: string | null): string {
  if (!started) return "Undated";
  const date = new Date(started);
  if (isNaN(date.getTime())) return "Undated";
  const sameYear = date.getFullYear() === new Date().getFullYear();
  return date.toLocaleDateString("en-US", { month: "short", day: "numeric", ...(sameYear ? {} : { year: "numeric" }) });
}

function sessionLabel(session: SessionInfoWire): string {
  return session.title?.trim() || session.summary.trim() || formatSessionDate(session.started);
}

/**
 * Left-rail session browser. Sessions sort newest-first by creation time;
 * expanding one lazily fetches the first user prompt of each run. Clicking a
 * prompt resumes that session and locates the matching message in ChatView.
 */
export default function SessionRail({ sessions, currentId, send, collapsed, onToggleCollapsed }: Props) {
  const sessionPrompts = useStore((s) => s.sessionPrompts);
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const sorted = useMemo(
    () =>
      [...sessions].sort((a, b) => {
        const ta = a.started ? new Date(a.started).getTime() : 0;
        const tb = b.started ? new Date(b.started).getTime() : 0;
        return tb - ta;
      }),
    [sessions],
  );

  // Machine-generated sessions (AutoDream memory upkeep, bench smoke) are not
  // conversations. Keep them out of the main list; show them under their own
  // collapsed group so the history stays reachable without polluting the rail.
  const MACHINE_TAGS = new Set(["dream", "bench-smoke"]);
  const { conversations, machine } = useMemo(() => {
    const conversations = sorted.filter((s) => !s.tag || !MACHINE_TAGS.has(s.tag));
    const machine = sorted.filter((s) => s.tag && MACHINE_TAGS.has(s.tag));
    return { conversations, machine };
  }, [sorted]);
  const [machineOpen, setMachineOpen] = useState(false);

  // Lazily fetch run prompts when a session row is expanded.
  useEffect(() => {
    if (!expandedId) return;
    if (sessionPrompts[expandedId] !== undefined) return;
    send({ type: "session_prompts", session_id: expandedId });
  }, [expandedId, sessionPrompts, send]);

  const handlePromptClick = (sessionId: string, messageIndex: number) => {
    const state = useStore.getState();
    // Locate by the source JSONL ordinal recorded on each restored chat
    // message (srcIndex) — chat messages merge/split source entries, so a
    // positional lookup into the rendered list would drift.
    const locate = () => {
      const messages = useStore.getState().messages;
      const target =
        messages.find((m) => m.srcIndex === messageIndex && m.role === "user") ??
        messages.find((m) => m.srcIndex === messageIndex);
      if (target) useStore.getState().setLocatedMessage(target.id);
    };
    if (sessionId === currentId) {
      locate();
    } else {
      state.prepareSessionSwitch(sessionId);
      send({ type: "resume_session", id: sessionId });
      // Wait until the resumed snapshot has loaded (sessionId flips on Info,
      // messages land via MessagesLoaded) before locating.
      let attempts = 0;
      const timer = window.setInterval(() => {
        attempts += 1;
        const s = useStore.getState();
        if (s.sessionId === sessionId && s.messages.some((m) => m.srcIndex === messageIndex)) {
          window.clearInterval(timer);
          locate();
        } else if (attempts > 40) {
          window.clearInterval(timer);
        }
      }, 100);
    }
  };

  return (
    <div className="sessionrail">
      <button
        className="sessionrail__head"
        onClick={onToggleCollapsed}
        aria-expanded={!collapsed}
        title={collapsed ? "Expand sessions" : "Collapse sessions"}
      >
        <span className="sessionrail__head-caret">{collapsed ? "▸" : "▾"}</span>
        <span className="sessionrail__title">Conversations</span>
        <span className="sessionrail__head-count">{conversations.length}</span>
      </button>
      {!collapsed && (
      <div className="sessionrail__list">
        {conversations.length === 0 && machine.length === 0 && (
          <div className="sessionrail__empty sessionrail__empty--guide">
            <span className="sessionrail__empty-icon">◌</span>
            你的对话会按创建时间列在这里
            <br />
            展开一个 session 可以看到每轮的第一个问题
          </div>
        )}
        {conversations.map((s) => {
          const expanded = expandedId === s.id;
          const prompts = sessionPrompts[s.id];
          return (
            <div key={s.id} className={`sessionrail__item${s.id === currentId ? " sessionrail__item--current" : ""}`}>
              <button
                className="sessionrail__row"
                onClick={() => setExpandedId(expanded ? null : s.id)}
                title={s.id}
              >
                <span className="sessionrail__caret">{expanded ? "▾" : "▸"}</span>
                <span className="sessionrail__session-title">{sessionLabel(s)}</span>
                <span className="sessionrail__session-meta">
                  <span className="sessionrail__session-date">{formatSessionDate(s.started)}</span>
                  <span className="sessionrail__session-count">{s.message_count} messages</span>
                </span>
              </button>
              {expanded && (
                <div className="sessionrail__prompts">
                  {prompts === undefined && <div className="sessionrail__empty">loading…</div>}
                  {prompts !== undefined && prompts.length === 0 && (
                    <div className="sessionrail__empty">no prompts</div>
                  )}
                  {prompts?.map((p, i) => (
                    <button
                      key={`${p.message_index}-${i}`}
                      className="sessionrail__prompt"
                      title={p.preview}
                      onClick={() => handlePromptClick(s.id, p.message_index)}
                    >
                      <span className="sessionrail__prompt-num">{i + 1}</span>
                      <span className="sessionrail__prompt-text">{p.preview}</span>
                    </button>
                  ))}
                </div>
              )}
            </div>
          );
        })}
        {machine.length > 0 && (
          <div className="sessionrail__machine">
            <button
              className="sessionrail__row sessionrail__row--machine"
              onClick={() => setMachineOpen(!machineOpen)}
              title="后台自动生成的会话（AutoDream 记忆整理 / bench smoke），不参与 auto-resume"
            >
              <span className="sessionrail__caret">{machineOpen ? "▾" : "▸"}</span>
              <span className="sessionrail__date">⚙ 后台会话</span>
              <span className="sessionrail__count">{machine.length}</span>
            </button>
            {machineOpen && (
              <div className="sessionrail__prompts">
                {machine.map((s) => (
                  <button
                    key={s.id}
                    className="sessionrail__prompt"
                    title={s.id}
                    onClick={() => {
                      // Machine sessions resume from the top (message index 0
                      // of their ledger); no prompt preview is fetched.
                      if (s.id === currentId) return;
                      useStore.getState().prepareSessionSwitch(s.id);
                      send({ type: "resume_session", id: s.id });
                    }}
                  >
                    <span className="sessionrail__prompt-num">{s.tag === "dream" ? "✿" : "◇"}</span>
                    <span className="sessionrail__prompt-text">
                      {formatStarted(s.started) || s.id.slice(0, 8)}
                    </span>
                  </button>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
      )}
    </div>
  );
}
