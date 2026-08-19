import { memo, useEffect, useRef, useState } from "react";
import type { ChatMessage, ClientMsg, SubagentRun, SubagentTool } from "../types";
import { useStore } from "../store";
import Markdown from "./Markdown";
import { createRenderedExportArtifact, type ExportFormat } from "../export";

/** F3 Message Fork: branch the session before this user message via the
 * REST fork endpoint, then switch to the new session. */
async function forkAtMessage(msg: ChatMessage, send: (m: ClientMsg) => void) {
  const state = useStore.getState();
  const sessionId = state.sessionId;
  if (!sessionId || msg.srcIndex === undefined) return;
  if (!window.confirm("Fork：将在此消息前分支新会话（复制之前的对话），并切换过去？")) return;
  try {
    const res = await fetch(
      `/api/sessions/${encodeURIComponent(sessionId)}/fork`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ at_index: msg.srcIndex, title: `fork @${msg.srcIndex}` }),
      },
    );
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      throw new Error(body.error || `fork failed (${res.status})`);
    }
    const body = await res.json() as { session_id: string };
    state.prepareSessionSwitch(body.session_id);
    // Same resume path as the session rail; messages arrive via MessagesLoaded.
    send({ type: "resume_session", id: body.session_id });
  } catch (reason) {
    window.alert(reason instanceof Error ? reason.message : "fork failed");
  }
}

interface Props {
  messages: ChatMessage[];
  streamingIdx: number | null;
  toolsHidden: boolean;
  send: (message: ClientMsg) => void;
}

