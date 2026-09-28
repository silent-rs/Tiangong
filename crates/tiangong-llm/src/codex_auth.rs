//! Codex（ChatGPT 账号）OAuth 登录与凭据管理。
//!
//! 参考 opencode `plugin/openai/codex.ts` 的做法，使用 OpenAI 公开的 Codex OAuth 客户端：
//! - 浏览器登录：PKCE 授权码流程，本地 `http://localhost:1455/auth/callback` 接收回调；
//! - 设备码登录：无浏览器回调场景（CLI / 远程），用户在网页输入验证码完成授权；
//! - 凭据落盘：`~/.tiangong/auth/codex.json`（Unix 下权限 0600）；
//! - 自动续期：access token 过期（或服务端返回 401）时用 refresh token 刷新并回写。
//!
//! 请求侧见 `providers/openai`：`ProviderProtocol::Codex` 复用 Responses 映射，
//! 发请求前通过 [`access`] 取得有效 access token、账号 ID 与数据驻留区域。

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, watch};

use crate::error::LlmError;

/// Codex 推理后端（Responses 兼容）地址。
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Codex 公开 OAuth 客户端 ID（Codex CLI / opencode 同款）。
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// `/models` 必填的 `client_version` 查询参数；取足够高的版本以返回全部可用模型。
pub const CODEX_MODELS_CLIENT_VERSION: &str = "99.0.0";
/// 请求头 `originator`：标识调用方客户端。
pub const CODEX_ORIGINATOR: &str = "tiangong";

const ISSUER: &str = "https://auth.openai.com";
const OAUTH_SCOPE: &str = "openid profile email offline_access";
const OAUTH_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DEVICE_POLL_SAFETY_MARGIN: Duration = Duration::from_secs(3);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// 响应未携带 `expires_in` 时的默认有效期（秒）。
const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;
/// 提前刷新余量（秒），避免请求途中过期。
const REFRESH_MARGIN_SECS: i64 = 60;

/// 落盘的 Codex 凭据。
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CodexCredentials {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// access token 过期时间（Unix 秒）。
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl std::fmt::Debug for CodexCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCredentials")
            .field("access_token", &!self.access_token.is_empty())
            .field("refresh_token", &!self.refresh_token.is_empty())
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("plan_type", &self.plan_type)
            .finish()
    }
}

/// 登录状态（供 UI / CLI 展示，不含任何令牌）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexAuthStatus {
    pub logged_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// 是否有进行中的登录（浏览器或设备码）。
    pub login_pending: bool,
}

/// 发起登录后返回给调用方的信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexLoginStart {
    /// 需要在浏览器中打开的地址。
    pub url: String,
    /// 设备码登录时需要用户输入的验证码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
}

/// 发请求所需的访问凭据。
#[derive(Clone)]
pub struct CodexAccess {
    pub access_token: String,
    pub account_id: Option<String>,
    /// 数据驻留区域（`chatgpt_compute_residency`，无约束时为 None）。
    pub residency: Option<String>,
}

impl std::fmt::Debug for CodexAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAccess")
            .field("account_id", &self.account_id)
            .field("residency", &self.residency)
            .finish_non_exhaustive()
    }
}

// ── 路径与存储 ─────────────────────────────────────────────

/// 与 `tiangong_config::io::storage_root` 保持一致（本 crate 不依赖 config）。
fn storage_root() -> PathBuf {
    if let Some(root) = std::env::var_os("TIANGONG_STORAGE_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(root);
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(".tiangong")
}

/// 天工保存 Codex 凭据的文件路径。
pub fn credentials_path() -> PathBuf {
    storage_root().join("auth").join("codex.json")
}

/// 读取已保存的凭据；未登录返回 `Ok(None)`。
pub fn load_credentials() -> Result<Option<CodexCredentials>> {
    let path = credentials_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("读取 Codex 凭据失败：{}", path.display()));
        }
    };
    let creds: CodexCredentials = serde_json::from_str(&raw)
        .with_context(|| format!("解析 Codex 凭据失败：{}", path.display()))?;
    Ok((!creds.access_token.trim().is_empty()).then_some(creds))
}

