//! MCP sidecar 业务服务：承载配置、capability 探测与工具执行，按操作名分发请求。
//!
//! 整合原 plugin.rs 的状态管理与 management.rs 的管理 CRUD，全部经 IPC 操作
//! 暴露给运行时（host 侧 invoke_sidecar）与 WASM 桥接。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;

use anyhow::{Context, Result, anyhow, bail};

use tiangong_plugin_mcp_protocol::config::{
    McpConfig, McpServerConfig, RegisterMcpServerRequest, UpdateConfigEntryRequest,
};
use tiangong_plugin_mcp_protocol::management::{
    CONFIG_GET_OPERATION, CONFIG_SNAPSHOT_OPERATION, CONFIG_UPDATE_ENTRY_OPERATION,
    RemoveServerRequest, SERVER_MERGE_DISK_OPERATION, SERVER_REGISTER_OPERATION,
    SERVER_REMOVE_OPERATION, SERVER_SET_ENABLED_OPERATION, SERVER_UPDATE_OPERATION,
    ServersResponse, SetEnabledRequest, UpdateServerRequest,
};
use tiangong_plugin_mcp_protocol::tool::{ExecuteToolResponse, ListToolsResponse};
use tiangong_plugin_mcp_protocol::{
    MCP_MENTION_CANDIDATES_OPERATION, MCP_PROTOCOL_VERSION, MessageResponse, PLUGIN_ID,
    PLUGIN_VERSION,
};
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};

use crate::capability::McpCapabilityIndex;
use crate::execution::{McpFunctionTarget, build_targets, execute_tool, list_tools_response};
use crate::paths::{default_mcp_capability_cache_path, default_mcp_config_path};
use crate::validate::{describe_mcp_servers, summarize_mcp_servers, validate_mcp_config};

/// MCP capability 后台刷新间隔（秒）。
const MCP_CAPABILITY_REFRESH_INTERVAL_SECS: u64 = 300;

/// MCP sidecar 业务服务。
pub struct McpService {
    mcp_config: RwLock<McpConfig>,
    mcp_targets: RwLock<std::collections::HashMap<String, McpFunctionTarget>>,
    capability: McpCapabilityIndex,
    capability_cache_path: PathBuf,
    mcp_config_path: PathBuf,
    /// 启动时配置文件加载失败的原因；存在期间拒绝任何写回，防止默认空配置覆盖磁盘上的真实配置。
    config_load_error: RwLock<Option<String>>,
    /// 旧配置迁移失败的原因；存在期间拒绝写回，防止空配置抢先落盘后旧配置被永久跳过。
    legacy_migration_error: Option<String>,
    /// 当前会话工作目录（由 reconfigure 注入，stdio MCP 子进程用）。
    workspace: RwLock<Option<PathBuf>>,
}

impl McpService {
    /// 用默认存储路径构造。
    pub fn new() -> Result<Self> {
        // 迁移失败不阻断启动，但禁止写回：否则新目录里的空配置会先落盘，
        // 下次迁移见到同名文件就跳过，旧配置被永久忽略。未写完成标记，
        // 下次启动会重试。
        let migration_error = crate::paths::migrate_legacy().err().map(|error| {
            tracing::warn!("MCP 旧数据迁移失败，将在下次启动重试: {error:#}");
            format!("旧配置迁移失败：{error:#}")
        });
        let service = Self::with_paths(
            default_mcp_config_path(),
            default_mcp_capability_cache_path(),
        )?;
        Ok(Self {
            legacy_migration_error: migration_error,
            ..service
        })
    }

    pub fn with_paths(mcp_config_path: PathBuf, capability_cache_path: PathBuf) -> Result<Self> {
        let (mcp_config, config_load_error) = load_mcp_config_from_path(&mcp_config_path);
        let capability = McpCapabilityIndex::new();
        let _ = capability.load_cache(&capability_cache_path);
        Ok(Self {
            mcp_config: RwLock::new(mcp_config),
            mcp_targets: RwLock::new(std::collections::HashMap::new()),
            capability,
            capability_cache_path,
            mcp_config_path,
            config_load_error: RwLock::new(config_load_error),
            legacy_migration_error: None,
            workspace: RwLock::new(None),
        })
    }

