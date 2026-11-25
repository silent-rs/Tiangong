//! 中枢插件工具测试（R7 测试集中：原 src/lib.rs 内嵌 mod tests 迁出）。
//! 走公共入口：CentralPlugin::new + ToolOverrideHandler::handle（call_tool
//! 辅助包装）——schema-handler 一致性、派发/控制/查询/目标报告全矩阵。
use central_plugin::CentralPlugin;
use central_state::persistence::{add_confirmed_workspace, load_work_items, upsert_work_item};
use tiangong_core::core::Plugin as CentralPluginId;
use tiangong_core::session::Session;
use tiangong_core::tools::extension::ToolOverrideHandler;
use tiangong_core::tools::result::ToolResult;
use tiangong_llm::tool::ToolCall;

use central_plugin::*;
use tiangong_core::tools::extension::ToolSpecProvider;

#[test]
fn central_session_id_matches_frontend_constant() {
    // 与前端 frontend/src/lib/centralSession.ts 的 CENTRAL_SESSION_ID 一致；
    // 变更需两侧同步（历史文件按 ID 命名，改值会丢弃既有中枢对话）。
    assert_eq!(CENTRAL_SESSION_ID, "central-hub");
    assert!(is_central_session(CENTRAL_SESSION_ID));
    assert!(!is_central_session("0198abcd"));
    assert!(!is_central_session(""));
}

/// 中枢会话文件单独存储，且路径规则单点维护（读/写/元数据共用）。
#[test]
fn central_session_file_is_isolated_but_single_rule() {
    use tiangong_core::session::Session;
    let root = std::path::Path::new("/data/tiangong");
    assert_eq!(
        Session::session_file_path(root, CENTRAL_SESSION_ID).unwrap(),
        std::path::PathBuf::from("/data/tiangong/central/sessions/central-hub.json")
    );
    assert_eq!(
        Session::session_file_path(root, "0198abcd").unwrap(),
        std::path::PathBuf::from("/data/tiangong/sessions/0198abcd.json")
    );
    assert!(Session::session_file_path(root, "../evil").is_err());
    assert!(Session::session_file_path(root, "a/b").is_err());
}

/// 端到端读写一致性：中枢 Session 持久化到独立目录并可按同一路径加载。
#[test]
fn central_session_roundtrip_in_isolated_dir() {
    use tiangong_core::session::Session;
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new("中枢对话");
    session.id = CENTRAL_SESSION_ID.to_string();
    session.bind_storage_root(dir.path());
    session.try_persist_to_disk().unwrap();

    assert!(dir
        .path()
        .join("central/sessions/central-hub.json")
        .exists());
    assert!(!dir.path().join("sessions/central-hub.json").exists());

    let loaded = Session::load_from_storage(dir.path(), CENTRAL_SESSION_ID).unwrap();
    assert_eq!(loaded.id, CENTRAL_SESSION_ID);
}

/// 人设注入 + 白名单工具声明（§5.3 前三项）。
/// 测试辅助：构造 plugin（派发请求收集到返回的接收端，供断言）。
fn plugin_with_channel(
    workspace_dir: String,
    manager: tiangong_core_manager::CoreManager,
) -> (
    CentralPlugin,
    tokio::sync::mpsc::UnboundedReceiver<CentralPluginMessage>,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    // S3 fail-closed 后默认工作区须真实存在（is_dir）才放行子树——
    // fixture 统一创建登记目录，保持「默认目录已授权」的测试前提
    let _ = std::fs::create_dir_all(&workspace_dir);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let reports: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    (
        CentralPlugin::new(workspace_dir, manager, tx, std::sync::Arc::clone(&reports)),
        rx,
        reports,
    )
}

/// 测试辅助：构造带独立 sessions 目录的 manager，并写入指定 cwd 的会话元数据。
fn manager_with_session_cwds(
    root: &std::path::Path,
    cwds: &[&str],
) -> tiangong_core_manager::CoreManager {
    use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
    let sessions_dir = root.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    // 同上：会话 cwd 真实存在（复用派发的 cwd 沿用会话目录）
    for (index, cwd) in cwds.iter().enumerate() {
        let _ = std::fs::create_dir_all(cwd);
        let id = format!("s{index}");
        let session = serde_json::json!({
            "id": id,
            "title": format!("会话 {index}"),
            "created_at": "2026-11-08 10:00:00",
            "updated_at": "2026-11-08 10:00:00",
            "trust_mode": "Supervised",
            "cwd": cwd,
            "messages": []
        });
        std::fs::write(sessions_dir.join(format!("{id}.json")), session.to_string()).unwrap();
    }
    tiangong_core_manager::CoreManager::new(
        CoreConfigProvider::new(CoreConfig::default()),
        root.to_path_buf(),
    )
}

#[test]
fn central_plugin_persona_and_tool_specs() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    assert_eq!(CentralPluginId::id(&plugin), "central");
    let sections =
        <CentralPlugin as tiangong_core::tools::extension::PromptSectionProvider>::prompt_sections(
            &plugin,
        );
    assert_eq!(sections.len(), 1);
    assert!(sections[0].contains("天工中枢"));
    assert!(sections[0].contains("先调用工具了解情况再回答"));
    let specs = plugin.tool_specs();
    assert!(
        specs.iter().any(|spec| spec.name == "inspect_context"),
        "inspect_context 必须有声明（review o：有 handler 无 spec 则模型不可见）"
    );
    assert!(
        specs
            .iter()
            .find(|spec| spec.name == "dispatch_work")
            .is_some_and(|spec| spec
                .input_schema
                .get("properties")
                .and_then(|props| props.get("session_id"))
                .is_some()),
        "dispatch_work 声明必须包含 session_id（复用入口）"
    );
}

/// 工具声明与 handler 一致性：spec 集合 == 可处理集合（防再脱节）。
#[test]
fn central_tool_specs_equal_handlers() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let spec_names: std::collections::HashSet<String> = plugin
        .tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    // handler 侧从源码 match arm 提取（无副作用、无遗漏——review 硬断言：
    // spec 与 handler 双向相等，曾以 10 名清单 + let _ = result 假绿）
    let source = include_str!("../src/lib.rs");
    let handle_pos = source
        .find("let result = match call.name.as_str() {")
        .expect("handle match");
    let match_body = &source[handle_pos..source.len().min(handle_pos + 16000)];
    // 截断到 match 闭合（首个顶格 '}'）
    let body_end = match_body
        .find("\n        let result")
        .unwrap_or(match_body.len());
    let body = &match_body[..body_end];
    // 按 => 分段：每段末尾的引号词（可能跨行以 | 连接）是 arm 名
    let mut handler_names: std::collections::HashSet<String> = Default::default();
    for segment in body.split("=>") {
        // 取段尾部连续的 arm 名行（以引号词结尾的行，向上直到非 arm 行）
        let tail: Vec<&str> = segment
            .lines()
            .rev()
            .take_while(|line| {
                let t = line.trim();
                (t.starts_with('"') || t.starts_with('|')) && !t.contains("match")
            })
            .collect();
        for line in tail {
            for token in line.split('|') {
                let token = token.trim().trim_matches('"');
                if !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    handler_names.insert(token.to_string());
                }
            }
        }
    }
    // match 头部（call.name.as_str() {）不会出现在 tail（无引号词）；
    // 兜底过滤显然非工具名的内容
    handler_names.retain(|name| name != "as_str");
    assert_eq!(
        spec_names, handler_names,
        "spec 与 handler 集合必须相等（spec 缺失模型不可见；handler 缺失为死声明）"
    );
    assert_eq!(
        spec_names.len(),
        10,
        "工具数应为 10（二十轮合并：inspect_context / control / work / goal 四个统一入口）"
    );
}

