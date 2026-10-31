//! tiangong-core-manager
//!
//! 会话级 TiangongCore 注册表与资源管理层（issue #245）。
//!
//! 定位：app-state 收窄后剥离出来的「资源加载与管理」职责的归宿。
//!
//! - **CoreManager**：持有 `session_id -> TiangongCore` 映射，提供 ensure / retire /
//!   sync_config / load_session 等管理性操作，**不执行 turn**。CoreManager 针对
//!   TiangongCore，不抽象「其他 Core 类型」——Core 构造内置在 `ensure_core`，
//!   host 在调用前构造好 plugin 集合并作为参数传入
//! - **SessionMetadata**：UI 展示 + 配置构建所需的轻量视图，替代完整 Session 列表
//!
//! 真相源仍是磁盘：CoreManager 只做按需加载与缓存，session 真正的写入仍由 Core
//! 的 turn worker 实时落盘（`PERSISTENCE_WRITE_LOCK` 保证原子写）。
//!
//! 实例生命周期：Core 在 turn 结束后留在 registry 中等待复用；空闲（无交互、
//! 无运行中 turn）超过保留时长（暂定 1 小时，见 `idle` 模块常量）后由后台
//! 回收任务销毁；当前激活会话占用单值豁免槽位、不受回收影响。下一次交互
//! 经 `ensure_core` 从磁盘按需重建。

pub mod core_manager;
mod metadata;
mod workspace;

pub use core_manager::mentions::MentionSource;
pub use core_manager::{
    ContextCompactionError, CoreManager, CoreRegistry, CoreRegistryGuard, EnsuredCore,
};
pub use metadata::SessionMetadata;
pub use workspace::resolve_effective_cwd;

use std::path::{Path, PathBuf};

/// 校验 session_id 为单个安全路径片段（拒绝 `..`、绝对路径、路径分隔符），
/// 返回 `{storage_root}/sessions/{id}.json`。
fn session_file_path(storage_root: &Path, session_id: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(session_id).components();
    let valid_id = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !valid_id {
        return Err("会话 ID 必须是单个安全路径片段".to_string());
    }
    Ok(storage_root
        .join("sessions")
        .join(format!("{session_id}.json")))
}
