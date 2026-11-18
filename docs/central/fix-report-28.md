# 第 28 轮 review 修复报告（R28-1～8 + 五组闭环）

> 性质：修复实施报告，供第 29 轮 review 作对照依据。不是完成度宣传——每项含修复位置、锁定测试、已知边界与未完成清单。
>
> 修复基线：`f70988d7`（第 28 轮 review 快照前：嗅探行首匹配 + 同会话互斥三豁免 + 通知克制 persona——本轮 review 已认可的方向延续）。
> 完成快照：`e46c2068`（第 28 轮历史快照）；第 29 轮工作区修复及当前验收见 §7。
> 本文 §1/§2 的绿色标记仅为第 28 轮当时声明，不代表 R29 复核后完整闭合；§3 已按实际缺口纠偏，R29-8 的专项验证见 §8。
> 依据文档：`docs/central/review-28.md`（第 28 轮 review，问题 ID 沿用其编号）。

## 0. 提交清单

| 提交 | 内容 |
|---|---|
| `f70988d7` | 征询嗅探行首匹配 + 互斥三豁免 + persona 通知克制（第 28 轮 review 认可的先行修复；嗅探后续在 `1cb50fac` 再次收敛） |
| `a1ef2e6b` | 第 28 轮 review 报告入库（828 行） |
| `9fe06604` | 恢复链 G0/G1/G2 + 授权 S3（R28-1/2/8） |
| `7afc10e0` | 日志链 L1-L5（R28-3/4/5/6 + 跨进程幂等） |
| `df8b425b` | 状态链 S1/S2/G3（R28-7 + 中断口径 + pending_question） |
| `b2e0d892` | P2 表达与契约（G4/S4/U1/C01/C02 + persona 裁决同步） |
| `1cb50fac` | 嗅探最终收敛独占行协议 + C15/C16 |
| `dd8944c2` | I1 文档对账（废弃标注/裁决纠偏/基线刷新） |
| `e3a601eb` | I2 merge main（DeepSeek 全家桶并入，无冲突） |
| `4c6ff1a3` | merge 后 Cargo.lock 同步 |
| `e46c2068` | 复核补齐（S4 全 RMW / P2-03 / P2-09 / P2-10 / C03——详见 §5） |

---

## 1. 八项 P1 合入阻断逐项报告

### R28-1（G1）登记原任务与实际下发答复分离 ✅

- **修复**：`central-host/src/host/dispatch.rs` plan 阶段不再以 `owner_task` 覆盖 `request.task`；新增 `instruction_override`（恢复组合指令：【原目标】+【本次用户/中枢答复】）与 `registry_task`（登记/事件投影显示 owner 原目标）两个 DispatchPlan 字段；事件 payload 另记 `resume_answer` 供追溯。
- **架构裁决落档**：Worker 一切消息均由中枢下发，用户不直接与 Worker 交互（persona + 代码注释 + plan.md）。
- **锁定测试**：`host_waiting_user_merge_restores_owner`（升级为端到端）——生产 CC2 形态（同 owner work_id + 新 execution），断言投递内容含「选方案B」真实答复与【原目标】上下文、登记 task 保持 owner 原文。
- **验收对照**：报告 G1 要求「断言 deliver 收到的 ContentBlock 包含真实答复」✅（经 worker 会话文件读回）；「原任务与 follow_up 都可查询」✅（registry_task + resume_answer）；空答复/多行 Unicode/前次终态/目标不存在**未逐一加测**——边界留 review 复核。

### R28-2（G2）恢复新 execution 终态可达 ✅

- **修复**：`central-state` 新增 `DispatchIntent` 枚举（NewExecution/ResumeWaiting/ResumePaused/SteerRunning）+ `classify_dispatch_intent` 表驱动——按 registry **转移前状态**显式分类，取代「merge 置 Running 后 work_id 反猜」（探针 PR-08 callback=0 的误判形态）。恢复意图一律 register（投递前声明活跃执行）+ attach（投递成功挂载回调）；仅真 SteerRunning 不替换执行。upsert 前终态保护：fire-before-attach 缓冲下立即完成的 turn 已落终态时，迟到的 Running upsert 不得覆盖。
- **锁定测试**：同上端到端测试的 fire 段——`execution_router.fire` 触发后断言 WaitingUser→Running→ReportQueued 且完成回报入队；`dispatch_intent_classification_matrix`（10 组合表驱动）。
- **验收对照**：「覆盖 live Core、无 Core 重建、投递失败、fire 先于 attach、重复 fire、旧 exec 迟到」中，live Core + fire 先于 attach（缓冲语义）经测试覆盖；**无 Core 重建路径沿用的 ensure 分支未单独加测**——留 review。

