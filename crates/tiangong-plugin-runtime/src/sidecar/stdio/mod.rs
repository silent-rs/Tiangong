//! stdio 传输的宿主侧连接器（RFC 0017 D16 / S2）。
//!
//! 帧协议与 TCP 完全一致（JSON Lines + Auth 首帧 + Request/Response/Progress/
//! Notification），仅传输通道不同：spawn 时以继承管道直连子进程。
//! 由此沙箱内可零网络放行、无监听端口；Auth token 由宿主生成、经
//! `TIANGONG_PLUGIN_STDIO_TOKEN` 注入子进程，首帧校验。
//!
//! 生命周期与 TCP 版（detached + endpoint 换代重启）不同：stdio 模式下
//! 父子进程强绑定，宿主退出即管道关闭；sidecar 崩溃由下次 invoke 检测并
//! 自动重启（换代）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::protocol::{
    HANDSHAKE_OPERATION, HandshakeResponse, IpcAuth, IpcFrame, IpcRequest, PROTOCOL_VERSION,
    Request, Response,
};
use crate::sidecar::{
    EXEC_ENV_JSON_ENV, PLUGIN_DATA_DIR_ENV, PLUGIN_ENDPOINT_ENV, PLUGIN_ID_ENV, PLUGIN_VERSION_ENV,
    ResponseWait, SERVER_TOKEN_ENV, SERVER_URL_ENV, STORAGE_ROOT_ENV, SidecarConfig,
    SidecarConnection, SidecarInvokeError,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

mod process;
mod sandbox_exemptions;

use process::{
    SpawnAttemptError, apply_user_environment_policy, configure_process_lifecycle, preparation,
    prepare_policy_fd, sanitize_spawn_environment, terminate_process_tree,
};
#[cfg(windows)]
use process::{WindowsJob, WindowsLifecycle, WindowsStopEvent};
use sandbox_exemptions::{
    apply_user_cache_write, exempt_authorized_reads, exempt_authorized_user_credentials,
    exempt_mcp_config_write, resolve_launcher, retain_existing_writable_roots,
    tiangong_protected_paths,
};

#[cfg(test)]
mod tests;

/// 子进程环境：传输模式标记。
pub const TRANSPORT_ENV: &str = "TIANGONG_PLUGIN_TRANSPORT";
/// 子进程环境：stdio 模式的认证 token。
pub const STDIO_TOKEN_ENV: &str = "TIANGONG_PLUGIN_STDIO_TOKEN";
/// 子进程环境：创建并持有 stdio 连接的宿主进程 PID。
pub const HOST_PID_ENV: &str = "TIANGONG_PLUGIN_HOST_PID";
pub const TRANSPORT_STDIO: &str = "stdio";
const PROCESS_GROUP_ENV: &str = "TIANGONG_SIDECAR_OWN_PROCESS_GROUP";

/// stdio 传输连接：单子进程 + 常驻读线程按 request_id 路由响应与进度，
/// Notification 帧走全局通知转发（与 TCP 通知监听等价）。
pub struct StdioSidecarConnection {
    config: SidecarConfig,
    state: Mutex<StdioState>,
    exec_env: Mutex<std::collections::BTreeMap<String, String>>,
    stopped: AtomicBool,
}

struct StdioState {
    process: Option<Arc<StdioProcess>>,
}

struct StdioProcess {
    child: Arc<Mutex<Child>>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<String, PendingWaiter>>>,
    /// 本进程代次的认证 token（Auth 首帧内容，子进程经环境变量持有同值）。
    token: String,
    /// 本进程是否已发送过 Auth 首帧。
    authenticated: AtomicBool,
    /// 本进程代次就绪握手的结果（身份已校验；capabilities 仅供诊断）。
    /// 进程启动后只握手一次，换代重建时随之失效。
    readiness: Mutex<Option<HandshakeResponse>>,
    /// stdout 读线程观察到 EOF/错误后置位；并发请求据此无阻塞换代。
    closed: Arc<AtomicBool>,
    #[cfg(windows)]
    lifecycle: WindowsLifecycle,
}

#[derive(Clone)]
struct PendingWaiter {
    response: SyncSender<Result<Value, String>>,
    progress: SyncSender<String>,
    invocation: Option<crate::sidecar::SidecarInvocationContext>,
    invocation_context: Option<crate::protocol::RequestInvocationContext>,
}

impl PendingWaiter {
    /// 请求是否属于目标会话。新版统一调用上下文与旧版 sidecar 上下文都
    /// 参与判断；同一会话在两种上下文形态下发起的请求都会被会话级
    /// 联取消命中。
    fn belongs_to_session(&self, session_id: &str) -> bool {
        self.invocation_context
            .as_ref()
            .is_some_and(|context| context.session_id == session_id)
            || self
                .invocation
                .as_ref()
                .is_some_and(|context| context.session_id == session_id)
    }
}

/// 提取本次调用的宿主权威工作区。
///
/// 新版 Runtime 调用优先使用 RequestInvocationContext；旧版 sidecar
/// 调用继续兼容 SidecarInvocationContext。两者都不存在时，由调用方
/// 回退到连接配置中的默认工作区。
///
/// SidecarInvocationContext 将在现有插件全部迁移到统一调用上下文后
/// 再评估淘汰，当前不得删除（兼容层约定见 issue #479）。
fn invocation_workspace<'a>(
    legacy_context: Option<&'a crate::sidecar::SidecarInvocationContext>,
    runtime_context: Option<&'a crate::protocol::RequestInvocationContext>,
) -> Option<&'a Path> {
    runtime_context
        .map(|context| context.workspace.trim())
        .filter(|workspace| !workspace.is_empty())
        .map(Path::new)
        .or_else(|| legacy_context.map(|context| context.authoritative_workspace.as_path()))
}

impl StdioSidecarConnection {
    pub fn new(config: SidecarConfig) -> Self {
        Self {
            config,
            state: Mutex::new(StdioState { process: None }),
            exec_env: Mutex::new(std::collections::BTreeMap::new()),
            stopped: AtomicBool::new(false),
        }
    }

    /// 当前 sidecar 的插件 ID。
    pub fn plugin_id(&self) -> &str {
        &self.config.plugin_id
    }

    /// 更新 exec_env（下次 spawn 时注入）。
    pub fn update_exec_env(&self, env: std::collections::BTreeMap<String, String>) {
        if let Ok(mut guard) = self.exec_env.lock() {
            *guard = env;
        }
    }

