//! 工具图标：界面工具行展示用的图标查询表。
//!
//! 优先级（高 → 低）：
//! 1. 插件在 `plugin.json` 的 `tool_icons` 中为该工具声明的图标；
//! 2. 插件在 `tool_icons` 中用 `*` 声明的默认图标；
//! 3. runtime 内置表（按工具原名，集中在 [`crate::builtin_tables`]，插件普遍声明后整体删除）；
//! 4. 都没有时不给出，前端使用默认图标（工具交叉）。
//!
//! 通用 + 特指：键 `工具名` 为该工具的通用图标；键 `工具名:参数=取值`
//! （如 `file_read:action=list`）为调用参数取该值时的特指图标，前端按调用
//! 参数选用，参数未到或取值无特指声明时使用通用图标。值始终为字符串，不
//! 识别特指键的旧版宿主只是多出一个永不命中的条目。
//!
//! 图标值为宿主内置图标名（小写字母、数字与 `-`），或插件目录内 png/svg/jpeg
//! 相对路径。声明不合法时只跳过该条并告警，不影响插件装载与运行。

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::Serialize;
use tiangong_core::tools::extension::namespaced_tool_name;

use crate::builtin_tables::{BUILTIN_TOOL_ICONS, builtin_tool_icon};

/// `tool_icons` 中表示「本插件其余工具」的键。
pub const PLUGIN_DEFAULT_ICON_KEY: &str = "*";

/// 特指键中工具名与参数条件的分隔符（`工具名:参数=取值`）。
const ARG_KEY_SEPARATOR: char = ':';

/// 特指键中参数名的长度上限。
const MAX_ARG_NAME_LEN: usize = 64;

/// 图标资源扩展名白名单（与 UI 贡献图标一致）。
const ICON_RESOURCE_EXTENSIONS: &[&str] = &["png", "svg", "jpg", "jpeg"];

/// 查询表中的一项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolIcon {
    /// 通用图标：图标名，或插件目录内的图标资源相对路径。只声明了特指
    /// 图标、无通用图标可用时为空串（前端用默认图标）。
    pub icon: String,
    /// 图标来自的插件；资源图标据此读取文件，内置表条目为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// 特指图标：参数名 → 参数取值 → 图标（均来自 `plugin_id` 插件的声明）。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub variants: BTreeMap<String, BTreeMap<String, String>>,
}

impl ToolIcon {
    fn fixed(icon: impl Into<String>, plugin_id: Option<&str>) -> Self {
        Self {
            icon: icon.into(),
            plugin_id: plugin_id.map(str::to_string),
            variants: BTreeMap::new(),
        }
    }

    /// 按调用参数选图标：命中特指声明时用特指图标，否则用通用图标。
    pub fn icon_for_args(&self, args: &serde_json::Value) -> &str {
        self.variants
            .iter()
            .find_map(|(arg, values)| {
                args.get(arg)
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| values.get(value))
            })
            .map_or(self.icon.as_str(), String::as_str)
    }
}

/// 解析工具键（`tool_icons` 的键与工具视图贡献的 `tools` 项共用）：通用键
/// 返回 `(工具名, None)`，特指键返回 `(工具名, Some((参数名, 取值)))`；
/// 特指键格式不合法时返回 None。
pub fn parse_tool_key(key: &str) -> Option<(&str, Option<(&str, &str)>)> {
    let Some((tool, condition)) = key.split_once(ARG_KEY_SEPARATOR) else {
        return Some((key, None));
    };
    let (arg, value) = condition.split_once('=')?;
    let valid_arg = !arg.is_empty()
        && arg.len() <= MAX_ARG_NAME_LEN
        && arg
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    (!tool.is_empty() && tool != PLUGIN_DEFAULT_ICON_KEY && valid_arg && !value.is_empty())
        .then_some((tool, Some((arg, value))))
}

/// 参与建表的插件信息：插件 id、`tool_icons` 声明与插件声明的工具原名。
pub struct PluginToolIcons<'a> {
    pub plugin_id: &'a str,
    pub declared: Option<&'a BTreeMap<String, String>>,
    pub tools: Vec<String>,
}

/// 图标值是否为资源路径形态（含 `/` 或 `.`）。
pub fn is_icon_resource(icon: &str) -> bool {
    icon.contains('/') || icon.contains('.')
}

