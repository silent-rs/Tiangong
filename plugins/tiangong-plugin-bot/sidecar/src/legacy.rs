//! 旧版 Bot（宿主 `tiangong-bots` 管理的独立制品）数据迁移。
//!
//! 旧版数据位于 `<存储根>/bots/`：`bots.json` 记录实例（id、平台制品、
//! 启用状态、手工填写的凭据），`<实例 id>/` 下保存扫码凭据
//! `credentials.json` 与推送目标 `targets.json`。两种文件格式与本插件
//! 一致，迁移只需复制。
//!
//! 规则（主进程启动时执行一次，可重复执行）：
//! - 平台目录已有凭据或已迁移标记时跳过，不覆盖用户在插件内的配置；
//! - 只迁移旧版已启用的实例：本插件有凭据即建立接收连接，迁移未启用实例
//!   会让原本停用的平台开始收消息；
//! - 同一平台多个已启用实例时取最近使用的（推送目标最近时间）；
//! - 凭据优先取实例目录的扫码凭据，其次 `bots.json` 手工配置；
//! - 推送目标只在平台目录尚无目标时复制；投递去重记录不迁移；
//! - 旧目录原样保留，`mcp.json` 中旧 Bot 的 MCP 条目不处理。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const LEGACY_DIR: &str = "bots";
const LEGACY_CONFIG: &str = "bots.json";
const CREDENTIALS_FILE: &str = "credentials.json";
const TARGETS_FILE: &str = "targets.json";
/// 迁移完成标记（记录来源，设置页据此提示）。
pub(crate) const MARKER_FILE: &str = "legacy-migrated.json";

#[derive(Debug, Deserialize)]
struct LegacyConfig {
    #[serde(default)]
    bots: Vec<LegacyBot>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyBot {
    id: String,
    artifact_id: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    config: serde_json::Map<String, Value>,
}

/// 迁移标记内容。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct MigrationMarker {
    /// 来源旧实例 id。
    pub(crate) source: String,
    /// 凭据来源：`provisioned`（扫码）或 `manual`（手工配置）。
    pub(crate) credentials: String,
    /// 是否复制了推送目标。
    pub(crate) targets: bool,
    /// 同平台未迁移的其他已启用旧实例。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) skipped: Vec<String>,
    pub(crate) migrated_at: String,
}

/// 迁移所有平台；单个平台失败只记录告警，不影响其他平台与插件启动。
pub(crate) fn migrate_all(storage_root: &Path, data_dir: &Path, platforms: &[&str]) {
    let legacy_dir = storage_root.join(LEGACY_DIR);
    let bots = match read_legacy_config(&legacy_dir) {
        Ok(Some(bots)) => bots,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!("读取旧版 Bot 配置失败，跳过迁移: {error:#}");
            return;
        }
    };
    for platform in platforms {
        match migrate_platform(&legacy_dir, &bots, platform, &data_dir.join(platform)) {
            Ok(Some(marker)) => {
                tracing::info!(platform, source = %marker.source, "已从旧版 Bot 迁移配置")
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(platform, "迁移旧版 Bot 配置失败: {error:#}"),
        }
    }
}

/// 读取平台目录下的迁移标记（设置页展示用）。
pub(crate) fn read_marker(platform_dir: &Path) -> Option<MigrationMarker> {
    let content = std::fs::read(platform_dir.join(MARKER_FILE)).ok()?;
    serde_json::from_slice(&content).ok()
}

fn read_legacy_config(legacy_dir: &Path) -> Result<Option<Vec<LegacyBot>>> {
    let path = legacy_dir.join(LEGACY_CONFIG);
    let content = match std::fs::read(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("读取 {}", path.display()));
        }
    };
    let config: LegacyConfig =
        serde_json::from_slice(&content).with_context(|| format!("解析 {}", path.display()))?;
    Ok(Some(config.bots))
}

