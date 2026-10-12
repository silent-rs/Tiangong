//! web_fetch 业务实现：reqwest 阻塞抓取、SSRF 防护、text 提取与 download 落盘。
//!
//! 整体从原进程内插件 `handler.rs` 迁移，改造点：
//! - 入参由 `ToolCall` 改为 `protocol::WebFetchRequest`，URL 解析与 SSRF 校验在 sidecar 内做
//! - 输出由 `core::ToolResult` 改为 `protocol::WebFetchResponse`
//! - 去掉 `AtomicBool` 取消机制：sidecar 内每次请求由 `spawn_blocking` 独立执行，
//!   不再依赖 future drop 取消（原机制专为进程内 async handler 设计）

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, LOCATION, USER_AGENT};
use reqwest::{StatusCode, Url};
use scraper::{Html, Selector};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tiangong_plugin_fetch_protocol::web_fetch::{WebFetchRequest, WebFetchResponse};
use tiangong_plugin_fetch_protocol::{ExtractMode, FetchMode};
use tiangong_toolkit as shared;

const MAX_TIMEOUT_MS: u64 = 60_000;
const MAX_CHARS: usize = 50_000;
const MAX_BODY_BYTES: usize = 1_048_576;
const MAX_DOWNLOAD_BYTES: u64 = 104_857_600;
const MAX_REDIRECTS: usize = 5;

/// 执行一次 web_fetch（在 spawn_blocking 线程内调用）。
pub fn execute(request: WebFetchRequest, base: &Path, is_full_trust: bool) -> WebFetchResponse {
    match run(request, base, is_full_trust) {
        Ok(resp) => resp,
        Err(error) => error_response(error),
    }
}

fn run(request: WebFetchRequest, base: &Path, is_full_trust: bool) -> Result<WebFetchResponse> {
    let (request, url) = normalize_request(request)?;
    let client = Client::builder()
        .timeout(Duration::from_millis(request.timeout_ms))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("创建 web_fetch HTTP 客户端失败")?;

    let original_url = url.clone();
    let response = fetch_with_redirects(&client, url, request.follow_redirects)?;
    let final_url = response.url().clone();
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("HTTP 状态码错误：{status}"));
    }

    match request.mode {
        FetchMode::Text => finish_text_fetch(&request, original_url, final_url, status, response),
        FetchMode::Download => finish_download(
            &request,
            original_url,
            final_url,
            status,
            response,
            base,
            is_full_trust,
        ),
    }
}

/// 规范化请求：解析并校验 URL（含 SSRF），clamp 数值参数。
///
/// 返回更新后的请求与解析出的 `Url`（下游重定向、落盘需要强类型 Url）。
fn normalize_request(mut request: WebFetchRequest) -> Result<(WebFetchRequest, Url)> {
    let raw_url = request.url.trim();
    if raw_url.is_empty() {
        return Err(anyhow!("web_fetch 缺少 url 参数"));
    }
    let url = Url::parse(raw_url).with_context(|| format!("URL 格式非法：{raw_url}"))?;
    ensure_url_allowed(&url)?;
    request.url = url.to_string();

    request.max_chars = request.max_chars.clamp(1, MAX_CHARS);
    request.timeout_ms = request.timeout_ms.clamp(1_000, MAX_TIMEOUT_MS);

    if request.mode == FetchMode::Download {
        // output_path 允许为空（走 URL 末段推断），但显式空串视为未提供。
        if request
            .output_path
            .as_deref()
            .is_some_and(|p| p.trim().is_empty())
        {
            request.output_path = None;
        }
    }
    Ok((request, url))
}

#[derive(Serialize)]
struct TextOutput {
    mode: &'static str,
    url: String,
    final_url: String,
    status: u16,
    content_type: String,
    title: Option<String>,
    text: String,
    truncated: bool,
    bytes_read: usize,
}

#[derive(Serialize)]
struct DownloadOutput {
    mode: &'static str,
    url: String,
    final_url: String,
    status: u16,
    content_type: String,
    file_path: String,
    bytes_written: u64,
    sha256: String,
}

