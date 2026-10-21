//! 安装/启停/回滚/卸载与文件系统事务（staged 校验、换代交换、
//! 数据保留目录、禁用标记、IO 重试工具）。

use super::connections::{
    kill_sidecar_orphans, remove_sidecar_connection, sidecar_binary_path,
    stop_connection_for_directory, stop_loaded_sidecar, unload_plugin_wasm,
};
use super::*;

pub fn install_staged_plugin(storage_root: &Path, staged_path: &Path) -> Result<PluginStatus> {
    install_staged_plugin_inner(storage_root, staged_path, false)
}

/// 导入用户选择的本地插件；允许同版本重新导入，但不允许降级。
pub fn import_staged_plugin(storage_root: &Path, staged_path: &Path) -> Result<PluginStatus> {
    install_staged_plugin_inner(storage_root, staged_path, true)
}

fn install_staged_plugin_inner(
    storage_root: &Path,
    staged_path: &Path,
    allow_same_version: bool,
) -> Result<PluginStatus> {
    let total_started = Instant::now();
    let lock_started = Instant::now();
    let _operation = LOAD_OPERATION
        .write()
        .map_err(|_| anyhow::anyhow!("插件加载操作锁已损坏"))?;
    let lock_wait_ms = lock_started.elapsed().as_millis() as u64;

    let validation_started = Instant::now();
    let staged_result = validate_staged_plugin(storage_root, staged_path);
    let staged_validation_ms = validation_started.elapsed().as_millis() as u64;
    let staged = match staged_result {
        Ok(staged) => staged,
        Err(error) => {
            tracing::warn!(
                lock_wait_ms,
                staged_validation_ms,
                total_ms = total_started.elapsed().as_millis() as u64,
                %error,
                "插件安装目标校验失败"
            );
            return Err(error);
        }
    };
    // sidecar 运行检查移至安装事务之后（尽力而为）：插件包静态校验合法
    // 即完成安装；解释器/沙箱/握手等运行异常交给插件状态管理——插件
    // 管理中显示错误、有 UI 回退 UI、无 UI 调用返回真实运行错误，
    // 用户可重试、停用或删除，不因暂时性运行异常回滚安装。
    let plugin_id = staged.manifest.id.clone();
    let destination = plugin_directory(storage_root, &staged.manifest.id);
    let lookup_started = Instant::now();
    let current = if destination.exists() {
        match find_installed_plugin(storage_root, &plugin_id) {
            Ok(installed) => Some(installed),
            Err(error) => {
                // 保留旧行为：无效残留目录交给 install_new_plugin 的恢复路径处理。
                tracing::warn!(
                    plugin_id,
                    %error,
                    "目标插件现有目录无效，按残留目录恢复路径处理"
                );
                None
            }
        }
    } else {
        None
    };
    let target_lookup_ms = lookup_started.elapsed().as_millis() as u64;

    let is_replacement = current.is_some();
    let switch_started = Instant::now();
    let status = (|| {
        if let Some(current) = current {
            if current.directory != destination {
                bail!(
                    "插件 {} 安装目录与 ID 不一致: {}",
                    staged.manifest.id,
                    current.directory.display()
                );
            }
            ensure_installable_version(&current.manifest, &staged.manifest, allow_same_version)?;
            replace_installed_plugin(storage_root, staged_path, &current, staged.manifest.clone())
        } else {
            install_new_plugin(storage_root, staged_path, staged.manifest.clone())
        }
    })();
    let switch_ms = switch_started.elapsed().as_millis() as u64;
    if status.is_ok() && staged.manifest.sidecar.is_some() {
        post_install_sidecar_check(storage_root, &plugin_id);
    }
    // sidecar 二进制是新落盘文件：macOS 首次执行有一次性的安全评估
    // （实测约 1.6s）。导入完成后后台预热，避免这笔开销落到首次
    // 业务调用（打开终端 / 首次工具执行）上。
    if status.is_ok() {
        prewarm_plugin_sidecar(storage_root, &staged.manifest.id);
    }
    tracing::info!(
        plugin_id,
        allow_same_version,
        lock_wait_ms,
        staged_validation_ms,
        target_lookup_ms,
        switch_ms,
        total_ms = total_started.elapsed().as_millis() as u64,
        success = status.is_ok(),
        "插件安装运行时阶段完成"
    );
    // 迁移成功即广播事实：kind 按是否已有同 ID 插件区分（全新安装/换代）。
    let kind = if is_replacement {
        PluginChangeKind::Upgraded
    } else {
        PluginChangeKind::Installed
    };
    crate::events::announced(kind, &plugin_id, status)
}

