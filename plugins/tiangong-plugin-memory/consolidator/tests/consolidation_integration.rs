//! 记忆深度整理集成测试（独立工具，经 Memory 公开 API 读写）。
//!
//! 覆盖：全量扫描 → 相似检索 → 规则整理 → 关联链重建 → 低质量遗忘，
//! 以及试运行不落库与 `max_nodes` 批次限制两条边界路径。

use serial_test::serial;
use tiangong_llm::LlmEndpointConfig;
use tiangong_memory::MemoryOptions;
use tiangong_memory::{
    ManualMemoryDraft, MemoryCognitiveType, MemoryListQuery, MemoryRelationDraft,
    MemoryRelationKind, MemoryStatus, start_with_options,
};
use tiangong_memory_consolidator::consolidation::{
    ConsolidationOptions, ConsolidationStage, run_consolidation,
};

/// 只执行 similar 阶段的默认参数（以下多数用例针对相似判定）。
fn similar_only() -> ConsolidationOptions {
    ConsolidationOptions {
        stages: vec![ConsolidationStage::Similar],
        ..Default::default()
    }
}

struct EnvGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_userprofile: Option<std::ffi::OsString>,
    home: tempfile::TempDir,
}

impl EnvGuard {
    fn enter() -> Self {
        let home = tempfile::tempdir().expect("创建临时 HOME 失败");
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());
        }
        Self {
            prev_home,
            prev_userprofile,
            home,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
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
        let _ = self.home.path();
    }
}

/// 写入一条手动记忆。
async fn save(
    handle: &tiangong_memory::MemoryHandle,
    title: &str,
    summary: &str,
    importance: f32,
) -> String {
    handle
        .upsert_manual_memory(ManualMemoryDraft {
            title: title.to_string(),
            summary: summary.to_string(),
            keywords: vec!["redis".to_string(), "sentinel".to_string()],
            importance,
            memory_type: MemoryCognitiveType::DomainKnowledge,
            ..Default::default()
        })
        .await
        .expect("手动记忆应写入成功")
        .id
}

/// 准备一组记忆：3 条高度一致 + 1 条无关 + 1 条低质量。
async fn prepare(
    handle: &tiangong_memory::MemoryHandle,
) -> (String, String, String, String, String) {
    let m1 = save(
        handle,
        "Redis 哨兵部署",
        "使用 Redis Sentinel 管理主从切换，配置在 infra/redis/sentinel.conf",
        0.8,
    )
    .await;
    let m2 = save(
        handle,
        "Redis 哨兵部署",
        "使用 Redis Sentinel 管理主从切换，配置在 infra/redis/sentinel.conf",
        0.7,
    )
    .await;
    let m3 = save(
        handle,
        "Redis 哨兵部署",
        "使用 Redis Sentinel 管理主从切换，配置在 infra/redis/sentinel.conf",
        0.6,
    )
    .await;
    let unrelated = save(
        handle,
        "PostgreSQL 主从复制",
        "使用流复制搭建只读副本，故障切换到 standby",
        0.7,
    )
    .await;
    let low_quality = save(handle, "临时备注", "x", 0.1).await;
    // m2 关联到无关记忆，整理后应重指向主记忆。
    handle
        .upsert_relation(MemoryRelationDraft {
            from_node_id: m2.clone(),
            to_node_id: unrelated.clone(),
            relation_kind: MemoryRelationKind::DependsOn,
            weight: 0.7,
            note: Some("哨兵切换依赖 PostgreSQL 状态检查".to_string()),
            ..Default::default()
        })
        .await
        .expect("关系应写入成功");
    (m1, m2, m3, unrelated, low_quality)
}

async fn active_ids(handle: &tiangong_memory::MemoryHandle) -> Vec<String> {
    handle
        .list_nodes(MemoryListQuery {
            status: Some(MemoryStatus::Active),
            limit: 100,
            ..Default::default()
        })
        .await
        .into_iter()
        .map(|node| node.id)
        .collect()
}

