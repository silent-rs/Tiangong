# node-sidecar 模板

带常驻 node sidecar 的全工程化工具插件：页面用 Vue + TypeScript + 插件 SDK
（与 ts-tool 同款开发体验），sidecar 用 Node 写常驻逻辑。Agent 调用 `query`
工具时，页面经宿主桥接把请求转发给常驻 sidecar 处理。

## 构建产物

`build` 一步完成两端打包：

- 页面：vue-tsc 类型检查 + vite 单文件打包 → `dist/index.html`；
- sidecar：esbuild 将 `sidecar/main.mjs` 连同 vendor 协议库与你在
  `package.json` 的 `dependencies` 打成**单个自包含文件**。

`package` 组装 `release/` 并生成内容哈希清单。安装后的插件运行时零依赖、
零网络（构建期才需要网络装依赖），全部文件参与防篡改哈希锁定。

## 依赖管理

- 添加第三方依赖：`npx -y @silent-ai/plugin-creator@1.0.2 add <id> <包名>`
  （经命令通道执行，锁定精确版本），在代码里正常 `import`。
- 限制：原生 `.node` 模块暂不支持打包；需要时用官方插件链路（Rust）。

## 结构

- `plugin.json`：声明 `sidecar.runtime = "node"`、入口 `sidecar/main.mjs`、
  `sidecar.invoke` 权限与 `query` 工具。
- `src/`：Vue 工具页（`App.vue` 的 `handleToolCall` 转发 sidecar 操作）。
- `sidecar/main.mjs`：sidecar 源码，业务写在 `dispatch`（示例操作 `demo.echo`）。
- `sidecar/vendor/tiangong-sidecar-sdk/`：sidecar 协议库副本（上游
  `plugins/sdk-sidecar`，改库后同步复制）。
- `vendor/plugin-sdk/`：页面 SDK 副本（上游 `plugins/sdk`，与 ts-tool 同源）。
- `scripts/build.mjs` / `scripts/package.mjs`：构建与组装 release。

## 运行形态

- 本模板声明 `lifecycle: "resident"`（常驻）：跨调用复用进程，可保存进程内
  状态与发送通知。省略该字段即为按需（默认）——每次调用独立进程、完成即清，
  适合一次性任务；按需模式无进程内状态，通知不可用。
- 开发机构建：yarn + Node ≥ 20（网络装依赖）。
- 宿主运行：PATH 中的 `node` ≥ 20（或 `TIANGONG_NODE_PATH` 指定）。
- 安装经 plugin-creator 自动签名（免确认）；安装后 sidecar 文件被改动将拒绝启动
  （内容哈希锁定）。

## 迭代

修改页面或 sidecar 后，重新构建安装：

```bash
npx -y @silent-ai/plugin-creator@1.0.2 validate <id>
npx -y @silent-ai/plugin-creator@1.0.2 build <id>
```

然后在「插件创作」页安装（或让 Agent 调 `plugin_install`）。

## 请求取消

模板使用 sidecar SDK 0.2.0。长任务应监听 `ctx.signal`，并在 `cancel` 钩子中清理子进程和句柄。

## 实例资源（instance_resources）

若每个拓展区标签实例持有后端资源（子进程、PTY、远程连接等），在 `plugin.json`
的 `extension.tab` 贡献上声明 `"instance_resources": true`，并在 sidecar 的
`dispatch` 中实现两个宿主协议操作（均需幂等）：

- `instanceClosed`：`{ session_id, instance_id }` → 释放该实例资源（未知编号返回成功）；
- `listInstances`：`{ session_id }` → `{ instances: [{ instance_id }] }`。

资源一律按页面 `runtime.context.app.instance_id` 归属。标签关闭后宿主保证发出
`instanceClosed`；切换会话时宿主按 `listInstances` 恢复标签并释放没有标签的多余
资源；删除会话时释放全部实例。`registerBeforeClose` 只做状态保存。无后端资源的
插件不要声明该字段。
