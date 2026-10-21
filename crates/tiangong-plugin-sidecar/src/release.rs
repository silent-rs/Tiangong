//! 插件发布约定：官方目录、签名发布清单的数据结构与信任根常量。
//!
//! 宿主按这些约定发现、安装和验签插件；需要自更新的 sidecar（如 memory）
//! 按同一约定下载并验签自身制品。这里只定义数据形态和常量，校验逻辑由
//! 使用方实现（宿主见 `tiangong_plugin_runtime::signature`）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 官方插件目录地址。
pub const PLUGIN_CATALOG_ENDPOINT: &str =
    "https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/plugins-index/catalog.json";

/// 签名发布清单文件名。
pub const SIGNED_RELEASE_FILE: &str = "release.json";
/// 签名发布清单的 minisign 签名文件名。
pub const SIGNATURE_FILE: &str = "release.json.sig";
/// 当前支持的签名发布清单版本。
pub const SIGNED_RELEASE_SCHEMA_VERSION: u32 = 1;
/// 插件内容清单固定文件名（devkit 构建生成，本地信任与官方签名共用的信任锚）。
pub const CONTENT_MANIFEST_FILE: &str = "content-manifest.json";

/// 插件专用 minisign 公钥。私钥只保存在官方发布环境（CI secret 与
/// 官方开发者本机，打包时经 TIANGONG_PLUGIN_SIGNING_PRIVATE_KEY_PATH 提供）。
pub const OFFICIAL_PUBKEY_B64: &str = include_str!("official-pubkey.b64");

/// 官方发布者标识（内置公钥验签）。
pub const OFFICIAL_PUBLISHER: &str = "tiangong-official";
/// 本机用户密钥的发布者标识（创作链自动签名使用）。
pub const LOCAL_PUBLISHER: &str = "local";

/// 官方插件目录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCatalog {
    pub version: u32,
    pub plugins: Vec<PluginRelease>,
}

/// 目录中的一个插件发布。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRelease {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub manifest: RemoteArtifact,
    /// 纯 UI 插件（无 WASM 制品）在目录中省略 wasm 条目；
    /// 是否下载以 plugin.json 是否声明 wasm 为准。
    #[serde(default)]
    pub wasm: Option<RemoteArtifact>,
    #[serde(default)]
    pub signed_releases: BTreeMap<String, RemoteSignedRelease>,
    #[serde(default)]
    pub sidecars: BTreeMap<String, RemoteArtifact>,
    #[serde(default)]
    pub ui: BTreeMap<String, RemoteArtifact>,
}

/// 远程签名发布清单及其签名地址。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSignedRelease {
    pub url: String,
    pub signature_url: String,
}

/// 远程制品地址与校验和。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteArtifact {
    pub url: String,
    pub checksum: String,
}

/// 签名发布清单（`release.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedPluginRelease {
    pub schema_version: u32,
    pub id: String,
    pub version: String,
    pub publisher: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    pub manifest: SignedArtifact,
    /// 纯 TS/sidecar 插件无 wasm 制品，签名清单省略 wasm 条目。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasm: Option<SignedArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<SignedArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<SignedArtifact>,
    /// 解释器 sidecar 插件：官方签名锚定完整内容清单（content-manifest.json
    /// 的哈希），清单覆盖全树——与本地信任锚同构，签名即信任根。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_manifest: Option<SignedArtifact>,
}

/// 签名清单中的一个制品（相对路径 + sha256）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedArtifact {
    pub path: PathBuf,
    pub sha256: String,
}

impl SignedPluginRelease {
    pub fn has_permission(&self, permission: &str) -> bool {
        self.permissions.iter().any(|item| item == permission)
    }
}

/// 当前平台的发布平台键（如 `darwin-aarch64`）。
pub fn current_platform_key() -> String {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    };
    format!("{os}-{arch}")
}
