# @tiangong/plugin-sdk

天工插件 SDK：插件 UI 沙箱内访问宿主能力的统一客户端与类型定义。

## 安装

```sh
# 本地开发（仓库内引用）
yarn add @tiangong/plugin-sdk@file:../plugins/sdk
# 或直接把 src/index.ts 复制进工程
```

## 快速开始

插件 UI 入口（建议用 esbuild 等打包为单文件，`entry` 指向构建产物）：

```ts
import { createTiangongBridge, pluginStorage } from '@tiangong/plugin-sdk';

const bridge = await createTiangongBridge();

// 私有数据持久化（宿主落盘在插件 data 目录，无需逻辑层）
await pluginStorage.set(bridge, 'title', '我的看板');
const title = await pluginStorage.get(bridge, 'title');

// 带逻辑层的插件可调用自身 WASM 方法（plugin.* 转发）
const config = await bridge.call('plugin.getConfig', '{}');

// 事件订阅（Shadow 容器；需在 manifest capabilities.events 声明命名空间）
const off = bridge.on('session.updated', (payload) => console.log(payload));
```

## 容器适配

Vue/React 的虚拟 DOM 是插件内部渲染方式，Shadow/iframe 是宿主容器，两者互不替代。
例如 Vue 插件通常仍声明 `sandbox: "shadow"`。

| 容器 | manifest 声明 | 通道 |
| --- | --- | --- |
| shadow（默认） | `"sandbox": "shadow"` 或缺省 | 宿主动态注入桥接、插件根节点、上下文与清理登记 |
| iframe | `"sandbox": "iframe"` | `postMessage` 协议 |
| native | 仅官方签名插件 | 官方 React 组件 |

`createTiangongBridge()` 自动探测容器并返回统一的 `HostBridge`：

```ts
interface HostBridge {
  call(method: string, payload: string): Promise<string>;
  on(channel: string, handler: (payload: string) => void): () => void;
}
```

Shadow 插件可用 `getShadowHostRuntime()` 挂载前端框架，并在宿主动态更新或卸载插件时释放实例：

```ts
import { createTiangongBridge, getShadowHostRuntime } from '@tiangong/plugin-sdk';

const runtime = getShadowHostRuntime();
const root = runtime?.root ?? document;
const target = root.querySelector('#app');

const stopContext = runtime?.onContextChange((context) => {
  console.log(context.session?.id);
});
runtime?.registerCleanup(() => {
  stopContext?.();
  // 在这里卸载 Vue/React 等框架实例
});

const bridge = await createTiangongBridge();
```

已有只使用 `bridge` 的 Shadow 脚本无需修改。Shadow 只隔离样式，与宿主共享
JavaScript 环境；不可信插件应声明 `iframe`。

## 桥接方法命名空间

| 命名空间 | 能力 | 所需权限 |
| --- | --- | --- |
| `plugin.*` | 转发到本插件 WASM 逻辑层 | `bridge.call` |
| `storage.get/set/delete/list` | 插件私有数据读写 | `storage.private` |
| `session.*` | 按宿主版本渐进开放 | 见设计文档 6.3 |
| `tool.resolve` | Desktop TS 插件闭合自己声明的工具调用 | `tool.provide` |
| `app.open` | 打开本插件声明的拓展区 App | `app.use` + `extension.tab` 贡献 |

负载均为字符串（JSON 序列化），宿主不做业务解析。

声明了 `extension.tab` 的 App 插件应通过 SDK 决定是否自动展开拓展区：

```ts
import { openExtensionApp } from '@tiangong/plugin-sdk';

await openExtensionApp(bridge, {
  sessionId: invocation.session_id,
  instanceId: pageId,
  showPanel: invocation.arguments.open === true,
});
```

