/**
 * 宿主通信适配层：同一套前端组件既运行在 Tauri 桌面窗口内，也运行在
 * `tiangong config` 启动的浏览器配置页中。
 *
 * - Tauri：直接使用 `@tauri-apps/api` 的 invoke / listen；
 * - 浏览器配置页：页面由配置服务注入 `window.__TIANGONG_WEB__ = { remote }`，
 *   invoke 改走 `POST api/invoke/<命令名>`（参数为原 invoke 的参数对象，
 *   成功响应为 `{ result }`，失败为 `{ error }`），listen 为空操作
 *   （配置页不推送宿主事件，组件按需轮询）。
 */
import { invoke as tauriInvoke } from '@tauri-apps/api/core';
import { listen as tauriListen } from '@tauri-apps/api/event';
import type { EventCallback, UnlistenFn } from '@tauri-apps/api/event';

interface WebHost {
  /** 配置服务是否监听在非回环地址（在其他机器的浏览器中配置）。 */
  remote?: boolean;
}

declare global {
  interface Window {
    __TIANGONG_WEB__?: WebHost;
  }
}

/** 当前是否运行在浏览器配置页（非 Tauri 窗口）。 */
export function isWebHost(): boolean {
  return typeof window !== 'undefined' && !!window.__TIANGONG_WEB__;
}

/** 浏览器配置页是否为远程配置（浏览器与天工不在同一台机器）。 */
export function isRemoteWebHost(): boolean {
  return isWebHost() && !!window.__TIANGONG_WEB__?.remote;
}

async function webPost(path: string, body: unknown): Promise<unknown> {
  const response = await fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body ?? {}),
  });
  const text = await response.text();
  let data: unknown = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = text;
  }
  if (!response.ok) {
    // 与 Tauri invoke 一致：命令错误以字符串形式抛出。
    throw data && typeof data === 'object' && 'error' in data
      ? String((data as { error: unknown }).error)
      : `请求失败：${response.status}`;
  }
  return data;
}

async function webInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const data = await webPost(`api/invoke/${encodeURIComponent(command)}`, args);
  return (data as { result: T }).result;
}

/** 关闭浏览器配置页服务（"完成并关闭"）。 */
export async function closeWebHost(): Promise<void> {
  await webPost('api/close', {});
}

export function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (isWebHost()) return webInvoke<T>(command, args);
  return tauriInvoke<T>(command, args);
}

export function listen<T>(event: string, handler: EventCallback<T>): Promise<UnlistenFn> {
  if (isWebHost()) return Promise.resolve(() => {});
  return tauriListen<T>(event, handler);
}
