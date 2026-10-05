//! 远程访问配置与设备绑定（`~/.tiangong/remote.json`）。
//!
//! 同一时刻只允许一个手机端使用：
//! - 扫码时携带一次性配对码（10 分钟有效，仅能使用一次），配对成功后签发设备令牌；
//! - 只保存一个设备令牌的 SHA-256 摘要，重新配对会使旧设备失效；
//! - 已绑定设备同时也只允许一条在线连接，新连接接入时旧连接被踢下线。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// 配对码有效期。
pub const PAIRING_TTL: Duration = Duration::from_secs(600);
/// 局域网直连缺省端口。
pub const DEFAULT_LAN_PORT: u16 = 8790;

/// 远程访问方式。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteMode {
    /// 局域网直连：桌面端自身监听局域网端口，手机与电脑在同一网络内扫码使用。
    #[default]
    Lan,
    /// 经自部署中继（tiangong-relay），适合跨网络访问。
    Relay,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RemoteConfig {
    /// 是否启用远程访问。
    #[serde(default)]
    pub enabled: bool,
    /// 访问方式。
    #[serde(default)]
    pub mode: RemoteMode,
    /// 局域网直连监听端口（缺省 [`DEFAULT_LAN_PORT`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_port: Option<u16>,
    /// 局域网直连二维码使用的地址（留空自动探测本机局域网 IP）。
    #[serde(default)]
    pub lan_host: String,
    /// 中继地址（如 `https://relay.example.com`）。
    #[serde(default)]
    pub host: String,
    /// 通道密钥：由天工自动生成并只保存在本机，接入中继时出示；中继只用它的
    /// 单向摘要（通道 ID）路由手机端，部署中继时无需配置任何令牌。
    /// 局域网直连同样使用它，重新生成后需要重新扫码。
    #[serde(default)]
    pub token: String,
    /// 已绑定设备的令牌摘要（hex）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_token_sha256: Option<String>,
    /// 绑定时间（本地时间文本）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_bound_at: Option<String>,
    /// 设备标识（User-Agent 摘要，仅用于展示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_label: Option<String>,
}

pub fn config_path(root: &Path) -> PathBuf {
    root.join("remote.json")
}

