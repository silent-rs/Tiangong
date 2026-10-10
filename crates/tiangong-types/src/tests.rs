use crate::*;

#[test]
fn message_new() {
    let msg = Message::new(MessageRole::User, "你好");
    assert_eq!(msg.role, MessageRole::User);
    assert_eq!(msg.text_content(), "你好");
    assert!(!msg.id.is_empty());
    assert!(!msg.created_at.is_empty());
}

#[test]
fn message_with_reasoning() {
    let msg = Message::with_reasoning(MessageRole::Assistant, "回复", "思考过程");
    assert_eq!(msg.reasoning_content(), "思考过程");
}

#[test]
fn turn_status_serde() {
    // 序列化为小写形式，与 RunStatus/MessagePhase 风格一致。
    assert_eq!(
        serde_json::to_string(&TurnStatus::Cancelled).unwrap(),
        "\"cancelled\""
    );
    assert_eq!(
        serde_json::from_str::<TurnStatus>("\"failed\"").unwrap(),
        TurnStatus::Failed
    );
}

#[test]
fn message_turn_metadata_backward_compatible() {
    // 旧 session 的用户消息不包含 elapsed_ms / turn_status，反序列化应为 None。
    let legacy = r#"{
        "id": "u1",
        "role": "user",
        "content": "你好",
        "reasoning_content": "",
        "created_at": "2026-01-01 00:00:00"
    }"#;
    let msg: Message = serde_json::from_str(legacy).unwrap();
    assert_eq!(msg.role, MessageRole::User);
    assert_eq!(msg.elapsed_ms(), None);
    assert_eq!(msg.turn_status(), None);
}

#[test]
fn legacy_attachment_blocks_migrate_to_ready_content() {
    let legacy = r#"{
        "id":"legacy-message",
        "role":"user",
        "content":[
            {"type":"text","text":"查看资源"},
            {"type":"attachment","attachment":{
                "asset_id":"inline-1",
                "local_path":"/tmp/inline.png",
                "original_name":"inline.png",
                "mime_type":"image/png",
                "size":4,
                "kind":"image",
                "handling_mode":"inline_image",
                "capability":"chat_multimodal",
                "capability_available":true
            }},
            {"type":"attachment","attachment":{
                "asset_id":"resource-1",
                "local_path":"/tmp/resource.png",
                "original_name":"resource.png",
                "mime_type":"image/png",
                "size":8,
                "kind":"image",
                "handling_mode":"analyze_with_plugin",
                "capability":"analyze_attachment",
                "capability_available":true
            }}
        ],
        "created_at":"2026-01-01 00:00:00"
    }"#;

    let message: Message = serde_json::from_str(legacy).unwrap();

    assert!(matches!(
        &message.content[1],
        ContentBlock::Image { asset, data: None } if asset.asset_id == "inline-1"
    ));
    assert!(matches!(
        &message.content[2],
        ContentBlock::AssetReference { asset } if asset.asset_id == "resource-1"
    ));
    assert!(matches!(
        &message.content[3],
        ContentBlock::ModelInstruction { text }
            if text.contains("message_id=legacy-message")
                && text.contains("attachment_index=1")
                && text.contains("path=/tmp/resource.png")
    ));
    let migrated_json = serde_json::to_string(&message).unwrap();
    assert!(!migrated_json.contains("handling_mode"));
    assert!(!migrated_json.contains("analyze_attachment"));
}

#[test]
fn legacy_user_media_migrates_but_assistant_media_remains_display_only() {
    let user_json = r#"{
        "id":"legacy-user",
        "role":"user",
        "content":"处理文件",
        "media":[{"kind":"file","url":"/tmp/report.pdf","title":"report.pdf"}],
        "created_at":"2026-01-01 00:00:00"
    }"#;
    let user: Message = serde_json::from_str(user_json).unwrap();
    assert!(matches!(
        &user.content[1],
        ContentBlock::AssetReference { asset } if asset.local_path == "/tmp/report.pdf"
    ));
    assert!(matches!(
        &user.content[2],
        ContentBlock::ModelInstruction { text } if text.contains("path=/tmp/report.pdf")
    ));

    let assistant_json = r#"{
        "id":"legacy-assistant",
        "role":"assistant",
        "content":"结果",
        "media":[{"kind":"image","url":"/tmp/result.png"}],
        "created_at":"2026-01-01 00:00:00"
    }"#;
    let assistant: Message = serde_json::from_str(assistant_json).unwrap();
    assert!(matches!(
        &assistant.content[1],
        ContentBlock::Media { url, .. } if url == "/tmp/result.png"
    ));
}

