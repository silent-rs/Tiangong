export type MentionTarget = { kind: 'global' } | { kind: 'session'; session_id: string } | { kind: 'draft'; workspace: string };
export interface MentionRequest {
  target: MentionTarget;
  query: string;
  allowed_kinds?: string[];
  max_per_group?: number;
}

import { invoke, listen } from './host';

// ============================================================================
// 类型定义
// ============================================================================

/** 手动安装或更新 Sandbox 的结果。 */
export type LauncherUpdateResult = { status: 'installed'; version: string };

/** Sandbox 状态（启动准备页与设置页共用）。 */
export interface SandboxUpdateState {
  status: 'missing' | 'preparing' | 'ready' | 'failed';
  version: string | null;
  failure: string | null;
}

export interface StartupPrepareResult {
  installed_version: string | null;
  /** 沙箱不可用等降级原因：应用仍可进入，仅插件工具受限。 */
  degraded_reason: string | null;
  /** 启动失败的插件清单（各插件已标记异常，设置页可查详情）。 */
  plugin_failures: string[];
}

/** 内置注入类环境变量屏蔽清单（管理 Modal 提示与保存去重用）。 */
export interface BuiltinEnvBlocklist {
  /** 精确变量名（匹配大小写不敏感）。 */
  exact: string[];
  /** 变量名前缀（LD_/DYLD_ 动态加载注入类）。 */
  prefixes: string[];
}

export interface Session {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  message_count: number;
  /** 会话工作目录，用于按 workspace 分组展示 */
  cwd: string;
}

export interface DeleteResult {
  succeeded: string[];
  failed: string[];
}

export interface TrashedSession {
  id: string;
  title: string;
  message_count: number;
  updated_at: string;
  purging: boolean;
}

export interface PurgeProgress {
  current: number;
  total: number;
  session_id: string;
  title: string;
  status: string;
}

export interface LoadedSession {
  id: string;
  messages: Message[];
  /** `messages` 第一条在完整消息序列中的下标；0 表示已是全部历史。 */
  start: number;
  /** 完整消息条数。 */
  total: number;
  /** 完整的用户提问目录（刻度条、回合跳转使用）。 */
  user_outline: UserOutlineItem[];
  token_stats: TokenStats;
  last_duration_ms?: number;
  last_usage?: TokenUsage;
  cwd: string;
  reasoning_effort: string;
}

/** 用户提问目录项。 */
export interface UserOutlineItem {
  id: string;
  question: string;
  answer: string;
}

/** 向前分页加载的一段历史。 */
export interface SessionMessagesPage {
  messages: Message[];
  /** 本段第一条在完整消息序列中的下标；0 表示已到最早。 */
  start: number;
  total: number;
}

/** 拓展区 App tab 类型。browser/terminal 为旧内置时代的存量值（布局
 *  反序列化兼容），新生 tab 一律为 plugin（App 全部插件化）。 */
export type TabKind = 'browser' | 'terminal' | 'plugin';

export interface TabState {
  id: string;
  kind: TabKind;
  title: string;
  url: string;
  created_at: string;
  /** plugin tab 专属：贡献来源插件（三方 App 实例）。 */
  plugin_id?: string;
  /** plugin tab 专属：extension.tab 贡献 ID。 */
  contribution_id?: string;
  /** plugin tab 专属：沙箱级别（shadow/iframe）。 */
  sandbox?: SandboxKind;
  /** plugin tab 专属：实例持有后端资源（manifest `instance_resources`）。 */
  instance_resources?: boolean;
}

/** notice：系统发给用户的通知（如轮次失败原因），仅前端可见，不进模型上下文。 */
export type MessageRole = 'system' | 'user' | 'assistant' | 'tool' | 'notice';

/** 单个对话轮次的执行状态（持久化在起轮的用户消息上，历史会话同样可见）。
 *  processing：已起轮尚未收尾；引导消息不携带状态。 */
export type TurnStatus = 'processing' | 'success' | 'failed' | 'cancelled';
export interface TokenUsage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  prompt_cache_hit_tokens?: number | null;
  prompt_cache_miss_tokens?: number | null;
  cache_hit_rate?: number | null;
}

export interface MessageUsage extends TokenUsage {
  model: string;
  agent_id: string;
  turn_id: string | null;
  source: string;
  status: TurnStatus;
}

export interface TokenStats {
  current_tokens: number;
  compression_threshold_tokens: number;
  context_limit_tokens: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_tokens: number;
  active_agent_current_tokens: number;
  active_agent_id: string | null;
  agent_current_tokens: Record<string, number>;
  agent_token_usage: Record<string, TokenUsage>;
}

export interface BalanceInfo {
  currency: string;
  total_balance: string;
  granted_balance: string;
  topped_up_balance: string;
}

export interface ProviderBalance {
  is_available: boolean;
  balance_infos: BalanceInfo[];
}

/** ChatGPT（Codex）账号登录状态（不含令牌）。 */
export interface CodexAuthStatus {
  logged_in: boolean;
  email?: string;
  plan_type?: string;
  account_id?: string;
  expires_at?: number;
  login_pending: boolean;
}

/** ChatGPT 额度窗口（如 5 小时 / 每周）。 */
export interface CodexUsageWindow {
  used_percent: number;
  window_seconds?: number;
  reset_at?: number;
}

export interface CodexNamedLimit {
  name: string;
  windows: CodexUsageWindow[];
  limit_reached: boolean;
}