#[tokio::test]
#[serial]
async fn consolidation_merges_similar_memories_and_forgets_low_quality() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let (m1, m2, m3, unrelated, low_quality) = prepare(&handle).await;

    let report = run_consolidation(&|| handle.clone(), &similar_only(), None).await;

    assert_eq!(report.scanned, 5, "应扫描全部活跃记忆");
    // 一致组（3 条）算作一次处理，组员不再作为主记忆重复处理。
    assert_eq!(
        report.processed, 3,
        "应逐条处理完所有记忆（组员计入已处理）"
    );
    assert_eq!(report.remaining, 0, "过滤已处理后不应有剩余");
    assert_eq!(
        report.retrieval_backend, "lexical",
        "测试环境无 Embedding 端点"
    );
    assert!(
        report.merged_groups >= 1,
        "应发生合并：{:?}",
        report.actions
    );
    assert!(report.merged_away >= 2, "一致记忆应被归档：{report:?}");
    assert!(report.forgotten >= 1, "低质量记忆应被遗忘");
    assert!(!report.used_llm, "未配置 LLM 时走规则整理");

    let active = active_ids(&handle).await;
    assert_eq!(active.len(), 2, "仅主记忆与无关记忆保持活跃：{active:?}");
    assert!(active.contains(&unrelated), "无关记忆不应被动");
    assert!(!active.contains(&low_quality), "低质量记忆应被遗忘");
    let survivors: Vec<&String> = [&m1, &m2, &m3]
        .into_iter()
        .filter(|id| active.contains(*id))
        .collect();
    assert_eq!(survivors.len(), 1, "一致组只应保留一条主记忆");

    // 关联链重建：m2 → unrelated 应重指向主记忆。
    let primary = survivors[0];
    let relations = handle.list_relations(primary.clone()).await;
    assert!(
        relations
            .iter()
            .any(|relation| relation.to_node_id == unrelated
                && relation.relation_kind == MemoryRelationKind::DependsOn),
        "归档记忆的关联应重指向主记忆：{relations:?}"
    );
    let from_unrelated = handle.list_relations(unrelated.clone()).await;
    assert_eq!(
        from_unrelated.len(),
        1,
        "归档记忆的原关系应被替换为重指向关系：{from_unrelated:?}"
    );
    assert_eq!(from_unrelated[0].from_node_id, *primary);
    assert_eq!(from_unrelated[0].to_node_id, unrelated);
}

#[tokio::test]
#[serial]
async fn consolidation_offset_skips_newest_memories() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let (m1, _m2, _m3, unrelated, low_quality) = prepare(&handle).await;

    // 跳过最新的 2 条（保存顺序即更新时间从旧到新，跳过的是最后保存的两条）。
    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Similar],
            offset: 2,
            ..Default::default()
        },
        None,
    )
    .await;

    assert_eq!(report.scanned, 3, "应跳过最新的 2 条后扫描剩余 3 条");
    let active = active_ids(&handle).await;
    // 被跳过的两条（unrelated / low_quality 中最新的两条）不应被改动。
    let skipped = [&unrelated, &low_quality];
    let untouched = skipped.iter().filter(|id| active.contains(**id)).count();
    assert_eq!(untouched, 2, "跳过的记忆不应被整理：{active:?}");
    let _ = (m1, report);
}

#[tokio::test]
#[serial]
async fn consolidation_dry_run_changes_nothing() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let (m1, m2, m3, unrelated, low_quality) = prepare(&handle).await;

    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Similar],
            dry_run: true,
            ..Default::default()
        },
        None,
    )
    .await;

    assert!(report.dry_run);
    assert!(report.merged_groups >= 1);
    assert!(report.forgotten >= 1);
    let active = active_ids(&handle).await;
    assert_eq!(active.len(), 5, "试运行不得改动记忆库：{active:?}");
    for id in [&m1, &m2, &m3, &unrelated, &low_quality] {
        assert!(active.contains(id), "试运行不应归档 {id}");
    }
}

#[tokio::test]
#[serial]
async fn consolidation_respects_max_nodes_and_reports_remaining() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let (m1, m2, m3, unrelated, low_quality) = prepare(&handle).await;

    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Similar],
            max_nodes: 2,
            ..Default::default()
        },
        None,
    )
    .await;

    assert_eq!(report.processed, 2, "应按上限只处理两条");
    assert!(
        (1..=3).contains(&report.remaining),
        "应报告剩余未处理数量（扫描顺序无关）：{report:?}"
    );
    let active = active_ids(&handle).await;
    // 只改动已处理部分：至少留下未处理的记忆，也不会全部归档。
    assert!(
        (2..=4).contains(&active.len()),
        "批次上限内只应改动已处理部分：{active:?}"
    );
    let survivors = [&m1, &m2, &m3]
        .into_iter()
        .filter(|id| active.contains(*id))
        .count();
    assert!(survivors <= 3, "一致组至多保留三条（未处理时原样保留）");
    let _ = (unrelated, low_quality);
}

