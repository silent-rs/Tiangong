//! 解释器（node/python）运行环境发现、缓存与入口注入。
//!
//! GUI 进程（launchd/Finder 启动）不执行 shell 初始化，nvm/Homebrew 等
//! 安装位置不在其 PATH 中。本模块的发现分两层：
//!
//! - **应用级缓存**：每种解释器正常只探测一次，后续使用前仅做
//!   `is_file` 轻量校验；程序被删除等无法创建进程的情形经
//!   [`invalidate_if_matches`] 失效后重新发现一次。
//! - **平台探测策略**：Windows 只检查显式路径、PATH 与环境变量可直接
//!   构造的固定路径（不枚举版本目录）；macOS/Linux 保留版本管理器
//!   目录探测，仅在缓存未命中时作为慢路径执行。
//!
//! 探测结果只由传入的 [`InterpreterEnv`] 快照和文件系统状态决定：
//! 某个来源缺失就跳过继续尝试后续层次，全部未命中才判定解释器不可用。
//! 进程入口经 [`ensure_interpreter_env`] 把发现结果注入
//! `TIANGONG_*_PATH` 与 PATH，供插件 sidecar 与命令通道整棵子进程树使用。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

use crate::manifest::SidecarRuntime;

/// 解释器种类（sidecar 清单声明的 runtime 去掉 native）。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InterpreterKind {
    Node,
    Python,
}

impl InterpreterKind {
    fn env_key(self) -> &'static str {
        match self {
            InterpreterKind::Node => "TIANGONG_NODE_PATH",
            InterpreterKind::Python => "TIANGONG_PYTHON_PATH",
        }
    }

    fn install_hint(self) -> &'static str {
        match self {
            InterpreterKind::Node => "帮我安装 Node.js",
            InterpreterKind::Python => "帮我安装 Python",
        }
    }

    fn program_names(self) -> &'static [&'static str] {
        match self {
            InterpreterKind::Node => {
                if cfg!(windows) {
                    &["node.exe"]
                } else {
                    &["node"]
                }
            }
            InterpreterKind::Python => {
                if cfg!(windows) {
                    &["python.exe", "python3.exe", "py.exe"]
                } else {
                    &["python3", "python"]
                }
            }
        }
    }
}

/// 解释器的发现来源；用户显式指定与应用自动注入必须可区分。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterpreterSource {
    /// 用户经 `TIANGONG_*_PATH` 显式指定：路径无效时直接报错，不回退。
    ExplicitOverride,
    /// PATH 中直接命中。
    Path,
    /// 安装工具的环境变量直接构造出的固定路径（Windows 策略）。
    Environment,
    /// 版本管理器目录枚举或系统标准位置（macOS/Linux 慢路径）。
    CommonLocation,
}

#[derive(Clone, Debug)]
struct CachedInterpreter {
    path: PathBuf,
    source: InterpreterSource,
}

type InterpreterCache = HashMap<InterpreterKind, CachedInterpreter>;

/// 应用级解释器缓存：只保存成功发现的程序，不缓存"未找到"（用户在
/// 应用运行期间安装解释器后应能被发现）。
fn global_cache() -> &'static RwLock<InterpreterCache> {
    static CACHE: OnceLock<RwLock<InterpreterCache>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 记录 `TIANGONG_*_PATH` 当前值是否为应用注入（而非用户显式设置）。
/// 该状态独立于缓存存活：缓存失效重建后残留的环境变量值仍能正确
/// 归类，不会被误认为用户显式配置。
fn mark_env_injected(kind: InterpreterKind) {
    if let Ok(mut guard) = injected_flags().write() {
        guard.insert(kind, true);
    }
}

fn env_injected_by_app(kind: InterpreterKind) -> bool {
    injected_flags()
        .read()
        .ok()
        .and_then(|guard| guard.get(&kind).copied())
        .unwrap_or(false)
}

fn injected_flags() -> &'static RwLock<HashMap<InterpreterKind, bool>> {
    static INJECTED: OnceLock<RwLock<HashMap<InterpreterKind, bool>>> = OnceLock::new();
    INJECTED.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 每种解释器一把探测锁：同一种解释器并发首次解析只探测一次，Node
/// 与 Python 互不阻塞。
fn probe_lock(kind: InterpreterKind) -> &'static Mutex<()> {
    static NODE: Mutex<()> = Mutex::new(());
    static PYTHON: Mutex<()> = Mutex::new(());
    match kind {
        InterpreterKind::Node => &NODE,
        InterpreterKind::Python => &PYTHON,
    }
}

/// 跨平台获取用户 home 目录（与 sidecar 框架 `endpoint::home_dir` 同源）。
pub(crate) fn user_home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(profile));
    }
    let drive = std::env::var_os("HOMEDRIVE").filter(|v| !v.is_empty());
    let path = std::env::var_os("HOMEPATH").filter(|v| !v.is_empty());
    match (drive, path) {
        (Some(drive), Some(path)) => {
            let mut buf = PathBuf::from(drive);
            buf.push(path);
            Some(buf)
        }
        _ => None,
    }
}

/// 解释器发现所需的环境快照：生产路径经 [`InterpreterEnv::from_process`]
/// 从进程环境读取一次，测试注入伪造值，不修改真实环境变量。
#[derive(Default)]
pub(crate) struct InterpreterEnv {
    /// `PATH` 拆分后的搜索目录。
    search_paths: Vec<PathBuf>,
    home: Option<PathBuf>,
    /// 各安装工具声明的自定义根目录（未设置时按各工具默认位置推导）。
    nvm_dir: Option<PathBuf>,
    nvm_symlink: Option<PathBuf>,
    volta_home: Option<PathBuf>,
    scoop: Option<PathBuf>,
    chocolatey_install: Option<PathBuf>,
    pyenv_root: Option<PathBuf>,
    asdf_data_dir: Option<PathBuf>,
    program_files: Option<PathBuf>,
    program_data: Option<PathBuf>,
    /// 各平台的解释器系统标准安装位置（Homebrew、系统包管理等）。
    node_system_locations: Vec<PathBuf>,
    python_system_locations: Vec<PathBuf>,
}

