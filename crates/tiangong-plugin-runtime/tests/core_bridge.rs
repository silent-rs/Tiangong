//! runtime 聚合桥（`RuntimeCorePlugin`）的差量语义验证。
//!
//! Core 只持有桥这一个编译期插件成员；已安装插件的能力经桥聚合可见。
//! 本文件验证：新装插件下一次聚合自然出现、卸载后消失、并发聚合收敛
//! 到同一实例、聚合结果跨次稳定（Core 视角的「稳定插件集合」）。

use std::path::Path;
use std::sync::Arc;

use tiangong_core::tools::extension::{PromptSectionProvider, ToolSpecProvider};
use tiangong_plugin_runtime::RuntimeCorePlugin;

/// 全局插件注册表是进程级单例，本文件用例经此锁串行执行。
static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ensure_config() {
    if tiangong_config::registry::try_models().is_none() {
        tiangong_config::registry::init_from_dir(Path::new("/nonexistent"));
    }
}

/// 造一个纯 manifest 的 Desktop TS 工具插件（无 wasm/sidecar）。
/// 校验链：schema v2 + entrypoints 仅 desktop + tool.provide 权限 +
/// capabilities.tools。
fn stage_ts_tool_plugin(root: &Path, id: &str, tool: &str) {
    let dir = root.join("plugins").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        format!(
            r#"{{"schema_version":2,"id":"{id}","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide"],"capabilities":{{"tools":true}},"tools":[{{"name":"{tool}","description":"测试工具","input_schema":{{"type":"object"}}}}]}}"#
        ),
    )
    .unwrap();
}

fn tool_names(plugin: &RuntimeCorePlugin) -> Vec<String> {
    let mut names = plugin
        .tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn 桥聚合_新装插件下一轮可见且卸载后消失() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    stage_ts_tool_plugin(root.path(), "bridge-view-a", "bridge_a_tool");
    assert!(
        tiangong_plugin_runtime::registry::preload_installed_plugins(root.path()) >= 1,
        "插件 A 应装入注册表"
    );

    let bridge = RuntimeCorePlugin::desktop(root.path().to_path_buf());
    // 固定通道工具（call_local_plugin/list_local_plugins）恒在声明中，
    // 逐项断言插件工具的存在性而非全集相等。
    let has = |names: &[String], expect: &str| names.iter().any(|n| n == expect);
    assert!(
        has(&tool_names(&bridge), "bridge_a_tool"),
        "首轮聚合应见 A 工具"
    );

    // 模拟新装插件 B：registry 出现新记录，下一次聚合即交付（无需通知）。
    stage_ts_tool_plugin(root.path(), "bridge-view-b", "bridge_b_tool");
    tiangong_plugin_runtime::registry::preload_installed_plugins(root.path());
    let after_install = tool_names(&bridge);
    assert!(has(&after_install, "bridge_a_tool") && has(&after_install, "bridge_b_tool"));

    // prompt 段落聚合为空（两个插件都不声明 prompt），不 panic。
    assert!(bridge.prompt_sections().is_empty());

    // 模拟卸载 B：registry 记录消失，下一次聚合不再交付。
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "bridge-view-b", false)
        .expect("卸载测试插件 B");
    let after_uninstall = tool_names(&bridge);
    assert!(
        has(&after_uninstall, "bridge_a_tool") && !has(&after_uninstall, "bridge_b_tool"),
        "卸载后 B 工具消失、其余不受影响"
    );
    // 注册表是进程级全局，preload 只增量合并：测试插件必须各自卸载，
    // 否则残留条目污染后续测试的清单/指纹断言。
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "bridge-view-a", false)
        .expect("清理测试插件 A");
}

