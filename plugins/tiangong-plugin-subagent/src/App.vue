<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue';
import type { HostContext } from '@tiangong/plugin-sdk';
import {
  BACKEND_IMPLEMENTED,
  BACKEND_LABELS,
  RUN_STATUS_LABELS,
  TASK_STATUS_LABELS,
  WORKSPACE_POLICY_LABELS,
  type AgentEventRecord,
  type AgentSummary,
  type MemoryFileEntry,
  type StateSnapshot,
} from './types';
import { sidecarCall, subscribeSubagentEvents } from './api';
import AgentForm, { type AgentFormValue } from './components/AgentForm.vue';

interface Props {
  initialHostContext?: HostContext;
  subscribeHostContext?: (handler: (context: HostContext) => void) => () => void;
  shadowContainer?: boolean;
}

const props = withDefaults(defineProps<Props>(), {
  initialHostContext: undefined,
  subscribeHostContext: undefined,
  shadowContainer: false,
});

type FilterKind = 'all' | 'session' | 'running' | 'waiting' | 'stopped';

const FILTERS: Array<{ key: FilterKind; label: string }> = [
  { key: 'all', label: '全部' },
  { key: 'session', label: '当前会话' },
  { key: 'running', label: '运行中' },
  { key: 'waiting', label: '等待处理' },
  { key: 'stopped', label: '已停用' },
];

const snapshot = ref<StateSnapshot | null>(null);
const loaded = ref(false);
const filter = ref<FilterKind>('all');
const sessionContext = ref<{ id: string; workspace: string } | null>(null);
const connectionError = ref('');
const busyAgentId = ref<string | null>(null);
const messageDraft = ref<Record<string, string>>({});
const expandedAgentId = ref<string | null>(null);
const formVisible = ref(false);
const formInitial = ref<AgentFormValue | null>(null);
const formError = ref('');

let stopEvents: (() => void) | null = null;
let stopHostContext: (() => void) | null = null;
let refreshTimer: ReturnType<typeof setTimeout> | null = null;
let disposed = false;

const sessionId = computed(() => sessionContext.value?.id ?? '');
const workspace = computed(() => sessionContext.value?.workspace ?? '');

function applyContext(context?: HostContext) {
  const session = context?.session;
  sessionContext.value = session?.id
    ? { id: session.id, workspace: session.workspace ?? '' }
    : null;
}

const filteredAgents = computed<AgentSummary[]>(() => {
  const agents = snapshot.value?.agents ?? [];
  switch (filter.value) {
    case 'session':
      return agents.filter((agent) => agent.activated_in_session);
    case 'running':
      return agents.filter((agent) => agent.runtime_status === 'working');
    case 'waiting':
      return agents.filter((agent) =>
        agent.runtime_status === 'blocked' || agent.runtime_status === 'approval_required');
    case 'stopped':
      return agents.filter((agent) => !agent.config.enabled);
    default:
      return agents;
  }
});

const waitingCount = computed(() =>
  (snapshot.value?.agents ?? []).filter((agent) =>
    agent.runtime_status === 'blocked' || agent.runtime_status === 'approval_required').length);

const runningCount = computed(() =>
  (snapshot.value?.agents ?? []).filter((agent) => agent.runtime_status === 'working').length);

function sessionActivation(agent: AgentSummary) {
  return agent.activations.find((item) => item.session_id === sessionId.value);
}

function runtimeStatusLabel(agent: AgentSummary): string {
  return agent.runtime_status ? RUN_STATUS_LABELS[agent.runtime_status] : '空闲';
}

function runtimeStatusTone(agent: AgentSummary): string {
  switch (agent.runtime_status) {
    case 'working':
    case 'stopping':
      return 'tone-working';
    case 'blocked':
    case 'approval_required':
      return 'tone-waiting';
    case 'failed':
      return 'tone-failed';
    case 'completed':
      return 'tone-completed';
    default:
      return 'tone-idle';
  }
}

function tasksOf(agent: AgentSummary) {
  const active = snapshot.value?.active_tasks ?? [];
  return active.filter((task) => task.agent_id === agent.config.id);
}

function eventsOf(agent: AgentSummary) {
  const events = snapshot.value?.recent_events ?? [];
  return events.filter((event) => event.agent_id === agent.config.id).slice(0, 12);
}

function eventText(payload: Record<string, unknown>): string {
  const text = payload?.text;
  if (typeof text === 'string') return text;
  const status = payload?.status;
  return typeof status === 'string' ? status : '';
}

/// 发起方标注：协作事件（origin_session）映射为发起成员名，主会话发起无标注。
function originLabel(event: AgentEventRecord): string {
  const origin = event.payload?.origin_session;
  if (typeof origin !== 'string' || !origin) return '';
  const members = snapshot.value?.agents ?? [];
  const from = members.find((agent) => agent.config.session_id === origin);
  return from ? `来自 ${from.config.name}` : `来自 ${origin.slice(-6)}`;
}

