//! DeepSeek 文本工具调用兜底解析。
//!
//! 正常情况下 DeepSeek 通过结构化 `tool_calls` 字段返回工具调用。但部分场景下
//! 模型会把工具调用写进 `content` 文本，需要从文本中兜底解析，否则对话会因
//! 无法识别工具调用而异常终止。
//!
//! 已知两套文本协议，解析策略不同：
//!
//! 1. **原生协议**（DeepSeek tokenizer 内置 token，id 128806~128814）：
//!    标记是原子 token，模型输出时完整出现，不会断片。解析采用严格匹配，入口
//!    要求 `<｜tool▁calls▁begin｜>` 完整存在。竖线为全角 ｜（U+FF5C），
//!    分隔符为 ▁（U+2581）。
//!
//! 2. **DSML 协议**（V3.2 引入的 XML 风格，实测对话样本中出现）：
//!    标记不是原子 token，流式时按字符分片到达，标记可能残缺或外层包裹缺失。
//!    解析采用部分识别：只要出现 `<｜｜DSML｜｜invoke` 内层调用标记就尝试提取，
//!    不强依赖外层 `<｜｜DSML｜｜tool_calls>` 包裹完整。
//!
//! 解析采用纯字符串扫描，不引入正则依赖。

use serde_json::Value;

/// 解析出的工具调用（名称 + 参数 JSON 文本，由上层转成 `Value`）。
#[derive(Debug, Clone, PartialEq)]
pub struct DsmlToolCall {
    pub name: String,
    pub arguments: String,
}

/// 文本协议工具调用 id 全局计数器：进程内单调递增，保证跨轮唯一
/// （官方要求 call_id 非空且唯一，Responses 历史每轮全量回传时不得重复）。
static TEXT_CALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 生成全局唯一的文本协议工具调用 id。
pub fn next_text_call_id() -> String {
    use std::sync::atomic::Ordering;
    format!("textcall_{}", TEXT_CALL_SEQ.fetch_add(1, Ordering::Relaxed))
}
// ── 原生协议标记（全角竖线 U+FF5C + 下块 U+2581） ───────────────────────

pub const NATIVE_CALLS_BEGIN: &str = "<｜tool▁calls▁begin｜>";
pub const NATIVE_CALLS_END: &str = "<｜tool▁calls▁end｜>";
const NATIVE_CALL_BEGIN: &str = "<｜tool▁call▁begin｜>";
const NATIVE_CALL_END: &str = "<｜tool▁call▁end｜>";
const NATIVE_SEP: &str = "<｜tool▁sep｜>";

// ── DSML 协议标记（双全角竖线） ───────────────────────────────────────

/// 全角竖线常量（U+FF5C）。
const BAR: char = '｜';
const DSML_TOOL_CALLS_OPEN: &str = "<｜｜DSML｜｜tool_calls>";
pub const DSML_TOOL_CALLS_CLOSE: &str = "</｜｜DSML｜｜tool_calls>";
/// DSML invoke 闭合标签。
pub const DSML_INVOKE_CLOSE: &str = "</｜｜DSML｜｜invoke>";

/// 尝试从 `content` 文本中解析工具调用。
///
/// 依次尝试原生协议与 DSML 协议；命中任一即返回。两者都不命中时返回 `None`，
/// 表示该 content 是普通文本。
pub fn parse_dsml_tool_calls(content: &str) -> Option<Vec<DsmlToolCall>> {
    // 原生协议：标记是原子 token，严格匹配。
    if content.contains(NATIVE_CALLS_BEGIN)
        && let Some(calls) = parse_native(content)
    {
        return Some(calls);
    }
    // DSML 协议：标记非原子 token，部分识别——只要含内层 invoke 标记即尝试提取。
    if content.contains(DSML_INVOKE_PREFIX) {
        return parse_dsml(content);
    }
    None
}