fn save_credentials(creds: &CodexCredentials) -> Result<()> {
    let path = credentials_path();
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("Codex 凭据路径无效：{}", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("创建凭据目录失败：{}", dir.display()))?;
    let body = serde_json::to_vec_pretty(creds).context("序列化 Codex 凭据失败")?;
    let tmp = path.with_extension(format!("json.tmp-{}", scru128::new()));
    if let Err(err) = write_private_file(&tmp, &body) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("写入 Codex 凭据失败：{}", tmp.display()));
    }
    if let Err(err) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("保存 Codex 凭据失败：{}", path.display()));
    }
    Ok(())
}

#[cfg(unix)]
fn write_private_file(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(body)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private_file(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, body)
}

// ── JWT 声明解析 ───────────────────────────────────────────

fn decode_jwt_payload(jwt: &str) -> Option<Value> {
    let mut parts = jwt.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn auth_claims(claims: &Value) -> Option<&Value> {
    claims.get("https://api.openai.com/auth")
}

/// 账号 ID 取值顺序：`chatgpt_account_id` → auth 声明内 `chatgpt_account_id` → `organizations[0].id`。
fn account_id_from_claims(claims: &Value) -> Option<String> {
    non_empty_str(claims.get("chatgpt_account_id"))
        .or_else(|| non_empty_str(auth_claims(claims).and_then(|a| a.get("chatgpt_account_id"))))
        .or_else(|| {
            non_empty_str(
                claims
                    .get("organizations")
                    .and_then(Value::as_array)
                    .and_then(|orgs| orgs.first())
                    .and_then(|org| org.get("id")),
            )
        })
}

/// 数据驻留区域；`no_constraint` 视为无约束。
fn residency_from_token(access_token: &str) -> Option<String> {
    let claims = decode_jwt_payload(access_token)?;
    non_empty_str(auth_claims(&claims).and_then(|a| a.get("chatgpt_compute_residency")))
        .or_else(|| non_empty_str(claims.get("chatgpt_compute_residency")))
        .filter(|value| value != "no_constraint")
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// 由令牌响应构造（或更新）凭据；账号 ID 优先取 id_token，其次 access token。
fn apply_tokens(previous: Option<CodexCredentials>, tokens: TokenResponse) -> CodexCredentials {
    let previous = previous.unwrap_or_default();
    let id_claims = tokens.id_token.as_deref().and_then(decode_jwt_payload);
    let access_claims = decode_jwt_payload(&tokens.access_token);
    let claim_sources = [id_claims.as_ref(), access_claims.as_ref()];
    let account_id = claim_sources
        .iter()
        .flatten()
        .find_map(|claims| account_id_from_claims(claims))
        .or(previous.account_id);
    let email = claim_sources
        .iter()
        .flatten()
        .find_map(|claims| {
            non_empty_str(claims.get("email")).or_else(|| {
                non_empty_str(
                    claims
                        .get("https://api.openai.com/profile")
                        .and_then(|p| p.get("email")),
                )
            })
        })
        .or(previous.email);
    let plan_type = claim_sources
        .iter()
        .flatten()
        .find_map(|claims| {
            non_empty_str(auth_claims(claims).and_then(|a| a.get("chatgpt_plan_type")))
        })
        .or(previous.plan_type);
    let expires_at = now_unix() + tokens.expires_in.unwrap_or(DEFAULT_EXPIRES_IN_SECS);
    CodexCredentials {
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .filter(|token| !token.trim().is_empty())
            .unwrap_or(previous.refresh_token),
        expires_at,
        account_id,
        email,
        plan_type,
        updated_at: Some(chrono::Local::now().naive_local().to_string()),
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

fn needs_refresh(creds: &CodexCredentials) -> bool {
    creds.expires_at - now_unix() < REFRESH_MARGIN_SECS
}

// ── 状态查询 / 登出 ────────────────────────────────────────

/// 当前登录状态（只读，不触发网络请求）。
pub async fn status() -> CodexAuthStatus {
    let login_pending = LOGIN
        .lock()
        .await
        .as_ref()
        .is_some_and(|task| !task.abort.is_finished());
    match load_credentials() {
        Ok(Some(creds)) => CodexAuthStatus {
            logged_in: true,
            email: creds.email,
            plan_type: creds.plan_type,
            account_id: creds.account_id,
            expires_at: Some(creds.expires_at),
            login_pending,
        },
        _ => CodexAuthStatus {
            login_pending,
            ..Default::default()
        },
    }
}

/// 退出登录：取消进行中的登录并删除本地凭据。
pub async fn logout() -> Result<CodexAuthStatus> {
    cancel_login().await;
    {
        let _guard = REFRESH_LOCK.lock().await;
        let path = credentials_path();
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("删除 Codex 凭据失败：{}", path.display()));
            }
        }
    }
    Ok(status().await)
}

// ── 访问令牌 / 刷新 ────────────────────────────────────────

/// 串行化刷新与写盘，避免并发请求重复刷新导致 refresh token 轮换冲突。
static REFRESH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn http_client() -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(crate::headers::TIANGONG_USER_AGENT)
        .build()
        .map_err(|err| LlmError::Transport(format!("创建 HTTP 客户端失败：{err}")))
}

