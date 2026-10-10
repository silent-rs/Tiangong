/**
 * 插件 UI 容器共享的宿主上下文：主题 token 采集。
 *
 * iframe 容器经 postMessage 推送 `tiangong_host_context`；
 * Shadow 容器直接继承 App 根节点的同名 CSS 变量。
 * 两种容器 token 同源，插件 UI 用同一套变量名消费。
 */

export type HostTheme = 'light' | 'dark';

/** 宿主暴露给插件的设计 token 名（同时是 CSS 变量名）。 */
export const HOST_TOKEN_NAMES = [
  'background',
  'foreground',
  'card',
  'card-foreground',
  'muted',
  'muted-foreground',
  'accent',
  'accent-foreground',
  'primary',
  'primary-foreground',
  'destructive',
  'border',
  'input',
  'ring',
  'status-success',
  'status-warning',
  'status-error',
  'status-info',
  'radius',
] as const;

/** 从主文档读取当前主题 token 值。 */
export function collectHostTokens(): Record<string, string> {
  const styles = getComputedStyle(document.documentElement);
  return Object.fromEntries(
    HOST_TOKEN_NAMES.map((name) => [name, styles.getPropertyValue(`--${name}`).trim()]),
  );
}

/** 消息级 Slot（session.message-action / session.message-item）的消息上下文。 */
export interface HostMessageContext {
  id: string;
  role: string;
  /** 面向用户的可见文本（助手消息为最终回复正文）。 */
  text: string;
  /** 消息引用的媒体附件（本地路径）。 */
  attachments: Array<{ kind: string; path: string; mime_type?: string; name?: string }>;
  /** 插件接管显示时的渲染声明（视图 ID 与插件自定义数据）。 */
  render?: { view: string; data?: unknown };
}

/**
 * iframe 容器的 hostContext 消息体（沿用既有协议，保持 v1 插件兼容）。
 * workspace 为当前会话工作目录（无活跃会话时为全局工作区），供终端等
 * 插件作为默认初始目录。
 */
export function hostContext(
  theme: HostTheme,
  channel: string,
  sessionId?: string | null,
  workspace?: string | null,
  app?: { instance_id: string; visible: boolean },
  message?: HostMessageContext,
) {
  const session: { id?: string; workspace?: string } = {};
  if (sessionId) session.id = sessionId;
  if (workspace) session.workspace = workspace;
  return {
    type: 'tiangong_host_context',
    channel,
    theme,
    tokens: collectHostTokens(),
    fontFamily: getComputedStyle(document.body).fontFamily,
    ...(sessionId || workspace ? { session } : {}),
    ...(app ? { app } : {}),
    ...(message ? { message } : {}),
  };
}

/** iframe 与 Shadow 容器共享的宿主上下文负载。 */
export type PluginHostContext = ReturnType<typeof hostContext>;
