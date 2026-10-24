//! 发布 tag 的插件清单兼容性回归（`tests/fixtures/plugin-manifests/`）。
//!
//! fixtures 原样取自各插件最后一个发布 tag 的 `plugin.json`（不改写、不整理）：
//!
//! | fixture | 来源 tag | 兼容点 |
//! |---|---|---|
//! | bot.json | plugin/bot/v0.1.3 | mention.hint 静态入口 |
//! | computer-use.json | plugin/computer-use/v0.4.2 | mention.hint 静态入口 |
//! | plugin-creator.json | plugin/plugin-creator/v0.2.3 | mention.hint 静态入口 |
//! | volcengine.json | plugin/volcengine/v0.1.2 | mention.hint + mark 静态入口 |
//! | generate-image / generate-video / speech-to-text / text-to-speech（v0.2.2） | 已下线老插件 | v1 时代清单整体兼容 |
//! | index.json | plugin/index/v0.2.9 | mention.hint（迁移 candidates_op 前的最后发布） |
//! | scheduler.json | plugin/scheduler/v0.2.7 | mention.hint（同上） |
//!
//! 清单格式演进不得让**已发布安装**在新版本宿主上解析失败——否则老插件
//! 直接失效且自动升级也可能跳过它（见 `artifacts::installed_plugin_states`
//! 的身份回落）。改动 `PluginManifest` 前先跑本测试。

use std::path::PathBuf;

use tiangong_plugin_runtime::manifest::PluginManifest;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin-manifests")
}

#[test]
fn 发布tag清单全部可解析并通过校验() {
    let mut checked = 0;
    for entry in std::fs::read_dir(fixtures_dir()).expect("fixtures 目录应存在") {
        let path = entry.expect("读取目录项失败").path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let manifest = PluginManifest::load(&path)
            .unwrap_or_else(|error| panic!("发布清单 {name} 应可解析: {error:#}"));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("发布清单 {name} 应通过校验: {error:#}"));
        checked += 1;
    }
    // 目录被清空时测试不应静默通过。
    assert!(checked >= 10, "应至少校验 10 份发布清单，实际 {checked}");
}

#[test]
fn 静态mention发布清单保留旧语义() {
    for (file, expected_hint) in [
        (
            "bot.json",
            "经 Bot 向已授权的飞书 / QQ / 微信会话主动推送消息",
        ),
        (
            "volcengine.json",
            "火山引擎：生成图片或视频、语音合成与语音识别",
        ),
    ] {
        let manifest = PluginManifest::load(&fixtures_dir().join(file)).expect("发布清单应可解析");
        let mention = manifest.mention.as_ref().expect("应保留 mention 声明");
        assert_eq!(mention.hint.as_deref(), Some(expected_hint), "{file}");
        assert!(
            mention.candidates_op.is_none(),
            "{file} 是旧发布，不应有 candidates_op"
        );
    }
}
