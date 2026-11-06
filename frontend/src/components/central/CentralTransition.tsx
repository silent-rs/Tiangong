/**
 * 中枢模式两阶段切换过渡（设计稿 §4.1）。
 *
 * 阶段一（title-scan，2s）：顶栏激光从左向右扫描对话标题完成顶部切换
 * （新旧标题按扫描进度双向裁剪、扫过即替换，StatusPanel 内实现），本层
 * 同步在顶栏播放两遍横向光带；主区域维持原视图。此阶段并行预热截图：
 * html2canvas 抓取主区域，按像素采样生成粒子种子——阶段二的「碎裂」
 * 粒子即来自原界面真实内容（文字/图形），截图失败回退象征网格。
 *
 * 阶段二（shatter）：
 * 进入（~4.2s）：界面像素粒子爆散（打碎）→ 收束进倾斜银河盘面慢速
 * 轮转 1 圈（旋臂 + 内快外慢较差自转 + 中心亮核）→ 粒子逐个飞入
 * 「中枢」二字的文字点阵，逐个落位拼出文字（非整体淡入淡出）→
 * 拼成后驻留呼吸，整层淡出露出真实 CentralModeView。
 * 退出（~1s）：当前中枢视图截图采样后快速爆散消散，普通视图即时回归。
 *
 * 层为 pointer-events-none 的 fixed 覆盖。jsdom 等无 2D 上下文环境下
 * 粒子层退化为纯计时段，动画照常结束（测试可运行）。
 */
import { useEffect, useRef } from 'react';
import html2canvas from 'html2canvas';

/** 阶段一：激光扫描 title 并维持的时长（ms）。 */
export const CENTRAL_TITLE_SCAN_MS = 2000;

/** 阶段二粒子时长（ms）。 */
export const CENTRAL_PARTICLE_MS = {
  /** 进入：打碎 0.7s + 银河 1 圈 2.0s + 拼字 1.2s + 驻留 0.5s + 淡出 0.5s（首尾重叠） */
  entering: 4200,
  /** 退出：快速爆散消散 */
  exiting: 1000,
} as const;

export type CentralTransitionPhase = 'title-scan' | 'shatter';

interface CentralTransitionProps {
  phase: CentralTransitionPhase;
  /** true = 进入中枢（银河 + 拼字）；false = 退出（快速消散）。 */
  entering: boolean;
  /** 当前阶段完成（时长到期）后回调，调用方据此推进或结束过渡。 */
  onDone: () => void;
}

// ── 粒子模型 ─────────────────────────────────────────────────────────────────

interface Particle {
  /** 起点（原界面像素位置或象征网格） */
  ox: number;
  oy: number;
  /** 终点（「中枢」文字点阵位置） */
  tx: number;
  ty: number;
  /** 原色（截图像素），银河期向星系色混合、拼字期回归 */
  r: number;
  g: number;
  b: number;
  /** 银河轨道 */
  radius: number;
  angle: number;
  /** 较差自转角速度（rad/ms），内圈快外圈慢 */
  spin: number;
  /** 爆散速度 */
  vx: number;
  vy: number;
  size: number;
  /** 闪烁相位 */
  twinkle: number;
  /** 拼字延迟（错峰落位，逐个拼成） */
  delay: number;
}

/** 截图采样的粒子种子（原界面真实内容像素）。 */
interface Seed {
  x: number;
  y: number;
  r: number;
  g: number;
  b: number;
}

const GALAXY_TILT = 0.36; // 银河盘面纵向压扁比（透视倾角）
const ARMS = 3; // 旋臂数
const ARM_WIND = 2.6; // 旋臂缠绕系数
const MAX_PARTICLES = 2600;

/** 进入时序（ms）。 */
const T_BURST = 700;
const T_ORBIT_IN = 450;
const T_SPIN = 2000; // 外圈整 1 圈（慢速）
const T_ASSEMBLE = 1200;
const T_HOLD = 500;
const T_FADE = 500;

/** 星系色板（银河期与原色混合用）。 */
const STAR_COLORS: Array<[number, number, number]> = [
  [224, 242, 254], // 星白
  [34, 211, 238], // 青
  [167, 139, 250], // 紫
  [125, 211, 252], // 浅蓝
];

function easeInOut(t: number): number {
  return t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
}

