//! M0 端到端验证：旧 v1 WASM 插件经 Slot + Host Bridge 渲染设置页。
//!
//! 前置条件：需先构建 prompt 与 memory wasm 组件：
//! ```sh
//! cargo build -p tiangong-plugin-prompt-wasm --target wasm32-wasip2
//! cargo build -p tiangong-plugin-memory-wasm --target wasm32-wasip2
//! ```
//! 覆盖验收项（任务 spec `005-端到端验证.md`）：
//! - v1 清单按旧规则解析（prompt 纯 WASM 无 sidecar + memory 声明 sidecar 两条路径）
//! - `settings.plugin-page` Slot 列出 v1 插件贡献
//! - `open_view` 返回设置页 HTML
//! - `bridge.call("plugin.*")` 完成读 → 写 → 读回的双向通信闭环
//! - 未知 method 被拒绝

use std::path::{Path, PathBuf};

use tiangong_plugin_runtime::bridge_call;
use tiangong_plugin_runtime::registry::{
    ContributionSource, list_slot_contributions, open_view, preload_installed_plugins,
};

/// 定位 wasm32-wasip2 调试产物。
fn wasm_path(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target/wasm32-wasip2/debug");
    path.push(name);
    path
}

/// 准备受管临时安装目录：prompt（纯 WASM）+ memory（声明 sidecar、制品缺失）。
fn stage_plugins(root: &Path) {
    let prompt_dir = root.join("plugins").join("prompt");
    std::fs::create_dir_all(&prompt_dir).unwrap();
    std::fs::copy(
        wasm_path("tiangong_plugin_prompt_wasm.wasm"),
        prompt_dir.join("tiangong_plugin_prompt_wasm.wasm"),
    )
    .unwrap();
    std::fs::write(
        prompt_dir.join("plugin.json"),
        r#"{
            "schema_version": 1,
            "id": "prompt",
            "version": "0.1.0",
            "wasm": { "binary": "tiangong_plugin_prompt_wasm.wasm" },
            "permissions": [],
            "storage_access": true
        }"#,
    )
    .unwrap();

    let memory_dir = root.join("plugins").join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::copy(
        wasm_path("tiangong_plugin_memory_wasm.wasm"),
        memory_dir.join("tiangong_plugin_memory_wasm.wasm"),
    )
    .unwrap();
    // v1 + sidecar 清单：目录中故意不放置 sidecar 二进制，
    // 验证 sidecar 不可用时插件仍保持加载、设置页贡献与页面渲染可用。
    // version 须与 WASM describe 返回一致，否则清单校验拒绝加载。
    std::fs::write(
        memory_dir.join("plugin.json"),
        r#"{
            "schema_version": 1,
            "id": "memory",
            "version": "0.1.1",
            "wasm": { "binary": "tiangong_plugin_memory_wasm.wasm" },
            "sidecar": {
                "binary": "tiangong-memory-sidecar",
                "transport_protocol": "0.1.0",
                "business_protocol": 1,
                "startup_timeout_ms": 5000,
                "request_timeout_ms": 30000
            },
            "permissions": ["sidecar.invoke"]
        }"#,
    )
    .unwrap();
}

