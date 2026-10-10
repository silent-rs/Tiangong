//! 插件输入桥接（`session.input.*`）的宿主实现。
//!
//! 运行时只负责权限与方法白名单（见 `tiangong_plugin_runtime::bridge`），
//! 本模块校验负载并经定向事件 `session_input_attachment` 交给当前输入框：
//!
//! - `addAttachment`：加入草稿附件（PNG data URL，或宿主媒体目录内的文件）；
//! - `sendText`：发送一段文本，可带媒体目录内的文件附件（如语音输入的录音），
//!   以及可选的渲染声明 `render{view, data}`——`plugin` 由宿主填为调用方，
//!   视图是否存在由前端判定（找不到 replace 贡献时按默认样式显示）；
//! - `insertText`：把文本写入草稿，不发送；
//! - `showOverlay` / `hideOverlay`：`session.input-overlay` 贡献申请接管 / 归还
//!   输入区，事件 `session_input_overlay` 由前端覆盖层宿主消费。
//!
//! 宿主不理解业务含义（录音、截图等），只校验来源安全与大小上限；附件按 MIME
//! 归为普通图片 / 音频 / 视频 / 文件，专属显示由插件经渲染声明自行接管。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

/// 输入附件事件名（前端输入框监听）。
pub const INPUT_EVENT: &str = "session_input_attachment";
/// 输入覆盖层显隐事件名（前端覆盖层宿主监听）。
pub const OVERLAY_EVENT: &str = "session_input_overlay";
/// 覆盖层提示文案长度上限（字符），防止插件撑破输入区底栏。
const MAX_OVERLAY_HINT_CHARS: usize = 60;
/// 输入覆盖层 Slot。
pub const OVERLAY_SLOT: &str = "session.input-overlay";
/// 消息条目 Slot（渲染声明的目标视图须为其 replace 贡献）。
pub const MESSAGE_ITEM_SLOT: &str = "session.message-item";

const MAX_TEXT_BYTES: usize = 10_000;
const MAX_IMAGE_BASE64_BYTES: u64 = 50 * 1024 * 1024;
const MAX_MEDIA_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// 桥接调用解析结果：要向前端推送的事件。
#[derive(Debug, Clone, PartialEq)]
pub struct InputEvent {
    pub name: &'static str,
    pub payload: Value,
}

#[derive(Deserialize)]
struct TextInput {
    text: String,
    #[serde(default)]
    attachments: Vec<AttachmentInput>,
    /// 插件渲染声明（仅 sendText）：消息以调用方的 replace 视图显示。
    #[serde(default)]
    render: Option<RenderInput>,
}

/// 插件提交的渲染声明：不含 `plugin`，由宿主按调用方填写。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderInput {
    view: String,
    #[serde(default)]
    data: Value,
}

#[derive(Deserialize)]
struct AttachmentInput {
    source: String,
    #[serde(default)]
    original_name: String,
    #[serde(default)]
    mime_type: String,
}

