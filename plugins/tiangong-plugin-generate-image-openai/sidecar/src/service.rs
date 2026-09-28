//! Generate-Image-OpenAI sidecar 业务服务。
//!
//! 通过 OpenAI Responses API 的 image_generation 工具（或 Chat Completions 兼容生图）
//! 生成图片，解析响应并归档落盘。支持三种模型来源：全局模型配置（models.json）、
//! ChatGPT 账号（Codex 登录）或手动输入端点。

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use serde_json::{Value, json};
use tiangong_llm::{ModelCapability, ModelsConfig, ProviderProtocol};
use tiangong_plugin_generate_image_openai_protocol::{
    Ack, ChatgptAccountInfo, ConfigBootstrap, ConfigSelection, Empty, GENERATE_OPERATION,
    GET_CONFIG_OPERATION, GenerateRequest, GenerateResponse, GeneratedImage,
    IMAGE_PROTOCOL_VERSION, ImageApiProtocol, ImageGenConfig, ModelInfo, ModelSource, PLUGIN_ID,
    PLUGIN_VERSION, RECONFIGURE_OPERATION, ResolvedEndpoint, SET_CONFIG_OPERATION,
};
use tiangong_plugin_runtime::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};

use crate::config;
use crate::extract;
use crate::transport;

/// ChatGPT 账号生图的默认模型。
const DEFAULT_CHATGPT_MODEL: &str = "gpt-5.5";

pub struct ImageService;

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for ImageService {
    async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }

        let payload = match dispatch_operation(&request.operation, request.payload).await {
            Ok(value) => value,
            Err(error) => {
                return Response::error(
                    &request_id,
                    ErrorCode::ServiceError,
                    format!("{error:#}"),
                    false,
                );
            }
        };
        Response::success(&request_id, payload)
    }
}

async fn dispatch_operation(operation: &str, payload: Value) -> Result<Value> {
    match operation {
        HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
            plugin_id: PLUGIN_ID.to_string(),
            plugin_version: PLUGIN_VERSION.to_string(),
            sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION.to_string(),
            business_protocol: IMAGE_PROTOCOL_VERSION,
            capabilities: vec!["image_generation".to_string()],
            instance_id: format!("image-openai-sidecar-{}", std::process::id()),
            status: ServiceStatus::Ready,
        })
        .context("序列化握手响应失败"),

        GENERATE_OPERATION => {
            let req: GenerateRequest =
                serde_json::from_value(payload).context("解析 generate 请求失败")?;
            let result = generate(req).await?;
            serde_json::to_value(result).context("序列化 generate 响应失败")
        }

        GET_CONFIG_OPERATION => {
            let _payload: Empty = serde_json::from_value(payload).unwrap_or_default();
            let bootstrap = build_bootstrap()?;
            serde_json::to_value(bootstrap).context("序列化配置 bootstrap 响应失败")
        }

        SET_CONFIG_OPERATION => {
            let selection: ConfigSelection =
                serde_json::from_value(payload).context("解析配置选择失败")?;
            // 保存时立即解析端点并缓存，运行时不再依赖 models.json。
            let resolved = resolve_selection_endpoint(&selection)?;
            config::save_selection(&selection, resolved)?;
            serde_json::to_value(Ack::default()).context("序列化配置保存响应失败")
        }

        RECONFIGURE_OPERATION => {
            // on_config_updated 触发：重新读盘并尝试刷新已缓存的端点。
            let existing = config::load_or_default();
            if let Some(updated) = try_refresh_resolved(&existing)? {
                config::save_resolved(&updated)?;
            }
            serde_json::to_value(Ack::default()).context("序列化 reconfigure 响应失败")
        }

        other => Err(anyhow!("未知的操作: {other}")),
    }
}

