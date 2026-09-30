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

static PLAYING: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// 收割已退出的播放进程；仍在播放时返回 `true`。锁中毒时按「无播放」处理。
pub fn play_running() -> bool {
    let Ok(mut guard) = PLAYING.lock() else {
        return false;
    };
    match guard.as_mut() {
        None => false,
        Some(child) => match child.try_wait() {
            Ok(Some(_)) | Err(_) => {
                *guard = None;
                false
            }
            Ok(None) => true,
        },
    }
}

/// 终止当前播放进程（仅终止本 sidecar 自己启动的进程）。
pub fn stop_playback() {
    let Ok(mut guard) = PLAYING.lock() else {
        return;
    };
    if let Some(mut child) = guard.take() {
        let _ = child.kill();
        let _ = child.wait();
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
    *PLAYING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(child);
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
    let duration = session.stop()?;
    Ok(RecordStopResponse {
        file_path,
        mime_type: "audio/wav".to_string(),
        duration: Some(duration),
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
    fn resolve_media_audio_rejects_outside_paths() {
        assert!(resolve_media_audio(" ").is_err());
        let outside = std::env::temp_dir().join(format!("volc-outside-{}.wav", scru128::new()));
        std::fs::write(&outside, b"RIFF").unwrap();
        let result = resolve_media_audio(outside.to_str().unwrap());
        std::fs::remove_file(&outside).ok();
        assert!(result.is_err());
    }
}
