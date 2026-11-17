use serde::{Deserialize, Serialize};

// ── 文件对象 ──────────────────────────────────────────────

/// 上传的文件对象，归属 API key，可在 Chat Completions 与
/// Responses API 中通过 `file_id` 引用。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FileObject {
    /// 形如 `file-api-` 前缀加十六进制字符串。
    #[serde(default)]
    pub id: String,
    /// 恒为 `file`。
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub bytes: u64,
    /// 创建时间（Unix 秒级时间戳）。
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub filename: String,
    /// 恒为 `user_data`。
    #[serde(default)]
    pub purpose: String,
    /// 仅上传时设置了有效期才出现。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

// ── 列出文件 ──────────────────────────────────────────────

/// Files API 文档解析方式（上传时指定，见「文件解析」指南）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileParsePurpose {
    /// 只有文字：文档里的图表、照片、示意图会被丢弃；适合以文字为主的文档。
    #[default]
    #[serde(rename = "text_recognition")]
    TextRecognition,
    /// 文字加文档里的图：图表等被裁出并按原位置交给模型；适合需要结合图理解的文档。
    #[serde(rename = "rich_layout_recognition")]
    RichLayoutRecognition,
}

impl FileParsePurpose {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TextRecognition => "text_recognition",
            Self::RichLayoutRecognition => "rich_layout_recognition",
        }
    }
}

/// 上传并解析文档的文件大小上限：100 MiB。
pub const MAX_DOCUMENT_FILE_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ListFilesParams {
    /// 分页游标：返回排在该 file_id 之后的文件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// 1–1000，默认 1000。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// 按创建时间排序，默认 asc。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ListOrder>,
    /// 目前唯一取值 `user_data`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ListOrder {
    #[default]
    #[serde(rename = "asc")]
    Asc,
    #[serde(rename = "desc")]
    Desc,
}

impl ListOrder {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ListFilesResponse {
    /// 恒为 `list`。
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub data: Vec<FileObject>,
    /// 列表首个文件 ID，可作分页游标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,
    /// 列表末个文件 ID，可作分页游标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
    #[serde(default)]
    pub has_more: bool,
}

// ── 删除文件 ──────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeleteFileResponse {
    #[serde(default)]
    pub id: String,
    /// 恒为 `file`。
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub deleted: bool,
}
