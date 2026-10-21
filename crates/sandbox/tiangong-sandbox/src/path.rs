use std::io;
use std::path::{Path, PathBuf};

/// 解析真实路径。Windows AppContainer 无法查询标准规范化名称，因此仅在
/// 已确认处于 AppContainer 时使用绝对路径展开，由文件 ACL 承担最终边界。
/// 普通进程仍必须完成真实路径解析。
pub fn canonicalize_path(path: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        let resolved = if windows::current_process_is_app_container().unwrap_or(false) {
            std::path::absolute(path)?
        } else {
            std::fs::canonicalize(path)?
        };
        Ok(dunce::simplified(&resolved).to_path_buf())
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenIsAppContainer};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::*;

    pub(super) fn current_process_is_app_container() -> io::Result<bool> {
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut is_app_container = 0u32;
        let mut returned = 0u32;
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenIsAppContainer,
                (&mut is_app_container as *mut u32).cast(),
                std::mem::size_of_val(&is_app_container) as u32,
                &mut returned,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(is_app_container != 0)
    }
}
