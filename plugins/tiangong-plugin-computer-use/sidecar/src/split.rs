//! 自动分屏状态：天工窗口原位置、分屏是否生效、哪个会话可显示「恢复窗口」。
//!
//! 打开应用（`desktop_open_app`）成功后，平台后端把天工窗口放到屏幕左侧
//! 固定宽度、目标窗口铺满右侧；本模块只记录状态并推送变化通知，
//! 不涉及任何平台 API，便于单元测试。
//!
//! 按钮显示规则（插件 UI 读取 [`SplitState`]）：
//! - 仅当天工窗口处于分屏布局（`active`）且原位置已保存；
//! - 仅对使用过自动分屏的会话，且该会话的对话已完成（轮次结束）；
//! - 该会话开始新一轮对话时隐藏，轮次结束后再次显示；
//! - 用户点击恢复后天工窗口回到原位置，分屏状态清空，按钮隐藏。

use std::sync::Mutex;

use tiangong_plugin_computer_use_protocol::Bounds;
use tiangong_plugin_computer_use_protocol::ops::{SPLIT_NOTIFICATION_CHANNEL, SplitState};

/// 分屏前天工窗口的状态，恢复时按此还原。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SavedHostFrame {
    /// 可见窗口边界（macOS points / Windows 物理像素，均为平台窗口坐标）。
    pub bounds: Bounds,
    /// 分屏前是否最大化（Windows）。
    pub maximized: bool,
    /// 分屏前是否处于全屏（macOS）。
    pub fullscreen: bool,
}

#[derive(Debug, Default)]
struct SplitStore {
    saved_host: Option<SavedHostFrame>,
    /// 最近一次触发分屏的会话（轮次结束后才确定）。
    owner_session: Option<String>,
    /// 当前可显示恢复按钮的会话。
    restorable_session: Option<String>,
}

impl SplitStore {
    fn state(&self) -> SplitState {
        SplitState {
            active: self.saved_host.is_some(),
            restorable_session: self
                .saved_host
                .is_some()
                .then(|| self.restorable_session.clone())
                .flatten(),
        }
    }
}

static STORE: Mutex<SplitStore> = Mutex::new(SplitStore {
    saved_host: None,
    owner_session: None,
    restorable_session: None,
});

fn with_store<R>(apply: impl FnOnce(&mut SplitStore) -> R) -> R {
    let mut store = STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    apply(&mut store)
}

/// 当前分屏状态。
pub fn state() -> SplitState {
    with_store(|store| store.state())
}

/// 分屏生效：仅在尚未保存时记录原位置（连续打开多个应用时，恢复仍回到
/// 最初的位置）。分屏发生在对话进行中，恢复按钮先隐藏。
pub fn record_applied(original: Option<SavedHostFrame>) -> SplitState {
    let state = with_store(|store| {
        if store.saved_host.is_none() {
            store.saved_host = original;
        }
        store.restorable_session = None;
        store.state()
    });
    notify(&state);
    state
}

/// 会话开始新一轮对话：该会话的恢复按钮隐藏。
pub fn turn_started(session_id: &str) -> SplitState {
    let (state, changed) = with_store(|store| {
        let changed = store.restorable_session.as_deref() == Some(session_id);
        if changed {
            store.restorable_session = None;
        }
        (store.state(), changed)
    });
    if changed {
        notify(&state);
    }
    state
}

/// 会话一轮对话结束。`used_split` 表示本轮触发过自动分屏；分屏仍生效且
/// 该会话是分屏发起方时显示恢复按钮。
pub fn turn_finished(session_id: &str, used_split: bool) -> SplitState {
    let state = with_store(|store| {
        if used_split {
            store.owner_session = Some(session_id.to_string());
        }
        if store.saved_host.is_some() && store.owner_session.as_deref() == Some(session_id) {
            store.restorable_session = Some(session_id.to_string());
        }
        store.state()
    });
    notify(&state);
    state
}

/// 取出待恢复的原位置（成功恢复后调用 [`clear`]）。
pub fn saved_host() -> Option<SavedHostFrame> {
    with_store(|store| store.saved_host)
}

/// 恢复完成（或原窗口已不存在）：清空分屏状态并隐藏按钮。
pub fn clear() -> SplitState {
    let state = with_store(|store| {
        *store = SplitStore::default();
        store.state()
    });
    notify(&state);
    state
}

/// 推送分屏状态变化（经宿主 `sidecar.event` 到达插件 UI）。
fn notify(state: &SplitState) {
    if let Ok(body) = serde_json::to_string(state) {
        tiangong_plugin_sidecar::server::emit_notification(SPLIT_NOTIFICATION_CHANNEL, body);
    }
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    with_store(|store| *store = SplitStore::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局状态：测试串行执行。
    static SERIAL: Mutex<()> = Mutex::new(());

    fn frame(x: f64) -> SavedHostFrame {
        SavedHostFrame {
            bounds: Bounds {
                x,
                y: 0.0,
                width: 1400.0,
                height: 900.0,
            },
            ..Default::default()
        }
    }

    #[test]
    fn restore_button_only_after_owner_turn_finishes() {
        let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        let state = record_applied(Some(frame(10.0)));
        assert!(state.active);
        assert_eq!(state.restorable_session, None, "对话进行中不显示");

        let other = turn_finished("other", false);
        assert_eq!(other.restorable_session, None, "未使用分屏的会话不显示");

        let owner = turn_finished("s1", true);
        assert_eq!(owner.restorable_session.as_deref(), Some("s1"));

        let running = turn_started("s1");
        assert_eq!(running.restorable_session, None, "新一轮进行中隐藏");
        let done = turn_finished("s1", false);
        assert_eq!(
            done.restorable_session.as_deref(),
            Some("s1"),
            "分屏仍生效，再次显示"
        );
        reset_for_test();
    }

    #[test]
    fn keeps_first_original_frame_across_repeated_splits() {
        let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        record_applied(Some(frame(10.0)));
        record_applied(Some(frame(0.0)));
        assert_eq!(saved_host(), Some(frame(10.0)));
        reset_for_test();
    }

    #[test]
    fn clear_hides_button_and_forgets_owner() {
        let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        record_applied(Some(frame(10.0)));
        turn_finished("s1", true);
        let cleared = clear();
        assert_eq!(cleared, SplitState::default());
        assert_eq!(saved_host(), None);
        let later = turn_finished("s1", false);
        assert_eq!(later.restorable_session, None, "恢复后不再显示");
        reset_for_test();
    }

    #[test]
    fn split_without_saved_frame_is_not_restorable() {
        let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        let state = record_applied(None);
        assert!(!state.active);
        assert_eq!(turn_finished("s1", true).restorable_session, None);
        reset_for_test();
    }
}
