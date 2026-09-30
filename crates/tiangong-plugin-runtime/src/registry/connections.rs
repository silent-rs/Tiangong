//! sidecar 连接池：按目录复用、换代刷新、按需/临时连接与停机清理。

use super::*;

pub(super) fn sidecar_binary_path(directory: &Path, binary: &Path) -> Result<PathBuf> {
    let mut path = directory.join(binary);
    if !std::env::consts::EXE_SUFFIX.is_empty() {
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow::anyhow!("sidecar 文件名无效"))?;
        if !file_name.ends_with(std::env::consts::EXE_SUFFIX) {
            path.set_file_name(format!("{file_name}{}", std::env::consts::EXE_SUFFIX));
        }
    }
    Ok(path)
}

pub(super) fn stop_loaded_sidecar(plugin_id: &str) -> Result<()> {
    let sidecar = loaded_plugins()
        .lock()
        .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?
        .get(plugin_id)
        .and_then(|loaded| loaded.sidecar.clone());
    if let Some(sidecar) = sidecar {
        sidecar.stop()?;
    }
    Ok(())
}

pub(crate) fn stop_connection_for_directory(directory: &Path) -> Result<()> {
    let connections = sidecar_connections()
        .lock()
        .map_err(|_| anyhow::anyhow!("插件 sidecar 连接表已损坏"))?
        .iter()
        .filter(|(key, _)| key.directory == directory)
        .map(|(_, connection)| connection.clone())
        .collect::<Vec<_>>();
    // 先摘表再停进程：任一连接停止失败也不能让连接残留在表中（残留
    // 会被下次调用直接复用并打到旧二进制）。停止失败聚合成首个错误
    // 照常上报——停用回滚、卸载的 Windows 二进制占用保护等调用方依赖
    // 失败语义；需要尽力而为语义的调用点（如热加载）自行吞错告警。
    remove_sidecar_connection(directory);
    let mut first_error = None;
    for connection in connections {
        if let Err(error) = connection.stop() {
            tracing::warn!(
                directory = %directory.display(),
                %error,
                "停止 sidecar 连接失败（连接已从表中摘除）"
            );
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// 兜底按二进制 image 名清理该插件的所有残留 sidecar 进程。
///
/// 注册表里的连接未必覆盖全部进程——热加载覆盖连接时旧 sidecar 进程可能成为
/// 孤儿，持续占用二进制文件。升级/卸载改写二进制前按 image 名再清一遍，避免
/// 目录改名/删除因文件被占用而失败。
pub(super) fn kill_sidecar_orphans(installed: &InstalledPlugin) {
    let Some(sidecar) = installed.manifest.sidecar.as_ref() else {
        return;
    };
    // 解释器 sidecar 的进程 image 是系统 node/python，按 image 清理会误杀无关
    // 进程；其孤儿治理完全依赖 stdio 连接的宿主绑定生命周期（EOF/进程组）。
    if sidecar.runtime != crate::manifest::SidecarRuntime::Native {
        return;
    }
    let binary = sidecar
        .binary
        .as_deref()
        .map(|binary| {
            sidecar_binary_path(&installed.directory, binary)
                .unwrap_or_else(|_| installed.directory.join(binary))
        })
        .unwrap_or_else(|| installed.directory.clone());
    crate::sidecar::kill_sidecar_processes_by_image(&binary);
}

/// 卸载插件的 WASM 实例（drop Store），释放其对安装目录的 WASI preopen 句柄。
///
/// Windows 上 cap-std 打开目录不带 FILE_SHARE_DELETE，已加载实例的 Store 会长期持有
/// 安装目录句柄，阻止其被 rename/delete，导致升级切换、卸载删除失败（code=32）。
/// 在改写目录前调用本函数清空实例，让目录可被改写；后续 reload 会重建实例。
pub(super) fn unload_plugin_wasm(plugin_id: &str) {
    let adapters = {
        let mut plugins = match loaded_plugins().lock() {
            Ok(plugins) => plugins,
            Err(error) => {
                tracing::error!(plugin_id, %error, "插件注册表已损坏，无法卸载 WASM 实例");
                return;
            }
        };
        let Some(loaded) = plugins.get_mut(plugin_id) else {
            return;
        };
        loaded.ui_plugin = None;
        loaded.component = None;
        loaded.wasm_bytes = None;
        loaded.descriptor = None;
        let adapters = loaded
            .instances
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        // Desktop TS 适配器不持有 WASM Store，但在切换窗口内必须停用，
        // 避免并发工具调用重新取得旧 sidecar/目录资源。
        for adapter in loaded.ts_instances.iter().filter_map(Weak::upgrade) {
            adapter.set_enabled(false);
        }
        adapters
    };
    for adapter in adapters {
        adapter.release_inner();
    }
    // `ui_plugin` 可能已经被设置页命令克隆并正在处理。等待短暂窗口让命令
    // 释放其 Store；真正永久占用不应靠 8 秒 rename 重试掩盖。
    #[cfg(windows)]
    std::thread::sleep(Duration::from_millis(50));
}

pub(crate) fn remove_sidecar_connection(directory: &Path) {
    if let Ok(mut connections) = sidecar_connections().lock() {
        connections.retain(|key, _| key.directory != directory);
    }
}

/// 扫描插件目录，返回可加载插件与被忽略的无效插件。
///
/// 无效插件不再静默丢弃：签名无效、沙箱声明越权、清单损坏的目录都会登记
pub(crate) fn sidecar_connection(
    storage_root: &Path,
    installed: &InstalledPlugin,
    refresh: bool,
) -> Result<Arc<dyn SidecarConnection>> {
    sidecar_connection_inner(storage_root, installed, refresh, false, None)
}

/// 按 ID 反查插件当前现役 sidecar 连接（WASM 宿主状态刷新过期引用用）。
///
/// 与加载路径同键查表：连接被 server 端点变化等停止机制换代后，这里返回
/// 新连接（spawn 时携带新注入的环境）；未安装或未声明 sidecar 返回 None。
pub(crate) fn sidecar_connection_for_plugin(plugin_id: &str) -> Option<Arc<dyn SidecarConnection>> {
    let directory = plugin_install_directory(plugin_id)?;
    let storage_root = directory.parent()?.parent()?.to_path_buf();
    let installed = find_installed_plugin(&storage_root, plugin_id).ok()?;
    sidecar_connection(&storage_root, &installed, false).ok()
}

/// 检测长期持有的连接引用是否已被停止换代（server 端点变化等触发的
/// 依赖插件重启）：已停止且注册表能查到现役连接时返回换代后的新连接，
/// 否则原样返回（查不到时调用方保留旧引用、按原错误上报）。
///
/// 静态注入连接的两类持有方共用：WASM 宿主状态（调用转发）与适配器
/// （会话取消）——取消若仍打旧引用，换代后的调用将无法取消。
pub(crate) fn refresh_stale_sidecar(
    sidecar: Option<&Arc<dyn SidecarConnection>>,
    plugin_id: &str,
) -> Option<Arc<dyn SidecarConnection>> {
    if sidecar.is_some_and(|conn| conn.is_stopped())
        && let Some(connection) = sidecar_connection_for_plugin(plugin_id)
    {
        tracing::info!(plugin_id, "sidecar 连接已换代，过期引用刷新");
        return Some(connection);
    }
    sidecar.cloned()
}

/// 带宿主权威会话工作区的连接构造。
pub(crate) fn sidecar_connection_with_workspace(
    storage_root: &Path,
    installed: &InstalledPlugin,
    refresh: bool,
    session_workspace: Option<&Path>,
) -> Result<Arc<dyn SidecarConnection>> {
    let session_workspace = session_workspace
        .map(canonicalize_session_workspace)
        .transpose()?;
    sidecar_connection_inner(storage_root, installed, refresh, false, session_workspace)
}

/// 临时验证连接（安装期完整验证用）：与按需直连同样不进入共享连接表，
/// 连接及其进程完全归属验证流程——成功与失败路径都必须显式停止，
/// 避免验证失败后把坏连接或存活进程留给业务调用与后续安装。
pub(crate) fn ephemeral_sidecar_connection(
    storage_root: &Path,
    installed: &InstalledPlugin,
) -> Result<Arc<dyn SidecarConnection>> {
    sidecar_connection_inner(storage_root, installed, false, true, None)
}

fn canonicalize_session_workspace(workspace: &Path) -> Result<PathBuf> {
    if !workspace.is_absolute() {
        bail!("当前会话工作区必须是绝对路径: {}", workspace.display());
    }
    let workspace = std::fs::canonicalize(workspace)
        .with_context(|| format!("解析当前会话工作区失败: {}", workspace.display()))?;
    if !workspace.is_dir() {
        bail!("当前会话工作区不是目录: {}", workspace.display());
    }
    Ok(workspace)
}

/// 临时连接（按需直连的并发隔离用）：走同样的启动门槛与配置构造，但不
/// 进入共享连接表——连接及其进程完全归属发起本次调用的执行方，超时/取消
/// 只影响自己，并发调用互不可见。
pub(crate) fn ephemeral_sidecar_connection_with_workspace(
    storage_root: &Path,
    installed: &InstalledPlugin,
    session_workspace: Option<&Path>,
) -> Result<Arc<dyn SidecarConnection>> {
    let session_workspace = session_workspace
        .map(canonicalize_session_workspace)
        .transpose()?;
    sidecar_connection_inner(storage_root, installed, false, true, session_workspace)
}

fn sidecar_connection_inner(
    storage_root: &Path,
    installed: &InstalledPlugin,
    refresh: bool,
    ephemeral: bool,
    session_workspace: Option<PathBuf>,
) -> Result<Arc<dyn SidecarConnection>> {
    let sidecar = installed
        .manifest
        .sidecar
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("插件 {} 未声明 sidecar", installed.manifest.id))?;
    // 启动门槛（三分支）：官方签名照旧；未签名插件仅当"解释器形态 + 本地信任
    // （安装时原生确认落锚 + 内容哈希清单复核通过）"时放行；其余拒绝。
    let interpreter = match sidecar.runtime {
        SidecarRuntime::Native => None,
        SidecarRuntime::Node | SidecarRuntime::Python => {
            Some(resolve_interpreter_launch(installed, sidecar)?)
        }
    };
    let signed_release = match installed.signed_release.as_ref() {
        Some(signed_release) => {
            if installed.directory.join(LOCAL_TRUST_FILE).is_file() {
                bail!(
                    "插件 {} 同时携带官方签名与本地信任标记，来源不明确，拒绝启动",
                    installed.manifest.id
                );
            }
            // 官方签名解释器 sidecar：验签时已按内容清单完成全树校验
            // （signature.rs validate），按官方信任放行。
            Some(signed_release)
        }
        None => {
            let trusted = interpreter.is_some() && verify_local_trust(&installed.directory)?;
            if !trusted {
                bail!(
                    "未签名插件 {} 不允许启动原生 sidecar（解释器 sidecar 需签名安装：官方目录、创作链自动签名或已导入的第三方公钥）",
                    installed.manifest.id
                );
            }
            None
        }
    };
    if signed_release.is_some_and(|release| !release.has_permission("sidecar.invoke")) {
        bail!("插件 {} 的签名未授权 sidecar.invoke", installed.manifest.id);
    }
    if !installed.manifest.permissions.is_empty()
        && !installed.manifest.has_permission("sidecar.invoke")
    {
        bail!("插件 {} 未声明 sidecar.invoke 权限", installed.manifest.id);
    }
    // 沙箱策略由宿主权威策略表决定（RFC 0017 透明执行封套）：
    // 不读 manifest 的 sandbox / sandbox_network（插件自声明是提权通道）。
    let official_signed =
        signed_release.is_some_and(|release| release.publisher == crate::trust::OFFICIAL_PUBLISHER);
    if installed.manifest.id == "command" && !official_signed {
        bail!("command 插件必须由官方发布者签名");
    }
    let host_policy = crate::host_policy::resolve(&installed.manifest.id, official_signed);
    // 用户全局沙箱开关只控制首次实际使用才启动的 sidecar：terminal、
    // command、解释器和清单声明的按需插件。随 App 启动的预加载常驻服务
    // 继续执行宿主强制沙箱；配置读取失败按开启处理（fail-safe）。
    let follows_user_sandbox_switch = !installed.manifest.should_preload_sidecar();
    let use_stdio = interpreter.is_some()
        || host_policy.transport == crate::host_policy::SidecarTransport::Stdio;

    // native：插件目录内可执行文件（补平台后缀）；解释器：宿主白名单程序 + 入口。
    let binary = match interpreter.as_ref() {
        None => {
            let raw = sidecar.binary.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "插件 {} native sidecar 缺少 binary 声明",
                    installed.manifest.id
                )
            })?;
            sidecar_binary_path(&installed.directory, raw)?
        }
        Some(launch) => launch.entry.clone(),
    };
    let endpoint = installed.directory.join("runtime").join("endpoint.json");
    let log = installed.directory.join("logs").join("sidecar.log");
    let data_dir = installed.directory.join("data");
    let (server_url, server_token) = current_server_endpoint()
        .map(|(url, token)| (Some(url), token))
        .unwrap_or((None, None));
    let mut config = SidecarConfig::new(
        &installed.manifest.id,
        &installed.manifest.version,
        binary,
        endpoint,
        log,
        data_dir,
        storage_root,
    )
    .with_timeouts(
        Duration::from_millis(sidecar.startup_timeout_ms),
        Duration::from_millis(sidecar.request_timeout_ms),
    )
    .with_server_endpoint(server_url, server_token)
    .with_sandbox(host_policy.sandbox)
    .with_sandbox_user_switch(follows_user_sandbox_switch)
    .with_sandbox_program_root(Some(installed.directory.clone()))
    .with_sandbox_network(host_policy.allow_network)
    .with_user_credential_reads(host_policy.user_credential_reads)
    .with_sandbox_user_cache_write(host_policy.allow_user_cache_write)
    .with_sandbox_audio_input(host_policy.allow_audio_input)
    .with_sandbox_user_policy(&tiangong_config::registry::try_sandbox_policy());
    // 统一写域：宿主会话工作区 + 存储根（敏感清单双禁由传输层施加）。
    // 没有会话上下文的全局调用才使用应用默认工作区；带会话上下文时
    // 不采信插件 payload，也不允许默认工作区覆盖当前对话。
    if host_policy.sandbox {
        let workspace = session_workspace.or_else(|| {
            let configured = tiangong_config::registry::config().workspace_dir.clone();
            let path = PathBuf::from(configured);
            path.is_dir()
                .then(|| std::fs::canonicalize(&path).unwrap_or(path))
        });
        if let Some(workspace) = workspace {
            config = config.with_sandbox_workspace(Some(workspace));
        }
    }
    if let Some(signed_release) = signed_release {
        // 最小权限：逐文件按已验签权限开放读取；未授权项保持禁读。
        config = config.with_sensitive_storage(crate::sidecar::SensitiveStorageAccess {
            model_config: signed_release.has_permission("model-config.read"),
            mcp_config: installed.manifest.id == "mcp",
            server_config: signed_release.has_permission("app-storage.read"),
            app_config: false,
        });
    }
    if let Some(launch) = interpreter {
        // 本地信任解释器 sidecar：spawn 前按内容清单复核文件树，防安装后篡改。
        config = config
            .with_interpreter(launch)
            .with_integrity_manifest(installed.directory.join(CONTENT_MANIFEST_FILE));
    }
    config = config.with_lifecycle(installed.manifest.sidecar_lifecycle());

    if ephemeral {
        return Ok(if installed.manifest.id == "command" {
            Arc::new(EphemeralCommandConnection::new(config)) as Arc<dyn SidecarConnection>
        } else if use_stdio {
            Arc::new(StdioSidecarConnection::new(config)) as Arc<dyn SidecarConnection>
        } else {
            Arc::new(ProcessSidecarConnection::new(config))
        });
    }
    let connection_key = SidecarConnectionKey {
        directory: installed.directory.clone(),
        workspace: config.sandbox_workspace.clone(),
    };
    let mut connections = sidecar_connections()
        .lock()
        .map_err(|_| anyhow::anyhow!("插件 sidecar 连接表已损坏"))?;
    if refresh {
        connections.retain(|key, _| key.directory != installed.directory);
    }
    if !connections.contains_key(&connection_key) {
        // 通信通道由宿主策略表权威决定（插件不声明）：spawn 时注入的
        // 环境变量即通道选择，sidecar 通用库自动适配。
        let connection: Arc<dyn SidecarConnection> = if installed.manifest.id == "command" {
            Arc::new(EphemeralCommandConnection::new(config))
        } else if use_stdio {
            Arc::new(StdioSidecarConnection::new(config))
        } else {
            Arc::new(ProcessSidecarConnection::new(config))
        };
        connections.insert(connection_key.clone(), connection);
    }
    connections
        .get(&connection_key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("创建插件 sidecar 连接失败"))
}

