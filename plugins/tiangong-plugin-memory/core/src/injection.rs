//! Injection 层：三级注入文件读写
//!
//! 按 Profile → Workspace → Session 三级顺序加载 agent.md 文件，
//! 组合成注入上下文。

use std::fs;
use std::path::PathBuf;

use anyhow::Result;

use crate::command::InjectionLevel;

/// 获取 memory 基础目录
pub(crate) fn memory_base_dir() -> PathBuf {
    crate::paths::memory_data_dir()
}

/// 读取注入文件，返回文件内容（不存在则返回 None）
fn read_injection_file(path: &PathBuf) -> Option<String> {
    if path.exists() {
        fs::read_to_string(path).ok()
    } else {
        None
    }
}

/// 加载三级注入上下文
///
/// 顺序：Profile → Workspace → Session
/// 每级文件存在且非空则加入输出列表。
pub fn load_injection_context(session_id: &str, workspace_id: Option<&str>) -> Vec<String> {
    let mut ctx = Vec::new();
    let base = memory_base_dir();

    // Level 1: Profile
    let profile_path = base.join("profile").join("agent.md");
    if let Some(text) = read_injection_file(&profile_path) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            ctx.push(trimmed.to_string());
        }
    }

    // Level 2: Workspace
    if let Some(wid) = workspace_id {
        let ws_path = base.join("workspaces").join(wid).join("agent.md");
        if let Some(text) = read_injection_file(&ws_path) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                ctx.push(trimmed.to_string());
            }
        }
    }

    // Level 3: Session
    let session_path = base.join("sessions").join(session_id).join("agent.md");
    if let Some(text) = read_injection_file(&session_path) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            ctx.push(trimmed.to_string());
        }
    }

    ctx
}

/// 写入注入文件（Profile/Workspace/Session 各级）
pub fn write_injection_file(level: InjectionLevel, target_id: &str, content: &str) -> Result<()> {
    let base = memory_base_dir();
    let path = match level {
        InjectionLevel::Profile => base.join("profile").join("agent.md"),
        InjectionLevel::Workspace => base.join("workspaces").join(target_id).join("agent.md"),
        InjectionLevel::Session => base.join("sessions").join(target_id).join("agent.md"),
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, content)?;

    Ok(())
}
