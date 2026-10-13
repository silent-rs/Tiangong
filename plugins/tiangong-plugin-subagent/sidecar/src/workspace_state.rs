//! 成员工作区状态：agents/<id>/workspaces/<workspace-id>/ 下的成员自维护
//! 工作状态（plan/context/tasks）。sidecar 只提供安全存取与稳定身份
//! 解析（路径→workspace-id），不理解业务内容——工作含义由成员维护，
//! 执行事实由运行记录维护，两类状态分离。

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::agent_store::AgentStore;
use crate::paths;

/// 工作区状态注入摘要上限（成员规划过长时截断，避免挤占任务正文）。
const STATE_INJECTION_MAX_CHARS: usize = 1500;

/// 解析（或首次登记）workspace 稳定身份：agents/<id>/workspaces/index.json
/// 维护「本机路径 → workspace-id」映射；路径只是位置，id 才是身份。
///
/// 登记全程（查询、创建、写回）持文件锁：宿主 sidecar 与外部 MCP 进程
/// 可能同时访问同一 agents 目录，单次原子写不能保护读-改-写整体。
pub(crate) fn workspace_id(
    agents: &AgentStore,
    agent_id: &str,
    workspace_path: &str,
) -> Result<String> {
    let dir = workspaces_dir(agents, agent_id)?;
    let _guard = IndexLock::acquire(&dir)?;
    workspace_id_locked(&dir, workspace_path)
}

/// 已持 [`IndexLock`] 的工作区身份解析（勿在锁外调用）。
fn workspace_id_locked(dir: &std::path::Path, workspace_path: &str) -> Result<String> {
    // 路径归一：符号链接与挂载别名统一到 canonical 形态再登记比较，
    // 避免同一工作区因 /tmp 与 /private/tmp 等差异分裂出多个身份。
    let workspace_path = std::fs::canonicalize(workspace_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| workspace_path.to_string());
    let index_path = dir.join("index.json");
    let mut index: serde_json::Value = std::fs::read_to_string(&index_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({ "workspaces": [] }));
    let entries = index
        .get_mut("workspaces")
        .and_then(|value| value.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("workspace 索引损坏"))?;
    for entry in entries.iter() {
        if entry.get("path").and_then(|p| p.as_str()) == Some(workspace_path.as_str())
            && let Some(id) = entry.get("id").and_then(|id| id.as_str())
        {
            return Ok(id.to_string());
        }
    }
    // 同目录历史登记（canonicalize 差异）：按已登记 id 的 workspace.json
    // 路径字段反查，命中则补齐新路径别名。
    for entry in entries.iter() {
        let Some(id) = entry.get("id").and_then(|id| id.as_str()) else {
            continue;
        };
        let known = dir.join(id).join("workspace.json");
        if let Ok(meta) = std::fs::read_to_string(&known)
            && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta)
            && meta
                .get("paths")
                .and_then(|p| p.as_array())
                .is_some_and(|paths| {
                    paths
                        .iter()
                        .any(|p| p.as_str() == Some(workspace_path.as_str()))
                })
        {
            return Ok(id.to_string());
        }
    }
    let id = format!("ws-{}", paths::new_id());
    entries.push(serde_json::json!({ "id": id, "path": workspace_path.clone() }));
    paths::atomic_write(&index_path, serde_json::to_string(&index)?.as_bytes())?;
    let ws_dir = dir.join(&id);
    std::fs::create_dir_all(&ws_dir)?;
    let meta = serde_json::json!({
        "workspace_id": id,
        "paths": [workspace_path.clone()],
    });
    paths::atomic_write(
        &ws_dir.join("workspace.json"),
        serde_json::to_string(&meta)?.as_bytes(),
    )?;
    Ok(id)
}

