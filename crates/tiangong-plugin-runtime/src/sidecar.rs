//! 通用插件 sidecar 进程与连接管理。
//!
//! 本模块只处理进程、endpoint、鉴权和 JSON Lines 传输，不理解插件业务协议。
//! TCP 与 stdio 两种传输并存：TCP 为存量默认，stdio 为沙箱友好的新传输
//! （RFC 0017 D16）。当前只有 command 由宿主策略强制使用 stdio，插件清单
//! 不参与通信通道或沙箱权限决策。

pub mod command;
pub mod stdio;

pub use command::EphemeralCommandConnection;
pub use stdio::{StdioSidecarConnection, TRANSPORT_STDIO};

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, IpcAuth, IpcEndpoint, IpcFrame, IpcRequest,
    PROTOCOL_VERSION, Request, Response,
};

const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 内部往返的响应等待策略。
///
/// 启动、Auth、完整验证握手与进程就绪握手必须使用 [`ResponseWait::Bounded`]；
/// 业务 Handler 请求使用 [`ResponseWait::UntilCancelled`]——不设自动时限，
/// 仅由用户取消、会话取消、插件卸载或宿主退出终止。显式策略代替按操作
/// 名称隐式判断，避免新增调用点时意外引入无限等待。
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponseWait {
    Bounded(Duration),
    UntilCancelled,
}

impl ResponseWait {
    /// 读取超时时间：无限等待按短轮询间隔唤醒（排空进度、感知断开）。
    fn poll_interval(self) -> Duration {
        match self {
            Self::Bounded(timeout) => timeout.min(POLL_INTERVAL),
            Self::UntilCancelled => POLL_INTERVAL,
        }
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(200);

pub const PLUGIN_ID_ENV: &str = "TIANGONG_PLUGIN_ID";
pub const PLUGIN_VERSION_ENV: &str = "TIANGONG_PLUGIN_VERSION";
pub const PLUGIN_ENDPOINT_ENV: &str = "TIANGONG_PLUGIN_ENDPOINT";
pub const PLUGIN_DATA_DIR_ENV: &str = "TIANGONG_PLUGIN_DATA_DIR";
pub const STORAGE_ROOT_ENV: &str = "TIANGONG_STORAGE_ROOT";
pub const EXEC_ENV_JSON_ENV: &str = "TIANGONG_EXEC_ENV_JSON";

/// 本机 server 的 HTTP 地址（如 `http://127.0.0.1:8080`），供需要回调 host 的
/// sidecar（如 scheduler 到点投递消息）使用。
pub const SERVER_URL_ENV: &str = "TIANGONG_SERVER_URL";
/// 本机 server 的鉴权 token（可选，未配置鉴权时为空）。
pub const SERVER_TOKEN_ENV: &str = "TIANGONG_SERVER_TOKEN";

/// 插件内容清单固定文件名（devkit 构建生成，本地信任与官方签名共用的信任锚）。
pub const CONTENT_MANIFEST_FILE: &str = "content-manifest.json";

/// 宿主在单次工具调用边界确定的权威上下文。
///
/// 该上下文不经过插件协议，也不从工具参数推导；command 沙箱只使用这里的
/// 工作区构造策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarInvocationContext {
    pub session_id: String,
    pub invocation_id: String,
    pub authoritative_workspace: PathBuf,
}

impl SidecarInvocationContext {
    pub fn new(
        session_id: impl Into<String>,
        invocation_id: impl Into<String>,
        authoritative_workspace: impl Into<PathBuf>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            invocation_id: invocation_id.into(),
            authoritative_workspace: authoritative_workspace.into(),
        }
    }
}

/// 将宿主权威上下文转换为通用请求信封。TCP 与 stdio 共用此入口，避免
/// 传输方式改变会话归属语义。
fn handshake_handles_tool(handshake: &HandshakeResponse, tool_name: &str) -> bool {
    handshake
        .capabilities
        .iter()
        .any(|capability| capability == &format!("tool:{tool_name}") || capability == "tool:*")
}

fn request_invocation_context(
    context: &SidecarInvocationContext,
) -> crate::protocol::RequestInvocationContext {
    crate::protocol::RequestInvocationContext {
        session_id: context.session_id.clone(),
        invocation_id: context.invocation_id.clone(),
        workspace: context
            .authoritative_workspace
            .to_string_lossy()
            .into_owned(),
        actor_id: String::new(),
        deadline_ms: None,
    }
}

#[derive(Debug)]
pub enum SidecarInvokeError {
    Unavailable(String),
    Timeout,
    PermissionDenied,
    ProtocolMismatch(String),
    Internal(String),
}

impl std::fmt::Display for SidecarInvokeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "sidecar 不可用: {message}"),
            Self::Timeout => formatter.write_str("sidecar 请求超时"),
            Self::PermissionDenied => formatter.write_str("sidecar 权限不足"),
            Self::ProtocolMismatch(message) => write!(formatter, "sidecar 协议不兼容: {message}"),
            Self::Internal(message) => write!(formatter, "sidecar 内部错误: {message}"),
        }
    }
}

impl std::error::Error for SidecarInvokeError {}

/// 通用 sidecar 连接：运行时负责协议封装，调用方只提供操作名和 JSON 负载。
pub trait SidecarConnection: Send + Sync {
    fn invoke(&self, operation: &str, payload: &str) -> Result<String>;

    /// 连接是否已被主动停止（stop 后不再自动重启）。
    ///
    /// 长期持有连接引用的调用方（如 WASM 宿主状态）据此发现引用过期，
    /// 改经注册表取换代后的新连接；默认 false（TCP/一次性连接无此语义）。
    fn is_stopped(&self) -> bool {
        false
    }

    fn invoke_with_progress(
        &self,
        operation: &str,
        payload: &str,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        let _ = on_progress;
        self.invoke(operation, payload)
    }

    /// 带宿主权威调用上下文的请求。Runtime 的 stdio 按需连接会在启动沙箱时
    /// 使用其中的工作区；command 的一次性连接还会据此构造完整执行封套。
    /// 其他普通连接默认忽略上下文。
    fn invoke_with_context(
        &self,
        operation: &str,
        payload: &str,
        context: &SidecarInvocationContext,
    ) -> Result<String> {
        let _ = context;
        self.invoke(operation, payload)
    }

    fn invoke_with_context_and_progress(
        &self,
        operation: &str,
        payload: &str,
        context: &SidecarInvocationContext,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        let _ = context;
        self.invoke_with_progress(operation, payload, on_progress)
    }

    /// 带完整统一调用元数据的入口。旧连接实现无需调整，默认退化为原有
    /// SidecarInvocationContext；Runtime 自带连接覆写后透传 actor/deadline。
    fn invoke_with_invocation_context_and_progress(
        &self,
        operation: &str,
        payload: &str,
        context: &crate::protocol::RequestInvocationContext,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        self.invoke_with_context_and_progress(
            operation,
            payload,
            &SidecarInvocationContext::new(
                &context.session_id,
                &context.invocation_id,
                &context.workspace,
            ),
            on_progress,
        )
    }