/** ChatGPT 账号用量额度。 */
export interface CodexUsage {
  plan_type?: string;
  allowed: boolean;
  limit_reached: boolean;
  windows: CodexUsageWindow[];
  extra_limits: CodexNamedLimit[];
  credits_balance?: string;
  credits_unlimited: boolean;
}

export interface CodexLoginStart {
  url: string;
  user_code?: string;
}

export type MediaKind = 'image' | 'video' | 'audio' | 'file';

export interface StoredAsset {
  asset_id: string;
  local_path: string;
  original_name: string;
  mime_type: string;
  size: number;
  kind: MediaKind;
}

export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'model_instruction'; text: string }
  | {
      type: 'media';
      kind: MediaKind;
      url: string;
      mime_type?: string;
      title?: string;
    }
  | { type: 'asset_reference'; asset: StoredAsset }
  | {
      type: 'image';
      asset: StoredAsset;
      data?: string;
    };

export interface MediaAsset {
  kind: MediaKind;
  url: string;
  mime_type?: string;
  title?: string;
  capability?: string;
}

export interface RawAttachment {
  kind: MediaKind;
  source: string;
  original_name?: string;
  mime_type?: string;
}

export interface InputCache {
  text: string;
  attachments: RawAttachment[];
  is_sending: boolean;
  revision: number;
}

export interface AttachmentDataUrl {
  data_url: string;
  mime_type: string;
  title: string;
  base64_size: number;
}

export * from './message';
import type { Message, MessageRender } from './message';

/** Core 经 Desktop 按会话转发的单个流事件。 */
export interface StreamEvent {
  type: string;
  message_id?: string;
  content?: string;
  content_blocks?: ContentBlock[];
  media?: MediaAsset[];
  message?: Message | string;
  name?: string;
  names?: string[];
  calls?: { id: string; name: string; arguments?: unknown }[];
  tool_call_id?: string | null;
  ok?: boolean;
  output?: string;
  duration_ms?: number | null;
  usage?: TokenUsage | null;
  current_tokens?: number | null;
  compression_threshold_tokens?: number | null;
  context_limit_tokens?: number | null;
  source?: string;
  agent_id?: string | null;
  role?: string;
  args_summary?: string;
  attempt?: number;
  max_attempts?: number;
  phase?: string;
  seconds?: number;
  strategy?: string;
  hit_count?: number;
  label?: string;
  status?: string;
  action?: string;
  summary_up_to?: number;
  remaining_messages?: number;
  count?: number;
  holder_agent_label?: string | null;
  path?: string;
  /** title_changed 事件携带的新标题。 */
  title?: string;
  /** model_switch_started / model_switched 携带的模型名（用于展示）。 */
  model_name?: string;
  /** user_message 携带的插件渲染声明（写入消息 meta.render）。 */
  render?: MessageRender | null;
}

export interface SessionStreamEvent {
  session_id: string;
  event: StreamEvent;
}

/** 提取消息的纯文本内容，兼容旧格式 content 为字符串的情况 */
export function textContent(msg: Message): string {
  const content = msg.content;
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content
    .flatMap((block) => block.type === 'text' && block.text ? [block.text] : [])
    .join('');
}

/** 消息是否包含媒体内容块 */
export function hasMediaBlocks(msg: Message): boolean {
  const content = msg.content;
  if (!Array.isArray(content)) return false;
  return content.some((b) =>
    b.type === 'media' || b.type === 'asset_reference' || b.type === 'image'
  );
}

// --- 通讯网关（bot）类型 ---

export type FieldType =
  | { kind: 'string' }
  | { kind: 'secret' }
  | { kind: 'boolean' }
  | { kind: 'barcode' }
  | { kind: 'select'; options: string[] };

export interface ConfigFieldSchema {
  key: string;
  label: string;
  field_type: FieldType;
  required: boolean;
  default?: unknown;
  help?: string;
}

export interface BotConfig {
  id: string;
  artifact_id: string;
  enabled: boolean;
  config: Record<string, unknown>;
  created_at: string;
  updated_at: string;
}

export interface RegisterBotRequest {
  id: string;
  artifact_id: string;
  config?: Record<string, unknown>;
  enabled?: boolean;
}

export interface UpdateBotRequest {
  config?: Record<string, unknown>;
}

export interface BotArtifact {
  url: string;
  checksum: string;
}

export interface BotManifest {
  id: string;
  name: string;
  version: string;
  description: string;
  config_schema: ConfigFieldSchema[];
  platforms: Record<string, BotArtifact>;
  min_app_version?: string;
}

export interface BotsIndex {
  version: number;
  bots: BotManifest[];
}

export interface LocalArtifact {
  id: string;
  name: string;
  artifact_id: string;
  version: string;
  config_schema: ConfigFieldSchema[];
  supports_mcp: boolean;
}

export interface BotPushTarget {
  target_id: string;
  label: string;
  kind: 'direct' | 'group' | string;
  enabled: boolean;
  availability: 'ready' | 'reply_window' | 'unavailable' | 'unknown' | string;
  last_seen_at: string;
  limitation?: string;
}

export interface QrSession {
  qr_url: string;
  expires_at: number;
  interval: number;
  state: unknown;
}

export interface BotLog {
  content: string;
  truncated: boolean;
}

export interface BotTransferProgress {
  downloaded: number;
  total: number;
}

/// 插件安装/升级下载进度事件。
export interface PluginInstallProgress {
  plugin_id: string;
  downloaded: number;
  total: number;
}

// ============================================================================
// 插件 Harness（Slot / Seam / UI 贡献）
// ============================================================================

