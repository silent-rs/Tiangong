//! Linux / Windows 上的 ONNX Runtime 动态库：按需下载、校验并在进程内加载。
//!
//! - Linux：pyke 提供的静态库依赖 glibc 2.38+（`__isoc23_*` 符号），在
//!   Ubuntu 22.04 等仍在支持期的发行版上无法链接。改为运行时 dlopen 微软官方
//!   发布的 `libonnxruntime.so`：其 glibc 基线为 2.27、libstdc++ 为
//!   GLIBCXX_3.4.22，覆盖 Ubuntu 18.04+ / Debian 10+ / RHEL 8+。
//! - Windows：pyke 静态库以动态 CRT 构建并强制链接 DirectML，静态链接后
//!   sidecar.exe 会静态导入 `MSVCP140.dll` / `MSVCP140_1.dll` /
//!   `VCRUNTIME140_1.dll` / `DirectML.dll` / `d3d12.dll` / `dxgi.dll` 等。任一
//!   模块缺失或被安全软件拒绝访问时，进程在加载阶段即以 0xC0000022 /
//!   0xC0000135 退出，一行用户代码都不执行，整个插件无法启动。改为运行时加载
//!   微软官方 CPU 版 `onnxruntime.dll`，并随附它依赖的 VC++ 运行库
//!   （`vcruntime140.dll` / `vcruntime140_1.dll` / `msvcp140.dll` /
//!   `msvcp140_1.dll`）：先按绝对路径预加载这些运行库，再加载 ONNX Runtime。
//!   Windows 加载器解析导入时优先复用进程内已加载的同名模块（它们不属于
//!   KnownDLLs），因此不依赖、也不触碰 System32 中的副本——用户无需安装
//!   VC++ 运行库，系统副本缺失、版本过旧或被安全软件锁定都不影响。
//!
//! 库文件与模型走同一套下载源与校验（大小 + sha256），落盘到
//! `<storage>/memory/models/onnxruntime/<version>/<platform>/`，路径只在本进程
//! 内指定，不修改 `LD_LIBRARY_PATH` / `PATH`，也不依赖系统安装。
//!
//! macOS 仍静态链接，本模块在该平台上的 [`ensure_loaded`] 为空操作。

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::catalog::ModelFile;

/// 动态库使用的 ONNX Runtime 版本。
///
/// fastembed 7.1 固定开启 `ort/api-24`，ort 加载时要求库的次版本号 ≥ 24。
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub(crate) const ORT_VERSION: &str = "1.24.1";

/// 某个平台的动态库文件描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeLibrary {
    /// 平台目录名（与 OSS 路径一致）。
    pub(crate) platform: &'static str,
    /// 与主库放在同一目录、需在主库之前按顺序预加载的依赖（Linux 为空）。
    pub(crate) dependencies: &'static [ModelFile],
    /// ONNX Runtime 主库。
    pub(crate) file: ModelFile,
}

impl RuntimeLibrary {
    /// 需要下载的全部文件：依赖在前（即加载顺序），主库最后。
    pub(crate) fn files(&self) -> impl Iterator<Item = &ModelFile> {
        self.dependencies.iter().chain(std::iter::once(&self.file))
    }

    /// 全部文件总字节数。
    #[cfg(test)]
    pub(crate) fn total_size(&self) -> u64 {
        self.files().map(|file| file.size).sum()
    }
}

/// 微软官方 GitHub Release `onnxruntime-linux-x64-1.24.1.tgz` 中的
/// `lib/libonnxruntime.so.1.24.1`（上游 tgz sha256
/// 9142552248b735920f9390027e4512a2cacf8946a1ffcbe9071a5c210531026f）。
pub(crate) const LINUX_X86_64: RuntimeLibrary = RuntimeLibrary {
    platform: "linux-x86_64",
    dependencies: &[],
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
    dependencies: &[],
    file: ModelFile {
        path: "libonnxruntime.so.1.24.1",
        size: 18_559_840,
        sha256: "7954e8bdedb497f830c6a679e818d98399b7f4d81ade1126c3e0be74d28111ab",
    },
};

/// Windows x64：微软官方 VC++ 2015-2022 可再发行运行库 14.44.35211
/// （`vc_redist.x64.exe` 内 X64 Minimum / Additional Runtime 的原始文件），
/// 按加载顺序排列：每个文件只依赖排在它之前的文件与系统 DLL
/// （KERNEL32、Win10+ 内置的 UCRT `api-ms-win-crt-*`）。
///
/// 14.44 满足 ONNX Runtime 1.24（VS 2022 17.x 构建）对 STL ≥ 14.40 的要求，
/// 系统中若只有更旧的运行库也不受影响。
const WINDOWS_X86_64_VC_RUNTIME: &[ModelFile] = &[
    ModelFile {
        path: "vcruntime140.dll",
        size: 124_544,
        sha256: "d5e4d9a3e835fa679450145d6a7d94e36573a509317111904d9b3712c30d9066",
    },
    ModelFile {
        path: "vcruntime140_1.dll",
        size: 49_792,
        sha256: "1f2d41c4aa5db0bc33ebf7b66d72943a817d7ce6cbe880502a9403823633093f",
    },
    ModelFile {
        path: "msvcp140.dll",
        size: 557_728,
        sha256: "0f885b509a685d2bbfa652fed26b5fb31d88fbdab0a978c641d1c7b8aa460aa9",
    },
    ModelFile {
        path: "msvcp140_1.dll",
        size: 35_952,
        sha256: "bfad5aef4c63a669e3c140655cdfdf395b6c979b400a447bd5dcb65ed8826c3d",
    },
];

