//! 原生录音会话：cpal 采集默认麦克风 → 线性重采样 16 kHz → hound WAV 落盘。
//!
//! 不依赖 ffmpeg/arecord 等外部命令：跨平台经 CoreAudio（macOS）、WASAPI
//! （Windows）、ALSA（Linux）采集，也避免了 DirectShow 设备名不可靠的问题。
//!
//! 线程模型：cpal 的 `Stream` 不是 `Send`，全程由录音线程独占——采集回调
//! 在音频线程执行，只做多通道混合为单声道 f32 并写入有界通道（满则丢帧，
//! 绝不阻塞音频线程）；录音线程循环消费通道做重采样与写盘，并响应停止/
//! 取消命令。停止时线程停流、排空队列、hound 正常收尾（WAV 头完整），
//! 再回传录音时长。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TrySendError};
use std::time::Duration;

use anyhow::{Context, Result};
use cpal::Sample;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// 目标采样率（16 kHz 单声道 16bit，STT 输入的常用规格）。
pub const TARGET_SAMPLE_RATE: u32 = 16_000;
/// 采集回调 → 写盘循环之间有界队列的容量（帧），缓冲约数秒音频。
const AUDIO_QUEUE_FRAMES: usize = 512;
/// 写盘循环等待音频数据的超时：期间轮询控制命令，停止延迟不超过该值。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

enum Control {
    /// 停止并收尾，回传写盘的目标采样率样本总数（换算时长）。
    Stop(Sender<u64>),
    /// 取消：丢弃录音文件。
    Cancel,
}

/// 一次进行中的录音会话句柄（可跨线程持有；采集流留在录音线程内）。
pub struct RecordSession {
    pub session_id: String,
    pub file_path: PathBuf,
    control: std::sync::mpsc::Sender<Control>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// 启动一次录音：spawn 录音线程，等待其完成设备打开与文件创建。
pub fn start(session_id: String, file_path: PathBuf) -> Result<RecordSession> {
    let (control_tx, control_rx) = std::sync::mpsc::channel::<Control>();
    let (started_tx, started_rx) = std::sync::mpsc::channel::<Result<()>>();
    let spawn_path = file_path.clone();
    let worker = std::thread::Builder::new()
        .name("volcengine-record".to_string())
        .spawn(move || {
            if let Err(error) = run_recording(&spawn_path, &control_rx, &started_tx) {
                tracing::warn!(%error, "录音线程结束");
            }
        })
        .context("创建录音线程失败")?;

    started_rx
        .recv_timeout(Duration::from_secs(5))
        .context("等待录音启动超时")??;

    Ok(RecordSession {
        session_id,
        file_path,
        control: control_tx,
        worker: Some(worker),
    })
}

impl RecordSession {
    /// 停止录音：等待写盘收尾，返回录音时长（秒）。
    pub fn stop(mut self) -> Result<f64> {
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<u64>();
        self.control
            .send(Control::Stop(ack_tx))
            .context("发送停止命令失败")?;
        let samples = ack_rx
            .recv_timeout(Duration::from_secs(10))
            .context("等待录音收尾超时")?;
        self.join_worker();
        Ok(samples as f64 / TARGET_SAMPLE_RATE as f64)
    }

