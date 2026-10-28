//! 单 provider 客户端：把扁平的 [`ModelEndpoint`] 包装成同步/流式 LLM 调用入口。
//!
//! 这里聚合了：
//! - [`ModelClient`] trait、[`ModelRequest`] / [`ModelResponse`] / [`ModelStreamChunk`] 等数据类型
//! - [`SingleProviderClient`]：持有 [`ModelEndpoint`]，按 `protocol` 分发到 Anthropic / OpenAI / DeepSeek provider
//! - provider 构造辅助、协议约束修正（tool_call 配对）、流式消费、错误映射等私有逻辑
//!
//! 会话消息到 [`ChatMessage`] 的组装由调用方（core）负责，本模块不理解宿主
//! 消息格式，也不读取本地文件。
//!
//! 迁移自 `tiangong-core/src/model.rs`，已移除 `ModelProviderConfig` 依赖，直接消费
//! [`ModelEndpoint`]（`base_url` / `api_key` / `model` / `protocol` / `timeout_ms` / `options`）。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::endpoint::ModelEndpoint;
use crate::message::{
    ChatMessage, MessageContent as LlmMessageContent, MessageRole as LlmMessageRole,
};
use crate::model::ProviderProtocol;
use crate::provider::LlmProvider;
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

fn collect_openai_stream_tool_calls(
    mut calls: std::collections::BTreeMap<String, (String, String)>,
    order: Vec<String>,
) -> Vec<ToolCall> {
    let mut ordered_ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in order {
        if !id.is_empty() && seen.insert(id.clone()) {
            ordered_ids.push(id);
        }
    }
    for id in calls.keys() {
        if !id.is_empty() && seen.insert(id.clone()) {
            ordered_ids.push(id.clone());
        }
    }

    ordered_ids
        .into_iter()
        .filter_map(|id| {
            let (name, raw_args) = calls.remove(&id)?;
            if name.trim().is_empty() {
                return None;
            }
            let raw_args_preview: String = raw_args.chars().take(256).collect();
            tracing::info!(
                tool_call_id = %id,
                tool_name = %name,
                raw_args_len = raw_args.len(),
                %raw_args_preview,
                "解析 tool call arguments"
            );
            Some(ToolCall {
                arguments: parse_tool_arguments_or_error(&name, &id, &raw_args),
                id,
                name,
            })
        })
        .collect()
}

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
    pub text: String,
    pub reasoning_content: String,
    pub reasoning_signature: Option<String>,
    pub stop_reason: Option<StopReason>,
    pub usage: TokenUsage,
    pub tool_calls: Vec<ToolCall>,
    pub invalid_tool_calls: Vec<InvalidToolCall>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub reason: String,
}

fn filter_invalid_openai_tool_calls(
    protocol: ProviderProtocol,
    functions: &[ToolSpec],
    mut response: ModelResponse,
) -> Result<ModelResponse> {
    if !matches!(
        protocol,
        ProviderProtocol::OpenAi
            | ProviderProtocol::OpenAiChatCompletions
            | ProviderProtocol::Codex
    ) || response.tool_calls.is_empty()
    {
        return Ok(response);
    }

    let original_count = response.tool_calls.len();
    let mut valid_calls = Vec::with_capacity(original_count);
    for call in response.tool_calls {
        let validation_error =
            if let Some(message) = call.arguments.get("__parse_error").and_then(Value::as_str) {
                Some(message.to_string())
            } else {
                match functions.iter().find(|function| function.name == call.name) {
                    None => Some(format!("工具 {} 不在本次 tools 定义中", call.name)),
                    Some(spec) if spec.input_schema.is_null() => None,
                    Some(spec) => {
                        let validator = jsonschema::validator_for(&spec.input_schema)
                            .with_context(|| format!("工具 {} 的 input_schema 无效", spec.name))?;
                        let errors = validator
                        .iter_errors(&call.arguments)
                        .take(3)
                        .map(|error| {
                            let instance_path = if error.instance_path().as_str().is_empty() {
                                "$".to_string()
                            } else {
                                format!("${}", error.instance_path())
                            };
                            let schema_path = if error.schema_path().as_str().is_empty() {
                                "#".to_string()
                            } else {
                                format!("#{}", error.schema_path())
                            };
                            format!(
                                "参数位置={instance_path}，schema 位置={schema_path}，原因={error}"
                            )
                        })
                        .collect::<Vec<_>>();
                        (!errors.is_empty())
                            .then(|| format!("参数不符合 schema：{}", errors.join("；")))
                    }
                }
            };

        if let Some(reason) = validation_error {
            tracing::warn!(
                tool_call_id = %call.id,
                tool_name = %call.name,
                %reason,
                "剔除不符合本次 tools 定义的 OpenAI 工具调用"
            );
            let arguments = if call.arguments.get("__parse_error").is_some() {
                call.arguments
                    .get("__raw_args_preview")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new()))
            } else {
                call.arguments
            };
            response.invalid_tool_calls.push(InvalidToolCall {
                id: call.id,
                name: call.name,
                arguments,
                reason,
            });
        } else {
            valid_calls.push(call);
        }
    }
    response.tool_calls = valid_calls;

    let invalid_count = original_count - response.tool_calls.len();
    if invalid_count > 0 {
        tracing::warn!(
            invalid_count,
            valid_count = response.tool_calls.len(),
            "OpenAI 工具调用异常项已剔除"
        );
    }
    Ok(response)
}

