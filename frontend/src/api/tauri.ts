export type MentionTarget = { kind: 'global' } | { kind: 'session'; session_id: string } | { kind: 'draft'; workspace: string };
export interface MentionRequest {
  target: MentionTarget;
  query: string;
  allowed_kinds?: string[];
  max_per_group?: number;
}

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

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
  token_stats: TokenStats;
  last_duration_ms?: number;
  last_usage?: TokenUsage;
  cwd: string;
  reasoning_effort: string;
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
}

/** notice：系统发给用户的通知（如轮次失败原因），仅前端可见，不进模型上下文。 */
export type MessageRole = 'system' | 'user' | 'assistant' | 'tool' | 'notice';
export type MessagePhase = 'normal' | 'react' | 'summary' | 'compressedresume' | 'hostinjected';

/** 单个对话轮次的最终执行状态（持久化在用户消息上，历史会话同样可见）。 */
export type TurnStatus = 'success' | 'failed' | 'cancelled';
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

export interface Message {
  id: string;
  role: MessageRole;
  content: ContentBlock[];
  reasoning_content: string;
  usage?: MessageUsage | null;
  worker_id?: string;
  media?: MediaAsset[];
  tool_calls?: { id: string; name: string; arguments?: unknown }[];
  tool_call_id?: string;
  tool_name?: string;
  tool_result_is_error?: boolean;
  compact?: boolean;
  phase?: MessagePhase;
  created_at: string;
  /** 该用户消息所属轮次的执行时长（毫秒）。仅用户消息携带，前端展示「执行总时长」。 */
  elapsed_ms?: number;
  /** 该轮次的最终状态。仅用户消息携带，便于区分成功/失败/取消。 */
  turn_status?: TurnStatus;
  /** 本次模型输出思考阶段的耗时（毫秒）。仅 assistant 消息携带。 */
  reasoning_elapsed_ms?: number | null;
  /** 本次模型输出正文生成阶段的耗时（毫秒）。仅 assistant 消息携带。 */
  text_elapsed_ms?: number | null;
  /** 单次工具调用耗时（毫秒）。由 ToolResult 流式事件写入工具消息，历史消息无此字段。 */
  duration_ms?: number | null;
}

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
  agent_role?: string;
  role?: string;
  agent_label?: string;
  messages?: Message[];
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

export interface McpServer {
  name: string;
  transport: 'auto' | 'stdio' | 'http';
  command: string;
  args: string[];
  endpoint: string;
  auth_header: string;
  headers?: Record<string, string>;
  env?: Record<string, string>;
  enabled: boolean;
}

export interface RegisterMcpServerInput {
  name: string;
  transport?: 'auto' | 'stdio' | 'http' | 'sse';
  command?: string;
  args?: string[];
  endpoint?: string;
  authHeader?: string;
  headers?: Record<string, string>;
  env?: Record<string, string>;
}

export interface UpdateMcpServerInput {
  name: string;
  transport?: 'auto' | 'stdio' | 'http' | 'sse';
  command?: string;
  args?: string[];
  endpoint?: string;
  authHeader?: string;
  headers?: Record<string, string>;
  env?: Record<string, string>;
}

export interface McpHealthStatus {
  name: string;
  healthy: boolean;
  tool_count: number;
  last_error?: string;
  server_version?: string;
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
}

export interface SessionInputAttachmentPayload {
  plugin_id: string;
  /** kind="text" 为插件指令文本（session.input.sendText），其余同附件。 */
  attachment: Omit<RawAttachment, 'kind'> & { kind: RawAttachment['kind'] | 'text'; text?: string };
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
    invoke('load_session', { sessionId }),

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

  cancelTurn: (): Promise<boolean> =>
    invoke('cancel_turn'),

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
  // MCP 管理
  // ----------------------------------------------------------------
  getMcpServers: (): Promise<McpServer[]> =>
    invoke('get_mcp_servers'),

  getMcpHealth: (): Promise<McpHealthStatus[]> =>
    invoke('get_mcp_health'),

  probeMcpServer: (name: string): Promise<void> =>
    invoke('probe_mcp_server', { name }),

  registerMcpServer: (input: RegisterMcpServerInput): Promise<string> =>
    invoke('register_mcp_server', {
      name: input.name,
      command: input.command ?? '',
      args: input.args ?? [],
      transport: input.transport,
      endpoint: input.endpoint,
      authHeader: input.authHeader,
      headers: input.headers,
      env: input.env,
    }),

