//! Windows 坐标级键鼠合成输入（SendInput，与 macOS CGEvent 手势同语义）。
//!
//! 坐标系：sidecar 启动时声明 Per-Monitor-V2 DPI 感知，UIA bounds、
//! `SetCursorPos` 与截图 `logical_bounds` 统一为虚拟桌面物理像素（主屏
//! 左上原点，副屏可为负），无需换算。
//!
//! 系统鼠标「借用并归还」：click 手势 = 天工指针平滑滑行到目标（同步等待
//! 到达）→ 到位停顿 → 系统鼠标闪移到目标完成 down/up → 闪移归还。drag
//! 轨迹必须由系统指针承载，指针沿途跟随。滚轮投递给指针下的窗口，同样
//! 借用系统鼠标后归还。
//!
//! 键盘：`type` 以 `KEYEVENTF_UNICODE` 逐 UTF-16 单元分派（不经输入法、不依赖
//! 键盘布局，中文/表情同路径）；`key`/`combo` 走虚拟键码，扩展键带
//! `KEYEVENTF_EXTENDEDKEY`。`cmd` 在 Windows 上等同 Ctrl（跨平台快捷键
//! `cmd+c` 直接可用），Windows 徽标键用 `win`。
//!
//! UIPI：无法向更高完整性级别（如管理员运行）的窗口注入输入，系统会静默
//! 丢弃；`SendInput` 返回值不足时明确报错。
//!
//! 串行化：所有键鼠手势经进程级 [`INPUT_LOCK`] 互斥执行。宿主可能并发下发
//! 同批次的多个工具调用，若不互斥，`combo` 按住修饰键期间插入的单键会变成
//! 组合键（如 Ctrl 按住时的 Enter），表现为按键「丢失」。
use std::mem::size_of;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetCursorPos,
};

use tiangong_plugin_computer_use_protocol::ops::{KeyboardActionKind, MouseGesture};

use super::input_cancel::{CancelToken, cancelled_message};
use super::keys::{combo_keys, key_symbol, normalize_key_name};
use super::win_overlay as overlay;

/// down → up 间隔：足够应用完成点击归一（不触成长按）。
const CLICK_DOWN_UP: Duration = Duration::from_millis(60);
/// 指针滑到目标后的停顿。
const CLICK_SETTLE: Duration = Duration::from_millis(60);
/// 系统鼠标闪移后等待命中测试更新（悬停态/窗口激活）。
const WARP_SETTLE: Duration = Duration::from_millis(15);
/// 双击两段间隔（小于系统双击时间 500ms 默认值）。
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(90);
/// drag：down 后先停顿（应用建立拖拽会话）再开始轨迹。
const DRAG_SETTLE: Duration = Duration::from_millis(60);
/// drag 轨迹：步长与步进间隔。
const DRAG_STEP_PX: f64 = 12.0;
const DRAG_STEP_INTERVAL: Duration = Duration::from_millis(12);
/// 右键抬起后到归还系统鼠标的等待：多数应用在处理 WM_CONTEXTMENU 时按
/// 「当前」鼠标位置弹出菜单（WinUI/XAML 应用还是异步弹出），过早归还会让
/// 菜单出现在用户原来的鼠标位置。
const CONTEXT_MENU_SETTLE: Duration = Duration::from_millis(350);
/// 滚轮投递后到归还系统鼠标的等待（输入线程完成目标窗口判定）。
const SCROLL_SETTLE: Duration = Duration::from_millis(50);
/// 滚轮：100 像素折算为一格（WHEEL_DELTA=120）。
const WHEEL_DELTA: f64 = 120.0;
const PIXELS_PER_NOTCH: f64 = 100.0;

/// key 手势 down → up 间隔。
const KEY_DOWN_UP: Duration = Duration::from_millis(50);
/// type 手势相邻字符间隔。
const TYPE_GAP: Duration = Duration::from_millis(15);
/// combo 修饰键相邻事件间隔。
const MOD_GAP: Duration = Duration::from_millis(20);

/// 键鼠手势互斥锁：同一时刻只执行一个手势，事件不交错。
static INPUT_LOCK: Mutex<()> = Mutex::new(());

