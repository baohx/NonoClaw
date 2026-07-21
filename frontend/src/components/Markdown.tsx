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
          code({ className, children, ...props }: any) {
            const match = /language-(\w+)/.exec(className || "");
            const lang = match?.[1];
            const source = String(children).replace(/\n$/, "");
            if (lang === "mermaid") {
              return <MermaidBlock source={source} />;
            }
            return (
              <code className={className} {...props}>
                {children}
              </code>
            );
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
