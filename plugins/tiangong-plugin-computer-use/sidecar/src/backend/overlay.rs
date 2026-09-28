//! 天工虚拟指针 overlay（RFC 0018，仅 macOS）。
//!
//! `desktop_action` 走 AX 语义动作（AXPress/AXSetValue 等），系统鼠标
//! 指针纹丝不动，用户无法感知 Agent 的操作落点。本模块在 sidecar 进程
//! 内自建一个置顶、点击穿透的小窗口，把「天工指针」（白底箭头 + 靛紫
//! 渐变描边 + 右下角「天工」徽标）平滑移动到目标位置。指针以「分身」
//! 形式按需出现：本轮首次鼠标手势时从系统鼠标当前位置出现并滑向
//! 目标，之后跟随所有动作落点；轮次结束由插件生命周期钩子收起（淡出），
//! 没有鼠标操作的轮次不出现。空闲超时兜底收起（RFC 0018）。
//! 实现完全位于插件内，不依赖宿主与前端；指针图片源文件
//! `resources/virtual-cursor.svg` 与嵌入式浏览器共用（RFC 0018 预留）。
//!
//! 线程模型：AppKit 断言 `NSApplication`/`NSWindow` 只能在进程主线程
//! 使用（其他线程触发 Objective-C 异常，Rust 无法捕获会直接 abort）。
//! 因此 sidecar 的 `main` 把服务循环交给工作线程，主线程专跑
//! [`run_main_loop`]——持有本进程唯一的 `NSApplication`
//! （ActivationPolicy::Prohibited，无 Dock 图标、不抢焦点），手动泵
//! 事件循环驱动窗口绘制与淡出；`show_at` 经全局 channel 非阻塞投递，
//! 可在任意线程调用；服务收尾时经 `request_shutdown` 结束循环。
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use objc2::rc::{Allocated, Retained};
use objc2::{MainThreadMarker, class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent,
    NSEventMask, NSImage, NSImageView, NSScreen, NSStatusWindowLevel, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSData, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize};

/// 指针内容尺寸（points）；嵌入 PNG 为 512x384（4x）。
const CONTENT_W: f64 = 64.0;
const CONTENT_H: f64 = 48.0;
/// 箭头尖端（hotspot）相对内容左上角的偏移（points），与 SVG 规格
/// `(16,12)/2` 对齐。
const HOTSPOT_X: f64 = 8.0;
const HOTSPOT_Y: f64 = 6.0;
/// 泵节奏 ≈60fps，配合逐帧插值产生平滑移动。
const PUMP_INTERVAL: Duration = Duration::from_millis(16);
const FADE_STEP: f64 = 0.05;
/// 平滑移动：每帧向目标位置指数趋近的比例（60fps 下约 200ms 到达）。
const MOVE_EASE: f64 = 0.16;
/// 距目标小于该距离（points）即吸附，结束移动。
const MOVE_EPSILON: f64 = 0.5;
/// 点击脉冲：总时长与最大放大系数（0→1.25→1 的正弦回弹）。
const PULSE_MS: u64 = 280;
const PULSE_SCALE: f64 = 0.25;

/// 指针 PNG（由 resources/virtual-cursor.svg 按同规格生成，见
/// resources/gen_cursor_png.swift）。
const CURSOR_PNG: &[u8] = include_bytes!("../../../resources/virtual-cursor.png");

static SENDER: OnceLock<Sender<Command>> = OnceLock::new();
static RECEIVER: Mutex<Option<Receiver<Command>>> = Mutex::new(None);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
/// overlay 窗口已就绪（主循环写、任意线程读）：未就绪（窗口创建失败、
/// 测试进程无主循环）时移动等待直接跳过，避免每次点击空等超时。
static OVERLAY_READY: AtomicBool = AtomicBool::new(false);
/// 指针到达回执：主循环落地目标时递增序号并记录落点，`glide_and_wait`
/// 据此同步等待动画真正完成（时序前提：先移动到位、后触发点击）。
static ARRIVAL: std::sync::Mutex<(u64, (f64, f64))> = std::sync::Mutex::new((0, (0.0, 0.0)));
static ARRIVAL_CVAR: std::sync::Condvar = std::sync::Condvar::new();

