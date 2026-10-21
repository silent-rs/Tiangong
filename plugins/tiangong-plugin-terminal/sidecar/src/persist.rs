//! 终端输出持久化：按会话（scope）追加落盘，供诊断与关闭时清理。
//!
//! 移植自内置终端（tiangong-plugin-terminal 的 output_processor.rs）：
//! - 落盘 xterm 看到的原始输出（含控制序列）；
//! - 新 PTY 不读取会话级旧日志，避免把其他终端内容串入新标签；
//! - 日志 1 MiB 上限，超限滚动保留尾部一半。
//!
//! 日志位于插件数据目录（`TIANGONG_PLUGIN_DATA_DIR`，宿主启动 sidecar 时
//! 总是注入）：`terminal-logs/{scope}.log`。打开失败优雅降级为不持久化。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 日志文件大小上限（1 MiB），超过则保留尾部一半后重写，防无限增长
const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub struct OutputLogger {
    file: Arc<Mutex<File>>,
}

impl OutputLogger {
    /// 打开（或创建）日志文件。失败返回 None，调用方优雅降级（不持久化）。
    pub fn open(path: PathBuf) -> Option<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .map_err(|error| {
                tracing::warn!(error = %error, path = %path.display(), "打开终端日志文件失败");
            })
            .ok()?;
        Some(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    /// 追加写一段文本。超过上限时滚动（保留尾部一半），防日志无限膨胀。
    /// 写失败仅记录警告，不影响终端主流程。
    pub fn append(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut guard = match self.file.lock() {
            Ok(file) => file,
            Err(_) => return,
        };
        if let Err(error) = guard.write_all(text.as_bytes()) {
            tracing::warn!(error = %error, "写终端日志失败");
            return;
        }
        if let Ok(meta) = guard.metadata()
            && meta.len() > MAX_LOG_BYTES
            && let Err(error) = rotate_tail(&mut guard, meta.len())
        {
            tracing::warn!(error = %error, "滚动终端日志失败");
        }
    }
}

/// 滚动日志：读取文件尾部一半，truncate 后重写，使文件回到约一半大小。
///
/// `len / 2` 是任意字节偏移，可能落在 UTF-8 多字节字符中间，直接
/// `read_to_string` 会因无效 UTF-8 失败；先跳过到下一个 `\n` 再读取。
fn rotate_tail(file: &mut File, len: u64) -> std::io::Result<()> {
    let keep_from = len / 2;
    file.seek(SeekFrom::Start(keep_from))?;
    let mut skip_buf = [0u8; 1];
    while file.read(&mut skip_buf)? > 0 {
        if skip_buf[0] == b'\n' {
            break;
        }
    }
    let mut tail = String::new();
    file.read_to_string(&mut tail)?;
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(tail.as_bytes())
}

/// 会话（scope）的日志路径；插件数据目录缺失时返回 None（不持久化）。
pub fn scope_log_path(scope_id: &str) -> Option<PathBuf> {
    let data_dir = std::env::var_os(tiangong_plugin_sidecar::env::PLUGIN_DATA_DIR_ENV)?;
    Some(
        PathBuf::from(data_dir)
            .join("terminal-logs")
            .join(format!("{}.log", sanitize_path_segment(scope_id))),
    )
}

pub fn clear_scope_log(scope_id: &str) -> std::io::Result<()> {
    let Some(path) = scope_log_path(scope_id) else {
        return Ok(());
    };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// 路径段清洗（对齐内置终端）：仅保留字母数字与 `-_`。
fn sanitize_path_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

// ── 行处理器（vte 状态机 + 单行光标模拟，移植自内置终端）──

/// 终端行处理器：把原始输出解析成静态文本行，模拟光标行为以正确处理
/// zsh 行编辑器的重绘。SGR（颜色）等不影响行内容的序列被忽略。
pub struct TerminalLineProcessor {
    parser: vte::Parser,
    handler: LineBuildHandler,
}

impl TerminalLineProcessor {
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            handler: LineBuildHandler::new(),
        }
    }

