// 天工插件 sidecar 协议库（Node/stdio 通道，RFC 0017 D16）。
//
// 与宿主 tiangong-plugin-runtime 的 stdio 传输对齐：JSON Lines 帧协议、
// Auth 首帧认证、Request/Response/Progress/Notification 六种帧、握手
// （runtime.handshake）、stdin EOF 即退出。零依赖，Node >= 20。
//
// 用法（插件 sidecar 入口）：
//   import { runSidecar } from './vendor/tiangong-sidecar-sdk/index.mjs';
//   await runSidecar({
//     pluginId: 'my-plugin',
//     pluginVersion: '0.1.0',
//     dispatch(operation, payload, ctx) {
//       if (operation === 'my-plugin.greet') {
//         ctx.progress('处理中');
//         return { payload: { message: `hello ${payload?.name ?? 'world'}` } };
//       }
//       throw new SidecarError('unknown operation', 'bad_request');
//     },
//   });

export const PROTOCOL_VERSION = '0.1.0';
export const HANDSHAKE_OPERATION = 'runtime.handshake';

const TRANSPORT_ENV = 'TIANGONG_PLUGIN_TRANSPORT';
const STDIO_TOKEN_ENV = 'TIANGONG_PLUGIN_STDIO_TOKEN';
const PLUGIN_ID_ENV = 'TIANGONG_PLUGIN_ID';
const PLUGIN_VERSION_ENV = 'TIANGONG_PLUGIN_VERSION';

/** 业务错误：携带宿主协议的错误码（serde snake_case 枚举值）。 */
export class SidecarError extends Error {
  /**
   * @param {string} message
   * @param {'unavailable'|'timeout'|'payload_too_large'|'protocol_mismatch'|'permission_denied'|'bad_request'|'service_disabled'|'service_error'|'cancelled'} code
   * @param {boolean} retryable
   */
  constructor(message, code = 'service_error', retryable = false) {
    super(message);
    this.code = code;
    this.retryable = retryable;
  }
}

/**
 * 以 stdio sidecar 身份运行：认证 → 循环分发请求，stdin EOF（宿主关闭）即退出。
 * 宿主 spawn 时注入 TIANGONG_PLUGIN_TRANSPORT=stdio；本函数只服务该通道。
 *
 * @param {{
 *   pluginId?: string,
 *   pluginVersion?: string,
 *   businessProtocol?: number,
 *   capabilities?: string[],
 *   dispatch: (operation: string, payload: any, ctx: {progress: (message: string) => void, notify: (channel: string, payload: string) => void}) => Promise<{payload?: any}|any> | {payload?: any}|any,
 * }} options
 */