impl InterpreterEnv {
    pub(crate) fn from_process() -> Self {
        let dir = |key: &str| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            search_paths: std::env::var_os("PATH")
                .map(|value| std::env::split_paths(&value).collect())
                .unwrap_or_default(),
            home: user_home_dir(),
            nvm_dir: dir("NVM_DIR"),
            nvm_symlink: dir("NVM_SYMLINK"),
            volta_home: dir("VOLTA_HOME"),
            scoop: dir("SCOOP"),
            chocolatey_install: dir("ChocolateyInstall"),
            pyenv_root: dir("PYENV_ROOT"),
            asdf_data_dir: dir("ASDF_DATA_DIR"),
            program_files: dir("ProgramFiles"),
            program_data: dir("ProgramData"),
            node_system_locations: default_node_system_locations(),
            python_system_locations: default_python_system_locations(),
        }
    }
}

/// 显式环境变量存在时直接使用；仅显式变量缺失时经基准目录推导默认
/// 安装根（`base/<default_suffix>`）。
fn explicit_or_default_root(
    explicit: Option<&PathBuf>,
    base: Option<&Path>,
    default_suffix: &str,
) -> Option<PathBuf> {
    explicit
        .cloned()
        .or_else(|| base.map(|base| base.join(default_suffix)))
}

/// 统一的解释器解析入口：缓存命中（`is_file` 校验通过）直接返回，
/// 失效或缺失时重新探测并写回缓存。registry 与入口注入共用本入口，
/// 不各自独立探测。
pub(crate) fn resolve_interpreter(kind: InterpreterKind) -> anyhow::Result<PathBuf> {
    let explicit = std::env::var_os(kind.env_key())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    resolve_locked(
        kind,
        explicit,
        env_injected_by_app(kind),
        global_cache(),
        probe_lock(kind),
        || probe_interpreter(kind, &InterpreterEnv::from_process(), None),
    )
}

/// 快路径直接读缓存；未命中取该解释器的探测锁后双检再解析。
fn resolve_locked<F>(
    kind: InterpreterKind,
    explicit: Option<PathBuf>,
    injected_by_app: bool,
    cache: &RwLock<InterpreterCache>,
    lock: &Mutex<()>,
    probe: F,
) -> anyhow::Result<PathBuf>
where
    F: FnOnce() -> Option<CachedInterpreter>,
{
    if let Some(path) = valid_cached_in(cache, kind) {
        return Ok(path);
    }
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    resolve_via(kind, explicit, injected_by_app, cache, probe)
}

fn valid_cached_in(cache: &RwLock<InterpreterCache>, kind: InterpreterKind) -> Option<PathBuf> {
    cache
        .read()
        .ok()?
        .get(&kind)
        .filter(|cached| cached.path.is_file())
        .map(|cached| cached.path.clone())
}

/// [`resolve_interpreter`] 的参数化核心，测试注入独立缓存、显式值、
/// 注入标记与探测闭包（兼作探测次数计数），不触碰全局状态。
fn resolve_via<F>(
    kind: InterpreterKind,
    explicit: Option<PathBuf>,
    injected_by_app: bool,
    cache: &RwLock<InterpreterCache>,
    probe: F,
) -> anyhow::Result<PathBuf>
where
    F: FnOnce() -> Option<CachedInterpreter>,
{
    {
        let mut guard = cache
            .write()
            .map_err(|_| anyhow::anyhow!("解释器缓存锁已损坏"))?;
        match guard.get(&kind) {
            Some(cached) if cached.path.is_file() => return Ok(cached.path.clone()),
            Some(_) => {
                guard.remove(&kind);
            }
            None => {}
        }
    }
    // 环境变量为应用注入时（值可能已失效）直接忽略并重新探测；仅用户
    // 显式设置才按显式语义处理（无效即报错，不回退）。经 as_os_str 显式
    // 调用 OsStr::is_empty：Path::is_empty 是 unstable，nightly 工具链
    // 下方法解析会命中它并要求 feature gate。
    if let Some(explicit) = explicit.filter(|value| !value.as_os_str().is_empty())
        && !injected_by_app
    {
        if explicit.is_file() {
            if let Ok(mut guard) = cache.write() {
                guard.insert(
                    kind,
                    CachedInterpreter {
                        path: explicit.clone(),
                        source: InterpreterSource::ExplicitOverride,
                    },
                );
            }
            return Ok(explicit);
        }
        anyhow::bail!(
            "{} 指向的程序不存在: {}",
            kind.env_key(),
            explicit.display()
        );
    }
    match probe() {
        Some(discovered) => {
            tracing::debug!(
                kind = ?kind,
                source = ?discovered.source,
                path = %discovered.path.display(),
                "解释器发现结果已缓存"
            );
            if let Ok(mut guard) = cache.write() {
                guard.insert(kind, discovered.clone());
            }
            Ok(discovered.path)
        }
        None => anyhow::bail!(
            "未找到 {:?} sidecar 所需的解释器程序（{}）；可在会话中对助手说「{}」快速安装，或以 {} 指定路径",
            kind,
            kind.program_names().join(" / "),
            kind.install_hint(),
            kind.env_key()
        ),
    }
}

/// 解释器程序本身无法创建进程（文件被删、无执行权限、格式无效等）时
/// 失效缓存；仅当缓存当前值仍等于失败路径时清除，避免并发任务误删
/// 其他线程刚更新的新缓存。插件脚本错误、握手失败等不在本路径。
pub(crate) fn invalidate_if_matches(kind: InterpreterKind, failed_path: &Path) -> bool {
    invalidate_if_matches_in(global_cache(), kind, failed_path)
}

fn invalidate_if_matches_in(
    cache: &RwLock<InterpreterCache>,
    kind: InterpreterKind,
    failed_path: &Path,
) -> bool {
    let Ok(mut guard) = cache.write() else {
        return false;
    };
    match guard.get(&kind) {
        Some(cached) if cached.path == failed_path => {
            guard.remove(&kind);
            true
        }
        _ => false,
    }
}

/// 解释器进程创建失败后的原子恢复接口（探测锁内串行执行）：缓存仍
/// 为失败路径则失效并排除它重新探测 → 写入新缓存 → 返回新路径；若
/// 并发调用已完成恢复（缓存为其他有效路径）则直接复用该路径。运行期
/// 不修改宿主进程环境——缓存是运行期权威来源，新环境经
/// [`child_env_overrides`] 只注入到之后新建的子进程。
///
/// 返回 None 的情形：用户显式指定的路径无法启动（不静默替换）、排除
/// 失败路径后无其他可用解释器。
pub(crate) fn recover_interpreter_after_spawn_failure(
    kind: InterpreterKind,
    failed_path: &Path,
) -> Option<PathBuf> {
    recover_via(
        kind,
        failed_path,
        std::env::var_os(kind.env_key())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .as_deref(),
        env_injected_by_app(kind),
        global_cache(),
        probe_lock(kind),
        || probe_interpreter(kind, &InterpreterEnv::from_process(), Some(failed_path)),
    )
}

