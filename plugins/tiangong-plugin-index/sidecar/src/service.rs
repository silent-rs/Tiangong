//! Index sidecar 业务服务：承载 tantivy 索引、后台扫描与 rg/grep 检索，按操作名分发请求。
//!
//! 整合原 plugin.rs 的生命周期钩子（set_workspace / index_turn_batch / finalize）与
//! handler.rs 的工具执行（index_search / search_code）+ 管理 API，全部经 IPC 操作
//! 暴露给运行时（host 侧 invoke_sidecar）与 WASM 桥接。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow};

use tiangong_plugin_index_protocol::lifecycle::{
    FINALIZE_SESSION_OPERATION, FinalizeSessionRequest, INDEX_TURN_BATCH_OPERATION,
    IndexTurnBatchRequest,
};
use tiangong_plugin_index_protocol::management::{
    DELETE_WORKSPACE_INDEX_OPERATION, DeleteWorkspaceIndexRequest,
    LIST_WORKSPACE_INDEXES_OPERATION, PREWARM_WORKSPACE_INDEX_OPERATION,
    PrewarmWorkspaceIndexRequest, REBUILD_WORKSPACE_INDEX_OPERATION, RebuildWorkspaceIndexRequest,
    RebuildWorkspaceIndexResponse,
};
use tiangong_plugin_index_protocol::search::{
    INDEX_SEARCH_OPERATION, IndexSearchRequest, IndexSearchResponse, MENTION_CANDIDATES_OPERATION,
    MentionFileCandidate, MentionFilesRequest, MentionFilesResponse, SEARCH_CODE_OPERATION,
    SearchCodeRequest, SearchCodeResponse,
};
use tiangong_plugin_index_protocol::{
    INDEX_PROTOCOL_VERSION, IndexScope, LIFECYCLE_SESSION_ENDED_OPERATION,
    LIFECYCLE_SESSION_READY_OPERATION, LIFECYCLE_TURN_FINISHED_OPERATION,
    LIFECYCLE_TURN_STARTED_OPERATION, PLUGIN_ID, PLUGIN_VERSION, TOOL_INDEX_SEARCH,
    TOOL_SEARCH_CODE, TurnData, WorkspaceIndexInfo,
};
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};
use tiangong_toolkit as shared;

use crate::index::{IndexManager, IndexQuery};

/// Index sidecar 业务服务。
pub struct IndexService {
    manager: Arc<IndexManager>,
    /// 当前会话工作目录（由 set_workspace / 生命周期身份注入）。
    workspace: RwLock<Option<PathBuf>>,
    /// 当前会话 ID（index_search 的 session 范围使用；生命周期注入）。
    session_id: RwLock<Option<String>>,
}

impl IndexService {
    /// 用默认存储路径构造。
    pub fn new() -> Result<Self> {
        let manager = Arc::new(IndexManager::new()?);
        Ok(Self {
            manager,
            workspace: RwLock::new(None),
            session_id: RwLock::new(None),
        })
    }

