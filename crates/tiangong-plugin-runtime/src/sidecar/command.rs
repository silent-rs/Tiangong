//! command 专用的一次性 sidecar 连接。
//!
//! 每个执行请求都使用宿主调用上下文中的工作区创建独立 Launcher、临时目录
//! 和进程树。`command.set_workspace` 仅作为 WASM 兼容通知接收，不启动进程。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::launch_policy::canonicalize_path;
use anyhow::{Context, Result, anyhow, bail};

use super::{
    SidecarConfig, SidecarConnection, SidecarInvocationContext, SidecarInvokeError,
    StdioSidecarConnection,
};

const SET_WORKSPACE_OPERATION: &str = "command.set_workspace";
const RUN_COMMAND_OPERATION: &str = "command.run_command";
const RUN_SHELL_OPERATION: &str = "command.run_shell";

struct ActiveInvocation {
    session_id: String,
    connection: Arc<StdioSidecarConnection>,
    cancelled: Arc<AtomicBool>,
}

/// command 工具的宿主执行封套。该结构可被多个会话共享，但不保存工作区；
/// 工作区必须随每次 [`SidecarInvocationContext`] 传入。
pub struct EphemeralCommandConnection {
    template: SidecarConfig,
    exec_env: Mutex<BTreeMap<String, String>>,
    active: Mutex<HashMap<String, ActiveInvocation>>,
    program_sha256: OnceLock<Result<String, String>>,
}

impl EphemeralCommandConnection {
    /// `template.sandbox` 由唯一生产构造点（registry 策略表，含用户
    /// "命令沙箱"开关）显式决定，封套不再暗改——测试构造时需显式设置。
    pub fn new(template: SidecarConfig) -> Self {
        Self {
            template,
            exec_env: Mutex::new(BTreeMap::new()),
            active: Mutex::new(HashMap::new()),
            program_sha256: OnceLock::new(),
        }
    }

    fn invoke_ephemeral(
        &self,
        operation: &str,
        payload: &str,
        context: &SidecarInvocationContext,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        let started = Instant::now();
        if self.template.plugin_id != "command" {
            bail!("一次性 command 连接的插件身份无效");
        }
        let workspace = canonicalize_path(&context.authoritative_workspace).with_context(|| {
            format!(
                "本次工具调用的工作区无效: {}",
                context.authoritative_workspace.display()
            )
        })?;
        if !workspace.is_dir() {
            bail!("本次工具调用的工作区不是目录: {}", workspace.display());
        }

        let mut request: serde_json::Value =
            serde_json::from_str(payload).context("command 请求不是有效 JSON")?;
        let request_object = request
            .as_object_mut()
            .ok_or_else(|| anyhow!("command 请求必须是 JSON 对象"))?;
        // Windows AppContainer 无法展开宿主短路径别名，启动前由宿主统一 cwd。
        let requested_cwd = request_object
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from);
        if let Some(requested_cwd) = requested_cwd {
            let candidate = if requested_cwd.is_absolute() {
                requested_cwd
            } else {
                workspace.join(requested_cwd)
            };
            if let Ok(canonical_cwd) = canonicalize_path(&candidate) {
                request_object.insert(
                    "cwd".to_string(),
                    serde_json::Value::String(canonical_cwd.display().to_string()),
                );
            }
        }
        // CommandAccessContext 使用 serde(flatten)，最终只保留宿主权威工作区。
        request_object.insert(
            "workspace".to_string(),
            serde_json::Value::String(workspace.display().to_string()),
        );
        request_object.remove("full_trust");
        request_object.remove("allowed_commands");
        request_object.remove("access");

        let invocation_dir = tempfile::Builder::new()
            .prefix("tiangong-command-")
            .tempdir()
            .context("创建 command 专用临时目录失败")?;
        let temp_dir = invocation_dir.path().join("tmp");
        std::fs::create_dir_all(&temp_dir)
            .with_context(|| format!("创建 command 临时目录失败: {}", temp_dir.display()))?;

        let mut config = self.template.clone();
        config.sandbox_program_sha256 = Some(
            self.program_sha256
                .get_or_init(|| {
                    super::sha256_file(&self.template.binary).map_err(|error| format!("{error:#}"))
                })
                .clone()
                .map_err(anyhow::Error::msg)?,
        );
        config.endpoint = temp_dir.join("endpoint.json");
        config.log = temp_dir.join("sidecar.log");
        config.data_dir = temp_dir.join("data");
        config.sandbox_workspace = Some(workspace.clone());
        config.sandbox_extra_writable.push(temp_dir.clone());
        config.sandbox_temp_dir = Some(temp_dir);
        if config.sandbox_program_root.is_none() {
            config.sandbox_program_root = config.binary.parent().map(PathBuf::from);
        }

