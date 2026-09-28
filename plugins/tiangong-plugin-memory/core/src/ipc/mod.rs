//! IPC 模块（TCP loopback + 动态端口）
//!
//! 为保证 Windows / macOS / Linux 跨平台一致性，Memory IPC 统一使用：
//! - `127.0.0.1:0` 动态端口监听
//! - 本地 endpoint 文件发现
//! - 首帧 token 鉴权

pub mod protocol;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use plugin_ui::{CONFIG_GET_OPERATION, CONFIG_PROBE_OPERATION, CONFIG_SET_OPERATION};
use protocol::{
    IpcAuth, IpcEndpoint, IpcFrame, IpcRequest, IpcResponse, MemoryIpcRequestPayload,
    MemoryIpcResponsePayload,
};
use tiangong_plugin_memory_protocol::control::{
    DISABLE_OPERATION, ENABLE_OPERATION, STATUS_OPERATION, TEST_OPERATION,
};
use tiangong_plugin_memory_protocol::{
    injection as plugin_injection, recall as plugin_recall, rumination as plugin_rumination,
    ui as plugin_ui,
};
use tiangong_plugin_runtime::protocol::{
    ErrorCode as PluginErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION,
    Request as PluginRequest, Response as PluginResponse, ServiceStatus,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::handle::MemoryHandle;

/// 监听中的 IPC 服务端。
pub struct IpcServer {
    listener: TcpListener,
    endpoint: IpcEndpoint,
    endpoint_path: PathBuf,
}

/// IPC 客户端，建立连接后即完成鉴权。
pub struct IpcClient {
    connection: IpcConnection,
}

/// 已建立的双向连接。
pub struct IpcConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

/// 运行中的 IPC bridge 守卫。drop 时会触发后台服务退出。
pub struct IpcBridge {
    shutdown_tx: Option<watch::Sender<bool>>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl IpcServer {
    /// 绑定 `127.0.0.1:0`，创建 endpoint 文件。
    pub async fn bind(service: &str) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .with_context(|| "绑定 IPC TCP loopback 监听失败")?;
        let addr = listener
            .local_addr()
            .with_context(|| "读取 IPC 本地监听地址失败")?;
        let endpoint = IpcEndpoint {
            service: service.to_string(),
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            pid: std::process::id(),
            token: scru128::new().to_string(),
            updated_at: chrono::Local::now().naive_local().to_string(),
        };
        let endpoint_path = endpoint_path(service)?;
        persist_endpoint(&endpoint_path, &endpoint)?;

        Ok(Self {
            listener,
            endpoint,
            endpoint_path,
        })
    }

    pub fn endpoint(&self) -> &IpcEndpoint {
        &self.endpoint
    }

    pub fn endpoint_path(&self) -> &PathBuf {
        &self.endpoint_path
    }

    /// 接受一个客户端，并完成 token 鉴权。
    pub async fn accept_authenticated(&self) -> Result<IpcConnection> {
        let (stream, _addr) = self
            .listener
            .accept()
            .await
            .with_context(|| "接受 IPC 客户端连接失败")?;
        let mut conn = IpcConnection::from_stream(stream);

        match conn.read_frame().await? {
            IpcFrame::Auth(auth) if auth.token == self.endpoint.token => Ok(conn),
            IpcFrame::Auth(_) => {
                let _ = conn
                    .write_frame(&IpcFrame::Error {
                        message: "IPC 鉴权失败".to_string(),
                    })
                    .await;
                bail!("IPC 鉴权失败")
            }
            frame => bail!("IPC 首帧必须是 Auth，实际收到: {:?}", frame),
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.endpoint_path);
    }
}

impl IpcClient {
    /// 读取 endpoint 文件并连接。
    pub async fn connect(service: &str) -> Result<Self> {
        let endpoint = load_endpoint(service)?;
        Self::connect_endpoint(&endpoint).await
    }

    /// 使用给定 endpoint 建立连接并发送鉴权首帧。
    pub async fn connect_endpoint(endpoint: &IpcEndpoint) -> Result<Self> {
        let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .with_context(|| format!("连接 IPC 服务失败: {}:{}", endpoint.host, endpoint.port))?;
        let mut connection = IpcConnection::from_stream(stream);
        connection
            .write_frame(&IpcFrame::Auth(IpcAuth {
                token: endpoint.token.clone(),
            }))
            .await?;
        Ok(Self { connection })
    }

    pub async fn send_request(&mut self, request: IpcRequest) -> Result<()> {
        self.connection
            .write_frame(&IpcFrame::Request(request))
            .await
    }

    pub async fn read_response(&mut self) -> Result<IpcResponse> {
        match self.connection.read_frame().await? {
            IpcFrame::Response(resp) => Ok(resp),
            IpcFrame::Error { message } => bail!("IPC 服务端返回错误: {message}"),
            frame => bail!("期望 Response 帧，实际收到: {:?}", frame),
        }
    }
}

impl Drop for IpcBridge {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

/// 启动 Memory IPC bridge，将本地 `MemoryHandle` 暴露为 TCP loopback 服务。
pub fn spawn_memory_bridge(service: impl Into<String>, handle: MemoryHandle) -> Result<IpcBridge> {
    let service = service.into();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);

    let join_handle = thread::Builder::new()
        .name(format!("memory-ipc-bridge-{service}"))
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Memory IPC runtime 构建失败");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let server = match IpcServer::bind(&service).await {
                    Ok(server) => {
                        let _ = ready_tx.send(Ok(()));
                        Arc::new(server)
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(anyhow!(err.to_string())));
                        return;
                    }
                };
                if let Err(err) = run_memory_bridge(service, server, handle, shutdown_rx).await {
                    tracing::warn!("Memory IPC bridge 退出异常: {}", err);
                }
            });
        })
        .with_context(|| "创建 Memory IPC bridge 线程失败")?;

    ready_rx
        .recv()
        .with_context(|| "等待 Memory IPC bridge 就绪失败")??;

    Ok(IpcBridge {
        shutdown_tx: Some(shutdown_tx),
        join_handle: Some(join_handle),
    })
}

