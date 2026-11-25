/**
 * 活动状态值（Issue #778 保留部分）的前端展示口径：与后端
 * `central_state::activity` 的 `WorkActivity::label()` 同一套中文短标签，
 * 状态条与抽屉共用。
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

/** 非 running 的工作没有活动语义；旧后端缺省按 pending。 */
export function activityOf(item: { state: string; activity?: WorkActivity | null }): WorkActivity {
  return item.state === 'running' ? (item.activity ?? 'pending') : 'pending';
}
