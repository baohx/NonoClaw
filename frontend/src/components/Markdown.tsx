import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import rehypeHighlight from "rehype-highlight";
import rehypeKatex from "rehype-katex";
import React, { useEffect, useRef } from "react";
import type { ComponentPropsWithoutRef, ReactNode } from "react";

interface Props {
  content: string;
}

/** Render mermaid diagrams in a container after mount. */
function MermaidBlock({ source }: { source: string }) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    // Clear previous render
    el.innerHTML = "";
    const id = `mermaid-${Math.random().toString(36).slice(2, 9)}`;
    el.innerHTML = `<div class="mermaid" id="${id}">${source}</div>`;
    // Give mermaid a wide canvas so diagrams fill the container.
    const mermaidDiv = el.querySelector<HTMLElement>(`#${id}`);
    if (mermaidDiv) {
      const w = el.clientWidth || 800;
      mermaidDiv.style.width = `${w}px`;
    }
    // Trigger mermaid if loaded
    const win = window as any;
    if (win.mermaid) {
      try {
        win.mermaid.run({ nodes: [el.querySelector(`#${id}`)] });
      } catch {
        el.innerHTML = `<pre class="mermaid-raw">${source}</pre>`;
      }
    }
  }, [source]);

  return <div ref={ref} className="mermaid-container" />;
}

/** Recursively extract plain text from React children (pierces hljs spans). */
function extractText(node: ReactNode): string {
  if (node == null || typeof node === "boolean") return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(extractText).join("");
  if (React.isValidElement<{ children?: ReactNode }>(node)) {
    return extractText(node.props.children);
  }
  return "";
}

/** Render an SVG code block inline as an image. */
function SvgBlock({ source }: { source: string }) {
  const svg = source.trim().replace(/^<\?xml[^>]*\?>\s*/i, "");
  return (
    <div
      className="svg-container"
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  );
}

/** Render an ECharts chart from inert JSON option data. */
function EChartsBlock({ source }: { source: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [status, setStatus] = React.useState<"pending" | "active" | "chart unavailable" | "error">("pending");
  const [error, setError] = React.useState("");

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    let chart: any = null;
    let observer: ResizeObserver | null = null;
    let frame = 0;
    let disposed = false;
    let retried = false;
    let retryTimer = 0;

    const dispose = () => {
      if (frame) cancelAnimationFrame(frame);
      observer?.disconnect();
      observer = null;
      if (chart) {
        try { chart.dispose(); } catch {}
        chart = null;
      }
    };
    const scheduleResize = () => {
      if (!chart || disposed || el.clientWidth <= 0 || el.clientHeight <= 0 || frame) return;
      frame = requestAnimationFrame(() => {
        frame = 0;
        if (chart && el.clientWidth > 0 && el.clientHeight > 0) chart.resize();
      });
    };
    const initialize = (isRetry: boolean) => {
      if (disposed || chart) return;
      const win = window as any;
      if (!win.echarts) {
        if (isRetry) {
          setStatus("chart unavailable");
          setError("ECharts library is unavailable");
        }
        return;
      }
      try {
        const option = JSON.parse(source);
        if (!option || typeof option !== "object") throw new Error("Chart Source must be a JSON object or array");
        chart = win.echarts.init(el);
        chart.setOption(option);
        if (typeof ResizeObserver !== "undefined") {
          observer = new ResizeObserver(scheduleResize);
          observer.observe(el);
        }
        scheduleResize();
        setError("");
        setStatus("active");
      } catch (reason) {
        dispose();
        setStatus("error");
        setError(reason instanceof Error ? reason.message : "ECharts rejected the option");
      }
    };
    const retryOnce = () => {
      if (retried || disposed || chart) return;
      retried = true;
      initialize(true);
    };

    setStatus("pending");
    setError("");
    initialize(false);
    if (!chart) {
      window.addEventListener("echarts-ready", retryOnce, { once: true });
      retryTimer = window.setTimeout(retryOnce, 250);
    }
    return () => {
      disposed = true;
      window.removeEventListener("echarts-ready", retryOnce);
      if (retryTimer) window.clearTimeout(retryTimer);
      dispose();
    };
  }, [source]);

  const copySource = async () => {
    try { await navigator.clipboard.writeText(source); } catch {}
  };

  return (
    <figure className="echarts-block" data-chart-status={status}>
      <div ref={ref} style={{ width: "100%", minHeight: 320 }} />
      <figcaption role="status">
        <span>{status}{error ? ` · ${error}` : ""}</span>
        <button type="button" onClick={copySource}>copy chart source</button>
      </figcaption>
      <details>
        <summary>Chart Source</summary>
        <pre><code>{source}</code></pre>
      </details>
    </figure>
  );
}

const PIPE = ""; // private-use char as pipe placeholder inside math

/** Recursively restore pipe placeholders in React children, preserving React elements. */
function restorePipes(children: ReactNode): ReactNode {
  if (typeof children === "string") return children.split(PIPE).join("|");
  if (Array.isArray(children))
    return React.Children.map(children, (c) => restorePipes(c));
  if (React.isValidElement<{ children?: ReactNode }>(children)) {
    const childProps = children.props as { children?: ReactNode };
    if (childProps.children !== undefined) {
      return React.cloneElement(children, {
        children: restorePipes(childProps.children),
      });
    }
  }
  return children;
}

export default function Markdown({ content }: Props) {
  // Protect | inside $...$ and $$...$$ so GFM table parser doesn't split on them.
  const processed = content
    .replace(/\$\$([\s\S]+?)\$\$/g, (_, inner: string) =>
      `$$${inner.split("|").join(PIPE)}$$`
    )
    .replace(/\$([^$\n]+?)\$/g, (_, inner: string) =>
      `$${inner.split("|").join(PIPE)}$`
    );

  return (
    <div className="markdown-body">
      <ReactMarkdown
        remarkPlugins={[remarkMath, remarkGfm]}
        rehypePlugins={[rehypeHighlight, rehypeKatex]}
        components={{
          pre({ children, ...props }: any) {
            // children is the <code> element; with rehype-highlight its
            // children are highlighted spans — pierce them for raw source.
            const codeEl = Array.isArray(children) ? children[0] : children;
            const codeProps = React.isValidElement<{ className?: string }>(codeEl)
              ? (codeEl.props as { className?: string })
              : {};
            const className = codeProps.className || "";
            const lang = /language-(\w+)/.exec(className)?.[1];
            const source = extractText(children).replace(/\n$/, "");
            if (lang === "mermaid") {
              return <MermaidBlock source={source} />;
            }
            if (lang === "svg") {
              return <SvgBlock source={source} />;
            }
            if (lang === "echarts") {
              return <EChartsBlock source={source} />;
            }
            return <pre {...props}>{children}</pre>;
          },
          a: ({ href, children }) => (
            <a href={href} target="_blank" rel="noopener noreferrer">
              {children}
            </a>
          ),
          td: ({ children, ...props }: ComponentPropsWithoutRef<"td">) => (
            <td {...props}>{restorePipes(children)}</td>
          ),
          th: ({ children, ...props }: ComponentPropsWithoutRef<"th">) => (
            <th {...props}>{restorePipes(children)}</th>
          ),
        }}
      >
        {processed}
      </ReactMarkdown>
    </div>
  );
}
