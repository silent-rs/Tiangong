# 天工中枢模式重构方案

> 状态：重构基线草案
>
> 适用分支：`poc/central-mode` → 后续重构分支
>
> 本方案以当前 POC 的真实实现为起点，不把 POC 中的临时结构直接升级为长期架构。目标是保留已经验证过的核心体验，同时重做状态、事件、权限、持久化和前端交互边界。

---

## 1. 重构目标

### 1.1 必须保留的用户价值

1. 用户面对一个持续存在的中枢对话，而不是任务列表。
2. 用户只表达目标，中枢负责理解、澄清、安排和汇报。
3. 具体文件、命令、浏览器、插件和代码工作由后台 Worker 执行。
4. Worker 的进展、阻塞、完成、失败和验证证据可以回到中枢。
5. 进入、退出或重启中枢不取消后台工作、不覆盖普通会话、不丢失事件。
6. 已有 Worker 会话尽量复用，保持上下文连续。

### 1.2 必须解决的 POC 问题

当前 POC 的主要问题不是“功能不能用”，而是长期演进所需的边界不够硬：

- `src-tauri/src/app.rs` 承担了 Core、派发、回调、状态恢复、报告聚合等多个职责；
- `central-plugin/src/lib.rs` 同时承载人设、工具 schema、派发登记、持久化和权限前置校验；
- 终态回调依赖 session 级 callback slot 和事后 registry 查询，容易发生快速完成竞态；
- 多个 JSON 文件使用 load-modify-save，但没有统一事务写锁；
- Workspace 确认和模型切换的“用户同意”仍可能退化成模型自述；
- `parallel=true` 和后台派发参数主要由模型填写，宿主没有足够的硬校验；
- 中枢视图和普通模式虽然已分离，但切换状态仍主要由前端模块级变量维护；
- POC 的“成功/失败/等待用户”部分语义依赖自然语言标记，不足以作为长期事件协议。

### 1.3 重构完成标准

重构完成不以“文件拆完”为标准，而以以下行为成立为标准：

- 所有后台工作都有稳定的 `work_id`、`session_id`、`workspace_id` 和事件序列；
- 任何终态都能唯一归属到一个 work execution，不依赖“当前 session 最近一条 registry”；
- 进程崩溃、窗口关闭、模式切换、Core 重建都不会丢失已确认事件；
- 任何高风险授权都必须由宿主产生一次性用户确认票据；模型不能自行制造同意；
- 中枢不会直接获得 Worker 的工作型工具；权限网关在构造和调用两层都 fail-closed；
- 前端只消费结构化 Central Event / Snapshot，不从自然语言猜测状态；
- 中枢进入/退出是可取消、可重入、可恢复的显式状态机；
- 普通模式回归测试证明：会话列表、插件面板、附件、模型选择和普通输入行为不被中枢改写。

---

## 2. 产品定位与交互总原则

### 2.1 两个平面

中枢模式分成两个明确平面：

| 平面 | 用户看到什么 | 系统允许什么 |
| --- | --- | --- |
| 中枢平面 | 一个持续对话、目标、建议、工作摘要、需要用户决定的事项 | 读取上下文、组织目标、派发 Worker、接收事件、生成报告、请求授权 |
| 工作平面 | 按需打开的 Worker 详情、原始消息、事件时间线、验证证据、产物 | 使用文件、命令、浏览器、插件等具体工作能力 |

中枢平面不是“拥有全部工具但提醒自己小心”的 Agent。它是控制面；Worker 是执行面。

### 2.2 用户心智模型

用户不需要先理解以下内部概念：Session、Task、Subagent、Tool、Core、Work ID。

用户只需要表达：

- 想达成什么；
- 有哪些约束；
- 哪些事必须自己确认；
- 何时需要结果；
- 是否允许并行、是否只读、是否只分析不修改。

系统内部再把表达转换为：

