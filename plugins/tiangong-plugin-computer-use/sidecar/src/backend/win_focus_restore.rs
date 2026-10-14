//! 输入焦点「借用并归还」（Windows）：点击、键入前记下用户原来的输入焦点
//! 与光标位置，动作完成后立即还原；用户原本不在输入状态时什么都不做。
//!
//! 与 macOS 版（`focus_restore`）同一策略：Agent「先点击输入框 → 再键入」，
//! 点击后焦点已归还用户，因此记住 Agent 点击建立的焦点，下一次键盘手势前
//! 先切回该控件再输入，输入完成后再归还用户焦点。
//!
//! 「用户的焦点」只认支持 UIA TextPattern 的控件（文本选区可读写）。焦点
//! 元素、所在顶层窗口与选区均经 UI Automation 读取；归还时先激活窗口
//! （`SetForegroundWindow`），再 `SetFocus` 并 `Select` 原选区。

use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use uiautomation::core::{UIAutomation, UIElement};
use uiautomation::patterns::{UITextPattern, UITextRange};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// 切换窗口 / 设置焦点后的等待：让目标应用处理激活与焦点变化。
const FOCUS_SETTLE: Duration = Duration::from_millis(120);

/// 一处文本输入焦点：顶层窗口、控件与光标/选区。
#[derive(Clone)]
pub(crate) struct TextFocus {
    hwnd: isize,
    element: UIElement,
    range: Option<UITextRange>,
}

// SAFETY：UIA COM 接口支持 MTA 跨线程访问（与 windows.rs 的 SendElement 同理），
// 键鼠手势在阻塞线程池执行，焦点记录需跨手势保存在静态变量中。
unsafe impl Send for TextFocus {}

/// Agent 最近一次点击后建立的焦点（下一次键盘手势前切回）。
static AGENT_FOCUS: Mutex<Option<TextFocus>> = Mutex::new(None);

fn automation() -> Option<UIAutomation> {
    UIAutomation::new().ok()
}

/// 读取当前系统焦点；不是文本输入控件时返回 None。
fn current_text_focus(automation: &UIAutomation) -> Option<TextFocus> {
    let element = automation.get_focused_element().ok()?;
    let text: UITextPattern = element.get_pattern().ok()?;
    let range = text
        .get_selection()
        .ok()
        .and_then(|ranges| ranges.into_iter().next());
    // SAFETY：纯查询。
    let hwnd = unsafe { GetForegroundWindow() }.0 as isize;
    Some(TextFocus {
        hwnd,
        element,
        range,
    })
}

fn same_element(automation: &UIAutomation, a: &UIElement, b: &UIElement) -> bool {
    automation.compare_elements(a, b).unwrap_or(false)
}

fn agent_focus() -> Option<TextFocus> {
    AGENT_FOCUS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

fn set_agent_focus(focus: Option<TextFocus>) {
    *AGENT_FOCUS.lock().unwrap_or_else(|p| p.into_inner()) = focus;
}

/// 动作前记下用户的输入焦点。不在输入状态、或焦点就是 Agent 的输入框时
/// 返回 None（无需还原）。
pub(crate) fn capture_user_focus() -> Option<TextFocus> {
    let automation = automation()?;
    let focus = current_text_focus(&automation)?;
    let is_agent = agent_focus()
        .is_some_and(|agent| same_element(&automation, &agent.element, &focus.element));
    (!is_agent).then_some(focus)
}

/// 激活窗口并把焦点、光标放回 `focus`。尽力而为：控件已销毁等失败静默。
fn apply_focus(focus: &TextFocus) {
    // SAFETY：纯查询。
    let foreground = unsafe { GetForegroundWindow() }.0 as isize;
    if foreground != focus.hwnd && focus.hwnd != 0 {
        let hwnd = HWND(focus.hwnd as *mut core::ffi::c_void);
        super::win_desktop::activate_window(hwnd);
        sleep(FOCUS_SETTLE);
    }
    let _ = focus.element.set_focus();
    if let Some(range) = &focus.range {
        let _ = range.select();
    }
}

/// 动作完成后还原用户的输入焦点与光标位置。
pub(crate) fn restore_user_focus(user: Option<TextFocus>) {
    if let Some(user) = user {
        apply_focus(&user);
    }
}

/// 点击完成后：记下 Agent 建立的焦点（供后续键入接续），然后还原用户焦点。
pub(crate) fn after_click(user: Option<TextFocus>) {
    let Some(automation) = automation() else {
        return;
    };
    // 等目标应用处理完点击引起的焦点变化再读取。
    sleep(FOCUS_SETTLE);
    let agent = current_text_focus(&automation).filter(|focus| {
        user.as_ref()
            .is_none_or(|u| !same_element(&automation, &u.element, &focus.element))
    });
    set_agent_focus(agent);
    restore_user_focus(user);
}

/// 键盘手势前：用户焦点需要归还时，先把焦点切回 Agent 最近点击的输入框
/// （含其光标位置）。返回需要在手势结束后还原的用户焦点。
pub(crate) fn before_keyboard() -> Option<TextFocus> {
    let user = capture_user_focus()?;
    if let Some(agent) = agent_focus() {
        apply_focus(&agent);
    }
    Some(user)
}

/// 键盘手势后：更新 Agent 焦点的光标位置（下次接着输入），再还原用户焦点。
pub(crate) fn after_keyboard(user: Option<TextFocus>) {
    if user.is_none() {
        return;
    }
    if let Some(automation) = automation()
        && let Some(mut agent) = agent_focus()
        && let Some(now) = current_text_focus(&automation)
        && same_element(&automation, &now.element, &agent.element)
    {
        agent.range = now.range;
        set_agent_focus(Some(agent));
    }
    restore_user_focus(user);
}