/// 安装/升级成功后的 sidecar 运行检查（尽力而为）：临时启动进程完成
/// 认证握手并采集能力，成功则保存能力记录并刷新当前进程的运行状态；
/// 失败（解释器缺失、沙箱不可用、握手失败、清理失败等）仅登记插件
/// 错误状态——插件管理中显示异常，有 UI 插件回退 UI Handler，无 UI
/// 插件调用返回真实运行错误，用户可重试、停用或删除。不回滚安装。
fn post_install_sidecar_check(storage_root: &Path, plugin_id: &str) {
    let Ok(installed) = find_installed_plugin(storage_root, plugin_id) else {
        return;
    };
    post_install_sidecar_check_with(
        plugin_id,
        || crate::verification::verify_installed_sidecar(storage_root, &installed),
        |record| crate::verification::save_verification(&installed.directory, record),
    );
}

// 验证和保存是两个外部操作，状态处理只依赖它们的结果。
// 单元测试可分别注入失败，无需启动真实 sidecar 或改变目录权限。
pub(super) fn post_install_sidecar_check_with(
    plugin_id: &str,
    verify: impl FnOnce() -> Result<crate::verification::SidecarVerification>,
    save: impl FnOnce(&crate::verification::SidecarVerification) -> Result<()>,
) {
    let check_result = verify().and_then(|record| {
        save(&record)?;
        Ok(record)
    });
    match check_result {
        Ok(record) => {
            refresh_verified_sidecar(plugin_id, record.capabilities);
            tracing::info!(plugin_id, "安装后 sidecar 运行检查完成，能力记录已生效");
        }
        Err(error) => {
            tracing::warn!(
                plugin_id,
                %error,
                "安装后 sidecar 运行检查失败：插件保持安装，标记运行异常（可重试验证）"
            );
            set_runtime_error(plugin_id, format!("{error:#}"));
        }
    }
}

/// Launcher 就绪后的常驻 sidecar 补预热：启动期 Launcher 未就绪时预热
/// 失败的插件在此重试。幂等（连接缓存命中即返回）；单个插件失败不阻断
/// 其余插件。范围与启动期预热一致（should_preload_sidecar：原生常驻、
/// 非 terminal/command、不含未使用的解释器）。
pub fn prewarm_resident_sidecars(storage_root: &Path) {
    let targets: Vec<String> = {
        let Ok(plugins) = loaded_plugins().lock() else {
            return;
        };
        plugins
            .values()
            .filter(|loaded| loaded.enabled)
            .filter(|loaded| loaded.manifest.should_preload_sidecar())
            .map(|loaded| loaded.manifest.id.clone())
            .collect()
    };
    #[cfg(not(windows))]
    for plugin_id in targets {
        prewarm_plugin_sidecar(storage_root, &plugin_id);
    }
    #[cfg(windows)]
    {
        let mut targets = targets;
        targets.sort_unstable();
        let storage_root = storage_root.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("prewarm-resident-sidecars".into())
            .spawn(move || {
                let started = Instant::now();
                tracing::info!(plugins = targets.len(), "开始逐个准备常驻插件");
                for plugin_id in targets {
                    if sidecars_shutting_down() {
                        break;
                    }
                    let _ = prewarm_plugin_sidecar_blocking(&storage_root, &plugin_id);
                }
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "常驻插件准备批次结束"
                );
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "创建插件后台准备线程失败");
        }
    }
}

/// 后台预热插件 sidecar：拉起进程并完成握手（幂等，已运行则即时返回）。
/// 失败不影响使用——首个业务调用会按原路径重试启动。
pub fn prewarm_plugin_sidecar(storage_root: &Path, plugin_id: &str) {
    let storage_root = storage_root.to_path_buf();
    let plugin_id = plugin_id.to_string();
    let spawned = std::thread::Builder::new()
        .name(format!("prewarm-sidecar-{plugin_id}"))
        .spawn(move || {
            let _ = prewarm_plugin_sidecar_blocking(&storage_root, &plugin_id);
        });
    if let Err(error) = spawned {
        tracing::debug!(%error, "创建 sidecar 预热线程失败");
    }
}

pub(super) fn prewarm_plugin_sidecar_blocking(storage_root: &Path, plugin_id: &str) -> Result<()> {
    let _operation = LOAD_OPERATION
        .read()
        .map_err(|_| anyhow::anyhow!("插件加载操作锁已损坏"))?;
    if sidecars_shutting_down() {
        bail!("应用正在退出，插件准备已取消");
    }
    let installed = find_installed_plugin(storage_root, plugin_id)?;
    if !installed.enabled || !installed.manifest.should_preload_sidecar() {
        return Ok(());
    }
    if crate::verification::load_valid_capabilities(&installed.directory, &installed.manifest)
        .is_none()
    {
        bail!("插件尚未通过完整验证，无法准备常驻进程");
    }
    let result = resolve_sidecar(storage_root, &installed, false)
        .and_then(|connection| connection.context("常驻插件缺少后台连接"))
        .and_then(|connection| connection.ensure_running());
    match &result {
        Ok(()) => {
            if let Ok(mut plugins) = loaded_plugins().lock()
                && let Some(loaded) = plugins.get_mut(plugin_id)
            {
                loaded.runtime_error = None;
            }
            tracing::info!(plugin_id, "插件 sidecar 预热完成");
        }
        Err(error) => {
            set_runtime_error(plugin_id, format!("{error:#}"));
            tracing::debug!(plugin_id, error = %format!("{error:#}"), "插件 sidecar 预热失败");
        }
    }
    result
}