/// 协作时间线：跨成员按时间正序聚合「有发起方或含结果文本」的关键事件。
const collaborationTimeline = computed(() => {
  const events = snapshot.value?.recent_events ?? [];
  const members = snapshot.value?.agents ?? [];
  const nameOf = (agentId: string) =>
    members.find((agent) => agent.config.id === agentId)?.config.name ?? agentId;
  return events
    .filter(
      (event) =>
        typeof event.payload?.origin_session === 'string'
        || event.payload?.text
        || event.event_type === 'run_started',
    )
    .slice(0, 60)
    .map((event) => ({
      ...event,
      from: originLabel(event) || '主会话',
      target: nameOf(event.agent_id),
      summary: eventText(event.payload),
    }))
    .reverse();
});

/// MCP 接入配置参考（外部 agent 工具的 mcpServers 片段）。
const mcpCopied = ref(false);
const mcpConfigSnippet = JSON.stringify(
  {
    mcpServers: {
      'tiangong-subagent': {
        command: '~/.tiangong/plugins/subagent/tiangong-subagent-sidecar',
        args: [],
        env: {
          TIANGONG_STORAGE_ROOT: '~/.tiangong',
          TIANGONG_SERVER_URL: 'http://127.0.0.1:9090',
        },
      },
    },
  },
  null,
  2,
);

async function copyMcpConfig() {
  try {
    await navigator.clipboard.writeText(mcpConfigSnippet);
    mcpCopied.value = true;
    setTimeout(() => (mcpCopied.value = false), 1500);
  } catch {
    mcpCopied.value = false;
  }
}

const COLLAB_EVENT_LABELS: Record<string, string> = {
  run_started: '发起运行',
  completed: '完成回报',
  failed: '失败回报',
  cancelled: '已取消',
  interrupted: '被中断',
  user_message: '追加消息',
};

async function refresh() {
  try {
    console.log('[subagent] refresh: sessionId =', JSON.stringify(sessionId.value));
    const body = await sidecarCall<StateSnapshot>('ui_state_snapshot', {
      session_id: sessionId.value,
    });
    console.log('[subagent] refresh: agents =', body?.agents?.length);
    if (!disposed) {
      snapshot.value = body;
      connectionError.value = '';
      loaded.value = true;
    }
  } catch (error) {
    console.error('[subagent] refresh error:', error);
    if (!disposed) {
      connectionError.value = String((error as Error).message ?? error);
      loaded.value = true;
    }
  }
}

function scheduleRefresh() {
  if (refreshTimer || disposed) return;
  refreshTimer = setTimeout(() => {
    refreshTimer = null;
    void refresh();
  }, 300);
}

async function withBusy(agentId: string, action: () => Promise<unknown>) {
  busyAgentId.value = agentId;
  try {
    await action();
    await refresh();
  } catch (error) {
    connectionError.value = String((error as Error).message ?? error);
  } finally {
    busyAgentId.value = null;
  }
}

function activate(agent: AgentSummary) {
  if (!sessionContext.value) {
    connectionError.value = '缺少当前会话上下文，无法激活';
    return;
  }
  void withBusy(agent.config.id, () => sidecarCall('ui_activate', {
    agent_id: agent.config.id,
    session_id: sessionId.value,
    workspace: workspace.value,
  }));
}

function deactivate(agent: AgentSummary) {
  void withBusy(agent.config.id, () => sidecarCall('ui_deactivate', {
    agent_id: agent.config.id,
    session_id: sessionId.value,
  }));
}

function sendMessage(agent: AgentSummary) {
  const content = (messageDraft.value[agent.config.id] ?? '').trim();
  if (!content || !sessionContext.value) return;
  messageDraft.value[agent.config.id] = '';
  void withBusy(agent.config.id, () => sidecarCall('ui_send_message', {
    agent_id: agent.config.id,
    session_id: sessionId.value,
    workspace: workspace.value,
    content,
  }));
}

function submitTask(agent: AgentSummary) {
  if (!sessionContext.value) {
    connectionError.value = '缺少当前会话上下文，无法提交任务';
    return;
  }
  void withBusy(agent.config.id, () => sidecarCall('ui_submit_task', {
    agent_id: agent.config.id,
    session_id: sessionId.value,
    workspace: workspace.value,
    goal: `管理页触发：请执行 ${agent.config.name} 的职责`,
  }));
}

function interruptRun(agent: AgentSummary) {
  if (!agent.active_run_id) return;
  void withBusy(agent.config.id, () => sidecarCall('ui_interrupt_run', {
    run_id: agent.active_run_id,
  }));
}

