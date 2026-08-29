// The three temporal anomaly strategies — pure functions over the flattened
// tool-call list / turn list. Ported from the DSH dsh-trajectory-governance
// plugin's diagnose strategies: loop deadlock, invalid retry, goal drift.

import {
  argumentsSimilarity,
  embed,
  errorSimilarity,
  textSimilarity,
  toolCallSimilarity,
  vectorCosine,
} from "./similarity";
import type {
  AnomalyResult,
  GovernanceConfig,
  ToolCallRecord,
  TurnRecord,
} from "./types";

let anomalyCounter = 0;
function nextId(type: string): string {
  anomalyCounter += 1;
  return `${type}-${anomalyCounter}`;
}

function mergeIntervals(intervals: Array<[number, number]>): Array<[number, number]> {
  if (intervals.length === 0) return [];
  const sorted = [...intervals].sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const merged: Array<[number, number]> = [sorted[0]];
  for (let i = 1; i < sorted.length; i += 1) {
    const last = merged[merged.length - 1];
    const [start, end] = sorted[i];
    if (start <= last[1] + 1) {
      last[1] = Math.max(last[1], end);
    } else {
      merged.push([start, end]);
    }
  }
  return merged;
}

/**
 * Strategy A — loop deadlock. A sliding window of tool calls that repeatedly
 * invoke the SAME tool with SIMILAR arguments and get an UNCHANGED result
 * (no progress) is a deadlock. Iterative refinement (result changes) is NOT
 * flagged. confidence = 0.7·min(callSim) + 0.3·min(resultSim).
 */
export function detectLoopDeadlock(
  calls: ToolCallRecord[],
  config: GovernanceConfig,
): AnomalyResult[] {
  const { windowSize, callSimMin, resultSimMin } = config.loop;
  if (calls.length < windowSize) return [];
  const intervals: Array<[number, number]> = [];
  for (let i = 0; i + windowSize <= calls.length; i += 1) {
    let minCallSim = 1;
    let minResultSim = 1;
    let deadlock = true;
    for (let j = 0; j < windowSize - 1; j += 1) {
      const a = calls[i + j];
      const b = calls[i + j + 1];
      const callSim = toolCallSimilarity({ name: a.name, input: a.input }, { name: b.name, input: b.input });
      const resultSim = textSimilarity(a.result, b.result);
      minCallSim = Math.min(minCallSim, callSim);
      minResultSim = Math.min(minResultSim, resultSim);
      if (callSim < callSimMin || resultSim < resultSimMin) {
        deadlock = false;
        break;
      }
    }
    if (deadlock) intervals.push([i, i + windowSize - 1]);
  }
  return mergeIntervals(intervals).map(([start, end]) => {
    const window = calls.slice(start, end + 1);
    const names = [...new Set(window.map((c) => c.name))].join(", ");
    let minCallSim = 1;
    let minResultSim = 1;
    for (let j = 0; j < window.length - 1; j += 1) {
      minCallSim = Math.min(
        minCallSim,
        toolCallSimilarity({ name: window[j].name, input: window[j].input }, { name: window[j + 1].name, input: window[j + 1].input }),
      );
      minResultSim = Math.min(minResultSim, textSimilarity(window[j].result, window[j + 1].result));
    }
    const confidence = Math.round((0.7 * minCallSim + 0.3 * minResultSim) * 100) / 100;
    return {
      anomalyId: nextId("loop"),
      type: "loop_deadlock" as const,
      nodeIds: window.map((c) => c.nodeId),
      confidence,
      description: `${window.length} consecutive ${names} calls with identical results (no progress)`,
      suggestion: "Interrupt the loop and inspect why the tool result never changes — likely a stale cache, retry guard, or missing side effect.",
    };
  });
}

/**
 * Strategy B — invalid retry. Repeatedly re-issuing a SIMILAR action that
 * keeps hitting the SAME error (no new information). Requires the tool results
 * to be marked failed (ok === false). confidence = 0.7·min(actionSim) + 0.2.
 */
