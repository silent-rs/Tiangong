//! 浏览器页面自动观察器。
//!
//! 原 `tiangong-core::react::helpers::maybe_inject_browser_update` 的能力下沉实现（#225）。
//! core 不再感知浏览器——本模块在插件内部周期性观察浏览器当前页面，发现变化时经
//! [`PluginFeedbackTx`] 投递 `browser_data` 工具结果，由 core 的 feedback 通道统一注入会话。
//!
//! 与原 core 同步观察的差异（已确认接受）：
//! - 不再在 ReAct 轮次起始 / 每个工具后强制同步观察，改为后台任务按节流周期轮询；
//! - round-0 的「首个模型请求前同步观察」精度不再保证，取决于后台任务与排空时序。
//!
//! 节流策略沿用原 core 逻辑：首次检测无间隔；后续至少间隔 5 秒；URL 变化 / 文本差
//! >500 字符 / 有用户操作 feedback 时才投递，避免无变化刷屏。
//!
//! ## session 隔离
//!
//! [`BrowserWatcher`](crate::webview_host::watcher::BrowserWatcher) 是 session-scoped，随
//! [`BrowserPlugin`](crate::webview_host::plugin::BrowserPlugin) 生命周期存在：每个 Core/session 构造
//! 自己的 watcher，只向当前 plugin 持有的 feedback channel 注入 `browser_data`。
//! 不做跨 session 广播——后台/历史 session 不会被无关页面污染。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::json;
use tiangong_core::core::plugin::PluginFeedbackTx;
use tokio::time::sleep;

use crate::webview_host::capability::PageFetcher;
use crate::webview_host::types::{format_browser_events, PageStatus};

/// 两次自动观察之间的最小间隔。
const MIN_OBSERVE_INTERVAL: Duration = Duration::from_secs(5);
/// 单次 observe_page 的超时（与原 core 一致）。
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(3);
/// 文本长度变化超过该阈值才投递（避免微小抖动）。
const TEXT_DIFF_THRESHOLD: u64 = 500;
/// 后台轮询的 tick 间隔（实际 observe 仍受 MIN_OBSERVE_INTERVAL 节流）。
const POLL_TICK: Duration = Duration::from_secs(2);
/// observe 连续超时/失败的最大允许次数；超过后进入退避，拉长 tick 间隔，
/// 避免复杂页面持续触发 eval 堆积加剧卡顿。
const MAX_CONSECUTIVE_FAILURES: u32 = 2;
/// 退避倍数：连续失败达到阈值后，本轮 tick 间隔乘以该倍数。
const BACKOFF_MULTIPLIER: u64 = 4;

/// 浏览器页面观察器：持有 fetcher 与当前 session 的 feedback 通道，后台周期性观察，
/// 变化时只向自己的通道投递 `browser_data`。
///
/// 生命周期由 [`BrowserPlugin`](crate::webview_host::plugin::BrowserPlugin) 管理：
/// - 构造时创建，但不立即 spawn；
/// - `set_feedback_tx` 注入当前 session 通道时懒启动后台任务（同一实例只启动一次）；
/// - 通道关闭后后台任务检测到并自然跳过（空转等待下次注入）。
pub struct BrowserWatcher {
    fetcher: Arc<dyn PageFetcher>,
    /// 当前 session 的 feedback 通道；未注入时为 None，后台任务空转跳过。
    feedback_tx: RwLock<Option<PluginFeedbackTx>>,
    /// 后台任务是否已启动（防止 set_feedback_tx 重复调用时重复 spawn）。
    started: AtomicBool,
    /// 上一次观察到的快照（url + 文本长度），用于变化检测。
    last_snapshot: RwLock<Option<(String, usize)>>,
    /// 上一次观察时间，用于节流。
    last_check: RwLock<Option<Instant>>,
    /// 暂停标志：turn 被取消时置位，watcher 停止 observe/inject；
    /// 新 turn 开始时清除，恢复推送。
    paused: AtomicBool,
    /// 连续 observe 失败/超时次数；超过阈值后退避，减少对卡顿页面的轮询压力。
    consecutive_failures: std::sync::atomic::AtomicU32,
}

