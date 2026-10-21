//! 平台适配工具：子进程「无窗口」配置。
//!
//! 与 `tiangong-toolkit` 的同名实现是经认可的双份副本（core 已不再需要
//! 该函数，由本 crate 自持一份供起子进程时复用）：Windows 上抑制控制台
//! 窗口弹出，其余平台空操作。

use std::process::Command;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 跨平台的「无窗口」命令配置：Windows 上抑制子进程控制台窗口弹出，其余平台空操作。
pub fn configure_no_window(command: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;

        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