#[derive(Debug, Clone, Default)]
pub struct ModelStreamChunk {
    pub content: String,
    pub reasoning_content: String,
    pub usage: Option<TokenUsageData>,
    /// 本批正文增量经真实 tokenizer 计数的 token 数（`token_count` 模块）。
    ///
    /// 供上层做流式实时速度（TPS）展示；跨批合并的未定界尾部由计数器
    /// 内部缓冲，流末 `finish` 补齐。思考增量不计入（正文口径）。
    pub tokens: Option<u64>,
}

pub trait ModelClient {
    fn api_base_url(&self) -> &str;
    fn api_timeout_ms(&self) -> u64;
    fn api_model(&self) -> &str;
    fn complete(&self, req: ModelRequest) -> Result<ModelResponse>;
    /// 流式调用，通过 on_delta 实时回调内容和用量。
    fn complete_stream(
        &self,
        req: ModelRequest,
        on_delta: &mut dyn FnMut(&ModelStreamChunk),
    ) -> Result<ModelResponse>;
}

/// 重试回调类型：(attempt, max_attempts, delay_ms, error_text)
pub type OnRetryCallback = Arc<dyn Fn(u32, u32, u64, &str) + Send + Sync>;

#[derive(Clone)]
pub struct SingleProviderClient {
    session_id: String,
    cfg: ModelEndpoint,
    /// 重试时的回调通知（可选）
    on_retry: Option<OnRetryCallback>,
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
    /// 按值消费请求：消息直接移入 provider 请求，不复制消息内容。
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

    /// 返回 provider、provider 请求与本次工具定义（响应侧校验工具调用用）。
    fn prepare_request(
        &self,
        req: ModelRequest,
    ) -> Result<(ProviderDispatch, ProviderRequest, Vec<ToolSpec>)> {
        let timeout_ms = req
            .timeout_ms
            .unwrap_or_else(|| function_timeout_ms(self.cfg.timeout_ms));
        let provider =
            self.build_provider_dispatch(timeout_ms, req.session_id.as_deref(), MAX_RETRIES)?;
        let request = self.build_provider_request(req)?;
        let tools = request.tools.clone();
        Ok((provider, request, tools))
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }
    pub fn model_name(&self) -> &str {
        self.cfg.model.trim()
    }
    /// 可取消的非流式调用；完整保留终态和用量，由业务调用方判断内容是否有效。
    pub async fn complete_async(&self, req: ModelRequest) -> Result<ModelResponse> {
        let (provider, request, tools) = self.prepare_request(req)?;
        let response = provider.complete(request).await.map_err(map_llm_error)?;
        filter_invalid_openai_tool_calls(
            self.protocol(),
            &tools,
            convert_provider_response(response),
        )
    }