    /// 取消录音：丢弃产物（文件由录音线程删除）。
    pub fn cancel(mut self) {
        let _ = self.control.send(Control::Cancel);
        self.join_worker();
    }

    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 录音线程主体：打开设备 → 采集写盘循环 → 按命令收尾。
fn run_recording(
    file_path: &Path,
    control_rx: &Receiver<Control>,
    started_tx: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    let (stream, src_rate, audio_rx, dropped) = match open_input_stream() {
        Ok(opened) => opened,
        Err(error) => {
            let _ = started_tx.send(Err(anyhow::anyhow!(format!("{error:#}"))));
            return Err(error);
        }
    };

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let wav = match hound::WavWriter::create(file_path, spec) {
        Ok(wav) => wav,
        Err(error) => {
            let _ = started_tx.send(Err(anyhow::anyhow!(format!(
                "创建录音文件失败：{}（{error}）",
                file_path.display()
            ))));
            return Err(error.into());
        }
    };

    stream.play().context("启动麦克风采集失败")?;
    if let Err(error) = started_tx.send(Ok(())) {
        return Err(error.into());
    }

    // —— 采集写盘循环 ——
    let mut writer = (wav, Resampler::new(src_rate as f64));
    let mut written_samples: u64 = 0;
    let mut stop_ack: Option<Sender<u64>> = None;
    let mut cancel = false;
    loop {
        match control_rx.try_recv() {
            Ok(Control::Stop(ack)) => {
                stop_ack = Some(ack);
                break;
            }
            Ok(Control::Cancel) => {
                cancel = true;
                break;
            }
            // 控制端句柄全部 dropped（调用方异常）：按取消收尾，不遗留文件。
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                cancel = true;
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        match audio_rx.recv_timeout(POLL_INTERVAL) {
            Ok(samples) => {
                write_samples(&mut writer, &samples, &mut written_samples)?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            // 采集回调端全部释放（设备错误路径）：结束录音并按停止收尾。
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // 停止采集并排空队列中的剩余音频。
    drop(stream);
    while let Ok(samples) = audio_rx.try_recv() {
        write_samples(&mut writer, &samples, &mut written_samples)?;
    }

    if cancel {
        drop(writer.0);
        let _ = std::fs::remove_file(file_path);
        return Ok(());
    }

    writer.0.finalize().context("完成 WAV 文件收尾失败")?;
    let dropped_frames = dropped.load(Ordering::Relaxed);
    if dropped_frames > 0 {
        tracing::warn!(dropped_frames, "录音期间写入跟不上，丢弃了部分音频帧");
    }
    if let Some(ack) = stop_ack {
        let _ = ack.send(written_samples);
    }
    Ok(())
}

/// 打开默认麦克风的输入流（Stream 留在录音线程使用）。
/// 打开输入流的产物：采集流、源采样率、音频数据队列、丢帧计数。
type OpenedStream = (cpal::Stream, u32, Receiver<Vec<f32>>, Arc<AtomicU64>);

fn open_input_stream() -> Result<OpenedStream> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("未找到默认麦克风输入设备")?;
    let supported = device
        .default_input_config()
        .context("查询麦克风支持的输入格式失败")?;
    let src_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.clone().into();

    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(AUDIO_QUEUE_FRAMES);
    let dropped = Arc::new(AtomicU64::new(0));

    macro_rules! spawn_stream {
        ($ty:ty, $convert:expr) => {{
            let tx = audio_tx.clone();
            let dropped = Arc::clone(&dropped);
            device.build_input_stream(
                &config,
                move |data: $ty, _| {
                    let mono: Vec<f32> = $convert(data, channels);
                    match tx.try_send(mono) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(TrySendError::Disconnected(_)) => {}
                    }
                },
                |error| tracing::warn!(%error, "麦克风采集错误"),
                None,
            )
        }};
    }

    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            spawn_stream!(&[f32], |data: &[f32], ch: usize| mix_to_mono_f32(data, ch))
        }
        cpal::SampleFormat::I16 => spawn_stream!(&[i16], |data: &[i16], ch: usize| {
            mix_to_mono(data, ch, |s: &i16| s.to_sample::<f32>())
        }),
        cpal::SampleFormat::U16 => spawn_stream!(&[u16], |data: &[u16], ch: usize| {
            // to_sample 的 u16→f32 已是居中映射（32768→0.0），不得再偏移缩放。
            mix_to_mono(data, ch, |s: &u16| s.to_sample::<f32>())
        }),
        cpal::SampleFormat::I32 => spawn_stream!(&[i32], |data: &[i32], ch: usize| {
            mix_to_mono(data, ch, |s: &i32| s.to_sample::<f32>())
        }),
        format => anyhow::bail!("麦克风输入采样格式不支持：{format:?}"),
    }
    .context("打开麦克风输入流失败")?;

    Ok((stream, src_rate, audio_rx, dropped))
}

fn write_samples<W: std::io::Write + std::io::Seek>(
    writer: &mut (hound::WavWriter<W>, Resampler),
    samples: &[f32],
    written: &mut u64,
) -> Result<()> {
    writer.1.push(samples, |chunk| {
        for sample in chunk {
            writer.0.write_sample(*sample).context("写入音频数据失败")?;
        }
        *written += chunk.len() as u64;
        Ok(())
    })
}

/// 多通道（交错）混合为单声道；单通道直接转换。
fn mix_to_mono<T>(data: &[T], channels: usize, convert: impl Fn(&T) -> f32) -> Vec<f32> {
    if channels <= 1 {
        return data.iter().map(convert).collect();
    }
    data.chunks(channels)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| chunk.iter().map(&convert).sum::<f32>() / chunk.len() as f32)
        .collect()
}

fn mix_to_mono_f32(data: &[f32], channels: usize) -> Vec<f32> {
    mix_to_mono(data, channels, |s| *s)
}

/// 线性插值重采样器：任意源采样率 → 16 kHz，跨 chunk 保持相位。
///
/// 维护绝对源坐标 `next_pos`（相对当前 chunk 起点）：每个输出样本在两个
/// 源样本之间线性插值，步长 = 源率/目标率；上一 chunk 的最后样本作为下一
/// chunk 首个输出点的左端点。
struct Resampler {
    ratio: f64,
    next_pos: f64,
    prev: f32,
    buffer: Vec<i16>,
}

impl Resampler {
    fn new(src_rate: f64) -> Self {
        Self {
            ratio: src_rate / TARGET_SAMPLE_RATE as f64,
            next_pos: 0.0,
            prev: 0.0,
            buffer: Vec::new(),
        }
    }