    fn config_snapshot(&self) -> McpConfig {
        self.mcp_config
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn target_for_function(&self, function_name: &str) -> Option<McpFunctionTarget> {
        self.mcp_targets
            .read()
            .ok()
            .and_then(|guard| guard.get(function_name).cloned())
    }

    fn reconfigure(&self) {
        let config = self.config_snapshot();
        self.capability.configure_scheduler(
            config.clone(),
            self.capability_cache_path.clone(),
            MCP_CAPABILITY_REFRESH_INTERVAL_SECS,
        );
        self.capability.refresh_async(config.clone());
        self.rebuild_targets(&config);
    }

    /// 启动即启动探测调度并重建绑定（工具直连形态下 sidecar 首次被访问
    /// 前就可能需要工具声明；原 WASM 时代由宿主 config_updated 钩子触发）。
    pub fn prime(&self) {
        self.reconfigure();
    }

    fn sync_probe_and_rebuild(&self, server_name: &str) {
        let config = self.config_snapshot();
        if let Some(server) = config.servers.iter().find(|s| s.name == server_name)
            && server.enabled
        {
            let outcome = self.capability.probe_single(server, config.timeout_ms);
            if outcome.healthy {
                tracing::info!(
                    "MCP server 同步探测成功：server={} tools={}",
                    server_name,
                    outcome.tool_count
                );
            } else if let Some(err) = outcome.last_error {
                tracing::warn!(
                    "MCP server 同步探测失败（后台调度器会重试）：server={} error={}",
                    server_name,
                    err
                );
            }
        }
        self.rebuild_targets(&config);
    }

    fn rebuild_targets(&self, config: &McpConfig) {
        let active = self.capability.cached_active_tools();
        let targets = build_targets(config, active);
        if let Ok(mut guard) = self.mcp_targets.write() {
            *guard = targets;
        }
        self.notify_tools_updated();
    }

    /// 工具声明可能已变化（配置增删、探测结果更新）：通知宿主经
    /// `tool_specs` 重取快照，下一轮工具装配生效。
    fn notify_tools_updated(&self) {
        tiangong_plugin_sidecar::server::emit_notification(
            tiangong_plugin_sidecar::protocol::TOOLS_UPDATED_CHANNEL,
            serde_json::json!({ "plugin_id": PLUGIN_ID }).to_string(),
        );
    }

    /// 按 operation 分发运行时请求，返回通用 Response。
    pub async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "MCP 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }
        match self
            .dispatch_operation(&request.operation, request.payload)
            .await
        {
            Ok(payload) => Response::success(&request_id, payload),
            Err(error) => Response::error(
                &request_id,
                ErrorCode::ServiceError,
                error.to_string(),
                false,
            ),
        }
    }

