import { memo, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import type { ChatMessage } from "../types";
import Markdown from "./Markdown";

interface Props {
  messages: ChatMessage[];
  streamingIdx: number | null;
}

export default function ChatView({ messages }: Props) {
  // Find the last non-streaming assistant message index for export buttons.
  const lastAssistantIdx = (() => {
    for (let i = messages.length - 1; i >= 0; i--) {
      const m = messages[i];
      if (m.role === "assistant" && !m.streaming && m.content.trim()) return i;
    }
    return -1;
  })();

  return (
    <div>
      {messages.length === 0 && <WelcomeMessage />}
      {messages.map((msg, i) => (
        <MessageCard
          key={msg.id}
          msg={msg}
          isLastAssistant={i === lastAssistantIdx}
        />
      ))}
    </div>
  );
}

function WelcomeMessage() {
  return (
    <div className="welcome">
      <div className="welcome__mark">
        Nono<i>Claw</i>
      </div>
      <div className="welcome__sub">A Rust agent CLI. Type a prompt below to begin.</div>
      <div className="welcome__hint">
        Ctrl / Enter to send · /clear to reset · the reef breathes with the token stream
      </div>
    </div>
  );
}

// ── Clipboard helper ────────────────────────────────────────────────────────
function copyText(text: string) {
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text);
  } else {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed"; ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    document.body.removeChild(ta);
  }
}

// ── Export helpers ──────────────────────────────────────────────────────────

/** Render markdown to an HTML string using the same Markdown component. */
function markdownToHtml(md: string): Promise<string> {
  return new Promise((resolve) => {
    const container = document.createElement("div");
    container.style.position = "fixed"; container.style.opacity = "0";
    document.body.appendChild(container);
    const root = createRoot(container);
    root.render(
      <Markdown content={md} />
    );
    // Wait one tick for React to flush.
    setTimeout(() => {
      const html = container.innerHTML;
      root.unmount();
      document.body.removeChild(container);
      resolve(html);
    }, 50);
  });
}

const EXPORT_CSS = `
  body { font-family: -apple-system, "Segoe UI", sans-serif; color: #1a1a1a; max-width: 800px; margin: 40px auto; line-height: 1.7; }
  pre { background: #f5f5f5; padding: 12px; border-radius: 6px; overflow-x: auto; font-size: 13px; }
  code { background: #f0f0f0; padding: 2px 5px; border-radius: 3px; font-size: 0.9em; }
  pre code { background: none; padding: 0; }
  table { border-collapse: collapse; width: 100%; }
  th, td { border: 1px solid #ddd; padding: 6px 12px; text-align: left; }
  th { background: #f5f5f5; }
  blockquote { border-left: 3px solid #ccc; padding-left: 12px; color: #666; margin: 8px 0; }
  h1 { font-size: 1.6em; } h2 { font-size: 1.3em; } h3 { font-size: 1.1em; }
  img { max-width: 100%; }
`;

async function exportMarkdown(content: string, format: "md" | "docx" | "pdf") {
  if (format === "md") {
    const blob = new Blob([content], { type: "text/markdown;charset=utf-8" });
    downloadBlob(blob, "nonoclaw-export.md");
    return;
  }

  const html = await markdownToHtml(content);
  const fullHtml = `<!DOCTYPE html><html><head><meta charset="utf-8"><style>${EXPORT_CSS}</style></head><body>${html}</body></html>`;

  if (format === "pdf") {
    // Open in a new window and trigger print → user selects "Save as PDF".
    const w = window.open("", "_blank");
    if (!w) return;
    w.document.write(fullHtml);
    w.document.close();
    setTimeout(() => w.print(), 300);
    return;
  }

  if (format === "docx") {
    // Word-compatible HTML — download as .doc (Word opens HTML with .doc ext).
    const docxHtml = `<!DOCTYPE html><html xmlns:o="urn:schemas-microsoft-com:office:office" xmlns:w="urn:schemas-microsoft-com:office:word" xmlns="http://www.w3.org/TR/REC-html40"><head><meta charset="utf-8"><style>${EXPORT_CSS}</style></head><body>${html}</body></html>`;
    const blob = new Blob(["﻿", docxHtml], { type: "application/msword" });
    downloadBlob(blob, "nonoclaw-export.doc");
    return;
  }
}

