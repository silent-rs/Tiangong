# Computer Use 异常分析（微信会话总结现场）

- 分支：`analyze/computer-use-exception`（自 main `6579a1f0` 检出）
- 现场来源：一次"读取微信 salvo 群聊并总结"的任务，Agent 自助尝试全部失败后的求助报告

## 1. 异常现场还原

| # | 通道 | 现象 | 结论 |
|---|---|---|---|
| 1 | 微信辅助功能树 | 微信 4.x 只有标题栏按钮，无聊天内容 | 外部应用不发布 AX 元素 |
| 2 | 终端截屏 | `NSScreen=0`，WindowServer 不可达 | seatbelt `deny mach-lookup`（按设计） |
| 3 | 键盘/鼠标模拟 | System Events 报 -10827 | Apple Events 依赖的 mach 服务被拒（按设计） |
| 4 | 安装 cliclick | Homebrew 目录属主异常 | 环境问题 + 写域在沙箱外 |
| 5 | Swift/JXA OCR | OCR 本身可用，缺截屏 | Vision 只读文件，不碰 WindowServer |

另有一次同根因的活体复现：在当前天工终端内执行 `git worktree add ../<dir>` 报
`could not create leading directories ... Operation not permitted`——写域被限制在
会话工作区内（见 §2.3）。

## 2. 执行架构与根因

### 2.1 分层隔离模型

`crates/tiangong-plugin-runtime/src/host_policy.rs`：

- 所有插件 sidecar 默认经 Launcher 进 OS 沙箱（`sandbox: true`）；
- **唯一例外**（host_policy.rs:77-82）：macOS + 官方签名的 `computer-use`
  不进沙箱——因为辅助功能 TCC 授权归属于天工 App，sidecar 套 Seatbelt 后
  `AXIsProcessTrusted` 无法继承宿主授权；
- `registry/connections.rs:284-345`：spawn 时按宿主权威策略表
  `.with_sandbox(host_policy.sandbox)` 组装，插件的 manifest 自声明不构成提权。

### 2.2 seatbelt 策略内容（crates/tiangong-sandbox/src/sandbox/seatbelt.rs）

- 写侧：`allow file-write*` 仅限工作区与显式授予根，随后
  `(deny file-write*)` 兜底（seatbelt.rs:125-138 / 174-183）；
- **mach 服务：`(deny mach-lookup)` 全局兜底**（seatbelt.rs:216），
  仅 trustd / SecurityServer 等随网络、凭据授权按精确服务名放行；
- 由此，沙箱内进程无法连接 WindowServer、AppleEvents、System Events 等
  任何 GUI 基础设施。

### 2.3 逐项根因映射

| 现象 | 根因链 | 代码位置 |
|---|---|---|
| 终端截屏 NSScreen=0 | terminal sidecar 沙箱内 `mach-lookup` 被拒 → AppKit 拿不到 WindowServer 端口 → 屏幕列表为空；`screencapture` 同理 | seatbelt.rs:216；host_policy.rs:82 |
| System Events -10827 | osascript 发 Apple Event 需要 `com.apple.systemevents` 等 mach 服务，全部被 `(deny mach-lookup)` 拒绝 | seatbelt.rs:216 |
| worktree 写仓库外失败 | `(deny file-write*)` 兜底，写域=会话工作区 | seatbelt.rs:138 |
| 微信只有菜单栏 | 微信 4.x 为 Qt 界面，聊天区不自发布 macOS AX 元素；computer-use 后端是纯 AX 实现，读到的是应用唯一暴露的原生 NSMenu（菜单栏） | backend/macos.rs:1-5, backend.rs:87-107 |
| OCR 可用 | Vision OCR 只需读图像文件（沙箱内 file-read 放行），不依赖 WindowServer | — |
| cliclick 安装失败 | 双重原因：Homebrew 前缀属主异常（环境）；且即便属主正常，Homebrew 前缀在写域之外 | seatbelt.rs:138 |

### 2.4 能力缺口（本分析的核心结论）

`plugins/tiangong-plugin-computer-use/protocol/src/ops.rs:14-19` 定义且仅定义六个操作：
`desktop_status / desktop_list_windows / desktop_snapshot / desktop_find /
desktop_action / desktop_wait`——**没有截屏，没有合成输入（CGEvent），没有 OCR**。

于是对微信这类"不发布 AX 的 Qt 应用"，Agent 的唯一视觉通道是终端截图，
而终端按安全设计永远不可达 WindowServer。五项失败里有三项（#2/#3/#5 的截屏半边）
都汇聚到同一个缺口：**computer-use 插件缺少视觉（截图+OCR）与坐标级输入能力**。

这不是回归 bug，是能力矩阵缺口；seatbelt 侧不应为了截图放开 mach-lookup。

## 3. 建议方案

