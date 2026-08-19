import { useMemo } from "react";
import { useStore } from "../store";
import type { EngineEvent, TokenBudgetComponent } from "../types";

/**
 * F2 Context X-Ray（DSH dsh-context 思路）：解剖最近一次模型请求的上下文拼装。
 * 数据源：engine 每轮发出的 token_budget_breakdown 事件（system/tools/messages
 * 三个分区的完整组件明细），由 websocket 层原样存入 store.xrayBudget。
 *
 * X-Ray Doctor 诊断：重复注入检测（同名组件跨分区 / 组件占比异常）。
 */

type Comp = TokenBudgetComponent;

const SECTION_COLORS: Record<string, string> = {
  system: "var(--accent, #0071e3)",
  tools: "#34c759",
  messages: "#ff9f0a",
};

function compact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/** 单分区：排序后的组件条形列表。 */
function SectionList({ title, comps, color, charsPerToken }: {
  title: string; comps: Comp[]; color: string; charsPerToken: number;
}) {
  const sorted = useMemo(
    () => [...comps].sort((a, b) => b.chars - a.chars || a.name.localeCompare(b.name)),
    [comps]
  );
  const total = sorted.reduce((s, c) => s + c.chars, 0) || 1;
  return (
    <div className="xray-section">
      <div className="xray-section__head">
        <span className="xray-section__dot" style={{ background: color }} />
        <strong>{title}</strong>
        <span className="xray-section__sum">
          {compact(total)} chars ≈ {compact(Math.round(total / (charsPerToken || 4)))} tok
        </span>
      </div>
      {sorted.map((c) => {
        const pct = (c.chars / total) * 100;
        return (
          <div key={c.name} className="xray-row" title={`${c.name}: ${c.chars} chars`}>
            <span className="xray-row__name">{c.name}</span>
            <span className="xray-row__bar">
              <span className="xray-row__fill" style={{ width: `${Math.max(pct, 0.5)}%`, background: color }} />
            </span>
            <span className="xray-row__val">{compact(c.chars)}</span>
          </div>
        );
      })}
    </div>
  );
}

/** X-Ray Doctor：重复/异常诊断。 */
function Doctor({ budget }: { budget: EngineEvent }) {
  const findings = useMemo(() => {
    const out: string[] = [];
    const groups: [string, Comp[]][] = [
      ["system", budget.system ?? []],
      ["tools", budget.tools ?? []],
      ["messages", budget.messages ?? []],
    ];
    // 1. 跨分区同名组件 → 可能重复注入
    const seen = new Map<string, string>();
    for (const [g, comps] of groups) {
      for (const c of comps) {
        const prev = seen.get(c.name);
        if (prev && prev !== g) out.push(`duplicate component "${c.name}" in ${prev} and ${g}`);
        else seen.set(c.name, g);
      }
    }
    // 2. 单组件占比 >60% 警告
    for (const [g, comps] of groups) {
      const total = comps.reduce((s, c) => s + c.chars, 0);
      if (total > 0) {
        const top = comps.reduce((a, b) => (a.chars > b.chars ? a : b));
        if (top.chars / total > 0.6 && top.chars > 10_000) {
          out.push(`${g}: "${top.name}" 占 ${Math.round((top.chars / total) * 100)}%（${compact(top.chars)} chars）— 考虑瘦身`);
        }
      }
    }
    // 3. tools 分区总量 > system → 上下文被工具定义主导
    const sysChars = groups[0][1].reduce((s, c) => s + c.chars, 0);
    const toolChars = groups[1][1].reduce((s, c) => s + c.chars, 0);
    if (toolChars > sysChars * 2 && toolChars > 50_000) {
      out.push(`tool definitions（${compact(toolChars)}）远超 system prompt（${compact(sysChars)}）— 检查 MCP 工具数量`);
    }
    return out;
  }, [budget]);
  if (findings.length === 0) return null;
  return (
    <div className="xray-doctor">
      <strong>🩺 X-Ray Doctor</strong>
      {findings.map((f) => <div key={f} className="xray-doctor__item">· {f}</div>)}
    </div>
  );
}

export default function ContextXray() {
  const budget = useStore((s) => s.xrayBudget);
  if (!budget || budget.kind !== "token_budget_breakdown") {
    return (
      <div className="xray-empty">
        Context X-Ray 等待数据 — 发起一次对话后，此处展示每轮请求的
        system / tools / messages 完整拼装明细。
      </div>
    );
  }
  const cpt = budget.chars_per_token || 4;
  const grand = (budget.system_chars ?? 0) + (budget.tools_chars ?? 0) + (budget.messages_chars ?? 0);
  return (
    <div className="xray-root">
      <div className="xray-total">
        <span>合计 {compact(grand)} chars ≈ {compact(budget.estimated_tokens ?? Math.round(grand / cpt))} tokens</span>
        <span className="xray-total__legend">
          {(["system", "tools", "messages"] as const).map((g) => (
            <span key={g}>
              <i style={{ background: SECTION_COLORS[g] }} />{g}
            </span>
          ))}
        </span>
      </div>
      {/* 总量堆叠条 */}
      <div className="xray-stack">
        {(["system", "tools", "messages"] as const).map((g) => {
          const chars = budget[`${g}_chars` as const] ?? 0;
          const pct = grand > 0 ? (chars / grand) * 100 : 0;
          return (
            <span key={g}
              className="xray-stack__seg"
              style={{ width: `${pct}%`, background: SECTION_COLORS[g] }}
              title={`${g}: ${chars} chars (${pct.toFixed(1)}%)`} />
          );
        })}
      </div>
      <Doctor budget={budget} />
      <SectionList title="System Prompt（含 NONOCLAW.md / memory / skills）" comps={budget.system ?? []} color={SECTION_COLORS.system} charsPerToken={cpt} />
      <SectionList title="Tool Definitions" comps={budget.tools ?? []} color={SECTION_COLORS.tools} charsPerToken={cpt} />
      <SectionList title="Messages（对话历史 + 工具结果）" comps={budget.messages ?? []} color={SECTION_COLORS.messages} charsPerToken={cpt} />
    </div>
  );
}