/** 首版 Slot 目录（与后端 BUILTIN_SLOTS 对齐，字段 snake_case）。 */
export const SLOT_IDS = [
  'session.turn-node',
  'session.message-item',
  'session.message-action',
  'session.input-action',
  'session.before-input',
  'session.after-input',
  'session.input-status',
  'session.interaction',
  'session.input-overlay',
  'session.empty-state',
  'extension.tab',
  'extension.side',
  'sidebar.nav-item',
  'sidebar.panel',
  'sidebar.bottom',
  'settings.plugin-page',
  'global.status-item',
  'global.command',
  'global.toast-action',
] as const;

/** 挂载点稳定 ID。 */
export type SlotId = (typeof SLOT_IDS)[number];

/** Slot 可注入的上下文键。 */
export type SlotContextKey = 'session' | 'turn' | 'message' | 'workspace';

/** 接缝类别（与后端 SeamKind 对齐）。 */
export type SeamKind =
  | 'tool'
  | 'prompt'
  | 'lifecycle'
  | 'ui'
  | 'approval'
  | 'interaction'
  | 'event'
  | 'storage';

/** App 打开模式，仅对 `extension.tab` 生效。 */
export type OpenMode = 'singleton' | 'multi';

/** UI 贡献的沙箱级别。 */
export type SandboxKind = 'shadow' | 'iframe' | 'native' | 'webview';

/** manifest `ui.contributions[]` 声明的 UI 贡献。 */
export interface UiContribution {
  slot: SlotId;
  id: string;
  title: string;
  icon: string;
  entry: string;
  open_mode: OpenMode;
  context: string[];
  sandbox: SandboxKind;
}

/** manifest v2 `capabilities` 能力声明。 */
export interface PluginCapabilities {
  tools: boolean;
  prompt: boolean;
  lifecycle: boolean;
  approval: boolean;
  interaction: boolean;
  events: string[];
}

/** 拓展区 App 元数据（插件的 extension.tab 贡献）。 */
export interface AppEntry {
  plugin_id: string;
  contribution_id: string;
  /** 插件名（矩阵主标题）。 */
  name: string;
  title: string;
  description: string;
  icon: string;
  open_mode: OpenMode;
  sandbox: SandboxKind;
  /** 实例持有后端资源（manifest `instance_resources`）：宿主接管关闭/恢复/核查。 */
  instance_resources?: boolean;
}

/** 会话下持有资源的插件实例（宿主 listInstances 聚合，恢复标签用）。 */
export interface PluginInstanceEntry {
  plugin_id: string;
  contribution_id: string;
  title: string;
  sandbox: SandboxKind;
  instance_id: string;
  url: string;
  page_title: string;
}

/** Slot 元数据（来自后端 SlotDescriptor）。 */
export interface SlotDescriptorInfo {
  id: SlotId;
  instances: 'singleton' | 'multiple';
  context: SlotContextKey[];
  description: string;
}

/** 宿主桥接事件推送（bridge_event）。 */
export interface BridgeEventPayload {
  plugin_id: string;
  channel: string;
  payload: string;
}

/** 按挂载点查询得到的统一 UI 贡献项（对应后端 SlotContribution）。 */
export interface SlotContributionEntry {
  plugin_id: string;
  contribution_id: string;
  slot: SlotId;
  title: string;
  description: string;
  icon: string;
  group: string;
  has_view: boolean;
  open_mode: OpenMode;
  sandbox: SandboxKind;
  /** 贡献来源：wasm（v1 运行时声明）或 manifest（v2 清单声明）。 */
  source: 'wasm' | 'manifest';
  /** session.message-item 的渲染方式：缺省附加区；replace 按消息渲染声明替换默认显示。 */
  render?: 'append' | 'replace';
}

export interface SessionInputAttachmentPayload {
  plugin_id: string;
  /**
   * kind="text" 为插件文本（session.input.sendText / insertText）：
   * mode="insert" 只写入草稿，否则发送，attachments 为随文本发送的音频附件。
   * 其余 kind 同草稿附件（PNG 图片 / 媒体目录内音频）。
   */
  attachment: Omit<RawAttachment, 'kind'> & {
    kind: RawAttachment['kind'] | 'text';
    text?: string;
    mode?: 'send' | 'insert';
    attachments?: RawAttachment[];
  };
}

/** 输入覆盖层显隐请求（session.input.showOverlay / hideOverlay）。 */
export interface SessionInputOverlayPayload {
  plugin_id: string;
  visible: boolean;
  session_id?: string | null;
  /** 覆盖层显示期间替换输入区底部快捷键提示的文案。 */
  hint?: string | null;
}

/** 工具图标查询表条目：图标名或插件内资源路径（资源图标带所属插件）。 */
export interface ToolIconEntry {
  icon: string;
  plugin_id?: string | null;
}

/** 插件入口资源响应（字节数组 + MIME）。 */
export interface PluginEntryResource {
  data: number[];
  mime: string;
}

export type ProvisionStatus =
  | { status: 'pending'; retry_after?: number }
  | { status: 'success' }
  | { status: 'expired' }
  | { status: 'error'; message: string };

export type BotHealth =
  | 'running'
  | 'stopped'
  | 'missing_artifact'
  | { error: { message: string } };

export interface ServerConfig {
  host: string;
  port: number;
  auth_token_masked: string;
  enabled: boolean;
  running: boolean;
  status: 'stopped' | 'running' | 'error';
}

// 模型配置（Provider + Model + Routing 三层架构）

export interface ProviderConfigView {
  headers?: Record<string, string>;
  base_url: string;
  api_key: string;
  timeout_ms: number;
  protocol: string;
}

