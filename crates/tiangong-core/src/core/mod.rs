//! TiangongCore：天工智能体核心
//!
//! 输入经 [`AgentInput::deliver`] 进入：空闲起轮（Core 直接构建 TurnContext 并
//! spawn turn task）、运行中投通道（命令进入活跃 turn 的命令通道，由 turn 内
//! 部仲裁：引导/取消）。每个 turn 从最新磁盘 Session 构建上下文（ALR-201/204）。
//! CLI / GUI / Server / Connector 统一通过 TiangongCore 运行。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use typed_builder::TypedBuilder;

use crate::config::core::{CoreConfig, CoreConfigProvider};
use crate::react::turn::run_turn;
use crate::session::Session;
use crate::turn_context::TurnContext;
use tiangong_llm::ModelEndpoint;
use tiangong_llm::SingleProviderClient;
use tiangong_types::StreamEvent;

pub mod command;
pub(crate) use command::Command;
#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod integration_tests;
pub mod plugin;
#[cfg(test)]
pub(crate) mod test_support;
pub use plugin::Plugin;
pub mod error;
pub mod storage_location;

pub use error::CoreError;
pub use storage_location::CoreStorageLocation;

/// 上下文管理留痕类别：所有压缩/切换模型类 Notice 的 [`Message::notice`]
/// category 参数（前端按 `[类别] ` 前缀剥离后展示）。
///
/// 留痕构造统一走 `Message::notice`（`[类别] 内容` 单点形态）；Notice
/// 角色本身被排除出模型上下文与压缩范围，文案不影响 KV cache 前缀。
pub const CONTEXT_NOTICE_CATEGORY: &str = "上下文管理";

/// 用户手动整理（`/compress` 指令与宿主手动入口）的留痕内容（不含
/// 类别前缀，出口由 [`Message::notice`] 统一补全）。
///
/// 编排类整理（模型切换前、插件交接）的原因**不属于 core**——由各编排方
/// 以内容形式注入（见 `compact_context` 的 `notice` 参数），core 只提供
/// 留痕通道，不感知上游业务概念。
pub const MANUAL_COMPACT_NOTICE: &str = "已按你的要求整理上下文";

/// 判断标题是否仍是默认值（"新对话"/"会话 X"）。
///
/// 用于标题自动生成（现由 core-manager 经 [`crate::TiangongCore::set_title`]
/// 驱动）预检与写回校验，避免覆盖用户已手动改过的标题。
pub fn is_default_title(title: &str) -> bool {
    title == "新对话" || title.starts_with("会话 ")
}

/// 天工智能体核心
#[derive(TypedBuilder, Clone)]
#[builder(
    builder_method(vis = "pub"),
    builder_type(vis = "pub"),
    build_method(vis = "pub")
)]
pub struct TiangongCore {
    /// 会话 ID
    #[builder(setter(into))]
    session_id: String,
    /// 当前 Core 独立的配置提供者。
    config: CoreConfigProvider,
    /// 会话信任模式。
    #[builder(setter(transform = |mode: crate::permission::TrustMode| std::sync::Arc::new(std::sync::Mutex::new(mode))))]
    trust_mode: std::sync::Arc<std::sync::Mutex<crate::permission::TrustMode>>,
    /// 存储根目录(turn task 从此加载/保存 session)。
    #[builder(setter(into))]
    storage_root: std::path::PathBuf,
    /// 新对话创建后固定使用的工作目录。
    #[builder(setter(into))]
    workspace_dir: String,
    /// 事件输出通道(会话级,跨 turn;clone 给每个 turn task 的 forwarder)。
    stream_tx: Sender<StreamEvent>,
    /// 进程内插件(会话级,跨 turn)。
    #[builder(setter(transform = |plugins: Vec<Arc<dyn Plugin>>| Arc::new(std::sync::Mutex::new(plugins))))]
    plugins: Arc<std::sync::Mutex<Vec<Arc<dyn Plugin>>>>,
    /// on_session_ready 是否已对本 Core 实例执行过（会话级一次性状态，不落盘）。
    /// 首次 turn 置为 true，此后复用同一 Core 的轮次只触发 on_turn_started。
    #[builder(default)]
    session_ready: Arc<AtomicBool>,
    /// 当前**实际已应用**的模型端点（运行时唯一真相）。
    ///
    /// 与 `Session.model_ref`（用户选择策略，None 表示跟随默认）不同，这里
    /// 是 Core 真正在用的模型；身份由端点自身派生（见 `ModelEndpoint::model_key`），
    /// 宿主据此判断是否需要切换。构造时由宿主按注册表解析后传入；之后只由
    /// [`Self::switch_model`] 在切换成功时更新——切换失败保持旧值，下一次
    /// 发送仍会识别出差异并重新走压缩+切换。
    #[builder(setter(transform = |endpoint: ModelEndpoint| Arc::new(std::sync::Mutex::new(endpoint))))]
    model_endpoint: Arc<std::sync::Mutex<ModelEndpoint>>,
    /// 测试专用的模型客户端；发布构建不存在该字段及 builder 配置入口。
    #[cfg(test)]
    #[builder(default, setter(strip_option))]
    test_client: Option<SingleProviderClient>,
}

