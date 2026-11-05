//! D1 契约域：`plugin.json` schema 与校验、Slot/贡献声明、启动策略、
//! 工具键解析与存储布局。只描述「插件是什么、放在哪」，不含进程、
//! 权限决策与任何运行状态。
pub mod launch_policy;
pub mod manifest;
pub mod slots;
pub mod storage_layout;
pub mod tool_key;
