//! manifest 级视图：slots/贡献/扩展页与图标/资源读取。

use super::*;
use tiangong_core::tools::extension::ToolSpecProvider;

pub fn plugin_manifest(plugin_id: &str) -> Option<PluginManifest> {
    let plugins = loaded_plugins().lock().ok()?;
    let loaded = plugins.get(plugin_id)?;
    loaded.enabled.then(|| loaded.manifest.clone())
}

/// 收集所有已加载 WASM 插件的设置页贡献及其加载代次。
pub fn list_contributions() -> Vec<(String, u64, Vec<Contribution>)> {
    let entries = {
        let Ok(plugins) = loaded_plugins().lock() else {
            return Vec::new();
        };
        plugins
            .iter()
            .filter_map(|(id, loaded)| {
                if !loaded.enabled {
                    return None;
                }
                loaded
                    .ui_plugin
                    .as_ref()
                    .map(|plugin| (id.clone(), loaded.generation, plugin.clone()))
            })
            .collect::<Vec<_>>()
    };
    entries
        .into_iter()
        .filter_map(|(id, generation, plugin)| {
            call_wasm_off_runtime(plugin, WasmPlugin::contributions)
                .ok()
                .map(|contributions| (id, generation, contributions))
        })
        .collect()
}

/// 按 Slot 列出 UI 贡献（宿主 UI 接缝的统一查询入口）。
///
/// - v1 插件：WASM 运行时声明的设置页贡献整体映射到 `settings.plugin-page`，
///   零改动兼容（设计文档 11）。
/// - v2 插件：manifest `ui.contributions` 中 slot 匹配的项。
pub fn list_slot_contributions(slot: &str) -> Vec<SlotContribution> {
    let mut result = Vec::new();
    let Ok(plugins) = loaded_plugins().lock() else {
        return result;
    };
    for (plugin_id, loaded) in plugins.iter() {
        if !loaded.enabled {
            continue;
        }
        // v2：manifest 声明的贡献
        for contribution in loaded.manifest.ui_contributions() {
            if contribution.slot == slot {
                result.push(SlotContribution {
                    plugin_id: plugin_id.clone(),
                    contribution_id: contribution.id.clone(),
                    slot: contribution.slot.clone(),
                    title: contribution.title.clone(),
                    description: contribution.description.clone(),
                    icon: contribution.icon.clone(),
                    group: String::new(),
                    has_view: true,
                    open_mode: contribution.open_mode,
                    sandbox: contribution.sandbox,
                    source: ContributionSource::Manifest,
                    render: contribution.render,
                    tools: exposed_tool_keys(plugin_id, &contribution.tools),
                    titles: exposed_tool_titles(plugin_id, &contribution.titles),
                });
            }
        }
        // v1：WASM 设置页贡献映射到 settings.plugin-page
        if loaded.manifest.schema_version == 1
            && slot == "settings.plugin-page"
            && let Some(ui_plugin) = &loaded.ui_plugin
            && let Ok(contributions) =
                call_wasm_off_runtime(ui_plugin.clone(), WasmPlugin::contributions)
        {
            for contribution in contributions {
                result.push(SlotContribution {
                    plugin_id: plugin_id.clone(),
                    contribution_id: contribution.id.clone(),
                    slot: slot.to_string(),
                    title: contribution.title.clone(),
                    description: contribution.description.clone(),
                    icon: contribution.icon.clone(),
                    group: contribution.group.clone(),
                    has_view: contribution.has_view,
                    open_mode: crate::slots::OpenMode::Singleton,
                    sandbox: crate::slots::SandboxKind::Iframe,
                    source: ContributionSource::Wasm,
                    render: crate::slots::MessageItemRender::Append,
                    tools: Vec::new(),
                    titles: Default::default(),
                });
            }
        }
    }
    result.sort_by(|left, right| {
        left.plugin_id
            .cmp(&right.plugin_id)
            .then(left.contribution_id.cmp(&right.contribution_id))
    });
    result
}

/// 工具键换成对外名（`工具名[:参数=取值]` → `{插件id}__工具名[:参数=取值]`），
/// 与界面拿到的工具调用名一致。
fn exposed_tool_key(plugin_id: &str, key: &str) -> Option<String> {
    let (tool, condition) = match key.split_once(':') {
        Some((tool, condition)) => (tool, Some(condition)),
        None => (key, None),
    };
    let exposed = tiangong_core::tools::extension::namespaced_tool_name(plugin_id, tool)?;
    Some(match condition {
        Some(condition) => format!("{exposed}:{condition}"),
        None => exposed,
    })
}

