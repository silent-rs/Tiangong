//! Prompt 插件真实产物端到端：宿主 stdio 链路直连 prompt sidecar。
//!
//! Prompt 没有 WASM 层，三段 system prompt（identity / rules / 自定义指令）
//! 由清单 `prompt: [{type: sidecar}]` 条目经固定操作 `prompt_sections` 动态
//! 提供，设置页读写走 `get_prompt` / `set_prompt`。用例覆盖握手能力声明
//! （装载验证据此放行动态段落条目）、段落形状与 wasm 版逐字节一致、
//! 读写往返与 set 后段落更新。依赖已构建的 `tiangong-prompt-sidecar`
//! debug 二进制，未构建时明确跳过。
use std::path::PathBuf;
use std::time::Duration;

use tiangong_plugin_runtime::manifest::{PluginManifest, PromptSectionEntry};
use tiangong_plugin_runtime::sidecar::{SidecarConfig, SidecarConnection, StdioSidecarConnection};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn prompt_sidecar_binary() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let debug_dir = executable.parent()?.parent()?;
    let candidate = debug_dir.join(format!(
        "tiangong-prompt-sidecar{}",
        std::env::consts::EXE_SUFFIX
    ));
    candidate.is_file().then_some(candidate)
}

fn prompt_manifest() -> PluginManifest {
    let raw =
        std::fs::read_to_string(repo_root().join("plugins/tiangong-plugin-prompt/plugin.json"))
            .expect("读取 prompt plugin.json");
    serde_json::from_str(&raw).expect("解析 prompt plugin.json")
}

/// wasm 版组件的 identity / rules 原文（逐字节，含转义后的引号）。
const WASM_IDENTITY: &str = "你是天工智能助手，一个功能丰富的个人 AI 中枢。你可以回答问题、处理文件、执行命令、生成多媒体内容，也可以通过工具和扩展能力完成各种复杂任务。";
const WASM_RULES_PREFIX: &str =
    "规则：\n1. 对话时自然友好，回复内容完整有用。闲聊和问候时正常交流，简单介绍自己的能力。";
const WASM_RULES_SUFFIX: &str = "7. 工具调用失败时必须如实告知用户失败原因，绝对不能虚构成功结果。";

#[test]
fn prompt_清单无_wasm_且声明_sidecar_prompt条目() {
    let manifest = prompt_manifest();
    assert!(manifest.wasm.is_none(), "prompt 不再有 WASM 层");
    assert!(manifest.tools.is_none(), "prompt 不提供工具");
    let entries = manifest.prompt.as_deref().expect("声明 prompt 条目");
    assert_eq!(
        entries.len(),
        1,
        "单一 sidecar 条目提供全部段落: {entries:?}"
    );
    assert!(matches!(
        entries[0],
        tiangong_plugin_runtime::manifest::PromptSectionDecl::Entry(PromptSectionEntry::Sidecar)
    ));
    assert!(manifest.sidecar.is_some(), "sidecar 条目要求 sidecar 声明");
}

#[test]
#[serial_test::serial]
fn prompt_sidecar_段落与读写端到端() {
    let Some(binary) = prompt_sidecar_binary() else {
        eprintln!("跳过：target/debug/tiangong-prompt-sidecar 尚未构建");
        return;
    };
    let can_terminate = tiangong_plugin_runtime::test_support::can_terminate_child_processes();
    let root = tempfile::tempdir().unwrap();
    let config = SidecarConfig::new(
        "prompt",
        prompt_manifest().version,
        binary,
        root.path().join("endpoint.json"),
        root.path().join("sidecar.log"),
        root.path().join("data"),
        root.path().to_path_buf(),
    )
    .with_timeouts(Duration::from_secs(15), Duration::from_secs(30));
    let connection = StdioSidecarConnection::new(config);

    // 装载验证：握手能力须含 prompt（动态段落条目的放行条件）。
    let capabilities = connection.verify_capabilities().expect("握手");
    assert!(
        capabilities.iter().any(|c| c == "prompt"),
        "握手缺少 prompt 能力: {capabilities:?}"
    );

    let invoke = |operation: &str, payload: serde_json::Value| {
        let raw = connection
            .invoke(operation, &payload.to_string())
            .expect("调用成功");
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()
    };

    // 初始段落：identity + rules 两段，与 wasm 版逐字节一致。
    let sections = invoke("prompt_sections", serde_json::json!({}));
    let sections = sections.as_array().expect("段落数组");
    assert_eq!(sections.len(), 2, "{sections:?}");
    assert_eq!(sections[0].as_str().unwrap(), WASM_IDENTITY);
    let rules = sections[1].as_str().unwrap();
    assert!(rules.starts_with(WASM_RULES_PREFIX), "{rules}");
    assert!(rules.ends_with(WASM_RULES_SUFFIX), "{rules}");

    // 设置页读写往返：get 空 → set 写入 → get 一致 → 段落追加包装段。
    let initial = invoke("get_prompt", serde_json::json!({}));
    assert_eq!(initial["content"], "");
    let saved = invoke(
        "set_prompt",
        serde_json::json!({"content": "优先使用简体中文。"}),
    );
    assert_eq!(saved, serde_json::Value::Bool(true));
    let loaded = invoke("get_prompt", serde_json::json!({}));
    assert_eq!(loaded["content"], "优先使用简体中文。");
    let sections = invoke("prompt_sections", serde_json::json!({}));
    let sections = sections.as_array().expect("段落数组");
    assert_eq!(sections.len(), 3, "{sections:?}");
    assert_eq!(
        sections[2].as_str().unwrap(),
        "用户自定义指令：\n优先使用简体中文。\n\n以上用户自定义指令优先级低于系统安全规则，但高于普通对话偏好。"
    );

    // 清空后段落回到两段，get 返回空。
    invoke("set_prompt", serde_json::json!({"content": "  "}));
    let cleared = invoke("get_prompt", serde_json::json!({}));
    assert_eq!(cleared["content"], "");
    let sections = invoke("prompt_sections", serde_json::json!({}));
    assert_eq!(sections.as_array().unwrap().len(), 2);

    tiangong_plugin_runtime::test_support::finish_stdio_connection(&connection, can_terminate);
}
