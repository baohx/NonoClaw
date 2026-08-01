import type { SessionInfoWire } from "../types";

interface Props {
  sessions: SessionInfoWire[];
  currentId: string;
  onNew: () => void;
  onResume: (id: string) => void;
  onClose: () => void;
}

/** Format an RFC3339 timestamp in the browser's local timezone as YYYY-MM-DD HH:mm. */
function formatLocalDate(started: string | null): string | null {
  if (!started) return null;
  const d = new Date(started);
  if (isNaN(d.getTime())) return null;
  const yyyy = d.getFullYear();
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  const hh = String(d.getHours()).padStart(2, "0");
  const mi = String(d.getMinutes()).padStart(2, "0");
  return `${yyyy}-${mm}-${dd} ${hh}:${mi}`;
}

export default function SessionPicker({ sessions, currentId, onNew, onResume, onClose }: Props) {
  // Sort by creation time (started) descending, falling back to id.
  const sorted = [...sessions].sort((a, b) => {
    const ta = a.started ? new Date(a.started).getTime() : 0;
    const tb = b.started ? new Date(b.started).getTime() : 0;
    return tb - ta;
  });

  return (
    <div className="dialog-overlay top" onClick={onClose}>
      <div className="dialog" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 500 }}>
        <div className="dialog__eyebrow mint">sessions</div>
        <div className="dialog__title">Resume a conversation</div>
        <button className="sp-new" onClick={onNew}>
          + start a new session
        </button>
        {sorted.length === 0 && (
          <div style={{ color: "var(--faint)", fontSize: 13, padding: "6px 2px" }}>
            No prior sessions in this directory.
          </div>
        )}
        <div className="sp-list">
          {sorted.map((s) => {
            const active = s.id === currentId;
            const date = formatLocalDate(s.started);
            return (
              <button
                key={s.id}
                className={`sp-row${active ? " active" : ""}`}
                onClick={() => onResume(s.id)}
              >
                <div className="sp-row__top">
                  <span className="sp-row__date">{date ?? s.id.slice(0, 8)}</span>
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
