//! CLI 输出格式化 — 类似 codex/claude code 风格

use std::io::Write;

// ANSI 颜色码
const CYAN: &str = "\x1b[36m";
const CYAN_BOLD: &str = "\x1b[1;36m";
const GREEN_BOLD: &str = "\x1b[1;32m";
const BLUE: &str = "\x1b[34m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const RED_BOLD: &str = "\x1b[1;31m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// 终端宽度
fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

/// 打印分隔线
pub fn separator() {
    let w = term_width();
    println!("{DIM}{}{RESET}", "─".repeat(w));
}

/// 打印用户消息
pub fn user_message(content: &str) {
    println!();
    println!("{CYAN_BOLD}❯ {RESET}{BOLD}{content}{RESET}");
}

/// 打印解释文本流开始
pub fn explanation_start() {
    print!("\n{DIM}💭 ");
    let _ = std::io::stdout().flush();
}

/// 打印解释文本增量
pub fn explanation_delta(text: &str) {
    print!("{text}");
    let _ = std::io::stdout().flush();
}

/// 打印解释文本流结束
pub fn explanation_end() {
    println!("{RESET}");
}

/// 打印助手回复开始标记
pub fn assistant_start() {
    println!();
}

/// 打印流式文本增量
pub fn delta(text: &str) {
    print!("{text}");
    let _ = std::io::stdout().flush();
}

/// 流式结束换行
pub fn delta_end() {
    println!();
}

/// 打印工具调用意图
pub fn tool_calls(names: &[String]) {
    println!();
    for name in names {
        println!("{BLUE}  {name}{RESET}");
    }
}

/// 打印工具开始执行
pub fn tool_start(name: &str) {
    print!("{DIM}  {name}...{RESET}");
    let _ = std::io::stdout().flush();
}

/// 打印工具执行结果
pub fn tool_result(name: &str, ok: bool, output: &str) {
    // 清除 tool_start 的行
    print!("\r\x1b[2K");
    let _ = std::io::stdout().flush();

    if ok {
        let preview = output_preview(output);
        println!("{GREEN_BOLD}  {name}{RESET} {DIM}{preview}{RESET}");
    } else {
        let preview = output_preview(output);
        println!("{RED}  {name} 失败{RESET} {DIM}{preview}{RESET}");
    }
}

/// 打印错误
pub fn error(msg: &str) {
    println!("{RED_BOLD}✗ 错误：{msg}{RESET}");
}

/// 打印状态信息
pub fn status(msg: &str) {
    println!("{DIM}{msg}{RESET}");
}

/// 打印警告
pub fn warn(msg: &str) {
    println!("{YELLOW}⚠ {msg}{RESET}");
}

/// 打印欢迎信息
pub fn welcome() {
    println!("{CYAN}天工 CLI{RESET} — {DIM}/help 查看命令，Ctrl+C 清空/退出{RESET}");
    println!();
}

/// 输出预览（单行，截断）
fn output_preview(output: &str) -> String {
    let first_line = output.lines().next().unwrap_or("");
    if first_line.chars().count() > 60 {
        let truncated: String = first_line.chars().take(57).collect();
        format!("{truncated}...")
    } else if output.lines().count() > 1 {
        format!("{first_line} ...")
    } else {
        first_line.to_string()
    }
}

/// 打印历史会话消息（切换会话后回显）。
pub fn session_messages(messages: &[tiangong_core::session::Message]) {
    for msg in messages {
        match msg.kind() {
            tiangong_core::session::MessageRole::User => {
                let text = msg
                    .content
                    .iter()
                    .filter_map(|block| block.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                user_message(&text);
            }
            tiangong_core::session::MessageRole::Assistant => assistant_message(msg),
            tiangong_core::session::MessageRole::System
            | tiangong_core::session::MessageRole::Notice
            | tiangong_core::session::MessageRole::Tool => system_message(msg),
        }
    }
}

/// 助手消息回显。
fn assistant_message(msg: &tiangong_core::session::Message) {
    println!("{GREEN_BOLD}助手{RESET}");
    let content = msg
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join("");
    let content = content.trim();
    if !content.is_empty() {
        for line in content.lines() {
            println!("  {line}");
        }
    }
    println!();
}

/// 系统 / 通知 / 工具消息回显（暗色行）。
fn system_message(msg: &tiangong_core::session::Message) {
    let content = msg
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join("");
    let content = content.trim();
    if !content.is_empty() {
        for line in content.lines() {
            println!("{DIM}{line}{RESET}");
        }
    }
}
