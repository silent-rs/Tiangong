# 中枢模式重构计划

> 分支：`feature/central-refactor`（worktree `.worktrees/central-refactor`）
> 本文档由本地 PLAN.md 迁入版本控制（第十八轮 Z3④）——重构分支的进度
> 与验证基线以本文档与 docs/central/plan.md 附录 A 为准。
> 基线：`develop/central-mode`（设计定稿，Issue #773）+ POC 实现（`poc/central-mode` 合入）
> 方案：`docs/central/plan.md`（阶段 R0～R5）｜设计：`docs/central/design-draft.md`

## 当前状态（2026-11-18 R29-8 专项补修后刷新）
R0～R5 主体完成；R29 的 Interrupted 重派、attach 同步重放、owner 重建、迟到 execution 隔离及日志幂等/非法 UTF-8/最大 offset、pending_question/内存 owner task 已按 TDD 修复。R29-8 的严格/宽松读与 append/轮转已共用稳定生命周期锁，并验证了同进程独立实例及跨进程的读-轮转快照。详细红转绿、实测与已知边界见 `docs/central/fix-report-28.md` §7–8；五组链路均为“自动回归通过但仍有缺口”，不宣称全闭合。
剩余：I3 实机回归、真实模型与失败回滚完整矩阵、多写者在阈值边界竞争轮转及轮转失败注入、结构化 outcome、票据/授权/取消重派 UI、事件刷新时序和 C 系列工程债。main 合并为 R28 历史事实，本轮不重复声称 I2 未做。
本轮改动留工作区供用户审阅，未 commit、未 push。
| 阶段 | 状态 | 提交 | 说明 |
| --- | --- | --- | --- |
| R0 冻结契约与观测 | ✅ 完成 | `bbbb67f9` | Execution 契约、execution_id 三处一致（登记/落盘/时间线）、观测日志、旧数据兼容与行为基线测试 |
| R1 拆分宿主控制器 | ✅ 完成 | `34e65b20` `563fe2e0` `dcc248d5` | ExecutionRouter 终态路由（快速完成暂存补发/失败取消/stale 清理）；app.rs 1371 行拆入 `central_host/` 六模块；central-plugin 拆出 persona/tool_specs/workspace_summary |
| R2 事件日志与事务存储 | ✅（轮转读回补齐，十九轮） | `d4bb144a` | append-only 事件日志（崩溃恢复/幂等/投影）、持久层事务加固（随机后缀+写锁+fsync）、旧时间线迁移、双写接线 |
| R3 权限票据化 | ✅ 完成 | `1ba43261` `ee5a35a7` | 票据契约与注册表；移除 confirm_workspace（模型不可自证同意）；宿主签发命令（approve_workspace / resolve_decision）；派发双通道校验；parallel 高风险豁免收紧 |
| R4 ModeCoordinator 与前端 | ✅ | `120ef2bb` `1cf0fe61` | 宿主模式状态机 + 快照归还 + 偏好持久化 + 前端接线（宿主快照优先，本地兜底过渡）；前端工作视图（抽屉/卡片）待做 |
| R5 清理 POC 兼容层 | ✅ | `0ea3293e` `fd7f9cab` `d0ace0a7` `13704405` `5cd27fb6` | 时间线双写消除（事件日志唯一事实源、旧数据迁移+兼容读）；插件压缩/快照 envelope/实机验收待做 |

## 验证基线（2026-11-18 R29-8 工作区实测）
- `cargo clippy --workspace --all-targets --tests -- -D warnings` 通过；0 Clippy error，仍有 Cargo manifest 依赖声明 warning。
- `cargo test -p tiangong-central -p central-state -p central-plugin -p central-host -p tiangong-app` **313/313**（tiangong-central 53 / central-state 33 / central-plugin 40 / central-host 11 / tiangong-app 176=lib123+bin1+集成52）；上轮 308，本轮新增 4 项快照回归和 1 个子进程辅助项，R28 历史基线 297。不称全 workspace 测试。
- 覆盖率：下项为历史测量，本轮未重测；核心三包94.41%（分支84.41%），central-host全文件≥70%。
- 前端：`npx tsc --noEmit` 0错误；`yarn vitest run --configLoader runner` **593/593（66文件）**。
- `cargo fmt --check` / `git diff --check` 通过；原始日志路径与红转绿断言见修复报告§7–8。
- 前端已完成 R4/R6b：模式切换/工作视图/抽屉/卡片+特性模块化；票据决定/授权等缺口见下文，不以已有UI笼统代称闭合。
## R4 剩余要点（方案 §4 / §6）

1. DecisionTicket 消费点接线：等待用户→用户选择→恢复新 execution 的链路校验（决定卡片触发；真 steer 并入才保持当前 execution）。
2. 前端授权卡片（consume approve_workspace 命令）与决定卡片（consume resolve_decision）。
3. 工作抽屉、报告卡片、Supervisor 活动时间线（方案 §6.1 组件树）。
4. ModeSnapshot 前端消费 + 快速切换/恢复失败交互（§8.5 测试矩阵）。

## R4 要点（方案 §6）

- 宿主 `ModeCoordinator` 持有模式状态机（§4.2），前端消费 ModeSnapshot；`preCentralContext` 移入宿主。
- 组件树：CentralWorkDrawer / UserDecisionCard / ReportCard / SupervisorActivityBlock。
- 授权卡片（WorkspaceApproval）与决定卡片（Decision）经 R3 票据签发。
- 前端测试矩阵见方案 §8.5。

## R5 要点

- 删除 `WorkItemRecord.events` 时间线双写与 `append_work_item_event`（事件日志为唯一事实源）。
- 删除 R0 前旧字段兼容（execution_id None 分支）。
- `central-plugin` 目标 300～500 行（工具执行适配继续拆分）。
- 快照 envelope 化（schema 版本包裹）与 `CentralStore` trait 完整化（commit_snapshot/recover）。
- 更新设计文档实现态注记、实机验收记录（方案 §8.6）。

## 一次性 Workspace 授权票据修复（2026-11-25，分支 `fix/central-workspace-approval-ticket`）
- 现象：中枢派发被拒 → 用户点「授权」（仅此一次）→ 重派同一任务仍被拒「尚未经用户授权」，后台工作（如 review）派不出去；只有点「记住」才行。
- 根因：中枢插件在交给宿主前按落盘的长期授权预检，看不到宿主内存里的一次性票据，授权后的重派在插件层即被短路；宿主入队即消费票据，排队项投递时无票可用。
- 修复：授权只在宿主执行（插件预检移除）；入队只查验、执行时消费、投递未成立退还；拒绝文案与 persona 改为「卡片已自动发出，不必再调 request_workspace_approval」；相似历史强制复用跳过无会话文件的登记。端到端回归：派发 → 拒绝 → 真实授权命令 → 同任务重派 → 放行。
- 验证：fmt / clippy -D warnings / nextest 五 crate 通过 / `git diff --check`。
