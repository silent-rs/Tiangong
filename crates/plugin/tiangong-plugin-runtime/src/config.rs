//! WASM 插件运行时配置。

use std::time::Duration;

/// WASM 插件运行时资源配置。
///
/// 阶段一 PoC 仅提供合理的硬编码默认值；后续阶段从插件清单或宿主配置读取。
#[derive(Debug, Clone)]
pub struct PluginRuntimeConfig {
    /// 单次工具调用可消耗的最大 fuel（指令数）。
    pub fuel_limit: u64,
    /// epoch 心跳间隔：宿主以此周期递增 epoch 计数。
    pub epoch_interval: Duration,
    /// 旧版 epoch 时限配置。Agent Handler 调用不再使用，仅保留配置兼容。
    pub epoch_deadline: Duration,
    /// 单个 WASM 实例线性内存上限（字节）。
    pub memory_limit: usize,
}

impl Default for PluginRuntimeConfig {
    fn default() -> Self {
        Self {
            // 10 亿 fuel：足够完成一次工具调用，但能拦住死循环。
            fuel_limit: 1_000_000_000,
            // 每 50ms 推进一次 epoch。
            epoch_interval: Duration::from_millis(50),
            // 单次调用最多执行 10 秒。
            epoch_deadline: Duration::from_secs(10),
            // 单实例最多 64 MB 内存。
            memory_limit: 64 * 1024 * 1024,
        }
    }
}

impl PluginRuntimeConfig {
    /// 把超时时长换算为 epoch tick 数（deadline）。
    ///
    /// wasmtime 的 epoch 以 `epoch_interval` 为单位递增，deadline 表示「允许
    /// 经过多少次 epoch 递增」。这里取超时时长 / 心跳间隔，向上取整。
    pub fn epoch_deadline_ticks(&self) -> u64 {
        let ticks = self.epoch_deadline.as_nanos() / self.epoch_interval.as_nanos().max(1);
        ticks.max(1) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_deadline_ticks_scales_with_timeout() {
        let cfg = PluginRuntimeConfig {
            epoch_interval: Duration::from_millis(50),
            epoch_deadline: Duration::from_secs(10),
            ..PluginRuntimeConfig::default()
        };
        // 10s / 50ms = 200 ticks
        assert_eq!(cfg.epoch_deadline_ticks(), 200);
    }

    #[test]
    fn epoch_deadline_ticks_at_least_one() {
        let cfg = PluginRuntimeConfig {
            epoch_interval: Duration::from_secs(1),
            epoch_deadline: Duration::from_millis(1),
            ..PluginRuntimeConfig::default()
        };
        assert_eq!(cfg.epoch_deadline_ticks(), 1);
    }
}