/// 分身空闲自动隐藏时长：轮次结束钩子未送达（取消、崩溃）时的兜底。
const IDLE_HIDE: Duration = Duration::from_secs(120);

/// overlay 命令。
enum Command {
    /// 平滑移动到目标（AX 坐标）。`summon` 为真时，指针未显示则先在系统
    /// 鼠标当前位置「分身」出现再滑向目标；为假时仅在已显示时跟随。
    MoveTo { x: f64, y: f64, summon: bool },
    /// 开关：开启时在系统鼠标当前位置出现，关闭时淡出（轮次结束收起）。
    SetEnabled(bool),
    /// 点击脉冲：指针移动到目标并播放一次按压回弹动画（点击反馈）。
    ClickPulse { x: f64, y: f64 },
    /// 按键 HUD：在主屏中下部短暂显示键帽序列（key/combo，不含文本输入）。
    KeyCast(Vec<String>),
    /// 查询包含某点（AX 坐标）的屏幕可用区域（扣除菜单栏与 Dock），经
    /// 回执 channel 同步返回 AX 坐标系下的 (x, y, width, height)。
    /// NSScreen 只能在主线程访问，故由 overlay 主循环代答。
    ScreenWorkArea {
        x: f64,
        y: f64,
        reply: Sender<Option<(f64, f64, f64, f64)>>,
    },
}

/// 查询包含 AX 坐标点的屏幕可用区域（AX 坐标，points）。
///
/// 未命中任何屏幕时退回主屏。主循环未运行（测试进程）或超时返回 None。
pub fn screen_work_area_at(x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
    let sender = SENDER.get()?;
    let (reply, rx) = mpsc::channel();
    sender.send(Command::ScreenWorkArea { x, y, reply }).ok()?;
    rx.recv_timeout(Duration::from_millis(1000)).ok().flatten()
}

/// AppKit 屏幕 frame（左下原点）→ AX 坐标（主屏左上原点）。
fn appkit_rect_to_ax(rect: NSRect, main_top: f64) -> (f64, f64, f64, f64) {
    (
        rect.origin.x,
        main_top - (rect.origin.y + rect.size.height),
        rect.size.width,
        rect.size.height,
    )
}

fn contains(rect: (f64, f64, f64, f64), x: f64, y: f64) -> bool {
    x >= rect.0 && x < rect.0 + rect.2 && y >= rect.1 && y < rect.1 + rect.3
}

fn work_area_at(mtm: MainThreadMarker, x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
    // AX 全局坐标以「主屏」（screens[0]，菜单栏所在屏）左上为原点；
    // mainScreen 是键盘焦点所在屏，多显示器时可能不同，不能用于换算。
    let screens = NSScreen::screens(mtm);
    let primary = screens.iter().next()?;
    let primary_frame = primary.frame();
    let main_top = primary_frame.origin.y + primary_frame.size.height;
    let mut fallback = None;
    for screen in screens.iter() {
        let frame = appkit_rect_to_ax(screen.frame(), main_top);
        let visible = appkit_rect_to_ax(screen.visibleFrame(), main_top);
        if fallback.is_none() {
            fallback = Some(visible);
        }
        if contains(frame, x, y) {
            return Some(visible);
        }
    }
    fallback
}

/// 显示按键 HUD（键帽符号序列；非阻塞投递，overlay 未运行时丢弃）。
pub fn key_cast(keys: Vec<String>) {
    if let Some(sender) = SENDER.get() {
        let _ = sender.send(Command::KeyCast(keys));
    }
}