function easeOutCubic(t: number): number {
  return 1 - Math.pow(1 - t, 3);
}

/**
 * 截图并采样主区域：html2canvas 渲染 → 逐像素步进采样（alpha > 0），
 * 随机抽取至目标数量。失败（不支持/跨域污染/超时）返回 null。
 */
async function captureSeeds(element: HTMLElement, target: number): Promise<Seed[] | null> {
  try {
    const canvas = await html2canvas(element, {
      logging: false,
      useCORS: true,
      backgroundColor: null,
      scale: Math.min(window.devicePixelRatio || 1, 1.5),
    });
    const ctx = canvas.getContext('2d');
    if (!ctx) return null;
    const width = canvas.width;
    const height = canvas.height;
    const data = ctx.getImageData(0, 0, width, height).data;
    // 元素在视口中的位置（粒子起点用视口坐标）
    const rect = element.getBoundingClientRect();
    const scaleX = rect.width / width;
    const scaleY = rect.height / height;
    const step = 3;
    const candidates: Seed[] = [];
    for (let y = 0; y < height; y += step) {
      for (let x = 0; x < width; x += step) {
        const i = (y * width + x) * 4;
        const alpha = data[i + 3];
        if (alpha < 40) continue;
        candidates.push({
          x: rect.left + x * scaleX,
          y: rect.top + y * scaleY,
          r: data[i],
          g: data[i + 1],
          b: data[i + 2],
        });
      }
    }
    if (candidates.length === 0) return null;
    // 均匀抽取至目标数
    if (candidates.length <= target) return candidates;
    const ratio = candidates.length / target;
    const seeds: Seed[] = [];
    for (let i = 0; i < target; i++) {
      seeds.push(candidates[Math.floor(i * ratio + Math.random() * ratio)]);
    }
    return seeds;
  } catch {
    return null;
  }
}

/** 离屏绘制「中枢」二字并采样文字点阵（拼字目标）。 */
function textGlyphPoints(
  text: string,
  fontSize: number,
  target: number,
): Array<{ x: number; y: number }> {
  const canvas = document.createElement('canvas');
  const font = `600 ${fontSize}px system-ui, -apple-system, 'PingFang SC', 'Microsoft YaHei', sans-serif`;
  const measure = canvas.getContext('2d');
  const textWidth = measure ? (measure.font = font, measure.measureText(text).width) : fontSize * text.length;
  const w = Math.ceil(textWidth + 40);
  const h = Math.ceil(fontSize * 1.5);
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext('2d');
  if (!ctx) return [];
  ctx.font = font;
  ctx.fillStyle = '#fff';
  ctx.textBaseline = 'middle';
  ctx.textAlign = 'center';
  ctx.fillText(text, w / 2, h / 2);
  const data = ctx.getImageData(0, 0, w, h).data;
  const candidates: Array<{ x: number; y: number }> = [];
  const step = 2;
  for (let y = 0; y < h; y += step) {
    for (let x = 0; x < w; x += step) {
      if (data[(y * w + x) * 4 + 3] > 96) {
        candidates.push({ x: x - w / 2, y: y - h / 2 }); // 以文字中心为原点
      }
    }
  }
  if (candidates.length === 0) return [];
  if (candidates.length <= target) return candidates;
  const ratio = candidates.length / target;
  const points: Array<{ x: number; y: number }> = [];
  for (let i = 0; i < target; i++) {
    points.push(candidates[Math.floor(i * ratio)]);
  }
  return points;
}

