//! PTY 输出：原始/可见流记录、事件推送与内部标记过滤。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tiangong_plugin_sidecar::server::emit_notification;

use super::{CHANNEL_OUTPUT, PtySession};
use crate::persist;

/// PTY 输出节流：读取线程按此间隔批量推送（约 60fps 上限）。
pub(super) const OUTPUT_FLUSH_INTERVAL_MS: u64 = 16;

/// 会话最近输出环形缓冲上限（字节）：UI 重新附着时重放历史（含控制序列原样字节）。
const HISTORY_LIMIT_BYTES: usize = 128 * 1024;

/// 内部命令边界标记公共前缀；这些行不得显示给用户。
pub(super) const MARKER_PREFIX: &str = "__TIANGONG_";

#[derive(Debug, Clone, Serialize)]
struct OutputNotification<'a> {
    pub(super) session_id: &'a str,
    pub(super) data: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ExitNotification<'a> {
    pub(super) session_id: &'a str,
    pub(super) exit_code: Option<u32>,
}

/// 记录原始 PTY 输出，供命令边界检测和 stdout 收集。
pub(super) fn record_raw_output(
    sessions: &Arc<Mutex<HashMap<String, PtySession>>>,
    session_id: &str,
    bytes: &[u8],
) {
    let mut sessions = sessions.lock().expect("会话表锁损坏");
    let Some(session) = sessions.get_mut(session_id) else {
        return;
    };
    session.raw_history.extend_from_slice(bytes);
    session.raw_bytes_total = session.raw_bytes_total.saturating_add(bytes.len() as u64);
    let overflow = session
        .raw_history
        .len()
        .saturating_sub(HISTORY_LIMIT_BYTES);
    if overflow > 0 {
        session.raw_history.drain(..overflow);
        session.raw_history_start = session.raw_history_start.saturating_add(overflow as u64);
    }
}

/// 记录过滤后的用户可见输出。日志写在会话表锁外，避免阻塞输入与 resize。
pub(super) fn record_display_output(
    sessions: &Arc<Mutex<HashMap<String, PtySession>>>,
    session_id: &str,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    let logger = {
        let mut sessions = sessions.lock().expect("会话表锁损坏");
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };
        session.display_history.extend_from_slice(bytes);
        let overflow = session
            .display_history
            .len()
            .saturating_sub(HISTORY_LIMIT_BYTES);
        if overflow > 0 {
            session.display_history.drain(..overflow);
        }
        session.logger.clone()
    };
    if let Some(logger) = logger {
        logger.append(&String::from_utf8_lossy(bytes));
    }
}

pub(super) fn publish_display_output(
    sessions: &Arc<Mutex<HashMap<String, PtySession>>>,
    session_id: &str,
    data: &str,
) {
    record_display_output(sessions, session_id, data.as_bytes());
    let payload =
        serde_json::to_string(&OutputNotification { session_id, data }).unwrap_or_default();
    emit_notification(CHANNEL_OUTPUT, payload);
}

pub(super) fn contains_marker(text: &str) -> bool {
    text.contains(MARKER_PREFIX)
}

/// 行级 marker 过滤器：内部 wrapper 的 shell 回显和边界行不进入 UI。
///
/// 同时承担命令回显：start marker 是命令真正开始执行的位置，过滤器把
/// 它**替换**成 Agent 的命令文本，而不是丢弃。命令行因此与自己的输出
/// 严格相邻，中间不会再夹进 shell 为就绪探针重绘的提示符——与用户手敲
/// 命令时看到的形态一致。
pub(super) struct RawOutputFilter {
    pending: String,
    suppress_until_newline: bool,
    /// start marker → 该命令的可见文本。命令写入 PTY 前登记，
    /// 命中后消费一次。
    pub(super) pending_echo: Arc<Mutex<HashMap<String, String>>>,
}

impl RawOutputFilter {
    pub(super) fn new(pending_echo: Arc<Mutex<HashMap<String, String>>>) -> Self {
        Self {
            pending: String::new(),
            suppress_until_newline: false,
            pending_echo,
        }
    }

    /// marker 行命中登记时取出命令文本，作为可见行替换它。
    fn echo_for(&self, segment: &str) -> Option<String> {
        let trimmed = segment.trim();
        let mut pending = self.pending_echo.lock().expect("命令回显表锁损坏");
        if pending.is_empty() {
            return None;
        }
        let marker = pending
            .keys()
            .find(|marker| trimmed.contains(marker.as_str()))
            .cloned()?;
        pending.remove(&marker)
    }