/// LLM 连续失败（无法连通）时：重试耗尽即中断、报错，且不写入任何改动。
#[tokio::test]
#[serial]
async fn consolidation_aborts_when_llm_unreachable() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let (m1, m2, m3, unrelated, low_quality) = prepare(&handle).await;

    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Similar],
            // 指向必然无法连通的端点：验证重试耗尽后中断且不写库。
            llm: Some(LlmEndpointConfig::new(
                "test-key",
                "http://127.0.0.1:9",
                "test-model",
            )),
            llm_retry_attempts: 2,
            llm_retry_delay: std::time::Duration::from_millis(50),
            forget_low_quality: false,
            ..Default::default()
        },
        None,
    )
    .await;

    assert!(
        report.fatal_error.is_some(),
        "LLM 连续失败应致命中断：{report:?}"
    );
    assert!(report.llm_calls == 0, "全部调用失败，不计成功调用");
    let active = active_ids(&handle).await;
    assert_eq!(active.len(), 5, "中断后不应写入任何改动：{active:?}");
    for id in [&m1, &m2, &m3, &unrelated, &low_quality] {
        assert!(active.contains(id), "中断时记忆 {id} 应保持原样");
    }
}

/// 模拟 OpenAI 兼容 Chat Completions 的 LLM：记录每次请求送审的记忆 ID，
/// 按给定规则返回判定 JSON。
struct MockLlm {
    base_url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl MockLlm {
    /// `decide`：输入本批记忆 (id, title) 列表，返回判定 JSON 字符串。
    fn start(decide: fn(&[(String, String)]) -> String) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定 LLM 服务失败");
        let addr = listener.local_addr().expect("读取地址失败");
        listener.set_nonblocking(true).expect("设置非阻塞失败");
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            loop {
                if rx.try_recv().is_ok() {
                    break;
                }
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                stream.set_nonblocking(false).ok();
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 16384];
                let body = loop {
                    let Ok(read) = stream.read(&mut chunk) else {
                        break None;
                    };
                    if read == 0 {
                        break None;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&buffer[..end]).to_string();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buffer.len() < end + 4 + length {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    }
                    break String::from_utf8(buffer[end + 4..].to_vec()).ok();
                };
                // 取最后一条用户消息里的 memories。
                let members: Vec<(String, String)> = body
                    .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                    .and_then(|value| {
                        let messages = value.get("messages")?.as_array()?.clone();
                        let content = messages.last()?.get("content")?.as_str()?.to_string();
                        serde_json::from_str::<serde_json::Value>(&content).ok()
                    })
                    .and_then(|prompt| {
                        prompt
                            .get("memories")
                            .or_else(|| prompt.get("logs"))?
                            .as_array()
                            .cloned()
                    })
                    .unwrap_or_default()
                    .iter()
                    .map(|item| {
                        (
                            item["id"].as_str().unwrap_or_default().to_string(),
                            item["title"]
                                .as_str()
                                .or_else(|| item["tool"].as_str())
                                .unwrap_or_default()
                                .to_string(),
                        )
                    })
                    .collect();
                seen.lock()
                    .unwrap()
                    .push(members.iter().map(|(id, _)| id.clone()).collect());
                let content = decide(&members);
                let payload = serde_json::json!({
                    "id": "mock",
                    "object": "chat.completion",
                    "model": "mock",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": content},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
            shutdown: Some(tx),
            join: Some(join),
        }
    }

    fn config(&self) -> LlmEndpointConfig {
        let mut config = LlmEndpointConfig::new("test-key", self.base_url.clone(), "mock");
        config.protocol = tiangong_llm::ProviderProtocol::OpenAiChatCompletions;
        config.max_retries = 0;
        config
    }

    fn requests(&self) -> Vec<Vec<String>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 仅把标题以「同一事实」开头的记忆归为一组，其余保持原样。
fn merge_same_fact(members: &[(String, String)]) -> String {
    let same: Vec<&String> = members
        .iter()
        .filter(|(_, title)| title.starts_with("同一事实"))
        .map(|(id, _)| id)
        .collect();
    if same.len() < 2 {
        return "{}".to_string();
    }
    serde_json::json!({
        "groups": [{
            "member_ids": same,
            "title": "同一事实（已归一）",
            "summary": "Redis Sentinel 配置在 infra/redis/sentinel.conf，负责主从切换",
            "keywords": ["redis", "sentinel"],
            "memory_type": "domain_knowledge",
            "reason": "描述同一配置"
        }]
    })
    .to_string()
}

/// 相似只决定送审范围，是否合并由 LLM 决定；种子的全部相似记忆（超过旧版
/// 的 6 条上限）都会送审，未被 LLM 点名的记忆保持原样。
#[tokio::test]
#[serial]
async fn llm_decides_merges_among_all_similar_candidates() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let summary = "Redis Sentinel 配置在 infra/redis/sentinel.conf，负责主从切换";
    // 10 条文本完全相同的记忆（词法相似度 1.0，全部互为候选）：
    // 3 条「同一事实」应被合并，7 条「独立事件」由 LLM 判定保留。
    let mut same = Vec::new();
    let mut separate = Vec::new();
    for index in 0..10 {
        let title = if index < 3 {
            format!("同一事实 {index}")
        } else {
            format!("独立事件 {index}")
        };
        // 标题不同但正文相同，确保词法相似度达到阈值。
        let id = handle
            .upsert_manual_memory(ManualMemoryDraft {
                title: title.clone(),
                summary: summary.to_string(),
                keywords: vec!["redis".to_string(), "sentinel".to_string()],
                importance: 0.6,
                memory_type: MemoryCognitiveType::DomainKnowledge,
                ..Default::default()
            })
            .await
            .expect("写入记忆失败")
            .id;
        if index < 3 {
            same.push(id);
        } else {
            separate.push(id);
        }
    }
    let llm = MockLlm::start(merge_same_fact);
    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Similar],
            similarity_threshold: 0.6,
            llm: Some(llm.config()),
            llm_retry_attempts: 1,
            forget_low_quality: false,
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(report.fatal_error.is_none(), "{report:?}");
    let requests = llm.requests();
    assert!(!requests.is_empty(), "应调用 LLM");
    assert_eq!(
        requests[0].len(),
        10,
        "首批应包含种子与全部 9 条相似记忆（不再截断前 6 条）：{requests:?}"
    );
    assert_eq!(report.merged_groups, 1, "{}", report.render());
    assert_eq!(report.merged_away, 2);
    let active = active_ids(&handle).await;
    assert_eq!(
        active.len(),
        8,
        "3 条合并为 1 条，其余 7 条保持原样：{active:?}"
    );
    for id in &separate {
        assert!(active.contains(id), "LLM 未点名的记忆 {id} 应保留");
    }
    let survivors: Vec<&String> = same.iter().filter(|id| active.contains(*id)).collect();
    assert_eq!(survivors.len(), 1);
    let merged = handle
        .list_nodes(MemoryListQuery {
            limit: 100,
            ..Default::default()
        })
        .await
        .into_iter()
        .find(|node| node.id == *survivors[0])
        .expect("主记忆应存在");
    assert_eq!(merged.title, "同一事实（已归一）");
    // 同一对记忆只送审一次：总送审条次远小于两两组合。
    let total: usize = requests.iter().map(Vec::len).sum();
    assert!(total <= 20, "同一对记忆不应重复送审：{requests:?}");
    let rendered = report.render();
    assert!(rendered.contains("★"), "明细应列出主记忆与组员：{rendered}");
}

