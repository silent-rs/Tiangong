//! 中枢对话（central conversation）的跨 crate 契约。
//!
//! 中枢对话是**同一个 CoreManager 管理下的特殊 Core**：统一的生命周期
//! （创建锁、空闲回收、投递、取消、模型编排），特殊性只有两点——
//! 会话文件单独存储（`{storage_root}/central/sessions/`，路径规则内建于
//! `tiangong_core::session::Session::session_file_path`），以及宿主在
//! Core 构造点注入的白名单插件集（src-tauri `central.rs`）。
//!
//! 前端使用同名常量（`frontend/src/lib/centralSession.ts`），两侧测试锁定一致。

/// 中枢对话的固定 Session ID。
///
/// 设计稿 §13.1 `central_conversation`（开放问题 2「全局唯一还是每用户一个」
/// POC 取全局唯一）：单一中枢对话，首次发送时创建，此后跨重启延续。
/// 变更该值会丢弃既有中枢对话历史（文件按 ID 命名）。
pub const CENTRAL_SESSION_ID: &str = "central-hub";

/// 该 session 是否为中枢对话。
pub fn is_central_session(session_id: &str) -> bool {
    session_id == CENTRAL_SESSION_ID
}
