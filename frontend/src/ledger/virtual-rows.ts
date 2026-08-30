/**
 * Trajectory virtual ledger rows — ported from DSH `ui-trajectory`
 * `trajectory-virtual-rows.ts`. Pure projection from turn models to
 * measurable virtual rows: turn boundary separators, step markers and
 * requestOnly merging.
 */

import type { LedgerTurnModel } from "./layout";

/** Row kinds the table renders. */
export type LedgerRowKind =
  | "turn-header"
  | "record"
  | "turn-divider"
  | "between-header";

export interface LedgerRow {
  kind: LedgerRowKind;
  /** Semantic key surviving re-layouts and page prepends. */
  key: string;
  /** 1-based record index for `record` rows. */
  index?: number;
  /** Turn number the row belongs to. */
  turn: number;
  /** Estimated rendered height (px) used by the virtual scroller. */
  height: number;
  /** Record data for `record` rows. */
  cell?: LedgerTurnModel["cells"][number];
  /** Request summary for turn headers. */
  request?: LedgerTurnModel["request"];
  usage?: LedgerTurnModel["usage"];
}

export interface VirtualLedgerProjection {
  rows: LedgerRow[];
  totalHeight: number;
  /** Flat record cells in render order (for search highlighting). */
  cells: LedgerTurnModel["cells"][number][];
}

const HEADER_HEIGHT = 34;
const RECORD_HEIGHT = 30;
const DIVIDER_HEIGHT = 18;

/** Split a turn's record list into steps at assistant boundaries. */
function stepsOf(cells: readonly LedgerTurnModel["cells"][number][]): LedgerTurnModel["cells"][number][][] {
  const steps: LedgerTurnModel["cells"][number][][] = [];
  let current: LedgerTurnModel["cells"][number][] = [];
  for (const cell of cells) {
    if (cell.kind === "assistant" && current.length > 0) {
      steps.push(current);
      current = [];
    }
    current.push(cell);
  }
  if (current.length > 0) steps.push(current);
  return steps;
}

/**
 * Project turns into flat measurable rows. Request-only records merge into
 * the next measurable row rather than emitting zero-height separators.
 */
export function projectLedgerRows(
  turns: readonly LedgerTurnModel[],
  collapsedTurns?: ReadonlySet<number>,
): VirtualLedgerProjection {
  const rows: LedgerRow[] = [];
  const cells: LedgerTurnModel["cells"][number][] = [];

  for (const turn of turns) {
    if (turn.n === -1) {
      if (turn.cells.length === 0) continue;
      rows.push({ kind: "between-header", key: "between-turns", turn: -1, height: HEADER_HEIGHT });
      for (const cell of turn.cells) {
        rows.push({ kind: "record", key: `between\u0000${cell.index}`, index: cell.index, turn: -1, height: RECORD_HEIGHT, cell });
        cells.push(cell);
      }
      continue;
    }
    rows.push({
      kind: "turn-header",
      key: `turn\u0000${turn.n}`,
      turn: turn.n,
      height: HEADER_HEIGHT,
      request: turn.request,
      usage: turn.usage,
    });
    // Collapsed turns keep their header (so the chevron remains visible) but
    // hide the record rows beneath it — never remove the whole turn.
    if (!collapsedTurns?.has(turn.n)) {
      const steps = stepsOf(turn.cells);
      let stepNo = 0;
      for (const step of steps) {
        stepNo += 1;
        for (const cell of step) {
          const key = `t${turn.n}\u0000s${stepNo}\u0000${cell.index}`;
          rows.push({ kind: "record", key, index: cell.index, turn: turn.n, height: RECORD_HEIGHT, cell });
          cells.push(cell);
        }
      }
    }
    rows.push({ kind: "turn-divider", key: `divider\u0000${turn.n}`, turn: turn.n, height: DIVIDER_HEIGHT });
  }

  return { rows, totalHeight: rows.reduce((sum, row) => sum + row.height, 0), cells };
}

/** Visible window of rows given scrollTop and viewport height. */
export function visibleLedgerRows(
  projection: VirtualLedgerProjection,
  scrollTop: number,
  viewportHeight: number,
  buffer = 6,
): { rows: LedgerRow[]; startIndex: number; offsetY: number } {
  const { rows } = projection;
  let offset = 0;
  let startIndex = 0;
  for (let i = 0; i < rows.length; i++) {
    if (offset + rows[i].height > scrollTop) { startIndex = i; break; }
    offset += rows[i].height;
    startIndex = i + 1;
  }
  let end = startIndex;
  let height = 0;
  let i = startIndex;
  const start = Math.max(0, startIndex - buffer);
  let startOffset = offset;
  for (let j = startIndex - 1; j >= start; j--) {
    startOffset -= rows[j].height;
  }
  while (i < rows.length && (height < viewportHeight + buffer * RECORD_HEIGHT || i < startIndex + buffer)) {
    height += rows[i].height;
    end = i + 1;
    i += 1;
  }
  return { rows: rows.slice(start, end), startIndex: start, offsetY: startOffset };
}