/// 工具视图贡献的工具键换成对外名。
fn exposed_tool_keys(plugin_id: &str, keys: &[String]) -> Vec<String> {
    keys.iter()
        .filter_map(|key| exposed_tool_key(plugin_id, key))
        .collect()
}

/// 工具视图贡献的标题键换成对外名。
fn exposed_tool_titles(
    plugin_id: &str,
    titles: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    titles
        .iter()
        .filter_map(|(key, title)| Some((exposed_tool_key(plugin_id, key)?, title.clone())))
        .collect()
}

/// 读取 v2 manifest UI 贡献声明的入口 HTML 文件。
///
/// v1 贡献的页面由 WASM `open-view` 提供（见 [`open_view`]），本函数只服务
/// manifest 声明的 `entry`。
pub fn open_manifest_view(plugin_id: &str, contribution_id: &str) -> Result<String> {
    let (directory, entry) = {
        let plugins = loaded_plugins()
            .lock()
            .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
        let loaded = plugins
            .get(plugin_id)
            .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 未加载"))?;
        let contribution = loaded
            .manifest
            .ui_contributions()
            .into_iter()
            .find(|item| item.id == contribution_id)
            .ok_or_else(|| {
                anyhow::anyhow!("插件 {plugin_id} 无 manifest 贡献 {contribution_id}")
            })?;
        (loaded.directory.clone(), contribution.entry)
    };
    let path = directory.join(&entry);
    std::fs::read_to_string(&path).with_context(|| format!("读取插件页面失败: {}", path.display()))
}

/// 读取 v2 manifest UI 贡献的相对资源文件（以 entry 所在目录为根）。
///
/// 供 Shadow/iframe 容器加载入口 HTML 引用的脚本与样式：路径按 Web 相对语义
/// 解析（`./`、子目录），规范化后不得逃出插件安装目录。MIME 按扩展名推断。
pub fn read_manifest_resource(
    plugin_id: &str,
    contribution_id: &str,
    path: &str,
) -> Result<(Vec<u8>, String)> {
    let (directory, entry) = {
        let plugins = loaded_plugins()
            .lock()
            .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
        let loaded = plugins
            .get(plugin_id)
            .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 未加载"))?;
        let contribution = loaded
            .manifest
            .ui_contributions()
            .into_iter()
            .find(|item| item.id == contribution_id)
            .ok_or_else(|| {
                anyhow::anyhow!("插件 {plugin_id} 无 manifest 贡献 {contribution_id}")
            })?;
        (loaded.directory.clone(), contribution.entry)
    };

    let base_dir = directory
        .join(&entry)
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| directory.clone());
    let resource = base_dir.join(path);
    // 规范化后必须仍在插件目录内，拒绝 `../` 逃逸。
    let resolved = resource
        .canonicalize()
        .with_context(|| format!("资源路径无效: {path}"))?;
    let plugin_root = directory
        .canonicalize()
        .with_context(|| format!("插件目录无效: {}", directory.display()))?;
    if !resolved.starts_with(&plugin_root) {
        bail!("插件 {plugin_id} 资源路径 {path} 逃出插件目录，已拒绝");
    }
    let bytes = std::fs::read(&resolved)
        .with_context(|| format!("读取插件资源失败: {}", resolved.display()))?;
    Ok((bytes, mime_of(&resolved)))
}

/// 读取插件 App 的自定义图标（拓展区矩阵渲染用）。
///
/// 以插件安装目录为根；扩展名白名单（png/svg/jpeg/jpg）；大小上限 256KB；
/// 规范化后必须仍在插件目录内，拒绝 `../` 逃逸。图标是 UI 展示资源——
/// 经 `<img>` 渲染（img 中的 SVG 不执行脚本），不进入任何执行路径。
pub fn read_plugin_icon(plugin_id: &str, contribution_id: &str) -> Result<(Vec<u8>, String)> {
    let (directory, icon) = {
        let plugins = loaded_plugins()
            .lock()
            .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
        let loaded = plugins
            .get(plugin_id)
            .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 未加载"))?;
        let contribution = loaded
            .manifest
            .ui_contributions()
            .into_iter()
            .find(|item| item.id == contribution_id)
            .ok_or_else(|| {
                anyhow::anyhow!("插件 {plugin_id} 无 manifest 贡献 {contribution_id}")
            })?;
        (loaded.directory.clone(), contribution.icon.clone())
    };
    read_icon_file(plugin_id, &directory, &icon)
}

