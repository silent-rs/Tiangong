//! Core 的 runtime 聚合桥：Core 插件列表的唯一编译期成员。
//!
//! 桌面端 Core 的 `plugins` 列表只持有一个 [`RuntimeCorePlugin`]——构造后
//! 永不变化，对 Core 而言插件集合是稳定的。已安装插件（WASM/TS 工具）的
//! 全部能力（工具声明、执行路由、prompt 段落、@提及、生命周期钩子、
//! exec_env）由桥在**被调用时**向 runtime 注册表聚合：
//!
//! - 新装插件：下一次聚合自然出现（无需通知 Core，turn 进行中不重聚，
//!   下一轮生效）；
//! - 启停/升级/卸载：runtime 经 Weak 就地更新既有适配器，桥不感知；
//! - 差量装载的三段式锁纪律见 [`RuntimeCorePlugin::adapters`]。
//!
//! 每个桥实例持独立的交付表（per-Core 适配器隔离，与静态装配时代
//! 「各 Core 独立实例化」语义一致）。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tiangong_core::core::plugin::Plugin;
use tiangong_core::permission::TrustMode;
use tiangong_core::session::Session;
use tiangong_core::tools::extension::{
    PromptSectionProvider, ToolOverrideHandler, ToolSpecProvider, call_with_name,
    namespace_tool_specs,
};
use tiangong_core::tools::result::ToolResult;
use tiangong_llm::tool::{ToolCall, ToolSpec};
use tiangong_types::PluginSession;

use crate::registry::{self, RuntimeKind};

/// 工具结果抬头的状态词。
fn result_status_text(ok: bool) -> &'static str {
    if ok { "成功" } else { "失败" }
}

/// 插件工具结果的抬头：带上实际处理的插件 id 与插件声明的原工具名。
///
/// 抬头由插件侧（runtime 适配器）提供，core 只原样放在结果首行。
pub fn plugin_result_header(plugin_id: &str, tool_name: &str, ok: bool) -> String {
    format!(
        "调用插件 {plugin_id} 的 {tool_name}：{}",
        result_status_text(ok)
    )
}

/// 无法定位到具体插件时的抬头（如自制插件通道缺少参数）。
fn tool_result_header(tool_name: &str, ok: bool) -> String {
    format!("调用工具 {tool_name}：{}", result_status_text(ok))
}

/// 自制插件动态调用工具名（description 恒定，不含任何插件信息——
/// 插件装卸不改变 tools 声明，KV cache 前缀保持稳定）。
pub const CALL_LOCAL_PLUGIN_TOOL: &str = "call_local_plugin";
/// 自制插件清单查询工具名。
pub const LIST_LOCAL_PLUGINS_TOOL: &str = "list_local_plugins";

/// 清单注入的 tool_name（注入通道使用，注册在固定声明中的解释见
/// `CALL_LOCAL_PLUGIN_TOOL` 的 description）。
pub const LOCAL_PLUGIN_LIST_INJECTION: &str = "local_plugin_list";

/// `call_local_plugin` 的路由结果（同步段解析，异步段执行/返回错误）。
enum LocalCallRouting {
    /// 参数或目标无效：message 为主错误，inventory 为随附清单（None=不必附）。
    Invalid {
        message: String,
        inventory: Option<serde_json::Value>,
    },
    /// 命中自制插件方法，转发执行。
    Forwarded {
        adapter: Arc<dyn Plugin>,
        inner_call: ToolCall,
    },
}

impl LocalCallRouting {
    fn invalid(message: &str, inventory: Option<serde_json::Value>) -> Self {
        Self::Invalid {
            message: message.to_string(),
            inventory,
        }
    }
}