/// 种子的相似记忆超过单批上限时分多批判定，且每批都包含种子。
#[tokio::test]
#[serial]
async fn candidates_beyond_batch_size_are_judged_in_multiple_batches() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let summary = "Redis Sentinel 配置在 infra/redis/sentinel.conf，负责主从切换";
    for index in 0..7 {
        handle
            .upsert_manual_memory(ManualMemoryDraft {
                title: format!("独立事件 {index}"),
                summary: summary.to_string(),
                keywords: vec!["redis".to_string()],
                importance: 0.6,
                memory_type: MemoryCognitiveType::DomainKnowledge,
                ..Default::default()
            })
            .await
            .expect("写入记忆失败");
    }
    let llm = MockLlm::start(|_| "{}".to_string());
    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Similar],
            similarity_threshold: 0.6,
            batch_size: 3,
            llm: Some(llm.config()),
            llm_retry_attempts: 1,
            forget_low_quality: false,
            dry_run: true,
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(report.fatal_error.is_none(), "{report:?}");
    let requests = llm.requests();
    // 首个种子有 6 条相似记忆，每批 1 种子 + 2 候选 → 3 批。
    let seed = requests[0][0].clone();
    let seeded: Vec<&Vec<String>> = requests
        .iter()
        .filter(|batch| batch.first() == Some(&seed))
        .collect();
    assert_eq!(seeded.len(), 3, "应分 3 批判定：{requests:?}");
    assert!(seeded.iter().all(|batch| batch.len() <= 3));
    // 7 条记忆两两共 21 对，每批覆盖 3 对：全部判定完且不重复。
    let pairs: usize = requests
        .iter()
        .map(|batch| batch.len() * (batch.len() - 1) / 2)
        .sum();
    assert!(pairs >= 21, "全部记忆对都应被判定过：{requests:?}");
    assert_eq!(report.merged_groups, 0, "LLM 未要求合并时不应合并");
    assert_eq!(active_ids(&handle).await.len(), 7);
}

