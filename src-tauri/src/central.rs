//! 中枢模式（central mode）宿主支持：中枢对话的独立执行域。
//!
//! **中枢对话直接由 TiangongCore 承载，不经 CoreManager**：中枢需要单独
//! 内置且唯一的中枢插件（[`CentralPlugin`]，承载中枢助手人设与 §5.3 白
//! 名单工具集），CoreManager 的插件构造（runtime 聚合桥）与编排不适配；
//! 中枢 Core 由 `TiangongApp` 直接构造、持有与投递（见 `app.rs` 的
//! `ensure_central_core` 等方法）。
//!
//! 其余边界不变：
//! - **单独存储**：路径规则内建于 `tiangong_core::session::Session::
//!   session_file_path`（`{storage_root}/central/sessions/`，读 / 写 /
//!   元数据共用）；会话列表与按组删除只扫 `sessions/`，天然不触达；
//! - **固定 Session ID**：常量位于 `tiangong_types::central_session`
//!   （前端 `frontend/src/lib/centralSession.ts` 同名常量）。

pub use tiangong_types::central_session::{is_central_session, CENTRAL_SESSION_ID};

/// 中枢助手人设（system prompt 段落，设计稿 §5.1 / §5.2 的 POC 收敛版）。
///
/// 中枢是控制面与交互面：理解目标、澄清需求、组织回答与计划；不直接
/// 执行工作型任务。§5.3 白名单工具集（后台工作协调、征询、通知等）随
/// 中枢编排后续片在本插件上扩展。
const CENTRAL_ASSISTANT_PROMPT: &str = "\
你是天工中枢——用户的个人超级智能助手，是所有工作的统一入口与协调者。

你的职责：
- 理解用户的目标与约束，必要时澄清，给出简洁的计划或建议；
- 组织回答与汇总，区分「已确认的事实」「你的推测」「你的建议」；
- 当前版本你以对话为主：帮助用户理清目标、拆解步骤、准备后续交给后台执行的方案。

你的边界（由系统强制，不依赖你的自觉）：
- 你不直接执行工作型任务：不读写文件、不运行命令、不操作浏览器或数据库；
- 没有工具可用时如实说明，不虚构执行结果；
- 回答使用中文，保持简洁。";

/// 中枢助手内置插件：中枢 Core 唯一挂载的插件。
///
/// POC 阶段只注入人设 prompt 段落；§5.3 白名单工具集（`request_user`、
/// 后台工作协调等）随中枢编排后续片在 [`Plugin`](tiangong_core::core::Plugin)
/// 的工具能力上扩展。
pub struct CentralPlugin;

impl tiangong_core::core::Plugin for CentralPlugin {
    fn id(&self) -> &str {
        "central"
    }
}

impl tiangong_core::tools::extension::ToolSpecProvider for CentralPlugin {}
impl tiangong_core::tools::extension::ToolOverrideHandler for CentralPlugin {}
impl tiangong_core::tools::extension::PromptSectionProvider for CentralPlugin {
    fn prompt_sections(&self) -> Vec<String> {
        vec![CENTRAL_ASSISTANT_PROMPT.to_string()]
    }
}

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

    /// 中枢会话文件单独存储，且路径规则单点维护（读/写/元数据共用）。
    #[test]
    fn central_session_file_is_isolated_but_single_rule() {
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

    /// 端到端读写一致性：中枢 Session 持久化到独立目录并可按同一路径加载。
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

    /// 中枢插件注入人设段落，且不声明任何工具（§5.2 权限隔离）。
    #[test]
    fn central_plugin_injects_persona_without_tools() {
        use tiangong_core::core::Plugin;
        use tiangong_core::tools::extension::{PromptSectionProvider, ToolSpecProvider};

        let plugin = CentralPlugin;
        assert_eq!(plugin.id(), "central");
        assert!(plugin.tool_specs().is_empty(), "POC 中枢插件不得暴露工具");
        let sections = plugin.prompt_sections();
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("天工中枢"));
        assert!(sections[0].contains("不直接执行工作型任务"));
    }
}
