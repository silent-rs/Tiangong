//! 通行密钥桥接的平台无关逻辑：页面消息解析、来源与 RP ID 校验、
//! base64url 编解码、attestationObject（CBOR）解析与响应 JSON 组装。
//!
//! 原生调用（AuthenticationServices）只在 macOS 侧；这里保持纯函数，
//! 便于在任意平台单测。
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

/// COSE 算法 ES256（Apple 平台通行密钥唯一支持的算法）。
pub const COSE_ALG_ES256: i64 = -7;

/// user.id 最大长度（WebAuthn 规范上限 64 字节）。
const MAX_USER_ID_LEN: usize = 64;

/// CBOR 嵌套深度上限，防御恶意/畸形数据。
const MAX_CBOR_DEPTH: u8 = 16;

/// P-256 公钥 SubjectPublicKeyInfo 的 DER 固定前缀（后接 0x04||X||Y）。
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// 回传给页面的错误，`name` 对应 DOMException 名称。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasskeyError {
    NotAllowed(String),
    Security(String),
    NotSupported(String),
    InvalidState(String),
    Type(String),
    Unknown(String),
}

impl PasskeyError {
    pub fn dom_name(&self) -> &'static str {
        match self {
            Self::NotAllowed(_) => "NotAllowedError",
            Self::Security(_) => "SecurityError",
            Self::NotSupported(_) => "NotSupportedError",
            Self::InvalidState(_) => "InvalidStateError",
            Self::Type(_) => "TypeError",
            Self::Unknown(_) => "UnknownError",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::NotAllowed(m)
            | Self::Security(m)
            | Self::NotSupported(m)
            | Self::InvalidState(m)
            | Self::Type(m)
            | Self::Unknown(m) => m,
        }
    }

    /// 经 replyHandler 回传的错误串，页面侧按 `Name: message` 还原 DOMException。
    pub fn to_reply(&self) -> String {
        format!("{}: {}", self.dom_name(), self.message())
    }
}

