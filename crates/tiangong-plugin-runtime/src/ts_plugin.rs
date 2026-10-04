//! Desktop TypeScript 工具插件的进程内适配器。
//!
//! 适配器只把清单中的工具规格和提示词接入 Core，并把工具调用转交给
//! [`crate::ts_tools`]。工具参数和结果语义完全由插件 TypeScript 实现。

use std::future::Future;
use std::pin::Pin;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tiangong_core::core::Plugin;
use tiangong_core::session::Session;
use tiangong_core::tools::extension::{
    PromptSectionProvider, ToolOverrideHandler, ToolSpecProvider,
};
use tiangong_core::tools::result::ToolResult;
use tiangong_llm::tool::{ToolCall, ToolSpec};

use crate::manifest::{PluginManifest, TsToolDecl};

#[derive(Clone, Default)]
struct TsPluginState {
    tools: Vec<TsToolDecl>,
    prompts: Vec<String>,
    /// 旧版插件以此声明工具全部由 sidecar 执行，页面仅负责展示。
    legacy_tools_direct: bool,
}

pub struct TsPluginAdapter {
    id: String,
    state: RwLock<TsPluginState>,
    enabled: AtomicBool,
    feedback_tx: RwLock<Option<tiangong_core::core::plugin::PluginFeedbackTx>>,
    /// 无界面且声明 sidecar：工具由宿主直连 sidecar 执行（不进页面接应
    /// 协议）。有界面的插件照旧走页面路径（页面可深度参与执行）。
    sidecar_direct: AtomicBool,
    /// 清单声明了 sidecar（与 sidecar_direct 分开记录，供路由组合判断）。
    has_sidecar: AtomicBool,
    /// 清单声明了 UI 贡献。
    has_ui: AtomicBool,
    /// 安装阶段验证记录中的 sidecar 能力；None 表示记录缺失或失效
    ///（路由回退 UI 或返回不可用，不做即时探测）。
    verified_sidecar: RwLock<Option<Vec<String>>>,
}

impl TsPluginAdapter {
    pub(crate) fn notify_tools_recovered(&self, names: &[String]) {
        if !self.enabled.load(Ordering::Acquire) {
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
    pub(crate) fn from_manifest(
        manifest: &PluginManifest,
        enabled: bool,
        verified_sidecar: Option<Vec<String>>,
    ) -> Self {
        Self {
            id: manifest.id.clone(),
            state: RwLock::new(TsPluginState {
                tools: manifest.tools.clone().unwrap_or_default(),
                prompts: manifest.prompt.clone().unwrap_or_default(),
                legacy_tools_direct: manifest
                    .sidecar
                    .as_ref()
                    .is_some_and(|s| s.tools_direct == Some(true)),
            }),
            enabled: AtomicBool::new(enabled),
            feedback_tx: RwLock::new(None),
            sidecar_direct: AtomicBool::new(sidecar_direct_of(manifest)),
            has_sidecar: AtomicBool::new(manifest.sidecar.is_some()),
            has_ui: AtomicBool::new(!manifest.ui_contributions().is_empty()),
            verified_sidecar: RwLock::new(verified_sidecar),
        }
    }

    pub(crate) fn reconfigure(
        &self,
        manifest: &PluginManifest,
        enabled: bool,
        verified_sidecar: Option<Vec<String>>,
    ) {
        let next = TsPluginState {
            tools: manifest.tools.clone().unwrap_or_default(),
            prompts: manifest.prompt.clone().unwrap_or_default(),
            legacy_tools_direct: manifest
                .sidecar
                .as_ref()
                .is_some_and(|s| s.tools_direct == Some(true)),
        };
        match self.state.write() {
            Ok(mut state) => *state = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
        self.set_enabled(enabled);
        self.sidecar_direct
            .store(sidecar_direct_of(manifest), Ordering::Release);
        self.has_sidecar
            .store(manifest.sidecar.is_some(), Ordering::Release);
        self.has_ui
            .store(!manifest.ui_contributions().is_empty(), Ordering::Release);
        match self.verified_sidecar.write() {
            Ok(mut verified) => *verified = verified_sidecar,
            Err(poisoned) => *poisoned.into_inner() = verified_sidecar,
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    fn handler_for_tool(&self, name: &str) -> Result<crate::invocation::HandlerKind, &'static str> {
        let legacy_direct = self
            .state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .legacy_tools_direct;
        let verified = self
            .verified_sidecar
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if legacy_direct && self.has_ui.load(Ordering::Acquire) {
            // 旧握手没有 capabilities。只在完整制品检查仍有效时兼容旧声明，
            // 不因清单标记绕过验证，也不能回退到不接收工具的展示页面。
            let capabilities = verified
                .as_deref()
                .ok_or("插件运行检查尚未完成或已失效，请在插件管理中重新验证后重试")?;
            if capabilities.is_empty()
                || crate::invocation::capabilities_handle_tool(capabilities, name)
            {
                return Ok(crate::invocation::HandlerKind::Sidecar);
            }
            return Err("插件后台未声明支持该工具，请更新插件后重试");
        }
        Ok(crate::invocation::select_ts_handler(
            self.sidecar_direct.load(Ordering::Acquire),
            self.has_sidecar.load(Ordering::Acquire),
            self.has_ui.load(Ordering::Acquire),
            verified.as_deref(),
            name,
        ))
    }

    fn tool(&self, name: &str) -> Option<TsToolDecl> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }
}

impl Plugin for TsPluginAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_feedback_tx(&self, tx: tiangong_core::core::plugin::PluginFeedbackTx) {
        if let Ok(mut feedback) = self.feedback_tx.write() {
            *feedback = Some(tx);
        }
    }
}

impl ToolSpecProvider for TsPluginAdapter {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        if !self.is_enabled() {
            return Vec::new();
        }
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .tools
            .iter()
            .map(|tool| ToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            })
            .collect()
    }
}

