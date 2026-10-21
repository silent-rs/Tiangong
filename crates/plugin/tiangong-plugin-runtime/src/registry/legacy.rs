//! 旧插件 ID 迁移：合并保留目录（runtime/logs/data）、归档旧目录。

use super::*;

pub(super) const PRESERVED_ENTRIES: [&str; 3] = ["runtime", "logs", "data"];

/// 旧编号插件 ID → 统一命名后的新 ID。
///
/// 「统一官方插件命名」去掉官方插件 ID 的 `-handler` 后缀后，宿主与前端
/// 全部链路只认新编号；存量安装目录不会自动跟随（插件目录亦无对应制品
/// 可供升级检测），需在装载前迁移，否则旧插件注册的工具接到已退役的
/// 链路上，表现为工具可调用但界面不响应（如 web_fetch 不再弹出浏览器）。
const LEGACY_PLUGIN_ID_MAP: [(&str, &str); 3] = [
    ("browser-handler", "browser"),
    ("terminal-handler", "terminal"),
    ("interaction-handler", "interaction"),
];

/// 存量插件编号迁移（幂等，仅做文件操作，失败不阻塞装载）：
///
/// - 新编号已安装：把旧目录的 data/runtime/logs 并入新目录（不覆盖已有
///   文件，保留会话等用户数据），旧目录整体归档到 `.legacy-plugins/`；
/// - 新编号未安装：给旧目录打禁用标记，避免半失联的工具继续注册；
///   用户安装新编号插件后，下次启动自动完成数据并入。
pub fn migrate_legacy_plugin_ids(storage_root: &Path) {
    for (legacy_id, current_id) in LEGACY_PLUGIN_ID_MAP {
        let legacy_dir = plugin_directory(storage_root, legacy_id);
        if !legacy_dir.join(MANIFEST_FILE).is_file() {
            continue;
        }
        let current_dir = plugin_directory(storage_root, current_id);
        if current_dir.join(MANIFEST_FILE).is_file() {
            merge_preserved_entries(&legacy_dir, &current_dir);
            archive_legacy_directory(storage_root, legacy_id);
            tracing::info!(legacy_id, current_id, "旧编号插件数据已并入新编号并归档");
        } else if !legacy_dir.join(DISABLED_MARKER).is_file() {
            match std::fs::write(legacy_dir.join(DISABLED_MARKER), "旧编号插件待迁移\n") {
                Ok(()) => tracing::warn!(
                    legacy_id,
                    current_id,
                    "旧编号插件存在而新编号未安装，已禁用；安装新编号插件后将自动并入数据"
                ),
                Err(error) => {
                    tracing::warn!(%error, legacy_id, "标记旧编号插件禁用失败")
                }
            }
        }
    }
}

/// 把旧目录中安装过程需保留的三个子目录并入新目录，不覆盖已有文件。
fn merge_preserved_entries(legacy_dir: &Path, current_dir: &Path) {
    for entry in PRESERVED_ENTRIES {
        let source = legacy_dir.join(entry);
        if !source.is_dir() {
            continue;
        }
        if let Err(error) = merge_directory_contents(&source, &current_dir.join(entry)) {
            tracing::warn!(%error, entry, "并入旧编号插件数据失败");
        }
    }
}

fn merge_directory_contents(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)
        .with_context(|| format!("创建目录失败: {}", target.display()))?;
    for entry in
        std::fs::read_dir(source).with_context(|| format!("读取目录失败: {}", source.display()))?
    {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.path().is_dir() {
            merge_directory_contents(&entry.path(), &destination)?;
        } else if !destination.exists() {
            std::fs::copy(entry.path(), &destination)
                .with_context(|| format!("复制文件失败: {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// 把旧编号插件目录整体移入 `.legacy-plugins/` 归档（移出插件扫描范围）。
fn archive_legacy_directory(storage_root: &Path, legacy_id: &str) {
    let legacy_dir = plugin_directory(storage_root, legacy_id);
    let archive_root = storage_root.join(".legacy-plugins");
    let mut target = archive_root.join(legacy_id);
    if target.exists() {
        // 归档位已占用（如用户手动恢复过旧目录）：追加时间戳避免覆盖。
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();
        target = archive_root.join(format!("{legacy_id}-{stamp}"));
    }
    if let Err(error) =
        std::fs::create_dir_all(&archive_root).and_then(|()| std::fs::rename(&legacy_dir, &target))
    {
        tracing::warn!(%error, legacy_id, "归档旧编号插件目录失败");
    }
}