impl TiangongCore {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 替换该 Core 的独立配置。
    ///
    /// 运行配置在后续 turn 生效；工具及提示声明仅在压缩成功或清理上下文时重新采集。
    pub fn replace_config(&self, config: CoreConfig) -> Result<(), CoreError> {
        let changed = serde_json::to_value(&*self.config.snapshot()).ok()
            != serde_json::to_value(&config).ok();
        if changed && self.session_ready.load(Ordering::Acquire) {
            for plugin in self.plugin_instances() {
                plugin.on_config_updated(&config);
            }
        }
        self.config.replace(config);
        Ok(())
    }

    /// 是否有活跃的 turn task（有则视为 Running）。
    pub fn is_stopped(&self) -> bool {
        !crate::shared_runtime::is_running(&self.session_id)
    }

    /// 是否正在执行 turn（对外 `Running`）。
    pub fn is_busy(&self) -> bool {
        crate::shared_runtime::is_running(&self.session_id)
    }

    /// 当前实际已应用模型的身份标识（运行时唯一真相）。
    ///
    /// 由端点的 `base_url + model + protocol` 派生，宿主据此判断本轮是否
    /// 需要切换模型——不要用 `Session.model_ref` 代替：后者是用户的选择
    /// 策略（None 表示跟随默认），不是实际状态。
    pub fn current_model_key(&self) -> String {
        self.model_endpoint
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .model_key()
    }

