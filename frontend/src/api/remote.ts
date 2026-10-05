/**
 * 远程访问传输层（手机 H5）：页面由天工桌面端经中继提供，注入
 * `window.__TIANGONG_REMOTE__`。invoke 与宿主事件经 `/ws` 与桌面端往返，
 * 命令由桌面端按与本地完全相同的处理链路执行。
 *
 * 首次扫码：URL 片段携带一次性配对码（`#pair=...`），配对成功后桌面端签发
 * 设备令牌保存在本机，之后直接用设备令牌接入。桌面端只绑定一个设备，
 * 同一时刻只允许一条连接在线。
 */

type Pending = { resolve: (value: unknown) => void; reject: (reason: unknown) => void };
type Handler = (event: { event: string; id: number; payload: unknown }) => void;

export type RemoteStatus =
  | { kind: 'connecting' }
  | { kind: 'ready' }
  | { kind: 'denied'; reason: string }
  | { kind: 'offline'; reason: string };

const DEVICE_KEY = 'tiangong-remote-device';
const RECONNECT_MS = 3000;

let socket: WebSocket | null = null;
let ready = false;
let stopped = false;
let nextId = 1;
let mediaKey = '';
let status: RemoteStatus = { kind: 'connecting' };
let pairCode: string | null = null;
const pending = new Map<number, Pending>();
const queue: string[] = [];
const handlers = new Map<string, Set<Handler>>();
const statusListeners = new Set<(status: RemoteStatus) => void>();

export function isRemoteHost(): boolean {
  return typeof window !== 'undefined' && !!(window as { __TIANGONG_REMOTE__?: unknown }).__TIANGONG_REMOTE__;
}

function setStatus(next: RemoteStatus) {
  status = next;
  statusListeners.forEach((listener) => listener(next));
}

export function getRemoteStatus(): RemoteStatus {
  return status;
}

export function onRemoteStatus(listener: (status: RemoteStatus) => void): () => void {
  statusListeners.add(listener);
  return () => statusListeners.delete(listener);
}

/** 会话媒体文件的远程访问地址（设备在线期间有效）。 */
export function remoteFileUrl(path: string): string {
  return `remote/file?path=${encodeURIComponent(path)}&k=${encodeURIComponent(mediaKey)}`;
}

function takePairCode(): string | null {
  const match = /(?:^#|&)pair=([^&]+)/.exec(window.location.hash);
  if (!match) return null;
  // 配对码只用一次：立即从地址栏移除，避免被分享或回退复用。
  history.replaceState(null, '', window.location.pathname + window.location.search);
  return decodeURIComponent(match[1]);
}

function wsUrl(): string {
  const base = new URL('ws', window.location.href);
  base.protocol = base.protocol === 'https:' ? 'wss:' : 'ws:';
  base.hash = '';
  return base.toString();
}

function failPending(reason: string) {
  pending.forEach(({ reject }) => reject(reason));
  pending.clear();
}

function connect() {
  if (stopped) return;
  setStatus({ kind: 'connecting' });
  ready = false;
  const ws = new WebSocket(wsUrl());
  socket = ws;
  ws.onopen = () => {
    ws.send(JSON.stringify({
      t: 'hello',
      pair: pairCode ?? undefined,
      device: localStorage.getItem(DEVICE_KEY) ?? undefined,
      label: navigator.userAgent,
    }));
  };
  ws.onmessage = (message) => {
    let data: { t: string; [key: string]: unknown };
    try {
      data = JSON.parse(String(message.data));
    } catch {
      return;
    }
    switch (data.t) {
      case 'ready': {
        pairCode = null;
        if (typeof data.device === 'string' && data.device) {
          localStorage.setItem(DEVICE_KEY, data.device);
        }
        mediaKey = typeof data.media_key === 'string' ? data.media_key : '';
        ready = true;
        setStatus({ kind: 'ready' });
        while (queue.length > 0) ws.send(queue.shift()!);
        break;
      }
      case 'result': {
        const entry = pending.get(Number(data.id));
        if (!entry) return;
        pending.delete(Number(data.id));
        if (data.ok) entry.resolve(data.value);
        else entry.reject(typeof data.error === 'string' ? data.error : JSON.stringify(data.error));
        break;
      }
      case 'event': {
        const name = String(data.event);
        handlers.get(name)?.forEach((handler) => handler({ event: name, id: 0, payload: data.payload }));
        break;
      }
      case 'denied':
      case 'kicked': {
        stopped = true;
        if (data.t === 'denied') localStorage.removeItem(DEVICE_KEY);
        const reason = typeof data.reason === 'string' ? data.reason : '连接被拒绝';
        setStatus({ kind: 'denied', reason });
        failPending(reason);
        break;
      }
    }
  };
  ws.onclose = (event) => {
    if (socket !== ws) return;
    socket = null;
    ready = false;
    if (stopped) return;
    const reason = event.code === 4001 ? '天工桌面端未在线，正在重试…' : '连接已断开，正在重连…';
    setStatus({ kind: 'offline', reason });
    failPending(reason);
    setTimeout(connect, RECONNECT_MS);
  };
}

let started = false;
function ensureStarted() {
  if (started) return;
  started = true;
  pairCode = takePairCode();
  if (!pairCode && !localStorage.getItem(DEVICE_KEY)) {
    stopped = true;
    setStatus({ kind: 'denied', reason: '请在天工桌面端「设置 → 远程访问」生成二维码后扫码打开' });
    return;
  }
  connect();
}

export function remoteInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  ensureStarted();
  if (stopped) {
    return Promise.reject(status.kind === 'denied' ? status.reason : '远程连接不可用');
  }
  const id = nextId++;
  const text = JSON.stringify({ t: 'invoke', id, cmd: command, args: args ?? {} });
  return new Promise<T>((resolve, reject) => {
    pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
    if (ready && socket) socket.send(text);
    else queue.push(text);
  });
}

export function remoteListen(event: string, handler: Handler): Promise<() => void> {
  ensureStarted();
  let set = handlers.get(event);
  if (!set) {
    set = new Set();
    handlers.set(event, set);
  }
  set.add(handler);
  return Promise.resolve(() => {
    handlers.get(event)?.delete(handler);
  });
}

/** 测试辅助：触发一个远程事件。 */
export function __emitRemoteEventForTest(event: string, payload: unknown) {
  handlers.get(event)?.forEach((handler) => handler({ event, id: 0, payload }));
}
