//! Windows AppContainer 沙箱。
//!
//! Launcher 为每次调用创建独立 AppContainer 身份，只向目标程序、工作区与
//! 专用临时目录授予该身份所需的最小 ACL。目标以挂起状态创建，加入带资源
//! 配额的 Job Object 后才恢复运行；网络能力只在策略明确允许时加入。

#![cfg(windows)]

use std::collections::HashSet;
use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_HANDLE_EOF, ERROR_INSUFFICIENT_BUFFER,
    ERROR_MORE_DATA, ERROR_SUCCESS, GENERIC_ALL, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    LocalFree, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    BuildTrusteeWithSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
    REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    AllocateAndInitializeSid, CONTAINER_INHERIT_ACE, CreateRestrictedToken, CreateWellKnownSid,
    DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE, FreeSid, GetTokenInformation, NO_INHERITANCE,
    OBJECT_INHERIT_ACE, PSID, SECURITY_CAPABILITIES, SECURITY_MAX_SID_SIZE,
    SECURITY_RESOURCE_MANAGER_AUTHORITY, SID_AND_ATTRIBUTES, SetTokenInformation,
    TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS,
    TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl, TokenGroups, TokenUser, WinAuthenticatedUserSid,
    WinBuiltinUsersSid, WinCapabilityInternetClientSid, WinCapabilityPrivateNetworkClientServerSid,
    WinRestrictedCodeSid, WinWorldSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, FindClose, FindFirstFileNameW,
    FindNextFileNameW, GetFileInformationByHandle, GetVolumePathNameW, OPEN_EXISTING, WRITE_DAC,
    WRITE_OWNER,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateMutexW,
    CreateProcessAsUserW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, GetProcessMitigationPolicy, INFINITE,
    InitializeProcThreadAttributeList, OpenEventW, OpenProcess, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, PROCESS_SYNCHRONIZE,
    ProcessChildProcessPolicy, ReleaseMutex, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    SYNCHRONIZATION_SYNCHRONIZE, TerminateProcess, UpdateProcThreadAttribute,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::PROCESS_CREATION_CHILD_PROCESS_OVERRIDE;

use super::{SandboxAvailability, SandboxMode, SandboxPolicy, SandboxResourceLimits};

mod persistent;
mod volume_metadata;
pub use persistent::revoke as revoke_persistent_grants;
pub use volume_metadata::{prepare_volume_metadata, revoke_volume_metadata};

const SE_GROUP_ENABLED: u32 = 4;
const SE_GROUP_LOGON_ID: u32 = 0xc000_0000;
const ACL_MUTATION_MUTEX: &str = "Local\\TiangongSandboxAclMutation-v1";
const FILE_WRITE_ACCESS: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | FILE_DELETE_CHILD
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER;
const FILE_WORKSPACE_ACCESS: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;
const FILE_PROGRAM_ACCESS: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// Windows Launcher 的原生启动参数。
pub struct WindowsLaunchRequest<'a> {
    pub program: &'a Path,
    pub program_root: &'a Path,
    pub args: &'a [String],
    pub policy: &'a SandboxPolicy,
    pub host_pid: Option<u32>,
    pub stop_event_name: Option<&'a str>,
    pub timeout: Option<Duration>,
}

pub fn availability() -> SandboxAvailability {
    let name = wide("TiangongSandbox.AvailabilityProbe");
    let mut sid = std::ptr::null_mut();
    let result = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
    if result < 0 {
        return SandboxAvailability::Unsupported(format!(
            "系统无法创建 AppContainer 身份（HRESULT=0x{:08x}）",
            result as u32
        ));
    }
    unsafe {
        FreeSid(sid);
    }
    SandboxAvailability::Available
}

/// 创建、运行并清理一次 Windows AppContainer 调用。
pub fn launch(request: WindowsLaunchRequest<'_>) -> Result<i32> {
    launch_with_grant_cache(request, None)
}

pub fn launch_with_grant_cache(
    request: WindowsLaunchRequest<'_>,
    cache: Option<&Path>,
) -> Result<i32> {
    trace_self_check(&request, "校验启动请求");
    validate_launch_request(&request)?;
    trace_self_check(&request, "创建网络能力集合");
    let mut capabilities = CapabilitySet::new(request.policy.allow_network)?;
    volume_metadata::add_capability(&mut capabilities)?;
    if cache.is_some() {
        trace_self_check(&request, "检查持久目录授权");
    }
    let lease = cache
        .map(|path| persistent::prepare(&request, path, &mut capabilities))
        .transpose()?;
    trace_self_check(&request, "创建临时 AppContainer 身份");
    let mut profile = AppContainerProfile::create(&capabilities)?;
    trace_self_check(&request, "创建临时受限身份");
    let temporary_restriction = if lease.is_none() {
        Some(RestrictionSid::new()?)
    } else {
        None
    };
    let restriction = lease
        .as_ref()
        .map(|lease| &lease.restriction)
        .or(temporary_restriction.as_ref())
        .expect("已创建受限身份");
    trace_self_check(&request, "应用临时目录授权");
    let mut grants = if lease.is_none() {
        Some(AclGrants::apply(
            profile.sid,
            restriction.sid,
            request.program,
            request.policy,
        )?)
    } else {
        None
    };

    trace_self_check(&request, "启动受限目标进程");
    let execution = run_restricted_process(&request, &profile, restriction, &capabilities);
    trace_self_check(&request, "受限目标进程已结束");
    let acl_cleanup = grants.as_mut().map_or(Ok(()), AclGrants::revoke);
    trace_self_check(
        &request,
        if lease.is_some() {
            "持久文件授权已保留"
        } else {
            "临时目录授权已撤销"
        },
    );
    let profile_cleanup = profile.delete();
    trace_self_check(&request, "临时 AppContainer 身份已删除");

    match (execution, acl_cleanup, profile_cleanup) {
        (Ok(code), Ok(()), Ok(())) => Ok(code),
        (Err(error), Ok(()), Ok(())) => Err(error),
        (execution, acl_cleanup, profile_cleanup) => {
            let mut messages = Vec::new();
            if let Err(error) = execution {
                messages.push(format!("目标执行失败: {error:#}"));
            }
            if let Err(error) = acl_cleanup {
                messages.push(format!("撤销临时目录授权失败: {error:#}"));
            }
            if let Err(error) = profile_cleanup {
                messages.push(format!("删除临时 AppContainer 身份失败: {error:#}"));
            }
            bail!(messages.join("; "))
        }
    }
}

