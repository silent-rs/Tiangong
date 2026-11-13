//! 后台执行归属契约（重构方案 §5.4，阶段 R0 冻结）。
//!
//! POC 的终态归属依赖「当前 session 最近一条 registry」，快速完成与
//! 迟到回调都可能错挂。R0 起，每次实际执行（首次派发 / 恢复 / 重试 /
//! 排队重放 / 引导）都必须生成唯一的 `execution_id`，并随派发请求、
//! 状态登记、事件时间线与日志一起流动，保证任何后台工作可在日志与
//! 持久化记录中唯一定位。
//!
//! 契约版本（`CENTRAL_CONTRACT_SCHEMA_VERSION`）随 schema 演进递增；
//! 已持久化数据按「未知字段兼容、缺失字段回退」读取。

use serde::{Deserialize, Serialize};

use crate::ids::{new_id, now_text};

/// 中枢契约 schema 版本（R0 冻结为 1）。
///
/// 事件、执行、登记记录的序列化格式向后兼容：新增字段一律
/// `#[serde(default)]`，读取旧数据不失败；破坏性变更必须递增本版本
/// 并提供迁移。
pub const CENTRAL_CONTRACT_SCHEMA_VERSION: u32 = 1;

/// 执行前缀：execution_id 统一为 `exec-<scru128>`，日志中一眼可辨。
pub const EXECUTION_ID_PREFIX: &str = "exec-";

/// 生成新的 execution_id。
pub fn new_execution_id() -> String {
    format!("{EXECUTION_ID_PREFIX}{}", new_id())
}

/// 一次后台执行的归属记录（§5.4）。
///
/// ```text
/// work_item_id   用户层工作（POC 现名 work_id）
/// id             一次实际执行 / 恢复 / 重试（execution_id）
/// session_id     Worker 对话上下文
/// workspace_id   权限与写入锁边界（POC 现为 cwd）
/// attempt        同一 work_item 的第几次尝试（从 1 起）
/// ```
///
/// 状态仍由现有状态机（`WorkItemStatus` / `WorkState`）承载；本结构
/// 只做归属，不引入第三套状态机。事件的 `execution_id` 字段把
/// `Started / Progress / WaitingUser / Resumed / Done / Failed / Cancelled`
/// 唯一挂到执行上，终态回调不再猜测归属。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Execution {
    pub id: String,
    pub work_item_id: String,
    pub session_id: String,
    pub workspace_id: String,
    pub attempt: u32,
    pub created_at: String,
}

impl Execution {
    /// 创建一次执行。`attempt` 由调用方按该 work_item 的历史执行数
    /// 传入（首次为 1；恢复 / 重试 / 重放递增）。
    pub fn new(
        work_item_id: impl Into<String>,
        session_id: impl Into<String>,
        workspace_id: impl Into<String>,
        attempt: u32,
    ) -> Self {
        Self {
            id: new_execution_id(),
            work_item_id: work_item_id.into(),
            session_id: session_id.into(),
            workspace_id: workspace_id.into(),
            attempt,
            created_at: now_text(),
        }
    }

    /// 是否为格式完整的执行记录（非空 id 与归属字段）。
    pub fn is_well_formed(&self) -> bool {
        !self.id.is_empty()
            && !self.work_item_id.is_empty()
            && !self.session_id.is_empty()
            && !self.workspace_id.is_empty()
            && self.attempt >= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_ids_are_unique_and_prefixed() {
        let a = Execution::new("work-1", "sess-1", "/tmp/ws", 1);
        let b = Execution::new("work-1", "sess-1", "/tmp/ws", 2);
        assert!(a.id.starts_with(EXECUTION_ID_PREFIX));
        assert_ne!(a.id, b.id, "连续生成的 execution_id 不得重复");
        assert_eq!(a.attempt, 1);
        assert_eq!(b.attempt, 2);
    }

    #[test]
    fn execution_is_well_formed_after_creation() {
        let execution = Execution::new("work-1", "sess-1", "/ws", 1);
        assert!(execution.is_well_formed());

        let mut broken = execution.clone();
        broken.session_id = String::new();
        assert!(!broken.is_well_formed());
    }

    #[test]
    fn execution_round_trips_through_json() {
        let execution = Execution::new("work-1", "sess-1", "/ws", 3);
        let json = serde_json::to_string(&execution).expect("序列化失败");
        let parsed: Execution = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(parsed, execution);
        assert!(json.contains("\"attempt\":3"), "attempt 应参与序列化");
    }

    #[test]
    fn schema_version_is_frozen_at_r0() {
        assert_eq!(CENTRAL_CONTRACT_SCHEMA_VERSION, 1);
    }
}
