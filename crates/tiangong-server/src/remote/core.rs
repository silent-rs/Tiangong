use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::api::SharedState;
use crate::remote::event::{EventBus, TiangongEvent};
use anyhow::{Result, anyhow};
use tiangong_core::agent_input::AgentInputKind;
use tiangong_core::permission::TrustMode;
use tiangong_core::session::{MessageRole, Session};
use tiangong_media_archive::{AttachmentStore, AttachmentTransaction, RawAttachment};
use tiangong_types::{
    ContentBlock, MediaAsset, MediaKind, MessageContent, OutgoingMessage, StreamEvent,
};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone)]
pub struct ServerCoreManager {
    state: SharedState,
    core_manager: tiangong_app_state::app_state::CoreManager,
    event_bus: Arc<EventBus>,
    /// 同一会话的 Core 创建、消息投递和删除共用这一把锁。
    ///
    /// 锁对象不主动删除，避免旧等待者尚未退出时为同一 session 创建第二把锁。
    session_operation_locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    session_wait_locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    /// 串行全局配置刷新与新 Core 安装，避免新实例错过刚完成的配置更新。
    config_update_lock: Arc<AsyncMutex<()>>,
    trackers: Arc<Mutex<HashMap<String, Arc<ExecutionTracker>>>>,
    remote_sessions: Arc<Mutex<HashMap<String, String>>>,
}

impl ServerCoreManager {
    pub fn new(
        state: SharedState,
        core_manager: tiangong_app_state::app_state::CoreManager,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            state,
            core_manager,
            event_bus,
            session_operation_locks: Arc::new(Mutex::new(HashMap::new())),
            session_wait_locks: Arc::new(Mutex::new(HashMap::new())),
            config_update_lock: Arc::new(AsyncMutex::new(())),
            trackers: Arc::new(Mutex::new(HashMap::new())),
            remote_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 取消指定会话当前执行中的轮次：向活跃 turn 投递 Cancel 信号，Core
    /// 立即终止并以「已取消」错误唤醒全部等待方（快速停止，不排队）。
    /// 返回 true 表示命令已投递到活跃执行，false 表示当时没有可接受命令的活跃
    /// 执行。首轮未命中时只等待发送准备到消息入队的操作边界，不等待整轮完成。
    pub async fn cancel_session_turn(&self, session_id: &str) -> Result<bool> {
        let session_id = normalize_session_id(session_id)?;
        let core_manager = {
            let state = self.state.lock().await;
            state.core_manager.clone()
        };
        if core_manager.cancel_core(&session_id) {
            return Ok(true);
        }

        // 发送流程在消息入队前持有 operation lock；取消不拿 wait lock，避免等待整轮。
        let operation_lock = self.session_operation_lock(&session_id);
        let _operation_guard = operation_lock.lock().await;
        Ok(core_manager.cancel_core(&session_id))
    }

    /// 从 App State 刷新全局模板，并按 session 为每个 Core 替换独立配置快照。
    pub async fn sync_config_from_state(&self) {
        let _config_guard = self.config_update_lock.lock().await;
        let (template, session_configs) = {
            let state = self.state.lock().await;
            let mut template = state.config.to_core_config();
            template.trust_mode = TrustMode::FullTrust;
            let session_configs = state
                .core_manager
                .list_session_metadata()
                .iter()
                .map(|metadata| {
                    let mut config = state.config.to_core_config();
                    config.trust_mode = TrustMode::FullTrust;
                    (metadata.id.clone(), config)
                })
                .collect::<HashMap<_, _>>();
            (template, session_configs)
        };

        self.core_manager.sync_config(template, &session_configs);
    }

    fn session_operation_lock(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = match self.session_operation_locks.lock() {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!(error = %error, "会话操作锁表已损坏，恢复后继续");
                error.into_inner()
            }
        };
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn session_wait_lock(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = match self.session_wait_locks.lock() {
            Ok(locks) => locks,
            Err(poisoned) => poisoned.into_inner(),
        };
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    pub async fn send_connector_message_and_wait(
        &self,
        connector: &str,
        channel_id: &str,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
    ) -> Result<(String, OutgoingMessage)> {
        let session_id = self
            .resolve_connector_session_id(connector, channel_id)
            .await?;
        self.send_message_and_wait(&session_id, content, message_id, media)
            .await
    }

    /// 发送消息到 Core。所有入口都串行到当前轮次真正结束，避免后台消息在等待型
    /// 请求执行期间被吸收到同一轮、导致回复归属错乱。
    pub async fn send_message(
        &self,
        requested_session_id: &str,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
    ) -> Result<()> {
        let _ = self
            .send_message_and_wait(requested_session_id, content, message_id, media)
            .await?;
        Ok(())
    }

    pub async fn send_message_and_wait(
        &self,
        requested_session_id: &str,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
    ) -> Result<(String, OutgoingMessage)> {
        let requested_session_id = normalize_session_id(requested_session_id)?;
        // 一个 Core 执行可吸收运行中追加消息但只产生一个终态；等待型入口按会话
        // 串行到终态，确保每个调用拿到与自身消息对应的回复。DELETE 使用独立锁，
        // 仍可随时取消正在等待的 Core。
        let wait_lock = self.session_wait_lock(&requested_session_id);
        let _wait_guard = wait_lock.lock().await;
        let session_lock = self.session_operation_lock(&requested_session_id);
        let session_guard = session_lock.lock().await;
        let session_existed = self
            .state
            .lock()
            .await
            .core_manager
            .session_exists(&requested_session_id);
        let session_id = self.ensure_core_locked(&requested_session_id).await?;

        let msg_id = message_id.unwrap_or_else(|| scru128::new().to_string());
        // 附件准备成功后才登记 waiter，准备失败不会污染 tracker。
        let (transaction, prepared) = self
            .prepare_user_message(msg_id.clone(), content, media)
            .await?;
        let tracker = self.tracker_for(&session_id);
        let turn_id = tracker.start_turn(msg_id.clone());
        // fire-and-forget：enqueue 成功即提交附件。worker 在消息持久化失败时会发出
        // Error 终态，由下方 wait_for_turn 捕获并转为失败返回。
        let created_paths = commit_enqueued_attachment_transaction(transaction);
        if let Err(error) = self.enqueue_prepared(&session_id, &msg_id, prepared) {
            cleanup_created_attachment_paths(&created_paths).ok();
            tracker.cancel_turn(turn_id);
            return Err(error);
        }
        if !session_existed {
            let mut state = self.state.lock().await;
            if state.active_session_id.trim().is_empty() {
                state.active_session_id = session_id.clone();
            }
            drop(state);
            self.event_bus
                .publish(TiangongEvent::SessionCreated(session_id.clone()));
        }
        // 消息已入队；完整 turn 可能持续较久，释放锁让 DELETE 可以取消 Core。
        drop(session_guard);

        let tracker_for_wait = tracker.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let outcome = tracker_for_wait.wait_for_turn(turn_id);
            let _ = tx.send(outcome);
        });
        let outcome = match rx.await {
            Ok(outcome) => outcome,
            Err(_) => {
                tracker.cancel_turn(turn_id);
                return Err(anyhow!("等待执行结果的线程意外退出"));
            }
        };
        let response = match outcome {
            TurnOutcome::Completed => self.last_assistant_outgoing(&session_id).await,
            TurnOutcome::Failed(message) => return Err(anyhow!(message)),
        };

        Ok((session_id, response))
    }

