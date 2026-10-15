//! Shell 方言：类型识别、参数引用、登录参数与就绪探针。

use super::output::MARKER_PREFIX;
use crate::persist;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellKind {
    Posix,
    Cmd,
    PowerShell,
}

impl ShellKind {
    pub(super) fn of(program: &str) -> Self {
        let name = std::path::Path::new(program)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or(program);
        if name.eq_ignore_ascii_case("cmd") {
            Self::Cmd
        } else if name.eq_ignore_ascii_case("powershell") || name.eq_ignore_ascii_case("pwsh") {
            Self::PowerShell
        } else {
            Self::Posix
        }
    }

    pub(super) fn quote(self, value: &str) -> String {
        match self {
            Self::PowerShell => format!("'{}'", value.replace('\'', "''")),
            Self::Cmd => {
                if value.is_empty()
                    || value.contains(|ch: char| ch.is_whitespace() || "\"&|<>()^".contains(ch))
                {
                    format!("\"{}\"", value.replace('"', "\"\""))
                } else {
                    value.to_string()
                }
            }
            Self::Posix => posix_quote(value),
        }
    }
}

fn posix_quote(value: &str) -> String {
    if value.is_empty()
        || value.contains(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '\'' | '"'
                        | '\\'
                        | '$'
                        | '`'
                        | '!'
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '|'
                        | '&'
                        | ';'
                        | '<'
                        | '>'
                        | '~'
                )
        })
    {
        format!("'{}'", value.replace('\'', "'\\''"))
    } else {
        value.to_string()
    }
}

pub(super) fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                std::path::PathBuf::from(
                    std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
                )
                .join("System32/WindowsPowerShell/v1.0/powershell.exe")
                .to_string_lossy()
                .into_owned()
            } else {
                "sh".to_string()
            }
        })
}

pub(super) fn login_shell_args(shell: &str) -> Vec<&'static str> {
    match ShellKind::of(shell) {
        // 禁用第三方交互扩展及个人启动脚本，确保代理命令边界可预测。
        ShellKind::PowerShell => vec![
            "-NoLogo",
            "-NoProfile",
            "-NoExit",
            "-Command",
            "Remove-Module PSReadLine -ErrorAction SilentlyContinue",
        ],
        ShellKind::Cmd => vec!["/d"],
        ShellKind::Posix => {
            let basename = std::path::Path::new(shell)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(shell);
            match basename {
                "bash" | "zsh" => vec!["--login"],
                "sh" => vec!["-l"],
                _ => Vec::new(),
            }
        }
    }
}

/// 就绪探针命令：各 Shell 都以「拼接」形式书写，探针文本本身绝不含
/// 完整 marker。PTY 回显开启时写进去的字节会原样回流，若探针里带完整
/// marker，未就绪终端（残留前台进程占用 stdin）的纯回显也会命中匹配，
/// 就绪判定退化成恒真——`cancel()` 的中断确认将失去意义。
pub(super) fn shell_ready_probe(shell: ShellKind, marker: &str) -> String {
    let suffix = marker.strip_prefix(MARKER_PREFIX).unwrap_or(marker);
    match shell {
        ShellKind::Cmd => format!("echo {MARKER_PREFIX}^{suffix}\r"),
        ShellKind::PowerShell => format!("Write-Output ('{MARKER_PREFIX}' + '{suffix}')\r"),
        // 相邻引用串在 POSIX shell 中天然拼接：执行后输出完整 marker，
        // 回显的命令行则被 `'` 断开，不构成完整 marker。
        ShellKind::Posix => format!("echo '{MARKER_PREFIX}''{suffix}'\r"),
    }
}

/// 就绪判定：把原始字节经终端仿真还原成「终端上真实呈现的行」后，再要求
/// 某一整行等于 marker。
///
/// 不能在原始字节流上做子串匹配。原始流里混着 ZLE 重绘序列、光标回退、
/// `\r` 覆写以及 zsh-autosuggestions / zsh-syntax-highlighting 的预测文本
/// 与擦除——这些字节从未作为「终端最终显示的内容」存在过，只是绘制过程的
/// 中间态。在中间态上 `contains` 会把行编辑器重绘出的、**尚未执行**的命令
/// 行当成执行结果：autosuggest 重绘时引号被消化，完整 marker 随之出现，
/// 就绪判定退化成恒真，正常运行的命令被误判为「包装脚本夭折」。
///
/// 还原后改用整行相等匹配，[`shell_ready_probe`] 的相邻引用串拼接防护才真
/// 正生效：回显的 `echo '前缀''后缀'` 整行不可能等于纯 marker 行，唯有 shell
/// 实际执行过 echo 才会产生独占一行的 marker。
pub(super) fn shell_ready_probe_completed(raw: &str, marker: &str) -> bool {
    let mut processor = persist::TerminalLineProcessor::new();
    let mut lines = processor.process(raw);
    let current = processor.current_line();
    if !current.trim().is_empty() {
        lines.push(current);
    }
    lines.iter().any(|line| line.trim() == marker)
}