fn trace_self_check(request: &WindowsLaunchRequest<'_>, stage: &str) {
    if request.timeout.is_some() || std::env::var_os("TIANGONG_SANDBOX_DIAGNOSTICS").is_some() {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let elapsed = START.get_or_init(Instant::now).elapsed().as_millis();
        eprintln!("Windows AppContainer 阶段 elapsed_ms={elapsed}: {stage}");
    }
}

/// 由受限目标自检自身所在 Job 的资源上限。
pub fn current_process_limits_match(expected: SandboxResourceLimits) -> bool {
    let mut actual = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let queried = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut actual).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    queried != 0
        && actual.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS != 0
        && actual.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0
        && actual.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_TIME != 0
        && actual.BasicLimitInformation.PerJobUserTimeLimit
            == expected.max_cpu_time_seconds as i64 * 10_000_000
        && actual.BasicLimitInformation.ActiveProcessLimit == expected.max_processes
        && actual.JobMemoryLimit == expected.max_memory_bytes as usize
}

/// 由受限目标读取系统实际应用的子进程策略标志。
pub fn current_process_child_policy_flags() -> Result<u32> {
    let mut flags = 0u32;
    if unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            ProcessChildProcessPolicy,
            (&raw mut flags).cast(),
            size_of::<u32>(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("读取 Windows 子进程策略失败");
    }
    Ok(flags)
}

