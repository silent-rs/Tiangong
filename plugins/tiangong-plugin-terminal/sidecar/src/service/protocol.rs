//! 非交互命令协议：包装脚本、边界标记与执行结果解析。

use std::io::Write;

use anyhow::{Context as _, Result, bail};

use super::output::contains_marker;
use super::shell::{ShellKind, shell_compatible_cwd};
use super::types::{ExecRequest, ExecResponse};
use crate::persist;

/// zsh 默认开启 `EQUALS`：以 `=` 开头的裸词被当作 equals-expansion
/// 展开为命令路径，`echo ===` 会在**解析期**报 `== not found`，
/// 并直接终止被 `source` 的整个脚本——包装脚本的 end marker 永不输出，
/// 调用方只能轮询到超时（issue #571）。解析期错误无法靠 `set -e`/`trap`
/// 补救，只能从源头关掉该选项。
///
/// 写法约束：
/// - 必须对 bash/dash/sh 无害，因此 `setopt` 只在 `ZSH_VERSION` 存在时
///   经 `eval` 执行（非 zsh 连 `[[ -o equals ]]` 都不会被解析到）；
/// - 记录原值并在用户命令后恢复，不改变用户 shell 的既有偏好。
const ZSH_EQUALS_DISABLE_SNIPPET: &str = "__tiangong_equals_restore=''\nif [ -n \"${ZSH_VERSION-}\" ]; then eval 'if [[ -o equals ]]; then __tiangong_equals_restore=1; fi; setopt noequals'; fi\n";

/// 与 [`ZSH_EQUALS_DISABLE_SNIPPET`] 配对：仅当原本开启时才恢复，
/// 用户主动 `unsetopt equals` 的偏好不会被打开。
const ZSH_EQUALS_RESTORE_SNIPPET: &str = "if [ -n \"$__tiangong_equals_restore\" ]; then eval 'setopt equals'; fi\nunset __tiangong_equals_restore\n";

pub(super) struct CommandMarkers {
    pub(super) start: String,
    pub(super) end: String,
    pub(super) cwd: String,
    pub(super) exit_code: String,
}

impl CommandMarkers {
    pub(super) fn new(id: &str) -> Self {
        Self {
            start: format!("__TIANGONG_START_{id}__"),
            end: format!("__TIANGONG_END_{id}__"),
            cwd: format!("__TIANGONG_CWD_{id}__"),
            exit_code: format!("__TIANGONG_RC_{id}__"),
        }
    }
}

#[derive(Default)]
pub(super) struct ParsedCommandOutput {
    pub(super) stdout: String,
    pub(super) exit_code: Option<i32>,
    pub(super) cwd_after: String,
    pub(super) completed: bool,
}

impl ParsedCommandOutput {
    pub(super) fn into_response(self, timed_out: bool) -> ExecResponse {
        ExecResponse {
            stdout: self.stdout,
            stderr: String::new(),
            exit_code: self.exit_code.unwrap_or(if timed_out { -1 } else { 0 }),
            timed_out,
            cwd_after: self.cwd_after,
            interactive_mode: false,
            session_ended: false,
        }
    }
}

/// 把 Agent 的请求拼成要执行的命令文本。
///
/// **不含 cwd 切换**：工作目录由包装脚本负责（见 [`prepare_posix_command`]），
/// 不能拼进命令文本。命令文本会原样回显给用户、记入 `running_command`
/// 并出现在失败摘要里，混入 `cd ... &&` 会让用户看到自己绝不会这样敲的
/// 命令；宿主每次调用都传会话工作目录，拼进去等于每条命令都平白多一截
/// 前缀，用户命令与 Agent 命令的形态就不一致了。
pub(super) fn command_from_request(request: &ExecRequest, shell: ShellKind) -> Result<String> {
    if let Some(script) = request.script.as_deref() {
        if script.trim().is_empty() {
            bail!("script 不能为空");
        }
        return Ok(script.to_string());
    }
    if request.cmd.trim().is_empty() {
        bail!("cmd 不能为空");
    }
    // cmd 可含参数（与 command 插件的 run_command 语义一致）：先按引号
    // 感知规则拆分成程序名 + 内联参数，再逐词 quote 拼接，避免
    // "git status" 被整体 quote 成单个命令名导致找不到命令。
    let (program, inline_args) = split_command(request.cmd.trim());
    let mut command = shell.quote(&program);
    if shell == ShellKind::PowerShell {
        command.insert_str(0, "& ");
    }
    for arg in inline_args.iter().chain(request.args.iter()) {
        command.push(' ');
        command.push_str(&shell.quote(arg));
    }
    Ok(command)
}

