//! 模型请求组装快照（issue #691）。
//!
//! 同一组会话上下文经 core 组装、llm 协议适配后，各协议实际发出的 HTTP
//! 请求体必须与 `tests/snapshots/request_assembly/` 下的基准逐字节一致。
//! 基准由迁移前（llm 层自行组装会话消息）的实现录制，证明组装职责迁移到
//! core（`ModelContext` → `ModelRequest` 转换）后请求内容（含缓存前缀）不变。
//!
//! 有意调整请求内容时，用 `UPDATE_REQUEST_SNAPSHOTS=1` 运行本测试重录基准，
//! 并在评审中说明差异。

use std::path::PathBuf;

use serde_json::{Value, json};
use tiangong_core::context::model_context::ModelContext;
use tiangong_core::session::{
    ContentBlock, MediaKind, Message, MessageRole, MessageToolCall, Role, StoredAsset, UserSource,
};
use tiangong_llm::tool::ToolSpec;
use tiangong_llm::{
    ModelEndpoint, ModelRequest, ProviderProtocol, ReasoningEffort, SingleProviderClient,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const MISSING_IMAGE_PATH: &str = "/nonexistent/tiangong-request-snapshot/missing.png";

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/request_assembly")
}

fn asset(asset_id: &str, kind: MediaKind, local_path: &str, mime_type: &str) -> StoredAsset {
    StoredAsset {
        asset_id: asset_id.to_string(),
        local_path: local_path.to_string(),
        original_name: format!("{asset_id}.bin"),
        mime_type: mime_type.to_string(),
        size: 4,
        kind,
    }
}

fn system(text: &str) -> Message {
    Message::with_role(Role::System, vec![ContentBlock::text(text)])
}

fn user(content: Vec<ContentBlock>) -> Message {
    Message::with_role(Role::user(), content)
}

fn assistant_with_calls(
    text: &str,
    reasoning: &str,
    signature: Option<&str>,
    calls: &[(&str, &str)],
) -> Message {
    let mut message = Message::with_reasoning(MessageRole::Assistant, text, reasoning);
    if let Role::Assistant {
        reasoning_signature,
        ..
    } = &mut message.role
    {
        *reasoning_signature = signature.map(str::to_string);
    }
    message.with_tool_calls(
        calls
            .iter()
            .map(|(id, name)| MessageToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: json!({"path": id}),
            })
            .collect(),
    )
}

/// 各快照场景：名称 + 会话上下文（`session.context()` 的形态）。
fn cases(image_path: &str) -> Vec<(&'static str, Vec<Message>)> {
    vec![
        (
            // 文本、思考、并行工具调用、模型指令、附件引用与旧格式媒体。
            "conversation",
            vec![
                system("  你是通用助手。\n遵守规则。  "),
                user(vec![ContentBlock::text("请读取两个文件")]),
                assistant_with_calls(
                    "我来读取。",
                    "  先读 a 再读 b\n保留原文  ",
                    Some("sig-1"),
                    &[("call_a", "read_file"), ("call_b", "read_file")],
                ),
                Message::tool_result("call_a", "read_file", "内容 A", false),
                Message::tool_result("call_b", "read_file", "读取失败", true),
                assistant_with_calls("两个文件都已读取。", "", None, &[]),
                user(vec![
                    ContentBlock::text("处理附件"),
                    ContentBlock::AssetReference {
                        asset: asset(
                            "asset-pdf",
                            MediaKind::File,
                            "/tmp/report.pdf",
                            "application/pdf",
                        ),
                    },
                    ContentBlock::model_instruction("  附件路径 path=/tmp/report.pdf\n"),
                    ContentBlock::Media {
                        kind: MediaKind::Image,
                        url: "data:image/png;base64,SHOULD_NOT_INLINE".to_string(),
                        mime_type: Some("image/png".to_string()),
                        title: None,
                    },
                ]),
                Message::with_role(
                    Role::Notice { usage: None },
                    vec![ContentBlock::text("通知不进模型")],
                ),
                assistant_with_calls("", "", None, &[("call_dangling", "read_file")]),
            ],
        ),
        (
            // 本地可读图片、当前请求内联数据、远程 URL 与缺失文件降级。
            "images",
            vec![
                system("你是看图助手。"),
                user(vec![
                    ContentBlock::text("看看这些图"),
                    ContentBlock::Image {
                        asset: asset("asset-local", MediaKind::Image, image_path, "image/png"),
                        data: None,
                    },
                    ContentBlock::Image {
                        asset: asset(
                            "asset-runtime",
                            MediaKind::Image,
                            MISSING_IMAGE_PATH,
                            "image/jpeg",
                        ),
                        data: Some("UlVOVElNRQ==".to_string()),
                    },
                    ContentBlock::Image {
                        asset: asset(
                            "asset-remote",
                            MediaKind::Image,
                            "  https://example.com/remote.png  ",
                            "image/png",
                        ),
                        data: None,
                    },
                ]),
                assistant_with_calls("", "", None, &[("call_shot", "desktop_screenshot")]),
                Message::tool_result("call_shot", "desktop_screenshot", "{\"ok\":true}", false),
                user(vec![
                    ContentBlock::model_instruction("[injected-images provenance]\n- 截图"),
                    ContentBlock::Image {
                        asset: asset(
                            "asset-missing",
                            MediaKind::Image,
                            "/nonexistent/tiangong-request-snapshot/\u{202E}gnp.missing\nline",
                            "image/png",
                        ),
                        data: None,
                    },
                ])
                .with_source(UserSource::HostInjected),
            ],
        ),
        (
            // summary 截断后以 Assistant/Tool 开头：跳过到首条 User。
            "leading_non_user",
            vec![
                system("系统提示"),
                assistant_with_calls("截断前的回答", "", None, &[("call_old", "read_file")]),
                Message::tool_result("call_old", "read_file", "旧结果", false),
                user(vec![ContentBlock::text("继续")]),
            ],
        ),
        (
            // 只有 System：补兜底用户消息。
            "empty_context",
            vec![system("系统提示")],
        ),
        (
            // 空 system 文本与只含空白的用户消息：请求不带 system，补兜底。
            "blank_system_and_user",
            vec![
                system("   "),
                user(vec![ContentBlock::text("   ")]),
                Message::with_role(
                    Role::Notice { usage: None },
                    vec![ContentBlock::text("通知")],
                ),
            ],
        ),
    ]
}

