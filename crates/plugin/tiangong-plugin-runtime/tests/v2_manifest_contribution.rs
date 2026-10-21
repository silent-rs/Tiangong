//! v2 manifest UI 贡献后端链路验证（T002 解析 + T004 列出 + T006 资源读取）。
//!
//! 用运行时测试插件 runtime-probe 构造 schema v2 插件：manifest 声明
//! `settings.plugin-page` 的 shadow/iframe 两条贡献，验证贡献按 source=manifest
//! 列出、entry HTML 读取、相对资源读取与 `../` 逃逸拒绝；以及 extension.tab
//! 贡献聚合为 App 元数据。

use std::path::{Path, PathBuf};

use tiangong_plugin_runtime::registry::{
    ContributionSource, list_extension_apps, list_slot_contributions, open_manifest_view,
    preload_installed_plugins, read_manifest_resource,
};
use tiangong_plugin_runtime::test_support::runtime_probe_wasm;

const PLUGIN_ID: &str = "runtime-probe";

/// 全局插件注册表是进程级单例，本文件用例经此锁串行执行。
static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 把测试插件制品放入 `<root>/plugins/runtime-probe/` 并写入清单。
fn stage_probe(root: &Path, manifest: &str) -> PathBuf {
    let dir = root.join("plugins").join(PLUGIN_ID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(runtime_probe_wasm(), dir.join("runtime_probe.wasm")).unwrap();
    std::fs::write(dir.join("plugin.json"), manifest).unwrap();
    dir
}

/// 准备 v2 清单插件：shadow/iframe 两条设置页贡献 + entry 资源文件。
fn stage_v2_plugin(root: &Path) {
    let dir = stage_probe(
        root,
        r#"{
            "schema_version": 2,
            "id": "runtime-probe",
            "version": "1.0.0",
            "wasm": { "binary": "runtime_probe.wasm" },
            "permissions": ["bridge.call"],
            "ui": {
                "contributions": [
                    {
                        "slot": "settings.plugin-page",
                        "id": "probe-shadow",
                        "title": "设置页（Shadow）",
                        "entry": "ui/shadow.html"
                    },
                    {
                        "slot": "settings.plugin-page",
                        "id": "probe-iframe",
                        "title": "设置页（iframe）",
                        "entry": "ui/iframe.html",
                        "sandbox": "iframe"
                    }
                ]
            }
        }"#,
    );
    std::fs::create_dir_all(dir.join("ui")).unwrap();
    std::fs::write(
        dir.join("ui").join("shadow.html"),
        "<html><body><div id=\"root\"></div><script src=\"app.js\"></script></body></html>",
    )
    .unwrap();
    std::fs::write(
        dir.join("ui").join("iframe.html"),
        "<html><body>iframe page</body></html>",
    )
    .unwrap();
    std::fs::write(
        dir.join("ui").join("app.js"),
        "bridge.call('plugin.ping', '{}');",
    )
    .unwrap();
}

#[test]
fn v2_manifest_贡献经_slot_列出并可读取_entry_与资源() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let root = tempfile::TempDir::new().unwrap();
    stage_v2_plugin(root.path());

    assert_eq!(
        preload_installed_plugins(root.path()),
        1,
        "v2 插件应正常加载"
    );

    // ── 贡献按 Slot 列出，source=manifest，sandbox 归一化正确 ──
    // （注册表为进程级单例，其他用例可能注入别的插件，按 plugin_id 过滤）
    let contributions = list_slot_contributions("settings.plugin-page")
        .into_iter()
        .filter(|item| item.plugin_id == PLUGIN_ID)
        .collect::<Vec<_>>();
    assert_eq!(contributions.len(), 2, "应列出两条 manifest 贡献");
    assert!(
        contributions
            .iter()
            .all(|item| item.source == ContributionSource::Manifest)
    );

    let shadow = contributions
        .iter()
        .find(|item| item.contribution_id == "probe-shadow")
        .unwrap();
    // 未声明 sandbox → 归一化为默认 shadow
    assert_eq!(shadow.sandbox, tiangong_plugin_runtime::SandboxKind::Shadow);
    let iframe = contributions
        .iter()
        .find(|item| item.contribution_id == "probe-iframe")
        .unwrap();
    assert_eq!(iframe.sandbox, tiangong_plugin_runtime::SandboxKind::Iframe);

    // ── entry HTML 读取（子目录 entry）──
    let html = open_manifest_view(PLUGIN_ID, "probe-shadow").unwrap();
    assert!(html.contains("app.js"));

    // ── 相对资源读取（以 entry 所在目录为根）──
    let (js, mime) = read_manifest_resource(PLUGIN_ID, "probe-shadow", "app.js").unwrap();
    assert_eq!(mime, "text/javascript");
    assert!(String::from_utf8(js).unwrap().contains("plugin.ping"));

    // ── 路径逃逸拒绝：`../` 到插件目录外 ──
    let escape =
        read_manifest_resource(PLUGIN_ID, "probe-shadow", "../../../etc/hosts").unwrap_err();
    let message = format!("{escape:#}");
    assert!(
        message.contains("逃出插件目录") || message.contains("路径无效"),
        "逃逸路径应被拒绝: {message}"
    );

    // ── 未知贡献拒绝 ──
    assert!(open_manifest_view(PLUGIN_ID, "nonexistent").is_err());
}

