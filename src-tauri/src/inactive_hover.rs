//! macOS 后台窗口悬停监听。
//!
//! macOS 只向激活应用派发 mouseMoved，窗口在后台时 CSS `:hover` 不生效，
//! 消息列表右侧导航（刻度尺与按钮组）因此无法唤出。本模块在窗口未激活
//! 时轮询全局鼠标位置，鼠标位于页面内则把页面视口坐标（与
//! PointerEvent.clientX/clientY 同系）经 `window:inactive_cursor` 事件下发给
//! 前端，由前端对照导航热区矩形自行决定是否显示导航；payload 为 null 表示
//! 离开页面或窗口已激活（激活后由 CSS `:hover` 接管）。
//! 窗口从未激活转入激活且鼠标刚在页面内时，经 `window:inactive_click`
//! 补发首击位置——系统首击被窗口激活消费、未到达页面（wry 的
//! acceptsFirstMouse 挂在 WKWebView 上，实际接收点击的内部 WKContentView
//! 不受其控制），前端据位置执行横条/预览卡跳转。
//!
//! 坐标转换全部走 macOS 原生路径（主线程）：NSEvent::mouseLocation 全局
//! 逻辑点 → NSWindow convertPointFromScreen → WebView 视图
//! convertPoint:fromView: → 依视图翻转与内容偏移得到页面左上原点坐标。
//! 不枚举显示器、不手动换算缩放，任意显示器布局（负原点、跨屏、混合
//! DPI）下由系统保证一致；命中判断用 WebView 实际边界（排除标题栏、
//! 边框与内容偏移），而不是窗口外框。

use objc2_app_kit::{NSEvent, NSView, NSWindow};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// 采样间隔。导航本身有 200ms 过渡动画，50ms 采样延迟无感，
/// 且仅在「窗口未激活 + 鼠标在页面内」时才产生事件流量。
const POLL_INTERVAL_MS: u64 = 50;

/// 位置变化超过该阈值（页面像素）才下发，避免静止时重复事件。
const MOVE_THRESHOLD: f64 = 1.0;

/// 窗口从未激活转入激活时，鼠标在页面内的最近确认距今小于该值视为后台首击。
/// 取两倍采样间隔，覆盖静止悬停后（无坐标下发）点击的情况。
const CLICK_WINDOW_MS: u64 = 150;

/// 主线程采样超时：超过视为本轮失败（主线程繁忙），跳过本轮。
const SAMPLE_TIMEOUT_MS: u64 = 200;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InactiveCursorPoint {
    pub x: f64,
    pub y: f64,
}

/// 主线程单轮采样结果；轮询线程据此做显隐与首击决策。
enum Sample {
    /// 主窗口已不存在，轮询结束。
    WindowGone,
    /// 窗口状态快照；`in_page` 为真时 x/y 为页面视口坐标。
    State {
        /// 窗口可见且未最小化
        visible: bool,
        focused: bool,
        in_page: bool,
        x: f64,
        y: f64,
    },
}

/// 启动后台悬停轮询线程。窗口关闭（拿不到主窗口）即退出。
pub fn spawn(app: AppHandle) {
    std::thread::Builder::new()
        .name("inactive-hover".into())
        .spawn(move || poll_loop(app))
        .expect("启动 inactive-hover 线程失败");
}

