//! 依赖方向护栏：插件、`tiangong-plugin-sidecar` 与 `tiangong-plugin-update`
//! 不得依赖宿主 `tiangong-plugin-runtime`；`tiangong-plugin-sidecar` 也不得依赖
//! `tiangong-plugin-update`（更新约定变化不应牵动全部插件）。
//!
//! 宿主与插件共用的约定（协议、环境变量、发布约定）定义在
//! `tiangong-plugin-sidecar`，Runtime 依赖并重新导出它；反向依赖会把整个
//! 宿主（wasmtime、core、LLM 适配、沙箱）编进每个插件。

use std::path::{Path, PathBuf};

const HOST_CRATE: &str = "tiangong-plugin-runtime";
const UPDATE_CRATE: &str = "tiangong-plugin-update";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// 收集目录下的全部 Cargo.toml（跳过构建产物与前端依赖目录）。
fn cargo_manifests(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name.as_ref(),
                "target" | "node_modules" | "dist" | "release"
            ) {
                continue;
            }
            cargo_manifests(&path, found);
        } else if name == "Cargo.toml" {
            found.push(path);
        }
    }
}

/// 任一依赖表（含 target 专属与 dev/build 依赖）中出现指定 crate 即视为依赖：
/// 覆盖 `name = ...`、`name.workspace = true` 与 `[dev-dependencies.name]` 三种写法。
fn depends_on(manifest: &Path, dependency: &str) -> bool {
    let text = std::fs::read_to_string(manifest).expect("读取 Cargo.toml 失败");
    text.lines().any(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return false;
        }
        if let Some(header) = line.strip_prefix('[') {
            return header
                .trim_end_matches(']')
                .split('.')
                .any(|part| part.trim().trim_matches('"') == dependency);
        }
        line.split(['=', ' ', '.'])
            .next()
            .is_some_and(|key| key.trim_matches('"') == dependency)
    })
}

fn violations(manifests: &[PathBuf], dependency: &str) -> Vec<String> {
    manifests
        .iter()
        .filter(|manifest| depends_on(manifest, dependency))
        .map(|manifest| manifest.display().to_string())
        .collect()
}

#[test]
fn 插件与插件基础库不依赖宿主_runtime() {
    let root = workspace_root();
    let mut manifests = vec![
        root.join("crates/plugin/tiangong-plugin-sidecar/Cargo.toml"),
        root.join("crates/plugin/tiangong-plugin-update/Cargo.toml"),
    ];
    cargo_manifests(&root.join("plugins"), &mut manifests);
    assert!(manifests.len() > 10, "应扫描到插件 crate: {manifests:?}");
    let found = violations(&manifests, HOST_CRATE);
    assert!(
        found.is_empty(),
        "以下 crate 依赖了宿主 {HOST_CRATE}，共用约定应放在 tiangong-plugin-sidecar 或 tiangong-plugin-update：{found:#?}"
    );
}

#[test]
fn sidecar_运行库不依赖更新约定() {
    let root = workspace_root();
    let manifests = vec![root.join("crates/plugin/tiangong-plugin-sidecar/Cargo.toml")];
    let found = violations(&manifests, UPDATE_CRATE);
    assert!(
        found.is_empty(),
        "tiangong-plugin-sidecar 不得依赖 {UPDATE_CRATE}：更新约定变化会牵动全部插件"
    );
}
