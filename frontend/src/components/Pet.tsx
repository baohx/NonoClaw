import { useEffect, useState } from "react";
import { breathController, type BreathPhase } from "../breath";

/**
 * F1 状态宠物（DSH dsh-digipet/dsh-clippy 思路）：右下角像素猫爪，
 * 由 BreathController 的 canonical 状态机驱动 —— 打瞌睡/追毛线球/
 * 举爪提问/炸毛/睡觉。纯前端，零 token。
 */

type PetPhase = BreathPhase;

/** 每个呼吸阶段 → 宠物 CSS 状态类。缺省（connecting/waiting/compacting/subagent
 *  等未映射阶段）回退 idle 姿态。 */
const PHASE_TO_PET: Partial<Record<PetPhase, string>> = {
  idle: "pet-idle",
  thinking: "pet-sleep",
  streaming: "pet-sleep",
  tool: "pet-yarn",
  "waiting-permission": "pet-ask",
  "waiting-question": "pet-ask",
  reconnecting: "pet-idle",
  error: "pet-blow",
  success: "pet-happy",
};

/** 猫爪像素 SVG：三个肉垫 + 掌心。 */
function PawBody({ color, className }: { color: string; className?: string }) {
  return (
    <g fill={color} className={className}>
      {/* 主掌心（爱心形） */}
      <path d="M8,10 h8 c2,0 3,1 3,3 c0,3 -3,6 -7,8 c-4,-2 -7,-5 -7,-8 c0,-2 1,-3 3,-3 z" />
      {/* 左趾 */}
      <ellipse cx="4.5" cy="9" rx="2.6" ry="3.4" />
      {/* 中趾 */}
      <ellipse cx="11" cy="6.5" rx="2.8" ry="3.6" />
      {/* 右趾 */}
      <ellipse cx="17.5" cy="9" rx="2.6" ry="3.4" />
    </g>
  );
}

/** 按宠物状态渲染不同姿态/装饰的猫爪。pet 取已映射的 pet-* 状态值。 */
function PetFrame({ pet, theme }: { pet: string; theme: string }) {
  const pawColor = theme === "dark" ? "#f0abfc" : "#e879f9";
  const dim = theme === "dark" ? "#a78bfa" : "#8b5cf6";
  switch (pet) {
    case "pet-sleep": // thinking/streaming：打瞌睡
      return (
        <g>
          <PawBody color={pawColor} />
          {/* Zzz 泡泡 */}
          <text x="17" y="6" fontSize="4" fill={dim} className="pet-zzz-1">z</text>
          <text x="20" y="3.5" fontSize="3" fill={dim} className="pet-zzz-2">z</text>
          <ellipse cx="9" cy="14" rx="1" ry="0.6" fill="#00000033" />
        </g>
      );
    case "pet-yarn": // tool 运行中：追毛线球
      return (
        <g>
          <PawBody color={pawColor} className="pet-yarn-paw" />
          {/* 毛线球 */}
          <circle cx="18" cy="16" r="3" fill={dim} className="pet-yarn-ball" />
          <circle cx="18" cy="16" r="3" fill="none" stroke="#ffffff55" strokeWidth="0.4" />
          <path d="M15.5,14.5 Q18,17 20.5,14.5 M15.8,17.5 Q18,15 20.2,17.5"
            fill="none" stroke="#ffffff44" strokeWidth="0.3" />
        </g>
      );
    case "pet-ask": // 等权限/问题：举爪问号
      return (
        <g>
          <PawBody color={pawColor} />
          {/* 举起的前爪 */}
          <ellipse cx="17" cy="7" rx="2" ry="2.6" fill={pawColor} className="pet-raise" />
          <text x="18.5" y="4" fontSize="5" fill={dim} fontWeight="bold">?</text>
        </g>
      );
    case "pet-blow": // 错误：炸毛
      return (
        <g>
          {/* 放大 + 尖刺轮廓表现炸毛 */}
          <g className="pet-blow-body">
            <PawBody color={dim} />
          </g>
          {/* 尖刺 */}
          {Array.from({ length: 8 }).map((_, i) => {
            const a = (i / 8) * Math.PI * 2;
            return (
              <line key={i}
                x1={11 + Math.cos(a) * 6} y1={12 + Math.sin(a) * 6}
                x2={11 + Math.cos(a) * 9} y2={12 + Math.sin(a) * 9}
                stroke={dim} strokeWidth="1" className="pet-spike" />
            );
          })}
          <text x="19" y="5" fontSize="6" fill="#ef4444" fontWeight="bold">!</text>
        </g>
      );
    case "pet-happy": // run 成功：开心跳
      return (
        <g className="pet-jump">
          <PawBody color={pawColor} />
          {/* 开心眯眼 ^^ */}
          <path d="M7,10 q1.5,-1.5 3,0 M12,10 q1.5,-1.5 3,0"
            fill="none" stroke="#00000055" strokeWidth="0.5" />
        </g>
      );
    default: // idle / reconnecting
      return <PawBody color={pawColor} />;
  }
}

/** 情绪映射到宠物动画。error/success 是瞬态（breath errorHoldMs~1.9s 后回 idle），
 *  宠物跟随同一状态机自然回落，无需额外生命周期管理。 */
export default function Pet() {
  const [phase, setPhase] = useState<PetPhase>("idle");
  const [theme, setTheme] = useState<string>(
    () => document.documentElement.dataset.theme ?? "light"
  );

  useEffect(() => {
    const unsub = breathController.subscribe((snap) => {
      setPhase(snap.phase);
      const t = document.documentElement.dataset.theme;
      if (t) setTheme(t);
    });
    return unsub;
  }, []);

  const pet = PHASE_TO_PET[phase] ?? "pet-idle";

  return (
    <div className={`pet-corner pet-${pet}`} title={`paw says: ${pet}`} aria-hidden>
      <svg viewBox="0 0 26 26" width="56" height="56" className="pet-svg">
        <PetFrame pet={pet} theme={theme} />
      </svg>
    </div>
  );
}
