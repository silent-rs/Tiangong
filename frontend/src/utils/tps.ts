/** 实时 TPS 的前端消费工具。
 *
 * 数据源是 stream 上的官方 usage 累计：`StreamEvent::Delta/ReactText/
 * SummaryText.tokens` 携带该消息截至本批的累计 output tokens（provider
 * 官方口径——Anthropic 逐事件提供，DeepSeek / OpenAI 系仅在流末，正文
 * 期间字段缺失）。前端按相邻事件差分得到实时速度，不做任何本地估算；
 * 字段缺失（旧后端 / 插件反馈 / DeepSeek 流中）时不记样本。
 *
 * 本模块是纯函数 / 纯数据结构，不依赖 React 与 store（hook 见 hooks/useLiveTps.ts）。
 */

/** 速度文本：≥100 tok/s 显示整数，否则保留一位小数。 */
export function formatTps(tps: number): string {
  return tps >= 100 ? `${tps.toFixed(0)} tok/s` : `${tps.toFixed(1)} tok/s`;
}

/** 保留一位小数（显示口径统一）。 */
export function round1(value: number): number {
  return Math.round(value * 10) / 10;
}

/** 滑动窗口时长（毫秒）：窗口越长速度越平滑，越短越跟手。 */
export const TPS_WINDOW_MS = 2000;
/** 速度分母下限（毫秒）：开局两个样本不至于除以极小值出现数值尖峰。 */
const MIN_SPAN_MS = 300;

export interface LiveTpsSnapshot {
  /** 本轮是否已有官方累计样本（区分「无官方数据」与「速度为 0」）。 */
  active: boolean;
  /** 实时速度（tokens/s，官方累计差分）；样本不足时为 0。 */
  tps: number;
  /** 首个样本距今的毫秒数；无样本为 null。 */
  elapsedMs: number | null;
}

interface TpsSample {
  at: number;
  /** 该时点的官方累计 output tokens。 */
  cumulative: number;
}

interface TpsSessionState {
  samples: TpsSample[];
}

/** 实时 TPS 滑动窗口：消费官方累计样本，窗口内首尾差分（纯数据结构，时间可注入便于测试）。 */
export class LiveTpsTracker {
  private sessions = new Map<string, TpsSessionState>();

  /** 记录一个官方累计样本（`cumulative` 缺失 / 非法时忽略本事件）。 */
  recordCumulative(
    sessionId: string,
    cumulative: number | null | undefined,
    now: number = Date.now(),
  ): void {
    if (!sessionId) return;
    if (typeof cumulative !== 'number' || !Number.isFinite(cumulative) || cumulative < 0) return;
    const state = this.sessions.get(sessionId) ?? { samples: [] };
    const last = state.samples[state.samples.length - 1];
    // 同批（≤1ms）样本合并；累计值回退（消息切换）时重开窗口。
    if (last && now - last.at <= 1 && cumulative >= last.cumulative) {
      state.samples[state.samples.length - 1] = { at: last.at, cumulative };
      this.prune(state, now);
      this.sessions.set(sessionId, state);
      return;
    }
    if (last && cumulative < last.cumulative) {
      state.samples = [];
    }
    state.samples.push({ at: now, cumulative });
    this.prune(state, now);
    this.sessions.set(sessionId, state);
  }

  /** 轮次结束 / 新轮次开始：清空该会话窗口。 */
  reset(sessionId: string): void {
    if (sessionId) this.sessions.delete(sessionId);
  }

  /** 当前窗口快照。 */
  snapshot(sessionId: string, now: number = Date.now()): LiveTpsSnapshot {
    const state = this.sessions.get(sessionId);
    if (!state || state.samples.length === 0) {
      return { active: false, tps: 0, elapsedMs: null };
    }
    this.prune(state, now);
    const samples = state.samples;
    const elapsedMs = now - samples[0].at;
    if (samples.length < 2) return { active: true, tps: 0, elapsedMs };
    const produced = samples[samples.length - 1].cumulative - samples[0].cumulative;
    if (produced <= 0) return { active: true, tps: 0, elapsedMs };
    // 分母取窗口内首尾样本的实际跨度（≥下限、≤窗口长）。
    const span = Math.max(
      MIN_SPAN_MS,
      Math.min(TPS_WINDOW_MS, samples[samples.length - 1].at - samples[0].at),
    );
    return { active: true, tps: produced / (span / 1000), elapsedMs };
  }

  /** 滑出窗口的样本移除；至少保留 1 个。 */
  private prune(state: TpsSessionState, now: number): void {
    if (state.samples.length <= 1) return;
    const cutoff = now - TPS_WINDOW_MS;
    while (state.samples.length > 1 && state.samples[0].at < cutoff) state.samples.shift();
  }
}

/** 全局单例：store 投喂事件，各 UI 采样读取。 */
export const liveTpsTracker = new LiveTpsTracker();