    async fn dispatch_operation(
        &self,
        operation: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match operation {
            HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
                plugin_id: PLUGIN_ID.to_string(),
                plugin_version: PLUGIN_VERSION.to_string(),
                sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION.to_string(),
                business_protocol: MCP_PROTOCOL_VERSION,
                capabilities: vec![
                    "mcp".to_string(),
                    // 工具承接通配：操作名即工具名（mcp__{server}__{tool}），
                    // 名单运行时确定；宿主装载时据此经固定操作 tool_specs
                    // 拉取规格快照。
                    "tool:*".to_string(),
                ],
                instance_id: format!("mcp-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 MCP 握手响应失败"),
            CONFIG_GET_OPERATION | CONFIG_SNAPSHOT_OPERATION => {
                serde_json::to_value(self.config_snapshot()).with_context(|| "序列化 MCP 配置失败")
            }
            CONFIG_UPDATE_ENTRY_OPERATION => {
                let req: UpdateConfigEntryRequest = serde_json::from_value(payload)?;
                let message = self.update_config_entry(&req.key, &req.value)?;
                Ok(serde_json::to_value(MessageResponse { message })?)
            }
            SERVER_REGISTER_OPERATION => {
                let req: RegisterMcpServerRequest = serde_json::from_value(payload)?;
                let message = self.register_server(req)?;
                Ok(serde_json::to_value(MessageResponse { message })?)
            }
            SERVER_UPDATE_OPERATION => {
                let req: UpdateServerRequest = serde_json::from_value(payload)?;
                let message = self.update_server(&req.name, req.request)?;
                Ok(serde_json::to_value(MessageResponse { message })?)
            }
            SERVER_REMOVE_OPERATION => {
                let req: RemoveServerRequest = serde_json::from_value(payload)?;
                let message = self.remove_server(&req.name)?;
                Ok(serde_json::to_value(MessageResponse { message })?)
            }
            SERVER_SET_ENABLED_OPERATION => {
                let req: SetEnabledRequest = serde_json::from_value(payload)?;
                let message = self.set_enabled(&req.name, req.enabled)?;
                Ok(serde_json::to_value(MessageResponse { message })?)
            }
            SERVER_MERGE_DISK_OPERATION => {
                self.merge_with_disk()?;
                Ok(serde_json::to_value(MessageResponse {
                    message: "已合并磁盘外部新增的 server".to_string(),
                })?)
            }
            tiangong_plugin_mcp_protocol::query::SERVER_LIST_OPERATION => {
                let servers = self.config_snapshot().servers;
                Ok(serde_json::to_value(ServersResponse { servers })?)
            }
            tiangong_plugin_mcp_protocol::query::SERVER_CACHED_TOOLS_OPERATION => {
                use tiangong_plugin_mcp_protocol::query::{ServerNameRequest, ToolsResponse};
                let req: ServerNameRequest = serde_json::from_value(payload)?;
                let tools = self
                    .capability
                    .cached_server_tools(&req.name)
                    .unwrap_or_default();
                Ok(serde_json::to_value(ToolsResponse { tools })?)
            }
            tiangong_plugin_mcp_protocol::query::SERVER_HEALTH_OPERATION => {
                use tiangong_plugin_mcp_protocol::query::HealthResponse;
                let statuses = self.capability.health_statuses();
                Ok(serde_json::to_value(HealthResponse { statuses })?)
            }
            tiangong_plugin_mcp_protocol::query::SERVER_SUMMARY_OPERATION => {
                use tiangong_plugin_mcp_protocol::NameFilterRequest;
                use tiangong_plugin_mcp_protocol::query::TextResponse;
                let req: NameFilterRequest = serde_json::from_value(payload)?;
                let text =
                    summarize_mcp_servers(&self.config_snapshot().servers, req.filter.as_deref());
                Ok(serde_json::to_value(TextResponse { text })?)
            }
            tiangong_plugin_mcp_protocol::query::SERVER_DETAIL_OPERATION => {
                use tiangong_plugin_mcp_protocol::NameFilterRequest;
                use tiangong_plugin_mcp_protocol::query::TextResponse;
                let req: NameFilterRequest = serde_json::from_value(payload)?;
                let text =
                    describe_mcp_servers(&self.config_snapshot().servers, req.filter.as_deref());
                Ok(serde_json::to_value(TextResponse { text })?)
            }
            tiangong_plugin_mcp_protocol::capability::SERVER_PROBE_OPERATION => {
                use tiangong_plugin_mcp_protocol::query::ServerNameRequest;
                let req: ServerNameRequest = serde_json::from_value(payload)?;
                self.probe_server(&req.name)?;
                Ok(serde_json::to_value(
                    tiangong_plugin_mcp_protocol::Empty {},
                )?)
            }
            tiangong_plugin_mcp_protocol::capability::RECONFIGURE_OPERATION => {
                let req: tiangong_plugin_mcp_protocol::capability::ReconfigureRequest =
                    serde_json::from_value(payload)?;
                if let Some(ws) = req.workspace
                    && let Ok(mut guard) = self.workspace.write()
                {
                    *guard = Some(PathBuf::from(ws));
                }
                self.reconfigure();
                Ok(serde_json::to_value(
                    tiangong_plugin_mcp_protocol::Empty {},
                )?)
            }
            tiangong_plugin_mcp_protocol::tool::LIST_TOOLS_OPERATION => {
                let config = self.config_snapshot();
                let active = self.capability.cached_active_tools();
                let response: ListToolsResponse = list_tools_response(&config, active);
                Ok(serde_json::to_value(response)?)
            }
            tiangong_plugin_mcp_protocol::tool::EXECUTE_TOOL_OPERATION => {
                use tiangong_plugin_mcp_protocol::tool::ExecuteToolRequest;
                let req: ExecuteToolRequest = serde_json::from_value(payload)?;
                let config = self.config_snapshot();
                let function_name = format!("mcp__{}__{}", req.server_name, req.tool_name);
                // WASM 为满足模型函数名限制会把连字符等字符转成下划线；
                // 执行时必须用 capability 建立的绑定还原真实 server/tool 名。
                let target =
                    self.target_for_function(&function_name)
                        .unwrap_or(McpFunctionTarget {
                            server_name: req.server_name,
                            tool_name: req.tool_name,
                        });
                let workspace = req
                    .workspace
                    .clone()
                    .map(PathBuf::from)
                    .or_else(|| self.workspace.read().ok().and_then(|guard| guard.clone()));
                let result: ExecuteToolResponse =
                    execute_tool(&target, req.arguments, &config, workspace)
                        .await
                        .unwrap_or_else(|err| ExecuteToolResponse {
                            ok: false,
                            summary: format!("MCP工具调用失败：{err}"),
                            stderr: err.to_string(),
                            exit_code: 1,
                            ..Default::default()
                        });
                if result.ok {
                    self.capability.record_success(&target.server_name);
                }
                Ok(serde_json::to_value(result)?)
            }
            tiangong_plugin_mcp_protocol::env::ENV_COLLECT_OPERATION => {
                let env: BTreeMap<String, String> = self.collect_runtime_env();
                Ok(serde_json::to_value(env)?)
            }

            // ── 工具直连形态（原 WASM 桥接职责迁入）──

            // 动态工具规格快照：宿主装载 / runtime.tools_updated 通知重取。
            tiangong_plugin_sidecar::protocol::TOOL_SPECS_OPERATION => {
                let config = self.config_snapshot();
                let active = self.capability.cached_active_tools();
                let response: ListToolsResponse = list_tools_response(&config, active);
                let specs: Vec<serde_json::Value> = response
                    .servers
                    .iter()
                    .flat_map(|entry| {
                        entry
                            .tools
                            .iter()
                            .map(move |tool| (entry.server.clone(), tool))
                    })
                    .map(|(server, tool)| {
                        serde_json::json!({
                            "name": crate::execution::resolve_mcp_function_name(&server, &tool.name),
                            "description": format!(
                                "MCP调用：server={} tool={} description={}",
                                server, tool.name, tool.description
                            ),
                            "input_schema": if tool.input_schema.is_object() {
                                tool.input_schema.clone()
                            } else {
                                serde_json::json!({})
                            },
                            "timeout_ms": config.timeout_ms,
                        })
                    })
                    .collect();
                Ok(serde_json::to_value(specs)?)
            }

            // 工作目录变化（原 WASM set_workspace + on_config_updated →
            // Reconfigure 语义迁入）：更新工作目录并重新探测。
            "lifecycle.set_workspace" => {
                let workspace = payload
                    .get("workspace")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from);
                if let Ok(mut guard) = self.workspace.write() {
                    *guard = workspace;
                }
                self.reconfigure();
                Ok(serde_json::to_value(
                    tiangong_plugin_mcp_protocol::Empty {},
                )?)
            }

            // @提及候选（原 WASM mcp_mention_candidates 迁入，v2 形状）：
            // 入口 entry + 启用中的 server 动态候选。
            MCP_MENTION_CANDIDATES_OPERATION => {
                let config = self.config_snapshot();
                let tool_counts: std::collections::HashMap<String, usize> =
                    list_tools_response(&config, self.capability.cached_active_tools())
                        .servers
                        .into_iter()
                        .map(|entry| (entry.server, entry.tools.len()))
                        .collect();
                let candidates: Vec<serde_json::Value> = config
                    .servers
                    .iter()
                    .filter(|server| server.enabled)
                    .map(|server| {
                        serde_json::json!({
                            "value": format!("@mcp:{}", server.name),
                            "label": server.name,
                            "kind": "mcp",
                            "hint": format!(
                                "{} 工具",
                                tool_counts.get(&server.name).copied().unwrap_or(0)
                            ),
                            "mark": "M",
                        })
                    })
                    .collect();
                Ok(serde_json::json!({
                    "entry": {
                        "hint": "调用外部 MCP server 的工具",
                        "mark": "M",
                    },
                    "candidates": candidates,
                }))
            }

            // 工具执行直连：operation 即函数名 mcp__{server}__{tool}（原
            // WASM handle_tool 迁入——解析名转发执行）。
            name if name.starts_with("mcp__") => {
                let arguments = payload;
                let target = self.target_for_function(name);
                let config = self.config_snapshot();
                // workspace 优先宿主权威调用上下文，回退 reconfigure 注入值。
                let workspace = tiangong_plugin_sidecar::invocation_context()
                    .map(|context| context.workspace)
                    .filter(|workspace| !workspace.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| self.workspace.read().ok().and_then(|guard| guard.clone()));
                let result: ExecuteToolResponse = match target.as_ref() {
                    Some(target) => execute_tool(target, arguments, &config, workspace)
                        .await
                        .unwrap_or_else(|err| ExecuteToolResponse {
                            ok: false,
                            summary: format!("MCP工具调用失败：{err}"),
                            stderr: err.to_string(),
                            exit_code: 1,
                            ..Default::default()
                        }),
                    None => ExecuteToolResponse {
                        ok: false,
                        summary: format!("不是 MCP 工具：{name}"),
                        stderr: format!("不是 MCP 工具：{name}"),
                        exit_code: 1,
                        ..Default::default()
                    },
                };
                if result.ok
                    && let Some(target) = target.as_ref()
                {
                    self.capability.record_success(&target.server_name);
                }
                Ok(serde_json::to_value(result)?)
            }

            other => bail!("不支持的 MCP 操作: {other}"),
        }
    }

    fn apply_config(&self, next: McpConfig, affected_server: Option<&str>) -> Result<()> {
        if let Some(error) = &self.legacy_migration_error {
            return Err(anyhow!(
                "MCP 旧配置迁移未完成，拒绝写回以防旧配置被覆盖：{}（{error}）",
                self.mcp_config_path.display()
            ));
        }
        // 启动时配置加载失败（文件损坏/不可读）：内存里是兜底默认配置，一旦
        // 写回会把磁盘上的真实配置整体抹掉，必须拒绝。只有明确确认文件已
        // 不在磁盘上（NotFound，用户删除后重建）才放行；状态无法确认时同样
        // 保守拒绝——`exists()` 在权限不足时也会返回 false，不能作放行依据。
        if let Some(error) = self
            .config_load_error
            .read()
            .ok()
            .and_then(|guard| guard.clone())
        {
            match std::fs::metadata(&self.mcp_config_path) {
                Ok(_) => {
                    return Err(anyhow!(
                        "MCP 配置文件加载失败，拒绝写回以防覆盖现有配置：{}（{error}）",
                        self.mcp_config_path.display()
                    ));
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(anyhow!(
                        "MCP 配置文件状态无法确认，拒绝写回以防覆盖现有配置：{}（{error}；{err}）",
                        self.mcp_config_path.display()
                    ));
                }
            }
        }
        write_mcp_config_to_path(&self.mcp_config_path, &next)?;
        if let Ok(mut guard) = self.mcp_config.write() {
            *guard = next.clone();
        } else {
            return Err(anyhow!("MCP 配置锁中毒"));
        }
        // 写回内容来自内存快照序列化，必然可解析，清除加载失败状态。
        if let Ok(mut guard) = self.config_load_error.write() {
            *guard = None;
        }
        self.capability.configure_scheduler(
            next.clone(),
            self.capability_cache_path.clone(),
            MCP_CAPABILITY_REFRESH_INTERVAL_SECS,
        );
        match affected_server {
            Some(name) => self.sync_probe_and_rebuild(name),
            None => self.rebuild_targets(&next),
        }
        Ok(())
    }

    fn register_server(&self, request: RegisterMcpServerRequest) -> Result<String> {
        let name = request.name.trim().to_string();
        if name.is_empty() {
            return Err(anyhow!("MCP server 名称不能为空"));
        }
        let fields = normalize_request_fields(request)?;
        if fields.command.is_empty() && fields.endpoint.is_empty() && fields.tags.is_empty() {
            return Err(anyhow!("MCP server 需至少配置 command、endpoint 或 tags"));
        }
        let mut next = self.config_snapshot();
        if next.servers.iter().any(|s| s.name == name) {
            return Err(anyhow!("MCP server 已存在：{name}"));
        }
        next.servers.push(McpServerConfig {
            name: name.clone(),
            transport: fields.transport,
            command: fields.command,
            args: fields.args,
            endpoint: fields.endpoint,
            auth_header: fields.auth_header,
            headers: fields.headers,
            env: fields.env,
            enabled: fields.enabled,
            tags: fields.tags,
        });
        validate_mcp_config(&next)?;
        self.apply_config(next, Some(&name))?;
        Ok(format!("MCP server 已注册：{name}"))
    }

    fn update_server(&self, name: &str, request: RegisterMcpServerRequest) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("MCP server 名称不能为空"));
        }
        let fields = normalize_request_fields(request)?;
        if fields.command.is_empty() && fields.endpoint.is_empty() && fields.tags.is_empty() {
            return Err(anyhow!("MCP server 需至少配置 command、endpoint 或 tags"));
        }
        let mut next = self.config_snapshot();
        let server = next
            .servers
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow!("未找到 MCP server：{name}"))?;
        server.transport = fields.transport;
        server.command = fields.command;
        server.args = fields.args;
        server.endpoint = fields.endpoint;
        server.auth_header = fields.auth_header;
        server.headers = fields.headers;
        server.env = fields.env;
        server.tags = fields.tags;
        validate_mcp_config(&next)?;
        self.apply_config(next, Some(name))?;
        Ok(format!("MCP server 已更新：{name}"))
    }

    fn remove_server(&self, name: &str) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("MCP server 名称不能为空"));
        }
        let mut next = self.config_snapshot();
        let before = next.servers.len();
        next.servers.retain(|s| s.name != name);
        if next.servers.len() == before {
            return Err(anyhow!("未找到 MCP server：{name}"));
        }
        validate_mcp_config(&next)?;
        self.apply_config(next, None)?;
        Ok(format!("MCP server 已删除：{name}"))
    }

    fn set_enabled(&self, name: &str, enabled: bool) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("MCP server 名称不能为空"));
        }
        let mut next = self.config_snapshot();
        let server = next
            .servers
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow!("未找到 MCP server：{name}"))?;
        server.enabled = enabled;
        validate_mcp_config(&next)?;
        self.apply_config(next, Some(name))?;
        Ok(format!("MCP server 状态已更新：{name} enabled={enabled}"))
    }

    fn update_config_entry(&self, key: &str, value: &str) -> Result<String> {
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            return Err(anyhow!("配置键不能为空"));
        }
        let mut next = self.config_snapshot();
        let updated = match key {
            "mcp.enabled" => {
                let parsed = parse_bool(value)?;
                next.enabled = parsed;
                parsed.to_string()
            }
            "mcp.timeout_ms" => {
                let parsed: u64 = value
                    .parse()
                    .with_context(|| format!("配置值无效，要求正整数：{value}"))?;
                if parsed == 0 {
                    return Err(anyhow!("mcp.timeout_ms 必须大于 0"));
                }
                next.timeout_ms = parsed;
                parsed.to_string()
            }
            _ => {
                return Err(anyhow!(
                    "不支持的配置键：{key}。支持：mcp.enabled、mcp.timeout_ms"
                ));
            }
        };
        validate_mcp_config(&next)?;
        self.apply_config(next, None)?;
        Ok(format!("配置已更新：{key}={updated}"))
    }

    fn probe_server(&self, name: &str) -> Result<()> {
        self.capability.probe_single_by_name(name)?;
        let config = self.config_snapshot();
        self.rebuild_targets(&config);
        Ok(())
    }

    fn merge_with_disk(&self) -> Result<()> {
        let disk_content = match std::fs::read_to_string(&self.mcp_config_path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(anyhow!(
                    "读取 mcp 配置失败：{}（{err}）",
                    self.mcp_config_path.display()
                ));
            }
        };
        let disk_mcp: McpConfig = serde_json::from_str(&disk_content)
            .with_context(|| format!("解析 mcp 配置失败：{}", self.mcp_config_path.display()))?;
        let mut next = self.config_snapshot();
        let memory_names: Vec<String> = next.servers.iter().map(|s| s.name.clone()).collect();
        let to_add: Vec<McpServerConfig> = disk_mcp
            .servers
            .into_iter()
            .filter(|s| !memory_names.contains(&s.name))
            .collect();
        if to_add.is_empty() {
            return Ok(());
        }
        next.servers.extend(to_add);
        validate_mcp_config(&next)?;
        self.apply_config(next, None)
    }

    /// 收集启用 MCP server 的环境变量（exec_env 回传）。
    pub fn collect_runtime_env(&self) -> BTreeMap<String, String> {
        let mut runtime_env = BTreeMap::new();
        let config = self.config_snapshot();
        if config.enabled {
            for server in &config.servers {
                if !server.enabled {
                    continue;
                }
                for (key, value) in &server.env {
                    let key = key.trim();
                    if !key.is_empty() {
                        runtime_env.insert(key.to_string(), value.trim().to_string());
                    }
                }
            }
        }
        runtime_env
    }
}

