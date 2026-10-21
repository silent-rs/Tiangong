//! 解释器形态 sidecar 端到端测试：宿主 stdio 连接 ↔ Node 协议库真实往返。
//!
//! 覆盖：解释器分派 spawn、握手、请求往返、崩溃换代重启、内容清单篡改检测。
//! 运行环境无 node 时跳过（不失败）。

use std::path::PathBuf;
use std::time::Duration;

use tiangong_plugin_runtime::manifest::SidecarLifecycle;
use tiangong_plugin_runtime::sidecar::{
    InterpreterLaunch, SidecarConfig, SidecarConnection, StdioSidecarConnection,
};

/// 在 PATH 中查找 node；找不到返回 None（测试跳过）。
fn find_node() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TIANGONG_NODE_PATH") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let candidates = if cfg!(windows) {
        ["node.exe"]
    } else {
        ["node"]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|directory| {
            candidates
                .iter()
                .map(move |candidate| directory.join(candidate))
        })
        .find(|path| path.is_file())
}

fn sdk_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/sdk-sidecar/index.mjs")
}

/// 组装临时 sidecar 项目：vendor 协议库 + demo 操作入口。
fn write_sidecar_project(base: &std::path::Path, text_marker: &str) -> PathBuf {
    let sdk_dir = base.join("sidecar/vendor/tiangong-sidecar-sdk");
    std::fs::create_dir_all(&sdk_dir).unwrap();
    std::fs::copy(sdk_source(), sdk_dir.join("index.mjs")).unwrap();
    let entry = base.join("sidecar/main.mjs");
    std::fs::write(
        &entry,
        format!(
            r#"
import {{ runSidecar, SidecarError }} from './vendor/tiangong-sidecar-sdk/index.mjs';
await runSidecar({{
  pluginId: 'node-e2e',
  pluginVersion: '0.1.0',
  dispatch(operation, payload, ctx) {{
    if (operation === 'demo.echo') {{
      ctx.progress('echo 处理中');
      return {{ payload: {{ text: typeof payload?.text === 'string' ? payload.text : '', marker: '{text_marker}', pid: process.pid }} }};
    }}
    if (operation === 'demo.crash') {{
      process.exit(1);
    }}
    throw new SidecarError(`未知操作: ${{operation}}`, 'bad_request');
  }},
}});
"#
        ),
    )
    .unwrap();
    entry
}

fn connection(entry: PathBuf, integrity_manifest: Option<PathBuf>) -> StdioSidecarConnection {
    connection_with_lifecycle(entry, integrity_manifest, SidecarLifecycle::Resident)
}

fn connection_with_lifecycle(
    entry: PathBuf,
    integrity_manifest: Option<PathBuf>,
    lifecycle: SidecarLifecycle,
) -> StdioSidecarConnection {
    let base = entry.parent().unwrap().parent().unwrap().to_path_buf();
    let config = SidecarConfig::new(
        "node-e2e",
        "0.1.0",
        entry.clone(),
        base.join("runtime/endpoint.json"),
        base.join("logs/sidecar.log"),
        base.join("data"),
        base.join("storage"),
    )
    .with_timeouts(Duration::from_secs(15), Duration::from_secs(15))
    .with_lifecycle(lifecycle)
    .with_interpreter(InterpreterLaunch {
        kind: tiangong_plugin_runtime::interpreter_env::InterpreterKind::Node,
        entry,
        args: Vec::new(),
    });
    let config = match integrity_manifest {
        Some(path) => config.with_integrity_manifest(path),
        None => config,
    };
    StdioSidecarConnection::new(config)
}

