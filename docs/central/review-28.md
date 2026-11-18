# 天工中枢模式重构（feature/central-refactor）Review 总结与修复方案

> 文档性质：审查成果归档与后续实施方案，不是修复完成声明。
>
> 核查快照：`f70988d7a47b208cf24475d1e33d34c1869fad92`；分支 `feature/central-refactor`。
>
> 证据截止：2026-11-17，第 28 轮全量复审。本文编写时再次确认 HEAD 未变化；不重新执行测试，验证数字沿用第 28 轮真实结果。
>
> 编写权限：本次仅新增本文；不修改其他文件，不执行暂存、提交、推送。本文提出的代码、测试和其他文档修改全部属于后续待办。

## 阅读说明与目录

- **第一部分：Review 总结报告**：背景与快照、历史演进、裁决账本、实测记录、问题全景、合入判定。
- **第二部分：修复方案**：恢复链、日志链、状态链、UI 链、集成链五组闭环；实施依赖、验收清单与交接约定。
- **证据标签**：`已确认` 指源码、实际命令或已说明来源的用户实测；`路径推导` 指由代码组合得到、尚未完成完整应用端到端复现的结论；`建议` 指未实现的修法；`待确认` 指证据不足或裁决来源不完整。
- **位置口径**：所有当前代码位置均相对仓库根目录，行号基于上述 HEAD。历史提交说明中的行号可能已经失效，以本文当前位置为准。实施后应按符号重新定位，不应机械依赖旧行号。
- **历史边界**：任务沿用“28 轮 review”总编号。按第 1—15 轮 POC、第 16—28 轮重构计算，重构编号实际覆盖 **13 轮**；任务描述中的“重构 12 轮”与编号不一致。本文不人为合并或删除轮次。早期 POC 并非每轮都有可恢复的完整报告，缺失部分明确标注，不补造问题、裁决或验证结果。
- **结论优先级**：最新用户明确裁决高于旧报告、旧文档和模型推断。本文中“模型选择由中枢自主指定”的扩展解释仍标为中枢侧推断，不能冒充用户逐字确认的“不持久化”裁决。

---

# 第一部分：Review 总结报告

## 1. 概述

### 1.1 分支背景与目标

中枢模式的目标是建立统一控制面：中枢理解用户目标、协调工作、组织汇报；实际代码分析、命令执行和其他工作由 Worker 会话承担。POC 验证了派发、复用、控制、回报、工作视图与模式切换，再由 `feature/central-refactor` 完成契约、状态决策、日志持久化、宿主编排和前端特性边界的重构。

当前中枢工具声明见 `crates/central/central-plugin/src/tool_specs.rs:17-154`；中枢 Core 只装配 CentralPlugin，见 `crates/central/central-host/src/host/core_lifecycle.rs:96-113`。当前 Core 采用每个活跃 turn 一个 task 的模型；现行架构说明是 `docs/core-architecture.md:1-55` 与 `docs/agent-loop-refactor/design.md:1-43`，不应使用已删除的常驻 Driver/Inbox 方案解释现状。

### 1.2 当前快照与 main 分叉

| 项目 | 第 28 轮核实结果 |
|---|---|
| HEAD | `f70988d7a47b208cf24475d1e33d34c1869fad92` |
| 分支 | `feature/central-refactor` |
| 当前本地 main | `17dce4aec2a29f1f28de34269cce2297316efcf0` |
| 共同祖先 | `b33b9617c51de557365534e606704026885f158b` |
| 分支独有提交 | 146 |
| main 独有提交 | 1 |
| main 独有内容 | `17dce4ae`，DeepSeek 全家桶 API，PR #775 |
| 第 27 轮至第 28 轮增量 | `35b3ce11`、`f70988d7`；12 文件，+419 / −217 |
| 第 28 轮首尾工作区 | 干净 |
| 当前合入结论 | **未达到可合并 main** |

“分支独有 146 提交”不等于“从最新 main 快进领先 146 提交”。直接比较 `main..HEAD` 的文件差异，会把 main 独有能力显示成反向差异；这不证明重构主动删除了该能力。应结合共同祖先差异、双方独有提交和未来实际合并结果判断范围。

### 1.3 第 28 轮验证基线

以下是第 28 轮亲跑结果，**不是本次文档编写重新运行的结果**。

| 验证 | 结果 |
|---|---|
| 五包 `cargo test` | 288 通过，0 失败 |
| `cargo clippy --workspace --all-targets --tests -- -D warnings` | 通过；Cargo manifest 层仍有依赖声明提示，不应解释为所有输出完全无 warning |
| `cargo fmt --check` | 通过 |
| `yarn vitest run --configLoader runner` | 66 文件、593 用例通过 |
| 中枢前端子集 | 13 文件、100 用例通过 |
| `npx tsc --noEmit` | 通过 |
| `git diff --check` | 通过 |
| 首尾 HEAD / 工作区 | HEAD 未变，工作区干净 |

Rust 计数口径：central-host 11；central-plugin 39；central-state 31（8+9+2+12）；tiangong-central 38；tiangong-app 169（lib 123、bin 1、集成 45），合计 288。该数字不是整个 Cargo workspace 的测试总数，也不是覆盖率。本文不沿用未经本轮执行验证的“workspace 约 735 项”等估计作为通过证据。

### 1.4 证据使用限制

1. 第 28 轮独立探针位于临时目录，调用生产 crate；路由最小复核包含生产 `execution_router.rs`。它验证的是相关机制组合，不冒充完整桌面端到端实测。
2. 用户实测与源码快照不能自动等同：用户运行的构建未提供精确构建哈希时，只能说明现象与当前修法一致。
3. 未连接真实模型的测试，不证明模型一定选择正确工具、及时转发或正确理解问题。
4. Coding 工作流插件返回过“未知操作”；该工具故障不构成项目验证结果。仓库规则、Git 状态、源码和命令输出已独立读取。

## 2. 架构演进与现状评价

### 2.1 POC 第 1—15 轮：可追溯成果与记录缺口

本节按提交史恢复阶段成果，不将其伪装为已找回的每轮完整审查报告。

| 阶段/轮次 | 可追溯锚点 | 已确认成果与限制 |
|---|---|---|
| POC 起点，2026-11-06—11-08 | `25876835`、`91317932`、`b732aa9c`、`556b0c56`、`f867b906` | 域契约、desktop 模式入口、中枢独立 Core、首批只读白名单工具、指定模型派发的最小闭环。不能据此补造第 1—4 轮逐项问题。 |
| 第 5 轮，2026-11-10 | `6948e700..f029ee9e` | 修复派发登记、会话复用/事件流、徽章与可达性等 review 项；保留提交作为历史锚点，不在缺失原文时推演全部字母编号含义。 |
| 第 6—7 轮 | 历史报告不完整 | 可确认阶段内有复用兜底、默认上下文与重启恢复等连续修改；不足以重建两轮独立结论及测试数字。 |
| 第 8 轮，2026-11-10 | `2f6e5ab4` | 提交说明记录 P0 批量修复；原编号 N/M 项的完整审查上下文不全，不能把编号当当前问题定义。 |
| 第 9 轮 | 历史报告不完整 | 不补造逐项结论。 |
| 第 10 轮，2026-11-11 | `7c732e29`、`b016616b` | 失败态如实呈现、Workspace 确认、验收缺口与测试补齐；“150 达标”是当时提交说明，不是当前计数。 |
| 阶段检查，2026-11-11 | `63761d61` | 修复测试结构损坏；提交记录 Rust 147 通过、clippy 通过。553 前端属于同日 POC 验证口径，不能将 147 与后来 150 强行视为同一快照。 |
| 第 11 轮，2026-11-11 | `9381b9b4`、`a51e1807`、`f4ab3e72`、`e9e0294c` | 征询回退、徽章常驻、UTF-8 安全、Steer 确认、会话聚合、重启状态显示。 |
| 第 12 轮，2026-11-12 | `fab25a51`、`27a07eee`、`6edf6a94` | 守卫、a11y、硬断言与回调归属收口；当前测试仍须按当前生产身份重新审视。 |
| 第 13 轮，2026-11-12 | `88f0ed90` | 第二 turn 被一次性集合误杀、steer 覆盖归属等修复；证明状态驱动幂等优于永久“已处理”集合，但不证明后来恢复链路无回归。 |
| 第 14 轮，2026-11-12 | `f4d7829e`、`6405651c` | 决策/执行分离；曾丢失排队早退语义，随后修复。 |
| 第 15 轮，2026-11-12 | `9d5019c7`、`6031fb00` | 真两 turn 场景、反向验证、flush(None) 防消费后丢弃、执行段集成测试。历史“真两 turn”仍未覆盖当前同 owner id 的完整恢复路由。 |

POC 的主要教训：先验证使用闭环，再建立明确的工作身份、执行身份和恢复边界；测试数量、函数搬迁和注释“旧代码会红”都不能代替真实行为证据。

### 2.2 重构第 16—28 轮历史摘要

下表保留当轮快照结果，不把各轮不同计数口径相加。第 27 轮五包测试遇到宿主集成失败后停止，不能称已执行完全部包。

