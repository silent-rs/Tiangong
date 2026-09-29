//! ensure / retire：Core 生命周期入口。
//!
//! `ensure_core` 复刻桌面 `app.rs` 的逻辑（issue #241/#234 已收窄）：先取创建锁，
//! 命中既有 Core 则 replace_config + 同步会话级运行配置；否则用 host 传入的 plugins
//! 构造新 TiangongCore 并插入 registry。`retire_core` 先 cancel（可选）再 take +
//! shutdown_join。

use std::sync::Arc;
use std::sync::mpsc::Sender;

use tiangong_core::agent_input::{AgentInput, AgentInputKind, MessageInput};
use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
use tiangong_core::core::{Plugin, TiangongCore};
use tiangong_llm::ModelEndpoint;
use tiangong_types::StreamEvent;

use crate::CoreManager;
use crate::core_manager::EnsuredCore;

impl CoreManager {
    /// 确保 registry 中存在该会话的 Core，返回是否新建。
    ///
    /// **线程安全保证**：覆盖同一会话从首次检查到 Core 插入的完整创建区间
    /// （`creation_lock`），避免落选 Core 仍执行插件恢复钩子。
    ///
    /// - 命中既有 Core：替换配置并同步会话级运行配置，返回 `is_new=false`
    /// - 未命中：调用 `build_plugins` 构造插件集合，构造全新 TiangongCore 并插入 registry
    ///
    /// `initial_model_ref` 是本次发送携带的模型选择（新对话首条消息场景：
    /// Core 正是在这次发送中才创建）。提供时新 Core 直接以该目标模型
    /// 初始化，投递路径的模型比较判定一致、不触发切换编排；为 `None` 时
    /// 按 `Session.model_ref` / 路由默认解析（见 [`Self::resolve_initial_model`]）。
    /// 引用失效或凭据缺失时解析失败直接返回错误，不静默落到路由默认。
    ///
    /// `build_plugins` 是**按需回调**：只有 Core 不存在（需要新建）时才会被调用。
    /// 这样命中分支不会浪费一次完整的插件构造（含 WASM 实例化）。
    /// session 真相源是磁盘，Core 内部按需 `load_from_storage`。
    pub async fn ensure_core<F>(
        &self,
        session_id: &str,
        session_config: CoreConfig,
        workspace_dir: String,
        initial_model_ref: Option<&str>,
        stream_tx: Sender<StreamEvent>,
        build_plugins: F,
    ) -> Result<EnsuredCore, String>
    where
        F: FnOnce() -> Vec<Arc<dyn Plugin>>,
    {
        let creation_lock = self.creation_lock(session_id);
        let _creation_guard = creation_lock.lock_owned().await;

        // 命中既有 Core：刷新配置和会话运行设置（cwd 由磁盘真相源维护，无需投递）。
        // build_plugins 回调不会被调用，避免每次发送都重新构造插件集合。
        {
            let registry = self.registry();
            if let Some(core) = registry.get(session_id) {
                let _ = core.replace_config(session_config.clone());
                core.set_trust_mode(session_config.trust_mode);
                core.set_reasoning_effort(session_config.reasoning_effort);
                return Ok(EnsuredCore {
                    session_id: session_id.to_string(),
                    is_new: false,
                });
            }
        }

        // 未命中：Core 构造即需持有实际模型。优先使用本次发送携带的模型
        // 选择（新对话首条消息选择了非默认模型的场景），使新 Core 一次到位，
        // 投递路径的模型比较判定一致、不触发切换；未携带时按 Session.model_ref
        // 与当前模型注册表解析，失效或未设置时回退路由默认（见
        // `resolve_initial_model`）。解析不出任何可用模型时不静默兜底，由
        // 发送路径给出明确错误。
        let initial_model = match initial_model_ref {
            Some(model_ref) => self.resolve_turn_model(Some(model_ref))?,
            None => self.resolve_initial_model(session_id),
        };
        let plugins = build_plugins();
        let core = TiangongCore::builder()
            .session_id(session_id.to_string())
            .config(CoreConfigProvider::new(session_config.clone()))
            .trust_mode(session_config.trust_mode)
            .storage_root(self.storage_root.to_path_buf())
            .workspace_dir(workspace_dir)
            .stream_tx(stream_tx)
            .plugins(plugins)
            .model_endpoint(initial_model)
            .build();
        let id = core.session_id().to_string();
        self.registry().insert(id.clone(), core);
        Ok(EnsuredCore {
            session_id: id,
            is_new: true,
        })
    }

