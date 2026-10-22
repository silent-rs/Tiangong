import type { HostBridge } from '@tiangong/plugin-sdk';

/**
 * 终端插件 TS 壳（视图辅助层）。
 *
 * 工具编排（command/run_shell/terminal 的 action 模式）已下沉 sidecar，
 * 并由握手 capabilities 注册 Handler：
 * 宿主直连 sidecar 执行，不经 tool.requested 页面竞争接应。本文件只保留
 * 视图共用的 sidecar 直调封装与 PTY 会话注册表。
 */

/** 插件内共享的 PTY 会话注册表（视图恢复与附着用）。 */
export interface TerminalSessionInfo {
  session_id: string;
  /** 关联的会话（作用域）ID。 */
  scope_id: string;
  created_at: number;
}

// multi 模式下每个终端顶部标签都挂载一个实例，注册表挂主文档 window
// 供各实例共同使用。
type TerminalToolWindow = Window & {
  __tiangongTerminalSessions?: Map<string, TerminalSessionInfo>;
  __tiangongTerminalFrontendTabs?: Map<string, Set<string>>;
  __tiangongTerminalGcTimers?: Map<string, number>;
};

const TERMINAL_GC_SYNC_DELAY_MS = 500;

function sessionRegistry(): Map<string, TerminalSessionInfo> {
  const shared = window as TerminalToolWindow;
  return shared.__tiangongTerminalSessions
    ?? (shared.__tiangongTerminalSessions = new Map());
}

function frontendTabRegistry(): Map<string, Set<string>> {
  const shared = window as TerminalToolWindow;
  return shared.__tiangongTerminalFrontendTabs
    ?? (shared.__tiangongTerminalFrontendTabs = new Map());
}

function gcTimerRegistry(): Map<string, number> {
  const shared = window as TerminalToolWindow;
  return shared.__tiangongTerminalGcTimers
    ?? (shared.__tiangongTerminalGcTimers = new Map());
}

function liveFrontendTabs(scopeId: string): string[] {
  return [...(frontendTabRegistry().get(scopeId) ?? [])].sort();
}

function submitTerminalGc(bridge: HostBridge, scopeId: string): Promise<Record<string, unknown>> {
  return sidecarCall(bridge, 'terminalGc', {
    session_id: scopeId,
    live_terminal_ids: liveFrontendTabs(scopeId),
  });
}

/** 新标签登记后合并同一轮挂载，再提交该会话的完整存活集合。 */
export function registerFrontendTerminalTab(
  bridge: HostBridge,
  scopeId: string,
  sessionId: string,
): void {
  if (!scopeId || !sessionId) return;
  const tabs = frontendTabRegistry().get(scopeId) ?? new Set<string>();
  const added = !tabs.has(sessionId);
  tabs.add(sessionId);
  frontendTabRegistry().set(scopeId, tabs);
  if (!added) return;

  const timers = gcTimerRegistry();
  const current = timers.get(scopeId);
  if (current !== undefined) window.clearTimeout(current);
  timers.set(scopeId, window.setTimeout(() => {
    timers.delete(scopeId);
    void submitTerminalGc(bridge, scopeId)
      .catch((error) => console.warn('[terminal] 新建标签 GC 对账失败:', error));
  }, TERMINAL_GC_SYNC_DELAY_MS));
}

/** 用户明确关闭标签：从存活集合移除并立即提交；失败不阻止前端关闭。 */
export function closeFrontendTerminalTab(
  bridge: HostBridge,
  scopeId: string,
  sessionId: string,
): void {
  if (!scopeId || !sessionId) return;
  const tabs = frontendTabRegistry().get(scopeId);
  tabs?.delete(sessionId);
  if (tabs?.size === 0) frontendTabRegistry().delete(scopeId);

  const timers = gcTimerRegistry();
  const current = timers.get(scopeId);
  if (current !== undefined) {
    window.clearTimeout(current);
    timers.delete(scopeId);
  }
  void submitTerminalGc(bridge, scopeId)
    .catch((error) => console.warn('[terminal] 关闭标签 GC 对账失败:', error));
}

/** 供 TerminalView 复用的 sidecar 直调封装。 */
export async function sidecarCall(
  bridge: HostBridge,
  operation: string,
  payload: unknown,
): Promise<Record<string, unknown>> {
  const raw = await bridge.call(`sidecar.${operation}`, JSON.stringify(payload ?? {}));
  return JSON.parse(raw) as Record<string, unknown>;
}

/** PTY 会话注册表访问（视图间共享）。 */
export const terminalSessions = sessionRegistry();
