//! 目录定位：存储根、Agent 身份目录与运行态目录。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const STORAGE_ROOT_ENV: &str = "TIANGONG_STORAGE_ROOT";

/// 解析天工存储根：优先宿主注入，回退 `~/.tiangong`（与 scheduler 一致）。
pub fn storage_root() -> Result<PathBuf> {
    if let Ok(root) = std::env::var(STORAGE_ROOT_ENV)
        && !root.is_empty()
    {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var("HOME")
        .context("无法确定用户 home 目录（且宿主未注入 TIANGONG_STORAGE_ROOT）")?;
    Ok(Path::new(&home).join(".tiangong"))
}

/// 插件 ID（自有数据目录名）。
const PLUGIN_ID: &str = "subagent";

/// 插件自有数据目录 `~/.tiangong/data/subagent/`；首次访问时把存储根下旧
/// `agents/`、`agents-runtime/` 迁移进来（见 [`crate::migration`]）。迁移失败
/// 时返回错误，所有身份与运行态读写随之失败，避免新目录生成空数据后旧数据
/// 再也迁不过来。
fn data_root() -> Result<PathBuf> {
    static MIGRATION: std::sync::OnceLock<std::result::Result<(), String>> =
        std::sync::OnceLock::new();
    let data_dir = plugin_data_dir();
    MIGRATION
        .get_or_init(|| {
            let root = storage_root().map_err(|error| format!("{error:#}"))?;
            crate::migration::migrate_legacy_dirs(
                &data_dir,
                &[
                    (&root.join("agents"), AGENTS_DIR),
                    (&root.join("agents-runtime"), RUNTIME_DIR),
                ],
            )
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
        })
        .clone()
        .map_err(|error| anyhow::anyhow!("Subagent 旧数据迁移未完成：{error}"))?;
    Ok(data_dir)
}

/// 插件自有数据目录（不触发迁移）：优先宿主注入，否则 `<存储根>/data/subagent`。
fn plugin_data_dir() -> PathBuf {
    tiangong_plugin_sidecar::storage::storage_data_dir(PLUGIN_ID)
}

const AGENTS_DIR: &str = "agents";
const RUNTIME_DIR: &str = "runtime";

/// Agent 身份目录根：`~/.tiangong/data/subagent/agents/`。
pub fn agents_root() -> Result<PathBuf> {
    Ok(data_root()?.join(AGENTS_DIR))
}

/// 运行态目录根：`~/.tiangong/data/subagent/runtime/`。
pub fn runtime_root() -> Result<PathBuf> {
    Ok(data_root()?.join(RUNTIME_DIR))
}

/// 校验 ID 是安全的单段路径（防目录逃逸）。
pub fn validate_id_segment(value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        bail!("ID 必须是 1..=128 位的字母数字、'-' 或 '_' 组合: {value:?}")
    }
}

/// 进程内持久化写锁（多个 dispatch 并发落盘时避免同目录临时文件竞争）。
static PERSISTENCE_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 原子写文件（临时文件 + 持久化替换）。
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let _guard = PERSISTENCE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))
        .with_context(|| format!("创建临时文件失败: {}", path.display()))?;
    std::io::Write::write_all(&mut temp, contents)?;
    temp.persist(path)
        .with_context(|| format!("原子替换失败: {}", path.display()))?;
    Ok(())
}

/// 本地时间戳（用户约定：chrono::Local::now().naive_local()）。
pub fn now_string() -> String {
    chrono::Local::now().naive_local().to_string()
}

/// 新 scru128 ID。
pub fn new_id() -> String {
    scru128::new().to_string()
}
