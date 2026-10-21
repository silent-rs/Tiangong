//! WASM 组件加载器。
//!
//! 负责：
//! - 管理 App 级共享的 [`Engine`] 和 [`Linker`]；
//! - 编译并缓存单文件 `.wasm` Component；
//! - 从编译结果创建资源受限的独立 [`Store`] 和插件实例。

use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, StoreLimitsBuilder};

use crate::bindings::TiangongPlugin;
use crate::bindings::exports::tiangong::plugin::plugin::{PluginError, ToolCall as WitToolCall};
use crate::config::PluginRuntimeConfig;
use crate::host_state::HostState;
use crate::sidecar::SidecarConnection;
use crate::sidecar::SidecarInvocationContext;

static SHARED_ENGINE: OnceLock<Engine> = OnceLock::new();
static SHARED_LINKER: OnceLock<Arc<Linker<HostState>>> = OnceLock::new();

/// host import 的 host_getter：返回 HostState 自身的可变借用。
fn host_self_getter(state: &mut HostState) -> &mut HostState {
    state
}

/// 所有插件实例共享同一个 Engine，确保缓存的 Component 可直接复用。
pub fn shared_engine() -> &'static Engine {
    SHARED_ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        config.strategy(wasmtime::Strategy::Cranelift);
        Engine::new(&config).expect("创建 Wasmtime Engine 失败")
    })
}

/// 所有插件实例共享已注册宿主接口的 Linker。
pub fn shared_linker() -> &'static Arc<Linker<HostState>> {
    SHARED_LINKER.get_or_init(|| {
        let mut linker = Linker::<HostState>::new(shared_engine());
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).expect("接入 WASI 失败");
        crate::bindings::tiangong::plugin::clock::add_to_linker::<HostState, HasSelf<HostState>>(
            &mut linker,
            host_self_getter,
        )
        .expect("接入 clock 失败");
        crate::bindings::tiangong::plugin::sidecar::add_to_linker::<HostState, HasSelf<HostState>>(
            &mut linker,
            host_self_getter,
        )
        .expect("接入 sidecar 失败");
        crate::bindings::tiangong::plugin::feedback::add_to_linker::<HostState, HasSelf<HostState>>(
            &mut linker,
            host_self_getter,
        )
        .expect("接入 feedback 失败");
        Arc::new(linker)
    })
}

/// 编译 WASM 字节。生产路径在预加载或热加载时调用一次并缓存结果。
pub fn compile_component(bytes: &[u8]) -> Result<Component> {
    Component::new(shared_engine(), bytes)
        .map_err(|error| anyhow::anyhow!("编译 wasm 组件失败: {error}"))
}

/// 从已编译 Component 创建独立 Store 和插件实例。
pub fn instantiate_component(
    component: &Component,
    config: &PluginRuntimeConfig,
    sidecar: Option<Arc<dyn SidecarConnection>>,
    plugin_id: &str,
    storage_access: bool,
) -> Result<WasmPlugin> {
    let limits = StoreLimitsBuilder::new()
        .memory_size(config.memory_limit)
        .build();
    let mut store = Store::new(
        shared_engine(),
        HostState::new(limits, sidecar, plugin_id.to_string(), storage_access),
    );
    store.limiter(|state: &mut HostState| state.limits_mut());
    let _ = store.set_fuel(config.fuel_limit);
    store.set_epoch_deadline(u64::MAX);

    let instance = TiangongPlugin::instantiate(&mut store, component, shared_linker())
        .map_err(|error| anyhow::anyhow!("实例化 wasm 组件失败: {error}"))?;
    Ok(WasmPlugin { instance, store })
}

/// 兼容直接从路径加载的入口；生产注册表会分别执行编译和实例化以复用缓存。
pub struct WasmPluginLoader {
    sidecar: Option<Arc<dyn SidecarConnection>>,
}

impl WasmPluginLoader {
    /// 以给定配置创建加载器，不注入 sidecar（invoke 返回 unavailable）。
    pub fn new(config: &PluginRuntimeConfig) -> Result<Self> {
        Self::with_sidecar(config, None)
    }

    /// 以给定配置创建加载器，并注入 sidecar 连接。
    pub fn with_sidecar(
        _config: &PluginRuntimeConfig,
        sidecar: Option<Arc<dyn SidecarConnection>>,
    ) -> Result<Self> {
        let _ = shared_engine();
        let _ = shared_linker();
        Ok(Self { sidecar })
    }

