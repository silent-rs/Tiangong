//! 全家桶应用 API（OCR / ASR / TTS / 文件解析 / 对话扩展参数）单元测试。

use crate::testing::{MockServer, http_response, mock_client};
use crate::tts::validate_tts_request;
use crate::types::{
    AsrResponse, ChatTool, CreateResponseRequest, FileParsePurpose, OcrPurpose, OcrResponse,
    ResponsesTool, SafetyCheck, TtsAudioFormat, TtsRequest, TtsVoice,
};
use serde_json::json;

// ── OCR ─────────────────────────────────────────────────────

#[test]
fn ocr_purpose_serialization() {
    assert_eq!(OcrPurpose::TextRecognition.as_str(), "text_recognition");
    assert_eq!(
        OcrPurpose::RichLayoutRecognition.as_str(),
        "rich_layout_recognition"
    );
}

#[tokio::test]
async fn ocr_posts_multipart_with_purpose_before_file() {
    let body = json!({
        "id": "ocr-1", "object": "ocr.result", "created": 1762790000,
        "purpose": "text_recognition", "page_count": 1,
        "pages": [{"index": 0, "text": "你好", "finish_reason": "stop"}],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
    });
    let server = MockServer::start(vec![http_response(
        "200 OK",
        "application/json",
        // 前导换行（识别期间的 keep-alive）应被 JSON 解析器跳过
        format!("\n\n{}", body).as_bytes(),
    )]);
    let client = mock_client(&server.addr);
    let result: OcrResponse = client
        .ocr()
        .recognize("doc.pdf", b"%PDF-1.4".to_vec(), OcrPurpose::TextRecognition)
        .await
        .unwrap();
    assert_eq!(result.page_count, 1);
    assert_eq!(result.pages[0].text, "你好");

    let (request_line, sent) = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(
        request_line.contains("POST /v1/tools/ocr"),
        "{request_line}"
    );
    let body = String::from_utf8_lossy(&sent);
    // purpose 必须位于 file 之前
    let purpose_pos = body.find("name=\"purpose\"").expect("purpose field");
    let file_pos = body.find("name=\"file\"").expect("file field");
    assert!(purpose_pos < file_pos);
    assert!(body.contains("text_recognition"));
    assert!(body.contains("filename=\"doc.pdf\""));
}

#[tokio::test]
async fn ocr_rejects_oversized_file_locally() {
    let client = mock_client("127.0.0.1:1");
    let oversize = vec![0u8; 100 * 1024 * 1024 + 1];
    let err = client
        .ocr()
        .recognize("big.pdf", oversize, OcrPurpose::TextRecognition)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too large"), "{err}");
}

// ── ASR ─────────────────────────────────────────────────────

#[tokio::test]
async fn asr_posts_wav_and_parses_response() {
    let server = MockServer::start(vec![http_response(
        "200 OK",
        "application/json",
        r#"{"request_id":"req-1","text":"上海今天天气怎么样？","duration_ms":3200}"#.as_bytes(),
    )]);
    let client = mock_client(&server.addr);
    let mut wav = b"RIFF\x00\x00\x00\x00WAVEfmt ".to_vec();
    wav.extend_from_slice(&[0u8; 16]);
    let response: AsrResponse = client.asr().transcribe("speech.wav", wav).await.unwrap();
    assert_eq!(response.text, "上海今天天气怎么样？");
    assert_eq!(response.duration_ms, 3200);

    let (request_line, sent) = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(
        request_line.contains("POST /v1/tools/asr"),
        "{request_line}"
    );
    let body = String::from_utf8_lossy(&sent);
    assert!(body.contains("name=\"file\""));
    assert!(body.contains("filename=\"speech.wav\""));
}