/// 获取手势锁；前一个手势 panic 导致中毒时照常继续（锁内无共享数据）。
fn input_guard() -> std::sync::MutexGuard<'static, ()> {
    INPUT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 批量投递输入事件；系统拦截（UIPI、安全桌面）时返回错误。
fn send(inputs: &[INPUT]) -> Result<(), String> {
    // SAFETY：inputs 为有效的 INPUT 切片，cbsize 与结构体大小一致。
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(format!(
            "SendInput 仅投递 {sent}/{} 个事件：{}（目标可能是以管理员身份运行的窗口或安全桌面，普通权限无法注入输入）",
            inputs.len(),
            windows::core::Error::from_win32()
        ))
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                // mouseData 为 DWORD，滚轮负值按补码传递。
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_button(flags: MOUSE_EVENT_FLAGS) -> Result<(), String> {
    send(&[mouse_input(0, 0, 0, flags)])
}

/// 虚拟桌面矩形（x, y, 宽, 高）。
fn virtual_desktop() -> (f64, f64, f64, f64) {
    // SAFETY：纯查询。
    unsafe {
        (
            f64::from(GetSystemMetrics(SM_XVIRTUALSCREEN)),
            f64::from(GetSystemMetrics(SM_YVIRTUALSCREEN)),
            f64::from(GetSystemMetrics(SM_CXVIRTUALSCREEN)),
            f64::from(GetSystemMetrics(SM_CYVIRTUALSCREEN)),
        )
    }
}

/// 屏幕像素 → SendInput 绝对坐标（虚拟桌面归一到 0..=65535）。
fn normalize_absolute(value: f64, origin: f64, extent: f64) -> i32 {
    let span = (extent - 1.0).max(1.0);
    (((value - origin) * 65535.0 / span).round() as i32).clamp(0, 65535)
}

/// 以绝对坐标投递一次移动事件（drag 轨迹用，拖拽会话需要真实 move 事件）。
fn absolute_move(x: f64, y: f64) -> Result<(), String> {
    let (vx, vy, vw, vh) = virtual_desktop();
    send(&[mouse_input(
        normalize_absolute(x, vx, vw),
        normalize_absolute(y, vy, vh),
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )])
}

fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    // SAFETY：point 为有效可写指针。
    unsafe { GetCursorPos(&mut point) }
        .ok()
        .map(|()| (point.x, point.y))
}

/// 系统鼠标闪移到目标。`SetCursorPos` 被拒绝（如受限令牌、桌面切换）时
/// 回退为 `SendInput` 绝对移动事件，两者都失败才报错。
fn warp(x: f64, y: f64) -> Result<(), String> {
    // SAFETY：纯设置调用。
    match unsafe { SetCursorPos(x.round() as i32, y.round() as i32) } {
        Ok(()) => Ok(()),
        Err(primary) => absolute_move(x, y).map_err(|fallback| {
            format!("移动系统鼠标失败：{primary}；回退 SendInput 亦失败：{fallback}")
        }),
    }
}

fn restore_cursor(origin: Option<(i32, i32)>) {
    if let Some((x, y)) = origin {
        // SAFETY：纯设置调用；归还失败不影响已完成的动作。
        let _ = unsafe { SetCursorPos(x, y) };
    }
}

/// 滚动像素 → 滚轮单位：100 像素一格，不足一格按一格（部分旧应用忽略
/// 小于 120 的增量）。
fn wheel_units(pixels: f64) -> i32 {
    if pixels == 0.0 {
        return 0;
    }
    let units = (pixels / PIXELS_PER_NOTCH * WHEEL_DELTA).round();
    let units = if units.abs() < WHEEL_DELTA {
        WHEEL_DELTA.copysign(pixels)
    } else {
        units
    };
    units as i32
}

