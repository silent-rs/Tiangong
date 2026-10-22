//! 用法引导：识别 Agent 命令里不推荐的写法，在工具结果中给出替代做法。
//!
//! 只提示、不拦截——命令照常执行。两类写法会让终端插件的能力失效：
//!
//! - **现写多行脚本**：一次调用串起多步操作，中间某步失败时后续照跑，
//!   错误被淹没在整段输出里，也无法逐步确认结果。PTY 终端可以逐条执行
//!   并各自返回退出码，分步调用更可控；已有的脚本文件则直接执行即可。
//! - **`nohup` / `&` 放到后台**：后台进程脱离终端，看不到输出、拿不到退出
//!   码，用完常被遗忘（issue #672）。终端插件支持多终端并行，长期运行的
//!   服务应放进单独终端前台运行，用完 Ctrl+C 停止。

/// 超过这么多行（不含空行与注释）视为现写脚本。
const SCRIPT_LINE_THRESHOLD: usize = 3;

/// 针对命令文本生成用法提示（可能为空）。
pub(super) fn command_hints(command: &str, is_shell: bool) -> Vec<&'static str> {
    let mut hints = Vec::new();
    if launches_background(command) {
        hints.push(
            "检测到 nohup / & 后台启动：后台进程看不到输出、容易遗留。终端插件支持多终端并行，\
长期运行的服务请用 terminal(action=open) 新开终端，再用 terminal(action=send)（enter=true）在前台运行，\
用完发送 \\x03 停止并 terminal(action=close)；已启动的后台进程可在 terminal(action=status) 概要中查到 pid 并 kill",
        );
    }
    if is_shell && script_lines(command) > SCRIPT_LINE_THRESHOLD {
        hints.push(
            "检测到多行脚本：多步操作请拆成多次 run_command 逐步执行并确认每步结果（某步失败不会\
被后续步骤掩盖）；已有的脚本文件直接用 run_command 执行（如 bash scripts/x.sh）",
        );
    }
    hints
}

/// 有效行数：去掉空行、注释行与续行（`\` 结尾的物理行与下一行算一行）。
fn script_lines(script: &str) -> usize {
    let mut count = 0;
    let mut continued = false;
    for line in script.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !continued {
            count += 1;
        }
        continued = trimmed.ends_with('\\');
    }
    count
}

/// 命令是否把进程放到后台：出现 `nohup` / `setsid` / `disown` 命令词，或
/// 引号之外有单独的 `&`（排除 `&&`、`>&`、`&>`、`|&` 等重定向与逻辑与）。
fn launches_background(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut word = String::new();
    let mut at_command_start = true;
    let is_launcher = |word: &str| matches!(word, "nohup" | "setsid" | "disown");
    for (index, &ch) in chars.iter().enumerate() {
        if escaped {
            escaped = false;
            word.push(ch);
            continue;
        }
        match ch {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ if in_single || in_double => word.push(ch),
            '&' => {
                let prev = index.checked_sub(1).map(|i| chars[i]);
                let next = chars.get(index + 1).copied();
                let redirect_or_and =
                    matches!(prev, Some('&' | '>' | '<' | '|')) || matches!(next, Some('&' | '>'));
                if !redirect_or_and {
                    return true;
                }
                word.clear();
                at_command_start = true;
            }
            c if c.is_whitespace() || matches!(c, ';' | '|' | '(' | ')') => {
                if at_command_start && is_launcher(&word) {
                    return true;
                }
                if !word.is_empty() {
                    // 命令名之后的词是参数；`sudo nohup x` 这类前缀命令仍按命令词检查。
                    at_command_start = matches!(word.as_str(), "sudo" | "env" | "exec" | "time");
                }
                if matches!(c, ';' | '|' | '(' | ')' | '\n') {
                    at_command_start = true;
                }
                word.clear();
            }
            _ => word.push(ch),
        }
    }
    at_command_start && is_launcher(&word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 识别后台启动() {
        for command in [
            "nohup ./server > /tmp/log 2>&1 &",
            "nohup ./server",
            "./server &",
            "(./server &); sleep 15; curl localhost",
            "cd app && sudo nohup ./run",
            "setsid ./daemon",
            "./a & ./b",
        ] {
            assert!(launches_background(command), "应识别: {command}");
        }
    }

    #[test]
    fn 不误判逻辑与重定向和引号内的符号() {
        for command in [
            "cargo build && cargo test",
            "make 2>&1 | tee log",
            "cmd &> out.log",
            "echo 'a & b' \"nohup &\"",
            "grep nohup README.md",
            "echo a\\&b",
            "ls |& cat",
        ] {
            assert!(!launches_background(command), "不应误判: {command}");
        }
    }

    #[test]
    fn 多行脚本才提示拆分() {
        let short = "cd crates\ncargo check";
        assert!(command_hints(short, true).is_empty());
        let long =
            "#!/bin/sh\nset -e\n\ncargo fmt\ncargo clippy \\\n  -- -D warnings\ncargo test\n";
        assert_eq!(script_lines(long), 4);
        assert_eq!(command_hints(long, true).len(), 1);
        assert!(
            command_hints(long, false).is_empty(),
            "run_command 不提示多行"
        );
    }
}
