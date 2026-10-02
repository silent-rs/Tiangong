# 内嵌浏览器通行密钥（Passkey / WebAuthn）

## 背景

WKWebView 默认只允许应用关联域名使用通行密钥。内嵌浏览器要让任意网站使用 iCloud 钥匙串里的通行密钥，需要 Apple 管控权限
`com.apple.developer.web-browser.public-key-credential`（仅 macOS，支持 Developer ID 分发）。

- 申请：2026-10-01 已提交，Request ID `74JWXCQKDG`，App ID `com.silent.tiangong`（Team `SE3EQ3A77S`）
- 状态查询：Certificates, Identifiers & Profiles → Identifiers → `com.silent.tiangong` → Capability Requests →
  Web Browser Public Key Credential Requests

## 实现（`src-tauri/src/webview_host/passkey/`）

| 文件 | 作用 |
|---|---|
| `js/passkey.js` | 注入页面，接管 `navigator.credentials.create/get({ publicKey })`，经 `window.webkit.messageHandlers.tiangongPasskey` 转交原生；处理器不存在时回退 WebKit 原生实现 |
| `passkey/protocol.rs` | 平台无关逻辑：消息解析、origin / RP ID 校验（含公共后缀拦截）、base64url、attestationObject（CBOR）解析与 ES256 公钥提取、响应 JSON、错误映射；全部有单测 |
| `passkey/macos.rs` | `WKScriptMessageHandlerWithReply` + `ASAuthorizationWebBrowserPublicKeyCredentialManager` + `ASAuthorizationController` |

流程：页面请求 → 原生用 WebKit 提供的 `frameInfo.securityOrigin` 校验来源（不信任页面自报）→ 授权状态为
「未决定」时调用 `requestAuthorizationForPublicKeyCredentials`（**网页第一次需要通行密钥时才弹系统授权**）→ 已授权则
发起系统通行密钥对话框 → 结果回传页面。用户拒绝时提示到「系统设置 > 隐私与安全性 > 访问网页浏览器的通行密钥」开启。

### 门控

`passkey::is_available()` 同时要求 macOS 13.5+ 与进程签名带有上述权限（`SecTaskCopyValueForEntitlement` 运行时检测）。
任一不满足时不注入脚本、不注册处理器，行为与改动前完全一致。因此本分支可在审批前合入。

### 当前限制

- 仅平台通行密钥（iCloud 钥匙串 / 手机扫码），不支持 USB 安全密钥（`cross-platform` 返回 NotSupportedError）
- 仅主框架；iframe 内的请求返回 NotAllowedError
- 不接管条件式（自动填充）请求 `mediation: "conditional"`，`isConditionalMediationAvailable()` 返回 false
- 不支持 PRF / largeBlob 等扩展，`getClientExtensionResults()` 返回空对象

## 审批通过后的上线步骤

1. 开发者后台为 `com.silent.tiangong` 启用 Web Browser Public Key Credential Requests 能力
2. 生成 **Developer ID** 类型的 provisioning profile（绑定该 App ID 与 Developer ID Application 证书）
3. `src-tauri/Entitlements.plist` 增加：
   ```xml
   <key>com.apple.application-identifier</key>
   <string>SE3EQ3A77S.com.silent.tiangong</string>
   <key>com.apple.developer.team-identifier</key>
   <string>SE3EQ3A77S</string>
   <key>com.apple.developer.web-browser.public-key-credential</key>
   <true/>
   ```
4. 打包时把 profile 放到 `Contents/embedded.provisionprofile`（`tauri.conf.json` 的 `bundle.macOS.files`），CI 增加
   profile secret 并在 `Verify macOS signature` 步骤检查 entitlement
5. 实机验证：签名包启动不闪退；在 <https://webauthn.io> 注册 + 登录；首次请求出现系统授权弹窗；拒绝后给出设置指引；
   系统设置列表中出现「天工」

> 未嵌入有效 profile 而直接写入第 3 步的权限，签名包启动会被系统终止，必须与第 2、4 步同时完成。