/// 并发聚合首见同一新插件：装载在锁外进行，双方结果一致且线程不卡死。
#[test]
fn 桥聚合_并发首见收敛() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    stage_ts_tool_plugin(root.path(), "bridge-view-c", "bridge_c_tool");
    tiangong_plugin_runtime::registry::preload_installed_plugins(root.path());

    let bridge = Arc::new(RuntimeCorePlugin::desktop(root.path().to_path_buf()));
    let (tx, rx) = std::sync::mpsc::channel();
    let mut handles = Vec::new();
    for _ in 0..4 {
        let bridge = bridge.clone();
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            let names = tool_names(&bridge);
            tx.send(names).expect("回传聚合结果");
        }));
    }
    drop(tx);
    let first = rx.recv().expect("至少一个结果");
    for names in rx {
        assert_eq!(names, first, "并发聚合结果必须一致");
    }
    for handle in handles {
        handle.join().expect("并发聚合线程不得 panic 或卡死");
    }
    assert!(
        first.iter().any(|n| n == "bridge_c_tool"),
        "并发聚合应交付 C 工具（固定通道工具亦在声明中）：{first:?}"
    );
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "bridge-view-c", false)
        .expect("清理测试插件 C");
}

// ── 自制插件动态调用通道（local 签名 → 固定工具 + 对话内清单）──

use tiangong_core::session::Session;
use tiangong_core::tools::extension::ToolOverrideHandler;
use tiangong_llm::tool::ToolCall;
use tiangong_plugin_runtime::registry::{
    is_local_plugin, local_plugin_inventory, preload_installed_plugins,
};
use tiangong_plugin_runtime::signature::{
    SIGNED_RELEASE_FILE, SIGNED_RELEASE_SCHEMA_VERSION, SignedArtifact, SignedPluginRelease,
};
use tiangong_plugin_runtime::trust::{
    LOCAL_PUBLISHER, ensure_user_signing_key, sign_with_user_key,
};

fn sha256_of(path: &std::path::Path) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(std::fs::read(path).unwrap()))
}

/// 造一个 local 签名的纯 TS 工具插件（用户密钥签名，publisher=local）。
fn stage_local_signed_plugin(root: &std::path::Path, id: &str, tool: &str) {
    stage_local_signed_manifest(
        root,
        id,
        &format!(
            r#"{{"schema_version":2,"id":"{id}","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide"],"capabilities":{{"tools":true}},"tools":[{{"name":"{tool}","description":"测试工具","input_schema":{{"type":"object"}}}}]}}"#
        ),
    );
}

/// 造一个 local 签名插件，manifest 内容自定义（签名流程与上方一致）。
fn stage_local_signed_manifest(root: &std::path::Path, id: &str, manifest_body: &str) {
    let dir = root.join("plugins").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.json"), manifest_body).unwrap();
    let manifest =
        tiangong_plugin_runtime::manifest::PluginManifest::load(&dir.join("plugin.json")).unwrap();
    ensure_user_signing_key(root).unwrap();
    let release = SignedPluginRelease {
        schema_version: SIGNED_RELEASE_SCHEMA_VERSION,
        id: id.to_string(),
        version: manifest.version.clone(),
        publisher: LOCAL_PUBLISHER.to_string(),
        permissions: manifest.permissions.clone(),
        manifest: SignedArtifact {
            path: "plugin.json".into(),
            sha256: sha256_of(&dir.join("plugin.json")),
        },
        wasm: None,
        ui: Vec::new(),
        sidecar: None,
        content_manifest: None,
    };
    std::fs::write(
        dir.join(SIGNED_RELEASE_FILE),
        serde_json::to_vec_pretty(&release).unwrap(),
    )
    .unwrap();
    sign_with_user_key(root, &dir.join(SIGNED_RELEASE_FILE)).unwrap();
}

fn local_call(plugin: &str, function: &str) -> ToolCall {
    ToolCall {
        id: format!("t_{}", scru128::new()),
        name: "call_local_plugin".to_string(),
        arguments: serde_json::json!({
            "plugin_name": plugin,
            "function_name": function,
            "args": {}
        }),
    }
}