/// 启用或停用插件，并立即同步所有存活 Core 实例。
///
/// 迁移成功（含无实际变化的幂等短路）即广播插件变化事件。
pub fn set_plugin_enabled(
    storage_root: &Path,
    plugin_id: &str,
    enabled: bool,
) -> Result<PluginStatus> {
    let result = set_plugin_enabled_inner(storage_root, plugin_id, enabled);
    let kind = if enabled {
        PluginChangeKind::Enabled
    } else {
        PluginChangeKind::Disabled
    };
    crate::events::announced(kind, plugin_id, result)
}

fn set_plugin_enabled_inner(
    storage_root: &Path,
    plugin_id: &str,
    enabled: bool,
) -> Result<PluginStatus> {
    let _operation = LOAD_OPERATION
        .write()
        .map_err(|_| anyhow::anyhow!("插件加载操作锁已损坏"))?;
    let installed = find_installed_plugin(storage_root, plugin_id)?;
    if installed.enabled == enabled {
        return list_plugin_status_without_preload(&installed.manifest)
            .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 状态丢失"));
    }

    let marker = installed.directory.join(DISABLED_MARKER);

    // 无 WASM/sidecar 插件的启停只需更新标记、注册表与存活 TS 适配器。
    if installed.manifest.wasm_binary().is_none() && installed.manifest.sidecar.is_none() {
        if enabled {
            remove_file_if_exists(&marker)?;
        } else {
            create_disabled_marker(&marker)?;
            crate::ts_tools::cancel_plugin_calls(plugin_id);
            crate::bridge::clear_plugin_subscriptions(plugin_id);
        }
        let mut plugins = loaded_plugins()
            .lock()
            .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
        let loaded = plugins
            .get_mut(plugin_id)
            .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 尚未加载"))?;
        let ts_instances = loaded
            .ts_instances
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        loaded.enabled = enabled;
        loaded.load_error = None;
        loaded.runtime_error = None;
        drop(plugins);
        for adapter in ts_instances {
            adapter.set_enabled(enabled);
        }
        return list_plugin_status_without_preload(&installed.manifest)
            .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 状态丢失"));
    }

    if enabled {
        remove_file_if_exists(&marker)?;
        let mut enabled_plugin = installed.clone();
        enabled_plugin.enabled = true;
        if let Err(error) = reload_plugin_inner(storage_root, &enabled_plugin) {
            create_disabled_marker(&marker)?;
            set_load_error(plugin_id, error.to_string());
            return Err(error).with_context(|| format!("启用插件 {plugin_id} 失败"));
        }
    } else {
        create_disabled_marker(&marker)?;
        let (instances, ts_instances) = {
            let mut plugins = loaded_plugins()
                .lock()
                .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
            let loaded = plugins
                .get_mut(plugin_id)
                .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 尚未加载"))?;
            loaded.enabled = false;
            loaded.load_error = None;
            loaded.runtime_error = None;
            let instances = loaded
                .instances
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            let ts_instances = loaded
                .ts_instances
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            (instances, ts_instances)
        };
        for adapter in &instances {
            adapter.set_enabled(false);
        }
        for adapter in &ts_instances {
            adapter.set_enabled(false);
        }
        crate::ts_tools::cancel_plugin_calls(plugin_id);
        crate::bridge::clear_plugin_subscriptions(plugin_id);
        if let Err(error) = stop_connection_for_directory(&installed.directory) {
            for adapter in &instances {
                adapter.set_enabled(true);
            }
            for adapter in &ts_instances {
                adapter.set_enabled(true);
            }
            if let Ok(mut plugins) = loaded_plugins().lock()
                && let Some(loaded) = plugins.get_mut(plugin_id)
            {
                loaded.enabled = true;
                loaded.runtime_error = Some(error.to_string());
            }
            remove_file_if_exists(&marker)?;
            return Err(error).with_context(|| format!("停用插件 {plugin_id} 失败"));
        }
    }

    let mut refreshed = installed;
    refreshed.enabled = enabled;
    list_plugin_status_without_preload(&refreshed.manifest)
        .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 状态丢失"))
}

/// 后台补验证/重新验证成功后，同步刷新注册表与存活 TS 适配器的验证能力
///（已构建的 Core 实例立即按新能力路由，无需等待下一次 Core 构建）。
pub(crate) fn refresh_verified_sidecar(plugin_id: &str, capabilities: Vec<String>) {
    let (manifest, enabled, ts_instances) = {
        let Ok(mut plugins) = loaded_plugins().lock() else {
            return;
        };
        let Some(loaded) = plugins.get_mut(plugin_id) else {
            return;
        };
        loaded.verified_sidecar = Some(capabilities.clone());
        // 运行检查重新成功：sidecar 已可真实握手，清除此前登记的启动
        // 异常（load_error 属安装前提类，不受影响）。
        loaded.runtime_error = None;
        (
            loaded.manifest.clone(),
            loaded.enabled,
            loaded
                .ts_instances
                .iter()
                .filter_map(|weak| weak.upgrade())
                .collect::<Vec<_>>(),
        )
    };
    for adapter in ts_instances {
        adapter.reconfigure(&manifest, enabled, Some(capabilities.clone()));
    }
}

