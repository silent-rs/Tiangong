//! Windows 截图与应用唤起（与 macOS 同协议语义）。
//!
//! 坐标：进程为 Per-Monitor-V2 DPI 感知，屏幕坐标即虚拟桌面物理像素（主屏
//! 左上原点），与 UIA bounds、`desktop_input` 坐标同系；截图 1 图片像素 =
//! 1 屏幕像素，超过长边上限时按 1/2、1/4… 缩小，`scale` 给出倍率。
//!
//! 截图：GDI `BitBlt(SRCCOPY | CAPTUREBLT)` 从屏幕 DC 取所见即所得画面
//! （包含遮挡物，与真人看到的一致；天工指针/HUD 在截取瞬间短暂隐藏，
//! 不进入截图，但对远程桌面与录屏保持可见），
//! WIC 高质量缩放后编码 JPEG。
//!
//! 唤起：已运行的应用（按进程名/窗口标题匹配）恢复最小化并置前；未运行时
//! 先在开始菜单快捷方式（按显示名/本地化名匹配）中查找，再交给
//! `ShellExecuteEx`（App Paths 注册名，如 `WeChat`、`notepad`）。
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Foundation::{GENERIC_WRITE, POINT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY,
    MONITORINFO, MonitorFromPoint, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR,
    GUID_WICPixelFormat32bppBGR, IWICImagingFactory, IWICPalette, WICBitmapDitherTypeNone,
    WICBitmapEncoderNoCache, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
};
use windows::Win32::System::Com::StructuredStorage::{IPropertyBag2, PROPBAG2};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::Threading::GetProcessId;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_Programs, KF_FLAG_DEFAULT, SEE_MASK_FLAG_NO_UI,
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, SHGetKnownFolderPath,
    ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetForegroundWindow, GetWindow, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE, SW_SHOWNORMAL,
    SetForegroundWindow, ShowWindow,
};
use windows::core::{GUID, HSTRING, PCWSTR, PWSTR, w};

use tiangong_plugin_computer_use_protocol::ops::{
    DEFAULT_SCREENSHOT_MAX_DIMENSION, OpenAppRequest, OpenAppResponse, ScreenshotRequest,
};
use tiangong_plugin_computer_use_protocol::{
    Bounds, DesktopError, DesktopResult, InjectedAsset, ScreenshotResponse,
};

use super::screenshot_plan::{
    SCREENSHOT_JPEG_QUALITY, plan_screenshot_output, screenshot_output_dir,
};

/// 应用窗口出现的等待上限。
const WINDOW_WAIT: Duration = Duration::from_secs(10);

// ── 顶层窗口枚举 ──────────────────────────────────────────────

/// 用户可见的顶层应用窗口（按 Z 序，最前在前）。
#[derive(Debug, Clone)]
pub(crate) struct TopWindow {
    pub hwnd: HWND,
    pub pid: u32,
    pub title: String,
    /// 进程可执行文件名（不含扩展名，如 `WeChat`、`notepad`）。
    pub exe_stem: String,
    pub bounds: Bounds,
    pub minimized: bool,
}

// SAFETY：HWND 只是系统窗口句柄值（非进程内指针），可在线程间传递；
// 所有调用都经 Win32 API，由系统校验句柄有效性。
unsafe impl Send for TopWindow {}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY：lparam 为 enum_top_windows 传入的 Vec<HWND> 指针，枚举期间有效。
    let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    list.push(hwnd);
    BOOL(1)
}

fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY：buf 可写。
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

/// 可见（非隐藏、未被 DWM 隐藏）的无 owner 顶层窗口视为应用窗口。
fn is_app_window(hwnd: HWND) -> bool {
    // SAFETY：纯查询。
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
            return false;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return false;
        }
    }
    !window_text(hwnd).is_empty()
}

/// 窗口可见边框（DWM 扩展边框，不含阴影；失败时退回 GetWindowRect）。
pub(crate) fn window_bounds(hwnd: HWND) -> Bounds {
    let mut rect = RECT::default();
    // SAFETY：rect 可写。
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        )
        .is_ok()
            || GetWindowRect(hwnd, &mut rect).is_ok()
    };
    if !ok {
        return Bounds::default();
    }
    Bounds {
        x: f64::from(rect.left),
        y: f64::from(rect.top),
        width: f64::from(rect.right - rect.left),
        height: f64::from(rect.bottom - rect.top),
    }
}

