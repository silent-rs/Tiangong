//! 中枢模式（central mode）宿主支持。
//!
//! **中枢对话是同一个 CoreManager 管理下的特殊 Core**，不是独立管理器：
//! 中枢未来要派发、唤醒、暂停、取消后台工作（Worker 同样由 CoreManager
//! 托管），必须共用同一套生命周期与编排入口（创建锁、空闲回收、投递、
//! 取消、模型解析）。中枢的特殊性只有三处，全部落在正确的层次：
//!
//! - **单独存储**：路径规则内建于 `tiangong_core::session::Session::
//!   session_file_path`（`{storage_root}/central/sessions/`，读 / 写 /
//!   元数据 / 回收站共用）；会话列表与按组删除只扫 `sessions/`，天然
//!   不触达中枢对话（设计稿 §13.1）；
//! - **插件白名单**：宿主在 `TiangongApp::ensure_core` 的构造点对中枢
//!   返回空插件集（§5.2 权限隔离由系统强制；§5.3 白名单工具集随中枢
//!   编排后续片接入）；
//! - **固定 Session ID**：常量定义于 `tiangong_types::central_session`
//!   （前端 `frontend/src/lib/centralSession.ts` 使用同名常量）。
//!
//! 本模块只保留宿主侧文档与跨层一致性守护测试。

pub use tiangong_types::central_session::{is_central_session, CENTRAL_SESSION_ID};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn central_session_id_matches_frontend_constant() {
        // 与前端 frontend/src/lib/centralSession.ts 的 CENTRAL_SESSION_ID 一致；
        // 变更需两侧同步（历史文件按 ID 命名，改值会丢弃既有中枢对话）。
        assert_eq!(CENTRAL_SESSION_ID, "central-hub");
        assert!(is_central_session(CENTRAL_SESSION_ID));
        assert!(!is_central_session("0198abcd"));
        assert!(!is_central_session(""));
    }

    /// 中枢会话文件单独存储，且由单一 CoreManager 统一解析路径：
    /// 读（load_from_storage）、写（try_persist_to_disk）与 CoreManager
    /// 侧共用 `Session::session_file_path`，普通会话不受影响。
    #[test]
    fn central_session_file_is_isolated_but_single_manager() {
        use tiangong_core::session::Session;
        let root = std::path::Path::new("/data/tiangong");
        assert_eq!(
            Session::session_file_path(root, CENTRAL_SESSION_ID).unwrap(),
            std::path::PathBuf::from("/data/tiangong/central/sessions/central-hub.json")
        );
        assert_eq!(
            Session::session_file_path(root, "0198abcd").unwrap(),
            std::path::PathBuf::from("/data/tiangong/sessions/0198abcd.json")
        );
        // 非法 ID 仍被拒绝（路径穿越防护不因中枢规则放宽）
        assert!(Session::session_file_path(root, "../evil").is_err());
        assert!(Session::session_file_path(root, "a/b").is_err());
    }

    /// 端到端读写一致性：中枢 Session 持久化到独立目录，且能按同一路径
    /// 加载回来；普通会话目录（sessions/）不含中枢文件。
    #[test]
    fn central_session_roundtrip_in_isolated_dir() {
        use tiangong_core::session::Session;
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new("中枢对话");
        session.id = CENTRAL_SESSION_ID.to_string();
        session.bind_storage_root(dir.path());
        session.try_persist_to_disk().unwrap();

        let central_file = dir.path().join("central/sessions/central-hub.json");
        assert!(central_file.exists());
        assert!(!dir.path().join("sessions/central-hub.json").exists());

        let loaded = Session::load_from_storage(dir.path(), CENTRAL_SESSION_ID).unwrap();
        assert_eq!(loaded.id, CENTRAL_SESSION_ID);
    }
}
