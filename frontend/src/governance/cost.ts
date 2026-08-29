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
  const startTs = Math.min(...involved.map((call) => call.timestamp));
  const endTs = Math.max(...involved.map((call) => call.timestamp));
  let inputTokens = 0;
  let outputTokens = 0;
  for (const turn of turns) {
    if (turn.timestamp >= startTs && turn.timestamp <= endTs) {
      inputTokens += turn.inputTokens;
      outputTokens += turn.outputTokens;
    }
  }
  return { inputTokens, outputTokens, elapsedMs: Math.max(0, endTs - startTs) };
}
