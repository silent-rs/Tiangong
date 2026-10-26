use super::*;

/// 在临时目录装一个最小 skill，返回（保持目录存活，服务实例）。
fn service_with_skill(enabled: bool) -> (tempfile::TempDir, SkillService) {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("demo-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
            skill_dir.join("skill.toml"),
            format!(
                "id = \"demo-skill\"\nname = \"演示\"\nversion = \"0.1.0\"\nentry = \"SKILL.md\"\navailable = {}\ndescription = \"演示技能\"\n",
                enabled
            ),
        )
        .unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "说明见 {skill_dir}/SKILL.md").unwrap();
    let service = SkillService::with_root(dir.path());
    (dir, service)
}

/// 动态 prompt 段落：摘要段（含 skill 条目）+ 目录声明 + 创建规范。
#[tokio::test]
async fn prompt_段落三段形状() {
    let (_dir, service) = service_with_skill(true);
    let response = service
        .dispatch(Request::new(
            tiangong_plugin_sidecar::protocol::PROMPT_SECTIONS_OPERATION,
            serde_json::json!({}),
        ))
        .await;
    assert!(response.success, "{:?}", response.error_message);
    let sections: Vec<String> = serde_json::from_value(response.payload.unwrap()).unwrap();
    assert!(sections.len() >= 3, "至少三段：{sections:?}");
    assert!(sections[0].contains("demo-skill") && sections[0].contains("演示技能"));
    assert!(sections[1].contains("额外允许文件操作目录"));
    assert!(sections[2].contains("创建新 Skill 的规范"));
}

/// 提及候选：enabled skill 进入候选，value 带 @skill: 前缀。
#[tokio::test]
async fn mention_候选含启用skill() {
    let (_dir, service) = service_with_skill(true);
    let response = service
        .dispatch(Request::new(
            MENTION_CANDIDATES_OPERATION,
            serde_json::json!({}),
        ))
        .await;
    assert!(response.success, "{:?}", response.error_message);
    let candidates: Vec<serde_json::Value> =
        serde_json::from_value(response.payload.unwrap()).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["value"], "@skill:demo-skill");
    assert_eq!(candidates[0]["label"], "演示");
    assert_eq!(candidates[0]["mark"], "S");
}

/// 工具直连：禁用 skill 返回 ok:false 且 stderr 列出可用项；
/// 启用 skill 的 readme 完成 {skill_dir} 替换。
#[tokio::test]
async fn 工具直连_禁用与启用路径() {
    let (_dir, service) = service_with_skill(false);
    let response = service
        .dispatch(Request::new(
            TOOL_GET_SKILL_DETAIL_OPERATION,
            serde_json::json!({ "skill_id": "demo-skill" }),
        ))
        .await;
    assert!(response.success);
    let outcome = response.payload.unwrap();
    assert_eq!(outcome["ok"], serde_json::json!(false));
    assert!(outcome["stderr"].as_str().unwrap().contains("已禁用"));

    let (_dir, service) = service_with_skill(true);
    let response = service
        .dispatch(Request::new(
            TOOL_GET_SKILL_DETAIL_OPERATION,
            serde_json::json!({ "skill_id": "demo-skill" }),
        ))
        .await;
    let outcome = response.payload.unwrap();
    assert_eq!(outcome["ok"], serde_json::json!(true));
    let stdout = outcome["stdout"].as_str().unwrap();
    assert!(!stdout.contains("{skill_dir}"), "{stdout}");
    assert!(stdout.contains("SKILL.md"));
}

/// 工具直连缺参：通道错误（与未知操作可区分）。
#[tokio::test]
async fn 工具直连_缺参报错() {
    let (_dir, service) = service_with_skill(true);
    let response = service
        .dispatch(Request::new(
            TOOL_GET_SKILL_DETAIL_OPERATION,
            serde_json::json!({}),
        ))
        .await;
    assert!(!response.success);
    assert!(
        response
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("skill_id"))
    );
}

/// 管理页结构化详情（skill_detail）与工具直连（get_skill_detail）
/// 操作名分离，响应形状不同。
#[tokio::test]
async fn 管理详情与工具操作分离() {
    assert_eq!(SKILL_DETAIL_OPERATION, "skill_detail");
    assert_eq!(TOOL_GET_SKILL_DETAIL_OPERATION, "get_skill_detail");
    let (_dir, service) = service_with_skill(true);
    let response = service
        .dispatch(Request::new(
            SKILL_DETAIL_OPERATION,
            serde_json::json!({ "id": "demo-skill" }),
        ))
        .await;
    assert!(response.success, "{:?}", response.error_message);
    let payload = response.payload.unwrap();
    assert!(
        payload["detail"]["id"].is_string(),
        "结构化 detail：{payload}"
    );
}