    /// 停止并等待目标 Core 退出后，再删除会话状态和文件。
    ///
    /// 返回 `false` 表示会话本来就不存在；即使如此也会清理同 ID 的孤立 Core。
    pub async fn delete_session(&self, requested_session_id: &str) -> Result<bool> {
        let session_id = normalize_session_id(requested_session_id)?;
        let session_lock = self.session_operation_lock(&session_id);
        let _session_guard = session_lock.lock().await;

        let existed = self.core_manager.session_exists(&session_id);
        if !existed {
            return Ok(false);
        }

        // 逻辑删除：原子移动到 trash + 取消 Core。
        self.core_manager
            .delete_session(&session_id)
            .await
            .map_err(anyhow::Error::msg)?;

        self.trackers
            .lock()
            .map_err(|error| anyhow!("执行跟踪锁已损坏：{error}"))?
            .remove(&session_id);
        self.remote_sessions
            .lock()
            .map_err(|error| anyhow!("远程会话映射锁已损坏：{error}"))?
            .retain(|_, mapped_session_id| mapped_session_id != &session_id);

        let mut state = self.state.lock().await;
        if state.active_session_id == session_id {
            state.active_session_id.clear();
        }
        Ok(true)
    }

    async fn prepare_user_message(
        &self,
        message_id: String,
        content: String,
        media: Vec<MediaAsset>,
    ) -> Result<(AttachmentTransaction, Vec<ContentBlock>)> {
        let raw = media.into_iter().map(raw_attachment_from_media).collect();
        let media_root = self.state.lock().await.config.storage_root.join("media");
        tokio::task::spawn_blocking(move || {
            prepare_user_message_blocking(media_root, raw, message_id, content)
        })
        .await
        .map_err(|error| anyhow!("附件准备任务失败：{error}"))?
        .map_err(|error| anyhow!("附件准备失败：{error}"))
    }

    fn enqueue_prepared(
        &self,
        session_id: &str,
        message_id: &str,
        prepared: Vec<ContentBlock>,
    ) -> Result<()> {
        self.core_manager
            .deliver_to_core_if_live(
                session_id,
                AgentInputKind::prepared_with_id(message_id.to_string(), prepared),
            )
            .then_some(())
            .ok_or_else(|| anyhow!("消息投递失败：会话 Core 不存在或已关闭"))
    }

    /// 调用方必须持有目标 session 的 `session_operation_lock`。
    async fn ensure_core_locked(&self, requested_session_id: &str) -> Result<String> {
        let session_id = normalize_session_id(requested_session_id)?;

        if self.core_manager.has_live_core(&session_id) {
            return Ok(session_id);
        }

        let (stream_tx, stream_rx) = mpsc::channel::<StreamEvent>();

        let _config_guard = self.config_update_lock.lock().await;

        // async 初始化期间仍做第二次检查。会话锁保证正常路径不会并发创建，
        // 这里同时防御未来新增的未加锁入口。
        if self.core_manager.has_live_core(&session_id) {
            return Ok(session_id);
        }

        let (mut session_config, storage_root, workspace_dir) = {
            let state = self.state.lock().await;
            let workspace_dir = state
                .core_manager
                .list_session_metadata()
                .into_iter()
                .find(|metadata| metadata.id == session_id)
                .map(|metadata| metadata.cwd)
                .filter(|cwd| !cwd.trim().is_empty())
                .unwrap_or_else(|| state.workspace_dir.clone());
            (
                state.config.to_core_config(),
                state.config.storage_root.clone(),
                workspace_dir,
            )
        };
        session_config.trust_mode = TrustMode::FullTrust;

        // 与桌面端一致：Core 只持有 runtime 聚合桥，在需要新建 Core 时才为该
        // 会话实例化；已安装插件由桥在被调用时按需装载。
        let plugin_session_id = session_id.clone();
        let ensured = self
            .core_manager
            .ensure_core(
                &session_id,
                session_config,
                workspace_dir,
                None,
                stream_tx,
                move || {
                    vec![tiangong_plugin_runtime::RuntimeCorePlugin::new(
                        tiangong_plugin_runtime::registry::RuntimeKind::Server,
                        storage_root,
                        plugin_session_id,
                    )
                        as std::sync::Arc<dyn tiangong_core::core::Plugin>]
                },
            )
            .await
            .map_err(anyhow::Error::msg)?;
        let actual_session_id = ensured.session_id;

        if !ensured.is_new {
            return Ok(actual_session_id);
        }

        let tracker = self.tracker_for(&actual_session_id);
        self.spawn_stream_forwarder(actual_session_id.clone(), stream_rx, tracker);

        Ok(actual_session_id)
    }

