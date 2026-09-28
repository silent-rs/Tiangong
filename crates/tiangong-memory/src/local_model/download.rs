//! 内置模型下载：分文件断点续传 + 大小/sha256 校验 + 原子落盘。
//!
//! 目录布局（`<storage>/memory/models/`）：
//!
//! ```text
//! models/
//!   bge-base-zh-v1.5/        # 完整模型（全部文件校验通过后整体改名而来）
//!     .complete              # 校验完成标记（内容为 revision）
//!     model_quantized.onnx
//!     tokenizer.json ...
//!   .partial-bge-m3/         # 下载中（*.part 为未完成文件，可续传）
//!   .lock                    # 跨进程互斥
//! ```
//!
//! 下载源按顺序尝试（同一文件断线后换源续传）：
//!
//! 1. `TIANGONG_MEMORY_MODEL_MIRROR`（可多个，逗号分隔）；
//! 2. 天工官方 OSS（`<oss>/memory-models/`）；
//! 3. ModelScope（国内 CDN，同名仓库）；
//! 4. hf-mirror.com；
//! 5. huggingface.co。
//!
//! 基地址形式的源按 HuggingFace 路径拼接 `<base>/<repo>/resolve/<revision>/<path>`；
//! 含 `{repo}` 占位符的源按模板展开（支持 `{repo}` `{revision}` `{path}`）。
//! 无论来源，内容都按清单中的大小与 sha256 校验，因此镜像无需被信任。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};

use super::catalog::{LocalModelSpec, ModelFile};

/// 自定义镜像环境变量（逗号分隔的基地址）。
pub const MODEL_MIRROR_ENV: &str = "TIANGONG_MEMORY_MODEL_MIRROR";
/// 默认下载源（按顺序尝试）。
const DEFAULT_SOURCES: [&str; 4] = [
    "https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/memory-models",
    // ModelScope 同名仓库只有 master 分支；内容由 sha256 锁定，分支漂移会被校验拦下。
    "https://www.modelscope.cn/models/{repo}/resolve/master/{path}",
    "https://hf-mirror.com",
    "https://huggingface.co",
];
const COMPLETE_MARKER: &str = ".complete";
/// 单个文件的最大重试轮次（每轮依次尝试所有下载源，续传不从头开始）。
const MAX_ROUNDS: usize = 6;

/// 下载取消令牌：配置变更后让在途下载尽快退出，不再把带宽和磁盘浪费在
/// 已经不需要的模型上。
#[derive(Debug, Clone, Default)]
pub(crate) struct CancelToken(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    pub(crate) fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// 下载未完成的原因，供调用方决定是否计入失败重试。
#[derive(Debug)]
pub(crate) enum DownloadError {
    /// 另一个进程正在下载：等它完成即可，不算本次失败。
    Busy,
    /// 已被取消（配置变更）。
    Cancelled,
    /// 真正的下载故障。
    Failed(anyhow::Error),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "另一个 Memory 进程正在下载内置模型，稍后重试"),
            Self::Cancelled => write!(f, "下载已取消"),
            Self::Failed(error) => write!(f, "{error:#}"),
        }
    }
}

/// 模型根目录。
pub(crate) fn models_dir() -> PathBuf {
    crate::paths::memory_data_dir().join("models")
}

/// 模型完整目录。
pub(crate) fn model_dir(root: &Path, spec: &LocalModelSpec) -> PathBuf {
    root.join(spec.id)
}

/// 模型是否已完整下载（存在校验完成标记且 revision 一致、文件大小齐全）。
///
/// 只做轻量检查（sha256 在下载时已校验），用于启动时快速判断。
pub(crate) fn is_installed(root: &Path, spec: &LocalModelSpec) -> bool {
    let dir = model_dir(root, spec);
    let marker = std::fs::read_to_string(dir.join(COMPLETE_MARKER)).unwrap_or_default();
    marker.trim() == spec.revision
        && spec.files().iter().all(|file| {
            std::fs::metadata(dir.join(file.local_name()))
                .map(|meta| meta.len() == file.size)
                .unwrap_or(false)
        })
}

