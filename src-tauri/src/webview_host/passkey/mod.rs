//! 内嵌浏览器的通行密钥（WebAuthn / Passkey）桥接。
//!
//! WKWebView 默认只允许应用关联域名使用通行密钥。获得 Apple 授予的
//! `com.apple.developer.web-browser.public-key-credential` 权限后，浏览器类应用
//! 可以经 AuthenticationServices 代任意网站创建/使用 iCloud 钥匙串中的通行密钥。
//!
//! 流程：注入脚本接管页面的 `navigator.credentials.create/get({ publicKey })`
//! → `window.webkit.messageHandlers.tiangongPasskey` 送到原生 → 原生用
//! WebKit 提供的 frame 来源（不信任页面自报）校验 origin 与 RP ID →
//! 首次需要时请求系统授权（「访问网页浏览器的通行密钥」）→ 系统弹窗完成
//! 认证 → 结果回传页面。
//!
//! 门控：只有 macOS 13.5+ 且进程签名确实带有上述权限时才注入脚本、注册
//! 处理器（[`is_available`]）。权限审批通过前打出的包行为与现在完全一致。

pub mod protocol;

#[cfg(target_os = "macos")]
mod macos;

/// 注入页面的 WebAuthn 接管脚本（仅在 [`is_available`] 为真时注入）。
pub const PASSKEY_SCRIPT: &str = include_str!("../js/passkey.js");

/// 页面与原生通信的 WKScriptMessageHandler 名称（与 passkey.js 保持一致）。
pub const MESSAGE_HANDLER_NAME: &str = "tiangongPasskey";

/// Apple 管控的浏览器通行密钥权限。
pub const ENTITLEMENT: &str = "com.apple.developer.web-browser.public-key-credential";

/// 当前进程能否启用通行密钥桥接（结果缓存，进程内只检测一次）。
pub fn is_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::is_available()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// 为新建的浏览器 WebView 注册通行密钥消息处理器。
///
/// 调用方须先确认 [`is_available`]；处理器注册在主线程异步完成，注册前页面
/// 发起的请求由脚本回退到 WebKit 原生实现。
pub fn attach(webview: &tauri::Webview<tauri::Wry>) {
    #[cfg(target_os = "macos")]
    macos::attach(webview);
    #[cfg(not(target_os = "macos"))]
    let _ = webview;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_uses_registered_handler_name() {
        assert!(PASSKEY_SCRIPT.contains(&format!("'{MESSAGE_HANDLER_NAME}'")));
    }

    /// 测试进程未带浏览器通行密钥权限：门控必须关闭，且检测本身不崩溃。
    #[test]
    fn disabled_without_entitlement() {
        assert!(!is_available());
    }
}
