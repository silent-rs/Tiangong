# 终端插件（Terminal）

终端能力的 manifest v2 插件化。工具声明与 prompt 引导来自本清单，经
TsPluginAdapter 注入 Core。

工具执行策略在 sidecar（`sidecar/src/service/`，Rust 进程，按职责分模块，见 `mod.rs` 模块说明）：plugin.json
sidecar 握手通过 `tool:<name>` capabilities 注册五个工具 Handler，宿主把工具调用直连 sidecar，并随请求注入
宿主权威会话上下文（`session_id` 与会话工作目录，来自 Session 真相源）。
## 入口分工（issue #697）

- 桌面 / CLI / Server 全部入口：`run_command` / `run_shell` 与
  `terminal`（action 模式）由本插件提供（PTY 执行、终端复用、交互
  程序支持）。
- 旧 command 插件已弃用：entrypoints 已清空，所有入口不再注册，升级
  后自然淘汰并按卸载流程移除。

## 运行形态（issue #697）

宿主在 spawn sidecar 时经 TIANGONG_HOST_UI 注入运行形态：

- desktop：有前端界面。工具执行建立/收起插件标签，输出经
  terminal.output 推送给 xterm 渲染，交互画面快照由前端
  terminalScreenUpdate 回传。
- headless（CLI / Server）：没有 App 层。不发出 app.open / app.close
  反馈、不等待前端附着、不推送输出通知；画面快照由 sidecar 内的屏幕
  解析器（sidecar/src/screen.rs，vte 状态机 + 网格模型）生成，
  terminal_send 对 vi / top / less 等全屏程序同样能返回正确画面。

生命周期：CLI 退出与 Server 会话删除时，宿主调
release_session_plugin_resources（listInstances + instanceClosed）回收
会话终端；无主终端由空闲 PTY 超时回收兜底（默认 30 分钟，环境变量
TIANGONG_TERMINAL_IDLE_PTY_SECS 可调，0 关闭）。

每个会话的终端编排（选终端、新建、执行、收结果）由 sidecar 内对应会话
作用域唯一完成；页面实例（`src/main.ts` + `terminal-view.ts`）只承担显示
与输入，不参与工具调度，也无 `tool.requested` 竞争接应。

## 执行模型（issue #697 v7：拉模型）

`run_command` 是唯一的命令执行入口：`cmd` 为 shell 命令文本（管道、重定向、
`&&` 直接写入）；`terminal_id` 可选——指定终端忙则拒绝并告知当前状态，缺省
由插件复用最早空闲终端、无空闲自动新建。`timeout` 是**单次等待命令反馈的
窗口**（默认 5 秒、上限 110 秒）：窗口内完成返回完整输出与退出码（marker
协议）；未完成**不中断命令**，登记为可续等命令并返回 still_running 与已产
生的输出——是不是交互程序、在不在等输入由 Agent 从输出内容自行识别
（sidecar 不做理解性判定），再用 `terminal`（action=send，input 留空）续等
收割完整输出与退出码、action=status 查询画面与进程，或发送 `\x03` 打断。
预期耗时较长的命令应显式设置更长的 `timeout`。工具层没有任何自动打断路径
（宿主会话取消除外，用户主动取消属于合理打断）。

交还的终端保持执行态不被复用；命令自行完成后由选终端时的接触检测**就地
收割**放回池（`settle_handed_over`），或经续等调用正常收割。

`terminal`（action=send/close/status；open 已删除，新终端由 run_command
自动或经 terminal_id 指定）：
- `send`：`terminal_id` 必填。空 input 且不设 enter 为续等——marker 闭合
  即返回 `completed` 与退出码，未闭合等满 `wait`（默认 10、上限 110 秒）
  返回 `still_running` 与该期间增量输出。`enter: true` 用于在 shell 里
  执行命令（正文写入后单独补发回车，等命令真正结束）；向 vi/REPL 等交互
  程序写入用不设 enter 的原样输入（支持 `\r`、`\n`、`\xHH`、`\e` 转义）。
  上一条命令未完成时 `enter=true` 的新命令被拒绝且不写入（防止排进 tty
  输入缓冲导致输出错位）；只含控制键的输入（如 `\x03`）即使带 enter 也
  按按键直接写入。终端不复用前台被程序占据的终端（如已 SSH 登录）。