/// 复用校验：中枢对话自身不可作为工作会话（review q）。
#[test]
fn dispatch_rejects_reusing_central_hub_itself() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let result = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "自查", "session_id": CENTRAL_SESSION_ID}),
    );
    assert!(!result.ok);
    assert!(result.stderr.contains("中枢对话自身"));
    assert!(dispatch_rx.try_recv().is_err(), "守卫拒绝不得派发");
}

fn call_tool(plugin: &CentralPlugin, name: &str, args: serde_json::Value) -> ToolResult {
    let call = ToolCall {
        id: "t1".to_string(),
        name: name.to_string(),
        arguments: args,
    };
    let mut session = Session::new("测试");
    let future = ToolOverrideHandler::handle(plugin, &call, &mut session, "test");
    futures_block_on(future).expect("中枢工具必须被本插件处理")
}

/// 极简 block_on（测试内同步等待一次性 future，不引入运行时依赖）。
fn futures_block_on<F: std::future::Future>(future: F) -> F::Output {
    // 简单轮询：当前 future 全部同步完成（Box::pin(async move { result })），
    // poll 一次即 Ready。
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop(_: *const ()) {}
    fn clone(ptr: *const ()) -> RawWaker {
        RawWaker::new(ptr, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("中枢插件工具不应挂起"),
    }
}

#[test]
fn get_context_reports_central_state() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let result = call_tool(&plugin, "get_context", serde_json::json!({}));
    assert!(result.ok);
    assert!(result.stdout.contains("身份: 天工中枢"));
    assert!(result.stdout.contains("/tmp/ws"));
}

/// 派发：模型 key 校验通过后经通道发出并登记。
#[test]
fn dispatch_work_sends_request_and_registers() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    // 提供一个可用模型，供派发校验
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {
                "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}
            },
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    // /tmp 非默认工作区：预先落盘宿主授权产物（Workspace 授权）
    let _ = add_confirmed_workspace(plugin.sessions.storage_root(), std::path::Path::new("/tmp"));
    let result = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "审查仓库提交", "model": "k3", "cwd": "/tmp"}),
    );
    assert!(result.ok, "stderr: {}", result.stderr);
    assert!(result.stdout.contains("已受理派发"));
    let message = dispatch_rx.try_recv().expect("派发请求应经通道发出");
    let request = match message {
        CentralPluginMessage::Dispatch(request) => request,
        other => panic!("应收到 Dispatch 消息，实际 {other:?}"),
    };
    assert_eq!(request.model.as_deref(), Some("k3"));
    assert_eq!(request.task, "审查仓库提交");
    assert_eq!(request.cwd.as_deref(), Some("/tmp"));
    assert!(!request.work_id.is_empty());
    // 登记表可见
    let listing = call_tool(&plugin, "work", serde_json::json!({"action": "list"}));
    assert!(listing.stdout.contains(&request.work_id));
    assert!(listing.stdout.contains("审查仓库提交"));
}

/// 派发：模型 key 不存在时拒绝并附可用清单。
#[test]
fn dispatch_work_rejects_unknown_model() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {
                "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}
            },
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let result = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "审查", "model": "not-exist"}),
    );
    assert!(!result.ok);
    assert!(result.stderr.contains("可用模型"));
    assert!(result.stderr.contains("k3"));
    assert!(dispatch_rx.try_recv().is_err(), "校验失败不得派发");
}

/// 模型清单：列出 key 并标记路由默认。
#[test]
fn list_models_reports_registry() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {
                "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]},
                "glm": {"provider": "p", "model": "glm-5.3", "capabilities": ["chat"]}
            },
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let result = call_tool(&plugin, "list_models", serde_json::json!({}));
    assert!(result.ok);
    assert!(result.stdout.contains("k3"));
    assert!(result.stdout.contains("k3-pro"));
    assert!(result.stdout.contains("（默认）"));
}

#[test]
fn inspect_workspace_reads_git_metadata() {
    let main = tempfile::tempdir().unwrap();
    let git = main.path().join(".git");
    std::fs::create_dir_all(git.join("worktrees/central-poc")).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/poc/central-mode\n").unwrap();
    let worktree_dir = main.path().join("wt/central-poc");
    std::fs::create_dir_all(&worktree_dir).unwrap();
    // worktree 的 .git 是指向主仓库元目录的指针文件
    std::fs::write(
        worktree_dir.join(".git"),
        format!("gitdir: {}\n", git.join("worktrees/central-poc").display()),
    )
    .unwrap();
    std::fs::write(
        git.join("worktrees/central-poc/HEAD"),
        "ref: refs/heads/poc/central-mode\n",
    )
    .unwrap();
    std::fs::write(
        git.join("worktrees/central-poc/gitdir"),
        format!("{}/.git\n", worktree_dir.display()),
    )
    .unwrap();

    let (plugin, _dispatch_rx, _reports) = plugin_with_channel(
        main.path().display().to_string(),
        manager_with_session_cwds(main.path(), &[]),
    );
    let result = call_tool(
        &plugin,
        "inspect_context",
        serde_json::json!({"focus": "workspace", "path": worktree_dir.display().to_string()}),
    );
    assert!(result.ok);
    assert!(result.stdout.contains("主仓库分支: poc/central-mode"));
    assert!(result.stdout.contains("worktree central-poc"));
}

#[test]
fn inspect_workspace_rejects_missing_path() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let result = call_tool(
        &plugin,
        "inspect_context",
        serde_json::json!({"focus": "workspace", "path": "/definitely/not/exist"}),
    );
    assert!(!result.ok);
    assert_eq!(result.error_kind.as_deref(), Some("argument_error"));
}
#[test]
fn list_workspaces_aggregates_session_cwds_with_git_branch() {
    let root = tempfile::tempdir().unwrap();
    // 两个会话 cwd，其中一个重复
    let repo = root.path().join("dev/tiangong");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/poc/central-mode\n").unwrap();
    let manager = manager_with_session_cwds(
        root.path(),
        &[repo.to_str().unwrap(), repo.to_str().unwrap()],
    );
    let (plugin, _dispatch_rx, _reports) =
        plugin_with_channel(root.path().display().to_string(), manager);
    let result = call_tool(
        &plugin,
        "inspect_context",
        serde_json::json!({"focus": "workspaces"}),
    );
    assert!(result.ok);
    // 默认工作区在列（去重后仅一次 dev/tiangong）
    assert!(result.stdout.contains("（默认）"));
    assert_eq!(result.stdout.matches("dev/tiangong").count(), 1);
    assert!(result.stdout.contains("git 分支 poc/central-mode"));
}
/// 应用尽用兜底：无 session_id 的后续派发，返回应附既往派发提示。
#[test]
fn dispatch_without_reuse_hints_prior_works() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {
                "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}
            },
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    // 第一次派发（新建）：无既往，无提示
    let first = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "第一轮 review"}),
    );
    assert!(first.ok);
    assert!(!first.stdout.contains("既往派发"));
    // 第二次派发仍未指定 session_id：应提示既往派发供自查
    let second = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "继续 review 新增内容"}),
    );
    assert!(second.ok);
    assert!(second.stdout.contains("既往派发"));
    assert!(second.stdout.contains(
        "继续 review 新增内容"
            .chars()
            .take(30)
            .collect::<String>()
            .as_str()
    ));
    assert!(second.stdout.contains("向对应会话重派"));
}

