//! `tiangong bot configure` 的终端交互原语。
//!
//! Bot 扫码授权需要在终端展示二维码，因此保留终端交互；其余配置统一走
//! `tiangong config` 网页配置页。
//!
//! 设计原则：
//! - 非 TTY 环境（脚本/CI/Docker）调用 `ensure_terminal()` 会报错退出，
//!   不卡住自动化流程。
//! - 所有原语返回 `anyhow::Result`，用户按 Ctrl+C/Esc 时 dialoguer 会返回
//!   `io::Error`（被 anyhow 转换），调用方可据此中断。

use std::io::IsTerminal;

use anyhow::{Result, anyhow};

/// 确认当前处于交互式终端，否则报错退出。
pub fn ensure_terminal() -> Result<()> {
    if std::io::stdin().is_terminal() {
        Ok(())
    } else {
        Err(anyhow!("当前非交互终端，请在 TTY 中运行。"))
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