    pub fn process(&mut self, raw: &str) -> Vec<String> {
        // vte::Parser 内部维护状态机，跨 chunk 的不完整序列自动暂存
        self.parser.advance(&mut self.handler, raw.as_bytes());
        std::mem::take(&mut self.handler.complete_lines)
    }

    pub fn current_line(&self) -> String {
        self.handler.line.iter().collect()
    }
}

/// [`vte::Perform`] 实现：维护单行字符缓冲 + 光标位置，收集完整行。
struct LineBuildHandler {
    line: Vec<char>,
    cursor: usize,
    row: usize,
    complete_lines: Vec<String>,
}

impl LineBuildHandler {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            cursor: 0,
            row: 1,
            complete_lines: Vec::new(),
        }
    }

    /// 从 vte Params 提取第一个参数值（默认 0）。
    fn first_param(params: &vte::Params) -> usize {
        params.iter().next().map(|p| p[0]).unwrap_or(0) as usize
    }

    fn param(params: &vte::Params, index: usize, default: usize) -> usize {
        params
            .iter()
            .nth(index)
            .and_then(|param| param.first())
            .copied()
            .map(usize::from)
            .unwrap_or(default)
    }

    fn finish_line(&mut self) {
        let line: String = self.line.iter().collect();
        if !line.trim().is_empty() {
            self.complete_lines.push(line);
        }
        self.line.clear();
        self.cursor = 0;
    }
}

impl vte::Perform for LineBuildHandler {
    fn print(&mut self, c: char) {
        if self.cursor >= self.line.len() {
            self.line.push(c);
        } else {
            self.line[self.cursor] = c;
        }
        self.cursor += 1;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.finish_line();
                self.row = self.row.saturating_add(1);
            }
            b'\r' => self.cursor = 0,
            b'\x08' => self.cursor = self.cursor.saturating_sub(1),
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        c: char,
    ) {
        match c {
            // ConPTY 经虚拟终端序列移动到下一输出行，不一定发送 CRLF。
            'H' | 'f' => {
                let row = Self::param(params, 0, 1).max(1);
                let col = Self::param(params, 1, 1).max(1);
                if row != self.row {
                    self.finish_line();
                    self.row = row;
                }
                self.cursor = col - 1;
                while self.line.len() < self.cursor {
                    self.line.push(' ');
                }
            }
            // ESC[K — 清行
            'K' => match Self::first_param(params) {
                0 => self.line.truncate(self.cursor),
                1 => {
                    let after: Vec<char> = self.line.drain(self.cursor..).collect();
                    self.line = after;
                    self.cursor = 0;
                }
                2 => {
                    self.line.clear();
                    self.cursor = 0;
                }
                _ => {}
            },
            // ESC[G — 光标水平绝对定位
            'G' => {
                let col = Self::first_param(params).max(1);
                self.cursor = col - 1;
                while self.line.len() < self.cursor {
                    self.line.push(' ');
                }
            }
            // ESC[J — 清屏（单行模型只处理 2=全清）
            'J' => {
                if Self::first_param(params) >= 2 {
                    self.line.clear();
                    self.cursor = 0;
                }
            }
            // ESC[C — 光标右移
            'C' => {
                let n = Self::first_param(params).max(1);
                for _ in 0..n {
                    if self.cursor < self.line.len() {
                        self.cursor += 1;
                    } else {
                        self.line.push(' ');
                        self.cursor = self.line.len();
                    }
                }
            }
            // ESC[D — 光标左移
            'D' => {
                let n = Self::first_param(params).max(1);
                self.cursor = self.cursor.saturating_sub(n);
            }
            // ESC[P — 删除字符
            'P' => {
                let n = Self::first_param(params).max(1);
                for _ in 0..n {
                    if self.cursor < self.line.len() {
                        self.line.remove(self.cursor);
                    }
                }
            }
            // ESC[@ — 插入空白
            '@' => {
                let n = Self::first_param(params).max(1);
                for _ in 0..n {
                    self.line.insert(self.cursor, ' ');
                }
            }
            // SGR（颜色/样式）等其他序列不影响行内容收集，忽略
            _ => {}
        }
    }
}