    /// Core 重建时的初始模型端点：按 Session.model_ref 与当前注册表解析。
    ///
    /// 用户选择失效（key 或 provider 已删）时给出**空端点**作为当前状态——
    /// 空端点与任何可用端点的 `model_key` 都不同，下一次用户选择有效模型
    /// 时 Manager 仍能识别出发生了切换；真正发送前的解析会给出明确报错，
    /// 不静默改成默认。
    fn resolve_initial_model(&self, session_id: &str) -> ModelEndpoint {
        let session_ref = self
            .load_session(session_id)
            .ok()
            .and_then(|session| session.model_ref);
        match session_ref {
            Some(key) => self.resolve_turn_model(Some(&key)).unwrap_or_default(),
            None => tiangong_config::default_chat_endpoint_at(
                &tiangong_config::io::load_models_config_at(&self.storage_root),
            )
            .unwrap_or_default(),
        }
    }

    /// 会话当前生效的上下文窗口（token 统计分母、压缩阈值的依据）。
    ///
    /// 优先取活跃 Core 的当前端点——它随模型切换同步变化；Core 未建时按
    /// 会话的模型选择解析。都解析不出时返回 `None`，由调用方兜底。
    pub fn session_context_limit(&self, session_id: &str) -> Option<usize> {
        let live = {
            let registry = self.registry();
            registry
                .get(session_id)
                .map(|core| core.current_endpoint())
                .filter(|endpoint| endpoint.is_usable())
        };
        let endpoint = match live {
            Some(endpoint) => endpoint,
            None => {
                let model_ref = self
                    .load_session(session_id)
                    .ok()
                    .and_then(|session| session.model_ref);
                self.resolve_turn_model(model_ref.as_deref()).ok()?
            }
        };
        endpoint.context_window.filter(|window| *window > 0)
    }

    /// 解析本轮的目标模型端点（`None` 表示跟随当前 Chat 默认）。
    ///
    /// `None` 只是选择策略，不能直接与 Core 当前模型比较——默认模型本身
    /// 会变化。必须先解析成实际目标再比较。
    pub fn resolve_turn_model(&self, model_ref: Option<&str>) -> Result<ModelEndpoint, String> {
        let models = tiangong_config::io::load_models_config_at(&self.storage_root);
        match model_ref.map(str::trim).filter(|key| !key.is_empty()) {
            Some(key) => {
                let entry = models.models.get(key).ok_or_else(|| {
                    format!("会话模型 {key} 已不在配置中，请重新选择模型或恢复该配置")
                })?;
                if !entry
                    .capabilities
                    .contains(&tiangong_llm::models_config::ModelCapability::Chat)
                {
                    return Err(format!("模型 {key} 不支持对话（缺少 Chat 能力）"));
                }
                let provider = models.providers.get(&entry.provider).ok_or_else(|| {
                    format!(
                        "会话模型 {key} 的服务提供方 {} 已删除，请重新选择模型或恢复该配置",
                        entry.provider
                    )
                })?;
                let api_key =
                    tiangong_llm::models_config::ModelsConfig::resolve_api_key(&provider.api_key);
                if api_key.trim().is_empty() && !provider.protocol.uses_oauth() {
                    return Err(format!(
                        "会话模型 {key} 的凭据未配置（服务提供方 {} 的 api_key 为空或其环境变量未设置）",
                        entry.provider
                    ));
                }
                Ok(ModelEndpoint::from_resolved(
                    tiangong_llm::models_config::ResolvedModel {
                        headers: provider.headers.clone(),
                        provider: entry.provider.clone(),
                        base_url: provider.base_url.clone(),
                        api_key,
                        timeout_ms: provider.timeout_ms,
                        protocol: provider.protocol,
                        model: entry.model.clone(),
                        options: entry.options.clone(),
                        context_window: entry.context_window,
                    },
                ))
            }
            None => {
                let target = tiangong_config::default_chat_endpoint_at(&models).unwrap_or_default();
                if !target.is_usable() {
                    return Err("未配置可用的对话模型，请先在设置中配置模型".to_string());
                }
                Ok(target)
            }
        }
    }

