//! 手动整理上下文（`/compress`）的端到端测试。
//!
//! 关注点是**等待终态**这一语义：与模型切换前的整理走同一条实现
//! （`TiangongCore::compact_context`），因此调用返回时压缩必然已有结论
//! ——成功时摘要边界已推进并落盘，失败时如实回报而不是"投递成功"。

use std::sync::Arc;

use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
use tiangong_core::core::Plugin;
use tiangong_core::permission::TrustMode;
use tiangong_core::session::{MessageRole, Session};
use tiangong_core_manager::{ContextCompactionError, CoreManager};
use tiangong_types::StreamEvent;
use tiangong_types::stream::ContextCompressAction;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn manager_at(dir: &tempfile::TempDir) -> CoreManager {
    CoreManager::new(
        CoreConfigProvider::new(CoreConfig::default()),
        dir.path().to_path_buf(),
    )
}

fn seed_models(dir: &tempfile::TempDir, base_url: &str) {
    let config = serde_json::json!({
        "providers": {
            "p": {
                "base_url": base_url,
                "api_key": "test-key",
                "timeout_ms": 60000,
                "protocol": "openai_chat_completions",
                "headers": {}
            }
        },
        "models": {
            "model-a": {
                "provider": "p",
                "model": "model-a",
                "capabilities": ["chat"]
            }
        },
        "routing": { "chat": "model-a" }
    });
    std::fs::write(
        dir.path().join("models.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
}

/// 预置带两轮问答历史的会话（压缩需要可折叠的历史）。
fn seed_session(dir: &tempfile::TempDir, id: &str) {
    let mut session = Session::new("整理测试");
    session.id = id.to_string();
    session.bind_storage_root(dir.path().to_path_buf());
    for round in [("第一问", "第一答"), ("第二问", "第二答")] {
        session.append_message(MessageRole::User, round.0);
        session.append_message(MessageRole::Assistant, round.1);
    }
    session.try_persist_to_disk().unwrap();
}

async fn make_core(manager: &CoreManager, id: &str) {
    drop(make_core_with_stream(manager, id).await);
}

/// 创建 Core 并返回其会话事件流接收端（用于断言事件顺序）。
async fn make_core_with_stream(
    manager: &CoreManager,
    id: &str,
) -> std::sync::mpsc::Receiver<StreamEvent> {
    let (stream_tx, stream_rx) = std::sync::mpsc::channel();
    manager
        .ensure_core(
            id,
            CoreConfig::builder()
                .with_trust_mode(TrustMode::FullTrust)
                .build(),
            "/tmp".to_string(),
            // initial_model_ref：测试会话不指定初始模型，按路由默认解析。
            None,
            stream_tx,
            Vec::<Arc<dyn Plugin>>::new,
        )
        .await
        .expect("ensure_core 失败");
    stream_rx
}

fn summary_reply() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": "[[SUMMARY]]\n整理后的摘要"}}],
        "usage": {"prompt_tokens": 40, "completion_tokens": 8, "total_tokens": 48}
    }))
}

/// 返回时压缩必须已完成落盘——调用方紧接着读会话就能看到结果，
/// 不需要轮询等待。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 手动整理返回时摘要已落盘() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(summary_reply())
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri());
    seed_session(&dir, "compact-ok");
    let manager = manager_at(&dir);
    make_core(&manager, "compact-ok").await;

    manager
        .compact_session_context("compact-ok")
        .await
        .expect("整理应成功");

    // 不 sleep、不轮询：返回即终态。
    let session = manager.load_session("compact-ok").unwrap();
    assert_eq!(
        session.context_summary.as_deref(),
        Some("整理后的摘要"),
        "返回时摘要应已落盘"
    );
    assert!(session.summary_up_to > 0, "摘要边界应推进");
    manager.retire_core("compact-ok", true).await.unwrap();
}

