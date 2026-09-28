use crate::app::TiangongApp;
use crate::view::*;
use base64::{engine::general_purpose, Engine as _};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State, Window};
use tauri_plugin_notification::{NotificationExt, PermissionState};
use tiangong_core::agent_input::AgentInputKind;
use tracing::warn;

const MAX_ATTACHMENT_BASE64_BYTES: u64 = 50 * 1024 * 1024;

#[allow(dead_code)]
fn done_event_keeps_turn_running(
    event: &tiangong_types::StreamEvent,
    has_pending_turn: bool,
) -> bool {
    matches!(event, tiangong_types::StreamEvent::Done { .. }) && has_pending_turn
}

fn merge_agent_worker_messages(
    messages: &mut Vec<tiangong_types::Message>,
    cached: &[tiangong_types::Message],
) {
    let mut indices = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let worker_id = message.worker_id.as_deref()?;
            worker_id
                .starts_with("agent:")
                .then(|| ((worker_id.to_string(), message.id.clone()), index))
        })
        .collect::<std::collections::HashMap<_, _>>();

    for message in cached {
        let Some(worker_id) = message
            .worker_id
            .as_deref()
            .filter(|worker_id| worker_id.starts_with("agent:"))
        else {
            continue;
        };
        let key = (worker_id.to_string(), message.id.clone());
        if let Some(index) = indices.get(&key).copied() {
            messages[index] = message.clone();
        } else {
            let index = messages.len();
            messages.push(message.clone());
            indices.insert(key, index);
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct AttachmentDataUrl {
    pub data_url: String,
    pub mime_type: String,
    pub title: String,
    pub base64_size: u64,
}

#[tauri::command]
pub async fn read_attachment_as_data_url(
    path: String,
    max_base64_bytes: Option<u64>,
) -> Result<AttachmentDataUrl, String> {
    let path_buf = PathBuf::from(&path);
    if !path_buf.is_file() {
        return Err(format!("附件不是可读取文件：{path}"));
    }

    let max_base64_bytes = max_base64_bytes.unwrap_or(MAX_ATTACHMENT_BASE64_BYTES);
    let metadata = std::fs::metadata(&path_buf).map_err(|e| format!("读取附件信息失败：{e}"))?;
    let estimated_base64_size = metadata.len().div_ceil(3) * 4;
    if estimated_base64_size > max_base64_bytes {
        return Err(format!(
            "附件过大：base64 编码后约 {:.1}MB，超过 50MB 限制",
            estimated_base64_size as f64 / 1024.0 / 1024.0
        ));
    }

    let bytes = std::fs::read(&path_buf).map_err(|e| format!("读取附件失败：{e}"))?;
    let encoded = general_purpose::STANDARD.encode(bytes);
    if encoded.len() as u64 > max_base64_bytes {
        return Err("附件过大：base64 编码后超过 50MB 限制".to_string());
    }

    let mime_type = mime_type_from_path(&path_buf);
    let title = path_buf
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(path.as_str())
        .to_string();
    Ok(AttachmentDataUrl {
        data_url: format!("data:{mime_type};base64,{encoded}"),
        mime_type,
        title,
        base64_size: encoded.len() as u64,
    })
}

fn mime_type_from_path(path: &std::path::Path) -> String {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[tauri::command]
pub async fn request_desktop_notification_permission(app: AppHandle) -> Result<bool, String> {
    let permission = app
        .notification()
        .request_permission()
        .map_err(|err| err.to_string())?;
    Ok(matches!(permission, PermissionState::Granted))
}

#[tauri::command]
pub async fn send_desktop_notification(
    title: String,
    body: String,
    session_id: Option<String>,
    app: AppHandle,
) -> Result<bool, String> {
    let permission = app
        .notification()
        .request_permission()
        .map_err(|err| err.to_string())?;
    if !matches!(permission, PermissionState::Granted) {
        return Ok(false);
    }

    let _ = session_id;
    show_desktop_notification(&app, title, body, "tiangong-background-sessions")?;
    Ok(true)
}

fn show_desktop_notification(
    app: &AppHandle,
    title: String,
    body: String,
    group: &str,
) -> Result<(), String> {
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .group(group)
        .auto_cancel()
        .show()
        .map_err(|err| err.to_string())
}

/// 宿主内部事件（托盘菜单等无窗口路径）的桌面通知：
/// 权限未授予时静默跳过，不主动弹出权限请求。
pub fn show_host_notification(app: &AppHandle, title: &str, body: &str) {
    let granted = app
        .notification()
        .permission_state()
        .map(|state| matches!(state, PermissionState::Granted))
        .unwrap_or(false);
    if !granted {
        return;
    }
    let _ = show_desktop_notification(app, title.to_string(), body.to_string(), "tiangong-server");
}

fn parse_model_capability(
    capability: &str,
) -> Result<tiangong_llm::models_config::ModelCapability, String> {
    tiangong_llm::models_config::ModelCapability::from_key(capability)
        .ok_or_else(|| format!("不支持的能力类型：{capability}"))
}

fn has_capability_in_state(
    core_state: &tiangong_app_state::app_state::TiangongState,
    capability: tiangong_llm::models_config::ModelCapability,
) -> bool {
    core_state.config.models.has_capability(capability)
}

// ============================================================================
// 会话管理
// ============================================================================

/// 获取所有会话列表
#[tauri::command]
pub async fn get_sessions(state: State<'_, TiangongApp>) -> Result<Vec<SessionListItem>, String> {
    let manager = state.core_manager.clone();
    tokio::task::spawn_blocking(move || {
        manager
            .list_session_metadata()
            .iter()
            .filter(|metadata| metadata.parent_session_id.is_none())
            .map(SessionListItem::from_metadata)
            .collect()
    })
    .await
    .map_err(|error| format!("等待会话列表加载失败：{error}"))
}

/// 获取单个会话的元数据（精确更新列表时使用，避免全量刷新）。
#[tauri::command]
pub async fn get_session_meta(
    session_id: String,
    state: State<'_, TiangongApp>,
) -> Result<Option<SessionListItem>, String> {
    let manager = state.core_manager.clone();
    tokio::task::spawn_blocking(move || {
        let meta = tiangong_core_manager::SessionMetadata::load_from_storage(
            manager.storage_root(),
            &session_id,
        )
        .ok()
        .filter(|m| m.parent_session_id.is_none());
        Ok(meta.as_ref().map(SessionListItem::from_metadata))
    })
    .await
    .map_err(|error| format!("读取会话元数据失败：{error}"))?
}

/// 切换到指定会话
#[tauri::command]
pub async fn switch_session(
    app: AppHandle,
    session_id: String,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    if !state.inner().core_manager.session_exists(&session_id) {
        return Err(format!("会话不存在：{session_id}"));
    }

    if !state.core_manager.has_live_core(&session_id) {
        let (stream_tx, stream_rx) = std::sync::mpsc::channel::<tiangong_types::StreamEvent>();
        let ensured = state
            .ensure_core(&session_id, None, None, None, None, stream_tx)
            .await?;
        if ensured.is_new {
            start_stream_consumer(app, ensured.session_id, stream_rx);
        }
    }

    state
        .with_state(|core_state| {
            core_state.active_session_id = session_id;
            Ok(())
        })
        .await?;

    Ok(())
}

/// 从 Session 真相源加载指定会话的界面数据。
#[tauri::command]
pub async fn load_session(
    session_id: String,
    state: State<'_, TiangongApp>,
) -> Result<crate::view::LoadedSessionView, String> {
    let config = state.core_manager.config().snapshot();
    // token 统计的分母跟随会话实际模型：有活跃 Core 时取其当前端点的窗口，
    // 否则按会话的模型选择解析；都拿不到才回落默认窗口。
    let context_limit = state
        .core_manager
        .session_context_limit(&session_id)
        .filter(|limit| *limit > 0)
        .unwrap_or_else(tiangong_core::config::core::default_context_limit);
    let default_reasoning_effort = config.reasoning_effort;
    let manager = state.core_manager.clone();
    let session_id_for_load = session_id.clone();
    let session = tokio::task::spawn_blocking(move || manager.load_session(&session_id_for_load))
        .await
        .map_err(|error| format!("等待会话加载失败：{error}"))??;

    let mut view = crate::view::LoadedSessionView::from_session(
        &session,
        context_limit,
        &default_reasoning_effort,
    );
    merge_agent_worker_messages(
        &mut view.messages,
        &state.agent_worker_view_messages(&session_id),
    );
    Ok(view)
}

/// 删除指定会话（逻辑删除）。
///
/// 会话文件原子移动到 `trash/sessions/`，从列表立即消失。
/// 不做媒体/teams/PTY/browser 清理——那些留给配置页的物理删除。
#[tauri::command]
pub async fn delete_session(
    session_id: String,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let deleted_id = session_id;
    let _cache_guard = state
        .input_cache_update_lock(&deleted_id)
        .lock_owned()
        .await;
    let _send_guard = state.session_send_lock(&deleted_id).lock_owned().await;
    // 逻辑删除：原子移动到 trash + 取消 Core。
    state
        .inner()
        .core_manager
        .delete_session(&deleted_id)
        .await
        .map_err(|error| format!("删除会话失败：{error}"))?;
    // 清理内存状态。
    state.fail_remote_session_waiters(&deleted_id, "目标会话已删除");
    state
        .with_state(|core_state| {
            crate::session_ops::remove_session_state(core_state, &state.core_manager, &deleted_id);
            Ok(())
        })
        .await?;
    let _ = state.release_any_input_send_claim(&deleted_id);
    state.clear_agent_worker_view(&deleted_id);
    state.remove_session_send_lock(&deleted_id);
    state.config_handoff_store.forget(&deleted_id);
    Ok(())
}

/// 删除指定 workspace（cwd）下的所有会话（逻辑删除）。
#[tauri::command]
pub async fn delete_sessions_by_cwd(
    cwd: String,
    state: State<'_, TiangongApp>,
) -> Result<DeleteResult, String> {
    let mut deleted_ids = state
        .with_state_read(|core_state| {
            let ids: Vec<String> = core_state
                .core_manager
                .list_session_metadata()
                .iter()
                .filter(|metadata| metadata.cwd == cwd)
                .map(|metadata| metadata.id.clone())
                .collect();
            Ok::<_, anyhow::Error>(ids)
        })
        .await?;
    deleted_ids.sort();
    let mut input_cache_guards = Vec::with_capacity(deleted_ids.len());
    for id in &deleted_ids {
        input_cache_guards.push(state.input_cache_update_lock(id).lock_owned().await);
    }
    let mut send_guards = Vec::with_capacity(deleted_ids.len());
    for id in &deleted_ids {
        send_guards.push(state.session_send_lock(id).lock_owned().await);
    }
    // 并发逻辑删除（等待 worker 退出 + 原子移动到 trash）。收集每项结果。
    let core_manager = state.inner().core_manager.clone();
    let deletes = deleted_ids.iter().map(|id| {
        let id = id.clone();
        let manager = core_manager.clone();
        async move {
            let result = manager.delete_session(&id).await;
            (id, result)
        }
    });
    let results = futures_util::future::join_all(deletes).await;
    let (succeeded, failed): (Vec<_>, Vec<_>) =
        results.into_iter().partition(|(_, result)| result.is_ok());
    let succeeded_ids: Vec<String> = succeeded.into_iter().map(|(id, _)| id).collect();
    let failed_ids: Vec<String> = failed
        .into_iter()
        .map(|(id, result)| {
            tracing::warn!(session_id = %id, error = ?result.err(), "批量删除：该会话删除失败");
            id
        })
        .collect();
    // 只清理成功删除的会话的内存状态。
    state
        .with_state(|core_state| {
            for id in &succeeded_ids {
                state.fail_remote_session_waiters(id, "目标会话已删除");
                crate::session_ops::remove_session_state(core_state, &state.core_manager, id);
            }
            Ok(())
        })
        .await?;
    for id in &succeeded_ids {
        let _ = state.release_any_input_send_claim(id);
        state.clear_agent_worker_view(id);
        state.remove_session_send_lock(id);
        // 交接记账随会话一并清理（与单删路径一致），否则定档指纹泄漏。
        state.config_handoff_store.forget(id);
    }
    drop(send_guards);
    drop(input_cache_guards);
    if !failed_ids.is_empty() {
        tracing::warn!(
            succeeded = succeeded_ids.len(),
            failed = failed_ids.len(),
            "批量删除部分失败"
        );
    }
    Ok(DeleteResult {
        succeeded: succeeded_ids,
        failed: failed_ids,
    })
}

/// 失败回滚只能关闭本次 ensure 绑定的实例，并且必须等待 worker 最终写盘结束，
/// 才能恢复宿主快照，避免旧 Core 的迟到持久化覆盖回滚结果。
pub(crate) async fn shutdown_join_core_if_current(state: &TiangongApp, session_id: &str) {
    if let Err(error) = state.core_manager.retire_core(session_id, false).await {
        tracing::warn!(%session_id, %error, "失败回滚时关闭 Core 失败");
    }
}

/// 更新会话标题
#[tauri::command]
pub async fn update_session_title(
    title: String,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err("会话标题不能为空".to_string());
    }

    let session_id = state
        .with_state_read(|core_state| Ok(core_state.active_session_id.as_str().to_string()))
        .await?;
    let session_lock = state.session_send_lock(&session_id);
    let _send_guard = session_lock.lock_owned().await;
    let active_session_id = state
        .with_state_read(|core_state| Ok(core_state.active_session_id.clone()))
        .await?;
    if active_session_id != session_id {
        return Err("活动会话已切换，请重新修改标题".to_string());
    }
    // 统一经 CoreManager（有 live Core 时委托 Core 走 turn 安全写入，否则直写盘）。
    // 标题变更通知由 Core 统一发 TitleChanged 事件，前端据此更新，不再 emit sessions_updated。
    state
        .core_manager
        .set_core_title(&session_id, title, false)?;
    Ok(())
}

// ============================================================================
// 消息和执行
// ============================================================================

#[derive(Clone, Copy)]
enum UserMessageDeliveryKind {
    NewTurn,
    Append,
}

struct UserMessageDeliveryRequest {
    session_id: String,
    content: String,
    attachments: Vec<tiangong_media_archive::RawAttachment>,
    revision: u64,
    workspace_dir: Option<String>,
    initial_trust_mode: Option<tiangong_types::TrustMode>,
    initial_reasoning_effort: Option<tiangong_llm::request::ReasoningEffort>,
    initial_model_ref: Option<String>,
    delivery_kind: UserMessageDeliveryKind,
    requires_input_claim: bool,
}

/// 发送消息并执行
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn send_message(
    session_id: String,
    content: String,
    attachments: Vec<tiangong_media_archive::RawAttachment>,
    revision: u64,
    cwd: Option<String>,
    trust_mode: Option<tiangong_types::TrustMode>,
    reasoning_effort: Option<String>,
    model_ref: Option<String>,
    app: AppHandle,
    _window: Window,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    send_message_inner(
        UserMessageDeliveryRequest {
            session_id,
            content,
            attachments,
            revision,
            workspace_dir: cwd,
            initial_trust_mode: trust_mode,
            initial_reasoning_effort: reasoning_effort
                .as_deref()
                .map(tiangong_llm::request::ReasoningEffort::parse_flexible),
            initial_model_ref: model_ref,
            delivery_kind: UserMessageDeliveryKind::NewTurn,
            requires_input_claim: true,
        },
        app,
        state.inner(),
    )
    .await
}

/// 旧前端兼容入口。新代码统一调用带 session_id/revision 的 `send_message`。
#[tauri::command]
pub async fn send_message_with_media(
    content: String,
    media: Vec<tiangong_types::MediaAsset>,
    app: AppHandle,
    _window: Window,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let (session_id, revision) = state
        .with_state_read(|core_state| {
            let session_id = core_state.active_session_id.as_str().to_string();
            let revision = crate::state_ops::input_cache(core_state, &session_id).revision;
            Ok((session_id, revision))
        })
        .await?;
    let attachments = media
        .into_iter()
        .map(|asset| tiangong_media_archive::RawAttachment {
            kind: asset.kind,
            source: asset.url,
            mime_type: asset.mime_type,
            original_name: asset.title,
        })
        .collect();
    send_message_inner(
        UserMessageDeliveryRequest {
            session_id,
            content,
            attachments,
            revision,
            workspace_dir: None,
            initial_trust_mode: None,
            initial_reasoning_effort: None,
            initial_model_ref: None,
            delivery_kind: UserMessageDeliveryKind::NewTurn,
            requires_input_claim: false,
        },
        app,
        state.inner(),
    )
    .await
}

async fn send_message_inner(
    request: UserMessageDeliveryRequest,
    app: AppHandle,
    state: &TiangongApp,
) -> Result<(), String> {
    use std::sync::mpsc;
    use tiangong_types::StreamEvent;

    let UserMessageDeliveryRequest {
        session_id,
        content,
        attachments,
        revision,
        workspace_dir,
        initial_trust_mode,
        initial_reasoning_effort,
        initial_model_ref,
        delivery_kind,
        requires_input_claim,
    } = request;

    if session_id.trim().is_empty() {
        return Err("目标会话 ID 不能为空".to_string());
    }

    if let Some(command) = parse_context_slash_command(&content) {
        let active_id = state
            .with_state_read(|core_state| Ok(core_state.active_session_id.as_str().to_string()))
            .await?;
        if active_id != session_id {
            return Err("目标会话已切换，请在原会话中重试该命令".to_string());
        }
        run_context_slash_command(command, app, state).await?;
        return Ok(());
    }

    let session_lock = state.session_send_lock(&session_id);
    let _send_guard = session_lock.lock().await;

    if state.remote_turn_owner(&session_id).is_some() {
        abort_session_send(state, &session_id, revision, false).await;
        return Err("目标会话正在处理远端请求，请等待本轮完成".to_string());
    }

    if requires_input_claim && !state.has_input_send_claim(&session_id, revision) {
        abort_session_send(state, &session_id, revision, false).await;
        return Err("发送输入尚未冻结，请基于最新输入重试".to_string());
    }

    let has_pending_turn = state
        .with_state_read(|core_state| {
            Ok(crate::state_ops::has_pending_turn(core_state, &session_id))
        })
        .await?;
    match delivery_kind {
        UserMessageDeliveryKind::NewTurn if has_pending_turn => {
            abort_session_send(state, &session_id, revision, false).await;
            return Err("目标会话已在执行，请使用追加消息".to_string());
        }
        UserMessageDeliveryKind::Append if !has_pending_turn => {
            abort_session_send(state, &session_id, revision, false).await;
            return Err("目标会话当前没有可追加的执行任务".to_string());
        }
        _ => {}
    }
    if state.input_revision_was_delivered(&session_id, revision) {
        abort_session_send(state, &session_id, revision, false).await;
        return Err("该版本输入已成功发送，已拒绝重复投递".to_string());
    }

    // 注：发送消息不改变任何配置（trust_mode/reasoning_effort/model 由用户在设置页
    // 手动变更，那些路径自行调用 sync_core_config_from_state）。ensure_core 内部对既有
    // Core 已做 replace_config（ensure.rs），无需在此全量加载所有 session metadata。
    if let Err(error) = state
        .with_state(|core_state| {
            crate::state_ops::begin_input_send(core_state, &session_id, revision)
        })
        .await
    {
        abort_session_send(state, &session_id, revision, false).await;
        return Err(error);
    }

    let capabilities = match attachment_capability_snapshot(state).await {
        Ok(value) => value,
        Err(error) => {
            abort_session_send(state, &session_id, revision, true).await;
            return Err(error);
        }
    };
    let user_message_id = scru128::new().to_string();
    let message_id_for_prepare = user_message_id.clone();
    let content_for_prepare = content.clone();
    let prepared_batch = tokio::task::spawn_blocking(move || {
        let store = tiangong_media_archive::AttachmentStore::default();
        let mut transaction = store.store_batch(attachments)?;
        let prepared = transaction.prepare_message(
            &message_id_for_prepare,
            content_for_prepare,
            capabilities,
        )?;
        Ok::<_, String>((transaction, prepared))
    })
    .await
    .map_err(|error| format!("附件准备任务失败：{error}"));
    let (transaction, prepared) = match prepared_batch {
        Ok(Ok(value)) => value,
        Ok(Err(error)) | Err(error) => {
            abort_session_send(state, &session_id, revision, true).await;
            return Err(format!("附件准备失败：{error}"));
        }
    };

    // 附件所有权从临时事务转移给该消息。
    let created_paths = transaction
        .newly_created_paths()
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    transaction.commit();

    // 获取或创建 TiangongCore(Core 的 deliver 负责 load session + 追加消息 +
    // persist,host 不再预写——issue #245 整改方案)。
    let (stream_tx, stream_rx) = mpsc::channel::<StreamEvent>();
    let workspace_dir = if state.core_manager.session_exists(&session_id) {
        None
    } else {
        let cwd = match workspace_dir {
            Some(cwd) if !cwd.trim().is_empty() => cwd,
            _ => {
                state
                    .with_state_read(|core_state| Ok(core_state.workspace_dir.clone()))
                    .await?
            }
        };
        if !std::path::Path::new(&cwd).is_dir() {
            abort_session_send(state, &session_id, revision, true).await;
            return Err(format!("新对话工作目录不存在或不是目录：{cwd}"));
        }
        Some(cwd)
    };
    // 本次发送携带的模型选择提前交给 ensure_core：新对话的 Core 正是在
    // 这次发送中创建，直接以目标模型初始化，投递时模型比较判定一致，
    // 不触发多余的切换流程。
    let turn_model_ref = initial_model_ref.filter(|key| !key.trim().is_empty());
    let ensured = state
        .ensure_core(
            &session_id,
            workspace_dir,
            initial_trust_mode,
            initial_reasoning_effort,
            turn_model_ref.as_deref(),
            stream_tx,
        )
        .await;
    let ensured = match ensured {
        Ok(ensured) => ensured,
        Err(error) => {
            let _ = restore_failed_user_message_state(state, &session_id, &user_message_id).await;
            cleanup_unreferenced_input_attachments(state, raw_attachments_for_paths(created_paths))
                .await;
            abort_session_send(state, &session_id, revision, true).await;
            return Err(error);
        }
    };
    let sid = ensured.session_id.clone();
    // 消费者必须在投递**之前**启动：投递内部的「切换前整理上下文 + 切换
    // 模型」是同步等待的，期间 core 会发出压缩/切换进度事件。若等投递返回
    // 再起消费者，这些事件只能堆在 channel 缓冲里，等到被消费时早已过期，
    // 用户在整段等待中看不到任何反馈。
    //
    // 提前启动是安全的：消费者只处理已发生的事件；投递失败时下方回滚路径
    // 的 shutdown_join_core_if_current 会关闭 Core，消费者随通道关闭正常退出。
    if ensured.is_new {
        start_stream_consumer(app, sid.clone(), stream_rx);
    }
    // 用户的模型选择交给 Manager：记录选择 → 解析目标 → 必要时整理上下文
    // 并切换 → 投递。app 层不参与编排，避免切换失败牵连会话 Core。
    // 新建 Core 时已用该选择初始化，此处比较通常一致、直接投递；既有 Core
    // 模型不符时仍按原流程切换。
    if let Err(error) = state
        .deliver_prepared_if_live(
            &sid,
            user_message_id.clone(),
            prepared.clone(),
            turn_model_ref.as_deref(),
        )
        .await
    {
        // 追加消息（引导消息）失败时不得关闭 Core：当前 turn 仍在正常执行，
        // 关闭会直接打断这段对话并让它显示为失败。只有新起轮次失败才回滚 Core。
        if matches!(delivery_kind, UserMessageDeliveryKind::NewTurn) {
            shutdown_join_core_if_current(state, &sid).await;
        }
        let _ = restore_failed_user_message_state(state, &session_id, &user_message_id).await;
        cleanup_unreferenced_input_attachments(state, raw_attachments_for_paths(created_paths))
            .await;
        abort_session_send(state, &session_id, revision, true).await;
        return Err(format!("消息投递失败：{error}"));
    }

    // 消息已投递给 Core（Core 内部会持久化），附件从此由消息引用持有，不能再自动回滚。
    state.mark_input_revision_delivered(&session_id, revision);
    let finish_result = state
        .with_state(|core_state| {
            let cache =
                crate::state_ops::finish_input_send(core_state, &session_id, revision, true)?;
            Ok(cache)
        })
        .await;
    if let Err(error) = finish_result {
        // Core 已确认稳定消息，此时不能把已发送的消息误报为可重试失败。
        tracing::error!(session_id, revision, error = %error, "消息已发送，但输入缓存终态更新失败");
    }
    release_input_send_claim_and_cleanup(state, &session_id, revision).await;

    Ok(())
}

async fn abort_session_send(state: &TiangongApp, session_id: &str, revision: u64, began: bool) {
    if began {
        let _ = state
            .with_state(|core_state| {
                crate::state_ops::finish_input_send(core_state, session_id, revision, false)
            })
            .await;
    }
    release_input_send_claim_and_cleanup(state, session_id, revision).await;
}

async fn release_input_send_claim_and_cleanup(
    state: &TiangongApp,
    session_id: &str,
    revision: u64,
) {
    let paths = state.release_input_send_claim(session_id, revision);
    if !paths.is_empty() {
        cleanup_unreferenced_input_attachments(state, raw_attachments_for_paths(paths)).await;
    }
}

pub(crate) async fn restore_failed_user_message_state(
    state: &TiangongApp,
    session_id: &str,
    message_id: &str,
) -> Result<(), String> {
    state
        .with_state(|core_state| {
            if core_state
                .core_manager
                .list_session_metadata()
                .iter()
                .any(|m| m.id == session_id)
            {
                crate::session_ops::remove_failed_message(
                    &state.core_manager,
                    session_id,
                    message_id,
                )?;
                crate::state_ops::remove_pending_message(core_state, session_id, message_id);
                Ok(())
            } else {
                Err(anyhow::anyhow!("目标会话已不存在：{session_id}"))
            }
        })
        .await
}

pub(crate) async fn attachment_capability_snapshot(
    state: &TiangongApp,
) -> Result<tiangong_media_archive::AttachmentCapabilitySnapshot, String> {
    state
        .with_state_read(|core_state| {
            use tiangong_llm::ModelCapability;
            let models = &core_state.config.models;
            let chat_multimodal = models.chat_is_multimodal();
            Ok(tiangong_media_archive::AttachmentCapabilitySnapshot {
                chat_multimodal,
                analyze_attachment: !chat_multimodal
                    && models
                        .resolve_for_capability(ModelCapability::Multimodal)
                        .is_some(),
                audio_processor: models
                    .resolve_for_capability(ModelCapability::Stt)
                    .is_some(),
                // 当前没有“视频内容分析”插件；视频生成能力不能冒充输入处理能力。
                video_processor: false,
            })
        })
        .await
}

fn emit_session_stream_event(
    app: &AppHandle,
    session_id: &str,
    event: &tiangong_types::StreamEvent,
) {
    let _ = app.emit(
        "stream_event",
        &tiangong_types::SessionEvent {
            session_id: session_id.to_string(),
            event: event.clone(),
        },
    );
}

/// 消费 StreamEvent：按会话转发给前端，并维护消息投递边界。
pub(crate) fn start_stream_consumer(
    app: AppHandle,
    session_id: String,
    stream_rx: std::sync::mpsc::Receiver<tiangong_types::StreamEvent>,
) {
    use tiangong_types::StreamEvent;

    let rt = tokio::runtime::Handle::current();
    thread::spawn(move || {
        for session_event in stream_rx.iter() {
            let app_state = app.state::<TiangongApp>();
            if matches!(&session_event, StreamEvent::TurnElapsed { .. }) {
                if app_state.core_manager.has_live_core(&session_id) {
                    emit_session_stream_event(&app, &session_id, &session_event);
                }
                continue;
            }

            // 标题变更：通知类事件，不触碰消息投递临界区，只转发流事件。
            // 前端收到 title_changed 后直接更新内存中对应会话标题，无需整表刷新。
            if matches!(&session_event, StreamEvent::TitleChanged { .. }) {
                emit_session_stream_event(&app, &session_id, &session_event);
                continue;
            }

            // 进度提示类事件走免锁快速通道：它们只驱动状态栏文案，不触碰
            // pending、消息列表或任何应用状态。
            //
            // 必须免锁——发送路径（send_message_inner / 编辑重发 / 嵌入
            // server）在投递期间全程持有同一把 session_send_lock，而投递
            // 内部的「切换前整理上下文 + 切换模型」是同步等待的。若这些
            // 事件也排队等锁，用户在整条链结束前看不到任何反馈，界面表现
            // 为凭空卡住数十秒。
            if matches!(
                &session_event,
                StreamEvent::ContextCompressing { .. }
                    | StreamEvent::ModelSwitchStarted { .. }
                    | StreamEvent::ModelSwitched { .. }
            ) {
                if app_state.core_manager.has_live_core(&session_id) {
                    emit_session_stream_event(&app, &session_id, &session_event);
                }
                continue;
            }

            let event_lock = app_state.session_send_lock(&session_id);
            let _event_guard = rt.block_on(event_lock.lock_owned());
            if !app_state.core_manager.has_live_core(&session_id) {
                // 已退役 Core 的缓冲事件不得修改同会话新实例的 pending、消息或 UI。
                continue;
            }

            let terminal_event = matches!(
                &session_event,
                StreamEvent::Done { .. } | StreamEvent::Error { .. }
            );
            // 普通流事件立即转发；终态等运行状态收尾后再对外发布。
            if !terminal_event {
                emit_session_stream_event(&app, &session_id, &session_event);
            }

            let sid = session_id.clone();
            let event = session_event;
            let is_done = matches!(event, StreamEvent::Done { .. });
            let is_error = matches!(event, StreamEvent::Error { .. });
            if let StreamEvent::AgentOutput {
                agent_id,
                agent_role,
                agent_label,
                messages,
            } = &event
            {
                app_state.merge_agent_output_view(
                    &sid,
                    agent_id,
                    agent_role,
                    agent_label,
                    messages,
                );
            }

            // App 只维护投递边界；正文、Thinking、工具和计时事件不触碰应用状态锁。
            let accepted_message_id = match &event {
                StreamEvent::UserMessage { message_id, .. } => Some(message_id.clone()),
                _ => None,
            };
            if accepted_message_id.is_some() || is_done || is_error {
                let _ = rt.block_on(app.state::<TiangongApp>().with_state(|core_state| {
                    if let Some(message_id) = accepted_message_id {
                        crate::state_ops::accept_pending_message(core_state, &sid, &message_id);
                    }
                    if is_done || is_error {
                        crate::state_ops::complete_accepted_turn(core_state, &sid);
                    }
                    Ok(())
                }));
            }

            if is_done || is_error {
                let final_sid = sid.clone();
                let completed_remote_message_id = app_state.remote_turn_owner(&final_sid);

                if let Some(message_id) = completed_remote_message_id {
                    rt.block_on(crate::embedded_server::complete_remote_turn_from_stream(
                        app.state::<TiangongApp>().inner(),
                        &final_sid,
                        &message_id,
                    ));
                }

                emit_session_stream_event(&app, &final_sid, &event);
                // 精确通知：该会话的元数据已更新（消息数/时间），前端只更新这一条。
                let _ = app.emit("session_meta_updated", &final_sid);
                // 标题生成已在 send_message_inner 投递后并行启动（与 chat turn 同时跑），
                // 完成后经 Command::SetTitle 投递给 turn task 安全写入，此处不再串行处理。
                // 不 break — 消费线程继续运行，等待下一轮消息的 StreamEvent
            }
        }

        let app_state = app.state::<TiangongApp>();
        let eof_lock = app_state.session_send_lock(&session_id);
        let _eof_guard = rt.block_on(eof_lock.lock_owned());
        // 通道关闭说明持有该发送端的 Core 已被显式移除。若同会话已经创建了新 Core，
        // 这是旧消费者的正常退出，不能按 session_id 清理新实例或覆盖新一轮状态。
        if !app_state.core_manager.has_live_core(&session_id) {
            let _ = rt.block_on(app_state.with_state(|core_state| {
                crate::state_ops::clear_pending_turn(core_state, &session_id);
                Ok(())
            }));
            app_state.fail_remote_session_waiters(&session_id, "执行已中断：Core 事件流已关闭");
            emit_session_stream_event(
                &app,
                &session_id,
                &tiangong_types::StreamEvent::Error {
                    message: "Core 事件流已关闭".to_string(),
                },
            );
        }
    });
}

/// 截断该消息之后的所有内容，更新消息内容，然后创建新的 core 重新执行 turn。
// Tauri 命令参数保持与现有前端调用协议一一对应，避免把传输层字段额外嵌套。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn edit_and_resend(
    session_id: String,
    message_id: String,
    new_content: String,
    attachments: Vec<tiangong_media_archive::RawAttachment>,
    revision: u64,
    base_content: Vec<tiangong_types::ContentBlock>,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    use std::sync::mpsc;

    if session_id.trim().is_empty() || message_id.trim().is_empty() {
        return Err("会话 ID 和消息 ID 不能为空".to_string());
    }
    if new_content.trim().is_empty() && attachments.is_empty() {
        return Err("编辑后的消息不能为空".to_string());
    }
    if base_content.is_empty() {
        return Err("编辑基线版本不能为空".to_string());
    }
    tracing::debug!(session_id, message_id, revision, "开始编辑重发校验");
    let session_lock = state.session_send_lock(&session_id);
    let _send_guard = session_lock.lock().await;
    if state.remote_turn_owner(&session_id).is_some() {
        return Err("目标会话正在处理远端请求，暂时不能编辑重发".to_string());
    }
    let has_pending_turn = state
        .with_state_read(|core_state| {
            Ok(crate::state_ops::has_pending_turn(core_state, &session_id))
        })
        .await?;
    if has_pending_turn {
        return Err("目标会话正在执行，暂时不能编辑重发".to_string());
    }
    // 注：编辑重发不改变配置；ensure_core 内部对既有 Core 已做 replace_config，
    // 无需在此全量加载所有 session metadata（与 send_message 同理）。

    // 第一遍只读校验发生在任何附件 IO 之前。
    state
        .with_state_read(|core_state| {
            // 会话存在性用单文件存在性判定（O(1)），避免遍历解析全部会话文件；
            // 校验本身用从磁盘 load 的最新 session（issue #245：真相源归磁盘）。
            if !core_state.core_manager.session_exists(&session_id) {
                return Err(anyhow::anyhow!("会话不存在：{session_id}"));
            }
            if crate::state_ops::has_pending_turn(core_state, &session_id) {
                return Err(anyhow::anyhow!("目标会话正在执行，暂时不能编辑重发"));
            }
            Ok(())
        })
        .await?;
    // 从磁盘 load 校验（不在 app-state 锁内做文件 IO）。
    let session_for_validation = state.inner().core_manager.load_session(&session_id)?;
    validate_editable_message(&session_for_validation, &message_id, &base_content)
        .map_err(|error| error.to_string())?;

    let capabilities = attachment_capability_snapshot(state.inner()).await?;
    let content_for_prepare = new_content.clone();
    let message_id_for_prepare = message_id.clone();
    let (transaction, prepared) = tokio::task::spawn_blocking(move || {
        let store = tiangong_media_archive::AttachmentStore::default();
        let mut transaction = store.store_batch(attachments)?;
        let prepared = transaction.prepare_message(
            &message_id_for_prepare,
            content_for_prepare,
            capabilities,
        )?;
        Ok::<_, String>((transaction, prepared))
    })
    .await
    .map_err(|error| format!("附件准备任务失败：{error}"))?
    .map_err(|error| format!("附件准备失败：{error}"))?;

    // 附件准备可能很慢；在同一把 App 状态锁内做完整二次校验、修改和持久化。
    // 只有本步骤成功后才允许终止旧任务。
    let prepared_for_state = tiangong_types::stable_content_blocks(&prepared);
    let (original_session, _session_snapshot) = state
        .with_state(|core_state| {
            let (original, runtime) = crate::session_ops::edit_prepared_user_message(
                core_state,
                &state.core_manager,
                &session_id,
                &message_id,
                prepared_for_state,
            )?;
            Ok((original, runtime))
        })
        .await?;

    let replaced_attachment_candidates = original_session
        .messages
        .iter()
        .flat_map(|message| {
            message
                .extract_media_assets()
                .into_iter()
                .map(|asset| asset.url)
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    // 编辑后的稳定 App 消息已落盘，立即转移附件所有权，避免异步
    // future 被取消时 Drop 误删已被稳定消息引用的新文件。
    let created_paths = transaction
        .newly_created_paths()
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    transaction.commit();

    // 编辑后的稳定消息已由 edit_prepared_user_message 落盘。此处复用既有 Core：
    // 编辑重发流程已校验无活跃 turn，Core 不会主动写盘，不存在旧 Core 覆盖编辑结果的
    // 风险。编辑重发复用现有 Core，不重复执行会话级初始化（on_session_ready 仅在该
    // Core 首次 turn 执行一次）；新一轮 turn 通过 on_turn_started 拿到截断后的最新
    // session。因此无需销毁重建 Core（省去 shutdown_join 同步阻塞与 WASM 插件重新
    // 实例化），与 send_message 的复用路径保持一致。
    let (stream_tx, stream_rx) = mpsc::channel::<tiangong_types::StreamEvent>();
    // 编辑重发不改变模型：沿用会话已记录的选择策略。
    let session_model_ref = state
        .core_manager
        .load_session(&session_id)
        .ok()
        .and_then(|session| session.model_ref);
    let ensured = state
        .ensure_core(&session_id, None, None, None, None, stream_tx)
        .await;
    let ensured = match ensured {
        Ok(ensured) => ensured,
        Err(error) => {
            restore_edited_session(state.inner(), &session_id, original_session).await;
            cleanup_unreferenced_input_attachments(
                state.inner(),
                raw_attachments_for_paths(created_paths.clone()),
            )
            .await;
            return Err(error);
        }
    };
    let sid = ensured.session_id.clone();
    if let Err(error) = state
        .deliver_prepared_if_live(
            &sid,
            message_id.clone(),
            prepared.clone(),
            // 编辑重发沿用会话已记录的模型选择，不改变模型。
            session_model_ref.as_deref(),
        )
        .await
    {
        // 复用 Core 时不销毁 Core（它仍可能被其它流程持有）。deliver 失败时尚未启动
        // 新 turn，只需把磁盘 session 恢复到编辑前状态，Core 下次读取即为正确内容。
        restore_edited_session(state.inner(), &session_id, original_session).await;
        cleanup_unreferenced_input_attachments(
            state.inner(),
            raw_attachments_for_paths(created_paths.clone()),
        )
        .await;
        return Err(format!("编辑消息投递失败：{error}"));
    }

    cleanup_unreferenced_input_attachments(
        state.inner(),
        raw_attachments_for_paths(replaced_attachment_candidates),
    )
    .await;
    if ensured.is_new {
        start_stream_consumer(app, sid, stream_rx);
    }

    Ok(())
}

fn validate_editable_message(
    session: &tiangong_core::session::Session,
    message_id: &str,
    base_content: &[tiangong_types::ContentBlock],
) -> Result<usize, anyhow::Error> {
    let message_index = session
        .messages
        .iter()
        .position(|message| message.id == message_id)
        .ok_or_else(|| anyhow::anyhow!("消息不存在：{message_id}"))?;
    let message = &session.messages[message_index];
    if message.role != tiangong_core::session::MessageRole::User {
        return Err(anyhow::anyhow!("只能编辑用户消息"));
    }
    if message.compact || message_index < session.summary_up_to {
        return Err(anyhow::anyhow!("该消息已被压缩或清空，无法编辑"));
    }
    if message.phase == tiangong_core::session::MessagePhase::CompressedResume {
        return Err(anyhow::anyhow!("该消息为压缩恢复消息，无法编辑"));
    }
    if message.phase == tiangong_core::session::MessagePhase::HostInjected {
        return Err(anyhow::anyhow!("该消息为宿主注入消息，无法编辑"));
    }
    if message.content != base_content {
        return Err(anyhow::anyhow!("消息已被更新，请基于最新内容重新编辑"));
    }
    Ok(message_index)
}

async fn restore_edited_session(
    state: &TiangongApp,
    session_id: &str,
    original_session: tiangong_core::session::Session,
) {
    let _ = state
        .with_state(|core_state| {
            crate::session_ops::restore_session(original_session)?;
            crate::state_ops::clear_pending_turn(core_state, session_id);
            Ok(())
        })
        .await;
}
#[tauri::command]
pub async fn cancel_turn(state: State<'_, TiangongApp>) -> Result<bool, String> {
    let session_id = state
        .with_state_read(|core_state| Ok(core_state.active_session_id.as_str().to_string()))
        .await?;
    if state.inner().core_manager.cancel_core(&session_id) {
        return Ok(true);
    }
    let session_lock = state.session_send_lock(&session_id);
    Ok(cancel_after_session_send_boundary(session_lock, || {
        state.inner().core_manager.cancel_core(&session_id)
    })
    .await)
}

async fn cancel_after_session_send_boundary(
    session_lock: Arc<tokio::sync::Mutex<()>>,
    cancel: impl FnOnce() -> bool,
) -> bool {
    // 发送准备阶段尚未安装 Core。等待同一会话的投递边界后再查找实例，确保取消
    // 命中本次刚创建的 Core，而不是在附件归档期间静默丢失。
    let _send_guard = session_lock.lock_owned().await;
    cancel()
}

#[derive(Clone, Copy)]
enum ContextSlashCommand {
    Compress,
    Reset,
}

fn parse_context_slash_command(content: &str) -> Option<ContextSlashCommand> {
    match content.trim() {
        "/compress" | "/压缩对话" => Some(ContextSlashCommand::Compress),
        "/reset" | "/清理对话" => Some(ContextSlashCommand::Reset),
        _ => None,
    }
}

async fn run_context_slash_command(
    command: ContextSlashCommand,
    app: AppHandle,
    state: &TiangongApp,
) -> Result<bool, String> {
    use std::sync::mpsc;

    // 注：compress/reset 不改变配置；ensure_core 内部对既有 Core 已做 replace_config，
    // 无需在此全量加载所有 session metadata（与 send_message 同理）。
    loop {
        let expected_session_id = state
            .with_state_read(|core_state| Ok(core_state.active_session_id.as_str().to_string()))
            .await?;
        let session_lock = state.session_send_lock(&expected_session_id);
        let _send_guard = session_lock.lock_owned().await;
        if state.remote_turn_owner(&expected_session_id).is_some() {
            return Err("目标会话正在处理远端请求，暂时不能修改上下文".to_string());
        }
        let current_session_id = state
            .with_state_read(|core_state| Ok(core_state.active_session_id.clone()))
            .await?;
        if current_session_id != expected_session_id {
            continue;
        }
        let session_id = current_session_id;
        let (stream_tx, stream_rx) = mpsc::channel::<tiangong_types::StreamEvent>();
        let ensured = state
            .ensure_core(&session_id, None, None, None, None, stream_tx)
            .await?;
        if ensured.is_new {
            start_stream_consumer(app.clone(), ensured.session_id.clone(), stream_rx);
        }
        return match command {
            // 整理上下文与模型切换前的整理共用同一实现：等待压缩进入终态
            // 后再返回，命令结果反映**压缩本身**的成败，而非"命令已投递"。
            // 失败沿调用栈回报，前端据此提示用户，不再出现"提示成功但历史
            // 没变"的中间态。
            ContextSlashCommand::Compress => {
                // 此 await 在 session_send_lock 内进行（压缩可达数十秒），
                // 与切换模型链路（set_session_model → 切换前整理）同一口
                // 径接受：压缩与后续写入共用一个临界区，顺序确定；代价是
                // 同会话其他写入型命令在此期间排队。取消不受影响（cancel
                // 先无锁试 cancel_core），进度事件走免锁快速通道（见
                // start_stream_consumer）。
                state
                    .core_manager
                    .compact_session_context(&ensured.session_id)
                    .await
                    .map(|()| true)
            }
            // 清理上下文是同步操作（只推进摘要边界、清空用量，不发模型
            // 请求），维持即发即走。
            ContextSlashCommand::Reset => Ok(state
                .core_manager
                .deliver_to_core_if_live(&ensured.session_id, AgentInputKind::reset_context())),
        };
    }
}

/// 手动触发上下文压缩
#[tauri::command]
pub async fn compress_context(
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<bool, String> {
    run_context_slash_command(ContextSlashCommand::Compress, app, state.inner()).await
}

/// 清理上下文（重置 LLM 上下文到初始 system prompt）
#[tauri::command]
pub async fn reset_context(app: AppHandle, state: State<'_, TiangongApp>) -> Result<bool, String> {
    run_context_slash_command(ContextSlashCommand::Reset, app, state.inner()).await
}

/// 向正在执行的 turn 追加用户消息
#[tauri::command]
pub async fn append_message(
    session_id: String,
    content: String,
    attachments: Vec<tiangong_media_archive::RawAttachment>,
    revision: u64,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<bool, String> {
    if session_id.trim().is_empty() {
        return Err("当前会话 ID 不能为空".to_string());
    }

    let is_running = state
        .with_state_read(|core_state| {
            Ok(crate::state_ops::has_pending_turn(core_state, &session_id))
        })
        .await?;
    if !is_running {
        abort_session_send(state.inner(), &session_id, revision, false).await;
        return Ok(false);
    }

    send_message_inner(
        UserMessageDeliveryRequest {
            session_id,
            content,
            attachments,
            revision,
            workspace_dir: None,
            initial_trust_mode: None,
            initial_reasoning_effort: None,
            initial_model_ref: None,
            delivery_kind: UserMessageDeliveryKind::Append,
            requires_input_claim: true,
        },
        app,
        state.inner(),
    )
    .await?;
    Ok(true)
}

/// 获取当前信任模式
#[tauri::command]
pub async fn get_trust_mode(
    session_id: Option<String>,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    state
        .with_state_read(|core_state| {
            let target_id = session_id
                .as_deref()
                .unwrap_or(core_state.active_session_id.as_str());
            let mode = core_state
                .core_manager
                .list_session_metadata()
                .iter()
                .find(|m| m.id == target_id)
                .map(|m| m.trust_mode)
                .unwrap_or(core_state.config.default_trust_mode);
            Ok(serde_json::to_value(mode)
                .unwrap_or_default()
                .as_str()
                .unwrap_or("full_trust")
                .to_string())
        })
        .await
}

/// 设置信任模式
#[tauri::command]
pub async fn set_trust_mode(
    mode: String,
    session_id: Option<String>,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let trust_mode: tiangong_core::permission::TrustMode =
        serde_json::from_value(serde_json::Value::String(mode))
            .map_err(|e| format!("无效的信任模式: {e}"))?;

    let session_id = match session_id {
        Some(session_id) => session_id,
        None => {
            state
                .with_state_read(|core_state| Ok(core_state.active_session_id.as_str().to_string()))
                .await?
        }
    };
    let session_lock = state.session_send_lock(&session_id);
    let _send_guard = session_lock.lock_owned().await;

    let previous_mode =
        crate::session_ops::update_trust_mode(&state.core_manager, &session_id, trust_mode)
            .map_err(|error| error.to_string())?;

    if let Err(error) = state.sync_core_config_from_state().await {
        rollback_session_trust_mode(state.inner(), &session_id, previous_mode).await;
        return Err(error);
    }
    // 配置替换命令可能排在当前 turn 后面，信任模式句柄必须立即生效。
    state
        .inner()
        .core_manager
        .set_core_trust_mode(&session_id, trust_mode);
    // Session 中的 trust_mode 不在这里单独落盘，由当前 turn 或下一轮统一持久化。
    Ok(())
}

async fn rollback_session_trust_mode(
    state: &TiangongApp,
    session_id: &str,
    previous_mode: tiangong_core::permission::TrustMode,
) {
    if let Err(error) =
        crate::session_ops::update_trust_mode(&state.core_manager, session_id, previous_mode)
    {
        warn!(%error, %session_id, "回滚会话信任模式失败");
    }
    if let Err(error) = state.sync_core_config_from_state().await {
        warn!(%error, %session_id, "回滚会话信任模式后同步配置失败");
    }
    state
        .core_manager
        .set_core_trust_mode(session_id, previous_mode);
}

#[tauri::command]
pub async fn get_default_trust_mode(state: State<'_, TiangongApp>) -> Result<String, String> {
    state
        .with_state_read(|core_state| {
            let mode = core_state.config.default_trust_mode;
            Ok(serde_json::to_value(mode)
                .unwrap_or_default()
                .as_str()
                .unwrap_or("full_trust")
                .to_string())
        })
        .await
}

#[tauri::command]
pub async fn set_default_trust_mode(
    mode: String,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let trust_mode: tiangong_core::permission::TrustMode =
        serde_json::from_value(serde_json::Value::String(mode))
            .map_err(|e| format!("无效的默认信任模式: {e}"))?;

    let mut config = state
        .with_state_read(|core_state| Ok(core_state.config.clone()))
        .await?;
    config.default_trust_mode = trust_mode;
    tiangong_config::registry::update(config.clone()).map_err(|error| error.to_string())?;
    state
        .with_state(|core_state| {
            core_state.config = config;
            core_state.agent_config.default_trust_mode = trust_mode;
            Ok(())
        })
        .await?;
    state.sync_core_config_from_state().await?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SandboxPolicyView {
    pub directory_allowlist: Vec<String>,
    pub environment_blocklist: Vec<String>,
}

impl From<tiangong_config::SandboxUserPolicy> for SandboxPolicyView {
    fn from(value: tiangong_config::SandboxUserPolicy) -> Self {
        Self {
            directory_allowlist: value.directory_allowlist,
            environment_blocklist: value.environment_blocklist,
        }
    }
}

#[tauri::command]
pub async fn get_sandbox_policy(
    state: State<'_, TiangongApp>,
) -> Result<SandboxPolicyView, String> {
    state
        .with_state_read(|core_state| Ok(core_state.config.sandbox_policy.clone().into()))
        .await
}

fn normalize_path_list(values: Vec<String>) -> Result<Vec<String>, String> {
    let mut seen = std::collections::BTreeSet::new();
    let base = std::env::current_dir().map_err(|error| error.to_string())?;
    values
        .into_iter()
        .filter_map(|raw| {
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }
            let path = std::path::PathBuf::from(raw);
            let absolute = if path.is_absolute() {
                path
            } else {
                base.join(path)
            };
            if !absolute.exists() {
                return Some(Err(format!("目录不存在：{}", absolute.display())));
            }
            if !absolute.is_dir() {
                return Some(Err(format!("路径不是目录：{}", absolute.display())));
            }
            let normalized = match std::fs::canonicalize(&absolute) {
                Ok(path) => path,
                Err(error) => {
                    return Some(Err(format!(
                        "解析目录失败：{}（{error}）",
                        absolute.display()
                    )));
                }
            };
            let text = normalized.to_string_lossy().to_string();
            seen.insert(text.clone()).then_some(Ok(text))
        })
        .collect()
}

fn normalize_env_list(values: Vec<String>) -> Result<Vec<String>, String> {
    let mut seen = std::collections::BTreeSet::new();
    values
        .into_iter()
        .filter_map(|raw| {
            let item = raw.trim().to_string();
            if item.is_empty() {
                return None;
            }
            Some(if is_valid_env_name(&item) {
                let key = item.to_ascii_lowercase();
                Ok(seen.insert(key).then_some(item))
            } else {
                Err(format!("无效的环境变量名：{item}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|values| values.into_iter().flatten().collect())
}

#[tauri::command]
pub async fn set_sandbox_policy(
    policy: SandboxPolicyView,
    state: State<'_, TiangongApp>,
) -> Result<SandboxPolicyView, String> {
    let cleaned = tiangong_config::SandboxUserPolicy {
        directory_allowlist: normalize_path_list(policy.directory_allowlist)?,
        environment_blocklist: normalize_env_list(policy.environment_blocklist)?,
    };
    let mut config = state
        .with_state_read(|core_state| Ok(core_state.config.clone()))
        .await?;
    config.sandbox_policy = cleaned.clone();
    config.command_env_blocklist = cleaned.environment_blocklist.clone();
    #[cfg(windows)]
    let storage_root = config.storage_root.clone();
    tiangong_config::registry::update(config.clone()).map_err(|error| error.to_string())?;
    state
        .with_state(|core_state| {
            core_state.config = config;
            Ok(())
        })
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::registry::on_sandbox_setting_changed();
        #[cfg(windows)]
        tiangong_plugin_runtime::registry::invalidate_persistent_grants(&storage_root)
            .map_err(|error| format!("配置已保存，但旧授权清理或插件重载失败：{error:#}"))?;
        Ok::<_, String>(())
    })
    .await
    .map_err(|error| error.to_string())??;
    Ok(cleaned.into())
}

#[tauri::command]
pub async fn get_sandbox_disabled(state: State<'_, TiangongApp>) -> Result<bool, String> {
    state
        .with_state_read(|core_state| Ok(core_state.config.sandbox_disabled))
        .await
}

#[tauri::command]
pub async fn get_command_env_blocklist(
    state: State<'_, TiangongApp>,
) -> Result<Vec<String>, String> {
    let config = state
        .with_state_read(|core_state| Ok(core_state.config.clone()))
        .await?;
    Ok(config.command_env_blocklist)
}

/// 手动安装或更新 Sandbox 的结果。程序与签名直接位于存储目录的 sandbox 下。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum LauncherUpdateResult {
    Installed { version: String },
}

#[tauri::command]
pub async fn upgrade_launcher(
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<LauncherUpdateResult, String> {
    use tauri::Emitter as _;
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await
        .map_err(|e| e.to_string())?;
    tiangong_plugin_runtime::launcher_update::mark_launcher_preparing(true);
    let _ = app.emit(
        "startup-prepare-step",
        serde_json::json!({ "step": "launcher" }),
    );
    let result = tiangong_plugin_runtime::launcher_update::LauncherUpdater::new()
        .install_or_update(&storage_root)
        .await;
    tiangong_plugin_runtime::launcher_update::mark_launcher_preparing(false);
    match result {
        Ok(version) => {
            tiangong_plugin_runtime::launcher_update::record_startup_prepare_failure(None);
            tiangong_plugin_runtime::registry::prewarm_resident_sidecars(&storage_root);
            // 沙箱修复后自动重试启动期失败的插件验证与常驻进程。
            state.retry_failed_plugin_preload();
            let _ = app.emit(
                "startup-prepare-step",
                serde_json::json!({ "step": "done", "version": version }),
            );
            Ok(LauncherUpdateResult::Installed { version })
        }
        Err(error) => {
            let reason = format!("{error:#}");
            tiangong_plugin_runtime::launcher_update::record_startup_prepare_failure(Some(
                reason.clone(),
            ));
            Err(format!("Sandbox 安装或更新失败：{reason}"))
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct SandboxUpdateState {
    pub status: String,
    pub version: Option<String>,
    pub failure: Option<String>,
}

#[tauri::command]
pub async fn get_sandbox_update_state(
    state: State<'_, TiangongApp>,
) -> Result<SandboxUpdateState, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await
        .map_err(|e| e.to_string())?;
    let (status, version) = tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::launcher_update::launcher_status(&storage_root)
    })
    .await
    .map_err(|error| error.to_string())?;
    let failure = (status
        == tiangong_plugin_runtime::launcher_update::SandboxLauncherStatus::Failed)
        .then(|| {
            tiangong_plugin_runtime::launcher_update::startup_prepare_failure_reason()
                .unwrap_or_else(|| "启动准备失败".to_string())
        });
    Ok(SandboxUpdateState {
        status: status.as_str().to_string(),
        version,
        failure,
    })
}

#[derive(Debug, serde::Serialize)]
pub struct StartupPrepareResult {
    pub installed_version: Option<String>,
    /// 沙箱不可用等降级原因：应用仍可进入，仅插件工具受限。
    pub degraded_reason: Option<String>,
    /// 启动失败的插件清单（逐插件已标记异常状态，设置页可查）。
    pub plugin_failures: Vec<String>,
}

#[tauri::command]
pub async fn prepare_startup_resources(
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<StartupPrepareResult, String> {
    use tauri::Emitter as _;
    state.retry_failed_plugin_preload();
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await
        .map_err(|e| e.to_string())?;
    let check_root = storage_root.clone();
    let available = tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::launcher_update::launcher_available(&check_root)
    })
    .await
    .map_err(|error| error.to_string())?;
    if available {
        tiangong_plugin_runtime::launcher_update::record_startup_prepare_failure(None);
        // 插件级失败只随结果返回供前端提示，不阻断应用进入。这里的 Err
        // 仅剩"应用正在退出/加载锁损坏"两种语义：退出中无人消费结果，
        // 锁损坏属内部状态异常，同样降级放行只留日志。
        let readiness = state.wait_plugin_preload().await.unwrap_or_else(|error| {
            tracing::warn!(error, "插件预加载失败，应用降级进入");
            tiangong_plugin_runtime::registry::StartupPluginReadiness::default()
        });
        return Ok(StartupPrepareResult {
            installed_version: None,
            degraded_reason: None,
            plugin_failures: readiness.failures,
        });
    }
    tiangong_plugin_runtime::launcher_update::mark_launcher_preparing(true);
    let _ = app.emit(
        "startup-prepare-step",
        serde_json::json!({ "step": "launcher" }),
    );
    let result = tiangong_plugin_runtime::launcher_update::LauncherUpdater::new()
        .install_or_update(&storage_root)
        .await;
    tiangong_plugin_runtime::launcher_update::mark_launcher_preparing(false);
    match result {
        Ok(version) => {
            tiangong_plugin_runtime::launcher_update::record_startup_prepare_failure(None);
            // 安装期间的旧准备可能因 Launcher 缺失失败；先等它结束，再重试失败结果。
            let _ = state.wait_plugin_preload().await;
            state.retry_failed_plugin_preload();
            let readiness = state.wait_plugin_preload().await.unwrap_or_else(|error| {
                tracing::warn!(error, "插件预加载失败，应用降级进入");
                tiangong_plugin_runtime::registry::StartupPluginReadiness::default()
            });
            let _ = app.emit(
                "startup-prepare-step",
                serde_json::json!({ "step": "done", "version": version }),
            );
            Ok(StartupPrepareResult {
                installed_version: Some(version),
                degraded_reason: None,
                plugin_failures: readiness.failures,
            })
        }
        Err(error) => {
            let reason = format!("{error:#}");
            tracing::warn!(reason, "沙箱程序安装失败，应用降级进入");
            tiangong_plugin_runtime::launcher_update::record_startup_prepare_failure(Some(
                reason.clone(),
            ));
            // 失败原因经返回值同步交给前端提示：不放事件——主界面挂载
            // 晚于此处的 emit，事件在首启场景必然丢失。
            // 沙箱不可用不阻断应用：对话不依赖沙箱，插件工具在调用时
            // 各自报错，可在设置页修复后自动重试。
            let readiness = state.wait_plugin_preload().await.unwrap_or_else(|error| {
                tracing::warn!(error, "插件预加载失败，应用降级进入");
                tiangong_plugin_runtime::registry::StartupPluginReadiness::default()
            });
            Ok(StartupPrepareResult {
                installed_version: None,
                degraded_reason: Some(reason),
                plugin_failures: readiness.failures,
            })
        }
    }
}

/// 内置注入类环境变量屏蔽清单（环境变量管理 Modal 提示
/// "已由系统内置屏蔽"与保存去重用；与执行侧同源不漂移）。
#[derive(Debug, serde::Serialize)]
pub struct BuiltinEnvBlocklist {
    /// 精确变量名（匹配大小写不敏感）。
    pub exact: Vec<String>,
    /// 变量名前缀（LD_/DYLD_ 动态加载注入类）。
    pub prefixes: Vec<String>,
}

#[tauri::command]
pub fn get_builtin_env_blocklist() -> BuiltinEnvBlocklist {
    BuiltinEnvBlocklist {
        exact: tiangong_plugin_runtime::builtin_denied_env_keys()
            .iter()
            .map(|key| key.to_string())
            .collect(),
        prefixes: tiangong_plugin_runtime::builtin_denied_env_prefixes()
            .iter()
            .map(|prefix| prefix.to_string())
            .collect(),
    }
}

/// 校验并规范化命令环境变量屏蔽清单（后端为最终权威，任何调用来源
/// 都得到相同约束）：去空白与空项 → 校验变量名格式（非法整次拒绝并
/// 返回具体非法项，不静默丢弃）→ 大小写不敏感去重 → 剔除系统内置
/// 屏蔽项（精确名与 LD_/DYLD_ 前缀，系统已始终屏蔽，用户清单不冗余）。
fn normalize_env_blocklist(raw: Vec<String>) -> Result<Vec<String>, String> {
    let invalid: Vec<String> = raw
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty() && !is_valid_env_name(item))
        .map(|item| item.to_string())
        .collect();
    if !invalid.is_empty() {
        return Err(format!(
            "非法变量名：{}（须以字母或下划线开头，仅含字母、数字、下划线）",
            invalid.join("、")
        ));
    }
    let builtin_exact: std::collections::HashSet<String> =
        tiangong_plugin_runtime::builtin_denied_env_keys()
            .iter()
            .map(|key| key.to_ascii_lowercase())
            .collect();
    let builtin_prefixes = tiangong_plugin_runtime::builtin_denied_env_prefixes();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    Ok(raw
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .filter(|item| {
            let lower = item.to_ascii_lowercase();
            // 内置屏蔽项剔除：系统始终屏蔽，保存进用户清单只造成冗余。
            if builtin_exact.contains(&lower)
                || builtin_prefixes
                    .iter()
                    .any(|prefix| item.to_ascii_uppercase().starts_with(prefix))
            {
                return false;
            }
            seen.insert(lower)
        })
        .collect())
}

/// 环境变量名格式校验：^[A-Za-z_][A-Za-z0-9_]*$。
fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z' | 'A'..='Z' | '_'))
        && chars.all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_'))
}

#[tauri::command]
pub async fn set_command_env_blocklist(
    blocklist: Vec<String>,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let cleaned = normalize_env_blocklist(blocklist)?;
    let mut config = state
        .with_state_read(|core_state| Ok(core_state.config.clone()))
        .await?;
    if config.command_env_blocklist == cleaned {
        return Ok(());
    }
    config.command_env_blocklist = cleaned.clone();
    tiangong_config::registry::update(config.clone()).map_err(|error| error.to_string())?;
    state
        .with_state(|core_state| {
            core_state.config = config;
            Ok(())
        })
        .await?;
    tracing::info!(count = cleaned.len(), "用户已更新命令环境变量屏蔽清单");
    Ok(())
}

#[tauri::command]
pub async fn set_sandbox_disabled(
    disabled: bool,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let mut config = state
        .with_state_read(|core_state| Ok(core_state.config.clone()))
        .await?;
    if config.sandbox_disabled == disabled {
        return Ok(());
    }
    config.sandbox_disabled = disabled;
    tiangong_config::registry::update(config.clone()).map_err(|error| error.to_string())?;
    state
        .with_state(|core_state| {
            core_state.config = config;
            Ok(())
        })
        .await?;
    // 沙箱状态由 plugin-runtime 宿主执行层自主管理；它会停止当前按需
    // sidecar（含 terminal），下一次终端操作/工具调用按新开关重建。
    // OS 沙箱不能在存活进程上原地切换。
    tiangong_plugin_runtime::registry::on_sandbox_setting_changed();
    if disabled {
        tracing::warn!(
            "用户已关闭沙箱：所有按需 sidecar（含 terminal/command）将以完整用户权限启动"
        );
    } else {
        tracing::info!("用户已重新开启沙箱：所有按需 sidecar 将恢复系统沙箱");
    }
    Ok(())
}

/// 查询会话级对话模型引用（None 表示跟随路由默认）。
#[tauri::command]
pub async fn get_session_model(
    session_id: String,
    state: State<'_, TiangongApp>,
) -> Result<Option<String>, String> {
    Ok(state
        .core_manager
        .load_session(&session_id)
        .ok()
        .and_then(|session| session.model_ref))
}

/// 会话可切换的 Chat 模型列表与路由默认模型引用。
#[derive(serde::Serialize)]
pub struct SessionChatModelsView {
    /// 仅 Chat 能力的模型项（含所属服务提供方，供前端按提供方分组）。
    pub models: Vec<SessionChatModelItem>,
    /// 路由 Chat 槽位当前指向的注册表 key（未配置或不在注册表时 None）。
    pub default_ref: Option<String>,
}

/// 单个可选模型：注册表 key、模型显示名与所属服务提供方。
#[derive(serde::Serialize)]
pub struct SessionChatModelItem {
    /// 注册表 key（会话 model_ref 存的就是它）。
    pub key: String,
    /// 模型显示名（models.json 的 model 字段，缺失时前端回落到 key）。
    pub label: String,
    /// 所属服务提供方名称（models.json 的 provider 字段）。
    pub provider: String,
}

/// 列出会话可切换的 Chat 模型（数据源为宿主模型注册表与路由表）。
#[tauri::command]
pub async fn list_session_chat_models(
    state: State<'_, TiangongApp>,
) -> Result<SessionChatModelsView, String> {
    use tiangong_llm::models_config::{ModelCapability, RoutingSlot};
    let models = state
        .with_state_read(|core_state| Ok(core_state.config.models.clone()))
        .await?;
    let mut chat_models: Vec<SessionChatModelItem> = models
        .models
        .iter()
        .filter(|(_, entry)| entry.capabilities.contains(&ModelCapability::Chat))
        .map(|(key, entry)| SessionChatModelItem {
            key: key.clone(),
            label: entry.model.clone(),
            provider: entry.provider.clone(),
        })
        .collect();
    // HashMap 迭代顺序不稳定：先按提供方再按 key 排序，保证分组与组内顺序固定。
    chat_models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.key.cmp(&right.key))
    });
    let default_ref = models.routing.get(&RoutingSlot::Chat).and_then(|routed| {
        models
            .models
            .iter()
            .find(|(_, entry)| entry.provider == routed.provider && entry.model == routed.model)
            .map(|(key, _)| key.clone())
    });
    Ok(SessionChatModelsView {
        models: chat_models,
        default_ref,
    })
}

/// 切换会话级对话模型（models 注册表 key；None 恢复跟随路由默认）。
///
/// 轻量切换：只写入引用，**不切换执行端点**——端点切换推迟到下一次
/// 发消息（投递前的端点校正按引用完成）。这样用户切走又切回时端点
/// 从未变化。运行中不切换（core 忙即拒绝）。
#[tauri::command]
pub async fn set_session_model(
    session_id: String,
    model_ref: Option<String>,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let session_lock = state.session_send_lock(&session_id);
    let _send_guard = session_lock.lock_owned().await;
    // 选择时即校验：失效引用直接拒绝，不写进会话（切回「跟随默认」的
    // None 不需要校验，这是用户从失效状态自救的出口）。
    if let Some(key) = model_ref.as_deref().filter(|key| !key.trim().is_empty()) {
        state.core_manager.resolve_turn_model(Some(key))?;
    }
    // 与其他 ensure_core 调用点一致：新建 Core 必须启动流消费者，否则
    // 该会话的全部流式事件发不出去、界面哑掉。
    let (stream_tx, stream_rx) = std::sync::mpsc::channel::<tiangong_types::StreamEvent>();
    let ensured = state
        .ensure_core(&session_id, None, None, None, None, stream_tx)
        .await?;
    if ensured.is_new {
        start_stream_consumer(app, ensured.session_id.clone(), stream_rx);
    }
    state
        .core_manager
        .set_core_model_ref(&ensured.session_id, model_ref)
}

#[tauri::command]
pub async fn get_reasoning_effort(
    session_id: Option<String>,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    let (target_id, default_effort) = state
        .with_state_read(|core_state| {
            Ok((
                session_id.unwrap_or_else(|| core_state.active_session_id.clone()),
                core_state.agent_config.reasoning_effort,
            ))
        })
        .await?;
    Ok(state
        .inner()
        .core_manager
        .load_session(&target_id)
        .ok()
        .and_then(|session| session.reasoning_effort)
        .unwrap_or(default_effort)
        .as_str()
        .to_string())
}

#[tauri::command]
pub async fn set_reasoning_effort(
    effort: String,
    session_id: Option<String>,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let effort = tiangong_llm::request::ReasoningEffort::parse_flexible(&effort);
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => {
            state
                .with_state_read(|core_state| Ok(core_state.active_session_id.as_str().to_string()))
                .await?
        }
    };
    crate::session_ops::update_reasoning_effort(&state.core_manager, &session_id, Some(effort))
        .map_err(|error| error.to_string())?;

    // 只通知目标会话当前 Core；活跃 turn 的下一次模型请求使用新强度。
    // 不等待会话发送锁，也不刷新其他 Core 或插件。
    state
        .inner()
        .core_manager
        .set_core_reasoning_effort(&session_id, effort);
    Ok(())
}

#[tauri::command]
pub async fn get_provider_balance(
    provider_name: String,
    state: State<'_, TiangongApp>,
) -> Result<serde_json::Value, String> {
    let (base_url, api_key) = state
        .with_state_read(|core_state| {
            let models = &core_state.config.models;
            let provider = models
                .providers
                .get(&provider_name)
                .ok_or_else(|| anyhow::anyhow!("Provider '{provider_name}' 不存在"))?;
            let resolved_key =
                tiangong_llm::models_config::ModelsConfig::resolve_api_key(&provider.api_key);
            if resolved_key.trim().is_empty() {
                return Err(anyhow::anyhow!(
                    "Provider '{provider_name}' 的 API Key 未设置"
                ));
            }
            Ok((provider.base_url.clone(), resolved_key))
        })
        .await?;

    // 余额 API 挂在域名根路径下，需去掉 base_url 中的路径部分
    // e.g. https://api.deepseek.com/anthropic → https://api.deepseek.com/user/balance
    let trimmed = base_url.trim_end_matches('/');
    let origin = if let Some(scheme_end) = trimmed.find("://") {
        let rest = &trimmed[scheme_end + 3..];
        if let Some(slash_pos) = rest.find('/') {
            trimmed[..scheme_end + 3 + slash_pos].to_string()
        } else {
            trimmed.to_string()
        }
    } else if let Some(slash_pos) = trimmed.find('/') {
        trimmed[..slash_pos].to_string()
    } else {
        trimmed.to_string()
    };
    let url = format!("{origin}/user/balance");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("tiangong/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;
    let response = client
        .get(&url)
        .bearer_auth(&api_key)
        .send()
        .await
        .map_err(|e| format!("请求余额失败 ({url}): {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("查询余额失败: HTTP {status}, {body}"));
    }

    response
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("解析余额响应失败: {e}"))
}

/// 获取当前活跃的 Worker 列表
#[tauri::command]
pub async fn list_workers(state: State<'_, TiangongApp>) -> Result<Vec<serde_json::Value>, String> {
    state.with_state_read(|_core_state| Ok(Vec::new())).await
}

/// 检查 TTS 能力是否已配置
#[tauri::command]
pub async fn has_tts_capability(state: State<'_, TiangongApp>) -> Result<bool, String> {
    has_model_capability("tts".to_string(), state).await
}

/// 检查 STT 能力是否已配置
#[tauri::command]
pub async fn has_stt_capability(state: State<'_, TiangongApp>) -> Result<bool, String> {
    has_model_capability("stt".to_string(), state).await
}

/// 统一的能力可用性查询（基于配置快速检测）
#[tauri::command]
pub async fn has_model_capability(
    capability: String,
    state: State<'_, TiangongApp>,
) -> Result<bool, String> {
    let capability = parse_model_capability(&capability)?;
    state
        .with_state_read(|core_state| Ok(has_capability_in_state(core_state, capability)))
        .await
}

/// 获取所有能力的当前配置状态
#[tauri::command]
pub async fn get_available_capabilities(
    state: State<'_, TiangongApp>,
) -> Result<Vec<CapabilityAvailabilityInfo>, String> {
    use tiangong_llm::models_config::ModelCapability;

    state
        .with_state_read(|core_state| {
            Ok(ModelCapability::all()
                .iter()
                .map(|capability| CapabilityAvailabilityInfo {
                    key: capability.key().to_string(),
                    display_name: capability.display_name().to_string(),
                    enabled: has_capability_in_state(core_state, *capability),
                    routed_model: core_state
                        .config
                        .models
                        .routed_model(*capability)
                        .map(str::to_string),
                })
                .collect())
        })
        .await
}

/// App 装配通用插件，Manager 解析上下文并收集；不创建会话 Core。
async fn query_mention_groups(
    state: &TiangongApp,
    request: tiangong_types::MentionRequest,
) -> Result<Vec<MentionGroup>, String> {
    state.wait_plugin_preload().await?;
    let manager = state.core_manager.clone();
    tokio::task::spawn_blocking(move || {
        // 查询入口刷新句柄，覆盖安装工具/后台升级等非 GUI 变更来源。
        manager.set_mention_sources(tiangong_plugin_runtime::registry::mention_sources());
        manager.query_mentions(request)
    })
    .await
    .map_err(|e| format!("mention 查询任务失败: {e}"))?
}

/// 兼容平铺入口；缺省仅查询全局候选。
#[tauri::command]
pub async fn get_mention_candidates(
    state: State<'_, TiangongApp>,
    request: Option<tiangong_types::MentionRequest>,
) -> Result<Vec<MentionCandidate>, String> {
    Ok(query_mention_groups(&state, request.unwrap_or_default())
        .await?
        .into_iter()
        .flat_map(|group| group.candidates)
        .collect())
}

/// 按会话上下文查询并分组；保留旧参数，搜索发生在截断前。
#[tauri::command]
pub async fn get_mention_groups(
    state: State<'_, TiangongApp>,
    request: Option<tiangong_types::MentionRequest>,
    allowed_kinds: Option<Vec<String>>,
    max_per_group: Option<usize>,
) -> Result<Vec<MentionGroup>, String> {
    let mut request = request.unwrap_or_default();
    if let Some(kinds) = allowed_kinds {
        request.allowed_kinds = kinds;
    }
    if let Some(limit) = max_per_group {
        request.max_per_group = limit;
    }
    query_mention_groups(&state, request).await
}

/// 获取输入框缓存。
#[tauri::command]
pub async fn get_input_cache(
    cache_key: String,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_app_state::app_state::InputCache, String> {
    if cache_key.trim().is_empty() {
        return Err("输入缓存键不能为空".to_string());
    }
    state
        .with_state_read(|core_state| Ok(crate::state_ops::input_cache(core_state, &cache_key)))
        .await
}

/// 设置输入框缓存。
#[tauri::command]
pub async fn set_input_cache(
    cache_key: String,
    mut cache: tiangong_app_state::app_state::InputCache,
    claim_revision: Option<u64>,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_app_state::app_state::InputCache, String> {
    if cache_key.trim().is_empty() {
        return Err("输入缓存键不能为空".to_string());
    }
    if state.input_cache_was_discarded(&cache_key) {
        return Err("该输入缓存已被丢弃".to_string());
    }
    let cache_guard = state.input_cache_update_lock(&cache_key).lock_owned().await;
    if state.input_cache_was_discarded(&cache_key) {
        return Err("该输入缓存已被丢弃".to_string());
    }
    let current = state
        .with_state_read(|core_state| Ok(crate::state_ops::input_cache(core_state, &cache_key)))
        .await?;
    if cache.revision < current.revision {
        return Ok(current);
    }

    let old_attachments = current.attachments.clone();
    let mut transaction = None;
    if same_input_attachment_selection(&cache.attachments, &current.attachments) {
        cache.attachments = current.attachments;
    } else if !cache.attachments.is_empty() {
        let raw = std::mem::take(&mut cache.attachments);
        let staged = tokio::task::spawn_blocking(move || {
            tiangong_media_archive::AttachmentStore::default().store_batch(raw)
        })
        .await
        .map_err(|error| format!("输入附件保存任务失败：{error}"))?
        .map_err(|error| format!("输入附件保存失败：{error}"))?;
        cache.attachments = staged
            .stored()
            .iter()
            .map(|attachment| tiangong_media_archive::RawAttachment {
                kind: attachment.kind,
                source: attachment.local_path.clone(),
                mime_type: Some(attachment.mime_type.clone()),
                original_name: Some(attachment.original_name.clone()),
            })
            .collect();
        transaction = Some(staged);
    }

    let (stored, applied) = state
        .with_state(|core_state| crate::state_ops::set_input_cache(core_state, &cache_key, cache))
        .await?;
    if applied {
        if let Some(transaction) = transaction.take() {
            transaction.commit();
        }
    }
    // stale 写入未被采用时，transaction 在此作用域结束自动回滚新归档文件。
    drop(transaction);

    let mut cleanup_candidates = if applied { old_attachments } else { Vec::new() };
    let mut claim_error = None;
    if let Some(revision) = claim_revision {
        if stored.revision != revision {
            claim_error = Some(format!(
                "输入已在发送前更新（发送 revision={revision}，当前 revision={}）",
                stored.revision
            ));
        } else {
            let attachment_paths = stored
                .attachments
                .iter()
                .map(|attachment| attachment.source.clone())
                .collect();
            match state.register_input_send_claim(&cache_key, revision, attachment_paths) {
                Ok(replaced_paths) => {
                    cleanup_candidates.extend(raw_attachments_for_paths(replaced_paths));
                }
                Err(error) => claim_error = Some(error),
            }
        }
    }
    // 输入缓存与租约已更新/登记，清理等待 send lock 前释放缓存锁，
    // 使慢发送期间的 R+1/R+2 新输入仍能按会话串行并立即同步。
    drop(cache_guard);
    // 输入缓存已先更新；文件清理再等待该会话发送事务结束，避免用户在慢发送期间
    // 删除附件时把正在投递的稳定文件提前移除。
    let cleanup_lock = state.session_send_lock(&cache_key);
    let _cleanup_guard = cleanup_lock.lock().await;
    cleanup_unreferenced_input_attachments(state.inner(), cleanup_candidates).await;
    if let Some(error) = claim_error {
        return Err(error);
    }
    Ok(stored)
}

fn same_input_attachment_selection(
    incoming: &[tiangong_media_archive::RawAttachment],
    current: &[tiangong_media_archive::RawAttachment],
) -> bool {
    incoming == current
}

pub(crate) async fn cleanup_unreferenced_input_attachments(
    state: &TiangongApp,
    candidates: Vec<tiangong_media_archive::RawAttachment>,
) {
    if candidates.is_empty() {
        return;
    }
    let claimed_paths = state.claimed_input_attachment_paths();
    // 引用扫描需要遍历消息内容（完整 Session 字段）。session 真相源归磁盘
    //（issue #245）：先在锁外拿到会话 id 列表，再逐个从磁盘 load 扫描媒体引用，
    // 避免在 app-state 锁内做文件 IO。
    let session_ids: Vec<String> = state
        .with_state_read(|core_state| {
            Ok(core_state
                .core_manager
                .list_session_metadata()
                .iter()
                .map(|metadata| metadata.id.clone())
                .collect::<Vec<_>>())
        })
        .await
        .unwrap_or_default();
    let mut referenced = state
        .with_state_read(|core_state| {
            let mut paths = claimed_paths;
            for cache in core_state.input_caches.values() {
                paths.extend(cache.attachments.iter().map(|item| item.source.clone()));
            }
            Ok(paths)
        })
        .await
        .unwrap_or_default();

    // 从磁盘加载完整会话，扫描媒体引用（真相源归磁盘）。
    for session_id in &session_ids {
        let Ok(session) = state.core_manager.load_session(session_id) else {
            continue;
        };
        for message in &session.messages {
            referenced.extend(
                message
                    .extract_media_assets()
                    .into_iter()
                    .map(|item| item.url),
            );
        }
    }

    let referenced = referenced
        .into_iter()
        .filter_map(|path| tiangong_media_archive::canonical_archived_media_path(&path))
        .collect::<std::collections::HashSet<_>>();
    let mut checked = std::collections::HashSet::new();
    for candidate in candidates {
        let Some(path) = tiangong_media_archive::canonical_archived_media_path(&candidate.source)
        else {
            continue;
        };
        if !checked.insert(path.clone()) || referenced.contains(&path) {
            continue;
        }
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), error = %error, "清理未引用输入附件失败");
        }
    }
}

pub(crate) fn raw_attachments_for_paths(
    paths: Vec<String>,
) -> Vec<tiangong_media_archive::RawAttachment> {
    paths
        .into_iter()
        .map(|source| tiangong_media_archive::RawAttachment {
            kind: tiangong_types::MediaKind::File,
            source,
            mime_type: None,
            original_name: None,
        })
        .collect()
}

/// 为尚未落盘的新会话生成稳定 SCRU128 ID。这里只分配 ID，不创建 Session。
#[tauri::command]
pub fn new_session_id() -> String {
    scru128::new().to_string()
}

// ============================================================================
// 物理删除（配置页触发）
// ============================================================================

/// 批量删除结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeleteResult {
    /// 实际删除成功的会话 ID 列表。
    pub succeeded: Vec<String>,
    /// 删除失败的会话 ID 列表。
    pub failed: Vec<String>,
}

/// 回收区中的会话摘要（配置页展示用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrashedSession {
    pub id: String,
    pub title: String,
    pub message_count: usize,
    pub updated_at: String,
    /// 是否正在清理中（资源可能已部分删除，不可恢复）。
    pub purging: bool,
}

