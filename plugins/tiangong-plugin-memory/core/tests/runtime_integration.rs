use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;
use tiangong_llm::{LlmEndpointConfig, ProviderProtocol};
use tiangong_memory::{
    EnhancedTurnResult, Episode, EpisodeOutcome, MemoryCandidate, MemoryKind, MemoryOptions,
    MemoryRecallRequest, MemoryRelationDraft, MemoryRelationKind, MemoryVectorMode, RecallAnchors,
    RecallDepth, TurnArtifact, TurnArtifactKind, TurnResult, load_injection_sync,
    start_with_options, workspace_id_from_path,
};

struct EnvGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_userprofile: Option<std::ffi::OsString>,
    prev_cwd: PathBuf,
}

impl EnvGuard {
    fn enter(home: &Path, cwd: &Path) -> Self {
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_cwd = std::env::current_dir().expect("读取当前工作目录失败");

        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("USERPROFILE", home);
        }
        std::env::set_current_dir(cwd).expect("切换当前工作目录失败");

        Self {
            prev_home,
            prev_userprofile,
            prev_cwd,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev_cwd);
        unsafe {
            match &self.prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("创建目录失败");
    }
    fs::write(path, content).expect("写入文件失败");
}

fn setup_workspace() -> (TempDir, TempDir, PathBuf, String) {
    let home = TempDir::new().expect("创建 fake home 失败");
    let workspace = TempDir::new().expect("创建 workspace 失败");
    let workspace_path = workspace.path().to_path_buf();
    let workspace_id = workspace_id_from_path(&workspace_path);
    (home, workspace, workspace_path, workspace_id)
}

struct MockMemoryLlmServer {
    base_url: String,
    shutdown_tx: Option<mpsc::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl MockMemoryLlmServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定 mock Memory LLM 失败");
        let addr = listener
            .local_addr()
            .expect("读取 mock Memory LLM 地址失败");
        listener
            .set_nonblocking(true)
            .expect("设置 mock Memory LLM 非阻塞失败");
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        // per-request 多线程：每个连接独立处理，避免 Deep Recall 的多轮
                        // LLM 调用因串行排队而超时。Mock 响应是纯计算（无 IO），线程开销可忽略。
                        std::thread::spawn(move || handle_mock_request(stream));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => {
                        // accept 错误不终止服务器：瞬时 socket 错误不应让后续 LLM 调用全部失败
                        eprintln!("[mock-llm] accept 错误，继续轮询: {err}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            shutdown_tx: Some(shutdown_tx),
            join: Some(join),
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

/// 处理单个 Mock LLM 请求（在独立线程中运行）。
fn start_slow_memory_llm(delay: Duration) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定慢速 mock Memory LLM 失败");
    let addr = listener.local_addr().expect("读取慢速 mock 地址失败");
    let join = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("等待慢速 mock 请求失败");
        handle_mock_request_with_delay(&mut stream, delay);
    });
    (format!("http://{addr}/v1"), join)
}
fn handle_mock_request(mut stream: std::net::TcpStream) {
    handle_mock_request_with_delay(&mut stream, Duration::ZERO);
}
fn handle_mock_request_with_delay(stream: &mut std::net::TcpStream, delay: Duration) {
    let request = read_http_request(stream);
    if !delay.is_zero() {
        std::thread::sleep(delay);
    }
    let body = request
        .as_deref()
        .map(memory_llm_mock_response)
        .unwrap_or_else(|| serde_json::json!({"error": "bad request"}));
    let payload = body.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    let _ = stream.write_all(response.as_bytes());
}

impl Drop for MockMemoryLlmServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buffer.len() < header_end + content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(buffer[header_end..header_end + content_length].to_vec()).ok()
}

fn memory_llm_mock_response(request_body: &str) -> serde_json::Value {
    let request = serde_json::from_str::<serde_json::Value>(request_body)
        .unwrap_or_else(|_| serde_json::json!({}));
    let (system, prompt) = openai_messages_text(&request);
    let content = if system.contains("MesoRumination") {
        mock_meso_response(&prompt)
    } else if system.contains("检索锚点规划器") {
        mock_recall_anchor_response(&prompt)
    } else if system.contains("深度回忆裁决器") {
        serde_json::json!({
            "need_deep_recall": true,
            "reason": "需要跨会话追溯产物、Entity 和 Decision",
            "followup_queries": [
                "nebula artifact export",
                "nebula-billing-provider.rs",
                "event ledger queue decision"
            ],
            "target_kinds": ["episode", "entity", "decision"],
            "max_rounds": 2
        })
        .to_string()
    } else if system.contains("结果整理器") {
        mock_recall_synthesis_response(&prompt)
    } else {
        serde_json::json!({"marker": "memory_llm_smoke_ok", "summary": "mock ok"}).to_string()
    };
    serde_json::json!({
        "id": "chatcmpl-memory-mock",
        "object": "chat.completion",
        "model": "memory-deep-recall-mock",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150
        }
    })
}

fn openai_messages_text(request: &serde_json::Value) -> (String, String) {
    let mut system = String::new();
    let mut user = String::new();
    for message in request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = message
            .get("role")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let content =
            message_content_text(message.get("content").unwrap_or(&serde_json::Value::Null));
        match role {
            "system" => system.push_str(&content),
            "user" => user.push_str(&content),
            _ => {}
        }
    }
    (system, user)
}