// ── 页面消息 ──

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum IncomingMessage {
    Create {
        request_id: String,
        options: CreateOptions,
    },
    Get {
        request_id: String,
        options: GetOptions,
    },
    Cancel {
        request_id: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOptions {
    #[serde(default)]
    pub rp: RpEntity,
    pub user: UserEntity,
    pub challenge: String,
    #[serde(default)]
    pub pub_key_cred_params: Vec<CredParam>,
    #[serde(default)]
    pub exclude_credentials: Vec<CredDescriptor>,
    #[serde(default)]
    pub authenticator_selection: Option<AuthenticatorSelection>,
    #[serde(default)]
    pub attestation: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpEntity {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserEntity {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub struct CredParam {
    #[serde(default, rename = "type")]
    pub kind: String,
    pub alg: i64,
}

#[derive(Debug, Deserialize)]
pub struct CredDescriptor {
    #[serde(default, rename = "type")]
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticatorSelection {
    #[serde(default)]
    pub authenticator_attachment: Option<String>,
    #[serde(default)]
    pub user_verification: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetOptions {
    pub challenge: String,
    #[serde(default)]
    pub rp_id: Option<String>,
    #[serde(default)]
    pub allow_credentials: Vec<CredDescriptor>,
    #[serde(default)]
    pub user_verification: Option<String>,
}

pub fn parse_message(text: &str) -> Result<IncomingMessage, PasskeyError> {
    serde_json::from_str(text).map_err(|e| PasskeyError::Type(format!("无效的请求参数：{e}")))
}

// ── 校验后的请求 ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserVerification {
    Required,
    Preferred,
    Discouraged,
}

impl UserVerification {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("required") => Self::Required,
            Some("discouraged") => Self::Discouraged,
            _ => Self::Preferred,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attestation {
    None,
    Indirect,
    Direct,
}

impl Attestation {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("direct") => Self::Direct,
            Some("indirect") => Self::Indirect,
            // enterprise 只对受管设备有意义，按 none 处理
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCreate {
    pub user_id: Vec<u8>,
    pub user_name: String,
    pub display_name: String,
    pub exclude_credentials: Vec<Vec<u8>>,
    pub user_verification: UserVerification,
    pub attestation: Attestation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedGet {
    pub allow_credentials: Vec<Vec<u8>>,
    pub user_verification: UserVerification,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedKind {
    Create(PreparedCreate),
    Get(PreparedGet),
}

/// 已通过来源与参数校验、可直接交给系统接口的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRequest {
    pub origin: String,
    pub rp_id: String,
    pub challenge: Vec<u8>,
    pub kind: PreparedKind,
}

/// 发起请求的页面来源（来自 WebKit 的 frameInfo.securityOrigin，不信任页面自报）。
#[derive(Debug, Clone, Copy)]
pub struct FrameOrigin<'a> {
    pub scheme: &'a str,
    pub host: &'a str,
    /// 0 表示协议默认端口
    pub port: u16,
}

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost")
}

fn is_ip_literal(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::Ipv6Addr>()
            .is_ok()
}

/// 按 WebAuthn 安全上下文要求构造 origin 串：仅 https，或本机 http。
pub fn build_origin(frame: FrameOrigin<'_>) -> Result<String, PasskeyError> {
    let scheme = frame.scheme.to_ascii_lowercase();
    let host = frame.host.to_ascii_lowercase();
    if host.is_empty() {
        return Err(PasskeyError::Security("页面来源无效".into()));
    }
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" if is_localhost(&host) => 80,
        _ => {
            return Err(PasskeyError::Security(
                "通行密钥仅支持 HTTPS 页面".to_string(),
            ));
        }
    };
    if frame.port == 0 || frame.port == default_port {
        Ok(format!("{scheme}://{host}"))
    } else {
        Ok(format!("{scheme}://{host}:{}", frame.port))
    }
}

/// 校验并返回生效的 RP ID：缺省为页面主机；显式指定时必须是主机本身或其
/// 可注册父域（不能是公共后缀）。
pub fn resolve_rp_id(host: &str, requested: Option<&str>) -> Result<String, PasskeyError> {
    let host = host.to_ascii_lowercase();
    let Some(requested) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(host);
    };
    let rp_id = requested.to_ascii_lowercase();
    if rp_id == host {
        return Ok(rp_id);
    }
    let not_allowed = || PasskeyError::Security(format!("RP ID {rp_id} 与页面来源 {host} 不匹配"));
    if is_ip_literal(&host) || is_ip_literal(&rp_id) {
        return Err(not_allowed());
    }
    if !host.ends_with(&format!(".{rp_id}")) {
        return Err(not_allowed());
    }
    if psl::suffix_str(&rp_id) == Some(rp_id.as_str()) {
        return Err(not_allowed());
    }
    Ok(rp_id)
}

pub fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn b64url_decode(value: &str, field: &str) -> Result<Vec<u8>, PasskeyError> {
    URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .map_err(|_| PasskeyError::Type(format!("{field} 不是有效的 base64url")))
}

fn decode_challenge(value: &str) -> Result<Vec<u8>, PasskeyError> {
    let challenge = b64url_decode(value, "challenge")?;
    if challenge.is_empty() {
        return Err(PasskeyError::Type("challenge 不能为空".into()));
    }
    Ok(challenge)
}

fn decode_descriptors(list: &[CredDescriptor], field: &str) -> Result<Vec<Vec<u8>>, PasskeyError> {
    list.iter()
        .filter(|d| d.kind.is_empty() || d.kind == "public-key")
        .map(|d| b64url_decode(&d.id, field))
        .collect()
}

pub fn prepare_create(
    frame: FrameOrigin<'_>,
    options: &CreateOptions,
) -> Result<PreparedRequest, PasskeyError> {
    let origin = build_origin(frame)?;
    let rp_id = resolve_rp_id(frame.host, options.rp.id.as_deref())?;
    let challenge = decode_challenge(&options.challenge)?;
    let user_id = b64url_decode(&options.user.id, "user.id")?;
    if user_id.is_empty() || user_id.len() > MAX_USER_ID_LEN {
        return Err(PasskeyError::Type("user.id 长度必须在 1~64 字节".into()));
    }
    let supports_es256 = options.pub_key_cred_params.is_empty()
        || options
            .pub_key_cred_params
            .iter()
            .any(|p| (p.kind.is_empty() || p.kind == "public-key") && p.alg == COSE_ALG_ES256);
    if !supports_es256 {
        return Err(PasskeyError::NotSupported(
            "仅支持 ES256（-7）算法的通行密钥".into(),
        ));
    }
    let selection = options.authenticator_selection.as_ref();
    if selection.and_then(|s| s.authenticator_attachment.as_deref()) == Some("cross-platform") {
        return Err(PasskeyError::NotSupported(
            "内置浏览器暂不支持外部安全密钥".into(),
        ));
    }
    Ok(PreparedRequest {
        origin,
        rp_id,
        challenge,
        kind: PreparedKind::Create(PreparedCreate {
            user_id,
            user_name: options.user.name.clone(),
            display_name: options.user.display_name.clone(),
            exclude_credentials: decode_descriptors(
                &options.exclude_credentials,
                "excludeCredentials.id",
            )?,
            user_verification: UserVerification::parse(
                selection.and_then(|s| s.user_verification.as_deref()),
            ),
            attestation: Attestation::parse(options.attestation.as_deref()),
        }),
    })
}

pub fn prepare_get(
    frame: FrameOrigin<'_>,
    options: &GetOptions,
) -> Result<PreparedRequest, PasskeyError> {
    let origin = build_origin(frame)?;
    let rp_id = resolve_rp_id(frame.host, options.rp_id.as_deref())?;
    let challenge = decode_challenge(&options.challenge)?;
    Ok(PreparedRequest {
        origin,
        rp_id,
        challenge,
        kind: PreparedKind::Get(PreparedGet {
            allow_credentials: decode_descriptors(
                &options.allow_credentials,
                "allowCredentials.id",
            )?,
            user_verification: UserVerification::parse(options.user_verification.as_deref()),
        }),
    })
}

// ── 最小 CBOR 解码（只用于读取 attestationObject 与 COSE 公钥）──

#[derive(Debug, Clone, PartialEq)]
enum Cbor {
    Uint(u64),
    Nint(i128),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Simple,
}

fn cbor_err(msg: &str) -> PasskeyError {
    PasskeyError::Unknown(format!("系统返回的凭据数据无效：{msg}"))
}

fn take<'a>(data: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8], PasskeyError> {
    let end = pos
        .checked_add(len)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| cbor_err("数据截断"))?;
    let slice = &data[*pos..end];
    *pos = end;
    Ok(slice)
}

fn read_arg(data: &[u8], pos: &mut usize, info: u8) -> Result<u64, PasskeyError> {
    let len = match info {
        0..=23 => return Ok(u64::from(info)),
        24 => 1,
        25 => 2,
        26 => 4,
        27 => 8,
        _ => return Err(cbor_err("不支持的长度编码")),
    };
    Ok(take(data, pos, len)?
        .iter()
        .fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

fn decode_cbor(data: &[u8], pos: &mut usize, depth: u8) -> Result<Cbor, PasskeyError> {
    if depth > MAX_CBOR_DEPTH {
        return Err(cbor_err("嵌套过深"));
    }
    let head = *take(data, pos, 1)?
        .first()
        .ok_or_else(|| cbor_err("数据截断"))?;
    let major = head >> 5;
    let info = head & 0x1f;
    if major == 7 {
        return match info {
            20..=23 => Ok(Cbor::Simple),
            25 => take(data, pos, 2).map(|_| Cbor::Simple),
            26 => take(data, pos, 4).map(|_| Cbor::Simple),
            27 => take(data, pos, 8).map(|_| Cbor::Simple),
            _ => Err(cbor_err("不支持的简单值")),
        };
    }
    let arg = read_arg(data, pos, info)?;
    let remaining = (data.len() - *pos) as u64;
    match major {
        0 => Ok(Cbor::Uint(arg)),
        1 => Ok(Cbor::Nint(-1 - i128::from(arg))),
        2 | 3 => {
            if arg > remaining {
                return Err(cbor_err("数据截断"));
            }
            let bytes = take(data, pos, arg as usize)?.to_vec();
            if major == 2 {
                Ok(Cbor::Bytes(bytes))
            } else {
                String::from_utf8(bytes)
                    .map(Cbor::Text)
                    .map_err(|_| cbor_err("文本不是 UTF-8"))
            }
        }
        4 => {
            if arg > remaining {
                return Err(cbor_err("数据截断"));
            }
            (0..arg)
                .map(|_| decode_cbor(data, pos, depth + 1))
                .collect::<Result<Vec<_>, _>>()
                .map(Cbor::Array)
        }
        5 => {
            if arg > remaining {
                return Err(cbor_err("数据截断"));
            }
            let mut entries = Vec::with_capacity(arg as usize);
            for _ in 0..arg {
                let key = decode_cbor(data, pos, depth + 1)?;
                let value = decode_cbor(data, pos, depth + 1)?;
                entries.push((key, value));
            }
            Ok(Cbor::Map(entries))
        }
        // tag：跳过标签号，解析被标注的值
        6 => decode_cbor(data, pos, depth + 1),
        _ => Err(cbor_err("未知类型")),
    }
}

fn map_get<'a>(entries: &'a [(Cbor, Cbor)], key: &Cbor) -> Option<&'a Cbor> {
    entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// 从 attestationObject 中提取的注册信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationInfo {
    pub authenticator_data: Vec<u8>,
    pub public_key_spki: Option<Vec<u8>>,
    pub public_key_algorithm: i64,
}

pub fn parse_attestation_object(attestation: &[u8]) -> Result<RegistrationInfo, PasskeyError> {
    let mut pos = 0;
    let Cbor::Map(entries) = decode_cbor(attestation, &mut pos, 0)? else {
        return Err(cbor_err("attestationObject 不是 map"));
    };
    let Some(Cbor::Bytes(auth_data)) = map_get(&entries, &Cbor::Text("authData".into())) else {
        return Err(cbor_err("缺少 authData"));
    };
    let (public_key_spki, public_key_algorithm) = parse_credential_public_key(auth_data)?;
    Ok(RegistrationInfo {
        authenticator_data: auth_data.clone(),
        public_key_spki,
        public_key_algorithm,
    })
}

/// authenticatorData：rpIdHash(32) flags(1) signCount(4) [aaguid(16) credIdLen(2) credId COSEKey]
fn parse_credential_public_key(auth_data: &[u8]) -> Result<(Option<Vec<u8>>, i64), PasskeyError> {
    const ATTESTED_DATA_FLAG: u8 = 0x40;
    let flags = *auth_data.get(32).ok_or_else(|| cbor_err("authData 过短"))?;
    if flags & ATTESTED_DATA_FLAG == 0 {
        return Err(cbor_err("authData 缺少凭据数据"));
    }
    let mut pos = 37 + 16;
    let len_bytes = take(auth_data, &mut pos, 2)?;
    let cred_len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
    take(auth_data, &mut pos, cred_len)?;
    let Cbor::Map(cose) = decode_cbor(auth_data, &mut pos, 0)? else {
        return Err(cbor_err("公钥不是 COSE map"));
    };
    let algorithm = match map_get(&cose, &Cbor::Uint(3)) {
        Some(Cbor::Nint(v)) => i64::try_from(*v).map_err(|_| cbor_err("算法值越界"))?,
        Some(Cbor::Uint(v)) => i64::try_from(*v).map_err(|_| cbor_err("算法值越界"))?,
        _ => return Err(cbor_err("公钥缺少算法")),
    };
    let is_p256 = map_get(&cose, &Cbor::Uint(1)) == Some(&Cbor::Uint(2))
        && map_get(&cose, &Cbor::Nint(-1)) == Some(&Cbor::Uint(1));
    let spki = match (
        is_p256,
        map_get(&cose, &Cbor::Nint(-2)),
        map_get(&cose, &Cbor::Nint(-3)),
    ) {
        (true, Some(Cbor::Bytes(x)), Some(Cbor::Bytes(y))) if x.len() == 32 && y.len() == 32 => {
            let mut der = Vec::with_capacity(P256_SPKI_PREFIX.len() + 65);
            der.extend_from_slice(&P256_SPKI_PREFIX);
            der.push(0x04);
            der.extend_from_slice(x);
            der.extend_from_slice(y);
            Some(der)
        }
        _ => None,
    };
    Ok((spki, algorithm))
}

// ── 响应 JSON（字段与 WebAuthn L3 toJSON 结构一致，二进制均为 base64url）──

pub fn registration_response(
    credential_id: &[u8],
    client_data_json: &[u8],
    attestation_object: &[u8],
) -> Result<Value, PasskeyError> {
    let info = parse_attestation_object(attestation_object)?;
    let id = b64url_encode(credential_id);
    Ok(json!({
        "id": id,
        "rawId": id,
        "type": "public-key",
        "authenticatorAttachment": "platform",
        "response": {
            "clientDataJSON": b64url_encode(client_data_json),
            "attestationObject": b64url_encode(attestation_object),
            "authenticatorData": b64url_encode(&info.authenticator_data),
            "transports": ["hybrid", "internal"],
            "publicKey": info.public_key_spki.as_deref().map(b64url_encode),
            "publicKeyAlgorithm": info.public_key_algorithm,
        },
        "clientExtensionResults": {},
    }))
}

pub fn assertion_response(
    credential_id: &[u8],
    client_data_json: &[u8],
    authenticator_data: &[u8],
    signature: &[u8],
    user_handle: &[u8],
) -> Value {
    let id = b64url_encode(credential_id);
    json!({
        "id": id,
        "rawId": id,
        "type": "public-key",
        "authenticatorAttachment": "platform",
        "response": {
            "clientDataJSON": b64url_encode(client_data_json),
            "authenticatorData": b64url_encode(authenticator_data),
            "signature": b64url_encode(signature),
            "userHandle": (!user_handle.is_empty()).then(|| b64url_encode(user_handle)),
        },
        "clientExtensionResults": {},
    })
}

/// ASAuthorizationError 错误码 → 页面错误。
pub fn map_authorization_error(code: isize) -> PasskeyError {
    match code {
        1001 => PasskeyError::NotAllowed("用户取消了操作".into()),
        1006 => PasskeyError::InvalidState("该设备上已存在此账号的通行密钥".into()),
        1010 => PasskeyError::NotAllowed("本机尚未设置通行密钥（需开启 iCloud 钥匙串）".into()),
        other => PasskeyError::NotAllowed(format!("系统未能完成通行密钥操作（错误码 {other}）")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(host: &str) -> FrameOrigin<'_> {
        FrameOrigin {
            scheme: "https",
            host,
            port: 0,
        }
    }

    // ── 测试用最小 CBOR 编码 ──
    fn head(major: u8, arg: u64) -> Vec<u8> {
        let m = major << 5;
        match arg {
            0..=23 => vec![m | arg as u8],
            24..=0xff => vec![m | 24, arg as u8],
            _ => {
                let mut v = vec![m | 25];
                v.extend_from_slice(&(arg as u16).to_be_bytes());
                v
            }
        }
    }
    fn uint(v: u64) -> Vec<u8> {
        head(0, v)
    }
    fn nint(v: i64) -> Vec<u8> {
        head(1, (-1 - v) as u64)
    }
    fn bytes(b: &[u8]) -> Vec<u8> {
        let mut v = head(2, b.len() as u64);
        v.extend_from_slice(b);
        v
    }
    fn text(s: &str) -> Vec<u8> {
        let mut v = head(3, s.len() as u64);
        v.extend_from_slice(s.as_bytes());
        v
    }
    fn map(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut v = head(5, entries.len() as u64);
        for (k, val) in entries {
            v.extend_from_slice(k);
            v.extend_from_slice(val);
        }
        v
    }

    fn cose_es256(x: &[u8; 32], y: &[u8; 32]) -> Vec<u8> {
        map(&[
            (uint(1), uint(2)),
            (uint(3), nint(-7)),
            (nint(-1), uint(1)),
            (nint(-2), bytes(x)),
            (nint(-3), bytes(y)),
        ])
    }

    fn auth_data(cose: &[u8], cred_id: &[u8]) -> Vec<u8> {
        let mut v = vec![0xAA; 32];
        v.push(0x45); // UP | UV | AT
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(&[0; 16]);
        v.extend_from_slice(&(cred_id.len() as u16).to_be_bytes());
        v.extend_from_slice(cred_id);
        v.extend_from_slice(cose);
        v
    }

    fn attestation(auth: &[u8]) -> Vec<u8> {
        map(&[
            (text("fmt"), text("none")),
            (text("attStmt"), map(&[])),
            (text("authData"), bytes(auth)),
        ])
    }

    #[test]
    fn rp_id_defaults_to_host() {
        assert_eq!(
            resolve_rp_id("Login.Example.com", None).unwrap(),
            "login.example.com"
        );
        assert_eq!(
            resolve_rp_id("example.com", Some("  ")).unwrap(),
            "example.com"
        );
    }

    #[test]
    fn rp_id_allows_registrable_parent() {
        assert_eq!(
            resolve_rp_id("login.example.com", Some("example.com")).unwrap(),
            "example.com"
        );
        assert_eq!(
            resolve_rp_id("a.b.example.co.uk", Some("example.co.uk")).unwrap(),
            "example.co.uk"
        );
    }

    #[test]
    fn rp_id_rejects_foreign_or_public_suffix() {
        assert!(resolve_rp_id("evil.com", Some("example.com")).is_err());
        assert!(resolve_rp_id("notexample.com", Some("example.com")).is_err());
        assert!(resolve_rp_id("example.com", Some("com")).is_err());
        assert!(resolve_rp_id("example.co.uk", Some("co.uk")).is_err());
        assert!(resolve_rp_id("login.example.com", Some("other.example.com")).is_err());
        assert!(resolve_rp_id("10.0.0.1", Some("0.0.1")).is_err());
    }

    #[test]
    fn origin_requires_secure_context() {
        assert_eq!(
            build_origin(frame("example.com")).unwrap(),
            "https://example.com"
        );
        assert_eq!(
            build_origin(FrameOrigin {
                scheme: "https",
                host: "example.com",
                port: 8443
            })
            .unwrap(),
            "https://example.com:8443"
        );
        assert_eq!(
            build_origin(FrameOrigin {
                scheme: "http",
                host: "localhost",
                port: 3000
            })
            .unwrap(),
            "http://localhost:3000"
        );
        let err = build_origin(FrameOrigin {
            scheme: "http",
            host: "example.com",
            port: 0,
        })
        .unwrap_err();
        assert_eq!(err.dom_name(), "SecurityError");
        assert!(build_origin(FrameOrigin {
            scheme: "file",
            host: "",
            port: 0
        })
        .is_err());
    }

    #[test]
    fn parses_create_message_and_prepares_request() {
        let msg = r#"{"op":"create","requestId":"1","options":{
            "rp":{"id":"example.com","name":"Example"},
            "user":{"id":"dXNlcg","name":"alice","displayName":"Alice"},
            "challenge":"Y2hhbGxlbmdl",
            "pubKeyCredParams":[{"type":"public-key","alg":-257},{"type":"public-key","alg":-7}],
            "excludeCredentials":[{"type":"public-key","id":"AQI"}],
            "authenticatorSelection":{"userVerification":"required"},
            "attestation":"direct"}}"#;
        let IncomingMessage::Create {
            request_id,
            options,
        } = parse_message(msg).unwrap()
        else {
            panic!("应解析为 create");
        };
        assert_eq!(request_id, "1");
        let prepared = prepare_create(frame("login.example.com"), &options).unwrap();
        assert_eq!(prepared.origin, "https://login.example.com");
        assert_eq!(prepared.rp_id, "example.com");
        assert_eq!(prepared.challenge, b"challenge");
        let PreparedKind::Create(create) = prepared.kind else {
            panic!("应为注册请求");
        };
        assert_eq!(create.user_id, b"user");
        assert_eq!(create.exclude_credentials, vec![vec![1, 2]]);
        assert_eq!(create.user_verification, UserVerification::Required);
        assert_eq!(create.attestation, Attestation::Direct);
    }

    #[test]
    fn create_rejects_unsupported_parameters() {
        let parse = |extra: &str| {
            let msg = format!(
                r#"{{"op":"create","requestId":"1","options":{{"user":{{"id":"dXNlcg","name":"a"}},"challenge":"Y2g"{extra}}}}}"#
            );
            let IncomingMessage::Create { options, .. } = parse_message(&msg).unwrap() else {
                panic!("应解析为 create");
            };
            prepare_create(frame("example.com"), &options)
        };
        assert!(parse("").is_ok());
        assert_eq!(
            parse(r#","pubKeyCredParams":[{"type":"public-key","alg":-257}]"#)
                .unwrap_err()
                .dom_name(),
            "NotSupportedError"
        );
        assert_eq!(
            parse(r#","authenticatorSelection":{"authenticatorAttachment":"cross-platform"}"#)
                .unwrap_err()
                .dom_name(),
            "NotSupportedError"
        );
        let long_id = b64url_encode(&[7u8; 65]);
        let msg = format!(
            r#"{{"op":"create","requestId":"1","options":{{"user":{{"id":"{long_id}","name":"a"}},"challenge":"Y2g"}}}}"#
        );
        let IncomingMessage::Create { options, .. } = parse_message(&msg).unwrap() else {
            panic!("应解析为 create");
        };
        assert_eq!(
            prepare_create(frame("example.com"), &options)
                .unwrap_err()
                .dom_name(),
            "TypeError"
        );
    }

    #[test]
    fn parses_get_and_cancel_messages() {
        let msg = r#"{"op":"get","requestId":"7","options":{"challenge":"Y2g=","rpId":"example.com","allowCredentials":[{"type":"public-key","id":"AQI"}]}}"#;
        let IncomingMessage::Get { options, .. } = parse_message(msg).unwrap() else {
            panic!("应解析为 get");
        };
        let prepared = prepare_get(frame("example.com"), &options).unwrap();
        assert_eq!(prepared.challenge, b"ch");
        assert_eq!(
            prepared.kind,
            PreparedKind::Get(PreparedGet {
                allow_credentials: vec![vec![1, 2]],
                user_verification: UserVerification::Preferred,
            })
        );
        assert!(matches!(
            parse_message(r#"{"op":"cancel","requestId":"7"}"#).unwrap(),
            IncomingMessage::Cancel { request_id } if request_id == "7"
        ));
        assert_eq!(parse_message("{}").unwrap_err().dom_name(), "TypeError");
        let empty = r#"{"op":"get","requestId":"1","options":{"challenge":""}}"#;
        let IncomingMessage::Get { options, .. } = parse_message(empty).unwrap() else {
            panic!("应解析为 get");
        };
        assert!(prepare_get(frame("example.com"), &options).is_err());
    }

    #[test]
    fn extracts_es256_public_key_from_attestation() {
        let x = [1u8; 32];
        let y = [2u8; 32];
        let auth = auth_data(&cose_es256(&x, &y), &[9, 9, 9]);
        let info = parse_attestation_object(&attestation(&auth)).unwrap();
        assert_eq!(info.authenticator_data, auth);
        assert_eq!(info.public_key_algorithm, COSE_ALG_ES256);
        let spki = info.public_key_spki.unwrap();
        assert_eq!(spki.len(), 91);
        assert_eq!(&spki[..26], &P256_SPKI_PREFIX);
        assert_eq!(spki[26], 0x04);
        assert_eq!(&spki[27..59], &x);
        assert_eq!(&spki[59..], &y);
    }

    #[test]
    fn rejects_malformed_attestation() {
        assert!(parse_attestation_object(&[]).is_err());
        assert!(parse_attestation_object(&text("x")).is_err());
        // authData 缺少 AT 标志
        let mut auth = vec![0u8; 37];
        auth[32] = 0x01;
        assert!(parse_attestation_object(&attestation(&auth)).is_err());
        // 凭据长度越界
        let mut truncated = auth_data(&cose_es256(&[1; 32], &[2; 32]), &[1]);
        truncated[53] = 0xff;
        assert!(parse_attestation_object(&attestation(&truncated)).is_err());
        // 声明长度远超实际数据，不应预分配巨量内存
        assert!(
            parse_attestation_object(&[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])
                .is_err()
        );
    }

    #[test]
    fn builds_response_json() {
        let auth = auth_data(&cose_es256(&[1; 32], &[2; 32]), &[9]);
        let att = attestation(&auth);
        let value = registration_response(&[9], b"{}", &att).unwrap();
        assert_eq!(value["id"], "CQ");
        assert_eq!(value["response"]["publicKeyAlgorithm"], -7);
        assert_eq!(value["response"]["attestationObject"], b64url_encode(&att));

        let value = assertion_response(&[9], b"{}", &[1], &[2], &[]);
        assert!(value["response"]["userHandle"].is_null());
        let value = assertion_response(&[9], b"{}", &[1], &[2], b"u");
        assert_eq!(value["response"]["userHandle"], "dQ");
    }

    #[test]
    fn maps_authorization_errors() {
        assert_eq!(map_authorization_error(1001).dom_name(), "NotAllowedError");
        assert_eq!(
            map_authorization_error(1006).dom_name(),
            "InvalidStateError"
        );
        assert!(map_authorization_error(1004)
            .to_reply()
            .starts_with("NotAllowedError: "));
    }
}