/// 交互模式的工作目录切换：作为独立一行写入 PTY，不并入命令文本。
pub(super) fn interactive_cwd_command(cwd: Option<&str>, shell: ShellKind) -> Option<String> {
    let cwd = cwd
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(shell_compatible_cwd)?;
    let quoted = shell.quote(cwd);
    Some(match shell {
        ShellKind::Cmd => format!("cd /d {quoted}\r"),
        ShellKind::PowerShell => format!("Set-Location -LiteralPath {quoted}\r"),
        ShellKind::Posix => format!("cd {quoted}\r"),
    })
}

/// 包装脚本里的工作目录切换语句（不进入用户可见的命令文本）。
/// 切换失败必须让命令整体失败，不能在错误的目录里执行。
fn cwd_prelude(cwd: Option<&str>, shell: ShellKind) -> String {
    let Some(cwd) = cwd
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(shell_compatible_cwd)
    else {
        return String::new();
    };
    match shell {
        ShellKind::Cmd => format!(
            "cd /d {}\r\nif errorlevel 1 goto :eof\r\n",
            shell.quote(cwd)
        ),
        ShellKind::PowerShell => format!(
            "Set-Location -LiteralPath {} -ErrorAction Stop\n",
            shell.quote(cwd)
        ),
        ShellKind::Posix => format!("cd {} || return $?\n", shell.quote(cwd)),
    }
}

/// 按引号感知规则拆分命令字符串为（程序名, 参数列表）。
///
/// 与 command 插件 sidecar 的 `split_command` 语义一致：单双引号成组、
/// 反斜杠转义，空白分隔；无引号的裸词按空白切分。
fn split_command(raw: &str) -> (String, Vec<String>) {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single && !cfg!(windows) => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    if parts.is_empty() {
        return (raw.to_string(), Vec::new());
    }
    let cmd = parts.remove(0);
    (cmd, parts)
}

pub(super) struct PreparedCommand {
    pub(super) input: String,
    /// Unix 下保持临时脚本存活到命令结束；drop 后自动删除。
    /// POSIX 路径有两个文件：wrapper 与承载用户命令的内层脚本。
    _files: Vec<tempfile::TempPath>,
}

