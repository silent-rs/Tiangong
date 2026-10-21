//! Sandbox 程序定位。
//!
//! 安装目录完全由宿主决定；本模块只约定目录内的程序名与伴生签名名，
//! 不认识天工的存储根或目录布局。

use std::path::{Path, PathBuf};

pub fn builtin_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn launcher_file_name() -> String {
    format!("tiangong-sandbox{}", std::env::consts::EXE_SUFFIX)
}

pub fn installed_program(directory: &Path) -> PathBuf {
    directory.join(launcher_file_name())
}

pub fn signature_path(program: &Path) -> PathBuf {
    program.with_file_name(format!(
        "{}.sig",
        program
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    ))
}

/// 返回值仍需由调用方逐次验签与自检。
pub fn resolve_installed_program(directory: &Path) -> Option<PathBuf> {
    regular_installed_program(directory)
}

fn regular_installed_program(directory: &Path) -> Option<PathBuf> {
    if !directory.is_absolute() {
        return None;
    }
    let program = installed_program(directory);
    let program_meta = std::fs::symlink_metadata(&program).ok()?;
    if !program_meta.is_file() || program_meta.file_type().is_symlink() {
        return None;
    }
    let signature_meta = std::fs::symlink_metadata(signature_path(&program)).ok()?;
    if !signature_meta.is_file() || signature_meta.file_type().is_symlink() {
        return None;
    }
    Some(program)
}

/// 查找当前宿主程序同目录的 Sandbox。是否采用该布局由宿主决定。
pub fn sibling_program() -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    let directory = current_exe.parent()?;
    let direct = directory.join(launcher_file_name());
    if direct.is_file() {
        return Some(direct);
    }
    #[cfg(debug_assertions)]
    if directory.file_name().is_some_and(|name| name == "deps") {
        let debug_binary = directory.parent()?.join(launcher_file_name());
        if debug_binary.is_file() {
            return Some(debug_binary);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_directory_is_owned_by_host() {
        let directory = Path::new("/host/chosen/location");
        assert_eq!(
            installed_program(directory),
            directory.join(launcher_file_name())
        );
    }

    #[test]
    fn resolver_requires_program_and_signature() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("chosen");
        std::fs::create_dir_all(&directory).unwrap();
        let program = installed_program(&directory);
        std::fs::write(&program, b"sandbox").unwrap();
        assert!(resolve_installed_program(&directory).is_none());
        std::fs::write(signature_path(&program), b"signature").unwrap();
        assert_eq!(resolve_installed_program(&directory), Some(program));
    }

    #[cfg(unix)]
    #[test]
    fn resolver_rejects_symlink_program() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("chosen");
        std::fs::create_dir_all(&directory).unwrap();
        let target = root.path().join("target");
        std::fs::write(&target, b"sandbox").unwrap();
        let program = installed_program(&directory);
        std::os::unix::fs::symlink(target, &program).unwrap();
        std::fs::write(signature_path(&program), b"signature").unwrap();
        assert!(resolve_installed_program(&directory).is_none());
    }
}