fn message_content_text(content: &serde_json::Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn mock_meso_response(prompt: &str) -> String {
    let module_episode_id =
        episode_id_for_title(prompt, "nebula billing provider module").unwrap_or_default();
    let decision_episode_id =
        episode_id_for_title(prompt, "choose event ledger queue for nebula").unwrap_or_default();
    serde_json::json!({
        "entities": [{
            "name": "nebula-billing-provider.rs",
            "entity_type": "module",
            "description": "Nebula billing provider module keeps handoff rules in /workspace/src/nebula/billing_provider.rs",
            "file_path": "/workspace/src/nebula/billing_provider.rs",
            "related_episode_ids": [module_episode_id],
            "importance": 0.82
        }],
        "decisions": [{
            "title": "event ledger queue for nebula release",
            "context": "Nebula release chose event ledger queue instead of direct webhook fanout.",
            "alternatives": ["direct webhook fanout", "event ledger queue"],
            "chosen": "event ledger queue",
            "reasons": ["preserve retry trace", "support idempotency"],
            "episode_ids": [decision_episode_id]
        }]
    })
    .to_string()
}

fn episode_id_for_title(prompt: &str, title_fragment: &str) -> Option<String> {
    let mut last_id = None;
    for line in prompt.lines() {
        if let Some(id) = line.trim().strip_prefix("- id=") {
            last_id = Some(id.trim().to_string());
            continue;
        }
        if line.trim().starts_with("title=") && line.contains(title_fragment) {
            return last_id;
        }
    }
    None
}

fn mock_recall_anchor_response(prompt: &str) -> String {
    let query = if prompt.contains("deep recall:") && prompt.contains("artifact") {
        "nebula artifact export"
    } else if prompt.contains("deep recall:") && prompt.contains("nebula-billing-provider.rs") {
        "nebula-billing-provider.rs"
    } else if prompt.contains("deep recall:") && prompt.contains("event ledger") {
        "event ledger queue decision"
    } else {
        "nebula release overview"
    };
    serde_json::json!({
        "query": query,
        "keywords": ["nebula", "release", "artifact", "decision"],
        "strategy": "keyword",
        "limit": 3
    })
    .to_string()
}

fn mock_recall_synthesis_response(prompt: &str) -> String {
    let mut lines = Vec::new();
    if prompt.contains("https://example.invalid/artifacts/nebula-release-plan.svg") {
        lines.push(
            "产物: https://example.invalid/artifacts/nebula-release-plan.svg /tmp/tiangong/nebula-release-plan.svg",
        );
    }
    if prompt.contains("/workspace/src/nebula/billing_provider.rs") {
        lines.push("Entity: nebula-billing-provider.rs /workspace/src/nebula/billing_provider.rs");
    }
    if prompt.contains("event ledger queue") {
        lines.push("Decision: event ledger queue rather than direct webhook fanout");
    }
    if lines.is_empty() {
        "没有发现当前上下文之外的增量记忆。".to_string()
    } else {
        lines.join("\n")
    }
}

async fn wait_for_recall_hit(
    handle: &tiangong_memory::MemoryHandle,
    query: &str,
) -> Vec<tiangong_memory::RecallHit> {
    for _ in 0..20 {
        let hits = handle
            .recall(
                RecallAnchors {
                    query: query.to_string(),
                    keywords: Vec::new(),
                    strategy: None,
                },
                5,
            )
            .await;
        if !hits.is_empty() {
            return hits;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Vec::new()
}

async fn wait_for_recall_title_absent(
    handle: &tiangong_memory::MemoryHandle,
    query: &str,
    title: &str,
) -> bool {
    for _ in 0..30 {
        let hits = handle
            .recall(
                RecallAnchors {
                    query: query.to_string(),
                    keywords: Vec::new(),
                    strategy: None,
                },
                5,
            )
            .await;
        if !hits.iter().any(|hit| hit.title == title) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn wait_for_recall_kind(
    handle: &tiangong_memory::MemoryHandle,
    query: &str,
    kind: MemoryKind,
) -> Vec<tiangong_memory::RecallHit> {
    for attempt in 1..=30 {
        let hits = handle
            .recall(
                RecallAnchors {
                    query: query.to_string(),
                    keywords: Vec::new(),
                    strategy: None,
                },
                8,
            )
            .await;
        eprintln!(
            "[wait-kind] attempt={attempt} query={query:?} kind={kind:?} hits={}",
            hits.len()
        );
        if hits.iter().any(|hit| hit.kind == kind) {
            return hits;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Vec::new()
}

async fn wait_for_unique_kind_hits(
    handle: &tiangong_memory::MemoryHandle,
    query: &str,
    kind: MemoryKind,
) -> Vec<tiangong_memory::RecallHit> {
    for attempt in 1..=30 {
        let hits = handle
            .recall(
                RecallAnchors {
                    query: query.to_string(),
                    keywords: Vec::new(),
                    strategy: None,
                },
                8,
            )
            .await;
        let kind_hits = hits
            .iter()
            .filter(|hit| hit.kind == kind)
            .collect::<Vec<_>>();
        let unique_ids = kind_hits
            .iter()
            .map(|hit| hit.node_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        eprintln!(
            "[wait-unique-kind] attempt={attempt} query={query:?} kind={kind:?} kind_hits={} unique_ids={}",
            kind_hits.len(),
            unique_ids.len()
        );
        if !kind_hits.is_empty() && kind_hits.len() == unique_ids.len() {
            return hits;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Vec::new()
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn enhanced_rumination_enqueue_does_not_block_injection_reads() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _guard = EnvGuard::enter(home.path(), &workspace_path);
    let (base_url, mock_join) = start_slow_memory_llm(Duration::from_secs(2));
    let mut model = LlmEndpointConfig::new("test-key", &base_url, "memory-test");
    model.protocol = ProviderProtocol::OpenAiChatCompletions;
    model.timeout = Duration::from_secs(5);
    model.max_retries = 0;
    let handle =
        start_with_options(MemoryOptions::new().with_model(model)).expect("启动 Memory 失败");
    let turn_result = EnhancedTurnResult {
        session_id: "rumination-queue-session".to_string(),
        turn_id: "rumination-queue-turn".to_string(),
        had_tool_calls: true,
        user_input: "记录后台反刍队列优化".to_string(),
        summary: "已将耗时提取移出 Memory Actor".to_string(),
        workspace_id: Some(workspace_id.clone()),
        memory_candidates: vec![MemoryCandidate {
            tool_name: "coding".to_string(),
            step_index: 0,
            hint: "反刍队列优化".to_string(),
            suggested_kinds: Vec::new(),
            file_path: None,
            url: None,
            result_summary: Some("完成反刍队列优化".to_string()),
            success: true,
            tool_source: Some("file_operation".to_string()),
            is_recalled_context: false,
        }],
        ..Default::default()
    };

    let enqueue_started = std::time::Instant::now();
    handle
        .run_enhanced_micro_rumination(turn_result)
        .await
        .expect("增强反刍应成功入队");
    assert!(
        enqueue_started.elapsed() < Duration::from_secs(1),
        "增强反刍接口只能等待入队，不能等待模型提取"
    );

    let injection_started = std::time::Instant::now();
    let _ = handle
        .load_injection("rumination-queue-session", Some(&workspace_id))
        .await;
    assert!(
        injection_started.elapsed() < Duration::from_secs(1),
        "后台反刍执行期间 LoadInjection 不应被模型调用阻塞"
    );
    handle.shutdown().await;
    mock_join.join().expect("慢速 mock Memory LLM 不应异常退出");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn runtime_loads_profile_workspace_and_session_injections() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    write_file(
        &home.path().join(".tiangong/memory/profile/agent.md"),
        "profile-memory",
    );
    write_file(
        &home.path().join(format!(
            ".tiangong/memory/workspaces/{workspace_id}/agent.md"
        )),
        "workspace-memory",
    );
    write_file(
        &home
            .path()
            .join(".tiangong/memory/sessions/session-a/agent.md"),
        "session-memory",
    );

    let handle =
        start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
            .expect("启动 memory 失败");
    let loaded = handle
        .load_injection("session-a", Some(&workspace_id))
        .await;

    assert_eq!(
        loaded,
        vec![
            "profile-memory".to_string(),
            "workspace-memory".to_string(),
            "session-memory".to_string(),
        ]
    );

    let sync_loaded = load_injection_sync("session-a", Some(&workspace_id));
    assert_eq!(loaded, sync_loaded);

    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn runtime_can_write_episode_and_recall_without_core() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    let handle =
        start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
            .expect("启动 memory 失败");
    handle.write_episode(
        Episode::new(
            "session-b".to_string(),
            "fix login timeout".to_string(),
            "fix login timeout and retry flow".to_string(),
            EpisodeOutcome::Success,
            vec![
                "login".to_string(),
                "timeout".to_string(),
                "retry".to_string(),
            ],
            vec!["http_request".to_string()],
            0.8,
        ),
        Some(workspace_id.clone()),
    );

    let hits = wait_for_recall_hit(&handle, "login timeout").await;
    assert!(
        !hits.is_empty(),
        "写入 Episode 后应能通过 recall 命中 Tantivy 索引"
    );
    assert!(
        hits.iter().any(|hit| {
            hit.title.contains("login timeout") || hit.summary.contains("login timeout")
        }),
        "命中结果应包含已写入 Episode 的标题或摘要"
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn meta_rumination_archives_missing_paths_expired_urls_and_project_archived_markers() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    let alive_path = workspace_path.join("docs/alive-meta-reference.md");
    write_file(&alive_path, "alive reference");
    let missing_path = workspace_path.join("artifacts/missing-meta-reference.png");

    let handle =
        start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
            .expect("启动 memory 失败");
    let cases = [
        (
            "meta alive file alpha",
            format!(
                "meta alive file alpha keeps valid local reference {}",
                alive_path.display()
            ),
            vec!["meta-alive-alpha".to_string()],
        ),
        (
            "meta missing path beta",
            format!(
                "meta missing path beta points to removed artifact {}",
                missing_path.display()
            ),
            vec!["meta-missing-beta".to_string()],
        ),
        (
            "meta expired url gamma",
            "meta expired url gamma references https://example.invalid/expired/media.png"
                .to_string(),
            vec!["meta-expired-gamma".to_string()],
        ),
        (
            "meta archived workspace delta",
            "meta archived workspace delta belongs to project archived workspace".to_string(),
            vec!["meta-archived-delta".to_string()],
        ),
    ];

    for (title, summary, keywords) in cases {
        handle.write_episode(
            Episode::new(
                "session-meta-lifecycle".to_string(),
                title.to_string(),
                summary,
                EpisodeOutcome::Success,
                keywords,
                vec!["meta_rumination".to_string()],
                0.8,
            ),
            Some(workspace_id.clone()),
        );
    }

    for title in [
        "meta alive file alpha",
        "meta missing path beta",
        "meta expired url gamma",
        "meta archived workspace delta",
    ] {
        let hits = wait_for_recall_hit(&handle, title).await;
        assert!(
            hits.iter().any(|hit| hit.title == title),
            "Meta 执行前应能召回 {title}"
        );
    }

    handle.run_meta_rumination();

    let alive_hits = wait_for_recall_hit(&handle, "meta alive file alpha").await;
    assert!(
        alive_hits
            .iter()
            .any(|hit| hit.title == "meta alive file alpha"),
        "可达本地路径不应被 Meta 归档"
    );
    for title in [
        "meta missing path beta",
        "meta expired url gamma",
        "meta archived workspace delta",
    ] {
        assert!(
            wait_for_recall_title_absent(&handle, title, title).await,
            "Meta 应归档过时引用 {title}"
        );
    }

    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn runtime_can_expand_recalled_episode_with_depth2() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    let handle =
        start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
            .expect("启动 memory 失败");
    handle.write_episode(
        Episode::new(
            "session-depth2".to_string(),
            "depth2 expansion smoke".to_string(),
            "depth2 full content should include the original episode payload".to_string(),
            EpisodeOutcome::Success,
            vec![
                "depth2".to_string(),
                "expansion".to_string(),
                "payload".to_string(),
            ],
            vec!["memory_depth2_tool".to_string()],
            0.85,
        ),
        Some(workspace_id.clone()),
    );

    let hits = wait_for_recall_hit(&handle, "depth2 expansion").await;
    assert!(!hits.is_empty(), "测试前应先召回刚写入的 Episode");

    let expanded = handle
        .load_depth2(vec![hits[0].node_id.clone(), "missing-node".to_string()])
        .await;
    assert_eq!(expanded.len(), 1, "Depth2 应跳过不存在的节点");
    assert_eq!(expanded[0].node_id, hits[0].node_id);
    assert!(
        expanded[0]
            .full_content
            .contains("depth2 full content should include"),
        "Depth2 应返回 Episode 的完整序列化内容"
    );
    assert!(
        expanded[0].full_content.contains("memory_depth2_tool"),
        "Depth2 完整内容应包含 tool_calls 等摘要外信息"
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn micro_rumination_writes_episode_with_explicit_workspace_context() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    let handle =
        start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
            .expect("启动 memory 失败");
    handle.run_micro_rumination(TurnResult {
        session_id: "session-c".to_string(),
        turn_id: "turn-c".to_string(),
        had_tool_calls: true,
        summary: "complete database migration and fix schema mismatch".to_string(),
        workspace_id: Some(workspace_id.clone()),
        ..TurnResult::default()
    });

    let hits = wait_for_recall_hit(&handle, "database migration").await;
    assert!(
        !hits.is_empty(),
        "micro rumination 写入后应能通过 recall 命中 Episode"
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn artifact_only_turn_is_added_to_memory_and_recalled_by_context_tool() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    let image_url = "https://example.invalid/artifacts/architecture-sunrise.png";
    let file_path = "/tmp/tiangong/architecture-sunrise.png";
    let handle =
        start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
            .expect("启动 memory 失败");

    handle.run_micro_rumination(TurnResult {
        session_id: "session-artifact-add".to_string(),
        turn_id: "turn-artifact-only".to_string(),
        had_tool_calls: false,
        user_input: "展示刚生成的 architecture sunrise diagram".to_string(),
        summary: "assistant produced architecture sunrise diagram artifact".to_string(),
        tool_calls: Vec::new(),
        artifacts: vec![
            TurnArtifact {
                kind: TurnArtifactKind::Media,
                tool_name: Some("image_generation".to_string()),
                title: Some("architecture sunrise diagram".to_string()),
                url: Some(image_url.to_string()),
                path: None,
                summary: Some("final generated architecture diagram".to_string()),
            },
            TurnArtifact {
                kind: TurnArtifactKind::File,
                tool_name: Some("write_file".to_string()),
                title: Some("architecture sunrise local file".to_string()),
                url: None,
                path: Some(file_path.to_string()),
                summary: Some("local copy of generated diagram".to_string()),
            },
        ],
        workspace_id: Some(workspace_id.clone()),
    });
    eprintln!("[artifact-add] submitted artifact-only turn url={image_url} path={file_path}");

    let hits = wait_for_recall_hit(&handle, "architecture sunrise diagram").await;
    eprintln!("[artifact-add] hit_count={}", hits.len());
    for (idx, hit) in hits.iter().enumerate() {
        eprintln!(
            "[artifact-add] hit[{idx}] node_id={} score={:.3} title={} summary={}",
            hit.node_id, hit.score, hit.title, hit.summary
        );
    }
    assert!(
        hits.iter()
            .any(|hit| hit.summary.contains("architecture sunrise diagram")),
        "即使没有工具调用，只要 turn 含结构化产物，也应新增 Episode 记忆"
    );

    let expanded = handle
        .load_depth2(hits.iter().map(|hit| hit.node_id.clone()).collect())
        .await;
    let expanded_text = expanded
        .iter()
        .map(|item| item.full_content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    eprintln!(
        "[artifact-add] expanded_contains_url={} expanded_contains_path={} expanded=\n{}",
        expanded_text.contains(image_url),
        expanded_text.contains(file_path),
        compact_for_log(&expanded_text, 900)
    );
    assert!(expanded_text.contains(image_url));
    assert!(expanded_text.contains(file_path));

    let contextual = handle
        .recall_context(MemoryRecallRequest {
            query: "刚才那个 architecture sunrise diagram 的图片和文件路径".to_string(),
            reason: Some("用户用历史指代继续使用刚生成的结构化产物".to_string()),
            expected: vec!["media".to_string(), "file".to_string()],
            context: vec!["当前上下文没有图片 URL，也没有本地文件路径".to_string()],
            limit: 5,
            ..Default::default()
        })
        .await;
    eprintln!(
        "[artifact-add] contextual_hit_count={} used_llm={} content=\n{}",
        contextual.hits.len(),
        contextual.used_llm,
        contextual.content
    );
    assert!(
        contextual.content.contains(image_url),
        "按需回忆应返回结构化媒体 URL"
    );
    assert!(
        contextual.content.contains(file_path),
        "按需回忆应返回结构化文件路径"
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn contextual_recall_returns_incremental_memory_without_repeating_prompt_context() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    let handle =
        start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
            .expect("启动 memory 失败");
    let redundant_summary = "We chose coral teal palette for flamingo dashboard";
    let export_url = "https://example.invalid/artifacts/flamingo-dashboard.svg";
    let export_path = "/tmp/tiangong/flamingo-dashboard.svg";

    handle.write_episode(
        Episode::new(
            "session-incremental-recall".to_string(),
            "flamingo palette decision".to_string(),
            redundant_summary.to_string(),
            EpisodeOutcome::Success,
            vec![
                "flamingo".to_string(),
                "palette".to_string(),
                "dashboard".to_string(),
            ],
            vec!["recall_memory".to_string()],
            0.6,
        ),
        Some(workspace_id.clone()),
    );
    handle.write_episode(
        Episode::new(
            "session-incremental-recall".to_string(),
            "flamingo export artifact".to_string(),
            format!("Exported flamingo dashboard artifact {export_url} {export_path}"),
            EpisodeOutcome::Success,
            vec![
                "flamingo".to_string(),
                "artifact".to_string(),
                "dashboard".to_string(),
            ],
            vec!["generate_image".to_string(), "write_file".to_string()],
            0.8,
        ),
        Some(workspace_id.clone()),
    );
    eprintln!(
        "[incremental-recall] submitted redundant_summary={redundant_summary:?} export_url={export_url} export_path={export_path}"
    );

    let hits = wait_for_recall_hit(&handle, "flamingo dashboard artifact").await;
    eprintln!("[incremental-recall] precheck_hit_count={}", hits.len());
    assert!(
        hits.iter()
            .any(|hit| hit.summary.contains("flamingo dashboard artifact")),
        "测试前应能召回带 URL/路径的增量记忆"
    );

    let contextual = handle
        .recall_context(MemoryRecallRequest {
            query: "继续 flamingo dashboard，需要之前的导出产物".to_string(),
            reason: Some("当前上下文只有设计决策，缺少可继续使用的产物引用".to_string()),
            expected: vec!["file".to_string(), "media".to_string()],
            context: vec![
                "user: 继续 flamingo dashboard".to_string(),
                format!("assistant: {redundant_summary}"),
            ],
            limit: 5,
            ..Default::default()
        })
        .await;
    eprintln!(
        "[incremental-recall] contextual_hit_count={} used_llm={} content=\n{}",
        contextual.hits.len(),
        contextual.used_llm,
        contextual.content
    );

    assert!(
        contextual.content.contains(export_url),
        "回忆结果应保留当前上下文之外的 URL 增量信息"
    );
    assert!(
        contextual.content.contains(export_path),
        "回忆结果应保留当前上下文之外的路径增量信息"
    );
    assert!(
        !contextual.content.contains(redundant_summary),
        "回忆结果不应重复当前上下文已包含的摘要，避免浪费 prompt"
    );
    assert_eq!(
        contextual.content.matches(export_url).count(),
        1,
        "同一 URL 在回忆结果中应只出现一次"
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn contextual_recall_fixed_reference_cases_return_only_incremental_memory() {
    let cases = incremental_recall_cases();

    for case in &cases {
        let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
        let _env = EnvGuard::enter(home.path(), &workspace_path);
        let handle =
            start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
                .expect("启动 memory 失败");

        handle.write_episode(
            Episode::new(
                format!("session-incremental-{}", case.slug),
                format!("{} current context", case.slug),
                case.current_context.to_string(),
                EpisodeOutcome::Success,
                case.keywords(),
                vec!["recall_memory".to_string()],
                0.5,
            ),
            Some(workspace_id.clone()),
        );
        handle.write_episode(
            Episode::new(
                format!("session-incremental-{}", case.slug),
                format!("{} incremental memory", case.slug),
                case.incremental_memory.to_string(),
                EpisodeOutcome::Success,
                case.keywords(),
                case.tool_calls
                    .iter()
                    .map(|item| item.to_string())
                    .collect(),
                0.8,
            ),
            Some(workspace_id.clone()),
        );

        let hits = wait_for_recall_hit(&handle, case.query).await;
        assert!(
            hits.iter()
                .any(|hit| hit.summary.contains(case.expected_refs[0])),
            "固定样例 {} 应先能召回增量记忆",
            case.slug
        );

        let contextual = handle
            .recall_context(MemoryRecallRequest {
                query: case.query.to_string(),
                reason: Some(case.reason.to_string()),
                expected: case.expected.iter().map(|item| item.to_string()).collect(),
                context: vec![
                    format!("user: {}", case.user_context),
                    format!("assistant: {}", case.current_context),
                ],
                limit: 5,
                ..Default::default()
            })
            .await;
        eprintln!(
            "[incremental-case:{}] used_llm={} hit_count={} content=\n{}",
            case.slug,
            contextual.used_llm,
            contextual.hits.len(),
            contextual.content
        );

        assert!(
            !contextual.content.contains(case.current_context),
            "固定样例 {} 不应重复当前上下文已包含的摘要",
            case.slug
        );
        for expected_ref in case.expected_refs {
            assert!(
                contextual.content.contains(expected_ref),
                "固定样例 {} 应返回增量引用 {}",
                case.slug,
                expected_ref
            );
            assert_eq!(
                contextual.content.matches(expected_ref).count(),
                1,
                "固定样例 {} 的增量引用 {} 应只出现一次",
                case.slug,
                expected_ref
            );
        }

        handle.shutdown().await;
    }
}

struct IncrementalRecallCase {
    slug: &'static str,
    query: &'static str,
    user_context: &'static str,
    current_context: &'static str,
    incremental_memory: &'static str,
    expected_refs: &'static [&'static str],
    expected: &'static [&'static str],
    tool_calls: &'static [&'static str],
    reason: &'static str,
}

impl IncrementalRecallCase {
    fn keywords(&self) -> Vec<String> {
        self.query
            .split_whitespace()
            .chain([self.slug])
            .map(str::to_string)
            .collect()
    }
}

fn incremental_recall_cases() -> Vec<IncrementalRecallCase> {
    vec![
        IncrementalRecallCase {
            slug: "flamingo-export",
            query: "继续 flamingo-export dashboard 的导出产物",
            user_context: "继续 flamingo-export dashboard",
            current_context: "flamingo-export 已确定 coral teal palette",
            incremental_memory: "flamingo-export 导出产物位于 https://example.invalid/artifacts/flamingo-export.svg path=/tmp/tiangong/flamingo-export.svg",
            expected_refs: &[
                "https://example.invalid/artifacts/flamingo-export.svg",
                "/tmp/tiangong/flamingo-export.svg",
            ],
            expected: &["media", "file"],
            tool_calls: &["generate_image", "write_file"],
            reason: "用户用历史指代继续使用刚生成的 dashboard 产物",
        },
        IncrementalRecallCase {
            slug: "profile-migration",
            query: "继续 profile-migration 上次那个迁移文件",
            user_context: "继续 profile-migration 上次那个迁移",
            current_context: "profile-migration 决定拆成 schema 和 backfill 两步",
            incremental_memory: "profile-migration 迁移文件保存为 /workspace/migrations/20260506_profile_migration.sql，回滚说明在 /workspace/docs/profile_migration_rollback.md",
            expected_refs: &[
                "/workspace/migrations/20260506_profile_migration.sql",
                "/workspace/docs/profile_migration_rollback.md",
            ],
            expected: &["file"],
            tool_calls: &["write_file"],
            reason: "用户提到上次那个迁移，需要补回当前上下文缺失的文件路径",
        },
        IncrementalRecallCase {
            slug: "raven-poster",
            query: "把 raven-poster 刚才那张图再导出一次",
            user_context: "把 raven-poster 刚才那张图再导出一次",
            current_context: "raven-poster 使用 night garden prompt",
            incremental_memory: "raven-poster 图片地址是 https://example.invalid/media/raven-poster.png，本地副本 path=/tmp/tiangong/raven-poster.png",
            expected_refs: &[
                "https://example.invalid/media/raven-poster.png",
                "/tmp/tiangong/raven-poster.png",
            ],
            expected: &["media", "file"],
            tool_calls: &["generate_image"],
            reason: "用户用刚才那张图指代历史图片产物",
        },
        IncrementalRecallCase {
            slug: "mcp-config",
            query: "沿用 mcp-config 之前那个服务器配置",
            user_context: "沿用 mcp-config 之前那个服务器配置",
            current_context: "mcp-config 选择 stdio transport",
            incremental_memory: "mcp-config 配置文件在 /workspace/.tiangong/mcp/linear-server.json，endpoint 说明见 /workspace/docs/mcp-linear.md",
            expected_refs: &[
                "/workspace/.tiangong/mcp/linear-server.json",
                "/workspace/docs/mcp-linear.md",
            ],
            expected: &["file"],
            tool_calls: &["write_file"],
            reason: "用户提到之前那个服务器配置，需要返回可继续使用的配置文件",
        },
        IncrementalRecallCase {
            slug: "latency-trace",
            query: "继续 latency-trace 上次的性能排查",
            user_context: "继续 latency-trace 上次的性能排查",
            current_context: "latency-trace 判定瓶颈在 recall rerank 阶段",
            incremental_memory: "latency-trace profile 数据位于 /tmp/tiangong/latency-trace.json，火焰图为 https://example.invalid/perf/latency-trace.html",
            expected_refs: &[
                "/tmp/tiangong/latency-trace.json",
                "https://example.invalid/perf/latency-trace.html",
            ],
            expected: &["file"],
            tool_calls: &["run_command", "write_file"],
            reason: "用户继续性能排查，需要补回 profile 和火焰图引用",
        },
        IncrementalRecallCase {
            slug: "skill-template",
            query: "用 skill-template 上次那个技能模板继续生成",
            user_context: "用 skill-template 上次那个技能模板继续生成",
            current_context: "skill-template 采用 plan first 的写法",
            incremental_memory: "skill-template 模板文件保存为 /workspace/.tiangong/skills/memory-review/SKILL.md，示例输入在 /workspace/docs/skill-template-example.md",
            expected_refs: &[
                "/workspace/.tiangong/skills/memory-review/SKILL.md",
                "/workspace/docs/skill-template-example.md",
            ],
            expected: &["file"],
            tool_calls: &["write_file"],
            reason: "用户用上次那个技能模板指代历史文件产物",
        },
    ]
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn meso_rumination_extracts_entity_and_decision_memories() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    let handle =
        start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
            .expect("启动 memory 失败");
    handle.write_episode(
        Episode::new(
            "session-meso".to_string(),
            "vector backend evaluation".to_string(),
            "Compared different vector index backends for memory recall.".to_string(),
            EpisodeOutcome::Success,
            vec![
                "vector".to_string(),
                "vector".to_string(),
                "memory".to_string(),
            ],
            vec!["cargo_test".to_string()],
            0.7,
        ),
        Some(workspace_id.clone()),
    );
    handle.write_episode(
        Episode::new(
            "session-meso".to_string(),
            "choose embedded vector index for memory".to_string(),
            "We decided to choose embedded vector index to reduce user startup complexity."
                .to_string(),
            EpisodeOutcome::Success,
            vec![
                "vector".to_string(),
                "vector".to_string(),
                "memory".to_string(),
                "embedded".to_string(),
            ],
            vec!["architecture_decision".to_string()],
            0.85,
        ),
        Some(workspace_id.clone()),
    );
    eprintln!("[meso] submitted source episodes workspace_id={workspace_id}");

    handle.run_meso_rumination("session-meso".to_string(), workspace_id.clone());

    let entity_hits =
        wait_for_recall_kind(&handle, "vector memory entity", MemoryKind::Entity).await;
    for (idx, hit) in entity_hits.iter().enumerate() {
        eprintln!(
            "[meso] entity_hit[{idx}] kind={:?} title={} summary={}",
            hit.kind, hit.title, hit.summary
        );
    }
    assert!(
        entity_hits.iter().any(|hit| hit.kind == MemoryKind::Entity),
        "Meso 反刍应从近期 Episode 提炼 Entity 并写入可召回索引"
    );

    let decision_hits = wait_for_recall_kind(
        &handle,
        "choose embedded vector index decision",
        MemoryKind::Decision,
    )
    .await;
    for (idx, hit) in decision_hits.iter().enumerate() {
        eprintln!(
            "[meso] decision_hit[{idx}] kind={:?} title={} summary={}",
            hit.kind, hit.title, hit.summary
        );
    }
    let decision = decision_hits
        .iter()
        .find(|hit| hit.kind == MemoryKind::Decision)
        .expect("应召回 Decision 节点");
    let expanded = handle.load_depth2(vec![decision.node_id.clone()]).await;
    let expanded_text = expanded
        .iter()
        .map(|item| item.full_content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    eprintln!("[meso] decision_depth2={expanded_text}");
    assert!(
        expanded_text.contains("embedded vector index"),
        "Decision 完整内容应保留 chosen/context 等决策线索"
    );

    let injection = handle
        .load_injection("session-meso", Some(&workspace_id))
        .await
        .join("\n");
    eprintln!("[meso] workspace_injection=\n{injection}");
    assert!(injection.contains("实体记忆"));
    assert!(injection.contains("决策记忆"));

    handle.run_meso_rumination("session-meso".to_string(), workspace_id.clone());
    eprintln!("[meso] rerun submitted for idempotency verification");

    let rerun_entity_hits =
        wait_for_unique_kind_hits(&handle, "vector memory entity", MemoryKind::Entity).await;
    let entity_ids = rerun_entity_hits
        .iter()
        .filter(|hit| hit.kind == MemoryKind::Entity)
        .map(|hit| hit.node_id.as_str())
        .collect::<Vec<_>>();
    let unique_entity_ids = entity_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    eprintln!(
        "[meso] rerun_entity_hits={} unique_entity_ids={} ids={entity_ids:?}",
        entity_ids.len(),
        unique_entity_ids.len()
    );
    assert_eq!(
        entity_ids.len(),
        unique_entity_ids.len(),
        "重复运行 Meso 后 Entity 搜索结果不应出现同一 node_id 的重复命中"
    );

    let rerun_decision_hits = wait_for_unique_kind_hits(
        &handle,
        "choose embedded vector index decision",
        MemoryKind::Decision,
    )
    .await;
    let decision_ids = rerun_decision_hits
        .iter()
        .filter(|hit| hit.kind == MemoryKind::Decision)
        .map(|hit| hit.node_id.as_str())
        .collect::<Vec<_>>();
    let unique_decision_ids = decision_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    eprintln!(
        "[meso] rerun_decision_hits={} unique_decision_ids={} ids={decision_ids:?}",
        decision_ids.len(),
        unique_decision_ids.len()
    );
    assert_eq!(
        decision_ids.len(),
        unique_decision_ids.len(),
        "重复运行 Meso 后 Decision 搜索结果不应出现同一 node_id 的重复命中"
    );
    assert_eq!(
        unique_decision_ids.len(),
        1,
        "同一 Episode 的决策记忆重复反刍后仍应保持单个 Decision 节点"
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn deep_recall_fixed_scenario_traces_cross_session_artifact_entity_and_decision() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);
    let mock_llm = MockMemoryLlmServer::start();
    let model = LlmEndpointConfig {
        source_provider: Some("mock-memory".to_string()),
        api_key: "test-memory-key".to_string(),
        base_url: mock_llm.base_url(),
        model: "memory-deep-recall-mock".to_string(),
        protocol: ProviderProtocol::OpenAiChatCompletions,
        // Deep Recall 会触发 5-7 轮 LLM 调用（裁决器 + 锚点规划器 + 整理器），
        // CI 慢环境下累积延迟可能超过 5s。加大超时并允许 1 次重试，避免瞬时超时
        // 导致 LLM 返回空 → deep→Normal 降级。
        timeout: Duration::from_secs(15),
        max_retries: 1,
    };
    let handle = start_with_options(
        MemoryOptions::new()
            .with_model(model)
            .with_vector_mode(MemoryVectorMode::Disabled),
    )
    .expect("启动带 mock Memory LLM 的 memory 失败");

    let current_context =
        "Nebula release overview mentions billing provider and event ledger decision";
    let artifact_url = "https://example.invalid/artifacts/nebula-release-plan.svg";
    let artifact_path = "/tmp/tiangong/nebula-release-plan.svg";
    let module_path = "/workspace/src/nebula/billing_provider.rs";

    handle.write_episode(
        Episode::new(
            "session-nebula-overview".to_string(),
            "nebula release overview".to_string(),
            format!("{current_context}, but current context omits artifact URL and module path."),
            EpisodeOutcome::Success,
            vec![
                "nebula".to_string(),
                "release".to_string(),
                "overview".to_string(),
            ],
            vec!["recall_memory".to_string()],
            0.6,
        ),
        Some(workspace_id.clone()),
    );
    handle.write_episode(
        Episode::new(
            "session-nebula-artifact".to_string(),
            "nebula artifact export".to_string(),
            format!("Nebula release artifact exported at {artifact_url} path={artifact_path}"),
            EpisodeOutcome::Success,
            vec![
                "nebula".to_string(),
                "artifact".to_string(),
                "export".to_string(),
            ],
            vec!["generate_image".to_string(), "write_file".to_string()],
            0.8,
        ),
        Some(workspace_id.clone()),
    );
    handle.write_episode(
        Episode::new(
            "session-nebula-module".to_string(),
            "nebula billing provider module".to_string(),
            format!(
                "nebula-billing-provider.rs keeps billing provider handoff rules in {module_path}"
            ),
            EpisodeOutcome::Success,
            vec![
                "nebula".to_string(),
                "billing".to_string(),
                "provider".to_string(),
                "nebula-billing-provider.rs".to_string(),
            ],
            vec!["write_file".to_string()],
            0.8,
        ),
        Some(workspace_id.clone()),
    );
    handle.write_episode(
        Episode::new(
            "session-nebula-decision".to_string(),
            "choose event ledger queue for nebula".to_string(),
            "We decided to choose event ledger queue instead of direct webhook fanout for nebula release because retry trace and idempotency are required."
                .to_string(),
            EpisodeOutcome::Success,
            vec![
                "nebula".to_string(),
                "decision".to_string(),
                "event".to_string(),
                "ledger".to_string(),
            ],
            vec!["architecture_decision".to_string()],
            0.85,
        ),
        Some(workspace_id.clone()),
    );

    handle.run_meso_rumination("session-nebula-decision".to_string(), workspace_id.clone());

    let overview = wait_for_recall_hit(&handle, "nebula release overview")
        .await
        .into_iter()
        .find(|hit| hit.title.contains("nebula release overview"))
        .expect("应召回 overview Episode");
    let artifact = wait_for_recall_hit(&handle, "nebula artifact export")
        .await
        .into_iter()
        .find(|hit| hit.summary.contains(artifact_url))
        .expect("应召回 artifact Episode");
    let entity = wait_for_recall_kind(&handle, "nebula-billing-provider.rs", MemoryKind::Entity)
        .await
        .into_iter()
        .find(|hit| hit.kind == MemoryKind::Entity)
        .expect("Meso 应提炼 Entity");
    let decision =
        wait_for_recall_kind(&handle, "event ledger queue decision", MemoryKind::Decision)
            .await
            .into_iter()
            .find(|hit| hit.kind == MemoryKind::Decision)
            .expect("Meso 应提炼 Decision");

    for target in [&artifact, &entity, &decision] {
        handle
            .upsert_relation(MemoryRelationDraft {
                from_node_id: overview.node_id.clone(),
                to_node_id: target.node_id.clone(),
                relation_kind: MemoryRelationKind::RelatedTo,
                weight: 0.9,
                note: Some("deep recall fixed scenario trace".to_string()),
                ..Default::default()
            })
            .await
            .expect("写入 Deep Recall 图关系失败");
    }

    let response = handle
        .recall_context(MemoryRecallRequest {
            query: "继续 nebula release，找上次产物、billing provider 模块和 event ledger 决策"
                .to_string(),
            reason: Some("固定评测：跨会话追溯产物、Entity 和 Decision".to_string()),
            expected: vec![
                "media".to_string(),
                "file".to_string(),
                "entity".to_string(),
                "decision".to_string(),
            ],
            context: vec![
                "user: 继续 nebula release".to_string(),
                format!("assistant: {current_context}"),
            ],
            limit: 2,
            ..Default::default()
        })
        .await;
    eprintln!(
        "[deep-fixed] used_llm={} depth={:?} deep_queries={:?} usage={} content=\n{}",
        response.used_llm,
        response.recall_depth,
        response.deep_queries,
        response.usage.total_tokens,
        response.content
    );

    assert!(
        response.used_llm,
        "Deep Recall 固定评测应走 Memory LLM 路径"
    );
    assert_eq!(response.recall_depth, RecallDepth::Deep);
    assert!(
        response
            .deep_queries
            .iter()
            .any(|query| query.contains("artifact")),
        "应记录 Deep Recall 追溯查询"
    );
    assert!(
        response
            .hits
            .iter()
            .any(|hit| hit.kind == MemoryKind::Entity),
        "Deep Recall 结果应包含 Entity 命中"
    );
    assert!(
        response
            .hits
            .iter()
            .any(|hit| hit.kind == MemoryKind::Decision),
        "Deep Recall 结果应包含 Decision 命中"
    );
    for expected in [
        artifact_url,
        artifact_path,
        module_path,
        "event ledger queue",
    ] {
        assert!(
            response.content.contains(expected),
            "Deep Recall 输出应包含跨层线索：{expected}"
        );
    }
    assert!(
        !response.content.contains(current_context),
        "Deep Recall 输出不应重复当前上下文已有内容"
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn memory_recall_levels_cover_injection_depth1_depth2_and_context_summary() {
    let (home, _workspace, workspace_path, workspace_id) = setup_workspace();
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    eprintln!("[memory-levels] fake_home={}", home.path().display());
    eprintln!(
        "[memory-levels] workspace_path={}",
        workspace_path.display()
    );
    eprintln!("[memory-levels] workspace_id={workspace_id}");

    write_file(
        &home.path().join(".tiangong/memory/profile/agent.md"),
        "profile recall level",
    );
    write_file(
        &home.path().join(format!(
            ".tiangong/memory/workspaces/{workspace_id}/agent.md"
        )),
        "workspace recall level",
    );
    write_file(
        &home
            .path()
            .join(".tiangong/memory/sessions/session-levels/agent.md"),
        "session recall level",
    );

    let handle =
        start_with_options(MemoryOptions::new().with_vector_mode(MemoryVectorMode::Disabled))
            .expect("启动 memory 失败");

    let injection = handle
        .load_injection("session-levels", Some(&workspace_id))
        .await;
    eprintln!(
        "[memory-levels] injection_count={} items={:#?}",
        injection.len(),
        injection
    );
    assert_eq!(
        injection,
        vec![
            "profile recall level".to_string(),
            "workspace recall level".to_string(),
            "session recall level".to_string(),
        ],
        "Injection 层应按 profile -> workspace -> session 顺序加载"
    );

    let image_url = "https://example.invalid/media/raven-drinking-water.png";
    let file_path = "/tmp/tiangong/raven-drinking-water.png";
    handle.run_micro_rumination(TurnResult {
        session_id: "session-levels".to_string(),
        turn_id: "turn-raven-image".to_string(),
        had_tool_calls: true,
        user_input: "generate raven drinking water image".to_string(),
        summary: "generated raven drinking water image with seedream model".to_string(),
        tool_calls: vec!["generate_image".to_string(), "write_file".to_string()],
        artifacts: vec![
            TurnArtifact {
                kind: TurnArtifactKind::Media,
                tool_name: Some("generate_image".to_string()),
                title: Some("raven drinking water image".to_string()),
                url: Some(image_url.to_string()),
                path: None,
                summary: Some("final image artifact".to_string()),
            },
            TurnArtifact {
                kind: TurnArtifactKind::File,
                tool_name: Some("write_file".to_string()),
                title: Some("local image file".to_string()),
                url: None,
                path: Some(file_path.to_string()),
                summary: Some("saved generated image file".to_string()),
            },
        ],
        workspace_id: Some(workspace_id.clone()),
    });
    eprintln!(
        "[memory-levels] micro_rumination submitted media_url={image_url} file_path={file_path}"
    );
    handle.write_episode(
        Episode::new(
            "session-levels".to_string(),
            "duplicate raven image artifact".to_string(),
            format!("duplicate raven image memory pointing at {image_url}"),
            EpisodeOutcome::Success,
            vec![
                "raven".to_string(),
                "image".to_string(),
                "seedream".to_string(),
            ],
            vec!["generate_image".to_string()],
            0.7,
        ),
        Some(workspace_id.clone()),
    );
    eprintln!("[memory-levels] duplicate episode submitted for URL dedupe coverage");

    let hits = wait_for_recall_hit(&handle, "raven image seedream").await;
    eprintln!("[memory-levels] depth1_hit_count={}", hits.len());
    for (idx, hit) in hits.iter().enumerate() {
        eprintln!(
            "[memory-levels] depth1_hit[{idx}] node_id={} score={:.3} kind={:?} title={} summary={}",
            hit.node_id, hit.score, hit.kind, hit.title, hit.summary
        );
    }
    assert!(
        hits.iter()
            .any(|hit| hit.summary.contains("raven drinking water image")),
        "Depth1 粗召回应返回 Episode 摘要"
    );

    let expanded = handle
        .load_depth2(hits.iter().map(|hit| hit.node_id.clone()).collect())
        .await;
    eprintln!("[memory-levels] depth2_item_count={}", expanded.len());
    for (idx, item) in expanded.iter().enumerate() {
        eprintln!(
            "[memory-levels] depth2_item[{idx}] node_id={} contains_image_url={} contains_file_path={} contains_generate_image={} preview={}",
            item.node_id,
            item.full_content.contains(image_url),
            item.full_content.contains(file_path),
            item.full_content.contains("generate_image"),
            compact_for_log(&item.full_content, 700)
        );
    }
    let expanded_text = expanded
        .iter()
        .map(|item| item.full_content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        expanded_text.contains(image_url),
        "Depth2 展开应包含结构化媒体 URL"
    );
    assert!(
        expanded_text.contains(file_path),
        "Depth2 展开应包含结构化文件产物路径"
    );
    assert!(
        expanded_text.contains("generate_image"),
        "Depth2 展开应保留工具调用信息"
    );

    let contextual = handle
        .recall_context(MemoryRecallRequest {
            query: "raven image seedream".to_string(),
            reason: Some("用户要求继续使用之前生成的图片".to_string()),
            expected: vec!["media".to_string(), "file".to_string()],
            context: vec![
                "user: raven image seedream".to_string(),
                "assistant: 当前上下文只包含用户请求，不包含图片 URL".to_string(),
            ],
            limit: 5,
            ..Default::default()
        })
        .await;
    eprintln!(
        "[memory-levels] contextual_used_llm={} contextual_hit_count={} url_count={} path_count={} content=\n{}",
        contextual.used_llm,
        contextual.hits.len(),
        contextual.content.matches(image_url).count(),
        contextual.content.matches(file_path).count(),
        contextual.content
    );

    assert!(
        contextual.content.contains(image_url),
        "Tool 化上下文回忆应整理出可继续使用的媒体 URL"
    );
    assert!(
        contextual.content.contains(file_path),
        "Tool 化上下文回忆应整理出可继续使用的文件路径"
    );
    assert_eq!(
        contextual.content.matches(image_url).count(),
        1,
        "整理后的回忆结果应去重，避免重复输出同一 URL；实际内容:\n{}",
        contextual.content
    );
    assert!(
        !contextual.content.contains("当前上下文只包含用户请求"),
        "整理后的回忆结果不应复述当前上下文，避免浪费 prompt"
    );

    handle.shutdown().await;
}

fn compact_for_log(text: &str, max_chars: usize) -> String {
    let normalized = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let mut clipped = normalized.chars().take(max_chars).collect::<String>();
    clipped.push_str("...");
    clipped
}
