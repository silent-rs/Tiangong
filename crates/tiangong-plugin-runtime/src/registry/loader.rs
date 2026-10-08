//! 查询视图（列表/状态/清单）、WASM 编译装载与 Core 适配器创建。

use super::connections::{
    loaded_plugin_matches, stop_connection_for_directory, stop_loaded_sidecar,
};
use super::*;

/// 当前应进入 Core 的插件 id 列表（按 `load_installed_plugins` 同一套
/// 可用性过滤与排序：入口声明、`model_requirements` 能力、prompt 置顶）。
///
/// 供宿主的差量插件视图使用（`RuntimePluginSource`）：先取 id 集合做
/// 增删判定，再对新增 id 调 [`load_core_plugin`] 创建适配器。
pub fn core_plugin_ids(_storage_root: &Path, runtime: RuntimeKind) -> Vec<String> {
    let configured = configured_model_capabilities();
    let mut plugin_ids = {
        let Ok(plugins) = loaded_plugins().lock() else {
            return Vec::new();
        };
        plugins
            .values()
            .filter_map(|loaded| {
                if let Some(reason) =
                    check_plugin_availability(&loaded.manifest, runtime, &configured)
                {
                    tracing::info!(
                        plugin_id = %loaded.manifest.id,
                        reason,
                        "插件未注册工具（仍保持已安装）"
                    );
                    None
                } else {
                    Some(loaded.manifest.id.clone())
                }
            })
            .collect::<Vec<_>>()
    };
    plugin_ids.sort_by(|left, right| {
        (left != "prompt")
            .cmp(&(right != "prompt"))
            .then_with(|| left.cmp(right))
    });
    plugin_ids
}

/// 为一个会话直接创建全部已安装插件的独立适配器（不经聚合桥）。
///
/// 各入口构造 Core 时统一经 [`crate::RuntimeCorePlugin`] 接入插件；本函数
/// 保留给需要直接拿到单个适配器的测试与工具场景。实例使用注册表已接受的
/// 同一份 WASM 字节快照。
///
/// `runtime` 用于按入口过滤：插件声明了 `entrypoints` 但不含当前入口时不注册。
/// 同时按 `model_requirements` 过滤：必需模型能力未配置时不注册工具（插件保持已安装）。
pub fn load_installed_plugins(
    _storage_root: &Path,
    runtime: RuntimeKind,
    session_id: &str,
) -> Vec<Arc<dyn Plugin>> {
    let Ok(_operation) = LOAD_OPERATION.write() else {
        tracing::warn!("插件加载操作锁已损坏");
        return Vec::new();
    };

    core_plugin_ids(_storage_root, runtime)
        .into_iter()
        .filter_map(|plugin_id| load_core_plugin(&plugin_id, runtime, session_id))
        .collect()
}

