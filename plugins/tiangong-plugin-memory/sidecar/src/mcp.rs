//! `--mcp` 模式：以 stdio MCP Server 向第三方 Agent 提供记忆工具。
//!
//! MCP 只暴露模型需要主动决策的两个工具：`memory_recall`（回忆）与
//! `memory_remember`（记忆）。轮次上报、会话结束、列表、归档、状态等属于
//! 生命周期与管理操作，不占用模型的工具上下文，只通过 daemon 的 REST 接口
//! 提供（由 Agent 的 hook 或脚本调用）。
//!
//! [`tool_definitions`] 是全部操作的注册表（REST `/tools` 使用），
//! [`MCP_TOOLS`] 是其中暴露给 MCP 的子集。

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};

use crate::service::{MemoryService, ServiceError, ServiceResult, parse_input};

const INSTRUCTIONS: &str = "天工 Memory：跨会话的长期记忆。\
1) 用户提到之前、上次、继续、那个等历史指代，或任务依赖历史背景、项目约定时，先调用 memory_recall；\
2) 用户明确要求记住的偏好、约定或事实，或本次对话得出值得长期保留的结论时，调用 memory_remember。\
workspace 可传项目绝对路径，会按末级目录名归一，与天工同项目记忆共享。";

/// 暴露给 MCP 的工具（模型主动调用）。其余操作只走 REST。
pub const MCP_TOOLS: [&str; 2] = ["memory_recall", "memory_remember"];

/// MCP 工具定义。
pub fn mcp_tool_definitions() -> Vec<Tool> {
    tool_definitions()
        .into_iter()
        .filter(|tool| MCP_TOOLS.contains(&tool.name.as_ref()))
        .collect()
}

