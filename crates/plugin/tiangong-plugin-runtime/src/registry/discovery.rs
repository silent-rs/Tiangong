//! 磁盘发现、预加载与插件记录构建（含 WASM 预编译与 prewarm）。

use super::connections::remove_sidecar_connection;
use super::*;

/// 预加载设置页实例，供尚未创建 Core 时查询插件贡献。
pub fn preload_installed_plugins(storage_root: &Path) -> usize {
    preload_installed_plugins_inner(storage_root, false)
        .unwrap_or_else(|error| {
            tracing::warn!(error = %format!("{error:#}"), "插件预加载失败");
            StartupPluginReadiness::default()
        })
        .loaded
}

/// 桌面启动准备结果：插件级失败只写入各插件异常状态并汇总在 failures，
/// 不再阻断应用启动——对话不依赖插件，工具调用时会得到各自的失败原因。
#[derive(Debug, Default, Clone)]
pub struct StartupPluginReadiness {
    pub loaded: usize,
    pub failures: Vec<String>,
}

/// 桌面启动准备：全部验证与常驻进程准备完成后才返回，不把任务留给首次发送。
pub fn prepare_desktop_startup_plugins(storage_root: &Path) -> Result<StartupPluginReadiness> {
    preload_installed_plugins_inner(storage_root, true)
}

fn preload_installed_plugins_inner(
    storage_root: &Path,
    wait_for_ready: bool,
) -> Result<StartupPluginReadiness> {
    if sidecars_shutting_down() {
        bail!("应用正在退出，插件准备已取消");
    }
    migrate_legacy_plugin_ids(storage_root);

    let _operation = LOAD_OPERATION
        .write()
        .map_err(|_| anyhow::anyhow!("插件加载操作锁已损坏"))?;
    if wait_for_ready {
        match std::fs::read_dir(storage_root.join("plugins")) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("读取已安装插件目录失败"),
        }
    }
    if sidecars_shutting_down() {
        bail!("应用正在退出，插件准备已取消");
    }

    let (installed_plugins, discovered_invalid) = discover_installed_plugins(storage_root);
    let mut failures = discovered_invalid
        .iter()
        .filter(|entry| {
            let directory = storage_root.join("plugins").join(&entry.id);
            !directory.join(DISABLED_MARKER).is_file()
                && PluginManifest::load(&directory.join(MANIFEST_FILE))
                    .map(|manifest| manifest.available_at("desktop"))
                    .unwrap_or(true)
        })
        .map(|entry| format!("{}: {}", entry.id, entry.reason))
        .collect::<Vec<_>>();
    *invalid_plugins()
        .lock()
        .map_err(|_| anyhow::anyhow!("无效插件登记锁已损坏"))? = discovered_invalid;
    for installed in &installed_plugins {
        if sidecars_shutting_down() {
            bail!("应用正在退出，插件准备已取消");
        }
        // 幂等跳过仅当登记属于本次扫描到的同一安装目录：同 id 但目录
        // 不同（如测试以不同 storage root 反复预加载）必须以最后一次
        // 为准，否则注册表沿用已失效的旧目录，桥接等按目录推导的路径
        // 会写进旧根。
        let unchanged = loaded_plugins()
            .lock()
            .map(|plugins| {
                plugins.get(&installed.manifest.id).is_some_and(|loaded| {
                    loaded.directory == installed.directory
                        && loaded.manifest.version == installed.manifest.version
                        && loaded.enabled == installed.enabled
                        && loaded.load_error.is_none()
                })
            })
            .unwrap_or(false);
        if unchanged {
            continue;
        }
        if let Ok(plugins) = loaded_plugins().lock()
            && let Some(previous) = plugins.get(&installed.manifest.id)
        {
            remove_sidecar_connection(&previous.directory);
        }
        let loaded = load_plugin_record_with_prewarm(
            storage_root,
            installed.clone(),
            !wait_for_ready && !cfg!(windows),
        );
        loaded_plugins()
            .lock()
            .map_err(|_| anyhow::anyhow!("插件登记锁已损坏"))?
            .insert(installed.manifest.id.clone(), loaded);
    }
    drop(_operation);
    // 插件登记完成即开始保活：此后常驻 sidecar 意外退出或启动预热失败，
    // 都由运行时在后台重新拉起，各入口无需单独处理。
    super::keepalive::ensure_keepalive_started();
    if wait_for_ready {
        if let Err(error) = crate::verification::reverify_installed_sidecars_blocking(
            storage_root,
            false,
            Some("desktop"),
        ) {
            failures.push(format!("{error:#}"));
        }
        for installed in installed_plugins
            .iter()
            .filter(|installed| installed.enabled && installed.manifest.available_at("desktop"))
        {
            if sidecars_shutting_down() {
                bail!("应用正在退出，插件准备已取消");
            }
            if let Err(error) =
                prewarm_plugin_sidecar_blocking(storage_root, &installed.manifest.id)
            {
                failures.push(format!("{}: {error:#}", installed.manifest.id));
            }
            let plugins = loaded_plugins()
                .lock()
                .map_err(|_| anyhow::anyhow!("插件登记锁已损坏"))?;
            if let Some(error) = plugins
                .get(&installed.manifest.id)
                .and_then(|loaded| loaded.load_error.as_ref())
            {
                failures.push(format!("{}: {error}", installed.manifest.id));
            }
        }
        if sidecars_shutting_down() {
            bail!("应用正在退出，插件准备已取消");
        }
        // 插件级失败已逐个写入 runtime_error / invalid 登记，这里只汇总
        // 供启动层提示，不再整体报错阻断应用进入。
        failures.sort();
        failures.dedup();
        return Ok(StartupPluginReadiness {
            loaded: installed_plugins.len(),
            failures,
        });
    }
    // 存量旧插件（升级前安装）可能没有验证记录：后台补做完整验证，
    // 不阻塞应用启动，也不在工具调用热路径同步执行。
    crate::verification::reverify_installed_sidecars(storage_root);
    #[cfg(windows)]
    prewarm_resident_sidecars(storage_root);

    Ok(StartupPluginReadiness {
        loaded: installed_plugins.len(),
        failures,
    })
}