fn not_logged_in() -> LlmError {
    LlmError::Authentication(
        "尚未登录 ChatGPT 账号，请在「设置 → 模型配置 → ChatGPT」中登录".to_string(),
    )
}

/// 获取可用的访问凭据；过期或 `force_refresh` 时先刷新。
pub async fn access(force_refresh: bool) -> Result<CodexAccess, LlmError> {
    let _guard = REFRESH_LOCK.lock().await;
    let mut creds = load_credentials()
        .map_err(|err| LlmError::Configuration(err.to_string()))?
        .ok_or_else(not_logged_in)?;
    if force_refresh || needs_refresh(&creds) {
        creds = refresh_credentials(creds).await?;
    }
    Ok(CodexAccess {
        residency: residency_from_token(&creds.access_token),
        access_token: creds.access_token,
        account_id: creds.account_id,
    })
}

async fn post_token_form(params: &[(&str, &str)]) -> Result<reqwest::Response, LlmError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params)
        .finish();
    http_client()?
        .post(format!("{ISSUER}/oauth/token"))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|err| LlmError::Transport(format!("请求 OpenAI 授权服务失败：{err}")))
}

async fn refresh_credentials(creds: CodexCredentials) -> Result<CodexCredentials, LlmError> {
    if creds.refresh_token.trim().is_empty() {
        return Err(LlmError::Authentication(
            "ChatGPT 登录已过期且缺少刷新令牌，请重新登录".to_string(),
        ));
    }
    let response = post_token_form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", creds.refresh_token.as_str()),
        ("client_id", CODEX_CLIENT_ID),
    ])
    .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let preview: String = body.chars().take(300).collect();
        tracing::warn!(status = status.as_u16(), body = %preview, "刷新 ChatGPT 登录失败");
        return Err(if status.is_client_error() {
            LlmError::Authentication(format!(
                "ChatGPT 登录已失效（{status}），请重新登录：{preview}"
            ))
        } else {
            LlmError::Transport(format!("刷新 ChatGPT 登录失败（{status}）：{preview}"))
        });
    }
    let tokens: TokenResponse = response
        .json()
        .await
        .map_err(|err| LlmError::Serialization(format!("解析刷新响应失败：{err}")))?;
    let creds = apply_tokens(Some(creds), tokens);
    save_credentials(&creds).map_err(|err| LlmError::Configuration(err.to_string()))?;
    tracing::info!(expires_at = creds.expires_at, "ChatGPT 登录已刷新");
    Ok(creds)
}

/// 用授权码换取令牌并保存凭据。
async fn exchange_code(code: &str, verifier: &str, redirect_uri: &str) -> Result<CodexAuthStatus> {
    let response = post_token_form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CODEX_CLIENT_ID),
        ("code_verifier", verifier),
    ])
    .await
    .map_err(|err| anyhow!("{err}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let preview: String = body.chars().take(300).collect();
        return Err(anyhow!("换取 ChatGPT 令牌失败（{status}）：{preview}"));
    }
    let tokens: TokenResponse = response.json().await.context("解析令牌响应失败")?;
    let creds = apply_tokens(None, tokens);
    {
        let _guard = REFRESH_LOCK.lock().await;
        save_credentials(&creds)?;
    }
    tracing::info!(account_id = ?creds.account_id, "ChatGPT 账号登录完成");
    Ok(CodexAuthStatus {
        logged_in: true,
        email: creds.email,
        plan_type: creds.plan_type,
        account_id: creds.account_id,
        expires_at: Some(creds.expires_at),
        login_pending: false,
    })
}

