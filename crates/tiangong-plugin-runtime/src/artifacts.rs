//! 基于 OSS 静态目录的插件制品发现、下载与校验。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use semver::Version;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::manifest::{MANIFEST_FILE, PluginManifest};

pub use tiangong_plugin_sidecar::release::{
    PLUGIN_CATALOG_ENDPOINT, PluginCatalog, PluginRelease, RemoteArtifact, RemoteSignedRelease,
    current_platform_key,
};
const CATALOG_VERSION: u32 = 1;
/// 平台无关制品的目录键（解释器插件归档）：任一具体平台均可安装。
const ANY_PLATFORM_KEY: &str = "any";
const TRANSACTIONS_DIR: &str = ".transactions";
const FIRST_LAUNCH_MARKER: &str = ".first_launch_completed";

/// 默认插件 ID 列表。
///
/// 这些插件提供 Agent 的基础能力（系统提示词、文件操作、命令执行、网络获取、
/// 索引搜索、技能管理、MCP 工具桥接、审批征询、终端与浏览器），确保用户安装后即可
/// 获得基本体验。
pub const DEFAULT_PLUGIN_IDS: &[&str] = &[
    "prompt",
    "fs",
    "command",
    "fetch",
    "index",
    "skill",
    "mcp",
    "interaction",
    "terminal",
    "browser",
];

/// 与 `registry::DISABLED_MARKER` 保持一致的禁用标记文件名。
const DISABLED_MARKER_FILE: &str = ".disabled";

/// 场景分类常量：日常工作。
pub const CATEGORY_DAILY: &str = "daily";
/// 场景分类：编程开发。
pub const CATEGORY_CODING: &str = "coding";

/// 返回某插件的场景分类标签（多标签，可同时属于多个分类）。
///
/// 未在映射中声明的插件默认归入日常分类，保证插件市场不会出现无分类的孤立条目。
pub fn plugin_categories(id: &str) -> Vec<&'static str> {
    match id {
        // 基础能力：日常与编程通用。
        "prompt" | "fs" | "command" | "fetch" | "index" | "skill" | "mcp" | "memory"
        | "interaction" | "terminal" | "browser" => {
            vec![CATEGORY_DAILY, CATEGORY_CODING]
        }
        // 编程开发专属。
        "coding" => vec![CATEGORY_CODING],
        // 其余（定时任务、多媒体生成/分析）默认归入日常。
        _ => vec![CATEGORY_DAILY],
    }
}

/// 判断插件是否属于默认插件集合。
pub fn is_default_plugin(id: &str) -> bool {
    DEFAULT_PLUGIN_IDS.contains(&id)
}

/// 首次启动引导是否已完成（标记文件存在即视为完成）。
pub fn is_first_launch_completed(storage_root: &Path) -> bool {
    storage_root.join(FIRST_LAUNCH_MARKER).is_file()
}

/// 写入首次启动完成标记。内容为当前本地时间，便于排查。
pub fn mark_first_launch_completed(storage_root: &Path) -> Result<()> {
    let marker = storage_root.join(FIRST_LAUNCH_MARKER);
    std::fs::create_dir_all(storage_root)
        .with_context(|| format!("创建存储目录失败: {}", storage_root.display()))?;
    let timestamp = chrono::Local::now().naive_local().to_string();
    std::fs::write(&marker, timestamp)
        .with_context(|| format!("写入首次启动标记失败: {}", marker.display()))
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailablePlugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub supported: bool,
    pub installed_version: Option<String>,
    pub update_available: bool,
    /// 已安装插件的启用状态（未安装时为 false）。
    #[serde(default)]
    pub installed_enabled: bool,
    /// 是否为默认插件（基础能力，首次启动时会推荐安装）。
    #[serde(default)]
    pub is_default: bool,
    /// 场景分类标签（多标签，值为 `daily` / `coding` 的任意组合）。
    #[serde(default)]
    pub categories: Vec<&'static str>,
}

pub struct StagedPlugin {
    path: PathBuf,
}

impl StagedPlugin {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedPlugin {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), %error, "清理插件临时目录失败");
        }
    }
}

/// 将用户选择的本地完整插件目录复制到受管事务目录。
/// 安全解包插件归档（tar.zst）：逐条目校验相对路径（拒绝绝对路径与 `..`），
/// 只接受普通文件与目录条目（拒绝符号链接/硬链接/设备/FIFO 等特殊类型），
/// 并施加条目数、路径长度与解包总量上限，防止归档路径逃逸与资源耗尽。
pub fn extract_plugin_archive(archive: &Path, destination: &Path) -> Result<()> {
    /// 归档条目数上限（受管文件树远小于此量级）。
    const MAX_ENTRIES: usize = 4096;
    /// 单文件大小上限。
    const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
    /// 解包总大小上限。
    const MAX_TOTAL_SIZE: u64 = 256 * 1024 * 1024;
    /// 条目相对路径长度上限。
    const MAX_PATH_LEN: usize = 512;

    let file = std::fs::File::open(archive)
        .with_context(|| format!("打开插件归档失败: {}", archive.display()))?;
    let decoder = zstd::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);
    let mut total_size = 0u64;
    for (entry_count, entry) in tar
        .entries()
        .with_context(|| "读取插件归档条目失败")?
        .enumerate()
    {
        if entry_count >= MAX_ENTRIES {
            bail!("插件归档条目数超过上限 {MAX_ENTRIES}");
        }
        let mut entry = entry.with_context(|| "读取插件归档条目失败")?;
        let path = entry
            .path()
            .with_context(|| "归档条目路径无效")?
            .to_path_buf();
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
        {
            bail!("插件归档包含不安全路径: {}", path.display());
        }
        if path.as_os_str().len() > MAX_PATH_LEN {
            bail!("插件归档条目路径过长: {}", path.display());
        }
        match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Directory => {}
            other => bail!("插件归档包含不支持的条目类型 {other:?}: {}", path.display()),
        }
        let size = entry.header().size().unwrap_or(0);
        if size > MAX_FILE_SIZE {
            bail!("插件归档条目超过单文件大小上限: {}", path.display());
        }
        total_size += size;
        if total_size > MAX_TOTAL_SIZE {
            bail!("插件归档解包总量超过上限 {MAX_TOTAL_SIZE} 字节");
        }
        entry
            .unpack_in(destination)
            .with_context(|| format!("解包归档条目失败: {}", path.display()))?;
    }
    Ok(())
}