#[test]
fn 固定通道_自制插件不进声明_清单与判据正确() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    stage_local_signed_plugin(root.path(), "local-bridge-a", "local_a_tool");
    stage_ts_tool_plugin(root.path(), "unsigned-bridge-b", "unsigned_b_tool");
    assert!(
        preload_installed_plugins(root.path()) >= 2,
        "两个测试插件都应装入注册表"
    );

    // 判据：local 签名 vs 未签名。
    assert!(
        is_local_plugin("local-bridge-a"),
        "local 签名插件应命中判据"
    );
    assert!(
        !is_local_plugin("unsigned-bridge-b"),
        "未签名插件不走固定通道"
    );

    let bridge = RuntimeCorePlugin::desktop(root.path().to_path_buf());
    let names = tool_names(&bridge);
    // 固定通道工具在声明中（description 恒定）。
    assert!(names.iter().any(|n| n == "call_local_plugin"));
    assert!(names.iter().any(|n| n == "list_local_plugins"));
    // 自制插件的方法不进 tools 声明（保护 KV cache 前缀）。
    assert!(
        !names.iter().any(|n| n == "local_a_tool"),
        "自制插件方法不得进入 tools 声明"
    );
    // 未签名插件保持逐个声明的现状。
    assert!(
        names.iter().any(|n| n == "unsigned_b_tool"),
        "未签名插件保持独立声明"
    );

    // 清单：只含自制插件，含版本与方法签名。
    let inventory = local_plugin_inventory();
    let plugins = inventory["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1, "清单只含自制插件：{inventory}");
    assert_eq!(plugins[0]["name"], "local-bridge-a");
    assert_eq!(plugins[0]["functions"][0]["name"], "local_a_tool");
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "local-bridge-a", false)
        .expect("清理测试插件 A");
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "unsigned-bridge-b", false)
        .expect("清理测试插件 B");
}

#[tokio::test]
// std 锁跨 await 是刻意的串行保护：同二进制内持锁者即本测试，
// 运行路径无死锁；测试代码允许局部豁免。
#[allow(clippy::await_holding_lock)]
async fn 固定通道_方法不存在返回可用方法() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    stage_local_signed_plugin(root.path(), "local-bridge-c", "local_c_tool");
    preload_installed_plugins(root.path());

    let bridge = RuntimeCorePlugin::desktop(root.path().to_path_buf());
    let mut session = Session::new("local-call-missing-fn");
    let result = bridge
        .handle(
            &local_call("local-bridge-c", "no_such_fn"),
            &mut session,
            "test",
        )
        .await
        .expect("固定通道应拦截并返回结果");
    assert!(!result.ok);
    assert!(
        result.stderr.contains("无方法 no_such_fn"),
        "{}",
        result.stderr
    );
    assert!(
        result.stderr.contains("可用方法：local_c_tool"),
        "错误信息应带可用方法：{}",
        result.stderr
    );
    // 注册表是进程级全局：测试插件必须真正卸载，否则残留条目会污染
    // 后续测试的清单断言（preload 只增量合并，不清理扫描不到的插件）。
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "local-bridge-c", false)
        .unwrap();
}

#[tokio::test]
// std 锁跨 await 是刻意的串行保护：同二进制内持锁者即本测试，
// 运行路径无死锁；测试代码允许局部豁免。
#[allow(clippy::await_holding_lock)]
async fn 固定通道_插件卸载后实时返回清单() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    stage_local_signed_plugin(root.path(), "local-bridge-d", "local_d_tool");
    preload_installed_plugins(root.path());

    let bridge = RuntimeCorePlugin::desktop(root.path().to_path_buf());
    // 卸载后路由实时感知（不依赖聚合缓存）。
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "local-bridge-d", false)
        .unwrap();
    let mut session = Session::new("local-call-gone");
    let result = bridge
        .handle(
            &local_call("local-bridge-d", "local_d_tool"),
            &mut session,
            "test",
        )
        .await
        .expect("固定通道应拦截并返回结果");
    assert!(!result.ok);
    assert!(
        result.stderr.contains("local-bridge-d 已不存在"),
        "{}",
        result.stderr
    );
}

