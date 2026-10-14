//! Store 内部的宿主状态。
//!
//! 承载：
//! - WASI 上下文与资源表；
//! - 内存/表/实例上限；
//! - clock host import；
//! - sidecar host import（通用 JSON 转发，不理解业务）。
//!
//! 通用运行时不依赖任何具体插件。SidecarConnection 由入口侧注入。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use wasmtime::StoreLimits;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::tiangong::plugin::clock::Host as ClockHost;
use crate::bindings::tiangong::plugin::feedback::Host as FeedbackHost;
use crate::bindings::tiangong::plugin::sidecar::{Host as SidecarHost, SidecarError};
use crate::sidecar::{SidecarConnection, SidecarInvocationContext, SidecarInvokeError};
use tiangong_core::core::plugin::PluginFeedbackTx;
use tiangong_types::StreamEvent;

/// WASM Store 的宿主侧状态。
pub struct HostState {
    limits: StoreLimits,
    wasi: WasiCtx,
    table: ResourceTable,
    /// sidecar 连接（通用，由入口侧注入）。None 时 invoke 返回 unavailable。
    sidecar: Option<Arc<dyn SidecarConnection>>,
    /// 当前 turn 的插件反馈通道。
    feedback: Option<PluginFeedbackTx>,
    /// 当前同步工具调用的宿主权威上下文；调用返回后立即清空。
    invocation_context: Option<crate::protocol::RequestInvocationContext>,
    /// Runtime 统一调用生命周期；仅 Runtime Adapter 的新路径注入。
    runtime_invocation: Option<crate::invocation::RuntimeInvocation>,
    /// 插件 ID（用于权限隔离和 Runtime 控制反馈归属）。
    plugin_id: String,
}

impl HostState {
    pub fn new(
        limits: StoreLimits,
        sidecar: Option<Arc<dyn SidecarConnection>>,
        plugin_id: String,
        storage_access: bool,
    ) -> Self {
        let wasi = build_wasi_ctx(&plugin_id, storage_access);
        Self {
            limits,
            wasi,
            table: ResourceTable::new(),
            sidecar,
            feedback: None,
            invocation_context: None,
            runtime_invocation: None,
            plugin_id,
        }
    }

    /// 提供对内部限制器的可变借用，供 `Store::limiter` 闭包返回。
    pub fn limits_mut(&mut self) -> &mut StoreLimits {
        &mut self.limits
    }

    pub fn set_feedback(&mut self, feedback: PluginFeedbackTx) {
        self.feedback = Some(feedback);
    }

    pub fn set_invocation_context(&mut self, context: Option<SidecarInvocationContext>) {
        self.invocation_context =
            context.map(|context| crate::protocol::RequestInvocationContext {
                session_id: context.session_id,
                invocation_id: context.invocation_id,
                workspace: context
                    .authoritative_workspace
                    .to_string_lossy()
                    .into_owned(),
                actor_id: String::new(),
                deadline_ms: None,
            });
    }

    pub fn set_full_invocation_context(
        &mut self,
        context: Option<crate::protocol::RequestInvocationContext>,
    ) {
        self.invocation_context = context;
    }

    pub(crate) fn set_runtime_invocation(
        &mut self,
        invocation: Option<crate::invocation::RuntimeInvocation>,
    ) {
        self.runtime_invocation = invocation;
    }
}