export default function ChatView({ messages, toolsHidden, send }: Props) {
  if (!toolsHidden) {
    return (
      <div>
        {messages.length === 0 && <WelcomeMessage />}
        {messages.map((msg) => (
          <MessageCard
            key={msg.id}
            msg={msg}
            send={send}
            isLastAssistant={msg.role === "assistant" && !msg.streaming && msg.content.trim().length > 0}
          />
        ))}
      </div>
    );
  }

  // toolsHidden: group consecutive tool messages into a single placeholder.
  const rendered: React.ReactNode[] = [];
  let toolGroupCount = 0;
  let toolGroupFirstId = "";
  let groupKeyIndex = 0;
  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i];
    if (msg.role !== "tool") {
      toolGroupCount = 0;
      toolGroupFirstId = "";
      rendered.push(
        <MessageCard
          key={msg.id}
          msg={msg}
          send={send}
          isLastAssistant={msg.role === "assistant" && !msg.streaming && msg.content.trim().length > 0}
        />
      );
      continue;
    }
    // This is a tool message — count it in the current group.
    toolGroupCount++;
    if (toolGroupFirstId === "") toolGroupFirstId = msg.id;
    // Peek ahead: if the next message is also a tool, skip rendering the placeholder.
    const nextMsg = messages[i + 1];
    if (nextMsg && nextMsg.role === "tool") continue;
    // This is the last tool in a consecutive group — render a single placeholder.
    rendered.push(
      <div
        id={`msg-${toolGroupFirstId}`}
        className="msg msg-enter tool-hidden-placeholder"
        key={`tool-group-${groupKeyIndex++}`}
      >
        <span className="tool-hidden-placeholder__icon">◇</span>
        <span className="tool-hidden-placeholder__text">
          {toolGroupCount === 1
            ? "tool box hidden"
            : `${toolGroupCount} tool boxes hidden`}
        </span>
      </div>
    );
  }

  return (
    <div>
      {messages.length === 0 && <WelcomeMessage />}
      {rendered}
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

// Export artifacts are generated locally and structurally validated before download.

async function exportResponse(content: string, format: "md" | ExportFormat, renderedElement: HTMLElement | null) {
  if (format === "md") {
    const blob = new Blob([content], { type: "text/markdown;charset=utf-8" });
    downloadBlob(blob, "nonoclaw-export.md");
    return;
  }
  if (!renderedElement) throw new Error("The rendered response is not available for export");
  const renderedContent = renderedElement.querySelector<HTMLElement>(".markdown-body") ?? renderedElement;
  const artifact = await createRenderedExportArtifact(format, renderedContent);
  downloadBlob(new Blob([artifact.bytes], { type: artifact.mime }), artifact.filename);
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
  send,
}: {
  msg: ChatMessage;
  isLastAssistant: boolean;
  send: (m: ClientMsg) => void;
}) {
  const [showExport, setShowExport] = useState(false);
  const [exporting, setExporting] = useState<ExportFormat | null>(null);
  const renderedRef = useRef<HTMLDivElement>(null);

  if (msg.role === "system") {
    return (
      <div className="msg msg--system msg-enter">
        <span className="msg__line">{msg.content}</span>
      </div>
    );
  }
  if (msg.role === "tool") {
    return (
      <div className="msg msg-enter">
        <ToolCard msg={msg} />
      </div>
    );
  }

  const time = (() => {
    if (!msg.timestamp) return "";
    const d = new Date(msg.timestamp);
    if (isNaN(d.getTime())) return "";
    const yyyy = d.getFullYear();
    const mm = String(d.getMonth() + 1).padStart(2, "0");
    const dd = String(d.getDate()).padStart(2, "0");
    const hh = String(d.getHours()).padStart(2, "0");
    const mi = String(d.getMinutes()).padStart(2, "0");
    return `${yyyy}-${mm}-${dd} ${hh}:${mi}`;
  })();

  const isUser = msg.role === "user";
  const located = useStore((s) => s.locatedMessageId === msg.id);
  return (
    <div id={`msg-${msg.id}`} className={`msg msg-enter msg--${isUser ? "user" : "assistant"}${located ? " msg--located" : ""}`}>
      <div className={`msg__role msg__role--${isUser ? "user" : "assistant"}`}>
        <span className="msg__role-mark" />
        {isUser ? "you" : "Nono"}
        {time && <span className="msg__time">{time}</span>}
        {/* User message: copy + fork buttons */}
        {isUser && (
          <>
            <button
              className="msg-action"
              title="Copy"
              onClick={() => copyText(msg.content)}
            >
              ⧉
            </button>
            <button
              className="msg-action"
              title="Fork: 从这条消息之前分支一个新会话（不含此消息），可改写后重发"
              onClick={() => forkAtMessage(msg, send)}
            >
              ⟲
            </button>
          </>
        )}
        {/* Last assistant message: copy + export md */}
        {!isUser && isLastAssistant && (
          <>
            <button className="msg-action" title="Copy markdown" onClick={() => copyText(msg.content)}>⧉</button>
            <button className="msg-action" title="Export this completed turn" aria-expanded={showExport} onClick={() => setShowExport((value) => !value)}>↓</button>
            {showExport && (
              <span className="msg-export-options" role="menu" aria-label="Export format">
                {(["md", "docx", "pdf"] as const).map((format) => (
                  <button key={format} role="menuitem" disabled={exporting !== null} onClick={async () => {
                    const richFormat = format === "md" ? null : format;
                    try {
                      setExporting(richFormat);
                      await exportResponse(msg.content, format, renderedRef.current);
                      setShowExport(false);
                    } catch (reason) {
                      window.alert(reason instanceof Error ? reason.message : "Rendered export failed");
                    } finally {
                      setExporting(null);
                    }
                  }}>{exporting === format ? `${format.toUpperCase()}…` : format.toUpperCase()}</button>
                ))}
              </span>
            )}
          </>
        )}
      </div>
      <div className="msg__inner">
        <div ref={renderedRef} className="msg__bubble">
          {isUser ? (
            <>
              {msg.attachments && msg.attachments.length > 0 && (
                <div className="msg__attachments" aria-label="Attachments">
                  {msg.attachments.map((attachment, index) => (
                    attachment.previewUrl ? (
                      <img
                        key={`${attachment.filename}-${index}`}
                        src={attachment.previewUrl}
                        alt={attachment.filename}
                        className="msg__image"
                        title={attachment.filename}
                        onClick={() => window.open(attachment.previewUrl, "_blank")}
                      />
                    ) : (
                      <span
                        key={`${attachment.filename}-${index}`}
                        className="msg__attachment"
                        title={attachment.filename}
                      >
                        <span className="msg__attachment-icon" aria-hidden="true">↗</span>
                        <span className="msg__attachment-name">{attachment.filename}</span>
                      </span>
                    )
                  ))}
                </div>
              )}
              <span className="msg__user-text">{msg.content}</span>
            </>
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

const EMPTY_CHILD_IDS: string[] = [];

const ToolCard = memo(function ToolCard({ msg }: { msg: ChatMessage }) {
  const name = msg.toolName || "tool";
  const supportsChildren = name === "Agent" || name === "Coordinator";
  const parentToolId = msg.id.startsWith("tool-") ? msg.id.slice(5) : msg.id;
  const childIds = useStore((state) => supportsChildren
    ? state.childIdsByParentToolId[parentToolId] ?? EMPTY_CHILD_IDS
    : EMPTY_CHILD_IDS);
  const childRunsById = useStore((state) => state.subagentRunsById);
  const children = childIds.map((id) => childRunsById[id]).filter((run): run is SubagentRun => !!run);
  const childRunning = children.some((child) => child.status === "running" || child.status === "waiting_permission");
  const [collapsed, setCollapsed] = useState(true);
  const manuallyToggled = useRef(false);
  const prevStreaming = useRef(msg.streaming);

  // Child-bearing tools default open while active and fold after completion;
  // once the user chooses a state, live events never override it.
  useEffect(() => {
    if (!manuallyToggled.current) {
      if (supportsChildren && children.length > 0) setCollapsed(!childRunning);
      else if (prevStreaming.current && !msg.streaming) setCollapsed(true);
    }
    prevStreaming.current = msg.streaming;
  }, [childRunning, children.length, msg.streaming, supportsChildren]);

  const running = msg.streaming;
  const failed = msg.toolOk === false;
  const statusClass = running ? "run" : failed ? "err" : "ok";
  const statusSym = running ? "CHECKING" : failed ? "FAILURE" : "SUCCESS";
  const inputPreview = toolInputPreview(name, msg.toolInput);
  const toggle = () => {
    manuallyToggled.current = true;
    setCollapsed((value) => !value);
  };

  return (
    <div className="toolcard">
      <div className="toolcard__head" onClick={toggle}>
        <span className={`toolcard__status ${statusClass}`}>{statusSym}</span>
        <span className="toolcard__name">{name}</span>
        {inputPreview && <code className="toolcard__cmd">{inputPreview}</code>}
        {children.length > 0 && <span className="toolcard__child-count">{children.length} child{children.length === 1 ? "" : "ren"}</span>}
        <span className="toolcard__chev">{collapsed ? "▸" : "▾"}</span>
      </div>
      {!collapsed && (
        <div className="toolcard__details">
          <strong>Command</strong>
          <pre className="toolcard__pre">{JSON.stringify(msg.toolInput ?? {}, null, 2)}</pre>
          <strong>Result</strong>
          <pre className="toolcard__pre">{restoreNewlines(msg.content)}</pre>
          {supportsChildren && children.length > 0 && (
            <div className="subagents" aria-label="Subagent runs">
              {children.map((child) => <SubagentRunCard key={child.id} run={child} />)}
            </div>
          )}
        </div>
      )}
    </div>
  );
});

const SubagentRunCard = memo(function SubagentRunCard({ run }: { run: SubagentRun }) {
  const running = run.status === "running" || run.status === "waiting_permission";
  const [collapsed, setCollapsed] = useState(!running);
  const manual = useRef(false);
  const wasRunning = useRef(running);

  useEffect(() => {
    if (!manual.current) {
      if (running) setCollapsed(false);
      else if (wasRunning.current) setCollapsed(true);
    }
    wasRunning.current = running;
  }, [running]);

  const failed = run.status === "failed" || run.status === "interrupted";
  const statusClass = running ? "run" : failed ? "err" : "ok";
  return (
    <section className="subagent">
      <button className="subagent__head" type="button" onClick={() => {
        manual.current = true;
        setCollapsed((value) => !value);
      }}>
        <span className={`subagent__dot ${statusClass}`} />
        <span className="subagent__index">#{run.index + 1}</span>
        <span className="subagent__description">{run.description}</span>
        {run.profile && <span className="subagent__profile">{run.profile}</span>}
        <span className={`subagent__status ${statusClass}`}>{run.status.replace("_", " ")}</span>
        <span className="toolcard__chev">{collapsed ? "▸" : "▾"}</span>
      </button>
      {!collapsed && (
        <div className="subagent__body">
          {run.toolOrder.length > 0 && (
            <div className="subagent__tools">
              {run.toolOrder.map((id) => run.toolsById[id] && <SubagentToolCard key={id} tool={run.toolsById[id]} />)}
            </div>
          )}
          {(run.output || run.outputTruncated) && (
            <div className="subagent__output">
              <strong>{running ? "Streaming output" : "Final output"}</strong>
              {running ? <StreamingText text={run.output} /> : <Markdown content={run.output} />}
              {run.outputTruncated && <div className="subagent__truncated">Output truncated in browser state.</div>}
            </div>
          )}
        </div>
      )}
    </section>
  );
});

const SubagentToolCard = memo(function SubagentToolCard({ tool }: { tool: SubagentTool }) {
  // Child tool payloads/results are intentionally opt-in, even after success.
  const [collapsed, setCollapsed] = useState(true);
  const running = ["pending", "queued", "validated", "permission_allowed", "running"].includes(tool.status);
  const failed = tool.ok === false || tool.status.includes("failed") || tool.status.includes("denied");
  return (
    <div className="subtool">
      <button className="subtool__head" type="button" onClick={() => setCollapsed((value) => !value)}>
        <span className={`subagent__dot ${running ? "run" : failed ? "err" : "ok"}`} />
        <span className="subtool__name">{tool.name}</span>
        <span className="subtool__status">{tool.status.replace("_", " ")}</span>
        {tool.permission && <span className="subtool__permission">permission: {tool.permission}</span>}
        <span className="toolcard__chev">{collapsed ? "▸" : "▾"}</span>
      </button>
      {!collapsed && (
        <div className="subtool__details">
          {tool.input !== undefined && <pre className="toolcard__pre">{JSON.stringify(tool.input, null, 2)}</pre>}
          <pre className="toolcard__pre">{restoreNewlines(tool.result || (running ? "Waiting for result…" : "[no output]"))}</pre>
          {tool.truncated && <div className="subagent__truncated">Tool result truncated.</div>}
        </div>
      )}
    </div>
  );
});

function restoreNewlines(s: string): string {
  return s.replace(/ ?⏎ ?/g, "\n");
}
