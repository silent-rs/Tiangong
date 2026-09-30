//! 模型切换编排的端到端测试。
//!
//! 覆盖方案要求的场景：相同模型直接发送、不同模型先压缩再切换、目标
//! 模型不存在、失效引用、压缩失败、切换失败、默认模型变化、Core 重建
//! 后从 Session 恢复。

use std::sync::Arc;

use tiangong_core::agent_input::{AgentInput, AgentInputKind};
use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
use tiangong_core::core::Plugin;
use tiangong_core::permission::TrustMode;
use tiangong_core::session::{MessageRole, Session};
use tiangong_core_manager::CoreManager;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn manager_at(dir: &tempfile::TempDir) -> CoreManager {
    CoreManager::new(
        CoreConfigProvider::new(CoreConfig::default()),
        dir.path().to_path_buf(),
    )
}

/// 写入 models.json：两个模型 a/b 指向同一个 mock server，chat 路由默认指向 a。
fn seed_models(dir: &tempfile::TempDir, base_url: &str, default_key: &str) {
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
            },
            "model-b": {
                "provider": "p",
                "model": "model-b",
                "capabilities": ["chat"]
            },
            "embed-only": {
                "provider": "p",
                "model": "embed-only",
                "capabilities": ["embedding"]
            }
        },
        "routing": {
            "chat": default_key
        }
    });
    std::fs::write(
        dir.path().join("models.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
}

/// 预置带两轮问答历史的会话（压缩需要可折叠的历史）。
fn seed_session(dir: &tempfile::TempDir, id: &str, model_ref: Option<&str>) {
    let mut session = Session::new("切换测试");
    session.id = id.to_string();
    session.bind_storage_root(dir.path().to_path_buf());
    for round in [("第一问", "第一答"), ("第二问", "第二答")] {
        session.append_message(MessageRole::User, round.0);
        session.append_message(MessageRole::Assistant, round.1);
    }
    session.model_ref = model_ref.map(str::to_string);
    session.try_persist_to_disk().unwrap();
}

async fn make_core(manager: &CoreManager, id: &str) {
    let (stream_tx, _stream_rx) = std::sync::mpsc::channel();
    manager
        .ensure_core(
            id,
            CoreConfig::builder()
                .with_trust_mode(TrustMode::FullTrust)
                .build(),
            "/tmp".to_string(),
            None,
            stream_tx,
            Vec::<Arc<dyn Plugin>>::new,
        )
        .await
        .expect("ensure_core 失败");
}

fn summary_reply() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": "[[SUMMARY]]\n切换前摘要"}}],
        "usage": {"prompt_tokens": 40, "completion_tokens": 8, "total_tokens": 48}
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 目标与当前模型相同时不压缩不切换() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    seed_models(&dir, &server.uri(), "model-a");
    seed_session(&dir, "same-model", Some("model-a"));
    let manager = manager_at(&dir);
    make_core(&manager, "same-model").await;

    let target = manager.resolve_turn_model(Some("model-a")).unwrap();
    manager
        .switch_model_for_test("same-model", target)
        .await
        .expect("相同模型不应失败");

    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "模型未变化时不得触发压缩"
    );
    manager.retire_core("same-model", true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 模型不同时先压缩再切换() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(summary_reply())
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri(), "model-a");
    seed_session(&dir, "switch-ok", Some("model-a"));
    let manager = manager_at(&dir);
    make_core(&manager, "switch-ok").await;

    let before = {
        let registry = manager.registry();
        registry.get("switch-ok").unwrap().current_endpoint().model
    };
    assert_eq!(before, "model-a", "初始模型来自 Session.model_ref");

    let target = manager.resolve_turn_model(Some("model-b")).unwrap();
    manager
        .switch_model_for_test("switch-ok", target)
        .await
        .expect("切换应成功");

    // 压缩发生（摘要边界推进）。
    let session = manager.load_session("switch-ok").unwrap();
    assert!(session.summary_up_to > 0, "切换前应先整理上下文");
    assert_eq!(
        session.context_summary.as_deref(),
        Some("切换前摘要"),
        "摘要由旧模型产出"
    );
    // 模型已切换。
    let after = {
        let registry = manager.registry();
        registry.get("switch-ok").unwrap().current_endpoint().model
    };
    assert_eq!(after, "model-b");
    manager.retire_core("switch-ok", true).await.unwrap();
}