    async fn resolve_connector_session_id(
        &self,
        connector: &str,
        channel_id: &str,
    ) -> Result<String> {
        let channel_id = channel_id.trim();
        if channel_id.is_empty() {
            let state = self.state.lock().await;
            return Ok(state.active_session_id.as_str().to_string());
        }

        let key = remote_session_key(connector, channel_id);
        if let Some(session_id) = self.remote_sessions.lock().unwrap().get(&key).cloned() {
            return Ok(session_id);
        }

        let (session_id, created) = {
            let mut state = self.state.lock().await;
            let resolved =
                resolve_or_create_connector_session(&state.core_manager, connector, channel_id)?;
            if resolved.1 && state.active_session_id.trim().is_empty() {
                state.active_session_id = resolved.0.clone();
            }
            resolved
        };
        if created {
            self.event_bus
                .publish(TiangongEvent::SessionCreated(session_id.clone()));
        }

        self.remote_sessions
            .lock()
            .unwrap()
            .insert(key, session_id.clone());
        Ok(session_id)
    }

    fn spawn_stream_forwarder(
        &self,
        session_id: String,
        stream_rx: mpsc::Receiver<StreamEvent>,
        tracker: Arc<ExecutionTracker>,
    ) {
        let event_bus = self.event_bus.clone();
        thread::spawn(move || {
            for event in stream_rx {
                sync_stream_event_to_state(&event_bus, &session_id, &event);
                // 先发布终态事件，再唤醒本次调用的等待者。
                tracker.observe_event(&event);
            }
            tracker.fail_all("Core 事件流已关闭");
        });
    }

    fn tracker_for(&self, session_id: &str) -> Arc<ExecutionTracker> {
        let mut trackers = self.trackers.lock().unwrap();
        trackers
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(ExecutionTracker::default()))
            .clone()
    }

    async fn last_assistant_outgoing(&self, session_id: &str) -> OutgoingMessage {
        // 消息遍历需完整 Session;从磁盘 load(issue #245:真相源归磁盘)。
        let core_manager = self.state.lock().await.core_manager.clone();
        let Ok(session) = core_manager.load_session(session_id) else {
            return text_outgoing("处理完成");
        };
        assistant_outgoing_after_last_user(&session)
    }
}

fn assistant_outgoing_after_last_user(session: &Session) -> OutgoingMessage {
    let Some(last_user) = session
        .messages
        .iter()
        .rfind(|message| message.role == MessageRole::User)
    else {
        return text_outgoing("处理完成");
    };
    assistant_outgoing_after_user(session, &last_user.id)
}

/// 提取某一条稳定用户消息对应的回复。
///
/// 以用户消息 ID 划定区间，而不是读取“最后一轮”，供 Desktop 内嵌 Server 在同一
/// 会话存在排队轮次时准确返回各自的结果。
pub fn assistant_outgoing_after_user(session: &Session, user_message_id: &str) -> OutgoingMessage {
    let Some(user_index) = session
        .messages
        .iter()
        .position(|message| message.id == user_message_id && message.role == MessageRole::User)
    else {
        return text_outgoing("处理完成");
    };
    // 仅聚合主 Agent 回复（worker_id 为空）；子 Agent 过程消息不落盘（桌面端
    // 走内存视图），磁盘会话中不存在需要聚合的 worker 回复。
    let selected = session
        .messages
        .iter()
        .skip(user_index + 1)
        .take_while(|message| {
            message.role != MessageRole::User || message.worker_id.as_deref().is_some()
        })
        .filter(|message| message.role == MessageRole::Assistant && message.worker_id.is_none());
    let mut texts = Vec::new();
    let mut media_items = Vec::new();
    for message in selected {
        let (text, referenced_files) =
            extract_local_files_from_text(session, &message.text_content());
        if !text.trim().is_empty() {
            texts.clear();
            texts.push(text);
        }
        media_items.extend(referenced_files);
        for block in &message.content {
            match block {
                tiangong_types::ContentBlock::Media {
                    kind,
                    url,
                    mime_type,
                    title,
                } => {
                    media_items.push(MediaAsset {
                        kind: *kind,
                        url: url.clone(),
                        mime_type: mime_type.clone(),
                        title: title.clone(),
                        capability: None,
                    });
                }
                tiangong_types::ContentBlock::Image { asset, .. }
                | tiangong_types::ContentBlock::AssetReference { asset } => {
                    media_items.push(MediaAsset {
                        kind: asset.kind,
                        url: asset.local_path.clone(),
                        mime_type: Some(asset.mime_type.clone()),
                        title: Some(asset.original_name.clone()),
                        capability: None,
                    });
                }
                tiangong_types::ContentBlock::Text { .. }
                | tiangong_types::ContentBlock::ModelInstruction { .. } => {}
            }
        }
    }
    let latest_text = texts.join("\n\n");
    let mut seen_urls = HashSet::new();
    media_items.retain(|media| seen_urls.insert(media.url.clone()));

    if !media_items.is_empty() {
        return media_outgoing(media_items, latest_text);
    }

    if latest_text.trim().is_empty() {
        text_outgoing("处理完成")
    } else {
        text_outgoing(latest_text)
    }
}

