//! CLI REPL — 类似 codex/claude code 风格交互

use anyhow::Result;
use tiangong_core::agent_input::AgentInputKind;
use tiangong_types::StreamEvent;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::commands;
use crate::completion;
use crate::input::InputReader;
use crate::output;

pub fn run(trust_mode: Option<tiangong_core::permission::TrustMode>) -> Result<()> {
    // 初始化日志：输出到 stderr（不污染 stdout 的流式对话/交互输出）。
    // 默认 warn 级别以上，可用 RUST_LOG 覆盖。多次调用（如测试）安全忽略。
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let mut state = tiangong_app_state::app_state::TiangongState::new();
    let config = state.core_manager.config().clone();
    let storage_root = state.config.storage_root.clone();
    state.active_session_id = scru128::new().to_string();
    state.workspace_dir = std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    // MCP 插件：dual-ownership——core 拿 clone 做 LLM 工具（动态 MCP 工具 spec +
    // 执行分发），CLI 侧经 mcp_plugin 做管理（modal 里的 add/remove/toggle、
    // /config set mcp.*、@mcp 补全、skill 删除后的孤儿 MCP 清理）。
    let (stream_tx, stream_rx) = mpsc::channel::<StreamEvent>();
    // 对齐 server 入口（tiangong-server/src/lib.rs）：多线程 runtime + enter()，
    // 让主线程在整个 REPL 生命周期内持有 reactor guard。这样主循环里同步执行的
    // deliver_to_core_if_live → on_config_updated → tokio::spawn 才能找到 reactor
    // （见 issue #313）。Core 的 turn 循环仍由进程级共享 runtime 独立承载。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _runtime_guard = runtime.enter();
    let default_trust_mode = trust_mode.unwrap_or(state.config.default_trust_mode);
    let mut reader = InputReader::new();

    output::welcome();

    loop {
        // 消费残留事件
        while stream_rx.try_recv().is_ok() {}

        // prompt
        let short_id: String = state.active_session_id.chars().take(8).collect();
        let prompt = format!("\x1b[2m{short_id}\x1b[0m \x1b[1;36m❯\x1b[0m ");

        let input = {
            let storage_root = storage_root.clone();
            reader.read_line(&prompt, move |buf, cursor| {
                if let Some((trigger, _start, prefix)) = completion::detect_trigger(buf, cursor) {
                    completion::complete(trigger, &prefix, &storage_root)
                } else {
                    Vec::new()
                }
            })?
        };

        let input = match input {
            Some(line) => line,
            None => break,
        };

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        reader.push_history(trimmed);

        // / 命令（通过 TiangongState 处理）
        if trimmed.starts_with('/') {
            match commands::handle_command(&mut state, &config, trimmed, &storage_root) {
                Ok(true) => break,
                Ok(false) => continue,
                Err(err) => {
                    output::error(&format!("{err}"));
                    continue;
                }
            }
        }

        let session_id = state.active_session_id.clone();
        let mut session_config = (*config.snapshot()).clone();
        let workspace_dir = if let Ok(session) = state.core_manager.load_session(&session_id) {
            session_config.trust_mode = session.trust_mode;
            if let Some(reasoning_effort) = session.reasoning_effort {
                session_config.reasoning_effort = reasoning_effort;
            }
            if session.cwd.trim().is_empty() {
                state.workspace_dir.clone()
            } else {
                session.cwd
            }
        } else {
            session_config.trust_mode = default_trust_mode;
            state.workspace_dir.clone()
        };
        let plugins = if state.core_manager.has_live_core(&session_id) {
            Vec::new()
        } else {
            build_cli_plugins(&storage_root, &state.config.models)
        };
        runtime
            .block_on(state.core_manager.ensure_core(
                &session_id,
                session_config,
                workspace_dir,
                None,
                stream_tx.clone(),
                || plugins,
            ))
            .map_err(anyhow::Error::msg)?;

        // Session 由 Core 在首次 deliver 中创建并持久化。
        if !state
            .core_manager
            .deliver_to_core_if_live(&session_id, AgentInputKind::message(trimmed.to_string()))
        {
            output::error("消息投递失败");
            continue;
        }

        // 处理响应流
        handle_response(&stream_rx);

        output::separator();
    }

    output::status("再见！");
    let live_session_ids = {
        let registry = state.core_manager.registry();
        registry.keys().cloned().collect::<Vec<_>>()
    };
    for session_id in live_session_ids {
        if let Some(core) = state.core_manager.take_core(&session_id)
            && let Err(error) = core.shutdown_join()
        {
            tracing::warn!(%session_id, %error, "终止 Core 失败");
        }
    }
    Ok(())
}

