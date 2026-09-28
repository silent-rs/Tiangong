use anyhow::Result;

use tiangong_config::default_tiangong_dir;
use tiangong_config::io::custom_prompt_path;
use tiangong_config::{load_server_config, load_tiangong_config};
use tiangong_llm::models_config::RoutingSlot;

use crate::args::DoctorArgs;
use crate::secrets::env_secret_resolvable;

pub(crate) fn run_doctor_command(args: DoctorArgs) -> Result<()> {
    let mut report = DoctorReport::default();

    check_config_dir(&mut report);
    check_models(&mut report, args.deep);
    check_model_secrets(&mut report);
    check_server(&mut report);
    check_server_token(&mut report);
    check_memory(&mut report);
    check_mcp(&mut report);
    check_skills(&mut report);
    check_custom_prompt(&mut report);

    report.print();
    if report.has_error {
        Err(anyhow::anyhow!("环境诊断发现错误项，详见上方 ❌ 标记"))
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct DoctorReport {
    lines: Vec<String>,
    has_error: bool,
}

impl DoctorReport {
    fn ok(&mut self, label: &str, detail: impl Into<String>) {
        self.lines
            .push(format!("\x1b[32m✅\x1b[0m {label:<16} {}", detail.into()));
    }

    fn warn(&mut self, label: &str, detail: impl Into<String>) {
        self.lines
            .push(format!("\x1b[33m⚠️\x1b[0m  {label:<16} {}", detail.into()));
    }

    fn err(&mut self, label: &str, detail: impl Into<String>) {
        self.has_error = true;
        self.lines
            .push(format!("\x1b[31m❌\x1b[0m {label:<16} {}", detail.into()));
    }

    fn skip(&mut self, label: &str, detail: impl Into<String>) {
        self.lines
            .push(format!("\x1b[90m⏭️\x1b[0m {label:<16} {}", detail.into()));
    }

    fn blank(&mut self) {
        self.lines.push(String::new());
    }

    fn hint(&mut self, text: &str) {
        self.lines.push(format!("    {text}"));
    }

    fn print(&self) {
        for line in &self.lines {
            println!("{line}");
        }
    }
}

fn check_config_dir(report: &mut DoctorReport) {
    let dir = default_tiangong_dir();
    if dir.exists() {
        report.ok("配置目录", dir.display().to_string());
    } else {
        report.err("配置目录", format!("{} 不存在", dir.display()));
    }
}

fn check_models(report: &mut DoctorReport, deep: bool) {
    let models = tiangong_config::io::load_models_config_at(&tiangong_config::io::storage_root());
    let chat = models.resolve_slot(RoutingSlot::Chat);

    match &chat {
        Some(resolved) => {
            report.ok("模型配置", format!("chat -> {}", resolved.model));
        }
        None => {
            report.err("模型配置", "未配置 chat 路由");
            report.blank();
            report.hint("可执行：");
            report.hint("  tiangong model add-provider deepseek \\");
            report.hint("    --protocol deepseek \\");
            report.hint("    --base-url https://api.deepseek.com \\");
            report.hint("    --api-key-env DEEPSEEK_API_KEY");
            report.hint("");
            report.hint("  tiangong model add-model deepseek-chat \\");
            report.hint("    --provider deepseek --model-id deepseek-chat --capability chat");
            report.hint("");
            report.hint("  tiangong model route set chat deepseek-chat");
            report.blank();
        }
    }

    // 模型连通性（深度诊断）
    if deep {
        if let Some(resolved) = chat {
            // 请求前检查 API Key：resolve_slot 已解析 ${ENV}，未设置则返回空串
            if resolved.api_key.trim().is_empty() {
                report.err(
                    "模型连通性",
                    "API Key 为空（${ENV} 环境变量可能未设置），跳过请求",
                );
            } else {
                let endpoint = tiangong_llm::ModelEndpoint::from_resolved(resolved);
                match tiangong_llm::SingleProviderClient::list_models(&endpoint) {
                    Ok(_) => report.ok("模型连通性", format!("{} 请求成功", endpoint.model)),
                    Err(e) => report.err("模型连通性", format!("{e:#}")),
                }
            }
        } else {
            report.skip("模型连通性", "chat 路由未配置，跳过");
        }
    } else {
        report.skip("模型连通性", "使用 --deep 启用");
    }
}

fn check_server(report: &mut DoctorReport) {
    let server = load_server_config();
    report.ok("Server 配置", format!("{}:{}", server.host, server.port));
}

/// 检查模型与 Memory 配置中 `${ENV}` 形式的密钥是否可解析。
fn check_model_secrets(report: &mut DoctorReport) {
    let models = tiangong_config::io::load_models_config_at(&tiangong_config::io::storage_root());
    let mut checked = 0usize;
    let mut missing: Vec<String> = Vec::new();

    // chat 路由 provider 的 api_key
    if let Some(entry) = models.routing.get(&RoutingSlot::Chat)
        && let Some(provider) = models.providers.get(&entry.provider)
    {
        checked += 1;
        let (ok, var) = env_secret_resolvable(&provider.api_key);
        if !ok {
            missing.push(format!("chat 模型（{}）", var.unwrap_or_default()));
        }
    }

    if missing.is_empty() {
        if checked > 0 {
            report.ok("密钥环境变量", format!("{checked} 项密钥均可解析"));
        } else {
            report.skip("密钥环境变量", "未配置需解析的密钥");
        }
    } else {
        report.err(
            "密钥环境变量",
            format!("{} 项环境变量未设置：{}", missing.len(), missing.join("、")),
        );
        report.hint("请在启动 tiangong 前设置对应环境变量");
    }
}

fn check_server_token(report: &mut DoctorReport) {
    let server = load_server_config();
    match &server.auth_token {
        Some(t) if !t.trim().is_empty() => {
            report.ok("Server Token", "已配置");
        }
        _ => {
            report.warn("Server Token", "未配置（接口将无鉴权）");
            report.hint("可执行：tiangong server token generate");
        }
    }
}

fn check_memory(report: &mut DoctorReport) {
    match crate::memory::query_status() {
        Ok(status) => {
            let disabled = status
                .get("disabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let model_name = status
                .get("llm")
                .and_then(|value| value.get("model"))
                .and_then(serde_json::Value::as_str);
            if disabled {
                report.warn("Memory 配置", "已禁用");
            } else if let Some(model_name) = model_name {
                report.ok("Memory 配置", format!("已启用，LLM={model_name}"));
            } else {
                report.warn("Memory 配置", "已启用但 LLM 端点未配置");
                report.hint("可执行：tiangong memory config（打开配置页选择记忆文本模型）");
            }
        }
        Err(error) => report.err("Memory sidecar", error.to_string()),
    }
}

fn check_mcp(report: &mut DoctorReport) {
    // 经 sidecar 通道读取 MCP server 列表。
    let count = tiangong_plugin_runtime::registry::invoke_sidecar(
        &tiangong_config::io::storage_root(),
        "mcp",
        "mcp.server.list",
        serde_json::json!({}),
    )
    .and_then(|v| {
        let resp: tiangong_plugin_mcp_protocol::management::ServersResponse =
            serde_json::from_value(v)?;
        Ok(resp.servers.len())
    })
    .unwrap_or(0);
    if count == 0 {
        report.warn("MCP 配置", "0 个服务");
    } else {
        report.ok("MCP 配置", format!("{count} 个服务可解析"));
    }
}

fn check_skills(report: &mut DoctorReport) {
    // Skill 数据经 skill sidecar 查询。
    let storage_root = tiangong_config::io::storage_root();
    let count = tiangong_plugin_runtime::registry::invoke_sidecar(
        &storage_root,
        "skill",
        tiangong_plugin_skill_protocol::LIST_SKILLS_OPERATION,
        serde_json::to_value(tiangong_plugin_skill_protocol::Empty {}).unwrap_or_default(),
    )
    .ok()
    .and_then(|v| {
        serde_json::from_value::<tiangong_plugin_skill_protocol::ListSkillsResponse>(v).ok()
    })
    .map(|r| r.skills.len())
    .unwrap_or(0);
    if count == 0 {
        report.warn("Skill 目录", "0 个 Skill");
    } else {
        report.ok("Skill 目录", format!("{count} 个 Skill 可用"));
    }
}

fn check_custom_prompt(report: &mut DoctorReport) {
    let path = custom_prompt_path();
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(content) if !content.trim().is_empty() => {
                let chars = content.trim().chars().count();
                report.ok("自定义 Prompt", format!("已配置，{chars} 字"));
                return;
            }
            Ok(_) => {}
            Err(_) => {
                report.err("自定义 Prompt", format!("读取失败：{}", path.display()));
                return;
            }
        }
    }

    let prompt = load_tiangong_config().custom_system_prompt;
    let chars = prompt.trim().chars().count();
    if chars > 0 {
        report.ok("自定义 Prompt", format!("已配置，{chars} 字"));
    } else if path.exists() {
        report.warn("自定义 Prompt", "文件存在但为空");
    } else {
        report.skip("自定义 Prompt", "未配置（可选）");
    }
}
