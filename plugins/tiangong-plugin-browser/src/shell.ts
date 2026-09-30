import {
  createTiangongBridge,
  createToolProvider,
  getShadowHostRuntime,
  openExtensionApp,
  type HostBridge,
  type ToolClosed,
  type ToolInvocation,
  type ToolResolution,
} from '@tiangong/plugin-sdk';
import { tabsModel } from './tabs-model';

/**
 * 浏览器插件 TS 壳：
 * - 打开/导航经共享标签模型（tabs-model，与面板同源）；
 * - 求值与页面协作路由到宿主 webview 容器原语（bridge webview.*）；
 * - 管理界面（地址栏/工具栏，shadow DOM）见 App.vue。
 */

const TOOL_METHOD: Record<string, string> = {
  browser_open: 'webview.create',
  browser_navigate: 'webview.navigate',
  browser_eval: 'webview.eval',
  // 协作工具（策略在 TS，引擎经协作原语）：
  web_fetch: 'webview.fetch',
  web_page_text: 'webview.pageText',
  web_query_dom: 'webview.queryDom',
  web_click: 'webview.click',
  web_form_fill: 'webview.formFill',
  web_form_extract: 'webview.formExtract',
  web_locate_element: 'webview.locate',
};

type BrowserToolWindow = Window & {
  __tiangongBrowserToolClaims?: Set<string>;
};

/** 进行中的工具调用状态：收到 tool.closed（answered/cancelled/expired
 * 均同）后置 closed。执行体持有对象引用，Map 提前清理后依然可读。 */
type ActiveInvocation = {
  closed: boolean;
};

const activeInvocations = new Map<string, ActiveInvocation>();

class ToolResolutionError extends Error {
  constructor(cause: unknown) {
    super(`提交工具结果失败：${String(cause)}`);
    this.name = 'ToolResolutionError';
  }
}

function claimInvocation(invocationId: string): boolean {
  const sharedWindow = window as BrowserToolWindow;
  const claims = sharedWindow.__tiangongBrowserToolClaims
    ?? (sharedWindow.__tiangongBrowserToolClaims = new Set());
  if (claims.has(invocationId)) return false;
  claims.add(invocationId);
  return true;
}

/** 立即释放调用状态。finally 与 tool.closed 都会到达，必须幂等。 */
function releaseInvocation(invocationId: string): void {
  (window as BrowserToolWindow).__tiangongBrowserToolClaims?.delete(invocationId);
  activeInvocations.delete(invocationId);
}

/**
 * 提交工具结果；调用已闭合时静默跳过。宿主保证一次调用只能闭合一次，
 * 闭合后提交的 resolve 会被拒绝，这里提前短路，避免取消后仍执行有副
 * 作用或已失效的闭合。
 */
async function resolveActive(
  tools: { resolve: (resolution: ToolResolution) => Promise<void> },
  invocationId: string,
  state: ActiveInvocation,
  resolution: Omit<ToolResolution, 'invocation_id'>,
): Promise<void> {
  if (state.closed) return;
  try {
    await tools.resolve({ invocation_id: invocationId, ...resolution });
  } catch (error) {
    if (state.closed) return;
    throw new ToolResolutionError(error);
  }
}

type PageTextResult = {
  error?: string;
  title?: string;
  url?: string;
  total_chars?: number;
  offset?: number;
  end?: number;
  has_more?: boolean;
  text?: string;
  keyword?: string;
  match_count?: number;
  matches?: Array<{ offset: number; snippet: string }>;
};

