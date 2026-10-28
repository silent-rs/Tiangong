//! registry 的注册表级单元测试（原文件级测试模块整体迁移）。

use super::connections::remove_sidecar_connection;
use super::migrations::post_install_sidecar_check_with;

use super::*;

/// 指纹只反映插件能力的声明态：加载顺序与重复项不构成变化。
#[test]
fn 顺序与重复不影响指纹() {
    let base = fingerprint_of(&["fs@0.1.9".into(), "terminal@0.3.8".into()]);
    let reordered = fingerprint_of(&[
        "terminal@0.3.8".into(),
        "fs@0.1.9".into(),
        "fs@0.1.9".into(),
    ]);
    assert_eq!(base, reordered, "加载顺序与重复项不应改变指纹");
}

/// 版本与集合的任何变化都必须改变指纹。
#[test]
fn 插件版本与集合变化改变指纹() {
    let base = fingerprint_of(&["fs@0.1.9".into()]);
    assert_ne!(
        fingerprint_of(&["fs@0.2.0".into()]),
        base,
        "插件升级即使工具名不变也应改变指纹"
    );
    assert_ne!(
        fingerprint_of(&["fs@0.1.9".into(), "terminal@0.3.8".into()]),
        base,
        "新增启用插件应改变指纹"
    );
    assert_ne!(fingerprint_of(&[]), base, "停用全部插件应改变指纹");
}

/// 分隔符缺失时 ["ab","c"] 与 ["a","bc"] 会拼成同一串。
#[test]
fn 相邻插件键不因拼接歧义碰撞() {
    assert_ne!(
        fingerprint_of(&["ab".into(), "c".into()]),
        fingerprint_of(&["a".into(), "bc".into()]),
        "不同插件集合不得产生相同指纹"
    );
}

/// 无插件时仍给出确定值，调用方无需特判空集合。
#[test]
fn 空集合指纹稳定() {
    assert_eq!(fingerprint_of(&[]), fingerprint_of(&[]));
    assert!(!fingerprint_of(&[]).is_empty());
}

/// 记录 stop 调用的桩连接：验证热加载清理连接缓存的行为，
/// 可注入 stop 失败验证「先摘表再停」不被个别失败阻断。
struct StubSidecarConnection {
    stopped: std::sync::atomic::AtomicBool,
    fail_stop: bool,
}
impl SidecarConnection for StubSidecarConnection {
    fn invoke(&self, _: &str, _: &str) -> Result<String> {
        Ok("{}".into())
    }
    fn ensure_running(&self) -> Result<()> {
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        self.stopped.store(true, Ordering::SeqCst);
        if self.fail_stop {
            bail!("stub stop failure");
        }
        Ok(())
    }
}

