//! 补写缺失向量集成测试（整理工具侧实现）。
//!
//! 场景：Memory 用确定性 embedding 写入记忆并建好向量表 → 关闭 Memory、
//! 从表中删掉部分向量模拟「写入时漏写」→ 工具经 Memory 公开 API 列出活跃
//! 记忆、比对向量表并补写；覆盖试运行只统计、补写后覆盖完整、重复运行幂等、
//! 维度不一致拒绝写入。
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use lancedb::arrow::arrow_array::{Array, FixedSizeListArray, Float32Array, StringArray};
use lancedb::connect;
use lancedb::query::ExecutableQuery;
use serial_test::serial;
use tiangong_llm::EmbeddingProvider;
use tiangong_memory::{
    ManualMemoryDraft, MemoryCognitiveType, MemoryHandle, MemoryOptions, MemoryVectorMode,
    node_embedding_text, start_with_options, vector_table_name,
};
use tiangong_memory_consolidator::vector_repair::repair_missing_vectors;

const DIM: usize = 4;
const MODEL: &str = "repair-test-embedding";

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

    fn lancedb_dir(&self) -> std::path::PathBuf {
        self.home
            .path()
            .join(".tiangong")
            .join("data")
            .join("memory")
            .join("lancedb")
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
    }
}

/// 确定性 embedding：由文本长度派生，便于断言写入的向量来自同一文本规则。
fn vector_of(text: &str) -> Vec<f32> {
    let len = text.chars().count() as f32;
    vec![len, 1.0, 0.5, 0.25]
}

struct FixedEmbedding {
    dimension: usize,
}

#[async_trait]
impl EmbeddingProvider for FixedEmbedding {
    async fn embed(&self, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut vector = vector_of(text);
                vector.resize(self.dimension, 0.0);
                vector
            })
            .collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn model(&self) -> &str {
        MODEL
    }
}

