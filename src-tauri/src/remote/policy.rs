//! 远程访问的命令边界：手机端只能使用对话侧能力。
//!
//! 所有远程 invoke 先经 [`decide`] 裁决：
//! - [`Decision::Allow`]：与桌面端完全相同的命令处理（经主 WebView IPC 分发）；
//! - [`Decision::Stub`]：桌面专属能力（系统通知、MCP 配置等）在远程以固定值应答；
//! - [`Decision::Deny`]：设置、插件管理、拓展区、文件系统读取等一律拒绝。

use serde_json::{json, Value};

/// 远程可用的插件挂载点（对话侧）。拓展区、侧栏、设置页、全局命令不在其列。
pub const REMOTE_SLOTS: &[&str] = &[
    "session.turn-node",
    "session.message-item",
    "session.message-action",
    "session.input-action",
    "session.before-input",
    "session.after-input",
    "session.input-status",
    "session.interaction",
    "session.input-overlay",
    "session.empty-state",
    "global.status-item",
];

/// 对话侧命令：与桌面端一致地处理。
const ALLOWED: &[&str] = &[
    "get_sessions",
    "get_session_meta",
    "switch_session",
    "load_session",
    "load_session_messages",
    "get_session_model",
    "list_session_chat_models",
    "set_session_model",
    "delete_session",
    "delete_sessions_by_cwd",
    "update_session_title",
    "send_message",
    "append_message",
    "edit_and_resend",
    "cancel_turn",
    "get_input_cache",
    "set_input_cache",
    "new_session_id",
    "remove_input_cache",
    "get_workspace_dir",
    "get_trust_mode",
    "set_trust_mode",
    "get_default_trust_mode",
    "get_reasoning_effort",
    "set_reasoning_effort",
    "get_mention_candidates",
    "get_mention_groups",
    "compress_context",
    "reset_context",
    "list_tool_icons",
    "plugin_read_tool_icon",
    "get_sandbox_disabled",
    "get_sandbox_update_state",
];

/// 会转发给手机端的宿主事件（对话侧）。
pub const FORWARDED_EVENTS: &[&str] = &[
    "stream_event",
    "session_meta_updated",
    "sessions_updated",
    "bridge_event",
    "session_input_attachment",
    "session_input_overlay",
    "plugins_changed",
    "models_config_changed",
];

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Stub(Value),
    Deny(&'static str),
}

const DENIED: &str = "远程模式不支持该操作";