#[test]
#[serial_test::serial]
fn reload_clears_stale_connections_even_without_sidecar_declaration() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let config_directory = root.path().join("config");
    std::fs::create_dir_all(&config_directory).unwrap();
    tiangong_config::registry::init_from_dir(&config_directory);
    let id = format!("reload-stale-{}", scru128::new());
    let directory = root.path().join("plugins").join(&id);
    std::fs::create_dir_all(directory.join("app")).unwrap();
    std::fs::write(directory.join("app/index.html"), "ui").unwrap();
    // 新清单不含 sidecar 声明：目录整体替换后去掉 sidecar 的场景，
    // 热加载仍必须清掉旧连接缓存（否则旧进程残留、Windows 上目录被占用）。
    let manifest_value = serde_json::json!({"schema_version":2,"id":id,"version":"0.1.0",
            "permissions":["tool.provide"],"entrypoints":["desktop"],
            "ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"app/index.html"}]}});
    std::fs::write(directory.join(MANIFEST_FILE), manifest_value.to_string()).unwrap();
    let parse_manifest = || {
        serde_json::from_str::<PluginManifest>(
            &std::fs::read_to_string(directory.join(MANIFEST_FILE)).unwrap(),
        )
        .unwrap()
    };

    // 断言失败也清理全局状态，避免污染后续用例。
    struct GlobalStateGuard {
        id: String,
        directory: PathBuf,
    }
    impl Drop for GlobalStateGuard {
        fn drop(&mut self) {
            if let Ok(mut connections) = sidecar_connections().lock() {
                connections.retain(|key, _| key.directory != self.directory);
            }
            if let Ok(mut plugins) = loaded_plugins().lock() {
                plugins.remove(&self.id);
            }
        }
    }
    let _guard = GlobalStateGuard {
        id: id.clone(),
        directory: directory.clone(),
    };

    let healthy = std::sync::Arc::new(StubSidecarConnection {
        stopped: std::sync::atomic::AtomicBool::new(false),
        fail_stop: false,
    });
    let failing = std::sync::Arc::new(StubSidecarConnection {
        stopped: std::sync::atomic::AtomicBool::new(false),
        fail_stop: true,
    });
    {
        let mut connections = sidecar_connections().lock().unwrap();
        connections.insert(
            SidecarConnectionKey {
                directory: directory.clone(),
                workspace: None,
            },
            std::sync::Arc::clone(&healthy) as std::sync::Arc<dyn SidecarConnection>,
        );
        connections.insert(
            SidecarConnectionKey {
                directory: directory.clone(),
                workspace: Some(root.path().to_path_buf()),
            },
            std::sync::Arc::clone(&failing) as std::sync::Arc<dyn SidecarConnection>,
        );
    }
    // 注册表记录里的旧 sidecar（stop_loaded_sidecar 分支的目标）也种桩。
    let loaded_stub = std::sync::Arc::new(StubSidecarConnection {
        stopped: std::sync::atomic::AtomicBool::new(false),
        fail_stop: false,
    });
    loaded_plugins().lock().unwrap().insert(
        id.clone(),
        LoadedPlugin {
            directory: directory.clone(),
            manifest: parse_manifest(),
            signed_release: None,
            wasm_bytes: None,
            component: None,
            ui_plugin: None,
            descriptor: None,
            generation: 0,
            instances: Vec::new(),
            ts_instances: Vec::new(),
            sidecar: Some(
                std::sync::Arc::clone(&loaded_stub) as std::sync::Arc<dyn SidecarConnection>
            ),
            verified_sidecar: None,
            prompt_sections: None,
            dynamic_tool_specs: None,
            load_error: None,
            runtime_error: None,
            enabled: true,
        },
    );

    let installed = InstalledPlugin {
        directory: directory.clone(),
        manifest: parse_manifest(),
        enabled: true,
        signed_release: None,
    };
    // 热加载自身尽力而为：连接清理报错（failing 桩）只告警，重载仍成功。
    reload_plugin_inner(root.path(), &installed).expect("热加载应成功");

    // 无条件清理：该目录的连接全部出表；个别 stop 失败不阻断摘表，
    // 也不阻断其余连接的停止；注册表记录里的旧 sidecar 同样被停。
    let remaining = sidecar_connections()
        .lock()
        .unwrap()
        .keys()
        .filter(|key| key.directory == directory)
        .count();
    assert_eq!(remaining, 0, "热加载后连接表不应残留该目录的连接");
    assert!(healthy.stopped.load(Ordering::SeqCst));
    assert!(failing.stopped.load(Ordering::SeqCst));
    assert!(loaded_stub.stopped.load(Ordering::SeqCst));
}

#[test]
#[serial_test::serial]
fn startup_preparation_retries_failed_load_and_skips_disabled_plugins() {
    let root = tempfile::tempdir().unwrap();
    let config_directory = root.path().join("config");
    std::fs::create_dir_all(&config_directory).unwrap();
    tiangong_config::registry::init_from_dir(&config_directory);
    let id = format!("startup-load-{}", scru128::new());
    let directory = root.path().join("plugins").join(&id);
    std::fs::create_dir_all(directory.join("app")).unwrap();
    std::fs::write(directory.join("app/index.html"), "ready").unwrap();
    let mut manifest = serde_json::json!({"schema_version":2,"id":id,"version":"0.1.0",
            "ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"app/index.html"}]},
            "wasm":{"binary":"broken.wasm"}});
    std::fs::write(directory.join("broken.wasm"), "invalid wasm").unwrap();
    std::fs::write(directory.join(MANIFEST_FILE), manifest.to_string()).unwrap();
    let readiness = prepare_desktop_startup_plugins(root.path()).unwrap();
    assert!(
        readiness
            .failures
            .iter()
            .any(|failure| failure.contains(&id)),
        "{readiness:?}"
    );
    std::fs::write(directory.join(DISABLED_MARKER), "").unwrap();
    let readiness = prepare_desktop_startup_plugins(root.path()).unwrap();
    assert!(
        readiness.failures.is_empty(),
        "已禁用的失败插件不计入失败：{readiness:?}"
    );
    std::fs::remove_file(directory.join(DISABLED_MARKER)).unwrap();
    manifest["entrypoints"] = serde_json::json!(["cli"]);
    std::fs::write(directory.join(MANIFEST_FILE), manifest.to_string()).unwrap();
    let readiness = prepare_desktop_startup_plugins(root.path()).unwrap();
    assert!(
        readiness.failures.is_empty(),
        "非 desktop 插件不计入失败：{readiness:?}"
    );
    manifest.as_object_mut().unwrap().remove("wasm");
    manifest["entrypoints"] = serde_json::json!(["desktop"]);
    std::fs::write(directory.join(MANIFEST_FILE), manifest.to_string()).unwrap();
    assert_eq!(
        prepare_desktop_startup_plugins(root.path()).unwrap().loaded,
        1
    );
    assert!(
        loaded_plugins()
            .lock()
            .unwrap()
            .get(&id)
            .unwrap()
            .load_error
            .is_none()
    );
    loaded_plugins().lock().unwrap().remove(&id);
}

#[test]
#[serial_test::serial]
fn startup_preparation_reports_verification_and_resident_failures_then_recovers() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct RecoverableSidecar {
        failing: AtomicBool,
        starts: AtomicUsize,
    }
    impl SidecarConnection for RecoverableSidecar {
        fn invoke(&self, _: &str, _: &str) -> Result<String> {
            Ok("{}".into())
        }
        fn ensure_running(&self) -> Result<()> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if self.failing.load(Ordering::SeqCst) {
                bail!("resident temporarily unavailable");
            }
            Ok(())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let config_directory = root.path().join("config");
    std::fs::create_dir_all(&config_directory).unwrap();
    tiangong_config::registry::init_from_dir(&config_directory);
    let id = format!("startup-resident-{}", scru128::new());
    let directory = root.path().join("plugins").join(&id);
    std::fs::create_dir_all(directory.join("app")).unwrap();
    std::fs::write(directory.join("app/index.html"), "ready").unwrap();
    let binary = format!("test-sidecar{}", std::env::consts::EXE_SUFFIX);
    std::fs::write(directory.join(&binary), "not an executable").unwrap();
    let manifest = serde_json::json!({"schema_version":2,"id":id,"version":"0.1.0",
            "permissions":["sidecar.invoke"],"entrypoints":["desktop"],
            "ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"app/index.html"}]},
            "sidecar":{"binary":binary,"lifecycle":"resident","startup_timeout_ms":500}});
    std::fs::write(directory.join(MANIFEST_FILE), manifest.to_string()).unwrap();
    let artifact = |path: &str| serde_json::json!({"path":path,"sha256":hex::encode(sha2::Sha256::digest(std::fs::read(directory.join(path)).unwrap()))});
    let release = serde_json::json!({"schema_version":1,"id":id,"version":"0.1.0","publisher":"local",
            "permissions":["sidecar.invoke"],"manifest":artifact(MANIFEST_FILE),
            "ui":[artifact("app/index.html")],"sidecar":artifact(&binary)});
    std::fs::write(directory.join("release.json"), release.to_string()).unwrap();
    crate::trust::sign_with_user_key(root.path(), &directory.join("release.json")).unwrap();
    let readiness = prepare_desktop_startup_plugins(root.path()).unwrap();
    assert!(
        readiness
            .failures
            .iter()
            .any(|failure| failure.contains("插件验证失败")),
        "{readiness:?}"
    );
    assert!(
        readiness
            .failures
            .iter()
            .any(|failure| failure.contains(&id)),
        "{readiness:?}"
    );
    // 在同一入口中分别验证“验证记录可用但启动失败”和“失败后恢复”。
    let connection = Arc::new(RecoverableSidecar {
        failing: AtomicBool::new(true),
        starts: AtomicUsize::new(0),
    });
    for (key, cached) in sidecar_connections().lock().unwrap().iter_mut() {
        if key.directory == directory {
            *cached = connection.clone();
        }
    }
    loaded_plugins()
        .lock()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .sidecar = Some(connection.clone());
    let record = crate::verification::SidecarVerification {
        plugin_id: id.clone(),
        plugin_version: "0.1.0".into(),
        artifact_digest: crate::verification::artifact_digest(&directory).unwrap(),
        protocol_version: crate::protocol::PROTOCOL_VERSION.into(),
        capabilities: Vec::new(),
        verified_at: chrono::Local::now().naive_local().to_string(),
    };
    crate::verification::save_verification(&directory, &record).unwrap();
    let readiness = prepare_desktop_startup_plugins(root.path()).unwrap();
    assert!(
        readiness
            .failures
            .iter()
            .any(|failure| failure.contains("resident temporarily unavailable")),
        "{readiness:?}"
    );
    connection.failing.store(false, Ordering::SeqCst);
    let readiness = prepare_desktop_startup_plugins(root.path()).unwrap();
    assert!(readiness.failures.is_empty(), "{readiness:?}");
    assert_eq!(readiness.loaded, 1);
    assert_eq!(connection.starts.load(Ordering::SeqCst), 2);
    assert_eq!(loaded_runtime_error(&id), None);
    remove_sidecar_connection(&directory);
    loaded_plugins().lock().unwrap().remove(&id);
}

#[test]
fn 插件发现忽略内部事务目录并拒绝目录名与清单id不一致() {
    let root = tempfile::tempdir().unwrap();
    let plugins = root.path().join("plugins");
    for (directory, id) in [
        ("demo", "demo"),
        (".demo-staging-123", "demo"),
        ("wrong-directory", "other"),
    ] {
        let path = plugins.join(directory);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
                path.join(MANIFEST_FILE),
                format!(
                    r#"{{"schema_version":2,"id":"{id}","version":"1.0.0","permissions":["sidecar.invoke"],"mention":{{"candidates_op":"{id}.mention_candidates"}},"sidecar":{{"binary":"{id}-sidecar"}}}}"#
                ),
            )
            .unwrap();
    }

    let (installed, invalid) = discover_installed_plugins(root.path());
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].manifest.id, "demo");
    assert_eq!(invalid.len(), 1);
    assert_eq!(invalid[0].id, "wrong-directory");
    assert!(invalid[0].reason.contains("目录名"));
}