### R28-3（L1）任意长事件后 offset 连续 ✅

- **修复**：`tiangong-central/src/event_log.rs` `tail_last_offset` 固定 4096 字节窗口改为 4K→32K→256K… 逐步扩窗（覆盖文件头即止），窗口未覆盖全文件时跳过可能截半的首行。
- **锁定测试**：`long_event_tail_offset_survives_restart`——8KB/64KB 任务全文 payload（含中文），模拟新进程（force_repair + 新实例）append 断言 offset==2（PR-02 场景）。

### R28-4（L2）轮转全生命周期 ✅

- **修复**：锁迁移至同目录 `events.jsonl.lock` 专用锁文件（跨 rename 稳定，取代锁当前 inode）；轮转改为先 rename 归档、再重开当前文件写入（旧代码持旧 inode fd 跨 rename 续写——事件进归档、当前路径消失）；rename/open 失败显式返回 Err；当前文件空/无有效行时 offset 基准从归档 `.1` tail 兜底（轮转瞬间与新进程跨代际连续）。
- **测试钩子**：`open_with_rotate_threshold`（#[doc(hidden)]）注入小阈值。
- **锁定测试**：`rotation_lifecycle_keeps_current_file_and_offset`——小阈值触发轮转，断言当前路径存在、归档+当前事实齐全、新进程 offset 从归档兜底衔接（PR-05 场景）。**多进程阈值边界竞争未测**（报告 L2 验收第 3 条）——留 review。
- **平台说明**：FileLock 在非 unix 为空实现（生产平台 macOS；C11 要求的平台标注已在注释中）。

### R28-5（L3）恢复读取覆盖归档 ✅

- **修复**：`load_all_lenient` 与严格读取共享文件范围（归档 `.1` 在前 + 当前在后），逐段损坏计数（`parse_lenient_into`）。
- **锁定测试**：`lenient_read_includes_archived_history`——归档 3 条当前空，宽松/严格读取均 3 条（PR-06 场景）。

### R28-6（L4）冷启动不截中段 ✅

- **修复**：`repair_tail` 仅截「末尾写一半」（无换行结尾残片，逐步扩窗回扫最后换行）；中段坏行不再触发 set_len——原文件保留证据，由 lenient 跳过计数；修复持锁文件（与 append 互斥）。append 冷缓存重建同步 lenient 语义（基准取幸存事件最大 offset）。
- **锁定测试**：`cold_open_keeps_events_after_midfile_corruption`——冷文件「好→坏→好→好」，坏行后两条可见、坏行计数 1、损坏证据保留在原文件、追加 offset==4（PR-07 场景）。
- **配套**：`force_repair_on_next_open` 重置双门槛（尾部修复 + 冷缓存），C9 的「open 提前置 loaded 导致 append 冷重建不可达」同步消除。

### R28-7（S1）work/session/execution 生命周期 ✅（三处）

1. **插件侧**（`central-plugin/src/tools_dispatch.rs`）：复用会话仅当最近登记**未终态**（outcome/finished_at 均空）才沿用 work_id；终态后复用分配新 id。
2. **投影侧**（`tiangong-central` `project_work_state`）：终态吸收限缩到**同 execution**（terminal_executions 记录终态执行，不同 execution 的 Started 不被吞）；legacy 无 execution 数据保守保持旧行为。
3. **rebuild 侧**（`central-host` `rebuild_registry_from_log`）：owner 优先仅在既有条目**未终态**时成立（引导并入 owner）；既有已终态保留新条目。

- **锁定测试**：`projection_new_execution_after_terminal_not_absorbed`（同 work 新 execution 推进 Running + 同 execution 迟到事件仍吸收，PR-04 场景）。
- **验收对照**：「取消后新任务不被旧取消吸收；registry/work-items/事件重建一致」部分覆盖（投影层已锁，全链重启重建对账**未单独加测**）——留 review。

### R28-8（S3）空默认工作区 fail-closed ✅

- **修复**：`central-state/src/persistence.rs` `workspace_confirmed`——默认子树放行仅在默认路径非空且真实存在（is_dir）时成立；空 target 保持放行（上游已解析为合法默认后到达）。
- **锁定测试**：`workspace_confirmation_empty_default_fails_closed`——空默认拒 /tmp（PR-09 场景）、幽灵默认拒子树与自身、空 target 分层放行。
- **测试 fixture 连带**：`plugin_with_channel`/`manager_with_session_cwds` 真实创建登记目录（保持「默认目录已授权」前提）。

---

## 2. P2 群与持续项