fn finish_text_fetch(
    request: &WebFetchRequest,
    original_url: Url,
    final_url: Url,
    status: StatusCode,
    mut response: Response,
) -> Result<WebFetchResponse> {
    let content_type = header_text(response.headers(), CONTENT_TYPE.as_str());
    ensure_text_content_type(&content_type)?;
    let bytes = read_limited(&mut response, MAX_BODY_BYTES)?;
    let bytes_read = bytes.len();
    let raw_text = String::from_utf8(bytes).context("响应体不是有效 UTF-8 文本")?;
    let extracted = extract_text(&raw_text, &content_type, request.extract_mode);
    let truncated = extracted.text.chars().count() > request.max_chars;
    let text = if truncated {
        extracted.text.chars().take(request.max_chars).collect()
    } else {
        extracted.text
    };

    let output = TextOutput {
        mode: "text",
        url: original_url.to_string(),
        final_url: final_url.to_string(),
        status: status.as_u16(),
        content_type,
        title: extracted.title,
        text,
        truncated,
        bytes_read,
    };
    let stdout = serde_json::to_string_pretty(&output).context("序列化 web_fetch 输出失败")?;
    Ok(WebFetchResponse {
        ok: true,
        summary: format!(
            "web_fetch 读取成功：{} status={} bytes={} truncated={}",
            final_url, output.status, output.bytes_read, output.truncated
        ),
        stdout,
        stderr: String::new(),
        exit_code: 0,
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_download(
    request: &WebFetchRequest,
    original_url: Url,
    final_url: Url,
    status: StatusCode,
    mut response: Response,
    base: &Path,
    is_full_trust: bool,
) -> Result<WebFetchResponse> {
    if let Some(length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        && length > MAX_DOWNLOAD_BYTES
    {
        return Err(anyhow!(
            "下载文件超过大小限制：{} > {}",
            length,
            MAX_DOWNLOAD_BYTES
        ));
    }

    let content_type = header_text(response.headers(), CONTENT_TYPE.as_str());
    let target = resolve_download_target(
        request.output_path.as_deref(),
        &final_url,
        base,
        is_full_trust,
    )?;
    ensure_download_target(&target, request.overwrite)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建下载目录失败：{}", parent.display()))?;
    }

    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("下载目标文件名非法：{}", target.display()))?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("无法确定下载目标父目录：{}", target.display()))?;
    let temp_path = parent.join(format!(".{}.tmp-{}", file_name, scru128::new()));

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .with_context(|| format!("创建下载临时文件失败：{}", temp_path.display()))?;
    let mut hasher = Sha256::new();
    let mut written = 0_u64;
    let mut buf = [0_u8; 16 * 1024];
    loop {
        let n = response
            .read(&mut buf)
            .with_context(|| format!("读取下载响应失败：{}", final_url))?;
        if n == 0 {
            break;
        }
        written = written.saturating_add(n as u64);
        if written > MAX_DOWNLOAD_BYTES {
            let _ = fs::remove_file(&temp_path);
            return Err(anyhow!("下载文件超过大小限制：{}", MAX_DOWNLOAD_BYTES));
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("写入下载临时文件失败：{}", temp_path.display()))?;
    }
    file.flush()
        .with_context(|| format!("刷新下载临时文件失败：{}", temp_path.display()))?;

    if request.overwrite && target.exists() {
        fs::remove_file(&target)
            .with_context(|| format!("删除旧下载文件失败：{}", target.display()))?;
    }
    fs::rename(&temp_path, &target).with_context(|| {
        format!(
            "移动下载文件失败：temp={}, target={}",
            temp_path.display(),
            target.display()
        )
    })?;

    let sha256 = hex::encode(hasher.finalize());
    let output = DownloadOutput {
        mode: "download",
        url: original_url.to_string(),
        final_url: final_url.to_string(),
        status: status.as_u16(),
        content_type,
        file_path: target.display().to_string(),
        bytes_written: written,
        sha256,
    };
    let stdout = serde_json::to_string_pretty(&output).context("序列化 web_fetch 下载输出失败")?;
    Ok(WebFetchResponse {
        ok: true,
        summary: format!(
            "web_fetch 下载成功：{} ({} bytes, sha256={})",
            shared::display_rel_path_with(&target, base),
            output.bytes_written,
            output.sha256
        ),
        stdout,
        stderr: String::new(),
        exit_code: 0,
    })
}

fn fetch_with_redirects(client: &Client, mut url: Url, follow_redirects: bool) -> Result<Response> {
    for redirect_count in 0..=MAX_REDIRECTS {
        ensure_url_allowed(&url)?;
        let response = client
            .get(url.clone())
            .header(USER_AGENT, "tiangong-web-fetch/0.1")
            .header(ACCEPT, "*/*")
            .send()
            .with_context(|| format!("请求 URL 失败：{url}"))?;

        if !follow_redirects || !response.status().is_redirection() {
            return Ok(response);
        }
        if redirect_count == MAX_REDIRECTS {
            return Err(anyhow!("重定向次数超过限制：{MAX_REDIRECTS}"));
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("重定向响应缺少 Location：{}", response.status()))?;
        url = url
            .join(location)
            .with_context(|| format!("解析重定向 Location 失败：{location}"))?;
    }
    Err(anyhow!("重定向处理异常"))
}

fn ensure_url_allowed(url: &Url) -> Result<()> {
    match url.scheme() {
        "http" | "https" => {}
        scheme => return Err(anyhow!("协议不允许：{scheme}")),
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("URL 缺少 host：{url}"))?;
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost" || host_lower.ends_with(".localhost") {
        return Err(anyhow!("默认拒绝访问本机地址：{host}"));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        ensure_public_ip(ip, host)?;
        return Ok(());
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("无法确定 URL 端口：{url}"))?;
    let addrs = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("解析目标地址失败：{host}"))?;
    let mut resolved = false;
    for addr in addrs {
        resolved = true;
        ensure_public_ip(addr.ip(), host)?;
    }
    if !resolved {
        return Err(anyhow!("解析目标地址为空：{host}"));
    }
    Ok(())
}

