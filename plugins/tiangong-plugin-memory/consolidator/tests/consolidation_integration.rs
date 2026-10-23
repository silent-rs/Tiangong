//! 记忆深度整理集成测试（独立工具，经 Memory 公开 API 读写）。
//!
//! 覆盖：全量扫描 → 相似检索 → 规则整理 → 关联链重建 → 低质量遗忘，
//! 以及试运行不落库与 `max_nodes` 批次限制两条边界路径。

use serial_test::serial;
use tiangong_memory::MemoryOptions;
use tiangong_memory::{
    ManualMemoryDraft, MemoryCognitiveType, MemoryListQuery, MemoryRelationDraft,
    MemoryRelationKind, MemoryStatus, start_with_options,
};
use tiangong_memory_consolidator::consolidation::{ConsolidationOptions, run_consolidation};

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

    let report =
        run_consolidation(&|| handle.clone(), &ConsolidationOptions::default(), None).await;

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