| ID | 状态 | 说明 |
|---|---|---|
| P2-01 | ✅（分两次收敛） | `f70988d7` 行首匹配 → `1cb50fac` **独占行协议**（`line.trim() == 标记`）：PR-10 实证「标记+叙述句」行首形态仍误判后最终收敛。测试补真实多行样本（标题引用/叙述句/独占行三态） |
| P2-02 | ✅ | 互斥按状态×id×复用精确分类（`9fe06604`）：Running 仅 reused×同 id 放行；WaitingUser 放行（merge）；Paused/Queued 同 id 放行。三组边界断言 |
| P2-03 | ✅（`e46c2068`） | `upsert_work_item` 在途同 id 保留 owner 任务文本（引导/答复不覆盖登记目标）；终态后新工作新文本。回归锁 `upsert_keeps_owner_task_for_inflight_and_updates_for_new_work` |
| P2-04 | ✅ | 派发回执改「已受理派发（排队/拒绝由宿主决定，状态经 work 可查）」 |
| P2-05 | ✅（`e46c2068` 补全） | 三个 handler 全部接入 `with_goals`/`with_reports` 锁内 RMW（**复核发现首轮只接了 create_goal**——见 §5）；create_goal 写失败显式报错 |
| P2-06 | ✅ | `system_note_kind_for` 唯一判定源，即时注入与 commands.rs flush 双路径一致 |
| P2-07 | ✅ | `WorkStatus.pending_question` 结构化（waiting_user 登记/终态清理/事件 payload 携带/投影重建/前端展示） |
| P2-08 | ✅ | 恢复测试改生产 owner id + 新 execution（G1/G2 端到端） |
| P2-09 | ✅（`e46c2068`） | control 真断言：预置 owner 持久化记录，cancel 后 registry=Cancelled + 落盘 outcome=cancelled |
| P2-10 | ✅（`e46c2068`） | steer 测试改生产形态（同 owner id——**旧异 id 形态在新互斥下走排队，旧断言对排队也通过＝假绿**）+ 断言 Core 活跃与 pending 空（真并入不排队） |
| P2-11 | ❌ 未做 | 生产装配窄探针（app.rs deliver 路径）——留后续 |
| P2-12/13 | ✅（部分） | U1：StepChain 按 executionId 过滤（旧执行 failed/cancelled 不污染新执行）；「曾排队来自真实排队事实（payload 结构化 phase）」**未做**——state+事件并集推断保留 |
| P2-14 | ✅ | I1 文档对账（plan.md 三段废弃标注 + A.2 第 8/9 条更新 + 第 28 轮定稿段） |
| P2-15 | ✅（部分） | activeCount 排除 interrupted（S2）；「取消/确认/重派 UI 入口」**未做**（control 命令链已有，UI 入口留后续） |
| P2-16 | ❌ 未做 | DecisionTicket 前端闭环——票据签发/恢复消费链路存在，前端无实际使用入口 |
| P2-17 | ✅ | L5 跨进程幂等（锁内磁盘 id 行级查证）；`multiprocess_duplicate_id_is_idempotent` 两子进程同 id 一条事实（PR-03 场景） |
| P2-18 | ✅ | persona「默认立即转发可澄清」（D15）/ 换模型自主判断 + 持久化如实描述（D08 纠偏：降级为实现事实，plan.md A.2 第 9 条同步） |
| C01/C02 | ✅ | get_context 旧工具名改十工具+action；inspect_context path 契约统一「缺省用默认工作区」 |
| C03 | ✅（部分，`e46c2068`） | interrupted_work_items 排除排队挂起；get_context 文案改「近期未收尾（含进行中与重启中断）」不再泛称。**与运行 registry 的精确对账（跨层）未做** |
| C04 | ❌ 未做 | 损坏数据写前备份/隔离 |
| C15 | ✅ | centralCardsBar afterEach 无条件 useRealTimers |
| C16 | ✅ | 恢复摘要文本断言（中断数 1 + 失败计数 1 的实际文案） |
| C05/C06/C13 及其余 C 项 | ❌ 未做 | 工程债，不与 P1 混级（报告 §5.3 原判定） |

## 3. 五组闭环证据对照（报告 §6.3）

