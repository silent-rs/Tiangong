//! D3 分发与安装域：远程目录获取/下载/校验、staging 事务、安装/
//! 启停/回滚/卸载与文件系统事务、旧 ID 迁移。安装事务只发事实事件
//! （经 `crate::registry::events`），运行时资源收尾归事件发布方。
pub mod artifacts;
pub mod legacy;
pub mod migrations;