fn reply(protocol: ProviderProtocol) -> Value {
    match protocol {
        ProviderProtocol::Anthropic => json!({
            "id":"m","type":"message","role":"assistant","model":"test-model",
            "content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        }),
        ProviderProtocol::OpenAi => json!({
            "id":"resp","status":"completed","model":"test-model",
            "output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}],
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }),
        _ => json!({
            "id":"chat","created":0,"object":"chat.completion","model":"test-model",
            "choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"OK"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }),
    }
}

fn protocol_name(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::Anthropic => "anthropic",
        ProviderProtocol::OpenAi => "openai_responses",
        ProviderProtocol::OpenAiChatCompletions => "openai_chat",
        ProviderProtocol::DeepSeek => "deepseek",
        ProviderProtocol::Codex => "codex",
    }
}

fn build_request(context: &[Message]) -> ModelRequest {
    ModelRequest {
        session_id: Some("snapshot-session".to_string()),
        reasoning_effort: ReasoningEffort::High,
        max_output_tokens: Some(1024),
        temperature: Some(0.2),
        timeout_ms: Some(5_000),
        tools: vec![ToolSpec {
            name: "read_file".to_string(),
            description: "读取文件".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }],
        ..ModelRequest::try_from(ModelContext::from(context.to_vec())).unwrap()
    }
}

#[tokio::test]
async fn provider_request_bodies_match_snapshots() {
    let image_path = std::env::temp_dir().join(format!(
        "tiangong-request-snapshot-{}-{}.png",
        std::process::id(),
        scru128::new()
    ));
    std::fs::write(&image_path, [1_u8, 2, 3, 4]).unwrap();
    let update = std::env::var_os("UPDATE_REQUEST_SNAPSHOTS").is_some();
    let mut mismatches = Vec::new();

    for protocol in [
        ProviderProtocol::OpenAiChatCompletions,
        ProviderProtocol::OpenAi,
        ProviderProtocol::Anthropic,
        ProviderProtocol::DeepSeek,
    ] {
        for (case, context) in cases(image_path.to_str().unwrap()) {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(reply(protocol)))
                .mount(&server)
                .await;
            let client = SingleProviderClient::new(ModelEndpoint {
                base_url: server.uri(),
                api_key: "test-key".into(),
                model: "test-model".into(),
                protocol,
                ..Default::default()
            });
            client
                .complete_async(build_request(&context))
                .await
                .unwrap_or_else(|error| panic!("{case}/{protocol:?} 请求失败：{error}"));
            let requests = server.received_requests().await.unwrap();
            let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
            let actual = serde_json::to_string_pretty(&body).unwrap() + "\n";

            let path = snapshot_dir().join(format!("{}__{case}.json", protocol_name(protocol)));
            if update {
                std::fs::create_dir_all(snapshot_dir()).unwrap();
                std::fs::write(&path, &actual).unwrap();
                continue;
            }
            let expected = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("缺少快照 {}：{error}", path.display()));
            if expected != actual {
                mismatches.push(format!(
                    "{}\n--- expected\n{expected}\n--- actual\n{actual}",
                    path.display()
                ));
            }
        }
    }
    let _ = std::fs::remove_file(&image_path);
    assert!(
        mismatches.is_empty(),
        "请求体与快照不一致：\n{}",
        mismatches.join("\n")
    );
}
