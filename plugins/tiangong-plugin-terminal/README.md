# 终端插件（Terminal）

终端能力的 manifest v2 插件化。工具声明与 prompt 引导来自本清单，经
TsPluginAdapter 注入 Core。

工具执行策略在 sidecar（`sidecar/src/service/`，Rust 进程，按职责分模块，见 `mod.rs` 模块说明）：plugin.json
sidecar 握手通过 `tool:<name>` capabilities 注册五个工具 Handler，宿主把工具调用直连 sidecar，并随请求注入
宿主权威会话上下文（`session_id` 与会话工作目录，来自 Session 真相源）。
每个会话的终端编排（选终端、新建、开标签、执行、收结果）由 sidecar 内
对应会话作用域唯一完成；页面实例（`src/main.ts` + `terminal-view.ts`）
只承担显示与输入，不参与工具调度，也无 `tool.requested` 竞争接应。

`run_command` / `run_shell` 不接收终端编号：优先复用当前会话的第一个
空闲终端，没有可用终端时自动新建并静默建立标签（不展开拓展区）。
工具结果会返回实际终端编号；`terminal_send` / `terminal_close` 必须用
该编号精确操作，且目标终端必须属于当前会话——跨会话操作明确拒绝，
不回退当前可见会话。

`terminal_send` 默认原样写入 `input`（支持 `\r`、`\n`、`\e`、`\xHH` 等转义），
适合向 vi 等编辑器写入纯文本或发送控制按键。设 `enter: true` 时先去掉
`input` 末尾的一个换行，正文写入后间隔片刻再单独写入回车（`\r`），用于
执行命令——SSH 等远端行编辑器在正文与回车同包到达时偶发吞掉回车，分开
写入可稳定触发执行。

`enter: true` 发出的命令会等到真正结束才返回 `completed`，`wait`（默认
10 秒，最大 110 秒）只是上限：本地 shell 按前台进程组回到 shell 判定，
SSH 等交互程序内读不到对端进程，按发送前的提示符重新停在光标行判定；
程序退出、前台回到登录 shell 也判为完成。提示符变了（远端 `cd` 改了带
路径的提示符、REPL 续行 `...`、`[sudo] password:`）时，输出静默 1.5 秒
且光标停在一行未完成文本上即返回 `interactive` 交还控制权，终端不会被
永久锁住。发送 `\x03` 只是把中断键交给程序（只含控制键的输入即使带了
`enter: true` 也按按键直接写入，不会被当作新命令拒绝）：程序真被中断
后提示符重绘即完成；忽略中断的程序仍在跑时新命令继续被拒绝，避免排进
输入缓冲、等程序结束才执行。
超过上限仍未结束返回 `still_running` 与截至目前的新增输出；此时再以
`enter: true` 发送新命令会被拒绝且不写入（否则会被排进 tty 输入缓冲延后
执行，输出整体错位），`input` 留空且不设 `enter` 即继续等待并增量读取。
`run_command` / `run_shell` 不复用前台被程序占据的终端（如已 SSH 登录）。

终端里 `&` / `nohup` 启动的后台进程留在登录 shell 的会话里，sidecar 会
登记这些会话：仍有存活的后台进程时不做空闲退出（退出后新 sidecar 在另一
个沙箱实例中，无权向其发信号），`terminal_status` 概要以
`background_jobs` 列出其 pid、进程名与所属终端，供 Agent 用 `kill` 结束。

sidecar 执行中经进度帧请求宿主 App 原语（`host_action`，白名单
`app.open` / `app.close`）：长任务执行期间终端标签即时建立，用户可
实时查看输出。Agent 新建终端时，隐藏标签先完成输出订阅和精确 PTY
附着，sidecar 随后才开始命令；历史只用于重新附着同一存活 PTY，不会
填充到新建终端。

顶部标签切换或拓展区暂时隐藏时保留当前终端；新建或明确关闭终端 App
标签时，页面按天工会话提交当前仍存活的终端编号全集，sidecar 只回收该
会话中已不在列表内的终端。前端关闭不等待回收结果，再次打开会创建全新
终端。Sub Agent / Bot 等后台会话无需任何页面实例即可独立执行终端工具。

## 使用引导

prompt 与工具说明引导 Agent 以 PTY 终端的方式工作：

- 执行命令优先用 `run_command`，多步操作拆成多次调用逐步执行；终端自动
  复用并保持工作目录与环境，无需把多步拼成一段脚本。`run_shell` 只用于
  执行已有脚本，或需要管道、重定向等 shell 语法的单条命令。
- 不用 `nohup` / `&` 后台启动。插件支持多终端并行：长期运行的服务用
  `terminal_open` 新开终端、`terminal_send` 在前台运行，用完 `\x03` 停止。

sidecar 在 `run_command` / `run_shell` 结果摘要末尾对这两类写法给出替代
做法（`service/advice.rs`，只提示不拦截，命令照常执行）。

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

Windows 默认使用系统 PowerShell；已设置 `SHELL` 时沿用其选择。就绪探针、参数引用及执行结果按实际 Shell 处理。PowerShell 使用无个人配置的交互会话，避免启动脚本及 PSReadLine 干扰命令边界；不修改系统执行策略。CMD 命令可通过 `run_command` 调用 `cmd.exe` 并传入 `/d`、`/c` 和脚本参数，显式创建的 CMD 终端也可连续执行。

CMD 在 Windows 沙箱内需要读取磁盘根的基本信息；对应的最小权限准备见沙箱 README，不等同于开放磁盘文件访问。
