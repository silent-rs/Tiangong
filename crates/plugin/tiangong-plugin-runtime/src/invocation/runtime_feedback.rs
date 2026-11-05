//! 调用层对宿主 UI 与 App 实例的出口。
//!
//! 工具执行的进度消息可能携带 Runtime 控制指令（`app.open` / `app.close`），
//! TS 工具还需要查询页面订阅者并向页面推送 `tool.requested` / `tool.closed`。
//! 这些动作的落地实现属于宿主集成层（桥接路由与宿主注入的原生服务），
//! 调用层只依赖本 trait：桥接层在宿主启动时注册实现
//! （[`crate::bridge::install_runtime_feedback`]），未注册时调用层按「宿主
//! 未接入」的既有行为退化（不消费控制反馈、不发页面事件、不拉起 App），
//! 因此调用层不再引用桥接模块。
use anyhow::Result;
use std::sync::{Arc, OnceLock};
/// 调用层需要的宿主反馈出口。
pub trait RuntimeFeedback: Send + Sync + std::fmt::Debug {
    /// 处理进度消息里的 Runtime 控制反馈；返回 `true` 表示已消费该消息。
    fn handle_progress(&self, plugin_id: &str, message: &str) -> bool;
    /// 插件页面是否订阅了该通道。
    fn has_subscriber(&self, plugin_id: &str, channel: &str) -> bool;
    /// 向插件页面推送事件。
    fn emit_to_plugin(&self, plugin_id: &str, channel: &str, payload: &str);
    /// 宿主内部请求打开插件自己的 App 实例。
    fn open_app(&self, plugin_id: &str, payload: &str) -> Result<String>;
}
fn feedback_slot() -> &'static OnceLock<Arc<dyn RuntimeFeedback>> {
    static SLOT: OnceLock<Arc<dyn RuntimeFeedback>> = OnceLock::new();
    &SLOT
}
/// 注册宿主反馈出口（宿主启动时调用一次；重复调用以首次为准）。
pub fn set_runtime_feedback(feedback: Arc<dyn RuntimeFeedback>) {
    let _ = feedback_slot().set(feedback);
}
/// 当前反馈出口；宿主未接入桥接时为 `None`。
pub fn runtime_feedback() -> Option<Arc<dyn RuntimeFeedback>> {
    feedback_slot().get().cloned()
}