export interface ModelEntryView {
  provider: string;
  model: string;
  capabilities: string[];
  options: Record<string, unknown>;
  context_window?: number;
}

/** 供应商模型目录项；context_window 为服务端声明的上下文窗口（目前仅 ChatGPT 提供）。 */
export interface ProviderModelInfo {
  id: string;
  display_name?: string | null;
  context_window?: number | null;
}

export interface ModelsConfigView {
  providers: Record<string, ProviderConfigView>;
  models: Record<string, ModelEntryView>;
  routing: Record<string, ModelEntryView>;
}

export interface ModelCapabilityInfo {
  key: string;
  display_name: string;
}

export interface CapabilityAvailabilityInfo {
  key: string;
  display_name: string;
  enabled: boolean;
  routed_model?: string;
}

// ============================================================================
// 定时任务 & Webhook
// ============================================================================


export interface Webhook {
  id: string;
  name: string;
  description: string;
  session_id: string | null;
  payload: string;
  secret: string | null;
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export type WebhookRunStatus = 'running' | 'succeeded' | 'failed';

export interface WebhookRun {
  id: string;
  webhook_id: string;
  session_id: string;
  status: WebhookRunStatus;
  started_at: string;
  finished_at: string | null;
  result_summary: string | null;
}

// ============================================================================
// API 方法
// ============================================================================

export type TrustedPublisherEntry = {
  publisher: string;
  public_key_b64: string;
  fingerprint: string;
  imported_at: string;
};

export interface SandboxPolicyView {
  directory_allowlist: string[];
  environment_blocklist: string[];
}

export const api = {
  // ----------------------------------------------------------------
  // 会话管理
  // ----------------------------------------------------------------
  getSessions: (): Promise<Session[]> =>
    invoke('get_sessions'),

  switchSession: (sessionId: string): Promise<void> =>
    invoke('switch_session', { sessionId }),

  loadSession: (sessionId: string): Promise<LoadedSession> =>
    invoke('load_session', { sessionId, paged: true }),

  /** 向前分页加载 `beforeId` 之前的一段历史消息。 */
  loadSessionMessages: (sessionId: string, beforeId: string): Promise<SessionMessagesPage> =>
    invoke('load_session_messages', { sessionId, beforeId }),

  getSessionMeta: (sessionId: string): Promise<Session | null> =>
    invoke('get_session_meta', { sessionId }),

  deleteSession: (sessionId: string): Promise<void> =>
    invoke('delete_session', { sessionId }),

  deleteSessionsByCwd: (cwd: string): Promise<DeleteResult> =>
    invoke('delete_sessions_by_cwd', { cwd }),

  listTrashedSessions: (): Promise<TrashedSession[]> =>
    invoke('list_trashed_sessions'),

  purgeAllDeletedSessions: (): Promise<number> =>
    invoke('purge_all_deleted_sessions'),

  restoreDeletedSession: (sessionId: string): Promise<void> =>
    invoke('restore_deleted_session', { sessionId }),

  onPurgeProgress: (cb: (progress: PurgeProgress) => void): Promise<() => void> =>
    listen<PurgeProgress>('purge_progress', (event) => cb(event.payload)),

  updateSessionTitle: (title: string): Promise<void> =>
    invoke('update_session_title', { title }),

  requestDesktopNotificationPermission: (): Promise<boolean> =>
    invoke('request_desktop_notification_permission'),

  sendDesktopNotification: (title: string, body: string, sessionId?: string): Promise<boolean> =>
    invoke('send_desktop_notification', { title, body, sessionId }),

  // ----------------------------------------------------------------
  // 消息和执行
  // ----------------------------------------------------------------
  sendMessage: (
    sessionId: string,
    content: string,
    attachments: RawAttachment[],
    revision: number,
    cwd?: string,
    trustMode?: string,
    reasoningEffort?: string,
    modelRef?: string | null,
  ): Promise<void> =>
    invoke('send_message', {
      sessionId,
      content,
      attachments,
      revision,
      cwd,
      trustMode,
      reasoningEffort,
      modelRef,
    }),

  readAttachmentAsDataUrl: (path: string, maxBase64Bytes?: number): Promise<AttachmentDataUrl> =>
    invoke('read_attachment_as_data_url', { path, maxBase64Bytes }),

  cancelTurn: (sessionId: string): Promise<boolean> =>
    invoke('cancel_turn', { sessionId }),

  appendMessage: (
    sessionId: string,
    content: string,
    attachments: RawAttachment[],
    revision: number,
  ): Promise<boolean> =>
    invoke('append_message', { sessionId, content, attachments, revision }),

  editAndResend: (
    sessionId: string,
    messageId: string,
    newContent: string,
    attachments: RawAttachment[],
    revision: number,
    baseContent: ContentBlock[],
  ): Promise<void> =>
    invoke('edit_and_resend', {
      sessionId,
      messageId,
      newContent,
      attachments,
      revision,
      baseContent,
    }),

  getTrustMode: (sessionId?: string): Promise<string> =>
    invoke('get_trust_mode', { sessionId }),

  setTrustMode: (mode: string, sessionId?: string): Promise<void> =>
    invoke('set_trust_mode', { mode, sessionId }),

  getDefaultTrustMode: (): Promise<string> =>
    invoke('get_default_trust_mode'),

  setDefaultTrustMode: (mode: string): Promise<void> =>
    invoke('set_default_trust_mode', { mode }),

  getSandboxDisabled: (): Promise<boolean> =>
    invoke('get_sandbox_disabled'),

  setSandboxDisabled: (disabled: boolean): Promise<void> =>
    invoke('set_sandbox_disabled', { disabled }),

  getSandboxPolicy: (): Promise<SandboxPolicyView> =>
    invoke('get_sandbox_policy'),

  setSandboxPolicy: (policy: SandboxPolicyView): Promise<SandboxPolicyView> =>
    invoke('set_sandbox_policy', { policy }),

  getCommandEnvBlocklist: (): Promise<string[]> =>
    invoke('get_command_env_blocklist'),

  setCommandEnvBlocklist: (blocklist: string[]): Promise<void> =>
    invoke('set_command_env_blocklist', { blocklist }),

  /** 手动安装或更新固定路径中的 Sandbox。 */
  upgradeLauncher: (): Promise<LauncherUpdateResult> =>
    invoke('upgrade_launcher'),

  getSandboxUpdateState: (): Promise<SandboxUpdateState> =>
    invoke('get_sandbox_update_state'),

  prepareStartupResources: (): Promise<StartupPrepareResult> =>
    invoke('prepare_startup_resources'),


  getBuiltinEnvBlocklist: (): Promise<BuiltinEnvBlocklist> =>
    invoke('get_builtin_env_blocklist'),


  getSessionModel: (sessionId: string): Promise<string | null> =>
    invoke('get_session_model', { sessionId }),

  listSessionChatModels: (): Promise<{
    models: { key: string; label: string; provider: string }[];
    default_ref: string | null;
  }> =>
    invoke('list_session_chat_models'),

  setSessionModel: (sessionId: string, modelRef: string | null): Promise<void> =>
    invoke('set_session_model', { sessionId, modelRef }),

  getReasoningEffort: (sessionId?: string): Promise<string> =>
    invoke('get_reasoning_effort', { sessionId }),

  setReasoningEffort: (effort: string, sessionId?: string): Promise<void> =>
    invoke('set_reasoning_effort', { effort, sessionId }),

  getProviderBalance: (providerName: string): Promise<ProviderBalance> =>
    invoke('get_provider_balance', { providerName }),

  codexAuthStatus: (): Promise<CodexAuthStatus> =>
    invoke('codex_auth_status'),

  /** 发起 ChatGPT 账号登录（后端会自动打开浏览器）；method: browser | device */
  codexAuthStart: (method: 'browser' | 'device' = 'browser'): Promise<CodexLoginStart> =>
    invoke('codex_auth_start', { method }),

  codexAuthWait: (): Promise<CodexAuthStatus> =>
    invoke('codex_auth_wait'),

  codexAuthCancel: (): Promise<CodexAuthStatus> =>
    invoke('codex_auth_cancel'),

  codexAuthLogout: (): Promise<CodexAuthStatus> =>
    invoke('codex_auth_logout'),

  /** 手动刷新 ChatGPT 登录令牌（工具侧令牌过期时使用）。 */
  codexAuthRefresh: (): Promise<CodexAuthStatus> =>
    invoke('codex_auth_refresh'),
  /** 查询 ChatGPT 账号用量额度（只读，不消耗额度）。 */
  codexAuthUsage: (): Promise<CodexUsage> =>
    invoke('codex_auth_usage'),

  newSessionId: (): Promise<string> =>
    invoke('new_session_id'),

  removeInputCache: (cacheKey: string): Promise<void> =>
    invoke('remove_input_cache', { cacheKey }),

  getInputCache: (cacheKey: string): Promise<InputCache> =>
    invoke('get_input_cache', { cacheKey }),

  setInputCache: (
    cacheKey: string,
    cache: InputCache,
    claimRevision?: number,
  ): Promise<InputCache> =>
    invoke('set_input_cache', { cacheKey, cache, claimRevision }),

  setSessionCwd: (sessionId: string, cwd: string): Promise<void> =>
    invoke('set_session_cwd', { sessionId, cwd }),

  getWorkspaceDir: (): Promise<string> =>
    invoke('get_workspace_dir'),

  setWorkspaceDir: (workspaceDir: string): Promise<void> =>
    invoke('set_workspace_dir', { workspaceDir }),

  // ----------------------------------------------------------------
  // 通讯网关（bot）管理
  // ----------------------------------------------------------------
  botList: (): Promise<BotConfig[]> =>
    invoke('bot_list'),

  botHealth: (id: string): Promise<BotHealth> =>
    invoke('bot_health', { id }),

  botLog: (id: string): Promise<BotLog> =>
    invoke('bot_log', { id }),

  botConfigSchema: (artifactId: string, botId?: string): Promise<ConfigFieldSchema[]> =>
    invoke('bot_config_schema', { artifactId, botId: botId ?? null }),

  botProvisionBegin: (botId: string): Promise<QrSession> =>
    invoke('bot_provision_begin', { botId }),

  botProvisionPoll: (botId: string, session: QrSession): Promise<ProvisionStatus> =>
    invoke('bot_provision_poll', { botId, session }),

  botAvailable: (): Promise<BotsIndex> =>
    invoke('bot_available'),

  botScanLocal: (): Promise<LocalArtifact[]> =>
    invoke('bot_scan_local'),

  botPushTargets: (id: string): Promise<BotPushTarget[]> =>
    invoke('bot_push_targets', { id }),

  botDeletePushTarget: (id: string, targetId: string): Promise<string> =>
    invoke('bot_delete_push_target', { id, targetId }),

  botRegisterMcp: (id: string): Promise<string> =>
    invoke('bot_register_mcp', { id }),

  botRegister: (request: RegisterBotRequest): Promise<BotConfig> =>
    invoke('bot_register', { request }),

  botUpdate: (id: string, request: UpdateBotRequest): Promise<BotConfig> =>
    invoke('bot_update', { id, request }),

  botRemove: (id: string): Promise<string> =>
    invoke('bot_remove', { id }),

  botInstall: (artifactId: string, destBotId: string): Promise<string> =>
    invoke('bot_install', { artifactId, destBotId }),

  onBotInstallProgress: (callback: (progress: BotTransferProgress) => void) =>
    listen<BotTransferProgress>('bot_install_progress', (event) => callback(event.payload)),

  onPluginInstallProgress: (callback: (progress: PluginInstallProgress) => void) =>
    listen<PluginInstallProgress>('plugin_install_progress', (event) => callback(event.payload)),

  /** 插件安装/导入/升级/启停/回滚/卸载/重载成功后广播（拓展区刷新数据源）。 */
  onPluginsChanged: (callback: () => void) =>
    listen('plugins_changed', () => callback()),

  /** 模型配置保存后广播（会话区刷新可选模型）。 */
  onModelsConfigChanged: (callback: () => void) =>
    listen('models_config_changed', () => callback()),

  botStart: (id: string): Promise<string> =>
    invoke('bot_start', { id }),

  botStop: (id: string): Promise<string> =>
    invoke('bot_stop', { id }),

  botCheckUpdate: (artifactId: string): Promise<BotManifest | null> =>
    invoke('bot_check_update', { artifactId }),

  botUpgrade: (botId: string): Promise<string> =>
    invoke('bot_upgrade', { botId }),

  // ----------------------------------------------------------------
  // Server 管理
  // ----------------------------------------------------------------
  getServerConfig: (): Promise<ServerConfig> =>
    invoke('get_server_config'),

  setServerConfig: (host: string, port: number, authToken?: string): Promise<string> =>
    invoke('set_server_config', { host, port, authToken }),

  startServer: (): Promise<string> =>
    invoke('start_server'),

  stopServer: (): Promise<string> =>
    invoke('stop_server'),

  // ----------------------------------------------------------------
  // 模型配置（Provider + Model + Routing）
  // ----------------------------------------------------------------
  getModelsConfig: (): Promise<ModelsConfigView> =>
    invoke('get_models_config'),

  setModelsConfig: (config: ModelsConfigView): Promise<void> =>
    invoke('set_models_config', { config }),

  // 预热工作区索引（索引已存在则直接返回，否则后台扫描，立即返回不阻塞）
  // 索引管理（列表/删除/重建）由「设置 → 索引管理」页经插件 UI 通道处理。
  prewarmWorkspaceIndex: (root: string): Promise<void> =>
    invoke('prewarm_workspace_index', { root }),

  getModelCapabilities: (): Promise<ModelCapabilityInfo[]> =>
    invoke('get_model_capabilities'),

  getAvailableCapabilities: (): Promise<CapabilityAvailabilityInfo[]> =>
    invoke('get_available_capabilities'),

  hasModelCapability: (capability: string): Promise<boolean> =>
    invoke('has_model_capability', { capability }),

  getModelList: (): Promise<string[]> =>
    invoke('get_model_list'),

  fetchProviderModels: (
    baseUrl: string,
    apiKey: string,
    timeoutMs?: number,
    protocol?: string,
    headers?: Record<string, string>,
  ): Promise<string[]> =>
    invoke('fetch_provider_models', { baseUrl, apiKey, timeoutMs, protocol, headers }),
  /** 拉取模型目录并保留服务端元信息（ChatGPT 返回上下文窗口）。 */
  fetchProviderModelInfos: (
    baseUrl: string,
    apiKey: string,
    timeoutMs?: number,
    protocol?: string,
    headers?: Record<string, string>,
  ): Promise<ProviderModelInfo[]> =>
    invoke('fetch_provider_model_infos', { baseUrl, apiKey, timeoutMs, protocol, headers }),

  resolveModelContextWindow: (model: string): Promise<number> =>
    invoke('resolve_model_context_window', { model }),

  // ----------------------------------------------------------------
  // @提及补全
  // ----------------------------------------------------------------
  getMentionCandidates: (): Promise<{ value: string; label: string; kind: string; hint: string; mark?: string }[]> =>
    invoke('get_mention_candidates'),

  /** 获取按 kind 分组的 @提及候选（App 层统一分组/过滤/截断）。 */
  getMentionGroups: (
    allowedKinds?: string[],
    maxPerGroup?: number,
    request?: MentionRequest,
  ): Promise<{ kind: string; label: string; candidates: { value: string; label: string; kind: string; hint: string; mark?: string }[] }[]> =>
    invoke('get_mention_groups', { allowedKinds, maxPerGroup, request }),

  // ----------------------------------------------------------------
  // 上下文管理
  // ----------------------------------------------------------------
  compressContext: (): Promise<boolean> =>
    invoke('compress_context'),

  resetContext: (): Promise<boolean> =>
    invoke('reset_context'),

  // ----------------------------------------------------------------
  // 事件监听
  // ----------------------------------------------------------------
  onStreamEvent: (callback: (event: SessionStreamEvent) => void) =>
    listen<SessionStreamEvent>('stream_event', (event) => callback(event.payload)),

  // ----------------------------------------------------------------
  // Webhook 管理
  // ----------------------------------------------------------------
  webhookList: (): Promise<Webhook[]> =>
    invoke('webhook_list'),

  webhookCreate: (params: {
    name: string;
    description: string;
    sessionId?: string;
    payload: string;
    secret?: string;
    enabled?: boolean;
  }): Promise<Webhook> =>
    invoke('webhook_create', params),

  webhookUpdate: (params: {
    id: string;
    name?: string;
    description?: string;
    sessionId?: string;
    payload?: string;
    secret?: string;
    enabled?: boolean;
  }): Promise<Webhook> =>
    invoke('webhook_update', params),

  webhookDelete: (id: string): Promise<void> =>
    invoke('webhook_delete', { id }),

  webhookTrigger: (id: string): Promise<{ webhook_id: string; session_id: string; status: string }> =>
    invoke('webhook_trigger', { id }),

  webhookListRuns: (id: string, limit?: number): Promise<WebhookRun[]> =>
    invoke('webhook_list_runs', { id, limit }),

  // ── 插件 UI 桥接（WASM 插件动态 UI）──
  // 天工只提供通用桥接，不处理具体插件业务。

  listPluginContributions: (): Promise<PluginContributionEntry[]> =>
    invoke('list_plugin_contributions'),

  listPlugins: (): Promise<PluginStatus[]> => invoke('list_plugins'),

  listAvailablePlugins: (): Promise<AvailablePlugin[]> => invoke('list_available_plugins'),

  checkDefaultPlugins: (): Promise<DefaultPluginCheck> => invoke('check_default_plugins'),

  completeFirstLaunch: (): Promise<void> => invoke('complete_first_launch'),

  importLocalPlugin: (path: string): Promise<PluginStatus> =>
    invoke('import_local_plugin', { path }),

  listTrustedPublishers: (): Promise<TrustedPublisherEntry[]> =>
    invoke('plugin_list_trusted_publishers'),

  importTrustedPublisher: (publisher: string, publicKey: string): Promise<TrustedPublisherEntry> =>
    invoke('plugin_import_trusted_publisher', { publisher, publicKey }),

  removeTrustedPublisher: (publisher: string): Promise<boolean> =>
    invoke('plugin_remove_trusted_publisher', { publisher }),

  userKeyFingerprint: (): Promise<string | null> =>
    invoke('plugin_user_key_fingerprint'),

  readPublicKeyFile: (path: string): Promise<string> =>
    invoke('plugin_read_public_key_file', { path }),

  installPlugin: (pluginId: string): Promise<PluginStatus> =>
    invoke('install_plugin', { pluginId }),

  upgradePlugin: (pluginId: string): Promise<PluginStatus> =>
    invoke('upgrade_plugin', { pluginId }),

  setPluginEnabled: (pluginId: string, enabled: boolean): Promise<PluginStatus> =>
    invoke('set_plugin_enabled', { pluginId, enabled }),

  rollbackPlugin: (pluginId: string): Promise<PluginStatus> =>
    invoke('rollback_plugin', { pluginId }),

  uninstallPlugin: (pluginId: string, keepData: boolean): Promise<void> =>
    invoke('uninstall_plugin', { pluginId, keepData }),

  reloadPlugin: (pluginId: string): Promise<PluginStatus> =>
    invoke('reload_plugin', { pluginId }),

  /// 按需获取插件页面 HTML（用户点击进入时才调用）。
  pluginOpenView: (pluginId: string, contributionId: string): Promise<string> =>
    invoke('plugin_open_view', { pluginId, contributionId }),

  /// 通用桥接：转发到 WASM 的 handle-view-message。
  pluginCall: (pluginId: string, method: string, payload: string): Promise<string> =>
    invoke('plugin_call', { pluginId, method, payload }),

  /// 按挂载点列出 UI 贡献（v1 WASM 设置页 + v2 manifest 声明合并）。
  listSlotContributions: (slot: string): Promise<SlotContributionEntry[]> =>
    invoke('list_slot_contributions', { slot }),

  /// 同步当前会话实际挂载的 webview 插件标签。
  setWebviewMountedTabs: (sessionId: string, tabIds: string[]): Promise<void> =>
    invoke('set_webview_mounted_tabs', { sessionId, tabIds }),

  /// 列出拓展区 App（声明 extension.tab 贡献的插件，能力矩阵数据源）。
  listExtensionApps: (): Promise<AppEntry[]> =>
    invoke('list_extension_apps'),

  /// 工具图标查询表（键为工具名与 `{插件id}__{工具名}`；查不到的工具用默认图标）。
  listToolIcons: (): Promise<Record<string, ToolIconEntry>> =>
    invoke('list_tool_icons'),

  /// 读取插件在 tool_icons 中声明的图标资源。
  pluginReadToolIcon: (pluginId: string, icon: string): Promise<PluginEntryResource> =>
    invoke('plugin_read_tool_icon', { pluginId, icon }),

  /// 插件实例生命周期：宿主预留实例编号（scru128）。
  pluginInstanceReserve: (): Promise<string> =>
    invoke('plugin_instance_reserve'),

  /// 插件实例生命周期：标签已移除，宿主向资源方发出 instanceClosed。
  pluginInstanceClosed: (
    pluginId: string,
    sessionId: string,
    instanceId: string,
  ): Promise<void> =>
    invoke('plugin_instance_closed', { pluginId, sessionId, instanceId }),

  /// 插件实例生命周期：列出会话下持有资源的实例。
  pluginInstancesList: (sessionId: string): Promise<PluginInstanceEntry[]> =>
    invoke('plugin_instances_list', { sessionId }),

  /// 插件实例生命周期：提交当前标签集合，宿主释放多余资源，返回释放数量。
  pluginInstancesReconcile: (
    sessionId: string,
    live: { plugin_id: string; instance_id: string }[],
  ): Promise<number> =>
    invoke('plugin_instances_reconcile', { sessionId, live }),

  /// 插件实例生命周期：会话离开前台，隐藏其全部 webview 实例。
  pluginInstancesDetach: (sessionId: string): Promise<void> =>
    invoke('plugin_instances_detach', { sessionId }),

  /// 读取 v2 manifest UI 贡献的入口 HTML。
  pluginOpenEntry: (pluginId: string, contributionId: string): Promise<string> =>
    invoke('plugin_open_entry', { pluginId, contributionId }),

  /// 读取插件 App 的自定义图标（拓展区矩阵渲染；插件根为根、白名单与上限见宿主）。
  pluginReadIcon: (pluginId: string, contributionId: string): Promise<PluginEntryResource> =>
    invoke('plugin_read_icon', { pluginId, contributionId }),

  /// 读取 v2 manifest UI 贡献的相对资源（沙箱容器加载外链脚本/样式）。
  pluginReadEntryResource: (
    pluginId: string,
    contributionId: string,
    path: string,
  ): Promise<PluginEntryResource> =>
    invoke('plugin_read_entry_resource', { pluginId, contributionId, path }),

  // ── 宿主桥接（Host Bridge）：插件 UI ↔ 宿主统一通道 ──
  // method 按命名空间路由：plugin.* 转发到本插件 WASM，其余命名空间按接缝任务接入。

  bridgeCall: (
    pluginId: string,
    method: string,
    payload: string,
    sessionId?: string | null,
  ): Promise<string> =>
    invoke('bridge_call', { pluginId, method, payload, sessionId }),

  bridgeSubscribe: (pluginId: string, channel: string): Promise<void> =>
    invoke('bridge_subscribe', { pluginId, channel }),

  bridgeUnsubscribe: (pluginId: string, channel: string): Promise<void> =>
    invoke('bridge_unsubscribe', { pluginId, channel }),

  onSessionInputAttachment: (callback: (event: SessionInputAttachmentPayload) => void) =>
    listen<SessionInputAttachmentPayload>('session_input_attachment', (event) => callback(event.payload)),

  onSessionInputOverlay: (callback: (event: SessionInputOverlayPayload) => void) =>
    listen<SessionInputOverlayPayload>('session_input_overlay', (event) => callback(event.payload)),


  onBridgeEvent: (callback: (event: BridgeEventPayload) => void) =>
    listen<BridgeEventPayload>('bridge_event', (event) => callback(event.payload)),

  // ── 远程访问（经中继向手机 H5 提供对话侧能力） ──
  remoteGetConfig: (): Promise<RemoteAccessView> =>
    invoke('remote_get_config'),

  remoteSetConfig: (config: RemoteAccessInput): Promise<RemoteAccessView> =>
    invoke('remote_set_config', { config }),

  remoteCreatePairing: (): Promise<RemotePairing> =>
    invoke('remote_create_pairing'),

  remoteUnbindDevice: (): Promise<RemoteAccessView> =>
    invoke('remote_unbind_device'),

  remoteResetChannel: (): Promise<RemoteAccessView> =>
    invoke('remote_reset_channel'),
};

export type RemoteAccessMode = 'lan' | 'relay';

export interface RemoteAccessInput {
  enabled: boolean;
  mode: RemoteAccessMode;
  host: string;
  lanHost: string;
  lanPort: number | null;
}

export interface RemoteAccessView {
  enabled: boolean;
  mode: RemoteAccessMode;
  host: string;
  /** 缺省中继地址（host 为空时使用）。 */
  default_host: string;
  /** 通道 ID（通道密钥的单向摘要；密钥由天工自动生成，不出桌面端）。 */
  channel: string | null;
  lan_host: string;
  lan_port: number;
  detected_lan_ip: string | null;
  access_url: string | null;
  state: 'disabled' | 'connecting' | 'connected' | 'error';
  last_error: string | null;
  device_bound: boolean;
  device_label: string | null;
  device_bound_at: string | null;
  device_online: boolean;
}

export interface RemotePairing {
  url: string;
  expires_in_secs: number;
}

/// 插件设置页贡献项。
export interface PluginContributionEntry {
  plugin_id: string;
  generation: number;
  contribution_id: string;
  title: string;
  description: string;
  icon: string;
  group: string;
  /// 是否有可渲染的配置页面。
  has_view: boolean;
}

export interface PluginStatus {
  id: string;
  name: string;
  description?: string;
  manifest_version: string;
  loaded_version: string | null;
  state: 'loaded' | 'disabled' | 'degraded' | 'error' | 'invalid';
  generation: number;
  enabled: boolean;
  can_rollback: boolean;
  has_sidecar: boolean;
  sidecar_running: boolean;
  last_error: string | null;
}

export interface AvailablePlugin {
  id: string;
  name: string;
  version: string;
  description: string;
  supported: boolean;
  installed_version: string | null;
  update_available: boolean;
  /** 已安装插件的启用状态（未安装时为 false）。 */
  installed_enabled: boolean;
  is_default: boolean;
  /// 场景分类标签（多标签，`daily` / `coding` 的任意组合）。
  categories: string[];
}

/// 首次启动推荐安装检测结果。
export interface DefaultPluginCheck {
  /// 是否需要弹出首次启动推荐引导。
  first_launch_pending: boolean;
  /// 缺失的默认插件。
  missing: AvailablePlugin[];
  /// OSS 目录拉取失败原因。
  catalog_error: string | null;
}
