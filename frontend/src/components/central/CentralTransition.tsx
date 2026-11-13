/**
 * 中枢模式切换动画（仅 desktop 宿主）。
 *
 * 触发：监听 `centralActive`（已含 desktop 宿主守卫）的变化；首次挂载不播放，
 * 因此「重启直入中枢」不会闪动画。进入 = t 从 0 走到 T，退出 = 从 T 倒放回 0
 * （速度 × 1/exitRatio）；中途再次切换直接从当前 t 反向，保持连续。
 *
 * 实现约束：
 * - 布局切换仍由 React 立即完成（现有中枢模式行为与测试不变），动画只是一层
 *   pointer-events:none 的覆盖画布 + 转场期间的主体视觉变换；
 * - 主体变换通过 html 上的 CSS 变量驱动，转场结束即移除 class 与变量，
 *   避免长期 transform 影响内部 fixed 定位元素的包含块；
 * - 用 useLayoutEffect 在 React 提交新布局后、浏览器绘制前同步把主体压到透明，
 *   避免退出时「普通模式内容先清晰出现一帧」再开始动画；
 * - 尊重 prefers-reduced-motion；jsdom / 无 Canvas 环境静默降级为「无动画」，
 *   不影响任何功能与现有测试。
 */
import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { isDesktopHost } from '@/api/host';
import { useCentralMode } from '@/hooks/useCentralMode';
import {
  CENTRAL_TRANSITION_PARAMS,
  bodyReveal,
  buildTimeline,
} from './centralTransitionParams';
import {
  buildTransitionParticles,
  buildTransitionStars,
  drawTransitionFrame,
} from './centralTransitionRender';

const PARAMS = CENTRAL_TRANSITION_PARAMS;
const TIMELINE = buildTimeline(PARAMS);
const ACTIVE_CLASS = 'central-transition-active';

interface Motion {
  dir: 1 | -1;
  t: number;
  raf: number;
  last: number;
}

function schedule(callback: FrameRequestCallback): number {
  if (typeof window.requestAnimationFrame === 'function') {
    return window.requestAnimationFrame(callback);
  }
  return window.setTimeout(() => callback(performance.now()), 16);
}

function cancel(handle: number): void {
  if (typeof window.cancelAnimationFrame === 'function') {
    window.cancelAnimationFrame(handle);
  } else {
    window.clearTimeout(handle);
  }
}

function prefersReducedMotion(): boolean {
  return (
    typeof window.matchMedia === 'function' &&
    window.matchMedia('(prefers-reduced-motion: reduce)').matches
  );
}

function setBodyVars(reveal: number): void {
  const root = document.documentElement;
  // 新视图从完全透明渐显：React 已立即切换布局，不能让新内容先清晰露出一帧
  root.style.setProperty('--central-body-opacity', String(reveal));
  root.style.setProperty('--central-body-blur', `${(6 * (1 - reveal)).toFixed(2)}px`);
  root.style.setProperty('--central-body-shift', `${(6 * (1 - reveal)).toFixed(2)}px`);
  root.style.setProperty('--central-body-scale', (0.985 + 0.015 * reveal).toFixed(4));
}

function clearBodyVars(): void {
  const root = document.documentElement;
  root.classList.remove(ACTIVE_CLASS);
  root.style.removeProperty('--central-body-opacity');
  root.style.removeProperty('--central-body-blur');
  root.style.removeProperty('--central-body-shift');
  root.style.removeProperty('--central-body-scale');
}

