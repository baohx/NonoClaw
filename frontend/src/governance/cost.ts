// Cost attribution: estimate tokens / time wasted inside an anomaly range,
// from the turn usage already accumulated in the frontend store (no external
// billing needed — mirrors the DSH plugin's cost layer using official usage).

import type { AnomalyResult, CostAttribution, ToolCallRecord, TurnRecord } from "./types";

/** Attribute token + wall-clock cost to an anomaly from the turns it spans. */
export function attributeCost(
  anomaly: AnomalyResult,
  toolCalls: ToolCallRecord[],
  turns: TurnRecord[],
): CostAttribution {
  const involved = toolCalls.filter((call) => anomaly.nodeIds.includes(call.nodeId));
  if (involved.length === 0) {
    return { inputTokens: 0, outputTokens: 0, elapsedMs: 0 };
  }

  // Token attribution keys off the turn that owns each involved call — never
  // wall-clock ranges, which are meaningless for live runs (no timestamps) and
  // wrong for subagent tools (synthetic `+ index` timestamps).
  const involvedTurns = new Set(involved.map((call) => call.turn).filter((t): t is number => t !== undefined));
  let inputTokens = 0;
  let outputTokens = 0;
  for (const turn of turns) {
    if (involvedTurns.has(turn.turn)) {
      inputTokens += turn.inputTokens;
      outputTokens += turn.outputTokens;
    }
  }

  // Elapsed time only when at least two real wall-clock timestamps exist; a
  // synthetic sequence number must never be subtracted as elapsed wall-clock.
  const stamps = involved
    .map((call) => call.timestamp)
    .filter((t): t is number => typeof t === "number" && Number.isFinite(t));
  const elapsedMs = stamps.length >= 2
    ? Math.max(0, Math.max(...stamps) - Math.min(...stamps))
    : 0;

  return { inputTokens, outputTokens, elapsedMs };
}
