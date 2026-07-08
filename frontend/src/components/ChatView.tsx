import { useState } from "react";
import type { ChatMessage } from "../types";
import Markdown from "./Markdown";

interface Props {
  messages: ChatMessage[];
  streamingIdx: number | null;
}

export default function ChatView({ messages }: Props) {
  return (
    <div>
      {messages.length === 0 && <WelcomeMessage />}
      {messages.map((msg) => (
        <MessageCard key={msg.id} msg={msg} />
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
        ⌘/Ctrl + Enter to send · /clear to reset · the reef breathes with the token stream
      </div>
    </div>
  );
}

function MessageCard({ msg }: { msg: ChatMessage }) {
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
        {isUser ? "you" : "assistant"}
      </div>
      <div className="msg__inner">
        <div className="msg__bubble">
          {isUser ? (
            msg.content
          ) : (
            <>
              <Markdown content={msg.content} />
              {msg.streaming && <span className="stream-caret" />}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function ToolCard({ msg }: { msg: ChatMessage }) {
  const [collapsed, setCollapsed] = useState(false);
  const running = msg.streaming;
  const failed = msg.toolOk === false;
  const statusClass = running ? "run" : failed ? "err" : "ok";
  const statusSym = running ? "◌" : failed ? "✕" : "✓";

  return (
    <div className="toolcard">
      <div className="toolcard__head" onClick={() => setCollapsed((c) => !c)}>
        <span className={`toolcard__status ${statusClass}`}>{statusSym}</span>
        <span className="toolcard__name">{msg.toolName}</span>
        <span className="toolcard__chev">{collapsed ? "▸" : "▾"}</span>
      </div>
      {!collapsed && (
        <pre className="toolcard__pre">{restoreNewlines(msg.content)}</pre>
      )}
    </div>
  );
}

/** The backend flattens newlines in tool-result previews as ` ⏎ ` so they stay
 *  on one line. Restore real newlines for display in the expanded panel. */
function restoreNewlines(s: string): string {
  return s.replace(/ ?⏎ ?/g, "\n");
}
