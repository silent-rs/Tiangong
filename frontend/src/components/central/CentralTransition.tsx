/**
 * 中枢模式两阶段切换过渡（设计稿 §4.1）。
 *
 * 阶段一（title-scan，2s）：顶栏激光从左向右扫描对话标题完成顶部切换
 * （title 双层按扫描进度分割显隐，StatusPanel 内实现），本层同步在顶栏
 * 播放两遍横向光带强化「顶部被切换」的仪式感；主区域维持原视图。
 *
 * 阶段二（shatter，进入 ~4.3s / 退出 ~1s）：Canvas 粒子系统——
 * 进入：粒子自普通视图的网格位置爆散（打碎），收束进倾斜的银河盘面
 * 轮转 2 圈（旋臂 + 内快外慢较差自转 + 中心亮核），再沿缓动曲线飞向
 * 中枢 UI 的象征轮廓重组，尾声淡出露出真实 CentralModeView；
 * 退出：粒子快速爆散消散，普通视图即时回归。
 *
 * 层为 pointer-events-none 的 fixed 覆盖；各阶段时长到期回调 onDone。
 * jsdom 等无 2D 上下文环境下粒子层退化为纯计时段，动画照常结束。
 */
import { useEffect, useRef } from 'react';

/** 阶段一：激光扫描 title 并维持的时长（ms）。 */
export const CENTRAL_TITLE_SCAN_MS = 2000;

/** 阶段二粒子时长（ms）。 */
export const CENTRAL_PARTICLE_MS = {
  /** 进入：打碎 0.8s + 银河 2 圈 2.0s + 重组 1.2s（首尾重叠）+ 尾淡出 */
  entering: 4300,
  /** 退出：快速爆散消散 */
  exiting: 1000,
} as const;

export type CentralTransitionPhase = 'title-scan' | 'shatter';

interface CentralTransitionProps {
  phase: CentralTransitionPhase;
  /** true = 进入中枢（银河重组）；false = 退出（快速消散）。 */
  entering: boolean;
  /** 当前阶段完成（时长到期）后回调，调用方据此推进或结束过渡。 */
  onDone: () => void;
}

// ── 粒子系统常量 ─────────────────────────────────────────────────────────────

interface Particle {
  /** 打碎起点（普通视图象征网格） */
  ox: number;
  oy: number;
  /** 重组终点（中枢 UI 象征轮廓） */
  tx: number;
  ty: number;
  /** 银河轨道 */
  radius: number;
  angle: number;
  /** 较差自转角速度（rad/ms），内圈快外圈慢 */
  spin: number;
  /** 爆散速度 */
  vx: number;
  vy: number;
  size: number;
  /** 色板索引 */
  color: string;
  /** 闪烁相位 */
  twinkle: number;
}

const PALETTE = [
  'rgba(224,242,254,', // 星白
  'rgba(34,211,238,', // 青
  'rgba(167,139,250,', // 紫
  'rgba(125,211,252,', // 浅蓝
  'rgba(251,191,36,', // 金（少量点缀）
];

const PARTICLE_COUNT = 2600;
const GALAXY_TILT = 0.36; // 银河盘面纵向压扁比（透视倾角）
const ARMS = 3; // 旋臂数
const ARM_WIND = 2.6; // 旋臂缠绕系数

/** 进入时序（ms）：爆散 → 进入轨道（重叠）→ 轮转 2 圈 → 重组 → 尾淡出。 */
const T_BURST = 800;
const T_ORBIT_IN = 500; // 与 BURST 末段重叠：800→1300
const T_SPIN = 2000; // 外圈整 2 圈
const T_ASSEMBLE = 1200;
const T_FADE = 300;

function easeInOut(t: number): number {
  return t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
}

function easeOutCubic(t: number): number {
  return 1 - Math.pow(1 - t, 3);
}

/** 银河旋臂初始化：对数螺旋 + 抖动，内密外疏。 */
function galaxyInit(p: Particle, cx: number, cy: number, maxR: number, index: number) {
  const radius = maxR * (0.06 + 0.94 * Math.pow(Math.random(), 0.62));
  const arm = index % ARMS;
  const armAngle = (arm / ARMS) * Math.PI * 2;
  const wind = radius / maxR * ARM_WIND * Math.PI * 2;
  p.radius = radius;
  p.angle = armAngle + wind + (Math.random() - 0.5) * (0.5 + 0.9 * (radius / maxR));
  // 较差自转：外圈 2 圈 / T_SPIN，内圈提速至 ~2.6 倍
  p.spin = ((Math.PI * 2 * 2) / T_SPIN) * (1 + 1.6 * (1 - radius / maxR));
  void cx;
  void cy;
}

