// Trajectory governance type vocabulary. Mirrors the DSH
// dsh-trajectory-governance plugin's tree + anomaly model, adapted to the data
// NonoClaw's frontend already holds (messages / traceEntries / subagent map).

/** Three temporal anomaly families from the DSH plugin. */
export type AnomalyType = "loop_deadlock" | "invalid_retry" | "goal_drift";

export interface AnomalyResult {
  anomalyId: string;
  type: AnomalyType;
  /** Ids of the tree nodes / tool calls spanning the anomaly range. */
  nodeIds: string[];
  /** 0..1 */
  confidence: number;
  description: string;
  suggestion?: string;
  /** Estimated wasted tokens / time attributed to this range. */
  cost?: CostAttribution;
}

export interface CostAttribution {
  inputTokens: number;
  outputTokens: number;
  /** Wall-clock span of the anomaly range, in ms. */
  elapsedMs: number;
}

export type BranchType = "main" | "subagent";

export interface BranchInfo {
  branchId: string;
  branchType: BranchType;
  parentBranchId: string | null;
  /** Subagent descriptor label, when known. */
  label?: string;
}

export type TrajectoryNodeKind = "turn" | "tool" | "subagent";

export interface TrajectoryNode {
  nodeId: string;
  kind: TrajectoryNodeKind;
  /** Short human summary. */
  label: string;
  timestamp: number;
  /** Tool-call fields (kind === "tool"). */
  toolName?: string;
  toolInput?: unknown;
  toolOk?: boolean;
  toolResult?: string;
  /** Subagent fields (kind === "subagent"). */
  subagentStatus?: string;
  subagentProfile?: string;
  /** Anomaly mounted by the diagnosis pass. */
  anomaly?: AnomalyResult | null;
  children: TrajectoryNode[];
  branch: BranchInfo;
}

export interface TrajectoryTree {
  /** Synthetic root; its children are the top-level branches (normally one). */
  root: TrajectoryNode;
  nodesBuilt: number;
  subagentAttached: number;
  /** Tool calls in chronological order across the whole tree (detection input). */
  linearToolCalls: ToolCallRecord[];
  /** Assistant turns (text + usage) for goal-drift + cost attribution. */
  turns: TurnRecord[];
  /** The user's first prompt — goal-drift baseline. */
  userBaseline: string;
}

/** A flattened tool call — the detection strategies operate over this list. */
export interface ToolCallRecord {
  nodeId: string;
  name: string;
  input?: unknown;
  ok?: boolean;
  result: string;
  timestamp: number;
  /** Turn number, when known (for cost attribution). */
  turn?: number;
}

/** Turn record carrying assistant text + token usage (goal-drift + cost). */
export interface TurnRecord {
  turn: number;
  text: string;
  inputTokens: number;
  outputTokens: number;
  timestamp: number;
}

export interface Alert {
  anomaly: AnomalyResult;
  severity: "warning" | "critical";
  title: string;
  body: string;
}

export interface GovernanceReport {
  tree: TrajectoryTree;
  anomalies: AnomalyResult[];
  alerts: Alert[];
  totals: { turns: number; toolCalls: number; subagents: number };
}

/** Detection thresholds — all adjustable; mirrors the DSH config defaults. */
export interface GovernanceConfig {
  loop: { windowSize: number; callSimMin: number; resultSimMin: number };
  retry: { windowSize: number; errorSimMin: number; actionSimMin: number };
  drift: { sampleEveryNRounds: number; cosineMax: number; consecutiveSamples: number };
  alert: { minConfidence: number };
}

export const DEFAULT_CONFIG: GovernanceConfig = {
  loop: { windowSize: 5, callSimMin: 0.85, resultSimMin: 0.8 },
  retry: { windowSize: 4, errorSimMin: 0.8, actionSimMin: 0.85 },
  drift: { sampleEveryNRounds: 5, cosineMax: 0.25, consecutiveSamples: 2 },
  alert: { minConfidence: 0.6 },
};