/// 从指定路径加载 MCP 配置；返回（配置, 加载失败原因）。
///
/// 只有明确的 NotFound 才是首装常态（返回默认配置且无错误）；读取或解析
/// 失败（含权限不足等一切非 NotFound 错误）时同样返回默认配置供只读展示，
/// 但记录错误原因——`apply_config` 依赖它拒绝写回，避免默认空配置覆盖
/// 磁盘上的真实配置。不能用 `exists()` 预判：无权限 stat 的路径同样返回
/// false，会被误判成首装。
fn load_mcp_config_from_path(path: &std::path::Path) -> (McpConfig, Option<String>) {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return (McpConfig::default(), None);
        }
        Err(err) => {
            let reason = format!("读取失败：{err}");
            tracing::warn!(
                "MCP 配置读取失败，回退为默认配置（写回已禁止）：path={} error={err}",
                path.display()
            );
            return (McpConfig::default(), Some(reason));
        }
    };
    match serde_json::from_str::<McpConfig>(&content) {
        Ok(config) => (config, None),
        Err(err) => {
            let reason = format!("解析失败：{err}");
            tracing::warn!(
                "MCP 配置解析失败，回退为默认配置（写回已禁止）：path={} error={err}",
                path.display()
            );
            (McpConfig::default(), Some(reason))
        }
    }
}