    /// 模型切换编排：目标与 Core 当前实际模型不一致时，先用旧模型整理
    /// 上下文，再切换到新模型。
    ///
    /// 压缩**不是**切换的前置条件：它只是尽力让新模型接手更紧凑的上下文。
    /// 压缩失败、被取消或启动失败都只记录警告并继续切换——否则旧模型不可
    /// 用（额度耗尽、服务下线）时用户将永远换不掉模型。但必须等到压缩进入
    /// 终态才切换，避免与压缩任务并发读写 Session、或切换时撞上 `Busy`。
    ///
    /// 只有模型切换本身失败才返回 Err（调用方据此中止发送）。
    ///
    /// 内部方法：模型编排属于 Manager 职责，宿主经
    /// [`Self::deliver_user_message`] 投递即可，不感知切换过程。
    async fn switch_model_if_needed(
        &self,
        session_id: &str,
        target: ModelEndpoint,
    ) -> Result<(), String> {
        let core = {
            let registry = self.registry();
            registry.get(session_id).cloned()
        };
        let Some(core) = core else {
            return Err("会话无活跃 Core".to_string());
        };
        let current = core.current_endpoint();
        // 身份按端点派生（base_url + model + protocol）而非注册表 key：
        // 不同平台的同名 model id 必须区分，同一模型换了 key 或凭据则无需
        // 重新压缩切换。
        if current.is_same_model(&target) {
            return Ok(());
        }
        // 当前模型不可用（失效引用：Core 重建时端点为空）时跳过整理——用一个
        // 无法发请求的端点压缩必然失败，白等一轮。此时历史也从未被该模型
        // 处理过，直接切换即可。
        if current.is_usable() {
            if let Err(error) = core.compact_context("切换模型前已整理上下文").await {
                tracing::warn!(
                    session_id,
                    %error,
                    target_model = %target.model,
                    "模型切换前上下文整理未完成，继续切换模型"
                );
            }
        } else {
            tracing::info!(
                session_id,
                target_model = %target.model,
                "当前模型不可用，跳过切换前的上下文整理"
            );
        }
        core.switch_model(target).map_err(|error| match error {
            tiangong_core::core::CoreError::Busy => {
                "会话正在执行，当前回合结束后可切换模型".to_string()
            }
            other => other.to_string(),
        })
    }

    /// 仅供集成测试驱动模型编排（不投递消息）。
    ///
    /// 生产路径请用 [`Self::deliver_user_message`]：模型编排是投递的内部
    /// 步骤，宿主不应单独触发切换。
    #[doc(hidden)]
    pub async fn switch_model_for_test(
        &self,
        session_id: &str,
        target: ModelEndpoint,
    ) -> Result<(), String> {
        self.switch_model_if_needed(session_id, target).await
    }