/// 返回已安装插件状态，并探测 sidecar 当前是否可用。
///
/// `runtime` 用于判断插件是否可在当前入口注册工具，填充 `unavailable_reason`。
pub fn list_plugins(_storage_root: &Path, runtime: RuntimeKind) -> Vec<PluginStatus> {
    let configured = configured_model_capabilities();
    let Ok(plugins) = loaded_plugins().lock() else {
        return Vec::new();
    };
    let mut statuses = plugins
        .values()
        .map(|loaded| {
            let manifest = &loaded.manifest;
            let sidecar_running = loaded
                .sidecar
                .as_ref()
                .is_some_and(|connection| connection.has_runtime_endpoint());
            let state = plugin_state(
                manifest,
                loaded.enabled,
                loaded.ui_plugin.is_some(),
                loaded
                    .load_error
                    .as_deref()
                    .or(loaded.runtime_error.as_deref()),
            );
            PluginStatus {
                unavailable_reason: if loaded.enabled {
                    check_plugin_availability(manifest, runtime, &configured)
                } else {
                    None
                },
                id: manifest.id.clone(),
                // 展示名优先级：清单 name（静态声明）→ WASM descriptor → id 兜底。
                name: manifest
                    .name
                    .clone()
                    .or_else(|| loaded.descriptor.as_ref().map(|value| value.name.clone()))
                    .unwrap_or_else(|| manifest.id.clone()),
                description: manifest.description.clone(),
                manifest_version: manifest.version.clone(),
                loaded_version: loaded
                    .descriptor
                    .as_ref()
                    .map(|value| value.version.clone()),
                state: state.to_string(),
                generation: loaded.generation,
                enabled: loaded.enabled,
                can_rollback: rollback_directory(&loaded.directory, &manifest.id).is_dir(),
                has_sidecar: manifest.sidecar.is_some(),
                sidecar_running,
                last_error: loaded
                    .load_error
                    .clone()
                    .or_else(|| loaded.runtime_error.clone()),
            }
        })
        .collect::<Vec<_>>();
    // 无效插件（签名无效/沙箱越权/清单损坏）以 invalid 状态并列展示，供用户清理。
    if let Ok(invalid) = invalid_plugins().lock() {
        for entry in invalid.iter() {
            statuses.push(PluginStatus {
                id: entry.id.clone(),
                name: entry.name.clone(),
                description: None,
                manifest_version: entry.manifest_version.clone().unwrap_or_default(),
                loaded_version: None,
                state: "invalid".to_string(),
                generation: 0,
                enabled: false,
                can_rollback: false,
                has_sidecar: false,
                sidecar_running: false,
                last_error: Some(entry.reason.clone()),
                unavailable_reason: None,
            });
        }
    }
    statuses.sort_by(|left, right| left.id.cmp(&right.id));
    statuses
}

/// 从磁盘读取插件新版本。全部 UI/Core 实例成功创建后才切换。
///
/// 迁移成功（含制品未变化的幂等短路）即广播插件变化事件。
pub fn reload_plugin(storage_root: &Path, plugin_id: &str) -> Result<PluginStatus> {
    let result = reload_plugin_entry(storage_root, plugin_id);
    crate::events::announced(PluginChangeKind::Upgraded, plugin_id, result)
}

fn reload_plugin_entry(storage_root: &Path, plugin_id: &str) -> Result<PluginStatus> {
    let _operation = LOAD_OPERATION
        .write()
        .map_err(|_| anyhow::anyhow!("插件加载操作锁已损坏"))?;
    let installed = find_installed_plugin(storage_root, plugin_id)?;
    if loaded_plugin_matches(&installed)? {
        return list_plugin_status_without_preload(&installed.manifest)
            .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 当前状态丢失"));
    }

    let result = reload_plugin_inner(storage_root, &installed);
    if let Err(error) = &result {
        set_load_error(plugin_id, error.to_string());
    }
    result?;

    list_plugin_status_without_preload(&installed.manifest)
        .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 热加载后状态丢失"))
}

