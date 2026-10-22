use crate::runtime::LlmOutputRecord;
use crate::tools::result::{ToolExecutionRecord, ToolResult};

pub(crate) fn format_llm_output_message(output: &LlmOutputRecord) -> String {
    let mut lines = vec![format!("LLM 输出 [{}]", output.stage)];
    if output.usage.total_tokens > 0 {
        lines.push(format!(
            "tokens: prompt={}, completion={}, total={}",
            output.usage.prompt_tokens, output.usage.completion_tokens, output.usage.total_tokens
        ));
    }
    if !output.tool_calls.is_empty() {
        lines.push(format!("tool_calls: {}", output.tool_calls.join(", ")));
    }
    // reasoning_content 不写入系统消息的 content，避免作为上下文重复提交给 LLM
    // thinking 内容仅通过 message.reasoning_content 字段保留用于 TUI 展示
    if !output.content.trim().is_empty() {
        lines.push(format!("content:\n{}", output.content.trim()));
    }
    lines.join("\n")
}

pub(crate) fn format_tool_trace_message(result: &ToolResult) -> String {
    let Some(record) = result.execution.as_ref() else {
        let mut lines = vec!["工具执行 [unknown]".to_string()];
        lines.push(format!("summary: {}", result.summary));
        if !result.stdout.trim().is_empty() {
            lines.push("stdout:".to_string());
            lines.push("```text".to_string());
            lines.push(result.stdout.clone());
            lines.push("```".to_string());
        }
        if !result.stderr.trim().is_empty() {
            lines.push("stderr:".to_string());
            lines.push("```text".to_string());
            lines.push(result.stderr.clone());
            lines.push("```".to_string());
        }
        return lines.join("\n");
    };

    let mut lines = vec![format!("工具执行 [{}]", record.tool_name)];
    if let Some(command) = format_tool_command(record) {
        lines.push(format!("命令: {command}"));
    }
    lines.push(format!(
        "ok={} exit_code={} duration_ms={}",
        result.ok, result.exit_code, record.duration_ms
    ));
    lines.push(format!("summary: {}", result.summary));
    if !result.stdout.trim().is_empty() {
        lines.push("stdout:".to_string());
        lines.push("```text".to_string());
        lines.push(result.stdout.clone());
        lines.push("```".to_string());
    }
    if !result.stderr.trim().is_empty() {
        lines.push("stderr:".to_string());
        lines.push("```text".to_string());
        lines.push(result.stderr.clone());
        lines.push("```".to_string());
    }
    lines.join("\n")
}

fn format_tool_command(record: &ToolExecutionRecord) -> Option<String> {
    let args = record
        .args
        .iter()
        .filter(|arg| !arg.starts_with("__tiangong_cwd="))
        .cloned()
        .collect::<Vec<_>>();
    if args.is_empty() {
        return None;
    }

    // 通用格式：不按工具名区分，整体压成单行并截断（只进运行日志）。
    Some(single_line_ellipsis(&args.join(" "), 240))
}

fn single_line_ellipsis(text: &str, max_chars: usize) -> String {
    let normalized = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return String::new();
    }
    let mut chars = normalized.chars();
    let preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}