/// 从签名插件归档（tar.zst）暂存插件（三方导入通道）：安全解包到事务
/// 目录并校验清单可加载。签名验证（按发布者路由信任根）由安装链完成。
pub fn stage_plugin_archive(storage_root: &Path, archive: &Path) -> Result<StagedPlugin> {
    let staged = create_staged_plugin(storage_root)?;
    extract_plugin_archive(archive, staged.path())?;
    let manifest_path = staged.path().join(MANIFEST_FILE);
    ensure_regular_file(&manifest_path, "插件清单")?;
    let manifest = PluginManifest::load(&manifest_path)?;
    Version::parse(&manifest.version)
        .with_context(|| format!("插件 {} 版本不是有效语义版本", manifest.id))?;
    for directory in ["runtime", "logs", "data"] {
        std::fs::create_dir_all(staged.path().join(directory))?;
    }
    Ok(staged)
}

pub fn stage_local_plugin(storage_root: &Path, source: &Path) -> Result<StagedPlugin> {
    ensure_source_directory(source)?;
    let source_manifest = source.join(MANIFEST_FILE);
    ensure_regular_file(&source_manifest, "插件清单")?;
    let manifest = PluginManifest::load(&source_manifest)?;
    Version::parse(&manifest.version)
        .with_context(|| format!("本地插件 {} 版本不是有效语义版本", manifest.id))?;

    let staged = create_staged_plugin(storage_root)?;
    copy_regular_file(
        &source_manifest,
        &staged.path.join(MANIFEST_FILE),
        "插件清单",
    )?;
    if let Some(wasm_binary) = manifest.wasm_binary() {
        copy_local_artifact(source, wasm_binary, &staged.path, "WASM 制品")?;
    }

    if let Some(sidecar) = &manifest.sidecar {
        match sidecar.runtime {
            crate::manifest::SidecarRuntime::Native => {
                let binary = sidecar.binary.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("插件 {} native sidecar 缺少 binary 声明", manifest.id)
                })?;
                let binary = with_executable_suffix(binary)?;
                let destination =
                    copy_local_artifact(source, &binary, &staged.path, "sidecar 制品")?;
                set_executable(&destination)?;
            }
            crate::manifest::SidecarRuntime::Node | crate::manifest::SidecarRuntime::Python => {
                // 解释器入口及其同目录树（协议库等）整目录复制，保持只读语义。
                let entry = sidecar.entry.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("插件 {} 解释器 sidecar 缺少 entry 声明", manifest.id)
                })?;
                let entry_dir = entry.parent().ok_or_else(|| {
                    anyhow::anyhow!("插件 {} 解释器 sidecar entry 缺少父目录", manifest.id)
                })?;
                copy_resource_tree(source, entry_dir, &staged.path)?;
            }
        }
    }

    for contribution in manifest.ui_contributions() {
        copy_local_artifact(
            source,
            Path::new(&contribution.entry),
            &staged.path,
            "UI 入口",
        )?;
        // 自定义图标（资源路径形态）随安装复制，无需额外声明 resources。
        let icon = contribution.icon.trim();
        if icon.contains('/') || icon.contains('.') {
            copy_local_artifact(source, Path::new(icon), &staged.path, "UI 图标")?;
        }
    }

    for file in ["release.json", "release.json.sig"] {
        let source_file = source.join(file);
        if source_file.exists() {
            ensure_regular_file(&source_file, "插件签名制品")?;
            copy_regular_file(&source_file, &staged.path.join(file), "插件签名制品")?;
        }
    }

    // 内容清单（devkit 构建生成的路径 + sha256 树）：随插件进入安装目录，
    // 作为解释器 sidecar 本地信任的篡改检测锚。
    let content_manifest = source.join("content-manifest.json");
    if content_manifest.exists() {
        ensure_regular_file(&content_manifest, "插件内容清单")?;
        copy_regular_file(
            &content_manifest,
            &staged.path.join("content-manifest.json"),
            "插件内容清单",
        )?;
    }

    for directory in manifest.resources.iter().flatten() {
        copy_resource_tree(source, Path::new(directory), &staged.path)?;
    }

    for directory in ["runtime", "logs", "data"] {
        std::fs::create_dir_all(staged.path.join(directory))?;
    }
    Ok(staged)
}

/// 递归复制 manifest `resources` 声明的静态资产目录（拒绝符号链接）。
///
/// 跳过 `node_modules` 与 `.git`（防止开发者误打包本地依赖与仓库元数据）。
fn copy_resource_tree(
    source_root: &Path,
    relative_dir: &Path,
    destination_root: &Path,
) -> Result<()> {
    let mut source = source_root.to_path_buf();
    for component in relative_dir.components() {
        source.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&source)
            .with_context(|| format!("读取插件资源目录失败: {}", source.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("插件资源路径不能包含符号链接: {}", source.display());
        }
    }
    if !source.is_dir() {
        bail!("插件资源目录不存在: {}", source.display());
    }
    let mut stack = vec![source.clone()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("读取插件资源目录失败: {}", directory.display()))?
        {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.file_type().is_symlink() {
                bail!("插件资源路径不能包含符号链接: {}", entry.path().display());
            }
            let name = entry.file_name();
            if metadata.is_dir() {
                if name == "node_modules" || name == ".git" {
                    continue;
                }
                stack.push(entry.path());
            } else {
                let entry_path = entry.path();
                let relative = entry_path
                    .strip_prefix(source_root)
                    .context("插件资源相对路径推算失败")?;
                let destination = destination_root.join(relative);
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                copy_regular_file(&entry.path(), &destination, "插件资源文件")?;
            }
        }
    }
    Ok(())
}

/// 下载进度回调：`(downloaded_bytes, total_bytes)`。total 为 0 表示总大小未知。
pub type ProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

pub struct PluginRepository {
    http: reqwest::Client,
    /// 禁用代理的客户端：环境变量代理不可达（如 VPN 已关闭）时直连重试（#662）。
    direct_http: reqwest::Client,
}

