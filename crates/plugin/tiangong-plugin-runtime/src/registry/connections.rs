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

/// 停用插件的全部存活适配器（升级/卸载切换窗口内不可再接调用）。
///
/// 适配器不持有进程资源，但在切换窗口内必须停用，避免并发工具调用
/// 重新取得旧 sidecar/目录资源。
pub(super) fn unload_plugin_adapters(plugin_id: &str) {
    let mut plugins = match loaded_plugins().lock() {
        Ok(plugins) => plugins,
        Err(error) => {
            tracing::error!(plugin_id, %error, "插件注册表已损坏，无法停用适配器");
            return;
        }
    };
    let Some(loaded) = plugins.get_mut(plugin_id) else {
        return;
    };
    for adapter in loaded.ts_instances.iter().filter_map(Weak::upgrade) {
        adapter.set_enabled(false);
    }
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
    .with_sandbox_capabilities(host_policy.capabilities.clone())
    .with_sandbox_user_policy(&tiangong_config::registry::try_sandbox_policy());
    // 统一写域：宿主会话工作区 + 存储根（敏感清单双禁由传输层施加）。
    // 没有会话上下文的全局调用才使用应用默认工作区；带会话上下文时
    // 不采信插件 payload，也不允许默认工作区覆盖当前对话。
    let session_workspace_is_default = session_workspace.is_none();
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
        // 插件开发目录写域（host_policy 按已验证官方身份授予）：devkit
        // CLI 经终端命令通道写入 plugins-dev（项目骨架与构建产物）；导入
        // 收件箱 plugins/.inbox 承载待安装产物（`.` 前缀使 discovery 不扫描，
        // 正式安装目录 plugins/ 不在授权之列——安装仍经宿主 plugin-dev 通道
        // 的构建指纹核验或用户原生确认把关）。目录可能尚未初始化（新装机
        // 首次开发/导入），先确保存在——传输层的 retain_existing_writable_
        // roots 会静默丢弃不存在的目录；符号链接或非目录项视为篡改，
        // 拒绝授权（fail-closed）。
        if host_policy.plugin_dev_write {
            for writable_dir in [
                storage_root.join(crate::plugin_dev::PLUGIN_DEV_DIR),
                storage_root
                    .join("plugins")
                    .join(crate::plugin_dev::PLUGIN_INBOX_DIR),
            ] {
                let granted = match std::fs::symlink_metadata(&writable_dir) {
                    Ok(meta) if meta.file_type().is_dir() => Some(writable_dir),
                    Ok(_) => {
                        tracing::warn!(
                            path = %writable_dir.display(),
                            "插件开发写域目录不是实体目录，跳过授权"
                        );
                        None
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        match std::fs::create_dir_all(&writable_dir) {
                            Ok(()) => Some(writable_dir),
                            Err(error) => {
                                tracing::warn!(
                                    %error,
                                    path = %writable_dir.display(),
                                    "创建插件开发写域目录失败，跳过授权"
                                );
                                None
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            path = %writable_dir.display(),
                            "检查插件开发写域目录失败，跳过授权"
                        );
                        None
                    }
                };
                if let Some(writable_dir) = granted {
                    config = config.push_sandbox_extra_writable(writable_dir);
                }
            }
        }
    }
    if let Some(signed_release) = signed_release {
        // 最小权限：逐文件按已验签权限开放读取；未授权项保持禁读。
        config = config.with_sensitive_storage(crate::sidecar::SensitiveStorageAccess {
            model_config: signed_release.has_permission("model-config.read"),
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
    // 常驻插件的共享连接是唯一主进程：独占外部资源（IM 长连接等）只在
    // 主进程建立；会话工作区连接与临时连接均为辅助进程。
    config = config.with_primary_instance(
        installed.manifest.should_preload_sidecar() && session_workspace_is_default,
    );
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
    let plugins = loaded_plugins()
        .lock()
        .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?;
    let Some(loaded) = plugins.get(&installed.manifest.id) else {
        return Ok(false);
    };
    let manifest_match =
        serde_json::to_value(&loaded.manifest)? == serde_json::to_value(&installed.manifest)?;
    Ok(loaded.directory == installed.directory
        && loaded.enabled == installed.enabled
        && manifest_match)
}
