//! Codex（ChatGPT 账号）端到端冒烟：需要本机已登录 Codex CLI（~/.codex/auth.json）。
//!
//! 运行：`cargo run -p tiangong-llm --example codex_smoke`
//! 使用临时 `TIANGONG_STORAGE_ROOT`，不会写入真实 `~/.tiangong`。

use std::path::PathBuf;

use serde_json::Value;
use tiangong_llm::{ModelClient, ModelRequest};
use tiangong_llm::{ModelEndpoint, ProviderProtocol, SingleProviderClient};
use tiangong_types::{Message, MessageRole};

fn main() -> anyhow::Result<()> {
    let home = PathBuf::from(std::env::var("HOME")?);
    let raw = std::fs::read_to_string(home.join(".codex/auth.json"))?;
    let auth: Value = serde_json::from_str(&raw)?;
    let tokens = &auth["tokens"];
    let root = std::env::temp_dir().join(format!("tiangong-codex-smoke-{}", std::process::id()));
    std::fs::create_dir_all(root.join("auth"))?;
    // 过期时间取未来 1 小时，避免冒烟过程中触发刷新（刷新会轮换 Codex CLI 的 refresh token）。
    let creds = serde_json::json!({
        "access_token": tokens["access_token"],
        "refresh_token": "",
        "expires_at": chrono_now() + 3600,
        "account_id": tokens["account_id"],
    });
    std::fs::write(root.join("auth/codex.json"), serde_json::to_vec(&creds)?)?;
    // SAFETY: 单线程示例，在创建任何运行时之前设置。
    unsafe { std::env::set_var("TIANGONG_STORAGE_ROOT", &root) };

    let endpoint = ModelEndpoint {
        base_url: tiangong_llm::providers::codex::CODEX_BASE_URL.to_string(),
        model: "gpt-5.5".to_string(),
        protocol: ProviderProtocol::Codex,
        timeout_ms: 120_000,
        ..Default::default()
    };

    let models = SingleProviderClient::list_models(&endpoint)?;
    println!("models: {models:?}");

    let usage = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(tiangong_llm::providers::codex::usage())?;
    println!(
        "usage: plan={:?} allowed={} windows={:?}",
        usage.plan_type,
        usage.allowed,
        usage
            .windows
            .iter()
            .map(|w| (w.used_percent, w.window_seconds))
            .collect::<Vec<_>>()
    );

    let client = SingleProviderClient::new(endpoint.clone());
    let lite = client.complete_lite_with_system("简短回答", "只回复两个字：你好")?;
    println!("complete(non-stream via stream): {lite:?}");

    let request = ModelRequest {
        context: vec![
            Message::new(MessageRole::System, "需要时间时调用 get_time 工具。"),
            Message::new(MessageRole::User, "现在几点？请调用 get_time 工具"),
        ],
        tools: vec![tiangong_llm::tool::ToolSpec {
            name: "get_time".to_string(),
            description: "获取当前时间".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "tz": { "type": "string" } },
                "required": ["tz"]
            }),
        }],
        max_output_tokens: Some(512),
        temperature: Some(0.3),
        ..Default::default()
    };
    let mut streamed = String::new();
    let response = client.complete_stream(&request, &mut |chunk| {
        streamed.push_str(&chunk.content);
    })?;
    println!(
        "stream: text={:?} tool_calls={:?} usage_in={} out={}",
        response.text,
        response
            .tool_calls
            .iter()
            .map(|c| (&c.name, &c.arguments))
            .collect::<Vec<_>>(),
        response.usage.prompt_tokens,
        response.usage.completion_tokens,
    );
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
