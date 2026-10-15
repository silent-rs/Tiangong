//! `terminal_send` 交互输入：转义解码、写入与画面等待。

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use super::output::visible_text_from_raw;
use super::types::{SendRequest, SendResponse};
use super::{COMMAND_POLL_INTERVAL_MS, SessionPhase, TerminalService};

/// terminal_send 自动回车：正文写入后、单独补发回车前的间隔。SSH 等远端
/// 行编辑器在正文与回车同包到达时偶发吞掉回车，分开写入可稳定触发执行。
const SEND_ENTER_DELAY_MS: u64 = 80;

fn decode_terminal_escapes(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let Some(escaped) = chars.next() else {
            output.push('\\');
            break;
        };
        match escaped {
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            '0' => output.push('\0'),
            'e' | 'E' => output.push('\x1b'),
            '\\' => output.push('\\'),
            'x' => {
                let digits = chars.clone().take(2).collect::<String>();
                if digits.len() == 2
                    && let Ok(value) = u8::from_str_radix(&digits, 16)
                {
                    chars.next();
                    chars.next();
                    output.push(char::from(value));
                } else {
                    output.push_str("\\x");
                }
            }
            'u' => {
                let digits = chars.clone().take(4).collect::<String>();
                if digits.len() == 4
                    && let Ok(value) = u32::from_str_radix(&digits, 16)
                    && let Some(value) = char::from_u32(value)
                {
                    for _ in 0..4 {
                        chars.next();
                    }
                    output.push(value);
                } else {
                    output.push_str("\\u");
                }
            }
            other => {
                output.push('\\');
                output.push(other);
            }
        }
    }
    output
}

/// 去掉末尾一个行结束符（`\r\n`、`\n` 或 `\r`），避免与自动回车叠加成两次执行。
fn strip_trailing_line_ending(input: &str) -> &str {
    input
        .strip_suffix("\r\n")
        .or_else(|| input.strip_suffix('\n'))
        .or_else(|| input.strip_suffix('\r'))
        .unwrap_or(input)
}

impl TerminalService {
    pub(super) async fn send_to_session(&self, request: SendRequest) -> Result<SendResponse> {
        let decoded = decode_terminal_escapes(&request.input);
        let data = if request.enter {
            strip_trailing_line_ending(&decoded)
        } else {
            decoded.as_str()
        };
        let (session_id, start_offset, baseline_updates) = {
            let mut sessions = self.sessions.lock().expect("会话表锁损坏");
            let selected = sessions
                .get(&request.session_id)
                .filter(|session| session.scope_id.as_deref() == Some(request.scope_id.as_str()))
                .map(|_| request.session_id.clone())
                .ok_or_else(|| anyhow::anyhow!("目标终端不存在或不属于当前会话"))?;
            let session = sessions.get_mut(&selected).expect("刚选择的终端不存在");
            let start_offset = session.raw_bytes_total;
            let baseline_updates = session.screen_updates;
            session
                .writer
                .write_all(data.as_bytes())
                .context("写入交互输入失败")?;
            session.writer.flush().context("刷新交互输入失败")?;
            (selected, start_offset, baseline_updates)
        };
        if request.enter {
            if !data.is_empty() {
                tokio::time::sleep(Duration::from_millis(SEND_ENTER_DELAY_MS)).await;
            }
            self.with_session(&session_id, |session| {
                if session.scope_id.as_deref() != Some(request.scope_id.as_str()) {
                    anyhow::bail!("目标终端不存在或不属于当前会话");
                }
                session.writer.write_all(b"\r").context("写入回车失败")?;
                session.writer.flush().context("刷新回车失败")
            })?;
        }

        let deadline = Instant::now() + Duration::from_secs(request.wait.unwrap_or(3).max(1));
        let stable_window = Duration::from_millis(300);
        let mut last_raw_total = start_offset;
        let mut last_screen_updates = baseline_updates;
        let mut last_change_at = None;
        let mut saw_screen_update = false;
        loop {
            let (raw_total, screen_updates) = self.with_session(&session_id, |session| {
                Ok((session.raw_bytes_total, session.screen_updates))
            })?;
            if raw_total != last_raw_total {
                last_raw_total = raw_total;
                last_change_at = Some(Instant::now());
            }
            if screen_updates != last_screen_updates {
                last_screen_updates = screen_updates;
                last_change_at = Some(Instant::now());
                saw_screen_update = true;
            }
            let required_stability = if saw_screen_update {
                stable_window
            } else {
                stable_window * 2
            };
            if last_change_at.is_some_and(|changed| changed.elapsed() >= required_stability)
                || Instant::now() >= deadline
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }

        let raw = self.raw_output_since(&session_id, start_offset)?;
        let (screen, screen_updates, interactive_mode) =
            self.with_session(&session_id, |session| {
                Ok((
                    session.screen_snapshot.clone(),
                    session.screen_updates,
                    session.phase == SessionPhase::Interactive,
                ))
            })?;
        let raw_output = visible_text_from_raw(&raw);
        Ok(SendResponse {
            session_id,
            stdout: if screen_updates > baseline_updates && !screen.trim().is_empty() {
                screen
            } else if !raw_output.trim().is_empty() {
                raw_output
            } else {
                screen
            },
            exit_code: 0,
            interactive_mode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 自动回车只去掉末尾一个换行() {
        assert_eq!(strip_trailing_line_ending("ls -la"), "ls -la");
        assert_eq!(strip_trailing_line_ending("ls\n"), "ls");
        assert_eq!(strip_trailing_line_ending("ls\r"), "ls");
        assert_eq!(strip_trailing_line_ending("ls\r\n"), "ls");
        assert_eq!(strip_trailing_line_ending("a\n\n"), "a\n");
        assert_eq!(strip_trailing_line_ending(""), "");
    }
}
