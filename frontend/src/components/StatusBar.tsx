import { useStore } from "../store";
import type { PermissionMode } from "../types";

interface Props {
  model: string;
  sessionId: string;
  connectionStatus: "connecting" | "connected" | "disconnected";
  onOpenSessions: () => void;
  onShowQr: () => void;
  onSetPermissionMode: (mode: PermissionMode) => void;
  onSetModel: (name: string) => void;
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
  onSetPermissionMode,
  onSetModel,
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
        {model && availableModels.length > 1 ? (
          <>
            <span className="statusbar__divider" />
            <select
              className="mode-select"
              value={model}
              onChange={(e) => onSetModel(e.target.value)}
              title="Switch model"
            >
              {availableModels.map((m) => (
                <option key={m.name} value={m.name}>
                  {m.label || m.name}
                </option>
              ))}
            </select>
          </>
        ) : model ? (
          <>
            <span className="statusbar__divider" />
            <span className="statusbar__model">{model}</span>
          </>
        ) : null}
        {compacting && <span className="tag-compact">◌ compacting</span>}
        <select
          className="mode-select"
          value={permissionMode}
          onChange={(e) => onSetPermissionMode(e.target.value as PermissionMode)}
          title="Permission mode"
        >
          <option value="default">default</option>
          <option value="acceptEdits">acceptEdits</option>
          <option value="auto">auto</option>
          <option value="bypassPermissions">bypass</option>
          <option value="plan">plan</option>
        </select>
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
