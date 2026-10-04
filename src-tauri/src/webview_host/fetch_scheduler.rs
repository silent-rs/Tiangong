//! 浏览器命令调度：让同一批次的多个 web_fetch 真正并行。
//!
//! 命令消费循环此前逐条 await，后一条 FetchPage 要等前一条导航 + 抓取全部
//! 结束才开始，同批并发调用在宿主层被串行化。调度规则：
//!
//! - **FetchPage 共享执行**：持会话共享锁，在独立任务中运行；
//! - **同主域名串行**：Agent 按主域名复用工作标签，同域名并发导航会互相
//!   顶掉页面（「页面加载已被新的导航替代」），因此同会话同主域名的抓取
//!   按到达顺序串行，不同主域名之间并行加载；
//! - **导航起步串行**：`navigate_for_agent` 先切活跃标签再导航活跃标签，
//!   两次起步交叉会把 A 的地址导航进 B 的标签，起步阶段按会话互斥
//!   （仅覆盖切标签 + 发起导航，不覆盖页面加载等待）；
//! - **其余命令独占**：表单、点击、正文读取等作用于活跃标签的命令持会话
//!   独占锁，等在途抓取全部结束后再执行，保持与此前串行一致的可见顺序。
//!
//! 锁在消费循环中按到达顺序获取（共享锁先取再派发任务），命令之间的相对
//! 顺序与改动前一致。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, RwLock};

#[derive(Default)]
struct SessionLocks {
    /// FetchPage 等可并行命令持读锁，作用于活跃标签的命令持写锁。
    access: Arc<RwLock<()>>,
    /// 导航起步（切活跃标签 + 发起导航）互斥。
    navigation_start: Arc<AsyncMutex<()>>,
    /// 主域名 → 抓取互斥（同域名复用同一工作标签）。
    domains: HashMap<String, Arc<AsyncMutex<()>>>,
}

/// 按会话维护的浏览器命令调度锁。
#[derive(Default)]
pub(crate) struct BrowserCommandScheduler {
    sessions: Mutex<HashMap<String, SessionLocks>>,
}

impl BrowserCommandScheduler {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn with_session<T>(&self, session_id: &str, f: impl FnOnce(&mut SessionLocks) -> T) -> T {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(sessions.entry(session_id.to_string()).or_default())
    }

    /// 可并行命令的共享访问（不阻塞其他共享命令，等待独占命令结束）。
    pub(crate) async fn shared(&self, session_id: &str) -> OwnedRwLockReadGuard<()> {
        let access = self.with_session(session_id, |locks| locks.access.clone());
        access.read_owned().await
    }

    /// 作用于活跃标签的命令的独占访问（等待在途共享命令全部结束）。
    pub(crate) async fn exclusive(
        &self,
        session_id: &str,
    ) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        let access = self.with_session(session_id, |locks| locks.access.clone());
        access.write_owned().await
    }

    /// 非阻塞独占：有在途命令时返回 None（供可跳过的周期性观察使用）。
    pub(crate) fn try_exclusive(
        &self,
        session_id: &str,
    ) -> Option<tokio::sync::OwnedRwLockWriteGuard<()>> {
        let access = self.with_session(session_id, |locks| locks.access.clone());
        access.try_write_owned().ok()
    }

    /// 同会话同主域名的抓取互斥。
    pub(crate) async fn domain(&self, session_id: &str, domain: &str) -> OwnedMutexGuard<()> {
        let lock = self.with_session(session_id, |locks| {
            // 顺带回收无人持有的域名锁，避免长会话累积。
            locks.domains.retain(|_, lock| Arc::strong_count(lock) > 1);
            locks.domains.entry(domain.to_string()).or_default().clone()
        });
        lock.lock_owned().await
    }

    /// 导航起步互斥（切活跃标签 + 发起导航）。
    pub(crate) async fn navigation_start(&self, session_id: &str) -> OwnedMutexGuard<()> {
        let lock = self.with_session(session_id, |locks| locks.navigation_start.clone());
        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn hold_domain(
        scheduler: Arc<BrowserCommandScheduler>,
        session: &'static str,
        domain: &'static str,
        log: Arc<Mutex<Vec<String>>>,
        tag: &'static str,
    ) {
        let _shared = scheduler.shared(session).await;
        let _domain = scheduler.domain(session, domain).await;
        log.lock().unwrap().push(format!("{tag}:start"));
        tokio::time::sleep(Duration::from_millis(80)).await;
        log.lock().unwrap().push(format!("{tag}:end"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_domains_run_in_parallel() {
        let scheduler = Arc::new(BrowserCommandScheduler::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let started = std::time::Instant::now();
        tokio::join!(
            hold_domain(scheduler.clone(), "s", "a.com", log.clone(), "a"),
            hold_domain(scheduler.clone(), "s", "b.com", log.clone(), "b"),
            hold_domain(scheduler.clone(), "s", "c.com", log.clone(), "c"),
        );
        // 三个不同域名并行：总耗时接近单个（80ms），远小于串行的 240ms。
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "{:?}",
            started.elapsed()
        );
        let log = log.lock().unwrap();
        let first_end = log.iter().position(|e| e.ends_with(":end")).unwrap();
        assert_eq!(first_end, 3, "三个任务都应在任一结束前开始：{log:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_domain_is_serialized_in_arrival_order() {
        let scheduler = Arc::new(BrowserCommandScheduler::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let first = tokio::spawn(hold_domain(
            scheduler.clone(),
            "s",
            "a.com",
            log.clone(),
            "1",
        ));
        tokio::time::sleep(Duration::from_millis(10)).await;
        let second = tokio::spawn(hold_domain(
            scheduler.clone(),
            "s",
            "a.com",
            log.clone(),
            "2",
        ));
        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec!["1:start", "1:end", "2:start", "2:end"]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sessions_are_independent() {
        let scheduler = Arc::new(BrowserCommandScheduler::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let started = std::time::Instant::now();
        tokio::join!(
            hold_domain(scheduler.clone(), "s1", "a.com", log.clone(), "1"),
            hold_domain(scheduler.clone(), "s2", "a.com", log.clone(), "2"),
        );
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exclusive_waits_for_inflight_shared_and_blocks_later_shared() {
        let scheduler = Arc::new(BrowserCommandScheduler::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        // 在途抓取先取得共享锁。
        let shared = scheduler.shared("s").await;
        let exclusive = {
            let scheduler = scheduler.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let _guard = scheduler.exclusive("s").await;
                log.lock().unwrap().push("exclusive".to_string());
                tokio::time::sleep(Duration::from_millis(30)).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            log.lock().unwrap().is_empty(),
            "独占命令必须等在途共享命令结束"
        );
        // 独占命令排队后到达的共享命令排在其后（保持到达顺序）。
        let later = {
            let scheduler = scheduler.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let _guard = scheduler.shared("s").await;
                log.lock().unwrap().push("later-shared".to_string());
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(shared);
        exclusive.await.unwrap();
        later.await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["exclusive", "later-shared"]);
    }

    #[tokio::test]
    async fn idle_domain_locks_are_reclaimed() {
        let scheduler = BrowserCommandScheduler::new();
        drop(scheduler.domain("s", "a.com").await);
        drop(scheduler.domain("s", "b.com").await);
        let held = scheduler.domain("s", "c.com").await;
        let count = scheduler.with_session("s", |locks| locks.domains.len());
        assert_eq!(count, 1, "无人持有的域名锁应被回收");
        drop(held);
    }
}
