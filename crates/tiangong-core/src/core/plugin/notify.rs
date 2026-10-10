//! 收尾钩子的统一通知投递（issue #404），以及 `on_turn_finished` 本轮只读
//! 快照的组装（插件唯一拿到会话内容的入口）。
//!
//! `on_turn_finished` / `on_session_ended` 是通知型钩子：Core 只保证通知最终
//! 送达插件，不等待完成、不收集结果、不重试——收尾成败与产出由插件实现自行
//! 负责（失败/超时自行记录日志，重活交给插件自身的 sidecar 或后台任务）。
//!
//! 投递为纯 fire-and-forget：每个通知一个短命后台线程（与 WASM 适配器旧
//! detached 模式同款，`std::thread` 无 runtime 依赖，在同步 spawn_blocking
//! 上下文中也安全），调用方立即返回。插件 panic 由 `catch_unwind` 兜底，
//! 仅记告警不影响后续通知。

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::thread;

use tiangong_types::{MessageRole, PluginSession};

use super::Plugin;
use crate::session::Session;

/// 后台投递 `on_turn_finished`：turn 终态已发布，通知即返回。
///
/// `anchor_id` 为本轮锚点消息 ID；快照只组装一次、全部插件共享。
pub(crate) fn notify_turn_finished(
    plugins: &[Arc<dyn Plugin>],
    session: &Session,
    anchor_id: &str,
) {
    let snapshot = Arc::new(turn_snapshot(session, anchor_id));
    for plugin in plugins {
        let plugin = Arc::clone(plugin);
        let session = Arc::clone(&snapshot);
        let plugin_id = plugin.id().to_owned();
        let spawn_fail_id = plugin_id.clone();
        let spawned = thread::Builder::new()
            .name(format!("plugin-turn-finish-{plugin_id}"))
            .spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    plugin.on_turn_finished(&session);
                })) {
                    tracing::warn!(
                        plugin_id,
                        ?panic,
                        "插件 on_turn_finished 通知 panic，已忽略"
                    );
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(plugin_id = %spawn_fail_id, %error, "插件 on_turn_finished 通知线程启动失败，通知丢弃");
        }
    }
}