/// 回报批量模式：暂存 → collect 取走 → 再 collect 为空。
#[test]
fn collect_work_reports_takes_all_then_empty() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, _dispatch_rx, reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    reports
        .lock()
        .unwrap()
        .push("[后台工作完成] work-a：结果一".to_string());
    reports
        .lock()
        .unwrap()
        .push("[后台工作完成] work-b：结果二".to_string());

    let first = call_tool(&plugin, "work", serde_json::json!({"action": "reports"}));
    assert!(first.ok);
    assert!(first.stdout.contains("共 2 条"));
    assert!(first.stdout.contains("work-a"));
    assert!(first.stdout.contains("work-b"));

    let second = call_tool(&plugin, "work", serde_json::json!({"action": "reports"}));
    assert!(second.ok);
    assert!(second.stdout.contains("没有待汇报"));
}
/// 引导消息：校验 + 登记 + 通道消息类型。
#[test]
fn dispatch_work_to_existing_session_dispatches_reused() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let _ = add_confirmed_workspace(plugin.sessions.storage_root(), std::path::Path::new("/tmp"));
    // 目标不存在 → 拒绝
    let missing = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"session_id": "nope", "task": "调整"}),
    );
    assert!(!missing.ok);
    assert!(missing.stderr.contains("会话不存在"));
    // 建一个会话文件后 → 通过并发送 Dispatch（reused_session=true；
    // 二十轮合并：send_to_worker 并入 dispatch_work，Steer 消息消失，
    // 归属合并由宿主 plan_dispatch + central-state 具名锁保证）
    let target = root.path().join("sessions/worker-1.json");
    std::fs::write(
        &target,
        serde_json::json!({
            "id": "worker-1", "title": "w", "created_at": "2026-11-10 10:00:00",
            "updated_at": "2026-11-10 10:00:00", "trust_mode": "Supervised",
            "cwd": "/tmp", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    let ok = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"session_id": "worker-1", "task": "重点看事件流部分"}),
    );
    assert!(ok.ok, "stderr: {}", ok.stderr);
    match dispatch_rx.try_recv().expect("应有通道消息") {
        CentralPluginMessage::Dispatch(request) => {
            assert_eq!(request.session_id, "worker-1");
            assert_eq!(request.task, "重点看事件流部分");
            assert!(request.reused_session, "复用会话必须标记 reused_session");
            assert!(!request.work_id.starts_with("steer-"));
            assert!(!request.execution_id.is_empty());
        }
        other => panic!("应收到 Dispatch，实际 {other:?}"),
    }
    // 中枢自身 → 拒绝
    let self_send = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"session_id": CENTRAL_SESSION_ID, "task": "x"}),
    );
    assert!(!self_send.ok);
}
/// 跨会话历史搜索：标题与正文命中、片段返回。
#[tokio::test]
async fn search_history_finds_title_and_body() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    // 两个会话：一个标题命中，一个正文命中
    std::fs::write(
        root.path().join("sessions/s-title.json"),
        serde_json::json!({
            "id": "s-title", "title": "中枢模式开发准备", "created_at": "2026-11-10 10:00:00",
            "updated_at": "2026-11-10 11:00:00", "trust_mode": "Supervised",
            "cwd": "/tmp", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.path().join("sessions/s-body.json"),
        serde_json::json!({
            "id": "s-body", "title": "其他", "created_at": "2026-11-10 10:00:00",
            "updated_at": "2026-11-10 12:00:00", "trust_mode": "Supervised",
            "cwd": "/tmp",
            "messages": [
                {"id": "m1", "role": "assistant", "created_at": "2026-11-10 12:00:01",
                 "content": [{"type": "text", "text": "中枢模式的设计模式调整已经完成"}]}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let call = ToolCall {
        id: "t1".to_string(),
        name: "search_history".to_string(),
        arguments: serde_json::json!({"query": "中枢模式"}),
    };
    let mut session = Session::new("测试");
    let result = ToolOverrideHandler::handle(&plugin, &call, &mut session, "test")
        .await
        .expect("应有结果");
    assert!(result.ok);
    assert!(
        result.stdout.contains("s-title"),
        "标题命中：{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("设计模式调整"),
        "正文片段：{}",
        result.stdout
    );
    // 无命中
    let call2 = ToolCall {
        id: "t2".to_string(),
        name: "search_history".to_string(),
        arguments: serde_json::json!({"query": "绝不存在的关键词xyz"}),
    };
    let result2 = ToolOverrideHandler::handle(&plugin, &call2, &mut session, "test")
        .await
        .expect("应有结果");
    assert!(result2.ok);
    assert!(result2.stdout.contains("未找到"));
}
/// 派发登记会话维度 upsert：同会话再派发更新而非堆积；上限 20 条。
#[test]
fn work_items_upsert_by_session_and_cap_20() {
    let root = tempfile::tempdir().unwrap();
    // 预置 25 条不同会话的记录
    for i in 0..25 {
        let record = WorkItemRecord {
            work_id: format!("work-old-{i}"),
            reused_session: false,
            session_id: format!("sess-{i}"),
            task: format!("旧任务 {i}"),
            model: None,
            dispatched_at: format!("2026-11-10 1{:02}:00:00", i % 10),
            finished_at: Some("2026-11-10 12:00:00".to_string()),
            outcome: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            execution_id: None,
        };
        let _ = upsert_work_item(root.path(), &record);
    }
    // 同一会话 sess-24 再派发 → 更新（不新增）
    let again = WorkItemRecord {
        work_id: "work-new-1".to_string(),
        reused_session: true,
        session_id: "sess-24".to_string(),
        task: "继续上次的任务".to_string(),
        model: Some("k3".to_string()),
        dispatched_at: "2026-11-10 15:00:00".to_string(),
        finished_at: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        outcome: None,
        execution_id: None,
    };
    let _ = upsert_work_item(root.path(), &again);
    let items = load_work_items(root.path());
    assert_eq!(items.len(), 20, "上限 20：{}", items.len());
    // sess-24 只有一条且是更新后的
    let hits: Vec<_> = items
        .iter()
        .filter(|item| item.session_id == "sess-24")
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].work_id, "work-new-1");
    assert_eq!(hits[0].task, "继续上次的任务");
    assert!(hits[0].reused_session);
    // 最旧的 sess-0 已被裁剪
    assert!(!items.iter().any(|item| item.session_id == "sess-0"));
}
/// 终态登记兜底：work_id 未命中时按 session_id 找未终态条目——
/// 用户实测「执行完毕显示中断」的修复路径；双键都未命中时不落盘。
#[test]
fn mark_finished_falls_back_to_session() {
    let root = tempfile::tempdir().unwrap();
    let record = WorkItemRecord {
        work_id: "work-real".to_string(),
        reused_session: false,
        session_id: "sess-1".to_string(),
        task: "执行任务".to_string(),
        model: None,
        dispatched_at: "2026-11-11 12:00:00".to_string(),
        finished_at: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        outcome: None,
        execution_id: None,
    };
    let _ = upsert_work_item(root.path(), &record);
    // 错误 work_id + 正确 session_id：兜底命中并写终态
    let _ = mark_work_item_finished_with_session(root.path(), "work-wrong", Some("sess-1"), false);
    let items = load_work_items(root.path());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].work_id, "work-real");
    assert!(
        items[0].finished_at.is_some(),
        "session 兜底应写入终态：{items:?}"
    );
    // 双键都未命中：不新增不改动
    let _ = mark_work_item_finished_with_session(root.path(), "work-none", Some("sess-none"), true);
    let after = load_work_items(root.path());
    assert_eq!(after.len(), 1, "未命中不得新增条目：{after:?}");
    // 成败落盘：成功 → completed；失败 → failed
    let finished = &after[0];
    assert_eq!(finished.outcome.as_deref(), Some("completed"));
    let _ = mark_work_item_finished_with_session(root.path(), "work-x", Some("sess-9"), true);
    // sess-9 无未终态条目（sess-1 已终态）→ 不改动
    assert_eq!(load_work_items(root.path()).len(), 1);
}

/// legacy 终态从中枢会话回报推断成败：回报含「[后台工作失败] work-X」
/// → failed；其余终态 → completed。
#[test]
fn recover_infers_legacy_outcome_from_central_session() {
    let root = tempfile::tempdir().unwrap();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    // 构造中枢会话（含一条失败回报与一条完成回报）
    let central_dir = root.path().join("central").join("sessions");
    std::fs::create_dir_all(&central_dir).unwrap();
    std::fs::write(
        central_dir.join("central-hub.json"),
        serde_json::json!({
            "messages": [
                {"role": "user", "content": "[后台工作完成] work-legacy-ok（模型 k3）\n任务：正常\n执行结果：完成"},
                {"role": "user", "content": "[后台工作失败] work-legacy-bad（模型 k3）\n任务：出错\n失败原因：超时"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    for (id, task) in [
        ("work-legacy-ok", "正常任务"),
        ("work-legacy-bad", "出错任务"),
    ] {
        let _ = upsert_work_item(
            root.path(),
            &WorkItemRecord {
                work_id: id.to_string(),
                reused_session: false,
                session_id: format!("s-{id}"),
                task: task.to_string(),
                model: None,
                dispatched_at: now.clone(),
                finished_at: Some(now.clone()),
                outcome: None, // legacy：字段上线前的终态记录
                events: Vec::new(),
                queued_at: None,
                after_work_id: None,
                cwd: None,
                execution_id: None,
            },
        );
    }
    let entries = recover_registry_entries(root.path());
    let ok = entries.iter().find(|e| e.work_id == "work-legacy-ok");
    let bad = entries.iter().find(|e| e.work_id == "work-legacy-bad");
    assert_eq!(ok.map(|e| e.state.as_str()), Some("completed"));
    assert_eq!(bad.map(|e| e.state.as_str()), Some("failed"));
}

/// 重启重建：outcome 分态、旧终态近似 completed、未终态近期
/// interrupted、过期未终态跳过、completed/failed 各截 10。
#[test]
fn recover_registry_rebuilds_all_states() {
    let root = tempfile::tempdir().unwrap();
    let now = chrono::Local::now();
    let recent = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let stale = (now - chrono::Duration::hours(12))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let mut records = Vec::new();
    for i in 0..12 {
        records.push(WorkItemRecord {
            work_id: format!("work-c{i}"),
            reused_session: false,
            session_id: format!("sc{i}"),
            task: format!("完成 {i}"),
            model: None,
            dispatched_at: recent.clone(),
            finished_at: Some(recent.clone()),
            outcome: Some("completed".to_string()),
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            execution_id: None,
        });
    }
    records.push(WorkItemRecord {
        work_id: "work-old-done".to_string(),
        reused_session: false,
        session_id: "s-old".to_string(),
        task: "旧终态（无 outcome）".to_string(),
        model: None,
        dispatched_at: stale.clone(),
        finished_at: Some(stale.clone()),
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: None,
    });
    records.push(WorkItemRecord {
        work_id: "work-fail".to_string(),
        reused_session: false,
        session_id: "s-f".to_string(),
        task: "失败任务".to_string(),
        model: None,
        dispatched_at: recent.clone(),
        finished_at: Some(recent.clone()),
        outcome: Some("failed".to_string()),
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: None,
    });
    records.push(WorkItemRecord {
        work_id: "work-recent-open".to_string(),
        reused_session: false,
        session_id: "s-ro".to_string(),
        task: "近期未终态".to_string(),
        model: None,
        dispatched_at: recent.clone(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: None,
    });
    records.push(WorkItemRecord {
        work_id: "work-stale-open".to_string(),
        reused_session: false,
        session_id: "s-so".to_string(),
        task: "过期未终态".to_string(),
        model: None,
        dispatched_at: stale.clone(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: None,
    });
    for record in &records {
        let _ = upsert_work_item(root.path(), record);
    }
    let entries = recover_registry_entries(root.path());
    let count = |state: &str| entries.iter().filter(|e| e.state.as_str() == state).count();
    // completed = 12 条 outcome + 1 条旧终态近似 → 截断为 10
    assert_eq!(count("completed"), 10, "completed 截断 10：{entries:?}");
    // 截断保留最新的（work-c11 在，work-c0 被移除）
    assert!(entries.iter().any(|e| e.work_id == "work-c11"));
    assert!(!entries.iter().any(|e| e.work_id == "work-c0"));
    assert_eq!(count("failed"), 1);
    assert!(entries
        .iter()
        .any(|e| e.work_id == "work-fail" && e.state.as_str() == "failed"));
    // 近期未终态 → interrupted；过期未终态跳过
    assert_eq!(count("interrupted"), 1);
    assert!(entries
        .iter()
        .any(|e| e.work_id == "work-recent-open" && e.state.as_str() == "interrupted"));
    assert!(!entries.iter().any(|e| e.work_id == "work-stale-open"));
}

/// 登记文件损坏（坏 JSON）时按空表恢复，后续 upsert 修复文件。
#[test]
fn work_items_recover_from_corruption() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("central")).unwrap();
    std::fs::write(
        central_state::persistence::work_items_path(root.path()),
        "{ not valid json !!",
    )
    .unwrap();
    // 损坏 → 空表（不 panic）
    assert!(load_work_items(root.path()).is_empty());
    // upsert 以空表为基础写入 → 文件被合法内容覆盖
    let record = WorkItemRecord {
        work_id: "work-1".to_string(),
        reused_session: false,
        session_id: "sess-1".to_string(),
        task: "任务".to_string(),
        model: None,
        dispatched_at: "2026-11-11 12:00:00".to_string(),
        finished_at: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        outcome: None,
        execution_id: None,
    };
    let _ = upsert_work_item(root.path(), &record);
    let items = load_work_items(root.path());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].work_id, "work-1");
}

/// 工作区确认判定：默认工作区子树按前缀放行（含子目录）；同级近名
/// 目录不误放行；登记目录 canonicalize 精确匹配。
#[test]
fn workspace_confirmation_prefix_and_canonical() {
    let root = tempfile::tempdir().unwrap();
    let default_ws = tempfile::tempdir().unwrap(); // 默认工作区（真实存在，canonicalize 生效）
    let default = default_ws.path().to_string_lossy().to_string();
    let inside = default_ws.path().join("sub").join("proj");
    std::fs::create_dir_all(&inside).unwrap();
    // 默认工作区与其子目录：天然已确认
    assert!(workspace_confirmed(root.path(), &default, &default));
    assert!(workspace_confirmed(
        root.path(),
        &default,
        &inside.to_string_lossy()
    ));
    // 同级近名目录（/x-pro 不是 /x 子树）：不放行
    let near = default_ws.path().parent().unwrap().join(format!(
        "{}-near",
        default_ws.path().file_name().unwrap().to_string_lossy()
    ));
    assert!(!workspace_confirmed(
        root.path(),
        &default,
        &near.to_string_lossy()
    ));
    // 未登记的无关目录：不放行
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().canonicalize().unwrap();
    assert!(!workspace_confirmed(
        root.path(),
        &default,
        &outside_path.to_string_lossy()
    ));
    // 登记（canonicalize 落盘）后：原样路径与带尾斜杠变体均放行
    std::fs::create_dir_all(root.path().join("central")).unwrap();
    std::fs::write(
        central_state::persistence::confirmed_workspaces_path(root.path()),
        serde_json::to_string(&vec![outside_path.to_string_lossy().to_string()]).unwrap(),
    )
    .unwrap();
    assert!(workspace_confirmed(
        root.path(),
        &default,
        &outside_path.to_string_lossy()
    ));
}

/// R28-8（第 28 轮 review S3，探针 PR-09）：空默认工作区 fail-closed。
/// 空默认不得成为空前缀放行任意绝对路径；空 target（上游已解析为合法
/// 默认后到达）保持放行；不存在的默认路径同样不放行子树。
#[test]
fn workspace_confirmation_empty_default_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    // 空默认 + 未登记绝对路径：拒绝（PR-09 场景——此前空字符串前缀
    // 匹配一切绝对路径）
    assert!(!workspace_confirmed(root.path(), "", "/tmp"));
    assert!(!workspace_confirmed(root.path(), "  ", "/tmp/whatever"));
    // 空 target：保持放行（「使用默认目录」由上游解析；dispatch 规划段
    // 在 cwd 非空且 is_dir 校验之后才调用本函数）
    assert!(workspace_confirmed(root.path(), "", ""));
    assert!(workspace_confirmed(root.path(), "/some/default", "  "));
    // 默认路径不存在（is_dir false）：子树不放行
    let ghost_default = root.path().join("ghost-default");
    let ghost_child = ghost_default.join("child");
    assert!(!workspace_confirmed(
        root.path(),
        &ghost_default.to_string_lossy(),
        &ghost_child.to_string_lossy()
    ));
    // 即便 target 恰好等于不存在的默认路径字符串：也不走默认子树放行
    // （fail-closed；如确属该目录，需用户显式登记或默认真实存在）
    assert!(!workspace_confirmed(
        root.path(),
        &ghost_default.to_string_lossy(),
        &ghost_default.to_string_lossy()
    ));
}

/// 目标登记与状态更新：同名幂等、非法状态拒绝、get_context 注入。
#[test]
fn goal_lifecycle_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let _ = &mut dispatch_rx; // 本测试无派发，防 unused
                              // 登记
    let created = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "save_goal", "title": "完成中枢模式 POC", "description": "按设计稿实现完整功能"}),
    );
    assert!(created.ok, "stderr: {}", created.stderr);
    assert!(created.stdout.contains("完成中枢模式 POC"));
    // 同名幂等
    let again = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "save_goal", "title": "完成中枢模式 POC"}),
    );
    assert!(again.ok);
    assert!(again.stdout.contains("已存在"));
    // 空标题拒绝
    let empty = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "save_goal", "title": " "}),
    );
    assert!(!empty.ok);
    // 状态更新（取出 goal_id）
    let goal_id = load_goals(plugin.sessions.storage_root())[0]
        .goal_id
        .clone();
    let updated = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "set_goal_status", "goal_id": goal_id, "status": "achieved"}),
    );
    assert!(updated.ok, "stderr: {}", updated.stderr);
    assert_eq!(
        load_goals(plugin.sessions.storage_root())[0].status,
        "achieved"
    );
    // 非法状态拒绝
    let bad = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "set_goal_status", "goal_id": goal_id, "status": "done"}),
    );
    assert!(!bad.ok);
    // get_context 注入目标（新的 active 目标）
    let _ = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "save_goal", "title": "重构状态机"}),
    );
    let context = call_tool(&plugin, "get_context", serde_json::json!({}));
    assert!(context.ok);
    assert!(context.stdout.contains("重构状态机"), "{}", context.stdout);
}