#[test]
fn legacy_user_local_image_remains_a_sendable_image() {
    let json = r#"{
        "id":"legacy-image",
        "role":"user",
        "content":"查看图片",
        "media":[{"kind":"image","url":"/tmp/history.png","title":"history.png"}],
        "created_at":"2026-01-01 00:00:00"
    }"#;

    let message: Message = serde_json::from_str(json).unwrap();
    assert!(matches!(
        &message.content[1],
        ContentBlock::Image { asset, data: None }
            if asset.local_path == "/tmp/history.png" && asset.mime_type == "image/png"
    ));
}

#[test]
fn legacy_user_data_url_is_redacted_during_migration() {
    let legacy_payload = "VERY_LARGE_LEGACY_BASE64_PAYLOAD";
    let json = format!(
        r#"{{
            "id":"legacy-data-url",
            "role":"user",
            "content":[
                {{"type":"text","text":"处理旧图片"}},
                {{"type":"media","kind":"image","url":"data:image/png;base64,{legacy_payload}"}},
                {{"type":"attachment","attachment":{{
                    "asset_id":"data:image/png;base64,{legacy_payload}",
                    "local_path":"data:image/png;base64,{legacy_payload}",
                    "original_name":"legacy.png",
                    "mime_type":"image/png",
                    "size":32,
                    "kind":"image",
                    "handling_mode":"inline_image"
                }}}}
            ],
            "created_at":"2026-01-01 00:00:00"
        }}"#
    );

    let message: Message = serde_json::from_str(&json).unwrap();
    let migrated = serde_json::to_string(&message).unwrap();

    assert!(!migrated.contains(legacy_payload));
    assert!(migrated.contains("legacy-inline-data-unavailable"));
    assert!(migrated.contains("重新上传"));
    assert_eq!(message.extract_stored_assets().len(), 2);
    assert!(matches!(
        &message.content[1],
        ContentBlock::AssetReference { asset }
            if asset.asset_id.starts_with("legacy-")
                && !asset.asset_id.contains(legacy_payload)
    ));
}

#[test]
fn new_content_blocks_redact_case_insensitive_inline_data_references_on_load() {
    let secret = "SECRET_CASE_INSENSITIVE_BASE64";
    let user_json = format!(
        r#"{{
            "id":"new-image-data-path",
            "role":"user",
            "content":[{{
                "type":"image",
                "asset":{{
                    "asset_id":"asset-1",
                    "local_path":"DATA:image/png;base64,{secret}",
                    "original_name":"image.png",
                    "mime_type":"image/png",
                    "size":4,
                    "kind":"image"
                }}
            }}],
            "created_at":"2026-01-01 00:00:00"
        }}"#
    );
    let assistant_json = format!(
        r#"{{
            "id":"assistant-data-media",
            "role":"assistant",
            "content":[{{"type":"media","kind":"image","url":"DaTa:image/png;base64,{secret}"}}],
            "created_at":"2026-01-01 00:00:00"
        }}"#
    );

    for json in [user_json, assistant_json] {
        let message: Message = serde_json::from_str(&json).unwrap();
        let stable_json = serde_json::to_string(&message).unwrap();
        assert!(!stable_json.contains(secret));
        assert!(stable_json.contains("inline-data-reference-unavailable"));
    }
}

#[test]
fn session_new() {
    let session = Session::new("测试");
    assert_eq!(session.title, "测试");
    assert!(session.messages.is_empty());
}

#[test]
fn session_append() {
    let mut session = Session::new("测试");
    session.append_message(MessageRole::User, "你好");
    session.append_message_with_reasoning(MessageRole::Assistant, "回复", "思考");
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[1].reasoning_content(), "思考");
}

#[test]
fn token_usage_accumulate() {
    let mut a = TokenUsage {
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
        prompt_cache_hit_tokens: None,
        prompt_cache_miss_tokens: None,
    };
    let b = TokenUsage {
        prompt_tokens: 200,
        completion_tokens: 100,
        total_tokens: 300,
        prompt_cache_hit_tokens: None,
        prompt_cache_miss_tokens: None,
    };
    a.accumulate(&b);
    assert_eq!(a.total_tokens, 450);
}

