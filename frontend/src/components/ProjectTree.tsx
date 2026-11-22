import { useState } from "react";
import type { ClientMsg, RegisteredProject, SessionInfoWire } from "../types";

interface Props {
  projects: RegisteredProject[];
  activePath: string;
  expandedPath: string | null;
  sessionsByPath: Record<string, SessionInfoWire[]>;
  send: (msg: ClientMsg) => void;
  /** Called when a project node should expand (path) or collapse (null). */
  onToggleExpand: (path: string | null) => void;
  onOpenSession: (sessionId: string) => void;
}

function sessionLabel(s: SessionInfoWire): string {
  if (s.title?.trim()) return s.title.trim();
  if (s.summary?.trim()) {
    const first = s.summary.trim().split("\n")[0];
    return first.length > 42 ? `${first.slice(0, 42)}…` : first;
  }
  return s.started
    ? new Date(s.started).toLocaleDateString()
    : s.id.slice(0, 8);
}

/** Cursor-style project tree: top-level nodes are projects; expanding a
 *  project lists its sessions without switching the active project. */
export default function ProjectTree({
  projects,
  activePath,
  expandedPath,
  sessionsByPath,
  send,
  onToggleExpand,
  onOpenSession,
}: Props) {
  const [adding, setAdding] = useState(false);
  const [addPath, setAddPath] = useState("");

  const submitAdd = () => {
    const path = addPath.trim();
    if (!path) return;
    send({ type: "add_project", path });
    setAddPath("");
    setAdding(false);
  };

  return (
    <div className="projecttree">
      <div className="projecttree__head">
        <span className="projecttree__title">Projects</span>
        <button
          className="projecttree__add"
          onClick={() => setAdding(!adding)}
          title="Add a project directory"
        >
          {adding ? "×" : "+"}
        </button>
      </div>
      {adding && (
        <div className="projecttree__addform">
          <input
            className="projecttree__input"
            placeholder="/absolute/project/path"
            value={addPath}
            onChange={(e) => setAddPath(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submitAdd();
              if (e.key === "Escape") setAdding(false);
            }}
            autoFocus
          />
          <button className="projecttree__confirm" onClick={submitAdd}>
            Add
          </button>
        </div>
      )}
      <div className="projecttree__list">
        {projects.map((p) => {
          const expanded = expandedPath === p.path;
          const isActive = p.path === activePath;
          const sessions = sessionsByPath[p.path];
          return (
            <div key={p.path} className="projecttree__node">
              <div className="projecttree__row">
                <button
                  className={`projecttree__caret${expanded ? " projecttree__caret--open" : ""}`}
                  onClick={() => {
                    if (expanded) {
                      onToggleExpand(null);
                    } else {
                      send({ type: "list_project_sessions", path: p.path });
                      onToggleExpand(p.path);
                    }
                  }}
                  aria-expanded={expanded}
                  title={expanded ? "Collapse" : "Show sessions"}
                >
                  {expanded ? "▾" : "▸"}
                </button>
                <button
                  className={`projecttree__name${isActive ? " projecttree__name--active" : ""}`}
                  onClick={() => {
                    if (!isActive) send({ type: "switch_project", path: p.path });
                  }}
                  title={p.path}
                >
                  {p.name}
                  {isActive && <span className="projecttree__activemark"> ●</span>}
                </button>
                {!isActive && (
                  <button
                    className="projecttree__remove"
                    onClick={() => send({ type: "remove_project", path: p.path })}
                    title={`Remove ${p.name} from the registry (does not delete files)`}
                  >
                    ×
                  </button>
                )}
              </div>
              {expanded && (
                <div className="projecttree__sessions">
                  {sessions === undefined && (
                    <div className="projecttree__empty">loading…</div>
                  )}
                  {sessions !== undefined && sessions.length === 0 && (
                    <div className="projecttree__empty">no sessions</div>
                  )}
                  {sessions?.map((s) => (
                    <button
                      key={s.id}
                      className="projecttree__session"
                      title={
                        isActive
                          ? s.id
                          : "Switch to this project to open its sessions"
                      }
                      disabled={!isActive}
                      onClick={() => onOpenSession(s.id)}
                    >
                      <span className="projecttree__session-title">
                        {sessionLabel(s)}
                      </span>
                      <span className="projecttree__session-meta">
                        {s.message_count} msg
                      </span>
                    </button>
                  ))}
                </div>
              )}
            </div>
          );
        })}
        {projects.length === 0 && (
          <div className="projecttree__empty">No projects registered</div>
        )}
      </div>
    </div>
  );
}