/// 生成/编辑图片：读配置 → 解析端点 → 调 Responses API → 提取图片 → 归档落盘。
///
/// `req.images` 非空时为编辑模式，空时为生成模式。
async fn generate(req: GenerateRequest) -> Result<GenerateResponse> {
    if req.prompt.trim().is_empty() {
        anyhow::bail!("prompt 不能为空");
    }

    let config = config::load_or_default();
    let resolved = resolve_endpoint(&config)?;
    let payload = match resolved.protocol {
        ImageApiProtocol::ChatCompletions => {
            build_chat_request(&req.prompt, &resolved.model, &config, &req.images)?
        }
        ImageApiProtocol::Responses => {
            build_responses_request(&req.prompt, &resolved.model, &config, &req.images)?
        }
        ImageApiProtocol::Codex => {
            build_codex_request(&req.prompt, &resolved.model, &config, &req.images)?
        }
    };

    tracing::debug!(model = %resolved.model, protocol = ?resolved.protocol, "生图请求");
    let response = send_request(&resolved, payload).await?;
    let raw_images = extract::extract_images(&response)?;
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&resolved.model)
        .to_string();

    let mut images = Vec::new();
    for raw in &raw_images {
        let reference = match tiangong_media_archive::archive_image_reference(raw, None, None) {
            Ok(archived) => archived.path().to_string(),
            Err(err) => {
                tracing::warn!(error = %err, "图片归档失败，保留原始引用");
                raw.clone()
            }
        };
        images.push(GeneratedImage { reference });
    }

    Ok(GenerateResponse { images, model })
}

/// 运行时解析模型端点：只读 config.json 里缓存的 resolved，不再依赖 models.json。
///
/// resolved 在保存配置时（SET_CONFIG_OPERATION）或 on_config_updated 触发时已写入。
/// Codex 协议不缓存令牌：发请求时从宿主维护的登录凭据只读取用。
fn resolve_endpoint(config: &ImageGenConfig) -> Result<ResolvedEndpoint> {
    let resolved = &config.resolved;
    if resolved.base_url.trim().is_empty() {
        anyhow::bail!("未配置有效端点，请在设置页选择模型或手动输入端点后保存");
    }
    if resolved.model.trim().is_empty() {
        anyhow::bail!("已缓存端点缺少 model");
    }
    if resolved.protocol == ImageApiProtocol::Codex {
        return Ok(resolved.clone());
    }
    // api_key 支持 ${ENV_VAR}，在保存配置时已解析；这里兜底再解析一次（兼容旧配置）。
    let api_key = ModelsConfig::resolve_api_key(&resolved.api_key);
    if api_key.trim().is_empty() {
        anyhow::bail!("已缓存端点缺少 api_key");
    }
    Ok(ResolvedEndpoint {
        api_key,
        ..resolved.clone()
    })
}

/// 按协议发送请求；Codex 协议此时读取登录凭据。
async fn send_request(resolved: &ResolvedEndpoint, payload: Value) -> Result<Value> {
    if resolved.protocol == ImageApiProtocol::Codex {
        let access =
            tiangong_llm::providers::codex::access_readonly().map_err(|err| anyhow!("{err}"))?;
        let target = transport::Target {
            protocol: ImageApiProtocol::Codex,
            base_url: &resolved.base_url,
            api_key: &access.access_token,
            account_id: access.account_id.as_deref(),
            residency: access.residency.as_deref(),
        };
        return transport::send(&target, payload).await;
    }
    let target = transport::Target {
        protocol: resolved.protocol,
        base_url: &resolved.base_url,
        api_key: &resolved.api_key,
        account_id: None,
        residency: None,
    };
    transport::send(&target, payload).await
}

/// 全局模型所属供应商协议 → 生图协议。
fn image_protocol_for(protocol: ProviderProtocol) -> ImageApiProtocol {
    match protocol {
        ProviderProtocol::Codex => ImageApiProtocol::Codex,
        ProviderProtocol::OpenAiChatCompletions => ImageApiProtocol::ChatCompletions,
        // Responses 为 image_generation 工具的原生协议；其余协议沿用旧行为按 Responses 调用。
        _ => ImageApiProtocol::Responses,
    }
}

