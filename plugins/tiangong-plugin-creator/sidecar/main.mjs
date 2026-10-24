// plugin-creator 的按需 sidecar：承载 devkit 开发工具链（init/add/validate/
// build/run/logs）。每次工具调用由宿主独立起进程执行、完成即清——无进程内
// 状态、无存活窗口，页面与 Agent 工具共用同一入口（bridge sidecar.devkit.*）。
// devkit 源与协议库在构建时打进本产物；CLI（npx @silent-ai/plugin-creator）
// 形态保留，供终端与 CI 使用。
import { init } from '../../devkit/src/init.mjs';
import { add } from '../../devkit/src/add.mjs';
import { validate } from '../../devkit/src/validate.mjs';
import { build } from '../../devkit/src/build.mjs';
import { run } from '../../devkit/src/run.mjs';
import { logs } from '../../devkit/src/logs.mjs';
import { runSidecar, SidecarError } from '../../sdk-sidecar/index.mjs';

const commands = { init, add, validate, build, run, logs };

await runSidecar({
  pluginId: 'plugin-creator',
  // pluginVersion 由宿主环境变量注入（协议库缺省读 TIANGONG_PLUGIN_VERSION）。
  businessProtocol: 0,
  dispatch(operation, payload) {
    // @提及（内容由 sidecar 提供：entry 的 hint/mark + 候选数组）。
    if (operation === 'plugin-creator.mention_candidates') {
      return {
        entry: { hint: '插件创作：生成、构建与安装自建插件（devkit 工具链）' },
        candidates: [],
      };
    }
    const command = operation.startsWith('devkit.')
      ? operation.slice('devkit.'.length)
      : null;
    const handler = command ? commands[command] : null;
    if (!handler) {
      throw new SidecarError(`未知操作: ${operation}`, 'bad_request');
    }
    const args = Array.isArray(payload?.args)
      ? payload.args.map((item) => String(item))
      : [];
    const rootOverride =
      typeof payload?.root === 'string' && payload.root ? payload.root : undefined;
    return handler(args, { rootOverride });
  },
});
