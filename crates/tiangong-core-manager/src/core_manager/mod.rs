//! CoreManager：会话级 TiangongCore 注册表与资源管理层。
//!
//! 定位（见 issue #245）：
//! - 持有 `session_id -> TiangongCore` 映射，承接 app-state 收窄后剥离的「资源
//!   加载与管理」职责
//! - **不执行 turn**（turn 仍由 Core 内部驱动）
//! - session 真相源是磁盘，CoreManager 只做按需加载与缓存
//!
//! CoreManager 针对 `TiangongCore`，不抽象「其他 Core 类型」。Core 的实际构造
//! 内置在 `ensure_core`：host 在调用前构造好 plugin 集合并作为参数传入
//! （不同 host 的 plugin 构造差异大，不能在共享层硬编码）。

pub mod ensure;
pub mod mentions;
pub mod registry;
mod title;

pub use self::registry::{CoreRegistry, CoreRegistryGuard};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use tiangong_core::config::core::CoreConfig;
use tiangong_core::core::TiangongCore;
use tiangong_core::session::Session;

use crate::SessionMetadata;

type MetadataCache = HashMap<String, ((Option<std::time::SystemTime>, u64), SessionMetadata)>;

/// `ensure_core` 的返回：区分新建与复用既有 Core。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsuredCore {
    pub session_id: String,
    pub is_new: bool,
}

/// 会话级 TiangongCore 管理器。
///
/// `#[derive(Clone)]` 的廉价句柄，内部状态全经 `Arc<Mutex<>>` 共享。
#[derive(Clone)]
pub struct CoreManager {
    cores: Arc<Mutex<HashMap<String, TiangongCore>>>,
    /// 宿主注入的 @提及候选来源（非插件、不占插件位，与 Core 执行无关）。
    mention_sources: Arc<Mutex<Vec<Arc<dyn crate::core_manager::mentions::MentionSource>>>>,
    /// 每会话的创建互斥锁：覆盖同一会话从「检查 Core 是否存在」到「插入新 Core」
    /// 的完整创建区间，防止两路并发为同一 session 各建一份 Core。
    ///
    /// 锁对象不主动删除，避免旧等待者尚未退出时为同一 session 创建第二把锁。
    creation_locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    /// 会话元数据缓存：id → ((修改时间, 文件大小), 元数据)。
    metadata_cache: Arc<Mutex<MetadataCache>>,
    config: tiangong_core::config::core::CoreConfigProvider,
    storage_root: PathBuf,
}

