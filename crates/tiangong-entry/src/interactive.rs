//! 交互式配置向导的基础原语。
//!
//! 基于 `dialoguer` 封装 select/input/confirm/password 等提示，
//! 供 `tiangong model/server configure` 使用。
//!
//! 设计原则：
//! - 非 TTY 环境（脚本/CI/Docker）调用 `ensure_terminal()` 会报错退出，
//!   不卡住自动化流程。
//! - 所有原语返回 `anyhow::Result`，用户按 Ctrl+C/Esc 时 dialoguer 会返回
//!   `io::Error`（被 anyhow 转换），调用方可据此中断向导。

use std::io::IsTerminal;

use anyhow::{Result, anyhow};

/// 确认当前处于交互式终端，否则报错退出。
///
/// 向导命令开头调用，保证非 TTY（管道/重定向/CI）环境不会卡住。
pub fn ensure_terminal() -> Result<()> {
    if std::io::stdin().is_terminal() {
        Ok(())
    } else {
        Err(anyhow!(
            "当前非交互终端，无法启动配置向导。请使用参数式命令（如 `tiangong model add-provider --help`），或在 TTY 中运行。"
        ))
    }
}

/// 单选提示，返回选中项的索引。
pub fn select(prompt: &str, items: &[&str]) -> Result<usize> {
    let selection = dialoguer::Select::new()
        .with_prompt(prompt)
        .items(items)
        .default(0)
        .interact()?;
    Ok(selection)
}

/// 单选提示，指定默认选中索引。
pub fn select_with_default(prompt: &str, items: &[&str], default: usize) -> Result<usize> {
    let selection = dialoguer::Select::new()
        .with_prompt(prompt)
        .items(items)
        .default(default)
        .interact()?;
    Ok(selection)
}

/// 文本输入提示，带默认值（回车采用默认）。
pub fn input(prompt: &str, default: &str) -> Result<String> {
    let result = dialoguer::Input::<String>::new()
        .with_prompt(prompt)
        .default(default.to_string())
        .allow_empty(true)
        .interact_text()?;
    Ok(result)
}

/// 文本输入提示，无默认值，强制非空（trim 后）。
pub fn input_required(prompt: &str) -> Result<String> {
    let result = dialoguer::Input::<String>::new()
        .with_prompt(prompt)
        .validate_with(|input: &String| -> std::result::Result<(), String> {
            if input.trim().is_empty() {
                Err("不能为空".to_string())
            } else {
                Ok(())
            }
        })
        .interact_text()?;
    Ok(result.trim().to_string())
}

/// 多选提示，返回选中项的索引列表。
#[allow(dead_code)]
pub fn multiselect(prompt: &str, items: &[&str]) -> Result<Vec<usize>> {
    let selections = dialoguer::MultiSelect::new()
        .with_prompt(prompt)
        .items(items)
        .interact()?;
    Ok(selections)
}

/// 多选提示，可指定默认勾选项的索引列表。
pub fn multiselect_with_defaults(
    prompt: &str,
    items: &[&str],
    defaults: &[usize],
) -> Result<Vec<usize>> {
    // defaults() 需要长度与 items 相同的 bool 数组，每个 bool 表示对应项是否默认勾选。
    let default_flags: Vec<bool> = (0..items.len()).map(|i| defaults.contains(&i)).collect();

    let selections = dialoguer::MultiSelect::new()
        .with_prompt(prompt)
        .items(items)
        .defaults(&default_flags)
        .interact()?;
    Ok(selections)
}

/// 确认提示（Y/n），带默认值。
pub fn confirm(prompt: &str, default: bool) -> Result<bool> {
    let result = dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()?;
    Ok(result)
}

/// 密钥输入提示（不回显）。
pub fn password(prompt: &str) -> Result<String> {
    let result = dialoguer::Password::new().with_prompt(prompt).interact()?;
    Ok(result)
}