| 轮次 | 快照/日期锚点 | 关键审查成果与当轮验证摘要 |
|---|---|---|
| 16 | `f846008c`，2026-11-13 | 首次全量重审：ExecutionRouter、事件日志、原子写成立；clippy 门禁、投影、注入渲染、死代码、文档五类阻断。Rust 258、前端 578；clippy 未过。 |
| 17 | `26d02f97`，2026-11-13 | clippy 首次回绿；投影 payload 与锁修复；系统注入改渲染标记；删大量死代码；顺序、steer 归属和文档未闭合。Rust 243。 |
| 18 | `f33dd0cb`，2026-11-13 | UI 折叠、排序、徽章与 SVG 进步；后端必修基本未推进；强调行为断言。Rust 242+bin 1，前端 582。 |
| 19 | `a7263278`，提交日期 2026-11-14 | 顺序不变量、work-items RMW、写 API Result、轮转读回方向推进；模型切换失败仍成功回显等问题。Rust 248、前端 584。 |
| 20 | `c024ea13`，2026-11-15 | 21→10 工具合并；暴露复用 model 丢失、引导归属、重启 offset 初始化死分支。Rust 246、前端 586。 |
| 21 | `a64bebbe`，2026-11-16 | R6 后端 crate、R6b 前端特性抽离；补测发现真实队列/恢复缺陷；模型实际持久化与旧语义冲突。Rust 284、前端 586。 |
| 22 | `35be7b44`，2026-11-16 | R7 测试搬迁零丢失；摘要计数修复、重启 owner 优先；运行期 owner 与 offset 仍错；审查期间外部暂存。Rust 285、前端 583。 |
| 23 | `29d6cbb9`，2026-11-16 | flock+末行 offset 修复短事件跨进程；owner id 插件落盘；发现 4KB 长行、轮转、任务文本与终态吸收边界。Rust 287、前端 589。 |
| 24 | `c3f94026`，2026-11-16 | 中枢接入 InteractionPluginHost/InputOverlayPluginHost；仅挂载层测试，仍缺跨层内容验证。Rust 287、前端 589。 |
| 25 | `10c4210d`，2026-11-16 | 宿主跨会话可见性放宽；指出插件 session 仍为 central-hub 导致空卡；decision 仅改一条注入路径。Rust 287、前端 591。 |
| 26 | `7cd000e8`，2026-11-16 | `71ac9646` 修正沙箱 worker session，用户 request_user 实机链路通过；新增直答命令却丢答复、漏恢复执行路由。Rust 287、前端 594。重试不另算轮次。 |
| 27 | `0491e250`，2026-11-17 | 删除直通、改回中枢转发；无差别同会话互斥造成 4 项宿主集成失败；审查期间横幅折叠外部改动，前端结果不能归属于干净 HEAD。 |
| 28 | `f70988d7`，2026-11-17 | 门禁与工作区恢复；句中标记识别改善；全量复审确认 R28-1～8；修正若干旧误判。Rust 288、前端 593、中枢 100。 |

### 2.3 当前分层

```text
tiangong-central               契约、票据、事件、事件日志
central-state                  状态决策 + persistence 文件 IO
central-plugin                 10 工具、人设、派发/回报通道
central-host                   Tauri 绑定的中枢宿主编排
src-tauri                      应用装配、真实 Core 接线
frontend/src/features/central   中枢前端 API、切换、展示与测试
```

四个 crate 的依赖声明见 `crates/central/central-host/Cargo.toml:1-31`、`crates/central/central-plugin/Cargo.toml:1-23`、`crates/central/central-state/Cargo.toml:1-14`、`crates/central/tiangong-central/Cargo.toml:1-17`。

### 2.4 五维复评

| 维度 | 当前评价 | 可行动结论 |
|---|---|---|
| 分层与可测性 | 相对第 16 轮显著改善；R6/R6b 收敛边界，R7 搬迁未发现测试丢失。central-host 仍依赖 Tauri/CoreManager；central-state 含 IO，不能称整个 crate 纯逻辑。 | 不为“纯架构”扩大重构；优先补生产装配探针。`src-tauri/src/app.rs:326-394` 与 `:990-1058` 是不同装配，测试闭包绕过远端守卫/交接/快照。 |
| 工具契约 | spec/handler 十工具集合闭合；中枢不装 Worker 工具桥；但输出旧名、可选参数与 handler 默认行为有漂移。 | 修 `crates/central/central-plugin/src/tools_context.rs:27-39/89-94` 与 `tool_specs.rs:29-35`，让模型能遵循真实契约。 |
| 状态机 | 单函数规则有测试；work/session/execution 身份关系未统一。 | `tools_dispatch.rs:100-109` 复用 work_id，`dispatch.rs:481-518` 却用 id 是否改变辨别恢复，`event_log.rs:441-443` 又把 work_id 终态绝对吸收。必须一起设计。 |
| 持久化 | work-items 锁内 RMW、原子写、fsync、flock、envelope 有基础；归档恢复、长行、冷启动损坏处理仍有数据完整性缺口。 | 按日志链整体修复，不接受“只修一条尾读”替代恢复完整性。主要位置 `event_log.rs:75-326/342-445`。 |
| 汇报链路 | 成功汇总、失败立即、waiting 立即三类明确，system-note 渲染落地；B 答复出口未闭合。 | `reports.rs:24-147/237-275` 与 `src-tauri/src/commands.rs:793-811` 需同源；恢复必须证明答复送达及最终回报。 |

## 3. 用户裁决记录表

### 3.1 来源与命名规则

- “等待答复 A/B”曾用于**直通/中枢决策**，“模型 A/B”曾用于**持久化/仅本轮**，不可混用。
- 当前等待答复 **B 语义**：用户答复先进入中枢，由中枢判断；默认及时转发，信息不足可追问或调整任务；不是卡片直通 Worker。
- 下表日期优先注明实际提交日期；提交说明内自述的裁决日期另写，不把二者混为一谈。没有逐字会话原文的日期注明来源局限。

### 3.2 裁决与方向变更账本

| 编号 | 日期/证据 | 裁决或方向 | 当前效力与同步状态 |
|---|---|---|---|
| D01 | 2026-11-11，`a51e1807` | 后台徽章常驻：无任务也显示弱化空态。 | 有效；当前实现 `frontend/src/features/central/components/CentralWorkStatusBar.tsx:364-407`。 |
| D02 | 2026-11-11，`9381b9b4` | 征询功能暂缓，中枢 Core 不挂 RuntimeCorePlugin/request_user；Worker 交互改为自主处理/回报阻塞。 | 历史明确回退；当前 Core 隔离仍见 `core_lifecycle.rs:103-111`；后来 waiting 文本协议恢复，不能把 11-11 整段解释为永久禁止一切等待。 |
| D03 | 2026-11-11 起，`a847637d`、后续复用规则 | 延续任务优先复用 Worker 会话，保留上下文。 | 有效；`persona.rs:36-47`。复用会话不等于所有新工作都必须复用旧 work_id，见 R28-7。 |
| D04 | 2026-11-13 起，R3 与 `84aafd18`/`00cb9d61` | Workspace 授权由真实用户操作签发；模型只能请求，不能自证授权；可一次性或记住目录。 | 当前入口 `authorization.rs:25-85`、`tools_control.rs:78-115`；空默认工作区仍有 R28-8。 |
| D05 | 2026-11-15，`56451176` | 移除独立 switch_worker_model，换模型通过 dispatch_work 的 session_id+model。工具先从 22 减到 21。 | 有效；`tool_specs.rs:47-61`。不得把“移除模型工具”写成底层所有 switch_model 函数已删除。 |
| D06 | 2026-11-15，`c024ea13` | 21→10 工具统一入口：inspect_context、dispatch_work、control、work、goal 等。 | 有效；`tool_specs.rs:17-154`，spec/handler 锁 `central-plugin/tests/tools.rs:142-195`。send_to_worker 是历史名称，不是当前可调用工具。 |
| D07 | 第 20 轮“仅本轮”历史口径，2026-11-15 前后 | 模型覆盖曾按仅本轮、不持久改写解释。 | 历史语义；当前实现实际持久化，不能作为现行已实现事实。 |
| D08 | 2026-11-16，`defa2485` 及 `docs/central/plan.md:895` | 文档记“用户裁决 A：持久化会话默认”，persona/spec 同步。 | **来源存疑**：本审查会话未取得用户明确选择的原始答复，不得把仓库记载直接升级为已核实授权。当前代码确实持久化，见 `ensure.rs:309-320`，这是实现事实而不是裁决证明。 |
| D09 | 2026-11-16，`872ccb73` | 中枢输入区不再自建整套输入组件，直接复用 MessageInput。 | 有效；`CentralModeView.tsx:85-104`；模型、用量、发送能力沿用普通组件，中枢隐藏目录/特定槽。 |
| D10 | 2026-11-16，第 24 轮 `c3f94026` | 补 InteractionPluginHost/InputOverlayPluginHost 接线。 | 是实现/缺陷修复，不自动构成“恢复全部征询产品语义”的用户裁决。`CentralModeView.tsx:49-104`。 |
| D11 | 2026-11-16，第 25 轮 `10c4210d` | 中枢显示 Worker 征询容器，decision 默认展开。 | 当时沙箱会话未改，卡片内容为空的风险成立；随后 D12 修复。不能把这一提交写成已端到端成功。 |
| D12 | 2026-11-16，`71ac9646` | 沙箱上下文切到 pending Worker 会话，尊重插件按会话过滤。 | 当前有效；`InteractionPluginHost.tsx:213-218/263-270`；用户 choice 链实测与此一致。 |
| D13 | 2026-11-16，`7cd000e8` | waiting_user 卡片直答，新增 central_answer_waiting，机械直通 Worker。 | **已被推翻**；命令、API、注册已删除。旧文字仍在 `plan.md:945`，应标废弃，不能恢复实现。 |
| D14 | 2026-11-16—17，`ecdf8035`→`0491e250` | 直答卡曾升覆盖层，随后按新裁决撤除，改 B：答复回中枢，由中枢驱动。 | 最新方向有效；提交 0491e250 日期为 11-17，说明自述裁决发生于 11-16。`plan.md:941/943/945` 三段并存待清理。 |
| D15 | 第 28 轮任务中的明确更新；接收/审查日期 2026-11-17 | **中枢自主判断**：默认立即转发，答复不足以驱动时可先追问或调整任务。 | 当前核查基准，无需重复问用户；`persona.rs:65-67`“必须立即原样、不要反问”尚未同步。 |
| D16 | `0491e250`，提交 2026-11-17，说明记用户定谳 11-16 | 一 Worker 一任务，防同会话反复并发派发；曾采用在途一律排队。 | 产品边界有效，但无差别排队实施已被 `f70988d7` 修正；合法恢复/steer/重放需豁免。`central-state/src/lib.rs:270-313`。 |
| D17 | 2026-11-17，`35b3ce11` | 用户否定顶部横幅堆放，改为 Worker 浮层行内多步骤链。 | 最新展示方向有效；横幅已删。当前 `CentralWorkStatusBar.tsx:18-123`，`plan.md:941` 仍误记常驻横幅。 |
| D18 | 第 28 轮“这些都使用中枢进行自主判断”的扩展解释；2026-11-17 记录 | 中枢侧推断：模型也由中枢每次自主指定，而非依赖持久化默认。 | **推断，未经用户逐字确认“不持久化”**。每次显式选择与保存 Session.model_ref 可同时存在，不能据此擅改普通会话默认模型机制。 |
| D19 | `f70988d7` 提交说明，2026-11-17 | 用户不要求补写这次开发环境残留数据，后续正常即可。 | 不执行历史日志补记/清理；仍需修通用对账、标识与取消 UX。`recovery.rs:84-99` 的 Interrupted 迁移不是“历史事实已纠正”。 |

