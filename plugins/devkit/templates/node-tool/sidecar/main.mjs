// {{PLUGIN_NAME}} 的工具 sidecar（node 运行时，按需执行，无界面）。
//
// 工具调用由宿主直连本 sidecar：操作名 = plugin.json 声明的工具名，
// 参数 = 工具调用参数对象；返回 ToolOutcome 形状
// （{ok, summary, stdout?, stderr?, exit_code?}）。
// 改造入口：下方 dispatch 的工具分支与 plugin.json 的 tools 声明。

import { runSidecar, SidecarError } from './vendor/tiangong-sidecar-sdk/index.mjs';

await runSidecar({
  pluginId: '{{PLUGIN_ID}}',
  businessProtocol: 0,
  dispatch(operation, payload) {
    // @提及：宿主查询时返回入口候选的展示字段（hint/mark）与动态候选数组；
    // 操作名与 plugin.json 的 mention.candidates_op 一致。
    if (operation === '{{PLUGIN_ID}}.mention_candidates') {
      return {
        payload: {
          entry: { hint: '{{PLUGIN_NAME}}（请改为一句话能力描述）' },
          candidates: [],
        },
      };
    }
    if (operation === 'text_analyze') {
      const text = typeof payload?.text === 'string' ? payload.text : '';
      const chars = [...text].length;
      const reversed = [...text].reverse().join('');
      return {
        payload: {
          ok: true,
          summary: `文本 ${chars} 字，倒序完成`,
          stdout: `字数：${chars}\n倒序：${reversed}`,
          stderr: '',
          exit_code: 0,
        },
      };
    }
    throw new SidecarError(`未知工具: ${operation}`, 'bad_request');
  },
});
