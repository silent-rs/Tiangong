//! 导航地址规范化与资源页识别（纯函数）。

use super::*;

/// 规范化 URL 用于比较：去除末尾的 /，统一 https://
pub(super) fn normalize_url_for_compare(url: &str) -> String {
    let s = url.trim_end_matches('/');
    s.to_string()
}

/// 构造中的 file URL 先编码会破坏结构的字符再经 Url 解析规范化编码
/// （空格/非 ASCII 等）；# 与 ? 不预编码会被切成 fragment/query。
pub(super) fn canonical_file_url(candidate: String) -> String {
    let encoded = candidate
        .replace('%', "%25")
        .replace('#', "%23")
        .replace('?', "%3F");
    match encoded.parse::<Url>() {
        Ok(url) => url.to_string(),
        Err(_) => encoded,
    }
}

/// 裸本地路径转标准 file URL；非本地路径形式原样返回。
/// Windows 盘符路径（`C:\x`、`c:/x`，允许混合斜杠）与 UNC 路径
/// （`\\server\share\x`）按平台无关规则转写，盘符统一大写。
pub(super) fn local_path_to_file_url(value: &str) -> String {
    // UNC 网络路径：反斜杠写法各平台一致按主机名处理；正斜杠写法
    // （//server/share）在 Windows 生态常见，但 Unix 上前导双斜杠是
    // implementation-defined 的本地路径，因此仅 Windows 按 UNC 归一。
    #[cfg(windows)]
    if let Some(rest) = value.strip_prefix("//") {
        return canonical_file_url(format!("file://{}", rest.replace('\\', "/")));
    }
    if let Some(rest) = value.strip_prefix(r"\\") {
        return canonical_file_url(format!("file://{}", rest.replace('\\', "/")));
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let drive = bytes[0].to_ascii_uppercase() as char;
        return canonical_file_url(format!("file:///{drive}:{}", value[2..].replace('\\', "/")));
    }
    if bytes.first() == Some(&b'/') {
        return canonical_file_url(format!("file:///{}", value.trim_start_matches('/')));
    }
    value.to_string()
}

/// 导航地址统一归一化：本地路径（Windows 盘符 / UNC / Unix 绝对路径）
/// 与 file: 变体（`file:C:/x`、`file://C:/x`）收敛为标准三斜杠形式并
/// 大写盘符（对齐 Chromium 实际地址，保证导航状态比较一致）；其余
/// 地址原样返回。幂等：已是标准形式的输入输出不变。
pub(crate) fn normalize_navigation_url(raw: &str) -> String {
    let value = raw.trim();
    if value
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        if let Ok(parsed) = value.parse::<Url>() {
            let path = parsed.path();
            if parsed.host_str().is_none() {
                let bytes = path.as_bytes();
                if bytes.len() >= 3
                    && bytes[0] == b'/'
                    && bytes[1].is_ascii_alphabetic()
                    && bytes[2] == b':'
                {
                    // 盘符统一大写时重拼字符串，query 与 fragment 需一并带回
                    let drive = bytes[1].to_ascii_uppercase() as char;
                    let mut rebuilt = format!("file:///{drive}{}", &path[2..]);
                    if let Some(query) = parsed.query() {
                        rebuilt.push('?');
                        rebuilt.push_str(query);
                    }
                    if let Some(fragment) = parsed.fragment() {
                        rebuilt.push('#');
                        rebuilt.push_str(fragment);
                    }
                    return rebuilt;
                }
            }
            return parsed.to_string();
        }
        // 解析失败的 file: 变体剥掉协议与斜杠后按裸路径重建兜底。
        // 注意与裸路径的语义差异：此处 % 视为已编码序列原样保留，
        // 裸路径则把字面 % 预编码为 %25——两种写法约定不同，勿"统一"。
        return local_path_to_file_url(value[5..].trim_start_matches('/'));
    }
    local_path_to_file_url(value)
}

/// 图片、PDF、音视频、字体等按扩展名识别的非 HTML 资源地址。此类资源页
/// 没有可注入脚本的 HTML 文档（eval 无响应），不能走 DOM 快照流程。
/// SVG 例外：浏览器中按 XML 文档渲染，有 DOM 且可执行脚本，按正常页面处理。
pub(super) fn is_non_html_resource_url(url: &str) -> bool {
    let Ok(parsed) = url.parse::<Url>() else {
        return false;
    };
    let Some(extension) = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension))
    else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "ico"
            | "bmp"
            | "pdf"
            | "mp4"
            | "webm"
            | "mov"
            | "mkv"
            | "ogg"
            | "mp3"
            | "wav"
            | "flac"
            | "m4a"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
    )
}

/// 展示用百分号解码：文件名等 UI 文本按 UTF-8 解回原文；
/// 非法编码序列原样保留，不因解码失败丢信息。
pub(super) fn percent_decode_lossy(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            let hex = |b: u8| (b as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hex(bytes[index + 1]) * 16 + hex(bytes[index + 2]));
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 资源页展示标题：地址最后一段路径解码后的文件名（标签页与工具
/// 结果共用，保证界面与 web_fetch 汇报一致）。
pub(super) fn resource_page_title(url: &str) -> String {
    let file_name = url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .split('#')
        .next()
        .unwrap_or_default();
    percent_decode_lossy(file_name)
}

pub(super) fn agent_domain_for_url(url: &str) -> Result<String, String> {
    let parsed = url
        .parse::<Url>()
        .map_err(|error| format!("URL 解析失败：{error}"))?;
    let Some(host) = parsed.host_str() else {
        return match parsed.scheme() {
            "file" => Ok("file:".to_string()),
            scheme => Err(format!("{scheme} 地址不包含可识别的主机名")),
        };
    };
    let normalized_host = host.trim_end_matches('.').to_ascii_lowercase();
    Ok(psl::domain_str(&normalized_host)
        .unwrap_or(&normalized_host)
        .to_string())
}

pub(super) fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(super) fn navigation_error_data_url(requested_url: &str) -> String {
    let escaped_url = escape_html(requested_url);
    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN" data-tiangong-navigation-error="true">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>页面加载异常</title>
  <style>
    :root {{ color-scheme: light dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: Canvas; color: CanvasText; }}
    main {{ width: min(560px, calc(100% - 40px)); }}
    h1 {{ margin: 0 0 12px; font-size: 24px; letter-spacing: 0; }}
    p {{ margin: 0 0 20px; line-height: 1.6; color: GrayText; }}
    code {{ display: block; margin-bottom: 24px; padding: 12px; overflow-wrap: anywhere; border: 1px solid color-mix(in srgb, CanvasText 18%, transparent); border-radius: 6px; font-family: ui-monospace, monospace; font-size: 13px; }}
    a {{ display: inline-block; padding: 9px 14px; border-radius: 6px; background: CanvasText; color: Canvas; text-decoration: none; font-weight: 600; }}
  </style>
</head>
<body>
  <main>
    <h1>页面加载异常</h1>
    <p>{PAGE_LOAD_ERROR_MESSAGE}</p>
    <code>{escaped_url}</code>
    <a href="{escaped_url}">重新加载</a>
  </main>
</body>
</html>"#
    );
    format!(
        "data:text/html;base64,{}",
        base64_url::encode(html.as_bytes())
    )
}
