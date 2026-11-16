//! risk 领域测试（R7 测试集中：原 src/risk.rs 内嵌测试迁出）。

use tiangong_central::risk::RiskLevel;
// 原 mod tests

#[test]
fn policy_matches_design() {
    assert!(RiskLevel::Low.auto_dispatch_allowed());
    assert!(!RiskLevel::Low.requires_final_confirmation());

    assert!(!RiskLevel::Medium.auto_dispatch_allowed());
    assert!(!RiskLevel::Medium.requires_final_confirmation());

    assert!(!RiskLevel::High.auto_dispatch_allowed());
    assert!(RiskLevel::High.requires_final_confirmation());
}

#[test]
fn serde_snake_case() {
    assert_eq!(
        serde_json::to_string(&RiskLevel::Medium).unwrap(),
        "\"medium\""
    );
    assert_eq!(
        serde_json::from_str::<RiskLevel>("\"high\"").unwrap(),
        RiskLevel::High
    );
}