| 链 | 状态 | 依据与缺口 |
|---|---|---|
| 恢复链 | 部分闭合：R29 自动回归通过，非全链验收 | T1/T2 锁定 Interrupted 重派与 attach 同步重放，真实答复恢复测试保留；无 Core 恢复的真实模型行为、失败回滚完整矩阵与 I3 未验收 |
| 日志链 | 部分闭合：R29-8 共同生命周期锁已修复 | 严格/宽松读取与 append/轮转共用稳定锁，同进程独立实例和跨进程的读-轮转交错均已回归。幂等、非法 UTF-8、最大 offset、长行、归档亦有验证。多写者阈值边界竞争与轮转失败注入仍未覆盖；一级归档仍会覆盖更早历史 |
| 状态链 | 部分闭合：R29 生命周期回归通过 | T1～T4 + steer 日志接线 + 当前 execution 待决问题重建/取消清理；完整跨层对账、无 execution 历史记录的歧义仍留边界 |
| UI 链 | 部分闭合：前端 593 项回归通过 | StepChain/pending_question/activeCount 已有覆盖；事件订阅刷新时序、决定票据 UI 与取消/重派入口未补，不能称整体闭合 |
| 集成链 | 部分闭合：五包与静态门禁通过 | R29-8 专项后五包 313/313，workspace Clippy/fmt/tsc/Vitest 通过；历史 main 合并事实保留，未重跑全 workspace 测试、未做实机 I3 |

## 4. 验证证据（完整口径，2026-11-17 `e46c2068`）

| 命令 | 结果 |
|---|---|
| `git status --porcelain=v1` | 干净 |
| `git rev-list --left-right --count main...HEAD` | 0 / 156（main 无落后） |
| `cargo fmt -- --check` | 通过 |
| `cargo clippy --workspace --all-targets --tests --benches -- -D warnings` | 0 error（tiangong-scheduler 2 个 manifest 依赖声明 warning 为 pre-existing，双分支同态） |
| `cargo nextest run --workspace --no-tests pass --no-fail-fast` | **2125 测试：2124 通过，1 失败**——`tiangong-sandbox` seatbelt `profile_denies_write_outside_roots_and_network`，**主仓库 main（c64c20f3）同样复现**，非本分支引入 |
| 中枢五包 nextest | **297/297**（tiangong-central 44 / central-state 33 / central-plugin 40 / central-host 11 / tiangong-app 169） |
| `cargo deny check` | bans ok / sources ok；licenses 与 advisories FAILED——**与 main 完全同态**（webpki-roots license 例外未命中、chrono unmaintained 等依赖生态问题） |
| `npx tsc --noEmit` | 0 错误 |
| `yarn vitest run --configLoader runner` | 593/593（66 文件） |

新增回归测试清单（本轮全部）：`dispatch_intent_classification_matrix`、`host_waiting_user_merge_restores_owner`（端到端重写）、`long_event_tail_offset_survives_restart`、`rotation_lifecycle_keeps_current_file_and_offset`、`lenient_read_includes_archived_history`、`cold_open_keeps_events_after_midfile_corruption`、`multiprocess_duplicate_id_is_idempotent`、`projection_new_execution_after_terminal_not_absorbed`、`workspace_confirmation_empty_default_fails_closed`、`upsert_keeps_owner_task_for_inflight_and_updates_for_new_work`、互斥边界三组、嗅探真实多行样本三态、C16 摘要断言。

## 5. 复核过程披露（review 重点核查提示）

第一轮修复声明经复核发现**四处与实现不符**，已在 `e46c2068` 补齐——review 时建议优先核查这些位置（历史上最容易「声明完成但实现不全」）：

1. S4：`with_goals/with_reports` 建了但只接了 create_goal（update_goal_status/create_report 锁外三步仍在）——补齐。
2. P2-10：steer 测试异 id 形态在新互斥下走排队，旧断言对排队也通过（假绿）——改生产形态 + 真并入断言。
3. P2-09：control 调用无结果断言 + 空集合断言恒真——预置记录 + 双断言。
4. P2-03：宿主侧 registry_task 保护了内存登记，插件侧 upsert 落盘的 task 仍被引导文本覆盖——upsert 在途保留 owner 任务。

另有一次实现期死锁（`with_goals` 内调自取锁的 `save_goals` → 同线程重入）被测试挂住暴露，以 `save_goals_locked/save_reports_locked` 无锁写核修复——**review 可复核 RMW 区间内不得调用自取锁写函数**这一约束的注释与实现。

## 6. 已知边界与未完成（诚实清单）

1. **嗅探独占行协议的边界**：代码块内独占一行的标记文本层无法区分（真征询同为独占行）。完整解法为结构化 outcome（S2 修法 1 的 render 层方向）——当前文本 fallback 的已知限制，测试注释已声明。
2. **G2 验收缺口**：无 Core 重建路径的恢复、投递失败回滚、重复 fire 未单独加测。
3. **G1 验收缺口**：空答复/多行 Unicode/前次终态/目标不存在的无意义投递防护未逐一加测。
4. **L2 验收缺口**：多进程在阈值边界竞争轮转未测。
5. **I3 实机十步未做**（需 dev 重启后真实桌面环境；含模型行为实测——「测试绿 ≠ 模型一定选对工具」）。
6. P2-11（装配探针）、P2-16（票据前端）、C04/C05/C06/C13、C03 完整对账、U1 刷新时序与排队事实结构化——未做，定位为非阻断。
7. 开发环境残留数据（work-03h8ujgnub0s5jk2iufuhvujk 的 waiting_user）按 D19 用户裁决**不补记**——重启后由 recover 迁移 Interrupted，本文档不以任何方式将该残留算作已修复。

