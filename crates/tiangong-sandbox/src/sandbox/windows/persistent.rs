//! Windows 持久授权。
//!
//! - 通用授权（[`prepare_common`]）：天工所有沙箱进程共享的可写目录，按稳定
//!   身份授予一次并持久保存；之后启动只核对各根目录上的授权项，缺失才补授。
//!   只增不减，不随单个插件策略变化撤销，避免并发运行的进程失去访问。
//! - 插件级授权（[`prepare`]）：旧宿主按插件持久化的整套策略授权，保留兼容；
//!   新宿主只使用通用授权，插件差异部分每次启动临时授予。
use super::*;
use std::fs::{File, OpenOptions};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, DeriveCapabilitySidsFromName, EqualSid, GetAce, GetLengthSid,
};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    generation: String,
    program: PathBuf,
    policy: SandboxPolicy,
    prepared: bool,
    /// 已由通用持久授权覆盖、本记录不授予也不撤销的可写根。
    #[serde(default)]
    common_roots: Vec<PathBuf>,
}

/// 通用持久授权记录：稳定身份与已授予的根目录（只增不减）。
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CommonRecord {
    schema: u32,
    generation: String,
    roots: Vec<PathBuf>,
}

pub(super) struct Lease {
    _lock: File,
    pub(super) restriction: RestrictionSid,
}

fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .context("打开持久授权锁失败")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error).context("持久授权仍被运行中的沙箱占用"),
        }
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("持久授权记录必须是普通文件");
    }
    Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
}

fn read(path: &Path) -> Result<Option<Record>> {
    let Some(record) = read_json::<Record>(path)? else {
        return Ok(None);
    };
    if record.schema != 1 {
        bail!("不支持的持久授权记录版本");
    }
    record
        .generation
        .parse::<scru128::Id>()
        .context("持久授权身份无效")?;
    Ok(Some(record))
}

fn save<T: serde::Serialize>(path: &Path, record: &T) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("授权记录缺少父目录")?)?;
    serde_json::to_writer(file.as_file_mut(), record)?;
    file.as_file().sync_all()?;
    // 其他插件传播目录权限时可能短暂占用记录，保留原子替换并限时重试。
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match file.persist(path) {
            Ok(_) => return Ok(()),
            Err(error)
                if matches!(error.error.raw_os_error(), Some(5 | 32 | 33))
                    && Instant::now() < deadline =>
            {
                file = error.file;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.error).context("保存持久授权记录失败"),
        }
    }
}

fn capability(set: &mut CapabilitySet, generation: &str) -> Result<PSID> {
    add_named_capability(set, &format!("TiangongSandbox.Files.{generation}"))
}

pub(super) fn add_named_capability(set: &mut CapabilitySet, name: &str) -> Result<PSID> {
    let name = wide(name);
    let mut groups: *mut PSID = std::ptr::null_mut();
    let mut group_count = 0;
    let mut sids: *mut PSID = std::ptr::null_mut();
    let mut count = 0;
    if unsafe {
        DeriveCapabilitySidsFromName(
            name.as_ptr(),
            &mut groups,
            &mut group_count,
            &mut sids,
            &mut count,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("创建持久文件访问身份失败");
    }
    let bytes = if count > 0 {
        let sid = unsafe { *sids };
        Some(
            unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize) }
                .to_vec()
                .into_boxed_slice(),
        )
    } else {
        None
    };
    // 系统分别分配数组和各 SID，必须逐项释放。
    for (array, length) in [(groups, group_count), (sids, count)] {
        if !array.is_null() {
            for index in 0..length as usize {
                unsafe {
                    LocalFree((*array.add(index)).cast());
                }
            }
            unsafe {
                LocalFree(array.cast());
            }
        }
    }
    let mut bytes = bytes.context("系统没有返回文件访问身份")?;
    let sid = bytes.as_mut_ptr().cast();
    set._sid_storage.push(bytes);
    set.entries.push(SID_AND_ATTRIBUTES {
        Sid: sid,
        Attributes: SE_GROUP_ENABLED,
    });
    Ok(sid)
}

fn restriction(record: &Record) -> Result<RestrictionSid> {
    RestrictionSid::from_value(record.generation.parse::<scru128::Id>()?.to_u128())
}

