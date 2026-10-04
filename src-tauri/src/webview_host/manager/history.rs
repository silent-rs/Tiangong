//! 标签浏览历史、全局历史与缩放的持久化。

use super::*;

pub(super) fn is_recordable_history_url(url: &str) -> bool {
    !url.is_empty() && !url.starts_with("about:") && !url.starts_with("data:")
}

pub(super) fn history_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) fn append_tab_navigation(
    state: &mut BrowserState,
    tab_id: &str,
    url: &str,
    title: Option<&str>,
) -> Option<usize> {
    if !is_recordable_history_url(url) {
        return None;
    }
    let title = title.filter(|title| !title.is_empty()).unwrap_or(url);
    let current_index = state.tab_history_indices.get(tab_id).copied();
    let entries = state.tab_histories.entry(tab_id.to_string()).or_default();
    if let Some(index) = current_index.filter(|index| *index < entries.len()) {
        entries.truncate(index.saturating_add(1));
    }
    entries.push(HistoryEntry {
        url: url.to_string(),
        title: title.to_string(),
        timestamp: history_timestamp(),
    });
    if entries.len() > 200 {
        let remove_count = entries.len().saturating_sub(160);
        entries.drain(0..remove_count);
    }
    let index = entries.len().saturating_sub(1);
    state.tab_history_indices.insert(tab_id.to_string(), index);
    Some(index)
}

pub(super) fn apply_tab_navigation_intent(
    state: &mut BrowserState,
    tab_id: &str,
    url: &str,
    intent: NavigationIntent,
) -> Result<Option<usize>, String> {
    match intent {
        NavigationIntent::Normal => Ok(append_tab_navigation(state, tab_id, url, None)),
        NavigationIntent::History { target_index } => {
            let entries = state
                .tab_histories
                .get(tab_id)
                .ok_or_else(|| "当前标签没有导航记录".to_string())?;
            if entries.get(target_index).is_none() {
                return Err("目标导航记录不存在".to_string());
            }
            state
                .tab_history_indices
                .insert(tab_id.to_string(), target_index);
            Ok(Some(target_index))
        }
        NavigationIntent::Reload | NavigationIntent::Retry | NavigationIntent::Restore => {
            let current_index = state
                .tab_history_indices
                .get(tab_id)
                .copied()
                .filter(|index| {
                    state
                        .tab_histories
                        .get(tab_id)
                        .is_some_and(|entries| *index < entries.len())
                });
            Ok(current_index.or_else(|| append_tab_navigation(state, tab_id, url, None)))
        }
    }
}

pub(super) fn update_tab_navigation_entry(
    state: &mut BrowserState,
    tab_id: &str,
    history_index: Option<usize>,
    url: &str,
    title: Option<&str>,
) {
    if !is_recordable_history_url(url) {
        return;
    }
    let index = history_index.or_else(|| append_tab_navigation(state, tab_id, url, title));
    let Some(index) = index else {
        return;
    };
    let Some(entry) = state
        .tab_histories
        .get_mut(tab_id)
        .and_then(|entries| entries.get_mut(index))
    else {
        return;
    };
    entry.url = url.to_string();
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        entry.title = title.to_string();
    }
    entry.timestamp = history_timestamp();
    state.tab_history_indices.insert(tab_id.to_string(), index);
}

pub(super) fn global_history_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".tiangong")
        .join("browser-history.json")
}

pub(super) fn upsert_global_history(history: &mut Vec<HistoryEntry>, url: &str, title: &str) {
    let entry = HistoryEntry {
        url: url.to_string(),
        title: if title.is_empty() {
            url.to_string()
        } else {
            title.to_string()
        },
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };
    if let Some(index) = history.iter().position(|item| item.url == url) {
        history.remove(index);
    }
    history.push(entry);
    if history.len() > 1000 {
        let keep_from = history.len() - 800;
        history.drain(0..keep_from);
    }
}

