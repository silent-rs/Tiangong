//! 坐标级键盘合成输入（RFC 0018 §2.4，仅 macOS）：CGEvent 键盘事件。
//!
//! 与 desktop_mouse 同属 CGEvent 合成输入族：键盘事件投递给当前焦点
//! 应用（与真人一致），不影响系统鼠标，无借用归还问题。输入框焦点
//! 由 Agent 先用 desktop_mouse click 建立。
//!
//! 三个手势而非裸事件：
//! - `type`：逐字符 Unicode 字符串分派（`CGEventKeyboardSetUnicodeString`），
//!   不经输入法组字、不依赖物理布局，中文/表情与 ASCII 同路径；
//! - `key`：单键名 → 虚拟键码（HIToolbox ANSI 表，US 布局）；
//! - `combo`：修饰键 down → 普通键 down/up → 修饰键逆序 up 的真实
//!   HID 序列（部分应用监听修饰键本身，与点击手势同一真实管线路由）。
//!
//! 键名大小写不敏感，接受常见别名（enter→return、esc→escape、
//! backspace→delete、del→forward_delete 等）。
//!
//! 修饰键状态：每个事件都**显式设置 flags**，不继承事件源状态——
//! 修饰键 down 携带「截至此刻已按下」的修饰集合，普通键携带全部修饰键，
//! 修饰键 up 携带「剩余」修饰集合，type 字符携带空集合。否则抬起事件会
//! 沿用按下时的状态标志，系统判定修饰键仍被按住（后续字符变成快捷键）。
//!
//! 串行化：键鼠手势经进程级 [`input_guard`] 互斥执行，宿主并发下发的
//! 多个工具调用不会让事件交错（如 ⌘ 按住期间插入的字符变成 ⌘A）。
use std::sync::{Mutex, MutexGuard};
use std::thread::sleep;
use std::time::Duration;

use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

use tiangong_plugin_computer_use_protocol::ops::KeyboardActionKind;

use super::input_cancel::{CancelToken, cancelled_message};
use super::keys::normalize_key_name;

/// key 手势 down → up 间隔。
const KEY_DOWN_UP: Duration = Duration::from_millis(50);
/// type 手势相邻字符间隔。
const TYPE_GAP: Duration = Duration::from_millis(15);
/// combo 修饰键相邻事件间隔。
const MOD_GAP: Duration = Duration::from_millis(20);
/// 手势结束后的收尾间隔：让目标应用处理完本手势的事件，再开始下一个。
const GESTURE_SETTLE: Duration = Duration::from_millis(30);

/// 键鼠手势互斥锁：同一时刻只执行一个手势，事件不交错。
static INPUT_LOCK: Mutex<()> = Mutex::new(());

/// 获取手势锁；前一个手势 panic 导致中毒时照常继续（锁内无共享数据）。
pub(crate) fn input_guard() -> MutexGuard<'static, ()> {
    INPUT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct KeyboardIo {
    source: CGEventSource,
}

