//! System Prompt Sections
//!
//! core 只负责 Prompt 段落的顺序组装、会话运行上下文与摘要合并。
//! 产品身份、回复风格、多媒体能力说明等文案由各插件经 `PromptSectionProvider`
//! 注入（产品基础文案见 `tiangong-plugin-prompt`，能力说明见对应能力插件）。

use crate::session::{Message, MessageRole, Session};

/// 构建 system prompt 所需的动态配置数据快照
///
/// 由调用方从插件收集段落，传入 `session.rebuild_system_prompt()`。
pub struct SystemPromptConfig {
    /// Plugin 注入的段落（产品身份 / 通用规则 / 自定义指令外围 / 各能力插件说明等）。
    /// 按插件注册顺序追加；产品文案插件注册在最前，保证身份与规则排在 prompt 开头。
    pub plugin_sections: Vec<String>,
}

impl SystemPromptConfig {
    /// 注入 Plugin 提供的 prompt 段落
    pub fn from_plugin_sections(sections: Vec<String>) -> Self {
        Self {
            plugin_sections: sections,
        }
    }
}

/// 构建完整的 system prompt 消息
///
/// 组装顺序：插件段（产品身份 / 通用规则 / 自定义指令 / 各能力说明）
/// → 摘要段。
/// 环境信息（当前工作目录、允许文件操作目录）不进入 system prompt：
/// 属会话动态信息（cwd 可能迟落定或变更），改写已发送前缀会作废对话
/// 历史缓存，改由 fs 工具结果回显（模型经首次文件工具调用获知）。
/// 返回 `Message { role: System }`，由 `build_provider_messages()` 提取到 system prompt。
pub fn build_full_system_prompt(session: &Session, config: &SystemPromptConfig) -> Message {
    let mut parts = Vec::new();

    // 插件段（产品文案 + 各能力插件段落，按注册顺序）
    for section in &config.plugin_sections {
        let trimmed = section.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }

    // 摘要段
    parts.extend(collect_summary_part(session));

    if parts.is_empty() {
        tracing::warn!(
            "system prompt 组装为空：无插件段落且无摘要，请求将不带 system（检查插件加载状态）"
        );
    }

    assemble_system_message(parts)
}

/// 收集摘要段
///
/// 摘要仅在上下文压缩时变化（压缩本身即缓存断点，不构成额外的前缀不稳定），
/// 是 core 压缩职责的自然输出，故由 core 拼装。
fn collect_summary_part(session: &Session) -> Vec<String> {
    if let Some(summary) = session
        .context_summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        vec![format!("此前对话摘要：\n{summary}")]
    } else {
        Vec::new()
    }
}

/// 组装最终的 System Message
fn assemble_system_message(parts: Vec<String>) -> Message {
    Message::new(MessageRole::System, parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_full_system_prompt_with_empty_sections_still_valid() {
        // 未注入任何插件段落时，core 仍应构建合法的 system prompt。
        // 环境信息已退出 system prompt（经 fs 工具结果回显），空段落时
        // 内容允许为空，由插件段兜底非空。
        let session = Session::new("测试会话");
        let config = SystemPromptConfig::from_plugin_sections(Vec::new());
        let msg = build_full_system_prompt(&session, &config);
        assert_eq!(msg.role, MessageRole::System);
        let text = msg.content.first().unwrap().as_text().unwrap_or_default();
        // 标题不进入 system prompt（保持 KV cache 前缀稳定）
        assert!(!text.contains("测试会话"));
    }

    #[test]
    fn build_full_system_prompt_includes_plugin_sections_in_order() {
        let session = Session::new("测试");
        let config = SystemPromptConfig::from_plugin_sections(vec![
            "身份段".to_string(),
            "规则段".to_string(),
            "  ".to_string(), // 空白段应被过滤
        ]);
        let msg = build_full_system_prompt(&session, &config);
        let text = msg.content.first().unwrap().as_text().unwrap_or_default();
        let id_idx = text.find("身份段").unwrap();
        let rule_idx = text.find("规则段").unwrap();
        assert!(id_idx < rule_idx);
    }
}