/// 保存配置时解析选择对应的端点，写入 config.resolved 缓存。
///
/// - global：从 models.json 按 key（或 chat 能力回退）解析完整端点。
/// - manual：直接用手动输入的 base_url/api_key/model（api_key 解析 ${ENV_VAR}）。
///
/// 解析失败时返回错误（不保存），让 UI 提示用户。
fn resolve_selection_endpoint(selection: &ConfigSelection) -> Result<ResolvedEndpoint> {
    match selection.source {
        ModelSource::Global => {
            let resolved = if let Some(key) = selection.global_model_key.as_deref() {
                tiangong_plugin_sidecar::model::resolve_for_model_key(key)
            } else {
                tiangong_plugin_sidecar::model::resolve_for_capability(ModelCapability::Chat)
            }?;
            let protocol = image_protocol_for(resolved.protocol);
            Ok(ResolvedEndpoint {
                base_url: if protocol == ImageApiProtocol::Codex {
                    tiangong_llm::providers::codex::CODEX_BASE_URL.to_string()
                } else {
                    resolved.base_url
                },
                // Codex 鉴权来自登录态，不缓存任何令牌。
                api_key: if protocol == ImageApiProtocol::Codex {
                    String::new()
                } else {
                    resolved.api_key
                },
                model: resolved.model,
                protocol,
            })
        }
        ModelSource::Chatgpt => {
            let model = selection
                .chatgpt_model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .unwrap_or(DEFAULT_CHATGPT_MODEL);
            Ok(ResolvedEndpoint {
                base_url: tiangong_llm::providers::codex::CODEX_BASE_URL.to_string(),
                api_key: String::new(),
                model: model.to_string(),
                protocol: ImageApiProtocol::Codex,
            })
        }
        ModelSource::Manual => {
            let endpoint = &selection.manual_endpoint;
            if endpoint.base_url.trim().is_empty() {
                anyhow::bail!("手动端点缺少 base_url");
            }
            if endpoint.model.trim().is_empty() {
                anyhow::bail!("手动端点缺少 model id");
            }
            let api_key = ModelsConfig::resolve_api_key(&endpoint.api_key);
            if api_key.trim().is_empty() {
                anyhow::bail!("手动端点缺少 api_key");
            }
            if endpoint.protocol == ImageApiProtocol::Codex {
                anyhow::bail!("手动端点仅支持 Responses 或 Chat Completions 协议");
            }
            Ok(ResolvedEndpoint {
                base_url: endpoint.base_url.clone(),
                api_key,
                model: endpoint.model.clone(),
                protocol: endpoint.protocol,
            })
        }
    }
}

/// on_config_updated 触发时尝试刷新已缓存的端点。
///
/// 仅 global 来源重新解析（用户可能改了 models.json）；manual 来源保持不变。
/// 解析失败时返回 None（保留旧配置，不阻断服务）。
fn try_refresh_resolved(existing: &ImageGenConfig) -> Result<Option<ImageGenConfig>> {
    if existing.source != ModelSource::Global {
        return Ok(None);
    }
    let selection = ConfigSelection::from(existing);
    match resolve_selection_endpoint(&selection) {
        Ok(new_resolved) => {
            if new_resolved == existing.resolved {
                Ok(None)
            } else {
                let mut updated = existing.clone();
                updated.resolved = new_resolved;
                Ok(Some(updated))
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "刷新全局端点失败，保留旧配置");
            Ok(None)
        }
    }
}

/// 构造 Responses API 请求体。
///
/// 使用 OpenAI Responses API 的内置 `image_generation` 工具。
/// - 无图片时为生成模式（input 是字符串）。
/// - 有图片时为编辑模式（input 是数组，含 input_text + 每张图的 input_image，tools 设 action: edit）。
fn build_responses_request(
    prompt: &str,
    model: &str,
    config: &ImageGenConfig,
    images: &[String],
) -> Result<Value> {
    let mut body = if images.is_empty() {
        // 生成模式
        json!({
            "model": model,
            "tools": [{"type": "image_generation"}],
            "input": prompt,
        })
    } else {
        // 编辑模式：input 数组含文本 + 每张原图
        let mut content = vec![json!({"type": "input_text", "text": prompt})];
        for path in images {
            let data_uri = read_image_as_data_uri(path)?;
            content.push(json!({"type": "input_image", "image_url": data_uri}));
        }
        json!({
            "model": model,
            "tools": [{"type": "image_generation", "action": "edit"}],
            "input": [{"role": "user", "content": content}],
        })
    };

    if let Some(extra) = config.extra_prompt.as_deref() {
        let trimmed = extra.trim();
        if !trimmed.is_empty() {
            body["instructions"] = json!(trimmed);
        }
    }
    Ok(body)
}