    /// 引擎句柄，供外部（如 epoch 心跳线程）调用 `increment_epoch`。
    pub fn engine(&self) -> &'static Engine {
        shared_engine()
    }

    /// 加载并实例化一个 `.wasm` Component，返回宿主侧 [`WasmPlugin`]。
    pub fn load(&self, wasm_path: &Path, config: &PluginRuntimeConfig) -> Result<WasmPlugin> {
        let plugin_id = wasm_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("plugin");
        self.load_for_plugin(wasm_path, config, plugin_id)
    }

    /// 使用清单中的插件 ID 加载并实例化一个 `.wasm` Component。
    pub fn load_for_plugin(
        &self,
        wasm_path: &Path,
        config: &PluginRuntimeConfig,
        plugin_id: &str,
    ) -> Result<WasmPlugin> {
        let bytes = std::fs::read(wasm_path).map_err(|e| {
            anyhow::anyhow!("读取 wasm 组件失败 {path}: {e}", path = wasm_path.display())
        })?;
        self.load_bytes_for_plugin(&bytes, config, plugin_id, false)
            .map_err(|error| {
                anyhow::anyhow!(
                    "加载 wasm 组件失败 {path}: {error}",
                    path = wasm_path.display()
                )
            })
    }

    /// 从字节编译并实例化。注册表生产路径会绕过此组合入口以复用编译结果。
    pub fn load_bytes_for_plugin(
        &self,
        bytes: &[u8],
        config: &PluginRuntimeConfig,
        plugin_id: &str,
        storage_access: bool,
    ) -> Result<WasmPlugin> {
        let component = compile_component(bytes)?;
        instantiate_component(
            &component,
            config,
            self.sidecar.clone(),
            plugin_id,
            storage_access,
        )
    }
}

/// 宿主侧持有的已实例化 WASM 插件句柄。
///
/// 内部包含独立的 [`Store`]，编译产物和 Engine 由所有实例共享。
pub struct WasmPlugin {
    instance: TiangongPlugin,
    store: Store<HostState>,
}

impl WasmPlugin {
    pub fn set_feedback(&mut self, feedback: tiangong_core::core::plugin::PluginFeedbackTx) {
        self.store.data_mut().set_feedback(feedback);
    }

    /// 插件描述符。
    pub fn describe(&mut self) -> Result<Descriptor> {
        self.instance
            .tiangong_plugin_plugin()
            .call_describe(&mut self.store)
            .map_err(|e| anyhow::anyhow!("describe 调用失败: {e}"))?
            .map_err(plugin_err)
            .map(|d| Descriptor {
                id: d.id,
                name: d.name,
                version: d.version,
            })
    }

    /// 插件声明的工具规格（JSON Schema 仍为文本）。
    pub fn tool_specs(&mut self) -> Result<Vec<Spec>> {
        self.instance
            .tiangong_plugin_plugin()
            .call_tool_specs(&mut self.store)
            .map_err(|e| anyhow::anyhow!("tool-specs 调用失败: {e}"))?
            .map_err(plugin_err)
            .map(|specs| {
                specs
                    .into_iter()
                    .map(|s| Spec {
                        name: s.name,
                        description: s.description,
                        input_schema: s.input_schema,
                    })
                    .collect()
            })
    }

    /// 插件贡献的 prompt 段落。
    pub fn prompt_sections(&mut self) -> Result<Vec<String>> {
        self.instance
            .tiangong_plugin_plugin()
            .call_prompt_sections(&mut self.store)
            .map_err(|e| anyhow::anyhow!("prompt-sections 调用失败: {e}"))?
            .map_err(plugin_err)
    }

    /// 在施加资源限制的前提下处理一次工具调用。
    pub fn handle_tool(&mut self, call: ToolCall, limits: &PluginRuntimeConfig) -> Result<Outcome> {
        self.handle_tool_inner(call, limits, None)
    }

