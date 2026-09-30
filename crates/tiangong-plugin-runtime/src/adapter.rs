//! WASM 插件 → 进程内 `Plugin` trait 适配器。
//!
//! [`WasmPluginAdapter`] 包装一个已实例化的 [`WasmPlugin`]，使其能被宿主当作
//! 原生插件注册。Wasmtime 的调用是同步阻塞的；适配器统一把处于 Tokio
//! runtime 内的调用转移到普通 OS 线程，避免 wasmtime-wasi 嵌套 `block_on`。
//!
//! 当前已转发插件描述、工具规格、工具调用、Prompt 段落和生命周期钩子。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::sidecar::SidecarConnection;
use tiangong_core::config::core::CoreConfig;
use tiangong_core::core::Plugin;
use tiangong_core::core::plugin::PluginFeedbackTx;
use tiangong_core::permission::TrustMode;
use tiangong_core::session::Session;
use tiangong_core::tools::extension::{
    PromptSectionProvider, ToolOverrideHandler, ToolSpecProvider,
};
use tiangong_core::tools::result::{ToolExecutionRecord, ToolResult};
use tiangong_llm::tool::{ToolCall, ToolSpec};
use tokio::task;

use crate::config::PluginRuntimeConfig;
use crate::loader::WasmPlugin;

/// 把一个 WASM 插件实例包装为进程内 `Plugin`。
///
/// 注意：Wasmtime 的 Store 要求 `Send`（Engine 编译产物可跨线程），故
/// `Arc<Mutex<WasmPlugin>>` 可用于 `spawn_blocking`。
pub struct WasmPluginAdapter {
    inner: RwLock<Option<Arc<Mutex<WasmPlugin>>>>,
    config: PluginRuntimeConfig,
    /// 插件 id，构造期确定后不变。
    id: String,
    /// 反馈通道（每 turn 注入），供 handle 发送流事件。
    feedback_tx: RwLock<Option<PluginFeedbackTx>>,
    context: Mutex<ReloadContext>,
    /// 串行化配置与运行上下文通知，避免旧通知晚完成覆盖新状态。
    context_updates: Mutex<()>,
    enabled: AtomicBool,
    sidecar: Option<Arc<dyn SidecarConnection>>,
    /// 声明冻结快照：首次成功读取后，同一次运行内 tools/prompt 固定
    /// 返回冻结值——插件内部状态波动（探测重写、健康翻转）不得改写
    /// 请求前缀（KV cache 稳定），也免去每轮 WASM 声明调用。换代
    ///（replace_inner/release_inner，升级/卸载）或配置变化后重新冻结。
    cached_tools: Mutex<Option<Vec<ToolSpec>>>,
    cached_prompt_sections: Mutex<Option<Vec<String>>>,
}

#[derive(Clone, Default)]
struct ReloadContext {
    workspace: Option<String>,
    config_json: Option<String>,
    session_json: Option<String>,
    trust_mode: Option<TrustMode>,
    exec_env: std::collections::BTreeMap<String, String>,
    config_applied: bool,
    workspace_applied: bool,
}

impl WasmPluginAdapter {
    pub fn new(plugin: WasmPlugin, config: PluginRuntimeConfig) -> Self {
        Self::new_with_enabled(plugin, config, true, None)
    }

    pub(crate) fn new_with_enabled(
        plugin: WasmPlugin,
        config: PluginRuntimeConfig,
        enabled: bool,
        sidecar: Option<Arc<dyn SidecarConnection>>,
    ) -> Self {
        let inner = Arc::new(Mutex::new(plugin));
        let id_string = call_wasm_off_runtime(inner.clone(), |plugin| plugin.describe())
            .map(|d| d.id)
            .unwrap_or_else(|_| "wasm-unknown".to_string());
        Self {
            inner: RwLock::new(Some(inner)),
            id: id_string,
            config,
            feedback_tx: RwLock::new(None),
            context: Mutex::new(ReloadContext::default()),
            context_updates: Mutex::new(()),
            enabled: AtomicBool::new(enabled),
            sidecar,
            cached_tools: Mutex::new(None),
            cached_prompt_sections: Mutex::new(None),
        }
    }

