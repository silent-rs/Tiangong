//! 前端标签协作：附着确认、按会话查找/列出终端与存活集合回收。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use super::types::{
    FindRequest, FindResponse, ListByScopeRequest, ListByScopeResponse, OkResponse,
    ScopeTerminalEntry, TerminalBindingRequest, TerminalGcRequest,
};
use super::{FRONTEND_ATTACH_WAIT_MS, TerminalService};
use crate::persist;

impl TerminalService {
    fn mark_frontend_attached(&self, session_id: &str) {
        self.frontend_attached
            .lock()
            .expect("前端附着表锁损坏")
            .insert(session_id.to_string());
    }

    pub(super) fn attach_frontend(&self, request: TerminalBindingRequest) -> Result<OkResponse> {
        let session_id = request.session_id.trim();
        let scope_id = request.scope_id.trim();
        if session_id.is_empty() || scope_id.is_empty() {
            bail!("终端附着状态需要有效的 scope_id 与 session_id");
        }
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        let session = sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {session_id}"))?;
        if session.scope_id.as_deref() != Some(scope_id) {
            bail!("终端 {session_id} 不属于当前会话");
        }
        drop(sessions);

        self.mark_frontend_attached(session_id);
        self.confirmed_frontend_tabs
            .lock()
            .expect("前端终端标签表锁损坏")
            .entry(scope_id.to_string())
            .or_default()
            .insert(session_id.to_string());
        Ok(OkResponse { ok: true })
    }

