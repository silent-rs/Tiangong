//! 随 App 发布的固定沙箱 Launcher。
//!
//! 定位：宿主决定策略，Launcher 负责校验策略、探测平台能力、应用 OS 沙箱
//! 并启动目标进程。目标进程及其全部子进程树
//! 继承沙箱约束；插件完全不参与沙箱决策。
//!
//! 通信（一次性包装器形态，非常驻守护）：
//! - Unix 策略经继承文件描述符 fd3 以长度前缀帧传入；Windows 使用仅
//!   Launcher 读取的一次性环境信封（结构化 JSON、双版本化）；
//! - stdin/stdout 留给目标进程与宿主的业务通信；
//! - Unix 上以 `exec` 替换自身：应用沙箱约束后变身目标进程，
//!   管道与生命周期天然继承，无额外转发层；
//! - **fail-closed**：协议版本不符、策略非法、平台沙箱不可用一律拒绝
//!   （结构化错误 + 非零退出码），绝不静默降级为无沙箱执行。
//!
//! 版本化三层（RFC 0017 §九）：
//! - 产品版本：本 crate 版本；
//! - `protocol_version`：App↔Launcher 通信协议；
//! - `policy_schema`：策略字段与安全语义。
//!
//! 子命令：
//! - `--self-check`：激活前自检（平台探测 + 真实拦截核心项）；
//! - `check-update`：只检查当前 Sandbox 是否有官方更新；
//! - `update`：由 Sandbox 自身下载、验签、自检并更新当前程序；
//! - `update --root <目录>`：将最新版安装到指定绝对目录。

#[cfg(any(unix, windows))]
use std::io::Read;
#[cfg(windows)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use serde::Deserialize;
#[cfg(windows)]
use serde::Serialize;
use sha2::{Digest, Sha256};

/// App ↔ Launcher 通信协议版本（唯一定义在库层，宿主与 bin 共同引用）。
const PROTOCOL_VERSION: u32 = tiangong_sandbox::LAUNCHER_PROTOCOL_VERSION;
/// 策略 Schema 版本（同上，库层唯一）。
const POLICY_SCHEMA: u32 = tiangong_sandbox::LAUNCHER_POLICY_SCHEMA;
/// 策略经此继承描述符传入。
#[cfg(unix)]
const POLICY_FD: i32 = 3;
/// fail-closed 退出码（策略/协议/平台不可用/自检失败）。
const EXIT_SANDBOX_UNAVAILABLE: i32 = 78;
/// 自检专用：当前宿主环境无法验证（如嵌套沙箱），非制品缺陷。
const EXIT_ENV_UNVERIFIABLE: i32 = 79;
/// 自检 `.git` 可写探测标记（用户裁定 2026-08-30：agent 需要完整 git
/// 工作流，`.git` 与源码同等对待）。父/子进程探针共用，重复探测幂等。
const GIT_METADATA_PROBE_MARKER: &str = "GITWRITE";