    /// 使用预加载阶段已校验的插件 ID 构造实例，避免每个 Core 重复调用 describe。
    pub(crate) fn new_with_id(
        plugin: WasmPlugin,
        config: PluginRuntimeConfig,
        enabled: bool,
        id: String,
        sidecar: Option<Arc<dyn SidecarConnection>>,
    ) -> Self {
        Self {
            inner: RwLock::new(Some(Arc::new(Mutex::new(plugin)))),
            id,
            config,
            feedback_tx: RwLock::new(None),
            context: Mutex::new(ReloadContext::default()),
            context_updates: Mutex::new(()),
            enabled: AtomicBool::new(enabled),
            sidecar,
            cached_tools: Mutex::new(None),
            cached_prompt_sections: Mutex::new(None),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub(crate) fn notify_tools_recovered(&self, names: &[String]) {
        if !self.is_enabled() {
            return;
        }
        if names.is_empty() {
            return;
        }
        if let Some(feedback) = self.feedback_tx.read().ok().and_then(|value| value.clone()) {
            feedback.inject_tool(
                "plugin_availability",
                serde_json::json!({"plugin_id":self.id,"available":true,"tools":names}),
            );
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn runtime_config(&self) -> PluginRuntimeConfig {
        self.config.clone()
    }

    pub(crate) fn prepare_replacement(
        &self,
        mut plugin: WasmPlugin,
        activate: bool,
    ) -> anyhow::Result<Arc<Mutex<WasmPlugin>>> {
        if !activate {
            return Ok(Arc::new(Mutex::new(plugin)));
        }
        let context = self
            .context
            .lock()
            .map_err(|_| anyhow::anyhow!("wasm 插件上下文锁已损坏"))?
            .clone();
        if let Ok(feedback) = self.feedback_tx.read()
            && let Some(feedback) = feedback.clone()
        {
            plugin.set_feedback(feedback);
        }
        if let Some(config_json) = context.config_json {
            plugin.on_config_updated(config_json)?;
        }
        plugin.set_workspace(context.workspace, self.is_full_trust())?;
        if let Some(session_json) = context.session_json {
            plugin.on_session_ready(session_json)?;
        }
        Ok(Arc::new(Mutex::new(plugin)))
    }

    pub(crate) fn replace_inner(&self, replacement: Arc<Mutex<WasmPlugin>>) {
        let _update = self
            .context_updates
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut current = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *current = Some(replacement);
        drop(current);
        self.reset_applied_context();
        // 换代即新版本：声明缓存作废，避免旧版本声明兜住新版本实例。
        self.clear_declaration_cache();
    }

    /// 卸载内部 WASM 实例（drop Store），释放其对插件目录的 WASI preopen 句柄。
    ///
    /// Windows 上 cap-std 打开目录不带 FILE_SHARE_DELETE，只要 Store 还持有该目录，
    /// 安装目录就无法 rename/delete，升级与卸载会失败。在改写目录前调用本方法释放句柄，
    /// 后续 reload 会重建实例。
    pub(crate) fn release_inner(&self) {
        let _update = self
            .context_updates
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut current = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *current = None;
        // 声明读取会先锁缓存再读取 inner，不能持有 inner 写锁清缓存。
        drop(current);
        self.reset_applied_context();
        self.clear_declaration_cache();
    }

    fn clear_declaration_cache(&self) {
        let mut tools = self
            .cached_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *tools = None;
        let mut sections = self
            .cached_prompt_sections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *sections = None;
    }

    fn current_inner(&self) -> Option<Arc<Mutex<WasmPlugin>>> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn reset_applied_context(&self) {
        let mut context = self.context.lock().unwrap_or_else(|p| p.into_inner());
        context.config_applied = false;
        context.workspace_applied = false;
    }

    /// 调用方持有 context_updates。只有成功应用后才跳过重复通知。
    fn apply_workspace_context(&self) {
        let (workspace, full_trust) = {
            let context = self.context.lock().unwrap_or_else(|p| p.into_inner());
            if context.workspace_applied || !self.is_enabled() {
                return;
            }
            (
                context.workspace.clone(),
                context.trust_mode == Some(TrustMode::FullTrust),
            )
        };
        match self.call_wasm_off_runtime(move |plugin| plugin.set_workspace(workspace, full_trust))
        {
            Ok(()) => {
                self.context
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .workspace_applied = true
            }
            Err(error) => {
                tracing::warn!(plugin_id = %self.id, %error, "更新 wasm 工作目录与信任模式失败")
            }
        }
    }
}

impl Plugin for WasmPluginAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    /// 注入反馈通道（每 turn 注入），缓存供 handle 发送流事件。
    fn set_feedback_tx(&self, tx: PluginFeedbackTx) {
        if let Ok(mut guard) = self.feedback_tx.write() {
            *guard = Some(tx.clone());
        }
        if let Err(error) = self.call_wasm_off_runtime(move |plugin| {
            plugin.set_feedback(tx);
            Ok(())
        }) {
            tracing::warn!(%error, "注入 wasm 插件反馈通道失败");
        }
    }

    /// CoreConfig 变更：序列化为 JSON 转发到 WASM 组件的 on-config-updated。
    /// 序列化失败（不应发生）或 WASM 调用失败时仅记录 warning，不阻断 core 流程。
    fn on_config_updated(&self, config: &CoreConfig) {
        let _update = self
            .context_updates
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_json = match plugin_config_payload(config) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("序列化 CoreConfig 失败，跳过 wasm 插件通知: {e}");
                return;
            }
        };
        let changed = {
            let mut context = self
                .context
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let changed = context.config_json.as_ref() != Some(&config_json);
            if !changed && context.config_applied {
                return;
            }
            context.config_json = Some(config_json.clone());
            context.config_applied = false;
            changed
        };
        if self.is_enabled() {
            match self.call_wasm_off_runtime(move |plugin| plugin.on_config_updated(config_json)) {
                Ok(()) => {
                    self.context
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .config_applied = true
                }
                Err(error) => {
                    tracing::warn!(plugin_id = %self.id, %error, "通知 wasm 插件配置变更失败")
                }
            }
        }
        // 在配置通知结束后清理，避免并发读取把旧声明留到下一轮。
        // 失败也可能已部分更新插件状态，不能继续沿用旧配置的声明。
        if changed {
            self.clear_declaration_cache();
        }
    }

    /// 会话就绪：序列化 session 只读快照转发到 WASM。
    fn on_session_ready(&self, session: &mut Session) {
        self.forward_session_hook("on_session_ready", session, |plugin, json| {
            plugin.on_session_ready(json)
        });
    }

    /// 轮次开始：序列化 session 转发。
    fn on_turn_started(&self, session: &mut Session, turn_start_idx: usize) {
        self.forward_session_hook_with_idx(
            "on_turn_started",
            session,
            turn_start_idx,
            |plugin, json, idx| plugin.on_turn_started(json, idx),
        );
    }

    /// 轮次结束：序列化 session 只读快照转发（通知型钩子）。
    ///
    /// Core 已在后台通知线程中调用本方法（issue #404），此处同步转发即可；
    /// Memory 等插件在 WASM 内同步确认 sidecar 入队，耗时被隔离在通知线程。
    fn on_turn_finished(&self, session: &Session, turn_start_idx: usize) {
        self.forward_session_hook_with_idx(
            "on_turn_finished",
            session,
            turn_start_idx,
            |plugin, json, idx| plugin.on_turn_finished(json, idx),
        );
    }

    /// 会话结束：序列化 session 转发（WASM 内部触发 meso 反刍）。
    ///
    /// 通知型钩子：Core 已在后台通知线程中调用本方法（issue #404），此处同步
    /// 转发，不再自行 detached。
    fn on_session_ended(&self, session: &Session) {
        self.forward_session_hook("on_session_ended", session, |plugin, json| {
            plugin.on_session_ended(json)
        });
    }

    /// 注入工作目录：传给 WASM 缓存，供 prompt_sections 拉注入用。
    ///
    /// 同时携带当前信任模式（full_trust），供插件放宽/收紧工作区外路径校验。
    fn set_workspace(&self, workspace: Option<&std::path::Path>) {
        let _update = self
            .context_updates
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let ws = workspace.map(|p| p.to_string_lossy().to_string());
        if let Ok(mut context) = self.context.lock() {
            if context.workspace != ws {
                context.workspace_applied = false;
            }
            context.workspace = ws.clone();
        }
        self.apply_workspace_context();
    }

    fn set_trust_mode(&self, trust: TrustMode) {
        let _update = self
            .context_updates
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Ok(mut context) = self.context.lock() {
            if context.trust_mode != Some(trust) {
                context.workspace_applied = false;
            }
            context.trust_mode = Some(trust);
        }
        // 信任模式可能在 set_workspace 之后变更：重新调一次 set_workspace，
        // 把最新 trust 推送给 WASM（workspace 保持 context 中缓存的值不变）。
        self.apply_workspace_context();
    }

    fn set_execution_context(&self, workspace: Option<&std::path::Path>, trust: TrustMode) {
        let _update = self
            .context_updates
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let ws = workspace.map(|p| p.to_string_lossy().to_string());
        {
            let mut context = self.context.lock().unwrap_or_else(|p| p.into_inner());
            if context.workspace != ws || context.trust_mode != Some(trust) {
                context.workspace_applied = false;
            }
            context.workspace = ws;
            context.trust_mode = Some(trust);
        }
        self.apply_workspace_context();
    }

    fn set_exec_env(&self, env: std::collections::BTreeMap<String, String>) {
        if let Ok(mut context) = self.context.lock() {
            context.exec_env = env.clone();
        }
        // 转发给 sidecar（下次 spawn 时注入子进程环境）。
        if let Some(sidecar) = &self.sidecar {
            sidecar.update_exec_env(env);
        }
    }

    fn on_cancel<'a>(
        &'a self,
        session: &mut Session,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        // 引用可能已被换代（wasm 调用侧触发刷新）：取消要打到现役连接。
        if let Some(sidecar) =
            crate::registry::refresh_stale_sidecar(self.sidecar.as_ref(), &self.id)
            && let Err(error) = sidecar.cancel_session(&session.id)
        {
            tracing::warn!(plugin_id = %self.id, session_id = %session.id, %error, "取消 sidecar 调用失败");
        }
        Box::pin(async {})
    }
}