/// [`recover_interpreter_after_spawn_failure`] 的参数化核心，测试注入
/// 独立缓存、显式值与排除探测闭包，不触碰全局状态。
fn recover_via<F>(
    kind: InterpreterKind,
    failed_path: &Path,
    explicit: Option<&Path>,
    injected_by_app: bool,
    cache: &RwLock<InterpreterCache>,
    lock: &Mutex<()>,
    probe_excluding_failed: F,
) -> Option<PathBuf>
where
    F: FnOnce() -> Option<CachedInterpreter>,
{
    // 先取探测锁再判断缓存：并发恢复完全串行化——后来者要么复用已
    // 恢复的新路径，要么在锁内执行探测，不会因缓存暂时为空而放弃。
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = valid_cached_in(cache, kind)
        && cached != failed_path
    {
        return Some(cached);
    }
    invalidate_if_matches_in(cache, kind, failed_path);
    // 用户显式指定的路径无法启动：直接失败，不静默替换为其他解释器
    if explicit.is_some() && !injected_by_app {
        return None;
    }
    let discovered = probe_excluding_failed()?;
    let path = discovered.path.clone();
    if let Ok(mut guard) = cache.write() {
        guard.insert(kind, discovered);
    }
    Some(path)
}

/// 宿主新建子进程的环境覆盖：缓存中各解释器的 `TIANGONG_*_PATH` 与
/// 前置了各解释器目录的 PATH。启动注入之后的运行期环境权威来源是
/// 缓存——宿主不再修改自身全局环境，只对新建子进程单独注入（恢复
/// 后的新路径由此传导给 sidecar 及其派生的命令通道进程）。
pub(crate) fn child_env_overrides() -> Vec<(&'static str, std::ffi::OsString)> {
    derive_child_env_overrides(global_cache(), std::env::var_os("PATH").unwrap_or_default())
}

fn derive_child_env_overrides(
    cache: &RwLock<InterpreterCache>,
    base_path: std::ffi::OsString,
) -> Vec<(&'static str, std::ffi::OsString)> {
    let mut overrides = Vec::new();
    let mut path_value: Option<std::ffi::OsString> = None;
    for kind in [InterpreterKind::Node, InterpreterKind::Python] {
        let Some(cached) = valid_cached_in(cache, kind) else {
            continue;
        };
        if let Some(bin_dir) = cached.parent() {
            let base = path_value.clone().unwrap_or_else(|| base_path.clone());
            path_value = force_prepend_dir_to_path_value(&base, bin_dir).or(Some(base));
        }
        overrides.push((kind.env_key(), cached.into_os_string()));
    }
    if let Some(path_value) = path_value {
        overrides.push(("PATH", path_value));
    }
    overrides
}

/// 进程入口最早调用（任何后台线程启动前）：发现解释器并注入进程环境，
/// 使插件 sidecar 与命令通道整棵子进程树可见。
///
/// - `TIANGONG_NODE_PATH` / `TIANGONG_PYTHON_PATH` 未设置时写入探测结果
///   并记录缓存来源；外部显式指定（开发调试、CI）不覆盖，路径无效时
///   跳过留待运行时 fail-loud 报错；
/// - 无论显式还是探测所得，均把解释器所在目录前置进 PATH（已包含则
///   跳过），sidecar 派生的 node/yarn/npx 等命令通道子进程直接可用；
/// - 未探测到时不做任何改动，留待运行时报错引导安装；
/// - `std::env::set_var` 与并发读存在数据竞争，本函数只允许在 main
///   最开头、线程池启动前调用。
pub fn ensure_interpreter_env() {
    let snapshot = InterpreterEnv::from_process();
    for kind in [InterpreterKind::Node, InterpreterKind::Python] {
        let explicit = std::env::var_os(kind.env_key())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let program = match &explicit {
            Some(path) if path.is_file() => {
                if let Ok(mut guard) = global_cache().write() {
                    guard.insert(
                        kind,
                        CachedInterpreter {
                            path: path.clone(),
                            source: InterpreterSource::ExplicitOverride,
                        },
                    );
                }
                path.clone()
            }
            Some(_) => continue,
            None => match probe_interpreter(kind, &snapshot, None) {
                Some(discovered) => {
                    let path = discovered.path.clone();
                    unsafe { std::env::set_var(kind.env_key(), &path) };
                    mark_env_injected(kind);
                    if let Ok(mut guard) = global_cache().write() {
                        guard.insert(kind, discovered);
                    }
                    path
                }
                None => continue,
            },
        };
        if let Some(bin_dir) = program.parent()
            && let Some(value) =
                prepend_dir_to_path_value(&std::env::var_os("PATH").unwrap_or_default(), bin_dir)
        {
            unsafe { std::env::set_var("PATH", value) };
        }
    }
}

/// 强制前置：把目录从 PATH 中移除后放到最前。恢复场景下旧解释器
/// 目录可能仍排在前面，仅"不存在才前置"无法保证新目录的优先级。
fn force_prepend_dir_to_path_value(
    existing: &std::ffi::OsStr,
    directory: &Path,
) -> Option<std::ffi::OsString> {
    std::env::join_paths(
        std::iter::once(directory.to_path_buf())
            .chain(std::env::split_paths(existing).filter(|path| path != directory)),
    )
    .ok()
}

/// 计算前置目录后的新 PATH；目录已在 PATH 中或拼接失败（路径含列表
/// 分隔符等极端情况）时返回 None（保持原值）。
fn prepend_dir_to_path_value(
    existing: &std::ffi::OsStr,
    directory: &Path,
) -> Option<std::ffi::OsString> {
    if std::env::split_paths(existing).any(|dir| dir == directory) {
        return None;
    }
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(existing)),
    )
    .ok()
}

/// 全量探测：PATH → 平台策略（Windows 固定环境路径 / Unix 常见位置）。
/// `exclude` 排除一个已知无法创建进程的路径（所有候选来源统一过滤），
/// 供失败恢复时跳过"文件仍在但不可执行"的旧解释器。
fn probe_interpreter(
    kind: InterpreterKind,
    env: &InterpreterEnv,
    exclude: Option<&Path>,
) -> Option<CachedInterpreter> {
    if let Some(found) = probe_from_path(kind, env, exclude) {
        return Some(found);
    }
    if cfg!(windows) {
        probe_windows_environment(kind, env, exclude)
    } else {
        probe_unix_common_locations(kind, env, exclude)
    }
}

