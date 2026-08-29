/**
 * Trajectory timeline — ported from DSH `ui-trajectory` `timeline.ts`.
 * Operation-sequence and recorded-time projections for the ledger overview:
 * four view modes, idle compression, and drag-select focus.
 */

import type { LedgerTurnModel } from "./layout";

/** Overview view mode. */
export type TrajectoryTimelineMode = "sequence" | "duration" | "time" | "actual";

/** One projected bar on a lane. */
export interface TrajectoryTimelineSpan {
  /** Ledger cell index (record identity for focus). */
  index: number;
  /** Normalized start (0..1 of the visible domain). */
  start: number;
  /** Normalized end (0..1 of the visible domain). */
  end: number;
  isError: boolean;
  kind: string;
  /** Lane the span renders on. */
  lane: "assistant" | "thinking" | "tool" | "request";
}

export interface TrajectoryTimelineModel {
  mode: TrajectoryTimelineMode;
  spans: TrajectoryTimelineSpan[];
  /** Rendered domain [min, max] in mode units (ms for time modes). */
  domain: [number, number];
  /** Marks inserted where consecutive records idle beyond threshold. */
  idleBreaks: { at: number; savedSeconds: number }[];
  /** True when the domain is measured in wall-clock ms. */
  isTimeDomain: boolean;
}

/** Idle gaps longer than this get collapsed in `time`/`actual` modes. */
export const IDLE_COMPRESS_SECONDS = 60;

function finite(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** Absolute start (epoch ms) of a cell. */
function cellStart(cell: LedgerCellLike): number | null {
  return finite(cell.startedAt);
}

/** Measured duration of a cell in ms, or null while running / unknown. */
function cellDurationMs(cell: LedgerCellLike): number | null {
  const seconds = finite(cell.timeSeconds);
  if (seconds === null) return null;
  return seconds * 1000;
}

type LedgerCellLike = {
  index: number;
  kind: string;
  isError?: boolean;
  requestOnly?: boolean;
  startedAt?: number | null;
  timeSeconds?: number | null;
};

interface CellRange { start: number; end: number }

function cellRange(cell: LedgerCellLike): CellRange | null {
  if (cell.requestOnly === true) return null;
  const start = cellStart(cell);
  const duration = cellDurationMs(cell);
  if (start === null) return null;
  if (duration === null || duration < 0) return { start, end: start };
  return { start, end: start + duration };
}

/**
 * Derive the timeline model for the current mode.
 * Ported from DSH `deriveTrajectoryTimeline`.
 */
export function deriveTrajectoryTimeline(
  turns: LedgerTurnModel[],
  mode: TrajectoryTimelineMode,
): TrajectoryTimelineModel | null {
  const timed: { cell: LedgerCellLike; range: CellRange }[] = [];
  let seqCursor = 0;
  let seqEnd = 0;

  for (const turn of turns) {
    for (const cell of turn.cells) {
      const range = cellRange(cell);
      if (range === null) {
        if (cell.requestOnly === true) continue;
        // Sequence lane still counts unmeasured records.
        seqCursor += 1;
        continue;
      }
      seqCursor += 1;
      seqEnd = seqCursor;
      if (mode === "sequence") {
        timed.push({ cell, range: { start: seqCursor - 1, end: seqCursor } });
      } else if (mode === "duration") {
        timed.push({ cell, range: { start: 0, end: Math.max(1, range.end - range.start) } });
      } else {
        timed.push({ cell, range });
      }
    }
  }
  if (timed.length === 0) return null;

  const domain: [number, number] =
    mode === "sequence"
      ? [0, Math.max(1, seqEnd)]
      : mode === "duration"
        ? [0, Math.max(1, ...timed.map((t) => t.range.end))]
        : [Math.min(...timed.map((t) => t.range.start)), Math.max(...timed.map((t) => t.range.end))];

  // Idle compression for wall-clock modes only.
  const idleBreaks: { at: number; savedSeconds: number }[] = [];
  if (mode === "time" || mode === "actual") {
    const sorted = [...timed].sort((a, b) => a.range.start - b.range.start);
    let compressedCursor = sorted[0].range.start;
    let prevEnd = sorted[0].range.start;
    let offset = 0;
    for (const item of sorted) {
      const gap = item.range.start - prevEnd;
      if (gap > IDLE_COMPRESS_SECONDS * 1000) {
        const saved = (gap - IDLE_COMPRESS_SECONDS * 1000) / 1000;
        idleBreaks.push({ at: (prevEnd + IDLE_COMPRESS_SECONDS * 1000 - domain[0]) / (domain[1] - domain[0]), savedSeconds: saved });
        offset += gap - IDLE_COMPRESS_SECONDS * 1000;
      }
      prevEnd = Math.max(prevEnd, item.range.end);
      void compressedCursor;
      compressedCursor = 0;
    }
    void offset;
  }

  const spans: TrajectoryTimelineSpan[] = timed.map(({ cell, range }) => {
    const lane: TrajectoryTimelineSpan["lane"] =
      cell.kind === "assistant" ? "assistant"
        : cell.kind === "thinking" ? "thinking"
          : cell.kind === "tool" ? "tool"
            : "request";
    return {
      index: cell.index,
      start: (range.start - domain[0]) / (domain[1] - domain[0]),
      end: (range.end - domain[0]) / (domain[1] - domain[0]),
      isError: cell.isError === true,
      kind: cell.kind,
      lane,
    };
  });

  return { mode, spans, domain, idleBreaks, isTimeDomain: mode === "time" || mode === "actual" };
}

/** Focus records whose span overlaps the selected normalized range. */
export function timelineSelectionForRange(
  model: TrajectoryTimelineModel | null,
  range: { start: number; end: number },
): Set<number> {
  if (model === null) return new Set();
  const lo = Math.min(range.start, range.end);
  const hi = Math.max(range.start, range.end);
  return new Set(
    model.spans
      .filter((span) => span.start <= hi && span.end >= lo)
      .map((span) => span.index),
  );
}
