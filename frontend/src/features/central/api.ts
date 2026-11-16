// 中枢模式前端 API（R6 抽离）：与后端 central-host crate 的 8 个
// Tauri command 一一对应。集中于此保持中枢边界清晰。
import { invoke } from '@/api/host';

export const centralApi = {
  getCentralWorkStatus: (): Promise<
    Array<{
      work_id: string;
      session_id: string;
      task: string;
      model: string | null;
      dispatched_at: string;
      state: string;
      reused_session: boolean;
    }>
  > => invoke('get_central_work_status'),
  centralActivationGreeting: (): Promise<void> => invoke('central_activation_greeting'),
  /** R4 模式协调：进入中枢（上报普通上下文快照，宿主保存为事实源）。 */
  centralModeEnter: (
    activeSessionId: string | null,
    newConversationId: string | null,
  ): Promise<{ mode: string }> =>
    invoke('central_mode_enter', { activeSessionId, newConversationId }),
  /** R4 模式协调：退出中枢（宿主归还快照；null = 无恢复点，须显式恢复分支）。 */
  centralModeExit: (): Promise<{
    mode: string;
    snapshot: {
      active_session_id: string | null;
      new_conversation_id: string | null;
      saved_at: string;
    } | null;
  }> => invoke('central_mode_exit'),
  /** R4 模式协调：查询模式状态与上次停留偏好（重启恢复依据）。 */
  centralModeState: (): Promise<{ mode: string; last_mode: string }> =>
    invoke('central_mode_state'),
  /** R5 授权卡片：用户真实操作签发 Workspace 授权票据（一次性/可记住）。 */
  centralApproveWorkspace: (
    path: string,
    taskFingerprint: string,
    persist: boolean,
  ): Promise<{ approval_id: string; workspace_id: string; expires_at: string }> =>
    invoke('central_approve_workspace', {
      path,
      taskFingerprint,
      persist,
    }),
  /** 决定卡片：用户选定选项后签发 DecisionTicket（绑定执行与版本）。 */
  centralResolveDecision: (
    executionId: string,
    questionVersion: number,
    option: string,
  ): Promise<{
    ticket_id: string;
    execution_id: string;
    chosen_option: string;
    expires_at: string;
  }> =>
    invoke('central_resolve_decision', {
      executionId,
      questionVersion,
      option,
    }),
  /** R4 工作抽屉：工作项事件时间线（事件日志事实源）。 */
  centralWorkTimeline: (
    workId: string,
  ): Promise<Array<{ at: string; kind: string; note: string }>> =>
    invoke('central_work_timeline', { workId }),
  /** 等待用户决策的工作：卡片上直接答复（提交即恢复该工作，卡片消失）。 */
  centralAnswerWaiting: (
    workId: string,
    answer: string,
  ): Promise<{ work_id: string; state: string }> =>
    invoke('central_answer_waiting', { workId, answer }),
};