impl RemoteConfig {
    pub fn load(root: &Path) -> Self {
        std::fs::read_to_string(config_path(root))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        std::fs::create_dir_all(root).ok();
        let path = config_path(root);
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("写入 {} 失败", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// 确保存在通道密钥；新生成时返回 true（调用方负责保存）。
    pub fn ensure_token(&mut self) -> bool {
        if tiangong_relay::validate_secret(&self.token).is_ok() {
            return false;
        }
        self.token = random_token();
        true
    }

    /// 通道 ID（通道密钥的单向摘要），出现在手机端访问地址中。
    pub fn channel(&self) -> Option<String> {
        tiangong_relay::validate_secret(&self.token)
            .ok()
            .map(|_| tiangong_relay::channel_id(&self.token))
    }

    /// 规范化后的中继地址（去尾部 `/`，要求 http/https）。
    pub fn normalized_host(&self) -> Result<String> {
        normalize_host(&self.host)
    }

    pub fn device_matches(&self, token: &str) -> bool {
        let Some(expected) = &self.device_token_sha256 else {
            return false;
        };
        !token.is_empty()
            && sha256_hex(token)
                .as_bytes()
                .ct_eq(expected.as_bytes())
                .into()
    }

    pub fn lan_port(&self) -> u16 {
        self.lan_port.unwrap_or(DEFAULT_LAN_PORT)
    }

    /// 手机端访问地址：中继模式为 `<中继地址>/?c=<通道 ID>`，局域网模式为
    /// `http://<局域网地址>:<端口>/?c=<通道 ID>`。
    pub fn access_url(&self) -> Result<String> {
        let base = match self.mode {
            RemoteMode::Relay => self.normalized_host()?,
            RemoteMode::Lan => {
                let host = match normalize_lan_host(&self.lan_host)? {
                    Some(host) => host,
                    None => detect_lan_ip()
                        .map(|ip| ip.to_string())
                        .context("未能探测到本机局域网地址，请手动填写")?,
                };
                format!("http://{}:{}", host, self.lan_port())
            }
        };
        let channel = self.channel().context("通道密钥缺失，请重新启用远程访问")?;
        Ok(format!("{base}/?c={channel}"))
    }
}

/// 规范化手动填写的局域网地址：接受 IPv4 或主机名，去掉误填的协议与端口。
pub fn normalize_lan_host(host: &str) -> Result<Option<String>> {
    let host = host
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    if host.is_empty() {
        return Ok(None);
    }
    let host = host.split(':').next().unwrap_or_default();
    if host.is_empty()
        || !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
    {
        anyhow::bail!("局域网地址只能是 IPv4 地址或主机名");
    }
    Ok(Some(host.to_string()))
}

/// 探测本机的局域网 IPv4 地址。
///
/// 枚举网卡而不按路由表出口推断：开着代理时出口常是 TUN 网卡（如 198.18.0.1），
/// 手机无法访问。只取已启用、非点对点网卡上的私有地址，按常见家用网段优先。
pub fn detect_lan_ip() -> Option<std::net::IpAddr> {
    let interfaces = if_addrs::get_if_addrs().ok()?;
    let mut candidates: Vec<(u8, std::net::Ipv4Addr)> = interfaces
        .iter()
        .filter(|iface| iface.is_oper_up() && !iface.is_p2p() && !iface.is_loopback())
        .filter_map(|iface| match iface.ip() {
            std::net::IpAddr::V4(ip) => lan_rank(ip).map(|rank| (rank, ip)),
            std::net::IpAddr::V6(_) => None,
        })
        .collect();
    candidates.sort_by_key(|(rank, _)| *rank);
    candidates.first().map(|(_, ip)| std::net::IpAddr::V4(*ip))
}

/// 局域网地址优先级（越小越优先）；非私有地址返回 None。
fn lan_rank(ip: std::net::Ipv4Addr) -> Option<u8> {
    let [a, b, ..] = ip.octets();
    match (a, b) {
        (192, 168) => Some(0),
        (10, _) => Some(1),
        (172, 16..=31) => Some(2),
        _ => None,
    }
}

pub fn normalize_host(host: &str) -> Result<String> {
    let host = host.trim().trim_end_matches('/');
    if host.is_empty() {
        anyhow::bail!("请先填写中继地址");
    }
    if !(host.starts_with("https://") || host.starts_with("http://")) {
        anyhow::bail!("中继地址需以 https:// 或 http:// 开头");
    }
    if host.contains(['?', '#', ' ']) {
        anyhow::bail!("中继地址不能包含查询参数或空格");
    }
    Ok(host.to_string())
}

/// 桌面端接入中继的 WebSocket 地址。
pub fn agent_ws_url(host: &str) -> Result<String> {
    let host = normalize_host(host)?;
    let ws = if let Some(rest) = host.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        format!("ws://{}", host.trim_start_matches("http://"))
    };
    Ok(format!("{ws}/agent/ws"))
}

pub fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 生成高强度随机令牌（URL 安全）。
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("系统随机源不可用");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 一次性配对码。
#[derive(Debug)]
pub struct Pairing {
    code: String,
    expires_at: Instant,
}

impl Pairing {
    pub fn new() -> Self {
        Self {
            code: random_token(),
            expires_at: Instant::now() + PAIRING_TTL,
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    pub fn matches(&self, candidate: &str) -> bool {
        Instant::now() < self.expires_at
            && !candidate.is_empty()
            && candidate.as_bytes().ct_eq(self.code.as_bytes()).into()
    }
}

impl Default for Pairing {
    fn default() -> Self {
        Self::new()
    }
}

/// 扫码地址：配对码放在 URL 片段中，不进入中继访问日志。
pub fn pairing_url(access_url: &str, code: &str) -> String {
    format!("{access_url}#pair={code}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_normalization() {
        assert_eq!(normalize_host(" https://a.com/ ").unwrap(), "https://a.com");
        assert!(normalize_host("a.com").is_err());
        assert!(normalize_host("").is_err());
        assert!(normalize_host("https://a.com/?x=1").is_err());
        assert_eq!(
            agent_ws_url("https://a.com/r").unwrap(),
            "wss://a.com/r/agent/ws"
        );
        assert_eq!(
            agent_ws_url("http://1.2.3.4:8790").unwrap(),
            "ws://1.2.3.4:8790/agent/ws"
        );
        assert_eq!(
            pairing_url("https://a.com/?c=abc", "c"),
            "https://a.com/?c=abc#pair=c"
        );
    }

    #[test]
    fn device_binding_is_single_and_hashed() {
        let mut config = RemoteConfig::default();
        let first = random_token();
        config.device_token_sha256 = Some(sha256_hex(&first));
        assert!(config.device_matches(&first));
        assert!(!config.device_matches(""));
        // 重新配对：旧设备失效。
        let second = random_token();
        config.device_token_sha256 = Some(sha256_hex(&second));
        assert!(config.device_matches(&second));
        assert!(!config.device_matches(&first));
    }

    #[test]
    fn pairing_code_validity() {
        let pairing = Pairing::new();
        assert!(pairing.matches(pairing.code()));
        assert!(!pairing.matches("wrong"));
        let expired = Pairing {
            code: "x".into(),
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        assert!(!expired.matches("x"));
    }

    #[test]
    fn lan_base_url() {
        assert_eq!(normalize_lan_host("").unwrap(), None);
        assert_eq!(
            normalize_lan_host(" http://192.168.1.5:8790/ ")
                .unwrap()
                .as_deref(),
            Some("192.168.1.5")
        );
        assert_eq!(
            normalize_lan_host("mac.local").unwrap().as_deref(),
            Some("mac.local")
        );
        assert!(normalize_lan_host("a b").is_err());
        let token = "s".repeat(32);
        let channel = tiangong_relay::channel_id(&token);
        let config = RemoteConfig {
            mode: RemoteMode::Lan,
            lan_host: "192.168.1.5".into(),
            lan_port: Some(9000),
            token: token.clone(),
            ..Default::default()
        };
        assert_eq!(
            config.access_url().unwrap(),
            format!("http://192.168.1.5:9000/?c={channel}")
        );
        let default_port = RemoteConfig {
            lan_host: "10.0.0.2".into(),
            token: token.clone(),
            ..Default::default()
        };
        assert_eq!(
            default_port.access_url().unwrap(),
            format!("http://10.0.0.2:8790/?c={channel}")
        );
        let relay = RemoteConfig {
            mode: RemoteMode::Relay,
            host: "https://r.example.com/".into(),
            token,
            ..Default::default()
        };
        assert_eq!(
            relay.access_url().unwrap(),
            format!("https://r.example.com/?c={channel}")
        );
        // 没有通道密钥时不能生成地址；补齐后保持不变。
        let mut missing = RemoteConfig {
            lan_host: "10.0.0.2".into(),
            ..Default::default()
        };
        assert!(missing.access_url().is_err());
        assert!(missing.ensure_token());
        assert!(!missing.ensure_token());
        assert!(missing.access_url().is_ok());
        // 旧配置缺省字段时按局域网模式读取。
        let legacy: RemoteConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!(legacy.mode, RemoteMode::Lan);
        assert_eq!(legacy.lan_port(), DEFAULT_LAN_PORT);
    }

    #[test]
    fn lan_address_ranking() {
        use std::net::Ipv4Addr;
        assert_eq!(lan_rank(Ipv4Addr::new(192, 168, 0, 102)), Some(0));
        assert_eq!(lan_rank(Ipv4Addr::new(10, 1, 2, 3)), Some(1));
        assert_eq!(lan_rank(Ipv4Addr::new(172, 20, 0, 1)), Some(2));
        // 代理 TUN（基准测试保留段）、公网、CGNAT 都不当作局域网地址。
        assert_eq!(lan_rank(Ipv4Addr::new(198, 18, 0, 1)), None);
        assert_eq!(lan_rank(Ipv4Addr::new(8, 8, 8, 8)), None);
        assert_eq!(lan_rank(Ipv4Addr::new(100, 64, 0, 1)), None);
        assert_eq!(lan_rank(Ipv4Addr::new(172, 32, 0, 1)), None);
        if let Some(std::net::IpAddr::V4(ip)) = detect_lan_ip() {
            assert!(lan_rank(ip).is_some(), "探测结果应为私有地址：{ip}");
        }
    }

    #[test]
    fn config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let config = RemoteConfig {
            enabled: true,
            host: "https://a.com".into(),
            token: "t".repeat(20),
            ..Default::default()
        };
        config.save(dir.path()).unwrap();
        assert_eq!(RemoteConfig::load(dir.path()), config);
    }
}