    /// 流式与非流式消费同一请求，取消 future 时关闭底层 HTTP 流。
    pub async fn stream_async(
        self,
        req: ModelRequest,
        chunk_tx: tokio::sync::mpsc::UnboundedSender<ModelStreamChunk>,
    ) -> Result<ModelResponse> {
        let (provider, request, tools) = self.prepare_request(req)?;
        let preserve_tool_call_order = matches!(
            self.protocol(),
            ProviderProtocol::OpenAi
                | ProviderProtocol::OpenAiChatCompletions
                | ProviderProtocol::Codex
        );
        let stream = provider.stream(request).await.map_err(map_llm_error)?;
        let response = consume_provider_stream_events_async(
            stream,
            &mut |chunk: &ModelStreamChunk| {
                let _ = chunk_tx.send(chunk.clone());
            },
            preserve_tool_call_order,
        )
        .await;
        filter_invalid_openai_tool_calls(self.protocol(), &tools, response)
    }

    pub fn new(cfg: ModelEndpoint) -> Self {
        Self {
            session_id: scru128::new().to_string(),
            cfg,
            on_retry: None,
        }
    }

    /// 设置重试回调
    pub fn with_on_retry(mut self, cb: OnRetryCallback) -> Self {
        self.on_retry = Some(cb);
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
        let provider = build_codex_provider_from_config(
            cfg,
            cfg.timeout_ms,
            None,
            &scru128::new().to_string(),
            MAX_RETRIES,
        )?;
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
                MAX_RETRIES,
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
                MAX_RETRIES,
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
                MAX_RETRIES,
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
                MAX_RETRIES,
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
                MAX_RETRIES,
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
            MAX_RETRIES,
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
    ) -> Result<ProviderDispatch> {
        let session_id = session_id.unwrap_or(&self.session_id);
        match self.protocol() {
            ProviderProtocol::Anthropic => Ok(ProviderDispatch::Anthropic(Box::new(
                build_anthropic_provider_from_config(
                    &self.cfg,
                    timeout_ms,
                    self.on_retry.clone(),
                    session_id,
                    max_retries,
                )?,
            ))),
            ProviderProtocol::OpenAi => Ok(ProviderDispatch::OpenAiResponses(Box::new(
                build_openai_responses_provider_from_config(
                    &self.cfg,
                    timeout_ms,
                    self.on_retry.clone(),
                    session_id,
                    max_retries,
                )?,
            ))),
            ProviderProtocol::Codex => Ok(ProviderDispatch::Codex(Box::new(
                build_codex_provider_from_config(
                    &self.cfg,
                    timeout_ms,
                    self.on_retry.clone(),
                    session_id,
                    max_retries,
                )?,
            ))),
            ProviderProtocol::OpenAiChatCompletions => Ok(ProviderDispatch::OpenAiChat(Box::new(
                build_openai_provider_from_config(
                    &self.cfg,
                    timeout_ms,
                    self.on_retry.clone(),
                    session_id,
                    max_retries,
                )?,
            ))),
            ProviderProtocol::DeepSeek => Ok(ProviderDispatch::DeepSeek(Box::new(
                build_deepseek_provider_from_config(
                    &self.cfg,
                    timeout_ms,
                    self.on_retry.clone(),
                    session_id,
                    max_retries,
                )?,
            ))),
        }
    }

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

    /// 标题生成仅提供任务参数，复用统一非流式请求。
    pub fn complete_lite(&self, prompt: &str) -> Result<String> {
        self.complete_lite_request(
            "你是会话标题生成助手。根据用户输入生成简洁的标题，要求：1. 标题不超过10个汉字2. 直接返回标题，不要任何解释或额外文字3. 标题要概括性强，简洁明了",
            prompt,
            0.3,
        )
    }

    pub fn complete_lite_with_system(&self, system: &str, prompt: &str) -> Result<String> {
        self.complete_lite_request(system, prompt, 0.1)
    }