    /// 投递用户消息：Manager 内部完成模型编排后再投递，宿主无感。
    ///
    /// 模型选择随消息携带（`AgentInputKind::with_model_ref`），本方法据此：
    /// 1. 记录到 `Session.model_ref`（选择策略，持久化供前端回显）；
    /// 2. 解析成实际目标端点；
    /// 3. 与 Core 当前实际模型比较，不同则先整理上下文再切换；
    /// 4. 投递消息。
    ///
    /// 宿主不应再单独调用模型切换相关接口——切换全程是本方法的内部步骤。
    /// 非用户消息（命令等）不做编排，直接投递。
    ///
    /// **会话执行中不做任何模型编排**：此时消息是引导消息，由活跃 turn 接管，
    /// 本轮沿用当前模型。整理上下文与切换模型在运行中都会被拒绝（`Busy`），
    /// 若据此判定投递失败，宿主的失败回滚会关闭 Core——正在进行的对话被直接
    /// 打断并显示为失败。
    pub async fn deliver_user_message(
        &self,
        session_id: &str,
        input: AgentInputKind,
    ) -> Result<(), String> {
        let model_ref = match &input {
            AgentInputKind::Message(MessageInput::UserMessage { model_ref, .. }) => model_ref
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(str::to_string),
            _ => None,
        };
        if self.is_core_busy(session_id) {
            tracing::info!(session_id, "会话执行中：引导消息沿用当前模型，跳过模型编排");
            return self
                .deliver_to_core_if_live(session_id, input)
                .then_some(())
                .ok_or_else(|| "会话 Core 投递失败".to_string());
        }
        // 先记录用户选择：忙时拒写仅告警（运行中不换模型，本轮沿用当前模型）。
        if let Some(key) = model_ref.clone()
            && let Err(error) = self.set_core_model_ref(session_id, Some(key))
        {
            tracing::warn!(session_id, error, "写入会话模型引用失败");
        }
        let target = self.resolve_turn_model(model_ref.as_deref())?;
        self.switch_model_if_needed(session_id, target).await?;
        self.deliver_to_core_if_live(session_id, input)
            .then_some(())
            .ok_or_else(|| "会话 Core 投递失败".to_string())
    }

    /// 关闭并等待指定会话的 Core 结束。
    ///
    /// Core 的 worker join 是同步阻塞调用，本方法用 `spawn_blocking` 包裹以适配
    /// async 调用方。`cancel` 为 true 时先投递 `Command::Cancel` 再 take + join，
    /// 用于删除会话等需要主动终止在途 turn 的场景；失败回滚传 false（仅取走本次
    /// 绑定的 Core 并等其写盘结束）。Core 不存在时直接返回。
    pub async fn retire_core(&self, session_id: &str, cancel: bool) -> Result<(), String> {
        let creation_lock = self.creation_lock(session_id);
        let _creation_guard = creation_lock.lock_owned().await;
        self.retire_core_locked(session_id, cancel).await
    }

    pub(crate) async fn retire_core_locked(
        &self,
        session_id: &str,
        cancel: bool,
    ) -> Result<(), String> {
        if cancel {
            let _ = self.cancel_core(session_id);
        }
        let Some(core) = self.take_core(session_id) else {
            return Ok(());
        };
        let sid = session_id.to_string();
        match tokio::task::spawn_blocking(move || core.shutdown_join()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(format!("关闭会话 {sid} 的 Core 失败：{error}")),
            Err(error) => Err(format!("等待会话 {sid} 的 Core 关闭失败：{error}")),
        }
    }

    /// 取消指定会话的执行。
    ///
    /// 返回 true 表示 Cancel 已投递到活跃 turn task，false 表示当时没有可接受
    /// 取消命令的活跃 task；它不表示 turn 已经完成收尾。
    pub fn cancel_core(&self, session_id: &str) -> bool {
        let registry = self.registry();
        registry
            .get(session_id)
            .is_some_and(|core| core.deliver(AgentInputKind::cancel()).is_ok())
    }

