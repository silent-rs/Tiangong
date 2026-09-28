use serial_test::serial;
use tiangong_memory::{
    ManualMemoryDraft, MemoryCognitiveType, MemoryListQuery, MemoryOptions, MemoryRelationDraft,
    MemoryRelationKind, MemoryStatus, start_with_options,
};

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

#[tokio::test]
#[serial]
async fn manual_memory_can_be_recalled_and_archived() {
    let _env = EnvGuard::enter();
    let workspace_id = format!("manual-memory-{}", scru128::new());
    let handle = start_with_options(MemoryOptions::new()).expect("Memory 应可启动");

    let saved = handle
        .upsert_manual_memory(ManualMemoryDraft {
            title: "Redis 哨兵部署".to_string(),
            summary:
                "当前工作区使用 Redis Sentinel 管理主从切换，配置文件在 infra/redis/sentinel.conf。"
                    .to_string(),
            keywords: vec!["redis".to_string(), "sentinel".to_string()],
            importance: 0.8,
            memory_type: MemoryCognitiveType::DomainKnowledge,
            workspace_id: Some(workspace_id.clone()),
            ..Default::default()
        })
        .await
        .expect("手动记忆应写入成功");

    let nodes = handle
        .list_nodes(MemoryListQuery {
            workspace_id: Some(workspace_id.clone()),
            query: Some("sentinel".to_string()),
            status: Some(MemoryStatus::Active),
            limit: 10,
            ..Default::default()
        })
        .await;
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, saved.id);
    assert_eq!(nodes[0].memory_type, MemoryCognitiveType::DomainKnowledge);

    let preference = handle
        .upsert_manual_memory(ManualMemoryDraft {
            title: "部署偏好".to_string(),
            summary: "用户希望基础设施部署说明优先保留配置文件路径和回滚步骤。".to_string(),
            keywords: vec!["deploy".to_string(), "preference".to_string()],
            importance: 0.7,
            memory_type: MemoryCognitiveType::UserPreference,
            workspace_id: Some(workspace_id.clone()),
            ..Default::default()
        })
        .await
        .expect("第二条手动记忆应写入成功");

    let relation = handle
        .upsert_relation(MemoryRelationDraft {
            from_node_id: preference.id.clone(),
            to_node_id: saved.id.clone(),
            relation_kind: MemoryRelationKind::RelatedTo,
            weight: 1.0,
            note: Some("部署偏好关联到 Redis Sentinel 事实记忆".to_string()),
            ..Default::default()
        })
        .await
        .expect("记忆关系应写入成功");
    let relations = handle.list_relations(preference.id.clone()).await;
    assert_eq!(relations.len(), 1);
    assert_eq!(relations[0].id, relation.id);
    assert_eq!(relations[0].to_node_id, saved.id);

    let hits = handle
        .recall(
            tiangong_memory::RecallAnchors {
                keywords: Vec::new(),
                query: "redis sentinel 配置".to_string(),
                strategy: None,
            },
            5,
        )
        .await;
    assert!(
        hits.iter().any(|hit| hit.node_id == saved.id),
        "手动写入的记忆应可被 recall 命中"
    );

    handle
        .set_node_status(saved.id.clone(), MemoryStatus::Archived)
        .await
        .expect("手动记忆应可归档");
    let active_nodes = handle
        .list_nodes(MemoryListQuery {
            workspace_id: Some(workspace_id),
            status: Some(MemoryStatus::Active),
            limit: 10,
            ..Default::default()
        })
        .await;
    assert!(!active_nodes.iter().any(|node| node.id == saved.id));

    handle.shutdown().await;
}