/// 解析并校验一次输入桥接调用。
///
/// `media_root` 为宿主媒体目录（`<storage>/media`），文件附件必须位于其中；
/// `overlay_owner` 判断调用方是否声明了 `session.input-overlay` 贡献。
pub fn handle(
    plugin_id: &str,
    method: &str,
    payload: &str,
    media_root: &Path,
    overlay_owner: impl Fn(&str) -> bool,
    replace_view_owner: impl Fn(&str, &str) -> bool,
) -> Result<InputEvent> {
    match method {
        "session.input.sendText" | "session.input.insertText" => {
            let input: TextInput = serde_json::from_str(payload).context("输入文本格式无效")?;
            if input.text.trim().is_empty() {
                bail!("输入文本不能为空");
            }
            if input.text.len() > MAX_TEXT_BYTES {
                bail!("输入文本超过 10KB 上限");
            }
            let insert = method == "session.input.insertText";
            if insert && !input.attachments.is_empty() {
                bail!("insertText 不支持附件，请改用 addAttachment");
            }
            if insert && input.render.is_some() {
                bail!("insertText 不支持渲染声明（草稿由用户编辑后发送）");
            }
            let render = input
                .render
                .map(|render| message_render(plugin_id, render))
                .transpose()?;
            let attachments = input
                .attachments
                .into_iter()
                .map(|attachment| media_file_attachment(attachment, media_root))
                .collect::<Result<Vec<_>>>()?;
            // 复用截图插件的输入事件通道：文本作为 kind="text" 的输入项分流处理。
            let mut item = json!({
                "kind": "text",
                "text": input.text,
                "mode": if insert { "insert" } else { "send" },
            });
            if !attachments.is_empty() {
                item["attachments"] = Value::Array(attachments);
            }
            if let Some(render) = render {
                item["render"] = serde_json::to_value(render)?;
            }
            Ok(input_event(plugin_id, item))
        }
        "session.input.addAttachment" => {
            let attachment: AttachmentInput =
                serde_json::from_str(payload).context("输入附件格式无效")?;
            let item = if attachment.source.starts_with("data:") {
                png_attachment(attachment)?
            } else {
                media_file_attachment(attachment, media_root)?
            };
            Ok(input_event(plugin_id, item))
        }
        "session.input.showOverlay" | "session.input.hideOverlay" => {
            if !overlay_owner(plugin_id) {
                bail!("插件 {plugin_id} 未声明 {OVERLAY_SLOT} 贡献，不能接管输入区");
            }
            #[derive(Deserialize, Default)]
            struct OverlayInput {
                #[serde(default)]
                session_id: Option<String>,
                /// 覆盖层显示期间替换输入区底部快捷键提示的文案（可选）。
                #[serde(default)]
                hint: Option<String>,
            }
            let input: OverlayInput = if payload.trim().is_empty() {
                OverlayInput::default()
            } else {
                serde_json::from_str(payload).context("覆盖层参数格式无效")?
            };
            Ok(InputEvent {
                name: OVERLAY_EVENT,
                payload: json!({
                    "plugin_id": plugin_id,
                    "visible": method == "session.input.showOverlay",
                    "session_id": input.session_id,
                    "hint": input
                        .hint
                        .map(|hint| hint.trim().chars().take(MAX_OVERLAY_HINT_CHARS).collect::<String>())
                        .filter(|hint| !hint.is_empty()),
                }),
            })
        }
        other => bail!("未知输入草稿方法 {other}"),
    }
}

/// 组装渲染声明：`plugin` 固定为调用方，整体按
/// [`tiangong_types::MessageRender::validate`] 校验非空与大小。视图是否存在
/// 不在此校验：前端找不到对应 replace 贡献时按默认样式显示。
fn message_render(plugin_id: &str, render: RenderInput) -> Result<tiangong_types::MessageRender> {
    let view = render.view.trim().to_string();
    let render = tiangong_types::MessageRender {
        plugin: plugin_id.to_string(),
        view,
        data: render.data,
    };
    render.validate().map_err(anyhow::Error::msg)?;
    Ok(render)
}

fn input_event(plugin_id: &str, attachment: Value) -> InputEvent {
    InputEvent {
        name: INPUT_EVENT,
        payload: json!({ "plugin_id": plugin_id, "attachment": attachment }),
    }
}

fn png_attachment(attachment: AttachmentInput) -> Result<Value> {
    if attachment.mime_type != "image/png"
        || !attachment.source.starts_with("data:image/png;base64,")
    {
        bail!("图片附件仅支持 PNG data URL");
    }
    let base64 = attachment
        .source
        .split_once(',')
        .map(|(_, value)| value)
        .unwrap_or_default();
    if base64.is_empty() || base64.len() as u64 > MAX_IMAGE_BASE64_BYTES {
        bail!("图片内容为空或超过 50MB 限制");
    }
    let title = if attachment.original_name.trim().is_empty() {
        "screenshot.png".to_string()
    } else {
        attachment.original_name
    };
    Ok(json!({
        "kind": "image",
        "source": attachment.source,
        "original_name": title,
        "mime_type": "image/png",
    }))
}

