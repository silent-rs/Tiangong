//! Windows 自动分屏：天工窗口靠左固定宽度，目标应用窗口铺满右侧（SetWindowPos）。
//!
//! 坐标为虚拟桌面物理像素（进程 Per-Monitor-V2 DPI 感知）。「400」按逻辑
//! 像素处理，乘以天工窗口所在显示器的缩放比例。Win10/11 窗口带透明的
//! 缩放边框，`SetWindowPos` 作用于含边框的窗口矩形，需按 DWM 可见边框
//! 补偿，否则两窗口之间会出现缝隙。Snap Layouts 没有公开 API，不使用。

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, HWND_TOP, IsIconic, IsZoomed, SW_MAXIMIZE, SW_RESTORE, SWP_NOACTIVATE,
    SWP_NOZORDER, SetWindowPos, ShowWindow,
};

use tiangong_plugin_computer_use_protocol::Bounds;
use tiangong_plugin_computer_use_protocol::ops::{SPLIT_HOST_WIDTH, SplitOutcome, split_layout};

use super::win_desktop::{enum_top_windows, window_bounds};
use crate::split::SavedHostFrame;

/// 实际位置与目标的容差（物理像素）。
const TOLERANCE: f64 = 4.0;

fn host_pid() -> Option<u32> {
    std::env::var("TIANGONG_PLUGIN_HOST_PID")
        .ok()?
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
}

/// 进程最前的可见顶层窗口。
fn top_window_of(pid: u32) -> Option<HWND> {
    enum_top_windows()
        .into_iter()
        .find(|window| window.pid == pid)
        .map(|window| window.hwnd)
}

/// 窗口所在显示器的工作区（扣除任务栏）。
fn work_area_of(hwnd: HWND) -> Option<Bounds> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY：纯查询，info 可写。
    let ok = unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(monitor, &mut info).as_bool()
    };
    if !ok {
        return None;
    }
    let r = info.rcWork;
    Some(Bounds {
        x: f64::from(r.left),
        y: f64::from(r.top),
        width: f64::from(r.right - r.left),
        height: f64::from(r.bottom - r.top),
    })
}

/// 显示器缩放比例（96 DPI = 1.0）。
fn scale_of(hwnd: HWND) -> f64 {
    // SAFETY：纯查询。
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 }
}

/// 窗口矩形（含透明缩放边框）与可见边框的差值：(左, 上, 右, 下)。
fn invisible_border(hwnd: HWND) -> (f64, f64, f64, f64) {
    let mut rect = RECT::default();
    // SAFETY：rect 可写。
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let visible = window_bounds(hwnd);
    if visible.width <= 0.0 || visible.height <= 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    (
        (visible.x - f64::from(rect.left)).max(0.0),
        (visible.y - f64::from(rect.top)).max(0.0),
        (f64::from(rect.right) - (visible.x + visible.width)).max(0.0),
        (f64::from(rect.bottom) - (visible.y + visible.height)).max(0.0),
    )
}

/// 把窗口的可见边框放到目标矩形。
fn place(hwnd: HWND, frame: Bounds) -> Result<Bounds, String> {
    // SAFETY：对他进程窗口的标准调整调用；句柄无效时系统返回错误。
    unsafe {
        if IsZoomed(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    }
    let (left, top, right, bottom) = invisible_border(hwnd);
    let x = (frame.x - left).round() as i32;
    let y = (frame.y - top).round() as i32;
    let width = (frame.width + left + right).round() as i32;
    let height = (frame.height + top + bottom).round() as i32;
    // SAFETY：同上。
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
    }
    .map_err(|error| format!("调整窗口失败（目标可能以管理员身份运行）：{error}"))?;
    Ok(window_bounds(hwnd))
}

fn close_to(actual: Bounds, expected: Bounds) -> bool {
    (actual.x - expected.x).abs() <= TOLERANCE
        && (actual.y - expected.y).abs() <= TOLERANCE
        && (actual.width - expected.width).abs() <= TOLERANCE
        && (actual.height - expected.height).abs() <= TOLERANCE
}

/// 打开应用后执行自动分屏。
pub(crate) fn split_with_host(target_hwnd: HWND, target_pid: u32) -> SplitOutcome {
    let skip = |detail: &str| SplitOutcome {
        applied: false,
        detail: detail.to_string(),
        ..Default::default()
    };
    let Some(host_pid) = host_pid() else {
        return skip("未获取到天工窗口进程，未自动分屏");
    };
    if host_pid == target_pid {
        return skip("目标就是天工自身，未自动分屏");
    }
    let Some(host) = top_window_of(host_pid) else {
        return skip("未找到天工主窗口，未自动分屏");
    };
    // SAFETY：纯查询。
    let host_maximized = unsafe { IsZoomed(host).as_bool() };
    let host_before = window_bounds(host);
    let Some(work_area) = work_area_of(host) else {
        return skip("无法读取显示器工作区，未自动分屏");
    };
    let host_width = (SPLIT_HOST_WIDTH * scale_of(host)).round();
    let Some((host_frame, target_frame)) = split_layout(work_area, host_width) else {
        return skip("屏幕宽度不足以分屏，保持原窗口布局");
    };
    let host_actual = match place(host, host_frame) {
        Ok(actual) => actual,
        Err(reason) => return skip(&format!("天工窗口{reason}，未自动分屏")),
    };
    crate::split::record_applied(Some(SavedHostFrame {
        bounds: host_before,
        maximized: host_maximized,
        fullscreen: false,
    }));
    let target_result = place(target_hwnd, target_frame);
    let target_actual = target_result.as_ref().ok().copied();
    let all_ok = close_to(host_actual, host_frame)
        && target_actual.is_some_and(|actual| close_to(actual, target_frame));
    let detail = match (&target_result, all_ok) {
        (Err(reason), _) => format!("天工已移到左侧，但目标窗口{reason}"),
        (Ok(_), true) => "已分屏：天工在左侧，目标应用在右侧".to_string(),
        (Ok(_), false) => "已分屏，但应用限制了窗口尺寸，实际位置与目标略有差异".to_string(),
    };
    SplitOutcome {
        applied: true,
        host: Some(host_actual),
        target: target_actual,
        detail,
    }
}

/// 恢复天工窗口到分屏前的位置（不调整其他应用）。
pub(crate) fn restore_host(saved: SavedHostFrame) -> Result<(), String> {
    let host_pid = host_pid().ok_or("未获取到天工窗口进程")?;
    let host = top_window_of(host_pid).ok_or("未找到天工主窗口")?;
    place(host, saved.bounds)?;
    if saved.maximized {
        // SAFETY：标准窗口状态调用。
        unsafe {
            let _ = ShowWindow(host, SW_MAXIMIZE);
        }
    }
    Ok(())
}
