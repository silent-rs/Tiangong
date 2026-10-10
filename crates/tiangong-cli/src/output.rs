//! CLI 输出格式化 — 类似 codex/claude code 风格

use std::io::Write;

// ANSI 颜色码
const CYAN: &str = "\x1b[36m";
const CYAN_BOLD: &str = "\x1b[1;36m";
const GREEN_BOLD: &str = "\x1b[1;32m";
const BLUE: &str = "\x1b[34m";
#[allow(dead_code)]
const MAGENTA: &str = "\x1b[35m";
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

/// 打印 thinking 开始指示
#[allow(dead_code)]
pub fn thinking_start() {
    print!("{DIM}  ⏳ 思考中...{RESET}");
    let _ = std::io::stdout().flush();
}

/// 清除 thinking 指示（覆盖当前行）
#[allow(dead_code)]
pub fn thinking_clear() {
    print!("\r\x1b[2K"); // 回到行首 + 清除整行
    let _ = std::io::stdout().flush();
}

/// 打印 thinking 摘要（折叠格式）
#[allow(dead_code)]
pub fn thinking_summary(reasoning: &str) {
    let trimmed = reasoning.trim();
    if trimmed.is_empty() {
        return;
    }
    let char_count = trimmed.chars().count();
    let summary: String = if char_count > 60 {
        let truncated: String = trimmed.chars().take(57).collect();
        format!("{truncated}...")
    } else {
        trimmed.to_string()
    };
    println!("{DIM}  💭 思考 ({char_count} 字符) — {summary}{RESET}");
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
        let icon = tool_icon(name);
        println!("{BLUE}  {icon} {name}{RESET}");
    }
}

/// 打印工具开始执行
pub fn tool_start(name: &str) {
    let icon = tool_icon(name);
    print!("{DIM}  {icon} {name}...{RESET}");
    let _ = std::io::stdout().flush();
}

/// 打印工具执行结果
pub fn tool_result(name: &str, ok: bool, output: &str) {
    // 清除 tool_start 的行
    print!("\r\x1b[2K");
    let _ = std::io::stdout().flush();

    let icon = tool_icon(name);
    if ok {
        let preview = output_preview(output);
        println!("{GREEN_BOLD}  {icon} {name}{RESET} {DIM}{preview}{RESET}");
    } else {
        let preview = output_preview(output);
        println!("{RED}  {icon} {name} 失败{RESET} {DIM}{preview}{RESET}");
    }
}

/// 打印审批请求
#[allow(dead_code)]
pub fn approval_needed(tool_name: &str, args_summary: &str) {
    println!("{YELLOW}  🔒 {tool_name} 需要审批{RESET}");
    println!("{DIM}     {args_summary}{RESET}");
}

/// 打印错误
pub fn error(msg: &str) {
    println!("{RED_BOLD}✗ 错误：{msg}{RESET}");
}

/// 打印完成标记
#[allow(dead_code)]
pub fn done() {
    // 静默，不需要额外标记
}

/// 打印状态信息
pub fn status(msg: &str) {
    println!("{DIM}{msg}{RESET}");
}

/// 打印警告
pub fn warn(msg: &str) {
    println!("{YELLOW}⚠ {msg}{RESET}");
}

/// 使用 Markdown 渲染输出完整文本块
#[allow(dead_code)]
pub fn render_markdown(text: &str) {
    let skin = termimad::MadSkin::default();
    let rendered = skin.term_text(text);
    // 缩进每行
    for line in rendered.to_string().lines() {
        println!("  {line}");
    }
}

/// 打印欢迎信息
pub fn welcome() {
    println!("{CYAN}天工 CLI{RESET} — {DIM}/help 查看命令，Ctrl+C 清空/退出{RESET}");
    println!();
}

/// 打印 prompt 前的状态
#[allow(dead_code)]
pub fn prompt_status(session_id: &str) {
    let short: String = session_id.chars().take(8).collect();
    print!("{DIM}[{short}]{RESET} ");
    let _ = std::io::stdout().flush();
}

/// 工具图标
fn tool_icon(name: &str) -> &'static str {
    // 重名工具按 `{插件id}__{工具名}` 暴露：按原名取图标（MCP 工具名保持原样）。
    let name = match name.split_once("__") {
        Some((_, base)) if !name.starts_with("mcp__") => base,
        _ => name,
    };
    match name {
        "read_file" => "📄",
        "write_file" | "replace_in_file" => "✏️",
        "list_dir" => "📂",
        "tree_dir" => "🌳",
        "search_code" => "🔍",
        "run_command" | "run_shell" => "⚡",
        "apply_patch" => "🩹",
        "generate_image" => "🎨",
        "text_to_speech" => "🔊",
        "speech_to_text" => "🎤",
        _ => "🔧",
    }
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

// ==================== 旧函数兼容（commands/modal 模块使用）====================
#[allow(dead_code)]
pub fn print_user_message(content: &str) {
    user_message(content);
}
#[allow(dead_code)]
pub fn print_assistant_message(msg: &tiangong_core::session::Message) {
    println!("{GREEN_BOLD}助手{RESET}");
    let content = msg.text_content();
    let content = content.trim();
    if !content.is_empty() {
        for line in content.lines() {
            println!("  {line}");
        }
    }
    println!();
}
#[allow(dead_code)]
pub fn print_system_message(msg: &tiangong_core::session::Message) {
    let content = msg.text_content();
    let content = content.trim();
    if !content.is_empty() {
        for line in content.lines() {
            println!("{DIM}{line}{RESET}");
        }
    }
}
#[allow(dead_code)]
pub fn print_session_messages(messages: &[tiangong_core::session::Message]) {
    for msg in messages {
        match msg.kind() {
            tiangong_core::session::MessageRole::User => print_user_message(&msg.text_content()),
            tiangong_core::session::MessageRole::Assistant => print_assistant_message(msg),
            tiangong_core::session::MessageRole::System => print_system_message(msg),
            tiangong_core::session::MessageRole::Notice => print_system_message(msg),
            tiangong_core::session::MessageRole::Tool => print_system_message(msg),
        }
    }
}
#[allow(dead_code)]
pub fn print_error(msg: &str) {
    error(msg);
}
#[allow(dead_code)]
pub fn print_info(msg: &str) {
    println!("{msg}");
}
#[allow(dead_code)]
pub fn print_warn(msg: &str) {
    warn(msg);
}
#[allow(dead_code)]
pub fn print_status(msg: &str) {
    status(msg);
}
#[allow(dead_code)]
pub fn print_delta(delta: &str) {
    self::delta(delta);
}
#[allow(dead_code)]
pub fn flush_line() {
    delta_end();
}
