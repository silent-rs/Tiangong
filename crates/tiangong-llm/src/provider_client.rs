//! 配置好的生成客户端：stream(Vec<Message>) 在 LLM 内进行纯转换与 ProviderRequest 装配。
//! 不理解 Session、不读取文件；调用方准备媒体与会话策略。旧请求类型/执行桥仅用于回归基线。
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

use crate::endpoint::ModelEndpoint;
use crate::message::{
    ChatMessage, MessageContent as LlmMessageContent, MessageRole as LlmMessageRole,
};
use crate::model::ProviderProtocol;
use crate::provider::{LlmProvider, ModelCatalog};
use crate::providers::anthropic::{AnthropicConfig, AnthropicProvider};
use crate::providers::codex::{CodexConfig, CodexProvider};
use crate::providers::deepseek::{DeepSeekConfig, DeepSeekProvider};
use crate::providers::openai::{OpenAiResponsesConfig, OpenAiResponsesProvider};
use crate::providers::openai_chatcompletions::{OpenAiChatCompletionsProvider, OpenAiChatConfig};
use crate::request::ProviderRequest;
use crate::response::{ProviderResponse, StopReason};
use crate::stream::{ProviderStream, ProviderStreamEvent};
use crate::tool::{
    ToolCall, ToolChoice, ToolChoice as LlmToolChoice, ToolResult as LlmToolResult, ToolSpec,
};
use crate::usage::TokenUsageData;
use tiangong_types::TokenUsage;
use tokio::runtime::Builder as TokioRuntimeBuilder;

pub use crate::request::ReasoningEffort;

#[cfg(test)]
fn merge_stream_usage(current: &mut TokenUsageData, next: TokenUsageData) {
    current.prompt_tokens = current.prompt_tokens.max(next.prompt_tokens);
    current.completion_tokens = current.completion_tokens.max(next.completion_tokens);
    let computed_total = current.prompt_tokens + current.completion_tokens;
    current.total_tokens = current
        .total_tokens
        .max(next.total_tokens)
        .max(computed_total);
    if let Some(hit) = next.prompt_cache_hit_tokens {
        current.prompt_cache_hit_tokens = Some(hit);
    }
    if let Some(miss) = next.prompt_cache_miss_tokens {
        current.prompt_cache_miss_tokens = Some(miss);
    }
}

#[cfg(test)]
fn append_stream_tool_call_arguments(raw_args: &mut String, partial_json: &str) {
    if raw_args.trim().is_empty() || serde_json::from_str::<Value>(raw_args).is_err() {
        raw_args.push_str(partial_json);
    }
}

/// 已组装好的模型请求。
///
/// 会话语义（选取上下文、模型指令、附件、图片读取与降级、兜底消息等）由
/// 调用方组装为 `system` 与 `messages`；llm 层只做协议映射与必要的协议
/// 约束修正（如 tool_call / tool_result 配对），不理解宿主消息格式。
#[derive(Debug, Clone, Default)]
pub struct ModelRequest {
    /// 系统提示；空白时请求不带 system。
    pub system: String,
    /// 对话消息，至少需要一条 User 消息。
    pub messages: Vec<ChatMessage>,
    /// 会话级缓存路由标识，不参与消息内容构造。
    pub session_id: Option<String>,
    /// 思考强度：None 关闭思考，其余档位开启。
    pub reasoning_effort: ReasoningEffort,
    /// 该请求允许的最大输出 token 数。
    ///
    /// `None` 时由 llm 层用默认上限（`MAX_TOKENS_MAIN`）。压缩等空间敏感请求
    /// 应显式设置：按 `context_limit - 预估 prompt_tokens` 计算，留出足够
    /// 摘要输出空间，避免 provider 因 `prompt + max_tokens > limit` 报错。
    pub max_output_tokens: Option<u32>,
    /// 完整工具定义及顺序；普通对话和摘要均从这里传递。
    pub tools: Vec<ToolSpec>,
    /// 未指定时，有工具则允许自动调用；Some(None) 明确禁止调用但保留声明。
    pub tool_choice: Option<ToolChoice>,
    /// 未指定时沿用现有全局温度配置。
    pub temperature: Option<f32>,
    /// 显式请求超时优先于环境变量及端点配置。
    pub timeout_ms: Option<u64>,
}

impl ModelRequest {
    pub fn from_messages(messages: Vec<tiangong_types::Message>) -> Result<Self> {
        let (system, messages) = crate::input::convert_messages(messages)?;
        Ok(Self {
            system: system.unwrap_or_default(),
            messages,
            ..Default::default()
        })
    }
}
impl ModelRequest {
    pub fn with_tools(mut self, tools: Vec<ToolSpec>, tool_choice: Option<ToolChoice>) -> Self {
        self.tools = tools;
        self.tool_choice = tool_choice;
        self
    }

    pub fn with_max_output_tokens(mut self, max_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_tokens);
        self
    }
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub provider_usage: Option<TokenUsageData>,
    /// 权威模型内容；便捷字段用于旧调用方迁移。
    pub output: Vec<LlmMessageContent>,
    pub status: crate::collect::CompletionStatus,
    pub text: String,
    pub reasoning_content: String,
    pub reasoning_signature: Option<String>,
    pub stop_reason: Option<StopReason>,
    pub usage: TokenUsage,
    pub tool_calls: Vec<ToolCall>,
    pub invalid_tool_calls: Vec<InvalidToolCall>,
    /// 输出阶段耗时（毫秒），**decode 口径、真实测量**：首个输出增量
    /// 到最后一个输出增量的实测间隔，不含 TTFT（排队 + prefill / 网关
    /// 预热）。输出增量包括正文、思考与**工具调用增量**——纯工具调用
    /// 轮同样有值。计时在 SSE 消费侧（事件真实到达时刻）完成，不做
    /// 任何阈值判别/置空/折算：批量下发端点的轮次间隔会被压缩（数值
    /// 偏高），TTFT 列（含网关预热）是解读依据，由消费方自行判断。
    ///
    /// TPS 口径：`usage.completion_tokens / output_elapsed_ms`。分子官方
    /// 计量含思考 token，分母含思考输出时段，两者同口径。
    pub output_elapsed_ms: Option<u64>,
    /// 思考阶段耗时（毫秒）：首个思考增量到最后一个思考增量的间隔；
    /// 仅供思考块「思考耗时」展示，不参与 TPS 分母（思考时段已含在
    /// `output_elapsed_ms` 内）。无思考增量时为 `None`。
    pub reasoning_elapsed_ms: Option<u64>,
    /// 首字延迟 TTFT（毫秒）：请求发出（HTTP 发起）到首个输出增量到达的
    /// 间隔，含排队与服务端 prefill。独立于 TPS 的延迟指标——诊断「模型
    /// 卡多久才开始吐字」；与 `output_elapsed_ms` 互补（端到端 ≈ TTFT +
    /// 输出耗时）。无输出增量时为 `None`。
    pub ttft_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct ModelStreamChunk {
    pub content: String,
    pub reasoning_content: String,
    pub usage: Option<TokenUsageData>,
    /// 截至本 chunk 的累计 output tokens（实时 TPS 数据源）。
    ///
    /// 口径：流中按输出 delta 事件计数（正文与思考增量各计 1）。这是 token
    /// 数的**近似**——各 provider / 代理的 SSE 分块粒度不一（DeepSeek 官方
    /// 接近逐 token，opencode 等代理常把多个 token 聚合进一个 chunk，事件数
    /// 会低于 token 数）；无任何分词开销。`Usage` 事件到达时用官方
    /// `completion_tokens` 校准覆盖，结束收敛到官方计量。
    pub cumulative_tokens: Option<u64>,
}

pub trait ModelClient {
    fn api_base_url(&self) -> &str;
    fn api_timeout_ms(&self) -> u64;
    fn api_model(&self) -> &str;
    #[cfg(test)]
    fn complete(&self, req: ModelRequest) -> Result<ModelResponse>;
    /// 流式调用，通过 on_delta 实时回调内容和用量。
    #[cfg(test)]
    fn complete_stream(
        &self,
        req: ModelRequest,
        on_delta: &mut dyn FnMut(&ModelStreamChunk),
    ) -> Result<ModelResponse>;
}

/// 重试回调类型：(attempt, max_attempts, delay_ms, error_text)
pub type OnRetryCallback = Arc<dyn Fn(u32, u32, u64, &str) + Send + Sync>;

type ProviderCache =
    Arc<std::sync::Mutex<std::collections::HashMap<(u64, String, u32), ProviderHandle>>>;
#[derive(Clone)]
pub struct SingleProviderClient {
    session_id: String,
    cfg: ModelEndpoint,
    /// 重试时的回调通知（可选）
    on_retry: Option<OnRetryCallback>,
    providers: ProviderCache,
    options: crate::Options,
}

impl std::fmt::Debug for SingleProviderClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleProviderClient")
            .field("cfg", &self.cfg)
            .field("on_retry", &self.on_retry.is_some())
            .finish()
    }
}