pub(crate) fn process_exe_path(pid: u32) -> Option<PathBuf> {
    // SAFETY：句柄用后关闭；缓冲区长度由 size 传入。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &buf[..size as usize],
        )))
    }
}

fn exe_stem(pid: u32) -> String {
    process_exe_path(pid)
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// 枚举顶层应用窗口（EnumWindows 按 Z 序自顶向下）。
pub(crate) fn enum_top_windows() -> Vec<TopWindow> {
    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY：回调只在本调用期间使用 handles 指针。
    unsafe {
        let _ = EnumWindows(
            Some(collect_window),
            LPARAM(&mut handles as *mut Vec<HWND> as isize),
        );
    }
    handles
        .into_iter()
        .filter(|hwnd| is_app_window(*hwnd))
        .map(|hwnd| {
            let mut pid = 0u32;
            // SAFETY：pid 可写。
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            TopWindow {
                hwnd,
                pid,
                title: window_text(hwnd),
                exe_stem: exe_stem(pid),
                bounds: window_bounds(hwnd),
                // SAFETY：纯查询。
                minimized: unsafe { IsIconic(hwnd).as_bool() },
            }
        })
        .collect()
}

/// 前台窗口所属进程 pid。
pub(crate) fn foreground_pid() -> Option<u32> {
    // SAFETY：纯查询。
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

/// 系统内置应用的中文/英文显示名 → 可执行文件名。这些应用在开始菜单
/// 中多为系统/UWP 入口（无同名 .lnk），仅靠快捷方式无法解析中文名。
const BUILTIN_ALIASES: &[(&str, &str)] = &[
    ("记事本", "notepad"),
    ("画图", "mspaint"),
    ("计算器", "calc"),
    ("命令提示符", "cmd"),
    ("文件资源管理器", "explorer"),
    ("资源管理器", "explorer"),
    ("写字板", "wordpad"),
    ("截图工具", "snippingtool"),
    ("任务管理器", "taskmgr"),
    ("控制面板", "control"),
    ("注册表编辑器", "regedit"),
    ("paint", "mspaint"),
    ("calculator", "calc"),
    ("file explorer", "explorer"),
    ("task manager", "taskmgr"),
];

/// 名称 → 可执行文件名别名（无对照时返回 None）。
pub(crate) fn builtin_exe_alias(name: &str) -> Option<&'static str> {
    let needle = name.trim().to_lowercase();
    BUILTIN_ALIASES
        .iter()
        .find(|(alias, _)| *alias == needle)
        .map(|(_, exe)| *exe)
}

/// 名称匹配（大小写不敏感）：可执行文件名（含内置别名）与窗口标题任一
/// 命中即可。所有按 app_name 定位窗口的工具（open/screenshot/snapshot/
/// list/wait）统一走此规则，保证同一名称在各工具中结果一致。
pub(crate) fn name_matches(exe_stem: &str, title: &str, needle: &str) -> bool {
    name_match_rank(exe_stem, title, needle).is_some()
}

/// 名称匹配优先级：0 = 可执行文件名/别名精确，1 = 可执行文件名包含，
/// 2 = 窗口标题包含；未命中返回 None。
pub(crate) fn name_match_rank(exe_stem: &str, title: &str, needle: &str) -> Option<u8> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let stem = exe_stem.to_lowercase();
    if !stem.is_empty() {
        if stem == needle || builtin_exe_alias(&needle).is_some_and(|exe| stem == exe) {
            return Some(0);
        }
        if stem.contains(&needle) {
            return Some(1);
        }
    }
    title.to_lowercase().contains(&needle).then_some(2)
}

#[cfg(test)]
fn window_matches(window: &TopWindow, needle: &str) -> bool {
    name_matches(&window.exe_stem, &window.title, needle)
}

/// 定位目标应用最前的窗口：pid 优先，其次名称（按匹配优先级取最佳，
/// 同级取 Z 序最前）。
pub(crate) fn find_app_window(pid: Option<u32>, name: Option<&str>) -> Option<TopWindow> {
    let windows = enum_top_windows();
    if let Some(pid) = pid {
        return windows.into_iter().find(|w| w.pid == pid);
    }
    let name = name?;
    windows
        .iter()
        .filter_map(|w| name_match_rank(&w.exe_stem, &w.title, name).map(|r| (r, w)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, w)| w.clone())
}

