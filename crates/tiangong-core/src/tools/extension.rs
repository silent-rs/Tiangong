use std::future::Future;
use std::pin::Pin;

use crate::session::Session;
use crate::tools::result::ToolResult;
use tiangong_llm::tool::{ToolCall, ToolSpec};

/// 工具覆盖处理器。
///
/// 当 Agent 调用指定工具时，优先使用注册的处理器替代默认行为。
/// Plugin 通过此机制注入浏览器获取能力，替代硬编码的工具名拦截。
pub trait ToolOverrideHandler: Send + Sync + 'static {
    /// 处理工具调用。返回 None 表示不拦截，由默认逻辑处理。
    ///
    /// `session` 为当前对话的可变引用：插件可读取 `session.id` 用于按对话路由
    /// （如终端 PTY），也可在工具返回前同步提交必要的会话状态。`actor_id`
    /// 是当前工具调用方的稳定身份，不从 Session 的展示状态推断。
    /// 默认不拦截任何调用，不关心工具覆盖的插件无需覆写。
    fn handle(
        &self,
        _call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        Box::pin(async { None })
    }

    /// 工具结果交给模型时的抬头（首行），由处理该调用的插件提供。
    ///
    /// core 只负责把返回的文本原样放在结果首行，不生成、不解释抬头内容；
    /// 返回 None（默认）时结果不带抬头。
    fn result_header(&self, _call: &ToolCall, _ok: bool) -> Option<String> {
        None
    }
}

/// 插件工具的对外名称：`{插件id}__{工具名}`。
///
/// 插件 id 中不允许出现在函数名里的字符（如 `.`）替换为 `-`；替换过或拼接后
/// 超过 [`TOOL_NAME_MAX_LEN`] 时截断插件 id 并附短哈希，保证唯一且不超长。
/// 工具名已以 `{插件id}__` 开头（如 MCP 的 `mcp__{server}__{tool}`）时保持不变。
/// 工具名本身过长、无法容纳任何前缀时返回 None。
pub fn namespaced_tool_name(plugin_id: &str, tool_name: &str) -> Option<String> {
    if tool_name.starts_with(&format!("{plugin_id}__")) {
        return Some(tool_name.to_string());
    }
    let sanitized: String = plugin_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let plain = format!("{sanitized}__{tool_name}");
    if sanitized == plugin_id && plain.len() <= TOOL_NAME_MAX_LEN {
        return Some(plain);
    }
    let hash = short_hash(plugin_id);
    // `{截断id}-{hash}__{tool}`
    let budget = TOOL_NAME_MAX_LEN.checked_sub(tool_name.len() + 2 + 1 + hash.len())?;
    let head: String = sanitized.chars().take(budget).collect();
    if head.is_empty() {
        return None;
    }
    Some(format!("{head}-{hash}__{tool_name}"))
}

/// 插件 id 的稳定短哈希（FNV-1a，取 4 位十六进制）。
fn short_hash(text: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{:04x}", hash & 0xffff)
}

/// 工具名长度上限（主流模型接口的函数名限制为 64 个字符）。
pub const TOOL_NAME_MAX_LEN: usize = 64;

/// 把一个插件声明的工具全部改为 `{插件id}__{工具名}` 对外暴露，描述前标注
/// `[插件 <id>]`，模型从工具名即可知道归属。
///
/// 返回 (原名, 对外规格)，顺序同输入；同一插件重复声明只保留一份，
/// 名称过长无法加前缀的工具不暴露并记录告警。跨插件的去重由调用方负责
/// （插件 id 唯一，加前缀后正常不会冲突）。
pub fn namespace_tool_specs(plugin_id: &str, specs: Vec<ToolSpec>) -> Vec<(String, ToolSpec)> {
    let mut seen = std::collections::HashSet::new();
    let mut resolved = Vec::with_capacity(specs.len());
    for mut spec in specs {
        let original = spec.name.clone();
        let Some(exposed) = namespaced_tool_name(plugin_id, &original) else {
            tracing::warn!(tool = %original, plugin = %plugin_id, "工具名过长，无法加插件前缀，不暴露该工具");
            continue;
        };
        if !seen.insert(exposed.clone()) {
            continue;
        }
        spec.name = exposed;
        spec.description = format!("[插件 {plugin_id}] {}", spec.description);
        resolved.push((original, spec));
    }
    resolved
}

/// 把对外名称还原为插件声明的原名后再交给处理器。
pub fn call_with_name(call: &ToolCall, name: &str) -> ToolCall {
    ToolCall {
        id: call.id.clone(),
        name: name.to_string(),
        arguments: call.arguments.clone(),
    }
}

/// 以对外名称注册、以原名转发的处理器包装（插件工具加前缀后使用）。
pub struct RenamedToolHandler {
    inner: std::sync::Arc<dyn ToolOverrideHandler>,
    original: String,
}

impl RenamedToolHandler {
    pub fn new(inner: std::sync::Arc<dyn ToolOverrideHandler>, original: String) -> Self {
        Self { inner, original }
    }
}

impl ToolOverrideHandler for RenamedToolHandler {
    fn handle(
        &self,
        call: &ToolCall,
        session: &mut Session,
        actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        self.inner
            .handle(&call_with_name(call, &self.original), session, actor_id)
    }

