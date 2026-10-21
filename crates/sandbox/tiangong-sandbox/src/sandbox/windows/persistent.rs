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
            // Windows 锁冲突返回 ERROR_LOCK_VIOLATION(33)，标准库不归为
            // WouldBlock，两者都按“仍被占用、稍后重试”处理。
            Err(error)
                if (error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(33))
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error).context("持久授权仍被运行中的沙箱占用"),
        }
    }
}

fn read(path: &Path) -> Result<Option<Record>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("持久授权记录必须是普通文件");
    }
    let record: Record = serde_json::from_slice(&std::fs::read(path)?)?;
    if record.schema != 1 {
        bail!("不支持的持久授权记录版本");
    }
    record
        .generation
        .parse::<scru128::Id>()
        .context("持久授权身份无效")?;
    Ok(Some(record))
}

fn save(path: &Path, record: &Record) -> Result<()> {
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
    if !writable.iter().all(|root| {
        sids.iter()
            .all(|sid| has_entry(root, *sid, FILE_WORKSPACE_ACCESS, inheritance, false))
    }) {
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
    paths.extend(record.policy.writable_roots());
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

pub(super) fn prepare(
    request: &WindowsLaunchRequest<'_>,
    path: &Path,
    capabilities: &mut CapabilitySet,
) -> Result<Lease> {
    if !path.is_absolute() {
        bail!("持久授权记录必须使用绝对路径");
    }
    let parent = path.parent().context("持久授权记录缺少目录")?;
    std::fs::create_dir_all(parent)?;
    let parent = crate::canonicalize_path(parent)?;
    let protected = request
        .policy
        .denied_read_roots()
        .iter()
        .any(|root| crate::canonicalize_path(root).is_ok_and(|root| parent.starts_with(root)));
    if !protected {
        bail!("持久授权目录必须位于宿主声明的禁读目录内");
    }
    let file = lock(path)?;
    let previous = read(path)?;
    let mut record = match previous {
        Some(record) if record.program == request.program && record.policy == *request.policy => {
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
        let mut grants =
            AclGrants::apply(file_sid, restriction.sid, request.program, request.policy)?;
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
        let lease = prepare(&request(&policy), &cache, &mut caps).unwrap();
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
}
