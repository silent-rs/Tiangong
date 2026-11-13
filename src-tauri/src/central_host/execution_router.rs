//! 执行路由器：按 `execution_id` 路由 Worker turn 终态（R1，方案 §5.2）。
//!
//! 取代 POC 的 `central_watch_slots`（session 级回调槽）。旧机制的两类
//! 缺陷在此消除：
//!
//! 1. **归属靠猜**：流终态事件不携带 work 标识，fire 侧触发该会话全部
//!    已挂回调，靠回调侧查「registry 当前条目」过滤（review T6）——
//!    同会话快速完成 / 迟到回调都可能错挂。本路由器维护
//!    *session → active execution* 映射，fire 只交给活跃执行的回调。
//!
//! 2. **先挂后投的两难**：先挂回调再投递，投递失败会残留回调（下次
//!    终态重复回报）；后挂回调，投递后瞬间完成的 turn 会丢失终态
//!    （回调无人再触发，工作永挂 running）。本路由器以
//!    *暂存终态 + attach 补发 + 投递失败显式取消* 闭合竞态：
//!
//! ```text
//! register(session, exec)   投递前声明活跃执行
//! fire(session, done, …)    有活跃回调 → 触发；无 → 暂存 (exec, done, …)
//! attach(session, exec, h)  挂载；暂存匹配本执行 → 立即补发并清除
//! cancel(session, exec)     投递失败：清活跃声明与残留回调/暂存
//! ```
//!
//! 一个会话同时至多一个在途 turn（会话维度 upsert、引导并入当前轮），
//! 因此 session → 单活跃执行成立；引导（steer 并入）不换活跃执行，
//! 终态仍路由到原执行——与 POC「归属保持原工作」语义一致。

use std::collections::HashMap;
use std::sync::Mutex;

use crate::commands::StreamFinishedHandler;

/// 单会话的监视状态。
#[derive(Default)]
struct SessionWatch {
    /// 已挂载待终态的回调（execution_id → handler）。引导并入等时序下
    /// 可能短暂存在多个，保留 Vec；fire 只触发活跃执行那一个。
    handlers: Vec<(String, StreamFinishedHandler)>,
    /// fire 先于 attach 的暂存终态（execution_id, done, reason）。
    buffered: Option<(String, bool, String)>,
    /// 会话当前活跃执行（register 设置；cancel 清除）。
    active_execution: Option<String>,
}

/// 全局执行路由器（挂载于 [`crate::app::TiangongApp`]）。
#[derive(Default)]
pub struct ExecutionRouter {
    sessions: Mutex<HashMap<String, SessionWatch>>,
}

impl ExecutionRouter {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionWatch>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 投递前声明会话的活跃执行。
    ///
    /// 必须在消息投递**之前**调用：瞬间完成的 turn fire 时才能把终态
    /// 暂存到正确的 execution_id 上，attach 后补发不丢归属。
    pub fn register(&self, session_id: &str, execution_id: &str) {
        let mut sessions = self.lock();
        let watch = sessions.entry(session_id.to_string()).or_default();
        watch.active_execution = Some(execution_id.to_string());
        // 新执行开始：旧执行若仍有残留回调/暂存（理论上已被消费或
        // cancel），一并清除——同会话新 turn 的终态不得归属旧执行。
        watch.handlers.clear();
        watch.buffered = None;
    }

