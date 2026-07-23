import { useMemo, useState } from "react";
import { useStore } from "../store";
import {
  exportTraceRun,
  groupTraceRuns,
  MAX_RENDERED_TRACE_EVENTS,
  type TraceCategory,
  type TraceEntry,
  type TraceRun,
  type TraceStatus,
} from "../trace";

const CATEGORY_OPTIONS: (TraceCategory | "all")[] = [
  "all", "lifecycle", "context", "model", "provider", "tool", "permission",
  "hook", "subagent", "background", "session", "extension", "config", "usage", "error",
];
const STATUS_OPTIONS: (TraceStatus | "all")[] = ["all", "active", "waiting", "success", "warning", "failure", "cancel", "info"];

function lastMatching(entries: TraceEntry[] | undefined, predicate: (entry: TraceEntry) => boolean): TraceEntry | undefined {
  if (!entries) return undefined;
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    if (predicate(entries[index])) return entries[index];
  }
  return undefined;
}

export default function TechnicalTrace() {
  const entries = useStore((state) => state.traceEntries);
  const clearTrace = useStore((state) => state.clearTrace);
  const [query, setQuery] = useState("");
  const [category, setCategory] = useState<TraceCategory | "all">("all");
  const [status, setStatus] = useState<TraceStatus | "all">("all");
  const [selectedRun, setSelectedRun] = useState("latest");
  const [developer, setDeveloper] = useState(false);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [notice, setNotice] = useState("");

  const allRuns = useMemo(() => groupTraceRuns(entries), [entries]);
  const latest = allRuns[0];
  const runs = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const source = selectedRun === "all" ? allRuns : allRuns.filter((run) => run.runId === (selectedRun === "latest" ? latest?.runId : selectedRun));
    return source.map((run) => ({
      ...run,
      entries: run.entries.filter((entry) =>
        (category === "all" || entry.category === category)
        && (status === "all" || entry.status === status)
        && (!needle || `${entry.summary} ${entry.category} ${entry.status} ${Object.values(entry.details).join(" ")}`.toLowerCase().includes(needle))
      ),
    })).filter((run) => run.entries.length > 0);
  }, [allRuns, category, latest?.runId, query, selectedRun, status]);

  const totalVisible = runs.reduce((count, run) => count + run.entries.length, 0);
  let renderBudget = MAX_RENDERED_TRACE_EVENTS;
  const windowedRuns = runs.map((run) => {
    const take = Math.min(run.entries.length, renderBudget);
    renderBudget -= take;
    return { ...run, entries: run.entries.slice(-take) };
  }).filter((run) => run.entries.length > 0);

  const selected = selectedRun === "all" ? latest : allRuns.find((run) => run.runId === (selectedRun === "latest" ? latest?.runId : selectedRun));
  const toggle = (id: string) => setExpanded((previous) => {
    const next = new Set(previous);
    if (next.has(id)) next.delete(id); else next.add(id);
    return next;
  });
  const payload = () => selected ? JSON.stringify(exportTraceRun(selected), null, 2) : "";
  const announce = (message: string) => { setNotice(message); window.setTimeout(() => setNotice(""), 1800); };
  const copy = async () => {
    if (!selected) return;
    try { await navigator.clipboard.writeText(payload()); announce("redacted run copied"); }
    catch { announce("copy unavailable"); }
  };
  const download = () => {
    if (!selected) return;
    const blob = new Blob([payload()], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `nonoclaw-trace-${selected.runId.slice(0, 12)}.json`;
    anchor.click();
    URL.revokeObjectURL(url);
    announce("redacted trace exported");
  };

  const context = lastMatching(latest?.entries, (entry) => entry.category === "context");
  const model = lastMatching(latest?.entries, (entry) => entry.kind === "model_resolved" || entry.kind === "model_info");
  const usage = lastMatching(latest?.entries, (entry) => entry.category === "usage" || entry.kind === "run_finished" || entry.kind === "done");
  const turnEntry = lastMatching(latest?.entries, (entry) => typeof entry.details.turn === "number");
  const turn = turnEntry?.details.turn;

  return (
    <section className="trace" aria-label="Technical trace">
      <div className="trace__summary">
        <div><span>run</span><b className={`trace-status trace-status--${latest?.status ?? "info"}`}>{latest ? latest.status : "idle"}</b></div>
        <div title={model?.summary}><span>model</span><b>{model?.summary.replace(/^Actual model · |^.* → /, "") ?? "—"}</b></div>
        <div><span>turn</span><b>{turn ?? "—"}</b></div>
        <div title={context?.summary}><span>context</span><b>{context?.details.estimated_tokens?.toLocaleString?.() ?? "—"}</b></div>
        <div><span>tokens</span><b>{usage?.details.total_out?.toLocaleString?.() ?? "—"}</b></div>
      </div>

      <div className="trace__controls">
        <select aria-label="Trace run" value={selectedRun} onChange={(event) => setSelectedRun(event.target.value)}>
          <option value="latest">latest run</option>
          <option value="all">all retained runs</option>
          {allRuns.map((run) => <option key={run.runId} value={run.runId}>{run.runId.slice(0, 10)} · {run.status}</option>)}
        </select>
        <button onClick={copy} disabled={!selected} title="Copy selected redacted run">copy</button>
        <button onClick={download} disabled={!selected} title="Export selected redacted run as JSON">export</button>
        <button onClick={clearTrace} disabled={entries.length === 0} title="Clear retained trace facts">clear</button>
      </div>
      {notice && <div className="trace__notice" role="status">{notice}</div>}

      <div className="trace__filters">
        <input aria-label="Search trace" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="search safe facts…" />
        <select aria-label="Trace category" value={category} onChange={(event) => setCategory(event.target.value as TraceCategory | "all")}>
          {CATEGORY_OPTIONS.map((value) => <option key={value} value={value}>{value === "all" ? "all categories" : value}</option>)}
        </select>
        <select aria-label="Trace status" value={status} onChange={(event) => setStatus(event.target.value as TraceStatus | "all")}>
          {STATUS_OPTIONS.map((value) => <option key={value} value={value}>{value === "all" ? "all states" : value}</option>)}
        </select>
        <label className="trace__developer"><input type="checkbox" checked={developer} onChange={(event) => setDeveloper(event.target.checked)} /> diagnostics</label>
      </div>

      <div className="trace__timeline" aria-live="polite">
        {windowedRuns.map((run) => <TraceRunGroup key={run.runId} run={run} expanded={expanded} developer={developer} onToggle={toggle} />)}
        {windowedRuns.length === 0 && <div className="trace__empty">No matching technical facts yet. Hidden reasoning and raw content are never collected.</div>}
      </div>
      {totalVisible > MAX_RENDERED_TRACE_EVENTS && <div className="trace__window-note">Rendering newest {MAX_RENDERED_TRACE_EVENTS} of {totalVisible} matching facts.</div>}
    </section>
  );
}

