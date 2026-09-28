//! Linux 上的 ONNX Runtime 动态库：按需下载、校验并在进程内加载。
//!
//! pyke 为 Linux 提供的静态库依赖 glibc 2.38+（`__isoc23_*` 符号），在
//! Ubuntu 22.04 等仍在支持期的发行版上无法链接。因此 Linux 构建启用
//! `ort/load-dynamic`，运行时 dlopen 微软官方发布的 `libonnxruntime.so`：
//! 其 glibc 基线为 2.27、libstdc++ 为 GLIBCXX_3.4.22，覆盖 Ubuntu 18.04+ /
//! Debian 10+ / RHEL 8+。
//!
//! 库文件与模型走同一套下载源与校验（大小 + sha256），落盘到
//! `<storage>/memory/models/onnxruntime/<version>/<platform>/`，路径只在本进程
//! 通过 `ort::init_from` 指定，不修改 `LD_LIBRARY_PATH`，也不依赖系统安装。
//!
//! macOS / Windows 仍静态链接，本模块在这些平台上的 [`ensure_loaded`] 为空操作。

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::catalog::ModelFile;

/// 动态库使用的 ONNX Runtime 版本。
///
/// fastembed 7.1 固定开启 `ort/api-24`，ort 加载时要求库的次版本号 ≥ 24。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const ORT_VERSION: &str = "1.24.1";

/// 某个平台的动态库文件描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeLibrary {
    /// 平台目录名（与 OSS 路径一致）。
    pub(crate) platform: &'static str,
    pub(crate) file: ModelFile,
}

/// 微软官方 GitHub Release `onnxruntime-linux-x64-1.24.1.tgz` 中的
/// `lib/libonnxruntime.so.1.24.1`（上游 tgz sha256
/// 9142552248b735920f9390027e4512a2cacf8946a1ffcbe9071a5c210531026f）。
pub(crate) const LINUX_X86_64: RuntimeLibrary = RuntimeLibrary {
    platform: "linux-x86_64",
    file: ModelFile {
        path: "libonnxruntime.so.1.24.1",
        size: 22_044_576,
        sha256: "e5a7e3646718d8f1f8f52c8fcb770fe229ab44305caf3ea702d558e6e426c9aa",
    },
};

/// 微软官方 GitHub Release `onnxruntime-linux-aarch64-1.24.1.tgz` 中的
/// `lib/libonnxruntime.so.1.24.1`（上游 tgz sha256
/// 0f56edd68f7602df790b68b874a46b115add037e88385c6c842bb763b39b9f89）。
pub(crate) const LINUX_AARCH64: RuntimeLibrary = RuntimeLibrary {
    platform: "linux-aarch64",
    file: ModelFile {
        path: "libonnxruntime.so.1.24.1",
        size: 18_559_840,
        sha256: "7954e8bdedb497f830c6a679e818d98399b7f4d81ade1126c3e0be74d28111ab",
    },
};

/// 当前平台需要的动态库；静态链接平台返回 None。
pub(crate) fn current_library() -> Option<&'static RuntimeLibrary> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(&LINUX_X86_64)
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(&LINUX_AARCH64)
    } else {
        None
    }
}

/// 当前平台是否支持内置本地推理。
///
/// 静态链接平台恒为 true；Linux 仅 x86_64 / aarch64 有官方动态库。
pub(crate) fn platform_supported() -> bool {
    !cfg!(target_os = "linux") || current_library().is_some()
}

/// 动态库在 OSS 上相对下载源根的路径：`onnxruntime/<version>/<platform>/<file>`。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn library_url_path(library: &RuntimeLibrary) -> String {
    format!(
        "onnxruntime/{ORT_VERSION}/{}/{}",
        library.platform, library.file.path
    )
}

/// 动态库本地目录：`<models>/onnxruntime/<version>/<platform>/`。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn library_dir(root: &Path, library: &RuntimeLibrary) -> PathBuf {
    root.join("onnxruntime")
        .join(ORT_VERSION)
        .join(library.platform)
}

