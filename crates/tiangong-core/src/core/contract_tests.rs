//! Inbox、关闭与单执行驱动的内部可靠性契约测试。
//!
//! 完整模型交互场景见 `integration_tests.rs`；本文件只保留不重复的内部契约。

use std::time::{Duration, Instant};

use tiangong_types::TurnStatus;

use crate::agent_input::{AgentInput, AgentInputKind};
use crate::core::TiangongCore;

use super::test_support::{
    MockReply, POLL, PromptRoute, TestEnv, WAIT, latest_user_contains, mount_prompt_router,
    send_message, stream_text_chunks, wait_idle, wait_requests,
};

fn core_for(env: &TestEnv, sid: &str, endpoint: &str) -> TiangongCore {
    let (core, _events) = super::test_support::core_for(env, sid, endpoint);
    core
}

/// 已保存的活动消息在正常关闭后仍必须存在；正常临时存储不接受宽泛错误兜底。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_and_saved_message_survives_shutdown() {
    let (env, sid) = TestEnv::new("survive-a");
    let server = wiremock::MockServer::start().await;
    let routes = mount_prompt_router(
        &server,
        vec![PromptRoute::new(
            "shutdown-active",
            latest_user_contains("SHUTDOWN-ACTIVE"),
            MockReply::delayed_sse(
                stream_text_chunks(&["长时间", "任务执行中"]),
                Duration::from_secs(3),
            ),
        )],
    )
    .await;
    let core = core_for(&env, &sid, &server.uri());

    send_message(&core, "msg-a", "SHUTDOWN-ACTIVE 关闭前正在处理的消息");
    let deadline = Instant::now() + WAIT;
    loop {
        if env
            .load_session(&sid)
            .messages
            .iter()
            .any(|message| message.id == "msg-a")
        {
            break;
        }
        assert!(Instant::now() < deadline, "等待 A 保存超时");
        tokio::time::sleep(POLL).await;
    }
    wait_requests(&server, 1).await;

    core.shutdown_join().expect("正常存储环境下关闭必须成功");
    assert!(
        env.load_session(&sid)
            .messages
            .iter()
            .any(|message| message.id == "msg-a"),
        "已保存的 A 在关闭后必须仍然存在"
    );
    routes["shutdown-active"].assert_hits(1);
}

/// 手动压缩期间的用户消息取消压缩并立即起轮（压缩可随时重新发起）；
/// 其余信号（插件可用性广播等）在压缩状态下不接受、不打断压缩。
/// 压缩请求走非流式 completion，接续消息走 prompt 路由错误响应。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_during_manual_compression_cancels_and_starts_turn() {
    let (env, sid) = TestEnv::new("steer-compress");
    let server = wiremock::MockServer::start().await;
    let routes = mount_prompt_router(
        &server,
        vec![
            PromptRoute::new(
                "deferred-steer",
                latest_user_contains("COMPRESS-DEFERRED-STEER"),
                MockReply::error(400, "deferred steer failed"),
            ),
            PromptRoute::new(
                "manual-compression",
                |request| request.is_compression(),
                MockReply::delayed_completion(
                    "[[SUMMARY]]\n压缩完成的历史摘要",
                    Duration::from_millis(500),
                ),
            ),
        ],
    )
    .await;
    let (core, mut events) = super::test_support::core_for(&env, &sid, &server.uri());

    {
        let mut session = env.load_session(&sid);
        session.append_message(crate::session::MessageRole::User, "第一条问题");
        session.append_message(crate::session::MessageRole::User, "第二条问题");
        session.try_persist_to_disk().expect("预置历史失败");
    }
    core.deliver(AgentInputKind::compress_context())
        .expect("空闲期手动压缩应被接受");
    let deadline = Instant::now() + WAIT;
    while server
        .received_requests()
        .await
        .map_or(0, |requests| requests.len())
        < 1
    {
        assert!(Instant::now() < deadline, "等待手动压缩请求超时");
        tokio::time::sleep(POLL).await;
    }

    let message_id = format!("{sid}-steer");
    core.deliver(AgentInputKind::prepared_with_id(
        message_id.clone(),
        vec![tiangong_types::ContentBlock::text(
            "COMPRESS-DEFERRED-STEER 换个方向处理",
        )],
    ))
    .expect("压缩期间的 steer 应被接受");

    let deadline = Instant::now() + WAIT;
    let status = loop {
        let status = env
            .load_session(&sid)
            .messages
            .iter()
            .find(|message| message.id == message_id)
            .and_then(|message| message.turn_status)
            .filter(|status| status.is_terminal());
        if let Some(status) = status {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "压缩期间的引导消息应取消压缩并立即起轮"
        );
        tokio::time::sleep(POLL).await;
    };
    assert_eq!(
        status,
        TurnStatus::Failed,
        "接续消息的失败响应必须形成 Failed 终态"
    );
    wait_idle(&sid).await;
    events.wait_error_containing("deferred steer failed");
    events.assert_single_failure_terminal("deferred steer failed");
    let session = env.load_session(&sid);
    assert_eq!(
        session.context_summary, None,
        "压缩被用户消息取消，未应用任何结果"
    );
    routes["manual-compression"].assert_hits(1);
    routes["deferred-steer"].assert_hits(1);
    core.shutdown_join().expect("关闭失败");
}
