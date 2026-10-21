//! 存量插件编号迁移验证（统一官方插件命名，`*-handler` → 新编号）：
//! 新编号已安装时数据并入并归档旧目录；未安装时禁用旧插件待迁。

use std::path::Path;

use tiangong_plugin_runtime::registry::{migrate_legacy_plugin_ids, preload_installed_plugins};

static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 写一个最小合法的 schema_version 2 清单目录。
fn stage_plugin(root: &Path, id: &str) {
    let dir = root.join("plugins").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        format!(
            r#"{{
                "schema_version": 2,
                "id": "{id}",
                "version": "0.1.0",
                "permissions": ["bridge.call"],
                "ui": {{
                    "contributions": [
                        {{
                            "slot": "extension.tab",
                            "id": "panel",
                            "title": "面板",
                            "entry": "index.html"
                        }}
                    ]
                }}
            }}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("index.html"), "<html></html>").unwrap();
}

#[test]
fn 新编号已安装时并入数据并归档旧目录() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    stage_plugin(root.path(), "browser");
    stage_plugin(root.path(), "browser-handler");
    std::fs::create_dir_all(root.path().join("plugins/browser-handler/data/nested")).unwrap();
    std::fs::write(
        root.path()
            .join("plugins/browser-handler/data/bridge-storage.json"),
        "{\"tabs\":[]}",
    )
    .unwrap();
    std::fs::write(
        root.path()
            .join("plugins/browser-handler/data/nested/deep.log"),
        "log",
    )
    .unwrap();

    migrate_legacy_plugin_ids(root.path());

    assert!(
        !root.path().join("plugins/browser-handler").exists(),
        "旧编号目录应移出插件扫描范围"
    );
    assert!(
        root.path()
            .join(".legacy-plugins/browser-handler/plugin.json")
            .is_file(),
        "旧编号目录应整体归档"
    );
    let new_data = root.path().join("plugins/browser/data");
    assert_eq!(
        std::fs::read_to_string(new_data.join("bridge-storage.json")).unwrap(),
        "{\"tabs\":[]}",
        "旧数据文件应并入新编号目录"
    );
    assert!(
        new_data.join("nested/deep.log").is_file(),
        "子目录数据应一并并入"
    );
}

#[test]
fn 新编号未安装时禁用旧插件并在安装后完成迁移() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    stage_plugin(root.path(), "terminal-handler");
    std::fs::create_dir_all(root.path().join("plugins/terminal-handler/data")).unwrap();
    std::fs::write(
        root.path().join("plugins/terminal-handler/data/state.json"),
        "{}",
    )
    .unwrap();

    migrate_legacy_plugin_ids(root.path());

    assert!(
        root.path()
            .join("plugins/terminal-handler/.disabled")
            .is_file(),
        "新编号未安装时应禁用旧插件"
    );
    assert!(
        root.path()
            .join("plugins/terminal-handler/data/state.json")
            .is_file(),
        "禁用分支不应动旧目录数据"
    );

    // 用户安装新编号后再次启动：数据并入、旧目录归档、禁用标记随之消失。
    stage_plugin(root.path(), "terminal");
    migrate_legacy_plugin_ids(root.path());
    assert!(
        root.path()
            .join("plugins/terminal/data/state.json")
            .is_file(),
        "安装新编号后旧数据应并入"
    );
    assert!(
        !root.path().join("plugins/terminal-handler").exists(),
        "并入后旧目录应归档移出"
    );
}

#[test]
fn 无旧编号目录时迁移无副作用且不阻塞装载() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    stage_plugin(root.path(), "fs");

    migrate_legacy_plugin_ids(root.path());
    migrate_legacy_plugin_ids(root.path());

    assert_eq!(
        preload_installed_plugins(root.path()),
        1,
        "常规插件应不受迁移影响正常装载"
    );
    assert!(
        !root.path().join(".legacy-plugins").join("fs").exists(),
        "无关插件不应被归档"
    );
}
