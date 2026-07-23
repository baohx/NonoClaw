import type { EngineEvent } from "./types";

export type BreathPhase =
  | "idle"
  | "connecting"
  | "thinking"
  | "streaming"
  | "tool"
  | "waiting"
  | "waiting-permission"
  | "waiting-question"
  | "compacting"
  | "subagent"
  | "success"
  | "error"
  | "reconnecting";

export interface BreathSnapshot {
  phase: BreathPhase;
  label: string;
  hidden: boolean;
  reducedMotion: boolean;
}

export interface BreathVisualState {
  amplitude: number;
  frequency: number;
  turbulence: number;
  warmth: number;
  flare: number;
  velocity: number;
  paused: boolean;
}

interface VisualTarget {
  amplitude: number;
  frequency: number;
  turbulence: number;
  warmth: number;
}

interface BreathControllerOptions {
  now?: () => number;
  transitionTauMs?: number;
  successHoldMs?: number;
  errorHoldMs?: number;
}

type ConnectionSignal = "connecting" | "connected" | "disconnected" | "closed";
type Listener = (snapshot: BreathSnapshot) => void;

const TARGETS: Record<BreathPhase, VisualTarget> = {
  idle: { amplitude: 0.10, frequency: 0.16, turbulence: 0.05, warmth: 0.42 },
  connecting: { amplitude: 0.15, frequency: 0.42, turbulence: 0.12, warmth: 0.48 },
  thinking: { amplitude: 0.18, frequency: 0.48, turbulence: 0.20, warmth: 0.48 },
  streaming: { amplitude: 0.24, frequency: 0.68, turbulence: 0.28, warmth: 0.54 },
  tool: { amplitude: 0.27, frequency: 0.52, turbulence: 0.36, warmth: 0.58 },
  waiting: { amplitude: 0.11, frequency: 0.13, turbulence: 0.06, warmth: 0.50 },
  "waiting-permission": { amplitude: 0.12, frequency: 0.12, turbulence: 0.05, warmth: 0.66 },
  "waiting-question": { amplitude: 0.12, frequency: 0.12, turbulence: 0.05, warmth: 0.60 },
  compacting: { amplitude: 0.08, frequency: 0.24, turbulence: 0.04, warmth: 0.46 },
  subagent: { amplitude: 0.23, frequency: 0.44, turbulence: 0.32, warmth: 0.56 },
  success: { amplitude: 0.32, frequency: 0.32, turbulence: 0.12, warmth: 0.82 },
  error: { amplitude: 0.07, frequency: 0.18, turbulence: 0.30, warmth: 0.12 },
  reconnecting: { amplitude: 0.13, frequency: 0.20, turbulence: 0.16, warmth: 0.40 },
};

const LABELS: Record<BreathPhase, string> = {
  idle: "idle",
  connecting: "connecting",
  thinking: "thinking",
  streaming: "streaming",
  tool: "running tool",
  waiting: "waiting",
  "waiting-permission": "waiting for permission",
  "waiting-question": "waiting for answer",
  compacting: "compacting",
  subagent: "running subagent",
  success: "complete",
  error: "error",
  reconnecting: "reconnecting",
};

const TOKEN_ENERGY_PER_CHAR = 1 / 180;
const MAX_TOKEN_ENERGY = 1;
const TOKEN_DECAY_TAU_MS = 520;
const FLARE_DECAY_TAU_MS = 850;
const TOKEN_GAP_MS = 240;

function clamp(value: number, min = 0, max = 1): number {
  return Math.min(max, Math.max(min, value));
}

function defaultNow(): number {
  return typeof performance !== "undefined" ? performance.now() : Date.now();
}

function statusFailed(status: string | undefined): boolean {
  return status === "failed" || status === "denied";
}

/**
 * The sole owner of breath state. It consumes canonical run/connection events,
 * while the canvas samples bounded frame data without causing token-rate React renders.
 */
