//! 插件收件箱导入（`plugin-dev.import`）的端到端测试：经公开桥接入口
//! [`tiangong_plugin_runtime::plugin_dev::call`] 驱动，覆盖受信安装方授权、
//! 用户确认（fail-closed / 拒绝 / 批准）、暂存摘要正确性与安装落地全链路。
//!
//! 受信安装方判定与导入确认回调是进程内全局注入：全部场景收在单个
//! integration 测试内按序切换，避免与其他测试的执行顺序耦合；场景间
//! 的状态迁移（未注入 → 注入拒绝 → 注入批准）本身就是被测行为。

use std::path::{Path, PathBuf};

use serde_json::json;
use tiangong_plugin_runtime::plugin_dev::PLUGIN_INBOX_DIR;

const CREATOR_ID: &str = "plugin-creator";

/// 收件箱产物（纯 UI 模板形态）：plugin.json + dist + 内容清单。
fn make_inbox_artifact(root: &Path, id: &str) -> PathBuf {
    let dir = root.join("plugins").join(PLUGIN_INBOX_DIR).join(id);
    std::fs::create_dir_all(dir.join("dist")).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        r#"{"schema_version":2,"id":"ID_PLACEHOLDER","version":"1.0.0","permissions":[],"entrypoints":["desktop"],"capabilities":{"prompt":true},"prompt":["外来说明"],"ui":{"contributions":[{"slot":"extension.tab","id":"app","title":"导入验证","entry":"dist/index.html"}]}}"#
            .replace("ID_PLACEHOLDER", id),
    )
    .unwrap();
    std::fs::write(dir.join("dist/index.html"), "<html></html>").unwrap();
    write_content_manifest(&dir);
    dir
}

/// devkit build 同款内容清单：全树（排除清单自身）路径 + sha256。
fn write_content_manifest(release: &Path) {
    use sha2::Digest;
    let mut files = Vec::new();
    let mut stack = vec![release.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                let path = entry.path();
                let relative = path.strip_prefix(release).unwrap().display().to_string();
                if relative == "content-manifest.json" {
                    continue;
                }
                let raw = std::fs::read(&path).unwrap();
                files.push(json!({
                    "path": relative.replace('\\', "/"),
                    "sha256": hex::encode(sha2::Sha256::digest(&raw)),
                }));
            }
        }
    }
    std::fs::write(
        release.join("content-manifest.json"),
        json!({"algorithm": "sha256", "files": files}).to_string(),
    )
    .unwrap();
}

/// 安装占位的官方创作链调用方（plugin-creator），使 `plugin_dev::call`
/// 能从注册表解析其安装目录——桥接入口的调用方前提。
fn install_creator_placeholder(root: &Path) {
    let dev = tiangong_plugin_runtime::test_support::write_dev_project(root, CREATOR_ID);
    let release = dev.join("release");
    std::fs::create_dir_all(release.join("dist")).unwrap();
    std::fs::write(
        release.join("plugin.json"),
        r#"{"schema_version":2,"id":"plugin-creator","version":"0.0.1","permissions":[],"ui":{"contributions":[{"slot":"extension.tab","id":"app","title":"占位","entry":"dist/index.html"}]}}"#,
    )
    .unwrap();
    std::fs::write(release.join("dist/index.html"), "<html></html>").unwrap();
    write_content_manifest(&release);
    tiangong_plugin_runtime::test_support::install_dev_project(root, CREATOR_ID)
        .expect("占位 creator 安装");
}

fn call_import(caller: &str, id: &str) -> Result<serde_json::Value, String> {
    tiangong_plugin_runtime::plugin_dev::call(
        caller,
        "plugin-dev.import",
        &json!({"id": id}).to_string(),
    )
    .map(|raw| serde_json::from_str(&raw).unwrap())
    .map_err(|error| error.to_string())
}

#[test]
#[serial_test::serial]
fn integration_inbox导入_授权确认与安装全链路() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    tiangong_config::registry::init_from_dir(&config_dir);
    install_creator_placeholder(root.path());
    make_inbox_artifact(root.path(), "inbox-demo");

    // 1) 受信安装方未注入（e2e 进程初始态）：fail-closed。
    let error = call_import(CREATOR_ID, "inbox-demo").unwrap_err();
    assert!(
        error.contains("受信安装方"),
        "未注入受信安装方判定必须拒绝：{error}"
    );

    // 2) 注入受信安装方，但导入确认回调未注入：仍 fail-closed。
    tiangong_plugin_runtime::set_plugin_dev_trusted_installer(std::sync::Arc::new(
        |identity: &tiangong_plugin_runtime::InstallerIdentity| identity.plugin_id == CREATOR_ID,
    ));
    let error = call_import(CREATOR_ID, "inbox-demo").unwrap_err();
    assert!(
        error.contains("导入确认回调"),
        "未注入确认回调必须拒绝导入：{error}"
    );
    assert!(
        !root.path().join("plugins/inbox-demo/plugin.json").is_file(),
        "拒绝路径不得产生安装"
    );

    // 3) 用户拒绝（回调返回 false）：失败且不安装。
    tiangong_plugin_runtime::set_plugin_dev_import_confirm(std::sync::Arc::new(
        |_summary: &tiangong_plugin_runtime::ImportSummary| false,
    ));
    let error = call_import(CREATOR_ID, "inbox-demo").unwrap_err();
    assert!(error.contains("未批准"), "用户拒绝必须失败：{error}");
    assert!(!root.path().join("plugins/inbox-demo").exists());

    // 4) 用户批准：暂存摘要字段正确，安装落地并带用户密钥签名清单。
    tiangong_plugin_runtime::set_plugin_dev_import_confirm(std::sync::Arc::new(
        |summary: &tiangong_plugin_runtime::ImportSummary| {
            assert_eq!(summary.plugin_id, "inbox-demo");
            assert_eq!(summary.version, "1.0.0");
            assert!(
                summary.file_count >= 2,
                "摘要至少覆盖 plugin.json 与内容清单"
            );
            true
        },
    ));
    let result = call_import(CREATOR_ID, "inbox-demo").expect("批准后导入成功");
    assert_eq!(result["plugin_id"], json!("inbox-demo"));
    assert_eq!(result["version"], json!("1.0.0"));
    assert!(
        root.path().join("plugins/inbox-demo/plugin.json").is_file(),
        "批准后应落入正式安装目录"
    );
    assert!(
        root.path()
            .join("plugins/inbox-demo/release.json")
            .is_file(),
        "导入产物应获得用户密钥签名清单"
    );

    // 5) 收件箱目录名路径逃逸与缺失产物。
    for bad in ["..", "../escape", "a/b"] {
        let error = call_import(CREATOR_ID, bad).unwrap_err();
        assert!(
            error.contains("收件箱目录名"),
            "路径逃逸必须拒绝：{bad} -> {error}"
        );
    }
    let error = call_import(CREATOR_ID, "ghost").unwrap_err();
    assert!(
        error.contains("收件箱中没有插件"),
        "缺失产物应给出明确指引：{error}"
    );
}