```text
UserGoal
  -> Intent / Constraints / CompletionCriteria
  -> WorkPlan
  -> WorkItem
  -> WorkerSession / Execution
  -> EventLog
  -> Report / Decision / UserPrompt
```

### 2.3 中枢对话的基本回复结构

中枢每次回复尽量采用以下结构，降低“模型说了很多但用户不知道下一步”的问题：

1. **当前判断**：我理解你想达成什么；
2. **已经做的事**：已经查到、已经安排或已经收到的事实；
3. **下一步**：将要安排什么，或正在等待什么；
4. **需要你决定的事**：只有确实需要时出现，一次一个；
5. **可复核入口**：需要细节时打开工作详情、事件、验证或产物。

禁止仅使用“好的，我来处理”作为派发后的唯一反馈。

---

## 3. 完整交互模式描述

### 3.1 空闲中枢

进入中枢但没有活动工作时：

- 顶栏显示“中枢”；
- 主区域显示简短问候和能力提示；
- 输入框占据视觉中心，placeholder 为“你想让我做什么？”；
- 不显示普通模式侧栏、Session 列表、模型选择、工作区选择、附件入口和插件矩阵；
- 可通过快捷键或顶栏按钮退出中枢。

空态只说明能力，不虚构最近工作、不主动创建目标。

### 3.2 用户表达一个目标

用户输入自然语言后，中枢按以下顺序工作：

1. 判断是否是闲聊、查询、目标表达、已有工作追问或控制请求；
2. 若信息不足，优先在中枢对话中澄清，不创建 Worker；
3. 若能执行，提取约束、完成标准、风险等级和候选 Workspace；
4. Workspace 无歧义且已授权时，创建 WorkPlan；
5. Workspace 有歧义或未授权时，先向用户询问，不调用 Worker；
6. 生成结构化派发命令，由宿主校验后提交；
7. 中枢回复派发事实：工作目的、执行范围、是否修改文件、预计回报方式。

用户不需要手动填写 `work_id`。只有在查看详情或继续既有工作时才看到内部标识。

### 3.3 继续、追加和复查

用户表达“继续上次的 review”“再看看刚才的问题”“补充一个要求”时：

1. 中枢先查询近期 WorkItem 和会话索引；
2. 若唯一匹配，复用原 `session_id`；
3. 若多个匹配，展示简短候选卡片，请用户选择；
4. 若没有可靠匹配，创建新 WorkItem，并说明“未找到可安全复用的既有工作”；
5. 追加要求使用 `SteerExecution`，不能把同一工作误建成全新工作；
6. 用户明确要求“重新开始”时才创建新 session。

### 3.4 并行工作

并行不是模型自由填写的开关，而是一个受约束的用户意图：

- 默认：同 Workspace 的写入任务串行；
- 只读分析任务可以并行，但仍需通过宿主能力判定；
- 用户明确说“并行”只能作为候选依据，最终由宿主根据任务类型、Workspace 锁和风险决定；
- 高风险写入、发布、迁移、删除操作禁止通过普通并行参数绕过锁；
- UI 必须展示“并行原因”和“冲突保护方式”。

### 3.5 Worker 进行中

中枢不转发每个工具调用，而按反馈策略展示：

- `quiet`：只显示开始、阻塞、失败和完成；
- `standard`：加上阶段完成、长等待和关键发现；
- `verbose`：显示计划、Worker、事件和验证命令。

状态条只展示摘要：

```text
执行中 2 · 等待确认 1 · 排队 1
```

点击后进入工作抽屉，而不是切走中枢对话。

### 3.6 Worker 阻塞与用户决定

Worker 不直接调用用户征询工具。它只能提交结构化阻塞事件：

```json
{
  "kind": "needs_user_decision",
  "question": "是否继续执行迁移？",
  "options": ["继续", "暂停", "取消"],
  "risk": "high",
  "evidence": ["..."],
  "execution_id": "..."
}
```

中枢收到后：