fn write_mcp_config_to_path(path: &std::path::Path, config: &McpConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 MCP 配置目录失败：{}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(config).context("序列化 mcp 配置失败")?;
    std::fs::write(path, content)
        .with_context(|| format!("写入 mcp 配置失败：{}", path.display()))?;
    Ok(())
}

fn parse_bool(raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("布尔值无效：{raw}（可用 true/false）")),
    }
}

/// 规范化注册/编辑请求字段。
fn normalize_request_fields(request: RegisterMcpServerRequest) -> Result<NormalizedMcpFields> {
    let tags = request
        .tags
        .into_iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();
    let args = request
        .args
        .into_iter()
        .map(|arg| arg.trim().to_string())
        .filter(|arg| !arg.is_empty())
        .collect::<Vec<_>>();
    let transport = request.options.transport.unwrap_or_default();
    let endpoint = request
        .options
        .endpoint
        .unwrap_or_default()
        .trim()
        .to_string();
    let auth_header = request
        .options
        .auth_header
        .unwrap_or_default()
        .trim()
        .to_string();
    let headers = request
        .options
        .headers
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if key.is_empty() || value.is_empty() {
                None
            } else {
                Some((key, value))
            }
        })
        .collect::<BTreeMap<_, _>>();
    let env = request
        .options
        .env
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if key.is_empty() || value.is_empty() {
                None
            } else {
                Some((key, value))
            }
        })
        .collect::<BTreeMap<_, _>>();
    Ok(NormalizedMcpFields {
        transport,
        command: request.command.trim().to_string(),
        args,
        endpoint,
        auth_header,
        headers,
        env,
        enabled: request.enabled,
        tags,
    })
}