/// 失败结果：ok:false 的提示（携带当前清单让模型一次纠正到位）。
fn local_call_failure(message: &str, inventory: Option<serde_json::Value>) -> ToolResult {
    let mut stderr = message.to_string();
    if let Some(inventory) = inventory {
        stderr.push_str(" 当前自制插件清单：");
        stderr.push_str(&inventory.to_string());
    }
    ToolResult {
        ok: false,
        summary: message.to_string(),
        stdout: String::new(),
        stderr,
        exit_code: 1,
        execution: None,
    }
}

fn call_local_plugin_spec() -> ToolSpec {
    ToolSpec {
        name: CALL_LOCAL_PLUGIN_TOOL.to_string(),
        description: "调用本机自制插件的方法。可用插件与方法名用 list_local_plugins 查询；对话中若有系统投递的自制插件清单（数据来源 local_plugin_list），可直接使用最近一条，无需重复查询。".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "plugin_name":   { "type": "string", "description": "自制插件名（清单中的 name）" },
                "function_name": { "type": "string", "description": "要调用的方法名（清单中的 functions[].name）" },
                "args":          { "type": "object",  "description": "方法参数，结构见清单中的 args_schema" }
            },
            "required": ["plugin_name", "function_name"]
        }),
    }
}

fn list_local_plugins_spec() -> ToolSpec {
    ToolSpec {
        name: LIST_LOCAL_PLUGINS_TOOL.to_string(),
        description: "列出当前可用的自制插件及其方法签名。".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {}
        }),
    }
}

/// 聚合工具路由：对外名称 → 处理插件与其声明的原名。
#[derive(Clone)]
struct ToolRoute {
    adapter: Arc<dyn Plugin>,
    original: String,
}

impl ToolRoute {
    fn inner_call(&self, call: &ToolCall) -> ToolCall {
        call_with_name(call, &self.original)
    }
}

/// Core 侧的 runtime 聚合插件。
pub struct RuntimeCorePlugin {
    storage_root: PathBuf,
    runtime: RuntimeKind,
    /// 已交付适配器（plugin_id → 适配器），per-Core 隔离。
    delivered: Mutex<HashMap<String, Arc<dyn Plugin>>>,
    /// 最近一次聚合构建的工具路由表（tool_name → 拥有者适配器）。
    /// `tool_specs` 聚合时重建；`handle` 只读查询。
    tool_routes: RwLock<HashMap<String, ToolRoute>>,
    /// 本 Core 自留的反馈通道（turn 内有效）：自制插件清单变化时经
    /// 注入通道追加到对话历史。
    feedback_tx: RwLock<Option<tiangong_core::core::plugin::PluginFeedbackTx>>,
    /// 最近一次注入的自制插件清单及注入时的上下文摘要：轮开始时比对，
    /// 清单变化或其后发生过压缩才注入——append-only，cache 前缀不受影响。
    last_inventory: Mutex<Option<InjectedInventory>>,
    /// 最近一轮结束时的上下文摘要（`on_turn_finished` 更新）。
    latest_summary: Mutex<Option<String>>,
    /// 所属会话：桥随 Core 构造为该会话实例化，装载的适配器在构造时获得。
    session_id: String,
}

/// 最近一次注入的自制插件清单状态。
#[derive(PartialEq, Eq)]
struct InjectedInventory {
    /// 清单序列化文本。
    text: String,
    /// 注入时的上下文摘要：摘要变化说明其后发生过压缩，清单可能已被折叠。
    context_summary: Option<String>,
}

