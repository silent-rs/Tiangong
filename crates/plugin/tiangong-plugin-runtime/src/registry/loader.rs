//! 查询视图（列表/状态/清单）、WASM 编译装载与 Core 适配器创建。

use serde_json::Value;

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
        // 单次装载快照：验证记录 + prompt 段落各读取一次（前者重算制品
        // 摘要、后者含阻塞 sidecar I/O），适配器与最终记录完全共用。
        let snapshot = PluginLoadSnapshot::read(storage_root, installed);
        // reconfigure 消费 Vec<String>；None 只在注册表中表示「未声明」。
        let adapter_sections = snapshot.prompt_sections.clone().unwrap_or_default();
        let adapter_tools = snapshot.tool_specs.clone().unwrap_or_default();
        for adapter in &ts_instances {
            adapter.reconfigure(
                &installed.manifest,
                installed.enabled,
                snapshot.verified_sidecar.clone(),
                adapter_sections.clone(),
                adapter_tools.clone(),
            );
        }
        let mut loaded =
            load_plugin_record_with_snapshot(storage_root, installed.clone(), snapshot);
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
    loaded.verified_sidecar = verified_sidecar.clone();
    loaded.prompt_sections = assemble_prompt_sections(
        storage_root,
        &installed.directory,
        &installed.manifest,
        verified_sidecar.as_deref(),
    );
    loaded.dynamic_tool_specs = assemble_dynamic_tool_specs(
        storage_root,
        &installed.manifest,
        verified_sidecar.as_deref(),
    );
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
    invoke_sidecar_op(storage_root, plugin_id, operation, payload, None, None)
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
    invoke_sidecar_op(storage_root, plugin_id, operation, payload, workspace, None)
}

