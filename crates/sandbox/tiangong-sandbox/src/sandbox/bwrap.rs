//! Linux bubblewrap 编译器。
//!
//! 全盘只读挂载 + 可写根分层 bind；受保护路径以只读 bind 覆盖
//! （bwrap 按参数顺序挂载，后 bind 同一路径覆盖先 bind）。

use std::path::Path;

use super::policy::{SandboxMode, SandboxPolicy};

const CANDIDATE_BINS: [&str; 3] = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"];

pub fn bwrap_available() -> Option<String> {
    if let Ok(path) = std::env::var("TIANGONG_SANDBOX_BWRAP")
        && Path::new(&path).exists()
    {
        return Some(path);
    }
    CANDIDATE_BINS
        .iter()
        .find(|bin| Path::new(bin).exists())
        .map(|bin| bin.to_string())
}

/// 编译为 bwrap 参数前缀（不含 `--` 与被包装命令）。
pub fn wrap_argv(policy: &SandboxPolicy) -> Vec<String> {
    // 全盘只读镜像。
    let mut argv = vec![
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
    ];
    // 可写根。
    if policy.mode == SandboxMode::WorkspaceWrite {
        for root in policy.writable_roots() {
            let root = root.display().to_string();
            argv.push("--bind".into());
            argv.push(root.clone());
            argv.push(root);
        }
    }
    // 防篡改段：只读 bind 覆盖（在可写 bind 之后声明才生效）。
    for path in policy.read_only_roots() {
        if path.exists() {
            let path = path.display().to_string();
            argv.push("--ro-bind".into());
            argv.push(path.clone());
            argv.push(path);
        }
    }
    // 读取隔离在所有可写挂载之后覆盖：目录替换为空 tmpfs，文件替换为
    // 只读 /dev/null。宿主敏感内容不会出现在沙箱挂载命名空间内。
    for path in policy.denied_read_roots() {
        if path.is_dir() {
            argv.push("--tmpfs".into());
            argv.push(path.display().to_string());
        } else if path.is_file() {
            argv.push("--ro-bind".into());
            argv.push("/dev/null".into());
            argv.push(path.display().to_string());
        }
    }
    argv.push("--unshare-user".into());
    argv.push("--unshare-pid".into());
    if !policy.allow_network {
        argv.push("--unshare-net".into());
    }
    argv.push("--die-with-parent".into());
    // 环境清理责任在调用方（如 command sidecar 的 env_clear + 白名单重建）：
    // 此处若 --clearenv 会把宿主注入的环境变量一并清掉。
    argv.push("--".into());
    argv
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn argv_orders_ro_bind_before_writable_and_protection() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("ws");
        let secrets = root.path().join("home/.ssh");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&secrets).unwrap();
        let mut policy = SandboxPolicy::workspace_write(&workspace);
        policy.denied_read_paths = vec![secrets.clone()];
        let argv = wrap_argv(&policy);
        let ro_root = argv.iter().position(|a| a == "--ro-bind").unwrap();
        let bind_ws = argv
            .iter()
            .position(|a| a == &workspace.display().to_string())
            .unwrap();
        assert!(bind_ws > ro_root);
        let hidden_secret = argv
            .iter()
            .position(|a| a == &secrets.display().to_string())
            .unwrap();
        assert!(hidden_secret > bind_ws);
        assert_eq!(argv[hidden_secret - 1], "--tmpfs");
        assert!(argv.contains(&"--unshare-net".to_string()));
        assert!(argv.last().unwrap() == "--");
        assert!(!argv.iter().any(|arg| arg.contains("bwrap")));
    }
}