pub(super) fn reload_plugin_inner(storage_root: &Path, installed: &InstalledPlugin) -> Result<()> {
    // 无 WASM 插件可能仅提供 UI，也可能通过 Desktop TS 工具适配器接入 Core。
    // UI 记录直接替换；存活 Core 中的 TS 适配器原位更新，下一轮立即使用新清单。
    if installed.manifest.wasm_binary().is_none() {
        crate::ts_tools::cancel_plugin_calls(&installed.manifest.id);
        // 停掉旧 sidecar（进程 + 该安装目录的连接缓存）：热加载的前提是
        // 插件目录可能已被整体替换，残留会让工具继续打到旧二进制；新清
        // 单即使去掉 sidecar 声明也照清（此时旧连接/旧进程正是要清除的
        // 残留，Windows 上还会占用目录导致替换失败）。两步独立执行、各
        // 自告警：停进程失败不能阻断连接清理，否则下次调用会复用表内
        // 旧连接。桥订阅有意不清——可见标签里的旧页面仍可接应工具调用
        //（执行走重启后的 sidecar），后台执行壳则由前端收到
        // plugin_reloaded 后卸载重建。
        if let Err(error) = stop_loaded_sidecar(&installed.manifest.id) {
            tracing::warn!(
                plugin_id = %installed.manifest.id,
                %error,
                "热加载停止 sidecar 失败，工具调用将沿用旧进程"
            );
        }
        if let Err(error) = stop_connection_for_directory(&installed.directory) {
            tracing::warn!(
                plugin_id = %installed.manifest.id,
                %error,
                "热加载清理 sidecar 连接缓存失败，下次调用可能复用旧连接"
            );
        }
        let ts_instances = loaded_plugins()
            .lock()
            .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?
            .get(&installed.manifest.id)
            .map(|loaded| {
                loaded
                    .ts_instances
                    .iter()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for adapter in &ts_instances {
            let verified_sidecar = crate::verification::load_valid_capabilities(
                &installed.directory,
                &installed.manifest,
            );
            adapter.reconfigure(&installed.manifest, installed.enabled, verified_sidecar);
        }
        let mut loaded = load_plugin_record(storage_root, installed.clone());
        loaded.ts_instances = ts_instances.iter().map(Arc::downgrade).collect();
        loaded_plugins()
            .lock()
            .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?
            .insert(installed.manifest.id.clone(), loaded);
        tracing::info!(plugin_id = %installed.manifest.id, "无 WASM 插件已重新加载");
        return Ok(());
    }

    let wasm_bytes = Arc::new(read_wasm_bytes(installed)?);
    let sidecar = resolve_sidecar(storage_root, installed, true)?;
    let verified_sidecar =
        crate::verification::load_valid_capabilities(&installed.directory, &installed.manifest);
    // 仅预热原生常驻 sidecar。解释器即使脚本声明常驻，也等首次实际
    // 使用时才启动；command 继续由每次调用的独立连接承载。
    // 启动失败是运行异常：不阻断重载/升级（安装前提类失败已在上方
    // read_wasm_bytes/resolve_sidecar 以 Err 返回），登记后由插件管理
    // 显示启动异常。
    let mut preload_error = None;
    if installed.enabled
        && installed.manifest.should_preload_sidecar()
        && verified_sidecar.is_some()
        && let Some(connection) = &sidecar
        && let Err(error) = connection.ensure_running()
    {
        tracing::warn!(plugin_id = %installed.manifest.id, %error, "插件 sidecar 暂不可用");
        preload_error = Some(error.to_string());
    }
    let (instances, next_generation) = {
        let plugins = loaded_plugins()
            .lock()
            .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
        let loaded = plugins
            .get(&installed.manifest.id)
            .ok_or_else(|| anyhow::anyhow!("插件 {} 尚未预加载", installed.manifest.id))?;
        (
            loaded
                .instances
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>(),
            loaded.generation.saturating_add(1).max(1),
        )
    };

    let runtime_config = PluginRuntimeConfig::default();
    let (component, ui_plugin, descriptor) = compile_plugin(
        wasm_bytes.clone(),
        &installed.manifest,
        sidecar.clone(),
        runtime_config,
    )?;

    let mut replacements = Vec::with_capacity(instances.len());
    for adapter in &instances {
        let component = component.clone().expect("热加载替换仅适用于带逻辑层的插件");
        let plugin = instantiate_from_compiled(
            component,
            sidecar.clone(),
            adapter.runtime_config(),
            installed.manifest.id.clone(),
            installed.manifest.storage_access,
        )?;
        let adapter = adapter.clone();
        let activate = installed.enabled;
        let replacement = crate::execution::run_outside_tokio(move || {
            adapter.prepare_replacement(plugin, activate)
        })?;
        replacements.push(replacement);
    }

    for (adapter, replacement) in instances.iter().zip(replacements) {
        adapter.replace_inner(replacement);
        adapter.set_enabled(installed.enabled);
    }

    let mut plugins = loaded_plugins()
        .lock()
        .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
    let loaded = plugins
        .get_mut(&installed.manifest.id)
        .ok_or_else(|| anyhow::anyhow!("插件 {} 在切换前被移除", installed.manifest.id))?;
    loaded.directory = installed.directory.clone();
    loaded.manifest = installed.manifest.clone();
    loaded.wasm_bytes = Some(wasm_bytes);
    loaded.component = component;
    loaded.ui_plugin = ui_plugin.map(|plugin| Arc::new(Mutex::new(plugin)));
    loaded.descriptor = descriptor;
    loaded.generation = next_generation;
    loaded.instances = instances.iter().map(Arc::downgrade).collect();
    loaded.sidecar = sidecar;
    loaded.verified_sidecar = verified_sidecar;
    loaded.load_error = None;
    loaded.runtime_error = preload_error;
    loaded.enabled = installed.enabled;
    tracing::info!(
        plugin_id = %installed.manifest.id,
        generation = next_generation,
        instances = instances.len(),
        "WASM 插件热加载完成"
    );
    Ok(())
}

/// 通过插件 ID 调用其 sidecar，入口不需要了解制品位置或传输协议。
pub fn invoke_sidecar(
    storage_root: &Path,
    plugin_id: &str,
    operation: &str,
    payload: serde_json::Value,
) -> Result<serde_json::Value> {
    invoke_sidecar_with_workspace(storage_root, plugin_id, operation, payload, None)
}

/// 使用宿主权威会话工作区调用 sidecar。
///
/// `workspace` 独立于插件业务负载，不能由 payload 中的 `cwd` 或全局活跃
/// 工作区替代。按需启动的连接按规范化后的工作区隔离，避免不同会话串用权限；
/// 随应用预热并持续运行的连接仍使用原有通用可写域。
pub fn invoke_sidecar_with_workspace(
    storage_root: &Path,
    plugin_id: &str,
    operation: &str,
    payload: serde_json::Value,
    workspace: Option<&Path>,
) -> Result<serde_json::Value> {
    let installed = find_installed_plugin(storage_root, plugin_id)?;
    if !installed.enabled {
        bail!("插件 {plugin_id} 已停用");
    }
    let workspace = if installed.manifest.should_preload_sidecar() {
        None
    } else {
        workspace
    };
    let connection = sidecar_connection_with_workspace(storage_root, &installed, false, workspace)?;
    let payload = serde_json::to_string(&payload).with_context(|| "序列化插件请求失败")?;
    let response = connection.invoke(operation, &payload)?;
    serde_json::from_str(&response).with_context(|| "解析插件响应失败")
}

/// 宿主注入 CoreManager 的 @提及候选来源。
///
/// 每个已启用插件一个来源，只用于候选查询：不实现 `Plugin`、不占插件位、
/// 不进插件列表，也不借用会话执行适配器（不依赖活跃 Core）。
pub fn mention_sources() -> Vec<Arc<dyn tiangong_core_manager::MentionSource>> {
    let Ok(plugins) = loaded_plugins().lock() else {
        return Vec::new();
    };
    let mut ids: Vec<_> = plugins
        .iter()
        .filter(|(_, plugin)| plugin.enabled)
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    ids.into_iter()
        .map(|id| {
            Arc::new(PluginMentionSource { id }) as Arc<dyn tiangong_core_manager::MentionSource>
        })
        .collect()
}

struct PluginMentionSource {
    id: String,
}

impl tiangong_core_manager::MentionSource for PluginMentionSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn query(
        &self,
        query: &tiangong_types::MentionQuery,
    ) -> Result<Vec<tiangong_types::MentionCandidate>, String> {
        let snapshot = {
            let plugins = loaded_plugins().lock().map_err(|e| e.to_string())?;
            plugins.get(&self.id).filter(|p| p.enabled).map(|p| {
                (
                    p.ui_plugin.clone(),
                    p.manifest.clone(),
                    p.descriptor.as_ref().map(|d| d.name.clone()),
                    p.generation,
                )
            })
        };
        let Some((instance, manifest, descriptor_name, generation)) = snapshot else {
            return Ok(Vec::new());
        };
        let mut candidates = Vec::new();
        if let Some(instance) = &instance {
            let result = crate::execution::run_outside_tokio(|| {
                let Ok(mut plugin) = instance.try_lock() else {
                    return Ok(Vec::new());
                };
                plugin.query_mentions(query)
            });
            match result {
                Ok(values) => candidates.extend(values.into_iter().map(|c| {
                    tiangong_types::MentionCandidate {
                        value: c.value,
                        label: c.label,
                        kind: c.kind,
                        hint: c.hint,
                        mark: c.mark,
                    }
                })),
                Err(error) => tracing::warn!(plugin_id = %self.id, %error, "mention 查询失败"),
            }
        }
        // 静态清单候选也按本次查询词过滤（不过滤会列出全部声明了 mention
        // 的插件），判定与宿主兜底同一函数。
        if let Some(candidate) = crate::ts_plugin::mention_candidate_from_manifest(
            &manifest,
            descriptor_name.as_deref(),
            &query.query,
        ) {
            candidates.push(candidate);
        }
        let plugins = loaded_plugins().lock().map_err(|e| e.to_string())?;
        let current = plugins.get(&self.id).filter(|p| {
            p.enabled && p.generation == generation && p.manifest.version == manifest.version
        });
        let valid = current.is_some_and(|p| match (&instance, &p.ui_plugin) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => {
                serde_json::to_value(&p.manifest).ok() == serde_json::to_value(&manifest).ok()
            }
            _ => false,
        });
        Ok(if valid { candidates } else { Vec::new() })
    }
}