/// 解析解释器 sidecar 的启动规格：宿主白名单程序 + 入口绝对路径 + 清单参数。
fn resolve_interpreter_launch(
    installed: &InstalledPlugin,
    sidecar: &crate::manifest::SidecarManifest,
) -> Result<InterpreterLaunch> {
    let entry = sidecar.entry.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "插件 {} 解释器 sidecar 缺少 entry 声明",
            installed.manifest.id
        )
    })?;
    let entry_path = installed.directory.join(entry);
    if !entry_path.is_file() {
        bail!("插件 sidecar 入口脚本不存在: {}", entry_path.display());
    }
    // 加载期预解析一次：提前暴露"未找到解释器"（写入插件 last_error）
    // 并预热缓存；运行期每次启动均经缓存入口取最新路径。
    resolve_interpreter_program(sidecar.runtime)?;
    Ok(InterpreterLaunch {
        kind: match sidecar.runtime {
            SidecarRuntime::Native => bail!("native sidecar 无解释器"),
            SidecarRuntime::Node => InterpreterKind::Node,
            SidecarRuntime::Python => InterpreterKind::Python,
        },
        entry: entry_path,
        args: sidecar.args.clone(),
    })
}

/// 解析解释器程序：统一走应用级缓存入口（见 interpreter_env 模块），
/// registry 不再自行读取环境变量或构造候选路径。
fn resolve_interpreter_program(runtime: SidecarRuntime) -> Result<PathBuf> {
    let kind = match runtime {
        SidecarRuntime::Native => bail!("native sidecar 无解释器"),
        SidecarRuntime::Node => InterpreterKind::Node,
        SidecarRuntime::Python => InterpreterKind::Python,
    };
    interpreter_env::resolve_interpreter(kind)
}

