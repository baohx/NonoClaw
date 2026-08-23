import { useState, useEffect, useCallback } from "react";
import { useStore } from "../store";
import { getBrowserAccessToken } from "../security";

interface RawLogEntry {
  file: string;
  kind: string;
  ts_ms: number;
  trace: string;
  size: string;
}

interface RawLogList {
  enabled: boolean;
  dir: string;
  entries: RawLogEntry[];
}

interface RawLogContent {
  file: string;
  content: string;
}

function apiUrl(path: string): string {
  const token = getBrowserAccessToken(window.location.search);
  return token ? `${path}?token=${encodeURIComponent(token)}` : path;
}

const KIND_LABEL: Record<string, string> = {
  request: "REQ",
  resp: "SSE",
  summary: "SUM",
  other: "—",
};

export default function ApiLogDrawer({ onClose }: { onClose: () => void }) {
  const [list, setList] = useState<RawLogList | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [content, setContent] = useState<RawLogContent | null>(null);
  const [contentLoading, setContentLoading] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await fetch(apiUrl("/api/logs/raw"));
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      setList(await res.json());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const openFile = useCallback(
    async (file: string) => {
      setSelected(file);
      setContent(null);
      setContentLoading(true);
      setError(null);
      try {
        const res = await fetch(apiUrl(`/api/logs/raw/${encodeURIComponent(file)}`));
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        setContent(await res.json());
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setContentLoading(false);
      }
    },
    [],
  );

  return (
    <div className="dialog-overlay" onClick={onClose}>
      <div
        className="dialog api-log-drawer"
        style={{ maxWidth: 920, height: "85vh", display: "flex", flexDirection: "column" }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="dialog__eyebrow mint">raw API log</div>
        <div className="api-log-header">
          <div className="dialog__title">Unredacted request / response</div>
          <div className="api-log-header__actions">
            <button className="btn btn--ghost" onClick={refresh} disabled={loading}>
              {loading ? "Refreshing…" : "Refresh"}
            </button>
            <button className="btn btn--ghost" onClick={onClose}>
              Close
            </button>
          </div>
        </div>

        {list && !list.enabled && (
          <div className="api-log-empty">
            <strong>Raw API logging is off.</strong> Restart the server with{" "}
            <code>--log-raw-api</code> to capture full request/response payloads here.
            <div className="api-log-empty__dir">{list.dir}</div>
          </div>
        )}

        {error && <div className="api-log-error">{error}</div>}

        <div className="api-log-body">
          <div className="api-log-list" aria-hidden={!!selected}>
            {loading && !list && <div className="api-log-note">Loading…</div>}
            {list && list.enabled && list.entries.length === 0 && (
              <div className="api-log-note">
                No log entries yet. Send a run — each turn writes a <code>.request.json</code>,{" "}
                <code>.resp.sse</code>, and <code>.summary.json</code> here.
              </div>
            )}
            {list?.entries.map((e) => (
              <button
                key={e.file}
                className={`api-log-entry${selected === e.file ? " api-log-entry--active" : ""}`}
                onClick={() => openFile(e.file)}
                title={e.file}
              >
                <span className={`api-log-kind api-log-kind--${e.kind}`}>{KIND_LABEL[e.kind]}</span>
                <span className="api-log-entry__meta">
                  <span className="api-log-entry__trace">{e.trace}</span>
                  <span className="api-log-entry__time">{new Date(e.ts_ms).toLocaleTimeString()}</span>
                  <span className="api-log-entry__size">{e.size}</span>
                </span>
              </button>
            ))}
          </div>
          <div className="api-log-view">
            {contentLoading && <div className="api-log-note">Loading…</div>}
            {content && (
              <pre className="api-log-pre">{content.content}</pre>
            )}
            {!contentLoading && !content && selected && (
              <div className="api-log-note">Select an entry to view its payload.</div>
            )}
            {!contentLoading && !content && !selected && (
              <div className="api-log-note">Choose a request, response, or summary on the left.</div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