    /// 使用宿主权威调用上下文处理工具调用。Core 的实际执行路径使用此入口；
    /// 直接调用旧入口时不会向 sidecar 提供工作区权限。
    pub fn handle_tool_with_context(
        &mut self,
        call: ToolCall,
        limits: &PluginRuntimeConfig,
        invocation_context: SidecarInvocationContext,
    ) -> Result<Outcome> {
        self.handle_tool_inner(call, limits, Some(invocation_context))
    }

    pub fn handle_tool_with_full_context(
        &mut self,
        call: ToolCall,
        limits: &PluginRuntimeConfig,
        invocation_context: crate::protocol::RequestInvocationContext,
    ) -> Result<Outcome> {
        self.handle_tool_inner_full(call, limits, Some(invocation_context))
    }

    pub(crate) fn handle_tool_with_runtime_invocation(
        &mut self,
        call: ToolCall,
        limits: &PluginRuntimeConfig,
        invocation: crate::invocation::RuntimeInvocation,
    ) -> Result<Outcome> {
        let context = invocation.context().clone();
        self.store
            .data_mut()
            .set_runtime_invocation(Some(invocation));
        let result = self.handle_tool_inner_full(call, limits, Some(context));
        self.store.data_mut().set_runtime_invocation(None);
        result
    }

    fn handle_tool_inner(
        &mut self,
        call: ToolCall,
        limits: &PluginRuntimeConfig,
        invocation_context: Option<SidecarInvocationContext>,
    ) -> Result<Outcome> {
        let invocation_context =
            invocation_context.map(|context| crate::protocol::RequestInvocationContext {
                session_id: context.session_id,
                invocation_id: context.invocation_id,
                workspace: context
                    .authoritative_workspace
                    .to_string_lossy()
                    .into_owned(),
                actor_id: String::new(),
                deadline_ms: None,
            });
        self.handle_tool_inner_full(call, limits, invocation_context)
    }

    fn handle_tool_inner_full(
        &mut self,
        call: ToolCall,
        limits: &PluginRuntimeConfig,
        invocation_context: Option<crate::protocol::RequestInvocationContext>,
    ) -> Result<Outcome> {
        // 单次调用前重置 fuel 与 epoch deadline。
        // set_fuel 仅在未开启 consume_fuel 时返回 Err，配置已开启，安全忽略。
        let _ = self.store.set_fuel(limits.fuel_limit);
        // Agent 调用不设墙钟时限；fuel 与内存上限仍防止失控执行。
        self.store.set_epoch_deadline(u64::MAX);

        let wit_call = WitToolCall {
            id: call.id,
            name: call.name,
            arguments: call.arguments,
        };

        self.store
            .data_mut()
            .set_full_invocation_context(invocation_context);
        let result = match self
            .instance
            .tiangong_plugin_plugin()
            .call_handle_tool(&mut self.store, &wit_call)
        {
            Ok(Ok(res)) => Ok(Outcome {
                ok: res.ok,
                summary: res.summary,
                stdout: res.stdout,
                stderr: res.stderr,
                exit_code: res.exit_code,
                execution: res.execution.map(|execution| OutcomeExecution {
                    tool_name: execution.tool_name,
                    args: execution.args,
                    duration_ms: execution.duration_ms,
                    ok: execution.ok,
                    exit_code: execution.exit_code,
                    summary: execution.summary,
                }),
            }),
            Ok(Err(e)) => Err(plugin_err(e)),
            Err(e) => Err(anyhow::anyhow!("handle-tool 调用失败: {e}")),
        };
        self.store.data_mut().set_full_invocation_context(None);
        result
    }

