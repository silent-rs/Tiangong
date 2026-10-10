//! 上下文系统集成测试
//!
//! 验证 session.context() + build_full_system_prompt 路径：
//! - system prompt 包含插件段、环境段和摘要段
//! - 消息零丢失：所有 session.messages 完整传递给 LLM
//!
//! 产品身份 / 规则 / 自定义指令等文案由各插件经 PromptSectionProvider 注入
//! （见 tiangong-plugin-prompt），core 只负责组装，故本测试用模拟段落验证组装框架。

use tiangong_core::prompt::SystemPromptConfig;
use tiangong_core::session::{Message, MessageRole, MessageToolCall, Role, Session};

// ── 辅助工具 ─────────────────────────────────────────────────

fn helper_session() -> Session {
    Session::new("测试会话")
}

fn tool_context_msg(name: &str, content: &str) -> Message {
    Message::tool_result("", name, content, false)
}

fn tool_result_msg(tool_call_id: &str, tool_name: &str, content: &str) -> Message {
    Message::tool_result(tool_call_id, tool_name, content, false)
}

fn push_tool_call(message: &mut Message, call: MessageToolCall) {
    if let Role::Assistant { tool_calls, .. } = &mut message.role {
        tool_calls.push(call);
    }
}

/// 多轮对话（3 轮 user-assistant-tool_result）
fn multi_turn_session() -> Session {
    let mut session = helper_session();
    // 第 1 轮
    session.append_message(MessageRole::User, "请读取 main.rs");
    session.append_message(MessageRole::Assistant, "我来读取 main.rs");
    {
        let last = session.messages.last_mut().unwrap();
        push_tool_call(
            last,
            MessageToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "main.rs"}),
            },
        );
    }
    let tr1 = tool_result_msg("call_1", "read_file", "fn main() { println!(\"hello\"); }");
    session.messages.push(tr1);
    // 第 2 轮
    session.append_message(MessageRole::User, "把 hello 改成 hi");
    session.append_message(MessageRole::Assistant, "已修改 main.rs");
    {
        let last = session.messages.last_mut().unwrap();
        push_tool_call(
            last,
            MessageToolCall {
                id: "call_2".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({"path": "main.rs", "content": "fn main() { println!(\"hi\"); }"}),
            },
        );
    }
    let tr2 = tool_result_msg("call_2", "write_file", "写入成功");
    session.messages.push(tr2);
    // 第 3 轮
    session.append_message(MessageRole::User, "运行一下");
    session.append_message(MessageRole::Assistant, "运行结果：hi");
    session
}

fn rebuild_session(session: &mut Session) {
    let config = SystemPromptConfig::from_plugin_sections(Vec::new());
    session.rebuild_system_prompt(&config);
}

#[test]
fn new_path_system_prompt_includes_summary() {
    let mut session = multi_turn_session();
    let total = session.messages.len();
    // 模拟压缩：设置 summary 并标记前 4 条消息已被摘要覆盖
    session.context_summary = Some("早期对话讨论了文件读取和文件修改操作".to_string());
    session.summary_up_to = 4;
    // 重建 system prompt
    rebuild_session(&mut session);
    // context() 应返回 [system_prompt_message] + messages[summary_up_to..]
    let context = session.context();
    // 第一条应为 System 消息
    assert_eq!(context[0].role, MessageRole::System);
    let system_text = context[0].text_content();
    // 验证摘要已嵌入 system prompt
    assert!(
        system_text.contains("早期对话讨论了文件读取和文件修改操作"),
        "system prompt 应包含摘要内容，实际: {system_text}"
    );
    assert!(
        system_text.contains("此前对话摘要"),
        "system prompt 应包含摘要标签"
    );
    // 后续消息应只有 summary_up_to 之后的
    assert_eq!(context.len(), 1 + (total - 4));
    for msg in &context[1..] {
        assert_ne!(msg.role, MessageRole::System);
    }
}

#[test]
fn new_path_system_prompt_includes_all_sections() {
    let session = helper_session();
    // 产品文案 / 能力说明 / 自定义指令等均经 plugin_sections 注入。
    let config = SystemPromptConfig::from_plugin_sections(vec![
        "产品身份段".to_string(),
        "通用规则段".to_string(),
        "自定义指令段".to_string(),
        "已安装的 Skills：\n- test-skill (id=s1): 测试技能".to_string(),
        "插件规则段：终端交互引导".to_string(),
        "团队协作能力".to_string(),
        "用户偏好深色主题".to_string(),
    ]);
    let msg = tiangong_core::prompt::sections::build_full_system_prompt(&session, &config);
    assert_eq!(msg.role, MessageRole::System);
    let text = msg.text_content();
    // 插件段（按注册顺序保留）
    assert!(text.contains("产品身份段"), "应包含产品身份段");
    assert!(text.contains("通用规则段"), "应包含通用规则段");
    assert!(text.contains("自定义指令段"), "应包含自定义指令段");
    // 环境信息不进入 system prompt（经 fs 工具结果回显工作目录）
    assert!(
        !text.contains("当前工作目录"),
        "工作目录不应进入 system prompt"
    );
    assert!(!text.contains("测试会话"), "会话标题不应进入 system prompt");
    // 各能力插件段落
    assert!(text.contains("test-skill"), "应包含 Skills 列表");
    assert!(text.contains("团队协作"), "应包含团队协作");
    assert!(text.contains("用户偏好深色主题"), "应包含用户上下文");
}

