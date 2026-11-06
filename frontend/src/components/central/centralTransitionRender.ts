/**
 * 中枢模式切换动画的 Canvas 2D 绘制（从 docs/central-demo.html 调参台移植）。
 *
 * 与调参台的差别：app 内只用一层覆盖画布（主体之上、顶栏之下），
 * 所有绘制额外乘以 `alpha`（sin(π·t/T) 曲线），使转场两端完全透明、
 * 结束后不残留任何程序化背景，静态界面保持 app 现版原貌。
 */
import {
  clamp01,
  easeInOut,
  easeOut,
  easeSoft,
  segmentProgress,
  type CentralTransitionParams,
  type Timeline,
} from './centralTransitionParams';

const TAU = Math.PI * 2;

export interface TransitionParticle {
  /** 归一化起点（0~1，随窗口尺寸缩放） */
  sx: number;
  sy: number;
  dir: number;
  delay: number;
  size: number;
  bright: number;
  alt: boolean;
  wobble: number;
  speed: number;
}

export interface TransitionStar {
  x: number;
  y: number;
  r: number;
  ph: number;
  b: number;
}

export interface FrameOptions {
  width: number;
  height: number;
  /** 时间轴位置（秒） */
  t: number;
  /** 环境相位（轨道公转 / 星点闪烁 / 核心呼吸） */
  clock: number;
  /** 覆盖层整体可见度：两端 0、中段 1 */
  alpha: number;
  timeline: Timeline;
  params: CentralTransitionParams;
  particles: readonly TransitionParticle[];
  stars: readonly TransitionStar[];
}

function rgb(hex: string): [number, number, number] {
  const raw = hex.replace('#', '');
  const value = parseInt(
    raw.length === 3 ? raw.split('').map((c) => c + c).join('') : raw,
    16,
  );
  return [(value >> 16) & 255, (value >> 8) & 255, value & 255];
}

function rgba(hex: string, alpha: number): string {
  const [r, g, b] = rgb(hex);
  return `rgba(${r},${g},${b},${clamp01(alpha)})`;
}

function mulberry32(seed: number): () => number {
  let state = seed;
  return () => {
    state |= 0;
    state = (state + 0x6d2b79f5) | 0;
    let value = Math.imul(state ^ (state >>> 15), 1 | state);
    value = (value + Math.imul(value ^ (value >>> 7), 61 | value)) ^ value;
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
  };
}

/** 光尘起点按 app 主体区域采样：侧栏 16rem / 消息区 / 输入区 / 顶栏下缘。 */
const SOURCES = [
  { x: 0.01, y: 0.07, w: 0.19, h: 0.87 },
  { x: 0.24, y: 0.1, w: 0.7, h: 0.66 },
  { x: 0.24, y: 0.8, w: 0.7, h: 0.14 },
  { x: 0.02, y: 0.055, w: 0.96, h: 0.02 },
] as const;

export function buildTransitionParticles(
  params: CentralTransitionParams,
  seed = 20241106,
): TransitionParticle[] {
  const random = mulberry32(seed);
  const count = Math.max(0, Math.round(params.particles));
  return Array.from({ length: count }, () => {
    const source = SOURCES[Math.floor(random() * SOURCES.length)];
    return {
      sx: source.x + random() * source.w,
      sy: source.y + random() * source.h,
      dir: random() < 0.5 ? -1 : 1,
      delay: random() * params.stagger,
      size: (0.55 + random() * 0.9) * params.size,
      bright: 0.35 + random() * 0.65,
      alt: random() < 0.24,
      wobble: random() * TAU,
      speed: 0.82 + random() * 0.36,
    };
  });
}

export function buildTransitionStars(
  params: CentralTransitionParams,
  seed = 20241106,
): TransitionStar[] {
  const random = mulberry32(seed ^ 0x5f3759df);
  return Array.from({ length: Math.max(0, Math.round(params.stars)) }, () => ({
    x: random(),
    y: random(),
    r: 0.5 + random() * 1.3,
    ph: random() * TAU,
    b: 0.3 + random() * 0.7,
  }));
}

function particlePos(
  particle: TransitionParticle,
  e: number,
  w: number,
  h: number,
  cx: number,
  cy: number,
  params: CentralTransitionParams,
): [number, number] {
  const dx = particle.sx * w - cx;
  const dy = particle.sy * h - cy;
  const dist = Math.hypot(dx, dy) || 1;
  const swirl = Math.sin(Math.PI * e) * params.curvature * dist * 0.34 * particle.dir;
  const wobble = Math.sin(e * 9 + particle.wobble) * 3 * (1 - e);
  return [
    cx + dx * (1 - e) + (-dy / dist) * swirl + wobble,
    cy + dy * (1 - e) + (dx / dist) * swirl + wobble * 0.6,
  ];
}

