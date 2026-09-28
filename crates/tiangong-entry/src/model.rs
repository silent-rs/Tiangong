use anyhow::{Context, Result, anyhow};

use tiangong_llm::ModelEndpoint;
use tiangong_llm::SingleProviderClient;
use tiangong_llm::models_config::{ModelCapability, ModelEntry, ModelsConfig, RoutingSlot};

use crate::args::{ChatgptSubcommand, ModelArgs, ModelSubcommand, RouteSubcommand};

pub(crate) fn run_model_command(args: ModelArgs) -> Result<()> {
    let dir = tiangong_config::io::storage_root();
    let mut config = tiangong_config::io::load_models_config_at(&dir);
    match args.command {
        ModelSubcommand::List { scope } => {
            print_list(&config, scope.as_deref());
        }
        ModelSubcommand::AddProvider {
            name,
            protocol,
            base_url,
            api_key,
            api_key_env,
            timeout_ms,
        } => {
            let api_key = match (api_key, api_key_env) {
                (Some(key), None) => key,
                (None, Some(env_name)) => format!("${{{env_name}}}"),
                _ => {
                    return Err(anyhow!(
                        "请指定 --api-key（明文）或 --api-key-env（环境变量名），二者互斥"
                    ));
                }
            };
            config.upsert_provider(&name, &base_url, &api_key, protocol, timeout_ms);
            tiangong_config::io::save_models_config_at(&dir, &config)?;
            println!("已保存供应商 {name}");
        }
        ModelSubcommand::RemoveProvider { name, force } => {
            if !config.providers.contains_key(&name) {
                return Err(anyhow!("供应商 {name} 不存在"));
            }
            let refs = config.provider_referenced_by(&name);
            if !refs.models.is_empty() || !refs.routes.is_empty() {
                if !force {
                    eprintln!("供应商 {name} 被以下配置引用：");
                    if !refs.models.is_empty() {
                        eprintln!("  模型：{}", refs.models.join(", "));
                    }
                    if !refs.routes.is_empty() {
                        eprintln!("  路由：{}", refs.routes.join(", "));
                    }
                    return Err(anyhow!("请使用 --force 强制删除，或先移除引用"));
                }
                let removed = config.remove_provider_force(&name);
                tiangong_config::io::save_models_config_at(&dir, &config)?;
                println!("已强制删除供应商 {name}（连带移除 {removed} 项）");
                return Ok(());
            }
            config.providers.remove(&name);
            tiangong_config::io::save_models_config_at(&dir, &config)?;
            println!("已删除供应商 {name}");
        }
        ModelSubcommand::AddModel {
            name,
            provider,
            model_id,
            capability,
        } => {
            if !config.providers.contains_key(&provider) {
                return Err(anyhow!(
                    "供应商 {provider} 不存在，请先 `tiangong model add-provider {provider} ...`"
                ));
            }
            let capabilities = parse_capabilities(&capability)?;
            config.upsert_model(&name, &provider, &model_id, capabilities.clone());
            tiangong_config::io::save_models_config_at(&dir, &config)?;
            let cap_str = if capabilities.is_empty() {
                "（无显式能力）".to_string()
            } else {
                capabilities
                    .iter()
                    .map(|c| c.key())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            println!(
                "已保存模型 {name}（provider={provider}, model_id={model_id}, capability={cap_str}）"
            );
        }
        ModelSubcommand::RemoveModel { name } => {
            let (removed, dangling) = config.remove_model(&name);
            if !removed {
                return Err(anyhow!("模型 {name} 不存在"));
            }
            tiangong_config::io::save_models_config_at(&dir, &config)?;
            if dangling.is_empty() {
                println!("已删除模型 {name}");
            } else {
                println!(
                    "已删除模型 {name}（注意以下路由变为悬空：{}）",
                    dangling.join(",")
                );
            }
        }
        ModelSubcommand::Configure => {
            super::configure::run_model_configure(&mut config)?;
        }
        ModelSubcommand::Route { command } => match command {
            RouteSubcommand::List => print_routes(&config),
            RouteSubcommand::Set { capability, model } => {
                let slot = parse_slot(&capability)?;
                config
                    .set_route_by_name(slot, &model)
                    .map_err(|e| anyhow!(e))?;
                tiangong_config::io::save_models_config_at(&dir, &config)?;
                println!("已设置路由 {capability} -> {model}");
            }
        },
        ModelSubcommand::Validate => {
            validate(&config)?;
            println!("模型配置校验通过");
        }
        ModelSubcommand::Test { target } => {
            test_model(&config, target.as_deref())?;
        }
        ModelSubcommand::Chatgpt { command } => match command {
            ChatgptSubcommand::Login { device } => {
                codex_login(device)?;
                ensure_codex_provider(&mut config);
                match sync_codex_models(&mut config) {
                    Ok(models) => println!(
                        "已同步 {} 个 ChatGPT 模型（chat + multimodal）：{}",
                        models.len(),
                        models.join(", ")
                    ),
                    Err(err) => eprintln!(
                        "拉取 ChatGPT 模型列表失败（{err:#}），可稍后用 `tiangong model add-model` 手动添加"
                    ),
                }
                tiangong_config::io::save_models_config_at(&dir, &config)?;
                println!(
                    "已添加供应商 {}，可用 `tiangong model route set chat <模型>` 设为默认对话模型",
                    super::configure::CODEX_PROVIDER_NAME
                );
            }
            ChatgptSubcommand::Logout => {
                block_on(tiangong_llm::codex_auth::logout())??;
                println!("已退出 ChatGPT 账号");
            }
            ChatgptSubcommand::Status => {
                let status = block_on(tiangong_llm::codex_auth::status())?;
                print_codex_status(&status);
            }
        },
    }
    Ok(())
}

fn block_on<F: std::future::Future>(future: F) -> Result<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("初始化异步运行时失败")?;
    Ok(runtime.block_on(future))
}