/// 取已启用插件的安装目录（供桥接层访问插件私有数据）。
pub fn plugin_install_directory(plugin_id: &str) -> Option<PathBuf> {
    let plugins = loaded_plugins().lock().ok()?;
    let loaded = plugins.get(plugin_id)?;
    loaded.enabled.then(|| loaded.directory.clone())
}

/// 从指定路径为会话 `session_id` 加载 WASM 插件，供运行时集成测试使用。
pub fn load_wasm_plugin_at(
    wasm_path: &Path,
    sidecar: Option<Arc<dyn SidecarConnection>>,
    session_id: &str,
) -> Option<Arc<dyn Plugin>> {
    let plugin_id = wasm_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("plugin")
        .to_string();
    let bytes = Arc::new(std::fs::read(wasm_path).ok()?);
    let config = PluginRuntimeConfig::default();
    let plugin_id_for_load = plugin_id.clone();
    let plugin = crate::execution::run_outside_tokio(move || {
        let loader = WasmPluginLoader::with_sidecar(&config, sidecar)?;
        loader.load_bytes_for_plugin(&bytes, &config, &plugin_id_for_load, false)
    })
    .ok()?;
    Some(Arc::new(WasmPluginAdapter::new(
        plugin,
        PluginRuntimeConfig::default(),
        session_id,
    )))
}
/// 适配器。创建失败（纯 UI 插件、WASM 实例化失败）返回 `None`。
pub fn load_core_plugin(
    plugin_id: &str,
    runtime: RuntimeKind,
    session_id: &str,
) -> Option<Arc<dyn Plugin>> {
    let (manifest, component, descriptor_id, sidecar, enabled, storage_access, verified_sidecar) = {
        let plugins = loaded_plugins().lock().ok()?;
        let loaded = plugins.get(plugin_id)?;
        (
            loaded.manifest.clone(),
            loaded.component.clone(),
            loaded
                .descriptor
                .as_ref()
                .map(|descriptor| descriptor.id.clone())
                .unwrap_or_else(|| plugin_id.to_string()),
            loaded.sidecar.clone(),
            loaded.enabled,
            loaded.manifest.storage_access,
            loaded.verified_sidecar.clone(),
        )
    };

    let has_ts_contributions = manifest
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
        || manifest
            .prompt
            .as_ref()
            .is_some_and(|prompts| !prompts.is_empty());
    if runtime == RuntimeKind::Desktop && has_ts_contributions {
        let adapter = Arc::new(TsPluginAdapter::from_manifest(
            &manifest,
            enabled,
            verified_sidecar,
        ));
        if let Ok(mut plugins) = loaded_plugins().lock()
            && let Some(loaded) = plugins.get_mut(plugin_id)
        {
            loaded
                .ts_instances
                .retain(|instance| instance.strong_count() > 0);
            loaded.ts_instances.push(Arc::downgrade(&adapter));
        }
        return Some(adapter);
    }

    let component = component?;
    let plugin = match instantiate_from_compiled(
        component,
        sidecar.clone(),
        PluginRuntimeConfig::default(),
        plugin_id.to_string(),
        storage_access,
    ) {
        Ok(plugin) => plugin,
        Err(error) => {
            set_load_error(plugin_id, error.to_string());
            tracing::warn!(plugin_id, %error, "创建 Core WASM 插件实例失败");
            return None;
        }
    };
    let adapter = Arc::new(WasmPluginAdapter::new_with_id(
        plugin,
        PluginRuntimeConfig::default(),
        enabled,
        descriptor_id,
        sidecar.map(|s| s as Arc<dyn SidecarConnection>),
        session_id,
    ));
    if let Ok(mut plugins) = loaded_plugins().lock()
        && let Some(loaded) = plugins.get_mut(plugin_id)
    {
        loaded
            .instances
            .retain(|instance| instance.strong_count() > 0);
        loaded.instances.push(Arc::downgrade(&adapter));
    }
    Some(adapter)
}