/// 生成 POSIX 包装：wrapper 负责 marker 边界与环境保存/恢复，
/// **用户命令单独落在内层脚本**，由 wrapper `source` 执行。
///
/// 分成两层是 issue #571 的根治点：内联时用户命令的任何**解析期错误**
///（zsh equals 展开、未闭合引号、非法重定向等）都会直接终止整份脚本，
/// end marker 永不输出，调用方只能轮询到超时；错误发生在解析阶段，
/// `set -e`、`trap`、`||` 兜底一律无效——脚本余下部分根本不会被执行。
/// 拆成内层文件后，解析失败只终止内层 `source`（shell 把它折算成非零
/// 退出码，zsh 126 / bash 1），wrapper 的控制流完好，照常吐出 cwd、
/// 退出码与 end marker，调用方立刻拿到结果和 shell 的原始报错。
///
/// 同时关闭 zsh `EQUALS` 选项，让 `echo ===`、`awk =x` 这类最常见的形态
/// 直接按字面量正常执行，而不是「稳定报错但能收尾」。
fn prepare_posix_command(
    command: &str,
    cwd: Option<&str>,
    markers: &CommandMarkers,
) -> Result<PreparedCommand> {
    let mut inner = tempfile::Builder::new()
        .prefix(&markers.start)
        .suffix(".inner.sh")
        .tempfile()
        .context("创建终端命令临时文件失败")?;
    inner
        .write_all(format!("{command}\n").as_bytes())
        .context("写入终端命令临时文件失败")?;
    inner.flush().context("刷新终端命令临时文件失败")?;
    let inner_path = ShellKind::Posix.quote(&inner.path().to_string_lossy());

    let script = posix_wrapper_script(&inner_path, cwd, markers);
    let mut file = tempfile::Builder::new()
        .prefix(&markers.start)
        .suffix(".sh")
        .tempfile()
        .context("创建终端命令临时文件失败")?;
    file.write_all(script.as_bytes())
        .context("写入终端命令临时文件失败")?;
    file.flush().context("刷新终端命令临时文件失败")?;
    let path = file.path().to_string_lossy();
    Ok(PreparedCommand {
        // marker 放在输入行最前面，reader 从首个 chunk 起就会暂存并过滤
        // shell/ZLE 回显，不会因路径换行或语法高亮把内部 source 命令漏到 UI。
        input: format!("__TIANGONG_= . {}\r", ShellKind::Posix.quote(&path)),
        _files: vec![file.into_temp_path(), inner.into_temp_path()],
    })
}

/// wrapper 脚本正文：`inner_path` 是已 quote 的内层脚本路径。
/// 独立成函数便于测试直接对脚本文本断言。
fn posix_wrapper_script(inner_path: &str, cwd: Option<&str>, markers: &CommandMarkers) -> String {
    format!(
        "echo '{}'\n{}{}__tiangong_had_PAGER=${{PAGER+x}}\n__tiangong_old_PAGER=${{PAGER-}}\n__tiangong_had_GIT_PAGER=${{GIT_PAGER+x}}\n__tiangong_old_GIT_PAGER=${{GIT_PAGER-}}\n__tiangong_had_GH_PAGER=${{GH_PAGER+x}}\n__tiangong_old_GH_PAGER=${{GH_PAGER-}}\n__tiangong_had_LESS=${{LESS+x}}\n__tiangong_old_LESS=${{LESS-}}\nexport PAGER=cat GIT_PAGER=cat GH_PAGER=cat LESS=FRX\n. {}\n__tiangong_rc=$?\n{}if [ -n \"$__tiangong_had_PAGER\" ]; then PAGER=\"$__tiangong_old_PAGER\"; export PAGER; else unset PAGER; fi\nif [ -n \"$__tiangong_had_GIT_PAGER\" ]; then GIT_PAGER=\"$__tiangong_old_GIT_PAGER\"; export GIT_PAGER; else unset GIT_PAGER; fi\nif [ -n \"$__tiangong_had_GH_PAGER\" ]; then GH_PAGER=\"$__tiangong_old_GH_PAGER\"; export GH_PAGER; else unset GH_PAGER; fi\nif [ -n \"$__tiangong_had_LESS\" ]; then LESS=\"$__tiangong_old_LESS\"; export LESS; else unset LESS; fi\nprintf '\\n{}'; pwd\necho '{}'$__tiangong_rc\necho '{}'\n",
        markers.start,
        cwd_prelude(cwd, ShellKind::Posix),
        ZSH_EQUALS_DISABLE_SNIPPET,
        inner_path,
        ZSH_EQUALS_RESTORE_SNIPPET,
        markers.cwd,
        markers.exit_code,
        markers.end,
    )
}