/** 粒子层：全屏 Canvas，动画结束回调。seeds 为原界面采样（可为 null 走象征网格）。 */
function ParticleField({
  entering,
  duration,
  seeds,
  onDone,
}: {
  entering: boolean;
  duration: number;
  seeds: Seed[] | null;
  onDone: () => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const doneRef = useRef(false);

  useEffect(() => {
    doneRef.current = false;
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext('2d') ?? null;

    // jsdom / 无 2D 上下文：退化为纯计时段（测试与极简环境可运行）
    if (!canvas || !ctx) {
      const timer = window.setTimeout(onDone, duration);
      return () => window.clearTimeout(timer);
    }

    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const width = window.innerWidth;
    const height = window.innerHeight;
    canvas.width = width * dpr;
    canvas.height = height * dpr;
    canvas.style.width = `${width}px`;
    canvas.style.height = `${height}px`;
    ctx.scale(dpr, dpr);

    const cx = width / 2;
    const cy = height * 0.46;
    const maxR = Math.min(width * 0.42, height * 0.5);

    // 拼字目标：「中枢」文字点阵（相对中心的偏移）
    const glyphFontSize = Math.min(width * 0.2, 190);
    const glyph = entering
      ? textGlyphPoints('中枢', glyphFontSize, Math.floor(MAX_PARTICLES * 0.86))
      : [];

    // 无截图种子时的回退：主区域象征网格
    const fallbackSeeds = (): Seed[] => {
      const seeds: Seed[] = [];
      for (let i = 0; i < MAX_PARTICLES; i++) {
        seeds.push({
          x: Math.random() * width,
          y: height * 0.12 + Math.random() * height * 0.85,
          r: 148,
          g: 163,
          b: 184,
        });
      }
      return seeds;
    };
    const source = seeds && seeds.length > 0 ? seeds : fallbackSeeds();

    const particles: Particle[] = [];
    for (let i = 0; i < MAX_PARTICLES; i++) {
      const seed = source[i % source.length];
      // 拼字落点：按索引错峰（从左到右逐列拼成），delay 0..0.42
      const glyphPoint = glyph.length > 0 ? glyph[i % glyph.length] : { x: 0, y: 0 };
      const columnOrder = glyphFontSize > 0 ? (glyphPoint.x + glyphFontSize) / (glyphFontSize * 2.4) : Math.random();
      particles.push({
        ox: seed.x,
        oy: seed.y,
        tx: cx + glyphPoint.x,
        ty: cy + glyphPoint.y,
        r: seed.r,
        g: seed.g,
        b: seed.b,
        radius: maxR * (0.06 + 0.94 * Math.pow(Math.random(), 0.62)),
        angle: (() => {
          const arm = i % ARMS;
          const wind = 0;
          void wind;
          return (
            (arm / ARMS) * Math.PI * 2 +
            (particles.length / MAX_PARTICLES) * 0 +
            Math.random() * Math.PI * 2
          );
        })(),
        spin: 0,
        vx: (Math.random() - 0.5) * 1.7,
        vy: (Math.random() - 0.5) * 1.7,
        size: 0.8 + Math.random() * 2.0,
        twinkle: Math.random() * Math.PI * 2,
        delay: entering ? Math.min(0.42, Math.max(0, columnOrder * 0.42 + Math.random() * 0.06)) : 0,
      });
    }
    // 银河初始化：旋臂 + 较差自转（外圈 1 圈 / T_SPIN，内圈提速至 ~2.2 倍）
    for (let i = 0; i < particles.length; i++) {
      const p = particles[i];
      const arm = i % ARMS;
      const armAngle = (arm / ARMS) * Math.PI * 2;
      const wind = (p.radius / maxR) * ARM_WIND * Math.PI * 2;
      p.angle = armAngle + wind + (Math.random() - 0.5) * (0.5 + 0.9 * (p.radius / maxR));
      p.spin = ((Math.PI * 2) / T_SPIN) * (1 + 1.2 * (1 - p.radius / maxR));
      // 星系色（按轨道分配）
      const star = STAR_COLORS[i % STAR_COLORS.length];
      p.r = Math.round(p.r * 0.45 + star[0] * 0.55);
      p.g = Math.round(p.g * 0.45 + star[1] * 0.55);
      p.b = Math.round(p.b * 0.45 + star[2] * 0.55);
    }

    const start = performance.now();
    let raf = 0;

    const drawGalaxyCore = (alpha: number) => {
      const coreR = maxR * 0.24;
      const g = ctx.createRadialGradient(cx, cy, 0, cx, cy, coreR);
      g.addColorStop(0, `rgba(224,242,254,${0.5 * alpha})`);
      g.addColorStop(0.35, `rgba(34,211,238,${0.2 * alpha})`);
      g.addColorStop(1, 'rgba(34,211,238,0)');
      ctx.fillStyle = g;
      ctx.beginPath();
      ctx.ellipse(cx, cy, coreR, coreR * GALAXY_TILT, 0, 0, Math.PI * 2);
      ctx.fill();
    };

    const frame = (now: number) => {
      const t = now - start;
      if (t >= duration) {
        if (!doneRef.current) {
          doneRef.current = true;
          onDone();
        }
        return;
      }

      ctx.clearRect(0, 0, width, height);

      // 背景遮暗程度：打碎/银河期几乎全遮，拼字驻留后整体淡出
      let bgAlpha: number;
      if (!entering) {
        bgAlpha = 0.45 * (1 - easeInOut(t / duration));
      } else if (t < T_BURST + T_ORBIT_IN) {
        bgAlpha = 0.35 + 0.5 * easeInOut(t / (T_BURST + T_ORBIT_IN));
      } else if (t < T_BURST + T_ORBIT_IN + T_SPIN) {
        bgAlpha = 0.9;
      } else {
        const after = t - T_BURST - T_ORBIT_IN - T_SPIN;
        const span = T_ASSEMBLE + T_HOLD;
        bgAlpha = after < span ? 0.9 * (1 - easeInOut(Math.min(1, after / span)) * 0.85) : 0.08;
      }
      ctx.globalCompositeOperation = 'source-over';
      ctx.fillStyle = `rgba(5,7,13,${Math.max(0, Math.min(1, bgAlpha))})`;
      ctx.fillRect(0, 0, width, height);

      // 尾段整层淡出
      let globalAlpha = 1;
      if (t > duration - T_FADE) {
        globalAlpha = Math.max(0, 1 - (t - (duration - T_FADE)) / T_FADE);
      }

      ctx.globalCompositeOperation = 'lighter';

      if (entering) {
        const spinStart = T_BURST + T_ORBIT_IN;
        const spinEnd = spinStart + T_SPIN;
        const asmStart = spinEnd;
        const holdEnd = asmStart + T_ASSEMBLE + T_HOLD;
        for (const p of particles) {
          let x: number;
          let y: number;
          let alpha = globalAlpha;
          if (t < T_BURST) {
            // 打碎：原界面像素位置爆散
            const bt = t / T_BURST;
            const drift = easeOutCubic(bt) * 180;
            x = p.ox + p.vx * drift;
            y = p.oy + p.vy * drift;
          } else if (t < asmStart) {
            // 进入轨道（BURST 末段→ORBIT_IN 完成平滑衔接）后较差自转
            let gx: number;
            let gy: number;
            if (t < spinStart) {
              const ot = easeInOut((t - T_BURST) / T_ORBIT_IN);
              const bx = p.ox + p.vx * 180;
              const by = p.oy + p.vy * 180;
              gx = cx + Math.cos(p.angle) * p.radius;
              gy = cy + Math.sin(p.angle) * p.radius * GALAXY_TILT;
              gx = bx + (gx - bx) * ot;
              gy = by + (gy - by) * ot;
            } else {
              p.angle += p.spin * 16.7;
              gx = cx + Math.cos(p.angle) * p.radius;
              gy = cy + Math.sin(p.angle) * p.radius * GALAXY_TILT;
            }
            x = gx;
            y = gy;
            p.twinkle += 0.14;
            alpha *= 0.72 + 0.28 * Math.sin(p.twinkle);
          } else if (t < holdEnd) {
            // 拼字：错峰出发（delay 归一化），二次贝塞尔飞向文字点阵
            const at = Math.min(
              1,
              Math.max(0, (t - asmStart) / T_ASSEMBLE - p.delay) / (1 - 0.42),
            );
            if (at <= 0) {
              // 未出发：仍在轨道
              p.angle += p.spin * 16.7;
              x = cx + Math.cos(p.angle) * p.radius;
              y = cy + Math.sin(p.angle) * p.radius * GALAXY_TILT;
              p.twinkle += 0.14;
              alpha *= 0.72 + 0.28 * Math.sin(p.twinkle);
            } else {
              const e = easeInOut(at);
              const sx = cx + Math.cos(p.angle) * p.radius;
              const sy = cy + Math.sin(p.angle) * p.radius * GALAXY_TILT;
              const mx = (sx + p.tx) / 2 + (p.ty - sy) * 0.1;
              const my = (sy + p.ty) / 2 - Math.abs(p.tx - sx) * 0.05;
              const u = 1 - e;
              x = u * u * sx + 2 * u * e * mx + e * e * p.tx;
              y = u * u * sy + 2 * u * e * my + e * e * p.ty;
              if (at >= 1) {
                // 已落位：拼成的文字粒子轻微呼吸，不再消失
                x = p.tx;
                y = p.ty;
                p.twinkle += 0.08;
                alpha *= 0.9 + 0.1 * Math.sin(p.twinkle);
              }
            }
          } else {
            // 驻留结束：保持文字点阵，随整层淡出
            x = p.tx;
            y = p.ty;
            p.twinkle += 0.08;
            alpha *= 0.9 + 0.1 * Math.sin(p.twinkle);
          }
          const a = Math.max(0, Math.min(1, alpha));
          if (a <= 0.01) continue;
          ctx.fillStyle = `rgba(${p.r},${p.g},${p.b},${(0.9 * a).toFixed(3)})`;
          ctx.fillRect(x, y, p.size, p.size);
        }
        // 中心亮核（银河期最强，拼字期消退）
        if (t >= T_BURST && t < asmStart + T_ASSEMBLE) {
          const coreT = Math.min(1, Math.max(0, (t - T_BURST) / T_ORBIT_IN));
          const decay = t > asmStart ? 1 - (t - asmStart) / T_ASSEMBLE : 1;
          drawGalaxyCore(coreT * decay * globalAlpha);
        }
      } else {
        // 退出：快速爆散消散
        const et = t / duration;
        const drift = easeOutCubic(et) * 260;
        for (const p of particles) {
          const x = p.ox + p.vx * drift;
          const y = p.oy + p.vy * drift;
          const a = Math.max(0, (1 - et) * globalAlpha);
          if (a <= 0.01) continue;
          ctx.fillStyle = `rgba(${p.r},${p.g},${p.b},${(0.8 * a).toFixed(3)})`;
          ctx.fillRect(x, y, p.size, p.size);
        }
      }

      raf = requestAnimationFrame(frame);
    };

    raf = requestAnimationFrame(frame);
    return () => {
      cancelAnimationFrame(raf);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [entering, duration, seeds, onDone]);

  return (
    <canvas
      ref={canvasRef}
      data-testid="central-particle-field"
      aria-hidden
      className="pointer-events-none fixed inset-0 z-[100]"
    />
  );
}

export function CentralTransition({ phase, entering, onDone }: CentralTransitionProps) {
  // 阶段一并行预热：抓取当前主区域像素（普通视图尚在），供阶段二碎裂采样
  const seedsRef = useRef<Seed[] | null>(null);
  useEffect(() => {
    if (phase !== 'title-scan') return;
    seedsRef.current = null;
    let cancelled = false;
    const main = document.querySelector<HTMLElement>('main');
    if (main) {
      void captureSeeds(main, MAX_PARTICLES).then((seeds) => {
        if (!cancelled) seedsRef.current = seeds;
      });
    }
    const timer = window.setTimeout(onDone, CENTRAL_TITLE_SCAN_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [phase, onDone]);

  if (phase === 'title-scan') {
    return (
      <div data-testid="central-transition" aria-hidden className="pointer-events-none fixed inset-0 z-[100]">
        {/* 顶栏光带：随 title 激光同步扫过整个顶部两遍（2s 维持期） */}
        <div className="absolute left-0 right-0 top-0 h-12 overflow-hidden">
          <div
            className="absolute inset-y-0 w-[36%] animate-central-header-sweep"
            style={{
              background:
                'linear-gradient(90deg, transparent, rgba(34,211,238,.10) 20%, rgba(34,211,238,.34) 55%, rgba(167,139,250,.22) 80%, transparent)',
              boxShadow: '0 8px 32px rgba(34,211,238,.30)',
            }}
          />
        </div>
      </div>
    );
  }

  // shatter 阶段：粒子层。进入方向用阶段一预热的原界面截图采样；
  // 退出方向布局已切回普通视图（原中枢视图不在 DOM），用象征网格碎裂。
  if (entering) {
    return (
      <ParticleField
        entering
        duration={CENTRAL_PARTICLE_MS.entering}
        seeds={seedsRef.current}
        onDone={onDone}
      />
    );
  }
  return (
    <ParticleField
      entering={false}
      duration={CENTRAL_PARTICLE_MS.exiting}
      seeds={null}
      onDone={onDone}
    />
  );
}
