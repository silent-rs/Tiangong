/**
 * 天工插件 SDK：UI 沙箱内访问宿主能力的统一客户端。
 *
 * 两种容器自动适配：
 * - Shadow 容器：宿主以 `bridge` 参数注入（脚本打包后经受限执行）。
 * - iframe 容器：经 `postMessage` 协议与宿主通信（`tiangong_host_context` 提供频道）。
 *
 * 桥接方法命名空间（白名单控制）：
 * - `plugin.*`   转发到本插件 WASM 逻辑层（带逻辑层的插件）
 * - `storage.*`  插件私有数据读写：get/set/delete/list
 * - 其余命名空间（session/tool）按宿主版本渐进开放。
 *
 * 用法（打包工具如 esbuild 把本 SDK 与插件代码一起构建）：
 * ```ts
 * import { createTiangongBridge } from '@tiangong/plugin-sdk';
 * const bridge = await createTiangongBridge();
 * await bridge.call('storage.set', JSON.stringify({ key: 'title', value: '看板' }));
 * ```
 */

// ── 类型定义（与宿主 tauri.ts 契约对齐）──

/** 宿主桥接：call 调用宿主能力，on 订阅宿主事件（返回取消函数）。 */
export interface HostBridge {
  call(method: string, payload: string): Promise<string>;
  on(channel: string, handler: (payload: string) => void): () => void;
}

/** 打开本插件 `extension.tab` App 的参数。 */
export interface OpenExtensionAppOptions {
  /** App 所属会话；缺省时由宿主使用当前会话。 */
  sessionId?: string;
  /** 插件生成的实例编号，用于幂等打开或聚焦指定实例。 */
  instanceId?: string;
  /** 是否自动展开并聚焦拓展区，缺省为 true。 */
  showPanel?: boolean;
}

/** App 打开模式：singleton 全局至多一个实例，multi 每次打开新建。 */
export type OpenMode = 'singleton' | 'multi';

/** UI 贡献沙箱级别。 */
export type SandboxKind = 'shadow' | 'iframe' | 'native' | 'webview';

/** manifest v2 `ui.contributions[]` 声明。 */
export interface UiContribution {
  slot: string;
  id: string;
  title: string;
  description?: string;
  icon?: string;
  entry: string;
  open_mode?: OpenMode;
  context?: string[];
  sandbox?: SandboxKind;
}

/** manifest v2 `capabilities` 声明。 */
export interface PluginCapabilities {
  tools: boolean;
  prompt: boolean;
  lifecycle: boolean;
  approval: boolean;
  interaction: boolean;
  events: string[];
}

/** 宿主主题与会话上下文（iframe 经 postMessage，Shadow 经运行时参数收到）。 */
export interface HostContext {
  type: 'tiangong_host_context';
  channel: string;
  theme: 'light' | 'dark';
  tokens: Record<string, string>;
  fontFamily?: string;
  session?: {
    id?: string;
    /** 当前会话工作目录（无活跃会话时为全局工作区）。 */
    workspace?: string;
  };
  /** extension.tab 顶部标签实例及其当前显隐状态。 */
  app?: {
    /**
     * 实例编号（宿主生成 scru128，即标签编号）。声明 `instance_resources`
     * 的插件按此编号归属后端资源，并在 sidecar 实现 `instanceClosed` /
     * `listInstances`：标签移除后宿主保证发出 `instanceClosed`。
     */
    instance_id: string;
    visible: boolean;
  };
  /** 消息级 Slot（session.message-item / session.message-action）当前绑定的消息。 */
  message?: HostMessageContext;
}

/** 消息级 Slot 的消息上下文。 */
export interface HostMessageContext {
  id: string;
  role: string;
  /** 面向用户的可见文本（不含只给模型的指令块）。 */
  text: string;
  /**
   * 消息引用的媒体附件：`path` 为本地路径，`url` 为宿主解析好的可显示地址
   * （可直接用于 img / audio / video 的 src），供接管显示的插件自行渲染。
   */
  attachments: Array<{ kind: string; path: string; url?: string; mime_type?: string; name?: string }>;
  /**
   * 渲染声明：仅当消息 `meta.render` 指向本插件的 `render: "replace"` 贡献时
   * 出现，`data` 为声明方写入的自定义数据（序列化后不超过 16KB）。
   */
  render?: { view: string; data?: unknown };
}

/**
 * 消息渲染声明：随外部消息（`POST /api/v1/messages` 的 `render` 字段）写入
 * 消息 `meta.render`，宿主据此把消息交给 `plugin` 的 `session.message-item`
 * 贡献 `view`（须声明 `render: "replace"`）渲染。只影响显示，不进模型请求。
 */
export interface MessageRender {
  plugin: string;
  view: string;
  data?: unknown;
}

// ── 运行时 ──