/// 报告存档与查询往返。
#[test]
fn report_archive_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let created = call_tool(
        &plugin,
        "goal",
        serde_json::json!({
            "action": "save_report",
            "work_id": "work-x1",
            "summary": "第十轮 review 修复全部完成",
            "artifacts": ["/tmp/report.md"],
            "risks": ["旧记录无 outcome"]
        }),
    );
    assert!(created.ok, "stderr: {}", created.stderr);
    // 空摘要拒绝
    let empty = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "save_report", "work_id": "w", "summary": " "}),
    );
    assert!(!empty.ok);
    // 查询
    let listed = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "list_reports"}),
    );
    assert!(listed.ok);
    assert!(listed.stdout.contains("第十轮 review"), "{}", listed.stdout);
    assert!(listed.stdout.contains("/tmp/report.md"));
    // 按工作过滤
    let filtered = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "list_reports", "work_id": "work-x1"}),
    );
    assert!(filtered.ok);
    assert!(filtered.stdout.contains("第十轮 review"));
    let none = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "list_reports", "work_id": "work-none"}),
    );
    assert!(none.ok);
    assert!(none.stdout.contains("没有"));
}

/// dispatch_work schema：after_work_id/parallel 必须在 properties 内
/// （review P1：曾落在 properties 外——模型看不到参数，写入锁豁免
/// 不可达且同目录派发被无条件串行）。
#[test]
fn dispatch_work_schema_exposes_queue_and_parallel_params() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let specs = plugin.tool_specs();
    let spec = specs
        .iter()
        .find(|spec| spec.name == "dispatch_work")
        .expect("dispatch_work spec");
    let properties = spec
        .input_schema
        .get("properties")
        .and_then(|value| value.as_object())
        .expect("properties 必须是对象");
    for key in [
        "task",
        "model",
        "cwd",
        "session_id",
        "after_work_id",
        "parallel",
    ] {
        assert!(
            properties.contains_key(key),
            "dispatch_work.properties 缺少 {key}（模型将看不到该参数）"
        );
    }
}