// ── 登录任务管理 ───────────────────────────────────────────

type LoginOutcome = Option<Result<CodexAuthStatus, String>>;

struct LoginTask {
    abort: tokio::task::AbortHandle,
    result: watch::Receiver<LoginOutcome>,
}

static LOGIN: LazyLock<Mutex<Option<LoginTask>>> = LazyLock::new(|| Mutex::new(None));

/// 在后台运行登录流程，结果写入 watch 通道供 [`wait_login`] 读取。
fn spawn_login_task<F>(timeout: Duration, flow: F) -> LoginTask
where
    F: std::future::Future<Output = Result<CodexAuthStatus>> + Send + 'static,
{
    let (tx, rx) = watch::channel(None);
    let handle = tokio::spawn(async move {
        let outcome = match tokio::time::timeout(timeout, flow).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(err)) => Err(format!("{err:#}")),
            Err(_) => Err("登录超时，请重新发起".to_string()),
        };
        if let Err(err) = &outcome {
            tracing::warn!(error = %err, "ChatGPT 账号登录未完成");
        }
        let _ = tx.send(Some(outcome));
    });
    LoginTask {
        abort: handle.abort_handle(),
        result: rx,
    }
}

/// 等待进行中的登录完成。
pub async fn wait_login() -> Result<CodexAuthStatus> {
    let mut rx = LOGIN
        .lock()
        .await
        .as_ref()
        .map(|task| task.result.clone())
        .ok_or_else(|| anyhow!("没有进行中的 ChatGPT 登录"))?;
    let outcome = rx
        .wait_for(Option::is_some)
        .await
        .map_err(|_| anyhow!("ChatGPT 登录已取消"))?
        .clone()
        .unwrap_or_else(|| Err("ChatGPT 登录已取消".to_string()));
    outcome.map_err(|err| anyhow!(err))
}

/// 取消进行中的登录（释放回调端口）。
pub async fn cancel_login() {
    if let Some(task) = LOGIN.lock().await.take() {
        task.abort.abort();
    }
}

// ── 浏览器登录（PKCE） ─────────────────────────────────────

fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|err| anyhow!("生成随机数失败：{err}"))?;
    Ok(bytes)
}

/// PKCE code_verifier：43 位 unreserved 字符（RFC 7636）。
fn generate_verifier() -> Result<String> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    Ok(random_bytes(43)?
        .into_iter()
        .map(|b| CHARS[b as usize % CHARS.len()] as char)
        .collect())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn build_authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> Result<String> {
    let url = url::Url::parse_with_params(
        &format!("{ISSUER}/oauth/authorize"),
        [
            ("response_type", "code"),
            ("client_id", CODEX_CLIENT_ID),
            ("redirect_uri", redirect_uri),
            ("scope", OAUTH_SCOPE),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", state),
            ("originator", CODEX_ORIGINATOR),
        ],
    )
    .context("构建授权地址失败")?;
    Ok(url.into())
}

/// 请求占用 1455 端口的其他登录服务（Codex CLI / opencode 同样支持 `/cancel`）退出。
async fn request_foreign_cancel() {
    let Ok(Ok(mut stream)) = tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", OAUTH_PORT)),
    )
    .await
    else {
        return;
    };
    let request = format!(
        "GET /cancel HTTP/1.1\r\nHost: localhost:{OAUTH_PORT}\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(request.as_bytes()).await;
    let mut buf = [0u8; 64];
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
}