    /// 停止子进程（宿主关闭流程调用）。清理失败（信号被拒/限时未退出）
    /// 返回 Err——严格验证路径据此判定清理未完成。
    pub fn stop(&self) -> Result<()> {
        // stop 是终止语义。先置位可阻止与取消并发的 invoke 在进程被杀后重启。
        self.stopped.store(true, Ordering::Release);
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("stdio sidecar 状态锁已损坏"))?;
        if let Some(process) = state.process.take()
            && let Ok(mut child) = process.child.lock()
        {
            terminate_process_tree(&process, &mut child)?;
        }
        Ok(())
    }

    /// 确保进程存活并完成就绪握手，返回本代次握手声明的能力列表
    /// （安装验证 / 预热 / 诊断用）。
    ///
    /// 按需生命周期不保留进程：临时启动、握手校验后立即清理——预热/安装
    /// 验证仍确认可达性，但不留下任何存活进程。常驻进程复用同一代次的
    /// 就绪握手结果，不重复握手。
    pub fn ensure_running_checked(&self) -> Result<Vec<String>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("stdio sidecar 状态锁已损坏"))?;
        match self.config.lifecycle {
            crate::manifest::SidecarLifecycle::OnDemand => {
                if self.stopped.load(Ordering::Acquire) {
                    bail!("stdio sidecar 已停止");
                }
                // 临时校验进程完全不经过共享 state：按需调用可能正在进行
                //（state.process 是其活跃进程），经 state 启停会误杀它。
                let process = Arc::new(self.spawn(None)?);
                let result = self.handshake_exchange(&process);
                let cleanup = process
                    .child
                    .lock()
                    .map_err(|_| anyhow!("stdio sidecar 子进程锁已损坏"))
                    .and_then(|mut child| terminate_process_tree(&process, &mut child));
                // 严格验证语义：临时验证进程清理失败时即使握手成功也判失败
                //（残留进程会与后续安装冲突），与 connection.stop 的判定一致。
                let capabilities = result.map(|handshake| handshake.capabilities);
                match (capabilities, cleanup) {
                    (Ok(capabilities), Ok(())) => Ok(capabilities),
                    (Err(handshake_error), Ok(())) => Err(handshake_error),
                    (_, Err(cleanup_error)) => {
                        Err(cleanup_error).context("stdio sidecar 临时验证进程清理失败")
                    }
                }
            }
            crate::manifest::SidecarLifecycle::Resident => {
                let process = self.ensure_running(&mut state)?;
                process
                    .readiness
                    .lock()
                    .map_err(|_| anyhow!("stdio sidecar 就绪状态锁已损坏"))?
                    .clone()
                    .map(|handshake| handshake.capabilities)
                    .ok_or_else(|| anyhow!("stdio sidecar 就绪握手结果丢失"))
            }
        }
    }

    /// 确保子进程存活且完成过握手。进程退出时自动重启（换代）。
    fn ensure_running(&self, state: &mut StdioState) -> Result<Arc<StdioProcess>> {
        if self.stopped.load(Ordering::Acquire) {
            bail!("stdio sidecar 已停止");
        }
        if let Some(process) = state.process.as_ref() {
            if !process.closed.load(Ordering::Acquire) {
                return Ok(Arc::clone(process));
            }
            let process = state.process.take().expect("已确认存在的进程代次");
            if let Ok(mut child) = process.child.lock() {
                // 已断流进程的回收尽力而为：清理失败不阻塞换代重启。
                let _ = terminate_process_tree(&process, &mut child);
            }
            tracing::warn!(plugin_id = %self.config.plugin_id, "stdio sidecar 已退出，准备重启");
        }
        self.start_fresh(state, None)
    }

    /// 启动全新进程并完成就绪握手（写入 state.process）。
    /// 覆盖前先终止旧进程——预热残留或异常路径留下的进程不允许失去管理引用。
    fn start_fresh(
        &self,
        state: &mut StdioState,
        sandbox_workspace: Option<&Path>,
    ) -> Result<Arc<StdioProcess>> {
        if let Some(process) = state.process.take()
            && let Ok(mut child) = process.child.lock()
        {
            // 覆盖清理尽力而为：旧进程已断流，残留不阻塞新代次启动。
            let _ = terminate_process_tree(&process, &mut child);
        }
        let process = self.spawn_ready(sandbox_workspace)?;
        state.process = Some(Arc::clone(&process));
        Ok(process)
    }

    /// 启动私有进程并完成就绪握手（写入进程自身的 readiness 缓存）。
    /// 不触碰共享 state——并发调用各自的进程互不误杀；是否登记
    /// state.process（全局终止/换代语义用）由调用方决定。
    fn spawn_ready(&self, sandbox_workspace: Option<&Path>) -> Result<Arc<StdioProcess>> {
        let process = Arc::new(self.spawn(sandbox_workspace)?);
        match self.handshake_exchange(&process) {
            Ok(handshake) => {
                *process
                    .readiness
                    .lock()
                    .map_err(|_| anyhow!("stdio sidecar 就绪状态锁已损坏"))? = Some(handshake);
            }
            Err(error) => {
                let cleanup = process
                    .child
                    .lock()
                    .ok()
                    .and_then(|mut child| terminate_process_tree(&process, &mut child).ok());
                if cleanup.is_none() {
                    tracing::warn!(
                        plugin_id = %self.config.plugin_id,
                        "握手失败的进程清理未完成，可能残留"
                    );
                }
                return Err(error);
            }
        }
        tracing::info!(
            plugin_id = %self.config.plugin_id,
            "stdio sidecar 已就绪"
        );
        Ok(process)
    }

    /// 按需调用：每次请求独立进程（spawn → 握手 → 请求 → 清理），
    /// 不复用也不保留进程——工具型调用的最小存活窗口。
    ///
    /// 锁只在起止瞬间持有：round_trip 期间不持锁，会话取消
    /// （cancel_current）才能及时终止进行中的进程而不与调用方互等。
    fn invoke_on_demand(
        &self,
        operation: &str,
        payload: Value,
        invocation: Option<crate::sidecar::SidecarInvocationContext>,
        invocation_context: Option<crate::protocol::RequestInvocationContext>,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<Value> {
        let sandbox_workspace =
            invocation_workspace(invocation.as_ref(), invocation_context.as_ref())
                .map(validate_invocation_workspace)
                .transpose()?;
        if self.stopped.load(Ordering::Acquire) {
            bail!("stdio sidecar 已停止");
        }
        // 按需调用进程完全私有，不登记共享 state.process：并发调用
        //（如 UI 消息与工具调用同时到达）各自 spawn/握手/清理，先启动
        // 方的进程不会被后启动方经单槽登记误杀；state.process 仅供
        // stop 等全局终止语义使用（与 ensure_running_checked 的临时
        // 校验进程同一原则）。
        let process = self.spawn_ready(sandbox_workspace.as_deref())?;
        let result = self.round_trip(
            &process,
            operation,
            payload,
            invocation,
            invocation_context,
            ResponseWait::UntilCancelled,
            on_progress,
        );
        if let Ok(mut child) = process.child.lock() {
            // 业务结果优先：收尾清理尽力而为，失败仅告警（进程可能残留，
            // 下一次调用不受影响——私有进程不与任何登记交互）。
            let _ = terminate_process_tree(&process, &mut child);
        }
        result
    }

    fn finish_waiter_then_cancel(
        &self,
        waiter: Option<PendingWaiter>,
        request_id: &str,
        cancel: impl FnOnce() -> Result<()>,
    ) {
        // 必须先唤醒调用方，再尽力写 Cancel；否则写端已关闭时会退化到请求超时。
        if let Some(waiter) = waiter {
            let _ = waiter.response.send(Err("请求已取消".to_string()));
        }
        if let Err(error) = cancel() {
            tracing::debug!(
                plugin_id = %self.config.plugin_id,
                %request_id,
                %error,
                "stdio sidecar 已不可通信，忽略取消帧写入失败"
            );
        }
    }

    /// 按宿主会话取消其仍在执行的请求（会话级联取消）。
    ///
    /// 常驻 sidecar：取消属于目标会话的全部请求——请求级 Cancel 帧定向
    /// 取消，进程本身继续服务其他会话；按需 sidecar：每次调用独占进程，
    /// 直接清理进程树。取消后允许后续调用重新使用或启动 sidecar。
    pub fn cancel_session(&self, session_id: &str) -> Result<()> {
        let process = self
            .state
            .lock()
            .map_err(|_| anyhow!("stdio sidecar 状态锁已损坏"))?
            .process
            .clone();
        let Some(process) = process else {
            return Ok(());
        };
        if self.config.lifecycle == crate::manifest::SidecarLifecycle::OnDemand {
            self.cancel_current();
            return Ok(());
        }
        let request_ids = process
            .pending
            .lock()
            .map_err(|_| anyhow!("stdio sidecar pending 锁已损坏"))?
            .iter()
            .filter(|(_, waiter)| waiter.belongs_to_session(session_id))
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        for request_id in request_ids {
            let waiter = process
                .pending
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&request_id));
            self.finish_waiter_then_cancel(waiter, &request_id, || {
                self.cancel_request(&process, &request_id)
            });
        }
        Ok(())
    }

    /// 测试与诊断：当前活跃请求数。只读 pending 表，不触发进程动作。
    #[doc(hidden)]
    pub fn active_request_count(&self) -> usize {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.process.clone())
            .and_then(|process| process.pending.lock().ok().map(|pending| pending.len()))
            .unwrap_or(0)
    }

    /// 终止当前进行中的调用进程（工具级超时 / 会话取消用）：
    /// 与 stop 不同，不改变停止标志——后续调用会重新起进程继续服务。
    pub fn cancel_current(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(process) = state.process.take()
            && let Ok(mut child) = process.child.lock()
        {
            // 取消是尽力语义：终止失败不改变取消结果（waiter 已被唤醒）。
            let _ = terminate_process_tree(&process, &mut child);
        }
    }

    fn spawn(&self, sandbox_workspace: Option<&Path>) -> Result<StdioProcess> {
        match self.spawn_once(sandbox_workspace) {
            Ok(process) => Ok(process),
            Err(SpawnAttemptError::Preparation(error)) => Err(error),
            Err(SpawnAttemptError::ProcessCreation { program, source }) => {
                // 仅进程创建失败（文件被删、无执行权限、格式无效等）才
                // 走恢复接口：缓存匹配失效 → 排除失败路径重探；恢复出新
                // 路径后重试一次。spawn_once 每次都经缓存入口取最新路径，
                // 后续重启自然使用新路径。
                if let Some(launch) = self.config.interpreter.as_ref()
                    && crate::interpreter_env::recover_interpreter_after_spawn_failure(
                        launch.kind,
                        &program,
                    )
                    .is_some()
                {
                    // 第二次失败返回真实错误，不再第三次恢复；若仍是
                    // 解释器创建失败，清掉刚恢复的新缓存（已知坏路径）。
                    return match self.spawn_once(sandbox_workspace) {
                        Ok(process) => Ok(process),
                        Err(SpawnAttemptError::Preparation(error)) => Err(error),
                        Err(SpawnAttemptError::ProcessCreation {
                            program: second_program,
                            source: second_source,
                        }) => {
                            if let Some(launch) = self.config.interpreter.as_ref() {
                                crate::interpreter_env::invalidate_if_matches(
                                    launch.kind,
                                    &second_program,
                                );
                            }
                            Err(second_source)
                        }
                    };
                }
                Err(source)
            }
        }
    }

    /// 解释器形态每次启动都经缓存入口解析程序路径（命中只做一次
    /// `is_file` 校验，不触发目录扫描），不在配置中保存可能过期的
    /// 解释器路径副本。
    fn spawn_once(
        &self,
        sandbox_workspace: Option<&Path>,
    ) -> std::result::Result<StdioProcess, SpawnAttemptError> {
        if let Some(parent) = self.config.log.parent() {
            preparation(
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建 sidecar 日志目录失败: {}", parent.display())),
            )?;
        }
        preparation(
            std::fs::create_dir_all(&self.config.data_dir).with_context(|| {
                format!(
                    "创建 sidecar 数据目录失败: {}",
                    self.config.data_dir.display()
                )
            }),
        )?;
        let stderr = preparation(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.config.log)
                .with_context(|| format!("打开 sidecar 日志失败: {}", self.config.log.display())),
        )?;

        let token = scru128::new().to_string();
        // 解释器形态：以宿主缓存解析的解释器程序运行 entry（本地信任时
        // 先复核内容清单）。OS 沙箱路径（经 tiangong-sandbox Launcher 启动）
        // 在本传输层之上叠加：声明沙箱的 sidecar 一律经 Launcher 启动，
        // 策略通过继承描述符传入，见下方 launch_policy。
        let (program, target_args) = match self.config.interpreter.as_ref() {
            Some(launch) => {
                if let Some(manifest_path) = &self.config.integrity_manifest {
                    let root = preparation(
                        manifest_path
                            .parent()
                            .ok_or_else(|| anyhow!("内容清单缺少父目录")),
                    )?;
                    preparation(SidecarConfig::verify_integrity_manifest(
                        manifest_path,
                        root,
                    ))?;
                }
                let program =
                    preparation(crate::interpreter_env::resolve_interpreter(launch.kind))?;
                if !program.is_file() {
                    return Err(SpawnAttemptError::Preparation(anyhow!(
                        "解释器程序不存在: {}（可用 TIANGONG_NODE_PATH/TIANGONG_PYTHON_PATH 指定）",
                        program.display()
                    )));
                }
                if !launch.entry.is_file() {
                    return Err(SpawnAttemptError::Preparation(anyhow!(
                        "sidecar 入口脚本不存在: {}",
                        launch.entry.display()
                    )));
                }
                let mut args = Vec::new();
                #[cfg(windows)]
                if launch.kind == crate::interpreter_env::InterpreterKind::Node {
                    args.push("--preserve-symlinks".to_string());
                    args.push("--preserve-symlinks-main".to_string());
                }
                args.push(launch.entry.display().to_string());
                args.extend(launch.args.iter().cloned());
                (program, args)
            }
            None => {
                if !self.config.binary.is_file() {
                    return Err(SpawnAttemptError::Preparation(anyhow!(
                        "sidecar 二进制不存在: {}",
                        self.config.binary.display()
                    )));
                }
                (self.config.binary.clone(), Vec::new())
            }
        };
        // 用户开关仅作用于宿主标记为“首次实际使用才启动”的 sidecar。
        // 每次 spawn 都读取最新配置，关闭与重新开启无需重建连接对象；配置
        // 尚未初始化时按开启处理（fail-safe 向保护方向）。
        let sandbox_disabled = crate::registry::sandbox_disabled();
        let sandbox_enabled = self.config.sandbox_enabled_for_spawn(sandbox_disabled);
        if self.config.sandbox_follows_user_switch && sandbox_disabled {
            tracing::warn!(
                plugin_id = %self.config.plugin_id,
                "用户已关闭沙箱：按需 sidecar 将以完整用户权限启动"
            );
        }

        // 显式专用目录用于 command 等宿主需要回收的临时调用。普通 Windows
        // sidecar 使用 AppContainer 自动提供的私有 Temp；Unix 常驻 sidecar
        // 继续使用存储根下的固定目录。
        let effective_temp_dir = if let Some(temp_dir) = &self.config.sandbox_temp_dir {
            Some(temp_dir.clone())
        } else if sandbox_enabled {
            #[cfg(windows)]
            {
                None
            }
            #[cfg(not(windows))]
            {
                let dir = self
                    .config
                    .storage_root
                    .join("tmp")
                    .join(&self.config.plugin_id);
                match std::fs::create_dir_all(&dir) {
                    Ok(()) => Some(dir),
                    Err(error) => {
                        return Err(SpawnAttemptError::Preparation(anyhow!(
                            "创建 sidecar 专用临时目录失败: {}: {error:#}",
                            dir.display()
                        )));
                    }
                }
            }
        } else {
            None
        };
        // 本次实际启用沙箱时经官方 Launcher 启动；用户显式关闭且该
        // sidecar 属按需进程时直接启动目标程序。解释器形态把解释器作为
        // 目标程序，入口脚本和固定参数作为参数。
        let launch_policy = if sandbox_enabled {
            // 统一写域模型：按需调用优先采用本次宿主权威会话工作区；
            // 无调用上下文和常驻进程继续使用连接配置中的固定工作区，最后
            // 才回退存储根。敏感清单读写双禁，其余目录只读。
            let workspace = sandbox_workspace_for_spawn(&self.config, sandbox_workspace);
            let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write(workspace);
            policy.extra_writable = self.config.sandbox_extra_writable.clone();
            // 存储根统一并入可写域（显式 workspace 即存储根时自然去重）。
            policy.extra_writable.push(self.config.storage_root.clone());
            // 工具链缓存（npm 等）是功能基础设施而非用户数据：npx/uvx
            // 启动 MCP server 需要写包缓存。
            if let Some(home) = crate::interpreter_env::user_home_dir() {
                policy.extra_writable.push(home.join(".npm"));
            }
            apply_user_cache_write(&mut policy, self.config.sandbox_user_cache_write);
            if let Some(temp_dir) = &effective_temp_dir {
                policy.extra_writable.push(temp_dir.clone());
            }
            // Unix 平台继续开放系统临时目录；Windows 由 AppContainer
            // 自动提供隔离的 AC/Temp，不向策略加入宿主系统 Temp。
            #[cfg(not(windows))]
            policy.extra_writable.push(std::env::temp_dir());
            // 全局 /tmp 仅 Unix 存在——Windows 上没有此路径，加了会在
            // Seatbelt/bwrap 的路径校验中产生无效条目。
            #[cfg(unix)]
            policy.extra_writable.push(std::path::PathBuf::from("/tmp"));
            // 配置与信任件读写双禁：模型/服务/MCP/应用配置（app.json 含
            // 沙箱开关本身）、签名密钥与信任库、Launcher 目录（沙箱内
            // 进程必须够不到 Launcher 与信任锚，防替换逃逸）；家目录
            // 凭据默认同禁，随后仅按宿主验证过的插件身份移除读取禁令。
            policy.protected_paths = tiangong_protected_paths(&self.config.storage_root);
            tiangong_sandbox::sandbox::presets::apply_tiangong(
                &mut policy,
                &self.config.storage_root,
            );
            // 按宿主授权最小开放对应天工配置的读取（写禁与防篡改保持
            // 不变，密钥/信任库/Launcher 不在本段豁免之列）。
            exempt_authorized_reads(
                &mut policy,
                self.config.sensitive_storage,
                &self.config.storage_root,
            );
            // mcp.json 由 mcp 插件自管（官方身份经宿主验证后才置位
            // mcp_config）：开放其写权限，否则 bot 注册 MCP server 时
            // sidecar 无法落盘配置；其余敏感配置仍由宿主写入、插件只读。
            if self.config.sensitive_storage.mcp_config {
                exempt_mcp_config_write(&mut policy, &self.config.storage_root);
            }
            policy.allow_network = self.config.sandbox_network;
            policy.allow_audio_input = self.config.sandbox_audio_input;
            exempt_authorized_user_credentials(&mut policy, self.config.user_credential_reads);
            policy
                .denied_read_paths
                .extend(self.config.sandbox_denied_read_paths.clone());
            if let Some(limits) = &self.config.sandbox_resource_limits {
                policy.resource_limits = *limits;
            }
            retain_existing_writable_roots(&mut policy);
            Some(policy)
        } else {
            None
        };
        // fd 守卫必须存活到 spawn 完成：match 臂内绑定会在臂结束时提前
        // drop、导致沙箱程序读不到策略（审查修复）。
        let mut policy_fd_guard = None;
        // 沙箱路径下真正创建的进程是 Launcher：spawn 失败的错误上下文按
        // 实际程序呈现；program 字段仍保留解释器路径供恢复接口比对。
        let mut spawned_program: Option<std::path::PathBuf> = None;
        let mut command = match &launch_policy {
            Some(policy) => {
                let program_sha256 = preparation(
                    self.config
                        .sandbox_program_sha256
                        .clone()
                        .filter(|_| self.config.interpreter.is_none())
                        .map(Ok)
                        .unwrap_or_else(|| super::sha256_file(&program)),
                )?;
                let sandbox_bin = preparation(resolve_launcher(
                    &self.config.storage_root,
                )
                .ok_or_else(|| {
                    anyhow!(
                        "插件 {} 声明沙箱但 Launcher 未就绪（在线更新尚未完成或安装失败），拒绝启动；请稍后重试或检查网络",
                        self.config.plugin_id
                    )
                }))?;
                // Launcher 是安全边界执行者：每次启动前验签（官方或本机
                // 信任根任一通过），签名缺失/不匹配一律拒绝，防单文件替换。
                // Launcher 是安全边界执行者：每次启动前验签（官方或本机
                // 信任根任一通过），签名缺失/不匹配一律拒绝，防单文件替换。
                preparation(
                    crate::signature::verify_launcher_signature(
                        &sandbox_bin,
                        &self.config.storage_root,
                    )
                    .context("沙箱 Launcher 验签失败，拒绝启动沙箱"),
                )?;
                // 解释器形态的权威目录是解释器所在目录（目标程序不在
                // 插件目录内，Launcher 按解释器形态做退化校验）；native
                // 形态沿用插件目录（清单比对依据）。
                let program_root = preparation(
                    if self.config.interpreter.is_some() {
                        program.parent()
                    } else {
                        self.config
                            .sandbox_program_root
                            .as_deref()
                            .or_else(|| program.parent())
                    }
                    .map(|path| path.display().to_string())
                    .ok_or_else(|| anyhow!("sidecar 目标程序缺少权威目录")),
                )?;
                let request = serde_json::json!({
                    "protocol_version": tiangong_sandbox::LAUNCHER_PROTOCOL_VERSION,
                    "policy_schema": tiangong_sandbox::LAUNCHER_POLICY_SCHEMA,
                    "policy": policy,
                    "plugin_id": self.config.plugin_id,
                    "program": program.display().to_string(),
                    "program_root": program_root,
                    "program_sha256": program_sha256,
                    "args": target_args,
                    "interpreter": self.config.interpreter.is_some(),
                });
                #[cfg(windows)]
                let request = {
                    let mut request = request;
                    if !self.config.sandbox_follows_user_switch
                        && self.config.lifecycle == crate::manifest::SidecarLifecycle::Resident
                    {
                        request["grant_cache"] =
                            serde_json::json!(crate::registry::persistent_grants_path(
                                &self.config.storage_root,
                                &self.config.plugin_id
                            ));
                    }
                    request
                };
                let mut command = Command::new(&sandbox_bin);
                policy_fd_guard = Some(preparation(prepare_policy_fd(
                    &mut command,
                    request.to_string(),
                ))?);
                spawned_program = Some(sandbox_bin);
                command
            }
            None => {
                let mut command = Command::new(&program);
                command.args(&target_args);
                command
            }
        };
        sanitize_spawn_environment(&mut command);
        apply_user_environment_policy(&mut command, &self.config);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .env(TRANSPORT_ENV, TRANSPORT_STDIO)
            .env(STDIO_TOKEN_ENV, &token)
            .env(HOST_PID_ENV, std::process::id().to_string())
            .env(PLUGIN_ID_ENV, &self.config.plugin_id)
            .env(PLUGIN_VERSION_ENV, &self.config.plugin_version)
            // stdio 模式无 endpoint 文件；保留路径占位以兼容读取方。
            .env(PLUGIN_ENDPOINT_ENV, &self.config.endpoint)
            .env(PLUGIN_DATA_DIR_ENV, &self.config.data_dir)
            .env(PROCESS_GROUP_ENV, "1");
        // 注入本机 server 连接信息（scheduler/subagent 等需回调 host 的 sidecar 使用；
        // 与 TCP 版 spawn 保持一致，endpoint 变化时由重启机制换代）。
        if let Some(url) = self.config.server_url.as_deref() {
            command.env(SERVER_URL_ENV, url);
        }
        if let Some(token) = self.config.server_token.as_deref() {
            command.env(SERVER_TOKEN_ENV, token);
        }
        // 运行期解释器环境的权威来源是缓存：不修改宿主全局环境，仅对
        // 新建子进程注入覆盖（TIANGONG_*_PATH + 前置解释器目录的 PATH），
        // 恢复后的新路径由此传导给 sidecar 及其派生的命令通道进程。
        for (key, value) in crate::interpreter_env::child_env_overrides() {
            command.env(key, value);
        }
        if let Some(temp_dir) = &effective_temp_dir {
            if !temp_dir.is_absolute() || !temp_dir.is_dir() {
                return Err(SpawnAttemptError::Preparation(anyhow!(
                    "sidecar 专用临时目录无效: {}",
                    temp_dir.display()
                )));
            }
            command
                .env("TMPDIR", temp_dir)
                .env("TMP", temp_dir)
                .env("TEMP", temp_dir);
        }
        #[cfg(windows)]
        let sandbox_stop = preparation(
            launch_policy
                .as_ref()
                .map(|_| WindowsStopEvent::new())
                .transpose()
                .context("创建 Windows Sandbox 停止事件失败"),
        )?;
        #[cfg(windows)]
        if let Some(stop) = &sandbox_stop {
            command.env(tiangong_sandbox::WINDOWS_STOP_EVENT_ENV, stop.name());
        }
        // 与 tcp sidecar 的 spawn 同口径：免沙箱 sidecar 无条件注入存储根
        //（路径指针而非提权），保证其媒体产物进入统一媒体目录而非系统
        // 临时目录。
        if self.config.sensitive_storage.any() || !self.config.sandbox {
            command.env(STORAGE_ROOT_ENV, &self.config.storage_root);
        }
        if let Some(env) = self.exec_env.lock().ok().filter(|env| !env.is_empty())
            && let Ok(json) = serde_json::to_string(&*env)
        {
            command.env(EXEC_ENV_JSON_ENV, json);
        }
        preparation(configure_process_lifecycle(&mut command))?;
        #[cfg(windows)]
        let lifecycle = match sandbox_stop {
            Some(stop) => WindowsLifecycle::Sandbox(stop),
            None => WindowsLifecycle::Job(preparation(
                WindowsJob::new(None).context("创建 sidecar Job Object 失败"),
            )?),
        };
        if crate::registry::sidecars_shutting_down() {
            return Err(SpawnAttemptError::Preparation(anyhow!(
                "应用正在退出，取消插件启动"
            )));
        }
        let mut child = command.spawn().map_err(|error| {
            let display = spawned_program.as_deref().unwrap_or(&program);
            let context = format!("启动 stdio sidecar 失败: {}", display.display());
            SpawnAttemptError::ProcessCreation {
                program,
                source: anyhow::Error::new(error).context(context),
            }
        })?;
        drop(policy_fd_guard);
        #[cfg(windows)]
        if let Err(error) = lifecycle.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SpawnAttemptError::Preparation(
                Err::<(), _>(error)
                    .context("将 sidecar 加入 Job Object 失败")
                    .unwrap_err(),
            ));
        }
        let stdin = preparation(child.stdin.take().context("stdio sidecar 未提供 stdin"))?;
        let stdout = preparation(child.stdout.take().context("stdio sidecar 未提供 stdout"))?;
        let pid = child.id();
        tracing::info!(
            plugin_id = %self.config.plugin_id,
            pid,
            transport = TRANSPORT_STDIO,
            "stdio sidecar 已启动"
        );

        let pending: Arc<Mutex<HashMap<String, PendingWaiter>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let child = Arc::new(Mutex::new(child));
        let closed = Arc::new(AtomicBool::new(false));
        spawn_stdio_reader(
            self.config.plugin_id.clone(),
            stdout,
            Arc::clone(&pending),
            Arc::clone(&child),
            Arc::clone(&closed),
        );
        Ok(StdioProcess {
            child,
            stdin: Mutex::new(stdin),
            pending,
            token,
            authenticated: AtomicBool::new(false),
            readiness: Mutex::new(None),
            closed,
            #[cfg(windows)]
            lifecycle,
        })
    }

    /// 就绪/验证握手：认证链路可用且进程已进入协议处理状态，校验插件
    /// ID、版本与协议兼容，返回握手响应（capabilities 仅供诊断）。
    /// 等待受 `start_timeout` 有限保护。
    fn handshake_exchange(&self, process: &Arc<StdioProcess>) -> Result<HandshakeResponse> {
        let payload = self
            .round_trip(
                process,
                HANDSHAKE_OPERATION,
                serde_json::Value::Null,
                None,
                None,
                ResponseWait::Bounded(self.config.start_timeout),
                &mut |_| {},
            )
            .with_context(|| {
                format!(
                    "stdio sidecar 就绪握手失败（时限 {}ms）",
                    self.config.start_timeout.as_millis()
                )
            })?;
        let handshake: HandshakeResponse =
            serde_json::from_value(payload).context("解析 stdio sidecar 握手响应失败")?;
        if handshake.plugin_id != self.config.plugin_id {
            bail!(
                "stdio sidecar 插件身份不匹配: expected={}, actual={}",
                self.config.plugin_id,
                handshake.plugin_id
            );
        }
        if handshake.protocol_version != PROTOCOL_VERSION {
            return Err(SidecarInvokeError::ProtocolMismatch(format!(
                "stdio sidecar 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                handshake.protocol_version
            ))
            .into());
        }
        // 插件整体版本必须一致，避免连接到升级前残留的旧进程。
        if handshake.plugin_version != self.config.plugin_version {
            return Err(SidecarInvokeError::ProtocolMismatch(format!(
                "stdio sidecar 插件版本不匹配: expected={}, actual={}",
                self.config.plugin_version, handshake.plugin_version
            ))
            .into());
        }
        Ok(handshake)
    }

    /// deadline 语义下的最小剩余等待（排队耗尽预算时兜底）。
    const MIN_DEADLINE_REMAINING_MS: u64 = 100;

    /// 单请求往返：注册 pending → 写帧（新进程首帧前补 Auth）→ 循环收进度/响应。
    ///
    /// 等待策略由 `wait` 显式决定：握手类往返使用有限等待；业务 Handler
    /// 不设自动时限，仅由取消或进程断开终止（每轮唤醒仅排空进度）。
    #[allow(clippy::too_many_arguments)]
    fn round_trip(
        &self,
        process: &Arc<StdioProcess>,
        operation: &str,
        payload: Value,
        invocation: Option<crate::sidecar::SidecarInvocationContext>,
        invocation_context: Option<crate::protocol::RequestInvocationContext>,
        wait: ResponseWait,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<Value> {
        // 显式查询 deadline 才限制等待；普通 Agent Handler 的 None 仍无限等待到取消。
        let wait = invocation_context
            .as_ref()
            .and_then(|ctx| ctx.deadline_ms)
            .map(|deadline| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                // 排队可能吃掉全部预算：保留最小收响应窗口，让已写出的
                // 请求有机会完成，而不是 0ms 立即超时。
                let remaining = deadline
                    .saturating_sub(now)
                    .max(Self::MIN_DEADLINE_REMAINING_MS);
                ResponseWait::Bounded(Duration::from_millis(remaining))
            })
            .unwrap_or(wait);
        let bounded_deadline = match wait {
            ResponseWait::Bounded(timeout) => Some(std::time::Instant::now() + timeout),
            ResponseWait::UntilCancelled => None,
        };
        let request = Request::new(operation, payload);
        let request_id = request.request_id.clone();
        let (response_tx, response_rx) = sync_channel::<Result<Value, String>>(1);
        let (progress_tx, progress_rx) = sync_channel::<String>(64);
        process
            .pending
            .lock()
            .map_err(|_| anyhow!("stdio sidecar pending 锁已损坏"))?
            .insert(
                request_id.clone(),
                PendingWaiter {
                    response: response_tx,
                    progress: progress_tx,
                    invocation_context: invocation_context.or_else(|| {
                        invocation.as_ref().map(|context| {
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
                        })
                    }),
                    invocation,
                },
            );

        let write_result = self.write_request(process, &request);
        if let Err(error) = write_result {
            remove_pending(process, &request_id);
            return Err(SidecarInvokeError::Unavailable(error.to_string()).into());
        }

        loop {
            // 业务请求不设时限；周期唤醒仅用于排空进度和感知进程断开。
            while let Ok(message) = progress_rx.try_recv() {
                on_progress(message);
            }
            match response_rx.recv_timeout(wait.poll_interval()) {
                Ok(result) => {
                    return result.map_err(|message| {
                        let message = if message == "stdio sidecar 已关闭" {
                            format!("{message}; {}", self.failure_detail(process))
                        } else {
                            message
                        };
                        SidecarInvokeError::Internal(message).into()
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(deadline) = bounded_deadline
                        && std::time::Instant::now() >= deadline
                    {
                        remove_pending(process, &request_id);
                        // 只取消本次查询，不取消同会话其他工具，也不停止常驻进程。
                        let _ = self.cancel_request(process, &request_id);
                        return Err(SidecarInvokeError::Timeout)
                            .with_context(|| format!("stdio sidecar 请求超时: {operation}"));
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // 读线程已退出（进程死亡）：按不可用处理，下次 ensure 重启。
                    remove_pending(process, &request_id);
                    return Err(SidecarInvokeError::Unavailable(format!(
                        "stdio sidecar 读通道已关闭; {}",
                        self.failure_detail(process)
                    ))
                    .into());
                }
            }
        }
    }

    /// 写请求帧（每个新进程首个请求前补 Auth 首帧，token 与子进程环境一致）。
    /// 管道写入受 `request_timeout` 保护，防止 sidecar 停止读取后宿主永久阻塞。
    fn write_request(&self, process: &Arc<StdioProcess>, request: &Request) -> Result<()> {
        write_with_protection(process, self.config.request_timeout, || {
            let mut stdin = process
                .stdin
                .lock()
                .map_err(|_| anyhow!("stdio sidecar 写端锁已损坏"))?;
            if !process.authenticated.load(Ordering::Acquire) {
                let frame = IpcFrame::Auth(IpcAuth {
                    token: process.token.clone(),
                });
                write_line(&mut stdin, &frame)?;
                process.authenticated.store(true, Ordering::Release);
            }
            let mut payload = serde_json::to_value(request).context("序列化 sidecar 请求失败")?;
            if let Some(context) = process.pending.lock().ok().and_then(|pending| {
                pending
                    .get(&request.request_id)
                    .and_then(|waiter| waiter.invocation_context.clone())
            }) && let Some(object) = payload.as_object_mut()
            {
                object.insert(
                    "context".to_string(),
                    serde_json::to_value(context).context("序列化 sidecar 调用上下文失败")?,
                );
            }
            let frame = IpcFrame::Request(IpcRequest {
                request_id: request.request_id.clone(),
                payload,
            });
            write_line(&mut stdin, &frame)
        })
    }

    fn write_frame(&self, process: &Arc<StdioProcess>, frame: &IpcFrame) -> Result<()> {
        write_with_protection(process, self.config.request_timeout, || {
            let mut stdin = process
                .stdin
                .lock()
                .map_err(|_| anyhow!("stdio sidecar 写端锁已损坏"))?;
            write_line(&mut stdin, frame)
        })
    }

    fn cancel_request(&self, process: &Arc<StdioProcess>, request_id: &str) -> Result<()> {
        self.write_frame(
            process,
            &IpcFrame::Cancel {
                request_id: request_id.to_string(),
            },
        )?;
        Ok(())
    }
}

fn child_status(process: &StdioProcess) -> String {
    match process.child.try_lock() {
        Ok(mut child) => match child.try_wait() {
            Ok(Some(status)) => format!("子进程退出状态: {status}"),
            Ok(None) => "子进程仍在运行但输出已关闭".to_string(),
            Err(error) => format!("读取子进程状态失败: {error}"),
        },
        Err(std::sync::TryLockError::WouldBlock) => "子进程仍在运行（状态查询忙）".to_string(),
        Err(std::sync::TryLockError::Poisoned(_)) => "无法读取子进程状态".to_string(),
    }
}

impl StdioSidecarConnection {
    /// 进程异常断开时的诊断信息：退出状态 + sidecar 日志尾部。
    ///
    /// 子进程 stderr 全量重定向到日志文件——Launcher 拒绝启动（如嵌套
    /// 沙箱不可用，退出码 78）、解释器启动失败等真实原因都在其中，
    /// 不带日志尾部时错误只剩"已关闭; exit status: 78"无法归因。
    fn failure_detail(&self, process: &StdioProcess) -> String {
        let mut detail = child_status(process);
        if let Some(tail) = sidecar_log_tail(&self.config.log) {
            detail.push_str("; 日志尾部: ");
            detail.push_str(&tail);
        }
        detail
    }
}

/// 提取 sidecar 日志的最后几行非空内容（截断单行长度），用于错误归因。
fn sidecar_log_tail(log: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL_BYTES: u64 = 4096;
    let mut file = std::fs::File::open(log).ok()?;
    let size = file.metadata().ok()?.len();
    let start = size.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut raw = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    // 从截断点后半段开始取整行，避免输出半个 JSON。
    let text = text.strip_prefix('\n').unwrap_or(&text);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail: Vec<String> = lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(|line| {
            let line = line.trim();
            line.chars().take(400).collect::<String>()
        })
        .collect();
    (!tail.is_empty()).then(|| tail.join(" | "))
}

/// 带看门狗的管道写入：写入超过 `timeout` 仍未完成时终止进程树，让
/// 阻塞在管道上的写立即以错误返回，防止 stdin 写入永久卡死。
fn write_with_protection<F>(process: &Arc<StdioProcess>, timeout: Duration, write: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let finished = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let watchdog_finished = Arc::clone(&finished);
    let watchdog_process = Arc::clone(process);
    let watchdog = std::thread::Builder::new()
        .name("stdio-sidecar-write-watchdog".to_string())
        .spawn(move || {
            let deadline = std::time::Instant::now() + timeout;
            let (state, notified) = &*watchdog_finished;
            let Ok(mut done) = state.lock() else {
                return;
            };
            while !*done {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (updated, _) = notified
                    .wait_timeout(done, remaining)
                    .expect("写入看门狗状态锁已损坏");
                done = updated;
            }
            if !*done && let Ok(mut child) = watchdog_process.child.lock() {
                tracing::warn!("stdio sidecar 管道写入超时，终止进程解除阻塞");
                // 看门狗尽力解除写阻塞：终止失败时写入方仍可能阻塞至
                // 上层取消，无法在此处进一步兜底。
                let _ = terminate_process_tree(&watchdog_process, &mut child);
            }
        });
    let result = write();
    if let Ok(mut done) = finished.0.lock() {
        *done = true;
        finished.1.notify_all();
    }
    if let Ok(watchdog) = watchdog {
        let _ = watchdog.join();
    }
    result
}

fn write_line(stdin: &mut ChildStdin, frame: &IpcFrame) -> Result<()> {
    let line = serde_json::to_string(frame).context("序列化 sidecar 帧失败")?;
    stdin
        .write_all(line.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .context("写入 stdio sidecar 帧失败")
}

fn remove_pending(process: &StdioProcess, request_id: &str) {
    if let Ok(mut pending) = process.pending.try_lock() {
        pending.remove(request_id);
    }
}

/// 常驻读线程：解析 stdout 的 JSON Lines 帧，按 request_id 路由响应与进度，
/// Notification 帧经全局转发器送出（与 TCP 通知监听等价）。
fn spawn_stdio_reader(
    plugin_id: String,
    stdout: std::process::ChildStdout,
    pending: Arc<Mutex<HashMap<String, PendingWaiter>>>,
    child: Arc<Mutex<Child>>,
    closed: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name(format!("plugin-sidecar-stdio-reader-{plugin_id}"))
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // EOF / IO 错误：进程关闭。
                    Ok(_) => {}
                }
                let Ok(frame) = serde_json::from_str::<IpcFrame>(line.trim_end()) else {
                    tracing::warn!(plugin_id = %plugin_id, "stdio sidecar 输出无法解析的帧");
                    continue;
                };
                match frame {
                    IpcFrame::Response(response) => {
                        let waiter = pending
                            .lock()
                            .ok()
                            .and_then(|mut map| map.remove(&response.request_id));
                        if let Some(waiter) = waiter {
                            let parsed = parse_response_payload(response.payload);
                            let _ = waiter.response.send(parsed);
                        }
                    }
                    IpcFrame::Progress {
                        request_id,
                        message,
                    } => {
                        let waiter = pending
                            .lock()
                            .ok()
                            .and_then(|map| map.get(&request_id).cloned());
                        if let Some(waiter) = waiter {
                            let _ = waiter.progress.try_send(message);
                        }
                    }
                    IpcFrame::Notification { channel, payload } => {
                        if channel == crate::protocol::TOOLS_RECOVERED_CHANNEL {
                            crate::registry::dispatch_tools_recovered(&plugin_id, &payload);
                        }
                        if let Some(forwarder) = crate::sidecar::sidecar_notification_forwarder() {
                            forwarder(&plugin_id, &channel, &payload);
                        }
                    }
                    IpcFrame::Error { message } => {
                        fail_all_pending(&pending, format!("stdio sidecar 错误: {message}"));
                    }
                    IpcFrame::Auth(_) | IpcFrame::Request(_) | IpcFrame::Cancel { .. } => {
                        tracing::warn!(
                            plugin_id = %plugin_id,
                            "stdio sidecar 发送了非预期的帧类型"
                        );
                    }
                }
            }
            closed.store(true, Ordering::Release);
            fail_all_pending(&pending, "stdio sidecar 已关闭".to_string());
            // 自行退出的常驻 sidecar（如最后一个 PTY 会话关闭后的
            // terminal）必须由宿主回收。轮询时不长期持有 child 锁，保证
            // stop/cancel 仍能并发终止关闭 stdout 后未退出的异常进程。
            loop {
                let exited = child
                    .lock()
                    .map(|mut child| match child.try_wait() {
                        Ok(Some(_)) | Err(_) => true,
                        Ok(None) => false,
                    })
                    .unwrap_or(true);
                if exited {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .expect("启动 stdio 读线程失败");
}

fn fail_all_pending(pending: &Mutex<HashMap<String, PendingWaiter>>, message: String) {
    if let Ok(mut map) = pending.lock() {
        for (_, waiter) in map.drain() {
            let _ = waiter.response.send(Err(message.clone()));
        }
    }
}

/// 解析 Response 信封：协议版本校验 + success 展开（对齐 TCP 实现）。
fn parse_response_payload(payload: Value) -> Result<Value, String> {
    let response: Response = serde_json::from_value(payload)
        .map_err(|error| format!("解析 sidecar 协议响应失败: {error}"))?;
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "sidecar 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
            response.protocol_version
        ));
    }
    if !response.success {
        return Err(response
            .error_message
            .unwrap_or_else(|| "sidecar 请求失败".to_string()));
    }
    Ok(response.payload.unwrap_or(Value::Null))
}