/// 执行一次鼠标手势；成功返回人读摘要。
pub fn perform_mouse(
    gesture: MouseGesture,
    x: f64,
    y: f64,
    to: Option<(f64, f64)>,
    scroll: (f64, f64),
    cancel: &CancelToken,
) -> Result<String, String> {
    let _guard = input_guard();
    // 排队等锁期间可能已被取消：拿到锁后先检查。
    if cancel.is_cancelled() {
        return Err(cancelled_message("鼠标手势"));
    }
    match gesture {
        MouseGesture::Move => {
            // 纯虚拟移动：只动天工指针，系统鼠标不动（hover 指示语义）。
            overlay::move_to(x, y);
            Ok(format!("指针已移动到 ({x:.0}, {y:.0})"))
        }
        MouseGesture::Click | MouseGesture::RightClick | MouseGesture::DoubleClick => {
            overlay::glide_and_wait(x, y);
            sleep(CLICK_SETTLE);
            let origin = cursor_position();
            let (down, up) = if matches!(gesture, MouseGesture::RightClick) {
                (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
            } else {
                (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
            };
            let result = (|| {
                warp(x, y)?;
                sleep(WARP_SETTLE);
                mouse_button(down)?;
                sleep(CLICK_DOWN_UP);
                mouse_button(up)?;
                overlay::click_pulse(x, y);
                if matches!(gesture, MouseGesture::DoubleClick) {
                    sleep(DOUBLE_CLICK_GAP);
                    mouse_button(MOUSEEVENTF_LEFTDOWN)?;
                    sleep(CLICK_DOWN_UP);
                    mouse_button(MOUSEEVENTF_LEFTUP)?;
                    overlay::click_pulse(x, y);
                }
                if matches!(gesture, MouseGesture::RightClick) {
                    sleep(CONTEXT_MENU_SETTLE);
                }
                Ok::<(), String>(())
            })();
            restore_cursor(origin);
            result?;
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
            let origin = cursor_position();
            overlay::glide_and_wait(x, y);
            let result = (|| {
                warp(x, y)?;
                sleep(DRAG_SETTLE);
                mouse_button(MOUSEEVENTF_LEFTDOWN)?;
                sleep(DRAG_SETTLE);
                let distance = (tx - x).hypot(ty - y).max(1.0);
                let steps = ((distance / DRAG_STEP_PX).ceil() as usize).clamp(2, 120);
                for step in 1..=steps {
                    // 取消时在当前位置抬起（外层统一补发 LEFTUP 并归还）。
                    if cancel.is_cancelled() {
                        return Err(cancelled_message("拖拽"));
                    }
                    let progress = step as f64 / steps as f64;
                    let ease = progress * (2.0 - progress); // ease-out 轨迹
                    let nx = x + (tx - x) * ease;
                    let ny = y + (ty - y) * ease;
                    absolute_move(nx, ny)?;
                    overlay::move_to(nx, ny);
                    sleep(DRAG_STEP_INTERVAL);
                }
                warp(tx, ty)?;
                mouse_button(MOUSEEVENTF_LEFTUP)
            })();
            if result.is_err() {
                // 任何失败都不允许遗留按住状态。
                let _ = mouse_button(MOUSEEVENTF_LEFTUP);
            }
            restore_cursor(origin);
            result?;
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
            let origin = cursor_position();
            let result = (|| {
                warp(x, y)?;
                sleep(WARP_SETTLE);
                let mut inputs = Vec::with_capacity(2);
                // 协议 delta_y 正=向下；滚轮正值=向上（远离用户），取反。
                let vertical = wheel_units(-delta_y);
                if vertical != 0 {
                    inputs.push(mouse_input(0, 0, vertical, MOUSEEVENTF_WHEEL));
                }
                // 水平：正=向右，与 HWHEEL 同向。
                let horizontal = wheel_units(delta_x);
                if horizontal != 0 {
                    inputs.push(mouse_input(0, 0, horizontal, MOUSEEVENTF_HWHEEL));
                }
                send(&inputs)?;
                sleep(SCROLL_SETTLE);
                Ok::<(), String>(())
            })();
            restore_cursor(origin);
            result?;
            Ok(format!(
                "已在 ({x:.0}, {y:.0}) 滚动 (Δy={delta_y:.0}, Δx={delta_x:.0})"
            ))
        }
    }
}

// ── 键盘 ──────────────────────────────────────────────────────

/// 修饰键虚拟键码（左右区分；cmd 等同 Ctrl）。
fn modifier_vk(name: &str) -> Option<u16> {
    Some(match name {
        "cmd" | "ctrl" => 0xA2,   // VK_LCONTROL
        "rcmd" | "rctrl" => 0xA3, // VK_RCONTROL
        "shift" => 0xA0,          // VK_LSHIFT
        "rshift" => 0xA1,         // VK_RSHIFT
        "alt" => 0xA4,            // VK_LMENU
        "ralt" => 0xA5,           // VK_RMENU
        "win" => 0x5B,            // VK_LWIN
        _ => return None,
    })
}

/// 非修饰键虚拟键码（US 布局）。
fn plain_vk(name: &str) -> Option<u16> {
    let vk = match name {
        "return" => 0x0D,
        "tab" => 0x09,
        "space" => 0x20,
        "delete" => 0x08,
        "forward_delete" => 0x2E,
        "insert" => 0x2D,
        "escape" => 0x1B,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "home" => 0x24,
        "end" => 0x23,
        "page_up" => 0x21,
        "page_down" => 0x22,
        "help" => 0x2F,
        other => {
            let bytes = other.as_bytes();
            return match bytes {
                [c @ b'a'..=b'z'] => Some(u16::from(c.to_ascii_uppercase())),
                [c @ b'0'..=b'9'] => Some(u16::from(*c)),
                [b'f', rest @ ..] => std::str::from_utf8(rest)
                    .ok()
                    .and_then(|n| n.parse::<u16>().ok())
                    .filter(|n| (1..=24).contains(n))
                    .map(|n| 0x6F + n), // VK_F1 = 0x70
                _ => None,
            };
        }
    };
    Some(vk)
}

/// 需要 `KEYEVENTF_EXTENDEDKEY` 的虚拟键（方向、导航、右侧修饰键、Win）。
fn is_extended(vk: u16) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0xA3 | 0xA5 | 0x5B | 0x5C)
}

