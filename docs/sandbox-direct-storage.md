# Sandbox 通用化与宿主安装目录

## 目标

按照 issue #458 的 P1 范围，让 Sandbox 可以脱离天工插件上下文独立使用，同时保持天工生产信任边界不降低。

## 需求

1. Sandbox 不认识天工的 `storage_root`，安装目录完全由宿主决定。库只负责在宿主传入的目录中定位 `tiangong-sandbox[.exe]` 和伴生签名。
2. 提供策略文件和直接命令行参数两种通用执行形式：

```text
tiangong-sandbox run --policy <policy.json> -- <command> [args...]

tiangong-sandbox run \
  --mode workspace-write \
  --workspace /absolute/workspace \
  --writable /absolute/run-temp \
  --protect /absolute/workspace/protected \
  --deny-read /home/user/.ssh \
  --network deny \
  --max-cpu-seconds 300 \
  --max-memory-bytes 2147483648 \
  --max-processes 64 \
  -- <command> [args...]
```

3. `--writable`、`--protect` 和 `--deny-read` 可以重复。直接参数缺省使用 `workspace-write`、禁止网络和默认资源上限，并且必须提供绝对路径的 `--workspace`。
4. `--policy` 与直接策略参数互斥，不做隐式合并或覆盖。策略文件直接使用 `SandboxPolicy` JSON。保护路径、禁读路径、额外可写路径和网络权限全部由宿主传入。
5. 目标程序默认执行路径、摘要、文件类型和权限校验，不要求 `plugin.json`。天工插件宿主可以显式启用插件清单比对。
6. 常见凭据和天工数据文件清单是可选预设，不是通用策略的隐式默认值。
7. 更新端点和 minisign 信任根可由调用方通过显式参数配置；不允许环境变量静默替换生产信任根。
8. 天工生产安装验证入口仍只接受 crate 内置官方公钥，并在验签后执行自检。

## 兼容性

- fd3 / Windows 信封协议继续可用。
- 新增的目标校验字段缺省为通用摘要校验；旧 `interpreter` 字段继续兼容。
- 策略中新增默认值，旧策略缺少额外可写、保护/禁读、网络和资源限制字段时仍可解析。
- 请求不再按协议版本或策略 Schema 版本化：新增字段一律带默认值，Launcher 忽略请求中的 `protocol_version` / `policy_schema`，宿主也不再据自检报告中的这两个字段判断兼容性。
- 特殊能力改用 `capabilities` 集合表达。Launcher 内置临时兼容层，继续接受已发布 App（≤ v0.17.5）的 `allow_credential_services` / `allow_audio_input` / `allow_browser_runtime` 布尔字段（并入能力集合），因此可以**先发布 Launcher、后发布 App**。
- 为兼容已发布组件，Launcher 自检报告与更新清单仍输出冻结值 `protocol_version = 1`、`policy_schema` / `policy_schema_max = 2`，宿主请求也继续携带这两个值（已发布 Launcher ≤ 0.1.15 要求必填且等值）；定义见 `tiangong_sandbox_update::legacy`，新代码不读取。
- 新 App 遇到能力协商之前的旧 Launcher（自检报告无 `capabilities`）时视为不支持任何特殊能力：无需特殊能力的插件照常运行，需要的插件提示更新沙箱组件。
- 宿主（App）不依赖 `tiangong-sandbox`（#703）：安装、更新、验签、自检与交互常量在轻量 crate `tiangong-sandbox-update`；启动策略由宿主按 JSON 约定自行构造（`tiangong-plugin-runtime` 的 `launch_policy`），契约测试保证 Launcher 按相同语义解析；Windows 持久授权经 Launcher `revoke-grants` 子命令撤销，旧 Launcher 不支持时记日志并在更新后回收。
- 已发布 App 与 Launcher 全部退出支持后，可移除布尔兼容层与上述冻结字段，或决定永久保留。

## 策略示例

```json
{
  "mode": "workspace_write",
  "workspace": "/absolute/workspace",
  "extra_writable": ["/absolute/run-temp"],
  "protected_paths": ["/absolute/workspace/protected"],
  "denied_read_paths": ["/home/user/.ssh"],
  "allow_network": false,
  "capabilities": [],
  "resource_limits": {
    "max_cpu_time_seconds": 300,
    "max_memory_bytes": 2147483648,
    "max_processes": 64
  }
}
```

## 非目标

1. 不执行 P2 的独立仓库、中性产品命名或公共宿主集成 crate 拆分。
2. 不迁移旧版本目录，不增加最低产品版本门槛。
3. 不修改终端跨会话路由、设置页和启动准备页。

## 完成标准

- 宿主可自行选择任意绝对安装目录。
- 无天工插件清单时可用策略文件或直接命令行策略执行普通命令。
- 越界写和禁读行为仍由三平台现有沙箱实现强制执行。
- 天工生产官方根与第三方显式信任根入口分离。
- Sandbox crate 构建、测试和严格检查通过。