impl SidecarConnection for StdioSidecarConnection {
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn invoke(&self, operation: &str, payload: &str) -> Result<String> {
        self.invoke_with_progress(operation, payload, &mut |_| {})
    }

    fn invoke_with_progress(
        &self,
        operation: &str,
        payload: &str,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        let payload = serde_json::from_str(payload).with_context(|| "sidecar 请求不是有效 JSON")?;
        let response = match self.config.lifecycle {
            crate::manifest::SidecarLifecycle::OnDemand => self
                .invoke_on_demand(operation, payload, None, None, on_progress)
                .map_err(|error| {
                    if error.downcast_ref::<SidecarInvokeError>().is_some() {
                        error
                    } else {
                        SidecarInvokeError::Unavailable(error.to_string()).into()
                    }
                })?,
            crate::manifest::SidecarLifecycle::Resident => {
                let process = {
                    let mut state = self.state.lock().map_err(|_| {
                        anyhow!(SidecarInvokeError::Unavailable(
                            "stdio sidecar 状态锁已损坏".to_string()
                        ))
                    })?;
                    self.ensure_running(&mut state).map_err(|error| {
                        if error.downcast_ref::<SidecarInvokeError>().is_some() {
                            error
                        } else {
                            SidecarInvokeError::Unavailable(error.to_string()).into()
                        }
                    })?
                };
                self.round_trip(
                    &process,
                    operation,
                    payload,
                    None,
                    None,
                    ResponseWait::UntilCancelled,
                    on_progress,
                )
                .map_err(|error| {
                    if error.downcast_ref::<SidecarInvokeError>().is_some() {
                        error
                    } else {
                        SidecarInvokeError::Internal(error.to_string()).into()
                    }
                })?
            }
        };
        serde_json::to_string(&response).with_context(|| "序列化 sidecar 响应失败")
    }