    /// 当前实际执行端点。
    pub fn current_endpoint(&self) -> ModelEndpoint {
        self.model_endpoint
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 整理上下文：对既有历史执行一次手动压缩，并等待其进入终态。
    ///
    /// 供宿主在模型切换前调用——用**旧模型**把历史折叠成摘要，新模型接手
    /// 的是整理后的上下文。复用 `CommandInput::CompressContext` 的同一份压缩
    /// 实现，区别只在于本方法等待压缩任务进入终态（成功 / 失败 / 取消）后
    /// 才返回，便于宿主据此决定下一步。
    ///
    /// 返回 `Ok(())` 表示压缩已成功应用或无可压缩历史；`Err` 表示压缩未能
    /// 完成（模型调用失败、被取消、worker 退出等）。**返回 Err 时压缩任务
    /// 同样已进入终态**，调用方可以安全地继续后续操作。
    pub async fn compact_context(&self, notice: &str) -> Result<(), CoreError> {
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        self.spawn_context_compression(Some(completion_tx), notice.to_string())?;
        // 任务在任何终态下都会发送一次通知；发送端被丢弃（worker 异常退出）
        // 时 recv 返回 Err，同样结束等待，不会悬挂。
        match completion_rx.await {
            Ok(outcome) => outcome,
            Err(_) => Err(CoreError::ContextCompactionFailed(
                "压缩任务未给出结果（worker 已退出）".to_string(),
            )),
        }
    }

    /// 切换当前模型（原子）：运行中不切换（忙即拒绝）。
    ///
    /// 只有切换成功才更新当前模型；失败保持旧值，宿主下一次
    /// 发送仍会识别出差异并重新编排。切换后的请求使用新模型，已发出的
    /// 请求不受影响。
    pub fn switch_model(&self, target: ModelEndpoint) -> Result<(), CoreError> {
        if self.is_busy() {
            return Err(CoreError::Busy);
        }
        if !target.is_usable() {
            return Err(CoreError::ModelSwitchFailed(format!(
                "目标模型 {} 的端点配置不完整",
                target.model_key()
            )));
        }
        let _ = self.stream_tx.send(StreamEvent::ModelSwitchStarted {
            model_name: target.model.clone(),
        });
        let model_name = target.model.clone();
        {
            let mut current = self
                .model_endpoint
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *current = target;
        }
        // 切换留痕：写入会话消息列表，让用户在历史里查证「这一段之后换了
        // 模型」。仅发流事件不够——那只驱动顶部状态栏文案，刷新或切换会话
        // 后就消失，而模型变化是理解后续回复的重要上下文。
        //
        // 用 Notice：角色本身保证排除出模型上下文与压缩范围（见
        // Session::context 与 is_compressible），不污染 prompt。
        // 此处 is_busy 已为 false（函数入口已校验），无活跃 turn 会并发写
        // 会话文件；落盘失败只告警，不因留痕失败而让切换回退。
        match self.load_session() {
            Ok(mut session) => {
                let record = crate::session::Message::notice(
                    CONTEXT_NOTICE_CATEGORY,
                    format!("已切换模型：{model_name}"),
                );
                let snapshot = record.clone();
                session.messages.push(record);
                if let Err(error) = session.try_persist_to_disk() {
                    tracing::warn!(%error, session_id = %self.session_id, "模型切换留痕落盘失败");
                } else {
                    let _ = self.stream_tx.send(StreamEvent::SessionMessageUpsert {
                        message: snapshot,
                        deferred_tool_injections: None,
                    });
                }
            }
            Err(error) => {
                tracing::warn!(%error, session_id = %self.session_id, "模型切换留痕读取会话失败");
            }
        }
        let _ = self
            .stream_tx
            .send(StreamEvent::ModelSwitched { model_name });
        Ok(())
    }

    /// 设置会话信任模式。
    ///
    /// 更新后对当前会话的权限门与插件实时生效；活跃 turn 通过命令通道同步。
    pub fn set_trust_mode(&self, mode: crate::permission::TrustMode) {
        // Core 和插件立即使用新值；Session 字段由活跃 turn 或下一轮统一写入。
        *self.trust_mode.lock().unwrap_or_else(|p| p.into_inner()) = mode;
        for plugin in self.plugin_instances() {
            plugin.set_trust_mode(mode);
        }
        if self.is_busy() {
            let _ =
                crate::shared_runtime::send_command(&self.session_id, Command::SetTrustMode(mode));
        }
    }

    /// 设置会话思考强度。
    ///
    /// 已经发出的模型请求不变；活跃 turn 会在下一次构建模型请求时使用新值。
    pub fn set_reasoning_effort(&self, effort: tiangong_llm::ReasoningEffort) {
        self.config
            .update(|config| config.reasoning_effort = effort);
        if self.is_busy() {
            let _ = crate::shared_runtime::send_command(
                &self.session_id,
                Command::SetReasoningEffort(effort),
            );
        }
    }

    /// 更新会话标题。
    ///
    /// - turn 进行中：投 `Command::SetTitle`，由 turn 在命令分支写入 `ctx.session`，
    ///   turn 结束 run_turn 统一落盘（避免与 turn 对 session 的读写竞争）。
    /// - Core 空闲：Core 是 session 权威持有者且无并发 turn，直接 load+改+persist。
    ///
    /// `only_if_default=true` 时仅当当前标题仍是默认值才覆盖（标题自动生成
    /// 由 core-manager 在投递用户消息时驱动，经此方法写回）；用户手动编辑传 false。
    pub fn set_title(&self, title: String, only_if_default: bool) -> Result<(), CoreError> {
        let title = title.trim().to_string();
        if title.is_empty() {
            return Err(CoreError::WorkerStopped);
        }
        if self.is_busy() {
            return if crate::shared_runtime::send_command(
                &self.session_id,
                Command::SetTitle {
                    title,
                    only_if_default,
                },
            ) {
                Ok(())
            } else {
                Err(CoreError::WorkerStopped)
            };
        }
        let mut session = self.load_session()?;
        if only_if_default && !is_default_title(&session.title) {
            return Ok(());
        }
        session.title = title.clone();
        session.updated_at = tiangong_types::now_text();
        session
            .try_persist_to_disk()
            .map_err(|_| CoreError::WorkerStopped)?;
        // 空闲时也发 TitleChanged，让前端统一经事件更新标题（手动改空闲会话同样通知）。
        let _ = self.stream_tx.send(StreamEvent::TitleChanged { title });
        Ok(())
    }

    /// 设置会话级对话模型引用（models 注册表 key；None 表示跟随路由默认）。
    ///
    /// 与 set_title 同构的会话字段设置器：只在空闲期写盘，忙时返回 Busy
    /// 不投递命令——运行中的 turn 不得中途换模型。同值直接返回，避免为
    /// 无变化的写入承担整份 session 的读改写窗口。
    pub fn set_model_ref(&self, model_ref: Option<String>) -> Result<(), CoreError> {
        if self.is_busy() {
            return Err(CoreError::Busy);
        }
        let mut session = self.load_session()?;
        if session.model_ref == model_ref {
            return Ok(());
        }
        session.model_ref = model_ref;
        session.updated_at = tiangong_types::now_text();
        session
            .try_persist_to_disk()
            .map_err(|_| CoreError::WorkerStopped)?;
        Ok(())
    }

    fn load_session(&self) -> Result<Session, CoreError> {
        match Session::load_from_storage(&self.storage_root, &self.session_id) {
            Ok(session) => Ok(session),
            Err(error)
                if !self
                    .storage_root
                    .join("sessions")
                    .join(format!("{}.json", self.session_id))
                    .exists() =>
            {
                let config = self.config.snapshot();
                let mut session = Session::new("新对话");
                session.id = self.session_id.clone();
                session.trust_mode = *self
                    .trust_mode
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                session.reasoning_effort = Some(config.reasoning_effort);
                session.cwd = self.workspace_dir.clone();
                session.cwd_mode = crate::session::SessionCwdMode::Custom;
                session.bind_storage_root(self.storage_root.clone());
                session.try_persist_to_disk().map_err(|persist_error| {
                    tracing::warn!(
                        error = %persist_error,
                        session_id = %self.session_id,
                        "创建 Session 失败"
                    );
                    CoreError::WorkerStopped
                })?;
                tracing::debug!(
                    session_id = %self.session_id,
                    load_error = %error,
                    "首次加载创建 Session"
                );
                Ok(session)
            }
            Err(error) => {
                tracing::warn!(%error, session_id = %self.session_id, "加载 Session 失败");
                Err(CoreError::WorkerStopped)
            }
        }
    }

    /// 加载最新 Session，并根据当前配置构建本轮执行上下文（ALR-204）。
    fn build_turn_context(&self) -> Result<TurnContext, CoreError> {
        let mut session = self.load_session()?;
        let trust_mode = *self.trust_mode.lock().unwrap_or_else(|p| p.into_inner());
        session.trust_mode = trust_mode;

        let config = self.config.snapshot();
        // 执行端点 = 当前生效端点（switch 过为会话自持值，否则动态跟随
        // 路由默认）；模型注册表与 model_ref 的解析都在宿主，core 只经
        // [`Self::switch_endpoint`] 接收结果。
        let endpoint = self.current_endpoint();
        let stream_tx = self.stream_tx.clone();
        let retry_tx = stream_tx.clone();
        let on_retry: tiangong_llm::OnRetryCallback =
            Arc::new(move |attempt, max_attempts, _delay_ms, error_text| {
                let _ = retry_tx.send(StreamEvent::Retry {
                    message: error_text.to_string(),
                    attempt,
                    max_attempts,
                });
            });
        #[cfg(test)]
        let client = if let Some(test_client) = self.test_client.clone() {
            test_client.with_on_retry(on_retry.clone())
        } else {
            SingleProviderClient::new(endpoint.clone()).with_on_retry(on_retry.clone())
        };
        #[cfg(not(test))]
        let client = SingleProviderClient::new(endpoint.clone()).with_on_retry(on_retry.clone());
        let plugins = self
            .plugins
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let prepared_plugins =
            crate::core::plugin::prepare_plugins(&plugins, &config, trust_mode, &session);

        Ok(TurnContext::builder()
            .client(client)
            .session(session)
            .stream_tx(stream_tx)
            .plugins(prepared_plugins.plugins)
            // 上下文窗口跟随当前实际模型：会话切换模型后窗口必须同步变化，
            // 否则换到小窗口模型仍按旧窗口判断自动压缩，请求会被 Provider
            // 拒绝。端点未声明窗口时回落到配置值（路由默认模型推导而来）。
            .context_limit(
                endpoint
                    .context_window
                    .unwrap_or_else(crate::config::core::default_context_limit),
            )
            .agent_config(crate::config::agent::AgentConfig {
                trust_mode,
                default_trust_mode: config.default_trust_mode,
                custom_system_prompt: config.custom_system_prompt.clone(),
                reasoning_effort: config.reasoning_effort,
            })
            .trust_mode(trust_mode)
            .observer(crate::observe::Observer::new(self.storage_root.clone()))
            .tool_overrides(prepared_plugins.tool_overrides)
            .tools(prepared_plugins.tools)
            .build())
    }

    fn plugin_instances(&self) -> Vec<Arc<dyn Plugin>> {
        self.plugins
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 空闲起轮：构建上下文 → 校验并保存用户消息（成功才确认）→ spawn turn task。
    ///
    /// on_session_ready 仅在本 Core 实例的首次 turn 执行（会话级一次性初始化），
    /// 与系统提示重建、落盘一起在任务启动闭包内同步完成后再进入 Agent Loop。
    fn start_user_turn(
        &self,
        message_id: String,
        content: Vec<tiangong_types::ContentBlock>,
    ) -> Result<(), CoreError> {
        let mut ctx = self.build_turn_context()?;
        // 保存成功才确认（ALR-202）；失败向调用方返回明确错误，不虚报成功。
        ctx.session
            .try_append_prepared_user_message_with_id(message_id.clone(), content.clone())
            .map_err(|error| {
                tracing::warn!(%error, session_id = %self.session_id, "用户消息保存失败");
                CoreError::WorkerStopped
            })?;
        let _ = ctx.stream_tx.send(StreamEvent::UserMessage {
            message_id,
            content: tiangong_types::content_blocks_text(&content),
            content_blocks: tiangong_types::stable_content_blocks(&content),
            media: Vec::new(),
        });
        let session_ready = self.session_ready.clone();
        let core = self.clone();
        crate::shared_runtime::spawn_turn(ctx, move |mut ctx, cmd_rx| {
            if session_ready
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                for plugin in &ctx.plugins {
                    plugin.on_session_ready(&mut ctx.session);
                }
            }
            // 每轮重拼 system prompt：内容一致时 rebuild 内部保留原消息
            //（前缀与消息 id 均稳定），变化才会改写。
            let prompt_sections = ctx
                .plugins
                .iter()
                .flat_map(|plugin| plugin.prompt_sections())
                .collect();
            ctx.session.rebuild_system_prompt(
                &crate::prompt::SystemPromptConfig::from_plugin_sections(prompt_sections),
            );
            if let Err(error) = ctx.session.try_persist_to_disk() {
                tracing::warn!(%error, "持久化本轮系统提示失败");
            }
            let mut cmd_rx = cmd_rx;
            Ok(async move {
                run_turn(ctx, &mut cmd_rx).await;
                // 收尾排空：turn 已定终态，通道中未被消费的用户消息不丢弃。
                // 与引导语义一致——连发消息以最后一条为准：其余消息保存进会话
                // 作为历史（不丢），最后一条直接续轮（start_user_turn 自带保存
                // 与确认，不再重复落盘）。关闭（Cancel）路径同样经此保存。
                let mut pending_user_messages = Vec::new();
                while let Ok(command) = cmd_rx.try_recv() {
                    if let Command::InjectUserMessage {
                        message_id,
                        content,
                    } = command
                    {
                        pending_user_messages.push((message_id, content));
                    }
                }
                if let Some((message_id, content)) = pending_user_messages.pop() {
                    if !pending_user_messages.is_empty() {
                        core.save_pending_user_messages(&pending_user_messages);
                    }
                    crate::shared_runtime::release_agent(&core.session_id);
                    if let Err(error) = core.start_user_turn(message_id, content) {
                        tracing::warn!(%error, session_id = %core.session_id, "接续排队消息起轮失败");
                    }
                }
            })
        })
    }

    /// 把未被消费的用户消息保存进会话并落盘（不丢）。
    fn save_pending_user_messages(&self, messages: &[(String, Vec<tiangong_types::ContentBlock>)]) {
        let Ok(mut session) = self.load_session() else {
            tracing::warn!(session_id = %self.session_id, "排队消息落盘前加载会话失败");
            return;
        };
        for (message_id, content) in messages {
            if let Err(error) = session
                .try_append_prepared_user_message_with_id(message_id.clone(), content.clone())
            {
                tracing::warn!(%error, message_id, "排队消息保存失败");
            }
        }
        if let Err(error) = session.try_persist_to_disk() {
            tracing::warn!(%error, session_id = %self.session_id, "排队消息落盘失败");
        }
    }

    /// 活动期输入守门：仅当有活跃 turn 时投递其命令通道；空闲时明确拒绝。
    fn deliver_to_turn(
        &self,
        command: Command,
        message_type: &'static str,
    ) -> Result<(), CoreError> {
        if !self.is_busy() {
            // 空闲工具注入不丢弃：写入会话延迟队列，下一轮模型请求自动携带。
            if let Command::InjectTool { tool_name, payload } = command {
                let mut session = self.load_session()?;
                session.defer_tool_injection(tool_name, payload);
                session.try_persist_to_disk().map_err(|error| {
                    tracing::warn!(%error, session_id = %self.session_id, "空闲注入落盘失败");
                    CoreError::WorkerStopped
                })?;
                return Ok(());
            }
            tracing::warn!(session_id = %self.session_id, message_type, "Agent 空闲，拒绝活动期输入");
            return Err(CoreError::WorkerStopped);
        }
        if crate::shared_runtime::send_command(&self.session_id, command) {
            Ok(())
        } else {
            Err(CoreError::WorkerStopped)
        }
    }

    /// 空闲期手动上下文压缩：spawn 独立压缩任务占用本会话的任务槽。
    ///
    /// 压缩状态下只接受引导消息与取消类命令：用户消息取消压缩并直接
    /// 起轮（压缩可随时重新发起）；其余信号（插件可用性广播等）不接受，
    /// 不打断压缩。运行中调用返回 `Busy`。
    ///
    /// `completion_tx` 在压缩进入任一终态时收到一次结果，供等待方结束等待。
    /// 任务结束路径上必然发送（发送端随任务一起释放，接收端不会悬挂）。
    fn spawn_context_compression(
        &self,
        completion_tx: Option<tokio::sync::oneshot::Sender<Result<(), CoreError>>>,
        notice: String,
    ) -> Result<(), CoreError> {
        if self.is_busy() {
            return Err(CoreError::Busy);
        }
        let ctx = self.build_turn_context()?;
        let core = self.clone();
        let session_id = self.session_id.clone();
        crate::shared_runtime::spawn_turn(ctx, move |mut ctx, mut cmd_rx| {
            // 防御性重建 system prompt：压缩请求需要它承载旧摘要（裸 session 直接
            // 手动压缩时可能缺失）。内容一致时保留原消息。
            let prompt_sections = ctx
                .plugins
                .iter()
                .flat_map(|plugin| plugin.prompt_sections())
                .collect();
            ctx.session.rebuild_system_prompt(
                &crate::prompt::SystemPromptConfig::from_plugin_sections(prompt_sections),
            );
            Ok(async move {
                use crate::react::compression::ManualCompressionOutcome as Outcome;
                let outcome = crate::react::compression::run_manual_context_compression(
                    ctx,
                    &mut cmd_rx,
                    notice,
                )
                .await;
                let interrupted_by_message = matches!(
                    outcome,
                    Outcome::Interrupted(crate::react::compression::CompressionInterrupt::Command(
                        Command::InjectUserMessage { .. }
                    ))
                );
                // 等待方被唤醒后往往立刻切换模型，而 spawn_turn 的 wrapper 要等
                // 本 future 返回才注销任务槽——必须先腾出槽位再通知，否则等待方
                // 读到的仍是 Busy，切换会被拒绝。引导消息分支同样需要空槽位起新轮。
                if completion_tx.is_some() || interrupted_by_message {
                    crate::shared_runtime::release_agent(&session_id);
                }
                if let Some(tx) = completion_tx {
                    let result = match &outcome {
                        Outcome::Succeeded | Outcome::Noop => Ok(()),
                        Outcome::Failed(reason) => {
                            Err(CoreError::ContextCompactionFailed(reason.clone()))
                        }
                        Outcome::Interrupted(_) => Err(CoreError::ContextCompactionFailed(
                            "整理上下文被中断".to_string(),
                        )),
                    };
                    let _ = tx.send(result);
                }
                if let Outcome::Interrupted(
                    crate::react::compression::CompressionInterrupt::Command(
                        Command::InjectUserMessage {
                            message_id,
                            content,
                        },
                    ),
                ) = outcome
                {
                    // 压缩已被引导消息终止且未应用任何结果；槽位已腾出，
                    // 直接起新轮。取消类终止无需接续。
                    if let Err(error) = core.start_user_turn(message_id, content) {
                        tracing::warn!(%error, session_id = %session_id, "压缩中断后起新轮失败");
                    }
                }
            })
        })
    }

    /// 用户手动触发的上下文压缩（`CommandInput::CompressContext`）。
    fn compress_context(&self) -> Result<(), CoreError> {
        self.spawn_context_compression(None, MANUAL_COMPACT_NOTICE.to_string())
    }

    /// 空闲期清理上下文（同步执行，不涉及模型请求）。
    ///
    /// 只推进摘要边界并清空用量；system prompt 由下一轮 turn 启动时重拼。
    fn reset_context(&self) -> Result<(), CoreError> {
        if self.is_busy() {
            return Err(CoreError::Busy);
        }
        let mut session = self.load_session()?;
        let total = session.messages.len();
        session.summary_up_to = total;
        crate::context::compressor::mark_compact_boundary(&mut session.messages, total);
        session.context_summary = None;
        session.current_tokens = 0;
        session.active_agent_current_tokens = 0;
        session.agent_current_tokens.clear();
        session.try_persist_to_disk().map_err(|error| {
            tracing::warn!(%error, session_id = %self.session_id, "清空上下文落盘失败");
            CoreError::WorkerStopped
        })?;
        crate::react::compression::notify_cleared(&self.stream_tx, &session);
        Ok(())
    }

    /// 关闭并获取最终 session。
    ///
    /// worker panic 时返回 [`CoreError::WorkerPanicked`]，不再静默兜底为
    /// `Session::new("recovered")`——避免丢失原会话数据后调用方误判成功。
    /// 从磁盘加载最终 session。
    pub fn into_session(self) -> Result<Session, CoreError> {
        self.finalize_session()
    }

    /// 关闭 Core,不取回 session(session 已在磁盘上)。
    pub fn shutdown_join(self) -> Result<(), CoreError> {
        self.finalize_session().map(|_| ())
    }

    fn finalize_session(&self) -> Result<Session, CoreError> {
        // 取消活跃 turn 并等待任务收敛，再投递插件收尾通知、返回最终 session。
        crate::shared_runtime::cancel_and_join(&self.session_id)?;
        let session = self.load_session()?;
        self.finalize_plugins(&session);
        Ok(session)
    }

    /// 投递全部 `on_session_ended` 通知（worker 退出前的 finalize 钩子）。
    ///
    /// 通知型钩子：后台线程投递、不等待完成——关闭会话/退出应用不被任何插件
    /// 无限阻塞（issue #404）。钩子收到只读快照，收尾成败与产出由插件自行负责。
    fn finalize_plugins(&self, session: &Session) {
        crate::core::plugin::notify_session_ended(&self.plugin_instances(), session);
    }
}

impl crate::agent_input::AgentInput for TiangongCore {
    fn deliver(&self, input: crate::agent_input::AgentInputKind) -> Result<(), CoreError> {
        use crate::agent_input::{AgentInputKind, CommandInput, MessageInput};

        match input {
            // 空闲起轮、运行中投通道：运行中的用户消息由活跃 turn 作为引导处理
            // （中断当前活动 → 保存新消息 → 从新意图重启）。
            AgentInputKind::Message(MessageInput::UserMessage {
                prepared,
                message_id,
                // 模型选择由 CoreManager 在投递前消费（解析并按需切换），
                // Core 只负责执行；到这里已无需再看。
                model_ref: _,
            }) => {
                let message_id = message_id.unwrap_or_else(scru128::new_string);
                if self.is_busy() {
                    return if crate::shared_runtime::send_command(
                        &self.session_id,
                        Command::InjectUserMessage {
                            message_id,
                            content: prepared,
                        },
                    ) {
                        Ok(())
                    } else {
                        Err(CoreError::WorkerStopped)
                    };
                }
                self.start_user_turn(message_id, prepared)
            }
            AgentInputKind::Tool(tool) => self.deliver_to_turn(
                Command::InjectTool {
                    tool_name: tool.tool_name().to_string(),
                    payload: tool.render(),
                },
                "InjectTool",
            ),
            AgentInputKind::Command(cmd) => match cmd {
                CommandInput::Cancel => {
                    if crate::shared_runtime::send_command(&self.session_id, Command::Cancel) {
                        Ok(())
                    } else {
                        Err(CoreError::WorkerStopped)
                    }
                }
                CommandInput::SetTrustMode(trust_mode) => {
                    self.set_trust_mode(trust_mode);
                    Ok(())
                }
                CommandInput::CompressContext => self.compress_context(),
                CommandInput::ResetContext => self.reset_context(),
            },
        }
    }
}

impl Drop for TiangongCore {
    fn drop(&mut self) {
        // 空实现：turn task 在共享 runtime 上自然跑完并落盘（含 agent-team 子
        // Agent 被父轮释放的场景）。显式取消与关闭由 Cancel 命令、shutdown_join
        // / CoreManager 承担；不在此发 Cancel，避免任务闭包持有的 Core 克隆在
        // 释放时误杀自己启动的后继任务。
    }
}

#[cfg(test)]
mod model_endpoint_tests {
    use super::*;
    use crate::config::core::{CoreConfig, CoreConfigProvider};