## 4. 实测记录与证据边界

### 4.1 三次征询/等待实测

由中枢侧提供结果；精确构建哈希未提供，日期按 2026-11-16—17 相关审查上下文记录。

| 实测 | 观察结果 | 能证明什么 / 不能证明什么 |
|---|---|---|
| WU-01：request_user choice | Worker 调用工具，用户卡片选 A，工具阻塞至响应，应答准确返回，Worker 按选择执行。 | 证明交互插件链可用；与 `InteractionPluginHost.tsx:213-218/269` 沙箱会话修复相符。不能证明 B 文本协议恢复。 |
| WU-02：kind=text | 中枢侧报告文本应答链路通过。 | 保留实测原述；本仓库交互 schema/解析列的是 input 等类型，不因运行层称“text”就宣称当前 schema 新增了 text 枚举，见 `plugins/tiangong-plugin-interaction/src/interaction.ts:103-129`。 |
| WU-03：文本 WaitingUser | Worker 发出独立标记并等待；用户没有提交原直答 UI，改为裁决 B。 | 证明进入 WaitingUser；**没有完成用户答复→中枢判断→派发→终态**，不能拿此样本证明恢复成功或失败。 |

### 4.2 中断/等待残留

用户报告：前次 WaitingUser 未释放，重启后出现中断残留，UI 仍称等待回复，而 Worker 实际空闲。`f70988d7` 另记录开发日志在终稿同秒写入 WaitingUser，并将原因归于全文标记嗅探。

事实与解释必须分开：

- `crates/central/central-host/src/host/recovery.rs:84-99` 会把在途投影转为 Interrupted；这不改写旧 WaitingUser 事件，也不证明用户看到的所有旧 UI 已同步。
- `frontend/src/features/central/components/CentralWorkStatusBar.tsx:378-405` 分别处理 WaitingUser 与 Interrupted，但 `:390-392` 的 activeCount 仍包含 Interrupted。
- 历史报告若真的提出尚未回答的问题，按原协议进入 WaitingUser 本身合理；只有引用、说明或已解决事项误命中才是分类错误。
- 新行首识别仍把独立行或代码块中的标记判为 WaitingUser；第 28 轮探针已确认，位置 `central-state/src/lib.rs:124-126`。
- 用户已表示不补写该开发数据；本文不要求偷偷修改历史事件来“做绿”。应提供可解释的中断、取消/确认、必要重派路径。

### 4.3 中枢通知误发循环

用户提供运行现象：多工具连续调用时，中枢约十次发送无意义 send_notification，预期转发动作没有成功。记录为**运行风险**，不是已定位的底层确定性 bug。

当前缓解是 `crates/central/central-plugin/src/persona.rs:81-85` 的“同事实至多通知一次、不得用通知代替 dispatch_work”。没有硬去重、限频或循环终止机制；不能声称根治。当前真实转发工具为 dispatch_work，send_to_worker 只是历史称谓。

### 4.4 第 28 轮独立探针结果

探针调用生产 crate；路由复核直接包含 `crates/central/central-host/src/host/execution_router.rs:60-197`。临时脚本和路径不是永久交付依赖，后续应把有价值样本迁入已有测试，不要求读者拥有 `/tmp` 文件。

| 编号 | 场景与结果 | 证据强度 |
|---|---|---|
| PR-01 | 小事件两进程顺序写，offset 为 1、2、3。 | 实际运行，短路径正常。 |
| PR-02 | 10KB payload 事件后新进程 append，offset 为 1、1。 | 实际运行，R28-3。 |
| PR-03 | 两进程写相同 event id，产生两条、offset 1、2。 | 实际运行，跨进程幂等不成立。 |
| PR-04 | 同 work_id：Started→Completed→Started，投影仍 Completed。 | 实际运行，R28-7。 |
| PR-05 | 合法事件日志用完整空白行补至 >100MB，触发轮转：offset=2，但当前路径不存在，`.1` 存在；新进程再写 offset=1。 | 实际运行，R28-4；不是依靠无效 JSON 填充得出的结论。 |
| PR-06 | 归档三条，当前空：严格读取 3，恢复使用的宽松读取 0。 | 实际运行，R28-5。 |
| PR-07 | 冷打开“好行→坏行→两条好行”，open 后只剩第一条好行。 | 实际运行，R28-6。 |
| PR-08 | merge 返回原任务，reply_retained=false；同 owner id 恢复后 steer=true；路由 active 仍旧 exec，新执行终态 callback=0。 | 生产状态函数+生产路由源码最小组合复核；非完整桌面 E2E，R28-1/2。 |
| PR-09 | 空默认工作区，对 `/tmp` 的 workspace_confirmed 返回 true。 | 实际运行，R28-8。 |
| PR-10 | 独立标记行、代码块标记仍为 WaitingUser，句中引用为 Finished。 | 实际运行，分类修复不完整。 |

## 5. 当前问题全景

### 5.1 八个 P1 合入阻断

P1 表示需在合入前解决的功能/数据完整性或授权边界问题，不表示八项均已由用户桌面实测触发。

| ID | 问题与根因 | 当前定位 | 证据与影响 |
|---|---|---|---|
| R28-1 | waiting 答复作为 request.task 到达后，被 owner_task 覆盖；保留登记任务与实际下发内容混为一谈。 | `crates/central/central-host/src/host/dispatch.rs:457-482/557-559` | PR-08；下发的可能是旧任务，Worker 收不到本次决定。 |
| R28-2 | merge 已置 Running，随后相同 owner id 被误判为正在执行的 steer，跳过新 register/attach。 | `crates/central/central-state/src/lib.rs:321-338`；`crates/central/central-host/src/host/dispatch.rs:503-518/567-586/613-636`；`execution_router.rs:117-162` | PR-08；旧 handler 已消费，新 execution 的终态无正确回调。 |
| R28-3 | tail 固定回读 4096 字节，长末行不含行首 `{`，解析失败退 0。 | `crates/central/tiangong-central/src/event_log.rs:299-326` | PR-02；重启追加 offset 重复。生产 Started payload 带任务全文，见 `dispatch.rs:656-662`。 |
| R28-4 | 先打开当前文件并锁 inode，再 rename，仍向旧 fd 写；新文件/新进程失去编号基准，锁也随 inode 分裂。 | `crates/central/tiangong-central/src/event_log.rs:246-293/450-479` | PR-05；触发轮转事件进入归档、当前路径缺失、跨进程编号回退。 |
| R28-5 | 恢复读取用 load_all_lenient，仅打开当前文件；严格读取才拼 `.1`。 | `crates/central/tiangong-central/src/event_log.rs:342-384`；`crates/central/central-host/src/host/central_events.rs:225-235` | PR-06；归档事实不进入恢复投影。 |
| R28-6 | open 先 repair_tail；遇中段坏行就停止累计，再 set_len，后面的有效事件在宽松读取前已被删除。 | `crates/central/tiangong-central/src/event_log.rs:115-122/147-205` | PR-07；数据丢失。现有 lenient 测试先 open 再写坏行，未覆盖冷启动，见 `tests/event_log.rs:213-246`。 |
| R28-7 | 插件空闲复用也沿用旧 work_id，投影对 Completed/Cancelled 的 work_id 绝对吸收，吞掉新 Started。 | `crates/central/central-plugin/src/tools_dispatch.rs:100-109`；`crates/central/tiangong-central/src/event_log.rs:418-445`；`crates/central/central-host/src/host/central_events.rs:243-297` | PR-04；重启后新执行可能显示旧终态；owner 优先还可能保留旧 metadata。 |
| R28-8 | 空默认路径参与前缀比较，目标绝对路径可被空前缀放行。 | `crates/central/central-state/src/persistence.rs:642-666` | PR-09；授权边界过宽，应 fail-closed。 |

### 5.2 P2 群：契约、分类、诚实性与验证