impl RuntimeCorePlugin {
    /// 为一个会话构造桥实例：在构造该会话的 Core 时调用（各入口统一）。
    ///
    /// 会话身份等与对话内容无关的会话数据在此一次性提供；已安装插件的
    /// 适配器在被调用时按需装载，构造时即获得所属会话。
    pub fn new(
        runtime: RuntimeKind,
        storage_root: PathBuf,
        session_id: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            storage_root,
            runtime,
            delivered: Mutex::new(HashMap::new()),
            tool_routes: RwLock::new(HashMap::new()),
            feedback_tx: RwLock::new(None),
            last_inventory: Mutex::new(None),
            latest_summary: Mutex::new(None),
            session_id: session_id.into(),
        })
    }

    /// 当前应交付的适配器集合（三段式差量同步）。
    ///
    /// **锁纪律**：`delivered` 锁的两段持有都只做纯内存操作（微秒级）；
    /// 慢操作（`load_core_plugin` 的 WASM 实例化、与迁移路径的注册表锁
    /// 竞争）全部在**锁外**执行——否则并发聚合（`tool_specs` 与
    /// `mention_candidates` 同时到达）会在本锁上串行卡等。
    /// 并发首见同一新插件时可能各自装载一次，合并段只保留先插入者；
    /// 落选适配器随 Weak 失效被回收，无泄漏。
    fn adapters(&self) -> Vec<Arc<dyn Plugin>> {
        let ids = registry::core_plugin_ids(&self.storage_root, self.runtime);
        let missing = {
            let Ok(mut delivered) = self.delivered.lock() else {
                return Vec::new();
            };
            delivered.retain(|id, _| ids.contains(id));
            ids.iter()
                .filter(|id| !delivered.contains_key(*id))
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut loaded = Vec::with_capacity(missing.len());
        for id in missing {
            if let Some(adapter) = registry::load_core_plugin(&id, self.runtime, &self.session_id) {
                loaded.push((id, adapter));
            }
        }
        let Ok(mut delivered) = self.delivered.lock() else {
            return Vec::new();
        };
        for (id, adapter) in loaded {
            delivered.entry(id).or_insert(adapter);
        }
        // core_plugin_ids 已按（prompt 置顶 + id 字典序）排序，
        // 与 core::plugin::prepare_plugins 的插件序一致。
        delivered.values().cloned().collect()
    }

    /// 用聚合结果重建工具路由表，返回聚合的工具声明（含去重）。
    ///
    /// 分流：自制插件（local 签名）不进声明——其能力经对话内清单 +
    /// [`CALL_LOCAL_PLUGIN_TOOL`] 固定通道调用，装卸不改变 tools 字段；
    /// 官方/三方/未签名插件保持逐个声明的现状。
    fn aggregate_tool_specs(&self) -> Vec<ToolSpec> {
        let adapters = self.adapters();
        let mut specs = Vec::new();
        let mut routes: HashMap<String, ToolRoute> = HashMap::new();
        // 插件工具一律以 `{插件id}__{工具名}` 暴露，路由表记录原名以便转发。
        for adapter in adapters
            .iter()
            .filter(|adapter| !registry::is_local_plugin(adapter.id()))
        {
            for (original, spec) in namespace_tool_specs(adapter.id(), adapter.tool_specs()) {
                if routes.contains_key(&spec.name) {
                    tracing::warn!(tool = %spec.name, plugin = %adapter.id(), "工具名冲突，保留先注册者");
                    continue;
                }
                routes.insert(
                    spec.name.clone(),
                    ToolRoute {
                        adapter: adapter.clone(),
                        original,
                    },
                );
                specs.push(spec);
            }
        }
        // 固定通道工具（description 恒定）。路由表按函数名直查自制适配器，
        // 不经过 routes 缓存。
        specs.push(call_local_plugin_spec());
        specs.push(list_local_plugins_spec());
        if let Ok(mut tool_routes) = self.tool_routes.write() {
            *tool_routes = routes;
        }
        specs
    }

    fn each_adapter(&self, mut apply: impl FnMut(&Arc<dyn Plugin>)) {
        for adapter in self.adapters() {
            apply(&adapter);
        }
    }

    /// 轮开始时检查自制插件清单是否需要注入。
    ///
    /// 首轮（从未注入）且存在自制插件 → 注入基线清单；此后清单内容变化
    /// （装卸/升级/启停自制插件）→ 注入新清单（note 标注早前作废）。
    /// 自制插件为空时不推送空清单（没有信息量）；之后再出现自制插件时
    /// 按变化注入。
    ///
    /// **压缩自愈**：注入的清单与其他 Tool 消息一样可被压缩折叠，折叠后模型
    /// 侧「以最近一条清单为准」的锚点不复存在。桥看不到会话历史，按上下文
    /// 摘要判定：最近一轮结束时的摘要与注入时不同，说明其后发生过压缩，即使
    /// 清单内容未变也重新注入一份。压缩本身已重建请求前缀，此时追加注入不
    /// 额外破坏 KV cache。摘要来自 `on_turn_finished`，轮次之间的手动压缩会
    /// 在其后一轮结束时被观察到。
    ///
    /// 经反馈通道走 `Command::InjectTool` → 工具批次收敛后的安全点注入，
    /// 对话历史 append-only，KV cache 前缀不受影响。
    fn maybe_inject_local_inventory(&self) {
        let inventory = registry::local_plugin_inventory();
        let text = inventory.to_string();
        let empty = inventory["plugins"]
            .as_array()
            .is_some_and(std::vec::Vec::is_empty);
        let current = InjectedInventory {
            text,
            context_summary: self
                .latest_summary
                .lock()
                .ok()
                .and_then(|summary| summary.clone()),
        };
        let needs_inject = {
            let Ok(mut last) = self.last_inventory.lock() else {
                return;
            };
            if last.as_ref() == Some(&current) {
                false
            } else if empty {
                // 自制插件为空：不推送空清单。记录当前状态，之后再出现
                // 自制插件时清单变化，照常注入。
                *last = Some(current);
                false
            } else {
                *last = Some(current);
                true
            }
        };
        if !needs_inject {
            return;
        }
        let sent = self
            .feedback_tx
            .read()
            .ok()
            .and_then(|guard| {
                guard
                    .as_ref()
                    .map(|tx| tx.inject_tool(LOCAL_PLUGIN_LIST_INJECTION, inventory))
            })
            .unwrap_or(false);
        if !sent {
            tracing::debug!("自制插件清单注入未送达（无活跃 turn），下轮重试");
            // 回退记录，下轮重试（turn 未开/通道关闭时命令会丢）。
            if let Ok(mut last) = self.last_inventory.lock() {
                *last = None;
            }
        }
    }

    /// 解析并路由 `call_local_plugin`。
    ///
    /// 实时性：定位经 [`Self::adapters`]（每次实时差量同步注册表），
    /// 不使用 `tool_routes` 缓存——自制插件的装卸在两次聚合之间也能
    /// 正确路由或给出带清单的失败信息。
    fn route_local_plugin_call(&self, call: &ToolCall) -> LocalCallRouting {
        let arguments = &call.arguments;
        let Some(plugin_name) = arguments.get("plugin_name").and_then(|v| v.as_str()) else {
            return LocalCallRouting::invalid("缺少 plugin_name 参数", None);
        };
        let Some(function_name) = arguments.get("function_name").and_then(|v| v.as_str()) else {
            return LocalCallRouting::invalid("缺少 function_name 参数", None);
        };
        let args = arguments
            .get("args")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let adapter = self
            .adapters()
            .into_iter()
            .find(|adapter| adapter.id() == plugin_name && registry::is_local_plugin(adapter.id()));
        let Some(adapter) = adapter else {
            return LocalCallRouting::invalid(
                &format!("插件 {plugin_name} 已不存在。"),
                Some(registry::local_plugin_inventory()),
            );
        };
        let declared = adapter.tool_specs();
        let Some(_spec) = declared.iter().find(|spec| spec.name == function_name) else {
            let available: Vec<&str> = declared.iter().map(|spec| spec.name.as_str()).collect();
            return LocalCallRouting::invalid(
                &format!(
                    "插件 {plugin_name} 无方法 {function_name}。可用方法：{}",
                    available.join("、")
                ),
                None,
            );
        };
        LocalCallRouting::Forwarded {
            adapter,
            inner_call: ToolCall {
                id: format!("local_{}", scru128::new()),
                name: function_name.to_string(),
                arguments: args,
            },
        }
    }
}

