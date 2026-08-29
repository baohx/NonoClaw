// Governance orchestration: build the trajectory tree, run the three anomaly
// strategies, attach cost, and emit alerts. Pure function over frontend state.

import { detectAnomalies } from "./anomalies";
import { attributeCost } from "./cost";
import { buildTrajectoryTree, type TreeInput } from "./tree";
import { DEFAULT_CONFIG, type Alert, type GovernanceConfig, type GovernanceReport, type TurnRecord } from "./types";
import type { TraceEntry } from "../trace";

export interface AnalyzeInput extends TreeInput {
  /** Optional technical event stream — enriches turns with token usage. */
  traceEntries?: TraceEntry[];
}

function mergeUsage(turns: TurnRecord[], traceEntries: TraceEntry[] | undefined): TurnRecord[] {
  if (!traceEntries || traceEntries.length === 0) return turns;
  const byTurn = new Map<number, { input: number; output: number }>();
  for (const entry of traceEntries) {
    if (entry.kind !== "usage_updated") continue;
    const turn = entry.details.turn;
    if (typeof turn !== "number") continue;
    const input = typeof entry.details.turn_in === "number" ? entry.details.turn_in : 0;
    const output = typeof entry.details.turn_out === "number" ? entry.details.turn_out : 0;
    byTurn.set(turn, { input, output });
  }
  if (byTurn.size === 0) return turns;
  return turns.map((turn) => {
    const usage = byTurn.get(turn.turn);
    if (!usage) return turn;
    return { ...turn, inputTokens: usage.input, outputTokens: usage.output };
  });
}

function buildAlerts(anomalies: GovernanceReport["anomalies"], config: GovernanceConfig): Alert[] {
  const severityOf = (type: string): Alert["severity"] =>
    type === "goal_drift" ? "warning" : "critical";
  return anomalies
    .filter((anomaly) => anomaly.confidence >= config.alert.minConfidence)
    .map((anomaly) => {
      const label = { loop_deadlock: "Loop deadlock", invalid_retry: "Invalid retry", goal_drift: "Goal drift" }[anomaly.type] ?? anomaly.type;
      const tokens = anomaly.cost ? anomaly.cost.inputTokens + anomaly.cost.outputTokens : 0;
      return {
        anomaly,
        severity: severityOf(anomaly.type),
        title: `${label} · ${Math.round(anomaly.confidence * 100)}% confidence`,
        body: tokens > 0
          ? `${anomaly.description}. ~${tokens.toLocaleString()} tokens in this range.`
          : anomaly.description,
      };
    });
}

export function analyzeTrajectory(input: AnalyzeInput, config: GovernanceConfig = DEFAULT_CONFIG): GovernanceReport {
  const tree = buildTrajectoryTree(input);
  tree.turns = mergeUsage(tree.turns, input.traceEntries);
  const anomalies = detectAnomalies(tree.linearToolCalls, tree.turns, tree.userBaseline, config);
  for (const anomaly of anomalies) {
    anomaly.cost = attributeCost(anomaly, tree.linearToolCalls, tree.turns);
  }
  return {
    tree,
    anomalies,
    alerts: buildAlerts(anomalies, config),
    totals: {
      turns: tree.turns.length,
      toolCalls: tree.linearToolCalls.length,
      subagents: tree.subagentAttached,
    },
  };
}
