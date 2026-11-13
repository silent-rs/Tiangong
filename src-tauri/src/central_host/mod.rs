//! 中枢宿主层（重构方案 §5.1 `central-host` 的落位，阶段 R1 起从
//! `app.rs` 逐步拆出）。首片：执行路由器。

mod execution_router;

pub use execution_router::ExecutionRouter;
