//! `tiangong memory`：Memory 插件的命令行入口。
//!
//! 模型与检索配置统一在网页配置页完成（与天工设置页同一份页面），
//! `tiangong memory config` 只负责拉起已安装 sidecar 的 `--config` 模式；
//! 这里只保留启停、状态与连通性检查等运维命令。

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

use crate::args::{MemoryArgs, MemoryConfigArgs, MemorySubcommand};

pub(crate) fn run_memory_command(args: MemoryArgs) -> Result<()> {
    match args.command {
        MemorySubcommand::Config(args) => open_config_page(args),
        MemorySubcommand::Enable => {
            set_enabled(true)?;
            println!("Memory 已启用");
            Ok(())
        }
        MemorySubcommand::Disable => {
            set_enabled(false)?;
            println!("Memory 已禁用");
            Ok(())
        }
        MemorySubcommand::Status => {
            print_status(&query_status()?);
            Ok(())
        }
        MemorySubcommand::Test => test_memory(),
    }
}

/// 以已安装的 memory sidecar 运行 `--config`，阻塞到页面点击"完成并关闭"。
fn open_config_page(args: MemoryConfigArgs) -> Result<()> {
    let binary = installed_sidecar_binary()?;
    let mut command = std::process::Command::new(&binary);
    command.arg("--config");
    if let Some(host) = args.host {
        command.args(["--host", &host]);
    }
    if let Some(port) = args.port {
        command.args(["--port", &port.to_string()]);
    }
    if args.no_open {
        command.arg("--no-open");
    }
    // 与宿主使用同一存储根；清除插件宿主传输变量，确保进入独立配置模式。
    command
        .env(
            tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV,
            tiangong_config::io::storage_root(),
        )
        .env_remove("TIANGONG_PLUGIN_TRANSPORT");
    let status = command
        .status()
        .with_context(|| format!("启动 Memory 配置页失败：{}", binary.display()))?;
    if !status.success() {
        bail!("Memory 配置页异常退出：{status}");
    }
    Ok(())
}

/// 已安装 memory 插件的 sidecar 可执行文件路径。
fn installed_sidecar_binary() -> Result<PathBuf> {
    let directory = tiangong_config::io::storage_root()
        .join("plugins")
        .join("memory");
    let manifest_path = directory.join("plugin.json");
    if !manifest_path.is_file() {
        bail!(
            "未安装 Memory 插件（{}），请先在天工中安装，或直接运行 tiangong-memory-sidecar --config",
            directory.display()
        );
    }
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .with_context(|| format!("读取 {} 失败", manifest_path.display()))?,
    )
    .with_context(|| format!("解析 {} 失败", manifest_path.display()))?;
    let name = manifest
        .pointer("/sidecar/binary")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty() && !name.contains(['/', '\\']))
        .ok_or_else(|| anyhow!("Memory 插件清单缺少 sidecar 声明"))?;
    let mut binary = directory.join(name);
    let suffix = std::env::consts::EXE_SUFFIX;
    if !suffix.is_empty() && !name.ends_with(suffix) {
        binary.set_file_name(format!("{name}{suffix}"));
    }
    if !binary.is_file() {
        bail!("Memory sidecar 不存在：{}", binary.display());
    }
    Ok(binary)
}

pub(crate) fn set_enabled(enabled: bool) -> Result<()> {
    let operation = if enabled { "enable" } else { "disable" };
    invoke(operation, serde_json::json!({}))?;
    Ok(())
}

pub(crate) fn query_status() -> Result<serde_json::Value> {
    invoke("status", serde_json::json!({}))
}

pub(crate) fn test_config() -> Result<serde_json::Value> {
    invoke("test", serde_json::json!({}))
}

fn invoke(operation: &str, payload: serde_json::Value) -> Result<serde_json::Value> {
    tiangong_plugin_runtime::registry::invoke_sidecar(
        &tiangong_config::io::storage_root(),
        "memory",
        operation,
        payload,
    )
}

fn print_status(status: &serde_json::Value) {
    let disabled = status
        .get("disabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    println!("启用状态：{}", if disabled { "已禁用" } else { "已启用" });
    println!(
        "vector_mode: {}",
        status
            .get("vector_mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("未知")
    );
    for (key, label) in [
        ("llm", "LLM"),
        ("embedding", "Embedding"),
        ("rerank", "Rerank"),
    ] {
        let entry = status.get(key);
        let source = entry
            .and_then(|value| value.get("source"))
            .and_then(serde_json::Value::as_str);
        let model = entry
            .and_then(|value| value.get("model"))
            .and_then(serde_json::Value::as_str);
        let text = match (source, model) {
            (Some("builtin"), _) => format!(
                "内置（档位 {}）",
                entry
                    .and_then(|value| value.get("tier"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("mid")
            ),
            (_, Some(model)) => model.to_string(),
            _ => "未配置".to_string(),
        };
        println!("{label}：{text}");
    }
    println!("\n修改配置：tiangong memory config");
}

fn test_memory() -> Result<()> {
    let result = test_config()?;
    let ok = result
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if ok {
        println!("Memory 配置测试通过");
        return Ok(());
    }
    let issues = result
        .get("issues")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    for issue in &issues {
        eprintln!("- {}", issue.as_str().unwrap_or("未知问题"));
    }
    Err(anyhow!("Memory 配置测试未通过（{} 个问题）", issues.len()))
}