    /// 按 sidecar 协议分发请求。
    ///
    /// `async`：慢操作（full_scan / search_code 子进程）经 `spawn_blocking` 在独立
    /// 线程执行，避免在单线程 runtime 上阻塞其他连接的请求与健康检查。
    pub async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Index 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }

        let payload = match self
            .dispatch_operation(&request.operation, request.payload)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return Response::error(
                    &request_id,
                    ErrorCode::ServiceError,
                    error.to_string(),
                    false,
                );
            }
        };
        Response::success(&request_id, payload)
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
                business_protocol: INDEX_PROTOCOL_VERSION,
                capabilities: vec![
                    "index".to_string(),
                    format!("tool:{TOOL_INDEX_SEARCH}"),
                    format!("tool:{TOOL_SEARCH_CODE}"),
                ],
                instance_id: format!("index-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 Index 握手响应失败"),
            // ── 宿主清单直连（v2 无 wasm 桥接）：operation 为工具名，返回
            // ToolOutcome 形状；文案沿用原桥接。工作区与会话 ID 由生命周期
            // 身份注入缓存，取代原 wasm set_workspace/session 缓存。──
            name if name == TOOL_INDEX_SEARCH => {
                let query = payload
                    .get("query")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string();
                if query.is_empty() {
                    return Ok(index_tool_failure(
                        false,
                        "查询为空",
                        "query parameter is required",
                    ));
                }
                let request = IndexSearchRequest {
                    query,
                    scope: match payload.get("scope").and_then(serde_json::Value::as_str) {
                        Some("workspace") => IndexScope::Workspace,
                        Some("session") => IndexScope::Session,
                        _ => IndexScope::All,
                    },
                    limit: payload
                        .get("limit")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(10)
                        .clamp(1, 20) as usize,
                    workspace: self.workspace().map(|path| path.display().to_string()),
                    session_id: self.cached_session_id(),
                };
                let manager = self.manager.clone();
                let cwd = self.resolve_workspace(request.workspace.clone());
                let scan_root = cwd.clone();
                let query = request.query.clone();
                let resp = tokio::task::spawn_blocking(move || {
                    handle_index_search_blocking(&manager, &cwd, request)
                })
                .await
                .with_context(|| "index_search 后台任务失败")??;
                if let Some(root) = scan_root.as_deref() {
                    self.ensure_full_scan(root);
                }
                Ok(index_search_outcome(&resp, &query))
            }
            name if name == TOOL_SEARCH_CODE => {
                let pattern = payload
                    .get("pattern")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string();
                if pattern.is_empty() {
                    return Ok(index_tool_failure(
                        false,
                        "search_code 缺少 pattern 参数",
                        "empty pattern",
                    ));
                }
                let req = SearchCodeRequest {
                    pattern,
                    path: payload
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from),
                    workspace: self.workspace().map(|path| path.display().to_string()),
                    // 直连通道暂不携带信任模式，按受限默认（工作区外路径将被拒绝）。
                    full_trust: false,
                };
                let base = self.resolve_workspace(req.workspace.clone());
                let resp =
                    tokio::task::spawn_blocking(move || handle_search_code_blocking(&base, req))
                        .await
                        .with_context(|| "search_code 后台任务失败")?;
                Ok(serde_json::json!({
                    "ok": resp.ok,
                    "summary": resp.summary,
                    "stdout": resp.stdout,
                    "stderr": resp.stderr,
                    "exit_code": resp.exit_code as i32,
                }))
            }
            // ── 宿主生命周期转发（capabilities.lifecycle）──
            name if name == LIFECYCLE_SESSION_READY_OPERATION
                || name == LIFECYCLE_TURN_STARTED_OPERATION =>
            {
                // 身份快照：缓存会话 ID；工作区变化时触发后台重扫（取代原
                // wasm set_workspace 转发）。
                let session: tiangong_types::PluginSession =
                    serde_json::from_value(payload).context("解析会话身份失败")?;
                if let Ok(mut guard) = self.session_id.write() {
                    *guard = (!session.id.is_empty()).then(|| session.id.clone());
                }
                let new_workspace = (!session.cwd.trim().is_empty()).then(|| session.cwd.clone());
                let changed = self.workspace() != new_workspace.as_ref().map(PathBuf::from);
                if changed {
                    self.handle_set_workspace(new_workspace)?;
                }
                Ok(serde_json::json!({"ok": true}))
            }
            LIFECYCLE_TURN_FINISHED_OPERATION => {
                // 全量快照：提取本轮消息批量写入会话索引（原 wasm
                // collect_turn_data/turn_index_content 逻辑内聚于此）。
                let session: tiangong_types::PluginSession =
                    serde_json::from_value(payload).context("解析轮次快照失败")?;
                let turns = collect_turn_data(&session);
                if turns.is_empty() {
                    return Ok(serde_json::json!({"ok": true}));
                }
                let request = IndexTurnBatchRequest {
                    session_id: session.id,
                    turns,
                };
                let manager = self.manager.clone();
                tokio::task::spawn_blocking(move || {
                    manager.index_turn_batch(&request.session_id, &request.turns)
                })
                .await
                .with_context(|| "index_turn_batch 后台任务失败")??;
                Ok(serde_json::json!({"ok": true}))
            }
            LIFECYCLE_SESSION_ENDED_OPERATION => {
                let session: tiangong_types::PluginSession =
                    serde_json::from_value(payload).context("解析会话结束快照失败")?;
                let request = FinalizeSessionRequest {
                    session_id: session.id,
                };
                let manager = self.manager.clone();
                tokio::task::spawn_blocking(move || {
                    manager.finalize_session_index(&request.session_id)
                })
                .await
                .with_context(|| "finalize_session 后台任务失败")??;
                Ok(serde_json::json!({"ok": true}))
            }
            // ── 动态提及候选（mention.candidates_op）──
            MENTION_CANDIDATES_OPERATION => {
                let workspace = payload
                    .pointer("/context/workspace")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let Some(workspace) = workspace else {
                    // 草稿/全局查询没有工作区，直接无候选。
                    return Ok(serde_json::json!({ "candidates": [] }));
                };
                let request = MentionFilesRequest {
                    query: payload
                        .get("query")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    limit: payload
                        .get("max_per_group")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0) as usize,
                    workspace: Some(workspace),
                };
                let manager = self.manager.clone();
                let scan_root = request.workspace.clone();
                let resp = match tokio::task::spawn_blocking(move || {
                    let workspace = request.workspace.clone();
                    let workspace = workspace.as_deref().map(std::path::Path::new);
                    handle_mention_files_blocking(&manager, workspace, request)
                })
                .await
                {
                    Ok(Ok(resp)) => resp,
                    Ok(Err(error)) => {
                        return Ok(mention_failure_response(&format!("{error:#}")));
                    }
                    Err(error) => {
                        return Ok(mention_failure_response(&format!(
                            "mention 后台任务失败：{error}"
                        )));
                    }
                };
                if let Some(root) = scan_root.as_deref() {
                    self.ensure_full_scan(std::path::Path::new(root));
                }
                Ok(mention_response(&resp))
            }
            INDEX_SEARCH_OPERATION => {
                let req: IndexSearchRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 index_search 请求失败")?;
                let manager = self.manager.clone();
                let cwd = self.resolve_workspace(req.workspace.clone());
                let scan_root = cwd.clone();
                let resp = tokio::task::spawn_blocking(move || {
                    handle_index_search_blocking(&manager, &cwd, req)
                })
                .await
                .with_context(|| "index_search 后台任务失败")??;
                // 查询可能刚触发索引重建：在此补一次全量扫描，否则空索引没人管。
                if let Some(root) = scan_root.as_deref() {
                    self.ensure_full_scan(root);
                }
                serde_json::to_value(resp).with_context(|| "序列化 index_search 响应失败")
            }
            SEARCH_CODE_OPERATION => {
                let req: SearchCodeRequest =
                    serde_json::from_value(payload).with_context(|| "解析 search_code 请求失败")?;
                let base = self.resolve_workspace(req.workspace.clone());
                let resp =
                    tokio::task::spawn_blocking(move || handle_search_code_blocking(&base, req))
                        .await
                        .with_context(|| "search_code 后台任务失败")?;
                serde_json::to_value(resp).with_context(|| "序列化 search_code 响应失败")
            }
            INDEX_TURN_BATCH_OPERATION => {
                let req: IndexTurnBatchRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 index_turn_batch 请求失败")?;
                let manager = self.manager.clone();
                tokio::task::spawn_blocking(move || {
                    manager.index_turn_batch(&req.session_id, &req.turns)
                })
                .await
                .with_context(|| "index_turn_batch 后台任务失败")??;
                serde_json::to_value(tiangong_plugin_index_protocol::Ack {})
                    .with_context(|| "序列化 index_turn_batch 响应失败")
            }
            FINALIZE_SESSION_OPERATION => {
                let req: FinalizeSessionRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 finalize_session 请求失败")?;
                let manager = self.manager.clone();
                tokio::task::spawn_blocking(move || {
                    manager.finalize_session_index(&req.session_id)
                })
                .await
                .with_context(|| "finalize_session 后台任务失败")??;
                serde_json::to_value(tiangong_plugin_index_protocol::Empty {})
                    .with_context(|| "序列化 finalize_session 响应失败")
            }
            LIST_WORKSPACE_INDEXES_OPERATION => {
                let manager = self.manager.clone();
                let infos = tokio::task::spawn_blocking(move || manager.list_workspace_indexes())
                    .await
                    .with_context(|| "list_workspace_indexes 后台任务失败")??;
                let resp: Vec<WorkspaceIndexInfo> = infos
                    .into_iter()
                    .map(|info| WorkspaceIndexInfo {
                        id: info.id,
                        root: info.root,
                        entry_count: info.entry_count,
                        updated_at: info.updated_at,
                    })
                    .collect();
                serde_json::to_value(resp).with_context(|| "序列化 list_workspace_indexes 响应失败")
            }
            DELETE_WORKSPACE_INDEX_OPERATION => {
                let req: DeleteWorkspaceIndexRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 delete_workspace_index 请求失败")?;
                let manager = self.manager.clone();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let root = Path::new(&req.root);
                    // 取得扫描 permit 与后台扫描/重建互斥，避免删除时后台线程继续写已移除目录。
                    let _permit = wait_for_scan_permit(&manager, root)?;
                    manager.delete_workspace_index(root, &req.workspace_id)
                })
                .await
                .with_context(|| "delete_workspace_index 后台任务失败")??;
                serde_json::to_value(tiangong_plugin_index_protocol::Empty {})
                    .with_context(|| "序列化 delete_workspace_index 响应失败")
            }
            REBUILD_WORKSPACE_INDEX_OPERATION => {
                let req: RebuildWorkspaceIndexRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 rebuild_workspace_index 请求失败")?;
                let manager = self.manager.clone();
                let count = tokio::task::spawn_blocking(move || -> Result<usize> {
                    let root = Path::new(&req.root);
                    // 取得扫描 permit 与后台扫描/删除互斥，避免并发写同一磁盘索引。
                    let _permit = wait_for_scan_permit(&manager, root)?;
                    manager.full_scan(root)
                })
                .await
                .with_context(|| "rebuild_workspace_index 后台任务失败")??;
                serde_json::to_value(RebuildWorkspaceIndexResponse { count })
                    .with_context(|| "序列化 rebuild_workspace_index 响应失败")
            }
            PREWARM_WORKSPACE_INDEX_OPERATION => {
                let req: PrewarmWorkspaceIndexRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 prewarm_workspace_index 请求失败")?;
                let manager = self.manager.clone();
                let root = PathBuf::from(&req.root);
                tokio::task::spawn_blocking(move || handle_prewarm_blocking(manager, &root))
                    .await
                    .with_context(|| "prewarm_workspace_index 后台任务失败")??;
                serde_json::to_value(tiangong_plugin_index_protocol::Empty {})
                    .with_context(|| "序列化 prewarm_workspace_index 响应失败")
            }
            operation => Err(anyhow!("不支持的 Index 操作: {operation}")),
        }
    }

    /// 解析本次请求实际使用的工作目录：优先请求携带的，回退全局缓存。
    ///
    /// 全局 workspace 仅由 `set_workspace` 钩子写入，用于触发后台扫描；查询时必须
    /// 按请求路由，避免同一 sidecar 服务多个不同工作区的会话时互相串用。
    fn resolve_workspace(&self, request_workspace: Option<String>) -> Option<PathBuf> {
        request_workspace
            .map(PathBuf::from)
            .or_else(|| self.workspace())
    }

    // ── 生命周期 ─────────────────────────────────────────────

    fn handle_set_workspace(&self, workspace: Option<String>) -> Result<()> {
        let new_path = workspace.map(PathBuf::from);
        if let Ok(mut guard) = self.workspace.write() {
            *guard = new_path.clone();
        }
        if let Some(ref root) = new_path
            && root.is_dir()
            && !self.manager.is_workspace_scanning(root)
        {
            self.refresh_workspace(root);
        }
        Ok(())
    }

    /// 首次打开或成功扫描超过一小时后触发增量刷新。
    fn refresh_workspace(&self, root: &Path) {
        const STALE_THRESHOLD_SECS: u64 = 3600;
        let needs_refresh = !self.manager.workspace_index_exists(root)
            || self
                .manager
                .workspace_index_age_secs(root)
                .is_none_or(|age| age > STALE_THRESHOLD_SECS);
        if needs_refresh {
            self.spawn_background_scan(root);
        }
    }

    /// 索引不可用（刚被删除重建 / 从未扫描）时补一次全量扫描。
    ///
    /// 由查询路径在拿到"扫描中"结果后调用。与 {@link spawn_background_scan}
    /// 的区别：后者服务于 `set_workspace` 的常规增量刷新，此处专治"索引被
    /// 删空后没人负责重建"——查询路径不经过 `set_workspace`，若不在此补，
    /// 空索引会一直空着。
    fn ensure_full_scan(&self, root: &Path) {
        let needs = self.manager.take_pending_full_scan(root)
            || self.manager.workspace_index_unavailable(root);
        if !needs {
            return;
        }
        let Some(permit) = self.manager.try_begin_workspace_scan(root) else {
            // 已有后台扫描在跑：把标记还回去，避免这次重建被漏掉。
            self.manager.mark_pending_full_scan(root);
            tracing::debug!(
                workspace = %root.display(),
                "Workspace 索引已有后台扫描在进行，重建补扫延后"
            );
            return;
        };
        let manager = self.manager.clone();
        let root = root.to_path_buf();
        tracing::info!(workspace = %root.display(), "Workspace 索引重建后补扫启动");
        std::thread::spawn(move || {
            let _permit = permit;
            match manager.full_scan(&root) {
                Ok(count) => tracing::info!(count, "Workspace 索引重建后补扫完成"),
                Err(e) => tracing::warn!("Workspace 索引重建后补扫失败: {e}"),
            }
        });
    }

    fn spawn_background_scan(&self, root: &Path) {
        let Some(permit) = self.manager.try_begin_workspace_scan(root) else {
            tracing::debug!(
                workspace = %root.display(),
                "Workspace 索引已有后台扫描在进行，跳过本次"
            );
            return;
        };
        let manager = self.manager.clone();
        let root = root.to_path_buf();
        tracing::info!(workspace = %root.display(), "Workspace 索引后台扫描启动");
        std::thread::spawn(move || {
            // permit 持有期间状态保持占用；drop（含 panic 展开）时自动复位。
            let _permit = permit;
            match manager.incremental_scan(&root) {
                Ok(count) => tracing::info!(count, "Workspace 索引后台增量刷新完成"),
                Err(e) => tracing::warn!("Workspace 索引后台增量刷新失败: {e}"),
            }
        });
    }

    // ── 辅助 ─────────────────────────────────────────────────────

    fn workspace(&self) -> Option<PathBuf> {
        self.workspace.read().ok()?.clone()
    }

    /// 生命周期注入的会话 ID 缓存。
    fn cached_session_id(&self) -> Option<String> {
        self.session_id.read().ok().and_then(|guard| guard.clone())
    }
}