/// 微软官方 GitHub Release `onnxruntime-win-x64-1.24.1.zip` 中的
/// `lib/onnxruntime.dll`（上游 zip sha256
/// c72736c16442cad417ee4f45e3e2fbd10778e70a0fc1cb0e86f028d766e525af）。
///
/// CPU 版：导入表不含 DirectML / d3d12；C++ 运行库由 [`WINDOWS_X86_64_VC_RUNTIME`]
/// 随附提供，其余仅依赖系统 DLL（KERNEL32 / ADVAPI32 / SETUPAPI / dbghelp / dxgi /
/// UCRT）。
pub(crate) const WINDOWS_X86_64: RuntimeLibrary = RuntimeLibrary {
    platform: "windows-x86_64",
    dependencies: WINDOWS_X86_64_VC_RUNTIME,
    file: ModelFile {
        path: "onnxruntime.dll",
        size: 14_131_232,
        sha256: "8a1aad8d59d02a5337d4e3f5bbd1158c3f7bf84fe3b3f0052f957dd3e75a91cb",
    },
};

/// 当前平台需要的动态库；静态链接平台返回 None。
pub(crate) fn current_library() -> Option<&'static RuntimeLibrary> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(&LINUX_X86_64)
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(&LINUX_AARCH64)
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(&WINDOWS_X86_64)
    } else {
        None
    }
}

/// 当前平台是否运行时加载 ONNX Runtime 动态库（否则为编译期静态链接）。
const fn uses_dynamic_runtime() -> bool {
    cfg!(any(target_os = "linux", target_os = "windows"))
}

/// 当前平台是否支持内置本地推理。
///
/// 静态链接平台恒为 true；Linux 仅 x86_64 / aarch64、Windows 仅 x86_64 有官方动态库。
pub(crate) fn platform_supported() -> bool {
    !uses_dynamic_runtime() || current_library().is_some()
}

/// 运行库文件在 OSS 上相对下载源根的路径：`onnxruntime/<version>/<platform>/<file>`。
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub(crate) fn file_url_path(library: &RuntimeLibrary, file: &ModelFile) -> String {
    format!(
        "onnxruntime/{ORT_VERSION}/{}/{}",
        library.platform, file.path
    )
}

/// 动态库本地目录：`<models>/onnxruntime/<version>/<platform>/`。
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub(crate) fn library_dir(root: &Path, library: &RuntimeLibrary) -> PathBuf {
    root.join("onnxruntime")
        .join(ORT_VERSION)
        .join(library.platform)
}