/// 从 `content` 中移除工具调用文本块，返回剩余的可见文本。
///
/// 工具调用兜底解析成功后用于剥离标记原文，避免把标记当作回复展示给用户。
pub fn strip_tool_call_block(content: &str) -> String {
    // 原生协议：标记是原子 token，剥离 calls_begin 到 calls_end 之间（含）全部内容。
    if let Some(begin) = content.find(NATIVE_CALLS_BEGIN) {
        let prefix = &content[..begin];
        let after = &content[begin..];
        let suffix = after
            .find(NATIVE_CALLS_END)
            .map(|e| &after[e + NATIVE_CALLS_END.len()..])
            .unwrap_or_default();
        return format!("{prefix}{suffix}").trim().to_string();
    }
    // DSML 协议：标记非原子 token，外层包裹可能残缺。先剥外层包裹区间（若有），
    // 再逐个剥除散落的 invoke 块，最后清理残留的孤立标记。
    let mut text = content.to_string();
    text = strip_span(&text, DSML_TOOL_CALLS_OPEN, DSML_TOOL_CALLS_CLOSE);
    text = strip_dsml_invokes(&text);
    // 清理可能残留的外层孤立标记（包裹残缺时）。
    text = text
        .replace(DSML_TOOL_CALLS_OPEN, "")
        .replace(DSML_TOOL_CALLS_CLOSE, "");
    text.trim().to_string()
}

/// 剥离 `open...close` 包裹区间（含两端标记）。close 缺失时剥到末尾。
fn strip_span(text: &str, open: &str, close: &str) -> String {
    let Some(begin) = text.find(open) else {
        return text.to_string();
    };
    let prefix = &text[..begin];
    let after = &text[begin + open.len()..];
    let suffix = after
        .find(close)
        .map(|e| &after[e + close.len()..])
        .unwrap_or_default();
    format!("{prefix}{suffix}")
}

/// 逐个剥离 DSML invoke 块（含起止标记），适配外层包裹缺失时散落的调用。
fn strip_dsml_invokes(text: &str) -> String {
    let invoke_close = format!("</{B}{B}DSML{B}{B}invoke>", B = BAR);
    let mut remaining = text;
    let mut result = String::with_capacity(text.len());

    loop {
        let Some(start) = remaining.find(DSML_INVOKE_PREFIX) else {
            result.push_str(remaining);
            break;
        };
        // 保留 invoke 标记之前的普通文本。
        result.push_str(&remaining[..start]);
        let after_prefix = &remaining[start + DSML_INVOKE_PREFIX.len()..];
        // 起始标签以 `>` 结束（属性值不含 `>`）。
        let Some(tag_end) = after_prefix.find('>') else {
            // 标签未闭合（截断）：剩余全是 invoke 残片，丢弃。
            break;
        };
        let after_tag = &after_prefix[tag_end + 1..];
        match after_tag.find(&invoke_close) {
            Some(close_rel) => {
                // 跳过整个 invoke 块，继续处理其后内容。
                remaining = &after_tag[close_rel + invoke_close.len()..];
            }
            None => {
                // 闭合缺失：剩余全是 invoke 内容，丢弃。
                break;
            }
        }
    }
    result
}

// ── 原生协议解析 ─────────────────────────────────────────────────────

fn parse_native(content: &str) -> Option<Vec<DsmlToolCall>> {
    let begin = content.find(NATIVE_CALLS_BEGIN)?;
    let after = &content[begin + NATIVE_CALLS_BEGIN.len()..];
    let body_end = after.find(NATIVE_CALLS_END).unwrap_or(after.len());
    let body = &after[..body_end];

    let mut calls = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = body[cursor..].find(NATIVE_CALL_BEGIN) {
        let abs = cursor + rel;
        let after_cb = &body[abs + NATIVE_CALL_BEGIN.len()..];
        // 原生格式：<｜tool▁call▁begin｜>function<｜tool▁sep｜>{name}\n```json\n{args}\n```\n<｜tool▁call▁end｜>
        let Some(sep_rel) = after_cb.find(NATIVE_SEP) else {
            break;
        };
        // function_part 形如 "function"，真正的函数名在 sep 之后。
        let after_sep = &after_cb[sep_rel + NATIVE_SEP.len()..];

        let (inner, next_cursor) = match after_sep.find(NATIVE_CALL_END) {
            Some(end_rel) => {
                let inner = &after_sep[..end_rel];
                let next_cursor = abs
                    + NATIVE_CALL_BEGIN.len()
                    + sep_rel
                    + NATIVE_SEP.len()
                    + end_rel
                    + NATIVE_CALL_END.len();
                (inner, next_cursor)
            }
            None => (after_sep, body.len()),
        };

        let (name, arguments) = split_name_and_args(inner);
        let name = name.trim().to_string();
        if !name.is_empty() {
            calls.push(DsmlToolCall {
                name,
                arguments: arguments.trim().to_string(),
            });
        }
        if next_cursor >= body.len() {
            break;
        }
        cursor = next_cursor;
    }

    (!calls.is_empty()).then_some(calls)
}