/// 图标值是否合法：内置图标名（小写字母、数字、`-`）或安全的白名单资源路径。
pub fn is_valid_icon(icon: &str) -> bool {
    let icon = icon.trim();
    if icon.is_empty() || icon.len() > 256 {
        return false;
    }
    if !is_icon_resource(icon) {
        return icon
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
    }
    let path = Path::new(icon);
    let safe = path.components().all(|component| {
        !matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    safe && ICON_RESOURCE_EXTENSIONS.contains(&extension.as_str())
}

/// 取插件声明中的合法图标；不合法时告警并视为未声明。
fn declared_icon(
    plugin_id: &str,
    declared: Option<&BTreeMap<String, String>>,
    key: &str,
) -> Option<String> {
    let icon = declared?.get(key)?.trim();
    if is_valid_icon(icon) {
        Some(icon.to_string())
    } else {
        tracing::warn!(plugin = %plugin_id, tool = %key, icon = %icon, "插件声明的工具图标不合法，已忽略");
        None
    }
}

/// 插件为该工具声明的合法特指图标：参数名 → 取值 → 图标。
fn declared_variants(
    plugin_id: &str,
    declared: Option<&BTreeMap<String, String>>,
    tool: &str,
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut variants: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for key in declared.into_iter().flat_map(BTreeMap::keys) {
        if !key.contains(ARG_KEY_SEPARATOR) {
            continue;
        }
        let Some((name, Some((arg, value)))) = parse_tool_key(key) else {
            tracing::warn!(plugin = %plugin_id, key = %key, "插件声明的特指工具图标键不合法，已忽略");
            continue;
        };
        if name != tool {
            continue;
        }
        if let Some(icon) = declared_icon(plugin_id, declared, key) {
            variants
                .entry(arg.to_string())
                .or_default()
                .insert(value.to_string(), icon);
        }
    }
    variants
}

/// 解析单个插件工具的图标（插件声明优先，其次内置表）；特指图标只来自插件声明。
pub fn resolve_plugin_tool_icon(
    plugin_id: &str,
    declared: Option<&BTreeMap<String, String>>,
    tool: &str,
) -> Option<ToolIcon> {
    let variants = declared_variants(plugin_id, declared, tool);
    let mut icon = if let Some(icon) = declared_icon(plugin_id, declared, tool)
        .or_else(|| declared_icon(plugin_id, declared, PLUGIN_DEFAULT_ICON_KEY))
    {
        ToolIcon::fixed(icon, Some(plugin_id))
    } else if let Some(icon) = builtin_tool_icon(tool) {
        ToolIcon::fixed(icon, None)
    } else if !variants.is_empty() {
        ToolIcon::fixed("", None)
    } else {
        return None;
    };
    if !variants.is_empty() {
        // 特指图标均为本插件声明：资源图标按本插件读取（内置表通用图标是
        // 图标名，不受 plugin_id 影响）。
        icon.plugin_id = Some(plugin_id.to_string());
        icon.variants = variants;
    }
    Some(icon)
}

/// 建立工具图标查询表：键为工具对外名 `{插件id}__{工具名}` 与原名（旧会话历史）。
///
/// 先放入内置表（覆盖无插件归属的工具，如 core 内置注入工具），再按插件
/// 覆盖；查不到的工具不在表中，由前端使用默认图标。
pub fn build_tool_icon_table<'a>(
    plugins: impl IntoIterator<Item = PluginToolIcons<'a>>,
) -> BTreeMap<String, ToolIcon> {
    let mut table: BTreeMap<String, ToolIcon> = BUILTIN_TOOL_ICONS
        .iter()
        .map(|(name, icon)| ((*name).to_string(), ToolIcon::fixed(*icon, None)))
        .collect();
    for plugin in plugins {
        for tool in &plugin.tools {
            let Some(icon) = resolve_plugin_tool_icon(plugin.plugin_id, plugin.declared, tool)
            else {
                continue;
            };
            if let Some(exposed) = namespaced_tool_name(plugin.plugin_id, tool) {
                table.insert(exposed, icon.clone());
            }
            table.insert(tool.clone(), icon);
        }
    }
    table
}

/// 工具在终端（CLI）中的字形：按查询表取图标（对外名优先，其次原名），
/// 再经 [`crate::builtin_tables::terminal_glyph`] 换成字形；查不到或为资源
/// 图标时用默认字形。
pub fn terminal_glyph_for(table: &BTreeMap<String, ToolIcon>, tool_name: &str) -> &'static str {
    let base = tool_name
        .split_once("__")
        .filter(|_| !tool_name.starts_with("mcp__"))
        .map_or(tool_name, |(_, base)| base);
    table
        .get(tool_name)
        .or_else(|| table.get(base))
        .and_then(|entry| crate::builtin_tables::terminal_glyph(&entry.icon))
        .unwrap_or(crate::builtin_tables::DEFAULT_TERMINAL_GLYPH)
}