    fn endpoint(base_url: &str, model: &str) -> ModelEndpoint {
        ModelEndpoint {
            base_url: base_url.to_string(),
            api_key: "sk-test".to_string(),
            model: model.to_string(),
            ..Default::default()
        }
    }

    fn target(model: &str) -> ModelEndpoint {
        endpoint("https://api.example.com/v1", model)
    }

    fn test_core(root: &std::path::Path, session_id: &str) -> TiangongCore {
        let (event_tx, _event_rx) = std::sync::mpsc::channel();
        TiangongCore::builder()
            .session_id(session_id)
            .config(CoreConfigProvider::new(CoreConfig::default()))
            .trust_mode(crate::permission::TrustMode::FullTrust)
            .storage_root(root)
            .workspace_dir(root.to_string_lossy())
            .stream_tx(event_tx)
            .plugins(vec![])
            .model_endpoint(target("initial-model"))
            .build()
    }

    #[test]
    fn 构造时即持有实际模型() {
        let root = tempfile::tempdir().unwrap();
        let core = test_core(root.path(), "runtime-init");
        assert_eq!(core.current_endpoint().model, "initial-model");
        assert_eq!(
            core.current_model_key(),
            target("initial-model").model_key(),
            "模型标识直接从端点派生"
        );
    }