/// `max_llm_calls` 限制 LLM 调用次数；种子按相似记忆数从多到少处理，
/// 没有相似记忆的记忆不会占用 `max_nodes` 名额。
#[tokio::test]
#[serial]
async fn llm_budget_and_dense_first_seed_order() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let summary = "Redis Sentinel 配置在 infra/redis/sentinel.conf，负责主从切换";
    // 最新写入的若干条互不相似（按更新时间从新到旧会被先扫描到）。
    for index in 0..4 {
        handle
            .upsert_manual_memory(ManualMemoryDraft {
                title: format!("相似组 {index}"),
                summary: summary.to_string(),
                keywords: vec!["redis".to_string()],
                importance: 0.6,
                memory_type: MemoryCognitiveType::DomainKnowledge,
                ..Default::default()
            })
            .await
            .expect("写入记忆失败");
    }
    for (index, text) in [
        "PostgreSQL 主从复制配置要点",
        "前端 Vue 组件拆分约定",
        "Kubernetes 节点亲和性调度策略",
    ]
    .iter()
    .enumerate()
    {
        handle
            .upsert_manual_memory(ManualMemoryDraft {
                title: format!("孤立 {index}"),
                summary: text.to_string(),
                keywords: vec![format!("k{index}")],
                importance: 0.6,
                memory_type: MemoryCognitiveType::DomainKnowledge,
                ..Default::default()
            })
            .await
            .expect("写入记忆失败");
    }
    let llm = MockLlm::start(|_| "{}".to_string());
    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Similar],
            similarity_threshold: 0.6,
            batch_size: 2,
            max_nodes: 1,
            max_llm_calls: 2,
            llm: Some(llm.config()),
            llm_retry_attempts: 1,
            forget_low_quality: false,
            dry_run: true,
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(report.fatal_error.is_none(), "{report:?}");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "应在 2 次 LLM 调用后停止：{requests:?}");
    assert!(report.llm_budget_exhausted);
    assert!(report.render().contains("已达 LLM 调用上限"));
    assert_eq!(report.processed, 1);
    assert_eq!(report.clusters.count, 1, "{}", report.render());
    assert_eq!(report.clusters.largest, 4);
}

// ---------------------------------------------------------------------------
// dedup / distill 阶段
// ---------------------------------------------------------------------------

async fn save_in(
    handle: &tiangong_memory::MemoryHandle,
    title: &str,
    summary: &str,
    importance: f32,
    workspace: Option<&str>,
) -> String {
    handle
        .upsert_manual_memory(ManualMemoryDraft {
            title: title.to_string(),
            summary: summary.to_string(),
            keywords: vec!["dedup".to_string()],
            importance,
            memory_type: MemoryCognitiveType::Factual,
            workspace_id: workspace.map(str::to_string),
            ..Default::default()
        })
        .await
        .expect("写入记忆失败")
        .id
}

