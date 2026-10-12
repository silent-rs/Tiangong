//! stdio 连接的单元测试（工作区提取、取消顺序、日志尾部）。
//!
//! 文件级 `use super::*` 桥接 stdio 连接本体；文件内嵌套的测试 mod
//! 再经一层 glob 继承该别名集合。

use super::*;

#[cfg(test)]
mod invocation_workspace_tests {
    use super::*;

    fn runtime_context(workspace: &str) -> crate::protocol::RequestInvocationContext {
        crate::protocol::RequestInvocationContext {
            session_id: "session-a".into(),
            invocation_id: "call-a".into(),
            workspace: workspace.into(),
            actor_id: "agent-a".into(),
            deadline_ms: None,
        }
    }

    fn legacy_context(workspace: &Path) -> crate::sidecar::SidecarInvocationContext {
        crate::sidecar::SidecarInvocationContext::new("session-a", "call-a", workspace)
    }

    #[test]
    fn runtime_context_workspace_wins_when_present() {
        let root = tempfile::tempdir().unwrap();
        let legacy_ws = root.path().join("legacy");
        let runtime_ws = root.path().join("runtime");
        std::fs::create_dir(&legacy_ws).unwrap();
        std::fs::create_dir(&runtime_ws).unwrap();

        // 仅新版上下文。
        assert_eq!(
            invocation_workspace(None, Some(&runtime_context(runtime_ws.to_str().unwrap()))),
            Some(runtime_ws.as_path())
        );
        // 仅旧版上下文。
        assert_eq!(
            invocation_workspace(Some(&legacy_context(&legacy_ws)), None),
            Some(legacy_ws.as_path())
        );
        // 新旧同时存在时新版优先。
        assert_eq!(
            invocation_workspace(
                Some(&legacy_context(&legacy_ws)),
                Some(&runtime_context(runtime_ws.to_str().unwrap()))
            ),
            Some(runtime_ws.as_path())
        );
        // 新版工作区为空（含空白）时回退旧版。
        assert_eq!(
            invocation_workspace(
                Some(&legacy_context(&legacy_ws)),
                Some(&runtime_context("  "))
            ),
            Some(legacy_ws.as_path())
        );
        // 两者均不存在。
        assert_eq!(invocation_workspace(None, None), None);
    }

    #[test]
    fn invalid_workspace_is_rejected_by_validation_not_extraction() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, "not a directory").unwrap();

        // 提取层不做校验，交由 validate_invocation_workspace 拒绝。
        let context = runtime_context(file.to_str().unwrap());
        let extracted = invocation_workspace(None, Some(&context)).expect("提取层只负责选择");
        assert!(
            validate_invocation_workspace(extracted)
                .unwrap_err()
                .to_string()
                .contains("不是目录")
        );
        assert!(
            validate_invocation_workspace(Path::new("relative-workspace"))
                .unwrap_err()
                .to_string()
                .contains("必须是绝对路径")
        );
    }

    #[test]
    fn pending_waiter_session_membership_spans_both_contexts() {
        let (response_tx, _response_rx) = sync_channel(1);
        let (progress_tx, _progress_rx) = sync_channel(1);
        let mk = |invocation, invocation_context| PendingWaiter {
            response: response_tx.clone(),
            progress: progress_tx.clone(),
            invocation,
            invocation_context,
        };
        let legacy = legacy_context(Path::new("/legacy"));
        let runtime = runtime_context("/runtime");

        // 仅新版上下文：按新版会话命中。
        let waiter = mk(None, Some(runtime.clone()));
        assert!(waiter.belongs_to_session("session-a"));
        assert!(!waiter.belongs_to_session("session-b"));
        // 仅旧版上下文：仍可取消（兼容已发布插件）。
        let waiter = mk(Some(legacy.clone()), None);
        assert!(waiter.belongs_to_session("session-a"));
        assert!(!waiter.belongs_to_session("session-b"));
        // 两者都有：任一命中即归属。
        let waiter = mk(Some(legacy), Some(runtime));
        assert!(waiter.belongs_to_session("session-a"));
        // 两者皆无：不属于任何会话。
        let waiter = mk(None, None);
        assert!(!waiter.belongs_to_session("session-a"));
    }

    #[test]
    fn invocation_workspace_overrides_connection_default_for_spawn() {
        let root = tempfile::tempdir().unwrap();
        let connection_workspace = root.path().join("connection");
        let invocation_workspace = root.path().join("invocation");
        std::fs::create_dir(&connection_workspace).unwrap();
        std::fs::create_dir(&invocation_workspace).unwrap();
        let config = SidecarConfig::new(
            "fs",
            "0.0.0",
            root.path().join("missing-sidecar"),
            root.path().join("endpoint.json"),
            root.path().join("sidecar.log"),
            root.path().join("data"),
            root.path(),
        )
        .with_sandbox_workspace(Some(connection_workspace.clone()));

        let storage_data = root.path().join("data").join("fs");
        assert_eq!(
            sandbox_workspace_for_spawn(&config, Some(&invocation_workspace), &storage_data),
            invocation_workspace
        );
        assert_eq!(
            sandbox_workspace_for_spawn(&config, None, &storage_data),
            connection_workspace
        );
        // 无任何工作区时兜底到插件自有数据目录，绝不回退存储根本身。
        let mut unset = config.clone();
        unset.sandbox_workspace = None;
        assert_eq!(
            sandbox_workspace_for_spawn(&unset, None, &storage_data),
            storage_data
        );
    }

    #[test]
    fn invocation_workspace_must_be_an_existing_absolute_directory() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();

        assert_eq!(
            validate_invocation_workspace(&workspace).unwrap(),
            std::fs::canonicalize(&workspace).unwrap()
        );
        assert!(
            validate_invocation_workspace(Path::new("relative-workspace"))
                .unwrap_err()
                .to_string()
                .contains("必须是绝对路径")
        );
        assert!(
            validate_invocation_workspace(&root.path().join("missing"))
                .unwrap_err()
                .to_string()
                .contains("解析本次 sidecar 调用的工作区失败")
        );
        let file = root.path().join("file");
        std::fs::write(&file, "not a directory").unwrap();
        assert!(
            validate_invocation_workspace(&file)
                .unwrap_err()
                .to_string()
                .contains("不是目录")
        );
    }
}