/** 粒子层：全屏 Canvas，动画结束回调。 */
function ParticleField({
  entering,
  duration,
  onDone,
}: {
  entering: boolean;
  duration: number;
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

    const headerH = 48;
    const areaTop = headerH;
    const areaH = Math.max(160, height - headerH);
    const cx = width / 2;
    const cy = areaTop + areaH * 0.46;
    const maxR = Math.min(width * 0.44, areaH * 0.52);

    // 中枢 UI 象征轮廓（重组目标）：中央问候卡片 + 两条文字条 + 底部输入条
    const cardW = Math.min(560, width * 0.62);
    const cardH = 168;
    const cardX = cx - cardW / 2;
    const cardY = cy - cardH / 2 - 24;
    const lineW = cardW * 0.62;
    const targets: Array<() => { x: number; y: number }> = [];
    const pushRectOutline = (x: number, y: number, w: number, h: number, weight: number) => {
      for (let i = 0; i < weight; i++) {
        const edge = Math.floor(Math.random() * 4);
        const t = Math.random();
        if (edge === 0) targets.push(() => ({ x: x + w * t, y }));
        else if (edge === 1) targets.push(() => ({ x: x + w * t, y: y + h }));
        else if (edge === 2) targets.push(() => ({ x, y: y + h * t }));
        else targets.push(() => ({ x: x + w, y: y + h * t }));
      }
    };
    const pushRectFill = (x: number, y: number, w: number, h: number, weight: number) => {
      for (let i = 0; i < weight; i++) {
        const fx = Math.random();
        const fy = Math.random();
        targets.push(() => ({ x: x + w * fx, y: y + h * fy }));
      }
    };
    pushRectOutline(cardX, cardY, cardW, cardH, Math.floor(PARTICLE_COUNT * 0.32));
    pushRectFill(cx - lineW / 2, cardY + 34, lineW, 14, Math.floor(PARTICLE_COUNT * 0.1));
    pushRectFill(cx - lineW * 0.38, cardY + 62, lineW * 0.76, 14, Math.floor(PARTICLE_COUNT * 0.08));
    pushRectOutline(cx - lineW / 2, cardY + cardH + 40, lineW, 44, Math.floor(PARTICLE_COUNT * 0.16));

    const particles: Particle[] = [];
    for (let i = 0; i < PARTICLE_COUNT; i++) {
      // 打碎起点：主区域均匀网格（普通视图的象征）
      const gx = Math.random() * width;
      const gy = areaTop + Math.random() * areaH;
      const target = targets[i % targets.length]();
      const p: Particle = {
        ox: gx,
        oy: gy,
        tx: target.x,
        ty: target.y,
        radius: 0,
        angle: 0,
        spin: 0,
        vx: (Math.random() - 0.5) * 1.6,
        vy: (Math.random() - 0.5) * 1.6,
        size: 0.8 + Math.random() * 2.1,
        color: PALETTE[
          Math.random() < 0.06 ? 4 : Math.floor(Math.random() * 4)
        ],
        twinkle: Math.random() * Math.PI * 2,
      };
      galaxyInit(p, cx, cy, maxR, i);
      particles.push(p);
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
      ctx.globalCompositeOperation = 'lighter';

      // 背景遮暗程度：打碎/银河期几乎全遮，重组尾段回落露出真实 UI
      let bgAlpha: number;
      if (!entering) {
        bgAlpha = 0.45 * (1 - easeInOut(t / duration));
      } else if (t < T_BURST + T_ORBIT_IN) {
        bgAlpha = 0.35 + 0.5 * easeInOut(t / (T_BURST + T_ORBIT_IN));
      } else if (t < T_BURST + T_ORBIT_IN + T_SPIN) {
        bgAlpha = 0.88;
      } else {
        const at = (t - T_BURST - T_ORBIT_IN - T_SPIN) / T_ASSEMBLE;
        bgAlpha = 0.88 * (1 - easeInOut(Math.min(1, at * 1.25)));
      }
      ctx.globalCompositeOperation = 'source-over';
      ctx.fillStyle = `rgba(5,7,13,${bgAlpha})`;
      ctx.fillRect(0, 0, width, height);
      ctx.globalCompositeOperation = 'lighter';

      // 全局粒子透明度（尾淡出）
      let globalAlpha = 1;
      if (t > duration - T_FADE) {
        globalAlpha = 1 - (t - (duration - T_FADE)) / T_FADE;
      }

      if (entering) {
        const spinStart = T_BURST + T_ORBIT_IN;
        const spinEnd = spinStart + T_SPIN;
        const asmStart = spinEnd;
        for (const p of particles) {
          let x: number;
          let y: number;
          let alpha = globalAlpha;
          if (t < T_BURST) {
            // 打碎：网格位置爆散
            const bt = t / T_BURST;
            const drift = easeOutCubic(bt) * 190;
            x = p.ox + p.vx * drift;
            y = p.oy + p.vy * drift;
          } else if (t < asmStart) {
            // 进入轨道（BURST 末段→ORBIT_IN 完成）后较差自转
            let gx: number;
            let gy: number;
            if (t < spinStart) {
              const ot = easeInOut((t - T_BURST) / T_ORBIT_IN);
              const bx = p.ox + p.vx * 190;
              const by = p.oy + p.vy * 190;
              const a0 = p.angle;
              gx = cx + Math.cos(a0) * p.radius;
              gy = cy + Math.sin(a0) * p.radius * GALAXY_TILT;
              gx = bx + (gx - bx) * ot;
              gy = by + (gy - by) * ot;
              p.angle = a0;
            } else {
              p.angle += p.spin * 16.7;
              gx = cx + Math.cos(p.angle) * p.radius;
              gy = cy + Math.sin(p.angle) * p.radius * GALAXY_TILT;
            }
            x = gx;
            y = gy;
            // 银河期闪烁
            p.twinkle += 0.18;
            alpha *= 0.75 + 0.25 * Math.sin(p.twinkle);
          } else {
            // 重组：轨道位置 → 中枢 UI 轮廓（缓动飞行）
            const at = Math.min(1, (t - asmStart) / T_ASSEMBLE);
            const e = easeInOut(at);
            const sx = cx + Math.cos(p.angle) * p.radius;
            const sy = cy + Math.sin(p.angle) * p.radius * GALAXY_TILT;
            // 弧线：先向外抬一点再落向目标，避免直线的机械感
            const mx = (sx + p.tx) / 2 + (p.ty - sy) * 0.12;
            const my = (sy + p.ty) / 2 - Math.abs(p.tx - sx) * 0.06;
            const u = 1 - e;
            x = u * u * sx + 2 * u * e * mx + e * e * p.tx;
            y = u * u * sy + 2 * u * e * my + e * e * p.ty;
            if (at > 0.86) alpha *= 1 - (at - 0.86) / 0.14; // 到位即隐入真实 UI
          }
          const a = Math.max(0, Math.min(1, alpha));
          if (a <= 0.01) continue;
          ctx.fillStyle = `${p.color}${(0.85 * a).toFixed(3)})`;
          ctx.fillRect(x, y, p.size, p.size);
        }
        // 中心亮核（银河期最强）
        if (t >= T_BURST && t < asmStart + T_ASSEMBLE * 0.5) {
          const coreT = Math.min(1, Math.max(0, (t - T_BURST) / T_ORBIT_IN));
          drawGalaxyCore(coreT * (t > asmStart ? 1 - (t - asmStart) / (T_ASSEMBLE * 0.5) : 1) * globalAlpha);
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
          ctx.fillStyle = `${p.color}${(0.8 * a).toFixed(3)})`;
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
  }, [entering, duration, onDone]);

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
  // title-scan 阶段：等待层 + 顶栏横向光带（title 激光在 StatusPanel 内播放）
  useEffect(() => {
    if (phase !== 'title-scan') return;
    const timer = window.setTimeout(onDone, CENTRAL_TITLE_SCAN_MS);
    return () => window.clearTimeout(timer);
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

  // shatter 阶段：粒子层
  return (
    <ParticleField
      entering={entering}
      duration={entering ? CENTRAL_PARTICLE_MS.entering : CENTRAL_PARTICLE_MS.exiting}
      onDone={onDone}
    />
  );
}
