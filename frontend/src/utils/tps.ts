/** 实时 TPS 的前端消费工具。
 *
 * 数据源是后端真实值：`tiangong-llm` 在流式 delta 处用 BPE tokenizer
 * （tiktoken o200k_base）计数，经 `StreamEvent::Delta/ReactText/SummaryText`
 * 的 `tokens` 字段下发（见 docs/message-tps-display-design.md 第 5 节）。
 * 前端不估算 token；事件缺失该字段（旧后端 / 插件文本反馈）时不记样本。
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
/** 速度分母下限（毫秒）：开局一两个样本不至于除以极小值出现数值尖峰。 */
const MIN_SPAN_MS = 300;

export interface LiveTpsSnapshot {
  /** 本轮是否已有正文样本（区分「尚未出字」与「速度为 0」）。 */
  active: boolean;
  /** 实时速度（tokens/s，后端真实计数）；样本不足时为 0。 */
  tps: number;
  /** 首个样本距今的毫秒数；无样本为 null。 */
  elapsedMs: number | null;
}

interface TpsSample {
  at: number;
  tokens: number;
}

interface TpsSessionState {
  samples: TpsSample[];
  startedAt: number | null;
}

/** 实时 TPS 滑动窗口（纯数据结构，时间可注入便于测试）。 */
export class LiveTpsTracker {
  private sessions = new Map<string, TpsSessionState>();

  /** 记录一批正文增量的真实 token 数（`tokens` 缺失 / 非法时忽略本批）。 */
  recordTokens(sessionId: string, tokens: number | null | undefined, now: number = Date.now()): void {
    if (!sessionId) return;
    if (typeof tokens !== 'number' || !Number.isFinite(tokens) || tokens <= 0) return;
    const state = this.sessions.get(sessionId) ?? { samples: [], startedAt: null };
    if (state.startedAt == null) state.startedAt = now;
    const last = state.samples[state.samples.length - 1];
    // 同批（≤1ms）事件合并为一个样本，窗口内同刻不堆积。
    if (last && now - last.at <= 1) {
      state.samples[state.samples.length - 1] = { at: last.at, tokens: last.tokens + tokens };
    } else {
      state.samples.push({ at: now, tokens });
    }
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
    if (!state || state.startedAt == null) return { active: false, tps: 0, elapsedMs: null };
    this.prune(state, now);
    const elapsedMs = now - state.startedAt;
    if (state.samples.length < 2) return { active: true, tps: 0, elapsedMs };
    const windowTokens = state.samples.reduce((sum, sample) => sum + sample.tokens, 0);
    if (windowTokens <= 0) return { active: true, tps: 0, elapsedMs };
    // 分母取窗口内最早样本至今的实际跨度：开局按真实跨度（≥下限），稳态封顶窗口长。
    const oldestAt = state.samples[0].at;
    const span = Math.max(MIN_SPAN_MS, Math.min(TPS_WINDOW_MS, now - oldestAt));
    return { active: true, tps: windowTokens / (span / 1000), elapsedMs };
  }

  /** 滑出窗口的样本移除；至少保留 1 个用于跨度参考。 */
  private prune(state: TpsSessionState, now: number): void {
    if (state.samples.length <= 1) return;
    const cutoff = now - TPS_WINDOW_MS;
    while (state.samples.length > 1 && state.samples[0].at < cutoff) state.samples.shift();
  }
}

/** 全局单例：store 投喂事件，各 UI 采样读取。 */
export const liveTpsTracker = new LiveTpsTracker();
