use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use std::{
    io::{Read, Write},
    net::TcpListener,
};

use serial_test::serial;
use tempfile::TempDir;
use tiangong_llm::{EmbeddingEndpointConfig, ProviderProtocol};
use tiangong_memory::{
    Episode, EpisodeOutcome, MemoryOptions, MemoryStatus, MemoryVectorMode, RecallAnchors,
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

fn embedding_config_from_tiangong_config() -> Option<EmbeddingEndpointConfig> {
    // Embedding 已归 Memory 独立配置（~/.tiangong/memory/config.json）管理。
    tiangong_memory::MemoryConfig::load_or_default()
        .to_options()
        .embedding
}

struct DeterministicEmbeddingServer {
    base_url: String,
    shutdown_tx: Option<mpsc::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl DeterministicEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定 deterministic embedding 失败");
        let addr = listener.local_addr().expect("读取 embedding mock 地址失败");
        listener
            .set_nonblocking(true)
            .expect("设置 embedding mock 非阻塞失败");
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let body = read_http_body(&mut stream)
                            .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                            .unwrap_or_else(|| serde_json::json!({}));
                        let inputs = body
                            .get("input")
                            .and_then(serde_json::Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let data = inputs
                            .iter()
                            .enumerate()
                            .map(|(index, item)| {
                                let text = item.as_str().unwrap_or_default();
                                serde_json::json!({
                                    "object": "embedding",
                                    "index": index,
                                    "embedding": deterministic_embedding(text)
                                })
                            })
                            .collect::<Vec<_>>();
                        let payload = serde_json::json!({
                            "object": "list",
                            "data": data,
                            "model": "deterministic-memory-embedding"
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            payload.len(),
                            payload
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            shutdown_tx: Some(shutdown_tx),
            join: Some(join),
        }
    }

    fn config(&self) -> EmbeddingEndpointConfig {
        self.config_with_model("deterministic-memory-embedding")
    }

    fn config_with_model(&self, model: &str) -> EmbeddingEndpointConfig {
        EmbeddingEndpointConfig {
            base_url: self.base_url.clone(),
            api_key: "deterministic-test-key".to_string(),
            model: model.to_string(),
            protocol: ProviderProtocol::OpenAiChatCompletions,
            timeout: Duration::from_secs(5),
            dimension: 4,
        }
    }
}

impl Drop for DeterministicEmbeddingServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_http_body(stream: &mut std::net::TcpStream) -> Option<String> {
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

fn deterministic_embedding(text: &str) -> Vec<f32> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("dimension")
        || lower.contains("inference server")
        || lower.contains("settings form")
        || lower.contains("automatic form")
    {
        vec![1.0, 0.0, 0.0, 0.0]
    } else if lower.contains("checksum") || lower.contains("apple") || lower.contains("banana") {
        vec![0.0, 1.0, 0.0, 0.0]
    } else if lower.contains("redis")
        || lower.contains("sentinel")
        || lower.contains("cluster guardian")
        || lower.contains("continuity")
    {
        vec![0.0, 0.0, 1.0, 0.0]
    } else {
        vec![0.0, 0.0, 0.0, 1.0]
    }
}

/// 轮询向量索引元数据，直到满足条件（actor 异步打开索引并在后台回填）。
async fn wait_for_meta(
    lancedb_dir: &Path,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let path = lancedb_dir.join("index_meta.json");
    let mut last = serde_json::Value::Null;
    for _ in 0..100 {
        if let Some(meta) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        {
            if predicate(&meta) {
                return meta;
            }
            last = meta;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等待向量索引元数据超时，最后状态：{last}");
}

async fn wait_for_expected_hit(
    handle: &tiangong_memory::MemoryHandle,
    query: &str,
    expected_node_id: &str,
) -> Vec<tiangong_memory::RecallHit> {
    for attempt in 1..=60 {
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
        println!(
            "[recall attempt {attempt}] query={query:?}, hits={}",
            hits.len()
        );
        for (idx, hit) in hits.iter().enumerate() {
            println!(
                "  #{idx}: id={} score={:.4} title={} summary={}",
                hit.node_id, hit.score, hit.title, hit.summary
            );
        }
        if hits.iter().any(|hit| hit.node_id == expected_node_id) {
            return hits;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Vec::new()
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn embedded_hybrid_retrieval_uses_deterministic_embedding_without_external_service() {
    let embedding_server = DeterministicEmbeddingServer::start();
    let home = TempDir::new().expect("创建 fake home 失败");
    let workspace = TempDir::new().expect("创建 workspace 失败");
    let workspace_path = workspace.path().to_path_buf();
    let workspace_id = workspace_id_from_path(&workspace_path);
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    let handle = start_with_options(
        MemoryOptions::new()
            .with_embedding(embedding_server.config())
            .with_vector_mode(MemoryVectorMode::EmbeddedLanceDb),
    )
    .expect("启动 memory 失败");

    let semantic_episode = Episode::new(
        "hybrid-deterministic-session".to_string(),
        "settings vector probe workflow".to_string(),
        "Count the returned vector length from the inference server response and fill the settings form automatically."
            .to_string(),
        EpisodeOutcome::Success,
        vec!["settings".to_string(), "probe".to_string()],
        vec!["probe_embedding_dimension".to_string()],
        0.9,
    );
    let expected_id = semantic_episode.id.clone();
    handle.write_episode(semantic_episode, Some(workspace_id.clone()));
    handle.write_episode(
        Episode::new(
            "hybrid-deterministic-session".to_string(),
            "lexical sentinel unrelated checksum".to_string(),
            "apple banana checksum marker for pure keyword recall control".to_string(),
            EpisodeOutcome::Success,
            vec!["checksum".to_string()],
            vec!["noop".to_string()],
            0.2,
        ),
        Some(workspace_id),
    );

    let hits =
        wait_for_expected_hit(&handle, "automatic form dimension discovery", &expected_id).await;
    handle.shutdown().await;

    assert_eq!(
        hits.first().map(|hit| hit.node_id.as_str()),
        Some(expected_id.as_str()),
        "deterministic embedding 应让 hybrid 召回把语义相关 Episode 排到第一"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn recall_benchmark_compares_bm25_only_and_hybrid_hit_rate() {
    let embedding_server = DeterministicEmbeddingServer::start();
    let bm25_score = benchmark_recall(false, None).await;
    let hybrid_score = benchmark_recall(true, Some(embedding_server.config())).await;

    println!(
        "[recall-benchmark] bm25_hits={} hybrid_hits={}",
        bm25_score, hybrid_score
    );
    assert!(
        hybrid_score > bm25_score,
        "hybrid 应在语义指代样例上优于 BM25-only"
    );
    assert_eq!(hybrid_score, 3, "hybrid 应命中全部固定语义样例");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn archived_node_is_removed_from_embedded_vector_index_and_can_be_restored() {
    let embedding_server = DeterministicEmbeddingServer::start();
    let home = TempDir::new().expect("创建 fake home 失败");
    let workspace = TempDir::new().expect("创建 workspace 失败");
    let workspace_path = workspace.path().to_path_buf();
    let workspace_id = workspace_id_from_path(&workspace_path);
    let _env = EnvGuard::enter(home.path(), &workspace_path);
    let handle = start_with_options(
        MemoryOptions::new()
            .with_embedding(embedding_server.config())
            .with_vector_mode(MemoryVectorMode::EmbeddedLanceDb),
    )
    .expect("启动 memory 失败");

    let episode = Episode::new(
        "hybrid-archive-session".to_string(),
        "settings vector archive target".to_string(),
        "Count the returned vector length from the inference server response and fill the settings form automatically."
            .to_string(),
        EpisodeOutcome::Success,
        vec!["settings".to_string(), "probe".to_string()],
        vec!["probe_embedding_dimension".to_string()],
        0.9,
    );
    let node_id = episode.id.clone();
    handle.write_episode(episode, Some(workspace_id));
    let initial_hits =
        wait_for_expected_hit(&handle, "automatic form dimension discovery", &node_id).await;
    assert!(
        initial_hits.iter().any(|hit| hit.node_id == node_id),
        "归档前应能通过 embedded vector 召回目标节点"
    );

    handle
        .set_node_status(node_id.clone(), MemoryStatus::Archived)
        .await
        .expect("归档节点应成功");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let archived_hits = handle
        .recall(
            RecallAnchors {
                query: "automatic form dimension discovery".to_string(),
                keywords: Vec::new(),
                strategy: None,
            },
            5,
        )
        .await;
    assert!(
        archived_hits.iter().all(|hit| hit.node_id != node_id),
        "归档后目标节点不应继续从 SQLite/Tantivy/embedded vector 召回"
    );

    handle
        .set_node_status(node_id.clone(), MemoryStatus::Active)
        .await
        .expect("恢复节点应成功");
    let restored_hits =
        wait_for_expected_hit(&handle, "automatic form dimension discovery", &node_id).await;
    handle.shutdown().await;
    assert!(
        restored_hits.iter().any(|hit| hit.node_id == node_id),
        "恢复 active 后应重建当前向量索引并重新召回目标节点"
    );
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn vector_index_is_bound_to_model_fingerprint_and_rebuilt_on_switch() {
    let embedding_server = DeterministicEmbeddingServer::start();
    let home = TempDir::new().expect("创建 fake home 失败");
    let workspace = TempDir::new().expect("创建 workspace 失败");
    let workspace_path = workspace.path().to_path_buf();
    let workspace_id = workspace_id_from_path(&workspace_path);
    let _env = EnvGuard::enter(home.path(), &workspace_path);
    let lancedb_dir = home.path().join(".tiangong/memory/lancedb");
    let start = |model: &str| {
        start_with_options(
            MemoryOptions::new()
                .with_embedding(embedding_server.config_with_model(model))
                .with_vector_mode(MemoryVectorMode::EmbeddedLanceDb),
        )
        .expect("启动 memory 失败")
    };

    // 1. 首次使用模型 A 写入并召回。
    let handle = start("text-embedding-bge-m3");
    let episode = Episode::new(
        "fingerprint-session".to_string(),
        "settings vector probe workflow".to_string(),
        "Count the returned vector length from the inference server response and fill the settings form automatically."
            .to_string(),
        EpisodeOutcome::Success,
        vec!["settings".to_string()],
        vec!["noop".to_string()],
        0.9,
    );
    let node_id = episode.id.clone();
    handle.write_episode(episode, Some(workspace_id));
    let hits = wait_for_expected_hit(&handle, "automatic form dimension discovery", &node_id).await;
    assert!(hits.iter().any(|hit| hit.node_id == node_id));
    // 新建表需后台回填完成后才标记可用（中途关闭会在下次启动时续传）。
    let meta = wait_for_meta(&lancedb_dir, |meta| {
        meta["tables"]["bge-m3@4"]["complete"] == true
    })
    .await;
    handle.shutdown().await;
    let fingerprint_a = meta["active"].as_str().unwrap().to_string();
    assert_eq!(fingerprint_a, "bge-m3@4", "模型名应规范化");

    // 2. 同维度但不同模型：新建表并后台回填，回填完成后语义召回恢复。
    let handle = start("bge-large-zh-v1.5");
    let meta = wait_for_meta(&lancedb_dir, |meta| {
        meta["tables"]["bge-large-zh-v1.5@4"]["complete"] == true
    })
    .await;
    assert_eq!(meta["active"], "bge-large-zh-v1.5@4");
    assert_eq!(
        meta["previous"].as_str(),
        Some(fingerprint_a.as_str()),
        "保留上一代"
    );
    let hits = wait_for_expected_hit(&handle, "automatic form dimension discovery", &node_id).await;
    assert!(
        hits.first().is_some_and(|hit| hit.node_id == node_id),
        "回填完成后应能通过新模型的向量表召回"
    );
    handle.shutdown().await;

    // 3. 名称写法不同的同一模型切回：直接复用上一代表，无需回填。
    let handle = start("BAAI/bge-m3");
    let meta = wait_for_meta(&lancedb_dir, |meta| {
        meta["active"].as_str() == Some(fingerprint_a.as_str())
    })
    .await;
    assert_eq!(meta["tables"][&fingerprint_a]["complete"], true);
    assert_eq!(meta["previous"], "bge-large-zh-v1.5@4");
    let hits = wait_for_expected_hit(&handle, "automatic form dimension discovery", &node_id).await;
    handle.shutdown().await;
    assert!(hits.iter().any(|hit| hit.node_id == node_id));
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn legacy_single_table_is_not_claimed_by_current_model() {
    let embedding_server = DeterministicEmbeddingServer::start();
    let home = TempDir::new().expect("创建 fake home 失败");
    let workspace = TempDir::new().expect("创建 workspace 失败");
    let workspace_path = workspace.path().to_path_buf();
    let workspace_id = workspace_id_from_path(&workspace_path);
    let _env = EnvGuard::enter(home.path(), &workspace_path);
    let lancedb_dir = home.path().join(".tiangong/memory/lancedb");
    let options = || {
        MemoryOptions::new()
            .with_embedding(embedding_server.config_with_model("text-embedding-bge-m3"))
            .with_vector_mode(MemoryVectorMode::EmbeddedLanceDb)
    };

    // 构造旧版布局：写入一次后删除元数据、把表改回旧的固定表名。
    let handle = start_with_options(options()).expect("启动 memory 失败");
    let episode = Episode::new(
        "legacy-session".to_string(),
        "settings vector probe workflow".to_string(),
        "Count the returned vector length from the inference server response and fill the settings form automatically."
            .to_string(),
        EpisodeOutcome::Success,
        vec!["settings".to_string()],
        vec!["noop".to_string()],
        0.9,
    );
    let node_id = episode.id.clone();
    handle.write_episode(episode, Some(workspace_id));
    let hits = wait_for_expected_hit(&handle, "automatic form dimension discovery", &node_id).await;
    assert!(hits.iter().any(|hit| hit.node_id == node_id));
    handle.shutdown().await;
    std::fs::remove_file(lancedb_dir.join("index_meta.json")).unwrap();
    let _ = std::fs::remove_dir_all(lancedb_dir.join("__manifest"));
    std::fs::rename(
        lancedb_dir.join("memory_vectors_bge_m3_4.lance"),
        lancedb_dir.join("memory_vectors.lance"),
    )
    .expect("重命名为旧表名");

    // 重启：旧表来源模型未知，只能登记为上一代；当前模型另建新表并回填。
    // 维度相同不代表语义空间相同，直接认领会让查询向量与存量向量不匹配。
    let handle = start_with_options(options()).expect("重启 memory 失败");
    let meta = wait_for_meta(&lancedb_dir, |meta| {
        meta["active"].as_str() == Some("bge-m3@4")
    })
    .await;
    assert_eq!(
        meta["previous"], "legacy-unknown@4",
        "旧表登记为来源未知的上一代：{meta}"
    );
    assert_eq!(
        meta["tables"]["legacy-unknown@4"]["table"], "memory_vectors",
        "旧表保留不删：{meta}"
    );
    assert_ne!(
        meta["tables"]["bge-m3@4"]["table"], "memory_vectors",
        "当前模型应使用独立的新表：{meta}"
    );

    // 回填完成后仍可召回（数据来自新表，不是直接复用旧向量）。
    let meta = wait_for_meta(&lancedb_dir, |meta| {
        meta["tables"]["bge-m3@4"]["complete"] == true
    })
    .await;
    assert_eq!(meta["tables"]["bge-m3@4"]["complete"], true);
    let hits = wait_for_expected_hit(&handle, "automatic form dimension discovery", &node_id).await;
    handle.shutdown().await;
    assert!(
        hits.iter().any(|hit| hit.node_id == node_id),
        "回填完成后应能召回"
    );
}

async fn benchmark_recall(hybrid: bool, embedding: Option<EmbeddingEndpointConfig>) -> usize {
    let home = TempDir::new().expect("创建 benchmark fake home 失败");
    let workspace = TempDir::new().expect("创建 benchmark workspace 失败");
    let workspace_path = workspace.path().to_path_buf();
    let workspace_id = workspace_id_from_path(&workspace_path);
    let _env = EnvGuard::enter(home.path(), &workspace_path);
    let mut options = MemoryOptions::new();
    if hybrid {
        options = options
            .with_embedding(embedding.expect("hybrid benchmark 需要 embedding"))
            .with_vector_mode(MemoryVectorMode::EmbeddedLanceDb);
    }
    let handle = start_with_options(options).expect("启动 benchmark memory 失败");
    let cases = [
        (
            "settings vector probe workflow",
            "Count the returned vector length from the inference server response and fill the settings form automatically.",
            "dimension discovery",
        ),
        (
            "redis failover deployment",
            "Redis Sentinel manages primary replica failover and stores the sentinel deployment file.",
            "cluster guardian continuity",
        ),
        (
            "checksum control marker",
            "apple banana checksum marker for keyword-only baseline control",
            "apple banana checksum",
        ),
    ];
    let mut expected = Vec::new();
    for (title, summary, _) in cases {
        let episode = Episode::new(
            "hybrid-benchmark-session".to_string(),
            title.to_string(),
            summary.to_string(),
            EpisodeOutcome::Success,
            vec!["benchmark".to_string()],
            vec!["benchmark".to_string()],
            0.8,
        );
        expected.push(episode.id.clone());
        handle.write_episode(episode, Some(workspace_id.clone()));
    }

    let mut best_hit_count = 0;
    for _ in 1..=60 {
        let mut hit_count = 0;
        for (idx, (_, _, query)) in cases.iter().enumerate() {
            let hits = handle
                .recall(
                    RecallAnchors {
                        query: query.to_string(),
                        keywords: Vec::new(),
                        strategy: None,
                    },
                    3,
                )
                .await;
            if hits.first().is_some_and(|hit| hit.node_id == expected[idx]) {
                hit_count += 1;
            }
        }
        best_hit_count = best_hit_count.max(hit_count);
        if best_hit_count == cases.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    handle.shutdown().await;
    best_hit_count
}

#[tokio::test(flavor = "current_thread")]
#[serial]
#[ignore = "需要真实 ~/.tiangong/memory/config.json 在线 embedding 配置；使用 --ignored --nocapture 运行"]
async fn embedded_hybrid_retrieval_loads_configured_embedding_and_recalls_semantic_episode() {
    let Some(embedding) = embedding_config_from_tiangong_config() else {
        println!("[skip] 未在配置文件中找到 embedding 路由或 options.dimension");
        return;
    };
    if !matches!(embedding.protocol, ProviderProtocol::OpenAiChatCompletions) {
        println!(
            "[skip] embedding 协议不是 OpenAI 兼容协议: {}",
            embedding.protocol.as_str()
        );
        return;
    }

    let home = TempDir::new().expect("创建 fake home 失败");
    let workspace = TempDir::new().expect("创建 workspace 失败");
    let workspace_path = workspace.path().to_path_buf();
    let workspace_id = workspace_id_from_path(&workspace_path);
    let _env = EnvGuard::enter(home.path(), &workspace_path);

    println!(
        "[config] embedding_model={} dimension={} backend=embedded_flat",
        embedding.model, embedding.dimension
    );

    let handle = start_with_options(
        MemoryOptions::new()
            .with_embedding(embedding)
            .with_vector_mode(MemoryVectorMode::EmbeddedLanceDb),
    )
    .expect("启动 memory 失败");

    let semantic_episode = Episode::new(
        "hybrid-session".to_string(),
        "vector length probe for settings dialog".to_string(),
        "Count the returned vector length from the inference server response and fill the settings form automatically."
            .to_string(),
        EpisodeOutcome::Success,
        vec!["settings".to_string(), "probe".to_string()],
        vec!["probe_embedding_dimension".to_string()],
        0.9,
    );
    let expected_id = semantic_episode.id.clone();
    println!("[write] semantic_episode_id={expected_id}");
    handle.write_episode(semantic_episode, Some(workspace_id.clone()));

    handle.write_episode(
        Episode::new(
            "hybrid-session".to_string(),
            "lexical sentinel unrelated checksum".to_string(),
            "apple banana checksum marker for pure keyword recall control".to_string(),
            EpisodeOutcome::Success,
            vec!["checksum".to_string()],
            vec!["noop".to_string()],
            0.2,
        ),
        Some(workspace_id),
    );

    let hits = wait_for_expected_hit(&handle, "embedding dimension", &expected_id).await;
    handle.shutdown().await;

    assert!(
        hits.iter().any(|hit| hit.node_id == expected_id),
        "混合检索应通过配置的 embedding + 内置向量索引召回语义相关 Episode"
    );
}