        let sidecar_log = config.log.clone();
        emit_sandbox_diagnostic(&context.invocation_id, "prepared", started, &sidecar_log);
        let connection = Arc::new(StdioSidecarConnection::new(config));
        if let Ok(env) = self.exec_env.lock() {
            connection.update_exec_env(env.clone());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let key = format!("{}:{}", context.session_id, context.invocation_id);
        {
            let mut active = self
                .active
                .lock()
                .map_err(|_| anyhow!("command 活跃调用表已损坏"))?;
            if active.contains_key(&key) {
                bail!("重复的 command 调用标识: {}", context.invocation_id);
            }
            active.insert(
                key.clone(),
                ActiveInvocation {
                    session_id: context.session_id.clone(),
                    connection: Arc::clone(&connection),
                    cancelled: Arc::clone(&cancelled),
                },
            );
        }

        let result = (|| {
            if cancelled.load(Ordering::Acquire) {
                bail!("command 调用已取消");
            }
            let init = serde_json::json!({
                "workspace": workspace.display().to_string(),
            });
            let init_result = connection.invoke(SET_WORKSPACE_OPERATION, &init.to_string());
            emit_sandbox_diagnostic(
                &context.invocation_id,
                "workspace-ready",
                started,
                &sidecar_log,
            );
            init_result?;
            if cancelled.load(Ordering::Acquire) {
                bail!("command 调用已取消");
            }
            let invoke_result =
                connection.invoke_with_progress(operation, &request.to_string(), on_progress);
            emit_sandbox_diagnostic(
                &context.invocation_id,
                "command-finished",
                started,
                &sidecar_log,
            );
            invoke_result
        })();
        let _ = connection.stop();
        emit_sandbox_diagnostic(
            &context.invocation_id,
            "process-stopped",
            started,
            &sidecar_log,
        );
        if let Ok(mut active) = self.active.lock() {
            active.remove(&key);
        }
        let result = result.with_context(|| {
            format!(
                "command sidecar 日志（末尾）:\n{}",
                read_log_tail(&sidecar_log)
            )
        });
        drop(invocation_dir);

        let response = result?;
        annotate_sandbox_violation(response)
    }

    fn stop_matching(&self, session_id: Option<&str>) -> Result<()> {
        let connections = {
            let mut active = self
                .active
                .lock()
                .map_err(|_| anyhow!("command 活跃调用表已损坏"))?;
            let keys = active
                .iter()
                .filter(|(_, invocation)| session_id.is_none_or(|id| invocation.session_id == id))
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| active.remove(&key))
                .map(|item| {
                    item.cancelled.store(true, Ordering::Release);
                    item.connection
                })
                .collect::<Vec<_>>()
        };
        for connection in connections {
            connection.stop()?;
        }
        Ok(())
    }
}

fn emit_sandbox_diagnostic(
    invocation_id: &str,
    stage: &str,
    started: Instant,
    sidecar_log: &std::path::Path,
) {
    if std::env::var_os("TIANGONG_SANDBOX_DIAGNOSTICS").is_some() {
        eprintln!(
            "SANDBOX_DIAGNOSTIC invocation={invocation_id} stage={stage} elapsed_ms={} log={}",
            started.elapsed().as_millis(),
            sidecar_log.display()
        );
    }
}

impl SidecarConnection for EphemeralCommandConnection {
    fn invoke(&self, operation: &str, _payload: &str) -> Result<String> {
        if operation == SET_WORKSPACE_OPERATION {
            return Ok("{}".to_string());
        }
        if matches!(operation, RUN_COMMAND_OPERATION | RUN_SHELL_OPERATION) {
            return Err(SidecarInvokeError::PermissionDenied)
                .context("command 执行缺少宿主工具调用上下文");
        }
        bail!("不支持的 command 操作: {operation}")
    }

    fn invoke_with_context(
        &self,
        operation: &str,
        payload: &str,
        context: &SidecarInvocationContext,
    ) -> Result<String> {
        self.invoke_with_context_and_progress(operation, payload, context, &mut |_| {})
    }

    fn invoke_with_context_and_progress(
        &self,
        operation: &str,
        payload: &str,
        context: &SidecarInvocationContext,
        on_progress: &mut dyn FnMut(String),
    ) -> Result<String> {
        if operation == SET_WORKSPACE_OPERATION {
            return Ok("{}".to_string());
        }
        if !matches!(operation, RUN_COMMAND_OPERATION | RUN_SHELL_OPERATION) {
            bail!("不支持的 command 操作: {operation}");
        }
        self.invoke_ephemeral(operation, payload, context, on_progress)
    }

    fn update_exec_env(&self, env: BTreeMap<String, String>) {
        if let Ok(mut current) = self.exec_env.lock() {
            *current = env;
        }
    }

    fn stop(&self) -> Result<()> {
        self.stop_matching(None)
    }

    fn cancel_session(&self, session_id: &str) -> Result<()> {
        self.stop_matching(Some(session_id))
    }

    fn cancel_current(&self) {
        // 沙箱开关切换或工具级取消：停止所有当前临时 command 进程，
        // 不使连接永久失效；后续调用按最新 runtime 沙箱状态重建。
        let _ = self.stop_matching(None);
    }

    fn ensure_running(&self) -> Result<()> {
        // command 没有常驻进程；真正执行时再完成 Launcher 握手。
        Ok(())
    }

    fn plugin_id(&self) -> &str {
        &self.template.plugin_id
    }
}

fn read_log_tail(path: &std::path::Path) -> String {
    const MAX_BYTES: usize = 8 * 1024;

    let Ok(bytes) = std::fs::read(path) else {
        return format!("<无法读取 {}>", path.display());
    };
    let start = bytes.len().saturating_sub(MAX_BYTES);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn annotate_sandbox_violation(response: String) -> Result<String> {
    let mut value: serde_json::Value =
        serde_json::from_str(&response).context("解析 command 响应失败")?;
    if value.get("ok").and_then(serde_json::Value::as_bool) == Some(false)
        && let Some(hint) = value
            .get("stderr")
            .and_then(serde_json::Value::as_str)
            .and_then(crate::launch_policy::explain_violation)
    {
        let stderr = value
            .get("stderr")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "stderr".to_string(),
                serde_json::Value::String(format!("{stderr}\n[沙箱提示] {hint}")),
            );
        }
    }
    serde_json::to_string(&value).context("序列化 command 响应失败")
}