1. 把 Worker 状态置为 `waiting_user`；
2. 在中枢对话中说明背景、已完成部分、选项、影响；
3. 一次只展示一个必须决定的事项；
4. 用户选择后由宿主生成 `DecisionTicket`；
5. 票据绑定 `execution_id`、选项、有效期和用户确认来源；
6. Worker 只能使用该票据恢复，不能使用自然语言“用户同意了”恢复。

过期、关闭、取消和无效票据都保持等待或转为取消，不得默认视为同意。

### 3.7 Supervisor 持续指导与 Worker 回报

中枢不是 Worker 结果的转发器，而是持续工作的 Supervisor。Worker 的每次交付先进入内部监督通道，由中枢判断下一步，再决定是否向用户汇报。

严格禁止以下交互：

- Worker 最终答复直接作为中枢对话中的 `UserMessage`；
- Worker 原始长文本直接显示在中枢聊天气泡中；
- 中枢只在 Worker 终态到达后做一次被动转述；
- 把 Worker 的自然语言结论当作用户已经看到或已经确认的事实。

正确链路：

```text
WorkerReportIngress
  -> SupervisorContext（内部，不进入用户对话）
  -> SupervisorTurn
  -> Guidance / FollowUp / UserReport / UserDecision
  -> WorkerExecution 或中枢对话
```

#### Worker 报告的结构

Worker 不直接发送一段无法区分阶段和可信度的文本，而是提交结构化报告：

```json
{
  "execution_id": "exec-123",
  "sequence": 7,
  "kind": "finding",
  "phase": "diagnosis",
  "summary": "超时主要集中在连接池等待",
  "details": "……",
  "evidence": [
    {"kind": "log", "ref": "logs/payment-2026-11-20.txt", "excerpt": "……"},
    {"kind": "command", "command": "cargo test -p payment", "exit_code": 0}
  ],
  "needs_supervisor": true,
  "needs_user_decision": false,
  "suggested_next": "检查连接池配置并补充回归测试"
}
```

报告至少区分：

- `progress`：阶段进展，不要求中枢立即打断 Worker；
- `finding`：关键发现，需要 Supervisor 判断是否改变计划；
- `evidence`：验证命令、日志、产物和引用；
- `question`：Worker 需要 Supervisor 决策，不代表需要用户决定；
- `needs_user_decision`：只有高风险或超出授权时才升级给用户；
- `result`：阶段性结果，不等于整个 WorkItem 完成；
- `completed` / `failed`：真实终态，必须附证据或明确失败分类。

#### Supervisor 何时必须介入

中枢不需要对每个工具调用做逐字指导，但必须在以下节点介入：

1. Worker 开始后确认目标、约束和完成标准；
2. Worker 发现与原计划不一致的事实；
3. Worker 找到关键证据或多个可行方向；
4. Worker 即将进入验证、修改、删除、发布等风险更高的阶段；
5. Worker 请求补充信息、改变范围、切换模型或选择方案；
6. Worker 长时间无进展、重复失败或模型/环境异常；
7. Worker 阶段性结果不足以证明完成；
8. Worker 交付最终结果后，检查完成标准是否满足并决定是否追加验证。

#### Supervisor 的指导类型

Supervisor 的内部决策必须结构化：

```text
Continue        按当前计划继续
Steer           补充约束、调整方向、缩小范围
RequestEvidence 要求 Worker 补验证或产物
Split           拆分为新的后续工作
Retry           在明确边界内重试
AskUser         升级为一次用户决定
Accept          接受阶段结果，生成用户可读报告
Reject          结果不足，要求 Worker 继续，不得汇报完成
```

指导必须回到同一个 `execution_id`，并记录：原因、依据、影响范围和下一次检查点。Worker 不能把“中枢没有回复”解释为批准，也不能把 Supervisor 的内部指导伪装成用户指令。

#### 用户看到什么

用户只看到中枢整理后的可读内容：