#[tokio::test]
// std 锁跨 await 是刻意的串行保护：同二进制内持锁者即本测试，
// 运行路径无死锁；测试代码允许局部豁免。
#[allow(clippy::await_holding_lock)]
async fn 固定通道_查询工具返回清单() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    stage_local_signed_plugin(root.path(), "local-bridge-e", "local_e_tool");
    preload_installed_plugins(root.path());

    let bridge = RuntimeCorePlugin::desktop(root.path().to_path_buf());
    let mut session = Session::new("local-list");
    let call = ToolCall {
        id: "t_list".to_string(),
        name: "list_local_plugins".to_string(),
        arguments: serde_json::json!({}),
    };
    let result = bridge
        .handle(&call, &mut session, "test")
        .await
        .expect("查询工具应返回结果");
    assert!(result.ok);
    assert!(
        result.stdout.contains("local-bridge-e"),
        "{}",
        result.stdout
    );
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "local-bridge-e", false)
        .expect("清理测试插件 E");
}

// ── 压缩链路口径回归：指纹只计入进入模型请求前缀的插件 ──

use tiangong_plugin_runtime::registry::enabled_plugin_fingerprint;

/// 未签名（官方/三方通道）纯 manifest 插件，内容自定义。
fn stage_unsigned_manifest(root: &Path, id: &str, manifest_body: &str) {
    let dir = root.join("plugins").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.json"), manifest_body).unwrap();
}

#[test]
fn 指纹只计入进入模型前缀的插件() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    // 基线：未签名带工具插件计入指纹。
    stage_ts_tool_plugin(root.path(), "unsigned-fp-a", "unsigned_fp_tool");
    preload_installed_plugins(root.path());
    let base = enabled_plugin_fingerprint();

    // local 签名工具插件：能力经对话内清单提供，不进 tools 声明与
    // system prompt → 装卸不得改变交接指纹。
    stage_local_signed_plugin(root.path(), "local-fp-b", "local_fp_tool");
    preload_installed_plugins(root.path());
    assert_eq!(
        enabled_plugin_fingerprint(),
        base,
        "自制插件装卸不得触发上下文交接"
    );
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "local-fp-b", false).unwrap();
    assert_eq!(enabled_plugin_fingerprint(), base);

    // 纯 UI 插件（无 tools 无 prompt，与发布者无关）：对模型请求零影响。
    stage_unsigned_manifest(
        root.path(),
        "unsigned-fp-ui",
        r#"{"schema_version":2,"id":"unsigned-fp-ui","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide"],"capabilities":{"tools":true},"tools":[]}"#,
    );
    preload_installed_plugins(root.path());
    assert_eq!(
        enabled_plugin_fingerprint(),
        base,
        "纯 UI 插件不得触发上下文交接"
    );

    // WASM 形态插件：manifest 的 tools/prompt 恒为空（工具声明在组件内），
    // 判据不得只看 manifest 声明字段——否则官方 WASM 插件升级漏出交接。
    // （预加载会因二进制非真实 WASM 而编译失败，但记录仍按安装态登记，
    // 指纹读的是声明态，正好验证「计入与否与加载成败无关」。）
    let wasm_dir = root.path().join("plugins").join("unsigned-wasm-fp");
    std::fs::create_dir_all(&wasm_dir).unwrap();
    std::fs::write(
        wasm_dir.join("plugin.json"),
        r#"{"schema_version":1,"id":"unsigned-wasm-fp","version":"0.1.0","wasm":{"binary":"fixture.wasm"}}"#,
    )
    .unwrap();
    std::fs::write(wasm_dir.join("fixture.wasm"), b"not-a-real-wasm").unwrap();
    preload_installed_plugins(root.path());
    assert_ne!(
        enabled_plugin_fingerprint(),
        base,
        "WASM 插件（manifest 声明 wasm 制品）必须计入指纹"
    );
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "unsigned-wasm-fp", false)
        .unwrap();
    assert_eq!(
        enabled_plugin_fingerprint(),
        base,
        "卸载 WASM 插件后指纹复原"
    );

    // 对照：未签名工具插件的装卸仍如实改变指纹。
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "unsigned-fp-a", false)
        .unwrap();
    assert_ne!(
        enabled_plugin_fingerprint(),
        base,
        "进入请求前缀的插件变化必须反映到指纹"
    );
}

