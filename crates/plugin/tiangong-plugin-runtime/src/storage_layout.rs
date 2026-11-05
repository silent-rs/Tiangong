//! 存储布局契约（域：契约）。
//!
//! 插件子系统在存储根下的目录约定集中在这里，避免各域各自拼路径：
//! 沙箱组装曾直接引用 `plugin_dev` 的目录常量，使传输层反向依赖创作工作流。
//!
//! ```text
//! <storage_root>/
//!   plugins/                正式安装目录（discovery 扫描）
//!     .inbox/<id>/          导入收件箱（`.` 前缀，不参与加载）
//!   plugins-dev/<id>/       开发项目目录（devkit 工作区）
//! ```

use std::path::{Path, PathBuf};

/// 正式安装目录名。
pub const PLUGINS_DIR: &str = "plugins";
/// 插件开发目录名（位于存储根下，与 `plugins/` 平级）。
pub const DEV_DIR: &str = "plugins-dev";
/// 导入收件箱子目录名（位于 `plugins/` 下，`.` 前缀使目录扫描天然跳过）。
pub const INBOX_DIR: &str = ".inbox";

/// 存储根下的目录视图。
#[derive(Debug, Clone, Copy)]
pub struct StorageLayout<'a> {
    root: &'a Path,
}

impl<'a> StorageLayout<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &'a Path {
        self.root
    }

    /// 正式安装目录。
    pub fn plugins(&self) -> PathBuf {
        self.root.join(PLUGINS_DIR)
    }

    /// 单个插件的安装目录。
    pub fn plugin(&self, plugin_id: &str) -> PathBuf {
        self.plugins().join(plugin_id)
    }

    /// 插件开发根目录。
    pub fn dev_root(&self) -> PathBuf {
        self.root.join(DEV_DIR)
    }

    /// 单个开发项目目录。
    pub fn dev_project(&self, project_id: &str) -> PathBuf {
        self.dev_root().join(project_id)
    }

    /// 导入收件箱根目录。
    pub fn inbox(&self) -> PathBuf {
        self.plugins().join(INBOX_DIR)
    }

    /// 收件箱中的单个产物目录。
    pub fn inbox_plugin(&self, plugin_id: &str) -> PathBuf {
        self.inbox().join(plugin_id)
    }

    /// 插件创作写域（开发根 + 收件箱）：宿主按已验证身份授予后，作为
    /// sidecar 沙箱的额外可写根（spawn 前保障目录存在且非符号链接）。
    pub fn dev_writable_roots(&self) -> Vec<PathBuf> {
        vec![self.dev_root(), self.inbox()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 目录布局与写域() {
        let layout = StorageLayout::new(Path::new("/root"));
        assert_eq!(layout.plugins(), PathBuf::from("/root/plugins"));
        assert_eq!(layout.plugin("demo"), PathBuf::from("/root/plugins/demo"));
        assert_eq!(
            layout.dev_project("demo"),
            PathBuf::from("/root/plugins-dev/demo")
        );
        assert_eq!(
            layout.inbox_plugin("demo"),
            PathBuf::from("/root/plugins/.inbox/demo")
        );
        assert_eq!(
            layout.dev_writable_roots(),
            vec![
                PathBuf::from("/root/plugins-dev"),
                PathBuf::from("/root/plugins/.inbox")
            ]
        );
    }
}