fn validate_launch_request(request: &WindowsLaunchRequest<'_>) -> Result<()> {
    if request.policy.mode == SandboxMode::FullAccess {
        bail!("Windows Launcher 不接受 full_access 策略");
    }
    if !request.program.is_absolute() || !request.program_root.is_absolute() {
        bail!("Windows 目标程序及根目录必须是绝对路径");
    }
    if !request.program.is_file() || !request.program_root.is_dir() {
        bail!("Windows 目标程序或根目录不存在");
    }
    let writable_roots = request
        .policy
        .writable_roots()
        .into_iter()
        .map(|root| {
            crate::canonicalize_path(&root)
                .with_context(|| format!("解析 Windows 可写目录失败: {}", root.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    for root in &writable_roots {
        if writable_roots
            .iter()
            .any(|parent| parent != root && root.starts_with(parent))
        {
            continue;
        }
        validate_writable_tree(root, &writable_roots)
            .with_context(|| format!("Windows 可写目录不安全: {}", root.display()))?;
    }
    Ok(())
}

fn validate_writable_tree(root: &Path, writable_roots: &[PathBuf]) -> Result<()> {
    if !root.is_absolute() || !root.is_dir() {
        bail!("可写根必须是已存在的绝对目录");
    }
    let metadata = std::fs::symlink_metadata(root)
        .with_context(|| format!("读取路径元数据失败: {}", root.display()))?;
    let mut pending = vec![(root.to_path_buf(), metadata)];
    while let Some((path, metadata)) = pending.pop() {
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!("可写目录包含重解析点，拒绝授予权限: {}", path.display());
        }
        if metadata.is_file() {
            let link_count = match file_link_count(&path) {
                Ok(count) => count,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if link_count > 1 {
                validate_hardlink_aliases(&path, link_count, writable_roots)?;
            }
        }
        if metadata.is_dir() {
            // 子目录可能在枚举后被替换；遍历前重新检查，不能跟随新出现的链接。
            let current = match std::fs::symlink_metadata(&path) {
                Ok(current) => current,
                Err(error) if path != root && error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => return Err(error).context("复核可写目录失败"),
            };
            if current.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                bail!("可写目录包含重解析点，拒绝授予权限: {}", path.display());
            }
            for entry in std::fs::read_dir(&path)
                .with_context(|| format!("扫描可写目录失败: {}", path.display()))?
            {
                let entry = entry?;
                // Windows 的目录枚举已携带元数据；复用它，不再按路径重查每个文件。
                match entry.metadata() {
                    Ok(metadata) => pending.push((entry.path(), metadata)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("读取目录项元数据失败"),
                }
            }
        }
    }
    Ok(())
}

fn file_link_count(path: &Path) -> Result<u32> {
    let path = wide_os(path.as_os_str());
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("打开文件以检查硬链接失败");
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(raw_handle(&handle), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error()).context("读取文件硬链接数量失败");
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("硬链接检查遇到重解析点，拒绝授予权限");
    }
    Ok(info.nNumberOfLinks)
}

fn validate_hardlink_aliases(
    path: &Path,
    expected_count: u32,
    writable_roots: &[PathBuf],
) -> Result<()> {
    // 硬链接本身不是逃逸；只有无法完整枚举或任一名称越过授权根时才拒绝。
    let aliases = hardlink_aliases(path)?;
    if aliases.len() != expected_count as usize {
        bail!(
            "硬链接名称数量与文件元数据不一致: {}（元数据={}, 枚举={}）",
            path.display(),
            expected_count,
            aliases.len()
        );
    }
    for alias in aliases {
        let resolved = crate::canonicalize_path(&alias)
            .with_context(|| format!("解析硬链接名称失败: {}", alias.display()))?;
        if !writable_roots.iter().any(|root| resolved.starts_with(root)) {
            bail!(
                "硬链接名称超出授权可写目录: {} -> {}",
                path.display(),
                resolved.display()
            );
        }
    }
    let final_count = file_link_count(path)?;
    if final_count != expected_count {
        bail!(
            "硬链接数量在校验期间发生变化: {}（校验前={}, 校验后={}）",
            path.display(),
            expected_count,
            final_count
        );
    }
    Ok(())
}

fn hardlink_aliases(path: &Path) -> Result<HashSet<PathBuf>> {
    // FindFirstFileNameW 返回卷内相对名称，先补回卷根再做真实路径校验。
    let volume_root = volume_root(path)?;
    let path_wide = wide_os(path.as_os_str());
    let mut buffer = vec![0u16; 512];
    let search = loop {
        let mut length = buffer.len() as u32;
        let handle =
            unsafe { FindFirstFileNameW(path_wide.as_ptr(), 0, &mut length, buffer.as_mut_ptr()) };
        if handle != INVALID_HANDLE_VALUE {
            break FileNameSearch(handle);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
            return Err(error).context("枚举硬链接名称失败");
        }
        resize_name_buffer(&mut buffer, length)?;
    };

    let mut aliases = HashSet::new();
    aliases.insert(hardlink_name_path(&volume_root, &buffer)?);
    loop {
        buffer.fill(0);
        let mut length = buffer.len() as u32;
        if unsafe { FindNextFileNameW(search.0, &mut length, buffer.as_mut_ptr()) } != 0 {
            aliases.insert(hardlink_name_path(&volume_root, &buffer)?);
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == ERROR_HANDLE_EOF as i32 => break,
            Some(code) if code == ERROR_MORE_DATA as i32 => {
                resize_name_buffer(&mut buffer, length)?;
            }
            _ => return Err(error).context("继续枚举硬链接名称失败"),
        }
    }
    search.close().context("关闭硬链接名称枚举失败")?;
    Ok(aliases)
}

fn volume_root(path: &Path) -> Result<PathBuf> {
    let path = wide_os(path.as_os_str());
    let mut buffer = vec![0u16; 32_768];
    if unsafe { GetVolumePathNameW(path.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) } == 0 {
        return Err(std::io::Error::last_os_error()).context("读取硬链接所在卷根目录失败");
    }
    os_string_from_wide_buffer(&buffer)
        .map(PathBuf::from)
        .context("硬链接所在卷根目录为空")
}

fn hardlink_name_path(volume_root: &Path, buffer: &[u16]) -> Result<PathBuf> {
    let name = os_string_from_wide_buffer(buffer).context("枚举到空的硬链接名称")?;
    let mut relative = PathBuf::new();
    for component in Path::new(&name).components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(value) => relative.push(value),
            _ => bail!("枚举到非法硬链接名称: {}", Path::new(&name).display()),
        }
    }
    if relative.as_os_str().is_empty() {
        bail!("枚举到空的硬链接相对路径");
    }
    Ok(volume_root.join(relative))
}

fn os_string_from_wide_buffer(buffer: &[u16]) -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt;

    let length = buffer.iter().position(|unit| *unit == 0)?;
    (length > 0).then(|| OsString::from_wide(&buffer[..length]))
}

fn resize_name_buffer(buffer: &mut Vec<u16>, required: u32) -> Result<()> {
    let required = usize::try_from(required).context("硬链接名称缓冲区长度溢出")?;
    let next = required.max(buffer.len().saturating_mul(2));
    if next <= buffer.len() {
        bail!("硬链接名称缓冲区无法继续扩展");
    }
    buffer.resize(next, 0);
    Ok(())
}

struct FileNameSearch(HANDLE);

impl FileNameSearch {
    fn close(mut self) -> std::io::Result<()> {
        let handle = std::mem::replace(&mut self.0, INVALID_HANDLE_VALUE);
        if unsafe { FindClose(handle) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for FileNameSearch {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                FindClose(self.0);
            }
        }
    }
}

struct CapabilitySet {
    _sid_storage: Vec<Box<[u8]>>,
    entries: Vec<SID_AND_ATTRIBUTES>,
}

impl CapabilitySet {
    fn new(allow_network: bool) -> Result<Self> {
        let mut sid_storage = Vec::new();
        if allow_network {
            for kind in [
                WinCapabilityInternetClientSid,
                WinCapabilityPrivateNetworkClientServerSid,
            ] {
                let mut storage = vec![0u8; SECURITY_MAX_SID_SIZE as usize].into_boxed_slice();
                let mut size = storage.len() as u32;
                if unsafe {
                    CreateWellKnownSid(
                        kind,
                        std::ptr::null_mut(),
                        storage.as_mut_ptr().cast(),
                        &mut size,
                    )
                } == 0
                {
                    return Err(std::io::Error::last_os_error())
                        .context("创建 AppContainer 网络能力 SID 失败");
                }
                sid_storage.push(storage);
            }
        }
        let entries = sid_storage
            .iter_mut()
            .map(|storage| SID_AND_ATTRIBUTES {
                Sid: storage.as_mut_ptr().cast(),
                Attributes: SE_GROUP_ENABLED,
            })
            .collect();
        Ok(Self {
            _sid_storage: sid_storage,
            entries,
        })
    }