- “我已经看到 Worker 的阶段结果，正在要求它补一项验证。”
- “Worker 发现了一个与原假设不一致的现象，我已把范围收窄到连接池配置。”
- “验证已通过，我再检查完成标准和产物后向你汇报。”
- “这一步会修改 staging 数据库，需要你决定是否继续。”

原始 Worker 报告、Supervisor 思考依据、指导记录和事件时间线进入工作详情抽屉，默认不以聊天消息展示。

### 3.8 完成、失败与报告

完成必须来自 Worker 的结构化终态和证据，并且经过 Supervisor 的完成检查：

- 完成：必须有终态事件，满足完成标准，并附验证结果和产物；
- 失败：保留明确失败类别，区分模型/网络、环境、权限、参数和任务本身失败；
- 取消：不生成“完成”报告；
- 等待用户：不是失败，也不是完成；
- 中断恢复：明确说明结果是否存在，不能把“进程停止”写成“任务失败”；
- Supervisor 判定证据不足时，状态保持 `supervising` 或 `needs_follow_up`，不能直接汇报完成。

成功回报默认在多个工作结束后汇总；失败、阻塞和高风险事项即时进入 Supervisor；用户对话只接收 Supervisor 生成的最终汇报。

报告结构：

```text
结论
已完成
验证证据
产物
替你决定的事项
风险与未尽事项
原始工作入口
```

### 3.9 工作详情抽屉

工作详情不离开中枢模式，使用右侧抽屉或底部面板，包含：

- 目标与完成标准；
- Workspace 与权限范围；
- 当前 Worker / session；
- 当前状态和最近阶段；
- **监督活动**：Worker 报告已到达、Supervisor 正在判断、已发出指导、等待新报告；
- **指导记录**：每次 Supervisor 指导的原因、约束、下一检查点和执行结果；
- **原始 Worker 报告**：默认折叠，只在用户主动展开时查看；
- 事件时间线；
- 验证命令及真实结果；
- 产物；
- “打开原始工作会话”按钮；
- 暂停、恢复、取消、继续说明等控制操作。

原始 Worker 会话是复核入口，不是默认操作入口。Supervisor 活动是默认视图，不能把 Worker 原始消息伪装成用户消息。

### 3.9 通知策略

- 中枢对话是主要反馈渠道；
- 系统通知只用于失败、需要及时决定、用户离开期间发生关键变化；
- 中枢自身完成不额外发“任务完成”系统通知，避免同一事实双重打扰；
- 通知必须可以点击回到对应 WorkItem，而不是只展示一段无法追踪的文字。

---

## 4. 中枢模式切换设计

### 4.1 入口

支持三种入口：

1. 顶栏中枢按钮；
2. `Cmd/Ctrl + Shift + J`；
3. 重启后按上次停留偏好恢复。

远程 H5、CLI、Bot 不复用桌面中枢入口；这些入口以后接入中枢协议时，应使用各自的交互适配器。

### 4.2 切换状态机

```text
Normal
  -> EnterRequested
  -> SnapshottingNormalContext
  -> LoadingCentralSnapshot
  -> CentralActive

CentralActive
  -> ExitRequested
  -> DrainingCentralView
  -> RestoringNormalContext
  -> Normal
```

异常分支：

```text
EnterRequested / LoadingCentralSnapshot
  -> EnterFailed
  -> Normal

ExitRequested / RestoringNormalContext
  -> RestoreFailed
  -> CentralActive + 可重试提示
```

切换状态必须由一个宿主级 `ModeCoordinator` 持有，不能继续依赖前端模块级 `preCentralContext` 作为唯一事实。

### 4.3 进入中枢

进入过程：

1. 锁定模式切换，短时间内重复请求合并；
2. 保存普通模式上下文快照：当前 session、新对话草稿、输入缓存、Workspace、扩展面板状态；
3. 读取中枢会话快照和 Central Runtime Snapshot；
4. 如果中枢 Core 不存在，按中枢权限配置创建；
5. 建立中枢事件订阅；
6. 切换视觉层；
7. 触发一次受频率控制的激活问候；
8. 允许用户立即输入，但输入必须等待 Central Ready 或进入有界队列。