    /// sidecar 握手声明的工具 Handler。能力名使用 `tool:<name>`；旧 sidecar
    /// 没有声明时返回 false，由兼容适配层继续使用既有 UI/WASM 路径。
    ///
    /// 仅供诊断；正式工具调用路由必须消费安装阶段保存的验证记录，不得
    /// 在调用热路径启动 sidecar 探测能力。
    fn handles_tool(&self, _tool_name: &str) -> Result<bool> {
        Ok(false)
    }

    /// 完整验证（导入/安装/升级/补验证）：确保 sidecar 可启动并完成认证
    /// 握手，返回其声明的能力列表。身份、版本与协议兼容性校验由各传输
    /// 实现的握手语义保证。默认实现仅确认进程可达，不采集能力。
    fn verify_capabilities(&self) -> Result<Vec<String>> {
        self.ensure_running()?;
        Ok(Vec::new())
    }

    /// 更新 exec_env（下次 spawn 时注入子进程环境）。默认空实现。
    fn update_exec_env(&self, _env: std::collections::BTreeMap<String, String>) {}

    /// 停止 sidecar 进程（宿主关闭流程调用）。默认空实现（无进程的连接）。
    fn stop(&self) -> Result<()> {
        Ok(())
    }

    /// 终止指定会话仍在执行的临时 sidecar。常驻 sidecar 默认无需处理。
    fn cancel_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    /// 终止当前进行中的调用（工具级超时 / 会话取消用）。与 stop 不同，
    /// 不改变停止标志——后续调用继续服务。默认无操作（非按需连接无此语义）。
    fn cancel_current(&self) {}

    /// 当前 sidecar 的插件 ID。默认空。
    fn plugin_id(&self) -> &str {
        ""
    }

    /// 确保进程存活并完成握手（安装验证用）。默认空实现。
    fn ensure_running(&self) -> Result<()> {
        Ok(())
    }

    /// 是否存在可用的运行端点（安装验证用）。默认 false。
    fn has_runtime_endpoint(&self) -> bool {
        false
    }
}

/// 解释器形态 sidecar 的启动规格：宿主白名单程序 + 插件目录内入口脚本。
#[derive(Debug, Clone)]
pub struct InterpreterLaunch {
    /// 解释器种类；程序路径每次启动经缓存入口解析，不在配置中保存
    /// 可能过期的副本。
    pub kind: crate::interpreter_env::InterpreterKind,
    /// 入口脚本绝对路径（插件目录内，参与内容哈希锁定）。
    pub entry: PathBuf,
    /// 清单声明的固定参数。
    pub args: Vec<String>,
}

/// 宿主按已验签权限授予的天工敏感配置读取能力（最小权限：逐文件开放；
/// 密钥/信任库/Launcher 目录不可开放；用户凭据由独立策略管理）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SensitiveStorageAccess {
    /// `models.json`（模型配置，含端点与密钥）——`model-config.read`。
    pub model_config: bool,
    /// `mcp.json`（MCP server 配置）——MCP 插件所需。
    pub mcp_config: bool,
    /// `server.json`（服务配置）——`app-storage.read`。
    pub server_config: bool,
    /// `app.json`（应用配置，含沙箱开关）——暂无授权路径，保留字段。
    pub app_config: bool,
}

impl SensitiveStorageAccess {
    /// 是否值得注入存储根路径（至少一项可读才有意义）。
    pub fn any(self) -> bool {
        self.model_config || self.mcp_config || self.server_config || self.app_config
    }
}

/// 一个插件 sidecar 的本地运行配置。
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    pub plugin_id: String,
    pub plugin_version: String,
    pub binary: PathBuf,
    pub endpoint: PathBuf,
    pub log: PathBuf,
    pub data_dir: PathBuf,
    pub storage_root: PathBuf,
    pub sensitive_storage: SensitiveStorageAccess,
    /// 宿主按已验证插件身份授予的用户凭据目录只读能力。
    pub user_credential_reads: crate::host_policy::UserCredentialReadAccess,
    /// 是否允许写入当前用户的 `~/.cache`。
    pub sandbox_user_cache_write: bool,
    /// 是否在沙箱内放行系统音频输入服务。
    pub sandbox_audio_input: bool,
    pub start_timeout: Duration,
    pub request_timeout: Duration,
    /// 本机 server 的 HTTP 地址（供需要回调 host 的 sidecar 使用，如 scheduler）。
    pub server_url: Option<String>,
    /// 本机 server 的鉴权 token。
    pub server_token: Option<String>,
    /// 解释器启动规格；存在时以「程序 + entry + args」替代直接启动 binary。
    pub interpreter: Option<InterpreterLaunch>,
    /// 进程生命周期：按需（默认，每次调用独立进程即起即清）或常驻复用。
    pub lifecycle: crate::manifest::SidecarLifecycle,
    /// 内容哈希清单（本地信任解释器 sidecar 的 spawn 前复核锚）。
    pub integrity_manifest: Option<PathBuf>,
    // OS 沙箱字段为沙箱覆盖分支预留的配置面（本分支仅传输层，无消费方）。
    /// sidecar 进程是否进 OS 沙箱（RFC 0017 D12 继承式，仅 stdio 传输支持）。
    #[allow(dead_code)]
    pub sandbox: bool,
    /// 是否跟随用户全局沙箱开关。仅首次实际使用才启动的 sidecar（包括
    /// terminal、command、解释器与清单声明的按需插件）开启；随 App 预加载
    /// 的常驻服务保持宿主强制沙箱，不接受此开关降级。
    pub sandbox_follows_user_switch: bool,
    /// 沙箱可写根覆盖（一次性实例的会话工作区；None 用数据目录）。
    #[allow(dead_code)]
    pub sandbox_workspace: Option<PathBuf>,
    /// 沙箱额外可写根（每次执行的专用临时目录等）。
    #[allow(dead_code)]
    pub sandbox_extra_writable: Vec<PathBuf>,
    /// 额外禁止读取的路径（测试与宿主强制策略使用）。
    pub sandbox_denied_read_paths: Vec<PathBuf>,
    /// 用户环境变量黑名单。
    pub sandbox_environment_blocklist: Vec<String>,
    /// 沙箱内进程使用的专用临时目录。
    pub sandbox_temp_dir: Option<PathBuf>,
    /// 覆盖单次执行的资源上限（None 用沙箱默认值；Launcher 与 sidecar 双层施加）。
    pub sandbox_resource_limits: Option<tiangong_sandbox::SandboxResourceLimits>,
    /// Launcher 允许启动目标程序的插件权威目录。
    #[allow(dead_code)]
    pub sandbox_program_root: Option<PathBuf>,
    /// 宿主针对当前已安装制品计算的摘要；Launcher 仍会在每次启动时独立复核。
    sandbox_program_sha256: Option<String>,
    /// 沙箱内是否放行网络（文件写白名单不受影响）。
    #[allow(dead_code)]
    pub sandbox_network: bool,
}

