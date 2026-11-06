//! D5 进程与传输域：进程拉起/终止/进程树、stdio 与 TCP 帧、握手、
//! 超时、取消、环境注入、沙箱策略组装、连接池与换代、解释器发现、
//! Launcher 能力探测。输入是已定策略，不读注册表状态（运行环境与
//! 回调经 `RuntimeEnv` / `SidecarEvents` 注入）。
pub mod connections;
pub mod interpreter_env;
pub mod launcher_update;
pub mod platform;
pub mod sidecar;