    #[test]
    fn 切换成功后当前模型更新并发出事件() {
        let root = tempfile::tempdir().unwrap();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let core = TiangongCore::builder()
            .session_id("runtime-switch")
            .config(CoreConfigProvider::new(CoreConfig::default()))
            .trust_mode(crate::permission::TrustMode::FullTrust)
            .storage_root(root.path())
            .workspace_dir(root.path().to_string_lossy())
            .stream_tx(event_tx)
            .plugins(vec![])
            .model_endpoint(target("initial-model"))
            .build();

        core.switch_model(target("picked-model")).unwrap();
        assert_eq!(core.current_endpoint().model, "picked-model");
        assert_eq!(core.current_model_key(), target("picked-model").model_key());

        let events: Vec<StreamEvent> = event_rx.try_iter().collect();
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::ModelSwitchStarted { model_name } if model_name == "picked-model"
            )),
            "应发出 ModelSwitchStarted"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::ModelSwitched { model_name } if model_name == "picked-model"
            )),
            "应发出 ModelSwitched"
        );
    }

    /// 模型切换必须在会话消息列表留痕，而不只是发流事件。
    ///
    /// 回归：早期实现只发 ModelSwitchStarted/ModelSwitched，那只驱动顶部
    /// 状态栏文案，刷新或切换会话后就消失——用户回看历史时无从得知某段
    /// 对话之后换过模型。
    #[test]
    fn 切换模型在消息列表留痕且不进模型上下文() {
        let root = tempfile::tempdir().unwrap();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let core = TiangongCore::builder()
            .session_id("switch-record")
            .config(CoreConfigProvider::new(CoreConfig::default()))
            .trust_mode(crate::permission::TrustMode::FullTrust)
            .storage_root(root.path())
            .workspace_dir(root.path().to_string_lossy())
            .stream_tx(event_tx)
            .plugins(vec![])
            .model_endpoint(target("initial-model"))
            .build();

        // 预置一轮对话，确保切换留痕写入的是既有会话。
        use crate::session::MessageRole;
        let mut session = Session::new("切换留痕");
        session.id = "switch-record".to_string();
        session.bind_storage_root(root.path().to_path_buf());
        session.append_message(MessageRole::User, "问题");
        session.append_message(MessageRole::Assistant, "回答");
        session.try_persist_to_disk().unwrap();

        core.switch_model(target("picked-model")).unwrap();

        let saved = Session::load_from_storage(root.path(), "switch-record").unwrap();
        let record_prefix = format!("[{CONTEXT_NOTICE_CATEGORY}] ");
        let record = saved
            .messages
            .iter()
            .find(|message| {
                message.text_content().starts_with(record_prefix.as_str())
                    && message.text_content().contains("已切换模型")
            })
            .expect("切换应在消息列表留痕");
        assert_eq!(
            record.role,
            MessageRole::Notice,
            "留痕用 Notice：按角色排除出模型上下文与压缩范围"
        );
        assert!(
            record.text_content().contains("picked-model"),
            "留痕应说明切换到哪个模型：{}",
            record.text_content()
        );
        assert!(
            !saved
                .context()
                .iter()
                .any(|message| message.text_content().starts_with(record_prefix.as_str())),
            "留痕不得进入模型上下文"
        );

        // 留痕同时推送前端，无需等切换会话重新读盘。
        let events: Vec<StreamEvent> = event_rx.try_iter().collect();
        assert!(
            events.iter().any(|event| matches!(
                event,
                StreamEvent::SessionMessageUpsert { message, .. }
                    if message.text_content().starts_with("[上下文管理] 已切换模型")
            )),
            "应推送留痕消息到前端"
        );
    }

    #[test]
    fn 端点不完整时切换失败且保持旧模型() {
        let root = tempfile::tempdir().unwrap();
        let core = test_core(root.path(), "runtime-invalid");
        let broken = endpoint("", "");
        let error = core.switch_model(broken).expect_err("不完整端点应拒绝切换");
        assert!(matches!(error, CoreError::ModelSwitchFailed(_)));
        assert_eq!(
            core.current_endpoint().model,
            "initial-model",
            "切换失败必须保持旧模型，下一次发送才会重新编排"
        );
    }

    #[test]
    fn 模型引用写入落盘且同值幂等() {
        let root = tempfile::tempdir().unwrap();
        let core = test_core(root.path(), "ref-persist");
        core.set_model_ref(Some("key-a".to_string())).unwrap();
        let loaded = Session::load_from_storage(root.path(), "ref-persist").unwrap();
        assert_eq!(loaded.model_ref.as_deref(), Some("key-a"));
        let first_updated = loaded.updated_at.clone();

        // 同值写入直接返回，不改 updated_at（不承担整份 session 读改写）。
        core.set_model_ref(Some("key-a".to_string())).unwrap();
        let again = Session::load_from_storage(root.path(), "ref-persist").unwrap();
        assert_eq!(again.updated_at, first_updated, "同值写入不应刷新时间戳");

        // None 恢复跟随默认（用户选择策略，不影响 Core 当前实际模型）。
        core.set_model_ref(None).unwrap();
        let cleared = Session::load_from_storage(root.path(), "ref-persist").unwrap();
        assert!(cleared.model_ref.is_none());
        assert_eq!(
            core.current_endpoint().model,
            "initial-model",
            "清除用户选择不改变 Core 已应用的实际模型"
        );
    }

    #[tokio::test]
    async fn 无可整理历史时不调用模型() {
        let root = tempfile::tempdir().unwrap();
        let core = test_core(root.path(), "compact-empty");
        // 空会话（无对话历史）：压缩任务判定 Noop 并立即给出终态，不触发模型请求。
        core.compact_context(MANUAL_COMPACT_NOTICE)
            .await
            .expect("无历史时整理上下文应直接成功");
    }

    /// 旧模型不可达时压缩必然失败，但等待必须结束（不能挂到超时）。
    ///
    /// 回归：早期实现靠轮询 `is_busy()` + 180s 上限判断收敛，端点不可用
    /// 会把用户卡在一次长等待里。现在由压缩任务主动通知终态。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 压缩失败时等待立即结束并返回错误() {
        let root = tempfile::tempdir().unwrap();
        let (event_tx, _event_rx) = std::sync::mpsc::channel();
        let core = TiangongCore::builder()
            .session_id("compact-unreachable")
            .config(CoreConfigProvider::new(CoreConfig::default()))
            .trust_mode(crate::permission::TrustMode::FullTrust)
            .storage_root(root.path())
            .workspace_dir(root.path().to_string_lossy())
            .stream_tx(event_tx)
            // 指向必然连接失败的地址，且超时很短。
            .model_endpoint(ModelEndpoint {
                timeout_ms: 300,
                ..endpoint("http://127.0.0.1:1", "dead-model")
            })
            .plugins(vec![])
            .build();

        // 预置可压缩历史，确保真的会发起模型请求。
        let mut session = Session::new("失败压缩");
        session.id = "compact-unreachable".to_string();
        session.bind_storage_root(root.path().to_path_buf());
        for round in [("第一问", "第一答"), ("第二问", "第二答")] {
            session.append_message(crate::session::MessageRole::User, round.0);
            session.append_message(crate::session::MessageRole::Assistant, round.1);
        }
        session.try_persist_to_disk().unwrap();

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            core.compact_context(MANUAL_COMPACT_NOTICE),
        )
        .await
        .expect("压缩失败必须及时结束等待，不得挂到固定超时")
        .expect_err("端点不可达时压缩应失败");
        assert!(matches!(error, CoreError::ContextCompactionFailed(_)));
        // 终态已到达：任务槽已释放，后续切换不会撞 Busy。
        assert!(!core.is_busy(), "压缩返回时任务槽必须已释放");
    }
}