    fn as_ptr(&self) -> *const SID_AND_ATTRIBUTES {
        if self.entries.is_empty() {
            std::ptr::null()
        } else {
            self.entries.as_ptr()
        }
    }
}

struct RestrictionSid {
    sid: PSID,
}

impl RestrictionSid {
    fn new() -> Result<Self> {
        Self::from_value(scru128::new().to_u128())
    }

    fn from_value(value: u128) -> Result<Self> {
        let mut sid = std::ptr::null_mut();
        let created = unsafe {
            AllocateAndInitializeSid(
                &SECURITY_RESOURCE_MANAGER_AUTHORITY,
                4,
                (value >> 96) as u32,
                (value >> 64) as u32,
                (value >> 32) as u32,
                value as u32,
                0,
                0,
                0,
                0,
                &mut sid,
            )
        };
        if created == 0 {
            return Err(std::io::Error::last_os_error()).context("创建 Windows 临时受限 SID 失败");
        }
        Ok(Self { sid })
    }
}

impl Drop for RestrictionSid {
    fn drop(&mut self) {
        if !self.sid.is_null() {
            unsafe {
                FreeSid(self.sid);
            }
        }
    }
}

struct RestrictedToken(OwnedHandle);

impl RestrictedToken {
    fn new(restriction_sid: PSID, appcontainer_sid: PSID) -> Result<Self> {
        let mut source = std::ptr::null_mut();
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY,
                &mut source,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("打开 Windows Launcher 主令牌失败");
        }
        let source = unsafe { OwnedHandle::from_raw_handle(source) };

        let user_storage = token_information(raw_handle(&source), TokenUser, "用户")?;
        let user = unsafe { &*user_storage.as_ptr().cast::<TOKEN_USER>() };
        let group_storage = token_information(raw_handle(&source), TokenGroups, "用户组")?;
        let groups = unsafe { &*group_storage.as_ptr().cast::<TOKEN_GROUPS>() };
        let group_entries = unsafe {
            std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize)
        };

        let mut sid_storage = Vec::new();
        for kind in [
            WinWorldSid,
            WinBuiltinUsersSid,
            WinAuthenticatedUserSid,
            WinRestrictedCodeSid,
        ] {
            let mut storage = vec![0u8; SECURITY_MAX_SID_SIZE as usize].into_boxed_slice();
            let mut size = storage.len() as u32;
            if unsafe {
                CreateWellKnownSid(
                    kind,
                    std::ptr::null_mut(),
                    storage.as_mut_ptr().cast(),
                    &mut size,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("创建 Windows 受限令牌兼容 SID 失败");
            }
            sid_storage.push(storage);
        }

        let mut restrictions = vec![
            SID_AND_ATTRIBUTES {
                Sid: restriction_sid,
                Attributes: 0,
            },
            SID_AND_ATTRIBUTES {
                Sid: user.User.Sid,
                Attributes: 0,
            },
        ];
        if let Some(logon) = group_entries
            .iter()
            .find(|group| group.Attributes & SE_GROUP_LOGON_ID != 0)
        {
            restrictions.push(SID_AND_ATTRIBUTES {
                Sid: logon.Sid,
                Attributes: 0,
            });
        }
        restrictions.extend(sid_storage.iter_mut().map(|storage| SID_AND_ATTRIBUTES {
            Sid: storage.as_mut_ptr().cast(),
            Attributes: 0,
        }));

        let mut token = std::ptr::null_mut();
        if unsafe {
            CreateRestrictedToken(
                raw_handle(&source),
                DISABLE_MAX_PRIVILEGE,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                restrictions.len() as u32,
                restrictions.as_ptr(),
                &mut token,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("创建 Windows 受限主令牌失败");
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        // 子进程对象沿用令牌默认 DACL；缺少这两个身份时 AppContainer 内的
        // CreateProcess 会在文件权限已放行的情况下仍返回拒绝访问。
        grant_token_default_access(raw_handle(&token), &[appcontainer_sid, restriction_sid])?;
        Ok(Self(token))
    }

    fn raw(&self) -> HANDLE {
        raw_handle(&self.0)
    }
}

fn grant_token_default_access(token: HANDLE, sids: &[PSID]) -> Result<()> {
    let storage = token_information(token, TokenDefaultDacl, "默认 DACL")?;
    let current = unsafe { &*storage.as_ptr().cast::<TOKEN_DEFAULT_DACL>() };
    let mut entries = Vec::with_capacity(sids.len());
    for sid in sids {
        let mut trustee = Default::default();
        unsafe {
            BuildTrusteeWithSidW(&mut trustee, *sid);
        }
        entries.push(EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: trustee,
        });
    }

    let mut updated_acl = std::ptr::null_mut();
    let acl_status = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            current.DefaultDacl,
            &mut updated_acl,
        )
    };
    if acl_status != ERROR_SUCCESS {
        return Err(win32_error(acl_status)).context("构造 Windows 受限令牌默认 ACL 失败");
    }

    let default_dacl = TOKEN_DEFAULT_DACL {
        DefaultDacl: updated_acl,
    };
    let configured = unsafe {
        SetTokenInformation(
            token,
            TokenDefaultDacl,
            (&raw const default_dacl).cast(),
            size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    };
    unsafe {
        LocalFree(updated_acl.cast());
    }
    if configured == 0 {
        return Err(std::io::Error::last_os_error()).context("配置 Windows 受限令牌默认 ACL 失败");
    }
    Ok(())
}

