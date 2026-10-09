#![allow(dead_code)]

use tiangong_types::{MentionGroup, MentionRequest, MentionTarget};

/// 补全候选项
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CompletionCandidate {
    /// 补全后的完整文本（替换触发词）
    pub value: String,
    /// 显示用的标签
    pub label: String,
    /// 简短描述
    pub hint: String,
}

/// 补全上下文类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionTrigger {
    /// `/` 命令补全
    SlashCommand,
    /// `@` 提及补全
    AtMention,
}

/// 斜杠命令定义
struct SlashCommandDef {
    name: &'static str,
    hint: &'static str,
}

const SLASH_COMMANDS: &[SlashCommandDef] = &[
    SlashCommandDef {
        name: "/exit",
        hint: "退出 REPL",
    },
    SlashCommandDef {
        name: "/quit",
        hint: "退出 REPL",
    },
    SlashCommandDef {
        name: "/new",
        hint: "新建会话",
    },
    SlashCommandDef {
        name: "/history",
        hint: "查看会话历史",
    },
    SlashCommandDef {
        name: "/sessions",
        hint: "会话管理（同 /history）",
    },
    SlashCommandDef {
        name: "/cancel",
        hint: "取消当前任务",
    },
    SlashCommandDef {
        name: "/config",
        hint: "打开网页配置页（模型 / 插件 / Prompt 等）",
    },
    SlashCommandDef {
        name: "/help",
        hint: "显示帮助信息",
    },
];

/// 检测输入中的补全���发点
///
/// `cursor` 为字符偏移（非字节偏移）。
/// 返回 (触发类型, 触发词起始位置（字符偏移）, 当��已输入的前缀)
pub fn detect_trigger(input: &str, cursor: usize) -> Option<(CompletionTrigger, usize, String)> {
    // 将字符偏移转为字节偏移，安全切片
    let byte_cursor = input
        .char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(input.len());
    let before_cursor = &input[..byte_cursor];

    // 从光标向左扫描，找到最近的触发字符
    // `@` 提及：光标前的 @word 片段
    if let Some(at_byte_pos) = before_cursor.rfind('@') {
        let prefix = &before_cursor[at_byte_pos..];
        // @ 必须在行首或前面是空���
        if at_byte_pos == 0 || before_cursor.as_bytes()[at_byte_pos - 1] == b' ' {
            // 确保 @ 后没有空格（在输入中间截断的提及）
            if !prefix[1..].contains(' ') {
                let at_char_pos = before_cursor[..at_byte_pos].chars().count();
                return Some((
                    CompletionTrigger::AtMention,
                    at_char_pos,
                    prefix[1..].to_string(),
                ));
            }
        }
    }

    // `/` 命令：仅在行首
    if before_cursor.starts_with('/') && !before_cursor.contains(' ') {
        return Some((
            CompletionTrigger::SlashCommand,
            0,
            before_cursor.to_string(),
        ));
    }

    None
}

/// 生成补全候选列表
///
/// `@` 提及与桌面端同一通道：由宿主注入的 `query_mentions`（CoreManager +
/// runtime 插件来源）返回候选，CLI 不再自行枚举文件/Skill/MCP。
pub fn complete(
    trigger: CompletionTrigger,
    prefix: &str,
    target: &MentionTarget,
    query_mentions: &dyn Fn(MentionRequest) -> Result<Vec<MentionGroup>, String>,
) -> Vec<CompletionCandidate> {
    match trigger {
        CompletionTrigger::SlashCommand => complete_slash_commands(prefix),
        CompletionTrigger::AtMention => complete_at_mentions(prefix, target, query_mentions),
    }
}

/// 单次 `@` 补全每组最多取的候选数（终端只展示前 10 条）。
const MENTION_MAX_PER_GROUP: usize = 20;

fn complete_slash_commands(prefix: &str) -> Vec<CompletionCandidate> {
    SLASH_COMMANDS
        .iter()
        .filter(|cmd| cmd.name.starts_with(prefix))
        .map(|cmd| CompletionCandidate {
            value: cmd.name.to_string(),
            label: cmd.name.to_string(),
            hint: cmd.hint.to_string(),
        })
        .collect()
}

