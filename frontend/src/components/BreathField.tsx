import { useEffect, useRef } from "react";
import { breathMeter } from "../breath";

/**
 * BreathField — the "deep reef" living background.
 *
 * A handful of large, soft, additively-blended orbs drift slowly and breathe.
 * Their pulse frequency and brightness follow the token-stream rhythm reported
 * by the BreathMeter: idle = a slow, deep sigh; streaming = a quicker shimmer;
 * tool activity = a brief flare. Rendered on a canvas at -1 z so all UI floats
 * above it.
 */

type Orb = {
  // base position as a fraction of viewport
  bx: number;
  by: number;
  // drift amplitude + speed
  ax: number;
  ay: number;
  sx: number;
  sy: number;
  phase: number;
  // radius as a fraction of min(viewport)
  r: number;
  // color [r,g,b]
  c: [number, number, number];
  // per-orb breath phase offset
  bp: number;
};

// Bioluminescence palette — cyan / mint / magenta / violet, all in 0-255 rgb.
const PALETTE: [number, number, number][] = [
  [45, 212, 191], // cyan   #2dd4bf
  [94, 234, 212], // mint   #5eead4
  [129, 140, 248], // violet #818cf8
  [232, 121, 249], // magenta #e879f9
  [56, 189, 248], // sky    #38bdf8
  [167, 243, 208], // pale mint #a7f3d0
];

// Deterministic pseudo-random in [0,1) from a seed — keeps the field stable
// across renders (no Math.random so the layout doesn't jump on hot reload).
function rng(seed: number) {
  let s = seed % 2147483647;
  if (s <= 0) s += 2147483646;
  return () => (s = (s * 16807) % 2147483647) / 2147483647;
}

function makeOrbs(): Orb[] {
  const rand = rng(982451653);
  return Array.from({ length: 6 }, (_, i) => {
    const r = rand();
    return {
      bx: 0.12 + rand() * 0.76,
      by: 0.12 + rand() * 0.76,
      ax: 0.04 + rand() * 0.1,
      ay: 0.04 + rand() * 0.1,
      sx: 0.03 + rand() * 0.05,
      sy: 0.03 + rand() * 0.05,
      phase: rand() * Math.PI * 2,
      r: 0.34 + rand() * 0.28,
      c: PALETTE[i % PALETTE.length],
      bp: rand() * Math.PI * 2,
    };
  });
}

export default function BreathField() {
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d", { alpha: false });
    if (!ctx) return;

    let w = 0;
    let h = 0;
    let dpr = 1;
    const orbs = makeOrbs();

    const resize = () => {
      dpr = Math.min(window.devicePixelRatio || 1, 1.5);
      w = window.innerWidth;
      h = window.innerHeight;
      canvas.width = Math.floor(w * dpr);
      canvas.height = Math.floor(h * dpr);
      canvas.style.width = w + "px";
      canvas.style.height = h + "px";
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    };
    resize();
    window.addEventListener("resize", resize);

    let raf = 0;
    const start = performance.now();

    const render = () => {
      raf = requestAnimationFrame(render);
      const t = (performance.now() - start) / 1000;
      const { velocity, intensity } = breathMeter.sample();

      // Breath frequency: idle ~0.16 Hz (slow sigh), full stream ~1.05 Hz.
      const breathHz = 0.16 + velocity * 0.89;

      // Base background — near-black with a faint teal cast.
      ctx.globalCompositeOperation = "source-over";
      ctx.fillStyle = "#070a0f";
      ctx.fillRect(0, 0, w, h);

      // Overall ambient lift from velocity + tool flares.
      const lift = Math.min(0.5, velocity * 0.4 + intensity * 0.5);

      const minDim = Math.min(w, h);
      ctx.globalCompositeOperation = "lighter";

      for (const o of orbs) {
        // Slow positional drift.
        const x = (o.bx + Math.sin(t * o.sx + o.phase) * o.ax) * w;
        const y = (o.by + Math.cos(t * o.sy + o.phase * 1.3) * o.ay) * h;

        // Breath: scale oscillates with the token-driven frequency.
        const breath = Math.sin(t * Math.PI * 2 * breathHz + o.bp);
        const scale = 0.82 + breath * (0.12 + velocity * 0.16);
        const radius = o.r * minDim * scale;

        // Alpha: idle baseline so it always glows softly; brighter when active.
        const baseA = 0.05 + velocity * 0.16;
        const flareA = lift * 0.12;
        const alpha = Math.min(0.62, baseA + flareA + Math.max(0, breath) * (0.03 + velocity * 0.05));

        const [cr, cg, cb] = o.c;
        const grad = ctx.createRadialGradient(x, y, 0, x, y, radius);
        grad.addColorStop(0, `rgba(${cr},${cg},${cb},${alpha})`);
        grad.addColorStop(0.45, `rgba(${cr},${cg},${cb},${alpha * 0.35})`);
        grad.addColorStop(1, `rgba(${cr},${cg},${cb},0)`);
        ctx.fillStyle = grad;
        ctx.beginPath();
        ctx.arc(x, y, radius, 0, Math.PI * 2);
        ctx.fill();
      }

      // A quiet full-field bloom on intensity spikes (tool activity).
      if (lift > 0.02) {
        const cx = w / 2;
        const cy = h * 0.62;
        const gr = ctx.createRadialGradient(cx, cy, 0, cx, cy, minDim * 0.7);
        gr.addColorStop(0, `rgba(94,234,212,${lift * 0.06})`);
        gr.addColorStop(1, "rgba(94,234,212,0)");
        ctx.fillStyle = gr;
        ctx.fillRect(0, 0, w, h);
      }

      ctx.globalCompositeOperation = "source-over";
    };
    render();

    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("resize", resize);
    };
  }, []);

  return <canvas ref={canvasRef} className="breath-field" aria-hidden="true" />;
}
