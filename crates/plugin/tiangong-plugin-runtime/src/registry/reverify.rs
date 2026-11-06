//! sidecar 验证编排：何时验证、用哪条连接验证、失败如何记录。
//!
//! 纯记录读写属于信任域（`crate::verification`）；本模块是状态机对验证的
//! 编排——发现待验证插件、决定复用常驻连接还是起临时连接、把能力写回运行
//! 态、把失败记到插件运行错误上。分开放置，避免信任域反过来依赖状态机。
use crate::protocol::PROTOCOL_VERSION;
use crate::verification::{
    SidecarVerification, artifact_digest, load_valid_capabilities, save_verification,
};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
/// 后台补验证并发防抖。
static REVERIFY_RUNNING: AtomicBool = AtomicBool::new(false);

/// 对已安装（或暂存）插件执行完整验证并生成验证记录。
///
/// 临时启动 sidecar（或复用常驻进程）完成认证握手，校验身份、版本与
/// 协议兼容后采集能力列表；随后计算制品摘要组装记录。调用方负责在
/// 安装事务成功后保存记录。
///
/// 记录缺失或失效时走专属临时连接，不进入运行时共享连接表：验证失败时
/// 坏连接与存活进程不会留给业务调用；已有记录仍与当前制品一致时，允许
/// 复用同版本常驻连接做运行状态复核。
pub(crate) fn verify_installed_sidecar(
    storage_root: &Path,
    installed: &crate::registry::InstalledPlugin,
) -> Result<SidecarVerification> {
    if installed.manifest.sidecar.is_none() {
        bail!("插件 {} 未声明 sidecar，无需验证", installed.manifest.id);
    }
    let (connection, stop_after_verification) =
        if let Some(connection) = crate::registry::resident_sidecar_for_verification(installed) {
            (connection, false)
        } else {
            (
                crate::registry::ephemeral_sidecar_connection(storage_root, installed)?,
                true,
            )
        };
    // 临时连接必须在验证后清理；已加载的同版本常驻连接属于业务运行态，
    // 只复用其已校验握手能力，不能为补验证而停止。
    let verification_result = connection
        .verify_capabilities()
        .with_context(|| format!("插件 {} sidecar 完整验证失败", installed.manifest.id));
    let stop_result = if stop_after_verification {
        connection.stop()
    } else {
        Ok(())
    };
    let capabilities = match (verification_result, stop_result) {
        (Ok(capabilities), Ok(())) => capabilities,
        (Err(verify_error), Ok(())) => return Err(verify_error),
        (Ok(_), Err(stop_error)) => {
            return Err(stop_error).context(format!(
                "插件 {} sidecar 验证成功，但验证进程清理失败",
                installed.manifest.id
            ));
        }
        (Err(verify_error), Err(stop_error)) => {
            bail!(
                "插件 {} sidecar 验证失败：{verify_error:#}；验证进程清理同时失败：{stop_error:#}",
                installed.manifest.id
            );
        }
    };
    let digest = artifact_digest(&installed.directory)?;
    if !stop_after_verification
        && load_valid_capabilities(&installed.directory, &installed.manifest).is_none()
    {
        bail!(
            "插件 {} 在复用常驻进程验证期间制品发生变化，拒绝更新验证记录",
            installed.manifest.id
        );
    }
    Ok(SidecarVerification {
        plugin_id: installed.manifest.id.clone(),
        plugin_version: installed.manifest.version.clone(),
        artifact_digest: digest,
        protocol_version: PROTOCOL_VERSION.to_string(),
        capabilities,
        verified_at: chrono::Local::now().naive_local().to_string(),
    })
}

/// 后台补验证：为有 sidecar 但没有有效验证记录的已安装插件补做完整
/// 验证。成功保存记录；失败记录插件错误状态（有 UI 的插件运行时回退
/// UI Handler，无 UI 的插件调用时返回真实运行错误）。
///
/// 不阻塞调用方；并发调用由全局防抖合并。
pub fn reverify_installed_sidecars(storage_root: &Path) {
    if REVERIFY_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let storage_root = storage_root.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("reverify-sidecars".to_string())
        .spawn(move || {
            let _ = reverify_installed_sidecars_blocking(&storage_root, true, None);
            REVERIFY_RUNNING.store(false, Ordering::Release);
        });
    if let Err(error) = spawned {
        REVERIFY_RUNNING.store(false, Ordering::Release);
        tracing::debug!(%error, "创建 sidecar 补验证线程失败");
    }
}

/// 同步补验证（后台线程体；测试直接调用）。
pub(crate) fn reverify_installed_sidecars_blocking(
    storage_root: &Path,
    prewarm_after_verification: bool,
    entrypoint: Option<&str>,
) -> Result<()> {
    let (installed_plugins, _) = crate::registry::discover_installed_plugins(storage_root);
    let mut failures = Vec::new();
    for installed in installed_plugins {
        if installed.manifest.sidecar.is_none()
            || !installed.enabled
            || entrypoint.is_some_and(|entry| !installed.manifest.available_at(entry))
        {
            continue;
        }
        if let Some(capabilities) =
            load_valid_capabilities(&installed.directory, &installed.manifest)
        {
            crate::registry::refresh_verified_sidecar(&installed.manifest.id, capabilities);
            continue;
        }
        let Some(_operation) = crate::registry::background_sidecar_operation() else {
            bail!("插件补验证已取消：应用正在退出或加载锁不可用");
        };
        if let Some(capabilities) =
            load_valid_capabilities(&installed.directory, &installed.manifest)
        {
            crate::registry::refresh_verified_sidecar(&installed.manifest.id, capabilities);
            continue;
        }
        let result = verify_installed_sidecar(storage_root, &installed).and_then(|record| {
            save_verification(&installed.directory, &record)
                .context("保存 sidecar 验证记录失败")?;
            crate::registry::refresh_verified_sidecar(&installed.manifest.id, record.capabilities);
            if prewarm_after_verification {
                crate::registry::prewarm_plugin_sidecar(storage_root, &installed.manifest.id);
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                tracing::info!(plugin_id = %installed.manifest.id, "旧插件 sidecar 补验证完成")
            }
            Err(error) => {
                let reason = format!("{error:#}");
                tracing::warn!(plugin_id = %installed.manifest.id, error = %reason, "旧插件 sidecar 补验证失败");
                crate::registry::set_runtime_error(&installed.manifest.id, reason.clone());
                failures.push(format!("{}: {reason}", installed.manifest.id));
            }
        }
    }
    if !failures.is_empty() {
        bail!("插件验证失败：\n{}", failures.join("\n"));
    }
    Ok(())
}
