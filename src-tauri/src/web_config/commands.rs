//! 浏览器配置页的命令分发：按 Tauri 命令名实现设置页用到的命令子集。
//!
//! 前端组件在浏览器中经 `POST api/invoke/<命令名>` 调用，参数与 Tauri invoke
//! 完全一致（camelCase）。这里不依赖 Tauri 运行时状态：每次从磁盘读取配置、
//! 写盘后刷新进程级配置单例。配置页由用户本人临时打开，关闭即退出，
//! 因此不做额外的访问授权与密钥脱敏，行为与桌面设置页一致。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tiangong_llm::models_config::{ModelCapability, ModelsConfig};
use tiangong_llm::{ModelEndpoint, ProviderProtocol, SingleProviderClient};
use tiangong_plugin_runtime::registry::{self, RuntimeKind};

use crate::view::{ModelCapabilityInfo, ModelsConfigView, ServerConfigView};

/// 插件配置页可用的桥接命名空间（与桌面 PluginIframe 一致；配置页没有会话，
/// 不提供 sidecar/终端等依赖会话的宿主能力）。
const PAGE_BRIDGE_NAMESPACES: &[&str] = &["plugin.", "storage.", "session.", "tool."];

/// 命令错误：HTTP 状态码 + 文本（前端按 Tauri 语义当作字符串错误抛出）。
#[derive(Debug)]
pub(crate) struct CommandError {
    pub status: u16,
    pub message: String,
}

impl CommandError {
    pub(super) fn bad(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }
}

impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self::bad(message)
    }
}

impl From<anyhow::Error> for CommandError {
    fn from(error: anyhow::Error) -> Self {
        Self::bad(format!("{error:#}"))
    }
}

pub(super) type CommandResult = Result<Value, CommandError>;

/// 配置页运行期上下文。
pub(crate) struct WebConfigContext {
    dir: PathBuf,
    /// 是否监听在非回环地址：浏览器回调式登录在远程时不可用。
    remote: bool,
    /// Bot 管理（`bots.json` 加载失败时为 None，Bot 命令返回错误）。
    bots: Result<super::bots::BotContext, String>,
}