/// 重新验证插件 sidecar 并保存记录（后台补验证失败后的重试入口）。
///
/// 同步执行完整验证（有限时限），成功后立即刷新运行时能力；失败返回
/// 具体原因且保留既有记录（若有）。
pub fn reverify_plugin_sidecar(storage_root: &Path, plugin_id: &str) -> Result<PluginStatus> {
    let installed = find_installed_plugin(storage_root, plugin_id)?;
    if installed.manifest.sidecar.is_none() {
        anyhow::bail!("插件 {plugin_id} 未声明 sidecar，无需验证");
    }
    let record = crate::verification::verify_installed_sidecar(storage_root, &installed)?;
    crate::verification::save_verification(&installed.directory, &record)?;
    refresh_verified_sidecar(plugin_id, record.capabilities);
    prewarm_plugin_sidecar(storage_root, plugin_id);
    list_plugin_status_without_preload(&installed.manifest)
        .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 状态丢失"))
}

/// 将插件切换到本地保留的上一个版本，失败时恢复当前版本。
pub fn rollback_plugin(storage_root: &Path, plugin_id: &str) -> Result<PluginStatus> {
    let result = rollback_plugin_inner(storage_root, plugin_id);
    crate::events::announced(PluginChangeKind::Upgraded, plugin_id, result)
}

fn rollback_plugin_inner(storage_root: &Path, plugin_id: &str) -> Result<PluginStatus> {
    let _operation = LOAD_OPERATION
        .write()
        .map_err(|_| anyhow::anyhow!("插件加载操作锁已损坏"))?;
    let current = find_installed_plugin(storage_root, plugin_id)?;
    let rollback = rollback_directory(&current.directory, plugin_id);
    if !rollback.is_dir() {
        bail!("插件 {plugin_id} 没有可回滚版本");
    }

    stop_loaded_sidecar(plugin_id)?;
    crate::ts_tools::cancel_plugin_calls(plugin_id);
    let transaction = transaction_directory(storage_root, "rollback")?;
    swap_with_rollback(&current.directory, &rollback, &transaction, current.enabled)?;

    // 回滚后制品变化，现有验证记录（摘要锚定新制品）随之失效；删除记录
    // 并触发补验证，为回滚版本重建能力快照。
    crate::verification::remove_verification(&current.directory);
    crate::verification::reverify_installed_sidecars(storage_root);

    let rolled_back = find_installed_plugin(storage_root, plugin_id)?;
    if let Err(error) = reload_plugin_inner(storage_root, &rolled_back) {
        let _ = stop_connection_for_directory(&current.directory);
        let restore_transaction = transaction_directory(storage_root, "rollback-restore")?;
        let restore_result = swap_with_rollback(
            &current.directory,
            &rollback,
            &restore_transaction,
            current.enabled,
        )
        .and_then(|()| reload_plugin_inner(storage_root, &current));
        if let Err(restore_error) = restore_result {
            bail!("回滚插件 {plugin_id} 失败: {error}; 恢复当前版本失败: {restore_error}");
        }
        set_load_error(plugin_id, error.to_string());
        return Err(error).with_context(|| format!("回滚插件 {plugin_id} 失败"));
    }

    list_plugin_status_without_preload(&rolled_back.manifest)
        .ok_or_else(|| anyhow::anyhow!("插件 {plugin_id} 回滚后状态丢失"))
}

/// 卸载插件。保留数据时，安装目录中只留下 data 目录。
///
/// 无效插件目录（签名无效/沙箱越权/清单损坏）无法通过 `find_installed_plugin`
/// 校验，先查无效插件登记表，命中则直接走同一删除路径后返回。
pub fn uninstall_plugin(storage_root: &Path, plugin_id: &str, keep_data: bool) -> Result<()> {
    let result = uninstall_plugin_inner(storage_root, plugin_id, keep_data);
    crate::events::announced(PluginChangeKind::Uninstalled, plugin_id, result)
}

