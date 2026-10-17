//! Core 配置归拢：Agent 运行时配置、TiangongCore 最小配置契约、模型路由配置转发。
//!
//! - [`agent`]：Agent 运行时配置（信任模式、思考强度等）
//! - [`core`]：TiangongCore 运行所需的最小配置契约（模型端点、工具、权限）
//! - [`models`]：模型路由配置类型的转发层（定义在 `tiangong-llm`）

pub mod agent;
pub mod core;
pub(crate) mod max_output;
pub mod models;