/// 读路径解析（search_code 用）：只解析为绝对路径，读取范围由 OS 沙箱裁决。
fn resolve_read_path(raw: &str, base: &Path) -> Result<PathBuf> {
    shared::resolve_read_path_with(raw, base)
}

/// index_search 阻塞实现（在 spawn_blocking 线程内执行）。
fn handle_index_search_blocking(
    manager: &IndexManager,
    cwd: &Option<PathBuf>,
    req: IndexSearchRequest,
) -> Result<IndexSearchResponse> {
    let limit = if req.limit == 0 {
        10
    } else {
        req.limit.clamp(1, 20)
    };
    let scope = req.scope;

    let mut workspace_hits = Vec::new();
    let mut scanning = false;

    // Workspace 索引查询
    if matches!(scope, IndexScope::Workspace | IndexScope::All)
        && let Some(cwd) = cwd
        && cwd.is_dir()
    {
        if manager.is_workspace_scanning(cwd) {
            scanning = true;
        } else if manager.workspace_index_unavailable(cwd) {
            // 索引刚被删除重建或从未扫描：此时一定为空，与后台扫描同样按
            // "扫描中"处理，避免把空索引误报为"没有匹配"。
            scanning = true;
        } else {
            let index_query = IndexQuery::new(&req.query)
                .with_scope(IndexScope::Workspace)
                .with_limit(limit);
            match manager.search(cwd, &index_query) {
                Ok(hits) => workspace_hits = hits,
                Err(e) => {
                    tracing::warn!("工作区搜索失败: {e}");
                }
            }
        }
    }

    // Session 索引查询
    let mut session_hits = Vec::new();
    if matches!(scope, IndexScope::Session | IndexScope::All)
        && let Some(session_id) = &req.session_id
    {
        match manager.search_session(session_id, &req.query, limit) {
            Ok(hits) => session_hits = hits,
            Err(e) => {
                tracing::warn!("对话搜索失败: {e}");
            }
        }
    }

    Ok(IndexSearchResponse {
        workspace_hits,
        session_hits,
        scanning,
    })
}