function cancelRun(agent: AgentSummary) {
  if (!agent.active_run_id) return;
  void withBusy(agent.config.id, () => sidecarCall('ui_cancel_run', {
    run_id: agent.active_run_id,
  }));
}

function openCreate() {
  formInitial.value = null;
  formError.value = '';
  formVisible.value = true;
}

function openEdit(agent: AgentSummary) {
  formInitial.value = {
    agentId: agent.config.id,
    name: agent.config.name,
    description: agent.config.description,
    backend: agent.config.backend,
    command: agent.config.command ?? '',
    sessionId: agent.config.session_id ?? null,
    workspacePolicy: agent.config.workspace_policy,
    enabled: agent.config.enabled,
    instructions: agent.instructions ?? '',
  };
  formError.value = '';
  formVisible.value = true;
}

async function submitForm(value: AgentFormValue) {
  try {
    if (value.agentId) {
      await sidecarCall('ui_agent_update', {
        agent_id: value.agentId,
        name: value.name,
        description: value.description,
        backend: value.backend,
        command: value.command,
        session_id: value.sessionId,
        workspace_policy: value.workspacePolicy,
        enabled: value.enabled,
        instructions: value.instructions,
      });
    } else {
      await sidecarCall('ui_agent_create', {
        name: value.name,
        description: value.description,
        backend: value.backend,
        command: value.command,
        session_id: value.sessionId,
        workspace_policy: value.workspacePolicy,
        instructions: value.instructions,
      });
    }
    formVisible.value = false;
    await refresh();
  } catch (error) {
    formError.value = String((error as Error).message ?? error);
  }
}

async function removeAgent(agent: AgentSummary) {
  const label = agent.config.name;
  if (!window.confirm(`确定删除 Subagent「${label}」？身份、指令与产物目录将一并删除（任务与运行历史保留）。`)) {
    return;
  }
  void withBusy(agent.config.id, () => sidecarCall('ui_agent_delete', {
    agent_id: agent.config.id,
  }));
}

function toggleExpand(agentId: string) {
  const expanding = expandedAgentId.value !== agentId;
  expandedAgentId.value = expanding ? agentId : null;
  if (expanding) {
    void loadMemory(agentId);
    void loadWorkspaceStates(agentId);
  }
}

// ── 长期记忆面板 ─────────────────────────────────────────────

const memoryFiles = ref<Record<string, MemoryFileEntry[]>>({});
const memoryDrafts = ref<Record<string, { name: string; content: string; original: string }>>({});
const memoryError = ref('');
const memoryNotice = ref('');

interface WorkspaceState {
  workspace_id: string;
  paths: string[];
  plan: string;
  context: string;
  task: string;
}

const workspaceStates = ref<Record<string, WorkspaceState[]>>({});

async function loadWorkspaceStates(agentId: string) {
  try {
    const body = await sidecarCall<{ workspaces: WorkspaceState[] }>('ui_list_workspace_states', {
      agent_id: agentId,
    });
    workspaceStates.value = { ...workspaceStates.value, [agentId]: body.workspaces ?? [] };
  } catch {
    workspaceStates.value = { ...workspaceStates.value, [agentId]: [] };
  }
}

async function loadMemory(agentId: string) {
  try {
    const body = await sidecarCall<{ files: MemoryFileEntry[] }>('ui_list_memory', {
      agent_id: agentId,
    });
    memoryFiles.value = { ...memoryFiles.value, [agentId]: body.files ?? [] };
  } catch (error) {
    memoryError.value = String((error as Error).message ?? error);
  }
}

async function openMemory(agentId: string, name: string) {
  try {
    const body = await sidecarCall<{ name: string; content: string }>('ui_read_memory', {
      agent_id: agentId,
      name,
    });
    memoryDrafts.value = {
      ...memoryDrafts.value,
      [agentId]: { name, content: body.content, original: body.content },
    };
  } catch (error) {
    memoryError.value = String((error as Error).message ?? error);
  }
}

async function saveMemory(agentId: string) {
  const draft = memoryDrafts.value[agentId];
  if (!draft) return;
  void withBusy(agentId, async () => {
    await sidecarCall('ui_write_memory', {
      agent_id: agentId,
      name: draft.name,
      content: draft.content,
    });
    memoryDrafts.value = { ...memoryDrafts.value, [agentId]: { ...draft, original: draft.content } };
    await loadMemory(agentId);
  });
}

async function deleteMemory(agentId: string, name: string) {
  if (!window.confirm(`确定删除记忆文件「${name}」？`)) return;
  void withBusy(agentId, async () => {
    await sidecarCall('ui_delete_memory', { agent_id: agentId, name });
    if (memoryDrafts.value[agentId]?.name === name) {
      const next = { ...memoryDrafts.value };
      delete next[agentId];
      memoryDrafts.value = next;
    }
    await loadMemory(agentId);
  });
}