| ID | 问题/现状 | 位置与后续方向 |
|---|---|---|
| P2-01 | 行首识别只解决句中引用；新测试把真实多行样本改成同一行，不能称现场原文锁定。 | `crates/central/central-state/tests/transitions.rs:282-295` 对比 `src/lib.rs:124-126`；保真样本与独立 outcome。 |
| P2-02 | 自身互斥豁免不只限 Queued，Running/Paused 也被无条件跳过。 | `crates/central/central-state/src/lib.rs:292-300`；按显式派发意图限缩，不直接断言 Core 已并发。 |
| P2-03 | 引导仍整条替换 task/model/execution 记录，丢原任务展示。 | `crates/central/central-plugin/src/tools_dispatch.rs:144-163/217-219`、`central-state/src/persistence.rs:451-474`；区分 owner metadata 与 follow_up。 |
| P2-04 | 工具入通道就回“已派发”，宿主可能排队/拒绝。 | `crates/central/central-plugin/src/tools_dispatch.rs:179-183/238-255`；用已受理/排队/投递结果区分。 |
| P2-05 | 新建目标保存失败仍回“目标已登记”；目标/报告 RMW 锁外。 | `crates/central/central-plugin/src/tools_goal_report.rs:21-62/81-88/115-123`；失败结果与事务区间补齐。报告 save_report 已会显式报错，不应泛化为两者都假成功。 |
| P2-06 | decision 即时/延后注入分叉：一处用分类函数，一处固定 report。 | `crates/central/central-host/src/host/reports.rs:250-265`；`src-tauri/src/commands.rs:799-806`；共用结果类型与渲染选择。 |
| P2-07 | pending_question 未结构化，WorkStatus 只有原任务，用户/模型需要在消息流找问题。 | `crates/central/central-host/src/host/reports.rs:74-114`；`frontend/src/features/central/components/CentralWorkDrawer.tsx:211-215`；B 语义不需要恢复直答 UI，但需可关联问题。 |
| P2-08 | 现有恢复测试使用新 work-ans9，与生产 owner id 不同；另一测试手改 Running 再直接调终态。 | `src-tauri/tests/central_host_integration.rs:848-874/2058-2082`；补真实 dispatch+fire 终点测试。 |
| P2-09 | control 调用无断言，work-items 空集合断言恒真。 | `src-tauri/tests/central_host_integration.rs:412-429`；预置持久化记录并断言状态/结果。 |
| P2-10 | steer 测试对“排队没投递”也可通过，未明确验证消息到达。 | `src-tauri/tests/central_host_integration.rs:2483-2559`；补 deliver 次数、内容、pending 队列断言。 |
| P2-11 | 生产装配和测试装配不同，关键接线漏测。 | `src-tauri/src/app.rs:326-394/990-1058/1189-1210`；真实边界探针，不仅验证 mock closure。 |
| P2-12 | StepChain 只依 workId 拉一次；同 id 多 execution 的旧失败/取消污染新执行；曾排队在 state 变化后消失。 | `frontend/src/features/central/components/CentralWorkStatusBar.tsx:34-79`；按 execution/revision 刷新及隔离。 |
| P2-13 | 步骤链固定种类并集不是事件顺序；“等待当前”测试只检查始终存在的等待文字。 | `frontend/src/features/central/components/CentralWorkStatusBar.tsx:54-79`；`__tests__/centralWorkStatusBar.test.tsx:394-441`。 |
| P2-14 | 历史 docs 混当前态：横幅、覆盖层、直通命令三段仍在。 | `docs/central/plan.md:941-945`；旧版应标废弃，并有当前态唯一入口。 |
| P2-15 | 中断条目仍计入 activeCount，UI 缺确认/取消入口及真实状态解释。 | `frontend/src/features/central/components/CentralWorkStatusBar.tsx:388-405/663-684`；`recovery.rs:84-99`。 |
| P2-16 | 决策票据过渡：命令能签发，前端无实际使用；恢复按新 execution 消费，未闭合问题版本与答复对象。 | `crates/central/central-host/src/host/authorization.rs:96-132`；`dispatch.rs:467-479`；`frontend/src/features/central/api.ts:47-63`。不要把 UI helper 声明等同实际入口。 |
| P2-17 | 跨进程幂等未成立，cache 先登记后写入的失败语义也需整理。 | `crates/central/tiangong-central/src/event_log.rs:219-269/288-293`；PR-03；和日志组一起修。 |
| P2-18 | 最新裁决未同步：persona 仍禁止反问，模型仍强依赖旧默认/用户确认叙述。 | `crates/central/central-plugin/src/persona.rs:63-90`；`tool_specs.rs:55-60`；`ensure.rs:301-320`。 |

### 5.3 持续风险与工程债

以下不全是同等级合入阻断，实施者应依据真实影响分组，避免把风格项等同数据完整性问题。

| ID | 状态与位置 | 处理原则 |
|---|---|---|
| C01 | get_context 输出已删工具名：`crates/central/central-plugin/src/tools_context.rs:27/38/39`。 | 改成 goal、inspect_context、work 对应 action；加输出不含旧工具名断言。 |
| C02 | inspect_context path 契约冲突：`tool_specs.rs:29-35`、`tools_context.rs:89-94/315-319`。 | 选定“必填”或文档明确缺省，不两种口径并存。 |
| C03 | “重启中断数”来自 finished_at/cutoff，可能把当前未结束/排队项也计入：`tools_context.rs:18/40`、`persistence.rs:438-447`。 | 与运行 registry 和恢复事实对账；不把它泛称“所有 Interrupted 都变 Queued”。 |
| C04 | 损坏按空表、无备份：`persistence.rs:175-190/571-578/611-618`。 | 后续写入前保留可恢复证据，不覆盖用户数据。 |
| C05 | unbounded 派发、内存回报、待投递队列：`central-host/src/lib.rs:91-100`、`host/dispatch.rs:19/278-283`。 | 根据可观测负载决定限流/上限；先防无限增长与静默丢弃。 |
| C06 | ExecutionRouter HashMap 未回收，回调边界无专门 panic 隔离：`execution_router.rs:45-73/160-184`。 | 明确生命周期，避免清掉仍活跃执行；不泛化成整个 Core 都无 panic 防护。 |
| C07 | 测试 root.keep 六处：`src-tauri/tests/central_host_integration.rs:267/2398/2493/2571/2646/2760`。 | 让 guard 活到异步场景结束再自动清理；不是生产存储泄漏证据。 |
| C08 | Worker 全文注入与双份/重复摘要风险：`dispatch.rs:844-895`、`recovery.rs:111-124/129-166`、`reports.rs:250-275`。 | 当前不截断有完整性依据；未来可用结果文件+摘要引用，不能任意裁掉结论。 |
| C09 | append 冷加载块生产不可达：`event_log.rs:115-122/225-233`；对应假注释 `tests/event_log.rs:296-312`。 | 统一打开/初始化语义并修真实重启用例。 |
| C10 | 日志注册表用原 PathBuf、seen_ids 无上限：`event_log.rs:48-60/80-92/267-268`。 | canonical key、缓存生命周期和幂等真相源一起规划，勿仅做表面清空。 |
| C11 | repair_tail 仅持进程内锁，非 Unix FileLock 空实现：`event_log.rs:103-124/147-154/476-479`。 | 生命周期锁统一，并标明支持平台与语义。 |
| C12 | timeline 每次解析日志，步骤链一行一请求放大开销：`central-plugin/src/lib.rs:187-210`、`central-host/src/host/central_events.rs:165-193`、`CentralWorkStatusBar.tsx:36-49`。 | 在正确性后按测量引入 revision 缓存/批量读取。 |
| C13 | R7 测试便利 API 进入生产：`src-tauri/src/app.rs:987-1058`、`src-tauri/src/lib.rs:17-27`、`central-plugin/src/lib.rs:127-147`。 | 测试支持 gate/窄接口，避免外部随意改共享状态；公共方法本身不等于死代码。 |
| C14 | CentralModeView 只有静态接线断言，hint 隐藏时不清空，未共用 wiring：`CentralModeView.tsx:52-104`、`centralGreeting.test.tsx:24-39/87-95`。 | 补锁定/恢复行为；是否抽 hook 是维护建议，不强制为了过审抽象。 |
| C15 | fake timers 未在 afterEach 恢复：`centralCardsBar.test.tsx:53-56/82-100`。 | 无条件恢复定时器；此前偶发失败与异常耗时仅提示相关性，未证明所有 flaky 都由它导致。 |
| C16 | 宿主摘要计数位置已修但文本无专门断言：`recovery.rs:84-118`。 | 锁住迁移后的实际摘要 N。 |
| C17 | 失败审计 outcome 未统一结构化：`dispatch.rs:399-415`、`authorization.rs:61-72`。 | 日志 payload 与前端事件用同一枚举，避免仅依 note 解析。 |
| C18 | 模式启动偏好对账与串行切换仍分开：`centralConversation.ts:30-34/180-197`、`mode_coordinator.rs:94-121`。 | 验证迟到启动对账不覆盖用户新切换；低风险问题先做时序用例，不预设必须重写。 |

### 5.4 已纠偏、撤销或重新限定的旧判断

