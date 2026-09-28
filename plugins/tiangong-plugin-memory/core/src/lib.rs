//! 天工 Memory 系统
//!
//! 提供分层记忆管理、三级 Injection 注入、Episode 写入与渐进式召回。
//!
//! 本 crate 只在 Memory sidecar 进程内启动 Actor、存储与 IPC 服务。
//! App、CLI、Server 通过 WASM 插件运行时访问 sidecar，不直接依赖本 crate；
//! 第三方 Agent 通过 sidecar 的 `--mcp` / `--daemon` 模式访问（见 [`external`]）。
//!
//! ```no_run
//! let handle = tiangong_memory::start().expect("Memory 系统启动失败");
//! let injections = tiangong_memory::load_injection_sync("session-1", None);
//! ```

pub(crate) mod command;
pub mod config;
pub mod election;
pub mod external;
pub mod handle;
pub mod ipc;
pub mod types;

mod actor;
mod db;
mod injection;
mod llm_metrics;
pub mod local_model;
mod options;
mod paths;
mod recall;
mod recall_anchor;
mod recall_context;
mod recovery;
mod rumination;
mod search;
mod store;
mod writer;

pub use actor::{start_memory as start, start_memory_with_options as start_with_options};
pub use config::{
    MEMORY_CONFIG_VERSION, MemoryComponentSelection, MemoryConfig, MemoryConfigSelection,
    MemoryEmbeddingSource, MemoryLlmSelection, MemoryLlmSource, MemoryLocalTier,
    MemoryRemoteEndpoint, MemoryRemoteSelection, MemoryRerankSource, default_memory_config_path,
    disable_memory, disable_memory_at, enable_memory, enable_memory_at, is_memory_disabled,
    legacy_models_handoff_path, memory_disabled_marker_path,
};
pub use election::{
    LeaderInfo, LeaderState, ManagedMemory, ProcessType, leader_info_path, leader_lock_path,
    memory_service_name, read_leader_info, start_or_connect, start_or_connect_with_options,
    start_or_connect_with_options_and_service,
};
pub use handle::MemoryHandle;
pub use options::{MemoryOptions, MemoryVectorMode};
pub use recovery::recover_plugin_data_dir;
pub use types::*;

/// 同步加载三级注入上下文（不经过 Actor，直接读文件）
///
/// 用于不在异步上下文中调用注入内容的场景（如测试、同步 prompt 装配）。
pub fn load_injection_sync(session_id: &str, workspace_id: Option<&str>) -> Vec<String> {
    injection::load_injection_context(session_id, workspace_id)
}

/// 同步初始化 Memory（不启动 Actor，仅确保 SQLite 数据库就绪）
///
/// 适合无 tokio 运行时的入口（如 CLI 同步模式），保证数据库文件被创建并加密。
pub fn init_blocking() -> anyhow::Result<()> {
    let _ = db::MemoryDb::open()?;
    Ok(())
}