impl WasmPluginAdapter {
    /// 当前是否为完全信任模式（供 set_workspace 推送给 WASM）。
    fn is_full_trust(&self) -> bool {
        self.context.lock().ok().and_then(|c| c.trust_mode) == Some(TrustMode::FullTrust)
    }

    /// 在 Tokio runtime 外执行 WASM 调用，避免 wasmtime-wasi 嵌套 block_on。
    fn call_wasm_off_runtime<R>(
        &self,
        call: impl FnOnce(&mut WasmPlugin) -> anyhow::Result<R> + Send,
    ) -> anyhow::Result<R>
    where
        R: Send,
    {
        let inner = self
            .current_inner()
            .ok_or_else(|| anyhow::anyhow!("wasm 插件实例已卸载"))?;
        call_wasm_off_runtime(inner, call)
    }

    /// 序列化 PluginSession 只读快照，在独立线程调 WASM 钩子。失败仅 warn。
    fn forward_session_hook(
        &self,
        hook: &'static str,
        session: &Session,
        call: impl Fn(&mut WasmPlugin, String) -> anyhow::Result<()> + Send + Sync,
    ) {
        let plugin_session = tiangong_types::PluginSession::from(session);
        let json = match serde_json::to_string(&plugin_session) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("序列化 PluginSession 失败，跳过 wasm 钩子: {e}");
                return;
            }
        };
        if let Ok(mut context) = self.context.lock() {
            context.session_json = Some(json.clone());
        }
        if !self.is_enabled() {
            return;
        }
        if let Err(error) = self.call_wasm_off_runtime(move |plugin| call(plugin, json)) {
            tracing::warn!(plugin_id = %self.id, hook, %error, "wasm 生命周期钩子失败");
        }
    }

    /// 同上，但带 turn_start_idx 参数。
    fn forward_session_hook_with_idx(
        &self,
        hook: &'static str,
        session: &Session,
        turn_start_idx: usize,
        call: impl Fn(&mut WasmPlugin, String, u32) -> anyhow::Result<()> + Send + Sync,
    ) {
        let mut plugin_session = tiangong_types::PluginSession::from(session);
        // 本轮起点同时以消息 ID 提供：插件按 ID 定位不受快照消息增删影响。
        plugin_session.turn_start_message_id = session
            .messages
            .get(turn_start_idx)
            .map(|message| message.id.clone());
        let json = match serde_json::to_string(&plugin_session) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("序列化 PluginSession 失败，跳过 wasm 钩子: {e}");
                return;
            }
        };
        if let Ok(mut context) = self.context.lock() {
            context.session_json = Some(json.clone());
        }
        if !self.is_enabled() {
            return;
        }
        // idx 兼容仍按位置定位的旧版插件；快照剔除 Notice 后位置前移，同步换算。
        let idx = tiangong_core::session::plugin_turn_start_idx(session, turn_start_idx) as u32;
        if let Err(error) = self.call_wasm_off_runtime(move |plugin| call(plugin, json, idx)) {
            tracing::warn!(plugin_id = %self.id, hook, %error, "wasm 生命周期钩子失败");
        }
    }
}