/// 确保 ONNX Runtime 可用：Linux / Windows 上下载（如需）并加载动态库；其他平台为空操作。
///
/// 进程内只加载一次；加载失败（如 glibc 过旧、文件被安全软件拦截、架构不符）会返回
/// 可读错误，调用方据此把本地模型标记为不可用并降级召回。
pub(crate) async fn ensure_loaded(cancel: &super::download::CancelToken) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        dynamic::ensure_loaded(cancel).await
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = cancel;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod dynamic {
    use std::path::Path;
    use std::sync::OnceLock;

    use anyhow::{Context, Result, anyhow, bail};

    use super::super::download::{self, CancelToken, DownloadError};
    use super::{RuntimeLibrary, current_library, library_dir};

    /// 加载结果（成功后永久复用；失败保存原因，避免每次重试都重新加载）。
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
                "当前平台（{}-{}）没有可用的 ONNX Runtime，内置本地模型不可用",
                std::env::consts::OS,
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
        // 加载与版本检查：失败多为系统不兼容或文件被拦截，重试无意义，记为永久结果。
        let result = tokio::task::spawn_blocking(move || {
            preload_dependencies(&dir, library)?;
            ort::init_from(&path)
                .map(|builder| {
                    builder.commit();
                })
                .map_err(|error| {
                    format!("加载 ONNX Runtime 运行库失败：{error}{}", access_hint(&dir))
                })
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

    /// 按顺序以绝对路径预加载随附的 C++ 运行库，并在进程生命周期内保持加载。
    ///
    /// `LOAD_WITH_ALTERED_SEARCH_PATH` 让每个运行库的导入优先在其自身目录解析；
    /// 随后加载 `onnxruntime.dll` 时，加载器按模块名复用这些已加载的副本，
    /// 不会再去 System32 查找。
    #[cfg(target_os = "windows")]
    fn preload_dependencies(dir: &Path, library: &RuntimeLibrary) -> Result<(), String> {
        use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library};

        for file in library.dependencies {
            let path = dir.join(file.local_name());
            // SAFETY: 加载的是按清单校验过 sha256 的微软官方 VC++ 运行库，其
            // DllMain 仅做 CRT 初始化，不依赖调用方状态。
            let loaded = unsafe { Library::load_with_flags(&path, LOAD_WITH_ALTERED_SEARCH_PATH) };
            match loaded {
                // 与 ONNX Runtime 同生命周期：钉住模块，进程内永不卸载。
                Ok(module) => {
                    if let Err(error) = module.pin() {
                        tracing::debug!(file = file.path, %error, "钉住 C++ 运行库失败，改为泄漏句柄");
                    }
                    std::mem::forget(module);
                }
                Err(error) => {
                    return Err(format!(
                        "加载 C++ 运行库 {} 失败：{error}{}",
                        file.path,
                        access_hint(dir)
                    ));
                }
            }
        }
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn preload_dependencies(_dir: &Path, library: &RuntimeLibrary) -> Result<(), String> {
        debug_assert!(library.dependencies.is_empty());
        Ok(())
    }

    /// 加载失败时的排障提示：Windows 上最常见的是安全软件拦截了运行库目录。
    fn access_hint(dir: &Path) -> String {
        if cfg!(target_os = "windows") {
            format!(
                "；运行库位于 {}，若提示拒绝访问（os error 5），请在安全软件中放行该目录",
                dir.display()
            )
        } else {
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_paths_follow_oss_layout() {
        assert_eq!(
            file_url_path(&LINUX_X86_64, &LINUX_X86_64.file),
            "onnxruntime/1.24.1/linux-x86_64/libonnxruntime.so.1.24.1"
        );
        assert_eq!(
            file_url_path(&LINUX_AARCH64, &LINUX_AARCH64.file),
            "onnxruntime/1.24.1/linux-aarch64/libonnxruntime.so.1.24.1"
        );
        let windows: Vec<String> = WINDOWS_X86_64
            .files()
            .map(|file| file_url_path(&WINDOWS_X86_64, file))
            .collect();
        assert_eq!(
            windows,
            [
                "onnxruntime/1.24.1/windows-x86_64/vcruntime140.dll",
                "onnxruntime/1.24.1/windows-x86_64/vcruntime140_1.dll",
                "onnxruntime/1.24.1/windows-x86_64/msvcp140.dll",
                "onnxruntime/1.24.1/windows-x86_64/msvcp140_1.dll",
                "onnxruntime/1.24.1/windows-x86_64/onnxruntime.dll",
            ]
        );
        let root = Path::new("/data/models");
        assert_eq!(
            library_dir(root, &LINUX_AARCH64),
            Path::new("/data/models/onnxruntime/1.24.1/linux-aarch64")
        );
        assert_eq!(
            library_dir(root, &WINDOWS_X86_64),
            Path::new("/data/models/onnxruntime/1.24.1/windows-x86_64")
        );
    }

    #[test]
    fn library_entries_are_well_formed() {
        for library in [LINUX_X86_64, LINUX_AARCH64, WINDOWS_X86_64] {
            for file in library.files() {
                assert_eq!(file.sha256.len(), 64, "{}", file.path);
                assert!(file.sha256.chars().all(|c| c.is_ascii_hexdigit()));
                assert!(file.size > 0);
                assert_eq!(file.local_name(), file.path, "运行库文件不应有子目录");
            }
            assert!(library.file.size > 10_000_000);
        }
        for library in [LINUX_X86_64, LINUX_AARCH64] {
            assert!(library.dependencies.is_empty());
            assert!(library.file.path.ends_with(ORT_VERSION));
        }
        // Windows：C++ 运行库必须全部随附，且 vcruntime140 排在最前（其余都依赖它）。
        let names: Vec<&str> = WINDOWS_X86_64
            .dependencies
            .iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(
            names,
            [
                "vcruntime140.dll",
                "vcruntime140_1.dll",
                "msvcp140.dll",
                "msvcp140_1.dll"
            ]
        );
        assert_eq!(WINDOWS_X86_64.total_size(), 14_899_248);
    }

    #[test]
    fn platform_support_matches_target() {
        if cfg!(target_os = "linux") {
            assert_eq!(
                platform_supported(),
                matches!(std::env::consts::ARCH, "x86_64" | "aarch64")
            );
        } else if cfg!(target_os = "windows") {
            assert_eq!(platform_supported(), std::env::consts::ARCH == "x86_64");
        } else {
            assert!(platform_supported());
            assert!(current_library().is_none());
        }
    }
}