fn complete_at_mentions(
    prefix: &str,
    target: &MentionTarget,
    query_mentions: &dyn Fn(MentionRequest) -> Result<Vec<MentionGroup>, String>,
) -> Vec<CompletionCandidate> {
    let request = mention_request(prefix, target);
    let empty_query = request.query.trim().is_empty() && request.allowed_kinds.is_empty();
    let groups = match query_mentions(request) {
        Ok(groups) => groups,
        Err(error) => {
            tracing::debug!(%error, "mention 候选查询失败");
            return Vec::new();
        }
    };
    groups
        .into_iter()
        // 与桌面端一致：未输入任何字符时不罗列文件（量大且无意义）。
        .filter(|group| !(empty_query && group.kind == "file"))
        .flat_map(|group| group.candidates)
        // value 为空的是状态占位（如「索引创建中…」），终端里不可选中，跳过。
        .filter(|candidate| !candidate.value.is_empty())
        .map(|candidate| CompletionCandidate {
            hint: match (candidate.label.is_empty(), candidate.hint.is_empty()) {
                (false, false) if candidate.label != candidate.hint => {
                    format!("{} - {}", candidate.label, candidate.hint)
                }
                (false, _) => candidate.label,
                (true, _) => candidate.hint,
            },
            label: candidate.value.clone(),
            value: candidate.value,
        })
        .collect()
}

/// 把 `@` 后的输入转成查询：`kind:rest` 限定分组并以 rest 作查询词。
fn mention_request(prefix: &str, target: &MentionTarget) -> MentionRequest {
    let (allowed_kinds, query) = match prefix.split_once(':') {
        Some((kind, rest))
            if !kind.is_empty()
                && kind
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            (vec![kind.to_string()], rest.to_string())
        }
        _ => (Vec::new(), prefix.to_string()),
    };
    MentionRequest {
        target: target.clone(),
        query,
        allowed_kinds,
        max_per_group: MENTION_MAX_PER_GROUP,
    }
}

/// 格式化帮助信息（显示所有命令和 @ 提及类型）
pub fn help_text() -> String {
    let mut lines = vec!["可用命令：".to_string()];
    for cmd in SLASH_COMMANDS {
        lines.push(format!("  {:<12} {}", cmd.name, cmd.hint));
    }
    lines.push(String::new());
    lines.push("@ 提及：".to_string());
    lines.push("  @<关键词>          搜索插件提供的提及（文件、Skill、MCP 等）".to_string());
    lines.push("  @<类型>:<关键词>   只搜索指定类型，如 @file:main、@skill:review".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use tiangong_types::MentionCandidate;

    use super::*;

    fn candidate(kind: &str, value: &str, label: &str, hint: &str) -> MentionCandidate {
        MentionCandidate {
            value: value.to_string(),
            label: label.to_string(),
            kind: kind.to_string(),
            hint: hint.to_string(),
            mark: String::new(),
        }
    }

    fn groups() -> Vec<MentionGroup> {
        vec![
            MentionGroup {
                kind: "file".into(),
                label: "file".into(),
                candidates: vec![
                    candidate("file", "@file:src/main.rs", "main.rs", "src/main.rs"),
                    candidate("file", "", "索引创建中…", "正在扫描工作区文件，请稍候"),
                ],
            },
            MentionGroup {
                kind: "skill".into(),
                label: "skill".into(),
                candidates: vec![candidate("skill", "@skill:review", "Review", "代码审查")],
            },
        ]
    }

    #[test]
    fn mention_request_splits_kind_prefix() {
        let target = MentionTarget::Draft {
            workspace: "/tmp/ws".into(),
        };
        let request = mention_request("file:main", &target);
        assert_eq!(request.allowed_kinds, vec!["file".to_string()]);
        assert_eq!(request.query, "main");
        assert_eq!(request.target, target);

        let request = mention_request("rev", &target);
        assert!(request.allowed_kinds.is_empty());
        assert_eq!(request.query, "rev");

        // 非法 kind（含空格/路径符号）不当作分组限定。
        let request = mention_request("a/b:c", &target);
        assert!(request.allowed_kinds.is_empty());
        assert_eq!(request.query, "a/b:c");
    }

    #[test]
    fn at_mention_uses_injected_source_and_hides_placeholders() {
        let seen = RefCell::new(None);
        let query = |request: MentionRequest| {
            *seen.borrow_mut() = Some(request);
            Ok(groups())
        };
        let result = complete(
            CompletionTrigger::AtMention,
            "ma",
            &MentionTarget::Global,
            &query,
        );
        assert_eq!(seen.borrow().as_ref().unwrap().query, "ma");
        let values: Vec<_> = result.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["@file:src/main.rs", "@skill:review"]);
        assert_eq!(result[0].hint, "main.rs - src/main.rs");
    }

    #[test]
    fn empty_at_mention_skips_file_group() {
        let query = |_: MentionRequest| Ok(groups());
        let result = complete(
            CompletionTrigger::AtMention,
            "",
            &MentionTarget::Global,
            &query,
        );
        let values: Vec<_> = result.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["@skill:review"]);
    }

    #[test]
    fn mention_source_error_yields_no_candidates() {
        let query = |_: MentionRequest| Err("boom".to_string());
        assert!(
            complete(
                CompletionTrigger::AtMention,
                "x",
                &MentionTarget::Global,
                &query
            )
            .is_empty()
        );
    }
}