/// 物理清理进度事件 payload。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PurgeProgress {
    pub current: usize,
    pub total: usize,
    pub session_id: String,
    pub title: String,
    pub status: String, // "cleaning" | "done" | "error"
}

/// 从磁盘 JSON 读取回收区会话摘要（只读浅字段）。
fn read_trashed_summary(id: &str, path: &std::path::Path, purging: bool) -> TrashedSession {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .map(|value| TrashedSession {
            id: id.to_string(),
            title: value
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("（无标题）")
                .to_string(),
            message_count: value
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("user"))
                        .count()
                })
                .unwrap_or(0),
            updated_at: value
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            purging,
        })
        .unwrap_or_else(|| TrashedSession {
            id: id.to_string(),
            title: "（无法读取）".to_string(),
            message_count: 0,
            updated_at: String::new(),
            purging,
        })
}

/// 扫描回收区，返回待物理清理的会话列表。
#[tauri::command]
pub async fn list_trashed_sessions(
    state: State<'_, TiangongApp>,
) -> Result<Vec<TrashedSession>, String> {
    let manager = state.core_manager.clone();
    tokio::task::spawn_blocking(move || {
        let storage_root = manager.storage_root();
        // 扫描回收区（可恢复）和正在清理（不可恢复）两个目录。
        let trashed_ids = manager.list_trashed_session_ids();
        let purging_ids = manager.list_purging_session_ids();
        let mut result = Vec::new();
        // 回收区会话（可恢复）
        for id in &trashed_ids {
            let path = storage_root
                .join("trash")
                .join("sessions")
                .join(format!("{id}.json"));
            result.push(read_trashed_summary(id, &path, false));
        }
        // 正在清理会话（不可恢复，资源可能已部分删除）
        for id in &purging_ids {
            let path = storage_root
                .join("trash")
                .join("purging")
                .join(format!("{id}.json"));
            result.push(read_trashed_summary(id, &path, true));
        }
        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(result)
    })
    .await
    .map_err(|error| format!("扫描回收区失败：{error}"))?
}

