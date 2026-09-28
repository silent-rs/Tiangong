//! 单 provider 客户端：把扁平的 [`ModelEndpoint`] 包装成同步/流式 LLM 调用入口。
//!
//! 这里聚合了：
//! - [`ModelClient`] trait、[`ModelRequest`] / [`ModelResponse`] / [`ModelStreamChunk`] 等数据类型
//! - [`SingleProviderClient`]：持有 [`ModelEndpoint`]，按 `protocol` 分发到 Anthropic / OpenAI / DeepSeek provider
//! - provider 构造辅助、消息构建、流式消费、错误映射等私有逻辑
//!
//! 迁移自 `tiangong-core/src/model.rs`，已移除 `ModelProviderConfig` 依赖，直接消费
//! [`ModelEndpoint`]（`base_url` / `api_key` / `model` / `protocol` / `timeout_ms` / `options`）。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tiangong_types::{ContentBlock, Message, MessageRole, StoredAsset};

use crate::endpoint::ModelEndpoint;
use crate::message::{
    ChatMessage, MessageContent as LlmMessageContent, MessageRole as LlmMessageRole,
    ThinkingContent as LlmThinkingContent,
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
    ToolCall, ToolCall as LlmToolCall, ToolChoice, ToolChoice as LlmToolChoice,
    ToolResult as LlmToolResult, ToolResultContent as LlmToolResultContent, ToolSpec,
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

#[derive(Debug, Clone, Default)]
pub struct ModelRequest {
    pub user_input: String,
    pub context: Vec<Message>,
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
}

pub trait ModelClient {
    fn api_base_url(&self) -> &str;
    fn api_timeout_ms(&self) -> u64;
    fn api_model(&self) -> &str;
    fn complete(&self, req: &ModelRequest) -> Result<ModelResponse>;
    /// 流式调用，通过 on_delta 实时回调内容和用量。
    fn complete_stream(
        &self,
        req: &ModelRequest,
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
    fn build_provider_request(&self, req: &ModelRequest) -> Result<ProviderRequest> {
        let model = self.cfg.model.trim();
        if model.is_empty() {
            return Err(anyhow!("API_MODEL 不能为空，无法发起模型请求"));
        }
        let (system, messages) = build_provider_messages(req)?;
        Ok(ProviderRequest {
            session_id: req
                .session_id
                .clone()
                .or_else(|| Some(self.session_id.clone())),
            model: model.to_string(),
            system: (!system.trim().is_empty()).then_some(system),
            messages,
            tools: req.tools.clone(),
            tool_choice: req
                .tool_choice
                .clone()
                .or_else(|| (!req.tools.is_empty()).then_some(LlmToolChoice::Auto)),
            max_tokens: req.max_output_tokens.unwrap_or(MAX_TOKENS_MAIN),
            temperature: req.temperature.or_else(configured_temperature_f32),
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: req.reasoning_effort,
        })
    }

    fn prepare_request(&self, req: &ModelRequest) -> Result<(ProviderDispatch, ProviderRequest)> {
        let request = self.build_provider_request(req)?;
        let timeout_ms = req
            .timeout_ms
            .unwrap_or_else(|| function_timeout_ms(self.cfg.timeout_ms));
        let provider =
            self.build_provider_dispatch(timeout_ms, req.session_id.as_deref(), MAX_RETRIES)?;
        Ok((provider, request))
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }
    pub fn model_name(&self) -> &str {
        self.cfg.model.trim()
    }
    /// 可取消的非流式调用；完整保留终态和用量，由业务调用方判断内容是否有效。
    pub async fn complete_async(&self, req: &ModelRequest) -> Result<ModelResponse> {
        let (provider, request) = self.prepare_request(req)?;
        let response = provider.complete(request).await.map_err(map_llm_error)?;
        filter_invalid_openai_tool_calls(
            self.protocol(),
            &req.tools,
            convert_provider_response(response),
        )
    }

    /// 流式与非流式消费同一请求，取消 future 时关闭底层 HTTP 流。
    pub async fn stream_async(
        self,
        req: ModelRequest,
        chunk_tx: tokio::sync::mpsc::UnboundedSender<ModelStreamChunk>,
    ) -> Result<ModelResponse> {
        let (provider, request) = self.prepare_request(&req)?;
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
        filter_invalid_openai_tool_calls(self.protocol(), &req.tools, response)
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
                    display_name: None,
                    context_window: None,
                    id,
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
            context: vec![
                Message::new(MessageRole::System, system),
                Message::new(MessageRole::User, prompt),
            ],
            max_output_tokens: Some(MAX_TOKENS_LITE),
            reasoning_effort: ReasoningEffort::None,
            temperature: Some(temperature),
            timeout_ms: Some(120_000),
            ..Default::default()
        };
        let response = self.complete(&request)?;
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
        req: &ModelRequest,
        on_delta: &mut dyn FnMut(&ModelStreamChunk),
    ) -> Result<ModelResponse> {
        let (provider, request) = self.prepare_request(req)?;
        let response = block_on_provider_stream(provider, request, on_delta)?;
        filter_invalid_openai_tool_calls(self.protocol(), &req.tools, response)
    }

    fn complete(&self, req: &ModelRequest) -> Result<ModelResponse> {
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

fn build_provider_messages(req: &ModelRequest) -> Result<(String, Vec<ChatMessage>)> {
    let mut messages = Vec::new();
    // System prompt 必须由 core 显式注入（session.system_prompt_message）：
    // context 缺 System 角色消息视为调用方错误，fail-fast。文本允许为空
    //（core 不再生产 prompt 内容，无插件段落且无摘要时为空，请求即不带 system）。
    let mut system_texts: Vec<String> = Vec::new();
    let mut has_system_message = false;
    for msg in &req.context {
        if msg.role == MessageRole::System {
            has_system_message = true;
            let text = msg.text_content().trim().to_string();
            if !text.is_empty() {
                system_texts.push(text);
            }
            continue;
        }
        if let Some(message) = provider_message_from_session(msg)? {
            messages.push(message);
        }
    }

    if !has_system_message {
        anyhow::bail!(
            "build_provider_messages: context 缺少 System 消息。system prompt 应由 core 显式注入（session.system_prompt_message）。"
        );
    }

    // 跳过消息列表开头的非 User 消息（summary_up_to 截断可能导致以 Assistant/Tool 开头）
    if let Some(idx) = messages.iter().position(|m| m.role == LlmMessageRole::User)
        && idx > 0
    {
        tracing::debug!(
            skipped = idx,
            "跳过消息列表开头的非 User 消息（可能是 summary 截断导致）"
        );
        messages = messages.split_off(idx);
    }

    if !req.user_input.is_empty() {
        messages.push(ChatMessage::text(
            LlmMessageRole::User,
            req.user_input.clone(),
        ));
    }

    let mut messages = sanitize_provider_messages(messages);
    if messages.is_empty() {
        let fallback = if req.user_input.trim().is_empty() {
            "请继续处理当前任务。".to_string()
        } else {
            req.user_input.trim().to_string()
        };
        messages.push(ChatMessage::text(LlmMessageRole::User, fallback));
    }

    Ok((system_texts.join("\n"), messages))
}

fn provider_message_from_session(msg: &Message) -> Result<Option<ChatMessage>> {
    let role = match msg.role {
        MessageRole::User => LlmMessageRole::User,
        MessageRole::Assistant => LlmMessageRole::Assistant,
        MessageRole::Tool => LlmMessageRole::Tool,
        MessageRole::System => return Ok(None),
        // Notice 是系统发给用户的通知，任何路径都不得进入模型请求。
        MessageRole::Notice => return Ok(None),
    };

    if msg.role == MessageRole::Tool {
        let text = msg.text_content();
        let Some(tool_call_id) = msg.tool_call_id.as_ref() else {
            if text.trim().is_empty() {
                return Ok(None);
            }
            let tool_name = msg.tool_name.as_deref().unwrap_or("runtime_context");
            return Ok(Some(ChatMessage::text(
                LlmMessageRole::User,
                format!(
                    "<tool-context name=\"{tool_name}\">\n{}\n</tool-context>",
                    text.trim()
                ),
            )));
        };
        return Ok(Some(ChatMessage::new(
            role,
            vec![LlmMessageContent::ToolResult(LlmToolResult {
                tool_call_id: tool_call_id.clone(),
                content: LlmToolResultContent::Text(text),
                is_error: msg.tool_result_is_error,
            })],
        )));
    }

    let mut content = Vec::new();
    if !msg.reasoning_content.trim().is_empty() {
        content.push(LlmMessageContent::Thinking(LlmThinkingContent {
            thinking: msg.reasoning_content.clone(),
            signature: msg.reasoning_signature.clone(),
        }));
    }

    // 宿主已经完成全部输入准备；这里只做内容块到 Provider 消息的机械映射。
    for block in &msg.content {
        match block {
            ContentBlock::Text { text: s } | ContentBlock::ModelInstruction { text: s } => {
                if !s.trim().is_empty() {
                    content.push(LlmMessageContent::Text(s.clone()));
                }
            }
            ContentBlock::Image { asset, data } => {
                match image_content_from_ready_image(asset, data.as_deref()) {
                    Some(img_content) => content.push(LlmMessageContent::Image(img_content)),
                    None => {
                        if msg.phase == tiangong_types::MessagePhase::HostInjected {
                            // 评审问题 2：宿主注入消息的图片文件缺失（被清理/
                            // 声明失效）时降级为文字说明，不让整个请求失败——
                            // 否则该会话之后每一轮请求都会报错。路径为工具
                            // 可控字段，净化后拼接（复评小问题 3）。
                            content.push(LlmMessageContent::Text(format!(
                                "[injected-image unavailable] 图片文件已不可读（asset_id={}，path={}），内容不可用",
                                asset.asset_id,
                                escape_asset_text(&asset.local_path)
                            )));
                        } else {
                            return Err(anyhow!("已就绪图片无法读取：asset_id={}", asset.asset_id));
                        }
                    }
                }
            }
            ContentBlock::Media { .. } | ContentBlock::AssetReference { .. } => {}
        }
    }

    content.extend(msg.tool_calls.iter().map(|call| {
        LlmMessageContent::ToolCall(LlmToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        })
    }));
    if content.is_empty() {
        return Ok(None);
    }

    Ok(Some(ChatMessage::new(role, content)))
}

fn image_content_from_runtime(mime_type: &str, data: &str) -> crate::message::ImageContent {
    let data = if data.trim_start().starts_with("data:") {
        data.to_string()
    } else {
        format!("data:{mime_type};base64,{data}")
    };
    crate::message::ImageContent {
        mime_type: mime_type.to_string(),
        data,
    }
}

fn image_content_from_ready_image(
    asset: &StoredAsset,
    runtime_data: Option<&str>,
) -> Option<crate::message::ImageContent> {
    if let Some(data) = runtime_data.filter(|data| !data.trim().is_empty()) {
        return Some(image_content_from_runtime(&asset.mime_type, data));
    }

    let trimmed = asset.local_path.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(crate::message::ImageContent {
            mime_type: asset.mime_type.clone(),
            data: trimmed.to_string(),
        });
    }

    let bytes = std::fs::read(Path::new(trimmed)).ok()?;
    let mime_type = asset.mime_type.clone();
    let data = format!(
        "data:{mime_type};base64,{}",
        general_purpose::STANDARD.encode(bytes)
    );
    Some(crate::message::ImageContent { mime_type, data })
}

/// 降级说明中资产路径的净化（复评小问题 3）：压缩控制字符与 Unicode
/// 行/双向控制符、按字符边界截断。
fn escape_asset_text(value: &str) -> String {
    fn is_suspicious(c: char) -> bool {
        c.is_control()
            || matches!(
                c,
                '\u{2028}' | '\u{2029}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}'
                    | '\u{2066}'..='\u{2069}'
            )
    }
    value
        .chars()
        .map(|c| if is_suspicious(c) { ' ' } else { c })
        .take(160)
        .collect()
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

        // 一个调用批次只消费紧随其后的 Tool 消息；工具内部上下文可以夹在
        // 并行结果之间，但普通 User/Assistant 会立即封闭批次。
        let mut cursor = index + 1;
        let mut results = std::collections::HashMap::<String, LlmToolResult>::new();
        let mut deferred_contexts = Vec::new();
        while cursor < input.len() {
            let message = &input[cursor];
            if message.role == LlmMessageRole::Tool {
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
                continue;
            }
            if is_internal_tool_context_message(message) {
                deferred_contexts.push(message.clone());
                cursor += 1;
                continue;
            }
            break;
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
        for context in deferred_contexts {
            push_provider_message(&mut output, context);
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

fn is_internal_tool_context_message(message: &ChatMessage) -> bool {
    message.role == LlmMessageRole::User
        && message.content.iter().any(|content| {
            matches!(
                content,
                LlmMessageContent::Text(text) if text.trim_start().starts_with("<tool-context")
            )
        })
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
                    on_delta(&ModelStreamChunk {
                        content: delta,
                        reasoning_content: String::new(),
                        usage: None,
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
                on_delta(&ModelStreamChunk {
                    content: String::new(),
                    reasoning_content: String::new(),
                    usage: Some(usage.clone()),
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
            .filter(|(_, (name, _))| !name.is_empty())
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
    use tiangong_types::{
        ContentBlock, MediaKind, Message, MessagePhase, MessageRole, StoredAsset,
    };
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
    fn build_provider_messages_fails_without_system_message() {
        // context 无 System 消息时必须 fail-fast，
        // 防止调用方遗漏 system prompt 注入。
        let user_msg = Message {
            id: "u".to_string(),
            role: MessageRole::User,
            content: vec![ContentBlock::text("你好")],
            reasoning_content: String::new(),
            reasoning_signature: None,
            usage: None,
            worker_id: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: MessagePhase::Normal,
            created_at: String::new(),
            elapsed_ms: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
            turn_status: None,
        };
        let req = ModelRequest {
            session_id: None,
            user_input: String::new(),
            context: vec![user_msg],
            reasoning_effort: ReasoningEffort::None,
            max_output_tokens: None,
            ..Default::default()
        };

        let err = build_provider_messages(&req).unwrap_err();
        assert!(
            err.to_string().contains("System"),
            "错误信息应指出缺少 System 消息，实际：{err}"
        );
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
            user_input: "检查项目".to_string(),
            context: vec![Message::new(MessageRole::System, "system")],
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
            user_input: "检查项目".to_string(),
            context: vec![Message::new(MessageRole::System, "system")],
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
            user_input: "检查项目".to_string(),
            context: vec![Message::new(MessageRole::System, "system")],
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

    #[test]
    fn sanitize_defers_internal_tool_context_until_tool_results_complete() {
        let messages = vec![
            ChatMessage::text(LlmMessageRole::User, "开始"),
            ChatMessage::new(
                LlmMessageRole::Assistant,
                vec![
                    LlmMessageContent::ToolCall(LlmToolCall {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "a"}),
                    }),
                    LlmMessageContent::ToolCall(LlmToolCall {
                        id: "call_2".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "b"}),
                    }),
                ],
            ),
            ChatMessage::new(
                LlmMessageRole::Tool,
                vec![LlmMessageContent::ToolResult(LlmToolResult {
                    tool_call_id: "call_1".to_string(),
                    content: LlmToolResultContent::Text("a".to_string()),
                    is_error: false,
                })],
            ),
            ChatMessage::text(
                LlmMessageRole::User,
                "<tool-context name=\"read_file\">trace</tool-context>",
            ),
            ChatMessage::new(
                LlmMessageRole::Tool,
                vec![LlmMessageContent::ToolResult(LlmToolResult {
                    tool_call_id: "call_2".to_string(),
                    content: LlmToolResultContent::Text("b".to_string()),
                    is_error: false,
                })],
            ),
        ];

        let sanitized = sanitize_provider_messages(messages);

        assert_eq!(sanitized.len(), 4);
        assert_eq!(sanitized[2].role, LlmMessageRole::Tool);
        assert_eq!(provider_message_tool_result_count(&sanitized[2]), 2);
        assert_eq!(sanitized[3].role, LlmMessageRole::User);
        assert!(is_internal_tool_context_message(&sanitized[3]));
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

    #[test]
    fn ready_image_prefers_current_data() {
        let msg = test_user_message_with_ready_image(
            "/path/that/does/not/exist.png",
            Some("CURRENT_RUNTIME_BASE64"),
        );

        let result = provider_message_from_session(&msg)
            .expect("映射不应失败")
            .expect("应生成消息");
        let image = result
            .content
            .iter()
            .find_map(|content| match content {
                LlmMessageContent::Image(image) => Some(image),
                _ => None,
            })
            .expect("已就绪图片应进入请求");
        assert_eq!(image.data, "data:image/png;base64,CURRENT_RUNTIME_BASE64");
    }

    /// 评审问题 2 回归：HostInjected 消息的图片文件缺失（被清理/声明
    /// 失效）时，provider 映射降级为文字说明，整个请求不得失败——
    /// 否则该会话之后每一轮请求都会报错。
    #[test]
    fn host_injected_image_with_missing_file_degrades_to_text() {
        let mut msg = test_message(vec![
            ContentBlock::model_instruction("[injected-images provenance]\n- 工具截图产出图片"),
            ContentBlock::Image {
                asset: test_asset(
                    MediaKind::Image,
                    "/nonexistent/path/deleted-by-cleanup.png",
                    "image/png",
                ),
                data: None,
            },
        ]);
        msg.role = MessageRole::User;
        msg.phase = tiangong_types::MessagePhase::HostInjected;

        let result = provider_message_from_session(&msg)
            .expect("映射不得失败")
            .expect("应生成消息");
        assert!(
            result.content.iter().any(|content| matches!(
                content,
                LlmMessageContent::Text(text) if text.contains("injected-image unavailable")
            )),
            "缺失图片应降级为文字说明"
        );
        assert!(
            !result
                .content
                .iter()
                .any(|content| matches!(content, LlmMessageContent::Image(_)))
        );
    }

    /// 抓包式判据：含 HostInjected 图片消息的请求经 OpenAI Chat
    /// Completions 协议真实发出时，HTTP body 必须包含 image_url 形式的
    /// 原生图片内容与 provenance 文本——排除序列化层静默丢图。
    #[tokio::test]
    async fn openai_request_body_carries_host_injected_image() {
        use wiremock::{Mock, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("tiangong-wire-{}-{unique}.png", std::process::id()));
        std::fs::write(&path, [5_u8, 6, 7, 8]).unwrap();

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-ok",
                "object": "chat.completion",
                "created": 0,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11}
            })))
            .mount(&server)
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
        let mut injected = test_message(vec![
            ContentBlock::model_instruction(
                "[injected-images provenance]\n- 工具 desktop_screenshot 产出图片",
            ),
            ContentBlock::Image {
                asset: test_asset(MediaKind::Image, path.to_str().unwrap(), "image/png"),
                data: None,
            },
        ]);
        injected.role = MessageRole::User;
        injected.phase = tiangong_types::MessagePhase::HostInjected;
        let request = ModelRequest {
            session_id: None,
            user_input: String::new(),
            context: vec![
                Message::new(MessageRole::System, "system"),
                Message::new(MessageRole::User, "看看屏幕"),
                injected,
            ],
            reasoning_effort: ReasoningEffort::None,
            max_output_tokens: None,
            ..Default::default()
        };
        let _response = client.complete_async(&request).await.expect("请求应成功");

        let bodies = server.received_requests().await.expect("应捕获到请求");
        let body = String::from_utf8(bodies[0].body.clone()).expect("body 应为 UTF-8");
        assert!(
            body.contains("image_url"),
            "HTTP body 必须包含 image_url 原生图片内容，实际 body 前 500 字：{}",
            &body[..body.len().min(500)]
        );
        assert!(
            body.contains("injected-images"),
            "provenance 文本必须随请求发出"
        );
    }

    /// 端到端序列判据（RFC 0017 注入消息「进入 agent 上下文」）：完整
    /// 工具轮序列 user → assistant(tool_calls) → tool 结果 → HostInjected
    /// 注入消息（provenance + Image）经 build_provider_messages 组装后，
    /// 注入消息必须保留——provenance 文本与原生图片内容都在，且位于
    /// Tool 消息之后。任何环节（映射、跳头、sanitize 配对整形）把它
    /// 丢弃都视为回归。
    #[test]
    fn host_injected_message_survives_full_request_assembly() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tiangong-inject-seq-{}-{unique}.png",
            std::process::id()
        ));
        std::fs::write(&path, [1_u8, 2, 3, 4]).unwrap();

        let system_msg = Message {
            id: "sys".to_string(),
            role: MessageRole::System,
            content: vec![ContentBlock::text("你是通用助手。")],
            reasoning_content: String::new(),
            reasoning_signature: None,
            usage: None,
            worker_id: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: MessagePhase::Normal,
            created_at: String::new(),
            elapsed_ms: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
            turn_status: None,
        };
        let mut anchor_user = test_message(vec![ContentBlock::text("看看屏幕")]);
        anchor_user.role = MessageRole::User;

        let mut assistant = test_message(Vec::new());
        assistant.role = MessageRole::Assistant;
        assistant.tool_calls = vec![tiangong_types::MessageToolCall {
            id: "call-1".to_string(),
            name: "desktop_screenshot".to_string(),
            arguments: serde_json::json!({}),
        }];

        let mut tool_result =
            test_message(vec![ContentBlock::text("{\"path\": \"/tmp/shot.png\"}")]);
        tool_result.role = MessageRole::Tool;
        tool_result.tool_call_id = Some("call-1".to_string());
        tool_result.tool_name = Some("desktop_screenshot".to_string());

        let mut injected = test_message(vec![
            ContentBlock::model_instruction(
                "[injected-images provenance]\n- 工具 desktop_screenshot（call-1）产出图片 shot.png",
            ),
            ContentBlock::Image {
                asset: test_asset(MediaKind::Image, path.to_str().unwrap(), "image/png"),
                data: None,
            },
        ]);
        injected.role = MessageRole::User;
        injected.phase = tiangong_types::MessagePhase::HostInjected;

        let req = ModelRequest {
            session_id: None,
            user_input: String::new(),
            context: vec![system_msg, anchor_user, assistant, tool_result, injected],
            reasoning_effort: ReasoningEffort::None,
            max_output_tokens: None,
            ..Default::default()
        };
        let (_system, messages) = build_provider_messages(&req).unwrap();

        // 定位注入消息：必须是 User 角色、含 provenance 文本和图片内容。
        let position = messages
            .iter()
            .position(|m| {
                m.role == LlmMessageRole::User
                    && m.content.iter().any(|content| {
                        matches!(
                            content,
                            LlmMessageContent::Text(text) if text.contains("injected-images")
                        )
                    })
            })
            .expect("注入消息必须存在于组装后的请求");
        assert!(
            messages[position]
                .content
                .iter()
                .any(|content| matches!(content, LlmMessageContent::Image(_))),
            "注入消息必须携带原生图片内容"
        );
        // 位置判据：必须在 Tool 结果消息之后（不被配对整形吞掉或前移）。
        let tool_position = messages
            .iter()
            .position(|m| m.role == LlmMessageRole::Tool)
            .expect("工具结果消息必须存在");
        assert!(position > tool_position, "注入消息应位于工具结果之后");
    }

    #[test]
    fn historical_ready_image_is_encoded_from_local_path() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tiangong-provider-inline-{}-{unique}.png",
            std::process::id()
        ));
        std::fs::write(&path, [1_u8, 2, 3, 4]).unwrap();
        let msg = test_user_message_with_ready_image(path.to_str().unwrap(), None);

        let result = provider_message_from_session(&msg)
            .expect("映射不应失败")
            .expect("应生成消息");
        let image = result
            .content
            .iter()
            .find_map(|content| match content {
                LlmMessageContent::Image(image) => Some(image),
                _ => None,
            })
            .expect("历史图片应从本地路径重编码");
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, "data:image/png;base64,AQIDBA==");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_historical_ready_image_fails_the_request() {
        let msg = test_user_message_with_ready_image("/path/that/does/not/exist-history.png", None);

        let error = provider_message_from_session(&msg).unwrap_err();
        assert!(error.to_string().contains("已就绪图片无法读取"));
        assert!(error.to_string().contains("asset-ready"));
    }

    /// RFC 0017「看见而非知道」判据的回归保护：HostInjected 注入消息
    /// （provenance + Image、data=None）映射后必须携带原生图片内容
    /// （从 local_path 读取编码），不得退化为文本提及路径。
    #[test]
    fn host_injected_message_carries_native_image() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tiangong-model-only-{}-{unique}.png",
            std::process::id()
        ));
        std::fs::write(&path, [9_u8, 8, 7, 6]).unwrap();
        let mut msg = test_message(vec![
            ContentBlock::model_instruction(
                "[injected-images provenance]\n- 工具 desktop_screenshot",
            ),
            ContentBlock::Image {
                asset: test_asset(MediaKind::Image, path.to_str().unwrap(), "image/png"),
                data: None,
            },
        ]);
        msg.role = MessageRole::User;
        msg.phase = tiangong_types::MessagePhase::HostInjected;

        let result = provider_message_from_session(&msg)
            .expect("映射不应失败")
            .expect("应生成消息");
        assert!(result.content.iter().any(|content| matches!(
            content,
            LlmMessageContent::Text(text) if text.contains("injected-images")
        )));
        let image = result
            .content
            .iter()
            .find_map(|content| match content {
                LlmMessageContent::Image(image) => Some(image),
                _ => None,
            })
            .expect("注入消息必须携带原生图片内容");
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, "data:image/png;base64,CQgHBg==");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn model_instruction_is_mapped_verbatim_and_asset_reference_is_ignored() {
        let instruction = "  使用宿主已选择的资源能力处理 path=/tmp/report.pdf\n";
        let msg = test_message(vec![
            ContentBlock::text("处理资源"),
            ContentBlock::AssetReference {
                asset: test_asset(MediaKind::File, "/tmp/report.pdf", "application/pdf"),
            },
            ContentBlock::model_instruction(instruction),
        ]);

        let result = provider_message_from_session(&msg)
            .expect("映射不应失败")
            .expect("应生成消息");
        assert!(result.content.iter().any(|content| matches!(
            content,
            LlmMessageContent::Text(text) if text == instruction
        )));
        assert_eq!(
            result
                .content
                .iter()
                .filter(|content| matches!(content, LlmMessageContent::Text(_)))
                .count(),
            2
        );
        assert!(!result.content.iter().any(|content| matches!(
            content,
            LlmMessageContent::Image(_) | LlmMessageContent::File(_)
        )));
    }

    #[test]
    fn legacy_media_is_ignored_without_provider_decision() {
        let msg = test_message(vec![
            ContentBlock::text("仅保留用户文本"),
            ContentBlock::Media {
                kind: MediaKind::Image,
                url: "data:image/png;base64,SHOULD_NOT_INLINE".to_string(),
                mime_type: Some("image/png".to_string()),
                title: None,
            },
        ]);
        let result = provider_message_from_session(&msg)
            .expect("映射不应失败")
            .expect("应生成消息");
        assert_eq!(result.content.len(), 1);
        assert!(matches!(
            &result.content[0],
            LlmMessageContent::Text(text) if text == "仅保留用户文本"
        ));
    }

    fn test_user_message_with_ready_image(local_path: &str, data: Option<&str>) -> Message {
        test_message(vec![
            ContentBlock::text("处理图片"),
            ContentBlock::Image {
                asset: test_asset(MediaKind::Image, local_path, "image/png"),
                data: data.map(str::to_string),
            },
        ])
    }

    fn test_asset(kind: MediaKind, local_path: &str, mime_type: &str) -> StoredAsset {
        StoredAsset {
            asset_id: "asset-ready".to_string(),
            local_path: local_path.to_string(),
            original_name: "ready-resource".to_string(),
            mime_type: mime_type.to_string(),
            size: 4,
            kind,
        }
    }

    fn test_message(content: Vec<ContentBlock>) -> Message {
        Message {
            id: "msg-ready".to_string(),
            role: MessageRole::User,
            content,
            reasoning_content: String::new(),
            reasoning_signature: None,
            usage: None,
            worker_id: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: MessagePhase::Normal,
            created_at: String::new(),
            elapsed_ms: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
            turn_status: None,
        }
    }

    #[test]
    fn end_to_end_request_preserves_ready_instruction() {
        let instruction = "读取宿主提供的资源 path=/tmp/report.pdf";
        let user_msg = test_message(vec![
            ContentBlock::text("请处理资源。"),
            ContentBlock::AssetReference {
                asset: test_asset(MediaKind::File, "/tmp/report.pdf", "application/pdf"),
            },
            ContentBlock::model_instruction(instruction),
        ]);

        let system_msg = Message {
            id: "sys".to_string(),
            role: MessageRole::System,
            content: vec![ContentBlock::text("你是通用助手。")],
            reasoning_content: String::new(),
            reasoning_signature: None,
            usage: None,
            worker_id: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: MessagePhase::Normal,
            created_at: String::new(),
            elapsed_ms: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
            turn_status: None,
        };

        let req = ModelRequest {
            session_id: None,
            user_input: String::new(),
            context: vec![system_msg, user_msg],
            reasoning_effort: ReasoningEffort::None,
            max_output_tokens: None,
            ..Default::default()
        };

        let (system, messages) = build_provider_messages(&req).unwrap();

        assert_eq!(system, "你是通用助手。");
        let user_content = messages
            .iter()
            .find(|m| m.role == LlmMessageRole::User)
            .expect("应有 user 消息");
        assert!(user_content.content.iter().any(|content| matches!(
            content,
            LlmMessageContent::Text(text) if text == instruction
        )));
    }
}
