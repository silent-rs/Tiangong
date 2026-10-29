//! MCP 插件真实产物端到端：宿主 stdio 链路直连 mcp sidecar。
//!
//! MCP 没有 WASM 层，工具声明是动态的：清单 `tools: [{"type":"sidecar"}]`
//! 条目经固定操作 `tool_specs` 在装载时拉取快照，执行按 `tool:*` 通配直连
//! （operation 即函数名 `mcp__{server}__{tool}`）。用例覆盖握手能力声明、
//! 空配置下的规格形状、直连执行的失败路径与 @提及候选。依赖已构建的
//! `tiangong-mcp-sidecar` debug 二进制，未构建时明确跳过。
use std::path::PathBuf;
use std::time::Duration;

use tiangong_plugin_runtime::manifest::PluginManifest;
use tiangong_plugin_runtime::sidecar::{SidecarConfig, SidecarConnection, StdioSidecarConnection};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn mcp_sidecar_binary() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let debug_dir = executable.parent()?.parent()?;
    let candidate = debug_dir.join(format!(
        "tiangong-mcp-sidecar{}",
        std::env::consts::EXE_SUFFIX
    ));
    candidate.is_file().then_some(candidate)
}

fn mcp_manifest() -> PluginManifest {
    let raw = std::fs::read_to_string(repo_root().join("plugins/tiangong-plugin-mcp/plugin.json"))
        .expect("读取 mcp plugin.json");
    serde_json::from_str(&raw).expect("解析 mcp plugin.json")
}

#[test]
fn mcp_清单无_wasm_且声明动态工具() {
    let manifest = mcp_manifest();
    assert!(manifest.wasm.is_none(), "mcp 不再有 WASM 层");
    assert!(
        manifest.tools.is_none(),
        "MCP 工具名单运行时确定，不静态枚举: {:?}",
        manifest.tools
    );
    assert!(manifest.sidecar.is_some(), "工具由 sidecar 承接");
    assert_eq!(
        manifest
            .mention
            .as_ref()
            .and_then(|m| m.candidates_op.as_deref()),
        Some("mcp.mention_candidates")
    );
}

#[test]
#[serial_test::serial]
fn mcp_sidecar_直连握手与工具规格端到端() {
    let Some(binary) = mcp_sidecar_binary() else {
        eprintln!("跳过：target/debug/tiangong-mcp-sidecar 尚未构建");
        return;
    };
    let can_terminate = tiangong_plugin_runtime::test_support::can_terminate_child_processes();
    let root = tempfile::tempdir().unwrap();
    let config = SidecarConfig::new(
        "mcp",
        mcp_manifest().version,
        binary,
        root.path().join("endpoint.json"),
        root.path().join("sidecar.log"),
        root.path().join("data"),
        root.path().to_path_buf(),
    )
    .with_timeouts(Duration::from_secs(20), Duration::from_secs(30));
    let connection = StdioSidecarConnection::new(config);

    // 装载验证：`tool:*` 通配即动态形态（声明快照与执行直连接同一语义）。
    let capabilities = connection.verify_capabilities().expect("握手");
    assert!(
        capabilities.iter().any(|c| c == "tool:*"),
        "握手缺少 tool:* 通配: {capabilities:?}"
    );

    let invoke = |operation: &str, payload: serde_json::Value| {
        let raw = connection
            .invoke(operation, &payload.to_string())
            .expect("调用成功");
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()
    };

    // 空配置（temp 存储根无 mcp.json）：规格为空数组，mention 入口仍在。
    let specs = invoke("tool_specs", serde_json::json!({}));
    assert_eq!(specs, serde_json::json!([]), "空配置无工具声明");
    let mention = invoke("mcp.mention_candidates", serde_json::json!({}));
    assert_eq!(mention["entry"]["hint"], "调用外部 MCP server 的工具");
    assert_eq!(mention["candidates"], serde_json::json!([]));

    // 直连执行未知工具：返回失败 ToolOutcome（不是传输错误）。
    let failure = invoke("mcp__nope__missing", serde_json::json!({"x": 1}));
    assert_eq!(failure["ok"], false, "{failure}");
    assert!(
        failure["summary"]
            .as_str()
            .unwrap()
            .contains("不是 MCP 工具")
    );

    // 工作目录变更（宿主 set_workspace 转发）：接受并重探。
    let reconfigured = invoke(
        "lifecycle.set_workspace",
        serde_json::json!({"workspace": root.path().to_string_lossy()}),
    );
    assert!(reconfigured.as_object().is_some(), "{reconfigured}");

    tiangong_plugin_runtime::test_support::finish_stdio_connection(&connection, can_terminate);
}
