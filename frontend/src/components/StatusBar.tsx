import { useState, useEffect, useRef } from "react";
import { createPortal } from "react-dom";
import { useStore } from "../store";
import { THEME_COLORS, type Theme } from "../store/slices";

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

const ALL_THEMES = Object.keys(THEME_COLORS) as Theme[];

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
  const setTheme = useStore((s) => s.setTheme);
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
    const mm = String(d.getMonth() + 1).padStart(2, "0");
    const dd = String(d.getDate()).padStart(2, "0");
    const hh = String(d.getHours()).padStart(2, "0");
    const mi = String(d.getMinutes()).padStart(2, "0");
    return `${mm}-${dd} ${hh}:${mi}`;
  })();

  const [pickerOpen, setPickerOpen] = useState(false);
  const [pickerPos, setPickerPos] = useState<{ top: number; left: number } | null>(null);
  const dotRef = useRef<HTMLButtonElement>(null);
  const dropdownRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node) &&
          dotRef.current && !dotRef.current.contains(e.target as Node)) {
        setPickerOpen(false);
      }
    };
    if (pickerOpen) document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [pickerOpen]);

  const openPicker = () => {
    if (dotRef.current) {
      const r = dotRef.current.getBoundingClientRect();
      setPickerPos({ top: r.bottom + 6, left: r.left + r.width / 2 });
    }
    setPickerOpen(true);
  };

  const dotColor = THEME_COLORS[theme];

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
            {sessionId.slice(0, 8)}{sessionDateLabel ? ` · ${sessionDateLabel}` : ""} ▾
          </button>
        )}
        <button
          ref={dotRef}
          className="theme-dot"
          style={{ background: dotColor }}
          onClick={openPicker}
          title={`Theme: ${theme} (click to change)`}
          aria-label="Pick theme color"
        />
        {pickerOpen && pickerPos && createPortal(
          <div
            ref={dropdownRef}
            className="theme-dropdown"
            style={{ top: pickerPos.top, left: pickerPos.left }}
          >
            {ALL_THEMES.map((name) => (
              <button
                key={name}
                className="theme-option"
                style={{ background: THEME_COLORS[name] }}
                onClick={() => { setTheme(name); setPickerOpen(false); }}
                title={name}
                aria-label={`Theme: ${name}`}
              >
                {name === theme && <span className="theme-check" />}
              </button>
            ))}
          </div>,
          document.body
        )}
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