/// `@` 提及文件候选阻塞实现（在 spawn_blocking 线程内执行）。
///
/// 与 `index_search` 的分工：mention 只查 path 字段（用户要「指向某个文件」），
/// 覆盖二进制/文档（它们只有 path 条目），多词 AND；索引未就绪时返回空候选并置
/// `scanning`，由 UI 提示，不用 fs 遍历兜底——两套来源会互相覆盖同 kind 候选。
fn handle_mention_files_blocking(
    manager: &IndexManager,
    workspace: Option<&Path>,
    req: MentionFilesRequest,
) -> Result<MentionFilesResponse> {
    let Some(workspace) = workspace else {
        return Ok(MentionFilesResponse::default());
    };
    if !workspace.is_dir() {
        return Ok(MentionFilesResponse::default());
    }
    if manager.is_workspace_scanning(workspace) {
        return Ok(MentionFilesResponse {
            candidates: Vec::new(),
            scanning: true,
        });
    }
    // 索引刚被删除重建（schema 升级 / 损坏恢复）：此刻一定为空。不能把空索引
    // 当作"没有匹配的文件"返回——用户会以为工作区里没有可提及的文件。转成
    // 扫描中，由调用方触发补扫，下次查询即可拿到候选。
    // 判据用 meta.json 而非进程内状态：重建索引的 sidecar 进程与随后发起查询的
    // 进程往往不是同一个（stdio sidecar 按连接短生命周期拉起）。
    if manager.workspace_index_unavailable(workspace) {
        return Ok(MentionFilesResponse {
            candidates: Vec::new(),
            scanning: true,
        });
    }
    let hits = manager.search_paths(workspace, &req.query, mention_limit(req.limit))?;
    let candidates = hits
        .into_iter()
        .map(|hit| {
            // label 取文件名（末段）；路径统一 `/` 分隔，跨平台可原样往返
            let file_name = hit
                .path
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(&hit.path)
                .to_string();
            MentionFileCandidate {
                relative_path: hit.path,
                file_name,
            }
        })
        .collect();
    Ok(MentionFilesResponse {
        candidates,
        scanning: false,
    })
}