struct NormalizedMcpFields {
    transport: tiangong_plugin_mcp_protocol::config::McpTransportMode,
    command: String,
    args: Vec<String>,
    endpoint: String,
    auth_header: String,
    headers: BTreeMap<String, String>,
    env: BTreeMap<String, String>,
    enabled: bool,
    tags: Vec<String>,
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for McpService {
    async fn dispatch(
        &self,
        request: tiangong_plugin_sidecar::protocol::Request,
    ) -> tiangong_plugin_sidecar::protocol::Response {
        McpService::dispatch(self, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register_request(name: &str) -> RegisterMcpServerRequest {
        RegisterMcpServerRequest {
            name: name.to_string(),
            command: "echo".to_string(),
            args: vec![],
            tags: vec![],
            enabled: true,
            options: Default::default(),
        }
    }

    #[test]
    fn register_rejected_when_config_file_corrupted() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("mcp.json");
        std::fs::write(&config_path, "{ 损坏的配置").unwrap();
        let service =
            McpService::with_paths(config_path.clone(), dir.path().join("cache.json")).unwrap();
        let error = service
            .register_server(register_request("new-server"))
            .unwrap_err();
        assert!(
            error.to_string().contains("拒绝写回"),
            "损坏配置下注册应拒绝写回，实际：{error}"
        );
        // 磁盘文件保持原样，未被默认空配置覆盖。
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "{ 损坏的配置"
        );
    }

    #[test]
    #[cfg(unix)]
    fn register_rejected_when_config_file_inaccessible() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("mcp.json");
        let existing = serde_json::to_string_pretty(&McpConfig::default()).unwrap();
        std::fs::write(&config_path, &existing).unwrap();

        // 目录去掉执行权限后 stat 会失败，exists() 会把"无法访问"误报成
        // "不存在"；加载必须按读取失败记录并保持写回保护，而不是当作首装。
        let original = std::fs::metadata(dir.path()).unwrap().permissions();
        let mut restricted = original.clone();
        restricted.set_mode(0o600);
        std::fs::set_permissions(dir.path(), restricted).unwrap();
        let service = McpService::with_paths(config_path.clone(), dir.path().join("cache.json"));
        // 先恢复权限再断言，保证 tempdir 清理不受断言失败影响。
        std::fs::set_permissions(dir.path(), original).unwrap();

        let service = service.unwrap();
        let error = service
            .register_server(register_request("new-server"))
            .unwrap_err();
        assert!(
            error.to_string().contains("拒绝写回"),
            "目录不可访问时注册应拒绝写回，实际：{error}"
        );
        // 原文件仍在且未被默认空配置覆盖。
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), existing);
    }

    #[test]
    fn register_rejected_when_legacy_migration_failed() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("mcp.json");
        let service = McpService {
            legacy_migration_error: Some("旧配置迁移失败：模拟".to_string()),
            ..McpService::with_paths(config_path.clone(), dir.path().join("cache.json")).unwrap()
        };
        let error = service
            .register_server(register_request("new-server"))
            .unwrap_err();
        assert!(
            error.to_string().contains("迁移未完成"),
            "迁移失败时注册应拒绝写回，实际：{error}"
        );
        // 空配置未抢先落盘，下次启动迁移仍可复制旧配置。
        assert!(!config_path.exists());
    }

    #[test]
    fn register_succeeds_when_config_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let service =
            McpService::with_paths(dir.path().join("mcp.json"), dir.path().join("cache.json"))
                .unwrap();
        let message = service
            .register_server(register_request("first-server"))
            .unwrap();
        assert!(message.contains("first-server"));
    }

    /// 预写一份「demo server 已探测出两个工具」的 capability 缓存与启用配置。
    fn service_with_cached_tools() -> McpService {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("mcp.json");
        std::fs::write(
            &config_path,
            serde_json::to_string(&serde_json::json!({
                "enabled": true,
                "servers": [{
                    "name": "demo", "command": "echo", "enabled": true,
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cache.json"),
            serde_json::to_string(&serde_json::json!({
                "servers": {
                    "demo": {
                        "healthy": true,
                        "tools": [
                            {"name": "query", "description": "查询数据", "input_schema": {"type": "object"}},
                            {"name": "list-all", "description": "列出全部"},
                        ],
                    },
                },
            }))
            .unwrap(),
        )
        .unwrap();
        McpService::with_paths(config_path, dir.path().join("cache.json")).unwrap()
    }

    #[tokio::test]
    async fn tool_specs_平铺缓存工具为直连规格() {
        let service = service_with_cached_tools();
        let value = service
            .dispatch_operation(
                tiangong_plugin_sidecar::protocol::TOOL_SPECS_OPERATION,
                serde_json::json!({}),
            )
            .await
            .unwrap();
        let specs = value.as_array().unwrap();
        let names: Vec<&str> = specs
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["mcp__demo__query", "mcp__demo__list_all"]);
        assert_eq!(
            specs[0]["description"].as_str().unwrap(),
            "MCP调用：server=demo tool=query description=查询数据"
        );
        assert_eq!(specs[0]["input_schema"]["type"], "object");
        // 缺 schema 的工具回退空对象。
        assert_eq!(specs[1]["input_schema"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn mcp_工具直连未知名返回失败结果() {
        let service = service_with_cached_tools();
        let value = service
            .dispatch_operation("mcp__nope__missing", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["summary"].as_str().unwrap().contains("不是 MCP 工具"));
    }

    #[tokio::test]
    async fn mention_候选_入口与启用_server() {
        let service = service_with_cached_tools();
        let value = service
            .dispatch_operation(MCP_MENTION_CANDIDATES_OPERATION, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(value["entry"]["hint"], "调用外部 MCP server 的工具");
        let candidates = value["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0]["value"], "@mcp:demo");
        assert_eq!(candidates[0]["hint"], "2 工具");
    }

    #[test]
    fn register_appends_to_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("mcp.json");
        let existing = serde_json::to_string_pretty(&McpConfig {
            servers: vec![McpServerConfig {
                name: "existing".to_string(),
                transport: Default::default(),
                command: "echo".to_string(),
                args: vec![],
                endpoint: String::new(),
                auth_header: String::new(),
                headers: BTreeMap::new(),
                env: BTreeMap::new(),
                enabled: true,
                tags: vec![],
            }],
            ..Default::default()
        })
        .unwrap();
        std::fs::write(&config_path, existing).unwrap();
        let service =
            McpService::with_paths(config_path.clone(), dir.path().join("cache.json")).unwrap();
        service.register_server(register_request("added")).unwrap();
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("\"existing\""));
        assert!(content.contains("\"added\""));
    }
}
