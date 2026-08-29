import { useMemo, useState } from "react";
import { useStore } from "../store";
import { analyzeTrajectory } from "../governance";
import type { AnomalyResult, TrajectoryNode } from "../governance/types";
import TrajectoryLedger from "./TrajectoryLedger";

type Tab = "tree" | "anomalies" | "alerts" | "ledger";

const ANOMALY_LABEL: Record<AnomalyResult["type"], string> = {
  loop_deadlock: "Loop deadlock",
  invalid_retry: "Invalid retry",
  goal_drift: "Goal drift",
};

const ANOMALY_ICON: Record<AnomalyResult["type"], string> = {
  loop_deadlock: "⟳",
  invalid_retry: "↻",
  goal_drift: "↯",
};

function fmtTokens(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}

function fmtMs(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${(ms / 60_000).toFixed(1)}m`;
}

function statusClass(ok: boolean | undefined): string {
  if (ok === false) return "gov-node__pip gov-node__pip--fail";
  if (ok === true) return "gov-node__pip gov-node__pip--ok";
  return "gov-node__pip";
}

/** Recursively render a trajectory tree node. */
function TreeNode({ node, anomalyIds, depth, expanded, toggle }: {
  node: TrajectoryNode;
  anomalyIds: Set<string>;
  depth: number;
  expanded: Set<string>;
  toggle: (id: string) => void;
}) {
  const hasChildren = node.children.length > 0;
  const open = expanded.has(node.nodeId);
  const flagged = anomalyIds.has(node.nodeId);
  const kindClass = node.kind === "subagent" ? "gov-node--subagent" : node.kind === "tool" ? "gov-node--tool" : "gov-node--turn";
  const indent = { paddingLeft: `${8 + depth * 14}px` };
  return (
    <div className="gov-node" style={indent}>
      <button
        className={`gov-node__row ${kindClass}${flagged ? " gov-node--flagged" : ""}`}
        onClick={() => hasChildren && toggle(node.nodeId)}
        aria-expanded={hasChildren ? open : undefined}
        title={flagged ? "part of a detected anomaly" : undefined}
      >
        <span className="gov-node__chevron">{hasChildren ? (open ? "▾" : "▸") : "·"}</span>
        <span className={statusClass(node.kind === "tool" ? node.toolOk : undefined)} />
        <span className="gov-node__label">{node.label}</span>
        {node.toolName && <span className="gov-node__meta">{node.toolName}</span>}
        {node.kind === "subagent" && node.subagentStatus && (
          <span className="gov-node__status">{node.subagentStatus}</span>
        )}
        {flagged && <span className="gov-node__flag">⚠</span>}
      </button>
      {hasChildren && open && (
        <div className="gov-node__children">
          {node.children.map((child) => (
            <TreeNode key={child.nodeId} node={child} anomalyIds={anomalyIds} depth={depth + 1} expanded={expanded} toggle={toggle} />
          ))}
        </div>
      )}
    </div>
  );
}

export default function TrajectoryGovernance({ onClose }: { onClose: () => void }) {
  const messages = useStore((s) => s.messages);
  const traceEntries = useStore((s) => s.traceEntries);
  const subagentRunsById = useStore((s) => s.subagentRunsById);
  const childIdsByParentToolId = useStore((s) => s.childIdsByParentToolId);

  const report = useMemo(
    () => analyzeTrajectory({ messages, traceEntries, subagentRunsById, childIdsByParentToolId }),
    [messages, traceEntries, subagentRunsById, childIdsByParentToolId],
  );

  const [tab, setTab] = useState<Tab>("tree");
  const [expanded, setExpanded] = useState<Set<string>>(() => {
    const ids = new Set<string>();
    const walk = (node: TrajectoryNode) => {
      ids.add(node.nodeId);
      node.children.forEach(walk);
    };
    walk(report.tree.root);
    return ids;
  });

  const toggle = (id: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const anomalyIds = useMemo(() => {
    const ids = new Set<string>();
    for (const anomaly of report.anomalies) {
      for (const id of anomaly.nodeIds) ids.add(id);
    }
    return ids;
  }, [report.anomalies]);

  const { totals } = report;
  const wastedTokens = report.anomalies.reduce((sum, a) => sum + (a.cost?.inputTokens ?? 0) + (a.cost?.outputTokens ?? 0), 0);

  return (
    <div className="dialog-overlay" onClick={onClose}>
      <div className="dialog gov-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="dialog__eyebrow mint">trajectory governance · DSH-style</div>
        <div className="gov-header">
          <div>
            <div className="dialog__title">Trajectory Governance</div>
            <div className="gov-header__sub">
              multi-branch tree · loop / retry / drift detection · cost attribution
            </div>
          </div>
          <button className="btn btn--ghost" onClick={onClose}>Close</button>
        </div>

        <div className="gov-stats">
          <Stat label="turns" value={totals.turns} />
          <Stat label="tool calls" value={totals.toolCalls} />
          <Stat label="subagents" value={totals.subagents} />
          <Stat label="anomalies" value={report.anomalies.length} accent={report.anomalies.length > 0} />
          <Stat label="wasted tokens" value={fmtTokens(wastedTokens)} accent={wastedTokens > 0} />
        </div>

        <div className="gov-tabs" role="tablist">
          <TabButton active={tab === "tree"} onClick={() => setTab("tree")} label="Tree" count={totals.toolCalls} />
          <TabButton active={tab === "anomalies"} onClick={() => setTab("anomalies")} label="Anomalies" count={report.anomalies.length} />
          <TabButton active={tab === "alerts"} onClick={() => setTab("alerts")} label="Alerts" count={report.alerts.length} />
          <TabButton active={tab === "ledger"} onClick={() => setTab("ledger")} label="Ledger" count={totals.toolCalls + totals.turns} />
        </div>

        <div className="gov-body">
          {tab === "tree" && (
            <div className="gov-tree">
              {report.tree.root.children.length === 0 ? (
                <div className="gov-empty">No trajectory yet — run an agent turn and the tree will build here.</div>
              ) : (
                report.tree.root.children.map((node) => (
                  <TreeNode key={node.nodeId} node={node} anomalyIds={anomalyIds} depth={0} expanded={expanded} toggle={toggle} />
                ))
              )}
            </div>
          )}

          {tab === "anomalies" && (
            <div className="gov-list">
              {report.anomalies.length === 0 ? (
                <div className="gov-empty">No anomalies detected — trajectory looks healthy.</div>
              ) : (
                report.anomalies.map((anomaly) => <AnomalyCard key={anomaly.anomalyId} anomaly={anomaly} />)
              )}
            </div>
          )}

          {tab === "alerts" && (
            <div className="gov-list">
              {report.alerts.length === 0 ? (
                <div className="gov-empty">No alerts above the confidence threshold.</div>
              ) : (
                report.alerts.map((alert) => (
                  <div key={alert.anomaly.anomalyId} className={`gov-alert gov-alert--${alert.severity}`}>
                    <div className="gov-alert__title">{alert.title}</div>
                    <div className="gov-alert__body">{alert.body}</div>
                    {alert.anomaly.suggestion && <div className="gov-alert__suggestion">💡 {alert.anomaly.suggestion}</div>}
                  </div>
                ))
              )}
            </div>
          )}
          {tab === "ledger" && <TrajectoryLedger />}
        </div>
      </div>
    </div>
  );
}

function Stat({ label, value, accent }: { label: string; value: number | string; accent?: boolean }) {
  return (
    <div className="gov-stat">
      <span className={`gov-stat__value${accent ? " gov-stat__value--accent" : ""}`}>{value}</span>
      <span className="gov-stat__label">{label}</span>
    </div>
  );
}

function TabButton({ active, onClick, label, count }: { active: boolean; onClick: () => void; label: string; count: number }) {
  return (
    <button className={`gov-tab${active ? " gov-tab--active" : ""}`} role="tab" aria-selected={active} onClick={onClick}>
      {label}<span className="gov-tab__count">{count}</span>
    </button>
  );
}

function AnomalyCard({ anomaly }: { anomaly: AnomalyResult }) {
  const confidence = Math.round(anomaly.confidence * 100);
  const tokens = (anomaly.cost?.inputTokens ?? 0) + (anomaly.cost?.outputTokens ?? 0);
  return (
    <div className="gov-anomaly">
      <div className="gov-anomaly__head">
        <span className="gov-anomaly__icon">{ANOMALY_ICON[anomaly.type]}</span>
        <span className="gov-anomaly__type">{ANOMALY_LABEL[anomaly.type]}</span>
        <span className="gov-anomaly__confidence" title="confidence">{confidence}%</span>
        <span className="gov-anomaly__spacer" />
        {tokens > 0 && <span className="gov-anomaly__cost">~{fmtTokens(tokens)} tok</span>}
        {anomaly.cost && anomaly.cost.elapsedMs > 0 && <span className="gov-anomaly__cost">{fmtMs(anomaly.cost.elapsedMs)}</span>}
      </div>
      <div className="gov-anomaly__desc">{anomaly.description}</div>
      {anomaly.suggestion && <div className="gov-anomaly__suggestion">💡 {anomaly.suggestion}</div>}
      <div className="gov-anomaly__nodes">{anomaly.nodeIds.length} node(s) affected</div>
    </div>
  );
}