async fn all_active(handle: &tiangong_memory::MemoryHandle) -> Vec<tiangong_memory::MemoryNode> {
    handle
        .list_nodes(MemoryListQuery {
            status: Some(MemoryStatus::Active),
            limit: 500,
            ..Default::default()
        })
        .await
}

/// 摘要完全一致的记忆在同一范围内归一：标题取多数、重要性取最高、关联改挂；
/// 不同范围与过短摘要不处理；不调用 LLM。
#[tokio::test]
#[serial]
async fn dedup_merges_identical_summaries_within_scope() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let summary = "调用插件 fs 的 file_write：成功 replacements=1,replace_all=false";
    let a1 = save_in(&handle, "写入文件", summary, 0.5, Some("ws-a")).await;
    let a2 = save_in(&handle, "写入文件", summary, 0.4, Some("ws-a")).await;
    let a3 = save_in(&handle, "另一个标题", summary, 0.9, Some("ws-a")).await;
    let other_scope = save_in(&handle, "写入文件", summary, 0.5, Some("ws-b")).await;
    let short1 = save_in(&handle, "短", "成功", 0.5, Some("ws-a")).await;
    let short2 = save_in(&handle, "短", "成功", 0.5, Some("ws-a")).await;
    let outside = save_in(
        &handle,
        "无关",
        "完全不同的一条记忆，用于验证关联改挂",
        0.5,
        Some("ws-a"),
    )
    .await;
    handle
        .upsert_relation(MemoryRelationDraft {
            from_node_id: a2.clone(),
            to_node_id: outside.clone(),
            relation_kind: MemoryRelationKind::RelatedTo,
            weight: 0.8,
            ..Default::default()
        })
        .await
        .expect("关系应写入成功");
    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Dedup],
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(report.fatal_error.is_none(), "{report:?}");
    assert!(!report.used_llm, "dedup 不应调用 LLM");
    assert_eq!(report.dedup_groups, 1, "{}", report.render());
    assert_eq!(report.dedup_removed, 2);
    assert_eq!(report.dedup_relinked, 1);
    let active = all_active(&handle).await;
    let ids: Vec<&str> = active.iter().map(|node| node.id.as_str()).collect();
    // 同类记忆按重要性选主：a3（0.9）保留，标题改为多数的「写入文件」。
    assert!(ids.contains(&a3.as_str()));
    assert!(!ids.contains(&a1.as_str()) && !ids.contains(&a2.as_str()));
    for id in [&other_scope, &short1, &short2, &outside] {
        assert!(ids.contains(&id.as_str()), "{id} 不应被归一");
    }
    let primary = active.iter().find(|node| node.id == a3).unwrap();
    assert_eq!(primary.title, "写入文件");
    assert!((primary.importance - 0.9).abs() < 1e-6);
    let relations = handle.list_relations(a3.clone()).await;
    assert!(
        relations
            .iter()
            .any(|relation| relation.to_node_id == outside && (relation.weight - 0.8).abs() < 1e-6),
        "组员的关联应改挂到主记忆：{relations:?}"
    );
}

/// 试运行时，后面的阶段跳过前面阶段模拟删除的记忆：dedup 已归一的记忆不会
/// 在 similar 阶段再被合并一次，且数据库保持不变。
#[tokio::test]
#[serial]
async fn dry_run_later_stages_skip_simulated_removals() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let summary = "使用 Redis Sentinel 管理主从切换，配置在 infra/redis/sentinel.conf";
    for importance in [0.8, 0.7, 0.6] {
        save_in(&handle, "Redis 哨兵部署", summary, importance, None).await;
    }
    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Dedup, ConsolidationStage::Similar],
            dry_run: true,
            forget_low_quality: false,
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(report.fatal_error.is_none(), "{report:?}");
    assert_eq!(report.dedup_removed, 2);
    assert_eq!(report.scanned, 1, "similar 只应看到 dedup 后剩下的主记忆");
    assert_eq!(report.merged_groups, 0, "{}", report.render());
    assert_eq!(all_active(&handle).await.len(), 3, "试运行不应改动数据库");
}