    fn complete_lite_request(
        &self,
        system: &str,
        prompt: &str,
        temperature: f32,
    ) -> Result<String> {
        let request = ModelRequest {
            system: system.to_string(),
            messages: vec![ChatMessage::text(LlmMessageRole::User, prompt)],
            max_output_tokens: Some(MAX_TOKENS_LITE),
            reasoning_effort: ReasoningEffort::None,
            temperature: Some(temperature),
            timeout_ms: Some(120_000),
            ..Default::default()
        };
        let response = self.complete(request)?;
        Ok(if self.protocol() == ProviderProtocol::Anthropic {
            strip_think_tags(&response.text).trim().to_string()
        } else {
            response.text
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

    fn complete_stream(
        &self,
        req: ModelRequest,
        on_delta: &mut dyn FnMut(&ModelStreamChunk),
    ) -> Result<ModelResponse> {
        let (provider, request, tools) = self.prepare_request(req)?;
        let response = block_on_provider_stream(provider, request, on_delta)?;
        filter_invalid_openai_tool_calls(self.protocol(), &tools, response)
    }

    fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        self.block_on_llm(self.complete_async(req))
    }
}

/// 主模型请求最大输出 token 数。
///
/// 规划/执行/响应等主模型请求统一使用此值。
/// thinking 模式下 thinking tokens 与 output tokens 共享此预算，
/// 因此 32k 确保 thinking 消耗 15-20k 后仍有 12-17k 给实际输出。
/// 此值从 tiangong-core 到 tiangong-llm 再到 provider 逐层透传，不做任何修改。
const MAX_TOKENS_MAIN: u32 = 32_768;

/// 轻量级任务（标题生成、分类判断等）最大输出 token 数。
/// 任务输出通常不超过几十个字，200 足够且能有效控制成本。
const MAX_TOKENS_LITE: u32 = 200;

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
    on_retry: Option<OnRetryCallback>,
    session_id: &str,
    max_retries: u32,
) -> Result<CodexProvider> {
    let mut config = CodexConfig::new(cfg.base_url.clone());
    config.timeout = Duration::from_millis(timeout_ms);
    config.max_retries = max_retries;
    config.retry_notifier = on_retry;
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
    }
}

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