fn has_entry(path: &Path, sid: PSID, mask: u32, inheritance: u32, deny: bool) -> bool {
    // Windows 不在普通文件上保留目录继承标记。
    let inheritance = if path.is_dir() { inheritance } else { 0 };
    let name = wide_os(path.as_os_str());
    let mut acl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let result = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != ERROR_SUCCESS {
        return false;
    }
    let mut found = false;
    if !acl.is_null() {
        for index in 0..unsafe { (*acl).AceCount } as u32 {
            let mut raw = std::ptr::null_mut();
            if unsafe { GetAce(acl, index, &mut raw) } == 0 {
                break;
            }
            let header = unsafe { &*raw.cast::<ACE_HEADER>() };
            if header.AceType != u8::from(deny) || header.AceFlags & 16 != 0 {
                continue;
            }
            let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
            if ace.Mask & mask == mask
                && u32::from(header.AceFlags) & inheritance == inheritance
                && unsafe { EqualSid((&raw const ace.SidStart).cast_mut().cast(), sid) } != 0
            {
                found = true;
                break;
            }
        }
    }
    unsafe {
        LocalFree(descriptor);
    }
    found
}

fn permissions_present(record: &Record, file_sid: PSID, restriction_sid: PSID) -> bool {
    let sids = [file_sid, restriction_sid];
    if !sids
        .iter()
        .all(|sid| has_entry(&record.program, *sid, FILE_PROGRAM_ACCESS, 0, false))
    {
        return false;
    }
    let writable = record.policy.writable_roots();
    let inheritance = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    if !writable
        .iter()
        .filter(|root| !record.common_roots.contains(root))
        .collect::<Vec<_>>()
        .iter()
        .all(|root| {
            sids.iter()
                .all(|sid| has_entry(root, *sid, FILE_WORKSPACE_ACCESS, inheritance, false))
        })
    {
        return false;
    }
    for path in record.policy.read_only_roots() {
        if path.exists() && !has_entry(&path, restriction_sid, FILE_WRITE_ACCESS, inheritance, true)
        {
            return false;
        }
    }
    for path in record.policy.denied_read_roots() {
        if path.exists()
            && writable.iter().any(|root| path.starts_with(root))
            && !has_entry(&path, restriction_sid, FILE_ALL_ACCESS, inheritance, true)
        {
            return false;
        }
    }
    true
}

fn revoke_record(record: &Record) -> Result<()> {
    let mut capabilities = CapabilitySet::new(false)?;
    let file_sid = capability(&mut capabilities, &record.generation)?;
    let restriction = restriction(record)?;
    let mut paths = vec![record.program.clone()];
    paths.extend(
        record
            .policy
            .writable_roots()
            .into_iter()
            .filter(|root| !record.common_roots.contains(root)),
    );
    paths.extend(record.policy.read_only_roots());
    let writable = record.policy.writable_roots();
    paths.extend(
        record
            .policy
            .denied_read_roots()
            .into_iter()
            .filter(|path| writable.iter().any(|root| path.starts_with(root))),
    );
    let mut grants = AclGrants {
        roots: paths
            .into_iter()
            .map(|path| AclRoot {
                path,
                sids: vec![file_sid, restriction.sid],
            })
            .collect(),
        active: true,
    };
    grants.revoke()
}

pub fn revoke(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let _lock = lock(path)?;
    if let Some(record) = read(path)? {
        revoke_record(&record)?;
    }
    std::fs::remove_file(path).context("删除已撤销的授权记录失败")
}

/// 授权记录必须位于策略禁读目录内（宿主管理目录，沙箱进程不可读写）。
fn ensure_managed_location(path: &Path, policy: &SandboxPolicy) -> Result<()> {
    if !path.is_absolute() {
        bail!("持久授权记录必须使用绝对路径");
    }
    let parent = path.parent().context("持久授权记录缺少目录")?;
    std::fs::create_dir_all(parent)?;
    let parent = crate::canonicalize_path(parent)?;
    let protected = policy
        .denied_read_roots()
        .iter()
        .any(|root| crate::canonicalize_path(root).is_ok_and(|root| parent.starts_with(root)));
    if !protected {
        bail!("持久授权目录必须位于宿主声明的禁读目录内");
    }
    Ok(())
}

fn common_capability(set: &mut CapabilitySet, generation: &str) -> Result<PSID> {
    add_named_capability(set, &format!("TiangongSandbox.Common.{generation}"))
}