/// 平台支持判断（市场列表与下载共用规则）：无 sidecar 制品的纯 UI/WASM
/// 插件全平台可用；`any` 平台无关归档（解释器插件）全平台可用；原生
/// sidecar 插件要求当前平台条目存在。
fn sidecar_supported(sidecars: &BTreeMap<String, RemoteArtifact>, platform: &str) -> bool {
    sidecars.is_empty()
        || sidecars.contains_key(platform)
        || sidecars.contains_key(ANY_PLATFORM_KEY)
}

impl PluginRepository {
    pub fn new() -> Result<Self> {
        let builder = || {
            reqwest::Client::builder()
                .user_agent("tiangong-plugin-downloader")
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(120))
        };
        let http = builder().build().context("构建插件下载客户端失败")?;
        let direct_http = builder()
            .no_proxy()
            .build()
            .context("构建插件下载客户端失败")?;
        Ok(Self { http, direct_http })
    }

    pub async fn list_available(&self, storage_root: &Path) -> Result<Vec<AvailablePlugin>> {
        let catalog = self.fetch_catalog().await?;
        let installed = installed_plugin_states(storage_root);
        let platform = current_platform_key();
        Ok(catalog
            .plugins
            .into_iter()
            .map(|plugin| {
                let installed_state = installed.get(&plugin.id);
                let installed_version = installed_state.map(|state| state.version.clone());
                let update_available = installed_version
                    .as_deref()
                    .is_some_and(|local| version_is_newer(local, &plugin.version));
                AvailablePlugin {
                    supported: sidecar_supported(&plugin.sidecars, &platform),
                    is_default: is_default_plugin(&plugin.id),
                    categories: plugin_categories(&plugin.id),
                    installed_enabled: installed_state.is_some_and(|state| state.enabled),
                    id: plugin.id,
                    name: plugin.name,
                    version: plugin.version,
                    description: plugin.description,
                    installed_version,
                    update_available,
                }
            })
            .collect())
    }

    pub async fn download(
        &self,
        storage_root: &Path,
        plugin_id: &str,
        progress: Option<ProgressFn>,
    ) -> Result<StagedPlugin> {
        let catalog = self.fetch_catalog().await?;
        let release = catalog
            .plugins
            .into_iter()
            .find(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| anyhow!("OSS 插件目录中不存在: {plugin_id}"))?;
        self.download_release(storage_root, &release, progress)
            .await
    }

    async fn fetch_catalog(&self) -> Result<PluginCatalog> {
        let endpoint = catalog_endpoint();
        validate_download_url(&endpoint, "OSS 插件目录")?;
        let response = self
            .get(&endpoint)
            .await
            .with_context(|| format!("请求 OSS 插件目录失败: {endpoint}"))?;
        if !response.status().is_success() {
            bail!("OSS 插件目录响应异常: {} {endpoint}", response.status());
        }
        let mut catalog: PluginCatalog = response
            .json()
            .await
            .with_context(|| format!("解析 OSS 插件目录失败: {endpoint}"))?;
        validate_catalog(&catalog)?;
        catalog
            .plugins
            .sort_by(|left, right| left.id.cmp(&right.id));
        Ok(catalog)
    }

    async fn download_release(
        &self,
        storage_root: &Path,
        release: &PluginRelease,
        progress: Option<ProgressFn>,
    ) -> Result<StagedPlugin> {
        let staged = create_staged_plugin(storage_root)?;

        // manifest 体积小，先单独下载（不纳入分段进度统计）。
        let manifest_path = staged.path.join(MANIFEST_FILE);
        validate_artifact_file_name(&release.manifest.url, MANIFEST_FILE)?;
        self.download_file(&release.manifest, &manifest_path, None)
            .await?;
        let manifest = PluginManifest::load(&manifest_path)?;
        if manifest.id != release.id {
            bail!(
                "插件目录 ID 与清单不一致: catalog={}, manifest={}",
                release.id,
                manifest.id
            );
        }
        if manifest.version != release.version {
            bail!(
                "插件目录版本与清单不一致: catalog={}, manifest={}",
                release.version,
                manifest.version
            );
        }

        let interpreter_plugin = manifest
            .sidecar
            .as_ref()
            .is_some_and(|sidecar| sidecar.runtime != crate::manifest::SidecarRuntime::Native);
        let ui_entries = if interpreter_plugin {
            // 解释器插件的全部制品（含 UI）在单一归档内。
            BTreeSet::<String>::new()
        } else {
            let paths = release.ui.keys().map(PathBuf::from).collect();
            manifest.validate_ui_artifacts(&paths)?;
            release.ui.keys().cloned().collect::<BTreeSet<_>>()
        };

        // 统计需要下载的制品文件，用于把进度按文件均分到 [0, 100]。
        let platform = current_platform_key();
        let has_wasm = manifest.wasm_binary().is_some();
        let has_sidecar = manifest.sidecar.is_some();
        let has_signed = manifest.sidecar.is_some() || !release.signed_releases.is_empty();
        let total_steps = ((has_wasm as usize)
            + (has_sidecar as usize)
            + (has_signed as usize)
            + ui_entries.len())
        .max(1);
        // 复用闭包：把单文件内的字节进度映射到全局百分比区间。
        let make_file_progress = |index: usize| -> Option<ProgressFn> {
            let progress = progress.clone()?;
            let start = (index * 100 / total_steps) as u64;
            let end = ((index + 1) * 100 / total_steps) as u64;
            Some(Arc::new(move |downloaded: u64, content_len: u64| {
                let ratio = if content_len > 0 {
                    downloaded as f64 / content_len as f64
                } else {
                    1.0
                };
                let percent = start as f64 + ratio * (end - start) as f64;
                progress(percent.round() as u64, 100);
            }))
        };

        let mut step_index = 0;
        if let Some(wasm_binary) = manifest.wasm_binary() {
            let wasm_artifact = release
                .wasm
                .as_ref()
                .ok_or_else(|| anyhow!("插件 {} 声明了 WASM，但目录缺少 WASM 制品", release.id))?;
            let wasm_path = staged.path.join(wasm_binary);
            let wasm_name = wasm_binary
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow!("WASM 制品文件名无效"))?;
            validate_artifact_file_name(&wasm_artifact.url, wasm_name)?;
            self.download_file(wasm_artifact, &wasm_path, make_file_progress(step_index))
                .await?;
            step_index += 1;
        } else if release.wasm.is_some() {
            bail!("插件 {} 未声明 WASM，但 OSS 目录包含 WASM 制品", release.id);
        }

        if has_sidecar {
            let sidecar_binary = manifest.sidecar.as_ref().expect("已确认存在 sidecar");
            if sidecar_binary.runtime != crate::manifest::SidecarRuntime::Native {
                // 解释器插件：下载平台无关的完整归档并解包到暂存目录——
                // 归档含全部受管文件（清单/UI/sidecar/模板/内容清单/签名），
                // 校验和锚定归档整体，签名验签由安装链（verify_signed_release，
                // 含内容清单全树校验）完成。
                let artifact = release.sidecars.get(ANY_PLATFORM_KEY).ok_or_else(|| {
                    anyhow!("插件 {} 没有解释器 sidecar 归档条目（any）", release.id)
                })?;
                let archive_path = staged
                    .path
                    .parent()
                    .ok_or_else(|| anyhow!("暂存目录缺少父目录"))?
                    .join(format!(".{}-download.tar.zst", release.id));
                self.download_file(artifact, &archive_path, make_file_progress(step_index))
                    .await?;
                // 先解包到独立目录并与目录清单下载的 plugin.json 逐字节比对，
                // 防止归档偷换 ID/版本后覆盖已校验的清单；一致后才合并进暂存。
                let extract_dir = staged
                    .path
                    .parent()
                    .ok_or_else(|| anyhow!("暂存目录缺少父目录"))?
                    .join(format!(".{}-extract", release.id));
                let _ = std::fs::remove_dir_all(&extract_dir);
                std::fs::create_dir_all(&extract_dir).context("创建插件归档解包目录失败")?;
                let extracted = extract_plugin_archive(&archive_path, &extract_dir);
                let _ = std::fs::remove_file(&archive_path);
                extracted?;
                let archive_manifest = std::fs::read(extract_dir.join(MANIFEST_FILE))
                    .with_context(|| format!("插件 {} 归档缺少 plugin.json", release.id))?;
                let catalog_manifest =
                    std::fs::read(&manifest_path).context("读取已下载的插件清单失败")?;
                if archive_manifest != catalog_manifest {
                    bail!(
                        "插件 {} 归档内 plugin.json 与目录声明不一致，拒绝安装",
                        release.id
                    );
                }
                // 暂存目录此刻只含 plugin.json，逐顶层条目原子迁入（同分区 rename）。
                for entry in
                    std::fs::read_dir(&extract_dir).with_context(|| "读取插件归档解包结果失败")?
                {
                    let entry = entry.context("读取插件归档解包结果失败")?;
                    let destination = staged.path.join(entry.file_name());
                    if destination.symlink_metadata().is_ok() {
                        if destination.is_dir() {
                            std::fs::remove_dir_all(&destination)
                                .context("清理暂存目录冲突条目失败")?;
                        } else {
                            std::fs::remove_file(&destination)
                                .context("清理暂存目录冲突条目失败")?;
                        }
                    }
                    std::fs::rename(entry.path(), &destination).with_context(|| {
                        format!("合并插件归档条目失败: {}", entry.path().display())
                    })?;
                }
                std::fs::remove_dir(&extract_dir).with_context(|| {
                    format!("清理插件归档解包目录失败: {}", extract_dir.display())
                })?;
                step_index += 1;
                // 签名清单独立于归档（签名非确定）：按目录条目下载到暂存，
                // 验签由安装链完成（含内容清单全树校验）。
                let signed = release
                    .signed_releases
                    .get(ANY_PLATFORM_KEY)
                    .ok_or_else(|| anyhow!("插件 {} 没有解释器签名条目（any）", release.id))?;
                self.download_unchecked(&signed.url, &staged.path.join("release.json"))
                    .await?;
                self.download_unchecked(
                    &signed.signature_url,
                    &staged.path.join("release.json.sig"),
                )
                .await?;
                if let Some(cb) = make_file_progress(step_index) {
                    cb(100, 100);
                }
                for directory in ["runtime", "logs", "data"] {
                    std::fs::create_dir_all(staged.path.join(directory))?;
                }
                return Ok(staged);
            }
            let artifact = release
                .sidecars
                .get(&platform)
                .ok_or_else(|| anyhow!("插件 {} 没有当前平台 {platform} 的 sidecar", release.id))?;
            let binary = sidecar_binary
                .binary
                .as_ref()
                .expect("validate 保证 native 有 binary");
            let sidecar_path = staged.path.join(with_executable_suffix(binary)?);
            let sidecar_name = sidecar_path
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow!("sidecar 文件名无效"))?;
            validate_artifact_file_name(&artifact.url, sidecar_name)?;
            self.download_file(artifact, &sidecar_path, make_file_progress(step_index))
                .await?;
            set_executable(&sidecar_path)?;
            step_index += 1;
        } else if !release.sidecars.is_empty() {
            bail!(
                "插件 {} 未声明 sidecar，但 OSS 目录包含 sidecar",
                release.id
            );
        }

        if has_signed {
            let signed = release
                .signed_releases
                .get(&platform)
                .ok_or_else(|| anyhow!("插件 {} 没有当前平台 {platform} 的签名清单", release.id))?;
            self.download_unchecked(&signed.url, &staged.path.join("release.json"))
                .await?;
            self.download_unchecked(&signed.signature_url, &staged.path.join("release.json.sig"))
                .await?;
            if let Some(cb) = make_file_progress(step_index) {
                cb(100, 100);
            }
            step_index += 1;
        }
        for entry in ui_entries {
            let artifact = release
                .ui
                .get(&entry)
                .ok_or_else(|| anyhow!("插件 {} 缺少 UI 制品 {entry}", release.id))?;
            let file_name = Path::new(&entry)
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow!("UI 制品文件名无效: {entry}"))?;
            validate_artifact_file_name(&artifact.url, file_name)?;
            self.download_file(
                artifact,
                &staged.path.join(&entry),
                make_file_progress(step_index),
            )
            .await?;
            step_index += 1;
        }
        for directory in ["runtime", "logs", "data"] {
            std::fs::create_dir_all(staged.path.join(directory))?;
        }
        Ok(staged)
    }

    /// GET 请求；连接阶段失败（如环境变量代理不可达）时以直连客户端重试一次（#662）。
    ///
    /// 连接失败时请求尚未送达服务端，重试不会造成重复下载；HTTP 状态等其他错误不重试。
    async fn get(&self, url: &str) -> reqwest::Result<reqwest::Response> {
        match self.http.get(url).send().await {
            Err(error) if error.is_connect() => self.direct_http.get(url).send().await,
            result => result,
        }
    }

    async fn download_unchecked(&self, url: &str, destination: &Path) -> Result<()> {
        validate_download_url(url, "插件签名制品")?;
        let response = self
            .get(url)
            .await
            .with_context(|| format!("下载插件签名制品失败: {url}"))?;
        if !response.status().is_success() {
            bail!("插件签名制品下载响应异常: {}", response.status());
        }
        std::fs::write(destination, response.bytes().await?)
            .with_context(|| format!("写入插件签名制品失败: {}", destination.display()))
    }
    async fn download_file(
        &self,
        artifact: &RemoteArtifact,
        destination: &Path,
        progress: Option<ProgressFn>,
    ) -> Result<()> {
        let expected = parse_checksum(&artifact.checksum)?;
        validate_download_url(&artifact.url, "插件制品")?;
        if let Some(parent) = destination.parent() {
            ensure_not_symlink(parent)?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建插件制品目录失败: {}", parent.display()))?;
        }
        ensure_not_symlink(destination)?;

        let response = self
            .get(&artifact.url)
            .await
            .with_context(|| format!("下载插件制品失败: {}", artifact.url))?;
        if !response.status().is_success() {
            bail!(
                "插件制品下载响应异常: {} {}",
                response.status(),
                artifact.url
            );
        }
        let content_len = response.content_length();

        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .with_context(|| format!("创建插件制品失败: {}", destination.display()))?;
        let mut downloaded: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("读取插件制品失败: {}", artifact.url))?;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .with_context(|| format!("写入插件制品失败: {}", destination.display()))?;
            if let Some(cb) = &progress {
                downloaded += chunk.len() as u64;
                cb(downloaded, content_len.unwrap_or(0));
            }
        }
        file.sync_all()
            .with_context(|| format!("同步插件制品失败: {}", destination.display()))?;
        let actual = hasher.finalize();
        if actual.as_slice() != expected {
            let _ = std::fs::remove_file(destination);
            bail!(
                "插件制品 SHA-256 校验失败: expected={}, actual={}",
                hex::encode(expected),
                hex::encode(actual)
            );
        }
        Ok(())
    }
}