/** Shadow 容器注入的桥接与运行参数（宿主执行作用域内可见）。 */
declare const bridge: HostBridge | undefined;
declare const pluginRoot: ShadowRoot | undefined;
declare const hostContext: HostContext | undefined;
declare const onHostContextChange: ((handler: (context: HostContext) => void) => () => void) | undefined;
declare const registerCleanup: ((cleanup: () => void) => void) | undefined;
declare const registerBeforeClose: ((handler: () => void | Promise<void>) => void) | undefined;

/** Shadow 脚本由宿主动态注入的 DOM、上下文与生命周期能力；样式变量直接继承 App 根节点。 */
export interface ShadowHostRuntime {
  root: ShadowRoot;
  context: HostContext;
  /** 会话等非样式上下文变化；主题颜色直接继承 App 根节点。 */
  onContextChange(handler: (context: HostContext) => void): () => void;
  registerCleanup(cleanup: () => void): void;
  /**
   * 顶部标签明确关闭前执行；异步处理完成后宿主才移除实例。只做状态保存：
   * 后端资源释放以宿主在标签移除后发出的 `instanceClosed` 为准。
   */
  registerBeforeClose(handler: () => void | Promise<void>): void;
}

/** 容器类型。 */
export type TiangongContainer = 'shadow' | 'iframe' | 'unknown';

/** 探测当前运行的容器类型。 */
export function detectContainer(): TiangongContainer {
  // Shadow 容器：宿主以 bridge 参数注入脚本执行环境
  try {
    if (typeof bridge !== 'undefined' && bridge) return 'shadow';
  } catch {
    // typeof 检查不会抛出，防御性兜底
  }
  // iframe 容器：srcdoc 沙箱，window.parent 可达（沙箱 allow-scripts）
  if (typeof window !== 'undefined' && window.parent !== window) return 'iframe';
  return 'unknown';
}

/**
 * 获取 Shadow 容器运行时。iframe 或普通网页中返回 null，便于同一入口兼容两种容器。
 */
export function getShadowHostRuntime(): ShadowHostRuntime | null {
  try {
    if (
      typeof pluginRoot === 'undefined'
      || !pluginRoot
      || typeof hostContext === 'undefined'
      || !hostContext
      || typeof onHostContextChange !== 'function'
      || typeof registerCleanup !== 'function'
      || typeof registerBeforeClose !== 'function'
    ) {
      return null;
    }
    return {
      root: pluginRoot,
      context: hostContext,
      onContextChange: onHostContextChange,
      registerCleanup,
      registerBeforeClose,
    };
  } catch {
    return null;
  }
}

/** iframe 容器的 postMessage 桥接实现。 */
class IframeBridge implements HostBridge {
  private channel: string | null = null;
  private channelWaiters: Array<() => void> = [];
  private nextCallId = 0;
  private readonly pending = new Map<string, { resolve: (v: string) => void; reject: (e: Error) => void }>();

  constructor() {
    window.addEventListener('message', (event) => {
      const data = event.data as Record<string, unknown> | null;
      if (!data) return;
      if (data.type === 'tiangong_host_context' && typeof data.channel === 'string') {
        this.channel = data.channel;
        this.channelWaiters.forEach((notify) => notify());
        this.channelWaiters = [];
        return;
      }
      if (typeof data.id === 'string' && (data.channel === this.channel)) {
        const waiter = this.pending.get(data.id);
        if (!waiter) return;
        this.pending.delete(data.id);
        if (typeof data.error === 'string') waiter.reject(new Error(data.error));
        else if (typeof data.result === 'string') waiter.resolve(data.result);
      }
    });
  }

  private async ensureChannel(): Promise<string> {
    if (this.channel) return this.channel;
    return new Promise((resolve) => {
      this.channelWaiters.push(() => resolve(this.channel!));
    });
  }

  async call(method: string, payload: string): Promise<string> {
    const channel = await this.ensureChannel();
    const id = `sdk-${++this.nextCallId}`;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      window.parent.postMessage({ type: 'plugin_call', channel, id, method, payload }, '*');
      setTimeout(() => {
        if (this.pending.delete(id)) {
          reject(new Error(`bridge.call(${method}) 超时`));
        }
      }, 30_000);
    });
  }

  on(eventChannel: string, handler: (payload: string) => void): () => void {
    // 经 postMessage 发起订阅（宿主按 capabilities.events 校验放行），
    // 事件经 { type: 'bridge_event', channel, payload } 回推。
    let stopEvents: (() => void) | null = null;
    void this.ensureChannel().then((channel) => {
      window.parent.postMessage(
        { type: 'plugin_subscribe', channel, event: eventChannel },
        '*',
      );
    });
    const listener = (event: MessageEvent) => {
      const data = event.data as Record<string, unknown> | null;
      if (data?.type === 'bridge_event' && data.channel === eventChannel
        && typeof data.payload === 'string') {
        handler(data.payload);
      }
    };
    window.addEventListener('message', listener);
    stopEvents = () => {
      window.removeEventListener('message', listener);
      void this.ensureChannel().then((channel) => {
        window.parent.postMessage(
          { type: 'plugin_unsubscribe', channel, event: eventChannel },
          '*',
        );
      });
    };
    return () => stopEvents?.();
  }
}