fn token_information(token: HANDLE, class: i32, label: &str) -> Result<Vec<usize>> {
    let mut bytes = 0u32;
    unsafe {
        GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut bytes);
    }
    if bytes == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("读取 Windows Launcher {label}信息长度失败"));
    }
    let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    if unsafe { GetTokenInformation(token, class, storage.as_mut_ptr().cast(), bytes, &mut bytes) }
        == 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("读取 Windows Launcher {label}信息失败"));
    }
    Ok(storage)
}

struct AppContainerProfile {
    name: Vec<u16>,
    sid: PSID,
    active: bool,
}

impl AppContainerProfile {
    fn create(capabilities: &CapabilitySet) -> Result<Self> {
        let name = wide(&format!("TiangongSandbox.{}", scru128::new()));
        let display = wide("Tiangong Sandbox");
        let description = wide("Temporary Tiangong sandbox identity");
        let mut sid = std::ptr::null_mut();
        let result = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                display.as_ptr(),
                description.as_ptr(),
                capabilities.as_ptr(),
                capabilities.entries.len() as u32,
                &mut sid,
            )
        };
        if result < 0 {
            bail!(
                "创建临时 AppContainer 身份失败（HRESULT=0x{:08x}）",
                result as u32
            );
        }
        Ok(Self {
            name,
            sid,
            active: true,
        })
    }

    fn delete(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        for attempt in 0..10 {
            let result = unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
            if result >= 0 {
                self.active = false;
                return Ok(());
            }
            if attempt == 9 {
                bail!(
                    "删除 AppContainer Profile 失败（HRESULT=0x{:08x}）",
                    result as u32
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unreachable!()
    }
}

impl Drop for AppContainerProfile {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                DeleteAppContainerProfile(self.name.as_ptr());
            }
        }
        if !self.sid.is_null() {
            unsafe {
                FreeSid(self.sid);
            }
        }
    }
}

struct AclRoot {
    sids: Vec<PSID>,
    path: PathBuf,
}

struct AclGrants {
    roots: Vec<AclRoot>,
    active: bool,
}

impl AclGrants {
    fn apply(
        appcontainer_sid: PSID,
        restriction_sid: PSID,
        program: &Path,
        policy: &SandboxPolicy,
    ) -> Result<Self> {
        let mut grants = Self {
            roots: Vec::new(),
            active: true,
        };
        let result = (|| {
            // AppContainer 保留目录穿越能力；修改祖先 DACL 会让 Windows 向整棵
            // 子树传播继承项，因此只授权最终程序和策略根。
            grants.add(
                &[appcontainer_sid, restriction_sid],
                program,
                FILE_PROGRAM_ACCESS,
                NO_INHERITANCE,
            )?;
            let writable = policy.writable_roots();
            for root in &writable {
                grants.add(
                    &[appcontainer_sid, restriction_sid],
                    root,
                    FILE_WORKSPACE_ACCESS,
                    OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                )?;
            }
            for path in policy.read_only_roots() {
                if path.exists() {
                    grants.deny(
                        restriction_sid,
                        &path,
                        FILE_WRITE_ACCESS,
                        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                    )?;
                }
            }
            for path in policy.denied_read_roots() {
                if path.exists() && writable.iter().any(|root| path.starts_with(root)) {
                    grants.deny(
                        restriction_sid,
                        &path,
                        FILE_ALL_ACCESS,
                        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                    )?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            let cleanup = grants.revoke();
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow!("{error:#}; 回滚临时 ACL 失败: {cleanup:#}")),
            };
        }
        Ok(grants)
    }

    fn add(
        &mut self,
        sids: &[PSID],
        path: &Path,
        permissions: u32,
        inheritance: u32,
    ) -> Result<()> {
        modify_acl(path, sids, permissions, inheritance, GRANT_ACCESS)?;
        self.roots.push(AclRoot {
            sids: sids.to_vec(),
            path: path.to_path_buf(),
        });
        Ok(())
    }

    fn deny(&mut self, sid: PSID, path: &Path, permissions: u32, inheritance: u32) -> Result<()> {
        modify_acl(path, &[sid], permissions, inheritance, DENY_ACCESS)?;
        self.roots.push(AclRoot {
            sids: vec![sid],
            path: path.to_path_buf(),
        });
        Ok(())
    }

    fn revoke(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        // 每项 ACL 都写在授权根；只从相同根撤销显式项，让 Windows 传播
        // 继承变化。逐文件撤销会触碰根内不属于天工管理的受保护文件。
        let mut entries = self
            .roots
            .iter()
            .map(|root| (root.path.clone(), root.sids.clone()))
            .collect::<Vec<_>>();
        entries.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        let mut seen = HashSet::new();
        let mut failures = Vec::new();
        for (path, mut sids) in entries {
            sids.retain(|sid| seen.insert((path.clone(), *sid as usize)));
            if sids.is_empty() || !path.exists() {
                continue;
            }
            if let Err(error) = modify_acl(&path, &sids, 0, NO_INHERITANCE, REVOKE_ACCESS) {
                failures.push(format!("{}: {error:#}", path.display()));
            }
        }
        if failures.is_empty() {
            self.active = false;
            Ok(())
        } else {
            bail!(failures.join("; "))
        }
    }
}

impl Drop for AclGrants {
    fn drop(&mut self) {
        if self.active {
            let _ = self.revoke();
        }
    }
}

fn modify_acl(
    path: &Path,
    sids: &[PSID],
    permissions: u32,
    inheritance: u32,
    mode: i32,
) -> Result<()> {
    let _mutation_lock = AclMutationLock::acquire()?;
    let path_wide = wide_os(path.as_os_str());
    let mut old_acl = std::ptr::null_mut();
    let mut security_descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut old_acl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(win32_error(status)).context("读取 ACL 失败");
    }