    /// 关闭插件。
    pub fn shutdown(&mut self) -> Result<()> {
        match self
            .instance
            .tiangong_plugin_plugin()
            .call_shutdown(&mut self.store)
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(plugin_err(e)),
            Err(e) => Err(anyhow::anyhow!("shutdown 调用失败: {e}")),
        }
    }

    // ── 通用生命周期 ──

    /// 注入工作目录与信任模式（对应 `Plugin::set_workspace`）。
    pub fn set_workspace(&mut self, workspace: Option<String>, full_trust: bool) -> Result<()> {
        self.instance
            .tiangong_plugin_plugin()
            .call_set_workspace(&mut self.store, workspace.as_deref(), full_trust)
            .map_err(|e| anyhow::anyhow!("set-workspace 调用失败: {e}"))?
            .map_err(plugin_err)
    }

    /// 通知 WASM 组件 CoreConfig 变更（对应 `Plugin::on_config_updated`）。
    /// `config_json` 为 CoreConfig 的 JSON 文本。
    pub fn on_config_updated(&mut self, config_json: String) -> Result<()> {
        self.instance
            .tiangong_plugin_plugin()
            .call_on_config_updated(&mut self.store, &config_json)
            .map_err(|e| anyhow::anyhow!("on-config-updated 调用失败: {e}"))?
            .map_err(plugin_err)
    }

    /// 会话就绪钩子。
    pub fn on_session_ready(&mut self, session_json: String) -> Result<()> {
        self.instance
            .tiangong_plugin_plugin()
            .call_on_session_ready(&mut self.store, &session_json)
            .map_err(|e| anyhow::anyhow!("on-session-ready 调用失败: {e}"))?
            .map_err(plugin_err)
    }

    /// 轮次开始钩子。
    pub fn on_turn_started(&mut self, session_json: String, turn_start_idx: u32) -> Result<()> {
        self.instance
            .tiangong_plugin_plugin()
            .call_on_turn_started(&mut self.store, &session_json, turn_start_idx)
            .map_err(|e| anyhow::anyhow!("on-turn-started 调用失败: {e}"))?
            .map_err(plugin_err)
    }

    /// 轮次结束钩子（触发 micro 反刍）。
    pub fn on_turn_finished(&mut self, session_json: String, turn_start_idx: u32) -> Result<()> {
        self.instance
            .tiangong_plugin_plugin()
            .call_on_turn_finished(&mut self.store, &session_json, turn_start_idx)
            .map_err(|e| anyhow::anyhow!("on-turn-finished 调用失败: {e}"))?
            .map_err(plugin_err)
    }

    /// 会话结束钩子（触发 meso 反刍）。
    pub fn on_session_ended(&mut self, session_json: String) -> Result<()> {
        self.instance
            .tiangong_plugin_plugin()
            .call_on_session_ended(&mut self.store, &session_json)
            .map_err(|e| anyhow::anyhow!("on-session-ended 调用失败: {e}"))?
            .map_err(plugin_err)
    }

    // ── UI 贡献：设置页 ──

    /// 插件贡献的设置页入口列表。
    pub fn contributions(&mut self) -> Result<Vec<Contribution>> {
        Ok(self
            .instance
            .tiangong_plugin_plugin_ui()
            .call_contributions(&mut self.store)
            .map_err(|e| anyhow::anyhow!("contributions 调用失败: {e}"))?
            .map_err(plugin_err)?
            .into_iter()
            .map(|c| Contribution {
                id: c.id,
                title: c.title,
                description: c.description,
                icon: c.icon,
                group: c.group,
                has_view: c.has_view,
            })
            .collect())
    }

    /// @提及查询的单请求预算（含 sidecar 往返）：补全是交互路径，
    /// 超时的来源返回空候选，不阻塞输入框。
    const MENTION_QUERY_TIMEOUT_MS: u64 = 3000;
    /// 按查询返回插件贡献的 @提及候选。
    ///
    /// 复用 0.1.0 world 已有的 plugin-ui 消息通道，避免给现有 world 增加强制导出：
    /// 旧插件收到未知方法会返回 plugin-error，此处按“不支持 Mention”降级为空列表；
    /// 新插件返回 MentionCandidate JSON 数组。请求级上下文，不调用 set_workspace；
    /// 错误返回时也清除宿主上下文。
    pub fn query_mentions(
        &mut self,
        query: &tiangong_types::MentionQuery,
    ) -> Result<Vec<MentionCandidate>> {
        let request = crate::bindings::exports::tiangong::plugin::plugin_ui::ViewMessageRequest {
            method: "__tiangong.mention_candidates.v1".into(),
            payload: serde_json::to_string(query)?,
        };
        let limits = PluginRuntimeConfig::default();
        self.store
            .set_fuel(limits.fuel_limit)
            .map_err(|e| anyhow::anyhow!("重置 mention fuel 失败: {e}"))?;
        self.store.set_epoch_deadline(limits.epoch_deadline_ticks());
        self.store.data_mut().set_full_invocation_context(Some(
            crate::protocol::RequestInvocationContext {
                session_id: query.context.session_id.clone().unwrap_or_default(),
                invocation_id: scru128::new().to_string(),
                workspace: query.context.workspace.clone().unwrap_or_default(),
                actor_id: String::new(),
                deadline_ms: Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64
                        + Self::MENTION_QUERY_TIMEOUT_MS,
                ),
            },
        ));
        let response = self
            .instance
            .tiangong_plugin_plugin_ui()
            .call_handle_view_message(&mut self.store, &request);
        self.store.data_mut().set_full_invocation_context(None);
        self.store.set_epoch_deadline(u64::MAX);
        let response = response.map_err(|e| anyhow::anyhow!("mention-candidates 调用失败: {e}"))?;
        let payload = match response {
            Ok(response) => response.payload,
            Err(_) => return Ok(Vec::new()),
        };
        serde_json::from_str(&payload)
            .map_err(|e| anyhow::anyhow!("mention-candidates 响应格式错误: {e}"))
    }

    /// 打开页面，返回入口 HTML。
    pub fn open_view(&mut self, contribution_id: String) -> Result<String> {
        Ok(self
            .instance
            .tiangong_plugin_plugin_ui()
            .call_open_view(&mut self.store, &contribution_id)
            .map_err(|e| anyhow::anyhow!("open-view 调用失败: {e}"))?
            .map_err(plugin_err)?
            .html)
    }

    /// 获取页面资源（返回字节 + MIME）。
    pub fn get_view_resource(&mut self, path: String) -> Result<(Vec<u8>, String)> {
        let res = self
            .instance
            .tiangong_plugin_plugin_ui()
            .call_get_view_resource(&mut self.store, &path)
            .map_err(|e| anyhow::anyhow!("get-view-resource 调用失败: {e}"))?
            .map_err(plugin_err)?;
        Ok((res.data, res.mime))
    }

    /// 处理页面消息（iframe ↔ 插件双向通信）。
    pub fn handle_view_message(&mut self, method: String, payload: String) -> Result<String> {
        Ok(self
            .instance
            .tiangong_plugin_plugin_ui()
            .call_handle_view_message(
                &mut self.store,
                &crate::bindings::exports::tiangong::plugin::plugin_ui::ViewMessageRequest {
                    method,
                    payload,
                },
            )
            .map_err(|e| anyhow::anyhow!("handle-view-message 调用失败: {e}"))?
            .map_err(plugin_err)?
            .payload)
    }

    /// 引擎句柄（测试与 epoch 心跳用）。
    pub fn engine(&self) -> &'static Engine {
        shared_engine()
    }

    /// 暴露 store 的可变借用，用于测试断言（如剩余 fuel）。
    pub fn store(&mut self) -> &mut Store<HostState> {
        &mut self.store
    }
}