/// 从 OSS 仓库下载并安装插件（宿主命令层的完整下沉实现）。
///
/// 流程：目录检索 → 下载暂存（progress 可选回调）→ 阻塞线程安装事务。
/// 安装成功由 [`crate::registry`] 的迁移函数广播插件变化事件（含指纹），
/// 宿主订阅者自行响应；本函数不承担任何通知扇出。计时日志与原
/// `download_and_install_plugin` 对齐，便于排障对照。
pub async fn install_plugin_from_repository(
    storage_root: &Path,
    plugin_id: &str,
    progress: Option<ProgressFn>,
) -> Result<crate::registry::PluginStatus> {
    let total_started = std::time::Instant::now();
    let repository = PluginRepository::new()?;
    let download_started = std::time::Instant::now();
    let staged = repository
        .download(storage_root, plugin_id, progress)
        .await
        .with_context(|| format!("下载插件 {plugin_id} 失败"))?;
    let download_ms = download_started.elapsed().as_millis() as u64;

    let install_started = std::time::Instant::now();
    let (staged_root, staged_path) = (storage_root.to_path_buf(), staged.path.clone());
    let status = tokio::task::spawn_blocking(move || {
        crate::registry::install_staged_plugin(&staged_root, &staged_path)
    })
    .await
    .with_context(|| format!("安装插件 {plugin_id} 任务失败"))??;
    tracing::info!(
        plugin_id,
        download_ms,
        install_ms = install_started.elapsed().as_millis() as u64,
        total_ms = total_started.elapsed().as_millis() as u64,
        "插件下载安装完成"
    );
    Ok(status)
}