/// PATH 中按程序名直接查找。
fn probe_from_path(
    kind: InterpreterKind,
    env: &InterpreterEnv,
    exclude: Option<&Path>,
) -> Option<CachedInterpreter> {
    for directory in &env.search_paths {
        for name in kind.program_names() {
            let path = directory.join(name);
            if path.is_file() && Some(path.as_path()) != exclude {
                return Some(CachedInterpreter {
                    path,
                    source: InterpreterSource::Path,
                });
            }
        }
    }
    None
}

/// Windows 策略：只检查环境变量可直接构造的固定路径，不枚举任何版本
/// 目录。`NVM_HOME` 是版本仓库，无法在不枚举的前提下得知当前版本，
/// 因此不参与探测（当前版本经 `NVM_SYMLINK` 获取）。本函数不依赖
/// 编译期平台开关，可在任意平台的测试中执行。
fn probe_windows_environment(
    kind: InterpreterKind,
    env: &InterpreterEnv,
    exclude: Option<&Path>,
) -> Option<CachedInterpreter> {
    let home = env.home.as_deref();
    let mut candidates = Vec::new();
    match kind {
        InterpreterKind::Node => {
            if let Some(symlink) = &env.nvm_symlink {
                candidates.push(symlink.join("node.exe"));
            }
            if let Some(volta) = explicit_or_default_root(env.volta_home.as_ref(), home, ".volta") {
                candidates.push(volta.join("bin").join("node.exe"));
            }
            if let Some(scoop) = explicit_or_default_root(env.scoop.as_ref(), home, "scoop") {
                candidates.push(scoop.join("shims").join("node.exe"));
            }
            if let Some(chocolatey) = explicit_or_default_root(
                env.chocolatey_install.as_ref(),
                env.program_data.as_deref(),
                "chocolatey",
            ) {
                candidates.push(chocolatey.join("bin").join("node.exe"));
            }
            if let Some(program_files) = &env.program_files {
                candidates.push(program_files.join("nodejs").join("node.exe"));
            }
        }
        InterpreterKind::Python => {
            if let Some(pyenv) = explicit_or_default_root(env.pyenv_root.as_ref(), home, ".pyenv") {
                candidates.push(pyenv.join("shims").join("python.exe"));
                candidates.push(pyenv.join("pyenv-win").join("shims").join("python.exe"));
            }
            if let Some(scoop) = explicit_or_default_root(env.scoop.as_ref(), home, "scoop") {
                candidates.push(scoop.join("shims").join("python.exe"));
            }
            if let Some(chocolatey) = explicit_or_default_root(
                env.chocolatey_install.as_ref(),
                env.program_data.as_deref(),
                "chocolatey",
            ) {
                candidates.push(chocolatey.join("bin").join("python.exe"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|path| path.is_file() && Some(path.as_path()) != exclude)
        .map(|path| CachedInterpreter {
            path,
            source: InterpreterSource::Environment,
        })
}

/// macOS/Linux 慢路径：版本管理器目录（从新到旧）与系统标准位置。
/// 仅在缓存未命中或失效时执行，正常调用不会重复枚举。
fn probe_unix_common_locations(
    kind: InterpreterKind,
    env: &InterpreterEnv,
    exclude: Option<&Path>,
) -> Option<CachedInterpreter> {
    let runtime = match kind {
        InterpreterKind::Node => SidecarRuntime::Node,
        InterpreterKind::Python => SidecarRuntime::Python,
    };
    common_interpreter_locations(runtime, env)
        .into_iter()
        .find(|path| path.is_file() && Some(path.as_path()) != exclude)
        .map(|path| CachedInterpreter {
            path,
            source: InterpreterSource::CommonLocation,
        })
}

/// Unix 常见安装位置（分层回退）：安装工具声明的根目录 → 版本管理器
/// 目录（从新到旧）→ 系统标准位置。某工具变量缺失只说明未使用该工具，
/// 继续尝试后续层次，不据此判定解释器不存在。显式根各自独立生效，
/// home 缺失不影响已显式声明的工具。
fn common_interpreter_locations(runtime: SidecarRuntime, env: &InterpreterEnv) -> Vec<PathBuf> {
    let home = env.home.as_deref();
    let versioned: Vec<(PathBuf, &str)> = match runtime {
        SidecarRuntime::Native => Vec::new(),
        SidecarRuntime::Node => {
            let mut roots = Vec::new();
            if let Some(nvm) = explicit_or_default_root(env.nvm_dir.as_ref(), home, ".nvm") {
                roots.push((nvm.join("versions").join("node"), "bin/node"));
            }
            if let Some(asdf) = explicit_or_default_root(env.asdf_data_dir.as_ref(), home, ".asdf")
            {
                roots.push((asdf.join("installs").join("nodejs"), "bin/node"));
            }
            roots
        }
        SidecarRuntime::Python => explicit_or_default_root(env.pyenv_root.as_ref(), home, ".pyenv")
            .map(|pyenv| vec![(pyenv.join("versions"), "bin/python3")])
            .unwrap_or_default(),
    };
    let mut locations = Vec::new();
    for (root, leaf) in versioned {
        locations.extend(versioned_bin_candidates(&root, leaf));
    }
    if matches!(runtime, SidecarRuntime::Node)
        && let Some(volta) = explicit_or_default_root(env.volta_home.as_ref(), home, ".volta")
    {
        locations.push(volta.join("bin").join("node"));
    }
    locations.extend(match runtime {
        SidecarRuntime::Node => env.node_system_locations.iter().cloned(),
        SidecarRuntime::Python => env.python_system_locations.iter().cloned(),
        SidecarRuntime::Native => [].iter().cloned(),
    });
    locations
}

/// node 的各平台系统标准安装位置（Homebrew、系统包管理等）。
fn default_node_system_locations() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/opt/homebrew/bin/node"),
            PathBuf::from("/usr/local/bin/node"),
        ]
    } else if cfg!(windows) {
        Vec::new()
    } else {
        vec![
            PathBuf::from("/usr/local/bin/node"),
            PathBuf::from("/usr/bin/node"),
        ]
    }
}

/// python 的各平台系统标准安装位置。
fn default_python_system_locations() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/opt/homebrew/bin/python3"),
            PathBuf::from("/usr/local/bin/python3"),
            PathBuf::from("/usr/bin/python3"),
        ]
    } else if cfg!(windows) {
        Vec::new()
    } else {
        vec![
            PathBuf::from("/usr/local/bin/python3"),
            PathBuf::from("/usr/bin/python3"),
        ]
    }
}