/// 下载中已落盘的字节数（完成的文件 + `.part` 临时文件），跨进程可见，
/// 用于页面展示下载进度。
pub(crate) fn partial_bytes(root: &Path, spec: &LocalModelSpec) -> u64 {
    let partial = root.join(format!(".partial-{}", spec.id));
    spec.files()
        .iter()
        .map(|file| {
            let done = std::fs::metadata(partial.join(file.local_name()))
                .map(|meta| meta.len())
                .unwrap_or(0);
            let part = std::fs::metadata(partial.join(format!("{}.part", file.local_name())))
                .map(|meta| meta.len())
                .unwrap_or(0);
            done.max(part).min(file.size)
        })
        .sum()
}

/// 测试钩子：让测试持有真实下载锁，验证状态判定。
#[cfg(test)]
pub(crate) mod test_hooks {
    use super::{DownloadLock, Path, Result};

    pub(crate) fn acquire_lock(root: &Path) -> Result<Option<DownloadLock>> {
        DownloadLock::acquire(root)
    }
}

/// 同一存储根是否有进程正在下载（用下载锁探测）。
///
/// 只凭残留 `.part` 文件无法区分「正在下载」和「上次中断的残留」；能拿到
/// 独占锁说明当前没有下载者在跑。注意同进程内重复加锁可能成功，因此本
/// 判断只用于「本进程没有下载任务」的场景。
pub(crate) fn has_active_downloader(root: &Path) -> bool {
    match DownloadLock::acquire(root) {
        // 拿到锁：无人下载（立即释放）。
        Ok(Some(_)) => false,
        Ok(None) => true,
        // 锁文件不可用时无法判断，按无人下载处理（页面提示可继续下载）。
        Err(_) => false,
    }
}

fn sources() -> Vec<String> {
    let mut sources = std::env::var(MODEL_MIRROR_ENV)
        .unwrap_or_default()
        .split(',')
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    sources.extend(DEFAULT_SOURCES.iter().map(|value| value.to_string()));
    sources
}

fn file_url(source: &str, spec: &LocalModelSpec, file: &ModelFile) -> String {
    if source.contains("{repo}") {
        return source
            .replace("{repo}", spec.repo)
            .replace("{revision}", spec.revision)
            .replace("{path}", file.path);
    }
    format!(
        "{source}/{}/resolve/{}/{}",
        spec.repo, spec.revision, file.path
    )
}

/// 跨进程文件锁：同一存储根同时只允许一个进程下载。
pub(crate) struct DownloadLock {
    file: std::fs::File,
}

impl DownloadLock {
    fn acquire(root: &Path) -> Result<Option<Self>> {
        use fs2::FileExt;
        std::fs::create_dir_all(root)
            .with_context(|| format!("创建模型目录失败: {}", root.display()))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(".lock"))
            .with_context(|| "打开模型下载锁失败")?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
impl DownloadLock {
    /// 测试辅助：模拟其他持锁者（返回的守卫在 drop 时释放）。
    pub(crate) fn acquire_for_other_process(root: &Path) -> Option<Self> {
        Self::acquire(root).ok().flatten()
    }
}

impl Drop for DownloadLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// 确保模型已下载；已安装时立即返回。
///
/// 另一进程正在下载时返回 [`DownloadError::Busy`]（不算失败，稍后重试）。
pub(crate) async fn ensure_model(
    root: &Path,
    spec: &LocalModelSpec,
    cancel: &CancelToken,
) -> std::result::Result<PathBuf, DownloadError> {
    ensure_model_from(root, spec, &sources(), Duration::from_secs(1), cancel).await
}

/// 指定下载源与重试基准间隔（测试注入本地源、缩短等待）。
async fn ensure_model_from(
    root: &Path,
    spec: &LocalModelSpec,
    sources: &[String],
    retry_base: Duration,
    cancel: &CancelToken,
) -> std::result::Result<PathBuf, DownloadError> {
    let dir = model_dir(root, spec);
    if is_installed(root, spec) {
        return Ok(dir);
    }
    if cancel.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }
    let lock = DownloadLock::acquire(root).map_err(DownloadError::Failed)?;
    let Some(_lock) = lock else {
        return Err(DownloadError::Busy);
    };
    // 拿到锁后再检查一次：可能刚被其他进程下完。
    if is_installed(root, spec) {
        return Ok(dir);
    }
    ensure_model_locked(root, spec, sources, retry_base, cancel, dir).await
}