// ToolSpecProvider：返回插件声明的工具规格。
impl ToolSpecProvider for WasmPluginAdapter {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.try_tool_specs().unwrap_or_else(|error| {
            tracing::error!(%error, "读取 wasm 插件工具规格失败");
            Vec::new()
        })
    }

    fn try_tool_specs(&self) -> Result<Vec<ToolSpec>, String> {
        if !self.is_enabled() {
            return Ok(Vec::new());
        }
        // 声明冻结：首次成功读取后，同一次运行内固定返回冻结值——后续
        // 不再改写（插件内部状态波动如探测重写、健康翻转不得改写请求
        // 前缀），也免去每轮 WASM 调用。换代/卸载或配置变化后重新冻结。
        let mut frozen = self
            .cached_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(specs) = frozen.clone() {
            return Ok(specs);
        }
        let specs: Vec<ToolSpec> = self
            .call_wasm_off_runtime(WasmPlugin::tool_specs)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|s| {
                Ok(ToolSpec {
                    name: s.name,
                    description: s.description,
                    input_schema: serde_json::from_str(&s.input_schema)
                        .map_err(|error| error.to_string())?,
                })
            })
            .collect::<Result<_, String>>()?;
        *frozen = Some(specs.clone());
        Ok(specs)
    }
}