/// 压缩失败不得阻断模型切换。
///
/// 旧模型额度耗尽或服务下线时压缩必然失败；若把它当作整体失败，用户将
/// 永远换不掉模型。压缩只是尽力而为，失败仅记录警告。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 压缩失败仍继续切换模型() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    // 压缩恒失败。
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri(), "model-a");
    seed_session(&dir, "compact-fail", Some("model-a"));
    let manager = manager_at(&dir);
    make_core(&manager, "compact-fail").await;

    let target = manager.resolve_turn_model(Some("model-b")).unwrap();
    manager
        .switch_model_for_test("compact-fail", target)
        .await
        .expect("压缩失败不应阻断模型切换");

    // 压缩确实尝试过且失败（摘要边界未推进）。
    let session = manager.load_session("compact-fail").unwrap();
    assert_eq!(session.summary_up_to, 0, "压缩失败不应推进摘要边界");
    assert!(
        !server.received_requests().await.unwrap().is_empty(),
        "应实际尝试过压缩"
    );

    let current = {
        let registry = manager.registry();
        registry
            .get("compact-fail")
            .unwrap()
            .current_endpoint()
            .model
    };
    assert_eq!(current, "model-b", "压缩失败后仍应切换到目标模型");
    manager.retire_core("compact-fail", true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 无可整理历史时零模型调用直接切换() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    seed_models(&dir, &server.uri(), "model-a");
    // 空会话：没有可折叠的历史。
    let mut session = Session::new("空会话");
    session.id = "empty-history".to_string();
    session.bind_storage_root(dir.path().to_path_buf());
    session.model_ref = Some("model-a".to_string());
    session.try_persist_to_disk().unwrap();
    let manager = manager_at(&dir);
    make_core(&manager, "empty-history").await;

    let target = manager.resolve_turn_model(Some("model-b")).unwrap();
    manager
        .switch_model_for_test("empty-history", target)
        .await
        .expect("无历史时切换应成功");

    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "无可整理历史不得调用模型"
    );
    let current = {
        let registry = manager.registry();
        registry
            .get("empty-history")
            .unwrap()
            .current_endpoint()
            .model
    };
    assert_eq!(current, "model-b");
    manager.retire_core("empty-history", true).await.unwrap();
}

#[test]
fn 目标模型不存在或不支持对话时解析失败() {
    let dir = tempfile::tempdir().unwrap();
    seed_models(&dir, "http://unused.invalid", "model-a");
    let manager = manager_at(&dir);

    let missing = manager
        .resolve_turn_model(Some("no-such-model"))
        .expect_err("不存在的模型应报错");
    assert!(missing.contains("已不在配置中"), "{missing}");

    let not_chat = manager
        .resolve_turn_model(Some("embed-only"))
        .expect_err("非 Chat 模型应报错");
    assert!(not_chat.contains("不支持对话"), "{not_chat}");
}

#[test]
fn 默认模型变化时目标随之变化() {
    let dir = tempfile::tempdir().unwrap();
    seed_models(&dir, "http://unused.invalid", "model-a");
    let manager = manager_at(&dir);

    // None = 跟随默认：解析结果应是当前默认模型。
    let first = manager.resolve_turn_model(None).unwrap();
    assert_eq!(first.model, "model-a");

    // 默认改为 model-b 后，同样的 None 解析出不同目标——这正是不能直接
    // 用 None 与当前模型比较的原因。
    seed_models(&dir, "http://unused.invalid", "model-b");
    let second = manager.resolve_turn_model(None).unwrap();
    assert_eq!(second.model, "model-b");
}