/// 枚举版本管理器安装根目录（如 nvm 的 `versions/node/`）下的 bin 候选，
/// 按版本号新到旧排序；目录不存在时返回空。
fn versioned_bin_candidates(root: &Path, leaf: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort_by_key(|name| std::cmp::Reverse(version_sort_key(name)));
    names
        .into_iter()
        .map(|name| root.join(name).join(leaf))
        .collect()
}

/// 版本目录名（`v22.16.0` / `22.16.0`）转数字段序列供排序；带后缀
/// （如 pyenv 的 `3.11.0-env`）只取主版本段；混合段（如 Windows Python
/// 的 `Python312`）取段内数字拼接比较；无法解析的段按 0 计。
fn version_sort_key(name: &str) -> Vec<u64> {
    name.trim_start_matches(['v', 'V'])
        .split('.')
        .map(|part| {
            let part = part.split_once('-').map_or(part, |(major, _)| major);
            part.parse().unwrap_or_else(|_| {
                let digits: String = part.chars().filter(|c| c.is_ascii_digit()).collect();
                digits.parse().unwrap_or(0)
            })
        })
        .collect()
}

#[cfg(test)]
mod interpreter_discovery_tests {
    use super::*;
    use std::cell::Cell;

    fn empty_cache() -> RwLock<InterpreterCache> {
        RwLock::new(HashMap::new())
    }

    #[test]
    fn version_sort_key_orders_segments_numerically() {
        let mut names = vec!["v9.11.2", "v22.16.0", "v10.24.1"];
        names.sort_by_key(|name| std::cmp::Reverse(version_sort_key(name)));
        assert_eq!(names, ["v22.16.0", "v10.24.1", "v9.11.2"]);
    }

    #[test]
    fn version_sort_key_tolerates_prefix_and_suffix() {
        assert_eq!(version_sort_key("v22.16.0"), version_sort_key("22.16.0"));
        assert!(version_sort_key("3.11.0-env") > version_sort_key("3.10.9"));
        assert!(version_sort_key("Python312") > version_sort_key("Python39"));
        assert_eq!(version_sort_key("not-a-version"), vec![0]);
    }

    #[test]
    fn prepend_dir_to_path_value_prepends_and_deduplicates() {
        let directory = PathBuf::from("/opt/homebrew/bin");
        let tail = [PathBuf::from("/usr/bin"), PathBuf::from("/bin")];
        // 用 join/split 按平台分隔符构造与校验，输入输出均不写死格式
        let existing = std::env::join_paths(&tail).unwrap();
        let value = prepend_dir_to_path_value(&existing, &directory).unwrap();
        let mut expected = vec![directory.clone()];
        expected.extend(tail);
        assert_eq!(std::env::split_paths(&value).collect::<Vec<_>>(), expected);
        // 目录已存在时保持原值（返回 None）
        assert!(prepend_dir_to_path_value(&value, &directory).is_none());
    }

