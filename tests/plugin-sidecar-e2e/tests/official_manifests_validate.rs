//! 官方插件清单护栏：`plugins/` 下所有正式插件的 plugin.json 必须能通过
//! 清单校验。
//!
//! 背景：schema v1 清单声明 `mention` 等字段会被校验拒绝（插件无法加载），
//! 但清单校验工具只核对版本号，曾出现 v1+mention 组合溜进仓库直到 review
//! 才发现。本测试遍历全部官方清单逐一 validate，防止同类问题复发。

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    // plugin-sidecar-e2e/ → tests/ → 仓库根
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("定位仓库根失败")
}

#[test]
fn official_plugin_manifests_all_validate() {
    let plugins_dir = repo_root().join("plugins");
    let mut checked = 0;
    let mut failures = Vec::new();

    let mut entries: Vec<_> = std::fs::read_dir(&plugins_dir)
        .expect("读取 plugins 目录失败")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("plugin.json").is_file())
        .collect();
    entries.sort();

    for plugin_dir in entries {
        let manifest_path = plugin_dir.join("plugin.json");
        let raw = std::fs::read_to_string(&manifest_path).expect("读取 plugin.json 失败");
        let manifest: tiangong_plugin_runtime::manifest::PluginManifest =
            serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("{} 反序列化失败: {e}", manifest_path.display()));
        if let Err(error) = manifest.validate() {
            failures.push(format!("{}: {error:#}", manifest_path.display()));
        }
        // 资源形态的工具图标须随插件分发：文件存在且位于 resources 声明目录内
        // （否则安装时不会被复制，界面回落默认图标）。
        for icon in manifest.tool_icons.iter().flat_map(|icons| icons.values()) {
            if !tiangong_plugin_runtime::tool_icons::is_icon_resource(icon) {
                continue;
            }
            let declared = manifest.resources.iter().flatten().any(|root| {
                let root = std::path::Path::new(root);
                std::path::Path::new(icon).starts_with(root) && std::path::Path::new(icon) != root
            });
            if !declared {
                failures.push(format!(
                    "{}: 工具图标 {icon} 不在 resources 声明的目录内",
                    manifest_path.display()
                ));
            }
            if !plugin_dir.join(icon).is_file() {
                failures.push(format!(
                    "{}: 工具图标文件 {icon} 不存在",
                    manifest_path.display()
                ));
            }
        }
        checked += 1;
    }

    assert!(checked > 0, "未发现任何插件清单，测试目录定位可能错误");
    assert!(
        failures.is_empty(),
        "{} 个官方插件清单校验失败：\n{}",
        failures.len(),
        failures.join("\n")
    );
}
