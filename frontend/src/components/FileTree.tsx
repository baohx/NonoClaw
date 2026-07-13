import { useMemo, useState, useCallback } from "react";
import type { FileEntry } from "../types";

interface Props {
  root: string;
  entries: FileEntry[];
  onOpen: (path: string, forceCode: boolean) => void;
  onRefresh: () => void;
}

/** Basename of the cwd — shown as the tree root label. */
function rootLabel(root: string): string {
  const clean = root.replace(/\/+$/, "");
  const slash = clean.lastIndexOf("/");
  return slash >= 0 ? clean.slice(slash + 1) : clean || "project";
}

/** A tiny monochrome glyph per file extension, for at-a-glance scanning. */
function fileGlyph(name: string): string {
  const ext = name.includes(".") ? name.split(".").pop()!.toLowerCase() : "";
  switch (ext) {
    case "rs":
      return "󱁋"; // gear-ish; falls back to a box if font lacks it
    case "ts":
    case "tsx":
      return "TS";
    case "js":
    case "jsx":
      return "JS";
    case "json":
      return "{}";
    case "md":
      return "M↓";
    case "toml":
      return "tl";
    case "html":
      return "<>";
    case "css":
      return "#";
    case "lock":
      return "🔒";
    default:
      return "·";
  }
}

export default function FileTree({ root, entries, onOpen, onRefresh }: Props) {
  // ALL directories collapsed by default — user expands what they need.
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set<string>());

  // When a fresh tree arrives, don't auto-expand anything.
  const seedKey = entries.map((e) => e.path).join("\n");
  const [lastSeed, setLastSeed] = useState(seedKey);
  if (seedKey !== lastSeed) {
    setLastSeed(seedKey);
    // Drop stale expanded paths that no longer exist.
    const known = new Set(entries.filter((e) => e.is_dir).map((e) => e.path));
    setExpanded((prev) => {
      const next = new Set(prev);
      for (const p of [...next]) if (!known.has(p)) next.delete(p);
      return next;
    });
  }

  const dirPaths = useMemo(
    () => new Set(entries.filter((e) => e.is_dir).map((e) => e.path)),
    [entries]
  );

  const ancestorsOf = useCallback(
    (path: string): string[] => {
      const parts = path.split("/");
      const out: string[] = [];
      for (let i = 1; i < parts.length; i++) {
        const a = parts.slice(0, i).join("/");
        if (dirPaths.has(a)) out.push(a);
      }
      return out;
    },
    [dirPaths]
  );

  const toggle = useCallback((path: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  const collapseAll = useCallback(() => setExpanded(new Set()), []);

  const visible = entries.filter((e) =>
    ancestorsOf(e.path).every((a) => expanded.has(a))
  );

  return (
    <div className="filetree">
      <div className="filetree__head">
        <span className="filetree__root" title={root}>
          <span className="filetree__rootmark">◆</span>
          {rootLabel(root)}
        </span>
        <span className="filetree__actions">
          <button className="iconbtn" title="Collapse all" onClick={collapseAll}>
            ⇲
          </button>
          <button className="iconbtn" title="Refresh" onClick={onRefresh}>
            ↻
          </button>
        </span>
      </div>

      <div className="filetree__list">
        {visible.length === 0 && (
          <div className="filetree__empty">No files.</div>
        )}
        {visible.map((e) => {
          const open = expanded.has(e.path);
          if (e.is_dir) {
            return (
              <button
                key={e.path}
                className="tree-row tree-row--dir"
                style={{ paddingLeft: 10 + e.depth * 13 }}
                onClick={() => toggle(e.path)}
                title={e.path}
              >
                <span className="tree-row__caret">{open ? "▾" : "▸"}</span>
                <span className="tree-row__glyph tree-row__glyph--dir">▣</span>
                <span className="tree-row__name">{e.name}</span>
              </button>
            );
          }
          return (
            <button
              key={e.path}
              className="tree-row tree-row--file"
              style={{ paddingLeft: 10 + e.depth * 13 + 13 }}
              onClick={(ev) => onOpen(e.path, ev.shiftKey)}
              title={`${e.path} — click to open · shift+click for VS Code`}
            >
              <span className="tree-row__glyph">{fileGlyph(e.name)}</span>
              <span className="tree-row__name">{e.name}</span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
