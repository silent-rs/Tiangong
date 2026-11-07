//! 中枢式分离存储的回归测试：会话存储根独立时，模型配置仍从共享根读取。
//!
//! 背景（src-tauri central.rs 中枢对话）：中枢 CoreManager 的
//! storage_root 指向 `{root}/central`（会话物理隔离），若模型注册表也
//! 从该目录读取则解析不出任何可用模型（目录下无 models.json），首轮
//! 投递即报「无可用模型」且界面无反馈。`with_models_config_root` 把
//! 模型配置读取固定到共享主配置根。

#[cfg(test)]
mod separated_roots_tests {
    use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
    use tiangong_core_manager::CoreManager;

    fn write_models(root: &std::path::Path, chat: &str) {
        std::fs::write(
            root.join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {
                    "text": {"provider": "p", "model": "text-model", "capabilities": ["chat"]}
                },
                "routing": {"chat": chat}
            })
            .to_string(),
        )
        .unwrap();
    }

    /// 会话根独立（无 models.json）+ 模型配置根共享：默认 chat 端点可解析。
    #[test]
    fn default_chat_resolves_from_shared_models_root() {
        let config_dir = tempfile::tempdir().unwrap();
        write_models(config_dir.path(), "text");
        let sessions_dir = tempfile::tempdir().unwrap();
        // 会话根确实没有 models.json（分离存储前提）
        assert!(!sessions_dir.path().join("models.json").exists());

        let manager = CoreManager::with_models_config_root(
            CoreConfigProvider::new(CoreConfig::default()),
            sessions_dir.path(),
            config_dir.path(),
        );
        let endpoint = manager
            .resolve_turn_model(None)
            .expect("共享模型配置根应能解析默认 chat 端点");
        assert_eq!(endpoint.model, "text-model");
        assert_eq!(endpoint.base_url, "https://api.example.com");
    }

    /// 对照：若从会话根读模型配置（旧行为），独立根解析不出端点。
    #[test]
    fn legacy_single_root_manager_without_models_json_fails_to_resolve() {
        let sessions_dir = tempfile::tempdir().unwrap();
        let manager = CoreManager::new(
            CoreConfigProvider::new(CoreConfig::default()),
            sessions_dir.path(),
        );
        // 无 models.json → 无可用模型：显式错误而非静默空端点
        assert!(manager.resolve_turn_model(None).is_err());
    }
}