/// 从回收区恢复单个会话（trash/sessions/ → sessions/）。
#[tauri::command]
pub async fn restore_deleted_session(
    session_id: String,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    use tauri::Emitter;
    let manager = state.core_manager.clone();
    tokio::task::spawn_blocking(move || manager.restore_session_file(&session_id))
        .await
        .map_err(|error| format!("恢复任务失败：{error}"))??;
    // 恢复后会话重新出现在列表中，通知前端刷新。
    let _ = app.emit("sessions_updated", &());
    Ok(())
}

/// 发送清理失败进度事件。
fn emit_error(app: &AppHandle, index: usize, total: usize, session_id: &str) {
    use tauri::Emitter;
    let _ = app.emit(
        "purge_progress",
        PurgeProgress {
            current: index + 1,
            total,
            session_id: session_id.to_string(),
            title: String::new(),
            status: "error".to_string(),
        },
    );
}

/// 物理清理所有回收区会话（配置页"全部清理"触发）。
///
/// 逐个清理 media/teams/layout/PTY/browser/trash 文件，通过 `purge_progress`
/// 事件推送进度。已清理的会话不会重复清理。
#[tauri::command]
pub async fn purge_all_deleted_sessions(
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<usize, String> {
    use tauri::Emitter;

    let manager = state.core_manager.clone();
    // 合并待清理列表：回收区中的 + 上次清理失败残留在 purging 中的。
    let trashed_ids = manager.list_trashed_session_ids();
    let purging_ids = manager.list_purging_session_ids();
    let all_ids: Vec<String> = trashed_ids
        .iter()
        .chain(purging_ids.iter())
        .cloned()
        .collect();
    let total = all_ids.len();
    if total == 0 {
        return Ok(0);
    }

    let storage_root = manager.storage_root().to_path_buf();
    let media_root = storage_root.join("media");
    let app_handle = app.clone();

    let mut succeeded: Vec<String> = Vec::new();
    for (index, session_id) in all_ids.iter().enumerate() {
        let _ = app_handle.emit(
            "purge_progress",
            PurgeProgress {
                current: index,
                total,
                session_id: session_id.clone(),
                title: String::new(),
                status: "cleaning".to_string(),
            },
        );

        // 阶段 1：移到 purging（原子移动，标记"正在清理"）。
        // 已在 purging 中的（上次失败残留）跳过移动。
        if trashed_ids.contains(session_id) {
            if let Err(e) = manager.move_to_purging(session_id) {
                warn!(%e, session_id, "移动到 purging 失败");
                emit_error(&app_handle, index, total, session_id);
                continue;
            }
        }

        // 阶段 2：清理全部资源。
        if let Some(host_state) = app.try_state::<crate::webview_host::WebviewHostState>() {
            host_state.registry.destroy_session(session_id);
        }
        // media/teams（占用空间最大，必须删成功）
        if let Err(error) = purge_session_resources(&storage_root, &media_root, session_id) {
            warn!(%error, session_id, "物理清理失败，保留 purging 记录供重试");
            emit_error(&app_handle, index, total, session_id);
            continue;
        }

        // 阶段 3：全部资源清理成功 → 删 purging 记录。
        // 记录删除也是清理完成的一部分；失败时保留记录供下次重试，不能误报成功。
        if let Err(e) = manager.delete_purging_session(session_id) {
            warn!(%e, session_id, "删除 purging 记录失败，保留记录供重试");
            emit_error(&app_handle, index, total, session_id);
            continue;
        }

        succeeded.push(session_id.clone());

        let _ = app_handle.emit(
            "purge_progress",
            PurgeProgress {
                current: index + 1,
                total,
                session_id: session_id.clone(),
                title: String::new(),
                status: "done".to_string(),
            },
        );
    }

    Ok(succeeded.len())
}

/// 清理单个会话的磁盘资源（media/teams/trash 文件）。不含 PTY/browser 运行时。
fn purge_session_resources(
    storage_root: &std::path::Path,
    media_root: &std::path::Path,
    session_id: &str,
) -> Result<(), String> {
    // 删媒体目录（按会话隔离的新版结构）。
    let session_media = media_root.join(session_id);
    if session_media.exists() {
        std::fs::remove_dir_all(&session_media)
            .map_err(|error| format!("删除媒体目录失败：{error}"))?;
    }
    // 删 teams 目录。
    let teams_dir = storage_root.join("teams").join(session_id);
    if teams_dir.exists() {
        std::fs::remove_dir_all(&teams_dir)
            .map_err(|error| format!("删除 teams 目录失败：{error}"))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn remove_input_cache(
    cache_key: String,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let cache_lock = state.input_cache_update_lock(&cache_key);
    let _cache_guard = cache_lock.lock_owned().await;
    let send_lock = state.session_send_lock(&cache_key);
    let _send_guard = send_lock.lock_owned().await;
    let mut attachments = state
        .with_state_read(|core_state| {
            Ok(crate::state_ops::input_cache(core_state, &cache_key).attachments)
        })
        .await?;
    state
        .with_state(|core_state| {
            core_state.input_caches.remove(&cache_key);
            Ok(())
        })
        .await?;
    attachments.extend(raw_attachments_for_paths(
        state.release_any_input_send_claim(&cache_key),
    ));
    state.mark_input_cache_discarded(&cache_key);
    cleanup_unreferenced_input_attachments(state.inner(), attachments).await;
    Ok(())
}

/// 获取 Desktop 工作空间目录
#[tauri::command]
pub async fn get_workspace_dir(state: State<'_, TiangongApp>) -> Result<String, String> {
    state
        .with_state_read(|core_state| Ok(core_state.workspace_dir.as_str().to_string()))
        .await
}

/// 设置 Desktop 工作空间目录
#[tauri::command]
pub async fn set_workspace_dir(
    workspace_dir: String,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let path = std::path::Path::new(&workspace_dir);
    if !path.is_dir() {
        return Err(format!("路径不存在或不是目录：{workspace_dir}"));
    }
    let mut config = state
        .with_state_read(|core_state| Ok(core_state.config.clone()))
        .await?;
    config.workspace_dir = workspace_dir.clone();
    #[cfg(windows)]
    let storage_root = config.storage_root.clone();
    tiangong_config::registry::update(config.clone()).map_err(|error| error.to_string())?;
    state
        .with_state(|core_state| {
            core_state.config = config;
            crate::session_ops::update_workspace_dir(
                core_state,
                &state.core_manager,
                workspace_dir.clone(),
            )
        })
        .await?;

    // cwd 由 app-state 快照维护，下次 turn 从快照重载，无需投递到 worker。

    #[cfg(windows)]
    tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::registry::invalidate_persistent_grants(&storage_root)
            .map_err(|error| format!("工作区已保存，但旧授权清理或插件重载失败：{error:#}"))
    })
    .await
    .map_err(|error| error.to_string())??;

    Ok(())
}

/// 设置指定会话的工作目录
#[tauri::command]
pub async fn set_session_cwd(
    session_id: String,
    cwd: String,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    if session_id.trim().is_empty() {
        return Err("会话 ID 不能为空".to_string());
    }
    let path = std::path::Path::new(&cwd);
    if !path.is_dir() {
        return Err(format!("路径不存在或不是目录：{cwd}"));
    }
    crate::session_ops::update_session_cwd(&state.core_manager, &session_id, cwd)
        .map_err(|error| error.to_string())?;

    // cwd 由 app-state 快照维护，下次 turn 从快照重载，无需投递到 worker。

    Ok(())
}

// ============================================================================
// MCP 管理
// ============================================================================

/// 经运行时 sidecar 通道调用 MCP 插件操作（Desktop 入口）。
fn mcp_invoke(operation: &str, payload: serde_json::Value) -> Result<serde_json::Value, String> {
    let storage_root = tiangong_config::io::storage_root();
    tiangong_plugin_runtime::registry::invoke_sidecar(&storage_root, "mcp", operation, payload)
        .map_err(|e| e.to_string())
}

/// 获取 MCP 服务器列表
#[tauri::command]
pub async fn get_mcp_servers(_state: State<'_, TiangongApp>) -> Result<Vec<McpServerView>, String> {
    let servers: Vec<tiangong_plugin_mcp_protocol::config::McpServerConfig> =
        serde_json::from_value::<tiangong_plugin_mcp_protocol::management::ServersResponse>(
            mcp_invoke("mcp.server.list", serde_json::json!({}))?,
        )
        .map_err(|e: serde_json::Error| e.to_string())?
        .servers;
    Ok(servers.iter().map(McpServerView::from_core).collect())
}

/// 获取 MCP 服务器健康状态
#[tauri::command]
pub async fn get_mcp_health(
    _state: State<'_, TiangongApp>,
) -> Result<Vec<serde_json::Value>, String> {
    let response: tiangong_plugin_mcp_protocol::query::HealthResponse =
        serde_json::from_value(mcp_invoke("mcp.server.health", serde_json::json!({}))?)
            .map_err(|e: serde_json::Error| e.to_string())?;
    response
        .statuses
        .into_iter()
        .map(|s| serde_json::to_value(s).map_err(|e| e.to_string()))
        .collect()
}

/// 探测单个 MCP 服务器（按 name），写回健康缓存。供前端添加/编辑/重试后刷新该行。
#[tauri::command]
pub async fn probe_mcp_server(name: String, _state: State<'_, TiangongApp>) -> Result<(), String> {
    mcp_invoke(
        "mcp.server.probe",
        serde_json::to_value(tiangong_plugin_mcp_protocol::query::ServerNameRequest { name })
            .map_err(|e| e.to_string())?,
    )?;
    Ok(())
}

/// 注册 MCP 服务器
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn register_mcp_server(
    name: String,
    command: String,
    args: Vec<String>,
    transport: Option<String>,
    endpoint: Option<String>,
    auth_header: Option<String>,
    headers: Option<std::collections::HashMap<String, String>>,
    env: Option<std::collections::HashMap<String, String>>,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    use tiangong_plugin_mcp_protocol::config::{
        McpTransportMode, RegisterMcpServerOptions, RegisterMcpServerRequest,
    };

    let transport = match transport
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "auto" => Some(McpTransportMode::Auto),
            "stdio" => Some(McpTransportMode::Stdio),
            "http" | "sse" | "streamablehttp" | "streamable_http" | "streamable-http" => {
                Some(McpTransportMode::Http)
            }
            other => {
                return Err(format!(
                    "不支持的 MCP transport：{other}，支持 auto/stdio/http/sse"
                ));
            }
        },
        None => None,
    };

    let header_vec = headers.unwrap_or_default().into_iter().collect();
    let env_vec = env.unwrap_or_default().into_iter().collect();
    let request = RegisterMcpServerRequest {
        name,
        command,
        args,
        tags: vec![],
        enabled: true,
        options: RegisterMcpServerOptions {
            transport,
            endpoint,
            auth_header,
            headers: header_vec,
            env: env_vec,
        },
    };
    let response: tiangong_plugin_mcp_protocol::MessageResponse =
        serde_json::from_value(mcp_invoke(
            "mcp.server.register",
            serde_json::to_value(&request).map_err(|e| e.to_string())?,
        )?)
        .map_err(|e: serde_json::Error| e.to_string())?;
    state.sync_core_config_from_state().await?;
    Ok(response.message)
}