fn uninstall_plugin_inner(storage_root: &Path, plugin_id: &str, keep_data: bool) -> Result<()> {
    let _operation = LOAD_OPERATION
        .write()
        .map_err(|_| anyhow::anyhow!("插件加载操作锁已损坏"))?;
    #[cfg(windows)]
    {
        let cache = persistent_grants_path(storage_root, plugin_id);
        if cache.is_file() {
            stop_loaded_sidecar(plugin_id)?;
            stop_connection_for_directory(&plugin_directory(storage_root, plugin_id))?;
            crate::launcher_update::revoke_persistent_grants(storage_root, &cache)?;
        }
    }
    if remove_invalid_plugin_if_registered(storage_root, plugin_id, keep_data)? {
        return Ok(());
    }
    let installed = find_installed_plugin(storage_root, plugin_id)?;
    let expected = plugin_directory(storage_root, plugin_id);
    if installed.directory != expected {
        bail!(
            "插件 {plugin_id} 安装目录与 ID 不一致: {}",
            installed.directory.display()
        );
    }
    stop_loaded_sidecar(plugin_id)?;
    crate::ts_tools::cancel_plugin_calls(plugin_id);
    crate::bridge::clear_plugin_subscriptions(plugin_id);
    // 补齐连接表兜底：插件不在 loaded_plugins 时，仍可能保留在 sidecar 连接表中，
    // 不一并停止会导致 Windows 上二进制文件被占用、卸载删除失败。
    stop_connection_for_directory(&installed.directory)?;
    kill_sidecar_orphans(&installed);
    unload_plugin_wasm(plugin_id);

    let removed = transaction_directory(storage_root, "uninstall")?;
    rename_with_retry(&installed.directory, &removed)?;
    let uninstall_result = if keep_data {
        preserve_only_data(&removed, &installed.directory)
    } else {
        remove_directory_if_exists(&removed)
    };
    if let Err(error) = uninstall_result {
        tracing::error!(plugin_id, %error, "卸载插件删除目录失败，尝试恢复插件");
        let restore_result = (|| {
            remove_directory_if_exists(&installed.directory)?;
            rename_with_retry(&removed, &installed.directory)?;
            reload_plugin_inner(storage_root, &installed)
        })();
        if let Err(restore_error) = restore_result {
            bail!("卸载插件 {plugin_id} 失败: {error}; 恢复插件失败: {restore_error}");
        }
        return Err(error).with_context(|| format!("卸载插件 {plugin_id} 失败"));
    }

    let rollback = rollback_directory(&installed.directory, plugin_id);
    if let Err(error) = remove_directory_if_exists(&rollback) {
        tracing::warn!(path = %rollback.display(), %error, "插件已卸载，但清理回滚目录失败");
    }
    // 验证记录随插件一起删除（记录锚定制品摘要，目录已不存在即失效）。
    crate::verification::remove_verification(&installed.directory);
    remove_sidecar_connection(&installed.directory);
    if let Ok(mut plugins) = loaded_plugins().lock()
        && let Some(loaded) = plugins.remove(plugin_id)
    {
        for adapter in loaded
            .instances
            .into_iter()
            .filter_map(|item| item.upgrade())
        {
            adapter.set_enabled(false);
        }
        for adapter in loaded
            .ts_instances
            .into_iter()
            .filter_map(|item| item.upgrade())
        {
            adapter.set_enabled(false);
        }
    }
    Ok(())
}

fn validate_staged_plugin(storage_root: &Path, staged_path: &Path) -> Result<InstalledPlugin> {
    let transactions = plugins_directory(storage_root).join(".transactions");
    if staged_path.parent() != Some(transactions.as_path()) {
        bail!("插件临时目录不在受管事务目录中: {}", staged_path.display());
    }
    ensure_directory(staged_path)?;
    let manifest = PluginManifest::load(&staged_path.join(MANIFEST_FILE))?;
    let signed_release = verify_signed_release(staged_path, &manifest)?;
    manifest.validate_ui_native_sandbox(signed_release.is_some())?;
    let installed = InstalledPlugin {
        directory: staged_path.to_path_buf(),
        manifest,
        enabled: true,
        signed_release,
    };
    let result = (|| {
        let wasm_bytes = match installed.manifest.wasm_binary() {
            Some(_) => Some(Arc::new(read_wasm_bytes(&installed)?)),
            None => None,
        };
        let sidecar = resolve_sidecar(storage_root, &installed, true)?;
        if let Some(sidecar_manifest) = &installed.manifest.sidecar {
            match sidecar_manifest.runtime {
                crate::manifest::SidecarRuntime::Native => {
                    let binary = sidecar_manifest.binary.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "插件 {} native sidecar 缺少 binary 声明",
                            installed.manifest.id
                        )
                    })?;
                    let binary = sidecar_binary_path(staged_path, binary)?;
                    if !binary.is_file() {
                        bail!("插件 sidecar 制品不存在: {}", binary.display());
                    }
                }
                crate::manifest::SidecarRuntime::Node | crate::manifest::SidecarRuntime::Python => {
                    let entry = sidecar_manifest.entry.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "插件 {} 解释器 sidecar 缺少 entry 声明",
                            installed.manifest.id
                        )
                    })?;
                    let entry_path = staged_path.join(entry);
                    if !entry_path.is_file() {
                        bail!("插件 sidecar 入口脚本不存在: {}", entry_path.display());
                    }
                }
            }
        }
        compile_plugin(
            wasm_bytes.clone().unwrap_or_else(|| Arc::new(Vec::new())),
            &installed.manifest,
            sidecar,
            PluginRuntimeConfig::default(),
        )?;
        let _ = wasm_bytes;
        Ok(())
    })();
    remove_sidecar_connection(staged_path);
    result?;
    Ok(installed)
}

