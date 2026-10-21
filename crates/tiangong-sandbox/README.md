# tiangong-sandbox

`tiangong-sandbox` 是一个可独立使用的操作系统沙箱 Launcher。调用方提供策略和目标命令，Launcher 验证请求后使用当前平台的隔离能力启动目标进程；隔离能力不可用、策略非法或目标不可信时会明确拒绝执行，不会退回普通进程。

## 平台实现

- macOS：Seatbelt（`sandbox-exec`）
- Linux：bubblewrap（`bwrap`）
- Windows：AppContainer 与 Job Object

目标进程创建的子进程会继承相同约束。不同平台的底层能力有所区别，但策略入口和失败关闭原则保持一致。

## 安装目录

Sandbox 不决定宿主的存储布局。安装目录由 App、服务或其他调用方选择，目录内文件名约定为：

```text
<宿主选择的目录>/tiangong-sandbox[.exe]
<宿主选择的目录>/tiangong-sandbox[.exe].sig
```

库提供程序定位、签名验证和自检能力，但不会自动拼接天工的存储根或 `sandbox` 目录。

## 正式版本下载

正式版由独立发布流程构建、签名并发布到官方 OSS。推荐先读取 [最新版本清单](https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/latest.json)，清单包含当前版本、协议版本、各平台下载地址、SHA-256 和签名地址。

当前正式版为 `0.1.3`：

| 平台 | 程序 | minisign 签名 |
| --- | --- | --- |
| macOS Apple Silicon | [tiangong-sb-darwin-aarch64](https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/0.1.3/tiangong-sb-darwin-aarch64) | [签名](https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/0.1.3/tiangong-sb-darwin-aarch64.sig) |
| macOS Intel | [tiangong-sb-darwin-x86_64](https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/0.1.3/tiangong-sb-darwin-x86_64) | [签名](https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/0.1.3/tiangong-sb-darwin-x86_64.sig) |
| Linux x86_64 | [tiangong-sb-linux-x86_64](https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/0.1.3/tiangong-sb-linux-x86_64) | [签名](https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/0.1.3/tiangong-sb-linux-x86_64.sig) |
| Windows x86_64 | [tiangong-sb-windows-x86_64](https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/0.1.3/tiangong-sb-windows-x86_64) | [签名](https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/0.1.3/tiangong-sb-windows-x86_64.sig) |

手动下载后，将程序重命名为 `tiangong-sandbox`（Windows 为 `tiangong-sandbox.exe`），签名放在同目录并命名为程序名加 `.sig`。macOS 和 Linux 还需要添加执行权限：

```bash
chmod +x tiangong-sandbox
./tiangong-sandbox --self-check
```