/// 投递未携带模型选择（插件 sendText、队列放行等）时沿用会话已记录的
/// 选择，不得回落到路由默认模型（曾导致会话选 A、插件消息却由默认 B 回复）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 未携带模型的投递沿用会话选择而非路由默认() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "好的"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
        })))
        .mount(&server)
        .await;
    // 路由默认为 a，会话选择了 b。
    seed_models(&dir, &server.uri(), "model-a");
    seed_session(&dir, "keep-session-model", Some("model-b"));
    let manager = manager_at(&dir);
    make_core(&manager, "keep-session-model").await;

    manager
        .deliver_user_message(
            "keep-session-model",
            AgentInputKind::prepared_with_id(
                "plugin-msg".to_string(),
                vec![tiangong_types::ContentBlock::text("插件发送的文本")],
            ),
        )
        .await
        .expect("投递应成功");

    let current = {
        let registry = manager.registry();
        registry
            .get("keep-session-model")
            .unwrap()
            .current_endpoint()
            .model
    };
    assert_eq!(current, "model-b", "未携带模型时应沿用会话选择 model-b");
    let session = manager.load_session("keep-session-model").unwrap();
    assert_eq!(session.model_ref.as_deref(), Some("model-b"));
    assert_eq!(session.summary_up_to, 0, "模型未变化不应触发切换前整理");
    manager
        .retire_core("keep-session-model", true)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core重建后从会话恢复目标模型() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    seed_models(&dir, &server.uri(), "model-a");
    // 会话选择了非默认模型 b。
    seed_session(&dir, "restore-model", Some("model-b"));
    let manager = manager_at(&dir);
    make_core(&manager, "restore-model").await;

    let current = {
        let registry = manager.registry();
        registry
            .get("restore-model")
            .unwrap()
            .current_endpoint()
            .model
    };
    assert_eq!(
        current, "model-b",
        "Core 重建应按 Session.model_ref 恢复实际模型，而非路由默认"
    );

    // 目标与恢复后的当前模型一致 → 不触发压缩。
    let target = manager.resolve_turn_model(Some("model-b")).unwrap();
    manager
        .switch_model_for_test("restore-model", target)
        .await
        .unwrap();
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "恢复后模型一致，不应重复压缩"
    );
    manager.retire_core("restore-model", true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 失效引用不静默回退默认且重选有效模型仍识别为切换() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(summary_reply())
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri(), "model-a");
    // 会话引用了一个已从配置中删除的模型。
    seed_session(&dir, "stale-ref", Some("deleted-model"));
    let manager = manager_at(&dir);
    make_core(&manager, "stale-ref").await;

    let current = {
        let registry = manager.registry();
        registry.get("stale-ref").unwrap().current_endpoint()
    };
    assert!(
        !current.is_usable(),
        "失效引用不得静默回退到路由默认，应保持不可用状态由发送路径报错"
    );

    // 发送前解析该失效引用会明确报错。
    let error = manager
        .resolve_turn_model(Some("deleted-model"))
        .expect_err("失效引用必须报错");
    assert!(error.contains("已不在配置中"), "{error}");

    // 用户重选有效模型：与失效的当前模型不同 → 识别为切换。
    // 当前模型不可用，跳过整理（用空端点压缩必然失败，会把用户永久卡死）。
    let target = manager.resolve_turn_model(Some("model-a")).unwrap();
    manager
        .switch_model_for_test("stale-ref", target)
        .await
        .expect("重选有效模型应成功切换");
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "当前模型不可用时不得尝试压缩"
    );
    let after = {
        let registry = manager.registry();
        registry.get("stale-ref").unwrap().current_endpoint().model
    };
    assert_eq!(after, "model-a");
    manager.retire_core("stale-ref", true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 会话执行中拒绝切换模型() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    // 慢响应：占住 Core。
    Mock::given(method("POST"))
        .respond_with(summary_reply().set_delay(std::time::Duration::from_secs(30)))
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri(), "model-a");
    seed_session(&dir, "busy-switch", Some("model-a"));
    let manager = manager_at(&dir);
    make_core(&manager, "busy-switch").await;

    {
        let registry = manager.registry();
        let core = registry.get("busy-switch").cloned().unwrap();
        drop(registry);
        core.deliver(AgentInputKind::prepared_with_id(
            "m1".to_string(),
            vec![tiangong_types::ContentBlock::text("占用")],
        ))
        .unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let target = manager.resolve_turn_model(Some("model-b")).unwrap();
    let error = manager
        .switch_model_for_test("busy-switch", target)
        .await
        .expect_err("执行中必须拒绝切换");
    assert!(error.contains("正在执行"), "{error}");

    let current = {
        let registry = manager.registry();
        registry
            .get("busy-switch")
            .unwrap()
            .current_endpoint()
            .model
    };
    assert_eq!(current, "model-a", "拒绝切换后保持旧模型");
    manager.retire_core("busy-switch", true).await.unwrap();
}