/// 平滑移动指针到屏幕坐标（points，AX 全局坐标系：主屏左上原点）。
///
/// 鼠标手势专用：本轮首次调用时指针从系统鼠标位置分身出现，再滑向
/// 目标。非阻塞投递、失败静默——指针是纯增益可视化，不允许拖垮
/// 桌面动作本身；overlay 主循环未运行（如单元测试进程）时丢弃。
pub fn move_to(x: f64, y: f64) {
    if let Some(sender) = SENDER.get() {
        let _ = sender.send(Command::MoveTo { x, y, summon: true });
    }
}

/// 仅在指针已显示时跟随到目标（AX 语义动作用）：没有鼠标操作的轮次
/// 不出现指针。
pub fn follow_to(x: f64, y: f64) {
    if let Some(sender) = SENDER.get() {
        let _ = sender.send(Command::MoveTo {
            x,
            y,
            summon: false,
        });
    }
}

/// 平滑移动指针到目标并**等待动画真正到达**（同步，带超时兜底）。
///
/// 供鼠标手势保证「先移动到位、后触发点击」的时序：虚拟指针是视觉
/// 主角（首次从系统鼠标位置分身出发、平滑滑行），系统鼠标仅在点击
/// 瞬间闪移借用。overlay 未就绪或超时（动画异常）时立即返回——
/// 可视化是增益，不得拖垮动作本身。
pub fn glide_and_wait(x: f64, y: f64) {
    const ARRIVAL_TIMEOUT: Duration = Duration::from_millis(1500);
    if !OVERLAY_READY.load(Ordering::Acquire) {
        return;
    }
    let Some(sender) = SENDER.get() else {
        return;
    };
    let before = ARRIVAL.lock().unwrap().0;
    if sender.send(Command::MoveTo { x, y, summon: true }).is_err() {
        return;
    }
    let deadline = Instant::now() + ARRIVAL_TIMEOUT;
    let mut state = ARRIVAL.lock().unwrap();
    while state.0 <= before || !arrived_at(state.1, (x, y)) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let (guard, result) = ARRIVAL_CVAR.wait_timeout(state, deadline - now).unwrap();
        state = guard;
        if result.timed_out() && (state.0 <= before || !arrived_at(state.1, (x, y))) {
            return;
        }
    }
}

/// 落点判定：与目标的距离在吸附阈值内即视为到达。
fn arrived_at(point: (f64, f64), target: (f64, f64)) -> bool {
    (point.0 - target.0).abs() < MOVE_EPSILON && (point.1 - target.1).abs() < MOVE_EPSILON
}

/// 点击脉冲：指针移动到目标并播放按压回弹动画（Agent 经
/// `desktop_mouse` 点击手势驱动）。非阻塞投递。
pub fn click_pulse(x: f64, y: f64) {
    if let Some(sender) = SENDER.get() {
        let _ = sender.send(Command::ClickPulse { x, y });
    }
}

/// 开关指针：开启时在系统鼠标当前位置出现；关闭时淡出。轮次结束时
/// 由插件生命周期钩子关闭（收起分身）。非阻塞投递。
pub fn set_enabled(enabled: bool) {
    if let Some(sender) = SENDER.get() {
        let _ = sender.send(Command::SetEnabled(enabled));
    }
}

/// 请求 overlay 主循环退出（服务线程收尾时调用）。
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