后台 Worker 不受影响，不取消、不暂停、不重建。

### 4.4 退出中枢

退出过程：

1. 禁止新的模式切换请求进入；
2. 不取消中枢当前 turn，除非用户明确点击“停止中枢回复”；
3. 保留中枢流式消息和待汇报事件；
4. 保存当前普通模式恢复上下文；
5. 恢复进入前普通会话或草稿；
6. 恢复前端输入缓存和普通布局；
7. 清除中枢视图专属临时状态，但不删除中枢数据；
8. 解除模式锁。

如果普通会话已被外部删除或恢复失败：

- 不静默跳到无关会话；
- 显示“原会话不可恢复”；
- 提供“打开新普通对话”和“查看会话列表”两个明确选项。

### 4.5 视觉转场原则

转场是状态切换的表现，不是业务逻辑的一部分：

- 普通和中枢两端布局都必须可独立渲染；
- 转场可以中途反向；
- `prefers-reduced-motion` 时直接切换；
- 视觉层异常不能阻塞模式切换；
- 不使用 DOM 截图、像素爆散等不可复现的异步特效；
- 使用单一时间轴：`t=0` 普通，`t=1` 中枢；
- 顶栏常驻，标题和关键按钮在时间轴上平滑切换。

---

## 5. 重构后的架构

### 5.1 推荐分层

```text
central-contract
  纯数据结构、命令、事件、状态、错误、票据

central-domain
  状态机、幂等规则、归属规则、报告聚合、风险判定

central-store
  事件日志、快照、索引、事务写入、崩溃恢复

central-orchestrator
  中枢 turn、Worker dispatch、依赖队列、事件路由、回报策略

central-policy
  能力白名单、Workspace 授权、风险等级、模型切换票据

central-host
  Tauri/CoreManager/插件/通知/前端事件适配

frontend/central
  中枢视图、模式协调、工作抽屉、授权卡片、事件投影
```

### 5.2 与当前代码的迁移关系

| 当前 | 重构后 |
| --- | --- |
| `crates/central/tiangong-central` | `central-contract` + `central-domain` |
| `crates/central/central-state` | `central-domain` + `central-store` |
| `crates/central/central-plugin/src/lib.rs` | `central-policy`、`central-orchestrator` 的工具适配层；目标控制在 300～500 行 |
| `src-tauri/src/app.rs` 中枢大段逻辑 | `src-tauri/src/central_host/` 多模块 |
| `central_work_registry` | `CentralProjection`，由事件日志重建，非第二份事实源 |
| `central_watch_slots` | `ExecutionRouter` 按 `execution_id` 路由终态 |
| `work-items.json` 多函数直接改写 | 事务化 `CentralStore::append_event / commit_snapshot` |
| 前端 `preCentralContext` | 宿主 `ModeCoordinatorSnapshot` + 前端临时 UI 状态 |

### 5.3 单一事实源

长期架构只允许两类权威数据：

1. **追加式事件日志**：记录事实；
2. **可重建快照**：用于快速加载，不是独立事实源。

前端状态条、报告队列和中枢提示都由事件投影生成。禁止把“内存 registry + work-items.json + 中枢消息自然语言”同时当作事实源。

### 5.4 事件与执行归属

每次 Worker turn 创建一个 `Execution`：

```text
work_id       用户层工作
execution_id  一次实际执行/恢复/重试
session_id    Worker 对话上下文
workspace_id  权限与写入锁边界
attempt       第几次尝试
```

所有 `Started / Progress / WaitingUser / Resumed / Done / Failed / Cancelled` 事件必须带 `execution_id`。终态回调不再通过“当前 session 的最近条目”猜归属。

### 5.5 持久化事务

提供统一接口：

```rust
trait CentralStore {
    fn append(&self, event: CentralEvent) -> Result<EventOffset>;
    fn commit_snapshot(&self, snapshot: &CentralSnapshot) -> Result<()>;
    fn recover(&self) -> Result<RecoveredCentralState>;
}
```

