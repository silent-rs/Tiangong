//! 面向第三方 Agent 的通用记忆接口类型。
//!
//! 天工通过 WASM 生命周期钩子自动提交完整的轮次快照；外部 Agent（MCP 客户端、
//! HTTP 调用方）没有天工的会话结构，只能显式上报"用户说了什么、助手答了什么、
//! 调用了哪些工具"。本模块把这种精简输入转换为 Memory 内部的
//! [`EnhancedTurnResult`]，保证两条入口走同一套反刍与存储链路。

use serde::{Deserialize, Serialize};

use crate::types::{
    EnhancedTurnResult, MemoryCandidate, TurnArtifact, TurnArtifactKind, TurnMessage, TurnStatus,
};

/// 单条消息写入记忆前的最大字符数，避免外部调用方一次提交超长上下文。
const MAX_TEXT_CHARS: usize = 8_000;
/// 工具结果摘要的最大字符数。
const MAX_TOOL_SUMMARY_CHARS: usize = 2_000;

/// 外部 Agent 上报的一次工具调用。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExternalToolCall {
    pub name: String,
    #[serde(default = "default_true")]
    pub success: bool,
    /// 工具结果摘要（不必是完整输出）。
    #[serde(default)]
    pub summary: Option<String>,
    /// 涉及的文件路径。
    #[serde(default)]
    pub path: Option<String>,
    /// 涉及的 URL。
    #[serde(default)]
    pub url: Option<String>,
}

/// 外部 Agent 上报的一轮对话。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExternalTurn {
    pub session_id: String,
    /// 轮次 ID，缺省时自动生成。
    #[serde(default)]
    pub turn_id: Option<String>,
    /// 工作区：可传路径或名称，路径会按天工规则取末级目录名。
    #[serde(default)]
    pub workspace: Option<String>,
    pub user_input: String,
    #[serde(default)]
    pub assistant_output: String,
    #[serde(default)]
    pub tool_calls: Vec<ExternalToolCall>,
    /// `completed` | `cancelled` | `failed`，缺省 `completed`。
    #[serde(default)]
    pub status: Option<String>,
}

fn default_true() -> bool {
    true
}

/// 规范化工作区标识，与天工宿主 `PluginSession::workspace_id` 规则一致：
/// 路径取末级目录名，空值视为未指定。
///
/// 这样外部 Agent 传入项目路径时，与天工在同一项目下产生的记忆落在同一作用域。
pub fn normalize_workspace_id(workspace: Option<&str>) -> Option<String> {
    let raw = workspace?.trim();
    if raw.is_empty() {
        return None;
    }
    let trimmed = raw.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return Some(raw.to_string());
    }
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .or_else(|| Some(trimmed.to_string()))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push('…');
    truncated
}

fn parse_status(status: Option<&str>) -> anyhow::Result<TurnStatus> {
    match status.map(str::trim).unwrap_or_default() {
        "" | "completed" | "success" => Ok(TurnStatus::Completed),
        "cancelled" | "canceled" => Ok(TurnStatus::Cancelled),
        "failed" | "error" => Ok(TurnStatus::Failed),
        other => anyhow::bail!("未知的轮次状态：{other}（可选 completed/cancelled/failed）"),
    }
}

