import type { SessionInfoWire } from "../types";

interface Props {
  sessions: SessionInfoWire[];
  currentId: string;
  onNew: () => void;
  onResume: (id: string) => void;
  onClose: () => void;
}

export default function SessionPicker({ sessions, currentId, onNew, onResume, onClose }: Props) {
  return (
    <div className="dialog-overlay top" onClick={onClose}>
      <div className="dialog" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 500 }}>
        <div className="dialog__eyebrow mint">sessions</div>
        <div className="dialog__title">Resume a conversation</div>
        <button className="sp-new" onClick={onNew}>
          + start a new session
        </button>
        {sessions.length === 0 && (
          <div style={{ color: "var(--faint)", fontSize: 13, padding: "6px 2px" }}>
            No prior sessions in this directory.
          </div>
        )}
        <div style={{ display: "flex", flexDirection: "column", gap: 7, marginTop: 4 }}>
          {sessions.map((s) => {
            const active = s.id === currentId;
            return (
              <button
                key={s.id}
                className={`sp-row${active ? " active" : ""}`}
                onClick={() => onResume(s.id)}
              >
                <div className="sp-row__top">
                  <span className="sp-row__date">{s.started ?? s.id.slice(0, 8)}</span>
                  <span className="sp-row__count">{s.message_count} msgs</span>
                  {active && <span className="sp-tag">current</span>}
                </div>
                <div className="sp-row__sum">{s.summary.trim() || "New conversation"}</div>
              </button>
            );
          })}
        </div>
        <div className="dialog__actions">
          <button className="btn btn--ghost" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