fn print_codex_status(status: &tiangong_llm::codex_auth::CodexAuthStatus) {
    if !status.logged_in {
        println!("未登录 ChatGPT 账号（运行 `tiangong model chatgpt login` 登录）");
        return;
    }
    println!(
        "已登录 ChatGPT 账号：{}{}",
        status.email.as_deref().unwrap_or("（未知邮箱）"),
        status
            .plan_type
            .as_deref()
            .map(|plan| format!("（{plan}）"))
            .unwrap_or_default()
    );
}

/// 确保存在固定供应商 ChatGPT（Codex 协议，无 api_key）。
pub(crate) fn ensure_codex_provider(config: &mut ModelsConfig) {
    config.upsert_provider(
        super::configure::CODEX_PROVIDER_NAME,
        tiangong_llm::codex_auth::CODEX_BASE_URL,
        "",
        tiangong_llm::ProviderProtocol::Codex,
        300_000,
    );
}

/// 拉取 ChatGPT 可用模型并注册（chat + multimodal，上下文窗口取服务端声明），返回模型 id 列表。
fn sync_codex_models(config: &mut ModelsConfig) -> Result<Vec<String>> {
    let provider_name = super::configure::CODEX_PROVIDER_NAME;
    let endpoint = ModelEndpoint {
        base_url: tiangong_llm::codex_auth::CODEX_BASE_URL.to_string(),
        protocol: tiangong_llm::ProviderProtocol::Codex,
        timeout_ms: 60_000,
        ..Default::default()
    };
    let models = SingleProviderClient::list_model_infos(&endpoint)?;
    config.register_provider_models(
        provider_name,
        &models,
        &super::configure::default_capabilities(tiangong_llm::ProviderProtocol::Codex),
    );
    Ok(models.into_iter().map(|info| info.id).collect())
}

