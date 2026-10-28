//! Prompt sidecar 业务服务：承载自定义指令读写、旧数据一次性迁移与动态
//! prompt 段落（`prompt_sections` 操作）。
//!
//! 整合原 WASM 组件的 `prompt_sections`（custom 段）与 `handle_view_message`
//! （设置页 get/set），全部经 IPC 操作暴露给运行时与设置页。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tiangong_plugin_prompt_protocol::{
    GET_PROMPT_OPERATION, GetPromptResponse, PLUGIN_ID, PLUGIN_VERSION, PROMPT_PROTOCOL_VERSION,
    SET_PROMPT_OPERATION, SetPromptRequest,
};
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROMPT_CAPABILITY,
    PROMPT_SECTIONS_OPERATION, PROTOCOL_VERSION, Request, Response, ServiceStatus,
};

use crate::paths;

/// Prompt sidecar 业务服务。
pub struct PromptService {
    /// 插件数据目录（`~/.tiangong/data/prompt`）。
    data_dir: PathBuf,
    /// 旧数据位置根（存储根 `~/.tiangong`；测试注入受控目录）。
    legacy_root: PathBuf,
}

impl PromptService {
    /// 构造服务：解析数据目录（不存在则创建）并执行旧数据一次性迁移。
    pub fn new() -> Result<Self> {
        let data_dir = paths::plugin_data_dir();
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("创建数据目录失败: {}", data_dir.display()))?;
        let service = Self {
            data_dir,
            legacy_root: paths::storage_root(),
        };
        service.migrate_legacy_prompt();
        Ok(service)
    }

    /// 测试构造：指定数据目录与旧数据根。
    #[cfg(test)]
    fn with_roots(data_dir: impl Into<PathBuf>, legacy_root: impl Into<PathBuf>) -> Self {
        let service = Self {
            data_dir: data_dir.into(),
            legacy_root: legacy_root.into(),
        };
        service.migrate_legacy_prompt();
        service
    }

    /// 当前生效的自定义指令文件（数据目录；构造时已保证存在）。
    fn custom_prompt_path(&self) -> PathBuf {
        self.data_dir.join("custom-prompt.md")
    }

    /// 旧数据迁移完成标记。
    fn legacy_migration_marker(&self) -> PathBuf {
        self.data_dir.join(".legacy-migrated")
    }

    /// 一次性把旧位置（存储根 `custom-prompt.md`，更早版本 `app.json`）的
    /// 自定义指令复制到数据目录。新位置已有内容时不覆盖；旧文件保留。
    /// 迁移失败不写标记，下次访问重试。
    fn migrate_legacy_prompt(&self) {
        if self.legacy_migration_marker().exists() {
            return;
        }
        if !self.custom_prompt_path().exists() {
            let legacy = self.legacy_root.join("custom-prompt.md");
            let legacy_content = std::fs::read_to_string(&legacy)
                .ok()
                .filter(|content| !content.trim().is_empty())
                .or_else(|| read_legacy_app_config_prompt(&self.legacy_root));
            if let Some(content) = legacy_content
                && std::fs::write(self.custom_prompt_path(), content).is_err()
            {
                return;
            }
        }
        let _ = std::fs::write(self.legacy_migration_marker(), "migrated\n");
    }

    /// 读取自定义指令（文件不存在时为空）。
    fn read_custom_prompt(&self) -> String {
        std::fs::read_to_string(self.custom_prompt_path()).unwrap_or_default()
    }

    /// 写入自定义指令（空内容时删除文件），成功后通知宿主段落已变化。
    fn write_custom_prompt(&self, content: &str) -> Result<()> {
        let path = self.custom_prompt_path();
        if content.trim().is_empty() {
            std::fs::remove_file(&path).ok();
        } else {
            std::fs::write(&path, content)
                .with_context(|| format!("写入 custom-prompt.md 失败: {}", path.display()))?;
        }
        // 段落内容变化：通知宿主经 prompt_sections 重取并触发上下文交接。
        tiangong_plugin_sidecar::server::emit_notification(
            tiangong_plugin_sidecar::protocol::PROMPT_UPDATED_CHANNEL,
            serde_json::json!({ "plugin_id": PLUGIN_ID }).to_string(),
        );
        Ok(())
    }

    /// 动态 prompt 段落（原 WASM `prompt_sections` 迁入）：identity / rules
    /// 恒在，自定义指令非空时追加包装段。
    fn prompt_sections(&self) -> Vec<String> {
        let mut sections = vec![identity_section().to_string(), rules_section().to_string()];
        sections.extend(self.custom_prompt_section());
        sections
    }

    /// 自定义指令非空时包装为一段，空时无段落（原 WASM
    /// `custom_prompt_section` 迁入）。
    fn custom_prompt_section(&self) -> Option<String> {
        let custom = self.read_custom_prompt();
        let custom = custom.trim();
        if custom.is_empty() {
            return None;
        }
        Some(format!(
            "用户自定义指令：\n{custom}\n\n以上用户自定义指令优先级低于系统安全规则，但高于普通对话偏好。"
        ))
    }

    /// 按 sidecar 协议分发请求。
    pub async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Prompt 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }

        let payload = match self.handle_operation(&request.operation, request.payload) {
            Ok(value) => value,
            Err(error) => {
                return Response::error(
                    &request_id,
                    ErrorCode::ServiceError,
                    error.to_string(),
                    false,
                );
            }
        };
        Response::success(&request_id, payload)
    }

    /// 操作分发（同步实现，`dispatch` 的 async 包装仅供 IPC 服务使用）。
    fn handle_operation(
        &self,
        operation: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match operation {
            HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
                plugin_id: PLUGIN_ID.to_string(),
                plugin_version: PLUGIN_VERSION.to_string(),
                sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION.to_string(),
                business_protocol: PROMPT_PROTOCOL_VERSION,
                // 动态 prompt 段落（宿主经固定操作 prompt_sections 获取）。
                capabilities: vec![PROMPT_CAPABILITY.to_string()],
                instance_id: format!("prompt-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 Prompt 握手响应失败"),

            PROMPT_SECTIONS_OPERATION => serde_json::to_value(self.prompt_sections())
                .with_context(|| "序列化 prompt_sections 响应失败"),

            GET_PROMPT_OPERATION => {
                let response = GetPromptResponse {
                    content: self.read_custom_prompt(),
                };
                serde_json::to_value(response).with_context(|| "序列化 get_prompt 响应失败")
            }

            SET_PROMPT_OPERATION => {
                let req: SetPromptRequest =
                    serde_json::from_value(payload).with_context(|| "解析 set_prompt 请求失败")?;
                self.write_custom_prompt(&req.content)?;
                Ok(serde_json::Value::Bool(true))
            }

            other => Err(anyhow::anyhow!("未知的 Prompt 操作: {other}")),
        }
    }
}

