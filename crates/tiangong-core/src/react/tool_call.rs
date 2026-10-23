//! 单个工具调用的启动逻辑。

use std::future::Future;
use std::pin::Pin;

use crate::tools::result::ToolResult;
use crate::turn_context::TurnContext;
use tiangong_llm::tool::ToolCall;

/// 根据本轮工具覆盖表启动一个工具调用。
pub(super) fn start_tool_call(
    ctx: &mut TurnContext,
    call: &ToolCall,
    actor_id: &str,
) -> Pin<Box<dyn Future<Output = ToolResult> + Send>> {
    if let Some(handler) = ctx.tool_overrides.get(&call.name).cloned() {
        let handler_future = handler.handle(call, &mut ctx.session, actor_id);
        let tool_name = call.name.clone();
        return Box::pin(async move {
            if let Some(result) = handler_future.await {
                return result;
            }

            unregistered_tool_result(tool_name)
        });
    }

    let tool_name = call.name.clone();
    Box::pin(async move { unregistered_tool_result(tool_name) })
}

fn unregistered_tool_result(tool_name: String) -> ToolResult {
    ToolResult {
        ok: false,
        summary: format!("未注册的工具：{tool_name}（请确认对应插件已启用）"),
        stdout: String::new(),
        stderr: format!("tool {tool_name} not handled by any plugin"),
        exit_code: 1,
        execution: None,
    }
}
