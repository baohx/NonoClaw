/**
 * Trajectory ledger record contract — ported from DSH `ui-trajectory`
 * `trajectory-record.ts`. One record = one selectable row in the turn-aware
 * event ledger (user / assistant / tool / system / compacted).
 */

/** Source NonoClaw side this record was projected from. */
export type LedgerSourceKind = "user" | "assistant" | "thinking" | "tool" | "system" | "compacted";

/** Timing + token facts for assistant records (TTFT / decode throughput). */
export interface AssistantMetricDetail {
  timingRecorded: boolean;
  /** Epoch ms the step started (model_request_started), if recorded. */
  stepStartTime: number | null;
  /** Epoch ms of the first streamed token after step start. */
  firstTokenTime: number | null;
  /** Epoch ms when the step finished (assistant_done / tool batch end). */
  completedTime: number | null;
  usageProvided: boolean;
  outputTokens: number | null;
}

/** Data for one trajectory ledger record (one virtualizable row). */
export interface LedgerCell {
  /** 1-based record index shown as `#N`; layout assigns at placement. */
  index: number;
  /** Projection-stable identity surviving prepend of older records. */
  recordId?: string;
  kind: LedgerSourceKind;
  /** Single-line summary; CSS ellipsis on overflow. */
  text: string;
  /** Longer preview for the details panel. */
  preview?: string;
  /** Whether this user record opens a new model turn. */
  opensTurn?: boolean;
  /** Source trace sequence for cross-record navigation. */
  sourceSeq?: number;
  /** Separator-only anchor for an auxiliary request with no visible record. */
  requestOnly?: boolean;
  /** Full input content for the details panel. */
  inputDetail?: string;
  /** Full output content for the details panel. */
  outputDetail?: string;
  /** Reasoning content for the details panel. */
  thinkingDetail?: string;
  /** Tool call id linking assistant blocks to tool records. */
  callId?: string;
  /** Owning subagent run id (records inside a subagent branch). */
  subagentRunId?: string;
  /** Tool result failure state. */
  isError?: boolean;
  /** Own duration in seconds, or null when unknown / still running. */
  timeSeconds: number | null;
  /** Unix epoch ms when this operation actually started, when known. */
  startedAt?: number | null;
  /** Unix epoch ms when this operation actually ended, when known. */
  endedAt?: number | null;
  /** Optional projection-only interval used by the compressed Time view.
   * Actual mode always uses startedAt/endedAt, preserving real-clock facts. */
  timelineStartedAt?: number | null;
  timelineEndedAt?: number | null;
  /** Message-only prompt token count. */
  input?: number;
  /** Input tokens served from provider cache. */
  cacheRead?: number;
  /** Input tokens written into provider cache. */
  cacheWrite?: number;
  /** Message-only completion token count. */
  output?: number;
  /** Message-only reasoning token count. */
  think?: number;
  /** Assistant TTFT / decode throughput facts, when timing was recorded. */
  assistantMetrics?: AssistantMetricDetail;
}

/**
 * Resolve the identity that survives prepending older projected records.
 * Ported verbatim from DSH `trajectoryRecordId`.
 */
export function ledgerRecordId(cell: LedgerCell): string {
  if (cell.recordId !== undefined) return cell.recordId;
  if (cell.callId !== undefined) return `${cell.kind}\u0000call\u0000${cell.callId}`;
  if (cell.sourceSeq !== undefined) return `${cell.kind}\u0000seq\u0000${cell.sourceSeq}`;
  return `${cell.kind}\u0000index\u0000${cell.index}`;
}

/** Format a duration in milliseconds with thousands separators. */
export function formatDurationMillis(milliseconds: number | null): string {
  if (milliseconds === null || !Number.isFinite(milliseconds)) return "—";
  const integer = String(Math.round(milliseconds));
  return `${integer.replace(/\B(?=(\d{3})+(?!\d))/g, ",")} ms`;
}

/** Format a duration given in seconds as a millisecond label. */
export function formatElapsedSeconds(seconds: number | null): string {
  return formatDurationMillis(seconds === null ? null : seconds * 1000);
}

/** Compact token count label (1.2k / 340 / —). */
export function formatTokenCount(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return "—";
  if (Math.abs(value) >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (Math.abs(value) >= 10_000) return `${Math.round(value / 1000)}k`;
  if (Math.abs(value) >= 1_000) return `${(value / 1000).toFixed(1)}k`;
  return String(value);
}