pub(crate) fn dispatch_tools_recovered(plugin_id: &str, payload: &str) {
    let Ok(event) = serde_json::from_str::<crate::protocol::ToolsRecovered>(payload) else {
        return;
    };
    let (wasm, ts) = {
        let Ok(plugins) = loaded_plugins().lock() else {
            return;
        };
        let Some(loaded) = plugins.get(plugin_id).filter(|plugin| plugin.enabled) else {
            return;
        };
        (
            loaded
                .instances
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>(),
            loaded
                .ts_instances
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>(),
        )
    };
    for adapter in wasm {
        adapter.notify_tools_recovered(&event.tools);
    }
    for adapter in ts {
        adapter.notify_tools_recovered(&event.tools);
    }
}

/// 编译产物：纯 UI 插件（无 wasm）三项均为 None。
type CompiledPlugin = (
    Option<Arc<wasmtime::component::Component>>,
    Option<WasmPlugin>,
    Option<Descriptor>,
);

pub(super) fn compile_plugin(
    bytes: Arc<Vec<u8>>,
    manifest: &PluginManifest,
    sidecar: Option<Arc<dyn SidecarConnection>>,
    config: PluginRuntimeConfig,
) -> Result<CompiledPlugin> {
    // 纯 UI 插件（wasm 省略）：无逻辑层，返回空三元组
    if manifest.wasm_binary().is_none() {
        return Ok((None, None, None));
    }
    let plugin_id = manifest.id.clone();
    let expected_version = manifest.version.clone();
    crate::execution::run_outside_tokio(move || {
        let component = Arc::new(compile_component(&bytes)?);
        let mut plugin = instantiate_component(
            &component,
            &config,
            sidecar,
            &plugin_id,
            manifest.storage_access,
        )?;
        let descriptor = plugin.describe()?;
        if descriptor.id != plugin_id {
            bail!(
                "插件清单 ID 与组件描述不一致: manifest={plugin_id}, component={}",
                descriptor.id
            );
        }
        if descriptor.version != expected_version {
            bail!(
                "插件清单版本与组件描述不一致: manifest={expected_version}, component={}",
                descriptor.version
            );
        }
        Ok((Some(component), Some(plugin), Some(descriptor)))
    })
}