    fn result_header(&self, call: &ToolCall, ok: bool) -> Option<String> {
        self.inner
            .result_header(&call_with_name(call, &self.original), ok)
    }
}

/// 工具规格提供者。
///
/// Plugin 通过此机制向 Agent 注入新的工具定义（ToolSpec）。
/// 注册后，新工具会与 core 内置工具合并，统一暴露给 LLM。
pub trait ToolSpecProvider: Send + Sync + 'static {
    /// 整理会话时保留读取错误，避免把暂时不可用当成删除全部工具。
    fn try_tool_specs(&self) -> Result<Vec<ToolSpec>, String> {
        Ok(self.tool_specs())
    }
    /// 返回该 plugin 暴露的所有工具规格。默认返回空，不暴露新工具的插件无需覆写。
    fn tool_specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
}

/// Prompt 规则提供者。
///
/// Plugin 通过此机制向 system prompt 注入规则段落（如终端交互引导、浏览器使用规范等）。
/// 段落会按 plugin 注册顺序追加到 system prompt 中。
/// 相同配置下必须保持稳定，不应包含当前时间、轮次、自动提取的记忆或成员状态。
/// 动态信息应经工具结果或追加消息进入上下文，避免改写已发送的请求前缀。
pub trait PromptSectionProvider: Send + Sync + 'static {
    fn try_prompt_sections(&self) -> Result<Vec<String>, String> {
        Ok(self.prompt_sections())
    }
    /// 返回该 plugin 暴露的所有 prompt 段落（每段会作为独立块拼接到 system prompt）。
    /// 默认返回空，不注入 prompt 的插件无需覆写。
    fn prompt_sections(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: "描述".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn 插件工具全部按插件前缀暴露并标注归属() {
        let resolved = namespace_tool_specs(
            "terminal",
            vec![spec("run_shell"), spec("run_command"), spec("run_shell")],
        );
        let names: Vec<(&str, &str)> = resolved
            .iter()
            .map(|(original, spec)| (original.as_str(), spec.name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("run_shell", "terminal__run_shell"),
                ("run_command", "terminal__run_command"),
            ],
            "同插件重复声明只保留一份"
        );
        assert_eq!(resolved[0].1.description, "[插件 terminal] 描述");
    }

    #[test]
    fn 已带本插件前缀的工具保持原名() {
        let resolved = namespace_tool_specs("mcp", vec![spec("mcp__probe__read")]);
        assert_eq!(resolved[0].1.name, "mcp__probe__read");
        assert_eq!(resolved[0].0, "mcp__probe__read");
    }

    #[test]
    fn 超长或含非法字符的插件id截断并附短哈希() {
        let long_id = "p".repeat(TOOL_NAME_MAX_LEN);
        let name = namespaced_tool_name(&long_id, "generate_image").unwrap();
        assert!(name.len() <= TOOL_NAME_MAX_LEN, "{name}");
        assert!(name.ends_with("__generate_image"));
        let other = namespaced_tool_name(&format!("{long_id}x"), "generate_image").unwrap();
        assert_ne!(name, other, "截断后靠哈希区分");
        assert_eq!(
            namespaced_tool_name(&long_id, "generate_image").unwrap(),
            name,
            "同一 id 名称稳定"
        );

        let dotted = namespaced_tool_name("com.acme", "tool").unwrap();
        assert!(
            dotted.starts_with("com-acme-") && dotted.ends_with("__tool"),
            "{dotted}"
        );
        assert!(
            dotted
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        assert_ne!(dotted, namespaced_tool_name("com-acme", "tool").unwrap());
    }

    #[test]
    fn 工具名本身过长时不暴露() {
        let resolved = namespace_tool_specs("a", vec![spec(&"t".repeat(TOOL_NAME_MAX_LEN))]);
        assert!(resolved.is_empty());
    }

    struct EchoHandler;
    impl ToolOverrideHandler for EchoHandler {
        fn handle(
            &self,
            call: &ToolCall,
            _session: &mut Session,
            _actor_id: &str,
        ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
            let name = call.name.clone();
            Box::pin(async move {
                Some(ToolResult {
                    ok: true,
                    summary: String::new(),
                    stdout: name,
                    stderr: String::new(),
                    exit_code: 0,
                    execution: None,
                })
            })
        }

        fn result_header(&self, call: &ToolCall, ok: bool) -> Option<String> {
            Some(format!("echo:{}:{ok}", call.name))
        }
    }

    #[tokio::test]
    async fn 加前缀的处理器以原名转发并给出原名抬头() {
        let handler = RenamedToolHandler::new(std::sync::Arc::new(EchoHandler), "tool".to_string());
        let call = ToolCall {
            id: "c1".to_string(),
            name: "echo__tool".to_string(),
            arguments: serde_json::json!({}),
        };
        let mut session = Session::new("renamed");
        let result = handler.handle(&call, &mut session, "main").await.unwrap();
        assert_eq!(result.stdout, "tool");
        assert_eq!(
            handler.result_header(&call, false).as_deref(),
            Some("echo:tool:false")
        );
    }
}
