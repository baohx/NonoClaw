import { useMemo, useState } from "react";
import type { CommitInfo, GitInfo } from "../types";

interface Props {
  git: GitInfo | null;
  onRefresh: () => void;
  onShow: (sha: string) => void;
}

export default function GitPane({ git, onRefresh, onShow }: Props) {
  const [q, setQ] = useState("");
  const filtered = useMemo(() => {
    if (!git) return [];
    const needle = q.trim().toLowerCase();
    const all = git.recent_commits;
    if (!needle) return all;
    return all.filter(
      (c) =>
        c.sha.toLowerCase().includes(needle) ||
        c.author.toLowerCase().includes(needle) ||
        c.subject.toLowerCase().includes(needle)
    );
  }, [git, q]);

  return (
    <div className="gitpane">
      <div className="gitpane__head">
        <span className="gitpane__title">git</span>
        <button className="iconbtn" onClick={onRefresh} title="Refresh status">
          ↻
        </button>
      </div>
      <div className="gitpane__body">
        {!git && <div className="git-none">not a git repo</div>}

        {git && (
          <>
            <div className="git-branch">
              ⎇ {git.branch ?? "HEAD"}
              {git.user ? <span style={{ color: "var(--faint)" }}> · {git.user}</span> : null}
            </div>

            {(git.ahead > 0 || git.behind > 0) && (
              <div style={{ display: "flex", gap: 12, marginTop: 7, fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--muted)" }}>
                {git.ahead > 0 && <span>↑ {git.ahead} ahead</span>}
                {git.behind > 0 && <span>↓ {git.behind} behind</span>}
              </div>
            )}

            {git.is_empty && <div className="git-empty">⚠ no commits yet</div>}

            <GitDots git={git} />

            {!git.is_empty && (
              <>
                <input
                  className="git-filter"
                  value={q}
                  onChange={(e) => setQ(e.target.value)}
                  placeholder={`filter ${git.recent_commits.length} commits (sha · author · msg)`}
                />
                <div className="git-commits">
                  {filtered.length === 0 ? (
                    <div className="git-none">no matches</div>
                  ) : (
                    filtered
                      .slice(0, q.trim() ? 60 : 12)
                      .map((c) => (
                        <CommitLine key={c.sha} c={c} onShow={onShow} />
                      ))
                  )}
                </div>
                <div className="git-none" style={{ marginTop: 4 }}>click a commit to view its patch</div>
              </>
            )}
          </>
        )}
      </div>
    </div>
  );
}

function GitDots({ git }: { git: GitInfo }) {
  const rows: { cls: string; label: string; n: number }[] = [
    { cls: "staged", label: "staged", n: git.staged },
    { cls: "modified", label: "modified", n: git.modified },
    { cls: "untracked", label: "untracked", n: git.untracked },
    { cls: "conflict", label: "conflict", n: git.conflicts },
  ];
  const shown = rows.filter((r) => r.n > 0);
  if (shown.length === 0) {
    return <div className="git-none" style={{ marginTop: 8 }}>clean working tree</div>;
  }
  return (
    <div className="git-dots">
      {shown.map((r) => (
        <span key={r.cls} className="git-dot">
          <span className={`pip ${r.cls}`} />
          <b>{r.n}</b> {r.label}
        </span>
      ))}
    </div>
  );
}

function CommitLine({ c, onShow }: { c: CommitInfo; onShow: (sha: string) => void }) {
  return (
    <button className="git-commit--btn" onClick={() => onShow(c.sha)} title={`git show ${c.sha}`}>
      <span className="sha">{c.sha}</span> <span className="git-commit__subj">{c.subject}</span>
      <span className="git-commit__meta"> · {c.author} · {c.date}</span>
    </button>
  );
}