/// 把最终回复中的本地 Markdown / 纯文本路径转换为结构化附件。
///
/// 只允许当前会话工作区与天工媒体目录；路径会先 canonicalize，因此通过符号链接
/// 指向允许目录之外的文件也不会被返回给外部 Bot。
fn extract_local_files_from_text(session: &Session, text: &str) -> (String, Vec<MediaAsset>) {
    let roots = trusted_outgoing_roots(session);
    if roots.is_empty() || text.trim().is_empty() {
        return (text.to_string(), Vec::new());
    }

    let (text, mut media) = extract_markdown_local_files(text, &roots);
    let (text, plain_media) = extract_plain_local_files(&text, &roots);
    media.extend(plain_media);
    (text.trim().to_string(), media)
}

fn trusted_outgoing_roots(session: &Session) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(storage_root) = session.bound_storage_root() {
        candidates.push(storage_root.join("media"));
    }
    if !session.cwd.trim().is_empty() {
        candidates.push(PathBuf::from(session.cwd.trim()));
    }

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .filter(|path| path.is_dir() && seen.insert(path.clone()))
        .collect()
}

fn extract_markdown_local_files(text: &str, roots: &[PathBuf]) -> (String, Vec<MediaAsset>) {
    let mut output = String::with_capacity(text.len());
    let mut media = Vec::new();
    let mut copied_until = 0;
    let mut search_from = 0;

    while let Some(open_offset) = text[search_from..].find('[') {
        let open = search_from + open_offset;
        let label_start = open + 1;
        let Some(label_end_offset) = text[label_start..].find(']') else {
            search_from = label_start;
            continue;
        };
        let label_end = label_start + label_end_offset;
        if text.as_bytes().get(label_end + 1) != Some(&b'(') {
            search_from = label_end + 1;
            continue;
        }
        let target_start = label_end + 2;
        let Some(target_end_offset) = text[target_start..].find(')') else {
            break;
        };
        let target_end = target_start + target_end_offset;
        let reference_end = target_end + 1;
        let is_image = open > 0 && text.as_bytes()[open - 1] == b'!';
        let reference_start = if is_image { open - 1 } else { open };
        let label = text[label_start..label_end].trim();
        let target = normalize_local_target(&text[target_start..target_end]);

        let Some(target) = target.filter(|target| target.is_absolute()) else {
            search_from = reference_end;
            continue;
        };

        output.push_str(&text[copied_until..reference_start]);
        if !is_image {
            output.push_str(label);
        }
        if let Some(asset) = trusted_media_asset(&target, label, is_image, roots) {
            media.push(asset);
        }
        copied_until = reference_end;
        search_from = reference_end;
    }

    output.push_str(&text[copied_until..]);
    (output, media)
}

fn extract_plain_local_files(text: &str, roots: &[PathBuf]) -> (String, Vec<MediaAsset>) {
    let markers = roots
        .iter()
        .filter_map(|root| root.to_str())
        .filter(|root| !root.is_empty())
        .collect::<Vec<_>>();
    if markers.is_empty() {
        return (text.to_string(), Vec::new());
    }

    let mut output = String::with_capacity(text.len());
    let mut media = Vec::new();
    let mut copied_until = 0;
    let mut search_from = 0;

    while search_from < text.len() {
        let next = markers
            .iter()
            .filter_map(|marker| {
                text[search_from..]
                    .find(marker)
                    .map(|offset| (search_from + offset, *marker))
            })
            .min_by_key(|(position, _)| *position);
        let Some((start, marker)) = next else {
            break;
        };

        let path_tail_start = start + marker.len();
        let end = text[path_tail_start..]
            .char_indices()
            .find_map(|(offset, ch)| {
                is_plain_path_terminator(ch).then_some(path_tail_start + offset)
            })
            .unwrap_or(text.len());
        let candidate = text[start..end].trim_end_matches(is_trailing_path_punctuation);
        let candidate_end = start + candidate.len();

        if let Some(asset) = trusted_media_asset(Path::new(candidate), "", false, roots) {
            output.push_str(&text[copied_until..start]);
            output.push_str(asset.title.as_deref().unwrap_or("文件"));
            media.push(asset);
            copied_until = candidate_end;
            search_from = candidate_end;
        } else {
            search_from = path_tail_start.max(start + 1);
        }
    }

    output.push_str(&text[copied_until..]);
    (output, media)
}

fn normalize_local_target(target: &str) -> Option<PathBuf> {
    let mut target = target.trim();
    if target.starts_with('<') && target.ends_with('>') && target.len() >= 2 {
        target = &target[1..target.len() - 1];
    }
    if let Some(path) = target.strip_prefix("file://") {
        target = path;
    }
    (!target.is_empty()).then(|| PathBuf::from(target))
}

fn trusted_media_asset(
    path: &Path,
    label: &str,
    force_image: bool,
    roots: &[PathBuf],
) -> Option<MediaAsset> {
    let path = std::fs::canonicalize(path).ok()?;
    if !path.is_file() || !roots.iter().any(|root| path.starts_with(root)) {
        return None;
    }

    let fallback_title = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "文件".to_string());
    let title = if label.trim().is_empty() {
        fallback_title
    } else {
        label.trim().to_string()
    };
    let kind = if force_image {
        MediaKind::Image
    } else {
        media_kind_for_path(&path)
    };
    Some(MediaAsset {
        kind,
        url: path.to_string_lossy().to_string(),
        mime_type: mime_type_for_path(&path).map(str::to_string),
        title: Some(title),
        capability: None,
    })
}