impl ToolOverrideHandler for TsPluginAdapter {
    fn handle(
        &self,
        call: &ToolCall,
        session: &mut Session,
        actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        if !self.is_enabled() {
            let failure = sidecar_tool_failure(&self.id, "插件已停用，工具不可用");
            return Box::pin(async move { Some(failure) });
        }
        let Some(tool) = self.tool(&call.name) else {
            let failure =
                sidecar_tool_failure(&self.id, "当前插件已不提供该工具，请重新加载会话能力");
            return Box::pin(async move { Some(failure) });
        };
        let plugin_id = self.id.clone();
        let call = call.clone();
        let session_id = session.id.clone();
        let actor_id = actor_id.to_string();
        let tool_timeout_ms = tool.timeout_ms;
        let workspace = std::path::PathBuf::from(session.cwd.trim());
        let feedback = self
            .feedback_tx
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        // 路由只消费安装阶段保存的验证能力，不启动 sidecar 探测。
        let handler = self.handler_for_tool(&call.name);
        let invocation = crate::invocation::RuntimeInvocation::new(
            &plugin_id,
            call.clone(),
            &session_id,
            workspace.to_string_lossy().into_owned(),
            &actor_id,
            feedback.clone(),
        );
        Box::pin(crate::invocation::dispatch(
            invocation.clone(),
            async move {
                match handler {
                    Ok(crate::invocation::HandlerKind::Sidecar) => Some(
                        invoke_sidecar_tool(
                            &plugin_id,
                            call,
                            tool_timeout_ms,
                            Some(invocation.clone()),
                        )
                        .await,
                    ),
                    Ok(crate::invocation::HandlerKind::Ui) => Some(
                        crate::ts_tools::execute(
                            plugin_id,
                            call,
                            tool_timeout_ms,
                            Some(invocation.clone()),
                        )
                        .await,
                    ),
                    Err(error) => Some(sidecar_tool_failure(&plugin_id, error)),
                }
            },
        ))
    }

    fn result_header(&self, call: &ToolCall, ok: bool) -> Option<String> {
        Some(crate::core_bridge::plugin_result_header(
            &self.id, &call.name, ok,
        ))
    }
}

