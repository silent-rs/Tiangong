//! 本机音频：媒体目录定位、后台播放与录音会话管理。
//!
//! 由原 text-to-speech / speech-to-text 插件迁入，行为保持不变：
//! - 音频统一落在 `~/.tiangong/media/`（宿主注入存储根时以其为准）；
//! - 播放为后台进程，经 `play_status` 轮询、`stop` 终止（sidecar 只有一路播放）；
//! - 录音同一时刻至多一路，停止/取消必须携带 `record_start` 的会话 ID。

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV;
use tiangong_plugin_volcengine_protocol::{
    PlayRequest, PlayResponse, RecordControlRequest, RecordStartRequest, RecordStartResponse,
    RecordStopResponse,
};

use crate::record_session::{self, LiveSender, RecordSession};

// ── 媒体目录 ──

/// 媒体目录：优先宿主注入的存储根，其次 `~/.tiangong`。
///
/// 本插件不申请模型配置等敏感存储权限，沙箱模式下宿主不注入存储根，
/// 此时回落到用户目录下的默认位置（与附件归档目录一致）。
pub fn media_dir() -> Result<PathBuf> {
    let root = std::env::var_os(STORAGE_ROOT_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".tiangong"))
        })
        .context("无法定位天工存储目录（未注入存储根且缺少 HOME）")?;
    Ok(root.join("media"))
}

/// 构造 `<media>/<prefix>_<scru128>.<ext>` 路径并确保目录存在。
pub fn media_file_path(prefix: &str, ext: &str) -> Result<PathBuf> {
    let dir = media_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建媒体目录失败：{}", dir.display()))?;
    Ok(dir.join(format!("{prefix}_{}.{ext}", scru128::new())))
}

/// 校验并规范化待识别的音频路径：必须位于媒体目录内且真实存在。
pub fn resolve_media_audio(file_path: &str) -> Result<PathBuf> {
    if file_path.trim().is_empty() {
        bail!("file_path 不能为空");
    }
    let canonical = std::fs::canonicalize(file_path).context("文件不存在或无法访问")?;
    let media = media_dir()?;
    let canonical_media = std::fs::canonicalize(&media).context("媒体目录不存在")?;
    if !canonical.starts_with(&canonical_media) {
        bail!("音频文件必须在 ~/.tiangong/media 目录下");
    }
    Ok(canonical)
}

// ── 播放 ──