fn media_kind_for_path(path: &Path) -> MediaKind {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif" => MediaKind::Image,
        "mp4" | "mov" | "webm" | "m4v" => MediaKind::Video,
        "mp3" | "wav" | "m4a" | "ogg" | "flac" | "aac" => MediaKind::Audio,
        _ => MediaKind::File,
    }
}

fn mime_type_for_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "avif" => Some("image/avif"),
        "mp4" | "m4v" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        "webm" => Some("video/webm"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "m4a" => Some("audio/mp4"),
        "ogg" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        "aac" => Some("audio/aac"),
        "pdf" => Some("application/pdf"),
        "txt" | "md" => Some("text/plain"),
        _ => None,
    }
}

fn is_plain_path_terminator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            ')' | ']' | '}' | '>' | '`' | '\'' | '"' | '，' | '。' | '；' | '！' | '？' | '、'
        )
}

fn is_trailing_path_punctuation(ch: char) -> bool {
    matches!(ch, '.' | ',' | ';' | ':' | '，' | '。' | '；' | '：')
}

fn raw_attachment_from_media(asset: MediaAsset) -> RawAttachment {
    RawAttachment {
        kind: asset.kind,
        source: asset.url,
        mime_type: asset.mime_type,
        original_name: asset.title,
    }
}

fn commit_enqueued_attachment_transaction(
    transaction: AttachmentTransaction,
) -> Vec<std::path::PathBuf> {
    let created_paths = transaction.newly_created_paths().to_vec();
    transaction.commit();
    created_paths
}

fn cleanup_created_attachment_paths(paths: &[std::path::PathBuf]) -> Result<()> {
    let mut failures = Vec::new();
    for path in paths {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(failures.join("；")))
    }
}

fn normalize_session_id(session_id: &str) -> Result<String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Err(anyhow!("会话 ID 不能为空"));
    }
    Ok(session_id.to_string())
}

fn prepare_user_message_blocking(
    media_root: std::path::PathBuf,
    raw: Vec<RawAttachment>,
    message_id: String,
    content: String,
) -> std::result::Result<(AttachmentTransaction, Vec<ContentBlock>), String> {
    let mut transaction = AttachmentStore::new(media_root).store_batch(raw)?;
    let prepared = transaction.prepare_message(&message_id, content)?;
    Ok((transaction, prepared))
}

fn remote_session_key(connector: &str, channel_id: &str) -> String {
    format!("{}:{}", connector.trim(), channel_id.trim())
}

pub fn resolve_or_create_connector_session(
    core_manager: &tiangong_app_state::app_state::CoreManager,
    connector: &str,
    channel_id: &str,
) -> Result<(String, bool)> {
    let channel_id = channel_id.trim();
    if channel_id.is_empty() {
        return Err(anyhow!("外部通道 ID 不能为空"));
    }

    let title = remote_session_title(connector, channel_id);
    let sessions = core_manager.list_session_metadata();
    if let Some(metadata) = sessions.iter().find(|metadata| metadata.id == channel_id) {
        return Ok((metadata.id.clone(), false));
    }
    if let Some(metadata) = sessions.iter().find(|metadata| metadata.title == title) {
        return Ok((metadata.id.clone(), false));
    }

    let storage_root = core_manager.storage_root();
    let mut session = Session::new_isolated(title, storage_root);
    session.trust_mode = TrustMode::FullTrust;
    session.bind_storage_root(storage_root.to_path_buf());
    session
        .try_persist_to_disk()
        .map_err(|error| anyhow!("创建外部通道会话失败：{error}"))?;
    Ok((session.id, true))
}

fn remote_session_title(connector: &str, channel_id: &str) -> String {
    let connector = connector.trim();
    let channel_id = channel_id.trim();
    let raw = if connector.is_empty() {
        format!("外部通道 {channel_id}")
    } else {
        format!("{connector} {channel_id}")
    };
    raw.chars().take(80).collect()
}

fn text_outgoing(text: impl Into<String>) -> OutgoingMessage {
    OutgoingMessage {
        content: MessageContent::Text(text.into()),
        attachments: Vec::new(),
        reply_to: None,
    }
}

fn media_outgoing(media: Vec<MediaAsset>, caption: String) -> OutgoingMessage {
    let mut media = media.into_iter();
    let first = media.next().expect("media_outgoing 至少需要一个媒体项");
    if !caption.trim().is_empty() && matches!(first.kind, MediaKind::File | MediaKind::Audio) {
        let attachments = std::iter::once(first)
            .chain(media)
            .map(|media| media_content(media, None))
            .collect();
        return OutgoingMessage {
            content: MessageContent::Text(caption),
            attachments,
            reply_to: None,
        };
    }
    let content = media_content(first, (!caption.trim().is_empty()).then_some(caption));
    let attachments = media
        .map(|media| media_content(media, None))
        .collect::<Vec<_>>();
    OutgoingMessage {
        content,
        attachments,
        reply_to: None,
    }
}

fn media_content(media: MediaAsset, caption: Option<String>) -> MessageContent {
    match media.kind {
        MediaKind::Image => MessageContent::Image {
            url: media.url,
            caption,
        },
        MediaKind::Video => MessageContent::Video {
            url: media.url,
            caption,
        },
        MediaKind::Audio => MessageContent::Audio {
            url: media.url,
            duration: None,
        },
        MediaKind::File => MessageContent::File {
            name: media.title.unwrap_or_else(|| "文件".to_string()),
            url: media.url,
        },
    }
}