    // 一次更新同一路径上的全部身份，避免每个身份都触发整棵目录继承传播。
    let access = sids
        .iter()
        .map(|sid| {
            let mut trustee = Default::default();
            unsafe {
                BuildTrusteeWithSidW(&mut trustee, *sid);
            }
            EXPLICIT_ACCESS_W {
                grfAccessPermissions: permissions,
                grfAccessMode: mode,
                grfInheritance: inheritance,
                Trustee: trustee,
            }
        })
        .collect::<Vec<_>>();
    let mut new_acl = std::ptr::null_mut();
    let acl_status =
        unsafe { SetEntriesInAclW(access.len() as u32, access.as_ptr(), old_acl, &mut new_acl) };
    if acl_status != ERROR_SUCCESS {
        unsafe {
            LocalFree(security_descriptor);
        }
        return Err(win32_error(acl_status)).context("构造 ACL 失败");
    }
    let set_status = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_acl,
            std::ptr::null(),
        )
    };
    unsafe {
        LocalFree(new_acl.cast());
        LocalFree(security_descriptor);
    }
    if set_status != ERROR_SUCCESS {
        return Err(win32_error(set_status)).context("写入 ACL 失败");
    }
    Ok(())
}

fn win32_error(status: u32) -> std::io::Error {
    std::io::Error::from_raw_os_error(i32::from_ne_bytes(status.to_ne_bytes()))
}

struct AclMutationLock(OwnedHandle);

impl AclMutationLock {
    fn acquire() -> Result<Self> {
        let name = wide(ACL_MUTATION_MUTEX);
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("创建 Windows ACL 更新互斥锁失败");
        }
        let lock = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        match unsafe { WaitForSingleObject(raw_handle(&lock.0), 30_000) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(lock),
            WAIT_TIMEOUT => bail!("等待 Windows ACL 更新互斥锁超时"),
            WAIT_FAILED => {
                Err(std::io::Error::last_os_error()).context("等待 Windows ACL 更新互斥锁失败")
            }
            status => bail!("等待 Windows ACL 更新互斥锁返回异常状态: {status}"),
        }
    }
}

impl Drop for AclMutationLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(raw_handle(&self.0));
        }
    }
}

fn run_restricted_process(
    request: &WindowsLaunchRequest<'_>,
    profile: &AppContainerProfile,
    restriction: &RestrictionSid,
    capabilities: &CapabilitySet,
) -> Result<i32> {
    trace_self_check(request, "创建受限 Job");
    let job = WindowsJob::new(request.policy.resource_limits)?;
    let host = request.host_pid.map(open_host_process).transpose()?;
    let stop_event = request.stop_event_name.map(open_stop_event).transpose()?;

    let security_capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: profile.sid,
        Capabilities: capabilities.as_ptr().cast_mut(),
        CapabilityCount: capabilities.entries.len() as u32,
        Reserved: 0,
    };
    trace_self_check(request, "准备标准输入输出句柄");
    let stdio = InheritableStdio::prepare()?;
    let child_process_policy = PROCESS_CREATION_CHILD_PROCESS_OVERRIDE;
    let mut attributes = ProcessAttributes::new(
        &security_capabilities,
        &stdio.handles,
        &child_process_policy,
    )?;
    trace_self_check(request, "创建受限进程令牌");
    let token = RestrictedToken::new(restriction.sid, profile.sid)?;

    let application = wide_os(request.program.as_os_str());
    let mut command_line = windows_command_line(request.program.as_os_str(), request.args);
    let cwd = if request.policy.mode == SandboxMode::WorkspaceWrite {
        request.policy.workspace.as_path()
    } else {
        request.program_root
    };
    let cwd = wide_os(cwd.as_os_str());
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdio.handles[0];
    startup.StartupInfo.hStdOutput = stdio.handles[1];
    startup.StartupInfo.hStdError = stdio.handles[2];
    startup.lpAttributeList = attributes.as_mut_ptr();
    let mut process_info = PROCESS_INFORMATION::default();

    trace_self_check(request, "以挂起状态创建目标进程");
    let created = unsafe {
        CreateProcessAsUserW(
            token.raw(),
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW,
            std::ptr::null(),
            cwd.as_ptr(),
            (&raw const startup.StartupInfo),
            &mut process_info,
        )
    };
    drop(stdio);
    if created == 0 {
        return Err(std::io::Error::last_os_error()).context("创建 AppContainer 目标进程失败");
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process_info.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(process_info.hThread) };
    trace_self_check(request, "配置目标进程令牌默认访问表");
    if let Err(error) =
        grant_process_token_default_access(raw_handle(&process), &[profile.sid, restriction.sid])
    {
        unsafe {
            TerminateProcess(raw_handle(&process), 1);
            WaitForSingleObject(raw_handle(&process), INFINITE);
        }
        return Err(error).context("配置 AppContainer 目标进程令牌失败");
    }
    trace_self_check(request, "将目标进程加入受限 Job");
    if unsafe { AssignProcessToJobObject(job.raw(), raw_handle(&process)) } == 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            TerminateProcess(raw_handle(&process), 1);
            WaitForSingleObject(raw_handle(&process), INFINITE);
        }
        return Err(error).context("将 AppContainer 进程加入 Job 失败");
    }
    trace_self_check(request, "恢复目标主线程");
    if unsafe { ResumeThread(raw_handle(&thread)) } == u32::MAX {
        let error = std::io::Error::last_os_error();
        job.terminate_and_wait()
            .context("恢复线程失败后清理 AppContainer Job 失败")?;
        return Err(error).context("恢复 AppContainer 主线程失败");
    }
    drop(thread);

    let mut handles = vec![raw_handle(&process)];
    if let Some(handle) = &host {
        handles.push(raw_handle(handle));
    }
    if let Some(handle) = &stop_event {
        handles.push(raw_handle(handle));
    }
    let wait_timeout = request
        .timeout
        .map(|timeout| timeout.as_millis().min((INFINITE - 1) as u128) as u32)
        .unwrap_or(INFINITE);
    trace_self_check(request, "等待目标进程退出");
    let wait =
        unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, wait_timeout) };
    if wait == WAIT_FAILED {
        let error = std::io::Error::last_os_error();
        job.terminate_and_wait()
            .context("等待失败后清理 AppContainer Job 失败")?;
        return Err(error).context("等待 AppContainer 进程失败");
    }
    if wait == WAIT_TIMEOUT {
        trace_self_check(request, "目标进程等待超时，终止受限 Job");
        job.terminate_and_wait()
            .context("超时后清理 AppContainer Job 失败")?;
        bail!("等待 AppContainer 进程超过 {:?}", request.timeout.unwrap());
    }
    let target_exited = wait == WAIT_OBJECT_0;
    let mut exit_code = 1;
    if target_exited && unsafe { GetExitCodeProcess(raw_handle(&process), &mut exit_code) } == 0 {
        let error = std::io::Error::last_os_error();
        job.terminate_and_wait()
            .context("读取退出码失败后清理 AppContainer Job 失败")?;
        return Err(error).context("读取 AppContainer 退出码失败");
    }
    trace_self_check(request, "终止目标残留进程");
    job.terminate_and_wait()?;
    trace_self_check(request, "目标进程树已清理");
    if unsafe { WaitForSingleObject(raw_handle(&process), INFINITE) } == WAIT_FAILED {
        return Err(std::io::Error::last_os_error()).context("等待 AppContainer 清理失败");
    }
    Ok(exit_code as i32)
}