/**
 * 创建宿主桥接客户端（自动适配容器）。
 * iframe 容器下首次调用会等待宿主推送频道（挂载即送达，通常立即就绪）。
 */
export async function createTiangongBridge(): Promise<HostBridge> {
  const container = detectContainer();
  if (container === 'shadow') {
    return bridge as HostBridge;
  }
  if (container === 'iframe') {
    return new IframeBridge();
  }
  throw new Error('未检测到天工容器（不在插件沙箱内运行）');
}

/**
 * 打开本插件声明的 `extension.tab` App。
 *
 * 仅声明了 `extension.tab` 贡献及 `app.use` 权限的插件可用，宿主会再次校验。
 * 实例统一建立拓展区顶部标签（可见、可关闭、计入在用绿点）；
 * `showPanel=false` 仅表示不自动展开拓展区面板（工具静默拉起）。
 */
export async function openExtensionApp(
  bridge: HostBridge,
  options: OpenExtensionAppOptions = {},
): Promise<void> {
  const payload: Record<string, unknown> = {};
  if (options.sessionId !== undefined) payload.session_id = options.sessionId;
  if (options.instanceId !== undefined) payload.instance_id = options.instanceId;
  if (options.showPanel === false) payload.mode = 'background';
  await bridge.call('app.open', JSON.stringify(payload));
}

/** Desktop 纯 TypeScript 工具插件收到的权威调用。 */
export interface ToolInvocation {
  invocation_id: string;
  session_id: string;
  tool_call_id: string;
  name: string;
  arguments: unknown;
  created_at: string;
}

/** 工具调用的宿主权威闭合事件。 */
export interface ToolClosed {
  invocation_id: string;
  status: 'answered' | 'expired' | 'cancelled';
}

/** 插件提交给宿主的通用工具结果。 */
export interface ToolResult {
  ok: boolean;
  summary: string;
  stdout?: string;
  stderr?: string;
  exit_code: number;
}

/** `tool.resolve` 的完整负载。 */
export interface ToolResolution {
  invocation_id: string;
  status?: ToolClosed['status'];
  result: ToolResult;
}

/** Desktop TypeScript 工具提供器客户端。 */
export function createToolProvider(bridge: HostBridge) {
  return {
    onRequested(handler: (invocation: ToolInvocation) => void): () => void {
      return bridge.on('tool.requested', (payload) => {
        handler(JSON.parse(payload) as ToolInvocation);
      });
    },
    onClosed(handler: (closed: ToolClosed) => void): () => void {
      return bridge.on('tool.closed', (payload) => {
        handler(JSON.parse(payload) as ToolClosed);
      });
    },
    async resolve(resolution: ToolResolution): Promise<void> {
      await bridge.call('tool.resolve', JSON.stringify(resolution));
    },
  };
}

/** 便捷的存储读写（storage.* 封装，值为字符串）。 */
/** 保存文件请求（saveFileDialog）。 */
export interface SaveFileRequest {
  suggestedName: string;
  /** 文本原样写入；二进制先 base64 并置 encoding。 */
  contents: string;
  encoding?: 'base64';
}

/** 保存文件结果：cancelled 为 true 表示用户取消了对话框。 */
export interface SaveFileResult {
  cancelled: boolean;
  path?: string;
}

/**
 * 经宿主原生保存对话框把内容写入用户选择的位置（权限 dialog.use）。
 * 宿主 webview 是 WebKit（macOS），没有浏览器的 File System Access API
 * （showSaveFilePicker 等）——插件导出文件必须走本通道。
 */
export async function saveFileDialog(
  bridge: HostBridge,
  request: SaveFileRequest,
): Promise<SaveFileResult> {
  const raw = await bridge.call(
    'dialog.saveFile',
    JSON.stringify({
      suggested_name: request.suggestedName,
      contents: request.contents,
      ...(request.encoding ? { encoding: request.encoding } : {}),
    }),
  );
  return JSON.parse(raw) as SaveFileResult;
}

export const pluginStorage = {
  async get(bridge: HostBridge, key: string): Promise<string | null> {
    const result = await bridge.call('storage.get', JSON.stringify({ key }));
    return result === 'null' ? null : JSON.parse(result) as string;
  },
  async set(bridge: HostBridge, key: string, value: string): Promise<void> {
    await bridge.call('storage.set', JSON.stringify({ key, value }));
  },
  async remove(bridge: HostBridge, key: string): Promise<void> {
    await bridge.call('storage.delete', JSON.stringify({ key }));
  },
  async keys(bridge: HostBridge): Promise<string[]> {
    const result = await bridge.call('storage.list', '{}');
    return JSON.parse(result) as string[];
  },
};