/// overlay 主循环：必须在进程主线程调用（占用直至进程退出）。
///
/// 初始化全局 channel 与 `NSApplication`，随后以 30ms 节奏泵事件、
/// 处理指针命令并推进淡出；`request_shutdown` 后返回。
pub fn run_main_loop() {
    let (tx, rx) = mpsc::channel();
    let _ = SENDER.set(tx);
    *RECEIVER.lock().unwrap() = Some(rx);
    let mtm = MainThreadMarker::new().expect("overlay 主循环必须在进程主线程运行");
    let app = NSApplication::sharedApplication(mtm);
    // 无 Dock 图标、不参与激活、永不抢用户焦点。
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let Some(window) = build_window(mtm) else {
        tracing::warn!("虚拟指针 overlay 窗口创建失败，指针可视化不可用");
        wait_for_shutdown();
        return;
    };
    let receiver = RECEIVER.lock().unwrap().take();
    OVERLAY_READY.store(true, Ordering::Release);
    // 按键 HUD 与指针窗口相互独立；创建失败只影响按键显示。
    let mut keycast = super::keycast::KeyCastHud::new(mtm);
    if keycast.is_none() {
        tracing::warn!("按键 HUD 窗口创建失败，按键可视化不可用");
    }
    let mut visible_until: Option<Instant> = None;
    let mut alpha: f64 = 0.0;
    // 分身显示状态：默认隐藏；本轮首次鼠标手势时从系统鼠标位置分身
    // 出现，轮次结束（SetEnabled(false)）或空闲超时后淡出。
    let mut enabled = false;
    let mut last_activity = Instant::now();
    // 平滑移动状态：current 为当前 AX 坐标，target 存在时逐帧趋近。
    // 首次出现或淡出后（不可见）直接落位，可见时连续滑动。
    let mut current: Option<(f64, f64)> = None;
    let mut target: Option<(f64, f64)> = None;
    let mut pulse_until: Option<Instant> = None;
    let mut last_scale: f64 = 1.0;
    let mut screen_top = main_screen_top(mtm);
    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            return;
        }
        let mut dirty = false;
        if let Some(rx) = receiver.as_ref() {
            while let Ok(command) = rx.try_recv() {
                match command {
                    Command::MoveTo { x, y, summon } => {
                        if !enabled && summon {
                            // 分身：从系统鼠标当前位置（AppKit 左下原点 →
                            // AX 左上原点）出现，随后平滑滑向目标。
                            screen_top = main_screen_top(mtm);
                            let location = NSEvent::mouseLocation();
                            current = Some((location.x, screen_top - location.y));
                            if let Some((cx, cy)) = current {
                                window.setFrameOrigin(window_origin(cx, cy, screen_top));
                            }
                            enabled = true;
                        }
                        if enabled {
                            target = Some((x, y));
                            alpha = 1.0;
                            visible_until = None;
                            last_activity = Instant::now();
                            screen_top = main_screen_top(mtm);
                            dirty = true;
                        }
                    }
                    Command::SetEnabled(true) => {
                        if !enabled {
                            screen_top = main_screen_top(mtm);
                            let location = NSEvent::mouseLocation();
                            current = Some((location.x, screen_top - location.y));
                            if let Some((cx, cy)) = current {
                                window.setFrameOrigin(window_origin(cx, cy, screen_top));
                            }
                        }
                        enabled = true;
                        target = None;
                        alpha = 1.0;
                        visible_until = None;
                        last_activity = Instant::now();
                        dirty = true;
                    }
                    Command::SetEnabled(false) => {
                        if enabled {
                            enabled = false;
                            target = None;
                            // 立即进入淡出。
                            visible_until = Some(Instant::now());
                            dirty = true;
                        }
                    }
                    Command::KeyCast(keys) => {
                        if let Some(hud) = keycast.as_mut() {
                            hud.show(&keys, mtm);
                        }
                    }
                    Command::ScreenWorkArea { x, y, reply } => {
                        let _ = reply.send(work_area_at(mtm, x, y));
                    }
                    Command::ClickPulse { x, y } => {
                        if enabled {
                            target = Some((x, y));
                            pulse_until = Some(Instant::now() + Duration::from_millis(PULSE_MS));
                            last_activity = Instant::now();
                            screen_top = main_screen_top(mtm);
                            dirty = true;
                        }
                    }
                }
            }
        }
        // 兜底：轮次结束钩子未送达（取消/异常）时空闲超时自动收起。
        if enabled && target.is_none() && last_activity.elapsed() >= IDLE_HIDE {
            enabled = false;
            visible_until = Some(Instant::now());
            dirty = true;
        }
        // 平滑移动：每帧向目标指数趋近（自带 ease-out），像真实鼠标
        // 连续滑动；到达后吸附并结束移动。
        if let Some((tx, ty)) = target {
            let landed = match current {
                None => Some((tx, ty)),
                Some(_) if alpha <= 0.0 => Some((tx, ty)),
                Some((cx, cy)) => {
                    let nx = cx + (tx - cx) * MOVE_EASE;
                    let ny = cy + (ty - cy) * MOVE_EASE;
                    if (nx - tx).abs() < MOVE_EPSILON && (ny - ty).abs() < MOVE_EPSILON {
                        Some((tx, ty))
                    } else {
                        current = Some((nx, ny));
                        None
                    }
                }
            };
            if let Some(position) = landed {
                current = Some(position);
                target = None;
                // 到达回执：唤醒 glide_and_wait 的等待者（序号+落点）。
                if let Ok(mut state) = ARRIVAL.lock() {
                    state.0 += 1;
                    state.1 = position;
                }
                ARRIVAL_CVAR.notify_all();
            }
            if let Some((cx, cy)) = current {
                let origin = window_origin(cx, cy, screen_top);
                window.setFrameOrigin(origin);
            }
            dirty = true;
        }
        // 点击脉冲：窗口尺寸按正弦回弹缩放（hotspot 始终对准目标点）。
        let scale = match pulse_until {
            Some(deadline) => {
                let total = PULSE_MS as f64;
                let remaining = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis() as f64;
                let progress = (1.0 - remaining / total).clamp(0.0, 1.0);
                if progress >= 1.0 {
                    pulse_until = None;
                    1.0
                } else {
                    1.0 + PULSE_SCALE * (std::f64::consts::PI * progress).sin()
                }
            }
            None => 1.0,
        };
        if scale != last_scale
            && let Some((cx, cy)) = current
        {
            let origin = window_origin_scaled(cx, cy, screen_top, scale);
            let size = NSSize::new(CONTENT_W * scale, CONTENT_H * scale);
            window.setFrame_display(NSRect::new(origin, size), true);
            last_scale = scale;
        }
        pump_events(&app);
        if let Some(hud) = keycast.as_mut() {
            hud.tick(mtm);
        }
        // 缓存系统鼠标当前位置（AX 坐标），供 desktop_mouse 的 drag
        // 手势「借用并归还」读取（NSEvent 仅主线程可用）。
        let location = NSEvent::mouseLocation();
        let screen_top_now = screen_top;
        super::mouse::cache_system_cursor(location.x, screen_top_now - location.y);
        if let Some(deadline) = visible_until
            && Instant::now() >= deadline
        {
            alpha = (alpha - FADE_STEP).max(0.0);
            if alpha == 0.0 {
                visible_until = None;
            }
            dirty = true;
        }
        if dirty {
            window.setAlphaValue(alpha);
        }
        std::thread::sleep(PUMP_INTERVAL);
    }
}