/// 错误分类保护：sidecar 运行检查重新成功只清除 runtime_error，
/// 加载错误（如 WASM 实例创建失败）必须保留——逻辑层仍不可用，
/// 插件状态不能被重验成功"洗白"。
#[test]
fn refresh_verified_sidecar_clears_only_runtime_error() {
    let manifest = PluginManifest {
        schema_version: 2,
        name: None,
        description: None,
        id: "load-error-demo".into(),
        version: "0.1.0".into(),
        wasm: None,
        sidecar: None,
        permissions: vec![],
        entrypoints: None,
        model_requirements: None,
        storage_access: false,
        capabilities: None,
        ui: None,
        tools: None,
        prompt: None,
        resources: None,
        mention: None,
        tool_icons: None,
        tool_titles: None,
    };
    let record = LoadedPlugin {
        directory: PathBuf::from("/tmp/load-error-demo"),
        manifest,
        signed_release: None,
        wasm_bytes: None,
        component: None,
        ui_plugin: None,
        descriptor: None,
        generation: 1,
        instances: Vec::new(),
        ts_instances: Vec::new(),
        sidecar: None,
        verified_sidecar: None,
        prompt_sections: None,
        dynamic_tool_specs: None,
        load_error: Some("WASM 实例创建失败".into()),
        runtime_error: Some("sidecar 启动失败".into()),
        enabled: true,
    };
    loaded_plugins()
        .lock()
        .unwrap()
        .insert("load-error-demo".into(), record);
    refresh_verified_sidecar("load-error-demo", Vec::new());
    {
        let plugins = loaded_plugins().lock().unwrap();
        let loaded = plugins.get("load-error-demo").unwrap();
        assert!(loaded.runtime_error.is_none(), "运行检查成功应清除启动异常");
        assert_eq!(
            loaded.load_error.as_deref(),
            Some("WASM 实例创建失败"),
            "加载错误不得被运行检查成功清除"
        );
    }
    loaded_plugins().lock().unwrap().remove("load-error-demo");
}