/// 轻量工具调用入参，解耦宿主对 wasmtime 生成类型的依赖。
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 描述符结果。
#[derive(Debug, Clone)]
pub struct Descriptor {
    pub id: String,
    pub name: String,
    pub version: String,
}

/// 工具规格结果（JSON Schema 仍为文本）。
#[derive(Debug)]
pub struct Spec {
    pub name: String,
    pub description: String,
    pub input_schema: String,
}

/// 工具调用结果。
#[derive(Debug)]
pub struct Outcome {
    pub ok: bool,
    pub summary: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub execution: Option<OutcomeExecution>,
}

#[derive(Debug)]
pub struct OutcomeExecution {
    pub tool_name: String,
    pub args: Vec<String>,
    pub duration_ms: u64,
    pub ok: bool,
    pub exit_code: i32,
    pub summary: String,
}

/// 设置页贡献项（镜像 WIT record）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Contribution {
    pub id: String,
    pub title: String,
    pub description: String,
    pub icon: String,
    pub group: String,
    pub has_view: bool,
}

/// @提及候选项（镜像 WIT record）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MentionCandidate {
    pub value: String,
    pub label: String,
    pub kind: String,
    pub hint: String,
    /// 候选标记（chip 角标字符），插件可选提供；旧插件不带时回退空。
    #[serde(default)]
    pub mark: String,
}

/// 把 WIT 层的 `plugin-error` 转为 anyhow。
fn plugin_err(e: PluginError) -> anyhow::Error {
    match e {
        PluginError::Message(m) => anyhow::anyhow!(m),
    }
}
