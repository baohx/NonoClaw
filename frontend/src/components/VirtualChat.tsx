/**
 * Windowed chat renderer — renders only the messages inside (or near) the
 * `.chat-scroll` viewport, keeping long sessions cheap to paint. Ported in
 * spirit from DSH's `@tanstack/react-virtual` usage in `ui-trajectory`,
 * without the dependency: a scroll listener + per-row ResizeObserver
 * measuring cached heights (chat rows vary from one line to huge tool
 * cards, so estimates alone would jitter).
 *
 * Invariants:
 * - Rows keep their DOM `id={msg-${id}}` anchors — deep links from the
 *   session rail must still scrollIntoView after virtualization.
 * - The bottom spacer always renders, so the App's "stick to bottom"
 *   scrollTop = scrollHeight effect is unchanged.
 * - A row that is currently measuring (first paint) renders immediately
 *   rather than being clipped by a stale cache entry.
 */

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";

/** Fallback height for rows never measured yet. */
const UNMEASURED_HEIGHT = 160;
/** Extra rows kept alive above/below the viewport. */
const OVERSCAN = 4;

export interface VirtualChatRow {
  key: string;
  node: ReactNode;
}

interface Props {
  rows: VirtualChatRow[];
}

export default function VirtualChat({ rows }: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const scrollerRef = useRef<HTMLElement | null>(null);
  const heightsRef = useRef(new Map<string, number>());
  const rowRefs = useRef(new Map<string, HTMLDivElement | null>());
  const [scrollTop, setScrollTop] = useState(0);
  const [viewport, setViewport] = useState(0);
  const [dirty, setDirty] = useState(0);

  // Heights feed the spacer math; a bump re-renders when measurement lands.
  const bump = () => setDirty((n) => n + 1);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const scroller = el.closest<HTMLElement>(".chat-scroll");
    if (!scroller) return;
    scrollerRef.current = scroller;
    const onScroll = () => setScrollTop(scroller.scrollTop);
    const onResize = () => setViewport(scroller.clientHeight);
    scroller.addEventListener("scroll", onScroll, { passive: true });
    const ro = new ResizeObserver(onResize);
    ro.observe(scroller);
    onResize();
    onScroll();
    return () => {
      scroller.removeEventListener("scroll", onScroll);
      ro.disconnect();
    };
  }, []);

  // Measure rendered rows; observe size changes (streaming text grows rows).
  useEffect(() => {
    const observer = new ResizeObserver((entries) => {
      let changed = false;
      for (const entry of entries) {
        const key = (entry.target as HTMLElement).dataset.vrow;
        if (!key) continue;
        const h = Math.round(entry.contentRect.height);
        if (heightsRef.current.get(key) !== h) {
          heightsRef.current.set(key, h);
          changed = true;
        }
      }
      if (changed) bump();
    });
    for (const [, node] of rowRefs.current) if (node) observer.observe(node);
    return () => observer.disconnect();
  }, [rows, scrollTop, dirty]);

  const offsets = useMemo(() => {
    const heights = heightsRef.current;
    const tops: number[] = new Array(rows.length + 1);
    let acc = 0;
    for (let i = 0; i < rows.length; i++) {
      tops[i] = acc;
      acc += heights.get(rows[i].key) ?? UNMEASURED_HEIGHT;
    }
    tops[rows.length] = acc;
    return tops;
  }, [rows, dirty]);

  const total = offsets[rows.length];
  const top = Math.max(0, scrollTop - UNMEASURED_HEIGHT * OVERSCAN);
  const bottom = scrollTop + viewport + UNMEASURED_HEIGHT * OVERSCAN;

  // Binary search: first row whose bottom edge passes `top`.
  let start = 0;
  let end = rows.length - 1;
  let first = rows.length;
  while (start <= end) {
    const mid = (start + end) >> 1;
    if (offsets[mid + 1] >= top) {
      first = mid;
      end = mid - 1;
    } else start = mid + 1;
  }
  let last = rows.length;
  for (let i = first; i <= rows.length; i++) {
    if (offsets[i] >= bottom) {
      last = i;
      break;
    }
  }
  const visible = rows.slice(first, last);

  return (
    <div ref={containerRef}>
      <div aria-hidden="true" style={{ height: offsets[first] }} />
      {visible.map((row) => (
        <div
          key={row.key}
          data-vrow={row.key}
          ref={(node) => {
            rowRefs.current.set(row.key, node);
          }}
        >
          {row.node}
        </div>
      ))}
      <div aria-hidden="true" style={{ height: Math.max(0, total - offsets[last]) }} />
    </div>
  );
}