要求：

- 同一存储根的进程内写锁；
- 多进程场景使用文件锁或 SQLite 事务；
- 临时文件名必须带随机后缀，不能共用固定 `.tmp`；
- `append + projection update + snapshot` 有明确提交顺序；
- 写失败必须让调用方知道，不能只记录日志后继续声称成功；
- 崩溃恢复时校验最后完整事件和 snapshot version；
- 事件 schema 带版本，支持向前迁移和未知字段兼容。

### 5.6 权限与票据

#### WorkspaceApprovalTicket

```text
approval_id
path / workspace_id
task_fingerprint
risk_level
user_decision
created_at
expires_at
used_at
```

只有宿主 UI 收到真实用户操作后才能签发。模型只能请求票据，不能生成票据。

#### DecisionTicket

绑定：

- `execution_id`；
- 阻塞问题版本；
- 选项；
- 用户决定；
- 签发时间和有效期。

#### ModelSwitchTicket

绑定：

- `session_id`；
- `work_id`；
- `target_model`；
- 用户确认记录；
- 是否只允许下一轮生效。

所有票据一次性使用，宿主验证后才进入执行路径。

### 5.7 Worker 调度与 Supervisor 编排

`CentralOrchestrator` 对外提供少量命令：

```text
CreateGoal
PlanWork
DispatchWork
SteerExecution
PauseWork
ResumeWork
CancelWork
CollectReports
OpenWorkView
RequestApproval
```

内部再拆为两个明确角色：

```text
WorkerRuntime
  只负责执行具体工作、产生结构化 WorkerReport、接受 Guidance

SupervisorRuntime
  负责读取 WorkerReport、判断是否继续、生成 Guidance、升级用户决定、生成 UserReport
```

调度器内部负责：

- Workspace 锁；
- 显式依赖；
- 会话复用；
- execution 归属；
- Worker 生命周期；
- WorkerReport 入库；
- SupervisorTurn 调度；
- Guidance 顺序与幂等；
- 背压和队列上限；
- 事件入库；
- 失败分类和重试建议。

Worker 的原始报告只能进入 Supervisor 输入通道，不能直接进入用户对话输入通道。Supervisor 生成的用户报告才可以进入中枢对话。

模型不能直接向 `tokio::mpsc` 发送未验证的执行请求。

---

## 6. 前端重构方案

### 6.1 组件树

```text
MainApp
└── ModeCoordinatorView
    ├── NormalModeView
    ├── CentralModeView
    │   ├── CentralTopBar
    │   ├── CentralConversation
    │   │   ├── UserMessage
    │   │   ├── SupervisorMessage
    │   │   ├── SupervisorActivityBlock
    │   │   ├── UserDecisionCard
    │   │   └── ReportCard
    │   ├── CentralWorkSummaryBar
    │   ├── CentralWorkDrawer
    │   │   ├── SupervisorActivityTimeline
    │   │   ├── GuidanceTimeline
    │   │   ├── WorkerReportPanel
    │   │   └── EvidencePanel
    │   ├── CentralDecisionCard
    │   └── CentralComposer
    └── ModeTransitionLayer
```

### 6.2 Supervisor 交互投影

中枢聊天区只能渲染以下几类消息：

- `UserMessage`：用户真正发送的内容；
- `SupervisorMessage`：中枢自己的判断、计划、指导结果和用户汇报；
- `UserDecisionCard`：需要用户确认的结构化事项；
- `ReportCard`：Supervisor 汇总后的可读结果。

Worker 原始报告不使用 `UserMessage`，也不伪装为中枢发言。默认以折叠的 `SupervisorActivityBlock` 表示：

```text
已收到 Worker 阶段报告
→ Supervisor 正在检查证据
→ 已发出下一步指导：补充连接池配置验证
→ 等待 Worker 返回
```

