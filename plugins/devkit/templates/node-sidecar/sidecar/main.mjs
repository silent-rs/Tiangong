// {{PLUGIN_NAME}} 的常驻 sidecar 入口（node 运行时，stdio 通道）。
//
// 宿主以 `node sidecar/main.mjs` 启动本文件；协议细节（认证、握手、
// 请求/响应/进度）由 vendor 内的协议库处理，业务只实现 dispatch。
// 改造入口：下方 dispatch 的操作分支；新增操作后同步更新插件页面的
// bridge.call 调用与 plugin.json 的工具声明。

import { runSidecar, SidecarError } from './vendor/tiangong-sidecar-sdk/index.mjs';

await runSidecar({
  pluginId: '{{PLUGIN_ID}}',
  // pluginVersion 由宿主环境变量注入（协议库缺省读 TIANGONG_PLUGIN_VERSION）。
  businessProtocol: 0,
  dispatch(operation, payload, ctx) {
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
    if (operation === 'demo.echo') {
      ctx.progress('echo 处理中');
      return {
        payload: {
          text: typeof payload?.text === 'string' ? payload.text : '',
          received_at: new Date().toISOString(),
        },
      };
    }
    throw new SidecarError(`未知操作: ${operation}`, 'bad_request');
  },
});