// ToolOverrideHandler：处理插件工具调用（同步阻塞，移到 spawn_blocking）。
impl ToolOverrideHandler for WasmPluginAdapter {
    fn handle(
        &self,
        call: &ToolCall,
        session: &mut tiangong_core::session::Session,
        actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        if !self.is_enabled() {
            let message = format!("插件 {} 已停用，工具不可用", self.id);
            return Box::pin(async move { Some(unavailable_tool_result(message)) });
        }
        let arguments = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into());
        let wit_call = crate::loader::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments,
        };
        let feedback = self
            .feedback_tx
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let invocation = crate::invocation::RuntimeInvocation::new(
            &self.id,
            call.clone(),
            &session.id,
            session.cwd.trim(),
            actor_id,
            feedback.clone(),
        );
        if let Some(sidecar) = &self.sidecar {
            let sidecar = sidecar.clone();
            let plugin_id = self.id.clone();
            let session_id = session.id.clone();
            invocation.on_cancel(move || {
                // 引用可能已被换代（wasm 调用侧触发刷新）：取消经注册表
                // 取现役连接，取不到时退回原引用。
                let sidecar = crate::registry::refresh_stale_sidecar(Some(&sidecar), &plugin_id)
                    .unwrap_or(sidecar);
                let _ = sidecar.cancel_session(&session_id);
            });
        }
        let Some(inner) = self.current_inner() else {
            let message = format!("插件 {} 当前不可用", self.id);
            return Box::pin(async move { Some(unavailable_tool_result(message)) });
        };
        let config = self.config.clone();
        let plugin_id = self.id.clone();
        let tool_name = call.name.clone();
        Box::pin(crate::invocation::dispatch(
            invocation.clone(),
            async move {
                let result = task::spawn_blocking(move || {
                    call_wasm_off_runtime(inner, move |plugin| {
                        plugin.handle_tool_with_runtime_invocation(wit_call, &config, invocation)
                    })
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        let error = format!("{error:#}");
                        tracing::warn!(%plugin_id, %tool_name, %error, "WASM 插件工具执行失败");
                        let mut result = unavailable_tool_result(format!(
                            "插件 {plugin_id} 执行工具 {tool_name} 失败: {error}"
                        ));
                        result.stderr = error;
                        return Some(result);
                    }
                };
                Some(ToolResult {
                    ok: result.ok,
                    summary: result.summary,
                    stdout: result.stdout,
                    stderr: result.stderr,
                    exit_code: result.exit_code,
                    execution: result.execution.map(|execution| ToolExecutionRecord {
                        tool_name: execution.tool_name,
                        args: execution.args,
                        duration_ms: execution.duration_ms,
                        ok: execution.ok,
                        exit_code: execution.exit_code,
                        summary: execution.summary,
                    }),
                })
            },
        ))
    }
    fn result_header(&self, call: &ToolCall, ok: bool) -> Option<String> {
        Some(tiangong_core::tools::extension::plugin_result_header(
            &self.id, &call.name, ok,
        ))
    }
}