async fn bind_callback_listener() -> Result<TcpListener> {
    for attempt in 0..10 {
        match TcpListener::bind(("127.0.0.1", OAUTH_PORT)).await {
            Ok(listener) => return Ok(listener),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                if attempt == 0 {
                    request_foreign_cancel().await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(err) => return Err(err).context("启动本地登录回调服务失败"),
        }
    }
    Err(anyhow!(
        "本地回调端口 {OAUTH_PORT} 被占用（可能有其他 Codex 登录正在进行），请关闭后重试"
    ))
}

/// 发起浏览器登录：启动本地回调服务并返回授权地址（由调用方打开浏览器）。
///
/// 重复调用会取消上一次未完成的登录。
pub async fn start_browser_login() -> Result<CodexLoginStart> {
    let mut guard = LOGIN.lock().await;
    if let Some(previous) = guard.take() {
        previous.abort.abort();
        // 等待上一任务释放监听端口。
        tokio::task::yield_now().await;
    }
    let listener = bind_callback_listener().await?;
    let verifier = generate_verifier()?;
    let state = URL_SAFE_NO_PAD.encode(random_bytes(32)?);
    let redirect_uri = format!("http://localhost:{OAUTH_PORT}{CALLBACK_PATH}");
    let url = build_authorize_url(&redirect_uri, &pkce_challenge(&verifier), &state)?;
    *guard = Some(spawn_login_task(
        LOGIN_TIMEOUT,
        serve_login_callback(listener, state, verifier, redirect_uri),
    ));
    Ok(CodexLoginStart {
        url,
        user_code: None,
    })
}

struct CallbackRequest {
    stream: TcpStream,
    path: String,
    query: Vec<(String, String)>,
}

async fn read_callback_request(mut stream: TcpStream) -> Option<CallbackRequest> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let read = async {
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
                break;
            }
        }
        Some(())
    };
    tokio::time::timeout(REQUEST_READ_TIMEOUT, read)
        .await
        .ok()
        .flatten()?;
    let head = String::from_utf8_lossy(&buf);
    let target = head.lines().next()?.split_whitespace().nth(1)?.to_string();
    let url = url::Url::parse(&format!("http://localhost{target}")).ok()?;
    Some(CallbackRequest {
        path: url.path().to_string(),
        query: url.query_pairs().into_owned().collect(),
        stream,
    })
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn respond_html(mut stream: TcpStream, status: &str, title: &str, message: &str) {
    let body = format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><title>{title}</title>\
         <style>body{{font-family:-apple-system,BlinkMacSystemFont,'PingFang SC',sans-serif;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#f7f7f8}}\
         .card{{background:#fff;padding:32px 40px;border-radius:12px;box-shadow:0 2px 12px rgba(0,0,0,.08);\
         text-align:center}}h1{{font-size:20px;margin:0 0 8px}}p{{color:#666;margin:0}}</style></head>\
         <body><div class=\"card\"><h1>{title}</h1><p>{message}</p></div></body></html>",
        title = html_escape(title),
        message = html_escape(message),
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn query_value<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

async fn serve_login_callback(
    listener: TcpListener,
    state: String,
    verifier: String,
    redirect_uri: String,
) -> Result<CodexAuthStatus> {
    // 每个连接独立读取，避免浏览器预连接等空连接阻塞真正的回调请求。
    let (tx, mut rx) = mpsc::channel::<CallbackRequest>(8);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("接收登录回调连接失败")?;
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Some(request) = read_callback_request(stream).await {
                        let _ = tx.send(request).await;
                    }
                });
            }
            Some(request) = rx.recv() => {
                if request.path == "/cancel" {
                    respond_html(request.stream, "200 OK", "已取消", "登录已取消。").await;
                    return Err(anyhow!("ChatGPT 登录已取消"));
                }
                if request.path != CALLBACK_PATH {
                    respond_html(request.stream, "404 Not Found", "Not Found", "").await;
                    continue;
                }
                if let Some(error) = query_value(&request.query, "error") {
                    let detail = query_value(&request.query, "error_description").unwrap_or(error);
                    respond_html(request.stream, "200 OK", "登录失败", detail).await;
                    return Err(anyhow!("授权失败：{detail}"));
                }
                let Some(code) = query_value(&request.query, "code").map(str::to_string) else {
                    respond_html(request.stream, "400 Bad Request", "登录失败", "缺少授权码。").await;
                    return Err(anyhow!("登录回调缺少授权码"));
                };
                if query_value(&request.query, "state") != Some(state.as_str()) {
                    respond_html(request.stream, "400 Bad Request", "登录失败", "state 校验失败，请重新发起登录。").await;
                    return Err(anyhow!("登录回调 state 校验失败"));
                }
                return match exchange_code(&code, &verifier, &redirect_uri).await {
                    Ok(status) => {
                        respond_html(request.stream, "200 OK", "登录成功", "已完成 ChatGPT 账号授权，可以关闭此页面返回天工。").await;
                        Ok(status)
                    }
                    Err(err) => {
                        respond_html(request.stream, "200 OK", "登录失败", &format!("{err:#}")).await;
                        Err(err)
                    }
                };
            }
        }
    }
}