#[derive(Debug, Clone)]
enum TurnOutcome {
    Completed,
    Failed(String),
}

#[derive(Default)]
struct ExecutionTracker {
    state: Mutex<ExecutionTrackerState>,
    notify: Condvar,
}

#[derive(Default)]
struct ExecutionTrackerState {
    next_turn_id: u64,
    pending_by_message: HashMap<String, u64>,
    active_turns: VecDeque<u64>,
    outcomes: HashMap<u64, TurnOutcome>,
}

impl ExecutionTracker {
    fn start_turn(&self, message_id: String) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.next_turn_id += 1;
        let turn_id = state.next_turn_id;
        state.pending_by_message.insert(message_id, turn_id);
        state.outcomes.remove(&turn_id);
        turn_id
    }

    fn cancel_turn(&self, turn_id: u64) {
        let mut state = self.state.lock().unwrap();
        remove_turn_state(&mut state, turn_id);
        self.notify.notify_all();
    }

    fn observe_event(&self, event: &StreamEvent) {
        let mut state = self.state.lock().unwrap();

        match event {
            StreamEvent::UserMessage { message_id, .. } => {
                if let Some(turn_id) = state.pending_by_message.remove(message_id) {
                    state.active_turns.push_back(turn_id);
                }
            }
            StreamEvent::Done { .. } => {
                if let Some(turn_id) = state.active_turns.pop_front() {
                    state.outcomes.insert(turn_id, TurnOutcome::Completed);
                    self.notify.notify_all();
                }
            }
            StreamEvent::Error { message } => {
                if let Some(turn_id) = state.active_turns.pop_front() {
                    state
                        .outcomes
                        .insert(turn_id, TurnOutcome::Failed(message.clone()));
                    self.notify.notify_all();
                }
            }
            _ => {}
        }
    }

    fn wait_for_turn(&self, turn_id: u64) -> TurnOutcome {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(outcome) = state.outcomes.remove(&turn_id) {
                return outcome;
            }
            match self.notify.wait(state) {
                Ok(guard) => state = guard,
                Err(poisoned) => {
                    state = poisoned.into_inner();
                }
            }
        }
    }

    fn fail_all(&self, message: &str) {
        let mut state = self.state.lock().unwrap();
        let mut turn_ids = state
            .pending_by_message
            .values()
            .copied()
            .collect::<Vec<_>>();
        turn_ids.extend(state.active_turns.iter().copied());
        state.pending_by_message.clear();
        state.active_turns.clear();
        for turn_id in turn_ids {
            state
                .outcomes
                .entry(turn_id)
                .or_insert_with(|| TurnOutcome::Failed(message.to_string()));
        }
        self.notify.notify_all();
    }

    #[cfg(test)]
    fn registered_turn_count(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.pending_by_message.len() + state.active_turns.len() + state.outcomes.len()
    }
}

fn remove_turn_state(state: &mut ExecutionTrackerState, turn_id: u64) {
    state
        .pending_by_message
        .retain(|_, pending_turn_id| *pending_turn_id != turn_id);
    state
        .active_turns
        .retain(|active_turn_id| *active_turn_id != turn_id);
    state.outcomes.remove(&turn_id);
}