#[test]
#[serial_test::serial]
fn sidecar_check_failures_keep_installation_and_publish_only_after_save() {
    use crate::verification::SidecarVerification;
    use std::cell::RefCell;

    let root = tempfile::tempdir().unwrap();
    let id = format!("verify-{}", scru128::new());
    let directory = root.path().join("plugins").join(&id);
    std::fs::create_dir_all(&directory).unwrap();
    let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
        "schema_version":2, "id":id, "version":"0.1.0",
        "permissions":["sidecar.invoke"],
        "sidecar":{"runtime":"node","entry":"must-not-run.mjs"}
    }))
    .unwrap();
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    std::fs::write(directory.join(MANIFEST_FILE), &manifest_bytes).unwrap();
    loaded_plugins().lock().unwrap().insert(
        id.clone(),
        LoadedPlugin {
            directory: directory.clone(),
            manifest,
            signed_release: None,
            wasm_bytes: None,
            component: None,
            ui_plugin: None,
            descriptor: None,
            generation: 1,
            instances: Vec::new(),
            ts_instances: Vec::new(),
            sidecar: None,
            verified_sidecar: None,
            prompt_sections: None,
            dynamic_tool_specs: None,
            load_error: None,
            runtime_error: None,
            enabled: true,
        },
    );
    let record = SidecarVerification {
        plugin_id: id.clone(),
        plugin_version: "0.1.0".into(),
        artifact_digest: crate::verification::artifact_digest(&directory).unwrap(),
        protocol_version: crate::protocol::PROTOCOL_VERSION.into(),
        capabilities: vec!["tool:probe".into()],
        verified_at: chrono::Local::now().naive_local().to_string(),
    };

    // 连续模拟验证失败、保存失败、恢复成功；整个过程不创建外部进程。
    for failure in ["verify", "save", "none"] {
        let operations = RefCell::new(Vec::new());
        post_install_sidecar_check_with(
            &id,
            || {
                operations.borrow_mut().push("verify");
                if failure == "verify" {
                    return Err(anyhow::anyhow!("模拟验证失败")).context("sidecar 完整验证失败");
                }
                Ok(record.clone())
            },
            |verified| {
                operations.borrow_mut().push("save");
                assert_eq!(verified.plugin_id, id);
                assert_eq!(verified.capabilities, record.capabilities);
                assert!(
                    loaded_verified_sidecar(&id).is_none(),
                    "保存成功前不能发布能力"
                );
                if failure == "save" {
                    return Err(anyhow::anyhow!("模拟磁盘写入失败")).context("保存验证记录失败");
                }
                Ok(())
            },
        );
        assert_eq!(
            std::fs::read(directory.join(MANIFEST_FILE)).unwrap(),
            manifest_bytes,
            "失败不能回滚或删除已安装插件"
        );
        assert!(loaded_plugins().lock().unwrap().contains_key(&id));
        if failure == "none" {
            assert_eq!(
                loaded_verified_sidecar(&id),
                Some(record.capabilities.clone())
            );
            assert!(loaded_runtime_error(&id).is_none(), "恢复后应清除异常");
        } else {
            assert!(loaded_verified_sidecar(&id).is_none());
            let error = loaded_runtime_error(&id).expect("失败应登记异常");
            assert!(
                error.contains(if failure == "verify" {
                    "模拟验证失败"
                } else {
                    "模拟磁盘写入失败"
                }),
                "{error}"
            );
        }
        assert_eq!(
            *operations.borrow(),
            if failure == "verify" {
                vec!["verify"]
            } else {
                vec!["verify", "save"]
            }
        );
    }
    loaded_plugins().lock().unwrap().remove(&id);
}

/// #550 验收：查询句柄不依赖会话 Core（无任何 Core 也可达），
/// 且停用/卸载后立即从候选集合消失。
#[test]
#[serial_test::serial]
fn mention_sources_are_reachable_without_any_session_core() {
    let manifest = |id: &str| -> PluginManifest {
        serde_json::from_str(&format!(
            r#"{{"schema_version":2,"id":"{id}","version":"1.0.0","mention":{{"candidates_op":"{id}.mention_candidates"}},"sidecar":{{"binary":"{id}-sidecar"}},"ui":{{"contributions":[{{"slot":"extension.tab","id":"app","title":"{id}","entry":"dist/index.html"}}]}}}}"#
        ))
        .expect("解析测试清单")
    };
    let record = |id: &str, enabled: bool| LoadedPlugin {
        directory: std::path::PathBuf::from(format!("/tmp/{id}")),
        manifest: manifest(id),
        signed_release: None,
        wasm_bytes: None,
        component: None,
        ui_plugin: None,
        descriptor: None,
        generation: 1,
        instances: Vec::new(),
        ts_instances: Vec::new(),
        sidecar: None,
        verified_sidecar: None,
        prompt_sections: None,
        dynamic_tool_specs: None,
        load_error: None,
        runtime_error: None,
        enabled,
    };
    {
        let mut plugins = loaded_plugins().lock().unwrap();
        plugins.insert("mention-enabled".into(), record("mention-enabled", true));
        plugins.insert("mention-disabled".into(), record("mention-disabled", false));
    }
    let query = tiangong_types::MentionQuery::default();
    // #550：来源由注册表持有，没有任何 TiangongCore、也不依赖适配器弱引用
    // 即可查询。提及内容自 v2 起由 sidecar 的 candidates_op 提供，测试装置
    // 没有安装 sidecar，因而启用插件返回空候选而非报错——这里断言的是可达
    // 性与停用/卸载行为，内容链路由 loader 与 ts_plugin 的单测覆盖。
    let source_ids: Vec<String> = mention_sources()
        .into_iter()
        .map(|source| source.id().to_string())
        .collect();
    assert!(
        source_ids.contains(&"mention-enabled".to_string()),
        "启用插件应有候选来源：{source_ids:?}"
    );
    assert!(
        !source_ids.iter().any(|id| id == "mention-disabled"),
        "停用插件不应有来源：{source_ids:?}"
    );
    let values: Vec<String> = mention_sources()
        .into_iter()
        .flat_map(|source| source.query(&query).expect("查询不应失败"))
        .map(|candidate| candidate.value)
        .collect();
    assert!(
        !values
            .iter()
            .any(|value| value.contains("mention-disabled")),
        "停用插件不应出现：{values:?}"
    );
    // 卸载后同一来源不得再返回候选（换代/移除保护）。
    // 只取本测试自己的来源：全局注册表可能残留其他测试插入的插件。
    let handle = mention_sources()
        .into_iter()
        .find(|source| source.id() == "mention-enabled")
        .expect("应取到本测试插件的候选来源");
    loaded_plugins().lock().unwrap().remove("mention-enabled");
    assert!(
        handle.query(&query).expect("查询不应失败").is_empty(),
        "卸载后旧来源必须返回空候选"
    );
    loaded_plugins().lock().unwrap().remove("mention-disabled");
}

