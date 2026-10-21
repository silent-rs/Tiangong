//! MCP server 模式（`--mcp`）：把 Subagent 总线经标准 MCP（stdio JSON-RPC）
//! 暴露给外部 agent 工具（Claude Code、Codex 等），复用同一套总线逻辑
//! 与存储——单一状态机，消息反馈与宿主内一致。

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::service::SubagentService;
use tiangong_plugin_sidecar::protocol::{PROTOCOL_VERSION, Request};

/// MCP 工具清单：(MCP 工具名, 描述, input_schema, 总线操作名)。
fn mcp_tools() -> Vec<(&'static str, &'static str, Value, &'static str)> {
    vec![
        (
            "list_agents",
            "查看全部持久 Subagent 成员（身份、后端、状态）。",
            json!({"type":"object","properties":{}}),
            "list_agents",
        ),
        (
            "send_agent_message",
            "向成员发送补充消息（追问/补充背景/纠正方向）；该发起方有进行中的工作时自动关联，不新建执行。",
            json!({"type":"object","properties":{"agent_id":{"type":"string","description":"成员 ID 或名称"},"content":{"type":"string","description":"消息内容"}},"required":["agent_id","content"]}),
            "send_agent_message",
        ),
        (
            "load_workspace_state",
            "读取成员的自维护工作区状态（按工作区列出 plan/context/task）——外部调度者查询成员工作进展用；写入仅成员自己的执行会话。",
            json!({"type":"object","properties":{"agent_id":{"type":"string"}},"required":["agent_id"]}),
            "ui_list_workspace_states",
        ),
        (
            "list_pending_work",
            "查看等待中的工作与协作关系（谁在为谁执行、谁在等结果，含 workspace 域）。",
            json!({"type":"object","properties":{"agent_id":{"type":"string","description":"可选：只看某成员"}}}),
            "list_pending_work",
        ),
    ]
}

fn rpc_result(id: Value, result: Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()
}

fn rpc_error(id: Value, code: i64, message: &str) -> String {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}).to_string()
}

/// MCP stdio 主循环（逐行 JSON-RPC；工具调用同步返回总线结果——外部
/// agent 工具获得快速消息反馈，回报与状态可继续经查询工具拉取）。
pub async fn run_mcp(
    service: std::sync::Arc<SubagentService>,
    workspace: String,
    first_line: Option<String>,
) -> anyhow::Result<()> {
    // MCP 调用方的固定身份：外部会话（回报仍投回其发起关系）。
    crate::service::init_mcp_context("mcp-external", &workspace);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut seq: u64 = 0;
    let mut pending_first = first_line;
    loop {
        let line = match pending_first.take() {
            Some(first) => first,
            None => match stdin.lock().lines().next() {
                Some(line) => line?,
                None => break,
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(_) => {
                let _ = writeln!(out, "{}", rpc_error(Value::Null, -32700, "解析错误"));
                out.flush()?;
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let reply = match method {
            "initialize" => {
                seq += 1;
                // 握手即视为该 workspace 已接入：登记后派活引导不再重复
                // 携带（去重以「真的握手过」为准，失败不影响握手返回）。
                crate::mcp_access::record_handshake(&workspace);
                rpc_result(
                    id,
                    json!({
                        "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2024-11-05"),
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "tiangong-subagent", "version": env!("CARGO_PKG_VERSION") }
                    }),
                )
            }
            "notifications/initialized" | "notifications/cancelled" => continue,
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(
                id,
                json!({ "tools": mcp_tools().into_iter().map(|(name, description, schema, _)| json!({
                    "name": name, "description": description, "inputSchema": schema
                })).collect::<Vec<_>>() }),
            ),
            "tools/call" => {
                seq += 1;
                let tool = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                let found = mcp_tools()
                    .into_iter()
                    .find(|(name, _, _, _)| *name == tool);
                let Some((_, _, _, operation)) = found else {
                    let _ = writeln!(
                        out,
                        "{}",
                        rpc_error(id.clone(), -32602, &format!("未知工具: {tool}"))
                    );
                    out.flush()?;
                    continue;
                };
                let request = Request {
                    protocol_version: PROTOCOL_VERSION.to_string(),
                    request_id: format!("mcp-{seq}"),
                    operation: operation.to_string(),
                    payload: arguments,
                };
                let result = service.dispatch_inner_public(&request).await;
                let text =
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
                // 业务失败如实映射为 MCP 错误（外部 Agent 可可靠判断后续动作）。
                let is_error = result.get("ok").and_then(serde_json::Value::as_bool) == Some(false);
                rpc_result(
                    id,
                    json!({ "content": [ { "type": "text", "text": text } ], "isError": is_error }),
                )
            }
            other => rpc_error(id, -32601, &format!("未知方法: {other}")),
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}
