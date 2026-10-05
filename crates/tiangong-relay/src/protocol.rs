//! 中继与桌面端之间的帧协议（WebSocket 文本帧，JSON）。
//!
//! 中继只做转发：手机端的连接、消息与静态资源请求都原样交给桌面端裁决，
//! 鉴权、单设备绑定和命令白名单全部由桌面端负责。

use serde::{Deserialize, Serialize};

/// 中继 → 桌面端。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayToAgent {
    /// 桌面端接入成功。
    Welcome,
    /// 手机端建立了一条数据连接。
    ClientOpen { conn: String },
    /// 手机端发来一条文本消息（原样转发）。
    ClientMsg { conn: String, data: String },
    /// 手机端连接已断开。
    ClientClose { conn: String },
    /// 手机端请求一个静态资源（前端页面或会话媒体文件）。
    AssetReq {
        req: String,
        path: String,
        query: String,
    },
}

/// 桌面端 → 中继。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentToRelay {
    /// 发给某条手机端连接的文本消息。
    ToClient { conn: String, data: String },
    /// 关闭某条手机端连接（附带原因，手机端可展示）。
    CloseClient { conn: String, reason: String },
    /// 静态资源响应，`body` 为 base64。
    AssetRes {
        req: String,
        status: u16,
        mime: String,
        body: String,
        #[serde(default)]
        no_store: bool,
        /// 以 `Content-Security-Policy: sandbox` 返回（用户文件等不可信内容）。
        #[serde(default)]
        sandbox: bool,
    },
    /// 保活。
    Ping,
}

/// 手机端连接被拒绝/关闭时使用的 WebSocket 关闭码。
pub const CLOSE_CODE_REJECTED: u16 = 4000;
/// 桌面端不在线。
pub const CLOSE_CODE_AGENT_OFFLINE: u16 = 4001;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frame = RelayToAgent::AssetReq {
            req: "r1".into(),
            path: "assets/a.js".into(),
            query: String::new(),
        };
        let text = serde_json::to_string(&frame).unwrap();
        assert!(text.contains("\"type\":\"asset_req\""));
        assert_eq!(serde_json::from_str::<RelayToAgent>(&text).unwrap(), frame);

        let reply: AgentToRelay = serde_json::from_str(
            r#"{"type":"asset_res","req":"r1","status":200,"mime":"text/html","body":""}"#,
        )
        .unwrap();
        assert!(matches!(
            reply,
            AgentToRelay::AssetRes {
                no_store: false,
                sandbox: false,
                ..
            }
        ));
    }
}