fn sidecar_direct_of(manifest: &PluginManifest) -> bool {
    manifest.ui_contributions().is_empty() && manifest.sidecar.is_some()
}

/// 无界面 sidecar 型插件的工具直达：宿主内置桥接，语义与 memory 等官方
/// 插件的 WASM 桥接层一致（透传）——operation 为工具名、参数为调用参数
/// 对象，sidecar 返回 ToolOutcome 形状（ok/summary/stdout/stderr/exit_code，
/// 后四项可缺省）。
///
/// Handler 调用本身不设时限；用户或会话取消时，进程守卫负责定向取消
/// 当前请求。启动、连接和写入仍保留基础故障保护。
async fn invoke_sidecar_tool(
    plugin_id: &str,
    call: ToolCall,
    _timeout_ms: u64,
    runtime_invocation: Option<crate::invocation::RuntimeInvocation>,
) -> ToolResult {
    let Some(directory) = crate::registry::plugin_install_directory(plugin_id) else {
        return sidecar_tool_failure(plugin_id, "插件未加载");
    };
    let Some(storage_root) = directory
        .parent()
        .and_then(|parent| parent.parent())
        .map(std::path::Path::to_path_buf)
    else {
        return sidecar_tool_failure(plugin_id, "无法定位插件存储根");
    };
    let installed = match crate::registry::find_installed_plugin(&storage_root, plugin_id) {
        Ok(installed) => installed,
        Err(error) => return sidecar_tool_failure(plugin_id, format!("{error:#}")),
    };
    // 按需形态：每次调用独立临时连接——进程完全归属本次调用，超时/取消
    // 只终止自己，并发调用互不可见；常驻形态：共享缓存连接（进程内多路
    // 复用），守卫取消当前共享进程（换代重启，常驻语义的单进程权衡）。
    let on_demand =
        installed.manifest.sidecar_lifecycle() == crate::manifest::SidecarLifecycle::OnDemand;
    let context = runtime_invocation
        .as_ref()
        .map(|invocation| invocation.context().clone());
    let session_id = context.as_ref().map(|context| context.session_id.clone());
    let authoritative_workspace = context
        .as_ref()
        .map(|context| std::path::PathBuf::from(&context.workspace))
        .unwrap_or_default();
    let session_workspace = if installed.manifest.should_preload_sidecar() {
        None
    } else {
        Some(authoritative_workspace.as_path())
    };
    let connection = if on_demand {
        crate::registry::ephemeral_sidecar_connection_with_workspace(
            &storage_root,
            &installed,
            session_workspace,
        )
    } else if session_workspace.is_some() {
        crate::registry::sidecar_connection_with_workspace(
            &storage_root,
            &installed,
            false,
            session_workspace,
        )
    } else {
        crate::registry::sidecar_connection(&storage_root, &installed, false)
    };
    let connection = match connection {
        Ok(connection) => connection,
        Err(error) => return sidecar_tool_failure(plugin_id, format!("{error:#}")),
    };
    if let Some(invocation) = &runtime_invocation {
        let connection = connection.clone();
        let session_id = invocation.context().session_id.clone();
        invocation.on_cancel(move || {
            let _ = connection.cancel_session(&session_id);
        });
    }
    // 进程守卫：调用完成（disarm）前，超时返回、panic 或会话取消（drop）
    // 都终止本次调用的进程——后台 spawn_blocking 里的调用随进程断开而失败，
    // 不再运行到 sidecar 总超时。
    let mut guard = SidecarProcessGuard {
        // 新统一路径的取消由 RuntimeInvocation 唯一触发；旧测试/兼容入口
        // 仍使用进程守卫，避免没有统一对象时遗留 sidecar。
        connection: runtime_invocation.is_none().then(|| connection.clone()),
        stop_on_drop: on_demand,
        session_id: session_id.clone(),
    };
    let operation = call.name.clone();
    let arguments = call.arguments.clone();
    let invocation_context = context;
    let blocking = connection.clone();
    let plugin_id_for_feedback = plugin_id.to_string();
    let invoked = tokio::task::spawn_blocking(move || {
        let payload = serde_json::to_string(&arguments).unwrap_or_default();
        let mut on_progress = |message: String| {
            if let Some(invocation) = &runtime_invocation {
                invocation.progress(message);
            } else {
                crate::bridge::handle_runtime_feedback(&plugin_id_for_feedback, &message);
            }
        };
        match invocation_context {
            Some(context) => blocking.invoke_with_invocation_context_and_progress(
                &operation,
                &payload,
                &context,
                &mut on_progress,
            ),
            None => blocking.invoke_with_progress(&operation, &payload, &mut on_progress),
        }
    })
    .await;
    let outcome = match invoked {
        Ok(Ok(raw)) => serde_json::from_str::<serde_json::Value>(&raw)
            .map_err(|error| anyhow::anyhow!("解析 sidecar 工具响应失败：{error}")),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(anyhow::anyhow!("sidecar 调用任务失败：{error}")),
    };
    // 完成判定在业务映射前：传输层 Ok 即完成。
    let outcome_is_complete = outcome.is_ok();
    let result = match outcome {
        Ok(value) => ToolResult {
            ok: value
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            summary: value
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            stdout: value
                .get("stdout")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            stderr: value
                .get("stderr")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            exit_code: value
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(
                    if value.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
                        1
                    } else {
                        0
                    },
                ) as i32,
            execution: None,
        },
        Err(error) => sidecar_tool_failure(plugin_id, format!("{error:#}")),
    };
    if outcome_is_complete {
        // 调用正常完成（含业务失败 ok:false——那是完整响应，不是传输故障））：
        // 解除守卫。仅超时/取消/panic 路径经 Drop 终止进程。
        guard.connection.take();
    }
    result
}