impl Plugin for RuntimeCorePlugin {
    fn id(&self) -> &str {
        "plugin-runtime"
    }
    /// 聚合桥自行按真实插件 id 命名工具，固定通道保持原名。
    fn names_own_tools(&self) -> bool {
        true
    }

    fn set_execution_context(&self, workspace: Option<&std::path::Path>, trust: TrustMode) {
        self.each_adapter(|adapter| adapter.set_execution_context(workspace, trust));
    }

    fn set_feedback_tx(&self, tx: tiangong_core::core::plugin::PluginFeedbackTx) {
        // 自留一份：自制插件清单变化时经注入通道下发（turn 内有效）。
        if let Ok(mut guard) = self.feedback_tx.write() {
            *guard = Some(tx.clone());
        }
        self.each_adapter(|adapter| adapter.set_feedback_tx(tx.clone()));
    }

    fn exec_env(&self) -> std::collections::BTreeMap<String, String> {
        let mut merged = std::collections::BTreeMap::new();
        for adapter in self.adapters() {
            for (key, value) in adapter.exec_env() {
                merged.insert(key, value);
            }
        }
        merged
    }

    fn set_exec_env(&self, env: std::collections::BTreeMap<String, String>) {
        self.each_adapter(|adapter| adapter.set_exec_env(env.clone()));
    }

