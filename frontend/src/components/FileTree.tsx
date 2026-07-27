import { useMemo, useState, useCallback, useEffect, useRef } from "react";
import { createPortal } from "react-dom";
import type { FileEntry } from "../types";
import { getMobileAccessToken } from "../security";

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
  const [menu, setMenu] = useState<{ entry: FileEntry; x: number; y: number } | null>(null);
  const menuOrigin = useRef<HTMLButtonElement | null>(null);

  const closeMenu = useCallback(() => {
    setMenu(null);
    requestAnimationFrame(() => menuOrigin.current?.focus());
  }, []);

  useEffect(() => {
    if (!menu) return;
    const close = () => closeMenu();
    const keydown = (event: KeyboardEvent) => {
      if (event.key === "Escape") closeMenu();
    };
    window.addEventListener("click", close);
    window.addEventListener("keydown", keydown);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("keydown", keydown);
    };
  }, [menu, closeMenu]);

  const openMenu = useCallback((entry: FileEntry, element: HTMLButtonElement, x: number, y: number) => {
    menuOrigin.current = element;
    setMenu({ entry, x, y });
  }, []);

  const download = useCallback((entry: FileEntry) => {
    if (entry.is_dir) return;
    const token = getMobileAccessToken();
    if (!token) {
      window.alert("Download requires an active access token.");
      closeMenu();
      return;
    }
    const form = document.createElement("form");
    form.method = "POST";
    form.action = "/api/download";
    form.target = "nonoclaw-download-target";
    form.hidden = true;
    for (const [name, value] of [["token", token], ["path", entry.path]]) {
      const input = document.createElement("input");
      input.type = "hidden";
      input.name = name;
      input.value = value;
      form.appendChild(input);
    }
    document.body.appendChild(form);
    form.submit();
    form.remove();
    closeMenu();
  }, [closeMenu]);

  const contextKey = useCallback((event: React.KeyboardEvent<HTMLButtonElement>, entry: FileEntry) => {
    if ((event.shiftKey && event.key === "F10") || event.key === "ContextMenu") {
      event.preventDefault();
      const rect = event.currentTarget.getBoundingClientRect();
      openMenu(entry, event.currentTarget, rect.left + 24, rect.bottom);
    }
  }, [openMenu]);

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
                onContextMenu={(event) => {
                  event.preventDefault();
                  openMenu(e, event.currentTarget, event.clientX, event.clientY);
                }}
                onKeyDown={(event) => contextKey(event, e)}
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
              onContextMenu={(event) => {
                event.preventDefault();
                openMenu(e, event.currentTarget, event.clientX, event.clientY);
              }}
              onKeyDown={(event) => contextKey(event, e)}
              title={`${e.path} — click to open · shift+click for VS Code`}
            >
              <span className="tree-row__glyph">{fileGlyph(e.name)}</span>
              <span className="tree-row__name">{e.name}</span>
            </button>
          );
        })}
      </div>
      <iframe name="nonoclaw-download-target" className="download-target" title="Download target" />
      {menu && createPortal(
        <div
          className="filetree-menu"
          role="menu"
          style={{
            left: Math.max(4, Math.min(menu.x + 4, window.innerWidth - 140)),
            top: Math.max(4, Math.min(menu.y + 4, window.innerHeight - (menu.entry.is_dir ? 76 : 44))),
          }}
          onClick={(event) => event.stopPropagation()}
        >
          <button
            role="menuitem"
            autoFocus
            disabled={menu.entry.is_dir}
            title={menu.entry.is_dir ? "Directory downloads are not supported." : `Download ${menu.entry.name}`}
            onClick={() => download(menu.entry)}
          >
            Download
          </button>
          {menu.entry.is_dir && (
            <div className="filetree-menu__hint">Directory downloads are not supported.</div>
          )}
        </div>,
        document.body
      )}
    </div>
  );
}
