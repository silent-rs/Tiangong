//! 坐标级鼠标手势（RFC 0018 §2.3，仅 macOS）：CGEvent 合成输入。
//!
//! 手势而非裸事件是暴露单元：`down`/`up` 永不单独出现，`drag` 由本层
//! 生成插值轨迹（步进 move）保证拖拽会话的时序要求，杜绝跨工具调用
//! 的按住状态泄漏。坐标与 AX/CGDisplay 同系（主屏左上原点，points），
//! 无需翻转。合成事件需要辅助功能授权（与 AX 动作同一 TCC 授权）。
//!
//! 系统鼠标「借用并归还」，虚拟指针为视觉主角：click 手势 = 虚拟指针
//! 平滑滑行到目标（同步等待到达）→ 到位停顿 → 系统鼠标闪移借用完成
//! down/up（down/up 事件自带目标坐标——窗口激活、上下文菜单与真人
//! 完全一致；只有点击这一瞬用到系统鼠标）→ 闪移归还。drag 的轨迹
//! 本身必须由系统指针承载（拖拽会话跟随系统指针），虚拟指针沿途跟随。
//! 滚轮无激活依赖，经 AX 定位目标进程后定向投递、系统鼠标不动。
use std::thread::sleep;
use std::time::Duration;

use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use tiangong_plugin_computer_use_protocol::ops::MouseGesture;

use super::input_cancel::{CancelToken, cancelled_message};
use super::overlay;

/// down → up 间隔：足够应用完成点击归一（不触成长按）。
const CLICK_DOWN_UP: Duration = Duration::from_millis(60);
/// 指针滑到目标后的停顿：视觉上「到位—再按」，也与真人操作一致。
const CLICK_SETTLE: Duration = Duration::from_millis(60);
/// 双击两段间隔。
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(90);
/// drag：down 后先停顿（应用建立拖拽会话）再开始轨迹。
const DRAG_SETTLE: Duration = Duration::from_millis(60);
/// drag 轨迹：步长与步进间隔（≈80px/100ms，模拟真实拖拽速度）。
const DRAG_STEP_PX: f64 = 12.0;
const DRAG_STEP_INTERVAL: Duration = Duration::from_millis(12);

/// 最近一次主线程缓存的系统鼠标位置（AX 坐标），供 drag 归还。
/// overlay 主循环每帧刷新；进程早期未启动 overlay 时回退 (0,0)。
static LAST_SYSTEM_CURSOR: std::sync::Mutex<(f64, f64)> = std::sync::Mutex::new((0.0, 0.0));

pub(crate) fn cache_system_cursor(x: f64, y: f64) {
    *LAST_SYSTEM_CURSOR.lock().unwrap() = (x, y);
}

fn current_cursor_position() -> (f64, f64) {
    *LAST_SYSTEM_CURSOR.lock().unwrap()
}

struct MouseIo {
    source: CGEventSource,
}

impl MouseIo {
    fn new() -> Result<Self, String> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|()| "创建 CGEventSource 失败".to_string())?;
        Ok(Self { source })
    }
    fn post(
        &self,
        event_type: CGEventType,
        button: CGMouseButton,
        x: f64,
        y: f64,
    ) -> Result<(), String> {
        let event =
            CGEvent::new_mouse_event(self.source.clone(), event_type, CGPoint::new(x, y), button)
                .map_err(|()| "创建鼠标事件失败".to_string())?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
    fn move_to(&self, x: f64, y: f64) -> Result<(), String> {
        self.post(CGEventType::MouseMoved, CGMouseButton::Left, x, y)
    }
    /// 借用序列的按压段：down → 间隔 → up（不带移动；调用方在虚拟
    /// 指针滑行到位后调用，双击在同一次借用内连续两次）。
    fn press_release(&self, x: f64, y: f64, button: CGMouseButton) -> Result<(), String> {
        let (down, up) = match button {
            CGMouseButton::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp),
            _ => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp),
        };
        self.post(down, button, x, y)?;
        sleep(CLICK_DOWN_UP);
        self.post(up, button, x, y)
    }
}