async function compileMemory(agent: AgentSummary) {
  if (!sessionContext.value) {
    memoryError.value = '缺少当前会话上下文，无法发送整理请求';
    return;
  }
  void withBusy(agent.config.id, async () => {
    await sidecarCall<{ sent: boolean }>('ui_compile_memory', {
      agent_id: agent.config.id,
      session_id: sessionId.value,
      workspace: workspace.value,
    });
    memoryError.value = '';
    memoryNotice.value = '';
    // 整理请求已发送：由成员整理保存并经回报消息反馈结果，
    // 这里不再直接生成记忆文件。
    memoryNotice.value = `整理请求已发送给「${agent.config.name}」，结果将由成员回报。`;
  });
}

function newMemory(agentId: string) {
  memoryDrafts.value = {
    ...memoryDrafts.value,
    [agentId]: { name: '', content: '', original: '' },
  };
}

function memorySizeLabel(size: number): string {
  if (size < 1024) return `${size} B`;
  return `${(size / 1024).toFixed(1)} KB`;
}

function closeMemory(agentId: string) {
  const next = { ...memoryDrafts.value };
  delete next[agentId];
  memoryDrafts.value = next;
}

function sessionShort(agent: AgentSummary): string {
  const sessionId = agent.workspace_session_id ?? '';
  return sessionId ? `…${sessionId.slice(-8)}` : '';
}

onMounted(async () => {
  console.log('[subagent] mounted: initialHostContext =', JSON.stringify(props.initialHostContext?.session));
  applyContext(props.initialHostContext);
  if (props.subscribeHostContext) {
    stopHostContext = props.subscribeHostContext((context) => {
      applyContext(context);
      void refresh();
    });
  }
  await refresh();
  // 新对话的宿主上下文可能晚于页面挂载到达——若首次刷新失败（桥接
  // 未就绪），等上下文推送后再试一次。
  if (connectionError.value && props.subscribeHostContext) {
    await new Promise((resolve) => setTimeout(resolve, 500));
    if (!disposed) await refresh();
  }
  try {
    stopEvents = await subscribeSubagentEvents(() => scheduleRefresh());
  } catch {
    // 通知不可用时页面仍可手动刷新
  }
});

onUnmounted(() => {
  disposed = true;
  if (refreshTimer) clearTimeout(refreshTimer);
  stopEvents?.();
  stopHostContext?.();
});
</script>

