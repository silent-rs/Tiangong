//! 工作区 git 概要（R1 从 lib.rs 拆出）：主分支/worktree 列表的
//! 只读摘要，供中枢上下文工具展示。

/// 工作区的 git 概要（一行）：主仓库当前分支或非 git 标记。
pub(crate) fn workspace_git_summary(path: &std::path::Path) -> String {
    if !path.join(".git").exists() {
        return "｜非 git 仓库".to_string();
    }
    let git_dir = if path.join(".git").is_file() {
        // worktree：回溯到主仓库 .git
        std::fs::read_to_string(path.join(".git"))
            .ok()
            .and_then(|content| {
                let meta = std::path::PathBuf::from(content.trim());
                meta.ancestors().nth(2).map(|p| p.to_path_buf())
            })
    } else {
        Some(path.join(".git"))
    };
    match git_dir.as_deref().and_then(read_head_branch) {
        Some(branch) => format!("｜git 分支 {branch}"),
        None => "｜git 仓库（HEAD 无法解析）".to_string(),
    }
}

/// 读取 git 仓库 HEAD 的当前分支名（`ref: refs/heads/<name>`）。
pub(crate) fn read_head_branch(git_dir: &std::path::Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.strip_prefix("ref: refs/heads/")
        .map(str::trim)
        .map(str::to_string)
}

/// 列出 `.git/worktrees/*`：各 worktree 的位置与当前分支。
pub(crate) fn list_worktrees(git_dir: &std::path::Path) -> Vec<String> {
    let root = git_dir.join("worktrees");
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let gitdir_file = entry.path().join("gitdir");
            let location = std::fs::read_to_string(&gitdir_file)
                .ok()
                .map(|content| {
                    std::path::Path::new(content.trim())
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                })
                .unwrap_or_else(|| "(位置未知)".to_string());
            let branch =
                read_head_branch(&entry.path()).unwrap_or_else(|| "( detached )".to_string());
            out.push(format!("worktree {name}: {location}（分支 {branch}）"));
        }
    }
    if out.is_empty() {
        out.push("worktrees: 无".to_string());
    } else {
        out.insert(0, "worktrees:".to_string());
    }
    out
}
