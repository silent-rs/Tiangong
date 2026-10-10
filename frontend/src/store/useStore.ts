import { create } from 'zustand';
import { api, textContent } from '../api/tauri';
import { isUserInput, messageKind, reasoningOf, toolCallIdOf, turnStatusOf } from '../api/message';
import type {
  ContentBlock,
  LoadedSession,
  Message,
  RawAttachment,
  SandboxUpdateState,
  SessionStreamEvent,
  Session,
  InputCache,
  StreamEvent,
  TokenStats,
  TurnStatus,
  UserOutlineItem,
} from '../api/tauri';
import { notifyBackgroundSessionCompleted } from '../utils/desktopNotification';
import {
  cloneInputCache,
  emptyInputCache,
  getInputCache,
  mergeStoredInputCache,
  setInputCacheSending,
  settleInputCacheSend,
  updateInputCacheAttachments,
  updateInputCacheText,
  type InputCacheMap,
} from './inputCache';

let switchRequestVersion = 0;
let newConversationRequestVersion = 0;
const inputCacheSyncRequestVersions = new Map<string, number>();
let switchCommitQueue: Promise<void> = Promise.resolve();
// loadSessions 的请求版本：旧请求晚到时放弃写入，避免覆盖较新的权威结果。
let loadSessionsRequestVersion = 0;
// 普通刷新的 in-flight 计数：任一普通请求未结束时保持 loading，
// protective 刷新不参与计数，避免它提前清除普通刷新持有的 loading。
let ordinaryLoadInFlight = 0;

interface SessionViewCache {
  hydrated: boolean;
  messages: Message[];
  /** 已加载消息的起点在完整历史中的下标；0 表示已加载全部历史。 */
  historyStart: number;
  /** 完整的用户提问目录（加载会话时由后端给出）；null 表示未知，按已加载消息推算。 */
  userOutline: UserOutlineItem[] | null;
  runStatus: string;
  runSummary: string;
  contextManagementPending: boolean;
  tokenStats: TokenStats | null;
  lastUsage: AppState['lastUsage'];
  lastDurationMs: number | null;
  /** 运行中工具调用的开始时刻（tool_call_id → Date.now()），用于运行行实时跳秒。 */
  toolCallStartedAt: Record<string, number>;
  /** 已执行完成、结果尚待按序提交的工具调用（tool_call_id → 结论与耗时）。 */
  toolCallFinished: Record<string, ToolCallFinished>;
  streamingMessageId: string | null;
  streamingContent: string;
  streamingReasoningContent: string;
  cwd: string;
  reasoningEffort: string;
}

/** 工具任务已完成但结果尚未按序提交时的展示信息。 */
export interface ToolCallFinished {
  ok: boolean;
  durationMs: number | null;
}

const sessionViewCaches = new Map<string, SessionViewCache>();
// 正在进行的向前分段加载（同一时刻只发一个请求，后来者复用其结果）。
let olderMessagesInFlight: Promise<boolean> | null = null;

/** 输入队列消息：执行期间暂存、等待空闲后按序投递的用户输入（含附件快照）。 */
export interface QueuedInputMessage {
  id: string;
  text: string;
  attachments: RawAttachment[];
  queuedAt: number;
}