/// 两个 provider 提供同名模型时，切换必须被识别。
///
/// 回归：若用模型名当身份（或只比模型名），此场景会被判为「未变化」，
/// 结果是新端点不生效、切换前的上下文整理也被跳过。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 同名模型跨服务端切换仍先压缩再切换() {
    let dir = tempfile::tempdir().unwrap();
    let old_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(summary_reply())
        .mount(&old_server)
        .await;
    let new_server = MockServer::start().await;

    // 两个 provider 的模型名完全相同，只有服务端地址不同。
    let config = serde_json::json!({
        "providers": {
            "old": {
                "base_url": old_server.uri(),
                "api_key": "test-key",
                "timeout_ms": 60000,
                "protocol": "openai_chat_completions",
                "headers": {}
            },
            "new": {
                "base_url": new_server.uri(),
                "api_key": "test-key",
                "timeout_ms": 60000,
                "protocol": "openai_chat_completions",
                "headers": {}
            }
        },
        "models": {
            "same-on-old": {"provider": "old", "model": "qwen3-max", "capabilities": ["chat"]},
            "same-on-new": {"provider": "new", "model": "qwen3-max", "capabilities": ["chat"]}
        },
        "routing": {"chat": "same-on-old"}
    });
    std::fs::write(
        dir.path().join("models.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    seed_session(&dir, "same-name", Some("same-on-old"));

    let manager = manager_at(&dir);
    make_core(&manager, "same-name").await;

    let target = manager.resolve_turn_model(Some("same-on-new")).unwrap();
    manager
        .switch_model_for_test("same-name", target)
        .await
        .expect("同名不同服务端应识别为切换");

    let session = manager.load_session("same-name").unwrap();
    assert!(
        session.summary_up_to > 0,
        "同名模型跨服务端切换同样要先整理上下文"
    );
    assert!(
        !old_server.received_requests().await.unwrap().is_empty(),
        "整理上下文应由旧服务端完成"
    );
    let current = {
        let registry = manager.registry();
        registry.get("same-name").unwrap().current_endpoint()
    };
    assert_eq!(current.model, "qwen3-max");
    assert_eq!(current.base_url, new_server.uri(), "端点应指向新服务端");
    manager.retire_core("same-name", true).await.unwrap();
}

/// 两个注册表条目指向同一服务端的同一模型时，不得判为切换。
///
/// 端点身份由 `base_url + model` 派生：换 key（甚至换 provider 名）但实际
/// 模型没变，重复压缩会白白丢失上下文细节并多花一次模型调用。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 同一模型换注册表条目不触发压缩与切换() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let config = serde_json::json!({
        "providers": {
            "p": {
                "base_url": server.uri(),
                "api_key": "test-key",
                "timeout_ms": 60000,
                "protocol": "openai_chat_completions",
                "headers": {}
            }
        },
        "models": {
            "alias-one": {"provider": "p", "model": "gpt-4o", "capabilities": ["chat"]},
            "alias-two": {"provider": "p", "model": "gpt-4o", "capabilities": ["chat"]}
        },
        "routing": {"chat": "alias-one"}
    });
    std::fs::write(
        dir.path().join("models.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    seed_session(&dir, "alias-switch", Some("alias-one"));

    let manager = manager_at(&dir);
    make_core(&manager, "alias-switch").await;

    let target = manager.resolve_turn_model(Some("alias-two")).unwrap();
    manager
        .switch_model_for_test("alias-switch", target)
        .await
        .expect("同一模型换条目不应失败");

    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "实际模型未变，不得触发压缩"
    );
    let session = manager.load_session("alias-switch").unwrap();
    assert_eq!(session.summary_up_to, 0, "实际模型未变，上下文应原样保留");
    manager.retire_core("alias-switch", true).await.unwrap();
}

/// 切换必须等压缩进入终态后才发生。
///
/// 若压缩刚启动就切换，会与压缩任务并发读写 Session，或撞上 `Busy`。
/// 用慢响应把压缩拖住：`switch_model_for_test` 返回时 Core 必须已空闲，
/// 且切换确实生效。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 切换在压缩进入终态后才发生() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(summary_reply().set_delay(std::time::Duration::from_millis(700)))
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri(), "model-a");
    seed_session(&dir, "await-terminal", Some("model-a"));
    let manager = manager_at(&dir);
    make_core(&manager, "await-terminal").await;

    let target = manager.resolve_turn_model(Some("model-b")).unwrap();
    manager
        .switch_model_for_test("await-terminal", target)
        .await
        .expect("切换应成功");

    let core = {
        let registry = manager.registry();
        registry.get("await-terminal").cloned().unwrap()
    };
    assert!(
        !core.is_busy(),
        "返回时压缩任务必须已进入终态，任务槽已释放"
    );
    assert_eq!(core.current_endpoint().model, "model-b");
    let session = manager.load_session("await-terminal").unwrap();
    assert!(session.summary_up_to > 0, "慢压缩应等到收敛而不是被跳过");
    manager.retire_core("await-terminal", true).await.unwrap();
}

