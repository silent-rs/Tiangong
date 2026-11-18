//! event 领域测试（R7 测试集中：原 src/event.rs 内嵌测试迁出）。

use tiangong_central::event::{EventSeverity, WorkEvent, WorkEventType};
// 原 mod tests

#[test]
fn event_names_match_design() {
    assert_eq!(WorkEventType::WorkItemCreated.as_str(), "work_item.created");
    assert_eq!(
        WorkEventType::ApprovalRequired.as_str(),
        "approval.required"
    );
    assert_eq!(
        WorkEventType::CentralModelFallback.as_str(),
        "central.model.fallback"
    );
    assert_eq!(WorkEventType::CentralDegraded.as_str(), "central.degraded");
}

#[test]
fn all_event_types_round_trip_by_name() {
    for event_type in WorkEventType::ALL {
        let name = event_type.as_str();
        assert_eq!(WorkEventType::from_str_name(name), Some(event_type));
        let json = serde_json::to_string(&event_type).unwrap();
        assert_eq!(json, format!("\"{name}\""));
        let back: WorkEventType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event_type);
    }
}

#[test]
fn new_event_fills_id_and_timestamp() {
    let event = WorkEvent::new("work_1", WorkEventType::WorkItemStarted);
    assert!(!event.id.is_empty());
    assert_eq!(event.work_item_id, "work_1");
    assert_eq!(event.severity, EventSeverity::Normal);
    assert!(!event.requires_user_action);
    assert!(!event.created_at.is_empty());
    assert!(event.acknowledged_at.is_none());
}

#[test]
fn work_item_final_events() {
    assert!(WorkEventType::WorkItemCompleted.is_work_item_final());
    assert!(WorkEventType::WorkItemFailed.is_work_item_final());
    assert!(!WorkEventType::WorkItemCancelled.is_work_item_final());
    assert!(!WorkEventType::ModelUnavailable.is_work_item_final());
}

/// R0 契约：旧事件 JSON（无 execution_id 字段）必须能兼容读取。
#[test]
fn legacy_event_json_without_execution_id_parses() {
    let legacy = r#"{
        "id": "ev-1",
        "work_item_id": "work-1",
        "worker_session_id": "sess-1",
        "event_type": "work_item.completed",
        "severity": "normal",
        "requires_user_action": false,
        "payload": {},
        "created_at": "2026-11-13 10:00:00"
    }"#;
    let event: WorkEvent = serde_json::from_str(legacy).expect("旧事件 JSON 必须兼容读取");
    assert_eq!(event.work_item_id, "work-1");
    assert_eq!(event.execution_id, None, "缺失 execution_id 应回退 None");
}

/// R0 契约：execution_id 参与序列化，且可往返。
#[test]
fn execution_id_round_trips() {
    let event =
        WorkEvent::new("work-1", WorkEventType::WorkItemStarted).with_execution("exec-abc123");
    assert_eq!(event.execution_id.as_deref(), Some("exec-abc123"));
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"execution_id\":\"exec-abc123\""));
    let back: WorkEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back.execution_id.as_deref(), Some("exec-abc123"));
}

// 原 mod coverage_extra_tests

/// builder 全链：user_action / payload / worker_session / severity。
#[test]
fn event_builders_set_optional_fields() {
    let event = WorkEvent::new("work-b1", WorkEventType::ApprovalRequired)
        .with_user_action()
        .with_payload(serde_json::json!({"dir": "/ws"}))
        .with_worker_session("s-worker")
        .with_severity(EventSeverity::Critical);
    assert!(event.requires_user_action);
    assert_eq!(event.payload["dir"], "/ws");
    assert_eq!(event.worker_session_id.as_deref(), Some("s-worker"));
    assert_eq!(event.severity, EventSeverity::Critical);
    // severity serde snake_case 往返
    let text = serde_json::to_string(&event.severity).unwrap();
    assert_eq!(text, "\"critical\"");
}

/// EventSeverity::as_str 全枚举 + 与 from_str_name 的名称一致性。
#[test]
fn severity_names_cover_all_variants() {
    for (sev, name) in [
        (EventSeverity::Info, "info"),
        (EventSeverity::Normal, "normal"),
        (EventSeverity::Warning, "warning"),
        (EventSeverity::Critical, "critical"),
    ] {
        assert_eq!(sev.as_str(), name);
    }
    // WorkEventType::as_str 与 ALL 一致（防新增变体漏映射）
    for event_type in WorkEventType::ALL {
        assert_eq!(
            WorkEventType::from_str_name(event_type.as_str()),
            Some(event_type),
            "{} 应可解析",
            event_type.as_str()
        );
    }
    assert_eq!(WorkEventType::from_str_name("no.such.event"), None);
}