// PromptSectionProvider：供首次会话初始化及压缩、清理后的声明整理使用。
impl PromptSectionProvider for WasmPluginAdapter {
    fn prompt_sections(&self) -> Vec<String> {
        self.try_prompt_sections().unwrap_or_else(|error| {
            tracing::warn!(%error, "wasm prompt_sections 失败");
            Vec::new()
        })
    }

    fn try_prompt_sections(&self) -> Result<Vec<String>, String> {
        if !self.is_enabled() {
            return Ok(Vec::new());
        }
        // 声明冻结：与 try_tool_specs 同语义，首次成功读取后运行期固定。
        let mut frozen = self
            .cached_prompt_sections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sections) = frozen.clone() {
            return Ok(sections);
        }
        let sections = self
            .call_wasm_off_runtime(WasmPlugin::prompt_sections)
            .map_err(|error| error.to_string())?;
        *frozen = Some(sections.clone());
        Ok(sections)
    }
}

pub(crate) fn call_wasm_off_runtime<R>(
    inner: Arc<Mutex<WasmPlugin>>,
    call: impl FnOnce(&mut WasmPlugin) -> anyhow::Result<R> + Send,
) -> anyhow::Result<R>
where
    R: Send,
{
    crate::execution::run_outside_tokio(move || {
        let mut plugin = inner
            .lock()
            .map_err(|e| anyhow::anyhow!("wasm 插件锁中毒: {e}"))?;
        call(&mut plugin)
    })
}

fn unavailable_tool_result(message: String) -> ToolResult {
    ToolResult {
        ok: false,
        summary: message.clone(),
        stdout: String::new(),
        stderr: message,
        exit_code: 1,
        execution: None,
    }
}

/// 序列化 CoreConfig 为插件配置载荷，并附加主 Chat 模型的能力声明。
///
/// CoreConfig 只含端点信息（base_url/model 等），不含能力路由；插件需要
/// 能力信息判断自身是否需要注册（如多模态主模型直接内联图片时，
/// 附件分析插件无需再提供工具）。配置尚未初始化时附加空列表（保守保留插件工具）。
fn plugin_config_payload(config: &CoreConfig) -> anyhow::Result<String> {
    let mut value = serde_json::to_value(config)?;
    let chat_capabilities: Vec<String> = tiangong_config::registry::try_models()
        .and_then(|models| {
            models
                .routing
                .get(&tiangong_llm::models_config::RoutingSlot::Chat)
                .map(|entry| {
                    entry
                        .capabilities
                        .iter()
                        .map(|cap| cap.key().to_string())
                        .collect()
                })
        })
        .unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "chat_capabilities".to_string(),
            serde_json::json!(chat_capabilities),
        );
    }
    Ok(serde_json::to_string(&value)?)
}

/// 未加载（或声明读取失败）状态下适配器行为的回归保护。
/// 此前一次重构曾把这两类失败误退成"工具未注册"级别的降级（#501），
/// 这里锁定失败必须原样传播、原因必须准确。
#[cfg(test)]
mod unloaded_adapter_tests {
    use super::*;

    fn unloaded_adapter() -> WasmPluginAdapter {
        WasmPluginAdapter {
            inner: RwLock::new(None),
            config: PluginRuntimeConfig::default(),
            id: "unloaded-plugin".into(),
            feedback_tx: RwLock::new(None),
            context: Mutex::new(ReloadContext::default()),
            context_updates: Mutex::new(()),
            enabled: AtomicBool::new(true),
            sidecar: None,
            cached_tools: Mutex::new(None),
            cached_prompt_sections: Mutex::new(None),
        }
    }