/// 若 `plugin_id` 是已登记的无效插件目录则删除它并返回 true。
///
/// 无效插件从未进入注册表，没有 sidecar 或 WASM 实例需要停止；但删除路径
/// 与正常卸载一致（事务目录暂存、可选保留 data）。登记表中的 id 均来自
/// 实际扫描到的目录名，天然不含路径分隔符，不会越出插件根目录。
pub(super) fn remove_invalid_plugin_if_registered(
    storage_root: &Path,
    plugin_id: &str,
    keep_data: bool,
) -> Result<bool> {
    let registered = invalid_plugins()
        .lock()
        .map_err(|_| anyhow::anyhow!("无效插件登记表已损坏"))?
        .clone();
    if !registered.iter().any(|entry| entry.id == plugin_id) {
        return Ok(false);
    }
    let directory = plugins_directory(storage_root).join(plugin_id);
    let removed = transaction_directory(storage_root, "uninstall-invalid")?;
    let result = (|| -> Result<()> {
        if !directory.is_dir() {
            // 目录已不存在（如用户手动删除）：仅同步登记表。
            return Ok(());
        }
        ensure_directory(&directory)?;
        rename_with_retry(&directory, &removed)?;
        let uninstall_result = if keep_data {
            preserve_only_data(&removed, &directory)
        } else {
            remove_directory_if_exists(&removed)
        };
        if let Err(error) = uninstall_result {
            // 无效插件无运行时状态，恢复目录即可让用户重试。
            let _ = remove_directory_if_exists(&directory);
            let _ = rename_with_retry(&removed, &directory);
            return Err(error);
        }
        Ok(())
    })();
    invalid_plugins()
        .lock()
        .map_err(|_| anyhow::anyhow!("无效插件登记表已损坏"))?
        .retain(|entry| entry.id != plugin_id);
    result?;
    tracing::info!(plugin_id, keep_data, "已清理无效插件目录");
    Ok(true)
}

pub(super) fn load_plugin_record(storage_root: &Path, installed: InstalledPlugin) -> LoadedPlugin {
    load_plugin_record_with_prewarm(storage_root, installed, true)
}