/// 时间线：事件追加持久化 + get_work_timeline 工具查询往返。
#[test]
fn work_timeline_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &["/tmp/ws"]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let dispatch = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "时间线任务", "session_id": "s0"}),
    );
    assert!(dispatch.ok, "stderr: {}", dispatch.stderr);
    // 追加事件（模拟宿主侧写入——R5：事件日志是唯一事实源）
    central_plugin::record_central_event_by_session(
        root.path(),
        "s0",
        tiangong_central::event::WorkEventType::WorkItemWaitingUser,
        "提出问题，工作中挂起",
    );
    central_plugin::record_central_event_by_session(
        root.path(),
        "s0",
        tiangong_central::event::WorkEventType::WorkItemStarted,
        "收到答复恢复",
    );
    // 查询往返
    let work_id = {
        let items = plugin.work_items.lock().unwrap_or_else(|e| e.into_inner());
        items
            .iter()
            .rev()
            .find(|item| item.session_id == "s0")
            .map(|item| item.work_id.clone())
            .unwrap()
    };
    let timeline = call_tool(
        &plugin,
        "work",
        serde_json::json!({"action": "timeline", "work_id": work_id}),
    );
    assert!(timeline.ok, "stderr: {}", timeline.stderr);
    assert!(
        timeline.stdout.contains("work_item.waiting_user"),
        "{}",
        timeline.stdout
    );
    assert!(
        timeline.stdout.contains("提出问题，工作中挂起"),
        "{}",
        timeline.stdout
    );
    assert!(
        timeline.stdout.contains("work_item.started"),
        "{}",
        timeline.stdout
    );
    // 未知的 work_id 拒绝
    let ghost = call_tool(
        &plugin,
        "work",
        serde_json::json!({"action": "timeline", "work_id": "work-none"}),
    );
    assert!(!ghost.ok);
}