impl SingleProviderClient {
    pub fn with_options(mut self, options: crate::Options) -> Self {
        self.options = options;
        self
    }
    pub fn with_tool_choice(mut self, tool_choice: Option<ToolChoice>) -> Self {
        self.options.tool_choice = tool_choice;
        self
    }
    pub fn with_max_output_tokens(mut self, max_tokens: u32) -> Self {
        self.options.max_output_tokens = max_tokens;
        self
    }
    pub fn options(&self) -> &crate::Options {
        &self.options
    }
    pub(crate) fn request_from_messages(
        &self,
        messages: Vec<tiangong_types::Message>,
    ) -> Result<ProviderRequest> {
        if self.cfg.model.trim().is_empty() {
            return Err(anyhow!("API_MODEL 不能为空，无法发起模型请求"));
        }
        let (system, messages) = crate::input::convert_messages(messages)?;
        let messages = sanitize_provider_messages(messages);
        if messages.is_empty() {
            return Err(anyhow!("模型请求缺少有效消息"));
        }
        Ok(ProviderRequest {
            model: self.cfg.model.trim().to_owned(),
            system,
            messages,
            session_id: self
                .options
                .cache_key
                .clone()
                .or_else(|| Some(self.session_id.clone())),
            tools: self.options.tools.clone(),
            tool_choice: self
                .options
                .tool_choice
                .clone()
                .or_else(|| (!self.options.tools.is_empty()).then_some(LlmToolChoice::Auto)),
            max_tokens: self.options.max_output_tokens,
            temperature: self.options.temperature,
            top_p: self.options.top_p,
            stop_sequences: self.options.stop_sequences.clone(),
            metadata: None,
            reasoning_effort: self.options.reasoning_effort,
        })
    }
    pub(crate) async fn consume_request(
        &self,
        request: ProviderRequest,
        chunks: Option<tokio::sync::mpsc::UnboundedSender<ModelStreamChunk>>,
    ) -> Result<ModelResponse> {
        let timeout = self
            .options
            .timeout_ms
            .unwrap_or_else(|| function_timeout_ms(self.cfg.timeout_ms));
        let provider = self.build_provider_dispatch(
            timeout,
            request.session_id.as_deref(),
            self.options.max_retries,
        )?;
        let mut observe = |chunk: &ModelStreamChunk| {
            if let Some(tx) = &chunks {
                let _ = tx.send(chunk.clone());
            }
        };
        if let Some(tx) = &chunks {
            tokio::select! {
                biased;
                _ = tx.closed() => Err(anyhow!("流式消费者已关闭")),
                result = consume_stream_with_retry(&provider, &request, &mut observe, true, self.on_retry.clone()) => result,
            }
        } else {
            consume_stream_with_retry(
                &provider,
                &request,
                &mut observe,
                true,
                self.on_retry.clone(),
            )
            .await
        }
    }
    /// 按值消费请求：消息直接移入 provider 请求，不复制消息内容。
    #[cfg(test)]
    fn build_provider_request(&self, req: ModelRequest) -> Result<ProviderRequest> {
        let model = self.cfg.model.trim();
        if model.is_empty() {
            return Err(anyhow!("API_MODEL 不能为空，无法发起模型请求"));
        }
        let messages = sanitize_provider_messages(req.messages);
        if messages.is_empty() {
            return Err(anyhow!(
                "模型请求缺少有效的 User 消息，无法发起模型请求（消息应由调用方组装）"
            ));
        }
        let system = req.system.trim();
        let tool_choice = req
            .tool_choice
            .or_else(|| (!req.tools.is_empty()).then_some(LlmToolChoice::Auto));
        Ok(ProviderRequest {
            session_id: req.session_id.or_else(|| Some(self.session_id.clone())),
            model: model.to_string(),
            system: (!system.is_empty()).then(|| system.to_string()),
            messages,
            tools: req.tools,
            tool_choice,
            max_tokens: req.max_output_tokens.unwrap_or(MAX_TOKENS_MAIN),
            temperature: req.temperature.or_else(configured_temperature_f32),
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: req.reasoning_effort,
        })
    }

    /// 构建拥有所有输入的统一请求；业务工具校验由 Core 完成。
    #[cfg(test)]
    fn prepare_request(&self, req: ModelRequest) -> Result<(ProviderHandle, ProviderRequest)> {
        let timeout_ms = req
            .timeout_ms
            .unwrap_or_else(|| function_timeout_ms(self.cfg.timeout_ms));
        let provider = self.build_provider_dispatch(
            timeout_ms,
            req.session_id.as_deref(),
            crate::retry::DEFAULT_MAX_RETRIES,
        )?;
        let request = self.build_provider_request(req)?;
        Ok((provider, request))
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }
    pub fn model_name(&self) -> &str {
        self.cfg.model.trim()
    }
    /// 收集唯一流式生成链路；保留完整性状态和用量。
    #[cfg(test)]
    pub async fn collect_async(&self, req: ModelRequest) -> Result<ModelResponse> {
        let (provider, request) = self.prepare_request(req)?;
        let preserve_order = true;
        let response = consume_stream_with_retry(
            &provider,
            &request,
            &mut |_| {},
            preserve_order,
            self.on_retry.clone(),
        )
        .await?;
        Ok(response)
    }

    /// 观察并收集同一条流；取消 future 时关闭底层 HTTP 读取。
    #[cfg(test)]
    pub async fn stream_async(
        self,
        req: ModelRequest,
        chunk_tx: tokio::sync::mpsc::UnboundedSender<ModelStreamChunk>,
    ) -> Result<ModelResponse> {
        let (provider, request) = self.prepare_request(req)?;
        let preserve_tool_call_order = matches!(
            self.protocol(),
            ProviderProtocol::OpenAi
                | ProviderProtocol::OpenAiChatCompletions
                | ProviderProtocol::Codex
        );
        let mut on_chunk = |chunk: &ModelStreamChunk| {
            let _ = chunk_tx.send(chunk.clone());
        };
        let response = tokio::select! {
            biased;
            _ = chunk_tx.closed() => return Err(anyhow!("流式消费者已关闭")),
            response = consume_stream_with_retry(
                &provider, &request,
                &mut on_chunk,
                preserve_tool_call_order, self.on_retry.clone(),
            ) => response?,
        };
        Ok(response)
    }

    pub fn new(cfg: ModelEndpoint) -> Self {
        Self {
            session_id: scru128::new().to_string(),
            cfg,
            on_retry: None,
            providers: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            options: crate::Options {
                temperature: configured_temperature_f32(),
                ..Default::default()
            },
        }
    }

    /// 设置重试回调
    pub fn with_on_retry(mut self, cb: OnRetryCallback) -> Self {
        self.on_retry = Some(cb);
        self.providers = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        self
    }

    /// 拉取模型目录并保留服务端元信息（目前仅 Codex 返回上下文窗口）。
    ///
    /// Codex 按服务端顺序（新模型在前）去重返回；其他协议退化为
    /// [`Self::list_models_async`] 的结果，`context_window` 为空。
    pub async fn list_model_infos_async(
        cfg: &ModelEndpoint,
    ) -> Result<Vec<crate::model::ProviderModelInfo>> {
        if cfg.protocol != ProviderProtocol::Codex {
            return Ok(Self::list_models_async(cfg)
                .await?
                .into_iter()
                .map(|id| crate::model::ProviderModelInfo {
                    id,
                    ..Default::default()
                })
                .collect());
        }
        Self::list_codex_model_infos(cfg).await
    }
    /// Codex 模型目录：按服务端顺序（新模型在前），保留上下文窗口。
    async fn list_codex_model_infos(
        cfg: &ModelEndpoint,
    ) -> Result<Vec<crate::model::ProviderModelInfo>> {
        let provider =
            build_codex_provider_from_config(cfg, cfg.timeout_ms, &scru128::new().to_string())?;
        provider.list_models().await.map_err(map_llm_error)
    }
    /// [`Self::list_model_infos_async`] 的同步版本（CLI 使用）。
    pub fn list_model_infos(cfg: &ModelEndpoint) -> Result<Vec<crate::model::ProviderModelInfo>> {
        let runtime = TokioRuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .context("初始化异步运行时失败")?;
        runtime.block_on(Self::list_model_infos_async(cfg))
    }
    pub async fn list_models_async(cfg: &ModelEndpoint) -> Result<Vec<String>> {
        if cfg.api_key.trim().is_empty() && !cfg.protocol.uses_oauth() {
            return Err(anyhow!("API_AUTH_TOKEN 不能为空，无法更新模型列表"));
        }

        let timeout_ms = cfg.timeout_ms;
        let mut models = if cfg.protocol == ProviderProtocol::Anthropic {
            let provider = build_anthropic_provider_from_config(
                cfg,
                timeout_ms,
                None,
                &scru128::new().to_string(),
                MODEL_LIST_MAX_RETRIES,
            )?;
            provider
                .list_models()
                .await
                .map(|items| items.into_iter().map(|item| item.id).collect::<Vec<_>>())
                .map_err(map_llm_error)?
        } else if cfg.protocol == ProviderProtocol::DeepSeek {
            let provider = build_deepseek_provider_from_config(
                cfg,
                timeout_ms,
                None,
                &scru128::new().to_string(),
                MODEL_LIST_MAX_RETRIES,
            )?;
            provider
                .list_models()
                .await
                .map(|items| items.into_iter().map(|item| item.id).collect::<Vec<_>>())
                .map_err(map_llm_error)?
        } else if cfg.protocol == ProviderProtocol::Codex {
            return Ok(Self::list_codex_model_infos(cfg)
                .await?
                .into_iter()
                .map(|item| item.id)
                .collect());
        } else {
            let provider = build_openai_provider_from_config(
                cfg,
                timeout_ms,
                None,
                &scru128::new().to_string(),
                MODEL_LIST_MAX_RETRIES,
            )?;
            provider
                .list_models()
                .await
                .map(|items| items.into_iter().map(|item| item.id).collect::<Vec<_>>())
                .map_err(map_llm_error)?
        };
        models.sort();
        models.dedup();
        Ok(models)
    }