#[test]
fn extension_tab_贡献聚合为_app_元数据() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let root = tempfile::TempDir::new().unwrap();
    let dir = stage_probe(
        root.path(),
        r#"{
            "schema_version": 2,
            "id": "runtime-probe",
            "version": "1.0.0",
            "wasm": { "binary": "runtime_probe.wasm" },
            "permissions": ["bridge.call"],
            "ui": {
                "contributions": [
                    {
                        "slot": "extension.tab",
                        "id": "board",
                        "title": "看板",
                        "description": "任务看板面板",
                        "icon": "board",
                        "entry": "app/index.html",
                        "open_mode": "multi"
                    },
                    {
                        "slot": "extension.tab",
                        "id": "chart",
                        "title": "图表",
                        "entry": "app/index.html"
                    },
                    {
                        "slot": "settings.plugin-page",
                        "id": "settings",
                        "entry": "app/index.html"
                    }
                ]
            }
        }"#,
    );
    std::fs::create_dir_all(dir.join("app")).unwrap();
    std::fs::write(
        dir.join("app").join("index.html"),
        "<html><body>board</body></html>",
    )
    .unwrap();

    assert_eq!(preload_installed_plugins(root.path()), 1);

    // ── App 列表：仅 extension.tab 贡献，settings 贡献不进入 ──
    let apps = list_extension_apps();
    let probe_apps = apps
        .iter()
        .filter(|app| app.plugin_id == PLUGIN_ID)
        .collect::<Vec<_>>();
    assert_eq!(probe_apps.len(), 2, "两条 extension.tab 贡献聚合为 App");
    let board = probe_apps
        .iter()
        .find(|app| app.contribution_id == "board")
        .unwrap();
    assert_eq!(
        board.name, "Runtime Probe",
        "插件 descriptor 名称作为 App 名"
    );
    assert_eq!(board.title, "看板");
    assert_eq!(board.description, "任务看板面板");
    assert_eq!(board.open_mode, tiangong_plugin_runtime::OpenMode::Multi);
    assert_eq!(board.sandbox, tiangong_plugin_runtime::SandboxKind::Shadow);

    // open_mode 缺省 singleton
    let chart = probe_apps
        .iter()
        .find(|app| app.contribution_id == "chart")
        .unwrap();
    assert_eq!(
        chart.open_mode,
        tiangong_plugin_runtime::OpenMode::Singleton
    );

    // settings 贡献仍在 Slot 通道可见（不进入 App 列表）
    let settings = list_slot_contributions("settings.plugin-page")
        .into_iter()
        .find(|item| item.plugin_id == PLUGIN_ID)
        .expect("settings 贡献应经 Slot 通道可见");
    assert_eq!(settings.source, ContributionSource::Manifest);
}

#[test]
fn 拓展区目录完全由插件贡献驱动() {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let apps = list_extension_apps();
    // 官方能力（浏览器/终端/Agent Team）均以插件形态提供：代码不再写死
    // 官方内置目录，装什么插件出什么 App（卸载即消失）。
    assert!(
        apps.iter().all(|app| app.plugin_id != "__builtin__"),
        "不应存在 __builtin__ 官方写死条目"
    );
    assert!(
        apps.iter().all(|app| !app.title.trim().is_empty()),
        "插件贡献应携带标题"
    );
}