    pub(super) fn filter(&mut self, chunk: &str) -> String {
        // 终端应答序列先行剥离：它们与 marker 无关，且可能出现在任意位置，
        // 在进入行边界切分前去掉，避免混进 UI 输出。
        self.pending.push_str(&strip_terminal_reports(chunk));
        let mut result = String::new();
        while let Some((end, newline)) = next_output_boundary(&self.pending) {
            let segment = self.pending[..end].to_string();
            self.pending.drain(..end);
            if contains_marker(&segment) {
                // start marker 就地换成命令文本：命令行与其输出相邻。
                if let Some(echo) = self.echo_for(&segment) {
                    result.push_str(&echo);
                    result.push_str("\r\n");
                }
                self.suppress_until_newline = !newline;
            } else if !self.suppress_until_newline {
                result.push_str(&segment);
            }
            if newline {
                self.suppress_until_newline = false;
            }
        }
        if self.pending.is_empty() {
            return result;
        }
        if self.suppress_until_newline {
            return result;
        }
        if contains_marker(&self.pending) {
            if self.pending.len() > 8192 {
                result.push_str(&self.pending);
                self.pending.clear();
            }
            return result;
        }
        let split = marker_safe_split(&self.pending);
        if split > 0 {
            result.push_str(&self.pending[..split]);
            self.pending.drain(..split);
        }
        result
    }
}

/// 剥离终端**应答**序列：这类字节是终端回答应用查询的结果，方向是
/// 终端 → 应用，本就不该出现在给用户看的输出里。
///
/// 触发场景：zsh 的 ZLE 行编辑器被唤醒重绘时（夭折探针写入、窗口尺寸
/// 变化等），zsh-syntax-highlighting 与主题会发出 OSC 11（查询背景色）
/// 和 DSR 6n（查询光标位置），终端的应答随即混入 PTY 输出流。过滤器
/// 若不处理，用户就会在长命令输出中途看到 `]11;rgb:1e1e/1e1e/2e2e` 与
/// `[24;1R` 这类乱码。
///
/// 只剥离两类明确的应答，不动任何正常渲染序列（SGR 颜色、光标移动等）：
/// - OSC 颜色应答：`ESC ] 1[01] ; rgb:... (ESC \ | BEL)`
/// - DSR 光标位置上报：`ESC [ <行> ; <列> R`
fn strip_terminal_reports(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\x1b' && index + 1 < bytes.len() {
            // DSR 光标位置上报：ESC [ digits ; digits R
            if bytes[index + 1] == b'[' {
                let mut end = index + 2;
                while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                    end += 1;
                }
                if end < bytes.len() && bytes[end] == b'R' && end > index + 2 {
                    index = end + 1;
                    continue;
                }
            }
            // OSC 颜色应答：ESC ] 10/11 ; ... 以 ESC \ 或 BEL 结束
            if bytes[index + 1] == b']' {
                let mut end = index + 2;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                let code = &value[index + 2..end];
                if matches!(code, "10" | "11" | "12") && end < bytes.len() && bytes[end] == b';' {
                    let mut scan = end;
                    while scan < bytes.len() {
                        if bytes[scan] == 0x07 {
                            scan += 1;
                            break;
                        }
                        if bytes[scan] == b'\x1b'
                            && scan + 1 < bytes.len()
                            && bytes[scan + 1] == b'\\'
                        {
                            scan += 2;
                            break;
                        }
                        scan += 1;
                    }
                    // 未见终止符说明序列尚未收全，留待下个分片再处理。
                    if scan < bytes.len() || value[end..].contains('\u{7}') {
                        index = scan;
                        continue;
                    }
                }
            }
        }
        let ch_len = value[index..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&value[index..index + ch_len]);
        index += ch_len;
    }
    out
}