    pub fn list_models(cfg: &ModelEndpoint) -> Result<Vec<String>> {
        if cfg.api_key.trim().is_empty() && !cfg.protocol.uses_oauth() {
            return Err(anyhow!("API_AUTH_TOKEN 不能为空，无法更新模型列表"));
        }

        let timeout_ms = cfg.timeout_ms;
        if cfg.protocol == ProviderProtocol::Codex {
            let runtime = TokioRuntimeBuilder::new_current_thread()
                .enable_all()
                .build()
                .context("初始化异步运行时失败")?;
            return runtime.block_on(Self::list_models_async(cfg));
        }
        if cfg.protocol == ProviderProtocol::Anthropic {
            let provider = build_anthropic_provider_from_config(
                cfg,
                timeout_ms,
                None,
                &scru128::new().to_string(),
                MODEL_LIST_MAX_RETRIES,
            )?;
            let runtime = TokioRuntimeBuilder::new_current_thread()
                .enable_all()
                .build()
                .context("初始化异步运行时失败")?;
            let mut models = runtime
                .block_on(provider.list_models())
                .map(|items| items.into_iter().map(|item| item.id).collect::<Vec<_>>())
                .map_err(map_llm_error)?;
            models.sort();
            models.dedup();
            return Ok(models);
        }
        if cfg.protocol == ProviderProtocol::DeepSeek {
            let provider = build_deepseek_provider_from_config(
                cfg,
                timeout_ms,
                None,
                &scru128::new().to_string(),
                MODEL_LIST_MAX_RETRIES,
            )?;
            let runtime = TokioRuntimeBuilder::new_current_thread()
                .enable_all()
                .build()
                .context("初始化异步运行时失败")?;
            let mut models = runtime
                .block_on(provider.list_models())
                .map(|items| items.into_iter().map(|item| item.id).collect::<Vec<_>>())
                .map_err(map_llm_error)?;
            models.sort();
            models.dedup();
            return Ok(models);
        }
        let provider = build_openai_provider_from_config(
            cfg,
            timeout_ms,
            None,
            &scru128::new().to_string(),
            MODEL_LIST_MAX_RETRIES,
        )?;
        let runtime = TokioRuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .context("初始化异步运行时失败")?;
        let mut models = runtime
            .block_on(provider.list_models())
            .map(|items| items.into_iter().map(|item| item.id).collect::<Vec<_>>())
            .map_err(map_llm_error)?;
        models.sort();
        models.dedup();
        Ok(models)
    }

    pub fn protocol(&self) -> ProviderProtocol {
        self.cfg.protocol
    }

    pub(crate) fn build_provider_dispatch(
        &self,
        timeout_ms: u64,
        session_id: Option<&str>,
        max_retries: u32,
    ) -> Result<ProviderHandle> {
        let session_id = session_id.unwrap_or(&self.session_id);
        let key = (timeout_ms, session_id.to_owned(), max_retries);
        let mut providers = self
            .providers
            .lock()
            .map_err(|_| anyhow!("模型客户端缓存锁不可用"))?;
        if let Some(provider) = providers.get(&key) {
            return Ok(provider.clone());
        }
        let provider: ProviderHandle = match self.protocol() {
            ProviderProtocol::Anthropic => Arc::new(build_anthropic_provider_from_config(
                &self.cfg,
                timeout_ms,
                self.on_retry.clone(),
                session_id,
                max_retries,
            )?),
            ProviderProtocol::OpenAi => Arc::new(build_openai_responses_provider_from_config(
                &self.cfg,
                timeout_ms,
                self.on_retry.clone(),
                session_id,
                max_retries,
            )?),
            ProviderProtocol::Codex => Arc::new(build_codex_provider_from_config(
                &self.cfg, timeout_ms, session_id,
            )?),
            ProviderProtocol::OpenAiChatCompletions => Arc::new(build_openai_provider_from_config(
                &self.cfg,
                timeout_ms,
                self.on_retry.clone(),
                session_id,
                max_retries,
            )?),
            ProviderProtocol::DeepSeek => Arc::new(build_deepseek_provider_from_config(
                &self.cfg,
                timeout_ms,
                self.on_retry.clone(),
                session_id,
                max_retries,
            )?),
        };
        if providers.len() >= 16 {
            providers.clear();
        }
        providers.insert(key, provider.clone());
        Ok(provider)
    }

    #[cfg(test)]
    fn block_on_llm<F, T>(&self, future: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>> + Send,
        T: Send,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            return std::thread::scope(|scope| {
                let handle = scope.spawn(move || {
                    let runtime = TokioRuntimeBuilder::new_current_thread()
                        .enable_all()
                        .build()
                        .context("初始化异步运行时失败")?;
                    runtime.block_on(future)
                });
                handle.join().map_err(|_| anyhow!("LLM 请求线程 panic"))?
            });
        }

        let runtime = TokioRuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .context("初始化异步运行时失败")?;
        runtime.block_on(future)
    }

    #[cfg(test)]
    pub fn complete_lite(&self, prompt: &str) -> Result<String> {
        self.complete_lite_with_system("你是会话标题生成助手。根据用户输入生成简洁的标题，要求：1. 标题不超过10个汉字2. 直接返回标题，不要任何解释或额外文字3. 标题要概括性强，简洁明了", prompt)
    }
    #[cfg(test)]
    pub fn complete_lite_with_system(&self, system: &str, prompt: &str) -> Result<String> {
        let client = self.clone().with_options(crate::Options {
            reasoning_effort: ReasoningEffort::None,
            max_output_tokens: 200,
            temperature: Some(0.3),
            timeout_ms: Some(120_000),
            ..Default::default()
        });
        self.block_on_llm(async {
            client
                .stream(vec![
                    tiangong_types::Message::new(tiangong_types::MessageRole::System, system),
                    tiangong_types::Message::new(tiangong_types::MessageRole::User, prompt),
                ])?
                .collect_text()
                .await
        })
    }
}
impl ModelClient for SingleProviderClient {
    fn api_base_url(&self) -> &str {
        &self.cfg.base_url
    }
    fn api_timeout_ms(&self) -> u64 {
        self.cfg.timeout_ms
    }
    fn api_model(&self) -> &str {
        &self.cfg.model
    }
    #[cfg(test)]
    fn complete_stream(
        &self,
        req: ModelRequest,
        on_delta: &mut dyn FnMut(&ModelStreamChunk),
    ) -> Result<ModelResponse> {
        let (provider, request) = self.prepare_request(req)?;
        block_on_provider_stream(provider, request, on_delta, self.on_retry.clone())
    }
    #[cfg(test)]
    fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        self.block_on_llm(self.collect_async(req))
    }
}
/// 主模型请求最大输出 token 数。
///
/// 规划/执行/响应等主模型请求统一使用此值。
/// thinking 模式下 thinking tokens 与 output tokens 共享此预算，
/// 因此 32k 确保 thinking 消耗 15-20k 后仍有 12-17k 给实际输出。
/// 此值从 tiangong-core 到 tiangong-llm 再到 provider 逐层透传，不做任何修改。
#[cfg(test)]
const MAX_TOKENS_MAIN: u32 = 32_768;

fn build_anthropic_provider_from_config(
    cfg: &ModelEndpoint,
    timeout_ms: u64,
    on_retry: Option<OnRetryCallback>,
    session_id: &str,
    max_retries: u32,
) -> Result<AnthropicProvider> {
    let token = cfg.api_key.trim();
    if token.is_empty() {
        return Err(anyhow!("API_AUTH_TOKEN 不能为空，无法发起 Anthropic 请求"));
    }

    let mut config = AnthropicConfig::new(token.to_string());
    config.base_url = (!cfg.base_url.trim().is_empty()).then(|| cfg.base_url.clone());
    config.timeout = Duration::from_millis(timeout_ms);
    config.max_retries = max_retries;
    config.retry_notifier = on_retry;
    config.headers = crate::headers::resolve_headers(&cfg.headers, session_id)?;
    AnthropicProvider::from_config(config).map_err(map_llm_error)
}

