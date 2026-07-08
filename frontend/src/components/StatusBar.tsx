import { useStore } from "../store";

interface Props {
  model: string;
  sessionId: string;
  connectionStatus: "connecting" | "connected" | "disconnected";
  onOpenSessions: () => void;
  compacting: boolean;
  leftRailCollapsed: boolean;
  insightCollapsed: boolean;
  onToggleLeftRail: () => void;
  onToggleInsight: () => void;
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
}: Props) {
  const inputTokens = useStore((s) => s.inputTokens);
  const outputTokens = useStore((s) => s.outputTokens);

  const dotClass =
    connectionStatus === "connected"
      ? "breath-dot"
      : connectionStatus === "connecting"
      ? "breath-dot connecting"
      : "breath-dot off";

  return (
    <div className="statusbar">
      <div className="statusbar__side">
        <button
          className="iconbtn"
          onClick={onToggleLeftRail}
          title={leftRailCollapsed ? "Show file tree + git" : "Hide file tree + git"}
          aria-label="Toggle left rail"
        >
          {leftRailCollapsed ? "»" : "«"}
        </button>
        <span className="statusbar__brand">
          Nono<i>Claw</i>
        </span>
        {model && (
          <>
            <span className="statusbar__divider" />
            <span className="statusbar__model">{model}</span>
          </>
        )}
        {compacting && <span className="tag-compact">◌ compacting</span>}
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
            {sessionId.slice(0, 8)} ▾
          </button>
        )}
        <button
          className="iconbtn"
          onClick={onToggleInsight}
          title={insightCollapsed ? "Show insight panel" : "Hide insight panel"}
          aria-label="Toggle insight rail"
        >
          {insightCollapsed ? "«" : "»"}
        </button>
        <span className={dotClass} title={connectionStatus} />
      </div>
    </div>
  );
}