#[test]
fn token_usage_accumulate_cache_fields() {
    // 双方都有 cache 值 → 相加
    let mut a = TokenUsage {
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
        prompt_cache_hit_tokens: Some(80),
        prompt_cache_miss_tokens: Some(20),
    };
    let b = TokenUsage {
        prompt_tokens: 200,
        completion_tokens: 100,
        total_tokens: 300,
        prompt_cache_hit_tokens: Some(60),
        prompt_cache_miss_tokens: Some(40),
    };
    a.accumulate(&b);
    assert_eq!(a.prompt_cache_hit_tokens, Some(140));
    assert_eq!(a.prompt_cache_miss_tokens, Some(60));

    // 自身为 None、对方为 Some → 取对方值（修复前的 bug：会被丢弃）
    let mut c = TokenUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        prompt_cache_hit_tokens: None,
        prompt_cache_miss_tokens: None,
    };
    let d = TokenUsage {
        prompt_tokens: 100,
        completion_tokens: 0,
        total_tokens: 100,
        prompt_cache_hit_tokens: Some(90),
        prompt_cache_miss_tokens: Some(10),
    };
    c.accumulate(&d);
    assert_eq!(c.prompt_cache_hit_tokens, Some(90), "None+Some 应取对方值");
    assert_eq!(c.prompt_cache_miss_tokens, Some(10), "None+Some 应取对方值");

    // 双方都为 None → 仍为 None
    let mut e = TokenUsage::default();
    let f = TokenUsage::default();
    e.accumulate(&f);
    assert_eq!(e.prompt_cache_hit_tokens, None);
    assert_eq!(e.prompt_cache_miss_tokens, None);
}