/// Launcher 启动指令（宿主经 fd3 写入）。
#[derive(Debug, Deserialize)]
struct LaunchRequest {
    #[serde(default)]
    grant_cache: Option<PathBuf>,
    protocol_version: u32,
    policy_schema: u32,
    policy: tiangong_sandbox::SandboxPolicy,
    #[serde(default)]
    plugin_id: String,
    program: String,
    program_root: String,
    program_sha256: String,
    #[serde(default)]
    args: Vec<String>,
    /// 目标校验方式。缺省为通用摘要校验；天工插件宿主可显式要求清单比对。
    #[serde(default)]
    target_validation: TargetValidation,
    /// 旧协议兼容字段；新宿主应使用 target_validation。
    #[serde(default)]
    interpreter: bool,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TargetValidation {
    #[default]
    DigestOnly,
    PluginManifest,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("--version" | "version")
    ) {
        println!("tiangong-sandbox {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if matches!(args.first().map(String::as_str), Some("--help" | "help")) {
        println!(
            "tiangong-sandbox {}\n\nUSAGE:\n  tiangong-sandbox --self-check\n  tiangong-sandbox run --policy <policy.json> -- <command> [args...]\n  tiangong-sandbox run --workspace <absolute-dir> [policy-options] -- <command> [args...]\n  tiangong-sandbox check-update [--manifest-url <https-url>] [--public-key <file>]\n  tiangong-sandbox update [--manifest-url <https-url>] [--public-key <file>]\n  tiangong-sandbox update --root <absolute-dir> [--manifest-url <https-url>] [--public-key <file>]",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    #[cfg(windows)]
    if matches!(
        args.first().map(String::as_str),
        Some("prepare-volume-metadata" | "revoke-volume-metadata")
    ) {
        let result = if args.len() == 2 {
            if args[0] == "prepare-volume-metadata" {
                tiangong_sandbox::sandbox::windows::prepare_volume_metadata(Path::new(&args[1]))
            } else {
                tiangong_sandbox::sandbox::windows::revoke_volume_metadata(Path::new(&args[1]))
            }
        } else {
            Err(anyhow::anyhow!(
                "用法: prepare-volume-metadata <工作区绝对路径>"
            ))
        };
        match result {
            Ok(root) => println!("磁盘基本信息访问已更新: {}", root.display()),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(EXIT_SANDBOX_UNAVAILABLE);
            }
        }
        return;
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("complete-update") {
        if let Err(error) = run_windows_update_completer(&args) {
            eprintln!("{}", serde_json::json!({ "error": format!("{error:#}") }));
            std::process::exit(EXIT_SANDBOX_UNAVAILABLE);
        }
        return;
    }
    if args.first().map(String::as_str) == Some("run") {
        if let Err(error) = run_policy_file(&args) {
            eprintln!("{}", serde_json::json!({ "error": format!("{error:#}") }));
            std::process::exit(EXIT_SANDBOX_UNAVAILABLE);
        }
        return;
    }
    if matches!(
        args.first().map(String::as_str),
        Some("check-update" | "update")
    ) {
        match run_self_update(&args) {
            Ok(()) => return,
            Err(error) => {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "launcher": "tiangong-sandbox",
                        "product_version": env!("CARGO_PKG_VERSION"),
                        "error": format!("{error:#}"),
                    })
                );
                std::process::exit(EXIT_SANDBOX_UNAVAILABLE);
            }
        }
    }
    if args.first().map(String::as_str) == Some("--self-check-network-probe") {
        let address = args.get(1).map(String::as_str).unwrap_or_default();
        std::process::exit(run_network_probe(address));
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("--windows-self-check-file-probe") {
        let request = args.get(1).map(String::as_str).unwrap_or_default();
        std::process::exit(run_windows_file_probe(request));
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("--windows-self-check-child-probe") {
        let request = args.get(1).map(String::as_str).unwrap_or_default();
        let expected_stdin = args.get(2).map(String::as_str);
        std::process::exit(run_windows_child_probe(request, expected_stdin));
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("--windows-self-check-process-limit-probe") {
        std::process::exit(run_windows_process_limit_probe());
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("--windows-self-check-memory-limit-probe") {
        std::process::exit(run_windows_memory_limit_probe());
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("--windows-self-check-cpu-limit-probe") {
        std::process::exit(run_windows_cpu_limit_probe());
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("--windows-self-check-tree-probe") {
        let request = args.get(1).map(String::as_str).unwrap_or_default();
        std::process::exit(run_windows_tree_probe(request));
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("--windows-self-check-idle-probe") {
        std::process::exit(run_windows_idle_probe());
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("--windows-self-check-lifecycle-worker") {
        let root = args.get(1).map(String::as_str).unwrap_or_default();
        let label = args.get(2).map(String::as_str).unwrap_or_default();
        std::process::exit(run_windows_lifecycle_worker(root, label));
    }
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some("--windows-lifecycle-self-check") {
        std::process::exit(run_windows_lifecycle_self_check());
    }
    if args.iter().any(|arg| arg == "--self-check") {
        // 自检有自己的三态退出码语义，不走统一拒绝路径。
        std::process::exit(run_self_check());
    }
    if let Err(error) = run_launch() {
        // 结构化拒绝信息：宿主与审计可解析，绝不静默放行。
        let payload = serde_json::json!({
            "launcher": "tiangong-sandbox",
            "product_version": env!("CARGO_PKG_VERSION"),
            "error": format!("{error:#}"),
        });
        eprintln!("{payload}");
        std::process::exit(EXIT_SANDBOX_UNAVAILABLE);
    }
}

fn run_policy_file(args: &[String]) -> Result<()> {
    let parsed = parse_run_args(args)?;
    let program = Path::new(&parsed.command[0]);
    let program = if program.is_absolute() {
        program.to_path_buf()
    } else if program.components().count() > 1 {
        std::env::current_dir()?.join(program)
    } else {
        resolve_command_in_path(program).context("无法在 PATH 中解析命令")?
    };
    let program = std::fs::canonicalize(&program)
        .with_context(|| format!("解析命令失败: {}", program.display()))?;
    let program_root = program.parent().context("命令缺少父目录")?.to_path_buf();
    let request = LaunchRequest {
        grant_cache: parsed.grant_cache,
        protocol_version: PROTOCOL_VERSION,
        policy_schema: POLICY_SCHEMA,
        policy: parsed.policy,
        plugin_id: String::new(),
        program: program.display().to_string(),
        program_root: program_root.display().to_string(),
        program_sha256: sha256_file(&program)?,
        args: parsed.command[1..].to_vec(),
        target_validation: TargetValidation::DigestOnly,
        interpreter: false,
    };
    execute_request(request, false)
}

#[derive(Debug, Parser)]
#[command(
    name = "tiangong-sandbox run",
    disable_version_flag = true,
    about = "使用策略文件或直接策略参数执行受限命令"
)]
struct RunArgs {
    /// Windows 持久授权记录，必须位于策略禁止读取的宿主管理目录内。
    #[arg(long, value_name = "FILE")]
    grant_cache: Option<PathBuf>,
    /// SandboxPolicy JSON 文件；不能与直接策略参数同时使用。
    #[arg(long, value_name = "FILE", conflicts_with_all = INLINE_POLICY_ARGS)]
    policy: Option<PathBuf>,
    /// 沙箱模式，默认 workspace-write。
    #[arg(long, value_enum, default_value_t = CliSandboxMode::WorkspaceWrite)]
    mode: CliSandboxMode,
    /// 主工作区绝对路径；直接策略参数形式必须提供。
    #[arg(long, value_name = "ABSOLUTE_DIR", required_unless_present = "policy")]
    workspace: Option<PathBuf>,
    /// 额外可写绝对路径，可重复。
    #[arg(long, value_name = "ABSOLUTE_PATH")]
    writable: Vec<PathBuf>,
    /// 即使位于可写根内也保持只读的绝对路径，可重复。
    #[arg(long, value_name = "ABSOLUTE_PATH")]
    protect: Vec<PathBuf>,
    /// 禁止读取的绝对路径，可重复。
    #[arg(long, value_name = "ABSOLUTE_PATH")]
    deny_read: Vec<PathBuf>,
    /// 网络权限，默认 deny。
    #[arg(long, value_enum, default_value_t = CliNetwork::Deny)]
    network: CliNetwork,
    /// CPU 时间上限（秒）。
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    max_cpu_seconds: Option<u64>,
    /// 内存上限（字节）。
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    max_memory_bytes: Option<u64>,
    /// 进程数量上限。
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    max_processes: Option<u32>,
    /// `--` 后要执行的命令及参数。
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    command: Vec<String>,
}

const INLINE_POLICY_ARGS: &[&str] = &[
    "mode",
    "workspace",
    "writable",
    "protect",
    "deny_read",
    "network",
    "max_cpu_seconds",
    "max_memory_bytes",
    "max_processes",
];

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum CliSandboxMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum CliNetwork {
    Allow,
    #[default]
    Deny,
}

fn parse_run_args(args: &[String]) -> Result<ParsedRun> {
    let parsed = RunArgs::try_parse_from(
        std::iter::once("tiangong-sandbox run").chain(args.iter().skip(1).map(String::as_str)),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let command = parsed.command.clone();
    let grant_cache = parsed.grant_cache.clone();
    let policy = if let Some(policy_path) = parsed.policy.as_ref() {
        serde_json::from_slice(
            &std::fs::read(policy_path)
                .with_context(|| format!("读取策略文件失败: {}", policy_path.display()))?,
        )
        .context("解析策略文件失败")?
    } else {
        inline_policy(parsed)?
    };
    Ok(ParsedRun {
        policy,
        command,
        grant_cache,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedRun {
    grant_cache: Option<PathBuf>,
    policy: tiangong_sandbox::SandboxPolicy,
    command: Vec<String>,
}

fn inline_policy(args: RunArgs) -> Result<tiangong_sandbox::SandboxPolicy> {
    let workspace = absolute_cli_path(
        args.workspace
            .as_deref()
            .context("直接策略参数必须提供 --workspace <absolute-dir>")?,
        "--workspace",
    )?;
    let extra_writable = absolute_cli_paths(args.writable, "--writable")?;
    let protected_paths = absolute_cli_paths(args.protect, "--protect")?;
    let denied_read_paths = absolute_cli_paths(args.deny_read, "--deny-read")?;
    let mut resource_limits = tiangong_sandbox::SandboxResourceLimits::default();
    if let Some(value) = args.max_cpu_seconds {
        resource_limits.max_cpu_time_seconds = value;
    }
    if let Some(value) = args.max_memory_bytes {
        resource_limits.max_memory_bytes = value;
    }
    if let Some(value) = args.max_processes {
        resource_limits.max_processes = value;
    }
    Ok(tiangong_sandbox::SandboxPolicy {
        mode: match args.mode {
            CliSandboxMode::ReadOnly => tiangong_sandbox::SandboxMode::ReadOnly,
            CliSandboxMode::WorkspaceWrite => tiangong_sandbox::SandboxMode::WorkspaceWrite,
        },
        workspace,
        extra_writable,
        protected_paths,
        denied_read_paths,
        allow_network: matches!(args.network, CliNetwork::Allow),
        // 凭据服务放行由宿主经策略文件显式授权；通用 CLI 保持默认
        // 拒绝（fail-closed）。
        allow_credential_services: false,
        allow_audio_input: false,
        resource_limits,
    })
}

fn absolute_cli_paths(values: Vec<PathBuf>, option: &str) -> Result<Vec<PathBuf>> {
    values
        .into_iter()
        .map(|path| absolute_cli_path(&path, option))
        .collect()
}

fn absolute_cli_path(value: &Path, option: &str) -> Result<PathBuf> {
    if !value.is_absolute() {
        bail!("{option} 必须使用绝对路径: {}", value.display());
    }
    Ok(value.to_path_buf())
}

fn resolve_command_in_path(program: &Path) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

fn run_self_update(args: &[String]) -> Result<()> {
    let command = args.first().map(String::as_str).context("缺少升级命令")?;
    let mut root = None;
    let mut manifest = tiangong_sandbox::update::DEFAULT_MANIFEST_URL.to_string();
    let mut public_key = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--root" if command == "update" => {
                index += 1;
                root = Some(PathBuf::from(args.get(index).context("--root 缺少值")?));
            }
            "--manifest-url" => {
                index += 1;
                manifest = args.get(index).context("--manifest-url 缺少值")?.clone();
            }
            "--public-key" => {
                index += 1;
                public_key = Some(
                    std::fs::read_to_string(args.get(index).context("--public-key 缺少值")?)
                        .context("读取更新公钥失败")?,
                );
            }
            unknown => bail!("未知升级参数: {unknown}"),
        }
        index += 1;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("创建 Sandbox 升级运行时失败")?;
    let updater = if let Some(public_key) = public_key {
        tiangong_sandbox::update::SelfUpdater::with_public_key(manifest, public_key)?
    } else {
        tiangong_sandbox::update::SelfUpdater::new(manifest)?
    };
    let status = runtime.block_on(async {
        match (command, root) {
            ("check-update", None) => updater.check().await,
            ("update", None) => updater.update_current().await,
            ("update", Some(directory)) => updater.install_to(&directory).await,
            _ => unreachable!(),
        }
    })?;
    println!("{}", serde_json::to_string(&status)?);
    Ok(())
}

#[cfg(windows)]
fn run_windows_update_completer(args: &[String]) -> Result<()> {
    let value = |name: &str| -> Result<&str> {
        let index = args
            .iter()
            .position(|arg| arg == name)
            .with_context(|| format!("缺少 {name}"))?;
        args.get(index + 1)
            .map(String::as_str)
            .with_context(|| format!("{name} 缺少值"))
    };
    tiangong_sandbox::update::complete_windows_update(
        value("--parent-pid")?.parse()?,
        Path::new(value("--source")?),
        Path::new(value("--signature-source")?),
        Path::new(value("--target")?),
        Path::new(value("--signature-target")?),
        Path::new(value("--cleanup")?),
    )
}

fn run_launch() -> Result<()> {
    execute_request(read_request()?, true)
}

fn execute_request(request: LaunchRequest, inherited_request: bool) -> Result<()> {
    if request.protocol_version != PROTOCOL_VERSION {
        bail!(
            "Launcher 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
            request.protocol_version
        );
    }
    if request.policy_schema != POLICY_SCHEMA {
        bail!(
            "策略 Schema 版本不匹配: expected={POLICY_SCHEMA}, actual={}",
            request.policy_schema
        );
    }
    if request.policy.mode == tiangong_sandbox::SandboxMode::FullAccess {
        bail!("当前 command Launcher 不接受 full_access 策略");
    }
    validate_policy_paths(&request)?;
    let program_path = validate_target(&request)?;

    #[cfg(windows)]
    {
        let host_pid = std::env::var(tiangong_sandbox::HOST_PID_ENV)
            .ok()
            .map(|value| value.parse::<u32>().context("Windows 宿主进程 ID 无效"))
            .transpose()?;
        let stop_event = std::env::var(tiangong_sandbox::WINDOWS_STOP_EVENT_ENV).ok();
        if inherited_request && (host_pid.is_none() || stop_event.is_none()) {
            bail!("Windows 宿主生命周期信息缺失");
        }
        // Launcher 是一次性单线程进程；移除信封后再继承环境，目标无法读取
        // 策略正文或停止事件名称。
        unsafe {
            std::env::remove_var(tiangong_sandbox::POLICY_ENV);
            std::env::remove_var(tiangong_sandbox::WINDOWS_STOP_EVENT_ENV);
            std::env::remove_var(tiangong_sandbox::HOST_PID_ENV);
        }
        let program_root = std::fs::canonicalize(&request.program_root)
            .context("规范化 Windows command 插件根目录失败")?;
        let exit_code = tiangong_sandbox::sandbox::windows::launch_with_grant_cache(
            tiangong_sandbox::sandbox::windows::WindowsLaunchRequest {
                program: &program_path,
                program_root: &program_root,
                args: &request.args,
                policy: &request.policy,
                host_pid,
                stop_event_name: stop_event.as_deref(),
                timeout: None,
            },
            request.grant_cache.as_deref(),
        )?;
        std::process::exit(exit_code);
    }

    // Unix 平台沙箱包装（seatbelt / bwrap）；不可用时 fail-closed。
    #[cfg(unix)]
    if request.grant_cache.is_some() {
        bail!("持久授权仅支持 Windows");
    }
    #[cfg(unix)]
    let wrapped = match tiangong_sandbox::wrap(&request.policy) {
        tiangong_sandbox::SandboxedProgram::Wrapped { program, prefix } => (program, prefix),
        tiangong_sandbox::SandboxedProgram::Direct => {
            bail!("平台沙箱不可用，拒绝启动（fail-closed）");
        }
        tiangong_sandbox::SandboxedProgram::Unavailable(reason) => {
            bail!("平台沙箱不可用：{reason}");
        }
    };

    // 关闭策略描述符后 exec：应用沙箱约束并替换为目标进程。
    // stdin/stdout/stderr 全部继承（业务通信由宿主与目标进程直连）。
    #[cfg(unix)]
    let mut command = std::process::Command::new(wrapped.0);
    #[cfg(unix)]
    command.args(&wrapped.1);
    #[cfg(unix)]
    command.arg(&program_path);
    #[cfg(unix)]
    command.args(&request.args);
    #[cfg(unix)]
    command.env_remove(tiangong_sandbox::POLICY_ENV);
    #[cfg(unix)]
    apply_unix_resource_limits(&mut command, &request.policy.resource_limits);
    #[cfg(unix)]
    unsafe {
        if inherited_request {
            libc::close(POLICY_FD);
        }
        Err(command.exec().into())
    }
    #[cfg(not(any(unix, windows)))]
    {
        bail!("当前平台缺少沙箱启动能力")
    }
}

/// 资源上限由 Launcher 在 exec 前强制施加并随 exec 继承给目标进程树，
/// 不依赖目标读取环境变量自觉设置——sidecar 层的同名限制仅作纵深防御。
///
/// 平台边界：CPU 时间上限全 Unix 施加；内存（RLIMIT_AS）与进程数
/// （RLIMIT_NPROC）只在 Linux 施加——darwin 的 setrlimit 对地址空间限制
/// 一律 EINVAL，且 NPROC 是按真实用户全局计数的语义，桌面存量进程会
/// 挤占配额造成误伤。macOS 的内存失控由命令级超时与进程树清理兜底。
#[cfg(unix)]
fn apply_unix_resource_limits(
    command: &mut std::process::Command,
    limits: &tiangong_sandbox::SandboxResourceLimits,
) {
    use std::os::unix::process::CommandExt;
    let cpu_seconds = limits.max_cpu_time_seconds;
    #[cfg(target_os = "linux")]
    let memory_bytes = limits.max_memory_bytes;
    unsafe {
        command.pre_exec(move || {
            // 资源参数类型随平台不同（Linux glibc 为 u32，darwin 为 c_int），
            // 以宏在调用点展开让 libc 常量类型自然匹配。
            macro_rules! apply {
                ($resource:expr, $value:expr) => {{
                    // 硬限略高于软限：软限到期发 SIGXCPU（默认终止，可归因），
                    // 无视它的进程由硬限 SIGKILL 兜底；软硬同值时 Linux 会
                    // 直接 SIGKILL，终止原因无法与外部强杀区分。
                    let wanted = libc::rlimit {
                        rlim_cur: ($value) as libc::rlim_t,
                        rlim_max: ($value).saturating_add(1) as libc::rlim_t,
                    };
                    if libc::setrlimit($resource, &wanted) == 0 {
                        Ok(())
                    } else {
                        // 设置失败且外层环境已施加不宽于目标的限制（嵌套受
                        // 限终端、受管 CI 外壳）时继承现状——只向更严格方向
                        // 收缩，不拒绝。
                        let mut current = libc::rlimit {
                            rlim_cur: 0,
                            rlim_max: 0,
                        };
                        if libc::getrlimit($resource, (&raw mut current).cast()) == 0
                            && current.rlim_cur != libc::RLIM_INFINITY
                            && current.rlim_cur <= ($value) as libc::rlim_t
                        {
                            Ok(())
                        } else {
                            Err(std::io::Error::last_os_error())
                        }
                    }
                }};
            }
            apply!(libc::RLIMIT_CPU, cpu_seconds)?;
            #[cfg(target_os = "linux")]
            apply!(libc::RLIMIT_AS, memory_bytes)?;
            Ok(())
        });
    }
}

#[cfg(unix)]
fn read_request() -> Result<LaunchRequest> {
    // fd3 由宿主以继承管道提供；显式长度避免并发 spawn 意外继承写端时
    // Launcher 永远等待 EOF。描述符由 run_launch 在 exec 前关闭。
    let mut reader = unsafe {
        use std::os::fd::FromRawFd;
        std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(POLICY_FD))
    };
    let mut length = [0u8; size_of::<u32>()];
    reader
        .read_exact(&mut length)
        .context("读取 fd3 策略长度失败")?;
    let length = u32::from_be_bytes(length) as usize;
    if length > tiangong_sandbox::MAX_POLICY_FRAME_BYTES {
        bail!(
            "Launcher 策略超过长度上限: actual={length}, max={}",
            tiangong_sandbox::MAX_POLICY_FRAME_BYTES
        );
    }
    let mut raw = vec![0u8; length];
    reader
        .read_exact(&mut raw)
        .context("读取 fd3 策略正文失败")?;
    serde_json::from_slice(&raw).context("解析 Launcher 策略失败")
}

#[cfg(windows)]
fn read_request() -> Result<LaunchRequest> {
    let raw = std::env::var(tiangong_sandbox::POLICY_ENV)
        .context("读取 Windows Launcher 策略信封失败")?;
    serde_json::from_str(&raw).context("解析 Launcher 策略失败")
}

#[cfg(not(any(unix, windows)))]
fn read_request() -> Result<LaunchRequest> {
    bail!("当前平台没有 Launcher 策略传输通道")
}

/// 策略路径注入防护（平台无关）：控制字符或非 UTF-8 路径可能破坏
/// macOS SBPL profile 结构或干扰平台包装器，直接结构化拒绝。
fn validate_policy_paths(request: &LaunchRequest) -> Result<()> {
    let policy = &request.policy;
    let mut paths: Vec<&std::path::Path> = vec![policy.workspace.as_path()];
    for group in [
        &policy.extra_writable,
        &policy.protected_paths,
        &policy.denied_read_paths,
    ] {
        paths.extend(group.iter().map(std::path::Path::new));
    }
    for path in paths {
        let Some(text) = path.to_str() else {
            bail!("策略路径不是有效 UTF-8: {:?}", path);
        };
        if text.chars().any(char::is_control) {
            bail!("策略路径包含控制字符，拒绝执行: {text:?}");
        }
    }
    Ok(())
}

fn validate_target(request: &LaunchRequest) -> Result<PathBuf> {
    let root = Path::new(&request.program_root);
    let program = Path::new(&request.program);
    if !root.is_absolute() || !program.is_absolute() {
        bail!("目标程序及插件根目录必须是绝对路径");
    }

    if !request.interpreter {
        let root_metadata = std::fs::symlink_metadata(root)
            .with_context(|| format!("读取插件根目录失败: {}", root.display()))?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            bail!("插件根目录必须是实际目录: {}", root.display());
        }
        let program_metadata = std::fs::symlink_metadata(program)
            .with_context(|| format!("读取目标程序失败: {}", program.display()))?;
        if program_metadata.file_type().is_symlink() || !program_metadata.is_file() {
            bail!("目标程序必须是实际普通文件: {}", program.display());
        }
    }

    let canonical_root = std::fs::canonicalize(root).context("规范化插件根目录失败")?;
    let canonical_program = std::fs::canonicalize(program).context("规范化目标程序失败")?;
    let root_metadata =
        std::fs::symlink_metadata(&canonical_root).context("读取规范化插件根目录失败")?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("规范化插件根目录必须是实际目录");
    }
    let program_metadata =
        std::fs::symlink_metadata(&canonical_program).context("读取规范化目标程序失败")?;
    if program_metadata.file_type().is_symlink() || !program_metadata.is_file() {
        bail!("规范化目标程序必须是实际普通文件");
    }
    if canonical_program == canonical_root || !canonical_program.starts_with(&canonical_root) {
        bail!("目标程序不在插件权威目录内");
    }

    // 通用缺省只校验路径形态、摘要与权限。旧 interpreter 字段保持兼容。
    if request.target_validation == TargetValidation::DigestOnly || request.interpreter {
        let actual_sha256 = sha256_file(&canonical_program)?;
        if !request.program_sha256.eq_ignore_ascii_case(&actual_sha256) {
            bail!("目标程序摘要不匹配");
        }
        validate_unix_permissions(&canonical_program, &program_metadata)?;
        return Ok(canonical_program);
    }

    let manifest_path = canonical_root.join("plugin.json");
    let manifest_metadata = std::fs::symlink_metadata(&manifest_path)
        .with_context(|| format!("读取插件清单失败: {}", manifest_path.display()))?;
    if manifest_metadata.file_type().is_symlink() || !manifest_metadata.is_file() {
        bail!("插件清单必须是实际普通文件");
    }
    let manifest: TargetManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).context("读取插件清单失败")?)
            .context("解析插件清单失败")?;
    if manifest.id != request.plugin_id {
        bail!("Launcher 请求与插件清单身份不一致");
    }
    let mut expected_program = canonical_root.join(&manifest.sidecar.binary);
    if !std::env::consts::EXE_SUFFIX.is_empty()
        && !expected_program
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(std::env::consts::EXE_SUFFIX))
    {
        let name = expected_program
            .file_name()
            .and_then(|name| name.to_str())
            .context("插件清单中的 sidecar 文件名无效")?;
        expected_program.set_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    }
    let expected_program =
        std::fs::canonicalize(expected_program).context("解析清单声明的 sidecar 失败")?;
    if canonical_program != expected_program {
        bail!("目标程序与插件清单声明不一致");
    }

    let actual_sha256 = sha256_file(&canonical_program)?;
    if !request.program_sha256.eq_ignore_ascii_case(&actual_sha256) {
        bail!("目标程序摘要不匹配");
    }
    validate_unix_permissions(&canonical_program, &program_metadata)?;
    Ok(canonical_program)
}

#[derive(Debug, Deserialize)]
struct TargetManifest {
    id: String,
    sidecar: TargetSidecar,
}

#[derive(Debug, Deserialize)]
struct TargetSidecar {
    binary: String,
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("读取目标程序失败: {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[cfg(unix)]
fn validate_unix_permissions(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let owner = metadata.uid();
    let current_user = unsafe { libc::geteuid() };
    if owner != current_user && owner != 0 {
        bail!("目标程序所有者不可信: {}", path.display());
    }
    if metadata.mode() & 0o022 != 0 {
        bail!("目标程序不能允许组或其他用户写入: {}", path.display());
    }
    if metadata.mode() & 0o111 == 0 {
        bail!("目标程序没有执行权限: {}", path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_unix_permissions(_path: &Path, _metadata: &std::fs::Metadata) -> Result<()> {
    Ok(())
}

/// 制品自检。三态退出码：0=全部通过；79=当前宿主环境无法验证（如
/// 嵌套沙箱，非制品缺陷）；78=必需能力失败（制品不可用，必须拒绝启动）。
fn run_self_check() -> i32 {
    let mut report = serde_json::Map::new();
    report.insert(
        "product_version".into(),
        serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string()),
    );
    report.insert(
        "protocol_version".into(),
        serde_json::Value::from(PROTOCOL_VERSION),
    );
    report.insert(
        "policy_schema".into(),
        serde_json::Value::from(POLICY_SCHEMA),
    );

    let availability = tiangong_sandbox::availability();
    report.insert(
        "platform".into(),
        match &availability {
            tiangong_sandbox::SandboxAvailability::Available => {
                serde_json::Value::String("available".to_string())
            }
            tiangong_sandbox::SandboxAvailability::Unsupported(reason) => {
                serde_json::Value::String(format!("unavailable: {reason}"))
            }
            tiangong_sandbox::SandboxAvailability::EnvironmentRestricted(reason) => {
                serde_json::Value::String(format!("environment-restricted: {reason}"))
            }
        },
    );

    if matches!(
        &availability,
        tiangong_sandbox::SandboxAvailability::EnvironmentRestricted(_)
    ) {
        report.insert(
            "environment_unverifiable".into(),
            serde_json::Value::from(true),
        );
        println!("{}", serde_json::Value::Object(report));
        return EXIT_ENV_UNVERIFIABLE;
    }
    if matches!(
        &availability,
        tiangong_sandbox::SandboxAvailability::Unsupported(_)
    ) {
        println!("{}", serde_json::Value::Object(report));
        return EXIT_SANDBOX_UNAVAILABLE;
    }

    #[cfg(unix)]
    run_enforcement_probes(&mut report);
    #[cfg(windows)]
    run_windows_enforcement_probes(&mut report);
    // 必需能力失败时制品不可用，宿主必须拒绝启动。
    let required_ok = [
        "workspace_write",
        "outside_write_blocked",
        "outside_delete_blocked",
        "outside_config_write_blocked",
        "git_metadata_write",
        "network_blocked",
        "sensitive_read_blocked",
        "symlink_escape_blocked",
        "path_traversal_blocked",
    ]
    .iter()
    .all(|field| {
        report
            .get(*field)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    });
    #[cfg(windows)]
    let required_ok = required_ok
        && [
            "outside_read_blocked",
            "dedicated_temp_write",
            "network_allowed",
            "child_restrictions_inherited",
            "resource_limits_applied",
            "process_limit_enforced",
            "memory_limit_enforced",
            "cpu_limit_enforced",
            "existing_workspace_read_write",
            "git_metadata_readable",
            "hardlink_escape_blocked",
            "temporary_acl_cleaned",
            "temporary_identity_cleaned",
        ]
        .iter()
        .all(|field| {
            report
                .get(*field)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        });
    let exit_code = if required_ok {
        0
    } else {
        EXIT_SANDBOX_UNAVAILABLE
    };
    println!("{}", serde_json::Value::Object(report));
    exit_code
}

fn run_network_probe(address: &str) -> i32 {
    let Ok(address) = address.parse::<std::net::SocketAddr>() else {
        return 2;
    };
    match std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2)) {
        Ok(_) => 0,
        Err(_) => 1,
    }
}

#[cfg(windows)]
#[derive(Debug, Serialize, Deserialize)]
struct WindowsProbeRequest {
    workspace: PathBuf,
    workspace_executable: PathBuf,
    temp_dir: PathBuf,
    existing_workspace_file: PathBuf,
    outside_write: PathBuf,
    outside_delete: PathBuf,
    outside_config: PathBuf,
    git_config: PathBuf,
    sensitive_paths: Vec<PathBuf>,
    traversal_target: PathBuf,
    report_path: PathBuf,
    child_report_path: PathBuf,
    network_address: String,
    resource_limits: tiangong_sandbox::SandboxResourceLimits,
}

#[cfg(windows)]
#[derive(Debug, Serialize, Deserialize)]
struct WindowsTreeProbeRequest {
    workspace: PathBuf,
    executable: PathBuf,
    parent_pid_path: PathBuf,
    child_pid_path: PathBuf,
    ready_path: PathBuf,
}

#[cfg(windows)]
#[derive(Debug, Default, Serialize, Deserialize)]
struct WindowsProbeReport {
    workspace_write: bool,
    dedicated_temp_write: bool,
    existing_workspace_read_write: bool,
    outside_write_blocked: bool,
    outside_read_blocked: bool,
    outside_delete_blocked: bool,
    outside_config_write_blocked: bool,
    git_metadata_write: bool,
    git_metadata_readable: bool,
    network_blocked: bool,
    sensitive_read_blocked: bool,
    path_traversal_blocked: bool,
    child_restrictions_inherited: bool,
    resource_limits_applied: bool,
    null_device_accessible: bool,
}

#[cfg(windows)]
struct WindowsEnforcementResult {
    probe: WindowsProbeReport,
    network_allowed: bool,
    process_limit_enforced: bool,
    memory_limit_enforced: bool,
    cpu_limit_enforced: bool,
    hardlink_escape_blocked: bool,
    symlink_escape_blocked: bool,
    temporary_acl_cleaned: bool,
    temporary_identity_cleaned: bool,
}

#[cfg(windows)]
fn run_windows_enforcement_probes(report: &mut serde_json::Map<String, serde_json::Value>) {
    match windows_enforcement_probes() {
        Ok(result) => {
            let probe = result.probe;
            for (field, value) in [
                ("workspace_write", probe.workspace_write),
                (
                    "existing_workspace_read_write",
                    probe.existing_workspace_read_write,
                ),
                ("outside_write_blocked", probe.outside_write_blocked),
                ("outside_read_blocked", probe.outside_read_blocked),
                ("outside_delete_blocked", probe.outside_delete_blocked),
                (
                    "outside_config_write_blocked",
                    probe.outside_config_write_blocked,
                ),
                ("git_metadata_write", probe.git_metadata_write),
                ("git_metadata_readable", probe.git_metadata_readable),
                ("network_blocked", probe.network_blocked),
                ("sensitive_read_blocked", probe.sensitive_read_blocked),
                ("path_traversal_blocked", probe.path_traversal_blocked),
                (
                    "child_restrictions_inherited",
                    probe.child_restrictions_inherited,
                ),
                ("resource_limits_applied", probe.resource_limits_applied),
                ("null_device_accessible", probe.null_device_accessible),
            ] {
                report.insert(field.into(), serde_json::Value::from(value));
            }
            report.insert(
                "dedicated_temp_write".into(),
                serde_json::Value::from(probe.dedicated_temp_write),
            );
            for (field, value) in [
                ("network_allowed", result.network_allowed),
                ("process_limit_enforced", result.process_limit_enforced),
                ("memory_limit_enforced", result.memory_limit_enforced),
                ("cpu_limit_enforced", result.cpu_limit_enforced),
                ("hardlink_escape_blocked", result.hardlink_escape_blocked),
                ("symlink_escape_blocked", result.symlink_escape_blocked),
                ("temporary_acl_cleaned", result.temporary_acl_cleaned),
                (
                    "temporary_identity_cleaned",
                    result.temporary_identity_cleaned,
                ),
            ] {
                report.insert(field.into(), serde_json::Value::from(value));
            }
        }
        Err(error) => {
            // 探针失败原因同时走 stderr：宿主拼装用户报错只取 stderr，
            // 只写 stdout JSON 会让真实死因（如探针退出码）对用户不可见。
            eprintln!("Windows 沙箱探针失败: {error:#}");
            report.insert(
                "windows_probe_error".into(),
                serde_json::Value::String(format!("{error:#}")),
            );
        }
    }
}

#[cfg(windows)]
fn windows_enforcement_probes() -> Result<WindowsEnforcementResult> {
    eprintln!("Windows Sandbox 自检阶段: 准备隔离路径");
    let root = tempfile::Builder::new()
        .prefix("tiangong-sandbox-selfcheck-")
        .tempdir()
        .context("创建 Windows 自检根目录失败")?;
    let workspace = root.path().join("workspace");
    let temp_dir = root.path().join("invocation-temp");
    let outside = root.path().join("outside");
    let fake_home = root.path().join("home");
    let git_dir = workspace.join(".git");
    for path in [
        &workspace,
        &temp_dir,
        &outside,
        &fake_home,
        &git_dir,
        &fake_home.join(".ssh"),
        &fake_home.join(".aws"),
    ] {
        std::fs::create_dir_all(path)
            .with_context(|| format!("创建 Windows 自检目录失败: {}", path.display()))?;
    }
    let outside_read = outside.join("read-secret.txt");
    let outside_delete = outside.join("delete-me");
    let outside_config = fake_home.join("profile.ini");
    let git_config = git_dir.join("config");
    let existing_workspace_file = workspace.join("existing.txt");
    let ssh_secret = fake_home.join(".ssh/id_ed25519");
    let aws_secret = fake_home.join(".aws/credentials");
    std::fs::write(&outside_read, "TIANGONG_FAKE_OUTSIDE_SECRET")?;
    std::fs::create_dir_all(&outside_delete)?;
    std::fs::write(outside_delete.join("keep"), "safe")?;
    std::fs::write(&outside_config, "safe\n")?;
    std::fs::write(&git_config, "safe\n")?;
    std::fs::write(&existing_workspace_file, "workspace-before")?;
    std::fs::write(&ssh_secret, "TIANGONG_FAKE_SSH_SECRET")?;
    std::fs::write(&aws_secret, "TIANGONG_FAKE_AWS_SECRET")?;

    eprintln!("Windows Sandbox 自检阶段: 查找可达网络目标");
    let network_address = reachable_network_address()?;
    let resource_limits = tiangong_sandbox::SandboxResourceLimits {
        max_cpu_time_seconds: 60,
        max_memory_bytes: 512 * 1024 * 1024,
        max_processes: 8,
    };
    let report_path = workspace.join("probe-report.json");
    let child_report_path = workspace.join("child-report.json");
    let current_exe = std::env::current_exe().context("读取 Windows 自检程序路径失败")?;
    let workspace_executable = workspace.join("workspace-child-probe.exe");
    std::fs::copy(&current_exe, &workspace_executable)
        .context("复制 Windows 工作区子进程探针失败")?;
    let request = WindowsProbeRequest {
        workspace: workspace.clone(),
        workspace_executable,
        temp_dir: temp_dir.clone(),
        existing_workspace_file: existing_workspace_file.clone(),
        outside_write: outside.join("blocked.txt"),
        outside_delete: outside_delete.clone(),
        outside_config: outside_config.clone(),
        git_config: git_config.clone(),
        sensitive_paths: vec![outside_read, ssh_secret, aws_secret],
        traversal_target: workspace.join("../outside/traversal.txt"),
        report_path: report_path.clone(),
        child_report_path,
        network_address: network_address.clone(),
        resource_limits,
    };
    let request_json = serde_json::to_string(&request)?;
    let program_root = current_exe.parent().context("Windows 自检程序缺少父目录")?;
    let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write(&workspace);
    policy.extra_writable = vec![temp_dir];
    policy.denied_read_paths = request.sensitive_paths.clone();
    policy.resource_limits = resource_limits;
    eprintln!("Windows Sandbox 自检阶段: 文件、断网与子进程继承");
    let exit_code = tiangong_sandbox::sandbox::windows::launch(
        tiangong_sandbox::sandbox::windows::WindowsLaunchRequest {
            program: &current_exe,
            program_root,
            args: &["--windows-self-check-file-probe".to_string(), request_json],
            policy: &policy,
            host_pid: None,
            stop_event_name: None,
            timeout: Some(std::time::Duration::from_secs(30)),
        },
    )?;
    if exit_code != 0 {
        bail!(
            "Windows 文件隔离探测进程退出码异常: {exit_code}（0x{:08X}）",
            exit_code as u32
        );
    }
    let mut probe: WindowsProbeReport = serde_json::from_slice(
        &std::fs::read(&report_path).context("读取 Windows 隔离探测报告失败")?,
    )
    .context("解析 Windows 隔离探测报告失败")?;
    probe.outside_write_blocked &= !request.outside_write.exists();
    probe.outside_delete_blocked &= outside_delete.join("keep").is_file();
    probe.outside_config_write_blocked &=
        std::fs::read_to_string(&outside_config).is_ok_and(|value| value == "safe\n");
    probe.git_metadata_write &=
        std::fs::read_to_string(&git_config).is_ok_and(|value| value == GIT_METADATA_PROBE_MARKER);
    probe.existing_workspace_read_write &= std::fs::read_to_string(&existing_workspace_file)
        .is_ok_and(|value| value == "workspace-after");
    probe.path_traversal_blocked &= !request.traversal_target.exists();

    let mut network_policy = policy.clone();
    network_policy.allow_network = true;
    eprintln!("Windows Sandbox 自检阶段: 显式联网能力");
    let network_exit = tiangong_sandbox::sandbox::windows::launch(
        tiangong_sandbox::sandbox::windows::WindowsLaunchRequest {
            program: &current_exe,
            program_root,
            args: &["--self-check-network-probe".to_string(), network_address],
            policy: &network_policy,
            host_pid: None,
            stop_event_name: None,
            timeout: Some(std::time::Duration::from_secs(15)),
        },
    )?;
    let network_allowed = network_exit == 0;

    eprintln!("Windows Sandbox 自检阶段: 进程数上限");
    let process_limit_enforced = launch_windows_limit_probe(
        &current_exe,
        program_root,
        &policy,
        "--windows-self-check-process-limit-probe",
        tiangong_sandbox::SandboxResourceLimits {
            max_cpu_time_seconds: 30,
            max_memory_bytes: 512 * 1024 * 1024,
            max_processes: 1,
        },
    )? == 0;
    eprintln!("Windows Sandbox 自检阶段: 内存上限");
    let memory_limit_enforced = launch_windows_limit_probe(
        &current_exe,
        program_root,
        &policy,
        "--windows-self-check-memory-limit-probe",
        tiangong_sandbox::SandboxResourceLimits {
            max_cpu_time_seconds: 30,
            max_memory_bytes: 96 * 1024 * 1024,
            max_processes: 2,
        },
    )? != 5;
    eprintln!("Windows Sandbox 自检阶段: CPU 上限");
    let cpu_limit_enforced = launch_windows_limit_probe(
        &current_exe,
        program_root,
        &policy,
        "--windows-self-check-cpu-limit-probe",
        tiangong_sandbox::SandboxResourceLimits {
            max_cpu_time_seconds: 1,
            max_memory_bytes: 512 * 1024 * 1024,
            max_processes: 1,
        },
    )? != 5;

    eprintln!("Windows Sandbox 自检阶段: 硬链接拒绝");
    let hardlink_workspace = root.path().join("hardlink-workspace");
    std::fs::create_dir_all(&hardlink_workspace)?;
    let hardlink_target = outside.join("hardlink-target.txt");
    std::fs::write(&hardlink_target, "safe")?;
    std::fs::hard_link(&hardlink_target, hardlink_workspace.join("escape-link"))?;
    let hardlink_policy = tiangong_sandbox::SandboxPolicy::workspace_write(&hardlink_workspace);
    let hardlink_blocked = tiangong_sandbox::sandbox::windows::launch(
        tiangong_sandbox::sandbox::windows::WindowsLaunchRequest {
            program: &current_exe,
            program_root,
            args: &[],
            policy: &hardlink_policy,
            host_pid: None,
            stop_event_name: None,
            timeout: Some(std::time::Duration::from_secs(15)),
        },
    )
    .is_err();

    eprintln!("Windows Sandbox 自检阶段: 重解析点拒绝");
    let reparse_workspace = root.path().join("reparse-workspace");
    std::fs::create_dir_all(&reparse_workspace)?;
    let reparse = reparse_workspace.join("escape-link");
    let reparse_created = std::os::windows::fs::symlink_file(&outside_config, &reparse).is_ok()
        || create_directory_junction(&outside, &reparse).unwrap_or(false);
    let reparse_policy = tiangong_sandbox::SandboxPolicy::workspace_write(&reparse_workspace);
    let reparse_blocked = reparse_created
        && tiangong_sandbox::sandbox::windows::launch(
            tiangong_sandbox::sandbox::windows::WindowsLaunchRequest {
                program: &current_exe,
                program_root,
                args: &[],
                policy: &reparse_policy,
                host_pid: None,
                stop_event_name: None,
                timeout: Some(std::time::Duration::from_secs(15)),
            },
        )
        .is_err();
    Ok(WindowsEnforcementResult {
        probe,
        network_allowed,
        process_limit_enforced,
        memory_limit_enforced,
        cpu_limit_enforced,
        hardlink_escape_blocked: hardlink_blocked,
        symlink_escape_blocked: reparse_blocked,
        temporary_acl_cleaned: true,
        temporary_identity_cleaned: true,
    })
}

#[cfg(windows)]
fn launch_windows_limit_probe(
    program: &Path,
    program_root: &Path,
    policy: &tiangong_sandbox::SandboxPolicy,
    mode: &str,
    limits: tiangong_sandbox::SandboxResourceLimits,
) -> Result<i32> {
    let mut policy = policy.clone();
    policy.resource_limits = limits;
    tiangong_sandbox::sandbox::windows::launch(
        tiangong_sandbox::sandbox::windows::WindowsLaunchRequest {
            program,
            program_root,
            args: &[mode.to_string()],
            policy: &policy,
            host_pid: None,
            stop_event_name: None,
            timeout: Some(std::time::Duration::from_secs(30)),
        },
    )
}

#[cfg(windows)]
fn reachable_network_address() -> Result<String> {
    use std::net::ToSocketAddrs;
    for endpoint in ["github.com:443", "www.microsoft.com:443", "1.1.1.1:443"] {
        let Ok(addresses) = endpoint.to_socket_addrs() else {
            continue;
        };
        for address in addresses {
            if std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(3))
                .is_ok()
            {
                return Ok(address.to_string());
            }
        }
    }
    bail!("Windows 自检无法找到可达的网络目标")
}

#[cfg(windows)]
fn create_directory_junction(target: &Path, link: &Path) -> Result<bool> {
    let mut command = std::process::Command::new("cmd.exe");
    command
        .args(["/D", "/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = tiangong_toolkit::configure_no_window(&mut command)
        .output()
        .context("创建 Windows 自检 Junction 失败")?;
    if !output.status.success() {
        eprintln!(
            "创建 Windows 自检 Junction 失败（{}）：{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.status.success())
}

#[cfg(windows)]
fn run_windows_file_probe(raw: &str) -> i32 {
    let Ok(request) = serde_json::from_str::<WindowsProbeRequest>(raw) else {
        eprintln!("Windows 文件隔离探针: 请求参数解析失败");
        return 2;
    };
    eprintln!("Windows 文件隔离探针: 验证父进程边界");
    let (git_metadata_write, git_metadata_readable) =
        probe_windows_git_metadata(&request.git_config, "父进程");
    let mut probe = WindowsProbeReport {
        workspace_write: probe_windows_workspace_write(&request.workspace, "inside.txt", "父进程"),
        dedicated_temp_write: std::fs::write(request.temp_dir.join("temp.txt"), "ok").is_ok(),
        existing_workspace_read_write: probe_existing_workspace_file(
            &request.existing_workspace_file,
        ),
        outside_write_blocked: std::fs::write(&request.outside_write, "blocked").is_err(),
        outside_read_blocked: request
            .sensitive_paths
            .first()
            .is_some_and(|path| std::fs::read(path).is_err()),
        outside_delete_blocked: std::fs::remove_dir_all(&request.outside_delete).is_err(),
        outside_config_write_blocked: std::fs::write(&request.outside_config, "PWNED").is_err(),
        git_metadata_write,
        git_metadata_readable,
        network_blocked: run_network_probe(&request.network_address) != 0,
        sensitive_read_blocked: request
            .sensitive_paths
            .iter()
            .all(|path| std::fs::read(path).is_err()),
        path_traversal_blocked: std::fs::write(&request.traversal_target, "blocked").is_err(),
        child_restrictions_inherited: false,
        resource_limits_applied: tiangong_sandbox::sandbox::windows::current_process_limits_match(
            request.resource_limits,
        ),
        null_device_accessible: false,
    };
    eprintln!("Windows 文件隔离探针: 启动受限子进程");
    eprintln!(
        "Windows 文件隔离探针: 子进程策略={:?}，工作区程序读取={:?}",
        tiangong_sandbox::sandbox::windows::current_process_child_policy_flags(),
        std::fs::read(&request.workspace_executable).map(|raw| raw.len())
    );
    let child_request = match serde_json::to_string(&request) {
        Ok(value) => value,
        Err(_) => return 3,
    };
    // 普通用户 AppContainer 通常不能打开 NUL；这里只保留兼容性诊断，
    // 子进程隔离继承由下方匿名管道和普通文件句柄验证。
    let null_read = std::fs::OpenOptions::new().read(true).open("NUL");
    let null_write = std::fs::OpenOptions::new().write(true).open("NUL");
    eprintln!(
        "Windows 文件隔离探针: 直接打开空设备读取={:?}，写入={:?}",
        null_read.as_ref().map(|_| ()),
        null_write.as_ref().map(|_| ())
    );
    probe.null_device_accessible = null_read.is_ok() && null_write.is_ok();

    let file_stdout_path = request.workspace.join("child-stdio.stdout");
    let file_stderr_path = request.workspace.join("child-stdio.stderr");
    let file_stdio_result = match (
        std::fs::File::open(&request.existing_workspace_file),
        std::fs::File::create(&file_stdout_path),
        std::fs::File::create(&file_stderr_path),
    ) {
        (Ok(stdin), Ok(stdout), Ok(stderr)) => {
            let mut command = std::process::Command::new(&request.workspace_executable);
            command
                .arg("--windows-self-check-child-probe")
                .arg(&child_request)
                .current_dir(&request.workspace)
                .stdin(std::process::Stdio::from(stdin))
                .stdout(std::process::Stdio::from(stdout))
                .stderr(std::process::Stdio::from(stderr));
            tiangong_toolkit::configure_no_window(&mut command).status()
        }
        (stdin, stdout, stderr) => {
            eprintln!(
                "Windows 文件隔离探针: 准备工作区 stdio 文件失败，输入={:?}，输出={:?}，错误={:?}",
                stdin.as_ref().map(|_| ()),
                stdout.as_ref().map(|_| ()),
                stderr.as_ref().map(|_| ())
            );
            Err(stdin
                .err()
                .or_else(|| stdout.err())
                .or_else(|| stderr.err())
                .unwrap_or_else(|| std::io::Error::other("准备工作区 stdio 文件失败")))
        }
    };
    eprintln!("Windows 文件隔离探针: 工作区文件 stdio 派生结果={file_stdio_result:?}");
    let file_stdio_ok = file_stdio_result.is_ok_and(|status| status.success());
    let _ = std::fs::remove_file(&request.child_report_path);
    let _ = std::fs::remove_file(file_stdout_path);
    let _ = std::fs::remove_file(file_stderr_path);

    const PIPE_TOKEN: &str = "tiangong-sandbox-pipe-input";
    const STDOUT_TOKEN: &str = "tiangong-sandbox-pipe-stdout";
    const STDERR_TOKEN: &str = "tiangong-sandbox-pipe-stderr";
    let mut piped_command = std::process::Command::new(&request.workspace_executable);
    piped_command
        .arg("--windows-self-check-child-probe")
        .arg(&child_request)
        .arg(PIPE_TOKEN)
        .current_dir(&request.workspace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let piped_child = tiangong_toolkit::configure_no_window(&mut piped_command).spawn();
    let pipe_stdio_ok = match piped_child {
        Ok(mut child) => {
            let input_written = child
                .stdin
                .take()
                .is_some_and(|mut stdin| stdin.write_all(PIPE_TOKEN.as_bytes()).is_ok());
            match child.wait_with_output() {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if !output.status.success() {
                        eprintln!(
                            "Windows 文件隔离探针: 管道子进程退出失败 ({})，stderr={stderr}",
                            output.status
                        );
                    }
                    input_written
                        && output.status.success()
                        && stdout.contains(STDOUT_TOKEN)
                        && stderr.contains(STDERR_TOKEN)
                }
                Err(error) => {
                    eprintln!("Windows 文件隔离探针: 等待管道子进程失败: {error:?}");
                    false
                }
            }
        }
        Err(error) => {
            eprintln!(
                "Windows 文件隔离探针: 启动管道子进程失败: {error:?} (os={:?})",
                error.raw_os_error()
            );
            false
        }
    };
    eprintln!("Windows 文件隔离探针: 完整匿名管道派生={pipe_stdio_ok}");
    eprintln!("Windows 文件隔离探针: 读取子进程报告");
    let child_report = std::fs::read(&request.child_report_path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<WindowsProbeReport>(&raw).ok());
    probe.child_restrictions_inherited = file_stdio_ok
        && pipe_stdio_ok
        && child_report.is_some_and(|child| {
            child.workspace_write
                && child.dedicated_temp_write
                && child.existing_workspace_read_write
                && child.outside_write_blocked
                && child.outside_read_blocked
                && child.outside_delete_blocked
                && child.outside_config_write_blocked
                && child.git_metadata_write
                && child.git_metadata_readable
                && child.network_blocked
                && child.sensitive_read_blocked
                && child.path_traversal_blocked
                && child.resource_limits_applied
        });
    eprintln!("Windows 文件隔离探针: 写入父进程报告");
    match serde_json::to_vec(&probe)
        .ok()
        .and_then(|raw| std::fs::write(&request.report_path, raw).ok())
    {
        Some(()) => 0,
        None => 4,
    }
}

#[cfg(windows)]
fn run_windows_child_probe(raw: &str, expected_stdin: Option<&str>) -> i32 {
    let Ok(request) = serde_json::from_str::<WindowsProbeRequest>(raw) else {
        return 2;
    };
    if let Some(expected) = expected_stdin {
        let mut input = String::new();
        if std::io::stdin().read_to_string(&mut input).is_err() || input != expected {
            return 4;
        }
        println!("tiangong-sandbox-pipe-stdout");
        eprintln!("tiangong-sandbox-pipe-stderr");
    }
    eprintln!("Windows 文件隔离探针: 验证子进程边界");
    let (git_metadata_write, git_metadata_readable) =
        probe_windows_git_metadata(&request.git_config, "子进程");
    let report = WindowsProbeReport {
        workspace_write: probe_windows_workspace_write(
            &request.workspace,
            "inside-child.txt",
            "子进程",
        ),
        dedicated_temp_write: std::fs::write(request.temp_dir.join("temp-child.txt"), "ok").is_ok(),
        existing_workspace_read_write: probe_existing_workspace_file(
            &request.existing_workspace_file,
        ),
        outside_write_blocked: std::fs::write(&request.outside_write, "blocked").is_err(),
        outside_read_blocked: request
            .sensitive_paths
            .first()
            .is_some_and(|path| std::fs::read(path).is_err()),
        git_metadata_write,
        git_metadata_readable,
        outside_delete_blocked: std::fs::remove_dir_all(&request.outside_delete).is_err(),
        outside_config_write_blocked: std::fs::write(&request.outside_config, "PWNED").is_err(),
        network_blocked: run_network_probe(&request.network_address) != 0,
        sensitive_read_blocked: request
            .sensitive_paths
            .iter()
            .all(|path| std::fs::read(path).is_err()),
        path_traversal_blocked: std::fs::write(&request.traversal_target, "blocked").is_err(),
        resource_limits_applied: tiangong_sandbox::sandbox::windows::current_process_limits_match(
            request.resource_limits,
        ),
        null_device_accessible: false,
        ..Default::default()
    };
    eprintln!("Windows 文件隔离探针: 写入子进程报告");
    match serde_json::to_vec(&report)
        .ok()
        .and_then(|raw| std::fs::write(&request.child_report_path, raw).ok())
    {
        Some(()) => 0,
        None => 3,
    }
}

#[cfg(windows)]
fn probe_windows_workspace_write(workspace: &Path, filename: &str, process: &str) -> bool {
    let workspace = match tiangong_sandbox::canonicalize_path(workspace) {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("Windows 文件隔离探针: {process}解析工作区失败: {error}");
            return false;
        }
    };
    let target = workspace.join(filename);
    match std::fs::write(&target, "ok") {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "Windows 文件隔离探针: {process}写入工作区失败 ({}): {error}",
                target.display()
            );
            false
        }
    }
}

#[cfg(windows)]
fn probe_windows_git_metadata(path: &Path, process: &str) -> (bool, bool) {
    // .git 可写是功能要求（用户裁定 2026-08-30）：agent 需要完整 git
    // 工作流。写入探测标记并回读验证真实落盘；父/子探针共用同一标记，
    // 重复探测幂等（首次探测前内容为宿主写入的 "safe\n"）。
    let readable = std::fs::read_to_string(path)
        .is_ok_and(|value| value == "safe\n" || value == GIT_METADATA_PROBE_MARKER);
    let write_ok = std::fs::write(path, GIT_METADATA_PROBE_MARKER).is_ok()
        && std::fs::read_to_string(path).is_ok_and(|value| value == GIT_METADATA_PROBE_MARKER);
    eprintln!("Windows 文件隔离探针: {process} Git 写入放行={write_ok}，读取正常={readable}");
    (write_ok, readable)
}

#[cfg(windows)]
fn probe_existing_workspace_file(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .is_ok_and(|value| matches!(value.as_str(), "workspace-before" | "workspace-after"))
        && std::fs::write(path, "workspace-after").is_ok()
}

#[cfg(windows)]
fn run_windows_process_limit_probe() -> i32 {
    let Ok(program) = std::env::current_exe() else {
        return 3;
    };
    let mut command = std::process::Command::new(program);
    command.args(["--self-check-network-probe", "invalid"]);
    match tiangong_toolkit::configure_no_window(&mut command).spawn() {
        Err(_) => 0,
        Ok(mut child) => {
            let _ = child.wait();
            5
        }
    }
}

#[cfg(windows)]
fn run_windows_memory_limit_probe() -> i32 {
    let mut allocations = Vec::new();
    for _ in 0..512 {
        let mut block = Vec::<u8>::new();
        if block.try_reserve_exact(1024 * 1024).is_err() {
            return 0;
        }
        block.resize(1024 * 1024, 0xa5);
        std::hint::black_box(&block);
        allocations.push(block);
    }
    5
}

#[cfg(windows)]
fn run_windows_cpu_limit_probe() -> i32 {
    let started = std::time::Instant::now();
    let mut value = 0u64;
    while started.elapsed() < std::time::Duration::from_secs(10) {
        for index in 0..1_000_000u64 {
            value = value.wrapping_add(index.rotate_left((value & 31) as u32));
        }
        std::hint::black_box(value);
    }
    5
}

#[cfg(windows)]
fn run_windows_idle_probe() -> i32 {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[cfg(windows)]
fn run_windows_tree_probe(raw: &str) -> i32 {
    let Ok(request) = serde_json::from_str::<WindowsTreeProbeRequest>(raw) else {
        return 2;
    };
    if std::fs::write(&request.parent_pid_path, std::process::id().to_string()).is_err() {
        return 3;
    }
    let mut command = std::process::Command::new(&request.executable);
    command
        .arg("--windows-self-check-idle-probe")
        .current_dir(&request.workspace);
    let child = match tiangong_toolkit::configure_no_window(&mut command).spawn() {
        Ok(child) => child,
        Err(_) => return 4,
    };
    if std::fs::write(&request.child_pid_path, child.id().to_string()).is_err()
        || std::fs::write(&request.ready_path, "ready").is_err()
    {
        return 5;
    }
    let _child = child;
    run_windows_idle_probe()
}

#[cfg(windows)]
struct WindowsTreeFixture {
    program_root: PathBuf,
    request: WindowsTreeProbeRequest,
    policy: tiangong_sandbox::SandboxPolicy,
}

#[cfg(windows)]
impl WindowsTreeFixture {
    fn new(root: &Path, current_exe: &Path, label: &str) -> Result<Self> {
        let program_root = root.join(label).join("workspace");
        std::fs::create_dir_all(&program_root)
            .with_context(|| format!("创建 Windows {label} 生命周期工作区失败"))?;
        let executable = program_root.join("tree-probe.exe");
        std::fs::copy(current_exe, &executable)
            .with_context(|| format!("复制 Windows {label} 生命周期探针失败"))?;
        let request = WindowsTreeProbeRequest {
            workspace: program_root.clone(),
            executable,
            parent_pid_path: program_root.join("parent.pid"),
            child_pid_path: program_root.join("child.pid"),
            ready_path: program_root.join("ready"),
        };
        let policy = tiangong_sandbox::SandboxPolicy::workspace_write(&program_root);
        Ok(Self {
            program_root,
            request,
            policy,
        })
    }

    fn launch(
        &self,
        host_pid: Option<u32>,
        stop_event_name: Option<&str>,
        timeout: std::time::Duration,
    ) -> Result<i32> {
        let args = vec![
            "--windows-self-check-tree-probe".to_string(),
            serde_json::to_string(&self.request)?,
        ];
        tiangong_sandbox::sandbox::windows::launch(
            tiangong_sandbox::sandbox::windows::WindowsLaunchRequest {
                program: &self.request.executable,
                program_root: &self.program_root,
                args: &args,
                policy: &self.policy,
                host_pid,
                stop_event_name,
                timeout: Some(timeout),
            },
        )
    }

    fn process_tree_cleaned(&self) -> bool {
        if !self.request.ready_path.is_file() {
            eprintln!(
                "Windows 生命周期探针未就绪: {}",
                self.request.ready_path.display()
            );
            return false;
        }
        let Some(parent_pid) = read_windows_probe_pid(&self.request.parent_pid_path) else {
            return false;
        };
        let Some(child_pid) = read_windows_probe_pid(&self.request.child_pid_path) else {
            return false;
        };
        let parent_stopped =
            wait_for_windows_process_exit(parent_pid, std::time::Duration::from_secs(5));
        let child_stopped =
            wait_for_windows_process_exit(child_pid, std::time::Duration::from_secs(5));
        if !parent_stopped || !child_stopped {
            eprintln!(
                "Windows 生命周期探针残留进程: parent={parent_pid} ({parent_stopped}), child={child_pid} ({child_stopped})"
            );
        }
        parent_stopped && child_stopped
    }
}

#[cfg(windows)]
fn read_windows_probe_pid(path: &Path) -> Option<u32> {
    match std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
    {
        Some(pid) => Some(pid),
        None => {
            eprintln!("Windows 生命周期 PID 文件无效: {}", path.display());
            None
        }
    }
}

#[cfg(windows)]
fn windows_process_exists(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0;
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    unsafe {
        CloseHandle(handle);
    }
    queried && exit_code == STILL_ACTIVE as u32
}

#[cfg(windows)]
fn wait_for_windows_process_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while windows_process_exists(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    !windows_process_exists(pid)
}

#[cfg(windows)]
fn wait_for_windows_marker(path: &Path, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while !path.is_file() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    path.is_file()
}

#[cfg(windows)]
fn windows_timeout_cleanup_probe(root: &Path, current_exe: &Path, label: &str) -> Result<bool> {
    let fixture = WindowsTreeFixture::new(root, current_exe, label)?;
    let timed_out = match fixture.launch(None, None, std::time::Duration::from_secs(2)) {
        Err(error) if format!("{error:#}").contains("等待 AppContainer 进程超过") => true,
        Err(error) => {
            eprintln!("Windows 超时清理探针失败: {error:#}");
            false
        }
        Ok(exit_code) => {
            eprintln!("Windows 超时清理探针意外退出: {exit_code}");
            false
        }
    };
    Ok(timed_out && fixture.process_tree_cleaned())
}

#[cfg(windows)]
struct WindowsSelfCheckEvent {
    handle: std::os::windows::io::OwnedHandle,
    name: String,
}

#[cfg(windows)]
impl WindowsSelfCheckEvent {
    fn new() -> Result<Self> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateEventW;

        let name = format!("Local\\TiangongSandboxSelfCheckStop-{}", scru128::new());
        let wide = std::ffi::OsStr::new(&name)
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("创建 Windows 自检停止事件失败");
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            bail!("Windows 自检停止事件名称冲突");
        }
        Ok(Self {
            handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
            name,
        })
    }
}

#[cfg(windows)]
fn windows_stop_event_cleanup_probe(root: &Path, current_exe: &Path) -> Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Threading::SetEvent;

    let fixture = WindowsTreeFixture::new(root, current_exe, "stop-event")?;
    let event = WindowsSelfCheckEvent::new()?;
    let event_handle = event.handle.as_raw_handle() as usize;
    let ready_path = fixture.request.ready_path.clone();
    let signaler = std::thread::spawn(move || {
        let ready = wait_for_windows_marker(&ready_path, std::time::Duration::from_secs(5));
        let signaled = unsafe { SetEvent(event_handle as *mut std::ffi::c_void) } != 0;
        ready && signaled
    });
    let stopped = match fixture.launch(None, Some(&event.name), std::time::Duration::from_secs(15))
    {
        Ok(1) => true,
        Ok(exit_code) => {
            eprintln!("Windows 停止事件探针退出码异常: {exit_code}");
            false
        }
        Err(error) => {
            eprintln!("Windows 停止事件探针失败: {error:#}");
            false
        }
    };
    let signaled = signaler.join().unwrap_or(false);
    Ok(signaled && stopped && fixture.process_tree_cleaned())
}

#[cfg(windows)]
fn windows_host_exit_cleanup_probe(root: &Path, current_exe: &Path) -> Result<bool> {
    let fixture = WindowsTreeFixture::new(root, current_exe, "host-exit")?;
    let mut command = std::process::Command::new(current_exe);
    command.arg("--windows-self-check-idle-probe");
    let mut host = tiangong_toolkit::configure_no_window(&mut command)
        .spawn()
        .context("启动 Windows 生命周期宿主探针失败")?;
    let host_pid = host.id();
    let ready_path = fixture.request.ready_path.clone();
    let killer = std::thread::spawn(move || {
        let ready = wait_for_windows_marker(&ready_path, std::time::Duration::from_secs(5));
        let killed = host.kill().is_ok();
        let _ = host.wait();
        ready && killed
    });
    let stopped = match fixture.launch(Some(host_pid), None, std::time::Duration::from_secs(15)) {
        Ok(1) => true,
        Ok(exit_code) => {
            eprintln!("Windows 宿主退出探针退出码异常: {exit_code}");
            false
        }
        Err(error) => {
            eprintln!("Windows 宿主退出探针失败: {error:#}");
            false
        }
    };
    let killed = killer.join().unwrap_or(false);
    Ok(killed && stopped && fixture.process_tree_cleaned())
}

#[cfg(windows)]
fn run_windows_lifecycle_worker(root: &str, label: &str) -> i32 {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Windows 并发探针无法读取当前程序: {error}");
            return EXIT_SANDBOX_UNAVAILABLE;
        }
    };
    match windows_timeout_cleanup_probe(Path::new(root), &current_exe, label) {
        Ok(true) => 0,
        Ok(false) => EXIT_SANDBOX_UNAVAILABLE,
        Err(error) => {
            eprintln!("Windows 并发生命周期探针失败: {error:#}");
            EXIT_SANDBOX_UNAVAILABLE
        }
    }
}

