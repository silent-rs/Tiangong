/**
 * 中枢模式切换动画测试（设计稿 §4.1；参数来自 docs/central-demo.html 调参台）。
 *
 * 覆盖：定型参数与时间轴、首次挂载/重启直入不播放、desktop 守卫、
 * reduced-motion、转场结束清理、Canvas 缺失时的静默降级、绘制函数边界。
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { CentralTransition } from '@/components/central/CentralTransition';
import {
  CENTRAL_TRANSITION_PARAMS as PARAMS,
  bodyReveal,
  buildTimeline,
  segmentProgress,
} from '@/components/central/centralTransitionParams';
import {
  buildTransitionParticles,
  buildTransitionStars,
  drawTransitionFrame,
  type FrameOptions,
} from '@/components/central/centralTransitionRender';
import { useStore } from '@/store/useStore';

const TIMELINE = buildTimeline(PARAMS);
const ACTIVE_CLASS = 'central-transition-active';

/** 任意属性读取 / 方法调用都返回自身的假 2D 上下文，绕开 jsdom 未实现的 canvas。 */
const proxy = new Proxy(function () {}, {
  get: () => proxy,
  apply: () => proxy,
  set: () => true,
});
const fakeCtx = proxy as unknown as CanvasRenderingContext2D;

let container: HTMLDivElement;
let root: Root;
let pendingFrame: FrameRequestCallback | null = null;
let clockNow = 0;

function setHost(kind: 'desktop' | 'web'): void {
  const w = window as unknown as {
    __TIANGONG_WEB__?: unknown;
    __TIANGONG_REMOTE__?: unknown;
  };
  delete w.__TIANGONG_WEB__;
  delete w.__TIANGONG_REMOTE__;
  if (kind === 'web') w.__TIANGONG_WEB__ = {};
}

function setReducedMotion(reduce: boolean): void {
  vi.stubGlobal('matchMedia', (query: string) => ({
    matches: reduce,
    media: query,
    onchange: null,
    addListener: () => {},
    removeListener: () => {},
    addEventListener: () => {},
    removeEventListener: () => {},
    dispatchEvent: () => false,
  }));
}

function mount(): void {
  act(() => {
    root.render(<CentralTransition />);
  });
}

function canvas(): HTMLCanvasElement | null {
  return container.querySelector<HTMLCanvasElement>('[data-testid="central-transition-canvas"]');
}

function advance(deltaMs: number): void {
  clockNow += deltaMs;
  const cb = pendingFrame;
  pendingFrame = null;
  if (cb) {
    act(() => cb(clockNow));
  }
}

beforeEach(() => {
  clockNow = performance.now() + 1000;
  pendingFrame = null;
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => {
    pendingFrame = cb;
    return 1;
  });
  vi.stubGlobal('cancelAnimationFrame', () => {});
  setReducedMotion(false);
  Object.defineProperty(HTMLCanvasElement.prototype, 'getContext', {
    configurable: true,
    value: () => fakeCtx,
  });
  setHost('desktop');
  useStore.setState({ centralMode: false });
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
  delete (HTMLCanvasElement.prototype as unknown as { getContext?: unknown }).getContext;
  document.documentElement.classList.remove(ACTIVE_CLASS);
  useStore.setState({ centralMode: false });
});

describe('定型参数与时间轴', () => {
  it('配色与时序与调参台 orbit 预设一致', () => {
    expect(PARAMS.bg).toBe('#070a12');
    expect(PARAMS.accent).toBe('#b15400');
    expect(PARAMS.accent2).toBe('#e6beb6');
    expect(PARAMS.particles).toBe(520);
    expect(PARAMS.exitRatio).toBe(0.62);
    expect(TIMELINE.total).toBeCloseTo(1.735, 3);
    expect(TIMELINE.segments.out.start).toBe(0);
  });

  it('段进度两端为 0 / 1，且段起点递进', () => {
    expect(segmentProgress('in', 0, TIMELINE)).toBe(0);
    expect(segmentProgress('in', TIMELINE.total, TIMELINE)).toBe(1);
    expect(TIMELINE.segments.open.start).toBeGreaterThan(0);
    expect(TIMELINE.segments.flow.start).toBeGreaterThan(TIMELINE.segments.open.start);
  });

  it('光尘与星点按种子确定性生成', () => {
    const a = buildTransitionParticles(PARAMS);
    const b = buildTransitionParticles(PARAMS);
    expect(a).toHaveLength(PARAMS.particles);
    expect(a[0].sx).toBe(b[0].sx);
    expect(buildTransitionStars(PARAMS)).toHaveLength(PARAMS.stars);
  });
});

describe('渐显与时间轴精确对应', () => {
  const { segments } = TIMELINE;

  it('进入：渐显只发生在 in 段（inD = 0.52s）', () => {
    expect(bodyReveal(1, 0, TIMELINE)).toBe(0);
    expect(bodyReveal(1, segments.in.start, TIMELINE)).toBe(0);
    const mid = bodyReveal(1, (segments.in.start + TIMELINE.total) / 2, TIMELINE);
    expect(mid).toBeGreaterThan(0);
    expect(mid).toBeLessThan(1);
    expect(bodyReveal(1, TIMELINE.total, TIMELINE)).toBeCloseTo(1, 5);
  });

  it('退出：渐显收尾落在 out 段倒放，中段保持 12% 失焦残影', () => {
    expect(bodyReveal(-1, TIMELINE.total, TIMELINE)).toBe(0);
    expect(bodyReveal(-1, segments.in.start, TIMELINE)).toBeCloseTo(0.12, 5);
    expect(bodyReveal(-1, segments.out.len, TIMELINE)).toBeCloseTo(0.12, 5);
    const midOut = bodyReveal(-1, segments.out.len / 2, TIMELINE);
    expect(midOut).toBeGreaterThan(0.12);
    expect(midOut).toBeLessThan(1);
    expect(bodyReveal(-1, 0, TIMELINE)).toBeCloseTo(1, 5);
  });

  it('渐显随 inD / out 参数变化，不是固定 450ms 动画', () => {
    const changed = buildTimeline({ ...PARAMS, inD: 1.2, out: 0.9 });
    expect(changed.segments.in.len).toBe(1.2);
    expect(changed.total).toBeGreaterThan(TIMELINE.total);
    expect(bodyReveal(1, TIMELINE.total, changed)).toBeLessThan(1);
  });
});