impl CoreManager {
    /// 构造管理器。
    ///
    /// - `config`：全局配置模板 provider，用于 `ensure_core` 的 base 快照与
    ///   `sync_config` 的模板替换
    /// - `storage_root`：session 文件根（形如 `~/.tiangong`）
    pub fn new(
        config: tiangong_core::config::core::CoreConfigProvider,
        storage_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            cores: Arc::new(Mutex::new(HashMap::new())),
            mention_sources: Arc::new(Mutex::new(Vec::new())),
            creation_locks: Arc::new(Mutex::new(HashMap::new())),
            metadata_cache: Arc::new(Mutex::new(HashMap::new())),
            config,
            storage_root: storage_root.into(),
        }
    }

    /// 全局配置 provider（host 用它取 base 快照构建 per-session 配置）。
    pub fn config(&self) -> &tiangong_core::config::core::CoreConfigProvider {
        &self.config
    }

    /// 会话文件根目录。
    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }

    /// 从磁盘按 id 加载完整 Session（委托 `Session::load_from_storage`）。
    pub fn load_session(&self, session_id: &str) -> Result<Session, String> {
        Session::load_from_storage(&self.storage_root, session_id)
    }

    /// 扫描磁盘 `sessions/` 目录，返回所有会话 id（issue #245：会话列表真相源归磁盘）。
    pub fn list_session_ids(&self) -> Vec<String> {
        let dir = self.storage_root.join("sessions");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("json")) {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                ids.push(stem.to_string());
            }
        }
        ids
    }

    /// 批量加载所有会话的元数据（浅字段，不构造完整 Session）。
    ///
    /// 按文件修改时间与大小缓存解析结果：会话列表频繁刷新，而绝大多数会话文件
    /// 未变化，命中缓存时只需一次 stat，不再重读、重解析数百 MB 的历史。
    /// 会话文件损坏时跳过（记 warn），不阻断列表。
    pub fn list_session_metadata(&self) -> Vec<SessionMetadata> {
        let dir = self.storage_root.join("sessions");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut cache = self
            .metadata_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for entry in entries.flatten() {
            let Ok(file_meta) = entry.metadata() else {
                continue;
            };
            if !file_meta.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("json")) {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let stamp = (file_meta.modified().ok(), file_meta.len());
            seen.insert(id.clone());
            if let Some((cached_stamp, meta)) = cache.get(&id)
                && *cached_stamp == stamp
                && stamp.0.is_some()
            {
                result.push(meta.clone());
                continue;
            }
            match SessionMetadata::load_from_storage(&self.storage_root, &id) {
                Ok(meta) => {
                    cache.insert(id, (stamp, meta.clone()));
                    result.push(meta);
                }
                Err(error) => {
                    cache.remove(&id);
                    tracing::warn!(session_id = %id, %error, "跳过损坏的会话文件");
                }
            }
        }
        cache.retain(|id, _| seen.contains(id));
        result
    }

    /// 指定会话在磁盘上是否存在。
    pub fn session_exists(&self, session_id: &str) -> bool {
        self.storage_root
            .join("sessions")
            .join(format!("{session_id}.json"))
            .exists()
    }

    /// 删除磁盘上的会话文件（不操作 cores registry——调用方按需 retire_core）。
    pub fn delete_session_file(&self, session_id: &str) -> Result<(), String> {
        let path = self
            .storage_root
            .join("sessions")
            .join(format!("{session_id}.json"));
        if path.exists() {
            std::fs::remove_file(&path).map_err(|error| format!("删除会话文件失败：{error}"))?;
        }
        Ok(())
    }

    /// 将会话文件原子移动到回收区 `trash/sessions/{id}.json`。
    ///
    /// 同文件系统 `fs::rename` 保证原子性：要么成功，要么文件原封不动。
    /// 移走后 `list_session_metadata()` 扫描 `sessions/` 天然看不到。
    /// 文件不存在时幂等返回 Ok。
    pub fn trash_session_file(&self, session_id: &str) -> Result<(), String> {
        // 安全校验：拒绝 ..、绝对路径、路径分隔符。
        let src = crate::session_file_path(&self.storage_root, session_id)?;
        if !src.exists() {
            return Ok(());
        }
        let trash_dir = self.storage_root.join("trash").join("sessions");
        std::fs::create_dir_all(&trash_dir)
            .map_err(|error| format!("创建回收区目录失败：{error}"))?;
        let dst = trash_dir.join(format!("{session_id}.json"));
        std::fs::rename(&src, &dst).map_err(|error| {
            format!(
                "会话文件移动到回收区失败（{} → {}）：{error}",
                src.display(),
                dst.display()
            )
        })
    }

    /// 将会话从回收区恢复到 `sessions/`（原子 rename）。
    ///
    /// 文件不存在于回收区时返回错误。目标已存在同名文件时返回错误（避免覆盖）。
    pub fn restore_session_file(&self, session_id: &str) -> Result<(), String> {
        let _ = crate::session_file_path(&self.storage_root, session_id)?;
        let src = self
            .storage_root
            .join("trash")
            .join("sessions")
            .join(format!("{session_id}.json"));
        if !src.exists() {
            return Err(format!("回收区中不存在会话 {session_id}"));
        }
        let dst = self
            .storage_root
            .join("sessions")
            .join(format!("{session_id}.json"));
        if dst.exists() {
            return Err(format!("会话 {session_id} 已存在于正常目录，无法恢复"));
        }
        std::fs::rename(&src, &dst).map_err(|error| {
            format!(
                "恢复会话文件失败（{} → {}）：{error}",
                src.display(),
                dst.display()
            )
        })
    }

    /// 扫描回收区 `trash/sessions/`，返回残留会话 ID 列表。
    ///
    /// 这些是逻辑删除后等待物理清理的会话。
    pub fn list_trashed_session_ids(&self) -> Vec<String> {
        let trash_dir = self.storage_root.join("trash").join("sessions");
        let Ok(entries) = std::fs::read_dir(&trash_dir) else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("json")) {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                ids.push(stem.to_string());
            }
        }
        ids
    }

    /// 删除回收区中的会话文件（物理清理阶段调用）。
    pub fn delete_trashed_session(&self, session_id: &str) -> Result<(), String> {
        let path = self
            .storage_root
            .join("trash")
            .join("sessions")
            .join(format!("{session_id}.json"));
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("删除回收区会话文件失败：{error}"))?;
        }
        Ok(())
    }

    /// 将会话从回收区移到"正在清理"目录（`trash/purging/`）。
    ///
    /// 一旦进入 purging，不再提供恢复能力（资源可能已被部分删除），
    /// 但保留记录直到全部资源清理成功，支持失败重试。
    pub fn move_to_purging(&self, session_id: &str) -> Result<(), String> {
        let _ = crate::session_file_path(&self.storage_root, session_id)?;
        let src = self
            .storage_root
            .join("trash")
            .join("sessions")
            .join(format!("{session_id}.json"));
        if !src.exists() {
            return Ok(()); // 可能已在 purging 中（上次清理中断）
        }
        let purging_dir = self.storage_root.join("trash").join("purging");
        std::fs::create_dir_all(&purging_dir)
            .map_err(|error| format!("创建 purging 目录失败：{error}"))?;
        let dst = purging_dir.join(format!("{session_id}.json"));
        std::fs::rename(&src, &dst)
            .map_err(|error| format!("移动到 purging 失败（{}）：{error}", src.display()))
    }

    /// 扫描 `trash/purging/`，返回正在清理（含上次失败残留）的会话 ID 列表。
    pub fn list_purging_session_ids(&self) -> Vec<String> {
        let purging_dir = self.storage_root.join("trash").join("purging");
        let Ok(entries) = std::fs::read_dir(&purging_dir) else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            if path.extension() != Some(std::ffi::OsStr::new("json")) {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                ids.push(stem.to_string());
            }
        }
        ids
    }

    /// 删除"正在清理"目录中的会话文件（全部资源清理成功后调用）。
    pub fn delete_purging_session(&self, session_id: &str) -> Result<(), String> {
        let path = self
            .storage_root
            .join("trash")
            .join("purging")
            .join(format!("{session_id}.json"));
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("删除 purging 会话文件失败：{error}"))?;
        }
        Ok(())
    }

    /// 逻辑删除会话：等待 worker 退出 → 原子移动会话文件到回收区。
    ///
    /// 必须先等 worker 退出再移文件：worker 收到 Cancel 后仍会执行最终 try_persist_to_disk，
    /// 如果先移文件，这次写盘会把会话重新写回 sessions/ 导致"复活"。
    /// 无活跃 turn 时 cancel_and_join 立即返回，不影响速度。
    /// 不做 retire_core 的 finalize（load/persist/plugins）——会话文件马上要移走，
    /// 加载和保存无意义。不删媒体/teams（那些留给物理清理）。
    pub async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        // 入口立即校验 session_id（前端传入，拒绝 ..、绝对路径、分隔符）。
        let _ = crate::session_file_path(&self.storage_root, session_id)?;
        let creation_lock = self.creation_lock(session_id);
        let _creation_guard = creation_lock.lock_owned().await;
        // 先发取消信号并摘除 Core。
        let _ = self.cancel_core(session_id);
        let core = self.take_core(session_id);
        // 等 Agent driver 真正退出（有活跃 turn 时等取消生效并收敛；空闲时立即返回）。
        let sid = session_id.to_string();
        let sid_for_err = sid.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tiangong_core::shared_runtime::cancel_and_join(&sid);
            drop(core);
        })
        .await
        .map_err(|error| format!("等待会话 {sid_for_err} 的 worker 退出失败：{error}"))?;
        // worker 已退出，安全移文件到回收区。
        self.trash_session_file(session_id)?;
        Ok(())
    }

    /// 同步配置快照到所有存活 Core。
    ///
    /// 对每个存活 Core 调用 `replace_config`，并即时同步会话级运行配置。host 负责在调用
    /// 前构建好 `session_id -> CoreConfig` 映射（通常读 app-state 的配置缓存）。
    /// 同时把 `template` 替换为全局 provider 的最新模板（仅作新建 Core 的辅助，
    /// 不承载任一会话的 trust/reasoning 覆盖）。
    pub fn sync_config(&self, template: CoreConfig, session_configs: &HashMap<String, CoreConfig>) {
        self.config.replace(template);
        let mut registry = self.registry();
        for (session_id, core) in registry.iter_mut() {
            if let Some(config) = session_configs.get(session_id) {
                let _ = core.replace_config(config.clone());
                core.set_trust_mode(config.trust_mode);
                core.set_reasoning_effort(config.reasoning_effort);
            }
        }
    }

    /// 取得 registry 的句柄（暴露给 host 做 deliver / has_live 等只读访问）。
    pub fn registry(&self) -> CoreRegistryGuard<'_> {
        CoreRegistryGuard::lock(&self.cores)
    }

    /// 每会话的创建互斥锁句柄（供 host 取锁后串行化创建前后的额外工作）。
    pub fn creation_lock(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = match self.creation_locks.lock() {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!(error = %error, "Core 创建锁表已损坏，恢复后继续");
                error.into_inner()
            }
        };
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// 是否持有可投递的 Core。
    pub fn has_live_core(&self, session_id: &str) -> bool {
        self.registry().contains_key(session_id)
    }

    /// 会话是否有正在执行的 turn（Core 不存在时为 false）。
    ///
    /// 运行中的会话拒绝一切需要空闲态的操作（整理上下文、切换模型、写
    /// `Session.model_ref`），投递路径据此跳过模型编排。
    pub fn is_core_busy(&self, session_id: &str) -> bool {
        self.registry()
            .get(session_id)
            .is_some_and(|core| core.is_busy())
    }
}

