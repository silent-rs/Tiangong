//! CMD 查询卷信息会打开盘符根。本模块只开放根属性，不授予目录枚举或文件内容权限。
use super::*;
use windows_sys::Win32::Security::Authorization::{SET_ACCESS, SetSecurityInfo};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;

const MAXIMUM_ALLOWED: u32 = 0x0200_0000;

const CAPABILITY_NAME: &str = "TiangongSandbox.VolumeMetadata.v1";

pub(super) fn add_capability(set: &mut CapabilitySet) -> Result<PSID> {
    persistent::add_named_capability(set, CAPABILITY_NAME)
}

/// 需有卷根 WRITE_DAC 权限；普通用户不具备时由管理员显式运行此初始化命令。
/// 授权不继承、不开放目录枚举、文件读写，重复执行幂等。
pub fn prepare_volume_metadata(workspace: &Path) -> Result<PathBuf> {
    let root = workspace_volume_root(workspace)?;
    let mut capabilities = CapabilitySet::new(false)?;
    let sid = add_capability(&mut capabilities)?;
    set_root_attributes(&root, sid, SET_ACCESS)?;
    Ok(root)
}

/// 撤销本程序登记的卷基本信息访问，不影响其他身份及文件权限。
pub fn revoke_volume_metadata(workspace: &Path) -> Result<PathBuf> {
    let root = workspace_volume_root(workspace)?;
    let mut capabilities = CapabilitySet::new(false)?;
    let sid = add_capability(&mut capabilities)?;
    set_root_attributes(&root, sid, REVOKE_ACCESS)?;
    Ok(root)
}

fn workspace_volume_root(workspace: &Path) -> Result<PathBuf> {
    use std::path::{Component, Prefix};
    if !workspace.is_absolute() || !workspace.is_dir() {
        bail!("工作区必须是现有绝对目录");
    }
    // CMD 使用盘符根查询卷信息。GetVolumePathNameW 对 SUBST 路径可能返回整个工作区。
    match workspace.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Ok(PathBuf::from(format!("{}:\\", char::from(letter))))
            }
            _ => bail!("目前仅支持本地盘符根的基本信息授权"),
        },
        _ => bail!("目前仅支持本地盘符根的基本信息授权"),
    }
}