/// 读取插件目录内的图标文件：扩展名白名单、256KB 上限、拒绝逃出插件目录。
fn read_icon_file(plugin_id: &str, directory: &Path, icon: &str) -> Result<(Vec<u8>, String)> {
    const MAX_ICON_BYTES: u64 = 256 * 1024;
    let extension = std::path::Path::new(icon)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "jpg" | "jpeg" => "image/jpeg",
        _ => bail!("插件 {plugin_id} 图标 {icon} 扩展名不在白名单（png/svg/jpeg）"),
    };
    let resource = directory.join(icon);
    let resolved = resource
        .canonicalize()
        .with_context(|| format!("图标路径无效: {icon}"))?;
    let plugin_root = directory
        .canonicalize()
        .with_context(|| format!("插件目录无效: {}", directory.display()))?;
    if !resolved.starts_with(&plugin_root) {
        bail!("插件 {plugin_id} 图标路径 {icon} 逃出插件目录，已拒绝");
    }
    let metadata = std::fs::metadata(&resolved)
        .with_context(|| format!("读取图标元数据失败: {}", resolved.display()))?;
    if !metadata.is_file() {
        bail!("插件 {plugin_id} 图标 {icon} 不是普通文件");
    }
    if metadata.len() > MAX_ICON_BYTES {
        bail!(
            "插件 {plugin_id} 图标超过 256KB 上限（{} 字节）",
            metadata.len()
        );
    }
    let bytes = std::fs::read(&resolved)
        .with_context(|| format!("读取图标失败: {}", resolved.display()))?;
    Ok((bytes, mime.to_string()))
}

/// 按扩展名推断资源 MIME（容器加载脚本/样式用，未知类型按二进制流返回）。
fn mime_of(path: &Path) -> String {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// 按 Slot 查询得到的统一 UI 贡献项。
#[derive(Debug, Clone, Serialize)]
pub struct SlotContribution {
    pub plugin_id: String,
    pub contribution_id: String,
    pub slot: String,
    pub title: String,
    pub description: String,
    pub icon: String,
    pub group: String,
    /// 是否有可渲染页面（v1 由 WASM 声明；v2 manifest 贡献恒有）。
    pub has_view: bool,
    pub open_mode: crate::slots::OpenMode,
    pub sandbox: crate::slots::SandboxKind,
    /// 贡献来源：WASM 运行时声明（v1）或 manifest 声明（v2）。
    pub source: ContributionSource,
    /// `session.message-item` 的渲染方式（附加区或替换默认显示）。
    pub render: crate::slots::MessageItemRender,
    /// 接管展示的工具调用：键为工具对外名 `{插件id}__{工具名}`，特指键为
    /// `{插件id}__{工具名}:参数=取值`；界面按调用匹配后挂载本贡献。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// 工具行标题：键同 `tools`（对外名形式），值为插件声明的固定标题。
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub titles: std::collections::BTreeMap<String, String>,
}

/// UI 贡献的声明来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributionSource {
    /// v1：WASM `contributions()` 运行时声明。
    Wasm,
    /// v2：manifest `ui.contributions` 声明。
    Manifest,
}

/// 拓展区 App 元数据：声明 `extension.tab` 贡献的插件即可作为 App 打开
/// （设计文档 6.6）。目录完全由已安装插件的贡献驱动，不在代码里写死；
/// 官方内置能力（浏览器/终端/Agent Team）后续以插件形态注册，装上即出现。
#[derive(Debug, Clone, Serialize)]
pub struct ExtensionApp {
    pub plugin_id: String,
    pub contribution_id: String,
    /// 插件 descriptor 名称（矩阵主标题）。
    pub name: String,
    /// 贡献标题（缺省回落 plugin_id）。
    pub title: String,
    pub description: String,
    pub icon: String,
    /// singleton：全局至多一个 tab，重复打开聚焦；multi：每次打开新建。
    pub open_mode: crate::slots::OpenMode,
    pub sandbox: crate::slots::SandboxKind,
    /// 实例持有后端资源：宿主接管关闭、恢复、核查与会话删除释放。
    pub instance_resources: bool,
}

