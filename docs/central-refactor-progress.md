# 中枢模式重构计划

> 分支：`feature/central-refactor`（worktree `.worktrees/central-refactor`）
> 本文档由本地 PLAN.md 迁入版本控制（第十八轮 Z3④）——重构分支的进度
> 与验证基线以本文档与 docs/central-refactor-plan.md 附录 A 为准。
> 基线：`develop/central-mode`（设计定稿，Issue #773）+ POC 实现（`poc/central-mode` 合入）
> 方案：`docs/central-refactor-plan.md`（阶段 R0～R5）｜设计：`docs/central-mode-design-draft.md`

## 当前状态（2026-11-13）

R0～R5 全部完成；消息卡片第一片完成（授权卡片票据闭环 + 决定卡片）。
剩余：实机验收十步（需真实桌面环境）与可选的 ReportCard 折叠展示。
全部提交独立可编译、可测试、可回滚。

| 阶段 | 状态 | 提交 | 说明 |
| --- | --- | --- | --- |
| R0 冻结契约与观测 | ✅ 完成 | `bbbb67f9` | Execution 契约、execution_id 三处一致（登记/落盘/时间线）、观测日志、旧数据兼容与行为基线测试 |
| R1 拆分宿主控制器 | ✅ 完成 | `34e65b20` `563fe2e0` `dcc248d5` | ExecutionRouter 终态路由（快速完成暂存补发/失败取消/stale 清理）；app.rs 1371 行拆入 `central_host/` 六模块；central-plugin 拆出 persona/tool_specs/workspace_summary |
| R2 事件日志与事务存储 | ✅（轮转读回补齐，十九轮） | `d4bb144a` | append-only 事件日志（崩溃恢复/幂等/投影）、持久层事务加固（随机后缀+写锁+fsync）、旧时间线迁移、双写接线 |
| R3 权限票据化 | ✅ 完成 | `1ba43261` `ee5a35a7` | 票据契约与注册表；移除 confirm_workspace（模型不可自证同意）；宿主签发命令（approve_workspace / resolve_decision）；派发双通道校验；parallel 高风险豁免收紧 |
| R4 ModeCoordinator 与前端 | ✅ | `120ef2bb` `1cf0fe61` | 宿主模式状态机 + 快照归还 + 偏好持久化 + 前端接线（宿主快照优先，本地兜底过渡）；前端工作视图（抽屉/卡片）待做 |
| R5 清理 POC 兼容层 | ✅ | `0ea3293e` `fd7f9cab` `d0ace0a7` `13704405` `5cd27fb6` | 时间线双写消除（事件日志唯一事实源、旧数据迁移+兼容读）；插件压缩/快照 envelope/实机验收待做 |

## 验证基线（2026-11-16 二十一轮修复后刷新）
- `cargo clippy --workspace --all-targets -- -D warnings` 退出 0（CI 口径）
- `cargo test -p tiangong-central -p central-state -p central-plugin -p central-host -p tiangong-app` 全绿（36 + 23 + 39 + 11 + 168；workspace 全量约 730 项）
- 覆盖率（llvm-cov）：核心三包 94.41%（分支 84.41%）；central-host 全文件 ≥70%（mode_commands/work_view 100%、authorization 87%、core_lifecycle 83%、central_events 85%）
- 前端：`tsc --noEmit` 0 错误；Vitest 586/586（65 文件，中枢 12 文件 94 用例）
- clippy / fmt 干净（仓库余 warning 均为 POC 既有 workspace 依赖声明，非本分支引入）
- 前端已完成 R4/R6b：模式切换/工作视图/抽屉/卡片 + features/central 特性模块化
## R4 剩余要点（方案 §4 / §6）

1. DecisionTicket 消费点接线：等待用户 → 用户选择 → 恢复同一 execution 的链路校验（决定卡片触发）。
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