<template>
  <div class="subagent-app">
    <header class="header">
      <div class="header-title">
        <h1>Subagent 管理</h1>
        <p class="session-line">
          <template v-if="sessionContext">
            当前会话 {{ sessionId.slice(0, 8) }}… · {{ workspace || '未指定 Workspace' }}
          </template>
          <template v-else>等待会话上下文…</template>
        </p>
      </div>
      <button class="btn btn-primary" type="button" @click="openCreate">新建 Subagent</button>
    </header>

    <p v-if="connectionError" class="error-line">{{ connectionError }}</p>

    <nav class="filters">
      <button
        v-for="item in FILTERS"
        :key="item.key"
        type="button"
        class="filter-chip"
        :class="{ active: filter === item.key }"
        @click="filter = item.key"
      >
        {{ item.label }}
        <span v-if="item.key === 'running' && runningCount" class="count">{{ runningCount }}</span>
        <span v-if="item.key === 'waiting' && waitingCount" class="count">{{ waitingCount }}</span>
      </button>
    </nav>

    <main class="content">
      <p v-if="!loaded" class="empty">正在连接 Subagent 服务…</p>
      <p v-else-if="!filteredAgents.length" class="empty">
        暂无 Subagent。点击右上角「新建 Subagent」创建第一个持久 Agent。
      </p>

      <div class="agents-grid">
      <section
        v-for="agent in filteredAgents"
        :key="agent.config.id"
        class="agent-card"
        :class="{ disabled: !agent.config.enabled }"
      >
        <div class="card-head" @click="toggleExpand(agent.config.id)">
          <div class="identity">
            <strong class="name">{{ agent.config.name }}</strong>
            <span class="tags">
              <span class="tag">{{ BACKEND_LABELS[agent.config.backend] }}</span>
              <span class="tag">{{ WORKSPACE_POLICY_LABELS[agent.config.workspace_policy] }}</span>
              <span v-if="!agent.config.enabled" class="tag tag-muted">已禁用</span>
              <span v-if="!BACKEND_IMPLEMENTED[agent.config.backend]" class="tag tag-muted">待接入</span>
            </span>
          </div>
          <span class="runtime" :class="runtimeStatusTone(agent)">● {{ runtimeStatusLabel(agent) }}</span>
        </div>

        <p class="description">{{ agent.config.description || '（无描述）' }}</p>

        <div class="status-row">
          <span class="status-item" :class="{ on: agent.config.enabled }">
            身份 · {{ agent.config.enabled ? '启用' : '禁用' }}
          </span>
          <span class="status-item" :class="{ on: agent.activated_in_session }">
            本会话 · {{ agent.activated_in_session ? '已激活' : '未激活' }}
          </span>
          <span
            v-if="agent.config.backend === 'tiangong_session' || agent.config.backend === 'agent_team'"
            class="status-item"
            :class="{ on: !!agent.workspace_session_id }"
          >
            本工作区 · {{ agent.workspace_session_id ? '已有会话' : '无会话' }}
          </span>
          <span class="status-item" :class="{ on: !!agent.activations.length }">
            活跃激活 {{ agent.activations.length }}
          </span>
        </div>

        <p v-if="agent.activated_in_session && sessionActivation(agent)" class="workspace-line">
          Workspace：{{ sessionActivation(agent)?.workspace }}
        </p>
        <p v-if="agent.workspace_session_id" class="workspace-line">
          本工作区会话：{{ sessionShort(agent) }}
        </p>

        <div class="actions">
          <button
            v-if="!agent.activated_in_session"
            class="btn"
            type="button"
            :disabled="busyAgentId === agent.config.id || !agent.config.enabled"
            @click="activate(agent)"
          >
            激活
          </button>
          <button
            v-else
            class="btn"
            type="button"
            :disabled="busyAgentId === agent.config.id"
            @click="deactivate(agent)"
          >
            停用
          </button>
          <button
            class="btn btn-primary"
            type="button"
            :disabled="busyAgentId === agent.config.id"
            @click="submitTask(agent)"
          >
            提交任务
          </button>
          <button
            v-if="agent.active_run_id && agent.runtime_status === 'working'"
            class="btn"
            type="button"
            :disabled="busyAgentId === agent.config.id"
            @click="interruptRun(agent)"
          >
            中断
          </button>
          <button
            v-if="agent.active_run_id && agent.runtime_status && ['working', 'blocked', 'approval_required', 'ready'].includes(agent.runtime_status)"
            class="btn btn-danger"
            type="button"
            :disabled="busyAgentId === agent.config.id"
            @click="cancelRun(agent)"
          >
            取消
          </button>
          <button class="btn btn-ghost" type="button" @click="openEdit(agent)">编辑</button>
          <button class="btn btn-ghost btn-danger-ghost" type="button" @click="removeAgent(agent)">删除</button>
        </div>

        <div
          class="message-row"
        >
          <input
            v-model="messageDraft[agent.config.id]"
            class="message-input"
            type="text"
            placeholder="发送消息（追问 / 补充背景 / 纠正方向）…"
            :disabled="busyAgentId === agent.config.id"
            @keydown.enter="sendMessage(agent)"
          />
          <button
            class="btn"
            type="button"
            :disabled="busyAgentId === agent.config.id || !(messageDraft[agent.config.id] ?? '').trim()"
            @click="sendMessage(agent)"
          >
            发送
          </button>
        </div>

        <div v-if="expandedAgentId === agent.config.id" class="detail">
          <div class="detail-block">
            <h3>进行中任务</h3>
            <p v-if="!tasksOf(agent).length" class="empty small">暂无</p>
            <ul v-else class="task-list">
              <li v-for="task in tasksOf(agent)" :key="task.task_id">
                <span class="task-status">{{ TASK_STATUS_LABELS[task.status] }}</span>
                <span class="task-goal">{{ task.goal }}</span>
              </li>
            </ul>
          </div>
          <div class="detail-block">
            <h3>最近事件</h3>
            <p v-if="!eventsOf(agent).length" class="empty small">暂无</p>
            <ul v-else class="event-list">
              <li v-for="event in eventsOf(agent)" :key="event.event_id">
                <span class="event-time">{{ event.created_at }}</span>
                <span class="event-type">{{ event.event_type }}</span>
                <span v-if="originLabel(event)" class="event-origin">{{ originLabel(event) }}</span>
                <span class="event-text">{{ eventText(event.payload) }}</span>
              </li>
            </ul>
          </div>
          <div v-if="(agent.instructions ?? '').trim()" class="detail-block">
            <h3>长期指令</h3>
            <p class="instructions-view">{{ agent.instructions }}</p>
          </div>
          <div class="detail-block">
            <h3>能力声明</h3>
            <p class="capability-line">
              消息 {{ agent.capabilities.messaging ? '✓' : '✗' }} ·
              任务 {{ agent.capabilities.tasks ? '✓' : '✗' }} ·
              流式 {{ agent.capabilities.streaming ? '✓' : '✗' }} ·
              中断 {{ agent.capabilities.interrupt ? '✓' : '✗' }} ·
              恢复 {{ agent.capabilities.resume ? '✓' : '✗' }} ·
              审批 {{ agent.capabilities.approval ? '✓' : '✗' }} ·
              写工作区 {{ agent.capabilities.workspace_write ? '✓' : '✗' }} ·
              产物 {{ agent.capabilities.artifacts ? '✓' : '✗' }}
            </p>
          </div>
          <div v-if="(workspaceStates[agent.config.id] ?? []).length" class="detail-block">
            <h3>工作区状态（成员自维护）</h3>
            <div v-for="ws in workspaceStates[agent.config.id]" :key="ws.workspace_id" class="ws-state">
              <p class="ws-path">{{ ws.paths?.[0] ?? ws.workspace_id }}</p>
              <p v-if="ws.plan" class="ws-section"><strong>规划</strong></p>
              <pre v-if="ws.plan" class="ws-content">{{ ws.plan }}</pre>
              <p v-if="ws.context" class="ws-section"><strong>背景</strong></p>
              <pre v-if="ws.context" class="ws-content">{{ ws.context }}</pre>
              <p v-if="ws.task" class="ws-section"><strong>当前工作</strong></p>
              <pre v-if="ws.task" class="ws-content">{{ ws.task }}</pre>
            </div>
          </div>
          <div class="detail-block">
            <div class="memory-head">
              <h3>长期记忆（memory/）</h3>
              <div class="memory-actions">
                <button
                  v-if="agent.config.backend === 'tiangong_session' || agent.config.backend === 'agent_team'"
                  class="btn btn-ghost"
                  type="button"
                  :disabled="busyAgentId === agent.config.id"
                  @click="compileMemory(agent)"
                >
                  从会话整理
                </button>
                <button class="btn btn-ghost" type="button" @click="newMemory(agent.config.id)">
                  新建文件
                </button>
              </div>
            </div>
            <p v-if="memoryError" class="session-hint">{{ memoryError }}</p>
            <p v-else-if="memoryNotice" class="session-hint">{{ memoryNotice }}</p>
            <p v-if="!(memoryFiles[agent.config.id] ?? []).length" class="empty small">
              暂无记忆文件（任务完成后自动归档结论；会话后端可「从会话整理」）
            </p>
            <ul v-else class="memory-list">
              <li v-for="file in memoryFiles[agent.config.id]" :key="file.name">
                <span class="memory-name" @click="openMemory(agent.config.id, file.name)">
                  {{ file.name }}
                </span>
                <span class="memory-meta">{{ memorySizeLabel(file.size_bytes) }} · {{ file.updated_at ?? '' }}</span>
                <button class="btn btn-ghost btn-danger-ghost" type="button" @click="deleteMemory(agent.config.id, file.name)">
                  删除
                </button>
              </li>
            </ul>
            <div v-if="memoryDrafts[agent.config.id]" class="memory-editor">
              <input
                v-model="memoryDrafts[agent.config.id]!.name"
                class="memory-name-input"
                type="text"
                placeholder="文件名（如 notes.md）"
                :disabled="Boolean(memoryDrafts[agent.config.id]!.original)"
              />
              <textarea
                v-model="memoryDrafts[agent.config.id]!.content"
                class="memory-content"
                rows="14"
                placeholder="记忆内容（markdown，可长文）…"
              />
              <div class="memory-editor-actions">
                <button
                  class="btn"
                  type="button"
                  :disabled="!memoryDrafts[agent.config.id]!.name.trim()"
                  @click="saveMemory(agent.config.id)"
                >
                  保存
                </button>
                <button
                  class="btn btn-ghost"
                  type="button"
                  @click="closeMemory(agent.config.id)"
                >
                  关闭
                </button>
              </div>
            </div>
          </div>
        </div>
      </section>
      </div>

      <section v-if="collaborationTimeline.length" class="collab-panel">
        <h2>协作时间线</h2>
        <p class="small muted">跨成员的关键事件按时间正序排列：谁发起、交给谁、结果回到谁。</p>
        <ul class="collab-list">
          <li v-for="event in collaborationTimeline" :key="event.event_id">
            <span class="event-time">{{ event.created_at }}</span>
            <span class="collab-from">{{ event.from }}</span>
            <span class="collab-arrow">→</span>
            <span class="collab-target">{{ event.target }}</span>
            <span class="event-type">{{ COLLAB_EVENT_LABELS[event.event_type] ?? event.event_type }}</span>
            <span class="event-text">{{ event.summary.slice(0, 120) }}</span>
          </li>
        </ul>
      </section>

      <section class="collab-panel mcp-panel">
        <h2>外部工具接入（MCP）</h2>
        <p class="small muted">
          Claude Code、Codex 等支持 MCP 的 agent 工具可直接接入 Subagent 总线：成员、任务、协作关系与天工内完全一致。
          回报投递需要本机 Server 已开启（默认 9090，以实际配置为准）。
        </p>
        <pre class="mcp-config">{{ mcpConfigSnippet }}</pre>
        <button class="btn btn-ghost" type="button" @click="copyMcpConfig">{{ mcpCopied ? '已复制' : '复制配置' }}</button>
      </section>
    </main>

    <AgentForm
      v-if="formVisible"
      :initial="formInitial"
      :error="formError"
      @submit="submitForm"
      @cancel="formVisible = false"
    />
  </div>
