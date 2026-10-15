//! PTY 会话生命周期：创建、选取复用、释放与回收。

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use tiangong_plugin_sidecar::server::emit_notification;

use super::output::{
    ExitNotification, OUTPUT_FLUSH_INTERVAL_MS, RawOutputFilter, publish_display_output,
    record_display_output, record_raw_output,
};
use super::readiness::{Foreground, foreground_of};
use super::shell::{
    ShellKind, default_shell, login_shell_args, shell_compatible_cwd, shell_ready_probe_completed,
};
use super::types::{
    AcquireRequest, AcquireResponse, CloseRequest, OkResponse, ScreenUpdateRequest,
    SessionIdRequest, SpawnRequest, SpawnResponse,
};
use super::{CHANNEL_EXIT, PtySession, SessionEndState, SessionPhase, TerminalService};
use crate::persist;

impl TerminalService {
    pub(super) fn spawn_session(&self, mut request: SpawnRequest) -> Result<SpawnResponse> {
        if let Some(cwd) = &mut request.cwd {
            *cwd = shell_compatible_cwd(cwd).to_string();
        }
        let _spawn_guard = self.spawn_lock.lock().expect("终端创建锁损坏");
        let requested_session_id = request
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if let Some(session_id) = requested_session_id.as_deref() {
            let mut sessions = self.sessions.lock().expect("会话表锁损坏");
            if let Some(session) = sessions.get_mut(session_id) {
                if session.scope_id.as_deref() != request.scope_id.as_deref() {
                    bail!("终端 {session_id} 已属于其他会话");
                }
                if request.reserve {
                    if session.phase != SessionPhase::Idle {
                        bail!("终端 {session_id} 正在使用");
                    }
                    session.phase = SessionPhase::Reserved;
                }
                return Ok(SpawnResponse {
                    session_id: session_id.to_string(),
                    boot_output: Some(
                        String::from_utf8_lossy(&session.display_history).to_string(),
                    ),
                });
            }
        }

        // 编号全局唯一（scru128）：sidecar 换代（崩溃重启/空闲退出/升级）
        // 后序号不回卷，Agent 持有的旧编号只会明确失败，不会误命中新终端。
        // sequence 与编号身份无关，仅用于同 scope 多终端时分辨最新。
        let (sequence, session_id) = loop {
            let sequence = self.next_sequence();
            let session_id = requested_session_id
                .clone()
                .unwrap_or_else(|| format!("tty-{}", scru128::new()));
            if !self
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .contains_key(&session_id)
            {
                break (sequence, session_id);
            }
        };
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: request.rows,
                cols: request.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("创建 PTY 失败")?;

        let shell_program = if request.cmd.is_empty() || request.script.is_some() {
            default_shell()
        } else {
            request.cmd.clone()
        };
        let shell_kind = ShellKind::of(&shell_program);
        // 脚本参数和交互协议均跟随实际 Shell。
        let mut command = if let Some(script) = request.script.as_deref() {
            let shell = shell_program.clone();
            let mut builder = CommandBuilder::new(shell);
            if shell_kind == ShellKind::PowerShell {
                builder.args(["-NoLogo", "-NoProfile", "-Command"]);
            } else if shell_kind == ShellKind::Cmd {
                builder.args(["/d", "/c"]);
            } else {
                builder.arg("-c");
            }
            builder.arg(script);
            builder
        } else if request.cmd.is_empty() {
            // 登录 shell 启动（对齐原版终端与 Terminal.app 行为）：source
            // /etc/profile 与 ~/.zprofile 等，拿到用户真实 PATH；并设置
            // TERM 让 zsh/zle 以全功能终端运行（否则语法高亮/补全降级错乱）。
            let shell = shell_program.clone();
            let mut builder = CommandBuilder::new(&shell);
            for arg in login_shell_args(&shell) {
                builder.arg(arg);
            }
            builder.env("TERM", "xterm-256color");
            builder.env("COLORTERM", "truecolor");
            builder
        } else {
            let mut builder = CommandBuilder::new(&request.cmd);
            builder.env("TERM", "xterm-256color");
            builder
        };
        for arg in &request.args {
            command.arg(arg);
        }
        // cwd 失效（目录被删等）不阻断会话创建：回退到继承进程工作目录
        if let Some(cwd) = request
            .cwd
            .as_deref()
            .filter(|cwd| std::path::Path::new(cwd).is_dir())
        {
            command.cwd(cwd);
        }

        let mut child = pair
            .slave
            .spawn_command(command)
            .context("启动 PTY 子进程失败")?;
        drop(pair.slave);

        let shell_pid = child.process_id();
        if let Some(pid) = shell_pid {
            self.jobs
                .register(pid, &session_id, request.scope_id.as_deref());
        }
        let killer = child.clone_killer();
        // 命令回显登记表：执行侧写入、输出过滤线程消费。
        let pending_echo: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // 输出持久化（按 scope 分文件）：仅供运行诊断和关闭时清理
        let logger = request
            .scope_id
            .as_deref()
            .and_then(persist::scope_log_path)
            .and_then(persist::OutputLogger::open)
            .map(Arc::new);
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("克隆 PTY 读取端失败")?;
        let writer = pair.master.take_writer().context("获取 PTY 写入端失败")?;
        let master = pair.master;

        // 必须先登记会话再启动输出搬运；否则启动提示符可能先于登记到达，
        // 原始输出和重放历史都会永久丢失。
        self.sessions.lock().expect("会话表锁损坏").insert(
            session_id.clone(),
            PtySession {
                shell: shell_kind,
                shell_ready: false,
                writer,
                killer,
                master,
                exec_lock: Arc::new(tokio::sync::Mutex::new(())),
                scope_id: request.scope_id,
                sequence,
                phase: if request.reserve {
                    SessionPhase::Reserved
                } else {
                    SessionPhase::Idle
                },
                raw_history: Vec::new(),
                raw_history_start: 0,
                raw_bytes_total: 0,
                last_output_at: Instant::now(),
                display_history: Vec::new(),
                screen_snapshot: String::new(),
                screen_updates: 0,
                exited_code: None,
                pending_ready_marker: None,
                pending_send: None,
                send_read_offset: None,
                prompt_hint: None,
                running_command: None,
                interactive_started: false,
                shell_pid,
                logger,
                pending_echo: Arc::clone(&pending_echo),
            },
        );

        // 输出管道：原始流保留给命令完成协议，用户可见流先过滤内部
        // marker 和包含 marker 的 shell 回显，再落盘、推送与重放。
        let output_session = session_id.clone();
        let output_sessions = Arc::clone(&self.sessions);
        let boot_sessions = Arc::clone(&self.sessions);
        let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(bytes) => {
                        if chunk_tx.send(buffer[..bytes].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut output_filter = RawOutputFilter::new(Arc::clone(&pending_echo));
        // 首批提示符随响应返回，避免 UI 订阅尚未建立时永久丢帧。
        let boot_output = match chunk_rx.recv_timeout(Duration::from_millis(40)) {
            Ok(first) => {
                record_raw_output(&boot_sessions, &output_session, &first);
                let filtered = output_filter.filter(&String::from_utf8_lossy(&first));
                record_display_output(&boot_sessions, &output_session, filtered.as_bytes());
                (!filtered.is_empty()).then_some(filtered)
            }
            Err(_) => None,
        };
        std::thread::spawn(move || {
            while let Ok(first) = chunk_rx.recv() {
                let mut pending = first;
                while let Ok(more) =
                    chunk_rx.recv_timeout(Duration::from_millis(OUTPUT_FLUSH_INTERVAL_MS))
                {
                    pending.extend_from_slice(&more);
                    if pending.len() >= 64 * 1024 {
                        break;
                    }
                }
                record_raw_output(&output_sessions, &output_session, &pending);
                let filtered = output_filter.filter(&String::from_utf8_lossy(&pending));
                if !filtered.is_empty() {
                    publish_display_output(&output_sessions, &output_session, &filtered);
                }
            }
        });

        // 等待线程：退出通知 + 标记死亡（不出表）。正在执行的命令仍需
        // 从会话读取输出与退出码正常收尾；出表由命令收尾或下次选终端
        // 时完成，write/resize 等操作在此期间按会话状态自行判断。
        let exit_session = session_id.clone();
        let exit_sequence = sequence;
        let exit_sessions = Arc::clone(&self.sessions);
        std::thread::spawn(move || {
            let status = child.wait();
            let exit_code = status.as_ref().ok().map(|status| status.exit_code());
            let payload = serde_json::to_string(&ExitNotification {
                session_id: &exit_session,
                exit_code,
            })
            .unwrap_or_default();
            emit_notification(CHANNEL_EXIT, payload);
            let mut sessions = exit_sessions.lock().expect("会话表锁损坏");
            if sessions
                .get(&exit_session)
                .is_some_and(|session| session.sequence == exit_sequence)
                && let Some(session) = sessions.get_mut(&exit_session)
            {
                session.exited_code = Some(exit_code.map(|code| code as i32).unwrap_or(-1));
            }
        });

        Ok(SpawnResponse {
            session_id,
            boot_output,
        })
    }

    /// 回收已死亡的会话（出表并清前端附着标记）。命令收尾时调用，
    /// 防御性保留给选终端等路径复用。
    pub(super) fn reap_ended_session(&self, session_id: &str) {
        let removed = self
            .sessions
            .lock()
            .expect("会话表锁损坏")
            .remove(session_id);
        if removed.is_some() {
            self.frontend_attached
                .lock()
                .expect("前端附着表锁损坏")
                .remove(session_id);
        }
    }

    /// 会话相对死亡/回收的三态：调用方需区分「活着」（正常继续）、
    /// 「已退出带码」（按死亡收尾）与「已被回收」（手动关闭或回收路径
    /// 先一步出表，读输出只会撞「会话不存在」，须给出明确错误）。
    pub(super) fn session_end_state(&self, session_id: &str) -> SessionEndState {
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        match sessions.get(session_id) {
            Some(session) => match session.exited_code {
                Some(code) => SessionEndState::Ended(code),
                None => SessionEndState::Alive,
            },
            None => SessionEndState::Reaped,
        }
    }

    pub(super) fn kill_session(&self, request: SessionIdRequest) -> Result<OkResponse> {
        let session_id = request.session_id;
        let removed = self
            .sessions
            .lock()
            .expect("会话表锁损坏")
            .remove(&session_id);
        match removed {
            Some(mut session) => {
                self.frontend_attached
                    .lock()
                    .expect("前端附着表锁损坏")
                    .remove(&session_id);
                let _ = session.killer.kill();
                Ok(OkResponse { ok: true })
            }
            None => Ok(OkResponse { ok: false }),
        }
    }

    pub(super) fn close_session(&self, request: CloseRequest) -> Result<OkResponse> {
        let closing_session_id = request.session_id.clone();
        let (removed, has_other_scope_sessions) = {
            let mut sessions = self.sessions.lock().expect("会话表锁损坏");
            let removed = if let Some(session_id) = request.session_id.as_deref() {
                if let Some(session) = sessions.get(session_id)
                    && session.scope_id.as_deref() != Some(request.scope_id.as_str())
                {
                    bail!("终端 {session_id} 不属于当前会话");
                }
                sessions.remove(session_id)
            } else {
                None
            };
            let has_other_scope_sessions = sessions
                .values()
                .any(|session| session.scope_id.as_deref() == Some(request.scope_id.as_str()));
            (removed, has_other_scope_sessions)
        };
        if let Some(mut session) = removed {
            if let Some(session_id) = closing_session_id {
                self.frontend_attached
                    .lock()
                    .expect("前端附着表锁损坏")
                    .remove(&session_id);
            }
            let _ = session.killer.kill();
        }
        if !has_other_scope_sessions {
            persist::clear_scope_log(&request.scope_id).context("清理终端恢复记录失败")?;
        }
        Ok(OkResponse { ok: true })
    }

    pub(super) fn acquire_session(&self, request: AcquireRequest) -> AcquireResponse {
        let mut sessions = self.sessions.lock().expect("会话表锁损坏");
        let had_live_terminal = sessions.values().any(|session| {
            session.scope_id.as_deref() == Some(request.scope_id.as_str())
                && session.exited_code.is_none()
        });
        // 防御性回收已死亡但尚未被命令收尾清理的会话（出表并清附着标记）。
        // 仅限本会话（scope）且空闲的死亡终端：执行中/交互中的死亡会话
        // 正被它自己的命令等待收尾读取（感知窗口最长一个轮询周期），
        // 选终端不得抢先摘表，否则该命令撞回「会话不存在」；其他会话的
        // 死终端同理留给其收尾路径。
        let ended_ids: Vec<String> = sessions
            .iter()
            .filter(|(_, session)| {
                session.exited_code.is_some()
                    && session.phase == SessionPhase::Idle
                    && session.scope_id.as_deref() == Some(request.scope_id.as_str())
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        sessions.retain(|_, session| {
            session.exited_code.is_none()
                || session.phase != SessionPhase::Idle
                || session.scope_id.as_deref() != Some(request.scope_id.as_str())
        });
        if !ended_ids.is_empty() {
            let mut attached = self.frontend_attached.lock().expect("前端附着表锁损坏");
            for session_id in &ended_ids {
                attached.remove(session_id);
            }
        }
        // 曾就绪确认超时的终端自行恢复后重新可用：遗留在 shell 输入缓冲
        // 里的探针会在前台进程树收尾后被执行并回显 marker。长任务跑完
        // （或后台作业转入后台）后终端就该回到复用池，而不是永久闲置。
        let recovered: Vec<String> = sessions
            .iter()
            .filter(|(_, session)| {
                session.phase == SessionPhase::Unresponsive
                    && session.exited_code.is_none()
                    && session.scope_id.as_deref() == Some(request.scope_id.as_str())
                    && session
                        .pending_ready_marker
                        .as_deref()
                        .is_some_and(|marker| {
                            shell_ready_probe_completed(
                                &String::from_utf8_lossy(&session.raw_history),
                                marker,
                            )
                        })
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in recovered {
            if let Some(session) = sessions.get_mut(&session_id) {
                session.phase = SessionPhase::Idle;
                session.shell_ready = true;
                session.pending_ready_marker = None;
                session.running_command = None;
                tracing::info!(session_id, "终端前台命令已收尾，提示符恢复，重新加入复用池");
            }
        }
        // 空闲但前台被程序占据的终端不复用：Agent 经 terminal_send 进入了
        // ssh / REPL，或发出的命令还在跑——此时 phase 仍是 Idle，但 shell
        // 并不在提示符。往里写本地包装脚本会被远端 shell / 程序当作输入
        // 执行，必然失败并污染该会话（issue #673）。terminal_send 登记的
        // 未完成命令同理。取不到前台信息（Windows）时按原逻辑复用。
        let selected = sessions
            .iter()
            .filter(|(_, session)| {
                session.scope_id.as_deref() == Some(request.scope_id.as_str())
                    && session.phase == SessionPhase::Idle
                    && session.pending_send.is_none()
                    && !matches!(foreground_of(session), Some(Foreground::Program { .. }))
            })
            .min_by_key(|(_, session)| session.sequence)
            .map(|(session_id, _)| session_id.clone());
        let Some(session_id) = selected else {
            return AcquireResponse {
                session_id: None,
                history: String::new(),
                reason: if had_live_terminal {
                    "all_busy".to_string()
                } else {
                    "no_available".to_string()
                },
            };
        };
        let session = sessions.get_mut(&session_id).expect("刚选择的终端不存在");
        session.phase = SessionPhase::Reserved;
        AcquireResponse {
            session_id: Some(session_id),
            history: String::from_utf8_lossy(&session.display_history).to_string(),
            reason: "reused_idle".to_string(),
        }
    }

    pub(super) fn release_session(&self, request: SessionIdRequest) -> Result<OkResponse> {
        self.with_session(&request.session_id, |session| {
            if session.phase == SessionPhase::Reserved {
                session.phase = SessionPhase::Idle;
            }
            Ok(OkResponse { ok: true })
        })
    }

    pub(super) fn update_screen(&self, request: ScreenUpdateRequest) -> Result<OkResponse> {
        self.with_session(&request.session_id, |session| {
            session.screen_snapshot = request.screen;
            session.screen_updates = session.screen_updates.saturating_add(1);
            Ok(OkResponse { ok: true })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn acquire_does_not_reap_running_ended_session() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        // 模拟「shell 刚死、命令尚未收尾」：执行中的死亡会话不被选终端回收，
        // 留给它自己的收尾路径。
        {
            let mut sessions = service.sessions.lock().expect("会话表锁损坏");
            let session = sessions.get_mut(&spawned.session_id).expect("会话应在表");
            session.exited_code = Some(3);
            session.phase = SessionPhase::Running;
        }
        let acquired = service.acquire_session(AcquireRequest {
            scope_id: "terminal-sidecar-test".to_string(),
        });
        assert!(
            service
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .get(&spawned.session_id)
                .is_some(),
            "执行中的死亡会话不应被选终端回收"
        );
        assert_ne!(
            acquired.session_id.as_deref(),
            Some(spawned.session_id.as_str())
        );
        if let Some(new_session) = acquired.session_id {
            service
                .kill_session(SessionIdRequest {
                    session_id: new_session,
                })
                .expect("清理新终端失败");
        }

        // 空闲后（命令已收尾）：同会话再次选终端时应被回收。
        {
            let mut sessions = service.sessions.lock().expect("会话表锁损坏");
            let session = sessions.get_mut(&spawned.session_id).expect("会话应在表");
            session.phase = SessionPhase::Idle;
        }
        let _ = service.acquire_session(AcquireRequest {
            scope_id: "terminal-sidecar-test".to_string(),
        });
        assert!(
            service
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .get(&spawned.session_id)
                .is_none(),
            "空闲的死亡会话应被选终端回收"
        );
    }
}