| 旧说法 | 当前正确口径与证据 |
|---|---|
| 未知工具 None 是 fail-open/静默成功 | **撤销。** `central-plugin/src/lib.rs:411` 返回 None 后，Core `crates/tiangong-core/src/react/tool_call.rs:16-41` 转明确失败。可改错误体验，但不是放开工作工具的证据。 |
| 模式切换完全没有 pending 回查 | **撤销绝对表述。** `crates/plugin/tiangong-plugin-runtime/src/bridge/mod.rs:707` 在订阅后 replay_pending；仍需测试跨挂载重放、幂等和并发选择。 |
| 中断记录完全没有清除路径 | **撤销。** `crates/central/central-host/src/host/dispatch.rs:164-224` 的 control(cancel) 能改状态、落 outcome、记事件；缺的是清晰 UI、对账与用户可复核结果。 |
| 第 25 轮空白卡 P1 仍然存在 | **已修。** `InteractionPluginHost.tsx:213-218/269` 改 worker session，用户 choice 链实测通过。 |
| request_user 链通过等于 waiting_user B 恢复通过 | **错误。** 两条链不同；`reports.rs:102-115` 的文本等待与工具阻塞桥不共用恢复代码。 |
| 行首标记必然是误报 | **错误。** 未答的真实问题应 WaitingUser；要保真区分引用/说明/真实待决。 |
| 第 27 轮四项红仍未修 | **已修直接回归。** 第 28 轮 45 集成全部通过；不代表 R28-1/2 已修。 |
| cargo 计数为 275（36+23+39+11+168） | **算术纠正为 277。** 当前真实五包 288，旧数字仍过时。 |
| main..HEAD 显示删除即分支主动删能力 | **错误。** main 与分支有 1/146 分叉，需按共同祖先和合并结果判断。 |
| 测试方法多/覆盖率高即可合入 | **错误。** 当前 288/593 绿与八项 P1 同时成立，说明覆盖边界不足；`central_host_integration.rs:848-874/2058-2082` 是典型。 |

## 6. 合入判定与验收口径

### 6.1 当前判定

**未达到可合并 main。**直接阻断是 R28-1～8 及其缺失的闭环证据，不是提交少了行号、组件没抽 hook 或文案细节。

门禁回绿、工作区稳定、架构边界改善应明确肯定；同时不得把“测试绿”写成“答复和恢复已闭合”。优先解决身份契约与事实源完整性，再收口文档、UI 与主分支集成。

### 6.2 20 条口径历史与第 28 轮状态

最初口径由多轮 review 逐步增加：前 13 项覆盖门禁、假绿、死代码、持久化、渲染、票据、文档、实机、提交定位、不变量、UI、读写与裁决；第 21 轮加入机制命名，第 22 轮加入干净快照，第 23 轮长事件，第 24—27 轮逐步加入征询闭环、跨层终点与全量验证。编号是审查沿革，不是用户逐条制定的二十条硬制度。

| # | 口径 | 第 28 轮状态 |
|---|---|---|
| 1 | 门禁全绿 | 通过：288/593，clippy/fmt/tsc 通过 |
| 2 | 假绿零容忍 | 未达：空断言、弱步骤断言、身份不符样本 |
| 3 | 死代码与无效分支 | 未达：append 初始化块；测试 API 暴露单列工程债，不将所有 pub 都称死代码 |
| 4 | 投影/RMW 不变量 | 部分：work-items RMW 成立，投影/日志缺口仍在 |
| 5 | 系统注入渲染诚实 | 通过：system-note 机制 |
| 6 | 票据链路 | 部分：Workspace 闭环，Decision 过渡态 |
| 7 | 文档与实现一致 | 未达 |
| 8 | 实机十步 | 部分：三次实测有证据，B 恢复未完成 |
| 9 | 提交可定位 | 部分：最新提交有部分文件:行号 |
| 10 | 共享不变量测试 | 部分：组合/路由终点不足 |
| 11 | UI 行为断言 | 部分：有形态测试，刷新/current 断言不足 |
| 12 | 写入读回成对 | 部分：归档恢复读遗漏 |
| 13 | 裁决闭环 | 部分：最新口径明确但 persona/模型/文档未同步 |
| 14 | 测试命名、样本与机制一致 | 未达 |
| 15 | 干净固定快照 | 通过 |
| 16 | 长事件不变量 | 未达 |
| 17 | 征询闭环 | 部分：插件链实测通过，B 文本链未闭合 |
| 18 | 跨层端到端 | 未达 |
| 19 | 恢复链必须证明终点 | 未达 |
| 20 | 全量验证证据 | 部分：最新说明覆盖宿主等，本轮五包已核实，但提交说明不完全按五包同一口径 |

汇总：3 通过、10 部分、7 未达。该分布不是百分比完成度，也不应靠继续增加口径数量衡量质量。此前“CI 红时不得提交”等说法应收敛为：**不能把红灯快照声明为可合并；提交可以保留有说明的开发进度，但必须诚实标注失败和缺口。**

### 6.3 五组闭环证据，取代继续追加第 21 条

1. **恢复链**：WaitingUser → 用户在中枢答复 → 中枢判断/必要追问 → Worker 收到真实答复 → 新 execution → Completed/Failed → 回报。
2. **日志链**：短行、长行、幂等、并发、轮转、重启、中段损坏、归档恢复在同一事实源规则下成立。
3. **状态链**：首轮完成、空闲复用、取消后新工作、排队重放、重启中断、残留确认/取消都可解释且一致。
4. **UI 链**：worker、work、execution、当前步骤与真实事件一致，内容可见可操作，状态变化后更新。
5. **集成链**：固定干净 HEAD，真实 main 合并结果、五包测试、前端检查与必要实机记录一致。

---

# 第二部分：修复方案

## 7. 实施约束与总依赖

### 7.1 不变量先于代码

后续实施必须同时遵守：

- 中枢不是机械代理：默认及时转发，可根据目标/授权判断、追问或调整任务；不恢复已撤销的卡片直通命令。
- 会话连续性、工作生命周期、执行归属是三个概念；不能仅凭 work_id 相等推断是否新增 turn。
- 一次真实恢复应分配并注册新 execution；steer 并入保持当前活跃 execution。记录层可以记引导事实，但不能把运行执行归属替换成不存在的执行。
- 事实源损坏时不能先销毁可恢复信息，再宣称宽松恢复；恢复读取范围必须与写入/轮转范围一致。
- 授权未知不等于默认允许；新模型选择不能越过用户已规定的风险/付费边界。
- 保留用户已有文件和历史数据，不为过审 reset/clean，不偷偷补写开发日志；任何历史修复需独立授权。

### 7.2 依赖图

```text
裁决与身份说明（G0）
  ├─ 恢复意图与下发（G1）──→ 路由终点验证（G2）
  └─ work/execution 生命周期（S1）──→ 投影 + UI 按执行隔离（U1）

日志生命周期锁/编号/幂等（L1/L2/L5）
  └─ 归档与损坏保全（L3/L4）──→ 状态恢复与残留对账（S2）

授权 fail-closed（S3，可独立并行）
裁决/回报元数据（G3/G4）──→ UI 问题与步骤呈现（U1/U2）

全部功能闭环 ──→ 文档对账 + main 合并结果验证（I1/I2）
```

可以并行调查和不同文件的独立修复；不应让多个写入者同时修改 `dispatch.rs`、`event_log.rs` 或共享状态结构。日志组必须有一个明确整合者。

## 8. 恢复链：真实答复、显式意图与终态

### G0：裁决与派发意图说明

**【问题】** `crates/central/central-plugin/src/persona.rs:65-67` 仍要求“必须立即原样、不要反问”；`dispatch.rs:504-518` 用 work_id 相等和当前 Running 猜测动作。两者都不符合最新 B 语义及状态边界。

**【修法】**

1. persona 改为“默认及时把有效答复发给对应 Worker；若不足以驱动工作，可先澄清或调整指令；需改变目标/风险授权时重新确认”。
2. 在 `crates/central/central-host/src/host/dispatch.rs:333-345/457-529` 的规划结果中表示显式意图，最小可用枚举如 `NewExecution`、`ResumeWaiting`、`ResumePaused`、`SteerRunning`、`ReplayQueued`。名称可按项目风格调整，不新增无用框架。
3. 判断意图时保留转移前状态，不先把 WaitingUser 改 Running 再反向猜测；队列豁免按意图和真实执行占用校验。
4. 当前 `dispatch_work` 仍为统一入口，不重新引入名为 send_to_worker 的公开工具。

**【验收标准】**

- 表驱动测试覆盖各状态×意图×复用×自身/其他条目的组合。
- WaitingUser 恢复、Paused 恢复、运行中 steer、自身 Queued 重放明确放行；真正新执行的并发受控。
- 不能只传一个任意“同 work_id”就绕过所有互斥；明确测试对应限制。
- persona 测试可核对最新规则，但不能用文本断言代替 G1/G2。

**【依赖关系】** 为 G1/G2/S1 的先行设计；先锁最小语义再编码，避免互相覆盖。

### G1：R28-1，登记原任务与实际下发答复分离

**【问题】** `dispatch.rs:481-482` 用 owner_task 覆盖请求 task，`:558` 实际下发只用覆盖后的文本。保留原任务登记不应导致答复丢失。

**【修法】**

- 保留 owner work_id、owner 原始目标和本次 follow_up/answer 三个值。
- `ResumeWaiting` 的投递内容应明确含原目标必要上下文与本次答复，例如“继续既定工作；本次用户/中枢决定：……”，或者专门将原文作为新的消息内容传给已有会话。
- 在 `dispatch.rs:557-559/574-578/625-629` 生成和投递真正的 outgoing content；不要把 `WorkStatus.task` 强行替换成只有答复的一行。
- `tools_dispatch.rs:144-163/217-219` 的记录更新与宿主 merge 一起调整：owner 的目标展示不被引导覆盖；新的执行/指令记录仍可追溯。
- 答复/问题元数据若新加字段，旧文件使用 serde default 兼容读取；不要擅自回写所有历史文件。

**【验收标准】**

- 宿主集成测试使用生产相同 owner id 和新的 execution_id；记录 `deliver_prepared` 接到的 ContentBlock，断言包含“选方案 B”等真实答复，而不是只检查 registry.task。
- 原任务与 follow_up 都可查询，且状态/日志/回显不相互矛盾。
- 另测答复为空、带多行/Unicode、前次终态、目标不存在；不触发无意义投递。

**【依赖关系】** 依赖 G0；与 G2 一并验收，单独“含答复”不代表恢复可收尾。