fn create_staged_plugin(storage_root: &Path) -> Result<StagedPlugin> {
    let root = storage_root.join("plugins").join(TRANSACTIONS_DIR);
    ensure_not_symlink(&root)?;
    std::fs::create_dir_all(&root)
        .with_context(|| format!("创建插件事务目录失败: {}", root.display()))?;
    let path = root.join(scru128::new().to_string());
    std::fs::create_dir(&path)
        .with_context(|| format!("创建插件临时目录失败: {}", path.display()))?;
    Ok(StagedPlugin { path })
}

fn copy_local_artifact(
    source_root: &Path,
    relative_path: &Path,
    destination_root: &Path,
    label: &str,
) -> Result<PathBuf> {
    let source = resolve_local_artifact(source_root, relative_path, label)?;
    let destination = destination_root.join(relative_path);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建{label}目录失败: {}", parent.display()))?;
    }
    copy_regular_file(&source, &destination, label)?;
    Ok(destination)
}

fn resolve_local_artifact(
    source_root: &Path,
    relative_path: &Path,
    label: &str,
) -> Result<PathBuf> {
    let mut source = source_root.to_path_buf();
    for component in relative_path.components() {
        source.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&source)
            .with_context(|| format!("读取{label}失败: {}", source.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("{label}路径不能包含符号链接: {}", source.display());
        }
    }
    ensure_regular_file(&source, label)?;
    Ok(source)
}