function downloadBlob(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
}

// ── Message card ────────────────────────────────────────────────────────────

const MessageCard = memo(function MessageCard({
  msg,
  isLastAssistant,
}: {
  msg: ChatMessage;
  isLastAssistant: boolean;
}) {
  const [showExport, setShowExport] = useState(false);

  if (msg.role === "system") {
    return (
      <div className="msg msg--system msg-enter">
        <span className="msg__line">{msg.content}</span>
      </div>
    );
  }
  if (msg.role === "tool") {
    return (
      <div className="msg-enter">
        <ToolCard msg={msg} />
      </div>
    );
  }

  const isUser = msg.role === "user";
  return (
    <div className={`msg msg-enter msg--${isUser ? "user" : "assistant"}`}>
      <div className={`msg__role msg__role--${isUser ? "user" : "assistant"}`}>
        <span className="msg__role-mark" />
        {isUser ? "you" : "Nono"}
        {/* User message: copy button */}
        {isUser && (
          <button
            className="msg-action"
            title="Copy"
            onClick={() => copyText(msg.content)}
          >
            ⧉
          </button>
        )}
        {/* Last assistant message: copy + export md */}
        {!isUser && isLastAssistant && (
          <>
            <button
              className="msg-action"
              title="Copy markdown"
              onClick={() => copyText(msg.content)}
            >
              ⧉
            </button>
            <button
              className="msg-action"
              title="Export as Markdown"
              onClick={() => exportMarkdown(msg.content, "md")}
            >
              ↓
            </button>
          </>
        )}
      </div>
      <div className="msg__inner">
        <div className="msg__bubble">
          {isUser ? (
            msg.content
          ) : msg.streaming ? (
            <StreamingText text={msg.content} />
          ) : (
            <Markdown content={msg.content} />
          )}
        </div>
      </div>
    </div>
  );
});

function StreamingText({ text }: { text: string }) {
  return (
    <>
      <pre className="stream-plain">{text}</pre>
      <span className="stream-caret" />
    </>
  );
}

/** Extract a one-line summary from a tool's input for display. */
function toolInputPreview(name: string, input: unknown): string {
  if (!input || typeof input !== "object") return "";
  const obj = input as Record<string, unknown>;
  // Show the most relevant field for each tool.
  const key = name === "Bash" ? "command" :
    name === "WebFetch" ? "url" :
    name === "WebSearch" ? "query" :
    name === "Grep" ? "pattern" :
    name === "Glob" ? "pattern" :
    name === "TodoWrite" ? undefined :
    Object.keys(obj)[0];
  if (!key || !(key in obj)) return "";
  const val = String(obj[key]);
  const max = 200;
  return val.length > max ? val.slice(0, max) + "…" : val;
}

const ToolCard = memo(function ToolCard({ msg }: { msg: ChatMessage }) {
  const [collapsed, setCollapsed] = useState(true);
  const prevStreaming = useRef(msg.streaming);

  // Auto-collapse when the tool result arrives (streaming → done).
  useEffect(() => {
    if (prevStreaming.current && !msg.streaming) {
      setCollapsed(true);
    }
    prevStreaming.current = msg.streaming;
  }, [msg.streaming]);

  const running = msg.streaming;
  const failed = msg.toolOk === false;
  const statusClass = running ? "run" : failed ? "err" : "ok";
  const statusSym = running ? "◌" : failed ? "✕" : "✓";
  const name = msg.toolName || "tool";
  const inputPreview = toolInputPreview(name, msg.toolInput);

  return (
    <div className="toolcard">
      <div className="toolcard__head" onClick={() => setCollapsed((c) => !c)}>
        <span className={`toolcard__status ${statusClass}`}>{statusSym}</span>
        <span className="toolcard__name">{name}</span>
        {inputPreview && (
          <code className="toolcard__cmd">{inputPreview}</code>
        )}
        <span className="toolcard__chev">{collapsed ? "▸" : "▾"}</span>
      </div>
      {!collapsed && (
        <pre className="toolcard__pre">{restoreNewlines(msg.content)}</pre>
      )}
    </div>
  );
});

function restoreNewlines(s: string): string {
  return s.replace(/ ?⏎ ?/g, "\n");
}