/// 全部操作定义（名称、描述、输入 Schema），REST `/tools` 与 MCP 共用。
pub fn tool_definitions() -> Vec<Tool> {
    vec![
        tool(
            "memory_recall",
            "回忆：按需找回跨会话的历史上下文（关键词 + 语义检索，配置了记忆 LLM 时还会规划查询并整理结果）。用户提到之前、上次、继续、那个等历史指代，或任务依赖历史背景、项目约定时调用。返回可直接阅读的整理结果与命中列表。",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "要回忆的内容，结合当前请求改写成可检索的描述"},
                    "reason": {"type": "string", "description": "为什么需要回忆"},
                    "expected": {"type": "array", "items": {"type": "string"}, "description": "期望找回的内容类型，如 decision、file、tool_result"},
                    "context": {"type": "array", "items": {"type": "string"}, "description": "最近几条对话摘要，帮助理解指代"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 10, "description": "最多返回条数，默认 5"}
                },
                "required": ["query"]
            }),
        ),
        tool(
            "memory_search",
            "快速检索记忆节点（不调用 LLM，关键词 + 语义混合），适合低延迟查找。",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "检索文本"},
                    "keywords": {"type": "array", "items": {"type": "string"}, "description": "额外关键词"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 30, "description": "默认 8"}
                },
                "required": ["query"]
            }),
        ),
        tool(
            "memory_remember",
            "记忆：立即写入一条长期记忆，用于用户明确要求记住的偏好、约定或事实，以及对话中得出的值得长期保留的结论。",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "简短标题"},
                    "summary": {"type": "string", "description": "记忆内容"},
                    "keywords": {"type": "array", "items": {"type": "string"}},
                    "memory_type": {
                        "type": "string",
                        "enum": ["factual", "user_preference", "user_habit", "skill", "project_structure", "architecture_decision", "problem_incident", "domain_knowledge"],
                        "description": "记忆类型，默认 factual"
                    },
                    "importance": {"type": "number", "minimum": 0, "maximum": 1, "description": "重要度 0~1"},
                    "workspace": {"type": "string", "description": "项目路径或名称；为空表示全局记忆"},
                    "session_id": {"type": "string"}
                },
                "required": ["title", "summary"]
            }),
        ),
        tool(
            "memory_record_turn",
            "每轮对话结束后上报本轮内容，记忆系统异步提炼事实、决策、产物等（立即返回，不阻塞对话）。",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "会话 ID，同一会话保持不变"},
                    "turn_id": {"type": "string", "description": "轮次 ID，缺省自动生成"},
                    "workspace": {"type": "string", "description": "项目路径或名称"},
                    "user_input": {"type": "string", "description": "用户本轮输入"},
                    "assistant_output": {"type": "string", "description": "助手最终回复"},
                    "tool_calls": {
                        "type": "array",
                        "description": "本轮关键工具调用",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "success": {"type": "boolean"},
                                "summary": {"type": "string", "description": "结果摘要"},
                                "path": {"type": "string"},
                                "url": {"type": "string"}
                            },
                            "required": ["name"]
                        }
                    },
                    "status": {"type": "string", "enum": ["completed", "cancelled", "failed"]}
                },
                "required": ["session_id", "user_input"]
            }),
        ),
        tool(
            "memory_end_session",
            "会话结束时调用，触发工作区级记忆整理与注入更新。",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "workspace": {"type": "string", "description": "项目路径或名称"}
                },
                "required": ["session_id", "workspace"]
            }),
        ),
        tool(
            "memory_list",
            "分页列出记忆节点，可按文本、工作区与状态过滤。",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "workspace": {"type": "string"},
                    "status": {"type": "string", "enum": ["active", "archived"]},
                    "offset": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "默认 20"}
                }
            }),
        ),
        tool(
            "memory_forget",
            "归档一条记忆（软删除），之后不再参与召回。",
            json!({
                "type": "object",
                "properties": {
                    "node_id": {"type": "string", "description": "memory_list / memory_search 返回的节点 ID"}
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "memory_status",
            "查看记忆系统配置与模型状态。",
            json!({"type": "object", "properties": {}}),
        ),
    ]
}

fn tool(name: &'static str, description: &'static str, schema: Value) -> Tool {
    let schema: JsonObject = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    Tool::new(name, description, Arc::new(schema))
}

/// 解析工具参数；失败时让 `call_tool` 直接返回输入错误。
macro_rules! try_input {
    ($arguments:expr) => {
        match parse_input($arguments) {
            Ok(input) => input,
            Err(error) => return Some(Err(error)),
        }
    };
}

/// 执行一次工具调用，返回 JSON 结果。未知工具返回 `None`。
pub async fn call_tool(
    service: &MemoryService,
    name: &str,
    arguments: Value,
) -> Option<ServiceResult<Value>> {
    let result = match name {
        "memory_recall" => to_value(service.recall(try_input!(arguments)).await),
        "memory_search" => to_value(service.search(try_input!(arguments)).await),
        "memory_remember" => to_value(service.remember(try_input!(arguments)).await),
        "memory_record_turn" => to_value(service.record_turn(try_input!(arguments)).await),
        "memory_end_session" => to_value(service.end_session(try_input!(arguments)).await),
        "memory_list" => to_value(service.list(try_input!(arguments)).await),
        "memory_forget" => to_value(service.forget(try_input!(arguments)).await),
        "memory_status" => service.status().await,
        _ => return None,
    };
    Some(result)
}

fn to_value<T: serde::Serialize>(result: ServiceResult<T>) -> ServiceResult<Value> {
    result.and_then(|value| {
        serde_json::to_value(value).map_err(|error| ServiceError::Internal(error.into()))
    })
}

/// MCP ServerHandler 实现。
#[derive(Clone)]
pub struct MemoryMcpServer {
    service: MemoryService,
}

impl MemoryMcpServer {
    pub fn new(service: MemoryService) -> Self {
        Self { service }
    }
}

impl ServerHandler for MemoryMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "tiangong-memory",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(mcp_tool_definitions()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        mcp_tool_definitions()
            .into_iter()
            .find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if !MCP_TOOLS.contains(&request.name.as_ref()) {
            return Err(McpError::invalid_params(
                format!("未知工具：{}", request.name),
                None,
            ));
        }
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let Some(result) = call_tool(&self.service, &request.name, arguments).await else {
            return Err(McpError::invalid_params(
                format!("未知工具：{}", request.name),
                None,
            ));
        };
        let result = match result {
            Ok(value) => {
                // 回忆结果以整理文本为主体，便于模型直接阅读；其余工具返回 JSON。
                let text = value
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|_| request.name == "memory_recall")
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string());
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                result.structured_content = Some(value);
                result
            }
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        Ok(result.into())
    }
}

/// 在 stdio 上运行 MCP Server，客户端断开即返回。
pub async fn serve_stdio(service: MemoryService) -> anyhow::Result<()> {
    let running = MemoryMcpServer::new(service)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| anyhow::anyhow!("MCP 初始化失败：{error}"))?;
    let reason = running.waiting().await?;
    tracing::info!(?reason, "MCP 客户端已断开");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_are_unique_objects() {
        let tools = tool_definitions();
        let mut names = tools
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), tools.len());
        for tool in &tools {
            assert_eq!(
                tool.input_schema.get("type").and_then(Value::as_str),
                Some("object"),
                "{} schema 必须是 object",
                tool.name
            );
            assert!(tool.description.as_deref().is_some_and(|d| !d.is_empty()));
        }
    }

    #[test]
    fn mcp_exposes_only_recall_and_remember() {
        let names = mcp_tool_definitions()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, ["memory_recall", "memory_remember"]);
        // MCP 子集必须都在完整注册表中（REST 与 MCP 同一实现）。
        let all = tool_definitions()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert!(MCP_TOOLS.iter().all(|name| all.iter().any(|n| n == name)));
    }
}
