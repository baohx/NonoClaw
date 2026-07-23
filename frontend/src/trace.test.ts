import { appendTraceEntry, eventToSafeFact, groupTraceRuns, MAX_TRACE_EVENTS, type TraceEntry } from "./trace.ts";

function check(condition: boolean, message: string): void {
  if (!condition) throw new Error(`trace invariant failed: ${message}`);
}

/** Deterministic pure-state checks; this file is type-checked by the production build. */
export function checkTraceStateInvariants(): void {
  const hidden = eventToSafeFact({ kind: "text_delta", text: "hidden output" });
  check(hidden === null, "stream text must not enter the trace");

  const tool = eventToSafeFact({ kind: "tool_use_start", id: "tool-1", name: "Read", input: { password: "secret" } });
  check(tool !== null && !("input" in (tool.details ?? {})), "tool input must be omitted");

  let entries: TraceEntry[] = [];
  for (let sequence = MAX_TRACE_EVENTS + 10; sequence >= 1; sequence -= 1) {
    entries = appendTraceEntry(entries, {
      id: `event-${sequence}`, runId: sequence % 2 ? "run-b" : "run-a", sessionId: "session",
      sequence, timestampMs: sequence, category: "lifecycle", status: "active", kind: "run_started",
      summary: `event ${sequence}`, details: {},
    });
  }
  check(entries.length === MAX_TRACE_EVENTS, "retention must be bounded");
  const runs = groupTraceRuns(entries);
  check(runs[0].entries.every((entry, index, all) => index === 0 || all[index - 1].sequence < entry.sequence), "run entries must be sequence ordered");
  const before = entries.length;
  entries = appendTraceEntry(entries, entries[0]);
  check(entries.length === before, "event ids must be deduplicated");
}