### G2：R28-2，恢复新 execution 必须 register/attach 并收回终态

**【问题】** `central-state/src/lib.rs:329-333` merge 先转 Running/新 execution，`dispatch.rs:504-518` 误判 steer；`:567-586` 跳过 register/attach。最小探针已得到 resumed callback=0。

**【修法】**

1. `ResumeWaiting`/`ResumePaused` 是新 execution，始终走新 register；只有真实 `SteerRunning` 不替换活跃执行。
2. 投递成功后 attach；继续保留路由器 fire-before-attach 缓冲语义。不要简单恢复为“先挂回调再投递”。
3. ensure/deliver 失败需取消新路由，并撤销/标明未成功恢复的状态；不能仅路由 cancel 后留下 Running。
4. 登记/日志成功转移不能在最终回调后被迟到的通用 Running upsert 覆盖；检查 `dispatch.rs:636/701-719` 与立即完成时序。
5. 原执行迟到回调要让位新执行，但不能影响原本的 steer 归属。

**【验收标准】**

- 在 `src-tauri/tests/central_host_integration.rs:848-874` 的恢复测试基础上，将 work_id 改成生产 owner id，保留旧 exec、新建恢复 exec。
- 走 `handle_central_dispatch`，捕获答复投递，再经 `execution_router.fire` 触发真实回调；断言 WaitingUser→Running→ReportQueued/Completed 或 Failed，且实际回报入队/送达。
- 不能像 `:2058-2082` 那样手工改 Running 后直接调用终态来冒充端到端恢复。
- 覆盖 live Core、无 Core 重建、投递失败、fire 先于 attach、重复 fire、旧 exec 迟到等边界。
- 与 G1 联合断言：真实答复被送达 **且** 终态回到正确 work/execution。

**【依赖关系】** G0/G1；回调内容和持久化结果随后与 S1/L3 联测。

### G3：decision 双路径与 pending_question 元数据

**【问题】** `reports.rs:258-265` 与 `src-tauri/src/commands.rs:799-806` 对 kind 不一致；WaitingUser 的状态、问题正文和任务标识没有一个结构化结果对象。

**【修法】**

- 从已知 outcome 构造最小回报封装：work_id、session_id、execution_id、outcome、question/reference、正文和 render kind；避免每条路径反复全文 contains。
- 由同一函数选择 `decision/report`，供即时注入和延后用户消息 flush 共用；历史纯字符串保持兼容。
- 等待详情展示 pending_question/问题引用；B 语义下仍由中枢对话接收用户答复，不恢复卡片直通 Worker。
- 多工作同时等待时，消息或上下文带明确对象；有歧义由中枢追问，不凭最后一条记录猜 work_id。

**【验收标准】**

- 即时、Core 缺失暂存后 flush、投递失败放回队列、历史 report fallback 四条路径同一问题标签一致。
- outcome=WaitingUser 才对应当前待决状态；引用旧问题不凭渲染 kind 改真实状态。
- pending_question 属于正确 execution；完成/取消/恢复后清理，不能复用上轮问题。

**【依赖关系】** G0/S1；可在 G1/G2 之后独立实现，不阻塞先修最小答复与路由。

### G4：最新模型选择与通知规则同步

**【问题】** `persona.rs:86-90` 仍逐次征得用户同意且依赖持久默认，`tool_specs.rs:55-60` model 可选；徽章 `central-state/src/lib.rs:360-364` 与实际应用模型矛盾。通知新增规则仅为提示词。

**【修法】**

1. 将已明确的“waiting 答复中枢自主判断”直接同步；模型扩展解释在 docs 中明确标为“中枢侧推断”，不要继续写无出处的“用户已明确选择 A”。
2. 在现有授权、费用、用户指定范围内，中枢派发时明确选择并携带 model；如未携带，必须定义它是“显式跟随某策略”还是非法缺省，不能 spec 说路由默认、实际沿用旧 Session。
3. 不直接删除通用 `crates/tiangong-core-manager/src/core_manager/ensure.rs:309-320` 的持久化。每次选模型与保留 Session.model_ref 用于历史回显/重建不冲突；是否需要中枢专属 turn override，基于最终产品语义和普通会话兼容性决定。
4. 用 actual/applied 模型或“正在执行，当前轮未切换”的真实回执替代 `registry_entry_after_dispatch` 的“已忽略 X”和先验“切换成功”。
5. 通知同事实一次可先提示词约束；若真实循环持续，再按 session/work/execution+事实类型设计有限去重窗口和可观测计数，不能无界存储或误抑制不同紧急事实。

**【验收标准】**

- 显式 model 的新会话、空闲复用、运行中 steer、Core 重建四条路径，投递参数/实际端点/Session 回显/用户文案一致。
- 未指定 model 的行为有单一契约；普通会话原有选择不回归。
- 如持久化继续保留，断言是实现与回显事实，不把测试命名成“用户已批准”。
- 通知循环实测另留记录；文本要求通过不等于机制去重通过。

**【依赖关系】** G0，涉及模型执行测试时依赖 G2；模型推断不阻塞其他确定性缺陷的修复。

## 9. 日志链：生命周期锁、编号、幂等与保全

本组涉及同一持久化协议，**不可拆成几个互不相干的小补丁估工**。下面分项是验收维度，不是可随意并发修改同文件的任务。

### L1：R28-3，任意长事件后的 offset 连续性

**【问题】** `event_log.rs:299-326` 仅回读 4096 字节，找不到完整最后一行就返回 0；PR-02 重启得到 1、1。

**【修法】**

- 末行定位不依赖固定最大事件长度：可逐步扩窗找到完整边界，或失败时在同一锁内完整扫描。
- 逐行解析应使用可继续尝试的逻辑（例如 find_map）；但仅把 find 改成 find_map 不能解决长行超过窗口的根因。
- 不应对未知/无法解析的事实源默默归 0；区分真正空日志、已归档历史、损坏待恢复。
- offset 基准取当前保留事实的最大有效 offset，不仅依赖“物理最后一行一定最大”的旧假设；历史重复/恢复数据另保留诊断。

**【验收标准】**

- 测例“≥8KB、64KB、Unicode 多行 payload（JSON 仍单行）事件→结束进程→新进程 append”均严格递增；至少包含 offset==2 的最小断言。
- 短行/无尾换行/空文件/只剩归档分别覆盖。
- 日志解析失败不产生误称成功的新编号。

**【依赖关系】** 与 L2/L5 共用锁与初始化，不得在锁外扫描后再锁内分配。

### L2：R28-4，轮转全生命周期正确

**【问题】** `event_log.rs:246-293` 先 open 当前 inode，再 rename，用旧 fd 写；当前路径可能不存在。仅锁旧 inode 无法保护新文件，跨进程编号回退。

**【修法】**

1. 使用跨 rename 不变化的锁对象（例如同目录专用锁文件），覆盖“读取代际/编号→判定轮转→rename→重开当前→追加→fsync→提交编号”全区间；明确 Unix/Windows 实现，不保留静默无锁的假等价。
2. 轮转后必须打开正确当前文件，不能继续向归档 fd 追加；rename/open/fsync 失败向上返回，不用 `let _ = rename` 后宣称归档成功。
3. 编号从归档+当前事实或可靠 checkpoint 恢复；若引入 checkpoint，写入顺序与崩溃恢复规则必须说明并测试。
4. 当前仅一级 `.1` 的保留策略需明确：覆盖归档意味着有限历史，不可宣传无限事实保留。需要多代/压缩时另定范围，先修当前语义。

**【验收标准】**

- 合法事件+完整空白行构造 >100MB 场景；触发轮转的事件必须位于约定当前文件，且当前路径存在。
- 结束进程后新写 offset 连续；归档+当前 id 不重复、不漏触发事件。
- 多进程在阈值边界竞争，不能锁住不同 inode 各自分配同编号。
- 注入 rename/open/write 失败与轮转后崩溃，保留可恢复旧事实，不误报成功。

**【依赖关系】** L1/L3/L5；L2 未定锁生命周期前不单独实现跨进程幂等缓存。

### L3：R28-5，恢复读取与普通读取覆盖相同历史

**【问题】** `load_all_inner` 拼 `.1`+当前，`load_all_lenient` 只读当前；`central_events.rs:231` 恢复走后一条，归档三条时返回 0。

**【修法】**

- 抽取统一“保留日志段枚举与顺序”逻辑，严格/宽松解析共享文件范围、版本与去重策略。
- 宽松读返回幸存事件与每段损坏计数；不要因为当前空就跳过归档或回退成另一份过时快照。
- 归档、当前缺失/空/损坏组合都定义恢复行为。offset 重复或非单调不能悄悄假定“天然递增”。

**【验收标准】**

- 同一归档+当前 fixture：严格读与宽松读在全部合法时事件集合与顺序相同。
- 只有归档、当前空、当前尚未创建的轮转瞬间都不丢旧事实。
- `rebuild_registry_from_log` 与 `recover_central_work_state` 必须从归档正确重建，不仅测试 EventLog 工具函数。

**【依赖关系】** L2 定义日志段生命周期；S2 恢复验收依赖 L3。

### L4：R28-6，冷启动不得截掉有效后缀

**【问题】** `open→repair_tail` 在宽松读之前运行；`event_log.rs:176-203` 把中段坏行后的全部内容删掉。现有测试先 open 再写坏行命中缓存门槛，避开冷启动。

**【修法】**

- 分开“末尾写一半”与“中段损坏”：只有确认是未完成尾部时才可按协议截断；中段坏行应保留原文件/备份，宽松扫描后续完整行。
- 修复也持 L2 生命周期锁，避免另一进程正在追加时误判尾部。
- 不静默破坏 UTF-8/坏字节后的幸存数据；读取按字节边界和完整行定义恢复，并返回诊断。
- 若需要生成修复副本，明确原文件保留、替换原子性和失败行为；本次用户不补历史数据的裁决仍然有效。

