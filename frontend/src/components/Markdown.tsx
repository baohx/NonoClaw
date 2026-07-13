import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import rehypeHighlight from "rehype-highlight";
import rehypeKatex from "rehype-katex";
import React from "react";
import type { ComponentPropsWithoutRef, ReactNode } from "react";

interface Props {
  content: string;
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