fn workspaces_dir(agents: &AgentStore, agent_id: &str) -> Result<PathBuf> {
    ensure_agent_exists(agents, agent_id)?;
    let dir = agents.root().join(agent_id).join("workspaces");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 工作区索引的跨进程排他锁（目录下 `.index.lock` 文件，flock 独占）。
///
/// 保护 workspace 身份登记的读-改-写整体：宿主 sidecar 与外部 MCP 进程
/// 可能同时登记同一工作区，无锁时并发读到旧索引会各自分配新身份并
/// 相互覆盖。RAII：Drop 解锁，锁文件本身不删除。
struct IndexLock {
    _file: std::fs::File,
}

impl IndexLock {
    fn acquire(dir: &std::path::Path) -> Result<Self> {
        use fs2::FileExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(".index.lock"))?;
        file.lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._file);
    }
}

/// 公共入口统一校验：成员不存在时不创建任何目录，直接报错。
fn ensure_agent_exists(agents: &AgentStore, agent_id: &str) -> Result<()> {
    if agents.load(agent_id).is_err() {
        bail!("Agent 不存在: {agent_id}");
    }
    Ok(())
}

fn state_dir(agents: &AgentStore, agent_id: &str, workspace_id: &str) -> Result<PathBuf> {
    if !workspace_id.starts_with("ws-") || workspace_id.contains('/') {
        bail!("非法 workspace 身份: {workspace_id}");
    }
    Ok(workspaces_dir(agents, agent_id)?.join(workspace_id))
}

/// 读取成员在某工作区的自维护状态（plan + context + task）。
pub fn load(
    agents: &AgentStore,
    agent_id: &str,
    workspace_path: &str,
) -> Result<serde_json::Value> {
    let id = workspace_id(agents, agent_id, workspace_path)?;
    let dir = state_dir(agents, agent_id, &id)?;
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .map(|body| body.trim().to_string())
            .unwrap_or_default()
    };
    Ok(serde_json::json!({
        "workspace_id": id,
        "workspace_path": workspace_path,
        "plan": read("plan.md"),
        "context": read("context.md"),
        "task": read("task.md"),
    }))
}

/// 写入成员工作状态文件（plan/context——整文件覆盖，调用方为持有
/// 有序锁的成员侧写入；task 为当前工作与待办，三文件均整文件覆盖）。
pub fn write(
    agents: &AgentStore,
    agent_id: &str,
    workspace_path: &str,
    file: &str,
    content: &str,
) -> Result<String> {
    let id = workspace_id(agents, agent_id, workspace_path)?;
    let dir = state_dir(agents, agent_id, &id)?;
    let name = match file {
        "plan" => "plan.md",
        "context" => "context.md",
        "task" => "task.md",
        other => bail!("工作状态只支持 plan / context / task 文件: {other}"),
    };
    paths::atomic_write(&dir.join(name), content.as_bytes())?;
    Ok(id)
}

/// 列举成员全部工作区的自维护状态（管理页/外部查询视图用）。
pub fn list_all(agents: &AgentStore, agent_id: &str) -> Result<serde_json::Value> {
    let dir = workspaces_dir(agents, agent_id)?;
    let mut items = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let id = entry.file_name().into_string().unwrap_or_default();
        if !id.starts_with("ws-") {
            continue;
        }
        let ws_dir = entry.path();
        let meta = std::fs::read_to_string(ws_dir.join("workspace.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let read = |name: &str| {
            std::fs::read_to_string(ws_dir.join(name))
                .map(|body| body.trim().to_string())
                .unwrap_or_default()
        };
        items.push(serde_json::json!({
            "workspace_id": id,
            "paths": meta.get("paths").cloned().unwrap_or(serde_json::json!([])),
            "plan": read("plan.md"),
            "context": read("context.md"),
            "task": read("task.md"),
        }));
    }
    Ok(serde_json::json!({ "workspaces": items }))
}

/// 投递正文注入的工作状态摘要（plan/context 头部，截断保上限）。
pub fn injection_summary(agents: &AgentStore, agent_id: &str, workspace_path: &str) -> String {
    let Ok(state) = load(agents, agent_id, workspace_path) else {
        return String::new();
    };
    let task = state
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let plan = state
        .get("plan")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let context = state
        .get("context")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if task.trim().is_empty() && plan.trim().is_empty() && context.trim().is_empty() {
        return String::new();
    }
    let per_part = STATE_INJECTION_MAX_CHARS / 3;
    let truncate = |text: &str| {
        if text.chars().count() > per_part {
            text.chars().take(per_part).collect::<String>() + "…"
        } else {
            text.to_string()
        }
    };
    let mut body =
        String::from("\n\n【工作区状态】（你在本工作区的自维护状态，收尾时用工作区状态工具更新）");
    if !task.trim().is_empty() {
        body.push_str(&format!("\n当前工作：\n{}", truncate(task)));
    }
    if !plan.trim().is_empty() {
        body.push_str(&format!("\n规划：\n{}", truncate(plan)));
    }
    if !context.trim().is_empty() {
        body.push_str(&format!("\n背景：\n{}", truncate(context)));
    }
    body
}

