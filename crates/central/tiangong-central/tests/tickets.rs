//! tickets 领域测试（R7 测试集中：原 src/tickets.rs 内嵌测试迁出）。

use tiangong_central::tickets::{RiskLevel, TicketRegistry, TicketVerdict};
// 原 mod tests

#[test]
fn workspace_approval_happy_path_is_one_shot() {
    let registry = TicketRegistry::new();
    let ticket = registry.issue_workspace_approval("/ws/proj", "任务指纹 A", RiskLevel::High, 600);
    // 首次消费：放行
    let first = registry.consume_workspace_approval(&ticket.approval_id, "/ws/proj", "任务指纹 A");
    assert!(first.is_granted());
    // 重放：拒绝（一次性）
    let replay = registry.consume_workspace_approval(&ticket.approval_id, "/ws/proj", "任务指纹 A");
    assert_eq!(replay, TicketVerdict::DeniedUsedOrMissing, "票据一次性");
}

/// 方案 8.3「票据错绑 work_id 被拒绝」：确认 A、执行 B 偷换被拒。
#[test]
fn workspace_approval_rejects_fingerprint_swap() {
    let registry = TicketRegistry::new();
    let ticket = registry.issue_workspace_approval("/ws/a", "任务 A", RiskLevel::Low, 600);
    let swapped = registry.consume_workspace_approval(&ticket.approval_id, "/ws/a", "任务 B");
    assert_eq!(swapped, TicketVerdict::DeniedMismatch);
    let wrong_ws = registry.consume_workspace_approval(&ticket.approval_id, "/ws/b", "任务 A");
    assert_eq!(wrong_ws, TicketVerdict::DeniedMismatch);
    // 错绑不消费票据：正确绑定仍可用
    let ok = registry.consume_workspace_approval(&ticket.approval_id, "/ws/a", "任务 A");
    assert!(ok.is_granted());
}

/// 方案 8.3「过期票据被拒绝」。
#[test]
fn expired_ticket_is_denied_not_granted_by_default() {
    let registry = TicketRegistry::new();
    let ticket = registry.issue_workspace_approval("/ws/e", "任务", RiskLevel::Low, 1);
    // 真实等待过期（R7 测试集中后不篡改私有字段——原内嵌版直接改
    // expires_at，迁出 crate 后不可达；1 秒 TTL + 1.1s 等待等价覆盖）
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let verdict = registry.consume_workspace_approval(&ticket.approval_id, "/ws/e", "任务");
    assert_eq!(verdict, TicketVerdict::DeniedExpired, "过期≠同意");
}

/// 模型伪造票据（不存在的 id）被拒：fail-closed。
#[test]
fn forged_ticket_id_is_denied() {
    let registry = TicketRegistry::new();
    let verdict = registry.consume_workspace_approval("forged-id", "/ws/x", "任务");
    assert_eq!(verdict, TicketVerdict::DeniedUsedOrMissing);
    let decision = registry.consume_decision("forged", "exec-1", 1);
    assert_eq!(decision, TicketVerdict::DeniedUsedOrMissing);
}

/// 决策票据绑定 execution 与问题版本。
#[test]
fn decision_ticket_binds_execution_and_version() {
    let registry = TicketRegistry::new();
    let ticket = registry.issue_decision("exec-run", 2, "继续", 600);
    // 错 execution
    assert_eq!(
        registry.consume_decision(&ticket.ticket_id, "exec-other", 2),
        TicketVerdict::DeniedMismatch
    );
    // 错问题版本（问题已更新，旧答复作废）
    assert_eq!(
        registry.consume_decision(&ticket.ticket_id, "exec-run", 3),
        TicketVerdict::DeniedMismatch
    );
    // 正确绑定
    assert!(
        registry
            .consume_decision(&ticket.ticket_id, "exec-run", 2)
            .is_granted()
    );
    // 一次性
    assert_eq!(
        registry.consume_decision(&ticket.ticket_id, "exec-run", 2),
        TicketVerdict::DeniedUsedOrMissing
    );
}
/// 派发路径按对消费：任务指纹偷换拒绝，匹配票据一次性放行。
#[test]
fn consume_for_dispatch_binds_fingerprint() {
    let registry = TicketRegistry::new();
    registry.issue_workspace_approval("/ws/d", "任务甲", RiskLevel::Low, 600);
    // 偷换任务：指纹不匹配视同无授权
    assert_eq!(
        registry.consume_workspace_approval_for("/ws/d", "任务乙"),
        TicketVerdict::DeniedUsedOrMissing
    );
    // 匹配任务：放行且一次性
    assert!(
        registry
            .consume_workspace_approval_for("/ws/d", "任务甲")
            .is_granted()
    );
    assert_eq!(
        registry.consume_workspace_approval_for("/ws/d", "任务甲"),
        TicketVerdict::DeniedUsedOrMissing,
        "同一任务重派需要新授权（一次性）"
    );
}