#[test]
fn node_interpreter_roundtrip_and_restart() {
    // 运行期解释器由宿主缓存入口解析；此处仅确认环境可用，不可用即跳过
    if find_node().is_none() {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let can_terminate = tiangong_plugin_runtime::test_support::can_terminate_child_processes();
    let base = std::env::temp_dir().join(format!("tiangong-node-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let entry = write_sidecar_project(&base, "first");
    let connection = connection(entry, None);

    // 握手 + 请求往返（含进度）。
    let raw = connection
        .invoke("demo.echo", r#"{"text":"hello node"}"#)
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(response["text"], "hello node");
    assert_eq!(response["marker"], "first");

    // 崩溃后代换重启。
    let _ = connection.invoke("demo.crash", "{}");
    std::thread::sleep(Duration::from_millis(300));
    let raw = connection
        .invoke("demo.echo", r#"{"text":"again"}"#)
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(response["text"], "again");

    tiangong_plugin_runtime::test_support::finish_stdio_connection(&connection, can_terminate);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn node_interpreter_lifecycle_on_demand_vs_resident() {
    // 运行期解释器由宿主缓存入口解析；此处仅确认环境可用，不可用即跳过
    if find_node().is_none() {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let can_terminate = tiangong_plugin_runtime::test_support::can_terminate_child_processes();
    let invoke_pid = |lifecycle: SidecarLifecycle| -> (i64, i64) {
        let base = std::env::temp_dir().join(format!(
            "tiangong-node-lifecycle-{:?}-{}",
            lifecycle,
            std::process::id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let entry = write_sidecar_project(&base, "lc");
        let connection = connection_with_lifecycle(entry, None, lifecycle);
        let first: serde_json::Value =
            serde_json::from_str(&connection.invoke("demo.echo", r#"{"text":"a"}"#).unwrap())
                .unwrap();
        let second: serde_json::Value =
            serde_json::from_str(&connection.invoke("demo.echo", r#"{"text":"b"}"#).unwrap())
                .unwrap();
        tiangong_plugin_runtime::test_support::finish_stdio_connection(&connection, can_terminate);
        let _ = std::fs::remove_dir_all(&base);
        (
            first["pid"].as_i64().unwrap(),
            second["pid"].as_i64().unwrap(),
        )
    };

    // 按需：每次调用独立进程（pid 不同）；常驻：跨调用复用（pid 相同）。
    let (a, b) = invoke_pid(SidecarLifecycle::OnDemand);
    assert_ne!(a, b, "按需模式两次调用应为不同进程");
    let (a, b) = invoke_pid(SidecarLifecycle::Resident);
    assert_eq!(a, b, "常驻模式两次调用应复用同一进程");
}

#[test]
fn node_interpreter_tampered_entry_rejected() {
    // 运行期解释器由宿主缓存入口解析；此处仅确认环境可用，不可用即跳过
    if find_node().is_none() {
        eprintln!("跳过：PATH 中未找到 node");
        return;
    };
    let base = std::env::temp_dir().join(format!("tiangong-node-tamper-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let entry = write_sidecar_project(&base, "original");

    // 构造内容清单（路径 + sha256），再篡改入口。
    let manifest_path = base.join("content-manifest.json");
    let files = [
        ("sidecar/main.mjs", std::fs::read(&entry).unwrap()),
        (
            "sidecar/vendor/tiangong-sidecar-sdk/index.mjs",
            std::fs::read(sdk_source()).unwrap(),
        ),
    ];
    use sha2::Digest;
    let entries: Vec<serde_json::Value> = files
        .iter()
        .map(|(path, raw)| {
            serde_json::json!({
                "path": path,
                "sha256": hex::encode(sha2::Sha256::digest(raw)),
            })
        })
        .collect();
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&serde_json::json!({ "algorithm": "sha256", "files": entries }))
            .unwrap(),
    )
    .unwrap();
    std::fs::write(&entry, "// tampered\n").unwrap();

    let connection = connection(entry, Some(manifest_path));
    let error = connection
        .invoke("demo.echo", r#"{"text":"x"}"#)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("篡改"),
        "篡改入口未被拒绝: {error:#}"
    );
    let _ = std::fs::remove_dir_all(&base);
}