    #[test]
    fn versioned_bin_candidates_newest_first_and_missing_root_empty() {
        let dir = tempfile::tempdir().unwrap();
        for version in ["v18.20.0", "v22.16.0", "v9.11.2"] {
            std::fs::create_dir_all(dir.path().join(version).join("bin")).unwrap();
        }
        let candidates = versioned_bin_candidates(dir.path(), "bin/node");
        let versions: Vec<_> = candidates
            .iter()
            .map(|path| {
                path.parent()
                    .and_then(|parent| parent.parent())
                    .and_then(|grandparent| grandparent.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap()
            })
            .collect();
        assert_eq!(versions, ["v22.16.0", "v18.20.0", "v9.11.2"]);
        assert!(candidates[0].ends_with("bin/node"));
        assert!(versioned_bin_candidates(&dir.path().join("absent"), "bin/node").is_empty());
    }

    /// 探测函数经 InterpreterEnv 注入伪造环境，不读写真实环境变量，
    /// 与其他并行测试互不干扰。
    #[test]
    #[cfg(not(windows))]
    fn common_interpreter_locations_covers_nvm_and_system_paths() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".nvm/versions/node/v22.16.0/bin")).unwrap();
        std::fs::create_dir_all(home.path().join(".nvm/versions/node/v18.20.0/bin")).unwrap();
        let env = InterpreterEnv {
            home: Some(home.path().to_path_buf()),
            node_system_locations: default_node_system_locations(),
            ..InterpreterEnv::default()
        };
        let locations = common_interpreter_locations(SidecarRuntime::Node, &env);
        assert_eq!(
            locations[0],
            home.path().join(".nvm/versions/node/v22.16.0/bin/node")
        );
        let expected = if cfg!(target_os = "macos") {
            "/opt/homebrew/bin/node"
        } else {
            "/usr/local/bin/node"
        };
        assert!(locations.contains(&PathBuf::from(expected)));
    }

    /// 显式声明的工具根目录直接生效，不依赖 HOME 推导（NVM_DIR、
    /// ASDF_DATA_DIR、VOLTA_HOME，home 缺失仍可工作）。
    #[test]
    #[cfg(not(windows))]
    fn explicit_node_tool_roots_work_without_home() {
        let nvm = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(nvm.path().join("versions/node/v22.16.0/bin")).unwrap();
        let asdf = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(asdf.path().join("installs/nodejs/v10.24.1/bin")).unwrap();
        let env = InterpreterEnv {
            nvm_dir: Some(nvm.path().to_path_buf()),
            asdf_data_dir: Some(asdf.path().to_path_buf()),
            volta_home: Some(PathBuf::from("/opt/volta")),
            ..InterpreterEnv::default()
        };
        let locations = common_interpreter_locations(SidecarRuntime::Node, &env);
        assert_eq!(
            locations[0],
            nvm.path().join("versions/node/v22.16.0/bin/node")
        );
        assert!(locations.contains(&asdf.path().join("installs/nodejs/v10.24.1/bin/node")));
        assert!(locations.contains(&PathBuf::from("/opt/volta/bin/node")));
    }

    /// 显式 PYENV_ROOT 直接生效，home 缺失仍可工作。
    #[test]
    #[cfg(not(windows))]
    fn explicit_pyenv_root_works_without_home() {
        let pyenv = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(pyenv.path().join("versions/3.12.1/bin")).unwrap();
        let env = InterpreterEnv {
            pyenv_root: Some(pyenv.path().to_path_buf()),
            ..InterpreterEnv::default()
        };
        let locations = common_interpreter_locations(SidecarRuntime::Python, &env);
        assert_eq!(
            locations[0],
            pyenv.path().join("versions/3.12.1/bin/python3")
        );
    }

    /// PATH 命中优先于常见安装位置。
    #[test]
    fn probe_prefers_path_before_common_locations() {
        let path_dir = tempfile::tempdir().unwrap();
        let program = path_dir
            .path()
            .join(if cfg!(windows) { "node.exe" } else { "node" });
        std::fs::write(&program, b"").unwrap();
        let env = InterpreterEnv {
            search_paths: vec![path_dir.path().to_path_buf()],
            volta_home: Some(PathBuf::from("/opt/volta")),
            ..InterpreterEnv::default()
        };
        assert_eq!(
            probe_interpreter(InterpreterKind::Node, &env, None)
                .map(|found| (found.path, found.source)),
            Some((program, InterpreterSource::Path))
        );
    }

    /// 空 PATH 时回退版本管理器目录（缓存未命中的慢路径）。
    #[test]
    fn empty_search_paths_fall_back_to_versioned_roots() {
        if cfg!(windows) {
            // Windows 策略不枚举版本目录，由固定路径测试覆盖
            return;
        }
        let nvm = tempfile::tempdir().unwrap();
        let program = nvm.path().join("versions/node/v22.16.0/bin/node");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, b"").unwrap();
        let env = InterpreterEnv {
            nvm_dir: Some(nvm.path().to_path_buf()),
            ..InterpreterEnv::default()
        };
        assert_eq!(
            probe_interpreter(InterpreterKind::Node, &env, None).map(|found| found.path),
            Some(program)
        );
    }

    /// 所有来源为空（快照全空、系统位置未填）时探测返回 None。
    #[test]
    fn all_sources_empty_returns_none() {
        let env = InterpreterEnv::default();
        assert!(
            probe_interpreter(InterpreterKind::Node, &env, None).is_none(),
            "全空快照不应发现解释器"
        );
        assert!(probe_interpreter(InterpreterKind::Python, &env, None).is_none());
    }

    /// Windows 策略命中环境变量直接构造的固定路径（本函数不依赖编译期
    /// 平台开关，全平台可测）。
    #[test]
    fn windows_probe_uses_fixed_env_paths() {
        let root = tempfile::tempdir().unwrap();
        let symlink_node = root.path().join("node-current").join("node.exe");
        std::fs::create_dir_all(symlink_node.parent().unwrap()).unwrap();
        std::fs::write(&symlink_node, b"").unwrap();
        let pyenv_shim = root.path().join("pyenv").join("shims").join("python.exe");
        std::fs::create_dir_all(pyenv_shim.parent().unwrap()).unwrap();
        std::fs::write(&pyenv_shim, b"").unwrap();
        let env = InterpreterEnv {
            nvm_symlink: Some(root.path().join("node-current")),
            pyenv_root: Some(root.path().join("pyenv")),
            ..InterpreterEnv::default()
        };
        assert_eq!(
            probe_windows_environment(InterpreterKind::Node, &env, None)
                .map(|found| (found.path, found.source)),
            Some((symlink_node, InterpreterSource::Environment))
        );
        assert_eq!(
            probe_windows_environment(InterpreterKind::Python, &env, None).map(|found| found.path),
            Some(pyenv_shim)
        );
    }

    /// Windows 策略不枚举版本目录：Program Files 下存在 Python312 也不
    /// 会被扫描（NVM_HOME 根本不进入快照，天然不可能被枚举）。
    #[test]
    fn windows_probe_never_scans_version_directories() {
        let root = tempfile::tempdir().unwrap();
        let python = root
            .path()
            .join("program-files")
            .join("Python312")
            .join("python.exe");
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, b"").unwrap();
        let env = InterpreterEnv {
            program_files: Some(root.path().join("program-files")),
            ..InterpreterEnv::default()
        };
        assert!(
            probe_windows_environment(InterpreterKind::Python, &env, None).is_none(),
            "Program Files 的 Python3xx 不应被枚举"
        );
    }

    /// 首次解析写入缓存，第二次直接使用缓存且不再探测。
    #[test]
    fn first_resolve_caches_and_second_uses_cache() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("node");
        std::fs::write(&program, b"").unwrap();
        let cache = empty_cache();
        let probes = Cell::new(0u32);
        let discovered = CachedInterpreter {
            path: program.clone(),
            source: InterpreterSource::CommonLocation,
        };
        assert_eq!(
            resolve_via(InterpreterKind::Node, None, false, &cache, || {
                probes.set(probes.get() + 1);
                Some(discovered.clone())
            })
            .unwrap(),
            program
        );
        assert_eq!(
            resolve_via(InterpreterKind::Node, None, false, &cache, || {
                panic!("缓存命中不应再次探测")
            })
            .unwrap(),
            program
        );
        assert_eq!(probes.get(), 1, "探测应只执行一次");
    }

    /// 缓存文件被删除后自动重新探测。
    #[test]
    fn cache_reprobes_when_file_removed() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("v22").join("node");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::write(&first, b"").unwrap();
        let cache = empty_cache();
        assert_eq!(
            resolve_via(InterpreterKind::Node, None, false, &cache, || Some(
                CachedInterpreter {
                    path: first.clone(),
                    source: InterpreterSource::CommonLocation,
                }
            ))
            .unwrap(),
            first
        );
        std::fs::remove_file(&first).unwrap();
        let second = dir.path().join("v24").join("node");
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        std::fs::write(&second, b"").unwrap();
        assert_eq!(
            resolve_via(InterpreterKind::Node, None, false, &cache, || Some(
                CachedInterpreter {
                    path: second.clone(),
                    source: InterpreterSource::CommonLocation,
                }
            ))
            .unwrap(),
            second,
            "缓存失效后应重新探测"
        );
    }

    /// 失效接口只清除与失败路径匹配的缓存，不误删其他条目。
    #[test]
    fn invalidate_only_clears_matching_path() {
        let cache = empty_cache();
        cache.write().unwrap().insert(
            InterpreterKind::Node,
            CachedInterpreter {
                path: PathBuf::from("/opt/old/node"),
                source: InterpreterSource::CommonLocation,
            },
        );
        assert!(!invalidate_if_matches_in(
            &cache,
            InterpreterKind::Node,
            Path::new("/opt/other/node")
        ));
        assert!(cache.read().unwrap().contains_key(&InterpreterKind::Node));
        assert!(invalidate_if_matches_in(
            &cache,
            InterpreterKind::Node,
            Path::new("/opt/old/node")
        ));
        assert!(!cache.read().unwrap().contains_key(&InterpreterKind::Node));
    }

    /// 用户显式路径无效时直接报错，不回退到其他解释器。
    #[test]
    fn explicit_invalid_path_errors_without_fallback() {
        let cache = empty_cache();
        let error = resolve_via(
            InterpreterKind::Node,
            Some(PathBuf::from("/nonexistent/node-20")),
            false,
            &cache,
            || panic!("显式无效时不应回退探测"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("TIANGONG_NODE_PATH"));
        assert!(cache.read().unwrap().is_empty(), "失败状态不写入缓存");
    }

    /// 真实恢复链路：应用自动发现并写入缓存与环境变量 → 程序文件被删
    /// → spawn 失败触发 invalidate_if_matches（缓存清空、环境变量残留）
    /// → 重新解析（注入标记独立于缓存存活）→ 探测到新路径。
    #[test]
    fn recovery_after_invalidate_ignores_stale_injected_env() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old-node");
        std::fs::write(&old, b"").unwrap();
        let cache = empty_cache();
        // 启动时应用自动发现：写缓存（非显式来源）并注入环境变量
        cache.write().unwrap().insert(
            InterpreterKind::Node,
            CachedInterpreter {
                path: old.clone(),
                source: InterpreterSource::Environment,
            },
        );
        std::fs::remove_file(&old).unwrap();
        // spawn 失败后的真实顺序：先失效缓存，再重新解析
        assert!(invalidate_if_matches_in(
            &cache,
            InterpreterKind::Node,
            &old
        ));
        let fresh = dir.path().join("fresh-node");
        std::fs::write(&fresh, b"").unwrap();
        assert_eq!(
            resolve_via(InterpreterKind::Node, Some(old), true, &cache, || {
                Some(CachedInterpreter {
                    path: fresh.clone(),
                    source: InterpreterSource::CommonLocation,
                })
            })
            .unwrap(),
            fresh,
            "失效后残留的应用注入值不应被误认为用户显式配置"
        );
    }

    /// 应用注入标记下，环境变量中残留的旧路径即使文件仍然存在（权限/
    /// 格式问题导致启动失败），重新解析也以重新探测的结果为准。
    #[test]
    fn injected_env_value_is_ignored_during_re_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("stale-node");
        std::fs::write(&stale, b"").unwrap();
        let cache = empty_cache();
        let fresh = dir.path().join("fresh-node");
        std::fs::write(&fresh, b"").unwrap();
        assert_eq!(
            resolve_via(
                InterpreterKind::Node,
                Some(stale.clone()),
                true,
                &cache,
                || {
                    Some(CachedInterpreter {
                        path: fresh.clone(),
                        source: InterpreterSource::CommonLocation,
                    })
                }
            )
            .unwrap(),
            fresh,
            "应用注入值不应在重新解析时短路探测"
        );
        assert!(stale.is_file(), "测试前提：旧文件仍存在");
    }

    /// 排除失败路径的探测：旧解释器文件仍然存在（权限/格式问题导致
    /// 无法创建进程）时，重新探测跳过它并选中次优来源。
    #[test]
    fn probe_excluding_failed_path_falls_through_to_next_source() {
        let path_dir = tempfile::tempdir().unwrap();
        let broken = path_dir
            .path()
            .join(if cfg!(windows) { "node.exe" } else { "node" });
        std::fs::write(&broken, b"").unwrap();
        let nvm = tempfile::tempdir().unwrap();
        let good = nvm.path().join("versions/node/v22.16.0/bin/node");
        std::fs::create_dir_all(good.parent().unwrap()).unwrap();
        std::fs::write(&good, b"").unwrap();
        let env = InterpreterEnv {
            search_paths: vec![path_dir.path().to_path_buf()],
            nvm_dir: Some(nvm.path().to_path_buf()),
            ..InterpreterEnv::default()
        };
        // 不排除：PATH 命中坏文件
        assert_eq!(
            probe_interpreter(InterpreterKind::Node, &env, None).map(|found| found.path),
            Some(broken.clone())
        );
        // 排除坏路径：PATH 过滤后落到版本管理器目录
        assert_eq!(
            probe_interpreter(InterpreterKind::Node, &env, Some(&broken)).map(|found| found.path),
            Some(good)
        );
    }

    /// 恢复接口：缓存已是其他有效路径（他方已完成恢复）时直接复用，
    /// 不再探测——并发恢复经探测锁串行化，后来者拿到新路径即可重试。
    #[test]
    fn recover_reuses_completed_recovery_from_peer() {
        let peer = tempfile::tempdir().unwrap();
        let recovered_by_peer = peer.path().join("node");
        std::fs::write(&recovered_by_peer, b"").unwrap();
        let cache = empty_cache();
        cache.write().unwrap().insert(
            InterpreterKind::Node,
            CachedInterpreter {
                path: recovered_by_peer.clone(),
                source: InterpreterSource::CommonLocation,
            },
        );
        let lock = std::sync::Mutex::new(());
        assert_eq!(
            recover_via(
                InterpreterKind::Node,
                Path::new("/opt/old/node"),
                None,
                true,
                &cache,
                &lock,
                || panic!("他方已恢复不应再次探测"),
            ),
            Some(recovered_by_peer)
        );
        assert!(cache.read().unwrap().contains_key(&InterpreterKind::Node));
    }

    /// 恢复接口：同一失败路径的第二次恢复复用第一次的结果，探测只
    /// 执行一次。
    #[test]
    fn second_recovery_reuses_first_result_without_reprobing() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old-node");
        std::fs::write(&old, b"").unwrap();
        let fresh = dir.path().join("fresh-node");
        std::fs::write(&fresh, b"").unwrap();
        let cache = empty_cache();
        cache.write().unwrap().insert(
            InterpreterKind::Node,
            CachedInterpreter {
                path: old.clone(),
                source: InterpreterSource::Environment,
            },
        );
        let lock = std::sync::Mutex::new(());
        let probes = std::cell::Cell::new(0u32);
        let probe_once = || {
            probes.set(probes.get() + 1);
            Some(CachedInterpreter {
                path: fresh.clone(),
                source: InterpreterSource::CommonLocation,
            })
        };
        assert_eq!(
            recover_via(
                InterpreterKind::Node,
                &old,
                Some(old.as_path()),
                true,
                &cache,
                &lock,
                probe_once,
            ),
            Some(fresh.clone())
        );
        // 第二个 sidecar 因同一路径失败：复用缓存中的恢复结果
        assert_eq!(
            recover_via(
                InterpreterKind::Node,
                &old,
                Some(old.as_path()),
                true,
                &cache,
                &lock,
                probe_once,
            ),
            Some(fresh)
        );
        assert_eq!(probes.get(), 1, "探测应只执行一次");
    }

    /// 强制前置：新解释器目录已在 PATH 中间时被移到最前，坏目录退后。
    #[test]
    fn force_prepend_moves_existing_directory_to_front() {
        let bad = PathBuf::from("/bad-node/bin");
        let good = PathBuf::from("/good-node/bin");
        let system = PathBuf::from("/usr/bin");
        let existing = std::env::join_paths([bad.clone(), good.clone(), system.clone()]).unwrap();
        let value = force_prepend_dir_to_path_value(&existing, &good).unwrap();
        assert_eq!(
            std::env::split_paths(&value).collect::<Vec<_>>(),
            [good, bad, system]
        );
    }

    /// 强制前置的完整恢复场景：原 PATH 为坏目录、新目录、系统目录
    /// （新目录已在中间），缓存指向新目录中的解释器，派生的子进程
    /// PATH 必须是新目录居首、坏目录退后。
    #[test]
    fn derived_path_puts_cached_interpreter_dir_before_broken_dir() {
        let bad = PathBuf::from("/bad-node/bin");
        let good_root = tempfile::tempdir().unwrap();
        let good = good_root.path().join("good-node").join("bin");
        let system = PathBuf::from("/usr/bin");
        let cache = empty_cache();
        let good_node = good.join("node");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(&good_node, b"").unwrap();
        cache.write().unwrap().insert(
            InterpreterKind::Node,
            CachedInterpreter {
                path: good_node,
                source: InterpreterSource::CommonLocation,
            },
        );
        let original = std::env::join_paths([bad.clone(), good.clone(), system.clone()]).unwrap();
        let overrides = derive_child_env_overrides(&cache, original);
        let path_override = overrides
            .iter()
            .find(|(key, _)| *key == "PATH")
            .expect("应包含 PATH 覆盖");
        assert_eq!(
            std::env::split_paths(&path_override.1).collect::<Vec<_>>(),
            [good, bad, system],
            "缓存解释器目录必须排在坏目录之前"
        );
    }

    /// 恢复接口：用户显式指定的路径无法启动时直接失败，不静默替换。
    #[test]
    fn recover_refuses_to_replace_user_explicit_path() {
        let cache = empty_cache();
        let explicit = PathBuf::from("/opt/explicit/node");
        cache.write().unwrap().insert(
            InterpreterKind::Node,
            CachedInterpreter {
                path: explicit.clone(),
                source: InterpreterSource::ExplicitOverride,
            },
        );
        let lock = std::sync::Mutex::new(());
        assert!(
            recover_via(
                InterpreterKind::Node,
                &explicit,
                Some(&explicit),
                false,
                &cache,
                &lock,
                || panic!("用户显式失败不应触发替换探测"),
            )
            .is_none()
        );
        assert!(
            !cache.read().unwrap().contains_key(&InterpreterKind::Node),
            "显式失败仍应清除缓存，让错误语义回到 resolve 的 fail-loud"
        );
    }

    /// 恢复接口：应用注入场景排除失败路径重探成功并更新缓存。
    #[test]
    fn recover_reprobes_excluding_failed_and_updates_cache() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old-node");
        std::fs::write(&old, b"").unwrap();
        let fresh = dir.path().join("fresh-node");
        std::fs::write(&fresh, b"").unwrap();
        let cache = empty_cache();
        cache.write().unwrap().insert(
            InterpreterKind::Node,
            CachedInterpreter {
                path: old.clone(),
                source: InterpreterSource::Environment,
            },
        );
        let lock = std::sync::Mutex::new(());
        let outcome = recover_via(
            InterpreterKind::Node,
            &old,
            Some(old.as_path()),
            true,
            &cache,
            &lock,
            || {
                Some(CachedInterpreter {
                    path: fresh.clone(),
                    source: InterpreterSource::CommonLocation,
                })
            },
        )
        .expect("应用注入场景应恢复");
        assert_eq!(outcome, fresh);
        assert_eq!(
            cache
                .read()
                .unwrap()
                .get(&InterpreterKind::Node)
                .map(|c| c.path.clone()),
            Some(fresh)
        );
    }

    /// 恢复接口：排除失败路径后无其他可用解释器时返回 None。
    #[test]
    fn recover_returns_none_without_alternative() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("only-node");
        std::fs::write(&old, b"").unwrap();
        let cache = empty_cache();
        cache.write().unwrap().insert(
            InterpreterKind::Node,
            CachedInterpreter {
                path: old.clone(),
                source: InterpreterSource::Environment,
            },
        );
        let lock = std::sync::Mutex::new(());
        assert!(
            recover_via(
                InterpreterKind::Node,
                &old,
                Some(old.as_path()),
                true,
                &cache,
                &lock,
                || None,
            )
            .is_none()
        );
    }

    /// 并发首次解析同一种解释器只探测一次（per-kind 探测锁 + 双检）。
    #[test]
    fn concurrent_first_resolve_probes_once() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("node");
        std::fs::write(&program, b"").unwrap();
        let cache = empty_cache();
        let lock = std::sync::Mutex::new(());
        let probes = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cache = std::sync::Arc::new(cache);
        let lock = std::sync::Arc::new(lock);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let cache = std::sync::Arc::clone(&cache);
            let lock = std::sync::Arc::clone(&lock);
            let probes = std::sync::Arc::clone(&probes);
            let program = program.clone();
            handles.push(std::thread::spawn(move || {
                resolve_locked(InterpreterKind::Node, None, false, &cache, &lock, || {
                    probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    Some(CachedInterpreter {
                        path: program,
                        source: InterpreterSource::CommonLocation,
                    })
                })
            }));
        }
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(
            probes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "并发首次解析应只探测一次"
        );
    }
}