fn migrate_platform(
    legacy_dir: &Path,
    bots: &[LegacyBot],
    platform: &str,
    platform_dir: &Path,
) -> Result<Option<MigrationMarker>> {
    if platform_dir.join(MARKER_FILE).exists() || platform_dir.join(CREDENTIALS_FILE).exists() {
        return Ok(None);
    }
    let mut candidates: Vec<(&LegacyBot, PathBuf)> = bots
        .iter()
        .filter(|bot| bot.enabled && bot.artifact_id == platform && is_plain_id(&bot.id))
        .map(|bot| (bot, legacy_dir.join(&bot.id)))
        .collect();
    // 推送目标最近使用时间新者优先（无目标排最后）。
    candidates.sort_by_key(|(_, dir)| std::cmp::Reverse(last_seen(dir)));
    let mut chosen = None;
    for (bot, dir) in &candidates {
        if let Some(credentials) = legacy_credentials(bot, dir, platform)? {
            chosen = Some((*bot, dir.clone(), credentials));
            break;
        }
    }
    let Some((bot, source_dir, (credentials, kind))) = chosen else {
        return Ok(None);
    };
    std::fs::create_dir_all(platform_dir)
        .with_context(|| format!("创建 {}", platform_dir.display()))?;
    crate::platform::write_credentials_file(&platform_dir.join(CREDENTIALS_FILE), &credentials)?;
    let targets = copy_targets(&source_dir, platform_dir)?;
    let marker = MigrationMarker {
        source: bot.id.clone(),
        credentials: kind.to_string(),
        targets,
        skipped: candidates
            .iter()
            .map(|(other, _)| other.id.clone())
            .filter(|id| *id != bot.id)
            .collect(),
        migrated_at: crate::delivery::now_string(),
    };
    crate::platform::write_credentials_file(&platform_dir.join(MARKER_FILE), &marker)?;
    Ok(Some(marker))
}

/// 旧实例凭据：扫码凭据优先，其次 `bots.json` 手工配置。返回 JSON 与来源。
fn legacy_credentials(
    bot: &LegacyBot,
    dir: &Path,
    platform: &str,
) -> Result<Option<(Value, &'static str)>> {
    let path = dir.join(CREDENTIALS_FILE);
    if path.is_file() {
        let value: Value = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("读取 {}", path.display()))?,
        )
        .with_context(|| format!("解析 {}", path.display()))?;
        if credentials_complete(&value, platform) {
            return Ok(Some((value, "provisioned")));
        }
    }
    let keys: &[&str] = if platform == "weixin" {
        &["bot_token"]
    } else {
        &["app_id", "app_secret"]
    };
    let mut manual = serde_json::Map::new();
    for key in keys {
        if let Some(value) = bot.config.get(*key).and_then(Value::as_str) {
            manual.insert((*key).to_string(), Value::String(value.trim().to_string()));
        }
    }
    let manual = Value::Object(manual);
    Ok(credentials_complete(&manual, platform).then_some((manual, "manual")))
}

fn credentials_complete(value: &Value, platform: &str) -> bool {
    let present = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty())
    };
    if platform == "weixin" {
        present("bot_token")
    } else {
        present("app_id") && present("app_secret")
    }
}

/// 平台目录尚无推送目标时复制旧目标（格式一致；0600 原子写）。
fn copy_targets(source_dir: &Path, platform_dir: &Path) -> Result<bool> {
    let source = source_dir.join(TARGETS_FILE);
    let destination = platform_dir.join(TARGETS_FILE);
    if !source.is_file() || destination.exists() {
        return Ok(false);
    }
    let value: Value = serde_json::from_slice(
        &std::fs::read(&source).with_context(|| format!("读取 {}", source.display()))?,
    )
    .with_context(|| format!("解析 {}", source.display()))?;
    crate::platform::write_credentials_file(&destination, &value)?;
    Ok(true)
}

/// 推送目标中最近一次使用时间（字符串比较即时间序）。
fn last_seen(dir: &Path) -> String {
    std::fs::read(dir.join(TARGETS_FILE))
        .ok()
        .and_then(|content| serde_json::from_slice::<Value>(&content).ok())
        .and_then(|store| {
            store["targets"].as_array().and_then(|targets| {
                targets
                    .iter()
                    .filter_map(|target| target["last_seen_at"].as_str())
                    .max()
                    .map(str::to_string)
            })
        })
        .unwrap_or_default()
}