/// Review Z2 定型：引导沿用 owner work_id（不再生成 steer-*）——
/// 归属与终态一致，重启投影无 Running 幽灵。
#[test]
fn dispatch_work_reused_session_keeps_registration_clean() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &["/tmp/ws"]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    // 先派发一个工作（owner 登记）
    let dispatched = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "原工作", "session_id": "s0"}),
    );
    assert!(dispatched.ok, "stderr: {}", dispatched.stderr);
    let owner_id = match dispatch_rx.try_recv().expect("派发请求") {
        CentralPluginMessage::Dispatch(request) => request.work_id,
        other => panic!("应收到 Dispatch，实际 {other:?}"),
    };
    assert!(!owner_id.starts_with("steer-"));
    // 复用会话再派发：Dispatch{reused_session=true} + 独立执行归属
    // （Z2 归属合并已上移宿主 plan_dispatch：并入 Running 原工作 /
    // merge_waiting_user 恢复 owner，central-state 具名锁
    // steer_into_running_work_keeps_ownership 覆盖）
    let steered = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"session_id": "s0", "task": "补充要求"}),
    );
    assert!(steered.ok, "stderr: {}", steered.stderr);
    match dispatch_rx.try_recv().expect("引导请求") {
        CentralPluginMessage::Dispatch(request) => {
            assert!(request.reused_session);
            assert_eq!(request.session_id, "s0");
            assert_ne!(request.execution_id, "", "复用派发仍是独立执行（R0 契约）");
            // CC2 插件侧归属（二十一轮 review）：复用派发沿用 owner work_id
            assert_eq!(
                request.work_id, owner_id,
                "复用派发必须沿用 owner work_id（status/control 可达）"
            );
        }
        other => panic!("应收到 Dispatch，实际 {other:?}"),
    }
    // 登记无 steer-* 条目，且同会话仍只有 owner 一条（无新 id 顶替/翻倍）
    let items = load_work_items(plugin.sessions.storage_root());
    assert!(items.iter().all(|item| !item.work_id.starts_with("steer-")));
    let s0_entries: Vec<_> = items.iter().filter(|i| i.session_id == "s0").collect();
    assert_eq!(s0_entries.len(), 1, "同会话登记不因复用派发翻倍");
    assert_eq!(s0_entries[0].work_id, owner_id);
    // R29 P2/F8：work list 读内存，不应显示刚发的引导/答复文本；
    // 与磁盘 owner task 必须一致（只测磁盘会漏掉这一缺陷）。
    assert_eq!(s0_entries[0].task, "原工作");
    let listed = call_tool(&plugin, "work", serde_json::json!({"action": "list"}));
    assert!(listed.ok);
    assert!(listed.stdout.contains("原工作"), "{}", listed.stdout);
    assert!(!listed.stdout.contains("补充要求"), "{}", listed.stdout);
    // work status(owner) 可达：不再报「未找到工作」
    let status = call_tool(
        &plugin,
        "work",
        serde_json::json!({"action": "status", "work_id": owner_id}),
    );
    assert!(status.ok, "stderr: {}", status.stderr);
}

/// 二十一轮 review 修复锁：复用会话带 model = 换模型通道（不再是
/// M5 时代的「已忽略请求的 X」——switch_worker_model 移除后的
/// 用户裁决，提示文本必须与新行为一致）。
#[test]
fn dispatch_work_reused_with_model_announces_switch() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &["/tmp/ws"]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    std::fs::write(
        root.path().join("sessions/s9.json"),
        serde_json::json!({
            "id": "s9", "title": "w", "created_at": "2026-11-10 10:00:00",
            "updated_at": "2026-11-10 10:00:00", "trust_mode": "Supervised",
            "cwd": "/tmp/ws", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    let ok = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"session_id": "s9", "task": "继续分析", "model": "k3"}),
    );
    assert!(ok.ok, "stderr: {}", ok.stderr);
    match dispatch_rx.try_recv().expect("派发请求") {
        CentralPluginMessage::Dispatch(request) => {
            assert!(request.reused_session);
            assert_eq!(request.model.as_deref(), Some("k3"), "model 必须随请求流动");
        }
        other => panic!("应收到 Dispatch，实际 {other:?}"),
    }
    assert!(
        ok.stdout.contains("切换为 k3"),
        "提示应宣告切换（新语义）：{}",
        ok.stdout
    );
    assert!(
        !ok.stdout.contains("已忽略请求"),
        "不得再出现 M5 旧语义（忽略请求模型）：{}",
        ok.stdout
    );
}
/// R5：模型只能「请求」Workspace 授权（触发卡片），不能登记授权。
#[test]
fn request_workspace_approval_sends_request_not_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    // 空路径/空指纹拒绝
    let empty = call_tool(
        &plugin,
        "request_workspace_approval",
        serde_json::json!({"path": "  ", "task_fingerprint": "t"}),
    );
    assert!(!empty.ok);
    let no_fingerprint = call_tool(
        &plugin,
        "request_workspace_approval",
        serde_json::json!({"path": "/tmp"}),
    );
    assert!(!no_fingerprint.ok, "缺任务指纹必须拒绝（防偷换无从校验）");

    // 正常请求：发出 RequestWorkspaceApproval 消息（不是授权登记）
    let ok = call_tool(
        &plugin,
        "request_workspace_approval",
        serde_json::json!({"path": "/tmp/proj", "task_fingerprint": "发布模块 v2"}),
    );
    assert!(ok.ok, "stderr: {}", ok.stderr);
    assert!(ok.stdout.contains("已向用户发出"), "{}", ok.stdout);
    match dispatch_rx.try_recv().expect("请求应经通道发出") {
        CentralPluginMessage::RequestWorkspaceApproval {
            path,
            task_fingerprint,
        } => {
            assert_eq!(path, "/tmp/proj");
            assert_eq!(task_fingerprint, "发布模块 v2");
        }
        other => panic!("应收到 RequestWorkspaceApproval，实际 {other:?}"),
    }
    // 请求不产生授权事实
    assert!(
        load_confirmed_workspaces(plugin.sessions.storage_root()).is_empty(),
        "请求授权不得直接登记工作区"
    );
}

/// 工作控制工具：work_id 校验 + Control 消息送达（通道层验证）。
#[test]
fn work_control_validates_and_sends() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &["/tmp/ws"]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    // 派发一个工作（登记存在）
    let dispatch = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "长任务", "session_id": "s0"}),
    );
    assert!(dispatch.ok, "stderr: {}", dispatch.stderr);
    let work_id = {
        let items = plugin.work_items.lock().unwrap_or_else(|e| e.into_inner());
        items
            .iter()
            .rev()
            .find(|item| item.session_id == "s0")
            .map(|item| item.work_id.clone())
            .unwrap()
    };
    // 排空派发消息（dispatch 先于 control 到达）
    let _ = dispatch_rx.try_recv();
    // 取消：消息送达
    let cancel = call_tool(
        &plugin,
        "control",
        serde_json::json!({"work_id": work_id, "action": "cancel"}),
    );
    assert!(cancel.ok, "stderr: {}", cancel.stderr);
    match dispatch_rx.try_recv() {
        Ok(CentralPluginMessage::Control(control)) => {
            assert_eq!(control.action, "cancelled");
            assert_eq!(control.work_id, work_id);
            assert_eq!(control.session_id, "s0");
        }
        other => panic!("应收到 Control 消息，实际 {other:?}"),
    }
    // 不存在的 work_id 拒绝
    let ghost = call_tool(
        &plugin,
        "control",
        serde_json::json!({"work_id": "work-none", "action": "pause"}),
    );
    assert!(!ghost.ok, "不存在的 work 必须拒绝");
    // 空 work_id 拒绝
    let empty = call_tool(
        &plugin,
        "control",
        serde_json::json!({"work_id": "  ", "action": "resume"}),
    );
    assert!(!empty.ok, "空 work_id 必须拒绝");
}