#[tokio::test]
async fn asr_rejects_unsupported_magic_and_oversize() {
    let client = mock_client("127.0.0.1:1");
    let err = client
        .asr()
        .transcribe("note.txt", b"plain text".to_vec())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("WAV or Ogg"), "{err}");

    let mut ogg = b"OggS".to_vec();
    ogg.extend(std::iter::repeat_n(0u8, 8 * 1024 * 1024));
    let err = client.asr().transcribe("big.ogg", ogg).await.unwrap_err();
    assert!(err.to_string().contains("too large"), "{err}");
}

// ── TTS ─────────────────────────────────────────────────────

#[test]
fn tts_request_serialization_and_validation() {
    let request = TtsRequest::new("你好，我是 DeepSeek。")
        .voice(TtsVoice::Tide)
        .format(TtsAudioFormat::Pcm)
        .filter_markdown(false);
    let value = json!(&request);
    assert_eq!(value["voice_id"], "tide");
    assert_eq!(value["format"], "pcm");
    assert_eq!(value["filter_markdown"], false);

    assert!(validate_tts_request(&TtsRequest::new("hello")).is_ok());
    assert!(validate_tts_request(&TtsRequest::new("   ")).is_err());
    // 汉字计 2 字符：5001 个汉字超 10000 计费字符
    let cjk_over = "汉".repeat(5001);
    assert!(validate_tts_request(&TtsRequest::new(cjk_over)).is_err());
    let cjk_max = "汉".repeat(5000);
    assert!(validate_tts_request(&TtsRequest::new(cjk_max)).is_ok());
}

#[test]
fn tts_ws_url_builds_from_v1_base() {
    let client = crate::DeepSeekClient::builder("k")
        .base_url("https://api.deepseek.com/v1")
        .build()
        .unwrap();
    let url = client.client_test_ws_url();
    assert_eq!(
        url,
        "wss://api.deepseek.com/v1/tools/tts?voice_id=stella&format=pcm"
    );
}

#[tokio::test]
async fn tts_http_streams_audio_bytes() {
    let response_head =
        b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    // chunked 音频：两个数据块
    let mut response = response_head.to_vec();
    response.extend_from_slice(b"4\r\nRIFF\r\n5\r\nWAVE\x00\r\n0\r\n\r\n");
    let server = MockServer::start(vec![response]);
    let client = mock_client(&server.addr);
    let audio = client
        .tts()
        .synthesize(TtsRequest::new("你好"))
        .await
        .unwrap();
    assert_eq!(audio, b"RIFFWAVE\x00");

    let (request_line, sent) = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(
        request_line.contains("POST /v1/tools/tts"),
        "{request_line}"
    );
    let body = String::from_utf8_lossy(&sent);
    assert!(body.contains("\"text\":\"你好\""));
}

// ── Files 文档解析 ──────────────────────────────────────────

#[test]
fn file_parse_purpose_serialization() {
    assert_eq!(
        FileParsePurpose::TextRecognition.as_str(),
        "text_recognition"
    );
    assert_eq!(
        FileParsePurpose::RichLayoutRecognition.as_str(),
        "rich_layout_recognition"
    );
}

#[tokio::test]
async fn files_upload_document_purpose_precedes_file() {
    let server = MockServer::start(vec![http_response(
        "200 OK",
        "application/json",
        br#"{"id":"file-api-1","object":"file","bytes":4,"created_at":1762790000,
            "filename":"a.pdf","purpose":"text_recognition"}"#,
    )]);
    let client = mock_client(&server.addr);
    let file = client
        .files()
        .upload_document(
            "a.pdf",
            b"%PDF1".to_vec(),
            FileParsePurpose::TextRecognition,
        )
        .await
        .unwrap();
    assert_eq!(file.id, "file-api-1");
    assert_eq!(file.purpose, "text_recognition");

    let (request_line, sent) = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(request_line.contains("POST /files"), "{request_line}");
    let body = String::from_utf8_lossy(&sent);
    let purpose_pos = body.find("name=\"purpose\"").expect("purpose field");
    let file_pos = body.find("name=\"file\"").expect("file field");
    assert!(purpose_pos < file_pos);
}