#[test]
fn run_status_serde() {
    let json = serde_json::to_string(&RunStatus::Executing).unwrap();
    assert_eq!(json, r#""executing""#);
    let parsed: RunStatus = serde_json::from_str(r#""idle""#).unwrap();
    assert_eq!(parsed, RunStatus::Idle);
}

#[test]
fn stream_event_serde() {
    let event = StreamEvent::Delta {
        message_id: "msg-1".into(),
        content: "你好".into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"delta""#));
    assert!(json.contains(r#""content":"你好""#));
    assert!(json.contains(r#""message_id":"msg-1""#));

    let done = StreamEvent::Done { usage: None };
    let json = serde_json::to_string(&done).unwrap();
    assert_eq!(json, r#"{"type":"done"}"#);

    let tool = StreamEvent::ToolCalls {
        message_id: "msg-2".into(),
        names: vec!["read_file".into(), "list_dir".into()],
        calls: Vec::new(),
        usage: None,
    };
    let json = serde_json::to_string(&tool).unwrap();
    assert!(json.contains(r#""type":"tool_calls""#));
    assert!(json.contains("read_file"));

    let elapsed = StreamEvent::TurnElapsed { seconds: 3 };
    let json = serde_json::to_string(&elapsed).unwrap();
    assert_eq!(json, r#"{"type":"turn_elapsed","seconds":3}"#);
}

#[test]
fn user_message_event_preserves_content_blocks_without_serializing_image_data() {
    let asset = StoredAsset {
        asset_id: "asset-1".into(),
        local_path: "/tmp/image.png".into(),
        original_name: "image.png".into(),
        mime_type: "image/png".into(),
        size: 4,
        kind: MediaKind::Image,
    };
    let event = StreamEvent::UserMessage {
        message_id: "msg-resource".into(),
        content: "查看资源".into(),
        content_blocks: vec![ContentBlock::Image {
            asset,
            data: Some("SECRET_BASE64".into()),
        }],
        media: Vec::new(),
        render: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("content_blocks"));
    assert!(json.contains("/tmp/image.png"));
    assert!(!json.contains("SECRET_BASE64"));

    let legacy = r#"{
        "type":"user_message",
        "message_id":"legacy-message",
        "content":"legacy",
        "media":[{"kind":"image","url":"/tmp/legacy.png"}]
    }"#;
    let parsed: StreamEvent = serde_json::from_str(legacy).unwrap();
    match parsed {
        StreamEvent::UserMessage {
            content_blocks,
            media,
            ..
        } => {
            assert!(content_blocks.is_empty());
            assert_eq!(media.len(), 1);
        }
        _ => panic!("应反序列化为 UserMessage"),
    }
}

#[test]
fn stream_event_phase_variants_serde() {
    // ReAct 阶段过程性文本
    let react = StreamEvent::ReactText {
        message_id: "m1".into(),
        content: "正在处理".into(),
    };
    let json = serde_json::to_string(&react).unwrap();
    assert!(
        json.contains(r#""type":"react_text""#),
        "react_text 标签错误: {json}"
    );
    let parsed: StreamEvent = serde_json::from_str(&json).unwrap();
    assert!(matches!(parsed, StreamEvent::ReactText { .. }));

    // 总结阶段最终回复
    let summary = StreamEvent::SummaryText {
        message_id: "m2".into(),
        content: "已完成".into(),
    };
    let json = serde_json::to_string(&summary).unwrap();
    assert!(
        json.contains(r#""type":"summary_text""#),
        "summary_text 标签错误: {json}"
    );

    // 阶段切换通知
    let phase = StreamEvent::PhaseChanged {
        phase: "summary".into(),
        iteration: 1,
    };
    let json = serde_json::to_string(&phase).unwrap();
    assert_eq!(
        json,
        r#"{"type":"phase_changed","phase":"summary","iteration":1}"#
    );
}

#[test]
fn message_role_serde() {
    let json = serde_json::to_string(&MessageRole::Assistant).unwrap();
    assert_eq!(json, r#""assistant""#);
}

#[test]
fn user_source_serde_and_forward_compat() {
    assert_eq!(
        serde_json::to_string(&UserSource::HostInjected).unwrap(),
        r#""host_injected""#
    );
    assert_eq!(
        serde_json::from_str::<UserSource>(r#""compressed_resume""#).unwrap(),
        UserSource::CompressedResume
    );
    // 前向兼容：未知来源降级为 Human，不得导致会话打不开。
    assert_eq!(
        serde_json::from_str::<UserSource>(r#""some_future_source""#).unwrap(),
        UserSource::Human
    );
    assert!(UserSource::Human.is_user_input());
    assert!(!UserSource::HostInjected.is_user_input());
    assert!(!UserSource::CompressedResume.is_user_input());
}

#[test]
fn new_format_roundtrip_keeps_role_fields_and_meta() {
    let mut assistant = Message::with_reasoning(MessageRole::Assistant, "回复", "思考");
    if let Role::Assistant {
        tool_calls,
        reasoning_signature,
        text_elapsed_ms,
        ..
    } = &mut assistant.role
    {
        tool_calls.push(MessageToolCall {
            id: "call-1".into(),
            name: "fs__read_file".into(),
            arguments: serde_json::json!({"path": "a"}),
        });
        *reasoning_signature = Some("sig".into());
        *text_elapsed_ms = Some(12);
    }
    let assistant = assistant.with_render(Some(MessageRender {
        plugin: "bot".into(),
        view: "card".into(),
        data: serde_json::json!({"channel": "微信私聊"}),
    }));
    let tool = Message::tool_result("call-1", "fs__read_file", "内容", true).with_duration_ms(5);
    let mut user = Message::user_prepared("u1", vec![ContentBlock::text("你好")]);
    user.set_turn_result(100, TurnStatus::Success);
    user.set_final_reply(Some(assistant.id.clone()));

    for original in [user, assistant, tool] {
        let json = serde_json::to_value(&original).unwrap();
        assert!(json["role"].is_object(), "新格式 role 为对象：{json}");
        assert!(json.get("phase").is_none());
        let parsed: Message = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }
}

#[test]
fn new_format_json_shape() {
    let tool = Message::tool_result("call-1", "fs__read_file", "内容", false).with_duration_ms(5);
    let json = serde_json::to_value(&tool).unwrap();
    assert_eq!(
        json["role"],
        serde_json::json!({"type": "tool", "tool_call_id": "call-1", "tool_name": "fs__read_file", "duration_ms": 5})
    );
    // 空 meta 不落盘。
    assert!(json.get("meta").is_none());
}

#[test]
fn legacy_flat_messages_migrate_into_roles() {
    let legacy = serde_json::json!([
        {"id": "u1", "role": "user", "content": "问题", "created_at": "t",
         "turn_status": "success", "elapsed_ms": 30},
        {"id": "a1", "role": "assistant", "content": "", "created_at": "t", "phase": "react",
         "reasoning_content": "想", "reasoning_signature": "sig",
         "tool_calls": [{"id": "c1", "name": "tool", "arguments": {}}],
         "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3,
                   "model": "m", "agent_id": "s", "turn_id": "u1", "source": "react", "status": "success"},
         "reasoning_elapsed_ms": 7},
        {"id": "t1", "role": "tool", "content": "结果", "created_at": "t", "phase": "react",
         "tool_call_id": "c1", "tool_name": "tool", "tool_result_is_error": true, "duration_ms": 9},
        {"id": "i1", "role": "user", "content": "", "created_at": "t", "phase": "hostinjected"},
        {"id": "a2", "role": "assistant", "content": "最终答复", "created_at": "t", "phase": "summary"},
        {"id": "n1", "role": "notice", "content": "[调用用量] x", "created_at": "t", "compact": true,
         "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3,
                   "model": "m", "agent_id": "s", "turn_id": null, "source": "x", "status": "success"}},
        {"id": "r1", "role": "user", "content": "恢复", "created_at": "t", "phase": "compressedresume"},
        {"id": "w1", "role": "user", "content": "worker 输入", "created_at": "t", "worker_id": "agent:dev:1"}
    ]);
    let session: Session = serde_json::from_value(serde_json::json!({
        "id": "s", "title": "t", "messages": legacy, "created_at": "t", "updated_at": "t"
    }))
    .unwrap();
    let m = &session.messages;

    assert_eq!(m[0].turn_status(), Some(TurnStatus::Success));
    assert_eq!(m[0].elapsed_ms(), Some(30));
    assert!(m[0].is_user_input());
    // summary 相位的助手消息迁移为起轮用户消息的 final_reply。
    assert_eq!(m[0].final_reply(), Some("a2"));

    assert_eq!(m[1].reasoning_content(), "想");
    assert_eq!(m[1].reasoning_signature(), Some("sig"));
    assert_eq!(m[1].tool_calls().len(), 1);
    assert_eq!(m[1].usage().map(|usage| usage.tokens.total_tokens), Some(3));
    assert_eq!(m[1].reasoning_elapsed_ms(), Some(7));

    assert_eq!(m[2].tool_call_id(), Some("c1"));
    assert_eq!(m[2].tool_name(), Some("tool"));
    assert!(m[2].tool_is_error());
    assert_eq!(m[2].duration_ms(), Some(9));

    assert_eq!(m[3].user_source(), Some(UserSource::HostInjected));
    assert!(!m[3].is_user_input());

    assert_eq!(m[5].kind(), MessageRole::Notice);
    assert!(m[5].meta.compact);
    assert!(m[5].usage().is_some());

    assert_eq!(m[6].user_source(), Some(UserSource::CompressedResume));
    // 旧 worker 输入不再单独区分来源，按普通用户消息读取。
    assert_eq!(m[7].user_source(), Some(UserSource::Human));

    // 保存即为新格式：再读回语义不变。
    let saved = serde_json::to_value(&session).unwrap();
    assert!(
        saved["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|msg| msg["role"].is_object())
    );
    let reloaded: Session = serde_json::from_value(saved).unwrap();
    assert_eq!(reloaded.messages[0].final_reply(), Some("a2"));
    assert_eq!(reloaded.messages[2].duration_ms(), Some(9));
}

#[test]
fn legacy_final_reply_falls_back_to_plain_assistant_text() {
    // 早期会话没有 phase：取本轮最后一条无工具调用的助手正文作最终答复；
    // 过程消息（react）与只有工具调用的助手消息不算。
    let session: Session = serde_json::from_value(serde_json::json!({
        "id": "s", "title": "t", "created_at": "t", "updated_at": "t",
        "messages": [
            {"id": "u1", "role": "user", "content": "一", "created_at": "t"},
            {"id": "a1", "role": "assistant", "content": "", "created_at": "t",
             "tool_calls": [{"id": "c", "name": "x"}]},
            {"id": "t1", "role": "tool", "content": "r", "created_at": "t", "tool_call_id": "c", "tool_name": "x"},
            {"id": "a2", "role": "assistant", "content": "回答一", "created_at": "t"},
            {"id": "u2", "role": "user", "content": "二", "created_at": "t"},
            {"id": "a3", "role": "assistant", "content": "过程", "created_at": "t", "phase": "react"}
        ]
    }))
    .unwrap();
    assert_eq!(session.messages[0].final_reply(), Some("a2"));
    assert_eq!(session.messages[4].final_reply(), None);
}

#[test]
fn plugin_session_messages_use_flat_format_and_accept_both() {
    let mut assistant = Message::with_reasoning(MessageRole::Assistant, "回复", "思考");
    if let Role::Assistant { tool_calls, .. } = &mut assistant.role {
        tool_calls.push(MessageToolCall {
            id: "c1".into(),
            name: "x".into(),
            arguments: serde_json::Value::Null,
        });
    }
    let injected = Message::new(MessageRole::User, "").with_source(UserSource::HostInjected);
    let snapshot = PluginSession {
        id: "s".into(),
        title: "t".into(),
        cwd: "/tmp".into(),
        workspace_id: "w".into(),
        reasoning_effort: None,
        messages: vec![assistant, injected],
        context_summary: None,
        created_at: "t".into(),
        updated_at: "t".into(),
    };
    let json = serde_json::to_value(&snapshot).unwrap();
    // 旧版插件按扁平格式解析：role 为字符串，字段平铺。
    assert_eq!(json["messages"][0]["role"], "assistant");
    assert_eq!(json["messages"][0]["reasoning_content"], "思考");
    assert_eq!(json["messages"][0]["tool_calls"][0]["id"], "c1");
    assert_eq!(json["messages"][1]["phase"], "hostinjected");
    let parsed: PluginSession = serde_json::from_value(json).unwrap();
    assert_eq!(parsed.messages[0].tool_calls().len(), 1);
    assert_eq!(
        parsed.messages[1].user_source(),
        Some(UserSource::HostInjected)
    );
}

#[test]
fn message_render_validation() {
    let render = |plugin: &str, view: &str, data: serde_json::Value| MessageRender {
        plugin: plugin.into(),
        view: view.into(),
        data,
    };
    assert!(
        render("bot", "card", serde_json::Value::Null)
            .validate()
            .is_ok()
    );
    assert!(
        render(" ", "card", serde_json::Value::Null)
            .validate()
            .is_err()
    );
    assert!(
        render("bot", "", serde_json::Value::Null)
            .validate()
            .is_err()
    );
    let big = "x".repeat(MESSAGE_RENDER_MAX_BYTES);
    assert!(
        render("bot", "card", serde_json::json!(big))
            .validate()
            .is_err()
    );
}

#[test]
fn session_serde_roundtrip() {
    let mut session = Session::new("测试会话");
    session.append_message(MessageRole::User, "你好");
    session.append_message(MessageRole::Assistant, "你好！");

    let json = serde_json::to_string(&session).unwrap();
    let parsed: Session = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.title, "测试会话");
    assert_eq!(parsed.messages.len(), 2);
    assert_eq!(parsed.messages[0].text_content(), "你好");
}

#[test]
fn empty_object_is_rejected_required_fields() {
    // 回归：id/role/content/created_at 为必填字段。空对象 `{}` 必须反序列化失败，
    // 而非静默生成空编号、空正文的用户消息（与 origin/main 的 derive 行为一致）。
    let result = serde_json::from_str::<Message>("{}");
    assert!(
        result.is_err(),
        "空对象应因缺少必填字段而失败，实际得到：{:?}",
        result.ok()
    );
}

#[test]
fn missing_created_at_is_rejected() {
    // 缺少 created_at（必填）应失败
    let json = r#"{"id":"x","role":"user","content":"hi"}"#;
    let result = serde_json::from_str::<Message>(json);
    assert!(
        result.is_err(),
        "缺少 created_at 应失败，实际得到：{:?}",
        result.ok()
    );
}

#[test]
fn tool_result_injection_serde_locks_protocol_shape() {
    // 权威 JSON 形状（RFC 0017）：工具 stdout 与业务字段共存，未知字段容忍。
    let stdout = r#"{
        "path": "/tmp/desktop-1.png",
        "width": 100,
        "injected_assets": [{
            "local_path": "/tmp/desktop-1.png",
            "mime_type": "image/png",
            "original_name": "desktop-1.png",
            "size_bytes": 2048,
            "kind": "image",
            "source": "desktop_screenshot"
        }]
    }"#;
    assert!(ToolResultInjection::has_declaration_marker(stdout));
    let decl = ToolResultInjection::parse(stdout).expect("合法 JSON 必须解析成功");
    assert_eq!(decl.injected_assets.len(), 1);
    let asset = &decl.injected_assets[0];
    assert_eq!(asset.local_path, "/tmp/desktop-1.png");
    assert_eq!(asset.kind, MediaKind::Image);
    assert!(asset.is_valid());
    let stored = asset.to_stored_asset("inject-1".to_string());
    assert_eq!(stored.asset_id, "inject-1");
    assert_eq!(stored.size, 2048);
    assert_eq!(stored.kind, MediaKind::Image);

    // 最小声明：除必要字段外全部缺省（kind 默认 image、名字从路径派生）。
    let minimal =
        r#"{"injected_assets": [{"local_path": "/tmp/shot.png", "mime_type": "image/png"}]}"#;
    let decl = ToolResultInjection::parse(minimal).expect("最小声明必须可解析");
    let asset = &decl.injected_assets[0];
    assert_eq!(asset.kind, MediaKind::Image);
    assert_eq!(asset.size_bytes, 0);
    assert!(asset.source.is_none());
    assert_eq!(
        asset.to_stored_asset("a".to_string()).original_name,
        "shot.png"
    );

    // 非图片类型保留扩展位：文件声明可反序列化，不在此层拒绝。
    let file_decl = r#"{"injected_assets": [{"local_path": "/tmp/report.pdf", "mime_type": "application/pdf", "kind": "file"}]}"#;
    let decl = ToolResultInjection::parse(file_decl).expect("文件声明必须可解析");
    assert_eq!(decl.injected_assets[0].kind, MediaKind::File);

    // 无声明/非 JSON：parse 的三种失败面。
    assert!(!ToolResultInjection::has_declaration_marker(
        "{\"path\": \"/tmp/a\"}"
    ));
    assert!(ToolResultInjection::parse("不是 JSON").is_none());
    // 有标记但损坏：None（调用方据此告警）。
    assert!(ToolResultInjection::parse("垃圾 \"injected_assets\" 内容").is_none());

    // 序列化往返稳定（skip 规则：空数组不序列化）。
    let empty = ToolResultInjection::default();
    assert!(
        !serde_json::to_string(&empty)
            .unwrap()
            .contains("injected_assets")
    );
}
#[test]
fn message_annotations_normalize_and_append_instruction() {
    let annotations = MessageAnnotations {
        instruction: Some("  经 Bot 回复  ".into()),
        render: Some(MessageRender {
            plugin: "bot".into(),
            view: "im-message".into(),
            data: serde_json::json!({ "platform": "weixin" }),
        }),
    }
    .normalized()
    .unwrap();
    assert_eq!(annotations.instruction.as_deref(), Some("经 Bot 回复"));
    let mut content = vec![ContentBlock::text("你好")];
    annotations.append_instruction(&mut content);
    assert!(matches!(
        content.last(),
        Some(ContentBlock::ModelInstruction { text }) if text == "经 Bot 回复"
    ));

    let blank = MessageAnnotations {
        instruction: Some("   ".into()),
        render: None,
    }
    .normalized()
    .unwrap();
    assert!(blank.is_empty());

    let invalid = MessageAnnotations {
        instruction: None,
        render: Some(MessageRender {
            plugin: " ".into(),
            view: "v".into(),
            data: serde_json::Value::Null,
        }),
    };
    assert!(invalid.normalized().is_err());

    // 旧版 IncomingMessage JSON 不带 annotations 仍可解析。
    let legacy: IncomingMessage = serde_json::from_value(serde_json::json!({
        "id": "m", "connector": "c", "channel_id": "ch", "sender_id": "s",
        "content": { "Text": "hi" }, "reply_to": null, "timestamp": "now"
    }))
    .unwrap();
    assert!(legacy.annotations.is_empty());
}
