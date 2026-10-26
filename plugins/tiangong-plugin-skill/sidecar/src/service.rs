//! Skill sidecar 业务服务：承载 skill 注册表扫描、skill.toml 读写、SKILL.md 加载、
//! 环境变量解析与审计日志。
//!
//! 整合原进程内插件的 `handler.rs`（get_skill_detail 工具）、`management.rs`（管理 API）、
//! `mcp_lock.rs`、`prompt.rs`（摘要数据）逻辑，全部经 IPC 操作暴露给运行时与 WASM 桥接。

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result, anyhow};
use tiangong_plugin_sidecar::env::STORAGE_ROOT_ENV;
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};
use tiangong_plugin_skill_protocol::{
    Empty, GetSkillDetailRequest, GetSkillEnvRequest, GetSkillEnvResponse, InstalledSkillConfig,
    ListSkillsResponse, MENTION_CANDIDATES_OPERATION, PLUGIN_ID, PLUGIN_VERSION,
    RemoveSkillRequest, RemoveSkillResponse, SKILL_DETAIL_OPERATION, SKILL_PROTOCOL_VERSION,
    SetSkillEnabledRequest, SetSkillEnvRequest, SkillDetail, SkillDetailResponse, SkillManifest,
    SkillMcpRequirementConfig, SkillSummaryItem, SkillSummaryResponse, TOOL_GET_SKILL_DETAIL,
    TOOL_GET_SKILL_DETAIL_OPERATION,
};

use crate::registry::SkillRegistry;

/// Skill sidecar 业务服务。
pub struct SkillService {
    /// skill 注册表（扫描 `~/.tiangong/skills/`）。
    registry: SkillRegistry,
}

impl SkillService {
    /// 构造服务：解析存储根、构造注册表并首次扫描。
    pub fn new() -> Result<Self> {
        let root = resolve_skills_root();
        let registry = SkillRegistry::new(root);
        registry.refresh();
        Ok(Self { registry })
    }

    /// 测试构造：指定 skills 根目录。
    #[cfg(test)]
    fn with_root(root: impl Into<std::path::PathBuf>) -> Self {
        let registry = SkillRegistry::new(root);
        registry.refresh();
        Self { registry }
    }