impl ExternalTurn {
    /// 校验并转换为 Memory 内部的增强轮次结果。
    pub fn into_enhanced(self) -> anyhow::Result<EnhancedTurnResult> {
        let session_id = self.session_id.trim().to_string();
        if session_id.is_empty() {
            anyhow::bail!("session_id 不能为空");
        }
        let user_input = truncate_chars(&self.user_input, MAX_TEXT_CHARS);
        let assistant_output = truncate_chars(&self.assistant_output, MAX_TEXT_CHARS);
        if user_input.is_empty() && assistant_output.is_empty() && self.tool_calls.is_empty() {
            anyhow::bail!("user_input、assistant_output、tool_calls 不能同时为空");
        }
        let turn_status = parse_status(self.status.as_deref())?;
        let turn_id = self
            .turn_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| scru128::new().to_string());

        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|call| !call.name.trim().is_empty())
            .collect::<Vec<_>>();
        let memory_candidates = tool_calls
            .iter()
            .enumerate()
            .map(|(step_index, call)| MemoryCandidate {
                tool_name: call.name.trim().to_string(),
                step_index,
                hint: String::new(),
                suggested_kinds: Vec::new(),
                file_path: call.path.clone(),
                url: call.url.clone(),
                result_summary: call
                    .summary
                    .as_deref()
                    .map(|summary| truncate_chars(summary, MAX_TOOL_SUMMARY_CHARS)),
                success: call.success,
                tool_source: Some("external".to_string()),
                is_recalled_context: false,
            })
            .collect::<Vec<_>>();
        let artifacts = tool_calls
            .iter()
            .filter(|call| call.success && (call.path.is_some() || call.url.is_some()))
            .map(|call| TurnArtifact {
                kind: if call.path.is_some() {
                    TurnArtifactKind::File
                } else {
                    TurnArtifactKind::ToolResult
                },
                tool_name: Some(call.name.trim().to_string()),
                title: None,
                url: call.url.clone(),
                path: call.path.clone(),
                summary: call
                    .summary
                    .as_deref()
                    .map(|summary| truncate_chars(summary, MAX_TOOL_SUMMARY_CHARS)),
            })
            .collect::<Vec<_>>();

        let mut turn_messages = Vec::new();
        if !user_input.is_empty() {
            turn_messages.push(TurnMessage {
                role: "user".to_string(),
                content: user_input.clone(),
            });
        }
        for call in &tool_calls {
            if let Some(summary) = call.summary.as_deref().filter(|s| !s.trim().is_empty()) {
                turn_messages.push(TurnMessage {
                    role: "tool".to_string(),
                    content: format!(
                        "{}: {}",
                        call.name.trim(),
                        truncate_chars(summary, MAX_TOOL_SUMMARY_CHARS)
                    ),
                });
            }
        }
        if !assistant_output.is_empty() {
            turn_messages.push(TurnMessage {
                role: "assistant".to_string(),
                content: assistant_output.clone(),
            });
        }

        Ok(EnhancedTurnResult {
            session_id,
            turn_id,
            had_tool_calls: !tool_calls.is_empty(),
            turn_status,
            user_input,
            summary: assistant_output,
            tool_calls: tool_calls
                .iter()
                .map(|call| call.name.trim().to_string())
                .collect(),
            artifacts,
            workspace_id: normalize_workspace_id(self.workspace.as_deref()),
            memory_candidates,
            turn_messages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_follows_host_rule() {
        assert_eq!(
            normalize_workspace_id(Some("/Users/me/projects/tiangong/")),
            Some("tiangong".to_string())
        );
        assert_eq!(
            normalize_workspace_id(Some(r"C:\work\demo")),
            Some("demo".to_string())
        );
        assert_eq!(
            normalize_workspace_id(Some("demo")),
            Some("demo".to_string())
        );
        assert_eq!(normalize_workspace_id(Some("  ")), None);
        assert_eq!(normalize_workspace_id(None), None);
        assert_eq!(normalize_workspace_id(Some("/")), Some("/".to_string()));
    }

    #[test]
    fn turn_converts_to_enhanced_result() {
        let turn = ExternalTurn {
            session_id: "s1".to_string(),
            workspace: Some("/tmp/proj".to_string()),
            user_input: "帮我生成报告".to_string(),
            assistant_output: "报告已写入 report.md".to_string(),
            tool_calls: vec![
                ExternalToolCall {
                    name: "write_file".to_string(),
                    success: true,
                    summary: Some("写入 1KB".to_string()),
                    path: Some("/tmp/proj/report.md".to_string()),
                    url: None,
                },
                ExternalToolCall {
                    name: "  ".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let enhanced = turn.into_enhanced().expect("转换成功");
        assert_eq!(enhanced.session_id, "s1");
        assert!(!enhanced.turn_id.is_empty());
        assert_eq!(enhanced.workspace_id.as_deref(), Some("proj"));
        assert!(enhanced.had_tool_calls);
        assert_eq!(enhanced.tool_calls, vec!["write_file".to_string()]);
        assert_eq!(enhanced.memory_candidates.len(), 1);
        assert_eq!(
            enhanced.memory_candidates[0].tool_source.as_deref(),
            Some("external")
        );
        assert_eq!(enhanced.artifacts.len(), 1);
        assert_eq!(enhanced.artifacts[0].kind, TurnArtifactKind::File);
        let roles = enhanced
            .turn_messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>();
        assert_eq!(roles, vec!["user", "tool", "assistant"]);
        assert_eq!(enhanced.summary, "报告已写入 report.md");
        assert_eq!(enhanced.turn_status, TurnStatus::Completed);
    }

    #[test]
    fn turn_rejects_invalid_input() {
        assert!(
            ExternalTurn {
                session_id: " ".to_string(),
                user_input: "hi".to_string(),
                ..Default::default()
            }
            .into_enhanced()
            .is_err()
        );
        assert!(
            ExternalTurn {
                session_id: "s".to_string(),
                ..Default::default()
            }
            .into_enhanced()
            .is_err()
        );
        assert!(
            ExternalTurn {
                session_id: "s".to_string(),
                user_input: "hi".to_string(),
                status: Some("weird".to_string()),
                ..Default::default()
            }
            .into_enhanced()
            .is_err()
        );
        let cancelled = ExternalTurn {
            session_id: "s".to_string(),
            user_input: "hi".to_string(),
            status: Some("canceled".to_string()),
            ..Default::default()
        }
        .into_enhanced()
        .expect("转换成功");
        assert_eq!(cancelled.turn_status, TurnStatus::Cancelled);
    }

    #[test]
    fn long_text_is_truncated() {
        let enhanced = ExternalTurn {
            session_id: "s".to_string(),
            user_input: "字".repeat(MAX_TEXT_CHARS + 10),
            ..Default::default()
        }
        .into_enhanced()
        .expect("转换成功");
        assert_eq!(enhanced.user_input.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(enhanced.user_input.ends_with('…'));
    }
}