struct SidecarProcessGuard {
    connection: Option<std::sync::Arc<dyn crate::sidecar::SidecarConnection>>,
    /// 临时连接（按需直连）用 stop 终止；共享连接按会话取消目标请求。
    stop_on_drop: bool,
    session_id: Option<String>,
}

impl Drop for SidecarProcessGuard {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            if self.stop_on_drop {
                // 独立连接：终止即安全（连接与进程都只属于本次调用）。
                let _ = connection.stop();
            } else if let Some(session_id) = self.session_id.as_deref() {
                // 共享连接：只取消当前会话关联的请求，常驻进程继续服务其他调用。
                let _ = connection.cancel_session(session_id);
            }
        }
    }
}

/// 测试通道：经真实直连路径调用（安装与阻塞超时验证）。
#[cfg(test)]
pub(crate) async fn invoke_sidecar_tool_for_test(
    plugin_id: &str,
    call: ToolCall,
    timeout_ms: u64,
) -> ToolResult {
    invoke_sidecar_tool(plugin_id, call, timeout_ms, None).await
}

/// 测试通道：携带统一调用生命周期（含新版权威工作区与取消）的真实
/// 直连调用，验证按需沙箱写域随会话工作区构造。
#[cfg(test)]
pub(crate) async fn invoke_sidecar_tool_with_invocation_for_test(
    plugin_id: &str,
    call: ToolCall,
    invocation: crate::invocation::RuntimeInvocation,
) -> ToolResult {
    invoke_sidecar_tool(plugin_id, call, 0, Some(invocation)).await
}

fn sidecar_tool_failure(plugin_id: &str, message: impl std::fmt::Display) -> ToolResult {
    ToolResult {
        ok: false,
        summary: format!("插件 {plugin_id} sidecar 工具执行失败：{message}"),
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 1,
        execution: None,
    }
}

impl PromptSectionProvider for TsPluginAdapter {
    fn prompt_sections(&self) -> Vec<String> {
        if !self.is_enabled() {
            return Vec::new();
        }
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .prompts
            .clone()
    }
}