// ── 成员 × 工作区 的专属会话映射 ──
//
// 最终架构约定：每个成员在每个 workspace 使用一个固定工作区的长期会话
// ((agent_id, workspace_id) → session_id)。会话创建后 workspace 不再修改：
// 首次投递随消息携带 workspace 由宿主创建（cwd 即工作区），后续复用会话
// 不携带；上下文过长走 Core 压缩，不因此换会话。

/// 解析（或首次登记）成员在该工作区的专属会话。返回 `(session_id, is_new)`。
///
/// 全程持文件锁（查询、登记、写回）：宿主 sidecar 与外部 MCP 进程并发
/// 首次访问同一成员和工作区时只产生一个有效映射。老成员配置中的全局
/// `session_id` 首次迁移为当前工作区的映射（保持连续性，此后不再使用）；
/// 映射会话已不存在（被删/损坏）时生成新号重建。
pub(crate) fn session_for_workspace(
    agents: &AgentStore,
    agent_id: &str,
    workspace_path: &str,
    legacy_session: Option<&str>,
) -> Result<(String, bool)> {
    let dir = workspaces_dir(agents, agent_id)?;
    let _guard = IndexLock::acquire(&dir)?;
    let ws_id = workspace_id_locked(&dir, workspace_path)?;
    let meta_path = dir.join(&ws_id).join("workspace.json");
    let mut meta: serde_json::Value = std::fs::read_to_string(&meta_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({ "workspace_id": ws_id, "paths": [workspace_path] }));

    let mapped = meta
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    // 映射即真相：登记即复用——会话由宿主在首条消息到达时创建（异步窗口），
    // 文件缺失是预期状态而非映射失效；被删会话的重置走显式入口，不靠这里
    // 猜测。唯一例外：会话已落盘但 cwd 不是本工作区（旧宿主丢弃了投递携带
    // 的 workspace，会话落在全局工作目录），有对话后宿主不允许再改 cwd，
    // 只能换新会话号重建；每个映射最多重建一次（`cwd_rebuilt`），避免在
    // 仍未修复的宿主上每次投递都换会话。
    if let Some(stale) = mapped {
        let rebuilt = meta
            .get(CWD_REBUILT_KEY)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if rebuilt || !session_cwd_mismatched(stale, workspace_path) {
            return Ok((stale.to_string(), false));
        }
        let session_id = paths::new_id();
        tracing::warn!(
            agent_id,
            workspace = workspace_path,
            stale_session = stale,
            new_session = %session_id,
            "成员工作区会话的 cwd 与工作区不一致，重建会话"
        );
        meta["session_id"] = serde_json::Value::String(session_id.clone());
        meta[CWD_REBUILT_KEY] = serde_json::Value::Bool(true);
        paths::atomic_write(&meta_path, serde_json::to_string(&meta)?.as_bytes())?;
        return Ok((session_id, true));
    }

    // 本工作区从未登记映射：老全局会话一次性迁移（会话须实际存在），
    // 否则生成新会话号。
    let never_mapped = mapped.is_none();
    let (session_id, is_new) = match legacy_session
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(legacy)
            if never_mapped
                && crate::sessions::session_exists(legacy)
                && !legacy_session_mapped(&dir, legacy)
                && legacy_session_cwd_matches(legacy, workspace_path) =>
        {
            (legacy.to_string(), false)
        }
        _ => (paths::new_id(), true),
    };
    meta["session_id"] = serde_json::Value::String(session_id.clone());
    paths::atomic_write(&meta_path, serde_json::to_string(&meta)?.as_bytes())?;
    Ok((session_id, is_new))
}