用户点击“查看详情”后，才在工作抽屉中展开原始报告、证据和指导记录。

### 6.3 前端状态边界

持久事实从 API 获取：

- `CentralSnapshot`；
- `CentralEvent`；
- `ModeState`；
- `WorkSummary`。

前端临时状态只保存：

- 当前打开的抽屉；
- 当前选中的工作；
- 输入框草稿；
- 转场动画进度；
- 当前请求是否等待响应。

不要让 Zustand 中的临时 `centralMode` 直接成为后台事实。它只能表达用户的视图偏好，最终布局由 `ModeCoordinator` 决定。

### 6.3 中枢输入区

保留独立输入区，但补齐：

- 明确的发送中、等待宿主、失败状态；
- `Esc` 取消当前输入；
- `Shift+Enter` 换行；
- 中枢回复进行中允许追加消息；
- 模式切换中输入进入有界队列，超过上限提示用户；
- 不显示 Worker 专属模型、信任模式、Workspace 选择和附件控件；
- 用户需要指定范围时在自然语言中表达，必要时中枢通过卡片确认。

### 6.4 工作抽屉

抽屉打开不改变模式，不改变普通会话上下文。抽屉关闭只影响视图，不影响 Worker。

---

## 7. 分阶段实施计划

### 阶段 R0：冻结契约与观测（1 个小迭代）

- 冻结 `CentralEvent`、`Execution`、`WorkItem`、`CentralSnapshot` schema；
- 为当前 POC 加事件日志观测，不改变用户交互；
- 给每次派发生成 `execution_id`；
- 增加派发、快速完成、失败、恢复和模式切换日志；
- 建立 POC 行为基线测试。

完成标准：旧 POC 的基本用户流程不回归，所有后台工作可在日志中唯一定位。

### 阶段 R1：拆分宿主控制器

- 从 `app.rs` 拆出 `central_host`；
- 从 `central-plugin/lib.rs` 拆出工具适配、策略和存储调用；
- 统一 `CentralCommand` 通道；
- 去掉 session 级 callback slot，改 execution router；
- 先保留现有前端界面。

完成标准：宿主不再通过自然语言和“当前 registry”判定终态归属。

### 阶段 R2：事件日志和事务存储

- 引入 `central-store`；
- 所有 work-items/goals/reports/events 迁移到统一事务接口；
- 做旧 JSON 到新 schema 的一次性迁移；
- 加崩溃恢复、重复事件和并发写入测试；
- 事件投影生成当前状态。

完成标准：并发完成、进程重启、写失败和重复回调均可恢复且不丢事实。

### 阶段 R3：权限票据化

- 移除模型可直接执行的 `confirm_workspace`；
- 接入 WorkspaceApprovalTicket；
- 接入 DecisionTicket；
- 接入 ModelSwitchTicket；
- `parallel` 改为宿主策略结果，不接受模型直接越权；
- 统一风险分级和审计记录。

完成标准：没有真实用户确认，就无法派发到未授权 Workspace、无法执行高风险动作、无法切换 Worker 模型。

### 阶段 R4：ModeCoordinator 与前端工作视图

- 宿主持有模式状态和普通上下文快照；
- 前端改为消费 ModeSnapshot；
- 新增工作抽屉、决定卡片、报告卡片；
- 转场只作为表现层；
- 补齐快速切换、恢复失败和重启恢复交互。

完成标准：模式切换可重入、可恢复、无损，用户可以在不离开中枢的情况下复核后台工作。

### 阶段 R5：清理 POC 兼容层

- 删除旧 `central_work_registry` 双写路径；
- 删除固定 work-items JSON 直接改写入口；
- 删除自然语言终态标记作为协议的依赖；
- 压缩 `central-plugin`；
- 更新设计文档、PLAN/TODO 和实机验收记录。

完成标准：新架构成为唯一路径，旧路径只保留必要的数据迁移代码。

---

## 8. 测试与验收矩阵

### 8.1 领域测试