/// Windows 下抑制子进程控制台窗口（CREATE_NO_WINDOW），避免播放时闪黑窗。
#[cfg(windows)]
fn suppress_console_window(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn suppress_console_window(_command: &mut std::process::Command) {}

/// 当前唯一一路播放：外部播放器进程（播放文件）或流式播放（边合成边播）。
enum Playing {
    Process(std::process::Child),
    Stream(crate::playback::StreamPlayer),
}

static PLAYING: Mutex<Option<Playing>> = Mutex::new(None);
/// 最近一次流式朗读的失败原因（`play_status` 读取即清除）。
static PLAYBACK_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn playing_slot() -> std::sync::MutexGuard<'static, Option<Playing>> {
    PLAYING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 收割已结束的播放；仍在播放时返回 `true`。
pub fn play_running() -> bool {
    let mut guard = playing_slot();
    let running = match guard.as_mut() {
        None => false,
        Some(Playing::Process(child)) => matches!(child.try_wait(), Ok(None)),
        Some(Playing::Stream(player)) => player.is_running(),
    };
    if !running {
        *guard = None;
    }
    running
}

/// 终止当前播放（仅终止本 sidecar 自己启动的播放）。
pub fn stop_playback() {
    let taken = playing_slot().take();
    match taken {
        Some(Playing::Process(mut child)) => {
            let _ = child.kill();
            let _ = child.wait();
        }
        Some(Playing::Stream(mut player)) => player.stop(),
        None => {}
    }
}

pub fn set_playback_error(message: String) {
    *PLAYBACK_ERROR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
}

pub fn take_playback_error() -> Option<String> {
    PLAYBACK_ERROR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

/// 开始流式播放（替换当前播放）：返回写入端与缓存写入器。
pub fn start_stream_playback(
    sample_rate: u32,
    cache_path: &std::path::Path,
) -> Result<(crate::playback::PlayerFeed, TtsCacheWriter)> {
    stop_playback();
    take_playback_error();
    let (player, feed) = crate::playback::StreamPlayer::start(sample_rate)?;
    *playing_slot() = Some(Playing::Stream(player));
    Ok((feed, TtsCacheWriter::new(cache_path, sample_rate)))
}

// ── 朗读缓存 ──

/// 朗读缓存路径：`<media>/tts_cache/<模型·音色·语速·文本 的哈希>.wav`。
///
/// 同一内容只合成一次；音色或语速变化视为不同音频。
pub fn tts_cache_path(
    model: &str,
    speaker: &str,
    speed: Option<f64>,
    text: &str,
) -> Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let dir = media_dir()?.join("tts_cache");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建朗读缓存目录失败：{}", dir.display()))?;
    // 两个不同种子的 SipHash 拼成 128 bit，碰撞概率可忽略。
    let key = format!(
        "{model}\u{1f}{speaker}\u{1f}{}\u{1f}{text}",
        speed.map(|value| format!("{value:.3}")).unwrap_or_default()
    );
    let digest = [0u64, 0x9e37_79b9_7f4a_7c15].map(|seed| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        seed.hash(&mut hasher);
        key.hash(&mut hasher);
        hasher.finish()
    });
    Ok(dir.join(format!("{:016x}{:016x}.wav", digest[0], digest[1])))
}

/// 流式合成的缓存写入：先写临时文件，完整收完后改名为正式缓存，
/// 中途停止 / 失败时删除，避免缓存半截音频。
pub struct TtsCacheWriter {
    target: PathBuf,
    temp: PathBuf,
    writer: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
    carry: Option<u8>,
}

