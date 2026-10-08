//! 进程内插件自注册架构（issue #156）目录化。
//!
//! 每个 [`Plugin`] 封装自己的全部能力，在 engine 创建/重建时自行向
//! [`RuntimeEngine`] 注册，消除三方中转仓库模式。
//!
//! 设计要点：
//! - [`Plugin`] 是能力的声明式聚合：通过 supertrait 约束同时要求实现工具规格、
//!   工具覆盖与 Prompt 段落三种能力（均提供默认空实现，插件按需覆写）。
//! - core 在收集 tool_specs 前会先通过 [`Plugin::set_workspace`] 注入当前会话
//!   工作目录、通过 [`Plugin::set_trust_mode`] 注入会话信任解析句柄，并在配置变更
//!   时调 [`Plugin::on_config_updated`] 让插件初始化内部状态。TurnContext 构建后，
//!   `spawn_turn` 再通过 [`Plugin::set_feedback_tx`] 注入本轮反馈通道。
//! - `PageFetcher` / `TerminalProvider` 等外部能力 trait 已随能力下沉重构（#225）
//!   迁入对应插件 crate，core 不再持有这些 trait；插件经 [`Plugin::set_feedback_tx`]
//!   注入的通道自行向会话投递外部事件（如浏览器页面快照）。
//!
//! 模块组织：
//! - [`trait_def`]：`Plugin` trait 定义与信任模式/反馈默认能力。
//! - [`feedback`]：插件状态反馈通道（`PluginFeedback` + `PluginFeedbackTx`）。
//! - [`notify`]：收尾钩子（on_turn_finished / on_session_ended）的统一后台通知投递，
//!   含 on_turn_finished 的本轮只读快照组装。
//! - [`registry`]：`register_plugin` 编排逻辑（core 在 engine 创建时遍历调用）。
//! - [`injection`]：插件事件注入通道（synthetic tool）的工具规格。
//! - [`tool_spec`]：插件基础设施相关的工具名常量集中点。
//!
//! 注意：本模块与根 `crate::plugin`（外部清单驱动插件）是两套不同的机制，不要混淆。

mod feedback;
mod injection;
mod notify;
mod registry;
mod tool_spec;
mod trait_def;

pub use feedback::{PluginFeedback, PluginFeedbackTx};
pub use trait_def::Plugin;

pub(crate) use injection::injection_tool_spec;
pub(crate) use notify::{notify_session_ended, notify_turn_finished};

pub(crate) use registry::prepare_plugins;