/// 旧实例 id 只允许字母数字（与旧版 BotId 规则一致），防路径穿越。
fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLATFORMS: [&str; 3] = ["feishu", "qq", "weixin"];

    struct Fixture {
        _root: tempfile::TempDir,
        storage: PathBuf,
        data: PathBuf,
    }

    impl Fixture {
        fn new(bots: Value) -> Self {
            let root = tempfile::tempdir().unwrap();
            let storage = root.path().join("storage");
            let data = root.path().join("data");
            std::fs::create_dir_all(storage.join(LEGACY_DIR)).unwrap();
            std::fs::write(
                storage.join(LEGACY_DIR).join(LEGACY_CONFIG),
                bots.to_string(),
            )
            .unwrap();
            Self {
                _root: root,
                storage,
                data,
            }
        }

        fn legacy(&self, id: &str, file: &str, value: Value) {
            let dir = self.storage.join(LEGACY_DIR).join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), value.to_string()).unwrap();
        }

        fn run(&self) {
            migrate_all(&self.storage, &self.data, &PLATFORMS);
        }

        fn read(&self, platform: &str, file: &str) -> Option<Value> {
            std::fs::read(self.data.join(platform).join(file))
                .ok()
                .map(|content| serde_json::from_slice(&content).unwrap())
        }
    }

    fn targets(last_seen: &str) -> Value {
        json!({"version": 1, "targets": [{
            "target_id": "t1", "chat_id": "c1", "kind": "group",
            "enabled": true, "last_seen_at": last_seen
        }]})
    }

    #[test]
    fn migrates_provisioned_credentials_and_targets() {
        let fixture = Fixture::new(json!({"bots": [
            {"id": "feishu", "artifact_id": "feishu", "enabled": true, "config": {}}
        ]}));
        fixture.legacy(
            "feishu",
            CREDENTIALS_FILE,
            json!({"app_id": "cli_a", "app_secret": "s"}),
        );
        fixture.legacy("feishu", TARGETS_FILE, targets("2026-10-01 10:00:00"));
        fixture.run();
        assert_eq!(
            fixture.read("feishu", CREDENTIALS_FILE).unwrap()["app_id"],
            "cli_a"
        );
        assert_eq!(
            fixture.read("feishu", TARGETS_FILE).unwrap()["targets"][0]["target_id"],
            "t1"
        );
        let marker = read_marker(&fixture.data.join("feishu")).unwrap();
        assert_eq!(marker.source, "feishu");
        assert_eq!(marker.credentials, "provisioned");
        assert!(marker.targets);
        // 旧数据原样保留，其他平台无旧实例不产生任何文件。
        assert!(
            fixture
                .storage
                .join("bots/feishu")
                .join(CREDENTIALS_FILE)
                .is_file()
        );
        assert!(fixture.read("qq", MARKER_FILE).is_none());
    }

    #[test]
    fn falls_back_to_manual_config_in_bots_json() {
        let fixture = Fixture::new(json!({"bots": [
            {"id": "wx", "artifact_id": "weixin", "enabled": true,
             "config": {"bot_token": " token-123 "}}
        ]}));
        fixture.run();
        assert_eq!(
            fixture.read("weixin", CREDENTIALS_FILE).unwrap(),
            json!({"bot_token": "token-123"})
        );
        let marker = read_marker(&fixture.data.join("weixin")).unwrap();
        assert_eq!(marker.credentials, "manual");
        assert!(!marker.targets);
    }

    #[test]
    fn existing_plugin_credentials_are_never_overwritten() {
        let fixture = Fixture::new(json!({"bots": [
            {"id": "qq", "artifact_id": "qq", "enabled": true,
             "config": {"app_id": "old", "app_secret": "old"}}
        ]}));
        let dir = fixture.data.join("qq");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(CREDENTIALS_FILE),
            json!({"app_id": "new", "app_secret": "new"}).to_string(),
        )
        .unwrap();
        fixture.run();
        assert_eq!(
            fixture.read("qq", CREDENTIALS_FILE).unwrap()["app_id"],
            "new"
        );
        assert!(fixture.read("qq", MARKER_FILE).is_none());
    }

    #[test]
    fn migrates_most_recent_enabled_instance_and_records_skipped() {
        let fixture = Fixture::new(json!({"bots": [
            {"id": "fsold", "artifact_id": "feishu", "enabled": false,
             "config": {"app_id": "disabled", "app_secret": "s"}},
            {"id": "fsa", "artifact_id": "feishu", "enabled": true,
             "config": {"app_id": "older", "app_secret": "s"}},
            {"id": "fsb", "artifact_id": "feishu", "enabled": true,
             "config": {"app_id": "newer", "app_secret": "s"}}
        ]}));
        fixture.legacy("fsold", TARGETS_FILE, targets("2026-10-09 00:00:00"));
        fixture.legacy("fsa", TARGETS_FILE, targets("2026-10-01 00:00:00"));
        fixture.legacy("fsb", TARGETS_FILE, targets("2026-10-05 00:00:00"));
        fixture.run();
        assert_eq!(
            fixture.read("feishu", CREDENTIALS_FILE).unwrap()["app_id"],
            "newer"
        );
        let marker = read_marker(&fixture.data.join("feishu")).unwrap();
        assert_eq!(marker.source, "fsb");
        // 未启用实例不参与迁移，也不列入未迁移提示。
        assert_eq!(marker.skipped, vec!["fsa".to_string()]);
    }

    #[test]
    fn disabled_instances_are_not_migrated() {
        let fixture = Fixture::new(json!({"bots": [
            {"id": "qq", "artifact_id": "qq", "enabled": false, "config": {}}
        ]}));
        fixture.legacy(
            "qq",
            CREDENTIALS_FILE,
            json!({"app_id": "a", "app_secret": "s"}),
        );
        fixture.legacy("qq", TARGETS_FILE, targets("2026-10-01 00:00:00"));
        fixture.run();
        assert!(fixture.read("qq", CREDENTIALS_FILE).is_none());
        assert!(fixture.read("qq", MARKER_FILE).is_none());
    }

    #[test]
    fn rerun_after_user_clears_credentials_does_not_migrate_again() {
        let fixture = Fixture::new(json!({"bots": [
            {"id": "qq", "artifact_id": "qq", "enabled": true,
             "config": {"app_id": "a", "app_secret": "s"}}
        ]}));
        fixture.run();
        std::fs::remove_file(fixture.data.join("qq").join(CREDENTIALS_FILE)).unwrap();
        fixture.run();
        assert!(fixture.read("qq", CREDENTIALS_FILE).is_none());
    }

    #[test]
    fn incomplete_or_unsafe_instances_are_skipped() {
        let fixture = Fixture::new(json!({"bots": [
            {"id": "../evil", "artifact_id": "qq", "enabled": true,
             "config": {"app_id": "a", "app_secret": "s"}},
            {"id": "qq", "artifact_id": "qq", "enabled": true,
             "config": {"app_id": "a"}}
        ]}));
        fixture.run();
        assert!(fixture.read("qq", CREDENTIALS_FILE).is_none());
        assert!(fixture.read("qq", MARKER_FILE).is_none());
    }

    #[test]
    fn corrupt_or_missing_legacy_config_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        // 无旧目录。
        migrate_all(root.path(), &root.path().join("data"), &PLATFORMS);
        assert!(!root.path().join("data").exists());
        // 旧配置损坏：不迁移、不报错。
        std::fs::create_dir_all(root.path().join(LEGACY_DIR)).unwrap();
        std::fs::write(root.path().join(LEGACY_DIR).join(LEGACY_CONFIG), "{ 损坏").unwrap();
        migrate_all(root.path(), &root.path().join("data"), &PLATFORMS);
        assert!(!root.path().join("data").exists());
    }
}