fn grant_process_token_default_access(process: HANDLE, sids: &[PSID]) -> Result<()> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_ADJUST_DEFAULT | TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("打开 AppContainer 目标进程令牌失败");
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    grant_token_default_access(raw_handle(&token), sids)
}

struct WindowsJob(OwnedHandle);

impl WindowsJob {
    fn new(limits: SandboxResourceLimits) -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("创建 Windows Job Object 失败");
        }
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_JOB_TIME;
        info.BasicLimitInformation.PerJobUserTimeLimit =
            limits.max_cpu_time_seconds.saturating_mul(10_000_000) as i64;
        info.BasicLimitInformation.ActiveProcessLimit = limits.max_processes;
        info.JobMemoryLimit = limits.max_memory_bytes as usize;
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("配置 Windows Job 配额失败");
        }
        Ok(job)
    }

    fn raw(&self) -> HANDLE {
        raw_handle(&self.0)
    }

    fn terminate_and_wait(&self) -> Result<()> {
        if unsafe { TerminateJobObject(self.raw(), 1) } == 0 {
            return Err(std::io::Error::last_os_error()).context("终止 Windows Sandbox Job 失败");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            if unsafe {
                QueryInformationJobObject(
                    self.raw(),
                    JobObjectBasicAccountingInformation,
                    (&raw mut accounting).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("读取 Windows Sandbox Job 清理状态失败");
            }
            if accounting.ActiveProcesses == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "Windows Sandbox Job 清理超时，仍有 {} 个进程",
                    accounting.ActiveProcesses
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

struct ProcessAttributes {
    storage: Vec<usize>,
}

impl ProcessAttributes {
    fn new(
        capabilities: &SECURITY_CAPABILITIES,
        inherited_handles: &[HANDLE],
        child_process_policy: &u32,
    ) -> Result<Self> {
        let mut bytes = 0usize;
        let initialized =
            unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 3, 0, &mut bytes) };
        if initialized != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(std::io::Error::last_os_error())
                .context("计算 Windows 进程属性列表长度失败");
        }
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let pointer = storage.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(pointer, 3, 0, &mut bytes) } == 0 {
            return Err(std::io::Error::last_os_error()).context("初始化 Windows 进程属性失败");
        }
        if unsafe {
            UpdateProcThreadAttribute(
                pointer,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                (capabilities as *const SECURITY_CAPABILITIES).cast(),
                size_of::<SECURITY_CAPABILITIES>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            unsafe {
                DeleteProcThreadAttributeList(pointer);
            }
            return Err(std::io::Error::last_os_error()).context("应用 AppContainer 安全属性失败");
        }
        if unsafe {
            UpdateProcThreadAttribute(
                pointer,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherited_handles.as_ptr().cast(),
                size_of_val(inherited_handles),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            unsafe {
                DeleteProcThreadAttributeList(pointer);
            }
            return Err(std::io::Error::last_os_error()).context("限制 AppContainer 继承句柄失败");
        }
        if unsafe {
            UpdateProcThreadAttribute(
                pointer,
                0,
                PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY as usize,
                (child_process_policy as *const u32).cast(),
                size_of::<u32>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            unsafe {
                DeleteProcThreadAttributeList(pointer);
            }
            return Err(std::io::Error::last_os_error())
                .context("允许 AppContainer 子进程继承隔离策略失败");
        }
        Ok(Self { storage })
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.storage.as_mut_ptr().cast()
    }
}

impl Drop for ProcessAttributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.as_mut_ptr());
        }
    }
}