/// 上下文窗口跟随当前模型：切换模型后窗口必须同步变化。
///
/// 这是把 `context_window` 从 `CoreConfig` 迁到 `ModelEndpoint` 的原因——
/// `CoreConfig.context_limit` 由**路由默认模型**推导，会话切到别的模型后
/// 不会变；若继续沿用，换到小窗口模型仍按旧窗口判断自动压缩，请求会被
/// Provider 拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 上下文窗口随模型切换同步生效() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(summary_reply())
        .mount(&server)
        .await;
    let config = serde_json::json!({
        "providers": {
            "p": {
                "base_url": server.uri(),
                "api_key": "test-key",
                "timeout_ms": 60000,
                "protocol": "openai_chat_completions",
                "headers": {}
            }
        },
        "models": {
            "wide": {
                "provider": "p",
                "model": "wide-model",
                "capabilities": ["chat"],
                "context_window": 400000
            },
            "narrow": {
                "provider": "p",
                "model": "narrow-model",
                "capabilities": ["chat"],
                "context_window": 32768
            }
        },
        "routing": {"chat": "wide"}
    });
    std::fs::write(
        dir.path().join("models.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    seed_session(&dir, "window-follow", Some("wide"));

    let manager = manager_at(&dir);
    make_core(&manager, "window-follow").await;

    let before = {
        let registry = manager.registry();
        registry.get("window-follow").unwrap().current_endpoint()
    };
    assert_eq!(before.context_window, Some(400_000), "初始窗口来自当前模型");

    // 切到小窗口模型。
    let target = manager.resolve_turn_model(Some("narrow")).unwrap();
    assert_eq!(target.context_window, Some(32_768));
    manager
        .switch_model_for_test("window-follow", target)
        .await
        .expect("切换应成功");

    let after = {
        let registry = manager.registry();
        registry.get("window-follow").unwrap().current_endpoint()
    };
    assert_eq!(
        after.context_window,
        Some(32_768),
        "切换后窗口必须同步为新模型的窗口，而不是沿用路由默认模型的窗口"
    );
    // 窗口不属于模型身份：仅窗口不同不触发切换。
    let mut same_model_wider = after.clone();
    same_model_wider.context_window = Some(400_000);
    assert!(after.is_same_model(&same_model_wider));
    // 往返不丢失。
    assert_eq!(after.to_resolved().context_window, Some(32_768));
    manager.retire_core("window-follow", true).await.unwrap();
}

/// 未在 models.json 中声明窗口时端点留空，由 turn 回落到 CoreConfig 兜底。
///
/// 真实窗口存在 `models.json` 的模型条目上；`context_windows.json` 只是前端
/// 编辑模型时的预填预设，运行期不回查，避免预设值覆盖用户未设置的语义。
#[test]
fn 未声明窗口的模型端点留空() {
    let dir = tempfile::tempdir().unwrap();
    seed_models(&dir, "http://unused.invalid", "model-a");
    let manager = manager_at(&dir);

    let endpoint = manager.resolve_turn_model(Some("model-a")).unwrap();
    assert_eq!(
        endpoint.context_window, None,
        "未声明窗口时留空，由调用方回落到 CoreConfig.context_limit"
    );
}

/// 模型编排失败时消息不投递，Core 状态保持不变。
///
/// 编排在 Manager 投递路径内部完成：解析失败直接返回错误，用户消息不进
/// 会话、也不触发任何模型调用，当前模型保持原样。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 模型解析失败不投递消息且保持当前模型() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    seed_models(&dir, &server.uri(), "model-a");
    seed_session(&dir, "bad-model", Some("model-a"));
    let manager = manager_at(&dir);
    make_core(&manager, "bad-model").await;

    let before = manager.load_session("bad-model").unwrap().messages.len();

    let error = manager
        .deliver_user_message(
            "bad-model",
            AgentInputKind::prepared_with_id(
                "m-bad".to_string(),
                vec![tiangong_types::ContentBlock::text("你好")],
            )
            .with_model_ref(Some("no-such-model".to_string())),
        )
        .await
        .expect_err("目标模型不存在应失败");

    assert!(error.contains("已不在配置中"), "{error}");

    // 消息未投递。
    let after = manager.load_session("bad-model").unwrap().messages.len();
    assert_eq!(after, before, "解析失败时不得投递用户消息");
    // Core 仍在且模型未变。
    let current = {
        let registry = manager.registry();
        registry.get("bad-model").unwrap().current_endpoint()
    };
    assert_eq!(current.model, "model-a", "失败后仍保持原模型");
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "解析失败不应触发任何模型调用"
    );
    manager.retire_core("bad-model", true).await.unwrap();
}