fn build_cli_plugins(
    storage_root: &std::path::Path,
    _models: &tiangong_llm::models_config::ModelsConfig,
) -> Vec<std::sync::Arc<dyn tiangong_core::core::Plugin>> {
    let storage_root = storage_root.to_path_buf();

    let mut plugins: Vec<std::sync::Arc<dyn tiangong_core::core::Plugin>> = Vec::new();
    plugins.extend(tiangong_plugin_runtime::registry::load_installed_plugins(
        &storage_root,
        tiangong_plugin_runtime::registry::RuntimeKind::Cli,
    ));
    // web_fetch 由 runtime 按 plugin.json 自动加载 fetch WASM 插件（issue #326）。
    // 不注册 scheduler 插件：定时任务属于 Desktop / Server 这类长期运行宿主的能力。
    // CLI 作为前台交互工具，生命周期不稳定，不承载调度执行（见 issue 说明）。
    plugins
}

/// 处理完整的响应流
fn handle_response(rx: &mpsc::Receiver<StreamEvent>) {
    let mut state = ResponseState::new();
    let mut last_event_at = Instant::now();
    let timeout = Duration::from_secs(300);
    let poll_interval = Duration::from_millis(50);

    loop {
        let mut had_event = false;

        loop {
            match rx.try_recv() {
                Ok(session_event) => {
                    had_event = true;
                    last_event_at = Instant::now();
                    if state.process(&session_event) {
                        return;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    state.finish();
                    output::error("响应流已断开");
                    break;
                }
            }
        }

        if last_event_at.elapsed() >= timeout {
            state.finish();
            output::error("等待响应超时（300s）");
            break;
        }

        if !had_event {
            std::thread::sleep(poll_interval);
        }
    }
}

/// 响应流状态机
struct ResponseState {
    active_stream: ActiveStream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActiveStream {
    Idle,
    Reasoning { message_id: String },
    Assistant { message_id: String },
    Worker { worker_id: String },
}

impl ResponseState {
    fn new() -> Self {
        Self {
            active_stream: ActiveStream::Idle,
        }
    }

    /// 处理单个事件，返回 true 表示本轮结束
    fn process(&mut self, event: &StreamEvent) -> bool {
        match event {
            StreamEvent::TurnBoundary { .. } => {}
            StreamEvent::UserMessage { .. } => {
                // CLI 模式下用户消息由 REPL 自己显示，忽略
            }
            StreamEvent::SessionMessageUpsert { .. } => {
                // Core 已持有权威会话；CLI 不维护第二份消息镜像。
            }
            StreamEvent::DeferredToolInjectionsChanged { .. } => {
                // Core 已持久化；CLI 不维护第二份会话镜像。
            }
            StreamEvent::Reasoning {
                message_id,
                content,
            } => {
                self.ensure_reasoning_stream(message_id);
                output::explanation_delta(content);
            }

            StreamEvent::Delta {
                message_id,
                content,
            }
            | StreamEvent::ReactText {
                message_id,
                content,
            }
            | StreamEvent::SummaryText {
                message_id,
                content,
            } => {
                self.ensure_assistant_stream(message_id);
                output::delta(content);
            }

            StreamEvent::PhaseChanged { .. } => {}
            StreamEvent::TurnElapsed { .. } => {}

            StreamEvent::ToolCalls { names, .. } => {
                self.end_active_stream();
                output::tool_calls(names);
            }

            StreamEvent::ToolStart { name, args_summary } => {
                self.end_active_stream();
                if args_summary.is_empty() {
                    output::tool_start(name);
                } else {
                    output::tool_start(&format!("{name} {args_summary}"));
                }
            }

            // 仅用于界面运行计时；CLI 以按序提交的 ToolResult 输出结果。
            StreamEvent::ToolFinished { .. } => {}

            StreamEvent::ToolResult {
                name, ok, output, ..
            } => {
                self.end_active_stream();
                output::tool_result(name, *ok, output);
            }

            StreamEvent::TokenUsage { .. } => {}

            StreamEvent::Done { .. } => {
                self.finish();
                return true;
            }

            StreamEvent::Error { message } => {
                self.finish();
                output::error(message);
                return true;
            }

            StreamEvent::Retry {
                message,
                attempt,
                max_attempts,
            } => {
                self.end_active_stream();
                output::warn(&format!("重试 ({attempt}/{max_attempts})：{message}"));
            }

            StreamEvent::WorkerStarted {
                worker_id: _,
                worker_label,
            } => {
                self.end_active_stream();
                output::worker_started(worker_label);
            }

            StreamEvent::WorkerChunk {
                worker_id,
                worker_label,
                content,
            } => {
                self.ensure_worker_stream(worker_id, worker_label);
                output::worker_stream_delta(content);
            }

            StreamEvent::WorkerCompleted {
                worker_id: _,
                worker_label,
                success,
            } => {
                self.end_active_stream();
                output::worker_completed(worker_label, *success);
            }

            StreamEvent::MemoryRecallStart { strategy } => {
                self.end_active_stream();
                output::status(&format!("记忆检索 (策略: {strategy})..."));
            }

            StreamEvent::MemoryRecallProgress { phase } => {
                output::status(&format!("记忆检索: {phase}..."));
            }

            StreamEvent::MemoryRecallDone { hit_count, hits } => {
                if *hit_count == 0 {
                    output::status("记忆检索完成，无相关记忆");
                } else {
                    output::status(&format!("记忆检索完成，命中 {hit_count} 条"));
                    for h in hits {
                        output::status(&format!("  [{:.2}] {}: {}", h.score, h.title, h.summary));
                    }
                }
            }

            // ===== 多智能体团队事件 =====
            StreamEvent::AgentCreated { role, label, .. } => {
                self.end_active_stream();
                output::status(&format!("[{role}] {label} 已加入团队"));
            }

            StreamEvent::AgentStatusChanged { label, status, .. } => {
                output::status(&format!("[{label}] 状态: {status}"));
            }

            StreamEvent::AgentNotification {
                agent_label,
                content,
                level,
                ..
            } => {
                self.end_active_stream();
                match level.as_str() {
                    "error" => output::error(&format!("[{agent_label}] {content}")),
                    "warning" => output::warn(&format!("[{agent_label}] {content}")),
                    "question" => output::status(&format!("[{agent_label}] ❓ {content}")),
                    _ => output::status(&format!("[{agent_label}] {content}")),
                }
            }

            StreamEvent::AgentMessage {
                from_agent_label,
                to_agent_label,
                content,
                ..
            } => {
                output::status(&format!(
                    "[{from_agent_label} → {to_agent_label}] {content}"
                ));
            }

            StreamEvent::AgentOutput {
                agent_label,
                messages,
                ..
            } => {
                self.end_active_stream();
                output::status(&format!("[{agent_label}] 输出 {} 条消息", messages.len()));
            }

            StreamEvent::FileLockChanged {
                path,
                holder_agent_label,
                action,
                ..
            } => {
                // 进程级文件锁不绑定 Agent，holder 通常为空；仅在确有持有者时显示。
                match holder_agent_label.as_deref() {
                    Some(holder) => {
                        output::status(&format!("文件锁 {action}: {} (by {holder})", path))
                    }
                    None => output::status(&format!("文件锁 {action}: {}", path)),
                }
            }

            StreamEvent::ContextCompressing {
                summary_up_to,
                total_messages,
            } => {
                self.end_active_stream();
                output::status(&format!(
                    "正在压缩上下文: 已摘要 {summary_up_to} 条，当前 {total_messages} 条消息"
                ));
            }

            StreamEvent::ContextCompressed {
                action,
                summary_up_to,
                remaining_messages,
            } => {
                self.end_active_stream();
                output::status(&format!(
                    "上下文{}: 已处理 {summary_up_to} 条，剩余 {remaining_messages} 条",
                    action.display_text()
                ));
            }

            StreamEvent::ModelSwitchStarted { model_name, .. } => {
                self.end_active_stream();
                output::status(&format!("正在切换模型: {model_name}"));
            }

            StreamEvent::ModelSwitched { model_name, .. } => {
                self.end_active_stream();
                output::status(&format!("已切换至模型: {model_name}"));
            }

            StreamEvent::IndexStatus { phase, count } => {
                if phase == "done" {
                    output::status(&format!("索引扫描完成: {count} 个文件"));
                }
            }
            // 标题变更由 GUI 消费；CLI 不维护会话列表视图，无需处理。
            StreamEvent::TitleChanged { .. } => {}
        }
        false
    }

    fn ensure_reasoning_stream(&mut self, message_id: &str) {
        if !matches!(
            &self.active_stream,
            ActiveStream::Reasoning {
                message_id: current_message_id
            } if current_message_id == message_id
        ) {
            self.end_active_stream();
            output::explanation_start();
            self.active_stream = ActiveStream::Reasoning {
                message_id: message_id.to_string(),
            };
        }
    }

    fn ensure_assistant_stream(&mut self, message_id: &str) {
        if !matches!(
            &self.active_stream,
            ActiveStream::Assistant {
                message_id: current_message_id
            } if current_message_id == message_id
        ) {
            self.end_active_stream();
            output::assistant_start();
            self.active_stream = ActiveStream::Assistant {
                message_id: message_id.to_string(),
            };
        }
    }

    fn ensure_worker_stream(&mut self, worker_id: &str, worker_label: &str) {
        if !matches!(
            &self.active_stream,
            ActiveStream::Worker {
                worker_id: current_worker_id
            } if current_worker_id == worker_id
        ) {
            self.end_active_stream();
            output::worker_stream_start(worker_label);
            self.active_stream = ActiveStream::Worker {
                worker_id: worker_id.to_string(),
            };
        }
    }

    fn end_active_stream(&mut self) {
        match self.active_stream {
            ActiveStream::Idle => {}
            ActiveStream::Reasoning { .. } => output::explanation_end(),
            ActiveStream::Assistant { .. } => output::delta_end(),
            ActiveStream::Worker { .. } => output::worker_stream_end(),
        }
        self.active_stream = ActiveStream::Idle;
    }

    fn finish(&mut self) {
        self.end_active_stream();
        println!();
    }
}