fn sync_stream_event_to_state(event_bus: &Arc<EventBus>, session_id: &str, event: &StreamEvent) {
    let completion_event = match event {
        StreamEvent::Done { .. } => Some(true),
        StreamEvent::Error { .. } => Some(false),
        _ => None,
    };

    if let Some(success) = completion_event {
        event_bus.publish(TiangongEvent::TurnCompleted {
            session_id: session_id.to_string(),
            success,
        });
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::path::Path;

    pub(crate) static STORAGE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(crate) struct TestHomeGuard {
        previous_home: Option<OsString>,
    }

    impl TestHomeGuard {
        pub(crate) fn new(home: &Path) -> Self {
            std::fs::create_dir_all(home).unwrap();
            let previous_home = std::env::var_os("HOME");
            // SAFETY: 所有修改 HOME 的 Server 测试均由 STORAGE_TEST_LOCK 串行。
            unsafe { std::env::set_var("HOME", home) };
            Self { previous_home }
        }
    }

    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            match self.previous_home.take() {
                Some(home) => {
                    // SAFETY: 见 TestHomeGuard::new。
                    unsafe { std::env::set_var("HOME", home) };
                }
                None => {
                    // SAFETY: 见 TestHomeGuard::new。
                    unsafe { std::env::remove_var("HOME") };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use tiangong_core::config::core::CoreConfig;
    use tiangong_core::core::Plugin;
    use tiangong_core::session::Message;
    use tiangong_types::ContentBlock;

    use super::test_support::{STORAGE_TEST_LOCK, TestHomeGuard};

    fn isolated_test_manager(
        _root: &Path,
    ) -> (Arc<ServerCoreManager>, Session, PathBuf, SharedState) {
        let mut state = tiangong_app_state::app_state::TiangongState::new();
        let storage_root = state.config.storage_root.clone();
        // 构造一个隔离 Session 并直接落盘，作为测试基准会话。
        let mut session = Session::new_isolated("测试会话", &storage_root);
        session.bind_storage_root(storage_root.clone());
        session.try_persist_to_disk().unwrap();
        state.active_session_id = session.id.clone();
        let core_manager = state.core_manager.clone();
        let session_path = storage_root
            .join("sessions")
            .join(format!("{}.json", session.id));
        let state = Arc::new(AsyncMutex::new(state));
        let manager = Arc::new(ServerCoreManager::new(
            state.clone(),
            core_manager,
            Arc::new(EventBus::default()),
        ));
        (manager, session, session_path, state)
    }

    #[test]
    fn committed_enqueued_attachments_survive_until_targeted_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let transaction = AttachmentStore::new(root.path().join("archive"))
            .store_batch(vec![RawAttachment {
                kind: MediaKind::File,
                source: "data:text/plain;base64,aGVsbG8=".to_string(),
                mime_type: Some("text/plain".to_string()),
                original_name: Some("hello.txt".to_string()),
            }])
            .unwrap();

        let created_paths = commit_enqueued_attachment_transaction(transaction);
        assert_eq!(created_paths.len(), 1);
        assert!(created_paths[0].is_file());

        cleanup_created_attachment_paths(&created_paths).unwrap();
        assert!(!created_paths[0].exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_idle_session_returns_false() {
        let _storage_guard = STORAGE_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let _home_guard = TestHomeGuard::new(&root.path().join("home"));
        let (manager, session, _session_path, _state) = isolated_test_manager(root.path());

        assert!(!manager.cancel_session_turn(&session.id).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_waits_only_for_operation_boundary() {
        let _storage_guard = STORAGE_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let _home_guard = TestHomeGuard::new(&root.path().join("home"));
        let (manager, session, _session_path, _state) = isolated_test_manager(root.path());
        let wait_lock = manager.session_wait_lock(&session.id);
        let _wait_guard = wait_lock.lock().await;
        let operation_lock = manager.session_operation_lock(&session.id);
        let operation_guard = operation_lock.lock_owned().await;

        let cancelling = {
            let manager = manager.clone();
            let session_id = session.id.clone();
            tokio::spawn(async move { manager.cancel_session_turn(&session_id).await })
        };
        tokio::task::yield_now().await;
        assert!(!cancelling.is_finished(), "取消应等待发送操作边界");

        drop(operation_guard);
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), cancelling)
            .await
            .expect("释放操作锁后取消应立即返回")
            .expect("取消任务不应 panic")
            .unwrap();
        assert!(!result);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_state_sync_does_not_rewrite_session_file() {
        let _storage_guard = STORAGE_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let _home_guard = TestHomeGuard::new(&root.path().join("home"));
        let (_manager, session, session_path, state) = isolated_test_manager(root.path());
        let authoritative_session = std::fs::read(&session_path).unwrap();

        let session_id = session.id.clone();
        tokio::task::spawn_blocking(move || {
            sync_stream_event_to_state(
                &Arc::new(EventBus::default()),
                &session_id,
                &StreamEvent::Done { usage: None },
            );
        })
        .await
        .expect("终态同步不应 panic");

        // session.json 应保持 Core 写入的原始内容，未被宿主重写。
        assert_eq!(std::fs::read(&session_path).unwrap(), authoritative_session);
        // 终态事件不改变本次运行中的活动会话。
        assert_eq!(
            state.lock().await.active_session_id.as_str(),
            session.id.as_str()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_session_install_is_serial_and_never_overwrites_the_winner() {
        let _storage_guard = STORAGE_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let _home_guard = TestHomeGuard::new(&root.path().join("home"));
        let (manager, session, _session_path, _state) = isolated_test_manager(root.path());
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let first_task = {
            let manager = manager.clone();
            let session_id = session.id.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let lock = manager.session_operation_lock(&session_id);
                let _guard = lock.lock().await;
                let (stream_tx, _stream_rx) = std::sync::mpsc::channel();
                manager
                    .core_manager
                    .ensure_core(
                        &session_id,
                        CoreConfig::default(),
                        String::new(),
                        None,
                        stream_tx,
                        Vec::new as fn() -> Vec<Arc<dyn Plugin>>,
                    )
                    .await
                    .map(|ensured| ensured.is_new)
            })
        };
        let second_task = {
            let manager = manager.clone();
            let session_id = session.id.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let lock = manager.session_operation_lock(&session_id);
                let _guard = lock.lock().await;
                let (stream_tx, _stream_rx) = std::sync::mpsc::channel();
                manager
                    .core_manager
                    .ensure_core(
                        &session_id,
                        CoreConfig::default(),
                        String::new(),
                        None,
                        stream_tx,
                        Vec::new as fn() -> Vec<Arc<dyn Plugin>>,
                    )
                    .await
                    .map(|ensured| ensured.is_new)
            })
        };
        barrier.wait().await;
        let first_installed = first_task.await.unwrap().unwrap();
        let second_installed = second_task.await.unwrap().unwrap();
        assert_ne!(first_installed, second_installed);
        assert_eq!(manager.core_manager.registry().len(), 1);

        // 唯一存活的 Core 必属于胜出方，且其 session_id 与预期一致。
        let stored_session_id = manager
            .core_manager
            .registry()
            .get(&session.id)
            .map(|core| core.session_id().to_string());
        assert_eq!(stored_session_id.as_deref(), Some(session.id.as_str()));

        assert!(manager.delete_session(&session.id).await.unwrap());
    }

    #[test]
    fn attachment_preparation_failure_leaves_tracker_empty() {
        let root = tempfile::tempdir().unwrap();
        let tracker = ExecutionTracker::default();
        let raw = vec![RawAttachment {
            kind: MediaKind::Image,
            source: "data:image/png;base64,%%%invalid%%%".to_string(),
            mime_type: Some("image/png".to_string()),
            original_name: Some("broken.png".to_string()),
        }];

        let result = prepare_user_message_blocking(
            root.path().join("media"),
            raw,
            "message-broken".to_string(),
            "broken".to_string(),
        );

        assert!(result.is_err());
        assert_eq!(tracker.registered_turn_count(), 0);
    }

    #[test]
    fn tracker_correlates_events_by_message_and_cleans_cancelled_turn() {
        let tracker = ExecutionTracker::default();
        let first = tracker.start_turn("message-1".to_string());
        let second = tracker.start_turn("message-2".to_string());

        tracker.observe_event(&StreamEvent::UserMessage {
            message_id: "message-2".to_string(),
            content: "second".to_string(),
            content_blocks: Vec::new(),
            media: Vec::new(),
        });
        tracker.observe_event(&StreamEvent::Done { usage: None });
        assert!(matches!(
            tracker.wait_for_turn(second),
            TurnOutcome::Completed
        ));

        tracker.cancel_turn(first);
        assert_eq!(tracker.registered_turn_count(), 0);
    }

    #[test]
    fn tracker_fails_all_waiters_when_core_stream_closes() {
        let tracker = ExecutionTracker::default();
        let pending = tracker.start_turn("message-pending".to_string());
        let active = tracker.start_turn("message-active".to_string());
        tracker.observe_event(&StreamEvent::UserMessage {
            message_id: "message-active".to_string(),
            content: "active".to_string(),
            content_blocks: Vec::new(),
            media: Vec::new(),
        });

        tracker.fail_all("Core 事件流已关闭");

        assert!(matches!(
            tracker.wait_for_turn(pending),
            TurnOutcome::Failed(message) if message.contains("事件流已关闭")
        ));
        assert!(matches!(
            tracker.wait_for_turn(active),
            TurnOutcome::Failed(message) if message.contains("事件流已关闭")
        ));
    }

    #[test]
    fn outgoing_is_correlated_to_the_requested_user_message_id() {
        let mut session = Session::new("correlated-outgoing");
        session.append_prepared_user_message_with_id(
            "user-1".to_string(),
            vec![ContentBlock::text("first")],
        );
        let mut worker_input = Message::new(MessageRole::User, "worker input");
        worker_input.worker_id = Some("agent:dev:one".to_string());
        session.messages.push(worker_input);
        let mut worker_output = Message::new(MessageRole::Assistant, "worker output");
        worker_output.worker_id = Some("agent:dev:one".to_string());
        session.messages.push(worker_output);
        session.append_message(MessageRole::Assistant, "first reply");
        session.append_prepared_user_message_with_id(
            "user-2".to_string(),
            vec![ContentBlock::text("second")],
        );
        session.append_message(MessageRole::Assistant, "second reply");

        let first = assistant_outgoing_after_user(&session, "user-1");
        let second = assistant_outgoing_after_user(&session, "user-2");
        assert!(matches!(first.content, MessageContent::Text(ref text) if text == "first reply"));
        assert!(matches!(second.content, MessageContent::Text(ref text) if text == "second reply"));
    }

    #[test]
    fn outgoing_converts_media_and_workspace_paths_but_rejects_other_files() {
        let root = tempfile::tempdir().unwrap();
        let media_dir = root.path().join("media/images");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&media_dir).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let image = media_dir.join("generated.jpg");
        let report = workspace.join("report.txt");
        let secret = root.path().join("secret.txt");
        std::fs::write(&image, b"image").unwrap();
        std::fs::write(&report, b"report").unwrap();
        std::fs::write(&secret, b"secret").unwrap();

        let mut session = Session::new("local-files").with_storage_root(root.path());
        session.cwd = workspace.to_string_lossy().to_string();
        session.append_prepared_user_message_with_id(
            "user-local-files".to_string(),
            vec![ContentBlock::text("生成文件")],
        );
        session.append_message(
            MessageRole::Assistant,
            format!(
                "## 已完成\n\n![生成图片]({})\n\n[报告]({})\n\n[机密]({})",
                image.display(),
                report.display(),
                secret.display()
            ),
        );

        let outgoing = assistant_outgoing_after_user(&session, "user-local-files");
        let image_path = std::fs::canonicalize(image).unwrap();
        match outgoing.content {
            MessageContent::Image { url, caption } => {
                assert_eq!(Path::new(&url), image_path);
                let caption = caption.unwrap();
                assert!(caption.contains("已完成"));
                assert!(caption.contains("报告"));
                assert!(caption.contains("机密"));
                assert!(!caption.contains(root.path().to_string_lossy().as_ref()));
            }
            other => panic!("expected image outgoing, got {other:?}"),
        }
        assert_eq!(outgoing.attachments.len(), 1);
        let report_path = std::fs::canonicalize(report).unwrap();
        assert!(matches!(
            &outgoing.attachments[0],
            MessageContent::File { url, name }
                if Path::new(url) == report_path && name == "报告"
        ));
    }

    #[test]
    fn outgoing_keeps_text_before_a_workspace_file() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let report = workspace.join("report.pdf");
        std::fs::write(&report, b"report").unwrap();

        let mut session = Session::new("workspace-file").with_storage_root(root.path());
        session.cwd = workspace.to_string_lossy().to_string();
        session.append_prepared_user_message_with_id(
            "user-workspace-file".to_string(),
            vec![ContentBlock::text("生成报告")],
        );
        session.append_message(
            MessageRole::Assistant,
            format!("报告已经生成：\n\n[下载报告]({})", report.display()),
        );

        let outgoing = assistant_outgoing_after_user(&session, "user-workspace-file");
        assert!(matches!(
            outgoing.content,
            MessageContent::Text(ref text)
                if text.contains("报告已经生成") && text.contains("下载报告")
        ));
        assert!(matches!(
            &outgoing.attachments[0],
            MessageContent::File { url, name }
                if Path::new(url) == std::fs::canonicalize(report).unwrap()
                    && name == "下载报告"
        ));
    }
}
