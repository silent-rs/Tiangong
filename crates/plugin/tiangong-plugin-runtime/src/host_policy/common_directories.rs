//! 对所有沙箱 sidecar 开放读写的通用目录清单（宿主产品策略）。
//!
//! 包管理器、版本管理器与各类工具链的缓存是功能基础设施而非用户数据：npx、uvx、
//! cargo、pnpm 等要写这些目录才能工作。清单对全部插件一视同仁，不按插件区分；
//! 插件私有的数据目录（`data/<id>`）不在此列。
//!
//! # 新增目录
//!
//! 在 [`COMMON_DIRECTORIES`] 里加一行，按工具填三类位置，并同步更新本文件末尾的单测。
//! 位置都是相对基准目录的路径：
//!
//! | 字段 | macOS | Linux | Windows |
//! | --- | --- | --- | --- |
//! | `home` | `~` | `~` | `%USERPROFILE%` |
//! | `cache` | `~/Library/Caches` | `$XDG_CACHE_HOME`，缺省 `~/.cache` | `%LOCALAPPDATA%` |
//! | `roaming` | 不使用 | 不使用 | `%APPDATA%` |
//!
//! 同一工具的多个位置可以一并列出：不存在的目录在 [`common_directory_roots`] 里丢弃，
//! 所以各平台的路径不必用条件编译分开。
//!
//! # 边界
//!
//! - 只列工具自己的目录。装有凭据或令牌的目录不列（`~/.ssh`、`~/.aws`、`~/.docker`、
//!   `~/.claude`、`~/.codex`、`~/.gemini`、`~/.config` 整体），`%LOCALAPPDATA%`、
//!   `%APPDATA%`、`~/Library/Caches`、`~/.cache`、`~/.local/share` 这些整体目录也不列。
//! - 混有凭据的目录只开放不含凭据的子目录（如 `~/.m2/repository`、`~/.gradle/caches`）；
//!   必须整目录开放时，把凭据文件登记到 [`COMMON_DIRECTORY_DENIED_FILES`]。
//! - 只认各工具的默认位置，不跟随 `CARGO_HOME`、`PNPM_HOME`、`UV_CACHE_DIR` 这类环境
//!   变量覆盖。
//! - 用户临时需要的其他目录不必改这里：设置页沙箱策略里的目录白名单
//!   （`directory_allowlist`）会对所有 sidecar 追加读写，即时生效。
//!
//! # 安全取舍
//!
//! 读写意味着沙箱内进程可以改写这些目录里的程序（如 `~/.cargo/bin`、`~/.local/bin`
//! 下的工具、nvm 管理的 node），之后会在沙箱外被直接执行。当前按「对所有插件读写」
//! 处理，是有意的取舍。

use std::path::{Path, PathBuf};

/// 一个工具的通用目录在各平台基准目录下的相对路径。
struct CommonDirectory {
    /// 工具名，仅用于对照与排查；非测试构建不读取。
    #[allow(dead_code)]
    tool: &'static str,
    /// 用户家目录下的相对路径。
    home: &'static [&'static str],
    /// 缓存目录下的相对路径（见模块说明的基准目录表）。
    cache: &'static [&'static str],
    /// Windows 漫游数据目录下的相对路径。
    roaming: &'static [&'static str],
}