/// 本轮的插件只读快照：只含本轮消息，不暴露 Core 的 [`Session`]。
///
/// 本轮从锚点消息（本轮用户输入）开始到末尾。锚点按消息 ID 定位：运行中
/// 压缩会在锚点之前插入消息，位置会后移，按 ID 定位不受影响。Notice 是宿主
/// 发给用户的系统通知，不属于对话内容，始终剔除。锚点不存在时消息为空。
/// 不带跨轮历史：长会话完整历史可达数十 MB，整份交给插件会让其解析超出
/// 执行预算而 trap。
fn turn_snapshot(session: &Session, anchor_id: &str) -> PluginSession {
    let messages = session
        .messages
        .iter()
        .position(|message| message.id == anchor_id)
        .map(|start| {
            session.messages[start..]
                .iter()
                .filter(|message| message.role != MessageRole::Notice)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // 工作区标识：取 cwd 的末尾目录名（平台无关，由宿主生成）。
    let workspace_id = session
        .cwd
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(&session.cwd)
        .to_string();
    PluginSession {
        id: session.id.clone(),
        title: session.title.clone(),
        cwd: session.cwd.clone(),
        workspace_id,
        reasoning_effort: session
            .reasoning_effort
            .map(|effort| effort.as_str().to_string()),
        messages,
        context_summary: session.context_summary.clone(),
        created_at: session.created_at.clone(),
        updated_at: session.updated_at.clone(),
    }
}

/// 后台投递 `on_session_ended`：会话关闭立即返回，插件收尾自行收敛。
pub(crate) fn notify_session_ended(plugins: &[Arc<dyn Plugin>]) {
    for plugin in plugins {
        let plugin = Arc::clone(plugin);
        let plugin_id = plugin.id().to_owned();
        let spawn_fail_id = plugin_id.clone();
        let spawned = thread::Builder::new()
            .name(format!("plugin-session-end-{plugin_id}"))
            .spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    plugin.on_session_ended();
                })) {
                    tracing::warn!(
                        plugin_id,
                        ?panic,
                        "插件 on_session_ended 通知 panic，已忽略"
                    );
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(plugin_id = %spawn_fail_id, %error, "插件 on_session_ended 通知线程启动失败，通知丢弃");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use super::super::Plugin;
    use super::turn_snapshot;
    use crate::session::Session;
    use crate::tools::extension::{PromptSectionProvider, ToolOverrideHandler, ToolSpecProvider};
    use tiangong_types::{Message, MessageRole, UserSource};

    #[test]
    fn 快照携带会话元信息() {
        let mut session = Session::new("元信息");
        session.cwd = "/tmp/ws".to_string();
        session.append_message(MessageRole::User, "第一轮");
        let anchor_id = session.messages[0].id.clone();
        let snapshot = turn_snapshot(&session, &anchor_id);
        assert_eq!(snapshot.id, session.id);
        assert_eq!(snapshot.workspace_id, "ws");
    }

    #[test]
    fn 轮次快照只含本轮消息且剔除_notice() {
        let mut session = Session::new("轮次");
        session.append_message(MessageRole::User, "上一轮");
        session.append_message(MessageRole::Assistant, "上一轮回复");
        session.append_message(MessageRole::Notice, "上一轮失败通知");
        session.append_message(MessageRole::User, "本轮输入");
        let anchor_id = session.messages[3].id.clone();
        session.append_message(MessageRole::Notice, "本轮通知");
        session.append_message(MessageRole::User, "运行中引导");
        let injected = Message::new(MessageRole::User, "[injected-images]")
            .with_source(UserSource::HostInjected);
        session.messages.push(injected);
        session.append_message(MessageRole::Assistant, "本轮回复");

        let texts: Vec<String> = turn_snapshot(&session, &anchor_id)
            .messages
            .iter()
            .map(Message::text_content)
            .collect();
        assert_eq!(
            texts,
            vec!["本轮输入", "运行中引导", "[injected-images]", "本轮回复"]
        );
    }

    #[test]
    fn 锚点之前插入消息不影响本轮范围() {
        // 运行中压缩会在锚点前插入恢复锚点与压缩记录，位置后移。
        let mut session = Session::new("压缩");
        session.append_message(MessageRole::User, "上一轮");
        session.append_message(MessageRole::User, "本轮输入");
        let anchor_id = session.messages[1].id.clone();
        session.append_message(MessageRole::Assistant, "本轮回复");
        session
            .messages
            .insert(0, Message::new(MessageRole::User, "压缩恢复锚点"));

        let texts: Vec<String> = turn_snapshot(&session, &anchor_id)
            .messages
            .iter()
            .map(Message::text_content)
            .collect();
        assert_eq!(texts, vec!["本轮输入", "本轮回复"]);
    }

    #[test]
    fn 锚点不存在时消息为空() {
        let mut session = Session::new("缺失");
        session.append_message(MessageRole::User, "输入");
        assert!(turn_snapshot(&session, "missing").messages.is_empty());
    }

    struct HookProbePlugin {
        id: &'static str,
        turn_finished_calls: AtomicU32,
        session_ended_calls: AtomicU32,
    }

    impl ToolSpecProvider for HookProbePlugin {}
    impl ToolOverrideHandler for HookProbePlugin {}
    impl PromptSectionProvider for HookProbePlugin {}

    impl Plugin for HookProbePlugin {
        fn id(&self) -> &str {
            self.id
        }
        fn on_turn_finished(&self, _session: &tiangong_types::PluginSession) {
            self.turn_finished_calls.fetch_add(1, Ordering::SeqCst);
        }
        fn on_session_ended(&self) {
            self.session_ended_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn wait_for(counter: &AtomicU32, expected: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while counter.load(Ordering::SeqCst) != expected {
            assert!(
                Instant::now() < deadline,
                "等待通知落地超时：期望 {expected}，实际 {}",
                counter.load(Ordering::SeqCst)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn notify_delivers_hooks_without_blocking_on_slow_plugin() {
        struct SlowPlugin {
            started: AtomicBool,
            finished: AtomicBool,
        }
        impl ToolSpecProvider for SlowPlugin {}
        impl ToolOverrideHandler for SlowPlugin {}
        impl PromptSectionProvider for SlowPlugin {}
        impl Plugin for SlowPlugin {
            fn id(&self) -> &str {
                "slow-notify"
            }
            fn on_session_ended(&self) {
                self.started.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_secs(2));
                self.finished.store(true, Ordering::SeqCst);
            }
        }

        let slow = Arc::new(SlowPlugin {
            started: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        let plugins: Vec<Arc<dyn Plugin>> = vec![slow.clone()];

        let notify_started = Instant::now();
        super::notify_session_ended(&plugins);
        let elapsed = notify_started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "通知投递应立即返回（慢插件不阻塞调用方），实际耗时 {elapsed:?}"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while !slow.started.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "通知应到达慢插件钩子");
            std::thread::sleep(Duration::from_millis(10));
        }
        // 未释放前钩子仍在后台执行，但已不影响调用方。
        assert!(
            !slow.finished.load(Ordering::SeqCst),
            "2 秒阻塞未结束前钩子不应已完成"
        );
    }

    #[test]
    fn notify_survives_plugin_panic() {
        struct PanickingPlugin;
        impl ToolSpecProvider for PanickingPlugin {}
        impl ToolOverrideHandler for PanickingPlugin {}
        impl PromptSectionProvider for PanickingPlugin {}
        impl Plugin for PanickingPlugin {
            fn id(&self) -> &str {
                "panicking-notify"
            }
            fn on_turn_finished(&self, _session: &tiangong_types::PluginSession) {
                panic!("插件钩子 panic 不应打穿通知线程");
            }
        }

        let panicking: Arc<dyn Plugin> = Arc::new(PanickingPlugin);
        let probe = Arc::new(HookProbePlugin {
            id: "probe-notify",
            turn_finished_calls: AtomicU32::new(0),
            session_ended_calls: AtomicU32::new(0),
        });
        let plugins: Vec<Arc<dyn Plugin>> = vec![panicking, probe.clone()];
        let session = Session::new("notify-test");

        std::panic::set_hook(Box::new(|_| {}));
        super::notify_turn_finished(&plugins, &session, "");
        wait_for(&probe.turn_finished_calls, 1);
        let _ = std::panic::take_hook();

        super::notify_session_ended(&plugins);
        wait_for(&probe.session_ended_calls, 1);
    }
}