## 7. 第 29 轮修复（2026-11-17，基线 `3424cff0`，工作区待用户审阅）
### 7.1 范围与实施约束
- 开始时 `git status --short` 无输出；分支 `feature/central-refactor`，HEAD `3424cff0`。未切换分支、未 commit、未 push。
- 严格 TDD：先仅新增 T1～T4，执行确认四项断言失败后才改生产代码；F4～F8 也先添加/增强会失败的用例。
- 采用最小局部修复，不迁移用户数据、不扩大 UI/架构重构范围。生产改动限 `dispatch.rs`、`central_events.rs`、`reports.rs`、`tools_dispatch.rs`、`event_log.rs` 五个文件。
- 测试中 deterministic deliver/ensure 通过现有宿主注入接口控制时序，不连接模型、不使用网络；T1 调真实 recover、插件工具入口和 dispatch/router；steer 接线另走真实测试 Core。

### 7.2 T1～T4 实际红转绿
第一阶段（生产代码未改）：
```text
cargo test -p tiangong-app --test central_host_integration r29_ -- --nocapture
0 passed; 3 failed（/tmp/r29-t123-red.log）
T1: Interrupted != Running
T2: Running != ReportQueued
T3: A != B
cargo test -p tiangong-central --test event_log r29_t4 -- --nocapture
0 passed; 1 failed（/tmp/r29-t4-red.log）
T4: late WorkItemCompleted: Completed != Running
```
每项修复后立即单跑：T1、T2、T3、T4 分别 **1 passed / 0 failed**，日志 `/tmp/r29-t1-green.log`～`/tmp/r29-t4-green.log`。最终五包全量包含这些测试并通过。下列文件行号基于本轮格式化后的工作区（后续编辑可按函数名定位）：

| 项 | 问题与根因 | 改动位置 | 测试与证据 |
|---|---|---|---|
| R29-1 / F1 | 重启留下旧 Interrupted；同 work_id 新 execution 被旧终态保护挡住 upsert、回调也被丢弃 | `crates/central/central-host/src/host/dispatch.rs:760-846,883-943`：成功派发先登记当前 execution，再 attach；状态保护按 execution；被新 execution 接管的旧回调一律跳过 | `r29_t1_interrupted_redispatch_reports_new_execution`（`src-tauri/tests/central_host_integration.rs:3311`）：真实 recover→Interrupted→插件沿用 work_id/生成新 execution→Running→ReportQueued/回报；重复/旧回调无新回报，红转绿 |
| R29-2 / F1 | attach 会同步重放，原尾部把 WaitingUser/ReportQueued 写回 Running、重插 running、清问题；且日志可能终态先于 Started | `dispatch.rs:628-680,714-846`：Started/落盘执行归属/Running/progress/running 集合在 attach 前完成；attach 后无 Running 写入；Queued 同执行可正常推进 | `r29_t2_fire_before_attach_preserves_callback_state`（`:3405`）：成功/征询两种同步重放、running 不残留、问题不丢、日志重建一致，红转绿；已有 `host_dispatch_queues_on_write_lock_then_delivers` 检出保护过严后修正并全绿 |
| R29-3 / F2 | 重建将“新条目在途+reused”误认为引导，旧 owner 顶掉新 B | `crates/central/central-host/src/host/central_events.rs:252-300`：最新 Started 默认保留；同 work 保留最新执行；异 id 只接受显式 `owner_work_id` 或同 execution 的有效引导关系 | `r29_t3_rebuild_keeps_new_running_work_over_old_owner`（`:3451`）A 完成→B Running，红转绿；既有 `host_rebuild_prefers_owner_over_steer_orphan` fixture 加明确 owner 关系（不再用含糊 reused 猜归属） |
| R29-4 / F3 | 终态只按“上一次终态 execution”吸收，旧 execution 非当前事件仍可污染新状态 | `crates/central/tiangong-central/src/event_log.rs:589-648`：只有 Started 切换当前 execution，旧 Completed/WaitingUser/Progress/Failed/Cancelled 不影响投影 | `r29_t4_late_events_do_not_pollute_current_execution`（`tests/event_log.rs:657`），5 类迟到事件、日志仍保留，红转绿 |
| R29-4 接线补齐 | 严格投影后发现真 steer 仍写了新 execution Started，owner 完成被误判迟到；这是 execution 契约冲突，须一并修正 | `dispatch.rs:691-712`：真 steer 只写 owner execution 的 Progress，落盘恢复活跃 owner execution，不创建假执行 Started | `r29_4_steer_log_keeps_active_owner_execution`（`:3552`）：真实 Core 两次 dispatch→owner fire→日志重建 Completed/owner execution；实际先红 `Running != Completed`（`/tmp/r29-steer-red.log`），修后 1 passed（`/tmp/r29-steer-green.log`） |