/// 原生协议 sep 之后的内容形如 `{函数名}\n```json\n{参数}\n```\n`，拆出名称与参数。
fn split_name_and_args(inner: &str) -> (String, String) {
    // 函数名在第一行（可能紧跟换行或直接是 ```json 围栏）。
    let first_newline = inner.find('\n').unwrap_or(inner.len());
    let name = inner[..first_newline].trim().to_string();
    let rest = &inner[first_newline..];

    // 参数通常在 ```json ... ``` 围栏内；找不到围栏时把 rest 去空白当作参数。
    let arguments = extract_code_fence(rest).unwrap_or_else(|| rest.trim().to_string());
    (name, arguments)
}

/// 提取 ```json ... ``` 围栏内的内容。
fn extract_code_fence(text: &str) -> Option<String> {
    let fence_start = text.find("```")?;
    let after_fence = &text[fence_start + 3..];
    // 跳过语言标识（json）到下一个换行。
    let lang_end = after_fence.find('\n')?;
    let json_start = fence_start + 3 + lang_end + 1;
    let rest = &text[json_start..];
    let fence_end = rest.find("```")?;
    Some(rest[..fence_end].trim().to_string())
}

// ── DSML 协议解析 ────────────────────────────────────────────────────

/// DSML 内层调用起始标记：`<｜｜DSML｜｜invoke `。
/// 作为部分识别的锚点——只要出现此标记就尝试提取工具调用，不依赖外层包裹完整。
const DSML_INVOKE_PREFIX: &str = "<｜｜DSML｜｜invoke ";

fn parse_dsml(content: &str) -> Option<Vec<DsmlToolCall>> {
    // DSML 标记不是原子 token，流式时按字符分片到达，外层 <｜｜DSML｜｜tool_calls>
    // 包裹可能残缺或完全缺失。因此不依赖外层标记，直接对全文扫描内层 invoke。
    let calls = parse_dsml_invokes(content);
    (!calls.is_empty()).then_some(calls)
}

/// 扫描 `<｜｜DSML｜｜invoke name="..."> ... </｜｜DSML｜｜invoke>` 片段。
fn parse_dsml_invokes(body: &str) -> Vec<DsmlToolCall> {
    let mut calls = Vec::new();
    let invoke_close = format!("</{B}{B}DSML{B}{B}invoke>", B = BAR);
    let mut remaining = body;

    while let Some(start) = remaining.find(DSML_INVOKE_PREFIX) {
        let after_prefix = &remaining[start + DSML_INVOKE_PREFIX.len()..];

        let (name, rest) = match extract_dsml_name_and_close(after_prefix) {
            Some(parsed) => parsed,
            None => break,
        };

        let (inner, next_remaining) = match rest.find(&invoke_close) {
            Some(close_rel) => {
                let inner = &rest[..close_rel];
                let after = &rest[close_rel + invoke_close.len()..];
                (inner, after)
            }
            None => (rest, ""),
        };

        let arguments = parse_dsml_parameters(inner);
        calls.push(DsmlToolCall { name, arguments });
        remaining = next_remaining;
        if remaining.is_empty() {
            break;
        }
    }

    calls
}

/// 解析 invoke 起始标签：`name="read_file">` → (`read_file`, 标签后的正文)。
fn extract_dsml_name_and_close(after_name_attr: &str) -> Option<(String, &str)> {
    let name_attr = "name=\"";
    let n_start = after_name_attr.find(name_attr)?;
    let value_start = n_start + name_attr.len();
    let value_end = after_name_attr[value_start..].find('"')?;
    let name = after_name_attr[value_start..value_start + value_end].to_string();
    let tag_close = after_name_attr[value_start + value_end..].find('>')?;
    let rest = &after_name_attr[value_start + value_end + tag_close + 1..];
    Some((name, rest))
}

/// 把 invoke 内部所有 `<｜｜DSML｜｜parameter name="...">值</...parameter>` 拼成 JSON 对象字符串。
fn parse_dsml_parameters(inner: &str) -> String {
    let param_open_prefix = format!("<{B}{B}DSML{B}{B}parameter ", B = BAR);
    let param_close = format!("</{B}{B}DSML{B}{B}parameter>", B = BAR);

    let mut map = serde_json::Map::new();
    let mut cursor = 0usize;
    while let Some(rel) = inner[cursor..].find(&param_open_prefix) {
        let abs = cursor + rel;
        let after_tag = &inner[abs + param_open_prefix.len()..];

        let Some(((key, value), next_cursor)) =
            extract_dsml_param(inner, after_tag, abs, &param_close)
        else {
            break;
        };
        map.insert(key, Value::String(value));
        if next_cursor >= inner.len() {
            break;
        }
        cursor = next_cursor;
    }

    Value::Object(map).to_string()
}