export function detectInvalidRetry(
  calls: ToolCallRecord[],
  config: GovernanceConfig,
): AnomalyResult[] {
  const { windowSize, errorSimMin, actionSimMin } = config.retry;
  const failed = calls
    .map((call, index) => ({ call, index }))
    .filter((entry) => entry.call.ok === false);
  if (failed.length < windowSize) return [];
  const intervals: Array<[number, number]> = [];
  for (let i = 0; i + windowSize <= failed.length; i += 1) {
    const window = failed.slice(i, i + windowSize);
    let minActionSim = 1;
    let minErrorSim = 1;
    let retry = true;
    for (let j = 0; j < windowSize - 1; j += 1) {
      const a = window[j].call;
      const b = window[j + 1].call;
      const actionSim = toolCallSimilarity({ name: a.name, input: a.input }, { name: b.name, input: b.input });
      const errSim = errorSimilarity(a.result, b.result);
      minActionSim = Math.min(minActionSim, actionSim);
      minErrorSim = Math.min(minErrorSim, errSim);
      if (actionSim < actionSimMin || errSim < errorSimMin) {
        retry = false;
        break;
      }
    }
    if (retry) intervals.push([window[0].index, window[windowSize - 1].index]);
  }
  return mergeIntervals(intervals).map(([start, end]) => {
    // Count only the FAILED calls inside the merged range — the span may
    // include unrelated successful calls between retries.
    const window = calls.slice(start, end + 1).filter((c) => c.ok === false);
    const name = window[0]?.name ?? "tool";
    return {
      anomalyId: nextId("retry"),
      type: "invalid_retry" as const,
      nodeIds: window.map((c) => c.nodeId),
      confidence: 0.9,
      description: `${window.length} retries of ${name} hitting the same error`,
      suggestion: "The retry is deterministic — stop re-running, fix the underlying cause first.",
    };
  });
}

/**
 * Strategy C — goal drift. Compare sampled assistant intents against the
 * user's FIRST prompt. Several consecutive low-similarity samples signal the
 * agent has wandered off-task. Lexical embedder only (no external service).
 */
export function detectGoalDrift(
  turns: TurnRecord[],
  userBaseline: string,
  config: GovernanceConfig,
): AnomalyResult[] {
  const { sampleEveryNRounds, cosineMax, consecutiveSamples } = config.drift;
  if (!userBaseline || turns.length < 2) return [];
  const baselineVec = embed(userBaseline);
  // Sample every turn on short sessions; only down-sample long ones to bound
  // embed work (mirrors the DSH `sampleEveryNRounds` intent).
  const step = turns.length > 60 ? sampleEveryNRounds : 1;
  const samples: Array<{ turn: number; cosine: number }> = [];
  for (let i = 0; i < turns.length; i += step) {
    const turn = turns[i];
    if (!turn.text.trim()) continue;
    const cosine = vectorCosine(baselineVec, embed(turn.text));
    samples.push({ turn: turn.turn, cosine });
  }
  const anomalies: AnomalyResult[] = [];
  let runStart = -1;
  let runMin = 1;
  for (let i = 0; i < samples.length; i += 1) {
    if (samples[i].cosine <= cosineMax) {
      if (runStart === -1) {
        runStart = i;
        runMin = samples[i].cosine;
      } else {
        runMin = Math.min(runMin, samples[i].cosine);
      }
    } else {
      runStart = -1;
      runMin = 1;
    }
    if (runStart !== -1 && i - runStart + 1 >= consecutiveSamples) {
      const confidence = Math.round(Math.min(1, 1 - runMin + 0.15) * 100) / 100;
      anomalies.push({
        anomalyId: nextId("drift"),
        type: "goal_drift" as const,
        nodeIds: [`turn:${samples[runStart].turn}`],
        confidence,
        description: `Assistant intent diverged from the original request around turn ${samples[runStart].turn} (cosine ${runMin.toFixed(2)})`,
        suggestion: "Review the last few assistant messages — it may have latched onto a tangent.",
      });
      runStart = -1;
      runMin = 1;
    }
  }
  return anomalies;
}

/** Run all three strategies and return the combined (deduplicated) list. */
export function detectAnomalies(
  calls: ToolCallRecord[],
  turns: TurnRecord[],
  userBaseline: string,
  config: GovernanceConfig,
): AnomalyResult[] {
  const loops = detectLoopDeadlock(calls, config);
  const retries = detectInvalidRetry(calls, config);
  const drifts = detectGoalDrift(turns, userBaseline, config);
  return [...loops, ...retries, ...drifts];
}

// Re-exported for tests that want to verify the primitive independently.
export { argumentsSimilarity };