  updateMcpServer: (name: string, input: UpdateMcpServerInput): Promise<string> =>
    invoke('update_mcp_server', {
      name,
      command: input.command ?? '',
      args: input.args ?? [],
      transport: input.transport,
      endpoint: input.endpoint,
      authHeader: input.authHeader,
      headers: input.headers,
      env: input.env,
    }),

  removeMcpServer: (name: string): Promise<string> =>
    invoke('remove_mcp_server', { name }),

  setMcpServerEnabled: (name: string, enabled: boolean): Promise<string> =>
    invoke('set_mcp_server_enabled', { name, enabled }),

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
  // 语音合成（经 tts 插件，前端经 bridge.call 调用插件 handle_view_message）
  // ----------------------------------------------------------------
  synthesizeSpeech: (text: string): Promise<{ file_path: string; mime_type: string }> =>
    api.bridgeCall('text-to-speech', 'plugin.synthesize', JSON.stringify({ text }))
      .then((raw) => JSON.parse(raw)),

  /** 播放音频并等待播放完成（轮询 sidecar 播放状态；stopAudio 可中断等待）。 */
  playAudioFile: async (filePath: string): Promise<void> => {
    await api.bridgeCall('text-to-speech', 'plugin.play', JSON.stringify({ file_path: filePath }));
    // 播放在 sidecar 后台执行（阻塞式播放会让 stop 请求永远排队），
    // 这里轮询播放状态直到自然结束或被 stop 终止。
    for (;;) {
      const raw = await api.bridgeCall('text-to-speech', 'plugin.play_status', '{}');
      if (!JSON.parse(raw).playing) return;
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  },

  stopAudio: (): Promise<void> =>
    api.bridgeCall('text-to-speech', 'plugin.stop', '{}')
      .then(() => undefined),

  hasTtsCapability: (): Promise<boolean> =>
    api.listPlugins().then((plugins) =>
      plugins.some((p) => p.id === 'text-to-speech' && p.enabled),
    ),

  listTtsVoices: (): Promise<{ id: string; name: string; gender?: string }[]> =>
    api.bridgeCall('text-to-speech', 'plugin.list_voices', '{}')
      .then((raw) => JSON.parse(raw).voices ?? []),

  // ----------------------------------------------------------------
  // 语音识别（经 stt 插件，前端经 bridge.call 调用插件 handle_view_message）
  // ----------------------------------------------------------------
  hasSttCapability: (): Promise<boolean> =>
    api.listPlugins().then((plugins) =>
      plugins.some((p) => p.id === 'speech-to-text' && p.enabled),
    ),

  /** 转录音频文件（经 stt 插件）。filePath 为 ~/.tiangong/media 下的音频文件路径。 */
  transcribeSpeech: (filePath: string): Promise<{ text: string; audio_path: string; duration?: number }> =>
    api.bridgeCall('speech-to-text', 'plugin.transcribe', JSON.stringify({ file_path: filePath }))
      .then((raw) => JSON.parse(raw)),

  /** 开始录音（经 stt 插件）。session_id 由调用方生成传入，后续停止/取消携带同一编号。 */
  startRecording: (sessionId: string): Promise<{ session_id: string }> =>
    api.bridgeCall('speech-to-text', 'plugin.record_start', JSON.stringify({ session_id: sessionId }))
      .then((raw) => JSON.parse(raw)),

  /** 停止录音（经 stt 插件）。返回音频文件路径。session_id 为开始录音返回的会话 ID。 */
  stopRecording: (sessionId: string): Promise<{ file_path: string; mime_type: string; duration?: number }> =>
    api.bridgeCall('speech-to-text', 'plugin.record_stop', JSON.stringify({ session_id: sessionId }))
      .then((raw) => JSON.parse(raw)),

  /** 取消录音（经 stt 插件）：终止录音进程并丢弃录音文件（带会话 ID 校验）。 */
  cancelRecording: (sessionId: string): Promise<void> =>
    api.bridgeCall('speech-to-text', 'plugin.record_cancel', JSON.stringify({ session_id: sessionId }))
      .then(() => undefined),

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


  onBridgeEvent: (callback: (event: BridgeEventPayload) => void) =>
    listen<BridgeEventPayload>('bridge_event', (event) => callback(event.payload)),
};

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
