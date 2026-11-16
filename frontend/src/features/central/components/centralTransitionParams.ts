/**
 * 中枢模式切换动画参数与时间轴（设计稿 §4.1；实现来源 docs/central-demo.html）。
 *
 * 参数来自用户在调参台确认的 orbit 预设链接：
 * bg #070a12 / accent #b15400 / accent2 #e6beb6，
 * out .45 / open .82 / flow 1.05 / pulse .38 / inD .52 / lag .55 / exitRatio .62，
 * particles 520 / curvature .34 / tail .085 / size 1.5 / stagger .34，
 * coreRadius 70 / glow .82 / ripple 170 / breath .55 / stars 150，
 * orbits 4 / tilt .42 / orbitRadius 330 / orbitSpeed .28 / arc .55 / orbitDots 5。
 * 极光 / 网格预设的专属参数不参与 app 内实现（只移植已定型的 orbit）。
 *
 * 时间轴是单一连续变量 t：0 = 普通模式、total = 中枢模式，5 段带重叠
 * （失焦 → 空间展开 → 光尘汇聚 → 核心脉冲 → 中枢浮现）。
 * 所有视觉都是 t 的解析函数，因此进入、退出（时间倒放）、中途反向都无需隐藏状态。
 */
export interface CentralTransitionParams {
  bg: string;
  accent: string;
  accent2: string;
  /** 各段时长（秒） */
  out: number;
  open: number;
  flow: number;
  pulse: number;
  /** 「中枢浮现」段时长；`in` 是保留字，故用 inD */
  inD: number;
  /** 相邻段重叠比例 0~0.8 */
  lag: number;
  /** 退出相对进入的时长比（<1 表示更快） */
  exitRatio: number;
  particles: number;
  curvature: number;
  tail: number;
  size: number;
  stagger: number;
  coreRadius: number;
  glow: number;
  ripple: number;
  breath: number;
  stars: number;
  orbits: number;
  tilt: number;
  orbitRadius: number;
  orbitSpeed: number;
  arc: number;
  orbitDots: number;
}

export const CENTRAL_TRANSITION_PARAMS: CentralTransitionParams = {
  bg: '#070a12',
  accent: '#b15400',
  accent2: '#e6beb6',
  out: 0.45,
  open: 0.82,
  flow: 1.05,
  pulse: 0.38,
  inD: 0.52,
  lag: 0.55,
  exitRatio: 0.62,
  particles: 520,
  curvature: 0.34,
  tail: 0.085,
  size: 1.5,
  stagger: 0.34,
  coreRadius: 70,
  glow: 0.82,
  ripple: 170,
  breath: 0.55,
  stars: 150,
  orbits: 4,
  tilt: 0.42,
  orbitRadius: 330,
  orbitSpeed: 0.28,
  arc: 0.55,
  orbitDots: 5,
};

export type SegmentKey = 'out' | 'open' | 'flow' | 'pulse' | 'in';

export interface Segment {
  start: number;
  len: number;
}

export interface Timeline {
  segments: Record<SegmentKey, Segment>;
  total: number;
}

/** 段起点按「上一段时长 × (1 - lag)」递进，总时长取最晚一段的结束时刻。 */
export function buildTimeline(params: CentralTransitionParams): Timeline {
  const defs: Array<[SegmentKey, number]> = [
    ['out', params.out],
    ['open', params.open],
    ['flow', params.flow],
    ['pulse', params.pulse],
    ['in', params.inD],
  ];
  let cursor = 0;
  const segments = {} as Record<SegmentKey, Segment>;
  for (const [key, len] of defs) {
    segments[key] = { start: cursor, len: Math.max(0.01, len) };
    cursor += segments[key].len * (1 - params.lag);
  }
  const total = Math.max(
    0.1,
    ...Object.values(segments).map((segment) => segment.start + segment.len),
  );
  return { segments, total };
}

export function clamp01(value: number): number {
  return Math.min(1, Math.max(0, value));
}

export function easeSoft(x: number): number {
  return x * x * (3 - 2 * x);
}

export function easeOut(x: number): number {
  return 1 - Math.pow(1 - x, 3);
}

export function easeInOut(x: number): number {
  return x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2;
}

/** 段内归一化进度（0~1）。 */
export function segmentProgress(key: SegmentKey, t: number, timeline: Timeline): number {
  const segment = timeline.segments[key];
  return clamp01((t - segment.start) / segment.len);
}

/**
 * 主体容器渐显进度：与调参台两端界面的透明度公式一一对应，完全由时间轴参数驱动。
 *
 * 进入（目标 = 中枢视图）：对应调参台 `central-ui` 的 `come = E.out(uIn)`，
 *   渐显只发生在 in 段（inD = 0.52s）。
 * 退出（目标 = 普通视图）：对应调参台 `app-ui` 的
 *   `lerp(1, .12, away) * (1 - soft(uIn))`；时间倒放时界面先保持 12% 失焦残影，
 *   直到 out 段倒放（out = 0.45s）才完全显现，与光尘消散同步收尾。
 *
 * 这样调 `out / inD / lag` 时渐显会同步变化，不再有固定 450ms 的独立动画错位。
 */
export function bodyReveal(dir: 1 | -1, t: number, timeline: Timeline): number {
  const away = easeSoft(segmentProgress('out', t, timeline));
  const uIn = segmentProgress('in', t, timeline);
  if (dir === 1) return easeOut(uIn);
  return (1 - 0.88 * away) * (1 - easeSoft(uIn));
}