fn load_plugin_record_with_prewarm(
    storage_root: &Path,
    installed: InstalledPlugin,
    prewarm: bool,
) -> LoadedPlugin {
    // 连接构造失败（解释器不可发现、签名/信任/权限不合法）属于安装
    // 前提类错误：写入 load_error，安装路径据此回滚。
    let signed_release = installed.signed_release.clone();
    let sidecar_result = resolve_sidecar(storage_root, &installed, false);
    let (sidecar, load_error) = match sidecar_result {
        Ok(connection) => (connection, None),
        Err(error) => (None, Some(error.to_string())),
    };
    // 读取安装阶段保存的验证记录（校验 ID、版本、制品摘要与协议兼容）；
    // 缺失或失效时先由独立进程完成补验证，再启动常驻进程，避免数据库类
    // 插件的验证进程与业务进程争用同一数据目录。
    let verified_sidecar =
        crate::verification::load_valid_capabilities(&installed.directory, &installed.manifest);
    // 常驻 sidecar 启动失败是运行异常：保留安装，插件管理显示启动异常
    //（重试验证/修复环境后恢复），不作为安装回滚条件。
    let mut runtime_error = None;
    if prewarm
        && installed.enabled
        && installed.manifest.should_preload_sidecar()
        && verified_sidecar.is_some()
        && let Some(connection) = &sidecar
        && let Err(error) = connection.ensure_running()
    {
        tracing::warn!(plugin_id = %installed.manifest.id, %error, "插件 sidecar 暂不可用");
        runtime_error = Some(error.to_string());
    }
    // 纯 UI 插件（wasm 省略）：无逻辑层，直接构造已加载记录
    let load_result = if installed.manifest.wasm_binary().is_some() {
        read_wasm_bytes(&installed)
            .map(|bytes| {
                let bytes = Arc::new(bytes);
                compile_plugin(
                    bytes.clone(),
                    &installed.manifest,
                    sidecar.clone(),
                    PluginRuntimeConfig::default(),
                )
                .map(|(component, plugin, descriptor)| (Some(bytes), component, plugin, descriptor))
            })
            .and_then(|result| result)
    } else {
        Ok((None, None, None, None))
    };

    match load_result {
        Ok((bytes, component, plugin, descriptor)) => {
            tracing::info!(plugin_id = %installed.manifest.id, "WASM 插件已预加载");
            LoadedPlugin {
                directory: installed.directory,
                manifest: installed.manifest,
                signed_release,
                wasm_bytes: bytes,
                component,
                ui_plugin: plugin.map(|plugin| Arc::new(Mutex::new(plugin))),
                descriptor,
                generation: 1,
                instances: Vec::new(),
                ts_instances: Vec::new(),
                sidecar,
                verified_sidecar,
                load_error,
                runtime_error,
                enabled: installed.enabled,
            }
        }
        Err(error) => {
            tracing::warn!(plugin_id = %installed.manifest.id, %error, "加载 WASM 插件失败");
            LoadedPlugin {
                directory: installed.directory,
                manifest: installed.manifest,
                signed_release,
                wasm_bytes: None,
                component: None,
                ui_plugin: None,
                descriptor: None,
                generation: 0,
                instances: Vec::new(),
                ts_instances: Vec::new(),
                sidecar,
                verified_sidecar,
                load_error: load_error.or_else(|| Some(error.to_string())),
                runtime_error,
                enabled: installed.enabled,
            }
        }
    }
}

/// 仅当磁盘上已有记录仍与当前制品一致时，重新验证才可复用同版本、同目录
/// 的常驻连接。记录缺失或失效时必须使用独立验证进程，不能让旧进程为当前
/// 磁盘内容生成新的验证记录。
pub(crate) fn resident_sidecar_for_verification(
    installed: &InstalledPlugin,
) -> Option<Arc<dyn SidecarConnection>> {
    if !installed.manifest.should_preload_sidecar()
        || crate::verification::load_valid_capabilities(&installed.directory, &installed.manifest)
            .is_none()
    {
        return None;
    }
    let plugins = loaded_plugins().lock().ok()?;
    let loaded = plugins.get(&installed.manifest.id)?;
    (loaded.directory == installed.directory
        && loaded.manifest.version == installed.manifest.version
        && loaded.enabled)
        .then(|| loaded.sidecar.clone())
        .flatten()
}