</template>

<style scoped>
.subagent-app {
  --ui-background: hsl(var(--background, 0 0% 100%));
  --ui-foreground: hsl(var(--foreground, 222.2 47.4% 11.2%));
  --ui-card: hsl(var(--card, 0 0% 100%));
  --ui-muted-foreground: hsl(var(--muted-foreground, 215.4 16.3% 46.9%));
  --ui-border: hsl(var(--border, 214.3 31.8% 91.4%));
  --ui-primary: hsl(var(--primary, 222.2 47.4% 11.2%));
  --ui-primary-foreground: hsl(var(--primary-foreground, 210 40% 98%));
  --ui-destructive: hsl(var(--destructive, 0 84.2% 60.2%));
  --ui-input: hsl(var(--input, 214.3 31.8% 91.4%));

  box-sizing: border-box;
  display: flex;
  flex-direction: column;
  gap: 14px;
  width: 100%;
  height: 100%;
  padding: 18px 20px;
  overflow-y: auto;
  color: var(--ui-foreground);
  background: var(--ui-background);
  font-size: 13px;
}

.header {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 12px;
}

.header-title h1 {
  margin: 0;
  font-size: 17px;
  font-weight: 600;
}

.session-line {
  margin: 4px 0 0;
  color: var(--ui-muted-foreground);
  font-size: 12px;
}