| 优先级 | 方案 | 说明 |
|---|---|---|
| P0 | computer-use 新增 `desktop_screenshot` op | sidecar 在 macOS 已是宿主直启（host_policy.rs:77-82），TCC 的"负责任进程"归到天工 App，与 AX 授权同机制；实现走 ScreenCaptureKit（新系统）或 CGWindowListCreateImage，产物写入媒体目录并返回路径+尺寸 |
| P0 | 图片原生注入模型上下文（替代 OCR 主路径） | 见 RFC 0017（`docs/computer-use/rfc-0017-proactive-image-injection.md` v2）：统一以**宿主注入 User 消息**承载图片（全 provider 公分母），工具结果只带文本引用；OCR 降为纯文本模型兜底 |
| P1 | 新增坐标级合成输入（CGEvent 鼠标点击/键盘） | 用于无 AX 应用的兜底操作；依赖辅助功能 TCC（已授予）；动作仍应走现有 AccessContext 批准流（监督模式逐次确认） |
| P2 | 维持 terminal 沙箱现状 | 不为 GUI 访问放开 `mach-lookup`；在插件工具描述中写明"截屏/取色请走 desktop_screenshot，勿用终端" |
| 环境 | 修复 Homebrew 属主（`sudo chown -R $(whoami) /opt/homebrew`）或改用用户级安装 | 与代码无关；沙箱内也不应放开 Homebrew 写域 |

## 4. 验证清单（实施 P0/P1 时）

1. `desktop_screenshot`：未授屏幕录制时返回明确错误引导（而非空图）；
   多显示器/窗口裁剪正确；产物落在媒体目录且 Agent 可读取。
2. `desktop_ocr`：对微信聊天截图输出按行文本，中文识别可用。
3. 合成输入：监督模式下未经批准不落键；对微信输入框真实生效。
4. 回归：terminal 沙箱策略逐字不变（`seatbelt.rs` 测试快照不变）。
5. 全 workspace `cargo check` / `clippy -D warnings` / 相关测试通过。

## 5. 后续异常：正式版无法打开开发分支会话（2026-09-23）

- 现象：正式版天工打不开会话「电脑操作异常分析」（`03gwww09n2l9or2msgq34nuwp`，
  1681 条消息，JSON 本身完好）。
- 根因：分支为 RFC 0017 宿主注入图片新增 `MessagePhase::HostInjected`
  （序列化 `"hostinjected"`）；正式版（main）枚举只有
  `Normal/React/Summary/CompressedResume`，serde 反序列化遇到未知变体
  直接失败，导致**整个会话**加载失败。逐字段核对：会话其余结构
  （含 `ContentBlock::Image` / `StoredAsset`）均与 main 兼容，唯一拦路的
  是 2 条注入图片消息的 `phase`。这是「开发分支写入格式超前于正式版
  读取能力」的前向兼容缺陷。
- 修复：
  1. 数据抢救：2 条消息 `phase` 由 `hostinjected` 降级 `normal`
     （原文件备份 `*.json.bak.20260923222211`），正式版恢复可打开。
  2. 代码根治：`MessagePhase` 改手写 `Deserialize`，未知阶段值降级
     `Normal`（`#[serde(other)]` 要求置于末位变体、无法映射 Normal，
     故不用），补未知值降级测试；后续合入 main 后正式版即可读新格式。
- 关联改动（同日）：`desktop_screenshot` 应用定位由 pid 交集改为
  CGWindowList owner 名主路径（`window_frame_for_app`），解决微信 4.x
  等多进程应用主窗口挂 helper 进程时按名定位漏窗的问题；diagnose
  诊断程序增加 app_name 截图路径。
- 遗留风险：~~宿主注入截图产物当前落在系统临时目录~~（已修复：宿主对免沙箱
  sidecar 无条件注入 `TIANGONG_STORAGE_ROOT`，截图落统一媒体目录
  `~/.tiangong/media/screenshots`，tcp/stdio 两条 spawn 路径同口径）。
  owner 名定位路径的端到端行为需在具备 TCC 授权的宿主环境复核。

## 6. "最后一公里"证据链（2026-09-24，三级实证）

现象：注入图片在前端消息中正常展示，但 agent（模型）表现得不知道图片到达。

| # | 环节 | 判定 | 证据 |
|---|---|---|---|
| 1 | 上下文组装 | ✅ 完好 | 序列级回归测试 `host_injected_message_survives_full_request_assembly`：完整工具轮序列（user → assistant(tool_calls) → tool → 注入消息）经 `build_provider_messages` 后 provenance 文本与图片内容均保留、位于 Tool 结果之后，未被跳头/sanitize 配对整形丢弃 |
| 2 | HTTP 序列化 | ✅ 完好 | 抓包级回归测试 `openai_request_body_carries_host_injected_image`：MockServer 捕获的真实请求 body 含 `image_url` 原生图片内容与 provenance 文本 |
| 3 | 落库/前端 | ✅ 完好 | 会话文件 Image 块完整（data=None 设计内）；前端 assistant 侧展示正常 |

**结论：天工侧链路完好，断点在上游**——glm-5.3 非多模态，OpenAI 兼容网关
对文本模型的 `image_url` 静默丢弃（provenance 文本是否随之受损取决于网关
对混合内容消息的处理）。

设计取向（维持，不做降级）：provenance 让 agent 知道图片到达及其本地路径；
agent 需要看图时自行调用 `analyze_attachment`（走多模态路由）解读——
前提是模型配置了 Multimodal 路由槽位。曾实现"端点能力快照三态降级"，
因改动面过大（7 crate）且与 analyze-attachment 在册信号重复而回退。
