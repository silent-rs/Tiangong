//! 常驻 sidecar 保活：进程意外退出后按退避重新拉起。
//!
//! stdio sidecar 崩溃后，运行时要等下一次调用才会换代重启；而 IM 长连接、
//! 定时调度等常驻进程需要持续在线才能接收外部事件，不能依赖调用触发。
//! 保活线程在首次预加载插件完成登记后由运行时自行启动（各入口无需调用），
//! 此后发现已启用的常驻 sidecar 未运行就拉起；插件禁用、卸载或宿主退出后
//! 不再拉起。
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// 保活轮询间隔。
const TICK: Duration = Duration::from_secs(5);
/// 重新拉起的最小与最大退避。
const MIN_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

static KEEPALIVE_STARTED: AtomicBool = AtomicBool::new(false);

/// 启动常驻 sidecar 保活线程（幂等）。
pub(super) fn ensure_keepalive_started() {
    if KEEPALIVE_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("resident-sidecar-keepalive".into())
        .spawn(keepalive_loop);
    if let Err(error) = spawned {
        KEEPALIVE_STARTED.store(false, Ordering::Release);
        tracing::warn!(%error, "创建常驻 sidecar 保活线程失败");
    }
}

#[derive(Debug, Clone, Copy)]
struct Backoff {
    next_attempt: Instant,
    delay: Duration,
}

fn keepalive_loop() {
    let mut backoff: HashMap<String, Backoff> = HashMap::new();
    while !sidecars_shutting_down() {
        std::thread::sleep(TICK);
        if sidecars_shutting_down() {
            break;
        }
        let targets = keepalive_targets();
        backoff.retain(|plugin_id, _| targets.iter().any(|(id, _)| id == plugin_id));
        let now = Instant::now();
        for (plugin_id, directory) in targets {
            if sidecar_alive(&directory) {
                backoff.remove(&plugin_id);
                continue;
            }
            if backoff
                .get(&plugin_id)
                .is_some_and(|state| state.next_attempt > now)
            {
                continue;
            }
            let Some(storage_root) = storage_root_of(&directory) else {
                continue;
            };
            tracing::info!(plugin_id, "常驻 sidecar 未运行，尝试拉起");
            let result = prewarm_plugin_sidecar_blocking(&storage_root, &plugin_id);
            if result.is_ok() && sidecar_alive(&directory) {
                backoff.remove(&plugin_id);
                continue;
            }
            let delay = next_delay(backoff.get(&plugin_id).map(|state| state.delay));
            backoff.insert(
                plugin_id,
                Backoff {
                    next_attempt: Instant::now() + delay,
                    delay,
                },
            );
        }
    }
}

fn next_delay(previous: Option<Duration>) -> Duration {
    previous
        .map(|delay| (delay * 2).min(MAX_BACKOFF))
        .unwrap_or(MIN_BACKOFF)
}

