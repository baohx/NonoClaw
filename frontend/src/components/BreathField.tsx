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

// Light theme ("Studio"): soft pastel tones, reads on #f5f5f7.
const PALETTE_LIGHT: [number, number, number][] = [
  [127, 216, 240], // soft cyan
  [165, 230, 207], // pastel mint
  [195, 194, 255], // lavender
  [242, 196, 255], // lilac
  [165, 216, 255], // powder blue
  [255, 212, 222], // blush
];

// Warm light theme ("Warm"): peachy/amber pastels.
const PALETTE_WARM: [number, number, number][] = [
  [255, 214, 140], // honey
  [255, 199, 169], // apricot
  [255, 224, 189], // cream peach
  [250, 214, 235], // rose cream
  [255, 231, 179], // wheat
  [255, 212, 207], // coral light
];

// Dark theme ("Night"): bioluminescence on near-black (original palette).
const PALETTE_DARK: [number, number, number][] = [
  [45, 212, 191], // cyan
  [94, 234, 212], // mint
  [129, 140, 248], // violet
  [232, 121, 249], // magenta
  [56, 189, 248], // sky
  [167, 243, 208], // pale mint
];

function paletteFor(theme: string | null): [number, number, number][] {
  if (theme === "frost") return PALETTE_DARK;
  if (theme === "amber") return PALETTE_WARM;
  return PALETTE_LIGHT;
}

function bgFor(theme: string | null): string {
  if (theme === "frost") return "#161619";
  if (theme === "amber") return "#faf6ef";
  return "#f5f5f7";
}

// Deterministic pseudo-random in [0,1) from a seed — keeps the field stable
// across renders (no Math.random so the layout doesn't jump on hot reload).
function rng(seed: number) {
  let s = seed % 2147483647;
  if (s <= 0) s += 2147483646;
  return () => (s = (s * 16807) % 2147483647) / 2147483647;
}

function makeOrbs(palette: [number, number, number][]): Orb[] {
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
      c: palette[i % palette.length],
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
    let orbs = makeOrbs(paletteFor(document.documentElement.getAttribute("data-theme")));

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

      // Theme-aware: palette + background + blend mode, re-read per frame so
      // theme switches apply live.
      const theme = document.documentElement.getAttribute("data-theme");
      const isDark = theme === "frost";
      const palette = paletteFor(theme);
      if (orbs.length && orbs[0].c !== palette[0]) {
        orbs = makeOrbs(palette);
      }

      // Breath frequency: idle ~0.16 Hz (slow sigh), full stream ~1.05 Hz.
      const breathHz = 0.16 + velocity * 0.89;

      // Base background.
      ctx.globalCompositeOperation = "source-over";
      ctx.fillStyle = bgFor(theme);
      ctx.fillRect(0, 0, w, h);

      // Overall ambient lift from velocity + tool flares.
      const lift = Math.min(0.5, velocity * 0.4 + intensity * 0.5);

      const minDim = Math.min(w, h);
      // Dark: additive glow. Light: normal alpha-blended pastel washes
      // (additive on white would be invisible).
      ctx.globalCompositeOperation = isDark ? "lighter" : "source-over";

      for (const o of orbs) {
        // Slow positional drift.
        const x = (o.bx + Math.sin(t * o.sx + o.phase) * o.ax) * w;
        const y = (o.by + Math.cos(t * o.sy + o.phase * 1.3) * o.ay) * h;

        // Breath: scale oscillates with the token-driven frequency.
        const breath = Math.sin(t * Math.PI * 2 * breathHz + o.bp);
        const scale = 0.82 + breath * (0.12 + velocity * 0.16);
        const radius = o.r * minDim * scale;

        // Alpha: idle baseline so it always glows softly; brighter when active.
        // Light themes need ~2.2x the alpha of the additive dark path to read.
        const alphaScale = isDark ? 1 : 2.2;
        const baseA = (0.05 + velocity * 0.16) * alphaScale;
        const flareA = lift * 0.12 * alphaScale;
        const alpha = Math.min(
          isDark ? 0.62 : 0.5,
          baseA + flareA + Math.max(0, breath) * (0.03 + velocity * 0.05) * alphaScale
        );

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
        const bloom = isDark
          ? `rgba(94,234,212,${lift * 0.06})`
          : `rgba(127,216,240,${lift * 0.10})`;
        gr.addColorStop(0, bloom);
        gr.addColorStop(1, "rgba(127,216,240,0)");
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