// ── 设备码登录（headless） ─────────────────────────────────

#[derive(Deserialize)]
struct DeviceUserCode {
    device_auth_id: String,
    user_code: String,
    #[serde(default)]
    interval: Value,
}

#[derive(Deserialize)]
struct DeviceTokenGrant {
    authorization_code: String,
    code_verifier: String,
}

/// 发起设备码登录：返回验证地址与验证码，后台轮询授权结果。
///
/// 重复调用会取消上一次未完成的登录。
pub async fn start_device_login() -> Result<CodexLoginStart> {
    let mut guard = LOGIN.lock().await;
    if let Some(previous) = guard.take() {
        previous.abort.abort();
    }
    let client = http_client().map_err(|err| anyhow!("{err}"))?;
    let response = client
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .json(&serde_json::json!({ "client_id": CODEX_CLIENT_ID }))
        .send()
        .await
        .context("请求设备码失败")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let preview: String = body.chars().take(300).collect();
        return Err(anyhow!("请求设备码失败（{status}）：{preview}"));
    }
    let device: DeviceUserCode = response.json().await.context("解析设备码响应失败")?;
    let interval_secs = match &device.interval {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .unwrap_or(5)
    .max(1);
    let user_code = device.user_code.clone();
    *guard = Some(spawn_login_task(
        DEVICE_LOGIN_TIMEOUT,
        poll_device_login(
            client,
            device,
            Duration::from_secs(interval_secs) + DEVICE_POLL_SAFETY_MARGIN,
        ),
    ));
    Ok(CodexLoginStart {
        url: format!("{ISSUER}/codex/device"),
        user_code: Some(user_code),
    })
}

async fn poll_device_login(
    client: reqwest::Client,
    device: DeviceUserCode,
    interval: Duration,
) -> Result<CodexAuthStatus> {
    loop {
        let response = client
            .post(format!("{ISSUER}/api/accounts/deviceauth/token"))
            .json(&serde_json::json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                let grant: DeviceTokenGrant =
                    response.json().await.context("解析设备码授权结果失败")?;
                return exchange_code(
                    &grant.authorization_code,
                    &grant.code_verifier,
                    &format!("{ISSUER}/deviceauth/callback"),
                )
                .await;
            }
            // 403 / 404 表示用户尚未完成授权，继续轮询。
            Ok(response)
                if matches!(
                    response.status(),
                    reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND
                ) => {}
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let preview: String = body.chars().take(300).collect();
                return Err(anyhow!("设备码授权失败（{status}）：{preview}"));
            }
            Err(err) => tracing::warn!(error = %err, "轮询设备码授权失败，稍后重试"),
        }
        tokio::time::sleep(interval).await;
    }
}

// ── 请求适配 ───────────────────────────────────────────────

