//! ID 与时间生成（项目约定：scru128 ID + 本地时间文本）。

/// 生成新的 scru128 ID（字符串形式）。
pub fn new_id() -> String {
    scru128::new().to_string()
}

/// 当前本地时间文本。
///
/// 与 `tiangong_types::now_text` 保持同一来源与格式：
/// `chrono::Local::now().naive_local().to_string()`。
pub fn now_text() -> String {
    chrono::Local::now().naive_local().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
