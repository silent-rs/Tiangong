//! MCP 接入登记：CLI 工具以 MCP 模式启动 sidecar 并完成握手时登记其
//! workspace，派活引导据此去重——已接入的 workspace 不再重复携带 MCP
//! 引导（注册与否以「sidecar 真的被以 MCP 形态握手过」为准，不靠提示语
//! 自觉忽略）。
//!
//! 登记按 workspace 关联（MCP 进程不知道自己属于哪个成员，只有 cwd）；
//! CLI 卸载注册后记录保留（不再引导同样成立——卸载是用户主动选择）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{atomic_write, now_string, runtime_root};

#[derive(Debug, Default, Serialize, Deserialize)]
struct AccessLog {
    accesses: Vec<AccessEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccessEntry {
    workspace: String,
    first_seen: String,
    last_seen: String,
}

fn access_path() -> Result<PathBuf> {
    Ok(runtime_root()?.join("mcp_access.json"))
}

/// 跨进程排他锁（登记文件旁 `.mcp_access.lock`，flock 独占）：多个 MCP
/// 进程（不同 CLI 工具/会话）可能同时握手登记，无锁时并发读-改-写会
/// 互相覆盖。RAII：Drop 解锁，锁文件本身不删除。
struct AccessLock {
    _file: std::fs::File,
}

impl AccessLock {
    fn acquire(path: &Path) -> Result<Self> {
        use fs2::FileExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_file_name(".mcp_access.lock"))?;
        file.lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

impl Drop for AccessLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._file);
    }
}

fn load_log(path: &Path) -> AccessLog {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 工作区路径规范化：登记方（MCP 进程 cwd）与查询方（激活 workspace）
/// 的路径形态可能不同（macOS realpath 的 /private 前缀等），两侧统一
/// canonicalize 后再比对，避免同一路径两种写法导致去重失配。
fn normalize_workspace(workspace: &str) -> String {
    let trimmed = workspace.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    std::fs::canonicalize(trimmed)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| trimmed.to_string())
}

/// 登记一次 MCP 握手：workspace 已在则刷新 last_seen，否则新增条目。
/// 失败只记日志——登记是引导去重的辅助信息，不阻断 MCP 服务。
pub fn record_handshake(workspace: &str) {
    let workspace = normalize_workspace(workspace);
    if workspace.is_empty() {
        return;
    }
    if let Err(error) = record_handshake_inner(&workspace) {
        tracing::warn!(workspace, %error, "MCP 接入登记失败（不影响服务）");
    }
}

fn record_handshake_inner(workspace: &str) -> Result<()> {
    let path = access_path()?;
    // 锁文件与登记文件都在 runtime/ 下，目录可能尚未创建。
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建运行态目录失败: {}", parent.display()))?;
    }
    let _guard = AccessLock::acquire(&path)?;
    let mut log = load_log(&path);
    let timestamp = now_string();
    match log
        .accesses
        .iter_mut()
        .find(|entry| entry.workspace == workspace)
    {
        Some(entry) => entry.last_seen = timestamp,
        None => log.accesses.push(AccessEntry {
            workspace: workspace.to_string(),
            first_seen: timestamp.clone(),
            last_seen: timestamp,
        }),
    }
    atomic_write(&path, serde_json::to_vec(&log)?.as_slice())
        .with_context(|| format!("写入 MCP 接入登记失败: {}", path.display()))
}

/// workspace 是否已接入（存在握手登记）。
pub fn is_connected(workspace: &str) -> bool {
    let workspace = normalize_workspace(workspace);
    let Ok(path) = access_path() else {
        return false;
    };
    if workspace.is_empty() {
        return false;
    }
    load_log(&path)
        .accesses
        .iter()
        .any(|entry| entry.workspace == workspace)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 子进程级 env 隔离（storage_root 读 TIANGONG_STORAGE_ROOT）；测试
    /// 加 #[serial] 防 env 并发互染。
    fn with_temp_root(f: impl FnOnce()) {
        let temp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("TIANGONG_STORAGE_ROOT", temp.path()) };
        f();
        unsafe { std::env::remove_var("TIANGONG_STORAGE_ROOT") };
    }

    #[test]
    #[serial_test::serial]
    fn 未登记的工作区不算已接入() {
        with_temp_root(|| {
            assert!(!is_connected("/tmp/ws-a"));
        });
    }

    #[test]
    #[serial_test::serial]
    fn 登记后查询命中且重复登记不产生重复条目() {
        with_temp_root(|| {
            record_handshake("/tmp/ws-a");
            record_handshake("/tmp/ws-a");
            assert!(is_connected("/tmp/ws-a"));
            assert!(!is_connected("/tmp/ws-b"));

            let log = load_log(&access_path().unwrap());
            assert_eq!(log.accesses.len(), 1);
            assert_eq!(log.accesses[0].workspace, "/tmp/ws-a");
            // 重复登记刷新 last_seen（微秒精度下必然后移），不产生新条目。
            assert!(log.accesses[0].last_seen >= log.accesses[0].first_seen);
        });
    }

    #[test]
    #[serial_test::serial]
    fn 路径形态不同的同一工作区互通() {
        // macOS 下 MCP 进程 cwd 是 realpath（/private 前缀），激活侧可能是
        // 符号链接路径——两侧 canonicalize 归一后应命中同一条目。
        with_temp_root(|| {
            let temp = tempfile::tempdir().unwrap();
            let real = temp.path().canonicalize().unwrap();
            record_handshake(&real.to_string_lossy());
            assert!(
                is_connected(&temp.path().to_string_lossy()),
                "符号链接形态查询应命中 realpath 形态登记"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn 空白工作区不登记也不命中() {
        with_temp_root(|| {
            record_handshake("   ");
            assert!(!is_connected(""));
            assert!(!is_connected("   "));
        });
    }
}