impl SidecarConfig {
    pub fn new(
        plugin_id: impl Into<String>,
        plugin_version: impl Into<String>,
        binary: impl Into<PathBuf>,
        endpoint: impl Into<PathBuf>,
        log: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        storage_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            plugin_version: plugin_version.into(),
            binary: binary.into(),
            endpoint: endpoint.into(),
            log: log.into(),
            data_dir: data_dir.into(),
            storage_root: storage_root.into(),
            sensitive_storage: SensitiveStorageAccess::default(),
            user_credential_reads: crate::host_policy::UserCredentialReadAccess::default(),
            sandbox_user_cache_write: false,
            sandbox_audio_input: false,
            start_timeout: DEFAULT_START_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            server_url: None,
            server_token: None,
            interpreter: None,
            lifecycle: crate::manifest::SidecarLifecycle::OnDemand,
            integrity_manifest: None,
            sandbox: false,
            sandbox_follows_user_switch: false,
            sandbox_workspace: None,
            sandbox_extra_writable: Vec::new(),
            sandbox_denied_read_paths: Vec::new(),
            sandbox_environment_blocklist: Vec::new(),
            sandbox_temp_dir: None,
            sandbox_resource_limits: None,
            sandbox_program_root: None,
            sandbox_program_sha256: None,
            sandbox_network: false,
        }
    }

    pub fn with_sensitive_storage(mut self, access: SensitiveStorageAccess) -> Self {
        self.sensitive_storage = access;
        self
    }

    pub fn with_user_credential_reads(
        mut self,
        access: crate::host_policy::UserCredentialReadAccess,
    ) -> Self {
        self.user_credential_reads = access;
        self
    }

    pub fn with_sandbox_user_cache_write(mut self, allowed: bool) -> Self {
        self.sandbox_user_cache_write = allowed;
        self
    }

    pub fn with_sandbox_audio_input(mut self, allowed: bool) -> Self {
        self.sandbox_audio_input = allowed;
        self
    }

    /// 设置解释器启动规格（存在时 spawn 以解释器运行 entry 而非直接启动 binary）。
    pub fn with_interpreter(mut self, launch: InterpreterLaunch) -> Self {
        self.interpreter = Some(launch);
        self
    }

    /// 设置进程生命周期（按需默认；常驻需显式声明）。
    pub fn with_lifecycle(mut self, lifecycle: crate::manifest::SidecarLifecycle) -> Self {
        self.lifecycle = lifecycle;
        self
    }

    /// 设置内容哈希清单路径；spawn 前复核清单内全部文件防篡改。
    pub fn with_integrity_manifest(mut self, manifest_path: impl Into<PathBuf>) -> Self {
        self.integrity_manifest = Some(manifest_path.into());
        self
    }

    /// 按 devkit 内容清单（路径 + sha256）复核插件目录文件树，任一文件缺失、
    /// 路径逃逸或哈希不一致即报错。本地信任解释器 sidecar 的启动前与安装时
    /// 校验共用本函数。
    pub fn verify_integrity_manifest(manifest_path: &Path, root: &Path) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct ContentFile {
            path: String,
            sha256: String,
        }
        #[derive(serde::Deserialize)]
        struct ContentManifest {
            files: Vec<ContentFile>,
        }
        use sha2::{Digest, Sha256};
        let raw = std::fs::read(manifest_path)
            .with_context(|| format!("读取内容清单失败: {}", manifest_path.display()))?;
        let content: ContentManifest = serde_json::from_slice(&raw)
            .with_context(|| format!("解析内容清单失败: {}", manifest_path.display()))?;
        // 路径唯一性：清单不允许重复条目。
        let mut listed = std::collections::BTreeSet::new();
        for file in &content.files {
            let relative = Path::new(&file.path);
            if relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            }) {
                bail!("内容清单包含不安全路径: {}", file.path);
            }
            if !listed.insert(file.path.clone()) {
                bail!("内容清单包含重复路径: {}", file.path);
            }
            let path = root.join(relative);
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| format!("内容清单文件缺失: {}", path.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("内容清单条目不是普通文件: {}", file.path);
            }
            let raw = std::fs::read(&path)
                .with_context(|| format!("内容清单文件缺失: {}", path.display()))?;
            let actual = hex::encode(Sha256::digest(&raw));
            if !actual.eq_ignore_ascii_case(&file.sha256) {
                bail!(
                    "插件文件 {} 与内容清单不一致（可能被篡改），拒绝启动",
                    file.path
                );
            }
        }
        // 反向遍历受管文件树：清单必须完整覆盖——未列出的受管文件视为
        // 篡改（绕过哈希锁定的替换/新增通道）。运行时自管目录与信任标记除外。
        // 运行时自管目录、信任标记与官方签名文件（验签产物，非内容清单
        // 管辖——签名锚定的是清单本身）。
        const UNMANAGED: [&str; 7] = [
            "runtime",
            "logs",
            "data",
            "local-trust.json",
            "content-manifest.json",
            "release.json",
            "release.json.sig",
        ];
        let mut actual_files = std::collections::BTreeSet::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory)
                .with_context(|| format!("读取插件目录失败: {}", directory.display()))?
            {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if directory == root && UNMANAGED.contains(&name.as_ref()) {
                    continue;
                }
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    stack.push(path);
                } else {
                    let relative = path
                        .strip_prefix(root)
                        .context("插件文件相对路径推算失败")?
                        .to_string_lossy()
                        .replace('\\', "/");
                    actual_files.insert(relative);
                }
            }
        }
        let unexpected: Vec<&String> = actual_files.difference(&listed).collect();
        if !unexpected.is_empty() {
            bail!(
                "插件目录存在内容清单未覆盖的文件（可能被篡改），拒绝启动: {}",
                unexpected
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(())
    }

    /// 旧调用方兼容入口。通用传输版本与插件内部业务版本不再由 Runtime
    /// 配置或比较，参数仅为保持源码兼容而接收。
    pub fn with_protocols(
        self,
        _transport_protocol: impl Into<String>,
        _business_protocol: u32,
    ) -> Self {
        self
    }

    pub fn with_timeouts(mut self, start_timeout: Duration, request_timeout: Duration) -> Self {
        self.start_timeout = start_timeout;
        self.request_timeout = request_timeout;
        self
    }

    /// 注入本机 server 的连接信息（供 scheduler 等需要回调 host 的 sidecar 使用）。
    pub fn with_server_endpoint(mut self, url: Option<String>, token: Option<String>) -> Self {
        self.server_url = url;
        self.server_token = token;
        self
    }

    /// sidecar 进程进 OS 沙箱（继承式，子进程树自动受约束；要求 stdio 传输）。
    pub fn with_sandbox(mut self, sandbox: bool) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// 设置此 sidecar 是否跟随用户全局沙箱开关。
    pub fn with_sandbox_user_switch(mut self, follows: bool) -> Self {
        self.sandbox_follows_user_switch = follows;
        self
    }

    /// 本次 spawn 是否实际启用 OS 沙箱。调用方传入当前用户开关快照；
    /// 宿主安全基线未声明沙箱，或受开关控制且用户已关闭时，返回 false。
    pub(crate) fn sandbox_enabled_for_spawn(&self, sandbox_disabled: bool) -> bool {
        self.sandbox && !(self.sandbox_follows_user_switch && sandbox_disabled)
    }

    /// 沙箱内放行网络（fetch 等网络型插件；文件写白名单不受影响）。
    pub fn with_sandbox_network(mut self, allow: bool) -> Self {
        self.sandbox_network = allow;
        self
    }

    /// 覆盖沙箱可写根（一次性实例按会话工作区构造策略）。
    pub fn with_sandbox_workspace(mut self, workspace: Option<PathBuf>) -> Self {
        self.sandbox_workspace = workspace;
        self
    }

    /// 沙箱额外可写根（每次执行的专用临时目录等）。
    pub fn with_sandbox_extra_writable(mut self, extra: Vec<PathBuf>) -> Self {
        self.sandbox_extra_writable = extra;
        self
    }

    /// 增加宿主权威的读取拒绝路径。
    pub fn with_sandbox_denied_read_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.sandbox_denied_read_paths = paths;
        self
    }

    pub fn with_sandbox_user_policy(mut self, policy: &tiangong_config::SandboxUserPolicy) -> Self {
        for configured in &policy.directory_allowlist {
            let path = PathBuf::from(configured);
            if path.is_dir() {
                self.sandbox_extra_writable.push(path);
            } else {
                tracing::warn!(
                    path = %path.display(),
                    "忽略不存在或已失效的沙箱目录白名单项"
                );
            }
        }
        self.sandbox_environment_blocklist = policy.environment_blocklist.clone();
        self
    }

    /// 设置本次沙箱进程的临时目录；调用方还需将其父级或自身加入可写根。
    pub fn with_sandbox_temp_dir(mut self, temp_dir: Option<PathBuf>) -> Self {
        self.sandbox_temp_dir = temp_dir;
        self
    }

    /// 设置 Launcher 可接受的目标程序根目录。
    pub fn with_sandbox_program_root(mut self, root: Option<PathBuf>) -> Self {
        self.sandbox_program_root = root;
        self
    }

    /// 覆盖单次执行的沙箱资源上限（Launcher 强制施加，sidecar 层纵深防御）。
    pub fn with_sandbox_resource_limits(
        mut self,
        limits: Option<tiangong_sandbox::SandboxResourceLimits>,
    ) -> Self {
        self.sandbox_resource_limits = limits;
        self
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("读取 sidecar 目标程序失败: {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

struct ActiveTcpRequestGuard {
    active: Arc<Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>>,
    session_id: String,
    request_id: String,
}

impl ActiveTcpRequestGuard {
    fn new(
        active: Arc<Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>>,
        session_id: String,
        request_id: String,
    ) -> Self {
        if let Ok(mut requests) = active.lock() {
            requests
                .entry(session_id.clone())
                .or_default()
                .insert(request_id.clone());
        }
        Self {
            active,
            session_id,
            request_id,
        }
    }
}

impl Drop for ActiveTcpRequestGuard {
    fn drop(&mut self) {
        let Ok(mut requests) = self.active.lock() else {
            return;
        };
        if let Some(ids) = requests.get_mut(&self.session_id) {
            ids.remove(&self.request_id);
            if ids.is_empty() {
                requests.remove(&self.session_id);
            }
        }
    }
}

/// 通过 endpoint 文件连接本地 sidecar，并在不可用时负责启动。
pub struct ProcessSidecarConnection {
    config: SidecarConfig,
    start_lock: Mutex<()>,
    exec_env: Mutex<std::collections::BTreeMap<String, String>>,
    notification_token: Arc<Mutex<Option<String>>>,
    active_requests:
        Arc<Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>>,
    /// 当前 endpoint 上的进程是否由本连接启动。
    ///
    /// `ensure_running` 命中已有健康进程时本连接只是复用（安装验证的
    /// 临时连接即此形态）：stop 不得终止复用来的进程或删除其 endpoint，
    /// 清理责任归属启动它的连接。
    owns_process: AtomicBool,
}

impl ProcessSidecarConnection {
    pub fn new(config: SidecarConfig) -> Self {
        Self {
            config,
            start_lock: Mutex::new(()),
            exec_env: Mutex::new(std::collections::BTreeMap::new()),
            notification_token: Arc::new(Mutex::new(None)),
            active_requests: Arc::new(Mutex::new(std::collections::HashMap::new())),
            owns_process: AtomicBool::new(false),
        }
    }

    /// 当前 sidecar 的插件 ID。
    pub fn plugin_id(&self) -> &str {
        &self.config.plugin_id
    }

    /// 更新 exec_env（下次 spawn 时注入子进程环境）。
    pub fn update_exec_env(&self, env: std::collections::BTreeMap<String, String>) {
        if let Ok(mut guard) = self.exec_env.lock() {
            *guard = env;
        }
    }

    pub fn ensure_running(&self) -> Result<()> {
        let result = self.ensure_running_inner();
        if result.is_ok() {
            self.ensure_notification_listener();
        }
        result
    }

    /// sidecar 就绪后为当前 endpoint token 启动常驻通知监听（幂等）。
    fn ensure_notification_listener(&self) {
        let Ok(token) = load_endpoint(&self.config.endpoint).map(|endpoint| endpoint.token) else {
            return;
        };
        let Ok(mut current_token) = self.notification_token.lock() else {
            tracing::warn!(
                plugin_id = %self.config.plugin_id,
                "sidecar 通知监听状态锁已损坏"
            );
            return;
        };
        if current_token.as_deref() == Some(token.as_str()) {
            return;
        }
        *current_token = Some(token.clone());
        if spawn_sidecar_notification_listener(
            self.config.plugin_id.clone(),
            self.config.endpoint.clone(),
            token.clone(),
            Arc::clone(&self.notification_token),
        ) {
            return;
        }
        if current_token.as_deref() == Some(token.as_str()) {
            *current_token = None;
        }
    }

    fn invalidate_notification_listener(&self) {
        if let Ok(mut current_token) = self.notification_token.lock() {
            *current_token = None;
        }
    }

    fn ensure_running_inner(&self) -> Result<()> {
        match self.health_check() {
            Ok(()) => return Ok(()),
            Err(error)
                if error
                    .downcast_ref::<SidecarInvokeError>()
                    .is_some_and(|error| {
                        matches!(error, SidecarInvokeError::ProtocolMismatch(_))
                    }) =>
            {
                return Err(error);
            }
            Err(_) => {}
        }

        let _guard = self
            .start_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("sidecar 启动锁已损坏"))?;
        match self.health_check() {
            Ok(()) => return Ok(()),
            Err(error)
                if error
                    .downcast_ref::<SidecarInvokeError>()
                    .is_some_and(|error| {
                        matches!(error, SidecarInvokeError::ProtocolMismatch(_))
                    }) =>
            {
                return Err(error);
            }
            Err(_) => {}
        }

        let _ = std::fs::remove_file(&self.config.endpoint);
        self.spawn()?;

        let deadline = Instant::now() + self.config.start_timeout;
        loop {
            match self.health_check() {
                Ok(()) => {
                    tracing::info!(plugin_id = %self.config.plugin_id, "插件 sidecar 已就绪");
                    return Ok(());
                }
                Err(error)
                    if error
                        .downcast_ref::<SidecarInvokeError>()
                        .is_some_and(|error| {
                            matches!(error, SidecarInvokeError::ProtocolMismatch(_))
                        }) =>
                {
                    return Err(error);
                }
                Err(_) => {}
            }
            if Instant::now() >= deadline {
                return Err(SidecarInvokeError::Timeout).with_context(|| {
                    format!(
                        "等待插件 {} sidecar 就绪超时，日志：{}",
                        self.config.plugin_id,
                        self.config.log.display()
                    )
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn health_check(&self) -> Result<()> {
        self.handshake_exchange().map(|_| ())
    }

    /// 就绪/验证握手：认证可达并校验身份、版本与协议兼容，返回握手响应
    ///（含诊断能力列表）。等待受 `start_timeout` 有限保护。
    fn handshake_exchange(&self) -> Result<HandshakeResponse> {
        let payload = self
            .invoke_protocol_once(
                HANDSHAKE_OPERATION,
                serde_json::json!({"plugin_id": self.config.plugin_id}),
                ResponseWait::Bounded(self.config.start_timeout),
            )
            .map_err(classify_transport_error)?;
        let handshake: HandshakeResponse =
            serde_json::from_value(payload).with_context(|| "解析 sidecar 握手响应失败")?;
        if handshake.plugin_id != self.config.plugin_id {
            return Err(SidecarInvokeError::ProtocolMismatch(format!(
                "sidecar 插件标识不匹配: expected={}, actual={}",
                self.config.plugin_id, handshake.plugin_id
            ))
            .into());
        }
        if handshake.plugin_version != self.config.plugin_version {
            return Err(SidecarInvokeError::ProtocolMismatch(format!(
                "sidecar 插件版本不匹配: expected={}, actual={}",
                self.config.plugin_version, handshake.plugin_version
            ))
            .into());
        }
        if handshake.protocol_version != PROTOCOL_VERSION {
            return Err(SidecarInvokeError::ProtocolMismatch(format!(
                "sidecar 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                handshake.protocol_version
            ))
            .into());
        }
        Ok(handshake)
    }

    /// 只读取 endpoint 文件判断 sidecar 是否已有运行记录，不发起网络连接或健康检查。
    ///
    /// 插件管理页状态查询必须保持纯读取，不能因为展示状态而触发 sidecar 业务。
    pub fn has_runtime_endpoint(&self) -> bool {
        self.config.endpoint.is_file()
    }

    pub fn is_running(&self) -> bool {
        self.health_check().is_ok()
    }

    /// 停止当前插件对应的 sidecar。
    ///
    /// 握手确认身份后终止进程；握手不可用（连不上）但进程仍存活时同样终止——
    /// Windows 上进程卡住、IPC 挂掉时常见这种状态，若不杀掉会持续占用二进制文件，
    /// 导致后续删除/升级失败。仅当身份明确不匹配（防 PID 复用误伤）时才放过。
    pub fn stop(&self) -> Result<()> {
        // 复用运行中进程的连接（如安装验证的临时连接命中已启动的常驻
        // sidecar）不拥有该进程：既不终止也不删除其 endpoint。
        if !self.owns_process.load(Ordering::Acquire) {
            self.invalidate_notification_listener();
            return Ok(());
        }
        let endpoint = match load_endpoint(&self.config.endpoint) {
            Ok(endpoint) => endpoint,
            Err(error)
                if error
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                self.invalidate_notification_listener();
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        let should_terminate = match self.verify_plugin_identity() {
            Ok(()) => true,
            Err(error)
                if error
                    .downcast_ref::<SidecarInvokeError>()
                    .is_some_and(|error| matches!(error, SidecarInvokeError::Unavailable(_))) =>
            {
                // 握手连不上：进程可能已退出，也可能卡住。仅在进程仍存活时终止；
                // 已退出则视为完成，避免对不存在的进程发起 kill/taskkill。
                process_alive(endpoint.pid)
            }
            // 身份不匹配等其它错误：不终止进程，避免误伤其他服务。
            Err(error) => return Err(error),
        };

        if should_terminate {
            terminate_process(endpoint.pid)?;
            wait_for_process_exit(endpoint.pid, Duration::from_secs(5))?;
        }
        let _ = std::fs::remove_file(&self.config.endpoint);
        self.owns_process.store(false, Ordering::Release);
        self.invalidate_notification_listener();
        Ok(())
    }

    fn verify_plugin_identity(&self) -> Result<()> {
        // 停止流程的握手只核对插件 ID：版本/协议不一致的旧进程同样要能停止。
        let payload = self
            .invoke_protocol_once(
                HANDSHAKE_OPERATION,
                serde_json::json!({"plugin_id": self.config.plugin_id}),
                ResponseWait::Bounded(self.config.start_timeout),
            )
            .map_err(classify_transport_error)?;
        let handshake: HandshakeResponse =
            serde_json::from_value(payload).with_context(|| "解析 sidecar 握手响应失败")?;
        if handshake.plugin_id != self.config.plugin_id {
            bail!(
                "sidecar 插件标识不匹配，拒绝停止进程: expected={}, actual={}",
                self.config.plugin_id,
                handshake.plugin_id
            );
        }
        Ok(())
    }

    fn spawn(&self) -> Result<()> {
        if !self.config.binary.is_file() {
            bail!("sidecar 二进制不存在: {}", self.config.binary.display());
        }
        if let Some(parent) = self.config.log.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 sidecar 日志目录失败: {}", parent.display()))?;
        }
        if let Some(parent) = self.config.endpoint.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 sidecar 运行目录失败: {}", parent.display()))?;
        }
        std::fs::create_dir_all(&self.config.data_dir).with_context(|| {
            format!(
                "创建 sidecar 数据目录失败: {}",
                self.config.data_dir.display()
            )
        })?;
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.config.log)
            .with_context(|| format!("打开 sidecar 日志失败: {}", self.config.log.display()))?;
        let stderr = stdout
            .try_clone()
            .with_context(|| "复制 sidecar 日志句柄失败")?;

        let mut command = Command::new(&self.config.binary);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .env(PLUGIN_ID_ENV, &self.config.plugin_id)
            .env(PLUGIN_VERSION_ENV, &self.config.plugin_version)
            .env(PLUGIN_ENDPOINT_ENV, &self.config.endpoint)
            .env(PLUGIN_DATA_DIR_ENV, &self.config.data_dir);
        // 免沙箱 sidecar（宿主策略直启以继承 TCC 等授权，本就持有完整
        // 文件系统访问）无条件注入存储根：这只是路径指针而非提权；缺省
        // 时其媒体产物（如 computer-use 截图）只能退回系统临时目录，
        // 会话中的引用会随系统清理失效。
        if self.config.sensitive_storage.any() || !self.config.sandbox {
            command.env(STORAGE_ROOT_ENV, &self.config.storage_root);
        }
        // 注入本机 server 连接信息（scheduler 等需回调 host 的 sidecar 使用）。
        if let Some(url) = self.config.server_url.as_deref() {
            command.env(SERVER_URL_ENV, url);
        }
        if let Some(token) = self.config.server_token.as_deref() {
            command.env(SERVER_TOKEN_ENV, token);
        }
        // 通过单独的 JSON 信封传递 exec_env，避免把贡献值直接暴露给所有 sidecar。
        if self.config.plugin_id == "command"
            && let Ok(env) = self.exec_env.lock()
            && !env.is_empty()
            && let Ok(json) = serde_json::to_string(&*env)
        {
            command.env(EXEC_ENV_JSON_ENV, json);
        }
        configure_detached(&mut command);

        let mut child = command
            .spawn()
            .with_context(|| format!("启动 sidecar 失败: {}", self.config.binary.display()))?;
        self.owns_process.store(true, Ordering::Release);
        let pid = child.id();
        tracing::info!(
            plugin_id = %self.config.plugin_id,
            pid,
            binary = %self.config.binary.display(),
            "插件 sidecar 已启动"
        );
        std::thread::Builder::new()
            .name(format!("plugin-sidecar-reaper-{}", self.config.plugin_id))
            .spawn(move || {
                if let Ok(status) = child.wait() {
                    tracing::warn!(pid, %status, "插件 sidecar 已退出");
                }
            })
            .with_context(|| "创建 sidecar 回收线程失败")?;
        Ok(())
    }

    fn invoke_protocol_once(
        &self,
        operation: &str,
        payload: serde_json::Value,
        wait: ResponseWait,
    ) -> Result<serde_json::Value> {
        self.invoke_protocol_once_inner(operation, payload, None, wait, &mut |_| {})
    }

    fn invoke_protocol_once_with_progress(
        &self,
        operation: &str,
        payload: serde_json::Value,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<serde_json::Value> {
        self.invoke_protocol_once_inner(
            operation,
            payload,
            None,
            ResponseWait::UntilCancelled,
            on_progress,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn invoke_protocol_once_inner(
        &self,
        operation: &str,
        payload: serde_json::Value,
        invocation: Option<&crate::protocol::RequestInvocationContext>,
        wait: ResponseWait,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<serde_json::Value> {
        let endpoint = load_endpoint(&self.config.endpoint)?;
        let mut stream = connect(&endpoint, self.config.request_timeout)?;
        // 等待策略显式作用于读取端：握手类往返有限等待，业务 Handler 由
        // 取消终止；写入端始终保留 request_timeout 保护，防永久阻塞。
        stream
            .set_read_timeout(match wait {
                ResponseWait::Bounded(timeout) => Some(timeout),
                ResponseWait::UntilCancelled => None,
            })
            .with_context(|| "设置 sidecar 读取等待策略失败")?;
        stream
            .set_write_timeout(Some(self.config.request_timeout))
            .with_context(|| "设置 sidecar 写入保护时限失败")?;
        write_frame(
            &mut stream,
            &IpcFrame::Auth(IpcAuth {
                token: endpoint.token,
            }),
        )?;

        let request = Request::new(operation, payload);
        let request_id = request.request_id.clone();
        let _active_guard = invocation.map(|context| {
            ActiveTcpRequestGuard::new(
                Arc::clone(&self.active_requests),
                context.session_id.clone(),
                request_id.clone(),
            )
        });
        let mut envelope =
            serde_json::to_value(request).with_context(|| "序列化 sidecar 协议请求失败")?;
        if let Some(context) = invocation.cloned()
            && let Some(object) = envelope.as_object_mut()
        {
            object.insert(
                "context".to_string(),
                serde_json::to_value(context).with_context(|| "序列化 sidecar 调用上下文失败")?,
            );
        }
        write_frame(
            &mut stream,
            &IpcFrame::Request(IpcRequest {
                request_id: request_id.clone(),
                payload: envelope,
            }),
        )?;

        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .with_context(|| "读取 sidecar 响应失败")?;
            if line.is_empty() {
                bail!("sidecar 在返回响应前关闭连接");
            }
            match serde_json::from_str::<IpcFrame>(line.trim_end())
                .with_context(|| "解析 sidecar 响应帧失败")?
            {
                IpcFrame::Progress {
                    request_id: progress_request_id,
                    message,
                } if progress_request_id == request_id => on_progress(message),
                IpcFrame::Progress {
                    request_id: progress_request_id,
                    ..
                } => bail!(
                    "sidecar 进度编号不匹配: expected={request_id}, actual={progress_request_id}"
                ),
                IpcFrame::Response(response) if response.request_id == request_id => {
                    let response: Response = serde_json::from_value(response.payload)
                        .with_context(|| "解析 sidecar 协议响应失败")?;
                    if response.protocol_version != PROTOCOL_VERSION {
                        return Err(SidecarInvokeError::ProtocolMismatch(format!(
                            "sidecar 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                            response.protocol_version
                        ))
                        .into());
                    }
                    if response.request_id != request_id {
                        bail!(
                            "sidecar 协议响应编号不匹配: expected={request_id}, actual={}",
                            response.request_id
                        );
                    }
                    if !response.success {
                        let message = response
                            .error_message
                            .unwrap_or_else(|| "sidecar 请求失败".to_string());
                        let error = match response.error_code {
                            Some(ErrorCode::Timeout) => SidecarInvokeError::Timeout,
                            Some(ErrorCode::PermissionDenied) => {
                                SidecarInvokeError::PermissionDenied
                            }
                            Some(ErrorCode::ProtocolMismatch) => {
                                SidecarInvokeError::ProtocolMismatch(message)
                            }
                            Some(ErrorCode::Unavailable | ErrorCode::ServiceDisabled) => {
                                SidecarInvokeError::Unavailable(message)
                            }
                            _ => SidecarInvokeError::Internal(message),
                        };
                        return Err(error.into());
                    }
                    return Ok(response.payload.unwrap_or(serde_json::Value::Null));
                }
                IpcFrame::Response(response) => bail!(
                    "sidecar 响应编号不匹配: expected={request_id}, actual={}",
                    response.request_id
                ),
                IpcFrame::Error { message } => bail!("sidecar 返回错误: {message}"),
                _ => bail!("sidecar 返回了无效响应帧"),
            }
        }
    }
}

impl SidecarConnection for ProcessSidecarConnection {
    fn invoke(&self, operation: &str, payload: &str) -> Result<String> {
        self.invoke_with_progress(operation, payload, &mut |_| {})
    }

    fn invoke_with_progress(
        &self,
        operation: &str,
        payload: &str,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        self.ensure_running().map_err(|error| {
            if error.downcast_ref::<SidecarInvokeError>().is_some() {
                error
            } else {
                SidecarInvokeError::Unavailable(error.to_string()).into()
            }
        })?;
        let payload = serde_json::from_str(payload).with_context(|| "sidecar 请求不是有效 JSON")?;
        let response = self
            .invoke_protocol_once_with_progress(operation, payload, on_progress)
            .map_err(classify_transport_error)?;
        serde_json::to_string(&response).with_context(|| "序列化 sidecar 响应失败")
    }

    fn invoke_with_context_and_progress(
        &self,
        operation: &str,
        payload: &str,
        context: &SidecarInvocationContext,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        let context = request_invocation_context(context);
        self.invoke_with_invocation_context_and_progress(operation, payload, &context, on_progress)
    }

    fn invoke_with_invocation_context_and_progress(
        &self,
        operation: &str,
        payload: &str,
        context: &crate::protocol::RequestInvocationContext,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        self.ensure_running().map_err(|error| {
            if error.downcast_ref::<SidecarInvokeError>().is_some() {
                error
            } else {
                SidecarInvokeError::Unavailable(error.to_string()).into()
            }
        })?;
        let payload = serde_json::from_str(payload).with_context(|| "sidecar 请求不是有效 JSON")?;
        let response = self
            .invoke_protocol_once_inner(
                operation,
                payload,
                Some(context),
                ResponseWait::UntilCancelled,
                on_progress,
            )
            .map_err(classify_transport_error)?;
        serde_json::to_string(&response).with_context(|| "序列化 sidecar 响应失败")
    }

    fn handles_tool(&self, tool_name: &str) -> Result<bool> {
        self.ensure_running()?;
        let handshake = self.handshake_exchange()?;
        Ok(handshake_handles_tool(&handshake, tool_name))
    }

    fn verify_capabilities(&self) -> Result<Vec<String>> {
        self.ensure_running()?;
        let handshake = self.handshake_exchange()?;
        Ok(handshake.capabilities)
    }

    fn cancel_session(&self, session_id: &str) -> Result<()> {
        let request_ids = self
            .active_requests
            .lock()
            .map_err(|_| anyhow::anyhow!("TCP sidecar 活跃请求表已损坏"))?
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        if request_ids.is_empty() {
            return Ok(());
        }
        let endpoint = load_endpoint(&self.config.endpoint)?;
        for request_id in request_ids {
            let mut stream = connect(&endpoint, self.config.request_timeout)?;
            write_frame(
                &mut stream,
                &IpcFrame::Auth(IpcAuth {
                    token: endpoint.token.clone(),
                }),
            )?;
            write_frame(&mut stream, &IpcFrame::Cancel { request_id })?;
        }
        Ok(())
    }

    fn update_exec_env(&self, env: std::collections::BTreeMap<String, String>) {
        if let Ok(mut guard) = self.exec_env.lock() {
            *guard = env;
        }
    }

    fn stop(&self) -> Result<()> {
        ProcessSidecarConnection::stop(self)
    }

    fn plugin_id(&self) -> &str {
        ProcessSidecarConnection::plugin_id(self)
    }

    fn ensure_running(&self) -> Result<()> {
        ProcessSidecarConnection::ensure_running(self)
    }

    fn has_runtime_endpoint(&self) -> bool {
        ProcessSidecarConnection::has_runtime_endpoint(self)
    }
}

fn classify_transport_error(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<SidecarInvokeError>().is_some() {
        return error;
    }

    let io_error = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>());
    match io_error.map(std::io::Error::kind) {
        Some(std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {
            SidecarInvokeError::Timeout.into()
        }
        Some(
            std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof,
        ) => SidecarInvokeError::Unavailable(error.to_string()).into(),
        _ => SidecarInvokeError::Internal(error.to_string()).into(),
    }
}

fn load_endpoint(path: &Path) -> Result<IpcEndpoint> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 sidecar endpoint 失败: {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("解析 sidecar endpoint 失败: {}", path.display()))
}

/// sidecar 通知转发回调：`(plugin_id, channel, payload)`。
///
/// 常驻通知连接收到 Notification 帧后调用；桌面入口注入后转 bridge 事件。
pub type SidecarNotificationForwarder = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

static SIDECAR_NOTIFICATION_FORWARDER: std::sync::OnceLock<SidecarNotificationForwarder> =
    std::sync::OnceLock::new();

/// 注入 sidecar 通知转发回调（宿主入口启动时调用）。
pub fn set_sidecar_notification_forwarder(forwarder: SidecarNotificationForwarder) {
    let _ = SIDECAR_NOTIFICATION_FORWARDER.set(forwarder);
}

/// 当前通知转发回调（stdio 读线程与 TCP 通知监听共用）。
pub(crate) fn sidecar_notification_forwarder() -> Option<SidecarNotificationForwarder> {
    SIDECAR_NOTIFICATION_FORWARDER.get().cloned()
}

/// sidecar 通知常驻连接：认证后只读，收到 Notification 帧即转发。
/// 连接断开（sidecar 重启）时指数退避重连；任务句柄 detached（随进程存活）。
fn spawn_sidecar_notification_listener(
    plugin_id: String,
    endpoint_path: std::path::PathBuf,
    token: String,
    listener_token: Arc<Mutex<Option<String>>>,
) -> bool {
    let forwarder = SIDECAR_NOTIFICATION_FORWARDER.get().cloned();
    // 常驻任务需要 tokio reactor；无 runtime 上下文（同步宿主/测试环境）时
    // 跳过监听（sidecar 请求-响应不受影响，仅无主动通知推送）。
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!(plugin_id = %plugin_id, "无 tokio 上下文，跳过 sidecar 通知监听");
        return false;
    };
    let task = handle.spawn(async move {
        let mut backoff_ms = 250u64;
        loop {
            if !notification_listener_is_current(&listener_token, &token) {
                break;
            }
            match run_notification_connection(
                &plugin_id,
                &endpoint_path,
                &token,
                forwarder.as_ref(),
            )
            .await
            {
                Ok(()) => break, // endpoint 已换代，本监听退出
                Err(error) => {
                    tracing::debug!(
                        plugin_id = %plugin_id,
                        %error,
                        "sidecar 通知连接断开，重连"
                    );
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(5_000);
        }
        clear_notification_listener(&listener_token, &token);
    });
    std::mem::forget(task);
    true
}

fn notification_listener_is_current(listener_token: &Mutex<Option<String>>, token: &str) -> bool {
    listener_token
        .lock()
        .is_ok_and(|current_token| current_token.as_deref() == Some(token))
}

fn clear_notification_listener(listener_token: &Mutex<Option<String>>, token: &str) {
    if let Ok(mut current_token) = listener_token.lock()
        && current_token.as_deref() == Some(token)
    {
        *current_token = None;
    }
}

async fn run_notification_connection(
    plugin_id: &str,
    endpoint_path: &std::path::Path,
    token: &str,
    forwarder: Option<&SidecarNotificationForwarder>,
) -> Result<()> {
    let endpoint_json = tokio::fs::read_to_string(endpoint_path)
        .await
        .with_context(|| "读取 sidecar endpoint 失败")?;
    let endpoint: crate::protocol::IpcEndpoint =
        serde_json::from_str(&endpoint_json).with_context(|| "解析 sidecar endpoint 失败")?;
    // endpoint 已被新的 sidecar 替换时，本监听只退出。新 token 对应的监听
    // 由新的 ensure 调用创建，旧任务不能借新凭证继续连接，否则会重复转发通知。
    if endpoint.token != token {
        return Ok(());
    }
    let address = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("sidecar 地址为空"))?;
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::net::TcpStream::connect(address),
    )
    .await
    .with_context(|| "连接 sidecar 超时")??;

    let mut stream = stream;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let auth = serde_json::to_string(&crate::protocol::IpcFrame::Auth(crate::protocol::IpcAuth {
        token: token.to_string(),
    }))?;
    stream.write_all(auth.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;

    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            bail!("sidecar 通知连接已关闭");
        }
        match serde_json::from_str::<crate::protocol::IpcFrame>(line.trim_end()) {
            Ok(crate::protocol::IpcFrame::Notification { channel, payload }) => {
                if channel == crate::protocol::TOOLS_RECOVERED_CHANNEL {
                    crate::registry::dispatch_tools_recovered(plugin_id, &payload);
                }
                if let Some(forwarder) = forwarder {
                    forwarder(plugin_id, &channel, &payload);
                }
            }
            Ok(_) => { /* 其他帧与通知连接无关，忽略 */ }
            Err(error) => {
                tracing::debug!(plugin_id, %error, "sidecar 通知帧解析失败");
            }
        }
    }
}

fn connect(endpoint: &IpcEndpoint, timeout: Duration) -> Result<TcpStream> {
    let address = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .with_context(|| "解析 sidecar 地址失败")?
        .next()
        .ok_or_else(|| anyhow::anyhow!("sidecar 地址为空"))?;
    connect_address(address, timeout)
}

fn connect_address(address: SocketAddr, timeout: Duration) -> Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&address, timeout)
        .with_context(|| format!("连接 sidecar 失败: {address}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .with_context(|| "设置 sidecar 读取超时失败")?;
    stream
        .set_write_timeout(Some(timeout))
        .with_context(|| "设置 sidecar 写入超时失败")?;
    Ok(stream)
}

fn write_frame(stream: &mut TcpStream, frame: &IpcFrame) -> Result<()> {
    serde_json::to_writer(&mut *stream, frame).with_context(|| "序列化 sidecar 请求帧失败")?;
    stream
        .write_all(b"\n")
        .with_context(|| "写入 sidecar 请求失败")?;
    stream.flush().with_context(|| "刷新 sidecar 请求失败")
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(pid, 0) 仅检测进程是否存在，信号 0 不实际发送信号。
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    // ESRCH 表示进程不存在；其它错误（如 EPERM，进程存在但无权限）视为存活。
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    let mut command = Command::new("tasklist");
    suppress_console(&mut command);
    command
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// 按 image 名清理指定 sidecar 二进制的所有残留进程。
///
/// 升级/卸载时停掉注册表里的连接未必覆盖全部进程——热加载覆盖连接时，旧 sidecar
/// 进程可能未被停止而成为孤儿，持续占用二进制文件，导致目录改名/删除失败。此处
/// 按二进制文件名兜底清理该 image 的全部进程（taskkill /IM 在无匹配进程时直接
/// 失败，属正常情况，忽略即可）。
#[cfg(windows)]
pub fn kill_sidecar_processes_by_image(binary: &Path) {
    let Some(name) = binary.file_name().and_then(|value| value.to_str()) else {
        return;
    };
    let mut command = Command::new("taskkill");
    suppress_console(&mut command);
    let status = command.args(["/IM", name, "/F"]).status();
    if let Ok(status) = status
        && status.success()
    {
        tracing::info!(image = %name, "已清理残留 sidecar 进程");
    }
}

#[cfg(not(windows))]
pub fn kill_sidecar_processes_by_image(_binary: &Path) {}

/// 为 taskkill/tasklist 等辅助命令抑制 Windows 控制台窗口。
///
/// 与拉起 sidecar 用的 [`configure_detached`] 不同：这里只需 CREATE_NO_WINDOW，
/// 无需 DETACHED_PROCESS（辅助命令短暂运行即退出）。
#[cfg(windows)]
fn suppress_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<()> {
    let pid = i32::try_from(pid).with_context(|| "sidecar PID 超出系统范围")?;
    // SAFETY: kill 只向已通过握手确认的 sidecar PID 发送 SIGTERM。
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error).with_context(|| format!("停止 sidecar 进程失败: pid={pid}"))
    }
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<()> {
    let mut command = Command::new("taskkill");
    suppress_console(&mut command);
    let status = command
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .with_context(|| format!("停止 sidecar 进程失败: pid={pid}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("停止 sidecar 进程失败: pid={pid}, status={status}")
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_process(_pid: u32) -> Result<()> {
    bail!("当前平台不支持停止 sidecar 进程")
}

/// 终止 sidecar 后轮询进程是否真正退出。
///
/// 不能依赖 endpoint 文件：Windows `taskkill /F` 强杀不触发 sidecar 析构，
/// endpoint 永不被清理。这里直接轮询进程本身，进程退出后再给文件句柄一点
/// 释放缓冲，确保后续删除/覆盖二进制不会因句柄占用失败。
fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            // Windows 上进程对象销毁与文件句柄释放可能略滞后于进程退出。
            #[cfg(windows)]
            std::thread::sleep(Duration::from_millis(100));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("等待 sidecar 进程退出超时: pid={pid}")
}

#[cfg(unix)]
fn configure_detached(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec 中只调用 async-signal-safe 的 setsid。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn configure_detached(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}

#[cfg(not(any(unix, windows)))]
fn configure_detached(_command: &mut Command) {}

#[cfg(test)]
mod sandbox_switch_tests {
    use super::*;

    fn config() -> SidecarConfig {
        SidecarConfig::new(
            "terminal",
            "0.1.0",
            "/tmp/terminal-sidecar",
            "/tmp/endpoint.json",
            "/tmp/sidecar.log",
            "/tmp/data",
            "/tmp/storage",
        )
    }

    #[test]
    fn sandbox_user_policy_ignores_missing_directories() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("existing");
        let missing = root.path().join("missing");
        std::fs::create_dir(&existing).unwrap();
        let policy = tiangong_config::SandboxUserPolicy {
            directory_allowlist: vec![
                existing.display().to_string(),
                missing.display().to_string(),
            ],
            environment_blocklist: vec!["SECRET".to_string()],
        };

        let configured = config().with_sandbox_user_policy(&policy);
        assert!(configured.sandbox_extra_writable.contains(&existing));
        assert!(!configured.sandbox_extra_writable.contains(&missing));
        assert_eq!(configured.sandbox_environment_blocklist, vec!["SECRET"]);
    }

    #[test]
    fn user_switch_only_disables_opted_in_sandbox() {
        let follows = config().with_sandbox(true).with_sandbox_user_switch(true);
        assert!(follows.sandbox_enabled_for_spawn(false));
        assert!(!follows.sandbox_enabled_for_spawn(true));

        let forced = config().with_sandbox(true).with_sandbox_user_switch(false);
        assert!(forced.sandbox_enabled_for_spawn(false));
        assert!(forced.sandbox_enabled_for_spawn(true));

        let unsandboxed = config().with_sandbox(false).with_sandbox_user_switch(true);
        assert!(!unsandboxed.sandbox_enabled_for_spawn(false));
        assert!(!unsandboxed.sandbox_enabled_for_spawn(true));
    }

    #[test]
    fn handler_capability_matches_exact_tool_or_wildcard() {
        let handshake = HandshakeResponse {
            plugin_id: "demo".into(),
            plugin_version: "1.0.0".into(),
            sidecar_version: "1.0.0".into(),
            protocol_version: PROTOCOL_VERSION.into(),
            business_protocol: 0,
            capabilities: vec!["tool:run_command".into()],
            instance_id: "instance".into(),
            status: crate::protocol::ServiceStatus::Ready,
        };
        assert!(handshake_handles_tool(&handshake, "run_command"));
        assert!(!handshake_handles_tool(&handshake, "terminal_open"));
        let wildcard = HandshakeResponse {
            capabilities: vec!["tool:*".into()],
            ..handshake
        };
        assert!(handshake_handles_tool(&wildcard, "terminal_open"));
    }
}