/// 读取本地图片文件，编码为 `data:image/{mime};base64,...` 形式。
fn read_image_as_data_uri(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        anyhow::bail!("图片路径为空");
    }
    let bytes = std::fs::read(trimmed).with_context(|| format!("读取图片文件失败：{trimmed}"))?;
    let mime = infer_image_mime(trimmed);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// 按扩展名推断图片 MIME。
fn infer_image_mime(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else {
        "image/png"
    }
}

/// 构造 Codex（ChatGPT 账号）生图请求。
///
/// Codex 后端约束：`input` 必须是消息数组、强制 `stream=true` / `store=false`，
/// `instructions` 必须存在（可为空串）。
fn build_codex_request(
    prompt: &str,
    model: &str,
    config: &ImageGenConfig,
    images: &[String],
) -> Result<Value> {
    let mut content = vec![json!({ "type": "input_text", "text": prompt })];
    for path in images {
        let data_uri = read_image_as_data_uri(path)?;
        content.push(json!({ "type": "input_image", "image_url": data_uri }));
    }
    let tool = if images.is_empty() {
        json!({ "type": "image_generation" })
    } else {
        json!({ "type": "image_generation", "action": "edit" })
    };
    let instructions = config
        .extra_prompt
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    Ok(json!({
        "model": model,
        "instructions": instructions,
        "input": [{ "type": "message", "role": "user", "content": content }],
        "tools": [tool],
        "store": false,
        "stream": true,
    }))
}

/// 构造 Chat Completions 生图请求：提示词与原图作为多模态 user 消息。
fn build_chat_request(
    prompt: &str,
    model: &str,
    config: &ImageGenConfig,
    images: &[String],
) -> Result<Value> {
    let mut messages = Vec::new();
    if let Some(extra) = config
        .extra_prompt
        .as_deref()
        .map(str::trim)
        .filter(|extra| !extra.is_empty())
    {
        messages.push(json!({ "role": "system", "content": extra }));
    }
    let user_content = if images.is_empty() {
        json!(prompt)
    } else {
        let mut parts = vec![json!({ "type": "text", "text": prompt })];
        for path in images {
            let data_uri = read_image_as_data_uri(path)?;
            parts.push(json!({ "type": "image_url", "image_url": { "url": data_uri } }));
        }
        Value::Array(parts)
    };
    messages.push(json!({ "role": "user", "content": user_content }));
    Ok(json!({
        "model": model,
        "messages": messages,
        // OpenRouter 等兼容网关以 modalities 声明需要图片输出，其他服务会忽略该字段。
        "modalities": ["image", "text"],
        "stream": false,
    }))
}

/// 构造设置页 bootstrap：当前配置 + 全局可选模型列表。
fn build_bootstrap() -> Result<ConfigBootstrap> {
    let config = config::load_or_default();
    let models = tiangong_plugin_sidecar::model::list_models_for_capability(ModelCapability::Chat)?;
    let model_infos = models
        .into_iter()
        .map(|m| ModelInfo {
            key: m.key,
            provider: m.provider,
            model: m.model,
            configured: m.configured,
        })
        .collect();
    Ok(ConfigBootstrap {
        config,
        models: model_infos,
        chatgpt: chatgpt_account_info(),
    })
}