    /// 输入一段单声道 f32 样本，插值输出 16 kHz i16 经回调写出。
    fn push(&mut self, samples: &[f32], mut emit: impl FnMut(&[i16]) -> Result<()>) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        self.buffer.clear();
        let len = samples.len() as f64;
        let mut pos = self.next_pos;
        while pos < len {
            let index = pos.floor() as usize;
            let frac = (pos - index as f64) as f32;
            let cur = samples[index];
            let left = if index == 0 {
                self.prev
            } else {
                samples[index - 1]
            };
            let value = left + (cur - left) * frac;
            self.buffer.push(clamp_to_i16(value));
            pos += self.ratio;
        }
        // 残余相位（< ratio）与最后样本带入下一 chunk。
        self.next_pos = pos - len;
        self.prev = samples[samples.len() - 1];
        if !self.buffer.is_empty() {
            emit(&self.buffer)?;
        }
        Ok(())
    }
}

fn clamp_to_i16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn resampler_降采样输出数量正确() {
        let mut resampler = Resampler::new(48_000.0);
        let mut total = 0usize;
        // 分 10 段送入共 1 秒 48 kHz 数据，验证跨 chunk 相位。
        for _ in 0..10 {
            resampler
                .push(&vec![0.3; 4_800], |chunk| {
                    total += chunk.len();
                    Ok(())
                })
                .unwrap();
        }
        assert!((total as i64 - 16_000).abs() < 20, "total={total}");
    }

    #[test]
    fn resampler_升采样输出数量正确() {
        let mut resampler = Resampler::new(8_000.0);
        let mut total = 0usize;
        for _ in 0..8 {
            resampler
                .push(&vec![0.2; 1_000], |chunk| {
                    total += chunk.len();
                    Ok(())
                })
                .unwrap();
        }
        assert!((total as i64 - 16_000).abs() < 20, "total={total}");
    }

    /// 本机手动实测真实麦克风（CI 无音频设备，默认忽略）：
    /// `cargo test -p tiangong-plugin-volcengine-sidecar -- --ignored`
    #[test]
    #[ignore = "需要真实麦克风，本机手动验证"]
    fn 实录3秒停止并校验wav() {
        let path = std::env::temp_dir().join(format!("stt-rec-test-{}.wav", scru128::new()));
        let session = start("live-test".into(), path.clone()).expect("启动录音失败");
        std::thread::sleep(Duration::from_secs(3));
        let duration = session.stop().expect("停止录音失败");
        assert!(duration > 2.5 && duration < 4.5, "duration={duration}");

        let meta = std::fs::metadata(&path).expect("录音文件不存在");
        let expected = 44u64 + (duration * TARGET_SAMPLE_RATE as f64) as u64 * 2;
        let diff = (meta.len() as i64 - expected as i64).abs();
        assert!(
            diff < 200,
            "文件大小 {} 与期望 {} 相差 {diff}",
            meta.len(),
            expected
        );

        let mut header = [0u8; 12];
        std::fs::File::open(&path)
            .unwrap()
            .read_exact(&mut header)
            .unwrap();
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(&header[8..12], b"WAVE");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[ignore = "需要真实麦克风，本机手动验证"]
    fn 实录取消不遗留文件() {
        let path = std::env::temp_dir().join(format!("stt-rec-test-{}.wav", scru128::new()));
        let session = start("live-cancel".into(), path.clone()).expect("启动录音失败");
        std::thread::sleep(Duration::from_millis(800));
        session.cancel();
        assert!(!path.exists(), "取消后录音文件应被删除");
    }

    /// cpal 的整数样本转换必须是居中映射：静音（中点）为 0，两端 ±1。
    /// 一旦转换语义变化（如变成 [0,1]），录音将整体失真，此测试守住该契约。
    #[test]
    fn 样本转换为居中浮点() {
        let min = i16::MIN.to_sample::<f32>();
        let mid = 0i16.to_sample::<f32>();
        let max = i16::MAX.to_sample::<f32>();
        assert!((min - (-1.0)).abs() < 1e-6, "i16::MIN -> {min}");
        assert!(mid.abs() < 1e-6, "i16 0 -> {mid}");
        assert!((max - 1.0).abs() < 1e-3, "i16::MAX -> {max}");

        let umin = u16::MIN.to_sample::<f32>();
        let umid = 32768u16.to_sample::<f32>();
        let umax = u16::MAX.to_sample::<f32>();
        assert!((umin - (-1.0)).abs() < 1e-6, "u16::MIN -> {umin}");
        assert!(umid.abs() < 1e-6, "u16 32768 -> {umid}");
        assert!((umax - 1.0).abs() < 1e-3, "u16::MAX -> {umax}");

        let i32min = i32::MIN.to_sample::<f32>();
        let i32mid = 0i32.to_sample::<f32>();
        let i32max = i32::MAX.to_sample::<f32>();
        assert!((i32min - (-1.0)).abs() < 1e-6, "i32::MIN -> {i32min}");
        assert!(i32mid.abs() < 1e-6, "i32 0 -> {i32mid}");
        assert!((i32max - 1.0).abs() < 1e-3, "i32::MAX -> {i32max}");
    }

    /// u16 设备路径的混音转换：静音样本应转换为 0（不产生直流偏移）。
    #[test]
    fn u16静音样本混音为零() {
        let silence = vec![32768u16; 64];
        let mono = mix_to_mono(&silence, 2, |s: &u16| s.to_sample::<f32>());
        assert_eq!(mono.len(), 32);
        for value in mono {
            assert!(value.abs() < 1e-6, "静音样本被转为 {value}");
        }
    }

    #[test]
    fn resampler_空输入不产出() {
        let mut resampler = Resampler::new(48_000.0);
        resampler.push(&[], |_| Ok(())).unwrap();
    }
}