/// 以 OpenAI 兼容格式返回确定性向量的本地 embedding 服务（供 Memory 建表写入）。
struct EmbeddingServer {
    base_url: String,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl EmbeddingServer {
    fn start() -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定 embedding 服务失败");
        let addr = listener.local_addr().expect("读取地址失败");
        listener.set_nonblocking(true).expect("设置非阻塞失败");
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            loop {
                if rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        let mut buffer = Vec::new();
                        let mut chunk = [0u8; 8192];
                        let body = loop {
                            let Ok(read) = stream.read(&mut chunk) else {
                                break None;
                            };
                            if read == 0 {
                                break None;
                            }
                            buffer.extend_from_slice(&chunk[..read]);
                            let Some(end) =
                                buffer.windows(4).position(|window| window == b"\r\n\r\n")
                            else {
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
                        let inputs = body
                            .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                            .and_then(|value| value.get("input").cloned())
                            .and_then(|value| value.as_array().cloned())
                            .unwrap_or_default();
                        let data: Vec<_> = inputs
                            .iter()
                            .enumerate()
                            .map(|(index, item)| {
                                serde_json::json!({
                                    "object": "embedding",
                                    "index": index,
                                    "embedding": vector_of(item.as_str().unwrap_or_default()),
                                })
                            })
                            .collect();
                        let payload =
                            serde_json::json!({"object": "list", "data": data, "model": MODEL})
                                .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                            payload.len()
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
            shutdown: Some(tx),
            join: Some(join),
        }
    }

    fn config(&self) -> tiangong_llm::EmbeddingEndpointConfig {
        tiangong_llm::EmbeddingEndpointConfig {
            base_url: self.base_url.clone(),
            api_key: "test-key".to_string(),
            model: MODEL.to_string(),
            protocol: tiangong_llm::ProviderProtocol::OpenAiChatCompletions,
            timeout: Duration::from_secs(5),
            dimension: DIM,
        }
    }
}

impl Drop for EmbeddingServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

async fn save(handle: &MemoryHandle, title: &str) -> String {
    handle
        .upsert_manual_memory(ManualMemoryDraft {
            title: title.to_string(),
            summary: format!("{title} 的详细摘要内容"),
            keywords: vec!["repair".to_string()],
            importance: 0.6,
            memory_type: MemoryCognitiveType::DomainKnowledge,
            ..Default::default()
        })
        .await
        .expect("写入记忆失败")
        .id
}

/// 读取向量表全部 (node_id → 向量)。
async fn table_vectors(
    dir: &std::path::Path,
    table: &str,
) -> std::collections::HashMap<String, Vec<f32>> {
    let db = connect(&dir.to_string_lossy()).execute().await.unwrap();
    let table = db.open_table(table).execute().await.unwrap();
    let batches = table
        .query()
        .execute()
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let mut rows = std::collections::HashMap::new();
    for batch in batches {
        let ids = batch
            .column_by_name("node_id")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .clone();
        let vectors = batch
            .column_by_name("vector")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap()
            .clone();
        for row in 0..batch.num_rows() {
            let item = vectors.value(row);
            let floats = item.as_any().downcast_ref::<Float32Array>().unwrap();
            rows.insert(ids.value(row).to_string(), floats.values().to_vec());
        }
    }
    rows
}

async fn delete_vectors(dir: &std::path::Path, table: &str, ids: &[String]) {
    let db = connect(&dir.to_string_lossy()).execute().await.unwrap();
    let table = db.open_table(table).execute().await.unwrap();
    for id in ids {
        table.delete(&format!("node_id = '{id}'")).await.unwrap();
    }
}

/// 等 Memory 后台把记忆向量写进表（写入由 actor 异步完成）。
async fn wait_for_vectors(dir: &std::path::Path, table: &str, expected: usize) {
    for _ in 0..100 {
        if dir.join(format!("{table}.lance")).exists()
            && table_vectors(dir, table).await.len() >= expected
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等待 Memory 写入向量超时");
}

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn repairs_only_missing_vectors_with_memory_text_and_is_idempotent() {
    let env = EnvGuard::enter();
    let server = EmbeddingServer::start();
    let dir = env.lancedb_dir();
    let table = vector_table_name(MODEL, DIM);

    // 1) Memory 用同一 embedding 写入 4 条记忆并建表。
    let writer = start_with_options(
        MemoryOptions::new()
            .with_embedding(server.config())
            .with_vector_mode(MemoryVectorMode::EmbeddedLanceDb),
    )
    .expect("启动 Memory 失败");
    let mut ids = Vec::new();
    for title in ["记忆甲", "记忆乙", "记忆丙", "记忆丁"] {
        ids.push(save(&writer, title).await);
    }
    wait_for_vectors(&dir, &table, ids.len()).await;
    let before = table_vectors(&dir, &table).await;
    writer.shutdown().await;

    // 2) 模拟写入时漏写：删掉其中 2 条的向量。
    let dropped = vec![ids[1].clone(), ids[3].clone()];
    delete_vectors(&dir, &table, &dropped).await;

    // 3) 工具侧：经 Memory 公开 API 列活跃记忆（不启用向量层，只读元数据）。
    let reader = start_with_options(MemoryOptions::new()).expect("启动 Memory 失败");
    let provider = || reader.clone();

    let dry = repair_missing_vectors(&provider, &dir, &table, None, None)
        .await
        .expect("试运行统计失败");
    assert_eq!((dry.active, dry.missing, dry.repaired), (4, 2, 0));
    assert!(dry.dry_run);
    assert_eq!(table_vectors(&dir, &table).await.len(), 2, "试运行不写入");

    let embedding: Arc<dyn EmbeddingProvider> = Arc::new(FixedEmbedding { dimension: DIM });
    let report = repair_missing_vectors(&provider, &dir, &table, Some(embedding.clone()), None)
        .await
        .expect("补写失败");
    assert_eq!((report.missing, report.repaired, report.failed), (2, 2, 0));
    assert_eq!(report.remaining(), 0);

    // 补写后：全部活跃记忆都有向量，且与 Memory 写入时完全一致（同文本规则）。
    let after = table_vectors(&dir, &table).await;
    assert_eq!(after.len(), 4);
    for id in &ids {
        assert_eq!(
            after[id], before[id],
            "记忆 {id} 的补写向量应与 Memory 写入一致"
        );
    }
    let nodes = reader
        .list_nodes(tiangong_memory::MemoryListQuery {
            limit: 10,
            ..Default::default()
        })
        .await;
    for node in nodes {
        assert_eq!(after[&node.id], vector_of(&node_embedding_text(&node)));
    }

    // 重复运行幂等：无缺失、不再写入。
    let again = repair_missing_vectors(&provider, &dir, &table, Some(embedding), None)
        .await
        .expect("重复补写失败");
    assert_eq!((again.missing, again.repaired), (0, 0));

    // 维度不一致的模型拒绝写入。
    let wrong: Arc<dyn EmbeddingProvider> = Arc::new(FixedEmbedding { dimension: 8 });
    let err = repair_missing_vectors(&provider, &dir, &table, Some(wrong), None)
        .await
        .expect_err("维度不一致应拒绝");
    assert!(err.to_string().contains("维度"), "{err}");

    reader.shutdown().await;
}