function drawStars(ctx: CanvasRenderingContext2D, opts: FrameOptions): void {
  const { width: w, height: h, clock, alpha, stars, params } = opts;
  const globalP = clamp01(opts.t / opts.timeline.total);
  const env = (0.3 + 0.7 * globalP) * alpha;
  for (const star of stars) {
    const a = (0.1 + 0.42 * Math.abs(Math.sin(clock * 0.55 + star.ph))) * star.b * env;
    if (a < 0.008) continue;
    ctx.fillStyle = rgba(params.accent, a);
    ctx.fillRect(star.x * w, star.y * h, star.r, star.r);
  }
}

function drawOrbits(ctx: CanvasRenderingContext2D, opts: FrameOptions): void {
  const { width: w, height: h, clock, alpha, params, timeline, t } = opts;
  const cx = w / 2;
  const cy = h / 2;
  const vis = easeOut(segmentProgress('open', t, timeline));
  const ambient = (0.32 + 0.68 * (1 - easeSoft(segmentProgress('in', t, timeline)))) * alpha;
  const count = Math.max(1, Math.round(params.orbits));
  ctx.save();
  ctx.translate(cx, cy);
  for (let i = 0; i < count; i++) {
    const f = count === 1 ? 1 : i / (count - 1);
    const open = easeOut(clamp01(vis * 1.35 - i * 0.07));
    const radius = params.orbitRadius * (0.32 + 0.68 * f) * open;
    if (radius < 2) continue;
    const ry = Math.max(1, radius * params.tilt);
    ctx.lineWidth = 1;
    ctx.strokeStyle = rgba(params.accent, (0.115 + 0.065 * (1 - f)) * ambient * open);
    ctx.beginPath();
    ctx.ellipse(0, 0, radius, ry, 0, 0, TAU);
    ctx.stroke();

    const a0 = clock * params.orbitSpeed * (1 + i * 0.18) + i * 1.7;
    ctx.strokeStyle = rgba(i % 2 ? params.accent2 : params.accent, 0.6 * ambient * open);
    ctx.lineWidth = 1.6;
    ctx.shadowBlur = 12;
    ctx.shadowColor = rgba(params.accent, 0.5);
    ctx.beginPath();
    ctx.ellipse(0, 0, radius, ry, 0, a0, a0 + TAU * params.arc * 0.18);
    ctx.stroke();
    ctx.shadowBlur = 0;

    const dots = Math.round(params.orbitDots);
    for (let j = 0; j < dots; j++) {
      const angle = a0 * 0.6 + (j * TAU) / dots + i * 0.9;
      const x = Math.cos(angle) * radius;
      const y = Math.sin(angle) * ry;
      const tx = -Math.sin(angle) * radius;
      const ty = Math.cos(angle) * ry;
      const tangent = Math.hypot(tx, ty) || 1;
      const len = 10 + 16 * params.orbitSpeed;
      ctx.strokeStyle = rgba(j % 3 === 0 ? params.accent2 : params.accent, 0.46 * ambient * open);
      ctx.lineWidth = 1.2;
      ctx.beginPath();
      ctx.moveTo(x - (tx / tangent) * len, y - (ty / tangent) * len);
      ctx.lineTo(x, y);
      ctx.stroke();
      ctx.fillStyle = rgba(params.accent, 0.95 * ambient * open);
      ctx.beginPath();
      ctx.arc(x, y, 1.5, 0, TAU);
      ctx.fill();
    }
  }
  ctx.restore();
}