/// 让 wasmtime-wasi 经由该状态访问 WASI 上下文与资源表。
impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// clock host import。
impl ClockHost for HostState {
    fn now_millis(&mut self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

impl FeedbackHost for HostState {
    fn emit_stream_event(&mut self, event_json: String) {
        let Some(feedback) = &self.feedback else {
            return;
        };
        match serde_json::from_str::<StreamEvent>(&event_json) {
            Ok(event) => feedback.send_stream_event(event),
            Err(error) => tracing::warn!(%error, "wasm 插件反馈事件解析失败"),
        }
    }
}

/// sidecar host import：通用 JSON 转发。
///
/// WASM 组件经 `sidecar.invoke(operation, payload)` 调用，Host 负责通用协议封装，
/// 不理解操作名与负载的业务含义。
impl SidecarHost for HostState {
    fn invoke(&mut self, operation: String, payload: String) -> Result<String, SidecarError> {
        // 加载时注入的连接被停止（server 端点变化等触发依赖插件重启）后，
        // 静态引用永不恢复：检测到已停止就经注册表取换代后的现役连接
        //（新进程携带新注入的环境）。
        self.sidecar =
            crate::registry::refresh_stale_sidecar(self.sidecar.as_ref(), &self.plugin_id);
        let Some(conn) = self.sidecar.clone() else {
            return Err(SidecarError::NotConfigured);
        };
        // 在独立 OS 线程上执行，避免 tokio worker 线程嵌套。
        let feedback = self.feedback.clone();
        let invocation_context = self.invocation_context.clone();
        let runtime_invocation = self.runtime_invocation.clone();
        let plugin_id = self.plugin_id.clone();
        std::thread::scope(|s| {
            s.spawn(move || {
                let mut on_progress = |event_json: String| {
                    if let Some(invocation) = &runtime_invocation {
                        invocation.progress(event_json);
                        return;
                    }
                    if crate::bridge::handle_runtime_feedback(&plugin_id, &event_json) {
                        return;
                    }
                    let Some(feedback) = &feedback else {
                        return;
                    };
                    match serde_json::from_str::<StreamEvent>(&event_json) {
                        Ok(event) => feedback.send_stream_event(event),
                        Err(error) => tracing::warn!(%error, "sidecar 进度事件解析失败"),
                    }
                };
                match invocation_context {
                    Some(context) => conn.invoke_with_invocation_context_and_progress(
                        &operation,
                        &payload,
                        &context,
                        &mut on_progress,
                    ),
                    None => conn.invoke_with_progress(&operation, &payload, &mut on_progress),
                }
            })
            .join()
            .map_err(|e| SidecarError::Internal(format!("sidecar 线程异常: {e:?}")))?
            .map_err(map_sidecar_error)
        })
    }
}

fn map_sidecar_error(error: anyhow::Error) -> SidecarError {
    match error.downcast_ref::<SidecarInvokeError>() {
        Some(SidecarInvokeError::Unavailable(message)) => {
            SidecarError::Unavailable(message.clone())
        }
        Some(SidecarInvokeError::Timeout) => SidecarError::Timeout,
        Some(SidecarInvokeError::PermissionDenied) => SidecarError::PermissionDenied,
        Some(SidecarInvokeError::ProtocolMismatch(message)) => {
            SidecarError::ProtocolMismatch(message.clone())
        }
        Some(SidecarInvokeError::Internal(message)) => SidecarError::Internal(message.clone()),
        None => SidecarError::Internal(error.to_string()),
    }
}

/// 构建 WASI 上下文，preopen 插件配置目录供 WASM 组件读写自己的配置。
///
/// - 所有插件额外 preopen 自有数据目录 `<存储根>/data/<plugin_id>`（`/data`）。
/// - `storage_access` 为 true 时额外 preopen 存储根（`/storage`），仅供迁移旧数据
///   等历史兼容场景使用；新数据应写入 `/data`。
///
/// 存储根遵循 `TIANGONG_STORAGE_ROOT`（与 `tiangong_config::io::storage_root`
/// 一致），测试与多实例可借此隔离，不触碰真实用户目录。
fn build_wasi_ctx(plugin_id: &str, storage_access: bool) -> WasiCtx {
    let mut builder = WasiCtxBuilder::new();
    let storage_root = tiangong_config::io::storage_root();
    let dir = storage_root.join("plugins").join(plugin_id);
    let _ = std::fs::create_dir_all(&dir);
    if let Err(e) = builder.preopened_dir(&dir, ".", wasmtime_wasi::FsPerms::ReadWrite) {
        tracing::debug!("preopen 插件配置目录失败（{e}），配置读写将不可用");
    }
    match ensure_plugin_data_dir(&storage_root, plugin_id) {
        Ok(data_dir) => {
            if let Err(e) =
                builder.preopened_dir(&data_dir, "/data", wasmtime_wasi::FsPerms::ReadWrite)
            {
                tracing::debug!("preopen 插件数据目录失败（{e}），数据目录读写将不可用");
            }
        }
        Err(e) => tracing::debug!("创建插件数据目录失败（{e}），数据目录读写将不可用"),
    }
    if storage_access {
        let _ = std::fs::create_dir_all(&storage_root);
        if let Err(e) =
            builder.preopened_dir(&storage_root, "/storage", wasmtime_wasi::FsPerms::ReadWrite)
        {
            tracing::debug!("preopen 存储根目录失败（{e}），存储根访问将不可用");
        }
    }
    builder.build()
}

/// 插件自有数据目录 `<存储根>/data/<plugin_id>`：已存在时必须是实体目录
/// （不跟随符号链接），不存在时创建。插件 ID 只允许单个普通路径段。
fn ensure_plugin_data_dir(
    storage_root: &std::path::Path,
    plugin_id: &str,
) -> std::io::Result<std::path::PathBuf> {
    use std::path::{Component, Path};
    let mut components = Path::new(plugin_id).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("插件 ID 不能作为数据目录名: {plugin_id}"),
        ));
    }
    let base = storage_root.join("data");
    let dir = base.join(plugin_id);
    for path in [&base, &dir] {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => {
                return Err(std::io::Error::other(format!(
                    "插件数据目录不是实体目录: {}",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir_all(path) {
                    Ok(()) => {}
                    Err(error)
                        if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(dir)
}
