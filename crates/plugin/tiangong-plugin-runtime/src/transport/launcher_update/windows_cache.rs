use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Key {
    schema: u32,
    program: PathBuf,
    digest: String,
    boot: [u8; 16],
}

impl Key {
    pub(super) fn new(program: &Path) -> Result<Self> {
        // Windows 每次启动生成新的 BootIdentifier，休眠恢复保留同一次启动。
        #[repr(C)]
        #[derive(Default)]
        struct BootEnvironment {
            identifier: windows_sys::core::GUID,
            firmware_type: u32,
            flags: u64,
        }
        let mut environment = BootEnvironment::default();
        let mut returned = 0;
        let status = unsafe {
            windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation(
                90, // SystemBootEnvironmentInformation
                (&raw mut environment).cast(),
                std::mem::size_of::<BootEnvironment>() as u32,
                &mut returned,
            )
        };
        if status < 0 || returned < 16 {
            bail!("无法取得 Windows 本次启动身份: {status:#x}");
        }
        let id = environment.identifier;
        let mut boot = [0; 16];
        boot[..4].copy_from_slice(&id.data1.to_le_bytes());
        boot[4..6].copy_from_slice(&id.data2.to_le_bytes());
        boot[6..8].copy_from_slice(&id.data3.to_le_bytes());
        boot[8..].copy_from_slice(&id.data4);
        if boot == [0; 16] {
            bail!("Windows 返回了空的启动身份");
        }
        Ok(Self {
            schema: 1,
            program: program.canonicalize()?,
            digest: hex::encode(Sha256::digest(std::fs::read(program)?)),
            boot,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    key: Key,
    version: String,
    capabilities: BTreeSet<String>,
}

fn path(storage_root: &Path) -> PathBuf {
    storage_root.join("sandbox").join("self-check.json")
}

pub(super) fn invalidate(storage_root: &Path) -> Result<()> {
    match std::fs::remove_file(path(storage_root)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("清理旧 Sandbox 自检记录失败"),
    }
}

pub(super) fn read(storage_root: &Path, key: &Key) -> Option<(String, BTreeSet<String>)> {
    use std::os::windows::fs::MetadataExt;
    let path = path(storage_root);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file()
        || metadata.len() > 8192
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return None;
    }
    let record: Record = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (record.key == *key && record.version.parse::<semver::Version>().is_ok())
        .then_some((record.version, record.capabilities))
}

pub(super) fn save(
    storage_root: &Path,
    key: Key,
    version: &str,
    capabilities: &BTreeSet<String>,
) -> Result<()> {
    let path = path(storage_root);
    let parent = path.parent().context("自检记录缺少父目录")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(
        file.as_file_mut(),
        &Record {
            key,
            version: version.into(),
            capabilities: capabilities.clone(),
        },
    )?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_and_binary_bound_cache_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("sandbox.exe");
        std::fs::write(&program, b"first").unwrap();
        let key = Key::new(&program).unwrap();
        let capabilities: BTreeSet<String> = ["browser_runtime".to_string()].into();
        assert!(read(root.path(), &key).is_none());
        save(root.path(), key, "0.1.4", &capabilities).unwrap();
        let mut same = Key::new(&program).unwrap();
        assert_eq!(
            read(root.path(), &same),
            Some(("0.1.4".to_string(), capabilities.clone()))
        );
        invalidate(root.path()).unwrap();
        assert!(read(root.path(), &same).is_none());
        save(
            root.path(),
            Key::new(&program).unwrap(),
            "0.1.4",
            &capabilities,
        )
        .unwrap();
        same.boot[0] ^= 1;
        assert!(read(root.path(), &same).is_none());
        std::fs::write(&program, b"other").unwrap();
        assert!(read(root.path(), &Key::new(&program).unwrap()).is_none());
        std::fs::write(path(root.path()), b"broken").unwrap();
        assert!(read(root.path(), &same).is_none());
    }
}