impl KeyboardIo {
    fn new() -> Result<Self, String> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|()| "创建 CGEventSource 失败".to_string())?;
        Ok(Self { source })
    }
    /// 投递按键事件，flags 显式指定（不继承事件源的修饰键状态）。
    fn post_key(
        &self,
        keycode: CGKeyCode,
        key_down: bool,
        flags: CGEventFlags,
    ) -> Result<(), String> {
        let event = CGEvent::new_keyboard_event(self.source.clone(), keycode, key_down)
            .map_err(|()| "创建键盘事件失败".to_string())?;
        event.set_flags(flags);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
    /// 投递一个 Unicode 字符：down/up 成对携带同一字符串（部分应用只读
    /// keyDown 的字符串，成对最稳）。虚拟键码为 0（字符串分派不依赖布局），
    /// flags 为空，避免残留修饰键把字符变成快捷键。
    fn post_char(&self, ch: &str, key_down: bool) -> Result<(), String> {
        let event = CGEvent::new_keyboard_event(self.source.clone(), 0, key_down)
            .map_err(|()| "创建键盘事件失败".to_string())?;
        event.set_string(ch);
        event.set_flags(CGEventFlags::CGEventFlagNull);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

/// 修饰键对应的事件标志位。
fn modifier_flag(name: &str) -> CGEventFlags {
    match name {
        "cmd" | "rcmd" | "win" => CGEventFlags::CGEventFlagCommand,
        "shift" | "rshift" => CGEventFlags::CGEventFlagShift,
        "alt" | "ralt" => CGEventFlags::CGEventFlagAlternate,
        "ctrl" | "rctrl" => CGEventFlags::CGEventFlagControl,
        _ => CGEventFlags::CGEventFlagNull,
    }
}

/// combo 事件序列：(键码, 是否按下, 事件 flags)。修饰键 down 携带累积集合，
/// 普通键 down/up 携带全部修饰键，修饰键逆序 up 携带剩余集合（最后为空）。
fn combo_events(modifiers: &[&str], plain: CGKeyCode) -> Vec<(CGKeyCode, bool, CGEventFlags)> {
    let codes: Vec<(CGKeyCode, CGEventFlags)> = modifiers
        .iter()
        .filter_map(|m| modifier_keycode(m).map(|code| (code, modifier_flag(m))))
        .collect();
    let mut events = Vec::with_capacity(codes.len() * 2 + 2);
    let mut held = CGEventFlags::CGEventFlagNull;
    for (code, flag) in &codes {
        held |= *flag;
        events.push((*code, true, held));
    }
    events.push((plain, true, held));
    events.push((plain, false, held));
    for (i, (code, _)) in codes.iter().enumerate().rev() {
        let remaining = codes[..i]
            .iter()
            .fold(CGEventFlags::CGEventFlagNull, |acc, (_, f)| acc | *f);
        events.push((*code, false, remaining));
    }
    events
}

/// 修饰键虚拟键码（HIToolbox：cmd=55 rcmd=54 shift=56 rshift=60
/// alt=58 ralt=61 ctrl=59 rctrl=62）。win（Windows 徽标键）在 macOS 上等同 cmd。
fn modifier_keycode(name: &str) -> Option<CGKeyCode> {
    Some(match name {
        "cmd" | "win" => 55,
        "rcmd" => 54,
        "shift" => 56,
        "rshift" => 60,
        "alt" => 58,
        "ralt" => 61,
        "ctrl" => 59,
        "rctrl" => 62,
        _ => return None,
    })
}

/// 非修饰键虚拟键码（HIToolbox Events.h ANSI 表，US 布局）。
fn plain_keycode(name: &str) -> Option<CGKeyCode> {
    Some(match name {
        "return" => 36,
        "tab" => 48,
        "space" => 49,
        "delete" => 51,
        "forward_delete" => 117,
        "escape" => 53,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        "home" => 115,
        "end" => 119,
        "page_up" => 116,
        "page_down" => 121,
        "help" => 114,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        "a" => 0x00,
        "s" => 0x01,
        "d" => 0x02,
        "f" => 0x03,
        "h" => 0x04,
        "g" => 0x05,
        "z" => 0x06,
        "x" => 0x07,
        "c" => 0x08,
        "v" => 0x09,
        "b" => 0x0B,
        "q" => 0x0C,
        "w" => 0x0D,
        "e" => 0x0E,
        "r" => 0x0F,
        "y" => 0x10,
        "t" => 0x11,
        "1" => 0x12,
        "2" => 0x13,
        "3" => 0x14,
        "4" => 0x15,
        "6" => 0x16,
        "5" => 0x17,
        "9" => 0x19,
        "7" => 0x1A,
        "8" => 0x1C,
        "0" => 0x1D,
        "o" => 0x1F,
        "u" => 0x20,
        "i" => 0x22,
        "p" => 0x23,
        "l" => 0x25,
        "j" => 0x26,
        "k" => 0x28,
        "n" => 0x2D,
        "m" => 0x2E,
        _ => return None,
    })
}

/// 校验键名（wasm 只做结构校验，键名合法性由 perform 单点裁决）。
#[cfg(test)]
pub(crate) fn is_known_key(name: &str) -> bool {
    let normalized = normalize_key_name(name);
    modifier_keycode(&normalized).is_some() || plain_keycode(&normalized).is_some()
}

/// 执行一次键盘手势；成功返回人读摘要。`cancel` 置位后在下一个事件前
/// 停止（已按下的键照常抬起），返回取消说明。
pub fn perform(
    action: KeyboardActionKind,
    text: Option<String>,
    key: Option<String>,
    keys: Option<Vec<String>>,
    cancel: &CancelToken,
) -> Result<String, String> {
    // 排队等锁期间可能已被取消：拿到锁后先检查，避免取消后仍开始输入。
    let _guard = input_guard();
    if cancel.is_cancelled() {
        return Err(cancelled_message("键盘输入"));
    }
    let io = KeyboardIo::new()?;
    let result = perform_locked(&io, action, text, key, keys, cancel);
    sleep(GESTURE_SETTLE);
    result
}

fn perform_locked(
    io: &KeyboardIo,
    action: KeyboardActionKind,
    text: Option<String>,
    key: Option<String>,
    keys: Option<Vec<String>>,
    cancel: &CancelToken,
) -> Result<String, String> {
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
                let unit = ch.to_string();
                io.post_char(&unit, true)?;
                sleep(KEY_DOWN_UP);
                io.post_char(&unit, false)?;
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
            let keycode = plain_keycode(&name)
                .ok_or_else(|| format!("不支持的键名: {name}（修饰键请用 combo）"))?;
            // 按键 HUD 先于事件显示，用户看到提示与界面响应同步。
            super::overlay::key_cast(vec![super::keys::key_symbol(&name)]);
            io.post_key(keycode, true, CGEventFlags::CGEventFlagNull)?;
            sleep(KEY_DOWN_UP);
            io.post_key(keycode, false, CGEventFlags::CGEventFlagNull)?;
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
            let modifiers: Vec<&String> = names
                .iter()
                .filter(|n| modifier_keycode(n).is_some())
                .collect();
            let plains: Vec<&String> = names
                .iter()
                .filter(|n| modifier_keycode(n).is_none())
                .collect();
            if plains.len() != 1 {
                return Err(format!(
                    "combo 需要恰好一个非修饰键（当前 {} 个）；单独按修饰键没有意义",
                    plains.len()
                ));
            }
            let plain_name = plains[0].clone();
            let plain = plain_keycode(&plain_name)
                .ok_or_else(|| format!("combo 不支持的键名: {plain_name}"))?;
            super::overlay::key_cast(super::keys::combo_keys(&names));
            // 修饰键 down（保持给定顺序）→ 普通键 down/up → 修饰键逆序 up，
            // 每个事件显式携带当时的修饰集合；中途失败也补发全部抬起。
            let modifier_names: Vec<&str> = modifiers.iter().map(|m| m.as_str()).collect();
            let events = combo_events(&modifier_names, plain);
            let mut result = Ok(());
            for (index, (code, down, flags)) in events.iter().enumerate() {
                // 取消只在按下阶段生效：已开始抬起就让序列走完。
                if *down && cancel.is_cancelled() {
                    result = Err(cancelled_message("组合键"));
                    break;
                }
                if let Err(error) = io.post_key(*code, *down, *flags) {
                    result = Err(error);
                    break;
                }
                let next_is_plain_up = events
                    .get(index + 1)
                    .is_some_and(|(c, d, _)| *c == plain && !*d);
                sleep(if next_is_plain_up {
                    KEY_DOWN_UP
                } else {
                    MOD_GAP
                });
            }
            if result.is_err() {
                for (code, _, _) in events.iter().filter(|(_, down, _)| *down) {
                    let _ = io.post_key(*code, false, CGEventFlags::CGEventFlagNull);
                }
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
    fn combo_events_carry_explicit_modifier_flags() {
        let cmd = CGEventFlags::CGEventFlagCommand;
        let shift = CGEventFlags::CGEventFlagShift;
        let none = CGEventFlags::CGEventFlagNull;
        // ⌘⇧Z：⌘↓(⌘) ⇧↓(⌘⇧) Z↓(⌘⇧) Z↑(⌘⇧) ⇧↑(⌘) ⌘↑(空)。
        let events = combo_events(&["cmd", "shift"], 0x06);
        assert_eq!(
            events,
            vec![
                (55, true, cmd),
                (56, true, cmd | shift),
                (0x06, true, cmd | shift),
                (0x06, false, cmd | shift),
                (56, false, cmd),
                (55, false, none),
            ]
        );
        // 最后一个事件必须不带任何修饰键，否则系统认为仍被按住。
        assert_eq!(events.last().map(|e| e.2), Some(none));
    }

    #[test]
    fn known_keys_cover_letters_digits_and_specials() {
        for k in ["a", "Z", "0", "9", "return", "esc", "f5", "up"] {
            assert!(is_known_key(k), "{k} 应识别");
        }
        for k in ["cmd", "ctrl", "shift", "alt", "rcmd", "win"] {
            assert!(is_known_key(k), "{k} 应识别为修饰键");
        }
        assert!(!is_known_key("hup"), "未知键应拒绝");
        assert!(!is_known_key(""), "空键名应拒绝");
        assert!(!is_known_key("fn"), "fn 不是可合成键");
    }

    #[test]
    fn plain_and_modifier_keycodes_disjoint() {
        // 修饰键不得同时出现在普通键表（combo 依赖两表互斥分区）。
        for m in [
            "cmd", "rcmd", "shift", "rshift", "alt", "ralt", "ctrl", "rctrl",
        ] {
            assert!(modifier_keycode(m).is_some());
            assert!(plain_keycode(m).is_none(), "{m} 不应视为普通键");
        }
        for p in ["return", "space", "a", "f12"] {
            assert!(plain_keycode(p).is_some());
            assert!(modifier_keycode(p).is_none());
        }
    }
}