/// 文件附件：只接受宿主媒体目录内的已存在文件（插件产物落盘位置），
/// 防止插件借输入通道把任意本机文件带进会话。类别按 MIME 归为普通
/// 图片 / 音频 / 视频 / 文件，与用户上传的同类文件一致处理。
fn media_file_attachment(attachment: AttachmentInput, media_root: &Path) -> Result<Value> {
    let path = resolve_media_file(&attachment.source, media_root)?;
    let size = std::fs::metadata(&path)
        .with_context(|| format!("读取附件文件失败：{}", path.display()))?
        .len();
    if size == 0 || size > MAX_MEDIA_FILE_BYTES {
        bail!("附件文件为空或超过 50MB 限制");
    }
    let title = if attachment.original_name.trim().is_empty() {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string())
    } else {
        attachment.original_name
    };
    let mime_type = attachment.mime_type.trim();
    let kind = match mime_type.split('/').next().unwrap_or_default() {
        "image" => "image",
        "audio" => "audio",
        "video" => "video",
        _ => "file",
    };
    let mut item = json!({
        "kind": kind,
        "source": path.to_string_lossy(),
        "original_name": title,
    });
    if !mime_type.is_empty() {
        item["mime_type"] = Value::String(mime_type.to_string());
    }
    Ok(item)
}