/// 窗口创建失败的兜底等待：保持主线程存活直至服务请求退出，
/// 避免 sidecar 进程过早终止。
fn wait_for_shutdown() {
    while !SHUTDOWN.load(Ordering::Relaxed) {
        std::thread::sleep(PUMP_INTERVAL);
    }
}

/// AX 全局坐标 → overlay 窗口原点（AppKit 全局坐标，主屏左下原点）。
///
/// AX `y` 以主屏左上为原点向下增长；AppKit 以主屏左下为原点向上增长，
/// `screen_top` 为主屏顶边的 AppKit y（单主屏时即主屏高度）。纯函数，
/// 便于单测坐标换算。
fn window_origin(x: f64, y: f64, screen_top: f64) -> NSPoint {
    window_origin_scaled(x, y, screen_top, 1.0)
}

/// 按缩放系数计算窗口原点：脉冲动画时 hotspot 仍对准目标点。
fn window_origin_scaled(x: f64, y: f64, screen_top: f64, scale: f64) -> NSPoint {
    NSPoint::new(
        x - HOTSPOT_X * scale,
        screen_top - y + HOTSPOT_Y * scale - CONTENT_H * scale,
    )
}

/// 主屏顶边的 AppKit y 坐标；读取失败时返回 0（指针纵向偏移，
/// 不影响动作本身）。
fn main_screen_top(mtm: MainThreadMarker) -> f64 {
    NSScreen::mainScreen(mtm)
        .map(|screen| {
            let frame = screen.frame();
            frame.origin.y + frame.size.height
        })
        .unwrap_or(0.0)
}

