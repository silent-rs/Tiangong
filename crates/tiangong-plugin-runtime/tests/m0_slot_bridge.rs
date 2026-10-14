//! M0 端到端验证：v1 WASM 插件经 Slot + Host Bridge 渲染设置页。
//!
//! 使用运行时测试插件 runtime-probe，覆盖：
//! - v1 清单按旧规则解析（声明 sidecar 但制品缺失时插件仍可加载）
//! - `settings.plugin-page` Slot 列出 v1 插件贡献
//! - `open_view` 返回设置页 HTML
//! - `bridge.call("plugin.*")` 完成读 → 写 → 读回的双向通信闭环，数据落在
//!   宿主挂载的插件数据目录 `<存储根>/data/<plugin_id>`
//! - 未知 method / v1 调用宿主能力命名空间被拒绝；声明过其他权限的 v1 插件
//!   走 `plugin.*` 不被权限层拒绝

use std::path::Path;

use tiangong_plugin_runtime::bridge_call;
use tiangong_plugin_runtime::registry::{
    ContributionSource, list_slot_contributions, open_view, preload_installed_plugins,
};
use tiangong_plugin_runtime::test_support::runtime_probe_wasm;

const PLUGIN_ID: &str = "runtime-probe";

/// 准备受管临时安装目录：v1 + sidecar 清单，目录中故意不放置 sidecar 二进制，
/// 验证 sidecar 不可用时插件仍保持加载、设置页贡献与页面桥接可用。
fn stage_plugin(root: &Path) {
    let dir = root.join("plugins").join(PLUGIN_ID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(runtime_probe_wasm(), dir.join("runtime_probe.wasm")).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{
            "schema_version": 1,
            "id": "runtime-probe",
            "version": "1.0.0",
            "wasm": { "binary": "runtime_probe.wasm" },
            "sidecar": {
                "binary": "runtime-probe-sidecar",
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
    let root = tempfile::TempDir::new().unwrap();
    // 存储根隔离到临时目录：插件数据目录落在这里，不读写真实的 ~/.tiangong。
    // 本测试文件只有这一个测试，独占进程环境。
    unsafe {
        std::env::set_var("TIANGONG_STORAGE_ROOT", root.path());
    }
    stage_plugin(root.path());

    // ── 1. v1 清单按旧规则解析并加载 ──
    assert_eq!(preload_installed_plugins(root.path()), 1, "v1 插件应被识别");

    // ── 2. settings.plugin-page Slot 列出 v1 贡献（wasm 来源）──
    let contributions = list_slot_contributions("settings.plugin-page");
    assert_eq!(contributions.len(), 1, "应列出测试插件的设置页贡献");
    let entry = &contributions[0];
    assert_eq!(entry.plugin_id, PLUGIN_ID);
    assert_eq!(entry.contribution_id, "probe-settings");
    assert_eq!(entry.source, ContributionSource::Wasm);
    assert!(entry.has_view);

    // ── 3. open_view 返回设置页 HTML ──
    let html = open_view(PLUGIN_ID, "probe-settings").expect("设置页应可打开");
    assert!(html.contains("runtime-probe"), "{html}");

    // ── 4. bridge.call("plugin.*") 双向通信：读 → 写 → 读回 ──
    let initial = bridge_call(PLUGIN_ID, "plugin.get", "").expect("读取应经宿主桥接成功");
    assert_eq!(initial, "", "数据目录初始为空");

    let probe = "m0-e2e-bridge-probe";
    let written = bridge_call(PLUGIN_ID, "plugin.set", probe).expect("写入应经宿主桥接成功");
    assert_eq!(written, "true");
    let updated = bridge_call(PLUGIN_ID, "plugin.get", "").expect("读回失败");
    assert_eq!(updated, probe, "写读闭环应经桥接保持一致");
    // 写入落在宿主为插件挂载的数据目录。
    assert_eq!(
        std::fs::read_to_string(root.path().join("data").join(PLUGIN_ID).join("probe.txt"))
            .unwrap(),
        probe
    );

    // ── 5. 未知 method 被拒绝并给出可读错误 ──
    let error = bridge_call(PLUGIN_ID, "rag.query", "{}").unwrap_err();
    assert!(format!("{error:#}").contains("拒绝未知 method"));

    // v1 插件调用宿主能力命名空间被拒（需升级 schema_version 2），
    // 而不是白名单内未接入命名空间的「尚未接入」。
    let error = bridge_call(PLUGIN_ID, "session.getMessages", "{}").unwrap_err();
    assert!(format!("{error:#}").contains("schema_version 1"));

    // ── 6. v1 + 非空 permissions 插件走 plugin.* 放行（回归 generate-image-openai）──
    // 插件声明了 sidecar.invoke 但没有 bridge.call；plugin.* 等价旧 plugin_call
    // 透传通道，不得因声明过其他权限被误拒：未知视图消息应由 WASM 自身报错。
    let error = bridge_call(PLUGIN_ID, "plugin.bootstrap", "{}").unwrap_err();
    let message = format!("{error:#}");
    assert!(
        !message.contains("权限"),
        "v1 + 非空 permissions 插件走 plugin.* 不应被权限层拒绝，实际: {message}"
    );
}