/// 准备通用持久授权，返回通用身份 SID（同时加入 `capabilities`，调用方还需
/// 把它加入受限令牌的限制 SID 列表）。返回的指针指向 `capabilities` 持有的
/// 存储，只在其存活期间有效。
///
/// 只在根目录缺少授权项时才校验目录树并授予（可继承，一次性传播）；已授予
/// 时仅读取根目录 ACL 核对。锁只在准备期间持有，不随进程生命周期占用，
/// 多个插件可并发启动。
pub(super) fn prepare_common(
    cache: &Path,
    roots: &[PathBuf],
    policy: &SandboxPolicy,
    capabilities: &mut CapabilitySet,
) -> Result<PSID> {
    ensure_managed_location(cache, policy)?;
    let _lock = lock(cache)?;
    let mut record = match read_json::<CommonRecord>(cache)? {
        Some(record) if record.schema == 1 => {
            record
                .generation
                .parse::<scru128::Id>()
                .context("通用授权身份无效")?;
            record
        }
        Some(_) => bail!("不支持的通用授权记录版本"),
        None => CommonRecord {
            schema: 1,
            generation: scru128::new().to_string(),
            roots: Vec::new(),
        },
    };
    let sid = common_capability(capabilities, &record.generation)?;
    let inheritance = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    let missing = roots
        .iter()
        .filter(|root| !has_entry(root, sid, FILE_WORKSPACE_ACCESS, inheritance, false))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        // 先登记再授予：中途失败后仍能按记录撤销。
        for root in &missing {
            if !record.roots.contains(root) {
                record.roots.push((*root).clone());
            }
        }
        save(cache, &record)?;
        for root in missing {
            // 硬链接别名只能落在通用根内：通用授权对所有插件生效。
            validate_writable_tree(root, roots)
                .with_context(|| format!("通用授权目录不安全: {}", root.display()))?;
            modify_acl(
                root,
                &[sid],
                FILE_WORKSPACE_ACCESS,
                inheritance,
                GRANT_ACCESS,
            )?;
        }
    }
    if std::env::var_os("TIANGONG_SANDBOX_DIAGNOSTICS").is_some() {
        eprintln!("Windows 通用持久授权 granted={}", roots.len());
    }
    Ok(sid)
}

/// 撤销通用持久授权并删除记录。
pub fn revoke_common(cache: &Path) -> Result<()> {
    if !cache.exists() {
        return Ok(());
    }
    let _lock = lock(cache)?;
    if let Some(record) = read_json::<CommonRecord>(cache)? {
        let mut capabilities = CapabilitySet::new(false)?;
        let sid = common_capability(&mut capabilities, &record.generation)?;
        let mut failures = Vec::new();
        for root in record.roots.iter().filter(|root| root.exists()) {
            if let Err(error) = modify_acl(root, &[sid], 0, NO_INHERITANCE, REVOKE_ACCESS) {
                failures.push(format!("{}: {error:#}", root.display()));
            }
        }
        if !failures.is_empty() {
            bail!("撤销通用授权失败: {}", failures.join("; "));
        }
    }
    std::fs::remove_file(cache).context("删除已撤销的通用授权记录失败")
}