fn set_root_attributes(root: &Path, sid: PSID, mode: i32) -> Result<()> {
    let _lock = AclMutationLock::acquire()?;
    let path = wide_os(root.as_os_str());
    // MAXIMUM_ALLOWED 打开的句柄让 SetSecurityInfo 不传播目录继承项；避免遍历整卷。
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            MAXIMUM_ALLOWED,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("打开卷根属性授权句柄失败");
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut acl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        windows_sys::Win32::Security::Authorization::GetSecurityInfo(
            raw_handle(&handle),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(win32_error(status)).context("读取卷根授权失败");
    }
    let mut trustee = Default::default();
    unsafe {
        BuildTrusteeWithSidW(&mut trustee, sid);
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        grfAccessMode: mode,
        grfInheritance: NO_INHERITANCE,
        Trustee: trustee,
    };
    let mut updated = std::ptr::null_mut();
    let status = unsafe { SetEntriesInAclW(1, &entry, acl, &mut updated) };
    let result = if status == ERROR_SUCCESS {
        unsafe {
            SetSecurityInfo(
                raw_handle(&handle),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                updated,
                std::ptr::null(),
            )
        }
    } else {
        status
    };
    unsafe {
        if !updated.is_null() {
            LocalFree(updated.cast());
        }
        LocalFree(descriptor);
    }
    if result != ERROR_SUCCESS {
        return Err(win32_error(result))
            .context("初始化磁盘基本信息访问失败，请由管理员运行此命令");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DriveAlias(String);
    impl Drop for DriveAlias {
        fn drop(&mut self) {
            let mut command = std::process::Command::new("subst.exe");
            command.args([&self.0, "/d"]);
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
            let _ = command.status();
        }
    }

    #[test]
    fn cmd_volume_metadata_keeps_sibling_files_inaccessible() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let sibling = root.path().join("outside.txt");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(&sibling, "private").unwrap();
        std::fs::write(workspace.join("visible.txt"), "visible").unwrap();
        // CI 的系统目录未必向 AppContainer 开放执行；此测试只验证卷属性权限。
        // 将真实 CMD 放入明确授权的测试工作区，避免 PATH 或系统目录 ACL 干扰。
        let system_root = std::env::var_os("SystemRoot").expect("Windows 缺少 SystemRoot");
        std::fs::copy(
            PathBuf::from(system_root).join("System32/cmd.exe"),
            workspace.join("cmd.exe"),
        )
        .expect("复制测试 CMD 失败");
        let letter = ('P'..='Z')
            .rev()
            .find(|letter| !Path::new(&format!("{letter}:\\")).exists())
            .expect("没有空闲测试盘符");
        let alias = format!("{letter}:");
        use std::os::windows::process::CommandExt;
        let result = std::process::Command::new("subst.exe")
            .creation_flags(CREATE_NO_WINDOW)
            .arg(&alias)
            .arg(root.path())
            .status()
            .unwrap();
        assert!(result.success());
        let _alias_guard = DriveAlias(alias.clone());
        let alias_root = PathBuf::from(format!("{alias}\\"));
        let alias_workspace = alias_root.join("workspace");
        std::fs::write(
            workspace.join("volume-probe-root.txt"),
            alias_root.to_string_lossy().as_bytes(),
        )
        .unwrap();
        let mut caps = CapabilitySet::new(false).unwrap();
        let sid = add_capability(&mut caps).unwrap();
        set_root_attributes(root.path(), sid, SET_ACCESS).unwrap();
        // 确认重复初始化不扩大继承标记。
        set_root_attributes(root.path(), sid, SET_ACCESS).unwrap();
        let program = std::env::current_exe().unwrap();
        let policy = SandboxPolicy::workspace_write(alias_workspace);
        let code = launch(WindowsLaunchRequest {
            program: &program,
            program_root: program.parent().unwrap(),
            args: &[
                "--exact".into(),
                "sandbox::windows::volume_metadata::tests::volume_probe_child".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            policy: &policy,
            host_pid: None,
            stop_event_name: None,
            timeout: Some(Duration::from_secs(20)),
        })
        .unwrap();
        assert_eq!(code, 0);
        // 直接由 Launcher 启动真实 CMD。卷权限检查不依赖沙箱内再次 CreateProcess。
        let cmd = workspace.join("cmd.exe");
        let run_cmd = |script: String| {
            launch(WindowsLaunchRequest {
                program: &cmd,
                program_root: &workspace,
                args: &["/d".into(), "/c".into(), script],
                policy: &policy,
                host_pid: None,
                stop_event_name: None,
                timeout: Some(Duration::from_secs(20)),
            })
            .unwrap()
        };
        let script = format!(
            "cd /d {} && dir /b >listed.txt && echo success>created.txt && type created.txt >readback.txt && del created.txt && exit /b 7",
            alias_root.join("workspace").display(),
        );
        assert_eq!(run_cmd(script), 7, "CMD 应完成文件操作并保留指定退出码");
        assert!(
            std::fs::read_to_string(workspace.join("listed.txt"))
                .unwrap()
                .contains("visible.txt")
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("readback.txt"))
                .unwrap()
                .trim(),
            "success"
        );
        assert!(!workspace.join("created.txt").exists());
        set_root_attributes(root.path(), sid, REVOKE_ACCESS).unwrap();
        assert_eq!(
            run_cmd(format!(
                "cd /d {} && dir /b",
                alias_root.join("workspace").display()
            )),
            1,
            "撤销根属性后 CMD 不应继续获得卷信息",
        );
    }

    #[test]
    #[ignore = "由真实 AppContainer 父检查启动"]
    fn volume_probe_child() {
        let alias_root = PathBuf::from(std::fs::read_to_string("volume-probe-root.txt").unwrap());
        assert!(
            std::fs::read_dir(&alias_root).is_err(),
            "不应允许根目录枚举"
        );
        assert!(
            std::fs::read(alias_root.join("outside.txt")).is_err(),
            "不应允许读取相邻文件"
        );
        assert!(
            std::fs::write(alias_root.join("outside-new.txt"), "escape").is_err(),
            "不应允许写相邻文件"
        );
        assert!(
            std::fs::read_dir(".")
                .unwrap()
                .any(|entry| entry.unwrap().file_name() == "visible.txt")
        );
        std::fs::write("probe-write.txt", "workspace write").unwrap();
        assert_eq!(
            std::fs::read_to_string("probe-write.txt").unwrap(),
            "workspace write"
        );
        std::fs::remove_file("probe-write.txt").unwrap();
    }
}
