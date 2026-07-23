import { useEffect, useRef } from "react";
import { breathController } from "../breath";

/** A theme-aware canvas driven exclusively by the canonical BreathController. */
type Orb = {
  bx: number;
  by: number;
  ax: number;
  ay: number;
  sx: number;
  sy: number;
  phase: number;
  r: number;
  c: [number, number, number];
  bp: number;
};

const PALETTE_LIGHT: [number, number, number][] = [
  [127, 216, 240],
  [165, 230, 207],
  [195, 194, 255],
  [242, 196, 255],
  [165, 216, 255],
  [255, 212, 222],
];

const PALETTE_WARM: [number, number, number][] = [
  [255, 214, 140],
  [255, 199, 169],
  [255, 224, 189],
  [250, 214, 235],
  [255, 231, 179],
  [255, 212, 207],
];

const PALETTE_DARK: [number, number, number][] = [
  [45, 212, 191],
  [94, 234, 212],
  [129, 140, 248],
  [232, 121, 249],
  [56, 189, 248],
  [167, 243, 208],
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

function rng(seed: number) {
  let value = seed % 2147483647;
  if (value <= 0) value += 2147483646;
  return () => (value = (value * 16807) % 2147483647) / 2147483647;
}

function makeOrbs(palette: [number, number, number][]): Orb[] {
  const random = rng(982451653);
  return Array.from({ length: 6 }, (_, index) => ({
    bx: 0.12 + random() * 0.76,
    by: 0.12 + random() * 0.76,
    ax: 0.04 + random() * 0.1,
    ay: 0.04 + random() * 0.1,
    sx: 0.03 + random() * 0.05,
    sy: 0.03 + random() * 0.05,
    phase: random() * Math.PI * 2,
    r: 0.34 + random() * 0.28,
    c: palette[index % palette.length],
    bp: random() * Math.PI * 2,
  }));
}

export default function BreathField() {
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const context = canvas.getContext("2d", { alpha: false });
    if (!context) return;

    let width = 0;
    let height = 0;
    let dpr = 1;
    let raf = 0;
    let fieldTime = 0;
    let breathPhase = 0;
    let lastFrameAt = performance.now();
    let currentPalette = paletteFor(document.documentElement.getAttribute("data-theme"));
    let orbs = makeOrbs(currentPalette);
    const motionQuery = window.matchMedia("(prefers-reduced-motion: reduce)");

    const resize = () => {
      dpr = Math.min(window.devicePixelRatio || 1, 1.5);
      width = window.innerWidth;
      height = window.innerHeight;
      canvas.width = Math.floor(width * dpr);
      canvas.height = Math.floor(height * dpr);
      canvas.style.width = `${width}px`;
      canvas.style.height = `${height}px`;
      context.setTransform(dpr, 0, 0, dpr, 0, 0);
      schedule();
    };

    const render = (now: number) => {
      raf = 0;
      const elapsed = Math.min(Math.max((now - lastFrameAt) / 1000, 0), 0.1);
      lastFrameAt = now;
      const frame = breathController.sample();
      fieldTime += elapsed;
      breathPhase += elapsed * Math.PI * 2 * frame.frequency;

      const theme = document.documentElement.getAttribute("data-theme");
      const isDark = theme === "frost";
      const palette = paletteFor(theme);
      if (palette !== currentPalette) {
        currentPalette = palette;
        orbs = makeOrbs(palette);
      }

      context.globalCompositeOperation = "source-over";
      context.fillStyle = bgFor(theme);
      context.fillRect(0, 0, width, height);

      const lift = Math.min(0.55, frame.amplitude * 0.8 + frame.velocity * 0.28 + frame.flare * 0.42);
      const minDimension = Math.min(width, height);
      const driftScale = 0.55 + frame.turbulence * 1.45;
      context.globalCompositeOperation = isDark ? "lighter" : "source-over";

      for (const orb of orbs) {
        const x = (orb.bx + Math.sin(fieldTime * orb.sx + orb.phase) * orb.ax * driftScale) * width;
        const y = (orb.by + Math.cos(fieldTime * orb.sy + orb.phase * 1.3) * orb.ay * driftScale) * height;
        const breath = frame.paused ? 0 : Math.sin(breathPhase + orb.bp);
        const scale = 0.86 + breath * frame.amplitude;
        const radius = orb.r * minDimension * scale;
        const alphaScale = isDark ? 1 : 2.2;
        const baseAlpha = (0.05 + frame.amplitude * 0.48 + frame.velocity * 0.05) * alphaScale;
        const flareAlpha = lift * 0.12 * alphaScale;
        const alpha = Math.min(
          isDark ? 0.62 : 0.5,
          baseAlpha + flareAlpha + Math.max(0, breath) * frame.amplitude * 0.24 * alphaScale,
        );

        const [red, green, blue] = orb.c;
        const gradient = context.createRadialGradient(x, y, 0, x, y, radius);
        gradient.addColorStop(0, `rgba(${red},${green},${blue},${alpha})`);
        gradient.addColorStop(0.45, `rgba(${red},${green},${blue},${alpha * 0.35})`);
        gradient.addColorStop(1, `rgba(${red},${green},${blue},0)`);
        context.fillStyle = gradient;
        context.beginPath();
        context.arc(x, y, radius, 0, Math.PI * 2);
        context.fill();
      }

      if (lift > 0.02) {
        const centerX = width / 2;
        const centerY = height * 0.62;
        const gradient = context.createRadialGradient(centerX, centerY, 0, centerX, centerY, minDimension * 0.7);
        const cool = [94, 234, 212];
        const warm = frame.warmth < 0.3 ? [255, 89, 80] : frame.warmth > 0.72 ? [165, 230, 207] : cool;
        const bloomAlpha = lift * (isDark ? 0.06 : 0.10);
        gradient.addColorStop(0, `rgba(${warm[0]},${warm[1]},${warm[2]},${bloomAlpha})`);
        gradient.addColorStop(1, `rgba(${warm[0]},${warm[1]},${warm[2]},0)`);
        context.fillStyle = gradient;
        context.fillRect(0, 0, width, height);
      }

      context.globalCompositeOperation = "source-over";
      if (!frame.paused) raf = requestAnimationFrame(render);
    };

    function schedule() {
      if (raf || document.visibilityState === "hidden") return;
      raf = requestAnimationFrame(render);
    }

    const onVisibility = () => {
      const hidden = document.visibilityState === "hidden";
      breathController.setVisibility(hidden);
      if (hidden && raf) {
        cancelAnimationFrame(raf);
        raf = 0;
      } else if (!hidden) {
        lastFrameAt = performance.now();
        schedule();
      }
    };
    const onMotionPreference = () => {
      breathController.setReducedMotion(motionQuery.matches);
      schedule();
    };
    const themeObserver = new MutationObserver(schedule);
    const unsubscribe = breathController.subscribe(schedule);

    breathController.setVisibility(document.visibilityState === "hidden");
    breathController.setReducedMotion(motionQuery.matches);
    resize();
    window.addEventListener("resize", resize);
    document.addEventListener("visibilitychange", onVisibility);
    motionQuery.addEventListener("change", onMotionPreference);
    themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    schedule();

    return () => {
      if (raf) cancelAnimationFrame(raf);
      unsubscribe();
      themeObserver.disconnect();
      window.removeEventListener("resize", resize);
      document.removeEventListener("visibilitychange", onVisibility);
      motionQuery.removeEventListener("change", onMotionPreference);
      breathController.setVisibility(false);
      breathController.setReducedMotion(false);
    };
  }, []);

  return <canvas ref={canvasRef} className="breath-field" aria-hidden="true" />;
}
