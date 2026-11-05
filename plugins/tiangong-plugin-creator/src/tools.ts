// 工具处理核心：Agent 工具调用与创作页共用同一后端。
// 开发工具链（init/validate/build/add/run/logs）经本插件的按需 node sidecar
// 执行（bridge sidecar.devkit.*，每次调用独立进程）；安装、收件箱导入与
// 只读查询走宿主受限通道（plugin-dev.install/import/list）。
import type { HostBridge } from '@tiangong/plugin-sdk';

/** 工具结果（宿主 ts_tools 契约：ok/summary/exit_code 必填）。 */
export interface ToolOutcome {
  ok: boolean;
  summary: string;
  stdout: string;
  stderr: string;
  exit_code: number;
}

export interface ProjectEntry {
  id: string;
  name: string;
  template: string;
  source_version: string | null;
  release_version: string | null;
  installed_version: string | null;
  created_at: string | null;
}

export interface InstallResult {
  plugin_id: string;
  version: string;
  state: string;
  enabled: boolean;
}

/** devkit 命令结果（sidecar 响应负载）。 */
export interface DevkitResult {
  ok: boolean;
  error?: string;
  log_tail?: string;
  [key: string]: unknown;
}

async function pluginDevCall<T>(
  bridge: HostBridge,
  operation: string,
  payload: Record<string, unknown>,
): Promise<T> {
  const raw = await bridge.call(`plugin-dev.${operation}`, JSON.stringify(payload));
  return JSON.parse(raw) as T;
}

async function sidecarCall<T>(
  bridge: HostBridge,
  command: string,
  args: string[],
): Promise<T> {
  const raw = await bridge.call(
    `sidecar.devkit.${command}`,
    JSON.stringify({ args }),
  );
  return JSON.parse(raw) as T;
}

/** 宿主侧受限通道（安装、导入与只读查询）。 */
export const pluginDev = {
  /** 把指令文本直接交给当前会话的 Agent 处理（session.input.sendText）。 */
  sendToAgent: (bridge: HostBridge, text: string) =>
    bridge.call('session.input.sendText', JSON.stringify({ text })).then(() => true),
  install: (bridge: HostBridge, id: string) =>
    pluginDevCall<InstallResult>(bridge, 'install', { id }),
  import: (bridge: HostBridge, id: string) =>
    pluginDevCall<InstallResult>(bridge, 'import', { id }),
  list: (bridge: HostBridge) => pluginDevCall<ProjectEntry[]>(bridge, 'list', {}),
};

/** 按需 node sidecar 通道（devkit 工具链；args 不含命令名）。 */
export const devkit = {
  init: (bridge: HostBridge, template: string, id: string, name?: string) => {
    const args = name ? [template, id, '--name', name] : [template, id];
    return sidecarCall<DevkitResult>(bridge, 'init', args);
  },
};

/** 对 devkit 命令的统一入口（command 不含在 args 内）。 */
export function devkitCommand(
  bridge: HostBridge,
  command: string,
  args: string[],
): Promise<DevkitResult> {
  return sidecarCall<DevkitResult>(bridge, command, args);
}

function summarizeDevkit(command: string, result: DevkitResult): ToolOutcome {
  if (!result.ok) {
    const detail = result.error ?? '未知错误';
    const tail = result.log_tail ? `\n日志尾部：${String(result.log_tail).slice(0, 800)}` : '';
    return { ok: false, summary: `devkit ${command} 失败：${detail}${tail}`, stdout: '', stderr: '', exit_code: 1 };
  }
  const extra = Object.entries(result)
    .filter(([key]) => !['ok', 'log_tail'].includes(key))
    .map(([key, value]) => `${key}=${JSON.stringify(value)}`)
    .join(' ');
  return { ok: true, summary: `devkit ${command} 完成${extra ? `（${extra}）` : ''}`, stdout: '', stderr: '', exit_code: 0 };
}

function ok(summary: string): ToolOutcome {
  return { ok: true, summary, stdout: '', stderr: '', exit_code: 0 };
}

function fail(summary: string): ToolOutcome {
  return { ok: false, summary, stdout: '', stderr: '', exit_code: 1 };
}

/** 分发一次 Agent 工具调用：plugin_init / plugin_devkit / plugin_install。 */
export async function handleAgentTool(
  bridge: HostBridge,
  name: string,
  args: Record<string, unknown>,
): Promise<ToolOutcome> {
  try {
    if (name === 'plugin_init') {
      const template = String(args.template ?? '');
      const id = String(args.id ?? '');
      const displayName = args.name === undefined ? undefined : String(args.name);
      const result = await devkit.init(bridge, template, id, displayName);
      return summarizeDevkit('init', result);
    }
    if (name === 'plugin_devkit') {
      const command = String(args.command ?? '');
      const commandArgs = Array.isArray(args.args)
        ? args.args.map((item) => String(item))
        : [];
      const result = await devkitCommand(bridge, command, commandArgs);
      return summarizeDevkit(command, result);
    }
    if (name === 'plugin_install') {
      const id = String(args.id ?? '');
      const source = args.source === 'inbox' ? 'inbox' : 'dev';
      const result =
        source === 'inbox'
          ? await pluginDev.import(bridge, id)
          : await pluginDev.install(bridge, id);
      const origin =
        source === 'inbox'
          ? '已经用户确认并从收件箱导入'
          : '已安装（开发目录构建产物，指纹核验通过）';
      return ok(
        `插件 ${result.plugin_id} v${result.version} ${origin}（状态 ${result.state}，` +
          `${result.enabled ? '已启用' : '未启用'}）。含 extension.tab 贡献时可在拓展区打开；` +
          '含 mention 声明时可在输入框 @plugin:<id> 点名调用。' +
          '注意：本机自制插件安装后当前会话即可使用：经固定工具 call_local_plugin 动态调用' +
          '（清单由系统注入对话，已开启的会话在下一轮生效），无需新开会话。',
      );
    }
    return fail(`未知工具 ${name}`);
  } catch (error) {
    return fail(`工具 ${name} 失败：${error instanceof Error ? error.message : String(error)}`);
  }
}