    fn on_cancel(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        // 与各适配器实现约定一致：取消工作在同步段完成，返回的 future 为
        // 空壳（见 WasmPluginAdapter::on_cancel）。桥在同步段逐适配器驱动；
        // 若未来出现真正的异步取消实现，需扩展 trait 签名。
        let adapters = self.adapters();
        for adapter in adapters {
            let future = adapter.on_cancel();
            let mut future = std::pin::pin!(future);
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            let _ = future.as_mut().poll(&mut context);
        }
        Box::pin(async {})
    }

    fn on_config_updated(&self, config: &tiangong_core::config::core::CoreConfig) {
        self.each_adapter(|adapter| adapter.on_config_updated(config));
    }

    fn on_session_ready(&self) {
        self.each_adapter(|adapter| adapter.on_session_ready());
    }

    fn on_turn_started(&self) {
        self.each_adapter(|adapter| adapter.on_turn_started());
        // 轮开始注入/刷新自制插件清单：覆盖首轮基线、空闲期间发生的
        // 装卸（变化事件的 turn 内注入由宿主订阅者另行投递），以及最近
        // 一条清单被压缩折叠后的自愈重注入。
        self.maybe_inject_local_inventory();
    }

    fn on_turn_finished(&self, session: &PluginSession) {
        // 记录本轮结束时的上下文摘要：压缩会改写摘要，下一轮开始时据此
        // 判断清单是否可能已被折叠。
        if let Ok(mut summary) = self.latest_summary.lock() {
            *summary = session.context_summary.clone();
        }
        self.each_adapter(|adapter| adapter.on_turn_finished(session));
    }

    fn on_session_ended(&self) {
        self.each_adapter(|adapter| adapter.on_session_ended());
    }
}

impl ToolSpecProvider for RuntimeCorePlugin {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.aggregate_tool_specs()
    }
}

