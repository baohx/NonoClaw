/**
 * Trajectory ledger view — ported from DSH `ui-trajectory` `TrajectoryView.tsx`.
 * Turn-aware event ledger with four-mode timeline overview, drag-select
 * focus, incremental search, virtualized rows and a local details inspector.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useStore } from "../store";
import {
  buildLedgerLayout,
  deriveTrajectoryTimeline,
  ledgerRecordId,
  projectLedgerRows,
  timelineSelectionForRange,
  TrajectorySearchIndex,
  visibleLedgerRows,
  formatDurationMillis,
  formatElapsedSeconds,
  formatTokenCount,
} from "../ledger";
import type { LedgerCell, TrajectoryTimelineMode } from "../ledger";

const MODES: TrajectoryTimelineMode[] = ["sequence", "duration", "time", "actual"];
const MODE_LABEL: Record<TrajectoryTimelineMode, string> = {
  sequence: "Sequence",
  duration: "Duration",
  time: "Time",
  actual: "Actual",
};
const LANES = ["assistant", "thinking", "tool", "request"] as const;
const LANE_LABEL: Record<(typeof LANES)[number], string> = {
  assistant: "assistant",
  thinking: "thinking",
  tool: "tool",
  request: "request",
};

function clockTime(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return "";
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}:${String(d.getSeconds()).padStart(2, "0")}`;
}

export default function TrajectoryLedger() {
  const messages = useStore((s) => s.messages);
  const traceEntries = useStore((s) => s.traceEntries);
  const subagentRunsById = useStore((s) => s.subagentRunsById);

  const [mode, setMode] = useState<TrajectoryTimelineMode>("sequence");
  const [query, setQuery] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [focusedIndices, setFocusedIndices] = useState<Set<number> | null>(null);
  const [followTail, setFollowTail] = useState(true);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportH, setViewportH] = useState(480);
  const [collapsedTurns, setCollapsedTurns] = useState<Set<number>>(new Set());
  const [dragRange, setDragRange] = useState<{ start: number; end: number } | null>(null);
  // Overview zoom/pan: `zoom` is a magnification factor (1 = whole domain fits,
  // 8 = 8x magnified) and `pan` is the domain start of the visible window
  // (0..1-1/zoom). Both live in one atom so the pan clamp stays consistent with
  // zoom during wheel updates.
  const [view, setView] = useState({ zoom: 1, pan: 0 });

  const viewportRef = useRef<HTMLDivElement | null>(null);
  const overviewRef = useRef<HTMLDivElement | null>(null);
  const indexRef = useRef<TrajectorySearchIndex>(new TrajectorySearchIndex());

  const layout = useMemo(
    () => buildLedgerLayout({ messages, traceEntries, subagentRunsById }),
    [messages, traceEntries, subagentRunsById],
  );

  const projection = useMemo(
    () => projectLedgerRows(layout.turns, collapsedTurns),
    [layout, collapsedTurns],
  );

  const timeline = useMemo(() => deriveTrajectoryTimeline(layout.turns, mode), [layout, mode]);

  useEffect(() => {
    indexRef.current.addCells(projection.cells);
  }, [projection.cells]);

  const matches = useMemo(() => (query.trim().length > 0 ? indexRef.current.search(query) : null), [query, projection.cells]);

  const selected = useMemo(() => {
    if (selectedId === null) return null;
    for (const turn of layout.turns) {
      for (const cell of turn.cells) {
        if (ledgerRecordId(cell) === selectedId) return cell;
      }
    }
    return null;
  }, [selectedId, layout]);

  const window_ = useMemo(
    () => visibleLedgerRows(projection, scrollTop, viewportH),
    [projection, scrollTop, viewportH],
  );

  // Follow-tail: keep pinned to bottom while enabled.
  useEffect(() => {
    if (followTail && viewportRef.current !== null) {
      viewportRef.current.scrollTop = projection.totalHeight;
    }
  }, [projection.totalHeight, followTail]);

  const onScroll = useCallback(() => {
    const el = viewportRef.current;
    if (el === null) return;
    setScrollTop(el.scrollTop);
    const atTop = el.scrollTop < 24;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
    if (atBottom) setFollowTail(true);
    else if (!atTop || el.scrollTop > 48) setFollowTail(false);
  }, []);

  // Map a pointer x (px within the overview) to a normalized domain position.
  const screenToDomain = useCallback((x: number, width: number) => {
    if (width <= 0) return 0;
    return Math.max(0, Math.min(1, view.pan + (x / width) * (1 / view.zoom)));
  }, [view]);

  // Map a normalized domain position to a 0..1 screen fraction of the overview.
  const domainToScreen = useCallback((d: number) => {
    return (d - view.pan) / (1 / view.zoom);
  }, [view]);

  // Drag-select on the overview: focus records overlapping the range.
  const dragRef = useRef<{ x: number; width: number } | null>(null);
  const onMouseDownOverview = useCallback((e: React.MouseEvent) => {
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    dragRef.current = { x: e.clientX - rect.left, width: rect.width };
    const d = screenToDomain(e.clientX - rect.left, rect.width);
    setDragRange({ start: d, end: d });
  }, [screenToDomain]);
  const onMouseMoveOverview = useCallback((e: React.MouseEvent) => {
    if (dragRef.current === null) return;
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const d = screenToDomain(e.clientX - rect.left, rect.width);
    setDragRange((prev) => (prev === null ? { start: d, end: d } : { start: prev.start, end: d }));
  }, [screenToDomain]);
  const onMouseUpOverview = useCallback(() => {
    if (dragRange !== null) {
      const sel = timelineSelectionForRange(timeline, dragRange);
      setFocusedIndices(sel.size > 0 ? sel : null);
    }
    dragRef.current = null;
  }, [dragRange, timeline]);
  const onContextMenuOverview = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    setDragRange(null);
    setFocusedIndices(null);
  }, []);

  // Wheel zoom on the overview (time modes only). Attached as a non-passive
  // listener so `preventDefault` actually stops the surrounding scroll — the
  // synthetic React onWheel is passive and cannot cancel the page scroll.
  // Plain wheel = zoom at cursor; Shift+wheel = pan.
  const isTimeDomainRef = useRef(false);
  isTimeDomainRef.current = timeline?.isTimeDomain ?? false;
  useEffect(() => {
    const el = overviewRef.current;
    if (el === null) return;
    const onWheel = (e: WheelEvent) => {
      if (!isTimeDomainRef.current) return;
      e.preventDefault();
      setView((v) => {
        const maxPan = 1 - 1 / v.zoom;
        if (e.shiftKey) {
          // Pan: delta in px → normalized visible window, then domain units.
          const step = (e.deltaY / Math.max(1, el.clientWidth)) / v.zoom;
          return { zoom: v.zoom, pan: Math.max(0, Math.min(maxPan, v.pan + step)) };
        }
        const rect = el.getBoundingClientRect();
        const frac = rect.width > 0 ? (e.clientX - rect.left) / rect.width : 0.5;
        const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15;
        const zoom = Math.min(8, Math.max(1, v.zoom * factor));
        const newMaxPan = 1 - 1 / zoom;
        // Keep the domain point under the cursor fixed while zooming.
        const domainAtCursor = v.pan + frac / v.zoom;
        const pan = Math.max(0, Math.min(newMaxPan, domainAtCursor - frac / zoom));
        return { zoom, pan };
      });
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  // Double-click resets zoom/pan back to the full domain.
  const onDoubleClickOverview = useCallback(() => {
    setView({ zoom: 1, pan: 0 });
  }, []);

  // Reset zoom/pan whenever the view mode changes (the domain changes shape).
  useEffect(() => {
    setView({ zoom: 1, pan: 0 });
  }, [mode]);

  const toggleTurn = useCallback((n: number) => {
    setCollapsedTurns((prev) => {
      const next = new Set(prev);
      if (next.has(n)) next.delete(n);
      else next.add(n);
      return next;
    });
  }, []);

  const dimmed = useCallback((cell: LedgerCell) => {
    if (matches !== null && !matches.has(`${cell.index}`)) return true;
    if (focusedIndices !== null && !focusedIndices.has(cell.index)) return true;
    return false;
  }, [matches, focusedIndices]);

  const stats = useMemo(() => {
    let tools = 0;
    let errors = 0;
    let tokensIn = 0;
    let tokensOut = 0;
    let cacheRead = 0;
    for (const turn of layout.turns) {
      if (turn.usage !== null) {
        tokensIn += turn.usage.input;
        tokensOut += turn.usage.output;
        cacheRead += turn.usage.cacheRead;
      }
      for (const cell of turn.cells) {
        // Subagent-internal tools ride under the parent turn but are a
        // separate branch — exclude them from the main agent's tool count
        // (they also carry no timestamps, so counting them here both inflates
        // the number and leaves a pile of "—" rows in the time column).
        if (cell.kind === "tool" && cell.subagentRunId == null) tools += 1;
        if (cell.isError) errors += 1;
      }
    }
    return { tools, errors, tokensIn, tokensOut, cacheRead };
  }, [layout]);

  return (
    <div className="ledger">
      <div className="ledger-toolbar">
        <span className="ledger-toolbar__title">Trajectory ledger</span>
        <span className="ledger-toolbar__stats">
          {layout.turns.length} turns · {stats.tools} tools · {stats.errors} errors · in {formatTokenCount(stats.tokensIn)} · out {formatTokenCount(stats.tokensOut)} · cache {formatTokenCount(stats.cacheRead)}
        </span>
        <input
          className="ledger-search"
          placeholder="Search ledger…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <div className="ledger-modes">
          {MODES.map((m) => (
            <button key={m} className={`ledger-mode${mode === m ? " ledger-mode--active" : ""}`} onClick={() => setMode(m)}>{MODE_LABEL[m]}</button>
          ))}
        </div>
      </div>

      <div
        className="ledger-overview"
        ref={overviewRef}
        onMouseDown={onMouseDownOverview}
        onMouseMove={onMouseMoveOverview}
        onMouseUp={onMouseUpOverview}
        onMouseLeave={onMouseUpOverview}
        onDoubleClick={onDoubleClickOverview}
        onContextMenu={onContextMenuOverview}
      >
        {timeline === null ? (
          <div className="ledger-overview__empty">No measurable records yet</div>
        ) : (
          <>
            <div className="ledger-overview__lanes">
              {LANES.map((lane) => (
                <div key={lane} className={`ledger-overview__lane ledger-overview__lane--${lane}`}>
                  <span className={`ledger-overview__lane-label ledger-overview__lane-label--${lane}`}>{LANE_LABEL[lane]}</span>
                  {timeline.spans.filter((s) => s.lane === lane).map((span) => (
                    <span
                      key={span.index}
                      className={`ledger-overview__span ledger-overview__span--${lane}${span.isError ? " ledger-overview__span--error" : ""}`}
                      style={{
                        left: `${domainToScreen(span.start) * 100}%`,
                        width: `${Math.max(0.4, (domainToScreen(span.end) - domainToScreen(span.start)) * 100)}%`,
                      }}
                      title={`#${span.index} · ${span.kind}`}
                    />
                  ))}
                </div>
              ))}
            </div>
            {timeline.idleBreaks.map((brk, i) => (
              <span key={i} className="ledger-overview__idle" style={{ left: `${domainToScreen(brk.at) * 100}%` }} title={`idle gap collapsed · saved ${Math.round(brk.savedSeconds)}s`}>⌁</span>
            ))}
            {dragRange !== null && (
              <span
                className="ledger-overview__selection"
                style={{
                  left: `${domainToScreen(Math.min(dragRange.start, dragRange.end)) * 100}%`,
                  width: `${Math.abs(domainToScreen(dragRange.end) - domainToScreen(dragRange.start)) * 100}%`,
                }}
              />
            )}
          </>
        )}
      </div>

      <div className="ledger-table" role="table" aria-label="trajectory ledger">
        <div className="ledger-table__head">
          <span className="ledger-table__h ledger-table__h--index">#</span>
          <span className="ledger-table__h ledger-table__h--event">Event</span>
          <span className="ledger-table__h ledger-table__h--time">Time</span>
        </div>
        <div
          className="ledger-table__body"
          ref={viewportRef}
          onScroll={onScroll}
          style={{ height: Math.min(420, Math.max(220, viewportH)) }}
        >
          <div style={{ height: projection.totalHeight, position: "relative" }}>
            <div style={{ transform: `translateY(${window_.offsetY}px)` }}>
              {window_.rows.map((row) => {
                if (row.kind === "turn-header") {
                  const collapsed = collapsedTurns.has(row.turn);
                  return (
                    <div key={row.key} className="ledger-turn-head" style={{ height: row.height }}>
                      <button className="ledger-turn-head__toggle" onClick={() => toggleTurn(row.turn)} aria-expanded={!collapsed}>
                        {collapsed ? "▸" : "▾"}
                      </button>
                      <span className="ledger-turn-head__label">Turn {row.turn + 1}</span>
                      {row.request?.provider && <span className="ledger-turn-head__meta">{row.request.provider}</span>}
                      {row.request?.model && <span className="ledger-turn-head__meta">{row.request.model}</span>}
                      {row.request?.retryAttempt !== undefined && row.request.retryAttempt > 1 && (
                        <span className="ledger-turn-head__meta ledger-turn-head__meta--retry">retry {row.request.retryAttempt}</span>
                      )}
                      {row.usage && (
                        <span className="ledger-turn-head__meta">
                          {formatTokenCount(row.usage.input)} in / {formatTokenCount(row.usage.output)} out{row.usage.cacheRead > 0 ? ` · ${formatTokenCount(row.usage.cacheRead)} cached` : ""}
                        </span>
                      )}
                    </div>
                  );
                }
                if (row.kind === "between-header") {
                  return (
                    <div key={row.key} className="ledger-turn-head ledger-turn-head--between" style={{ height: row.height }}>
                      <span className="ledger-turn-head__label">Between turns</span>
                    </div>
                  );
                }
                if (row.kind === "turn-divider") {
                  return <div key={row.key} className="ledger-divider" style={{ height: row.height }} />;
                }
                const cell = row.cell;
                if (cell === undefined) return null;
                const selectedRow = selectedId === ledgerRecordId(cell);
                return (
                  <div
                    key={row.key}
                    className={`ledger-row ledger-row--${cell.kind}${selectedRow ? " ledger-row--selected" : ""}${dimmed(cell) ? " ledger-row--dim" : ""}${cell.isError ? " ledger-row--error" : ""}${cell.requestOnly ? " ledger-row--request-only" : ""}`}
                    style={{ height: row.height }}
                    onClick={() => setSelectedId(selectedRow ? null : ledgerRecordId(cell))}
                    role="row"
                  >
                    <span className="ledger-row__index">#{cell.index}</span>
                    <span className="ledger-row__event">
                      <span className={`ledger-row__kind ledger-row__kind--${cell.kind}`}>{cell.kind}</span>
                      <span className="ledger-row__text" title={cell.text}>{cell.text}</span>
                      {matches !== null && matches.has(`${cell.index}`) && <span className="ledger-row__hit">●</span>}
                    </span>
                    <span className="ledger-row__time">
                      {cell.requestOnly === true
                        ? "…"
                        : formatElapsedSeconds(cell.timeSeconds)}
                    </span>
                  </div>
                );
              })}
            </div>
          </div>
        </div>
      </div>

      {selected !== null && (
        <div className="ledger-inspector">
          <div className="ledger-inspector__head">
            <span className={`ledger-row__kind ledger-row__kind--${selected.kind}`}>{selected.kind}</span>
            <span className="ledger-inspector__title">#{selected.index} · {selected.text}</span>
            <button className="ledger-inspector__close" onClick={() => setSelectedId(null)}>×</button>
          </div>
          <div className="ledger-inspector__facts">
            <Fact label="Duration" value={formatElapsedSeconds(selected.timeSeconds)} />
            <Fact label="Started" value={selected.startedAt != null ? clockTime(selected.startedAt) : "—"} />
            {selected.input !== undefined && <Fact label="Input tokens" value={formatTokenCount(selected.input)} />}
            {selected.cacheRead !== undefined && <Fact label="Cache read" value={formatTokenCount(selected.cacheRead)} />}
            {selected.cacheWrite !== undefined && <Fact label="Cache write" value={formatTokenCount(selected.cacheWrite)} />}
            {selected.output !== undefined && <Fact label="Output tokens" value={formatTokenCount(selected.output)} />}
            {selected.assistantMetrics?.timingRecorded === true && (
              <>
                <Fact
                  label="TTFT"
                  value={selected.assistantMetrics.stepStartTime !== null && selected.assistantMetrics.firstTokenTime !== null
                    ? formatDurationMillis(selected.assistantMetrics.firstTokenTime - selected.assistantMetrics.stepStartTime)
                    : "—"}
                />
                <Fact
                  label="Decode"
                  value={selected.assistantMetrics.firstTokenTime !== null && selected.assistantMetrics.completedTime !== null
                    ? formatDurationMillis(selected.assistantMetrics.completedTime - selected.assistantMetrics.firstTokenTime)
                    : "—"}
                />
                {selected.assistantMetrics.outputTokens != null && selected.assistantMetrics.firstTokenTime != null && selected.assistantMetrics.completedTime != null && (
                  <Fact
                    label="Throughput"
                    value={`${(
                      selected.assistantMetrics.outputTokens /
                      Math.max(0.001, (selected.assistantMetrics.completedTime - selected.assistantMetrics.firstTokenTime) / 1000)
                    ).toFixed(1)} tok/s`}
                  />
                )}
              </>
            )}
          </div>
          {selected.inputDetail && <InspectorSection title="Input">{selected.inputDetail}</InspectorSection>}
          {selected.thinkingDetail && <InspectorSection title="Thinking">{selected.thinkingDetail}</InspectorSection>}
          {selected.outputDetail && <InspectorSection title="Output">{selected.outputDetail}</InspectorSection>}
        </div>
      )}

      <div className="ledger-footnote">
        {followTail ? "following tail" : "scroll up to pause"} · drag on overview to focus · right-click clears · wheel zooms time modes
      </div>
    </div>
  );
}

function Fact({ label, value }: { label: string; value: string }) {
  return (
    <div className="ledger-fact">
      <span className="ledger-fact__label">{label}</span>
      <span className="ledger-fact__value">{value}</span>
    </div>
  );
}

function InspectorSection({ title, children }: { title: string; children: string }) {
  return (
    <div className="ledger-inspector__section">
      <div className="ledger-inspector__section-title">{title}</div>
      <pre className="ledger-inspector__code">{children}</pre>
    </div>
  );
}
