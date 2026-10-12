//! 定时任务执行相关工具。
//!
//! 原进程内执行逻辑（`SchedulerContext`、`execute_job`、`restore_cron_jobs`）已
//! 下沉到 scheduler sidecar（`tiangong-plugin-scheduler-sidecar`）。本模块只保留
//! 供 sidecar 和入口层共用的 cron 表达式校验。

/// 校验 schedule 字符串能否被调度器解析。
///
/// 与 sidecar 内 `restore_cron_jobs` 用同一套 `cron::Schedule::from_str` 解析（silent 的
/// `ProcessTime::try_from` 对 cron 字符串内部即调用它），确保「创建期校验通过」与
/// 「恢复期能解析」行为一致。底层 `cron` crate 要求 6~7 字段表达式
///（`秒 分 时 日 月 周 [年]`），5 字段的 crontab 写法（如 `25 21 * * *`）会在第 6
/// 字段（周）处 EOF 失败。
///
/// 返回 `Ok(())` 表示可解析；`Err` 携带底层解析错误，调用方可直接展示给用户。
pub fn validate_cron_schedule(expr: &str) -> anyhow::Result<()> {
    use std::str::FromStr;
    cron::Schedule::from_str(expr).map(|_| ())?;
    Ok(())
}
