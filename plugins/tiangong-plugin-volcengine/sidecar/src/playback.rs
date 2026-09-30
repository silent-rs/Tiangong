//! 流式语音播放：cpal 默认输出设备，边收边播 16 bit 单声道 PCM。
//!
//! 线程模型：cpal 的 `Stream` 不是 `Send`，由播放线程独占；网络任务经
//! [`PlayerFeed`] 把解码后的样本（已重采样到设备采样率）写入共享队列，
//! 输出回调从队列取样，队列暂空时补静音（网络慢于播放时不报错、不中断）。
//! 输入结束且队列排空后播放线程自行退出；停止时立即静音并退出。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cpal::Sample;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// 播放线程轮询停止 / 结束条件的间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// 队列排空后再等一小段，让设备缓冲里的尾音播完。
const DRAIN_TAIL: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Shared {
    queue: Mutex<VecDeque<f32>>,
    input_done: AtomicBool,
    stop: AtomicBool,
    finished: AtomicBool,
}

impl Shared {
    fn queue(&self) -> std::sync::MutexGuard<'_, VecDeque<f32>> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 播放端句柄（登记在播放槽位中，停止 / 查询状态用）。
pub struct StreamPlayer {
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// 写入端（网络任务持有）：写入 PCM、标记输入结束、感知是否已被停止。
pub struct PlayerFeed {
    shared: Arc<Shared>,
    resampler: LinearResampler,
    carry: Option<u8>,
    scratch: Vec<f32>,
}

impl StreamPlayer {
    /// 打开默认输出设备并开始播放（初始为静音，等待写入）。
    pub fn start(source_rate: u32) -> Result<(Self, PlayerFeed)> {
        let shared = Arc::new(Shared::default());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32>>();
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("volcengine-playback".to_string())
            .spawn(move || {
                if let Err(error) = run_output(&worker_shared, &ready_tx) {
                    let _ = ready_tx.send(Err(anyhow!(format!("{error:#}"))));
                    tracing::warn!(%error, "流式播放线程结束");
                }
                worker_shared.finished.store(true, Ordering::Release);
            })
            .context("创建播放线程失败")?;
        let device_rate = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .context("等待音频输出设备就绪超时")??;
        let player = Self {
            shared: Arc::clone(&shared),
            worker: Some(worker),
        };
        let feed = PlayerFeed {
            shared,
            resampler: LinearResampler::new(source_rate, device_rate),
            carry: None,
            scratch: Vec::new(),
        };
        Ok((player, feed))
    }

    /// 仍在播放（未停止且尚未播完）。
    pub fn is_running(&self) -> bool {
        !self.shared.finished.load(Ordering::Acquire)
    }

    /// 立即停止：静音、结束播放线程。
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for StreamPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl PlayerFeed {
    /// 播放已被停止（新的播放替换或用户停止）：网络任务应放弃后续数据。
    pub fn is_stopped(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire) || self.shared.finished.load(Ordering::Acquire)
    }

    /// 写入一段 16 bit 小端单声道 PCM（可跨包拆分奇数字节）。
    pub fn push_pcm16(&mut self, bytes: &[u8]) {
        let mut samples = Vec::with_capacity(bytes.len() / 2 + 1);
        let mut rest = bytes;
        if let Some(low) = self.carry.take() {
            match rest.split_first() {
                Some((high, tail)) => {
                    samples.push(i16::from_le_bytes([low, *high]).to_sample::<f32>());
                    rest = tail;
                }
                None => {
                    self.carry = Some(low);
                    return;
                }
            }
        }
        let (pairs, remainder) = rest.as_chunks::<2>();
        for pair in pairs {
            samples.push(i16::from_le_bytes(*pair).to_sample::<f32>());
        }
        if let [last] = remainder {
            self.carry = Some(*last);
        }
        self.scratch.clear();
        self.resampler.push(&samples, &mut self.scratch);
        self.shared.queue().extend(self.scratch.iter().copied());
    }

