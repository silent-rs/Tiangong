//! MCP 能力动态发现 + 后台刷新调度器 + 缓存。
//!
//! 原属 `tiangong-core::mcp::capability`，MCP 管理插件化后整块迁入本 crate。
//!
//! 状态隔离：[`McpCapabilityIndex`] 持有 capability 缓存与调度器配置，作为
//! [`crate::plugin::McpPlugin`] 的实例字段。每个 plugin 实例独立一份 capability
//! 状态，避免全局 static 导致多实例互相污染。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::client::{LocalMcpClient, McpClient, McpToolMeta, get_cached_server_version};
use tiangong_plugin_mcp_protocol::config::{McpConfig, McpServerConfig};

const MIN_REFRESH_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct McpServerCapability {
    #[serde(default, deserialize_with = "deserialize_tools_compat")]
    tools: Vec<McpToolMeta>,
    #[serde(default = "default_healthy")]
    healthy: bool,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    server_version: Option<String>,
}

fn default_healthy() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedCapabilityCache {
    #[serde(default)]
    servers: BTreeMap<String, McpServerCapability>,
}

#[derive(Debug, Clone, Default)]
struct CapabilitySchedulerConfig {
    mcp_config: McpConfig,
    cache_path: Option<PathBuf>,
    refresh_interval_secs: u64,
}