/// 只读查询成员在该工作区的专属会话（列表/管理页展示用）：不登记工作区、
/// 不创建目录、不写映射。
///
/// 依据与 [`session_for_workspace`] 一致：工作区映射中登记的会话；未登记时，
/// 老成员的全局会话若工作在该工作区（且未被其他工作区占用）也算——首次
/// 派活即沿用它。都没有返回 None。
pub(crate) fn existing_workspace_session(
    agents: &AgentStore,
    agent_id: &str,
    workspace_path: &str,
    legacy_session: Option<&str>,
) -> Option<String> {
    let dir = agents.root().join(agent_id).join("workspaces");
    let mapped = lookup_workspace_id(&dir, workspace_path).and_then(|ws_id| {
        std::fs::read_to_string(dir.join(ws_id).join("workspace.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|meta| {
                meta.get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
    });
    if mapped.is_some() {
        return mapped;
    }
    let legacy = legacy_session
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    (crate::sessions::session_exists(legacy)
        && !legacy_session_mapped(&dir, legacy)
        && legacy_session_cwd_matches(legacy, workspace_path))
    .then(|| legacy.to_string())
}

/// 只读解析工作区身份（与 [`workspace_id_locked`] 同口径：canonical 路径
/// 先查索引，再查各工作区登记的路径别名），未登记返回 None。
fn lookup_workspace_id(dir: &std::path::Path, workspace_path: &str) -> Option<String> {
    let workspace_path = std::fs::canonicalize(workspace_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| workspace_path.to_string());
    let index: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("index.json")).ok()?).ok()?;
    let entries = index.get("workspaces")?.as_array()?;
    let ids = entries
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry.get("path")?.as_str())));
    let mut alias_candidates = Vec::new();
    for (id, path) in ids {
        if path == Some(workspace_path.as_str()) {
            return Some(id.to_string());
        }
        alias_candidates.push(id);
    }
    alias_candidates.into_iter().find_map(|id| {
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(id).join("workspace.json")).ok()?,
        )
        .ok()?;
        meta.get("paths")?
            .as_array()?
            .iter()
            .any(|p| p.as_str() == Some(workspace_path.as_str()))
            .then(|| id.to_string())
    })
}

/// 旧会话是否已被本成员任一工作区映射占用（防同一旧会话迁移到多个
/// workspace——不同工作区必须使用不同会话）。
fn legacy_session_mapped(dir: &std::path::Path, legacy: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let meta = entry.path().join("workspace.json");
        std::fs::read_to_string(&meta)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|value| {
                value
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|session| session == legacy)
    })
}

/// 旧会话的工作目录是否与给定工作区一致（canonicalize 后比较）——
/// 只有会话本就工作在该项目下才沿用，避免把别的项目的会话错配过来。
fn legacy_session_cwd_matches(session_id: &str, workspace_path: &str) -> bool {
    session_cwd(session_id).is_some_and(|cwd| same_path(&cwd, workspace_path))
}

/// 映射元数据中「已因 cwd 不一致重建过」的标记键。
const CWD_REBUILT_KEY: &str = "cwd_rebuilt";

/// 会话已落盘且 cwd 明确不是该工作区。会话文件缺失/不可读/cwd 为空均视为
/// 「未知」而非不一致（首条消息前的异步窗口）。
fn session_cwd_mismatched(session_id: &str, workspace_path: &str) -> bool {
    session_cwd(session_id).is_some_and(|cwd| !same_path(&cwd, workspace_path))
}

fn session_cwd(session_id: &str) -> Option<String> {
    crate::sessions::load_session_json(session_id)
        .ok()?
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
}

fn same_path(left: &str, right: &str) -> bool {
    let canonical = |path: &str| {
        std::fs::canonicalize(path)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string())
    };
    canonical(left) == canonical(right)
}