/// 确保 ONNX Runtime 可用：Linux 上下载（如需）并加载动态库；其他平台为空操作。
///
/// 进程内只加载一次；加载失败（如 glibc 过旧、架构不符）会返回可读错误，
/// 调用方据此把本地模型标记为不可用并降级召回。
pub(crate) async fn ensure_loaded(cancel: &super::download::CancelToken) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::ensure_loaded(cancel).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cancel;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::OnceLock;

    use anyhow::{Context, Result, anyhow, bail};

    use super::super::download::{self, CancelToken, DownloadError};
    use super::{current_library, library_dir};

    /// 加载结果（成功后永久复用；失败保存原因，避免每次重试都 dlopen）。
    static LOADED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    /// 串行化首次加载。
    static LOAD_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(super) async fn ensure_loaded(cancel: &CancelToken) -> Result<()> {
        if let Some(result) = LOADED.get() {
            return result.clone().map_err(|error| anyhow!(error));
        }
        let _guard = LOAD_GUARD.lock().await;
        if let Some(result) = LOADED.get() {
            return result.clone().map_err(|error| anyhow!(error));
        }
        let Some(library) = current_library() else {
            bail!(
                "当前 Linux 架构（{}）没有可用的 ONNX Runtime，内置本地模型不可用",
                std::env::consts::ARCH
            );
        };
        let root = download::models_dir();
        let dir = library_dir(&root, library);
        let path = match download::ensure_runtime_library(&root, library, cancel).await {
            Ok(path) => path,
            // 下载阶段的失败可重试，不写入永久结果。
            Err(DownloadError::Busy) => bail!(super::super::TRANSIENT_BUSY),
            Err(DownloadError::Cancelled) => bail!(super::super::TRANSIENT_CANCELLED),
            Err(DownloadError::Failed(error)) => {
                return Err(error).with_context(|| "下载 ONNX Runtime 运行库失败");
            }
        };
        debug_assert!(path.starts_with(&dir));
        // dlopen 与版本检查：失败多为系统库不兼容，重试无意义，记为永久结果。
        let result = tokio::task::spawn_blocking(move || {
            ort::init_from(&path)
                .map(|builder| {
                    builder.commit();
                })
                .map_err(|error| format!("加载 ONNX Runtime 运行库失败：{error}"))
        })
        .await
        .map_err(|error| anyhow!("加载 ONNX Runtime 任务异常退出: {error}"))?;
        match &result {
            Ok(()) => tracing::info!(
                version = super::ORT_VERSION,
                platform = library.platform,
                "Memory 已加载 ONNX Runtime 动态库"
            ),
            Err(error) => tracing::warn!("{error}"),
        }
        let _ = LOADED.set(result.clone());
        result.map_err(|error| anyhow!(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_paths_follow_oss_layout() {
        assert_eq!(
            library_url_path(&LINUX_X86_64),
            "onnxruntime/1.24.1/linux-x86_64/libonnxruntime.so.1.24.1"
        );
        assert_eq!(
            library_url_path(&LINUX_AARCH64),
            "onnxruntime/1.24.1/linux-aarch64/libonnxruntime.so.1.24.1"
        );
        let root = Path::new("/data/models");
        assert_eq!(
            library_dir(root, &LINUX_AARCH64),
            Path::new("/data/models/onnxruntime/1.24.1/linux-aarch64")
        );
    }

    #[test]
    fn library_entries_are_well_formed() {
        for library in [LINUX_X86_64, LINUX_AARCH64] {
            assert_eq!(library.file.sha256.len(), 64);
            assert!(library.file.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(library.file.size > 10_000_000);
            assert!(library.file.path.ends_with(ORT_VERSION));
        }
    }

    #[test]
    fn platform_support_matches_target() {
        if cfg!(target_os = "linux") {
            assert_eq!(
                platform_supported(),
                matches!(std::env::consts::ARCH, "x86_64" | "aarch64")
            );
        } else {
            assert!(platform_supported());
            assert!(current_library().is_none());
        }
    }
}
