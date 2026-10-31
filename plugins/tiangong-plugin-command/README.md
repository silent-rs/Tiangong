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

> **已弃用（issue #697）**：CLI / Server 的命令执行默认由 terminal 插件
> 承担（PTY 复用、交互程序、画面快照）。本插件仅在 `app.json` 的
> `agent_config.command_backend = "legacy-command"` 显式回退时于 CLI /
> Server 注册；计划在两个正式版本后移除。回退开启时 terminal 在
> CLI / Server 不注册，两个插件的 `run_command` / `run_shell` 不会同时
> 进入工具表。

- CLI / Server：默认不加载；仅显式回退开关开启时由 runtime 注册。
- GUI：不加载本插件（entrypoints 不含 desktop），`run_command` / `run_shell` 由 terminal 插件（PTY 执行 + 命令回显）提供。

## 沙箱职责

- Runtime 根据权威会话工作区和用户设置构造沙箱策略。
- Launcher 施加目录、环境与系统能力边界。
- Command 不再维护命令名称白名单、参数路径猜测或 Shell 文本风险清单。
- Command 保留最小运行环境重建、危险环境变量过滤、超时取消和输出截断。