/// 资源图标是否确为该插件 `tool_icons` 中声明的值（读取文件前的校验；
/// 通用与特指声明均覆盖）。
pub fn declares_icon_resource(declared: Option<&BTreeMap<String, String>>, icon: &str) -> bool {
    is_icon_resource(icon)
        && is_valid_icon(icon)
        && declared.is_some_and(|map| map.values().any(|value| value.trim() == icon))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn 插件声明优先于内置表() {
        let icons = declared(&[("generate_image", "palette")]);
        let icon = resolve_plugin_tool_icon("volcengine", Some(&icons), "generate_image").unwrap();
        assert_eq!(icon.icon, "palette");
        assert_eq!(icon.plugin_id.as_deref(), Some("volcengine"));
    }

    #[test]
    fn 插件默认图标优先于内置表_未声明时回落内置表() {
        let icons = declared(&[("*", "flame")]);
        assert_eq!(
            resolve_plugin_tool_icon("volcengine", Some(&icons), "generate_image")
                .unwrap()
                .icon,
            "flame"
        );
        let builtin = resolve_plugin_tool_icon("terminal", None, "run_shell").unwrap();
        assert_eq!(builtin.icon, "square-terminal");
        assert_eq!(builtin.plugin_id, None);
        assert_eq!(resolve_plugin_tool_icon("demo", None, "unknown_tool"), None);
    }

    #[test]
    fn 不合法声明被忽略且不影响其余解析() {
        let icons = declared(&[
            ("generate_image", "../../etc/passwd.png"),
            ("generate_video", "Bad Name"),
            ("speak", "icons/speak.gif"),
            ("*", ""),
        ]);
        assert_eq!(
            resolve_plugin_tool_icon("p", Some(&icons), "generate_image")
                .unwrap()
                .icon,
            "image",
            "逃逸路径被忽略，回落内置表"
        );
        assert_eq!(
            resolve_plugin_tool_icon("p", Some(&icons), "generate_video")
                .unwrap()
                .icon,
            "video"
        );
        assert_eq!(
            resolve_plugin_tool_icon("p", Some(&icons), "speak"),
            None,
            "扩展名不在白名单，且无内置项"
        );
    }

    #[test]
    fn 资源图标合法性() {
        assert!(is_valid_icon("icons/tool.svg"));
        assert!(is_valid_icon("tool.PNG"));
        assert!(is_valid_icon("square-terminal"));
        assert!(!is_valid_icon("/abs/tool.svg"));
        assert!(!is_valid_icon("../tool.svg"));
        assert!(!is_valid_icon("tool.exe"));
        assert!(!is_valid_icon("Tool"));
    }

    #[test]
    fn 查询表同时以原名与插件前缀名为键() {
        let volc = declared(&[("generate_image", "icons/seedream.svg")]);
        let table = build_tool_icon_table([
            PluginToolIcons {
                plugin_id: "generate-image-openai",
                declared: None,
                tools: vec!["generate_image".to_string()],
            },
            PluginToolIcons {
                plugin_id: "volcengine",
                declared: Some(&volc),
                tools: vec!["generate_image".to_string(), "generate_video".to_string()],
            },
        ]);
        assert_eq!(
            table["generate-image-openai__generate_image"].icon, "image",
            "未声明的插件回落内置表"
        );
        assert_eq!(
            table["volcengine__generate_image"].icon,
            "icons/seedream.svg"
        );
        assert_eq!(
            table["volcengine__generate_image"].plugin_id.as_deref(),
            Some("volcengine")
        );
        assert_eq!(table["generate_video"].icon, "video");
        assert_eq!(
            table["plugin_injection"].icon, "plug",
            "无插件归属的工具用内置表"
        );
        assert!(!table.contains_key("unknown_tool"));
    }

    #[test]
    fn 终端字形按查询表取_资源图标与未知工具用默认字形() {
        let volc = declared(&[("generate_image", "icons/seedream.svg")]);
        let table = build_tool_icon_table([
            PluginToolIcons {
                plugin_id: "terminal",
                declared: None,
                tools: vec!["run_shell".to_string()],
            },
            PluginToolIcons {
                plugin_id: "volcengine",
                declared: Some(&volc),
                tools: vec!["generate_image".to_string()],
            },
        ]);
        assert_eq!(terminal_glyph_for(&table, "terminal__run_shell"), "⚡");
        assert_eq!(
            terminal_glyph_for(&table, "other__run_shell"),
            "⚡",
            "按原名回落"
        );
        assert_eq!(
            terminal_glyph_for(&table, "volcengine__generate_image"),
            crate::builtin_tables::DEFAULT_TERMINAL_GLYPH,
            "资源图标无终端字形"
        );
        assert_eq!(
            terminal_glyph_for(&table, "unknown"),
            crate::builtin_tables::DEFAULT_TERMINAL_GLYPH
        );
    }

    #[test]
    fn 解析通用键与特指键() {
        assert_eq!(parse_tool_key("file_read"), Some(("file_read", None)));
        assert_eq!(parse_tool_key("*"), Some(("*", None)));
        assert_eq!(
            parse_tool_key("file_read:action=list"),
            Some(("file_read", Some(("action", "list"))))
        );
        assert_eq!(
            parse_tool_key("t:mode=a=b"),
            Some(("t", Some(("mode", "a=b")))),
            "取值可含 ="
        );
        for bad in [
            "file_read:action",
            "file_read:=list",
            "file_read:action=",
            ":action=list",
            "*:action=list",
            "file_read:a b=list",
        ] {
            assert_eq!(parse_tool_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn 特指图标按参数取值选用_未命中用通用图标() {
        let icons = declared(&[
            ("file_read", "assets/read.svg"),
            ("file_read:action=list", "assets/folder.svg"),
            ("file_read:action=tree", "assets/folder.svg"),
            ("file_read:action=image", "assets/image.svg"),
            ("file_write", "assets/write.svg"),
        ]);
        let icon = resolve_plugin_tool_icon("fs", Some(&icons), "file_read").unwrap();
        assert_eq!(icon.icon, "assets/read.svg");
        assert_eq!(icon.plugin_id.as_deref(), Some("fs"));
        assert_eq!(icon.variants["action"].len(), 3);
        assert_eq!(
            icon.icon_for_args(&serde_json::json!({"action": "tree"})),
            "assets/folder.svg"
        );
        assert_eq!(
            icon.icon_for_args(&serde_json::json!({"action": "image"})),
            "assets/image.svg"
        );
        assert_eq!(
            icon.icon_for_args(&serde_json::json!({"action": "read"})),
            "assets/read.svg",
            "无特指声明的取值用通用图标"
        );
        assert_eq!(
            icon.icon_for_args(&serde_json::Value::Null),
            "assets/read.svg",
            "参数未到时用通用图标"
        );
        let write = resolve_plugin_tool_icon("fs", Some(&icons), "file_write").unwrap();
        assert!(write.variants.is_empty(), "特指只作用于同名工具");
        let json = serde_json::to_value(&icon).unwrap();
        assert_eq!(json["variants"]["action"]["list"], "assets/folder.svg");
        assert!(
            serde_json::to_value(&write)
                .unwrap()
                .get("variants")
                .is_none(),
            "无特指图标时不输出 variants，保持旧前端兼容"
        );
    }

    #[test]
    fn 只有特指图标时通用图标回落插件默认与内置表() {
        let icons = declared(&[
            ("file_read:action=image", "assets/image.svg"),
            ("*", "flame"),
        ]);
        let icon = resolve_plugin_tool_icon("fs", Some(&icons), "file_read").unwrap();
        assert_eq!(icon.icon, "flame");
        assert_eq!(icon.variants["action"]["image"], "assets/image.svg");

        let icons = declared(&[("run_shell:mode=image", "assets/image.svg")]);
        let icon = resolve_plugin_tool_icon("terminal", Some(&icons), "run_shell").unwrap();
        assert_eq!(icon.icon, "square-terminal", "回落内置表");
        assert_eq!(
            icon.plugin_id.as_deref(),
            Some("terminal"),
            "特指资源按本插件读取"
        );

        let icons = declared(&[("speak:voice=a", "icons/a.svg")]);
        let icon = resolve_plugin_tool_icon("p", Some(&icons), "speak").unwrap();
        assert_eq!(icon.icon, "", "无通用图标时为空串，由前端用默认图标");
        assert_eq!(
            icon.icon_for_args(&serde_json::json!({"voice": "a"})),
            "icons/a.svg"
        );
    }

    #[test]
    fn 不合法的特指声明被忽略() {
        let icons = declared(&[
            ("file_read", "assets/read.svg"),
            ("file_read:action=list", "../folder.svg"),
            ("file_read:action", "assets/folder.svg"),
            ("file_read:action=image", "assets/image.svg"),
        ]);
        let icon = resolve_plugin_tool_icon("fs", Some(&icons), "file_read").unwrap();
        assert_eq!(icon.variants["action"].len(), 1);
        assert_eq!(icon.variants["action"]["image"], "assets/image.svg");
    }

    #[test]
    fn 只能读取插件声明过的资源图标() {
        let icons = declared(&[("speak", "icons/speak.svg"), ("*", "flame")]);
        assert!(declares_icon_resource(Some(&icons), "icons/speak.svg"));
        assert!(!declares_icon_resource(Some(&icons), "icons/other.svg"));
        assert!(!declares_icon_resource(Some(&icons), "flame"));
        assert!(!declares_icon_resource(None, "icons/speak.svg"));
        let icons = declared(&[("file_read:action=list", "assets/folder.svg")]);
        assert!(
            declares_icon_resource(Some(&icons), "assets/folder.svg"),
            "特指声明的资源可读取"
        );
    }
}
