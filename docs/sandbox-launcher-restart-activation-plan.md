# Sandbox 直存与命令环境变量清单方案

## 目标

1. Sandbox 程序和签名直接放在天工存储目录的 `sandbox/` 下，不再叠加版本目录。
2. App 不维护 `active`、`pending` 版本指针，首次安装、启动自动更新与手动更新复用 Sandbox 自管理安装能力。
3. 命令环境变量屏蔽清单继续通过设置页 Modal 管理。
4. Sidecar 工具调用按权威会话路由，工作区由宿主根据会话 ID 加载，不能采用抢占事件的界面实例工作区。
5. macOS 官方 `computer-use` 为继承天工 App 的辅助功能授权，不套通用 Seatbelt；仍使用 stdio 并由宿主管理生命周期。该例外必须同时满足插件 ID 为 `computer-use` 且发布者已由官方公钥验签，第三方、自签、本地签名及同名插件不得获得例外。

## 磁盘布局

```text
<storage>/sandbox/
  tiangong-sandbox[.exe]
  tiangong-sandbox[.exe].sig
```

约束：

- 程序与签名必须是普通文件，拒绝符号链接。
- 版本由程序 `--self-check` 自报，不另存版本指针。
- 安装与更新使用 Sandbox `SelfUpdater::install_to`，复用 HTTPS、大小限制、SHA-256、官方验签、自检、跨进程锁与成对原子替换。
- 安装成功后清理遗留的 `active`、`pending`、`versions/` 和 `.transactions/`。
- App 每次启动 Sidecar 前仍对 Sandbox 逐次验签并检查协议与策略版本。

## 首次准备、自动更新与手动更新

- 启动准备页只校验本地程序：完整有效时直接进入应用，不因远端清单阻塞启动。
- 本地缺失或损坏时，启动准备页调用安装入口；失败不阻塞对话与浏览，需要 Sandbox 的功能保持拒绝执行。
- 启动后宿主在后台检查一次官方清单（与插件自动升级一致）：本地程序有效且远端版本更新时调用同一安装入口替换；本地无效时跳过，交由启动准备页处理。全程无弹窗，离线或失败仅记日志，下次启动重试。
- 设置页“检查并更新”调用相同安装入口，直接更新固定路径中的程序和签名。
- Sandbox 后续也可直接执行自身的 `check-update` 与 `update`。

## Command 插件执行边界

- Command 始终由独立 Command Sidecar 执行业务逻辑；Runtime 不实现命令解析、审核、执行或输出收集旁路。
- 沙箱开启时 Runtime 经 Launcher 启动 Command Sidecar；沙箱关闭时 Runtime 直接启动同一 Sidecar。开关只改变隔离边界，不改变工具执行者。
- Runtime 仅负责权威会话工作区、用户目录白名单与环境黑名单、stdio 传输、请求取消和进程生命周期。

## 会话路由

- 当前 Terminal 暂时沿用主分支的插件级 `tool.requested` 订阅与 invocation claim 机制；每会话唯一 `TerminalManager`、按会话精确投递和视图/调度职责分离作为后续专项实现，详见 GitHub Issue #471。
- 在专项完成前，宿主侧 Sidecar 权限与工作区路由仍必须以 `invocation.session_id` 对应的 Session 为权威，不能信任抢占事件的页面实例上下文；该约束保证临时回退不扩大文件系统权限。
- Shadow/iframe Bridge 只把会话 ID传给宿主，不把页面实例中的 workspace 当作权限依据。
- 宿主按会话 ID 从 Session 真相源读取 `cwd`，再构造或选择对应工作区的 Sidecar 连接。
- 会话缺失、工作区为空或路径无效时明确拒绝，不回退到抢占实例或当前可见会话。

## 独立沙箱设置页与用户策略

- 设置页提供独立“沙箱”Tab，集中管理开关、Launcher 更新、路径规则与环境规则。
- 用户可自主配置目录白名单；白名单目录额外开放读取和写入，宿主仅做绝对化、规范化与去重，不限制授权范围。
- 用户可自主维护环境变量黑名单；黑名单变量从按需进程环境中移除。
- 宿主管理面强制保护优先于用户规则：`app.json`、`keys/`、`trust.db`、`sandbox/`、Launcher/签名与授权配置不可由沙箱进程读取或写入，用户白名单不能覆盖。
- 策略只能经宿主设置命令写入并原子持久化；插件清单、Agent 参数、工作区文件与沙箱进程不能修改管理项。

## 环境变量 Modal

- 设置页仅显示配置数量和“管理”入口。
- Modal 使用本地草稿，每行一项并兼容中英文逗号。
- 名称按 `^[A-Za-z_][A-Za-z0-9_]*$` 校验，大小写不敏感去重。
- 取消不保存，保存失败保留草稿，非法项整次拒绝。
- 系统内置屏蔽项不重复保存。

## 验收

- 稳定状态下 `sandbox/` 根目录只包含当前程序与签名，不存在版本仓库或指针。
- 程序缺失、签名错误、自检失败或协议不兼容时 Sidecar fail-closed。
- 多会话同时存在终端实例时，工具调用只由所属会话处理，实际可写工作区与 `invocation.session_id` 对应 Session 一致。
- Rust workspace、前端、终端插件构建和相关测试通过。
- macOS 官方 `computer-use` 由天工授权后可访问辅助功能；非官方同名插件及其他插件仍保持 OS 沙箱，且所有路径继续使用 stdio 与宿主生命周期管理。