/// 经反刍规则兜底生成工具日志（无 LLM：每个带文件路径的工具调用写一条
/// 「<工具> 产物」证据），等待异步反刍落库。
async fn seed_tool_logs(handle: &tiangong_memory::MemoryHandle, workspace: &str, count: usize) {
    let candidates = (0..count)
        .map(|index| tiangong_memory::MemoryCandidate {
            tool_name: "fs__file_write".to_string(),
            step_index: index,
            hint: "写入文件".to_string(),
            suggested_kinds: Vec::new(),
            file_path: Some(format!("/repo/src/module_{index}.rs")),
            url: None,
            result_summary: Some(format!("调用插件 fs 的 file_write：成功 #{index}")),
            success: true,
            tool_source: Some("file_operation".to_string()),
            is_recalled_context: false,
        })
        .collect();
    handle
        .run_enhanced_micro_rumination(tiangong_memory::EnhancedTurnResult {
            session_id: "distill-session".to_string(),
            turn_id: "distill-turn".to_string(),
            had_tool_calls: true,
            user_input: "重构模块".to_string(),
            summary: "重构了若干模块".to_string(),
            workspace_id: Some(workspace.to_string()),
            memory_candidates: candidates,
            ..Default::default()
        })
        .await
        .expect("反刍应入队");
    for _ in 0..100 {
        let logs = all_active(handle)
            .await
            .into_iter()
            .filter(|node| node.title == "fs__file_write 产物")
            .count();
        if logs >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("工具日志未在预期时间内落库");
}

/// 提炼除最后一条外的全部日志为一条新经历并删除它们，最后一条点名保留。
fn distill_all_but_last(logs: &[(String, String)]) -> String {
    let ids: Vec<&String> = logs.iter().map(|(id, _)| id).collect();
    let Some((last, rest)) = ids.split_last() else {
        return "{}".to_string();
    };
    serde_json::json!({
        "experiences": [{
            "title": "重构 module 系列文件",
            "summary": "批量改写 /repo/src/module_*.rs",
            "keywords": ["refactor", "module"],
            "memory_type": "project_structure",
            "importance": 0.7,
            "source_ids": ids,
        }],
        "delete": rest.iter().map(|id| serde_json::json!({"id": id, "reason": "已提炼"})).collect::<Vec<_>>(),
        "keep": [{"id": last, "reason": "保留最终产物"}],
    })
    .to_string()
}

/// distill：工具日志连同证据路径交给 LLM，先写新经历再删日志；保留的日志
/// 与新经历建立 learned_from 关联；similar 阶段不再看到工具日志。
#[tokio::test]
#[serial]
async fn distill_turns_tool_logs_into_experience() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    seed_tool_logs(&handle, "ws-distill", 4).await;
    let logs: Vec<String> = all_active(&handle)
        .await
        .into_iter()
        .filter(|node| node.title == "fs__file_write 产物")
        .map(|node| node.id)
        .collect();
    assert_eq!(logs.len(), 4);
    let llm = MockLlm::start(distill_all_but_last);
    let options = ConsolidationOptions {
        stages: vec![ConsolidationStage::Distill],
        llm: Some(llm.config()),
        llm_retry_attempts: 1,
        ..Default::default()
    };
    // 先试运行：只报告不落库。
    let dry = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            dry_run: true,
            ..options.clone()
        },
        None,
    )
    .await;
    assert!(dry.fatal_error.is_none(), "{dry:?}");
    assert_eq!((dry.distill_created, dry.distill_deleted), (1, 3));
    assert_eq!(
        all_active(&handle).await.len(),
        logs.len() + 1,
        "试运行不落库（含反刍 Episode）"
    );

    let report = run_consolidation(&|| handle.clone(), &options, None).await;
    assert!(report.fatal_error.is_none(), "{report:?}");
    assert_eq!(report.distill_logs, 4);
    assert_eq!(report.distill_batches, 1);
    assert_eq!(report.distill_created, 1);
    assert_eq!(report.distill_deleted, 3);
    assert_eq!(report.distill_kept, 1);
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "试运行与正式执行各一次：{requests:?}");
    let active = all_active(&handle).await;
    let remaining: Vec<&String> = logs
        .iter()
        .filter(|id| active.iter().any(|node| node.id == **id))
        .collect();
    assert_eq!(remaining.len(), 1, "只保留被点名保留的日志");
    let experience = active
        .iter()
        .find(|node| node.title == "重构 module 系列文件")
        .expect("应写入提炼出的经历");
    assert_eq!(
        experience.scope_id.as_deref(),
        Some("ws-distill"),
        "新经历应留在原工作区"
    );
    assert_eq!(
        experience.memory_type,
        MemoryCognitiveType::ProjectStructure
    );
    let relations = handle.list_relations(experience.id.clone()).await;
    assert!(
        relations
            .iter()
            .any(|relation| relation.to_node_id == *remaining[0]
                && relation.relation_kind == MemoryRelationKind::LearnedFrom),
        "新经历应关联到保留的依据日志：{relations:?}"
    );
}