export function CentralTransition() {
  const { centralActive } = useCentralMode();
  const [playing, setPlaying] = useState(false);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const prevRef = useRef(centralActive);
  const motionRef = useRef<Motion | null>(null);
  const pendingDirRef = useRef<1 | -1 | null>(null);

  // 模式变化才播放；首次挂载（含持久化重启直入）与非 desktop 都跳过。
  // 必须用 layout effect：React 在同一次提交里就把布局切成目标模式，
  // 普通 useEffect 要等浏览器绘制后才跑，用户会先看到一帧清晰的普通界面。
  useLayoutEffect(() => {
    const prev = prevRef.current;
    prevRef.current = centralActive;
    if (prev === centralActive || !isDesktopHost() || prefersReducedMotion()) return;
    const dir: 1 | -1 = centralActive ? 1 : -1;
    const motion = motionRef.current;
    if (motion) {
      // 中途反向：保持当前帧连续，不重置主体状态
      motion.dir = dir;
      motion.last = performance.now();
      return;
    }
    // 绘制前同步压制首帧：新布局已提交但尚不可见，随后由 rAF 渐显。
    // 初始值用与 step 相同的公式，避免首帧与后续帧出现两条渐显曲线。
    document.documentElement.classList.add(ACTIVE_CLASS);
    setBodyVars(bodyReveal(dir, dir === 1 ? 0 : TIMELINE.total, TIMELINE));
    pendingDirRef.current = dir;
    setPlaying(true);
  }, [centralActive]);

  useEffect(() => {
    if (!playing) return;
    const canvas = canvasRef.current;
    if (!canvas) {
      clearBodyVars();
      setPlaying(false);
      return;
    }
    let context: CanvasRenderingContext2D | null = null;
    try {
      context = canvas.getContext('2d');
    } catch {
      context = null;
    }
    // 无 Canvas 支持（如 jsdom）时直接降级：清掉 layout effect 预先压制的
    // 主体状态，保留现有界面，不产生动画。
    if (!context) {
      clearBodyVars();
      setPlaying(false);
      return;
    }
    const ctx: CanvasRenderingContext2D = context;
    const particles = buildTransitionParticles(PARAMS);
    const stars = buildTransitionStars(PARAMS);
    let width = 1;
    let height = 1;

    // canvas 是替换元素：仅靠 fixed + inset 不会拉伸，显示尺寸会跟随
    // canvas.width/height 的固有尺寸（调参台的 fit() 正是显式设了 style.width/height）。
    // 这里用窗口尺寸显式设定 CSS 尺寸，物理像素按 DPR 放大，绘制仍用逻辑坐标，
    // 保证覆盖「顶栏以下」的整个窗口，包括底部输入区。
    const TOP_INSET = 48; // StatusPanel 高度 h-12
    const resize = () => {
      const w = Math.max(1, window.innerWidth);
      const h = Math.max(1, window.innerHeight - TOP_INSET);
      // 低 DPR（例如缩放后的 webview）不低于 1，避免物理像素不足导致画面发糊。
      const dpr = Math.max(1, Math.min(window.devicePixelRatio || 1, 2));
      canvas.style.width = `${w}px`;
      canvas.style.height = `${h}px`;
      canvas.width = Math.max(1, Math.round(w * dpr));
      canvas.height = Math.max(1, Math.round(h * dpr));
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      width = w;
      height = h;
    };
    resize();
    window.addEventListener('resize', resize);

    const motion: Motion = {
      dir: pendingDirRef.current ?? 1,
      t: (pendingDirRef.current ?? 1) === 1 ? 0 : TIMELINE.total,
      raf: 0,
      last: performance.now(),
    };
    pendingDirRef.current = null;
    motionRef.current = motion;

    const step = (now: number) => {
      const current = motionRef.current;
      if (!current) return;
      const dt = Math.min(Math.max((now - current.last) / 1000, 0), 0.05);
      current.last = now;
      const speedMul = current.dir < 0 ? 1 / Math.max(0.2, PARAMS.exitRatio) : 1;
      current.t = Math.min(
        TIMELINE.total,
        Math.max(0, current.t + current.dir * dt * speedMul),
      );
      const t = current.t;
      setBodyVars(bodyReveal(current.dir, t, TIMELINE));
      const alpha = Math.pow(Math.sin(Math.PI * (t / TIMELINE.total)), 0.6);
      drawTransitionFrame(ctx, {
        width,
        height,
        t,
        clock: t,
        alpha,
        timeline: TIMELINE,
        params: PARAMS,
        particles,
        stars,
      });
      if ((current.dir === 1 && t >= TIMELINE.total) || (current.dir === -1 && t <= 0)) {
        motionRef.current = null;
        setPlaying(false);
        return;
      }
      current.raf = schedule(step);
    };
    motion.raf = schedule(step);

    return () => {
      if (motionRef.current) {
        cancel(motionRef.current.raf);
        motionRef.current = null;
      }
      window.removeEventListener('resize', resize);
      clearBodyVars();
    };
  }, [playing]);

  // 任何降级 / 卸载路径都保证 class 与 CSS 变量不残留（避免界面被永久压暗）。
  useEffect(() => () => clearBodyVars(), []);

  if (!playing) return null;
  return (
    <canvas
      ref={canvasRef}
      aria-hidden="true"
      data-testid="central-transition-canvas"
      className="pointer-events-none fixed left-0 top-12 z-50"
    />
  );
}