    /// 取回会话 Core（消费，用于持久化或显式切换）。
    pub fn take_core(&self, session_id: &str) -> Option<TiangongCore> {
        self.registry().remove(session_id)
    }

    /// 仅当指定会话存在 Core 时投递输入。
    ///
    /// 投递成功且输入为用户消息时顺带触发标题自动生成（见 [`title`] 模块）；
    /// 标题生成在后台进行，不影响投递返回。
    pub fn deliver_to_core_if_live(&self, session_id: &str, input: AgentInputKind) -> bool {
        let input = self.prepare_images_for_model(session_id, input);
        let user_text = match &input {
            AgentInputKind::Message(MessageInput::UserMessage { prepared, .. }) => prepared
                .iter()
                .find_map(|block| block.as_text().map(str::to_string)),
            _ => None,
        };
        let registry = self.registry();
        let delivered = registry
            .get(session_id)
            .is_some_and(|core| core.deliver(input).is_ok());
        if delivered && let Some(text) = user_text {
            self.spawn_title_generation_if_needed(session_id, &text);
        }
        delivered
    }

    /// 按会话模型决定用户消息中图片发送给 Core 的形式。
    ///
    /// 宿主只把附件归档到存储目录并以路径引用（`AssetReference`）注入；
    /// 会话模型（`Session.model_ref`，未选择时为 Chat 默认模型）支持多模态时，
    /// 这里把图片读出并转为 base64 原生图片，否则原样保留路径引用——是否调用
    /// 图片分析工具由 Agent 自行决定。
    pub(crate) fn prepare_images_for_model(
        &self,
        session_id: &str,
        input: AgentInputKind,
    ) -> AgentInputKind {
        let AgentInputKind::Message(MessageInput::UserMessage {
            prepared,
            message_id,
            model_ref,
        }) = input
        else {
            return input;
        };
        let prepared = if prepared.iter().any(is_image_reference)
            && self.session_model_is_multimodal(session_id)
        {
            prepared.into_iter().map(inline_image).collect()
        } else {
            prepared
        };
        AgentInputKind::Message(MessageInput::UserMessage {
            prepared,
            message_id,
            model_ref,
        })
    }

    /// 会话当前模型是否支持多模态（按 `Session.model_ref` 查模型注册表）。
    fn session_model_is_multimodal(&self, session_id: &str) -> bool {
        let models = tiangong_config::io::load_models_config_at(&self.storage_root);
        let model_ref = self
            .load_session(session_id)
            .ok()
            .and_then(|session| session.model_ref);
        match model_ref.as_deref().and_then(|key| models.models.get(key)) {
            Some(entry) => entry
                .capabilities
                .contains(&tiangong_llm::ModelCapability::Multimodal),
            None => models.chat_is_multimodal(),
        }
    }

    /// 手动整理会话上下文，并等待压缩进入终态。
    ///
    /// 与模型切换前的整理（见 [`Self::switch_model_if_needed`]）走**同一条**
    /// 实现：都经 `TiangongCore::compact_context` 等待压缩任务的终态通知，而
    /// 不是投递命令后即发即走。区别只在失败处理——切换场景失败只告警并继续
    /// 切换（模型已由用户选定，不能因整理失败卡住），手动整理则把失败如实
    /// 回报给调用方，由用户决定是否重试。
    ///
    /// 返回 `Ok(())` 表示压缩已应用或无可压缩历史；`Err` 表示未能完成。
    pub async fn compact_session_context(&self, session_id: &str) -> Result<(), String> {
        let core = {
            let registry = self.registry();
            registry.get(session_id).cloned()
        };
        let Some(core) = core else {
            return Err("会话无活跃 Core".to_string());
        };
        core.compact_context(tiangong_core::core::MANUAL_COMPACT_NOTICE)
            .await
            .map_err(|error| match error {
                tiangong_core::core::CoreError::Busy => {
                    "会话正在执行，当前回合结束后可整理上下文".to_string()
                }
                other => other.to_string(),
            })
    }

