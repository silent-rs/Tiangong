# Command Plugin

Command 插件提供 `run_command` / `run_shell` 基础命令执行，访问边界由 Runtime 与 Launcher 的系统沙箱统一实施。插件由三个独立 crate 组成：

- `protocol`：WASM 与 sidecar 共用的私有业务协议（`CommandAccessContext` 只携带权威工作区）；
- `wasm`：工具规格、参数解析、prompt 段落与生命周期入口，桥接 sidecar；
- `sidecar`：负责 tokio 子进程启动、受控环境、超时取消和输出处理。

通用的清单、WASM、sidecar 和本地导入说明见 [WASM 插件开发指南](../../../docs/plugin-development.md)。

完整插件使用以下命令统一检查、构建和部署：

```bash
cargo run -p xtask -- build-plugin command
```

## 入口分工

> **已弃用，进入淘汰流程（issue #697）**：CLI / Server 的命令执行已由
> terminal 插件承担（PTY 复用、交互程序、画面快照）。本插件的
> `entrypoints` 已清空——所有入口均不再注册，升级后自然淘汰，随后按
> 卸载流程归档移除。存量安装目录保留至卸载阶段，期间不提供回退开关。

- 全部入口：不注册（`entrypoints: []`），`run_command` / `run_shell` 由 terminal 插件（PTY 执行 + 命令回显）提供。

## 沙箱职责

- Runtime 根据权威会话工作区和用户设置构造沙箱策略。
- Launcher 施加目录、环境与系统能力边界。
- Command 不再维护命令名称白名单、参数路径猜测或 Shell 文本风险清单。
- Command 保留最小运行环境重建、危险环境变量过滤、超时取消和输出截断。
