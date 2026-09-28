//! macOS 自动分屏：天工窗口靠左固定宽度，目标应用窗口铺满右侧（AX 设置 frame）。
//!
//! 系统 Split View / 窗口平铺没有公开 API 且只支持固定比例，这里直接用
//! `AXPosition` / `AXSize` 摆放两个窗口，坐标均为 AX 全局坐标（主屏左上
//! 原点，points）。屏幕可用区域（扣除菜单栏与 Dock）由 overlay 主循环在
//! 主线程代查。需要辅助功能授权；未授权或窗口不可移动时如实返回原因。

use std::time::Duration;

use tiangong_plugin_computer_use_protocol::Bounds;
use tiangong_plugin_computer_use_protocol::ops::{SPLIT_HOST_WIDTH, SplitOutcome, split_layout};

use super::ax::{self, AxElement};
use crate::split::SavedHostFrame;

/// 退出全屏动画的等待时长。
const FULLSCREEN_EXIT_WAIT: Duration = Duration::from_millis(900);
/// 实际位置与目标的容差（points）：应用可能按自身步进对齐尺寸。
const TOLERANCE: f64 = 4.0;

/// 天工宿主进程 PID（sidecar 由宿主直接拉起，环境变量即主窗口所在进程）。
fn host_pid() -> Option<i32> {
    std::env::var("TIANGONG_PLUGIN_HOST_PID")
        .ok()?
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 1)
}

/// 应用的主窗口：优先 AXMainWindow，其次 AXFocusedWindow，最后第一个窗口。
fn main_window(pid: i32) -> Option<AxElement> {
    let app = AxElement::for_application(pid);
    app.element_attribute("AXMainWindow")
        .or_else(|| app.element_attribute("AXFocusedWindow"))
        .or_else(|| {
            app.elements_attribute("AXWindows")
                .ok()
                .and_then(|windows| windows.into_iter().next())
        })
}

fn frame_of(window: &AxElement) -> Bounds {
    let b = window.bounds();
    Bounds {
        x: b.x,
        y: b.y,
        width: b.width,
        height: b.height,
    }
}

fn close_to(actual: Bounds, expected: Bounds) -> bool {
    (actual.x - expected.x).abs() <= TOLERANCE
        && (actual.y - expected.y).abs() <= TOLERANCE
        && (actual.width - expected.width).abs() <= TOLERANCE
        && (actual.height - expected.height).abs() <= TOLERANCE
}

/// 退出全屏（全屏窗口设置 frame 不生效）。返回原先是否全屏。
async fn leave_fullscreen(window: &AxElement) -> bool {
    if window.bool_attribute("AXFullScreen") == Some(true)
        && window.set_bool_attribute("AXFullScreen", false).is_ok()
    {
        tokio::time::sleep(FULLSCREEN_EXIT_WAIT).await;
        return true;
    }
    false
}

/// 设置窗口 frame：先位置后尺寸，再补一次位置（部分应用缩放后会自行挪位）。
///
/// 固定尺寸的窗口（如计算器）只移动到目标区域左上角；连位置都不可设置时
/// 返回错误。尺寸达不到目标由调用方读回核对后说明。
fn place(window: &AxElement, frame: Bounds) -> Result<Bounds, String> {
    if !window.is_attribute_settable("AXPosition") {
        return Err("不允许移动".to_string());
    }
    window
        .set_position(frame.x, frame.y)
        .map_err(|e| format!("设置位置失败：{}", e.message()))?;
    if window.is_attribute_settable("AXSize") {
        window
            .set_size(frame.width, frame.height)
            .map_err(|e| format!("设置尺寸失败：{}", e.message()))?;
        let _ = window.set_position(frame.x, frame.y);
    }
    Ok(frame_of(window))
}

/// 打开应用后执行自动分屏。
pub async fn split_with_host(target_pid: i32) -> SplitOutcome {
    let skip = |detail: &str| SplitOutcome {
        applied: false,
        detail: detail.to_string(),
        ..Default::default()
    };
    if !ax::is_process_trusted() {
        return skip("未授予辅助功能权限，未自动分屏");
    }
    let Some(host_pid) = host_pid() else {
        return skip("未获取到天工窗口进程，未自动分屏");
    };
    if host_pid == target_pid {
        return skip("目标就是天工自身，未自动分屏");
    }
    let Some(host_window) = main_window(host_pid) else {
        return skip("未找到天工主窗口，未自动分屏");
    };
    let Some(target_window) = main_window(target_pid) else {
        return skip("未找到目标应用窗口，未自动分屏");
    };

    // 分屏放在天工窗口当前所在的屏幕（按窗口中心定位）。
    let host_fullscreen = leave_fullscreen(&host_window).await;
    let host_before = frame_of(&host_window);
    let center = (
        host_before.x + host_before.width / 2.0,
        host_before.y + host_before.height / 2.0,
    );
    let Some((x, y, width, height)) = super::overlay::screen_work_area_at(center.0, center.1)
    else {
        return skip("无法读取屏幕可用区域，未自动分屏");
    };
    let work_area = Bounds {
        x,
        y,
        width,
        height,
    };
    let Some((host_frame, target_frame)) = split_layout(work_area, SPLIT_HOST_WIDTH) else {
        return skip("屏幕宽度不足以分屏，保持原窗口布局");
    };

    // 原位置只记录一次：天工已处于分屏布局时保留最初位置。
    let original = SavedHostFrame {
        bounds: host_before,
        maximized: false,
        fullscreen: host_fullscreen,
    };
    let host_actual = match place(&host_window, host_frame) {
        Ok(actual) => actual,
        Err(reason) => return skip(&format!("天工窗口{reason}，未自动分屏")),
    };
    crate::split::record_applied(Some(original));

    leave_fullscreen(&target_window).await;
    let target_result = place(&target_window, target_frame);
    let target_actual = target_result.as_ref().ok().copied();
    let host_ok = close_to(host_actual, host_frame);
    let target_ok = target_actual.is_some_and(|actual| close_to(actual, target_frame));
    let detail = match (&target_result, host_ok && target_ok) {
        (Err(reason), _) => format!("天工已移到左侧，但目标窗口{reason}"),
        (Ok(_), true) => "已分屏：天工在左侧，目标应用在右侧".to_string(),
        (Ok(_), false) => {
            "已分屏，但目标应用限制了窗口尺寸，已移到右侧区域并保持其尺寸".to_string()
        }
    };
    SplitOutcome {
        applied: true,
        host: Some(host_actual),
        target: target_actual,
        detail,
    }
}

/// 恢复天工窗口到分屏前的位置（不调整其他应用）。
pub async fn restore_host(saved: SavedHostFrame) -> Result<(), String> {
    let host_pid = host_pid().ok_or("未获取到天工窗口进程")?;
    let window = main_window(host_pid).ok_or("未找到天工主窗口")?;
    place(&window, saved.bounds)?;
    if saved.fullscreen {
        let _ = window.set_bool_attribute("AXFullScreen", true);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_to_allows_small_alignment_drift() {
        let expected = Bounds {
            x: 400.0,
            y: 25.0,
            width: 1112.0,
            height: 920.0,
        };
        let drift = Bounds {
            width: 1110.0,
            ..expected
        };
        assert!(close_to(drift, expected));
        let clamped = Bounds {
            width: 1000.0,
            ..expected
        };
        assert!(!close_to(clamped, expected));
    }
}
