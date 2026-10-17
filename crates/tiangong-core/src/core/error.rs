//! TiangongCore 的错误类型。
//!
//! 用于 [`crate::core::TiangongCoreBuilder::build`]、
//! [`crate::agent_input::AgentInput::deliver`] 与
//! [`crate::core::TiangongCore::into_session`]、
//! [`crate::core::TiangongCore::shutdown_join`] 的失败语义，
//! 让调用方能够区分失败原因，而非依赖布尔值或静默兜底。

use std::fmt;

/// TiangongCore 构造与运行期错误。
///
/// 变体按实际可观测的失败原因收敛（不预留当前架构无法触达的状态）。
#[derive(Debug)]
pub enum CoreError {
    /// Builder 缺少必填字段（如 config/session/stream_tx/storage）。
    MissingBuilderField(&'static str),
    /// worker 已停止，命令通道已关闭——`deliver` 无法投递命令。
    WorkerStopped,
    /// 当前 Core 正在执行任务，不能同时执行空闲期维护操作。
    Busy,
    /// worker 线程 panic，会话不可恢复，关闭并等待 worker 的操作失败。
    WorkerPanicked,
    /// 上下文整理已启动但未完成（模型调用失败、应用摘要失败或任务异常退出）。
    ///
    /// 返回此错误时整理已进入终态，`ContextCompressed { Failed }` 已写入会话
    /// 事件流。
    ContextCompactionFailed(String),
    /// 上下文整理被取消或被引导消息中断；`ContextCompressed { Cancelled }` 已
    /// 写入会话事件流。
    ContextCompactionInterrupted,
    /// 模型切换失败（目标端点不可用等）。
    ModelSwitchFailed(String),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::MissingBuilderField(name) => {
                write!(f, "Builder 缺少必填字段：{name}")
            }
            CoreError::WorkerStopped => write!(f, "worker 已停止，命令通道已关闭"),
            CoreError::Busy => write!(f, "Core 正在执行，当前操作仅允许在空闲时进行"),
            CoreError::WorkerPanicked => write!(f, "worker 线程 panic，会话不可恢复"),
            CoreError::ContextCompactionFailed(reason) => {
                write!(f, "整理上下文未完成：{reason}")
            }
            CoreError::ContextCompactionInterrupted => write!(f, "整理上下文被中断"),
            CoreError::ModelSwitchFailed(reason) => write!(f, "模型切换失败：{reason}"),
        }
    }
}

impl std::error::Error for CoreError {}