fn install_new_plugin(
    storage_root: &Path,
    staged_path: &Path,
    manifest: PluginManifest,
) -> Result<PluginStatus> {
    let destination = plugin_directory(storage_root, &manifest.id);
    // 目录已存在但插件未被注册（如签名校验失败被忽略的旧版残留）：
    // 按升级路径原子切换并保留数据目录，而不是拒绝导入。
    if destination.exists() {
        match PluginManifest::load(&destination.join(MANIFEST_FILE)) {
            Ok(existing_manifest) => {
                let existing = InstalledPlugin {
                    directory: destination.clone(),
                    manifest: existing_manifest,
                    enabled: true,
                    signed_release: None,
                };
                return replace_installed_plugin(storage_root, staged_path, &existing, manifest);
            }
            Err(_) => {
                // 无法解析的坏残留：经事务目录中转后删除，再全新安装。
                let discard = transaction_directory(storage_root, "discard-stale")?;
                std::fs::rename(&destination, &discard)?;
                if let Err(error) = remove_directory_if_exists(&discard) {
                    tracing::warn!(path = %discard.display(), %error, "清理坏残留目录失败");
                }
            }
        }
    }
    let retained = if destination.exists() {
        validate_retained_data_directory(&destination)?;
        let retained = transaction_directory(storage_root, "retained-data")?;
        std::fs::rename(&destination, &retained)?;
        if let Err(error) = move_entry(&retained, staged_path, "data") {
            let _ = std::fs::rename(&retained, &destination);
            return Err(error);
        }
        Some(retained)
    } else {
        None
    };

    if let Err(error) = std::fs::rename(staged_path, &destination) {
        if let Some(retained) = &retained {
            let _ = move_entry(staged_path, retained, "data");
            let _ = std::fs::rename(retained, &destination);
        }
        return Err(error).with_context(|| format!("安装插件 {} 失败", manifest.id));
    }
    // 安装成功：数据保留壳（data 已挪入新目录）随即清理，不在事务目录累积。
    if let Some(retained) = &retained
        && let Err(error) = remove_directory_if_exists(retained)
    {
        tracing::warn!(path = %retained.display(), %error, "清理数据保留目录失败");
    }

    let installed = InstalledPlugin {
        directory: destination.clone(),
        manifest: manifest.clone(),
        enabled: true,
        signed_release: verify_signed_release(&destination, &manifest)?,
    };
    let loaded = load_plugin_record(storage_root, installed);
    // 回滚只针对安装前提类失败：逻辑制品无法加载、解释器不可发现、
    // 签名/信任构造失败（load_error）。常驻 sidecar 启动失败属于运行
    // 异常（runtime_error）——保留安装，插件管理显示启动异常。
    if (loaded.ui_plugin.is_none() && manifest.wasm_binary().is_some())
        || loaded.load_error.is_some()
    {
        let error = loaded
            .load_error
            .unwrap_or_else(|| "WASM 插件加载失败".to_string());
        let _ = stop_connection_for_directory(&destination);
        std::fs::rename(&destination, staged_path)?;
        if let Some(retained) = &retained {
            move_entry(staged_path, retained, "data")?;
            std::fs::rename(retained, &destination)?;
        }
        bail!("安装插件 {} 失败: {error}", manifest.id);
    }
    // sidecar 运行检查与能力记录由 post_install_sidecar_check 在安装
    // 事务提交后统一处理（尽力而为，失败不回滚）。
    loaded_plugins()
        .lock()
        .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?
        .insert(manifest.id.clone(), loaded);
    if let Some(retained) = retained
        && let Err(error) = remove_directory_if_exists(&retained)
    {
        tracing::warn!(path = %retained.display(), %error, "插件已安装，但清理数据迁移目录失败");
    }
    list_plugin_status_without_preload(&manifest)
        .ok_or_else(|| anyhow::anyhow!("插件 {} 安装后状态丢失", manifest.id))
}