/// 读取旧版 `app.json`（旧数据根）中保存的自定义指令（顶层字段优先，其次
/// `agent_config` 内字段）。
fn read_legacy_app_config_prompt(legacy_root: &Path) -> Option<String> {
    let path = legacy_root.join("app.json");
    let content = std::fs::read_to_string(Path::new(&path)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    [
        value.get("custom_system_prompt"),
        value
            .get("agent_config")
            .and_then(|config| config.get("custom_system_prompt")),
    ]
    .into_iter()
    .flatten()
    .filter_map(|prompt| prompt.as_str())
    .find(|prompt| !prompt.trim().is_empty())
    .map(str::to_string)
}

// ── prompt 段落（原 WASM 组件迁入，文案保持逐字节一致）──

fn identity_section() -> &'static str {
    "你是天工智能助手，一个功能丰富的个人 AI 中枢。你可以回答问题、处理文件、执行命令、生成多媒体内容，也可以通过工具和扩展能力完成各种复杂任务。"
}

fn rules_section() -> &'static str {
    "规则：\n\
     1. 对话时自然友好，回复内容完整有用。闲聊和问候时正常交流，简单介绍自己的能力。\n\
     2. 需要文件操作、代码搜索、命令执行等实际操作时，调用对应的工具。\n\
     3. 每次工具调用后会收到执行结果，根据结果决定下一步：继续调用工具或给出最终回复。\n\
     4. 执行工具任务时语言简洁高效，不要说\"让我查看\"之类的过渡语，直接给出结果。\n\
     5. 不要在回复中包含工具调用的原始痕迹（如 ok=、exit_code= 等元数据）。\n\
     6. 回复使用 Markdown 格式：代码和命令用代码块包裹，使用标题、列表等结构化排版。\n\
     7. 工具调用失败时必须如实告知用户失败原因，绝对不能虚构成功结果。"
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for PromptService {
    async fn dispatch(&self, request: Request) -> Response {
        PromptService::dispatch(self, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 受控根：`(guard, data_dir, legacy_root)`——数据目录 <root>/data/prompt，
    /// 旧数据根 <root>/legacy，两者互相隔离。
    fn temp_roots() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let root = dir.path().to_path_buf();
        let data_dir = root.join("data").join("prompt");
        std::fs::create_dir_all(&data_dir).expect("创建数据目录");
        let legacy_root = root.join("legacy");
        std::fs::create_dir_all(&legacy_root).expect("创建旧数据根");
        (dir, data_dir, legacy_root)
    }

    #[test]
    fn prompt_sections_shape_identity_rules_then_custom() {
        let (_guard, data_dir, legacy_root) = temp_roots();
        let service = PromptService::with_roots(&data_dir, &legacy_root);
        // 空 custom：identity + rules 恰好两段，identity 置顶。
        let sections = service.prompt_sections();
        assert_eq!(sections.len(), 2);
        assert!(sections[0].starts_with("你是天工智能助手"));
        assert!(sections[1].starts_with("规则：\n1. "));
    }

    #[test]
    fn non_empty_custom_prompt_appends_wrapped_section() {
        let (_guard, data_dir, legacy_root) = temp_roots();
        std::fs::write(
            data_dir.join("custom-prompt.md"),
            "  总是使用简体中文回复。  \n",
        )
        .expect("写入自定义指令");
        let service = PromptService::with_roots(&data_dir, &legacy_root);
        let sections = service.prompt_sections();
        assert_eq!(sections.len(), 3);
        assert_eq!(
            sections[2],
            "用户自定义指令：\n总是使用简体中文回复。\n\n以上用户自定义指令优先级低于系统安全规则，但高于普通对话偏好。"
        );
    }

    #[test]
    fn legacy_prompt_migrates_once_and_never_overwrites() {
        let (_guard, data_dir, legacy_root) = temp_roots();
        std::fs::write(legacy_root.join("custom-prompt.md"), "旧指令").expect("写入旧指令");
        let service = PromptService::with_roots(&data_dir, &legacy_root);
        // 迁移完成：新位置有内容、标记已写、旧文件保留。
        assert_eq!(service.read_custom_prompt(), "旧指令");
        assert!(data_dir.join(".legacy-migrated").exists());
        assert_eq!(
            std::fs::read_to_string(legacy_root.join("custom-prompt.md")).unwrap(),
            "旧指令"
        );
        // 新位置已有内容时旧文件变化不再覆盖，且标记存在时不再迁移。
        std::fs::write(legacy_root.join("custom-prompt.md"), "后来的旧指令").expect("改旧指令");
        let service = PromptService::with_roots(&data_dir, &legacy_root);
        assert_eq!(service.read_custom_prompt(), "旧指令");
    }

    #[test]
    fn legacy_app_config_prompt_migrates_when_no_markdown() {
        let (_guard, data_dir, legacy_root) = temp_roots();
        std::fs::write(
            legacy_root.join("app.json"),
            serde_json::json!({"agent_config": {"custom_system_prompt": "来自 app.json"}})
                .to_string(),
        )
        .expect("写入旧配置");
        let service = PromptService::with_roots(&data_dir, &legacy_root);
        assert_eq!(service.read_custom_prompt(), "来自 app.json");
    }

    #[test]
    fn set_then_clear_roundtrip() {
        let (_guard, data_dir, legacy_root) = temp_roots();
        let service = PromptService::with_roots(&data_dir, &legacy_root);
        service
            .write_custom_prompt("优先使用 Rust。")
            .expect("写入");
        assert_eq!(service.read_custom_prompt(), "优先使用 Rust。");
        assert_eq!(service.prompt_sections().len(), 3);
        service.write_custom_prompt("   ").expect("清空");
        assert_eq!(service.read_custom_prompt(), "");
        assert_eq!(service.prompt_sections().len(), 2);
        assert!(!service.custom_prompt_path().exists());
    }

    #[test]
    fn set_prompt_payload_persists_and_reports_true() {
        let (_guard, data_dir, legacy_root) = temp_roots();
        let service = PromptService::with_roots(&data_dir, &legacy_root);
        let value = service
            .handle_operation(
                SET_PROMPT_OPERATION,
                serde_json::json!({"content": "回复保持简洁。"}),
            )
            .expect("set_prompt 成功");
        assert_eq!(value, serde_json::Value::Bool(true));
        assert_eq!(
            std::fs::read_to_string(data_dir.join("custom-prompt.md")).unwrap(),
            "回复保持简洁。"
        );
    }

    #[test]
    fn get_prompt_returns_file_content() {
        let (_guard, data_dir, legacy_root) = temp_roots();
        std::fs::write(data_dir.join("custom-prompt.md"), "你好").expect("写入");
        let service = PromptService::with_roots(&data_dir, &legacy_root);
        let value = service
            .handle_operation(GET_PROMPT_OPERATION, serde_json::json!({}))
            .expect("get_prompt 成功");
        assert_eq!(value, serde_json::json!({"content": "你好"}));
    }

    #[test]
    fn unknown_operation_is_rejected() {
        let (_guard, data_dir, legacy_root) = temp_roots();
        let service = PromptService::with_roots(&data_dir, &legacy_root);
        assert!(
            service
                .handle_operation("nope", serde_json::json!({}))
                .is_err()
        );
    }
}