    /// 按 sidecar 协议分发请求。
    pub async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Skill 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }

        let payload = match self
            .dispatch_operation(&request.operation, request.payload)
            .await
        {
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

    async fn dispatch_operation(
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
                business_protocol: SKILL_PROTOCOL_VERSION,
                capabilities: vec![
                    "skill".to_string(),
                    // 动态 prompt 段落（宿主经固定操作 prompt_sections 获取）。
                    tiangong_plugin_sidecar::protocol::PROMPT_CAPABILITY.to_string(),
                    format!("tool:{TOOL_GET_SKILL_DETAIL}"),
                ],
                instance_id: format!("skill-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 Skill 握手响应失败"),

            "list_skills" => {
                let _payload: Empty = serde_json::from_value(payload).unwrap_or_default();
                let skills = self.list_skills();
                serde_json::to_value(ListSkillsResponse { skills })
                    .with_context(|| "序列化 list_skills 响应失败")
            }

            // 设置页的结构化详情（与工具直连的 ToolOutcome 形状分离）。
            SKILL_DETAIL_OPERATION => {
                let req: GetSkillDetailRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 skill_detail 请求失败")?;
                let detail = self.get_skill_detail(&req.id)?;
                serde_json::to_value(SkillDetailResponse { detail })
                    .with_context(|| "序列化 skill_detail 响应失败")
            }

            // ── 工具直连（operation = 工具名，返回 ToolOutcome 形状）──
            TOOL_GET_SKILL_DETAIL_OPERATION => self.tool_get_skill_detail(payload).await,

            // ── 动态 prompt 段落（宿主声明聚合时经固定操作获取）──
            tiangong_plugin_sidecar::protocol::PROMPT_SECTIONS_OPERATION => {
                serde_json::to_value(self.prompt_sections())
                    .with_context(|| "序列化 prompt_sections 响应失败")
            }

            // ── @ 提及候选（manifest mention.candidates_op，含插件前缀）──
            MENTION_CANDIDATES_OPERATION => {
                let candidates: Vec<serde_json::Value> = self
                    .list_skills()
                    .into_iter()
                    .filter(|skill| skill.enabled)
                    .map(|skill| {
                        serde_json::json!({
                            "value": format!("@skill:{}", skill.id),
                            "label": skill.name,
                            "kind": "skill",
                            "hint": if skill.description.is_empty() {
                                format!("v{}", skill.version)
                            } else {
                                skill.description
                            },
                            "mark": "S",
                        })
                    })
                    .collect();
                serde_json::to_value(candidates).with_context(|| "序列化提及候选失败")
            }

            "remove_skill" => {
                let req: RemoveSkillRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 remove_skill 请求失败")?;
                let resp = self.remove_skill(&req.id)?;
                serde_json::to_value(resp).with_context(|| "序列化 remove_skill 响应失败")
            }

            "set_skill_enabled" => {
                let req: SetSkillEnabledRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 set_skill_enabled 请求失败")?;
                let message = self.set_skill_enabled(&req.id, req.enabled)?;
                serde_json::to_value(tiangong_plugin_skill_protocol::MessageResponse { message })
                    .with_context(|| "序列化 set_skill_enabled 响应失败")
            }

            "refresh_skills" => {
                let _payload: Empty = serde_json::from_value(payload).unwrap_or_default();
                let message = self.refresh_skills()?;
                serde_json::to_value(tiangong_plugin_skill_protocol::MessageResponse { message })
                    .with_context(|| "序列化 refresh_skills 响应失败")
            }

            "get_skill_env" => {
                let req: GetSkillEnvRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 get_skill_env 请求失败")?;
                let env = self.get_skill_env(&req.id)?;
                serde_json::to_value(GetSkillEnvResponse { env })
                    .with_context(|| "序列化 get_skill_env 响应失败")
            }

            "set_skill_env" => {
                let req: SetSkillEnvRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 set_skill_env 请求失败")?;
                self.set_skill_env(&req.id, &req.env)?;
                serde_json::to_value(Empty {}).with_context(|| "序列化 set_skill_env 响应失败")
            }

            "get_skill_summary" => {
                let _payload: Empty = serde_json::from_value(payload).unwrap_or_default();
                let resp = self.get_skill_summary();
                serde_json::to_value(resp).with_context(|| "序列化 get_skill_summary 响应失败")
            }

            "init_skill" => {
                let req: tiangong_plugin_skill_protocol::InitSkillRequest =
                    serde_json::from_value(payload).with_context(|| "解析 init_skill 请求失败")?;
                let result = crate::skill_init::init_skill_scaffold(req)?;
                serde_json::to_value(result).with_context(|| "序列化 init_skill 响应失败")
            }

            "update_skill_md" => {
                let req: tiangong_plugin_skill_protocol::UpdateSkillMdRequest =
                    serde_json::from_value(payload)
                        .with_context(|| "解析 update_skill_md 请求失败")?;
                self.update_skill_md(&req.id, &req.content)?;
                serde_json::to_value(Empty {}).with_context(|| "序列化 update_skill_md 响应失败")
            }

            "reveal_skill_dir" => {
                let req: tiangong_plugin_skill_protocol::RevealSkillDirRequest =
                    serde_json::from_value(payload)
                        .with_context(|| "解析 reveal_skill_dir 请求失败")?;
                self.reveal_skill_dir(&req.id)?;
                serde_json::to_value(Empty {}).with_context(|| "序列化 reveal_skill_dir 响应失败")
            }

            other => Err(anyhow!("未知的 Skill 操作: {other}")),
        }
    }

    // ── 业务实现 ──────────────────────────────────────────────────

    /// get_skill_detail 工具直连入口（原 WASM 桥接迁入）：ToolOutcome 形状。
    async fn tool_get_skill_detail(&self, payload: serde_json::Value) -> Result<serde_json::Value> {
        let skill_id = payload
            .get("skill_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if skill_id.is_empty() {
            anyhow::bail!("缺少必填参数 skill_id");
        }
        let detail = self.get_skill_detail(&skill_id)?;
        if !detail.enabled {
            // 列出可用 skill 供 Agent 参考。
            let summary = self.get_skill_summary();
            let available = summary
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(serde_json::json!({
                "ok": false,
                "summary": format!("Skill {} 已禁用", detail.name),
                "stdout": "",
                "stderr": format!("Skill {} 当前已禁用。可用的 skill：{}", detail.name, available),
                "exit_code": 1,
            }));
        }
        // {skill_dir} 占位符替换：用 storage_root/<id> 还原真实目录。
        let summary = self.get_skill_summary();
        let skill_dir = format!(
            "{}/{}",
            summary.storage_root.trim_end_matches('/'),
            detail.id
        );
        let readme = detail.readme.replace("{skill_dir}", &skill_dir);
        Ok(serde_json::json!({
            "ok": true,
            "summary": format!("Skill {} 的使用说明", detail.name),
            "stdout": readme,
            "stderr": "",
            "exit_code": 0,
        }))
    }

    /// 系统提示注入段落（原 WASM build_prompt_sections 迁入）：已装 skill
    /// 摘要、允许文件操作目录声明、创建规范，共三段。
    fn prompt_sections(&self) -> Vec<String> {
        let summary = self.get_skill_summary();
        let mut sections = Vec::new();
        if !summary.items.is_empty() {
            let mut lines = String::from(
                "已安装的 Skills（如果 Skill 能处理用户请求，优先调用 get_skill_detail 获取完整说明，然后按文档使用 run_command/run_shell 执行对应脚本）：\n",
            );
            for item in &summary.items {
                let desc = if item.description.is_empty() {
                    "无描述"
                } else {
                    &item.description
                };
                lines.push_str(&format!("- {} (id={}): {}\n", item.name, item.id, desc));
            }
            sections.push(lines);
        }
        if !summary.storage_root.is_empty() {
            sections.push(format!("额外允许文件操作目录：{}", summary.storage_root));
        }
        sections.push(skill_creation_guide(&summary.storage_root));
        sections
    }

    /// 已安装 skill 列表（轻量，只读 skill.toml）。
    fn list_skills(&self) -> Vec<InstalledSkillConfig> {
        let view = self.registry.view();
        let mut installed = Vec::new();
        for entry in view.entries.values() {
            if let Some(config) = build_installed_skill_config(entry) {
                installed.push(config);
            }
        }
        installed.sort_by(|a, b| a.id.cmp(&b.id));
        installed
    }

    /// skill 完整详情（含 SKILL.md 全文）。
    fn get_skill_detail(&self, id: &str) -> Result<SkillDetail> {
        let loaded = self.registry.get(id)?;
        Ok(SkillDetail {
            id: loaded.manifest.id.clone(),
            name: loaded.manifest.name.clone(),
            version: loaded.manifest.version.clone(),
            description: loaded.manifest.description.clone(),
            entry: loaded.manifest.entry.clone(),
            enabled: loaded.manifest.available,
            readme: loaded.readme.clone(),
        })
    }

    /// 卸载 skill：删除目录 + 刷新 + 算孤儿 MCP，返回消息与 orphan 列表。
    fn remove_skill(&self, id: &str) -> Result<RemoveSkillResponse> {
        let id = id.trim();
        if id.is_empty() {
            anyhow::bail!("skill id 不能为空");
        }

        let view = self.registry.view();
        let entry = view
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("未找到 skill：{id}"))?;
        let skill_dir = entry.dir.clone();

        // 收集该 skill 声明的托管 MCP server 名。
        let removed_managed: Vec<String> =
            crate::registry::read_skill_manifest(&skill_dir.join("skill.toml"))
                .ok()
                .map(|m| managed_mcp_names(id, &m.requires.mcp))
                .unwrap_or_default();

        // 删除目录。
        if skill_dir.exists() {
            fs::remove_dir_all(&skill_dir)
                .with_context(|| format!("删除 skill 目录失败：{}", skill_dir.display()))?;
        }

        // 刷新 registry。
        self.registry.invalidate(id);
        self.registry.refresh();

        // 同步 mcp-lock。
        let _ = crate::mcp_lock::sync_mcp_dependency_lock(&self.registry);

        // 找出孤儿：被删除的 skill 声明过、但删除后没有任何其他 skill 引用的 mcp server。
        let installed_after = self.list_skills();
        let all_declared: std::collections::HashSet<String> = installed_after
            .iter()
            .flat_map(|s| s.managed_mcp_servers.iter().cloned())
            .collect();
        let orphan_mcp_servers: Vec<String> = removed_managed
            .into_iter()
            .filter(|name| !all_declared.contains(name))
            .collect();

        Ok(RemoveSkillResponse {
            message: format!("skill 已删除：{id}"),
            orphan_mcp_servers,
        })
    }

    /// 启用/禁用 skill。
    fn set_skill_enabled(&self, id: &str, enabled: bool) -> Result<String> {
        self.registry.set_available(id, enabled)?;
        // 同步 mcp-lock（启停影响依赖计数）。
        let _ = crate::mcp_lock::sync_mcp_dependency_lock(&self.registry);
        Ok(format!("skill 状态已更新：{id} enabled={enabled}"))
    }

    /// 重扫 skills 注册表，返回计数摘要。
    fn refresh_skills(&self) -> Result<String> {
        let view = self.registry.refresh();
        let total = view.entries.len();
        let enabled = view
            .entries
            .values()
            .filter(|entry| {
                crate::registry::read_skill_manifest(&entry.dir.join("skill.toml"))
                    .map(|m| m.available)
                    .unwrap_or(false)
            })
            .count();
        Ok(format!(
            "skills 已刷新：total={total} enabled={enabled} disabled={}",
            total.saturating_sub(enabled)
        ))
    }

    /// 读取 skill 的 .env.local 环境变量。
    fn get_skill_env(&self, id: &str) -> Result<BTreeMap<String, String>> {
        let view = self.registry.view();
        let entry = view
            .entries
            .get(id.trim())
            .cloned()
            .ok_or_else(|| anyhow!("未找到 skill：{id}"))?;
        let mut env = BTreeMap::new();
        for (key, value) in crate::env_local::load_local_env(&entry.dir) {
            env.insert(key, value);
        }
        Ok(env)
    }

    /// 写入 skill 的 .env.local。
    fn set_skill_env(&self, id: &str, env: &BTreeMap<String, String>) -> Result<()> {
        let view = self.registry.view();
        let entry = view
            .entries
            .get(id.trim())
            .cloned()
            .ok_or_else(|| anyhow!("未找到 skill：{id}"))?;

        let env_path = entry.dir.join(".env.local");
        let content: String = env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n");
        let content = if content.is_empty() {
            String::new()
        } else {
            format!("{content}\n")
        };
        fs::write(&env_path, content)
            .with_context(|| format!("写入 .env.local 失败：{}", env_path.display()))?;
        Ok(())
    }

    /// prompt 段落注入用的 skill 摘要（enabled skill 列表 + 存储根）。
    fn get_skill_summary(&self) -> SkillSummaryResponse {
        let view = self.registry.view();
        let mut items = Vec::new();
        for entry in view.entries.values() {
            let Ok(manifest) = crate::registry::read_skill_manifest(&entry.dir.join("skill.toml"))
            else {
                continue;
            };
            if !manifest.available {
                continue;
            }
            items.push(SkillSummaryItem {
                id: manifest.id,
                name: manifest.name,
                description: manifest.description,
            });
        }
        items.sort_by(|a, b| a.id.cmp(&b.id));
        SkillSummaryResponse {
            items,
            storage_root: self.registry.root().display().to_string(),
        }
    }

    /// 更新 skill 的 SKILL.md 内容。
    fn update_skill_md(&self, id: &str, content: &str) -> Result<()> {
        let id = id.trim();
        if id.is_empty() {
            anyhow::bail!("skill id 不能为空");
        }
        let view = self.registry.view();
        let entry = view
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("未找到 skill：{id}"))?;
        // 先读 manifest 拿到 entry 文件名（默认 SKILL.md）。
        let manifest = crate::registry::read_skill_manifest(&entry.dir.join("skill.toml"))?;
        let md_path = entry.dir.join(&manifest.entry);
        fs::write(&md_path, content)
            .with_context(|| format!("写入 {} 失败：{}", manifest.entry, md_path.display()))?;
        // 失效缓存，下次读取拿到新内容。
        self.registry.invalidate(id);
        self.registry.refresh();
        Ok(())
    }

    /// 在系统文件管理器中打开 skill 目录。
    fn reveal_skill_dir(&self, id: &str) -> Result<()> {
        let id = id.trim();
        if id.is_empty() {
            anyhow::bail!("skill id 不能为空");
        }
        let view = self.registry.view();
        let entry = view
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("未找到 skill：{id}"))?;
        let dir = entry.dir.clone();
        reveal_path_in_file_manager(&dir)
    }
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for SkillService {
    async fn dispatch(&self, request: Request) -> Response {
        SkillService::dispatch(self, request).await
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────

/// skill 创建规范模板（对齐原 wasm prompt.rs 的 skill_creation_guide）。
fn skill_creation_guide(root: &str) -> String {
    let root = if root.is_empty() {
        "~/.tiangong/skills"
    } else {
        root
    };
    format!(
        r#"创建新 Skill 的规范：
- Skill 存储目录：{root}
- 每个 Skill 是一个子目录，目录名即 skill id，需含 `skill.toml` 与 `SKILL.md` 两个文件。
- 不要调用任何专用安装工具，直接用文件写入工具（fs 插件 file_write，action=write）在存储目录下创建文件即可。

创建步骤：
1. 在 `{root}` 下创建子目录（目录名即 skill id，只允许小写字母、数字、短横线）。
2. 写入 `skill.toml`（最小模板）：
   ```toml
   id = "<skill id>"
   name = "<显示名>"
   version = "0.1.0"
   entry = "SKILL.md"
   available = true

   [source]
   kind = "local"
   value = ""

   [requires]

   [permissions]
   ```
3. 写入 `SKILL.md`（使用说明，支持 `{{{{skill_dir}}}}` 占位符，运行时替换为 skill 目录绝对路径）。

创建后需刷新（重启或等待缓存失效）才能生效。"#,
        root = root
    )
}

/// 解析 skills 存储根：优先 `TIANGONG_STORAGE_ROOT/skills`，回退 `~/.tiangong/skills`。
fn resolve_skills_root() -> std::path::PathBuf {
    if let Some(root) = std::env::var(STORAGE_ROOT_ENV)
        .ok()
        .filter(|s| !s.is_empty())
    {
        return std::path::PathBuf::from(root).join("skills");
    }
    crate::paths::default_skills_storage_dir_path()
}

/// 计算 skill 声明的托管 MCP server 名（`skill::<skill_id>::<mcp_id>`）。
fn managed_mcp_names(skill_id: &str, mcp: &[SkillMcpRequirementConfig]) -> Vec<String> {
    mcp.iter()
        .map(|req| {
            let mcp_id = if req.id.trim().is_empty() {
                req.package.trim()
            } else {
                req.id.trim()
            };
            format!("skill::{skill_id}::{mcp_id}")
        })
        .collect()
}

/// 从 registry entry 构建 InstalledSkillConfig。
fn build_installed_skill_config(
    entry: &crate::registry::SkillRegistryEntry,
) -> Option<InstalledSkillConfig> {
    let manifest: SkillManifest =
        crate::registry::read_skill_manifest(&entry.dir.join("skill.toml")).ok()?;
    let managed_mcp_servers = managed_mcp_names(&entry.id, &manifest.requires.mcp);
    Some(InstalledSkillConfig {
        id: manifest.id,
        name: manifest.name,
        version: manifest.version,
        description: manifest.description,
        entry: manifest.entry,
        enabled: manifest.available,
        installed_at: String::new(),
        managed_mcp_servers,
        source: tiangong_plugin_skill_protocol::SkillSourceConfig {
            kind: "local".to_string(),
            value: entry.dir.display().to_string(),
        },
        requires_mcp: manifest.requires.mcp,
        permissions: manifest.permissions,
    })
}

/// 跨平台在系统文件管理器中打开/定位目录。
///
/// - macOS：`open <dir>`
/// - Windows：`explorer <dir>`
/// - Linux：`xdg-open <dir>`
///
/// 跨平台在系统文件管理器中打开/定位目录。
///
/// sidecar 继承了宿主 GUI 应用的桌面会话，可正常唤起文件管理器。
/// 显式重定向标准流并等待命令退出，避免子进程因管道问题立即结束。
fn reveal_path_in_file_manager(dir: &std::path::Path) -> Result<()> {
    use std::process::Stdio;
    let (program, arg) = if cfg!(target_os = "macos") {
        ("open", dir.as_os_str())
    } else if cfg!(target_os = "windows") {
        ("explorer", dir.as_os_str())
    } else {
        ("xdg-open", dir.as_os_str())
    };
    let status = std::process::Command::new(program)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("启动 {program} 失败"))?;
    if !status.success() {
        anyhow::bail!("{program} 退出码非 0：{status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