    /// 记录会话级对话模型选择（models 注册表 key；None 恢复跟随默认）。
    ///
    /// 只写 `Session.model_ref` 这一**选择策略**，不触碰执行端点：实际切换
    /// 由下一次投递时的模型编排完成（见 [`Self::deliver_user_message`]），
    /// 因此用户切走又切回时端点从未变化、也不会白白整理一次上下文。
    /// 运行中不写（core 忙即拒绝）。
    pub fn set_core_model_ref(
        &self,
        session_id: &str,
        model_ref: Option<String>,
    ) -> Result<(), String> {
        let registry = self.registry();
        let core = registry
            .get(session_id)
            .ok_or_else(|| "会话无活跃 Core".to_string())?;
        core.set_model_ref(model_ref).map_err(|error| match error {
            tiangong_core::core::CoreError::Busy => {
                "会话正在执行，当前回合结束后可切换模型".to_string()
            }
            other => format!("写入会话模型失败：{other}"),
        })
    }

    /// 设置指定会话 core 的信任模式（实时生效）。
    pub fn set_core_trust_mode(&self, session_id: &str, mode: tiangong_types::TrustMode) {
        let registry = self.registry();
        if let Some(core) = registry.get(session_id) {
            core.set_trust_mode(mode);
        }
    }

    /// 设置指定会话 Core 的思考强度（下一次尚未发出的模型请求生效）。
    pub fn set_core_reasoning_effort(
        &self,
        session_id: &str,
        effort: tiangong_llm::request::ReasoningEffort,
    ) {
        let registry = self.registry();
        if let Some(core) = registry.get(session_id) {
            core.set_reasoning_effort(effort);
        }
    }

    /// 更新指定会话标题（落盘始终由 Core 负责，保证不与 turn 对 session 的读写竞争）。
    ///
    /// 必须存在 live Core（标题可编辑意味着 Core 应已创建）；不存在则视为异常并报错。
    /// Core 内部按 is_busy 分流：忙时投递 turn task，Core 空闲时 Core 自己写盘。
    ///
    /// `only_if_default=true` 时仅当当前标题仍是默认值才覆盖（lite 自动生成用，
    /// 用户手动改过则不覆盖）；用户手动编辑传 false。
    pub fn set_core_title(
        &self,
        session_id: &str,
        title: String,
        only_if_default: bool,
    ) -> Result<(), String> {
        let registry = self.registry();
        let Some(core) = registry.get(session_id) else {
            return Err(format!("会话 {session_id} 无可用 Core，无法更新标题"));
        };
        core.set_title(title, only_if_default)
            .map_err(|_| "更新会话标题失败".to_string())
    }
}

/// 可发送给多模态模型的图片引用（SVG 等模型不接受的格式按普通文件处理）。
fn is_image_reference(block: &tiangong_types::ContentBlock) -> bool {
    matches!(
        block,
        tiangong_types::ContentBlock::AssetReference { asset }
            if asset.kind == tiangong_types::MediaKind::Image
                && asset.mime_type != "image/svg+xml"
    )
}

/// 把图片路径引用读出并转为 base64 原生图片；读取失败时保留路径引用。
fn inline_image(block: tiangong_types::ContentBlock) -> tiangong_types::ContentBlock {
    use base64::Engine as _;
    use tiangong_types::ContentBlock;

    if !is_image_reference(&block) {
        return block;
    }
    let ContentBlock::AssetReference { asset } = block else {
        unreachable!("is_image_reference 已确认是图片引用");
    };
    match std::fs::read(&asset.local_path) {
        Ok(bytes) => ContentBlock::Image {
            asset,
            data: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
        },
        // 读取失败时保留路径引用，不让整条消息投递失败。
        Err(error) => {
            tracing::warn!(path = %asset.local_path, %error, "读取图片附件失败，保留路径引用");
            ContentBlock::AssetReference { asset }
        }
    }
}