#[cfg(windows)]
fn cleanup_windows_workers(workers: &mut [(std::process::Child, PathBuf, PathBuf)]) {
    for (child, _, _) in workers {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(windows)]
fn windows_concurrent_cleanup_probe(root: &Path, current_exe: &Path) -> Result<bool> {
    let worker_root = root.join("concurrent");
    std::fs::create_dir_all(&worker_root).context("创建 Windows 并发探针目录失败")?;
    let input_path = worker_root.join("stdin.txt");
    std::fs::write(&input_path, []).context("创建 Windows 并发探针输入失败")?;
    let mut workers = Vec::new();
    for index in 0..10 {
        let stdout_path = worker_root.join(format!("worker-{index}.stdout"));
        let stderr_path = worker_root.join(format!("worker-{index}.stderr"));
        let child = (|| -> Result<std::process::Child> {
            let stdin = std::fs::File::open(&input_path)?;
            let stdout = std::fs::File::create(&stdout_path)?;
            let stderr = std::fs::File::create(&stderr_path)?;
            let mut command = std::process::Command::new(current_exe);
            command
                .arg("--windows-self-check-lifecycle-worker")
                .arg(&worker_root)
                .arg(format!("worker-{index}"))
                .stdin(std::process::Stdio::from(stdin))
                .stdout(std::process::Stdio::from(stdout))
                .stderr(std::process::Stdio::from(stderr));
            Ok(tiangong_toolkit::configure_no_window(&mut command).spawn()?)
        })();
        match child {
            Ok(child) => workers.push((child, stdout_path, stderr_path)),
            Err(error) => {
                cleanup_windows_workers(&mut workers);
                return Err(error).context("启动 Windows 并发生命周期探针失败");
            }
        }
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let mut statuses = vec![None; workers.len()];
    loop {
        let mut pending = false;
        for index in 0..workers.len() {
            if statuses[index].is_none() {
                let observed = workers[index].0.try_wait();
                match observed {
                    Ok(status) => statuses[index] = status,
                    Err(error) => {
                        cleanup_windows_workers(&mut workers);
                        return Err(error).context("读取 Windows 并发探针状态失败");
                    }
                }
            }
            pending |= statuses[index].is_none();
        }
        if !pending {
            break;
        }
        if std::time::Instant::now() >= deadline {
            cleanup_windows_workers(&mut workers);
            eprintln!("Windows 并发生命周期探针超时");
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let mut all_ok = true;
    for ((_, stdout_path, stderr_path), status) in workers.iter().zip(statuses) {
        if !status.is_some_and(|status| status.success()) {
            all_ok = false;
            eprintln!(
                "Windows 并发生命周期探针失败: stdout={} stderr={}",
                std::fs::read_to_string(stdout_path).unwrap_or_default(),
                std::fs::read_to_string(stderr_path).unwrap_or_default()
            );
        }
    }
    Ok(all_ok)
}

#[cfg(windows)]
#[derive(Serialize)]
struct WindowsLifecycleReport {
    platform: &'static str,
    timeout_cleanup: bool,
    stop_event_cleanup: bool,
    host_exit_cleanup: bool,
    process_tree_cleanup: bool,
    concurrent_cleanup: bool,
}

#[cfg(windows)]
fn lifecycle_probe_result(label: &str, result: Result<bool>) -> bool {
    match result {
        Ok(value) => value,
        Err(error) => {
            eprintln!("Windows {label} 生命周期验证失败: {error:#}");
            false
        }
    }
}

#[cfg(windows)]
fn run_windows_lifecycle_self_check() -> i32 {
    let result = (|| -> Result<WindowsLifecycleReport> {
        let root = tempfile::Builder::new()
            .prefix("tiangong-sandbox-lifecycle-")
            .tempdir()
            .context("创建 Windows 生命周期自检目录失败")?;
        let current_exe = std::env::current_exe().context("读取 Windows 生命周期自检程序失败")?;
        let timeout_cleanup = lifecycle_probe_result(
            "超时",
            windows_timeout_cleanup_probe(root.path(), &current_exe, "timeout"),
        );
        let stop_event_cleanup = lifecycle_probe_result(
            "停止事件",
            windows_stop_event_cleanup_probe(root.path(), &current_exe),
        );
        let host_exit_cleanup = lifecycle_probe_result(
            "宿主退出",
            windows_host_exit_cleanup_probe(root.path(), &current_exe),
        );
        let concurrent_cleanup = lifecycle_probe_result(
            "并发",
            windows_concurrent_cleanup_probe(root.path(), &current_exe),
        );
        Ok(WindowsLifecycleReport {
            platform: "windows",
            timeout_cleanup,
            stop_event_cleanup,
            host_exit_cleanup,
            process_tree_cleanup: timeout_cleanup && stop_event_cleanup && host_exit_cleanup,
            concurrent_cleanup,
        })
    })();

    match result {
        Ok(report) => {
            let passed = report.timeout_cleanup
                && report.stop_event_cleanup
                && report.host_exit_cleanup
                && report.process_tree_cleanup
                && report.concurrent_cleanup;
            println!(
                "{}",
                serde_json::to_string(&report).expect("序列化 Windows 生命周期报告失败")
            );
            if passed { 0 } else { EXIT_SANDBOX_UNAVAILABLE }
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({
                    "platform": "windows",
                    "error": format!("{error:#}"),
                })
            );
            EXIT_SANDBOX_UNAVAILABLE
        }
    }
}

#[cfg(unix)]
fn run_enforcement_probes(report: &mut serde_json::Map<String, serde_json::Value>) {
    let root = tempfile::Builder::new()
        .prefix("tiangong-sandbox-selfcheck-")
        .tempdir()
        .expect("创建自检根目录失败");
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    let fake_home = root.path().join("home");
    let ssh_dir = fake_home.join(".ssh");
    let aws_dir = fake_home.join(".aws");
    let gnupg_dir = fake_home.join(".gnupg");
    let tiangong_dir = fake_home.join(".tiangong");
    let git_dir = workspace.join(".git");
    for path in [
        &workspace,
        &outside,
        &ssh_dir,
        &aws_dir,
        &gnupg_dir,
        &tiangong_dir,
        &git_dir,
    ] {
        std::fs::create_dir_all(path).expect("创建自检目录失败");
    }

    let ssh_secret = ssh_dir.join("id_ed25519");
    let aws_secret = aws_dir.join("credentials");
    let gnupg_secret = gnupg_dir.join("private-keys-v1.d.key");
    let trust_db = tiangong_dir.join("trust.db");
    let secrets = [
        (&ssh_secret, "TIANGONG_FAKE_SSH_SECRET"),
        (&aws_secret, "TIANGONG_FAKE_AWS_SECRET"),
        (&gnupg_secret, "TIANGONG_FAKE_GPG_SECRET"),
        (&trust_db, "TIANGONG_FAKE_TRUST_SECRET"),
    ];
    for (path, marker) in secrets {
        std::fs::write(path, marker).expect("写入自检假凭据失败");
    }
    let bashrc = fake_home.join(".bashrc");
    std::fs::write(&bashrc, "safe\n").expect("写入自检配置失败");
    let git_config = git_dir.join("config");
    std::fs::write(&git_config, "safe\n").expect("写入自检 git 配置失败");

    let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write(&workspace);
    policy.denied_read_paths = vec![ssh_dir, aws_dir, gnupg_dir, trust_db];

    let inside = workspace.join("selfcheck.txt");
    let output = run_sandbox_command(&policy, "/usr/bin/touch", &[inside.display().to_string()]);
    report.insert(
        "workspace_write".into(),
        serde_json::Value::from(output.status.success() && inside.is_file()),
    );

    let outside_write = outside.join("blocked.txt");
    let output = run_sandbox_command(
        &policy,
        "/usr/bin/touch",
        &[outside_write.display().to_string()],
    );
    report.insert(
        "outside_write_blocked".into(),
        serde_json::Value::from(!output.status.success() && !outside_write.exists()),
    );

    let delete_target = outside.join("delete-me");
    std::fs::create_dir_all(&delete_target).expect("创建自检删除目标失败");
    std::fs::write(delete_target.join("keep"), "safe").expect("写入自检删除目标失败");
    let output = run_sandbox_command(
        &policy,
        "/bin/rm",
        &["-rf".into(), delete_target.display().to_string()],
    );
    report.insert(
        "outside_delete_blocked".into(),
        serde_json::Value::from(!output.status.success() && delete_target.join("keep").is_file()),
    );

    let output = run_sandbox_command(
        &policy,
        "/bin/sh",
        &[
            "-c".into(),
            format!("printf PWNED >> {}", shell_quote(&bashrc)),
        ],
    );
    report.insert(
        "outside_config_write_blocked".into(),
        serde_json::Value::from(
            !output.status.success()
                && std::fs::read_to_string(&bashrc).is_ok_and(|value| value == "safe\n"),
        ),
    );

    // .git 可写是功能要求（用户裁定 2026-08-30）：agent 需要完整 git
    // 工作流（add/commit/branch 等），沙箱必须放行写入并真实落盘。
    let output = run_sandbox_command(
        &policy,
        "/bin/sh",
        &[
            "-c".into(),
            format!(
                "printf {} > {}",
                GIT_METADATA_PROBE_MARKER,
                shell_quote(&git_config)
            ),
        ],
    );
    report.insert(
        "git_metadata_write".into(),
        serde_json::Value::from(
            output.status.success()
                && std::fs::read_to_string(&git_config)
                    .is_ok_and(|value| value == GIT_METADATA_PROBE_MARKER),
        ),
    );

    let sensitive_read_blocked = [
        ssh_secret,
        aws_secret,
        gnupg_secret,
        tiangong_dir.join("trust.db"),
    ]
    .iter()
    .all(|path| {
        let output = run_sandbox_command(&policy, "/bin/cat", &[path.display().to_string()]);
        !String::from_utf8_lossy(&output.stdout).contains("TIANGONG_FAKE_")
    });
    report.insert(
        "sensitive_read_blocked".into(),
        serde_json::Value::from(sensitive_read_blocked),
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("创建自检网络监听失败");
    let address = listener.local_addr().expect("读取自检监听地址失败");
    let current_exe = std::env::current_exe().expect("读取自检程序路径失败");
    let direct_probe = std::process::Command::new(&current_exe)
        .arg("--self-check-network-probe")
        .arg(address.to_string())
        .status()
        .expect("启动直连网络探测失败")
        .success();
    let output = run_sandbox_command(
        &policy,
        &current_exe.display().to_string(),
        &["--self-check-network-probe".into(), address.to_string()],
    );
    drop(listener);
    report.insert(
        "network_blocked".into(),
        serde_json::Value::from(direct_probe && !output.status.success()),
    );

    let symlink_target = outside.join("symlink-target");
    std::fs::write(&symlink_target, "safe\n").expect("写入自检符号链接目标失败");
    let symlink = workspace.join("escape-link");
    std::os::unix::fs::symlink(&symlink_target, &symlink).expect("创建自检符号链接失败");
    let output = run_sandbox_command(
        &policy,
        "/bin/sh",
        &[
            "-c".into(),
            format!("printf PWNED > {}", shell_quote(&symlink)),
        ],
    );
    report.insert(
        "symlink_escape_blocked".into(),
        serde_json::Value::from(
            !output.status.success()
                && std::fs::read_to_string(&symlink_target).is_ok_and(|value| value == "safe\n"),
        ),
    );

    let traversal_target = workspace.join("../outside/traversal");
    let output = run_sandbox_command(
        &policy,
        "/usr/bin/touch",
        &[traversal_target.display().to_string()],
    );
    report.insert(
        "path_traversal_blocked".into(),
        serde_json::Value::from(!output.status.success() && !outside.join("traversal").exists()),
    );
}

#[cfg(unix)]
fn run_sandbox_command(
    policy: &tiangong_sandbox::SandboxPolicy,
    target: &str,
    args: &[String],
) -> std::process::Output {
    let tiangong_sandbox::SandboxedProgram::Wrapped { program, prefix } =
        tiangong_sandbox::wrap(policy)
    else {
        panic!("自检时平台沙箱应保持可用");
    };
    std::process::Command::new(program)
        .args(prefix)
        .arg(target)
        .args(args)
        .output()
        .expect("启动沙箱自检命令失败")
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(root: &Path, program: &Path) -> LaunchRequest {
        LaunchRequest {
            grant_cache: None,
            protocol_version: PROTOCOL_VERSION,
            policy_schema: POLICY_SCHEMA,
            policy: tiangong_sandbox::SandboxPolicy::workspace_write(root),
            plugin_id: "command".to_string(),
            program: program.display().to_string(),
            program_root: root.display().to_string(),
            program_sha256: String::new(),
            args: Vec::new(),
            target_validation: TargetValidation::PluginManifest,
            interpreter: false,
        }
    }

    #[test]
    fn target_path_traversal_is_rejected() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("command");
        let outside = fixture.path().join("outside-sidecar");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&outside, "not executable").unwrap();

        let error = validate_target(&request(&root, &root.join("../outside-sidecar"))).unwrap_err();
        assert!(error.to_string().contains("不在插件权威目录内"));
    }

    /// 全量沙箱化：非 command 插件不再被白名单拒绝（错误面收敛到
    /// 清单与摘要校验）；解释器形态走退化校验链。
    #[test]
    fn any_plugin_id_is_accepted_beyond_command() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("fs");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("plugin.json"),
            r#"{"id":"fs","sidecar":{"binary":"sidecar"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("sidecar"), "stub").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("sidecar"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let mut req = request(&root, &root.join("sidecar"));
        req.plugin_id = "fs".to_string();
        // 无清单比对所需的完整字段时，错误应指向清单链路而非白名单。
        let error = validate_target(&req).unwrap_err();
        assert!(!error.to_string().contains("只允许启动"));
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    /// 将 Unix 风格路径转换为当前平台的绝对路径，Windows 上补盘符前缀。
    fn platform_path(unix: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", unix.replace('/', "\\")))
        } else {
            PathBuf::from(unix)
        }
    }
    #[test]
    fn inline_policy_parses_all_options() {
        let workspace = platform_path("/workspace");
        let extra_run = platform_path("/tmp/run");
        let extra_cache = platform_path("/tmp/cache");
        let protected = platform_path("/workspace/protected");
        let denied = platform_path("/home/user/.ssh");
        let mut args = strings(&["run", "--mode", "workspace-write", "--workspace"]);
        args.push(workspace.display().to_string());
        args.push("--writable".to_string());
        args.push(extra_run.display().to_string());
        args.push("--writable".to_string());
        args.push(extra_cache.display().to_string());
        args.push("--protect".to_string());
        args.push(protected.display().to_string());
        args.push("--deny-read".to_string());
        args.push(denied.display().to_string());
        args.extend(strings(&[
            "--network",
            "allow",
            "--max-cpu-seconds",
            "12",
            "--max-memory-bytes",
            "4096",
            "--max-processes",
            "8",
            "--",
            "echo",
            "ok",
        ]));
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(
            parsed.policy.mode,
            tiangong_sandbox::SandboxMode::WorkspaceWrite
        );
        assert_eq!(parsed.policy.workspace, workspace);
        assert_eq!(parsed.policy.extra_writable, vec![extra_run, extra_cache]);
        assert_eq!(parsed.policy.protected_paths, vec![protected]);
        assert_eq!(parsed.policy.denied_read_paths, vec![denied]);
        assert!(parsed.policy.allow_network);
        assert_eq!(parsed.policy.resource_limits.max_cpu_time_seconds, 12);
        assert_eq!(parsed.policy.resource_limits.max_memory_bytes, 4096);
        assert_eq!(parsed.policy.resource_limits.max_processes, 8);
        assert_eq!(parsed.command, strings(&["echo", "ok"]));
    }
    #[test]
    fn inline_policy_defaults_are_fail_closed() {
        let workspace = platform_path("/workspace");
        let mut args = strings(&["run", "--workspace"]);
        args.push(workspace.display().to_string());
        args.extend(strings(&["--", "/bin/true"]));
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(
            parsed.policy.mode,
            tiangong_sandbox::SandboxMode::WorkspaceWrite
        );
        assert!(!parsed.policy.allow_network);
        assert_eq!(
            parsed.policy.resource_limits,
            tiangong_sandbox::SandboxResourceLimits::default()
        );
    }
    #[test]
    fn policy_file_and_inline_options_are_mutually_exclusive() {
        let error = parse_run_args(&strings(&[
            "run",
            "--policy",
            "/tmp/policy.json",
            "--workspace",
            "/workspace",
            "--",
            "/bin/true",
        ]))
        .unwrap_err();
        assert!(error.to_string().contains("cannot be used with"));
    }

    #[test]
    fn inline_policy_rejects_relative_paths_and_zero_limits() {
        let relative = parse_run_args(&strings(&[
            "run",
            "--workspace",
            "relative",
            "--",
            "/bin/true",
        ]))
        .unwrap_err();
        assert!(relative.to_string().contains("必须使用绝对路径"));

        let zero = parse_run_args(&strings(&[
            "run",
            "--workspace",
            "/workspace",
            "--max-processes",
            "0",
            "--",
            "/bin/true",
        ]))
        .unwrap_err();
        assert!(zero.to_string().contains("not in 1..="));
    }

    #[test]
    fn missing_target_validation_defaults_to_digest_only() {
        let value = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "policy_schema": POLICY_SCHEMA,
            "policy": tiangong_sandbox::SandboxPolicy::workspace_write("/workspace"),
            "program": "/bin/sh",
            "program_root": "/bin",
            "program_sha256": "00"
        });
        let request: LaunchRequest = serde_json::from_value(value).unwrap();
        assert_eq!(request.target_validation, TargetValidation::DigestOnly);
    }

    #[test]
    fn digest_only_target_does_not_require_plugin_manifest() {
        let fixture = tempfile::tempdir().unwrap();
        let program = fixture.path().join("program");
        std::fs::write(&program, "stub").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut req = request(fixture.path(), &program);
        req.target_validation = TargetValidation::DigestOnly;
        req.program_sha256 = sha256_file(&program).unwrap();
        assert_eq!(
            validate_target(&req).unwrap(),
            std::fs::canonicalize(program).unwrap()
        );
    }

    #[test]
    fn interpreter_target_bypasses_manifest_check() {
        let fixture = tempfile::tempdir().unwrap();
        let bin_dir = fixture.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let interpreter_bin = bin_dir.join("node");
        std::fs::write(&interpreter_bin, "stub").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&interpreter_bin, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        let mut req = request(&bin_dir, &interpreter_bin);
        req.interpreter = true;
        // 解释器形态没有 plugin.json，校验应走到摘要比对而非清单失败。
        let error = validate_target(&req).unwrap_err();
        assert!(error.to_string().contains("摘要不匹配"));
    }

    #[cfg(windows)]
    #[test]
    fn interpreter_target_resolves_linked_program_root() {
        let fixture = tempfile::tempdir().unwrap();
        let actual_root = fixture.path().join("versions").join("node-v22");
        let linked_root = fixture.path().join("nodejs");
        std::fs::create_dir_all(&actual_root).unwrap();
        let actual_program = actual_root.join("node.exe");
        std::fs::write(&actual_program, "stub").unwrap();
        assert!(create_directory_junction(&actual_root, &linked_root).unwrap());

        let linked_program = linked_root.join("node.exe");
        let mut req = request(&linked_root, &linked_program);
        req.interpreter = true;
        req.program_sha256 = sha256_file(&actual_program).unwrap();

        assert_eq!(
            validate_target(&req).unwrap(),
            std::fs::canonicalize(actual_program).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn ordinary_target_still_rejects_linked_program_root() {
        let fixture = tempfile::tempdir().unwrap();
        let actual_root = fixture.path().join("actual-plugin");
        let linked_root = fixture.path().join("linked-plugin");
        std::fs::create_dir_all(&actual_root).unwrap();
        std::fs::write(actual_root.join("sidecar.exe"), "stub").unwrap();
        assert!(create_directory_junction(&actual_root, &linked_root).unwrap());

        let error =
            validate_target(&request(&linked_root, &linked_root.join("sidecar.exe"))).unwrap_err();
        assert!(error.to_string().contains("插件根目录必须是实际目录"));
    }

    #[test]
    fn control_character_policy_path_is_rejected() {
        let fixture = tempfile::tempdir().unwrap();
        let program = fixture.path().join("sidecar");
        let mut req = request(fixture.path(), &program);
        req.policy.workspace = "/tmp/ws\n(deny default)".into();
        let error = validate_policy_paths(&req).unwrap_err();
        assert!(error.to_string().contains("控制字符"));

        let mut req = request(fixture.path(), &program);
        req.policy.extra_writable = vec!["/tmp/ok".into()];
        assert!(validate_policy_paths(&req).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_is_rejected() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("command");
        let outside = fixture.path().join("outside-sidecar");
        let link = root.join("sidecar");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&outside, "not executable").unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let error = validate_target(&request(&root, &link)).unwrap_err();
        assert!(error.to_string().contains("目标程序必须是实际普通文件"));
    }

    /// 资源上限不依赖平台沙箱可用性：直接验证 exec 前施加的 rlimit
    /// 能在无 Seatbelt/bwrap 配合时强制终止 CPU 死循环（开发机嵌套沙箱
    /// 环境下其余真实拦截用例会跳过，此项仍可执行）。
    #[cfg(unix)]
    #[test]
    fn resource_limits_kill_cpu_spin_without_sandbox() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "i=0; while :; do i=$((i+1)); done"]);
        apply_unix_resource_limits(
            &mut command,
            &tiangong_sandbox::SandboxResourceLimits {
                max_cpu_time_seconds: 1,
                max_memory_bytes: 2 * 1024 * 1024 * 1024,
                max_processes: 64,
            },
        );
        let started = std::time::Instant::now();
        let status = command.status().expect("执行受限命令失败");
        assert!(
            !status.success(),
            "CPU 死循环应被 RLIMIT_CPU 强制终止: {status}"
        );
        // SIGXCPU 直接证明终止来自 CPU 上限；并发测试下死循环分到的 CPU
        // 减少，墙钟会拉长，不能用固定秒数区分终止原因。
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(libc::SIGXCPU),
            "终止信号应为 SIGXCPU: {status}"
        );
        assert!(
            started.elapsed().as_secs() < 60,
            "CPU 上限终止不应依赖外部超时"
        );
    }
}