.error-line {
  margin: 0;
  padding: 8px 10px;
  border: 1px solid var(--ui-destructive);
  border-radius: 8px;
  color: var(--ui-destructive);
  font-size: 12px;
}

.filters {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.filter-chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 4px 10px;
  border: 1px solid var(--ui-border);
  border-radius: 999px;
  background: transparent;
  color: var(--ui-muted-foreground);
  font-size: 12px;
  cursor: pointer;
}

.filter-chip.active {
  border-color: var(--ui-primary);
  color: var(--ui-foreground);
  font-weight: 600;
}

.filter-chip .count {
  padding: 0 6px;
  border-radius: 999px;
  background: var(--ui-primary);
  color: var(--ui-primary-foreground);
  font-size: 11px;
}

.content {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.empty {
  margin: 24px 0;
  color: var(--ui-muted-foreground);
  text-align: center;
}

.empty.small {
  margin: 4px 0;
  text-align: left;
}

.agents-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
  align-items: start;
  gap: 10px;
}

.agent-card {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 10px;
  border: 1px solid var(--ui-border);
  border-radius: 10px;
  background: var(--ui-card);
}

.agent-card.disabled {
  opacity: 0.6;
}

.card-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 8px;
  cursor: pointer;
}

.identity {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.name {
  font-size: 13px;
}

.tags {
  display: flex;
  flex-wrap: wrap;
  gap: 3px;
}

.tag {
  padding: 0 6px;
  border: 1px solid var(--ui-border);
  border-radius: 999px;
  color: var(--ui-muted-foreground);
  font-size: 10px;
}

.tag-muted {
  font-style: italic;
}

.runtime {
  flex-shrink: 0;
  font-size: 11px;
  white-space: nowrap;
}

.tone-idle { color: var(--ui-muted-foreground); }
.tone-working { color: hsl(var(--primary, 222.2 47.4% 11.2%)); }
.tone-waiting { color: hsl(38 92% 50%); }
.tone-failed { color: var(--ui-destructive); }
.tone-completed { color: hsl(142 71% 45%); }

.description {
  margin: 0;
  color: var(--ui-muted-foreground);
  font-size: 12px;
  overflow: hidden;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
}

.status-row {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  font-size: 11px;
}

.status-item {
  color: var(--ui-muted-foreground);
}

.status-item.on {
  color: var(--ui-foreground);
  font-weight: 600;
}

.workspace-line {
  margin: 0;
  color: var(--ui-muted-foreground);
  font-size: 11px;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.actions {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
}

.btn {
  padding: 3px 9px;
  border: 1px solid var(--ui-border);
  border-radius: 7px;
  background: transparent;
  color: var(--ui-foreground);
  font-size: 11px;
  cursor: pointer;
}

.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.btn-primary {
  border-color: var(--ui-primary);
  background: var(--ui-primary);
  color: var(--ui-primary-foreground);
}

.btn-danger {
  border-color: var(--ui-destructive);
  color: var(--ui-destructive);
}

.btn-ghost {
  border-color: transparent;
  color: var(--ui-muted-foreground);
}

.btn-danger-ghost {
  color: var(--ui-destructive);
}

.message-row {
  display: flex;
  gap: 6px;
}

.message-input {
  flex: 1;
  min-width: 0;
  padding: 4px 8px;
  border: 1px solid var(--ui-input);
  border-radius: 7px;
  background: transparent;
  color: var(--ui-foreground);
  font-size: 11px;
}

.detail {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 10px;
  border-top: 1px dashed var(--ui-border);
}

.instructions-view {
  margin: 0;
  font-size: 12px;
  color: var(--ui-muted-foreground);
  white-space: pre-wrap;
  word-break: break-word;
  max-height: 180px;
  overflow-y: auto;
}

.detail-block h3 {
  margin: 0 0 6px;
  font-size: 12px;
  color: var(--ui-muted-foreground);
}

.task-list,
.event-list {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.task-list li,
.event-list li {
  display: flex;
  gap: 8px;
  font-size: 12px;
}

.event-origin {
  flex-shrink: 0;
  color: var(--ui-primary, #2563eb);
}

.mcp-config {
  margin: 8px 0;
  padding: 10px 12px;
  border: 1px solid var(--ui-border);
  border-radius: 8px;
  background: var(--ui-background, #f7f7f8);
  font-size: 11px;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  white-space: pre-wrap;
  word-break: break-all;
  user-select: text;
}

.collab-panel {
  margin-top: 16px;
  padding: 12px 16px;
  border: 1px solid var(--ui-border, rgba(0, 0, 0, 0.1));
  border-radius: 10px;
}

.collab-list {
  list-style: none;
  margin: 8px 0 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.collab-list li {
  display: flex;
  gap: 8px;
  font-size: 12px;
  align-items: baseline;
}

.collab-from {
  flex-shrink: 0;
  font-weight: 600;
}

.collab-arrow {
  color: var(--ui-muted-foreground);
}

.collab-target {
  flex-shrink: 0;
}

.muted {
  color: var(--ui-muted-foreground);
}

.small {
  font-size: 12px;
}

.task-status {
  flex-shrink: 0;
  color: var(--ui-muted-foreground);
}

.task-goal {
  word-break: break-all;
}

.event-time {
  flex-shrink: 0;
  color: var(--ui-muted-foreground);
}

.event-type {
  flex-shrink: 0;
  min-width: 84px;
  color: var(--ui-muted-foreground);
}

.event-text {
  word-break: break-all;
}

.capability-line {
  margin: 0;
  color: var(--ui-muted-foreground);
  font-size: 12px;
}

.memory-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.memory-head h3 {
  margin: 0;
}

.memory-actions {
  display: flex;
  gap: 6px;
}

.memory-list {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 6px 0 0;
  padding: 0;
  list-style: none;
}

.memory-list li {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12px;
}

.memory-name {
  color: var(--ui-foreground);
  cursor: pointer;
  text-decoration: underline dotted;
}

.memory-meta {
  flex: 1;
  color: var(--ui-muted-foreground);
}

.memory-editor {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin-top: 10px;
}

.ws-state {
  border: 1px solid var(--ui-border);
  border-radius: 8px;
  padding: 8px;
  margin-bottom: 8px;
}

.ws-path {
  margin: 0 0 4px;
  font-size: 11px;
  color: var(--ui-muted-foreground);
  word-break: break-all;
}

.ws-section {
  margin: 6px 0 2px;
  font-size: 11px;
}

.ws-content {
  margin: 0;
  padding: 6px 8px;
  border-radius: 6px;
  background: var(--ui-background, #f7f7f8);
  font-size: 11px;
  font-family: inherit;
  white-space: pre-wrap;
  word-break: break-word;
  max-height: 140px;
  overflow-y: auto;
}

.memory-name-input,
.memory-content {
  padding: 7px 10px;
  border: 1px solid var(--ui-input);
  border-radius: 8px;
  background: transparent;
  color: var(--ui-foreground);
  font-size: 12px;
}

.memory-content {
  min-height: 240px;
  resize: vertical;
  font-family: inherit;
  line-height: 1.6;
}

.memory-editor-actions {
  display: flex;
  justify-content: flex-end;
  gap: 6px;
}
</style>