impl WebConfigContext {
    pub(crate) fn new(dir: PathBuf, remote: bool) -> Self {
        let bots =
            super::bots::BotContext::new().map_err(|error| format!("加载 Bot 配置失败：{error:#}"));
        Self { dir, remote, bots }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

fn load_models(dir: &Path) -> ModelsConfig {
    tiangong_config::io::load_models_config_at(dir)
}

pub(super) fn parse<T: DeserializeOwned>(args: Value) -> Result<T, CommandError> {
    serde_json::from_value(args).map_err(|error| CommandError::bad(format!("参数无效：{error}")))
}

pub(super) fn to_json<T: serde::Serialize>(value: T) -> CommandResult {
    serde_json::to_value(value).map_err(|error| CommandError::bad(error.to_string()))
}

async fn blocking<T, F>(task: F) -> Result<T, CommandError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|error| CommandError::bad(format!("后台任务失败：{error}")))?
        .map_err(CommandError::from)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchModelsArgs {
    base_url: String,
    api_key: String,
    timeout_ms: Option<u64>,
    protocol: Option<String>,
    headers: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginIdArgs {
    plugin_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContributionArgs {
    plugin_id: String,
    contribution_id: String,
}

/// 按 Tauri 命令名分发。未实现的命令返回 404，前端按普通错误处理。
pub(crate) async fn dispatch(ctx: &WebConfigContext, command: &str, args: Value) -> CommandResult {
    if command.starts_with("bot_") {
        let bots = ctx
            .bots
            .as_ref()
            .map_err(|error| CommandError::bad(error.clone()))?;
        if let Some(result) = super::bots::dispatch(bots, command, args.clone()).await {
            return result;
        }
    }
    let dir = ctx.dir().to_path_buf();
    match command {
        // ── 智能体 / 通用 ──
        "get_default_trust_mode" => {
            let config = tiangong_config::load_tiangong_config_from_dir(&dir);
            let mode = serde_json::to_value(config.default_trust_mode).unwrap_or_default();
            Ok(Value::String(
                mode.as_str().unwrap_or("full_trust").to_string(),
            ))
        }
        "set_default_trust_mode" => {
            #[derive(Deserialize)]
            struct Args {
                mode: String,
            }
            let Args { mode } = parse(args)?;
            let trust_mode: tiangong_core::permission::TrustMode =
                serde_json::from_value(Value::String(mode))
                    .map_err(|e| CommandError::bad(format!("无效的默认信任模式: {e}")))?;
            let mut config = tiangong_config::load_tiangong_config_from_dir(&dir);
            config.default_trust_mode = trust_mode;
            tiangong_config::registry::update(config)?;
            Ok(Value::Null)
        }
        "get_workspace_dir" => {
            let config = tiangong_config::load_tiangong_config_from_dir(&dir);
            Ok(Value::String(config.workspace_dir))
        }
        "set_workspace_dir" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                workspace_dir: String,
            }
            let Args { workspace_dir } = parse(args)?;
            if !Path::new(&workspace_dir).is_dir() {
                return Err(CommandError::bad(format!(
                    "路径不存在或不是目录：{workspace_dir}"
                )));
            }
            let mut config = tiangong_config::load_tiangong_config_from_dir(&dir);
            config.workspace_dir = workspace_dir;
            tiangong_config::registry::update(config)?;
            Ok(Value::Null)
        }

        // ── 模型配置 ──
        "get_models_config" => to_json(ModelsConfigView::from_core(&load_models(&dir))),
        "set_models_config" => {
            #[derive(Deserialize)]
            struct Args {
                config: ModelsConfigView,
            }
            let Args { config } = parse(args)?;
            let current = tiangong_config::load_tiangong_config_from_dir(&dir);
            tiangong_config::registry::update_models(&current, config.to_core())?;
            Ok(Value::Null)
        }
        "get_model_capabilities" => {
            let caps: Vec<ModelCapabilityInfo> = ModelCapability::all()
                .iter()
                .map(|c| ModelCapabilityInfo {
                    key: serde_json::to_value(c)
                        .ok()
                        .and_then(|key| key.as_str().map(str::to_string))
                        .unwrap_or_default(),
                    display_name: c.display_name().to_string(),
                })
                .collect();
            to_json(caps)
        }
        "fetch_provider_models" | "fetch_provider_model_infos" => {
            let req: FetchModelsArgs = parse(args)?;
            let endpoint = ModelEndpoint {
                headers: req.headers.unwrap_or_default(),
                base_url: req.base_url,
                api_key: ModelsConfig::resolve_api_key(&req.api_key),
                model: String::new(),
                protocol: req
                    .protocol
                    .as_deref()
                    .and_then(|value| value.parse::<ProviderProtocol>().ok())
                    .unwrap_or_default(),
                timeout_ms: req.timeout_ms.unwrap_or(60_000),
                options: json!({}),
                context_window: None,
            };
            if command == "fetch_provider_models" {
                to_json(
                    SingleProviderClient::list_models_async(&endpoint)
                        .await
                        .map_err(|e| CommandError::bad(e.to_string()))?,
                )
            } else {
                to_json(
                    SingleProviderClient::list_model_infos_async(&endpoint)
                        .await
                        .map_err(|e| CommandError::bad(e.to_string()))?,
                )
            }
        }
        "resolve_model_context_window" => {
            #[derive(Deserialize)]
            struct Args {
                model: String,
            }
            let Args { model } = parse(args)?;
            to_json(tiangong_config::io::resolve_context_limit_at(&dir, &model))
        }
        "get_provider_balance" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                provider_name: String,
            }
            let Args { provider_name } = parse(args)?;
            let models = load_models(&dir);
            let provider = models
                .providers
                .get(&provider_name)
                .ok_or_else(|| CommandError::bad(format!("Provider '{provider_name}' 不存在")))?;
            let key = ModelsConfig::resolve_api_key(&provider.api_key);
            crate::commands::query_provider_balance(&provider.base_url, &key)
                .await
                .map_err(CommandError::from)
        }

        // ── ChatGPT 账号 ──
        "codex_auth_status" => to_json(tiangong_llm::providers::codex::status().await),
        "codex_auth_start" => {
            #[derive(Deserialize)]
            struct Args {
                method: Option<String>,
            }
            let Args { method } = parse(args)?;
            let device = method.as_deref() == Some("device");
            if ctx.remote && !device {
                return Err(CommandError::bad(
                    "远程配置时浏览器回调无法到达服务器，请改用设备码登录",
                ));
            }
            // 授权页由浏览器页面自行打开（服务端可能没有图形环境）。
            let start = if device {
                tiangong_llm::providers::codex::start_device_login().await
            } else {
                tiangong_llm::providers::codex::start_browser_login().await
            }?;
            to_json(start)
        }
        "codex_auth_wait" => to_json(tiangong_llm::providers::codex::wait_login().await?),
        "codex_auth_cancel" => {
            tiangong_llm::providers::codex::cancel_login().await;
            to_json(tiangong_llm::providers::codex::status().await)
        }
        "codex_auth_logout" => to_json(tiangong_llm::providers::codex::logout().await?),
        "codex_auth_refresh" => to_json(tiangong_llm::providers::codex::refresh_now().await?),
        "codex_auth_usage" => to_json(
            tiangong_llm::providers::codex::usage()
                .await
                .map_err(|error| CommandError::bad(error.to_string()))?,
        ),

