//! 推送工具参数（三平台共用）。

use serde::Deserialize;

fn default_final() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub(crate) struct SendTextInput {
    /// `list_push_targets` 返回的目标编号。
    pub(crate) target_id: String,
    /// 要发送的文本内容。
    pub(crate) text: String,
    /// 本次任务稳定编号；同一目标下重复使用不会再次发送。
    pub(crate) idempotency_key: String,
    /// 是否为本轮最终答复（仅 QQ/微信的回复额度规划使用；缺省视为最终答复）。
    #[serde(default = "default_final")]
    pub(crate) is_final: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SendLocalMediaInput {
    /// `list_push_targets` 返回的目标编号。
    pub(crate) target_id: String,
    /// 当前会话工作目录或 `~/.tiangong/media` 内的本地文件路径。
    pub(crate) file_path: String,
    /// 本次任务稳定编号；同一目标下重复使用不会再次发送。
    pub(crate) idempotency_key: String,
    /// 是否属于本轮最终答复（仅 QQ/微信使用；缺省视为最终答复）。
    #[serde(default = "default_final")]
    pub(crate) is_final: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_final_defaults_to_true() {
        let input: SendTextInput = serde_json::from_value(
            serde_json::json!({"target_id":"t","text":"x","idempotency_key":"k"}),
        )
        .unwrap();
        assert!(input.is_final);
        let input: SendLocalMediaInput = serde_json::from_value(serde_json::json!({
            "target_id":"t","file_path":"a","idempotency_key":"k","is_final":false
        }))
        .unwrap();
        assert!(!input.is_final);
    }
}
