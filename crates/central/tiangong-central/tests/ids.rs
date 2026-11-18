//! ids 领域测试（R7 测试集中：原 src/ids.rs 内嵌测试迁出）。

use tiangong_central::ids::{new_id, now_text};
// 原 mod tests

#[test]
fn new_id_is_unique_and_non_empty() {
    let a = new_id();
    let b = new_id();
    assert!(!a.is_empty());
    assert_ne!(a, b, "scru128 连续生成不得重复");
}

#[test]
fn now_text_is_non_empty() {
    assert!(!now_text().is_empty());
}