### 7.3 正确性与文档缺口逐项对账
| 项 | 根因 / 改动 | 锁定测试与实际红转绿 |
|---|---|---|
| R29-5 / F4 | 子串可能命中 payload.id，且转义/空白漏查；`event_log.rs:389-417` 按字节行反序列化 `LoggedEvent`，仅比较真实 `event.id` | `r29_5_idempotency_compares_real_event_id`（`:690`），先红 offset 1!=2，后绿；覆盖嵌套同名 id、带引号/反斜杠/Unicode id、字段空白和重复追加 |
| R29-6 / F5 | `lines().map_while(Result::ok)` 遇非法 UTF-8 无计数即停；`event_log.rs:519-540` 改字节换行扫描，坏行计数后继续，真实 I/O 错误计数+warn；幂等扫描也跳坏行 | `r29_6_invalid_utf8_is_counted_and_read_continues`（`:719`），先红 skipped 0!=2，后绿；归档/当前各含坏行、后续三条完整可读，严格读仍错误，冷 append=4 |
| R29-7 / F6 | 注释称最大 offset，实际只取末条；`event_log.rs:296-300` 真正 `.max()` | `r29_7_cold_offset_uses_maximum_not_last_entry`（`:755`），归档90→3/当前15→2，先红 append=3!=91，后绿 |
| R29-8 | 第 29 轮已定义的生产缺陷：公开读取只持进程内 mutex，append/轮转只持稳定文件锁，缺少共同生命周期锁；上轮因中枢摘录遗漏未修，本轮已修复。原 D1/D2 文档对账是独立工作，不等同于 R29-8 | 四项严格/宽松 × 同进程独立实例/跨进程测试先红后绿；追溯表及实现位置见 §8 |
| R29-9 / F7 | 重建误从 Started payload 取问题，提取首个标记子串又可能是引用，cancel 未清问题；`central_events.rs:335-349` 从当前 execution WaitingUser 取 payload；`reports.rs:15-33` 独占协议行提取；`dispatch.rs:188-191` cancel 清空 | `r29_9_pending_question_rebuild_and_terminal_cleanup` / `r29_9_question_extraction_uses_standalone_protocol_line` / `r29_9_cancel_clears_pending_question`（`:3475/:3508/:3522`）：三项先红（None!=当前问题/误取引用/取消问题残留），修后 3 passed；旧 execution 迟到、多个恢复、Cancelled/Completed 清理均锁定 |
| P2 / F8 | 磁盘保留 owner task，内存 upsert 直接替换导致 work list 显示答复；`central-plugin/src/tools_dispatch.rs:272-295` 与磁盘同样按同 work_id 且未终态保留 task | 增强 `dispatch_work_reused_session_keeps_registration_clean`（`central-plugin/tests/tools.rs:1223,1277-1284`），直接断言 work list 的内存输出；先红显示“补充要求”（`/tmp/r29-memory-red.log`），后绿显示“原工作”（`/tmp/r29-memory-green.log`） |

F4～F6 先测 `/tmp/r29-log-gaps-red.log` 为 1 passed（T4）/3 failed，修后 `/tmp/r29-log-gaps-green.log` 为 **4 passed**。F7 的先红/后绿日志为 `/tmp/r29-question-red.log` 与 `/tmp/r29-question-green.log`。`/tmp` 是本机复核辅助，不是永久工件；永久复核依据为入库测试与本节精确断言记录。