/// 压缩失败如实回报，而不是像即发即走那样返回"已投递"。
///
/// 回归：改造前命令只表示投递成功，模型调用失败时用户看到的是成功提示，
/// 历史却没有变化。
///
/// 同时锁定事件流契约：返回 `Failed` 时终态事件 `ContextCompressed { Failed }`
/// 已写入事件流，且是本次压缩的最后一个事件（用量、通知都在它之前）——
/// 前端据此收尾运行态，不会出现终态之后又有事件把会话拉回执行中。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 压缩失败如实回报错误() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri());
    seed_session(&dir, "compact-fail");
    let manager = manager_at(&dir);
    let stream_rx = make_core_with_stream(&manager, "compact-fail").await;

    let error = manager
        .compact_session_context("compact-fail")
        .await
        .expect_err("模型失败时整理必须回报错误");
    let ContextCompactionError::Failed(reason) = &error else {
        panic!("模型失败应归为已进入终态的 Failed：{error:?}");
    };
    assert!(!reason.is_empty(), "错误信息不应为空");

    let events: Vec<StreamEvent> = stream_rx.try_iter().collect();
    let last = events.last().expect("压缩应产生事件");
    assert!(
        matches!(
            last,
            StreamEvent::ContextCompressed {
                action: ContextCompressAction::Failed,
                ..
            }
        ),
        "返回时终态事件已入流且位于最后：{events:?}"
    );

    // 失败不得留下半截状态。
    let session = manager.load_session("compact-fail").unwrap();
    assert_eq!(session.summary_up_to, 0, "失败时摘要边界不得推进");
    manager.retire_core("compact-fail", true).await.unwrap();
}

/// 压缩进行中被取消：返回 `Interrupted`（不是失败），终态事件
/// `ContextCompressed { Cancelled }` 已入流且位于最后。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 压缩中取消返回中断且终态事件已入流() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(summary_reply().set_delay(std::time::Duration::from_secs(30)))
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri());
    seed_session(&dir, "compact-cancel");
    let manager = manager_at(&dir);
    let stream_rx = make_core_with_stream(&manager, "compact-cancel").await;

    let compaction = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.compact_session_context("compact-cancel").await })
    };
    // 等到模型请求真正发出（压缩处于进行中）再取消。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while server.received_requests().await.unwrap().is_empty() {
        assert!(std::time::Instant::now() < deadline, "压缩请求未发出");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(manager.cancel_core("compact-cancel"), "压缩进行中应可取消");

    let error = tokio::time::timeout(std::time::Duration::from_secs(10), compaction)
        .await
        .expect("取消后整理必须及时结束")
        .unwrap()
        .expect_err("被取消的整理不应返回成功");
    assert_eq!(error, ContextCompactionError::Interrupted);

    let events: Vec<StreamEvent> = stream_rx.try_iter().collect();
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::ContextCompressed {
                action: ContextCompressAction::Cancelled,
                ..
            })
        ),
        "返回时取消终态已入流且位于最后：{events:?}"
    );
    manager.retire_core("compact-cancel", true).await.unwrap();
}

/// 无可压缩历史时直接成功，且不调用模型。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 无可压缩历史时直接成功且零模型调用() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    seed_models(&dir, &server.uri());
    let mut session = Session::new("空会话");
    session.id = "compact-empty".to_string();
    session.bind_storage_root(dir.path().to_path_buf());
    session.try_persist_to_disk().unwrap();
    let manager = manager_at(&dir);
    make_core(&manager, "compact-empty").await;

    manager
        .compact_session_context("compact-empty")
        .await
        .expect("无历史时整理应直接成功");
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "无可压缩历史不得调用模型"
    );
    manager.retire_core("compact-empty", true).await.unwrap();
}

/// 会话无活跃 Core 时给出明确错误，而不是静默返回 false。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 无活跃core时报明确错误() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(&dir);
    let error = manager
        .compact_session_context("never-created")
        .await
        .expect_err("无 Core 时应报错");
    let ContextCompactionError::NotStarted(reason) = &error else {
        panic!("无 Core 时整理未启动，事件流不会有终态：{error:?}");
    };
    assert!(reason.contains("Core"), "错误应说明 Core 不存在：{error}");
}
