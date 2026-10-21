//! 纯 UI 插件（wasm 省略）端到端验证（T016）：
//! 免逻辑层安装加载 → App 目录/Slot 贡献可见 → storage.* 桥接读写私有数据。
//! 这是「UI 优先」开发模型（设计文档 9.1）的最小闭环。

use tiangong_plugin_runtime::bridge_call;
use tiangong_plugin_runtime::registry::{
    RuntimeKind, list_extension_apps, list_plugins, list_slot_contributions,
    preload_installed_plugins, set_plugin_enabled,
};

static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn stage_ui_only_plugin(root: &std::path::Path) {
    let dir = root.join("plugins").join("com.example.board");
    std::fs::create_dir_all(dir.join("app")).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{
            "schema_version": 2,
            "id": "com.example.board",
            "version": "1.0.0",
            "permissions": ["bridge.call", "storage.private"],
            "ui": {
                "contributions": [
                    {
                        "slot": "extension.tab",
                        "id": "board",
                        "title": "看板",
                        "description": "任务看板示例",
                        "entry": "app/index.html",
                        "open_mode": "multi"
                    },
                    {
                        "slot": "settings.plugin-page",
                        "id": "board-settings",
                        "entry": "app/index.html"
                    }
                ]
            }
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("app").join("index.html"),
        "<html><body>board</body></html>",
    )
    .unwrap();
}

#[test]
fn 纯_ui_插件免_wasm_加载并经_storage_桥接读写() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    stage_ui_only_plugin(root.path());

    // ── 免 WASM 安装加载 ──
    assert_eq!(
        preload_installed_plugins(root.path()),
        1,
        "纯 UI 插件应正常加载"
    );

    // ── App 目录与 Slot 贡献可见 ──
    let apps = list_extension_apps();
    let board = apps
        .iter()
        .find(|app| app.plugin_id == "com.example.board")
        .expect("纯 UI 插件应出现在 App 目录");
    assert_eq!(board.contribution_id, "board");
    assert_eq!(board.open_mode, tiangong_plugin_runtime::OpenMode::Multi);

    let contributions = list_slot_contributions("settings.plugin-page");
    assert!(
        contributions
            .iter()
            .any(|item| item.plugin_id == "com.example.board"
                && item.contribution_id == "board-settings")
    );

    // ── storage.* 桥接读写私有数据 ──
    let set_result = bridge_call(
        "com.example.board",
        "storage.set",
        r#"{"key":"board_title","value":"我的看板"}"#,
    )
    .expect("storage.set 应成功");
    assert_eq!(set_result, "true");

    let get_result = bridge_call(
        "com.example.board",
        "storage.get",
        r#"{"key":"board_title"}"#,
    )
    .expect("storage.get 应成功");
    assert_eq!(get_result, r#""我的看板""#);

    // 未设置的键返回 null
    let missing = bridge_call("com.example.board", "storage.get", r#"{"key":"absent"}"#).unwrap();
    assert_eq!(missing, "null");

    // list / delete
    let list = bridge_call("com.example.board", "storage.list", "{}").unwrap();
    assert!(list.contains("board_title"));
    let deleted = bridge_call(
        "com.example.board",
        "storage.delete",
        r#"{"key":"board_title"}"#,
    )
    .unwrap();
    assert_eq!(deleted, "true");
    let after = bridge_call(
        "com.example.board",
        "storage.get",
        r#"{"key":"board_title"}"#,
    )
    .unwrap();
    assert_eq!(after, "null");

    // 数据落在插件 data 目录
    let storage_file = root
        .path()
        .join("plugins")
        .join("com.example.board")
        .join("data")
        .join("bridge-storage.json");
    assert!(storage_file.exists(), "桥接存储应落盘在插件私有数据目录");

    // plugin.*（逻辑层转发）对无逻辑层插件不可用
    let error = bridge_call("com.example.board", "plugin.anything", "{}").unwrap_err();
    assert!(
        format!("{error:#}").contains("处理消息失败") || format!("{error:#}").contains("未加载")
    );
}

#[test]
fn 纯_ui_插件启停状态即时且不受其他无效插件影响() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    tiangong_config::registry::init_from_dir(root.path());
    stage_ui_only_plugin(root.path());
    assert_eq!(preload_installed_plugins(root.path()), 1);

    // 目标插件启停不应扫描其他插件。即使存在无效清单，也应立即返回目标插件状态。
    let invalid_dir = root.path().join("plugins").join("invalid-neighbor");
    std::fs::create_dir_all(&invalid_dir).unwrap();
    std::fs::write(invalid_dir.join("plugin.json"), "{ not valid json").unwrap();

    let disabled = set_plugin_enabled(root.path(), "com.example.board", false).unwrap();
    assert!(!disabled.enabled);
    assert_eq!(disabled.state, "disabled");

    let enabled = set_plugin_enabled(root.path(), "com.example.board", true).unwrap();
    assert!(enabled.enabled);
    assert_eq!(enabled.state, "loaded");
    assert_eq!(
        enabled.loaded_version, None,
        "纯 UI 插件不需要 WASM 描述信息"
    );

    let refreshed = list_plugins(root.path(), RuntimeKind::Desktop)
        .into_iter()
        .find(|status| status.id == "com.example.board")
        .expect("刷新列表后仍应找到纯 UI 插件");
    assert_eq!(
        refreshed.state, enabled.state,
        "单插件操作与刷新列表状态应一致"
    );
}

/// 回归：preload 对同 id 但不同 storage root 的安装目录必须以最后一次
/// 为准（此前的登记只按 id 判重，沿用旧目录导致桥接存储写进已失效的
/// 旧根——"启停"测试先跑时"免 wasm"测试偶发失败）。
#[test]
fn 预加载_同id不同存储根时以最后一次预加载为准() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    let first = tempfile::TempDir::new().unwrap();
    let second = tempfile::TempDir::new().unwrap();
    stage_ui_only_plugin(first.path());
    stage_ui_only_plugin(second.path());
    assert_eq!(preload_installed_plugins(first.path()), 1);
    assert_eq!(preload_installed_plugins(second.path()), 1);

    bridge_call(
        "com.example.board",
        "storage.set",
        r#"{"key":"root","value":"second"}"#,
    )
    .expect("storage.set 应成功");
    let storage_file = second
        .path()
        .join("plugins")
        .join("com.example.board")
        .join("data")
        .join("bridge-storage.json");
    assert!(
        storage_file.exists(),
        "桥接存储必须落在最后一次预加载的存储根"
    );
}
