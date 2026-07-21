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

/** Render an ECharts chart from a JSON option block. */
function EChartsBlock({ source }: { source: string }) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const win = window as any;
    if (!win.echarts) {
      el.innerHTML = `<pre class="mermaid-raw">ECharts not loaded</pre>`;
      return;
    }
    try {
      const option = JSON.parse(source);
      const chart = win.echarts.init(el, undefined, {
        width: undefined,
        height: 400,
      });
      chart.setOption(option);
      const onResize = () => chart.resize();
      window.addEventListener("resize", onResize);
      return () => { window.removeEventListener("resize", onResize); chart.dispose(); };
    } catch {
      el.innerHTML = `<pre class="mermaid-raw">${source}</pre>`;
    }
  }, [source]);

  return <div ref={ref} style={{ width: "100%", minHeight: 320 }} />;
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
