//! status 领域测试（R7 测试集中：原 src/status.rs 内嵌测试迁出）。

use tiangong_central::status::WorkItemStatus;
// 原 mod tests
use std::collections::HashSet;

/// 设计稿 §8.1 允许的全部转移（含 cancelled 补全）。
fn legal_pairs() -> HashSet<(WorkItemStatus, WorkItemStatus)> {
    use WorkItemStatus::*;
    [
        (Draft, Planned),
        (Draft, Cancelled),
        (Planned, Queued),
        (Planned, Cancelled),
        (Queued, Running),
        (Queued, Cancelled),
        (Running, WaitingUser),
        (Running, WaitingResource),
        (Running, Blocked),
        (Running, Paused),
        (Running, Failed),
        (Running, Completed),
        (WaitingUser, Running),
        (WaitingUser, Blocked),
        (WaitingUser, Failed),
        (WaitingUser, Cancelled),
        (WaitingResource, Running),
        (WaitingResource, Blocked),
        (WaitingResource, Failed),
        (WaitingResource, Cancelled),
        (Blocked, Running),
        (Blocked, Queued),
        (Blocked, Failed),
        (Blocked, Cancelled),
        (Paused, Running),
        (Paused, Queued),
        (Paused, Cancelled),
        (Failed, Queued),
        (Failed, Cancelled),
    ]
    .into_iter()
    .collect()
}

/// 全矩阵断言：合法转移集合之外一律拒绝。
#[test]
fn transition_matrix_matches_design() {
    let legal = legal_pairs();
    for from in WorkItemStatus::ALL {
        for to in WorkItemStatus::ALL {
            let allowed = from.can_transition_to(to);
            assert_eq!(
                allowed,
                legal.contains(&(from, to)),
                "转移 {:?} → {:?} 的判定与设计不符",
                from,
                to
            );
        }
    }
}

#[test]
fn standard_lifecycle_is_legal() {
    use WorkItemStatus::*;
    let chain = [Draft, Planned, Queued, Running, Completed];
    for pair in chain.windows(2) {
        assert!(
            pair[0].can_transition_to(pair[1]),
            "{:?} → {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn terminal_states_are_immutable() {
    for terminal in [WorkItemStatus::Completed, WorkItemStatus::Cancelled] {
        assert!(terminal.is_terminal());
        for target in WorkItemStatus::ALL {
            assert!(!terminal.can_transition_to(target));
        }
    }
}

#[test]
fn serde_snake_case_round_trip() {
    for status in WorkItemStatus::ALL {
        let text = serde_json::to_string(&status).unwrap();
        assert_eq!(text, format!("\"{}\"", status.as_str()));
        let back: WorkItemStatus = serde_json::from_str(&text).unwrap();
        assert_eq!(back, status);
    }
}