#[test]
fn 自制插件prompt不进系统段落_随清单注入() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    // local 签名的 prompt-only 插件：prompt 段落不进 system prompt（装卸
    // 会打穿 KV cache 前缀），内容随清单注入对话历史。
    stage_local_signed_manifest(
        root.path(),
        "local-prompt-a",
        r#"{"schema_version":2,"id":"local-prompt-a","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide"],"capabilities":{"tools":true,"prompt":true},"tools":[],"prompt":["自制插件专属段落"]}"#,
    );
    // 未签名的 prompt-only 插件：保持现状——prompt 进 system prompt、
    // 计入指纹（官方/三方通道未变）。
    stage_unsigned_manifest(
        root.path(),
        "unsigned-prompt-b",
        r#"{"schema_version":2,"id":"unsigned-prompt-b","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide"],"capabilities":{"tools":true,"prompt":true},"tools":[],"prompt":["未签名插件段落"]}"#,
    );
    assert!(
        preload_installed_plugins(root.path()) >= 2,
        "两个 prompt 插件都应装入注册表"
    );

    let bridge = RuntimeCorePlugin::desktop(root.path().to_path_buf());
    let sections = bridge.prompt_sections();
    assert!(
        !sections.iter().any(|s| s.contains("自制插件专属段落")),
        "自制插件 prompt 不得进入 system prompt：{sections:?}"
    );
    assert!(
        sections.iter().any(|s| s.contains("未签名插件段落")),
        "未签名插件 prompt 保持现状进入 system prompt：{sections:?}"
    );

    // 清单携带 prompt 内容：纯 prompt 的自制插件也出现在清单里，
    // 与「不进 system prompt」的分流两头一致。
    let inventory = local_plugin_inventory();
    let entry = inventory["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "local-prompt-a")
        .expect("纯 prompt 自制插件应出现在清单");
    assert_eq!(
        entry["prompt"][0], "自制插件专属段落",
        "清单应携带 prompt 内容：{entry}"
    );
    // 注册表是进程级全局，preload 只增量合并：残留插件会污染其他测试
    // 的 prompt/清单断言，各自卸载。
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "local-prompt-a", false)
        .expect("清理自制 prompt 插件");
    tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), "unsigned-prompt-b", false)
        .expect("清理未签名 prompt 插件");
}

#[test]
fn 桥聚合_重名工具按插件前缀暴露并路由到各自插件() {
    use tiangong_core::tools::extension::ToolOverrideHandler;
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    stage_ts_tool_plugin(root.path(), "dup-alpha", "dup_tool");
    stage_ts_tool_plugin(root.path(), "dup-beta", "dup_tool");
    stage_ts_tool_plugin(root.path(), "dup-gamma", "gamma_tool");
    tiangong_plugin_runtime::registry::preload_installed_plugins(root.path());

    let bridge = RuntimeCorePlugin::desktop(root.path().to_path_buf());
    let names = tool_names(&bridge);
    assert!(
        names.contains(&"dup-alpha__dup_tool".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"dup-beta__dup_tool".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"gamma_tool".to_string()),
        "不重名的工具保持原名：{names:?}"
    );
    assert!(
        !names.contains(&"dup_tool".to_string()),
        "重名原名不应再暴露：{names:?}"
    );

    let call = |name: &str| tiangong_llm::tool::ToolCall {
        id: "c".to_string(),
        name: name.to_string(),
        arguments: serde_json::json!({}),
    };
    assert_eq!(
        bridge
            .result_header(&call("dup-beta__dup_tool"), true)
            .as_deref(),
        Some("调用插件 dup-beta 的 dup_tool：成功"),
        "抬头应落到实际插件与原名"
    );
    assert_eq!(
        bridge.result_header(&call("gamma_tool"), false).as_deref(),
        Some("调用插件 dup-gamma 的 gamma_tool：失败")
    );
    assert_eq!(
        bridge
            .result_header(
                &tiangong_llm::tool::ToolCall {
                    id: "c".to_string(),
                    name: "call_local_plugin".to_string(),
                    arguments: serde_json::json!({"plugin_name": "demo", "function_name": "greet"}),
                },
                true
            )
            .as_deref(),
        Some("调用插件 demo 的 greet：成功"),
        "自制插件通道的抬头落到实际插件方法"
    );

    for id in ["dup-alpha", "dup-beta", "dup-gamma"] {
        tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), id, false).unwrap();
    }
}