/// 为指定插件创建一个 Core 适配器实例（WASM 或 Desktop TS 工具）。
///
/// 每次调用创建**新适配器**（per-Core 隔离 per-session 状态）并把 Weak
/// 登记进注册表——此后该插件的启停/升级由 runtime 经 Weak 就地更新这个
/// （含原因），随 `list_plugins` 展示，并支持经 `uninstall_plugin` 清理。
pub(crate) fn discover_installed_plugins(
    storage_root: &Path,
) -> (Vec<InstalledPlugin>, Vec<InvalidPluginEntry>) {
    let plugins_dir = storage_root.join("plugins");
    let Ok(entries) = std::fs::read_dir(&plugins_dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut manifest_paths = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            // `.transactions`、`.rollback` 与旧版 `.id-staging-*` 都是内部
            // 事务目录，即使包含完整 manifest 也绝不是有效安装目录。
            (!name.to_string_lossy().starts_with('.')).then(|| entry.path().join(MANIFEST_FILE))
        })
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    manifest_paths.sort();

    let mut installed_plugins = Vec::new();
    let mut invalid_plugins = Vec::new();
    for path in manifest_paths {
        let Some(directory) = path.parent().map(Path::to_path_buf) else {
            continue;
        };
        let directory_name = directory
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        match PluginManifest::load(&path) {
            Ok(manifest) => {
                if manifest.id != directory_name {
                    invalid_plugins.push(InvalidPluginEntry {
                        id: directory_name.clone(),
                        name: manifest.id.clone(),
                        manifest_version: Some(manifest.version.clone()),
                        reason: format!(
                            "插件目录名 {directory_name} 与清单 ID {} 不一致",
                            manifest.id
                        ),
                    });
                    continue;
                }
                let signed_release = match verify_signed_release(&directory, &manifest) {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "忽略签名无效的插件");
                        invalid_plugins.push(InvalidPluginEntry {
                            id: directory_name,
                            name: manifest.id.clone(),
                            manifest_version: Some(manifest.version.clone()),
                            reason: error.to_string(),
                        });
                        continue;
                    }
                };
                if let Err(error) = manifest.validate_ui_native_sandbox(signed_release.is_some()) {
                    tracing::warn!(path = %path.display(), %error, "忽略沙箱声明越权的插件");
                    invalid_plugins.push(InvalidPluginEntry {
                        id: directory_name,
                        name: manifest.id.clone(),
                        manifest_version: Some(manifest.version.clone()),
                        reason: error.to_string(),
                    });
                    continue;
                }
                installed_plugins.push(InstalledPlugin {
                    enabled: !directory.join(DISABLED_MARKER).is_file(),
                    directory,
                    manifest,
                    signed_release,
                });
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %format!("{error:#}"), "忽略无效插件清单");
                invalid_plugins.push(InvalidPluginEntry {
                    id: directory_name.clone(),
                    // 清单读不出来时用目录名作展示名。
                    name: directory_name,
                    manifest_version: None,
                    reason: error.to_string(),
                });
            }
        }
    }
    (installed_plugins, invalid_plugins)
}

pub(crate) fn find_installed_plugin(
    storage_root: &Path,
    plugin_id: &str,
) -> Result<InstalledPlugin> {
    if plugin_id.is_empty()
        || !plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || plugin_id == "."
        || plugin_id == ".."
    {
        bail!("插件 ID 无效: {plugin_id}");
    }

    // 单插件操作直接读取目标目录，不能扫描并验签全部插件。
    let directory = plugin_directory(storage_root, plugin_id);
    let directory_metadata = std::fs::symlink_metadata(&directory)
        .with_context(|| format!("插件未安装: {plugin_id}"))?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        bail!("插件安装路径必须是实际目录: {}", directory.display());
    }
    let manifest_path = directory.join(MANIFEST_FILE);
    let manifest_metadata = std::fs::symlink_metadata(&manifest_path)
        .with_context(|| format!("插件未安装: {plugin_id}"))?;
    if manifest_metadata.file_type().is_symlink() || !manifest_metadata.is_file() {
        bail!("插件清单必须是实际文件: {}", manifest_path.display());
    }
    let manifest = PluginManifest::load(&manifest_path)?;
    if manifest.id != plugin_id {
        bail!(
            "插件安装目录与清单 ID 不一致: expected={plugin_id}, actual={}",
            manifest.id
        );
    }
    let signed_release = verify_signed_release(&directory, &manifest)?;
    manifest.validate_ui_native_sandbox(signed_release.is_some())?;
    Ok(InstalledPlugin {
        enabled: !directory.join(DISABLED_MARKER).is_file(),
        directory,
        manifest,
        signed_release,
    })
}

pub(super) fn resolve_sidecar(
    storage_root: &Path,
    installed: &InstalledPlugin,
    refresh: bool,
) -> Result<Option<Arc<dyn SidecarConnection>>> {
    if installed.manifest.sidecar.is_none() {
        return Ok(None);
    }
    sidecar_connection(storage_root, installed, refresh).map(Some)
}