export class BreathController {
  private readonly now: () => number;
  private readonly transitionTauMs: number;
  private readonly successHoldMs: number;
  private readonly errorHoldMs: number;
  private readonly listeners = new Set<Listener>();
  private readonly activeTools = new Set<string>();
  private activeSubagents = 0;
  private phase: BreathPhase = "idle";
  private label = LABELS.idle;
  private hidden = false;
  private reducedMotion = false;
  private connectedOnce = false;
  private connected = false;
  private runActive = false;
  private resumePhase: BreathPhase = "thinking";
  private transientUntil = 0;
  private tokenEnergy = 0;
  private flareEnergy = 0;
  private flareWarmth = TARGETS.idle.warmth;
  private lastTokenAt = 0;
  private lastSampleAt: number | null = null;
  private visual: VisualTarget = { ...TARGETS.idle };

  constructor(options: BreathControllerOptions = {}) {
    this.now = options.now ?? defaultNow;
    this.transitionTauMs = options.transitionTauMs ?? 360;
    this.successHoldMs = options.successHoldMs ?? 1300;
    this.errorHoldMs = options.errorHoldMs ?? 1900;
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    listener(this.getSnapshot());
    return () => this.listeners.delete(listener);
  }

  getSnapshot(): BreathSnapshot {
    return {
      phase: this.phase,
      label: this.label,
      hidden: this.hidden,
      reducedMotion: this.reducedMotion,
    };
  }

  consumeConnection(signal: ConnectionSignal): void {
    switch (signal) {
      case "connecting":
        this.connected = false;
        this.transition(this.connectedOnce ? "reconnecting" : "connecting");
        break;
      case "connected":
        this.connected = true;
        this.connectedOnce = true;
        this.transition(this.runActive ? this.resumePhase : "idle");
        break;
      case "disconnected":
        this.connected = false;
        this.resumePhase = this.runActive && this.phase !== "reconnecting" ? this.phase : this.resumePhase;
        this.transition(this.connectedOnce ? "reconnecting" : "connecting");
        break;
      case "closed":
        this.connected = false;
        this.connectedOnce = false;
        this.resetRun();
        break;
    }
  }

  consume(event: EngineEvent): void {
    switch (event.kind) {
      case "run_started":
        this.runActive = true;
        this.activeTools.clear();
        this.activeSubagents = 0;
        this.resumePhase = "thinking";
        this.transition("thinking");
        break;
      case "context_prepared":
      case "model_request_started":
      case "model_resolved":
        if (this.runActive) this.transition("thinking");
        break;
      case "thinking_state":
        if (event.active !== false) this.transition("thinking");
        break;
      case "stream_state_changed":
        this.consumeStreamState(event.state);
        break;
      case "text_delta":
        this.runActive = true;
        this.addTokenEnergy((event.text ?? "").length);
        this.transition("streaming");
        break;
      case "tool_use_start":
        this.beginTool(event.id ?? event.tool_use_id ?? "legacy-tool");
        break;
      case "tool_queued":
      case "tool_execution_started":
        this.beginTool(event.tool_use_id ?? event.id ?? "tool");
        break;
      case "tool_result":
        this.finishTool(event.id ?? event.tool_use_id ?? "legacy-tool", event.ok !== false);
        break;
      case "tool_execution_finished":
        this.finishTool(event.tool_use_id ?? event.id ?? "tool", !statusFailed(event.status));
        break;
      case "permission_requested":
        this.resumePhase = this.activeTools.size ? "tool" : "thinking";
        this.transition("waiting-permission", event.waiting_on ? `waiting for ${event.waiting_on}` : undefined);
        break;
      case "permission_resolved":
        this.transition(this.activeTools.size ? "tool" : "thinking");
        break;
      case "retry_scheduled":
        this.resumePhase = "thinking";
        this.transition("waiting", event.delay_ms ? `retrying in ${event.delay_ms} ms` : "retrying");
        break;
      case "provider_diagnostic":
        if (event.status === "waiting") this.transition("waiting", event.detail || undefined);
        break;
      case "compacting":
      case "compaction_started":
        this.resumePhase = this.runActive ? "thinking" : "idle";
        this.transition("compacting");
        break;
      case "compacted":
        this.flare(0.38, 0.72);
        this.transition(this.runActive ? "thinking" : "success");
        if (!this.runActive) this.transientUntil = this.now() + this.successHoldMs;
        break;
      case "subagent_started":
        this.activeSubagents += 1;
        this.transition("subagent");
        this.flare(0.34, 0.62);
        break;
      case "subagent_finished":
        this.activeSubagents = Math.max(0, this.activeSubagents - 1);
        this.flare(statusFailed(event.status) ? 0.50 : 0.34, statusFailed(event.status) ? 0.16 : 0.72);
        this.transition(this.activeSubagents ? "subagent" : this.activeTools.size ? "tool" : "thinking");
        break;
      case "hook_started":
        if (event.status === "waiting") this.transition("waiting", "waiting for hook");
        break;
      case "hook_finished":
        this.flare(statusFailed(event.status) ? 0.42 : 0.24, statusFailed(event.status) ? 0.16 : 0.68);
        break;
      case "assistant_done":
        if (this.runActive && !this.activeTools.size && !this.activeSubagents) this.transition("thinking");
        break;
      case "cancellation_requested":
        this.transition("waiting", "cancelling");
        break;
      case "run_error":
        this.finishRun("error", event.message);
        break;
      case "run_finished":
        if (event.status === "cancelled") this.resetRun();
        else this.finishRun(statusFailed(event.status) ? "error" : "success", event.reason);
        break;
    }
  }