fn ensure_public_ip(ip: IpAddr, host: &str) -> Result<()> {
    let denied = match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.octets()[0] == 0
                || ip.octets()[0] >= 224
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
        }
    };
    if denied {
        return Err(anyhow!("默认拒绝访问非公网地址：{host} ({ip})"));
    }
    Ok(())
}

fn read_limited(response: &mut Response, limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0_u8; 16 * 1024];
    loop {
        let n = response.read(&mut buf).context("读取响应体失败")?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > limit {
            return Err(anyhow!("响应体超过大小限制：{limit} bytes"));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

fn ensure_text_content_type(content_type: &str) -> Result<()> {
    let ct = content_type.to_ascii_lowercase();
    if ct.is_empty()
        || ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("markdown")
        || ct.contains("html")
    {
        return Ok(());
    }
    Err(anyhow!("内容类型不支持 text 模式：{content_type}"))
}

struct ExtractedText {
    title: Option<String>,
    text: String,
}

fn extract_text(raw: &str, content_type: &str, mode: ExtractMode) -> ExtractedText {
    if mode == ExtractMode::Raw || !looks_like_html(content_type, raw) {
        return ExtractedText {
            title: None,
            text: normalize_text(raw),
        };
    }

    let cleaned = remove_html_block(raw, "script");
    let cleaned = remove_html_block(&cleaned, "style");
    let cleaned = remove_html_block(&cleaned, "noscript");
    let cleaned = remove_html_block(&cleaned, "svg");
    let document = Html::parse_document(&cleaned);
    let title = Selector::parse("title").ok().and_then(|selector| {
        document
            .select(&selector)
            .next()
            .map(|node| normalize_text(&node.text().collect::<Vec<_>>().join(" ")))
            .filter(|text| !text.is_empty())
    });
    let text = Selector::parse("body")
        .ok()
        .and_then(|selector| document.select(&selector).next())
        .map(|body| body.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| document.root_element().text().collect::<Vec<_>>().join(" "));

    ExtractedText {
        title,
        text: normalize_text(&text),
    }
}

fn looks_like_html(content_type: &str, raw: &str) -> bool {
    if content_type.to_ascii_lowercase().contains("html") {
        return true;
    }
    raw.trim_start()
        .to_ascii_lowercase()
        .starts_with("<!doctype html")
        || raw.trim_start().to_ascii_lowercase().starts_with("<html")
}

fn normalize_text(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn remove_html_block(raw: &str, tag: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = lower.find(&open) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_start = &rest[start..];
        let lower_after_start = after_start.to_ascii_lowercase();
        let Some(end) = lower_after_start.find(&close) else {
            break;
        };
        let skip = end + close.len();
        rest = &after_start[skip..];
    }
    out
}

fn resolve_download_target(
    raw_path: Option<&str>,
    final_url: &Url,
    base: &Path,
    is_full_trust: bool,
) -> Result<PathBuf> {
    let path = raw_path
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| infer_download_file_name(final_url));
    // 写范围由 OS 沙箱裁决，信任模式不影响下载落点解析。
    let _ = is_full_trust;
    shared::resolve_write_path_from_base(&path, base)
}

fn infer_download_file_name(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map(sanitize_file_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "download.bin".to_string())
}

fn sanitize_file_name(raw: &str) -> String {
    raw.chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect()
}

fn ensure_download_target(path: &Path, overwrite: bool) -> Result<()> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(anyhow!(
            "下载目标路径不允许包含路径穿越：{}",
            path.display()
        ));
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(anyhow!("下载目标不能是符号链接：{}", path.display()));
        }
        if meta.is_dir() {
            return Err(anyhow!("下载目标不能是目录：{}", path.display()));
        }
        if !overwrite {
            return Err(anyhow!("下载目标文件已存在：{}", path.display()));
        }
    }
    Ok(())
}

fn header_text(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn error_response(error: anyhow::Error) -> WebFetchResponse {
    // `{:#}` 展开完整错误链，保留底层 IO 原因（如沙箱拒绝落盘）。
    let summary = format!("web_fetch 失败：{error:#}");
    WebFetchResponse {
        ok: false,
        summary: summary.clone(),
        stdout: String::new(),
        stderr: summary,
        exit_code: 1,
    }
}