### 7.4 全量门禁（本轮真实执行）
| 命令 | 结果 / 数字 |
|---|---|
| `cargo test -p tiangong-central -p central-state -p central-plugin -p central-host -p tiangong-app` | **308 passed，0 failed，0 ignored**；tiangong-central 48（lib1+event8+event_log23+ids2+risk2+status4+tickets8）、central-state 33（9+9+2+13）、central-plugin 40、central-host 11、tiangong-app 176（lib123+bin1+集成52）；doc tests 0。基线297，本轮新增11（领域4+宿主7），另增强1个既有插件测试。日志 `/tmp/r29-five.log` |
| `cargo clippy --workspace --all-targets --tests -- -D warnings` | 通过，0 Clippy error，71s；Cargo manifest 依赖声明 warning 仍存在，不宣称“全输出无 warning”。日志 `/tmp/r29-clippy.log` |
| `cargo fmt --check` | 通过（按实际最终执行核查） |
| `yarn vitest run --configLoader runner`（frontend） | **593 passed / 593，66 文件，0 failed**；日志 `/tmp/r29-vitest.log` |
| `npx tsc --noEmit`（frontend） | 通过，0 错误；日志 `/tmp/r29-tsc.log` |
| `git diff --check` | 通过；最终 Git 范围仅5个生产文件+3个测试文件+本报告/progress/plan，共11个已跟踪文件，未改锁文件/依赖。根目录 TODO.md 按项目约定被 `.gitignore:23` 忽略，仅同步本地工作清单，不强制纳入版本控制 |

首轮五包曾报旧测试 `host_dispatch_queues_on_write_lock_then_delivers` 失败：同 execution Queued 被视为“已推进不可回退”。据实际失败把 Queued 列入允许转 Running 的起始态，最终重跑全五包通过，不隐去这次回归发现。

Coding 的 context/preflight/review 三操作均返回“未知的 Coding 操作”（插件版本不支持），不是代码审核失败。已用仓库规则读取、Git 基线与改动范围、生产 diff 人工核查、上述真实门禁替代；不能声称自动 Coding review 已通过。无外部用户决策阻塞。

### 7.5 完成与未做
**本轮完成**：T1～T4 先红后绿、F1～F8、D1/D2、指定五项门禁与范围/diff 核查；不 push、不 commit。

**未做 / 仍有边界（不计本轮已闭合）**：
1. 实机 I3 与真实模型行为、无 Core 真实建核后的恢复全矩阵、投递失败回滚全矩阵、多进程阈值竞争轮转；本轮 deterministic harness 不冒充真实模型/装配验证。
2. R28 §6 的 UI 决定票据/授权/取消重派入口、事件刷新时序、结构化 outcome、C 系列工程债和完整跨层对账未扩大实现。
3. 异 work_id、异 execution、没有 owner_work_id 的旧 reused 日志无法无歧义识别“引导还是新工作”；保留最新条目，不猜测旧 owner。legacy 完全缺 execution 仍保守兼容；一级归档策略未改。
4. 本轮未执行全 workspace cargo test/nextest、cargo deny、覆盖率或实机；不得将五包308写为全 workspace308，也不将 R28 的旧验证冒充本轮。
5. R29-8 是第 29 轮已定义的生产缺陷；上轮因中枢任务摘录遗漏未修，现已在专项补修（§8）修复。

### 7.6 上轮交付时 Git 状态
分支/HEAD 仍为 `feature/central-refactor` / `3424cff0`，全部未暂存、无新提交、未 push：
```text
 M crates/central/central-host/src/host/central_events.rs
 M crates/central/central-host/src/host/dispatch.rs
 M crates/central/central-host/src/host/reports.rs
 M crates/central/central-plugin/src/tools_dispatch.rs
 M crates/central/central-plugin/tests/tools.rs
 M crates/central/tiangong-central/src/event_log.rs
 M crates/central/tiangong-central/tests/event_log.rs
 M docs/central/fix-report-28.md
 M docs/central/plan.md
 M docs/central/progress.md
 M src-tauri/tests/central_host_integration.rs
```
本地被忽略的 `TODO.md` 已同步当前工作检查清单，遵循忽略规则未强加追踪。

## 8. R29-8 专项补修（2026-11-18）
在 `feature/central-refactor` / `3424cff0` 原工作区 11 个未提交改动之上继续；未回退、未暂存、未 commit、未 push。