// 原 mod coverage_extra_tests

/// consume_decision_for_execution：无票据 / 命中消费 / 二次消费拒绝 /
/// 过期拒绝。
#[test]
fn consume_decision_for_execution_paths() {
    let registry = TicketRegistry::default();
    // 无票据 → 拒绝
    assert_eq!(
        registry.consume_decision_for_execution("exec-none"),
        TicketVerdict::DeniedUsedOrMissing
    );
    // 签发 → 命中消费 → Granted
    registry.issue_decision("exec-d1", 1, "继续", 3600);
    assert_eq!(
        registry.consume_decision_for_execution("exec-d1"),
        TicketVerdict::Granted
    );
    // 二次消费 → 拒绝（一次性）
    assert_eq!(
        registry.consume_decision_for_execution("exec-d1"),
        TicketVerdict::DeniedUsedOrMissing
    );
    // 过期 → 拒绝
    registry.issue_decision("exec-d2", 1, "继续", 1);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(
        registry.consume_decision_for_execution("exec-d2"),
        TicketVerdict::DeniedExpired
    );
}

/// consume_decision（带 ticket_id + 版本）与按 execution 消费的互通。
#[test]
fn consume_decision_binds_ticket_id_and_version() {
    let registry = TicketRegistry::default();
    let ticket = registry.issue_decision("exec-t1", 2, "方案B", 3600);
    // 版本错绑拒绝（mismatch）
    assert_eq!(
        registry.consume_decision(&ticket.ticket_id, "exec-t1", 1),
        TicketVerdict::DeniedMismatch
    );
    // execution 错绑拒绝（mismatch）
    assert_eq!(
        registry.consume_decision(&ticket.ticket_id, "exec-other", 2),
        TicketVerdict::DeniedMismatch
    );
    // 正确组合 → Granted
    assert_eq!(
        registry.consume_decision(&ticket.ticket_id, "exec-t1", 2),
        TicketVerdict::Granted
    );
    // 同一票据按 execution 再消费 → 已用拒绝
    assert_eq!(
        registry.consume_decision_for_execution("exec-t1"),
        TicketVerdict::DeniedUsedOrMissing
    );
}

/// 按对查验（不消费）：排队派发入队时只查验、投递时才消费（2026-11-25 修复——
/// 此前入队即消费，依赖终态后重入派发链时票据已用，排队项必然被拒）。
#[test]
fn peek_workspace_approval_does_not_consume() {
    let registry = TicketRegistry::new();
    assert!(
        !registry.peek_workspace_approval_for("/ws/q", "任务 Q"),
        "无票据 → 未授权"
    );
    registry.issue_workspace_approval("/ws/q", "任务 Q", RiskLevel::High, 600);
    assert!(
        registry.peek_workspace_approval_for("/ws/q", "任务 Q"),
        "未用未过期票据 → 已授权"
    );
    assert!(
        !registry.peek_workspace_approval_for("/ws/q", "任务 R"),
        "指纹不一致 → 未授权（防偷换）"
    );
    assert!(
        !registry.peek_workspace_approval_for("/ws/other", "任务 Q"),
        "目录不一致 → 未授权"
    );
    // 多次查验不改变票据状态
    assert!(registry.peek_workspace_approval_for("/ws/q", "任务 Q"));
    assert!(registry.peek_workspace_approval_for("/ws/q", "任务 Q"));
    // 消费后查验为假
    assert!(
        registry
            .consume_workspace_approval_for("/ws/q", "任务 Q")
            .is_granted()
    );
    assert!(!registry.peek_workspace_approval_for("/ws/q", "任务 Q"));
}

/// 过期票据查验为假（过期 ≠ 同意）。
#[test]
fn peek_expired_workspace_approval_is_false() {
    let registry = TicketRegistry::new();
    registry.issue_workspace_approval("/ws/exp", "任务", RiskLevel::Low, 1);
    assert!(registry.peek_workspace_approval_for("/ws/exp", "任务"));
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(!registry.peek_workspace_approval_for("/ws/exp", "任务"));
}
