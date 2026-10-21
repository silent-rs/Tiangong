# runtime-probe

`tiangong-plugin-runtime` 集成测试专用的最小 WASM 插件（`wasm32-wasip2` 组件，WIT `tiangong:plugin@0.1.0`）。
与应用主体无关，不属于主工作区（`Cargo.toml` 自带空 `[workspace]`），CI 不编译。

源码与制品一起提交：

- `src/lib.rs`、`Cargo.toml`、`Cargo.lock`：源码，WIT 直接引用 `crates/plugin/tiangong-plugin-runtime/wit/tiangong/plugin.wit`。
- `runtime_probe.wasm`：测试直接加载的制品。
- `runtime_probe.wasm.sha256`：制品摘要，测试加载前校验，不一致时直接失败（不会静默跳过）。

| 项 | 值 |
| --- | --- |
| 插件 ID / 名称 / 版本 | `runtime-probe` / `Runtime Probe` / `1.0.0` |
| 工具 | `echo`、`fail`、`spin`、`sidecar` |
| 提示段落 | `runtime-probe prompt` |
| 设置页贡献 | `probe-settings` |

行为（全部由参数驱动，不含业务逻辑）：

- `echo`：成功，`stdout` 为原始参数 JSON。
- `fail`：返回插件错误 `probe failure: fail`。
- `spin`：死循环，用于验证 fuel 上限中断。
- `sidecar`：参数 `{"operation": "...", "payload": "..."}`，经宿主 `sidecar.invoke` 转发；成功时 `stdout` 为响应，sidecar 不可用时返回 `ok=false`。
- 生命周期钩子 `on-session-ready` / `on-turn-started` / `on-turn-finished` / `on-session-ended`：把收到的会话 JSON 原样转发给 sidecar，操作名为 `hook.session_ready`、`hook.turn_started`、`hook.turn_finished`、`hook.session_ended`（sidecar 未配置时忽略）。
- 设置页消息 `get` / `set`：读写 WASI `/data/probe.txt`（宿主挂载的插件数据目录）。

## 重新生成制品

WIT 变更、换设备或制品无法加载时，在本目录执行：

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2 --target-dir target
cp target/wasm32-wasip2/release/runtime_probe.wasm runtime_probe.wasm
shasum -a 256 runtime_probe.wasm > runtime_probe.wasm.sha256
```

`target/` 已被仓库忽略。构建使用锁定的 `wit-bindgen =0.57.1` 与 `Cargo.lock`；当前制品由 rustc 1.99.0 生成，同一工具链下产物可复现，不同工具链下摘要可能变化，按上述步骤同时更新制品与摘要即可。