// ── Responses 对话扩展 ──────────────────────────────────────

#[test]
fn responses_server_tools_and_safety_check_serialize() {
    let request = CreateResponseRequest {
        model: "deepseek-flash".into(),
        tools: Some(vec![
            ResponsesTool::DeepSeekWebSearch,
            ResponsesTool::DeepSeekReadUrl,
        ]),
        safety_check: Some(SafetyCheck::high()),
        ..Default::default()
    };
    let value = json!(&request);
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0], json!({"type": "deepseek_web_search"}));
    assert_eq!(tools[1], json!({"type": "deepseek_read_url"}));
    assert_eq!(value["safety_check"], json!({"level": "high"}));
}

#[test]
fn responses_usage_parses_server_tool_use_and_safety_tokens() {
    let payload = json!({
        "input_tokens": 1905,
        "output_tokens": 72,
        "total_tokens": 1977,
        "output_tokens_details": {"reasoning_tokens": 10, "safety_tokens": 3},
        "server_tool_use": {
            "web_search_requests": 1, "web_fetch_requests": 0,
            "tool_search": 2, "tool_fetch": 0, "tool_image_search": 0
        }
    });
    let usage: crate::types::ResponseUsage = serde_json::from_value(payload).unwrap();
    let server_tool_use = usage.server_tool_use.expect("server_tool_use");
    assert_eq!(server_tool_use.tool_search, Some(2));
    assert_eq!(usage.output_tokens_details.unwrap().safety_tokens, Some(3));
}

#[test]
fn responses_input_file_block_serializes() {
    let block = crate::types::ContentBlock::InputFile(crate::types::InputFileBlock {
        file_id: "file-api-1".into(),
    });
    assert_eq!(
        json!(&block),
        json!({"type": "input_file", "file_id": "file-api-1"})
    );
}

#[test]
fn chat_completion_response_parses_server_tool_calls() {
    let payload = json!({
        "id": "chat-1", "object": "chat.completion", "created": 1762790000,
        "model": "deepseek-flash",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "这篇文章主要讲……",
                "annotations": [{
                    "type": "url_citation",
                    "url_citation": {"url": "https://example.com/post", "title": "标题",
                                     "start_index": 0, "end_index": 10}
                }],
                "server_tool_calls": [{
                    "index": 0,
                    "reasoning_content": "",
                    "calls": [{
                        "index": 0, "id": "srvtoolu_rl_00_1", "type": "deepseek_read_url",
                        "kind": "read_link",
                        "action": {"type": "read_link", "url": "https://example.com/post"},
                        "status": "completed",
                        "page": {"url": "https://example.com/post", "title": "标题",
                                 "published_at": "2026-01-02"},
                        "signature": "c2ln..."
                    }]
                }]
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3,
                  "completion_tokens_details": {"reasoning_tokens": 0, "safety_tokens": 1},
                  "server_tool_use": {"tool_search": 1}}
    });
    let response: crate::types::ChatCompletionResponse = serde_json::from_value(payload).unwrap();
    let message = &response.choices[0].message;
    let annotation = message.annotations.as_ref().unwrap()[0].clone();
    assert_eq!(annotation.url_citation.url, "https://example.com/post");
    let step = message.server_tool_calls.as_ref().unwrap()[0].clone();
    let call = step.calls[0].clone();
    assert_eq!(call.kind, "deepseek_read_url");
    assert_eq!(call.tool_kind.as_deref(), Some("read_link"));
    assert_eq!(call.signature.as_deref(), Some("c2ln..."));
    assert_eq!(
        response.usage.server_tool_use.as_ref().unwrap().tool_search,
        Some(1)
    );
    assert!(matches!(
        ChatTool::DeepSeekWebSearch,
        ChatTool::DeepSeekWebSearch
    ));
}
