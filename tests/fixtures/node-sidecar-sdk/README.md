# node-sidecar-sdk

`tiangong-plugin-runtime` 测试用的 Node sidecar 协议库副本，供 runtime 测试拼装 Node 插件、验证宿主侧安装、启动、调用与取消链路。

- `index.mjs`：`plugins/sdk-sidecar/index.mjs` 的逐字节副本。
- runtime 测试经 `tiangong_plugin_runtime::test_support::node_sidecar_sdk()` 读取本文件，不直接引用 `plugins/` 目录（宿主基础库不依赖具体插件或 SDK 目录）。
- 真实 SDK 与宿主的协议往返由 `tests/plugin-sidecar-e2e`（`node_interpreter_e2e`）直接用 `plugins/sdk-sidecar/index.mjs` 覆盖；同一 crate 的 `fixture_sync` 测试校验本副本与真实 SDK 一致。

更新 `plugins/sdk-sidecar/index.mjs` 后同步本副本：

```bash
cp plugins/sdk-sidecar/index.mjs tests/fixtures/node-sidecar-sdk/index.mjs
```