  consumePrompt(kind: "permission" | "question", waiting: boolean): void {
    if (waiting) {
      this.resumePhase = this.activeTools.size ? "tool" : "thinking";
      this.transition(kind === "permission" ? "waiting-permission" : "waiting-question");
    } else if (this.phase === `waiting-${kind}`) {
      this.transition(this.activeTools.size ? "tool" : this.runActive ? "thinking" : "idle");
    }
  }

  consumeTerminal(status: "success" | "error", detail?: string): void {
    this.finishRun(status, detail);
  }

  signalError(detail?: string): void {
    this.resumePhase = this.phase;
    this.transition("error", detail || undefined);
    this.flare(0.46, 0.10);
    this.transientUntil = this.now() + this.errorHoldMs;
  }

  setVisibility(hidden: boolean): void {
    if (this.hidden === hidden) return;
    this.hidden = hidden;
    if (!hidden) this.lastSampleAt = this.now();
    this.emit();
  }

  setReducedMotion(reduced: boolean): void {
    if (this.reducedMotion === reduced) return;
    this.reducedMotion = reduced;
    this.emit();
  }

  resetRun(): void {
    this.runActive = false;
    this.activeTools.clear();
    this.activeSubagents = 0;
    this.tokenEnergy = 0;
    this.transientUntil = 0;
    this.resumePhase = "thinking";
    this.transition(this.connected ? "idle" : this.connectedOnce ? "reconnecting" : "idle");
  }

