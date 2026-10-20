# Fs Plugin

Fs 插件（基础文件工具：list_dir / tree_dir / read_file / read_image / write_file / replace_in_file / apply_patch / current_time）由三个独立 crate 组成：

- `protocol`：WASM 与 sidecar 共用的私有业务协议（含 `FsAccessContext` 访问能力上下文，为后续沙箱能力预留）；
- `wasm`：工具规格、参数解析与生命周期入口，桥接 sidecar（`current_time` 例外，用 clock host import 本地实现）；
- `sidecar`：文件读写（`std::fs`）、进程级文件锁表（跨 wasm 实例共享）、路径解析策略（可替换，为沙箱预留）进程。

通用的清单、WASM、sidecar 和本地导入说明见 [WASM 插件开发指南](../../../docs/plugin-development.md)。

完整插件使用以下命令统一检查、构建和部署：

```bash
cargo run -p xtask -- build-plugin fs
```

## 为什么走 sidecar

文件读写本身可由 WASI 承接，但 fs 走 sidecar 的真正理由是**锁表需跨 wasm 实例全局共享**：主 Agent 与子 Agent 是两个内存隔离的 wasm 实例，各自持有锁表互看不见；锁表只能落在所有实例共享的单一 sidecar 进程内，才能实现写同一文件的互斥。路径解析（动态工作区 + FullTrust 越界）随之一起下沉，避免为 fs 专用定制 WIT host import。

## 沙箱预留

- **路径策略可替换**：sidecar 内 `PathPolicy` trait 抽象了路径解析，当前实现基于信任模式；未来引入 landlock / 路径白名单 / overlay 时只换实现，业务代码不动。
- **访问能力随请求下发**：`FsAccessContext`（protocol 层）承载当前会话的 `full_trust` + `workspace`，未来细化权限只扩结构并 bump business-protocol，不动 WIT/wasm。