/// Windows 的本地扩展路径可用于文件 API，但 cmd 不支持将其作为工作目录。
/// 仅去掉本地盘符路径的前缀，不把真实 UNC 网络路径伪装成本地路径。
pub(super) fn shell_compatible_cwd(cwd: &str) -> &str {
    if cfg!(windows)
        && let Some(local) = cwd.strip_prefix(r"\\?\")
        && local.as_bytes().get(1) == Some(&b':')
    {
        return local;
    }
    cwd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn shell_ready_probe_uses_cmd_syntax() {
        assert_eq!(
            shell_ready_probe(ShellKind::Cmd, "__TIANGONG_READY_test__"),
            "echo __TIANGONG_^READY_test__\r"
        );
    }

    /// 探针文本本身绝不能含完整 marker：PTY 回显会把写入的字节原样回流，
    /// 若探针里带完整 marker，未就绪终端（残留前台进程占着 stdin、一个字节
    /// 都没执行）的纯回显也会命中子串匹配，就绪判定退化成恒真。
    #[test]
    fn shell_ready_probe_文本不含完整_marker() {
        let marker = "__TIANGONG_READY_abc__";
        for shell in [ShellKind::Posix, ShellKind::Cmd, ShellKind::PowerShell] {
            let probe = shell_ready_probe(shell, marker);
            assert!(
                !probe.contains(marker),
                "{shell:?} 探针回显会被误判为就绪: {probe}"
            );
        }
        assert_eq!(
            shell_ready_probe(ShellKind::Posix, marker),
            "echo '__TIANGONG_''READY_abc__'\r",
            "POSIX 探针须用相邻引用串拼接，执行后才输出完整 marker"
        );
    }

    #[test]
    fn 就绪判定_未执行的纯回显不算就绪() {
        let marker = "__TIANGONG_READY_abc__";
        // 残留前台进程占用 stdin：探针整行被 tty 回显，但无人执行。
        let echo_only = format!(
            "\u{1b}[?2004h{}\r\n",
            shell_ready_probe(ShellKind::Posix, marker).trim_end_matches('\r')
        );
        assert!(
            !shell_ready_probe_completed(&echo_only, marker),
            "纯回显必须判为未就绪: {echo_only:?}"
        );
    }

    /// 富提示符下**真正执行过**探针：marker 独占一行（前面可能有 starship
    /// 提示符与 OSC 颜色查询等噪声行），必须判为就绪。
    #[test]
    fn 就绪判定_命中富提示符噪声中独占一行的_marker() {
        let marker = "__TIANGONG_READY_abc__";
        let noisy = format!(
            "\u{1b}]11;rgb:1e1e/1e1e/2e2e\u{1b}\\\r\n\
             \u{1b}[1;32m❯\u{1b}[0m echo '__TIANGONG_READY_''abc__'\r\n\
             {marker}\u{1b}[K\r\n"
        );
        assert!(
            shell_ready_probe_completed(&noisy, marker),
            "执行后独占一行的 marker 必须判为就绪"
        );
        assert!(
            !shell_ready_probe_completed(&noisy, "__TIANGONG_READY_other__"),
            "不同随机 marker 不得互相命中"
        );
    }

    /// zsh-autosuggestions / zsh-syntax-highlighting 等 ZLE widget 会在**行编辑
    /// 阶段**重绘输入行，重绘时引号被消化，完整 marker 随之出现在原始字节流
    /// 中——但命令一个字都还没执行。
    ///
    /// 这是 #571 兜底假阴性的真正根因：旧实现在未经终端仿真的原始字节上做
    /// `contains`，把重绘中间态当成执行结果，导致 git fetch 等正常命令被误判
    /// 为「包装脚本夭折」，实际执行成功却回报失败，诱导调用方重复执行。
    #[test]
    fn 就绪判定_zsh行编辑器重绘不算就绪() {
        let marker = "__TIANGONG_READY_abc__";
        // autosuggest 的 xtrace 前缀里带完整 marker：命令尚未执行。
        let xtrace = format!("_zsh_autosuggest_bind_widgets:18> echo {marker}\r\n");
        assert!(
            !shell_ready_probe_completed(&xtrace, marker),
            "xtrace 回显的未执行命令行不得判为就绪: {xtrace:?}"
        );

        // 语法高亮重绘：marker 与提示符同行，且行尾被擦除序列覆盖。
        let redraw = format!("\u{1b}[1;32m❯\u{1b}[0m echo {marker}\u{1b}[K\r");
        assert!(
            !shell_ready_probe_completed(&redraw, marker),
            "行编辑器重绘出的命令行不得判为就绪: {redraw:?}"
        );
    }
}