/// 持锁后的实际下载流程（错误统一归类为 Failed / Cancelled）。
async fn ensure_model_locked(
    root: &Path,
    spec: &LocalModelSpec,
    sources: &[String],
    retry_base: Duration,
    cancel: &CancelToken,
    dir: PathBuf,
) -> std::result::Result<PathBuf, DownloadError> {
    match download_all(root, spec, sources, retry_base, cancel, &dir).await {
        Ok(()) => Ok(dir),
        Err(error) if cancel.is_cancelled() => {
            tracing::debug!(model = spec.id, "Memory 内置模型下载已取消: {error:#}");
            Err(DownloadError::Cancelled)
        }
        Err(error) => Err(DownloadError::Failed(error)),
    }
}

async fn download_all(
    root: &Path,
    spec: &LocalModelSpec,
    sources: &[String],
    retry_base: Duration,
    cancel: &CancelToken,
    dir: &Path,
) -> Result<()> {
    let partial = root.join(format!(".partial-{}", spec.id));
    std::fs::create_dir_all(&partial)
        .with_context(|| format!("创建下载目录失败: {}", partial.display()))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .build()
        .with_context(|| "创建模型下载客户端失败")?;
    for file in spec.files() {
        if cancel.is_cancelled() {
            bail!("下载已取消");
        }
        download_file(&client, sources, spec, &file, &partial, retry_base, cancel).await?;
    }

    std::fs::write(partial.join(COMPLETE_MARKER), spec.revision)
        .with_context(|| "写入模型完成标记失败")?;
    if dir.exists() {
        // 残缺的旧目录（revision 不符或缺文件），整体替换。
        std::fs::remove_dir_all(dir)
            .with_context(|| format!("清理旧模型目录失败: {}", dir.display()))?;
    }
    std::fs::rename(&partial, dir)
        .with_context(|| format!("落盘模型目录失败: {}", dir.display()))?;
    tracing::info!(model = spec.id, dir = %dir.display(), "Memory 内置模型下载完成");
    Ok(())
}

async fn download_file(
    client: &reqwest::Client,
    sources: &[String],
    spec: &LocalModelSpec,
    file: &ModelFile,
    partial: &Path,
    retry_base: Duration,
    cancel: &CancelToken,
) -> Result<()> {
    fetch_verified(
        client,
        sources,
        spec.id,
        file,
        partial,
        retry_base,
        cancel,
        |source| file_url(source, spec, file),
    )
    .await
}

/// 从多个来源下载单个文件到 `partial/<local_name>`：断点续传、换源、sha256 校验。
///
/// `url_for` 把下载源映射为具体地址；模型与运行库的路径布局不同，其余流程共用。
#[allow(clippy::too_many_arguments)]
async fn fetch_verified(
    client: &reqwest::Client,
    sources: &[String],
    label: &str,
    file: &ModelFile,
    partial: &Path,
    retry_base: Duration,
    cancel: &CancelToken,
    url_for: impl Fn(&str) -> String,
) -> Result<()> {
    let target = partial.join(file.local_name());
    if std::fs::metadata(&target)
        .map(|meta| meta.len() == file.size)
        .unwrap_or(false)
        && sha256_file(&target)? == file.sha256
    {
        return Ok(());
    }
    let part = partial.join(format!("{}.part", file.local_name()));
    let mut last_error = None;
    for round in 0..MAX_ROUNDS {
        for source in sources {
            if cancel.is_cancelled() {
                bail!("下载已取消");
            }
            let url = url_for(source);
            match fetch_resume(client, &url, &part, file.size).await {
                Ok(()) => {
                    let actual = sha256_file(&part)?;
                    if actual != file.sha256 {
                        // 内容不符：丢弃重下，并换下一个来源。
                        let _ = std::fs::remove_file(&part);
                        last_error = Some(anyhow!(
                            "{} 校验失败（来源 {source}）：sha256 {actual} ≠ {}",
                            file.path,
                            file.sha256
                        ));
                        continue;
                    }
                    std::fs::rename(&part, &target)
                        .with_context(|| format!("落盘模型文件失败: {}", target.display()))?;
                    return Ok(());
                }
                Err(error) => {
                    tracing::debug!(
                        model = label,
                        file = file.path,
                        source = %source,
                        round,
                        "Memory 模型文件下载中断，稍后续传: {error:#}"
                    );
                    last_error = Some(error);
                }
            }
        }
        if round + 1 < MAX_ROUNDS {
            tokio::time::sleep(retry_base * (round as u32 + 1).min(5)).await;
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("没有可用的下载源")))
        .with_context(|| format!("下载 {label} 的 {} 失败", file.path))
}

