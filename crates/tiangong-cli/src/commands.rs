use anyhow::{Result, anyhow};
use tiangong_app_state::app_state::TiangongState;
use tiangong_core::config::core::CoreConfigProvider;

use crate::completion;
use crate::modal;
use crate::output;

/// 处理 / 命令，返回 true 表示应退出。
pub fn handle_command(
    state: &mut TiangongState,
    config: &CoreConfigProvider,
    command: &str,
    _storage_root: &std::path::Path,
) -> Result<bool> {
    let command = command.trim();

    match command {
        "/exit" | "/quit" | "/q" => return Ok(true),
        "/help" | "/h" | "/?" => print_help(),
        "/new" => {
            state.active_session_id = scru128::new().to_string();
            output::print_status("已打开新对话（发送首条消息后才会记录）");
        }
        _ if command == "/history"
            || command == "/sessions"
            || command.starts_with("/history ")
            || command.starts_with("/sessions ") =>
        {
            let arg = command
                .trim_start_matches("/history")
                .trim_start_matches("/sessions")
                .trim();
            handle_sessions(state, arg)?;
        }
        "/cancel" => {
            if state.core_manager.cancel_core(&state.active_session_id) {
                output::print_status("已取消当前任务");
            } else {
                output::print_warn("当前没有可取消的任务");
            }
        }
        "/config" => {
            open_config(state, config)?;
        }
        _ => {
            output::print_warn(&format!("未知命令：{command}，输入 /help 查看可用命令"));
        }
    }

    Ok(false)
}

/// 打开网页配置页，页面关闭后重新加载配置供后续新对话使用。
fn open_config(state: &mut TiangongState, config: &CoreConfigProvider) -> Result<()> {
    output::print_status("正在打开配置页，完成后在页面点击\"完成并关闭\"返回对话…");
    crate::web_config::run(crate::web_config::WebConfigOptions::default())?;
    // 配置页直接写盘：重新加载，让新建会话使用最新模型、信任模式与 Prompt。
    let next = tiangong_config::registry::init();
    config.replace(next.to_core_config());
    state.config = next;
    output::print_status("配置已重新加载（对已开启的对话，模型与插件变更在新对话中生效）");
    Ok(())
}

fn print_help() {
    output::print_info(&completion::help_text());
}

fn handle_sessions(state: &mut TiangongState, arg: &str) -> Result<()> {
    let core_manager = state.core_manager.clone();
    if arg.is_empty() {
        if let Some(_id) = modal::sessions::open(state)? {
            let title = core_manager
                .load_session(&state.active_session_id)
                .ok()
                .map(|s| s.title)
                .unwrap_or_else(|| "未知".to_string());
            output::print_status(&format!("已切换会话：{title}"));
            if let Some(session) = core_manager.load_session(&state.active_session_id).ok()
                && !session.messages.is_empty()
            {
                output::print_session_messages(&session.messages);
            }
        }
        return Ok(());
    }

    // 按序号或 ID 前缀切换
    let sessions = core_manager.list_session_metadata();
    let target_id = if let Ok(idx) = arg.parse::<usize>() {
        sessions
            .get(idx.saturating_sub(1))
            .map(|m| m.id.clone())
            .ok_or_else(|| anyhow!("序号超出范围：{idx}"))?
    } else {
        sessions
            .iter()
            .find(|m| m.id.starts_with(arg))
            .map(|m| m.id.clone())
            .ok_or_else(|| anyhow!("未找到匹配的会话：{arg}"))?
    };

    let title = sessions
        .iter()
        .find(|m| m.id == target_id)
        .map(|m| m.title.clone())
        .unwrap_or_default();

    state.active_session_id = target_id.clone();
    output::print_status(&format!("已切换会话：{title}"));

    if let Some(session) = core_manager.load_session(&target_id).ok()
        && !session.messages.is_empty()
    {
        output::print_session_messages(&session.messages);
    }

    Ok(())
}