#[test]
fn v1_插件经_slot_与宿主桥接完成设置页闭环() {
    let wasm_prompt = wasm_path("tiangong_plugin_prompt_wasm.wasm");
    let wasm_memory = wasm_path("tiangong_plugin_memory_wasm.wasm");
    if !wasm_prompt.exists() || !wasm_memory.exists() {
        eprintln!(
            "跳过测试：未找到 wasm 组件（{} / {}），请先执行对应 target 的 cargo build",
            wasm_prompt.display(),
            wasm_memory.display()
        );
        return;
    }

    let root = tempfile::TempDir::new().unwrap();
    // 存储根隔离到临时目录：Prompt 插件的数据目录与旧位置都落在这里，
    // 不读写真实的 ~/.tiangong。本测试文件只有这一个测试，独占进程环境。
    unsafe {
        std::env::set_var("TIANGONG_STORAGE_ROOT", root.path());
    }
    // 旧版存储根下的自定义指令：首次访问时应迁移到 data/prompt/。
    std::fs::write(root.path().join("custom-prompt.md"), "旧位置的自定义指令").unwrap();
    stage_plugins(root.path());
    // 此处验证旧清单结构兼容性，版本仍须匹配当前实际构建的 WASM。
    let config = tiangong_plugin_runtime::PluginRuntimeConfig::default();
    let loader = tiangong_plugin_runtime::WasmPluginLoader::new(&config).unwrap();
    for id in ["prompt", "memory"] {
        let dir = root.path().join("plugins").join(id);
        let manifest_path = dir.join("plugin.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let binary = manifest["wasm"]["binary"].as_str().unwrap();
        let mut component = loader.load(&dir.join(binary), &config).unwrap();
        manifest["version"] = component.describe().unwrap().version.into();
        std::fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    }

    // ── 1. v1 清单按旧规则解析并加载 ──
    let loaded = preload_installed_plugins(root.path());
    assert_eq!(loaded, 2, "两个 v1 插件都应被识别");

    // ── 2. settings.plugin-page Slot 列出 v1 贡献（wasm 来源）──
    let contributions = list_slot_contributions("settings.plugin-page");
    assert_eq!(contributions.len(), 2, "应列出 prompt 与 memory 的贡献");
    assert!(
        contributions
            .iter()
            .all(|item| item.slot == "settings.plugin-page")
    );
    assert!(
        contributions
            .iter()
            .all(|item| item.source == ContributionSource::Wasm)
    );

    let prompt_entry = contributions
        .iter()
        .find(|item| item.plugin_id == "prompt")
        .expect("应包含 prompt 贡献");
    assert_eq!(prompt_entry.contribution_id, "prompt-settings");
    assert!(prompt_entry.has_view);

    // ── 3. open_view 返回设置页 HTML（纯 WASM 与带 sidecar 两条路径）──
    let prompt_html = open_view("prompt", "prompt-settings").expect("prompt 页面应可打开");
    assert!(!prompt_html.is_empty(), "prompt 设置页 HTML 非空");

    let memory_html = open_view("memory", "memory").expect("memory 页面应可打开");
    assert!(!memory_html.is_empty(), "memory 设置页 HTML 非空");

    // ── 4. bridge.call("plugin.*") 双向通信：读 → 写 → 读回 ──
    // 首次读取触发旧数据迁移：读到旧位置内容，且已复制到插件数据目录。
    let original =
        bridge_call("prompt", "plugin.get_prompt", "{}").expect("读取配置应经宿主桥接成功");
    let original_content = serde_json::from_str::<serde_json::Value>(&original)
        .expect("get_prompt 应返回 JSON")
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    assert_eq!(original_content, "旧位置的自定义指令", "应迁移旧位置内容");
    let data_prompt = root.path().join("data/prompt/custom-prompt.md");
    assert_eq!(
        std::fs::read_to_string(&data_prompt).unwrap(),
        "旧位置的自定义指令"
    );

    let probe = "m0-e2e-bridge-probe";
    bridge_call(
        "prompt",
        "plugin.set_prompt",
        &serde_json::to_string(&serde_json::json!({ "content": probe })).unwrap(),
    )
    .expect("写入配置应经宿主桥接成功");

    let updated = bridge_call("prompt", "plugin.get_prompt", "{}").expect("读回配置失败");
    let updated_json =
        serde_json::from_str::<serde_json::Value>(&updated).expect("get_prompt 应返回 JSON");
    let updated_content = updated_json
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    assert_eq!(updated_content, probe, "写读闭环应经新桥接保持一致");
    // 新内容只写入插件数据目录；旧文件保留原样。
    assert_eq!(std::fs::read_to_string(&data_prompt).unwrap(), probe);
    assert_eq!(
        std::fs::read_to_string(root.path().join("custom-prompt.md")).unwrap(),
        "旧位置的自定义指令"
    );

    // 清空后删除数据目录中的文件，旧内容不会被重新迁回。
    bridge_call(
        "prompt",
        "plugin.set_prompt",
        &serde_json::to_string(&serde_json::json!({ "content": "" })).unwrap(),
    )
    .expect("清空配置失败");
    assert!(!data_prompt.exists());
    let cleared = bridge_call("prompt", "plugin.get_prompt", "{}").expect("读回配置失败");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&cleared).unwrap()["content"],
        ""
    );

    // ── 5. 未知 method 被拒绝并给出可读错误 ──
    let error = bridge_call("prompt", "rag.query", "{}").unwrap_err();
    assert!(format!("{error:#}").contains("拒绝未知 method"));

    // v1 插件调用宿主能力命名空间被拒（需升级 schema_version 2），
    // 而不是白名单内未接入命名空间的「尚未接入」。
    let error = bridge_call("prompt", "session.getMessages", "{}").unwrap_err();
    assert!(format!("{error:#}").contains("schema_version 1"));

    // ── 6. v1 + 非空 permissions 插件走 plugin.* 放行（回归 generate-image-openai）──
    // memory 声明了 sidecar.invoke 等权限但没有（也不可能有）bridge.call；
    // plugin.* 等价旧 plugin_call 透传通道，不得因声明过其他权限被误拒。
    // 其 bootstrap 依赖 sidecar（测试环境缺失），期望错误是 WASM 处理失败
    // 而非「未声明权限」——即已通过权限层到达插件。
    let error = bridge_call("memory", "plugin.bootstrap", "{}").unwrap_err();
    let message = format!("{error:#}");
    assert!(
        !message.contains("未声明权限") && !message.contains("权限"),
        "v1 + 非空 permissions 插件走 plugin.* 不应被权限层拒绝，实际: {message}"
    );
}