**【验收标准】**

- 独立进程冷打开“好行→坏行→好行→好行”，后两条在恢复结果中可见，原损坏证据仍可取得。
- 真 torn tail 修复仍通过；完整坏行、非法 UTF-8、归档坏行分别测试。
- 测试不允许预先 open 或调用 force helper 来绕过真实冷启动。

**【依赖关系】** L2/L3；不能用 L3 的宽松读抵消 L4 已发生的删除。

### L5：跨进程幂等与初始化

**【问题】** `open` 置 loaded=true，`append:225-233` 冷重建块生产不可达；seen_ids 不包含别的进程/重启事实，PR-03 同 id 重复。

**【修法】**

- 在生命周期锁内初始化/对账已有 id 与 offset；幂等判断、分配编号和实际写入处于同一事务边界。
- 缓存只是优化，不能当跨进程事实源。记录文件代际/长度/revision 判断缓存失效；容量上限不能破坏重复 id 查询语义。
- 成功写入后再提交内存缓存，失败不应把未持久化事件当已存在；若允许 offset 空洞应明确，但不能有重复和假成功。
- 日志 key 规范化处理符号链接/路径别名；不因此绕过稳定文件锁。
- 修 `tests/event_log.rs:296-312` 的不可达分支“已覆盖”注释。

**【验收标准】**

- 顺序重启及并发进程写同 event id，最终只有一条事实，返回同一既有 offset。
- 与长事件、轮转、归档、失败重试组合；不能只测共享 Arc 的同进程实例。
- 修改前测试应真实失败，修改后通过；留下命令和断言，不把“helper 强制 loaded=false”当真实重启。

**【依赖关系】** L1—L4；日志组整体审查与验收。

## 10. 状态链：工作生命周期、残留对账与授权

### S1：R28-7，work/session/execution 生命周期统一

**【问题】** `tools_dispatch.rs:100-109` 所有复用都沿用 work_id，`event_log.rs:441-443` 却把 Completed/Cancelled 当该 id 绝对终态；重建 `central_events.rs:243-297` 还有 owner 优先，可能拿到过时任务/模型。

**【修法】**

- 优先明确最小规则：复用同一正在进行或 WaitingUser/Paused 的工作保留 owner；上一工作真正终态后复用会话做新工作，分配新 work_id 和 execution_id。
- 若产品要同 work 重试，明确哪些终态可重试及 execution 边界；不能单纯删除终态吸收导致迟到 progress 复活已取消工作。
- 投影按执行代际处理 Started/终态和迟到事件；“completed/cancelled 吸收”在同 execution 内保留。
- 重建 metadata 取当前工作/执行的 Started，不把所有 reused 都当应让位的孤儿。历史 steer-* 兼容单独定义，不能覆盖新生命周期语义。
- 在 `dispatch.rs:650-675` 对齐事件 work/execution 与登记身份；steer 不应替换 owner 的活跃执行归属。

**【验收标准】**

- 原完成→同会话新任务→重启显示新任务正确在途/中断，不显示旧 Completed。
- 取消后新任务不会被旧取消吸收；旧 execution 迟到事件不污染新工作。
- 等待/暂停恢复、运行中引导仍保持 owner，不出现幽灵条目。
- 比较 registry、work-items、事件重建的 work/task/model/execution 一致性。

**【依赖关系】** G0/G2；日志读取正确性 L3/L4 是重启验收前提。

### S2：中断残留、征询分类与清除 UX

**【问题】** `recovery.rs:84-99` 仅内存迁移，`CentralWorkStatusBar.tsx:390-392` 又把 Interrupted 算 active；真实征询与历史引用依赖自由文本扫描，旧事件不被新分类修正。

**【修法】**

1. 新回报优先携带结构化 outcome；历史文本 fallback 用清晰协议边界，并保留真实多行样本。不能把代码块/引用片段中的标记当当前请求，也不能吞掉真实独立标记问题。
2. UI 明确区分 Worker 实际运行、业务等待用户、重启中断和已确认历史；“worker 空闲”不自动表示业务问题已解决。
3. 提供“取消该工作/确认中断/复用重派”等经中枢或显式动作的路径，优先复用现有 control(cancel)，不要新建未经验证的直通答复通道。
4. 有历史修复需求时记录确认事件/迁移版本，不默改原始事实；本次 dev 旧数据按用户选择不补记。
5. activeCount 分清待处理与正在运行；如果产品确需显示“待处理数量”，直接改名称与可访问文案，不伪称执行数。
6. 恢复摘要只基于本次新事实/待处理事实，避免每次重启反复宣称“离开期间新增失败”。

**【验收标准】**

- 真问题、独立行历史引用、代码块引用、已解决记录、句中引用五类保真 fixture。
- 冷重启真实 WaitingUser→Interrupted，UI 标签、数量、详情说明一致；取消后不在下一次重启复活。
- recovery 摘要断言迁移后 N；不再用“计数变量位置正确”代替文本验证。
- 用户未答不能默认同意；运行空闲不能默认任务完成。

**【依赖关系】** G3、S1、L3/L4；UX 不能掩盖路由 R28-2。

### S3：R28-8，空默认工作区 fail-closed

**【问题】** `persistence.rs:651-657` 空 canonical_default 成为空前缀，对绝对路径放行。`dispatch.rs:376-387` 用此函数做最终授权判定。

**【修法】**

- 默认路径为空、无效或不能确认为已选择根时，不走默认子树放行；仅已有明确授权或有效一次性票据可放行。
- target 为空与“使用合法默认目录”分层处理；宿主应先解析最终 cwd，再对非空且存在的目录授权。
- canonicalize 失败的退回行为明确；不能因失败回退宽松匹配而扩大授权。
- 不改变用户已授权目录、子树边界和 task fingerprint 的现有契约。

**【验收标准】**

- 空默认+未授权绝对 target 返回 false；空 target 无有效默认不能派发。
- 默认目录自身/合法子目录通过；兄弟前缀 `/a-b`、符号链接逃逸、缺失路径拒绝或要求授权。
- 一次性票据同任务通过、换 task 失败、消费后再用失败；日志记录真实结果。

**【依赖关系】** 可独立实施；应优先修复，不等待其他设计讨论。

### S4：持久化 RMW 与回执诚实性

**【问题】** goals/reports 锁外 load→mutate→save；`tools_goal_report.rs:55-62` 保存失败仍报登记成功；插件入通道就宣称执行。

**【修法】**

- 为 goals/reports 复用最小锁内 RMW 模式，别在已有锁内再次调用会加同锁的 save 导致死锁。
- 目标写失败返回错误并保持可重试；报告已有失败返回应保留。
- 派发结果先说“已受理/等待宿主处理”，或引入带 request id 的真实回执，区分 queued/delivered/rejected；不因为队列消费者接收成功就声称已执行。
- 损坏数据写前保留备份/隔离证据，避免按空表后覆盖全部历史。

**【验收标准】**

- 两个并发目标/报告写入都保留；保存失败时工具结果非成功。
- 无权限/非法 cwd/通道关闭/入队/实际成功各返回与事实相符。
- 现有 work-items 并发 RMW 测试维持；不能以全局格式化扩大改动范围。

**【依赖关系】** 可与其他组分文件并行；接回执时注意和 G0 请求身份一致。

## 11. UI 链：步骤刷新、执行隔离与交互可恢复

### U1：StepChain 以当前 execution/revision 为基准

**【问题】** `CentralWorkStatusBar.tsx:36-49` 只按 workId 拉一次；`:51-72` 把整个历史种类并集当当前状态，旧 failed/cancelled 会压掉新运行。排队只看 state，执行后排队步骤消失。

**【修法】**

- API 时间线或状态响应带 execution_id 与 revision/last_offset；复用已有字段前确认前端类型是否遗漏。
- effect 对 work_id、execution/revision 的正确变化刷新，或在集中事件订阅中失效缓存，避免每一行独立重复全量解析。
- 只用当前 execution 的相关事件决定当前步骤；展示全部历史时分段，而不是让旧失败覆盖新执行。
- “曾排队”来自真实排队事实，不是当前 state==queued；若目前事件只用 progress+note，应先给 payload 补结构化 phase，不再依中文 note 猜。
- 区分“业务步骤”与“执行生命周期阶段”；当前固定派发/排队/执行/等待/完成链不等于 Worker 自己的计划步骤，不要过度宣传。

**【验收标准】**

- 同一个 React 实例从 running→waiting→running→completed，时间线更新、当前步骤跟随正确，不靠卸载重挂。
- 旧失败执行+新 running，不显示当前失败；取消后新执行同理。
- 先 queued 后 running，历史排队仍可见；重放后阶段顺序与事件一致。
- 测试精确断言 current/aria-current/语义状态，不仅 `textContent` 含“等待”或任意元素 class 含 animate-pulse。
- 请求失败显示可解释降级，不能伪造已完成阶段；评估多工作查询次数。

**【依赖关系】** S1/L3/G3 的身份与事件结构；可先补失败用例，再确定最小 API 改动。

### U2：征询接线与恢复输入

**【问题】** `CentralModeView.tsx:52-104` 的宿主可见性到输入禁用/恢复仍仅静态 mock 覆盖；`centralGreeting.test.tsx:24-39/87-95` 的宿主从不触发回调。交互插件链虽实测通过，不代表所有重挂/关闭边界有锁。

**【修法】**