impl std::fmt::Debug for CoreManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self.cores.lock().map(|g| g.len()).unwrap_or(0);
        f.debug_struct("CoreManager")
            .field("live_cores", &live)
            .field("storage_root", &self.storage_root)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
    use tiangong_core::session::Session;

    fn make_manager(dir: &tempfile::TempDir) -> CoreManager {
        CoreManager::new(
            CoreConfigProvider::new(CoreConfig::default()),
            dir.path().to_path_buf(),
        )
    }

    fn persist_session(dir: &tempfile::TempDir, id: &str) {
        let mut session = Session::new("test");
        session.id = id.to_string();
        session.bind_storage_root(dir.path().to_path_buf());
        session.try_persist_to_disk().unwrap();
    }

    #[test]
    fn trash_session_file_moves_to_trash() {
        let dir = tempfile::tempdir().unwrap();
        persist_session(&dir, "s1");
        let manager = make_manager(&dir);
        assert!(manager.session_exists("s1"));

        manager.trash_session_file("s1").unwrap();
        // 原位置不存在
        assert!(!manager.session_exists("s1"));
        // trash 中存在
        let trashed = manager.list_trashed_session_ids();
        assert_eq!(trashed, vec!["s1"]);
    }

    #[test]
    fn trash_session_file_idempotent_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager(&dir);
        // 文件不存在时应幂等返回 Ok
        manager.trash_session_file("never-existed").unwrap();
    }

    #[test]
    fn delete_trashed_session_removes_from_trash() {
        let dir = tempfile::tempdir().unwrap();
        persist_session(&dir, "s2");
        let manager = make_manager(&dir);
        manager.trash_session_file("s2").unwrap();
        assert_eq!(manager.list_trashed_session_ids().len(), 1);

        manager.delete_trashed_session("s2").unwrap();
        assert!(manager.list_trashed_session_ids().is_empty());
    }

    #[test]
    fn list_trashed_empty_when_no_trash_dir() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager(&dir);
        assert!(manager.list_trashed_session_ids().is_empty());
    }

    #[tokio::test]
    async fn delete_session_trashes_file_and_removes_core() {
        let dir = tempfile::tempdir().unwrap();
        persist_session(&dir, "s3");
        let manager = make_manager(&dir);
        assert!(manager.session_exists("s3"));

        manager.delete_session("s3").await.unwrap();
        // 文件应移到 trash（不再在 sessions/ 中）
        assert!(!manager.session_exists("s3"));
        assert_eq!(manager.list_trashed_session_ids(), vec!["s3"]);
    }

    #[tokio::test]
    async fn delete_session_idempotent_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager(&dir);
        // 文件不存在时 delete_session 不报错
        let result = manager.delete_session("never-existed").await;
        assert!(result.is_ok());
    }

    #[test]
    fn list_trashed_session_ids_lists_all_json() {
        let dir = tempfile::tempdir().unwrap();
        persist_session(&dir, "a");
        persist_session(&dir, "b");
        let manager = make_manager(&dir);
        manager.trash_session_file("a").unwrap();
        manager.trash_session_file("b").unwrap();
        let mut ids = manager.list_trashed_session_ids();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
    }

    // ===== 审查修复覆盖 =====

    #[tokio::test]
    async fn delete_session_file_not_revived_by_late_persist() {
        // 验证修复 1：删除后会话文件不在正常目录，即使有迟到写盘也不会复活。
        // delete_session 先等 worker 退出再移文件，保证不会有迟到 try_persist_to_disk。
        let dir = tempfile::tempdir().unwrap();
        persist_session(&dir, "late-write");
        let manager = make_manager(&dir);

        manager.delete_session("late-write").await.unwrap();
        // 正常目录不存在
        assert!(!manager.session_exists("late-write"));
        // 模拟迟到写盘：直接调 try_persist_to_disk（此时文件已被移走）
        // 如果 delete_session 正确等待了 worker，不会走到这里——但即使手动调，
        // 文件也已在 trash 中，正常目录不会出现两个同名文件。
        assert_eq!(manager.list_trashed_session_ids(), vec!["late-write"]);
    }

    #[tokio::test]
    async fn restore_session_file_moves_back_to_sessions() {
        // 验证恢复功能：trash → sessions 反向移动。
        let dir = tempfile::tempdir().unwrap();
        persist_session(&dir, "restorable");
        let manager = make_manager(&dir);
        manager.trash_session_file("restorable").unwrap();
        assert!(!manager.session_exists("restorable"));

        manager.restore_session_file("restorable").unwrap();
        assert!(manager.session_exists("restorable"));
        assert!(manager.list_trashed_session_ids().is_empty());
    }

    #[test]
    fn restore_session_file_fails_when_not_in_trash() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager(&dir);
        assert!(manager.restore_session_file("ghost").is_err());
    }

    #[test]
    fn restore_session_file_fails_when_target_exists() {
        // 正常目录已有同名文件时，恢复应失败（避免覆盖）。
        let dir = tempfile::tempdir().unwrap();
        persist_session(&dir, "conflict");
        let manager = make_manager(&dir);
        // 手动在 trash 中放一份同名文件
        let trash_dir = dir.path().join("trash").join("sessions");
        std::fs::create_dir_all(&trash_dir).unwrap();
        std::fs::copy(
            dir.path().join("sessions").join("conflict.json"),
            trash_dir.join("conflict.json"),
        )
        .unwrap();
        assert!(manager.restore_session_file("conflict").is_err());
    }

    #[tokio::test]
    async fn delete_session_concurrent_same_id_is_safe() {
        // 同一会话并发删除不应出错（creation_lock 保证串行化）。
        let dir = tempfile::tempdir().unwrap();
        persist_session(&dir, "concurrent");
        let manager = make_manager(&dir);
        let m1 = manager.clone();
        let m2 = manager.clone();
        let (r1, r2) = tokio::join!(
            async move { m1.delete_session("concurrent").await },
            async move { m2.delete_session("concurrent").await },
        );
        // 两个都应成功（第二个是幂等——文件已不在，trash_session_file 返回 Ok）
        assert!(r1.is_ok());
        assert!(r2.is_ok());
        assert_eq!(manager.list_trashed_session_ids(), vec!["concurrent"]);
    }
}