/// 反查会话归属的成员 ID：扫描全部成员的全部工作区映射，老全局绑定
/// （未迁移数据）兜底。低频路径（Hook 回报归因等），线性扫描可接受。
pub(crate) fn agent_for_session(agents: &AgentStore, session_id: &str) -> Option<String> {
    for config in agents.list() {
        if config.session_id.as_deref() == Some(session_id) {
            return Some(config.id);
        }
        let ws_dir = agents.root().join(&config.id).join("workspaces");
        let Ok(entries) = std::fs::read_dir(&ws_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let meta = entry.path().join("workspace.json");
            if let Ok(raw) = std::fs::read_to_string(&meta)
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw)
                && value.get("session_id").and_then(serde_json::Value::as_str) == Some(session_id)
            {
                return Some(config.id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立存储根下构造带一个成员的 AgentStore（serial：TIANGONG_STORAGE_ROOT
    /// 是进程级环境变量，测试间须串行防互染）。
    fn store_with_agent(tag: &str) -> (AgentStore, tempfile::TempDir, String) {
        let root = tempfile::tempdir().unwrap();
        // serial 测试内单线程设置（edition 2024 set_var 为 unsafe）。
        unsafe { std::env::set_var("TIANGONG_STORAGE_ROOT", root.path()) };
        let store = AgentStore::open().unwrap();
        let config = store
            .create(
                &format!("测试成员-{tag}"),
                "并发登记测试",
                tiangong_plugin_subagent_protocol::config::BackendKind::TiangongSession,
                None,
                Some("sess-test"),
                tiangong_plugin_subagent_protocol::config::WorkspacePolicy::ReadOnly,
                None,
            )
            .unwrap();
        (store, root, config.id)
    }

    #[serial_test::serial]
    #[test]
    fn 并发登记同一工作区只有一个稳定身份() {
        let (store, _root, agent_id) = store_with_agent("concurrent");
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().to_string_lossy().into_owned();

        // 8 线程同时首次登记同一工作区：文件锁保证读-改-写整体串行，
        // 全部拿到同一 id（此前无锁时会分裂出多个身份并相互覆盖索引）。
        let mut handles = Vec::new();
        for _ in 0..8 {
            let agent = agent_id.clone();
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                // 每线程独立 open（同一存储根），模拟宿主与外部 MCP 双进程并发登记。
                let store = AgentStore::open().unwrap();
                workspace_id(&store, &agent, &path).unwrap()
            }));
        }
        let ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = &ids[0];
        assert!(ids.iter().all(|id| id == first), "身份分裂: {ids:?}");

        // 索引只登记一条。
        let index =
            std::fs::read_to_string(store.root().join(&agent_id).join("workspaces/index.json"))
                .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&index).unwrap();
        assert_eq!(parsed["workspaces"].as_array().unwrap().len(), 1);
    }

    /// 在测试存储根下落一个会话文件（模拟宿主已创建该会话）。
    fn create_session_file_with_cwd(session_id: &str, cwd: Option<&std::path::Path>) {
        let dir = crate::paths::storage_root().unwrap().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let body = match cwd {
            Some(cwd) => format!(
                r#"{{"id":"x","cwd":{},"messages":[]}}"#,
                serde_json::to_string(&cwd.to_string_lossy().into_owned()).unwrap()
            ),
            None => r#"{"id":"x","messages":[]}"#.to_string(),
        };
        std::fs::write(dir.join(format!("{session_id}.json")), body).unwrap();
    }

    fn create_session_file(session_id: &str) {
        create_session_file_with_cwd(session_id, None);
    }

    #[serial_test::serial]
    #[test]
    fn 会话映射_首次生成_复用_跨工作区独立() {
        let (store, _root, agent_id) = store_with_agent("mapping");
        let w1 = tempfile::tempdir().unwrap();
        let w2 = tempfile::tempdir().unwrap();
        let p1 = w1.path().to_string_lossy().into_owned();
        let p2 = w2.path().to_string_lossy().into_owned();

        // 首次：生成新会话号。
        let (s1, is_new) = session_for_workspace(&store, &agent_id, &p1, None).unwrap();
        assert!(is_new, "首次应生成新会话");
        create_session_file(&s1);

        // 复用：同工作区返回同会话。
        let (s1b, is_new) = session_for_workspace(&store, &agent_id, &p1, None).unwrap();
        assert_eq!(s1, s1b);
        assert!(!is_new);

        // 跨工作区：独立会话。
        let (s2, is_new) = session_for_workspace(&store, &agent_id, &p2, None).unwrap();
        assert!(is_new);
        assert_ne!(s1, s2);

        // 反查归属。
        assert_eq!(agent_for_session(&store, &s1), Some(agent_id.clone()));
        assert_eq!(agent_for_session(&store, &s2), Some(agent_id.clone()));
        assert_eq!(agent_for_session(&store, "no-such"), None);
    }

    #[serial_test::serial]
    #[test]
    fn 会话映射_老绑定仅迁入工作目录匹配的工作区() {
        let (store, _root, agent_id) = store_with_agent("legacy");
        let w1 = tempfile::tempdir().unwrap();
        let w2 = tempfile::tempdir().unwrap();
        let p1 = w1.path().to_string_lossy().into_owned();
        let p2 = w2.path().to_string_lossy().into_owned();
        // 老会话实际工作在 W1。
        create_session_file_with_cwd("legacy-sess-1", Some(w1.path()));

        // W1：目录匹配且未被占用 → 迁移沿用（保持连续性）。
        let (s1, is_new) =
            session_for_workspace(&store, &agent_id, &p1, Some("legacy-sess-1")).unwrap();
        assert_eq!(s1, "legacy-sess-1");
        assert!(!is_new);

        // W2：老会话已被 W1 映射占用且其工作目录也不是 W2 →
        // 不迁移，创建独立新会话（不同工作区不同会话）。
        let (s2, is_new) =
            session_for_workspace(&store, &agent_id, &p2, Some("legacy-sess-1")).unwrap();
        assert!(is_new);
        assert_ne!(s1, s2);

        // 已映射后不再迁移：即使再传 legacy 也按映射走。
        let (s1b, _) = session_for_workspace(&store, &agent_id, &p1, None).unwrap();
        assert_eq!(s1b, "legacy-sess-1");
    }

    #[serial_test::serial]
    #[test]
    fn 会话映射_老绑定工作目录不匹配时不迁移() {
        let (store, _root, agent_id) = store_with_agent("legacy-cwd");
        let w = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let p = w.path().to_string_lossy().into_owned();
        // 老会话工作在其他目录：不得错配到本工作区。
        create_session_file_with_cwd("legacy-sess-2", Some(other.path()));

        let (s, is_new) =
            session_for_workspace(&store, &agent_id, &p, Some("legacy-sess-2")).unwrap();
        assert!(is_new, "目录不匹配的老会话不应被迁移");
        assert_ne!(s, "legacy-sess-2");
    }

    #[serial_test::serial]
    #[test]
    fn 会话映射_登记即固定不随会话文件漂移() {
        let (store, _root, agent_id) = store_with_agent("rebuild");
        let w = tempfile::tempdir().unwrap();
        let p = w.path().to_string_lossy().into_owned();
        let (s1, is_new) = session_for_workspace(&store, &agent_id, &p, None).unwrap();
        assert!(is_new);
        // 会话文件尚未由宿主创建（首条消息未达）：映射保持稳定——
        // 否则异步创建窗口内的连续投递会不断重建、分裂会话。
        let (s2, is_new) = session_for_workspace(&store, &agent_id, &p, None).unwrap();
        assert_eq!(s1, s2);
        assert!(!is_new);
    }

    #[serial_test::serial]
    #[test]
    fn 会话映射_会话cwd与工作区不一致时只重建一次() {
        let (store, _root, agent_id) = store_with_agent("cwd-rebuild");
        let w = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let p = w.path().to_string_lossy().into_owned();
        let (s1, _) = session_for_workspace(&store, &agent_id, &p, None).unwrap();
        // 旧宿主丢弃 workspace：会话落在其他目录。
        create_session_file_with_cwd(&s1, Some(other.path()));
        let (s2, is_new) = session_for_workspace(&store, &agent_id, &p, None).unwrap();
        assert!(is_new, "cwd 不一致应换新会话并携带 workspace 创建");
        assert_ne!(s1, s2);
        // 宿主仍未修复、新会话再次落错目录：不再反复重建。
        create_session_file_with_cwd(&s2, Some(other.path()));
        let (s3, is_new) = session_for_workspace(&store, &agent_id, &p, None).unwrap();
        assert_eq!(s2, s3);
        assert!(!is_new);
    }
    #[serial_test::serial]
    #[test]
    fn 工作区会话只读查询_有记录才算且不产生登记() {
        let (store, _root, agent_id) = store_with_agent("readonly-lookup");
        let w1 = tempfile::tempdir().unwrap();
        let w2 = tempfile::tempdir().unwrap();
        let p1 = w1.path().to_string_lossy().into_owned();
        let p2 = w2.path().to_string_lossy().into_owned();
        let ws_dir = store.root().join(&agent_id).join("workspaces");
        // 从未在任何工作区发起：无会话，且查询不创建任何登记。
        assert_eq!(
            existing_workspace_session(&store, &agent_id, &p1, None),
            None
        );
        assert!(
            !ws_dir.join("index.json").exists(),
            "只读查询不得登记工作区"
        );
        // 在 W1 发起过：W1 有会话（与派活使用的会话一致），W2 仍无。
        let (s1, _) = session_for_workspace(&store, &agent_id, &p1, None).unwrap();
        assert_eq!(
            existing_workspace_session(&store, &agent_id, &p1, None),
            Some(s1.clone())
        );
        assert_eq!(
            existing_workspace_session(&store, &agent_id, &p2, None),
            None
        );
        let index = std::fs::read_to_string(ws_dir.join("index.json")).unwrap();
        assert!(!index.contains(&p2), "查询 W2 不得登记 W2");
    }
    #[serial_test::serial]
    #[test]
    fn 工作区会话只读查询_老全局会话按工作目录计入() {
        let (store, _root, agent_id) = store_with_agent("readonly-legacy");
        let w = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let p = w.path().to_string_lossy().into_owned();
        let po = other.path().to_string_lossy().into_owned();
        create_session_file_with_cwd("legacy-ro", Some(w.path()));
        assert_eq!(
            existing_workspace_session(&store, &agent_id, &p, Some("legacy-ro")),
            Some("legacy-ro".to_string())
        );
        assert_eq!(
            existing_workspace_session(&store, &agent_id, &po, Some("legacy-ro")),
            None
        );
    }
    #[serial_test::serial]
    #[test]
    fn 会话映射_会话cwd一致时保持复用() {
        let (store, _root, agent_id) = store_with_agent("cwd-match");
        let w = tempfile::tempdir().unwrap();
        let p = w.path().to_string_lossy().into_owned();
        let (s1, _) = session_for_workspace(&store, &agent_id, &p, None).unwrap();
        create_session_file_with_cwd(&s1, Some(w.path()));
        let (s2, is_new) = session_for_workspace(&store, &agent_id, &p, None).unwrap();
        assert_eq!(s1, s2);
        assert!(!is_new);
    }
    #[serial_test::serial]
    #[test]
    fn 会话映射_并发首次访问只产生一个会话() {
        // store 仅用于建出成员身份；并发由各线程自建 AgentStore 验证。
        let (_store, _root, agent_id) = store_with_agent("mapping-concurrent");
        let w = tempfile::tempdir().unwrap();
        let p = w.path().to_string_lossy().into_owned();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let agent = agent_id.clone();
            let path = p.clone();
            handles.push(std::thread::spawn(move || {
                let store = AgentStore::open().unwrap();
                session_for_workspace(&store, &agent, &path, None).unwrap()
            }));
        }
        let results: Vec<(String, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let sessions: std::collections::HashSet<&String> = results.iter().map(|(s, _)| s).collect();
        assert_eq!(
            sessions.len(),
            1,
            "并发首次只应有一个映射会话: {sessions:?}"
        );
    }

    #[serial_test::serial]
    #[test]
    fn 同一工作区重复登记返回同一身份() {
        let (store, _root, agent_id) = store_with_agent("stable");
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().to_string_lossy().into_owned();
        let first = workspace_id(&store, &agent_id, &path).unwrap();
        let second = workspace_id(&store, &agent_id, &path).unwrap();
        assert_eq!(first, second);
    }
}