/// 执行一次手势；成功返回人读摘要。`cancel` 置位后不再开始手势，
/// 拖拽轨迹中途停止并补发抬起。
pub fn perform(
    gesture: MouseGesture,
    x: f64,
    y: f64,
    to: Option<(f64, f64)>,
    scroll: (f64, f64),
    cancel: &CancelToken,
) -> Result<String, String> {
    // 与键盘共用手势锁：并发调用时点击/拖拽不与按键事件交错。
    let _guard = super::keyboard::input_guard();
    if cancel.is_cancelled() {
        return Err(cancelled_message("鼠标手势"));
    }
    let io = MouseIo::new()?;
    match gesture {
        MouseGesture::Move => {
            // 纯虚拟移动：只动天工指针，系统鼠标不动（hover 指示语义）。
            overlay::move_to(x, y);
            Ok(format!("指针已移动到 ({x:.0}, {y:.0})"))
        }
        MouseGesture::Click | MouseGesture::RightClick | MouseGesture::DoubleClick => {
            // 虚拟指针是视觉主角：平滑滑行到目标并**等待真正到达**（未
            // 开启/未运行则跳过等待），到位停顿后系统鼠标闪移借用完成
            // down/up（down/up 事件自带目标坐标），再闪移归还——只有
            // 点击这一瞬用到系统鼠标。
            overlay::glide_and_wait(x, y);
            sleep(CLICK_SETTLE);
            let (origin_x, origin_y) = current_cursor_position();
            let button = if matches!(gesture, MouseGesture::RightClick) {
                CGMouseButton::Right
            } else {
                CGMouseButton::Left
            };
            io.press_release(x, y, button)?;
            overlay::click_pulse(x, y);
            if matches!(gesture, MouseGesture::DoubleClick) {
                sleep(DOUBLE_CLICK_GAP);
                io.press_release(x, y, CGMouseButton::Left)?;
                overlay::click_pulse(x, y);
            }
            io.move_to(origin_x, origin_y)?;
            let label = match gesture {
                MouseGesture::RightClick => "右键",
                MouseGesture::DoubleClick => "双击",
                _ => "左键",
            };
            Ok(format!(
                "已在 ({x:.0}, {y:.0}) 执行{label}点击（系统鼠标已归位）"
            ))
        }
        MouseGesture::Drag => {
            let (tx, ty) = to.ok_or("drag 缺少 to_x/to_y 终点")?;
            // 真实拖拽会话必须跟随系统指针：记录起点，结束后归还。
            let (origin_x, origin_y) = current_cursor_position();
            // 虚拟指针先平滑滑到起点并等待到位（视觉预告），随后系统
            // 鼠标闪移借用：down → 停顿建立拖拽会话 → 插值轨迹（拖拽
            // 本身必须由系统指针承载，虚拟指针沿途跟随）→ up → 闪移归还。
            overlay::glide_and_wait(x, y);
            io.move_to(x, y)?;
            sleep(DRAG_SETTLE);
            // down → 停顿建立拖拽会话 → 插值轨迹 → up → 归还系统鼠标。
            io.post(CGEventType::LeftMouseDown, CGMouseButton::Left, x, y)?;
            sleep(DRAG_SETTLE);
            let distance = ((tx - x).hypot(ty - y)).max(1.0);
            let steps = ((distance / DRAG_STEP_PX).ceil() as usize).clamp(2, 120);
            let mut last = (x, y);
            let mut interrupted = false;
            for step in 1..=steps {
                // 取消时在当前位置抬起并归还，不留按住状态。
                if cancel.is_cancelled() {
                    interrupted = true;
                    break;
                }
                let progress = step as f64 / steps as f64;
                let ease = progress * (2.0 - progress); // ease-out 轨迹
                let nx = x + (tx - x) * ease;
                let ny = y + (ty - y) * ease;
                io.post(CGEventType::LeftMouseDragged, CGMouseButton::Left, nx, ny)?;
                overlay::move_to(nx, ny);
                last = (nx, ny);
                sleep(DRAG_STEP_INTERVAL);
            }
            io.post(
                CGEventType::LeftMouseUp,
                CGMouseButton::Left,
                last.0,
                last.1,
            )?;
            // 闪移归还：系统鼠标瞬移回拖拽前的位置，用户指针无感。
            io.move_to(origin_x, origin_y)?;
            if interrupted {
                return Err(cancelled_message("拖拽"));
            }
            Ok(format!(
                "已从 ({x:.0}, {y:.0}) 拖拽到 ({tx:.0}, {ty:.0})（系统鼠标已归位）"
            ))
        }
        MouseGesture::Scroll => {
            let (delta_y, delta_x) = scroll;
            if delta_y == 0.0 && delta_x == 0.0 {
                return Err("scroll 缺少 delta_y/delta_x".to_string());
            }
            overlay::move_to(x, y);
            let event = CGEvent::new_scroll_event(
                io.source.clone(),
                ScrollEventUnit::PIXEL,
                2,
                delta_y.round() as i32,
                delta_x.round() as i32,
                0,
            )
            .map_err(|()| "创建滚轮事件失败".to_string())?;
            event.set_location(CGPoint::new(x, y));
            // 定向投递给坐标处进程（系统鼠标不动）；无 AX 命中走 HID 全局。
            match super::ax::pid_at_position(x, y) {
                Some(pid) => event.post_to_pid(pid),
                None => event.post(CGEventTapLocation::HID),
            }
            Ok(format!(
                "已在 ({x:.0}, {y:.0}) 滚动 (Δy={delta_y:.0}, Δx={delta_x:.0})"
            ))
        }
    }
}
