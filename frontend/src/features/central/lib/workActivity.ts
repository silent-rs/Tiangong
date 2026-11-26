/**
 * 活动状态值（Issue #778 保留部分）的前端展示口径。标签与后端
 * `central_state::activity::WorkActivity::label()` 保持一致，状态条与抽屉共用。
 */
import type { WorkActivity } from '../api';

export const ACTIVITY_LABELS: Record<WorkActivity, string> = {
  pending: '等待输出',
  responding: '回复中',
  reasoning: '思考中',
  requesting: '等待响应',
  tool_running: '执行中',
  compressing: '压缩中',
  switching_model: '切换模型',
  retrying: '重试中',
  subagent: '子代理中',
};

/** 非 running 的工作没有活动语义；旧后端或未知运行时值按 pending。 */
export function activityOf(item: { state: string; activity?: WorkActivity | null }): WorkActivity {
  const activity = item.activity;
  return item.state === 'running' &&
    activity &&
    Object.prototype.hasOwnProperty.call(ACTIVITY_LABELS, activity)
    ? activity
    : 'pending';
}

/** 最近动作与活动标签相同时不重复展示（如「回复中 · 回复中」）。 */
export function activityDetail(item: {
  state: string;
  activity?: WorkActivity | null;
  last_activity?: string | null;
}): string | null {
  if (item.state !== 'running') return null;
  const detail = item.last_activity?.trim();
  if (!detail) return null;
  return detail === ACTIVITY_LABELS[activityOf(item)] ? null : detail;
}