/// Workspace 授权只在宿主执行（2026-11-25 修复）：插件不再按落盘的长期授权
/// 预检——用户点「授权」（仅此一次）签发的票据在宿主内存，插件看不到，预检
/// 会让授权后的重派在插件层被短路（票据永远消费不到）。未确认目录（新派发
/// 与复用会话两条路径）都必须原样交给宿主裁决（宿主拒绝时自动发出授权卡片）。
#[test]
fn dispatch_leaves_workspace_authorization_to_host() {
    let root = tempfile::tempdir().unwrap();
    // 预置会话 s0（cwd 指向未确认目录，供复用路径验证）
    let pending = tempfile::tempdir().unwrap();
    let pending_path = pending.path().to_string_lossy().to_string();
    let manager = manager_with_session_cwds(root.path(), &[&pending_path]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let extra = tempfile::tempdir().unwrap();
    let extra_path = extra.path().to_string_lossy().to_string();
    // 默认工作区设为另一目录：extra 是「未确认的新工作区」
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    assert!(
        !workspace_confirmed(plugin.sessions.storage_root(), "/tmp/ws", &extra_path),
        "前置：extra 未授权"
    );
    // 新派发：不在插件层拒绝，派发请求原样交给宿主
    let handed_off = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "扫描依赖并升级", "cwd": extra_path}),
    );
    assert!(
        handed_off.ok,
        "未确认目录由宿主裁决，插件不得短路：{}",
        handed_off.stderr
    );
    assert!(!handed_off.stderr.contains("尚未经用户授权"));
    match dispatch_rx.try_recv() {
        Ok(CentralPluginMessage::Dispatch(request)) => {
            assert_eq!(request.cwd.as_deref(), Some(extra_path.as_str()));
            assert_eq!(request.task, "扫描依赖并升级");
        }
        _ => panic!("应发出 Dispatch 消息交宿主裁决"),
    }
    // 复用路径同样交给宿主（会话 cwd 未确认）
    let reuse = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "扫描依赖并升级（复用）", "session_id": "s0"}),
    );
    assert!(
        reuse.ok,
        "复用未确认 cwd 的会话由宿主裁决：{}",
        reuse.stderr
    );
    match dispatch_rx.try_recv() {
        Ok(CentralPluginMessage::Dispatch(request)) => assert_eq!(request.session_id, "s0"),
        _ => panic!("应发出 Dispatch 消息交宿主裁决"),
    }
    // 插件不自行发起授权卡片：卡片由宿主在拒绝时自动发出
    assert!(
        dispatch_rx.try_recv().is_err(),
        "不应有 RequestWorkspaceApproval 等其他消息"
    );
}

/// 相似任务强制复用：同前缀第二次（不带 session_id/【全新】）拒绝，
/// 带 session_id 与【全新】标记放行。
#[test]
fn dispatch_similarity_hint_excludes_self() {
    let root = tempfile::tempdir().unwrap();
    // 预置会话 s0（cwd=默认工作区）：复用链路的真实目标
    let manager = manager_with_session_cwds(root.path(), &["/tmp/ws"]);
    std::fs::write(
        root.path().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    // 首次派发（复用 s0）：无相似历史，不得自匹配拒绝
    let first = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "Review 天工仓库的分支开发情况", "session_id": "s0"}),
    );
    assert!(first.ok, "stderr: {}", first.stderr);
    assert!(
        !first.stderr.contains("高度相似"),
        "首次派发不得自匹配：{}",
        first.stderr
    );
    // 强制复用：同前缀任务且未带 session_id、未标【全新】时必须拒绝，
    // 且拒绝信息必须指向 s0（从登记取真相源，不做纯文本解析）
    let second = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "Review 天工仓库的分支开发情况（增量）"}),
    );
    assert!(
        !second.ok,
        "同前缀且未带 session_id/【全新】的派发必须被拒绝：{}",
        second.stderr
    );
    assert!(
        second.stderr.contains("高度相似"),
        "拒绝信息必须说明相似来源：{}",
        second.stderr
    );
    assert!(
        second.stderr.contains("session_id=s0"),
        "拒绝信息必须指明带 session_id=s0 重发：{}",
        second.stderr
    );
    // 带 session_id 重发同前缀任务：放行（无条件执行，不可静默跳过）
    let third = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "Review 天工仓库的分支开发情况（增量）", "session_id": "s0"}),
    );
    assert!(third.ok, "stderr: {}", third.stderr);
    assert!(
        !third.stderr.contains("高度相似"),
        "显式指定 session_id 不得被拒绝：{}",
        third.stderr
    );
    // 【全新】标记：跳过相似拒绝，允许无关的全新工作
    let fourth = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "【全新】Review 天工仓库的分支开发情况"}),
    );
    assert!(fourth.ok, "stderr: {}", fourth.stderr);
    assert!(
        !fourth.stderr.contains("高度相似"),
        "【全新】标记必须放行：{}",
        fourth.stderr
    );
}

// ============ 覆盖率补充批（2026-11-16）：control/work/notif/context ============

/// control 三动作发出 Control 消息；未知动作与未登记 work_id 拒绝。
#[test]
fn control_actions_send_messages_and_validate() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    std::fs::write(
        root.path().join("sessions/s0.json"),
        serde_json::json!({
            "id": "s0", "title": "w", "created_at": "2026-11-10 10:00:00",
            "updated_at": "2026-11-10 10:00:00", "trust_mode": "Supervised",
            "cwd": "/tmp", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    let _ = add_confirmed_workspace(plugin.sessions.storage_root(), std::path::Path::new("/tmp"));
    // 先经派发落一条登记
    let dispatched = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "被控工作", "session_id": "s0"}),
    );
    assert!(dispatched.ok, "stderr: {}", dispatched.stderr);
    let _ = dispatch_rx.try_recv();
    // 三动作对未登记 work_id 一律拒绝
    for action in ["cancel", "pause", "resume"] {
        let ok = call_tool(
            &plugin,
            "control",
            serde_json::json!({"action": action, "work_id": "work-ctrl"}),
        );
        assert!(!ok.ok, "{action} 对未登记 work_id 应拒绝");
    }
    // 从登记表取真实 work_id
    let items = load_work_items(plugin.sessions.storage_root());
    let real = items.last().unwrap().work_id.clone();
    for (action, verb) in [("cancel", "取消"), ("pause", "暂停"), ("resume", "恢复")] {
        let ok = call_tool(
            &plugin,
            "control",
            serde_json::json!({"action": action, "work_id": real}),
        );
        assert!(ok.ok, "{action} stderr: {}", ok.stderr);
        assert!(ok.stdout.contains(verb), "{}", ok.stdout);
        match dispatch_rx.try_recv().expect("应有 Control 消息") {
            CentralPluginMessage::Control(req) => {
                assert_eq!(req.work_id, real);
            }
            other => panic!("应收到 Control，实际 {other:?}"),
        }
    }
    // 未知 action 拒绝；空 work_id 拒绝
    let bad = call_tool(
        &plugin,
        "control",
        serde_json::json!({"action": "explode", "work_id": real}),
    );
    assert!(!bad.ok);
    let empty = call_tool(&plugin, "control", serde_json::json!({"action": "cancel"}));
    assert!(!empty.ok);
}