/// 终端内完成 ChatGPT 账号登录：浏览器回调或设备码。
pub(crate) fn codex_login(device: bool) -> Result<()> {
    block_on(async move {
        let start = if device {
            tiangong_llm::codex_auth::start_device_login().await?
        } else {
            tiangong_llm::codex_auth::start_browser_login().await?
        };
        match start.user_code.as_deref() {
            Some(code) => {
                println!("请在浏览器打开：{}", start.url);
                println!("并输入验证码：{code}（15 分钟内有效）");
            }
            None => {
                println!("请在浏览器中完成 ChatGPT 账号授权：");
                println!("{}", start.url);
                open_browser(&start.url);
            }
        }
        println!("等待授权完成...");
        let status = tiangong_llm::codex_auth::wait_login().await?;
        print_codex_status(&status);
        anyhow::Ok(())
    })?
}

/// 交互式向导内的登录：让用户选择登录方式。
pub(crate) fn codex_login_interactive() -> Result<()> {
    if let Ok(status) = block_on(tiangong_llm::codex_auth::status())
        && status.logged_in
    {
        print_codex_status(&status);
        if !crate::interactive::confirm("是否重新登录？", false)? {
            return Ok(());
        }
    }
    let methods = ["浏览器登录（本机）", "设备码登录（远程 / 无浏览器）"];
    let idx = crate::interactive::select("选择登录方式", &methods)?;
    codex_login(idx == 1)
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(err) = result {
        eprintln!("自动打开浏览器失败（{err}），请手动复制上面的链接");
    }
}

fn print_list(config: &ModelsConfig, scope: Option<&str>) {
    match scope {
        Some("providers") => print_providers(config),
        Some("models") => print_models(config),
        Some("routes") => print_routes(config),
        Some(other) => {
            eprintln!("无效的范围：{other}（可用 providers / models / routes）");
        }
        None => {
            print_providers(config);
            println!();
            print_models(config);
            println!();
            print_routes(config);
        }
    }
}

fn print_providers(config: &ModelsConfig) {
    println!("== Providers ({}) ==", config.providers.len());
    if config.providers.is_empty() {
        println!("（无）");
        return;
    }
    let mut names: Vec<&String> = config.providers.keys().collect();
    names.sort();
    for name in names {
        let p = &config.providers[name];
        println!(
            "{name}  protocol={} base_url={} timeout_ms={}",
            p.protocol.as_str(),
            p.base_url,
            p.timeout_ms
        );
    }
}

fn print_models(config: &ModelsConfig) {
    println!("== Models ({}) ==", config.models.len());
    if config.models.is_empty() {
        println!("（无）");
        return;
    }
    let mut names: Vec<&String> = config.models.keys().collect();
    names.sort();
    for name in names {
        let m = &config.models[name];
        let caps = m
            .capabilities
            .iter()
            .map(|c| c.key())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{name}  provider={} model={} capability={}",
            m.provider, m.model, caps
        );
    }
}

fn print_routes(config: &ModelsConfig) {
    println!("== Routing ==");
    if config.routing.is_empty() {
        println!("（无）");
        return;
    }
    // RoutingSlot 未实现 Ord，按 RoutingSlot::all() 的固定顺序输出
    for slot in RoutingSlot::all() {
        if let Some(entry) = config.routing.get(slot) {
            println!("{}  ->  {} ({})", slot.key(), entry.model, entry.provider);
        }
    }
}

fn validate(config: &ModelsConfig) -> Result<()> {
    let mut errors = Vec::new();

    // 检查路由引用的 provider 是否存在
    for (slot, entry) in &config.routing {
        if !config.providers.contains_key(&entry.provider) {
            errors.push(format!(
                "路由 {} 引用了不存在的 provider {}",
                slot.key(),
                entry.provider
            ));
        }
    }

    // 检查 models 注册项引用的 provider 是否存在
    for (name, entry) in &config.models {
        if !config.providers.contains_key(&entry.provider) {
            errors.push(format!(
                "模型 {name} 引用了不存在的 provider {}",
                entry.provider
            ));
        }
    }

    // chat 路由建议配置
    if !config.has_chat() {
        errors.push("未配置 chat 路由（建议 `tiangong model route set chat <model>`）".to_string());
    }

    if errors.is_empty() {
        Ok(())
    } else {
        for e in &errors {
            eprintln!("❌ {e}");
        }
        Err(anyhow!("模型配置校验未通过（{} 个问题）", errors.len()))
    }
}