function TraceRunGroup({ run, expanded, developer, onToggle }: { run: TraceRun; expanded: Set<string>; developer: boolean; onToggle: (id: string) => void }) {
  return (
    <div className="trace-run">
      <div className="trace-run__head">
        <span className={`trace-pip trace-pip--${run.status}`} />
        <span title={run.runId}>{run.runId.slice(0, 12)}</span>
        <span>{run.entries.length} facts</span>
      </div>
      {run.entries.map((entry) => <TraceRow key={entry.id} entry={entry} open={expanded.has(entry.id)} developer={developer} onToggle={onToggle} />)}
    </div>
  );
}

function TraceRow({ entry, open, developer, onToggle }: { entry: TraceEntry; open: boolean; developer: boolean; onToggle: (id: string) => void }) {
  const hasDetails = Object.keys(entry.details).length > 0 || developer;
  return (
    <button className={`trace-row trace-row--${entry.status}`} onClick={() => hasDetails && onToggle(entry.id)} aria-expanded={hasDetails ? open : undefined}>
      <span className="trace-row__time">{new Date(entry.timestampMs).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}</span>
      <span className={`trace-pip trace-pip--${entry.status}`} />
      <span className="trace-row__body">
        <span className="trace-row__summary">{entry.summary}</span>
        <span className="trace-row__meta">{entry.category} · #{entry.sequence}{hasDetails ? (open ? " · ▾" : " · ▸") : ""}</span>
        {open && <span className="trace-row__details">
          {Object.entries(entry.details).map(([key, value]) => <span key={key}><i>{key}</i><code>{String(value)}</code></span>)}
          {developer && <><span><i>event</i><code>{entry.kind}</code></span><span><i>event id</i><code>{entry.id}</code></span><span><i>session</i><code>{entry.sessionId}</code></span>{entry.parentRunId && <span><i>parent run</i><code>{entry.parentRunId}</code></span>}</>}
        </span>}
      </span>
    </button>
  );
}