// ── 截图 ──────────────────────────────────────────────────────

fn primary_monitor_bounds() -> Bounds {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY：纯查询。
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let _ = GetMonitorInfoW(monitor, &mut info);
    }
    let r = info.rcMonitor;
    Bounds {
        x: f64::from(r.left),
        y: f64::from(r.top),
        width: f64::from(r.right - r.left),
        height: f64::from(r.bottom - r.top),
    }
}

fn backend_error(reason: impl Into<String>) -> DesktopError {
    DesktopError::BackendUnavailable {
        reason: reason.into(),
    }
}

/// 从屏幕 DC 抓取矩形区域为 32bpp BGRX 像素（自顶向下）。
fn grab_screen(x: i32, y: i32, width: i32, height: i32) -> Result<Vec<u8>, DesktopError> {
    // SAFETY：GDI 资源在本函数内创建并逐一释放；像素区由 DIB 管理，拷出后释放。
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err(DesktopError::DesktopSessionUnavailable {
                reason: "无法获取屏幕 DC（可能处于锁屏或安全桌面）".to_string(),
            });
        }
        let memory = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            Ok(bitmap) if !bits.is_null() => bitmap,
            _ => {
                let _ = DeleteDC(memory);
                ReleaseDC(None, screen);
                return Err(backend_error("创建截图位图失败"));
            }
        };
        let old = SelectObject(memory, bitmap.into());
        let blit = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            Some(screen),
            x,
            y,
            SRCCOPY | CAPTUREBLT,
        );
        let pixels = blit.is_ok().then(|| {
            std::slice::from_raw_parts(bits.cast::<u8>(), (width * height * 4) as usize).to_vec()
        });
        SelectObject(memory, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        pixels.ok_or_else(|| backend_error(format!("BitBlt 截屏失败：{}", blit.unwrap_err())))
    }
}

/// WIC：BGRX 像素 → 缩放 → 24bpp → JPEG 文件。
fn encode_jpeg(
    pixels: &[u8],
    (width, height): (u32, u32),
    (out_w, out_h): (u32, u32),
    path: &Path,
) -> windows::core::Result<()> {
    // SAFETY：WIC COM 接口均在本函数内创建与释放。
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bitmap = factory.CreateBitmapFromMemory(
            width,
            height,
            &GUID_WICPixelFormat32bppBGR,
            width * 4,
            pixels,
        )?;
        let scaler = factory.CreateBitmapScaler()?;
        scaler.Initialize(&bitmap, out_w, out_h, WICBitmapInterpolationModeFant)?;
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &scaler,
            &GUID_WICPixelFormat24bppBGR,
            WICBitmapDitherTypeNone,
            None::<&IWICPalette>,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let stream = factory.CreateStream()?;
        stream.InitializeFromFilename(&HSTRING::from(path.as_os_str()), GENERIC_WRITE.0)?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        let mut options: Option<IPropertyBag2> = None;
        encoder.CreateNewFrame(&mut frame, &mut options)?;
        let frame = frame.ok_or_else(windows::core::Error::empty)?;
        if let Some(options) = options.as_ref() {
            let mut name: Vec<u16> = "ImageQuality".encode_utf16().chain([0]).collect();
            let bag = PROPBAG2 {
                pstrName: PWSTR(name.as_mut_ptr()),
                ..Default::default()
            };
            let quality = VARIANT::from(SCREENSHOT_JPEG_QUALITY as f32 / 100.0);
            let _ = options.Write(1, &bag, &quality);
        }
        frame.Initialize(options.as_ref())?;
        frame.SetSize(out_w, out_h)?;
        let mut format: GUID = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut format)?;
        frame.WriteSource(&converter, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
        Ok(())
    }
}

