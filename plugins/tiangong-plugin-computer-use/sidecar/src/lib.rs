//! Computer Use sidecar 库入口。
//!
//! 暴露平台后端与业务服务，便于集成测试与未来复用；main 二进制仅负责启动。

pub mod backend;
pub mod service;
pub mod split;

pub use service::ComputerUseService;