/// mention 候选条数：请求未带 limit 时取默认值，否则收敛到 `[1, MAX]`。
/// 上限比 index_search 严——mention 是交互路径，前端还有每组展示截断。
fn mention_limit(requested: usize) -> usize {
    const DEFAULT_MENTION_LIMIT: usize = 50;
    const MAX_MENTION_LIMIT: usize = 200;
    if requested == 0 {
        DEFAULT_MENTION_LIMIT
    } else {
        requested.clamp(1, MAX_MENTION_LIMIT)
    }
}

/// search_code 阻塞实现（在 spawn_blocking 线程内执行）。
fn handle_search_code_blocking(
    base: &Option<PathBuf>,
    req: SearchCodeRequest,
) -> SearchCodeResponse {
    let Some(base) = base else {
        return SearchCodeResponse {
            ok: false,
            summary: "会话工作目录未注入，无法执行检索".to_string(),
            stderr: "workspace not available".to_string(),
            ..Default::default()
        };
    };
    let pattern = req.pattern.trim();
    if pattern.is_empty() {
        return SearchCodeResponse {
            ok: false,
            summary: "search_code pattern 不能为空".to_string(),
            stderr: "empty pattern".to_string(),
            ..Default::default()
        };
    }
    let target = req.path.as_deref().unwrap_or(".");
    let full_path = match resolve_read_path(target, base) {
        Ok(p) => p,
        Err(e) => {
            return SearchCodeResponse {
                ok: false,
                summary: format!("search_code 失败：{e}"),
                stderr: e.to_string(),
                ..Default::default()
            };
        }
    };

    let timeout_ms = std::env::var("TOOL_COMMAND_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(30_000);
    let target_text = full_path.display().to_string();
    let rg_result = shared::execute_command_with_timeout(
        Command::new("rg")
            .arg("--line-number")
            .arg("--no-heading")
            .arg("--color")
            .arg("never")
            .arg(pattern)
            .arg(&target_text)
            .current_dir(base)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        timeout_ms,
    );

    let (output, timed_out) = match rg_result {
        Ok(payload) => payload,
        Err(_) => match shared::execute_command_with_timeout(
            Command::new("grep")
                .arg("-R")
                .arg("-n")
                .arg("-I")
                .arg("--exclude-dir=.git")
                .arg("--exclude-dir=target")
                .arg("--exclude-dir=node_modules")
                .arg("--exclude-dir=dist")
                .arg("--exclude-dir=build")
                .arg(pattern)
                .arg(&target_text)
                .current_dir(base)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
            timeout_ms,
        )
        .with_context(|| format!("执行代码检索失败：pattern={pattern}"))
        {
            Ok(p) => p,
            Err(e) => {
                return SearchCodeResponse {
                    ok: false,
                    summary: format!("代码检索失败：{e}"),
                    stderr: e.to_string(),
                    ..Default::default()
                };
            }
        },
    };

    let exit_code = if timed_out {
        -1
    } else {
        output.status.code().unwrap_or(-1)
    };
    let stdout = shared::truncate_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = shared::truncate_output(&String::from_utf8_lossy(&output.stderr));
    let ok = !timed_out && (output.status.success() || exit_code == 1);
    let summary = if timed_out {
        format!("代码检索超时：pattern={pattern} (timeout_ms={timeout_ms})")
    } else if exit_code == 1 {
        format!("代码检索完成：未找到匹配（pattern={pattern}）")
    } else if ok {
        format!("代码检索成功：pattern={pattern}")
    } else {
        format!("代码检索失败：pattern={pattern} (exit_code={exit_code})")
    };

    SearchCodeResponse {
        ok,
        summary,
        stdout,
        stderr,
        exit_code: exit_code as i64,
    }
}

/// 等待取得工作区扫描 permit（与后台扫描/重建/删除互斥）。
///
/// 用于 delete/rebuild 操作：在已有后台扫描进行时短暂轮询等待（最多约 30s），
/// 拿到 permit 后返回；超时仍未取得则返回错误，避免无限阻塞累积后台任务。
fn wait_for_scan_permit(
    manager: &IndexManager,
    root: &Path,
) -> Result<crate::index::WorkspaceScanPermit> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(permit) = manager.try_begin_workspace_scan(root) {
            return Ok(permit);
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!("等待索引扫描许可超时，请稍后重试"));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// prewarm 阻塞实现（在 spawn_blocking 线程内执行）。
fn handle_prewarm_blocking(manager: Arc<IndexManager>, root: &Path) -> Result<()> {
    if !root.is_dir() || manager.workspace_index_exists(root) {
        return Ok(());
    }
    let Some(permit) = manager.try_begin_workspace_scan(root) else {
        return Ok(());
    };
    let manager = manager.clone();
    let root = root.to_path_buf();
    tracing::info!(workspace = %root.display(), "Workspace 索引预热启动");
    std::thread::spawn(move || {
        let _permit = permit;
        match manager.incremental_scan(&root) {
            Ok(count) => tracing::info!(count, "Workspace 索引预热完成"),
            Err(e) => tracing::warn!("Workspace 索引预热失败: {e}"),
        }
    });
    Ok(())
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for IndexService {
    async fn dispatch(
        &self,
        request: tiangong_plugin_sidecar::protocol::Request,
    ) -> tiangong_plugin_sidecar::protocol::Response {
        IndexService::dispatch(self, request).await
    }
}

// ── 工具直连与生命周期辅助（原 WASM 桥接逻辑内聚）──

fn index_tool_failure(ok: bool, summary: &str, stderr: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": ok,
        "summary": summary,
        "stdout": "",
        "stderr": stderr,
        "exit_code": if ok { 0 } else { 1 },
    })
}

