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

use crate::ids::new_id;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_is_frozen_at_r0() {
        assert_eq!(CENTRAL_CONTRACT_SCHEMA_VERSION, 1);
    }
}