fn next_output_boundary(value: &str) -> Option<(usize, bool)> {
    let newline = value.find('\n').map(|index| (index + 1, true));
    let bytes = value.as_bytes();
    let mut cursor = None;
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != b'\x1b' || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let mut end = index + 2;
        while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
            end += 1;
        }
        if end < bytes.len() && matches!(bytes[end], b'H' | b'f') {
            cursor = Some((end + 1, false));
            break;
        }
        index += 1;
    }
    match (newline, cursor) {
        (Some(newline), Some(cursor)) => Some(if newline.0 <= cursor.0 {
            newline
        } else {
            cursor
        }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

fn marker_safe_split(value: &str) -> usize {
    for prefix_len in (1..MARKER_PREFIX.len()).rev() {
        if value.len() < prefix_len {
            continue;
        }
        let split = value.len() - prefix_len;
        if value.is_char_boundary(split) && MARKER_PREFIX.starts_with(&value[split..]) {
            return split;
        }
    }
    value.len()
}

pub(super) fn visible_text_from_raw(raw: &[u8]) -> String {
    let mut processor = persist::TerminalLineProcessor::new();
    let mut lines = processor.process(&String::from_utf8_lossy(raw));
    let current = processor.current_line();
    if !current.trim().is_empty() {
        lines.push(current);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_filter_hides_wrapper_and_keeps_command_output() {
        let mut filter = RawOutputFilter::new(Arc::new(Mutex::new(HashMap::new())));
        let visible = filter.filter(
            "$ __TIANGONG_= echo '__TIANGONG_START_x__'; ls\r\n\
             __TIANGONG_START_x__\r\nCargo.toml\r\n\
             __TIANGONG_RC_x__0\r\n",
        );
        assert_eq!(visible, "Cargo.toml\r\n");
    }

    /// 终端应答序列（方向是终端 → 应用）绝不能进入给用户看的输出。
    ///
    /// zsh 的 ZLE 被唤醒重绘时（夭折探针写入等），zsh-syntax-highlighting
    /// 与主题会发 OSC 11 查背景色、DSR 6n 查光标位置，终端应答随即混入 PTY
    /// 输出流。过滤器不处理的话，用户会在长命令输出中途看到
    /// `]11;rgb:1e1e/1e1e/2e2e` 与 `[24;1R` 这类乱码。
    #[test]
    fn marker_filter_剥离终端应答序列() {
        let mut filter = RawOutputFilter::new(Arc::new(Mutex::new(HashMap::new())));
        let visible = filter
            .filter("\u{1b}]11;rgb:1e1e/1e1e/2e2e\u{1b}\\building...\r\n\u{1b}[24;1Rdone\r\n");
        assert_eq!(
            visible, "building...\r\ndone\r\n",
            "OSC 颜色应答与 DSR 光标上报必须被剥离: {visible:?}"
        );
    }

    /// 剥离只针对应答序列，正常渲染序列（SGR 颜色、光标移动、擦除）必须
    /// 原样保留——它们是 UI 正确着色与重绘的依据。
    #[test]
    fn marker_filter_不误伤正常渲染序列() {
        let kept = strip_terminal_reports(
            "\u{1b}[1;32mgreen\u{1b}[0m\u{1b}[2J\u{1b}[H\u{1b}[K\u{1b}[10Dmoved",
        );
        assert_eq!(
            kept, "\u{1b}[1;32mgreen\u{1b}[0m\u{1b}[2J\u{1b}[H\u{1b}[K\u{1b}[10Dmoved",
            "正常渲染序列不得被剥离: {kept:?}"
        );
        // OSC 0/2（设置标题）是应用 → 终端方向，不属于应答，保留。
        let title = strip_terminal_reports("\u{1b}]0;my title\u{7}text");
        assert_eq!(
            title, "\u{1b}]0;my title\u{7}text",
            "设置标题序列不得被剥离"
        );
    }

    /// 命令行必须由 start marker 就地替换产生，紧贴自己的输出。
    /// 早前实现是在写入 PTY 前抢先推送命令文本，shell 为就绪探针重绘的
    /// 提示符会插在命令和输出之间，与用户手敲命令的形态不一致。
    #[test]
    fn marker_filter_把start_marker换成命令行并紧贴输出() {
        let pending = Arc::new(Mutex::new(HashMap::from([(
            "__TIANGONG_START_x__".to_string(),
            "cargo clippy --workspace".to_string(),
        )])));
        let mut filter = RawOutputFilter::new(Arc::clone(&pending));
        // shell 先回显 wrapper、重绘提示符，随后才是 start marker 与输出。
        let visible = filter.filter(
            "$ __TIANGONG_= . /tmp/x.sh\r\n\
             __TIANGONG_START_x__\r\n\
             Finished `dev` profile\r\n\
             __TIANGONG_RC_x__0\r\n",
        );
        assert_eq!(
            visible, "cargo clippy --workspace\r\nFinished `dev` profile\r\n",
            "命令行应紧邻其输出，中间不得夹入提示符或 wrapper 回显"
        );
        assert!(
            pending.lock().expect("锁损坏").is_empty(),
            "命令回显只消费一次"
        );
    }

    #[test]
    fn marker_filter_handles_windows_cursor_moves_and_wrapping() {
        let mut filter = RawOutputFilter::new(Arc::new(Mutex::new(HashMap::new())));
        let visible = filter.filter(
            "__TIANGONG_START_x__\x1b[10;1HC:\\work>echo __TIANGONG_STA\x1b[11;80HRT_x__>nul & dir /b\r\nresult.txt\x1b[13;1HC:\\work>set __TIANGONG_RC_VALUE=0\x1b[14;1H",
        );
        assert_eq!(visible, "result.txt\x1b[13;1H");
    }
}