/// server 连接信息可设置、覆盖与清除。
#[test]
fn server_endpoint_can_be_set_and_cleared() {
    set_server_endpoint("http://127.0.0.1:1".into(), Some("token".into()));
    assert_eq!(
        current_server_endpoint(),
        Some(("http://127.0.0.1:1".into(), Some("token".into())))
    );
    clear_server_endpoint();
    assert!(current_server_endpoint().is_none());
}
/// Server/CLI 没有插件前端：只有工具全部由 sidecar 承接的清单插件才注册工具。
#[test]
fn manifest_tools_need_sidecar_handlers_outside_desktop() {
    let parse = |json: &str| serde_json::from_str::<PluginManifest>(json).unwrap();
    let tool = r#""tools":[{"name":"send_text_message","description":"d","input_schema":{"type":"object"}}]"#;
    let sidecar =
        r#""sidecar":{"binary":"bot","transport_protocol":"0.1.0","business_protocol":1}"#;
    let page = r#""ui":{"contributions":[{"slot":"settings.plugin-page","id":"s","entry":"app/settings.html","sandbox":"iframe"}]}"#;
    let headless = parse(&format!(
        r#"{{"schema_version":2,"id":"bot-a","version":"0.1.0",{tool},{sidecar}}}"#
    ));
    let with_page = parse(&format!(
        r#"{{"schema_version":2,"id":"bot-b","version":"0.1.0",{tool},{sidecar},{page}}}"#
    ));
    let ui_only_tools = parse(&format!(
        r#"{{"schema_version":2,"id":"ui-c","version":"0.1.0",{tool},{page}}}"#
    ));
    assert!(tools_served_by_sidecar(&headless, None));
    assert!(!tools_served_by_sidecar(&with_page, None));
    assert!(!tools_served_by_sidecar(
        &with_page,
        Some(&["tool:other".to_string()])
    ));
    assert!(tools_served_by_sidecar(
        &with_page,
        Some(&["tool:send_text_message".to_string()])
    ));
    assert!(!tools_served_by_sidecar(
        &ui_only_tools,
        Some(&["tool:*".to_string()])
    ));
}

#[test]
fn sidecar_instance_role_defaults_to_auxiliary() {
    let config = crate::sidecar::SidecarConfig::new("p", "0.1.0", "/b", "/e", "/l", "/d", "/s");
    assert_eq!(
        config.instance_role(),
        crate::sidecar::INSTANCE_ROLE_AUXILIARY
    );
    let config = config.with_primary_instance(true);
    assert_eq!(
        config.instance_role(),
        crate::sidecar::INSTANCE_ROLE_PRIMARY
    );
}

/// 解析 candidates_op 响应：index 形态（仅 candidates，无 entry）。
#[test]
fn 提及响应仅含candidates时不合成入口候选() {
    let manifest: crate::manifest::PluginManifest = serde_json::from_str(
        r#"{"schema_version":2,"id":"index","version":"1.0.0","permissions":["sidecar.invoke"],"mention":{"candidates_op":"index.mention_candidates"},"sidecar":{"binary":"index-sidecar"}}"#,
    )
    .expect("清单应合法");
    let response = serde_json::json!({
        "candidates": [
            { "value": "@file:src/main.rs", "label": "main.rs", "kind": "file", "hint": "src/main.rs" }
        ]
    });
    let candidates = super::loader::parse_mention_op_response(
        &response,
        &manifest,
        None,
        "index",
        "index.mention_candidates",
    );
    assert_eq!(candidates.len(), 1, "内容提供方只有文件候选");
    assert_eq!(candidates[0].value, "@file:src/main.rs");
    assert!(
        candidates.iter().all(|c| c.kind == "file"),
        "不应出现 plugin 入口"
    );
}

/// entry 存在且带 hint：合成入口候选（宿主约定 value/kind）+ 动态候选并存。
#[test]
fn 提及响应entry与candidates可并存() {
    let manifest: crate::manifest::PluginManifest = serde_json::from_str(
        r#"{"schema_version":2,"id":"volcengine","version":"1.0.0","permissions":["sidecar.invoke"],"mention":{"candidates_op":"volcengine.mention_candidates"},"sidecar":{"binary":"volcengine-sidecar"}}"#,
    )
    .expect("清单应合法");
    let response = serde_json::json!({
        "entry": { "hint": "生成图片、视频与语音", "mark": "火山" },
        "candidates": [
            { "value": "@plugin:volcengine", "label": "文生图", "kind": "tool", "hint": "按描述生成图片" }
        ]
    });
    let candidates = super::loader::parse_mention_op_response(
        &response,
        &manifest,
        None,
        "volcengine",
        "volcengine.mention_candidates",
    );
    assert_eq!(candidates.len(), 2, "入口候选 + 动态候选");
    assert_eq!(candidates[0].value, "@plugin:volcengine");
    assert_eq!(candidates[0].kind, "plugin");
    assert_eq!(candidates[0].hint, "生成图片、视频与语音");
    assert_eq!(candidates[0].mark, "火山");
    assert_eq!(candidates[1].value, "@plugin:volcengine");
    assert_eq!(candidates[1].kind, "tool");
}