fn copy_regular_file(source: &Path, destination: &Path, label: &str) -> Result<()> {
    std::fs::copy(source, destination).with_context(|| {
        format!(
            "复制{label}失败: {} -> {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn ensure_source_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("读取本地插件目录失败: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("本地插件路径必须是实际目录: {}", path.display());
    }
    Ok(())
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("读取{label}失败: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label}必须是实际文件: {}", path.display());
    }
    Ok(())
}

fn with_executable_suffix(path: &Path) -> Result<PathBuf> {
    let mut path = path.to_path_buf();
    if !std::env::consts::EXE_SUFFIX.is_empty() {
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("sidecar 文件名无效"))?;
        if !file_name.ends_with(std::env::consts::EXE_SUFFIX) {
            path.set_file_name(format!("{file_name}{}", std::env::consts::EXE_SUFFIX));
        }
    }
    Ok(path)
}

fn validate_catalog(catalog: &PluginCatalog) -> Result<()> {
    if catalog.version != CATALOG_VERSION {
        bail!(
            "不支持的 OSS 插件目录版本: expected={CATALOG_VERSION}, actual={}",
            catalog.version
        );
    }
    let mut ids = HashSet::new();
    for plugin in &catalog.plugins {
        if plugin.id.is_empty() || !ids.insert(plugin.id.clone()) {
            bail!("OSS 插件目录包含空 ID 或重复 ID: {}", plugin.id);
        }
        Version::parse(&plugin.version)
            .with_context(|| format!("插件 {} 版本不是有效语义版本", plugin.id))?;
        validate_download_url(&plugin.manifest.url, "插件清单")?;
        parse_checksum(&plugin.manifest.checksum)?;
        if let Some(wasm) = &plugin.wasm {
            validate_download_url(&wasm.url, "WASM 制品")?;
            parse_checksum(&wasm.checksum)?;
        }
        for (entry, artifact) in &plugin.ui {
            validate_relative_artifact_path(Path::new(entry), "UI 制品")?;
            validate_download_url(&artifact.url, "UI 制品")?;
            parse_checksum(&artifact.checksum)?;
        }
        for (platform, artifact) in &plugin.sidecars {
            validate_download_url(&artifact.url, "sidecar 制品")?;
            parse_checksum(&artifact.checksum)?;
            let signed = plugin
                .signed_releases
                .get(platform)
                .ok_or_else(|| anyhow!("插件 {} 的平台 {platform} 缺少签名清单", plugin.id))?;
            validate_download_url(&signed.url, "插件签名清单")?;
            validate_download_url(&signed.signature_url, "插件签名文件")?;
        }
        // 平台键约定：`any`（解释器归档，平台无关）与具体平台键（原生二进制）
        // 不得混用——混用意味着发布形态不明确。
        if plugin.sidecars.contains_key(ANY_PLATFORM_KEY) && plugin.sidecars.len() > 1 {
            bail!(
                "插件 {} 的 sidecars 混用平台无关条目（any）与具体平台条目",
                plugin.id
            );
        }
        // 纯 WASM 插件（如 prompt）没有 sidecar 但同样需要官方签名建立信任，
        // 因此签名清单不强制要求对应 sidecar，这里只校验签名 URL 合法性。
        for signed in plugin.signed_releases.values() {
            validate_download_url(&signed.url, "插件签名清单")?;
            validate_download_url(&signed.signature_url, "插件签名文件")?;
        }
    }
    Ok(())
}

fn validate_relative_artifact_path(path: &Path, label: &str) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("{label}路径无效: {}", path.display());
    }
    Ok(())
}

/// 已安装插件的本地状态快照。
#[derive(Debug, Clone)]
struct InstalledPluginState {
    version: String,
    enabled: bool,
}

fn installed_plugin_states(storage_root: &Path) -> HashMap<String, InstalledPluginState> {
    let plugins_dir = storage_root.join("plugins");
    let Ok(entries) = std::fs::read_dir(plugins_dir) else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let manifest = PluginManifest::load(&entry.path().join(MANIFEST_FILE)).ok()?;
            // 只认目录名与清单 ID 一致的正式目录：本地部署中断残留的
            // `.{id}-staging-{pid}` 同样带清单，混入会以同 ID 覆盖正式目录
            // 的版本，让升级判定与安装校验读到的"当前版本"不一致。
            if *entry.file_name() != *manifest.id {
                return None;
            }
            let enabled = !entry.path().join(DISABLED_MARKER_FILE).is_file();
            Some((
                manifest.id,
                InstalledPluginState {
                    version: manifest.version,
                    enabled,
                },
            ))
        })
        .collect()
}

fn version_is_newer(installed: &str, available: &str) -> bool {
    match (Version::parse(installed), Version::parse(available)) {
        (Ok(installed), Ok(available)) => available > installed,
        _ => available != installed,
    }
}

fn parse_checksum(checksum: &str) -> Result<Vec<u8>> {
    let value = checksum
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow!("制品 checksum 必须使用 sha256:<hex> 格式"))?;
    let bytes = hex::decode(value).context("制品 checksum 不是有效十六进制")?;
    if bytes.len() != 32 {
        bail!("制品 SHA-256 长度无效");
    }
    Ok(bytes)
}

fn validate_artifact_file_name(url: &str, expected: &str) -> Result<()> {
    let url = reqwest::Url::parse(url).with_context(|| format!("插件制品 URL 无效: {url}"))?;
    let actual = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .ok_or_else(|| anyhow!("插件制品 URL 缺少文件名: {url}"))?;
    if actual != expected {
        bail!("插件制品名称不一致: expected={expected}, actual={actual}");
    }
    Ok(())
}

fn validate_download_url(url: &str, label: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("{label} URL 无效: {url}"))?;
    let local_http = parsed.scheme() == "http"
        && parsed
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
    if parsed.scheme() != "https" && !local_http {
        bail!("{label} 必须使用 HTTPS: {url}");
    }
    Ok(())
}