/// 工具图标查询表（界面工具行使用，见 [`crate::tool_icons`]）。
///
/// 工具名取自已启用插件的 TS 工具声明与已交付适配器的工具规格；插件
/// 声明优先，其次 runtime 内置表，查不到的工具不在表中（前端用默认图标）。
pub fn list_tool_icons() -> std::collections::BTreeMap<String, crate::tool_icons::ToolIcon> {
    let entries = tool_declaring_plugins();
    crate::tool_icons::build_tool_icon_table(entries.iter().map(
        |(id, manifest, tools, _dynamic)| crate::tool_icons::PluginToolIcons {
            plugin_id: id,
            declared: manifest.tool_icons.as_ref(),
            tools: tools.clone(),
        },
    ))
}

/// 工具行标题查询表（见 [`crate::tool_titles`]）：已启用插件在 `tool_titles`
/// 中声明的标题，键为工具对外名与原名；查不到的工具不在表中（前端显示
/// 通用标题「工具调用」）。
pub fn list_tool_titles() -> std::collections::BTreeMap<String, crate::tool_titles::ToolTitle> {
    let entries = tool_declaring_plugins();
    crate::tool_titles::build_tool_title_table(entries.iter().map(
        |(id, manifest, tools, dynamic)| crate::tool_titles::PluginToolTitles {
            plugin_id: id,
            declared: manifest.tool_titles.as_ref(),
            dynamic: dynamic.clone(),
            tools: tools.clone(),
        },
    ))
}

/// 收集已启用插件与各自声明的工具原名（清单 `tools`、TS/WASM 适配器规格，
/// 以及 `tool_icons` / `tool_titles` 特指键归并出的工具名），供展示表建表。
///
/// 读取工具规格不持注册表锁（WASM 适配器首次读取会调用插件）。
/// 工具声明快照：插件 id、清单、工具原名集与 sidecar 动态标题。
type ToolDeclaringPlugin = (
    String,
    PluginManifest,
    Vec<String>,
    std::collections::BTreeMap<String, String>,
);