- mock 宿主实际触发 onVisibilityChange(true/false)，或用最小真实宿主+输入组件组合，验证发送受阻及恢复。
- pending 订阅依赖后端 replay_pending，补切模式/重挂重放与重复 invocation 幂等，不新增重复回查通道。
- 对齐 hint 隐藏清空等普通分支行为；isolate/共享 hook 按真实层叠和维护需要决定，不为凑验收机械抽象。
- B 文本答复仍使用中枢正常输入；旧 request_user 工具链与 B 文本链测试分别命名。

**【验收标准】**

- 卡片真实内容非空、会话匹配、可选择/关闭；关闭/回答/超时后输入恢复。
- 多 Worker 征询不把应答发给错误 invocation；pending 保序策略明确。
- request_user 实机记录可保留，但 B 文本端到端须由 G2 单独提供。
- fake timers 在 afterEach 无条件恢复，所有 root 卸载；不把历史偶发失败当自动已根治。

**【依赖关系】** 保持已修沙箱 session 行为；G2 与插件链是不同依赖，不互相替代。

## 12. 集成链：文档、main 分叉与交付证据

### I1：文档对账与旧结论收敛

**【问题】** 当前 plan/progress/设计稿描述已删除对象和多次冲突裁决，测试数字、工具名和模型语义混杂。

**【修法】** 后续单独授权/开发任务中修改下列文件；本次仅编写本文，不执行这些修改。

| 文件位置 | 应改内容 |
|---|---|
| `docs/central/plan.md:894-898` | 当前 work/execution 偏差与模型裁决来源；验证数字按新快照。 |
| `docs/central/plan.md:929/932` | 合并 events.rs 重复覆盖率，旧测量注明时间，不冒充新覆盖率。 |
| `docs/central/plan.md:941-945` | 横幅/覆盖层/直通段落标废弃并指向最新 B+步骤链；可保留沿革但不能三段同时称当前定稿。 |
| `docs/central/plan.md:959/963-968` | 前端用例和矩阵按真实行为证据更新；恢复闭环未验证前不要打全绿。 |
| `docs/central/progress.md:9-34` | 阶段、当前功能、五包统计与实机状态一致；删除已删决定卡片的完成声明。 |
| `docs/central/design-draft.md:338/356-388/1152-1167/1371-1375` | 统一征询、B 决策与模型口径；派发带 model 是助手入口，底层可复用模型编排，不能把底层函数误写成助手可调用工具。 |
| `crates/central/central-plugin/src/tools_context.rs:27/38/39` | 面向模型的旧工具名改为真实十工具+action。 |
| `frontend/src/features/central/components/CentralCardsBar.tsx:1-10` | 删已撤决定卡片的当前态注释。 |
| `crates/central/tiangong-central/tests/event_log.rs:296-312` | 注释说明同进程 cache 测试与进程重启测试各覆盖什么。 |

**【验收标准】**

- 搜索已删组件/命令只允许出现在明确的“历史/已撤销”章节，不在当前能力声明。
- 同一快照五包算式、前端总数、子集数互相一致；所有数字写明日期和范围。
- 用户裁决、提交自述、模型推断分别标记；没有出处就不写“用户明确确认”。
- 本文 ID 与修复提交/测试映射可追溯，不新建没有用途的总结文件。

**【依赖关系】** 随实施阶段维护，最终数字在 I2 验证后更新。

### I2：真实 main 集成与固定快照验证

**【问题】** 当前 main 独有 `17dce4ae`，分支独有 146 提交；审查期间曾外部编辑导致结果不属于 HEAD。仅在分支树上通过不等于合并结果通过。

**【修法】**

- 在获得后续开发权限后，依项目流程选择合并/rebase/集成分支策略，保留用户已有改动；本文不授权任何 Git 写操作。
- 读取共同祖先和双方独有改动，确认 DeepSeek 主线能力不被意外回退，处理 Cargo/接口冲突后验证真实合并树。
- 每次验收记录 HEAD、工作区状态、命令、范围和真实结果。若外部进程改动，立即把已执行结果标为工作树快照，不伪归 HEAD。
- 生产装配 `app.rs:326-394` 至少建立窄场景探针，验证真实 deliver 路径的 model/render/守卫参数；不要仅用 new_for_test 的不同闭包证明生产接线。

**【验收标准】** 后续实施者运行下列命令并保留关键输出；本次文档任务不执行它们。

```bash
# 仓库根目录，先确认固定快照
git status --porcelain=v1
git rev-parse HEAD
git rev-list --left-right --count main...HEAD
git diff --check

# 核心中枢五包与严格检查
cargo test -p tiangong-central -p central-state -p central-plugin -p central-host -p tiangong-app
cargo clippy --workspace --all-targets --tests -- -D warnings
cargo fmt --check

# 前端；worktree 中使用 runner，避免共享 node_modules 的临时配置写入问题
cd frontend
yarn vitest run --configLoader runner
npx tsc --noEmit
```

如项目 CI 还要求 `--benches`、workspace nextest 或其他门禁，按 `CLAUDE.md:16-43` 与真实 CI 补齐，不能用五包替代整个 workspace 声明。每条失败都保留诊断；进度提交可存在，但不得标成可合并。

**【依赖关系】** G/L/S/U 全部关键验收完成；文档数字 I1 最后对账。

### I3：最小实机回归清单

后续在可用构建中执行，记录构建哈希、操作、预期与实际，不需要用户重复提供已授权范围内的普通信息。

1. 普通→中枢→普通，原会话/草稿正确恢复。
2. 新派发指定模型，Worker 会话与实际模型可核对。
3. Running steer 真正送达，不建立错误新 execution。
4. 真 WaitingUser 经中枢对话答复，模型可追问后转发，工作真实完成并回报。
5. pause→resume→终态，cancel 后不被迟到 Done 复活。
6. 同目录排队与 flush，新任务实际投递，无自身死锁。
7. 重启带在途/排队/等待条目，标签、摘要、可取消/重派一致。
8. 长回报/历史引用不伪造当前 WaitingUser，全文可读。
9. Worker 浮层步骤随当前 execution 更新，旧失败不污染新执行。
10. 非默认工作区授权通过/拒绝、空默认拒绝，票据不可偷换。

这些实机项不能取代日志进程级用例；自动测试也不能取代模型行为实测。若硬件/权限不可用，明确留下对应未验证项，不伪称通过。

## 13. 修复顺序、工作量与交接

### 13.1 建议顺序

| 阶段 | 内容 | 完成出口 |
|---|---|---|
| P0 准备 | 固定快照、最新裁决 G0、身份约定 S1 草案、现有回归证据 | 所有参与者使用同一 work/session/execution 定义；不恢复旧直通方向。 |
| P1-A 恢复 | G1+G2，补同 owner id 的真实下发与终态测试；S3 授权可独立并行 | 答复和终态都到达；空默认不放行。 |
| P1-B 日志完整性 | L1—L5 由同一整合者处理 | 长行、轮转、归档、冷损坏、幂等、跨进程全部闭环。 |
| P1-C 状态对账 | S1 最终落地、S2 分类/残留、G3 回报元数据 | 多 execution 投影、重启、待决对象一致。 |
| P2 表达与契约 | G4、S4、U1/U2、旧工具名、错误回执、弱测试 | 用户和模型看到的状态/模型/步骤与真实执行一致。 |
| 交付 | I1/I2/I3，更新本问题映射 | 干净合并结果、五组证据齐全，明确剩余接受风险。 |

### 13.2 工作量定性

- 恢复链代码表面改动可能不大，但执行身份、失败回滚、瞬时完成与迟到回调组合风险高；不得按“改两行”估算全部验证。
- 日志组是**中到高复杂度、数据完整性敏感**任务：跨进程锁、轮转、重启、缓存、归档和损坏处理互相依赖，不可拆成多个独立“小修各 0.1 天”相加估工。
- UI 链属中等范围；正确数据契约先行后再刷新/渲染，避免继续靠视觉组件掩盖状态问题。
- 文案和数字对账本身较小，但依赖最终实现；不应先宣称完成再倒填测试数字。
- 不给未经实施验证的工期承诺。可以按阶段独立提交审阅，但不得在关键链未完成时把分支判为可合并。

### 13.3 接手者执行清单

- [ ] 已读本文快照与裁决来源，不把旧报告结论自动当当前事实。
- [ ] 已确认 Git 状态、HEAD、main 独有提交；保留所有已有改动。
- [ ] R28-1～8 每项有修复提交、真实失败样本和通过证据映射。
- [ ] 恢复测试使用生产 owner id、独立 execution，验证 outgoing 内容与终态回报。
- [ ] 日志测试含独立进程、合法阈值 fixture、冷启动坏中段、归档读取，不用缓存 helper 伪装重启。
- [ ] 最新 B 决策写入 persona，模型推断有显式标签，普通会话模型语义未被无意改坏。
- [ ] 旧开发残留数据不偷偷补写；有授权才做数据操作，保留原始证据。
- [ ] UI 当前阶段与 execution 一致，测试不是 className/固定文字空断言。
- [ ] plan/progress/设计稿当前态唯一，历史方向明确废弃，测试算式正确。
- [ ] 最终真实集成树五组闭环证据完整；不再用“门禁绿”代替功能通过。

## 14. 最终交接结论

第 28 轮的有效进步是：上轮四项宿主回归修复、工作区恢复稳定、288/593 门禁可复核，分层重构成果保留。尚未合入的核心原因是：**恢复身份与消息内容未闭环，事件事实源在长行、轮转和冷损坏恢复时仍不可靠**。

未来的开发与 review 应围绕五组闭环验证，不再围绕每轮新增 UI 或继续追加验收编号推进。修复完成的标志不是“卡片消失”“状态改成 Running”“测试数量增加”，而是用户决定确实进入正确执行、执行有可回收的终态、事实在故障与重启后仍一致、界面与汇报诚实呈现这些结果。
