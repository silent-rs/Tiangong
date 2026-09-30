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

/// 重名工具的对外名称：`{插件id}__{工具名}`。
pub fn namespaced_tool_name(plugin_id: &str, tool_name: &str) -> String {
    format!("{plugin_id}__{tool_name}")
}

/// 工具名长度上限（主流模型接口的函数名限制为 64 个字符）。
pub const TOOL_NAME_MAX_LEN: usize = 64;

/// 工具规格按名称消解重名：同名工具由多个来源声明时，全部改为
/// `{来源id}__{工具名}` 对外暴露（描述前标注来源），避免任何一方被静默丢弃。
///
/// `entries` 为 (来源id, 规格) 列表，按注册顺序排列；返回 (来源id, 原名, 对外规格)。
/// 加前缀后超长或仍然冲突的，保留先注册者并记录告警。
pub fn resolve_tool_name_conflicts(
    entries: Vec<(String, ToolSpec)>,
) -> Vec<(String, String, ToolSpec)> {
    use std::collections::{HashMap, HashSet};
    let mut owners: HashMap<&str, Vec<&str>> = HashMap::new();
    for (owner, spec) in &entries {
        let list = owners.entry(spec.name.as_str()).or_default();
        if !list.contains(&owner.as_str()) {
            list.push(owner.as_str());
        }
    }
    let conflicted: HashSet<String> = owners
        .iter()
        .filter(|(_, list)| list.len() > 1)
        .map(|(name, list)| {
            tracing::warn!(
                tool = %name,
                plugins = %list.join(","),
                "多个插件声明了同名工具，改为按插件前缀暴露"
            );
            (*name).to_string()
        })
        .collect();

    let mut seen = HashSet::new();
    let mut resolved = Vec::with_capacity(entries.len());
    for (owner, mut spec) in entries {
        let original = spec.name.clone();
        if conflicted.contains(&original) {
            let exposed = namespaced_tool_name(&owner, &original);
            if exposed.len() <= TOOL_NAME_MAX_LEN {
                spec.name = exposed;
                spec.description = format!("[插件 {owner}] {}", spec.description);
            } else {
                tracing::warn!(tool = %original, plugin = %owner, "加插件前缀后工具名超长，该插件按原名暴露");
            }
        }
        if seen.insert(spec.name.clone()) {
            resolved.push((owner, original, spec));
        } else {
            tracing::warn!(tool = %spec.name, plugin = %owner, "工具名仍然冲突，保留先注册者");
        }
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

/// 以对外名称注册、以原名转发的处理器包装（重名工具加前缀时使用）。
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
    fn 重名工具全部按插件前缀暴露_不重名保持原名() {
        let resolved = resolve_tool_name_conflicts(vec![
            ("generate-image-openai".to_string(), spec("generate_image")),
            ("terminal".to_string(), spec("run_shell")),
            ("volcengine".to_string(), spec("generate_image")),
            ("volcengine".to_string(), spec("generate_video")),
        ]);
        let names: Vec<(&str, &str, &str)> = resolved
            .iter()
            .map(|(owner, original, spec)| (owner.as_str(), original.as_str(), spec.name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                (
                    "generate-image-openai",
                    "generate_image",
                    "generate-image-openai__generate_image"
                ),
                ("terminal", "run_shell", "run_shell"),
                ("volcengine", "generate_image", "volcengine__generate_image"),
                ("volcengine", "generate_video", "generate_video"),
            ]
        );
        assert!(
            resolved[0]
                .2
                .description
                .starts_with("[插件 generate-image-openai]")
        );
        assert_eq!(resolved[1].2.description, "描述", "不重名的工具描述不变");
    }

    #[test]
    fn 同一插件重复声明只保留一份() {
        let resolved = resolve_tool_name_conflicts(vec![
            ("a".to_string(), spec("tool")),
            ("a".to_string(), spec("tool")),
        ]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].2.name, "tool");
    }

    #[test]
    fn 前缀超长时该方按原名暴露_两者仍可调用() {
        let long_id = "p".repeat(TOOL_NAME_MAX_LEN);
        let resolved = resolve_tool_name_conflicts(vec![
            ("short".to_string(), spec("tool")),
            (long_id, spec("tool")),
        ]);
        let names: Vec<&str> = resolved
            .iter()
            .map(|(_, _, spec)| spec.name.as_str())
            .collect();
        assert_eq!(names, vec!["short__tool", "tool"]);
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