/** web_page_text 结果：区间读取给出位置与续读提示，关键词搜索列出命中片段。 */
function formatPageText(result: PageTextResult): string {
  if (result.error) return `页面正文查询失败：${result.error}`;
  const head = `标题：${result.title ?? ''}\nURL：${result.url ?? ''}\n全文 ${result.total_chars ?? 0} 字`;
  if (result.keyword !== undefined) {
    const matches = result.matches ?? [];
    if (matches.length === 0) return `${head}\n关键词「${result.keyword}」无命中。`;
    const shown = matches
      .map((match, index) => `[${index + 1}] offset=${match.offset}\n${match.snippet}`)
      .join('\n\n');
    const more = (result.match_count ?? 0) > matches.length
      ? `\n\n（共 ${result.match_count} 处命中，仅列出前 ${matches.length} 处；可按 offset 读取上下文）`
      : '';
    return `${head}\n关键词「${result.keyword}」命中 ${result.match_count} 处：\n\n${shown}${more}`;
  }
  const range = `第 ${result.offset ?? 0}–${result.end ?? 0} 字`;
  const next = result.has_more ? `\n\n（未读完，继续读取请用 offset=${result.end}）` : '\n\n（已读到末尾）';
  return `${head}，本次返回${range}：\n\n${result.text ?? ''}${next}`;
}

/** 打开浏览器插件 App（app.open 宿主原语，聚焦本进程内的会话实例）。 */
async function requestOpenInstance(
  bridge: HostBridge,
  sessionId: string,
  instanceId?: string,
  showPanel = true,
): Promise<void> {
  try {
    await openExtensionApp(bridge, { sessionId, instanceId, showPanel });
  } catch (error) {
    console.error('打开浏览器面板失败:', error);
  }
}

