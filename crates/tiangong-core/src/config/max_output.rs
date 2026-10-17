//! ReAct 主循环单次最大输出 token（含 thinking）。
//!
//! 数据由同目录 `max_output_tokens.json` 维护，编译时经 `include_str!` 内嵌。
//! 匹配规则：精确匹配 > 最长前缀匹配 > `_default`。

use std::collections::HashMap;
use std::sync::LazyLock;

static TABLE: LazyLock<HashMap<String, u32>> = LazyLock::new(|| {
    serde_json::from_str::<HashMap<String, serde_json::Value>>(include_str!(
        "max_output_tokens.json"
    ))
    .unwrap_or_default()
    .into_iter()
    .filter_map(|(key, value)| Some((key, u32::try_from(value.as_u64()?).ok()?)))
    .collect()
});

/// 按模型名解析单次最大输出 token。
pub(crate) fn max_output_tokens(model: &str) -> u32 {
    if let Some(value) = TABLE.get(model) {
        return *value;
    }
    TABLE
        .iter()
        .filter(|(key, _)| !key.starts_with('_') && model.starts_with(key.as_str()))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, value)| *value)
        .or_else(|| TABLE.get("_default").copied())
        .unwrap_or(128_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 按模型解析输出上限() {
        assert_eq!(max_output_tokens("claude-opus-5-5"), 128_000);
        assert_eq!(max_output_tokens("deepseek-v4-pro"), 384_000);
        assert_eq!(max_output_tokens("deepseek-flash"), 384_000);
        assert_eq!(max_output_tokens("unknown-model"), 128_000);
    }
}
