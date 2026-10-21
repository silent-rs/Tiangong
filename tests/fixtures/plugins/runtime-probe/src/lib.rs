//! runtime-probe：tiangong-plugin-runtime 集成测试专用的最小 WASM 插件。
//!
//! 只实现运行时测试需要的、由参数驱动的行为，不含任何业务逻辑：
//! - 工具 `echo` 原样返回参数；`fail` 返回插件错误；`spin` 死循环（fuel 中断）；
//!   `sidecar` 经宿主 sidecar 转发 `{operation, payload}`。
//! - 生命周期钩子把收到的会话 JSON 转发给 sidecar（`hook.<名称>`），
//!   便于验证适配器组装的会话身份、本轮快照与锁释放。
//! - 设置页 `get` / `set` 读写 `/data/probe.txt`，验证页面桥接与数据目录挂载。
#![no_main]

wit_bindgen::generate!({
    path: "../../../../crates/plugin/tiangong-plugin-runtime/wit/tiangong/plugin.wit",
});

use exports::tiangong::plugin::plugin::{
    Guest, PluginDescriptor, PluginError, ToolCall, ToolResult, ToolSpec,
};
use exports::tiangong::plugin::plugin_ui::{
    Contribution, Guest as UiGuest, ResourceResponse, ViewMessageRequest, ViewMessageResponse,
    ViewResponse,
};

const DATA_FILE: &str = "/data/probe.txt";

fn err(message: impl Into<String>) -> PluginError {
    PluginError::Message(message.into())
}

fn ok_result(summary: String, stdout: String) -> ToolResult {
    ToolResult {
        ok: true,
        summary,
        stdout,
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    }
}

/// 生命周期钩子转发：sidecar 未配置时静默忽略（与真实插件的 best-effort 一致）。
fn forward_hook(name: &str, session_json: String) -> Result<(), PluginError> {
    let _ = tiangong::plugin::sidecar::invoke(&format!("hook.{name}"), &session_json);
    Ok(())
}

/// 从 JSON 文本中取顶层字符串字段（只需支持测试构造的简单参数，避免引入 serde）。
fn string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let rest = &json[json.find(&needle)? + needle.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

struct Probe;

impl Guest for Probe {
    fn describe() -> Result<PluginDescriptor, PluginError> {
        Ok(PluginDescriptor {
            id: "runtime-probe".into(),
            name: "Runtime Probe".into(),
            version: "1.0.0".into(),
        })
    }

    fn tool_specs() -> Result<Vec<ToolSpec>, PluginError> {
        let object = r#"{"type":"object"}"#.to_string();
        Ok(["echo", "fail", "spin", "sidecar"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: format!("probe {name}"),
                input_schema: object.clone(),
            })
            .collect())
    }

    fn prompt_sections() -> Result<Vec<String>, PluginError> {
        Ok(vec!["runtime-probe prompt".into()])
    }

    fn handle_tool(call: ToolCall) -> Result<ToolResult, PluginError> {
        match call.name.as_str() {
            "echo" => Ok(ok_result("echo".into(), call.arguments)),
            "fail" => Err(err(format!("probe failure: {}", call.name))),
            "spin" => {
                let mut counter: u64 = 0;
                loop {
                    counter = std::hint::black_box(counter.wrapping_add(1));
                }
            }
            "sidecar" => {
                let operation = string_field(&call.arguments, "operation")
                    .ok_or_else(|| err("缺少 operation"))?;
                let payload = string_field(&call.arguments, "payload").unwrap_or_default();
                match tiangong::plugin::sidecar::invoke(&operation, &payload) {
                    Ok(response) => Ok(ok_result("sidecar".into(), response)),
                    Err(error) => Ok(ToolResult {
                        ok: false,
                        summary: "sidecar unavailable".into(),
                        stdout: String::new(),
                        stderr: format!("{error:?}"),
                        exit_code: 1,
                        execution: None,
                    }),
                }
            }
            other => Err(err(format!("unknown tool: {other}"))),
        }
    }

    fn shutdown() -> Result<(), PluginError> {
        Ok(())
    }

    fn set_workspace(_workspace: Option<String>, _full_trust: bool) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_config_updated(_config_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_session_ready(session_json: String) -> Result<(), PluginError> {
        forward_hook("session_ready", session_json)
    }

    fn on_turn_started(session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        forward_hook("turn_started", session_json)
    }

    fn on_turn_finished(session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        forward_hook("turn_finished", session_json)
    }

    fn on_session_ended(session_json: String) -> Result<(), PluginError> {
        forward_hook("session_ended", session_json)
    }
}

impl UiGuest for Probe {
    fn contributions() -> Result<Vec<Contribution>, PluginError> {
        Ok(vec![Contribution {
            id: "probe-settings".into(),
            title: "Probe".into(),
            description: "runtime probe settings".into(),
            icon: "flask".into(),
            group: "plugins".into(),
            has_view: true,
        }])
    }

    fn open_view(contribution_id: String) -> Result<ViewResponse, PluginError> {
        if contribution_id != "probe-settings" {
            return Err(err(format!("unknown contribution: {contribution_id}")));
        }
        Ok(ViewResponse {
            html: "<html><body>runtime-probe</body></html>".into(),
        })
    }

    fn get_view_resource(path: String) -> Result<ResourceResponse, PluginError> {
        Err(err(format!("no resource: {path}")))
    }

    fn handle_view_message(
        request: ViewMessageRequest,
    ) -> Result<ViewMessageResponse, PluginError> {
        match request.method.as_str() {
            "get" => Ok(ViewMessageResponse {
                payload: std::fs::read_to_string(DATA_FILE).unwrap_or_default(),
            }),
            "set" => {
                std::fs::write(DATA_FILE, &request.payload)
                    .map_err(|e| err(format!("write probe data: {e}")))?;
                Ok(ViewMessageResponse {
                    payload: "true".into(),
                })
            }
            other => Err(err(format!("unknown view message: {other}"))),
        }
    }
}

export!(Probe);