/// ChatGPT 账号状态：只读登录凭据元信息，不返回令牌。
fn chatgpt_account_info() -> ChatgptAccountInfo {
    let credentials = tiangong_llm::providers::codex::load_credentials()
        .ok()
        .flatten();
    let models = tiangong_plugin_sidecar::model::load_models_config()
        .map(|config| {
            let codex_providers: Vec<&String> = config
                .providers
                .iter()
                .filter(|(_, provider)| provider.protocol == ProviderProtocol::Codex)
                .map(|(name, _)| name)
                .collect();
            let mut models: Vec<String> = config
                .models
                .values()
                .filter(|entry| codex_providers.contains(&&entry.provider))
                .map(|entry| entry.model.clone())
                .collect();
            models.sort();
            models.dedup();
            models
        })
        .unwrap_or_default();
    ChatgptAccountInfo {
        logged_in: credentials.is_some(),
        email: credentials.and_then(|creds| creds.email),
        models,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_protocol_mapping() {
        assert_eq!(
            image_protocol_for(ProviderProtocol::Codex),
            ImageApiProtocol::Codex
        );
        assert_eq!(
            image_protocol_for(ProviderProtocol::OpenAiChatCompletions),
            ImageApiProtocol::ChatCompletions
        );
        assert_eq!(
            image_protocol_for(ProviderProtocol::OpenAi),
            ImageApiProtocol::Responses
        );
    }

    #[test]
    fn chatgpt_selection_resolves_without_secret() {
        let selection = ConfigSelection {
            source: ModelSource::Chatgpt,
            chatgpt_model: Some("gpt-6-sol".to_string()),
            ..Default::default()
        };
        let resolved = resolve_selection_endpoint(&selection).unwrap();
        assert_eq!(resolved.protocol, ImageApiProtocol::Codex);
        assert_eq!(resolved.model, "gpt-6-sol");
        assert!(resolved.api_key.is_empty());
        assert_eq!(
            resolved.base_url,
            tiangong_llm::providers::codex::CODEX_BASE_URL
        );

        let default_model = resolve_selection_endpoint(&ConfigSelection {
            source: ModelSource::Chatgpt,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(default_model.model, DEFAULT_CHATGPT_MODEL);
    }

    #[test]
    fn manual_selection_keeps_protocol_and_rejects_codex() {
        let mut selection = ConfigSelection {
            source: ModelSource::Manual,
            manual_endpoint: tiangong_plugin_generate_image_openai_protocol::ManualEndpoint {
                base_url: "https://example.com/v1".to_string(),
                api_key: "sk-test".to_string(),
                model: "img".to_string(),
                protocol: ImageApiProtocol::ChatCompletions,
            },
            ..Default::default()
        };
        let resolved = resolve_selection_endpoint(&selection).unwrap();
        assert_eq!(resolved.protocol, ImageApiProtocol::ChatCompletions);
        selection.manual_endpoint.protocol = ImageApiProtocol::Codex;
        assert!(resolve_selection_endpoint(&selection).is_err());
    }

    #[test]
    fn legacy_config_defaults_to_responses() {
        let legacy: ImageGenConfig = serde_json::from_value(json!({
            "source": "manual",
            "manual_endpoint": { "base_url": "https://x/v1", "api_key": "k", "model": "m" },
            "resolved": { "base_url": "https://x/v1", "api_key": "k", "model": "m" }
        }))
        .unwrap();
        assert_eq!(legacy.resolved.protocol, ImageApiProtocol::Responses);
        assert_eq!(legacy.manual_endpoint.protocol, ImageApiProtocol::Responses);
    }

    #[test]
    fn codex_request_uses_message_list_and_stream() {
        let body =
            build_codex_request("一只猫", "gpt-5.5", &ImageGenConfig::default(), &[]).unwrap();
        assert!(body["input"].is_array(), "Codex 要求 input 为数组");
        assert_eq!(body["input"][0]["content"][0]["text"], "一只猫");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "");
        assert_eq!(body["tools"][0]["type"], "image_generation");
    }

    #[test]
    fn chat_request_carries_prompt_and_extra_instructions() {
        let config = ImageGenConfig {
            extra_prompt: Some("写实风格".to_string()),
            ..Default::default()
        };
        let body = build_chat_request("一只猫", "img", &config, &[]).unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "一只猫");
        assert_eq!(body["stream"], false);
    }

    /// 真实 ChatGPT 生图：需设置 `TIANGONG_CODEX_IMAGE_E2E=1`，并以
    /// `TIANGONG_STORAGE_ROOT` 指向含 `auth/codex.json` 与 `generate-image-openai/config.json` 的目录。
    #[tokio::test]
    async fn chatgpt_image_generation_e2e() {
        if std::env::var("TIANGONG_CODEX_IMAGE_E2E").is_err() {
            return;
        }
        let response = generate(GenerateRequest {
            prompt: "a simple red circle on white background".to_string(),
            images: Vec::new(),
        })
        .await
        .expect("ChatGPT 生图失败");
        assert_eq!(response.images.len(), 1);
        let path = &response.images[0].reference;
        assert!(std::path::Path::new(path).is_file(), "图片未归档：{path}");
        let edited = generate(GenerateRequest {
            prompt: "make the circle blue".to_string(),
            images: vec![path.clone()],
        })
        .await
        .expect("ChatGPT 改图失败");
        assert_eq!(edited.images.len(), 1);
        eprintln!("generated={path} edited={}", edited.images[0].reference);
    }
}