    /// 挂载终态回调；若该执行已有暂存终态（fire 先到），立即补发。
    pub fn attach(&self, session_id: &str, execution_id: &str, handler: StreamFinishedHandler) {
        // 第一步：挂载并判断是否需要补发（锁内完成状态变更）。
        let replay = {
            let mut sessions = self.lock();
            let Some(watch) = sessions.get_mut(session_id) else {
                return;
            };
            watch.handlers.push((execution_id.to_string(), handler));
            match watch.buffered.take() {
                // 暂存匹配本执行：立即补发（回调刚挂即触发一次）。
                Some((buffered_exec, done, reason)) if buffered_exec == execution_id => {
                    Some((done, reason))
                }
                // 暂存属其他执行（stale）：丢弃。
                _ => None,
            }
        };
        let Some((done, reason)) = replay else {
            return;
        };
        // 第二步：取回刚挂载的回调并补发（锁外触发，避免回调内再进锁）。
        let handler = {
            let mut sessions = self.lock();
            let Some(watch) = sessions.get_mut(session_id) else {
                return;
            };
            let Some(position) = watch
                .handlers
                .iter()
                .rposition(|(exec, _)| exec == execution_id)
            else {
                return;
            };
            watch.handlers.remove(position).1
        };
        handler(done, reason);
    }

    /// turn 终态路由：触发活跃执行的回调；回调未挂（先于 attach）则暂存。
    ///
    /// 活跃执行之外的残留回调一并清理（stale，warn 记录）。
    pub fn fire(&self, session_id: &str, done: bool, reason: String) {
        let mut action = None;
        {
            let mut sessions = self.lock();
            let Some(watch) = sessions.get_mut(session_id) else {
                return;
            };
            match watch.active_execution.clone() {
                Some(active) => {
                    let position = watch.handlers.iter().rposition(|(exec, _)| *exec == active);
                    match position {
                        Some(position) => {
                            let (_, handler) = watch.handlers.remove(position);
                            // 其余回调全部 stale，清掉。
                            let stale = watch.handlers.len();
                            if stale > 0 {
                                tracing::warn!(
                                    session_id,
                                    stale_callbacks = stale,
                                    "执行路由：清除非活跃执行的残留回调"
                                );
                            }
                            watch.handlers.clear();
                            watch.buffered = None;
                            action = Some(handler);
                        }
                        None => {
                            // 回调未挂（投递后瞬间完成）：暂存到活跃执行，
                            // attach 时补发。
                            watch.buffered = Some((active, done, reason));
                            return;
                        }
                    }
                }
                // 无活跃执行声明（普通会话误触发等）：丢弃，清残留。
                None => {
                    if !watch.handlers.is_empty() {
                        tracing::warn!(session_id, "执行路由：终态无活跃执行声明，回调已清理");
                    }
                    watch.handlers.clear();
                }
            }
        }
        if let Some(handler) = action {
            handler(done, reason);
        }
    }

    /// 投递失败清理：撤销活跃声明，移除该执行的回调与暂存。
    pub fn cancel(&self, session_id: &str, execution_id: &str) {
        let mut sessions = self.lock();
        let Some(watch) = sessions.get_mut(session_id) else {
            return;
        };
        watch
            .handlers
            .retain(|(exec, _)| exec.as_str() != execution_id);
        if watch
            .buffered
            .as_ref()
            .is_some_and(|(exec, _, _)| exec == execution_id)
        {
            watch.buffered = None;
        }
        if watch.active_execution.as_deref() == Some(execution_id) {
            watch.active_execution = None;
            watch.buffered = None;
        }
    }

    /// 调试/测试：该会话剩余已挂回调数。
    pub fn pending_handler_count(&self, session_id: &str) -> usize {
        self.lock()
            .get(session_id)
            .map(|watch| watch.handlers.len())
            .unwrap_or(0)
    }