/// 投递路径内部完成模型切换：宿主只调一次投递，切换对其透明。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 投递用户消息时自动完成模型切换() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(summary_reply())
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri(), "model-a");
    seed_session(&dir, "deliver-switch", Some("model-a"));
    let manager = manager_at(&dir);
    make_core(&manager, "deliver-switch").await;

    manager
        .deliver_user_message(
            "deliver-switch",
            AgentInputKind::prepared_with_id(
                "m-1".to_string(),
                vec![tiangong_types::ContentBlock::text("换个模型继续")],
            )
            .with_model_ref(Some("model-b".to_string())),
        )
        .await
        .expect("投递应成功");

    // 切换已在投递路径内完成。
    let current = {
        let registry = manager.registry();
        registry.get("deliver-switch").unwrap().current_endpoint()
    };
    assert_eq!(current.model, "model-b", "投递路径内部应完成模型切换");
    // 用户选择已记录，供前端回显。
    let session = manager.load_session("deliver-switch").unwrap();
    assert_eq!(session.model_ref.as_deref(), Some("model-b"));
    manager.retire_core("deliver-switch", true).await.unwrap();
}

/// 模型选择随消息携带，非用户消息不受影响。
///
/// 宿主只需把界面上选的模型附在消息上，解析与切换是 Manager 投递路径的
/// 内部步骤——app 层不再传递额外参数、也不参与编排。
#[test]
fn 模型选择随用户消息携带() {
    use tiangong_core::agent_input::MessageInput;

    let input =
        AgentInputKind::prepared_with_id("m", vec![tiangong_types::ContentBlock::text("x")])
            .with_model_ref(Some("model-b".to_string()));
    match &input {
        AgentInputKind::Message(MessageInput::UserMessage { model_ref, .. }) => {
            assert_eq!(model_ref.as_deref(), Some("model-b"));
        }
        _ => panic!("应为用户消息"),
    }

    // 空白与空串归一为 None（跟随默认），避免把空选择当成有效 key。
    let blank =
        AgentInputKind::prepared_with_id("m", vec![]).with_model_ref(Some("   ".to_string()));
    match &blank {
        AgentInputKind::Message(MessageInput::UserMessage { model_ref, .. }) => {
            assert!(model_ref.is_none(), "空白选择应归一为跟随默认");
        }
        _ => panic!("应为用户消息"),
    }

    // 命令类输入不承载模型选择，附加操作是安全的空操作。
    let command = AgentInputKind::cancel().with_model_ref(Some("model-b".to_string()));
    assert!(matches!(command, AgentInputKind::Command(_)));
}

