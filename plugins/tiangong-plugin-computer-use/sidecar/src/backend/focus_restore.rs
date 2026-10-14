//! 输入焦点「借用并归还」（macOS）：点击、键入前记下用户原来的输入焦点与
//! 光标位置，动作完成后立即还原；用户原本不在输入状态时什么都不做。
//!
//! Agent 的典型流程是「先点击目标输入框 → 再键入」，而键盘事件只会投递给
//! 当前焦点。点击后焦点已还给用户，因此记住 Agent 最近一次点击建立的焦点
//! （[`AGENT_FOCUS`]），下一次键盘手势开始前先把焦点切回该控件再输入，
//! 输入完成后再归还用户焦点。
//!
//! 「用户的焦点」只认文本输入类控件（可读写 `AXSelectedTextRange`）。若此刻
//! 焦点就是 Agent 记下的控件（例如还原失败遗留），不把它当成用户焦点，
//! 避免把 Agent 的输入框误认为用户原位置。
//!
//! 所有 AX 调用都是跨进程同步调用，在键鼠手势的阻塞线程中执行。

use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};

use super::ax::{self, AxElement, AxTextRange};

/// 切换应用 / 设置焦点后的等待：让目标应用处理激活与焦点变化。
const FOCUS_SETTLE: Duration = Duration::from_millis(120);

/// 一处文本输入焦点：所属应用、控件与光标/选区。
#[derive(Clone)]
pub(crate) struct TextFocus {
    pid: i32,
    element: AxElement,
    range: Option<AxTextRange>,
}

/// Agent 最近一次点击后建立的焦点（下一次键盘手势前切回）。
static AGENT_FOCUS: Mutex<Option<TextFocus>> = Mutex::new(None);

fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| app.processIdentifier())
}

/// 读取当前系统焦点；不是文本输入控件时返回 None。
fn current_text_focus() -> Option<TextFocus> {
    if !ax::is_process_trusted() {
        return None;
    }
    let element = AxElement::system_focused(frontmost_pid())?;
    let pid = element.pid()?;
    let range = element.selected_text_range();
    let editable = range.is_some() && element.is_attribute_settable("AXSelectedTextRange");
    editable.then_some(TextFocus {
        pid,
        element,
        range,
    })
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
    let focus = current_text_focus()?;
    let is_agent = agent_focus().is_some_and(|agent| agent.element.same_as(&focus.element));
    (!is_agent).then_some(focus)
}

/// 激活应用并把焦点、光标放回 `focus`。尽力而为：控件已销毁等失败静默。
fn apply_focus(focus: &TextFocus, restore_range: bool) {
    if frontmost_pid() != Some(focus.pid) {
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(focus.pid)
        {
            let _ = app.activateWithOptions(NSApplicationActivationOptions::empty());
        }
        let _ = AxElement::for_application(focus.pid).set_bool_attribute("AXFrontmost", true);
        sleep(FOCUS_SETTLE);
    }
    // 焦点所在窗口置为主窗口并置前（同一应用多窗口时）。
    if let Some(window) = focus.element.element_attribute("AXWindow") {
        let _ = window.set_bool_attribute("AXMain", true);
        let _ = window.perform_action("AXRaise");
    }
    let _ = focus.element.set_bool_attribute("AXFocused", true);
    if restore_range && let Some(range) = focus.range {
        let _ = focus.element.set_selected_text_range(range);
    }
}

/// 动作完成后还原用户的输入焦点与光标位置。
pub(crate) fn restore_user_focus(user: Option<TextFocus>) {
    if let Some(user) = user {
        apply_focus(&user, true);
    }
}

/// 点击完成后：记下 Agent 建立的焦点（光标位置由点击决定，供后续键入
/// 接续），然后还原用户焦点。
pub(crate) fn after_click(user: Option<TextFocus>) {
    if !ax::is_process_trusted() {
        return;
    }
    // 等目标应用处理完点击引起的焦点变化再读取。
    sleep(FOCUS_SETTLE);
    // 始终记下点击建立的输入焦点：后续键入前据此判断「当前焦点是 Agent
    // 的输入框而非用户的」，避免把 Agent 的输入框误当用户焦点还原。
    let agent = current_text_focus().filter(|focus| {
        user.as_ref()
            .is_none_or(|u| !u.element.same_as(&focus.element))
    });
    set_agent_focus(agent);
    // 用户原本不在输入状态时无需还原。
    restore_user_focus(user);
}

/// 键盘手势前：用户焦点需要归还时，先把焦点切回 Agent 最近点击的输入框
/// （含其光标位置）。返回需要在手势结束后还原的用户焦点。
pub(crate) fn before_keyboard() -> Option<TextFocus> {
    let user = capture_user_focus()?;
    if let Some(agent) = agent_focus() {
        apply_focus(&agent, true);
    }
    Some(user)
}

/// 键盘手势后：更新 Agent 焦点的光标位置（下次接着输入），再还原用户焦点。
pub(crate) fn after_keyboard(user: Option<TextFocus>) {
    if user.is_none() {
        return;
    }
    if let Some(mut agent) = agent_focus()
        && let Some(now) = current_text_focus()
        && now.element.same_as(&agent.element)
    {
        agent.range = now.range;
        set_agent_focus(Some(agent));
    }
    restore_user_focus(user);
}