/// 解析单个 parameter：取出属性名与标签内文本值。
fn extract_dsml_param<'a>(
    inner: &'a str,
    after_tag: &'a str,
    abs: usize,
    param_close: &str,
) -> Option<((String, String), usize)> {
    let name_attr = "name=\"";
    let n_start = after_tag.find(name_attr)?;
    let value_start = n_start + name_attr.len();
    let value_end = after_tag[value_start..].find('"')?;
    let key = after_tag[value_start..value_start + value_end].to_string();

    let tag_close = after_tag[value_start + value_end..].find('>')?;
    let content_start = value_start + value_end + tag_close + 1;

    match after_tag[content_start..].find(param_close) {
        Some(close_rel) => {
            let value = after_tag[content_start..content_start + close_rel].to_string();
            let next_cursor = abs + param_close.len() + content_start + close_rel;
            Some(((key, dsml_unescape(value)), next_cursor))
        }
        None => {
            let value = after_tag[content_start..].to_string();
            Some(((key, dsml_unescape(value)), inner.len()))
        }
    }
}

/// 还原 DSML 参数值中常见的转义。
fn dsml_unescape(value: String) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 原生协议测试 ─────────────────────────────────────────────

    fn native_sample_two_calls() -> String {
        format!(
            "{cb}{cbb}function{sep}get_current_weather\n```json\n{{\"location\": \"San Francisco, CA\"}}\n```\n{ce}{cbb}function{sep}get_time\n```json\n{{\"timezone\": \"UTC\"}}\n```\n{ce}{cbe}",
            cb = NATIVE_CALLS_BEGIN,
            cbe = NATIVE_CALLS_END,
            cbb = NATIVE_CALL_BEGIN,
            ce = NATIVE_CALL_END,
            sep = NATIVE_SEP,
        )
    }

    #[test]
    fn native_parses_two_calls() {
        let calls = parse_native(&native_sample_two_calls()).expect("应解析出工具调用");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "get_current_weather");
        assert_eq!(calls[0].arguments, "{\"location\": \"San Francisco, CA\"}");
        assert_eq!(calls[1].name, "get_time");
        assert_eq!(calls[1].arguments, "{\"timezone\": \"UTC\"}");
    }

    #[test]
    fn native_handles_no_code_fence() {
        // 无 ```json 围栏的变体。
        let sample = format!(
            "{cb}{cbb}function{sep}get_x\n{{\"a\": 1}}\n{ce}{cbe}",
            cb = NATIVE_CALLS_BEGIN,
            cbe = NATIVE_CALLS_END,
            cbb = NATIVE_CALL_BEGIN,
            ce = NATIVE_CALL_END,
            sep = NATIVE_SEP,
        );
        let calls = parse_native(&sample).expect("应解析");
        assert_eq!(calls[0].name, "get_x");
        assert_eq!(calls[0].arguments, "{\"a\": 1}");
    }

    #[test]
    fn native_handles_truncated() {
        // calls_end 缺失的截断场景。
        let sample = format!(
            "{cb}{cbb}function{sep}get_x\n```json\n{{\"a\": 1}}\n```\n{ce}",
            cb = NATIVE_CALLS_BEGIN,
            cbb = NATIVE_CALL_BEGIN,
            ce = NATIVE_CALL_END,
            sep = NATIVE_SEP,
        );
        let calls = parse_native(&sample).expect("截断也应解析");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_x");
    }

    // ── DSML 协议测试（真实对话样本） ──────────────────────────────

    const DSML_SAMPLE_73: &str = "<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name=\"read_file\">
<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/Users/hubertshelley/Documents/silent/tiangong/crates/plugins/tiangong-plugin-index/wasm/src/index.css</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
<｜｜DSML｜｜invoke name=\"read_file\">
<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/Users/hubertshelley/Documents/silent/tiangong/crates/plugins/tiangong-plugin-scheduler/wasm/src/scheduler.css</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>";

    const DSML_SAMPLE_140: &str = "<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name=\"run_shell\">
