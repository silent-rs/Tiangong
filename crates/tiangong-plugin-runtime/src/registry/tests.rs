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
                    r#"{{"schema_version":2,"id":"{id}","version":"1.0.0","mention":{{"hint":"test"}}}}"#
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
        require_server: false,
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
            r#"{{"schema_version":2,"id":"{id}","version":"1.0.0","mention":{{"hint":"候选"}},"ui":{{"contributions":[{{"slot":"extension.tab","id":"app","title":"{id}","entry":"dist/index.html"}}]}}}}"#
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
    let values: Vec<String> = mention_sources()
        .into_iter()
        // 注册表持有的来源：没有任何 TiangongCore 存在，也不依赖适配器弱引用。
        .flat_map(|source| source.query(&query).expect("查询不应失败"))
        .map(|candidate| candidate.value)
        .collect();
    assert!(
        values.contains(&"@plugin:mention-enabled".to_string()),
        "启用插件应可见：{values:?}"
    );
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

/// server 连接信息变化时：声明 require_server、旧版 scheduler 与常驻原生
/// sidecar 需重启；terminal（会话驱动）与按需 sidecar 不重启。
#[test]
fn server_change_restarts_dependent_and_resident_sidecars() {
    let parse = |json: &str| serde_json::from_str::<PluginManifest>(json).unwrap();
    let sidecar = |lifecycle: &str| {
        format!(
            r#""sidecar":{{"binary":"s","transport_protocol":"0.1.0","business_protocol":1,"lifecycle":"{lifecycle}"}}"#
        )
    };
    let resident = sidecar("resident");
    let on_demand = sidecar("on_demand");
    let bot = parse(&format!(
        r#"{{"schema_version":2,"id":"bot","version":"0.1.0",{resident}}}"#
    ));
    let terminal = parse(&format!(
        r#"{{"schema_version":2,"id":"terminal","version":"0.1.0",{resident}}}"#
    ));
    let dependent = parse(&format!(
        r#"{{"schema_version":2,"id":"subagent","version":"0.1.0","require_server":true,{on_demand}}}"#
    ));
    let scheduler = parse(&format!(
        r#"{{"schema_version":1,"id":"scheduler","version":"0.2.4","wasm":{{"binary":"s.wasm"}},{on_demand}}}"#
    ));
    let plain = parse(&format!(
        r#"{{"schema_version":2,"id":"fs","version":"0.1.0",{on_demand}}}"#
    ));
    let no_sidecar =
        parse(r#"{"schema_version":2,"id":"ui","version":"0.1.0","require_server":true}"#);
    assert!(restarts_on_server_change(&bot));
    assert!(restarts_on_server_change(&dependent));
    assert!(restarts_on_server_change(&scheduler));
    assert!(!restarts_on_server_change(&terminal));
    assert!(!restarts_on_server_change(&plain));
    assert!(!restarts_on_server_change(&no_sidecar));

    set_server_endpoint("http://127.0.0.1:1".into(), Some("token".into()));
    assert_eq!(
        current_server_endpoint(),
        Some(("http://127.0.0.1:1".into(), Some("token".into())))
    );
    clear_server_endpoint();
    assert!(current_server_endpoint().is_none());
}
