//! fs 插件真实产物端到端：宿主 stdio 按需链路直连 fs sidecar。
//!
//! fs 没有 WASM 层，工具由宿主按清单 `tools` 直连 sidecar 执行：operation
//! 为工具名、payload 为调用参数，会话工作目录经请求上下文注入。用例覆盖
//! 握手能力声明（安装验证据此登记直连工具）、读写往返、工作目录来源与
//! 写操作锁事件（经进度通道上报）。依赖已构建的 `tiangong-fs-sidecar`
//! debug 二进制，未构建时明确跳过。

use std::path::PathBuf;
use std::time::Duration;

use tiangong_plugin_runtime::manifest::{PluginManifest, SidecarLifecycle};
use tiangong_plugin_runtime::sidecar::{
    SidecarConfig, SidecarConnection, SidecarInvocationContext, StdioSidecarConnection,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fs_sidecar_binary() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let debug_dir = executable.parent()?.parent()?;
    let candidate = debug_dir.join(format!(
        "tiangong-fs-sidecar{}",
        std::env::consts::EXE_SUFFIX
    ));
    candidate.is_file().then_some(candidate)
}

fn fs_manifest() -> PluginManifest {
    let raw = std::fs::read_to_string(repo_root().join("plugins/tiangong-plugin-fs/plugin.json"))
        .expect("读取 fs plugin.json");
    serde_json::from_str(&raw).expect("解析 fs plugin.json")
}

#[test]
fn fs_清单无_wasm_且工具全部由_sidecar_声明() {
    let manifest = fs_manifest();
    assert!(manifest.wasm.is_none(), "fs 不再有 WASM 层");
    let names: Vec<_> = manifest
        .tools
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, ["file_read", "file_write", "current_time"]);
    assert_eq!(
        manifest
            .sidecar
            .as_ref()
            .and_then(|sidecar| sidecar.lifecycle),
        Some(SidecarLifecycle::OnDemand),
        "直连无界面贡献之外的清单工具须为按需进程"
    );
}

#[test]
#[serial_test::serial]
fn fs_sidecar_经宿主直连执行工具() {
    let Some(binary) = fs_sidecar_binary() else {
        eprintln!("跳过：target/debug/tiangong-fs-sidecar 尚未构建");
        return;
    };
    let can_terminate = tiangong_plugin_runtime::test_support::can_terminate_child_processes();
    let manifest = fs_manifest();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("a.txt"), "hello\n").unwrap();
    let config = SidecarConfig::new(
        "fs",
        manifest.version.clone(),
        binary,
        root.path().join("endpoint.json"),
        root.path().join("sidecar.log"),
        root.path().join("data"),
        root.path().join("storage"),
    )
    .with_lifecycle(SidecarLifecycle::OnDemand)
    .with_timeouts(Duration::from_secs(15), Duration::from_secs(30));
    let connection = StdioSidecarConnection::new(config);

    // 安装验证据握手能力登记直连工具：三个工具都须声明。
    let capabilities = connection.verify_capabilities().expect("握手");
    for tool in ["file_read", "file_write", "current_time"] {
        assert!(
            capabilities.contains(&format!("tool:{tool}")),
            "握手缺少 tool:{tool}: {capabilities:?}"
        );
    }

    let context =
        SidecarInvocationContext::new("session-e2e", "call-1", workspace.to_str().unwrap());
    let call = |tool: &str, args: serde_json::Value, progress: &mut Vec<String>| {
        let raw = connection
            .invoke_with_context_and_progress(tool, &args.to_string(), &context, &mut |message| {
                progress.push(message)
            })
            .expect("工具调用");
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()
    };

    // 读：相对路径按宿主注入的会话工作目录解析。
    let mut progress = Vec::new();
    let read = call(
        "file_read",
        serde_json::json!({"action":"read","path":"a.txt"}),
        &mut progress,
    );
    assert_eq!(read["ok"], true, "{read}");
    assert!(read["stdout"].as_str().unwrap().contains("hello"));
    assert!(progress.is_empty(), "读操作不上报锁事件");

    // 写：落盘到会话工作目录，加锁/解锁经进度通道上报 file_lock_changed。
    let write = call(
        "file_write",
        serde_json::json!({"action":"write","path":"b.txt","content":"x"}),
        &mut progress,
    );
    assert_eq!(write["ok"], true, "{write}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("b.txt")).unwrap(),
        "x"
    );
    let actions: Vec<_> = progress
        .iter()
        .map(|message| serde_json::from_str::<serde_json::Value>(message).unwrap())
        .inspect(|event| assert_eq!(event["type"], "file_lock_changed"))
        .map(|event| event["action"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(actions, ["locked", "unlocked"]);

    // 参数错误：返回失败的工具结果（不是传输错误）。
    let invalid = call("file_read", serde_json::json!({}), &mut Vec::new());
    assert_eq!(invalid["ok"], false);

    let time = call("current_time", serde_json::json!({}), &mut Vec::new());
    assert_eq!(time["ok"], true);

    tiangong_plugin_runtime::test_support::finish_stdio_connection(&connection, can_terminate);
}