fn ensure_com() {
    // SAFETY：当前线程 COM 初始化（已初始化时返回 S_FALSE/RPC_E_CHANGED_MODE，
    // 均可继续使用 WIC）。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// 解析截取矩形与目标应用名。
fn resolve_capture_target(req: &ScreenshotRequest) -> Result<(Bounds, String), DesktopError> {
    if let Some(region) = req
        .region
        .filter(|r| r.width.is_finite() && r.height.is_finite() && r.width > 0.0 && r.height > 0.0)
    {
        let rect = Bounds {
            x: region.x.round(),
            y: region.y.round(),
            width: region.width.round().max(1.0),
            height: region.height.round().max(1.0),
        };
        return Ok((rect, req.app_name.clone().unwrap_or_default()));
    }
    let pid = req
        .pid
        .filter(|p| *p > 0)
        .or_else(|| req.foreground_only.then(foreground_pid).flatten());
    let name = req
        .app_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    if pid.is_none() && name.is_none() {
        return Ok((primary_monitor_bounds(), String::new()));
    }
    let window = find_app_window(pid, name).ok_or_else(|| {
        backend_error("未找到目标应用可截取的屏幕窗口（窗口可能已最小化或不在屏幕上）")
    })?;
    if window.minimized {
        return Err(backend_error(
            "目标窗口已最小化，请先用 desktop_app action=open 唤起后再截图",
        ));
    }
    let app_name = name.map(str::to_string).unwrap_or_else(|| {
        if window.exe_stem.is_empty() {
            window.title.clone()
        } else {
            window.exe_stem.clone()
        }
    });
    Ok((window.bounds, app_name))
}

pub(crate) fn capture_screenshot(req: &ScreenshotRequest) -> DesktopResult<ScreenshotResponse> {
    match capture_screenshot_inner(req) {
        Ok(response) => DesktopResult::Ok(response),
        Err(error) => DesktopResult::Err(error),
    }
}

fn capture_screenshot_inner(req: &ScreenshotRequest) -> Result<ScreenshotResponse, DesktopError> {
    ensure_com();
    let (rect, app_name) = resolve_capture_target(req)?;
    let (width, height) = (rect.width as u32, rect.height as u32);
    if width == 0 || height == 0 {
        return Err(backend_error("截取区域为空"));
    }
    let pixels = {
        let _hidden = super::win_overlay::hide_for_capture();
        grab_screen(rect.x as i32, rect.y as i32, width as i32, height as i32)?
    };
    let max_dimension = req
        .max_dimension
        .filter(|m| *m > 0)
        .unwrap_or(DEFAULT_SCREENSHOT_MAX_DIMENSION);
    let plan = plan_screenshot_output((width, height), (rect.width, rect.height), max_dimension);
    let dir = screenshot_output_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| backend_error(format!("创建截图目录失败 {}：{e}", dir.display())))?;
    let path = dir.join(format!("desktop-{}.jpg", scru128::new()));
    encode_jpeg(&pixels, (width, height), (plan.width, plan.height), &path)
        .map_err(|e| backend_error(format!("JPEG 编码失败：{e}")))?;
    let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size_bytes == 0 {
        return Err(backend_error(format!("截图产物为空：{}", path.display())));
    }
    let original_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("screenshot.jpg")
        .to_string();
    Ok(ScreenshotResponse {
        path: path.display().to_string(),
        width: plan.width,
        height: plan.height,
        app_name,
        size_bytes,
        logical_bounds: Some(rect),
        scale: f64::from(plan.factor),
        injected_assets: vec![InjectedAsset {
            local_path: path.display().to_string(),
            mime_type: "image/jpeg".to_string(),
            original_name: Some(original_name),
            size_bytes,
            kind: tiangong_types::MediaKind::Image,
            source: Some("desktop_screenshot".to_string()),
        }],
    })
}

// ── 应用唤起 ──────────────────────────────────────────────────

/// 前台锁绕过：先发送一次 Alt 按下/抬起，使本进程获得设置前台窗口的权利
/// （Windows 仅允许最近收到输入的进程切换前台）。
fn unlock_foreground() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0x12), // VK_MENU
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [
        key(windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(0)),
        key(KEYEVENTF_KEYUP),
    ];
    // SAFETY：有效 INPUT 数组。
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

/// 恢复最小化并置前；返回是否成为前台窗口。
pub(crate) fn activate_window(hwnd: HWND) -> bool {
    // SAFETY：对他进程窗口的标准激活调用。
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd).as_bool() {
            return true;
        }
        unlock_foreground();
        SetForegroundWindow(hwnd).as_bool()
    }
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY：返回的字符串由 CoTaskMemFree 释放。
    unsafe {
        let raw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = raw.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(raw.0 as *const _));
        path
    }
}

