//! 工具键解析（契约域）。
//!
//! 清单 `tool_icons` 的键与工具视图贡献的 `tools` 项共用同一套键语义：
//!
//! - 通用键 `工具名`；
//! - 特指键 `工具名:参数=取值`（如 `file_read:action=list`）；
//! - `*` 表示「本插件其余工具」（只对 `tool_icons` 有意义）。
//!
//! 放在契约层而不是展示层：`PluginManifest` 装载校验需要它，展示层
//! （`tool_icons` / `tool_titles`）也消费它——契约不能反向依赖展示。

/// `tool_icons` 中表示「本插件其余工具」的键。
pub const PLUGIN_DEFAULT_ICON_KEY: &str = "*";

/// 特指键中工具名与参数条件的分隔符（`工具名:参数=取值`）。
const ARG_KEY_SEPARATOR: char = ':';

/// 特指键中参数名的长度上限。
const MAX_ARG_NAME_LEN: usize = 64;

/// 键是否带参数条件（特指键 `工具名:参数=取值`）。
pub fn is_specific_key(key: &str) -> bool {
    key.contains(ARG_KEY_SEPARATOR)
}

/// 解析工具键：通用键返回 `(工具名, None)`，特指键返回
/// `(工具名, Some((参数名, 取值)))`；特指键格式不合法时返回 None。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 通用键与特指键解析() {
        assert_eq!(parse_tool_key("file_read"), Some(("file_read", None)));
        assert_eq!(
            parse_tool_key("file_read:action=list"),
            Some(("file_read", Some(("action", "list"))))
        );
    }

    #[test]
    fn 非法特指键拒绝() {
        assert_eq!(parse_tool_key("file_read:"), None);
        assert_eq!(parse_tool_key(":action=list"), None);
        assert_eq!(parse_tool_key("file_read:action="), None);
        assert_eq!(parse_tool_key("*:action=list"), None);
        assert_eq!(parse_tool_key("file_read:bad name=x"), None);
    }
}