- 状态转移合法性；
- 终态幂等；
- execution 归属；
- 同 session 多 execution；
- waiting user 恢复；
- cancel/pause 后迟到 Done；
- 依赖和 Workspace 锁；
- WorkerReport 不会进入用户消息流；
- Supervisor 收到阶段报告后能生成 Guidance；
- Guidance 必须回到同一 execution；
- Supervisor 判定证据不足时不会生成完成报告；
- 用户只看到 SupervisorMessage / ReportCard，不看到伪造的 Worker UserMessage；
- 报告聚合策略。

### 8.2 存储测试

- 两线程并发 append；
- snapshot 写一半进程退出；
- 临时文件残留恢复；
- 重复 event；
- 旧 JSON 迁移；
- 事件日志和 snapshot 不一致修复。

### 8.3 权限测试

- 模型伪造确认文本被拒绝；
- 过期票据被拒绝；
- 票据错绑 `work_id` 被拒绝；
- `parallel` 越权被拒绝；
- 中枢永远无法调用 Worker 工具；
- 未授权 Workspace 的所有入口都 fail-closed。

### 8.4 Host 集成测试

- 派发后立即完成；
- 派发失败；
- 回调在投递前/后到达；
- Worker 重建；
- 应用重启恢复；
- 多 Worker 同时终态；
- 模式切换期间 Worker 继续执行；
- 中枢 Core 失败不影响普通 Worker。

### 8.5 前端测试

- 进入/退出/快速反向切换；
- 进入加载失败；
- 普通草稿恢复；
- 中枢输入和追加消息；
- 工作抽屉不改变中枢上下文；
- 决定卡片一次一个；
- 通知点击定位 WorkItem；
- reduced motion；
- 普通模式真实组件 smoke test。

### 8.6 实机验收

1. 普通模式已有会话和草稿；
2. 进入中枢，激活问候出现；
3. 中枢派发一个只读 Worker；
4. 派发一个写入 Worker，验证 Workspace 授权卡片；
5. Worker 阻塞，用户决定后恢复同一 execution；
6. Worker 快速完成、失败、取消、暂停、恢复；
7. 中枢与 Worker 同时流式输出；
8. 中枢退出再进入；
9. 重启应用后恢复事件、报告和普通上下文；
10. 普通模式列表不显示中枢会话，普通插件能力不受影响。

---

## 9. 不在本轮重构范围内

以下能力不作为重构前置条件：

- 跨设备中枢同步；
- 云端事件总线；
- 多中枢并行人格；
- 自动长期记忆重写；
- 自主主动联系用户；
- CLI/Bot 完整交互卡片；
- 复杂 DAG 可视化编辑器。

先把桌面端单中枢、单用户、可复核后台工作闭环做成可靠基础，再扩展多入口和主动智能。

---

## 10. 建议的第一批重构提交

建议拆成以下互不混淆的提交：

1. `central: freeze event and execution contracts`
2. `central: add transactional event store`
3. `central: route terminal events by execution id`
4. `central: persist dispatch before delivery`
5. `central: introduce approval and decision tickets`
6. `central: enforce workspace and parallel policy at host boundary`
7. `central: extract orchestrator from app state`
8. `frontend: add mode coordinator snapshot`
9. `frontend: add central work drawer and decision cards`
10. `central: migrate legacy poc records and remove dual writes`

每个提交都应能单独编译、测试和回滚，不要在一次提交中同时重写后端状态机、前端转场和全部工具协议。

---

## 11. 最终原则

1. 中枢是控制面，不是“全工具 Agent”。
2. 用户确认必须是宿主事实，不是模型文字。
3. 事件是事实，快照是缓存，UI 是投影。
4. 每次执行必须有唯一归属，不能靠当前 session 猜测。
5. 模式切换只改变用户视图，不改变后台工作的生命周期。
6. 失败要可解释，完成要有证据，等待不能伪装成失败。
7. 先保证恢复和权限，再追求主动智能和华丽转场。