/// 轮询线程只负责节奏与决策；AppKit 访问（NSEvent/NSWindow/NSView/坐标
/// 转换/窗口状态）经 `run_on_main_thread` 在主线程执行。`in_flight` 仅由
/// 主线程闭包在完成时复位：主线程持续繁忙时旧任务始终占位，后续轮询一律
/// 跳过、不再提交，队列中至多积压一个任务；每轮使用独立的一次性通道，
/// 超时轮的接收端丢弃后，迟到结果发送失败自然作废，不会串轮消费。
fn poll_loop(app: AppHandle) {
    // 上次下发的页面坐标；None 表示当前不在「未激活且鼠标在页面内」状态
    let mut last: Option<InactiveCursorPoint> = None;
    // 最近一次确认鼠标在页面内的时刻（含静止未下发坐标的轮次）
    let mut in_page_at: Option<Instant> = None;
    let in_flight = Arc::new(AtomicBool::new(false));

    loop {
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));

        if in_flight.swap(true, Ordering::AcqRel) {
            // 上一轮主线程采样未返回（含超时后仍在排队的旧任务），跳过本轮
            continue;
        }
        let (tx, rx) = mpsc::channel::<Sample>();
        let app_handle = app.clone();
        let done = Arc::clone(&in_flight);
        let submit = app.run_on_main_thread(move || {
            let sample = sample_on_main_thread(&app_handle);
            let _ = tx.send(sample);
            done.store(false, Ordering::Release);
        });
        if submit.is_err() {
            // 任务未提交（app 退出），由本线程复位并结束轮询
            in_flight.store(false, Ordering::Release);
            break;
        }
        let sample = match rx.recv_timeout(Duration::from_millis(SAMPLE_TIMEOUT_MS)) {
            Ok(sample) => sample,
            Err(_) => {
                // 主线程繁忙未按时返回：不消费结果也不复位 in_flight，
                // 后续轮询持续跳过，直到旧任务在主线程完成并复位
                continue;
            }
        };

        match sample {
            Sample::WindowGone => break,
            Sample::State {
                visible,
                focused,
                in_page,
                x,
                y,
            } => {
                // 窗口隐藏（托盘）或最小化时不产生悬停信号
                if !visible {
                    last = emit_leave(&app, last);
                    in_page_at = None;
                    continue;
                }
                // 激活时 CSS :hover 生效，前端也在 focus 时清状态；此处只补发一次离开
                if focused {
                    // 后台首击补发：窗口从未激活转入激活，且极短时间内仍确认过鼠标
                    // 在页面内，视为后台点击（系统首击被窗口激活消费、未到达页面）。
                    // 把点击位置下发给前端执行命中判定；Cmd+Tab 等无点击激活因
                    // 鼠标静止无近期确认而天然排除。
                    if let (Some(point), Some(at)) = (last, in_page_at) {
                        if at.elapsed() < Duration::from_millis(CLICK_WINDOW_MS) {
                            tracing::debug!(
                                point_x = point.x,
                                point_y = point.y,
                                "inactive-hover：补发后台首击位置"
                            );
                            let _ = app.emit("window:inactive_click", &point);
                        }
                    }
                    last = emit_leave(&app, last);
                    in_page_at = None;
                    continue;
                }
                if !in_page {
                    last = emit_leave(&app, last);
                    in_page_at = None;
                    continue;
                }
                in_page_at = Some(Instant::now());
                let point = InactiveCursorPoint { x, y };
                let moved = last
                    .map(|p| {
                        (p.x - point.x).abs() > MOVE_THRESHOLD
                            || (p.y - point.y).abs() > MOVE_THRESHOLD
                    })
                    .unwrap_or(true);
                if moved {
                    tracing::debug!(
                        point_x = point.x,
                        point_y = point.y,
                        "inactive-hover：下发页面内鼠标坐标"
                    );
                    let _ = app.emit("window:inactive_cursor", &point);
                    last = Some(point);
                }
            }
        }
    }
}

/// 主线程采样：读取窗口状态并把全局鼠标位置转换为页面视口坐标。
/// 原生对象访问失败时按「本轮无悬停」处理，不影响后续轮次。
fn sample_on_main_thread(app: &AppHandle) -> Sample {
    // 按窗口标签取主窗口：内嵌子 webview（浏览器等）后 `get_webview_window` 返回 None。
    let Some(window) = app.get_window("main") else {
        return Sample::WindowGone;
    };
    let visible = window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(false);
    let focused = window.is_focused().unwrap_or(true);
    if !visible || focused {
        return Sample::State {
            visible,
            focused,
            in_page: false,
            x: 0.0,
            y: 0.0,
        };
    }
    let state = |in_page: bool| Sample::State {
        visible,
        focused,
        in_page,
        x: 0.0,
        y: 0.0,
    };
    let (Ok(ns_window_ptr), Ok(content_ptr)) = (window.ns_window(), window.ns_view()) else {
        return state(false);
    };
    // 指针仅在本次主线程调用内解引用，原生对象由 Tauri 窗口持有
    let ns_window = unsafe { &*(ns_window_ptr.cast::<NSWindow>()) };
    let content = unsafe { &*(content_ptr.cast::<NSView>()) };
    // wry 将 WKWebView 直接挂在 contentView 下；命中与坐标以 WebView 实际
    // 边界为准，找不到时退回 contentView。
    let subviews = content.subviews();
    // FastEnumeration 迭代产生 owned Retained，命中后移出持有，避免借用循环变量
    let mut webview_owned: Option<objc2::rc::Retained<NSView>> = None;
    for view in subviews.iter() {
        let is_webview = view
            .class()
            .name()
            .to_str()
            .map(|name| name.contains("WKWebView"))
            .unwrap_or(false);
        if is_webview {
            webview_owned = Some(view);
            break;
        }
    }
    let webview: &NSView = webview_owned.as_deref().unwrap_or(content);

    let global = NSEvent::mouseLocation();
    let in_window = ns_window.convertPointFromScreen(global);
    let in_view = webview.convertPoint_fromView(in_window, None);
    let bounds = webview.bounds();
    let flipped = webview.isFlipped();
    // 页面左上原点坐标：非翻转视图先翻 y，再统一扣除内容偏移（bounds.origin）
    let x = in_view.x - bounds.origin.x;
    let y = if flipped {
        in_view.y - bounds.origin.y
    } else {
        bounds.size.height - in_view.y - bounds.origin.y
    };
    let in_page = x >= 0.0 && x <= bounds.size.width && y >= 0.0 && y <= bounds.size.height;
    if !in_page {
        return state(false);
    }
    Sample::State {
        visible,
        focused,
        in_page,
        x,
        y,
    }
}

fn emit_leave(app: &AppHandle, last: Option<InactiveCursorPoint>) -> Option<InactiveCursorPoint> {
    if last.is_some() {
        tracing::debug!("inactive-hover：鼠标离开页面或窗口已激活，下发 null");
        let _ = app.emit("window:inactive_cursor", serde_json::Value::Null);
    }
    None
}