/// 携带统一调用上下文（含 `deadline_ms` 交互预算）调用 sidecar。
///
/// deadline 由 stdio 往返等待消费：超时返回 Err 而不是等满清单的
/// `request_timeout_ms`——交互路径（@提及补全）不能让挂起的 sidecar
/// 阻塞输入框。上下文同时把会话工作区带给按需连接的沙箱启动。
pub fn invoke_sidecar_op(
    storage_root: &Path,
    plugin_id: &str,
    operation: &str,
    payload: serde_json::Value,
    workspace: Option<&Path>,
    invocation_context: Option<&crate::protocol::RequestInvocationContext>,
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
    let response = match invocation_context {
        Some(context) => connection.invoke_with_invocation_context_and_progress(
            operation,
            &payload,
            context,
            &mut |_| {},
        )?,
        None => connection.invoke(operation, &payload)?,
    };
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
        // 提及来源按清单写法分流（两者并存时以 candidates_op 为准，静态字段
        // 被忽略——不作为 sidecar 不可用时的回退，见 MentionManifest 文档）：
        //
        // 1. v2 写法（candidates_op 且带 sidecar）：查询直达 sidecar（载荷
        //    MentionQuery JSON）。响应 `{ entry?, candidates }`：entry 可选——
        //    能力型插件用它声明"可被 @ 调用"的入口（宿主合成 value/label/kind，
        //    op 提供 hint/mark）；内容提供方（如 index）只返回 candidates，
        //    插件本身不作为被提及对象。动态候选逐项完备性过滤。sidecar 不可用
        //    或超预算时返回空：无法提供即不出现。
        // 2. v0.17.6 静态写法（只有 hint）：复用入口合成，把静态 hint/mark
        //    当作 entry 传入——兼容存量插件（不经升级无法改清单），不要求
        //    sidecar。查询词过滤由 CoreManager 聚合层统一处理。
        let mention_op = manifest
            .mention
            .as_ref()
            .and_then(|mention| mention.candidates_op.as_deref())
            .map(str::trim)
            .filter(|op| !op.is_empty());
        if let Some(op) = mention_op
            && manifest.sidecar.is_some()
        {
            // 交互级预算：补全是输入路径，等满清单 request_timeout_ms 会卡住
            // 输入框。会话上下文（工作区）随统一调用元数据带给 sidecar。
            const MENTION_QUERY_BUDGET_MS: u64 = 3000;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let invocation_context = crate::protocol::RequestInvocationContext {
                session_id: query.context.session_id.clone().unwrap_or_default(),
                invocation_id: scru128::new().to_string(),
                workspace: query.context.workspace.clone().unwrap_or_default(),
                actor_id: String::new(),
                deadline_ms: Some(now + MENTION_QUERY_BUDGET_MS),
            };
            let payload = serde_json::to_value(query).unwrap_or_default();
            let storage_root = crate::registry::plugin_install_directory(&self.id)
                .and_then(|dir| dir.parent().map(|p| p.to_path_buf()))
                .and_then(|plugins_dir| plugins_dir.parent().map(|p| p.to_path_buf()));
            let result = storage_root.map(|root| {
                crate::registry::invoke_sidecar_op(
                    &root,
                    &self.id,
                    op,
                    payload,
                    None,
                    Some(&invocation_context),
                )
            });
            match result {
                Some(Ok(value)) => candidates.extend(parse_mention_op_response(
                    &value,
                    &manifest,
                    descriptor_name.as_deref(),
                    &self.id,
                    op,
                )),
                Some(Err(error)) => {
                    tracing::warn!(plugin_id = %self.id, %error, op, "sidecar 动态提及查询失败或超预算")
                }
                None => {
                    tracing::warn!(plugin_id = %self.id, op, "无法定位插件存储根，跳过提及查询")
                }
            }
        } else if let Some(mention) = manifest.mention.as_ref() {
            // v0.17.6 静态入口（仅存量插件兼容）：静态 hint/mark 走同一条入口
            // 合成路径，完备性（hint 非空）由 mention_entry_candidate 兜底。
            if let Some(candidate) = crate::ts_plugin::mention_entry_candidate(
                &manifest,
                descriptor_name.as_deref(),
                &static_mention_entry(mention),
            ) {
                candidates.push(candidate);
            }
        }
        if let Some(instance) = &instance {
            let result = crate::execution::run_outside_tokio(|| {
                let Ok(mut plugin) = instance.try_lock() else {
                    return Ok(Vec::new());
                };
                plugin.query_mentions(query)
            });
            match result {
                Ok(values) => candidates.extend(
                    values
                        .into_iter()
                        .map(|c| tiangong_types::MentionCandidate {
                            value: c.value,
                            label: c.label,
                            kind: c.kind,
                            hint: c.hint,
                            mark: c.mark,
                        })
                        .filter(mention_candidate_is_complete),
                ),
                Err(error) => tracing::warn!(plugin_id = %self.id, %error, "mention 查询失败"),
            }
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

/// 动态提及候选完备性：value / label / hint 均非空。
///
/// 三者是前端渲染与查询匹配的必备字段——hint 是副标题且与 label / value
/// 一同参与查询词匹配（`candidate_matches_query`），label 空会渲染出空行，
/// value 空则候选无插入内容。入口候选（op entry 合成）的完备性由
/// `mention_entry_candidate` 保证（缺 hint 即不合成）。
fn mention_candidate_is_complete(candidate: &tiangong_types::MentionCandidate) -> bool {
    !candidate.value.trim().is_empty()
        && !candidate.label.trim().is_empty()
        && !candidate.hint.trim().is_empty()
}

/// 把 `candidates_op` 返回的单个 JSON 项转为候选；字段残缺（缺
/// value / label / hint）时返回 None，由调用方跳过并计数告警，不把
/// 渲染不出、也搜不到的候选透给前端。
fn dynamic_mention_candidate(item: &serde_json::Value) -> Option<tiangong_types::MentionCandidate> {
    let text = |key: &str| {
        item.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let candidate = tiangong_types::MentionCandidate {
        value: text("value"),
        label: text("label"),
        kind: text("kind"),
        hint: text("hint"),
        mark: text("mark"),
    };
    mention_candidate_is_complete(&candidate).then_some(candidate)
}

/// v0.17.6 静态 mention 声明转入口 entry：hint/mark 透传，完备性（hint
/// 非空、mark 1-4 字符）由 {@link crate::ts_plugin::mention_entry_candidate}
/// 兜底——与 op 响应的 entry 走同一条合成路径，行为一致。
pub(crate) fn static_mention_entry(
    mention: &crate::manifest::MentionManifest,
) -> serde_json::Value {
    serde_json::json!({
        "hint": mention.hint.clone().unwrap_or_default(),
        "mark": mention.mark.clone().unwrap_or_default(),
    })
}

/// 解析 `candidates_op` 响应 `{ entry?, candidates }`。///
/// - `entry` 可选：有则合成入口候选（宿主约定 value/label/kind + op 的
///   hint/mark，见 {@link crate::ts_plugin::mention_entry_candidate}），
///   缺 hint 跳过并告警；内容提供方（如 index）不返回 entry，插件本身
///   不作为被提及对象。
/// - `candidates` 逐项完备性过滤，残缺项跳过并计数告警。
/// - 形状不符（非对象、无 candidates）返回已解析部分，不视为错误。
pub(crate) fn parse_mention_op_response(
    value: &serde_json::Value,
    manifest: &crate::manifest::PluginManifest,
    descriptor_name: Option<&str>,
    plugin_id: &str,
    op: &str,
) -> Vec<tiangong_types::MentionCandidate> {
    let mut candidates = Vec::new();
    if let Some(entry) = value.get("entry") {
        match crate::ts_plugin::mention_entry_candidate(manifest, descriptor_name, entry) {
            Some(candidate) => candidates.push(candidate),
            None => tracing::warn!(plugin_id, op, "mention entry 缺少 hint，入口候选已跳过"),
        }
    }
    if let Some(items) = value.get("candidates").and_then(Value::as_array) {
        let mut skipped = 0usize;
        for item in items {
            match dynamic_mention_candidate(item) {
                Some(candidate) => candidates.push(candidate),
                None => skipped += 1,
            }
        }
        if skipped > 0 {
            tracing::warn!(
                plugin_id,
                op,
                skipped,
                "动态提及候选缺少 value/label/hint，已跳过"
            );
        }
    }
    candidates
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
/// 清单声明的工具是否全部由 sidecar 承接（无需桌面前端页面执行）。
///
/// Server/CLI 没有插件前端，只有满足该条件的清单工具插件才能在这些入口
/// 注册工具：无界面直连形态天然满足；带界面的插件需握手能力覆盖全部工具。
pub(crate) fn tools_served_by_sidecar(
    manifest: &PluginManifest,
    verified_capabilities: Option<&[String]>,
) -> bool {
    if manifest.sidecar.is_none() {
        return false;
    }
    if manifest.ui_contributions().is_empty() {
        return true;
    }
    // 动态工具（`tool:*` 通配）：名单运行时确定、清单不静态枚举，
    // 通配即全部承接。
    let tools = manifest.tools.as_deref().unwrap_or_default();
    if tools.is_empty() {
        return verified_capabilities
            .is_some_and(|capabilities| capabilities.iter().any(|c| c == "tool:*"));
    }
    !tools.is_empty()
        && verified_capabilities.is_some_and(|capabilities| {
            tools
                .iter()
                .all(|tool| crate::invocation::capabilities_handle_tool(capabilities, &tool.name))
        })
}

/// 动态 prompt 段落读取预算：装载/通知刷新路径使用，防止故障 sidecar
/// 长时间拖住装载（与 @提及交互预算同量级）。
const DYNAMIC_PROMPT_QUERY_BUDGET_MS: u64 = 3_000;
/// 动态工具规格读取预算：装载/通知刷新路径使用（sidecar 返回的是探测
/// 缓存快照，不触发实时探测，预算与 prompt 同量级）。
const DYNAMIC_TOOL_SPECS_QUERY_BUDGET_MS: u64 = 3_000;

/// 单个 prompt 文件的大小上限（64 KiB）。
const MAX_PROMPT_FILE_BYTES: u64 = 64 * 1024;

/// 测试观测：assemble_prompt_sections 的调用计数（验证装载快照只组装一次）。
#[cfg(test)]
pub(crate) static ASSEMBLE_PROMPT_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 解析 [`PROMPT_SECTIONS_OPERATION`] 响应：字符串数组，段落非空。
fn parse_prompt_sections_response(value: serde_json::Value) -> anyhow::Result<Vec<String>> {
    let mut sections = serde_json::from_value::<Vec<String>>(value)
        .map_err(|error| anyhow::anyhow!("解析响应失败: {error}"))?;
    // 单条空段落只丢弃该条（warn），不让整批段落丢失。
    sections.retain(|section| {
        let non_empty = !section.trim().is_empty();
        if !non_empty {
            tracing::warn!("sidecar 返回了空 prompt 段落，已丢弃");
        }
        non_empty
    });
    Ok(sections)
}

/// **装载/刷新时**把清单 prompt 条目统一解析为段落快照：
/// - 内联文本：原样成段；
/// - 随包文件：读插件目录内文件全文成段；
/// - sidecar 条目：安装验证记录含 `prompt` 能力时经固定操作读取，
///   否则（未验证/未声明能力）fail-honest 跳过并告警。
///
/// 任何条目失败都只跳过该条目（warn），不阻断装载——声明聚合只读注册表
/// 快照，不做 I/O；文件与 sidecar 内容变化经 [`PROMPT_UPDATED_CHANNEL`]
/// 通知 / 重载重取，计入能力指纹走上下文交接。清单未声明 prompt 时返回
/// None（与「声明了但当前解析为空」区分，指纹只对 Some 计入段落摘要）。
pub(crate) fn assemble_prompt_sections(
    storage_root: &Path,
    plugin_directory: &Path,
    manifest: &PluginManifest,
    verified_capabilities: Option<&[String]>,
) -> Option<Vec<String>> {
    let entries = manifest.prompt.as_ref()?;
    #[cfg(test)]
    ASSEMBLE_PROMPT_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut sections = Vec::new();
    for entry in entries {
        match entry {
            crate::manifest::PromptSectionDecl::Text(text) => sections.push(text.clone()),
            crate::manifest::PromptSectionDecl::Entry(
                crate::manifest::PromptSectionEntry::Text { text },
            ) => sections.push(text.clone()),
            crate::manifest::PromptSectionDecl::Entry(
                crate::manifest::PromptSectionEntry::File { file },
            ) => {
                // 超上限（误指向二进制/数据文件）只跳过并告警：段落会整段
                // 进入 system prompt / 对话内清单与能力指纹。
                let path = plugin_directory.join(file.trim());
                match std::fs::metadata(&path) {
                    Ok(meta) if meta.len() > MAX_PROMPT_FILE_BYTES => {
                        tracing::warn!(plugin_id = %manifest.id, file, size = meta.len(), "prompt 文件超过上限，跳过");
                    }
                    _ => match std::fs::read_to_string(&path) {
                        Ok(content) if !content.trim().is_empty() => sections.push(content),
                        Ok(_) => {
                            tracing::warn!(plugin_id = %manifest.id, file, "prompt 文件内容为空，跳过")
                        }
                        Err(error) => {
                            tracing::warn!(plugin_id = %manifest.id, file, %error, "prompt 文件读取失败，跳过该条目")
                        }
                    },
                }
            }
            crate::manifest::PromptSectionDecl::Entry(
                crate::manifest::PromptSectionEntry::Sidecar,
            ) => {
                sections.extend(fetch_sidecar_prompt_sections(
                    storage_root,
                    manifest,
                    verified_capabilities,
                ));
            }
        }
    }
    Some(sections)
}

/// sidecar 动态条目读取：验证记录含 `prompt` 能力时经固定操作获取
/// （短预算，见 [`DYNAMIC_PROMPT_QUERY_BUDGET_MS`]）；失败 fail-honest
/// 返回空（warn），通知或重载时重试。
fn fetch_sidecar_prompt_sections(
    storage_root: &Path,
    manifest: &PluginManifest,
    verified_capabilities: Option<&[String]>,
) -> Vec<String> {
    let verified_prompt = verified_capabilities.is_some_and(|capabilities| {
        capabilities
            .iter()
            .any(|capability| capability == tiangong_plugin_sidecar::protocol::PROMPT_CAPABILITY)
    });
    if !verified_prompt {
        tracing::warn!(plugin_id = %manifest.id, "sidecar 未验证 prompt 能力，动态 prompt 条目不注入");
        return Vec::new();
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let context = crate::protocol::RequestInvocationContext {
        session_id: String::new(),
        invocation_id: scru128::new().to_string(),
        workspace: String::new(),
        actor_id: String::new(),
        deadline_ms: Some(now_ms.saturating_add(DYNAMIC_PROMPT_QUERY_BUDGET_MS)),
    };
    let plugin_id = manifest.id.clone();
    let fetch = move || {
        invoke_sidecar_op(
            storage_root,
            &plugin_id,
            tiangong_plugin_sidecar::protocol::PROMPT_SECTIONS_OPERATION,
            serde_json::json!({}),
            None,
            Some(&context),
        )
    };
    match crate::execution::run_outside_tokio(fetch).and_then(parse_prompt_sections_response) {
        Ok(sections) => sections,
        Err(error) => {
            tracing::warn!(plugin_id = %manifest.id, %error, "动态 prompt 段落获取失败，本次不注入");
            Vec::new()
        }
    }
}

/// sidecar 动态工具规格快照：验证记录含 `tool:*` 通配（sidecar 声明
/// 承接任意工具名，名单运行时确定、清单不静态枚举）时经固定操作
/// [`TOOL_SPECS_OPERATION`] 获取（短预算，见
/// [`DYNAMIC_TOOL_SPECS_QUERY_BUDGET_MS`]）；失败 fail-honest 返回空
/// （warn），通知或重载时重试。无通配能力的插件返回 None。
pub(crate) fn assemble_dynamic_tool_specs(
    storage_root: &Path,
    manifest: &PluginManifest,
    verified_capabilities: Option<&[String]>,
) -> Option<Vec<crate::manifest::TsToolDecl>> {
    let wildcard = verified_capabilities
        .is_some_and(|capabilities| capabilities.iter().any(|capability| capability == "tool:*"));
    if !wildcard {
        return None;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let context = crate::protocol::RequestInvocationContext {
        session_id: String::new(),
        invocation_id: scru128::new().to_string(),
        workspace: String::new(),
        actor_id: String::new(),
        deadline_ms: Some(now_ms.saturating_add(DYNAMIC_TOOL_SPECS_QUERY_BUDGET_MS)),
    };
    let plugin_id = manifest.id.clone();
    let fetch = move || {
        invoke_sidecar_op(
            storage_root,
            &plugin_id,
            tiangong_plugin_sidecar::protocol::TOOL_SPECS_OPERATION,
            serde_json::json!({}),
            None,
            Some(&context),
        )
    };
    match crate::execution::run_outside_tokio(fetch).and_then(parse_tool_specs_response) {
        Ok(specs) => Some(specs),
        Err(error) => {
            tracing::warn!(plugin_id = %manifest.id, %error, "动态工具规格获取失败，本次不装配");
            Some(Vec::new())
        }
    }
}

/// 解析 `tool_specs` 响应为工具规格数组：每项须为对象且 `name` 合法
///（字母数字与 - _，与清单静态工具同约束），`description` /
/// `input_schema` 缺省回退；非法项整体失败（fail-honest 空快照）。
fn parse_tool_specs_response(
    value: serde_json::Value,
) -> anyhow::Result<Vec<crate::manifest::TsToolDecl>> {
    let raw = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tool_specs 响应必须是数组"))?;
    let mut specs = Vec::with_capacity(raw.len());
    for item in raw {
        let name = item
            .get("name")
            .and_then(|name| name.as_str())
            .filter(|name| {
                !name.trim().is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
            .ok_or_else(|| anyhow::anyhow!("tool_specs 条目 name 无效: {item}"))?
            .to_string();
        let description = item
            .get("description")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let input_schema = item
            .get("input_schema")
            .cloned()
            .filter(|schema| schema.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
        let timeout_ms = item
            .get("timeout_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(20_000);
        let non_idempotent = item
            .get("non_idempotent")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        specs.push(crate::manifest::TsToolDecl {
            name,
            description,
            input_schema,
            timeout_ms,
            non_idempotent,
        });
    }
    Ok(specs)
}

/// sidecar 经 [`TOOLS_UPDATED_CHANNEL`] 通知工具声明变更时重取并更新
/// 注册表与存活适配器；core 每轮经 `tool_specs()` 读取，快照更新后
/// 下一轮工具装配自动生效。通知线程调用，阻塞 I/O 无碍。
pub(crate) fn refresh_tool_specs(plugin_id: &str) {
    // plugin_install_directory 返回 <root>/plugins/<id>，即插件目录本身；
    // 存储根再上溯两级。
    let Some(plugin_dir) = plugin_install_directory(plugin_id) else {
        return;
    };
    let Some(storage_root) = plugin_dir
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
    else {
        return;
    };
    let (manifest, verified, enabled, generation, adapter_sections, ts_instances) = {
        let Ok(plugins) = loaded_plugins().lock() else {
            return;
        };
        let Some(loaded) = plugins.get(plugin_id).filter(|loaded| loaded.enabled) else {
            return;
        };
        (
            loaded.manifest.clone(),
            loaded.verified_sidecar.clone(),
            loaded.enabled,
            loaded.generation,
            loaded.prompt_sections.clone().unwrap_or_default(),
            loaded
                .ts_instances
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .collect::<Vec<_>>(),
        )
    };
    let specs = assemble_dynamic_tool_specs(&storage_root, &manifest, verified.as_deref());
    let changed = {
        let Ok(mut plugins) = loaded_plugins().lock() else {
            return;
        };
        let Some(loaded) = plugins.get_mut(plugin_id) else {
            return;
        };
        // 重取期间插件可能被升级/重载换代（directory+generation 变化）：
        // 旧线程的结果不得写进新记录，丢弃等待新代自己的装载/通知。
        if loaded.directory != plugin_dir || loaded.generation != generation {
            tracing::warn!(plugin_id, "工具规格重取期间插件已换代，丢弃本次结果");
            return;
        }
        let changed = loaded.dynamic_tool_specs != specs;
        loaded.dynamic_tool_specs = specs.clone();
        changed
    };
    if !changed {
        return;
    }
    // 存活适配器同步同一份结果（裸写 state 会漏 sidecar_direct 等派生项，
    // reconfigure 正是干这个的）。
    let adapter_specs = specs.unwrap_or_default();
    for adapter in &ts_instances {
        adapter.reconfigure(
            &manifest,
            enabled,
            verified.clone(),
            adapter_sections.clone(),
            adapter_specs.clone(),
        );
    }
    // 工具清单变化即插件能力变化：与升级/启停同一事件通道。
    crate::events::announce_plugin_change(crate::events::PluginChangeKind::ToolsChanged, plugin_id);
}

/// sidecar 经 [`PROMPT_UPDATED_CHANNEL`] 通知段落变更时重取并更新注册表；
/// 内容变化由 [`crate::registry::enabled_plugin_fingerprint`] 计入，触发
/// 上下文交接（压缩 message）。通知线程调用，阻塞 I/O 无碍。
pub(crate) fn refresh_prompt_sections(plugin_id: &str) {
    // plugin_install_directory 返回 <root>/plugins/<id>，即插件目录本身；
    // 存储根再上溯两级。
    let Some(plugin_dir) = plugin_install_directory(plugin_id) else {
        return;
    };
    let Some(storage_root) = plugin_dir
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
    else {
        return;
    };
    let (manifest, verified, enabled, generation, ts_instances) = {
        let Ok(plugins) = loaded_plugins().lock() else {
            return;
        };
        let Some(loaded) = plugins.get(plugin_id).filter(|loaded| loaded.enabled) else {
            return;
        };
        (
            loaded.manifest.clone(),
            loaded.verified_sidecar.clone(),
            loaded.enabled,
            loaded.generation,
            loaded
                .ts_instances
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .collect::<Vec<_>>(),
        )
    };
    let sections =
        assemble_prompt_sections(&storage_root, &plugin_dir, &manifest, verified.as_deref());
    let changed = {
        let Ok(mut plugins) = loaded_plugins().lock() else {
            return;
        };
        let Some(loaded) = plugins.get_mut(plugin_id) else {
            return;
        };
        // 组装期间插件可能被升级/重载换代（directory+generation 变化）：
        // 旧线程的结果不得写进新记录，丢弃等待新代自己的装载/通知。
        if loaded.directory != plugin_dir || loaded.generation != generation {
            tracing::warn!(plugin_id, "prompt 重取期间插件已换代，丢弃本次结果");
            return;
        }
        let changed = loaded.prompt_sections != sections;
        loaded.prompt_sections = sections.clone();
        changed
    };
    if !changed {
        return;
    }
    // 存活适配器同步同一份结果（裸写 state 会漏 sidecar_direct 等派生项，
    // reconfigure 正是干这个的）。工具快照沿用当前注册表值（本函数只刷新段落）。
    let adapter_sections = sections.unwrap_or_default();
    let adapter_tools = {
        let Ok(plugins) = loaded_plugins().lock() else {
            return;
        };
        plugins
            .get(plugin_id)
            .map(|loaded| loaded.dynamic_tool_specs.clone().unwrap_or_default())
            .unwrap_or_default()
    };
    for adapter in &ts_instances {
        adapter.reconfigure(
            &manifest,
            enabled,
            verified.clone(),
            adapter_sections.clone(),
            adapter_tools.clone(),
        );
    }
    // 内容变化即插件能力变化：与升级/启停同一事件通道，宿主（UI 刷新、
    // 上下文交接打标）按指纹定档响应。锁全部释放后发布。
    crate::events::announce_plugin_change(
        crate::events::PluginChangeKind::PromptSectionsChanged,
        plugin_id,
    );
}

/// TS 适配器（清单直连形态）是否承接该插件——两个条件同时成立：
///
/// 1. **清单带 TS 贡献**：工具、静态 prompt 段落，或动态 prompt 声明
///    （prompt 条目声明）；都没有则交给 WASM 组件或跳过。
/// 2. **当前运行形态提供得了这些贡献**：Desktop 恒可（页面可深度参与）；
///    CLI/Server 无页面，贡献必须由**经过安装验证的 sidecar** 承接——
///    工具全部覆盖，或无工具的纯动态 prompt 插件由握手声明 `prompt`
///    能力的 sidecar 提供段落（sidecar 自行多报不能扩大 manifest 接缝；
///    纯动态 prompt 插件即使带设置页等 UI 也可用，页面只负责交互展示）。
fn ts_adapter_serves(
    manifest: &PluginManifest,
    runtime: RuntimeKind,
    verified_capabilities: Option<&[String]>,
) -> bool {
    let tools = manifest.tools.as_deref().unwrap_or_default();
    let prompt_declared = manifest
        .prompt
        .as_ref()
        .is_some_and(|entries| !entries.is_empty());
    if tools.is_empty() && !prompt_declared {
        return false;
    }
    if runtime == RuntimeKind::Desktop {
        return true;
    }
    if tools_served_by_sidecar(manifest, verified_capabilities) {
        return true;
    }
    // 无头入口的无工具纯 prompt 插件：文本 / 文件条目装载时已组装成快照，
    // 天然可用；sidecar 条目要求已验证的 `prompt` 能力（sidecar 自行多报
    // 不能扩大 manifest 接缝）。带设置页等 UI 也不受限——页面只负责展示。
    tools.is_empty()
        && prompt_declared
        && (!manifest.declares_sidecar_prompt()
            || verified_capabilities.is_some_and(|capabilities| {
                capabilities.iter().any(|capability| {
                    capability == tiangong_plugin_sidecar::protocol::PROMPT_CAPABILITY
                })
            }))
}

pub fn load_core_plugin(
    plugin_id: &str,
    runtime: RuntimeKind,
    session_id: &str,
) -> Option<Arc<dyn Plugin>> {
    let (
        manifest,
        component,
        descriptor_id,
        sidecar,
        enabled,
        storage_access,
        verified_sidecar,
        dynamic_prompt_sections,
        dynamic_tool_specs,
    ) = {
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
            loaded.prompt_sections.clone(),
            loaded.dynamic_tool_specs.clone(),
        )
    };

    if ts_adapter_serves(&manifest, runtime, verified_sidecar.as_deref()) {
        // 段落装载时已组装（内联文本 / 文件 / sidecar 统一快照），此处只读。
        let prompt_sections = dynamic_prompt_sections.unwrap_or_default();
        let dynamic_tools = dynamic_tool_specs.unwrap_or_default();
        let adapter = Arc::new(TsPluginAdapter::from_manifest(
            &manifest,
            enabled,
            verified_sidecar,
            prompt_sections,
            dynamic_tools,
        ));
        adapter.set_session_id(session_id);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_specs_响应解析_形状与回退() {
        let parsed = parse_tool_specs_response(serde_json::json!([
            {
                "name": "mcp__demo__query",
                "description": "查询",
                "input_schema": {"type": "object"},
                "timeout_ms": 60000,
            },
            {"name": "mcp__demo__list_all"},
        ]))
        .expect("合法响应");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "mcp__demo__query");
        assert_eq!(parsed[0].description, "查询");
        assert_eq!(parsed[0].input_schema["type"], "object");
        assert_eq!(parsed[0].timeout_ms, 60000);
        // 缺省回退：空描述 / 空 schema / 默认超时。
        assert_eq!(parsed[1].description, "");
        assert_eq!(parsed[1].input_schema, serde_json::json!({}));
        assert_eq!(parsed[1].timeout_ms, 20_000);
    }

    #[test]
    fn tool_specs_响应解析_非数组或非法名整体失败() {
        assert!(parse_tool_specs_response(serde_json::json!({"servers": []})).is_err());
        assert!(parse_tool_specs_response(serde_json::json!(["ok"])).is_err());
        assert!(parse_tool_specs_response(serde_json::json!([{"name": "bad name"}])).is_err());
        assert!(parse_tool_specs_response(serde_json::json!([{"name": ""}])).is_err());
    }

    #[test]
    fn 动态工具条目_服务端承接须通配能力() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schema_version":2,"id":"mcp-demo","version":"1.0.0",
                "dynamic_tools":true,
                "sidecar":{"binary":"demo-sidecar"},
                "ui":{"contributions":[{"slot":"settings.plugin-page","id":"demo","title":"demo","entry":"app/demo.html"}]}}"#,
        )
        .unwrap();
        // 带 UI 的动态工具插件：只有 `tool:*` 通配才视为全部承接（server/cli 可注册）。
        assert!(tools_served_by_sidecar(
            &manifest,
            Some(&["tools".to_string(), "tool:*".to_string()])
        ));
        assert!(!tools_served_by_sidecar(
            &manifest,
            Some(&["tools".to_string(), "tool:demo".to_string()])
        ));
        assert!(!tools_served_by_sidecar(&manifest, None));
    }

    fn candidate(value: &str, label: &str, hint: &str) -> tiangong_types::MentionCandidate {
        tiangong_types::MentionCandidate {
            value: value.into(),
            label: label.into(),
            kind: "file".into(),
            hint: hint.into(),
            mark: "F".into(),
        }
    }

    #[test]
    fn 动态候选_三项齐全才采用() {
        let item = serde_json::json!({
            "value": "@file:src/main.rs",
            "label": "main.rs",
            "kind": "file",
            "hint": "src/main.rs",
            "mark": "F",
        });
        let parsed = dynamic_mention_candidate(&item).expect("完备候选应被采用");
        assert_eq!(parsed.value, "@file:src/main.rs");
        assert_eq!(parsed.hint, "src/main.rs");
        assert_eq!(parsed.mark, "F");
    }

    #[test]
    fn 动态候选_缺hint或label或value被跳过() {
        // 缺 hint：前端没有副标题，查询匹配也少一个字段
        assert!(
            dynamic_mention_candidate(&serde_json::json!({
                "value": "@file:a.rs", "label": "a.rs",
            }))
            .is_none()
        );
        // 空串 hint 等同缺失
        assert!(
            dynamic_mention_candidate(&serde_json::json!({
                "value": "@file:a.rs", "label": "a.rs", "hint": "  ",
            }))
            .is_none()
        );
        // 缺 label：渲染出空行
        assert!(
            dynamic_mention_candidate(&serde_json::json!({
                "value": "@file:a.rs", "hint": "a.rs",
            }))
            .is_none()
        );
        // 缺 value：候选无插入内容
        assert!(
            dynamic_mention_candidate(&serde_json::json!({
                "label": "a.rs", "hint": "a.rs",
            }))
            .is_none()
        );
    }

    #[test]
    fn 完备性谓词_空白字符串视为缺失() {
        assert!(mention_candidate_is_complete(&candidate(
            "@file:a.rs",
            "a.rs",
            "a.rs"
        )));
        assert!(!mention_candidate_is_complete(&candidate(
            "@file:a.rs",
            "a.rs",
            ""
        )));
        assert!(!mention_candidate_is_complete(&candidate(
            "", "a.rs", "a.rs"
        )));
        assert!(!mention_candidate_is_complete(&candidate(
            "@file:a.rs",
            " ",
            "a.rs"
        )));
    }
}

#[cfg(test)]
mod dynamic_prompt_loader_tests {
    use super::*;

    fn prompt_manifest() -> PluginManifest {
        serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "id": "prompt-with-settings",
            "version": "1.0.0",
            "permissions": ["sidecar.invoke"],
            "sidecar": {"runtime": "node", "entry": "sidecar/main.mjs"},
            "prompt": [{"type": "sidecar"}],
            "ui": {"contributions": [{
                "slot": "settings.plugin-page",
                "id": "settings",
                "title": "设置",
                "entry": "app/settings.html"
            }]}
        }))
        .expect("纯动态 prompt 测试清单")
    }

    #[test]
    fn 纯动态prompt带ui也可在无头入口加载() {
        let manifest = prompt_manifest();
        let verified = vec![tiangong_plugin_sidecar::protocol::PROMPT_CAPABILITY.to_string()];
        assert!(!tools_served_by_sidecar(&manifest, Some(&verified)));
        assert!(ts_adapter_serves(
            &manifest,
            RuntimeKind::Server,
            Some(&verified)
        ));
        assert!(ts_adapter_serves(
            &manifest,
            RuntimeKind::Cli,
            Some(&verified)
        ));
    }

    #[test]
    fn 无头动态prompt必须有已验证的sidecar能力() {
        let manifest = prompt_manifest();
        assert!(!ts_adapter_serves(&manifest, RuntimeKind::Server, None));
        assert!(!ts_adapter_serves(
            &manifest,
            RuntimeKind::Server,
            Some(&[])
        ));
        assert!(ts_adapter_serves(&manifest, RuntimeKind::Desktop, None));
    }

    #[test]
    fn 无ts贡献的插件不创建适配器() {
        let manifest = serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "id": "ui-only",
            "version": "1.0.0",
            "sidecar": {"runtime": "node", "entry": "sidecar/main.mjs"},
            "ui": {"contributions": [{
                "slot": "settings.plugin-page",
                "id": "settings",
                "title": "设置",
                "entry": "app/settings.html"
            }]}
        }))
        .expect("纯 UI 测试清单");
        assert!(!ts_adapter_serves(&manifest, RuntimeKind::Desktop, None));
        assert!(!ts_adapter_serves(
            &manifest,
            RuntimeKind::Server,
            Some(&[tiangong_plugin_sidecar::protocol::PROMPT_CAPABILITY.to_string()])
        ));
    }

    #[test]
    fn 动态prompt响应必须是字符串数组且段落非空() {
        assert_eq!(
            parse_prompt_sections_response(serde_json::json!([])).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_prompt_sections_response(serde_json::json!(["a", "b"])).unwrap(),
            ["a", "b"]
        );
        // 单条空段落被丢弃而非整批失败；结构错误才失败
        assert_eq!(
            parse_prompt_sections_response(serde_json::json!(["a", " ", "b"])).unwrap(),
            ["a", "b"]
        );
        for invalid in [
            serde_json::json!(null),
            serde_json::json!({"sections": ["a"]}),
            serde_json::json!(["a", 1]),
        ] {
            assert!(parse_prompt_sections_response(invalid).is_err());
        }
    }

    #[test]
    fn prompt组装_条目形态与门控() {
        use crate::manifest::PromptSectionDecl;
        let text = PromptSectionDecl::Text("内联段落".to_string());
        let tagged_text = PromptSectionDecl::Entry(crate::manifest::PromptSectionEntry::Text {
            text: "标签段落".to_string(),
        });
        let sidecar = PromptSectionDecl::Entry(crate::manifest::PromptSectionEntry::Sidecar);
        // 组装含文件条目：临时目录写文件验证读取成段
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("guide.md"), "文件段落").unwrap();
        let file = PromptSectionDecl::Entry(crate::manifest::PromptSectionEntry::File {
            file: "guide.md".to_string(),
        });
        let mut manifest = prompt_manifest();
        manifest.prompt = Some(vec![text, tagged_text, file]);
        assert_eq!(
            assemble_prompt_sections(std::path::Path::new("/tmp"), dir.path(), &manifest, None),
            Some(vec![
                "内联段落".to_string(),
                "标签段落".to_string(),
                "文件段落".to_string()
            ])
        );

        // sidecar 条目：验证记录无 prompt 能力 → fail-honest 跳过（其余条目保留）
        manifest.prompt = Some(vec![
            PromptSectionDecl::Text("保留".to_string()),
            sidecar.clone(),
        ]);
        assert_eq!(
            assemble_prompt_sections(std::path::Path::new("/tmp"), dir.path(), &manifest, None),
            Some(vec!["保留".to_string()])
        );
        // 验证记录含能力但 sidecar 不可达（/tmp 无安装）→ 跳过不 panic
        let verified = vec![tiangong_plugin_sidecar::protocol::PROMPT_CAPABILITY.to_string()];
        assert_eq!(
            assemble_prompt_sections(
                std::path::Path::new("/tmp"),
                dir.path(),
                &manifest,
                Some(&verified)
            ),
            Some(vec!["保留".to_string()])
        );
        // 未声明 prompt → None（区别于空组装）
        manifest.prompt = None;
        assert_eq!(
            assemble_prompt_sections(std::path::Path::new("/tmp"), dir.path(), &manifest, None),
            None
        );
    }
}
