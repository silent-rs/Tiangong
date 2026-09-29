//! Analyze-Attachment sidecar 业务服务。
//!
//! 读取图片文件 → 构造多模态 ModelRequest → 调 SingleProviderClient → 返回分析文本。

use anyhow::{Context, Result};
use tiangong_core::session::{Message, MessageRole};
use tiangong_llm::{
    ModelCapability, ModelEndpoint, ModelEntry, ModelRequest, ModelsConfig, ResolvedModel,
    RoutingSlot, SingleProviderClient,
};
use tiangong_plugin_analyze_attachment_protocol::{
    ANALYZE_OPERATION, ATTACHMENT_PROTOCOL_VERSION, AnalyzeRequest, AnalyzeResponse, PLUGIN_ID,
    PLUGIN_VERSION,
};
use tiangong_plugin_runtime::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};
use tiangong_types::{ContentBlock, MediaKind, StoredAsset};

pub struct AttachmentService;

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for AttachmentService {
    async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Attachment 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
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
                    error.to_string(),
                    false,
                );
            }
        };
        Response::success(&request_id, payload)
    }
}

async fn dispatch_operation(
    operation: &str,
    payload: serde_json::Value,
) -> Result<serde_json::Value> {
    match operation {
        HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
            plugin_id: PLUGIN_ID.to_string(),
            plugin_version: PLUGIN_VERSION.to_string(),
            sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION.to_string(),
            business_protocol: ATTACHMENT_PROTOCOL_VERSION,
            capabilities: vec!["multimodal".to_string()],
            instance_id: format!("attachment-sidecar-{}", std::process::id()),
            status: ServiceStatus::Ready,
        })
        .context("序列化 Attachment 握手响应失败"),

        ANALYZE_OPERATION => {
            let req: AnalyzeRequest =
                serde_json::from_value(payload).context("解析 analyze 请求失败")?;
            let result = analyze(req).await?;
            serde_json::to_value(result).context("序列化 analyze 响应失败")
        }

        other => Err(anyhow::anyhow!("未知的 Attachment 操作: {other}")),
    }
}

/// 分析附件：读图片 → 构造多模态请求 → 调模型 → 返回分析文本。
async fn analyze(req: AnalyzeRequest) -> Result<AnalyzeResponse> {
    if req.images.is_empty() {
        anyhow::bail!("没有可分析的图片");
    }

    // 从模型配置中挑一个能看图的模型。
    let models = tiangong_plugin_sidecar::model::load_models_config()?;
    let resolved = resolve_multimodal(&models).ok_or_else(|| {
        anyhow::anyhow!("没有可用的多模态模型：请在「设置 → 模型」中为至少一个模型勾选多模态能力")
    })?;
    let model_name = resolved.model.clone();
    let endpoint = ModelEndpoint::from_resolved(resolved);
    let client = SingleProviderClient::new(endpoint);

    // 构造多模态请求上下文。
    let instruction = if req.instruction.trim().is_empty() {
        "请解析附件内容，并提取与用户问题有关的信息。".to_string()
    } else {
        req.instruction
    };

    let mut context = vec![
        Message::new(
            MessageRole::System,
            "你是附件解析助手。只根据随消息提供的附件内容和解析要求回答，输出可供主模型直接使用的简洁中文结果。".to_string(),
        ),
        Message::new(
            MessageRole::Assistant,
            "好的，我将作为附件解析助手，根据附件内容和解析要求进行分析。".to_string(),
        ),
    ];

    let mut user_message = Message::new(
        MessageRole::User,
        format!(
            "用户原始消息：{}\n\n解析要求：{}",
            req.user_message_text.trim(),
            instruction
        ),
    );

    // 读取每张图片，构造 ContentBlock::Image。
    for image_path in &req.images {
        let asset = asset_from_path(image_path)?;
        user_message
            .content
            .push(ContentBlock::Image { asset, data: None });
    }
    context.push(user_message);

    let request = ModelRequest {
        session_id: None,
        user_input: String::new(),
        context,
        reasoning_effort: tiangong_llm::request::ReasoningEffort::None,
        max_output_tokens: None,
        ..Default::default()
    };

    let response = client
        .complete_async(&request)
        .await
        .map_err(|e| anyhow::anyhow!("多模态模型调用失败：{e}"))?;

    Ok(AnalyzeResponse {
        text: response.text,
        prompt_tokens: response.usage.prompt_tokens as u64,
        completion_tokens: response.usage.completion_tokens as u64,
        model: model_name,
    })
}