/// entry 缺 hint 跳过；candidates 残缺项跳过——不把渲染不出的候选透给前端。
#[test]
fn 提及响应残缺项被跳过() {
    let manifest: crate::manifest::PluginManifest = serde_json::from_str(
        r#"{"schema_version":2,"id":"bot","version":"1.0.0","permissions":["sidecar.invoke"],"mention":{"candidates_op":"bot.mention_candidates"},"sidecar":{"binary":"bot-sidecar"}}"#,
    )
    .expect("清单应合法");
    let response = serde_json::json!({
        "entry": { "hint": "   " },
        "candidates": [
            { "value": "@file:a.rs", "label": "a.rs" },
            { "value": "", "label": "b.rs", "kind": "file", "hint": "b.rs" },
            { "value": "@file:c.rs", "label": "c.rs", "kind": "file", "hint": "c.rs" }
        ]
    });
    let candidates = super::loader::parse_mention_op_response(
        &response,
        &manifest,
        None,
        "bot",
        "bot.mention_candidates",
    );
    assert_eq!(
        candidates.len(),
        1,
        "缺 hint 的 entry 与缺字段的候选全部跳过"
    );
    assert_eq!(candidates[0].value, "@file:c.rs");
}

/// 形状不符（空对象/数组/null）不视为错误：返回空，不 panic。
#[test]
fn 提及响应形状不符返回空() {
    let manifest: crate::manifest::PluginManifest = serde_json::from_str(
        r#"{"schema_version":2,"id":"index","version":"1.0.0","permissions":["sidecar.invoke"],"mention":{"candidates_op":"index.mention_candidates"},"sidecar":{"binary":"index-sidecar"}}"#,
    )
    .expect("清单应合法");
    for value in [
        serde_json::json!({}),
        serde_json::json!([]),
        serde_json::Value::Null,
        serde_json::json!({ "candidates": "not-an-array" }),
    ] {
        assert!(
            super::loader::parse_mention_op_response(
                &value,
                &manifest,
                None,
                "index",
                "index.mention_candidates"
            )
            .is_empty(),
            "形状 {value} 应解析为空候选"
        );
    }
}

/// v0.17.6 静态 mention（hint/mark）能合成入口候选：与 op entry 同一条
/// 路径，value/kind 是宿主约定，hint/mark 来自静态声明。
#[test]
fn 静态mention声明生成入口候选() {
    let manifest: crate::manifest::PluginManifest = serde_json::from_str(
        r#"{"schema_version":2,"id":"legacy-demo","version":"0.9.0","entrypoints":["desktop"],"capabilities":{"prompt":true},"prompt":["说明"],"mention":{"hint":"老插件的能力说明","mark":"老"}}"#,
    )
    .expect("静态清单应合法");
    manifest.validate().unwrap();
    let mention = manifest.mention.as_ref().unwrap();
    let candidate = crate::ts_plugin::mention_entry_candidate(
        &manifest,
        None,
        &super::loader::static_mention_entry(mention),
    )
    .expect("静态 hint 应合成入口候选");
    assert_eq!(candidate.value, "@plugin:legacy-demo");
    assert_eq!(candidate.kind, "plugin");
    assert_eq!(candidate.hint, "老插件的能力说明");
    assert_eq!(candidate.mark, "老");
    // 缺 hint（空值视为没写）：不合成——与 op entry 缺 hint 同语义。
    let broken = serde_json::json!({ "hint": "   ", "mark": "老" });
    assert!(
        crate::ts_plugin::mention_entry_candidate(&manifest, None, &broken).is_none(),
        "空 hint 不合成入口"
    );
}

/// 动态 prompt 段落装载时组装进注册表；内容变化必须改变能力指纹，
/// 走插件变化的上下文交接（压缩 message）链路——这是段落变更的
/// 唯一传播途径（声明聚合只读快照）。
#[test]
#[serial_test::serial]
fn 动态prompt段落变化改变能力指纹() {
    let manifest = serde_json::from_str::<PluginManifest>(
        r#"{"schema_version":2,"id":"prompt-digest-demo","version":"1.0.0","sidecar":{"binary":"demo-sidecar"}}"#,
    )
    .expect("解析测试清单");
    let record = |prompt_sections: Option<Vec<String>>| LoadedPlugin {
        dynamic_tool_specs: None,
        directory: std::path::PathBuf::from("/tmp/prompt-digest-demo"),
        manifest: manifest.clone(),
        signed_release: None,
        wasm_bytes: None,
        component: None,
        ui_plugin: None,
        descriptor: None,
        generation: 1,
        instances: Vec::new(),
        ts_instances: Vec::new(),
        sidecar: None,
        verified_sidecar: None,
        prompt_sections,
        load_error: None,
        runtime_error: None,
        enabled: true,
    };
    {
        let mut plugins = loaded_plugins().lock().unwrap();
        plugins.insert("prompt-digest-demo".into(), record(None));
    }
    let without_prompt = crate::registry::enabled_plugin_fingerprint();
    {
        let mut plugins = loaded_plugins().lock().unwrap();
        plugins.insert(
            "prompt-digest-demo".into(),
            record(Some(vec!["第一版段落".to_string()])),
        );
    }
    let with_prompt_v1 = crate::registry::enabled_plugin_fingerprint();
    {
        let mut plugins = loaded_plugins().lock().unwrap();
        plugins.insert(
            "prompt-digest-demo".into(),
            record(Some(vec!["第二版段落".to_string()])),
        );
    }
    let with_prompt_v2 = crate::registry::enabled_plugin_fingerprint();
    loaded_plugins()
        .lock()
        .unwrap()
        .remove("prompt-digest-demo");

    assert_ne!(without_prompt, with_prompt_v1, "声明动态段落即能力变化");
    assert_ne!(with_prompt_v1, with_prompt_v2, "段落内容变化必须改变指纹");
}