function newQueuedMessageId(): string {
  return `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

/** 直接由文本构造一条队列消息（不触碰草稿——用于外部文本在发送事务
 *  或执行中投递：草稿可能属于进行中的提交，不得被再次入队或覆盖）。 */
function queuedTextMessage(text: string, attachments: RawAttachment[] = []): QueuedInputMessage {
  return {
    id: newQueuedMessageId(),
    text,
    attachments: attachments.map((attachment) => ({ ...attachment })),
    queuedAt: Date.now(),
  };
}

/** 把当前草稿快照为一条队列消息（不修改草稿）。 */
function queuedMessageFromCache(cache: InputCache): QueuedInputMessage {
  return {
    id: newQueuedMessageId(),
    text: cache.text,
    attachments: cache.attachments.map((attachment) => ({ ...attachment })),
    queuedAt: Date.now(),
  };
}

function commitSessionSwitch(sessionId: string, requestVersion: number): Promise<boolean> {
  const commit = switchCommitQueue.then(async () => {
    if (requestVersion !== switchRequestVersion) return false;
    await api.switchSession(sessionId);
    return requestVersion === switchRequestVersion;
  });
  switchCommitQueue = commit.then(() => undefined, () => undefined);
  return commit;
}

interface InputCacheSyncQueue {
  pending: InputCache | null;
  pendingVersion: number;
  claimed: {
    cache: InputCache;
    version: number;
    revision: number;
  } | null;
  running: boolean;
  debounceVersion: number;
  nextVersion: number;
  immediateThroughVersion: number;
  waiters: Array<{
    version: number;
    resolve: (cache: InputCache) => void;
    reject: (error: unknown) => void;
  }>;
}

const inputCacheSyncQueues = new Map<string, InputCacheSyncQueue>();

function sameAttachments(left: RawAttachment[], right: RawAttachment[]): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function rebasePendingInputCacheOnStored(
  pending: InputCache,
  submitted: InputCache,
  stored: InputCache,
): InputCache {
  return {
    text: pending.text === submitted.text ? stored.text : pending.text,
    attachments: sameAttachments(pending.attachments, submitted.attachments)
      ? stored.attachments.map((attachment) => ({ ...attachment }))
      : pending.attachments,
    is_sending: pending.is_sending,
    revision: pending.revision,
  };
}

function waitForInputCacheDebounce(): Promise<void> {
  return new Promise((resolve) => globalThis.setTimeout(resolve, 200));
}

function discardInputCacheSyncQueue(cacheKey: string): void {
  const queue = inputCacheSyncQueues.get(cacheKey);
  if (queue) {
    for (const waiter of queue.waiters) waiter.resolve(emptyInputCache());
    queue.waiters = [];
  }
  inputCacheSyncQueues.delete(cacheKey);
  inputCacheSyncRequestVersions.delete(cacheKey);
}

function syncInputCacheInBackground(promise: Promise<InputCache>): void {
  void promise.catch(() => undefined);
}

// ---------------------------------------------------------------------------
// Agent 信息（从系统消息解析）
// ---------------------------------------------------------------------------

export interface AgentInfo {
  agentId?: string;
  role: string;
  label: string;
  status: 'idle' | 'running' | 'waiting_for_user' | 'waiting_for_lock' | 'terminated' | 'error';
}

/** 从系统消息中解析 Agent 列表 */
export function parseAgentsFromMessages(messages: Message[]): AgentInfo[] {
  const agents = new Map<string, AgentInfo>();
  for (const msg of messages) {
    if (messageKind(msg) !== 'system') continue;
    const text = textContent(msg);
    // [Agent] {label} ({role}) 已加入团队
    const createMatch = text.match(/^\[Agent\] (.+?) \((.+?)\) 已加入团队.*?id=([^\s]+)/);
    if (createMatch) {
      const [, label, role, agentId] = createMatch;
      agents.set(role, { agentId, role, label, status: 'idle' });
      continue;
    }
    // [Agent] {label} 状态变更: {status}
    const statusMatch = text.match(/^\[Agent\] (.+?) 状态变更: (\w+).*?id=([^\s]+)/);
    if (statusMatch) {
      const [, , status, agentId] = statusMatch;
      if (status === 'terminated') {
        for (const [role, info] of agents) {
          if (info.agentId === agentId) {
            agents.delete(role);
            break;
          }
        }
      } else if (
        status === 'idle'
        || status === 'running'
        || status === 'waiting_for_user'
        || status === 'waiting_for_lock'
        || status === 'error'
      ) {
        for (const [, info] of agents) {
          if (info.agentId === agentId) {
            info.status = status;
            break;
          }
        }
      }
    }
  }
  return Array.from(agents.values());
}

function isAgentSystemMessage(message: Message): boolean {
  if (messageKind(message) !== 'system') return false;
  const text = textContent(message);
  return text.startsWith('[Agent]') || text.startsWith('[文件锁]');
}

function sameJsonValue(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (left == null || right == null) return left == right;
  return JSON.stringify(left) === JSON.stringify(right);
}

function sameMessage(left: Message, right: Message): boolean {
  return left.id === right.id
    && left.created_at === right.created_at
    && sameJsonValue(left.role, right.role)
    && sameJsonValue(left.content, right.content)
    && sameJsonValue(left.meta, right.meta);
}

function mergeLoadedWithStreamMessages(
  loadedMessages: Message[],
  streamMessages: Message[],
): Message[] {
  if (streamMessages.length === 0) return loadedMessages;
  const streamById = new Map(streamMessages.map((message) => [message.id, message]));
  const loadedIds = new Set(loadedMessages.map((message) => message.id));
  const merged = loadedMessages.map((loaded) => {
    const streamed = streamById.get(loaded.id);
    if (!streamed) return loaded;
    const loadedText = textContent(loaded);
    const streamedText = textContent(streamed);
    const content = streamedText.length >= loadedText.length ? streamed.content : loaded.content;
    if (loaded.role?.type === 'assistant' && streamed.role?.type === 'assistant') {
      const loadedReasoning = loaded.role.reasoning_content ?? '';
      const streamedReasoning = streamed.role.reasoning_content ?? '';
      return {
        ...loaded,
        content,
        role: {
          ...loaded.role,
          reasoning_content: streamedReasoning.length >= loadedReasoning.length
            ? streamedReasoning
            : loadedReasoning,
          tool_calls: streamed.role.tool_calls?.length ? streamed.role.tool_calls : loaded.role.tool_calls,
          usage: streamed.role.usage ?? loaded.role.usage,
        },
      };
    }
    // 用户锚点：流式快照可能带最新的 final_reply / 轮次状态，以流式为准。
    return { ...loaded, content, role: streamed.role ?? loaded.role };
  });
  for (const streamed of streamMessages) {
    if (!loadedIds.has(streamed.id)) merged.push(streamed);
  }
  return merged;
}

function upsertStreamMessage(messages: Message[], message: Message): Message[] {
  const index = messages.findIndex((item) => item.id === message.id);
  if (index < 0) return [...messages, message];
  if (sameMessage(messages[index], message)) return messages;
  const next = [...messages];
  next[index] = message;
  return next;
}

function updateAssistantMessage(
  messages: Message[],
  messageId: string,
  update: (message: Message) => Message,
): Message[] {
  const index = messages.findIndex((message) => message.id === messageId);
  const current: Message = index >= 0 ? messages[index] : {
    id: messageId,
    role: { type: 'assistant' },
    content: [],
    created_at: new Date().toISOString(),
  };
  const nextMessage = update(current);
  if (index < 0) return [...messages, nextMessage];
  const next = [...messages];
  next[index] = nextMessage;
  return next;
}

function appendAssistantText(messages: Message[], event: StreamEvent): Message[] {
  if (!event.message_id || event.content == null) return messages;
  return updateAssistantMessage(messages, event.message_id, (message) => {
    const content = Array.isArray(message.content) ? [...message.content] : [];
    const last = content[content.length - 1];
    if (last?.type === 'text') {
      content[content.length - 1] = { ...last, text: `${last.text}${event.content}` };
    } else if (event.content) {
      content.push({ type: 'text', text: event.content });
    }
    return { ...message, content };
  });
}

function appendAssistantReasoning(messages: Message[], event: StreamEvent): Message[] {
  if (!event.message_id || event.content == null) return messages;
  return updateAssistantMessage(messages, event.message_id, (message) => {
    if (message.role?.type !== 'assistant') return message;
    return {
      ...message,
      role: {
        ...message.role,
        reasoning_content: `${message.role.reasoning_content || ''}${event.content}`,
      },
    };
  });
}

function applyUserMessage(messages: Message[], event: StreamEvent): Message[] {
  if (!event.message_id) return messages;
  const blocks = event.content_blocks && event.content_blocks.length > 0
    ? event.content_blocks
    : [
        ...(event.content ? [{ type: 'text' as const, text: event.content }] : []),
        ...(event.media || []).map((media) => ({
          type: 'media' as const,
          kind: media.kind,
          url: media.url,
          mime_type: media.mime_type,
          title: media.title,
        })),
      ];
  const existingIndex = messages.findIndex((message) => message.id === event.message_id);
  const existing = existingIndex >= 0 ? messages[existingIndex] : undefined;
  const turnBase = existingIndex >= 0 ? messages.slice(0, existingIndex) : messages;
  // 与后端同一规则：最近的起轮消息仍为 processing 时是引导消息（不带状态），
  // 否则起新轮。已加载的同 ID 消息以其持久化状态为准。
  const existingStatus = existing ? turnStatusOf(existing) : undefined;
  const turnStatus = existingStatus != null
    ? existingStatus
    : latestTurnStatus(turnBase) === 'processing' ? undefined : 'processing' as const;
  return upsertStreamMessage(existingIndex >= 0 ? messages.slice(0, existingIndex + 1) : messages, {
    id: event.message_id,
    role: {
      ...(existing?.role?.type === 'user' ? existing.role : { type: 'user' as const }),
      ...(turnStatus != null ? { turn_status: turnStatus } : {}),
    },
    content: blocks,
    created_at: existing?.created_at || new Date().toISOString(),
    ...(existing?.meta ? { meta: existing.meta } : {}),
  });
}

/** 最近一个带轮次状态的用户消息（起轮消息）的状态。 */
function latestTurnStatus(messages: Message[]): TurnStatus | undefined {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const status = turnStatusOf(messages[index]);
    if (status != null) return status;
  }
  return undefined;
}

function applyToolCalls(messages: Message[], event: StreamEvent): Message[] {
  if (!event.message_id) return messages;
  return updateAssistantMessage(messages, event.message_id, (message) => (
    message.role?.type === 'assistant'
      ? { ...message, role: { ...message.role, tool_calls: event.calls || [] } }
      : message
  ));
}

function applyToolResult(messages: Message[], event: StreamEvent): Message[] {
  const toolCallId = event.tool_call_id || undefined;
  const existingIndex = toolCallId
    ? messages.findIndex((message) => toolCallIdOf(message) === toolCallId)
    : -1;
  const id = existingIndex >= 0
    ? messages[existingIndex].id
    : `stream-tool-result:${toolCallId || `${event.name || 'tool'}:${messages.length}`}`;
  const message: Message = {
    id,
    role: {
      type: 'tool',
      tool_call_id: toolCallId,
      tool_name: event.name,
      is_error: event.ok === false,
      duration_ms: event.duration_ms ?? undefined,
    },
    content: [{ type: 'text', text: event.output || '' }],
    created_at: existingIndex >= 0
      ? messages[existingIndex].created_at
      : new Date().toISOString(),
  };
  if (existingIndex < 0) return [...messages, message];
  const next = [...messages];
  next[existingIndex] = message;
  return next;
}

function applyAgentLifecycle(messages: Message[], event: StreamEvent): Message[] {
  if (!event.agent_id || !event.label) return messages;
  const isCreated = event.type === 'agent_created';
  const id = isCreated ? `agent-created:${event.agent_id}` : `agent-status:${event.agent_id}`;
  const text = isCreated
    ? `[Agent] ${event.label} (${event.role || ''}) 已加入团队 id=${event.agent_id}`
    : `[Agent] ${event.label} 状态变更: ${event.status || ''} id=${event.agent_id}`;
  return upsertStreamMessage(messages, {
    id,
    role: { type: 'system' },
    content: [{ type: 'text', text }],
    created_at: new Date().toISOString(),
  });
}

/** 终态事件到达时更新本轮起轮消息（`processing` 状态的用户消息）。
 *  后端在终态前已推送带终态的起轮消息快照，这里只是本地兜底。 */
function updateProcessingTurn(
  messages: Message[],
  status: 'success' | 'failed' | 'cancelled',
  elapsedMs: number | null,
): Message[] {
  let index = -1;
  for (let candidate = messages.length - 1; candidate >= 0; candidate -= 1) {
    if (turnStatusOf(messages[candidate]) === 'processing') {
      index = candidate;
      break;
    }
  }
  if (index < 0) return messages;
  const next = [...messages];
  const target = next[index];
  if (target.role?.type !== 'user') return messages;
  next[index] = {
    ...target,
    role: {
      ...target.role,
      turn_status: status,
      elapsed_ms: elapsedMs ?? target.role.elapsed_ms,
    },
  };
  return next;
}

function applyTokenUsage(stats: TokenStats | null, event: StreamEvent): TokenStats | null {
  if (!event.usage) return stats;
  const usage = event.usage;
  const total = usage.total_tokens || usage.prompt_tokens + usage.completion_tokens;
  const next: TokenStats = stats ? {
    ...stats,
    agent_current_tokens: { ...stats.agent_current_tokens },
    agent_token_usage: { ...stats.agent_token_usage },
  } : {
    current_tokens: 0,
    compression_threshold_tokens: event.compression_threshold_tokens || 0,
    context_limit_tokens: event.context_limit_tokens || 0,
    total_prompt_tokens: 0,
    total_completion_tokens: 0,
    total_tokens: 0,
    active_agent_current_tokens: 0,
    active_agent_id: null,
    agent_current_tokens: {},
    agent_token_usage: {},
  };
  next.total_prompt_tokens += usage.prompt_tokens;
  next.total_completion_tokens += usage.completion_tokens;
  next.total_tokens += total;
  if (event.compression_threshold_tokens != null) {
    next.compression_threshold_tokens = event.compression_threshold_tokens;
  }
  if (event.context_limit_tokens != null) {
    next.context_limit_tokens = event.context_limit_tokens;
  }
  if (event.agent_id) {
    const previous = next.agent_token_usage[event.agent_id] || {
      prompt_tokens: 0,
      completion_tokens: 0,
      total_tokens: 0,
    };
    next.agent_token_usage[event.agent_id] = {
      prompt_tokens: previous.prompt_tokens + usage.prompt_tokens,
      completion_tokens: previous.completion_tokens + usage.completion_tokens,
      total_tokens: previous.total_tokens + total,
    };
    next.active_agent_id = event.agent_id;
    if (event.current_tokens != null) {
      next.active_agent_current_tokens = event.current_tokens;
      next.agent_current_tokens[event.agent_id] = event.current_tokens;
    }
  } else if (event.current_tokens != null) {
    next.current_tokens = event.current_tokens;
  }
  return next;
}

function emptySessionViewCache(runStatus = 'idle'): SessionViewCache {
  return {
    hydrated: false,
    messages: [],
    historyStart: 0,
    userOutline: null,
    runStatus,
    runSummary: runStatus === 'idle' ? '' : '正在处理',
    contextManagementPending: false,
    tokenStats: null,
    lastUsage: null,
    lastDurationMs: null,
    toolCallStartedAt: {},
    toolCallFinished: {},
    streamingMessageId: null,
    streamingContent: '',
    streamingReasoningContent: '',
    cwd: '',
    reasoningEffort: 'medium',
  };
}

function sessionViewCacheFromState(state: AppState): SessionViewCache {
  return {
    hydrated: !!state.activeSessionId,
    messages: state.messages,
    historyStart: state.historyStart,
    userOutline: state.userOutline,
    runStatus: state.runStatus,
    runSummary: state.runSummary,
    contextManagementPending: false,
    tokenStats: state.tokenStats,
    lastUsage: state.lastUsage,
    lastDurationMs: state.lastDurationMs,
    toolCallStartedAt: state.toolCallStartedAt,
    toolCallFinished: state.toolCallFinished,
    streamingMessageId: state.streamingMessageId,
    streamingContent: state.streamingContent,
    streamingReasoningContent: state.streamingReasoningContent,
    cwd: state.sessionCwd,
    reasoningEffort: state.reasoningEffort,
  };
}

function hydrateSessionViewCache(
  current: SessionViewCache,
  loaded: LoadedSession,
): SessionViewCache {
  const messages = mergeLoadedWithStreamMessages(loaded.messages, current.messages);
  const cacheHasNewerUsage = !!current.tokenStats
    && current.tokenStats.total_tokens >= loaded.token_stats.total_tokens;
  return {
    ...current,
    hydrated: true,
    messages,
    historyStart: loaded.start ?? 0,
    userOutline: loaded.user_outline ?? null,
    tokenStats: cacheHasNewerUsage && current.tokenStats
      ? current.tokenStats
      : loaded.token_stats,
    lastUsage: cacheHasNewerUsage
      ? current.lastUsage ?? loaded.last_usage ?? null
      : loaded.last_usage ?? current.lastUsage,
    lastDurationMs: current.lastDurationMs ?? loaded.last_duration_ms ?? null,
    cwd: loaded.cwd,
    reasoningEffort: loaded.reasoning_effort,
  };
}

function applyEventToSessionView(
  current: SessionViewCache,
  event: StreamEvent,
): SessionViewCache {
  let messages = current.messages;
  let runStatus = current.runStatus;
  let runSummary = current.runSummary;
  let contextManagementPending = current.contextManagementPending;
  let tokenStats = current.tokenStats;
  let lastUsage = current.lastUsage;
  let lastDurationMs = current.lastDurationMs;
  let toolCallStartedAt = current.toolCallStartedAt;
  let toolCallFinished = current.toolCallFinished;
  let streamingMessageId = current.streamingMessageId;
  let streamingContent = current.streamingContent;
  let streamingReasoningContent = current.streamingReasoningContent;

  // title_changed 是纯通知事件，不改变对话运行状态（自动/手动标题变更都会发）。
  if (
    event.type !== 'done'
    && event.type !== 'error'
    && event.type !== 'title_changed'
    && runStatus === 'idle'
  ) {
    runStatus = 'executing';
  }

  switch (event.type) {
    case 'user_message':
      messages = applyUserMessage(messages, event);
      runStatus = 'executing';
      runSummary = '正在处理';
      lastDurationMs = null;
      toolCallStartedAt = {};
      toolCallFinished = {};
      break;
    case 'delta':
    case 'react_text':
    case 'summary_text':
      messages = appendAssistantText(messages, event);
      streamingMessageId = event.message_id || null;
      if (streamingMessageId) {
        const message = messages.find((item) => item.id === streamingMessageId);
        streamingContent = message ? textContent(message) : streamingContent;
        streamingReasoningContent = message ? reasoningOf(message) : '';
      }
      runStatus = 'executing';
      runSummary = '正在回复...';
      break;
    case 'reasoning':
      messages = appendAssistantReasoning(messages, event);
      streamingMessageId = event.message_id || null;
      if (streamingMessageId) {
        const message = messages.find((item) => item.id === streamingMessageId);
        streamingContent = message ? textContent(message) : streamingContent;
        streamingReasoningContent = message ? reasoningOf(message) : '';
      }
      runStatus = 'executing';
      runSummary = '正在思考...';
      break;
    case 'session_message_upsert':
      if (event.message && typeof event.message !== 'string') {
        messages = upsertStreamMessage(messages, event.message);
        if (streamingMessageId === event.message.id) {
          streamingContent = textContent(event.message);
          streamingReasoningContent = reasoningOf(event.message);
        }
      }
      break;
    case 'tool_calls':
      messages = applyToolCalls(messages, event);
      // 记录本批调用的开始时刻，运行行据此实时跳秒；结果到达时移除。
      {
        const now = Date.now();
        const startedAt = { ...toolCallStartedAt };
        for (const call of event.calls || []) {
          if (call?.id) startedAt[call.id] = now;
        }
        toolCallStartedAt = startedAt;
      }
      streamingMessageId = null;
      streamingContent = '';
      streamingReasoningContent = '';
      runStatus = 'executing';
      runSummary = `正在执行：${(event.names || []).join(', ')}`;
      break;
    case 'tool_start':
      runStatus = 'executing';
      runSummary = event.args_summary
        ? `正在执行：${event.name || ''} ${event.args_summary}`
        : `正在执行：${event.name || ''}`;
      break;
    case 'tool_finished':
      // 任务已完成、结果待按序提交：运行行停止计时并显示真实耗时。
      if (event.tool_call_id) {
        toolCallFinished = {
          ...toolCallFinished,
          [event.tool_call_id]: {
            ok: event.ok !== false,
            durationMs: event.duration_ms ?? null,
          },
        };
      }
      break;
    case 'tool_result':
      messages = applyToolResult(messages, event);
      if (event.tool_call_id && toolCallStartedAt[event.tool_call_id] != null) {
        const { [event.tool_call_id]: _removed, ...rest } = toolCallStartedAt;
        toolCallStartedAt = rest;
      }
      if (event.tool_call_id && toolCallFinished[event.tool_call_id] != null) {
        const { [event.tool_call_id]: _finished, ...rest } = toolCallFinished;
        toolCallFinished = rest;
      }
      runSummary = `${event.ok === false ? '失败' : '完成'} ${event.name || ''}`.trim();
      break;
    case 'token_usage':
      tokenStats = applyTokenUsage(tokenStats, event);
      if (tokenStats) {
        lastUsage = {
          prompt_tokens: tokenStats.total_prompt_tokens,
          completion_tokens: tokenStats.total_completion_tokens,
          total_tokens: tokenStats.total_tokens,
        };
      }
      break;
    case 'retry':
      runStatus = 'executing';
      runSummary = `重试中 (${event.attempt || 0}/${event.max_attempts || 0})...`;
      break;
    case 'phase_changed':
      if (event.phase === 'analyzing') runSummary = '正在思考...';
      else if (event.phase === 'summary') runSummary = '正在整理回复...';
      break;
    case 'agent_created':
    case 'agent_status_changed':
      messages = applyAgentLifecycle(messages, event);
      runSummary = event.type === 'agent_created'
        ? `Agent ${event.label || ''} 已加入团队`
        : `Agent ${event.label || ''}: ${event.status || ''}`;
      break;
    case 'memory_recall_start':
      runSummary = '正在检索记忆...';
      break;
    case 'memory_recall_progress':
      runSummary = `正在检索记忆: ${event.phase || ''}`;
      break;
    case 'memory_recall_done':
      runSummary = event.hit_count
        ? `记忆检索完成，命中 ${event.hit_count} 条`
        : '记忆检索完成，无相关记忆';
      break;
    case 'model_switch_started':
      runStatus = 'executing';
      runSummary = `正在切换模型：${event.model_name || ''}`;
      break;
    case 'model_switched':
      runStatus = 'executing';
      runSummary = `已切换至 ${event.model_name || ''}`;
      break;
    case 'context_compressing':
      runStatus = 'executing';
      runSummary = '正在压缩早期上下文...';
      break;
    case 'context_compressed':
      if (event.action !== 'cancelled') {
        runSummary = event.action === 'clear'
          ? '上下文清理'
          : event.action === 'failed'
            ? '上下文压缩失败'
            : event.action === 'noop'
              ? '上下文无需压缩'
              : '上下文压缩';
      }
      if (tokenStats && event.action === 'clear') {
        tokenStats = {
          ...tokenStats,
          current_tokens: 0,
          active_agent_current_tokens: 0,
          agent_current_tokens: {},
        };
      }
      if (event.summary_up_to != null
        && ['clear', 'compress', 'auto'].includes(event.action || '')) {
        messages = messages.map((message, index) => ({
          ...message,
          compact: index < event.summary_up_to!,
        }));
      }
      if (contextManagementPending) {
        runStatus = 'idle';
        if (event.action === 'cancelled') runSummary = '';
        contextManagementPending = false;
        streamingMessageId = null;
        streamingContent = '';
        streamingReasoningContent = '';
      }
      break;
    case 'turn_elapsed':
      if (event.seconds != null) lastDurationMs = event.seconds * 1000;
      break;
    case 'index_status':
      runSummary = event.phase === 'scanning'
        ? '正在建立工作区索引...'
        : event.phase === 'done'
          ? `索引扫描完成: ${event.count || 0} 个文件`
          : event.phase === 'error'
            ? '索引扫描失败'
            : runSummary;
      break;
    case 'done':
      messages = updateProcessingTurn(messages, 'success', lastDurationMs);
      if (!lastUsage && event.usage) lastUsage = event.usage;
      runStatus = 'idle';
      runSummary = '';
      contextManagementPending = false;
      toolCallStartedAt = {};
      toolCallFinished = {};
      streamingMessageId = null;
      streamingContent = '';
      streamingReasoningContent = '';
      break;
    case 'error': {
      const errorMessage = typeof event.message === 'string' ? event.message : '';
      messages = updateProcessingTurn(
        messages,
        errorMessage === '已取消' ? 'cancelled' : 'failed',
        lastDurationMs,
      );
      runStatus = 'idle';
      runSummary = errorMessage ? `执行失败：${errorMessage}` : '执行失败';
      contextManagementPending = false;
      toolCallStartedAt = {};
      toolCallFinished = {};
      streamingMessageId = null;
      streamingContent = '';
      streamingReasoningContent = '';
      break;
    }
    default:
      break;
  }

  return {
    messages,
    historyStart: current.historyStart,
    userOutline: syncUserOutline(current.userOutline, current.messages, messages),
    runStatus,
    runSummary,
    contextManagementPending,
    tokenStats,
    lastUsage,
    lastDurationMs,
    toolCallStartedAt,
    toolCallFinished,
    streamingMessageId,
    streamingContent,
    streamingReasoningContent,
    cwd: current.cwd,
    reasoningEffort: current.reasoningEffort,
    hydrated: current.hydrated,
  };
}

/** 是否为轮次锚点（真正的用户提问），与后端 `is_turn_anchor`、前端分组规则一致。 */
export function isTurnAnchor(message: Message): boolean {
  return isUserInput(message);
}

/**
 * 流式事件追加新消息后同步提问目录：新增的用户提问追加到末尾；目录中最后一条
 * 还没有回复预览时，用其后第一段非空文本补齐。消息未变化时原样返回。
 *
 * 流式事件频繁触发，只从末尾向前扫描到最后一个轮次锚点（一轮的长度），
 * 不遍历整个会话。
 */
function syncUserOutline(
  outline: UserOutlineItem[] | null,
  previous: Message[],
  messages: Message[],
): UserOutlineItem[] | null {
  if (!outline || previous === messages) return outline;
  let anchorIndex = -1;
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    if (isTurnAnchor(messages[i])) {
      anchorIndex = i;
      break;
    }
  }
  if (anchorIndex < 0) return outline;
  const anchor = messages[anchorIndex];
  let next = outline;
  const last = outline[outline.length - 1];
  const question = textContent(anchor).trim().slice(0, 160);
  if (!last || last.id !== anchor.id) {
    const existing = outline.findIndex((item) => item.id === anchor.id);
    // 编辑重发：沿用原用户消息 id 并截断其后的历史，目录同步截断并刷新提问。
    next = existing >= 0
      ? [...outline.slice(0, existing), { id: anchor.id, question, answer: '' }]
      : [...outline, { id: anchor.id, question, answer: '' }];
  } else if (last.question !== question) {
    next = [...outline.slice(0, -1), { id: anchor.id, question, answer: '' }];
  }
  const tail = next[next.length - 1];
  if (!tail.answer) {
    for (let i = anchorIndex + 1; i < messages.length; i += 1) {
      const text = textContent(messages[i]).trim();
      if (text) {
        if (next === outline) next = [...outline];
        next[next.length - 1] = { ...tail, answer: text.slice(0, 360) };
        break;
      }
    }
  }
  return next;
}

export interface AppState {
  // 状态
  sessions: Session[];
  activeSessionId: string | null;
  /** 新对话预留的 Session ID；首次发送前不存在对应 Session。 */
  newConversationId: string | null;
  inputCaches: InputCacheMap;
  messages: Message[];
  /** 已加载消息起点在完整历史中的下标；大于 0 表示前面还有未加载的历史。 */
  historyStart: number;
  /** 完整的用户提问目录（刻度条、回合跳转）；null 表示按已加载消息推算。 */
  userOutline: UserOutlineItem[] | null;
  /** 正在向前加载历史。 */
  loadingOlderMessages: boolean;
  /** 正在从后端加载、尚未展示的目标会话（切换会话加载提示）。 */
  switchingSessionId: string | null;
  runStatus: string;
  runSummary: string;
  lastDurationMs: number | null;
  /** 运行中工具调用的开始时刻（tool_call_id → Date.now()），用于运行行实时跳秒。 */
  toolCallStartedAt: Record<string, number>;
  /** 已执行完成、结果尚待按序提交的工具调用（tool_call_id → 结论与耗时）。 */
  toolCallFinished: Record<string, ToolCallFinished>;
  lastUsage: { prompt_tokens: number; completion_tokens: number; total_tokens: number } | null;
  tokenStats: TokenStats | null;

  // 尚未首次发送的新对话
  isNewConversation: boolean;

  // 思考强度（按 session 存储）
  reasoningEffort: string;
  reasoningEffortPerSession: Record<string, string>;
  setReasoningEffort: (effort: string) => void;

  // 工作目录
  workspaceDir: string;
  sessionCwd: string;

  // 多会话运行状态 (session_id -> status)
  sessionRunStatuses: Record<string, string>;

  // 输入队列 (cacheKey -> 待投递消息，队首先行)
  inputQueues: Record<string, QueuedInputMessage[]>;
  /** 执行中 Enter：把当前草稿（文本+附件快照）排入会话队列并清空草稿。 */
  enqueueInputMessage: (cacheKey: string) => void;
  removeQueuedInputMessage: (cacheKey: string, messageId: string) => void;
  /** 拖拽调整队列顺序；自动放行按调整后的顺序投递。 */
  moveQueuedInputMessage: (cacheKey: string, fromIndex: number, toIndex: number) => void;
  /** 编辑：消息内容（文本+附件）回填草稿并从队列移除；草稿非空时先转入队首。 */
  editQueuedInputMessage: (cacheKey: string, messageId: string) => void;
  /** 立即投递指定队列消息：空闲走 sendMessage 开新轮，执行中走 appendMessage 引导。 */
  steerQueuedInputMessage: (cacheKey: string, messageId: string) => Promise<boolean>;
  /** turn 结束（runStatus 回 idle）且草稿为空时自动放行队首；失败不自动重试。 */
  dequeueNextQueuedInput: (cacheKey: string) => Promise<boolean>;
  /**
   * 插件外部文本按「用户普通 Enter」语义投递：保护现有草稿（先入队，不
   * 覆盖）→ 运行中入队等待 turn 结束自动放行（是否立即引导由用户在队列
   * 里决定）→ 空闲立即发送；信任模式用调用方传入的界面当前选择。
   * 不走 appendMessage（立即引导是用户的决定权，插件不得代行）。
   */
  submitExternalText: (
    cacheKey: string,
    content: string,
    trustMode: string,
    attachments?: RawAttachment[],
  ) => void;

  // 流式消息状态
  streamingMessageId: string | null;
  streamingContent: string;
  streamingReasoningContent: string; // 流式思考过程内容


  // Agent 团队
  agents: AgentInfo[];

  // 加载状态
  isLoadingSessions: boolean;
  /** 最近一次会话列表加载失败的原因；成功加载后清空。 */
  sessionsLoadError: string | null;

  // 更新检查
  updateAvailable: null | { version: string; body?: string; date?: string };
  setUpdateAvailable: (info: null | { version: string; body?: string; date?: string }) => void;

  // 从外部触发打开设置页到指定 tab
  pendingSettingsTab: string | null;
  setPendingSettingsTab: (tab: string | null) => void;

  // 操作
  // options.protective=true 用于 sessions_updated 触发的刷新：
  // - 不参与 loading 引用计数（避免侧栏闪「加载中」，也不清除普通刷新持有的 loading）；
  // - 旧请求晚到时放弃写入；active 不在权威列表时执行完整的会话切换或新对话初始化。
  // 前端在 getSessions 失败时保留旧列表，避免瞬时请求错误清空侧栏。
  loadSessions: (options?: { protective?: boolean }) => Promise<void>;
  updateSessionMeta: (sessionId: string) => Promise<void>;
  startNewConversation: (targetCwd?: string) => Promise<void>;
  switchSession: (id: string) => Promise<void>;
  /** 向前加载一段更早的历史；返回是否加载到了新消息。 */
  loadOlderMessages: () => Promise<boolean>;
  /** 加载当前会话的全部历史（会话内搜索前调用）。 */
  loadAllMessages: () => Promise<void>;
  deleteSession: (sessionId: string) => Promise<void>;
  deleteSessionsByCwd: (cwd: string) => Promise<{ failed: number }>;

  sendMessage: (
    cacheKey: string,
    content: string,
    attachments: RawAttachment[],
    revision: number,
    trustMode?: string,
    modelRef?: string | null,
  ) => Promise<boolean>;
  appendMessage: (
    sessionId: string,
    content: string,
    attachments: RawAttachment[],
    revision: number,
  ) => Promise<boolean>;
  editAndResend: (
    sessionId: string,
    messageId: string,
    newContent: string,
    attachments: RawAttachment[],
    revision: number,
    baseContent: ContentBlock[],
  ) => Promise<boolean>;
  cancelTurn: () => Promise<boolean>;

  setInputCacheText: (cacheKey: string, content: string) => void;
  setInputCacheAttachments: (cacheKey: string, attachments: RawAttachment[]) => void;
  syncInputCache: (
    cacheKey: string,
    cache: InputCache,
    immediate?: boolean,
    claimRevision?: number,
  ) => Promise<InputCache>;
  flushInputCacheQueue: (cacheKey: string) => Promise<void>;

  setSessionCwd: (cwd: string) => Promise<void>;
  setWorkspaceDir: (workspaceDir: string) => Promise<void>;

  /** 用户"按需进程沙箱"开关（全局）：null 表示尚未从宿主加载。 */
  sandboxDisabled: boolean | null;
  /** 沙箱程序状态（设置页 / 输入区指示 / 启动门闸共用）：null 表示尚未加载。 */
  sandboxState: SandboxUpdateState | null;
  commandEnvBlocklist: string[] | null;
  loadSandboxDisabled: () => Promise<void>;
  loadCommandEnvBlocklist: () => Promise<void>;
  setCommandEnvBlocklist: (blocklist: string[]) => Promise<void>;
  setSandboxDisabled: (disabled: boolean) => Promise<void>;
  /** force 时总是重新查询；否则仅在尚未加载时查询一次。 */
  loadSandboxState: (force?: boolean) => Promise<void>;
  setSandboxState: (state: SandboxUpdateState) => void;


  beginContextManagement: (summary: string) => void;
  endContextManagement: () => void;

  // 内部方法
  applyStreamEvents: (events: SessionStreamEvent[]) => void;
}

export function selectCurrentInputCacheKey(state: AppState): string | null {
  return state.activeSessionId ?? state.newConversationId;
}

export function selectCurrentInputCache(state: AppState): InputCache {
  return getInputCache(state.inputCaches, selectCurrentInputCacheKey(state));
}

export function selectCurrentIsSending(state: AppState): boolean {
  return selectCurrentInputCache(state).is_sending;
}

export const useStore = create<AppState>((set, get) => ({
  // 初始状态
  sessions: [],
  activeSessionId: null as string | null,
  newConversationId: null as string | null,
  inputCaches: {},
  messages: [],
  historyStart: 0,
  userOutline: null,
  loadingOlderMessages: false,
  switchingSessionId: null,
  runStatus: 'idle',
  runSummary: '',
  lastDurationMs: null,
  toolCallStartedAt: {},
  toolCallFinished: {},
  lastUsage: null,
  tokenStats: null,
  isNewConversation: true,
  updateAvailable: null,
  setUpdateAvailable: (info) => set({ updateAvailable: info }),
  pendingSettingsTab: null,
  setPendingSettingsTab: (tab) => set({ pendingSettingsTab: tab }),
  sandboxDisabled: null,
  sandboxState: null,
  commandEnvBlocklist: null,
  reasoningEffort: 'medium',
  reasoningEffortPerSession: {},
  setReasoningEffort: (effort: string) => {
    const { activeSessionId, reasoningEffortPerSession } = get();
    const key = activeSessionId || '__new_conversation__';
    const updated = { ...reasoningEffortPerSession, [key]: effort };
    set({ reasoningEffort: effort, reasoningEffortPerSession: updated });
    if (activeSessionId) {
      const cache = sessionViewCaches.get(activeSessionId);
      if (cache) sessionViewCaches.set(activeSessionId, { ...cache, reasoningEffort: effort });
      api.setReasoningEffort(effort, activeSessionId).catch(console.error);
    }
  },
  workspaceDir: '',
  sessionCwd: '',
  sessionRunStatuses: {},
  inputQueues: {},
  streamingMessageId: null,
  streamingContent: '',
  streamingReasoningContent: '',
  isLoadingSessions: false,
  sessionsLoadError: null,
  agents: [],

  // 加载会话列表
  loadSessions: async (options?: { protective?: boolean }) => {
    const isProtective = options?.protective === true;
    // 普通刷新用引用计数维护 loading：任一普通请求未结束都保持 loading；
    // protective 刷新（sessions_updated 触发）不参与计数，避免侧栏闪「加载中」，
    // 也避免它提前清除普通刷新持有的 loading。
    if (!isProtective) {
      ordinaryLoadInFlight += 1;
      if (ordinaryLoadInFlight === 1) {
        set({ isLoadingSessions: true });
      }
    }
    const requestVersion = ++loadSessionsRequestVersion;
    const finishOrdinary = () => {
      if (!isProtective) {
        ordinaryLoadInFlight -= 1;
        if (ordinaryLoadInFlight === 0) {
          set({ isLoadingSessions: false });
        }
      }
    };
    try {
      const sessions = await api.getSessions();
      // 旧请求晚到时放弃写入，避免覆盖较新的权威结果。
      if (requestVersion !== loadSessionsRequestVersion) {
        finishOrdinary();
        return;
      }
      const prev = get();
      const activeSessionId = prev.activeSessionId;
      const activeSessionInvalid = !!activeSessionId
        && !sessions.some((session) => session.id === activeSessionId);
      let newConversationId = prev.newConversationId;
      // 普通加载时为尚未初始化的新对话预生成 ID。active 失效由下方完整迁移处理。
      if (
        !isProtective &&
        !activeSessionId &&
        prev.isNewConversation &&
        !newConversationId
      ) {
        const idRequestVersion = ++newConversationRequestVersion;
        const generatedId = await api.newSessionId();
        if (requestVersion !== loadSessionsRequestVersion) {
          finishOrdinary();
          return;
        }
        newConversationId = idRequestVersion === newConversationRequestVersion
          ? generatedId
          : get().newConversationId;
      }
      const initialCache = newConversationId
        ? get().inputCaches[newConversationId] ?? emptyInputCache()
        : null;
      set((state) => ({
        sessions,
        sessionsLoadError: null,
        isLoadingSessions: ordinaryLoadInFlight === 0 ? false : state.isLoadingSessions,
        newConversationId,
        sessionCwd: activeSessionId ? state.sessionCwd : state.workspaceDir,
        inputCaches: newConversationId && initialCache
          ? { ...state.inputCaches, [newConversationId]: initialCache }
          : state.inputCaches,
      }));
      if (newConversationId && initialCache) {
        syncInputCacheInBackground(get().syncInputCache(newConversationId, initialCache));
      }

      if (activeSessionInvalid) {
        const nextSessionId = sessions[0]?.id;
        if (nextSessionId) await get().switchSession(nextSessionId);
        else await get().startNewConversation();
      } else {
        // 从后端恢复思考强度设置
        api.getReasoningEffort().then((effort) => {
          set({ reasoningEffort: effort });
        }).catch(console.error);
      }
      finishOrdinary();
      return;
    } catch (error) {
      console.error('加载会话失败:', error);
      // 只记录最新一次请求的失败；成功的刷新会清除该提示。
      if (requestVersion === loadSessionsRequestVersion) {
        set({ sessionsLoadError: error instanceof Error ? error.message : String(error) });
      }
      finishOrdinary();
      return;
    }
  },

  // 精确更新单条会话的元数据（消息数/更新时间），替代全量 loadSessions。
  updateSessionMeta: async (sessionId: string) => {
    try {
      const meta = await api.getSessionMeta(sessionId);
      if (!meta) return;
      set((state) => {
        const idx = state.sessions.findIndex((s) => s.id === sessionId);
        if (idx < 0) {
          // 会话不在列表中（如恢复后首次更新），加入列表
          return { sessions: [...state.sessions, meta] };
        }
        const sessions = state.sessions.slice();
        sessions[idx] = { ...sessions[idx], ...meta };
        return { sessions };
      });
    } catch (error) {
      console.error('更新会话元数据失败:', error);
    }
  },

  // 开始新对话：只预留 ID 并初始化输入缓存，首次发送时才由 Core 创建 Session。
  // targetCwd 用于在指定 workspace 分组下创建对话；不传则用全局 workspace。
  startNewConversation: async (targetCwd?: string) => {
    switchRequestVersion += 1;
    const requestVersion = ++newConversationRequestVersion;
    // 进行中的会话切换被新对话取代：撤下其加载提示。
    if (get().switchingSessionId) set({ switchingSessionId: null });
    const workspaceDir = get().workspaceDir;
    const newConversationCwd = targetCwd || workspaceDir;
    const { reasoningEffortPerSession } = get();
    const newConversationEffort = reasoningEffortPerSession['__new_conversation__'] || 'medium';
    try {
      const newConversationId = await api.newSessionId();
      if (requestVersion !== newConversationRequestVersion) return;
      const previousCacheId = get().newConversationId;
      const previousCacheIsSending = previousCacheId
        ? get().inputCaches[previousCacheId]?.is_sending === true
        : false;
      if (previousCacheId && previousCacheId !== newConversationId && !previousCacheIsSending) {
        discardInputCacheSyncQueue(previousCacheId);
        api.removeInputCache(previousCacheId).catch((error) =>
          console.error('清理旧输入缓存失败:', error),
        );
      }
      const initialCache = emptyInputCache();
      set((state) => {
        const inputCaches = { ...state.inputCaches };
        if (previousCacheId && !previousCacheIsSending) delete inputCaches[previousCacheId];
        inputCaches[newConversationId] = initialCache;
        return {
          isNewConversation: true,
          activeSessionId: null,
          newConversationId,
          inputCaches,
          messages: [],
          historyStart: 0,
          userOutline: null,
          runStatus: 'idle',
          runSummary: '',
          lastUsage: null,
          tokenStats: null,
          streamingMessageId: null,
          streamingContent: '',
          streamingReasoningContent: '',
          sessionCwd: newConversationCwd,
          agents: [],
          reasoningEffort: newConversationEffort,
        };
      });
      syncInputCacheInBackground(get().syncInputCache(newConversationId, initialCache));
      // 后台预热工作区索引：消除首次发送消息时的同步扫描延迟，不阻塞 UI。
      api
        .prewarmWorkspaceIndex(newConversationCwd)
        .catch((error) => console.error('索引预热失败:', error));
    } catch (error) {
      console.error('开始新对话失败:', error);
    }
  },

  // 切换会话
  switchSession: async (id: string) => {
    newConversationRequestVersion += 1;
    const requestVersion = ++switchRequestVersion;
    // 需要从后端拉取会话时显示加载提示（远程模式下传输较慢，避免看似无响应）。
    const needsFetch = !sessionViewCaches.get(id)?.hydrated;
    if (needsFetch) set({ switchingSessionId: id });
    const clearSwitching = () => {
      if (requestVersion === switchRequestVersion && get().switchingSessionId === id) {
        set({ switchingSessionId: null });
      }
    };
    try {
      const initialState = get();
      const existingCache = sessionViewCaches.get(id);
      const [loaded, storedCache] = await Promise.all([
        existingCache?.hydrated ? Promise.resolve(null) : api.loadSession(id),
        initialState.inputCaches[id] ? Promise.resolve(null) : api.getInputCache(id),
      ]);
      if (requestVersion !== switchRequestVersion) return;
      if (!await commitSessionSwitch(id, requestVersion)) return;
      set((state) => {
        let cache = sessionViewCaches.get(id) || existingCache
          || emptySessionViewCache(state.sessionRunStatuses[id]);
        if (loaded) cache = hydrateSessionViewCache(cache, loaded);

        const knownRunStatus = state.sessionRunStatuses[id];
        cache = {
          ...cache,
          runStatus: knownRunStatus || 'idle',
          runSummary: knownRunStatus ? cache.runSummary || '正在处理' : '',
        };
        sessionViewCaches.set(id, cache);
        const keepsStreamingMessage = !!cache.streamingMessageId
          && cache.messages.some((message) => message.id === cache.streamingMessageId);
        return {
          isNewConversation: false,
          activeSessionId: id,
          newConversationId: null,
          inputCaches: {
            ...state.inputCaches,
            ...(storedCache ? {
              [id]: state.inputCaches[id]?.revision > storedCache.revision
                ? state.inputCaches[id]
                : cloneInputCache(storedCache),
            } : {}),
          },
          messages: cache.messages,
          historyStart: cache.historyStart,
          userOutline: cache.userOutline,
          runStatus: cache.runStatus,
          runSummary: cache.runSummary,
          lastDurationMs: cache.lastDurationMs,
          toolCallStartedAt: cache.toolCallStartedAt,
          toolCallFinished: cache.toolCallFinished,
          lastUsage: cache.lastUsage,
          tokenStats: cache.tokenStats,
          sessionCwd: cache.cwd,
          streamingMessageId: keepsStreamingMessage ? cache.streamingMessageId : null,
          streamingContent: keepsStreamingMessage ? cache.streamingContent : '',
          streamingReasoningContent: keepsStreamingMessage
            ? cache.streamingReasoningContent
            : '',
          agents: parseAgentsFromMessages(cache.messages),
          reasoningEffort: cache.reasoningEffort,
          reasoningEffortPerSession: {
            ...state.reasoningEffortPerSession,
            [id]: cache.reasoningEffort,
          },
        };
      });
    } catch (error) {
      console.error('切换会话失败:', error);
    } finally {
      clearSwitching();
    }
  },

  loadOlderMessages: async () => {
    const sessionId = get().activeSessionId;
    const state = get();
    // 已有加载在进行：等待它完成并复用结果，避免滚动加载与回合跳转互相打断。
    if (olderMessagesInFlight) return olderMessagesInFlight;
    if (!sessionId || state.historyStart <= 0) return false;
    const first = state.messages[0];
    if (!first) return false;
    const run = (async () => {
    set({ loadingOlderMessages: true });
    try {
      const page = await api.loadSessionMessages(sessionId, first.id);
      let added = false;
      set((current) => {
        // 期间切走会话或历史已被替换：丢弃本段结果。
        if (current.activeSessionId !== sessionId || current.messages[0]?.id !== first.id) {
          return { loadingOlderMessages: false };
        }
        const known = new Set(current.messages.map((message) => message.id));
        const older = page.messages.filter((message) => !known.has(message.id));
        added = older.length > 0;
        const messages = [...older, ...current.messages];
        const cache = sessionViewCaches.get(sessionId);
        if (cache) {
          sessionViewCaches.set(sessionId, { ...cache, messages, historyStart: page.start });
        }
        return { messages, historyStart: page.start, loadingOlderMessages: false };
      });
      return added;
    } catch (error) {
      console.error('加载更早的历史失败:', error);
      set({ loadingOlderMessages: false });
      // 历史已变化（如编辑重发截断）：丢弃缓存整体重新加载当前会话。
      if (get().activeSessionId === sessionId) {
        sessionViewCaches.delete(sessionId);
        await get().switchSession(sessionId);
      }
      return false;
    }
    })();
    olderMessagesInFlight = run;
    try {
      return await run;
    } finally {
      if (olderMessagesInFlight === run) olderMessagesInFlight = null;
    }
  },

  loadAllMessages: async () => {
    const sessionId = get().activeSessionId;
    // 逐段向前加载直到最早；每段都会校验会话未切换。
    while (sessionId && get().activeSessionId === sessionId && get().historyStart > 0) {
      const added = await get().loadOlderMessages();
      if (!added) break;
    }
  },

  // 删除当前会话
  deleteSession: async (sessionId: string) => {
    try {
      const deletedSessionId = sessionId;
      const wasActive = get().activeSessionId === deletedSessionId;
      await api.deleteSession(deletedSessionId);
      sessionViewCaches.delete(deletedSessionId);
      discardInputCacheSyncQueue(deletedSessionId);
      // 本地直接移除被删会话，不重新拉列表（避免全量扫描卡顿）。
      set((state) => {
        const inputCaches = { ...state.inputCaches };
        const sessionRunStatuses = { ...state.sessionRunStatuses };
        const inputQueues = { ...state.inputQueues };
        delete inputCaches[deletedSessionId];
        delete sessionRunStatuses[deletedSessionId];
        delete inputQueues[deletedSessionId];
        return {
          sessions: state.sessions.filter((s) => s.id !== deletedSessionId),
          inputCaches,
          sessionRunStatuses,
          inputQueues,
        };
      });

      // 只有删的是当前活跃会话时才进入新对话态。
      if (wasActive) {
        await get().startNewConversation();
      }
    } catch (error) {
      console.error('删除会话失败:', error);
    }
  },

  // 删除指定 workspace（cwd）下的所有会话
  deleteSessionsByCwd: async (cwd: string) => {
    try {
      const before = get();
      const wasNewConversation = before.isNewConversation;
      const previousActiveSessionId = before.activeSessionId;
      // 后端返回成功和失败的 ID 列表。
      const { succeeded: succeededIds, failed: failedIds } = await api.deleteSessionsByCwd(cwd);

      for (const sessionId of succeededIds) {
        sessionViewCaches.delete(sessionId);
        discardInputCacheSyncQueue(sessionId);
      }
      // 本地只移除实际删除成功的会话。
      const remainingSessions = before.sessions.filter(
        (session) => !succeededIds.includes(session.id),
      );
      set((state) => {
        const inputCaches = { ...state.inputCaches };
        const sessionRunStatuses = { ...state.sessionRunStatuses };
        const inputQueues = { ...state.inputQueues };
        for (const sessionId of succeededIds) {
          delete inputCaches[sessionId];
          delete sessionRunStatuses[sessionId];
          delete inputQueues[sessionId];
        }
        return {
          sessions: remainingSessions,
          inputCaches,
          sessionRunStatuses,
          inputQueues,
        };
      });

      if (failedIds.length > 0) {
        console.warn('部分会话删除失败：', failedIds);
      }

      if (wasNewConversation) {
        return { failed: failedIds.length };
      }
      if (previousActiveSessionId
        && remainingSessions.some((session) => session.id === previousActiveSessionId)) {
        return { failed: failedIds.length };
      }
      const nextSessionId = remainingSessions[0]?.id;
      if (nextSessionId) await get().switchSession(nextSessionId);
      else await get().startNewConversation();
      return { failed: failedIds.length };
    } catch (error) {
      console.error('删除 workspace 会话失败:', error);
      return { failed: -1 };
    }
  },

  syncInputCache: (cacheKey, cache, immediate = false, claimRevision) => {
    let queue = inputCacheSyncQueues.get(cacheKey);
    if (!queue) {
      queue = {
        pending: null,
        pendingVersion: 0,
        claimed: null,
        running: false,
        debounceVersion: 0,
        nextVersion: 0,
        immediateThroughVersion: 0,
        waiters: [],
      };
      inputCacheSyncQueues.set(cacheKey, queue);
    }
    const enqueueVersion = queue.nextVersion + 1;
    queue.nextVersion = enqueueVersion;
    if (claimRevision !== undefined) {
      // 发送快照必须保留精确 revision，不能被正在排队的 R+1 新输入合并。
      // 更旧的普通 pending 可由该快照覆盖；快照之后的新输入会另存于 pending。
      queue.claimed = {
        cache: cloneInputCache(cache),
        version: enqueueVersion,
        revision: claimRevision,
      };
      queue.pending = null;
    } else {
      queue.pending = cloneInputCache(cache);
      queue.pendingVersion = enqueueVersion;
    }
    if (immediate) queue.immediateThroughVersion = enqueueVersion;
    queue.debounceVersion += 1;
    const debounceVersion = queue.debounceVersion;
    const completion = new Promise<InputCache>((resolve, reject) => {
      queue!.waiters.push({ version: enqueueVersion, resolve, reject });
    });

    if (!queue.running) {
      if (immediate) {
        void get().flushInputCacheQueue(cacheKey);
      } else {
        void waitForInputCacheDebounce().then(() => {
          const latest = inputCacheSyncQueues.get(cacheKey);
          if (
            latest === queue
            && !latest.running
            && latest.debounceVersion === debounceVersion
          ) {
            void get().flushInputCacheQueue(cacheKey);
          }
        });
      }
    }
    return completion;
  },

  flushInputCacheQueue: async (cacheKey) => {
    const queue = inputCacheSyncQueues.get(cacheKey);
    if (!queue || queue.running || (!queue.claimed && !queue.pending)) return;
    const claimed = queue.claimed;
    const submitted = claimed?.cache ?? queue.pending!;
    const submittedVersion = claimed?.version ?? queue.pendingVersion;
    const submittedClaimRevision = claimed?.revision;
    if (claimed) {
      queue.claimed = null;
    } else {
      queue.pending = null;
    }
    queue.running = true;
    const requestVersion = (inputCacheSyncRequestVersions.get(cacheKey) ?? 0) + 1;
    inputCacheSyncRequestVersions.set(cacheKey, requestVersion);
    try {
      const stored = await api.setInputCache(
        cacheKey,
        submitted,
        submittedClaimRevision ?? undefined,
      );
      if (
        inputCacheSyncQueues.get(cacheKey) !== queue
        || inputCacheSyncRequestVersions.get(cacheKey) !== requestVersion
      ) {
        return;
      }
      if (!queue.pending) {
        set((state) => ({
          inputCaches: mergeStoredInputCache(
            state.inputCaches,
            cacheKey,
            submitted.revision,
            stored,
          ),
        }));
      } else {
        queue.pending = rebasePendingInputCacheOnStored(queue.pending, submitted, stored);
      }
      const completed = queue.waiters.filter((waiter) => waiter.version <= submittedVersion);
      queue.waiters = queue.waiters.filter((waiter) => waiter.version > submittedVersion);
      for (const waiter of completed) waiter.resolve(cloneInputCache(stored));
    } catch (error) {
      console.error('同步输入缓存失败:', error);
      const failed = queue.waiters.filter((waiter) => waiter.version <= submittedVersion);
      queue.waiters = queue.waiters.filter((waiter) => waiter.version > submittedVersion);
      for (const waiter of failed) waiter.reject(error);
    } finally {
      queue.running = false;
      if (
        inputCacheSyncQueues.get(cacheKey) === queue
        && (queue.claimed || queue.pending)
      ) {
        const nextVersion = queue.claimed?.version ?? queue.pendingVersion;
        if (queue.claimed || nextVersion <= queue.immediateThroughVersion) {
          void get().flushInputCacheQueue(cacheKey);
        } else {
          queue.debounceVersion += 1;
          const debounceVersion = queue.debounceVersion;
          void waitForInputCacheDebounce().then(() => {
            const latest = inputCacheSyncQueues.get(cacheKey);
            if (
              latest === queue
              && !latest.running
              && latest.debounceVersion === debounceVersion
            ) {
              void get().flushInputCacheQueue(cacheKey);
            }
          });
        }
      }
    }
  },

  // 普通发送：新对话和已有会话都直接向目标 Core 投递。
  sendMessage: async (cacheKey, content, attachments, revision, trustMode, modelRef) => {
    let deliveryAttachments = attachments.map((attachment) => ({ ...attachment }));
    const startsNewConversation = get().newConversationId === cacheKey;
    const initialCwd = get().sessionCwd || get().workspaceDir;
    const initialReasoningEffort = get().reasoningEffort;
    const navigationVersion = switchRequestVersion;
    let sendingCache: InputCache | undefined;
    set((state) => {
      const inputCaches = setInputCacheSending(state.inputCaches, cacheKey, true);
      sendingCache = inputCaches[cacheKey];
      const isCurrent = state.activeSessionId === cacheKey;
      // 投递前即进入执行态：后端 send_message 内部可能先做「整理上下文 →
      // 切换模型」再投递，这段同步等待可达数十秒。若等 await 返回才置位，
      // 用户点发送后会看到输入框清空、界面却毫无动静，误以为应用卡死。
      return {
        inputCaches,
        runStatus: isCurrent ? 'executing' : state.runStatus,
        runSummary: isCurrent ? '正在发送...' : state.runSummary,
        sessionRunStatuses: {
          ...state.sessionRunStatuses,
          [cacheKey]: 'executing',
        },
      };
    });

    try {
      if (sendingCache) {
        const stored = await get().syncInputCache(
          cacheKey,
          sendingCache,
          true,
          revision,
        );
        if (stored.revision !== revision) {
          throw new Error('输入已在发送前发生变化，请重试');
        }
        deliveryAttachments = stored.attachments.map((attachment) => ({ ...attachment }));
      }

      await api.sendMessage(
        cacheKey,
        content,
        deliveryAttachments,
        revision,
        startsNewConversation ? initialCwd : undefined,
        startsNewConversation ? trustMode : undefined,
        startsNewConversation ? initialReasoningEffort : undefined,
        modelRef ?? undefined,
      );

      const shouldActivate = startsNewConversation
        && switchRequestVersion === navigationVersion
        && get().isNewConversation
        && get().activeSessionId === null
        && get().newConversationId === cacheKey;
      if (shouldActivate) {
        await get().switchSession(cacheKey);
        if (get().activeSessionId === cacheKey) {
          const sessions = await api.getSessions();
          set({ sessions });
        }
      }

      let settledCache: InputCache | undefined;
      set((state) => {
        const inputCaches = settleInputCacheSend(
          state.inputCaches,
          cacheKey,
          revision,
          true,
        );
        settledCache = inputCaches[cacheKey];
        const isCurrent = state.activeSessionId === cacheKey;
        return {
          inputCaches,
          runStatus: isCurrent ? 'executing' : state.runStatus,
          sessionRunStatuses: {
            ...state.sessionRunStatuses,
            [cacheKey]: 'executing',
          },
        };
      });
      if (settledCache) {
        syncInputCacheInBackground(get().syncInputCache(cacheKey, settledCache));
      }
      return true;
    } catch (error) {
      console.error('发送消息失败:', error);
      let settledCache: InputCache | undefined;
      set((state) => {
        const inputCaches = settleInputCacheSend(
          state.inputCaches,
          cacheKey,
          revision,
          false,
        );
        settledCache = inputCaches[cacheKey];
        // 投递失败：回滚发送前置的执行态，否则界面会永久停在「执行中」
        // ——本轮没有 Core 事件流，不会有 done/error 事件来收尾。
        const isCurrent = state.activeSessionId === cacheKey;
        const sessionRunStatuses = { ...state.sessionRunStatuses };
        delete sessionRunStatuses[cacheKey];
        return {
          inputCaches,
          runStatus: isCurrent ? 'idle' : state.runStatus,
          runSummary: isCurrent ? '' : state.runSummary,
          sessionRunStatuses,
        };
      });
      if (settledCache) {
        syncInputCacheInBackground(get().syncInputCache(cacheKey, settledCache));
      }
      return false;
    }
  },

  appendMessage: async (sessionId, content, attachments, revision) => {
    let deliveryAttachments = attachments.map((attachment) => ({ ...attachment }));
    let sendingCache: InputCache | undefined;
    set((state) => {
      const inputCaches = setInputCacheSending(state.inputCaches, sessionId, true);
      sendingCache = inputCaches[sessionId];
      return { inputCaches };
    });
    try {
      if (sendingCache) {
        const stored = await get().syncInputCache(
          sessionId,
          sendingCache,
          true,
          revision,
        );
        if (stored.revision !== revision) {
          throw new Error('输入已在追加前发生变化，请重试');
        }
        deliveryAttachments = stored.attachments.map((attachment) => ({ ...attachment }));
      }
      const appended = await api.appendMessage(
        sessionId,
        content,
        deliveryAttachments,
        revision,
      );
      let settledCache: InputCache | undefined;
      set((state) => {
        const inputCaches = settleInputCacheSend(
          state.inputCaches,
          sessionId,
          revision,
          appended,
        );
        settledCache = inputCaches[sessionId];
        return { inputCaches };
      });
      if (settledCache) {
        syncInputCacheInBackground(get().syncInputCache(sessionId, settledCache));
      }
      return appended;
    } catch (error) {
      console.error('追加消息失败:', error);
      let settledCache: InputCache | undefined;
      set((state) => {
        const inputCaches = settleInputCacheSend(state.inputCaches, sessionId, revision, false);
        settledCache = inputCaches[sessionId];
        return { inputCaches };
      });
      if (settledCache) {
        syncInputCacheInBackground(get().syncInputCache(sessionId, settledCache));
      }
      return false;
    }
  },

  // ===== 输入队列 =====
  // 执行中 Enter 不再直接追加引导，而是把当前草稿排入会话队列；投递统一走
  // "写回草稿 + 标准 sendMessage/appendMessage"路径，天然满足 revision/claim
  // 校验与防重复投递，且成功后由 settle 正常清空草稿。

  enqueueInputMessage: (cacheKey) => {
    const cache = get().inputCaches[cacheKey];
    if (!cache || (cache.text.trim().length === 0 && cache.attachments.length === 0)) return;
    const message = queuedMessageFromCache(cache);
    set((state) => ({
      inputQueues: {
        ...state.inputQueues,
        [cacheKey]: [...(state.inputQueues[cacheKey] ?? []), message],
      },
    }));
    get().setInputCacheText(cacheKey, '');
    get().setInputCacheAttachments(cacheKey, []);
  },

  submitExternalText: (cacheKey, content, trustMode, attachments = []) => {
    const text = content.trim();
    if (!text) return;
    const state = get();
    const cache = state.inputCaches[cacheKey];
    if (!cache) return;
    const queueExternal = () => {
      set((current) => ({
        inputQueues: {
          ...current.inputQueues,
          [cacheKey]: [...(current.inputQueues[cacheKey] ?? []), queuedTextMessage(text, attachments)],
        },
      }));
    };
    // 发送事务中：当前草稿属于正在提交的输入，不得把它再次入队（重复
    // 发送风险）——外部文本直接构造队列项，不触碰草稿。
    if (cache.is_sending) {
      queueExternal();
      return;
    }
    // 保护用户草稿（非发送事务的草稿才入队）：先入队再投递外部文本，
    // 任何状态都不覆盖用户未发送内容。
    if (cache.text.trim().length > 0 || cache.attachments.length > 0) {
      state.enqueueInputMessage(cacheKey);
    }
    // 与既有队列逻辑一致的运行判断：状态表存在非空值即运行中。
    const running = Boolean(state.sessionRunStatuses[cacheKey]);
    if (running) {
      queueExternal();
      return;
    }
    // 空闲：写入草稿并发送（sendMessage 自管 is_sending/revision/清缓存）。
    // 附件必须一并写入草稿：sendMessage 以同步后的草稿附件为准投递，
    // 只作为参数传入会被空草稿覆盖（语音输入的录音附件曾因此丢失）。
    get().setInputCacheText(cacheKey, text);
    if (attachments.length > 0) get().setInputCacheAttachments(cacheKey, attachments);
    const fresh = get().inputCaches[cacheKey];
    if (!fresh) return;
    void get().sendMessage(cacheKey, text, fresh.attachments, fresh.revision, trustMode);
  },

  removeQueuedInputMessage: (cacheKey, messageId) => {
    set((state) => {
      const queue = state.inputQueues[cacheKey];
      if (!queue?.some((message) => message.id === messageId)) return state;
      return {
        inputQueues: {
          ...state.inputQueues,
          [cacheKey]: queue.filter((message) => message.id !== messageId),
        },
      };
    });
  },

  moveQueuedInputMessage: (cacheKey, fromIndex, toIndex) => {
    set((state) => {
      const queue = state.inputQueues[cacheKey];
      if (
        !queue
        || fromIndex === toIndex
        || fromIndex < 0 || fromIndex >= queue.length
        || toIndex < 0 || toIndex >= queue.length
      ) {
        return state;
      }
      const next = queue.slice();
      const [moved] = next.splice(fromIndex, 1);
      next.splice(toIndex, 0, moved);
      return { inputQueues: { ...state.inputQueues, [cacheKey]: next } };
    });
  },

  editQueuedInputMessage: (cacheKey, messageId) => {
    const state = get();
    const message = (state.inputQueues[cacheKey] ?? []).find((item) => item.id === messageId);
    if (!message) return;
    // 草稿非空时先转入队首，避免回填覆盖丢失。
    const cache = state.inputCaches[cacheKey];
    const draftMessage = cache && (cache.text.trim().length > 0 || cache.attachments.length > 0)
      ? queuedMessageFromCache(cache)
      : null;
    set((current) => {
      const remaining = (current.inputQueues[cacheKey] ?? []).filter((item) => item.id !== messageId);
      return {
        inputQueues: {
          ...current.inputQueues,
          [cacheKey]: draftMessage ? [draftMessage, ...remaining] : remaining,
        },
      };
    });
    get().setInputCacheText(cacheKey, message.text);
    get().setInputCacheAttachments(cacheKey, message.attachments);
  },

  steerQueuedInputMessage: async (cacheKey, messageId) => {
    const state = get();
    const queue = state.inputQueues[cacheKey] ?? [];
    const index = queue.findIndex((message) => message.id === messageId);
    if (index < 0) return false;
    const message = queue[index];
    // 草稿非空时先入队保底，避免被队列消息覆盖丢失；排到队首，本轮投递后最先放行。
    const cache = state.inputCaches[cacheKey];
    const draftMessage = cache && (cache.text.trim().length > 0 || cache.attachments.length > 0)
      ? queuedMessageFromCache(cache)
      : null;
    set((current) => {
      const remaining = (current.inputQueues[cacheKey] ?? []).filter((item) => item.id !== messageId);
      return {
        inputQueues: {
          ...current.inputQueues,
          [cacheKey]: draftMessage ? [draftMessage, ...remaining] : remaining,
        },
      };
    });

    get().setInputCacheText(cacheKey, message.text);
    get().setInputCacheAttachments(cacheKey, message.attachments);
    const latest = get().inputCaches[cacheKey];
    if (!latest) return false;
    const content = message.text.trim()
      || (message.attachments.length > 0 ? '请处理这些附件。' : message.text);
    const isIdle = !get().sessionRunStatuses[cacheKey];
    const delivered = isIdle
      ? await get().sendMessage(cacheKey, content, latest.attachments, latest.revision)
      : await get().appendMessage(cacheKey, content, latest.attachments, latest.revision);
    if (!delivered) {
      // 失败回插队首并清空草稿（内容仍在队列，不丢失）。
      set((current) => {
        const currentQueue = current.inputQueues[cacheKey] ?? [];
        if (currentQueue.some((item) => item.id === message.id)) return current;
        return {
          inputQueues: { ...current.inputQueues, [cacheKey]: [message, ...currentQueue] },
        };
      });
      get().setInputCacheText(cacheKey, '');
      get().setInputCacheAttachments(cacheKey, []);
      return false;
    }
    return true;
  },

  dequeueNextQueuedInput: async (cacheKey) => {
    const state = get();
    if (state.sessionRunStatuses[cacheKey]) return false;
    const cache = state.inputCaches[cacheKey];
    if (cache && (cache.text.trim().length > 0 || cache.attachments.length > 0 || cache.is_sending)) {
      return false;
    }
    const first = state.inputQueues[cacheKey]?.[0];
    if (!first) return false;
    return get().steerQueuedInputMessage(cacheKey, first.id);
  },

  // 编辑态有自己的 revision；这里只按显式 session_id 更新该会话发送状态。
  editAndResend: async (
    sessionId,
    messageId,
    newContent,
    attachments,
    revision,
    baseContent,
  ) => {
    let sendingCache: InputCache | undefined;
    set((state) => {
      const inputCaches = setInputCacheSending(state.inputCaches, sessionId, true);
      sendingCache = inputCaches[sessionId];
      return { inputCaches };
    });
    try {
      if (sendingCache) {
        // 编辑重发不依赖主输入框草稿同步的结果（不同于 sendMessage 需要拿回
        // revision/attachments），改为后台落盘，避免阻塞编辑重发请求。
        syncInputCacheInBackground(get().syncInputCache(sessionId, sendingCache, true));
      }
      await api.editAndResend(
        sessionId,
        messageId,
        newContent,
        attachments,
        revision,
        baseContent,
      );
      let settledCache: InputCache | undefined;
      set((state) => {
        const inputCaches = setInputCacheSending(state.inputCaches, sessionId, false);
        settledCache = inputCaches[sessionId];
        const isCurrent = state.activeSessionId === sessionId;
        return {
          inputCaches,
          runStatus: isCurrent ? 'executing' : state.runStatus,
          sessionRunStatuses: { ...state.sessionRunStatuses, [sessionId]: 'executing' },
        };
      });
      if (settledCache) {
        syncInputCacheInBackground(get().syncInputCache(sessionId, settledCache));
      }
      return true;
    } catch (error) {
      console.error('编辑重发失败:', error);
      let settledCache: InputCache | undefined;
      set((state) => {
        const inputCaches = setInputCacheSending(state.inputCaches, sessionId, false);
        settledCache = inputCaches[sessionId];
        return { inputCaches };
      });
      if (settledCache) {
        syncInputCacheInBackground(get().syncInputCache(sessionId, settledCache));
      }
      return false;
    }
  },

  // 取消当前执行
  cancelTurn: async () => {
    // 取消目标即当前界面所在会话（新对话首轮投递期间为预留的会话 ID），
    // 明确传给后端，不依赖后端的全局活动会话。
    const cacheKey = selectCurrentInputCacheKey(get());
    if (!cacheKey) return false;
    {
      const cache = sessionViewCaches.get(cacheKey);
      if (cache && cache.runStatus !== 'idle') {
        sessionViewCaches.set(cacheKey, { ...cache, runSummary: '正在取消...' });
      }
    }
    set((state) => ({
      runSummary: state.runStatus === 'idle' ? state.runSummary : '正在取消...',
    }));
    let timeoutHandle: ReturnType<typeof setTimeout> | undefined;
    try {
      timeoutHandle = setTimeout(() => {
        set((state) => ({
          runSummary: state.runStatus === 'idle'
            ? state.runSummary
            : '取消仍在后台处理中，可再次点击停止重试',
        }));
      }, 2000);
      const cancelled = await api.cancelTurn(cacheKey);
      if (cancelled) {
        let settledCache: InputCache | undefined;
        set((state) => {
          const inputCaches = cacheKey
            ? setInputCacheSending(state.inputCaches, cacheKey, false)
            : state.inputCaches;
          settledCache = cacheKey ? inputCaches[cacheKey] : undefined;
          return {
            runSummary: state.runStatus === 'idle' ? state.runSummary : '正在取消...',
            inputCaches,
          };
        });
        if (cacheKey && settledCache) {
          syncInputCacheInBackground(get().syncInputCache(cacheKey, settledCache));
        }
      }
      return cancelled;
    } catch (error) {
      console.error('取消执行失败:', error);
      set((state) => ({
        runSummary: state.runStatus === 'idle'
          ? state.runSummary
          : '取消请求失败，任务可能仍在运行，请重试',
      }));
      return false;
    } finally {
      if (timeoutHandle) clearTimeout(timeoutHandle);
    }
  },

  setInputCacheText: (cacheKey: string, content: string) => {
    let nextCache: InputCache | undefined;
    set((state) => {
      const inputCaches = updateInputCacheText(state.inputCaches, cacheKey, content);
      nextCache = inputCaches[cacheKey];
      return { inputCaches };
    });
    if (nextCache) syncInputCacheInBackground(get().syncInputCache(cacheKey, nextCache));
  },

  setInputCacheAttachments: (cacheKey: string, attachments: RawAttachment[]) => {
    let nextCache: InputCache | undefined;
    set((state) => {
      const inputCaches = updateInputCacheAttachments(state.inputCaches, cacheKey, attachments);
      nextCache = inputCaches[cacheKey];
      return { inputCaches };
    });
    if (nextCache) syncInputCacheInBackground(get().syncInputCache(cacheKey, nextCache));
  },

  // 设置工作目录
  setSessionCwd: async (cwd: string) => {
    try {
      // 新对话只在前端保存，首次发送时作为 Core 创建 Session 的初始目录。
      const { isNewConversation, activeSessionId } = get();
      if (!isNewConversation && activeSessionId) {
        await api.setSessionCwd(activeSessionId, cwd);
        const cache = sessionViewCaches.get(activeSessionId);
        if (cache) sessionViewCaches.set(activeSessionId, { ...cache, cwd });
      }
      set({ sessionCwd: cwd });
      // 新对话选定目录后立即预热索引（现有会话由后端在目录变更时处理）。
      if (isNewConversation) {
        api
          .prewarmWorkspaceIndex(cwd)
          .catch((error) => console.error('索引预热失败:', error));
      }
    } catch (error) {
      console.error('设置工作目录失败:', error);
      throw error;
    }
  },

  // 设置 Desktop 工作空间
  setWorkspaceDir: async (workspaceDir: string) => {
    try {
      await api.setWorkspaceDir(workspaceDir);
      set({ workspaceDir });
    } catch (error) {
      console.error('设置工作空间失败:', error);
      throw error;
    }
  },

  // 加载用户"按需进程沙箱"开关（设置页与输入区状态图标共用）
  loadSandboxDisabled: async () => {
    try {
      const disabled = await api.getSandboxDisabled();
      set({ sandboxDisabled: disabled });
    } catch (error) {
      console.error('加载按需进程沙箱开关失败:', error);
    }
  },

  // 加载沙箱程序状态（设置页、输入区指示与启动门闸共用）
  loadSandboxState: async (force = false) => {
    if (!force && get().sandboxState !== null) return;
    try {
      const state = await api.getSandboxUpdateState();
      set({ sandboxState: state });
    } catch (error) {
      console.error('加载沙箱程序状态失败:', error);
    }
  },

  // 启动门闸等已查询方直接写入，避免输入区重复请求
  setSandboxState: (state) => set({ sandboxState: state }),

  // 命令环境变量屏蔽清单（宿主直跑路径的黑名单扩展）
  loadCommandEnvBlocklist: async () => {
    try {
      const blocklist = await api.getCommandEnvBlocklist();
      set({ commandEnvBlocklist: blocklist });
    } catch (error) {
      console.error('加载环境变量屏蔽清单失败:', error);
    }
  },

  setCommandEnvBlocklist: async (blocklist: string[]) => {
    try {
      await api.setCommandEnvBlocklist(blocklist);
      set({ commandEnvBlocklist: blocklist });
    } catch (error) {
      console.error('保存环境变量屏蔽清单失败:', error);
      throw error;
    }
  },

  // 写入用户"按需进程沙箱"开关并同步宿主配置
  setSandboxDisabled: async (disabled: boolean) => {
    try {
      await api.setSandboxDisabled(disabled);
      set({ sandboxDisabled: disabled });
    } catch (error) {
      console.error('设置命令沙箱开关失败:', error);
      throw error;
    }
  },



  beginContextManagement: (summary: string) => {
    const state = get();
    const { activeSessionId } = state;
    if (activeSessionId) {
      const cache = sessionViewCaches.get(activeSessionId) || sessionViewCacheFromState(state);
      sessionViewCaches.set(activeSessionId, {
        ...cache,
        runStatus: 'executing',
        runSummary: summary,
        contextManagementPending: true,
        lastDurationMs: null,
        toolCallStartedAt: {},
        toolCallFinished: {},
        streamingMessageId: null,
        streamingContent: '',
        streamingReasoningContent: '',
      });
    }
    set((state) => ({
      runStatus: 'executing',
      runSummary: summary,
      lastDurationMs: null,
      toolCallStartedAt: {},
      toolCallFinished: {},
      streamingMessageId: null,
      streamingContent: '',
      streamingReasoningContent: '',
      sessionRunStatuses: activeSessionId
        ? { ...state.sessionRunStatuses, [activeSessionId]: 'executing' }
        : state.sessionRunStatuses,
    }));
  },

  endContextManagement: () => {
    const { activeSessionId } = get();
    if (activeSessionId) {
      const cache = sessionViewCaches.get(activeSessionId);
      if (cache) {
        sessionViewCaches.set(activeSessionId, {
          ...cache,
          runStatus: 'idle',
          runSummary: '',
          contextManagementPending: false,
        });
      }
    }
    set((state) => {
      const nextStatuses = { ...state.sessionRunStatuses };
      if (activeSessionId) {
        delete nextStatuses[activeSessionId];
      }
      return {
        runStatus: 'idle',
        runSummary: '',
        sessionRunStatuses: nextStatuses,
      };
    });
    // 上下文管理结束回到空闲：若之前积累了输入队列，继续放行队首。
    if (activeSessionId) {
      void get().dequeueNextQueuedInput(activeSessionId);
    }
  },

  applyStreamEvents: (events) => {
    if (events.length === 0) return;
    const state = get();
    const previousStatuses = state.sessionRunStatuses;
    const currentSessionId = state.activeSessionId || state.newConversationId;
    const sessionRunStatuses = { ...previousStatuses };
    let currentCache: SessionViewCache | null = null;
    let refreshCurrentAgents = false;
    // 标题变更直接更新内存中的会话标题，不触发整表 sessions_updated 刷新。
    let sessions = state.sessions;
    let titleChanged = false;

    for (const envelope of events) {
      const sessionId = envelope.session_id;
      const event = envelope.event;
      // title_changed：直接改对应会话标题，不进入会话视图状态机。
      if (event.type === 'title_changed' && typeof event.title === 'string') {
        const idx = sessions.findIndex((s) => s.id === sessionId);
        if (idx >= 0 && sessions[idx].title !== event.title) {
          if (!titleChanged) {
            sessions = sessions.slice();
            titleChanged = true;
          }
          sessions[idx] = { ...sessions[idx], title: event.title };
        }
        continue;
      }
      const targetsCurrent = !!currentSessionId && sessionId === currentSessionId;
      const initial = sessionViewCaches.get(sessionId)
        || (targetsCurrent
          ? sessionViewCacheFromState(state)
          : emptySessionViewCache(sessionRunStatuses[sessionId]));
      const next = applyEventToSessionView(initial, event);
      sessionViewCaches.set(sessionId, next);

      if (next.runStatus === 'idle') delete sessionRunStatuses[sessionId];
      else sessionRunStatuses[sessionId] = next.runStatus;

      if (targetsCurrent) {
        currentCache = next;
        refreshCurrentAgents = refreshCurrentAgents
          || event.type === 'agent_created'
          || event.type === 'agent_status_changed'
          || (event.type === 'session_message_upsert'
            && !!event.message
            && typeof event.message !== 'string'
            && isAgentSystemMessage(event.message));
      }
    }

    set({
      ...(currentCache ? {
        messages: currentCache.messages,
        historyStart: currentCache.historyStart,
        userOutline: currentCache.userOutline,
        runStatus: currentCache.runStatus,
        runSummary: currentCache.runSummary,
        lastDurationMs: currentCache.lastDurationMs,
        toolCallStartedAt: currentCache.toolCallStartedAt,
        toolCallFinished: currentCache.toolCallFinished,
        lastUsage: currentCache.lastUsage,
        tokenStats: currentCache.tokenStats,
        streamingMessageId: currentCache.streamingMessageId,
        streamingContent: currentCache.streamingContent,
        streamingReasoningContent: currentCache.streamingReasoningContent,
        agents: refreshCurrentAgents
          ? parseAgentsFromMessages(currentCache.messages)
          : state.agents,
      } : {}),
      ...(titleChanged ? { sessions } : {}),
      sessionRunStatuses,
    });

    const nextState = get();
    const appIsForeground = document.visibilityState === 'visible' && document.hasFocus();
    for (const sessionId of Object.keys(previousStatuses)) {
      if (!nextState.sessionRunStatuses[sessionId]) {
        // 执行结束的会话若留有输入队列且草稿为空，自动放行队首。
        if ((nextState.inputQueues[sessionId]?.length ?? 0) > 0) {
          void get().dequeueNextQueuedInput(sessionId);
        }
        if (sessionId !== nextState.activeSessionId || !appIsForeground) {
          const session = nextState.sessions.find((item) => item.id === sessionId);
          notifyBackgroundSessionCompleted(session?.title || '对话', sessionId).catch(console.warn);
        }
      }
    }
  },
}));
