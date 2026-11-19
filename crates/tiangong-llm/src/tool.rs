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

/// 工具调用 id 分配器：新调用**首次构造** `ToolCall` 时分配最终 id，
/// 同一输出项只分配一次；此后执行、克隆、落盘、构造 `ToolResult` 与
/// 下一轮回传均复用同一 id。
///
/// 分配规则（相对本次请求历史与本轮响应保证唯一）：
/// 1. 上游 `call_id` 非空且未占用 → 原样保留（以 server 为主，可与其
///    日志对账，历史前缀稳定利于 context caching）；
/// 2. 上游 `call_id` 缺失、item id 非空且未占用 → 沿用 item id
///    （Responses 系协议的兼容行为）；
/// 3. 其余（缺失、或已被历史/本轮占用）→ 生成 `call_<scru128>` 重试
///    直到占位成功。上游 id 存在但冲突时**不保留**：重复 id 已失去
///    区分价值（服务端日志中其本身就歧义），保留必致唯一性校验失败。
///
/// 各协议适配器在「先识别出调用（按输出项 / 内容块 / choice 关联）、
/// 再构造 ToolCall」时调用；占用集以本次请求实际回传的历史 id 初始化
///（[`Self::from_messages`]）。不宣称能检测已被上下文裁掉的历史 id。
#[derive(Debug, Default, Clone)]
pub(crate) struct ToolCallIdAllocator {
    used: std::collections::HashSet<String>,
}

impl ToolCallIdAllocator {
    /// 以消息历史中已回传过的调用 id 初始化占用集（assistant 的
    /// ToolCall 与 Tool 消息的 ToolResult 两侧）。
    pub(crate) fn from_messages(messages: &[crate::message::ChatMessage]) -> Self {
        let mut used = std::collections::HashSet::new();
        for message in messages {
            for content in &message.content {
                match content {
                    crate::message::MessageContent::ToolCall(call) if !call.id.is_empty() => {
                        used.insert(call.id.clone());
                    }
                    crate::message::MessageContent::ToolResult(result)
                        if !result.tool_call_id.is_empty() =>
                    {
                        used.insert(result.tool_call_id.clone());
                    }
                    _ => {}
                }
            }
        }
        Self { used }
    }

    /// 为一个新调用的首次构造分配最终 id（见类型文档的分配规则）。
    /// 分配成功即登记占用。
    pub(crate) fn allocate(&mut self, call_id: &str, item_id: Option<&str>) -> String {
        if !call_id.trim().is_empty() && self.used.insert(call_id.to_string()) {
            return call_id.to_string();
        }
        if let Some(item) = item_id.filter(|item| !item.trim().is_empty())
            && self.used.insert(item.to_string())
        {
            return item.to_string();
        }
        loop {
            let candidate = format!("call_{}", scru128::new());
            if self.used.insert(candidate.clone()) {
                return candidate;
            }
        }
    }
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