/// 按 Codex 推理后端约束改写 Responses 请求体（对齐 Codex CLI / opencode）：
/// - 必须 `stream=true`、`store=false`；
/// - 不支持 `max_output_tokens` / `temperature` / `top_p`；
/// - 函数工具统一 `strict=false`，兼容不满足结构化输出约束的动态 schema。
pub(crate) fn adapt_payload(payload: &mut Value) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    for key in ["max_output_tokens", "temperature", "top_p"] {
        object.remove(key);
    }
    object.insert("store".to_string(), Value::Bool(false));
    object.insert("stream".to_string(), Value::Bool(true));
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) == Some("function")
                && let Some(tool) = tool.as_object_mut()
            {
                tool.insert("strict".to_string(), Value::Bool(false));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    #[test]
    fn apply_tokens_reads_claims_and_expiry() {
        let tokens = TokenResponse {
            access_token: jwt(serde_json::json!({
                "https://api.openai.com/auth": { "chatgpt_compute_residency": "us" }
            })),
            refresh_token: Some("r1".to_string()),
            id_token: Some(jwt(serde_json::json!({
                "email": "user@example.com",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-1",
                    "chatgpt_plan_type": "plus"
                }
            }))),
            expires_in: Some(7200),
        };
        let creds = apply_tokens(None, tokens);
        assert_eq!(creds.account_id.as_deref(), Some("acct-1"));
        assert_eq!(creds.email.as_deref(), Some("user@example.com"));
        assert_eq!(creds.plan_type.as_deref(), Some("plus"));
        assert_eq!(creds.refresh_token, "r1");
        assert!((creds.expires_at - now_unix() - 7200).abs() <= 2);
        assert_eq!(
            residency_from_token(&creds.access_token).as_deref(),
            Some("us")
        );
    }

    #[test]
    fn refresh_keeps_previous_refresh_token_and_account() {
        let previous = CodexCredentials {
            access_token: "old".to_string(),
            refresh_token: "r-old".to_string(),
            account_id: Some("acct-old".to_string()),
            ..Default::default()
        };
        let creds = apply_tokens(
            Some(previous),
            TokenResponse {
                access_token: "not-a-jwt".to_string(),
                refresh_token: None,
                id_token: None,
                expires_in: None,
            },
        );
        assert_eq!(creds.refresh_token, "r-old");
        assert_eq!(creds.account_id.as_deref(), Some("acct-old"));
        assert!((creds.expires_at - now_unix() - DEFAULT_EXPIRES_IN_SECS).abs() <= 2);
    }

    #[test]
    fn account_id_falls_back_to_organizations() {
        let claims = serde_json::json!({ "organizations": [{ "id": "org-1" }] });
        assert_eq!(account_id_from_claims(&claims).as_deref(), Some("org-1"));
        let top = serde_json::json!({ "chatgpt_account_id": "acct-top", "organizations": [{ "id": "org-1" }] });
        assert_eq!(account_id_from_claims(&top).as_deref(), Some("acct-top"));
    }

    #[test]
    fn residency_no_constraint_is_ignored() {
        let token = jwt(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_compute_residency": "no_constraint" }
        }));
        assert_eq!(residency_from_token(&token), None);
    }

    #[test]
    fn needs_refresh_near_expiry() {
        let mut creds = CodexCredentials {
            expires_at: now_unix() + 3600,
            ..Default::default()
        };
        assert!(!needs_refresh(&creds));
        creds.expires_at = now_unix() + 10;
        assert!(needs_refresh(&creds));
    }

    #[test]
    fn adapt_payload_strips_unsupported_fields_and_relaxes_tools() {
        let mut payload = serde_json::json!({
            "model": "gpt-5.5",
            "max_output_tokens": 100,
            "temperature": 0.2,
            "top_p": 0.9,
            "prompt_cache_key": "s1",
            "tools": [{ "type": "function", "name": "t", "parameters": {} }]
        });
        adapt_payload(&mut payload);
        assert_eq!(
            payload,
            serde_json::json!({
                "model": "gpt-5.5",
                "prompt_cache_key": "s1",
                "store": false,
                "stream": true,
                "tools": [{ "type": "function", "name": "t", "parameters": {}, "strict": false }]
            })
        );
    }

    #[test]
    fn pkce_and_authorize_url() {
        // RFC 7636 附录 B 的已知向量。
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let verifier = generate_verifier().unwrap();
        assert_eq!(verifier.len(), 43);
        let url = build_authorize_url(
            "http://localhost:1455/auth/callback",
            &pkce_challenge(&verifier),
            "state-1",
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.path(), "/oauth/authorize");
        assert_eq!(pairs["client_id"], CODEX_CLIENT_ID);
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["originator"], CODEX_ORIGINATOR);
        assert_eq!(pairs["state"], "state-1");
    }
}
