//! Volcengine 插件的 WASM 桥接组件。
//!
//! 只做参数解析、sidecar 转发与设置页桥接；火山方舟请求与配置持久化都在 sidecar 内。

mod bindings;
mod sidecar_client;

use bindings::exports::tiangong::plugin::plugin::{
    Guest, PluginDescriptor, PluginError, ToolCall, ToolResult, ToolSpec,
};
use bindings::exports::tiangong::plugin::plugin_ui::{
    Contribution, Guest as UiGuest, ResourceResponse, ViewMessageRequest, ViewMessageResponse,
    ViewResponse,
};
use serde_json::Value;
use tiangong_plugin_volcengine_protocol::{
    Empty, GenerateImage, GenerateVideo, GetConfig, ImageRequest, ListModels, ListVoices, Play,
    PlayStatus, RecordCancel, RecordStart, RecordStop, SetConfig, Stop, Synthesize,
    SynthesizeRequest, TOOL_GENERATE_IMAGE, TOOL_GENERATE_VIDEO, TOOL_SPEECH_TO_TEXT,
    TOOL_TEXT_TO_SPEECH, Transcribe, TranscribeRequest, VideoRequest, VideoStatus,
    VolcengineOperation,
};

mod descriptor {
    pub const ID: &str = tiangong_plugin_volcengine_protocol::PLUGIN_ID;
    pub const NAME: &str = "Volcengine";
    pub const VERSION: &str = tiangong_plugin_volcengine_protocol::PLUGIN_VERSION;
}

fn plugin_err(message: impl Into<String>) -> PluginError {
    PluginError::Message(message.into())
}

struct Component;

impl Guest for Component {
    fn describe() -> Result<PluginDescriptor, PluginError> {
        Ok(PluginDescriptor {
            id: descriptor::ID.to_string(),
            name: descriptor::NAME.to_string(),
            version: descriptor::VERSION.to_string(),
        })
    }

    fn tool_specs() -> Result<Vec<ToolSpec>, PluginError> {
        Ok(vec![
            ToolSpec {
                name: TOOL_GENERATE_IMAGE.to_string(),
                description: "使用火山方舟 Agent Plan 的 Seedream 根据文字描述生成图片；传入 images 时基于参考图生成。\
                每次调用等待完成后返回图片路径。\
                注意：同一轮次中不要重复调用相同 prompt 的 generate_image，\
                拿到图片结果后应直接继续后续任务（如编写 HTML、组合排版等）。"
                    .to_string(),
                input_schema: r#"{"type":"object","properties":{"prompt":{"type":"string","description":"图片描述"},"size":{"type":"string","description":"尺寸（可选）：1K / 2K / 4K，或 宽x高 如 2048x2048"},"images":{"type":"array","items":{"type":"string"},"description":"参考图本地路径或 URL（可选），传入时为图生图"}},"required":["prompt"]}"#
                    .to_string(),
            },
            ToolSpec {
                name: TOOL_GENERATE_VIDEO.to_string(),
                description: "使用火山方舟 Agent Plan 的 Seedance 根据文字描述生成视频；传入 image 时以该图为首帧。\
                提交后等待生成完成并返回视频地址，超时则返回任务 ID。"
                    .to_string(),
                input_schema: r#"{"type":"object","properties":{"prompt":{"type":"string","description":"视频描述"},"duration":{"type":"integer","description":"视频时长，单位秒（可选）"},"resolution":{"type":"string","description":"分辨率（可选）：480p / 720p / 1080p"},"ratio":{"type":"string","description":"宽高比（可选）：16:9 / 9:16 / 1:1 等"},"image":{"type":"string","description":"首帧图本地路径或 URL（可选）"}},"required":["prompt"]}"#
                    .to_string(),
            },
            ToolSpec {
                name: TOOL_TEXT_TO_SPEECH.to_string(),
                description: "使用火山方舟 Agent Plan 的豆包语音合成将文本合成为语音音频文件（mp3），返回本地文件路径。"
                    .to_string(),
                input_schema: r#"{"type":"object","properties":{"text":{"type":"string","description":"待合成文本"},"voice":{"type":"string","description":"音色 ID（可选，如 zh_female_vv_uranus_bigtts；留空使用设置中的默认音色）"},"speed":{"type":"number","description":"语速倍率（可选，1.0 为正常，范围 0.5～2.0）"}},"required":["text"]}"#
                    .to_string(),
            },
            ToolSpec {
                name: TOOL_SPEECH_TO_TEXT.to_string(),
                description: "使用火山方舟 Agent Plan 的豆包流式语音识别将音频文件转录为文本".to_string(),
                input_schema: r#"{"type":"object","properties":{"file_path":{"type":"string","description":"音频文件路径（仅允许 ~/.tiangong/media 目录下的 wav / mp3 / ogg 文件）"},"language":{"type":"string","description":"语种（可选，如 zh-CN / en-US；留空自动识别）"}},"required":["file_path"]}"#
                    .to_string(),
            },
        ])
    }