    pub(super) async fn wait_for_frontend_attach(&self, session_id: &str) -> bool {
        let deadline = Instant::now() + Duration::from_millis(FRONTEND_ATTACH_WAIT_MS);
        loop {
            if self
                .frontend_attached
                .lock()
                .expect("前端附着表锁损坏")
                .contains(session_id)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 前端在标签新建或关闭后提交当前会话的存活集合。页面并行挂载期间
    /// 集合可能暂时不完整，因此只回收曾被确认、后来从集合消失的 PTY。
    /// 调用只影响指定 scope，不增加后台周期任务。
    pub(super) fn gc_terminals(&self, request: TerminalGcRequest) -> Result<OkResponse> {
        let scope_id = request.session_id.trim();
        if scope_id.is_empty() {
            bail!("terminalGc 需要有效的 session_id");
        }
        if request.live_terminal_ids.len() > 1024 {
            bail!("terminalGc 的 live_terminal_ids 数量超过上限 1024");
        }
        let mut live = HashSet::with_capacity(request.live_terminal_ids.len());
        for terminal_id in request.live_terminal_ids {
            let terminal_id = terminal_id.trim();
            if terminal_id.is_empty() {
                bail!("terminalGc 的 live_terminal_ids 不能包含空编号");
            }
            live.insert(terminal_id.to_string());
        }

        let stale_confirmed_ids = {
            let mut confirmed = self
                .confirmed_frontend_tabs
                .lock()
                .expect("前端终端标签表锁损坏");
            let stale = confirmed
                .get(scope_id)
                .map(|previous| previous.difference(&live).cloned().collect::<HashSet<_>>())
                .unwrap_or_default();
            if live.is_empty() {
                confirmed.remove(scope_id);
            } else {
                confirmed.insert(scope_id.to_string(), live.clone());
            }
            stale
        };

        let (removed, has_remaining) = {
            let mut sessions = self.sessions.lock().expect("会话表锁损坏");
            let stale_ids = sessions
                .iter()
                .filter(|(session_id, session)| {
                    session.scope_id.as_deref() == Some(scope_id)
                        && stale_confirmed_ids.contains(*session_id)
                })
                .map(|(session_id, _)| session_id.clone())
                .collect::<Vec<_>>();
            let removed = stale_ids
                .into_iter()
                .filter_map(|session_id| {
                    sessions
                        .remove(&session_id)
                        .map(|session| (session_id, session))
                })
                .collect::<Vec<_>>();
            let has_remaining = sessions
                .values()
                .any(|session| session.scope_id.as_deref() == Some(scope_id));
            (removed, has_remaining)
        };

        let removed_ids = removed
            .iter()
            .map(|(session_id, _)| session_id.clone())
            .collect::<HashSet<_>>();
        if !removed_ids.is_empty() {
            self.frontend_attached
                .lock()
                .expect("前端附着表锁损坏")
                .retain(|terminal_id| !removed_ids.contains(terminal_id));
            self.active_tools
                .lock()
                .expect("活动工具表锁损坏")
                .retain(|_, active| !removed_ids.contains(&active.terminal_id));
        }
        for (session_id, mut session) in removed {
            let _ = session.killer.kill();
            tracing::info!(session_id, scope_id, "Terminal GC 已回收孤立终端");
        }
        if !has_remaining && live.is_empty() {
            persist::clear_scope_log(scope_id).context("Terminal GC 清理恢复记录失败")?;
        }
        Ok(OkResponse { ok: true })
    }

    /// 按宿主会话标识找活跃终端。精确编号存在时附着同一 PTY；精确编号
    /// 不存在时返回空结果，由页面创建全新 PTY，不把其他终端的会话日志
    /// 回放进新标签。
    pub(super) fn find_by_scope(&self, request: &FindRequest) -> FindResponse {
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        if let Some(session_id) = request.session_id.as_deref()
            && let Some(session) = sessions.get(session_id)
            && session.scope_id.as_deref() == Some(request.scope_id.as_str())
        {
            return FindResponse {
                session_id: Some(session_id.to_string()),
                history: String::from_utf8_lossy(&session.display_history).to_string(),
            };
        }
        if request.session_id.is_some() {
            return FindResponse {
                session_id: None,
                history: String::new(),
            };
        }
        if let Some((session_id, session)) = sessions
            .iter()
            .filter(|(_, session)| session.scope_id.as_deref() == Some(request.scope_id.as_str()))
            .max_by_key(|(_, session)| session.sequence)
        {
            return FindResponse {
                session_id: Some(session_id.clone()),
                history: String::from_utf8_lossy(&session.display_history).to_string(),
            };
        }
        FindResponse {
            session_id: None,
            history: String::new(),
        }
    }

    /// 列出宿主会话下全部存活终端（按创建序）。前端切换会话时恢复可见
    /// 标签用；已退出的终端（含死亡未出表）不返回——那些由既有收尾与
    /// GC 机制回收，恢复展示没有意义。
    pub(super) fn list_by_scope(
        &self,
        request: &ListByScopeRequest,
    ) -> Result<ListByScopeResponse> {
        let scope_id = request.scope_id.trim();
        if scope_id.is_empty() {
            bail!("terminalListByScope 需要有效的 scope_id");
        }
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        let mut terminals: Vec<(u64, String)> = sessions
            .iter()
            .filter(|(_, session)| {
                session.scope_id.as_deref() == Some(scope_id) && session.exited_code.is_none()
            })
            .map(|(session_id, session)| (session.sequence, session_id.clone()))
            .collect();
        terminals.sort_unstable();
        Ok(ListByScopeResponse {
            terminals: terminals
                .into_iter()
                .map(|(_, session_id)| ScopeTerminalEntry { session_id })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_plugin_sidecar::SidecarService as _;
    use tiangong_plugin_sidecar::protocol::Request;

    use crate::service::output::record_display_output;
    use crate::service::types::{SessionIdRequest, SpawnRequest, default_cols, default_rows};

    #[cfg(unix)]
    #[test]
    fn terminal_list_by_scope_只列同会话存活终端并按创建序() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawn = |session_id: &str, scope_id: &str| SpawnRequest {
            session_id: Some(session_id.to_string()),
            cmd: String::new(),
            args: Vec::new(),
            script: None,
            cwd: Some(cwd.path().to_string_lossy().to_string()),
            scope_id: Some(scope_id.to_string()),
            reserve: false,
            cols: default_cols(),
            rows: default_rows(),
        };
        service
            .spawn_session(spawn("terminal-first", "session-list"))
            .unwrap();
        service
            .spawn_session(spawn("terminal-second", "session-list"))
            .unwrap();
        service
            .spawn_session(spawn("terminal-other", "session-b"))
            .unwrap();
        service
            .spawn_session(spawn("terminal-dead", "session-list"))
            .unwrap();
        // 死亡未出表：shell 已退出但等命令收尾，列表不得恢复其标签。
        service
            .with_session("terminal-dead", |session| {
                session.exited_code = Some(-1);
                Ok(())
            })
            .unwrap();

        let listed = service
            .list_by_scope(&ListByScopeRequest {
                scope_id: "session-list".to_string(),
            })
            .expect("terminalListByScope 查询失败");
        let ids: Vec<&str> = listed
            .terminals
            .iter()
            .map(|terminal| terminal.session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["terminal-first", "terminal-second"]);

        // 前端经 bridgeCall 走 dispatch 通道，协议形状一并验证。
        let response = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("创建测试 runtime 失败")
            .block_on(service.dispatch(Request::new(
                "terminalListByScope",
                serde_json::json!({ "scope_id": "session-list" }),
            )));
        assert!(
            response.success,
            "terminalListByScope dispatch 失败: {response:?}"
        );
        assert_eq!(
            serde_json::to_value(&listed).expect("序列化失败"),
            response.payload.expect("dispatch 成功响应缺少 payload"),
        );

        for terminal in [
            "terminal-first",
            "terminal-second",
            "terminal-other",
            "terminal-dead",
        ] {
            service
                .kill_session(SessionIdRequest {
                    session_id: terminal.to_string(),
                })
                .unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminal_find_新编号不继承其他终端输出() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let old = service
            .spawn_session(SpawnRequest {
                session_id: Some("terminal-old".to_string()),
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("session-fresh".to_string()),
                reserve: false,
                cols: default_cols(),
                rows: default_rows(),
            })
            .expect("创建旧终端失败");
        record_display_output(&service.sessions, &old.session_id, b"old agent output\r\n");

        let found = service.find_by_scope(&FindRequest {
            scope_id: "session-fresh".to_string(),
            session_id: Some("terminal-new".to_string()),
        });
        assert!(found.session_id.is_none());
        assert!(found.history.is_empty(), "新终端不得继承旧终端输出");
        service
            .kill_session(SessionIdRequest {
                session_id: old.session_id,
            })
            .expect("清理旧终端失败");
    }
}
