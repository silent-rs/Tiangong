//! 中枢模式（central mode）宿主支持：中枢对话的独立执行域。
//!
//! 设计稿（`docs/central-mode-design-draft.md`）对中枢对话的三条硬边界：
//! - **单独存储**（§13.1 `central_conversation`）：中枢对话不与普通 Session
//!   混存——独立 `storage_root`（`{storage_root}/central/sessions/`），
//!   `get_sessions` / 删除 / 回收站 / 元数据刷新等普通会话链路天然不触达；
//! - **权限隔离**（§5.2 / §5.3）：中枢助手不直接执行工作型工具（fs /
//!   terminal / browser / computer-use 等全部排除）。隔离由宿主在 Core
//!   构造点强制（插件集为空），不靠提示词约束；§5.3 的白名单工具集
//!   （`request_user` 等）随中枢编排后续片接入；
//! - **对话链路复用**（§16）：执行面仍是 TiangongCore（流式、压缩、模型
//!   解析、持久化），由独立的 CoreManager 管理，模型等全局配置与普通
//!   manager 共享同一 provider。

/// 中枢对话的固定 Session ID。
///
/// 中枢对话单一存在（§13.1 `central_conversation`；开放问题 2「全局唯一还是
/// 每用户一个」POC 取全局唯一）：文件为 `{storage_root}/central/sessions/
/// central-hub.json`，首次发送时创建，此后跨重启延续。前端使用同名常量
/// （`frontend/src/lib/centralSession.ts`）。
pub const CENTRAL_SESSION_ID: &str = "central-hub";

/// 中枢对话存储根：独立于普通会话目录。
pub fn central_storage_root(base: &std::path::Path) -> std::path::PathBuf {
    base.join("central")
}

/// 该 session 是否为中枢对话。
pub fn is_central_session(session_id: &str) -> bool {
    session_id == CENTRAL_SESSION_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn central_session_id_matches_frontend_constant() {
        // 与前端 frontend/src/lib/centralSession.ts 的 CENTRAL_SESSION_ID 一致；
        // 变更需两侧同步（历史文件按 ID 命名，改值会丢弃既有中枢对话）。
        assert_eq!(CENTRAL_SESSION_ID, "central-hub");
    }

    #[test]
    fn central_storage_root_is_isolated_subdir() {
        let root = central_storage_root(std::path::Path::new("/data/tiangong"));
        assert_eq!(root, std::path::PathBuf::from("/data/tiangong/central"));
    }

    #[test]
    fn is_central_session_routes_only_hub() {
        assert!(is_central_session(CENTRAL_SESSION_ID));
        assert!(!is_central_session("0198abcd"));
        assert!(!is_central_session(""));
    }

    /// 中枢对话路由到独立 manager：storage_root 与普通会话目录分离。
    #[test]
    fn central_manager_is_routed_and_isolated() {
        let app = crate::app::TiangongApp::new();
        assert!(std::ptr::eq(
            app.manager_for(CENTRAL_SESSION_ID),
            &app.central_core_manager
        ));
        assert!(std::ptr::eq(app.manager_for("0198abcd"), &app.core_manager));
        // 独立存储根：中枢会话文件不落入普通 sessions 目录。
        assert_ne!(
            app.manager_for(CENTRAL_SESSION_ID).storage_root(),
            app.core_manager.storage_root()
        );
    }
}