/// 执行中投递用户消息（引导消息）：跳过模型编排，直接交给活跃 turn。
///
/// 回归（对话被打断并显示失败）：运行中整理上下文与切换模型都会被拒绝
/// （`Busy`）。若投递路径仍走编排并把 `Busy` 当成投递失败返回，宿主的失败
/// 回滚会关闭 Core，正在进行的对话被直接打断并显示为失败。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 执行中的引导消息跳过模型编排且投递成功() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    // 慢响应：占住 Core，保证投递时仍在执行中。
    Mock::given(method("POST"))
        .respond_with(summary_reply().set_delay(std::time::Duration::from_secs(30)))
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri(), "model-a");
    seed_session(&dir, "busy-inject", Some("model-a"));
    let manager = manager_at(&dir);
    make_core(&manager, "busy-inject").await;

    manager
        .deliver_user_message(
            "busy-inject",
            AgentInputKind::prepared_with_id(
                "m-first".to_string(),
                vec![tiangong_types::ContentBlock::text("占用")],
            )
            .with_model_ref(Some("model-a".to_string())),
        )
        .await
        .expect("首轮投递应成功");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    {
        let registry = manager.registry();
        assert!(
            registry.get("busy-inject").unwrap().is_busy(),
            "首轮应仍在执行中"
        );
    }

    // 执行中追加引导消息，并且界面上还选了另一个模型：不得因为切换被拒而失败。
    manager
        .deliver_user_message(
            "busy-inject",
            AgentInputKind::prepared_with_id(
                "m-guide".to_string(),
                vec![tiangong_types::ContentBlock::text("换个方向")],
            )
            .with_model_ref(Some("model-b".to_string())),
        )
        .await
        .expect("执行中的引导消息必须投递成功，不能被模型编排判定为失败");

    let current = {
        let registry = manager.registry();
        registry
            .get("busy-inject")
            .unwrap()
            .current_endpoint()
            .model
    };
    assert_eq!(current, "model-a", "执行中沿用当前模型，不做切换");
    manager.retire_core("busy-inject", true).await.unwrap();
}
/// 轮询事件流直到本轮完成（`Done`），收集全部事件；出错或超时即 panic。
fn collect_events_until_done(
    stream_rx: &std::sync::mpsc::Receiver<tiangong_types::StreamEvent>,
) -> Vec<tiangong_types::StreamEvent> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut events = Vec::new();
    loop {
        match stream_rx.try_recv() {
            Ok(event) => {
                let is_done = matches!(event, tiangong_types::StreamEvent::Done { .. });
                let is_error = matches!(event, tiangong_types::StreamEvent::Error { .. });
                events.push(event);
                if is_done {
                    return events;
                }
                if is_error {
                    panic!("turn 执行出错：{events:?}");
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "等待 turn 完成超时，已收到事件：{events:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("流通道提前关闭，已收到事件：{events:?}");
            }
        }
    }
}

fn chat_reply(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    }))
}

/// 从 mock server 收到的请求中提取全部请求体里的 model 字段。
async fn requested_models(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .unwrap()
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap()
                .to_string()
        })
        .collect()
}

/// 新对话首次发送携带非默认模型选择时不触发切换编排（回归）。
///
/// 曾缺陷：ensure 创建的新 Core 按路由默认模型（model-a）初始化，首条
/// 消息携带 model-b 投递时被判定为「切换」，发出 ModelSwitchStarted /
/// ModelSwitched 并写入切换留痕，前端显示「正在切换模型」。新对话的
/// Core 正是在首次发送中创建，修复为把本次选择传入 ensure_core，
/// Core 出生即目标模型。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 新对话首次选定模型不触发切换编排() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_reply("你好，我是助手。"))
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri(), "model-a");
    // 新对话：空会话、Session.model_ref = None、Core 未创建。
    let mut session = Session::new("新对话");
    session.id = "first-send".to_string();
    session.bind_storage_root(dir.path().to_path_buf());
    session.try_persist_to_disk().unwrap();
    let manager = manager_at(&dir);

    // 模拟完整发送流程：ensure 携带本次选择的 model-b 创建 Core →
    // deliver 同样携带 model-b。
    let (stream_tx, stream_rx) = std::sync::mpsc::channel();
    let ensured = manager
        .ensure_core(
            "first-send",
            CoreConfig::builder()
                .with_trust_mode(TrustMode::FullTrust)
                .build(),
            "/tmp".to_string(),
            Some("model-b"),
            stream_tx,
            Vec::<Arc<dyn Plugin>>::new,
        )
        .await
        .expect("ensure_core 失败");
    assert!(ensured.is_new, "新对话的 Core 应在首次发送时创建");

    // 新 Core 出生即目标模型，投递前比较已经一致。
    {
        let registry = manager.registry();
        let current = registry.get("first-send").unwrap().current_endpoint();
        assert_eq!(
            current.model, "model-b",
            "新 Core 应直接以本次发送的目标模型初始化"
        );
    }

    manager
        .deliver_user_message(
            "first-send",
            AgentInputKind::prepared_with_id(
                "m1".to_string(),
                vec![tiangong_types::ContentBlock::text("你好")],
            )
            .with_model_ref(Some("model-b".to_string())),
        )
        .await
        .expect("投递应成功");

    // 全程无切换事件；首次请求即用目标模型。
    // （无 ModelSwitchStarted 事件即证明未进入切换编排——压缩调用只
    // 存在于该编排内。若收到 model-a 请求，那是投递后的标题自动生成：
    // 它走 Lite/Chat 路由槽，独立于会话模型选择，不属于本回归范围。）
    let events = collect_events_until_done(&stream_rx);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            tiangong_types::StreamEvent::ModelSwitchStarted { .. }
        )),
        "新对话首次选定模型不得发出 ModelSwitchStarted：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, tiangong_types::StreamEvent::ModelSwitched { .. })),
        "新对话首次选定模型不得发出 ModelSwitched：{events:?}"
    );
    let models = requested_models(&server).await;
    assert!(
        models.iter().any(|model| model == "model-b"),
        "首次请求应使用目标模型 model-b，实际：{models:?}"
    );

    // 3. 会话无切换留痕，且选择已正确记录。
    let session = manager.load_session("first-send").unwrap();
    assert!(
        !session.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                block
                    .as_text()
                    .is_some_and(|text| text.contains("已切换模型"))
            })
        }),
        "首次选定模型不得写入切换留痕：{:?}",
        session.messages
    );
    assert_eq!(
        session.model_ref.as_deref(),
        Some("model-b"),
        "本次选择应记录到 Session.model_ref"
    );
    manager.retire_core("first-send", true).await.unwrap();
}