/// 运行库只托管在天工 OSS（及自定义镜像）上：ModelScope / HuggingFace 按模型
/// 仓库布局寻址，没有这些文件，跳过以免无效请求。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn runtime_sources() -> Vec<String> {
    sources()
        .into_iter()
        .filter(|source| !source.contains("{repo}") && !is_model_hub(source))
        .collect()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_model_hub(source: &str) -> bool {
    source.contains("huggingface.co") || source.contains("hf-mirror.com")
}

/// 确保当前平台的 ONNX Runtime 动态库已下载，返回库文件绝对路径。
///
/// 目录：`<root>/onnxruntime/<version>/<platform>/`，与模型共用下载锁与校验流程。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) async fn ensure_runtime_library(
    root: &Path,
    library: &super::runtime::RuntimeLibrary,
    cancel: &CancelToken,
) -> std::result::Result<PathBuf, DownloadError> {
    ensure_runtime_library_from(
        root,
        library,
        &runtime_sources(),
        Duration::from_secs(1),
        cancel,
    )
    .await
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
async fn ensure_runtime_library_from(
    root: &Path,
    library: &super::runtime::RuntimeLibrary,
    sources: &[String],
    retry_base: Duration,
    cancel: &CancelToken,
) -> std::result::Result<PathBuf, DownloadError> {
    let dir = super::runtime::library_dir(root, library);
    let target = dir.join(library.file.local_name());
    let installed = |target: &Path| {
        std::fs::metadata(target)
            .map(|meta| meta.len() == library.file.size)
            .unwrap_or(false)
    };
    if installed(&target) {
        return Ok(target);
    }
    if cancel.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }
    let Some(_lock) = DownloadLock::acquire(root).map_err(DownloadError::Failed)? else {
        return Err(DownloadError::Busy);
    };
    if installed(&target) {
        return Ok(target);
    }
    let result = async {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建运行库目录失败: {}", dir.display()))?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .build()
            .with_context(|| "创建运行库下载客户端失败")?;
        let url_path = super::runtime::library_url_path(library);
        // fetch_verified 仅在 sha256 一致后才把 .part 改名为目标文件，
        // 因此目标文件存在即代表已校验。
        fetch_verified(
            &client,
            sources,
            "onnxruntime",
            &library.file,
            &dir,
            retry_base,
            cancel,
            |source| format!("{source}/{url_path}"),
        )
        .await?;
        tracing::info!(path = %target.display(), "Memory ONNX Runtime 运行库下载完成");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    match result {
        Ok(()) => Ok(target),
        Err(_) if cancel.is_cancelled() => Err(DownloadError::Cancelled),
        Err(error) => Err(DownloadError::Failed(error)),
    }
}

/// 运行库已下载的字节数（含 `.part`），供页面展示进度。
#[cfg(test)]
pub(crate) fn runtime_partial_bytes(root: &Path, library: &super::runtime::RuntimeLibrary) -> u64 {
    let dir = super::runtime::library_dir(root, library);
    let name = library.file.local_name();
    let done = std::fs::metadata(dir.join(name))
        .map(|m| m.len())
        .unwrap_or(0);
    let part = std::fs::metadata(dir.join(format!("{name}.part")))
        .map(|m| m.len())
        .unwrap_or(0);
    done.max(part).min(library.file.size)
}

/// 从 `part` 当前长度续传到 `expected` 字节。
async fn fetch_resume(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    expected: u64,
) -> Result<()> {
    let mut offset = std::fs::metadata(part).map(|meta| meta.len()).unwrap_or(0);
    if offset > expected {
        std::fs::remove_file(part).ok();
        offset = 0;
    }
    if offset == expected {
        return Ok(());
    }
    let mut request = client.get(url);
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let response = request.send().await.with_context(|| "请求失败")?;
    let status = response.status();
    let resumed = status == reqwest::StatusCode::PARTIAL_CONTENT;
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(resumed)
        .write(true)
        .truncate(!resumed)
        .open(part)
        .with_context(|| format!("打开临时文件失败: {}", part.display()))?;
    if !resumed {
        // 服务端不支持 Range：从头写。
        offset = 0;
    }
    let mut written = offset;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| "读取响应失败")?;
        written += chunk.len() as u64;
        if written > expected {
            bail!("响应长度超出预期（{expected} 字节）");
        }
        out.write_all(&chunk).with_context(|| "写入临时文件失败")?;
    }
    out.flush().ok();
    if written != expected {
        bail!("连接提前结束（{written}/{expected} 字节）");
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("读取文件失败: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read =
            std::io::Read::read(&mut file, &mut buffer).with_context(|| "计算 sha256 失败")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::catalog::LocalModelKind;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// 测试用模型：5 个小文件，内容由路径决定。
    fn content(path: &str) -> Vec<u8> {
        path.repeat(700).into_bytes()
    }

    fn leak(value: String) -> &'static str {
        Box::leak(value.into_boxed_str())
    }

    fn test_spec(corrupt_onnx: bool) -> LocalModelSpec {
        let f = |path: &'static str| {
            let bytes = content(path);
            let digest = if corrupt_onnx && path.ends_with(".onnx") {
                "0".repeat(64)
            } else {
                sha(&bytes)
            };
            ModelFile {
                path,
                size: bytes.len() as u64,
                sha256: leak(digest),
            }
        };
        LocalModelSpec {
            id: "test-model",
            kind: LocalModelKind::Embedding,
            repo: "org/test-model",
            revision: "0123456789012345678901234567890123456789",
            onnx: f("onnx/model_quantized.onnx"),
            tokenizer: f("tokenizer.json"),
            config: f("config.json"),
            special_tokens_map: f("special_tokens_map.json"),
            tokenizer_config: f("tokenizer_config.json"),
            dimension: 4,
        }
    }

    /// 极简 HTTP 服务：支持 Range；前 `drop_first` 次请求只发一半内容后断开。
    fn serve(drop_first: usize) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .splitn(5, '/')
                    .nth(4)
                    .unwrap_or("")
                    .to_string();
                let path = path
                    .split_once('/')
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_default();
                let body = content(&path);
                let start = request
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .map(str::to_string)
                    })
                    .and_then(|value| value.trim_end_matches('-').parse::<usize>().ok())
                    .unwrap_or(0);
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let slice = &body[start.min(body.len())..];
                let status = if start > 0 {
                    "206 Partial Content"
                } else {
                    "200 OK"
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(header.as_bytes());
                if index < drop_first {
                    let _ = stream.write_all(&slice[..slice.len() / 2]);
                    continue; // 提前断开
                }
                let _ = stream.write_all(slice);
            }
        });
        (base, hits)
    }

    const FAST_RETRY: Duration = Duration::from_millis(10);

    #[tokio::test]
    async fn downloads_resumes_and_verifies() {
        let (base, hits) = serve(2);
        let sources = [base];
        let root = tempfile::tempdir().unwrap();
        let spec = test_spec(false);
        let cancel = CancelToken::default();
        let dir = ensure_model_from(root.path(), &spec, &sources, FAST_RETRY, &cancel)
            .await
            .expect("下载应成功");
        assert!(is_installed(root.path(), &spec));
        assert_eq!(
            std::fs::read(dir.join("model_quantized.onnx")).unwrap(),
            content("onnx/model_quantized.onnx")
        );
        assert!(!root.path().join(".partial-test-model").exists());
        // 前两次断开，续传后成功：总请求数 = 5 个文件 + 2 次续传。
        assert_eq!(hits.load(Ordering::SeqCst), 7);

        // 已安装时不再请求。
        ensure_model_from(root.path(), &spec, &sources, FAST_RETRY, &cancel)
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 7);
    }

    #[tokio::test]
    async fn busy_lock_is_reported_separately_from_failure() {
        let (base, hits) = serve(0);
        let sources = [base];
        let root = tempfile::tempdir().unwrap();
        let spec = test_spec(false);
        // 模拟另一个进程持锁：本次应返回 Busy 而不是下载失败。
        let _held = DownloadLock::acquire_for_other_process(root.path()).expect("持锁");
        let error = ensure_model_from(
            root.path(),
            &spec,
            &sources,
            FAST_RETRY,
            &CancelToken::default(),
        )
        .await
        .expect_err("锁被占用应返回 Busy");
        assert!(matches!(error, DownloadError::Busy), "{error}");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "Busy 时不应发起请求");
    }

    #[tokio::test]
    async fn cancelled_download_stops_without_failure() {
        let (base, hits) = serve(0);
        let sources = [base];
        let root = tempfile::tempdir().unwrap();
        let spec = test_spec(false);
        let cancel = CancelToken::default();
        cancel.cancel();
        let error = ensure_model_from(root.path(), &spec, &sources, FAST_RETRY, &cancel)
            .await
            .expect_err("取消后应停止");
        assert!(matches!(error, DownloadError::Cancelled), "{error}");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "取消后不应发起请求");
    }

    #[tokio::test]
    async fn rejects_checksum_mismatch() {
        let (base, _) = serve(0);
        let root = tempfile::tempdir().unwrap();
        let spec = test_spec(true);
        let error = download_file(
            &reqwest::Client::new(),
            std::slice::from_ref(&base),
            &spec,
            &spec.onnx,
            root.path(),
            FAST_RETRY,
            &CancelToken::default(),
        )
        .await
        .expect_err("sha256 不符应失败");
        assert!(format!("{error:#}").contains("校验失败"), "{error:#}");
        assert!(!root.path().join("model_quantized.onnx").exists());
        assert!(!is_installed(root.path(), &spec));
    }

    #[tokio::test]
    async fn runtime_library_downloads_and_verifies() {
        let bytes = b"fake-onnxruntime-shared-object".repeat(1000);
        let digest = sha(&bytes);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = bytes.clone();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                counter.fetch_add(1, Ordering::SeqCst);
                let ok = request
                    .starts_with("GET /onnxruntime/1.24.1/linux-test/libonnxruntime.so.1.24.1 ");
                let response = if ok {
                    let mut head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        served.len()
                    )
                    .into_bytes();
                    head.extend_from_slice(&served);
                    head
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                };
                let _ = std::io::Write::write_all(&mut stream, &response);
            }
        });
        let library = super::super::runtime::RuntimeLibrary {
            platform: "linux-test",
            file: ModelFile {
                path: "libonnxruntime.so.1.24.1",
                size: bytes.len() as u64,
                sha256: leak(digest),
            },
        };
        let root = tempfile::tempdir().unwrap();
        let cancel = CancelToken::default();
        let path = ensure_runtime_library_from(
            root.path(),
            &library,
            std::slice::from_ref(&base),
            FAST_RETRY,
            &cancel,
        )
        .await
        .expect("运行库下载应成功");
        assert_eq!(
            path,
            root.path()
                .join("onnxruntime/1.24.1/linux-test/libonnxruntime.so.1.24.1")
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            runtime_partial_bytes(root.path(), &library),
            library.file.size
        );

        // 已存在时不再请求。
        let before = hits.load(Ordering::SeqCst);
        ensure_runtime_library_from(root.path(), &library, &[base], FAST_RETRY, &cancel)
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), before);
    }

    #[test]
    #[serial_test::serial]
    fn runtime_sources_skip_model_hubs() {
        unsafe { std::env::set_var(MODEL_MIRROR_ENV, "https://mirror.example") };
        let list = runtime_sources();
        unsafe { std::env::remove_var(MODEL_MIRROR_ENV) };
        assert_eq!(
            list,
            [
                "https://mirror.example",
                "https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/memory-models",
            ]
        );
    }

    #[test]
    #[serial_test::serial]
    fn mirror_env_is_tried_first() {
        unsafe { std::env::set_var(MODEL_MIRROR_ENV, " https://a.example/ ,https://b.example") };
        let list = sources();
        unsafe { std::env::remove_var(MODEL_MIRROR_ENV) };
        assert_eq!(
            list,
            [
                "https://a.example",
                "https://b.example",
                "https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/memory-models",
                "https://www.modelscope.cn/models/{repo}/resolve/master/{path}",
                "https://hf-mirror.com",
                "https://huggingface.co",
            ]
        );
        let spec = test_spec(false);
        assert_eq!(
            file_url(&list[0], &spec, &spec.onnx),
            "https://a.example/org/test-model/resolve/0123456789012345678901234567890123456789/onnx/model_quantized.onnx"
        );
        assert_eq!(
            file_url(&list[3], &spec, &spec.onnx),
            "https://www.modelscope.cn/models/org/test-model/resolve/master/onnx/model_quantized.onnx"
        );
        assert_eq!(
            file_url(&list[2], &spec, &spec.tokenizer),
            "https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/memory-models/org/test-model/resolve/0123456789012345678901234567890123456789/tokenizer.json"
        );
    }
}