`showPanel` 缺省为 `true`；设为 `false` 时仍会建立正常、可见、可关闭的拓展区
顶部标签并计入在用标记，只是不自动展开拓展区面板（工具静默拉起）。插件工具
应尽量携带 `instanceId`：带编号的调用幂等聚焦同一标签；宿主内部在无插件 UI
接应时发起的兜底拉起（不带编号）与非当前会话的调用才会使用隐藏执行壳，不建
立可见标签。未声明 `extension.tab` 或缺少 `app.use` 权限时宿主会拒绝调用。

## Desktop TypeScript 工具提供器

纯 TypeScript 插件在 manifest 声明 `entrypoints: ["desktop"]`、`tools`、
`capabilities.tools=true`、`tool.provide` 权限和 `tool.*` 事件后，可以接收并闭合
自己声明的工具调用：

```ts
import { createTiangongBridge, createToolProvider } from '@tiangong/plugin-sdk';

const tools = createToolProvider(await createTiangongBridge());
tools.onRequested(async (invocation) => {
  await tools.resolve({
    invocation_id: invocation.invocation_id,
    status: 'answered',
    result: {
      ok: true,
      summary: JSON.stringify({ status: 'answered', result: true }),
      exit_code: 0,
    },
  });
});
tools.onClosed((closed) => console.log(closed.invocation_id, closed.status));
```

`status` 可为 `answered`、`expired` 或 `cancelled`。宿主保证调用只能闭合一次，
拒绝错插件、迟到和重复提交，并在插件未响应时按 manifest 的 `timeout_ms` 兜底。
默认交互处理器见 `plugins/tiangong-plugin-interaction`，它只是在这套通用协议上实现的一个
Desktop TS 工具插件。

## 接管消息显示（`session.message-item` + `render: "replace"`）

插件可为自己投递的消息声明渲染方式，由插件视图替换宿主默认的消息显示：

1. manifest 声明替换贡献（`render` 仅对 `session.message-item` 合法）：

   ```json
   { "slot": "session.message-item", "id": "im-message", "entry": "app/im-message.html",
     "sandbox": "shadow", "context": ["session", "message"], "render": "replace" }
   ```

2. 投递消息时携带渲染声明（`POST /api/v1/messages`）：

   ```json
   { "connector": "server-api", "channel_id": "<session_id>", "message": "用户原话",
     "instruction": "只给模型看的说明（界面不显示）",
     "render": { "plugin": "<插件 id>", "view": "im-message", "data": { "channel": "微信私聊" } } }
   ```

   界面内由插件发起的输入（`session.input.sendText`，需 `session.write` 权限）同样可带
   渲染声明，只填 `view` 与 `data`——`plugin` 由宿主固定为调用方；`view` 应是调用方
   自己声明的 replace 贡献，找不到时消息按默认样式显示：

   ```ts
   await bridge.call('session.input.sendText', JSON.stringify({
     text: '识别文本',
     render: { view: 'voice-bubble', data: { duration_s: 3 } },
   }));
   ```

3. 视图从 `context.message` 读取正文与 `render.data`：

   ```ts
   runtime?.onContextChange((context) => {
     const data = context.message?.render?.data;
     // 按 data 渲染卡片；正文用 context.message?.text
   });
   ```

约定：`render.data` 序列化后不超过 16KB；插件缺失、停用或未声明该视图时宿主按
默认方式显示正文，因此正文应保持可读。图片等媒体附件仍由宿主在插件视图下方渲染。
带渲染声明的用户消息不可编辑。

## 主题

iframe 容器在挂载及主题、会话切换时通过 `tiangong_host_context` postMessage
接收上下文。Shadow 容器中的 CSS 变量会从 App 根节点自然继承，插件 UI 直接使用
`var(--background)` 等变量即可跟随主题，不需要订阅或复制颜色；
`onContextChange` 只用于会话等非样式上下文。Shadow 脚本确实需要在 JavaScript
中判断当前主题时，可直接读取 App 根节点的 class 或计算后的 CSS 变量。
