//! 联通测试（#662）：进程环境中的 HTTP 代理已不可达（如 VPN 关闭后）时，
//! 各协议的对话、模型列表、Embedding、Rerank 请求均应直连重试成功。
//!
//! 测试会修改进程环境变量，本文件只放一个测试函数，避免与其他测试并发读写环境。

use std::time::Duration;

use serde_json::json;
use tiangong_llm::ModelEndpoint;
use tiangong_llm::SingleProviderClient;
use tiangong_llm::embedding::{EmbeddingEndpointConfig, embedding_provider_from_config};
use tiangong_llm::message::{ChatMessage, MessageRole};
use tiangong_llm::model::ProviderProtocol;
use tiangong_llm::provider_client::ModelRequest;
use tiangong_llm::rerank::{RerankEndpointConfig, RerankRequest};
use tiangong_llm::rerank_provider_from_config;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 端口 9（discard）通常无人监听，模拟 VPN 关闭后失效的代理。
const DEAD_PROXY: &str = "http://127.0.0.1:9";

fn set_dead_proxy_env() {
    // SAFETY: 本测试二进制只有一个测试函数，修改环境时没有其他线程读取环境变量。
    unsafe {
        for key in [
            "NO_PROXY",
            "no_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::remove_var(key);
        }
        std::env::set_var("HTTP_PROXY", DEAD_PROXY);
        std::env::set_var("http_proxy", DEAD_PROXY);
    }
}

fn endpoint(server: &MockServer, protocol: ProviderProtocol) -> ModelEndpoint {
    ModelEndpoint {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        protocol,
        ..Default::default()
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        system: "系统提示".to_string(),
        messages: vec![ChatMessage::text(MessageRole::User, "回答 OK")],
        max_output_tokens: Some(16),
        ..Default::default()
    }
}

fn chat_reply(protocol: ProviderProtocol) -> serde_json::Value {
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

fn models_reply(protocol: ProviderProtocol) -> serde_json::Value {
    match protocol {
        ProviderProtocol::DeepSeek => json!({
            "object":"list","data":[{"id":"test-model","object":"model","owned_by":"test"}]
        }),
        _ => json!({"data":[{"id":"test-model"}]}),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 代理不可达时各类请求直连重试成功() {
    set_dead_proxy_env();

    // 对照：遵循环境变量代理的普通客户端确实连不上。
    let probe = MockServer::start().await;
    let error = reqwest::Client::new()
        .get(probe.uri())
        .send()
        .await
        .expect_err("经失效代理的请求应失败");
    assert!(error.is_connect(), "{error}");

    for protocol in [
        ProviderProtocol::Anthropic,
        ProviderProtocol::DeepSeek,
        ProviderProtocol::OpenAiChatCompletions,
        ProviderProtocol::OpenAi,
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_reply(protocol)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(models_reply(protocol)))
            .mount(&server)
            .await;
        let cfg = endpoint(&server, protocol);
        let response = SingleProviderClient::new(cfg.clone())
            .complete_async(request())
            .await
            .unwrap_or_else(|error| panic!("{protocol:?} 对话应直连成功：{error:#}"));
        assert_eq!(response.text, "OK", "{protocol:?}");
        let models = SingleProviderClient::list_models_async(&cfg)
            .await
            .unwrap_or_else(|error| panic!("{protocol:?} 模型列表应直连成功：{error:#}"));
        assert_eq!(models, vec!["test-model".to_string()], "{protocol:?}");
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"embedding":[0.5],"index":0}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"results":[{"index":0,"relevance_score":0.9}]})),
        )
        .mount(&server)
        .await;
    let embedding = embedding_provider_from_config(&EmbeddingEndpointConfig {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "embedding".into(),
        protocol: ProviderProtocol::OpenAiChatCompletions,
        timeout: Duration::from_secs(10),
        dimension: 1,
    })
    .unwrap();
    let vectors = embedding
        .embed(vec!["天工".into()])
        .await
        .expect("Embedding 应直连成功");
    assert_eq!(vectors, vec![vec![0.5]]);
    let rerank = rerank_provider_from_config(&RerankEndpointConfig {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "rerank".into(),
        protocol: ProviderProtocol::OpenAiChatCompletions,
        timeout: Duration::from_secs(10),
    })
    .unwrap();
    let ranked = rerank
        .rerank(RerankRequest {
            query: "天工".into(),
            documents: vec!["甲".into()],
            top_n: 1,
        })
        .await
        .expect("Rerank 应直连成功");
    assert_eq!(ranked.results.len(), 1);
}