    /// 输入结束：队列播完后播放线程自行退出。
    pub fn finish(&self) {
        self.shared.input_done.store(true, Ordering::Release);
    }
}

impl Drop for PlayerFeed {
    fn drop(&mut self) {
        // 网络任务异常结束也要让播放线程能退出，不留常驻输出流。
        self.finish();
    }
}

fn run_output(shared: &Arc<Shared>, ready: &std::sync::mpsc::Sender<Result<u32>>) -> Result<()> {
    let device = cpal::default_host()
        .default_output_device()
        .context("未找到默认音频输出设备")?;
    let supported = device
        .default_output_config()
        .context("查询音频输出格式失败")?;
    let rate = supported.sample_rate().0;
    let channels = usize::from(supported.channels()).max(1);
    let config: cpal::StreamConfig = supported.config();

    macro_rules! build {
        ($ty:ty) => {{
            let shared = Arc::clone(shared);
            device.build_output_stream(
                &config,
                move |data: &mut [$ty], _| fill(data, channels, &shared),
                |error| tracing::warn!(%error, "音频输出错误"),
                None,
            )
        }};
    }
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::I32 => build!(i32),
        format => anyhow::bail!("音频输出采样格式不支持：{format:?}"),
    }
    .context("打开音频输出流失败")?;
    stream.play().context("启动音频输出失败")?;
    let _ = ready.send(Ok(rate));

    loop {
        std::thread::sleep(POLL_INTERVAL);
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        if shared.input_done.load(Ordering::Acquire) && shared.queue().is_empty() {
            std::thread::sleep(DRAIN_TAIL);
            break;
        }
    }
    drop(stream);
    Ok(())
}

/// 输出回调：从队列取单声道样本复制到各声道；队列空或已停止时补静音。
fn fill<T: Sample + cpal::FromSample<f32>>(data: &mut [T], channels: usize, shared: &Shared) {
    let stopped = shared.stop.load(Ordering::Acquire);
    let mut queue = shared.queue();
    for frame in data.chunks_mut(channels) {
        let value = if stopped {
            0.0
        } else {
            queue.pop_front().unwrap_or(0.0)
        };
        let sample = T::from_sample(value);
        frame.iter_mut().for_each(|out| *out = sample);
    }
}

/// 线性插值重采样（单声道 f32），跨分包保持相位。
struct LinearResampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl LinearResampler {
    fn new(source_rate: u32, target_rate: u32) -> Self {
        Self {
            step: f64::from(source_rate.max(1)) / f64::from(target_rate.max(1)),
            pos: 0.0,
            prev: 0.0,
        }
    }

    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let len = input.len() as f64;
        while self.pos < len {
            let index = self.pos.floor() as usize;
            let frac = (self.pos - index as f64) as f32;
            let left = if index == 0 {
                self.prev
            } else {
                input[index - 1]
            };
            out.push(left + (input[index] - left) * frac);
            self.pos += self.step;
        }
        self.pos -= len;
        self.prev = input[input.len() - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_without_device(source_rate: u32, device_rate: u32) -> PlayerFeed {
        PlayerFeed {
            shared: Arc::new(Shared::default()),
            resampler: LinearResampler::new(source_rate, device_rate),
            carry: None,
            scratch: Vec::new(),
        }
    }

    #[test]
    fn 重采样输出数量按采样率比例() {
        let mut resampler = LinearResampler::new(24_000, 48_000);
        let mut out = Vec::new();
        for _ in 0..10 {
            resampler.push(&[0.5; 2_400], &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() < 4, "len={}", out.len());
    }

    #[test]
    fn 跨包拆分的奇数字节按样本拼回() {
        let mut feed = feed_without_device(24_000, 24_000);
        let bytes = [0x00u8, 0x40, 0x00, 0xC0]; // 16384, -16384
        feed.push_pcm16(&bytes[..1]);
        feed.push_pcm16(&bytes[1..3]);
        feed.push_pcm16(&bytes[3..]);
        let queue: Vec<f32> = feed.shared.queue().iter().copied().collect();
        assert_eq!(queue.len(), 2, "{queue:?}");
        // 同采样率：首个输出样本从上一包末样本（初始 0）插值，第二个为 16384。
        assert!((queue[1] - 0.5).abs() < 1e-3, "{queue:?}");
    }

    #[test]
    fn 写入端析构即标记输入结束() {
        let feed = feed_without_device(24_000, 48_000);
        let shared = Arc::clone(&feed.shared);
        assert!(!shared.input_done.load(Ordering::Acquire));
        drop(feed);
        assert!(shared.input_done.load(Ordering::Acquire));
    }
}