/// P0 回归：runtime.prompt_updated 通知重取必须用**插件目录**组装——
/// 曾经多取一次 parent 把 <root>/plugins 当插件目录，file 条目必读失败，
/// 段落快照被截断。同时验证内容变化后发布 PromptSectionsChanged 事件。
#[test]
#[serial_test::serial]
fn 通知重取用插件目录组装并发布事件() {
    let root = tempfile::tempdir().unwrap();
    let plugin_dir = root.path().join("plugins").join("refresh-file-demo");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("guide.md"), "文件段落").unwrap();
    let manifest = serde_json::from_str::<PluginManifest>(
        r#"{"schema_version":2,"id":"refresh-file-demo","version":"1.0.0","entrypoints":["desktop"],"permissions":["sidecar.invoke"],"prompt":["内联",{"type":"file","file":"guide.md"}]}"#,
    )
    .unwrap();
    {
        let mut plugins = loaded_plugins().lock().unwrap();
        plugins.insert(
            "refresh-file-demo".into(),
            LoadedPlugin {
                directory: plugin_dir.clone(),
                manifest,
                signed_release: None,
                wasm_bytes: None,
                component: None,
                ui_plugin: None,
                descriptor: None,
                generation: 1,
                instances: Vec::new(),
                ts_instances: Vec::new(),
                sidecar: None,
                verified_sidecar: None,
                prompt_sections: Some(vec!["旧快照".to_string()]),
                dynamic_tool_specs: None,
                load_error: None,
                runtime_error: None,
                enabled: true,
            },
        );
    }
    refresh_prompt_sections("refresh-file-demo");
    let sections = {
        let plugins = loaded_plugins().lock().unwrap();
        plugins
            .get("refresh-file-demo")
            .unwrap()
            .prompt_sections
            .clone()
    };
    loaded_plugins().lock().unwrap().remove("refresh-file-demo");
    assert_eq!(
        sections,
        Some(vec!["内联".to_string(), "文件段落".to_string()]),
        "file 条目必须从插件目录读取成功，而非被截断"
    );
}

/// P0 回归：补验证成功（refresh_verified_sidecar）不得清空段落——
/// 曾经把空 vec 传给 reconfigure，存活适配器的静态与文件段落一并丢失。
#[test]
#[serial_test::serial]
fn 补验证刷新保留段落快照() {
    let root = tempfile::tempdir().unwrap();
    let plugin_dir = root.path().join("plugins").join("reverify-prompt-demo");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("guide.md"), "文件段落").unwrap();
    let manifest = serde_json::from_str::<PluginManifest>(
        r#"{"schema_version":2,"id":"reverify-prompt-demo","version":"1.0.0","entrypoints":["desktop"],"permissions":["sidecar.invoke"],"prompt":[{"type":"file","file":"guide.md"}]}"#,
    )
    .unwrap();
    let adapter = std::sync::Arc::new(crate::ts_plugin::TsPluginAdapter::from_manifest(
        &manifest,
        true,
        None,
        vec!["旧段落".to_string()],
        Vec::new(),
    ));
    {
        let mut plugins = loaded_plugins().lock().unwrap();
        plugins.insert(
            "reverify-prompt-demo".into(),
            LoadedPlugin {
                directory: plugin_dir.clone(),
                manifest,
                signed_release: None,
                wasm_bytes: None,
                component: None,
                ui_plugin: None,
                descriptor: None,
                generation: 1,
                instances: Vec::new(),
                ts_instances: vec![std::sync::Arc::downgrade(&adapter)],
                sidecar: None,
                verified_sidecar: None,
                prompt_sections: Some(vec!["旧段落".to_string()]),
                dynamic_tool_specs: None,
                load_error: None,
                runtime_error: Some("启动异常".to_string()),
                enabled: true,
            },
        );
    }
    refresh_verified_sidecar("reverify-prompt-demo", Vec::new());
    let record_sections = {
        let plugins = loaded_plugins().lock().unwrap();
        plugins
            .get("reverify-prompt-demo")
            .unwrap()
            .prompt_sections
            .clone()
    };
    loaded_plugins()
        .lock()
        .unwrap()
        .remove("reverify-prompt-demo");
    assert_eq!(record_sections, Some(vec!["文件段落".to_string()]));
    use tiangong_core::tools::extension::PromptSectionProvider as _;
    assert_eq!(adapter.prompt_sections(), vec!["文件段落".to_string()]);
}

