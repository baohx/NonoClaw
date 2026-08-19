import { useMemo } from "react";
import { useStore } from "../store";
import type { ChatMessage, SessionInfoWire } from "../types";

/**
 * F4 Trajectory Replay（DSH dsh-trajectory-debug 思路）：
 * 当前会话的消息时间轴 —— 每条消息一个节点（user/assistant/tool/system），
 * 点击定位到消息；顶部展示 fork 血缘链（解析各 session 的 fork:<sid>#<at> tag），
 * 形成"分支树"的线性化视图（祖先 → 当前）。
 *
 * 纯前端消费现有数据（messages + sessions），零 engine 改动。
 */

const ROLE_COLORS: Record<ChatMessage["role"], string> = {
  user: "#0071e3",
  assistant: "#34c759",
  tool: "#ff9f0a",
  system: "#8e8e93",
};

/** 解析 "fork:<sid>#<index>" tag → 血缘。 */
function parseFork(tag: string | null | undefined): { sid: string; at: number } | null {
  if (!tag) return null;
  const m = /^fork:([0-9a-f-]+)#(\d+)$/.exec(tag);
  return m ? { sid: m[1], at: Number(m[2]) } : null;
}

/** 从 sessions 构建当前会话的祖先链（最近祖先在前）。 */
function ancestry(currentId: string, sessions: SessionInfoWire[]): { sid: string; at: number; title: string }[] {
  const byId = new Map(sessions.map((s) => [s.id, s]));
  const chain: { sid: string; at: number; title: string }[] = [];
  let cursor = byId.get(currentId);
  const seen = new Set<string>([currentId]);
  while (cursor) {
    const fork = parseFork(cursor.tag);
    if (!fork || seen.has(fork.sid)) break;
    seen.add(fork.sid);
    const parent = byId.get(fork.sid);
    chain.push({
      sid: fork.sid,
      at: fork.at,
      title: parent?.title ?? parent?.summary?.slice(0, 24) ?? fork.sid.slice(0, 8),
    });
    cursor = parent;
  }
  return chain;
}

export default function TrajectoryReplay() {
  const messages = useStore((s) => s.messages);
  const sessions = useStore((s) => s.sessions);
  const sessionId = useStore((s) => s.sessionId);
  const setLocatedMessage = useStore((s) => s.setLocatedMessage);

  const lineage = useMemo(
    () => (sessionId ? ancestry(sessionId, sessions) : []),
    [sessionId, sessions]
  );

  if (messages.length === 0) {
    return (
      <div className="traj-empty">Trajectory Replay 等待数据 — 对话开始后显示消息时间轴。</div>
    );
  }

  return (
    <div className="traj-root">
      {lineage.length > 0 && (
        <div className="traj-lineage">
          <strong>🔀 分支链</strong>
          {lineage.map((anc, i) => (
            <span key={anc.sid} className="traj-lineage__hop" title={`${anc.sid} @ msg ${anc.at}`}>
              ← {i === lineage.length - 1 ? "根" : ""} {anc.title}
            </span>
          ))}
          <span className="traj-lineage__hop traj-lineage__cur">● 当前分支</span>
        </div>
      )}
      <div className="traj-axis">
        {messages.map((msg) => (
          <button
            key={msg.id}
            className={`traj-node traj-node--${msg.role}`}
            title={`#${msg.srcIndex ?? "?"} ${msg.role}: ${msg.content.slice(0, 80)}`}
            onClick={() => {
              // In toolsHidden mode a whole group of tool messages collapses into
              // one placeholder anchored at the group's FIRST tool id — jump there.
              if (msg.role === "tool") {
                const idx = messages.indexOf(msg);
                let first = idx;
                while (first > 0 && messages[first - 1].role === "tool") first--;
                setLocatedMessage(messages[first].id);
              } else {
                setLocatedMessage(msg.id);
              }
            }}
          >
            <span
              className="traj-node__dot"
              style={{ background: ROLE_COLORS[msg.role] ?? "#8e8e93" }}
            />
          </button>
        ))}
      </div>
      <div className="traj-legend">
        {(["user", "assistant", "tool", "system"] as const).map((r) => (
          <span key={r}>
            <i style={{ background: ROLE_COLORS[r] }} />{r}
          </span>
        ))}
        <span className="traj-legend__hint">点击节点定位消息</span>
      </div>
    </div>
  );
}