/// 需要保活的插件：已启用、无加载错误、已通过安装验证，且应随宿主常驻运行。
fn keepalive_targets() -> Vec<(String, PathBuf)> {
    loaded_plugins()
        .lock()
        .map(|plugins| {
            plugins
                .values()
                .filter(|loaded| {
                    loaded.enabled
                        && loaded.load_error.is_none()
                        && loaded.verified_sidecar.is_some()
                        && loaded.manifest.should_preload_sidecar()
                })
                .map(|loaded| (loaded.manifest.id.clone(), loaded.directory.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// 安装目录下是否有存活的 sidecar 进程（只查连接表，不创建连接）。
fn sidecar_alive(directory: &Path) -> bool {
    sidecar_connections()
        .lock()
        .map(|connections| {
            connections
                .iter()
                .filter(|(key, _)| key.directory == directory)
                .any(|(_, connection)| {
                    !connection.is_stopped() && connection.has_runtime_endpoint()
                })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_until_cap() {
        assert_eq!(next_delay(None), MIN_BACKOFF);
        assert_eq!(next_delay(Some(MIN_BACKOFF)), MIN_BACKOFF * 2);
        assert_eq!(next_delay(Some(Duration::from_secs(200))), MAX_BACKOFF);
        assert_eq!(next_delay(Some(MAX_BACKOFF)), MAX_BACKOFF);
    }

    /// 连接桩：`alive` 控制是否有运行端点，`stopped` 模拟被主动停止。
    struct Probe {
        alive: bool,
        stopped: bool,
    }

    impl SidecarConnection for Probe {
        fn invoke(&self, _: &str, _: &str) -> Result<String> {
            Ok("{}".into())
        }
        fn stop(&self) -> Result<()> {
            Ok(())
        }
        fn is_stopped(&self) -> bool {
            self.stopped
        }
        fn has_runtime_endpoint(&self) -> bool {
            self.alive
        }
    }

    fn record(id: &str, directory: &Path, sidecar: &str, enabled: bool) -> LoadedPlugin {
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "id": id,
            "version": "0.1.0",
            "sidecar": serde_json::from_str::<serde_json::Value>(sidecar).unwrap(),
        }))
        .unwrap();
        LoadedPlugin {
            directory: directory.to_path_buf(),
            manifest,
            signed_release: None,
            wasm_bytes: None,
            component: None,
            ui_plugin: None,
            descriptor: None,
            generation: 0,
            instances: Vec::new(),
            ts_instances: Vec::new(),
            sidecar: None,
            verified_sidecar: Some(Vec::new()),
            load_error: None,
            runtime_error: None,
            enabled,
        }
    }

    /// 只有已启用、已验证的原生常驻 sidecar 纳入保活（不看 require_server）；
    /// 存活判定只认未停止且有运行端点的连接。
    #[test]
    #[serial_test::serial]
    fn keepalive_targets_cover_enabled_resident_sidecars() {
        let root = tempfile::tempdir().unwrap();
        let tag = scru128::new().to_string().to_lowercase();
        let resident = format!("ka-resident-{tag}");
        let disabled = format!("ka-disabled-{tag}");
        let on_demand = format!("ka-on-demand-{tag}");
        let ids = [resident.clone(), disabled.clone(), on_demand.clone()];
        let directory = |id: &str| root.path().join("plugins").join(id);
        struct Cleanup(Vec<String>, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Ok(mut plugins) = loaded_plugins().lock() {
                    for id in &self.0 {
                        plugins.remove(id);
                    }
                }
                if let Ok(mut connections) = sidecar_connections().lock() {
                    connections.retain(|key, _| !key.directory.starts_with(&self.1));
                }
            }
        }
        let _cleanup = Cleanup(ids.to_vec(), root.path().to_path_buf());
        {
            let mut plugins = loaded_plugins().lock().unwrap();
            let native_resident = r#"{"binary":"bin","lifecycle":"resident"}"#;
            plugins.insert(
                resident.clone(),
                record(&resident, &directory(&resident), native_resident, true),
            );
            plugins.insert(
                disabled.clone(),
                record(&disabled, &directory(&disabled), native_resident, false),
            );
            plugins.insert(
                on_demand.clone(),
                record(
                    &on_demand,
                    &directory(&on_demand),
                    r#"{"binary":"bin","lifecycle":"on_demand"}"#,
                    true,
                ),
            );
        }
        let targets: Vec<String> = keepalive_targets()
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| ids.contains(id))
            .collect();
        assert_eq!(targets, vec![resident.clone()]);

        let resident_dir = directory(&resident);
        let insert = |workspace: Option<PathBuf>, probe: Probe| {
            sidecar_connections().lock().unwrap().insert(
                SidecarConnectionKey {
                    directory: resident_dir.clone(),
                    workspace,
                },
                Arc::new(probe) as Arc<dyn SidecarConnection>,
            );
        };
        assert!(!sidecar_alive(&resident_dir), "没有连接视为未运行");
        insert(
            None,
            Probe {
                alive: true,
                stopped: true,
            },
        );
        assert!(!sidecar_alive(&resident_dir), "已停止的连接不算存活");
        insert(
            None,
            Probe {
                alive: false,
                stopped: false,
            },
        );
        assert!(!sidecar_alive(&resident_dir), "进程已退出的连接不算存活");
        insert(
            Some(root.path().to_path_buf()),
            Probe {
                alive: true,
                stopped: false,
            },
        );
        assert!(sidecar_alive(&resident_dir));
    }
}
