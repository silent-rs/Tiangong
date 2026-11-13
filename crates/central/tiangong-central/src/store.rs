//! 中枢模式 JSON 持久化（沿用 `tiangong-core` 会话与 scheduler `JobStore` 范式）。
//!
//! 存储布局（POC）：
//! ```text
//! <base>/
//!   goals.json                 # 全部用户目标（数组）
//!   work_items.json            # 全部后台工作项（数组）
//!   events/<work_item_id>.json # 每个工作项的事件时间线（追加重写）
//! ```
//!
//! 写入策略：同目录临时文件 + 原子替换（同 `tiangong-core` 的
//! `atomic_replace_file`），进程内共享一把写锁，避免中枢与后台并发写坏文件。
//! 事件必须持久化（§8.4）：中枢重启、窗口关闭、模式切换或上下文压缩
//! 都不应成为状态丢失的理由。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::event::WorkEvent;
use crate::ids::now_text;
use crate::model::{UserGoal, WorkItem};
use crate::status::WorkItemStatus;

/// 同一进程内的持久化写入共用此锁（与 `tiangong-core::session` 同策略）。
static PERSISTENCE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 在目标文件所在目录写入临时文件，并原子替换目标文件。
fn atomic_replace_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let _write_guard = PERSISTENCE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;
    temp_file.as_file_mut().write_all(content)?;
    temp_file.as_file_mut().sync_all()?;
    temp_file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// 中枢模式存储。
///
/// 仅持有路径、可 `Clone`（同 scheduler `JobStore`），便于把同一存储根
/// 传给中枢协调与后台事件写入任务。
#[derive(Clone)]
pub struct CentralStore {
    goals_path: PathBuf,
    work_items_path: PathBuf,
    events_dir: PathBuf,
}

impl CentralStore {
    /// 在指定目录打开或创建存储。
    pub fn open_at(base: PathBuf) -> Result<Self> {
        let events_dir = base.join("events");
        std::fs::create_dir_all(&events_dir)
            .with_context(|| format!("创建中枢事件目录失败: {}", events_dir.display()))?;
        let store = Self {
            goals_path: base.join("goals.json"),
            work_items_path: base.join("work_items.json"),
            events_dir,
        };
        store.ensure_file(&store.goals_path)?;
        store.ensure_file(&store.work_items_path)?;
        Ok(store)
    }

    fn ensure_file(&self, path: &Path) -> Result<()> {
        if !path.exists() {
            atomic_replace_file(path, b"[]")
                .with_context(|| format!("初始化存储文件失败: {}", path.display()))?;
        }
        Ok(())
    }