impl TtsCacheWriter {
    fn new(target: &std::path::Path, sample_rate: u32) -> Self {
        let temp = target.with_extension(format!("{}.part", scru128::new()));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&temp, spec)
            .map_err(|error| tracing::warn!(%error, "创建朗读缓存失败，本次只播放不缓存"))
            .ok();
        Self {
            target: target.to_path_buf(),
            temp,
            writer,
            carry: None,
        }
    }

    /// 写入 16 bit 小端 PCM（可跨包拆分奇数字节）；写失败则放弃缓存。
    pub fn write(&mut self, bytes: &[u8]) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        let mut data = Vec::with_capacity(bytes.len() + 1);
        data.extend(self.carry.take());
        data.extend_from_slice(bytes);
        let (pairs, remainder) = data.as_chunks::<2>();
        let mut failed = false;
        for pair in pairs {
            if writer.write_sample(i16::from_le_bytes(*pair)).is_err() {
                failed = true;
                break;
            }
        }
        if let [last] = remainder {
            self.carry = Some(*last);
        }
        if failed {
            tracing::warn!("写入朗读缓存失败，本次只播放不缓存");
            self.discard_inner();
        }
    }

    /// 完整收完：收尾 WAV 头并改名为正式缓存。
    pub fn commit(mut self) {
        let Some(writer) = self.writer.take() else {
            return;
        };
        let committed =
            writer.finalize().is_ok() && std::fs::rename(&self.temp, &self.target).is_ok();
        if !committed {
            tracing::warn!(path = %self.target.display(), "保存朗读缓存失败");
            let _ = std::fs::remove_file(&self.temp);
        }
    }

    pub fn discard(mut self) {
        self.discard_inner();
    }

    fn discard_inner(&mut self) {
        if self.writer.take().is_some() {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

impl Drop for TtsCacheWriter {
    fn drop(&mut self) {
        self.discard_inner();
    }
}

/// 启动后台播放并立即返回；已有播放未结束时先停旧的。
///
/// 阻塞到播完会让播放期间的 stop 请求失去意义，因此完成状态经
/// `play_status` 轮询获取。
pub fn play(request: PlayRequest) -> Result<PlayResponse> {
    let path = std::path::Path::new(&request.file_path);
    if !path.exists() {
        bail!("音频文件不存在：{}", request.file_path);
    }
    stop_playback();

    let mut command = play_command(&request.file_path)?;
    suppress_console_window(&mut command);
    let child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("播放失败")?;
    *playing_slot() = Some(Playing::Process(child));
    Ok(PlayResponse { started: true })
}

/// 按平台构造播放命令。
///
/// 火山引擎合成产物为 mp3：macOS 用 afplay；Windows 用 WPF MediaPlayer
/// （SoundPlayer 只支持 wav）；Linux 依次尝试 ffplay / mpg123 / paplay。
fn play_command(file_path: &str) -> Result<std::process::Command> {
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("afplay");
        command.arg(file_path);
        Ok(command)
    }

    #[cfg(target_os = "windows")]
    {
        let escaped = file_path.replace('\'', "''");
        let script = format!(
            "Add-Type -AssemblyName PresentationCore; \
             $p = New-Object System.Windows.Media.MediaPlayer; \
             $p.Open([Uri]'{escaped}'); \
             while (-not $p.NaturalDuration.HasTimeSpan) {{ Start-Sleep -Milliseconds 50 }}; \
             $p.Play(); \
             Start-Sleep -Milliseconds ([int]$p.NaturalDuration.TimeSpan.TotalMilliseconds + 200); \
             $p.Close()"
        );
        let mut command = std::process::Command::new("powershell");
        command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        Ok(command)
    }

    #[cfg(target_os = "linux")]
    {
        let candidates: [(&str, &[&str]); 3] = [
            ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
            ("mpg123", &["-q"]),
            ("paplay", &[]),
        ];
        for (program, args) in candidates {
            if which(program) {
                let mut command = std::process::Command::new(program);
                command.args(args).arg(file_path);
                return Ok(command);
            }
        }
        bail!("未找到可用的音频播放器（需要 ffplay / mpg123 / paplay 之一）")
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = file_path;
        bail!("当前平台不支持本机音频播放")
    }
}

#[cfg(target_os = "linux")]
fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

// ── 录音 ──

static RECORDING: Mutex<Option<RecordSession>> = Mutex::new(None);

/// 开始录音：使用调用方传入的会话 ID 启动原生采集。
///
/// `live` 非空时，采集到的 16 kHz PCM 同时实时转发给识别任务。
pub fn record_start(
    request: RecordStartRequest,
    live: Option<LiveSender>,
) -> Result<RecordStartResponse> {
    let session_id = request.session_id.trim().to_string();
    if session_id.is_empty() {
        bail!("record_start 缺少 session_id（由调用方生成并传入）");
    }
    let mut guard = RECORDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // 槽位残留只可能来自异常路径（前端崩溃未发取消）：取消旧会话后开始新录音。
    if let Some(stale) = guard.take() {
        stale.cancel();
    }
    let file_path = media_file_path("stt_rec", "wav")?;
    *guard = Some(record_session::start(session_id.clone(), file_path, live)?);
    Ok(RecordStartResponse { session_id })
}

/// 停止录音：会话匹配校验 → 停流收尾 → 返回音频文件路径与时长。
pub fn record_stop(request: RecordControlRequest) -> Result<RecordStopResponse> {
    let mut guard = RECORDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let session = take_matching_session(&mut guard, &request)?;
    let file_path = session.file_path.to_string_lossy().to_string();
    let stats = session.stop()?;
    // 整段采样全 0：设备正常打开但系统只下发静音（麦克风权限或沙箱
    // device-microphone 未放行），送去识别只会得到「没有识别到内容」。
    if stats.peak == 0 {
        let _ = std::fs::remove_file(&file_path);
        bail!(
            "麦克风没有采集到声音（录音为静音）：请在「系统设置 → 隐私与安全性 → 麦克风」中允许天工访问，并确认输入设备正确"
        );
    }
    Ok(RecordStopResponse {
        file_path,
        mime_type: "audio/wav".to_string(),
        duration: Some(stats.duration),
        text: None,
    })
}

/// 取消录音：会话匹配时终止采集并删除录音文件；不匹配或无录音时静默成功（幂等）。
pub fn record_cancel(request: RecordControlRequest) {
    let mut guard = RECORDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Ok(session) = take_matching_session(&mut guard, &request) {
        session.cancel();
    }
}

/// 取出与请求会话 ID 匹配的录音会话；不匹配时**不动现有录音**并返回错误。
fn take_matching_session(
    guard: &mut Option<RecordSession>,
    request: &RecordControlRequest,
) -> Result<RecordSession> {
    let session = guard
        .take()
        .ok_or_else(|| anyhow::anyhow!("当前没有录音在进行中"))?;
    if session.session_id != request.session_id {
        *guard = Some(session);
        bail!(
            "录音会话不匹配（请求 {}，当前为其他会话），已忽略",
            request.session_id
        );
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_requests_without_recording_are_handled() {
        let request = RecordControlRequest {
            session_id: "missing".to_string(),
        };
        // 取消是幂等的：无录音时静默成功。
        record_cancel(request.clone());
        assert!(record_stop(request).is_err());
        assert!(
            record_start(
                RecordStartRequest {
                    session_id: "  ".to_string(),
                    ..RecordStartRequest::default()
                },
                None
            )
            .is_err()
        );
    }

    #[test]
    fn play_rejects_missing_file_and_status_is_idle() {
        assert!(
            play(PlayRequest {
                file_path: "/definitely/missing/volcengine.mp3".to_string()
            })
            .is_err()
        );
        stop_playback();
        assert!(!play_running());
    }

    #[test]
    fn tts_cache_key_distinguishes_voice_speed_and_text() {
        let base = tts_cache_path("m", "v1", None, "你好").unwrap();
        assert_eq!(base, tts_cache_path("m", "v1", None, "你好").unwrap());
        assert_eq!(base.extension().and_then(|ext| ext.to_str()), Some("wav"));
        for other in [
            tts_cache_path("m", "v2", None, "你好").unwrap(),
            tts_cache_path("m", "v1", Some(1.5), "你好").unwrap(),
            tts_cache_path("m", "v1", None, "你好。").unwrap(),
            tts_cache_path("m2", "v1", None, "你好").unwrap(),
        ] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn tts_cache_writer_commits_only_complete_audio() {
        let dir = std::env::temp_dir().join(format!("volc-tts-cache-{}", scru128::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("a.wav");

        let mut writer = TtsCacheWriter::new(&target, 24_000);
        writer.write(&[0x00, 0x40, 0x00]);
        writer.write(&[0xC0]);
        writer.commit();
        let reader = hound::WavReader::open(&target).unwrap();
        assert_eq!(reader.len(), 2, "跨包拆分的字节应拼成完整样本");

        let aborted = dir.join("b.wav");
        let mut writer = TtsCacheWriter::new(&aborted, 24_000);
        writer.write(&[0x00, 0x40]);
        writer.discard();
        assert!(!aborted.exists());
        // 临时文件也被清理：目录里只剩已提交的缓存。
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(names.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_media_audio_rejects_outside_paths() {
        assert!(resolve_media_audio(" ").is_err());
        let outside = std::env::temp_dir().join(format!("volc-outside-{}.wav", scru128::new()));
        std::fs::write(&outside, b"RIFF").unwrap();
        let result = resolve_media_audio(outside.to_str().unwrap());
        std::fs::remove_file(&outside).ok();
        assert!(result.is_err());
    }
}
