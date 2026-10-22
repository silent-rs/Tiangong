//! Fs 文件操作的请求与响应（7 个文件操作）。
//!
//! 对模型暴露的是 `file_read` / `file_write` 两个按 action 分派的工具
//! （见 crate 根的工具名常量）；sidecar 解析参数后按单一操作粒度处理。
//!
//! 所有读写请求都内嵌 [`FsAccessContext`]（沙箱预留点 B），sidecar 据此
//! 做路径解析。

use serde::{Deserialize, Serialize};
use tiangong_types::InjectedAsset;

use crate::FsAccessContext;

/// 文件操作在结果文案中的名称：对模型暴露的「工具(action)」形态。
pub const LIST_DIR_LABEL: &str = "file_read(list)";
pub const TREE_DIR_LABEL: &str = "file_read(tree)";
pub const READ_FILE_LABEL: &str = "file_read(read)";
pub const READ_IMAGE_LABEL: &str = "file_read(image)";
pub const WRITE_FILE_LABEL: &str = "file_write(write)";
pub const REPLACE_IN_FILE_LABEL: &str = "file_write(replace)";
pub const APPLY_PATCH_LABEL: &str = "file_write(patch)";

/// `read_image` 产物长边默认上限（与 computer-use 截图一致）。
pub const DEFAULT_READ_IMAGE_MAX_DIMENSION: u32 = 1568;
/// `read_image` 产物长边可设置的最大值（主流多模态接口的单边上限）。
pub const MAX_READ_IMAGE_MAX_DIMENSION: u32 = 8000;
/// 注入声明中的来源标记。
pub const READ_IMAGE_INJECTION_SOURCE: &str = "read_image";

// ── 通用响应（对齐 core ToolResult 字段，便于 sidecar 直接构造）─────

/// Fs 工具响应：保留与 core `ToolResult` 同构字段，便于 sidecar 直接构造。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsToolResponse {
    pub ok: bool,
    pub summary: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// 写工具返回：本次操作实际加锁的路径（供 sidecar 上报 locked 事件）。
    #[serde(default)]
    pub locked_paths: Vec<String>,
    /// 写工具返回：本次操作实际解锁的路径（供 sidecar 上报 unlocked 事件）。
    #[serde(default)]
    pub unlocked_paths: Vec<String>,
}

// ── 读工具请求/响应 ───────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListDirRequest {
    /// 目录路径，默认当前目录。
    #[serde(default)]
    pub path: Option<String>,
    #[serde(flatten)]
    pub access: FsAccessContext,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TreeDirRequest {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub max_depth: usize,
    #[serde(flatten)]
    pub access: FsAccessContext,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: String,
    #[serde(default)]
    pub start_line: usize,
    #[serde(default)]
    pub max_lines: usize,
    #[serde(flatten)]
    pub access: FsAccessContext,
}
/// `read_image` 请求：把本地图片规范化后放进媒体目录并声明注入。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadImageRequest {
    pub path: String,
    /// 产物长边上限；None 取 [`DEFAULT_READ_IMAGE_MAX_DIMENSION`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_dimension: Option<u32>,
    #[serde(flatten)]
    pub access: FsAccessContext,
}
/// `read_image` 成功时写入 [`FsToolResponse::stdout`] 的 JSON 结构。
///
/// 字段本身是给模型的「知道」；`injected_assets` 非空时 core 在工具批次
/// 闭合后把副本落成宿主注入的图片消息（「看见」，RFC 0017）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReadImageOutput {
    /// 源图片绝对路径。
    pub path: String,
    /// 媒体目录下的注入副本绝对路径。
    pub asset_path: String,
    /// 产物像素尺寸。
    pub width: u32,
    pub height: u32,
    /// 原图像素尺寸（已按 EXIF 方向校正）。
    pub original_width: u32,
    pub original_height: u32,
    /// 原图像素 / 产物像素的倍率（未缩放为 1）。
    pub scale: f64,
    pub mime_type: String,
    pub size_bytes: u64,
    /// 按文件头识别的源格式（png/jpeg/gif/webp/bmp/tiff/heif）。
    pub source_format: String,
    /// 是否经过重新编码（缩放、方向校正或格式转换）。
    #[serde(default)]
    pub transcoded: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub injected_assets: Vec<InjectedAsset>,
}

// ── 写工具请求/响应 ───────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WriteFileRequest {
    pub path: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub append: bool,
    #[serde(flatten)]
    pub access: FsAccessContext,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplaceInFileRequest {
    pub path: String,
    pub old: String,
    #[serde(default)]
    pub new: String,
    #[serde(default)]
    pub replace_all: bool,
    #[serde(default)]
    pub expected_count: Option<usize>,
    #[serde(flatten)]
    pub access: FsAccessContext,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApplyPatchRequest {
    pub patch: String,
    #[serde(default)]
    pub verify: bool,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(flatten)]
    pub access: FsAccessContext,
}