pub(crate) fn load_global_history() -> Vec<HistoryEntry> {
    let path = global_history_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 持久化全局浏览历史（后台线程写盘）。
///
/// 调用方可能在主线程（on_page_load → complete_navigation_for_tab），同步写
/// 文件会占用事件循环。写盘挪到后台线程；写入时在串行锁内重新读取最新
/// 历史，多次并发触发也能保证最终落盘的是最新内容。
pub(super) fn persist_global_history(shared: &Arc<BrowserSharedState>) {
    static WRITE_LOCK: Mutex<()> = Mutex::new(());
    let shared = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("browser-history-persist".into())
        .spawn(move || {
            let _serial = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            write_global_history(&shared);
        });
    if let Err(error) = spawned {
        warn!(%error, "浏览历史后台写盘线程启动失败");
    }
}

pub(super) fn write_global_history(shared: &BrowserSharedState) {
    let content = {
        let entries = shared
            .global_history
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        serde_json::to_string(&*entries)
    };
    if let Ok(content) = content {
        let _ = std::fs::write(global_history_path(), content);
    }
}

pub(super) fn zoom_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".tiangong")
        .join("browser-zoom.json")
}

pub(crate) fn load_zoom() -> f64 {
    let path = zoom_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str::<f64>(&content).unwrap_or(1.0),
        Err(_) => 1.0,
    }
}

pub(super) fn persist_zoom(shared: &Arc<BrowserSharedState>) {
    let zoom = shared.zoom_factor.lock().unwrap_or_else(|e| e.into_inner());
    let path = zoom_path();
    if let Ok(content) = serde_json::to_string(&*zoom) {
        let _ = std::fs::write(path, content);
    }
}

impl BrowserManager {
    /// 获取标签页浏览历史
    pub fn get_tab_history(&self, tab_id: Option<&str>) -> Option<TabHistoryResult> {
        let state = self.state.lock().ok()?;
        let target_id = tab_id
            .map(|s| s.to_string())
            .or_else(|| state.active_tab_id.clone())?;
        let entries = state.tab_histories.get(&target_id)?.clone();
        let current_index = state
            .tab_history_indices
            .get(&target_id)
            .copied()
            .unwrap_or(0) as i32;
        Some(TabHistoryResult {
            tab_id: target_id,
            entries,
            current_index,
        })
    }

    /// 获取全局浏览历史（分页，最新在前）
    pub fn get_global_history(&self, offset: usize, limit: usize) -> Vec<HistoryEntry> {
        let shared = self.shared_state();
        let history = shared
            .global_history
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let total = history.len();
        if offset >= total {
            return Vec::new();
        }
        // 倒序切片：offset=0 取最后 limit 条
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit);
        history[start..end].iter().rev().cloned().collect()
    }

    /// 清空全局浏览历史
    pub fn clear_global_history(&self) {
        let shared = self.shared_state();
        {
            let mut history = shared
                .global_history
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            history.clear();
        }
        persist_global_history(&shared);
    }

    /// 删除全局历史中指定 URL 的条目
    pub fn delete_global_history_entry(&self, url: &str) {
        let shared = self.shared_state();
        let should_persist = {
            let mut history = shared
                .global_history
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let before = history.len();
            history.retain(|entry| entry.url != url);
            before != history.len()
        };
        if should_persist {
            persist_global_history(&shared);
        }
    }

    /// 清除指定标签页的历史
    pub fn clear_tab_history(&self, tab_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.tab_histories.remove(tab_id);
            state.tab_history_indices.remove(tab_id);
        }
    }

    /// 初始化标签页历史
    pub fn init_tab_history(&self, tab_id: &str, url: &str, title: &str) {
        if let Ok(mut state) = self.state.lock() {
            let entry = HistoryEntry {
                url: url.to_string(),
                title: if title.is_empty() {
                    url.to_string()
                } else {
                    title.to_string()
                },
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            };
            state.tab_histories.insert(tab_id.to_string(), vec![entry]);
            state.tab_history_indices.insert(tab_id.to_string(), 0);
        }
    }
}