/// index_search 结果组装（文案沿用原 WASM 桥接）。
fn index_search_outcome(response: &IndexSearchResponse, query: &str) -> serde_json::Value {
    let mut parts: Vec<String> = Vec::new();
    if response.scanning {
        parts.push("【工作区索引正在构建中，请稍候】".to_string());
    } else if !response.workspace_hits.is_empty() {
        parts.push("【工作区文件】".to_string());
        for hit in &response.workspace_hits {
            parts.push(format!("- {} ({})", hit.path, hit.language));
        }
    }
    if !response.session_hits.is_empty() {
        parts.push("【对话历史】".to_string());
        for hit in &response.session_hits {
            let preview: String = hit.content.chars().take(200).collect();
            parts.push(format!("- [{}] {}", hit.role, preview));
        }
    }
    if parts.is_empty() {
        return index_tool_failure(true, &format!("未找到与 \"{query}\" 相关的索引结果"), "");
    }
    let stdout = parts.join("\n");
    let count = parts.iter().filter(|line| line.starts_with('-')).count();
    serde_json::json!({
        "ok": true,
        "summary": format!("找到 {count} 条索引结果"),
        "stdout": stdout,
        "stderr": "",
        "exit_code": 0,
    })
}

/// 收集本轮需要写入会话索引的消息（原 wasm collect_turn_data）。
fn collect_turn_data(session: &tiangong_types::PluginSession) -> Vec<TurnData> {
    use tiangong_types::MessageRole;
    session
        .messages
        .iter()
        .filter_map(|msg| {
            let role = match msg.kind() {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
                MessageRole::System | MessageRole::Notice => return None,
            };
            if msg.kind() == MessageRole::User && !msg.is_user_input() {
                return None;
            }
            let content = turn_index_content(msg);
            if content.trim().is_empty() {
                return None;
            }
            Some(TurnData {
                turn_id: msg.id.clone(),
                workspace_id: session.cwd.clone(),
                role: role.to_string(),
                content,
                topics: Vec::new(),
                entity_names: Vec::new(),
            })
        })
        .collect()
}

