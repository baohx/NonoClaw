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
  onShowQr,
}: Props) {
  const inputTokens = useStore((s) => s.inputTokens);
  const outputTokens = useStore((s) => s.outputTokens);
  const theme = useStore((s) => s.theme);
  const hasMobileAccessToken = useStore((s) => s.hasMobileAccessToken);
  const permissionMode = useStore((s) => s.permissionMode);
  const availableModels = useStore((s) => s.availableModels);
  const breathState = useStore((s) => s.breathState);
  const breathLabel = useStore((s) => s.breathLabel);

  const cycleTheme = useStore((s) => s.cycleTheme);
  const dotColor = theme === "amber" ? "#ff9f0a" : theme === "frost" ? "#0a84ff" : "#0071e3";

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
        >
          {leftRailCollapsed ? "»" : "«"}
        </button>
        <span className="statusbar__brand">
          Nono<i>Claw</i>
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
            {sessionId.slice(0, 8)} ▾
          </button>
        )}
        <button
          className="theme-dot"
          style={{ background: dotColor }}
          onClick={cycleTheme}
          title={`Theme: ${theme} (click to cycle)`}
          aria-label="Cycle theme"
        />
        <button
          className="iconbtn"
          onClick={onToggleInsight}
          title={insightCollapsed ? "Show insight panel" : "Hide insight panel"}
          aria-label="Toggle insight rail"
        >
          {insightCollapsed ? "«" : "»"}
        </button>
        {hasMobileAccessToken && (
          <button
            className="iconbtn"
            onClick={onShowQr}
            title="Show QR code for mobile access"
            aria-label="Show QR code"
          >
            &#x25f0;
          </button>
        )}
        <span className="breath-status" role="status" aria-live="polite">
          {breathLabel}
        </span>
        <span className={dotClass} title={`${breathLabel} · ${connectionStatus}`} data-phase={breathState} />
      </div>
    </div>
  );
}