/// 有回复历史后首次显式选定模型仍走完整切换编排（防误伤）。
///
/// 已产生回复的会话中，默认模型已实际参与对话，用户首次显式选模型是
/// 一次真正的切换：先整理上下文、再切换、留痕，事件齐全。ensure 未
/// 携带模型选择（既有 Core 场景下也无处使用），投递路径编排不变。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 有回复历史后首次选定模型仍走切换编排() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(summary_reply())
        .mount(&server)
        .await;
    seed_models(&dir, &server.uri(), "model-a");
    // 会话一直用默认模型（Session.model_ref = None）但已有两轮问答。
    seed_session(&dir, "late-select", None);
    let manager = manager_at(&dir);

    let (stream_tx, stream_rx) = std::sync::mpsc::channel();
    manager
        .ensure_core(
            "late-select",
            CoreConfig::builder()
                .with_trust_mode(TrustMode::FullTrust)
                .build(),
            "/tmp".to_string(),
            None,
            stream_tx,
            Vec::<Arc<dyn Plugin>>::new,
        )
        .await
        .expect("ensure_core 失败");

    manager
        .deliver_user_message(
            "late-select",
            AgentInputKind::prepared_with_id(
                "m1".to_string(),
                vec![tiangong_types::ContentBlock::text("换模型")],
            )
            .with_model_ref(Some("model-b".to_string())),
        )
        .await
        .expect("投递应成功");

    let events = collect_events_until_done(&stream_rx);
    assert!(
        events.iter().any(|event| matches!(
            event,
            tiangong_types::StreamEvent::ModelSwitchStarted { .. }
        )),
        "有历史后的首次显式选定是真正的切换，应发出事件：{events:?}"
    );
    let session = manager.load_session("late-select").unwrap();
    assert!(session.summary_up_to > 0, "切换前应先由旧模型整理上下文");
    assert!(
        session.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                block
                    .as_text()
                    .is_some_and(|text| text.contains("已切换模型"))
            })
        }),
        "真正的切换应写入留痕"
    );
    let current = {
        let registry = manager.registry();
        registry
            .get("late-select")
            .unwrap()
            .current_endpoint()
            .model
    };
    assert_eq!(current, "model-b");
    manager.retire_core("late-select", true).await.unwrap();
}

/// ensure 携带失效模型引用时直接报错，不创建 Core、不静默落到路由默认。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure携带失效模型引用时报错不建core() {
    let dir = tempfile::tempdir().unwrap();
    seed_models(&dir, "http://unused.invalid", "model-a");
    let manager = manager_at(&dir);
    let (stream_tx, _stream_rx) = std::sync::mpsc::channel();

    let error = manager
        .ensure_core(
            "stale-ensure",
            CoreConfig::builder()
                .with_trust_mode(TrustMode::FullTrust)
                .build(),
            "/tmp".to_string(),
            Some("deleted-model"),
            stream_tx,
            Vec::<Arc<dyn Plugin>>::new,
        )
        .await
        .expect_err("失效引用应直接报错");
    assert!(error.contains("已不在配置中"), "{error}");
    assert!(
        manager.registry().get("stale-ensure").is_none(),
        "报错时不得创建 Core"
    );
}