版本源码与发布记录可在 [silent-rs/silent-Tiangong](https://github.com/silent-rs/silent-Tiangong) 查看。正式二进制以官方最新版本清单及其签名为准。

### 包管理器支持计划

目前正式版通过官方 OSS 和内置自更新能力发布，尚未提供包管理器安装命令。后续计划逐步增加：

- macOS：Homebrew Formula；
- Debian / Ubuntu：APT 仓库与 `.deb`；
- Fedora / RHEL 系：RPM 仓库；
- Windows：WinGet，并评估 Chocolatey / Scoop；
- Rust 用户：评估 crates.io 安装入口。

包管理器发布仍需复用同一套版本、SHA-256、minisign 验签和 `--self-check` 门禁。在对应仓库和自动发布流程上线前，不应使用尚未发布的 `brew install`、`apt install` 等命令。

## 快速使用

### 直接在命令行中设置策略

```bash
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
  -- command arg1 arg2
```

参数说明：

- `--mode`：`read-only` 或 `workspace-write`，默认 `workspace-write`；不允许 `full-access`。
- `--workspace`：主工作区，直接策略形式必须提供绝对路径。
- `--writable`：额外可写绝对路径，可以重复。
- `--protect`：即使位于可写范围内也保持只读的绝对路径，可以重复。
- `--deny-read`：禁止读取的绝对路径，可以重复。
- `--network`：`allow` 或 `deny`，默认 `deny`。
- `--max-cpu-seconds`：CPU 时间上限，必须大于零。
- `--max-memory-bytes`：内存上限，必须大于零。
- `--max-processes`：进程数量上限，必须大于零。
- `--`：策略参数与目标命令的分隔符。

未设置资源参数时，默认限制为 300 秒 CPU 时间、2 GiB 内存和 64 个进程。

### 使用策略文件

```bash
tiangong-sandbox run --policy /absolute/policy.json -- command arg1 arg2
```

`--policy` 不能与直接策略参数混用。策略文件使用 `SandboxPolicy` JSON：

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

## 默认安全行为

- 网络默认禁止。
- `workspace-write` 只允许写入工作区和显式指定的额外可写路径。
- `read-only` 不提供可写根。
- `full-access` 请求会被 Launcher 拒绝。
- 命令、工作区及策略路径必须通过形态和绝对路径检查。
- 目标程序会校验普通文件形态、摘要和平台权限。
- 平台沙箱不可用时拒绝执行，不静默降级。
- 敏感路径不会由通用核心隐式猜测；宿主应通过 `protected_paths` 和 `denied_read_paths` 明确传入。crate 另提供可选的常见凭据与天工宿主预设。
- 特殊能力（`capabilities`）默认为空，只能取 Launcher 内置的封闭集合：`credential_services`（系统凭据服务）、`audio_input`（音频输入）、`browser_runtime`（无头浏览器）。未知能力导致策略解析失败，当前平台不支持的能力会被拒绝，均不会静默忽略。

## 宿主协议

除 `run` 命令外，宿主也可以使用 Launcher 请求协议：

- Unix：通过继承的文件描述符 3 传入长度前缀 JSON 帧。
- Windows：通过一次性环境信封传入请求，并可关联宿主进程和停止事件。

请求包含独立的协议版本和策略 Schema。版本不兼容时 Launcher 会拒绝启动。目标校验默认使用路径、摘要和权限；需要天工插件清单比对时，宿主可以显式选择对应校验方式。

## 签名、自检与更新

检查当前平台能力和协议信息：

```bash
tiangong-sandbox --self-check
```

自检报告的 `capabilities` 字段列出当前平台支持的特殊能力；只查询能力（不运行拦截探针）可使用：

```bash
tiangong-sandbox --capabilities
```

宿主在启动前比对授予的能力与 Launcher 上报的能力，缺失时提示更新沙箱组件。

检查和安装官方更新：

```bash
tiangong-sandbox check-update
tiangong-sandbox update
tiangong-sandbox update --root /absolute/install-directory
```

默认更新链使用 crate 内置的官方公钥。第三方宿主可以显式提供自己的 HTTPS 更新清单和 minisign 公钥，但环境变量不能静默替换信任根。天工生产验证入口始终只接受内置官方公钥，并在验签后执行真实自检。

## 构建与验证

在仓库根目录执行：

```bash
cargo build --locked -p tiangong-sandbox --bin tiangong-sandbox
cargo check --locked -p tiangong-sandbox --all-targets
cargo test --locked -p tiangong-sandbox
cargo clippy --locked -p tiangong-sandbox --all-targets -- -D warnings
```

真实隔离测试需要当前环境允许应用对应平台的沙箱。已经处于受限沙箱中的开发环境可能无法再次嵌套，此时 Launcher 会报告平台不可用并拒绝执行。

## 设计资料

- [Sandbox 通用化与宿主安装目录](../../docs/sandbox-direct-storage.md)
- [Sandbox 自管理与首版发布](../../docs/sandbox-self-management.md)

## 许可证

Apache-2.0

### 独立宿主执行测试（issue #485）

`tests/launcher_execution.rs` 不依赖 Runtime/sidecar SDK。默认测试只执行策略拒绝及宿主测试辅助逻辑；真实隔离用例标记为 `ignored`，避免在天工命令工具的沙箱内嵌套执行。
必须从**沙箱外**原生终端或 CI runner 显式运行：

```bash
cargo test --locked -p tiangong-sandbox --test launcher_execution -- --ignored --test-threads=1 --nocapture
```

Windows 需安装 Node 22，并将独立 `node.exe` 的绝对路径设置为 `SANDBOX_TEST_NODE`；Sandbox CI 已自动准备。CI 显式启用用例后任何失败都直接失败，不以环境探测失败后返回制造成功。受限环境未运行不等于功能验证通过。

取消/超时由外部测试宿主执行：Unix 关闭本次调用独占的进程组，通过继承通信端全部关闭确认后台进程结束；Windows 通过停止事件通知 Launcher 终止 Job，再清理 ACL 和临时身份。并发取消不得影响另一次任务。

| 覆盖项 | 验证边界 |
| --- | --- |
| 敏感读取、越界写、网络、路径逃逸 | Launcher 真实自检报告，每项断言 |
| 工作区、专用 Temp、敏感只读豁免 | Unix CLI 文件操作；Windows CLI 授权及私有 Temp，专用 Temp 由自检覆盖 |
| CPU、内存 | Unix CPU 读回/实际超限、Linux 内存读回/实际分配；Windows Job 自检 |
| 取消、超时、并发隔离 | Unix 外部宿主进程组；Windows 生命周期自检 |
| 宿主异常退出 | Windows 现有 Job 自检；macOS SDK 的 kqueue 路径不属于独立 Launcher，不在此伪造实现 |
| 进程数限制 | Windows Job 自检；当前 Linux Launcher 未施加该限制，保留独立待办，不用目标自己 setrlimit 代替 Launcher 验证 |

此测试组不表示 issue #485 的所有能力已完成，也不改变 Launcher 生产代码。平台 CI 未运行时不得宣称三平台真实隔离通过。

### Windows CMD 的磁盘基本信息权限

Launcher 0.1.7 为 CMD 查询盘符信息提供独立能力。工作区已获读写权限时，CMD 仍可能因无法读取盘符根属性而失败。具有盘符根权限的管理员可一次性执行：

```text
tiangong-sandbox prepare-volume-metadata D:\workspace
```

仅为专用能力增加盘符根的读取属性和同步权限，不继承到子项，不开放根目录枚举或文件内容读写；使用目录句柄更新，不向整个磁盘传播权限。重复执行幂等，普通启动及退出不会修改这项授权。只支持本地盘符（含映射到本地目录的盘符）。不自动提权或修改根权限。

需要撤销时，由同样具备权限的管理员执行：

```text
tiangong-sandbox revoke-volume-metadata D:\workspace
```

原有文件授权、禁读目录、临时身份和退出清理仍生效。PowerShell 列目录本身不依赖这项准备。