fn catalog_endpoint() -> String {
    std::env::var("TIANGONG_PLUGIN_CATALOG_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| PLUGIN_CATALOG_ENDPOINT.to_string())
}

fn ensure_not_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("插件路径不能是符号链接: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("检查插件路径失败: {}", path.display())),
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 代理不可达时直连重试() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        // 端口 9（discard）通常无人监听，模拟 VPN 关闭后失效的代理。
        let repository = PluginRepository {
            http: reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
                .build()
                .unwrap(),
            direct_http: reqwest::Client::builder().no_proxy().build().unwrap(),
        };
        let response = repository.get(&format!("http://{address}/")).await.unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        server.join().unwrap();
    }

    /// resources 声明目录随导入递归进入暂存区（plugin-dev 模板分发链路）。
    #[test]
    fn stage_local_plugin_复制resources目录() {
        let root = tempfile::tempdir().expect("临时目录");
        let source = root.path().join("source");
        std::fs::create_dir_all(source.join("app")).expect("入口目录");
        std::fs::create_dir_all(source.join("templates/ui-app/app")).expect("模板目录");
        std::fs::write(
            source.join("plugin.json"),
            r#"{"schema_version":2,"id":"res-demo","version":"0.1.0","permissions":[],"resources":["templates/"],"ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"app/index.html"}]}}"#,
        )
        .expect("清单");
        std::fs::write(source.join("app/index.html"), "<html></html>").expect("入口");
        std::fs::write(source.join("templates/ui-app/plugin.json"), "{}").expect("模板清单");
        std::fs::write(source.join("templates/ui-app/app/index.html"), "x").expect("模板页");
        // node_modules 应被跳过
        std::fs::create_dir_all(source.join("templates/ui-app/node_modules/pkg"))
            .expect("依赖目录");
        std::fs::write(
            source.join("templates/ui-app/node_modules/pkg/index.js"),
            "x",
        )
        .expect("依赖文件");

        let staged = stage_local_plugin(root.path(), &source).expect("暂存");
        assert!(staged.path().join("plugin.json").is_file());
        assert!(staged.path().join("app/index.html").is_file());
        assert!(staged.path().join("templates/ui-app/plugin.json").is_file());
        assert!(
            staged
                .path()
                .join("templates/ui-app/app/index.html")
                .is_file()
        );
        assert!(!staged.path().join("templates/ui-app/node_modules").exists());
    }

    /// 升级判定只认目录名与清单 ID 一致的正式目录：本地部署中断残留的
    /// `.{id}-staging-{pid}` 带同 ID 清单，不得覆盖正式目录的版本。
    #[test]
    fn installed_plugin_states_忽略staging残留目录() {
        let root = tempfile::tempdir().expect("临时目录");
        let plugins = root.path().join("plugins");
        std::fs::create_dir_all(plugins.join("demo")).expect("正式目录");
        std::fs::write(
            plugins.join("demo").join(MANIFEST_FILE),
            r#"{"schema_version":2,"id":"demo","version":"0.3.2","permissions":[],"capabilities":{"prompt":true}}"#,
        )
        .expect("正式清单");
        std::fs::create_dir_all(plugins.join(".demo-staging-123")).expect("残留目录");
        std::fs::write(
            plugins.join(".demo-staging-123").join(MANIFEST_FILE),
            r#"{"schema_version":2,"id":"demo","version":"0.3.1","permissions":[],"capabilities":{"prompt":true}}"#,
        )
        .expect("残留清单");

        let states = installed_plugin_states(root.path());
        assert_eq!(
            states.get("demo").expect("正式插件应被识别").version,
            "0.3.2"
        );
    }
}

#[cfg(test)]
mod extract_and_catalog_tests {
    use super::*;