pub(super) fn prepare(
    request: &WindowsLaunchRequest<'_>,
    path: &Path,
    common_roots: &[PathBuf],
    capabilities: &mut CapabilitySet,
) -> Result<Lease> {
    ensure_managed_location(path, request.policy)?;
    let file = lock(path)?;
    let previous = read(path)?;
    let mut record = match previous {
        Some(record)
            if record.program == request.program
                && record.policy == *request.policy
                && record.common_roots == common_roots =>
        {
            record
        }
        previous => {
            if let Some(record) = previous {
                revoke_record(&record)?;
            }
            Record {
                schema: 1,
                generation: scru128::new().to_string(),
                program: request.program.to_path_buf(),
                policy: request.policy.clone(),
                prepared: false,
                common_roots: common_roots.to_vec(),
            }
        }
    };
    let file_sid = capability(capabilities, &record.generation)?;
    let restriction = restriction(&record)?;
    let reused = record.prepared && permissions_present(&record, file_sid, restriction.sid);
    if !reused {
        // 先记身份再授予权限：进程意外结束后，下次仍能撤销未完成的授权。
        record.prepared = false;
        save(path, &record)?;
        let mut grants = AclGrants::apply(
            file_sid,
            restriction.sid,
            request.program,
            request.policy,
            common_roots,
        )?;
        record.prepared = true;
        save(path, &record)?;
        grants.active = false;
    }
    if std::env::var_os("TIANGONG_SANDBOX_DIAGNOSTICS").is_some() {
        eprintln!("Windows 持久授权 reused={reused}");
    }
    Ok(Lease {
        _lock: file,
        restriction,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn reuse_repair_policy_change_and_revoke() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let managed = root.path().join("managed");
        let other = root.path().join("other");
        for path in [&workspace, &managed, &other] {
            std::fs::create_dir(path).unwrap();
        }
        let program = root.path().join("program.exe");
        std::fs::write(&program, b"stub").unwrap();
        let cache = managed.join("grant.json");
        let mut policy = SandboxPolicy::workspace_write(&workspace);
        policy.denied_read_paths.push(managed);
        let protected = workspace.join("config.json");
        std::fs::write(&protected, b"{}").unwrap();
        policy.protected_paths.push(protected.clone());
        policy.denied_read_paths.push(protected);
        let request = |policy| WindowsLaunchRequest {
            program: &program,
            program_root: root.path(),
            args: &[],
            policy,
            host_pid: None,
            stop_event_name: None,
            timeout: None,
        };
        let mut caps = CapabilitySet::new(false).unwrap();
        let lease = prepare(&request(&policy), &cache, &[], &mut caps).unwrap();
        let initial = read(&cache).unwrap().unwrap();
        let occupied = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&cache)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(occupied);
        });
        save(&cache, &initial).unwrap();
        release.join().unwrap();
        let mut identities = CapabilitySet::new(false).unwrap();
        let sid = capability(&mut identities, &initial.generation).unwrap();
        assert!(has_entry(&workspace, sid, FILE_WORKSPACE_ACCESS, 3, false));
        assert!(permissions_present(&initial, sid, lease.restriction.sid));
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(cache.with_extension("lock"))
            .unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&contender).is_err());
        drop(contender);
        drop(lease);
        drop(
            prepare(
                &request(&policy),
                &cache,
                &[],
                &mut CapabilitySet::new(false).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            read(&cache).unwrap().unwrap().generation,
            initial.generation
        );

        modify_acl(&workspace, &[sid], 0, 0, REVOKE_ACCESS).unwrap();
        drop(
            prepare(
                &request(&policy),
                &cache,
                &[],
                &mut CapabilitySet::new(false).unwrap(),
            )
            .unwrap(),
        );
        assert!(has_entry(&workspace, sid, FILE_WORKSPACE_ACCESS, 3, false));

        let mut changed = policy.clone();
        changed.workspace = other;
        drop(
            prepare(
                &request(&changed),
                &cache,
                &[],
                &mut CapabilitySet::new(false).unwrap(),
            )
            .unwrap(),
        );
        assert_ne!(
            read(&cache).unwrap().unwrap().generation,
            initial.generation
        );
        assert!(!has_entry(&workspace, sid, FILE_WORKSPACE_ACCESS, 3, false));
        let current = read(&cache).unwrap().unwrap();
        let new_sid = capability(&mut identities, &current.generation).unwrap();
        revoke(&cache).unwrap();
        assert!(!cache.exists());
        assert!(!has_entry(
            &changed.workspace,
            new_sid,
            FILE_WORKSPACE_ACCESS,
            3,
            false
        ));
    }

    #[test]
    fn common_grants_are_granted_once_checked_and_revocable() {
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let tmp = root.path().join("tmp");
        let managed = root.path().join("managed");
        let workspace = root.path().join("workspace");
        for path in [&media, &tmp, &managed, &workspace] {
            std::fs::create_dir(path).unwrap();
        }
        let cache = managed.join("common.json");
        let mut policy = SandboxPolicy::workspace_write(&workspace);
        policy.extra_writable = vec![media.clone(), tmp.clone()];
        policy.denied_read_paths.push(managed.clone());
        let roots = vec![
            crate::canonicalize_path(&media).unwrap(),
            crate::canonicalize_path(&tmp).unwrap(),
        ];
        let inheritance = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;

        // 返回的 SID 指向 CapabilitySet 持有的存储，集合必须与 SID 同生命周期。
        let mut first_caps = CapabilitySet::new(false).unwrap();
        let sid = prepare_common(&cache, &roots, &policy, &mut first_caps).unwrap();
        for root in &roots {
            assert!(has_entry(
                root,
                sid,
                FILE_WORKSPACE_ACCESS,
                inheritance,
                false
            ));
        }
        // 工作区不属于通用授权。
        assert!(!has_entry(
            &workspace,
            sid,
            FILE_WORKSPACE_ACCESS,
            inheritance,
            false
        ));
        let first = read_json::<CommonRecord>(&cache).unwrap().unwrap();

        // 再次准备：身份不变；被外部撤掉的根自动补授。
        modify_acl(&roots[1], &[sid], 0, 0, REVOKE_ACCESS).unwrap();
        let mut again_caps = CapabilitySet::new(false).unwrap();
        let again = prepare_common(&cache, &roots, &policy, &mut again_caps).unwrap();
        assert!(unsafe { EqualSid(sid, again) } != 0);
        assert!(has_entry(
            &roots[1],
            sid,
            FILE_WORKSPACE_ACCESS,
            inheritance,
            false
        ));
        assert_eq!(
            read_json::<CommonRecord>(&cache)
                .unwrap()
                .unwrap()
                .generation,
            first.generation
        );

        // 记录必须位于禁读目录内。
        let mut unmanaged = policy.clone();
        unmanaged.denied_read_paths.clear();
        assert!(
            prepare_common(
                &cache,
                &roots,
                &unmanaged,
                &mut CapabilitySet::new(false).unwrap()
            )
            .is_err()
        );

        revoke_common(&cache).unwrap();
        assert!(!cache.exists());
        for root in &roots {
            assert!(!has_entry(
                root,
                sid,
                FILE_WORKSPACE_ACCESS,
                inheritance,
                false
            ));
        }
    }
}