fn prepare_cmd_command(
    command: &str,
    cwd: Option<&str>,
    markers: &CommandMarkers,
) -> Result<PreparedCommand> {
    let script = format!(
        "@echo off\r\necho {}\r\n{}{}\r\nset \"__TIANGONG_RC_VALUE=%errorlevel%\"\r\necho {}\r\necho %cd%\r\necho {}%__TIANGONG_RC_VALUE%\r\necho {}\r\n",
        markers.start,
        cwd_prelude(cwd, ShellKind::Cmd),
        command,
        markers.cwd,
        markers.exit_code,
        markers.end,
    );
    let mut file = tempfile::Builder::new()
        .prefix(&markers.start)
        .suffix(".cmd")
        .tempfile()
        .context("创建 Windows 终端命令临时文件失败")?;
    file.write_all(script.as_bytes())
        .context("写入 Windows 终端命令临时文件失败")?;
    file.flush().context("刷新 Windows 终端命令临时文件失败")?;
    let path = file.path().to_string_lossy();
    Ok(PreparedCommand {
        input: format!(
            "echo {}>nul & call {}\r",
            markers.start,
            ShellKind::Cmd.quote(&path)
        ),
        _files: vec![file.into_temp_path()],
    })
}

pub(super) fn prepare_non_interactive_command(
    command: &str,
    cwd: Option<&str>,
    markers: &CommandMarkers,
    shell: ShellKind,
) -> Result<PreparedCommand> {
    match shell {
        ShellKind::Posix => prepare_posix_command(command, cwd, markers),
        ShellKind::Cmd => prepare_cmd_command(command, cwd, markers),
        ShellKind::PowerShell => {
            // Out-Default 强制格式化在结束标记前完成，否则表格会延迟到提示符才输出。
            let result_variable = format!("{}VALUE", markers.exit_code);
            let script = format!(
                "try {{\nWrite-Output '{start}'\n{prelude}${state} = [pscustomobject]@{{ Result = 0; Before = $LASTEXITCODE; Errors = $Error.Count }}\ntry {{ . {{\n{command}\n${state}.Result = if ($?) {{ 0 }} elseif ($LASTEXITCODE -ne ${state}.Before) {{ $LASTEXITCODE }} elseif ($Error.Count -gt ${state}.Errors) {{ 1 }} else {{ ${state}.Before }}\n}} | Out-Default }} catch {{ ${state}.Result = 1; $_ | Out-Default }}\nWrite-Output ''\nWrite-Output '{cwd}'\nWrite-Output (Get-Location).Path\nWrite-Output ('{rc}' + ${state}.Result)\nWrite-Output '{end}'\n}} finally {{ Remove-Variable -Name '{state}' -ErrorAction SilentlyContinue }}\n",
                start = markers.start,
                prelude = cwd_prelude(cwd, ShellKind::PowerShell),
                cwd = markers.cwd,
                rc = markers.exit_code,
                end = markers.end,
                state = result_variable,
            );
            let mut file = tempfile::Builder::new()
                .prefix(&markers.start)
                .suffix(".ps1")
                .tempfile()
                .context("创建 PowerShell 命令临时文件失败")?;
            file.write_all(script.as_bytes())?;
            file.flush()?;
            // 用 UTF-8 读取已生成内容，不修改用户的执行策略。
            let path = ShellKind::PowerShell.quote(&file.path().to_string_lossy());
            Ok(PreparedCommand {
                input: format!(
                    "$null='__TIANGONG_'; try {{ . ([scriptblock]::Create([IO.File]::ReadAllText({path}))) }} catch {{ Write-Output '{start}'; $_ | Out-Default; Write-Output '{rc}1'; Write-Output '{end}' }}\r",
                    start = markers.start,
                    rc = markers.exit_code,
                    end = markers.end
                ),
                _files: vec![file.into_temp_path()],
            })
        }
    }
}