function drawCore(ctx: CanvasRenderingContext2D, opts: FrameOptions): void {
  const { width: w, height: h, clock, alpha, params, timeline, t } = opts;
  const cx = w / 2;
  const cy = h / 2;
  const open = easeOut(segmentProgress('open', t, timeline));
  const reveal = easeSoft(segmentProgress('in', t, timeline));
  const coreVis = open * (1 - 0.55 * reveal) * alpha;
  const pu = segmentProgress('pulse', t, timeline);
  const pulse =
    pu > 0 && pu < 1 ? Math.exp(-Math.pow((pu - 0.5) / 0.34, 2)) * alpha : 0;
  const breath = 1 + 0.022 * params.breath * Math.sin(clock * 1.7);
  const radius = params.coreRadius * breath;
  const glow = params.glow * (coreVis * 0.55 + pulse * 0.9);

  if (glow > 0.002) {
    const gradient = ctx.createRadialGradient(cx, cy, 0, cx, cy, radius * (2.6 + pulse * 1.2));
    gradient.addColorStop(0, rgba(params.accent, 0.22 * glow));
    gradient.addColorStop(0.32, rgba(params.accent, 0.1 * glow));
    gradient.addColorStop(0.7, rgba(params.accent2, 0.022 * glow));
    gradient.addColorStop(1, rgba(params.accent2, 0));
    ctx.fillStyle = gradient;
    ctx.fillRect(cx - radius * 4, cy - radius * 4, radius * 8, radius * 8);
  }
  if (coreVis < 0.01) return;

  ctx.save();
  ctx.translate(cx, cy);
  ctx.lineWidth = 1;
  ctx.strokeStyle = rgba(params.accent, 0.55 * coreVis);
  ctx.beginPath();
  ctx.arc(0, 0, radius, 0, TAU);
  ctx.stroke();
  ctx.strokeStyle = rgba(params.accent2, 0.22 * coreVis);
  ctx.beginPath();
  ctx.arc(0, 0, radius * 1.28, 0, TAU);
  ctx.stroke();

  ctx.rotate(clock * 0.12);
  for (let i = 0; i < 36; i++) {
    const angle = (i * TAU) / 36;
    const long = i % 6 === 0;
    const r0 = radius * 1.12;
    const r1 = radius * (long ? 1.24 : 1.18);
    ctx.strokeStyle = rgba(params.accent, (long ? 0.42 : 0.18) * coreVis);
    ctx.beginPath();
    ctx.moveTo(Math.cos(angle) * r0, Math.sin(angle) * r0);
    ctx.lineTo(Math.cos(angle) * r1, Math.sin(angle) * r1);
    ctx.stroke();
  }
  ctx.rotate(-clock * 0.12);

  const q = radius * 0.16 * (1 + pulse * 0.5);
  ctx.strokeStyle = rgba('#ffffff', 0.7 * coreVis);
  ctx.beginPath();
  ctx.moveTo(0, -q);
  ctx.lineTo(q, 0);
  ctx.lineTo(0, q);
  ctx.lineTo(-q, 0);
  ctx.closePath();
  ctx.stroke();
  ctx.restore();

  if (params.ripple > 0 && pu > 0 && pu < 1) {
    const rr = radius + easeOut(pu) * params.ripple;
    ctx.lineWidth = 1.4;
    ctx.strokeStyle = rgba(params.accent, 0.42 * (1 - pu) * coreVis);
    ctx.beginPath();
    ctx.arc(cx, cy, rr, 0, TAU);
    ctx.stroke();
  }
}

function drawParticles(ctx: CanvasRenderingContext2D, opts: FrameOptions): void {
  const { width: w, height: h, alpha, params, timeline, t, particles } = opts;
  const uf = segmentProgress('flow', t, timeline);
  if (uf <= 0 || uf >= 1) return;
  const fadeIn = clamp01(uf / 0.12);
  const fadeOut = 1 - clamp01((uf - 0.8) / 0.2);
  const win = easeSoft(fadeIn) * fadeOut * alpha;
  if (win < 0.005) return;
  const cx = w / 2;
  const cy = h / 2;
  ctx.save();
  ctx.globalCompositeOperation = 'lighter';
  for (const particle of particles) {
    const delay = (Math.min(0.95, params.stagger) * particle.delay) / Math.max(0.001, params.stagger);
    const local = clamp01((uf - delay) / (1 - delay));
    if (local <= 0 || local >= 1) continue;
    const e = easeInOut(local) * particle.speed;
    if (e >= 1) continue;
    const [x, y] = particlePos(particle, e, w, h, cx, cy, params);
    const [tx, ty] = particlePos(particle, Math.max(0, e - params.tail), w, h, cx, cy, params);
    const color = particle.alt ? params.accent2 : params.accent;
    const a = 0.78 * win * particle.bright * (1 - 0.3 * local);
    ctx.strokeStyle = rgba(color, a);
    ctx.lineWidth = particle.size * (0.7 + 0.5 * e);
    ctx.beginPath();
    ctx.moveTo(tx, ty);
    ctx.lineTo(x, y);
    ctx.stroke();
    ctx.fillStyle = rgba(color, a * 0.95);
    ctx.beginPath();
    ctx.arc(x, y, Math.max(0.6, particle.size * 0.5), 0, TAU);
    ctx.fill();
  }
  ctx.restore();
}

/** 绘制一帧：全部视觉由 t 解析式决定，无累积状态。 */
export function drawTransitionFrame(ctx: CanvasRenderingContext2D, opts: FrameOptions): void {
  const { width: w, height: h, alpha, params } = opts;
  ctx.clearRect(0, 0, w, h);
  if (alpha <= 0.002) return;

  // 深空暗幕：中段压暗主体，两端完全透明
  ctx.fillStyle = rgba(params.bg, 0.92 * alpha);
  ctx.fillRect(0, 0, w, h);
  const cx = w / 2;
  const cy = h / 2;
  const halo = ctx.createRadialGradient(cx, cy, 0, cx, cy, Math.max(w, h) * 0.62);
  halo.addColorStop(0, rgba(params.accent, 0.1 * alpha));
  halo.addColorStop(1, rgba(params.accent, 0));
  ctx.fillStyle = halo;
  ctx.fillRect(0, 0, w, h);

  drawStars(ctx, opts);
  drawOrbits(ctx, opts);
  drawCore(ctx, opts);
  drawParticles(ctx, opts);
}