describe('绘制函数（无 WebGL / 纯 Canvas 2D）', () => {
  it('关键帧均不抛错', () => {
    const particles = buildTransitionParticles(PARAMS);
    const stars = buildTransitionStars(PARAMS);
    for (const t of [0, 0.4, 0.95, 1.4, TIMELINE.total]) {
      const options: FrameOptions = {
        width: 1200,
        height: 750,
        t,
        clock: t,
        alpha: Math.sin(Math.PI * (t / TIMELINE.total)),
        timeline: TIMELINE,
        params: PARAMS,
        particles,
        stars,
      };
      expect(() => drawTransitionFrame(fakeCtx, options)).not.toThrow();
    }
  });
});

describe('触发与降级', () => {
  it('首次挂载不播放（普通模式）', () => {
    mount();
    expect(canvas()).toBeNull();
  });

  it('持久化重启直入中枢不播放', () => {
    useStore.setState({ centralMode: true });
    mount();
    expect(canvas()).toBeNull();
  });

  it('desktop 下切换模式启动转场', () => {
    mount();
    act(() => useStore.getState().setCentralMode(true));
    expect(canvas()).not.toBeNull();
    expect(document.documentElement.classList.contains(ACTIVE_CLASS)).toBe(true);
  });

  it('覆盖层铺满顶栏以下整个窗口（回归：曾只覆盖左上角）', () => {
    mount();
    act(() => useStore.getState().setCentralMode(true));
    const target = canvas();
    expect(target).not.toBeNull();
    const el = target as HTMLCanvasElement;
    const dpr = Math.max(1, Math.min(window.devicePixelRatio || 1, 2));
    const expectedWidth = window.innerWidth;
    const expectedHeight = Math.max(1, window.innerHeight - 48);
    expect(el.style.width).toBe(`${expectedWidth}px`);
    expect(el.style.height).toBe(`${expectedHeight}px`);
    expect(el.width).toBe(Math.round(expectedWidth * dpr));
    expect(el.height).toBe(Math.round(expectedHeight * dpr));
  });

  it('非 desktop 宿主不启动转场', () => {
    setHost('web');
    mount();
    act(() => useStore.getState().setCentralMode(true));
    expect(canvas()).toBeNull();
  });

  it('prefers-reduced-motion 下跳过动画', () => {
    setReducedMotion(true);
    mount();
    act(() => useStore.getState().setCentralMode(true));
    expect(canvas()).toBeNull();
  });

  it('Canvas 不可用时静默降级，不影响界面', () => {
    Object.defineProperty(HTMLCanvasElement.prototype, 'getContext', {
      configurable: true,
      value: () => null,
    });
    mount();
    act(() => useStore.getState().setCentralMode(true));
    expect(document.documentElement.classList.contains(ACTIVE_CLASS)).toBe(false);
  });
});

describe('首帧时序（切回不闪现普通界面）', () => {
  it('退出中枢时，提交新布局的同一帧就把主体压到透明', () => {
    useStore.setState({ centralMode: true });
    mount();
    act(() => useStore.getState().setCentralMode(false));
    // layout effect 在浏览器绘制前同步执行，无需推进 rAF 就应已生效
    expect(document.documentElement.classList.contains(ACTIVE_CLASS)).toBe(true);
    expect(document.documentElement.style.getPropertyValue('--central-body-opacity')).toBe('0');
    expect(canvas()).not.toBeNull();
  });

  it('进入中枢时同样压制首帧', () => {
    mount();
    act(() => useStore.getState().setCentralMode(true));
    expect(document.documentElement.style.getPropertyValue('--central-body-opacity')).toBe('0');
  });
});

describe('转场生命周期', () => {
  it('进入结束后移除 class 与 CSS 变量，画布卸载', () => {
    mount();
    act(() => useStore.getState().setCentralMode(true));
    expect(document.documentElement.style.getPropertyValue('--central-body-opacity')).not.toBe('');
    // 1.735s / 每帧上限 50ms，留足余量推进到终态
    for (let i = 0; i < 60; i += 1) advance(50);
    expect(canvas()).toBeNull();
    expect(document.documentElement.classList.contains(ACTIVE_CLASS)).toBe(false);
    expect(document.documentElement.style.getPropertyValue('--central-body-opacity')).toBe('');
  });

  it('退出反向播放并在结束后清理', () => {
    useStore.setState({ centralMode: true });
    mount();
    act(() => useStore.getState().setCentralMode(false));
    expect(canvas()).not.toBeNull();
    for (let i = 0; i < 60; i += 1) advance(50);
    expect(canvas()).toBeNull();
    expect(document.documentElement.classList.contains(ACTIVE_CLASS)).toBe(false);
  });
});
