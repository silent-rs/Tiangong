//! 中枢 Core 生命周期（R1 从 app.rs 拆出）：构建（ensure_central_core，直接持有、
//! 不经 CoreManager——内置唯一中枢插件）、消息投递、turn 取消、存活判定与
//! 关闭汇合；模式切换观测锚点（Core 创建/复用打点）也在此。

use std::sync::Arc;

use tiangong_core::agent_input::AgentInputKind;
use tiangong_core::config::core::CoreConfigProvider;

use crate::host::dispatch::central_dispatch_sender;

impl crate::CentralHostState {
    /// 确保中枢 Core 存在：已存在则刷新配置复用，否则直接构造（挂中枢插件）。
    /// 返回是否新建（调用方据此启动流事件消费者）。
    pub async fn ensure_central_core<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        stream_tx: std::sync::mpsc::Sender<tiangong_types::StreamEvent>,
    ) -> Result<bool, String> {
        let (app_config, agent_config, workspace_dir) = self
            .with_state_read(|state| {
                Ok((
                    state.config.clone(),
                    state.agent_config.clone(),
                    state.workspace_dir.clone(),
                ))
            })
            .await
            .unwrap_or_default();
        let mut session_config = app_config.to_core_config();
        let existing_session = self
            .core_manager
            .load_session(crate::CENTRAL_SESSION_ID)
            .ok();
        if let Some(session) = &existing_session {
            session_config.trust_mode = session.trust_mode;
            session_config.reasoning_effort = session
                .reasoning_effort
                .unwrap_or(agent_config.reasoning_effort);
        }
        // 复用：刷新会话级运行配置（同 CoreManager ensure 命中分支语义）。
        {
            let guard = self.central_core.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(core) = guard.as_ref() {
                let _ = core.replace_config(session_config.clone());
                core.set_trust_mode(session_config.trust_mode);
                core.set_reasoning_effort(session_config.reasoning_effort);
                // R0 观测：复用已有中枢 Core（进入中枢模式，无重建）。
                tracing::info!(
                    session_id = crate::CENTRAL_SESSION_ID,
                    "中枢 Core 复用（进入中枢模式，后台工作不受影响）"
                );
                return Ok(false);
            }
        }
        // 新建：模型 = 中枢 session.model_ref ?? 设置的主对话模型（§15.12）。
        // 模型故障兜底（§15.12「中枢不掉线」）：会话指定模型失效（被删除/
        // 凭据缺失/能力不符）时不让中枢构建失败——回退默认 chat 路由；两者
        // 都不可用才报错。回退事实写入日志，首个可用机会注入中枢对话。
        let session_model_ref = existing_session.and_then(|session| session.model_ref);
        let model_endpoint = match self
            .core_manager
            .resolve_turn_model(session_model_ref.as_deref())
        {
            Ok(endpoint) => endpoint,
            Err(reason) => {
                tracing::warn!(
                    %reason,
                    "中枢会话模型不可用，回退默认对话模型（§15.12 中枢不掉线）"
                );
                let fallback =
                    self.core_manager
                        .resolve_turn_model(None)
                        .map_err(|fallback_error| {
                            format!(
                                "中枢模型不可用（{reason}），默认模型回退也失败：{fallback_error}"
                            )
                        })?;
                // 回退事实注入中枢回报队列（抖动控制：每进程只通知一次——反复
                // ensure/命中复用路径不重复推送，阶段 2 要求）。
                if !self
                    .central_model_fallback_notified
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    let model_note = format!(
                        "[系统中枢] 配置的中枢模型不可用（{reason}），已自动回退默认模型；如需更换请调整设置。"
                    );
                    self.central_work_reports
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(model_note);
                }
                fallback
            }
        };
        let core = tiangong_core::core::TiangongCore::builder()
            .session_id(crate::CENTRAL_SESSION_ID.to_string())
            .config(CoreConfigProvider::new(session_config.clone()))
            .trust_mode(session_config.trust_mode)
            .storage_root(self.core_manager.storage_root().to_path_buf())
            .workspace_dir(workspace_dir.clone())
            .stream_tx(stream_tx)
            // 征询功能暂缓（用户裁决 2026-11-11）：只挂中枢插件，不加
            // RuntimeCorePlugin（request_user 通道随之移除）。中枢需要用户
            // 决策时在对话文本中直接询问用户。
            .plugins(vec![Arc::new(crate::CentralPlugin::new(
                workspace_dir,
                self.core_manager.clone(),
                central_dispatch_sender(app),
                std::sync::Arc::clone(&self.central_work_reports),
            ))])
            .model_endpoint(model_endpoint)
            .build();
        let mut guard = self.central_core.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            // 并发双检：另一路径已创建，丢弃本次实例（其 stream_tx 消费者由先建者负责）。
            return Ok(false);
        }
        *guard = Some(core);
        // R0 观测：中枢 Core 创建 = 模式切换的宿主锚点（进入中枢）。
        tracing::info!(
            session_id = crate::CENTRAL_SESSION_ID,
            "中枢 Core 已创建（进入中枢模式）"
        );
        Ok(true)
    }

    /// 向中枢 Core 投递用户消息（prepared blocks）。
    pub fn deliver_central_message(
        &self,
        message_id: String,
        prepared: Vec<tiangong_types::ContentBlock>,
        render: Option<tiangong_types::MessageRender>,
    ) -> Result<(), String> {
        use tiangong_core::agent_input::AgentInput;
        let core = self
            .central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "中枢 Core 不存在（先 ensure_central_core）".to_string())?;
        core.deliver(AgentInputKind::prepared_with_id(message_id, prepared).with_render(render))
            .map_err(|error| format!("中枢消息投递失败：{error}"))
    }

    /// 取消中枢当前 turn。返回是否送达。
    pub fn cancel_central_turn(&self) -> bool {
        use tiangong_core::agent_input::{AgentInput, CommandInput};
        self.central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|core| {
                core.deliver(AgentInputKind::Command(CommandInput::Cancel))
                    .is_ok()
            })
    }

    /// 中枢 Core 是否存活（流事件消费者的存活判定用）。
    pub fn central_core_alive(&self) -> bool {
        self.central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 关闭并等待中枢 Core 写盘结束（发送失败回滚 / 进程退出）。
    pub async fn shutdown_join_central_core(&self) {
        let core = self
            .central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(core) = core {
            let _ = tokio::task::spawn_blocking(|| core.shutdown_join()).await;
        }
    }
}