/// distill 的 LLM 连续失败时中断整理，不删除任何日志。
#[tokio::test]
#[serial]
async fn distill_aborts_without_deleting_when_llm_fails() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    seed_tool_logs(&handle, "ws-distill", 2).await;
    let before = all_active(&handle).await.len();
    let report = run_consolidation(
        &|| handle.clone(),
        &ConsolidationOptions {
            stages: vec![ConsolidationStage::Distill, ConsolidationStage::Similar],
            llm: Some(LlmEndpointConfig::new("k", "http://127.0.0.1:9", "m")),
            llm_retry_attempts: 1,
            llm_retry_delay: std::time::Duration::from_millis(10),
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(report.fatal_error.is_some(), "{report:?}");
    assert_eq!(report.pages, 0, "distill 中断后不应继续 similar");
    assert_eq!(all_active(&handle).await.len(), before);
}

/// 执行记录：每条被删除的记忆在删除前写入完整快照（含关联），试运行同样
/// 记录并标注 dry_run；记录可据此追溯。
#[tokio::test]
#[serial]
async fn journal_keeps_full_snapshot_of_every_deletion() {
    let _env = EnvGuard::enter();
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");
    let summary = "调用插件 fs 的 file_write：成功 replacements=1,replace_all=false";
    let keep = save_in(&handle, "写入文件", summary, 0.9, Some("ws")).await;
    let gone = save_in(&handle, "写入文件", summary, 0.4, Some("ws")).await;
    let outside = save_in(
        &handle,
        "无关",
        "完全不同的一条记忆，用于验证关联快照",
        0.5,
        Some("ws"),
    )
    .await;
    handle
        .upsert_relation(MemoryRelationDraft {
            from_node_id: gone.clone(),
            to_node_id: outside.clone(),
            relation_kind: MemoryRelationKind::RelatedTo,
            weight: 0.7,
            ..Default::default()
        })
        .await
        .expect("关系应写入成功");
    let dir = tempfile::tempdir().unwrap();
    for dry_run in [true, false] {
        let path = dir.path().join(format!("journal-{dry_run}.jsonl"));
        let journal = std::sync::Arc::new(
            tiangong_memory_consolidator::journal::Journal::create(&path).unwrap(),
        );
        let report = run_consolidation(
            &|| handle.clone(),
            &ConsolidationOptions {
                stages: vec![ConsolidationStage::Dedup],
                dry_run,
                journal: Some(journal),
                ..Default::default()
            },
            None,
        )
        .await;
        assert!(report.fatal_error.is_none(), "{report:?}");
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let deletes: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|line| line["event"] == "delete_node")
            .collect();
        assert_eq!(deletes.len(), 1, "{lines:?}");
        let deleted = deletes[0];
        assert_eq!(deleted["dry_run"], dry_run);
        assert_eq!(deleted["stage"], "dedup");
        assert_eq!(deleted["merged_into"], keep.as_str());
        assert_eq!(deleted["node"]["id"], gone.as_str());
        assert_eq!(deleted["node"]["summary"], summary, "应保存完整内容");
        assert!(
            deleted["relations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|relation| relation["to_node_id"] == outside.as_str()),
            "应保存删除前的关联：{deleted}"
        );
        assert!(
            lines.iter().any(|line| line["event"] == "upsert_relation"),
            "关联改挂也应记录"
        );
    }
    let active = all_active(&handle).await;
    assert!(!active.iter().any(|node| node.id == gone));
}