#[cfg(test)]
mod shared_runtime_tests {
    use super::*;
    use crate::config::core::{CoreConfig, CoreConfigProvider};

    /// 多个未启动活动的 Core 不创建 driver 任务，且可正常关闭。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiple_cores_share_runtime_and_shutdown_cleanly() {
        let root = tempfile::tempdir().unwrap();

        let config = CoreConfigProvider::new(CoreConfig::default());
        let cores: Vec<TiangongCore> = (0..3)
            .map(|i| {
                let mut session = Session::new(format!("shared-runtime-{i}"));
                session.bind_storage_root(root.path());
                session.try_persist_to_disk().unwrap();
                let (event_tx, _event_rx) = std::sync::mpsc::channel();
                TiangongCore::builder()
                    .session_id(session.id.clone())
                    .config(config.clone())
                    .trust_mode(session.trust_mode)
                    .storage_root(root.path())
                    .workspace_dir(root.path().to_string_lossy())
                    .stream_tx(event_tx)
                    .plugins(vec![])
                    .model_endpoint(crate::core::test_support::test_model("http://test.invalid"))
                    .build()
            })
            .collect();

        // 从未投递输入的 Core 不创建 driver（is_stopped 语义：未关闭即可接收；
        // 尚无任何活动时注册表中无条目）。
        for core in &cores {
            assert!(core.is_stopped(), "未启动活动的 Core 不应存在 driver");
        }

        // 逐个关闭，验证 shutdown 路径在共享 runtime 上正常工作
        for core in cores {
            let result = tokio::task::spawn_blocking(move || core.shutdown_join()).await;
            assert!(result.is_ok(), "shutdown_join 不应 panic");
        }
    }
}