fn build_openai_responses_provider_from_config(
    cfg: &ModelEndpoint,
    timeout_ms: u64,
    on_retry: Option<OnRetryCallback>,
    session_id: &str,
    max_retries: u32,
) -> Result<OpenAiResponsesProvider> {
    let token = cfg.api_key.trim();
    if token.is_empty() {
        return Err(anyhow!(
            "API_AUTH_TOKEN 不能为空，无法发起 OpenAI Responses 请求"
        ));
    }
    let mut config = OpenAiResponsesConfig::new(token.to_string(), cfg.base_url.clone());
    config.timeout = Duration::from_millis(timeout_ms);
    config.max_retries = max_retries;
    config.retry_notifier = on_retry;
    config.headers = crate::headers::resolve_headers(&cfg.headers, session_id)?;
    Ok(OpenAiResponsesProvider::new(config))
}

/// Codex（ChatGPT 账号）：鉴权来自 OAuth 登录态，无需 api_key。
fn build_codex_provider_from_config(
    cfg: &ModelEndpoint,
    timeout_ms: u64,
    session_id: &str,
) -> Result<CodexProvider> {
    // 重试额度为代码常量（打开阶段 WS_MAX_ATTEMPTS / 流间由消费层统一），
    // 不再随 client 配置走。
    let mut config = CodexConfig::new(cfg.base_url.clone());
    config.timeout = Duration::from_millis(timeout_ms);
    config.headers = crate::headers::resolve_headers(&cfg.headers, session_id)?;
    Ok(CodexProvider::new(config))
}

fn build_openai_provider_from_config(
    cfg: &ModelEndpoint,
    timeout_ms: u64,
    on_retry: Option<OnRetryCallback>,
    session_id: &str,
    max_retries: u32,
) -> Result<OpenAiChatCompletionsProvider> {
    let token = cfg.api_key.trim();
    if token.is_empty() {
        return Err(anyhow!("API_AUTH_TOKEN 不能为空，无法发起 OpenAI 兼容请求"));
    }
    let mut config = OpenAiChatConfig::new(token.to_string(), cfg.base_url.clone());
    config.timeout = Duration::from_millis(timeout_ms);
    config.max_retries = max_retries;
    config.retry_notifier = on_retry;
    config.headers = crate::headers::resolve_headers(&cfg.headers, session_id)?;
    Ok(OpenAiChatCompletionsProvider::new(config))
}

fn build_deepseek_provider_from_config(
    cfg: &ModelEndpoint,
    timeout_ms: u64,
    on_retry: Option<OnRetryCallback>,
    session_id: &str,
    max_retries: u32,
) -> Result<DeepSeekProvider> {
    let token = cfg.api_key.trim();
    if token.is_empty() {
        return Err(anyhow!("API_AUTH_TOKEN 不能为空，无法发起 DeepSeek 请求"));
    }
    let mut config = DeepSeekConfig::new(token.to_string());
    config.base_url = if cfg.base_url.trim().is_empty() {
        None
    } else {
        Some(cfg.base_url.clone())
    };
    config.timeout = Duration::from_millis(timeout_ms);
    config.max_retries = max_retries;
    config.retry_notifier = on_retry;
    config.headers = crate::headers::resolve_headers(&cfg.headers, session_id)?;
    DeepSeekProvider::from_config(config).map_err(|err| anyhow!("{err}"))
}

fn sanitize_provider_messages(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut filtered = Vec::new();
    let mut seen_user = false;

    for message in messages {
        if message.role == LlmMessageRole::System || is_empty_provider_message(&message) {
            continue;
        }
        if !seen_user {
            if message.role != LlmMessageRole::User {
                continue;
            }
            seen_user = true;
        }
        filtered.push(message);
    }

    // 修复上下文截断、迟到结果和跨轮复用 ID 导致的配对不一致。
    sanitize_tool_call_pairing(&mut filtered);
    filtered
}

/// 确保消息序列中 tool_call 与 tool_result 一一配对。
///
/// 上下文压缩或截断可能在不安全的位置切断消息序列，导致：
/// - 头部出现孤立的 tool result（对应的 assistant tool_call 被截掉了）
/// - 尾部的 assistant 消息包含 tool_call 但没有后续的 tool result
///
/// Anthropic 等提供商会校验配对关系，不一致时直接拒绝请求。
fn sanitize_tool_call_pairing(messages: &mut Vec<ChatMessage>) {
    let input = std::mem::take(messages);
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0usize;

    while index < input.len() {
        let mut assistant = input[index].clone();
        let has_tool_calls = assistant.role == LlmMessageRole::Assistant
            && assistant
                .content
                .iter()
                .any(|content| matches!(content, LlmMessageContent::ToolCall(_)));

        if !has_tool_calls {
            if assistant.role != LlmMessageRole::Tool {
                push_provider_message(&mut output, assistant);
            }
            index += 1;
            continue;
        }

        let mut call_order = Vec::new();
        let mut unique_call_ids = std::collections::HashSet::new();
        for content in &assistant.content {
            if let LlmMessageContent::ToolCall(call) = content
                && !call.id.is_empty()
                && unique_call_ids.insert(call.id.clone())
            {
                call_order.push(call.id.clone());
            }
        }

        // 一个调用批次只消费紧随其后的 Tool 消息；任何非 Tool 消息都封闭批次。
        let mut cursor = index + 1;
        let mut results = std::collections::HashMap::<String, LlmToolResult>::new();
        while let Some(message) = input
            .get(cursor)
            .filter(|message| message.role == LlmMessageRole::Tool)
        {
            for content in &message.content {
                if let LlmMessageContent::ToolResult(result) = content
                    && unique_call_ids.contains(&result.tool_call_id)
                {
                    results
                        .entry(result.tool_call_id.clone())
                        .or_insert_with(|| result.clone());
                }
            }
            cursor += 1;
        }

        let valid_ids = results
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let mut emitted_call_ids = std::collections::HashSet::new();
        assistant.content.retain(|content| match content {
            LlmMessageContent::ToolCall(call) => {
                !call.id.is_empty()
                    && valid_ids.contains(&call.id)
                    && emitted_call_ids.insert(call.id.clone())
            }
            _ => true,
        });
        if !assistant.content.is_empty() {
            push_provider_message(&mut output, assistant);
        }

        let paired_results = call_order
            .into_iter()
            .filter_map(|call_id| results.remove(&call_id))
            .map(LlmMessageContent::ToolResult)
            .collect::<Vec<_>>();
        if !paired_results.is_empty() {
            push_provider_message(
                &mut output,
                ChatMessage::new(LlmMessageRole::Tool, paired_results),
            );
        }
        index = cursor;
    }

    *messages = output;
}

#[cfg(test)]
fn provider_message_tool_result_count(message: &ChatMessage) -> usize {
    message
        .content
        .iter()
        .filter(|content| matches!(content, LlmMessageContent::ToolResult(_)))
        .count()
}

fn push_provider_message(messages: &mut Vec<ChatMessage>, message: ChatMessage) {
    if message.content.is_empty() {
        return;
    }
    // 保留已发送消息的边界；合并连续 User/Assistant 会改写下一请求的历史前缀。
    messages.push(message);
}

fn is_empty_provider_message(message: &ChatMessage) -> bool {
    message.content.iter().all(|content| match content {
        LlmMessageContent::Text(text) => text.trim().is_empty(),
        _ => false,
    })
}