  sample(): BreathVisualState {
    const now = this.now();
    this.expireTransient(now);
    const dt = this.lastSampleAt === null ? 1000 / 60 : clamp(now - this.lastSampleAt, 0, 100);
    this.lastSampleAt = now;

    const target = TARGETS[this.phase];
    const alpha = this.reducedMotion ? 1 : 1 - Math.exp(-dt / this.transitionTauMs);
    this.visual = {
      amplitude: this.visual.amplitude + (target.amplitude - this.visual.amplitude) * alpha,
      frequency: this.visual.frequency + (target.frequency - this.visual.frequency) * alpha,
      turbulence: this.visual.turbulence + (target.turbulence - this.visual.turbulence) * alpha,
      warmth: this.visual.warmth + (target.warmth - this.visual.warmth) * alpha,
    };

    const tokenQuiet = !this.runActive || !this.lastTokenAt || now - this.lastTokenAt > TOKEN_GAP_MS;
    if (tokenQuiet) this.tokenEnergy *= Math.exp(-dt / TOKEN_DECAY_TAU_MS);
    this.flareEnergy *= Math.exp(-dt / FLARE_DECAY_TAU_MS);
    this.flareWarmth += (target.warmth - this.flareWarmth) * (1 - Math.exp(-dt / FLARE_DECAY_TAU_MS));

    const paused = this.hidden || this.reducedMotion;
    const motionScale = this.reducedMotion ? 0 : 1;
    return {
      amplitude: clamp(this.visual.amplitude * motionScale, 0, 0.4),
      frequency: clamp(this.visual.frequency * motionScale, 0, 1.2),
      turbulence: clamp(this.visual.turbulence * motionScale, 0, 1),
      warmth: clamp(this.visual.warmth * 0.65 + this.flareWarmth * 0.35),
      flare: clamp(this.flareEnergy * (this.reducedMotion ? 0.15 : 1)),
      velocity: clamp(this.tokenEnergy),
      paused,
    };
  }

  private consumeStreamState(state: string | undefined): void {
    switch (state) {
      case "connecting":
        this.transition("connecting", "connecting to model");
        break;
      case "thinking":
        this.transition("thinking");
        break;
      case "streaming":
        this.transition("streaming");
        break;
      case "interrupted":
        this.signalError("stream interrupted");
        break;
    }
  }

  private beginTool(id: string): void {
    this.runActive = true;
    this.activeTools.add(id);
    this.transition("tool");
    this.flare(0.42, 0.62);
  }

  private finishTool(id: string, ok: boolean): void {
    this.activeTools.delete(id);
    this.flare(ok ? 0.34 : 0.52, ok ? 0.74 : 0.10);
    if (this.activeTools.size) this.transition("tool");
    else if (this.activeSubagents) this.transition("subagent");
    else if (this.runActive) this.transition("thinking");
  }

  private finishRun(status: "success" | "error", detail?: string): void {
    if (!this.runActive && this.phase === status && this.transientUntil > this.now()) return;
    this.runActive = false;
    this.activeTools.clear();
    this.activeSubagents = 0;
    this.tokenEnergy = 0;
    this.transition(status, detail || undefined);
    this.flare(status === "success" ? 0.58 : 0.50, status === "success" ? 0.88 : 0.08);
    this.transientUntil = this.now() + (status === "success" ? this.successHoldMs : this.errorHoldMs);
  }

  private addTokenEnergy(chars: number): void {
    if (chars <= 0) return;
    this.tokenEnergy = clamp(this.tokenEnergy + Math.min(chars, 2048) * TOKEN_ENERGY_PER_CHAR, 0, MAX_TOKEN_ENERGY);
    this.lastTokenAt = this.now();
  }

  private flare(amount: number, warmth: number): void {
    this.flareEnergy = clamp(this.flareEnergy + amount);
    this.flareWarmth = clamp(warmth);
  }

  private expireTransient(now: number): void {
    if (!this.transientUntil || now < this.transientUntil) return;
    const restore = this.runActive ? this.resumePhase : this.connected ? "idle" : this.connectedOnce ? "reconnecting" : "idle";
    this.transientUntil = 0;
    this.transition(restore);
  }

  private transition(phase: BreathPhase, label = LABELS[phase]): void {
    if (phase !== "success" && phase !== "error") this.transientUntil = 0;
    if (this.phase === phase && this.label === label) return;
    this.phase = phase;
    this.label = label;
    if (this.runActive && (phase === "thinking" || phase === "streaming" || phase === "tool"
      || phase === "compacting" || phase === "subagent")) {
      this.resumePhase = phase;
    }
    this.emit();
  }

  private emit(): void {
    const snapshot = this.getSnapshot();
    for (const listener of this.listeners) listener(snapshot);
  }
}

export const breathController = new BreathController();