struct InheritableStdio {
    _owned: [OwnedHandle; 3],
    handles: [HANDLE; 3],
}

impl InheritableStdio {
    fn prepare() -> Result<Self> {
        let source = unsafe {
            [
                GetStdHandle(STD_INPUT_HANDLE),
                GetStdHandle(STD_OUTPUT_HANDLE),
                GetStdHandle(STD_ERROR_HANDLE),
            ]
        };
        let current_process = unsafe { GetCurrentProcess() };
        let mut duplicated = Vec::with_capacity(3);
        for handle in source {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                bail!("Launcher 缺少可继承的标准输入输出句柄");
            }
            let mut copy = std::ptr::null_mut();
            if unsafe {
                DuplicateHandle(
                    current_process,
                    handle,
                    current_process,
                    &mut copy,
                    0,
                    1,
                    DUPLICATE_SAME_ACCESS,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error()).context("复制 stdio 继承句柄失败");
            }
            duplicated.push(unsafe { OwnedHandle::from_raw_handle(copy) });
        }
        let owned: [OwnedHandle; 3] = duplicated
            .try_into()
            .map_err(|_| anyhow!("复制 stdio 句柄数量异常"))?;
        let handles = owned.each_ref().map(raw_handle);
        Ok(Self {
            _owned: owned,
            handles,
        })
    }
}

fn open_host_process(pid: u32) -> Result<OwnedHandle> {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("打开宿主进程监视句柄失败");
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn open_stop_event(name: &str) -> Result<OwnedHandle> {
    let name = wide(name);
    let handle = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("打开 Sandbox 停止事件失败");
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn windows_command_line(program: &OsStr, args: &[String]) -> Vec<u16> {
    let mut result = Vec::new();
    push_windows_arg(&mut result, program.encode_wide());
    for arg in args {
        result.push(b' ' as u16);
        push_windows_arg(&mut result, arg.encode_utf16());
    }
    result.push(0);
    result
}

fn push_windows_arg<I>(output: &mut Vec<u16>, value: I)
where
    I: IntoIterator<Item = u16>,
{
    let value = value.into_iter().collect::<Vec<_>>();
    let needs_quotes = value.is_empty()
        || value
            .iter()
            .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16 || *unit == b'"' as u16);
    if !needs_quotes {
        output.extend(value);
        return;
    }
    output.push(b'"' as u16);
    let mut backslashes = 0usize;
    for unit in value {
        if unit == b'\\' as u16 {
            backslashes += 1;
        } else if unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            output.push(unit);
            backslashes = 0;
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            output.push(unit);
            backslashes = 0;
        }
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    output.push(b'"' as u16);
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn wide_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn windows_argument_quoting_handles_spaces_quotes_and_backslashes() {
        let line = windows_command_line(
            OsStr::new(r"C:\Program Files\tool.exe"),
            &[r#"a\"b"#.to_string(), r"tail\".to_string()],
        );
        let text = OsString::from_wide(&line[..line.len() - 1])
            .to_string_lossy()
            .into_owned();
        assert_eq!(text, r#""C:\Program Files\tool.exe" "a\\\"b" tail\"#);
    }

    #[test]
    fn writable_tree_allows_hardlinks_inside_authorized_root() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("artifact.pdb");
        let alias = root.path().join("artifact-hash.pdb");
        std::fs::write(&original, b"pdb").unwrap();
        std::fs::hard_link(&original, &alias).unwrap();
        let root = crate::canonicalize_path(root.path()).unwrap();

        validate_writable_tree(&root, std::slice::from_ref(&root)).unwrap();
    }

    #[test]
    fn writable_tree_allows_hardlink_alias_across_authorized_roots() {
        let container = tempfile::tempdir().unwrap();
        let first = container.path().join("workspace");
        let second = container.path().join("cache");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let original = first.join("artifact.pdb");
        std::fs::write(&original, b"pdb").unwrap();
        std::fs::hard_link(&original, second.join("artifact.pdb")).unwrap();
        let first = crate::canonicalize_path(&first).unwrap();
        let second = crate::canonicalize_path(&second).unwrap();

        validate_writable_tree(&first, &[first.clone(), second]).unwrap();
    }

    #[test]
    fn writable_tree_rejects_hardlink_alias_outside_authorized_root() {
        let container = tempfile::tempdir().unwrap();
        let writable = container.path().join("workspace");
        let outside = container.path().join("outside");
        std::fs::create_dir_all(&writable).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let original = outside.join("secret.txt");
        std::fs::write(&original, b"secret").unwrap();
        std::fs::hard_link(&original, writable.join("escape.txt")).unwrap();
        let writable = crate::canonicalize_path(&writable).unwrap();

        let error = validate_writable_tree(&writable, std::slice::from_ref(&writable)).unwrap_err();
        assert!(error.to_string().contains("超出授权可写目录"));
    }
}