/// 编辑 MCP 服务器（按 name 定位，name 自身不可改）
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn update_mcp_server(
    name: String,
    command: String,
    args: Vec<String>,
    transport: Option<String>,
    endpoint: Option<String>,
    auth_header: Option<String>,
    headers: Option<std::collections::HashMap<String, String>>,
    env: Option<std::collections::HashMap<String, String>>,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    use tiangong_plugin_mcp_protocol::config::{
        McpTransportMode, RegisterMcpServerOptions, RegisterMcpServerRequest,
    };
    use tiangong_plugin_mcp_protocol::management::UpdateServerRequest;

    let transport = match transport
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "auto" => Some(McpTransportMode::Auto),
            "stdio" => Some(McpTransportMode::Stdio),
            "http" | "sse" | "streamablehttp" | "streamable_http" | "streamable-http" => {
                Some(McpTransportMode::Http)
            }
            other => {
                return Err(format!(
                    "不支持的 MCP transport：{other}，支持 auto/stdio/http/sse"
                ));
            }
        },
        None => None,
    };

    let header_vec = headers.unwrap_or_default().into_iter().collect();
    let env_vec = env.unwrap_or_default().into_iter().collect();
    let request = UpdateServerRequest {
        name: name.clone(),
        request: RegisterMcpServerRequest {
            name: name.clone(),
            command,
            args,
            tags: vec![],
            enabled: true,
            options: RegisterMcpServerOptions {
                transport,
                endpoint,
                auth_header,
                headers: header_vec,
                env: env_vec,
            },
        },
    };
    let response: tiangong_plugin_mcp_protocol::MessageResponse =
        serde_json::from_value(mcp_invoke(
            "mcp.server.update",
            serde_json::to_value(&request).map_err(|e| e.to_string())?,
        )?)
        .map_err(|e: serde_json::Error| e.to_string())?;
    state.sync_core_config_from_state().await?;
    Ok(response.message)
}