impl ToolOverrideHandler for RuntimeCorePlugin {
    fn handle(
        &self,
        call: &ToolCall,
        session: &mut Session,
        actor_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<ToolResult>> + Send>> {
        // 自制插件固定通道：同步段实时路由（不使用聚合缓存），异步段
        // 转发到目标适配器或返回带清单的失败信息。
        match call.name.as_str() {
            CALL_LOCAL_PLUGIN_TOOL => match self.route_local_plugin_call(call) {
                LocalCallRouting::Forwarded {
                    adapter,
                    inner_call,
                } => adapter.handle(&inner_call, session, actor_id),
                LocalCallRouting::Invalid { message, inventory } => {
                    let failure = local_call_failure(&message, inventory);
                    Box::pin(async move { Some(failure) })
                }
            },
            LIST_LOCAL_PLUGINS_TOOL => {
                let inventory = registry::local_plugin_inventory();
                let text = serde_json::to_string_pretty(&inventory)
                    .unwrap_or_else(|_| inventory.to_string());
                Box::pin(async move {
                    Some(ToolResult {
                        ok: true,
                        summary: "当前自制插件清单".to_string(),
                        stdout: text,
                        stderr: String::new(),
                        exit_code: 0,
                        execution: None,
                    })
                })
            }
            _ => {
                let owner = self
                    .tool_routes
                    .read()
                    .ok()
                    .and_then(|routes| routes.get(&call.name).cloned());
                match owner {
                    Some(route) => route
                        .adapter
                        .handle(&route.inner_call(call), session, actor_id),
                    // 路由表在 tool_specs 聚合时重建；未知工具名不拦截，
                    // 交回 core 默认逻辑。
                    None => Box::pin(async { None }),
                }
            }
        }
    }

    fn result_header(&self, call: &ToolCall, ok: bool) -> Option<String> {
        match call.name.as_str() {
            // 自制插件经固定通道调用：抬头落到实际的插件与方法。
            CALL_LOCAL_PLUGIN_TOOL => {
                let arguments = &call.arguments;
                let plugin = arguments.get("plugin_name").and_then(|v| v.as_str());
                let function = arguments.get("function_name").and_then(|v| v.as_str());
                Some(match (plugin, function) {
                    (Some(plugin), Some(function)) => plugin_result_header(plugin, function, ok),
                    _ => tool_result_header(&call.name, ok),
                })
            }
            LIST_LOCAL_PLUGINS_TOOL => Some(tool_result_header(&call.name, ok)),
            _ => {
                let route = self
                    .tool_routes
                    .read()
                    .ok()
                    .and_then(|routes| routes.get(&call.name).cloned())?;
                route
                    .adapter
                    .result_header(&route.inner_call(call), ok)
                    .or_else(|| {
                        Some(plugin_result_header(
                            route.adapter.id(),
                            &route.original,
                            ok,
                        ))
                    })
            }
        }
    }
}

impl PromptSectionProvider for RuntimeCorePlugin {
    fn prompt_sections(&self) -> Vec<String> {
        let mut sections = Vec::new();
        for adapter in self.adapters() {
            // 分流与 aggregate_tool_specs 一致：自制插件的 prompt 段落不进
            // system prompt——装卸会打穿 KV cache 前缀。其内容随清单注入
            // 对话历史（local_plugin_inventory 的 prompt 字段），两头一致。
            if registry::is_local_plugin(adapter.id()) {
                continue;
            }
            sections.extend(adapter.prompt_sections());
        }
        sections
    }
}

/// 清单注入判定的单元测试（完整注入链路见 tests/core_bridge.rs）。
#[cfg(test)]
mod inventory_state_tests {
    use super::*;

    fn state(text: &str, summary: Option<&str>) -> InjectedInventory {
        InjectedInventory {
            text: text.to_string(),
            context_summary: summary.map(str::to_string),
        }
    }

    #[test]
    fn 清单与摘要都未变时视为已注入() {
        assert!(state("a", Some("s1")) == state("a", Some("s1")));
        assert!(state("a", None) == state("a", None));
    }

    #[test]
    fn 清单变化或其后发生压缩时需重新注入() {
        assert!(state("a", Some("s1")) != state("b", Some("s1")), "清单变化");
        assert!(state("a", None) != state("a", Some("s1")), "首次压缩");
        assert!(state("a", Some("s1")) != state("a", Some("s2")), "再次压缩");
    }
}