    fn invoke_with_context(
        &self,
        operation: &str,
        payload: &str,
        context: &crate::sidecar::SidecarInvocationContext,
    ) -> Result<String> {
        self.invoke_with_context_and_progress(operation, payload, context, &mut |_| {})
    }

    fn invoke_with_context_and_progress(
        &self,
        operation: &str,
        payload: &str,
        context: &crate::sidecar::SidecarInvocationContext,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        let payload = serde_json::from_str(payload).with_context(|| "sidecar 请求不是有效 JSON")?;
        let response = match self.config.lifecycle {
            crate::manifest::SidecarLifecycle::OnDemand => {
                self.invoke_on_demand(operation, payload, Some(context.clone()), None, on_progress)?
            }
            crate::manifest::SidecarLifecycle::Resident => {
                let process = {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| anyhow!("stdio sidecar 状态锁已损坏"))?;
                    self.ensure_running(&mut state)?
                };
                self.round_trip(
                    &process,
                    operation,
                    payload,
                    Some(context.clone()),
                    None,
                    ResponseWait::UntilCancelled,
                    on_progress,
                )?
            }
        };
        serde_json::to_string(&response).with_context(|| "序列化 sidecar 响应失败")
    }

    fn invoke_with_invocation_context_and_progress(
        &self,
        operation: &str,
        payload: &str,
        context: &crate::protocol::RequestInvocationContext,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        let payload = serde_json::from_str(payload).with_context(|| "sidecar 请求不是有效 JSON")?;
        let response = match self.config.lifecycle {
            crate::manifest::SidecarLifecycle::OnDemand => {
                self.invoke_on_demand(operation, payload, None, Some(context.clone()), on_progress)?
            }
            crate::manifest::SidecarLifecycle::Resident => {
                let process = {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| anyhow!("stdio sidecar 状态锁已损坏"))?;
                    self.ensure_running(&mut state)?
                };
                self.round_trip(
                    &process,
                    operation,
                    payload,
                    None,
                    Some(context.clone()),
                    ResponseWait::UntilCancelled,
                    on_progress,
                )?
            }
        };
        serde_json::to_string(&response).with_context(|| "序列化 sidecar 响应失败")
    }

    fn handles_tool(&self, tool_name: &str) -> Result<bool> {
        // 诊断通道：消费进程就绪握手缓存的能力声明，不额外发起往返，
        // 也不在工具调用热路径启动 sidecar。
        let capabilities = self.ensure_running_checked()?;
        Ok(capabilities
            .iter()
            .any(|capability| capability == &format!("tool:{tool_name}") || capability == "tool:*"))
    }

    fn verify_capabilities(&self) -> Result<Vec<String>> {
        self.ensure_running_checked()
    }

    fn update_exec_env(&self, env: std::collections::BTreeMap<String, String>) {
        StdioSidecarConnection::update_exec_env(self, env);
    }

    fn stop(&self) -> Result<()> {
        StdioSidecarConnection::stop(self)
    }

    fn cancel_session(&self, session_id: &str) -> Result<()> {
        StdioSidecarConnection::cancel_session(self, session_id)
    }

    fn cancel_current(&self) {
        StdioSidecarConnection::cancel_current(self)
    }

    fn ensure_running(&self) -> Result<()> {
        StdioSidecarConnection::ensure_running_checked(self).map(|_| ())
    }

    fn has_runtime_endpoint(&self) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|state| {
                state.process.as_ref().map(|process| {
                    process
                        .child
                        .lock()
                        .map(|mut child| {
                            child
                                .try_wait()
                                .map(|status| status.is_none())
                                .unwrap_or(false)
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    fn plugin_id(&self) -> &str {
        StdioSidecarConnection::plugin_id(self)
    }
}

fn sandbox_workspace_for_spawn(
    config: &SidecarConfig,
    invocation_workspace: Option<&Path>,
) -> PathBuf {
    invocation_workspace
        .map(PathBuf::from)
        .or_else(|| config.sandbox_workspace.clone())
        .unwrap_or_else(|| config.storage_root.clone())
}

fn validate_invocation_workspace(workspace: &Path) -> Result<PathBuf> {
    if !workspace.is_absolute() {
        bail!(
            "本次 sidecar 调用的工作区必须是绝对路径: {}",
            workspace.display()
        );
    }
    let workspace = std::fs::canonicalize(workspace)
        .with_context(|| format!("解析本次 sidecar 调用的工作区失败: {}", workspace.display()))?;
    if !workspace.is_dir() {
        bail!("本次 sidecar 调用的工作区不是目录: {}", workspace.display());
    }
    Ok(workspace)
}

impl Drop for StdioSidecarConnection {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