- `status`：概要（忙闲、后台遗留进程）或详情（运行命令、输出尾部、
  **画面快照**（sidecar 屏幕模拟器生成，vi/top 等全屏程序的当前画面，
  不依赖前端是否在观看）、会话内进程树）。拉模型的信息中枢：等待中想
  了解进展就查它。
- `close`：杀 PTY 释放资源；不触碰 App 标签（UI 归宿主）。

`run_command` / `terminal send` 的 `enter=true` 命令完成判定：本地 shell 按
前台进程组回到 shell 判定，SSH 等交互程序内按发送前的提示符重新停在光标行
判定；提示符变化（cd、REPL 续行、密码提示）时静默后返回 `interactive`
交还控制权，终端不会被永久锁住。

终端里 `&` / `nohup` 启动的后台进程留在登录 shell 的会话里，sidecar 会
登记这些会话：仍有存活的后台进程时不做空闲退出（退出后新 sidecar 在另一
个沙箱实例中，无权向其发信号），`terminal(action=status)` 概要以
`background_jobs` 列出其 pid、进程名与所属终端，供 Agent 用 `kill` 结束。

UI 展示全归宿主（App 层）：sidecar 不发送 `app.open` / `app.close`
host_action；前端在会话挂载/切换时经 `listInstances` 以同一编号恢复该
会话全部终端标签（按需拉取）。标签关闭时页面按天工会话提交当前仍存活
的终端编号全集（terminalGc），sidecar 只回收该会话中已不在列表内的终端。
Sub Agent / Bot 等后台会话无需任何页面实例即可独立执行终端工具。

## 使用引导

prompt 与工具说明引导 Agent 以 PTY 终端的方式工作：

- 执行命令一律用 `run_command`（shell 命令文本），多步操作拆成多次调用
  逐步执行；终端自动复用并保持工作目录与环境。
- 长命令显式设置更长的 `timeout`；返回 still_running 时按输出内容判断
  状况，空 input 续等收割、status 查询或 `\x03` 打断。
- 不用 `nohup` / `&` 后台启动：长期服务直接 `run_command` 前台跑（占住
  一个终端，其他命令自动用别的终端），用完 `\x03` 停止并 close。

sidecar 在 `run_command` 结果摘要末尾对这两类写法给出替代做法
（`service/advice.rs`，只提示不拦截，命令照常执行）。

## 开发循环

```sh
# 首次：确认官方私钥就位（不进仓库；有密码时另设
# TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PASSWORD）
export TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH=~/.tiangong/keys/plugin-signing.key

yarn package   # 构建 + 组装 release/ + 官方签名（release.json/.sig）
# 天工「设置 → 插件管理 → 导入本地插件」选择 release/ 目录
```

sidecar 插件必须带官方签名才能启动原生 sidecar（`tauri signer` 签名，
需要本地安装 tauri-cli）。无官方私钥时可用自生成密钥调试：
`tauri signer generate -w <key>` 打包签名后，启动应用时设置
官方信任根为应用内置公钥，不接受环境变量或配置覆盖（历史测试通道已移除）。

## 与宿主的协议

- 权限：`tool.provide`（工具结果提交）+ `sidecar.invoke`（视图直调
  sidecar 操作）+ `app.use`（App 标签原语）
- 工具直连：sidecar 握手以 `tool:<name>` 注册 Handler；宿主通过统一
  Runtime 调用，sidecar 从运行库读取 session_id、invocation_id 和权威
  workspace，缺失即拒绝执行
- 调用闭合：Handler 不设统一时限，由会话取消或宿主退出闭合；
  `run_command` / `run_shell` 仅在调用参数显式提供 `timeout` 时中断命令

### Windows 默认终端

Windows 默认使用系统 PowerShell；已设置 `SHELL` 时沿用其选择。就绪探针、参数引用及执行结果按实际 Shell 处理。PowerShell 使用无个人配置的交互会话，避免启动脚本及 PSReadLine 干扰命令边界；不修改系统执行策略。CMD 命令可通过 `command` 调用 `cmd.exe` 并传入 `/d`、`/c` 和脚本参数，显式创建的 CMD 终端也可连续执行。

CMD 在 Windows 沙箱内需要读取磁盘根的基本信息；对应的最小权限准备见沙箱 README，不等同于开放磁盘文件访问。