/// 通用目录清单。按生态分组，便于对照；新增时放进对应的组。
const COMMON_DIRECTORIES: &[CommonDirectory] = &[
    // Node 生态
    CommonDirectory {
        tool: "asdf",
        home: &[".asdf"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "bun",
        home: &[".bun"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "deno",
        home: &[".deno"],
        cache: &["deno"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "fnm",
        home: &[".fnm"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "mise",
        home: &[".local/share/mise"],
        cache: &["mise"],
        roaming: &[],
    },
    // npm 的全局包与 bin 在 Windows 上位于漫游数据目录；npx 缓存在 `~/.npm/_npx`。
    CommonDirectory {
        tool: "npm",
        home: &[".npm"],
        cache: &[],
        roaming: &["npm"],
    },
    // nvm-windows 的版本目录在漫游数据目录。
    CommonDirectory {
        tool: "nvm",
        home: &[".nvm"],
        cache: &[],
        roaming: &["nvm"],
    },
    CommonDirectory {
        tool: "node-gyp",
        home: &[".node-gyp"],
        cache: &["node-gyp"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "pnpm",
        home: &[".local/share/pnpm", ".pnpm-store", "Library/pnpm"],
        cache: &["pnpm"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "volta",
        home: &[".volta"],
        cache: &[],
        roaming: &[],
    },
    // Yarn 1 的缓存在缓存目录；Yarn 2+ 的缓存在 `~/.yarn/berry/cache`。
    CommonDirectory {
        tool: "yarn",
        home: &[".yarn"],
        cache: &["yarn"],
        roaming: &[],
    },
    // Python 生态
    CommonDirectory {
        tool: "conda",
        home: &[".conda"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "pip",
        home: &[],
        cache: &["pip"],
        roaming: &[],
    },
    // pipx 1.5 起用 XDG 数据目录，更早版本在 `~/.local/pipx`；Windows 在 `%LOCALAPPDATA%\pipx`。
    CommonDirectory {
        tool: "pipenv",
        home: &[".local/share/virtualenvs", ".virtualenvs"],
        cache: &["pipenv"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "pipx",
        home: &[".local/pipx", ".local/share/pipx"],
        cache: &["pipx"],
        roaming: &[],
    },
    // Poetry 的缓存与虚拟环境在缓存目录；Windows 上旧版在漫游数据目录。
    CommonDirectory {
        tool: "poetry",
        home: &[],
        cache: &["pypoetry"],
        roaming: &["pypoetry"],
    },
    CommonDirectory {
        tool: "pyenv",
        home: &[".pyenv"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "rye",
        home: &[".rye"],
        cache: &[],
        roaming: &[],
    },
    // uv 的缓存在缓存目录，Python 安装与工具在 XDG 数据目录；Windows 在漫游数据目录。
    CommonDirectory {
        tool: "uv",
        home: &[".local/share/uv"],
        cache: &["uv"],
        roaming: &["uv"],
    },
    // Rust 与 Go
    CommonDirectory {
        tool: "cargo",
        home: &[".cargo"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "go",
        home: &["go"],
        cache: &["go-build"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "rustup",
        home: &[".rustup"],
        cache: &[],
        roaming: &[],
    },
    // Java、Ruby、PHP、.NET
    CommonDirectory {
        tool: "bundle",
        home: &[".bundle"],
        cache: &[],
        roaming: &[],
    },
    // Composer 只开放缓存：配置目录里有 `auth.json`。
    CommonDirectory {
        tool: "composer",
        home: &[".composer/cache"],
        cache: &["composer"],
        roaming: &["Composer/cache"],
    },
    // 只开放全局包目录：`~/.nuget` 与 `%APPDATA%\NuGet` 的配置里可能有小源令牌。
    CommonDirectory {
        tool: "dotnet",
        home: &[".dotnet", ".nuget/packages"],
        cache: &[],
        roaming: &[],
    },
    // 只开放缓存与 wrapper：`~/.gradle/gradle.properties` 里可能有仓库凭据。
    CommonDirectory {
        tool: "gradle",
        home: &[".gradle/caches", ".gradle/wrapper"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "gem",
        home: &[".gem"],
        cache: &[],
        roaming: &[],
    },
    // 只开放本地仓库：`~/.m2/settings.xml` 里可能有仓库凭据。
    CommonDirectory {
        tool: "maven",
        home: &[".m2/repository"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "rbenv",
        home: &[".rbenv"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "rvm",
        home: &[".rvm"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "sdkman",
        home: &[".sdkman"],
        cache: &[],
        roaming: &[],
    },
    // 移动端与 Apple 平台
    CommonDirectory {
        tool: "android",
        home: &["Android/Sdk", ".android"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "carthage",
        home: &[],
        cache: &["org.carthage.CarthageKit"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "cocoapods",
        home: &[".cocoapods"],
        cache: &["CocoaPods"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "homebrew",
        home: &[],
        cache: &["Homebrew"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "swiftpm",
        home: &[".swiftpm"],
        cache: &["org.swift.swiftpm"],
        roaming: &[],
    },
    // 浏览器运行时与模型缓存
    CommonDirectory {
        tool: "electron",
        home: &[],
        cache: &["electron"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "huggingface",
        home: &[".cache/huggingface"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "ollama",
        home: &[".ollama"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "playwright",
        home: &[],
        cache: &["ms-playwright"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "puppeteer",
        home: &[],
        cache: &["puppeteer"],
        roaming: &[],
    },
    CommonDirectory {
        tool: "pytorch",
        home: &[".cache/torch"],
        cache: &[],
        roaming: &[],
    },
    // 编辑器与其他
    // `uv tool install`、`pipx install`、`cargo install` 之后命令行要能直接调用。
    CommonDirectory {
        tool: "local-bin",
        home: &[".local/bin"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "jetbrains",
        home: &[],
        cache: &["JetBrains"],
        roaming: &[],
    },
    // Scoop 的应用、缓存与持久数据都在它的根目录下。
    CommonDirectory {
        tool: "scoop",
        home: &["scoop", ".scoop"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "terraform",
        home: &[".terraform.d"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "vagrant",
        home: &[".vagrant.d"],
        cache: &[],
        roaming: &[],
    },
    CommonDirectory {
        tool: "vscode",
        home: &[".vscode", ".vscode-server"],
        cache: &[],
        roaming: &[],
    },
];

/// 整目录开放后必须单独禁读的凭据文件，相对用户家目录。
const COMMON_DIRECTORY_DENIED_FILES: &[&str] = &[
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".gem/credentials",
];

/// 解析通用目录所用的基准目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonDirectoryBases {
    /// 用户家目录。
    pub home: Option<PathBuf>,
    /// 缓存目录：macOS `~/Library/Caches`，Linux `$XDG_CACHE_HOME`（缺省 `~/.cache`），
    /// Windows `%LOCALAPPDATA%`。
    pub cache: Option<PathBuf>,
    /// Windows 漫游数据目录 `%APPDATA%`；其他平台为 None。
    pub roaming: Option<PathBuf>,
}

impl CommonDirectoryBases {
    /// 按当前平台与环境变量推导基准目录；家目录未知时三项都为 None。
    pub fn detect(home: Option<&Path>) -> Self {
        let home = home.map(Path::to_path_buf);
        let cache = home.as_deref().and_then(|home| {
            if cfg!(target_os = "macos") {
                Some(home.join("Library/Caches"))
            } else if cfg!(target_os = "windows") {
                absolute_env_dir("LOCALAPPDATA").or_else(|| Some(home.join("AppData/Local")))
            } else {
                absolute_env_dir("XDG_CACHE_HOME").or_else(|| Some(home.join(".cache")))
            }
        });
        let roaming = home.as_deref().and_then(|home| {
            if cfg!(target_os = "windows") {
                absolute_env_dir("APPDATA").or_else(|| Some(home.join("AppData/Roaming")))
            } else {
                None
            }
        });
        Self {
            home,
            cache,
            roaming,
        }
    }
}

/// 读取一个指向绝对路径的环境变量；未设置或非绝对路径时为 None。
fn absolute_env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// 把一组相对路径挂到基准目录下。清单只允许相对路径：绝对路径或带跳级会指向
/// 基准目录之外，直接丢弃；基准目录缺失时整组为空。
fn expand(base: Option<&Path>, relatives: &[&'static str]) -> impl Iterator<Item = PathBuf> {
    let base = base.map(Path::to_path_buf);
    relatives.iter().filter_map(move |relative| {
        let relative_path = Path::new(*relative);
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return None;
        }
        base.clone().map(|base| base.join(relative_path))
    })
}

/// 按基准目录展开清单，得到全部候选目录（含不存在的路径），去重并保持清单顺序。
pub fn common_directory_candidates(bases: &CommonDirectoryBases) -> Vec<PathBuf> {
    let mut seen = std::collections::BTreeSet::new();
    COMMON_DIRECTORIES
        .iter()
        .flat_map(|entry| {
            expand(bases.home.as_deref(), entry.home)
                .chain(expand(bases.cache.as_deref(), entry.cache))
                .chain(expand(bases.roaming.as_deref(), entry.roaming))
        })
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

/// 通用目录中实际存在的目录，按当前平台与环境变量推导基准位置。
///
/// 只返回已存在的目录：清单很长，而任何一台机器只会装其中一部分，不存在的条目
/// 不应进入策略（否则每次启动都会产生「忽略不存在的可写根」告警）。
pub fn common_directory_roots(home: Option<&Path>) -> Vec<PathBuf> {
    common_directory_candidates(&CommonDirectoryBases::detect(home))
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

/// [`common_directory_roots`] 开放的目录里需要单独禁读的凭据文件，只返回已存在的。
pub fn common_directory_denied_files(home: Option<&Path>) -> Vec<PathBuf> {
    let Some(home) = home else {
        return Vec::new();
    };
    COMMON_DIRECTORY_DENIED_FILES
        .iter()
        .map(|relative| home.join(relative))
        .filter(|path| path.exists())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bases() -> CommonDirectoryBases {
        CommonDirectoryBases {
            home: Some(PathBuf::from("/home")),
            cache: Some(PathBuf::from("/cache")),
            roaming: Some(PathBuf::from("/roaming")),
        }
    }

    fn candidates(tool: &str) -> Vec<PathBuf> {
        let entry = COMMON_DIRECTORIES
            .iter()
            .find(|entry| entry.tool == tool)
            .unwrap_or_else(|| panic!("清单缺少 {tool}"));
        let expand = |base: &Path, relatives: &[&str]| -> Vec<PathBuf> {
            relatives.iter().map(|r| base.join(r)).collect()
        };
        let bases = bases();
        [
            expand(bases.home.as_deref().unwrap(), entry.home),
            expand(bases.cache.as_deref().unwrap(), entry.cache),
            expand(bases.roaming.as_deref().unwrap(), entry.roaming),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    #[test]
    fn tools_cover_each_platform_location() {
        // 抽查各平台位置不同的工具，确认三类基准目录都被用到。
        assert_eq!(
            candidates("npm"),
            vec![PathBuf::from("/home/.npm"), PathBuf::from("/roaming/npm")]
        );
        assert_eq!(candidates("pip"), vec![PathBuf::from("/cache/pip")]);
        assert_eq!(
            candidates("uv"),
            vec![
                PathBuf::from("/home/.local/share/uv"),
                PathBuf::from("/cache/uv"),
                PathBuf::from("/roaming/uv"),
            ]
        );
        assert_eq!(
            candidates("pnpm"),
            vec![
                PathBuf::from("/home/.local/share/pnpm"),
                PathBuf::from("/home/.pnpm-store"),
                PathBuf::from("/home/Library/pnpm"),
                PathBuf::from("/cache/pnpm"),
            ]
        );
        assert_eq!(
            candidates("pipx"),
            vec![
                PathBuf::from("/home/.local/pipx"),
                PathBuf::from("/home/.local/share/pipx"),
                PathBuf::from("/cache/pipx"),
            ]
        );
    }

    #[test]
    fn sensitive_directories_stay_out_of_the_list() {
        // 装有凭据或令牌的目录与各平台的整体目录不得出现。
        let opened = common_directory_candidates(&bases());
        let names = |path: &Path| -> Vec<String> {
            path.iter()
                .map(|c| c.to_string_lossy().to_string())
                .collect()
        };
        for forbidden in [
            ".ssh", ".aws", ".azure", ".kube", ".docker", ".config", ".claude", ".codex",
            ".gemini", ".netrc",
        ] {
            assert!(
                !opened
                    .iter()
                    .any(|path| names(path).contains(&forbidden.to_string())),
                "{forbidden} 不应开放"
            );
        }
        // 整体目录只以具体子目录出现。
        for (whole, allowed) in [
            (".m2", vec!["/home/.m2/repository"]),
            (
                ".gradle",
                vec!["/home/.gradle/caches", "/home/.gradle/wrapper"],
            ),
            (".nuget", vec!["/home/.nuget/packages"]),
        ] {
            let matching: Vec<&PathBuf> = opened
                .iter()
                .filter(|path| names(path).contains(&whole.to_string()))
                .collect();
            let expected: Vec<PathBuf> = allowed.iter().map(PathBuf::from).collect();
            assert_eq!(matching, expected.iter().collect::<Vec<_>>(), "{whole}");
        }
    }

    #[test]
    fn entries_are_unique_and_use_relative_paths_only() {
        let mut tools = std::collections::BTreeSet::new();
        for entry in COMMON_DIRECTORIES {
            assert!(tools.insert(entry.tool), "工具名重复：{}", entry.tool);
            assert!(
                !entry.home.is_empty() || !entry.cache.is_empty() || !entry.roaming.is_empty(),
                "{} 没有任何位置",
                entry.tool
            );
            for relative in entry.home.iter().chain(entry.cache).chain(entry.roaming) {
                let path = Path::new(relative);
                assert!(path.is_relative(), "{relative}");
                assert!(!relative.contains(".."), "{relative}");
            }
        }
        // 清单里没有任何条目指向整体缓存目录本身。
        assert!(
            !common_directory_candidates(&bases())
                .iter()
                .any(|path| path == Path::new("/cache") || path == Path::new("/roaming"))
        );
    }

    #[test]
    fn denied_files_are_restricted_to_opened_directories() {
        // 凭据文件必须落在清单开放的某个目录之内，否则这条禁读没有意义。
        let opened = common_directory_candidates(&bases());
        for relative in COMMON_DIRECTORY_DENIED_FILES {
            let path = Path::new("/home").join(relative);
            assert!(
                opened.iter().any(|root| path.starts_with(root)),
                "{relative} 不在开放目录内"
            );
        }
    }

    #[test]
    fn missing_bases_yield_no_candidates() {
        assert!(
            common_directory_candidates(&CommonDirectoryBases {
                home: None,
                cache: None,
                roaming: None,
            })
            .is_empty()
        );
        // 缺少缓存与漫游基准时，只用家目录展开。
        let home_only = common_directory_candidates(&CommonDirectoryBases {
            home: Some(PathBuf::from("/home")),
            cache: None,
            roaming: None,
        });
        assert!(home_only.iter().all(|path| path.starts_with("/home")));
    }
}
