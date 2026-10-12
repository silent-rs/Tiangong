use anyhow::{Result, bail};

pub mod executor;
pub mod model;
pub mod store;

/// 把多行/空白折叠为单行，并要求结果非空。
///
/// 定时任务的「任务名称 / 任务描述」展示为单行字段，归一化避免卡片渲染跨行错位。
pub(crate) fn normalize_required_single_line(field: &str, value: &str) -> Result<String> {
    let normalized = value
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    if normalized.is_empty() {
        bail!("{field}不能为空");
    }

    Ok(normalized)
}

pub(crate) const fn default_true() -> bool {
    true
}