    fn prompt_sections() -> Result<Vec<String>, PluginError> {
        Ok(Vec::new())
    }

    fn handle_tool(call: ToolCall) -> Result<ToolResult, PluginError> {
        let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
        match call.name.as_str() {
            TOOL_GENERATE_IMAGE => handle_generate_image(&args),
            TOOL_GENERATE_VIDEO => handle_generate_video(&args),
            TOOL_TEXT_TO_SPEECH => handle_text_to_speech(&args),
            TOOL_SPEECH_TO_TEXT => handle_speech_to_text(&args),
            other => Err(plugin_err(format!("未知的工具: {other}"))),
        }
    }

    fn shutdown() -> Result<(), PluginError> {
        Ok(())
    }

    fn set_workspace(_workspace: Option<String>, _full_trust: bool) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_config_updated(_config_json: String) -> Result<(), PluginError> {
        // 插件配置独立于全局模型配置，无需响应。
        Ok(())
    }

    fn on_session_ready(_session_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_turn_started(_session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_turn_finished(_session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_session_ended(_session_json: String) -> Result<(), PluginError> {
        Ok(())
    }
}

fn required_prompt(args: &Value) -> Result<String, PluginError> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if prompt.is_empty() {
        return Err(plugin_err("缺少必填参数 prompt"));
    }
    Ok(prompt)
}

fn optional_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn handle_generate_image(args: &Value) -> Result<ToolResult, PluginError> {
    let request = ImageRequest {
        prompt: required_prompt(args)?,
        size: optional_string(args, "size"),
        images: args
            .get("images")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    };
    let response = sidecar_client::invoke::<GenerateImage>(&request)
        .map_err(|error| plugin_err(format!("图片生成失败: {error}")))?;

    let markdown = response
        .images
        .iter()
        .enumerate()
        .map(|(index, image)| format!("![图片 {}]({})", index + 1, image.reference))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ToolResult {
        ok: true,
        summary: format!("图片生成成功（模型：{}）", response.model),
        stdout: markdown,
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

fn handle_generate_video(args: &Value) -> Result<ToolResult, PluginError> {
    let request = VideoRequest {
        prompt: required_prompt(args)?,
        duration: args
            .get("duration")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        resolution: optional_string(args, "resolution"),
        ratio: optional_string(args, "ratio"),
        image: optional_string(args, "image"),
    };
    let response = sidecar_client::invoke::<GenerateVideo>(&request)
        .map_err(|error| plugin_err(format!("视频生成失败: {error}")))?;

    let model = &response.model;
    let task_id = &response.task_id;
    Ok(match response.status {
        VideoStatus::Succeeded {
            video_url,
            duration,
        } => {
            let duration_line = duration
                .map(|seconds| format!("\nDuration: {seconds:.1}s"))
                .unwrap_or_default();
            ToolResult {
                ok: true,
                summary: format!("视频生成成功（模型：{model}）"),
                stdout: format!("Video URL: {video_url}{duration_line}"),
                stderr: String::new(),
                exit_code: 0,
                execution: None,
            }
        }
        VideoStatus::Running { status } => ToolResult {
            ok: true,
            summary: format!("视频生成任务仍在处理（模型：{model}）"),
            stdout: format!("Task ID: {task_id}\nStatus: {status}"),
            stderr: String::new(),
            exit_code: 0,
            execution: None,
        },
        VideoStatus::Failed { error } => ToolResult {
            ok: false,
            summary: format!("视频生成失败：{error}"),
            stdout: format!("Task ID: {task_id}"),
            stderr: error,
            exit_code: 1,
            execution: None,
        },
    })
}

fn handle_text_to_speech(args: &Value) -> Result<ToolResult, PluginError> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(plugin_err("缺少必填参数 text"));
    }
    let request = SynthesizeRequest {
        text,
        voice: optional_string(args, "voice"),
        speed: args.get("speed").and_then(Value::as_f64),
    };
    let response = sidecar_client::invoke::<Synthesize>(&request)
        .map_err(|error| plugin_err(format!("语音合成失败: {error}")))?;
    let duration = response
        .duration
        .map(|seconds| format!("，时长 {seconds:.1}s"))
        .unwrap_or_default();
    Ok(ToolResult {
        ok: true,
        summary: format!("语音合成成功（{}{duration}）", response.model),
        stdout: format!("音频文件已保存到：{}", response.file_path),
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

fn handle_speech_to_text(args: &Value) -> Result<ToolResult, PluginError> {
    let file_path =
        optional_string(args, "file_path").ok_or_else(|| plugin_err("缺少必填参数 file_path"))?;
    let request = TranscribeRequest {
        file_path,
        language: optional_string(args, "language"),
    };
    let response = sidecar_client::invoke::<Transcribe>(&request)
        .map_err(|error| plugin_err(format!("语音识别失败: {error}")))?;
    let language = response
        .language
        .as_deref()
        .map(|language| format!("，语言：{language}"))
        .unwrap_or_default();
    let duration = response
        .duration
        .map(|seconds| format!("，音频时长：{seconds:.1}s"))
        .unwrap_or_default();
    Ok(ToolResult {
        ok: true,
        summary: format!("语音识别成功（{}{language}{duration}）", response.model),
        stdout: response.text,
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

impl UiGuest for Component {
    /// 设置页由 plugin.json 的 `ui.contributions`（settings.plugin-page）声明：
    /// schema v2 插件的 WASM 贡献不会映射到设置页 Slot。
    fn contributions() -> Result<Vec<Contribution>, PluginError> {
        Ok(Vec::new())
    }

    fn open_view(_contribution_id: String) -> Result<ViewResponse, PluginError> {
        Err(plugin_err("火山引擎设置页由 plugin.json 声明"))
    }

    fn get_view_resource(_path: String) -> Result<ResourceResponse, PluginError> {
        Err(plugin_err("无外部资源"))
    }

    /// 设置页消息与前端 `bridge.call("plugin.<method>")` 共用此入口。
    ///
    /// 语音方法（synthesize / play / transcribe / record_* 等）与原
    /// text-to-speech、speech-to-text 插件保持同名同载荷。
    fn handle_view_message(
        request: ViewMessageRequest,
    ) -> Result<ViewMessageResponse, PluginError> {
        let payload = request.payload.as_str();
        let payload = match request.method.as_str() {
            "bootstrap" => invoke_for_ui::<GetConfig>(&Empty {})?,
            "save_config" => forward::<SetConfig>(payload)?,
            "synthesize" => forward::<Synthesize>(payload)?,
            "list_voices" => invoke_for_ui::<ListVoices>(&Empty {})?,
            "play" => forward::<Play>(payload)?,
            "play_status" => invoke_for_ui::<PlayStatus>(&Empty {})?,
            "stop" => invoke_for_ui::<Stop>(&Empty {})?,
            "transcribe" => forward::<Transcribe>(payload)?,
            "record_start" => forward::<RecordStart>(payload)?,
            "record_stop" => forward::<RecordStop>(payload)?,
            "record_cancel" => forward::<RecordCancel>(payload)?,
            "list_models" => forward::<ListModels>(payload)?,
            other => return Err(plugin_err(format!("未知的消息: {other}"))),
        };
        Ok(ViewMessageResponse { payload })
    }
}

/// 解析请求载荷后转发 sidecar。
fn forward<O>(payload: &str) -> Result<String, PluginError>
where
    O: VolcengineOperation,
    O::Request: serde::de::DeserializeOwned,
    O::Response: serde::Serialize,
{
    let request: O::Request = serde_json::from_str(payload)
        .map_err(|error| plugin_err(format!("解析 {} 请求失败: {error}", O::NAME)))?;
    invoke_for_ui::<O>(&request)
}

fn invoke_for_ui<O>(request: &O::Request) -> Result<String, PluginError>
where
    O: VolcengineOperation,
    O::Response: serde::Serialize,
{
    let response =
        sidecar_client::invoke::<O>(request).map_err(|error| plugin_err(error.to_string()))?;
    serde_json::to_string(&response)
        .map_err(|error| plugin_err(format!("序列化 {} 响应失败: {error}", O::NAME)))
}

bindings::export!(Component with_types_in bindings);