fn instantiate_from_compiled(
    component: Arc<wasmtime::component::Component>,
    sidecar: Option<Arc<dyn SidecarConnection>>,
    config: PluginRuntimeConfig,
    plugin_id: String,
    storage_access: bool,
) -> Result<WasmPlugin> {
    crate::execution::run_outside_tokio(move || {
        instantiate_component(&component, &config, sidecar, &plugin_id, storage_access)
    })
}

pub(super) fn read_wasm_bytes(installed: &InstalledPlugin) -> Result<Vec<u8>> {
    let wasm_binary = installed.manifest.wasm_binary().ok_or_else(|| {
        anyhow::anyhow!(
            "插件 {} 是纯 UI 插件，没有 WASM 制品",
            installed.manifest.id
        )
    })?;
    let path = installed.directory.join(wasm_binary);
    std::fs::read(&path).with_context(|| format!("读取插件 WASM 制品失败: {}", path.display()))
}

/// 登记插件加载错误（WASM 读取/编译/实例化、解释器不可发现、连接构造
/// 失败、重载逻辑层失败）：插件不满足可用条件，插件管理显示加载异常；
/// 只能由重新加载成功清除，sidecar 运行检查成功不影响它。
pub(crate) fn set_load_error(plugin_id: &str, error: String) {
    if let Ok(mut plugins) = loaded_plugins().lock()
        && let Some(plugin) = plugins.get_mut(plugin_id)
    {
        plugin.load_error = Some(error);
    }
}