impl BrowserWatcher {
    pub fn new(fetcher: Arc<dyn PageFetcher>) -> Self {
        Self {
            fetcher,
            feedback_tx: RwLock::new(None),
            started: AtomicBool::new(false),
            last_snapshot: RwLock::new(None),
            last_check: RwLock::new(None),
            paused: AtomicBool::new(false),
            consecutive_failures: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// 暂停页面观察与 inject（turn 被取消时调用）。
    pub fn pause_inject(&self) {
        self.paused.store(true, Ordering::Release);
    }

    /// 恢复页面观察与 inject（新 turn 开始时调用）。
    pub fn resume_inject(&self) {
        self.paused.store(false, Ordering::Release);
    }

    /// 注入当前 session 的 feedback 通道并懒启动后台观察任务。
    ///
    /// 可重复调用（如 engine 重建后重新注入通道）：仅更新通道引用，后台任务
    /// 持有的是 watcher 的 `Arc`，会自动读到新通道；`started` 保证只 spawn 一次。
    /// 注入 feedback 通道（不启动后台任务，待 session_id 就绪后调 start）。
    pub fn set_feedback_tx(&self, tx: PluginFeedbackTx) {
        if let Ok(mut guard) = self.feedback_tx.write() {
            *guard = Some(tx);
        }
    }

    /// 启动后台观察任务（on_session_ready 注入 session_id 后调用）。
    pub fn start(self: &Arc<Self>) {
        self.ensure_started();
    }

    /// 确保后台任务已启动（同一实例只 spawn 一次）。
    fn ensure_started(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let watcher = self.clone();
        tokio::spawn(async move {
            watcher.run_loop().await;
        });
    }

    async fn run_loop(&self) {
        tracing::debug!("browser watcher started");
        loop {
            // 连续失败时退避：拉长 tick 间隔，减轻卡顿页面的轮询压力。
            // 正常情况下连续失败为 0，按标准 POLL_TICK 间隔运行。
            let failures = self.consecutive_failures.load(Ordering::Acquire);
            let tick = if failures >= MAX_CONSECUTIVE_FAILURES {
                POLL_TICK * BACKOFF_MULTIPLIER as u32
            } else {
                POLL_TICK
            };
            sleep(tick).await;
            // feedback channel 关闭时退出，避免 session 结束后 task 泄漏
            let closed = self
                .feedback_tx
                .read()
                .ok()
                .and_then(|g| g.as_ref().map(|tx| tx.is_closed()))
                .unwrap_or(true);
            if closed {
                tracing::debug!("browser watcher: feedback channel closed, stopping");
                self.started.store(false, Ordering::SeqCst);
                break;
            }
            self.maybe_observe_and_inject().await;
        }
    }

    /// 执行一次节流检查 + observe + 变化检测 + 向当前 session 通道投递。
    async fn maybe_observe_and_inject(&self) {
        // turn 被取消时暂停推送，避免向已终止的 turn 注入滞留内容。
        if self.paused.load(Ordering::Acquire) {
            return;
        }
        // 通道未注入或已关闭时跳过（没有会话需要通知）
        let tx = match self.feedback_tx.read() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        let Some(tx) = tx else { return };
        if tx.is_closed() {
            return;
        }

        // 节流：首次无限制；后续至少间隔 MIN_OBSERVE_INTERVAL
        let now = Instant::now();
        let has_prev = self
            .last_snapshot
            .read()
            .map(|g| g.is_some())
            .unwrap_or(false);
        if has_prev {
            let too_soon = self
                .last_check
                .read()
                .ok()
                .and_then(|g| g.as_ref().copied())
                .map(|prev| now.duration_since(prev) < MIN_OBSERVE_INTERVAL)
                .unwrap_or(false);
            if too_soon {
                return;
            }
        }
        if let Ok(mut g) = self.last_check.write() {
            *g = Some(now);
        }

        // observe_page（带超时）
        let snapshot =
            match tokio::time::timeout(OBSERVE_TIMEOUT, self.fetcher.observe_page()).await {
                Ok(Some(s)) => s,
                Ok(None) => return,
                Err(_) => {
                    self.consecutive_failures.fetch_add(1, Ordering::AcqRel);
                    tracing::warn!(
                        timeouts = self.consecutive_failures.load(Ordering::Acquire),
                        "browser watcher: observe timeout"
                    );
                    return;
                }
            };
        if matches!(&snapshot.status, PageStatus::Loading | PageStatus::Error(_)) {
            // 浏览器不可见时 handler 返回 Error status，计入失败用于触发退避
            self.consecutive_failures.fetch_add(1, Ordering::AcqRel);
            return;
        }
        // 成功拿到有效快照，重置连续失败计数
        self.consecutive_failures.store(0, Ordering::Release);
        if snapshot.url.is_empty() {
            return;
        }

        let feedback = format_browser_events(&snapshot.events);
        let has_feedback = feedback.is_some();

        // 变化检测：首次必投递；之后看 URL 变化 / 文本差 / 是否有用户操作 feedback
        let prev = self
            .last_snapshot
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|(u, l)| (u.clone(), *l)));
        let should_inject = match prev {
            None => true,
            Some((prev_url, prev_len)) => {
                has_feedback
                    || prev_url != snapshot.url
                    || (snapshot.text.len() as u64).abs_diff(prev_len as u64) > TEXT_DIFF_THRESHOLD
            }
        };

        let new_state = (snapshot.url.clone(), snapshot.text.len());

        if !should_inject {
            if let Ok(mut g) = self.last_snapshot.write() {
                *g = Some(new_state);
            }
            return;
        }

        // 构造 payload（结构与原 core maybe_inject_browser_update 一致）
        let tabs: Vec<(String, String, String)> = snapshot
            .tabs
            .iter()
            .map(|t| (t.id.clone(), t.url.clone(), t.title.clone()))
            .collect();
        let payload = json!({
            "title": snapshot.title,
            "url": snapshot.url,
            // 推送边界统一限长（头尾保留），Agent 需细节时主动 web_page_text。
            "text": crate::webview_host::types::clip_head_tail(
                &snapshot.text,
                crate::webview_host::types::PAGE_PUSH_MAX_CHARS,
            ),
            "tabs": tabs,
            "active_tab_id": snapshot.active_tab_id,
            "feedback": feedback,
        });
        // 纯页面快照可被更新快照顶替；携带用户操作反馈的按事件逐条保留。
        if has_feedback {
            tx.inject_tool("browser_data", payload);
        } else {
            tx.inject_snapshot("browser_data", payload);
        }
        tracing::info!(
            url = %snapshot.url,
            title = %snapshot.title,
            text_len = snapshot.text.len(),
            events_len = snapshot.events.len(),
            has_feedback,
            "browser watcher injected browser_data"
        );

        if let Ok(mut g) = self.last_snapshot.write() {
            *g = Some(new_state);
        }
    }
}