/// 单次装载快照：无 WASM 重载中验证记录与 prompt 段落各组装一次，
/// 存活适配器与注册表拿到的内容完全一致（旧代码 N+1 次且可能不一致）。
#[test]
#[serial_test::serial]
fn 无wasm重载只读取一次装载快照且内容一致() {
    use tiangong_core::tools::extension::PromptSectionProvider as _;

    let root = tempfile::tempdir().unwrap();
    let plugin_dir = root.path().join("plugins").join("snapshot-demo");
    std::fs::create_dir_all(plugin_dir.join("prompts")).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.json"),
        r#"{"schema_version":2,"id":"snapshot-demo","version":"0.1.0","entrypoints":["desktop"],"prompt":["内联",{"type":"file","file":"prompts/guide.md"}]}"#,
    )
    .unwrap();
    std::fs::write(plugin_dir.join("prompts").join("guide.md"), "文件段落").unwrap();
    let manifest: PluginManifest = serde_json::from_str(
        r#"{"schema_version":2,"id":"snapshot-demo","version":"0.1.0","entrypoints":["desktop"],"prompt":["内联",{"type":"file","file":"prompts/guide.md"}]}"#,
    )
    .unwrap();
    let adapter_a = std::sync::Arc::new(crate::ts_plugin::TsPluginAdapter::from_manifest(
        &manifest,
        true,
        None,
        Vec::new(),
        Vec::new(),
    ));
    let adapter_b = std::sync::Arc::new(crate::ts_plugin::TsPluginAdapter::from_manifest(
        &manifest,
        true,
        None,
        Vec::new(),
        Vec::new(),
    ));
    {
        let mut plugins = loaded_plugins().lock().unwrap();
        plugins.insert(
            "snapshot-demo".into(),
            LoadedPlugin {
                directory: plugin_dir.clone(),
                manifest: manifest.clone(),
                signed_release: None,
                wasm_bytes: None,
                component: None,
                ui_plugin: None,
                descriptor: None,
                generation: 1,
                instances: Vec::new(),
                ts_instances: vec![
                    std::sync::Arc::downgrade(&adapter_a),
                    std::sync::Arc::downgrade(&adapter_b),
                ],
                sidecar: None,
                verified_sidecar: None,
                prompt_sections: Some(vec!["旧段落".to_string()]),
                dynamic_tool_specs: None,
                load_error: None,
                runtime_error: None,
                enabled: true,
            },
        );
    }
    let installed = InstalledPlugin {
        directory: plugin_dir.clone(),
        manifest: manifest.clone(),
        enabled: true,
        signed_release: None,
    };
    let config_directory = root.path().join("config");
    std::fs::create_dir_all(&config_directory).unwrap();
    tiangong_config::registry::init_from_dir(&config_directory);
    crate::registry::ASSEMBLE_PROMPT_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    reload_plugin_inner(root.path(), &installed).unwrap();
    let count = crate::registry::ASSEMBLE_PROMPT_COUNT.load(std::sync::atomic::Ordering::SeqCst);
    let record = {
        let plugins = loaded_plugins().lock().unwrap();
        plugins
            .get("snapshot-demo")
            .unwrap()
            .prompt_sections
            .clone()
    };
    loaded_plugins().lock().unwrap().remove("snapshot-demo");

    assert_eq!(count, 1, "无 WASM 重载只允许组装一次（快照复用）");
    let expected = vec!["内联".to_string(), "文件段落".to_string()];
    assert_eq!(record, Some(expected.clone()), "注册表使用同一份快照");
    assert_eq!(
        adapter_a.prompt_sections(),
        expected,
        "适配器 A 与注册表一致"
    );
    assert_eq!(
        adapter_b.prompt_sections(),
        expected,
        "适配器 B 与注册表一致"
    );
}

/// None 语义：未声明 prompt 的清单重载后注册表保持 None（不是空数组），
/// 适配器段落为空。
#[test]
#[serial_test::serial]
fn 无wasm重载保留prompt未声明的none语义() {
    use tiangong_core::tools::extension::PromptSectionProvider as _;

    let root = tempfile::tempdir().unwrap();
    let plugin_dir = root.path().join("plugins").join("none-demo");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.json"),
        r#"{"schema_version":2,"id":"none-demo","version":"0.1.0","entrypoints":["desktop"],"ui":{"contributions":[{"slot":"settings.plugin-page","id":"s","title":"设置","entry":"app/s.html"}]}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(plugin_dir.join("app")).unwrap();
    std::fs::write(plugin_dir.join("app").join("s.html"), "<html></html>").unwrap();
    let manifest: PluginManifest = serde_json::from_str(
        r#"{"schema_version":2,"id":"none-demo","version":"0.1.0","entrypoints":["desktop"],"ui":{"contributions":[{"slot":"settings.plugin-page","id":"s","title":"设置","entry":"app/s.html"}]}}"#,
    )
    .unwrap();
    let adapter = std::sync::Arc::new(crate::ts_plugin::TsPluginAdapter::from_manifest(
        &manifest,
        true,
        None,
        Vec::new(),
        Vec::new(),
    ));
    {
        let mut plugins = loaded_plugins().lock().unwrap();
        plugins.insert(
            "none-demo".into(),
            LoadedPlugin {
                directory: plugin_dir.clone(),
                manifest: manifest.clone(),
                signed_release: None,
                wasm_bytes: None,
                component: None,
                ui_plugin: None,
                descriptor: None,
                generation: 1,
                instances: Vec::new(),
                ts_instances: vec![std::sync::Arc::downgrade(&adapter)],
                sidecar: None,
                verified_sidecar: None,
                // 重载前错误地持有 Some：装载快照应回归 None
                prompt_sections: Some(vec!["遗留".to_string()]),
                dynamic_tool_specs: None,
                load_error: None,
                runtime_error: None,
                enabled: true,
            },
        );
    }
    let installed = InstalledPlugin {
        directory: plugin_dir.clone(),
        manifest: manifest.clone(),
        enabled: true,
        signed_release: None,
    };
    let config_directory = root.path().join("config");
    std::fs::create_dir_all(&config_directory).unwrap();
    tiangong_config::registry::init_from_dir(&config_directory);
    reload_plugin_inner(root.path(), &installed).unwrap();
    let record = {
        let plugins = loaded_plugins().lock().unwrap();
        plugins.get("none-demo").unwrap().prompt_sections.clone()
    };
    loaded_plugins().lock().unwrap().remove("none-demo");
    assert_eq!(record, None, "未声明 prompt 必须保持 None（指纹语义）");
    assert!(adapter.prompt_sections().is_empty(), "适配器段落为空");
}