#[test]
fn 工具图标_插件声明优先_不合法声明不影响装载() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    ensure_config();
    let root = tempfile::TempDir::new().unwrap();

    // 未声明图标的插件：回落 runtime 内置表。
    stage_ts_tool_plugin(root.path(), "icon-plain", "run_shell");
    // 声明图标的插件：含合法资源、插件默认图标与不合法条目。
    let dir = root.path().join("plugins").join("icon-rich");
    std::fs::create_dir_all(dir.join("icons")).unwrap();
    std::fs::write(
        dir.join("icons/speak.svg"),
        "<svg xmlns='http://www.w3.org/2000/svg'/>",
    )
    .unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"schema_version":2,"id":"icon-rich","version":"0.1.0","entrypoints":["desktop"],"permissions":["tool.provide"],"capabilities":{"tools":true},
            "tools":[
              {"name":"speak","description":"测试","input_schema":{"type":"object"}},
              {"name":"read_file","description":"测试","input_schema":{"type":"object"}},
              {"name":"run_shell","description":"测试","input_schema":{"type":"object"}},
              {"name":"other_tool","description":"测试","input_schema":{"type":"object"}}
            ],
            "tool_icons":{"speak":"icons/speak.svg","read_file":"../escape.svg","*":"flame"}}"#,
    )
    .unwrap();
    tiangong_plugin_runtime::registry::preload_installed_plugins(root.path());

    let bridge = RuntimeCorePlugin::desktop(root.path().to_path_buf());
    let names = tool_names(&bridge);
    assert!(
        names.contains(&"speak".to_string()),
        "声明图标的插件应正常装载：{names:?}"
    );

    let table = tiangong_plugin_runtime::registry::list_tool_icons();
    assert_eq!(table["speak"].icon, "icons/speak.svg");
    assert_eq!(table["speak"].plugin_id.as_deref(), Some("icon-rich"));
    assert_eq!(table["other_tool"].icon, "flame", "插件默认图标");
    assert_eq!(
        table["icon-rich__read_file"].icon, "flame",
        "不合法条目被忽略，落到插件默认图标"
    );
    assert_eq!(
        table["icon-plain__run_shell"].icon, "square-terminal",
        "未声明时用内置表"
    );
    assert_eq!(
        table["icon-rich__run_shell"].icon, "flame",
        "重名时各自按插件解析"
    );

    let (bytes, mime) =
        tiangong_plugin_runtime::registry::read_plugin_tool_icon("icon-rich", "icons/speak.svg")
            .unwrap();
    assert_eq!(mime, "image/svg+xml");
    assert!(!bytes.is_empty());
    assert!(
        tiangong_plugin_runtime::registry::read_plugin_tool_icon("icon-rich", "plugin.json")
            .is_err(),
        "未声明的文件不可读取"
    );
    assert!(
        tiangong_plugin_runtime::registry::read_plugin_tool_icon("icon-rich", "../escape.svg")
            .is_err()
    );

    for id in ["icon-plain", "icon-rich"] {
        tiangong_plugin_runtime::registry::uninstall_plugin(root.path(), id, false).unwrap();
    }
}