/// 移除 MCP 服务器
#[tauri::command]
pub async fn remove_mcp_server(
    name: String,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    use tiangong_plugin_mcp_protocol::management::RemoveServerRequest;
    let response: tiangong_plugin_mcp_protocol::MessageResponse =
        serde_json::from_value(mcp_invoke(
            "mcp.server.remove",
            serde_json::to_value(RemoveServerRequest { name }).map_err(|e| e.to_string())?,
        )?)
        .map_err(|e: serde_json::Error| e.to_string())?;
    state.sync_core_config_from_state().await?;
    Ok(response.message)
}

/// 设置 MCP 服务器启用状态
#[tauri::command]
pub async fn set_mcp_server_enabled(
    name: String,
    enabled: bool,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    use tiangong_plugin_mcp_protocol::management::SetEnabledRequest;
    let response: tiangong_plugin_mcp_protocol::MessageResponse =
        serde_json::from_value(mcp_invoke(
            "mcp.server.set_enabled",
            serde_json::to_value(SetEnabledRequest { name, enabled }).map_err(|e| e.to_string())?,
        )?)
        .map_err(|e: serde_json::Error| e.to_string())?;
    state.sync_core_config_from_state().await?;
    Ok(response.message)
}

// ============================================================================
// 移动端控制（bot）管理
// ============================================================================

fn validate_bot_id(id: String) -> Result<tiangong_bots::BotId, String> {
    tiangong_bots::BotId::try_from(id).map_err(|error| error.to_string())
}

/// 获取已注册的 bot 列表
#[tauri::command]
pub async fn bot_list(
    state: State<'_, TiangongApp>,
) -> Result<Vec<tiangong_bots::BotConfig>, String> {
    Ok(state.bot_store.list())
}

