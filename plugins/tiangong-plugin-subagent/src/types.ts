// 与 plugins/tiangong-plugin-subagent/protocol（serde snake_case）对齐的 TS 类型。

export type BackendKind =
  | 'cli'
  | 'tiangong_session'
  | 'agent_team'
  | 'claude_code'
  | 'codex'
  | 'octoloop';

export type WorkspacePolicy = 'read-only' | 'read-write-exclusive' | 'isolated-worktree';

export interface AgentConfig {
  id: string;
  name: string;
  description: string;
  backend: BackendKind;
  command?: string | null;
  /** 天工会话后端关联的源会话 ID。 */
  session_id?: string | null;
  workspace_policy: WorkspacePolicy;
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface AdapterCapabilities {
  messaging: boolean;
  tasks: boolean;
  streaming: boolean;
  resume: boolean;
  correction: boolean;
  interrupt: boolean;
  approval: boolean;
  workspace_write: boolean;
  artifacts: boolean;
  internal_progress: boolean;
}

export type RunStatus =
  | 'ready'
  | 'working'
  | 'stopping'
  | 'blocked'
  | 'approval_required'
  | 'completed'
  | 'failed'
  | 'cancelled'
  | 'interrupted';

export interface ActivationRecord {
  activation_id: string;
  agent_id: string;
  session_id: string;
  workspace: string;
  workspace_policy: WorkspacePolicy;
  source_workspace: string;
  activated_at: string;
  deactivated_at?: string | null;
  worktree_retained: boolean;
}

export interface AgentSummary {
  config: AgentConfig;
  capabilities: AdapterCapabilities;
  activations: ActivationRecord[];
  activated_in_session: boolean;
  /** 成员在当前会话工作区的专属会话（在该工作区发起过才有）。 */
  workspace_session_id?: string | null;
  runtime_status?: RunStatus | null;
  active_run_id?: string | null;
  instructions?: string;
}

export type TaskStatus =
  | 'pending'
  | 'running'
  | 'completed'
  | 'failed'
  | 'cancelled'
  | 'interrupted';

export interface TaskRecord {
  task_id: string;
  agent_id: string;
  session_id: string;
  goal: string;
  completion_criteria?: string | null;
  status: TaskStatus;
  runs: string[];
  result_summary?: string | null;
  created_at: string;
  updated_at: string;
}

export interface RunRecord {
  run_id: string;
  task_id?: string | null;
  agent_id: string;
  activation_id: string;
  session_id: string;
  kind: 'task' | 'message';
  status: RunStatus;
  pid?: number | null;
  workspace: string;
  created_at: string;
  updated_at: string;
  finished_at?: string | null;
  summary?: string | null;
}

export interface AgentEventRecord {
  event_id: string;
  agent_id: string;
  activation_id?: string | null;
  session_id: string;
  task_id?: string | null;
  run_id?: string | null;
  workspace: string;
  event_type: string;
  created_at: string;
  payload: Record<string, unknown>;
}

export interface StateSnapshot {
  agents: AgentSummary[];
  session_id: string;
  active_tasks: TaskRecord[];
  recent_runs: RunRecord[];
  recent_events: AgentEventRecord[];
}

export const RUN_STATUS_LABELS: Record<RunStatus, string> = {
  ready: '空闲',
  working: '工作中',
  stopping: '停止请求中',
  blocked: '阻塞',
  approval_required: '待审批',
  completed: '已完成',
  failed: '失败',
  cancelled: '已取消',
  interrupted: '已中断',
};

export const TASK_STATUS_LABELS: Record<TaskStatus, string> = {
  pending: '待执行',
  running: '进行中',
  completed: '已完成',
  failed: '失败',
  cancelled: '已取消',
  interrupted: '已中断',
};

export const BACKEND_LABELS: Record<BackendKind, string> = {
  cli: 'CLI 命令',
  tiangong_session: '天工会话',
  agent_team: '天工原生 Subagent',
  claude_code: 'Claude Code',
  codex: 'Codex',
  octoloop: 'OctoLoop',
};

export const BACKEND_IMPLEMENTED: Record<BackendKind, boolean> = {
  cli: true,
  tiangong_session: true,
  agent_team: true,
  claude_code: false,
  codex: false,
  octoloop: false,
};

/** 会话列表条目（ui_list_sessions）。 */
export interface SessionBrief {
  id: string;
  title: string;
  updated_at: string;
  message_count: number;
}

/** 长期记忆文件条目（ui_list_memory）。 */
export interface MemoryFileEntry {
  name: string;
  size_bytes: number;
  updated_at?: string | null;
}

export const WORKSPACE_POLICY_LABELS: Record<WorkspacePolicy, string> = {
  'read-only': '只读',
  'read-write-exclusive': '独占写',
  'isolated-worktree': '隔离 worktree',
};
