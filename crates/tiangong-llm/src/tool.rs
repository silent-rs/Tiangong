use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 统一工具定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// 统一工具选择策略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// 由模型自行决定是否调用工具（提供 tools 时的默认行为）。
    Auto,
    /// 强制必须调用某个工具（任意一个）。
    Any,
    /// 强制调用指定名称的工具。
    Tool(String),
    /// 禁止调用任何工具。
    ///
    /// 用于「提供 tools schema（保持 KV cache 前缀一致）但明确不允许模型在本阶段
    /// 发起工具调用」的场景，例如总结阶段：模型只应产出文本最终回复。
    /// 映射到各 provider 的 none 语义（OpenAI/DeepSeek `"none"`、Anthropic `{"type":"none"}`）。
    None,
}

/// 统一工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// 解析 Provider 返回的工具参数。解析失败时保留为结构化错误，交给上层 ReAct 恢复链路处理。
///
/// 空参数（`""` / 纯空白）按**无参调用**处理返回空对象——无参工具
/// （如 `list_local_plugins`）的合法调用形态，不构成解析错误；仅真正的
/// JSON 语法错误才进入 `__parse_error` 恢复链路。
pub fn parse_tool_arguments_or_error(tool_name: &str, call_id: &str, raw_args: &str) -> Value {
    if raw_args.trim().is_empty() {
        return json!({});
    }

    serde_json::from_str(raw_args).unwrap_or_else(|err| {
        let raw_preview: String = raw_args.chars().take(512).collect();
        json!({
            "__parse_error": format!(
                "工具参数 JSON 无效：tool={tool_name} id={call_id} error={err}。请重新生成完整 JSON 参数后再调用工具，不要把 __parse_error 当作真实参数。"
            ),
            "__raw_args_preview": raw_preview,
        })
    })
}

/// 工具结果内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultContent {
    Text(String),
    Json(Value),
}

/// 统一工具结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: ToolResultContent,
    #[serde(default)]
    pub is_error: bool,
}