    fn write_installed_manifest(directory: &Path, id: &str, version: &str) {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(
            directory.join(MANIFEST_FILE),
            format!(
                r#"{{"schema_version":2,"id":"{id}","version":"{version}","mention":{{"hint":"test"}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn 已安装状态忽略暂存目录与目录名不匹配项() {
        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("plugins");
        write_installed_manifest(&plugins.join("demo"), "demo", "1.2.0");
        write_installed_manifest(&plugins.join(".demo-staging-123"), "demo", "1.1.0");
        write_installed_manifest(&plugins.join("wrong-directory"), "other", "9.9.9");

        let installed = installed_plugin_states(root.path());
        assert_eq!(installed.len(), 1);
        assert_eq!(
            installed.get("demo").map(|state| state.version.as_str()),
            Some("1.2.0")
        );
    }

    #[test]
    fn 版本一致或线上更旧时不提示更新() {
        assert!(!version_is_newer("1.2.3", "1.2.3"));
        assert!(!version_is_newer("1.2.3", "1.2.2"));
        assert!(version_is_newer("1.2.3", "1.2.4"));
    }

    fn remote_artifact(name: &str) -> RemoteArtifact {
        RemoteArtifact {
            url: format!("https://example.com/plugins/demo/0.2.0/{name}"),
            checksum: format!("sha256:{}", "a".repeat(64)),
        }
    }

    fn signed_release() -> RemoteSignedRelease {
        RemoteSignedRelease {
            url: "https://example.com/plugins/demo/0.2.0/release.json".to_string(),
            signature_url: "https://example.com/plugins/demo/0.2.0/release.json.sig".to_string(),
        }
    }

    #[test]
    fn 平台支持判断_any归档全平台可用() {
        let mut sidecars = BTreeMap::new();
        sidecars.insert(
            ANY_PLATFORM_KEY.to_string(),
            remote_artifact("demo.tar.zst"),
        );
        assert!(sidecar_supported(&sidecars, "darwin-aarch64"));
        assert!(sidecar_supported(&sidecars, "linux-x86_64"));
        assert!(sidecar_supported(&sidecars, "windows-x86_64"));
        assert!(sidecar_supported(&sidecars, "plan9-arm"));
    }

    #[test]
    fn 平台支持判断_原生制品按平台匹配() {
        let mut sidecars = BTreeMap::new();
        sidecars.insert(
            "darwin-aarch64".to_string(),
            remote_artifact("demo-aarch64"),
        );
        assert!(sidecar_supported(&sidecars, "darwin-aarch64"));
        assert!(!sidecar_supported(&sidecars, "linux-x86_64"));
        // 无 sidecar 制品（纯 UI/WASM 插件）全平台可用。
        assert!(sidecar_supported(&BTreeMap::new(), "darwin-aarch64"));
    }

    fn catalog_with_sidecars(
        sidecars: BTreeMap<String, RemoteArtifact>,
        signed: BTreeMap<String, RemoteSignedRelease>,
    ) -> PluginCatalog {
        PluginCatalog {
            version: CATALOG_VERSION,
            plugins: vec![PluginRelease {
                id: "demo".to_string(),
                name: "Demo".to_string(),
                version: "0.2.0".to_string(),
                description: String::new(),
                manifest: remote_artifact("plugin.json"),
                wasm: None,
                signed_releases: signed,
                sidecars,
                ui: BTreeMap::new(),
            }],
        }
    }

    #[test]
    fn 目录校验_any与平台条目混用拒绝() {
        let mut sidecars = BTreeMap::new();
        sidecars.insert(
            ANY_PLATFORM_KEY.to_string(),
            remote_artifact("demo.tar.zst"),
        );
        sidecars.insert(
            "darwin-aarch64".to_string(),
            remote_artifact("demo-aarch64"),
        );
        let mut signed = BTreeMap::new();
        signed.insert(ANY_PLATFORM_KEY.to_string(), signed_release());
        signed.insert("darwin-aarch64".to_string(), signed_release());
        let catalog = catalog_with_sidecars(sidecars, signed);
        let error = validate_catalog(&catalog).expect_err("混用 any 与平台条目应拒绝");
        assert!(error.to_string().contains("混用"), "{error:#}");
    }

    #[test]
    fn 目录校验_any缺少签名条目拒绝() {
        let mut sidecars = BTreeMap::new();
        sidecars.insert(
            ANY_PLATFORM_KEY.to_string(),
            remote_artifact("demo.tar.zst"),
        );
        let catalog = catalog_with_sidecars(sidecars, BTreeMap::new());
        let error = validate_catalog(&catalog).expect_err("any 缺签名条目应拒绝");
        assert!(error.to_string().contains("缺少签名清单"), "{error:#}");
    }

    #[test]
    fn 目录校验_单一any条目通过() {
        let mut sidecars = BTreeMap::new();
        sidecars.insert(
            ANY_PLATFORM_KEY.to_string(),
            remote_artifact("demo.tar.zst"),
        );
        let mut signed = BTreeMap::new();
        signed.insert(ANY_PLATFORM_KEY.to_string(), signed_release());
        let catalog = catalog_with_sidecars(sidecars, signed);
        validate_catalog(&catalog).expect("单一 any 条目应通过");
    }

    /// 构造含自定义条目的 tar.zst 归档。绕过 tar::Builder 的写入端路径
    /// 校验（测试需要投递 `..` 等恶意路径验证读取端防护）。
    fn build_archive(destination: &Path, entries: &[(&str, tar::EntryType, &[u8])]) {
        let file = std::fs::File::create(destination).unwrap();
        let encoder = zstd::Encoder::new(file, 3).unwrap();
        let mut builder = tar::Builder::new(encoder);
        for (name, entry_type, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(if *entry_type == tar::EntryType::Directory {
                0o755
            } else {
                0o644
            });
            header.set_entry_type(*entry_type);
            if name.len() < 100 {
                let old = header.as_old_mut();
                let name_bytes = name.as_bytes();
                old.name[..name_bytes.len()].copy_from_slice(name_bytes);
                old.name[name_bytes.len()] = 0;
                header.set_cksum();
                builder
                    .append(&header, std::io::Cursor::new(data.to_vec()))
                    .unwrap();
            } else {
                // 超长路径经 GNU longname 扩展投递（路径本身合法，仅超长）。
                builder
                    .append_data(&mut header, name, std::io::Cursor::new(data.to_vec()))
                    .unwrap();
            }
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
    }

    #[test]
    fn 解包_普通文件与目录通过() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("ok.tar.zst");
        build_archive(
            &archive,
            &[
                ("plugin.json", tar::EntryType::Regular, b"{}"),
                ("sidecar/", tar::EntryType::Directory, b""),
                ("sidecar/main.mjs", tar::EntryType::Regular, b"// entry"),
            ],
        );
        let destination = root.path().join("out");
        std::fs::create_dir_all(&destination).unwrap();
        extract_plugin_archive(&archive, &destination).expect("普通文件与目录应解包通过");
        assert_eq!(
            std::fs::read(destination.join("sidecar/main.mjs")).unwrap(),
            b"// entry"
        );
    }

    #[test]
    fn 解包_符号链接条目拒绝() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("link.tar.zst");
        build_archive(&archive, &[("link", tar::EntryType::Symlink, b"")]);
        let destination = root.path().join("out");
        std::fs::create_dir_all(&destination).unwrap();
        let error = extract_plugin_archive(&archive, &destination).expect_err("符号链接条目应拒绝");
        assert!(error.to_string().contains("不支持的条目类型"), "{error:#}");
    }

    #[test]
    fn 解包_fifo条目拒绝() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("fifo.tar.zst");
        build_archive(&archive, &[("pipe", tar::EntryType::Fifo, b"")]);
        let destination = root.path().join("out");
        std::fs::create_dir_all(&destination).unwrap();
        let error = extract_plugin_archive(&archive, &destination).expect_err("FIFO 条目应拒绝");
        assert!(error.to_string().contains("不支持的条目类型"), "{error:#}");
    }

    #[test]
    fn 解包_父目录逃逸拒绝() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("escape.tar.zst");
        build_archive(
            &archive,
            &[("../escape.txt", tar::EntryType::Regular, b"x")],
        );
        let destination = root.path().join("out");
        std::fs::create_dir_all(&destination).unwrap();
        let error = extract_plugin_archive(&archive, &destination).expect_err("逃逸路径应拒绝");
        assert!(error.to_string().contains("不安全路径"), "{error:#}");
    }

    #[test]
    fn 解包_超长路径拒绝() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("long.tar.zst");
        let long_name = format!("{}.txt", "n".repeat(600));
        build_archive(&archive, &[(&long_name, tar::EntryType::Regular, b"x")]);
        let destination = root.path().join("out");
        std::fs::create_dir_all(&destination).unwrap();
        let error = extract_plugin_archive(&archive, &destination).expect_err("超长路径应拒绝");
        assert!(error.to_string().contains("路径过长"), "{error:#}");
    }
}
