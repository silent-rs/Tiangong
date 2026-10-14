//! 自动分屏状态：天工窗口与被分屏应用窗口的原位置、分屏是否生效。
//!
//! 打开应用（`desktop_open_app`）成功后，平台后端把天工窗口放到屏幕右侧
//! 固定宽度、目标窗口铺满左侧；本模块只记录状态并推送变化通知，
//! 不涉及任何平台 API，便于单元测试。
//!
//! 按钮显示规则（插件 UI 读取 [`SplitState`]）：
//! - 天工窗口一进入分屏布局（`active`）就显示「恢复窗口」，不区分会话、
//!   不等对话结束，用户随时可以一键恢复；
//! - 点击恢复后天工窗口与本轮被分屏的全部应用窗口回到各自原位置，
//!   分屏状态清空，按钮隐藏。

use std::sync::Mutex;

use tiangong_plugin_computer_use_protocol::Bounds;
use tiangong_plugin_computer_use_protocol::ops::{SPLIT_NOTIFICATION_CHANNEL, SplitState};

/// 分屏前窗口的状态，恢复时按此还原。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SavedHostFrame {
    /// 可见窗口边界（macOS points / Windows 物理像素，均为平台窗口坐标）。
    pub bounds: Bounds,
    /// 分屏前是否最大化（Windows）。
    pub maximized: bool,
    /// 分屏前是否处于全屏（macOS）。
    pub fullscreen: bool,
}

/// 平台窗口句柄：macOS 为 AX 窗口元素，Windows 为 HWND 数值。
#[cfg(target_os = "macos")]
pub type TargetHandle = crate::backend::ax::AxElement;
#[cfg(target_os = "windows")]
pub type TargetHandle = isize;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub type TargetHandle = ();

/// 被分屏到左侧的应用窗口及其分屏前状态。
#[derive(Clone)]
pub struct SavedTarget {
    /// 窗口去重键（同一窗口多次被分屏只记录最初位置）。
    pub key: u64,
    pub handle: TargetHandle,
    /// 人读名称（恢复失败时说明用）。
    pub label: String,
    pub frame: SavedHostFrame,
}

impl std::fmt::Debug for SavedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedTarget")
            .field("key", &self.key)
            .field("label", &self.label)
            .field("frame", &self.frame)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct SplitStore {
    saved_host: Option<SavedHostFrame>,
    /// 按分屏先后顺序记录的应用窗口。
    targets: Vec<SavedTarget>,
}

impl SplitStore {
    fn state(&self) -> SplitState {
        SplitState {
            active: self.saved_host.is_some(),
        }
    }
}

static STORE: Mutex<SplitStore> = Mutex::new(SplitStore {
    saved_host: None,
    targets: Vec::new(),
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

/// 天工窗口已进入分屏布局：仅在尚未保存时记录原位置（连续打开多个应用
/// 时，恢复仍回到最初的位置）。立即推送状态，恢复按钮随即出现。
pub fn record_applied(original: Option<SavedHostFrame>) -> SplitState {
    let state = with_store(|store| {
        if store.saved_host.is_none() {
            store.saved_host = original;
        }
        store.state()
    });
    notify(&state);
    state
}

/// 记录一个被分屏的应用窗口。分屏未生效（天工原位置未保存）时忽略；
/// 同一窗口重复分屏只保留最初的位置。
pub fn record_target(target: SavedTarget) {
    with_store(|store| {
        if store.saved_host.is_some() && store.targets.iter().all(|t| t.key != target.key) {
            store.targets.push(target);
        }
    });
}

/// 待恢复的天工原位置与被分屏的应用窗口（成功恢复后调用 [`clear`]）。
pub fn saved() -> Option<(SavedHostFrame, Vec<SavedTarget>)> {
    with_store(|store| store.saved_host.map(|host| (host, store.targets.clone())))
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

    #[cfg(target_os = "macos")]
    fn handle() -> TargetHandle {
        crate::backend::ax::AxElement::for_application(1)
    }
    #[cfg(target_os = "windows")]
    fn handle() -> TargetHandle {
        0
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn handle() -> TargetHandle {}

    fn target(key: u64, x: f64) -> SavedTarget {
        SavedTarget {
            key,
            handle: handle(),
            label: format!("app-{key}"),
            frame: frame(x),
        }
    }

    #[test]
    fn restore_button_shows_as_soon_as_split_applies() {
        let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        assert!(!state().active);
        let state = record_applied(Some(frame(10.0)));
        assert!(state.active, "进入分屏立即可恢复，不等对话结束");
        reset_for_test();
    }

    #[test]
    fn keeps_first_original_frame_across_repeated_splits() {
        let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        record_applied(Some(frame(10.0)));
        record_applied(Some(frame(0.0)));
        assert_eq!(saved().map(|(host, _)| host), Some(frame(10.0)));
        reset_for_test();
    }

    #[test]
    fn records_every_split_target_once_in_order() {
        let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        record_applied(Some(frame(10.0)));
        record_target(target(1, 100.0));
        record_target(target(2, 200.0));
        // 同一窗口再次分屏：保留最初位置。
        record_target(target(1, 0.0));
        let (_, targets) = saved().expect("分屏已生效");
        let summary: Vec<(u64, f64)> = targets.iter().map(|t| (t.key, t.frame.bounds.x)).collect();
        assert_eq!(summary, vec![(1, 100.0), (2, 200.0)]);
        reset_for_test();
    }

    #[test]
    fn clear_hides_button_and_forgets_targets() {
        let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        record_applied(Some(frame(10.0)));
        record_target(target(1, 100.0));
        let cleared = clear();
        assert_eq!(cleared, SplitState::default());
        assert!(saved().is_none());
        reset_for_test();
    }

    #[test]
    fn split_without_saved_frame_is_not_restorable() {
        let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        let state = record_applied(None);
        assert!(!state.active);
        record_target(target(1, 100.0));
        assert!(saved().is_none(), "天工未进入分屏时不记录目标窗口");
        reset_for_test();
    }
}