/// MCP 能力索引：缓存各 server 探测到的工具集 + 后台刷新调度器配置。
///
/// 作为 [`crate::plugin::McpPlugin`] 的实例字段，每个 plugin 实例独立一份，
/// 避免全局 static 在多实例（测试隔离 / server API 与 core 共存）场景下互相污染。
/// 后台调度器线程持有 `Arc` 引用 + shutdown flag，plugin drop 时置 flag 让线程退出。
pub struct McpCapabilityIndex {
    index: Arc<RwLock<BTreeMap<String, McpServerCapability>>>,
    scheduler: Arc<RwLock<CapabilitySchedulerConfig>>,
    /// 调度器线程启动守护（Once 保证每个实例只启动一次后台线程）。
    scheduler_started: Arc<Once>,
    /// 后台线程停止标志：plugin drop 时置 true，调度器 loop 检测后退出，避免线程泄漏。
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl McpCapabilityIndex {
    pub fn new() -> Self {
        Self {
            index: Arc::new(RwLock::new(BTreeMap::new())),
            scheduler: Arc::new(RwLock::new(CapabilitySchedulerConfig::default())),
            scheduler_started: Arc::new(Once::new()),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// 从磁盘加载 capability 缓存到当前实例的 index。
    pub fn load_cache(&self, cache_path: &Path) -> Result<()> {
        if !cache_path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(cache_path)
            .with_context(|| format!("读取 MCP tools 缓存失败：{}", cache_path.display()))?;
        let payload: PersistedCapabilityCache =
            serde_json::from_str(&content).context("解析 MCP tools 缓存失败")?;
        if let Ok(mut guard) = self.index.write() {
            *guard = payload.servers;
        }
        Ok(())
    }

    /// 把当前实例的 capability index 持久化到磁盘。
    #[allow(dead_code)]
    pub fn persist_cache(&self, cache_path: &Path) -> Result<()> {
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建 MCP tools 缓存目录失败：{}", parent.display()))?;
        }
        let servers = self
            .index
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let payload = PersistedCapabilityCache { servers };
        let content =
            serde_json::to_string_pretty(&payload).context("序列化 MCP tools 缓存失败")?;
        fs::write(cache_path, content)
            .with_context(|| format!("写入 MCP tools 缓存失败：{}", cache_path.display()))?;
        Ok(())
    }

    /// 配置后台刷新调度器（幂等：仅首次调用启动后台线程）。
    pub fn configure_scheduler(
        &self,
        config: McpConfig,
        cache_path: PathBuf,
        refresh_interval_secs: u64,
    ) {
        if let Ok(mut guard) = self.scheduler.write() {
            guard.mcp_config = config;
            guard.cache_path = Some(cache_path);
            guard.refresh_interval_secs = refresh_interval_secs.max(MIN_REFRESH_INTERVAL_SECS);
        }
        let index = Arc::clone(&self.index);
        let scheduler = Arc::clone(&self.scheduler);
        let shutdown = Arc::clone(&self.shutdown);
        self.scheduler_started.call_once(|| {
            let _ = thread::Builder::new()
                .name("tiangong-mcp-capability-scheduler".to_string())
                .spawn(move || {
                    loop {
                        let interval_secs = scheduler
                            .read()
                            .map(|guard| guard.refresh_interval_secs.max(MIN_REFRESH_INTERVAL_SECS))
                            .unwrap_or(MIN_REFRESH_INTERVAL_SECS);
                        // 分段睡眠以便及时响应 shutdown。
                        let deadline = Instant::now() + Duration::from_secs(interval_secs);
                        while Instant::now() < deadline {
                            if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                                return;
                            }
                            thread::sleep(Duration::from_millis(200));
                        }
                        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                        let (config, cache_path) = scheduler
                            .read()
                            .map(|guard| (guard.mcp_config.clone(), guard.cache_path.clone()))
                            .unwrap_or_else(|_| (McpConfig::default(), None));
                        if !config.enabled || config.servers.is_empty() {
                            continue;
                        }
                        refresh_mcp_capabilities(&index, &config);
                        if let Some(path) = cache_path.as_deref() {
                            let _ = persist_mcp_capabilities_cache(&index, path);
                        }
                    }
                });
        });
    }

    /// 请求后台调度器线程停止（plugin drop 时调用，避免线程泄漏）。
    #[allow(dead_code)]
    pub fn shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 异步刷新所有 server 的 capability（spawn 线程，不阻塞调用方）。
    ///
    /// 用于启动时批量预热。管理操作（register/update）应改用 [`Self::probe_single`]
    /// 同步探测单个 server，确保操作返回时工具即用。
    pub fn refresh_async(&self, config: McpConfig) {
        if let Ok(mut guard) = self.scheduler.write() {
            guard.mcp_config = config.clone();
        }
        let index = Arc::clone(&self.index);
        let scheduler = Arc::clone(&self.scheduler);
        let shutdown = Arc::clone(&self.shutdown);
        let _ = thread::Builder::new()
            .name("tiangong-mcp-capability-prewarm".to_string())
            .spawn(move || {
                if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                refresh_mcp_capabilities(&index, &config);
                if let Some(path) = scheduler
                    .read()
                    .ok()
                    .and_then(|guard| guard.cache_path.clone())
                {
                    let _ = persist_mcp_capabilities_cache(&index, &path);
                }
            });
    }

    /// 同步探测单个 MCP server 并写回 index（供管理操作 register/update/set_enabled 使用）。
    ///
    /// 探测期间不持锁，探测完短暂持写锁写回单个结果。探测完持久化一次避免重启丢失。
    /// 返回探测结果（healthy / tools / last_error）供调用方决策。
    pub fn probe_single(&self, server: &McpServerConfig, timeout_ms: u64) -> ProbeOutcome {
        let capability = probe_server_capability(server, timeout_ms);
        let outcome = ProbeOutcome {
            healthy: capability.healthy,
            tool_count: capability.tools.len(),
            last_error: capability.last_error.clone(),
        };
        if let Ok(mut guard) = self.index.write() {
            if !capability.healthy {
                if let Some(existing) = guard.get_mut(&server.name) {
                    existing.healthy = false;
                    existing.last_error = capability.last_error;
                    tracing::warn!(
                        "MCP 单 server 探测失败，保留工具缓存：server={}（缓存 {} 个工具）",
                        server.name,
                        existing.tools.len()
                    );
                } else {
                    guard.insert(server.name.clone(), capability);
                }
            } else {
                emit_recovery_if_needed(&server.name, guard.get(&server.name), &capability);
                guard.insert(server.name.clone(), capability);
            }
        }
        // 探测完持久化一次，避免重启丢失
        if let Some(path) = self
            .scheduler
            .read()
            .ok()
            .and_then(|guard| guard.cache_path.clone())
        {
            let _ = persist_mcp_capabilities_cache(&self.index, &path);
        }
        outcome
    }

    /// 按 name 探测单个 MCP server（从 scheduler 缓存的 config 取配置）。
    ///
    /// 供前端"重试"按钮使用。若 server 不在当前 config 中则返回错误。
    pub fn probe_single_by_name(&self, name: &str) -> Result<()> {
        let (server, timeout_ms) = {
            let guard = self
                .scheduler
                .read()
                .map_err(|_| anyhow!("MCP 能力调度器锁中毒"))?;
            let server = guard
                .mcp_config
                .servers
                .iter()
                .find(|s| s.name == name && s.enabled)
                .ok_or_else(|| anyhow!("未找到启用中的 MCP server：{name}"))?
                .clone();
            (server, guard.mcp_config.timeout_ms)
        };
        self.probe_single(&server, timeout_ms);
        Ok(())
    }

    /// 读取某 server 缓存的工具列表（仅当 healthy）。
    pub fn cached_server_tools(&self, server_name: &str) -> Option<Vec<McpToolMeta>> {
        let guard = self.index.read().ok()?;
        guard
            .get(server_name)
            .filter(|entry| entry.healthy)
            .map(|entry| entry.tools.clone())
    }

    /// 工具声明沿用已发现缓存，健康状态仅用于执行及界面显示。
    pub fn cached_active_tools(&self) -> Vec<(String, Vec<McpToolMeta>)> {
        self.index
            .read()
            .map(|guard| {
                guard
                    .iter()
                    .map(|(name, capability)| (name.clone(), capability.tools.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    pub fn record_success(&self, server_name: &str) {
        if let Ok(mut index) = self.index.write()
            && let Some(current) = index.get_mut(server_name)
            && !current.healthy
        {
            let previous = current.clone();
            current.healthy = true;
            current.last_error = None;
            emit_recovery_if_needed(server_name, Some(&previous), current);
        }
    }

    /// 所有 server 的健康状态（供前端健康面板）。
    pub fn health_statuses(&self) -> Vec<McpServerHealthStatus> {
        self.index
            .read()
            .map(|guard| {
                guard
                    .iter()
                    .map(|(name, cap)| McpServerHealthStatus {
                        name: name.clone(),
                        healthy: cap.healthy,
                        tool_count: cap.tools.len(),
                        last_error: cap.last_error.clone(),
                        server_version: cap.server_version.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Default for McpCapabilityIndex {
    fn default() -> Self {
        Self::new()
    }
}

fn emit_recovery_if_needed(
    name: &str,
    previous: Option<&McpServerCapability>,
    current: &McpServerCapability,
) {
    let Some(event) = recovery_event(name, previous, current) else {
        return;
    };
    if let Ok(payload) = serde_json::to_string(&event) {
        tiangong_plugin_sidecar::server::emit_notification(
            tiangong_plugin_sidecar::protocol::TOOLS_RECOVERED_CHANNEL,
            payload,
        );
    }
}

fn recovery_event(
    name: &str,
    previous: Option<&McpServerCapability>,
    current: &McpServerCapability,
) -> Option<tiangong_plugin_sidecar::protocol::ToolsRecovered> {
    if !previous.is_some_and(|previous| !previous.healthy) || !current.healthy {
        return None;
    }
    let tools = current
        .tools
        .iter()
        .map(|tool| crate::execution::resolve_mcp_function_name(name, &tool.name))
        .collect::<Vec<_>>();
    if tools.is_empty() {
        return None;
    }
    Some(tiangong_plugin_sidecar::protocol::ToolsRecovered { tools })
}

#[cfg(test)]
mod declaration_tests {
    use super::*;
    #[test]
    fn unhealthy_tools_remain_declared_after_disk_reload() {
        let index = McpCapabilityIndex::new();
        let tools = vec![serde_json::from_value(serde_json::json!({"name":"read","description":"fixed","input_schema":{"type":"object"}})).unwrap()];
        let healthy = McpServerCapability {
            tools,
            healthy: true,
            ..Default::default()
        };
        index
            .index
            .write()
            .unwrap()
            .insert("probe".into(), healthy.clone());
        let before = serde_json::to_value(index.cached_active_tools()).unwrap();
        index
            .index
            .write()
            .unwrap()
            .get_mut("probe")
            .unwrap()
            .healthy = false;
        assert_eq!(
            serde_json::to_value(index.cached_active_tools()).unwrap(),
            before
        );
        assert!(!index.health_statuses()[0].healthy);
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cache.json");
        index.persist_cache(&path).unwrap();
        let restored = McpCapabilityIndex::new();
        restored.load_cache(&path).unwrap();
        assert_eq!(
            serde_json::to_value(restored.cached_active_tools()).unwrap(),
            before
        );
        let offline = restored.index.read().unwrap()["probe"].clone();
        assert!(recovery_event("probe", Some(&healthy), &healthy).is_none());
        assert!(recovery_event("probe", None, &healthy).is_none());
        let event = recovery_event("probe", Some(&offline), &healthy).unwrap();
        assert_eq!(event.tools, vec!["mcp__probe__read"]);
        restored.record_success("probe");
        assert_eq!(
            serde_json::to_value(restored.cached_active_tools()).unwrap(),
            before
        );
        assert!(restored.health_statuses()[0].healthy);
    }

    #[tokio::test]
    async fn explicit_disable_still_blocks_declarations_and_execution() {
        let config: McpConfig = serde_json::from_value(serde_json::json!({"enabled":false,"servers":[{"name":"probe","command":"must-not-run"}]})).unwrap();
        let declared = vec![(
            "probe".into(),
            vec![serde_json::from_value(serde_json::json!({"name":"read"})).unwrap()],
        )];
        assert!(
            crate::execution::list_tools_response(&config, declared)
                .servers
                .is_empty()
        );
        let target = crate::execution::McpFunctionTarget {
            server_name: "probe".into(),
            tool_name: "read".into(),
        };
        let error = crate::execution::execute_tool(&target, serde_json::json!({}), &config, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("已停用"));
    }
}

/// 单 server 同步探测结果摘要。
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub healthy: bool,
    pub tool_count: usize,
    pub last_error: Option<String>,
}

pub use tiangong_plugin_mcp_protocol::query::McpServerHealthStatus;

// ── 内部刷新逻辑（操作实例化的 index，不再用全局 static）──

/// 同步刷新所有启用 server 的 capability 到指定 index。
fn refresh_mcp_capabilities(
    index: &RwLock<BTreeMap<String, McpServerCapability>>,
    config: &McpConfig,
) {
    // 第一段：并发探测所有启用的 server，全程不持锁，避免阻塞健康面板读锁。
    let targets: Vec<&McpServerConfig> = config
        .servers
        .iter()
        .filter(|server| server.enabled)
        .collect();
    let results: Vec<(String, McpServerCapability)> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|server| {
                let server = (**server).clone();
                scope.spawn(move || {
                    (
                        server.name.clone(),
                        probe_server_capability(&server, config.timeout_ms),
                    )
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    // 第二段：短暂持写锁，只做合并 + 清理。
    if let Ok(mut guard) = index.write() {
        for (name, capability) in results {
            if !capability.healthy {
                // 探测失败：更新健康状态和错误信息，但保留已有工具缓存
                if let Some(existing) = guard.get_mut(&name) {
                    existing.healthy = false;
                    existing.last_error = capability.last_error;
                    tracing::warn!(
                        "MCP 探测失败，标记为不健康并保留工具缓存：server={}（缓存 {} 个工具）",
                        name,
                        existing.tools.len()
                    );
                } else {
                    guard.insert(name, capability);
                }
                continue;
            }
            emit_recovery_if_needed(&name, guard.get(&name), &capability);
            guard.insert(name, capability);
        }
        // 清理已移除或禁用的服务器
        let active_names: std::collections::HashSet<_> = config
            .servers
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.name.clone())
            .collect();
        guard.retain(|name, _| active_names.contains(name));
    }
}

/// 持久化指定 index 的内容到磁盘。
fn persist_mcp_capabilities_cache(
    index: &RwLock<BTreeMap<String, McpServerCapability>>,
    cache_path: &Path,
) -> Result<()> {
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 MCP tools 缓存目录失败：{}", parent.display()))?;
    }
    let servers = index.read().map(|guard| guard.clone()).unwrap_or_default();
    let payload = PersistedCapabilityCache { servers };
    let content = serde_json::to_string_pretty(&payload).context("序列化 MCP tools 缓存失败")?;
    fs::write(cache_path, content)
        .with_context(|| format!("写入 MCP tools 缓存失败：{}", cache_path.display()))?;
    Ok(())
}

fn probe_server_capability(server: &McpServerConfig, timeout_ms: u64) -> McpServerCapability {
    // capability 探测无会话上下文，workspace=None：子进程继承宿主 cwd。
    let client = LocalMcpClient::default();
    let server_name = server.name.clone();
    let server_transport = server.resolved_transport();
    let server = server.clone();
    let result = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("初始化 MCP 探测运行时失败")?;
                runtime.block_on(client.list_tools(&server, timeout_ms))
            })
            .join()
            .map_err(|_| anyhow!("MCP 探测线程 panic"))?
    });
    match result {
        Ok(tools) => {
            let tools = dedup_tools(&tools);
            let server_version = get_cached_server_version(&server_name);
            McpServerCapability {
                healthy: true,
                tools,
                last_error: None,
                server_version,
            }
        }
        Err(err) => {
            let err_msg = err.to_string();
            tracing::warn!(
                "MCP 探测失败：server={} transport={:?} error={}",
                server_name,
                server_transport,
                err_msg
            );
            McpServerCapability {
                healthy: false,
                tools: Vec::new(),
                last_error: Some(err_msg),
                server_version: None,
            }
        }
    }
}

fn dedup_tools(items: &[McpToolMeta]) -> Vec<McpToolMeta> {
    let mut output = BTreeMap::new();
    for item in items {
        let name = item.name.trim();
        if !name.is_empty() {
            output
                .entry(name.to_string())
                .or_insert_with(|| item.clone());
        }
    }
    output.into_values().collect()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum LegacyToolCompat {
    Name(String),
    Tool(McpToolMeta),
}

fn deserialize_tools_compat<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<McpToolMeta>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<LegacyToolCompat>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|item| match item {
            LegacyToolCompat::Name(name) => {
                let name = name.trim();
                if name.is_empty() {
                    None
                } else {
                    Some(McpToolMeta {
                        name: name.to_string(),
                        description: String::new(),
                        input_schema: serde_json::Value::Null,
                        argument_summaries: Vec::new(),
                    })
                }
            }
            LegacyToolCompat::Tool(tool) => {
                if tool.name.trim().is_empty() {
                    None
                } else {
                    Some(tool)
                }
            }
        })
        .collect())
}
