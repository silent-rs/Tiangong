/**
 * 中枢对话的会话切换（独立于共用 store 的中枢逻辑，见 lib/centralSession）。
 *
 * 中枢对话固定 Session ID、单独存储（后端 `src-tauri central.rs`）；进入
 * 中枢模式时成为当前对话，退出时恢复进入前的普通模式上下文。切换状态
 * （进入前上下文、竞态版本）保存在本模块，不进入共用 AppState——中枢
 * 相关逻辑全部收敛在 `lib/central*` 与 `components/central/`，普通模式
 * 文件只保留最小挂钩（模式开关与列表/通知的两处中枢判定，见 useStore）。
 */
import { api } from '@/api/tauri';
import { useStore } from '@/store/useStore';
import { CENTRAL_SESSION_ID } from './centralSession';

/** 进入中枢前的普通模式上下文（内存态）；null 表示无记录。 */
let preCentralContext: {
  activeSessionId: string | null;
  newConversationId: string | null;
} | null = null;

/** 中枢切换竞态版本：快速来回切换模式时作废在途的旧切换。 */
let centralSwitchVersion = 0;

/** 进入中枢模式：确保中枢对话成为当前对话。 */
export async function ensureCentralConversation(): Promise<void> {
  const version = ++centralSwitchVersion;
  const cur = useStore.getState();
  // 已在中枢对话（含 turn 进行中重复进入）：仅确保上下文已记录（幂等）
  if (cur.activeSessionId === CENTRAL_SESSION_ID) {
    if (!preCentralContext) {
      preCentralContext = { activeSessionId: null, newConversationId: null };
    }
    return;
  }
  // 激活问候（§4.2）：进入时注入状态摘要驱动中枢主动问候（后端幂等，
  // 每进程首次；失败静默不阻塞切换）
  void api.centralActivationGreeting().catch(() => undefined);
  // 记录进入前的普通模式上下文（退出中枢时恢复）
  preCentralContext = {
    activeSessionId: cur.activeSessionId,
    newConversationId: cur.newConversationId,
  };
  // 中枢对话历史从中枢存储加载；尚未创建（首次进入）按空对话处理。
  const loaded = await api.loadSession(CENTRAL_SESSION_ID).catch(() => null);
  // 加载期间模式被来回切换：放弃本次写入
  if (version !== centralSwitchVersion) return;
  const knownRunStatus = useStore.getState().sessionRunStatuses[CENTRAL_SESSION_ID];
  useStore.setState({
    isNewConversation: false,
    activeSessionId: CENTRAL_SESSION_ID,
    newConversationId: null,
    messages: loaded?.messages ?? [],
    historyStart: 0,
    userOutline: null,
    runStatus: knownRunStatus || 'idle',
    runSummary: knownRunStatus ? '正在处理' : '',
    lastDurationMs: null,
    toolCallStartedAt: {},
    toolCallFinished: {},
    lastUsage: null,
    tokenStats: null,
    streamingMessageId: null,
    streamingContent: '',
    streamingReasoningContent: '',
    sessionCwd: useStore.getState().workspaceDir,
    agents: [],
  });
}

/** 退出中枢模式：恢复进入前的普通模式会话（原会话或原新对话草稿）。 */
export async function restoreFromCentralConversation(): Promise<void> {
  // 作废在途的 ensure；后续切回动作由 store 自身的版本控制保证时序。
  ++centralSwitchVersion;
  const context = preCentralContext;
  preCentralContext = null;
  if (!context) return;
  if (context.activeSessionId) {
    const cur = useStore.getState();
    if (cur.activeSessionId === context.activeSessionId) return;
    // 列表尚未加载（空）时不能断定原会话已被删除：先刷新一次再判定，
    // 否则会误丢用户原会话、落到无关的新对话。
    let sessions = cur.sessions;
    if (!sessions.length) {
      await useStore.getState().loadSessions();
      sessions = useStore.getState().sessions;
    }
    if (sessions.some((session) => session.id === context.activeSessionId)) {
      await useStore.getState().switchSession(context.activeSessionId);
      return;
    }
    await useStore.getState().startNewConversation();
    return;
  }
  if (context.newConversationId) {
    // 进入前是新对话：恢复原草稿 ID。中枢对话自身的草稿缓存一并保留
    // （不清理任何 inputCaches 键）。
    restoreDraftConversation(context.newConversationId);
    return;
  }
  await useStore.getState().startNewConversation();
}

/** 以既有草稿 ID 恢复普通新对话态（不动任何输入缓存）。 */
function restoreDraftConversation(draftId: string): void {
  useStore.setState({
    isNewConversation: true,
    activeSessionId: null,
    newConversationId: draftId,
    messages: [],
    historyStart: 0,
    userOutline: null,
    runStatus: 'idle',
    runSummary: '',
    lastDurationMs: null,
    toolCallStartedAt: {},
    toolCallFinished: {},
    lastUsage: null,
    tokenStats: null,
    streamingMessageId: null,
    streamingContent: '',
    streamingReasoningContent: '',
    sessionCwd: useStore.getState().workspaceDir,
    agents: [],
  });
}

/** 仅测试用：重置模块级上下文与版本。 */
export function __resetCentralConversationForTests(): void {
  preCentralContext = null;
  centralSwitchVersion = 0;
}