    /// 调试/测试：会话当前活跃执行。
    pub fn active_execution(&self, session_id: &str) -> Option<String> {
        self.lock()
            .get(session_id)
            .and_then(|watch| watch.active_execution.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn recorder() -> (Arc<AtomicUsize>, Arc<AtomicUsize>, StreamFinishedHandler) {
        let done_count = Arc::new(AtomicUsize::new(0));
        let reason_last = Arc::new(AtomicUsize::new(0));
        let d = Arc::clone(&done_count);
        let r = Arc::clone(&reason_last);
        (
            done_count,
            reason_last,
            Box::new(move |done: bool, _reason: String| {
                d.fetch_add(1, Ordering::SeqCst);
                r.store(if done { 1 } else { 0 }, Ordering::SeqCst);
            }),
        )
    }

    /// 基本链路：register → attach → fire，回调恰好触发一次。
    #[test]
    fn routes_terminal_to_active_execution() {
        let router = ExecutionRouter::new();
        let (count, done_flag, handler) = recorder();
        router.register("s1", "exec-a");
        router.attach("s1", "exec-a", handler);
        router.fire("s1", true, "ok".into());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(done_flag.load(Ordering::SeqCst), 1);
        assert_eq!(router.pending_handler_count("s1"), 0, "触发后清空");
    }

    /// 快速完成竞态：fire 先于 attach（暂存 → attach 补发，不丢终态）。
    #[test]
    fn fire_before_attach_buffers_and_replays() {
        let router = ExecutionRouter::new();
        router.register("s1", "exec-fast");
        router.fire("s1", false, "model error".into());
        let (count, done_flag, handler) = recorder();
        router.attach("s1", "exec-fast", handler);
        assert_eq!(count.load(Ordering::SeqCst), 1, "attach 应立即补发暂存终态");
        assert_eq!(done_flag.load(Ordering::SeqCst), 0, "失败终态 done=false");
        // 补发只一次：再次 fire 无回调（新 turn 前无活跃声明外的挂载），
        // 老回调不得重复触发。
        router.fire("s1", true, "again".into());
        assert_eq!(count.load(Ordering::SeqCst), 1, "回调一次性");
    }

    /// 投递失败残留：cancel 清除活跃声明与回调，后续终态不误路由。
    #[test]
    fn cancel_removes_pending_callback_and_active() {
        let router = ExecutionRouter::new();
        let (count, _done, handler) = recorder();
        router.register("s1", "exec-x");
        router.attach("s1", "exec-x", handler);
        router.cancel("s1", "exec-x");
        assert_eq!(router.pending_handler_count("s1"), 0);
        assert_eq!(router.active_execution("s1"), None);
        router.fire("s1", true, "late".into());
        assert_eq!(count.load(Ordering::SeqCst), 0, "取消后终态不得触发回调");
    }

    /// 旧执行残留回调不得接收新执行终态：register 新执行时清残留。
    #[test]
    fn stale_callback_from_old_execution_is_dropped() {
        let router = ExecutionRouter::new();
        let (old_count, _, old_handler) = recorder();
        router.register("s1", "exec-old");
        router.attach("s1", "exec-old", old_handler);
        // 未 fire（旧 turn 永未完成）；新执行到来
        router.register("s1", "exec-new");
        let (new_count, _, new_handler) = recorder();
        router.attach("s1", "exec-new", new_handler);
        router.fire("s1", true, "done".into());
        assert_eq!(old_count.load(Ordering::SeqCst), 0, "旧执行回调必须被清理");
        assert_eq!(new_count.load(Ordering::SeqCst), 1, "新执行接收终态");
    }

    /// 同一执行重复 fire：第二次暂存（无回调），attach 不存在——不触发。
    #[test]
    fn duplicate_fire_without_handler_is_harmless() {
        let router = ExecutionRouter::new();
        router.register("s1", "exec-d");
        router.fire("s1", true, "first".into());
        router.fire("s1", true, "second".into());
        let (count, _, handler) = recorder();
        router.attach("s1", "exec-d", handler);
        assert_eq!(count.load(Ordering::SeqCst), 1, "重复 fire 只补发最后一次");
    }

    /// 未注册会话的 fire（普通会话）静默丢弃。
    #[test]
    fn fire_on_unknown_session_is_noop() {
        let router = ExecutionRouter::new();
        router.fire("s-other", true, "ignored".into());
        assert_eq!(router.pending_handler_count("s-other"), 0);
    }
}
