import { useStore } from "../store";

interface Props {
  model: string;
  sessionId: string;
  connectionStatus: "connecting" | "connected" | "disconnected";
  onOpenSessions: () => void;
  onShowQr: () => void;
  compacting: boolean;
  leftRailCollapsed: boolean;
  insightCollapsed: boolean;
  onToggleLeftRail: () => void;
  onToggleInsight: () => void;
  onToggleApiLog: () => void;
  onOpenVideoStudio: () => void;
  onToggleGovernance: () => void;
}

export default function StatusBar({
  model,
  sessionId,
  connectionStatus,
  onOpenSessions,
  compacting,
  leftRailCollapsed,
  insightCollapsed,
  onToggleLeftRail,
  onToggleInsight,
  onToggleApiLog,
  onOpenVideoStudio,
  onToggleGovernance,
  onShowQr,
}: Props) {
  const inputTokens = useStore((s) => s.inputTokens);
  const outputTokens = useStore((s) => s.outputTokens);
  const hasMobileAccessToken = useStore((s) => s.hasMobileAccessToken);
  const permissionMode = useStore((s) => s.permissionMode);
  const availableModels = useStore((s) => s.availableModels);
  const breathState = useStore((s) => s.breathState);
  const breathLabel = useStore((s) => s.breathLabel);
  const sessions = useStore((s) => s.sessions);

  // Find the current session's started timestamp for display.
  const currentStarted = sessions.find((s) => s.id === sessionId)?.started ?? null;
  const sessionDateLabel = (() => {
    if (!currentStarted) return null;
    const d = new Date(currentStarted);
    if (isNaN(d.getTime())) return null;
    const yyyy = d.getFullYear();
    const mm = String(d.getMonth() + 1).padStart(2, "0");
    const dd = String(d.getDate()).padStart(2, "0");
    const hh = String(d.getHours()).padStart(2, "0");
    const mi = String(d.getMinutes()).padStart(2, "0");
    return `${yyyy}-${mm}-${dd} ${hh}:${mi}`;
  })();

  const dotClass = [
    "breath-dot",
    breathState === "connecting" || breathState === "reconnecting" || breathState.startsWith("waiting")
      ? "connecting"
      : breathState === "error"
      ? "off"
      : "",
  ].filter(Boolean).join(" ");

  return (
    <div className="statusbar">
      <div className="statusbar__side">
        <button
          className="iconbtn"
          onClick={onToggleLeftRail}
          title={leftRailCollapsed ? "Show file tree + git" : "Hide file tree + git"}
          aria-label="Toggle left rail"
          aria-pressed={!leftRailCollapsed}
        >
          <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
            <rect x="3" y="3" width="14" height="14" rx="2" /><path d="M7 3v14" />
          </svg>
        </button>
        <span className="statusbar__brand">
          <svg className="brand-mark" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
            <path d="m8 6-5 6 5 6M16 6l5 6-5 6M14 3l-4 18" />
          </svg>
          NonoClaw
        </span>
        {model ? (
          <>
            <span className="statusbar__divider" />
            <span className="statusbar__model" title="Current model (change the next run in the composer)">{availableModels.find((item) => item.name === model)?.label || model}</span>
          </>
        ) : null}
        {compacting && <span className="tag-compact">◌ compacting</span>}
        <span className="statusbar__model" title="Current execution mode (change the next run in the composer)">{permissionMode}</span>
      </div>

      <div className="statusbar__side">
        {(inputTokens > 0 || outputTokens > 0) && (
          <span className="statusbar__tokens">
            <b>in</b> {inputTokens.toLocaleString()} · <b>out</b>{" "}
            {outputTokens.toLocaleString()}
          </span>
        )}
        {sessionId && (
          <button className="session-pill" onClick={onOpenSessions} title="Switch / resume session">
            {sessionId.slice(0, 8)}{sessionDateLabel ? ` · ${sessionDateLabel}` : ""} ▾
          </button>
        )}
        <span className="session-status" role="status" title={`${breathLabel} · ${connectionStatus}`} aria-live="polite">
          <span className={dotClass} data-phase={breathState} aria-hidden="true" />
          <span className="breath-status">{breathLabel === "idle" ? "Ready" : breathLabel}</span>
        </span>
        <button
          className="iconbtn"
          onClick={onToggleInsight}
          title={insightCollapsed ? "Show insight panel" : "Hide insight panel"}
          aria-label="Toggle insight rail"
          aria-pressed={!insightCollapsed}
        >
          <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
            <rect x="3" y="3" width="14" height="14" rx="2" /><path d="M13 3v14" />
          </svg>
        </button>
        <button
          className="iconbtn"
          onClick={onToggleApiLog}
          title="Show raw API log viewer"
          aria-label="Toggle API log viewer"
        >
          <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
            <path d="m7 5-4 5 4 5m6-10 4 5-4 5M11 3l-2 14" />
          </svg>
        </button>
        <button
          className="iconbtn"
          onClick={onOpenVideoStudio}
          title="Video generation studio (Seedance)"
          aria-label="Open video studio"
        >
          <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
            <rect x="3" y="6" width="10" height="9" rx="1.5" /><path d="m13 9 4-2v7l-4-2M6 6V3m5 3V3" />
          </svg>
        </button>
        <button
          className="iconbtn"
          onClick={onToggleGovernance}
          title="Trajectory governance — tree, loop/retry/drift detection"
          aria-label="Toggle trajectory governance"
        >
          <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
            <circle cx="10" cy="4" r="2" /><circle cx="5" cy="16" r="2" /><circle cx="15" cy="16" r="2" /><path d="M10 6v4m0 0-5 4m5-4 5 4" />
          </svg>
        </button>
        {hasMobileAccessToken && (
          <button
            className="iconbtn"
            onClick={onShowQr}
            title="Show QR code for mobile access"
            aria-label="Show QR code"
          >
            <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
              <path d="M3 8V3h5m4 0h5v5M3 12v5h5m4 0h5v-5M7 7h1m4 0h1M7 12h1m4 0h1" />
            </svg>
          </button>
        )}

      </div>
    </div>
  );
}