/// 按内容块原始顺序生成索引正文（原 wasm turn_index_content）。
fn turn_index_content(message: &tiangong_types::Message) -> String {
    use tiangong_types::ContentBlock;
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::ModelInstruction { text } => {
                let text = text.trim();
                (!text.is_empty()).then(|| text.to_string())
            }
            ContentBlock::Image { asset, .. } | ContentBlock::AssetReference { asset } => {
                Some(format!(
                    "[附件] {} ({}) path={}",
                    asset.original_name, asset.mime_type, asset.local_path
                ))
            }
            ContentBlock::Media { url, title, .. } => Some(format!(
                "[附件] {} path={url}",
                title.as_deref().unwrap_or("media")
            )),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 动态提及候选：`@file:<相对路径>` token，hint 为相对路径。
fn mention_candidates_value(response: &MentionFilesResponse) -> serde_json::Value {
    let candidates: Vec<serde_json::Value> = response
        .candidates
        .iter()
        .map(|candidate| {
            // 路径含空白时用反引号包裹：前端 token 扫描到空白即止。
            let token = if candidate.relative_path.chars().any(|c| c.is_whitespace()) {
                format!("@file:`{}`", candidate.relative_path)
            } else {
                format!("@file:{}", candidate.relative_path)
            };
            serde_json::json!({
                "value": token,
                "label": candidate.file_name,
                "kind": "file",
                "hint": candidate.relative_path,
                "mark": "F",
            })
        })
        .collect();
    serde_json::Value::Array(candidates)
}

/// 提及查询响应：仅 `{ candidates }`——index 是内容提供方，不提供
/// "被提及"入口（`@plugin:index` 不是有效对象），用户 @ 出来的是文件。
/// 索引扫描中时候选自然不完整，不做占位表达。
fn mention_response(response: &MentionFilesResponse) -> serde_json::Value {
    serde_json::json!({
        "candidates": mention_candidates_value(response),
    })
}

/// 查询失败：不出现（fail-honest），原因记入日志供诊断。
fn mention_failure_response(reason: &str) -> serde_json::Value {
    tracing::warn!("mention 文件候选查询失败：{reason}");
    serde_json::json!({ "candidates": [] })
}

/// 会话消息收集（原 wasm 的 collect_turn_data 单测，随逻辑一并迁移）。
#[cfg(test)]
mod collect_turn_data_tests {
    use super::*;
    use tiangong_types::{ContentBlock, Message, MessageRole, UserSource};

    fn test_session(messages: Vec<Message>) -> tiangong_types::PluginSession {
        tiangong_types::PluginSession {
            id: "s-1".to_string(),
            title: String::new(),
            cwd: "/tmp/ws".to_string(),
            workspace_id: "ws".to_string(),
            reasoning_effort: None,
            messages,
            context_summary: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// 构造 RFC 0017 的插件反馈图片注入消息（role=User、phase=HostInjected，
    /// ModelInstruction + Image 块，text_content 为空）。
    fn host_injected_image_message() -> Message {
        let mut message = Message::new(MessageRole::User, String::new());
        message.content = vec![
            ContentBlock::model_instruction("[injected-images provenance]"),
            ContentBlock::image(
                tiangong_types::StoredAsset {
                    asset_id: "a-1".to_string(),
                    local_path: "/media/shot.png".to_string(),
                    original_name: "shot.png".to_string(),
                    mime_type: "image/png".to_string(),
                    size: 1024,
                    kind: tiangong_types::MediaKind::Image,
                },
                None,
            ),
        ];
        message.with_source(UserSource::HostInjected)
    }

    #[test]
    fn skips_host_injected_and_empty_messages() {
        let session = test_session(vec![
            Message::new(MessageRole::User, "帮我看下这张截图"),
            host_injected_image_message(),
            Message::new(MessageRole::Assistant, "已查看截图内容"),
            // 宿主注入的纯媒体消息：不是用户意图，不应进入索引。
            host_injected_image_message(),
        ]);
        let turns = collect_turn_data(&session);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "帮我看下这张截图");
        assert_eq!(turns[1].role, "assistant");
        assert!(turns.iter().all(|turn| !turn.content.trim().is_empty()));
    }

    #[test]
    fn skips_compressed_resume_message() {
        let resume = Message::new(MessageRole::User, "压缩恢复锚点")
            .with_source(UserSource::CompressedResume);
        let session = test_session(vec![resume, Message::new(MessageRole::User, "真实输入")]);
        let turns = collect_turn_data(&session);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "真实输入");
    }

    fn image_asset() -> tiangong_types::StoredAsset {
        tiangong_types::StoredAsset {
            asset_id: "a-2".to_string(),
            local_path: "/media/images/photo.png".to_string(),
            original_name: "photo.png".to_string(),
            mime_type: "image/png".to_string(),
            size: 2048,
            kind: tiangong_types::MediaKind::Image,
        }
    }

    #[test]
    fn indexes_attachment_only_user_message() {
        // 用户只发附件、未附文字：仍要写入索引，并能检索到附件路径。
        let mut message = Message::new(MessageRole::User, String::new());
        message.content = vec![
            ContentBlock::asset_reference(image_asset()),
            ContentBlock::model_instruction("本条用户消息只包含上述附件"),
        ];
        let turns = collect_turn_data(&test_session(vec![message]));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "user");
        assert!(
            turns[0]
                .content
                .contains("[附件] photo.png (image/png) path=/media/images/photo.png")
        );
        assert!(turns[0].content.contains("本条用户消息只包含上述附件"));
    }

    #[test]
    fn keeps_text_and_attachment_in_block_order() {
        let mut message = Message::new(MessageRole::User, String::new());
        message.content = vec![
            ContentBlock::text("把这张图调亮"),
            ContentBlock::asset_reference(image_asset()),
        ];
        let turns = collect_turn_data(&test_session(vec![message]));
        assert_eq!(
            turns[0].content,
            "把这张图调亮\n[附件] photo.png (image/png) path=/media/images/photo.png"
        );
    }
}