async function main() {
  const bridge = await createTiangongBridge();
  await tabsModel.attach(bridge);
  const runtime = getShadowHostRuntime();
  tabsModel.scope = runtime?.context.session?.id ?? '__global__';
  await tabsModel.restore().catch(() => {});
  if (runtime) {
    const stop = runtime.onContextChange((context) => {
      const nextScope = context.session?.id ?? '__global__';
      if (nextScope === tabsModel.scope) return;
      tabsModel.scope = nextScope;
      void tabsModel.restore().catch(() => {});
    });
    runtime.registerCleanup(stop);
  }
  const tools = createToolProvider(bridge);

  // answered/cancelled/expired 都意味着调用已结束：标记后立即释放认领。
  // 宿主保证同一调用只会出现 requested → closed，闭合后不再重放 requested，
  // 因此无需定时器或永久闭合记录。
  tools.onClosed((closed: ToolClosed) => {
    const state = activeInvocations.get(closed.invocation_id);
    if (state) state.closed = true;
    releaseInvocation(closed.invocation_id);
  });

  tools.onRequested((invocation: ToolInvocation) => {
    // multi 模式下每个浏览器顶部标签都会挂载一个页面。宿主事件会送达
    // 所有实例，按 invocation_id 只允许其中一个实例执行工具。
    if (!claimInvocation(invocation.invocation_id)) return;
    const state: ActiveInvocation = { closed: false };
    activeInvocations.set(invocation.invocation_id, state);
    void (async () => {
      try {
        // browser_close（面板开关，app.* 原语）：带 tab_id 精确关闭一个
        // 页面，不带则收起整个浏览器面板（用户明确要求或任务完成时）。
        if (invocation.name === 'browser_close') {
          if (state.closed) return;
          const args = (invocation.arguments ?? {}) as { tab_id?: string };
          await bridge.call(
            'app.close',
            JSON.stringify(
              args.tab_id
                ? { session_id: invocation.session_id, instance_id: args.tab_id }
                : { session_id: invocation.session_id, all: true },
            ),
          );
          await resolveActive(tools, invocation.invocation_id, state, {
            status: 'answered',
            result: {
              ok: true,
              summary: args.tab_id ? `已关闭页面 ${args.tab_id}` : '已收起浏览器面板',
              exit_code: 0,
            },
          });
          return;
        }
        const method = TOOL_METHOD[invocation.name];
        if (!method) {
          await resolveActive(tools, invocation.invocation_id, state, {
            status: 'cancelled',
            result: { ok: false, summary: `未知工具 ${invocation.name}`, exit_code: 1 },
          });
          return;
        }
        // 发起会话与当前页面作用域一致时先刷新宿主页面快照；其他会话
        // 直接调用原语。可见标签仍统一由 App 拓展区顶部标签维护。
        if (
          (invocation.name === 'browser_open' || invocation.name === 'browser_navigate') &&
          tabsModel.scope === invocation.session_id &&
          typeof (invocation.arguments as { url?: unknown })?.url === 'string'
        ) {
          const target = (invocation.arguments as { url: string }).url;
          if (invocation.name === 'browser_open' || tabsModel.tabs.length === 0) {
            if (state.closed) return;
            const opened = await tabsModel.newTab(target);
            if (opened && !state.closed) {
              void requestOpenInstance(bridge, invocation.session_id, opened.id);
            }
          } else {
            if (state.closed) return;
            await tabsModel.navigate(target);
          }
          const summary =
            invocation.name === 'browser_open'
              ? `已在浏览器面板新标签打开：${target}`
              : `已导航到：${target}`;
          await resolveActive(tools, invocation.invocation_id, state, {
            status: 'answered',
            result: { ok: true, summary, exit_code: 0 },
          });
          return;
        }
        // 会话绑定（对齐终端插件）：Agent 打开/操作的页面归属发起对话，
        // 与该对话的浏览器面板是同一实例（插件×会话双维度隔离）。
        // web_fetch 的页面归属由宿主统一编排：宿主生成页面编号并以同一
        // 编号建立标签（app.open），有上限地等待挂载后抓取，插件无需补建。
        const invocationArgs = (invocation.arguments as Record<string, unknown>) ?? {};
        if (state.closed) return;
        const raw = await bridge.call(
          method,
          JSON.stringify({
            ...invocationArgs,
            session_id: invocation.session_id,
          }),
        );
        if (state.closed) return;
        const parsed = JSON.parse(raw) as {
          view_id?: string;
          tabs?: Array<{ id?: string; url?: string; title?: string }>;
          active_tab_id?: string | null;
          result?: string | null;
        };
        if (invocation.name === 'browser_open' && parsed.active_tab_id) {
          void requestOpenInstance(bridge, invocation.session_id, parsed.active_tab_id);
        }
        // 真实结果摘要：按工具类别格式化（策略层职责）
        let summary: string;
        if (invocation.name === 'browser_eval') {
          summary = parsed.result ?? '(无返回值)';
        } else if (invocation.name === 'web_fetch') {
          const content = (parsed as { content?: string }).content ?? '';
          summary = content.slice(0, 2_000_000) || '(空内容)';
        } else if (invocation.name === 'web_page_text') {
          summary = formatPageText(parsed as PageTextResult);
        } else if (
          invocation.name === 'web_query_dom' ||
          invocation.name === 'web_form_extract' ||
          invocation.name === 'web_locate_element'
        ) {
          summary = JSON.stringify(parsed).slice(0, 100_000);
        } else if (
          invocation.name === 'web_click' ||
          invocation.name === 'web_form_fill'
        ) {
          summary = JSON.stringify(parsed).slice(0, 10_000);
        } else {
          const tabs = parsed.tabs ?? [];
          const active = tabs.find((tab) => tab.id === parsed.active_tab_id) ?? tabs[0];
          summary = active
            ? `webview 实例 ${parsed.view_id ?? '?'}，当前页：${active.title ?? active.url ?? '未知'}`
            : `webview 实例 ${parsed.view_id ?? '?'} 已就绪`;
        }
        await resolveActive(tools, invocation.invocation_id, state, {
          status: 'answered',
          result: { ok: true, summary, exit_code: 0 },
        });
      } catch (error) {
        if (state.closed) return;
        if (error instanceof ToolResolutionError) {
          console.error(error.message);
          return;
        }
        await resolveActive(tools, invocation.invocation_id, state, {
          status: 'answered',
          result: { ok: false, summary: `webview 调用失败：${String(error)}`, exit_code: 1 },
        });
      }
    })().finally(() => releaseInvocation(invocation.invocation_id));
  });
}

void main();
