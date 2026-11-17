# 第 28 轮 review 修复报告（R28-1～8 + 五组闭环）

> 性质：修复实施报告，供第 29 轮 review 作对照依据。不是完成度宣传——每项含修复位置、锁定测试、已知边界与未完成清单。
>
> 修复基线：`f70988d7`（第 28 轮 review 快照前：嗅探行首匹配 + 同会话互斥三豁免 + 通知克制 persona——本轮 review 已认可的方向延续）。
> 完成快照：`e46c2068`（本文档 HEAD）。
> 依据文档：`docs/central-refactor-review-and-fix-plan.md`（第 28 轮 review，问题 ID 沿用其编号）。

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
| 恢复链 | ✅ 自动测试层闭合 | 生产 owner id + 新 execution：答复送达（含真实答复文本断言）→ register/attach → fire 终态 → 回报入队。**模型行为实测（及时转发/追问判断）待 I3** |
| 日志链 | ✅ 自动测试层闭合 | 短行/长行/幂等（同进程+跨进程）/轮转/重启/中段损坏/归档恢复各有独立测试。**多进程阈值边界竞争未测** |
| 状态链 | ✅ 主体闭合 | 首轮完成/空闲复用新 id/排队重放/重启中断迁移有测试；「取消后新工作不被吸收」投影层已锁，全链对账未单独测 |
| UI 链 | ✅ 主体闭合 | StepChain 执行隔离、pending_question 展示、activeCount 口径；**状态变化后的刷新时序（集中事件订阅失效缓存）未做**（U1 残余） |
| 集成链 | ✅ | merge main（DeepSeek 无回退）+ 全量验证（见 §4）；**实机 I3 未做** |

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
