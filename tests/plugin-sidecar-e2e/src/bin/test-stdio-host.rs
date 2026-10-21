//! 测试用 stdio 宿主：启动 sidecar 并保持一个长请求运行。
//!
//! 宿主崩溃清理测试（stdio_e2e）以 SIGKILL 终止本进程，验证 sidecar
//! 进程组随宿主退出被清理。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tiangong_plugin_command_protocol::{COMMAND_PROTOCOL_VERSION, PLUGIN_VERSION};
use tiangong_plugin_runtime::sidecar::{EphemeralCommandConnection, SidecarInvocationContext};
use tiangong_plugin_runtime::sidecar::{SidecarConfig, SidecarConnection, StdioSidecarConnection};

/// command 沙箱模式的传输协议版本。
const COMMAND_TRANSPORT_PROTOCOL: &str = "0.1.0";

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next().context("缺少运行模式或 sidecar 路径")?;
    #[cfg(windows)]
    if first == "--windows-command-tree-helper" {
        let command_pid_file = args
            .next()
            .map(PathBuf::from)
            .context("缺少 command PID 文件")?;
        let child_pid_file = args
            .next()
            .map(PathBuf::from)
            .context("缺少子进程 PID 文件")?;
        if args.next().is_some() {
            bail!("参数过多");
        }
        return run_windows_command_tree_helper(&command_pid_file, &child_pid_file);
    }
    #[cfg(windows)]
    if first == "--windows-wait-helper" {
        if args.next().is_some() {
            bail!("参数过多");
        }
        std::thread::sleep(Duration::from_secs(600));
        return Ok(());
    }
    if first == "--command-sandbox" {
        let sidecar = args
            .next()
            .map(PathBuf::from)
            .context("缺少 command sidecar 路径")?;
        let state_dir = args.next().map(PathBuf::from).context("缺少状态目录")?;
        if args.next().is_some() {
            bail!("参数过多");
        }
        return run_command_sandbox(sidecar, state_dir);
    }
    let sidecar = PathBuf::from(first);
    let state_dir = args.next().map(PathBuf::from).context("缺少状态目录")?;
    if args.next().is_some() {
        bail!("参数过多");
    }
    run_stdio(sidecar, state_dir)
}

fn run_stdio(sidecar: PathBuf, state_dir: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&state_dir).context("创建测试状态目录失败")?;
    let config = SidecarConfig::new(
        "test-stdio",
        "0.0.0",
        sidecar,
        state_dir.join("endpoint.json"),
        state_dir.join("sidecar.log"),
        state_dir.join("data"),
        state_dir.join("storage"),
    )
    .with_timeouts(Duration::from_secs(10), Duration::from_secs(600));
    let connection = StdioSidecarConnection::new(config);
    let payload = serde_json::json!({
        "sidecar_pid_file": state_dir.join("sidecar.pid"),
        "child_pid_file": state_dir.join("child.pid"),
    });
    connection.invoke("hang", &payload.to_string())?;
    bail!("hang 请求意外结束")
}
fn run_command_sandbox(sidecar: PathBuf, state_dir: PathBuf) -> Result<()> {
    let storage_root = state_dir.join("storage");
    let plugin_root = storage_root.join("plugins/command");
    let workspace = state_dir.join("workspace");
    let home = state_dir.join("home");
    std::fs::create_dir_all(&plugin_root).context("创建 command 插件目录失败")?;
    std::fs::create_dir_all(&workspace).context("创建 command 工作区失败")?;
    std::fs::create_dir_all(&home).context("创建测试 HOME 失败")?;
    let binary_name = format!("tiangong-command-sidecar{}", std::env::consts::EXE_SUFFIX);
    let installed_sidecar = plugin_root.join(&binary_name);
    std::fs::copy(&sidecar, &installed_sidecar).context("复制 command sidecar 失败")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&installed_sidecar, std::fs::Permissions::from_mode(0o755))
            .context("设置 command sidecar 权限失败")?;
    }
    std::fs::write(
        plugin_root.join("plugin.json"),
        serde_json::json!({
            "id": "command",
            "sidecar": { "binary": "tiangong-command-sidecar" }
        })
        .to_string(),
    )
    .context("写入 command 插件清单失败")?;

    let config = SidecarConfig::new(
        "command",
        PLUGIN_VERSION,
        installed_sidecar,
        plugin_root.join("endpoint.json"),
        plugin_root.join("sidecar.log"),
        plugin_root.join("data"),
        storage_root,
    )
    .with_timeouts(Duration::from_secs(15), Duration::from_secs(600))
    .with_protocols(COMMAND_TRANSPORT_PROTOCOL, COMMAND_PROTOCOL_VERSION)
    .with_sandbox_program_root(Some(plugin_root));
    let connection = EphemeralCommandConnection::new(config);
    connection.update_exec_env(BTreeMap::from([(
        "HOME".to_string(),
        home.display().to_string(),
    )]));
    #[cfg(unix)]
    let sidecar_pid_file = workspace.join("sidecar.pid");
    let child_pid_file = workspace.join("child.pid");
    #[cfg(unix)]
    let script = format!(
        "/usr/bin/printf '%s' \"$PPID\" > {}; /bin/sleep 300 & child=$!; /usr/bin/printf '%s' \"$child\" > {}; wait \"$child\"",
        shell_quote(&sidecar_pid_file),
        shell_quote(&child_pid_file),
    );
    #[cfg(unix)]
    let request = serde_json::json!({
        "script": script,
        "shell": "sh",
        "timeout_secs": 300,
        "workspace": workspace.display().to_string(),
    });
    #[cfg(unix)]
    let operation = "command.run_shell";
    #[cfg(windows)]
    let request = {
        let helper = workspace.join("host-crash-helper.exe");
        std::fs::copy(std::env::current_exe()?, &helper)
            .context("复制 Windows 宿主崩溃测试程序失败")?;
        let helper = helper.display().to_string().replace('\\', "/");
        serde_json::json!({
            "cmd": format!("\"{helper}\""),
            "args": [
                "--windows-command-tree-helper",
                workspace.join("command.pid").display().to_string(),
                child_pid_file.display().to_string(),
            ],
            "cwd": workspace.display().to_string(),
            "timeout_secs": 300,
            "workspace": workspace.display().to_string(),
        })
    };
    #[cfg(windows)]
    let operation = "command.run_command";
    let context = SidecarInvocationContext {
        session_id: "host-crash".to_string(),
        invocation_id: "host-crash".to_string(),
        authoritative_workspace: workspace,
    };
    connection.invoke_with_context(operation, &request.to_string(), &context)?;
    bail!("command 长请求意外结束")
}

#[cfg(windows)]
fn run_windows_command_tree_helper(command_pid_file: &Path, child_pid_file: &Path) -> Result<()> {
    let command_pid = std::process::id();
    std::fs::write(command_pid_file, command_pid.to_string())?;

    let mut child = std::process::Command::new(std::env::current_exe()?)
        .arg("--windows-wait-helper")
        .spawn()
        .context("启动 Windows 宿主崩溃测试子进程失败")?;
    std::fs::write(child_pid_file, child.id().to_string())?;
    child
        .wait()
        .context("等待 Windows 宿主崩溃测试子进程失败")?;
    Ok(())
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