fn test_model(config: &ModelsConfig, target: Option<&str>) -> Result<()> {
    // target 为 capability/槽位（如 chat）或模型名；默认 chat
    let target = target.unwrap_or("chat");
    let endpoint = if let Some(slot) = RoutingSlot::from_key(target) {
        // 作为路由槽位解析（chat/lite/multimodal/embedding 等统一走 resolve_slot）
        let resolved = config
            .resolve_slot(slot)
            .ok_or_else(|| anyhow!("路由槽位 {target} 未配置"))?;
        ModelEndpoint::from_resolved(resolved)
    } else {
        // 作为模型名解析
        let entry: &ModelEntry = config
            .models
            .get(target)
            .ok_or_else(|| anyhow!("模型 {target} 不存在，也不是有效路由槽位"))?;
        let provider = config
            .providers
            .get(&entry.provider)
            .ok_or_else(|| anyhow!("模型 {target} 的 provider {} 不存在", entry.provider))?;
        let resolved_api_key = ModelsConfig::resolve_api_key(&provider.api_key);
        ModelEndpoint {
            headers: provider.headers.clone(),
            base_url: provider.base_url.clone(),
            api_key: resolved_api_key,
            model: entry.model.clone(),
            protocol: provider.protocol,
            timeout_ms: provider.timeout_ms,
            options: entry.options.clone(),
            context_window: entry.context_window,
        }
    };

    println!("正在测试 {target} 连通性...");
    // 请求前检查 API Key 非空（${ENV} 未设置会解析为空串，避免无效请求）
    if endpoint.api_key.trim().is_empty() && !endpoint.protocol.uses_oauth() {
        return Err(anyhow!(
            "API Key 为空，可能是环境变量未设置。请检查 models.json 中的 api_key 或设置对应环境变量"
        ));
    }
    let models = SingleProviderClient::list_models(&endpoint).context("模型连通性测试失败")?;
    println!("✅ 连通成功，返回 {} 个模型", models.len());
    if !models.is_empty() {
        let preview: Vec<&str> = models.iter().take(10).map(|s| s.as_str()).collect();
        println!("前 {} 个：{}", preview.len(), preview.join(", "));
    }
    Ok(())
}

fn parse_capabilities(raw: &[String]) -> Result<Vec<ModelCapability>> {
    let mut result = Vec::new();
    for item in raw {
        if tiangong_llm::models_config::RETIRED_MODEL_KEYS.contains(&item.as_str()) {
            return Err(anyhow!(
                "{item} 能力已由 Memory 插件独立管理，请使用 `tiangong memory config` 打开配置页设置"
            ));
        }
        let cap = ModelCapability::from_key(item).ok_or_else(|| anyhow!("无效的能力 {item}"))?;
        if !result.contains(&cap) {
            result.push(cap);
        }
    }
    Ok(result)
}

fn parse_slot(raw: &str) -> Result<RoutingSlot> {
    if tiangong_llm::models_config::RETIRED_MODEL_KEYS.contains(&raw) {
        return Err(anyhow!(
            "{raw} 已由 Memory 插件独立管理，请使用 `tiangong memory config` 打开配置页设置"
        ));
    }
    RoutingSlot::from_key(raw).ok_or_else(|| {
        anyhow!("无效的路由槽位 {raw}（可用 chat/lite/multimodal/image_generation/video_generation/stt/tts）")
    })
}