/// 非阻塞泵出并派发全部挂起的 AppKit 事件（驱动窗口首次绘制等）。
fn pump_events(app: &NSApplication) {
    let past = NSDate::distantPast();
    while let Some(event) = unsafe {
        app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask(u64::MAX),
            Some(&past),
            NSDefaultRunLoopMode,
            true,
        )
    } {
        app.sendEvent(&event);
    }
}

fn build_window(_mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
    let frame = NSRect::new(
        NSPoint::new(-2.0 * CONTENT_W, -2.0 * CONTENT_H),
        NSSize::new(CONTENT_W, CONTENT_H),
    );
    unsafe {
        // SAFETY：NSWindow 类存在且 alloc 返回未初始化实例，随后立即 init。
        let allocated: Allocated<NSWindow> = msg_send![class!(NSWindow), alloc];
        let window = NSWindow::initWithContentRect_styleMask_backing_defer(
            allocated,
            frame,
            NSWindowStyleMask::empty(),
            NSBackingStoreType::Buffered,
            false,
        );
        // NSStatusWindowLevel：盖过普通应用窗口与工具栏。
        window.setLevel(NSStatusWindowLevel);
        // 所有桌面空间可见、可覆盖全屏应用（VS Code 等全屏时仍能看到）。
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setIgnoresMouseEvents(true);
        window.setHasShadow(false);
        window.setAlphaValue(0.0);
        // 纯 ARC 环境：窗口关闭不得触发 release 悬挂。
        window.setReleasedWhenClosed(false);
        let data = NSData::with_bytes(CURSOR_PNG);
        // SAFETY：同上，alloc 后立即 initWithData。
        let allocated_image: Allocated<NSImage> = msg_send![class!(NSImage), alloc];
        let image = NSImage::initWithData(allocated_image, &data)?;
        image.setSize(NSSize::new(CONTENT_W, CONTENT_H));
        let allocated_view: Allocated<NSImageView> = msg_send![class!(NSImageView), alloc];
        let view = NSImageView::initWithFrame(allocated_view, frame);
        view.setImage(Some(&image));
        window.setContentView(Some(&view));
        // 不抢 key/main 状态：用户当前操作零干扰。
        window.orderFrontRegardless();
        Some(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_origin_places_hotspot_on_target() {
        // 主屏高 900：AX (640, 500) → 尖端应落在该点。
        let origin = window_origin(640.0, 500.0, 900.0);
        // 尖端 = 窗口原点 + hotspot 偏移；窗口顶 = origin.y + CONTENT_H，
        // 尖端 AppKit y = origin.y + CONTENT_H - HOTSPOT_Y。
        assert_eq!(origin.x, 640.0 - HOTSPOT_X);
        let tip_y = origin.y + CONTENT_H - HOTSPOT_Y;
        assert_eq!(tip_y, 900.0 - 500.0);
    }

    #[test]
    fn window_origin_at_screen_top_left() {
        // AX (0,0)（主屏左上角）→ 窗口不越过屏幕顶边。
        let origin = window_origin(0.0, 0.0, 900.0);
        assert_eq!(origin.x, -HOTSPOT_X);
        assert_eq!(origin.y + CONTENT_H, 900.0 + HOTSPOT_Y);
    }
}