/// 获取指定 bot 的健康状态
#[tauri::command]
pub async fn bot_health(
    id: String,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_bots::BotHealth, String> {
    let id = validate_bot_id(id)?;
    Ok(state.bot_runtime.health(&id).await)
}

/// 获取指定 bot 当前日志文件的最近内容。
#[tauri::command]
pub async fn bot_log(
    id: String,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_bots::BotLog, String> {
    let id = validate_bot_id(id)?;
    if state.bot_store.get(&id).is_none() {
        return Err(format!("bot 不存在：{id}"));
    }
    tiangong_bots::read_log_tail(&id).map_err(|err| format!("读取 bot 日志失败：{err}"))
}

/// 获取 bot 的配置字段 schema（供前端动态渲染表单）。
///
/// schema 权威来源是 bot 二进制 `--describe` 上报（缓存在 ~/.tiangong/bots/<id>/schema.json）。
/// 优先读缓存；若该 bot 实例尚未安装制品，从 bots-index.json 的预览 schema 读取。
#[tauri::command]
pub async fn bot_config_schema(
    bot_id: Option<String>,
    artifact_id: String,
    state: State<'_, TiangongApp>,
) -> Result<Vec<tiangong_bots::ConfigFieldSchema>, String> {
    let bot_id = bot_id.map(validate_bot_id).transpose()?;
    let local_artifacts = state.bot_runtime.scan_local_artifacts();

    // 1) 本地自有 bot 没有版本记录，每次打开配置时重新读取 --describe。
    if let Some(id) = &bot_id {
        if local_artifacts
            .iter()
            .any(|local| local.id == id.as_str() && local.version.is_empty())
        {
            return tiangong_bots::describe_and_cache(id)
                .await
                .map_err(|err| format!("读取本地 Bot 配置失败：{err}"));
        }
        // 线上安装的 bot 优先使用安装时验证并缓存的 schema。
        if let Some(schema) = tiangong_bots::cached_schema(id) {
            return Ok(schema);
        }
    }
    // 2) 扫描本地制品；没有缓存时直接调用本地 bot --describe。
    for local in local_artifacts {
        if local.artifact_id == artifact_id {
            let local_id = validate_bot_id(local.id)?;
            if !local.version.is_empty() {
                if let Some(schema) = tiangong_bots::cached_schema(&local_id) {
                    return Ok(schema);
                }
            }
            return tiangong_bots::describe_and_cache(&local_id)
                .await
                .map_err(|err| format!("读取本地 Bot 配置失败：{err}"));
        }
    }
    // 3) 回退到 bots-index.json 的预览 schema（安装前展示）。
    match state.bot_runtime.fetch_index().await {
        Ok(index) => {
            let manifest = index
                .bots
                .into_iter()
                .find(|m| m.id == artifact_id)
                .ok_or_else(|| format!("未找到制品：{artifact_id}"))?;
            Ok(manifest.config_schema)
        }
        Err(e) => Err(format!("获取 schema 失败（线上不可达且本地无匹配）：{e}")),
    }
}

/// 为支持扫码配置的 bot 创建授权会话。
#[tauri::command]
pub async fn bot_provision_begin(
    bot_id: String,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_bots::QrSession, String> {
    let bot_id = validate_bot_id(bot_id)?;
    state
        .bot_runtime
        .provision_begin(&bot_id)
        .await
        .map_err(|err| err.to_string())
}

/// 轮询 bot 扫码授权状态。
#[tauri::command]
pub async fn bot_provision_poll(
    bot_id: String,
    session: tiangong_bots::QrSession,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_bots::ProvisionStatus, String> {
    let bot_id = validate_bot_id(bot_id)?;
    state
        .bot_runtime
        .provision_poll(&bot_id, &session)
        .await
        .map_err(|err| err.to_string())
}

/// 拉取远端 bots-index.json（可安装的 bot 列表）。
///
/// 线上不可达时返回明确错误；前端仍会独立加载并展示本地制品。
#[tauri::command]
pub async fn bot_available(
    state: State<'_, TiangongApp>,
) -> Result<tiangong_bots::BotsIndex, String> {
    state
        .bot_runtime
        .fetch_index()
        .await
        .map_err(|err| format!("加载线上 Bot 目录失败：{err}"))
}

/// 扫描本地已安装的制品（`~/.tiangong/bots/*/`）。
///
/// 不依赖线上 bots-index——本地已放置的 bot 二进制 + schema.json 即可被发现。
#[tauri::command]
pub async fn bot_scan_local(
    state: State<'_, TiangongApp>,
) -> Result<Vec<tiangong_bots::LocalArtifact>, String> {
    Ok(state.bot_runtime.scan_local_artifacts())
}

/// 获取 Bot 已发现的主动推送目标。
#[tauri::command]
pub async fn bot_push_targets(
    id: String,
    state: State<'_, TiangongApp>,
) -> Result<Vec<tiangong_bots::PushTargetView>, String> {
    let id = validate_bot_id(id)?;
    if state.bot_store.get(&id).is_none() {
        return Err(format!("bot 不存在：{id}"));
    }
    state
        .bot_runtime
        .push_targets(&id)
        .await
        .map_err(|error| error.to_string())
}

/// 删除一个 Bot 主动推送授权目标。
#[tauri::command]
pub async fn bot_delete_push_target(
    id: String,
    target_id: String,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    let id = validate_bot_id(id)?;
    if state.bot_store.get(&id).is_none() {
        return Err(format!("bot 不存在：{id}"));
    }
    state
        .bot_runtime
        .delete_push_target(&id, &target_id)
        .await
        .map_err(|error| error.to_string())?;
    Ok("推送授权已删除".to_string())
}

fn bot_mcp_connection_matches(
    existing: &tiangong_plugin_mcp_protocol::config::McpServerConfig,
    generated: &tiangong_bots::BotMcpConfig,
) -> bool {
    use tiangong_plugin_mcp_protocol::config::ResolvedMcpTransport;

    existing.resolved_transport() == ResolvedMcpTransport::Stdio
        && existing.command == generated.command
        && existing.args == generated.args
        && existing.endpoint.is_empty()
        && existing.auth_header.is_empty()
        && existing.headers.is_empty()
        && existing.env.is_empty()
        && existing.tags == generated.tags
}

fn bot_mcp_registration_request(
    generated: &tiangong_bots::BotMcpConfig,
    enabled: bool,
) -> tiangong_plugin_mcp_protocol::config::RegisterMcpServerRequest {
    use tiangong_plugin_mcp_protocol::config::{
        McpTransportMode, RegisterMcpServerOptions, RegisterMcpServerRequest,
    };

    RegisterMcpServerRequest {
        name: generated.name.clone(),
        command: generated.command.clone(),
        args: generated.args.clone(),
        tags: generated.tags.clone(),
        enabled,
        options: RegisterMcpServerOptions {
            transport: Some(McpTransportMode::Stdio),
            ..Default::default()
        },
    }
}

/// 查询当前 MCP server 列表（经 sidecar 通道）。
fn list_mcp_servers_via_sidecar(
    _state: &TiangongApp,
) -> Result<Vec<tiangong_plugin_mcp_protocol::config::McpServerConfig>, String> {
    serde_json::from_value::<tiangong_plugin_mcp_protocol::management::ServersResponse>(mcp_invoke(
        "mcp.server.list",
        serde_json::json!({}),
    )?)
    .map_err(|e: serde_json::Error| e.to_string())
    .map(|r| r.servers)
}

/// 经运行时 sidecar 通道调用 MCP 插件操作（&TiangongApp 重载，复用 mcp_invoke）。
fn mcp_invoke_state(
    _state: &TiangongApp,
    operation: &str,
    payload: serde_json::Value,
) -> Result<serde_json::Value, String> {
    mcp_invoke(operation, payload)
}

/// 为支持 MCP 的 Bot 确保对应普通 MCP 已注册并启用。
pub async fn ensure_bot_mcp_registered(
    id: &tiangong_bots::BotId,
    state: &TiangongApp,
) -> Result<Option<String>, String> {
    use tiangong_plugin_mcp_protocol::management::{
        SetEnabledRequest, SERVER_SET_ENABLED_OPERATION,
    };
    let supports_mcp = state
        .bot_runtime
        .supports_mcp(id)
        .await
        .map_err(|error| format!("读取 Bot MCP 能力失败：{error}"))?;
    if !supports_mcp {
        return Ok(None);
    }

    let generated = state
        .bot_runtime
        .generate_mcp_config(id)
        .await
        .map_err(|error| format!("生成 Bot MCP 配置失败：{error}"))?;
    let name = generated.name.clone();

    if let Some(existing) = list_mcp_servers_via_sidecar(state)?
        .into_iter()
        .find(|server| server.name == name)
    {
        if !bot_mcp_connection_matches(&existing, &generated) {
            return Err(format!("MCP 名称 {name} 已被其他配置占用，未自动覆盖"));
        }
        if !existing.enabled {
            let response: tiangong_plugin_mcp_protocol::MessageResponse =
                serde_json::from_value(mcp_invoke_state(
                    state,
                    SERVER_SET_ENABLED_OPERATION,
                    serde_json::to_value(SetEnabledRequest {
                        name: name.clone(),
                        enabled: true,
                    })
                    .map_err(|e| e.to_string())?,
                )?)
                .map_err(|e: serde_json::Error| e.to_string())?;
            if let Err(sync_error) = state.sync_core_config_from_state().await {
                let rollback = mcp_invoke_state(
                    state,
                    SERVER_SET_ENABLED_OPERATION,
                    serde_json::to_value(SetEnabledRequest {
                        name: name.clone(),
                        enabled: false,
                    })
                    .unwrap_or_default(),
                )
                .map(|_| "已恢复为停用".to_string())
                .unwrap_or_else(|error| format!("恢复停用失败：{error}"));
                return Err(format!("同步 MCP 配置失败：{sync_error}；{rollback}"));
            }
            return Ok(Some(response.message));
        }
        return Ok(Some(format!("MCP 已注册：{name}")));
    }

    let request = bot_mcp_registration_request(&generated, generated.enabled);
    let response: tiangong_plugin_mcp_protocol::MessageResponse =
        serde_json::from_value(mcp_invoke_state(
            state,
            "mcp.server.register",
            serde_json::to_value(&request).map_err(|e| e.to_string())?,
        )?)
        .map_err(|e: serde_json::Error| e.to_string())?;
    if let Err(sync_error) = state.sync_core_config_from_state().await {
        use tiangong_plugin_mcp_protocol::management::RemoveServerRequest;
        let rollback = mcp_invoke_state(
            state,
            "mcp.server.remove",
            serde_json::to_value(RemoveServerRequest { name: name.clone() }).unwrap_or_default(),
        )
        .map(|_| "已撤销 MCP 注册".to_string())
        .unwrap_or_else(|error| format!("撤销 MCP 注册失败：{error}"));
        return Err(format!("同步 MCP 配置失败：{sync_error}；{rollback}"));
    }
    Ok(Some(response.message))
}

async fn unregister_bot_mcp(
    id: &tiangong_bots::BotId,
    state: &TiangongApp,
) -> Result<bool, String> {
    use tiangong_plugin_mcp_protocol::management::RemoveServerRequest;
    let supports_mcp = state
        .bot_runtime
        .supports_mcp(id)
        .await
        .map_err(|error| format!("读取 Bot MCP 能力失败：{error}"))?;
    if !supports_mcp {
        return Ok(false);
    }

    let generated = state
        .bot_runtime
        .generate_mcp_config(id)
        .await
        .map_err(|error| format!("生成 Bot MCP 配置失败：{error}"))?;
    let Some(existing) = list_mcp_servers_via_sidecar(state)?
        .into_iter()
        .find(|server| server.name == generated.name)
    else {
        return Ok(false);
    };
    if !bot_mcp_connection_matches(&existing, &generated) {
        return Ok(false);
    }

    let _: tiangong_plugin_mcp_protocol::MessageResponse =
        serde_json::from_value(mcp_invoke_state(
            state,
            "mcp.server.remove",
            serde_json::to_value(RemoveServerRequest {
                name: generated.name.clone(),
            })
            .map_err(|e| e.to_string())?,
        )?)
        .map_err(|e: serde_json::Error| e.to_string())?;
    if let Err(sync_error) = state.sync_core_config_from_state().await {
        let request = bot_mcp_registration_request(&generated, existing.enabled);
        let rollback = mcp_invoke_state(
            state,
            "mcp.server.register",
            serde_json::to_value(&request).unwrap_or_default(),
        )
        .map(|_| "已恢复 MCP 注册".to_string())
        .unwrap_or_else(|error| format!("恢复 MCP 注册失败：{error}"));
        return Err(format!("同步 MCP 配置失败：{sync_error}；{rollback}"));
    }
    Ok(true)
}

/// 把 Bot 声明的出站能力注册为普通 stdio MCP。
#[tauri::command]
pub async fn bot_register_mcp(id: String, state: State<'_, TiangongApp>) -> Result<String, String> {
    let id = validate_bot_id(id)?;
    if state.bot_store.get(&id).is_none() {
        return Err(format!("bot 不存在：{id}"));
    }
    ensure_bot_mcp_registered(&id, state.inner())
        .await?
        .ok_or_else(|| "该 Bot 不支持 MCP".to_string())
}

/// 注册新 bot（不启动；需先 `bot_install` 下载制品再 `bot_start`）
#[tauri::command]
pub async fn bot_register(
    request: tiangong_bots::RegisterBotRequest,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_bots::BotConfig, String> {
    tiangong_bots::BotId::try_from(request.id.as_str()).map_err(|error| error.to_string())?;
    state
        .bot_store
        .register(request)
        .map_err(|error| error.to_string())
}

/// 更新已有 bot（id 主键不变）
#[tauri::command]
pub async fn bot_update(
    id: String,
    request: tiangong_bots::UpdateBotRequest,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_bots::BotConfig, String> {
    let id = validate_bot_id(id)?;
    state
        .bot_store
        .update(&id, request)
        .map_err(|error| error.to_string())
}

/// 删除 bot 配置（若运行中则先停止），保留已安装制品和运行目录。
#[tauri::command]
pub async fn bot_remove(id: String, state: State<'_, TiangongApp>) -> Result<String, String> {
    let id = validate_bot_id(id)?;
    if state.bot_store.get(&id).is_none() {
        return Err(format!("bot 不存在：{id}"));
    }
    let mcp_removed = unregister_bot_mcp(&id, state.inner()).await?;
    if let Err(stop_error) = state.bot_runtime.stop(&id).await {
        let recovery = if mcp_removed {
            ensure_bot_mcp_registered(&id, state.inner())
                .await
                .map(|_| "已恢复 MCP 注册".to_string())
                .unwrap_or_else(|error| format!("恢复 MCP 注册失败：{error}"))
        } else {
            "MCP 状态无需恢复".to_string()
        };
        return Err(format!(
            "停止 bot 失败，未删除配置：{stop_error}；{recovery}"
        ));
    }
    if let Err(remove_error) = state.bot_store.remove(&id) {
        let recovery = if mcp_removed {
            ensure_bot_mcp_registered(&id, state.inner())
                .await
                .map(|_| "已恢复 MCP 注册".to_string())
                .unwrap_or_else(|error| format!("恢复 MCP 注册失败：{error}"))
        } else {
            "MCP 状态无需恢复".to_string()
        };
        return Err(format!("删除 bot 配置失败：{remove_error}；{recovery}"));
    }
    Ok("bot 配置已删除，已安装程序保留".to_string())
}

/// 下载某制品到指定 bot 实例目录
#[tauri::command]
pub async fn bot_install(
    artifact_id: String,
    dest_bot_id: String,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    let dest_bot_id = validate_bot_id(dest_bot_id)?;
    let index = state
        .bot_runtime
        .fetch_index()
        .await
        .map_err(|e| e.to_string())?;
    let manifest = index
        .bots
        .into_iter()
        .find(|m| m.id == artifact_id)
        .ok_or_else(|| format!("bots-index 中未找到制品：{artifact_id}"))?;
    let progress: tiangong_bots::ProgressFn = std::sync::Arc::new({
        let app = app.clone();
        let total = manifest.current_artifact().map(|_| 0u64).unwrap_or(0);
        let _ = total;
        move |downloaded, content_len| {
            let _ = app.emit(
                "bot_install_progress",
                serde_json::json!({ "downloaded": downloaded, "total": content_len }),
            );
        }
    });
    state
        .bot_runtime
        .install(manifest, &dest_bot_id, Some(progress))
        .await
        .map_err(|e| e.to_string())?;
    Ok("制品安装完成".to_string())
}

/// 启动指定 bot 实例（需制品已安装）。
///
/// 启动 bot 前自动确保 embedded server 已运行（未运行则启动），
/// bot 需要通过 server 的 /api/v1/messages 收发消息。
#[tauri::command]
pub async fn bot_start(id: String, state: State<'_, TiangongApp>) -> Result<String, String> {
    let id = validate_bot_id(id)?;
    let bot = state
        .bot_store
        .get(&id)
        .ok_or_else(|| format!("bot 不存在：{id}"))?;

    // 读取 Server 配置生成 bot 回连 env，但不强制启动 Server（方案：bot 独立运行）。
    let server_config = state
        .with_state_read(|core_state| Ok(core_state.config.server.clone()))
        .await
        .map_err(|e| e.to_string())?;
    let extra_env = bot_server_env(&server_config);

    state
        .bot_runtime
        .start(&bot, &extra_env)
        .await
        .map_err(|e| e.to_string())?;
    save_started_bot_state(state.bot_store.as_ref(), &id, || async {
        state
            .bot_runtime
            .stop(&id)
            .await
            .map_err(|error| error.to_string())
    })
    .await?;
    // 提示 Server 状态（不阻止 bot 启动）。
    if !server_health_check(&server_config) {
        return Ok(format!(
            "bot 已启动：{}\n当前未检测到天工 Server，Bot 暂时无法调用 Agent。             请启动 Server 后 Bot 将自动恢复连接。",
            bot.id
        ));
    }
    Ok(format!("bot 已启动：{}", bot.id))
}

/// 停止指定 bot 实例
#[tauri::command]
pub async fn bot_stop(id: String, state: State<'_, TiangongApp>) -> Result<String, String> {
    let id = validate_bot_id(id)?;
    let mcp_removed = unregister_bot_mcp(&id, state.inner()).await?;
    let stop_result = stop_bot_with_state(state.bot_store.as_ref(), &id, || async {
        state
            .bot_runtime
            .stop(&id)
            .await
            .map_err(|error| error.to_string())
    })
    .await;
    if let Err(stop_error) = stop_result {
        let recovery = if mcp_removed {
            ensure_bot_mcp_registered(&id, state.inner())
                .await
                .map(|_| "已恢复 MCP 注册".to_string())
                .unwrap_or_else(|error| format!("恢复 MCP 注册失败：{error}"))
        } else {
            "MCP 状态无需恢复".to_string()
        };
        return Err(format!("{stop_error}；{recovery}"));
    }
    Ok("bot 已停止".to_string())
}

async fn save_started_bot_state<F, Fut>(
    store: &tiangong_bots::BotStore,
    id: &tiangong_bots::BotId,
    rollback_stop: F,
) -> Result<(), String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if let Err(save_error) = store.set_enabled(id, true) {
        return match rollback_stop().await {
            Ok(()) => Err(format!(
                "保存自动运行状态失败，已撤销本次启动：{save_error}"
            )),
            Err(stop_error) => Err(format!(
                "保存自动运行状态失败，且 bot 未能停止；请立即检查运行状态：保存错误={save_error}，停止错误={stop_error}"
            )),
        };
    }
    Ok(())
}

async fn stop_bot_with_state<F, Fut>(
    store: &tiangong_bots::BotStore,
    id: &tiangong_bots::BotId,
    stop: F,
) -> Result<(), String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let original = store.get(id).ok_or_else(|| format!("bot 不存在：{id}"))?;
    store
        .set_enabled(id, false)
        .map_err(|error| format!("取消自动运行失败，bot 未停止：{error}"))?;

    if let Err(stop_error) = stop().await {
        if !original.enabled {
            return Err(format!("停止 bot 失败，自动运行状态仍为关闭：{stop_error}"));
        }
        return match store.set_enabled(id, true) {
            Ok(_) => Err(format!("停止 bot 失败，已恢复自动运行状态：{stop_error}")),
            Err(restore_error) => Err(format!(
                "停止 bot 失败，且自动运行状态恢复失败；请立即检查运行状态：停止错误={stop_error}，恢复错误={restore_error}"
            )),
        };
    }
    Ok(())
}

/// 检查某制品是否有线上更新。
///
/// 返回 `Some(manifest)` 表示有更新（含版本号供前端展示），`None` 表示已是最新。
#[tauri::command]
pub async fn bot_check_update(
    artifact_id: String,
    state: State<'_, TiangongApp>,
) -> Result<Option<tiangong_bots::BotManifest>, String> {
    state
        .bot_runtime
        .check_update(&artifact_id)
        .await
        .map_err(|e| e.to_string())
}

/// 升级 bot 制品（记录原状态 → 停止 → 下载新版本 → 恢复原状态）。
#[tauri::command]
pub async fn bot_upgrade(
    bot_id: String,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    let bot_id = validate_bot_id(bot_id)?;
    // 先查 bot 配置拿 artifact_id。
    let bot = state
        .bot_store
        .get(&bot_id)
        .ok_or_else(|| format!("bot 不存在：{bot_id}"))?;
    let artifact_id = bot.artifact_id.clone();
    let was_running = state.bot_runtime.is_running(&bot_id).await;
    let restart_env = if was_running {
        let server_config = state
            .with_state_read(|core_state| Ok(core_state.config.server.clone()))
            .await?;
        Some(bot_server_env(&server_config))
    } else {
        None
    };

    // 拉取线上 manifest。
    let manifest = state
        .bot_runtime
        .check_update(&artifact_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "已是最新版本".to_string())?;

    let progress: tiangong_bots::ProgressFn = std::sync::Arc::new({
        let app = app.clone();
        move |downloaded, content_len| {
            let _ = app.emit(
                "bot_upgrade_progress",
                serde_json::json!({ "downloaded": downloaded, "total": content_len }),
            );
        }
    });
    let mcp_removed = unregister_bot_mcp(&bot_id, state.inner()).await?;
    if let Err(error) = state.bot_store.set_enabled(&bot_id, false) {
        let recovery = if mcp_removed {
            ensure_bot_mcp_registered(&bot_id, state.inner())
                .await
                .map(|_| "已恢复 MCP 注册".to_string())
                .unwrap_or_else(|restore_error| format!("恢复 MCP 注册失败：{restore_error}"))
        } else {
            "MCP 状态无需恢复".to_string()
        };
        return Err(format!(
            "升级前取消自动运行失败，未开始升级：{error}；{recovery}"
        ));
    }

    let upgrade_result = state
        .bot_runtime
        .upgrade(&bot_id, manifest, Some(progress))
        .await;
    if let Err(upgrade_error) = upgrade_result {
        let mut recovery = Vec::new();
        let is_running = state.bot_runtime.is_running(&bot_id).await;
        if was_running && !is_running {
            match state
                .bot_runtime
                .start(
                    &bot,
                    restart_env
                        .as_ref()
                        .expect("运行中的 bot 必须准备恢复环境变量"),
                )
                .await
            {
                Ok(()) => recovery.push("原运行状态已恢复".to_string()),
                Err(error) => recovery.push(format!("恢复运行失败：{error}")),
            }
        } else if !was_running && is_running {
            match state.bot_runtime.stop(&bot_id).await {
                Ok(()) => recovery.push("原停止状态已恢复".to_string()),
                Err(error) => recovery.push(format!("恢复停止状态失败：{error}")),
            }
        } else {
            recovery.push("运行状态无需恢复".to_string());
        }

        if bot.enabled {
            match state.bot_store.set_enabled(&bot_id, true) {
                Ok(_) => recovery.push("自动运行状态已恢复".to_string()),
                Err(error) => recovery.push(format!("恢复自动运行状态失败：{error}")),
            }
        } else {
            recovery.push("自动运行状态仍为关闭".to_string());
        }

        if state.bot_runtime.is_running(&bot_id).await {
            match ensure_bot_mcp_registered(&bot_id, state.inner()).await {
                Ok(Some(_)) => recovery.push("MCP 注册已恢复".to_string()),
                Ok(None) => recovery.push("Bot 不需要 MCP 注册".to_string()),
                Err(error) => recovery.push(format!("恢复 MCP 注册失败：{error}")),
            }
        } else if mcp_removed {
            recovery.push("Bot 未恢复运行，MCP 保持注销".to_string());
        }

        return Err(format!(
            "升级失败：{upgrade_error}；恢复结果：{}",
            recovery.join("；")
        ));
    }

    let mut restore_errors = Vec::new();
    let mut restored_running = !was_running;
    if was_running {
        match state
            .bot_runtime
            .start(
                &bot,
                restart_env
                    .as_ref()
                    .expect("运行中的 bot 必须准备恢复环境变量"),
            )
            .await
        {
            Ok(()) => restored_running = true,
            Err(error) => restore_errors.push(format!("恢复运行失败：{error}")),
        }
    }

    if bot.enabled {
        if let Err(error) = state.bot_store.set_enabled(&bot_id, true) {
            restore_errors.push(format!("恢复自动运行状态失败：{error}"));
        }
    }

    if was_running && restored_running {
        match ensure_bot_mcp_registered(&bot_id, state.inner()).await {
            Ok(_) => {}
            Err(error) => restore_errors.push(format!("恢复 MCP 注册失败：{error}")),
        }
    }

    if !restore_errors.is_empty() {
        return Err(format!(
            "升级已完成，但恢复原状态失败：{}",
            restore_errors.join("；")
        ));
    }

    if was_running {
        Ok("升级完成，Bot 已恢复运行".to_string())
    } else {
        Ok("升级完成，Bot 保持停止".to_string())
    }
}

// ============================================================================
// Skill 管理
//
// skill 列表/详情/启停/删除/env 编辑等管理操作全部经插件设置页（WASM UI）
// 走 handle_view_message 通道，不再暴露 Tauri 命令。@提及补全候选由 skill
// WASM 插件的 mention-candidates 导出贡献，经 Core.get_mentions 聚合。
// ============================================================================

// ============================================================================
// Server 管理
// ============================================================================

/// 获取 Server 配置
#[tauri::command]
pub async fn get_server_config(state: State<'_, TiangongApp>) -> Result<ServerConfigView, String> {
    let config = state
        .with_state_read(|core_state| Ok(core_state.config.server.clone()))
        .await?;
    let running = server_health_check(&config);
    let status = server_status_name(config.enabled, running).to_string();
    let auth_token_masked = config.masked_auth_token();
    Ok(ServerConfigView {
        host: config.host,
        port: config.port,
        auth_token_masked,
        enabled: config.enabled,
        running,
        status,
    })
}

/// 设置 Server 配置
#[tauri::command]
pub async fn set_server_config(
    host: String,
    port: u16,
    auth_token: Option<String>,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    let current = state
        .with_state_read(|core_state| Ok(core_state.config.server.clone()))
        .await?;
    let config = tiangong_server::config::ServerConfig {
        host,
        port,
        auth_token: auth_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .or(current.auth_token),
        enabled: current.enabled,
    };
    save_server_config_to_state(state.inner(), config).await?;
    Ok("Server 配置已保存".to_string())
}

/// 启动嵌入式 Server（Desktop 模式下 Server 运行在 app 进程内）
#[tauri::command]
pub async fn start_server(state: State<'_, TiangongApp>) -> Result<String, String> {
    start_embedded_server_with_intent(state.inner()).await
}

/// 保存持续开启意图并启动 Desktop 嵌入式 Server。
///
/// 开启意图先落盘；启动失败时保留该意图，使托盘和设置页显示异常状态，
/// 且下次 App 启动仍会继续尝试恢复。
pub async fn start_embedded_server_with_intent(state: &TiangongApp) -> Result<String, String> {
    let mut config = state
        .with_state_read(|core_state| Ok(core_state.config.server.clone()))
        .await?;
    if !config.enabled {
        config.enabled = true;
        save_server_config_to_state(state, config.clone()).await?;
    }

    if server_health_check(&config) {
        return Ok("Server 已在运行".to_string());
    }
    if state.is_embedded_server_running() {
        let _ = state.stop_embedded_server();
    }

    state.start_embedded_server(&config.host, config.port, config.auth_token.clone())?;
    if let Err(error) = wait_for_server_health(&config).await {
        let _ = state.stop_embedded_server();
        return Err(error);
    }

    Ok(format!("Server 已启动：{}:{}", config.host, config.port))
}

/// App 启动时恢复上次保持开启的嵌入式 Server。
pub async fn restore_embedded_server(state: &TiangongApp) -> Result<Option<String>, String> {
    let enabled = state
        .with_state_read(|core_state| Ok(core_state.config.server.enabled))
        .await?;
    if !enabled {
        return Ok(None);
    }
    start_embedded_server_with_intent(state).await.map(Some)
}

/// 停止 Server
#[tauri::command]
pub async fn stop_server(state: State<'_, TiangongApp>) -> Result<String, String> {
    stop_server_with_intent(state.inner()).await
}

/// 停止结果附带依赖提示：清单声明 `require_server` 且已启用的插件在
/// Server 关闭期间调用会失败（声明依赖只用于提示，不触发自动处理）。
fn stop_server_summary(base: &str) -> String {
    let dependents = tiangong_plugin_runtime::registry::server_dependent_enabled_plugins();
    if dependents.is_empty() {
        return base.to_string();
    }
    let names: Vec<String> = dependents
        .iter()
        .map(|(_, name)| format!("「{name}」"))
        .collect();
    format!(
        "{base}；以下插件依赖 Server，关闭期间其功能将不可用：{}",
        names.join("、")
    )
}

/// 停止服务并清除持续开启意图；异常状态下没有存活服务时也可正常关闭。
pub async fn stop_server_with_intent(state: &TiangongApp) -> Result<String, String> {
    let config = state
        .with_state_read(|core_state| Ok(core_state.config.server.clone()))
        .await?;

    // 优先停止嵌入式 server
    if state.is_embedded_server_running() {
        state.stop_embedded_server()?;

        // 持久化 enabled 标记
        let mut config = config;
        config.enabled = false;
        save_server_config_to_state(state, config).await?;

        return Ok(stop_server_summary("Server 已停止"));
    }

    // 兜底：检查是否有外部 server 进程
    let running_by_health = server_health_check(&config);
    let running_by_pid = server_pid_alive();
    if !running_by_health && !running_by_pid {
        cleanup_dead_server_pid();
        let mut config = config;
        config.enabled = false;
        save_server_config_to_state(state, config).await?;
        return Ok(stop_server_summary("Server 已关闭"));
    }
    if running_by_health && !running_by_pid {
        cleanup_dead_server_pid();
        return Err("Server 正在运行，但后台 PID 文件缺失或已失效，无法通过应用安全停止。请先手动关闭占用端口的 Server。".to_string());
    }
    tiangong_server::stop_daemon().map_err(|e| e.to_string())?;
    wait_for_server_stop(&config)?;

    // 持久化 enabled 标记
    let mut config = config;
    config.enabled = false;
    save_server_config_to_state(state, config).await?;

    Ok(stop_server_summary("Server 已停止"))
}

async fn save_server_config_to_state(
    state: &TiangongApp,
    config: tiangong_server::config::ServerConfig,
) -> Result<(), String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    tiangong_config::save_server_config_to_dir(&storage_root, &config)
        .map_err(|error| error.to_string())?;
    state
        .with_state(|core_state| {
            core_state.config.server = config;
            Ok(())
        })
        .await
}

#[allow(dead_code)]
pub async fn ensure_server_running_for_bots(
    state: &TiangongApp,
) -> Result<tiangong_server::config::ServerConfig, String> {
    let mut config = state
        .with_state_read(|core_state| Ok(core_state.config.server.clone()))
        .await?;
    if server_health_check(&config) {
        return Ok(config);
    }

    let started_here = if state.is_embedded_server_running() {
        false
    } else {
        tracing::info!("bot 启动前自动启用 embedded server");
        state.start_embedded_server(&config.host, config.port, config.auth_token.clone())?;
        true
    };

    if let Err(error) = wait_for_server_health(&config).await {
        if started_here {
            let _ = state.stop_embedded_server();
        }
        return Err(format!(
            "启动 embedded server 失败（bot 无法收发消息）：{error}"
        ));
    }

    if !config.enabled {
        config.enabled = true;
        if let Err(error) = save_server_config_to_state(state, config.clone()).await {
            if started_here {
                let _ = state.stop_embedded_server();
            }
            return Err(format!("保存 Server 自动运行状态失败：{error}"));
        }
    }
    Ok(config)
}

pub fn bot_server_env(
    config: &tiangong_server::config::ServerConfig,
) -> std::collections::BTreeMap<String, String> {
    tiangong_bots::server_env(&config.host, config.port, config.auth_token.clone())
}

/// 将持续开启意图与实时健康状态归一化为展示状态。
pub fn server_status_name(enabled: bool, running: bool) -> &'static str {
    if running {
        "running"
    } else if enabled {
        "error"
    } else {
        "stopped"
    }
}

/// 检查 Server 是否在运行：优先访问健康检查，PID 仅作为兜底。
pub fn is_server_running(config: &tiangong_server::config::ServerConfig) -> bool {
    if server_health_check(config) {
        return true;
    }

    cleanup_dead_server_pid();
    false
}

fn server_pid_path() -> PathBuf {
    tiangong_config::io::storage_root().join("server.pid")
}

fn server_pid_alive() -> bool {
    let pid_path = server_pid_path();
    if !pid_path.exists() {
        return false;
    }
    match std::fs::read_to_string(&pid_path) {
        Ok(pid_str) => {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                // 检查进程是否存在
                #[cfg(unix)]
                {
                    use std::process::Command;
                    Command::new("kill")
                        .arg("-0")
                        .arg(pid.to_string())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                }
                #[cfg(not(unix))]
                {
                    let _ = pid;
                    false
                }
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

fn cleanup_dead_server_pid() {
    let pid_path = server_pid_path();
    if pid_path.exists() && !server_pid_alive() {
        let _ = std::fs::remove_file(pid_path);
    }
}

async fn wait_for_server_health(
    config: &tiangong_server::config::ServerConfig,
) -> Result<(), String> {
    for _ in 0..30 {
        if server_health_check(config) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err("Server 已启动但健康检查未通过".to_string())
}

fn wait_for_server_stop(config: &tiangong_server::config::ServerConfig) -> Result<(), String> {
    for _ in 0..30 {
        if !server_health_check(config) {
            cleanup_dead_server_pid();
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("已发送停止信号，但 Server 仍在响应健康检查".to_string())
}

pub fn server_health_check(config: &tiangong_server::config::ServerConfig) -> bool {
    let host = connect_host(&config.host);
    let Ok(mut addrs) = (host.as_str(), config.port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(150)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(150)));
    let request = format!(
        "GET /api/v1/health HTTP/1.1\r\nHost: {host}:{}\r\nConnection: close\r\n\r\n",
        config.port
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = [0_u8; 1024];
    let Ok(len) = stream.read(&mut response) else {
        return false;
    };
    let response = String::from_utf8_lossy(&response[..len]);
    response.starts_with("HTTP/") && response.contains(" 200 ") && response.contains("status")
}

pub fn connect_host(host: &str) -> String {
    match host.trim() {
        "" | "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        value => value.to_string(),
    }
}

// ============================================================================
// 模型配置（Provider + Model + Routing 三层架构）
// ============================================================================

/// 获取模型配置
#[tauri::command]
pub async fn get_models_config(state: State<'_, TiangongApp>) -> Result<ModelsConfigView, String> {
    state
        .with_state_read(|core_state| Ok(ModelsConfigView::from_core(&core_state.config.models)))
        .await
}

/// 设置模型配置
#[tauri::command]
pub async fn set_models_config(
    config: ModelsConfigView,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let current = state
        .with_state_read(|core_state| Ok(core_state.config.clone()))
        .await?;
    let config = tiangong_config::registry::update_models(&current, config.to_core())
        .map_err(|error| error.to_string())?;
    state
        .with_state(|core_state| {
            core_state.config = config;
            Ok(())
        })
        .await?;
    state.sync_core_config_from_state().await?;
    Ok(())
}

// ── 索引预热 ──

/// 预热指定路径的工作区索引。
///
/// 后台静默调用（创建新对话/切换工作目录时触发，无用户交互），经 sidecar 通道
/// 转发：索引已存在则直接返回，否则后台扫描立即返回不阻塞。索引管理（列表/
/// 删除/重建）由「设置 → 索引管理」页经插件 UI 通道处理，不在此处。
#[tauri::command]
pub async fn prewarm_workspace_index(root: String) -> Result<(), String> {
    let storage_root = tiangong_config::io::storage_root();
    let payload = serde_json::to_value(
        tiangong_plugin_index_protocol::management::PrewarmWorkspaceIndexRequest { root },
    )
    .map_err(|e| e.to_string())?;
    tiangong_plugin_runtime::registry::invoke_sidecar(
        &storage_root,
        "index",
        tiangong_plugin_index_protocol::management::PREWARM_WORKSPACE_INDEX_OPERATION,
        payload,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 获取所有可用的模型能力列表
#[tauri::command]
pub async fn get_model_capabilities() -> Result<Vec<ModelCapabilityInfo>, String> {
    use tiangong_llm::models_config::ModelCapability;

    let caps = ModelCapability::all()
        .iter()
        .map(|c| {
            let key = serde_json::to_value(c).unwrap_or_default();
            ModelCapabilityInfo {
                key: key.as_str().unwrap_or_default().to_string(),
                display_name: c.display_name().to_string(),
            }
        })
        .collect();
    Ok(caps)
}

/// 获取模型列表
#[tauri::command]
pub async fn get_model_list(state: State<'_, TiangongApp>) -> Result<Vec<String>, String> {
    state
        .with_state_read(|core_state| Ok(core_state.model_list.to_vec()))
        .await
}

/// 根据 provider 配置获取该 provider 的可用模型列表
#[tauri::command]
pub async fn fetch_provider_models(
    base_url: String,
    api_key: String,
    timeout_ms: Option<u64>,
    protocol: Option<String>,
    headers: Option<BTreeMap<String, String>>,
) -> Result<Vec<String>, String> {
    use tiangong_llm::models_config::ModelsConfig;
    use tiangong_llm::ModelEndpoint;
    use tiangong_llm::{ProviderProtocol, SingleProviderClient};

    let resolved_key = ModelsConfig::resolve_api_key(&api_key);
    let endpoint = ModelEndpoint {
        headers: headers.unwrap_or_default(),
        base_url,
        api_key: resolved_key,
        model: String::new(),
        protocol: protocol
            .as_deref()
            .and_then(|value| value.parse::<ProviderProtocol>().ok())
            .unwrap_or_default(),
        timeout_ms: timeout_ms.unwrap_or(60_000),
        options: serde_json::Value::Object(serde_json::Map::new()),
        context_window: None,
    };
    SingleProviderClient::list_models_async(&endpoint)
        .await
        .map_err(|e| e.to_string())
}

// ── Webhook 管理 ─────────────────────────────────────────────

#[tauri::command]
pub async fn webhook_list() -> Result<Vec<serde_json::Value>, String> {
    let store = tiangong_server::webhook::store::WebhookStore::open().map_err(|e| e.to_string())?;
    let webhooks = store.list().map_err(|e| e.to_string())?;
    Ok(webhooks
        .into_iter()
        .map(|w| serde_json::to_value(w).unwrap())
        .collect())
}

#[tauri::command]
pub async fn webhook_create(
    name: String,
    description: String,
    session_id: Option<String>,
    payload: String,
    secret: Option<String>,
    enabled: Option<bool>,
) -> Result<serde_json::Value, String> {
    let store = tiangong_server::webhook::store::WebhookStore::open().map_err(|e| e.to_string())?;
    let now = chrono::Local::now().naive_local().to_string();
    let webhook = tiangong_server::webhook::model::Webhook {
        id: scru128::new().to_string(),
        name,
        description,
        session_id,
        payload,
        secret,
        enabled: enabled.unwrap_or(true),
        created_at: now.clone(),
        updated_at: now,
    };
    store.insert(&webhook).map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(webhook).unwrap())
}

#[tauri::command]
pub async fn webhook_update(
    id: String,
    name: Option<String>,
    description: Option<String>,
    session_id: Option<String>,
    payload: Option<String>,
    secret: Option<String>,
    enabled: Option<bool>,
) -> Result<serde_json::Value, String> {
    let store = tiangong_server::webhook::store::WebhookStore::open().map_err(|e| e.to_string())?;
    let req = tiangong_server::webhook::model::UpdateWebhookRequest {
        name,
        description,
        session_id,
        payload,
        secret,
        enabled,
    };
    let updated = store.update(&id, &req).map_err(|e| e.to_string())?;
    if !updated {
        return Err(format!("Webhook '{id}' 不存在"));
    }
    let webhook = store.get(&id).map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(webhook).unwrap())
}

#[tauri::command]
pub async fn webhook_delete(id: String) -> Result<(), String> {
    let store = tiangong_server::webhook::store::WebhookStore::open().map_err(|e| e.to_string())?;
    let deleted = store.delete(&id).map_err(|e| e.to_string())?;
    if !deleted {
        return Err(format!("Webhook '{id}' 不存在"));
    }
    Ok(())
}

#[tauri::command]
pub async fn webhook_trigger(
    id: String,
    state: State<'_, TiangongApp>,
) -> Result<serde_json::Value, String> {
    // 校验 webhook 存在（CRUD 仍直接读写本地 store）
    let store = tiangong_server::webhook::store::WebhookStore::open().map_err(|e| e.to_string())?;
    let webhook = store
        .get(&id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Webhook '{id}' 不存在"))?;

    // webhook 触发（投递消息到 Core）是 server 的能力：经嵌入 server 的 HTTP 接口执行，
    // 由 server 用 ServerCoreBackend 投递。需嵌入 server 在线。
    let server_config = state
        .with_state_read(|core_state| Ok(core_state.config.server.clone()))
        .await
        .map_err(|e| e.to_string())?;
    if !server_health_check(&server_config) {
        return Err("Webhook 触发需要先启动内嵌 Server".to_string());
    }
    let host = connect_host(&server_config.host);
    let url = format!(
        "http://{host}:{}/api/v1/webhooks/{id}/trigger",
        server_config.port
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?;
    let mut request = client.post(&url);
    if let Some(token) = server_config.auth_token.as_deref() {
        if !token.trim().is_empty() {
            request = request.bearer_auth(token);
        }
    }
    let resp = request
        .send()
        .await
        .map_err(|e| format!("触发 Webhook 失败：{e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("触发 Webhook 失败：HTTP {status}，{body}"));
    }
    let value: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析触发响应失败：{e}"))?;

    Ok(serde_json::json!({
        "webhook_id": webhook.id,
        "session_id": webhook.session_id,
        "status": value.get("status").cloned().unwrap_or(serde_json::json!("triggered")),
    }))
}

#[tauri::command]
pub async fn webhook_list_runs(
    id: String,
    limit: Option<usize>,
) -> Result<Vec<serde_json::Value>, String> {
    let store = tiangong_server::webhook::store::WebhookStore::open().map_err(|e| e.to_string())?;
    let runs = store
        .list_runs(&id, limit.unwrap_or(20))
        .map_err(|e| e.to_string())?;
    Ok(runs
        .into_iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::{
        cancel_after_session_send_boundary, done_event_keeps_turn_running,
        merge_agent_worker_messages, normalize_path_list, save_started_bot_state,
        stop_bot_with_state,
    };

    #[test]
    fn sandbox_directory_allowlist_requires_existing_directories() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("allowed");
        let file = root.path().join("file.txt");
        let missing = root.path().join("missing");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(&file, "x").unwrap();

        let normalized = normalize_path_list(vec![directory.display().to_string()]).unwrap();
        assert_eq!(
            normalized,
            vec![std::fs::canonicalize(&directory)
                .unwrap()
                .display()
                .to_string()]
        );
        assert!(normalize_path_list(vec![missing.display().to_string()])
            .unwrap_err()
            .contains("目录不存在"));
        assert!(normalize_path_list(vec![file.display().to_string()])
            .unwrap_err()
            .contains("路径不是目录"));
    }

    #[tokio::test]
    async fn cancel_waits_for_send_boundary_and_returns_delivery_result() {
        let session_lock = Arc::new(tokio::sync::Mutex::new(()));
        let send_guard = session_lock.clone().lock_owned().await;
        let delivered = Arc::new(AtomicBool::new(false));
        let delivered_in_cancel = delivered.clone();
        let mut cancel = tokio::spawn(async move {
            cancel_after_session_send_boundary(session_lock, || {
                delivered_in_cancel.store(true, Ordering::Release);
                true
            })
            .await
        });

        tokio::task::yield_now().await;
        assert!(!cancel.is_finished(), "发送边界释放前取消必须等待");
        assert!(!delivered.load(Ordering::Acquire));
        drop(send_guard);

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut cancel)
                .await
                .expect("发送边界释放后取消应继续")
                .expect("取消任务不应 panic")
        );
        assert!(delivered.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn cancel_reports_when_no_core_accepts_the_command() {
        let session_lock = Arc::new(tokio::sync::Mutex::new(()));
        assert!(!cancel_after_session_send_boundary(session_lock, || false).await);
    }

    #[test]
    fn empty_done_keeps_pending_turn_running() {
        let event = tiangong_types::StreamEvent::Done { usage: None };
        assert!(done_event_keeps_turn_running(&event, true));
        assert!(!done_event_keeps_turn_running(&event, false));
    }

    #[test]
    fn final_done_with_usage_keeps_queued_next_turn_running() {
        let event = tiangong_types::StreamEvent::Done {
            usage: Some(tiangong_types::TokenUsage::default()),
        };
        assert!(done_event_keeps_turn_running(&event, true));
    }

    #[test]
    fn error_event_never_keeps_turn_running() {
        let event = tiangong_types::StreamEvent::Error {
            message: "failed".to_string(),
        };
        assert!(!done_event_keeps_turn_running(&event, true));
    }

    #[test]
    fn agent_worker_merge_is_scoped_by_worker_and_message_id() {
        use tiangong_core::session::{Message, MessageRole, Session};

        fn worker_message(id: &str, worker_id: &str, content: &str) -> Message {
            let mut message = Message::new(MessageRole::Assistant, content);
            message.id = id.to_string();
            message.worker_id = Some(worker_id.to_string());
            message
        }

        let mut session = Session::new("desktop-agent-workers");
        let mut main = Message::new(MessageRole::Assistant, "main");
        main.id = "shared".to_string();
        session.messages.push(main);

        let cached = vec![
            worker_message("shared", "agent:dev:agent-dev", "latest"),
            worker_message("next", "agent:dev:agent-dev", "next"),
            worker_message("shared", "agent:test:agent-test", "tester"),
            worker_message("ignored", "background:worker", "ignored"),
        ];
        let authoritative_messages = session.messages.clone();
        let mut snapshot_messages = authoritative_messages.clone();
        snapshot_messages.push(worker_message("shared", "agent:dev:agent-dev", "stale"));
        merge_agent_worker_messages(&mut snapshot_messages, &cached);

        assert_eq!(session.messages.len(), authoritative_messages.len());
        assert_eq!(session.messages[0].id, authoritative_messages[0].id);
        assert_eq!(session.messages[0].text_content(), "main");
        assert!(session
            .messages
            .iter()
            .all(|message| message.worker_id.is_none()));
        assert_eq!(snapshot_messages[0].text_content(), "main");
        let workers = snapshot_messages
            .iter()
            .filter(|message| {
                message
                    .worker_id
                    .as_deref()
                    .is_some_and(|worker_id| worker_id.starts_with("agent:"))
            })
            .collect::<Vec<_>>();
        assert_eq!(workers.len(), 3);
        assert!(workers.iter().any(|message| {
            message.id == "shared"
                && message.worker_id.as_deref() == Some("agent:dev:agent-dev")
                && message.text_content() == "latest"
        }));
        assert!(workers.iter().any(|message| {
            message.id == "shared"
                && message.worker_id.as_deref() == Some("agent:test:agent-test")
                && message.text_content() == "tester"
        }));
        assert!(workers.iter().any(|message| message.id == "next"));
    }

    fn bot_store(enabled: bool) -> (tempfile::TempDir, tiangong_bots::BotStore) {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config").join("bots.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let config = tiangong_bots::BotsConfig {
            bots: vec![tiangong_bots::BotConfig {
                id: tiangong_bots::BotId::try_from("feishu").unwrap(),
                artifact_id: "feishu".into(),
                config: std::collections::BTreeMap::new(),
                enabled,
                created_at: "now".into(),
                updated_at: "now".into(),
            }],
        };
        std::fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();
        let store = tiangong_bots::BotStore::with_config_path(config_path).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn failed_start_state_write_triggers_stop_rollback() {
        let (dir, store) = bot_store(false);
        let id = tiangong_bots::BotId::try_from("feishu").unwrap();
        let config_dir = dir.path().join("config");
        std::fs::remove_file(config_dir.join("bots.json")).unwrap();
        std::fs::remove_dir(&config_dir).unwrap();
        std::fs::write(&config_dir, b"blocks directory creation").unwrap();
        let rollback_called = Arc::new(AtomicBool::new(false));
        let rollback_called_in_task = rollback_called.clone();

        let error = save_started_bot_state(&store, &id, || async move {
            rollback_called_in_task.store(true, Ordering::Release);
            Ok(())
        })
        .await
        .unwrap_err();

        assert!(error.contains("已撤销本次启动"));
        assert!(rollback_called.load(Ordering::Acquire));
        assert!(!store.get(&id).unwrap().enabled);
    }

    #[tokio::test]
    async fn failed_stop_restores_original_enabled_state() {
        let (_dir, store) = bot_store(true);
        let id = tiangong_bots::BotId::try_from("feishu").unwrap();

        let error = stop_bot_with_state(&store, &id, || async {
            Err("simulated stop failure".to_string())
        })
        .await
        .unwrap_err();

        assert!(error.contains("已恢复自动运行状态"));
        assert!(store.get(&id).unwrap().enabled);
    }
}

/// 按模型名从 context_windows.json 映射表解析默认 context_window（token 数）。
/// 供前端在编辑模型时预填默认值。
#[tauri::command]
pub async fn resolve_model_context_window(
    model: String,
    state: State<'_, TiangongApp>,
) -> Result<usize, String> {
    let dir = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    Ok(tiangong_config::io::resolve_context_limit_at(&dir, &model))
}

// ── 插件 UI 桥接（WASM 插件动态 UI）──
//
// 天工只提供通用桥接，不处理具体插件业务：
// - list_plugin_contributions：列出插件声明（含是否有页面）
// - plugin_open_view：按需获取插件页面 HTML（天工只当容器，不处理内容）
// - plugin_call：通用桥接，转发到 WASM 的 handle-view-message

/// 列出所有已加载 WASM 插件的设置页贡献。
#[tauri::command]
pub async fn list_plugin_contributions() -> Result<Vec<PluginContributionEntry>, String> {
    let entries = tiangong_plugin_runtime::registry::list_contributions();
    Ok(entries
        .into_iter()
        .flat_map(|(plugin_id, generation, contributions)| {
            contributions
                .into_iter()
                .map(move |c| PluginContributionEntry {
                    plugin_id: plugin_id.clone(),
                    generation,
                    contribution_id: c.id,
                    title: c.title,
                    description: c.description,
                    icon: c.icon,
                    group: c.group,
                    has_view: c.has_view,
                })
        })
        .collect())
}

/// 按挂载点列出 UI 贡献（UI 接缝统一入口）。
///
/// v1 插件的 WASM 设置页贡献映射到 `settings.plugin-page`（零改动兼容）；
/// v2 插件取 manifest `ui.contributions` 中 slot 匹配的项。
#[tauri::command]
pub async fn list_slot_contributions(
    slot: String,
) -> Result<Vec<tiangong_plugin_runtime::registry::SlotContribution>, String> {
    Ok(tiangong_plugin_runtime::registry::list_slot_contributions(
        &slot,
    ))
}

/// 同步拓展区中实际存在的 webview 插件标签，供页面事件注入校验。
#[tauri::command]
pub async fn set_webview_mounted_tabs(
    session_id: String,
    tab_ids: Vec<String>,
    state: State<'_, crate::webview_host::WebviewHostState>,
) -> Result<(), String> {
    if session_id.trim().is_empty() {
        return Err("会话 ID 不能为空".to_string());
    }
    let scope = format!("webview:browser:{session_id}");
    state.manager_for(&scope).set_mounted_tabs(tab_ids);
    Ok(())
}

/// 列出拓展区 App：声明 `extension.tab` 贡献的已启用插件（能力矩阵数据源）。
#[tauri::command]
pub async fn list_extension_apps(
) -> Result<Vec<tiangong_plugin_runtime::registry::ExtensionApp>, String> {
    Ok(tiangong_plugin_runtime::registry::list_extension_apps())
}

/// 读取 v2 manifest UI 贡献的入口 HTML（entry 相对插件目录）。
#[tauri::command]
pub async fn plugin_open_entry(
    plugin_id: String,
    contribution_id: String,
) -> Result<String, String> {
    tiangong_plugin_runtime::registry::open_manifest_view(&plugin_id, &contribution_id)
        .map_err(|error| error.to_string())
}

/// 读取 v2 manifest UI 贡献的相对资源（以 entry 所在目录为根，沙箱容器加载
/// 外链脚本/样式用；路径逃出插件目录会被拒绝）。
#[tauri::command]
pub async fn plugin_read_entry_resource(
    plugin_id: String,
    contribution_id: String,
    path: String,
) -> Result<PluginEntryResource, String> {
    let (data, mime) = tiangong_plugin_runtime::registry::read_manifest_resource(
        &plugin_id,
        &contribution_id,
        &path,
    )
    .map_err(|error| error.to_string())?;
    Ok(PluginEntryResource { data, mime })
}

/// 列出已导入的第三方插件信任公钥。
#[tauri::command]
pub async fn plugin_list_trusted_publishers(
    state: State<'_, TiangongApp>,
) -> Result<Vec<tiangong_plugin_runtime::trust::TrustedPublisher>, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::trust::list_trusted_publishers(&storage_root)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("读取信任公钥列表失败: {error}"))?
}

/// 导入第三方插件信任公钥（设置界面手动操作；内容为 base64(公钥文本)，
/// 与开发者公开的 .pub 公钥文件内容同格式）。
#[tauri::command]
pub async fn plugin_import_trusted_publisher(
    publisher: String,
    public_key: String,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_plugin_runtime::trust::TrustedPublisher, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::trust::import_trusted_publisher(
            &storage_root,
            &publisher,
            &public_key,
        )
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("导入信任公钥失败: {error}"))?
}

/// 移除第三方插件信任公钥（移除后对应插件下次启动即失效）。
#[tauri::command]
pub async fn plugin_remove_trusted_publisher(
    publisher: String,
    state: State<'_, TiangongApp>,
) -> Result<bool, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::trust::remove_trusted_publisher(&storage_root, &publisher)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("移除信任公钥失败: {error}"))?
}

/// 读取用户选择的公钥文件内容（信任导入辅助）：仅接受 .pub/.txt 后缀、
/// 不超过 16KB 的文本文件，返回 trim 后内容。
#[tauri::command]
pub async fn plugin_read_public_key_file(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = std::path::PathBuf::from(&path);
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !matches!(extension.as_str(), "pub" | "txt") {
            return Err(format!(
                "仅支持 .pub / .txt 公钥文件: {path}",
                path = path.display()
            ));
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("读取文件信息失败: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("公钥必须是普通文件".to_string());
        }
        if metadata.len() > 16 * 1024 {
            return Err("公钥文件超过 16KB 上限".to_string());
        }
        std::fs::read_to_string(&path)
            .map(|content| content.trim().to_string())
            .map_err(|error| format!("读取公钥文件失败: {error}"))
    })
    .await
    .map_err(|error| format!("读取公钥文件任务失败: {error}"))?
}

/// 读取本机用户签名密钥的公钥指纹（尚未生成时返回 None——首次创作安装
/// 时自动生成）。
#[tauri::command]
pub async fn plugin_user_key_fingerprint(
    state: State<'_, TiangongApp>,
) -> Result<Option<String>, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        match tiangong_plugin_runtime::trust::user_public_key_b64(&storage_root) {
            Ok(public_b64) => tiangong_plugin_runtime::trust::publisher_fingerprint(&public_b64)
                .map(Some)
                .map_err(|error| error.to_string()),
            Err(_) => Ok(None),
        }
    })
    .await
    .map_err(|error| format!("读取用户密钥指纹失败: {error}"))?
}

/// 插件入口资源响应（字节数组 + MIME）。
/// 读取插件 App 的自定义图标（拓展区矩阵渲染；插件根为根、扩展名白名单、
/// 256KB 上限、防逃逸，见 registry::read_plugin_icon）。
#[tauri::command]
pub async fn plugin_read_icon(
    plugin_id: String,
    contribution_id: String,
) -> Result<PluginEntryResource, String> {
    let (data, mime) =
        tiangong_plugin_runtime::registry::read_plugin_icon(&plugin_id, &contribution_id)
            .map_err(|error| error.to_string())?;
    Ok(PluginEntryResource { data, mime })
}

#[derive(serde::Serialize)]
pub struct PluginEntryResource {
    pub data: Vec<u8>,
    pub mime: String,
}

/// 列出已安装插件、当前加载版本和 sidecar 状态。
#[tauri::command]
pub async fn list_plugins(
    state: State<'_, TiangongApp>,
) -> Result<Vec<tiangong_plugin_runtime::registry::PluginStatus>, String> {
    state.wait_plugin_preload().await?;
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::registry::list_plugins(
            &storage_root,
            tiangong_plugin_runtime::registry::RuntimeKind::Desktop,
        )
    })
    .await
    .map_err(|error| format!("读取插件状态失败: {error}"))
}

/// 从 OSS 静态目录读取当前平台可安装的插件。
#[tauri::command]
pub async fn list_available_plugins(
    state: State<'_, TiangongApp>,
) -> Result<Vec<tiangong_plugin_runtime::artifacts::AvailablePlugin>, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    let repository = tiangong_plugin_runtime::artifacts::PluginRepository::new()
        .map_err(|error| error.to_string())?;
    repository
        .list_available(&storage_root)
        .await
        .map_err(|error| error.to_string())
}