    #[test]
    fn unreadable_declarations_propagate_failure() {
        let adapter = unloaded_adapter();
        assert!(adapter.try_tool_specs().is_err());
        assert!(adapter.try_prompt_sections().is_err());
    }

    #[tokio::test]
    async fn disabled_and_unloaded_tools_keep_their_failure_reason() {
        let adapter = unloaded_adapter();
        let call = ToolCall {
            id: scru128::new().to_string(),
            name: "sample_tool".into(),
            arguments: serde_json::json!({}),
        };
        let mut session = Session::new("unit");
        for (enabled, expected) in [(false, "已停用"), (true, "当前不可用")] {
            adapter.set_enabled(enabled);
            let result = adapter.handle(&call, &mut session, "").await.unwrap();
            assert!(!result.ok);
            assert!(result.stderr.contains(expected), "{}", result.stderr);
            assert!(!result.summary.contains("未注册"));
        }
    }

    /// 声明冻结语义：首次成功读取后运行期固定返回冻结值（插件内部
    /// 波动不改写请求前缀）；未冻结时读取失败原样传播（core 侧该插件
    /// 本轮缺席）；换代（升级/卸载）清空冻结，重新冻结新版本。
    #[test]
    fn declaration_freeze_keeps_specs_stable_within_run() {
        let adapter = unloaded_adapter();
        // 未冻结时失败原样传播。
        assert!(adapter.try_tool_specs().is_err());
        assert!(adapter.try_prompt_sections().is_err());

        // 模拟首次成功读取后的冻结值：后续读取（无论底层成败）一律
        // 返回冻结值，不再触达 WASM。
        let spec = ToolSpec {
            name: "frozen_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        *adapter
            .cached_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(vec![spec]);
        *adapter
            .cached_prompt_sections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(vec!["冻结提示".into()]);
        let specs = adapter.try_tool_specs().expect("冻结值应直接返回");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "frozen_tool");
        assert_eq!(
            adapter.try_prompt_sections().expect("冻结值应直接返回"),
            vec!["冻结提示".to_string()]
        );

        let mut config = CoreConfig::default();
        adapter.context.lock().unwrap().config_json = Some(plugin_config_payload(&config).unwrap());
        adapter.context.lock().unwrap().config_applied = true;
        adapter.on_config_updated(&config);
        assert!(
            adapter.context.lock().unwrap().config_applied,
            "相同配置不应再次调用已不可用的实例"
        );
        assert_eq!(adapter.try_tool_specs().unwrap()[0].name, "frozen_tool");
        assert_eq!(adapter.try_prompt_sections().unwrap(), vec!["冻结提示"]);

        // 任意配置字段变化即可触发通知（模型端点已不属于 CoreConfig）。
        config.custom_system_prompt = "changed-prompt".into();
        adapter.on_config_updated(&config);
        assert!(
            !adapter.context.lock().unwrap().config_applied,
            "失败的配置通知不能缓存为成功"
        );
        assert!(adapter.cached_tools.lock().unwrap().is_none());
        assert!(adapter.cached_prompt_sections.lock().unwrap().is_none());
        assert!(adapter.try_tool_specs().is_err());
        assert!(adapter.try_prompt_sections().is_err());

        let workspace = std::path::Path::new("/test-workspace");
        {
            let mut context = adapter.context.lock().unwrap();
            context.workspace = Some(workspace.to_string_lossy().to_string());
            context.trust_mode = Some(TrustMode::FullTrust);
            context.workspace_applied = true;
        }
        adapter.set_execution_context(Some(workspace), TrustMode::FullTrust);
        adapter.set_workspace(Some(workspace));
        adapter.set_trust_mode(TrustMode::FullTrust);
        assert!(adapter.context.lock().unwrap().workspace_applied);
        adapter.set_execution_context(None, TrustMode::Supervised);
        assert!(!adapter.context.lock().unwrap().workspace_applied);

        // 升级换代（换内部实例）：冻结作废，读取失败重新传播。
        *adapter.cached_tools.lock().unwrap() = Some(specs);
        *adapter.cached_prompt_sections.lock().unwrap() = Some(vec!["冻结提示".into()]);
        adapter.release_inner();
        assert!(
            adapter.try_tool_specs().is_err(),
            "换代后不得沿用旧版本冻结"
        );
        assert!(adapter.try_prompt_sections().is_err());
    }
}