/// 消费流事件并组装响应。
///
/// 流中断（传输错误或流内错误事件）时直接结束循环，把已产出的内容定稿为
/// 本次响应（issue #551）：断在 tool_use 参数 JSON 中间的调用带 `__parse_error`
/// 标记，由上层既有的无效参数链路处理；开头即断时为空响应，由上层按无效
/// 输出处理。调用方对中断无感，不再让整轮失败。
async fn consume_provider_stream_events_async<F: FnMut(&ModelStreamChunk) + ?Sized>(
    mut stream: ProviderStream,
    on_delta: &mut F,
    preserve_tool_call_order: bool,
) -> ModelResponse {
    let mut text = String::new();
    let mut reasoning_content = String::new();
    let mut reasoning_signature: Option<String> = None;
    let mut usage = TokenUsageData::default();
    let mut stop_reason = None;
    // 正文增量的真实 token 计数器（跨批合并由计数器内部缓冲定界）。
    let mut token_counter = crate::token_count::StreamTokenCounter::new();
    let mut tool_calls: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    let mut tool_call_order: Vec<String> = Vec::new();

    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                tracing::warn!(error = %err, "流式响应中断，按已产出内容收尾");
                break;
            }
        };
        match event {
            ProviderStreamEvent::ReasoningDelta(delta) => {
                if !delta.is_empty() {
                    reasoning_content.push_str(&delta);
                    on_delta(&ModelStreamChunk {
                        content: String::new(),
                        reasoning_content: delta,
                        usage: None,
                        tokens: None,
                    });
                }
            }
            ProviderStreamEvent::ReasoningSignatureDelta(signature) => {
                if !signature.trim().is_empty() {
                    reasoning_signature = Some(signature);
                }
            }
            ProviderStreamEvent::TextDelta(delta) => {
                if !delta.is_empty() {
                    text.push_str(&delta);
                    let tokens = token_counter.push(&delta);
                    on_delta(&ModelStreamChunk {
                        content: delta,
                        reasoning_content: String::new(),
                        usage: None,
                        tokens: Some(tokens),
                    });
                }
            }
            ProviderStreamEvent::ToolCallStart(call) => {
                let args = if call.arguments.is_null() || call.arguments == json!({}) {
                    String::new()
                } else {
                    call.arguments.to_string()
                };
                tool_call_order.push(call.id.clone());
                tool_calls.insert(call.id, (call.name, args));
            }
            ProviderStreamEvent::ToolCallDelta {
                call_id,
                partial_json,
            } => {
                let actual_id = if tool_calls.contains_key(&call_id) {
                    call_id
                } else if let Ok(idx) = call_id.parse::<usize>() {
                    tool_call_order.get(idx).cloned().unwrap_or_default()
                } else {
                    call_id
                };
                let entry = tool_calls
                    .entry(actual_id)
                    .or_insert_with(|| (String::new(), String::new()));
                append_stream_tool_call_arguments(&mut entry.1, &partial_json);
            }
            ProviderStreamEvent::Usage(stream_usage) => {
                merge_stream_usage(&mut usage, stream_usage);
                // usage 事件通常出现在流末（completed）：把计数器未定界尾部
                // 一并补齐送出，实时 TPS 不丢最后几个 token（finish 幂等）。
                let tail_tokens = token_counter.finish();
                on_delta(&ModelStreamChunk {
                    content: String::new(),
                    reasoning_content: String::new(),
                    usage: Some(usage.clone()),
                    tokens: Some(tail_tokens),
                });
            }
            ProviderStreamEvent::Error(message) => {
                tracing::warn!(error = %message, "流式响应中断，按已产出内容收尾");
                break;
            }
            ProviderStreamEvent::MessageEnd {
                stop_reason: stream_stop_reason,
            } => {
                if stream_stop_reason.is_some() {
                    stop_reason = stream_stop_reason;
                }
            }
            ProviderStreamEvent::MessageStart | ProviderStreamEvent::ToolCallEnd { .. } => {}
        }
    }

    let tool_calls = if preserve_tool_call_order {
        collect_openai_stream_tool_calls(tool_calls, tool_call_order)
    } else {
        tool_calls
            .into_iter()
            .filter(|entry| {
                let keep = !entry.1.0.is_empty();
                if !keep {
                    // 仅有增量片段、缺少 ToolCallStart 的调用无法还原工具名，
                    // 过滤前显式告警，避免上游漏发 Start 事件时静默丢数据。
                    tracing::warn!(
                        call_id = %entry.0,
                        "丢弃缺少 Start 事件的流式工具调用增量（工具名未知）"
                    );
                }
                keep
            })
            .map(|(id, (name, raw_args))| ToolCall {
                arguments: parse_tool_arguments_or_error(&name, &id, &raw_args),
                id,
                name,
            })
            .collect()
    };

    ModelResponse {
        text: text.trim().to_string(),
        reasoning_content,
        reasoning_signature: reasoning_signature.filter(|value| !value.trim().is_empty()),
        stop_reason,
        usage: usage.into(),
        tool_calls,
        invalid_tool_calls: Vec::new(),
    }
}

pub(crate) enum ProviderDispatch {
    Anthropic(Box<AnthropicProvider>),
    OpenAiResponses(Box<OpenAiResponsesProvider>),
    OpenAiChat(Box<OpenAiChatCompletionsProvider>),
    DeepSeek(Box<DeepSeekProvider>),
    Codex(Box<CodexProvider>),
}

impl ProviderDispatch {
    pub(crate) async fn complete(
        self,
        request: ProviderRequest,
    ) -> std::result::Result<ProviderResponse, crate::error::LlmError> {
        match self {
            ProviderDispatch::Anthropic(provider) => provider.complete(request).await,
            ProviderDispatch::OpenAiResponses(provider) => provider.complete(request).await,
            ProviderDispatch::OpenAiChat(provider) => provider.complete(request).await,
            ProviderDispatch::DeepSeek(provider) => provider.complete(request).await,
            ProviderDispatch::Codex(provider) => provider.complete(request).await,
        }
    }

    async fn stream(
        self,
        request: ProviderRequest,
    ) -> std::result::Result<ProviderStream, crate::error::LlmError> {
        match self {
            ProviderDispatch::Anthropic(provider) => provider.stream(request).await,
            ProviderDispatch::OpenAiResponses(provider) => provider.stream(request).await,
            ProviderDispatch::OpenAiChat(provider) => provider.stream(request).await,
            ProviderDispatch::DeepSeek(provider) => provider.stream(request).await,
            ProviderDispatch::Codex(provider) => provider.stream(request).await,
        }
    }
}

