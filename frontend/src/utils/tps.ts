/** 实时 TPS 的前端消费工具。
 *
 * 数据源是 stream 的累计计数：`StreamEvent::Delta/ReactText/SummaryText.tokens`
 * 携带该消息截至本批的累计 output tokens（provider 层按正文 delta 事件
 * 计数——DeepSeek Responses / Anthropic 的 delta 事件即服务端 token 粒度）；
 * 官方 usage 到达时经 `calibrate` 对齐窗口基准，不参与产量差分。前端按
 * 相邻事件差分得到实时速度，不做本地分词；字段缺失（旧后端 / 插件反馈）
 * 时不计速度，但跳秒仍由 `startedAt` 独立驱动。
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
  /** 本轮是否已开始（首个流式文本事件起即 true，与是否有速度样本无关）。 */
  active: boolean;
  /** 实时速度（tokens/s，累计差分）；样本不足时为 0。 */
  tps: number;
  /** 首个流式文本事件距今的毫秒数；独立于速度样本窗口，长流不回退。 */
  elapsedMs: number | null;
}

interface TpsSample {
  at: number;
  /** 该时点的累计 output tokens。 */
  cumulative: number;
}

interface TpsSessionState {
  /** 会话内首个流式输出事件时刻（跳秒基准，不随窗口裁剪删除）。
   *  跨轮保留：仅在会话删除 / 页面重载时随之消失。 */
  startedAt: number | null;
  samples: TpsSample[];
  /** 最近一次算出的速度：跨轮保留（轮次结束不清空），等待期（TTFT /
   *  工具执行 / 轮次空闲）沿用最近值避免数字跳动；轮内新一次 LLM 请求
   *  （计数回退重开窗口）期间同样保留。仅会话删除（tracker.reset）或
   *  页面重载清空；新样本重新算出速度后自然覆盖。 */
  lastTps: number;
}

/** 实时 TPS 滑动窗口：累计样本差分 + 独立开始计时（纯数据结构，时间可注入便于测试）。 */
export class LiveTpsTracker {
  private sessions = new Map<string, TpsSessionState>();

  private stateOf(sessionId: string): TpsSessionState {
    let state = this.sessions.get(sessionId);
    if (!state) {
      state = { startedAt: null, samples: [], lastTps: 0 };
      this.sessions.set(sessionId, state);
    }
    return state;
  }

  /** 记录一个流式文本事件：驱动跳秒计时；携带累计值时另计速度样本。 */
  record(
    sessionId: string,
    cumulative: number | null | undefined,
    now: number = Date.now(),
  ): void {
    if (!sessionId) return;
    const state = this.stateOf(sessionId);
    if (state.startedAt == null) state.startedAt = now;

    if (typeof cumulative !== 'number' || !Number.isFinite(cumulative) || cumulative < 0) {
      return;
    }
    const last = state.samples[state.samples.length - 1];
    if (last && now - last.at <= 1 && cumulative >= last.cumulative) {
      // 同批（≤1ms）样本合并。
      state.samples[state.samples.length - 1] = { at: last.at, cumulative };
    } else if (last && cumulative < last.cumulative) {
      // 累计回退：ReAct 轮内新一次 LLM 请求的计数从 1 重新开始（每流独立
      // 计数），重开样本窗口重新累计；lastTps 保留——新窗口算出速度前
      //（工具执行等待 / 新流爬升期）沿用最近显示，不跳动。
      state.samples = [{ at: now, cumulative }];
    } else {
      state.samples.push({ at: now, cumulative });
    }
    this.prune(state, now);
  }

  /** 官方计量校准：把窗口基准对齐到官方累计值（单次 LLM 请求的
   *  completion_tokens），**不**把「delta 事件计数 ↔ token 数」的换算差
   *  当成一次产量差分——否则会瞬间冒出虚高速率，或在校准值偏小时产生
   *  负差分。校准只重置基准点：重置前若 UI 还没采样过（lastTps 仍为 0），
   *  先用校准前的样本补算一次速度，避免「短请求结束即校准」把这段已产生
   *  的速度整体丢掉（下一次请求正文到达前速度行会一直隐藏）。
   *  只允许主对话 ReAct 请求的官方 usage 调用；插件用量、上下文压缩、
   *  取消增量等来源量纲或语义不同，不得混入。 */
  calibrate(
    sessionId: string,
    cumulative: number | null | undefined,
    now: number = Date.now(),
  ): void {
    if (!sessionId) return;
    if (typeof cumulative !== 'number' || !Number.isFinite(cumulative) || cumulative < 0) {
      return;
    }
    const state = this.stateOf(sessionId);
    if (state.startedAt == null) state.startedAt = now;
    if (state.lastTps === 0) {
      const tps = this.windowTps(state);
      if (tps > 0) state.lastTps = tps;
    }
    state.samples = [{ at: now, cumulative }];
  }

  /** 用当前窗口样本差分出速度（纯计算，不修改状态）；样本不足或产量
   *  非正返回 0。snapshot 与 calibrate 共用同一口径。 */
  private windowTps(state: TpsSessionState): number {
    const samples = state.samples;
    if (samples.length < 2) return 0;
    const produced = samples[samples.length - 1].cumulative - samples[0].cumulative;
    if (produced <= 0) return 0;
    const span = Math.max(
      MIN_SPAN_MS,
      Math.min(TPS_WINDOW_MS, samples[samples.length - 1].at - samples[0].at),
    );
    return produced / (span / 1000);
  }

  /** 会话删除（core 实例销毁）时清空该会话窗口；轮次边界不清空。 */
  reset(sessionId: string): void {
    if (sessionId) this.sessions.delete(sessionId);
  }

  /** 当前窗口快照。样本不足或全部老化时返回最近速度（等待期保留显示）。 */
  snapshot(sessionId: string, now: number = Date.now()): LiveTpsSnapshot {
    const state = this.sessions.get(sessionId);
    if (!state || state.startedAt == null) {
      return { active: false, tps: 0, elapsedMs: null };
    }
    const elapsedMs = now - state.startedAt;
    this.prune(state, now);
    const tps = this.windowTps(state);
    if (tps > 0) state.lastTps = tps;
    return { active: true, tps: tps > 0 ? tps : state.lastTps, elapsedMs };
  }

  /** 滑出窗口的速度样本移除（startedAt 不参与裁剪）；至少保留 1 个。 */
  private prune(state: TpsSessionState, now: number): void {
    if (state.samples.length <= 1) return;
    const cutoff = now - TPS_WINDOW_MS;
    while (state.samples.length > 1 && state.samples[0].at < cutoff) state.samples.shift();
  }
}

/** 全局单例：store 投喂事件，各 UI 采样读取。 */
export const liveTpsTracker = new LiveTpsTracker();