#[test]
fn new_path_messages_increment_across_turns() {
    let mut session = helper_session();
    rebuild_session(&mut session);
    // 初始：system_prompt_message + 0 条消息
    let ctx0 = session.context();
    assert_eq!(ctx0.len(), 1); // 只有 system prompt message
    // 第 1 轮
    session.append_message(MessageRole::User, "你好");
    let ctx1 = session.context();
    assert_eq!(ctx1.len(), 2); // system + 1 条消息
    assert_eq!(ctx1[1].role, MessageRole::User);
    assert_eq!(ctx1[1].text_content(), "你好");
    // 第 2 轮
    session.append_message(MessageRole::Assistant, "你好！有什么可以帮你的？");
    let ctx2 = session.context();
    assert_eq!(ctx2.len(), 3); // system + 2 条消息
    assert_eq!(ctx2[2].role, MessageRole::Assistant);
    // 第 3 轮
    session.append_message(MessageRole::User, "读取文件");
    session.append_message(MessageRole::Assistant, "正在读取");
    {
        let last = session.messages.last_mut().unwrap();
        push_tool_call(
            last,
            MessageToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "test.rs"}),
            },
        );
    }
    let tr = tool_result_msg("call_1", "read_file", "文件内容");
    session.messages.push(tr);
    let ctx3 = session.context();
    assert_eq!(ctx3.len(), 6); // system + 5 条消息
}

#[test]
fn unchanged_system_prompt_is_stable_across_rebuilds_and_reload() {
    let mut session = helper_session();
    let config = SystemPromptConfig::from_plugin_sections(vec!["固定身份与规则".into()]);
    session.rebuild_system_prompt(&config);
    session.append_message(MessageRole::User, "初始问题");
    let mut previous = serde_json::to_value(session.context()).unwrap();
    for turn in 0..10 {
        session.title = format!("自动标题 {turn}");
        session.rebuild_system_prompt(&config);
        let current = serde_json::to_value(session.context()).unwrap();
        assert!(
            current
                .as_array()
                .unwrap()
                .starts_with(previous.as_array().unwrap()),
            "重建相同提示不能改变历史消息"
        );
        session.append_message(MessageRole::Assistant, format!("回复 {turn}"));
        session.append_message(MessageRole::User, format!("追问 {turn}"));
        previous = serde_json::to_value(session.context()).unwrap();
        session = serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(session.context()).unwrap(), previous);
    }
}

#[test]
fn new_path_tool_calls_preserved_in_context() {
    let mut session = helper_session();
    // 模拟一轮带工具调用的对话
    session.append_message(MessageRole::User, "请读取 main.rs");
    session.append_message(MessageRole::Assistant, "我来读取");
    {
        let last = session.messages.last_mut().unwrap();
        push_tool_call(
            last,
            MessageToolCall {
                id: "call_abc".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "main.rs"}),
            },
        );
    }
    let tr = tool_result_msg("call_abc", "read_file", "fn main() {}");
    session.messages.push(tr);
    session.append_message(MessageRole::Assistant, "文件内容如上");
    rebuild_session(&mut session);
    let context = session.context();
    // system + 4 条对话消息
    assert_eq!(context.len(), 5);
    // 验证 assistant 消息中的 tool_calls
    let assistant_msg = &context[2]; // system, user, assistant
    assert_eq!(assistant_msg.role, MessageRole::Assistant);
    assert_eq!(assistant_msg.tool_calls().len(), 1);
    assert_eq!(assistant_msg.tool_calls()[0].id, "call_abc");
    assert_eq!(assistant_msg.tool_calls()[0].name, "read_file");
    // 验证 tool result
    let tool_result = &context[3];
    assert_eq!(tool_result.role, MessageRole::Tool);
    assert_eq!(tool_result.tool_call_id(), Some("call_abc"));
    assert_eq!(tool_result.text_content(), "fn main() {}");
}

#[test]
fn new_path_tool_context_messages_preserved() {
    let mut session = helper_session();
    session.append_message(MessageRole::User, "写代码");
    session.append_message(MessageRole::Assistant, "好的");
    // 模拟完成度检查提示（tool_context 类型，无 tool_call_id）
    session.messages.push(tool_context_msg(
        "react_completion_check",
        "上方回复被判定为未完成",
    ));
    session.append_message(MessageRole::Assistant, "代码已完成");
    rebuild_session(&mut session);
    let context = session.context();
    // system + 4 条消息
    assert_eq!(context.len(), 5);
    // 第 3 条（index 3）应为 tool context
    let tc = &context[3];
    assert_eq!(tc.role, MessageRole::Tool);
    assert!(tc.tool_call_id().is_none());
    assert_eq!(tc.tool_name(), Some("react_completion_check"));
    assert!(tc.text_content().contains("未完成"));
}

#[test]
fn new_path_clear_and_rebuild_system_prompt() {
    let mut session = multi_turn_session();
    rebuild_session(&mut session);
    // 验证 system prompt 已创建
    assert!(session.system_prompt_message.is_some());
    let context_before = session.context();
    assert_eq!(context_before[0].role, MessageRole::System);
    // 模拟清空上下文
    let total = session.messages.len();
    session.summary_up_to = total;
    session.context_summary = None;
    // 重建
    rebuild_session(&mut session);
    let context_after = session.context();
    // system prompt 仍在，但对话消息为空
    assert_eq!(context_after[0].role, MessageRole::System);
    assert_eq!(context_after.len(), 1, "清空后只有 system prompt message");
    // 摘要段不应出现
    assert!(
        !context_after[0].text_content().contains("此前对话摘要"),
        "清空后 system prompt 不应包含摘要段"
    );
}