fn block_on_provider_stream(
    provider: ProviderDispatch,
    request: ProviderRequest,
    on_delta: &mut dyn FnMut(&ModelStreamChunk),
) -> Result<ModelResponse> {
    async fn run_stream(
        provider: ProviderDispatch,
        request: ProviderRequest,
        on_delta: &mut dyn FnMut(&ModelStreamChunk),
    ) -> Result<ModelResponse> {
        let preserve_tool_call_order = matches!(
            &provider,
            ProviderDispatch::OpenAiResponses(_)
                | ProviderDispatch::OpenAiChat(_)
                | ProviderDispatch::Codex(_)
        );
        let stream = provider.stream(request).await.map_err(map_llm_error)?;
        Ok(consume_provider_stream_events_async(stream, on_delta, preserve_tool_call_order).await)
    }

    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(run_stream(provider, request, on_delta)))
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
                        let response =
                            runtime.block_on(run_stream(provider, request, &mut |chunk| {
                                chunks.push(chunk.clone())
                            }));
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
            runtime.block_on(run_stream(provider, request, on_delta))
        }
    }
}

fn map_llm_error(error: crate::error::LlmError) -> anyhow::Error {
    anyhow::Error::new(error)
}

// ── 重试相关 ──────────────────────────────────────────────

/// 最大重试次数
const MAX_RETRIES: u32 = 3;

