// BreathMeter — a singleton that turns token-stream rhythm into a 0..1 velocity.
//
// It lives OUTSIDE React on purpose: text_delta events arrive many times per
// second during generation, and feeding them through the store would re-render
// the whole tree each time. Instead the WebSocket handler calls pulse() here,
// and the BreathField canvas reads sample() once per animation frame.

const IDLE = 0; // velocity floor when nothing is streaming
// Streaming chars/sec that maps to velocity = 1. Picked so normal prose
// (~1–4k chars/s) lands in the lively but not maxed-out band.
const FULL_RATE = 4000;
// EMA time constants (seconds): how fast the rate reacts / fades.
const RATE_TAU = 0.55;
// After this gap with no pulse we start fading the rate back to idle.
const GAP = 0.22;
// Intensity (tool spikes) decay time constant.
const INTENSITY_TAU = 0.9;

class BreathMeter {
  private rateEma = 0;
  private intensity = 0;
  private lastPulse = 0;
  private lastSample = 0;
  private streaming = false;

  /** Called on every streamed text_delta. `chars` is the delta length. */
  pulse(chars: number) {
    const now = perf();
    if (this.lastPulse > 0) {
      const dt = Math.max(now - this.lastPulse, 0.001);
      const instant = chars / dt; // chars/sec over the gap
      const alpha = 1 - Math.exp(-dt / RATE_TAU);
      this.rateEma = this.rateEma * (1 - alpha) + instant * alpha;
    }
    this.lastPulse = now;
    this.streaming = true;
  }

  /** Called when a turn finishes (assistant_done). */
  settle() {
    this.streaming = false;
  }

  /** Called on tool_use_start / tool_result — a brief flare of life. */
  flare(amount = 0.55) {
    this.intensity = Math.min(1, this.intensity + amount);
  }

  /** Read the current velocity [0..1] and intensity [0..1]. Frame-rate aware. */
  sample(): { velocity: number; intensity: number } {
    const now = perf();
    const dt = this.lastSample ? Math.min(now - this.lastSample, 0.1) : 1 / 60;
    this.lastSample = now;

    // If the stream has gone quiet, let the rate decay toward idle.
    const quiet = !this.streaming || (this.lastPulse > 0 && now - this.lastPulse > GAP);
    if (quiet) {
      this.rateEma *= Math.exp(-dt / RATE_TAU);
    }
    this.intensity *= Math.exp(-dt / INTENSITY_TAU);

    const velocity = this.streaming
      ? IDLE + (1 - IDLE) * Math.min(1, this.rateEma / FULL_RATE)
      : 0;
    return { velocity, intensity: this.intensity };
  }
}

function perf(): number {
  return typeof performance !== "undefined" ? performance.now() / 1000 : Date.now() / 1000;
}

export const breathMeter = new BreathMeter();