/// 登记插件运行异常（常驻 sidecar 启动/握手、运行检查、验证记录保存、
/// 运行期停止失败等）：插件保持安装，插件管理显示启动异常；运行检查
/// 重新成功后由 refresh_verified_sidecar 清除。
pub(crate) fn set_runtime_error(plugin_id: &str, error: String) {
    if let Ok(mut plugins) = loaded_plugins().lock()
        && let Some(plugin) = plugins.get_mut(plugin_id)
    {
        plugin.runtime_error = Some(error);
    }
}

fn plugin_state(
    manifest: &PluginManifest,
    enabled: bool,
    has_wasm_ui: bool,
    error: Option<&str>,
) -> &'static str {
    // 未运行的 sidecar 可能仍在等待首次调用，只有已记录的错误才影响插件状态。
    if !enabled {
        "disabled"
    } else if !has_wasm_ui && manifest.wasm_binary().is_some() {
        "error"
    } else if error.is_some() {
        "degraded"
    } else {
        "loaded"
    }
}

pub(super) fn list_plugin_status_without_preload(
    manifest: &PluginManifest,
) -> Option<PluginStatus> {
    let (descriptor, generation, sidecar, load_error, runtime_error, has_ui, enabled, directory) = {
        let plugins = loaded_plugins().lock().ok()?;
        let loaded = plugins.get(&manifest.id)?;
        (
            loaded.descriptor.clone(),
            loaded.generation,
            loaded.sidecar.clone(),
            loaded.load_error.clone(),
            loaded.runtime_error.clone(),
            loaded.ui_plugin.is_some(),
            loaded.enabled,
            loaded.directory.clone(),
        )
    };
    let sidecar_running = sidecar
        .as_ref()
        .is_some_and(|connection| connection.has_runtime_endpoint());
    let last_error = load_error.or(runtime_error);
    let state = plugin_state(manifest, enabled, has_ui, last_error.as_deref());
    let configured = configured_model_capabilities();
    Some(PluginStatus {
        unavailable_reason: if enabled {
            check_plugin_availability(manifest, RuntimeKind::Desktop, &configured)
        } else {
            None
        },
        id: manifest.id.clone(),
        name: descriptor
            .as_ref()
            .map(|value| value.name.clone())
            .unwrap_or_else(|| manifest.id.clone()),
        description: manifest.description.clone(),
        manifest_version: manifest.version.clone(),
        loaded_version: descriptor.map(|value| value.version),
        state: state.to_string(),
        generation,
        enabled,
        can_rollback: rollback_directory(&directory, &manifest.id).is_dir(),
        has_sidecar: manifest.sidecar.is_some(),
        sidecar_running,
        last_error,
    })
}