pub(super) fn parse_command_output(raw: &[u8], markers: &CommandMarkers) -> ParsedCommandOutput {
    let mut processor = persist::TerminalLineProcessor::new();
    let mut lines = processor.process(&String::from_utf8_lossy(raw));
    let current = processor.current_line();
    if !current.trim().is_empty() {
        lines.push(current);
    }

    let mut start_seen = false;
    let mut collecting_cwd = false;
    let mut output = Vec::new();
    let mut parsed = ParsedCommandOutput::default();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == markers.start {
            start_seen = true;
            continue;
        }
        if trimmed == markers.cwd {
            parsed.cwd_after.clear();
            collecting_cwd = true;
            continue;
        }
        if let Some(value) = trimmed.strip_prefix(&markers.cwd) {
            parsed.cwd_after = value.trim().to_string();
            collecting_cwd = true;
            continue;
        }
        if let Some(value) = trimmed.strip_prefix(&markers.exit_code) {
            collecting_cwd = false;
            parsed.exit_code = value.trim().parse().ok();
            continue;
        }
        if trimmed == markers.end {
            parsed.completed = start_seen;
            break;
        }
        if collecting_cwd {
            parsed.cwd_after.push_str(trimmed);
            continue;
        }
        if start_seen && !contains_marker(trimmed) {
            output.push(line.trim_end().to_string());
        }
    }
    parsed.stdout = output.join("\n");
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::shell::shell_ready_probe;

    #[test]
    fn split_command_拆分含参数的cmd() {
        let (program, args) = split_command("git status --short");
        assert_eq!(program, "git");
        assert_eq!(args, vec!["status".to_string(), "--short".to_string()]);
    }

    #[test]
    fn split_command_引号成组() {
        let (program, args) = split_command("echo 'a b' \"c d\"");
        assert_eq!(program, "echo");
        assert_eq!(args, vec!["a b".to_string(), "c d".to_string()]);
    }

    #[test]
    fn split_command_裸命令无参数() {
        let (program, args) = split_command("ls");
        assert_eq!(program, "ls");
        assert!(args.is_empty());
    }

    /// cwd 绝不能拼进命令文本：命令文本会原样回显给用户、记入
    /// running_command 并出现在失败摘要里，混入 `cd ... &&` 会让用户看到
    /// 自己绝不会这样敲的命令（宿主每次调用都带会话工作目录，等于每条
    /// 命令都平白多一截前缀）。目录切换由包装脚本承担。
    #[test]
    fn cwd不进入命令文本而由包装脚本承担() {
        let request = ExecRequest {
            session_id: "s".to_string(),
            cmd: "cargo".to_string(),
            args: vec!["clippy".to_string()],
            script: None,
            timeout: None,
            interactive: false,
            cwd: Some("/work/repo".to_string()),
        };
        let command = command_from_request(&request, ShellKind::Posix).expect("拼装命令失败");
        assert_eq!(command, "cargo clippy", "命令文本不得含 cd 前缀");

        let markers = CommandMarkers::new("x");
        let quoted = ShellKind::Posix.quote("/work/repo");
        let wrapper = posix_wrapper_script("'/tmp/inner.sh'", Some("/work/repo"), &markers);
        assert!(
            wrapper.contains(&format!("cd {quoted} || return $?")),
            "包装脚本须负责切换目录且失败即终止: {wrapper}"
        );
        let cd_at = wrapper.find(&format!("cd {quoted}")).expect("缺少 cd");
        let source_at = wrapper.find(". '/tmp/inner.sh'").expect("缺少 source");
        assert!(cd_at < source_at, "必须先切目录再执行命令");

        // 交互模式同样不污染命令文本，改为独立一行写入。
        assert_eq!(
            interactive_cwd_command(Some("/work/repo"), ShellKind::Posix),
            Some(format!("cd {quoted}\r"))
        );
        assert_eq!(interactive_cwd_command(None, ShellKind::Posix), None);
    }

    #[test]
    fn command_parser_returns_stdout_exit_code_and_cwd() {
        let markers = CommandMarkers::new("x");
        let parsed = parse_command_output(
            b"prompt wrapper\r\n__TIANGONG_START_x__\r\none\r\ntwo\r\n__TIANGONG_CWD_x__/tmp\r\n__TIANGONG_RC_x__7\r\n__TIANGONG_END_x__\r\n",
            &markers,
        );
        assert!(parsed.completed);
        assert_eq!(parsed.stdout, "one\ntwo");
        assert_eq!(parsed.exit_code, Some(7));
        assert_eq!(parsed.cwd_after, "/tmp");
    }

    #[test]
    fn command_parser_joins_wrapped_cwd_without_polluting_stdout() {
        let markers = CommandMarkers::new("x");
        let parsed = parse_command_output(
            b"__TIANGONG_START_x__\r\nagain\r\n__TIANGONG_CWD_x__\r\nC:\\Users\\Test\\very-long\r\n                                                                               path\r\n__TIANGONG_RC_x__0\r\n__TIANGONG_END_x__\r\n",
            &markers,
        );
        assert!(parsed.completed);
        assert_eq!(parsed.stdout, "again");
        assert_eq!(parsed.cwd_after, r"C:\Users\Test\very-longpath");
    }

    /// issue #571：用户命令必须落在独立的内层脚本里，由 wrapper `source`。
    /// 内联时用户命令的解析期错误会连带终止 wrapper，end marker 永不输出。
    #[test]
    fn posix_包装器把用户命令隔离在内层脚本() {
        let markers = CommandMarkers::new("x");
        let prepared = prepare_posix_command("echo A; echo ===; echo B", None, &markers)
            .expect("生成包装失败");
        assert_eq!(prepared._files.len(), 2, "应生成 wrapper 与内层两个脚本");
        let wrapper = std::fs::read_to_string(&prepared._files[0]).expect("读取 wrapper 失败");
        let inner = std::fs::read_to_string(&prepared._files[1]).expect("读取内层脚本失败");
        assert_eq!(inner, "echo A; echo ===; echo B\n");
        assert!(
            !wrapper.contains("echo A; echo ===; echo B"),
            "用户命令不得内联进 wrapper: {wrapper}"
        );
        assert!(
            wrapper.contains(&markers.end),
            "wrapper 必须自带 end marker: {wrapper}"
        );
    }

    /// zsh `EQUALS` 必须在用户命令前关闭、命令后按原值恢复；
    /// 且片段对 bash/dash 无害（`setopt` 只在 ZSH_VERSION 下经 eval 执行）。
    #[test]
    fn posix_包装器关闭并恢复_zsh_equals() {
        let markers = CommandMarkers::new("x");
        let wrapper = posix_wrapper_script("'/tmp/inner.sh'", None, &markers);
        let disable = wrapper.find("setopt noequals").expect("缺少关闭 equals");
        let source = wrapper.find(". '/tmp/inner.sh'").expect("缺少内层 source");
        let restore = wrapper
            .find("__tiangong_equals_restore\" ]; then eval 'setopt equals'")
            .expect("缺少恢复 equals");
        assert!(disable < source, "必须在执行用户命令前关闭 equals");
        assert!(source < restore, "必须在用户命令之后再恢复 equals");
        assert!(
            wrapper.contains("if [ -n \"${ZSH_VERSION-}\" ]; then eval"),
            "setopt 必须由 ZSH_VERSION 守卫并经 eval 执行，避免 bash/dash 解析失败"
        );
    }

    /// 夭折探针带公共 marker 前缀，其回显与执行结果都不得混进命令 stdout。
    #[test]
    fn 夭折探针不污染命令输出() {
        let markers = CommandMarkers::new("x");
        let probe_marker = "__TIANGONG_ABORTCHK_abc__";
        let probe_echo = shell_ready_probe(ShellKind::Posix, probe_marker);
        let raw = format!(
            "__TIANGONG_START_x__\r\nreal-output\r\n{}\r\n{probe_marker}\r\n__TIANGONG_CWD_x__/tmp\r\n__TIANGONG_RC_x__0\r\n__TIANGONG_END_x__\r\n",
            probe_echo.trim_end_matches('\r')
        );
        let parsed = parse_command_output(raw.as_bytes(), &markers);
        assert!(parsed.completed);
        assert_eq!(parsed.stdout, "real-output", "探针行不得进入命令输出");
    }
}