/// 开始菜单快捷方式（`.lnk`）按文件名（即显示名/本地化名）匹配。
fn find_start_menu_shortcut(name: &str) -> Option<PathBuf> {
    let needle = name.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let mut roots = Vec::new();
    if let Some(p) = known_folder(&FOLDERID_Programs) {
        roots.push(p);
    }
    if let Some(p) = known_folder(&FOLDERID_CommonPrograms) {
        roots.push(p);
    }
    let mut shortcuts = Vec::new();
    for root in roots {
        collect_shortcuts(&root, 0, &mut shortcuts);
    }
    pick_shortcut(&shortcuts, &needle)
}

fn collect_shortcuts(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_shortcuts(&path, depth + 1, out);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
        {
            out.push(path);
        }
    }
}

/// 精确同名优先，其次前缀，再次包含；排除卸载/帮助类快捷方式。
fn pick_shortcut(shortcuts: &[PathBuf], needle: &str) -> Option<PathBuf> {
    let stem = |p: &PathBuf| {
        p.file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let usable = |p: &&PathBuf| {
        let s = stem(p);
        !["uninstall", "卸载", "readme", "help", "帮助"]
            .iter()
            .any(|bad| s.contains(bad))
    };
    shortcuts
        .iter()
        .filter(usable)
        .find(|p| stem(p) == needle)
        .or_else(|| {
            shortcuts
                .iter()
                .filter(usable)
                .find(|p| stem(p).starts_with(needle))
        })
        .or_else(|| {
            shortcuts
                .iter()
                .filter(usable)
                .find(|p| stem(p).contains(needle))
        })
        .cloned()
}

/// ShellExecuteEx 启动目标（快捷方式路径、App Paths 注册名或可执行文件）。
fn shell_launch(target: &str) -> Result<Option<u32>, String> {
    let file = HSTRING::from(target);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
        lpVerb: w!("open"),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY：info 在调用期间有效；返回的进程句柄用后关闭。
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| e.to_string())?;
        if info.hProcess.is_invalid() {
            return Ok(None);
        }
        let pid = GetProcessId(info.hProcess);
        let _ = CloseHandle(info.hProcess);
        Ok((pid != 0).then_some(pid))
    }
}