fn convert_provider_response(response: ProviderResponse) -> ModelResponse {
    ModelResponse {
        provider_usage: response.usage.clone(),
        output: response.assistant_message.content.clone(),
        status: crate::collect::CompletionStatus::Complete,
        text: collect_provider_text(&response).trim().to_string(),
        reasoning_content: response.reasoning_content.clone().unwrap_or_default(),
        reasoning_signature: collect_provider_reasoning_signature(&response),
        stop_reason: response.stop_reason,
        usage: response.usage.unwrap_or_default().into(),
        tool_calls: response
            .assistant_message
            .content
            .iter()
            .filter_map(|content| match content {
                LlmMessageContent::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect(),
        invalid_tool_calls: Vec::new(),
        // 非流式响应没有增量事件，无从计量 decode/TTFT（不把整体耗时
        // 冒充输出阶段耗时）。
        output_elapsed_ms: None,
        reasoning_elapsed_ms: None,
        ttft_ms: None,
    }
}

#[cfg(test)]
fn parse_tool_arguments_or_error(tool_name: &str, call_id: &str, raw_args: &str) -> Value {
    if raw_args.trim().is_empty() {
        // 空参数按无参调用处理（与 tiangong_llm::tool 同语义）：无参工具
        // 的合法调用形态，不构成解析错误。
        return json!({});
    }

    serde_json::from_str(raw_args).unwrap_or_else(|err| {
        let raw_preview: String = raw_args.chars().take(512).collect();
        json!({
            "__parse_error": format!(
                "工具参数 JSON 无效：tool={tool_name} id={call_id} error={err}。\
        请重新生成完整 JSON 参数后再调用工具，不要把 __parse_error 当作真实参数。\
        长内容写入请分段调用 write_file，第一次 append=false，后续 append=true。"
            ),
            "__raw_args_preview": raw_preview,
        })
    })
}

fn collect_provider_text(response: &ProviderResponse) -> String {
    response
        .assistant_message
        .content
        .iter()
        .filter_map(|content| match content {
            LlmMessageContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn collect_provider_reasoning_signature(response: &ProviderResponse) -> Option<String> {
    response
        .assistant_message
        .content
        .iter()
        .find_map(|content| {
            if let LlmMessageContent::Thinking(thinking) = content {
                thinking
                    .signature
                    .as_ref()
                    .filter(|signature| !signature.trim().is_empty())
                    .cloned()
            } else {
                None
            }
        })
}

/// 流间统一重试：流中断且无可见产出、错误可重试时，重发请求的最大总尝试次数。
const MAX_STREAM_RETRY_ATTEMPTS: usize = 10;
/// 流间重试退避基数：第 n 次重试前等待 min(基数 × n, 上限)。
const STREAM_RETRY_BACKOFF_BASE: Duration = Duration::from_millis(600);
/// 流间重试退避上限。
const STREAM_RETRY_BACKOFF_CAP: Duration = Duration::from_secs(10);

fn stream_retry_backoff(failed_attempts: usize) -> Duration {
    std::cmp::min(
        STREAM_RETRY_BACKOFF_BASE * failed_attempts as u32,
        STREAM_RETRY_BACKOFF_CAP,
    )
}

/// 流中断原因（统一重试判定用）。
enum StreamInterrupt {
    /// 底层流 Err 项（传输/超时/provider 映射错误等原始错误）。
    Error(crate::error::LlmError),
    /// 流内 Error 事件的错误文案（provider 自报）。
    Message(String),
}

impl StreamInterrupt {
    fn display(&self) -> String {
        match self {
            StreamInterrupt::Error(err) => err.to_string(),
            StreamInterrupt::Message(message) => message.clone(),
        }
    }

    /// 是否值得统一层重发：传输/超时/限流类直接重试；provider 错误按瞬时
    /// 签名（降载 / server_error / 显式重试提示）判定；鉴权、请求非法等
    /// 重试无意义。
    fn retryable(&self) -> bool {
        match self {
            StreamInterrupt::Error(err) => match err {
                crate::error::LlmError::Transport(_)
                | crate::error::LlmError::Timeout(_)
                | crate::error::LlmError::Stream(_)
                | crate::error::LlmError::RateLimited(_) => true,
                crate::error::LlmError::Provider { message, .. } => {
                    crate::stream::retryable_stream_error_message(message)
                }
                _ => false,
            },
            StreamInterrupt::Message(message) => {
                crate::stream::retryable_stream_error_message(message)
            }
        }
    }
}

/// 流消费结果：响应 + 中断信息（供统一重试判定）。
struct StreamOutcome {
    response: ModelResponse,
    /// 流因错误提前结束（Err 项或流内 Error 事件）时的错误信息。
    interrupt: Option<StreamInterrupt>,
    /// 已产出可见内容（正文/工具调用）：true 时重发会重复产出，不可重试。
    visible_output: bool,
}

/// 消费 provider 事件流；中断且无可见产出、错误可重试时统一重发请求。
///
/// 各 provider 的建立阶段重试在各自 client 内完成；本层覆盖「流已建立、
/// 中途死于瞬时错误」的场景（如上游受理数秒后才降载），对所有 provider 一致：
/// 仅在尚无可见产出（正文/工具调用）时重发，绝不重复已产出的内容；思考增量
/// 不算可见产出，重发后会重新流出。总额度 [`MAX_STREAM_RETRY_ATTEMPTS`]。
async fn consume_stream_with_retry<F: FnMut(&ModelStreamChunk) + ?Sized>(
    provider: &ProviderHandle,
    request: &ProviderRequest,
    on_delta: &mut F,
    preserve_tool_call_order: bool,
    on_retry: Option<OnRetryCallback>,
) -> Result<ModelResponse> {
    // TTFT 锚点：紧贴首次 HTTP 发起（provider.stream 内部）之前；重试轮次
    // 共用同一锚点，TTFT 反映端到端等待。
    crate::retry::with_budget(MAX_STREAM_RETRY_ATTEMPTS, async {
        let request_started_at = std::time::Instant::now();
        let mut attempt = 1usize;
        loop {
            let stream = provider
                .stream(request.clone())
                .await
                .map_err(map_llm_error)?;
            let outcome = consume_provider_stream_events_async(
                stream,
                on_delta,
                preserve_tool_call_order,
                request_started_at,
            )
            .await;
            let should_retry = attempt < MAX_STREAM_RETRY_ATTEMPTS
                && !outcome.visible_output
                && outcome
                    .interrupt
                    .as_ref()
                    .is_some_and(StreamInterrupt::retryable);
            if !should_retry {
                return Ok(outcome.response);
            }
            let detail = outcome
                .interrupt
                .as_ref()
                .map(StreamInterrupt::display)
                .unwrap_or_default();
            let backoff = stream_retry_backoff(attempt);
            tracing::warn!(
                attempt = attempt + 1,
                max_attempts = MAX_STREAM_RETRY_ATTEMPTS,
                backoff_ms = backoff.as_millis() as u64,
                error = %detail,
                "流中断且无可见产出，统一层重发请求"
            );
            if let Some(notify) = &on_retry {
                notify(
                    (attempt + 1) as u32,
                    MAX_STREAM_RETRY_ATTEMPTS as u32,
                    backoff.as_millis() as u64,
                    &detail,
                );
            }
            tokio::time::sleep(backoff).await;
            attempt += 1;
        }
    })
    .await
}

/// 消费流事件并组装响应。
///
/// 中断时保留 partial 与显式 status；参数不完整的调用移入结构化无效列表。
/// Core 决定恢复/执行，collector 不注入 __parse_error 参数或修改会话。
async fn consume_provider_stream_events_async<F: FnMut(&ModelStreamChunk) + ?Sized>(
    mut stream: ProviderStream,
    on_delta: &mut F,
    preserve_tool_call_order: bool,
    // TTFT 锚点：HTTP 请求发出前的时刻（由调用方在发起请求处记录）。
    request_started_at: std::time::Instant,
) -> StreamOutcome {
    let mut state = crate::collect::ResponseAccumulator::default();
    let mut interrupt = None;
    let mut visible_output = false;
    let mut output_delta_events = 0u64;
    let mut first = None;
    let mut last = None;
    let mut reasoning_first = None;
    let mut reasoning_last = None;
    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                state.error = Some(error.clone());
                interrupt = Some(StreamInterrupt::Error(error));
                break;
            }
        };
        let now = std::time::Instant::now();
        let mut chunk = ModelStreamChunk::default();
        match &event {
            ProviderStreamEvent::TextDelta(delta) | ProviderStreamEvent::ReasoningDelta(delta)
                if !delta.is_empty() =>
            {
                first.get_or_insert(now);
                last = Some(now);
                output_delta_events += 1;
                if matches!(event, ProviderStreamEvent::TextDelta(_)) {
                    chunk.content = delta.clone();
                    visible_output = true;
                } else {
                    chunk.reasoning_content = delta.clone();
                    reasoning_first.get_or_insert(now);
                    reasoning_last = Some(now);
                }
            }
            ProviderStreamEvent::ToolCallMetadata { .. }
            | ProviderStreamEvent::ToolArgumentsDelta { .. }
            | ProviderStreamEvent::ToolArgumentsSnapshot { .. } => {
                visible_output = true;
                first.get_or_insert(now);
                last = Some(now);
            }
            ProviderStreamEvent::Error(message) => {
                interrupt = Some(StreamInterrupt::Message(message.clone()))
            }
            _ => {}
        }
        let is_usage = matches!(event, ProviderStreamEvent::Usage(_));
        state.push(event);
        if is_usage {
            chunk.usage = state.usage.clone();
            if let Some(usage) = &chunk.usage
                && usage.completion_tokens > 0
            {
                output_delta_events = usage.completion_tokens as u64;
            }
        }
        if !chunk.content.is_empty() || !chunk.reasoning_content.is_empty() || chunk.usage.is_some()
        {
            chunk.cumulative_tokens = Some(output_delta_events);
            on_delta(&chunk);
        }
        if state.error.is_some() {
            break;
        }
    }
    let collected = state.finish();
    let mut response = convert_provider_response(collected.response);
    response.status = collected.status;
    response.invalid_tool_calls = collected
        .invalid_tool_calls
        .into_iter()
        .map(|call| InvalidToolCall {
            id: call.id,
            name: call.name,
            arguments: Value::String(call.arguments),
            reason: call.reason,
        })
        .collect();
    response.output_elapsed_ms = first
        .zip(last)
        .map(|(a, b)| b.duration_since(a).as_millis() as u64);
    response.reasoning_elapsed_ms = reasoning_first
        .zip(reasoning_last)
        .map(|(a, b)| b.duration_since(a).as_millis() as u64);
    response.ttft_ms = first.map(|a| a.duration_since(request_started_at).as_millis() as u64);
    let _ = preserve_tool_call_order; // 所有协议均按响应内调用键稳定排序。
    StreamOutcome {
        response,
        interrupt,
        visible_output,
    }
}

pub(crate) type ProviderHandle = Arc<dyn LlmProvider>;
#[cfg(test)]
fn block_on_provider_stream(
    provider: ProviderHandle,
    request: ProviderRequest,
    on_delta: &mut dyn FnMut(&ModelStreamChunk),
    on_retry: Option<OnRetryCallback>,
) -> Result<ModelResponse> {
    async fn run_stream(
        provider: ProviderHandle,
        request: ProviderRequest,
        on_delta: &mut dyn FnMut(&ModelStreamChunk),
        on_retry: Option<OnRetryCallback>,
    ) -> Result<ModelResponse> {
        let preserve_tool_call_order = true;
        consume_stream_with_retry(
            &provider,
            &request,
            on_delta,
            preserve_tool_call_order,
            on_retry,
        )
        .await
    }

    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| {
                handle.block_on(run_stream(provider, request, on_delta, on_retry.clone()))
            })
        }
        Ok(_) => {
            let (response, chunks) = std::thread::scope(|scope| {
                scope
                    .spawn(move || {
                        let runtime = TokioRuntimeBuilder::new_current_thread()
                            .enable_all()
                            .build()
                            .context("初始化异步运行时失败")?;
                        let mut chunks = Vec::new();
                        let response = runtime.block_on(run_stream(
                            provider,
                            request,
                            &mut |chunk| chunks.push(chunk.clone()),
                            on_retry.clone(),
                        ));
                        Ok::<_, anyhow::Error>((response, chunks))
                    })
                    .join()
                    .map_err(|_| anyhow!("LLM 流式请求线程 panic"))?
            })?;
            for chunk in chunks {
                on_delta(&chunk);
            }
            response
        }
        Err(_) => {
            let runtime = TokioRuntimeBuilder::new_current_thread()
                .enable_all()
                .build()
                .context("初始化异步运行时失败")?;
            runtime.block_on(run_stream(provider, request, on_delta, on_retry))
        }
    }
}