/// 首次启动推荐安装检测结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DefaultPluginCheck {
    /// 是否需要弹出首次启动推荐引导（标记未完成且存在缺失的默认插件）。
    pub first_launch_pending: bool,
    /// 缺失的默认插件（OSS 目录中存在、当前平台支持且尚未安装）。
    pub missing: Vec<tiangong_plugin_runtime::artifacts::AvailablePlugin>,
    /// OSS 目录拉取失败原因。网络异常时不强弹引导，下次启动再试。
    pub catalog_error: Option<String>,
}

/// 检测是否需要首次启动推荐安装默认插件。
#[tauri::command]
pub async fn check_default_plugins(
    state: State<'_, TiangongApp>,
) -> Result<DefaultPluginCheck, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;

    // 已完成首次启动引导，直接跳过，不再打扰用户。
    if tiangong_plugin_runtime::artifacts::is_first_launch_completed(&storage_root) {
        return Ok(DefaultPluginCheck {
            first_launch_pending: false,
            missing: Vec::new(),
            catalog_error: None,
        });
    }

    let repository = tiangong_plugin_runtime::artifacts::PluginRepository::new()
        .map_err(|error| error.to_string())?;
    let available = match repository.list_available(&storage_root).await {
        Ok(list) => list,
        Err(error) => {
            // 网络或 OSS 不可达时不强弹引导，避免离线首启打扰用户。
            return Ok(DefaultPluginCheck {
                first_launch_pending: false,
                missing: Vec::new(),
                catalog_error: Some(error.to_string()),
            });
        }
    };

    let missing: Vec<_> = available
        .into_iter()
        .filter(|plugin| {
            plugin.is_default && plugin.supported && plugin.installed_version.is_none()
        })
        .collect();

    Ok(DefaultPluginCheck {
        first_launch_pending: !missing.is_empty(),
        missing,
        catalog_error: None,
    })
}

