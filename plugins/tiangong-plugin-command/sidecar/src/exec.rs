//! 命令执行实现：tokio 子进程 spawn + kill_on_drop + env_clear + 三层 env 注入
//! + 超时 + stdout/stderr 截断。整体从原进程内 `handler.rs` 迁移。
//!
//! env 注入三层：
//! 1. 基础运行环境（PATH/HOME 等系统与代理变量）
//! 2. runtime_env（宿主下发的受控环境变量）
//! 3. file_env（cwd 下 .env.local / .env）
//!
//! 注：runtime_env 当前经 sidecar 进程环境继承（exec_env 由 host 注入 sidecar
//! 进程）。env_clear 后只回注这三类，不泄漏 sidecar 全部环境。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tiangong_plugin_command_protocol::exec::ExecResponse;
use tiangong_toolkit as shared;
use tokio::process::Command;
use tokio::time::timeout;

/// 执行命令并收集输出（搬迁自原 handler.rs 的 exec_and_collect）。
pub async fn exec_and_collect(
    cmd: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: u64,
) -> Result<ExecResponse> {
    let env_allowlist = command_env_allowlist();
    // runtime_env：当前经 sidecar 进程环境继承。exec_env 由 host 在 spawn sidecar
    // 时注入（若 host 未注入则为空，与原进程内 runtime_env 恒空等价）。
    // TODO（沙箱预留点 C）：未来 runtime_env 接通后，此处应从受控来源读取，
    // 并过滤危险 key（LD_PRELOAD/DYLD_*/BASH_ENV/PATH 劫持等）。
    let runtime_env = collect_runtime_env();
    let file_env = load_local_env(cwd);

    let mut command = Command::new(cmd);
    shared::configure_tokio_no_window(&mut command);
    command.kill_on_drop(true);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    configure_command_lifecycle(&mut command)?;
    for (key, value) in &env_allowlist {
        command.env(key, value);
    }
    for (key, value) in &runtime_env {
        command.env(key, value);
    }
    for (key, value) in &file_env {
        command.env(key, value);
    }

    let child = command
        .spawn()
        .with_context(|| format!("执行命令失败：{cmd}"))?;
    let output_result = if timeout_ms > 0 {
        match timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await {
            Ok(o) => o,
            Err(_) => {
                return Ok(ExecResponse {
                    ok: false,
                    summary: format!("命令执行超时：{cmd} (timeout_ms={timeout_ms})"),
                    exit_code: -1,
                    ..Default::default()
                });
            }
        }
    } else {
        child.wait_with_output().await
    };

    let output = output_result.context(format!("执行命令失败：{cmd}"))?;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = shared::truncate_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = shared::truncate_output(&String::from_utf8_lossy(&output.stderr));
    let ok = output.status.success();
    let summary = if ok {
        format!("命令执行成功：{cmd}")
    } else {
        format!("命令执行失败：{cmd} (exit_code={exit_code})")
    };

    Ok(ExecResponse {
        ok,
        summary,
        stdout,
        stderr,
        exit_code,
    })
}

/// 拆分命令字符串为 (程序名, 参数列表)（搬迁自原 handler.rs）。
pub fn split_command(raw: &str) -> (String, Vec<String>) {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    if parts.is_empty() {
        return (raw.to_string(), Vec::new());
    }
    let cmd = parts.remove(0);
    (cmd, parts)
}

/// 派生 shell 程序和参数；只校验工具支持的 shell 类型，不分析脚本文本。
pub fn derive_shell_exec_args(script: &str, shell: Option<&str>) -> Result<(String, Vec<String>)> {
    let shell = shell
        .unwrap_or("auto")
        .trim()
        .to_ascii_lowercase()
        .replace(' ', "");
    let selected = match shell.as_str() {
        "" | "auto" => {
            if cfg!(target_os = "windows") {
                "powershell"
            } else {
                "bash"
            }
        }
        "bash" => "bash",
        "sh" => "sh",
        "powershell" => "powershell",
        "pwsh" => "pwsh",
        other => return Err(anyhow::anyhow!("不支持的 shell 类型：{other}")),
    };
    let flag = if matches!(selected, "powershell" | "pwsh") {
        "-Command"
    } else if selected == "bash" {
        "-lc"
    } else {
        "-c"
    };
    Ok((
        selected.to_string(),
        vec![flag.to_string(), script.to_string()],
    ))
}

/// Command 子进程所需的最小基础环境。
fn command_env_allowlist() -> Vec<(String, String)> {
    const ALLOWED: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "TERM",
        "SystemRoot",
        "ComSpec",
        "PATHEXT",
        "http_proxy",
        "https_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ];
    ALLOWED
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_string(), value))
        })
        .collect()
}

/// 收集 runtime_env：解析 host 在 sidecar 启动时注入的受控 JSON 信封。
///
/// 再过滤动态加载器、shell 初始化和追踪类危险 key，避免命令执行环境被劫持。
fn collect_runtime_env() -> BTreeMap<String, String> {
    let Ok(raw) = std::env::var(tiangong_plugin_sidecar::env::EXEC_ENV_JSON_ENV) else {
        return BTreeMap::new();
    };
    let Ok(env) = serde_json::from_str::<BTreeMap<String, String>>(&raw) else {
        return BTreeMap::new();
    };
    env.into_iter()
        .filter(|(key, _)| is_safe_env_key(key))
        .collect()
}

/// 加载 cwd 下的 .env.local / .env 文件（搬迁自原 handler.rs）。
fn load_local_env(cwd: &Path) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for file in [".env.local", ".env"] {
        let path = cwd.join(file);
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((key, value)) = trimmed.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if !is_valid_env_key(key) || !is_safe_env_key(key) {
                continue;
            }
            let value = normalize_env_value(value.trim());
            env.push((key.to_string(), value));
        }
    }
    env
}

fn is_safe_env_key(key: &str) -> bool {
    // 解释器启动注入类变量能让 node/python/perl/ruby/jvm 在启动前加载额外
    // 代码，与动态加载器变量同层级拒绝（对齐 octos 危险环境清单）。
    const DENIED_EXACT: &[&str] = &[
        "BASH_ENV",
        "ENV",
        "PATH",
        "PS4",
        "TEMP",
        "TMP",
        "TMPDIR",
        "NODE_OPTIONS",
        "PYTHONSTARTUP",
        "PYTHONPATH",
        "PERL5OPT",
        "RUBYOPT",
        "RUBYLIB",
        "JAVA_TOOL_OPTIONS",
        "ZDOTDIR",
    ];
    const DENIED_PREFIXES: &[&str] = &["LD_", "DYLD_"];
    let upper = key.to_ascii_uppercase();
    !DENIED_EXACT.contains(&upper.as_str())
        && !DENIED_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
}

fn configure_command_lifecycle(_command: &mut Command) -> Result<()> {
    // 资源上限由执行边界施加：沙箱链路在 Launcher 进程树级强制
    //（CPU 全 Unix、内存/进程数 Linux、Windows Job Object），关闭沙箱
    // 的宿主执行器同样自带限额——命令进程不再自行读取环境变量施加。
    Ok(())
}

fn is_valid_env_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    for (idx, ch) in key.chars().enumerate() {
        if idx == 0 && !(ch.is_ascii_alphabetic() || ch == '_') {
            return false;
        }
        if !(ch.is_ascii_alphanumeric() || ch == '_') {
            return false;
        }
    }
    true
}

fn normalize_env_value(raw: &str) -> String {
    let mut value = raw.trim().to_string();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0] as char;
        let last = bytes[value.len() - 1] as char;
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            value = value[1..value.len() - 1].to_string();
        }
    }
    value
}