/// 插件访问判定：`plugin_allowed(plugin)` 插件是否有对话侧贡献；
/// `contribution_allowed(plugin, contribution)` 指定贡献是否挂在对话侧挂载点。
pub trait PluginScope {
    fn plugin_allowed(&self, plugin_id: &str) -> bool;
    fn contribution_allowed(&self, plugin_id: &str, contribution_id: &str) -> bool;
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// 远程发送的附件只能是手机端上传的内容（data URL），不能引用桌面端本地路径。
fn attachments_are_uploads(args: &Value) -> bool {
    match args.get("attachments") {
        None | Some(Value::Null) => true,
        Some(Value::Array(items)) => items.iter().all(|item| {
            item.get("source")
                .and_then(Value::as_str)
                .is_some_and(|source| source.starts_with("data:"))
        }),
        Some(_) => false,
    }
}

pub fn decide(command: &str, args: &Value, scope: &dyn PluginScope) -> Decision {
    match command {
        "send_message" | "append_message" | "edit_and_resend" | "set_input_cache" => {
            if attachments_are_uploads(args)
                && args
                    .get("cache")
                    .map(attachments_are_uploads)
                    .unwrap_or(true)
            {
                Decision::Allow
            } else {
                Decision::Deny("远程模式只能发送从手机上传的附件")
            }
        }
        // 桌面专属能力：远程固定应答，前端流程与本地一致地继续。
        "request_desktop_notification_permission" | "send_desktop_notification" => {
            Decision::Stub(json!(false))
        }
        "get_mcp_servers" => Decision::Stub(json!([])),
        // 退订不下发：桌面端界面共用同一订阅表，避免手机端退订影响桌面端。
        "bridge_unsubscribe" => Decision::Stub(Value::Null),
        "list_slot_contributions" => {
            if REMOTE_SLOTS.contains(&str_arg(args, "slot")) {
                Decision::Allow
            } else {
                Decision::Stub(json!([]))
            }
        }
        "plugin_open_entry" | "plugin_read_entry_resource" => {
            if scope
                .contribution_allowed(str_arg(args, "pluginId"), str_arg(args, "contributionId"))
            {
                Decision::Allow
            } else {
                Decision::Deny(DENIED)
            }
        }
        "bridge_subscribe" => {
            if scope.plugin_allowed(str_arg(args, "pluginId")) {
                Decision::Allow
            } else {
                Decision::Deny(DENIED)
            }
        }
        "bridge_call" => {
            let method = str_arg(args, "method");
            // 浏览器控制与拓展区 App 打开属于拓展区能力，远程不开放。
            if method.starts_with("webview.") || method.starts_with("app.") {
                return Decision::Deny(DENIED);
            }
            if scope.plugin_allowed(str_arg(args, "pluginId")) {
                Decision::Allow
            } else {
                Decision::Deny(DENIED)
            }
        }
        _ if ALLOWED.contains(&command) => Decision::Allow,
        _ => Decision::Deny(DENIED),
    }
}

/// 宿主事件是否转发给手机端；插件桥接事件只转发对话侧插件。
pub fn event_forwardable(event: &str, payload: &str, scope: &dyn PluginScope) -> bool {
    if !FORWARDED_EVENTS.contains(&event) {
        return false;
    }
    if event != "bridge_event" {
        return true;
    }
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("plugin_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|plugin| scope.plugin_allowed(&plugin))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scope;
    impl PluginScope for Scope {
        fn plugin_allowed(&self, plugin_id: &str) -> bool {
            plugin_id == "interaction"
        }
        fn contribution_allowed(&self, plugin_id: &str, contribution_id: &str) -> bool {
            plugin_id == "interaction" && contribution_id == "interaction"
        }
    }

    #[test]
    fn settings_and_extension_commands_are_denied() {
        for command in [
            "get_models_config",
            "set_models_config",
            "install_plugin",
            "set_server_config",
            "start_server",
            "plugin_open_view",
            "list_extension_apps",
            "read_attachment_as_data_url",
            "set_session_cwd",
            "set_workspace_dir",
            "remote_get_config",
            "remote_create_pairing",
            "set_default_trust_mode",
        ] {
            assert!(
                matches!(decide(command, &json!({}), &Scope), Decision::Deny(_)),
                "{command} 应被拒绝"
            );
        }
    }

    #[test]
    fn conversation_commands_are_allowed() {
        for command in [
            "get_sessions",
            "load_session",
            "load_session_messages",
            "cancel_turn",
            "get_mention_groups",
        ] {
            assert_eq!(decide(command, &json!({}), &Scope), Decision::Allow);
        }
    }

    #[test]
    fn attachments_must_be_uploads() {
        let upload =
            json!({ "attachments": [{ "kind": "image", "source": "data:image/png;base64,AA" }] });
        assert_eq!(decide("send_message", &upload, &Scope), Decision::Allow);
        let local = json!({ "attachments": [{ "kind": "file", "source": "/etc/passwd" }] });
        assert!(matches!(
            decide("send_message", &local, &Scope),
            Decision::Deny(_)
        ));
        let cache = json!({ "cache": { "attachments": [{ "source": "/tmp/a" }] } });
        assert!(matches!(
            decide("set_input_cache", &cache, &Scope),
            Decision::Deny(_)
        ));
        assert_eq!(decide("send_message", &json!({}), &Scope), Decision::Allow);
    }

    #[test]
    fn plugin_access_is_limited_to_conversation_slots() {
        assert_eq!(
            decide(
                "list_slot_contributions",
                &json!({ "slot": "extension.tab" }),
                &Scope
            ),
            Decision::Stub(json!([]))
        );
        assert_eq!(
            decide(
                "list_slot_contributions",
                &json!({ "slot": "session.interaction" }),
                &Scope
            ),
            Decision::Allow
        );
        let entry = json!({ "pluginId": "interaction", "contributionId": "interaction" });
        assert_eq!(decide("plugin_open_entry", &entry, &Scope), Decision::Allow);
        let other = json!({ "pluginId": "browser", "contributionId": "browser" });
        assert!(matches!(
            decide("plugin_open_entry", &other, &Scope),
            Decision::Deny(_)
        ));

        let call = json!({ "pluginId": "interaction", "method": "tool.respond" });
        assert_eq!(decide("bridge_call", &call, &Scope), Decision::Allow);
        let webview = json!({ "pluginId": "interaction", "method": "webview.navigate" });
        assert!(matches!(
            decide("bridge_call", &webview, &Scope),
            Decision::Deny(_)
        ));
        let foreign = json!({ "pluginId": "terminal", "method": "sidecar.write" });
        assert!(matches!(
            decide("bridge_call", &foreign, &Scope),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn events_are_filtered() {
        assert!(event_forwardable("stream_event", "{}", &Scope));
        assert!(!event_forwardable("app:open_plugin", "{}", &Scope));
        assert!(!event_forwardable("browser:open", "{}", &Scope));
        assert!(event_forwardable(
            "bridge_event",
            r#"{"plugin_id":"interaction"}"#,
            &Scope
        ));
        assert!(!event_forwardable(
            "bridge_event",
            r#"{"plugin_id":"terminal"}"#,
            &Scope
        ));
    }
}