fn tool_declaring_plugins() -> Vec<ToolDeclaringPlugin> {
    /// 注册表快照：插件 id、清单、动态工具标题与仍存活的 WASM / TS 适配器。
    type PluginSnapshot = (
        String,
        PluginManifest,
        std::collections::BTreeMap<String, String>,
        Vec<Arc<WasmPluginAdapter>>,
        Vec<Arc<TsPluginAdapter>>,
    );
    let snapshot: Vec<PluginSnapshot> = {
        let Ok(plugins) = loaded_plugins().lock() else {
            return Vec::new();
        };
        plugins
            .iter()
            .filter(|(_, loaded)| loaded.enabled)
            .map(|(id, loaded)| {
                // sidecar 动态工具规格中的标题（`tool_specs` 快照 `title` 字段）。
                let dynamic_titles = loaded
                    .dynamic_tool_specs
                    .as_ref()
                    .map(|specs| {
                        specs
                            .iter()
                            .filter_map(|spec| {
                                spec.title.as_ref().map(|t| (spec.name.clone(), t.clone()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (
                    id.clone(),
                    loaded.manifest.clone(),
                    dynamic_titles,
                    loaded.instances.iter().filter_map(Weak::upgrade).collect(),
                    loaded
                        .ts_instances
                        .iter()
                        .filter_map(Weak::upgrade)
                        .collect(),
                )
            })
            .collect()
    };
    // 读取工具规格不持注册表锁（WASM 适配器首次读取会调用插件）。
    snapshot
        .into_iter()
        .map(|(id, manifest, dynamic_titles, wasm, ts)| {
            let mut tools: Vec<String> = manifest
                .tools
                .iter()
                .flatten()
                .map(|tool| tool.name.clone())
                .collect();
            for spec in wasm
                .iter()
                .flat_map(|adapter| adapter.tool_specs())
                .chain(ts.iter().flat_map(|adapter| adapter.tool_specs()))
            {
                if !tools.contains(&spec.name) {
                    tools.push(spec.name);
                }
            }
            for declared in [&manifest.tool_icons, &manifest.tool_titles] {
                let Some(declared) = declared else { continue };
                // 特指键 `工具名:参数=取值` 归到工具名，不单独成为工具。
                for key in declared.keys() {
                    let Some((tool, _)) = crate::tool_icons::parse_tool_key(key) else {
                        continue;
                    };
                    if tool != crate::tool_icons::PLUGIN_DEFAULT_ICON_KEY
                        && !tools.iter().any(|name| name == tool)
                    {
                        tools.push(tool.to_string());
                    }
                }
            }
            (id, manifest, tools, dynamic_titles)
        })
        .collect()
}

/// 读取插件声明的工具图标资源（只允许读取 `tool_icons` 中声明过的路径；
/// 安全约束与 [`read_plugin_icon`] 相同）。
pub fn read_plugin_tool_icon(plugin_id: &str, icon: &str) -> Result<(Vec<u8>, String)> {
    let directory = {
        let plugins = loaded_plugins()
            .lock()
            .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
        let loaded = plugins
            .get(plugin_id)
            .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 未加载"))?;
        if !crate::tool_icons::declares_icon_resource(loaded.manifest.tool_icons.as_ref(), icon) {
            bail!("插件 {plugin_id} 未声明工具图标 {icon}");
        }
        loaded.directory.clone()
    };
    read_icon_file(plugin_id, &directory, icon)
}

/// 列出全部可打开的拓展区 App：聚合已启用插件 manifest 中 slot 为
/// `extension.tab` 的贡献与插件 descriptor 名称。v1 插件无 manifest UI
/// 贡献，不进入 App 列表。
pub fn list_extension_apps() -> Vec<ExtensionApp> {
    let mut apps = Vec::new();
    let Ok(plugins) = loaded_plugins().lock() else {
        return apps;
    };
    for (plugin_id, loaded) in plugins.iter() {
        if !loaded.enabled {
            continue;
        }
        let plugin_name = loaded
            .descriptor
            .as_ref()
            .map(|descriptor| descriptor.name.clone())
            .unwrap_or_else(|| plugin_id.clone());
        for contribution in loaded.manifest.ui_contributions() {
            if contribution.slot != "extension.tab" {
                continue;
            }
            apps.push(ExtensionApp {
                plugin_id: plugin_id.clone(),
                contribution_id: contribution.id.clone(),
                name: plugin_name.clone(),
                title: contribution.title.clone(),
                description: contribution.description.clone(),
                icon: contribution.icon.clone(),
                open_mode: contribution.open_mode,
                sandbox: contribution.sandbox,
                instance_resources: contribution.instance_resources,
            });
        }
    }
    apps.sort_by(|left, right| {
        left.plugin_id
            .cmp(&right.plugin_id)
            .then(left.contribution_id.cmp(&right.contribution_id))
    });
    apps
}

/// 打开插件页面，返回入口 HTML。
pub fn open_view(plugin_id: &str, contribution_id: &str) -> Option<String> {
    let plugin = ui_plugin(plugin_id)?;
    let contribution_id = contribution_id.to_string();
    call_wasm_off_runtime(plugin, move |plugin| plugin.open_view(contribution_id)).ok()
}

/// 获取插件页面资源（字节 + MIME）。
pub fn get_view_resource(plugin_id: &str, path: &str) -> Option<(Vec<u8>, String)> {
    let plugin = ui_plugin(plugin_id)?;
    let path = path.to_string();
    call_wasm_off_runtime(plugin, move |plugin| plugin.get_view_resource(path)).ok()
}

/// 处理插件页面消息（iframe 与插件双向通信）。
pub fn handle_view_message(plugin_id: &str, method: &str, payload: &str) -> Option<String> {
    handle_view_message_result(plugin_id, method, payload).ok()
}

/// 处理插件页面消息并保留 WASM/sidecar 返回的具体错误。
pub fn handle_view_message_result(plugin_id: &str, method: &str, payload: &str) -> Result<String> {
    let plugin = ui_plugin(plugin_id)
        .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 未加载、未启用或没有逻辑层"))?;
    let method = method.to_string();
    let payload = payload.to_string();
    call_wasm_off_runtime(plugin, move |plugin| {
        plugin.handle_view_message(method, payload)
    })
}

fn ui_plugin(plugin_id: &str) -> Option<Arc<Mutex<WasmPlugin>>> {
    let plugins = loaded_plugins().lock().ok()?;
    let loaded = plugins.get(plugin_id)?;
    loaded.enabled.then(|| loaded.ui_plugin.clone()).flatten()
}