/// 在 PATH 中查找解释器程序（可经环境变量固定路径）。
///
/// GUI 进程（launchd/Finder 启动）不执行 shell 初始化，nvm/Homebrew 等
/// 安装位置不在其 PATH 中，PATH 未命中后继续探测常见安装位置；入口
/// 本地信任校验：安装时落锚的标记与内容清单哈希一致，且清单内全部文件未被篡改。
fn verify_local_trust(directory: &Path) -> Result<bool> {
    #[derive(serde::Deserialize)]
    struct LocalTrust {
        content_sha256: String,
    }
    let Ok(raw) = std::fs::read_to_string(directory.join(LOCAL_TRUST_FILE)) else {
        return Ok(false);
    };
    let trust: LocalTrust = serde_json::from_str(&raw).with_context(|| {
        format!(
            "插件本地信任标记损坏: {}",
            directory.join(LOCAL_TRUST_FILE).display()
        )
    })?;
    let manifest_path = directory.join(CONTENT_MANIFEST_FILE);
    let manifest_raw = std::fs::read(&manifest_path)
        .with_context(|| format!("本地信任插件缺少内容清单: {}", manifest_path.display()))?;
    let anchored = hex::encode(sha2::Sha256::digest(&manifest_raw));
    if !anchored.eq_ignore_ascii_case(&trust.content_sha256) {
        bail!(
            "插件内容清单与本地信任标记不一致（可能被篡改），拒绝启动: {}",
            directory.display()
        );
    }
    crate::sidecar::SidecarConfig::verify_integrity_manifest(&manifest_path, directory)?;
    Ok(true)
}

pub(super) fn loaded_plugin_matches(installed: &InstalledPlugin) -> Result<bool> {
    // 纯 UI/TS 插件无 WASM 字节可比较，完整清单也必须参与判断，
    // 否则工具、提示词或 UI 贡献变化不会触发热更新。
    let bytes = installed
        .manifest
        .wasm_binary()
        .map(|_| read_wasm_bytes(installed))
        .transpose()?;
    let plugins = loaded_plugins()
        .lock()
        .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
    let Some(loaded) = plugins.get(&installed.manifest.id) else {
        return Ok(false);
    };
    let bytes_match = match (&loaded.wasm_bytes, &bytes) {
        (Some(loaded_bytes), Some(bytes)) => loaded_bytes.as_slice() == bytes.as_slice(),
        (None, None) => true,
        _ => false,
    };
    let manifest_match =
        serde_json::to_value(&loaded.manifest)? == serde_json::to_value(&installed.manifest)?;
    Ok(loaded.directory == installed.directory
        && loaded.enabled == installed.enabled
        && manifest_match
        && bytes_match)
}
