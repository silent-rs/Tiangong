//! 工具行标题：界面工具行折叠时展示的标题查询表。
//!
//! 来源：插件在 `plugin.json` 顶层 `tool_titles` 中声明（键为工具原名或
//! 特指键 `工具名:参数=取值`，值 1-32 字标题）。与 `session.message-item`
//! 视图贡献的 `titles` 同一套键语义：前端先查视图声明（随视图演进的插件
//! 用），查不到再查本表——无视图的 v2 插件也能声明工具行标题；都没有时
//! 前端显示通用标题「工具调用」。
//!
//! 声明不合法时只跳过该条并告警，不影响插件装载与运行（与 `tool_icons`
//! 一致）；键名对应的工具不在插件声明中时同样跳过。

use std::collections::BTreeMap;

use serde::Serialize;
use tiangong_core::tools::extension::namespaced_tool_name;

/// 标题字数上限（与视图贡献 `titles` 一致）。
pub const MAX_TOOL_TITLE_CHARS: usize = 32;

/// 查询表中的一项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolTitle {
    /// 通用标题。
    pub title: String,
    /// 特指标题：参数名 → 参数取值 → 标题。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub variants: BTreeMap<String, BTreeMap<String, String>>,
}
/// 参与建表的插件信息：插件 id、`tool_titles` 声明与插件声明的工具原名。
pub struct PluginToolTitles<'a> {
    pub plugin_id: &'a str,
    pub declared: Option<&'a BTreeMap<String, String>>,
    /// sidecar 动态工具的标题（来自 `tool_specs` 快照的 `title` 字段），
    /// 仅当清单 `tool_titles` 未覆盖该工具时参与建表。
    pub dynamic: std::collections::BTreeMap<String, String>,
    pub tools: Vec<String>,
}
/// 取插件声明的合法标题；空值或超长视为未声明并告警。
fn declared_title(
    plugin_id: &str,
    declared: Option<&BTreeMap<String, String>>,
    key: &str,
) -> Option<String> {
    let title = declared?.get(key)?.trim();
    if title.is_empty() || title.chars().count() > MAX_TOOL_TITLE_CHARS {
        tracing::warn!(plugin = %plugin_id, tool = %key, "插件声明的工具标题须为 1-{MAX_TOOL_TITLE_CHARS} 字，已忽略");
        return None;
    }
    Some(title.to_string())
}
/// 插件为该工具声明的合法特指标题：参数名 → 取值 → 标题。
fn declared_variants(
    plugin_id: &str,
    declared: Option<&BTreeMap<String, String>>,
    tool: &str,
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut variants: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for key in declared.into_iter().flat_map(BTreeMap::keys) {
        let Some((name, Some((arg, value)))) = crate::tool_icons::parse_tool_key(key) else {
            continue;
        };
        if name != tool {
            continue;
        }
        if let Some(title) = declared_title(plugin_id, declared, key) {
            variants
                .entry(arg.to_string())
                .or_default()
                .insert(value.to_string(), title);
        }
    }
    variants
}
/// 解析单个插件工具的标题；插件未声明通用标题时为 None（仅特指、无通用
/// 标题的工具由建表函数以空通用标题 + 特指条目收录）。
pub fn resolve_plugin_tool_title(
    plugin_id: &str,
    declared: Option<&BTreeMap<String, String>>,
    tool: &str,
) -> Option<ToolTitle> {
    let title = declared_title(plugin_id, declared, tool)?;
    Some(ToolTitle {
        title,
        variants: declared_variants(plugin_id, declared, tool),
    })
}
/// 建立工具标题查询表：键为工具对外名 `{插件id}__{工具名}` 与原名（旧会话
/// 历史）。无内置表；查不到的工具不在表中，由前端显示通用标题。
///
/// 优先取清单 `tool_titles` 声明；清单未覆盖时回落 sidecar 动态工具的
/// `title`（来自 `tool_specs` 快照，如 MCP 转发工具的 server 元数据）。
pub fn build_tool_title_table<'a>(
    plugins: impl IntoIterator<Item = PluginToolTitles<'a>>,
) -> BTreeMap<String, ToolTitle> {
    let mut table: BTreeMap<String, ToolTitle> = BTreeMap::new();
    for plugin in plugins {
        for tool in &plugin.tools {
            let declared_hit = resolve_plugin_tool_title(plugin.plugin_id, plugin.declared, tool);
            let title = match declared_hit {
                Some(title) => title,
                None => {
                    // 清单未声明通用标题：优先取动态标题，再检查特指条目。
                    if let Some(dynamic) = plugin.dynamic.get(tool.as_str()).filter(|t| {
                        !t.trim().is_empty() && t.chars().count() <= MAX_TOOL_TITLE_CHARS
                    }) {
                        ToolTitle {
                            title: dynamic.clone(),
                            variants: declared_variants(plugin.plugin_id, plugin.declared, tool),
                        }
                    } else {
                        let variants = declared_variants(plugin.plugin_id, plugin.declared, tool);
                        if variants.is_empty() {
                            continue;
                        }
                        ToolTitle {
                            title: String::new(),
                            variants,
                        }
                    }
                }
            };
            if let Some(exposed) = namespaced_tool_name(plugin.plugin_id, tool) {
                table.insert(exposed, title.clone());
            }
            table.insert(tool.clone(), title);
        }
    }
    table
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
    fn 通用与特指标题都进入条目() {
        let titles = declared(&[
            ("generate_image", "生成图片"),
            ("generate_image:mode=edit", "编辑图片"),
        ]);
        let entry = resolve_plugin_tool_title("p", Some(&titles), "generate_image").unwrap();
        assert_eq!(entry.title, "生成图片");
        assert_eq!(entry.variants["mode"]["edit"], "编辑图片");
    }
    #[test]
    fn 仅特指无通用时保留特指条目() {
        let titles = declared(&[("run_command:cwd=/tmp", "临时目录命令")]);
        assert_eq!(
            resolve_plugin_tool_title("p", Some(&titles), "run_command"),
            None
        );
        let table = build_tool_title_table([PluginToolTitles {
            plugin_id: "p",
            dynamic: Default::default(),
            declared: Some(&titles),
            tools: vec!["run_command".to_string()],
        }]);
        let entry = table.get("run_command").unwrap();
        assert_eq!(entry.title, "");
        assert_eq!(entry.variants["cwd"]["/tmp"], "临时目录命令");
    }
    #[test]
    fn 空标题与超长标题被忽略() {
        let long = "长".repeat(MAX_TOOL_TITLE_CHARS + 1);
        let titles = declared(&[("a", "  "), ("b", &long)]);
        assert_eq!(resolve_plugin_tool_title("p", Some(&titles), "a"), None);
        assert_eq!(resolve_plugin_tool_title("p", Some(&titles), "b"), None);
    }
    #[test]
    fn 未声明标题的工具不建条目() {
        let table = build_tool_title_table([PluginToolTitles {
            plugin_id: "p",
            dynamic: Default::default(),
            declared: None,
            tools: vec!["x".to_string(), "y".to_string()],
        }]);
        assert!(table.is_empty());
    }
    #[test]
    fn 建表键含对外名与原名() {
        let titles = declared(&[("x", "工具X")]);
        let table = build_tool_title_table([PluginToolTitles {
            plugin_id: "p",
            dynamic: Default::default(),
            declared: Some(&titles),
            tools: vec!["x".to_string()],
        }]);
        assert_eq!(table["x"].title, "工具X");
        assert_eq!(table["p__x"].title, "工具X");
    }
    #[test]
    fn 动态标题_清单未声明时回落_sidecar标题() {
        let mut dynamic = BTreeMap::new();
        dynamic.insert(
            "mcp__dbx__list_tables".to_string(),
            "dbx_list_tables · dbx".to_string(),
        );
        let table = build_tool_title_table([PluginToolTitles {
            plugin_id: "mcp",
            declared: None,
            dynamic,
            tools: vec!["mcp__dbx__list_tables".to_string()],
        }]);
        assert_eq!(
            table["mcp__dbx__list_tables"].title,
            "dbx_list_tables · dbx"
        );
    }
    #[test]
    fn 动态标题_清单声明优先于_sidecar() {
        let declared = declared(&[("mcp__dbx__list_tables", "查库表")]);
        let mut dynamic = BTreeMap::new();
        dynamic.insert(
            "mcp__dbx__list_tables".to_string(),
            "dbx_list_tables · dbx".to_string(),
        );
        let table = build_tool_title_table([PluginToolTitles {
            plugin_id: "mcp",
            declared: Some(&declared),
            dynamic,
            tools: vec!["mcp__dbx__list_tables".to_string()],
        }]);
        assert_eq!(table["mcp__dbx__list_tables"].title, "查库表");
    }
}