        // ── Server ──
        "get_server_config" => {
            let config = tiangong_config::load_server_config_from_dir(&dir);
            let running = crate::commands::server_health_check(&config);
            to_json(ServerConfigView {
                status: crate::commands::server_status_name(config.enabled, running).to_string(),
                auth_token_masked: config.masked_auth_token(),
                host: config.host,
                port: config.port,
                enabled: config.enabled,
                running,
            })
        }
        "set_server_config" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                host: String,
                port: u16,
                auth_token: Option<String>,
            }
            let req: Args = parse(args)?;
            let mut config = tiangong_config::load_server_config_from_dir(&dir);
            config.host = req.host;
            config.port = req.port;
            if let Some(token) = req
                .auth_token
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
            {
                config.auth_token = Some(token);
            }
            tiangong_config::save_server_config_to_dir(&dir, &config)?;
            to_json("Server 配置已保存（运行中的 Server 需重启后生效）")
        }

        // ── 插件管理 ──
        "list_plugins" => {
            let statuses =
                blocking(move || Ok(registry::list_plugins(&dir, RuntimeKind::Cli))).await?;
            to_json(statuses)
        }
        "list_available_plugins" => {
            let repository = tiangong_plugin_runtime::artifacts::PluginRepository::new()?;
            to_json(repository.list_available(&dir).await?)
        }
        "install_plugin" | "upgrade_plugin" => {
            let PluginIdArgs { plugin_id } = parse(args)?;
            to_json(
                tiangong_plugin_runtime::artifacts::install_plugin_from_repository(
                    &dir, &plugin_id, None,
                )
                .await?,
            )
        }
        "set_plugin_enabled" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                plugin_id: String,
                enabled: bool,
            }
            let req: Args = parse(args)?;
            to_json(
                blocking(move || registry::set_plugin_enabled(&dir, &req.plugin_id, req.enabled))
                    .await?,
            )
        }
        "rollback_plugin" => {
            let PluginIdArgs { plugin_id } = parse(args)?;
            to_json(blocking(move || registry::rollback_plugin(&dir, &plugin_id)).await?)
        }
        "reload_plugin" => {
            let PluginIdArgs { plugin_id } = parse(args)?;
            to_json(blocking(move || registry::reload_plugin(&dir, &plugin_id)).await?)
        }
        "uninstall_plugin" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                plugin_id: String,
                keep_data: bool,
            }
            let req: Args = parse(args)?;
            blocking(move || registry::uninstall_plugin(&dir, &req.plugin_id, req.keep_data))
                .await?;
            Ok(Value::Null)
        }
        "import_local_plugin" => {
            #[derive(Deserialize)]
            struct Args {
                path: String,
            }
            let Args { path } = parse(args)?;
            let source = PathBuf::from(path.trim());
            to_json(
                blocking(move || {
                    let staged = if source
                        .extension()
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| value.eq_ignore_ascii_case("zst"))
                    {
                        tiangong_plugin_runtime::artifacts::stage_plugin_archive(&dir, &source)?
                    } else {
                        tiangong_plugin_runtime::artifacts::stage_local_plugin(&dir, &source)?
                    };
                    registry::import_staged_plugin(&dir, staged.path())
                })
                .await?,
            )
        }
        "plugin_list_trusted_publishers" => to_json(
            blocking(move || tiangong_plugin_runtime::trust::list_trusted_publishers(&dir)).await?,
        ),
        "plugin_import_trusted_publisher" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                publisher: String,
                public_key: String,
            }
            let req: Args = parse(args)?;
            to_json(
                blocking(move || {
                    tiangong_plugin_runtime::trust::import_trusted_publisher(
                        &dir,
                        &req.publisher,
                        &req.public_key,
                    )
                })
                .await?,
            )
        }
        "plugin_remove_trusted_publisher" => {
            #[derive(Deserialize)]
            struct Args {
                publisher: String,
            }
            let Args { publisher } = parse(args)?;
            to_json(
                blocking(move || {
                    tiangong_plugin_runtime::trust::remove_trusted_publisher(&dir, &publisher)
                })
                .await?,
            )
        }
        "plugin_read_public_key_file" => {
            #[derive(Deserialize)]
            struct Args {
                path: String,
            }
            let Args { path } = parse(args)?;
            to_json(crate::commands::plugin_read_public_key_file(path).await?)
        }
        "plugin_user_key_fingerprint" => to_json(
            tiangong_plugin_runtime::trust::user_public_key_b64(&dir)
                .ok()
                .and_then(|key| tiangong_plugin_runtime::trust::publisher_fingerprint(&key).ok()),
        ),

        // ── 插件配置页（settings.plugin-page） ──
        "list_slot_contributions" => {
            #[derive(Deserialize)]
            struct Args {
                slot: String,
            }
            let Args { slot } = parse(args)?;
            to_json(registry::list_slot_contributions(&slot))
        }
        "plugin_open_view" => {
            let req: ContributionArgs = parse(args)?;
            to_json(
                registry::open_view(&req.plugin_id, &req.contribution_id).ok_or_else(|| {
                    CommandError::bad(format!("插件 {} 未加载或无页面", req.plugin_id))
                })?,
            )
        }
        "plugin_open_entry" => {
            let req: ContributionArgs = parse(args)?;
            to_json(registry::open_manifest_view(
                &req.plugin_id,
                &req.contribution_id,
            )?)
        }
        "plugin_read_entry_resource" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                plugin_id: String,
                contribution_id: String,
                path: String,
            }
            let req: Args = parse(args)?;
            let (data, mime) =
                registry::read_manifest_resource(&req.plugin_id, &req.contribution_id, &req.path)?;
            Ok(json!({ "data": data, "mime": mime }))
        }
        "bridge_call" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                plugin_id: String,
                method: String,
                payload: String,
            }
            let req: Args = parse(args)?;
            if !PAGE_BRIDGE_NAMESPACES
                .iter()
                .any(|prefix| req.method.starts_with(prefix))
            {
                return Err(CommandError::bad(format!(
                    "配置页不支持调用 {}",
                    req.method
                )));
            }
            to_json(
                blocking(move || {
                    tiangong_plugin_runtime::bridge_call(&req.plugin_id, &req.method, &req.payload)
                })
                .await?,
            )
        }
        // 配置页不推送宿主事件，订阅为空操作。
        "bridge_subscribe" | "bridge_unsubscribe" => Ok(Value::Null),

        other => Err(CommandError {
            status: 404,
            message: format!("配置页不支持命令 {other}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(remote: bool) -> (tempfile::TempDir, WebConfigContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = WebConfigContext::new(dir.path().to_path_buf(), remote);
        (dir, ctx)
    }

    fn run(ctx: &WebConfigContext, command: &str, args: Value) -> CommandResult {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(dispatch(ctx, command, args))
    }

    #[test]
    fn models_config_round_trip() {
        let (dir, ctx) = context(false);
        let config = json!({
            "providers": {"o": {"base_url": "https://api.example.com/v1", "api_key": "sk-test", "timeout_ms": 30000, "protocol": "openai"}},
            "models": {},
            "routing": {}
        });
        run(&ctx, "set_models_config", json!({ "config": config })).expect("保存");
        let loaded = run(&ctx, "get_models_config", json!({})).expect("读取");
        assert_eq!(loaded["providers"]["o"]["api_key"], "sk-test");
        assert_eq!(load_models(dir.path()).providers["o"].timeout_ms, 30_000);
    }

    #[test]
    fn trust_mode_workspace_and_server() {
        let (dir, ctx) = context(false);
        run(&ctx, "set_default_trust_mode", json!({"mode":"supervised"})).unwrap();
        assert_eq!(
            run(&ctx, "get_default_trust_mode", json!({})).unwrap(),
            "supervised"
        );
        let workspace = dir.path().to_string_lossy().to_string();
        run(
            &ctx,
            "set_workspace_dir",
            json!({"workspaceDir": workspace}),
        )
        .unwrap();
        assert!(run(
            &ctx,
            "set_workspace_dir",
            json!({"workspaceDir":"/no/such/dir"})
        )
        .is_err());

        run(
            &ctx,
            "set_server_config",
            json!({"host":"0.0.0.0","port":9000,"authToken":"tg_token_value"}),
        )
        .unwrap();
        assert_eq!(
            run(&ctx, "get_server_config", json!({})).unwrap()["port"],
            9000
        );
        // 不传 Token 时保留原值（与桌面端一致）。
        run(
            &ctx,
            "set_server_config",
            json!({"host":"127.0.0.1","port":9001}),
        )
        .unwrap();
        let saved = tiangong_config::load_server_config_from_dir(dir.path());
        assert_eq!(saved.auth_token.as_deref(), Some("tg_token_value"));
    }

    #[test]
    fn unsupported_commands() {
        let (_dir, ctx) = context(false);
        assert_eq!(
            run(&ctx, "send_message", json!({})).unwrap_err().status,
            404
        );
        assert!(run(
            &ctx,
            "bridge_call",
            json!({"pluginId":"none","method":"sidecar.invoke","payload":""})
        )
        .is_err());
        let (_dir, remote) = context(true);
        assert!(run(&remote, "codex_auth_start", json!({"method":"browser"})).is_err());
    }
}