/// 等待应用窗口出现（按 pid 或名称），出现后置前。
async fn wait_for_window(pid: Option<u32>, name: &str) -> Option<TopWindow> {
    let deadline = Instant::now() + WINDOW_WAIT;
    loop {
        let found = pid
            .and_then(|p| find_app_window(Some(p), None))
            .or_else(|| find_app_window(None, Some(name)));
        if let Some(window) = found {
            activate_window(window.hwnd);
            return Some(window);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn open_response(window: &TopWindow, launched: bool, summary: String) -> OpenAppResponse {
    OpenAppResponse {
        app_name: if window.exe_stem.is_empty() {
            window.title.clone()
        } else {
            window.exe_stem.clone()
        },
        pid: window.pid,
        bundle_id: None,
        bundle_path: process_exe_path(window.pid).map(|p| p.display().to_string()),
        launched,
        window: Some(window.bounds),
        summary,
        split: None,
    }
}

pub(crate) async fn open_app(req: &OpenAppRequest) -> DesktopResult<OpenAppResponse> {
    // Windows 无 Bundle ID：bundle_id 视作可执行文件名/App Paths 注册名。
    let Some(name) = req
        .app_name
        .as_deref()
        .or(req.bundle_id.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
    else {
        return DesktopResult::Err(DesktopError::ApplicationNotFound {
            query: "desktop_open_app 需要 app_name".to_string(),
        });
    };
    ensure_com();

    // 已运行：恢复并置前。
    if let Some(window) = find_app_window(None, Some(&name)) {
        let foreground = activate_window(window.hwnd);
        let window = TopWindow {
            bounds: window_bounds(window.hwnd),
            ..window
        };
        let summary = if foreground {
            format!("「{}」已在运行，已唤起到前台", window.title)
        } else {
            format!(
                "「{}」已在运行并已恢复窗口，但系统前台锁阻止了置前，可点击其窗口激活",
                window.title
            )
        };
        return DesktopResult::Ok(open_response(&window, false, summary));
    }

    // 未运行：开始菜单快捷方式（显示名/本地化名）→ 内置应用别名 → App
    // Paths 注册名。
    let mut attempts = Vec::new();
    let shortcut = find_start_menu_shortcut(&name);
    let mut candidates: Vec<String> = Vec::new();
    if let Some(path) = &shortcut {
        candidates.push(path.display().to_string());
    }
    if let Some(exe) = builtin_exe_alias(&name) {
        candidates.push(exe.to_string());
    }
    candidates.push(name.clone());
    for target in candidates {
        match shell_launch(&target) {
            Ok(pid) => {
                // 快捷方式可能经启动器转交，窗口按名称或快捷方式名兜底匹配。
                let match_name = shortcut
                    .as_ref()
                    .and_then(|p| p.file_stem())
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| name.clone());
                let window = match wait_for_window(pid, &name).await {
                    Some(window) => Some(window),
                    None if match_name != name => wait_for_window(None, &match_name).await,
                    None => None,
                };
                return match window {
                    Some(window) => {
                        let summary = format!("已启动「{}」并置于前台", window.title);
                        DesktopResult::Ok(open_response(&window, true, summary))
                    }
                    None => DesktopResult::Ok(OpenAppResponse {
                        app_name: name.clone(),
                        pid: pid.unwrap_or(0),
                        bundle_id: None,
                        bundle_path: Some(target),
                        launched: true,
                        window: None,
                        summary: format!(
                            "已启动「{name}」，但 {} 秒内未检测到其窗口（可能在托盘或仍在加载）",
                            WINDOW_WAIT.as_secs()
                        ),
                        split: None,
                    }),
                };
            }
            Err(error) => attempts.push(format!("{target}：{error}")),
        }
    }
    DesktopResult::Err(DesktopError::ApplicationNotFound {
        query: format!("{name}（{}）", attempts.join("；")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_shortcut_prefers_exact_then_prefix_and_skips_uninstall() {
        let list: Vec<PathBuf> = [
            r"C:\Start\Programs\微信\卸载微信.lnk",
            r"C:\Start\Programs\微信\微信.lnk",
            r"C:\Start\Programs\Visual Studio Code.lnk",
            r"C:\Start\Programs\Code Helper.lnk",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(
            pick_shortcut(&list, "微信"),
            Some(PathBuf::from(r"C:\Start\Programs\微信\微信.lnk"))
        );
        assert_eq!(
            pick_shortcut(&list, "visual"),
            Some(PathBuf::from(r"C:\Start\Programs\Visual Studio Code.lnk"))
        );
        // 「Code Helper」含 help 被当作帮助类快捷方式排除，回落到包含匹配。
        assert_eq!(
            pick_shortcut(&list, "code"),
            Some(PathBuf::from(r"C:\Start\Programs\Visual Studio Code.lnk"))
        );
        assert_eq!(pick_shortcut(&list, "不存在"), None);
    }

    #[test]
    fn window_matching_uses_exe_and_title() {
        let window = TopWindow {
            hwnd: HWND::default(),
            pid: 1,
            title: "微信".to_string(),
            exe_stem: "Weixin".to_string(),
            bounds: Bounds::default(),
            minimized: false,
        };
        assert!(window_matches(&window, "weixin"));
        assert!(window_matches(&window, "微信"));
        assert!(!window_matches(&window, "QQ"));
        assert!(!window_matches(&window, "  "));
    }

    #[test]
    fn builtin_chinese_names_match_system_apps() {
        assert_eq!(builtin_exe_alias("记事本"), Some("notepad"));
        assert_eq!(builtin_exe_alias(" 画图 "), Some("mspaint"));
        assert_eq!(builtin_exe_alias("不存在"), None);
        assert!(name_matches("notepad", "*无标题 - 记事本", "notepad"));
        assert!(name_matches("notepad", "README.md - Notepad", "记事本"));
        assert!(name_matches("mspaint", "无标题 - 画图", "画图"));
        assert!(!name_matches("notepad", "无标题 - 记事本", "画图"));
        assert_eq!(name_match_rank("notepad", "x", "记事本"), Some(0));
        assert_eq!(
            name_match_rank("code", "记事本.txt - VS Code", "记事本"),
            Some(2)
        );
        assert_eq!(name_match_rank("notepad", "x", "note"), Some(1));
    }
}
