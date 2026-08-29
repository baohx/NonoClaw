/**
 * Incremental full-text index for the trajectory ledger.
 * Ported from DSH `ui-trajectory` `trajectory-search-index.ts`.
 */

import type { LedgerCell } from "./types";

/** Searchable text for one ledger record. */
function searchableText(cell: LedgerCell): string {
  return [cell.text, cell.inputDetail, cell.outputDetail, cell.thinkingDetail, cell.preview]
    .filter((part): part is string => typeof part === "string")
    .join(" \u0000 ")
    .toLocaleLowerCase();
}

/** Incremental index over appended ledger records. */
export class TrajectorySearchIndex {
  private entries = new Map<string, string>();
  private version = 0;

  /** Index cells not seen before; returns the number newly indexed. */
  addCells(cells: readonly LedgerCell[]): number {
    let added = 0;
    for (const cell of cells) {
      if (cell.requestOnly === true) continue;
      const id = `${cell.index}`;
      if (this.entries.has(id)) continue;
      this.entries.set(id, searchableText(cell));
      added += 1;
    }
    if (added > 0) this.version += 1;
    return added;
  }

  get size(): number {
    return this.entries.size;
  }

  /** Match a query of space-separated case-insensitive AND terms. */
  search(query: string): ReadonlySet<string> | null {
    const terms = query.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
    if (terms.length === 0) return null;
    const matches = new Set<string>();
    for (const [id, text] of this.entries) {
      if (terms.every((term) => text.includes(term))) matches.add(id);
    }
    return matches;
  }
}