/// 清理非流式响应中的 `<think>...</think>` 标签
///
/// 部分模型即使设置了 `thinking.type=disabled`，仍在 content 中输出 `<think>` 标签。
/// 此函数移除所有 `<think>...</think>` 块，返回纯文本内容。
fn strip_think_tags(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("<think>") {
        result.push_str(&remaining[..start]);
        if let Some(end) = remaining[start..].find("</think>") {
            remaining = &remaining[start + end + 8..];
        } else {
            // 没有闭合标签，丢弃从 <think> 开始的所有内容
            return result;
        }
    }
    result.push_str(remaining);
    result
}

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

    fn model_response(tool_calls: Vec<ToolCall>, total_tokens: usize) -> ModelResponse {
        ModelResponse {
            text: String::new(),
            reasoning_content: String::new(),
            reasoning_signature: None,
            stop_reason: Some(StopReason::ToolUse),
            usage: TokenUsage {
                total_tokens,
                ..Default::default()
            },
            tool_calls,
            invalid_tool_calls: Vec::new(),
        }
    }

    #[test]
    fn openai_filters_invalid_parallel_calls_and_keeps_validation_errors() {
        let functions = vec![schema_tool("read_file"), schema_tool("other_tool")];
        let valid = ToolCall {
            id: "call_valid".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "TODO.md"}),
        };
        let invalid = ToolCall {
            id: "call_invalid".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({}),
        };

        let response = filter_invalid_openai_tool_calls(
            ProviderProtocol::OpenAiChatCompletions,
            &functions,
            model_response(vec![valid, invalid], 10),
        )
        .unwrap();

        assert_eq!(response.usage.total_tokens, 10);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_valid");
        assert_eq!(response.tool_calls[0].arguments, json!({"path": "TODO.md"}));
        assert_eq!(response.invalid_tool_calls.len(), 1);
        assert_eq!(response.invalid_tool_calls[0].id, "call_invalid");
        assert_eq!(response.invalid_tool_calls[0].arguments, json!({}));
        assert!(
            response.invalid_tool_calls[0]
                .reason
                .contains("参数不符合 schema")
        );
        assert!(response.invalid_tool_calls[0].reason.contains("参数位置=$"));
        assert!(
            response.invalid_tool_calls[0]
                .reason
                .contains("schema 位置=#/required")
        );
    }

    #[test]
    fn non_openai_protocol_does_not_isolate_tool_calls() {
        let invalid = ToolCall {
            id: "call_invalid".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({}),
        };

        let response = filter_invalid_openai_tool_calls(
            ProviderProtocol::DeepSeek,
            &[schema_tool("read_file")],
            model_response(vec![invalid], 10),
        )
        .unwrap();

        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].arguments, json!({}));
    }

    #[test]
    fn openai_tool_without_schema_keeps_valid_arguments() {
        let functions = vec![ToolSpec {
            name: "mcp_tool".to_string(),
            description: String::new(),
            input_schema: Value::Null,
        }];
        let call = ToolCall {
            id: "call_mcp".to_string(),
            name: "mcp_tool".to_string(),
            arguments: serde_json::json!({"value": 1}),
        };

        let response = filter_invalid_openai_tool_calls(
            ProviderProtocol::OpenAi,
            &functions,
            model_response(vec![call], 10),
        )
        .unwrap();

        assert_eq!(response.tool_calls[0].arguments, json!({"value": 1}));
    }

    #[test]
    fn openai_invalid_json_preserves_raw_arguments_for_regeneration() {
        let raw_arguments = r#"{"path":"TODO.md""#;
        let call = ToolCall {
            id: "call_invalid_json".to_string(),
            name: "read_file".to_string(),
            arguments: parse_tool_arguments_or_error(
                "read_file",
                "call_invalid_json",
                raw_arguments,
            ),
        };

        let response = filter_invalid_openai_tool_calls(
            ProviderProtocol::OpenAiChatCompletions,
            &[schema_tool("read_file")],
            model_response(vec![call], 10),
        )
        .unwrap();

        assert!(response.tool_calls.is_empty());
        assert_eq!(response.invalid_tool_calls.len(), 1);
        assert_eq!(
            response.invalid_tool_calls[0].arguments,
            json!(raw_arguments)
        );
        assert!(
            response.invalid_tool_calls[0]
                .reason
                .contains("工具参数 JSON 无效")
        );
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

    #[tokio::test]
    async fn stream_interrupt_finalizes_produced_content() {
        let events = vec![
            Ok(ProviderStreamEvent::TextDelta("部分输出".to_string())),
            // 完整调用：Start 即携带全部参数。
            Ok(ProviderStreamEvent::ToolCallStart(ToolCall {
                id: "call_complete".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "a"}),
            })),
            // 不完整调用：参数 JSON 只流出一半即断流。
            Ok(ProviderStreamEvent::ToolCallStart(ToolCall {
                id: "call_partial".to_string(),
                name: "write_file".to_string(),
                arguments: json!({}),
            })),
            Ok(ProviderStreamEvent::ToolCallDelta {
                call_id: "call_partial".to_string(),
                partial_json: r#"{"path":"b""#.to_string(),
            }),
            Err(crate::error::LlmError::Transport(
                "error decoding response body".to_string(),
            )),
        ];

        let response =
            consume_provider_stream_events_async(synthetic_stream(events), &mut |_| {}, false)
                .await;

        // 断流时未收到终止事件，stop_reason 保持缺失。
        assert_eq!(response.stop_reason, None);
        assert_eq!(response.text, "部分输出");
        // 断在参数中间的调用带 __parse_error 标记，由上层既有的无效参数链路处理。
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "call_complete");
        assert!(
            response.tool_calls[1]
                .arguments
                .get("__parse_error")
                .is_some()
        );
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

        let response =
            consume_provider_stream_events_async(synthetic_stream(events), &mut |_| {}, false)
                .await;

        assert_eq!(response.text, "");
        assert!(response.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn in_stream_error_event_finalizes_produced_content() {
        // provider 在流内报告错误事件（如 overloaded）同样按中断收尾。
        let events = vec![
            Ok(ProviderStreamEvent::TextDelta("写到一半".to_string())),
            Ok(ProviderStreamEvent::Error("overloaded_error".to_string())),
        ];

        let response =
            consume_provider_stream_events_async(synthetic_stream(events), &mut |_| {}, false)
                .await;

        assert_eq!(response.text, "写到一半");
        assert_eq!(response.stop_reason, None);
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
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_valid");
        assert_eq!(response.invalid_tool_calls.len(), 1);
        assert_eq!(response.invalid_tool_calls[0].id, "call_invalid");
        assert!(
            response.invalid_tool_calls[0]
                .reason
                .contains("参数不符合 schema")
        );
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