fn map_llm_error(error: crate::error::LlmError) -> anyhow::Error {
    anyhow::Error::new(error)
}

// ── 重试相关 ──────────────────────────────────────────────

/// 模型列表查询的独立重试上限，不用于生成请求。
const MODEL_LIST_MAX_RETRIES: u32 = 3;

fn configured_temperature_number() -> Option<serde_json::Number> {
    let raw = std::env::var("API_TEMPERATURE").unwrap_or_else(|_| "0.2".to_string());
    let value = raw.trim().parse::<f64>().ok()?;
    if !(0.0..=2.0).contains(&value) {
        return None;
    }
    let rounded = (value * 100.0).round() / 100.0;
    serde_json::Number::from_f64(rounded)
}

fn configured_temperature_f32() -> Option<f32> {
    configured_temperature_number()
        .and_then(|value| value.as_f64())
        .map(|value| value as f32)
}

fn function_timeout_ms(base_timeout_ms: u64) -> u64 {
    let custom_timeout_ms = std::env::var("API_FUNCTION_TIMEOUT_MS")
        .ok()
        .and_then(|custom| custom.trim().parse::<u64>().ok());
    resolve_function_timeout_ms(base_timeout_ms, custom_timeout_ms)
}

fn resolve_function_timeout_ms(base_timeout_ms: u64, custom_timeout_ms: Option<u64>) -> u64 {
    custom_timeout_ms
        .filter(|timeout_ms| *timeout_ms > 0)
        .unwrap_or(base_timeout_ms)
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_usage_preserves_cache_details_across_partial_and_final_events() {
        let mut current = TokenUsageData::default();
        merge_stream_usage(
            &mut current,
            TokenUsageData {
                prompt_tokens: 4000,
                completion_tokens: 0,
                total_tokens: 4000,
                prompt_cache_hit_tokens: Some(3072),
                prompt_cache_miss_tokens: Some(928),
            },
        );
        merge_stream_usage(
            &mut current,
            TokenUsageData {
                prompt_tokens: 0,
                completion_tokens: 20,
                total_tokens: 4020,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
            },
        );
        assert_eq!(current.prompt_cache_hit_tokens, Some(3072));
        assert_eq!(current.prompt_cache_miss_tokens, Some(928));
        assert_eq!(current.total_tokens, 4020);
        // 明确的零命中必须保留，不能当成缺失，也不能跨事件累加缓存量。
        merge_stream_usage(
            &mut current,
            TokenUsageData {
                prompt_tokens: 4000,
                completion_tokens: 20,
                total_tokens: 4020,
                prompt_cache_hit_tokens: Some(0),
                prompt_cache_miss_tokens: Some(4000),
            },
        );
        assert_eq!(current.prompt_cache_hit_tokens, Some(0));
        assert_eq!(current.prompt_cache_miss_tokens, Some(4000));
        let mut absent = TokenUsageData::default();
        merge_stream_usage(&mut absent, TokenUsageData::default());
        assert_eq!(absent.prompt_cache_hit_tokens, None);
        assert_eq!(absent.prompt_cache_miss_tokens, None);
    }
    use crate::tool::{ToolCall as LlmToolCall, ToolResultContent as LlmToolResultContent};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn function_timeout_uses_provider_timeout_without_override() {
        assert_eq!(resolve_function_timeout_ms(300_000, None), 300_000);
        assert_eq!(resolve_function_timeout_ms(60_000, None), 60_000);
    }

    #[test]
    fn function_timeout_uses_only_positive_override() {
        assert_eq!(resolve_function_timeout_ms(300_000, Some(180_000)), 180_000);
        assert_eq!(resolve_function_timeout_ms(300_000, Some(0)), 300_000);
    }

    fn provider_tool_call(id: &str) -> LlmMessageContent {
        LlmMessageContent::ToolCall(LlmToolCall {
            id: id.to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": id}),
        })
    }

    fn provider_tool_result(id: &str) -> LlmMessageContent {
        LlmMessageContent::ToolResult(LlmToolResult {
            tool_call_id: id.to_string(),
            content: LlmToolResultContent::Text(format!("result-{id}")),
            is_error: false,
        })
    }

    #[test]
    fn empty_tool_arguments_become_empty_object() {
        let arguments = parse_tool_arguments_or_error("run_shell", "call_empty", "");
        assert_eq!(
            arguments,
            serde_json::json!({}),
            "空参数应按无参调用处理（空对象），不再构成解析错误"
        );
    }

    fn schema_tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: format!("测试工具 {name}"),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    #[test]
    fn incomplete_nested_json_keeps_accepting_stream_deltas() {
        let mut raw_args =
            r#"{"allowed_paths":[],"verification":[{"name":"cargo check","passed":true}"#
                .to_string();

        append_stream_tool_call_arguments(&mut raw_args, "]}");

        assert_eq!(
            serde_json::from_str::<Value>(&raw_args).unwrap(),
            json!({
                "allowed_paths": [],
                "verification": [{"name": "cargo check", "passed": true}]
            })
        );
    }

    #[test]
    fn complete_stream_arguments_ignore_repeated_delta() {
        let mut raw_args = r#"{"path":"TODO.md"}"#.to_string();

        append_stream_tool_call_arguments(&mut raw_args, r#"{"path":"TODO.md"}"#);

        assert_eq!(raw_args, r#"{"path":"TODO.md"}"#);
    }

    /// 把固定事件序列合成 ProviderStream，模拟传输中断（issue #551）。
    fn synthetic_stream(
        events: Vec<Result<ProviderStreamEvent, crate::error::LlmError>>,
    ) -> ProviderStream {
        Box::pin(futures_util::stream::iter(events))
    }

    /// 按时间表合成 ProviderStream：每个事件 yield 前等待指定时长，
    /// 用于测试 decode 耗时的真实测量。
    fn paced_stream(schedule: Vec<(Duration, ProviderStreamEvent)>) -> ProviderStream {
        Box::pin(futures_util::stream::unfold(
            schedule
                .into_iter()
                .collect::<std::collections::VecDeque<_>>(),
            |mut queue| async move {
                let (delay, event) = queue.pop_front()?;
                tokio::time::sleep(delay).await;
                Some((Ok::<_, crate::error::LlmError>(event), queue))
            },
        ))
    }

    /// 输出耗时 = 首末输出增量的实测间隔（真实测量，无任何判别）：TTFT
    /// 300ms 不计入；三个增量（正文/正文/工具调用）各隔 50ms → elapsed
    /// ≈ 100ms。
    #[tokio::test]
    async fn paced_stream_measures_real_decode_span() {
        let schedule = vec![
            (
                Duration::from_millis(300),
                ProviderStreamEvent::TextDelta("开".to_string()),
            ),
            (
                Duration::from_millis(50),
                ProviderStreamEvent::TextDelta("a".to_string()),
            ),
            (
                Duration::from_millis(50),
                ProviderStreamEvent::ToolCallMetadata {
                    key: crate::CallKey(0),
                    id: "c1".to_string(),
                    name: "get_weather".to_string(),
                },
            ),
            (
                Duration::ZERO,
                ProviderStreamEvent::Usage(TokenUsageData::new(10, 3)),
            ),
        ];
        let outcome = consume_provider_stream_events_async(
            paced_stream(schedule),
            &mut |_| {},
            false,
            std::time::Instant::now(),
        )
        .await;
        let response = outcome.response;
        let elapsed = response.output_elapsed_ms.expect("应有输出耗时");
        assert!(
            (90..=150).contains(&elapsed),
            "elapsed 应为首末增量实测间隔（≈100ms），实际 {elapsed}ms"
        );
        assert!(response.ttft_ms.is_some_and(|ttft| ttft >= 280));
    }

    #[tokio::test]
    async fn stream_interrupt_finalizes_produced_content() {
        let events = vec![
            Ok(ProviderStreamEvent::TextDelta("部分输出".to_string())),
            // 完整调用：Start 即携带全部参数。
            Ok(ProviderStreamEvent::ToolCallMetadata {
                key: crate::CallKey(0),
                id: "call_complete".into(),
                name: "read_file".into(),
            }),
            // 不完整调用：参数 JSON 只流出一半即断流。
            Ok(ProviderStreamEvent::ToolCallMetadata {
                key: crate::CallKey(1),
                id: "call_partial".into(),
                name: "write_file".into(),
            }),
            Ok(ProviderStreamEvent::ToolArgumentsDelta {
                key: crate::CallKey(1),
                fragment: r#"{"path":"b""#.to_string(),
            }),
            Err(crate::error::LlmError::Transport(
                "error decoding response body".to_string(),
            )),
        ];

        let outcome = consume_provider_stream_events_async(
            synthetic_stream(events),
            &mut |_| {},
            false,
            std::time::Instant::now(),
        )
        .await;

        let response = outcome.response;
        // 断流时未收到终止事件，stop_reason 保持缺失。
        assert_eq!(response.stop_reason, None);
        assert_eq!(response.text, "部分输出");
        assert!(
            response.tool_calls.is_empty(),
            "未明确结束的调用不应在中断后执行"
        );
        assert_eq!(response.invalid_tool_calls.len(), 2);
    }

    #[tokio::test]
    async fn stream_interrupt_before_output_yields_empty_response() {
        // 开头即断：定稿为空响应，由上层按无效输出处理。
        let events = vec![
            Ok(ProviderStreamEvent::MessageStart),
            Ok(ProviderStreamEvent::Usage(TokenUsageData::new(100, 0))),
            Err(crate::error::LlmError::Transport(
                "error decoding response body".to_string(),
            )),
        ];

        let outcome = consume_provider_stream_events_async(
            synthetic_stream(events),
            &mut |_| {},
            false,
            std::time::Instant::now(),
        )
        .await;

        let response = outcome.response;
        assert_eq!(response.text, "");
        assert!(response.tool_calls.is_empty());
        // 中断详情供统一重试层判定：传输类错误且无可见产出 → 可重试。
        let interrupt = outcome.interrupt.expect("开头即断应带中断信息");
        assert!(!outcome.visible_output);
        assert!(interrupt.retryable());
    }

    #[tokio::test]
    async fn in_stream_error_event_finalizes_produced_content() {
        // provider 在流内报告错误事件（如 overloaded）同样按中断收尾。
        let events = vec![
            Ok(ProviderStreamEvent::TextDelta("写到一半".to_string())),
            Ok(ProviderStreamEvent::Error("overloaded_error".to_string())),
        ];

        let outcome = consume_provider_stream_events_async(
            synthetic_stream(events),
            &mut |_| {},
            false,
            std::time::Instant::now(),
        )
        .await;

        let response = outcome.response;
        assert_eq!(response.text, "写到一半");
        assert_eq!(response.stop_reason, None);
    }

    #[test]
    fn stream_interrupt_retryable_classification() {
        use crate::error::LlmError;
        let transport = StreamInterrupt::Error(LlmError::Transport("broken pipe".into()));
        assert!(transport.retryable());
        let timeout = StreamInterrupt::Error(LlmError::Timeout(1000));
        assert!(timeout.retryable());
        let rate_limited = StreamInterrupt::Error(LlmError::RateLimited("429".into()));
        assert!(rate_limited.retryable());
        let overloaded =
            StreamInterrupt::Message("[server_is_overloaded] Our servers are overloaded.".into());
        assert!(overloaded.retryable());
        let plain_overload = StreamInterrupt::Message("Overloaded, please retry".into());
        assert!(plain_overload.retryable());
        let provider_transient = StreamInterrupt::Error(LlmError::Provider {
            provider: "codex",
            message: "[server_error] You can retry your request.".into(),
        });
        assert!(provider_transient.retryable());
        let invalid = StreamInterrupt::Error(LlmError::InvalidRequest("bad".into()));
        assert!(!invalid.retryable());
        let fatal = StreamInterrupt::Message("[invalid_request_error] unsupported".into());
        assert!(!fatal.retryable());
        let boom = StreamInterrupt::Message("boom".into());
        assert!(!boom.retryable());
    }
    fn sse_body(chunks: &[Value]) -> Vec<u8> {
        let mut body = String::new();
        for chunk in chunks {
            body.push_str(&format!("data: {chunk}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        body.into_bytes()
    }

    async fn mount_openai_stream(server: &MockServer, chunks: Vec<Value>) {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(sse_body(&chunks), "text/event-stream"),
            )
            .up_to_n_times(1)
            .mount(server)
            .await;
    }

    fn usage_chunk(prompt_tokens: usize, completion_tokens: usize) -> Value {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test-model",
            "choices": [],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens
            }
        })
    }

    #[tokio::test]
    async fn openai_stream_sniffs_sse_body_when_content_type_mislabeled() {
        // 部分网关返回标准 SSE 数据但漏标/错标响应类型：不能一律当成一次性 JSON，
        // 应按首块内容探测后仍走流式解析，避免流式能力回退。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                concat!(
                    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"漏标类型的流式回复\"}}]}\n\n",
                    "data: {\"id\":\"c2\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n",
                ),
                "application/octet-stream",
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = SingleProviderClient::new(ModelEndpoint {
            headers: Default::default(),
            base_url: server.uri(),
            api_key: "test-key".to_string(),
            model: "test-model".to_string(),
            protocol: ProviderProtocol::OpenAiChatCompletions,
            timeout_ms: 5_000,
            options: serde_json::Value::Object(serde_json::Map::new()),
            context_window: None,
        });
        let request = ModelRequest {
            session_id: None,
            system: "system".to_string(),
            messages: vec![ChatMessage::text(LlmMessageRole::User, "检查项目")],
            reasoning_effort: ReasoningEffort::None,
            max_output_tokens: None,
            ..Default::default()
        };
        let functions: Vec<ToolSpec> = Vec::new();
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();

        let response = client
            .stream_async(request.with_tools(functions, None), chunk_tx)
            .await
            .unwrap();

        assert_eq!(response.text, "漏标类型的流式回复");
        let mut streamed_text = String::new();
        while let Ok(chunk) = chunk_rx.try_recv() {
            streamed_text.push_str(&chunk.content);
        }
        assert_eq!(streamed_text, "漏标类型的流式回复");
    }

    #[tokio::test]
    async fn stream_chunks_carry_cumulative_tokens_calibrated_by_usage() {
        // 实时 TPS 数据源端到端：正文 delta 事件逐批携带递增累计，
        // usage 到达时校准为官方 completion_tokens。
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"思\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"考\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"，世界\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":19,\"total_tokens\":26}}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"后\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = SingleProviderClient::new(ModelEndpoint {
            headers: Default::default(),
            base_url: server.uri(),
            api_key: "test-key".to_string(),
            model: "test-model".to_string(),
            protocol: ProviderProtocol::OpenAiChatCompletions,
            timeout_ms: 5_000,
            options: serde_json::Value::Object(serde_json::Map::new()),
            context_window: None,
        });
        let request = ModelRequest {
            session_id: None,
            system: "system".to_string(),
            messages: vec![ChatMessage::text(LlmMessageRole::User, "hi")],
            reasoning_effort: ReasoningEffort::None,
            max_output_tokens: None,
            ..Default::default()
        };
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();
        client.stream_async(request, chunk_tx).await.unwrap();

        let mut cumulatives = Vec::new();
        let mut reasoning_cumulatives = Vec::new();
        while let Ok(chunk) = chunk_rx.try_recv() {
            if !chunk.content.is_empty() {
                cumulatives.push(chunk.cumulative_tokens);
            }
            if !chunk.reasoning_content.is_empty() {
                reasoning_cumulatives.push(chunk.cumulative_tokens);
            }
        }
        // 两个思考 delta + 两个正文 delta：统一输出计数 1 → 2 → 3 → 4
        //（官方 completion_tokens 口径含思考 token）。
        assert_eq!(reasoning_cumulatives, vec![Some(1), Some(2)]);
        // usage（completion=19）校准后，下一个正文 delta 从官方值继续：20。
        assert_eq!(cumulatives, vec![Some(3), Some(4), Some(20)]);
    }

    #[tokio::test]
    async fn openai_stream_accepts_server_ignoring_stream_flag() {
        // 部分网关忽略 stream 参数，返回 application/json 的一次性完整响应；
        // llm 层应按完整结果接住并合成流事件，而不是让 SSE 解析器静默丢弃。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-complete",
                "object": "chat.completion",
                "created": 0,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "一次性完整回复",
                        "tool_calls": [{
                            "id": "call_tool",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\":\"TODO.md\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 3,
                    "completion_tokens": 5,
                    "total_tokens": 8
                }
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let client = SingleProviderClient::new(ModelEndpoint {
            headers: Default::default(),
            base_url: server.uri(),
            api_key: "test-key".to_string(),
            model: "test-model".to_string(),
            protocol: ProviderProtocol::OpenAiChatCompletions,
            timeout_ms: 5_000,
            options: serde_json::Value::Object(serde_json::Map::new()),
            context_window: None,
        });
        let request = ModelRequest {
            session_id: None,
            system: "system".to_string(),
            messages: vec![ChatMessage::text(LlmMessageRole::User, "检查项目")],
            reasoning_effort: ReasoningEffort::None,
            max_output_tokens: None,
            ..Default::default()
        };
        let functions = vec![schema_tool("read_file"), schema_tool("other_tool")];
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();

        let response = client
            .stream_async(request.with_tools(functions, None), chunk_tx)
            .await
            .unwrap();

        assert_eq!(response.text, "一次性完整回复");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_tool");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(
            response.tool_calls[0].arguments,
            serde_json::json!({"path": "TODO.md"})
        );
        assert_eq!(response.usage.total_tokens, 8);
        let mut streamed_text = String::new();
        while let Ok(chunk) = chunk_rx.try_recv() {
            streamed_text.push_str(&chunk.content);
        }
        assert_eq!(streamed_text, "一次性完整回复");
    }

    #[tokio::test]
    async fn openai_stream_filters_invalid_parallel_calls_without_hidden_retry() {
        let server = MockServer::start().await;
        mount_openai_stream(
            &server,
            vec![
                serde_json::json!({
                    "id": "chatcmpl-first",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_valid",
                                    "function": {
                                        "name": "read_file",
                                        "arguments": "{\"path\":\"TODO.md\"}"
                                    }
                                },
                                {
                                    "index": 1,
                                    "id": "call_invalid",
                                    "function": { "name": "read_file", "arguments": "{}" }
                                }
                            ]
                        },
                        "finish_reason": "tool_calls"
                    }]
                }),
                usage_chunk(10, 2),
            ],
        )
        .await;

        let client = SingleProviderClient::new(ModelEndpoint {
            headers: Default::default(),
            base_url: server.uri(),
            api_key: "test-key".to_string(),
            model: "test-model".to_string(),
            protocol: ProviderProtocol::OpenAiChatCompletions,
            timeout_ms: 5_000,
            options: Value::Object(serde_json::Map::new()),
            context_window: None,
        });
        let request = ModelRequest {
            session_id: None,
            system: "system".to_string(),
            messages: vec![ChatMessage::text(LlmMessageRole::User, "检查项目")],
            reasoning_effort: ReasoningEffort::None,
            max_output_tokens: None,
            ..Default::default()
        };
        let functions = vec![schema_tool("read_file"), schema_tool("other_tool")];
        let (chunk_tx, _chunk_rx) = tokio::sync::mpsc::unbounded_channel();

        let response = client
            .stream_async(request.with_tools(functions, None), chunk_tx)
            .await
            .unwrap();

        assert_eq!(response.usage.total_tokens, 12);
        assert_eq!(
            response.tool_calls.len(),
            2,
            "LLM 层只保证结构完整；schema 校验由 Core 执行"
        );
        assert_eq!(response.tool_calls[0].id, "call_valid");
        assert_eq!(response.tool_calls[1].id, "call_invalid");
        assert!(response.invalid_tool_calls.is_empty());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
    }

    /// 插在并行结果之间的 User 消息封闭调用批次：之后的结果成为孤立结果被丢弃，
    /// 未得到结果的调用从 assistant 中移除。
    #[test]
    fn user_message_between_parallel_results_closes_the_batch() {
        let sanitized = sanitize_provider_messages(vec![
            ChatMessage::text(LlmMessageRole::User, "开始"),
            ChatMessage::new(
                LlmMessageRole::Assistant,
                vec![provider_tool_call("a"), provider_tool_call("b")],
            ),
            ChatMessage::new(LlmMessageRole::Tool, vec![provider_tool_result("a")]),
            ChatMessage::text(LlmMessageRole::User, "插入的用户消息"),
            ChatMessage::new(LlmMessageRole::Tool, vec![provider_tool_result("b")]),
        ]);

        assert_eq!(
            sanitized,
            vec![
                ChatMessage::text(LlmMessageRole::User, "开始"),
                ChatMessage::new(LlmMessageRole::Assistant, vec![provider_tool_call("a")]),
                ChatMessage::new(LlmMessageRole::Tool, vec![provider_tool_result("a")]),
                ChatMessage::text(LlmMessageRole::User, "插入的用户消息"),
            ]
        );
    }

    #[test]
    fn sanitize_keeps_a_complete_parallel_tool_batch() {
        let messages = vec![
            ChatMessage::text(LlmMessageRole::User, "开始"),
            ChatMessage::new(
                LlmMessageRole::Assistant,
                vec![provider_tool_call("a"), provider_tool_call("b")],
            ),
            ChatMessage::new(
                LlmMessageRole::Tool,
                vec![provider_tool_result("a"), provider_tool_result("b")],
            ),
        ];

        assert_eq!(sanitize_provider_messages(messages.clone()), messages);
    }

    #[test]
    fn sanitize_does_not_pair_a_result_that_precedes_its_call() {
        let sanitized = sanitize_provider_messages(vec![
            ChatMessage::text(LlmMessageRole::User, "开始"),
            ChatMessage::new(LlmMessageRole::Tool, vec![provider_tool_result("same")]),
            ChatMessage::new(LlmMessageRole::Assistant, vec![provider_tool_call("same")]),
        ]);

        assert_eq!(sanitized.len(), 1);
        assert_eq!(sanitized[0].role, LlmMessageRole::User);
    }

    #[test]
    fn sanitize_does_not_reuse_an_old_result_for_a_new_call() {
        let sanitized = sanitize_provider_messages(vec![
            ChatMessage::text(LlmMessageRole::User, "第一轮"),
            ChatMessage::new(
                LlmMessageRole::Assistant,
                vec![provider_tool_call("reused")],
            ),
            ChatMessage::new(LlmMessageRole::Tool, vec![provider_tool_result("reused")]),
            ChatMessage::text(LlmMessageRole::User, "第二轮"),
            ChatMessage::new(
                LlmMessageRole::Assistant,
                vec![provider_tool_call("reused")],
            ),
        ]);

        assert_eq!(sanitized.len(), 4);
        assert_eq!(sanitized[1].role, LlmMessageRole::Assistant);
        assert_eq!(sanitized[2].role, LlmMessageRole::Tool);
        assert_eq!(sanitized[3].role, LlmMessageRole::User);
    }

    #[test]
    fn sanitize_keeps_only_the_completed_part_of_a_parallel_batch() {
        let sanitized = sanitize_provider_messages(vec![
            ChatMessage::text(LlmMessageRole::User, "开始"),
            ChatMessage::new(
                LlmMessageRole::Assistant,
                vec![
                    LlmMessageContent::Text("处理中".to_string()),
                    provider_tool_call("a"),
                    provider_tool_call("b"),
                ],
            ),
            ChatMessage::new(LlmMessageRole::Tool, vec![provider_tool_result("a")]),
            ChatMessage::text(LlmMessageRole::User, "继续"),
        ]);

        assert_eq!(sanitized.len(), 4);
        let calls = sanitized[1]
            .content
            .iter()
            .filter_map(|content| match content {
                LlmMessageContent::ToolCall(call) => Some(call.id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls, vec!["a"]);
        assert_eq!(provider_message_tool_result_count(&sanitized[2]), 1);
    }

    #[test]
    fn sanitize_deduplicates_calls_and_results_within_one_batch() {
        let sanitized = sanitize_provider_messages(vec![
            ChatMessage::text(LlmMessageRole::User, "开始"),
            ChatMessage::new(
                LlmMessageRole::Assistant,
                vec![
                    provider_tool_call("same"),
                    provider_tool_call("same"),
                    provider_tool_call(""),
                ],
            ),
            ChatMessage::new(
                LlmMessageRole::Tool,
                vec![
                    provider_tool_result("same"),
                    provider_tool_result("same"),
                    provider_tool_result("extra"),
                ],
            ),
        ]);

        assert_eq!(sanitized.len(), 3);
        assert_eq!(provider_message_tool_call_count_for_test(&sanitized[1]), 1);
        assert_eq!(provider_message_tool_result_count(&sanitized[2]), 1);
    }

    fn provider_message_tool_call_count_for_test(message: &ChatMessage) -> usize {
        message
            .content
            .iter()
            .filter(|content| matches!(content, LlmMessageContent::ToolCall(_)))
            .count()
    }
}