#[cfg(test)]
mod image_injection_tests {
    use super::*;
    use tiangong_types::{ContentBlock, MediaKind, StoredAsset};

    fn image_reference(path: &std::path::Path) -> ContentBlock {
        ContentBlock::AssetReference {
            asset: StoredAsset {
                asset_id: "a1".to_string(),
                local_path: path.to_string_lossy().into_owned(),
                original_name: "shot.png".to_string(),
                mime_type: "image/png".to_string(),
                size: 3,
                kind: MediaKind::Image,
            },
        }
    }

    fn write_models(root: &std::path::Path, chat: &str) {
        std::fs::write(
            root.join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {
                    "text": {"provider": "p", "model": "text-model", "capabilities": ["chat"]},
                    "vision": {"provider": "p", "model": "vision-model", "capabilities": ["chat", "multimodal"]}
                },
                "routing": {"chat": chat}
            })
            .to_string(),
        )
        .unwrap();
    }

    fn manager_with_session(
        model_ref: Option<&str>,
        default_chat: &str,
    ) -> (tempfile::TempDir, CoreManager, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        write_models(dir.path(), default_chat);
        let image = dir.path().join("shot.png");
        std::fs::write(&image, b"img").unwrap();
        let manager = CoreManager::new(
            CoreConfigProvider::new(CoreConfig::default()),
            dir.path().to_path_buf(),
        );
        let mut session = tiangong_core::session::Session::new("图片注入");
        session.id = "s1".to_string();
        session.model_ref = model_ref.map(str::to_string);
        session.bind_storage_root(dir.path());
        session.try_persist_to_disk().unwrap();
        (dir, manager, image)
    }

    fn deliver(manager: &CoreManager, image: &std::path::Path) -> Vec<ContentBlock> {
        let input = AgentInputKind::prepared_with_id(
            "m1",
            vec![ContentBlock::text("看图"), image_reference(image)],
        );
        let AgentInputKind::Message(MessageInput::UserMessage { prepared, .. }) =
            manager.prepare_images_for_model("s1", input)
        else {
            panic!("应为用户消息");
        };
        prepared
    }

    #[test]
    fn 多模态会话模型改为_base64_原生图片() {
        // 默认 chat 不支持多模态，但会话选了多模态模型：以会话模型为准。
        let (_dir, manager, image) = manager_with_session(Some("vision"), "text");
        let prepared = deliver(&manager, &image);
        assert_eq!(prepared.len(), 2);
        assert!(matches!(
            &prepared[1],
            ContentBlock::Image { data: Some(data), .. } if data == "aW1n"
        ));
    }

    #[test]
    fn 非多模态会话模型原样保留路径引用() {
        let (_dir, manager, image) = manager_with_session(Some("text"), "vision");
        let prepared = deliver(&manager, &image);
        assert_eq!(
            prepared,
            vec![ContentBlock::text("看图"), image_reference(&image)]
        );
    }

    #[test]
    fn 未选择模型时跟随默认_chat_模型() {
        let (_dir, manager, image) = manager_with_session(None, "vision");
        assert!(matches!(
            deliver(&manager, &image)[1],
            ContentBlock::Image { .. }
        ));
        let (_dir, manager, image) = manager_with_session(None, "text");
        assert!(matches!(
            deliver(&manager, &image)[1],
            ContentBlock::AssetReference { .. }
        ));
    }

    #[test]
    fn 非图片与_svg_引用保持不变() {
        let svg = ContentBlock::AssetReference {
            asset: StoredAsset {
                asset_id: "s".to_string(),
                local_path: "/media/a.svg".to_string(),
                original_name: "a.svg".to_string(),
                mime_type: "image/svg+xml".to_string(),
                size: 1,
                kind: MediaKind::Image,
            },
        };
        assert_eq!(inline_image(svg.clone()), svg);
    }
}