/// work 四 action 分发与参数校验；status 读登记 + 事件时间线。
#[test]
fn work_actions_dispatch_and_report_status() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    std::fs::write(
        root.path().join("sessions/s0.json"),
        serde_json::json!({
            "id": "s0", "title": "w", "created_at": "2026-11-10 10:00:00",
            "updated_at": "2026-11-10 10:00:00", "trust_mode": "Supervised",
            "cwd": "/tmp", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    let _ = add_confirmed_workspace(plugin.sessions.storage_root(), std::path::Path::new("/tmp"));
    let dispatched = call_tool(
        &plugin,
        "dispatch_work",
        serde_json::json!({"task": "查询目标工作", "session_id": "s0"}),
    );
    assert!(dispatched.ok, "stderr: {}", dispatched.stderr);
    let _ = dispatch_rx.try_recv();
    let items = load_work_items(plugin.sessions.storage_root());
    let real = items.last().unwrap().work_id.clone();
    // action=list 与缺省一致
    let listed = call_tool(&plugin, "work", serde_json::json!({"action": "list"}));
    assert!(listed.ok);
    assert!(listed.stdout.contains(real.as_str()));
    let default = call_tool(&plugin, "work", serde_json::json!({}));
    assert!(default.ok);
    assert!(default.stdout.contains(real.as_str()));
    // status：登记 + 时间线（派发事件已入日志）
    let status = call_tool(
        &plugin,
        "work",
        serde_json::json!({"action": "status", "work_id": real}),
    );
    assert!(status.ok, "stderr: {}", status.stderr);
    assert!(status.stdout.contains(real.as_str()));
    assert!(status.stdout.contains("查询目标工作"));
    // 插件层测试宿主不在场：事件日志无记录 → 走「无事件记录」分支
    //（宿主写入路径由 tiangong-app host_* 测试覆盖）
    assert!(status.stdout.contains("无事件记录"), "{}", status.stdout);
    // 未知 work_id / 空 work_id / 未知 action 拒绝
    let ghost = call_tool(
        &plugin,
        "work",
        serde_json::json!({"action": "status", "work_id": "work-none"}),
    );
    assert!(!ghost.ok);
    for action in ["status", "timeline"] {
        let missing = call_tool(&plugin, "work", serde_json::json!({"action": action}));
        assert!(!missing.ok, "{action} 缺 work_id 应拒绝");
    }
    let bad = call_tool(&plugin, "work", serde_json::json!({"action": "explode"}));
    assert!(!bad.ok);
    // timeline：无事件但登记存在（手工落盘一条无事件登记）
    let legacy = central_state::WorkItemRecord {
        work_id: "work-legacy".to_string(),
        reused_session: false,
        session_id: "s-legacy".to_string(),
        task: "旧版本登记".to_string(),
        model: None,
        dispatched_at: "2026-11-01 10:00:00".to_string(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: None,
    };
    let _ = upsert_work_item(plugin.sessions.storage_root(), &legacy);
    let legacy_tl = call_tool(
        &plugin,
        "work",
        serde_json::json!({"action": "timeline", "work_id": "work-legacy"}),
    );
    assert!(legacy_tl.ok, "stderr: {}", legacy_tl.stderr);
    assert!(
        legacy_tl.stdout.contains("暂无事件记录"),
        "{}",
        legacy_tl.stdout
    );
}

/// send_notification：空标题拒绝、level 归一、通道投递。
#[test]
fn send_notification_validates_level_and_delivers() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, mut dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let empty = call_tool(
        &plugin,
        "send_notification",
        serde_json::json!({"title": "  ", "message": "x"}),
    );
    assert!(!empty.ok);
    // 非法 level 归一为 info
    let ok = call_tool(
        &plugin,
        "send_notification",
        serde_json::json!({"title": "构建失败", "message": "详情", "level": "loud"}),
    );
    assert!(ok.ok, "stderr: {}", ok.stderr);
    assert!(ok.stdout.contains("info"), "{}", ok.stdout);
    match dispatch_rx.try_recv().expect("应有 Notification") {
        CentralPluginMessage::Notification { title, level, .. } => {
            assert_eq!(title, "构建失败");
            assert_eq!(level, "info");
        }
        other => panic!("应收到 Notification，实际 {other:?}"),
    }
    // 合法 warning level 原样传递
    let warn = call_tool(
        &plugin,
        "send_notification",
        serde_json::json!({"title": "风险", "message": "m", "level": "warning"}),
    );
    assert!(warn.ok);
    match dispatch_rx.try_recv().expect("第二条 Notification") {
        CentralPluginMessage::Notification { level, .. } => assert_eq!(level, "warning"),
        other => panic!("应收到 Notification，实际 {other:?}"),
    }
}

/// inspect_context 三 focus + 缺省综合摘要。
#[test]
fn inspect_context_focus_matrix() {
    let root = tempfile::tempdir().unwrap();
    let plain = root.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let manager = manager_with_session_cwds(root.path(), &[plain.to_str().unwrap()]);
    let (plugin, _dispatch_rx, _reports) =
        plugin_with_channel(plain.to_string_lossy().to_string(), manager);
    std::fs::write(
        root.path().join("sessions/s-a.json"),
        serde_json::json!({
            "id": "s-a", "title": "标题A会话", "created_at": "2026-11-10 10:00:00",
            "updated_at": "2026-11-10 11:00:00", "trust_mode": "Supervised",
            "cwd": plain.to_string_lossy(), "messages": []
        })
        .to_string(),
    )
    .unwrap();
    // 缺省综合摘要：工作区 + 会话都在
    let summary = call_tool(&plugin, "inspect_context", serde_json::json!({}));
    assert!(summary.ok, "stderr: {}", summary.stderr);
    assert!(summary.stdout.contains("工作区"), "{}", summary.stdout);
    assert!(summary.stdout.contains("s-a"), "{}", summary.stdout);
    assert!(summary.stdout.contains("标题A会话"), "{}", summary.stdout);
    // focus=workspaces / sessions / workspace（默认目录）
    for focus in ["workspaces", "sessions"] {
        let r = call_tool(
            &plugin,
            "inspect_context",
            serde_json::json!({"focus": focus}),
        );
        assert!(r.ok, "{focus} stderr: {}", r.stderr);
    }
    let ws = call_tool(
        &plugin,
        "inspect_context",
        serde_json::json!({"focus": "workspace"}),
    );
    assert!(ws.ok, "stderr: {}", ws.stderr);
    assert!(ws.stdout.contains("非 git 仓库"), "{}", ws.stdout);
    // focus=workspace 缺 path 且默认目录无效 → 拒绝
    let bad = call_tool(
        &plugin,
        "inspect_context",
        serde_json::json!({"focus": "workspace", "path": "/nonexistent-dir-xyz"}),
    );
    assert!(!bad.ok);
}

/// list_sessions：倒序 + 消息数 + 空态。
#[test]
fn list_sessions_sorted_with_message_counts() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    // 空态
    let empty = call_tool(
        &plugin,
        "inspect_context",
        serde_json::json!({"focus": "sessions"}),
    );
    assert!(empty.ok);
    assert!(empty.stdout.contains("没有任何会话"), "{}", empty.stdout);
    // 两个会话，更新时间不同 + message_count 不同
    std::fs::write(
        root.path().join("sessions/older.json"),
        serde_json::json!({
            "id": "older", "title": "旧会话", "created_at": "2026-11-09 10:00:00",
            "updated_at": "2026-11-09 10:00:00", "trust_mode": "Supervised",
            "cwd": "/tmp", "messages": [{"role":"user","content":"a","created_at":"2026-11-09 10:00:00"}]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.path().join("sessions/newer.json"),
        serde_json::json!({
            "id": "newer", "title": "新会话", "created_at": "2026-11-10 10:00:00",
            "updated_at": "2026-11-10 12:00:00", "trust_mode": "Supervised",
            "cwd": "/tmp", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    let listed = call_tool(
        &plugin,
        "inspect_context",
        serde_json::json!({"focus": "sessions"}),
    );
    assert!(listed.ok);
    let older_pos = listed.stdout.find("older").expect("应含 older");
    let newer_pos = listed.stdout.find("newer").expect("应含 newer");
    assert!(newer_pos < older_pos, "按更新时间倒序：{}", listed.stdout);
    assert!(listed.stdout.contains("1 条消息"), "{}", listed.stdout);
}

/// goal 边界：未知 goal_id、非法 status、save_report 缺参。
#[test]
fn goal_validates_unknown_goal_and_bad_status() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager_with_session_cwds(root.path(), &[]);
    let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
    let unknown = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "set_goal_status", "goal_id": "goal-none", "status": "achieved"}),
    );
    assert!(!unknown.ok);
    let created = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "save_goal", "title": "边界目标"}),
    );
    assert!(created.ok, "stderr: {}", created.stderr);
    let goal_id = created
        .stdout
        .split("（")
        .nth(1)
        .and_then(|rest| rest.split('）').next())
        .map(str::to_string)
        .expect("应能从输出提取 goal_id");
    let bad_status = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "set_goal_status", "goal_id": goal_id, "status": "maybe"}),
    );
    assert!(!bad_status.ok, "非法 status 应拒绝：{}", bad_status.stdout);
    let ok_status = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "set_goal_status", "goal_id": goal_id, "status": "achieved"}),
    );
    assert!(ok_status.ok, "stderr: {}", ok_status.stderr);
    // save_report 缺 work_id / summary 拒绝
    let no_work = call_tool(
        &plugin,
        "goal",
        serde_json::json!({"action": "save_report", "summary": "s"}),
    );
    assert!(!no_work.ok);
}