<｜｜DSML｜｜parameter name=\"script\" string=\"true\">cd /tmp && node build-harness.mjs 2>&1; echo \"exit=$?\"</｜｜DSML｜｜parameter>
<｜｜DSML｜｜parameter name=\"timeout\" string=\"false\">60</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>";

    #[test]
    fn dsml_parses_two_read_file_calls() {
        let calls = parse_dsml_tool_calls(DSML_SAMPLE_73).expect("应解析");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_file");
        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(
            args["path"],
            "/Users/hubertshelley/Documents/silent/tiangong/crates/plugins/tiangong-plugin-index/wasm/src/index.css"
        );
    }

    #[test]
    fn dsml_parses_multi_param_call() {
        let calls = parse_dsml_tool_calls(DSML_SAMPLE_140).expect("应解析");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_shell");
        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert!(
            args["script"]
                .as_str()
                .unwrap()
                .contains("build-harness.mjs")
        );
        assert_eq!(args["timeout"], "60");
    }

    #[test]
    fn dsml_handles_truncated_unclosed() {
        let truncated = "<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name=\"run_shell\">
<｜｜DSML｜｜parameter name=\"script\" string=\"true\">ls -la";
        let calls = parse_dsml_tool_calls(truncated).expect("截断也应解析");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_shell");
    }

    #[test]
    fn dsml_unescapes_entities() {
        let text = "<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name=\"search\">
<｜｜DSML｜｜parameter name=\"q\" string=\"true\">a &lt; b &amp; c &gt; d</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>";
        let calls = parse_dsml_tool_calls(text).expect("应解析");
        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["q"], "a < b & c > d");
    }

    // ── 统一入口与工具函数 ────────────────────────────────────────

    #[test]
    fn dispatch_picks_native_first() {
        let calls = parse_dsml_tool_calls(&native_sample_two_calls()).expect("应解析");
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn returns_none_for_plain_text() {
        assert!(parse_dsml_tool_calls("这是一段普通回复").is_none());
        assert!(parse_dsml_tool_calls("").is_none());
    }

    // 缓冲状态机的判定逻辑（Idle→Probing→Confirmed）在 chat.rs 的 buffer_tests 中覆盖。

    #[test]
    fn strip_removes_native_block() {
        let mixed = format!("我来查询。\n{}\n完成", native_sample_two_calls());
        let leftover = strip_tool_call_block(&mixed);
        assert_eq!(leftover, "我来查询。\n\n完成");
    }

    #[test]
    fn strip_removes_dsml_block() {
        let mixed = format!("我来读取文件。\n{}\n完成", DSML_SAMPLE_73);
        let leftover = strip_tool_call_block(&mixed);
        assert_eq!(leftover, "我来读取文件。\n\n完成");
    }

    // ── DSML 部分识别（外层包裹缺失/残缺） ─────────────────────────

    #[test]
    fn dsml_parses_without_outer_wrapper() {
        // 完全没有 <｜｜DSML｜｜tool_calls> 外层包裹，只有散落的 invoke。
        let text = "<｜｜DSML｜｜invoke name=\"read_file\">
<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>";
        let calls = parse_dsml_tool_calls(text).expect("无外层包裹也应解析");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["path"], "/tmp/x");
    }

    #[test]
    fn dsml_parses_with_truncated_outer_wrapper() {
        // 外层包裹只有起始标记，缺失闭合。
        let text = "<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name=\"run_shell\">
<｜｜DSML｜｜parameter name=\"script\" string=\"true\">ls</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>";
        let calls = parse_dsml_tool_calls(text).expect("外层包裹残缺也应解析");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_shell");
    }

    #[test]
    fn dsml_parses_multiple_bare_invokes() {
        // 多个散落 invoke，无外层包裹。
        let text = "<｜｜DSML｜｜invoke name=\"fn_a\">
<｜｜DSML｜｜parameter name=\"x\" string=\"true\">1</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
中间普通文本
<｜｜DSML｜｜invoke name=\"fn_b\">
<｜｜DSML｜｜parameter name=\"y\" string=\"true\">2</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>";
        let calls = parse_dsml_tool_calls(text).expect("应解析");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "fn_a");
        assert_eq!(calls[1].name, "fn_b");
    }

    #[test]
    fn strip_handles_bare_invokes() {
        // 剥离散落 invoke 块后保留中间普通文本。
        let text = "前文
<｜｜DSML｜｜invoke name=\"fn_a\">
<｜｜DSML｜｜parameter name=\"x\" string=\"true\">1</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
中间文本
<｜｜DSML｜｜invoke name=\"fn_b\">
<｜｜DSML｜｜parameter name=\"y\" string=\"true\">2</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
后文";
        let leftover = strip_tool_call_block(text);
        assert!(leftover.contains("前文"));
        assert!(leftover.contains("中间文本"));
        assert!(leftover.contains("后文"));
        assert!(!leftover.contains("DSML"));
        assert!(!leftover.contains("fn_a"));
    }
}