async fn run_memory_bridge(
    service: String,
    server: Arc<IpcServer>,
    handle: MemoryHandle,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    tracing::info!(
        service = %service,
        endpoint = %server.endpoint_path().display(),
        port = server.endpoint().port,
        "Memory IPC bridge 已启动"
    );

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = server.accept_authenticated() => {
                match accepted {
                    Ok(connection) => {
                        let handle = handle.clone();
                        tokio::spawn(async move {
                            if let Err(err) = serve_connection(connection, handle).await {
                                tracing::debug!("Memory IPC 连接结束: {}", err);
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!("接受 Memory IPC 客户端失败: {}", err);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }

    tracing::info!(service = %service, "Memory IPC bridge 已关闭");
    Ok(())
}

async fn serve_connection(mut connection: IpcConnection, handle: MemoryHandle) -> Result<()> {
    loop {
        let request = connection.read_request().await?;
        let response_payload = if request.payload.get("protocol_version").is_some() {
            let plugin_response = match serde_json::from_value::<PluginRequest>(request.payload) {
                Ok(plugin_request) => {
                    handle_plugin_request_with_connection(
                        handle.clone(),
                        plugin_request,
                        &mut connection,
                    )
                    .await
                }
                Err(error) => PluginResponse::error(
                    "unknown",
                    PluginErrorCode::BadRequest,
                    format!("解析插件 sidecar 请求失败: {error}"),
                    false,
                ),
            };
            serde_json::to_value(plugin_response).with_context(|| "序列化插件 sidecar 响应失败")?
        } else {
            let payload: MemoryIpcRequestPayload = serde_json::from_value(request.payload)
                .with_context(|| "解析 Memory IPC 请求载荷失败")?;
            serde_json::to_value(handle_memory_request(handle.clone(), payload).await?)
                .with_context(|| "序列化 Memory IPC 响应载荷失败")?
        };
        connection
            .write_response(IpcResponse {
                request_id: request.request_id,
                payload: response_payload,
            })
            .await?;
    }
}

async fn handle_plugin_request_with_connection(
    handle: MemoryHandle,
    request: PluginRequest,
    connection: &mut IpcConnection,
) -> PluginResponse {
    let request_id = request.request_id.clone();
    if request.protocol_version != PROTOCOL_VERSION {
        return PluginResponse::error(
            request_id,
            PluginErrorCode::ProtocolMismatch,
            format!(
                "Memory 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                request.protocol_version
            ),
            false,
        );
    }
    if crate::is_memory_disabled()
        && !matches!(
            request.operation.as_str(),
            HANDSHAKE_OPERATION
                | ENABLE_OPERATION
                | DISABLE_OPERATION
                | STATUS_OPERATION
                | TEST_OPERATION
                | CONFIG_GET_OPERATION
                | CONFIG_SET_OPERATION
                | CONFIG_PROBE_OPERATION
                | tiangong_plugin_memory_protocol::control::RECONFIGURE_OPERATION
        )
    {
        return PluginResponse::error(
            request_id,
            PluginErrorCode::ServiceDisabled,
            "Memory 已禁用",
            false,
        );
    }
    if request.operation != plugin_recall::RECALL_CONTEXT_OPERATION {
        return dispatch_checked_plugin_request(handle, request).await;
    }

    let recall: plugin_recall::RecallContextRequest =
        match decode(&request.payload, plugin_recall::RECALL_CONTEXT_OPERATION) {
            Ok(request) => request,
            Err(error) => {
                return PluginResponse::error(
                    request_id,
                    PluginErrorCode::BadRequest,
                    error.to_string(),
                    false,
                );
            }
        };
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut memory_request: crate::types::MemoryRecallRequest = match transcode(recall.request) {
        Ok(request) => request,
        Err(error) => {
            return PluginResponse::error(
                request_id,
                PluginErrorCode::BadRequest,
                error.to_string(),
                false,
            );
        }
    };
    memory_request.progress = Some(Arc::new(move |phase| {
        let event = tiangong_types::StreamEvent::MemoryRecallProgress {
            phase: phase.to_string(),
        };
        if let Ok(json) = serde_json::to_string(&event) {
            let _ = progress_tx.send(json);
        }
    }));

    let recall_task = tokio::spawn(async move { handle.recall_context(memory_request).await });
    tokio::pin!(recall_task);
    let response = loop {
        tokio::select! {
            Some(message) = progress_rx.recv() => {
                if let Err(error) = connection.write_progress(&request_id, message).await {
                    return PluginResponse::error(
                        request_id,
                        PluginErrorCode::ServiceError,
                        error.to_string(),
                        false,
                    );
                }
            }
            result = &mut recall_task => {
                while let Ok(message) = progress_rx.try_recv() {
                    if let Err(error) = connection.write_progress(&request_id, message).await {
                        return PluginResponse::error(
                            request_id,
                            PluginErrorCode::ServiceError,
                            error.to_string(),
                            false,
                        );
                    }
                }
                break result;
            },
        }
    };

    match response {
        Ok(response) => {
            let response: plugin_recall::RecallContextResponse = match transcode(response) {
                Ok(response) => response,
                Err(error) => {
                    return PluginResponse::error(
                        request_id,
                        PluginErrorCode::ServiceError,
                        error.to_string(),
                        false,
                    );
                }
            };
            match serde_json::to_value(plugin_recall::RecallContextResult { response }) {
                Ok(payload) => PluginResponse::success(request_id, payload),
                Err(error) => PluginResponse::error(
                    request_id,
                    PluginErrorCode::ServiceError,
                    error.to_string(),
                    false,
                ),
            }
        }
        Err(error) => PluginResponse::error(
            request_id,
            PluginErrorCode::ServiceError,
            error.to_string(),
            false,
        ),
    }
}

/// 无连接形态的插件请求分发（stdio 传输适配用）：进度通道不可用，
/// 其余语义与 IPC 连接分发一致。
pub async fn dispatch_checked_plugin_request(
    handle: MemoryHandle,
    request: PluginRequest,
) -> PluginResponse {
    let request_id = request.request_id.clone();
    match dispatch_plugin_request(handle, request).await {
        Ok(payload) => PluginResponse::success(request_id, payload),
        Err(error) => PluginResponse::error(
            request_id,
            PluginErrorCode::ServiceError,
            error.to_string(),
            false,
        ),
    }
}

async fn dispatch_plugin_request(
    handle: MemoryHandle,
    request: PluginRequest,
) -> Result<serde_json::Value> {
    match request.operation.as_str() {
        HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
            plugin_id: tiangong_plugin_memory_protocol::PLUGIN_ID.to_string(),
            plugin_version: tiangong_plugin_memory_protocol::PLUGIN_VERSION.to_string(),
            sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION.to_string(),
            business_protocol: tiangong_plugin_memory_protocol::MEMORY_PROTOCOL_VERSION,
            capabilities: vec!["memory".to_string()],
            instance_id: format!("memory-sidecar-{}", std::process::id()),
            status: ServiceStatus::Ready,
        })
        .with_context(|| "序列化 Memory 握手响应失败"),
        CONFIG_GET_OPERATION => memory_ui_bootstrap(),
        CONFIG_SET_OPERATION => {
            let selection: plugin_ui::MemorySelection = serde_json::from_value(request.payload)
                .with_context(|| "解析 Memory 页面配置失败")?;
            let selection: crate::MemoryConfigSelection = transcode(selection)?;
            // 保存路径必须读到真实旧配置：回退默认值会让"密钥留空=保留原值"
            // 语义失效，把已保存的密钥清空。读取失败时拒绝写入。
            let previous = crate::MemoryConfig::load()
                .with_context(|| "读取现有 Memory 配置失败，为避免覆盖已保存内容已取消保存")?;
            let config = selection.to_memory(&previous)?;
            config.save()?;
            handle.reconfigure(config.to_options()).await?;
            serde_json::to_value(tiangong_plugin_memory_protocol::Ack::default())
                .with_context(|| "序列化 Memory 配置保存响应失败")
        }
        tiangong_plugin_memory_protocol::control::RECONFIGURE_OPERATION => {
            let _: tiangong_plugin_memory_protocol::Empty = serde_json::from_value(request.payload)
                .with_context(|| "解析 Memory 重载配置请求失败")?;
            let config = crate::MemoryConfig::load_or_default();
            handle.reconfigure(config.to_options()).await?;
            serde_json::to_value(tiangong_plugin_memory_protocol::Ack::default())
                .with_context(|| "序列化 Memory 重载配置响应失败")
        }
        ENABLE_OPERATION => {
            crate::enable_memory()?;
            Ok(serde_json::json!({"ok": true, "disabled": false}))
        }
        DISABLE_OPERATION => {
            crate::disable_memory()?;
            Ok(serde_json::json!({"ok": true, "disabled": true}))
        }
        STATUS_OPERATION => memory_status(),
        TEST_OPERATION => memory_config_test(),
        CONFIG_PROBE_OPERATION => {
            let probe: plugin_ui::ProbeRequest = serde_json::from_value(request.payload)
                .with_context(|| "解析 Memory 探测请求失败")?;
            let response = memory_config_probe(probe).await;
            serde_json::to_value(response).with_context(|| "序列化 Memory 探测响应失败")
        }
        operation => {
            let memory_request = decode_plugin_memory_request(operation, &request.payload)?
                .ok_or_else(|| anyhow!("不支持的 Memory 操作: {operation}"))?;
            let response = handle_memory_request(handle, memory_request).await?;
            encode_plugin_memory_response(operation, response)
        }
    }
}

fn memory_ui_bootstrap() -> Result<serde_json::Value> {
    use tiangong_llm::models_config::{ModelCapability, RoutingSlot};

    let models = tiangong_config::io::load_models_config_at(&crate::paths::storage_root());
    let config = crate::MemoryConfig::load_or_default();
    let selection = crate::MemoryConfigSelection::from_memory(&config);

    // LLM 快速选择候选：先 lite/chat 路由，再具备 chat 能力的注册模型。
    let mut model_entries = [RoutingSlot::Lite, RoutingSlot::Chat]
        .into_iter()
        .filter_map(|slot| {
            let resolved = models.resolve_slot(slot)?;
            Some(plugin_ui::MemoryUiModel {
                key: slot.key().to_string(),
                provider: resolved.provider,
                model: resolved.model,
                capabilities: vec!["chat".to_string()],
                kind: "route".to_string(),
            })
        })
        .collect::<Vec<_>>();
    let mut registered = models
        .models
        .iter()
        .filter(|(_, entry)| entry.capabilities.contains(&ModelCapability::Chat))
        .map(|(key, entry)| plugin_ui::MemoryUiModel {
            key: key.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            capabilities: entry
                .capabilities
                .iter()
                .map(|capability| capability.key().to_string())
                .collect(),
            kind: "model".to_string(),
        })
        .collect::<Vec<_>>();
    registered.sort_by(|left, right| left.key.cmp(&right.key));
    model_entries.extend(registered);

    let default_llm = models
        .resolve_slot(RoutingSlot::Lite)
        .map(|resolved| format!("lite · {}", resolved.model))
        .or_else(|| {
            models
                .resolve_slot(RoutingSlot::Chat)
                .map(|resolved| format!("chat · {}", resolved.model))
        });

    serde_json::to_value(plugin_ui::MemoryBootstrap {
        config: transcode(selection)?,
        models: model_entries,
        default_llm,
        disabled: crate::is_memory_disabled(),
        local_models: transcode(crate::local_model::local_model_statuses())?,
    })
    .with_context(|| "序列化 Memory 页面配置失败")
}

fn decode_plugin_memory_request(
    operation: &str,
    payload: &serde_json::Value,
) -> Result<Option<MemoryIpcRequestPayload>> {
    let request = match operation {
        plugin_injection::LOAD_OPERATION => {
            let request: plugin_injection::LoadInjectionRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::LoadInjection {
                session_id: request.session_id,
                workspace_id: request.workspace_id,
            }
        }
        plugin_recall::RECALL_OPERATION => {
            let request: plugin_recall::RecallRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::Recall {
                anchors: transcode(request.anchors)?,
                limit: request.limit,
            }
        }
        plugin_recall::RECALL_CONTEXT_OPERATION => {
            let request: plugin_recall::RecallContextRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::RecallContext {
                request: transcode(request.request)?,
            }
        }
        plugin_rumination::RUN_ENHANCED_MICRO_OPERATION => {
            let request: plugin_rumination::RunEnhancedMicroRuminationRequest =
                decode(payload, operation)?;
            MemoryIpcRequestPayload::RunEnhancedMicroRumination {
                turn_result: transcode(request.turn_result)?,
            }
        }
        plugin_rumination::RUN_MESO_OPERATION => {
            let request: plugin_rumination::RunMesoRuminationRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::RunMesoRumination {
                session_id: request.session_id,
                workspace_id: request.workspace_id,
            }
        }
        plugin_rumination::RUN_META_OPERATION => {
            let _: plugin_rumination::RunMetaRuminationRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::RunMetaRumination
        }
        plugin_ui::LIST_NODES_OPERATION => {
            let request: plugin_ui::ListNodesRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::ListNodes {
                query: transcode(request.query)?,
            }
        }
        plugin_ui::COUNT_NODES_OPERATION => {
            let request: plugin_ui::CountNodesRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::CountNodes {
                query: transcode(request.query)?,
            }
        }
        plugin_ui::LIST_RELATIONS_OPERATION => {
            let request: plugin_ui::ListRelationsRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::ListRelations {
                node_id: request.node_id,
            }
        }
        plugin_ui::LIST_RELATIONS_BATCH_OPERATION => {
            let request: plugin_ui::ListRelationsBatchRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::ListRelationsBatch {
                node_ids: request.node_ids,
            }
        }
        plugin_ui::UPSERT_MANUAL_MEMORY_OPERATION => {
            let request: plugin_ui::UpsertManualMemoryRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::UpsertManualMemory {
                draft: transcode(request.draft)?,
            }
        }
        plugin_ui::SET_NODE_STATUS_OPERATION => {
            let request: plugin_ui::SetNodeStatusRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::SetNodeStatus {
                node_id: request.node_id,
                status: transcode(request.status)?,
            }
        }
        plugin_ui::UPSERT_RELATION_OPERATION => {
            let request: plugin_ui::UpsertRelationRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::UpsertRelation {
                draft: transcode(request.draft)?,
            }
        }
        plugin_ui::DELETE_RELATION_OPERATION => {
            let request: plugin_ui::DeleteRelationRequest = decode(payload, operation)?;
            MemoryIpcRequestPayload::DeleteRelation {
                relation_id: request.relation_id,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(request))
}

fn encode_plugin_memory_response(
    operation: &str,
    response: MemoryIpcResponsePayload,
) -> Result<serde_json::Value> {
    let value = match (operation, response) {
        (plugin_injection::LOAD_OPERATION, MemoryIpcResponsePayload::Injection { items }) => {
            serde_json::to_value(plugin_injection::LoadInjectionResponse { items })?
        }
        (plugin_recall::RECALL_OPERATION, MemoryIpcResponsePayload::Recall { hits }) => {
            let hits: Vec<plugin_recall::RecallHit> = transcode(hits)?;
            serde_json::to_value(plugin_recall::RecallResponse { hits })?
        }
        (
            plugin_recall::RECALL_CONTEXT_OPERATION,
            MemoryIpcResponsePayload::RecallContext { response },
        ) => {
            let response: plugin_recall::RecallContextResponse = transcode(response)?;
            serde_json::to_value(plugin_recall::RecallContextResult { response })?
        }
        (
            plugin_rumination::RUN_ENHANCED_MICRO_OPERATION
            | plugin_rumination::RUN_MESO_OPERATION
            | plugin_rumination::RUN_META_OPERATION,
            MemoryIpcResponsePayload::Ack,
        ) => serde_json::to_value(tiangong_plugin_memory_protocol::Ack::default())?,
        (plugin_ui::LIST_NODES_OPERATION, MemoryIpcResponsePayload::Nodes { items }) => {
            let items: Vec<plugin_ui::MemoryNode> = transcode(items)?;
            serde_json::to_value(plugin_ui::NodesResponse { items })?
        }
        (plugin_ui::COUNT_NODES_OPERATION, MemoryIpcResponsePayload::NodeCount { count }) => {
            serde_json::to_value(plugin_ui::NodeCountResponse { count })?
        }
        (
            plugin_ui::LIST_RELATIONS_OPERATION | plugin_ui::LIST_RELATIONS_BATCH_OPERATION,
            MemoryIpcResponsePayload::Relations { items },
        ) => {
            let items: Vec<plugin_ui::MemoryRelation> = transcode(items)?;
            serde_json::to_value(plugin_ui::RelationsResponse { items })?
        }
        (plugin_ui::UPSERT_MANUAL_MEMORY_OPERATION, MemoryIpcResponsePayload::Node { item }) => {
            let item: plugin_ui::MemoryNode = transcode(item)?;
            serde_json::to_value(plugin_ui::NodeResponse { item })?
        }
        (
            plugin_ui::SET_NODE_STATUS_OPERATION | plugin_ui::DELETE_RELATION_OPERATION,
            MemoryIpcResponsePayload::Ack,
        ) => serde_json::to_value(tiangong_plugin_memory_protocol::Ack::default())?,
        (plugin_ui::UPSERT_RELATION_OPERATION, MemoryIpcResponsePayload::Relation { item }) => {
            let item: plugin_ui::MemoryRelation = transcode(item)?;
            serde_json::to_value(plugin_ui::RelationResponse { item })?
        }
        (_, response) => {
            bail!("Memory {operation} 返回了不匹配的响应: {response:?}")
        }
    };
    Ok(value)
}

fn decode<T>(payload: &serde_json::Value, operation: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(payload.clone())
        .with_context(|| format!("解析 Memory {operation} 请求失败"))
}

fn transcode<T, U>(value: T) -> Result<U>
where
    T: serde::Serialize,
    U: serde::de::DeserializeOwned,
{
    serde_json::from_value(serde_json::to_value(value)?)
        .with_context(|| "转换 Memory 插件协议类型失败")
}

fn memory_status() -> Result<serde_json::Value> {
    let config = crate::MemoryConfig::load_or_default();
    let models = tiangong_config::io::load_models_config_at(&crate::paths::storage_root());
    let llm = match config.resolve_llm(&models) {
        Ok(Some(model)) => serde_json::json!({
            "source": llm_source_label(&config),
            "model": model.model,
            "base_url": model.base_url,
            "configured": true,
        }),
        Ok(None) => serde_json::json!({ "source": llm_source_label(&config), "configured": false }),
        Err(error) => serde_json::json!({
            "source": llm_source_label(&config),
            "configured": false,
            "error": error.to_string(),
        }),
    };
    Ok(serde_json::json!({
        "disabled": crate::is_memory_disabled(),
        "vector_mode": format!("{:?}", config.vector_mode),
        "local_tier": config.local_tier.key(),
        "llm": llm,
        "embedding": config.embedding.as_ref().map(|source| match source {
            crate::MemoryEmbeddingSource::Builtin => builtin_status(&config, true),
            crate::MemoryEmbeddingSource::Remote { endpoint, dimension } => serde_json::json!({
                "source": "remote",
                "model": endpoint.model,
                "base_url": endpoint.base_url,
                "dimension": dimension,
                "configured": remote_configured(endpoint) && *dimension > 0,
            }),
        }),
        "rerank": config.rerank.as_ref().map(|source| match source {
            crate::MemoryRerankSource::Builtin => builtin_status(&config, false),
            crate::MemoryRerankSource::Remote(endpoint) => serde_json::json!({
                "source": "remote",
                "model": endpoint.model,
                "base_url": endpoint.base_url,
                "configured": remote_configured(endpoint),
            }),
        }),
    }))
}

/// 内置组件状态：模型、下载/加载进度（下载状态跨进程可见）。
fn builtin_status(config: &crate::MemoryConfig, embedding: bool) -> serde_json::Value {
    let status = crate::local_model::local_model_status(config.local_tier, embedding);
    serde_json::json!({
        "source": "builtin",
        "tier": config.local_tier.key(),
        "model": status.model,
        "dimension": status.dimension,
        "state": status.state,
        "size": status.size,
        "downloaded": status.downloaded,
        "error": status.error,
        "configured": true,
    })
}

fn llm_source_label(config: &crate::MemoryConfig) -> &'static str {
    match &config.model {
        Some(crate::MemoryLlmSource::Remote(_)) => "remote",
        Some(crate::MemoryLlmSource::ModelsRef { key: Some(key) }) if !key.trim().is_empty() => {
            "models_ref"
        }
        _ => "follow_lite_chat",
    }
}

fn memory_config_test() -> Result<serde_json::Value> {
    let config = crate::MemoryConfig::load_or_default();
    let models = tiangong_config::io::load_models_config_at(&crate::paths::storage_root());
    let mut issues = Vec::new();
    match config.resolve_llm(&models) {
        Ok(Some(_)) => {
            if let Some(crate::MemoryLlmSource::Remote(endpoint)) = &config.model {
                push_secret_issue(&mut issues, "LLM", &endpoint.api_key);
            }
        }
        Ok(None) => issues.push("未配置 LLM（lite / chat 路由均缺失）".to_string()),
        Err(error) => issues.push(format!("LLM 配置不可用：{error}")),
    }
    match &config.embedding {
        Some(crate::MemoryEmbeddingSource::Remote {
            endpoint,
            dimension,
        }) => {
            if !remote_configured(endpoint) || *dimension == 0 {
                issues.push("Embedding 在线端点配置不完整（地址/模型/维度）".to_string());
            } else {
                push_secret_issue(&mut issues, "Embedding", &endpoint.api_key);
            }
        }
        Some(crate::MemoryEmbeddingSource::Builtin) => {
            push_builtin_issue(&mut issues, "Embedding", &config, true);
        }
        None => {}
    }
    match &config.rerank {
        Some(crate::MemoryRerankSource::Remote(endpoint)) => {
            if !remote_configured(endpoint) {
                issues.push("Rerank 在线端点配置不完整（地址/模型）".to_string());
            } else {
                push_secret_issue(&mut issues, "Rerank", &endpoint.api_key);
            }
        }
        Some(crate::MemoryRerankSource::Builtin) => {
            push_builtin_issue(&mut issues, "Rerank", &config, false);
        }
        None => {}
    }
    Ok(serde_json::json!({
        "ok": issues.is_empty(),
        "issues": issues,
    }))
}

/// 内置模型只有下载失败才算问题；未下载/下载中会在后台自动完成。
fn push_builtin_issue(
    issues: &mut Vec<String>,
    label: &str,
    config: &crate::MemoryConfig,
    embedding: bool,
) {
    let status = crate::local_model::local_model_status(config.local_tier, embedding);
    if let Some(error) = status.error {
        issues.push(format!("内置 {label}（{}）不可用：{error}", status.model));
    }
}

fn push_secret_issue(issues: &mut Vec<String>, label: &str, value: &str) {
    let trimmed = value.trim();
    let Some(inner) = trimmed
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
    else {
        return;
    };
    let variable = inner.trim();
    if variable.is_empty() {
        issues.push(format!("{label} 端点密钥环境变量名称为空"));
    } else if std::env::var(variable).is_err() {
        issues.push(format!("{label} 端点密钥环境变量 {variable} 未设置"));
    }
}

fn remote_configured(endpoint: &crate::MemoryRemoteEndpoint) -> bool {
    !endpoint.base_url.trim().is_empty() && !endpoint.model.trim().is_empty()
}

/// 真实请求在线端点：Embedding 发送一条样本并返回向量维度，Rerank 做一次两文档精排。
async fn memory_config_probe(probe: plugin_ui::ProbeRequest) -> plugin_ui::ProbeResponse {
    match run_config_probe(probe).await {
        Ok(response) => response,
        Err(error) => plugin_ui::ProbeResponse {
            ok: false,
            dimension: None,
            message: error.to_string(),
        },
    }
}

async fn run_config_probe(probe: plugin_ui::ProbeRequest) -> Result<plugin_ui::ProbeResponse> {
    // 同保存路径：读不到真实配置时不能用默认值当 previous，否则密钥继承语义错乱。
    let saved = crate::MemoryConfig::load().with_context(|| "读取现有 Memory 配置失败")?;
    let previous = match probe.component.as_str() {
        "llm" => match &saved.model {
            Some(crate::MemoryLlmSource::Remote(endpoint)) => Some(endpoint.clone()),
            _ => None,
        },
        "embedding" => match &saved.embedding {
            Some(crate::MemoryEmbeddingSource::Remote { endpoint, .. }) => Some(endpoint.clone()),
            _ => None,
        },
        "rerank" => match &saved.rerank {
            Some(crate::MemoryRerankSource::Remote(endpoint)) => Some(endpoint.clone()),
            _ => None,
        },
        other => bail!("不支持探测的组件：{other}"),
    };
    let remote: crate::MemoryRemoteSelection = transcode(probe.remote)?;
    let endpoint = remote.endpoint_for_probe(previous.as_ref())?;
    let api_key = tiangong_llm::models_config::ModelsConfig::resolve_api_key(&endpoint.api_key);
    let timeout = Duration::from_millis(endpoint.timeout_ms.min(30_000));

    if probe.component == "llm" {
        // 发一次极短的文本补全，确认地址、协议、密钥与模型名可用。
        let config = tiangong_llm::LlmEndpointConfig {
            source_provider: None,
            base_url: endpoint.base_url.clone(),
            api_key,
            model: endpoint.model.clone(),
            protocol: endpoint.protocol,
            timeout,
            max_retries: 0,
        };
        let reply = tiangong_llm::complete_text(
            &config,
            "You are a connectivity probe. Reply with the single word: ok",
            "ping",
            16,
        )
        .await
        .map_err(|error| anyhow!("{error}"))?;
        let preview = reply.trim().chars().take(24).collect::<String>();
        return Ok(plugin_ui::ProbeResponse {
            ok: true,
            dimension: None,
            message: if preview.is_empty() {
                "连接成功".to_string()
            } else {
                format!("连接成功，模型回复：{preview}")
            },
        });
    }

    if probe.component == "embedding" {
        // 维度未知：先用占位维度构造 provider，按实际返回向量长度确定维度。
        let provider = tiangong_llm::OpenAiEmbeddingProvider::from_config(
            &tiangong_llm::EmbeddingEndpointConfig {
                base_url: endpoint.base_url.clone(),
                api_key,
                model: endpoint.model.clone(),
                protocol: endpoint.protocol,
                timeout,
                dimension: 1,
            },
        )?;
        use tiangong_llm::EmbeddingProvider;
        let vectors = provider.embed(vec!["dimension probe".to_string()]).await?;
        let dimension = vectors
            .first()
            .map(Vec::len)
            .filter(|value| *value > 0)
            .ok_or_else(|| anyhow!("Embedding 接口返回空向量"))?;
        return Ok(plugin_ui::ProbeResponse {
            ok: true,
            dimension: Some(dimension),
            message: format!("连接成功，向量维度 {dimension}"),
        });
    }

    let provider =
        tiangong_llm::rerank_provider_from_config(&tiangong_llm::RerankEndpointConfig {
            base_url: endpoint.base_url.clone(),
            api_key,
            model: endpoint.model.clone(),
            protocol: endpoint.protocol,
            timeout,
        })?;
    let response = provider
        .rerank(tiangong_llm::RerankRequest {
            query: "memory probe".to_string(),
            documents: vec!["memory probe".to_string(), "unrelated".to_string()],
            top_n: 2,
        })
        .await?;
    if response.results.is_empty() {
        bail!("Rerank 接口未返回结果");
    }
    Ok(plugin_ui::ProbeResponse {
        ok: true,
        dimension: None,
        message: "连接成功".to_string(),
    })
}

pub async fn handle_memory_request(
    handle: MemoryHandle,
    payload: MemoryIpcRequestPayload,
) -> Result<MemoryIpcResponsePayload> {
    match payload {
        MemoryIpcRequestPayload::LoadInjection {
            session_id,
            workspace_id,
        } => Ok(MemoryIpcResponsePayload::Injection {
            items: handle
                .load_injection(&session_id, workspace_id.as_deref())
                .await,
        }),
        MemoryIpcRequestPayload::Recall { anchors, limit } => {
            Ok(MemoryIpcResponsePayload::Recall {
                hits: handle.recall(anchors, limit).await,
            })
        }
        MemoryIpcRequestPayload::RecallContext { request } => {
            Ok(MemoryIpcResponsePayload::RecallContext {
                response: handle.recall_context(request).await,
            })
        }
        MemoryIpcRequestPayload::RoughRecall { context } => Ok(MemoryIpcResponsePayload::Recall {
            hits: handle.rough_recall(context).await,
        }),
        MemoryIpcRequestPayload::EvaluateRecallSufficiency {
            context,
            rough_hits,
        } => Ok(MemoryIpcResponsePayload::RecallSufficiency {
            result: handle
                .evaluate_recall_sufficiency(context, rough_hits)
                .await,
        }),
        MemoryIpcRequestPayload::LoadDepth2 { node_ids } => Ok(MemoryIpcResponsePayload::Depth2 {
            items: handle.load_depth2(node_ids).await,
        }),
        MemoryIpcRequestPayload::ListNodes { query } => Ok(MemoryIpcResponsePayload::Nodes {
            items: handle.list_nodes(query).await,
        }),
        MemoryIpcRequestPayload::CountNodes { query } => Ok(MemoryIpcResponsePayload::NodeCount {
            count: handle.count_nodes(query).await,
        }),
        MemoryIpcRequestPayload::ListRelations { node_id } => {
            Ok(MemoryIpcResponsePayload::Relations {
                items: handle.list_relations(node_id).await,
            })
        }
        MemoryIpcRequestPayload::ListRelationsBatch { node_ids } => {
            Ok(MemoryIpcResponsePayload::Relations {
                items: handle.list_relations_batch(node_ids).await,
            })
        }
        MemoryIpcRequestPayload::WriteEpisode {
            episode,
            workspace_id,
        } => {
            handle.write_episode(episode, workspace_id);
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::UpsertManualMemory { draft } => {
            Ok(MemoryIpcResponsePayload::Node {
                item: handle.upsert_manual_memory(draft).await?,
            })
        }
        MemoryIpcRequestPayload::SetNodeStatus { node_id, status } => {
            handle.set_node_status(node_id, status).await?;
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::UpsertRelation { draft } => {
            Ok(MemoryIpcResponsePayload::Relation {
                item: handle.upsert_relation(draft).await?,
            })
        }
        MemoryIpcRequestPayload::DeleteRelation { relation_id } => {
            handle.delete_relation(relation_id).await?;
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::Reconfigure { config } => {
            handle.reconfigure(config.to_options()).await?;
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::UpdateInjection {
            level,
            target_id,
            content,
        } => {
            handle.update_injection(level, target_id, content);
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::RunMicroRumination { turn_result } => {
            handle.run_micro_rumination(turn_result);
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::SubmitCandidate { candidate } => {
            handle.submit_memory_candidate(candidate);
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::RunEnhancedMicroRumination { turn_result } => {
            handle.run_enhanced_micro_rumination(turn_result).await?;
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::RunMesoRumination {
            session_id,
            workspace_id,
        } => {
            handle.run_meso_rumination(session_id, workspace_id);
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::RunMetaRumination => {
            handle.run_meta_rumination();
            Ok(MemoryIpcResponsePayload::Ack)
        }
        MemoryIpcRequestPayload::Shutdown => {
            handle.shutdown().await;
            Ok(MemoryIpcResponsePayload::Ack)
        }
    }
}

impl IpcConnection {
    fn from_stream(stream: TcpStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        }
    }

    pub async fn write_request(&mut self, request: IpcRequest) -> Result<()> {
        self.write_frame(&IpcFrame::Request(request)).await
    }

    pub async fn read_request(&mut self) -> Result<IpcRequest> {
        match self.read_frame().await? {
            IpcFrame::Request(req) => Ok(req),
            IpcFrame::Error { message } => bail!("IPC 对端返回错误: {message}"),
            frame => bail!("期望 Request 帧，实际收到: {:?}", frame),
        }
    }

    pub async fn write_response(&mut self, response: IpcResponse) -> Result<()> {
        self.write_frame(&IpcFrame::Response(response)).await
    }

    pub async fn write_progress(&mut self, request_id: &str, message: String) -> Result<()> {
        self.write_frame(&IpcFrame::Progress {
            request_id: request_id.to_string(),
            message,
        })
        .await
    }

    async fn write_frame(&mut self, frame: &IpcFrame) -> Result<()> {
        let line = serde_json::to_string(frame).with_context(|| "序列化 IPC 帧失败")?;
        self.writer
            .write_all(line.as_bytes())
            .await
            .with_context(|| "写入 IPC 帧失败")?;
        self.writer
            .write_all(b"\n")
            .await
            .with_context(|| "写入 IPC 换行失败")?;
        self.writer
            .flush()
            .await
            .with_context(|| "刷新 IPC 帧失败")?;
        Ok(())
    }

    async fn read_frame(&mut self) -> Result<IpcFrame> {
        let mut buf = String::new();
        let bytes = self
            .reader
            .read_line(&mut buf)
            .await
            .with_context(|| "读取 IPC 帧失败")?;
        if bytes == 0 {
            bail!("IPC 连接已关闭");
        }
        serde_json::from_str(buf.trim_end()).with_context(|| "解析 IPC 帧失败")
    }
}

pub fn endpoint_path(service: &str) -> Result<PathBuf> {
    if let Some(path) =
        std::env::var_os("TIANGONG_PLUGIN_ENDPOINT").filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(path));
    }
    let base = runtime_dir()?;
    Ok(base.join(format!("{service}.json")))
}

pub fn load_endpoint(service: &str) -> Result<IpcEndpoint> {
    let path = endpoint_path(service)?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 IPC endpoint 文件失败: {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("解析 IPC endpoint 文件失败: {}", path.display()))
}

fn persist_endpoint(path: &PathBuf, endpoint: &IpcEndpoint) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 IPC runtime 目录失败: {}", parent.display()))?;
    }
    let content =
        serde_json::to_string_pretty(endpoint).with_context(|| "序列化 IPC endpoint 失败")?;
    std::fs::write(path, content)
        .with_context(|| format!("写入 IPC endpoint 文件失败: {}", path.display()))
}

fn runtime_dir() -> Result<PathBuf> {
    // 与 leader.json 同在存储根下：不同存储根（TIANGONG_STORAGE_ROOT）的实例
    // 各自选举，endpoint 文件也必须随之隔离，否则会互相覆盖/误连。
    Ok(crate::paths::memory_data_dir().join("runtime"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    struct EnvGuard {
        prev_home: Option<std::ffi::OsString>,
        prev_userprofile: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn enter(home: &std::path::Path) -> Self {
            let prev_home = std::env::var_os("HOME");
            let prev_userprofile = std::env::var_os("USERPROFILE");
            unsafe {
                std::env::set_var("HOME", home);
                std::env::set_var("USERPROFILE", home);
            }
            Self {
                prev_home,
                prev_userprofile,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.prev_userprofile {
                    Some(value) => std::env::set_var("USERPROFILE", value),
                    None => std::env::remove_var("USERPROFILE"),
                }
            }
        }
    }

    #[test]
    #[serial]
    fn endpoint_follows_storage_root() {
        let home = TempDir::new().expect("创建 fake home 失败");
        let root = TempDir::new().expect("创建存储根失败");
        let _env = EnvGuard::enter(home.path());
        let previous = std::env::var_os(tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV);
        unsafe {
            std::env::remove_var("TIANGONG_PLUGIN_ENDPOINT");
            std::env::set_var(
                tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV,
                root.path(),
            );
        }
        let path = endpoint_path("memory").expect("endpoint 路径");
        unsafe {
            match previous {
                Some(value) => {
                    std::env::set_var(tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV, value)
                }
                None => std::env::remove_var(tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV),
            }
        }
        // 与 leader.json 同在存储根下，不同存储根的实例互不干扰。
        assert_eq!(path, root.path().join("memory/runtime/memory.json"));
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn tcp_loopback_ipc_roundtrip_works() {
        let home = TempDir::new().expect("创建 fake home 失败");
        let _env = EnvGuard::enter(home.path());

        let server = IpcServer::bind("memory-test").await.expect("绑定 IPC 失败");
        let endpoint = load_endpoint("memory-test").expect("读取 endpoint 失败");

        let server_task = tokio::spawn(async move {
            let mut conn = server
                .accept_authenticated()
                .await
                .expect("服务端接受连接失败");
            let req = conn.read_request().await.expect("服务端读取请求失败");
            assert_eq!(req.request_id, "req-1");
            conn.write_response(IpcResponse {
                request_id: req.request_id,
                payload: serde_json::json!({ "ok": true }),
            })
            .await
            .expect("服务端写响应失败");
        });

        let mut client = IpcClient::connect_endpoint(&endpoint)
            .await
            .expect("客户端连接失败");
        client
            .send_request(IpcRequest {
                request_id: "req-1".to_string(),
                payload: serde_json::json!({ "hello": "world" }),
            })
            .await
            .expect("客户端写请求失败");
        let resp = client.read_response().await.expect("客户端读响应失败");
        assert_eq!(resp.request_id, "req-1");
        assert_eq!(resp.payload["ok"], true);

        server_task.await.expect("服务端任务失败");
    }
}
