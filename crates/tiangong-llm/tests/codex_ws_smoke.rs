//! Codex 直连 WS 通路的手动冒烟（默认 `#[ignore]`，不参与常规测试）。
//!
//! Codex provider 现在只有 WS 一条请求通道（连接失败/首帧前断开在 WS 上重建重试，
//! 不降级 HTTP）。本测试用真实凭据打一次极短请求，确认 WS 通路端到端可用。
//!
//! ```bash
//! TIANGONG_STORAGE_ROOT=/tmp/tiangong-root \
//! cargo test -p tiangong-llm --test codex_ws_smoke -- --ignored --nocapture
//! ```
//!
//! 凭据：`$TIANGONG_STORAGE_ROOT/auth/codex.json`（默认 `/tmp/tiangong-root`）。
use tiangong_llm::{ModelEndpoint, ProviderProtocol, SingleProviderClient};

fn storage_root() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("TIANGONG_STORAGE_ROOT").unwrap_or_else(|_| "/tmp/tiangong-root".to_string()),
    )
}

#[test]
#[ignore = "需要真实 Codex 登录态与联网的手动冒烟"]
fn codex_ws_stream_roundtrip() {
    let root = storage_root();
    assert!(
        root.join("auth/codex.json").exists(),
        "缺少 Codex 凭据：{}",
        root.join("auth/codex.json").display()
    );
    // SAFETY: 单线程测试，在构造任何 provider（读取存储根）之前设置。
    unsafe { std::env::set_var("TIANGONG_STORAGE_ROOT", &root) };

    let endpoint = ModelEndpoint {
        base_url: tiangong_llm::providers::codex::CODEX_BASE_URL.to_string(),
        model: "gpt-5.6-terra".to_string(),
        protocol: ProviderProtocol::Codex,
        timeout_ms: 120_000,
        ..Default::default()
    };
    let client = SingleProviderClient::new(endpoint);
    let reply = client
        .complete_lite_with_system("简短回答", "只回复两个字：你好")
        .expect("WS 通路应返回内容");
    println!("reply: {reply:?}");
    assert!(!reply.trim().is_empty(), "WS 通路返回空内容");
}