    fn load_json_array<T>(&self, path: &Path) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("读取存储文件失败: {}", path.display()))?;
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&content)
            .with_context(|| format!("解析存储文件失败: {}", path.display()))
    }

    fn save_json_array<T>(&self, path: &Path, items: &[T]) -> Result<()>
    where
        T: serde::Serialize,
    {
        let json = serde_json::to_string_pretty(items)?;
        atomic_replace_file(path, json.as_bytes())
            .with_context(|| format!("写入存储文件失败: {}", path.display()))?;
        Ok(())
    }

    // ── 用户目标 ────────────────────────────────────────────────

    /// 写入（新增或整体替换）一个用户目标。
    pub fn insert_goal(&self, goal: &UserGoal) -> Result<()> {
        let mut goals = self.load_goals()?;
        match goals.iter().position(|g| g.id == goal.id) {
            Some(index) => goals[index] = goal.clone(),
            None => goals.push(goal.clone()),
        }
        self.save_json_array(&self.goals_path, &goals)
    }

    pub fn get_goal(&self, id: &str) -> Result<Option<UserGoal>> {
        Ok(self.load_goals()?.into_iter().find(|goal| goal.id == id))
    }

    /// 按创建时间正序列出全部目标。
    pub fn load_goals(&self) -> Result<Vec<UserGoal>> {
        let mut goals = self.load_json_array::<UserGoal>(&self.goals_path)?;
        goals.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(goals)
    }

    // ── 后台工作项 ──────────────────────────────────────────────

    /// 写入（新增或整体替换）一个工作项。
    pub fn insert_work_item(&self, item: &WorkItem) -> Result<()> {
        let mut items = self.load_work_items()?;
        match items.iter().position(|w| w.id == item.id) {
            Some(index) => items[index] = item.clone(),
            None => items.push(item.clone()),
        }
        self.save_json_array(&self.work_items_path, &items)
    }

    pub fn get_work_item(&self, id: &str) -> Result<Option<WorkItem>> {
        Ok(self
            .load_work_items()?
            .into_iter()
            .find(|item| item.id == id))
    }

    /// 按创建时间正序列出全部工作项。
    pub fn load_work_items(&self) -> Result<Vec<WorkItem>> {
        let mut items = self.load_json_array::<WorkItem>(&self.work_items_path)?;
        items.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(items)
    }

    /// 状态机强制转移：校验 `from → target` 合法后落盘并返回更新后的工作项。
    ///
    /// 非法转移直接失败（如 `completed → running`、`draft → running`），
    /// 保证「完成状态必须可验证」不被旁路改写（§19 原则 6）。
    /// 转入 `completed` 时自动回填 `completed_at`。
    pub fn transition_work_item(&self, id: &str, target: WorkItemStatus) -> Result<WorkItem> {
        let mut items = self.load_work_items()?;
        let item = items
            .iter_mut()
            .find(|item| item.id == id)
            .with_context(|| format!("工作项不存在: {id}"))?;
        let from = item.status;
        if !from.can_transition_to(target) {
            bail!(
                "非法状态转移: {} → {}（工作项 {}，状态机见设计稿 §8.1）",
                from.as_str(),
                target.as_str(),
                id
            );
        }
        item.status = target;
        item.updated_at = now_text();
        if target == WorkItemStatus::Completed {
            item.completed_at = Some(item.updated_at.clone());
        }
        let updated = item.clone();
        self.save_json_array(&self.work_items_path, &items)?;
        Ok(updated)
    }

    // ── 事件 ────────────────────────────────────────────────────

    /// 追加事件到对应工作项的时间线文件（按 `created_at` 正序持久化）。
    pub fn append_event(&self, event: &WorkEvent) -> Result<()> {
        let mut events = self.load_events(&event.work_item_id)?;
        events.push(event.clone());
        let path = self.event_path(&event.work_item_id);
        self.save_json_array(&path, &events)
    }

    /// 便捷入口：为工作项创建并追加一个事件。
    pub fn record_event(
        &self,
        work_item_id: &str,
        event_type: crate::event::WorkEventType,
    ) -> Result<WorkEvent> {
        let event = WorkEvent::new(work_item_id, event_type);
        self.append_event(&event)?;
        Ok(event)
    }

    /// 读取工作项的事件时间线（正序）。
    pub fn load_events(&self, work_item_id: &str) -> Result<Vec<WorkEvent>> {
        let path = self.event_path(work_item_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut events = self.load_json_array::<WorkEvent>(&path)?;
        events.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(events)
    }

    fn event_path(&self, work_item_id: &str) -> PathBuf {
        self.events_dir.join(format!("{work_item_id}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::WorkEventType;
    use crate::model::UserGoal;

    fn temp_store() -> (tempfile::TempDir, CentralStore) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let store = CentralStore::open_at(dir.path().to_path_buf()).expect("打开存储失败");
        (dir, store)
    }

    #[test]
    fn goal_round_trip_across_instances() {
        let (dir, store) = temp_store();
        let goal = UserGoal::new("conv_1", "排查支付服务响应变慢");
        store.insert_goal(&goal).unwrap();

        // 重新打开（模拟重启恢复，§15.2）
        let reopened = CentralStore::open_at(dir.path().to_path_buf()).unwrap();
        let loaded = reopened.get_goal(&goal.id).unwrap().expect("目标应存在");
        assert_eq!(loaded.id, goal.id);
        assert_eq!(loaded.title, "排查支付服务响应变慢");
    }

    #[test]
    fn goal_upsert_replaces_same_id() {
        let (_dir, store) = temp_store();
        let mut goal = UserGoal::new("conv_1", "初稿");
        store.insert_goal(&goal).unwrap();
        goal.title = "修订".to_string();
        store.insert_goal(&goal).unwrap();
        assert_eq!(store.load_goals().unwrap().len(), 1);
        assert_eq!(store.get_goal(&goal.id).unwrap().unwrap().title, "修订");
    }

    #[test]
    fn work_item_legal_lifecycle_persists() {
        let (_dir, store) = temp_store();
        let item = WorkItem::new("conv_1", "goal_1", "payment-service", "检查超时日志");
        store.insert_work_item(&item).unwrap();

        for target in [
            WorkItemStatus::Planned,
            WorkItemStatus::Queued,
            WorkItemStatus::Running,
            WorkItemStatus::Completed,
        ] {
            let updated = store
                .transition_work_item(&item.id, target)
                .unwrap_or_else(|error| panic!("转移 {target:?} 失败: {error}"));
            assert_eq!(updated.status, target);
        }
        let final_item = store.get_work_item(&item.id).unwrap().unwrap();
        assert_eq!(final_item.status, WorkItemStatus::Completed);
        assert!(final_item.completed_at.is_some());
    }

    #[test]
    fn illegal_transition_is_rejected() {
        let (_dir, store) = temp_store();
        let item = WorkItem::new("conv_1", "goal_1", "ws", "t");
        store.insert_work_item(&item).unwrap();

        // draft → running 跳过计划与排队：拒绝
        let error = store
            .transition_work_item(&item.id, WorkItemStatus::Running)
            .expect_err("draft → running 应被拒绝");
        assert!(error.to_string().contains("非法状态转移"));

        // 完成后回退：拒绝
        store
            .transition_work_item(&item.id, WorkItemStatus::Planned)
            .unwrap();
        store
            .transition_work_item(&item.id, WorkItemStatus::Queued)
            .unwrap();
        store
            .transition_work_item(&item.id, WorkItemStatus::Running)
            .unwrap();
        store
            .transition_work_item(&item.id, WorkItemStatus::Completed)
            .unwrap();
        assert!(
            store
                .transition_work_item(&item.id, WorkItemStatus::Running)
                .is_err()
        );
        assert_eq!(
            store.get_work_item(&item.id).unwrap().unwrap().status,
            WorkItemStatus::Completed,
            "被拒绝的转移不得改写状态"
        );
    }

    #[test]
    fn failed_item_can_retry_or_cancel() {
        let (_dir, store) = temp_store();
        let item = WorkItem::new("conv_1", "goal_1", "ws", "t");
        store.insert_work_item(&item).unwrap();
        for target in [
            WorkItemStatus::Planned,
            WorkItemStatus::Queued,
            WorkItemStatus::Running,
            WorkItemStatus::Failed,
        ] {
            store.transition_work_item(&item.id, target).unwrap();
        }
        // 重试重新排队
        store
            .transition_work_item(&item.id, WorkItemStatus::Queued)
            .unwrap();
    }

    #[test]
    fn transition_missing_item_fails() {
        let (_dir, store) = temp_store();
        assert!(
            store
                .transition_work_item("not-exist", WorkItemStatus::Planned)
                .is_err()
        );
    }

    #[test]
    fn events_persist_in_timeline_order() {
        let (_dir, store) = temp_store();
        let item = WorkItem::new("conv_1", "goal_1", "ws", "t");
        store.insert_work_item(&item).unwrap();

        store
            .record_event(&item.id, WorkEventType::WorkItemCreated)
            .unwrap();
        store
            .record_event(&item.id, WorkEventType::WorkItemStarted)
            .unwrap();
        store
            .record_event(&item.id, WorkEventType::WorkItemCompleted)
            .unwrap();

        let events = store.load_events(&item.id).unwrap();
        let names: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "work_item.created",
                "work_item.started",
                "work_item.completed"
            ]
        );

        // 其他工作项的时间线互不干扰
        let other = WorkItem::new("conv_1", "goal_1", "ws", "t2");
        store.insert_work_item(&other).unwrap();
        assert!(store.load_events(&other.id).unwrap().is_empty());
    }
}