export async function runSidecar(options) {
  if (process.env[TRANSPORT_ENV] !== 'stdio') {
    console.error('tiangong sidecar SDK：宿主未声明 stdio 通道（缺少 TIANGONG_PLUGIN_TRANSPORT=stdio），退出');
    process.exit(1);
  }
  const pluginId = options.pluginId ?? process.env[PLUGIN_ID_ENV] ?? 'sidecar';
  // pluginVersion 缺省读宿主注入的环境变量，避免与清单版本漂移导致握手被拒。
  const pluginVersion = options.pluginVersion ?? process.env[PLUGIN_VERSION_ENV] ?? '0.0.0';
  const expectedToken = process.env[STDIO_TOKEN_ENV] ?? '';
  const dispatch = options.dispatch;
  const instanceId = `${pluginId}-sidecar-${process.pid}`;

  const writeFrame = (frame) => {
    process.stdout.write(`${JSON.stringify(frame)}\n`);
  };
  const respond = (requestId, envelope) => {
    writeFrame({ kind: 'response', request_id: requestId, payload: envelope });
  };

  let authenticated = false;
  const maxConcurrency = Math.max(1, Number.parseInt(process.env.TIANGONG_SIDECAR_MAX_CONCURRENCY ?? '16', 10) || 16);
  let running = 0;
  const queue = [];
  const active = new Map();
  const rl = (await import('node:readline')).createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });

  rl.on('line', (line) => {
    const trimmed = line.trim();
    if (!trimmed) {
      return;
    }
    let frame;
    try {
      frame = JSON.parse(trimmed);
    } catch {
      process.stderr.write(`sidecar 收到无法解析的帧\n`);
      return;
    }
    if (frame?.kind === 'auth') {
      if (frame.token !== expectedToken) {
        writeFrame({ kind: 'error', message: 'stdio 认证失败：token 不匹配' });
        process.exit(1);
      }
      authenticated = true;
      return;
    }
    if (!authenticated) {
      writeFrame({ kind: 'error', message: 'stdio 首帧必须是 Auth' });
      process.exit(1);
    }
    if (frame?.kind === 'cancel') {
      const task = active.get(frame.request_id);
      if (task) {
        task.controller.abort(new SidecarError('请求已取消', 'cancelled'));
        active.delete(frame.request_id);
        Promise.resolve(options.cancel?.(task.operation, task.payload, task.ctx)).catch((error) => {
          process.stderr.write(`sidecar 取消清理异常: ${error?.stack ?? error}\n`);
        });
      }
      respond(frame.request_id, failureEnvelope(frame.request_id, 'cancelled', '请求已取消'));
      return;
    }
    if (frame?.kind !== 'request') {
      process.stderr.write(`sidecar 收到非预期帧类型: ${frame?.kind}\n`);
      return;
    }
    queue.push(frame);
    pump();
  });

  rl.on('close', () => {
    // 宿主已关闭管道（退出或停止）：随宿主退出。
    process.exit(0);
  });

  function pump() {
    while (running < maxConcurrency && queue.length > 0) {
      const frame = queue.shift();
      running += 1;
      handleRequest(frame).catch((error) => {
        process.stderr.write(`sidecar 请求处理异常: ${error?.stack ?? error}\n`);
      }).finally(() => {
        running -= 1;
        pump();
      });
    }
  }

  /**
   * @param {{kind: 'request', request_id: string, payload: any}} frame
   */
  async function handleRequest(frame) {
    const envelope = frame.payload ?? {};
    const requestId = envelope.request_id ?? frame.request_id;
    if (typeof requestId !== 'string' || typeof envelope.operation !== 'string') {
      respond(frame.request_id, failureEnvelope(frame.request_id, 'bad_request', '请求信封缺少 request_id 或 operation'));
      return;
    }
    if (envelope.protocol_version !== undefined && envelope.protocol_version !== PROTOCOL_VERSION) {
      respond(requestId, failureEnvelope(requestId, 'protocol_mismatch', `协议版本不匹配: expected=${PROTOCOL_VERSION}, actual=${envelope.protocol_version}`));
      return;
    }
    try {
      if (envelope.operation === HANDSHAKE_OPERATION) {
        respond(requestId, successEnvelope(requestId, {
          plugin_id: pluginId,
          plugin_version: pluginVersion,
          sidecar_version: pluginVersion,
          protocol_version: PROTOCOL_VERSION,
          business_protocol: options.businessProtocol ?? 0,
          capabilities: options.capabilities ?? [],
          instance_id: instanceId,
          status: 'ready',
        }));
        return;
      }
      const controller = new AbortController();
      const ctx = {
        signal: controller.signal,
        progress(message) {
          if (typeof message !== 'string' || message.length === 0) {
            return;
          }
          writeFrame({ kind: 'progress', request_id: requestId, message });
        },
        notify(channel, payload) {
          if (typeof channel !== 'string' || channel.length === 0) {
            return;
          }
          writeFrame({ kind: 'notification', channel, payload: typeof payload === 'string' ? payload : JSON.stringify(payload ?? null) });
        },
      };
      active.set(requestId, { controller, operation: envelope.operation, payload: envelope.payload ?? null, ctx });
      const result = await dispatch(envelope.operation, envelope.payload ?? null, ctx);
      if (controller.signal.aborted) return;
      active.delete(requestId);
      const payload = result && typeof result === 'object' && 'payload' in result ? result.payload : (result ?? null);
      respond(requestId, successEnvelope(requestId, payload));
    } catch (error) {
      active.delete(requestId);
      if (error instanceof SidecarError) {
        respond(requestId, failureEnvelope(requestId, error.code, error.message, error.retryable));
        return;
      }
      respond(requestId, failureEnvelope(requestId, 'service_error', String(error?.message ?? error)));
    }
  }
}

function successEnvelope(requestId, payload) {
  return {
    protocol_version: PROTOCOL_VERSION,
    request_id: requestId,
    success: true,
    payload: payload ?? null,
    retryable: false,
  };
}

function failureEnvelope(requestId, code, message, retryable = false) {
  return {
    protocol_version: PROTOCOL_VERSION,
    request_id: requestId,
    success: false,
    error_code: code,
    error_message: message,
    retryable,
  };
}