fn key_input(vk: u16, key_down: bool) -> INPUT {
    // SAFETY：纯查询。
    let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) } as u16;
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !key_down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_input(unit: u16, key_down: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if !key_down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn tap(vk: u16) -> Result<(), String> {
    send(&[key_input(vk, true)])?;
    sleep(KEY_DOWN_UP);
    send(&[key_input(vk, false)])
}

/// 校验键名（测试用）。
#[cfg(test)]
pub(crate) fn is_known_key(name: &str) -> bool {
    let normalized = normalize_key_name(name);
    modifier_vk(&normalized).is_some() || plain_vk(&normalized).is_some()
}

/// 执行一次键盘手势；成功返回人读摘要。
pub fn perform_keyboard(
    action: KeyboardActionKind,
    text: Option<String>,
    key: Option<String>,
    keys: Option<Vec<String>>,
    cancel: &CancelToken,
) -> Result<String, String> {
    let _guard = input_guard();
    if cancel.is_cancelled() {
        return Err(cancelled_message("键盘输入"));
    }
    match action {
        KeyboardActionKind::Type => {
            let text = text
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or("type 缺少 text")?;
            let total = text.chars().count();
            for (typed, ch) in text.chars().enumerate() {
                // 每个字符前检查取消；down/up 成对投递，不留按住状态。
                if cancel.is_cancelled() {
                    return Err(format!(
                        "{}，已输入 {typed}/{total} 个字符",
                        cancelled_message("文本输入")
                    ));
                }
                match ch {
                    // 换行/制表按真实按键发送（Unicode 分派在部分控件不换行）。
                    '\n' => tap(0x0D)?,
                    '\t' => tap(0x09)?,
                    '\r' => continue,
                    _ => {
                        let mut buf = [0u16; 2];
                        let units = ch.encode_utf16(&mut buf);
                        // 代理对的两个单元须连续投递，应用才能组合成一个字符。
                        let downs: Vec<INPUT> =
                            units.iter().map(|u| unicode_input(*u, true)).collect();
                        let ups: Vec<INPUT> =
                            units.iter().map(|u| unicode_input(*u, false)).collect();
                        send(&downs)?;
                        sleep(KEY_DOWN_UP);
                        send(&ups)?;
                    }
                }
                sleep(TYPE_GAP);
            }
            Ok(format!("已输入 {total} 个字符"))
        }
        KeyboardActionKind::Key => {
            let name = key
                .as_deref()
                .map(normalize_key_name)
                .filter(|n| !n.is_empty())
                .ok_or("key 缺少键名")?;
            let vk = plain_vk(&name)
                .ok_or_else(|| format!("不支持的键名: {name}（修饰键请用 combo）"))?;
            overlay::key_cast(vec![key_symbol(&name)]);
            tap(vk)?;
            Ok(format!("已按 {name}"))
        }
        KeyboardActionKind::Combo => {
            let keys = keys.ok_or("combo 缺少 keys")?;
            let names: Vec<String> = keys
                .iter()
                .map(|k| normalize_key_name(k))
                .filter(|n| !n.is_empty())
                .collect();
            if names.len() < 2 {
                return Err("combo 至少需要两个键（修饰键 + 普通键）".to_string());
            }
            let modifiers: Vec<u16> = names.iter().filter_map(|n| modifier_vk(n)).collect();
            let plains: Vec<&String> = names.iter().filter(|n| modifier_vk(n).is_none()).collect();
            if plains.len() != 1 {
                return Err(format!(
                    "combo 需要恰好一个非修饰键（当前 {} 个）；单独按修饰键没有意义",
                    plains.len()
                ));
            }
            let plain_name = plains[0].clone();
            let plain =
                plain_vk(&plain_name).ok_or_else(|| format!("combo 不支持的键名: {plain_name}"))?;
            overlay::key_cast(combo_keys(&names));
            // 修饰键 down（保持给定顺序）→ 普通键 down/up → 修饰键逆序 up；
            // 中途失败也必须释放已按下的修饰键，避免系统卡在按住状态。
            let mut pressed: Vec<u16> = Vec::new();
            let result = (|| {
                for vk in &modifiers {
                    if cancel.is_cancelled() {
                        return Err(cancelled_message("组合键"));
                    }
                    send(&[key_input(*vk, true)])?;
                    pressed.push(*vk);
                    sleep(MOD_GAP);
                }
                if cancel.is_cancelled() {
                    return Err(cancelled_message("组合键"));
                }
                tap(plain)
            })();
            for vk in pressed.iter().rev() {
                sleep(MOD_GAP);
                let _ = send(&[key_input(*vk, false)]);
            }
            result?;
            Ok(format!("已执行组合键 {}", names.join("+")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_keys_cover_letters_digits_function_and_modifiers() {
        for k in [
            "a", "Z", "0", "9", "return", "esc", "f5", "F12", "up", "del",
        ] {
            assert!(is_known_key(k), "{k} 应识别");
        }
        for k in ["cmd", "ctrl", "shift", "alt", "win", "rctrl"] {
            assert!(is_known_key(k), "{k} 应识别为修饰键");
        }
        for k in ["hup", "", "f0", "f25", "fn"] {
            assert!(!is_known_key(k), "{k} 应拒绝");
        }
    }

    #[test]
    fn virtual_key_codes_match_win32_table() {
        assert_eq!(plain_vk("a"), Some(0x41));
        assert_eq!(plain_vk("z"), Some(0x5A));
        assert_eq!(plain_vk("0"), Some(0x30));
        assert_eq!(plain_vk("f1"), Some(0x70));
        assert_eq!(plain_vk("f12"), Some(0x7B));
        assert_eq!(modifier_vk("cmd"), modifier_vk("ctrl"));
        assert!(is_extended(plain_vk("left").unwrap()));
        assert!(is_extended(modifier_vk("win").unwrap()));
        assert!(!is_extended(plain_vk("a").unwrap()));
    }

    #[test]
    fn wheel_units_round_to_notches_and_keep_direction() {
        assert_eq!(wheel_units(0.0), 0);
        assert_eq!(wheel_units(100.0), 120);
        assert_eq!(wheel_units(-300.0), -360);
        // 不足一格按一格。
        assert_eq!(wheel_units(10.0), 120);
        assert_eq!(wheel_units(-10.0), -120);
    }

    #[test]
    fn absolute_coordinates_span_virtual_desktop() {
        assert_eq!(normalize_absolute(0.0, 0.0, 1920.0), 0);
        assert_eq!(normalize_absolute(1919.0, 0.0, 1920.0), 65535);
        // 副屏在主屏左侧（虚拟桌面原点为负）。
        assert_eq!(normalize_absolute(-1920.0, -1920.0, 3840.0), 0);
        assert_eq!(normalize_absolute(5000.0, 0.0, 1920.0), 65535);
    }
}
