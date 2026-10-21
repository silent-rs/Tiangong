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
    IndexTurnBatchRequest, SET_WORKSPACE_OPERATION, SetWorkspaceRequest,
};
use tiangong_plugin_index_protocol::management::{
    DELETE_WORKSPACE_INDEX_OPERATION, DeleteWorkspaceIndexRequest,
    LIST_WORKSPACE_INDEXES_OPERATION, PREWARM_WORKSPACE_INDEX_OPERATION,
    PrewarmWorkspaceIndexRequest, REBUILD_WORKSPACE_INDEX_OPERATION, RebuildWorkspaceIndexRequest,
    RebuildWorkspaceIndexResponse,
};
use tiangong_plugin_index_protocol::search::{
    INDEX_SEARCH_OPERATION, IndexSearchRequest, IndexSearchResponse, MENTION_FILES_OPERATION,
    MentionFileCandidate, MentionFilesRequest, MentionFilesResponse, SEARCH_CODE_OPERATION,
    SearchCodeRequest, SearchCodeResponse,
};
use tiangong_plugin_index_protocol::{
    INDEX_PROTOCOL_VERSION, IndexScope, PLUGIN_ID, PLUGIN_VERSION, WorkspaceIndexInfo,
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
    /// 当前会话工作目录（由 set_workspace 注入）。
    workspace: RwLock<Option<PathBuf>>,
}

impl IndexService {
    /// 用默认存储路径构造。
    pub fn new() -> Result<Self> {
        let manager = Arc::new(IndexManager::new()?);
        Ok(Self {
            manager,
            workspace: RwLock::new(None),
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
                capabilities: vec!["index".to_string()],
                instance_id: format!("index-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 Index 握手响应失败"),
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
            MENTION_FILES_OPERATION => {
                let req: MentionFilesRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 mention_files 请求失败")?;
                let manager = self.manager.clone();
                let workspace = self.resolve_mention_workspace(req.workspace.clone());
                let scan_root = workspace.clone();
                let resp = tokio::task::spawn_blocking(move || {
                    handle_mention_files_blocking(&manager, workspace.as_deref(), req)
                })
                .await
                .with_context(|| "mention_files 后台任务失败")??;
                // 查询可能刚触发索引重建：在此补一次全量扫描，否则空索引没人管。
                if let Some(root) = scan_root.as_deref() {
                    self.ensure_full_scan(root);
                }
                serde_json::to_value(resp).with_context(|| "序列化 mention_files 响应失败")
            }
            SET_WORKSPACE_OPERATION => {
                let req: SetWorkspaceRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 set_workspace 请求失败")?;
                self.handle_set_workspace(req.workspace)?;
                serde_json::to_value(tiangong_plugin_index_protocol::Ack {})
                    .with_context(|| "序列化 set_workspace 响应失败")
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

    /// mention 文件候选的工作区：请求注入优先（wasm 从宿主查询上下文读出），
    /// 其次 `set_workspace` 注入的全局工作区，最后回落宿主权威调用上下文。
    fn resolve_mention_workspace(&self, requested: Option<String>) -> Option<PathBuf> {
        requested
            .map(PathBuf::from)
            .or_else(|| self.workspace())
            .or_else(|| {
                tiangong_plugin_sidecar::invocation_context()
                    .map(|ctx| ctx.workspace)
                    .filter(|workspace| !workspace.is_empty())
                    .map(PathBuf::from)
            })
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