### 8.1 问题追溯与修复位置
| 字段 | 记录 |
|---|---|
| 问题 ID / 定义 | **R29-8，第 29 轮已定义的生产缺陷**。`append` 持稳定文件 `flock`（原 `event_log.rs:241-250`）跨 rename 和重开；严格与宽松公开读（原 `:477-506`）只持不约束 append 的进程内 `append_lock`，无法获得完整代际快照。 |
| 第 30 轮核查 / 上轮漏项 | 第 30 轮通过源码路径确认共同锁缺失，是当时唯一合入阻断；当时是路径推导，非动态复现。上轮中枢任务摘录遗漏了独立条目，§7 将其归为文档项有误；现已纠正并补修。 |
| 失败序列 | 初始归档 `[1]`、超阈值当前 `[2]` → reader 读完归档暂停 → writer append `3` 触发轮转，将 `2` 移到归档，再 append `4` → reader 打开新当前得到 `[1,3,4]`；混合了轮转前归档和轮转后当前，漏整代 `[2]`。 |
| 回归测试 | `crates/central/tiangong-central/src/event_log_tests.rs:158-175`：`r29_8_strict_snapshot_across_instances`、`r29_8_lenient_snapshot_across_instances`、`r29_8_strict_snapshot_across_processes`、`r29_8_lenient_snapshot_across_processes`。跨进程复用当前测试二进制的 `r29_8_rotation_child`（`:96`），常规套件中该辅助项为空操作。 |
| 锁协议 / 实现 | `crates/central/tiangong-central/src/event_log.rs:349-388` 统一打开不参与 rename 的 `.jsonl.lock`，RAII 关闭、EINTR 重试。严格/宽松读取持共享锁读取归档和当前的全过程（`:477-517,544-557`）；尾部修复与 append 持独占锁（`:101-118,229-346`）。标准库 Unix `File::lock` / `lock_shared` 仍用 `flock`；Windows 用 `LockFileEx`，不再静默无锁。无新增依赖，移除该 crate 不再直接使用的 `libc`。 |
| 锁序与读失败 | 先取生命周期锁，再访问日志或 cache；append 冷缓存重建和幂等查证调用 `*_locked` 内部读入口，不递归获取同一锁（`:253-286`）。宽松读仍跳过损坏行，但取锁失败返回 `Err`，宿主记录告警并回退工作快照（`crates/central/central-host/src/host/central_events.rs:238-249`）。 |
| 结论 | R29-8 已补修。四项用例都验证轮转中 reader 得到 `[1,2]`，锁释放后 writer 完成轮转，后续 reader 得到 `[2,3,4]`；精确比较 offset 和 id 序列，既不漏代也不重复。轮转后旧归档 `1` 被覆盖是既有一级归档策略。 |

### 8.2 TDD 证据
- 先加仅测试编译的归档/当前之间钩子及四个用例，不改生产锁。执行 `cargo test -p tiangong-central --lib r29_8_ -- --nocapture`：**4 failed，1 passed（子进程辅助项）**；四项实际 `[1,3,4]`、期望 `[1,2]`，非编译失败或超时。见 `/tmp/tiangong-r29-8/red.log`。
- 同一命令修后为 **5 passed，0 failed**（`/tmp/tiangong-r29-8/green.log`），最终五包再次全部跑过。屏障固定 reader 的暂停点；写端通过非阻塞独占取锁探测，不用休眠或等待超时来推断互斥。旧实现会在暂停窗口完成真实 `append`/轮转；修后探测收到明确的 `WouldBlock`，先放行 reader，再执行真实轮转。
- 既有 `tests/event_log.rs` **23/23 通过**，包含轮转、归档、幂等、冷启动、坏行及跨进程 offset 用例。

### 8.3 全量指定门禁与范围
| 命令 | 实测 |
|---|---|
| `cargo test -p tiangong-central -p central-state -p central-plugin -p central-host -p tiangong-app` | **313 passed，0 failed，0 ignored**，上轮 308；tiangong-central **53**（lib 6、其余 47）/ central-state **33** / central-plugin **40** / central-host **11** / tiangong-app **176**；doc tests 0。日志 `five.log` |
| `cargo clippy --workspace --all-targets --tests -- -D warnings` | 通过；既有 Cargo manifest 未使用依赖告警仍在。日志 `clippy.log` |
| `cargo fmt --check`、`git diff --check` | 通过 |
| `yarn vitest run --configLoader runner`（frontend） | **593/593，66/66 文件，0 失败**。日志 `vitest.log` |
| `npx tsc --noEmit`（frontend） | 通过，0 错误。日志 `tsc.log` |
| `cargo check --offline --target x86_64-pc-windows-msvc -p tiangong-central --tests` | 交叉检查通过，不代表在 Windows 跑过并发用例。日志 `windows-check.log` |

以上日志位于 `/tmp/tiangong-r29-8/`，仅为本机复核辅助。Coding 插件不支持 preflight/review，本轮以实际 Git 基线、逐文件范围比对、人工 diff 核查和上述门禁替代，未将自动审查记作通过。当前验证覆盖读者与单写者跨代际快照；不等于已验收多写者在阈值边界竞争轮转，也未做轮转失败注入、实机 I3 或全 workspace 测试。现场并发测试运行环境为 macOS。

### 8.4 交付 Git 状态
`feature/central-refactor` / `3424cff0`；13 个已跟踪文件未暂存修改、1 个新增测试文件未跟踪。在 §7.6 所列 11 个文件之外，新增 `crates/central/tiangong-central/src/event_log_tests.rs`（测试专用），并修改该 crate 的 `Cargo.toml` 和根 `Cargo.lock`（仅移除其直接 `libc` 依赖）。无暂存、无新提交、未推送。