fn replace_installed_plugin(
    storage_root: &Path,
    staged_path: &Path,
    current: &InstalledPlugin,
    manifest: PluginManifest,
) -> Result<PluginStatus> {
    stop_loaded_sidecar(&current.manifest.id)?;
    // 升级同样补齐连接表兜底，确保旧 sidecar 进程被停止后再替换二进制文件，
    // 避免 Windows 上旧进程占用导致目录切换或旧文件清理失败。
    stop_connection_for_directory(&current.directory)?;
    kill_sidecar_orphans(current);
    crate::ts_tools::cancel_plugin_calls(&current.manifest.id);
    unload_plugin_wasm(&current.manifest.id);
    let rollback = rollback_directory(&current.directory, &current.manifest.id);
    if let Some(parent) = rollback.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let saved_rollback = if rollback.exists() {
        let saved = transaction_directory(storage_root, "previous-rollback")?;
        std::fs::rename(&rollback, &saved)?;
        Some(saved)
    } else {
        None
    };

    let switch_result = (|| {
        rename_with_retry(&current.directory, &rollback)?;
        move_preserved_entries(&rollback, staged_path)?;
        set_disabled_marker(staged_path, !current.enabled)?;
        rename_with_retry(staged_path, &current.directory)?;
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = switch_result {
        tracing::error!(plugin_id = %current.manifest.id, %error, "切换插件目录失败，尝试恢复旧版本");
        let _ = restore_upgrade_directories(staged_path, &current.directory, &rollback);
        let _ = restore_saved_rollback(&rollback, saved_rollback.as_deref());
        let _ = reload_plugin_inner(storage_root, current);
        return Err(error).with_context(|| format!("切换插件 {} 目录失败", current.manifest.id));
    }

    let upgraded = InstalledPlugin {
        directory: current.directory.clone(),
        manifest: manifest.clone(),
        enabled: current.enabled,
        signed_release: verify_signed_release(&current.directory, &manifest)?,
    };
    if let Err(error) = reload_plugin_inner(storage_root, &upgraded) {
        tracing::error!(plugin_id = %current.manifest.id, %error, "升级后重新加载插件失败，尝试恢复旧版本");
        let _ = stop_connection_for_directory(&current.directory);
        let restore_result =
            restore_upgrade_directories(staged_path, &current.directory, &rollback)
                .and_then(|()| restore_saved_rollback(&rollback, saved_rollback.as_deref()))
                .and_then(|()| reload_plugin_inner(storage_root, current));
        if let Err(restore_error) = restore_result {
            bail!(
                "升级插件 {} 失败: {error}; 恢复旧版本失败: {restore_error}",
                current.manifest.id
            );
        }
        set_load_error(&current.manifest.id, error.to_string());
        return Err(error).with_context(|| format!("升级插件 {} 失败", current.manifest.id));
    }
    // sidecar 运行检查与能力记录由 post_install_sidecar_check 在升级
    // 提交后统一处理（尽力而为，失败保留新版本并标记运行异常）。
    if let Some(saved) = saved_rollback
        && let Err(error) = remove_directory_if_exists(&saved)
    {
        tracing::warn!(path = %saved.display(), %error, "插件已升级，但清理旧回滚目录失败");
    }
    list_plugin_status_without_preload(&manifest)
        .ok_or_else(|| anyhow::anyhow!("插件 {} 升级后状态丢失", manifest.id))
}

fn restore_upgrade_directories(staged: &Path, destination: &Path, rollback: &Path) -> Result<()> {
    if destination.exists() {
        rename_with_retry(destination, staged)?;
    }
    if PRESERVED_ENTRIES
        .iter()
        .any(|entry| staged.join(entry).exists())
    {
        move_preserved_entries(staged, rollback)?;
    }
    if rollback.exists() {
        rename_with_retry(rollback, destination)?;
    }
    Ok(())
}

fn restore_saved_rollback(rollback: &Path, saved: Option<&Path>) -> Result<()> {
    if let Some(saved) = saved
        && saved.exists()
    {
        std::fs::rename(saved, rollback)?;
    }
    Ok(())
}

fn swap_with_rollback(
    destination: &Path,
    rollback: &Path,
    transaction: &Path,
    enabled: bool,
) -> Result<()> {
    std::fs::rename(destination, transaction)?;
    if let Err(error) = std::fs::rename(rollback, destination) {
        let _ = std::fs::rename(transaction, destination);
        return Err(error.into());
    }
    if let Err(error) = move_preserved_entries(transaction, destination) {
        let _ = std::fs::rename(destination, rollback);
        let _ = std::fs::rename(transaction, destination);
        return Err(error);
    }
    if let Err(error) = set_disabled_marker(destination, !enabled) {
        let _ = move_preserved_entries(destination, transaction);
        let _ = std::fs::rename(destination, rollback);
        let _ = std::fs::rename(transaction, destination);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(transaction, rollback) {
        let _ = move_preserved_entries(destination, transaction);
        let _ = std::fs::rename(destination, rollback);
        let _ = std::fs::rename(transaction, destination);
        return Err(error.into());
    }
    Ok(())
}

pub(super) fn preserve_only_data(source: &Path, destination: &Path) -> Result<()> {
    let data = source.join("data");
    std::fs::create_dir_all(destination)?;
    if data.exists() {
        std::fs::rename(&data, destination.join("data"))?;
    } else {
        std::fs::create_dir_all(destination.join("data"))?;
    }
    if let Err(error) = remove_directory_if_exists(source) {
        tracing::warn!(path = %source.display(), %error, "插件已卸载并保留数据，但清理旧制品失败");
    }
    Ok(())
}

fn validate_retained_data_directory(path: &Path) -> Result<()> {
    ensure_directory(path)?;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() != "data" || !entry.file_type()?.is_dir() {
            bail!("插件目录已存在且不是可恢复的数据目录: {}", path.display());
        }
    }
    Ok(())
}

fn ensure_installable_version(
    current: &PluginManifest,
    next: &PluginManifest,
    allow_same_version: bool,
) -> Result<()> {
    let current_version = Version::parse(&current.version)
        .with_context(|| format!("当前插件 {} 版本无效", current.id))?;
    let next_version =
        Version::parse(&next.version).with_context(|| format!("插件 {} 新版本无效", next.id))?;
    if next_version < current_version {
        bail!(
            "插件 {} 导入版本 {} 低于当前版本 {}",
            current.id,
            next.version,
            current.version
        );
    }
    if !allow_same_version && next_version == current_version {
        bail!(
            "插件 {} 可安装版本 {} 不高于当前版本 {}",
            current.id,
            next.version,
            current.version
        );
    }
    Ok(())
}

fn move_preserved_entries(source: &Path, destination: &Path) -> Result<()> {
    for entry in PRESERVED_ENTRIES {
        remove_directory_if_exists(&destination.join(entry))?;
    }

    let mut moved = Vec::new();
    for entry in PRESERVED_ENTRIES {
        let source_entry = source.join(entry);
        if !source_entry.exists() {
            continue;
        }
        let destination_entry = destination.join(entry);
        if let Err(error) = rename_with_retry(&source_entry, &destination_entry) {
            for moved_entry in moved.into_iter().rev() {
                let _ = std::fs::rename(destination.join(moved_entry), source.join(moved_entry));
            }
            return Err(error);
        }
        moved.push(entry);
    }
    Ok(())
}

fn move_entry(source: &Path, destination: &Path, entry: &str) -> Result<()> {
    let source = source.join(entry);
    if !source.exists() {
        return Ok(());
    }
    let destination = destination.join(entry);
    remove_directory_if_exists(&destination)?;
    std::fs::rename(&source, &destination).with_context(|| {
        format!(
            "迁移插件目录失败: {} -> {}",
            source.display(),
            destination.display()
        )
    })
}

fn set_disabled_marker(directory: &Path, disabled: bool) -> Result<()> {
    let marker = directory.join(DISABLED_MARKER);
    if disabled {
        create_disabled_marker(&marker)
    } else {
        remove_file_if_exists(&marker)
    }
}

fn create_disabled_marker(path: &Path) -> Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("创建插件停用标记失败: {}", path.display()))
        }
    }
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("删除文件失败: {}", path.display())),
    }
}