/// 按清单静态生成 @提及候选（未声明 mention 返回 None，查询词不命中返回 None）。
///
/// 供注册表实时聚合使用——TS 插件的候选是纯清单数据，不依赖适配器实例
///（适配器弱引用由会话 Core 构建时填充，安装后不存在）。
///
/// 静态候选同样必须按 query 过滤：不过滤会让输入任何字符都列出全部声明了
/// mention 的插件。判定与宿主兜底共用
/// {@link tiangong_types::mention::candidate_matches_query}，两层语义一致。
pub(crate) fn mention_candidate_from_manifest(
    manifest: &PluginManifest,
    descriptor_name: Option<&str>,
    query: &str,
) -> Option<tiangong_core::MentionCandidate> {
    let (label, hint, mark) = mention_candidate_parts(manifest, descriptor_name)?;
    let candidate = tiangong_core::MentionCandidate {
        value: format!("@plugin:{}", manifest.id),
        label,
        kind: "plugin".to_string(),
        hint,
        mark,
    };
    tiangong_types::mention::candidate_matches_query(&candidate, query).then_some(candidate)
}

/// 从清单推导 @提及候选的展示字段：hint 取 mention.hint（未声明 mention 则无
/// 候选），mark 取 mention.mark（可选，缺省空串由前端不显示标记）。
///
/// label 是插件名称，优先级：清单 `name` → 页面类贡献（`extension.tab` /
/// `settings.plugin-page`）标题 → WASM descriptor 名称 → 插件 id。
/// 不取任意首个贡献的标题：输入区按钮、消息操作等贡献的标题是动作名
///（如 computer-use 的「恢复窗口」），不是插件名。
fn mention_candidate_parts(
    manifest: &PluginManifest,
    descriptor_name: Option<&str>,
) -> Option<(String, String, String)> {
    let mention = manifest.mention.as_ref()?;
    let non_empty = |value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    };
    let page_title = || {
        manifest.ui.as_ref().and_then(|ui| {
            ui.contributions
                .iter()
                .filter(|c| matches!(c.slot.as_str(), "extension.tab" | "settings.plugin-page"))
                .find_map(|c| non_empty(&c.title))
        })
    };
    let label = manifest
        .name
        .as_deref()
        .and_then(non_empty)
        .or_else(page_title)
        .or_else(|| descriptor_name.and_then(non_empty))
        .unwrap_or_else(|| manifest.id.clone());
    let mark = mention.mark.clone().unwrap_or_default();
    Some((label, mention.hint.clone(), mark))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::MentionManifest;

    fn manifest_with_mention(hint: Option<&str>) -> PluginManifest {
        let mention = hint.map(|hint| {
            serde_json::from_str::<MentionManifest>(&format!(r#"{{"hint":"{hint}"}}"#)).unwrap()
        });
        PluginManifest {
            schema_version: 2,
            require_server: false,
            name: None,
            description: None,
            id: "demo".into(),
            version: "0.1.0".into(),
            wasm: None,
            sidecar: None,
            permissions: vec![],
            entrypoints: None,
            model_requirements: None,
            storage_access: false,
            capabilities: None,
            ui: Some(serde_json::from_str(
                r#"{"contributions":[{"slot":"extension.tab","id":"app","title":"演示插件","entry":"app/index.html"}]}"#,
            )
            .unwrap()),
            tools: None,
            prompt: None,
            resources: None,
            mention,
            tool_icons: None,
        }
    }

    #[test]
    fn mention候选_声明时生成_未声明为空() {
        // 静态候选只看清单：启停过滤由注册表查询句柄（MentionSource）负责。
        let manifest = manifest_with_mention(Some("问候能力"));
        // 空查询不过滤：刚唤出面板时枚举型候选照常返回。
        let candidate =
            mention_candidate_from_manifest(&manifest, None, "").expect("声明 mention 应有候选");
        assert_eq!(candidate.value, "@plugin:demo");
        assert_eq!(candidate.label, "演示插件");
        assert_eq!(candidate.kind, "plugin");
        assert_eq!(candidate.hint, "问候能力");
        // 未声明 mention：无候选
        assert!(mention_candidate_from_manifest(&manifest_with_mention(None), None, "").is_none());
    }
    #[test]
    fn mention候选_无ui标题时用插件id() {
        let mut manifest = manifest_with_mention(Some("能力"));
        manifest.ui = None;
        let candidate =
            mention_candidate_from_manifest(&manifest, None, "").expect("声明 mention 应有候选");
        assert_eq!(candidate.label, "demo");
    }
    #[test]
    fn mention候选_label取插件名而非动作贡献标题() {
        let mut manifest = manifest_with_mention(Some("能力"));
        // 只有动作类贡献（如 computer-use 的「恢复窗口」）：不当插件名
        manifest.ui = Some(
            serde_json::from_str(
                r#"{"contributions":[{"slot":"session.input-action","id":"restore","title":"恢复窗口","entry":"r.html"}]}"#,
            )
            .unwrap(),
        );
        let label = |m: &PluginManifest, d: Option<&str>| {
            mention_candidate_from_manifest(m, d, "").unwrap().label
        };
        assert_eq!(label(&manifest, Some("Computer Use")), "Computer Use");
        assert_eq!(label(&manifest, None), "demo");
        // 页面类贡献标题优先于 descriptor（设置页不在首位也能取到）
        manifest.ui = Some(
            serde_json::from_str(
                r#"{"contributions":[{"slot":"session.input-action","id":"a","title":"语音输入","entry":"a.html"},{"slot":"settings.plugin-page","id":"s","title":"火山引擎","entry":"s.html"}]}"#,
            )
            .unwrap(),
        );
        assert_eq!(label(&manifest, Some("Volcengine")), "火山引擎");
        // 清单 name 最优先
        manifest.name = Some("显式名称".into());
        assert_eq!(label(&manifest, Some("Volcengine")), "显式名称");
    }
    #[test]
    fn mention候选_按查询词过滤() {
        let manifest = manifest_with_mention(Some("问候能力"));
        // 命中 label（UI 标题）
        assert!(mention_candidate_from_manifest(&manifest, None, "演示").is_some());
        // 命中 hint
        assert!(mention_candidate_from_manifest(&manifest, None, "问候").is_some());
        // 命中 value 剥离 kind 前缀后的插件 id
        assert!(mention_candidate_from_manifest(&manifest, None, "demo").is_some());
        // kind 前缀本身不参与匹配：输入 plugin 不应命中 @plugin:demo
        assert!(mention_candidate_from_manifest(&manifest, None, "plugin").is_none());
        // 词间 AND：两个词分属 label 与 hint 时仍应命中
        assert!(mention_candidate_from_manifest(&manifest, None, "演示 问候").is_some());
        // 缺一个词即不命中
        assert!(mention_candidate_from_manifest(&manifest, None, "演示 不存在").is_none());
    }
    #[test]
    fn 无界面sidecar插件_工具走直连_有界面走页面() {
        // 无 UI + sidecar：直连
        let mut manifest = manifest_with_mention(None);
        manifest.ui = None;
        manifest.sidecar =
            Some(serde_json::from_str(r#"{"runtime":"node","entry":"sidecar/main.mjs"}"#).unwrap());
        let adapter = TsPluginAdapter::from_manifest(&manifest, true, None);
        assert!(
            adapter
                .sidecar_direct
                .load(std::sync::atomic::Ordering::Acquire),
            "无界面 sidecar 插件应直连"
        );
        // 有 UI：照旧页面路径
        let mut manifest = manifest_with_mention(None);
        manifest.sidecar =
            Some(serde_json::from_str(r#"{"runtime":"node","entry":"sidecar/main.mjs"}"#).unwrap());
        let adapter = TsPluginAdapter::from_manifest(&manifest, true, None);
        assert!(
            !adapter
                .sidecar_direct
                .load(std::sync::atomic::Ordering::Acquire),
            "有界面插件应保持页面接应路径"
        );
        // 无 UI 无 sidecar：也不直连（无接应由 ts_tools 明确报错）
        let mut manifest = manifest_with_mention(None);
        manifest.ui = None;
        let adapter = TsPluginAdapter::from_manifest(&manifest, true, None);
        assert!(
            !adapter
                .sidecar_direct
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn 路由消费验证能力_缺失时按结构回退() {
        let sidecar_manifest: serde_json::Value =
            serde_json::from_str(r#"{"runtime":"node","entry":"sidecar/main.mjs"}"#).unwrap();
        // 无 UI + sidecar + 无记录：仍走 sidecar 真实调用——运行异常以
        // 真实错误返回（安装后运行检查语义，不再以记录缺失拒绝执行）。
        let mut manifest = manifest_with_mention(None);
        manifest.ui = None;
        manifest.sidecar = Some(serde_json::from_value(sidecar_manifest.clone()).unwrap());
        let adapter = TsPluginAdapter::from_manifest(&manifest, true, None);
        assert_eq!(
            adapter_route(&adapter, "demo"),
            crate::invocation::HandlerKind::Sidecar
        );
        // 无 UI + sidecar + 有效记录：直连（能力列表可为空——结构即通道）。
        let adapter = TsPluginAdapter::from_manifest(&manifest, true, Some(Vec::new()));
        assert_eq!(
            adapter_route(&adapter, "demo"),
            crate::invocation::HandlerKind::Sidecar
        );
        // 有 UI + sidecar：已验证声明接管才走 sidecar。
        let mut manifest = manifest_with_mention(None);
        manifest.sidecar = Some(serde_json::from_value(sidecar_manifest).unwrap());
        let adapter =
            TsPluginAdapter::from_manifest(&manifest, true, Some(vec!["tool:demo".to_string()]));
        assert_eq!(
            adapter_route(&adapter, "demo"),
            crate::invocation::HandlerKind::Sidecar
        );
        assert_eq!(
            adapter_route(&adapter, "other"),
            crate::invocation::HandlerKind::Ui
        );
        // 记录缺失：有 UI 回退 UI。
        let adapter = TsPluginAdapter::from_manifest(&manifest, true, None);
        assert_eq!(
            adapter_route(&adapter, "demo"),
            crate::invocation::HandlerKind::Ui
        );
        // 旧版有 UI 的直连插件：空能力仍可使用已验证的旧声明；检查缺失
        // 或新版能力明确不支持时返回错误，不能交给纯展示页面无限等待。
        manifest.sidecar.as_mut().unwrap().tools_direct = Some(true);
        let adapter = TsPluginAdapter::from_manifest(&manifest, true, Some(Vec::new()));
        assert_eq!(
            adapter_route(&adapter, "demo"),
            crate::invocation::HandlerKind::Sidecar
        );
        adapter.reconfigure(&manifest, true, None);
        assert!(adapter.handler_for_tool("demo").is_err());
        adapter.reconfigure(&manifest, true, Some(vec!["tool:other".into()]));
        assert!(adapter.handler_for_tool("demo").is_err());
        assert_eq!(
            adapter_route(&adapter, "other"),
            crate::invocation::HandlerKind::Sidecar
        );
        manifest.sidecar.as_mut().unwrap().tools_direct = Some(false);
        adapter.reconfigure(&manifest, true, Some(Vec::new()));
        assert_eq!(
            adapter_route(&adapter, "demo"),
            crate::invocation::HandlerKind::Ui
        );
    }

    fn adapter_route(adapter: &TsPluginAdapter, tool_name: &str) -> crate::invocation::HandlerKind {
        adapter.handler_for_tool(tool_name).unwrap()
    }

    #[test]
    fn runtime反馈只识别受控app原语() {
        assert!(!crate::bridge::handle_runtime_feedback("demo", "普通进度"));
        assert!(!crate::bridge::handle_runtime_feedback(
            "demo",
            r#"{"type":"progress","message":"50%"}"#,
        ));
        assert!(crate::bridge::handle_runtime_feedback(
            "demo",
            r#"{"type":"app.open","payload":{"instance_id":"a"}}"#,
        ));
        assert!(crate::bridge::handle_runtime_feedback(
            "demo",
            r#"{"host_action":{"method":"app.close","payload":{"instance_id":"a"}}}"#,
        ));
    }
}