/// 选出一个可用于图片理解的模型。
///
/// 多模态没有独立路由，按以下顺序取第一个声明了多模态能力且 Provider 仍存在
/// 的模型：chat 路由 → lite 路由 → 模型注册表（按 key 排序，保证结果稳定）。
fn resolve_multimodal(models: &ModelsConfig) -> Option<ResolvedModel> {
    let routed = [RoutingSlot::Chat, RoutingSlot::Lite]
        .into_iter()
        .filter_map(|slot| models.routing.get(&slot));
    let mut registered: Vec<_> = models.models.iter().collect();
    registered.sort_by_key(|(key, _)| *key);
    routed
        .chain(registered.into_iter().map(|(_, entry)| entry))
        .filter(|entry| entry.capabilities.contains(&ModelCapability::Multimodal))
        .find_map(|entry| resolve_entry(models, entry))
}

fn resolve_entry(models: &ModelsConfig, entry: &ModelEntry) -> Option<ResolvedModel> {
    let provider = models.providers.get(&entry.provider)?;
    Some(ResolvedModel {
        headers: provider.headers.clone(),
        provider: entry.provider.clone(),
        base_url: provider.base_url.clone(),
        api_key: ModelsConfig::resolve_api_key(&provider.api_key),
        timeout_ms: provider.timeout_ms,
        protocol: provider.protocol,
        model: entry.model.clone(),
        options: entry.options.clone(),
        context_window: entry.context_window,
    })
}

/// 从图片路径构造 StoredAsset（读取文件元信息推断 MIME）。
fn asset_from_path(path: &str) -> Result<StoredAsset> {
    let path_trimmed = path.trim();
    if path_trimmed.is_empty() {
        anyhow::bail!("图片路径为空");
    }
    let mime_type = infer_image_mime(path_trimmed);
    let metadata = std::fs::metadata(path_trimmed)
        .with_context(|| format!("无法读取图片文件：{path_trimmed}"))?;
    if !metadata.is_file() {
        anyhow::bail!("图片路径不是文件：{path_trimmed}");
    }
    let size = metadata.len();
    let original_name = std::path::Path::new(path_trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image")
        .to_string();
    Ok(StoredAsset {
        asset_id: format!("attachment-{}", scru128::new()),
        local_path: path_trimmed.to_string(),
        original_name,
        mime_type,
        size,
        kind: MediaKind::Image,
    })
}

fn infer_image_mime(path: &str) -> String {
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
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_llm::ProviderConfig;

    fn config() -> ModelsConfig {
        let mut models = ModelsConfig::default();
        models.providers.insert(
            "p".to_string(),
            ProviderConfig {
                headers: Default::default(),
                base_url: "https://api.example.com".to_string(),
                api_key: "k".to_string(),
                timeout_ms: 60_000,
                protocol: Default::default(),
            },
        );
        models.upsert_model("text", "p", "text-model", vec![ModelCapability::Chat]);
        models.set_route_by_name(RoutingSlot::Chat, "text").unwrap();
        models
    }

    #[test]
    fn 非多模态主模型时从注册表选多模态模型() {
        let mut models = config();
        assert!(resolve_multimodal(&models).is_none());

        models.upsert_model(
            "vision-b",
            "p",
            "vision-b-model",
            vec![ModelCapability::Chat, ModelCapability::Multimodal],
        );
        models.upsert_model(
            "vision-a",
            "p",
            "vision-a-model",
            vec![ModelCapability::Chat, ModelCapability::Multimodal],
        );
        assert_eq!(resolve_multimodal(&models).unwrap().model, "vision-a-model");

        models
            .set_route_by_name(RoutingSlot::Lite, "vision-b")
            .unwrap();
        assert_eq!(
            resolve_multimodal(&models).unwrap().model,
            "vision-b-model",
            "路由中的多模态模型优先"
        );
    }
}