pub(super) fn ensure_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("读取目录失败: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("路径不是受管目录: {}", path.display());
    }
    Ok(())
}

/// 判断 IO 错误是否源于文件被占用。
///
/// Windows 上停止 sidecar 进程后，其二进制 image 的文件锁释放可能滞后于进程退出，
/// 紧随其后的目录改名/删除会撞上占用。ACCESS_DENIED(5) 与 SHARING_VIOLATION(32)
/// 即此类暂时性占用；其它平台极少出现，保留判断以备用。
fn is_file_locked(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5) | Some(32))
}

/// 对暂时性文件占用做有限重试的 IO 包装。
///
/// 这类占用通常在进程退出后数百毫秒内自行释放，故按固定间隔重试至超时；
/// 非占用错误立即向上抛出，避免无谓等待。
fn retry_io<F>(mut operation: F) -> Result<()>
where
    F: FnMut() -> std::io::Result<()>,
{
    const MAX_WAIT: Duration = Duration::from_secs(8);
    const INTERVAL: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + MAX_WAIT;
    let mut warned = false;
    loop {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error) if is_file_locked(&error) && Instant::now() < deadline => {
                if !warned {
                    warned = true;
                    tracing::warn!(
                        code = error.raw_os_error(),
                        "文件被占用，开始等待重试（进程退出后句柄或杀毒扫描释放可能有延迟）"
                    );
                }
                std::thread::sleep(INTERVAL);
            }
            Err(error) => return Err(anyhow::Error::from(error)),
        }
    }
}

/// 重命名文件/目录，遇到暂时性占用时重试。
pub(super) fn rename_with_retry(from: &Path, to: &Path) -> Result<()> {
    retry_io(|| std::fs::rename(from, to))
        .with_context(|| format!("重命名失败: {} -> {}", from.display(), to.display()))
}

pub(super) fn remove_directory_if_exists(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("拒绝删除符号链接目录: {}", path.display())
        }
        Ok(_) => retry_io(|| std::fs::remove_dir_all(path))
            .with_context(|| format!("删除目录失败: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("读取目录失败: {}", path.display())),
    }
}

pub(super) fn transaction_directory(storage_root: &Path, label: &str) -> Result<PathBuf> {
    let root = plugins_directory(storage_root).join(".transactions");
    std::fs::create_dir_all(&root)?;
    Ok(root.join(format!("{}-{label}", scru128::new())))
}

pub(super) fn plugins_directory(storage_root: &Path) -> PathBuf {
    storage_root.join("plugins")
}

pub(super) fn plugin_directory(storage_root: &Path, plugin_id: &str) -> PathBuf {
    plugins_directory(storage_root).join(plugin_id)
}

pub(super) fn rollback_directory(directory: &Path, plugin_id: &str) -> PathBuf {
    directory
        .parent()
        .unwrap_or(directory)
        .join(ROLLBACK_DIR)
        .join(plugin_id)
}