/// 标记首次启动引导已完成（用户跳过或安装结束后调用）。
#[tauri::command]
pub async fn complete_first_launch(state: State<'_, TiangongApp>) -> Result<(), String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::artifacts::mark_first_launch_completed(&storage_root)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("写入首次启动标记任务失败: {error}"))?
}

// 插件装卸/启停成功后的宿主响应（前端刷新、上下文交接打标）已改为
// runtime 插件变化事件的订阅者（见 main.rs setup 注册）；命令层不再扇出。
// 下载/安装编排同样下沉 runtime（artifacts::install_plugin_from_repository），
// 宿主仅注入进度回调转发到前端事件。

/// 下载并安装：runtime 编排 + 宿主进度事件转发。
pub(crate) async fn download_and_install_plugin(
    storage_root: std::path::PathBuf,
    plugin_id: String,
    app: AppHandle,
) -> Result<tiangong_plugin_runtime::registry::PluginStatus, String> {
    let progress: tiangong_plugin_runtime::artifacts::ProgressFn = std::sync::Arc::new({
        let app = app.clone();
        let plugin_id = plugin_id.clone();
        move |downloaded, total| {
            let _ = app.emit(
                "plugin_install_progress",
                serde_json::json!({ "plugin_id": plugin_id, "downloaded": downloaded, "total": total }),
            );
        }
    });
    tiangong_plugin_runtime::artifacts::install_plugin_from_repository(
        &storage_root,
        &plugin_id,
        Some(progress),
    )
    .await
    .map_err(|error| error.to_string())
}

/// 从用户选择的本地完整目录或签名插件归档（tar.zst）导入插件。
///
/// 信任语义：三方签名插件经导入的发布者公钥验证（见 runtime trust 模块），
/// 验证发生在安装链（verify_signed_release 按发布者路由）。带 sidecar 的
/// 插件在安装前展示权限清单（敏感权限显著标记）由用户确认一次——导入
/// 是用户手动触发的低频操作，此处确认不阻碍创作链自动化（创作链经
/// plugin-dev 自动签名，不经本命令）。
#[tauri::command]
pub async fn import_local_plugin(
    path: String,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_plugin_runtime::registry::PluginStatus, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    let app_handle = app.clone();
    let status = tauri::async_runtime::spawn_blocking(move || {
        let total_started = Instant::now();
        let source = std::path::Path::new(&path);
        let stage_started = Instant::now();
        let staged_result = if source
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("zst"))
        {
            tiangong_plugin_runtime::artifacts::stage_plugin_archive(&storage_root, source)
        } else {
            tiangong_plugin_runtime::artifacts::stage_local_plugin(&storage_root, source)
        }
        .map_err(|error| error.to_string());
        let stage_ms = stage_started.elapsed().as_millis() as u64;
        let staged = match staged_result {
            Ok(staged) => staged,
            Err(error) => {
                tracing::warn!(
                    stage_ms,
                    total_ms = total_started.elapsed().as_millis() as u64,
                    %error,
                    "本地插件暂存阶段失败"
                );
                return Err(error);
            }
        };

        // 带 sidecar 的插件（本地进程能力）：安装前展示权限清单确认一次。
        let manifest = tiangong_plugin_runtime::manifest::PluginManifest::load(
            &staged.path().join(tiangong_plugin_runtime::manifest::MANIFEST_FILE),
        )
        .map_err(|error| format!("插件清单无效: {error}"))?;
        if manifest.sidecar.is_some() {
            let sensitive: &[&str] = &["model-config.read", "app-storage.read"];
            let permissions = if manifest.permissions.is_empty() {
                "无".to_string()
            } else {
                manifest
                    .permissions
                    .iter()
                    .map(|permission| {
                        if sensitive.contains(&permission.as_str()) {
                            format!("{permission}（敏感）")
                        } else {
                            permission.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("、")
            };
            let message = format!(
                "即将导入插件：\n\n插件：{}（id: {}）\n版本：{}\n权限：{}\n来源：{}\n\n带本地进程能力的插件将按上述权限运行；签名插件经已导入的发布者公钥验证。请确认内容可信。",
                manifest.id, manifest.id, manifest.version, permissions, path
            );
            use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
            let confirmed = app_handle
                .dialog()
                .message(message)
                .title("导入插件确认")
                .kind(MessageDialogKind::Warning)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "导入".to_string(),
                    "取消".to_string(),
                ))
                .blocking_show();
            if !confirmed {
                return Err("用户取消了插件导入".to_string());
            }
        }

        let install_started = Instant::now();
        let result =
            tiangong_plugin_runtime::registry::import_staged_plugin(&storage_root, staged.path())
                .map_err(|error| error.to_string());
        let install_ms = install_started.elapsed().as_millis() as u64;
        tracing::info!(
            stage_ms,
            install_ms,
            total_ms = total_started.elapsed().as_millis() as u64,
            success = result.is_ok(),
            "本地插件导入阶段完成"
        );
        result
    })
    .await
    .map_err(|error| format!("导入本地插件任务失败: {error}"))??;
    Ok(status)
}

/// 从 OSS 下载并安装插件。
#[tauri::command]
pub async fn install_plugin(
    plugin_id: String,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_plugin_runtime::registry::PluginStatus, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    let status = download_and_install_plugin(storage_root, plugin_id, app.clone()).await?;
    Ok(status)
}

/// 从 OSS 下载并升级插件，运行时负责失败恢复和本地回滚版本保留。
#[tauri::command]
pub async fn upgrade_plugin(
    plugin_id: String,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_plugin_runtime::registry::PluginStatus, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    let status = download_and_install_plugin(storage_root, plugin_id, app.clone()).await?;
    Ok(status)
}

/// 启用或停用已安装插件。
#[tauri::command]
pub async fn set_plugin_enabled(
    plugin_id: String,
    enabled: bool,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_plugin_runtime::registry::PluginStatus, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    let status = tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::registry::set_plugin_enabled(&storage_root, &plugin_id, enabled)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("切换插件状态任务失败: {error}"))??;
    Ok(status)
}

/// 回滚到本地保留的上一个插件版本。
#[tauri::command]
pub async fn rollback_plugin(
    plugin_id: String,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_plugin_runtime::registry::PluginStatus, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    let status = tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::registry::rollback_plugin(&storage_root, &plugin_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("回滚插件任务失败: {error}"))??;
    Ok(status)
}

/// 卸载插件，可选择保留插件数据。
#[tauri::command]
pub async fn uninstall_plugin(
    plugin_id: String,
    keep_data: bool,
    state: State<'_, TiangongApp>,
) -> Result<(), String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::registry::uninstall_plugin(&storage_root, &plugin_id, keep_data)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("卸载插件任务失败: {error}"))??;
    Ok(())
}

/// 从已安装目录读取新制品并原子替换全部存活 WASM 实例。
#[tauri::command]
pub async fn reload_plugin(
    plugin_id: String,
    app: AppHandle,
    state: State<'_, TiangongApp>,
) -> Result<tiangong_plugin_runtime::registry::PluginStatus, String> {
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await?;
    let reloaded_id = plugin_id.clone();
    let status = tauri::async_runtime::spawn_blocking(move || {
        tiangong_plugin_runtime::registry::reload_plugin(&storage_root, &plugin_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("重载插件任务失败: {error}"))??;
    // 定向通知前端该插件已热加载：前端据此卸载旧的后台执行壳，下次
    // 工具调用按磁盘最新页面重建，避免旧壳吞掉重新挂载的请求。
    let _ = app.emit("plugin_reloaded", &reloaded_id);
    Ok(status)
}

/// 按需获取插件页面 HTML（用户点击进入时才调用）。
#[tauri::command]
pub async fn plugin_open_view(
    plugin_id: String,
    contribution_id: String,
) -> Result<String, String> {
    tiangong_plugin_runtime::registry::open_view(&plugin_id, &contribution_id)
        .ok_or_else(|| format!("插件 {plugin_id} 未加载或无页面"))
}

/// 通用桥接：转发到 WASM 的 handle-view-message。
/// iframe 内的 JS 经 postMessage → 前端 → 本命令 → WASM。
/// 天工不关心 method/payload 含义，只做透传。
#[tauri::command]
pub async fn plugin_call(
    plugin_id: String,
    method: String,
    payload: String,
) -> Result<String, String> {
    tiangong_plugin_runtime::registry::handle_view_message(&plugin_id, &method, &payload)
        .ok_or_else(|| format!("插件 {plugin_id} 未加载或处理消息失败"))
}

// ── 宿主桥接（Host Bridge）──
//
// 统一插件 UI ↔ 宿主通信管道：命名空间白名单 + 权限校验 + 负载透传。
// - bridge_call：调用宿主能力（plugin.* 转发到 WASM，其余命名空间按接缝任务接入）
// - bridge_subscribe / bridge_unsubscribe：事件订阅骨架（事件源在事件接缝接入）

/// 桥接调用：`method` 按命名空间路由（如 `plugin.getConfig` → WASM）。
///
/// `plugin-dev.*` 含构建（可长达数分钟）与安装原生确认（等待用户点击），
/// 经 spawn_blocking 执行，避免阻塞 tokio worker。
#[tauri::command]
pub async fn bridge_call(
    plugin_id: String,
    method: String,
    payload: String,
    session_id: Option<String>,
    state: State<'_, TiangongApp>,
) -> Result<String, String> {
    // 前端实例只传会话 ID；sidecar 可写域必须由宿主从权威 Session 加载，
    // 不能采用抢到全局工具事件的界面实例所携带的工作区。新对话首条消息
    // 发出前只有前端本地编号（未落盘）或工作区未定：此时按无会话处理，
    // 走通用可写域——终端等面板在新对话中立即可用，不因会话未就绪被拒。
    let authoritative_workspace = if method.starts_with("sidecar.") {
        match session_id.filter(|id| !id.trim().is_empty()) {
            Some(session_id) => match state.inner().core_manager.load_session(&session_id) {
                Ok(session) => {
                    let cwd = session.cwd.trim();
                    if cwd.is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(cwd))
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "sidecar 调用所属会话加载失败，降级为通用可写域"
                    );
                    None
                }
            },
            None => None,
        }
    } else {
        None
    };
    if method.starts_with("plugin-dev.") || method.starts_with("dialog.") {
        return tokio::task::spawn_blocking(move || {
            tiangong_plugin_runtime::bridge_call_with_workspace(
                &plugin_id,
                &method,
                &payload,
                authoritative_workspace.as_deref(),
            )
        })
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string());
    }
    tiangong_plugin_runtime::bridge_call_with_workspace(
        &plugin_id,
        &method,
        &payload,
        authoritative_workspace.as_deref(),
    )
    .map_err(|error| error.to_string())
}

/// 订阅宿主事件通道（按 capabilities.events 授权放行）。
#[tauri::command]
pub async fn bridge_subscribe(plugin_id: String, channel: String) -> Result<(), String> {
    tiangong_plugin_runtime::bridge_subscribe(&plugin_id, &channel)
        .map_err(|error| error.to_string())
}

/// 取消订阅宿主事件通道。
#[tauri::command]
pub async fn bridge_unsubscribe(plugin_id: String, channel: String) -> Result<(), String> {
    tiangong_plugin_runtime::bridge_unsubscribe(&plugin_id, &channel)
        .map_err(|error| error.to_string())
}

/// 插件设置页贡献项（传给前端）。
#[derive(serde::Serialize)]
pub struct PluginContributionEntry {
    pub plugin_id: String,
    pub generation: u64,
    pub contribution_id: String,
    pub title: String,
    pub description: String,
    pub icon: String,
    pub group: String,
    pub has_view: bool,
}