fn resolve_media_file(source: &str, media_root: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(source);
    if !path.is_absolute() {
        bail!("附件路径必须为绝对路径");
    }
    let canonical = std::fs::canonicalize(&path)
        .with_context(|| format!("附件文件不存在：{}", path.display()))?;
    let root = std::fs::canonicalize(media_root).context("宿主媒体目录不存在")?;
    if !canonical.starts_with(&root) || !canonical.is_file() {
        bail!("附件文件必须位于宿主媒体目录内");
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        (dir, media)
    }

    fn call(method: &str, payload: Value, media: &Path) -> Result<InputEvent> {
        handle("voice", method, &payload.to_string(), media, |id| {
            id == "voice"
        })
    }

    #[test]
    fn send_text_render_is_bound_to_caller() {
        let (_dir, media) = media_root();
        let event = call(
            "session.input.sendText",
            json!({ "text": "你好", "render": { "view": " voice-card ", "data": { "k": 1 } } }),
            &media,
        )
        .unwrap();
        let render = &event.payload["attachment"]["render"];
        assert_eq!(render["plugin"], "voice");
        assert_eq!(render["view"], "voice-card");
        assert_eq!(render["data"]["k"], 1);

        // 冒用其他插件（plugin 字段不被接受）、insertText 携带渲染均拒绝；
        // 视图是否存在交给前端判定。
        assert!(call(
            "session.input.sendText",
            json!({ "text": "你好", "render": { "plugin": "bot", "view": "voice-card" } }),
            &media,
        )
        .is_err());
        let error = call(
            "session.input.insertText",
            json!({ "text": "草稿", "render": { "view": "voice-card" } }),
            &media,
        )
        .unwrap_err();
        assert!(error.to_string().contains("渲染声明"));

        // 超过上限的渲染数据被拒绝。
        let big = "x".repeat(tiangong_types::MESSAGE_RENDER_MAX_BYTES);
        let error = call(
            "session.input.sendText",
            json!({ "text": "你好", "render": { "view": "voice-card", "data": big } }),
            &media,
        )
        .unwrap_err();
        assert!(error.to_string().contains("render 过大"));

        // 不带渲染声明时行为不变。
        let event = call("session.input.sendText", json!({ "text": "你好" }), &media).unwrap();
        assert!(event.payload["attachment"].get("render").is_none());
    }

    #[test]
    fn send_text_with_media_audio_attachment() {
        let (_dir, media) = media_root();
        let audio = media.join("rec.wav");
        std::fs::write(&audio, b"RIFF").unwrap();
        let event = call(
            "session.input.sendText",
            json!({
                "text": "你好",
                "attachments": [{ "source": audio, "mime_type": "audio/wav" }],
            }),
            &media,
        )
        .unwrap();
        assert_eq!(event.name, INPUT_EVENT);
        let item = &event.payload["attachment"];
        assert_eq!(item["kind"], "text");
        assert_eq!(item["mode"], "send");
        assert_eq!(item["attachments"][0]["kind"], "audio");
        assert_eq!(item["attachments"][0]["original_name"], "rec.wav");
    }

    #[test]
    fn media_files_are_classified_by_mime_like_user_uploads() {
        let (_dir, media) = media_root();
        for (name, mime, kind) in [
            ("a.jpg", "image/jpeg", "image"),
            ("b.mp4", "video/mp4", "video"),
            ("c.pdf", "application/pdf", "file"),
            ("d.bin", "", "file"),
        ] {
            let path = media.join(name);
            std::fs::write(&path, b"data").unwrap();
            let event = call(
                "session.input.addAttachment",
                json!({ "source": path, "mime_type": mime }),
                &media,
            )
            .unwrap();
            let item = &event.payload["attachment"];
            assert_eq!(item["kind"], kind, "{name}");
            assert_eq!(item["original_name"], name);
            assert_eq!(item.get("mime_type").is_some(), !mime.is_empty());
        }
    }

    #[test]
    fn media_file_outside_media_root_is_rejected() {
        let (dir, media) = media_root();
        let outside = dir.path().join("secret.wav");
        std::fs::write(&outside, b"RIFF").unwrap();
        let error = call(
            "session.input.addAttachment",
            json!({ "source": outside, "mime_type": "audio/wav" }),
            &media,
        )
        .unwrap_err();
        assert!(error.to_string().contains("媒体目录"));
        let error = call(
            "session.input.addAttachment",
            json!({ "source": media.join("../secret.wav"), "mime_type": "audio/wav" }),
            &media,
        )
        .unwrap_err();
        assert!(error.to_string().contains("媒体目录"));
    }

    #[test]
    fn insert_text_marks_insert_mode_and_rejects_attachments() {
        let (_dir, media) = media_root();
        let event = call(
            "session.input.insertText",
            json!({ "text": "草稿" }),
            &media,
        )
        .unwrap();
        assert_eq!(event.payload["attachment"]["mode"], "insert");
        let error = call(
            "session.input.insertText",
            json!({ "text": "草稿", "attachments": [{ "source": "/x", "mime_type": "audio/wav" }] }),
            &media,
        )
        .unwrap_err();
        assert!(error.to_string().contains("insertText"));
        assert!(call("session.input.sendText", json!({ "text": "  " }), &media).is_err());
    }

    #[test]
    fn png_attachment_is_still_supported() {
        let (_dir, media) = media_root();
        let event = call(
            "session.input.addAttachment",
            json!({ "source": "data:image/png;base64,AAAA", "mime_type": "image/png" }),
            &media,
        )
        .unwrap();
        assert_eq!(event.payload["attachment"]["kind"], "image");
        assert_eq!(
            event.payload["attachment"]["original_name"],
            "screenshot.png"
        );
        assert!(call(
            "session.input.addAttachment",
            json!({ "source": "data:image/jpeg;base64,AAAA", "mime_type": "image/jpeg" }),
            &media,
        )
        .is_err());
    }

    #[test]
    fn overlay_requires_declared_contribution() {
        let (_dir, media) = media_root();
        let event = call(
            "session.input.showOverlay",
            json!({ "session_id": "s1" }),
            &media,
        )
        .unwrap();
        assert_eq!(event.name, OVERLAY_EVENT);
        assert_eq!(event.payload["visible"], true);
        assert_eq!(event.payload["session_id"], "s1");
        assert!(event.payload["hint"].is_null());
        let with_hint = call(
            "session.input.showOverlay",
            json!({ "session_id": "s1", "hint": "  按住空格说话  " }),
            &media,
        )
        .unwrap();
        assert_eq!(with_hint.payload["hint"], "按住空格说话");
        let long = call(
            "session.input.showOverlay",
            json!({ "hint": "长".repeat(200) }),
            &media,
        )
        .unwrap();
        assert_eq!(
            long.payload["hint"].as_str().unwrap().chars().count(),
            MAX_OVERLAY_HINT_CHARS
        );
        let hide = handle(
            "voice",
            "session.input.hideOverlay",
            "",
            &media,
            |_| true,
            |_, _| false,
        )
        .unwrap();
        assert_eq!(hide.payload["visible"], false);
        let error = handle(
            "other",
            "session.input.showOverlay",
            "{}",
            &media,
            |_| false,
            |_, _| false,
        )
        .unwrap_err();
        assert!(error.to_string().contains(OVERLAY_SLOT));
    }
}