#[cfg(test)]
mod cancel_order_tests {
    use super::*;

    #[test]
    fn on_demand_processes_stay_private_and_never_share_the_state_slot() {
        // 回归：按需调用的进程完全私有，不登记共享 state.process。
        // 修复前 start_fresh 经单槽登记并清杀旧登记——并发调用（如 UI
        // 消息与工具调用同时到达）后启动方会误杀先启动方的进程。
        if cfg!(target_os = "windows") {
            // 依赖 POSIX sleep（无参数即退出、永不完成握手），Windows 跳过。
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let config = SidecarConfig::new(
            "spawn-privacy-test",
            "0.0.0",
            PathBuf::from("/bin/sleep"),
            root.path().join("endpoint.json"),
            root.path().join("sidecar.log"),
            root.path().join("data"),
            root.path(),
        )
        .with_timeouts(Duration::from_millis(800), Duration::from_millis(800));
        let connection = std::sync::Arc::new(StdioSidecarConnection::new(config));
        let first = std::sync::Arc::clone(&connection);
        let second = std::sync::Arc::clone(&connection);
        let left = std::thread::spawn(move || first.spawn_ready(None).is_err());
        let right = std::thread::spawn(move || second.spawn_ready(None).is_err());
        // sleep 无参数直接退出，握手必然失败；两个私有进程各自失败返回。
        assert!(left.join().expect("并发 spawn 线程 panic"));
        assert!(right.join().expect("并发 spawn 线程 panic"));
        // 核心不变量：按需进程从不进入共享槽，并发互杀的结构性根源已消除。
        assert!(
            connection
                .state
                .lock()
                .expect("状态锁可用")
                .process
                .is_none()
        );
    }

    #[test]
    fn cancel_notifies_waiter_before_ignoring_write_failure() {
        let root = tempfile::tempdir().unwrap();
        let config = SidecarConfig::new(
            "cancel-order",
            "0.0.0",
            root.path().join("missing-sidecar"),
            root.path().join("endpoint.json"),
            root.path().join("sidecar.log"),
            root.path().join("data"),
            root.path(),
        );
        let connection = StdioSidecarConnection::new(config);
        let (response_tx, response_rx) = sync_channel(1);
        let (progress_tx, _progress_rx) = sync_channel(1);
        connection.finish_waiter_then_cancel(
            Some(PendingWaiter {
                response: response_tx,
                progress: progress_tx,
                invocation: None,
                invocation_context: None,
            }),
            "request-cancel",
            || {
                // 写帧动作执行前，调用方必须已经收到取消结果；随后模拟写失败。
                let result = response_rx.try_recv().expect("等待者应先被取消唤醒");
                assert_eq!(result.unwrap_err(), "请求已取消");
                Err(anyhow!("stdio 已关闭"))
            },
        );
    }
}

#[cfg(test)]
mod log_tail_tests {
    use super::*;

    #[test]
    fn sidecar_log_tail_returns_last_nonempty_lines() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("sidecar.log");
        // 模拟 Launcher 拒绝启动场景：stderr JSON 落在日志末行。
        std::fs::write(
            &log,
            "old line 1\nold line 2\nold line 3\nold line 4\n{\"launcher\":\"tiangong-sandbox\",\"error\":\"嵌套沙箱不可用\"}\n",
        )
        .unwrap();
        let tail = sidecar_log_tail(&log).unwrap();
        assert!(
            tail.contains("tiangong-sandbox") && tail.contains("嵌套沙箱不可用"),
            "尾部应携带最后的可归因内容: {tail}"
        );
        assert!(!tail.contains("old line 1"), "不应回溯过旧的行");
    }

    #[test]
    fn sidecar_log_tail_handles_missing_and_empty_log() {
        let root = tempfile::tempdir().unwrap();
        assert!(sidecar_log_tail(&root.path().join("absent.log")).is_none());
        let log = root.path().join("empty.log");
        std::fs::write(&log, "\n\n").unwrap();
        assert!(sidecar_log_tail(&log).is_none());
    }

    #[test]
    fn sidecar_log_tail_truncates_long_lines() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("sidecar.log");
        let long_line = "x".repeat(2000);
        std::fs::write(&log, &long_line).unwrap();
        let tail = sidecar_log_tail(&log).unwrap();
        assert!(tail.chars().count() <= 400, "超长行应被截断");
    }
}
